//! Query enum (search spec §3): M1 carries Term, MatchAll, And, Or.
//! All queries have ConstantScore semantics.

use std::io;

use super::doc_iter::{ConjunctionDocIter, DisjunctionDocIter, MatchAllIter, SegmentDocIter};
use super::segment_reader::SegmentReader;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    Term { field: String, term: Vec<u8> },
    MatchAll,
    And { field: String, terms: Vec<Vec<u8>> },
    Or { field: String, terms: Vec<Vec<u8>> },
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

    pub(crate) fn segment_iterator(&self, seg: &mut SegmentReader) -> io::Result<Option<SegmentDocIter>> {
        match self {
            Query::MatchAll => Ok(Some(SegmentDocIter::All(MatchAllIter::new(seg.max_doc())))),
            Query::Term { field, term } => {
                let Some((has_freqs, entry)) = seg.seek_term(field, term)? else { return Ok(None); };
                if has_freqs { Ok(Some(SegmentDocIter::Freqs(seg.docs_freqs_enum(&entry)?))) }
                else { Ok(Some(SegmentDocIter::Docs(seg.docs_enum(&entry)?))) }
            }
            Query::And { field, terms } => {
                if terms.len() < 2 {
                    return if let Some(t) = terms.first() {
                        Query::Term { field: field.clone(), term: t.clone() }.segment_iterator(seg)
                    } else { Ok(None) };
                }
                let mut entries: Vec<(u32, codec_lucene9::terms_read::TermEntry)> = Vec::new();
                for t in terms {
                    let Some((_, entry)) = seg.seek_term(field, t)? else { return Ok(None); };
                    entries.push((entry.doc_freq, entry));
                }
                entries.sort_by_key(|(df, _)| *df);
                Ok(Some(SegmentDocIter::And(ConjunctionDocIter::new(seg, field, &entries)?)))
            }
            Query::Or { field, terms } => {
                if terms.len() < 2 {
                    return if let Some(t) = terms.first() {
                        Query::Term { field: field.clone(), term: t.clone() }.segment_iterator(seg)
                    } else { Ok(None) };
                }
                let mut entries: Vec<(u32, codec_lucene9::terms_read::TermEntry)> = Vec::new();
                for t in terms {
                    if let Some((_, entry)) = seg.seek_term(field, t)? { entries.push((entry.doc_freq, entry)); }
                }
                if entries.is_empty() { return Ok(None); }
                entries.sort_by_key(|(df, _)| *df);
                Ok(Some(SegmentDocIter::Or(DisjunctionDocIter::new(seg, field, &entries)?)))
            }
        }
    }
}
