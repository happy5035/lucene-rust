//! Multi-term query execution (search spec M2 §4): collect the term set of a
//! Terms/Prefix/Wildcard query for one segment, then dispatch on the Lucene 9
//! blended threshold — at most 16 terms rewrite to `Query::Or`
//! (AbstractMultiTermQueryConstantScoreWrapper.java:43-44), more than 16
//! materialize a per-segment FixedBitSet (Lucene's DocIdSet rewrite,
//! MultiTermQueryConstantScoreBlendedWrapper.java:55-120). No enumeration cap
//! (Lucene 9 dropped TooManyClauses for MTQs).

use std::io;

use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::terms_read::TermEntry;

use super::bitset::FixedBitSet;
use super::doc_iter::{BitsetDocIter, SegmentDocIter};
use super::query::Query;
use super::segment_reader::SegmentReader;

/// AbstractMultiTermQueryConstantScoreWrapper.java:44.
pub(crate) const BOOLEAN_REWRITE_THRESHOLD: usize = 16;

/// Terms of one query present in one segment's dictionary, df-sorted
/// (spec §4: 集合收集后按 df 排序交给双路).
pub(crate) struct CollectedTerms {
    pub terms: Vec<Vec<u8>>,
    pub entries: Vec<(u32, TermEntry)>,
}

impl CollectedTerms {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn sort_by_df(&mut self) {
        let mut pairs: Vec<(Vec<u8>, (u32, TermEntry))> = self
            .terms
            .drain(..)
            .zip(self.entries.drain(..))
            .collect();
        pairs.sort_by_key(|(_, (df, _))| *df);
        for (t, e) in pairs {
            self.terms.push(t);
            self.entries.push(e);
        }
    }
}

/// Direct term-set collection (Terms/IN): seek each term, keep the present
/// ones. `None` = unknown field (empty-hit semantics, same as TermQuery).
pub(crate) fn collect_direct(
    seg: &mut SegmentReader,
    field: &str,
    terms: &[Vec<u8>],
) -> io::Result<Option<(bool, CollectedTerms)>> {
    let Some(has_freqs) = seg.field_has_freqs(field) else {
        return Ok(None);
    };
    let mut collected = CollectedTerms {
        terms: Vec::new(),
        entries: Vec::new(),
    };
    for t in terms {
        if let Some((_, entry)) = seg.seek_term(field, t)? {
            collected.terms.push(t.clone());
            collected.entries.push((entry.doc_freq, entry));
        }
    }
    collected.sort_by_df();
    Ok(Some((has_freqs, collected)))
}

/// Threshold dispatch (spec §4): <=16 terms rewrite to `Query::Or` (heap
/// merge, zero new execution code); >16 materialize a FixedBitSet.
pub(crate) fn segment_iterator(
    seg: &mut SegmentReader,
    field: &str,
    has_freqs: bool,
    collected: &CollectedTerms,
    needs_freq: bool,
) -> io::Result<Option<SegmentDocIter>> {
    if collected.is_empty() {
        return Ok(None);
    }
    if collected.len() <= BOOLEAN_REWRITE_THRESHOLD {
        return Query::Or {
            field: field.to_string(),
            terms: collected.terms.clone(),
        }
        .segment_iterator(seg, needs_freq);
    }
    let bits = materialize(seg, &collected.entries, has_freqs)?;
    Ok(Some(SegmentDocIter::Bitset(BitsetDocIter::new(bits))))
}

/// Count fast path (spec §4: count 路径直接 popcount): `Some(popcount)` on
/// the bitset path, `None` when the OR path applies (caller iterates).
pub(crate) fn bitset_count(
    seg: &SegmentReader,
    has_freqs: bool,
    collected: &CollectedTerms,
) -> io::Result<Option<u64>> {
    if collected.len() <= BOOLEAN_REWRITE_THRESHOLD {
        return Ok(None);
    }
    Ok(Some(materialize(seg, &collected.entries, has_freqs)?.popcount()))
}

/// Bitset materialization (spec §4): per-term full postings scan, one bit
/// per hit doc. Uses no-freq enums on freqs fields — the bitset carries no
/// per-doc freq (ConstantScore).
pub(crate) fn materialize(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
) -> io::Result<FixedBitSet> {
    let mut bits = FixedBitSet::new(seg.max_doc() as usize);
    for (_, entry) in entries {
        if has_freqs {
            let mut en = seg.docs_freqs_enum(entry, false)?;
            loop {
                let d = en.next_doc()?;
                if d == NO_MORE_DOCS {
                    break;
                }
                bits.set(d as usize);
            }
        } else {
            let mut en = seg.docs_enum(entry)?;
            loop {
                let d = en.next_doc()?;
                if d == NO_MORE_DOCS {
                    break;
                }
                bits.set(d as usize);
            }
        }
    }
    Ok(bits)
}
