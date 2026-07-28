# RwLock Real-Time Search Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable concurrent real-time search on IndexWriter's unflushed DocWriter buffer plus committed disk segments, exposed via JNI with JSON query parsing and time-field sorting.

**Architecture:** Wrap IndexWriter in `Arc<RwLock<IndexWriter>>`. Write operations take write_lock (~10μs), search operations take read_lock (~1ms) and query both the in-memory DocWriter (via MemoryLeafReader) and disk segments (via SegmentReader). Stored fields are read from .fdt files without holding the lock. Sorting uses NumericDocValues with a heap-based top-N collector.

**Tech Stack:** Rust std::sync::RwLock, serde_json for query parsing, existing codec-lucene9 DocValuesReader/StoredFieldsWriter, jni 0.21 crate.

## Global Constraints

- `#![forbid(unsafe_code)]` in crates/core/src/lib.rs — no unsafe in core crate
- JNI binding crate already uses unsafe (inherent to JNI) — that's fine
- Existing 99 tests must remain green (no regression)
- Single writer thread (Java), multiple reader threads (Java)
- Sort only by Numeric long field (time), single field, asc/desc
- Missing DV value sorts last (i64::MIN for desc)

---

## File Structure

```
crates/core/src/
├── index_writer.rs          ← Modify: add search(), document_location(), &self accessors
├── doc_writer.rs            ← Modify: add numeric_dv() read method
├── memory_reader.rs         ← Modify: remove stored dependency from MemoryLeafReader
├── search/
│   ├── sorted_collector.rs  ← Create: SortedTopN heap collector
│   ├── segment_reader.rs    ← Modify: add DocValuesReader field + numeric_dv()
│   └── mod.rs               ← Modify: pub mod sorted_collector
├── lib.rs                   ← Modify: (no new module needed, sorted_collector under search/)

crates/codec-lucene9/src/
├── stored_fields.rs         ← Modify: expose flushed_doc_count(), chunk_file_pointer(), buffered_doc_bytes()

crates/jni-binding/src/
├── lib.rs                   ← Modify: Mutex → RwLock, add nativeSearch/nativeDocument
├── query_parser.rs          ← Create: JSON → Query deserialization
```

---

### Task 1: DocWriter numeric_dv() read accessor

**Files:**
- Modify: `crates/core/src/doc_writer.rs`
- Test: inline `#[cfg(test)]` in doc_writer.rs

**Interfaces:**
- Produces: `DocWriter::numeric_dv(&self, field: &str, doc: u32) -> Option<i64>` — used by Task 4 (IndexWriter::search) to read sort keys from the in-memory buffer.

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` in `crates/core/src/doc_writer.rs`:

```rust
#[test]
fn numeric_dv_read() {
    use crate::schema::{FieldSpec, Schema};
    use crate::document::{Document, FieldValue};

    let mut schema = Schema::new();
    schema.add(FieldSpec::long_point("ts").with_numeric_dv());
    schema.add(FieldSpec::keyword("level"));

    let mut dw = DocWriter::new();
    for i in 0..5u32 {
        let mut doc = Document::new();
        doc.add("ts", FieldValue::Long(1000 + i as i64));
        doc.add("level", FieldValue::Keyword("INFO".into()));
        dw.add_document(&schema, doc, None).unwrap();
    }

    // Existing docs return their value
    assert_eq!(dw.numeric_dv("ts", 0), Some(1000));
    assert_eq!(dw.numeric_dv("ts", 4), Some(1004));
    // Out-of-range doc
    assert_eq!(dw.numeric_dv("ts", 5), None);
    // Field without DV
    assert_eq!(dw.numeric_dv("level", 0), None);
    // Unknown field
    assert_eq!(dw.numeric_dv("nope", 0), None);
}

#[test]
fn numeric_dv_sparse() {
    use crate::schema::{FieldSpec, Schema};
    use crate::document::{Document, FieldValue};

    let mut schema = Schema::new();
    schema.add(FieldSpec::long_point("ts").with_numeric_dv());
    schema.add(FieldSpec::keyword("level"));

    let mut dw = DocWriter::new();
    // doc 0: has ts
    let mut doc = Document::new();
    doc.add("ts", FieldValue::Long(42));
    doc.add("level", FieldValue::Keyword("A".into()));
    dw.add_document(&schema, doc, None).unwrap();
    // doc 1: no ts
    let mut doc = Document::new();
    doc.add("level", FieldValue::Keyword("B".into()));
    dw.add_document(&schema, doc, None).unwrap();
    // doc 2: has ts
    let mut doc = Document::new();
    doc.add("ts", FieldValue::Long(99));
    doc.add("level", FieldValue::Keyword("C".into()));
    dw.add_document(&schema, doc, None).unwrap();

    assert_eq!(dw.numeric_dv("ts", 0), Some(42));
    assert_eq!(dw.numeric_dv("ts", 1), None); // sparse
    assert_eq!(dw.numeric_dv("ts", 2), Some(99));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rustlucene-core numeric_dv_read -- --nocapture`
Expected: FAIL — `numeric_dv` method not found on DocWriter.

- [ ] **Step 3: Write minimal implementation**

Add to `impl DocWriter` in `crates/core/src/doc_writer.rs` (after the existing `pub fn ram_bytes` method, around line 342):

```rust
/// Read a numeric doc value for a specific doc. Returns None if the field
/// has no numeric DV buffer, or the doc has no value (sparse).
pub fn numeric_dv(&self, field: &str, doc: u32) -> Option<i64> {
    let number = self.fields.iter().position(|f| f.name == field)?;
    let buf = self.buffers.get(number)?.as_ref()?;
    let dv = buf.numeric_dv.as_ref()?;
    let idx = dv.docs.binary_search(&doc).ok()?;
    Some(dv.values[idx])
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rustlucene-core numeric_dv -- --nocapture`
Expected: PASS (both numeric_dv_read and numeric_dv_sparse)

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/doc_writer.rs
git commit -m "feat(search): DocWriter::numeric_dv() read accessor for in-memory sort"
```

---

### Task 2: SortedTopN heap collector

**Files:**
- Create: `crates/core/src/search/sorted_collector.rs`
- Modify: `crates/core/src/search/mod.rs`
- Test: inline `#[cfg(test)]` in sorted_collector.rs

**Interfaces:**
- Produces: `SortedTopN::new(desc: bool, n: usize) -> Self`, `SortedTopN::collect(&mut self, doc: i32, sort_value: i64)`, `SortedTopN::results(self) -> (u64, Vec<i32>)` — used by Task 4 (IndexWriter::search).
- Produces: `pub struct SearchResults { pub total: u64, pub docs: Vec<i32> }` — used by Task 6 (JNI layer).

- [ ] **Step 1: Write the failing test**

Create `crates/core/src/search/sorted_collector.rs`:

```rust
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Heap-based top-N collector sorted by a numeric sort value.
/// For desc order, keeps the N largest values; for asc, the N smallest.
/// Missing values (i64::MIN) sort last in desc order.
pub struct SortedTopN {
    desc: bool,
    n: usize,
    // For desc: min-heap of (value, doc) — evict smallest when full.
    // For asc: max-heap of (Reverse(value), doc) — evict largest when full.
    heap_desc: BinaryHeap<(Reverse<i64>, i32)>,
    heap_asc: BinaryHeap<(i64, i32)>,
    total: u64,
}

pub struct SearchResults {
    pub total: u64,
    pub docs: Vec<i32>,
}

impl SortedTopN {
    pub fn new(desc: bool, n: usize) -> Self {
        Self {
            desc,
            n,
            heap_desc: BinaryHeap::new(),
            heap_asc: BinaryHeap::new(),
            total: 0,
        }
    }

    pub fn collect(&mut self, doc: i32, sort_value: i64) {
        self.total += 1;
        if self.n == 0 {
            return;
        }
        if self.desc {
            if self.heap_desc.len() < self.n {
                self.heap_desc.push((Reverse(sort_value), doc));
            } else if let Some(&(Reverse(min_val), _)) = self.heap_desc.peek() {
                if sort_value > min_val {
                    self.heap_desc.pop();
                    self.heap_desc.push((Reverse(sort_value), doc));
                }
            }
        } else {
            if self.heap_asc.len() < self.n {
                self.heap_asc.push((sort_value, doc));
            } else if let Some(&(max_val, _)) = self.heap_asc.peek() {
                if sort_value < max_val {
                    self.heap_asc.pop();
                    self.heap_asc.push((sort_value, doc));
                }
            }
        }
    }

    pub fn results(self) -> SearchResults {
        let mut docs: Vec<(i64, i32)> = if self.desc {
            self.heap_desc
                .into_iter()
                .map(|(Reverse(v), d)| (v, d))
                .collect()
        } else {
            self.heap_asc.into_iter().collect()
        };
        // Sort: desc → largest first; asc → smallest first
        if self.desc {
            docs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        } else {
            docs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        }
        SearchResults {
            total: self.total,
            docs: docs.into_iter().map(|(_, d)| d).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desc_top3() {
        let mut c = SortedTopN::new(true, 3);
        // values: doc0=10, doc1=50, doc2=30, doc3=90, doc4=20
        c.collect(0, 10);
        c.collect(1, 50);
        c.collect(2, 30);
        c.collect(3, 90);
        c.collect(4, 20);
        let r = c.results();
        assert_eq!(r.total, 5);
        assert_eq!(r.docs, vec![3, 1, 2]); // 90, 50, 30
    }

    #[test]
    fn asc_top3() {
        let mut c = SortedTopN::new(false, 3);
        c.collect(0, 10);
        c.collect(1, 50);
        c.collect(2, 30);
        c.collect(3, 90);
        c.collect(4, 20);
        let r = c.results();
        assert_eq!(r.total, 5);
        assert_eq!(r.docs, vec![0, 4, 2]); // 10, 20, 30
    }

    #[test]
    fn missing_sorts_last_desc() {
        let mut c = SortedTopN::new(true, 3);
        c.collect(0, i64::MIN); // missing
        c.collect(1, 50);
        c.collect(2, 30);
        c.collect(3, 90);
        let r = c.results();
        assert_eq!(r.total, 4);
        assert_eq!(r.docs, vec![3, 1, 2]); // 90, 50, 30 — missing excluded from top3
    }

    #[test]
    fn n_zero_counts_only() {
        let mut c = SortedTopN::new(true, 0);
        c.collect(0, 10);
        c.collect(1, 20);
        let r = c.results();
        assert_eq!(r.total, 2);
        assert!(r.docs.is_empty());
    }

    #[test]
    fn n_larger_than_hits() {
        let mut c = SortedTopN::new(true, 100);
        c.collect(0, 10);
        c.collect(1, 20);
        let r = c.results();
        assert_eq!(r.total, 2);
        assert_eq!(r.docs, vec![1, 0]); // 20, 10
    }

    #[test]
    fn tie_breaking_by_doc_id_asc() {
        let mut c = SortedTopN::new(true, 3);
        c.collect(5, 50);
        c.collect(2, 50);
        c.collect(8, 50);
        let r = c.results();
        assert_eq!(r.docs, vec![2, 5, 8]); // same value → doc id asc
    }
}
```

- [ ] **Step 2: Register the module**

Add to `crates/core/src/search/mod.rs` after the existing `pub mod segment_reader;` line:

```rust
pub mod sorted_collector;
```

And add to the `pub use` block:

```rust
pub use sorted_collector::{SearchResults, SortedTopN};
```

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test -p rustlucene-core sorted_collector -- --nocapture`
Expected: PASS (all 6 tests)

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/search/sorted_collector.rs crates/core/src/search/mod.rs
git commit -m "feat(search): SortedTopN heap collector for numeric field sorting"
```

---

### Task 3: SegmentReader DocValues integration

**Files:**
- Modify: `crates/core/src/search/segment_reader.rs`
- Test: inline in `crates/core/src/search/mod.rs` tests

**Interfaces:**
- Consumes: `codec_lucene9::doc_values_read::DocValuesReader` (already exists)
- Produces: `SegmentReader::numeric_dv(&self, field: &str, doc: u32) -> Option<i64>` — used by Task 4 for disk-segment sort keys.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `crates/core/src/search/mod.rs`:

```rust
#[test]
fn segment_reader_numeric_dv() {
    let root = temp_dir("segdv");
    let mut schema = Schema::new();
    schema.add(FieldSpec::keyword("level"));
    schema.add(FieldSpec::long_point("ts").with_numeric_dv());
    let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();
    for i in 0..10u32 {
        let mut d = Document::new();
        d.add("level", FieldValue::Keyword("INFO".to_string()));
        d.add("ts", FieldValue::Long(1000 + i as i64));
        w.add_document(d).unwrap();
    }
    w.commit().unwrap();
    drop(w);

    let dir = FSDirectory::open(&root).unwrap();
    let mut reader = Reader::open(&dir).unwrap();
    let (_base, seg) = reader.leaves().next().unwrap();
    assert_eq!(seg.numeric_dv("ts", 0), Some(1000));
    assert_eq!(seg.numeric_dv("ts", 9), Some(1009));
    assert_eq!(seg.numeric_dv("ts", 10), None); // out of range
    assert_eq!(seg.numeric_dv("level", 0), None); // no DV
    assert_eq!(seg.numeric_dv("nope", 0), None); // unknown
    drop(reader);
    fs::remove_dir_all(&root).unwrap();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rustlucene-core segment_reader_numeric_dv -- --nocapture`
Expected: FAIL — `numeric_dv` method not found on SegmentReader.

- [ ] **Step 3: Write minimal implementation**

Modify `crates/core/src/search/segment_reader.rs`:

Add import at top:
```rust
use codec_lucene9::doc_values_read::DocValuesReader;
```

Add field to struct:
```rust
pub struct SegmentReader {
    max_doc: i32,
    field_infos: FieldInfos,
    terms: TermsDict,
    postings: PostingsReader,
    points: Option<PointsReader>,
    doc_values: Option<DocValuesReader>,
}
```

In `SegmentReader::open`, after the `points` line, add:
```rust
let dv_suffix = "Lucene90_0";
let doc_values = DocValuesReader::open(dir, segment, segment_id, dv_suffix).ok();
```

And include `doc_values` in the struct literal.

Add method:
```rust
/// Read a numeric doc value by field name and local doc id.
/// Returns None if the field has no DV, the segment has no DV files,
/// or the doc has no value (sparse).
pub fn numeric_dv(&self, field: &str, doc: u32) -> Option<i64> {
    let fi = self.field_infos.by_name(field)?;
    let dv = self.doc_values.as_ref()?;
    let pairs = dv.numeric_values(fi.number).ok()?;
    // pairs is Vec<(u32, i64)> sorted by doc — binary search
    pairs
        .binary_search_by(|&(d, _)| d.cmp(&doc))
        .ok()
        .map(|idx| pairs[idx].1)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rustlucene-core segment_reader_numeric_dv -- --nocapture`
Expected: PASS

- [ ] **Step 5: Run full test suite for regression**

Run: `cargo test -p rustlucene-core`
Expected: All existing tests PASS (the new `doc_values` field is Option, so segments without DV files still work).

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/search/segment_reader.rs crates/core/src/search/mod.rs
git commit -m "feat(search): SegmentReader numeric_dv() via DocValuesReader integration"
```

---

### Task 4: IndexWriter unified search

**Files:**
- Modify: `crates/core/src/index_writer.rs`
- Test: inline `#[cfg(test)]` in index_writer.rs

**Interfaces:**
- Consumes: `DocWriter::numeric_dv()` (Task 1), `SortedTopN` (Task 2), `SegmentReader::numeric_dv()` (Task 3), `MemoryLeafReader` (existing)
- Produces: `IndexWriter::search(&self, query: &Query, sort_field: Option<(&str, bool)>, top_n: usize) -> io::Result<SearchResults>` — used by Task 6 (JNI).
- Produces: `IndexWriter::schema(&self) -> &Schema` — needed by MemoryLeafReader construction.

- [ ] **Step 1: Write the failing test**

Add `#[cfg(test)] mod tests` to `crates/core/src/index_writer.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, FieldValue};
    use crate::schema::FieldSpec;
    use crate::search::query::Query;
    use std::fs;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rustlucene-rt-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn rt_schema() -> Schema {
        let mut s = Schema::new();
        s.add(FieldSpec::keyword("level"));
        s.add(FieldSpec::text("message"));
        s.add(FieldSpec::long_point("ts").with_numeric_dv());
        s
    }

    fn rt_doc(level: &str, message: &str, ts: i64) -> Document {
        let mut d = Document::new();
        d.add("level", FieldValue::Keyword(level.to_string()));
        d.add("message", FieldValue::Text(message.to_string()));
        d.add("ts", FieldValue::Long(ts));
        d
    }

    #[test]
    fn search_memory_only() {
        let root = temp_dir("memonly");
        let mut w = IndexWriter::create(&root, rt_schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..10u32 {
            w.add_document(rt_doc("INFO", "hello world", 1000 + i as i64))
                .unwrap();
        }
        // No flush — all in memory
        let r = w.search(&Query::term("level", "INFO"), Some(("ts", true)), 5).unwrap();
        assert_eq!(r.total, 10);
        // desc by ts: docs 9,8,7,6,5 (ts 1009..1005)
        assert_eq!(r.docs, vec![9, 8, 7, 6, 5]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_across_flush_boundary() {
        let root = temp_dir("flushbound");
        let mut w = IndexWriter::create(&root, rt_schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..10u32 {
            w.add_document(rt_doc("INFO", "hello", 1000 + i as i64))
                .unwrap();
        }
        w.flush().unwrap();
        for i in 10..15u32 {
            w.add_document(rt_doc("INFO", "hello", 1000 + i as i64))
                .unwrap();
        }
        // 10 on disk + 5 in memory = 15 total
        let r = w.search(&Query::term("level", "INFO"), Some(("ts", true)), 5).unwrap();
        assert_eq!(r.total, 15);
        assert_eq!(r.docs, vec![14, 13, 12, 11, 10]); // top 5 by ts desc
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_no_sort_index_order() {
        let root = temp_dir("nosort");
        let mut w = IndexWriter::create(&root, rt_schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..5u32 {
            w.add_document(rt_doc("INFO", "hello", i as i64)).unwrap();
        }
        let r = w.search(&Query::term("level", "INFO"), None, 3).unwrap();
        assert_eq!(r.total, 5);
        assert_eq!(r.docs, vec![0, 1, 2]); // INDEXORDER
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_asc_sort() {
        let root = temp_dir("ascsort");
        let mut w = IndexWriter::create(&root, rt_schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..10u32 {
            w.add_document(rt_doc("INFO", "hello", 1000 + i as i64))
                .unwrap();
        }
        let r = w.search(&Query::term("level", "INFO"), Some(("ts", false)), 3).unwrap();
        assert_eq!(r.total, 10);
        assert_eq!(r.docs, vec![0, 1, 2]); // asc: ts 1000, 1001, 1002
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_empty_index() {
        let root = temp_dir("emptyrt");
        let w = IndexWriter::create(&root, rt_schema(), IndexWriterConfig::default()).unwrap();
        let r = w.search(&Query::MatchAll, Some(("ts", true)), 10).unwrap();
        assert_eq!(r.total, 0);
        assert!(r.docs.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rustlucene-core index_writer::tests -- --nocapture`
Expected: FAIL — `search` method not found on IndexWriter.

- [ ] **Step 3: Write minimal implementation**

Add imports to `crates/core/src/index_writer.rs`:

```rust
use crate::memory_reader::MemoryLeafReader;
use crate::search::query::Query;
use crate::search::sorted_collector::{SearchResults, SortedTopN};
use crate::search::segment_reader::SegmentReader;
```

Add methods to `impl IndexWriter`:

```rust
/// Schema accessor (needed by search to construct MemoryLeafReader).
pub fn schema(&self) -> &Schema {
    &self.schema
}

/// Unified search across in-memory buffer + flushed segments.
/// `sort_field`: Some((field_name, desc)) for sorted results, None for INDEXORDER.
pub fn search(
    &self,
    query: &Query,
    sort_field: Option<(&str, bool)>,
    top_n: usize,
) -> io::Result<SearchResults> {
    let (desc, n) = match sort_field {
        Some((_, desc)) => (desc, top_n),
        None => (false, top_n),
    };
    let mut collector = SortedTopN::new(desc, n);
    let mut index_order_docs: Vec<i32> = Vec::new();
    let mut total: u64 = 0;
    let use_sort = sort_field.is_some();

    // 1. Flushed segments (on disk)
    let mut doc_base: i32 = 0;
    for sci in &self.infos.segments {
        let mut reader = SegmentReader::open(&self.dir, sci)?;
        let local_docs = Self::exec_query_on_segment(&mut reader, query)?;
        for local_id in &local_docs {
            let global_id = doc_base + *local_id;
            if use_sort {
                let (field, _) = sort_field.unwrap();
                let sv = reader.numeric_dv(field, *local_id as u32).unwrap_or(i64::MIN);
                collector.collect(global_id, sv);
            } else {
                total += 1;
                if index_order_docs.len() < n {
                    index_order_docs.push(global_id);
                }
            }
        }
        if use_sort {
            // total counted inside collector
        }
        doc_base += sci.info.doc_count;
    }

    // 2. In-memory buffer (unflushed)
    if let Some(builder) = &self.builder {
        let mem = MemoryLeafReader::new(builder.doc_writer(), &self.schema);
        let local_docs = mem.exec_query(query)?;
        for local_id in &local_docs {
            let global_id = doc_base + *local_id as i32;
            if use_sort {
                let (field, _) = sort_field.unwrap();
                let sv = builder.doc_writer().numeric_dv(field, *local_id).unwrap_or(i64::MIN);
                collector.collect(global_id, sv);
            } else {
                total += 1;
                if index_order_docs.len() < n {
                    index_order_docs.push(global_id);
                }
            }
        }
    }

    if use_sort {
        Ok(collector.results())
    } else {
        Ok(SearchResults {
            total,
            docs: index_order_docs,
        })
    }
}

/// Execute a query on a single segment, returning local doc IDs.
fn exec_query_on_segment(
    reader: &mut SegmentReader,
    query: &Query,
) -> io::Result<Vec<i32>> {
    use codec_lucene9::postings_read::NO_MORE_DOCS;
    let mut docs = Vec::new();
    if let Some(mut iter) = query.segment_iterator(reader, false)? {
        loop {
            let doc = iter.next_doc()?;
            if doc == NO_MORE_DOCS {
                break;
            }
            if !iter.matches()? {
                continue;
            }
            docs.push(doc);
        }
    }
    Ok(docs)
}
```

Also add a `doc_writer()` accessor to `SegmentBuilder` in `crates/core/src/segment_builder.rs`:

```rust
/// Read-only access to the internal DocWriter (for real-time search).
pub fn doc_writer(&self) -> &DocWriter {
    &self.dw
}
```

- [ ] **Step 4: Make MemoryLeafReader::exec_query public**

In `crates/core/src/memory_reader.rs`, the `exec_query` method on `MemorySearcher` is private. We need `MemoryLeafReader` to expose query execution. Add a public method:

```rust
impl<'a> MemoryLeafReader<'a> {
    /// Execute a query returning sorted matching local doc IDs.
    pub fn exec_query(&self, query: &Query) -> io::Result<Vec<u32>> {
        // Delegate to the same logic as MemorySearcher::exec_query
        let searcher = MemorySearcher { reader: MemoryLeafReader { dw: self.dw, schema: self.schema, stored: &[] } };
        searcher.exec_query_inner(query)
    }
}
```

Alternatively, refactor `MemorySearcher::exec_query` into a free function or make it accessible. The simplest approach: make `MemorySearcher::exec_query` `pub(crate)` and construct a temporary MemorySearcher with an empty stored slice (stored is not needed for query execution, only for document retrieval).

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p rustlucene-core index_writer::tests -- --nocapture`
Expected: PASS (all 5 tests)

- [ ] **Step 6: Run full test suite**

Run: `cargo test -p rustlucene-core`
Expected: All tests PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/index_writer.rs crates/core/src/segment_builder.rs crates/core/src/memory_reader.rs
git commit -m "feat(search): IndexWriter::search() unified memory+disk with sort"
```

---

### Task 5: StoredFieldsWriter read accessors + document_location

**Files:**
- Modify: `crates/codec-lucene9/src/stored_fields.rs`
- Modify: `crates/core/src/index_writer.rs`
- Test: inline tests

**Interfaces:**
- Produces: `StoredFieldsWriter::flushed_doc_count() -> i32`, `StoredFieldsWriter::chunk_file_pointer(doc_id: u32) -> Option<u64>`, `StoredFieldsWriter::buffered_doc_bytes(n: u32) -> Option<&[u8]>` — used by Task 6 (JNI nativeDocument).
- Produces: `IndexWriter::document_location(&self, global_id: u32) -> DocLocation` — used by Task 6.

- [ ] **Step 1: Write the failing test for StoredFieldsWriter accessors**

Add to `crates/codec-lucene9/src/stored_fields.rs` tests:

```rust
#[test]
fn sfw_read_accessors() {
    use crate::directory::FSDirectory;
    let dir = FSDirectory::open(&std::env::temp_dir().join(format!("sfw-acc-{}", std::process::id()))).unwrap();
    let seg_id = [0u8; 16];
    let mut sfw = StoredFieldsWriter::new(&dir, "_0", seg_id, "").unwrap();

    // Write 3 docs
    for i in 0..3 {
        sfw.start_document();
        sfw.write_field(0, &StoredField::String(format!("val-{i}"))).unwrap();
        sfw.finish_document().unwrap();
    }
    assert_eq!(sfw.flushed_doc_count(), 0); // not flushed yet (chunk not full)
    assert!(sfw.buffered_doc_bytes(0).is_some());
    assert!(sfw.buffered_doc_bytes(2).is_some());
    assert!(sfw.buffered_doc_bytes(3).is_none()); // out of range
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p codec-lucene9 sfw_read_accessors -- --nocapture`
Expected: FAIL — methods not found.

- [ ] **Step 3: Implement StoredFieldsWriter accessors**

Add to `impl StoredFieldsWriter` in `crates/codec-lucene9/src/stored_fields.rs`:

```rust
/// Number of docs already flushed to disk (in completed chunks).
pub fn flushed_doc_count(&self) -> i32 {
    self.num_docs - self.buffered_docs.len() as i32
}

/// File pointer for the chunk containing doc_id (for disk read).
/// Returns None if doc_id is in the unflushed buffer.
pub fn chunk_file_pointer(&self, doc_id: u32) -> Option<u64> {
    // Binary search chunk_doc_bases for the containing chunk
    let idx = self.chunk_doc_bases.partition_point(|&base| base as u32 <= doc_id);
    if idx == 0 {
        return None;
    }
    Some(self.chunk_file_pointers[idx - 1])
}

/// Raw bytes of the n-th buffered (unflushed) document.
/// Returns None if n >= number of buffered docs.
pub fn buffered_doc_bytes(&self, n: u32) -> Option<&[u8]> {
    self.buffered_docs.get(n as usize).map(|d| d.as_slice())
}
```

Note: The exact field names (`num_docs`, `buffered_docs`, `chunk_doc_bases`, `chunk_file_pointers`) must match the actual internal fields of `StoredFieldsWriter`. Inspect the struct definition and adapt. If chunk tracking fields don't exist yet, add them:

```rust
// In StoredFieldsWriter struct, add:
chunk_doc_bases: Vec<i32>,       // docBase of each flushed chunk
chunk_file_pointers: Vec<u64>,   // file pointer of each flushed chunk
```

And populate them in the existing `flush()` method.

- [ ] **Step 4: Implement IndexWriter::document_location**

Add to `crates/core/src/index_writer.rs`:

```rust
/// Where a global doc_id's stored fields live.
pub enum DocLocation {
    /// In a committed segment's .fdt file.
    CommittedSegment {
        seg_name: String,
        seg_id: [u8; 16],
        local_id: u32,
    },
    /// In the current buffer's SFW (either flushed chunk or memory).
    Buffer {
        local_id: u32,
        flushed: bool,
    },
    /// No document at this id.
    NotFound,
}

impl IndexWriter {
    pub fn document_location(&self, global_id: u32) -> DocLocation {
        let mut base: u32 = 0;
        for sci in &self.infos.segments {
            let seg_docs = sci.info.doc_count as u32;
            if global_id < base + seg_docs {
                return DocLocation::CommittedSegment {
                    seg_name: sci.info.name.clone(),
                    seg_id: sci.info.id,
                    local_id: global_id - base,
                };
            }
            base += seg_docs;
        }
        // In-memory buffer
        if let Some(builder) = &self.builder {
            let local_id = global_id - base;
            if local_id < builder.buffered_docs() {
                let flushed = builder.sfw_flushed_doc_count() > local_id as i32;
                return DocLocation::Buffer { local_id, flushed };
            }
        }
        DocLocation::NotFound
    }
}
```

Add to `SegmentBuilder`:
```rust
pub fn sfw_flushed_doc_count(&self) -> i32 {
    self.sfw.as_ref().map_or(0, |s| s.flushed_doc_count())
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p codec-lucene9 sfw_read_accessors -- --nocapture`
Run: `cargo test -p rustlucene-core`
Expected: All PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/codec-lucene9/src/stored_fields.rs crates/core/src/index_writer.rs crates/core/src/segment_builder.rs
git commit -m "feat(search): StoredFieldsWriter read accessors + IndexWriter::document_location"
```

---

### Task 6: JNI query parser

**Files:**
- Create: `crates/jni-binding/src/query_parser.rs`
- Modify: `crates/jni-binding/Cargo.toml` (add serde, serde_json)
- Test: inline `#[cfg(test)]` in query_parser.rs

**Interfaces:**
- Consumes: `rustlucene_core::search::query::{Query, Occur}`
- Produces: `parse_search_request(json: &[u8]) -> Result<SearchRequest, String>` — used by Task 7 (JNI nativeSearch).

- [ ] **Step 1: Add serde dependencies**

Add to `crates/jni-binding/Cargo.toml` under `[dependencies]`:

```toml
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

- [ ] **Step 2: Write the failing test**

Create `crates/jni-binding/src/query_parser.rs`:

```rust
use serde::Deserialize;
use rustlucene_core::search::query::{Occur, Query};

#[derive(Deserialize)]
pub struct SearchRequest {
    pub query: QuerySpec,
    pub sort: Option<SortSpec>,
    #[serde(default = "default_top_n")]
    pub top_n: usize,
}

fn default_top_n() -> usize {
    10
}

#[derive(Deserialize)]
pub struct SortSpec {
    pub field: String,
    #[serde(default = "default_desc")]
    pub order: String,
}

fn default_desc() -> String {
    "desc".to_string()
}

#[derive(Deserialize)]
pub struct ClauseSpec {
    pub occur: String,
    pub query: QuerySpec,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuerySpec {
    Term { field: String, value: String },
    Bool { clauses: Vec<ClauseSpec> },
    Range { field: String, low: i64, high: i64 },
    Prefix { field: String, value: String },
    Wildcard { field: String, value: String },
    Phrase { field: String, terms: Vec<String> },
    MatchAll,
}

impl SearchRequest {
    pub fn to_query(&self) -> Result<Query, String> {
        spec_to_query(&self.query)
    }

    pub fn sort_field(&self) -> Option<(&str, bool)> {
        self.sort.as_ref().map(|s| {
            let desc = s.order != "asc";
            (s.field.as_str(), desc)
        })
    }
}

fn spec_to_query(spec: &QuerySpec) -> Result<Query, String> {
    match spec {
        QuerySpec::Term { field, value } => Ok(Query::term(field, value)),
        QuerySpec::MatchAll => Ok(Query::MatchAll),
        QuerySpec::Range { field, low, high } => Ok(Query::point_range(field, *low, *high)),
        QuerySpec::Prefix { field, value } => Ok(Query::prefix(field, value)),
        QuerySpec::Wildcard { field, value } => Ok(Query::wildcard(field, value)),
        QuerySpec::Phrase { field, terms } => {
            let refs: Vec<&str> = terms.iter().map(String::as_str).collect();
            Ok(Query::phrase(field, &refs))
        }
        QuerySpec::Bool { clauses } => {
            let mut out = Vec::with_capacity(clauses.len());
            for c in clauses {
                let occur = match c.occur.as_str() {
                    "must" => Occur::Must,
                    "should" => Occur::Should,
                    "must_not" => Occur::MustNot,
                    other => return Err(format!("unknown occur: {other}")),
                };
                out.push((occur, spec_to_query(&c.query)?));
            }
            Ok(Query::bool(out))
        }
    }
}

pub fn parse_search_request(json: &[u8]) -> Result<SearchRequest, String> {
    serde_json::from_slice(json).map_err(|e| format!("query parse error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_term_query() {
        let json = br#"{"query":{"type":"term","field":"level","value":"ERROR"},"top_n":50}"#;
        let req = parse_search_request(json).unwrap();
        assert_eq!(req.top_n, 50);
        assert!(req.sort.is_none());
        let q = req.to_query().unwrap();
        assert_eq!(q, Query::term("level", "ERROR"));
    }

    #[test]
    fn parse_bool_with_sort() {
        let json = br#"{
            "query":{"type":"bool","clauses":[
                {"occur":"must","query":{"type":"term","field":"level","value":"ERROR"}},
                {"occur":"must","query":{"type":"range","field":"ts","low":100,"high":200}},
                {"occur":"must_not","query":{"type":"term","field":"host","value":"test"}}
            ]},
            "sort":{"field":"ts","order":"desc"},
            "top_n":100
        }"#;
        let req = parse_search_request(json).unwrap();
        assert_eq!(req.top_n, 100);
        let (field, desc) = req.sort_field().unwrap();
        assert_eq!(field, "ts");
        assert!(desc);
        let q = req.to_query().unwrap();
        match q {
            Query::Bool { clauses } => assert_eq!(clauses.len(), 3),
            _ => panic!("expected Bool"),
        }
    }

    #[test]
    fn parse_match_all_default_top_n() {
        let json = br#"{"query":{"type":"match_all"}}"#;
        let req = parse_search_request(json).unwrap();
        assert_eq!(req.top_n, 10); // default
        assert_eq!(req.to_query().unwrap(), Query::MatchAll);
    }

    #[test]
    fn parse_phrase_prefix_wildcard() {
        let json = br#"{"query":{"type":"phrase","field":"msg","terms":["hello","world"]}}"#;
        let req = parse_search_request(json).unwrap();
        assert_eq!(req.to_query().unwrap(), Query::phrase("msg", &["hello", "world"]));

        let json = br#"{"query":{"type":"prefix","field":"path","value":"/api"}}"#;
        let req = parse_search_request(json).unwrap();
        assert_eq!(req.to_query().unwrap(), Query::prefix("path", "/api"));

        let json = br#"{"query":{"type":"wildcard","field":"tid","value":"req-*"}}"#;
        let req = parse_search_request(json).unwrap();
        assert_eq!(req.to_query().unwrap(), Query::wildcard("tid", "req-*"));
    }

    #[test]
    fn parse_invalid_json() {
        assert!(parse_search_request(b"not json").is_err());
        assert!(parse_search_request(br#"{"query":{"type":"unknown"}}"#).is_err());
    }

    #[test]
    fn parse_asc_sort() {
        let json = br#"{"query":{"type":"match_all"},"sort":{"field":"ts","order":"asc"}}"#;
        let req = parse_search_request(json).unwrap();
        let (_, desc) = req.sort_field().unwrap();
        assert!(!desc);
    }
}
```

- [ ] **Step 3: Register module in lib.rs**

Add to `crates/jni-binding/src/lib.rs` at the top:

```rust
mod query_parser;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p rustlucene-jni query_parser -- --nocapture`
Expected: PASS (all 6 tests)

- [ ] **Step 5: Commit**

```bash
git add crates/jni-binding/src/query_parser.rs crates/jni-binding/src/lib.rs crates/jni-binding/Cargo.toml
git commit -m "feat(jni): JSON query parser for real-time search"
```

---

### Task 7: JNI layer — RwLock + nativeSearch + nativeDocument

**Files:**
- Modify: `crates/jni-binding/src/lib.rs`

**Interfaces:**
- Consumes: `IndexWriter::search()` (Task 4), `IndexWriter::document_location()` (Task 5), `parse_search_request()` (Task 6)
- Produces: `Java_RustIndexWriter_nativeSearch(handle, byte[]) → byte[]`, `Java_RustIndexWriter_nativeDocument(handle, int) → byte[]`

- [ ] **Step 1: Refactor WriterHandle from Mutex to RwLock**

Replace in `crates/jni-binding/src/lib.rs`:

```rust
use std::sync::RwLock;

struct WriterHandle {
    index: RwLock<IndexWriter>,
    current: Option<Document>,
    binder: JsonBinder,
}
```

Update `handle()` helper:
```rust
fn handle<'a>(ptr: jlong) -> Result<&'a WriterHandle, String> {
    if ptr == 0 {
        return Err("null writer handle".into());
    }
    Ok(unsafe { &*(ptr as *const WriterHandle) })
}
```

Update all existing JNI functions to use `h.index.write().unwrap()` for write ops and `h.index.read().unwrap()` for read ops. For example, `nativeEndDocument`:

```rust
pub extern "system" fn Java_RustIndexWriter_nativeEndDocument(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) -> jlong {
    let h = jni_try!(&mut env, handle(ptr));
    let doc = match h.current.take() {
        Some(d) => d,
        None => {
            throw(&mut env, "java/lang/IllegalStateException", "beginDocument not called");
            return 0;
        }
    };
    let mut guard = h.index.write().unwrap();
    jni_try!(&mut env, guard.add_document(doc).map_err(|e| e.to_string()));
    0
}
```

Note: `current` is only accessed by the single writer thread (Java guarantees single-threaded writes), so it doesn't need to be inside the RwLock. But since `WriterHandle` is behind a raw pointer shared across threads, we need `current` to be safe. Use a `Mutex<Option<Document>>` for `current`, or keep the entire `WriterHandle` behind a `Mutex` for write-side state and a separate `Arc<RwLock<IndexWriter>>` for the index. The cleanest approach:

```rust
struct WriterHandle {
    index: RwLock<IndexWriter>,
    write_state: Mutex<WriteState>,
}

struct WriteState {
    current: Option<Document>,
    binder: JsonBinder,
}
```

- [ ] **Step 2: Add nativeSearch**

```rust
#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeSearch(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
    query_bytes: JByteArray,
) -> JByteArray {
    let h = jni_try!(&mut env, handle(ptr));
    let json = jni_try!(&mut env, env.convert_byte_array(&query_bytes).map_err(|e| e.to_string()));
    let req = jni_try!(&mut env, query_parser::parse_search_request(&json));
    let query = jni_try!(&mut env, req.to_query());
    let sort_field = req.sort_field();

    let guard = h.index.read().unwrap();
    let results = jni_try!(&mut env, guard.search(&query, sort_field, req.top_n).map_err(|e| e.to_string()));
    drop(guard);

    // Serialize: {"total":N,"docs":[id,...]}
    let json_out = serde_json::json!({
        "total": results.total,
        "docs": results.docs,
    });
    let bytes = json_out.to_string().into_bytes();
    jni_try!(&mut env, env.byte_array_from_slice(&bytes).map_err(|e| e.to_string()))
}
```

- [ ] **Step 3: Add nativeDocument**

```rust
#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeDocument(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
    doc_id: jint,
) -> JByteArray {
    let h = jni_try!(&mut env, handle(ptr));

    // Phase 1: lock to get location (~100ns)
    let guard = h.index.read().unwrap();
    let loc = guard.document_location(doc_id as u32);
    // Also grab dir path for disk reads
    let dir_path = guard.dir_path().to_path_buf();
    drop(guard);

    // Phase 2: read stored fields without lock
    let fields = jni_try!(&mut env, read_stored_fields(&dir_path, &loc).map_err(|e| e.to_string()));

    // Serialize fields as JSON object
    let json_out = serde_json::to_string(&fields).unwrap_or_default();
    let bytes = json_out.into_bytes();
    jni_try!(&mut env, env.byte_array_from_slice(&bytes).map_err(|e| e.to_string()))
}
```

The `read_stored_fields` function reads from .fdt (committed segment) or from the SFW buffer (in-memory). For committed segments, use the existing `StoredFieldsReader` from the worktree or implement a minimal chunk reader. For the buffer case, decode from `buffered_doc_bytes`.

- [ ] **Step 4: Add IndexWriter::dir_path accessor**

In `crates/core/src/index_writer.rs`:
```rust
pub fn dir_path(&self) -> &std::path::Path {
    self.dir.path()
}
```

(Verify `FSDirectory` has a `path()` method; if not, add one.)

- [ ] **Step 5: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/jni-binding/src/lib.rs crates/core/src/index_writer.rs
git commit -m "feat(jni): RwLock + nativeSearch + nativeDocument for real-time search"
```

---

### Task 8: Concurrent integration test

**Files:**
- Modify: `crates/core/src/index_writer.rs` (test section)

**Interfaces:**
- Consumes: All previous tasks.
- Validates: 1 writer + 4 readers under RwLock, no panic/deadlock, correct results across flush.

- [ ] **Step 1: Write the concurrent test**

Add to `crates/core/src/index_writer.rs` tests:

```rust
#[test]
fn concurrent_read_write_no_panic() {
    use std::sync::{Arc, RwLock};
    use std::thread;

    let root = temp_dir("concurrent");
    let w = IndexWriter::create(&root, rt_schema(), IndexWriterConfig::default()).unwrap();
    let index = Arc::new(RwLock::new(w));

    // Writer thread: 1000 docs
    let writer = {
        let idx = Arc::clone(&index);
        thread::spawn(move || {
            for i in 0..1000u32 {
                let mut guard = idx.write().unwrap();
                guard
                    .add_document(rt_doc("INFO", "hello", i as i64))
                    .unwrap();
            }
        })
    };

    // 4 reader threads: each does 50 searches
    let readers: Vec<_> = (0..4)
        .map(|tid| {
            let idx = Arc::clone(&index);
            thread::spawn(move || {
                for _ in 0..50 {
                    let guard = idx.read().unwrap();
                    let r = guard
                        .search(&Query::term("level", "INFO"), Some(("ts", true)), 10)
                        .unwrap();
                    // total should be monotonically non-decreasing
                    assert!(r.total <= 1000);
                    assert!(r.docs.len() <= 10);
                    // docs should be sorted desc by ts
                    for w in r.docs.windows(2) {
                        assert!(w[0] >= w[1], "reader {tid}: docs not desc");
                    }
                }
            })
        })
        .collect();

    writer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }

    // Final state: all 1000 docs searchable
    let guard = index.read().unwrap();
    let r = guard
        .search(&Query::MatchAll, Some(("ts", true)), 5)
        .unwrap();
    assert_eq!(r.total, 1000);
    assert_eq!(r.docs, vec![999, 998, 997, 996, 995]);
    drop(guard);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn concurrent_with_flush() {
    use std::sync::{Arc, RwLock};
    use std::thread;

    let root = temp_dir("concflush");
    let w = IndexWriter::create(&root, rt_schema(), IndexWriterConfig::default()).unwrap();
    let index = Arc::new(RwLock::new(w));

    // Writer: 500 docs, flush at 250
    let writer = {
        let idx = Arc::clone(&index);
        thread::spawn(move || {
            for i in 0..500u32 {
                let mut guard = idx.write().unwrap();
                guard
                    .add_document(rt_doc("INFO", "hello", i as i64))
                    .unwrap();
                if i == 249 {
                    guard.flush().unwrap();
                }
            }
        })
    };

    // Reader: continuous search
    let reader = {
        let idx = Arc::clone(&index);
        thread::spawn(move || {
            let mut last_total = 0u64;
            for _ in 0..200 {
                let guard = idx.read().unwrap();
                let r = guard
                    .search(&Query::MatchAll, None, 10)
                    .unwrap();
                assert!(r.total >= last_total, "total went backwards");
                last_total = r.total;
                drop(guard);
                std::thread::yield_now();
            }
        })
    };

    writer.join().unwrap();
    reader.join().unwrap();

    let guard = index.read().unwrap();
    let r = guard.search(&Query::MatchAll, None, 10).unwrap();
    assert_eq!(r.total, 500);
    drop(guard);
    fs::remove_dir_all(&root).unwrap();
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p rustlucene-core concurrent -- --nocapture`
Expected: PASS (both tests, no deadlock, no panic)

- [ ] **Step 3: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/index_writer.rs
git commit -m "test(search): concurrent read/write integration tests under RwLock"
```

---

### Task 9: Final regression + cleanup

**Files:**
- All modified files

- [ ] **Step 1: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests PASS (existing 99 + new tests).

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings.

- [ ] **Step 3: Verify no unsafe in core**

Run: `grep -r "unsafe" crates/core/src/`
Expected: No matches (forbid(unsafe_code) enforced by compiler anyway).

- [ ] **Step 4: Final commit if any cleanup needed**

```bash
git add -A
git commit -m "chore: clippy fixes and cleanup for real-time search"
```
