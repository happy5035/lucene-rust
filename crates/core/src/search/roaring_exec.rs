//! Roaring execution for Boolean queries (M3 §5 three-tier rule per
//! segment, M5 §2 croaring engine): each clause's doc source is its
//! validated inline bitmap — a frozen view — or, for clauses under the
//! bitmap threshold, a query-time materialized doc vec (df<4096, bounded).
//! AND tier-1: skewed → smallest-side iteration + contains probes
//! (8.5–28.7 ns direct memory probes), non-skewed → croaring
//! materialized `and` fold + result iteration (关键设计事实 8). OR
//! tier-1: materialized `or` fold; tier-2 mixed: k-way merge-union over
//! batch cursors + slices. Count over all-bitmap clauses is the
//! and/or_cardinality fold (µs级, spec §2 — no per-doc driving). When NO
//! clause has a bitmap the callers fall back to the existing PFOR
//! conjunction/disjunction untouched (tier 3).

use std::io;

use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::roaring::FrozenBitmap;
use codec_lucene9::terms_read::TermEntry;

use super::doc_iter::{DocIter, DocSource, RoaringAndDocIter, RoaringOrDocIter, SegmentDocIter};
use super::multi_term::for_each_doc;
use super::segment_reader::SegmentReader;

/// Tier-1 skew gate: when max/min df reaches this ratio the AND runs
/// smallest-side iteration + contains() probes; below it, the croaring
/// materialized `and` fold. Initial 4 pending T5 recalibration with
/// croaring costs (contains 8.5–28.7 ns/probe, and_cardinality 7.7µs
/// sparse / 4.2µs dense / 0.27µs run, materialized and 10.2µs sparse /
/// 157µs dense — probe REPORT §Bench; method 关键设计事实 15). Both
/// strategies pay both region reads under frozen (关键设计事实 6), so
/// the crossover can only be measured.
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

/// Opens every clause's frozen view (one sequential region read each —
/// open/probe 合一, 关键设计事实 6). None = no clause has a bitmap
/// (tier 3).
fn open_clauses(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
) -> io::Result<Option<Vec<Option<FrozenBitmap>>>> {
    let mut opened = Vec::with_capacity(entries.len());
    for (_, entry) in entries {
        opened.push(seg.open_term_bitmap(entry)?);
    }
    if opened.iter().all(|o| o.is_none()) {
        return Ok(None); // tier 3
    }
    Ok(Some(opened))
}

/// Tier-1/2 AND over frozen views (M5 §2). entries are df-ascending (==
/// cardinality-ascending, validation ③), so construction order is
/// cheapest-first.
fn and_iterator(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
) -> io::Result<Option<SegmentDocIter>> {
    let Some(opened) = open_clauses(seg, entries)? else {
        return Ok(None); // tier 3
    };
    let bitmap_count = opened.iter().filter(|o| o.is_some()).count();
    let mut sources: Vec<DocSource> = Vec::new();
    let mut probe_bitmaps: Vec<FrozenBitmap> = Vec::new();
    if bitmap_count < entries.len() {
        // tier 2: materialize the bitmap-less clauses (df<4096, bounded)
        // and point-probe their candidates against every bitmap view —
        // contains is a direct memory probe (spec §2 档 2 不变)
        for ((_, entry), opened) in entries.iter().zip(opened.into_iter()) {
            match opened {
                Some(v) => probe_bitmaps.push(v),
                None => sources.push(DocSource::slice(materialize_docs(seg, entry, has_freqs)?)),
            }
        }
    } else if entries.last().unwrap().0 as u64 >= SKEW_RATIO * entries[0].0 as u64 {
        // tier 1 skewed: smallest-side iteration, the rest contains-probes
        let mut it = opened.into_iter();
        let lead = it.next().unwrap().expect("tier 1: all present");
        sources.push(DocSource::bitmap(lead));
        probe_bitmaps.extend(it.map(|o| o.expect("tier 1: all present")));
    } else {
        // tier 1 non-skewed (spec §2): croaring materialized `and` fold
        // + result iteration (关键设计事实 8)
        let views: Vec<&FrozenBitmap> = opened.iter().map(|o| o.as_ref().unwrap()).collect();
        let docs = codec_lucene9::roaring::intersect_docs(&views);
        sources.push(DocSource::slice(docs));
    }
    Ok(Some(SegmentDocIter::RoaringAnd(RoaringAndDocIter::new(
        sources,
        probe_bitmaps,
    ))))
}

/// Tier-1/2 OR (M5 §2): all-bitmap → croaring materialized `or` fold +
/// result iteration; mixed → k-way merge-union over batch cursors +
/// materialized low-df slices. All-None = tier 3.
fn or_iterator(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
) -> io::Result<Option<SegmentDocIter>> {
    let Some(opened) = open_clauses(seg, entries)? else {
        return Ok(None); // tier 3
    };
    if opened.iter().all(|o| o.is_some()) {
        let views: Vec<&FrozenBitmap> = opened.iter().map(|o| o.as_ref().unwrap()).collect();
        let docs = codec_lucene9::roaring::union_docs(&views);
        return Ok(Some(SegmentDocIter::RoaringOr(RoaringOrDocIter::new(
            vec![DocSource::slice(docs)],
        ))));
    }
    let mut sources: Vec<DocSource> = Vec::new();
    for ((_, entry), opened) in entries.iter().zip(opened.into_iter()) {
        match opened {
            Some(v) => sources.push(DocSource::bitmap(v)),
            None => sources.push(DocSource::slice(materialize_docs(seg, entry, has_freqs)?)),
        }
    }
    Ok(Some(SegmentDocIter::RoaringOr(RoaringOrDocIter::new(
        sources,
    ))))
}

/// Three-tier segment iterator (spec §5): Some = roaring path taken
/// (tier 1/2); None = tier 3, the caller builds the PFOR iterator.
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

/// Count (spec §2): all-bitmap clauses → and/or_cardinality fold (µs级,
/// no per-doc driving); tier-2 mixed → drive the same iterator to
/// exhaustion (bounded candidates; count == iteration by construction).
/// None = tier 3, caller iterates.
pub(crate) fn count(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
    is_and: bool,
) -> io::Result<Option<u64>> {
    let Some(opened) = open_clauses(seg, entries)? else {
        return Ok(None); // tier 3
    };
    if opened.iter().all(|o| o.is_some()) {
        let views: Vec<&FrozenBitmap> = opened.iter().map(|o| o.as_ref().unwrap()).collect();
        return Ok(Some(if is_and {
            codec_lucene9::roaring::and_cardinality(&views)
        } else {
            codec_lucene9::roaring::or_cardinality(&views)
        }));
    }
    // tier 2: same-iterator count (reopening regions is µs级 and keeps
    // the two entry points stateless)
    let Some(mut it) = segment_iterator(seg, entries, has_freqs, is_and)? else {
        return Ok(None); // unreachable: open_clauses succeeded on the same bytes
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
