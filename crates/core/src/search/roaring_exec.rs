//! Roaring execution for Boolean queries (M3 §5 three-tier rule per
//! segment, M5 §2 croaring engine): each AND/OR clause's doc source is its
//! validated inline bitmap — a frozen-view batch cursor — or, for clauses
//! under the bitmap threshold, a query-time materialized doc vec (df<4096,
//! bounded). AND tier-1: skewed → smallest-side iteration + contains
//! probes (8.5–28.7 ns direct memory probes), non-skewed → k-way
//! merge-intersect (T2 keeps the M4 shape; T3 swaps in the croaring
//! materialized fold). OR: k-way merge-union over batch cursors +
//! materialized slices. When NO clause has a bitmap the callers fall back
//! to the existing PFOR conjunction/disjunction untouched (tier 3).

use std::io;

use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::roaring::FrozenBitmap;
use codec_lucene9::terms_read::TermEntry;

use super::doc_iter::{DocIter, DocSource, RoaringAndDocIter, RoaringOrDocIter, SegmentDocIter};
use super::multi_term::for_each_doc;
use super::segment_reader::SegmentReader;

/// Tier-1 skew gate: when max/min df reaches this ratio the AND runs
/// lead-cursor + contains() probes; below it, k-way merge-intersect. The
/// M4 calibration (256) is carried over mechanically — T3 swaps the
/// non-skewed strategy to the croaring materialized fold and resets the
/// initial value, T5 recalibrates with croaring costs (关键设计事实 8).
pub(crate) const SKEW_RATIO: u64 = 256;

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
        // tier 1 non-skewed (T2: M4 merge-intersect 形态; T3 换 croaring
        // 物化 fold, 关键设计事实 8)
        for opened in opened {
            sources.push(DocSource::bitmap(opened.expect("tier 1: all present")));
        }
    }
    Ok(Some(SegmentDocIter::RoaringAnd(RoaringAndDocIter::new(
        sources,
        probe_bitmaps,
    ))))
}

/// Tier-1/2 OR over frozen views (M5 §2): k-way merge-union — one open
/// per clause (the open doubles as the bitmap-presence probe), batch
/// cursors + materialized low-df slices. All-None = tier 3. (T3: 全
/// bitmap 时换 croaring 物化 or fold.)
fn or_iterator(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
) -> io::Result<Option<SegmentDocIter>> {
    let Some(opened) = open_clauses(seg, entries)? else {
        return Ok(None); // tier 3
    };
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

/// Count (spec §5: count 走同一引擎): T2 驱动同一迭代器到尽头（count
/// == 迭代结果数由构造保证）。None = tier 3, caller iterates. (T3: 全
/// bitmap 时换 and/or_cardinality 快路径, spec §2.)
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
