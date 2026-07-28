use std::io;
use std::path::Path;

use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::segment_infos::SegmentInfos;
use codec_lucene9::FSDirectory;

use crate::document::Document;
use crate::memory_access::MemoryLeafAccess;
use crate::schema::Schema;
use crate::search::doc_iter::DocIter;
use crate::search::leaf_access::LeafAccess;
use crate::search::query::Query;
use crate::search::segment_reader::SegmentReader;
use crate::search::sorted_collector::{SearchResults, SortedTopN};
use crate::segment_builder::SegmentBuilder;

/// Where a global doc_id's stored fields live.
#[derive(Debug)]
pub enum DocLocation {
    /// In a committed segment's .fdt file.
    CommittedSegment {
        seg_name: String,
        seg_id: [u8; 16],
        local_id: u32,
    },
    /// In the current buffer's SFW (either flushed chunk or memory).
    Buffer { local_id: u32, flushed: bool },
    /// No document at this id.
    NotFound,
}

pub struct IndexWriterConfig {
    /// Flush when this many docs are buffered (Lucene default: disabled / RAM-based).
    pub max_buffered_docs: u32,
    /// Flush when the buffered indexing data (postings/docvalues/points
    /// arenas, approximate) exceeds this many bytes. Default 512MB, per the
    /// project spec; Lucene's fair-comparison counterpart is
    /// IndexWriterConfig.setRAMBufferSizeMB.
    pub max_ram_bytes: usize,
    /// M3 §4 (experimental, default off): build inline roaring bitmaps for
    /// terms with df >= `bitmap_threshold` at segment flush. Off ⇒ the
    /// written index is byte-identical to M2.
    pub bitmap: bool,
    /// df threshold for the inline bitmap (spec §4: 4096 对齐 level-1 skip
    /// 粒度 32×128).
    pub bitmap_threshold: u32,
}

impl Default for IndexWriterConfig {
    fn default() -> Self {
        Self {
            max_buffered_docs: 1_000_000,
            max_ram_bytes: 512 * 1024 * 1024,
            bitmap: false,
            bitmap_threshold: 4096,
        }
    }
}

/// Append-only index writer: owns the current [`SegmentBuilder`] (one per
/// segment) plus the commit protocol. For multi-threaded writes, shard
/// documents across builders and commit the union of their segments with
/// [`commit_segments`].
pub struct IndexWriter {
    dir: FSDirectory,
    schema: Schema,
    config: IndexWriterConfig,
    builder: Option<SegmentBuilder>,
    infos: SegmentInfos,
    segment_counter: u64,
    /// Next segments_N generation; first commit writes segments_1.
    generation: i64,
}

impl IndexWriter {
    /// Creates a writer on an empty directory (OpenMode.CREATE semantics for M1).
    pub fn create(path: &Path, schema: Schema, config: IndexWriterConfig) -> io::Result<Self> {
        let dir = FSDirectory::open(path)?;
        if dir.list_all()?.iter().any(|f| f.starts_with("segments_")) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "index already exists (M1 supports CREATE only)",
            ));
        }
        Ok(Self {
            dir,
            schema,
            config,
            builder: None,
            infos: SegmentInfos::new(),
            segment_counter: 0,
            generation: 1,
        })
    }

    /// Mutable access to the schema for dynamic field registration (JSON
    /// Dynamic/StoredOnly policies). The schema is append-only; the current
    /// segment builder picks up new fields on the next `add_document`.
    pub fn schema_mut(&mut self) -> &mut Schema {
        &mut self.schema
    }

    pub fn add_document(&mut self, doc: Document) -> io::Result<()> {
        if self.builder.is_none() {
            let mut b = SegmentBuilder::new(self.dir.clone(), self.segment_counter);
            b.set_bitmap_threshold(if self.config.bitmap {
                Some(self.config.bitmap_threshold)
            } else {
                None
            });
            self.builder = Some(b);
            self.segment_counter += 1;
        }
        let b = self.builder.as_mut().unwrap();
        b.add_document(&self.schema, doc)?;
        if b.buffered_docs() >= self.config.max_buffered_docs
            || b.ram_bytes() >= self.config.max_ram_bytes
        {
            self.flush()?;
        }
        Ok(())
    }

    /// Flushes buffered docs into a new segment (files are written but
    /// invisible to readers until `commit`).
    pub fn flush(&mut self) -> io::Result<()> {
        if let Some(b) = self.builder.take() {
            if let Some(sci) = b.finalize()? {
                self.infos.segments.push(sci);
                self.infos.counter = self.segment_counter as i64;
                if self.infos.min_segment_version.is_none() {
                    self.infos.min_segment_version = Some((9, 12, 3));
                }
            }
        }
        Ok(())
    }

    /// Flushes and publishes a commit point with Lucene's two-phase protocol:
    /// fsync all segment files → write `pending_segments_N` → fsync →
    /// rename to `segments_N` → dir fsync (IndexWriter.commit /
    /// SegmentInfos.prepareCommit+finishCommit semantics). A crash before
    /// `finish_commit` can only leave a `pending_segments_N` behind, never a
    /// corrupt commit point.
    pub fn commit(&mut self) -> io::Result<()> {
        self.flush()?;
        commit_infos(&self.dir, &mut self.infos, self.generation)?;
        self.generation += 1;
        Ok(())
    }

    /// Schema accessor (needed by search to construct MemoryLeafAccess).
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Directory path accessor (for stored-field disk reads).
    pub fn dir_path(&self) -> &Path {
        self.dir.path()
    }

    /// Raw stored-field bytes of an unflushed buffered document.
    /// Returns None if the builder has no SFW or the doc is out of range.
    pub fn buffered_stored_bytes(&self, local_id: u32) -> Option<&[u8]> {
        self.builder.as_ref().and_then(|b| {
            let flushed = b.sfw_flushed_doc_count().max(0) as u32;
            let buffer_idx = local_id.checked_sub(flushed)?;
            b.sfw_buffered_doc_bytes(buffer_idx)
        })
    }

    /// Maps a global doc_id to where its stored fields live.
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

        // INDEXORDER: collector still works (sort_value=0 for all, tie-break by doc_id)
        Ok(collector.results())
    }

    fn drive_segment<L: LeafAccess>(
        seg: &mut L,
        query: &Query,
        collector: &mut SortedTopN,
        doc_base: i32,
        sort_field: Option<(&str, bool)>,
    ) -> io::Result<()> {
        if let Some(mut iter) = query.segment_iterator(seg, false)? {
            loop {
                let doc = iter.next_doc()?;
                if doc == NO_MORE_DOCS {
                    break;
                }
                if !iter.matches()? {
                    continue;
                }
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
}

/// Commits an externally-assembled `SegmentInfos` (e.g. the union of segments
/// produced by sharded builders) as the next generation in `dir`. Flushed
/// segment files must already be fully written.
pub fn commit_segments(
    dir_path: &Path,
    mut infos: SegmentInfos,
    generation: i64,
) -> io::Result<()> {
    let dir = FSDirectory::open(dir_path)?;
    commit_infos(&dir, &mut infos, generation)
}

pub(crate) fn commit_infos(
    dir: &FSDirectory,
    infos: &mut SegmentInfos,
    generation: i64,
) -> io::Result<()> {
    infos.version += 1;
    // Make every referenced segment file durable before publishing the
    // commit point that references them.
    let mut files: Vec<String> = Vec::new();
    for sci in &infos.segments {
        files.extend(sci.info.files.iter().cloned());
    }
    let file_refs: Vec<&str> = files.iter().map(String::as_str).collect();
    dir.sync(&file_refs)?;
    infos.commit(dir, generation)
}

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

    #[test]
    fn document_location_variants() {
        let root = temp_dir("docloc");
        let mut w = IndexWriter::create(&root, rt_schema(), IndexWriterConfig::default()).unwrap();

        // Empty index: everything is NotFound
        assert!(matches!(w.document_location(0), DocLocation::NotFound));

        // Add 5 docs (buffered, not flushed)
        for i in 0..5u32 {
            w.add_document(rt_doc("INFO", "hello", 1000 + i as i64))
                .unwrap();
        }
        // Buffered docs: local_id 0..5
        match w.document_location(0) {
            DocLocation::Buffer { local_id, flushed } => {
                assert_eq!(local_id, 0);
                assert!(!flushed); // small docs, not flushed to chunk
            }
            other => panic!("expected Buffer, got {other:?}"),
        }
        match w.document_location(4) {
            DocLocation::Buffer { local_id, .. } => assert_eq!(local_id, 4),
            other => panic!("expected Buffer, got {other:?}"),
        }
        assert!(matches!(w.document_location(5), DocLocation::NotFound));

        // Flush: docs move to committed segment
        w.flush().unwrap();
        match w.document_location(0) {
            DocLocation::CommittedSegment {
                seg_name,
                local_id,
                ..
            } => {
                assert_eq!(seg_name, "_0");
                assert_eq!(local_id, 0);
            }
            other => panic!("expected CommittedSegment, got {other:?}"),
        }
        match w.document_location(4) {
            DocLocation::CommittedSegment { local_id, .. } => assert_eq!(local_id, 4),
            other => panic!("expected CommittedSegment, got {other:?}"),
        }
        assert!(matches!(w.document_location(5), DocLocation::NotFound));

        // Add 3 more docs (buffered after committed segment)
        for i in 5..8u32 {
            w.add_document(rt_doc("INFO", "world", 1000 + i as i64))
                .unwrap();
        }
        // global_id 5 => Buffer local_id 0
        match w.document_location(5) {
            DocLocation::Buffer { local_id, .. } => assert_eq!(local_id, 0),
            other => panic!("expected Buffer, got {other:?}"),
        }
        // global_id 7 => Buffer local_id 2
        match w.document_location(7) {
            DocLocation::Buffer { local_id, .. } => assert_eq!(local_id, 2),
            other => panic!("expected Buffer, got {other:?}"),
        }
        assert!(matches!(w.document_location(8), DocLocation::NotFound));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn buffered_stored_bytes_after_sfw_chunk_flush() {
        let root = temp_dir("sfwflush");
        let mut w = IndexWriter::create(&root, rt_schema(), IndexWriterConfig::default()).unwrap();

        // 1100 docs with a stored text field exceeds MAX_DOCS_PER_CHUNK
        // (1024), forcing the SFW to flush at least one chunk to disk. Each
        // doc carries a unique message so we can verify the right slot.
        for i in 0..1100u32 {
            w.add_document(rt_doc("INFO", &format!("msg-{i}"), 1000 + i as i64))
                .unwrap();
        }

        // The SFW must have flushed a chunk; docs [flushed..1100) remain in
        // the unflushed buffer.
        let flushed = w.builder.as_ref().unwrap().sfw_flushed_doc_count();
        assert!(
            flushed >= 1024,
            "expected an SFW chunk flush, flushed={flushed}"
        );
        let flushed = flushed as u32;

        // Docs in the flushed region are not in the unflushed buffer.
        assert!(w.buffered_stored_bytes(0).is_none());
        assert!(w.buffered_stored_bytes(flushed - 1).is_none());

        // Docs in the unflushed portion resolve to their own bytes (offset
        // rebased by the flushed doc count). Before the fix, local_id was
        // passed straight through, indexing into the wrong buffer slot.
        for local_id in [flushed, flushed + 1, 1099] {
            let bytes = w
                .buffered_stored_bytes(local_id)
                .unwrap_or_else(|| panic!("no buffered bytes for local_id {local_id}"));
            let needle = format!("msg-{local_id}");
            assert!(
                bytes.windows(needle.len()).any(|win| win == needle.as_bytes()),
                "bytes for local_id {local_id} do not contain {needle:?}"
            );
        }

        // Out of range.
        assert!(w.buffered_stored_bytes(1100).is_none());

        fs::remove_dir_all(&root).unwrap();
    }

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

    // ── Migrated from memory_reader.rs (Task 7) ─────────────────────
    // These tests exercise the unified LeafAccess path via IndexWriter::search().

    fn mem_schema() -> Schema {
        let mut s = Schema::new();
        s.add(FieldSpec::keyword("level"));
        s.add(FieldSpec::keyword("tid"));
        s.add(FieldSpec::text("message"));
        s.add(FieldSpec::long_point("ts").with_numeric_dv());
        s
    }

    fn build_mem_writer(tag: &str) -> IndexWriter {
        let root = temp_dir(tag);
        let schema = mem_schema();
        let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();
        for i in 0..10u32 {
            let level = if i % 2 == 0 { "INFO" } else { "WARN" };
            let mut doc = Document::new();
            doc.add("level", FieldValue::Keyword(level.to_string()));
            doc.add("tid", FieldValue::Keyword(format!("tid-{i}")));
            doc.add("message", FieldValue::Text(format!("w{} common", i % 3)));
            doc.add("ts", FieldValue::Long(1000 + i as i64));
            w.add_document(doc).unwrap();
        }
        w
    }

    #[test]
    fn unified_term_query() {
        let w = build_mem_writer("unified-term");
        assert_eq!(w.search(&Query::term("level", "INFO"), None, 10).unwrap().total, 5);
        assert_eq!(w.search(&Query::term("level", "WARN"), None, 10).unwrap().total, 5);
        assert_eq!(w.search(&Query::term("level", "DEBUG"), None, 10).unwrap().total, 0);
        assert_eq!(w.search(&Query::term("tid", "tid-7"), None, 10).unwrap().total, 1);
        assert_eq!(w.search(&Query::term("message", "common"), None, 10).unwrap().total, 10);
        assert_eq!(w.search(&Query::term("message", "w0"), None, 10).unwrap().total, 4);
    }

    #[test]
    fn unified_matchall() {
        let w = build_mem_writer("unified-matchall");
        let r = w.search(&Query::MatchAll, None, 5).unwrap();
        assert_eq!(r.total, 10);
        assert_eq!(r.docs, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn unified_and_or() {
        let w = build_mem_writer("unified-andor");

        // AND: w0 ∩ common = docs with both = {0,3,6,9}
        let q = Query::and("message", &["w0", "common"]);
        assert_eq!(w.search(&q, None, 10).unwrap().total, 4);

        // AND with missing term → 0
        let q = Query::and("message", &["w0", "nosuch"]);
        assert_eq!(w.search(&q, None, 10).unwrap().total, 0);

        // OR: w0 ∪ w1 = {0,1,3,4,6,7,9}
        let q = Query::or("message", &["w0", "w1"]);
        assert_eq!(w.search(&q, None, 10).unwrap().total, 7);
    }

    #[test]
    fn unified_prefix() {
        let w = build_mem_writer("unified-prefix");

        // prefix "w" matches w0, w1, w2 → all 10 docs
        assert_eq!(w.search(&Query::prefix("message", "w"), None, 10).unwrap().total, 10);
        // prefix "w0" matches only w0 → 4 docs
        assert_eq!(w.search(&Query::prefix("message", "w0"), None, 10).unwrap().total, 4);
        // prefix "tid-1" matches tid-1 only
        assert_eq!(w.search(&Query::prefix("tid", "tid-1"), None, 10).unwrap().total, 1);
    }

    #[test]
    fn unified_wildcard() {
        let w = build_mem_writer("unified-wildcard");

        assert_eq!(w.search(&Query::wildcard("message", "w*"), None, 10).unwrap().total, 10);
        assert_eq!(w.search(&Query::wildcard("message", "w?"), None, 10).unwrap().total, 10);
        assert_eq!(w.search(&Query::wildcard("level", "INF*"), None, 10).unwrap().total, 5);
        assert_eq!(w.search(&Query::wildcard("level", "????"), None, 10).unwrap().total, 10);
    }

    #[test]
    fn unified_point_range() {
        let w = build_mem_writer("unified-pointrange");

        // ts values: 1000..1009
        let q = Query::point_range("ts", 1002, 1005);
        let r = w.search(&q, None, 10).unwrap();
        assert_eq!(r.total, 4);
        assert_eq!(r.docs, vec![2, 3, 4, 5]);

        // Full range
        let q = Query::point_range("ts", 0, 9999);
        assert_eq!(w.search(&q, None, 10).unwrap().total, 10);

        // Empty range
        let q = Query::point_range("ts", 2000, 3000);
        assert_eq!(w.search(&q, None, 10).unwrap().total, 0);
    }

    #[test]
    fn unified_bool_query() {
        use crate::search::query::Occur;
        let w = build_mem_writer("unified-bool");

        // MUST level=INFO AND message=w0 → docs 0,6
        let q = Query::bool(vec![
            (Occur::Must, Query::term("level", "INFO")),
            (Occur::Must, Query::term("message", "w0")),
        ]);
        let r = w.search(&q, None, 10).unwrap();
        assert_eq!(r.total, 2);
        assert_eq!(r.docs, vec![0, 6]);

        // SHOULD: level=INFO OR level=WARN → all 10
        let q = Query::bool(vec![
            (Occur::Should, Query::term("level", "INFO")),
            (Occur::Should, Query::term("level", "WARN")),
        ]);
        assert_eq!(w.search(&q, None, 10).unwrap().total, 10);

        // MUST + MUST_NOT: message=common NOT level=WARN → INFO docs = 5
        let q = Query::bool(vec![
            (Occur::Must, Query::term("message", "common")),
            (Occur::MustNot, Query::term("level", "WARN")),
        ]);
        assert_eq!(w.search(&q, None, 10).unwrap().total, 5);
    }

    #[test]
    fn unified_phrase_query() {
        let root = temp_dir("unified-phrase");
        let mut schema = Schema::new();
        schema.add(FieldSpec::text_with_positions("message"));
        let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();

        let docs_text = [
            "quick brown fox",   // 0: "quick brown" hit
            "quick fox brown",   // 1: not adjacent
            "quick quick brown", // 2: 2nd quick aligns
        ];
        for text in &docs_text {
            let mut doc = Document::new();
            doc.add("message", FieldValue::Text(text.to_string()));
            w.add_document(doc).unwrap();
        }

        let q = Query::phrase("message", &["quick", "brown"]);
        let r = w.search(&q, None, 10).unwrap();
        assert_eq!(r.total, 2);
        assert_eq!(r.docs, vec![0, 2]);

        let q = Query::phrase("message", &["quick", "fox"]);
        let r = w.search(&q, None, 10).unwrap();
        assert_eq!(r.docs, vec![1]);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unified_unknown_field_returns_zero() {
        let w = build_mem_writer("unified-unknownfield");
        assert_eq!(w.search(&Query::term("nope", "x"), None, 10).unwrap().total, 0);
        assert_eq!(w.search(&Query::prefix("nope", "x"), None, 10).unwrap().total, 0);
    }

    #[test]
    fn unified_searchable_immediately_after_each_write() {
        let root = temp_dir("unified-immediate");
        let schema = mem_schema();
        let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();

        for i in 0..5u32 {
            let mut doc = Document::new();
            doc.add("level", FieldValue::Keyword("INFO".to_string()));
            doc.add("tid", FieldValue::Keyword(format!("tid-{i}")));
            doc.add("message", FieldValue::Text("hello world".to_string()));
            doc.add("ts", FieldValue::Long(i as i64));
            w.add_document(doc).unwrap();

            assert_eq!(
                w.search(&Query::term("level", "INFO"), None, 10).unwrap().total,
                (i + 1) as u64
            );
            assert_eq!(
                w.search(&Query::MatchAll, None, 10).unwrap().total,
                (i + 1) as u64
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
