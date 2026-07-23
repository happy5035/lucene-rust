//! Query enum (search spec §3): M1 carries Term, MatchAll, And, Or.
//! All queries have ConstantScore semantics.

use std::io;

use super::doc_iter::{ConjunctionDocIter, DisjunctionDocIter, MatchAllIter, PhraseDocIter, SegmentDocIter};
use super::multi_term;
use super::segment_reader::SegmentReader;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    Term { field: String, term: Vec<u8> },
    MatchAll,
    And { field: String, terms: Vec<Vec<u8>> },
    Or { field: String, terms: Vec<Vec<u8>> },
    Terms { field: String, terms: Vec<Vec<u8>> },
    Prefix { field: String, prefix: Vec<u8> },
    Wildcard { field: String, pattern: Vec<u8> },
    Phrase { field: String, terms: Vec<Vec<u8>> },
}

impl Query {
    pub fn term(field: &str, term: &str) -> Query {
        Query::Term { field: field.to_string(), term: term.as_bytes().to_vec() }
    }
    pub fn and(field: &str, terms: &[&str]) -> Query {
        Query::And { field: field.to_string(), terms: terms.iter().map(|t| t.as_bytes().to_vec()).collect() }
    }
    pub fn or(field: &str, terms: &[&str]) -> Query {
        Query::Or { field: field.to_string(), terms: terms.iter().map(|t| t.as_bytes().to_vec()).collect() }
    }

    /// Terms(IN) — Boolean SHOULD sugar (spec M2 §1): the doc union of the
    /// term set, executed via the <=16/>16 dual path (spec M2 §4).
    pub fn terms(field: &str, terms: &[&str]) -> Query {
        Query::Terms {
            field: field.to_string(),
            terms: terms.iter().map(|t| t.as_bytes().to_vec()).collect(),
        }
    }

    /// Prefix query (spec M2 §3): all docs whose term starts with `prefix`.
    pub fn prefix(field: &str, prefix: &str) -> Query {
        Query::Prefix {
            field: field.to_string(),
            prefix: prefix.as_bytes().to_vec(),
        }
    }

    /// Wildcard query with '*' and '?' (spec M2 §5 classification).
    pub fn wildcard(field: &str, pattern: &str) -> Query {
        Query::Wildcard {
            field: field.to_string(),
            pattern: pattern.as_bytes().to_vec(),
        }
    }

    /// Exact phrase query (slop=0, spec M2 §6): consecutive offsets.
    pub fn phrase(field: &str, terms: &[&str]) -> Query {
        Query::Phrase {
            field: field.to_string(),
            terms: terms.iter().map(|t| t.as_bytes().to_vec()).collect(),
        }
    }

    /// Multi-term queries (Terms/Prefix/Wildcard) share the Searcher::count
    /// dual path (popcount on the bitset path, iteration otherwise).
    pub(crate) fn is_multi_term(&self) -> bool {
        matches!(self, Query::Terms { .. } | Query::Prefix { .. } | Query::Wildcard { .. })
    }

    /// Per-segment count shortcut: `Some(popcount)` when this query takes
    /// the bitset path in this segment, `None` otherwise (caller iterates).
    pub(crate) fn bitset_count(&self, seg: &mut SegmentReader) -> io::Result<Option<u64>> {
        match self {
            Query::Terms { field, terms } => {
                if terms.len() < 2 {
                    return Ok(None); // degenerate: empty set / single-term Term path
                }
                let Some((has_freqs, collected)) = multi_term::collect_direct(seg, field, terms)? else {
                    return Ok(Some(0)); // unknown field: empty hit set
                };
                multi_term::bitset_count(seg, has_freqs, &collected)
            }
            Query::Prefix { field, prefix } => {
                let Some((has_freqs, collected)) = multi_term::collect_prefix(seg, field, prefix)? else {
                    return Ok(Some(0));
                };
                multi_term::bitset_count(seg, has_freqs, &collected)
            }
            Query::Wildcard { field, pattern } => {
                let pat = multi_term::WildcardPattern::parse(pattern);
                let Some((has_freqs, collected)) = multi_term::collect_wildcard(seg, field, &pat)? else {
                    return Ok(Some(0));
                };
                multi_term::bitset_count(seg, has_freqs, &collected)
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn segment_iterator(
        &self,
        seg: &mut SegmentReader,
        needs_freq: bool,
    ) -> io::Result<Option<SegmentDocIter>> {
        match self {
            Query::MatchAll => Ok(Some(SegmentDocIter::All(MatchAllIter::new(seg.max_doc())))),
            Query::Term { field, term } => {
                let Some((has_freqs, entry)) = seg.seek_term(field, term)? else { return Ok(None); };
                if has_freqs { Ok(Some(SegmentDocIter::Freqs(seg.docs_freqs_enum(&entry, needs_freq)?))) }
                else { Ok(Some(SegmentDocIter::Docs(seg.docs_enum(&entry)?))) }
            }
            Query::And { field, terms } => {
                if terms.len() < 2 {
                    return if let Some(t) = terms.first() {
                        Query::Term { field: field.clone(), term: t.clone() }.segment_iterator(seg, needs_freq)
                    } else { Ok(None) };
                }
                let mut entries: Vec<(u32, codec_lucene9::terms_read::TermEntry)> = Vec::new();
                for t in terms {
                    let Some((_, entry)) = seg.seek_term(field, t)? else { return Ok(None); };
                    entries.push((entry.doc_freq, entry));
                }
                entries.sort_by_key(|(df, _)| *df);
                Ok(Some(SegmentDocIter::And(ConjunctionDocIter::new(seg, field, &entries, needs_freq)?)))
            }
            Query::Or { field, terms } => {
                if terms.len() < 2 {
                    return if let Some(t) = terms.first() {
                        Query::Term { field: field.clone(), term: t.clone() }.segment_iterator(seg, needs_freq)
                    } else { Ok(None) };
                }
                let mut entries: Vec<(u32, codec_lucene9::terms_read::TermEntry)> = Vec::new();
                for t in terms {
                    if let Some((_, entry)) = seg.seek_term(field, t)? { entries.push((entry.doc_freq, entry)); }
                }
                if entries.is_empty() { return Ok(None); }
                entries.sort_by_key(|(df, _)| *df);
                Ok(Some(SegmentDocIter::Or(DisjunctionDocIter::new(seg, field, &entries, needs_freq)?)))
            }
            Query::Terms { field, terms } => {
                if terms.is_empty() {
                    return Ok(None);
                }
                if terms.len() == 1 {
                    return Query::Term {
                        field: field.clone(),
                        term: terms[0].clone(),
                    }
                    .segment_iterator(seg, needs_freq);
                }
                let Some((has_freqs, collected)) = multi_term::collect_direct(seg, field, terms)? else {
                    return Ok(None);
                };
                multi_term::segment_iterator(seg, field, has_freqs, &collected, needs_freq)
            }
            Query::Prefix { field, prefix } => {
                let Some((has_freqs, collected)) = multi_term::collect_prefix(seg, field, prefix)? else {
                    return Ok(None);
                };
                multi_term::segment_iterator(seg, field, has_freqs, &collected, needs_freq)
            }
            Query::Wildcard { field, pattern } => {
                let pat = multi_term::WildcardPattern::parse(pattern);
                let Some((has_freqs, collected)) = multi_term::collect_wildcard(seg, field, &pat)? else {
                    return Ok(None);
                };
                multi_term::segment_iterator(seg, field, has_freqs, &collected, needs_freq)
            }
            Query::Phrase { field, terms } => {
                if terms.is_empty() {
                    return Ok(None);
                }
                if terms.len() == 1 {
                    return Query::Term {
                        field: field.clone(),
                        term: terms[0].clone(),
                    }
                    .segment_iterator(seg, needs_freq);
                }
                Ok(PhraseDocIter::new(seg, field, terms)?.map(SegmentDocIter::Phrase))
            }
        }
    }
}
