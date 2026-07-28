//! Per-segment read view (search spec §3 SegmentReader): field infos +
//! terms dict + postings of one segment. Open reads only segments_N/.si/.fnm
//! + codec headers; field FSTs load lazily inside the terms dict.

use std::io;

use codec_lucene9::directory::FSDirectory;
use codec_lucene9::doc_values_read::DocValuesReader;
use codec_lucene9::field_infos::{FieldInfo, FieldInfos, IndexOptions};
use codec_lucene9::points_read::PointsReader;
use codec_lucene9::postings_read::{DocsEnum, DocsFreqsEnum, PositionsEnum, PostingsReader};
use codec_lucene9::roaring::FrozenBitmap;
use codec_lucene9::segment_infos::SegmentCommitInfo;
use codec_lucene9::terms_read::{TermEntry, TermsDict, TermsIter};

use super::doc_iter::{PhraseDocIter, SegmentDocIter};
use super::leaf_access::{LeafAccess, PointsAccess, TermEntryLike, TermsIterAccess};

pub struct SegmentReader {
    max_doc: i32,
    field_infos: FieldInfos,
    terms: TermsDict,
    postings: PostingsReader,
    points: Option<PointsReader>,
    doc_values: Option<DocValuesReader>,
}

impl SegmentReader {
    pub fn open(dir: &FSDirectory, sci: &SegmentCommitInfo) -> io::Result<SegmentReader> {
        let segment = &sci.info.name;
        let segment_id = &sci.info.id;
        let field_infos = FieldInfos::read(dir, segment, segment_id, "")?;
        let terms = TermsDict::open(dir, segment, segment_id, &field_infos)?;
        let postings = PostingsReader::open(dir, segment, segment_id)?;
        let points = PointsReader::open(dir, segment, segment_id, &field_infos)?;
        let dv_suffix = "Lucene90_0";
        let doc_values = DocValuesReader::open(dir, segment, segment_id, dv_suffix).ok();
        Ok(SegmentReader {
            max_doc: sci.info.doc_count,
            field_infos,
            terms,
            postings,
            points,
            doc_values,
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

    /// EverythingEnum over a positions field (PhraseDocIter construction).
    pub(crate) fn positions_enum(&self, entry: &TermEntry) -> io::Result<PositionsEnum> {
        self.postings.positions(entry)
    }

    /// Frozen-view open of the term's inline bitmap (M5 §2/§3) for Term
    /// iteration / OR / AND merge + probes. None → postings fallback.
    pub(crate) fn open_term_bitmap(&self, entry: &TermEntry) -> io::Result<Option<FrozenBitmap>> {
        if !bitmap_enabled() {
            return Ok(None);
        }
        self.postings.open_term_bitmap(entry, self.max_doc as u32)
    }

    /// Look up a field info by name (for Boolean query construction).
    pub(crate) fn field_info(&self, name: &str) -> Option<&FieldInfo> {
        self.field_infos.by_name(name)
    }

    /// Whether the field indexes freqs (IndexOptions >= DOCS_AND_FREQS);
    /// None = unknown field (empty-hit semantics).
    pub(crate) fn field_has_freqs(&self, field: &str) -> Option<bool> {
        self.field_infos.by_name(field).map(|fi| {
            fi.index_options != IndexOptions::Docs && fi.index_options != IndexOptions::None
        })
    }

    /// TermsIter over a field by name (None = unknown field; a field with no
    /// .tmd record yields an immediately-exhausted iterator). Borrows the
    /// terms dict mutably for the iterator's lifetime.
    pub(crate) fn terms_iter(&mut self, field: &str) -> Option<TermsIter<'_>> {
        let fi = self.field_infos.by_name(field)?;
        Some(self.terms.terms_iter(fi))
    }

    /// Per-segment points reader (M6 §3.2); `None` when the segment has no
    /// points files (segment_builder.rs:240-245).
    pub(crate) fn points_reader(&self) -> Option<&PointsReader> {
        self.points.as_ref()
    }

    /// Read a numeric doc value by field name and local doc id.
    /// Returns None if the field has no DV, the segment has no DV files,
    /// or the doc has no value (sparse).
    pub fn numeric_dv(&self, field: &str, doc: u32) -> Option<i64> {
        let fi = self.field_infos.by_name(field)?;
        let dv = self.doc_values.as_ref()?;
        // Note: reads all (doc, value) pairs per call. For high-frequency use,
        // a per-search cache would be appropriate. Acceptable for <100 QPS.
        let pairs = dv.numeric_values(fi.number).ok()?;
        // pairs is Vec<(u32, i64)> sorted by doc — binary search
        pairs
            .binary_search_by(|&(d, _)| d.cmp(&doc))
            .ok()
            .map(|idx| pairs[idx].1)
    }
}

// ── DiskTermsIter adapter (TermsIterAccess over disk FST streaming) ────

struct DiskTermsIter<'a> {
    inner: TermsIter<'a>,
}

impl TermsIterAccess for DiskTermsIter<'_> {
    fn seek_ceil(&mut self, target: &[u8]) -> io::Result<bool> {
        self.inner.seek_ceil(target)
    }
    fn next(&mut self) -> io::Result<Option<(Vec<u8>, TermEntryLike)>> {
        match self.inner.next()? {
            Some((term, entry)) => Ok(Some((
                term,
                TermEntryLike {
                    doc_freq: entry.doc_freq,
                    total_term_freq: entry.total_term_freq,
                    handle: 0, // not used for disk path
                },
            ))),
            None => Ok(None),
        }
    }
}

// ── PointsAccess for PointsReader ─────────────────────────────────────

impl PointsAccess for PointsReader {
    fn intersect(
        &self,
        field: &str,
        low: i64,
        high: i64,
        visitor: &mut dyn FnMut(i64, i32),
    ) -> io::Result<()> {
        PointsReader::intersect(self, field, low, high, visitor)
    }
}

// ── LeafAccess for SegmentReader ──────────────────────────────────────

impl LeafAccess for SegmentReader {
    type TermHandle = TermEntry;

    fn max_doc(&self) -> i32 {
        self.max_doc
    }

    fn seek_term(
        &mut self,
        field: &str,
        term: &[u8],
    ) -> io::Result<Option<(bool, TermEntry)>> {
        SegmentReader::seek_term(self, field, term)
    }

    fn docs_enum(&self, entry: &TermEntry) -> io::Result<SegmentDocIter> {
        Ok(SegmentDocIter::Docs(self.postings.docs(entry)?))
    }

    fn docs_freqs_enum(&self, entry: &TermEntry, needs_freq: bool) -> io::Result<SegmentDocIter> {
        if needs_freq {
            Ok(SegmentDocIter::Freqs(self.postings.docs_and_freqs(entry)?))
        } else {
            Ok(SegmentDocIter::Freqs(
                self.postings.docs_and_freqs_no_freq(entry)?,
            ))
        }
    }

    fn positions_enum(&self, entry: &TermEntry) -> io::Result<SegmentDocIter> {
        let en = SegmentReader::positions_enum(self, entry)?;
        Ok(SegmentDocIter::Phrase(PhraseDocIter::from_entries(
            vec![(entry.doc_freq, 0, en)],
            None,
        )))
    }

    fn open_term_bitmap(&self, entry: &TermEntry) -> io::Result<Option<FrozenBitmap>> {
        SegmentReader::open_term_bitmap(self, entry)
    }

    fn field_info(&self, name: &str) -> Option<&FieldInfo> {
        self.field_infos.by_name(name)
    }

    fn field_has_freqs(&self, field: &str) -> Option<bool> {
        SegmentReader::field_has_freqs(self, field)
    }

    fn terms_iter(&mut self, field: &str) -> Option<Box<dyn TermsIterAccess + '_>> {
        Some(Box::new(DiskTermsIter {
            inner: SegmentReader::terms_iter(self, field)?,
        }))
    }

    fn points_reader(&self) -> Option<&dyn PointsAccess> {
        SegmentReader::points_reader(self).map(|p| p as &dyn PointsAccess)
    }

    fn numeric_dv(&self, field: &str, doc: u32) -> Option<i64> {
        SegmentReader::numeric_dv(self, field, doc)
    }

    fn term_doc_freq(&self, entry: &TermEntry) -> u32 {
        entry.doc_freq
    }
}

/// Process-wide kill switch for the roaring read path (M3 §6 A/B
/// discipline; mirrors RL_SIMD=0 in postings_ll/simd.rs:56-62):
/// `RL_BITMAP=0` forces the postings fallback everywhere with the same
/// binary and index.
pub(crate) fn bitmap_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("RL_BITMAP").map_or(true, |v| v != "0"))
}
