//! Roaring execution for Boolean queries (spec M3 §5 three-tier rule, per
//! segment): each term clause's doc source is its validated inline bitmap
//! when present; clauses without one are materialized at query time (their
//! df is < 4096 by the read gate, so at most 4095 docs — bounded) and the
//! whole clause set folds with container-level and/or. When NO clause has
//! a bitmap the callers fall back to the existing PFOR
//! conjunction/disjunction untouched (tier 3, M1's tuned path).

use std::io;

use codec_lucene9::roaring::RoaringBitmap;
use codec_lucene9::terms_read::TermEntry;

use super::doc_iter::{RoaringDocIter, SegmentDocIter};
use super::multi_term::for_each_doc;
use super::segment_reader::SegmentReader;

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

/// Three-tier segment iterator (spec §5): Some = roaring path taken
/// (tier 1/2); None = tier 3, the caller builds the PFOR iterator.
pub(crate) fn segment_iterator(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
    is_and: bool,
) -> io::Result<Option<SegmentDocIter>> {
    let Some(b) = fold_clauses(seg, entries, has_freqs, is_and)? else {
        return Ok(None);
    };
    Ok(Some(SegmentDocIter::Roaring(RoaringDocIter::new(b))))
}

/// Count fast path (spec §5: count = cardinality): cardinality of the
/// folded bitmap without any doc iteration. None = tier 3, caller iterates.
pub(crate) fn count(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
    is_and: bool,
) -> io::Result<Option<u64>> {
    Ok(fold_clauses(seg, entries, has_freqs, is_and)?.map(|b| b.cardinality()))
}
