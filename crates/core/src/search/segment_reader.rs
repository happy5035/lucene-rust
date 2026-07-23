//! Per-segment read view (search spec §3 SegmentReader): field infos +
//! terms dict + postings of one segment. Open reads only segments_N/.si/.fnm
//! + codec headers; field FSTs load lazily inside the terms dict.

use std::io;

use codec_lucene9::directory::FSDirectory;
use codec_lucene9::field_infos::{FieldInfo, FieldInfos, IndexOptions};
use codec_lucene9::postings_read::{DocsEnum, DocsFreqsEnum, PostingsReader};
use codec_lucene9::segment_infos::SegmentCommitInfo;
use codec_lucene9::terms_read::{TermEntry, TermsDict, TermsIter};

pub struct SegmentReader {
    max_doc: i32,
    field_infos: FieldInfos,
    terms: TermsDict,
    postings: PostingsReader,
}

impl SegmentReader {
    pub fn open(dir: &FSDirectory, sci: &SegmentCommitInfo) -> io::Result<SegmentReader> {
        let segment = &sci.info.name;
        let segment_id = &sci.info.id;
        let field_infos = FieldInfos::read(dir, segment, segment_id, "")?;
        let terms = TermsDict::open(dir, segment, segment_id, &field_infos)?;
        let postings = PostingsReader::open(dir, segment, segment_id)?;
        Ok(SegmentReader {
            max_doc: sci.info.doc_count,
            field_infos,
            terms,
            postings,
        })
    }

    pub fn max_doc(&self) -> i32 {
        self.max_doc
    }

    /// Term lookup: field resolution + terms-dict seek. Returns
    /// `(has_freqs, entry)`; `None` covers unknown field, non-indexed field
    /// (IndexOptions.NONE), and term-not-present — all empty-hit cases,
    /// matching Java TermQuery semantics.
    pub(crate) fn seek_term(
        &mut self,
        field: &str,
        term: &[u8],
    ) -> io::Result<Option<(bool, TermEntry)>> {
        let Some(fi) = self.field_infos.by_name(field) else {
            return Ok(None);
        };
        if fi.index_options == IndexOptions::None {
            return Ok(None);
        }
        let has_freqs = fi.index_options != IndexOptions::Docs;
        let Some(entry) = self.terms.seek_exact(fi, term)? else {
            return Ok(None);
        };
        Ok(Some((has_freqs, entry)))
    }

    pub(crate) fn docs_enum(&self, entry: &TermEntry) -> io::Result<DocsEnum> {
        self.postings.docs(entry)
    }

    pub(crate) fn docs_freqs_enum(
        &self,
        entry: &TermEntry,
        needs_freq: bool,
    ) -> io::Result<DocsFreqsEnum> {
        if needs_freq {
            self.postings.docs_and_freqs(entry)
        } else {
            self.postings.docs_and_freqs_no_freq(entry)
        }
    }

    /// Look up a field info by name (for Boolean query construction).
    pub(crate) fn field_info(&self, name: &str) -> Option<&FieldInfo> {
        self.field_infos.by_name(name)
    }

    /// Whether the field indexes freqs (IndexOptions >= DOCS_AND_FREQS);
    /// None = unknown field (empty-hit semantics).
    pub(crate) fn field_has_freqs(&self, field: &str) -> Option<bool> {
        self.field_infos
            .by_name(field)
            .map(|fi| fi.index_options != IndexOptions::Docs && fi.index_options != IndexOptions::None)
    }

    /// TermsIter over a field by name (None = unknown field; a field with no
    /// .tmd record yields an immediately-exhausted iterator). Borrows the
    /// terms dict mutably for the iterator's lifetime.
    pub(crate) fn terms_iter(&mut self, field: &str) -> Option<TermsIter<'_>> {
        let fi = self.field_infos.by_name(field)?;
        Some(self.terms.terms_iter(fi))
    }
}
