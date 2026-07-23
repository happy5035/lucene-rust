//! Block-tree terms dictionary reader (Lucene90BlockTreeTermsReader +
//! SegmentTermsEnum/Frame seek path, Lucene 9.12.3).
//!
//! Reads `_{segment}_Lucene912_0.{tim,tip,tmd}` written by
//! [`crate::postings::PostingsWriter`]. Implements seekExact + sequential
//! enumeration (TermsIter); the prefix/wildcard queries of search spec
//! phase 7 build on the enumerator.

use std::cmp::Ordering;
use std::io;

use crate::codec_util::{check_footer, check_footer_structure, check_index_header, corrupt};
use crate::directory::FSDirectory;
use crate::field_infos::{FieldInfo, FieldInfos, IndexOptions};
use crate::fst::{FstMetadata, FstReader};
use crate::io::{DataInput, IndexInput};
use crate::postings::{
    BLOCKTREE_VERSION, OUTPUT_FLAG_HAS_TERMS, OUTPUT_FLAG_IS_FLOOR, POSTINGS_VERSION,
    SEGMENT_SUFFIX, TERMS_CODEC, TIM_CODEC, TIP_CODEC, TMD_CODEC, file_name,
};
use crate::postings_ll::{BLOCK_SIZE, read_msb_vlong};

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
        IndexOptions::DocsAndFreqsAndPositions | IndexOptions::DocsAndFreqsAndPositionsAndOffsets
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
            let field_info = field_infos
                .by_number(field_number)
                .ok_or_else(|| corrupt(format!("invalid field number {field_number}")))?;
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
    pub fn seek_exact(&mut self, field: &FieldInfo, term: &[u8]) -> io::Result<Option<TermEntry>> {
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
                    last_state.last_pos_block_offset = if total_term_freq > BLOCK_SIZE as u64 {
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
                        return Err(corrupt("block-tree scan hit an exact sub-block match"));
                    }
                    return Ok(last_entry);
                }
            }
        }
        Ok(None) // SeekStatus.END → NOT_FOUND for exact seek
    }

    /// SegmentTermsEnum over one field (search spec M2 §3): sequential
    /// enumeration + seek_ceil, touching only the terms dict (.tim/.tip),
    /// never postings. Terms arrive in dictionary (byte) order.
    pub fn terms_iter(&mut self, field: &FieldInfo) -> TermsIter<'_> {
        TermsIter::new(self, field)
    }
}

// ===========================================================================
// TermsIter: sequential block-tree enumerator (SegmentTermsEnum next/seekCeil)
// ===========================================================================

/// One block-tree frame (SegmentTermsEnumFrame): prefix length, current block
/// fp (+ fp_end chaining of floor siblings) and the decoded block blobs.
/// stats/meta are decoded incrementally as the cursor advances — the same
/// decoding steps as [`TermsDict::scan_block`], kept across calls.
struct IterFrame {
    prefix_len: usize,
    fp: u64,      // fp of the block to (re)load
    fp_orig: u64, // fp of the group's first block (scanToSubBlock anchor on pop)
    fp_end: u64,  // end of the loaded block = next floor sibling's fp
    loaded: bool,
    ent_count: usize,
    is_last_in_floor: bool,
    is_leaf: bool,
    suffix_bytes: Vec<u8>,
    suffix_lengths: IndexInput,
    stats: IndexInput,
    meta: IndexInput,
    suffix_pos: usize,
    next_ent: usize,
    singleton_run: u32,
    last_state: TermState,
}

impl IterFrame {
    fn new(prefix_len: usize, fp: u64) -> IterFrame {
        IterFrame {
            prefix_len,
            fp,
            fp_orig: fp,
            fp_end: 0,
            loaded: false,
            ent_count: 0,
            is_last_in_floor: true,
            is_leaf: true,
            suffix_bytes: Vec::new(),
            suffix_lengths: IndexInput::in_memory(Vec::new()),
            stats: IndexInput::in_memory(Vec::new()),
            meta: IndexInput::in_memory(Vec::new()),
            suffix_pos: 0,
            next_ent: 0,
            singleton_run: 0,
            // EMPTY_STATE (Lucene912PostingsWriter.java:425-457)
            last_state: TermState {
                doc_start_fp: 0,
                pos_start_fp: 0,
                last_pos_block_offset: -1,
                singleton_doc_id: -1,
            },
        }
    }
}

/// One decoded block entry (SegmentTermsEnumFrame.next :291-298).
enum NextEntry {
    Term(TermEntry),
    SubBlock(u64),
}

/// SegmentTermsEnumFrame.loadBlock (:145-240) into an IterFrame; fp_end
/// chains floor siblings ("Sub-blocks of a single floor block are always
/// written one after another", :231-234; writer postings.rs write_block).
fn load_frame_block(tim_in: &mut IndexInput, frame: &mut IterFrame) -> io::Result<()> {
    tim_in.seek(frame.fp)?;
    let code = tim_in.read_vint()?;
    frame.ent_count = (code >> 1) as usize;
    frame.is_last_in_floor = code & 1 != 0;
    let code_l = tim_in.read_vlong()? as u64;
    frame.is_leaf = code_l & 0x04 != 0;
    let num_suffix_bytes = (code_l >> 3) as usize;
    let compression = code_l & 0x03;
    if compression != 0 {
        return Err(corrupt(format!(
            "unsupported suffix compression {compression} (writer emits NO_COMPRESSION)"
        )));
    }
    frame.suffix_bytes = vec![0u8; num_suffix_bytes];
    tim_in.read_bytes(&mut frame.suffix_bytes)?;
    let mut num_sl_bytes = tim_in.read_vint()? as usize;
    let all_equal = num_sl_bytes & 1 != 0;
    num_sl_bytes >>= 1;
    let mut sl_bytes = vec![0u8; num_sl_bytes];
    if all_equal {
        let b = tim_in.read_byte()?;
        sl_bytes.fill(b);
    } else {
        tim_in.read_bytes(&mut sl_bytes)?;
    }
    let num_stat_bytes = tim_in.read_vint()? as usize;
    let mut stat_bytes = vec![0u8; num_stat_bytes];
    tim_in.read_bytes(&mut stat_bytes)?;
    let num_meta_bytes = tim_in.read_vint()? as usize;
    let mut meta_bytes = vec![0u8; num_meta_bytes];
    tim_in.read_bytes(&mut meta_bytes)?;
    frame.suffix_lengths = IndexInput::in_memory(sl_bytes);
    frame.stats = IndexInput::in_memory(stat_bytes);
    frame.meta = IndexInput::in_memory(meta_bytes);
    frame.suffix_pos = 0;
    frame.next_ent = 0;
    frame.singleton_run = 0;
    frame.last_state = TermState {
        doc_start_fp: 0,
        pos_start_fp: 0,
        last_pos_block_offset: -1,
        singleton_doc_id: -1,
    };
    frame.fp_end = tim_in.file_pointer();
    frame.loaded = true;
    Ok(())
}

/// Reads the next entry of a loaded block: suffix (offset, len) + decoded
/// payload. nextLeaf :300-312 / nextNonLeaf :314-356 for the entry shape,
/// decodeMetaData :433-481 for stats, decodeTerm :235-277 for metadata.
fn next_frame_entry(
    frame: &mut IterFrame,
    has_freqs: bool,
    has_positions: bool,
) -> io::Result<Option<(usize, usize, NextEntry)>> {
    if frame.next_ent == frame.ent_count {
        return Ok(None);
    }
    frame.next_ent += 1;
    let (suffix_len, is_sub_block) = if frame.is_leaf {
        (frame.suffix_lengths.read_vint()? as usize, false)
    } else {
        let c = frame.suffix_lengths.read_vint()?;
        ((c >> 1) as usize, c & 1 != 0)
    };
    let off = frame.suffix_pos;
    frame.suffix_pos += suffix_len;
    if is_sub_block {
        // back-pointer lives in the suffixLengths stream (:348-349)
        let sub_fp = frame.fp - frame.suffix_lengths.read_vlong()? as u64;
        return Ok(Some((off, suffix_len, NextEntry::SubBlock(sub_fp))));
    }
    // stats (decodeMetaData :433-481)
    let (doc_freq, total_term_freq) = if frame.singleton_run > 0 {
        frame.singleton_run -= 1;
        (1u32, 1u64)
    } else {
        let token = frame.stats.read_vint()?;
        if token & 1 != 0 {
            frame.singleton_run = (token >> 1) as u32;
            (1u32, 1u64)
        } else {
            let df = (token >> 1) as u32;
            let ttf = if has_freqs {
                df as u64 + frame.stats.read_vlong()? as u64
            } else {
                df as u64
            };
            (df, ttf)
        }
    };
    // metadata (Lucene912PostingsReader.decodeTerm :235-277)
    let l = frame.meta.read_vlong()? as u64;
    if l & 1 == 0 {
        frame.last_state.doc_start_fp += l >> 1;
        frame.last_state.singleton_doc_id = if doc_freq == 1 {
            frame.meta.read_vint()? as i64
        } else {
            -1
        };
    } else {
        let delta = zigzag_decode(l >> 1);
        frame.last_state.singleton_doc_id += delta;
    }
    if has_positions {
        frame.last_state.pos_start_fp += frame.meta.read_vlong()? as u64;
        frame.last_state.last_pos_block_offset = if total_term_freq > BLOCK_SIZE as u64 {
            frame.meta.read_vlong()?
        } else {
            -1
        };
    }
    Ok(Some((
        off,
        suffix_len,
        NextEntry::Term(TermEntry {
            doc_freq,
            total_term_freq,
            state: frame.last_state,
        }),
    )))
}

/// pushFrame (:245-259) + scanToFloorFrame (:361-431) on one FST output:
/// returns `(group_anchor_fp, floor_adjusted_fp)` — the anchor is the FST
/// output's raw fp (what the parent's sub-block entry points at, kept as
/// the frame's `fp_orig` pop anchor), the adjusted fp is the block to scan
/// (Frame.fp vs Frame.fpOrig in Lucene). Same logic as the inline descent
/// in [`TermsDict::seek_exact`], factored for `seek_ceil`.
fn fst_output_block_fp(output: &[u8], term: &[u8], depth: usize) -> io::Result<(u64, u64)> {
    let mut out_in = IndexInput::in_memory(output.to_vec());
    let code = read_msb_vlong(&mut out_in)?;
    let fp_anchor = code >> 2;
    let mut fp = fp_anchor;
    let is_floor = code & OUTPUT_FLAG_IS_FLOOR != 0;
    if is_floor && depth < term.len() {
        let target_label = term[depth];
        let num_follow = out_in.read_vint()? as u32;
        let mut next_label = out_in.read_byte()?;
        if target_label >= next_label {
            for i in 0..num_follow {
                let sub_code = out_in.read_vlong()? as u64;
                fp = fp_anchor + (sub_code >> 1);
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
    Ok((fp_anchor, fp))
}

/// Reloads a parent frame after a pop and walks its floor chain to the
/// sub-block entry pointing at `child_fp_orig`, leaving the cursor just past
/// it (SegmentTermsEnum.next :1005-1010: scanToFloorFrame + loadBlock +
/// scanToSubBlock :497-525; the walk is linear because the writer lays a
/// group's blocks out consecutively — Lucene90BlockTreeTermsWriter
/// .writeBlocks :661-783).
fn reload_and_scan_to_sub(
    tim_in: &mut IndexInput,
    frame: &mut IterFrame,
    child_fp_orig: u64,
    has_freqs: bool,
    has_positions: bool,
) -> io::Result<()> {
    frame.fp = frame.fp_orig;
    loop {
        load_frame_block(tim_in, frame)?;
        while let Some((_off, _len, entry)) = next_frame_entry(frame, has_freqs, has_positions)? {
            if let NextEntry::SubBlock(fp) = entry {
                if fp == child_fp_orig {
                    return Ok(());
                }
            }
        }
        if frame.is_last_in_floor {
            return Err(corrupt(
                "sub-block entry not found while popping the block-tree frame",
            ));
        }
        frame.fp = frame.fp_end;
    }
}

/// Sequential block-tree terms enumerator (search spec M2 §3;
/// SegmentTermsEnum.next :960-1051 / seekCeil :581-837). Take semantics:
/// after construction the first `next()` yields the dictionary's first
/// term; after `seek_ceil(t)` the first `next()` yields the first term
/// >= t. Only the terms dict is touched — postings are never read.
pub struct TermsIter<'a> {
    dict: &'a mut TermsDict,
    field_index: Option<usize>,
    has_freqs: bool,
    has_positions: bool,
    frames: Vec<IterFrame>,
    term: Vec<u8>,
    pending: Option<(Vec<u8>, TermEntry)>,
    started: bool,
    done: bool,
}

impl<'a> TermsIter<'a> {
    fn new(dict: &'a mut TermsDict, field: &FieldInfo) -> TermsIter<'a> {
        let field_index = dict
            .fields
            .iter()
            .position(|f| f.field_number == field.number);
        TermsIter {
            has_freqs: field.index_options != IndexOptions::Docs,
            has_positions: field_has_positions(field),
            field_index,
            dict,
            frames: Vec::new(),
            term: Vec::new(),
            pending: None,
            started: false,
            done: field_index.is_none(),
        }
    }

    /// SegmentTermsEnum.seekCeil (:581-837): FST descent + floor navigation
    /// + block scan with exactOnly=false. Positions the iterator so the next
    /// [`TermsIter::next`] yields the first term >= `target`. Returns true
    /// on an exact hit (SeekStatus.FOUND vs NOT_FOUND/END).
    pub fn seek_ceil(&mut self, target: &[u8]) -> io::Result<bool> {
        self.frames.clear();
        self.term.clear();
        self.pending = None;
        self.started = true;
        self.done = false;
        let Some(field_index) = self.field_index else {
            self.done = true;
            return Ok(false);
        };
        {
            let meta = &self.dict.fields[field_index];
            if target < meta.min_term.as_slice() {
                // below the dictionary: enumerate from the first term
                self.init_root()?;
                return Ok(false);
            }
            if target > meta.max_term.as_slice() {
                self.done = true;
                return Ok(false);
            }
        }
        // FST descent (same as seek_exact): (depth, output) candidates,
        // deepest last.
        let mut outs: Vec<(usize, Vec<u8>)> =
            vec![(0, self.dict.fields[field_index].root_code.clone())];
        {
            let traced = self.dict.fst(field_index)?.trace_path(target)?;
            outs.extend(traced);
        }
        let depth = outs.last().unwrap().0;
        let (fp_anchor, fp) = fst_output_block_fp(&outs.last().unwrap().1, target, depth)?;
        // Ancestor frames stay unloaded (reloaded lazily on pop); the
        // deepest frame is floor-adjusted and loaded for the scan. Its
        // fp_orig stays the group anchor (Frame.fpOrig) so that popping
        // back finds the parent's sub-block entry.
        for (d, o) in &outs[..outs.len() - 1] {
            let mut oi = IndexInput::in_memory(o.clone());
            let code = read_msb_vlong(&mut oi)?;
            self.frames.push(IterFrame::new(*d, code >> 2));
        }
        let mut deepest = IterFrame::new(depth, fp);
        deepest.fp_orig = fp_anchor;
        self.frames.push(deepest);
        self.term = target[..depth].to_vec();
        let fi = self.frames.len() - 1;
        load_frame_block(&mut self.dict.tim_in, &mut self.frames[fi])?;
        self.scan_for_ceil(target)
    }

    /// Take the positioned term and advance (SegmentTermsEnum.next
    /// :960-1051).
    pub fn next(&mut self) -> io::Result<Option<(Vec<u8>, TermEntry)>> {
        if self.done {
            return Ok(None);
        }
        if !self.started {
            self.started = true;
            self.init_root()?;
        }
        let out = self.pending.take();
        if out.is_some() {
            self.advance()?;
        }
        Ok(out)
    }

    /// Loads the root block and positions on the first term.
    fn init_root(&mut self) -> io::Result<()> {
        let Some(field_index) = self.field_index else {
            self.done = true;
            return Ok(());
        };
        let root_code = self.dict.fields[field_index].root_code.clone();
        let mut oi = IndexInput::in_memory(root_code);
        let code = read_msb_vlong(&mut oi)?;
        self.frames.clear();
        self.term.clear();
        self.frames.push(IterFrame::new(0, code >> 2));
        let fi = self.frames.len() - 1;
        load_frame_block(&mut self.dict.tim_in, &mut self.frames[fi])?;
        self.advance()
    }

    /// scanToTerm with exactOnly=false (scanToTermLeaf :547-660,
    /// scanToTermNonLeaf :732-830): positions `pending` on the first
    /// term >= target. When the landing block is exhausted, the ceiling is
    /// the next term in dictionary order — `advance` finds it.
    fn scan_for_ceil(&mut self, target: &[u8]) -> io::Result<bool> {
        loop {
            let fi = self.frames.len() - 1;
            let prefix_len = self.frames[fi].prefix_len;
            let mut descended = false;
            while let Some((off, len, entry)) =
                next_frame_entry(&mut self.frames[fi], self.has_freqs, self.has_positions)?
            {
                let suffix = self.frames[fi].suffix_bytes[off..off + len].to_vec();
                let t = if prefix_len <= target.len() {
                    &target[prefix_len..]
                } else {
                    &[][..]
                };
                match suffix.as_slice().cmp(t) {
                    Ordering::Less => continue,
                    Ordering::Equal => match entry {
                        NextEntry::Term(te) => {
                            self.take_pending(&suffix, te);
                            return Ok(true);
                        }
                        // the FST descent consumes every exact sub-block
                        // prefix (compileIndex :490-578), like scan_block
                        NextEntry::SubBlock(_) => {
                            return Err(corrupt("ceil scan hit an exact sub-block match"));
                        }
                    },
                    Ordering::Greater => match entry {
                        NextEntry::Term(te) => {
                            self.take_pending(&suffix, te);
                            return Ok(false);
                        }
                        // the ceiling is the first term of this sub-block
                        // group (scanToTermNonLeaf :805-813): descend
                        NextEntry::SubBlock(sub_fp) => {
                            self.term.truncate(prefix_len);
                            self.term.extend_from_slice(&suffix);
                            self.frames.push(IterFrame::new(self.term.len(), sub_fp));
                            let ni = self.frames.len() - 1;
                            load_frame_block(&mut self.dict.tim_in, &mut self.frames[ni])?;
                            descended = true;
                            break;
                        }
                    },
                }
            }
            if !descended {
                // landing block exhausted without finding >= target
                self.advance()?;
                return Ok(false);
            }
        }
    }

    /// Records (term, entry) as the positioned (`pending`) term.
    fn take_pending(&mut self, suffix: &[u8], entry: TermEntry) {
        let prefix_len = self.frames.last().unwrap().prefix_len;
        self.term.truncate(prefix_len);
        self.term.extend_from_slice(suffix);
        self.pending = Some((self.term.clone(), entry));
    }

    /// Refills `pending` with the next term in dictionary order
    /// (SegmentTermsEnum.next :960-1051): pops exhausted frames — floor
    /// siblings chain via fp_end (loadNextFloorBlock :126-134) — and pushes
    /// into sub-blocks. Sets `done` at dictionary end.
    fn advance(&mut self) -> io::Result<()> {
        loop {
            // pop exhausted blocks
            loop {
                let Some(frame) = self.frames.last() else {
                    self.done = true;
                    return Ok(());
                };
                if frame.next_ent < frame.ent_count {
                    break;
                }
                if !frame.is_last_in_floor {
                    // floor sibling: blocks of a group are consecutive in .tim
                    let fp = frame.fp_end;
                    self.frames.last_mut().unwrap().fp = fp;
                    let fi = self.frames.len() - 1;
                    load_frame_block(&mut self.dict.tim_in, &mut self.frames[fi])?;
                    continue;
                }
                // pop to the parent frame
                let child_fp_orig = self.frames.last().unwrap().fp_orig;
                self.frames.pop();
                let Some(parent) = self.frames.last_mut() else {
                    self.done = true;
                    return Ok(());
                };
                if !parent.loaded {
                    // seek_ceil-produced ancestor: reload and walk to the
                    // sub-block entry pointing at the child
                    let has_freqs = self.has_freqs;
                    let has_positions = self.has_positions;
                    reload_and_scan_to_sub(
                        &mut self.dict.tim_in,
                        parent,
                        child_fp_orig,
                        has_freqs,
                        has_positions,
                    )?;
                }
                let plen = self.frames.last().unwrap().prefix_len;
                self.term.truncate(plen);
            }
            // consume one entry
            let fi = self.frames.len() - 1;
            let prefix_len = self.frames[fi].prefix_len;
            match next_frame_entry(&mut self.frames[fi], self.has_freqs, self.has_positions)? {
                None => continue, // raced empty; the pop loop above handles it
                Some((off, len, NextEntry::SubBlock(sub_fp))) => {
                    self.term.truncate(prefix_len);
                    let suffix = self.frames[fi].suffix_bytes[off..off + len].to_vec();
                    self.term.extend_from_slice(&suffix);
                    self.frames.push(IterFrame::new(self.term.len(), sub_fp));
                    let ni = self.frames.len() - 1;
                    load_frame_block(&mut self.dict.tim_in, &mut self.frames[ni])?;
                }
                Some((off, len, NextEntry::Term(te))) => {
                    self.term.truncate(prefix_len);
                    let suffix = self.frames[fi].suffix_bytes[off..off + len].to_vec();
                    self.term.extend_from_slice(&suffix);
                    self.pending = Some((self.term.clone(), te));
                    return Ok(());
                }
            }
        }
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

    fn collect_all(dict: &mut TermsDict, field: &FieldInfo) -> Vec<(Vec<u8>, u32)> {
        let mut it = dict.terms_iter(field);
        let mut out = Vec::new();
        while let Some((t, e)) = it.next().unwrap() {
            out.push((t, e.doc_freq));
        }
        out
    }

    #[test]
    fn terms_iter_full_enumeration_round_trip() {
        let root = temp_dir("iterall");
        let dir = FSDirectory::open(&root).unwrap();
        let fis = write_segment(&dir);
        let mut dict = TermsDict::open(&dir, "_0", &[3u8; 16], &fis).unwrap();
        let kw = fis.by_name("kw").unwrap();
        // kw write-side input order is ascending: a, b, t000..t299
        let mut expected: Vec<(Vec<u8>, u32)> = vec![(b"a".to_vec(), 1), (b"b".to_vec(), 3)];
        for i in 0..300 {
            expected.push((format!("t{i:03}").into_bytes(), 2));
        }
        assert_eq!(collect_all(&mut dict, kw), expected);
        // tx: freqs field, stats/ttf decoded per term
        let tx = fis.by_name("tx").unwrap();
        let mut it = dict.terms_iter(tx);
        let (t, e) = it.next().unwrap().expect("hello");
        assert_eq!(t, b"hello");
        assert_eq!(e.doc_freq, 200);
        let expected_ttf: u64 = (0..200).map(|i| (i % 7) + 1).sum::<u32>() as u64;
        assert_eq!(e.total_term_freq, expected_ttf);
        let (t, e) = it.next().unwrap().expect("world");
        assert_eq!(t, b"world");
        assert_eq!(e.doc_freq, 2);
        assert!(it.next().unwrap().is_none());
        // exhaustion is sticky
        assert!(it.next().unwrap().is_none());
        // field without a .tmd record yields an empty iterator
        let ghost = indexed("ghost", 99, IndexOptions::Docs);
        let mut it = dict.terms_iter(&ghost);
        assert!(it.next().unwrap().is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn terms_iter_seek_ceil_positions_on_ceiling() {
        let root = temp_dir("iterceil");
        let dir = FSDirectory::open(&root).unwrap();
        let fis = write_segment(&dir);
        let mut dict = TermsDict::open(&dir, "_0", &[3u8; 16], &fis).unwrap();
        let kw = fis.by_name("kw").unwrap();
        // exact hit: positioned ON the term, iteration continues in order
        let mut it = dict.terms_iter(kw);
        assert!(it.seek_ceil(b"t150").unwrap());
        let (t, e) = it.next().unwrap().expect("t150");
        assert_eq!(t, b"t150");
        assert_eq!(e.doc_freq, 2);
        assert_eq!(it.next().unwrap().unwrap().0, b"t151");
        // miss: ceiling is the next greater term
        let mut it = dict.terms_iter(kw);
        assert!(!it.seek_ceil(b"t150x").unwrap());
        assert_eq!(it.next().unwrap().unwrap().0, b"t151");
        // prefix-of-a-term target: ceiling is the term itself
        let mut it = dict.terms_iter(kw);
        assert!(!it.seek_ceil(b"t15").unwrap());
        assert_eq!(it.next().unwrap().unwrap().0, b"t150");
        // below min -> first term of the dictionary
        let mut it = dict.terms_iter(kw);
        assert!(!it.seek_ceil(b"0").unwrap());
        assert_eq!(it.next().unwrap().unwrap().0, b"a");
        // above max -> exhausted
        let mut it = dict.terms_iter(kw);
        assert!(!it.seek_ceil(b"zzz").unwrap());
        assert!(it.next().unwrap().is_none());
        // exact on the first / last term
        let mut it = dict.terms_iter(kw);
        assert!(it.seek_ceil(b"a").unwrap());
        assert_eq!(it.next().unwrap().unwrap().0, b"a");
        let mut it = dict.terms_iter(kw);
        assert!(it.seek_ceil(b"t299").unwrap());
        assert_eq!(it.next().unwrap().unwrap().0, b"t299");
        assert!(it.next().unwrap().is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn terms_iter_seek_ceil_then_enumerate_to_end() {
        // popping seek-created (unloaded) ancestor frames across internal
        // sub-blocks and floor siblings: t150 -> t299 is 150 terms. The
        // seek on "t150" floor-adjusts the landing frame into a sibling
        // block, so the pop back to the "t1"/"t" parents exercises the
        // fpOrig group-anchor reload path (Frame.fpOrig vs fp).
        let root = temp_dir("iterpop");
        let dir = FSDirectory::open(&root).unwrap();
        let fis = write_segment(&dir);
        let mut dict = TermsDict::open(&dir, "_0", &[3u8; 16], &fis).unwrap();
        let kw = fis.by_name("kw").unwrap();
        let mut it = dict.terms_iter(kw);
        it.seek_ceil(b"t150").unwrap();
        let mut got = Vec::new();
        while let Some((t, _)) = it.next().unwrap() {
            got.push(t);
        }
        assert_eq!(got.len(), 150);
        for (i, t) in got.iter().enumerate() {
            assert_eq!(t, &format!("t{:03}", 150 + i).into_bytes(), "position {i}");
        }
        // block-prefix target lands on the subtree's first term
        let mut it = dict.terms_iter(kw);
        assert!(!it.seek_ceil(b"t").unwrap());
        assert_eq!(it.next().unwrap().unwrap().0, b"t000");
        // t099..t299 = 201 terms
        let mut it = dict.terms_iter(kw);
        it.seek_ceil(b"t099").unwrap();
        let mut n = 0;
        while it.next().unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, 201);
        fs::remove_dir_all(&root).unwrap();
    }
}
