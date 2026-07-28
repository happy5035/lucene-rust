use std::io;
use std::path::Path;

use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::segment_infos::SegmentInfos;
use codec_lucene9::FSDirectory;

use crate::document::Document;
use crate::memory_reader::MemorySearcher;
use crate::schema::Schema;
use crate::search::doc_iter::DocIter;
use crate::search::query::Query;
use crate::search::segment_reader::SegmentReader;
use crate::search::sorted_collector::{SearchResults, SortedTopN};
use crate::segment_builder::SegmentBuilder;

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
            doc_base += sci.info.doc_count;
        }

        // 2. In-memory buffer (unflushed)
        if let Some(builder) = &self.builder {
            let mem = MemorySearcher::new(builder.doc_writer(), &self.schema, &[]);
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
}
