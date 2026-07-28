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

use super::doc_iter::{DocIter, DocSource, RoaringAndDocIter, RoaringOrDocIter, SegmentDocIter};
use super::leaf_access::LeafAccess;
use super::multi_term::for_each_doc;

/// Tier-1 skew gate: when max/min df reaches this ratio the AND runs
/// smallest-side iteration + contains() probes; below it, the croaring
/// materialized `and` fold. T5 calibration (m5 skew micro, two nested
/// ladders run/array-bitset containers, ratio 1..244, two rounds): the
/// croaring fold won at EVERY measurable ratio (A/B 0.06–0.38 — fold
/// 24–64µs vs probe 149–403µs; probe pays 4096 × (iter ~5ns +
/// contains 8.5–28.7ns) plus per-candidate view creation, while the
/// fold's SIMD C path is ratio-independent), so the gate sits above the
/// 1M-corpus ladder max (244 → 256, same disposition as M4): the probe
/// path stays for larger indexes where the crossover may exist, but is
/// never taken at this scale (关键设计事实 8/15).
pub(crate) const SKEW_RATIO: u64 = 256; // bench-calibrated (T5): fold wins all r≤244

/// Collects the (df, entry) pairs of an And/Or's term clauses, df-sorted
/// (conjunction cost order, same as the existing query.rs inline code).
/// Returns (has_freqs, entries); None = empty segment result: unknown
/// field, an absent AND clause, or no OR clause present. M6 T-A：泛型化
/// 到 `AsRef<[u8]>`——`Vec<u8>`（And/Or 平铺变体）与 `&[u8]`（Bool 拍平
/// 的借引用 terms）同入口，单态化零成本。
pub(crate) fn collect_bool_entries<L: LeafAccess, T: AsRef<[u8]>>(
    seg: &mut L,
    field: &str,
    terms: &[T],
    is_and: bool,
) -> io::Result<Option<(bool, Vec<(u32, L::TermHandle)>)>> {
    let Some(has_freqs) = seg.field_has_freqs(field) else {
        return Ok(None);
    };
    let mut entries = Vec::with_capacity(terms.len());
    for t in terms {
        match seg.seek_term(field, t.as_ref())? {
            Some((_, entry)) => {
                let df = seg.term_doc_freq(&entry);
                entries.push((df, entry));
            }
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
fn materialize_docs<L: LeafAccess>(
    seg: &L,
    entry: &L::TermHandle,
    has_freqs: bool,
) -> io::Result<Vec<u32>> {
    let mut docs = Vec::with_capacity(seg.term_doc_freq(entry) as usize);
    for_each_doc(seg, entry, has_freqs, &mut |d| docs.push(d))?;
    Ok(docs)
}

/// Opens every clause's frozen view (one sequential region read each —
/// open/probe 合一, 关键设计事实 6). None = no clause has a bitmap
/// (tier 3).
fn open_clauses<L: LeafAccess>(
    seg: &L,
    entries: &[(u32, L::TermHandle)],
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
fn and_iterator<L: LeafAccess>(
    seg: &L,
    entries: &[(u32, L::TermHandle)],
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
fn or_iterator<L: LeafAccess>(
    seg: &L,
    entries: &[(u32, L::TermHandle)],
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
pub(crate) fn segment_iterator<L: LeafAccess>(
    seg: &L,
    entries: &[(u32, L::TermHandle)],
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
pub(crate) fn count<L: LeafAccess>(
    seg: &L,
    entries: &[(u32, L::TermHandle)],
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
