//! Unified leaf-node data access interface (spec: 2026-07-27-leaf-access-unification-design.md §4).
//! Both disk segments (SegmentReader) and in-memory buffers (MemoryLeafAccess)
//! implement this trait; Query::segment_iterator is generic over it.

use std::io;

use codec_lucene9::field_infos::FieldInfo;
use codec_lucene9::roaring::FrozenBitmap;

use super::doc_iter::SegmentDocIter;

/// Lightweight term metadata returned by TermsIterAccess::next().
/// Disk side wraps TermEntry; memory side wraps term_id.
#[derive(Clone, Debug)]
pub struct TermEntryLike {
    pub doc_freq: u32,
    pub total_term_freq: u64,
    /// Opaque handle for the concrete LeafAccess impl to interpret.
    /// Disk: index into TermsDict; Memory: term_id in TermDict.
    pub handle: u64,
}

/// Unified term enumeration interface (disk FST streaming / memory sorted array).
pub trait TermsIterAccess {
    /// Seek to the first term >= target. Returns true if a term was found.
    fn seek_ceil(&mut self, target: &[u8]) -> io::Result<bool>;
    /// Advance to the next term. Returns None when exhausted.
    fn next(&mut self) -> io::Result<Option<(Vec<u8>, TermEntryLike)>>;
}

/// Unified points interface (disk BKD tree / memory linear scan).
pub trait PointsAccess {
    fn intersect(
        &self,
        field: &str,
        low: i64,
        high: i64,
        visitor: &mut dyn FnMut(i64, i32),
    ) -> io::Result<()>;
}

/// Unified leaf-node data access. Query execution is generic over this trait.
pub trait LeafAccess {
    /// Concrete term handle type (disk: TermEntry, memory: MemTermHandle).
    type TermHandle;

    fn max_doc(&self) -> i32;

    /// Term lookup. Returns (has_freqs, handle) or None if field/term absent.
    fn seek_term(
        &mut self,
        field: &str,
        term: &[u8],
    ) -> io::Result<Option<(bool, Self::TermHandle)>>;

    /// Docs-only postings iterator for a term.
    fn docs_enum(&self, entry: &Self::TermHandle) -> io::Result<SegmentDocIter>;

    /// Docs+freqs postings iterator. If needs_freq=false, may skip freq decoding.
    fn docs_freqs_enum(
        &self,
        entry: &Self::TermHandle,
        needs_freq: bool,
    ) -> io::Result<SegmentDocIter>;

    /// Positions iterator for phrase queries. Returns a SegmentDocIter::Phrase.
    fn positions_enum(&self, entry: &Self::TermHandle) -> io::Result<SegmentDocIter>;

    /// Open inline roaring bitmap for a term. Memory always returns None.
    fn open_term_bitmap(
        &self,
        entry: &Self::TermHandle,
    ) -> io::Result<Option<FrozenBitmap>>;

    /// Field metadata lookup.
    fn field_info(&self, name: &str) -> Option<&FieldInfo>;

    /// Whether the field indexes freqs (IndexOptions >= DOCS_AND_FREQS).
    fn field_has_freqs(&self, field: &str) -> Option<bool>;

    /// Term enumeration for prefix/wildcard queries.
    fn terms_iter(&mut self, field: &str) -> Option<Box<dyn TermsIterAccess + '_>>;

    /// Points reader for range queries.
    fn points_reader(&self) -> Option<&dyn PointsAccess>;

    /// Numeric doc value for sort keys.
    fn numeric_dv(&self, field: &str, doc: u32) -> Option<i64>;

    /// doc_freq for a term handle (used by fast_segment_count).
    fn term_doc_freq(&self, entry: &Self::TermHandle) -> u32;
}
