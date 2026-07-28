use std::io;

use codec_lucene9::doc_values::DocValuesWriter;
use codec_lucene9::field_infos::{FieldInfo, FieldInfos};
use codec_lucene9::points::PointsWriter;
use codec_lucene9::postings::PostingsWriter;
use codec_lucene9::segment_info::SegmentInfo;
use codec_lucene9::segment_infos::{random_id, SegmentCommitInfo};
use codec_lucene9::stored_fields::{self, StoredFieldsWriter};
use codec_lucene9::{DocValuesType, FSDirectory};

use crate::doc_writer::DocWriter;
use crate::document::Document;
use crate::schema::Schema;

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
        }
    }

    pub fn buffered_docs(&self) -> u32 {
        self.dw.max_doc
    }

    /// Read-only access to the internal DocWriter (for real-time search).
    pub fn doc_writer(&self) -> &DocWriter {
        &self.dw
    }

    /// M3 §4: `Some(t)` → write inline roaring bitmaps for terms with
    /// df >= t at finalize; None (default) keeps .doc byte-identical to M2.
    pub fn set_bitmap_threshold(&mut self, threshold: Option<u32>) {
        self.bitmap_threshold = threshold;
    }

    /// Number of docs already flushed to disk in the SFW (completed chunks).
    pub fn sfw_flushed_doc_count(&self) -> i32 {
        self.sfw.as_ref().map_or(0, |s| s.flushed_doc_count())
    }

    /// Raw stored-field bytes of the n-th unflushed buffered document.
    /// Returns None if no SFW exists or n is out of range.
    pub fn sfw_buffered_doc_bytes(&self, n: u32) -> Option<&[u8]> {
        self.sfw.as_ref().and_then(|s| s.buffered_doc_bytes(n))
    }

    /// Approximate RAM held by the indexing buffers (postings/docvalues/
    /// points arenas) plus one stored-fields compression chunk cushion.
    pub fn ram_bytes(&self) -> usize {
        self.dw.ram_bytes() + (1 << 17)
    }

    pub fn add_document(&mut self, schema: &Schema, doc: Document) -> io::Result<()> {
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
        } = self;
        let max_doc = dw.max_doc as i32;

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

        // 2. Stored fields: .fdt data already streamed; finish writes
        //    the last chunk plus .fdx/.fdm.
        let sfw = sfw.expect("sfw is created on first add_document");
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
                    _ => unreachable!("only Numeric/Sorted DV are supported"),
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
}
