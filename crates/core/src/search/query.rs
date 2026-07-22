//! Query enum (search spec §3): M1 carries Term and MatchAll only. All
//! queries have ConstantScore semantics — no scoring anywhere.

use std::io;

use super::doc_iter::{MatchAllIter, SegmentDocIter};
use super::segment_reader::SegmentReader;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    Term { field: String, term: Vec<u8> },
    MatchAll,
}

impl Query {
    pub fn term(field: &str, term: &str) -> Query {
        Query::Term {
            field: field.to_string(),
            term: term.as_bytes().to_vec(),
        }
    }

    /// Builds the per-segment iterator (Lucene leaf-level execution).
    /// `Ok(None)` = no hits in this segment.
    pub(crate) fn segment_iterator(
        &self,
        seg: &mut SegmentReader,
    ) -> io::Result<Option<SegmentDocIter>> {
        match self {
            Query::MatchAll => Ok(Some(SegmentDocIter::All(MatchAllIter::new(seg.max_doc())))),
            Query::Term { field, term } => {
                let Some((has_freqs, entry)) = seg.seek_term(field, term)? else {
                    return Ok(None);
                };
                if has_freqs {
                    Ok(Some(SegmentDocIter::Freqs(seg.docs_freqs_enum(&entry)?)))
                } else {
                    Ok(Some(SegmentDocIter::Docs(seg.docs_enum(&entry)?)))
                }
            }
        }
    }
}
