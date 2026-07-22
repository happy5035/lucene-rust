//! Streaming postings + block-tree terms writer (Lucene912 / Lucene90
//! blocktree, Lucene 9.12.3).
//!
//! Writes `_{segment}_Lucene912_0.{doc,tim,tip,tmd,psm}` (plus `.pos` when any
//! field has positions; never `.pay`). Every layout decision is cited to the
//! 9.12.3 sources; the guiding references are Lucene912PostingsWriter.java and
//! blocktree/Lucene90BlockTreeTermsWriter.java.

use std::io;

use crate::codec_util::{write_footer, write_index_header};
use crate::directory::FSDirectory;
use crate::field_infos::{FieldInfo, IndexOptions};
use crate::fst::FstCompiler;
use crate::io::{ChecksumIndexOutput, IndexOutput};
use crate::postings_ll::{
    for_delta_util_encode, pfor_util_encode, write_group_vints, write_msb_vlong, write_vint15,
    write_vlong15, BLOCK_SIZE,
};

// Lucene912PostingsFormat.java:324-361
const DOC_CODEC: &str = "Lucene912PostingsWriterDoc";
const POS_CODEC: &str = "Lucene912PostingsWriterPos";
const PSM_CODEC: &str = "Lucene912PostingsWriterMeta";
const TERMS_CODEC: &str = "Lucene90PostingsWriterTerms";
const POSTINGS_VERSION: u32 = 0;
// Lucene90BlockTreeTermsReader.java:78-104
const TIM_CODEC: &str = "BlockTreeTermsDict";
const TIP_CODEC: &str = "BlockTreeTermsIndex";
const TMD_CODEC: &str = "BlockTreeTermsMeta";
const BLOCKTREE_VERSION: u32 = 2;
/// PerFieldPostingsFormat assigns suffix 0 to the first (only) format.
const SEGMENT_SUFFIX: &str = "Lucene912_0";

/// Lucene912PostingsFormat.java:347-352
const LEVEL1_MASK: usize = 4095;
/// Lucene90BlockTreeTermsWriter.java:223,229
const MIN_BLOCK: usize = 25;
const MAX_BLOCK: usize = 48;

/// Lucene90BlockTreeTermsReader.java:72-75
const OUTPUT_FLAG_IS_FLOOR: u64 = 0x1;
const OUTPUT_FLAG_HAS_TERMS: u64 = 0x2;

fn file_name(segment: &str, ext: &str) -> String {
    format!("{segment}_{SEGMENT_SUFFIX}.{ext}")
}

/// Per-term state that lands in .tim metadata (IntBlockTermState,
/// Lucene912PostingsFormat.java:425-457).
#[derive(Clone, Copy, Default)]
struct TermState {
    doc_start_fp: u64,
    pos_start_fp: u64,
    last_pos_block_offset: i64,
    singleton_doc_id: i64,
}

/// EMPTY_STATE (Lucene912PostingsWriter.java:425-457): fps 0, singleton -1.
const EMPTY_STATE: TermState = TermState {
    doc_start_fp: 0,
    pos_start_fp: 0,
    last_pos_block_offset: -1,
    singleton_doc_id: -1,
};

/// BitUtil.zigZagEncode (BitUtil.java:293)
fn zigzag(v: i64) -> u64 {
    ((v >> 63) ^ (v << 1)) as u64
}

/// Lucene912PostingsWriter.encodeTerm (:605-643).
fn encode_term(
    out: &mut IndexOutput,
    state: &TermState,
    last: &mut TermState,
    has_positions: bool,
) -> io::Result<()> {
    if last.singleton_doc_id != -1
        && state.singleton_doc_id != -1
        && state.doc_start_fp == last.doc_start_fp
    {
        // Run of singletons sharing the same .doc pointer: zigzag-delta the ids.
        let delta = state.singleton_doc_id - last.singleton_doc_id;
        out.write_vlong(((zigzag(delta) << 1) | 0x01) as i64)?;
    } else {
        let has_singleton = state.singleton_doc_id != -1;
        out.write_vlong(
            (((state.doc_start_fp - last.doc_start_fp) << 1) | (has_singleton as u64)) as i64,
        )?;
        if has_singleton {
            out.write_vint(state.singleton_doc_id as i32)?;
        }
    }
    if has_positions {
        out.write_vlong((state.pos_start_fp - last.pos_start_fp) as i64)?;
        // Always write lastPosBlockOffset (uses 0 as the sentinel for "none",
        // avoiding the ambiguous peek in the reader).
        out.write_vlong(state.last_pos_block_offset.max(0))?;
    }
    *last = *state;
    Ok(())
}

/// Lucene912PostingsWriter.writeImpacts (:488-504). `impacts` = (freq, norm)
/// pairs, norm ascending (unsigned), freq strictly increasing.
fn write_impacts(out: &mut IndexOutput, impacts: &[(u32, u64)]) -> io::Result<()> {
    let (mut prev_freq, mut prev_norm) = (0u32, 0u64);
    for &(freq, norm) in impacts {
        let freq_delta = freq - prev_freq - 1;
        let norm_delta = norm.wrapping_sub(prev_norm).wrapping_sub(1);
        if norm_delta == 0 {
            out.write_vint((freq_delta << 1) as i32)?;
        } else {
            out.write_vint(((freq_delta << 1) | 1) as i32)?;
            out.write_zlong(norm_delta as i64)?;
        }
        prev_freq = freq;
        prev_norm = norm;
    }
    Ok(())
}

/// Blocktree StatsWriter (Lucene90BlockTreeTermsWriter.java:601-631):
/// singleton runs (df==1 && (!hasFreqs || ttf==1)) are run-length encoded.
struct StatsWriter {
    out: IndexOutput,
    has_freqs: bool,
    singleton_count: u32,
}

impl StatsWriter {
    fn new(has_freqs: bool) -> Self {
        Self {
            out: IndexOutput::in_memory(),
            has_freqs,
            singleton_count: 0,
        }
    }

    fn add(&mut self, df: u32, ttf: u64) -> io::Result<()> {
        if df == 1 && (!self.has_freqs || ttf == 1) {
            self.singleton_count += 1;
        } else {
            self.finish()?;
            self.out.write_vint((df << 1) as i32)?;
            if self.has_freqs {
                self.out.write_vlong((ttf - df as u64) as i64)?;
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.singleton_count > 0 {
            self.out
                .write_vint((((self.singleton_count - 1) << 1) | 1) as i32)?;
            self.singleton_count = 0;
        }
        Ok(())
    }
}

/// Pending stack entry (Lucene90BlockTreeTermsWriter.PendingEntry).
enum Pending {
    Term {
        bytes: Vec<u8>,
        state: TermState,
        doc_freq: u32,
        ttf: u64,
    },
    Block { prefix: Vec<u8>, fp: u64 },
}

impl Pending {
    fn is_term(&self) -> bool {
        matches!(self, Pending::Term { .. })
    }
}

struct FieldState {
    number: i32,
    has_freqs: bool,
    has_positions: bool,
    doc_count: u32,
    // blocktree pending stack machinery (Lucene90BlockTreeTermsWriter.TermsWriter)
    pending: Vec<Pending>,
    prefix_starts: Vec<u32>,
    last_term: Vec<u8>,
    fst_entries: Vec<(Vec<u8>, Vec<u8>)>,
    // stats
    num_terms: u64,
    sum_doc_freq: u64,
    sum_total_term_freq: u64,
    min_term: Option<Vec<u8>>,
    max_term: Vec<u8>,
}

/// Lucene912PostingsFormat writer: owns the segment's postings files.
pub struct PostingsWriter {
    dir: FSDirectory,
    segment: String,
    segment_id: [u8; 16],
    doc_out: ChecksumIndexOutput,
    pos_out: Option<ChecksumIndexOutput>,
    tim_out: ChecksumIndexOutput,
    tip_out: ChecksumIndexOutput,
    tmd_out: ChecksumIndexOutput,
    psm_out: ChecksumIndexOutput,
    field: Option<FieldState>,
    /// Serialized per-field .tmd records (Lucene90BlockTreeTermsWriter.fields).
    field_records: Vec<Vec<u8>>,
    max_num_impacts_level0: i32,
    max_impact_bytes_level0: i32,
    max_num_impacts_level1: i32,
    max_impact_bytes_level1: i32,
    files: Vec<String>,
}

impl PostingsWriter {
    pub fn new(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16]) -> io::Result<Self> {
        let mut doc_out = dir.create_output(&file_name(segment, "doc"))?;
        write_index_header(&mut doc_out, DOC_CODEC, POSTINGS_VERSION, segment_id, SEGMENT_SUFFIX)?;
        let mut tim_out = dir.create_output(&file_name(segment, "tim"))?;
        write_index_header(&mut tim_out, TIM_CODEC, BLOCKTREE_VERSION, segment_id, SEGMENT_SUFFIX)?;
        let mut tip_out = dir.create_output(&file_name(segment, "tip"))?;
        write_index_header(&mut tip_out, TIP_CODEC, BLOCKTREE_VERSION, segment_id, SEGMENT_SUFFIX)?;
        let mut tmd_out = dir.create_output(&file_name(segment, "tmd"))?;
        write_index_header(&mut tmd_out, TMD_CODEC, BLOCKTREE_VERSION, segment_id, SEGMENT_SUFFIX)?;
        // PostingsHeader lives in .tmd (Lucene912PostingsWriter.init:209-213).
        write_index_header(&mut tmd_out, TERMS_CODEC, POSTINGS_VERSION, segment_id, SEGMENT_SUFFIX)?;
        tmd_out.write_vint(BLOCK_SIZE as i32)?;
        let mut psm_out = dir.create_output(&file_name(segment, "psm"))?;
        write_index_header(&mut psm_out, PSM_CODEC, POSTINGS_VERSION, segment_id, SEGMENT_SUFFIX)?;
        Ok(Self {
            dir: dir.clone(),
            segment: segment.to_string(),
            segment_id: *segment_id,
            doc_out,
            pos_out: None,
            tim_out,
            tip_out,
            tmd_out,
            psm_out,
            field: None,
            field_records: Vec::new(),
            max_num_impacts_level0: 0,
            max_impact_bytes_level0: 0,
            max_num_impacts_level1: 0,
            max_impact_bytes_level1: 0,
            files: vec![
                file_name(segment, "doc"),
                file_name(segment, "tim"),
                file_name(segment, "tip"),
                file_name(segment, "tmd"),
                file_name(segment, "psm"),
            ],
        })
    }

    pub fn start_field(&mut self, field: &FieldInfo, doc_count: u32) -> io::Result<()> {
        assert!(self.field.is_none(), "previous field not finished");
        let has_freqs = field.index_options != IndexOptions::Docs;
        let has_positions = matches!(
            field.index_options,
            IndexOptions::DocsAndFreqsAndPositions
                | IndexOptions::DocsAndFreqsAndPositionsAndOffsets
        );
        if has_positions && self.pos_out.is_none() {
            // Lazily create .pos on the first field with positions
            // (Lucene912PostingsWriter:148-189 decides from hasProx upfront;
            // for an empty positions field the file is simply absent — legal).
            let mut pos_out = self
                .dir
                .create_output(&file_name(&self.segment, "pos"))?;
            write_index_header(
                &mut pos_out,
                POS_CODEC,
                POSTINGS_VERSION,
                &self.segment_id,
                SEGMENT_SUFFIX,
            )?;
            self.pos_out = Some(pos_out);
            self.files.push(file_name(&self.segment, "pos"));
        }
        self.field = Some(FieldState {
            number: field.number,
            has_freqs,
            has_positions,
            doc_count,
            pending: Vec::new(),
            prefix_starts: Vec::new(),
            last_term: Vec::new(),
            fst_entries: Vec::new(),
            num_terms: 0,
            sum_doc_freq: 0,
            sum_total_term_freq: 0,
            min_term: None,
            max_term: Vec::new(),
        });
        Ok(())
    }

    pub fn write_term(
        &mut self,
        term: &[u8],
        docs: &[u32],
        freqs: &[u32],
        positions: Option<&[Vec<u32>]>,
    ) -> io::Result<()> {
        assert!(!docs.is_empty(), "empty postings");
        debug_assert!(docs.windows(2).all(|w| w[0] < w[1]), "docs must ascend");
        let doc_freq = docs.len() as u32;

        // --- .pos: full pfor chunks + tail (before .doc so skip fps are known)
        let (pos_start_fp, last_pos_block_offset, pos_block_index) =
            self.write_positions(docs, freqs, positions)?;

        // --- .doc
        let doc_start_fp = self.doc_out.file_pointer();
        let singleton_doc_id = if doc_freq == 1 {
            docs[0] as i64
        } else {
            -1
        };
        if doc_freq > 1 {
            self.write_doc_postings(docs, freqs, pos_start_fp, &pos_block_index)?;
        }

        // --- blocktree
        let ttf: u64 = freqs.iter().map(|&f| f as u64).sum();
        let state = TermState {
            doc_start_fp,
            pos_start_fp,
            last_pos_block_offset,
            singleton_doc_id,
        };
        self.push_term(term, state, doc_freq, ttf)?;
        Ok(())
    }

    pub fn finish_field(&mut self) -> io::Result<()> {
        let mut f = self.field.take().expect("start_field first");
        if f.num_terms > 0 {
            // Force-close all open prefixes, then write the root block group
            // (Lucene90BlockTreeTermsWriter.finish:1149-1161).
            Self::push_term_boundary(&mut f, &[], &mut self.tim_out)?;
            let count = f.pending.len();
            Self::write_blocks(&mut f, 0, count, &mut self.tim_out)?;
            debug_assert!(f.pending.len() == 1 && !f.pending[0].is_term());

            // Build the per-field FST from all block-group first entries,
            // globally sorted (root prefix "" sorts first and becomes the
            // FST empty output == rootCode).
            f.fst_entries.sort();
            let mut compiler = FstCompiler::new();
            for (prefix, output) in &f.fst_entries {
                compiler.add(prefix, Some(output));
            }
            let fst = compiler.finish();
            let root_code = fst
                .empty_output()
                .expect("root block output missing")
                .to_vec();

            let index_start_fp = self.tip_out.file_pointer();
            self.tip_out.write_bytes(fst.bytes())?;

            // FieldMetadata (Lucene90BlockTreeTermsWriter.finish:1174-1188).
            let mut rec = IndexOutput::in_memory();
            rec.write_vint(f.number)?;
            rec.write_vlong(f.num_terms as i64)?;
            rec.write_vint(root_code.len() as i32)?;
            rec.write_bytes(&root_code)?;
            if f.has_freqs {
                rec.write_vlong(f.sum_total_term_freq as i64)?;
            }
            rec.write_vlong(f.sum_doc_freq as i64)?;
            rec.write_vint(f.doc_count as i32)?;
            let min_term = f.min_term.take().unwrap_or_default();
            rec.write_vint(min_term.len() as i32)?;
            rec.write_bytes(&min_term)?;
            rec.write_vint(f.max_term.len() as i32)?;
            rec.write_bytes(&f.max_term)?;
            rec.write_vlong(index_start_fp as i64)?;
            let mut rec_checksummed = ChecksumIndexOutput::new(IndexOutput::in_memory());
            rec_checksummed.write_bytes(&rec.into_bytes())?;
            fst.write_metadata(&mut rec_checksummed)?;
            self.field_records.push(rec_checksummed.into_bytes());
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<Vec<String>> {
        assert!(self.field.is_none(), "field not finished");
        // Lucene90BlockTreeTermsWriter.close:1221-1238
        write_footer(&mut self.tip_out)?;
        let index_length = self.tip_out.file_pointer();
        write_footer(&mut self.tim_out)?;
        let terms_length = self.tim_out.file_pointer();
        self.tmd_out.write_vint(self.field_records.len() as i32)?;
        for rec in &self.field_records {
            self.tmd_out.write_bytes(rec)?;
        }
        self.tmd_out.write_long(index_length as i64)?;
        self.tmd_out.write_long(terms_length as i64)?;
        write_footer(&mut self.tmd_out)?;

        write_footer(&mut self.doc_out)?;
        let doc_len = self.doc_out.file_pointer();
        let pos_len = match &mut self.pos_out {
            Some(pos) => {
                write_footer(pos)?;
                Some(pos.file_pointer())
            }
            None => None,
        };
        // .psm (Lucene912PostingsWriter.close:659-671)
        self.psm_out.write_int(self.max_num_impacts_level0)?;
        self.psm_out.write_int(self.max_impact_bytes_level0)?;
        self.psm_out.write_int(self.max_num_impacts_level1)?;
        self.psm_out.write_int(self.max_impact_bytes_level1)?;
        self.psm_out.write_long(doc_len as i64)?;
        if let Some(pl) = pos_len {
            self.psm_out.write_long(pl as i64)?;
        }
        write_footer(&mut self.psm_out)?;

        self.doc_out.flush()?;
        if let Some(pos) = &mut self.pos_out {
            pos.flush()?;
        }
        self.tim_out.flush()?;
        self.tip_out.flush()?;
        self.tmd_out.flush()?;
        self.psm_out.flush()?;
        Ok(self.files)
    }

    // ------------------------------------------------------------------
    // positions (.pos)
    // ------------------------------------------------------------------

    /// Writes the term's position deltas: full 128-blocks as pfor, tail as
    /// per-delta VInts (Lucene912PostingsWriter.addPosition:287-342,
    /// finishTerm:546-587). Returns (posStartFP, lastPosBlockOffset, index)
    /// where index maps a doc-block end (in docs) to (posFP, posBufferUpto).
    fn write_positions(
        &mut self,
        docs: &[u32],
        freqs: &[u32],
        positions: Option<&[Vec<u32>]>,
    ) -> io::Result<(u64, i64, Vec<(u64, u8)>)> {
        let Some(pos_lists) = positions else {
            return Ok((0, -1, Vec::new()));
        };
        let pos_out = self.pos_out.as_mut().expect(".pos not created");
        let pos_start_fp = pos_out.file_pointer();

        // Flatten per-doc position deltas (lastPosition resets per doc, :301).
        let total: usize = pos_lists.iter().map(Vec::len).sum();
        let mut deltas: Vec<u64> = Vec::with_capacity(total);
        for plist in pos_lists {
            let mut last = 0u32;
            for &p in plist {
                deltas.push((p - last) as u64);
                last = p;
            }
        }

        let full_chunks = deltas.len() / BLOCK_SIZE;
        let mut chunk_bytes: Vec<u64> = Vec::with_capacity(full_chunks);
        for c in 0..full_chunks {
            let chunk: &[u64; BLOCK_SIZE] = deltas[c * BLOCK_SIZE..(c + 1) * BLOCK_SIZE]
                .try_into()
                .unwrap();
            let before = pos_out.file_pointer();
            pfor_util_encode(pos_out, chunk)?;
            chunk_bytes.push(pos_out.file_pointer() - before);
        }
        let fp_after_full_blocks = pos_out.file_pointer();
        let ttf = deltas.len() as u64;
        // finishTerm:527-538 — recorded before the tail VInts.
        let last_pos_block_offset = if ttf > BLOCK_SIZE as u64 {
            (fp_after_full_blocks - pos_start_fp) as i64
        } else {
            -1
        };
        for &d in &deltas[full_chunks * BLOCK_SIZE..] {
            pos_out.write_vint(d as i32)?;
        }

        // Index: for every full doc-block end, (posFP, posBufferUpto).
        let mut index: Vec<(u64, u8)> = Vec::new();
        let mut cum: u64 = 0;
        let mut cum_bytes: u64 = 0;
        let mut chunk_i: usize = 0;
        for (i, &f) in freqs.iter().enumerate() {
            cum += f as u64;
            if (i + 1) % BLOCK_SIZE == 0 {
                let completed = (cum / BLOCK_SIZE as u64) as usize;
                while chunk_i < completed {
                    cum_bytes += chunk_bytes[chunk_i];
                    chunk_i += 1;
                }
                index.push((pos_start_fp + cum_bytes, (cum % BLOCK_SIZE as u64) as u8));
            }
        }
        let _ = docs;
        Ok((pos_start_fp, last_pos_block_offset, index))
    }

    // ------------------------------------------------------------------
    // postings (.doc)
    // ------------------------------------------------------------------

    /// Writes one term's doc/freq stream (Lucene912PostingsWriter.flushDocBlock
    /// :375-442 + writeLevel1SkipData :444-486 + finishTerm VInt tail via
    /// PostingsUtil.writeVIntBlock:55-72).
    fn write_doc_postings(
        &mut self,
        docs: &[u32],
        freqs: &[u32],
        pos_start_fp: u64,
        pos_block_index: &[(u64, u8)],
    ) -> io::Result<()> {
        let f = self.field.as_ref().expect("start_field first");
        let has_freqs = f.has_freqs;
        let has_positions = f.has_positions;

        let mut level1_buf = IndexOutput::in_memory();
        let mut level0_last_doc: i64 = -1;
        let mut level1_last_doc: i64 = -1;
        // pos fp deltas are relative to the term's posStartFP for the first
        // block, then to the previous block boundary
        // (Lucene912PostingsWriter.startTerm:226-227).
        let mut level0_last_pos_fp: u64 = pos_start_fp;
        let mut level1_last_pos_fp: u64 = pos_start_fp;
        let mut level1_max_freq: u32 = 0;

        for (blk_i, chunk) in docs.chunks(BLOCK_SIZE).enumerate() {
            let block_start = blk_i * BLOCK_SIZE;
            let block_end_doc = *chunk.last().unwrap() as i64;
            if chunk.len() == BLOCK_SIZE {
                // --- assemble level-0 skip fields + packed blocks
                let mut level0 = IndexOutput::in_memory();
                if has_freqs {
                    let block_max_freq =
                        freqs[block_start..block_start + BLOCK_SIZE].iter().max().unwrap();
                    let impacts = [(*block_max_freq, 1u64)];
                    let mut scratch = IndexOutput::in_memory();
                    write_impacts(&mut scratch, &impacts)?;
                    let impact_bytes = scratch.file_pointer();
                    self.max_num_impacts_level0 =
                        self.max_num_impacts_level0.max(impacts.len() as i32);
                    self.max_impact_bytes_level0 =
                        self.max_impact_bytes_level0.max(impact_bytes as i32);
                    level0.write_vlong(impact_bytes as i64)?;
                    level0.write_bytes(&scratch.into_bytes())?;
                    if has_positions {
                        let (pos_fp, pos_buffer_upto) = pos_block_index[blk_i];
                        level0.write_vlong((pos_fp - level0_last_pos_fp) as i64)?;
                        level0.write_byte(pos_buffer_upto)?;
                        level0_last_pos_fp = pos_fp;
                    }
                    level1_max_freq = level1_max_freq.max(*block_max_freq);
                }
                let mut num_skip_bytes = level0.file_pointer();

                let mut deltas = [0u64; BLOCK_SIZE];
                let mut prev = if blk_i == 0 {
                    -1i64
                } else {
                    docs[block_start - 1] as i64
                };
                for (i, &d) in chunk.iter().enumerate() {
                    deltas[i] = (d as i64 - prev) as u64;
                    prev = d as i64;
                }
                for_delta_util_encode(&mut level0, &deltas)?;
                if has_freqs {
                    let mut f128 = [0u64; BLOCK_SIZE];
                    for (i, &fq) in freqs[block_start..block_start + BLOCK_SIZE]
                        .iter()
                        .enumerate()
                    {
                        f128[i] = fq as u64;
                    }
                    pfor_util_encode(&mut level0, &f128)?;
                }

                let mut scratch = IndexOutput::in_memory();
                write_vint15(&mut scratch, (block_end_doc - level0_last_doc) as u32)?;
                write_vlong15(&mut scratch, level0.file_pointer())?;
                num_skip_bytes += scratch.file_pointer();
                level1_buf.write_vlong(num_skip_bytes as i64)?;
                level1_buf.write_bytes(&scratch.into_bytes())?;
                level1_buf.write_bytes(&level0.into_bytes())?;
                level0_last_doc = block_end_doc;
            } else {
                // --- VInt tail (no skip prefix), into level1_buf
                let mut prev = if blk_i == 0 {
                    -1i64
                } else {
                    docs[block_start - 1] as i64
                };
                let n = chunk.len();
                let mut gvi: Vec<u32> = Vec::with_capacity(n);
                for (i, &d) in chunk.iter().enumerate() {
                    let delta = (d as i64 - prev) as u32;
                    prev = d as i64;
                    if has_freqs {
                        gvi.push((delta << 1) | if freqs[block_start + i] == 1 { 1 } else { 0 });
                    } else {
                        gvi.push(delta);
                    }
                }
                write_group_vints(&mut level1_buf, &gvi)?;
                if has_freqs {
                    for i in 0..n {
                        let fq = freqs[block_start + i];
                        if fq != 1 {
                            level1_buf.write_vint(fq as i32)?;
                        }
                    }
                }
                level0_last_doc = block_end_doc;
            }

            let docs_done = block_start + chunk.len();
            if docs_done & LEVEL1_MASK == 0 {
                // --- level-1 skip data, then drain the 32 blocks
                self.doc_out
                    .write_vint((block_end_doc - level1_last_doc) as i32)?;
                if has_freqs {
                    let impacts = [(level1_max_freq, 1u64)];
                    let mut scratch = IndexOutput::in_memory();
                    write_impacts(&mut scratch, &impacts)?;
                    let num_impact_bytes = scratch.file_pointer();
                    self.max_num_impacts_level1 =
                        self.max_num_impacts_level1.max(impacts.len() as i32);
                    self.max_impact_bytes_level1 =
                        self.max_impact_bytes_level1.max(num_impact_bytes as i32);
                    if has_positions {
                        let (pos_fp, pos_buffer_upto) = pos_block_index[blk_i];
                        scratch.write_vlong((pos_fp - level1_last_pos_fp) as i64)?;
                        scratch.write_byte(pos_buffer_upto)?;
                        level1_last_pos_fp = pos_fp;
                    }
                    let level1_len =
                        2 * 2 + scratch.file_pointer() + level1_buf.file_pointer();
                    self.doc_out.write_vlong(level1_len as i64)?;
                    self.doc_out
                        .write_short((scratch.file_pointer() + 2) as i16)?;
                    self.doc_out.write_short(num_impact_bytes as i16)?;
                    self.doc_out.write_bytes(&scratch.into_bytes())?;
                } else {
                    self.doc_out
                        .write_vlong(level1_buf.file_pointer() as i64)?;
                }
                let drained = std::mem::replace(&mut level1_buf, IndexOutput::in_memory());
                self.doc_out.write_bytes(&drained.into_bytes())?;
                level1_last_doc = block_end_doc;
                level1_max_freq = 0;
            }
        }
        // Term end: drain remaining blocks without a level-1 header.
        self.doc_out.write_bytes(&level1_buf.into_bytes())?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // blocktree (.tim/.tip)
    // ------------------------------------------------------------------

    /// Lucene90BlockTreeTermsWriter.pushTerm (:1105-1146) + term bookkeeping.
    fn push_term(
        &mut self,
        term: &[u8],
        state: TermState,
        doc_freq: u32,
        ttf: u64,
    ) -> io::Result<()> {
        let tim_out = &mut self.tim_out;
        let f = self.field.as_mut().expect("start_field first");
        debug_assert!(
            f.last_term.is_empty() || f.last_term.as_slice() < term,
            "terms must ascend"
        );

        Self::push_term_boundary(f, term, tim_out)?;
        f.pending.push(Pending::Term {
            bytes: term.to_vec(),
            state,
            doc_freq,
            ttf,
        });
        f.num_terms += 1;
        f.sum_doc_freq += doc_freq as u64;
        f.sum_total_term_freq += ttf;
        if f.min_term.is_none() {
            f.min_term = Some(term.to_vec());
        }
        f.max_term.clear();
        f.max_term.extend_from_slice(term);
        Ok(())
    }

    /// Closes abandoned prefix levels for the next term (does not push it).
    fn push_term_boundary(
        f: &mut FieldState,
        term: &[u8],
        tim_out: &mut ChecksumIndexOutput,
    ) -> io::Result<()> {
        let prefix_len = f
            .last_term
            .iter()
            .zip(term.iter())
            .take_while(|(a, b)| a == b)
            .count();
        for i in (prefix_len..f.last_term.len()).rev() {
            let top = f.pending.len() - f.prefix_starts[i] as usize;
            if top >= MIN_BLOCK {
                Self::write_blocks(f, i + 1, top, tim_out)?;
                f.prefix_starts[i] -= (top - 1) as u32;
            }
        }
        if f.prefix_starts.len() < term.len() {
            f.prefix_starts.resize(term.len(), 0);
        }
        for i in prefix_len..term.len() {
            f.prefix_starts[i] = f.pending.len() as u32;
        }
        f.last_term.clear();
        f.last_term.extend_from_slice(term);
        Ok(())
    }

    /// Lucene90BlockTreeTermsWriter.writeBlocks (:655-783).
    fn write_blocks(
        f: &mut FieldState,
        prefix_len: usize,
        count: usize,
        tim_out: &mut ChecksumIndexOutput,
    ) -> io::Result<()> {
        let end = f.pending.len();
        let mut next_block_start = end - count;
        let mut last_suffix_lead: i32 = -1;
        let mut next_floor_lead: i32 = -1;
        let mut has_terms = false;
        let mut has_sub_blocks = false;
        let mut new_blocks: Vec<(Vec<u8>, u64, bool, bool, i32)> = Vec::new(); // (prefix, fp, hasTerms, isFloor, leadByte)

        let mut i = next_block_start;
        while i < end {
            let suffix_lead: i32 = match &f.pending[i] {
                Pending::Term { bytes, .. } => {
                    if bytes.len() > prefix_len {
                        bytes[prefix_len] as i32
                    } else {
                        -1
                    }
                }
                Pending::Block { prefix, .. } => prefix[prefix_len] as i32,
            };
            if suffix_lead != last_suffix_lead {
                let items_in_block = i - next_block_start;
                if items_in_block >= MIN_BLOCK && end - next_block_start > MAX_BLOCK {
                    let is_floor = items_in_block < count;
                    let blk = Self::write_block(
                        f,
                        prefix_len,
                        is_floor,
                        next_floor_lead,
                        next_block_start,
                        i,
                        has_terms,
                        has_sub_blocks,
                        tim_out,
                    )?;
                    new_blocks.push(blk);
                    has_terms = false;
                    has_sub_blocks = false;
                    next_floor_lead = suffix_lead;
                    next_block_start = i;
                }
                last_suffix_lead = suffix_lead;
            }
            if f.pending[i].is_term() {
                has_terms = true;
            } else {
                has_sub_blocks = true;
            }
            i += 1;
        }
        if next_block_start < end {
            let items_in_block = end - next_block_start;
            let is_floor = items_in_block < count;
            let blk = Self::write_block(
                f,
                prefix_len,
                is_floor,
                next_floor_lead,
                next_block_start,
                end,
                has_terms,
                has_sub_blocks,
                tim_out,
            )?;
            new_blocks.push(blk);
        }
        debug_assert!(!new_blocks.is_empty());

        // compileIndex (:490-578): FST entry for the first block of the group.
        let (prefix, fp, has_terms, is_floor, _) = &new_blocks[0];
        let mut out = IndexOutput::in_memory();
        let encoded = (fp << 2)
            | if *has_terms { OUTPUT_FLAG_HAS_TERMS } else { 0 }
            | if *is_floor { OUTPUT_FLAG_IS_FLOOR } else { 0 };
        write_msb_vlong(&mut out, encoded)?;
        if *is_floor {
            out.write_vint((new_blocks.len() - 1) as i32)?;
            for (_, sub_fp, sub_has_terms, _, sub_lead) in &new_blocks[1..] {
                out.write_byte(*sub_lead as u8)?;
                out.write_vlong((((sub_fp - fp) << 1) | if *sub_has_terms { 1 } else { 0 }) as i64)?;
            }
        }
        f.fst_entries.push((prefix.clone(), out.into_bytes()));

        f.pending.truncate(end - count);
        let (prefix, fp, ..) = new_blocks.into_iter().next().unwrap();
        f.pending.push(Pending::Block { prefix, fp });
        Ok(())
    }

    /// Lucene90BlockTreeTermsWriter.writeBlock (:801-1059). Always
    /// NO_COMPRESSION (a legal subset the reader decodes by flag).
    #[allow(clippy::too_many_arguments)]
    fn write_block(
        f: &mut FieldState,
        prefix_len: usize,
        is_floor: bool,
        floor_lead_label: i32,
        start: usize,
        end: usize,
        has_terms: bool,
        has_sub_blocks: bool,
        tim_out: &mut ChecksumIndexOutput,
    ) -> io::Result<(Vec<u8>, u64, bool, bool, i32)> {
        let start_fp = tim_out.file_pointer();
        let has_floor_lead_label = is_floor && floor_lead_label != -1;

        let num_entries = end - start;
        let mut code = (num_entries << 1) as i32;
        if end == f.pending.len() {
            code |= 1; // isLastInFloor
        }
        tim_out.write_vint(code)?;

        let is_leaf = !has_sub_blocks;
        let mut suffix_bytes: Vec<u8> = Vec::new();
        let mut suffix_lengths = IndexOutput::in_memory();
        let mut stats = StatsWriter::new(f.has_freqs);
        let mut meta = IndexOutput::in_memory();
        let mut last_state = EMPTY_STATE; // first metadata entry is absolute

        for i in start..end {
            match &f.pending[i] {
                Pending::Term {
                    bytes,
                    state,
                    doc_freq,
                    ttf,
                } => {
                    let suffix = bytes.len() - prefix_len;
                    if is_leaf {
                        suffix_lengths.write_vint(suffix as i32)?;
                    } else {
                        suffix_lengths.write_vint((suffix << 1) as i32)?;
                    }
                    suffix_bytes.extend_from_slice(&bytes[prefix_len..]);
                    stats.add(*doc_freq, *ttf)?;
                    encode_term(&mut meta, state, &mut last_state, f.has_positions)?;
                }
                Pending::Block { prefix, fp, .. } => {
                    debug_assert!(!is_leaf);
                    let suffix = prefix.len() - prefix_len;
                    suffix_lengths.write_vint(((suffix << 1) | 1) as i32)?;
                    suffix_bytes.extend_from_slice(&prefix[prefix_len..]);
                    // sub-block back-pointer lives in the lengths stream (:945-965)
                    suffix_lengths.write_vlong((start_fp - fp) as i64)?;
                }
            }
        }
        stats.finish()?;

        // suffix blob token (CompressionAlgorithm.NO_COMPRESSION = 0, :1011-1016)
        let token = ((suffix_bytes.len() as u64) << 3) | if is_leaf { 0x04 } else { 0 };
        tim_out.write_vlong(token as i64)?;
        tim_out.write_bytes(&suffix_bytes)?;

        // suffix lengths, with the all-equal-bytes trick (:1026-1037)
        let lengths = suffix_lengths.into_bytes();
        let n = lengths.len();
        if n > 0 && lengths[1..].iter().all(|&b| b == lengths[0]) {
            tim_out.write_vint(((n << 1) | 1) as i32)?;
            tim_out.write_byte(lengths[0])?;
        } else {
            tim_out.write_vint((n << 1) as i32)?;
            tim_out.write_bytes(&lengths)?;
        }

        // stats + metadata blobs
        let stats_bytes = stats.out.into_bytes();
        tim_out.write_vint(stats_bytes.len() as i32)?;
        tim_out.write_bytes(&stats_bytes)?;
        let meta_bytes = meta.into_bytes();
        tim_out.write_vint(meta_bytes.len() as i32)?;
        tim_out.write_bytes(&meta_bytes)?;

        let mut prefix = f.last_term[..prefix_len].to_vec();
        if has_floor_lead_label {
            prefix.push(floor_lead_label as u8);
        }
        Ok((prefix, start_fp, has_terms, is_floor, floor_lead_label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(f: impl FnOnce(&mut IndexOutput) -> io::Result<()>) -> Vec<u8> {
        let mut out = IndexOutput::in_memory();
        f(&mut out).unwrap();
        out.into_bytes()
    }

    #[test]
    fn stats_writer_runs() {
        // singleton run of 2, then a regular term, then singleton run of 1
        let mut sw = StatsWriter::new(true);
        sw.add(1, 1).unwrap();
        sw.add(1, 1).unwrap();
        sw.add(3, 5).unwrap();
        sw.add(1, 1).unwrap();
        sw.finish().unwrap();
        let bytes = sw.out.into_bytes();
        let mut expect = Vec::new();
        expect.push(((2 - 1) << 1 | 1) as u8); // run of 2
        expect.push((3 << 1) as u8); // df=3
        expect.push((5 - 3) as u8); // ttf-df=2
        expect.push((0 << 1 | 1) as u8); // run of 1
        assert_eq!(bytes, expect);
    }

    #[test]
    fn write_impacts_no_norms() {
        // single pair (maxFreq, 1): VInt((maxFreq-1)<<1)
        let bytes = enc(|o| write_impacts(o, &[(5, 1)]));
        assert_eq!(bytes, vec![(4 << 1) as u8]);
    }

    #[test]
    fn encode_term_singleton_zigzag() {
        // two consecutive singletons with same docStartFP
        let mut last = EMPTY_STATE;
        let s1 = TermState {
            doc_start_fp: 10,
            singleton_doc_id: 7,
            ..Default::default()
        };
        let s2 = TermState {
            doc_start_fp: 10,
            singleton_doc_id: 9,
            ..Default::default()
        };
        let mut out = IndexOutput::in_memory();
        // s1 is not preceded by a singleton: plain fp delta + VInt(id)
        encode_term(&mut out, &s1, &mut last, false).unwrap();
        encode_term(&mut out, &s2, &mut last, false).unwrap();
        let bytes = out.into_bytes();
        // s1: VLong(10<<1|1)=VLong(21), VInt(7); s2: VLong(zigzag(2)<<1|1)=VLong(9)
        assert_eq!(bytes, vec![21, 7, 9]);
    }

    #[test]
    fn encode_term_last_pos_block_offset() {
        let mut last = EMPTY_STATE;
        let s = TermState {
            doc_start_fp: 0,
            pos_start_fp: 0,
            last_pos_block_offset: 1234,
            singleton_doc_id: -1,
        };
        let mut out = IndexOutput::in_memory();
        encode_term(&mut out, &s, &mut last, true).unwrap();
        let bytes = out.into_bytes();
        // VLong(0), VLong(0), VLong(1234)
        assert_eq!(bytes[0], 0);
        assert_eq!(bytes[1], 0);
        // 1234 = 0b10011010010 -> VLong [0xD2, 0x09]
        assert_eq!(&bytes[2..], &[0xD2, 0x09]);
    }
}
