//! Block-tree terms dictionary reader (Lucene90BlockTreeTermsReader +
//! SegmentTermsEnum/Frame seek path, Lucene 9.12.3).
//!
//! Reads `_{segment}_Lucene912_0.{tim,tip,tmd}` written by
//! [`crate::postings::PostingsWriter`]. Only the seekExact path is
//! implemented (TermQuery); sequential term enumeration arrives with
//! prefix/wildcard queries (search spec phase 7).

use std::cmp::Ordering;
use std::io;

use crate::codec_util::{check_footer, check_footer_structure, check_index_header, corrupt};
use crate::directory::FSDirectory;
use crate::field_infos::{FieldInfo, FieldInfos, IndexOptions};
use crate::fst::{FstMetadata, FstReader};
use crate::io::{DataInput, IndexInput};
use crate::postings::{
    file_name, BLOCKTREE_VERSION, OUTPUT_FLAG_HAS_TERMS, OUTPUT_FLAG_IS_FLOOR, POSTINGS_VERSION,
    SEGMENT_SUFFIX, TERMS_CODEC, TIM_CODEC, TIP_CODEC, TMD_CODEC,
};
use crate::postings_ll::{read_msb_vlong, BLOCK_SIZE};

/// IntBlockTermState (Lucene912PostingsFormat.java:425-491): a term's
/// postings entry points, decoded from the .tim metadata blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermState {
    pub doc_start_fp: u64,
    pub pos_start_fp: u64,
    pub last_pos_block_offset: i64,
    pub singleton_doc_id: i64,
}

/// A found term: stats + postings entry.
#[derive(Clone, Copy, Debug)]
pub struct TermEntry {
    pub doc_freq: u32,
    pub total_term_freq: u64,
    pub state: TermState,
}

/// Per-field record from .tmd (Lucene90BlockTreeTermsReader constructor
/// :180-242).
pub struct FieldTermsMeta {
    pub field_number: i32,
    pub num_terms: u64,
    pub root_code: Vec<u8>,
    pub sum_total_term_freq: u64,
    pub sum_doc_freq: u64,
    pub doc_count: i32,
    pub min_term: Vec<u8>,
    pub max_term: Vec<u8>,
    pub index_start_fp: u64,
    fst_metadata: FstMetadata,
}

/// Block-tree terms dictionary of one segment (.tim + .tip + .tmd).
/// Field FSTs load lazily on first seek (spec §3 惰性加载).
pub struct TermsDict {
    tim_in: IndexInput,
    tip_in: IndexInput,
    fields: Vec<FieldTermsMeta>,
    fsts: Vec<Option<FstReader>>,
}

/// Lucene90BlockTreeTermsReader.readBytesRef (:271-282).
fn read_bytes_ref(input: &mut impl DataInput) -> io::Result<Vec<u8>> {
    let len = input.read_vint()? as usize;
    let mut b = vec![0u8; len];
    input.read_bytes(&mut b)?;
    Ok(b)
}

/// BitUtil.zigZagDecode (BitUtil.java:299).
fn zigzag_decode(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn field_has_positions(field: &FieldInfo) -> bool {
    matches!(
        field.index_options,
        IndexOptions::DocsAndFreqsAndPositions
            | IndexOptions::DocsAndFreqsAndPositionsAndOffsets
    )
}

impl TermsDict {
    /// Opens .tmd/.tip/.tim and parses every field record
    /// (Lucene90BlockTreeTermsReader.<init> :126-269). The postings header
    /// lives inside .tmd (Lucene912PostingsReader.init :188-206).
    pub fn open(
        dir: &FSDirectory,
        segment: &str,
        segment_id: &[u8; 16],
        field_infos: &FieldInfos,
    ) -> io::Result<TermsDict> {
        // .tmd: field records + lengths + footer (streaming CRC).
        let mut tmd = dir.open_checksum_input(&file_name(segment, "tmd"))?;
        check_index_header(
            &mut tmd,
            TMD_CODEC,
            BLOCKTREE_VERSION,
            BLOCKTREE_VERSION,
            segment_id,
            SEGMENT_SUFFIX,
        )?;
        check_index_header(
            &mut tmd,
            TERMS_CODEC,
            POSTINGS_VERSION,
            POSTINGS_VERSION,
            segment_id,
            SEGMENT_SUFFIX,
        )?;
        let block_size = tmd.read_vint()?;
        if block_size != BLOCK_SIZE as i32 {
            return Err(corrupt(format!(
                "expected postings blockSize {BLOCK_SIZE}, found {block_size}"
            )));
        }
        let num_fields = tmd.read_vint()?;
        if num_fields < 0 {
            return Err(corrupt(format!("invalid numFields {num_fields}")));
        }
        let mut fields = Vec::with_capacity(num_fields as usize);
        for _ in 0..num_fields {
            let field_number = tmd.read_vint()?;
            let num_terms = tmd.read_vlong()? as u64;
            if num_terms == 0 {
                return Err(corrupt(format!(
                    "illegal numTerms 0 for field number {field_number}"
                )));
            }
            let root_code = read_bytes_ref(&mut tmd)?;
            let field_info = field_infos.by_number(field_number).ok_or_else(|| {
                corrupt(format!("invalid field number {field_number}"))
            })?;
            let sum_total_term_freq = tmd.read_vlong()? as u64;
            // :195-198 — DOCS fields store a single value
            // (sumDocFreq == sumTotalTermFreq).
            let sum_doc_freq = if field_info.index_options == IndexOptions::Docs {
                sum_total_term_freq
            } else {
                tmd.read_vlong()? as u64
            };
            let doc_count = tmd.read_vint()?;
            let min_term = read_bytes_ref(&mut tmd)?;
            let max_term = read_bytes_ref(&mut tmd)?;
            let index_start_fp = tmd.read_vlong()? as u64;
            let fst_metadata = FstMetadata::read(&mut tmd)?;
            fields.push(FieldTermsMeta {
                field_number,
                num_terms,
                root_code,
                sum_total_term_freq,
                sum_doc_freq,
                doc_count,
                min_term,
                max_term,
                index_start_fp,
                fst_metadata,
            });
        }
        let index_length = tmd.read_long()? as u64; // :243 (.tip)
        let terms_length = tmd.read_long()? as u64; // :244 (.tim)
        check_footer(&mut tmd)?; // :249

        let mut tip_in = dir.open_input(&file_name(segment, "tip"))?;
        check_index_header(
            &mut tip_in,
            TIP_CODEC,
            BLOCKTREE_VERSION,
            BLOCKTREE_VERSION,
            segment_id,
            SEGMENT_SUFFIX,
        )?;
        // retrieveChecksum (:328-336): length + trailing footer structure
        check_footer_structure(&tip_in, index_length)?;
        let mut tim_in = dir.open_input(&file_name(segment, "tim"))?;
        check_index_header(
            &mut tim_in,
            TIM_CODEC,
            BLOCKTREE_VERSION,
            BLOCKTREE_VERSION,
            segment_id,
            SEGMENT_SUFFIX,
        )?;
        check_footer_structure(&tim_in, terms_length)?;
        let fsts = fields.iter().map(|_| None).collect();
        Ok(TermsDict {
            tim_in,
            tip_in,
            fields,
            fsts,
        })
    }

    pub fn field_meta(&self, field_number: i32) -> Option<&FieldTermsMeta> {
        self.fields.iter().find(|f| f.field_number == field_number)
    }

    /// Loads the field's FST from .tip on first use
    /// (OffHeapFSTStore :39-61 loads the [indexStartFP, +numBytes) image).
    fn fst(&mut self, field_index: usize) -> io::Result<&FstReader> {
        if self.fsts[field_index].is_none() {
            let (index_start_fp, fst_metadata) = {
                let m = &self.fields[field_index];
                (m.index_start_fp, m.fst_metadata.clone())
            };
            let mut bytes = vec![0u8; fst_metadata.num_bytes as usize];
            self.tip_in.seek(index_start_fp)?;
            self.tip_in.read_bytes(&mut bytes)?;
            self.fsts[field_index] = Some(FstReader::new(bytes, &fst_metadata));
        }
        Ok(self.fsts[field_index].as_ref().unwrap())
    }

    /// SegmentTermsEnum.seekExact (:311-578), fresh-descent-only
    /// simplification: min/max pruning (:317-319), FST descent collecting
    /// candidate frames (:477-545), floor navigation
    /// (SegmentTermsEnumFrame.scanToFloorFrame :361-431), block load
    /// (:145-240) and linear entry scan (:547-660,:732-830).
    pub fn seek_exact(
        &mut self,
        field: &FieldInfo,
        term: &[u8],
    ) -> io::Result<Option<TermEntry>> {
        let Some(field_index) = self
            .fields
            .iter()
            .position(|f| f.field_number == field.number)
        else {
            return Ok(None); // field without terms: no .tmd record
        };
        {
            let meta = &self.fields[field_index];
            if term < meta.min_term.as_slice() || term > meta.max_term.as_slice() {
                return Ok(None);
            }
        }
        // FST descent: (depth, output) candidates, deepest last.
        let mut frames: Vec<(usize, Vec<u8>)> =
            vec![(0, self.fields[field_index].root_code.clone())];
        {
            let traced = self.fst(field_index)?.trace_path(term)?;
            frames.extend(traced);
        }
        let (depth, output) = frames.last().unwrap();
        let depth = *depth;
        // pushFrame (:245-259): fp + flags from the output's leading
        // MSB-VLong; OUTPUT_FLAGS_NUM_BITS = 2 (Reader :72).
        let mut out_in = IndexInput::in_memory(output.clone());
        let code = read_msb_vlong(&mut out_in)?;
        let mut fp = code >> 2;
        let is_floor = code & OUTPUT_FLAG_IS_FLOOR != 0;
        let _has_terms = code & OUTPUT_FLAG_HAS_TERMS != 0;
        // scanToFloorFrame (Frame :361-431): pick the last floor sub-block
        // whose lead label <= the target byte at the frame's prefix length.
        if is_floor && depth < term.len() {
            let target_label = term[depth];
            let num_follow = out_in.read_vint()? as u32;
            let mut next_label = out_in.read_byte()?;
            if target_label >= next_label {
                let fp_orig = fp;
                for i in 0..num_follow {
                    let sub_code = out_in.read_vlong()? as u64;
                    fp = fp_orig + (sub_code >> 1);
                    if i + 1 == num_follow {
                        break;
                    }
                    next_label = out_in.read_byte()?;
                    if target_label < next_label {
                        break;
                    }
                }
            }
        }
        self.scan_block(fp, depth, term, field)
    }

    /// loadBlock (SegmentTermsEnumFrame :145-240) + scanToTermLeaf/NonLeaf
    /// (:547-660,:732-830) for exactOnly=true: linear entry scan with
    /// incremental stats/meta decode (:433-481 + decodeTerm :235-277).
    /// Sub-block descent during scan is unnecessary for exact seek — every
    /// sub-block entry is itself an FST input (Lucene90BlockTreeTermsWriter
    /// .compileIndex :490-578), so the FST descent already landed on the
    /// deepest candidate block.
    fn scan_block(
        &mut self,
        fp: u64,
        prefix_len: usize,
        term: &[u8],
        field: &FieldInfo,
    ) -> io::Result<Option<TermEntry>> {
        let has_freqs = field.index_options != IndexOptions::Docs;
        let has_positions = field_has_positions(field);

        self.tim_in.seek(fp)?;
        let code = self.tim_in.read_vint()?;
        let ent_count = (code >> 1) as usize;
        let code_l = self.tim_in.read_vlong()? as u64;
        let is_leaf = code_l & 0x04 != 0;
        let num_suffix_bytes = (code_l >> 3) as usize;
        let compression = code_l & 0x03;
        if compression != 0 {
            return Err(corrupt(format!(
                "unsupported suffix compression {compression} (writer emits NO_COMPRESSION)"
            )));
        }
        let mut suffix_bytes = vec![0u8; num_suffix_bytes];
        self.tim_in.read_bytes(&mut suffix_bytes)?;
        // suffix lengths blob, with the all-equal-bytes trick (writer :1026-1037)
        let mut num_sl_bytes = self.tim_in.read_vint()? as usize;
        let all_equal = num_sl_bytes & 1 != 0;
        num_sl_bytes >>= 1;
        let mut sl_bytes = vec![0u8; num_sl_bytes];
        if all_equal {
            let b = self.tim_in.read_byte()?;
            sl_bytes.fill(b);
        } else {
            self.tim_in.read_bytes(&mut sl_bytes)?;
        }
        let num_stat_bytes = self.tim_in.read_vint()? as usize;
        let mut stat_bytes = vec![0u8; num_stat_bytes];
        self.tim_in.read_bytes(&mut stat_bytes)?;
        let num_meta_bytes = self.tim_in.read_vint()? as usize;
        let mut meta_bytes = vec![0u8; num_meta_bytes];
        self.tim_in.read_bytes(&mut meta_bytes)?;

        let mut suffix_lengths = IndexInput::in_memory(sl_bytes);
        let mut stats = IndexInput::in_memory(stat_bytes);
        let mut meta = IndexInput::in_memory(meta_bytes);
        let mut suffix_pos = 0usize;
        let mut singleton_run: u32 = 0;
        // EMPTY_STATE (Lucene912PostingsWriter.java:425-457): fps 0, singleton -1
        let mut last_state = TermState {
            doc_start_fp: 0,
            pos_start_fp: 0,
            last_pos_block_offset: -1,
            singleton_doc_id: -1,
        };
        let mut last_entry: Option<TermEntry> = None;

        for _ in 0..ent_count {
            let (suffix_len, is_sub_block) = if is_leaf {
                (suffix_lengths.read_vint()? as usize, false) // nextLeaf :300-312
            } else {
                let c = suffix_lengths.read_vint()?; // nextNonLeaf :333-339
                ((c >> 1) as usize, c & 1 != 0)
            };
            let suffix = &suffix_bytes[suffix_pos..suffix_pos + suffix_len];
            suffix_pos += suffix_len;
            if is_sub_block {
                // back-pointer lives in the suffixLengths stream (:348-349);
                // never followed for exact seek (see fn doc).
                let _sub_fp = fp - suffix_lengths.read_vlong()? as u64;
            } else {
                // stats (decodeMetaData :433-481)
                let (doc_freq, total_term_freq) = if singleton_run > 0 {
                    singleton_run -= 1;
                    (1u32, 1u64)
                } else {
                    let token = stats.read_vint()?;
                    if token & 1 != 0 {
                        singleton_run = (token >> 1) as u32;
                        (1u32, 1u64)
                    } else {
                        let df = (token >> 1) as u32;
                        let ttf = if has_freqs {
                            df as u64 + stats.read_vlong()? as u64
                        } else {
                            df as u64
                        };
                        (df, ttf)
                    }
                };
                // metadata (Lucene912PostingsReader.decodeTerm :235-277)
                let l = meta.read_vlong()? as u64;
                if l & 1 == 0 {
                    last_state.doc_start_fp += l >> 1;
                    last_state.singleton_doc_id = if doc_freq == 1 {
                        meta.read_vint()? as i64
                    } else {
                        -1
                    };
                } else {
                    let delta = zigzag_decode(l >> 1);
                    last_state.singleton_doc_id += delta;
                }
                if has_positions {
                    last_state.pos_start_fp += meta.read_vlong()? as u64;
                    last_state.last_pos_block_offset =
                        if total_term_freq > BLOCK_SIZE as u64 {
                            meta.read_vlong()?
                        } else {
                            -1
                        };
                }
                last_entry = Some(TermEntry {
                    doc_freq,
                    total_term_freq,
                    state: last_state,
                });
            }
            match suffix.cmp(&term[prefix_len..]) {
                Ordering::Less => continue,
                Ordering::Greater => return Ok(None),
                Ordering::Equal => {
                    if is_sub_block {
                        // assert termExists in Frame :790-793 — the FST
                        // descent should have consumed this prefix.
                        return Err(corrupt(
                            "block-tree scan hit an exact sub-block match",
                        ));
                    }
                    return Ok(last_entry);
                }
            }
        }
        Ok(None) // SeekStatus.END → NOT_FOUND for exact seek
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::FSDirectory;
    use crate::field_infos::{FieldInfo, FieldInfos, IndexOptions};
    use crate::postings::PostingsWriter;
    use std::fs;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codec-lucene9-terms-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn indexed(name: &str, number: i32, opts: IndexOptions) -> FieldInfo {
        FieldInfo {
            name: name.to_string(),
            number,
            omit_norms: true,
            index_options: opts,
            ..FieldInfo::stored(name, number)
        }
    }

    /// kw (DOCS): "a" df=1 singleton doc 5; "b" df=3; plus t000..t299 df=2
    /// tx (DOCS_AND_FREQS): "hello" df=200 varied freqs; "world" df=2
    fn write_segment(dir: &FSDirectory) -> FieldInfos {
        let id = [3u8; 16];
        let kw = indexed("kw", 0, IndexOptions::Docs);
        let tx = indexed("tx", 1, IndexOptions::DocsAndFreqs);
        let mut w = PostingsWriter::new(dir, "_0", &id).unwrap();
        w.start_field(&kw, 400).unwrap();
        w.write_term(b"a", &[5], &[1], None).unwrap();
        w.write_term(b"b", &[1, 4, 9], &[1, 1, 1], None).unwrap();
        for i in 0..300 {
            let t = format!("t{i:03}");
            w.write_term(t.as_bytes(), &[1, 2], &[1, 1], None).unwrap();
        }
        w.finish_field().unwrap();
        w.start_field(&tx, 400).unwrap();
        let docs: Vec<u32> = (0..200).map(|i| i * 2).collect();
        let freqs: Vec<u32> = (0..200).map(|i| (i % 7) + 1).collect();
        w.write_term(b"hello", &docs, &freqs, None).unwrap();
        w.write_term(b"world", &[3, 300], &[2, 5], None).unwrap();
        w.finish_field().unwrap();
        w.finish().unwrap();
        let fis = FieldInfos::new(vec![kw, tx]);
        fis.write(dir, "_0", &id, "").unwrap();
        fis
    }

    #[test]
    fn field_metadata_parsed() {
        let root = temp_dir("meta");
        let dir = FSDirectory::open(&root).unwrap();
        let fis = write_segment(&dir);
        let dict = TermsDict::open(&dir, "_0", &[3u8; 16], &fis).unwrap();
        let kw = dict.field_meta(0).expect("kw record");
        assert_eq!(kw.num_terms, 302);
        assert_eq!(kw.min_term, b"a");
        assert_eq!(kw.max_term, b"t299");
        assert_eq!(kw.doc_count, 400);
        let tx = dict.field_meta(1).expect("tx record");
        assert_eq!(tx.num_terms, 2);
        assert_eq!(tx.min_term, b"hello");
        assert_eq!(tx.max_term, b"world");
        assert!(dict.field_meta(2).is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn seek_exact_found_and_singleton() {
        let root = temp_dir("seek");
        let dir = FSDirectory::open(&root).unwrap();
        let fis = write_segment(&dir);
        let mut dict = TermsDict::open(&dir, "_0", &[3u8; 16], &fis).unwrap();
        let kw = fis.by_name("kw").unwrap();
        let tx = fis.by_name("tx").unwrap();

        // singleton: df==1, docID 直接存在 TermState 里
        let e = dict.seek_exact(kw, b"a").unwrap().expect("found a");
        assert_eq!(e.doc_freq, 1);
        assert_eq!(e.state.singleton_doc_id, 5);
        // 普通 term
        let e = dict.seek_exact(kw, b"b").unwrap().expect("found b");
        assert_eq!(e.doc_freq, 3);
        assert_eq!(e.state.singleton_doc_id, -1);
        // freqs 字段的 ttf
        let e = dict.seek_exact(tx, b"hello").unwrap().expect("found hello");
        assert_eq!(e.doc_freq, 200);
        let expected_ttf: u64 = (0..200).map(|i| (i % 7) + 1).sum::<u32>() as u64;
        assert_eq!(e.total_term_freq, expected_ttf);
        // 大字典全部命中（floor / 多层 block 路径）
        for i in 0..300 {
            let t = format!("t{i:03}");
            let e = dict.seek_exact(kw, t.as_bytes()).unwrap().expect("found t");
            assert_eq!(e.doc_freq, 2, "{t}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn seek_exact_not_found() {
        let root = temp_dir("miss");
        let dir = FSDirectory::open(&root).unwrap();
        let fis = write_segment(&dir);
        let mut dict = TermsDict::open(&dir, "_0", &[3u8; 16], &fis).unwrap();
        let kw = fis.by_name("kw").unwrap();
        let tx = fis.by_name("tx").unwrap();
        // min/max 之外
        assert!(dict.seek_exact(kw, b"0").unwrap().is_none());
        assert!(dict.seek_exact(kw, b"zzz").unwrap().is_none());
        // 字典内 but 不存在
        assert!(dict.seek_exact(kw, b"t150x").unwrap().is_none());
        assert!(dict.seek_exact(kw, b"t2").unwrap().is_none());
        // 存在于别的字段不算
        assert!(dict.seek_exact(tx, b"a").unwrap().is_none());
        assert!(dict.seek_exact(tx, b"hellp").unwrap().is_none());
        // 无记录的字段号
        let ghost = indexed("ghost", 99, IndexOptions::Docs);
        assert!(dict.seek_exact(&ghost, b"a").unwrap().is_none());
        fs::remove_dir_all(&root).unwrap();
    }
}
