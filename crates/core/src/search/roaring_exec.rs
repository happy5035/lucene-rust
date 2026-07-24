//! Roaring execution for Boolean queries (M3 §5 three-tier rule per
//! segment, M4 §4/§5 zero-copy): each AND clause's doc source is its
//! validated inline bitmap — a full-mode byte cursor for the small side /
//! merge-intersect, a probe-mode contains() for the high-df side — or,
//! for clauses under the bitmap threshold, a query-time materialized doc
//! vec (df<4096, bounded). OR runs a k-way merge-union over full-mode
//! byte cursors + materialized slices (M4 §6) — zero container rebuilds.
//! When NO clause has a bitmap the callers
//! fall back to the existing PFOR conjunction/disjunction untouched
//! (tier 3, M1's tuned path).

use std::io;

use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::roaring::RoaringView;
use codec_lucene9::terms_read::TermEntry;

use super::doc_iter::{DocIter, DocSource, RoaringAndDocIter, RoaringOrDocIter, SegmentDocIter};
use super::multi_term::for_each_doc;
use super::segment_reader::SegmentReader;

/// Tier-1 skew gate (M4 §5): when max/min df reaches this ratio the AND
/// runs lead-cursor + contains() probes (the high-df side is never read
/// wholesale, 用户指令②); below it, k-way byte-cursor merge-intersect.
/// Initial value 4 per spec §5; T5 bench calibrates and records the
/// chosen value in .superpowers/sdd/m4-bench-report.md.
pub(crate) const SKEW_RATIO: u64 = 4; // bench-calibrated (T5)

/// Collects the (df, entry) pairs of an And/Or's term clauses, df-sorted
/// (conjunction cost order, same as the existing query.rs inline code).
/// Returns (has_freqs, entries); None = empty segment result: unknown
/// field, an absent AND clause, or no OR clause present.
pub(crate) fn collect_bool_entries(
    seg: &mut SegmentReader,
    field: &str,
    terms: &[Vec<u8>],
    is_and: bool,
) -> io::Result<Option<(bool, Vec<(u32, TermEntry)>)>> {
    let Some(has_freqs) = seg.field_has_freqs(field) else {
        return Ok(None);
    };
    let mut entries = Vec::with_capacity(terms.len());
    for t in terms {
        match seg.seek_term(field, t)? {
            Some((_, entry)) => entries.push((entry.doc_freq, entry)),
            None => {
                if is_and {
                    return Ok(None); // missing MUST clause: no hits in this segment
                }
            }
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }
    entries.sort_by_key(|(df, _)| *df);
    Ok(Some((has_freqs, entries)))
}

/// Tier-2 query-time materialization of one low-df clause: full postings
/// scan into an ascending doc vec (spec §5: df<4096 → ≤4095 docs,
/// bounded). Docs arrive ascending from the enum.
fn materialize_docs(
    seg: &SegmentReader,
    entry: &TermEntry,
    has_freqs: bool,
) -> io::Result<Vec<u32>> {
    let mut docs = Vec::with_capacity(entry.doc_freq as usize);
    for_each_doc(seg, entry, has_freqs, &mut |d| docs.push(d))?;
    Ok(docs)
}

/// Opens every clause's probe view (one container-directory scan each —
/// cheap, no data-section reads). None = no clause has a bitmap (tier 3).
fn probe_clauses(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
) -> io::Result<Option<Vec<Option<RoaringView>>>> {
    let mut probes = Vec::with_capacity(entries.len());
    for (_, entry) in entries {
        probes.push(seg.probe_term_bitmap(entry)?);
    }
    if probes.iter().all(|p| p.is_none()) {
        return Ok(None); // tier 3
    }
    Ok(Some(probes))
}

/// Tier-1/2 AND over views (M4 §5). entries are df-ascending (==
/// cardinality-ascending, validation ③), so construction order is
/// cheapest-first.
fn and_iterator(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
) -> io::Result<Option<SegmentDocIter>> {
    let Some(probes) = probe_clauses(seg, entries)? else {
        return Ok(None); // tier 3
    };
    let bitmap_count = probes.iter().filter(|p| p.is_some()).count();
    let mut sources: Vec<DocSource> = Vec::new();
    let mut probe_views: Vec<RoaringView> = Vec::new();
    if bitmap_count < entries.len() {
        // tier 2: materialize the bitmap-less clauses (df<4096, bounded)
        // and point-probe their candidates against every bitmap view —
        // zero wholesale bitmap reads (spec §5 档 2)
        for ((_, entry), probe) in entries.iter().zip(probes.into_iter()) {
            match probe {
                Some(v) => probe_views.push(v),
                None => sources.push(DocSource::slice(materialize_docs(seg, entry, has_freqs)?)),
            }
        }
    } else if entries.last().unwrap().0 as u64 >= SKEW_RATIO * entries[0].0 as u64 {
        // tier 1 skewed: smallest side full-mode iteration, the rest
        // contains-probes (the high-df side is never read wholesale)
        let Some(lead) = seg.open_term_bitmap(&entries[0].1)? else {
            return Ok(None); // unreachable: probe succeeded on the same bytes
        };
        sources.push(DocSource::view(lead));
        probe_views.extend(
            probes
                .into_iter()
                .skip(1)
                .map(|p| p.expect("tier 1: all present")),
        );
    } else {
        // tier 1 non-skewed: k-way byte-cursor merge-intersect over
        // full-mode views (spec §5 双字节游标 merge-intersect, k 路泛化)
        for (_, entry) in entries {
            let Some(v) = seg.open_term_bitmap(entry)? else {
                return Ok(None); // unreachable: probe succeeded on the same bytes
            };
            sources.push(DocSource::view(v));
        }
    }
    Ok(Some(SegmentDocIter::RoaringAnd(RoaringAndDocIter::new(
        sources,
        probe_views,
    ))))
}

/// Tier-1/2 OR over views (M4 §6): k-way merge-union — one full-mode
/// open per clause (the open doubles as the bitmap-presence probe,
/// 关键设计事实 17), materialized slices for the bitmap-less clauses
/// (df<4096, bounded). All-None = tier 3.
fn or_iterator(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
) -> io::Result<Option<SegmentDocIter>> {
    let mut sources: Vec<DocSource> = Vec::new();
    let mut any_bitmap = false;
    for (_, entry) in entries {
        match seg.open_term_bitmap(entry)? {
            Some(v) => {
                any_bitmap = true;
                sources.push(DocSource::view(v));
            }
            None => sources.push(DocSource::slice(materialize_docs(seg, entry, has_freqs)?)),
        }
    }
    if !any_bitmap {
        return Ok(None); // tier 3
    }
    Ok(Some(SegmentDocIter::RoaringOr(RoaringOrDocIter::new(
        sources,
    ))))
}

/// Three-tier segment iterator (spec §5): Some = roaring path taken
/// (tier 1/2); None = tier 3, the caller builds the PFOR iterator.
/// AND: view engine with the skew strategy (T3); OR: k-way merge-union
/// over byte cursors + materialized slices (M4 §6).
pub(crate) fn segment_iterator(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
    is_and: bool,
) -> io::Result<Option<SegmentDocIter>> {
    if is_and {
        and_iterator(seg, entries, has_freqs)
    } else {
        or_iterator(seg, entries, has_freqs)
    }
}

/// Count fast path (spec §5: count 走同一引擎): both directions drive
/// the same view iterator to exhaustion (count == iteration by
/// construction, 关键设计事实 10). None = tier 3, caller iterates.
pub(crate) fn count(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
    is_and: bool,
) -> io::Result<Option<u64>> {
    let Some(mut it) = segment_iterator(seg, entries, has_freqs, is_and)? else {
        return Ok(None);
    };
    let mut n = 0u64;
    loop {
        if it.next_doc()? == NO_MORE_DOCS {
            break;
        }
        n += 1;
    }
    Ok(Some(n))
}
