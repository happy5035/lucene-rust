# LeafAccess Unification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unify the memory and disk query execution paths behind a single `LeafAccess` trait, eliminating ~200 lines of duplicated query logic in `memory_reader.rs`.

**Architecture:** Define a `LeafAccess` trait with associated `TermHandle` type that both `SegmentReader` (disk) and `MemoryLeafAccess` (RAM buffer) implement. Generify `Query::segment_iterator` and all helper functions to `<L: LeafAccess>`. Memory postings use new `MemDocs`/`MemFreqs` variants in `SegmentDocIter`; phrase positions adapt via `MemPositionsEnum` into the existing `PhraseDocIter`.

**Tech Stack:** Rust traits with associated types, `SegmentDocIter` enum dispatch, existing `DocIter` trait, `codec_lucene9` types (`TermEntry`, `FrozenBitmap`, `FieldInfo`, `FieldInfos`).

## Global Constraints

- `#![forbid(unsafe_code)]` in `crates/core/src/lib.rs` — no unsafe in core crate
- All existing 300+ tests must pass unchanged (disk path regression)
- `SegmentDocIter` enum size is 18.7KB (dominated by `Freqs` variant's inline 8KB buffer) — new variants must not increase it
- `ConjunctionDocIter`/`DisjunctionDocIter`/`PhraseDocIter` constructors currently take `&SegmentReader` — must be refactored to accept pre-built iterators or trait objects
- Roaring three-tier optimization is disk-only; memory `open_term_bitmap` returns `None` (tier 3 always)
- Cross-platform: `#[cfg(unix)]`/`#[cfg(windows)]` for positional reads (already done in io.rs)

---

### Task 1: Define LeafAccess trait and helper types

**Files:**
- Create: `crates/core/src/search/leaf_access.rs`
- Modify: `crates/core/src/search/mod.rs`

**Interfaces:**
- Produces: `LeafAccess` trait, `TermsIterAccess` trait, `PointsAccess` trait, `TermEntryLike` struct — consumed by Tasks 3, 4, 5, 6.

- [ ] **Step 1: Create `leaf_access.rs` with trait definitions**

```rust
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
```

- [ ] **Step 2: Register the module in `mod.rs`**

Add after `pub mod sorted_collector;` in `crates/core/src/search/mod.rs`:

```rust
pub mod leaf_access;
```

And add to the `pub use` block:

```rust
pub use leaf_access::{LeafAccess, PointsAccess, TermEntryLike, TermsIterAccess};
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p rustlucene-core`
Expected: PASS (trait definitions only, no impls yet)

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/search/leaf_access.rs crates/core/src/search/mod.rs
git commit -m "feat(search): define LeafAccess trait + TermsIterAccess + PointsAccess"
```

---

### Task 2: Add MemDocs/MemFreqs variants to SegmentDocIter

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`
- Modify: `crates/core/src/memory_reader.rs` (remove MemDocsIter/MemFreqsIter after move)

**Interfaces:**
- Consumes: `DocIter` trait (existing in doc_iter.rs)
- Produces: `SegmentDocIter::MemDocs(MemDocsIter)`, `SegmentDocIter::MemFreqs(MemFreqsIter)` — consumed by Task 5 (MemoryLeafAccess).

- [ ] **Step 1: Move MemDocsIter and MemFreqsIter into doc_iter.rs**

Add after the `MatchAllIter` section (around line 78) in `crates/core/src/search/doc_iter.rs`:

```rust
// ── Memory postings iterators ────────────────────────────────────────

/// DocIter over a sorted Vec<u32> (postings from in-memory buffer).
pub struct MemDocsIter {
    docs: Vec<u32>,
    pos: usize,
    doc: i32,
}

impl MemDocsIter {
    pub fn new(docs: Vec<u32>) -> Self {
        Self { docs, pos: 0, doc: -1 }
    }
}

impl DocIter for MemDocsIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.pos >= self.docs.len() {
            self.doc = NO_MORE_DOCS;
            return Ok(NO_MORE_DOCS);
        }
        self.doc = self.docs[self.pos] as i32;
        self.pos += 1;
        Ok(self.doc)
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target {
            return Ok(self.doc);
        }
        let t = target.max(0) as u32;
        match self.docs[self.pos..].binary_search(&t) {
            Ok(i) => {
                self.pos += i;
                self.doc = self.docs[self.pos] as i32;
                self.pos += 1;
            }
            Err(i) => {
                self.pos += i;
                if self.pos >= self.docs.len() {
                    self.doc = NO_MORE_DOCS;
                } else {
                    self.doc = self.docs[self.pos] as i32;
                    self.pos += 1;
                }
            }
        }
        Ok(self.doc)
    }
    fn freq(&self) -> u32 {
        1
    }
}

/// DocIter over (doc, freq) pairs from in-memory buffer.
pub struct MemFreqsIter {
    docs: Vec<u32>,
    freqs: Vec<u32>,
    pos: usize,
    doc: i32,
    cur_freq: u32,
}

impl MemFreqsIter {
    pub fn new(docs: Vec<u32>, freqs: Vec<u32>) -> Self {
        Self { docs, freqs, pos: 0, doc: -1, cur_freq: 1 }
    }
}

impl DocIter for MemFreqsIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.pos >= self.docs.len() {
            self.doc = NO_MORE_DOCS;
            return Ok(NO_MORE_DOCS);
        }
        self.doc = self.docs[self.pos] as i32;
        self.cur_freq = self.freqs[self.pos];
        self.pos += 1;
        Ok(self.doc)
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target {
            return Ok(self.doc);
        }
        let t = target.max(0) as u32;
        match self.docs[self.pos..].binary_search(&t) {
            Ok(i) => {
                self.pos += i;
                self.doc = self.docs[self.pos] as i32;
                self.cur_freq = self.freqs[self.pos];
                self.pos += 1;
            }
            Err(i) => {
                self.pos += i;
                if self.pos >= self.docs.len() {
                    self.doc = NO_MORE_DOCS;
                } else {
                    self.doc = self.docs[self.pos] as i32;
                    self.cur_freq = self.freqs[self.pos];
                    self.pos += 1;
                }
            }
        }
        Ok(self.doc)
    }
    fn freq(&self) -> u32 {
        self.cur_freq
    }
}
```

- [ ] **Step 2: Add MemDocs/MemFreqs variants to SegmentDocIter enum**

In the `pub enum SegmentDocIter` definition (line ~1493), add after `Excluding(ExcludingDocIter),`:

```rust
    MemDocs(MemDocsIter),
    MemFreqs(MemFreqsIter),
```

- [ ] **Step 3: Add match arms to DocIter impl for SegmentDocIter**

In `impl DocIter for SegmentDocIter`, add arms to each method:

`doc_id`:
```rust
            Self::MemDocs(m) => m.doc_id(),
            Self::MemFreqs(m) => m.doc_id(),
```

`next_doc`:
```rust
            Self::MemDocs(m) => m.next_doc(),
            Self::MemFreqs(m) => m.next_doc(),
```

`advance`:
```rust
            Self::MemDocs(m) => m.advance(t),
            Self::MemFreqs(m) => m.advance(t),
```

`freq`:
```rust
            Self::MemFreqs(m) => m.freq(),
```
(add to the existing match that includes `Self::Freqs(f) => f.freq(),`)

`matches`: no change needed (falls through to `_ => Ok(true)`)

- [ ] **Step 4: Remove MemDocsIter/MemFreqsIter from memory_reader.rs**

Delete the `MemDocsIter` and `MemFreqsIter` structs and their `DocIter` impls from `crates/core/src/memory_reader.rs` (lines ~214-313). Update any imports in that file if needed.

- [ ] **Step 5: Run tests**

Run: `cargo test -p rustlucene-core`
Expected: All tests PASS (the moved iterators are identical; memory_reader tests still use MemorySearcher which doesn't use these iterators directly in exec_query)

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/memory_reader.rs
git commit -m "feat(search): add MemDocs/MemFreqs variants to SegmentDocIter"
```

---

### Task 3: Implement LeafAccess for SegmentReader

**Files:**
- Modify: `crates/core/src/search/segment_reader.rs`
- Modify: `crates/core/src/search/doc_iter.rs` (refactor ConjunctionDocIter/DisjunctionDocIter/PhraseDocIter constructors)

**Interfaces:**
- Consumes: `LeafAccess` trait (Task 1), `SegmentDocIter::MemDocs/MemFreqs` (Task 2)
- Produces: `impl LeafAccess for SegmentReader` — consumed by Task 4 (generification).

This is the largest task. The key challenge: `ConjunctionDocIter::new`, `DisjunctionDocIter::new`, and `PhraseDocIter::new` currently take `&SegmentReader` directly. They must be refactored to accept pre-built `SegmentDocIter` sub-iterators or a `&dyn LeafAccess` reference.

- [ ] **Step 1: Refactor PostingsIter to be constructible from SegmentDocIter**

In `doc_iter.rs`, the internal `PostingsIter` enum wraps `DocsEnum`/`DocsFreqsEnum`. Add a third variant that wraps a `SegmentDocIter` directly (for memory-sourced iterators):

```rust
enum PostingsIter {
    Docs(DocsEnum),
    Freqs(DocsFreqsEnum),
    Generic(SegmentDocIter),
}
```

Add to each PostingsIter method:
```rust
    Self::Generic(g) => g.doc_id(),
    Self::Generic(g) => g.next_doc(),
    Self::Generic(g) => g.advance(t),
    Self::Generic(g) => g.freq(),
```

Add a constructor:
```rust
    fn from_segment_iter(it: SegmentDocIter) -> Self {
        PostingsIter::Generic(it)
    }
```

- [ ] **Step 2: Refactor ConjunctionDocIter::new to accept pre-built iterators**

Change signature from:
```rust
pub fn new(seg: &SegmentReader, field: &str, sorted_entries: &[(u32, TermEntry)], needs_freq: bool) -> io::Result<Self>
```
to:
```rust
pub fn from_iters(sub: Vec<PostingsIter>) -> io::Result<Self>
```

The caller (query.rs `and_segment_iterator`) will build the `PostingsIter` vec using the `LeafAccess` trait methods. Keep the old `new` as a convenience wrapper that calls `from_iters` (for backward compat during transition):

```rust
pub fn new(
    seg: &SegmentReader,
    field: &str,
    sorted_entries: &[(u32, TermEntry)],
    needs_freq: bool,
) -> io::Result<Self> {
    let fi = seg.field_info(field);
    let has_freqs = fi.map(|f| f.index_options != IndexOptions::Docs).unwrap_or(false);
    let mut sub = Vec::with_capacity(sorted_entries.len());
    for (_, entry) in sorted_entries {
        sub.push(PostingsIter::new(seg, entry, has_freqs, needs_freq)?);
    }
    Self::from_iters(sub)
}
```

- [ ] **Step 3: Same refactor for DisjunctionDocIter**

Change to `from_iters(sub: Vec<PostingsIter>)` with the same pattern.

- [ ] **Step 4: Refactor PhraseDocIter::new to accept a &mut dyn LeafAccess-like interface**

`PhraseDocIter::new` needs `seek_term`, `field_info`, `open_term_bitmap`, and `positions_enum`. Since it's called from `query.rs` which will be generic, change it to accept pre-built data:

```rust
pub fn from_entries(
    entries: Vec<(u32, u32, SegmentDocIter)>,  // (doc_freq, offset, positions_iter)
    approx_bitmaps: Option<Vec<FrozenBitmap>>,
) -> io::Result<Option<PhraseDocIter>>
```

Keep the old `new(seg, field, terms)` as a wrapper for the disk path during transition.

- [ ] **Step 5: Implement LeafAccess for SegmentReader**

Add to `crates/core/src/search/segment_reader.rs`:

```rust
use super::leaf_access::{LeafAccess, PointsAccess, TermEntryLike, TermsIterAccess};

impl LeafAccess for SegmentReader {
    type TermHandle = TermEntry;

    fn max_doc(&self) -> i32 {
        self.max_doc
    }

    fn seek_term(&mut self, field: &str, term: &[u8]) -> io::Result<Option<(bool, TermEntry)>> {
        SegmentReader::seek_term(self, field, term)
    }

    fn docs_enum(&self, entry: &TermEntry) -> io::Result<SegmentDocIter> {
        Ok(SegmentDocIter::Docs(self.postings.docs(entry)?))
    }

    fn docs_freqs_enum(&self, entry: &TermEntry, needs_freq: bool) -> io::Result<SegmentDocIter> {
        if needs_freq {
            Ok(SegmentDocIter::Freqs(self.postings.docs_and_freqs(entry)?))
        } else {
            Ok(SegmentDocIter::Freqs(self.postings.docs_and_freqs_no_freq(entry)?))
        }
    }

    fn positions_enum(&self, entry: &TermEntry) -> io::Result<SegmentDocIter> {
        Ok(SegmentDocIter::Phrase(PhraseDocIter::from_positions_enum(
            self.postings.positions(entry)?,
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
        let fi = self.field_infos.by_name(field)?;
        let it = self.terms.terms_iter(fi);
        Some(Box::new(DiskTermsIter { inner: it }))
    }

    fn points_reader(&self) -> Option<&dyn PointsAccess> {
        self.points.as_ref().map(|p| p as &dyn PointsAccess)
    }

    fn numeric_dv(&self, field: &str, doc: u32) -> Option<i64> {
        SegmentReader::numeric_dv(self, field, doc)
    }

    fn term_doc_freq(&self, entry: &TermEntry) -> u32 {
        entry.doc_freq
    }
}
```

- [ ] **Step 6: Create DiskTermsIter adapter**

In `segment_reader.rs`:

```rust
struct DiskTermsIter<'a> {
    inner: TermsIter<'a>,
}

impl TermsIterAccess for DiskTermsIter<'_> {
    fn seek_ceil(&mut self, target: &[u8]) -> io::Result<bool> {
        self.inner.seek_ceil(target)
    }
    fn next(&mut self) -> io::Result<Option<(Vec<u8>, TermEntryLike)>> {
        match self.inner.next()? {
            Some((term, entry)) => Ok(Some((term, TermEntryLike {
                doc_freq: entry.doc_freq,
                total_term_freq: entry.total_term_freq,
                handle: 0, // not used for disk path
            }))),
            None => Ok(None),
        }
    }
}
```

- [ ] **Step 7: Implement PointsAccess for PointsReader**

In `segment_reader.rs` or a new adapter:

```rust
impl PointsAccess for PointsReader {
    fn intersect(&self, field: &str, low: i64, high: i64, visitor: &mut dyn FnMut(i64, i32)) -> io::Result<()> {
        PointsReader::intersect(self, field, low, high, visitor)
    }
}
```

- [ ] **Step 8: Run tests**

Run: `cargo test -p rustlucene-core`
Expected: All tests PASS (pure refactor, behavior unchanged)

- [ ] **Step 9: Commit**

```bash
git add crates/core/src/search/segment_reader.rs crates/core/src/search/doc_iter.rs
git commit -m "feat(search): implement LeafAccess for SegmentReader + refactor iterator constructors"
```

---

### Task 4: Generify query.rs, multi_term.rs, roaring_exec.rs

**Files:**
- Modify: `crates/core/src/search/query.rs`
- Modify: `crates/core/src/search/multi_term.rs`
- Modify: `crates/core/src/search/roaring_exec.rs`
- Modify: `crates/core/src/search/searcher.rs`

**Interfaces:**
- Consumes: `LeafAccess` trait (Task 1), `impl LeafAccess for SegmentReader` (Task 3)
- Produces: Generic `segment_iterator<L: LeafAccess>`, `fast_segment_count<L: LeafAccess>` — consumed by Task 6 (IndexWriter::search).

- [ ] **Step 1: Generify `Query::segment_iterator`**

Change signature in `query.rs`:
```rust
pub(crate) fn segment_iterator<L: LeafAccess>(
    &self,
    seg: &mut L,
    needs_freq: bool,
) -> io::Result<Option<SegmentDocIter>>
```

Update all internal calls. The body logic stays identical — it calls `seg.seek_term()`, `seg.docs_enum()`, etc. which now go through the trait.

- [ ] **Step 2: Generify helper functions in query.rs**

Change all `&mut SegmentReader` / `&SegmentReader` parameters to generic:

```rust
fn and_segment_iterator<L: LeafAccess, T: AsRef<[u8]>>(seg: &mut L, field: &str, terms: &[T], needs_freq: bool) -> io::Result<Option<SegmentDocIter>>
fn or_segment_iterator<L: LeafAccess, T: AsRef<[u8]>>(seg: &mut L, field: &str, terms: &[T], needs_freq: bool) -> io::Result<Option<SegmentDocIter>>
fn bool_segment_iterator<L: LeafAccess>(seg: &mut L, clauses: &[(Occur, Query)], needs_freq: bool) -> io::Result<Option<SegmentDocIter>>
pub(crate) fn point_range_bitmap<L: LeafAccess>(seg: &mut L, field: &str, low: i64, high: i64) -> io::Result<Option<MaterializedBitmap>>
pub(crate) fn fast_segment_count<L: LeafAccess>(seg: &mut L, query: &Query) -> io::Result<Option<u64>>
pub(crate) fn materialize_bool_bitmap<L: LeafAccess>(seg: &mut L, clauses: &[(Occur, Query)], budget: u64, cost: &mut u64) -> io::Result<MatOutcome>
fn materialize_query_bitmap<L: LeafAccess>(seg: &mut L, query: &Query, budget: u64, cost: &mut u64) -> io::Result<MatOutcome>
fn term_entry_bitmap<L: LeafAccess>(seg: &L, entry: &L::TermHandle, has_freqs: bool, budget: u64, cost: &mut u64) -> io::Result<MatOutcome>
fn fold_term_entries<L: LeafAccess>(seg: &L, entries: &[(u32, L::TermHandle)], has_freqs: bool, is_and: bool, budget: u64, cost: &mut u64) -> io::Result<MatOutcome>
fn drive_materialize<L: LeafAccess>(seg: &mut L, query: &Query) -> io::Result<MatOutcome>
fn bool_segment_fast_count<L: LeafAccess>(seg: &mut L, clauses: &[(Occur, Query)]) -> io::Result<Option<u64>>
fn prohibited_count<L: LeafAccess>(seg: &mut L, clauses: &[(Occur, Query)]) -> io::Result<u64>
```

- [ ] **Step 3: Generify multi_term.rs**

```rust
pub(crate) fn collect_direct<L: LeafAccess>(seg: &mut L, field: &str, terms: &[Vec<u8>]) -> io::Result<Option<(bool, CollectedTerms<L::TermHandle>)>>
pub(crate) fn collect_prefix<L: LeafAccess>(seg: &mut L, field: &str, prefix: &[u8]) -> io::Result<Option<(bool, CollectedTerms<L::TermHandle>)>>
pub(crate) fn collect_wildcard<L: LeafAccess>(seg: &mut L, field: &str, pat: &WildcardPattern) -> io::Result<Option<(bool, CollectedTerms<L::TermHandle>)>>
pub(crate) fn segment_iterator<L: LeafAccess>(seg: &mut L, field: &str, has_freqs: bool, collected: &CollectedTerms<L::TermHandle>, needs_freq: bool) -> io::Result<Option<SegmentDocIter>>
pub(crate) fn bitset_count<L: LeafAccess>(seg: &L, has_freqs: bool, collected: &CollectedTerms<L::TermHandle>) -> io::Result<Option<u64>>
pub(crate) fn for_each_doc<L: LeafAccess>(seg: &L, entry: &L::TermHandle, has_freqs: bool, f: &mut impl FnMut(u32)) -> io::Result<()>
fn materialize<L: LeafAccess>(seg: &L, entries: &[(u32, L::TermHandle)], has_freqs: bool) -> io::Result<FixedBitSet>
```

`CollectedTerms` becomes generic:
```rust
pub(crate) struct CollectedTerms<H> {
    pub terms: Vec<Vec<u8>>,
    pub entries: Vec<(u32, H)>,
}
```

- [ ] **Step 4: Generify roaring_exec.rs**

```rust
pub(crate) fn collect_bool_entries<L: LeafAccess, T: AsRef<[u8]>>(seg: &mut L, field: &str, terms: &[T], is_and: bool) -> io::Result<Option<(bool, Vec<(u32, L::TermHandle)>)>>
pub(crate) fn segment_iterator<L: LeafAccess>(seg: &L, entries: &[(u32, L::TermHandle)], has_freqs: bool, is_and: bool) -> io::Result<Option<SegmentDocIter>>
pub(crate) fn count<L: LeafAccess>(seg: &L, entries: &[(u32, L::TermHandle)], has_freqs: bool, is_and: bool) -> io::Result<Option<u64>>
fn open_clauses<L: LeafAccess>(seg: &L, entries: &[(u32, L::TermHandle)]) -> io::Result<Option<Vec<Option<FrozenBitmap>>>>
fn materialize_docs<L: LeafAccess>(seg: &L, entry: &L::TermHandle, has_freqs: bool) -> io::Result<Vec<u32>>
fn and_iterator<L: LeafAccess>(seg: &L, entries: &[(u32, L::TermHandle)], has_freqs: bool) -> io::Result<Option<SegmentDocIter>>
fn or_iterator<L: LeafAccess>(seg: &L, entries: &[(u32, L::TermHandle)], has_freqs: bool) -> io::Result<Option<SegmentDocIter>>
```

- [ ] **Step 5: Update searcher.rs call sites**

`Searcher::search`, `Searcher::count`, `Searcher::top_docs` call `query.segment_iterator(seg, ...)` and `fast_segment_count(seg, ...)`. Since `SegmentReader` now implements `LeafAccess`, these calls compile unchanged — the generic is inferred as `L = SegmentReader`.

- [ ] **Step 6: Run tests**

Run: `cargo test -p rustlucene-core`
Expected: All 300+ tests PASS (compilation success = correctness for this pure generification)

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/search/query.rs crates/core/src/search/multi_term.rs crates/core/src/search/roaring_exec.rs crates/core/src/search/searcher.rs
git commit -m "feat(search): generify query execution over LeafAccess trait"
```

---

### Task 5: Implement MemoryLeafAccess

**Files:**
- Create: `crates/core/src/memory_access.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: `LeafAccess` trait (Task 1), `SegmentDocIter::MemDocs/MemFreqs` (Task 2), `DocWriter` internals
- Produces: `MemoryLeafAccess<'a>` implementing `LeafAccess` — consumed by Task 6 (IndexWriter::search).

- [ ] **Step 1: Create `memory_access.rs` with MemoryLeafAccess**

```rust
//! In-memory LeafAccess implementation: borrows DocWriter's RAM buffers
//! and provides data to the generic query execution engine.

use std::io;

use codec_lucene9::field_infos::{FieldInfo, FieldInfos, IndexOptions};
use codec_lucene9::roaring::FrozenBitmap;

use crate::doc_writer::DocWriter;
use crate::schema::Schema;
use crate::search::doc_iter::{MemDocsIter, MemFreqsIter, SegmentDocIter};
use crate::search::leaf_access::{LeafAccess, PointsAccess, TermEntryLike, TermsIterAccess};

/// Term handle for in-memory postings.
#[derive(Clone, Debug)]
pub struct MemTermHandle {
    pub term_id: u32,
    pub doc_freq: u32,
    pub total_term_freq: u64,
}

/// In-memory leaf: borrows DocWriter's RAM buffers, implements LeafAccess.
pub struct MemoryLeafAccess<'a> {
    dw: &'a DocWriter,
    field_infos: FieldInfos,
    points: Option<MemoryPointsAccess<'a>>,
}

impl<'a> MemoryLeafAccess<'a> {
    pub fn new(dw: &'a DocWriter, schema: &Schema) -> Self {
        let field_infos = build_field_infos(dw, schema);
        let points = Some(MemoryPointsAccess { dw });
        MemoryLeafAccess { dw, field_infos, points }
    }

    fn field_buf(&self, field: &str) -> Option<&crate::doc_writer::FieldBuf> {
        let number = self.dw.fields().iter().position(|f| f.name == field)?;
        self.dw.field_buffer(number as u32)
    }
}

impl<'a> LeafAccess for MemoryLeafAccess<'a> {
    type TermHandle = MemTermHandle;

    fn max_doc(&self) -> i32 {
        self.dw.max_doc as i32
    }

    fn seek_term(&mut self, field: &str, term: &[u8]) -> io::Result<Option<(bool, MemTermHandle)>> {
        let Some(spec) = self.field_infos.by_name(field) else {
            return Ok(None);
        };
        if spec.index_options == IndexOptions::None {
            return Ok(None);
        }
        let Some(buf) = self.field_buf(field) else {
            return Ok(None);
        };
        let Some(dict) = &buf.dict else {
            return Ok(None);
        };
        let Some(id) = dict.find(term) else {
            return Ok(None);
        };
        let pb = dict.postings(id);
        let has_freqs = spec.index_options != IndexOptions::Docs;
        let doc_freq = pb.docs.len() as u32;
        let total_term_freq: u64 = pb.freqs.iter().map(|&f| f as u64).sum();
        Ok(Some((has_freqs, MemTermHandle { term_id: id, doc_freq, total_term_freq })))
    }

    fn docs_enum(&self, entry: &MemTermHandle) -> io::Result<SegmentDocIter> {
        let docs = self.postings_docs(entry.term_id);
        Ok(SegmentDocIter::MemDocs(MemDocsIter::new(docs)))
    }

    fn docs_freqs_enum(&self, entry: &MemTermHandle, needs_freq: bool) -> io::Result<SegmentDocIter> {
        if needs_freq {
            let (docs, freqs) = self.postings_docs_freqs(entry.term_id);
            Ok(SegmentDocIter::MemFreqs(MemFreqsIter::new(docs, freqs)))
        } else {
            let docs = self.postings_docs(entry.term_id);
            Ok(SegmentDocIter::MemDocs(MemDocsIter::new(docs)))
        }
    }

    fn positions_enum(&self, entry: &MemTermHandle) -> io::Result<SegmentDocIter> {
        // Build a MemPositionsEnum and wrap in PhraseDocIter
        // (detailed in Step 2)
        todo!()
    }

    fn open_term_bitmap(&self, _entry: &MemTermHandle) -> io::Result<Option<FrozenBitmap>> {
        Ok(None) // memory never has roaring bitmaps
    }

    fn field_info(&self, name: &str) -> Option<&FieldInfo> {
        self.field_infos.by_name(name)
    }

    fn field_has_freqs(&self, field: &str) -> Option<bool> {
        self.field_infos.by_name(field).map(|fi| {
            fi.index_options != IndexOptions::Docs && fi.index_options != IndexOptions::None
        })
    }

    fn terms_iter(&mut self, field: &str) -> Option<Box<dyn TermsIterAccess + '_>> {
        let buf = self.field_buf(field)?;
        let dict = buf.dict.as_ref()?;
        Some(Box::new(MemTermsIter::new(dict)))
    }

    fn points_reader(&self) -> Option<&dyn PointsAccess> {
        self.points.as_ref().map(|p| p as &dyn PointsAccess)
    }

    fn numeric_dv(&self, field: &str, doc: u32) -> Option<i64> {
        self.dw.numeric_dv(field, doc)
    }

    fn term_doc_freq(&self, entry: &MemTermHandle) -> u32 {
        entry.doc_freq
    }
}

// Helper methods
impl<'a> MemoryLeafAccess<'a> {
    fn postings_docs(&self, term_id: u32) -> Vec<u32> {
        // Navigate: field_buf → dict → postings(term_id) → docs.clone()
        // Need to find which field owns this term_id — store field in MemTermHandle
        // or iterate fields. Simplest: store field_number in MemTermHandle.
        todo!()
    }

    fn postings_docs_freqs(&self, term_id: u32) -> (Vec<u32>, Vec<u32>) {
        todo!()
    }
}
```

Note: `MemTermHandle` needs a `field_number: u32` field to navigate back to the correct `FieldBuf`. Add it in the actual implementation.

- [ ] **Step 2: Implement MemPositionsEnum**

```rust
/// Adapts in-memory positions data to the PositionsEnum interface,
/// allowing PhraseDocIter to work unchanged.
struct MemPositionsEnum {
    docs: Vec<u32>,
    positions: Vec<Vec<u32>>,  // positions[doc_idx] = sorted position list
    doc_idx: usize,
    pos_idx: usize,
    doc: i32,
}

impl MemPositionsEnum {
    fn doc_id(&self) -> i32 { self.doc }
    fn next_doc(&mut self) -> io::Result<i32> { /* advance doc_idx */ }
    fn freq(&self) -> u32 { self.positions[self.doc_idx].len() as u32 }
    fn next_position(&mut self) -> io::Result<u32> { /* advance pos_idx */ }
}
```

- [ ] **Step 3: Implement MemTermsIter**

```rust
struct MemTermsIter<'a> {
    dict: &'a crate::doc_writer::TermDict,
    sorted_ids: Vec<u32>,
    pos: usize,
}

impl<'a> MemTermsIter<'a> {
    fn new(dict: &'a crate::doc_writer::TermDict) -> Self {
        let sorted_ids = dict.sorted_ids();
        MemTermsIter { dict, sorted_ids, pos: 0 }
    }
}

impl TermsIterAccess for MemTermsIter<'_> {
    fn seek_ceil(&mut self, target: &[u8]) -> io::Result<bool> {
        // Binary search for first term >= target
        let result = self.sorted_ids[self.pos..].binary_search_by(|&id| {
            self.dict.bytes_of(id).cmp(target)
        });
        match result {
            Ok(i) => { self.pos += i; Ok(true) }
            Err(i) => {
                self.pos += i;
                Ok(self.pos < self.sorted_ids.len())
            }
        }
    }

    fn next(&mut self) -> io::Result<Option<(Vec<u8>, TermEntryLike)>> {
        if self.pos >= self.sorted_ids.len() {
            return Ok(None);
        }
        let id = self.sorted_ids[self.pos];
        self.pos += 1;
        let bytes = self.dict.bytes_of(id).to_vec();
        let pb = self.dict.postings(id);
        let doc_freq = pb.docs.len() as u32;
        let total_term_freq: u64 = pb.freqs.iter().map(|&f| f as u64).sum();
        Ok(Some((bytes, TermEntryLike {
            doc_freq,
            total_term_freq,
            handle: id as u64,
        })))
    }
}
```

- [ ] **Step 4: Implement MemoryPointsAccess**

```rust
struct MemoryPointsAccess<'a> {
    dw: &'a DocWriter,
}

impl PointsAccess for MemoryPointsAccess<'_> {
    fn intersect(&self, field: &str, low: i64, high: i64, visitor: &mut dyn FnMut(i64, i32)) -> io::Result<()> {
        let number = self.dw.fields().iter().position(|f| f.name == field);
        let Some(number) = number else { return Ok(()) };
        let Some(buf) = self.dw.field_buffer(number as u32) else { return Ok(()) };
        let Some(points) = &buf.points else { return Ok(()) };
        for &(v, doc) in &points.points {
            if v >= low && v <= high {
                visitor(v, doc as i32);
            }
        }
        Ok(())
    }
}
```

- [ ] **Step 5: Implement build_field_infos helper**

```rust
fn build_field_infos(dw: &DocWriter, schema: &Schema) -> FieldInfos {
    // Mirror segment_builder.rs finalize logic:
    // field number = schema index, IndexOptions from FieldSpec
    let mut infos = Vec::new();
    for (number, spec) in dw.fields().iter().enumerate() {
        let mut fi = FieldInfo::stored(&spec.name, number as i32);
        if spec.is_indexed() {
            fi.index_options = spec.index_options;
        }
        infos.push(fi);
    }
    FieldInfos::new(infos)
}
```

- [ ] **Step 6: Register module in lib.rs**

Add `pub mod memory_access;` to `crates/core/src/lib.rs`.

- [ ] **Step 7: Write unit tests for MemoryLeafAccess**

Test seek_term, docs_enum, terms_iter, points_reader, numeric_dv against a DocWriter with known data. Verify results match the old MemorySearcher behavior.

- [ ] **Step 8: Run tests**

Run: `cargo test -p rustlucene-core`
Expected: All tests PASS

- [ ] **Step 9: Commit**

```bash
git add crates/core/src/memory_access.rs crates/core/src/lib.rs
git commit -m "feat(search): implement MemoryLeafAccess for in-memory query execution"
```

---

### Task 6: Switch IndexWriter::search() to unified drive_segment

**Files:**
- Modify: `crates/core/src/index_writer.rs`

**Interfaces:**
- Consumes: `MemoryLeafAccess` (Task 5), generic `Query::segment_iterator<L: LeafAccess>` (Task 4)
- Produces: Unified `IndexWriter::search()` that drives both disk and memory through the same code path.

- [ ] **Step 1: Replace IndexWriter::search() body with drive_segment**

Replace the current `search()` implementation (which calls `exec_query_on_segment` for disk and `MemorySearcher` for memory) with:

```rust
pub fn search(&self, query: &Query, sort_field: Option<(&str, bool)>, top_n: usize) -> io::Result<SearchResults> {
    let (desc, n) = match sort_field {
        Some((_, desc)) => (desc, top_n),
        None => (false, top_n),
    };
    let mut collector = SortedTopN::new(desc, n);
    let use_sort = sort_field.is_some();

    // 1. Disk segments
    let mut doc_base: i32 = 0;
    for sci in &self.infos.segments {
        let mut reader = SegmentReader::open(&self.dir, sci)?;
        Self::drive_segment(&mut reader, query, &mut collector, doc_base, sort_field)?;
        doc_base += sci.info.doc_count;
    }

    // 2. In-memory buffer (same code path)
    if let Some(builder) = &self.builder {
        let mut mem = MemoryLeafAccess::new(builder.doc_writer(), &self.schema);
        Self::drive_segment(&mut mem, query, &mut collector, doc_base, sort_field)?;
    }

    if use_sort {
        Ok(collector.results())
    } else {
        // INDEXORDER: collector still works (sort_value=0 for all, tie-break by doc_id)
        Ok(collector.results())
    }
}

fn drive_segment<L: LeafAccess>(
    seg: &mut L,
    query: &Query,
    collector: &mut SortedTopN,
    doc_base: i32,
    sort_field: Option<(&str, bool)>,
) -> io::Result<()> {
    // fast_segment_count shortcut for total
    if let Some(mut iter) = query.segment_iterator(seg, false)? {
        loop {
            let doc = iter.next_doc()?;
            if doc == NO_MORE_DOCS { break; }
            if !iter.matches()? { continue; }
            let global_id = doc_base + doc;
            let sv = match sort_field {
                Some((field, _)) => seg.numeric_dv(field, doc as u32).unwrap_or(i64::MIN),
                None => 0,
            };
            collector.collect(global_id, sv);
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Remove exec_query_on_segment helper**

Delete the old `exec_query_on_segment` method from IndexWriter.

- [ ] **Step 3: Update imports**

Add `use crate::memory_access::MemoryLeafAccess;` and `use crate::search::leaf_access::LeafAccess;` to index_writer.rs.

- [ ] **Step 4: Run tests**

Run: `cargo test -p rustlucene-core`
Expected: All tests PASS (including the 5 IndexWriter::search tests from Task 4 of the RwLock plan, and the concurrent tests)

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/index_writer.rs
git commit -m "feat(search): IndexWriter::search() unified drive_segment over LeafAccess"
```

---

### Task 7: Delete old MemorySearcher execution logic

**Files:**
- Modify: `crates/core/src/memory_reader.rs` (gut it)
- Modify: `crates/core/src/lib.rs` (remove or keep as thin re-export)
- Modify: `crates/core/src/index_writer.rs` (remove MemorySearcher imports)

**Interfaces:**
- Consumes: All previous tasks complete, all tests passing via new path.
- Produces: Clean codebase with single query execution path.

- [ ] **Step 1: Identify what's still used from memory_reader.rs**

Check what `memory_reader.rs` exports that other code still references:
- `MemorySearcher` — used by old IndexWriter::search (now replaced)
- `MemoryLeafReader` — used by MemorySearcher (now replaced by MemoryLeafAccess)
- `LeafReader` trait — replaced by LeafAccess
- `MemDocsIter`/`MemFreqsIter` — already moved to doc_iter.rs in Task 2
- `TermMeta` — replaced by MemTermHandle

- [ ] **Step 2: Delete memory_reader.rs content**

Remove `MemorySearcher`, `MemoryLeafReader`, `LeafReader` trait, `TermMeta`, `WildcardPattern`, `intersect_sorted`, `union_sorted`, `difference_sorted`, and all the `exec_query` logic.

Keep the file as a thin re-export if needed for backward compat, or delete entirely and update `lib.rs`.

- [ ] **Step 3: Update lib.rs**

Remove `pub mod memory_reader;` (or replace with a re-export of `MemoryLeafAccess` from `memory_access` if external code needs it).

- [ ] **Step 4: Migrate the 12 memory_reader tests**

The existing tests in `memory_reader.rs` (memory_term_query, memory_matchall, memory_and_or, etc.) should be rewritten to use `IndexWriter::search()` instead of `MemorySearcher`. Move them to `index_writer.rs` tests or a new integration test file.

- [ ] **Step 5: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests PASS

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor(search): remove old MemorySearcher exec_query — unified LeafAccess path"
```

---

### Task 8: Final regression + equivalence battery

**Files:**
- Modify: `crates/core/src/index_writer.rs` (add equivalence tests)

**Interfaces:**
- Consumes: All previous tasks.
- Produces: Confidence that the unified path produces identical results to the old path.

- [ ] **Step 1: Write equivalence battery test**

Add a comprehensive test that exercises all query types through `IndexWriter::search()` on unflushed data:

```rust
#[test]
fn memory_search_equivalence_battery() {
    let root = temp_dir("equiv");
    let mut schema = Schema::new();
    schema.add(FieldSpec::keyword("level"));
    schema.add(FieldSpec::keyword("tid"));
    schema.add(FieldSpec::text_with_positions("message"));
    schema.add(FieldSpec::long_point("ts").with_numeric_dv());

    let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();
    // Write 20 docs with varied data (no flush — all in memory)
    for i in 0..20u32 {
        let level = match i % 4 { 0 => "INFO", 1 => "WARN", 2 => "ERROR", _ => "DEBUG" };
        let msg = if i % 8 == 0 { "quick brown fox" } else { format!("w{}", i % 5) };
        let mut d = Document::new();
        d.add("level", FieldValue::Keyword(level.to_string()));
        d.add("tid", FieldValue::Keyword(format!("tid-{i}")));
        d.add("message", FieldValue::Text(msg));
        d.add("ts", FieldValue::Long(1000 + i as i64));
        w.add_document(d).unwrap();
    }
    // NO flush — everything in memory buffer

    // Term
    let r = w.search(&Query::term("level", "INFO"), None, 100).unwrap();
    assert_eq!(r.total, 5);

    // MatchAll
    let r = w.search(&Query::MatchAll, None, 100).unwrap();
    assert_eq!(r.total, 20);

    // And
    let r = w.search(&Query::and("message", &["quick", "brown"]), None, 100).unwrap();
    assert_eq!(r.total, 3); // docs 0, 8, 16

    // Or
    let r = w.search(&Query::or("message", &["w0", "w1"]), None, 100).unwrap();
    assert_eq!(r.total, 11); // w0: 0,5,8,10,15,16 + w1: 1,6,11 + overlap at 0,8,16 → 9 unique... verify

    // Phrase
    let r = w.search(&Query::phrase("message", &["quick", "brown"]), None, 100).unwrap();
    assert_eq!(r.total, 3);

    // Prefix
    let r = w.search(&Query::prefix("message", "w"), None, 100).unwrap();
    assert_eq!(r.total, 17); // all docs with w0-w4 (not quick brown fox docs)

    // Wildcard
    let r = w.search(&Query::wildcard("message", "w?"), None, 100).unwrap();
    assert_eq!(r.total, 17);

    // PointRange
    let r = w.search(&Query::point_range("ts", 1005, 1010), None, 100).unwrap();
    assert_eq!(r.total, 6);

    // Bool
    let r = w.search(&Query::bool(vec![
        (Occur::Must, Query::term("level", "INFO")),
        (Occur::MustNot, Query::term("message", "w0")),
    ]), None, 100).unwrap();
    // INFO docs: 0,4,8,12,16; w0 docs: 0,5,8,10,15,16 → INFO-w0 = {4,12}
    assert_eq!(r.total, 2);

    // Sort desc by ts
    let r = w.search(&Query::term("level", "INFO"), Some(("ts", true)), 3).unwrap();
    assert_eq!(r.total, 5);
    assert_eq!(r.docs, vec![16, 12, 8]); // ts 1016, 1012, 1008

    fs::remove_dir_all(&root).unwrap();
}
```

- [ ] **Step 2: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All tests PASS

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No new warnings from our code

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/index_writer.rs
git commit -m "test(search): equivalence battery for unified LeafAccess memory search"
```
