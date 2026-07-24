//! Roaring execution for Boolean queries (M3 §5 three-tier rule per
//! segment, M4 §4/§5 zero-copy): each AND clause's doc source is its
//! validated inline bitmap — a full-mode byte cursor for the small side /
//! merge-intersect, a probe-mode contains() for the high-df side — or,
//! for clauses under the bitmap threshold, a query-time materialized doc
//! vec (df<4096, bounded). OR still folds container bitmaps here (T4
//! switches it to byte cursors). When NO clause has a bitmap the callers
//! fall back to the existing PFOR conjunction/disjunction untouched
//! (tier 3, M1's tuned path).

use std::io;

use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::roaring::{RoaringBitmap, RoaringView};
use codec_lucene9::terms_read::TermEntry;

use super::doc_iter::{DocIter, DocSource, RoaringAndDocIter, RoaringDocIter, SegmentDocIter};
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
/// scan into a roaring bitmap (spec §5: df<4096 → ≤4095 docs, bounded).
/// Docs arrive ascending from the enum, satisfying the build precondition
/// of `RoaringBitmap::from_sorted_docs`.
fn materialize_clause(
    seg: &SegmentReader,
    entry: &TermEntry,
    has_freqs: bool,
) -> io::Result<RoaringBitmap> {
    let mut docs = Vec::with_capacity(entry.doc_freq as usize);
    for_each_doc(seg, entry, has_freqs, &mut |d| docs.push(d))?;
    Ok(RoaringBitmap::from_sorted_docs(&docs))
}

/// Folds clause bitmaps with container and/or (spec §5 档 1/2): probes
/// every clause's inline bitmap, returns None when NO clause has one
/// (tier 3), materializes the missing clauses otherwise. The result is a
/// roaring bitmap — iterated directly, never flattened (spec §5).
fn fold_clauses(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
    is_and: bool,
) -> io::Result<Option<RoaringBitmap>> {
    let mut sources: Vec<Option<RoaringBitmap>> = Vec::with_capacity(entries.len());
    for (_, entry) in entries {
        sources.push(seg.read_term_bitmap(entry)?);
    }
    if sources.iter().all(|s| s.is_none()) {
        return Ok(None); // tier 3: no bitmaps at all in this segment
    }
    for (src, (_, entry)) in sources.iter_mut().zip(entries) {
        if src.is_none() {
            *src = Some(materialize_clause(seg, entry, has_freqs)?);
        }
    }
    let mut it = sources.into_iter().map(Option::unwrap);
    let mut acc = it.next().expect("entries is non-empty");
    for b in it {
        acc = if is_and { acc.and(&b) } else { acc.or(&b) };
    }
    Ok(Some(acc))
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

/// Three-tier segment iterator (spec §5): Some = roaring path taken
/// (tier 1/2); None = tier 3, the caller builds the PFOR iterator.
/// AND: M4 view engine (and_iterator); OR: M3 container fold (T4 switches).
pub(crate) fn segment_iterator(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
    is_and: bool,
) -> io::Result<Option<SegmentDocIter>> {
    if is_and {
        return and_iterator(seg, entries, has_freqs);
    }
    let Some(b) = fold_clauses(seg, entries, has_freqs, false)? else {
        return Ok(None);
    };
    Ok(Some(SegmentDocIter::Roaring(RoaringDocIter::new(b))))
}

/// Count fast path (spec §5: count 走同一引擎). AND: drives the same
/// view iterator (count == iteration by construction, 关键设计事实 10).
/// OR: cardinality of the folded bitmap (T4 unifies on the view engine).
/// None = tier 3, caller iterates.
pub(crate) fn count(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
    is_and: bool,
) -> io::Result<Option<u64>> {
    if is_and {
        let Some(mut it) = and_iterator(seg, entries, has_freqs)? else {
            return Ok(None);
        };
        let mut n = 0u64;
        loop {
            if it.next_doc()? == NO_MORE_DOCS {
                break;
            }
            n += 1;
        }
        return Ok(Some(n));
    }
    Ok(fold_clauses(seg, entries, has_freqs, false)?.map(|b| b.cardinality()))
}
