//! Per-segment read view (search spec §3 SegmentReader): field infos +
//! terms dict + postings of one segment. Open reads only segments_N/.si/.fnm
//! + codec headers; field FSTs load lazily inside the terms dict.

use std::io;

use codec_lucene9::automaton::WildcardDfa;
use codec_lucene9::directory::FSDirectory;
use codec_lucene9::doc_values_read::{BinaryDocValues, DocValuesReader, SortedDocValues};
use codec_lucene9::field_infos::{FieldInfo, FieldInfos, IndexOptions};
use codec_lucene9::points_read::PointsReader;
use codec_lucene9::postings_read::{DocsEnum, DocsFreqsEnum, PositionsEnum, PostingsReader};
use codec_lucene9::roaring::FrozenBitmap;
use codec_lucene9::segment_info::IndexSortFieldInfo;
use codec_lucene9::segment_infos::SegmentCommitInfo;
use codec_lucene9::terms_read::{TermEntry, TermsDict, TermsIter};

const DV_SUFFIX: &str = "Lucene90_0";

use super::doc_iter::{PhraseDocIter, PositionsEnumLike, SegmentDocIter};
use super::leaf_access::{LeafAccess, PointsAccess, TermsIterAccess};

pub struct SegmentReader {
    max_doc: i32,
    field_infos: FieldInfos,
    terms: TermsDict,
    postings: PostingsReader,
    points: Option<PointsReader>,
    index_sort: Vec<IndexSortFieldInfo>,
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
        let doc_values = DocValuesReader::open(dir, segment, segment_id, DV_SUFFIX).ok();
        Ok(SegmentReader {
            max_doc: sci.info.doc_count,
            field_infos,
            terms,
            postings,
            points,
            index_sort: sci.info.index_sort.clone(),
            doc_values,
        })
    }

    pub fn max_doc(&self) -> i32 {
        self.max_doc
    }

    /// Segment-level index sort metadata (empty = unsorted). Phase D early
    /// termination keys off this: a segment sorted by field F yields hits in
    /// docID order == F order, so field-sorted top-N can stop after N hits.
    pub(crate) fn index_sort(&self) -> &[IndexSortFieldInfo] {
        &self.index_sort
    }

    /// Numeric DocValues for a field as (doc, value) ascending by doc.
    /// Reuses the DocValuesReader opened with the segment. Unknown field or
    /// segment without DV data → empty Vec.
    pub fn numeric_values(&self, field: &str) -> io::Result<Vec<(u32, i64)>> {
        let Some(fi) = self.field_infos.by_name(field) else {
            return Ok(Vec::new());
        };
        let Some(dv) = self.doc_values.as_ref() else {
            return Ok(Vec::new());
        };
        dv.numeric_values(fi.number)
    }

    /// Binary DocValues for a field as (doc, bytes) ascending by doc.
    /// Reuses the DocValuesReader opened with the segment. Unknown field or
    /// segment without DV data → empty Vec.
    pub fn binary_values(&self, field: &str) -> io::Result<Vec<(u32, Vec<u8>)>> {
        let Some(fi) = self.field_infos.by_name(field) else {
            return Ok(Vec::new());
        };
        let Some(dv) = self.doc_values.as_ref() else {
            return Ok(Vec::new());
        };
        dv.binary_values(fi.number)
    }

    /// Binary DocValues for a field in packed form: `(doc_ids, data, offsets)`
    /// where `data` is one contiguous buffer and `offsets[i]` is the
    /// `(start, end)` of doc `doc_ids[i]`'s value within `data`. Ascending by
    /// doc. Avoids one allocation per doc — useful when holding a whole field
    /// (e.g. shard merge). Unknown field or segment without DV data → empty.
    pub fn binary_values_packed(
        &self,
        field: &str,
    ) -> io::Result<(Vec<u32>, Vec<u8>, Vec<(usize, usize)>)> {
        let Some(fi) = self.field_infos.by_name(field) else {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        };
        let Some(dv) = self.doc_values.as_ref() else {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        };
        dv.binary_values_packed(fi.number)
    }

    /// Sorted DocValues for a field as (doc, term_bytes) ascending by doc.
    /// Combines sorted_ords (doc → ord) with sorted_dict (ord → bytes).
    /// Reuses the DocValuesReader opened with the segment. Unknown field or
    /// segment without DV data → empty Vec.
    pub fn sorted_values(&self, field: &str) -> io::Result<Vec<(u32, Vec<u8>)>> {
        let Some(fi) = self.field_infos.by_name(field) else {
            return Ok(Vec::new());
        };
        let Some(dv) = self.doc_values.as_ref() else {
            return Ok(Vec::new());
        };
        let ords = dv.sorted_ords(fi.number)?;
        if ords.is_empty() {
            return Ok(Vec::new());
        }
        let dict = dv.sorted_dict(fi.number)?;
        Ok(ords
            .into_iter()
            .map(|(doc, ord)| {
                let bytes = dict
                    .get(ord as usize)
                    .cloned()
                    .unwrap_or_default();
                (doc, bytes)
            })
            .collect())
    }

    /// Binary DocValues as a zero-copy streaming iterator: `next` yields
    /// `(doc, &[u8])` borrowed straight from the .dvd mmap — no per-doc
    /// allocation (Java `BytesRef` semantics). Unknown field, segment
    /// without DV data, or field without a BINARY DV entry → NotFound
    /// error (the iterator borrows the reader, so unlike the Vec-returning
    /// APIs it cannot materialize an "empty" value without one).
    pub fn binary_doc_values(&self, field: &str) -> io::Result<BinaryDocValues<'_>> {
        let fi = self.field_infos.by_name(field).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown field {field}"),
            )
        })?;
        let dv = self.doc_values.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "segment has no doc-values data",
            )
        })?;
        dv.binary_doc_values(fi.number)
    }

    /// Sorted DocValues as a zero-copy streaming iterator: `next` yields
    /// `(doc, &[u8])` borrowed from the iterator's own dict buffer. Unknown
    /// field, segment without DV data, or field without a SORTED DV entry →
    /// NotFound error (same rationale as `binary_doc_values`).
    pub fn sorted_doc_values(&self, field: &str) -> io::Result<SortedDocValues> {
        let fi = self.field_infos.by_name(field).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown field {field}"),
            )
        })?;
        let dv = self.doc_values.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "segment has no doc-values data",
            )
        })?;
        dv.sorted_doc_values(fi.number)
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
    type TermHandle = TermEntry;

    fn seek_ceil(&mut self, target: &[u8]) -> io::Result<bool> {
        self.inner.seek_ceil(target)
    }
    fn next(&mut self) -> io::Result<Option<(Vec<u8>, TermEntry)>> {
        self.inner.next()
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

    fn intersect_docs(
        &self,
        field: &str,
        low: i64,
        high: i64,
        visitor: &mut dyn FnMut(u32),
    ) -> io::Result<()> {
        PointsReader::intersect_docs(self, field, low, high, visitor)
    }

    fn field_bounds(&self, field: &str) -> Option<(i64, i64, u32)> {
        PointsReader::field_bounds(self, field)
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
            vec![(entry.doc_freq, 0, PositionsEnumLike::Disk(en))],
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

    fn terms_iter(
        &mut self,
        field: &str,
    ) -> Option<Box<dyn TermsIterAccess<TermHandle = TermEntry> + '_>> {
        Some(Box::new(DiskTermsIter {
            inner: SegmentReader::terms_iter(self, field)?,
        }))
    }

    fn intersect_terms(
        &mut self,
        field: &str,
        dfa: &WildcardDfa,
    ) -> io::Result<Option<Vec<(Vec<u8>, TermEntry)>>> {
        let fi = match self.field_infos.by_name(field) {
            Some(fi) => fi,
            None => return Ok(None),
        };
        let results = self.terms.intersect(fi, dfa)?;
        Ok(Some(results))
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

/// 块级迭代路径开关（spec 2026-07-26 §5）：默认 ON；`RL_BLOCK=0`
/// 三个 driver 逐行回到 per-doc 路径。镜像 bitmap_enabled() 的
/// OnceLock 形态——env 只作 runtime/bench 逃生门，测试直调显式
/// drive 函数对拍（spec §7-2）。
pub(crate) fn block_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("RL_BLOCK").map_or(true, |v| v != "0"))
}
