use std::collections::HashMap;
use std::io;

use codec_lucene9::doc_values::DocValuesWriter;
use codec_lucene9::field_infos::{FieldInfo, FieldInfos};
use codec_lucene9::points::PointsWriter;
use codec_lucene9::postings::PostingsWriter;
use codec_lucene9::segment_info::SegmentInfo;
use codec_lucene9::segment_infos::{random_id, SegmentCommitInfo};
use codec_lucene9::stored_fields::{self, StoredField, StoredFieldsWriter};
use codec_lucene9::{DocValuesType, FSDirectory, IndexSortFieldInfo, MissingValue, SortFieldType};

use crate::doc_writer::DocWriter;
use crate::document::{Document, FieldValue};
use crate::schema::Schema;
use crate::sort::{self, DocMap, IndexSortField};

/// Per-field attribute entries the PerField reader requires on indexed fields
/// (PerFieldPostingsFormat.java:73-78; observed in Java-written .fnm).
const PFF_FORMAT_KEY: &str = "PerFieldPostingsFormat.format";
const PFF_SUFFIX_KEY: &str = "PerFieldPostingsFormat.suffix";
const PFF_FORMAT_VALUE: &str = "Lucene912";
const PFF_SUFFIX_VALUE: &str = "0";

/// Per-field attribute entries the PerField reader requires on DocValues
/// fields (PerFieldDocValuesFormat.java:62-67, 189/234). The segment suffix
/// is what the DV file names carry: `_N_Lucene90_0.dvd`.
const PFDVF_FORMAT_KEY: &str = "PerFieldDocValuesFormat.format";
const PFDVF_SUFFIX_KEY: &str = "PerFieldDocValuesFormat.suffix";
const PFDVF_FORMAT_VALUE: &str = "Lucene90";
const PFDVF_SUFFIX_VALUE: &str = "0";
const DV_SEGMENT_SUFFIX: &str = "Lucene90_0";

/// Builds one segment from a document stream: stored fields stream to disk
/// as documents arrive (Lucene's StoredFieldsConsumer model — RAM stays
/// bounded by one compression chunk instead of the whole corpus), postings
/// buffer in RAM, and [`Self::finalize`] writes the remaining files
/// (.fdx/.fdm, postings, .si). Single-use: one builder == one segment.
///
/// Single-threaded by design; M3 shards documents across builders.
pub struct SegmentBuilder {
    dir: FSDirectory,
    seg_name: String,
    seg_id: [u8; 16],
    dw: DocWriter,
    sfw: Option<StoredFieldsWriter>,
    /// M3 §4: Some(t) → finalize 时对 df >= t 的 term 写内联 bitmap。
    bitmap_threshold: Option<u32>,
    /// Index sort (Lucene setIndexSort): when Some, finalize physically
    /// reorders the segment's docs by this field.
    index_sort: Option<IndexSortField>,
    /// Per-oldDoc retained stored fields, populated only when `index_sort` is
    /// active (stored fields can't be streamed in original order then — they
    /// are written in sorted order at finalize). Parallel to docID.
    stored_docs: Vec<Vec<(String, FieldValue)>>,
}

impl SegmentBuilder {
    /// Names the segment `_<base36(name_counter)>`. Files appear on disk
    /// from the first added document but only become visible to readers via
    /// a `SegmentInfos` commit that references the finalized segment.
    pub fn new(dir: FSDirectory, name_counter: u64) -> Self {
        Self {
            dir,
            seg_name: format!("_{}", to_base36(name_counter)),
            seg_id: random_id(),
            dw: DocWriter::new(),
            sfw: None,
            bitmap_threshold: None,
            index_sort: None,
            stored_docs: Vec::new(),
        }
    }

    pub fn buffered_docs(&self) -> u32 {
        self.dw.max_doc
    }

    /// M3 §4: `Some(t)` → write inline roaring bitmaps for terms with
    /// df >= t at finalize; None (default) keeps .doc byte-identical to M2.
    pub fn set_bitmap_threshold(&mut self, threshold: Option<u32>) {
        self.bitmap_threshold = threshold;
    }

    /// Index sort: `Some(field)` → physically reorder the segment's docs by
    /// `field` (which must carry Numeric or Sorted DocValues) at finalize.
    pub fn set_index_sort(&mut self, sort: Option<IndexSortField>) {
        self.index_sort = sort;
    }

    /// Approximate RAM held by the indexing buffers (postings/docvalues/
    /// points arenas) plus one stored-fields compression chunk cushion.
    pub fn ram_bytes(&self) -> usize {
        self.dw.ram_bytes() + (1 << 17)
    }

    pub fn add_document(&mut self, schema: &Schema, doc: Document) -> io::Result<()> {
        if self.index_sort.is_some() {
            // index sort: retain stored fields in RAM (filtered to stored
            // fields) and index without streaming stored — finalize writes
            // them back in sorted order.
            let stored: Vec<(String, FieldValue)> = doc
                .fields
                .iter()
                .filter(|(name, _)| schema.get(name).map(|s| s.stored).unwrap_or(false))
                .cloned()
                .collect();
            self.stored_docs.push(stored);
            return self.dw.add_document(schema, doc, None);
        }
        if self.sfw.is_none() {
            self.sfw = Some(StoredFieldsWriter::new(
                &self.dir,
                &self.seg_name,
                self.seg_id,
                "",
            )?);
        }
        self.dw.add_document(schema, doc, self.sfw.as_mut())
    }

    /// Flushes buffered docs into the segment files (synced on disk, but
    /// only visible to readers once a `SegmentInfos` commit references them)
    /// and returns the commit info. No-op when empty; consumes the builder.
    pub fn finalize(self) -> io::Result<Option<SegmentCommitInfo>> {
        if self.dw.max_doc == 0 {
            return Ok(None);
        }
        let Self {
            dir,
            seg_name,
            seg_id,
            mut dw,
            sfw,
            bitmap_threshold,
            index_sort,
            stored_docs,
        } = self;
        let max_doc = dw.max_doc as i32;

        // Index sort: resolve the sort field, compute the docID permutation
        // and apply it to the RAM buffers before any format encoding (the
        // writers below all assume ascending docIDs, so they then emit the
        // physically reordered segment unchanged). No sort ⇒ map stays None.
        // Also build the codec IndexSortFieldInfo for .si persistence.
        let mut si_index_sort: Vec<IndexSortFieldInfo> = Vec::new();
        let doc_map: Option<DocMap> = match &index_sort {
            Some(sort) => {
                let field_num = dw
                    .fields()
                    .iter()
                    .position(|f| f.name == sort.field)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("index sort field not found: {}", sort.field),
                        )
                    })? as u32;
                let keys = dw.sort_keys(field_num).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "index sort field {} has no Numeric/Sorted DocValues",
                            sort.field
                        ),
                    )
                })?;
                // Value type + missing sentinel for .si, from the DV buffer kind.
                let buf = dw.field_buffer(field_num);
                let (field_type, missing) = if buf.and_then(|b| b.sorted_dv.as_ref()).is_some() {
                    let mv = match sort.missing {
                        sort::Missing::First => MissingValue::StringFirst,
                        sort::Missing::Last => MissingValue::StringLast,
                    };
                    (SortFieldType::String, Some(mv))
                } else {
                    let mv = match sort.missing {
                        sort::Missing::First => MissingValue::Long(i64::MIN),
                        sort::Missing::Last => MissingValue::Long(i64::MAX),
                    };
                    (SortFieldType::Long, Some(mv))
                };
                si_index_sort.push(IndexSortFieldInfo {
                    field: sort.field.clone(),
                    field_type,
                    reverse: sort.reverse,
                    missing,
                });
                let map = sort::compute(max_doc as u32, &keys, sort.reverse, sort.missing);
                if let Some(m) = &map {
                    dw.apply_doc_map(m);
                }
                map
            }
            None => None,
        };

        // 1. .fnm — fields in field-number order. Written first (attributes
        //    are known upfront: Lucene912/0 for indexed, Lucene90/0 for DV).
        //    Fields the segment never saw data for (possible after dynamic
        //    schema growth) lose their index/point flags — Lucene never marks
        //    a field indexed/pointed without data either; all-missing DV
        //    entries are written regardless (the .fnm/.dvm pairing is
        //    required: docs/format-notes-docvalues.md §8).
        let mut field_infos_vec: Vec<FieldInfo> = Vec::new();
        for (number, spec) in dw.fields().iter().enumerate() {
            let number = number as i32;
            let mut fi = FieldInfo::stored(&spec.name, number);
            if spec.is_indexed() && field_has_terms(&dw, number as usize) {
                fi.index_options = spec.index_options;
                fi.omit_norms = true;
                fi.attributes
                    .insert(PFF_FORMAT_KEY.to_string(), PFF_FORMAT_VALUE.to_string());
                fi.attributes
                    .insert(PFF_SUFFIX_KEY.to_string(), PFF_SUFFIX_VALUE.to_string());
            }
            if spec.doc_values != DocValuesType::None {
                fi.doc_values_type = spec.doc_values;
                fi.doc_values_gen = -1; // fresh flush (FieldInfos.java:830-831)
                fi.attributes
                    .insert(PFDVF_FORMAT_KEY.to_string(), PFDVF_FORMAT_VALUE.to_string());
                fi.attributes
                    .insert(PFDVF_SUFFIX_KEY.to_string(), PFDVF_SUFFIX_VALUE.to_string());
            }
            if let Some(p) = spec.points {
                if field_has_points(&dw, number as usize) {
                    fi.point_dimension_count = 1;
                    fi.point_index_dimension_count = 1;
                    fi.point_num_bytes = p.bytes_per_dim as i32;
                }
            }
            field_infos_vec.push(fi);
        }
        let field_infos = FieldInfos::new(field_infos_vec);
        let fnm_file = field_infos.write(&dir, &seg_name, &seg_id, "")?;

        // 2. Stored fields. Without index sort the .fdt data was streamed
        //    during add_document; with index sort it was retained in RAM
        //    (stored_docs) and is written here in sorted (newToOld) order.
        //    finish writes the last chunk plus .fdx/.fdm either way.
        let sfw = if index_sort.is_some() {
            let mut w = StoredFieldsWriter::new(&dir, &seg_name, seg_id, "")?;
            let name_to_num: HashMap<&str, u32> = dw
                .fields()
                .iter()
                .enumerate()
                .map(|(n, f)| (f.name.as_str(), n as u32))
                .collect();
            for new_doc in 0..max_doc as usize {
                let old_doc = doc_map
                    .as_ref()
                    .map(|m| m.new_to_old(new_doc as u32) as usize)
                    .unwrap_or(new_doc);
                let fields: Vec<(u32, StoredField)> = stored_docs[old_doc]
                    .iter()
                    .filter_map(|(name, value)| {
                        let num = *name_to_num.get(name.as_str())?;
                        Some((num, to_stored_field(value)))
                    })
                    .collect();
                w.write_document(&fields)?;
            }
            w
        } else {
            sfw.expect("sfw is created on first add_document")
        };
        sfw.finish(max_doc, &dir)?;
        let stored_files = stored_fields::file_names(&seg_name, "");

        // 3. Postings (.tim/.tip/.tmd/.doc/.psm[/+.pos]) — only when indexed fields exist.
        let mut postings_files: Vec<String> = Vec::new();
        let has_indexed = dw
            .fields()
            .iter()
            .enumerate()
            .any(|(n, f)| f.is_indexed() && field_has_terms(&dw, n));
        if has_indexed {
            let mut pw = PostingsWriter::new(&dir, &seg_name, &seg_id)?
                .with_bitmap_threshold(bitmap_threshold);
            for (number, spec) in dw.fields().iter().enumerate() {
                if !spec.is_indexed() || !field_has_terms(&dw, number) {
                    continue;
                }
                let buf = dw.field_buffer(number as u32).unwrap();
                let dict = buf.dict.as_ref().unwrap();
                let fi = field_infos.by_name(&spec.name).unwrap();
                pw.start_field(fi, buf.doc_count)?;
                for id in dict.sorted_ids() {
                    let pb = dict.postings(id);
                    let positions = if spec.has_positions() {
                        Some(pb.positions.as_slice())
                    } else {
                        None
                    };
                    pw.write_term(dict.bytes_of(id), &pb.docs, &pb.freqs, positions)?;
                }
                pw.finish_field()?;
            }
            postings_files = pw.finish()?;
        }

        // 4. Doc values (_N_Lucene90_0.dvd/.dvm) — one entry per DV field,
        //    including all-missing fields (the .fnm/.dvm pairing is required:
        //    docs/format-notes-docvalues.md §8).
        let mut dv_files: Vec<String> = Vec::new();
        let has_dv = dw
            .fields()
            .iter()
            .any(|f| f.doc_values != DocValuesType::None);
        if has_dv {
            let mut dvw = DocValuesWriter::new(&dir, &seg_name, &seg_id, DV_SEGMENT_SUFFIX)?;
            for number in 0..dw.fields().len() {
                let dv_type = dw.fields()[number].doc_values;
                if dv_type == DocValuesType::None {
                    continue;
                }
                let buf = dw.field_buffer_mut(number as u32).unwrap();
                match dv_type {
                    DocValuesType::Numeric => {
                        let dv = buf.numeric_dv.as_ref().unwrap();
                        let pairs: Vec<(u32, i64)> = dv
                            .docs
                            .iter()
                            .copied()
                            .zip(dv.values.iter().copied())
                            .collect();
                        dvw.add_numeric_field(number as i32, max_doc as u32, &pairs)?;
                    }
                    DocValuesType::Sorted => {
                        let dv = buf.sorted_dv.as_mut().unwrap();
                        // Ords = rank in the sorted dictionary (Lucene assigns
                        // ords by sorted term order, SortedDocValuesWriter.java:113-125).
                        let sorted = dv.dict.sorted_ids();
                        let mut remap = vec![0u32; dv.dict.len()];
                        for (ord, &id) in sorted.iter().enumerate() {
                            remap[id as usize] = ord as u32;
                        }
                        let dict: Vec<&[u8]> =
                            sorted.iter().map(|&id| dv.dict.bytes_of(id)).collect();
                        let ords: Vec<(u32, u32)> = dv
                            .docs
                            .iter()
                            .zip(&dv.term_ids)
                            .map(|(&d, &t)| (d, remap[t as usize]))
                            .collect();
                        dvw.add_sorted_field(number as i32, max_doc as u32, &dict, &ords)?;
                    }
                    DocValuesType::Binary => {
                        if let Some(dv) = buf.binary_dv.as_ref() {
                            let pairs: Vec<(u32, Vec<u8>)> = dv
                                .docs
                                .iter()
                                .copied()
                                .zip(dv.values.iter().cloned())
                                .collect();
                            dvw.add_binary_field(number as i32, max_doc as u32, &pairs)?;
                        } else {
                            dvw.add_binary_field(number as i32, max_doc as u32, &[])?;
                        }
                    }
                    _ => { /* None / SortedSet / SortedNumeric: skip */ }
                }
            }
            dv_files = dvw.finish()?;
        }

        // 5. Points (_N.kdd/.kdi/.kdm) — 1D BKD per point field.
        let mut point_files: Vec<String> = Vec::new();
        let has_points = dw
            .fields()
            .iter()
            .enumerate()
            .any(|(n, f)| f.points.is_some() && field_has_points(&dw, n));
        if has_points {
            let mut ptw = PointsWriter::new(&dir, &seg_name, &seg_id)?;
            for number in 0..dw.fields().len() {
                let Some(p) = dw.fields()[number].points else {
                    continue;
                };
                if !field_has_points(&dw, number) {
                    continue;
                }
                let buf = dw.field_buffer_mut(number as u32).unwrap();
                let pts = &mut buf.points.as_mut().unwrap().points;
                if p.bytes_per_dim == 8 {
                    ptw.write_field_long(number as i32, pts)?;
                } else {
                    let mut narrowed: Vec<(i32, u32)> =
                        pts.iter().map(|&(v, d)| (v as i32, d)).collect();
                    ptw.write_field_int(number as i32, &mut narrowed)?;
                }
            }
            point_files = ptw.finish()?;
        }

        // 6. .si — files set includes the .si itself (observed in Java-written indexes).
        let mut si = SegmentInfo::new(&seg_name, seg_id, max_doc);
        si.diagnostics
            .insert("source".to_string(), "flush".to_string());
        si.diagnostics
            .insert("lucene.version".to_string(), "9.12.3".to_string());
        si.attributes.insert(
            "Lucene90StoredFieldsFormat.mode".to_string(),
            "BEST_SPEED".to_string(),
        );
        si.files.insert(fnm_file);
        si.files.extend(stored_files);
        si.files.extend(postings_files);
        si.files.extend(dv_files);
        si.files.extend(point_files);
        si.files.insert(format!("{seg_name}.si"));
        si.index_sort = si_index_sort;
        si.write(&dir, "")?;

        Ok(Some(SegmentCommitInfo::new(si, random_id())))
    }
}

/// Indexed field with at least one term in this segment?
fn field_has_terms(dw: &DocWriter, number: usize) -> bool {
    dw.field_buffer(number as u32)
        .map(|b| b.doc_count > 0)
        .unwrap_or(false)
}

/// Maps a field value to its stored-fields representation (mirrors the inline
/// mapping in DocWriter::add_document).
fn to_stored_field(value: &FieldValue) -> StoredField {
    match value {
        FieldValue::Text(s) | FieldValue::Keyword(s) => StoredField::String(s.clone()),
        FieldValue::Long(v) => StoredField::Long(*v),
        FieldValue::Int(v) => StoredField::Int(*v),
        FieldValue::Bytes(b) => StoredField::Bytes(b.clone()),
    }
}

/// Point field with at least one point in this segment?
fn field_has_points(dw: &DocWriter, number: usize) -> bool {
    dw.field_buffer(number as u32)
        .and_then(|b| b.points.as_ref())
        .map(|p| !p.points.is_empty())
        .unwrap_or(false)
}

/// Long.toString(v, 36) equivalent for non-negative values.
pub fn to_base36(mut v: u64) -> String {
    if v == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while v > 0 {
        let d = (v % 36) as u8;
        out.push(if d < 10 { b'0' + d } else { b'a' + d - 10 });
        v /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

#[cfg(test)]
mod tests {
    use super::to_base36;

    #[test]
    fn base36() {
        assert_eq!(to_base36(0), "0");
        assert_eq!(to_base36(9), "9");
        assert_eq!(to_base36(10), "a");
        assert_eq!(to_base36(35), "z");
        assert_eq!(to_base36(36), "10");
        assert_eq!(to_base36(36 * 36 + 1), "101");
    }

    #[test]
    fn test_binary_dv_roundtrip() {
        use crate::document::{Document, FieldValue};
        use crate::index_writer::{IndexWriter, IndexWriterConfig};
        use crate::schema::{FieldSpec, Schema};
        use crate::search::Searcher;
        use codec_lucene9::FSDirectory;

        let root = std::env::temp_dir()
            .join(format!("rustlucene-bin-dv-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let mut schema = Schema::new();
        schema.add(FieldSpec::keyword("name").with_sorted_dv());
        schema.add(FieldSpec::binary_dv("data"));

        let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();
        let mut doc = Document::new();
        doc.add("name", FieldValue::Keyword("series1".to_string()));
        doc.add("data", FieldValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        w.add_document(doc).unwrap();
        w.commit().unwrap();
        drop(w);

        // Verify the index can be opened and has the expected doc count
        let dir = FSDirectory::open(&root).unwrap();
        let s = Searcher::open(&dir).unwrap();
        assert_eq!(s.max_doc(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_binary_dv_read_back() {
        use crate::document::{Document, FieldValue};
        use crate::index_writer::{IndexWriter, IndexWriterConfig};
        use crate::schema::{FieldSpec, Schema};
        use crate::search::reader::Reader;
        use codec_lucene9::FSDirectory;

        let root = std::env::temp_dir()
            .join(format!("rustlucene-bin-dv-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let mut schema = Schema::new();
        schema.add(FieldSpec::keyword("name").with_sorted_dv());
        schema.add(FieldSpec::binary_dv("data"));

        let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();
        let mut doc = Document::new();
        doc.add("name", FieldValue::Keyword("s1".to_string()));
        doc.add("data", FieldValue::Bytes(vec![0xDE, 0xAD]));
        w.add_document(doc).unwrap();
        let mut doc2 = Document::new();
        doc2.add("name", FieldValue::Keyword("s2".to_string()));
        doc2.add("data", FieldValue::Bytes(vec![0xBE, 0xEF, 0x00]));
        w.add_document(doc2).unwrap();
        w.commit().unwrap();
        drop(w);

        let dir = FSDirectory::open(&root).unwrap();
        let mut reader = Reader::open(&dir).unwrap();
        for (_doc_base, seg) in reader.leaves() {
            let bins = seg.binary_values("data").unwrap();
            assert_eq!(bins.len(), 2);
            let by_doc: std::collections::HashMap<u32, Vec<u8>> = bins.into_iter().collect();
            assert_eq!(by_doc.get(&0), Some(&vec![0xDE, 0xAD]));
            assert_eq!(by_doc.get(&1), Some(&vec![0xBE, 0xEF, 0x00]));
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
