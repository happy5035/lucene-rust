use std::io;
use std::path::Path;

use codec_lucene9::segment_infos::SegmentInfos;
use codec_lucene9::FSDirectory;

use crate::document::Document;
use crate::schema::Schema;
use crate::segment_builder::SegmentBuilder;
use crate::sort::IndexSortField;

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
    /// Index sort (Lucene IndexWriterConfig.setIndexSort): when Some, each
    /// flushed segment's docs are physically reordered by this field. The
    /// field must carry NumericDocValues or SortedDocValues. None (default)
    /// keeps append-only docID order.
    pub index_sort: Option<IndexSortField>,
}

impl Default for IndexWriterConfig {
    fn default() -> Self {
        Self {
            max_buffered_docs: 1_000_000,
            max_ram_bytes: 512 * 1024 * 1024,
            bitmap: false,
            bitmap_threshold: 4096,
            index_sort: None,
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
            b.set_index_sort(self.config.index_sort.clone());
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
