//! Postings (.doc) reader: Docs and DocsAndFreqs enumeration
//! (Lucene912PostingsReader BlockDocsEnum, Lucene 9.12.3).

use std::io;

use crate::codec_util::{check_footer, check_footer_structure, check_index_header, corrupt};
use crate::directory::FSDirectory;
use crate::io::{DataInput, IndexInput};
use crate::postings::{
    DOC_CODEC, POS_CODEC, POSTINGS_VERSION, PSM_CODEC, SEGMENT_SUFFIX, file_name,
};
use crate::postings_ll::{
    BLOCK_SIZE, for_delta_util_decode, pfor_util_decode, pfor_util_skip, read_group_vints,
    read_vint15, read_vlong15,
};
use crate::roaring::{self, FrozenBitmap};
use crate::terms_read::TermEntry;

/// DocIdSetIterator.NO_MORE_DOCS.
pub const NO_MORE_DOCS: i32 = i32::MAX;

/// Lucene912PostingsFormat.java:347-352.
const LEVEL1_NUM_DOCS: u32 = 4096;

/// Owns the segment's .doc stream (Lucene912PostingsReader :83-206) plus
/// the .pos stream when the segment has any positions field (:135-177).
pub struct PostingsReader {
    doc_in: IndexInput,
    pos_in: Option<IndexInput>,
}

impl PostingsReader {
    /// Opens .psm + .doc, validating headers and exact lengths
    /// (Lucene912PostingsReader constructor :83-185).
    pub fn open(
        dir: &FSDirectory,
        segment: &str,
        segment_id: &[u8; 16],
    ) -> io::Result<PostingsReader> {
        let mut psm = dir.open_checksum_input(&file_name(segment, "psm"))?;
        check_index_header(
            &mut psm,
            PSM_CODEC,
            POSTINGS_VERSION,
            POSTINGS_VERSION,
            segment_id,
            SEGMENT_SUFFIX,
        )?;
        // max impact params (levels 0/1): needed only for impact-driven
        // skipping, which we never do — parsed and discarded (:100-103).
        let _ = psm.read_int()?;
        let _ = psm.read_int()?;
        let _ = psm.read_int()?;
        let _ = psm.read_int()?;
        let doc_len = psm.read_long()? as u64;
        // posLen is present iff the writer created a .pos file (:106).
        let pos_len = if dir.file_exists(&file_name(segment, "pos")) {
            Some(psm.read_long()? as u64)
        } else {
            None
        };
        check_footer(&mut psm)?;
        let mut doc_in = dir.open_input(&file_name(segment, "doc"))?;
        check_index_header(
            &mut doc_in,
            DOC_CODEC,
            POSTINGS_VERSION,
            POSTINGS_VERSION,
            segment_id,
            SEGMENT_SUFFIX,
        )?;
        // retrieveChecksum (:150-172): length + trailing footer structure
        check_footer_structure(&doc_in, doc_len)?;
        let pos_in = match pos_len {
            Some(len) => {
                let mut pos_in = dir.open_input(&file_name(segment, "pos"))?;
                check_index_header(
                    &mut pos_in,
                    POS_CODEC,
                    POSTINGS_VERSION,
                    POSTINGS_VERSION,
                    segment_id,
                    SEGMENT_SUFFIX,
                )?;
                // retrieveChecksum (:160-161)
                check_footer_structure(&pos_in, len)?;
                Some(pos_in)
            }
            None => None,
        };
        Ok(PostingsReader { doc_in, pos_in })
    }

    /// Docs iterator over a DOCS field's postings (no freq blocks on disk).
    pub fn docs(&self, entry: &TermEntry) -> io::Result<DocsEnum> {
        Ok(DocsEnum {
            core: EnumCore::new(self.fresh_input()?, entry, false, false)?,
        })
    }

    /// Docs+freqs iterator over a field with frequencies
    /// (IndexOptions >= DOCS_AND_FREQS).
    pub fn docs_and_freqs(&self, entry: &TermEntry) -> io::Result<DocsFreqsEnum> {
        Ok(DocsFreqsEnum {
            core: EnumCore::new(self.fresh_input()?, entry, true, true)?,
        })
    }

    /// Docs-only iterator over a field WITH frequencies on disk: full-block
    /// freq PFOR payloads are skipped byte-wise (self-describing length,
    /// never decoded), tail freq vints are parsed and discarded. For
    /// count-only consumers; `freq()` on the returned enum panics.
    pub fn docs_and_freqs_no_freq(&self, entry: &TermEntry) -> io::Result<DocsFreqsEnum> {
        Ok(DocsFreqsEnum {
            core: EnumCore::new(self.fresh_input()?, entry, true, false)?,
        })
    }

    /// Docs+freqs+positions iterator (EverythingEnum), for fields with
    /// IndexOptions >= DOCS_AND_FREQS_AND_POSITIONS.
    pub fn positions(&self, entry: &TermEntry) -> io::Result<PositionsEnum> {
        let pos_in = self
            .pos_in
            .as_ref()
            .ok_or_else(|| corrupt("positions enum requested but the segment has no .pos file"))?;
        Ok(PositionsEnum {
            core: EnumCore::new_with_positions(
                self.fresh_input()?,
                pos_in.slice(0, pos_in.length())?,
                entry,
            )?,
        })
    }

    /// An independent positioned stream over .doc (enums own their cursor).
    fn fresh_input(&self) -> io::Result<IndexInput> {
        self.doc_in.slice(0, self.doc_in.length())
    }

    /// Locates + bounds-checks the inline bitmap region for `entry`
    /// (`[docStartFP-4-len, docStartFP-4)`, M3 §4). Returns
    /// Some((region_start, len)). Implements the read gate (df >=
    /// BITMAP_MIN_DF, spec §5) and validation ① (len bound); never seeks
    /// below 0 — `docStartFP >= 4 + len` is required before any read
    /// (正确性硬性要求 b). Shares the caller's positioned stream.
    fn locate_bitmap_region(
        input: &mut IndexInput,
        entry: &TermEntry,
        max_doc: u32,
    ) -> io::Result<Option<(u64, u32)>> {
        if entry.doc_freq < roaring::BITMAP_MIN_DF {
            return Ok(None);
        }
        let fp = entry.state.doc_start_fp;
        if fp < 4 {
            return Ok(None);
        }
        input.seek(fp - 4)?;
        let len = input.read_int()? as u32;
        if len == 0 || len as u64 > roaring::max_bitmap_len(max_doc) {
            return Ok(None);
        }
        if fp - 4 < len as u64 {
            return Ok(None);
        }
        Ok(Some((fp - 4 - len as u64, len)))
    }

    /// Frozen-view open of the term's inline bitmap (M5 §2/§3): v3 triple
    /// validation + one sequential region read into a 32B-aligned buffer
    /// + zero-copy croaring views (single unsafe site, roaring/frozen.rs).
    /// None → postings fallback. The query path's only bitmap entry point
    /// (probe mode is gone — frozen contains is a direct memory probe,
    /// 关键设计事实 6).
    pub fn open_term_bitmap(
        &self,
        entry: &TermEntry,
        max_doc: u32,
    ) -> io::Result<Option<FrozenBitmap>> {
        let mut input = self.fresh_input()?;
        let Some((start, len)) = Self::locate_bitmap_region(&mut input, entry, max_doc)? else {
            return Ok(None);
        };
        input.seek(start)?;
        let mut region = vec![0u8; len as usize];
        input.read_bytes(&mut region)?;
        Ok(roaring::parse_region(&region, entry.doc_freq))
    }
}

/// BlockDocsEnum state machine (:345-625), shared by the two public enums.
/// `has_freqs` describes the on-disk layout (freq blocks present); Java
/// decodes freqs lazily on first `freq()` call via freqFP, M1 decodes eagerly
/// per block — except in no-freq mode (`decode_freqs == false`), where the
/// self-describing PFOR block is stepped over byte-wise and tail freq vints
/// are parsed and discarded, keeping the stream position identical while
/// never materializing freq_buffer. `freq()` panics in no-freq mode.
struct EnumCore {
    doc_in: IndexInput,
    doc_freq: u32,
    total_term_freq: u64,
    singleton_doc_id: i64,
    has_freqs: bool,
    decode_freqs: bool,
    doc: i64,
    prev_doc_id: i64,
    doc_count_upto: u32,
    level0_last_doc: i64,
    level1_last_doc: i64,
    level1_doc_end_fp: u64,
    level1_doc_count_upto: u32,
    doc_buffer: [u64; BLOCK_SIZE + 1],
    freq_buffer: [u32; BLOCK_SIZE],
    doc_buffer_upto: usize,
    has_positions: bool,
    // level-0/level-1 pos skip state (EverythingEnum :696-710); the pos fp
    // deltas chain across blocks, first block's base = posTermStartFP
    // (writer postings.rs:566-568).
    level0_pos_end_fp: u64,
    level0_block_pos_upto: u64,
    level1_pos_end_fp: u64,
    level1_block_pos_upto: u64,
    pos: Option<PosCore>,
}

/// EverythingEnum position state (:650-710): .pos stream + pending
/// bookkeeping. No payloads/offsets exist in this system's indexes, so only
/// the delta buffer is kept.
struct PosCore {
    pos_in: IndexInput,
    /// File pointer of the tail (VInt) block; -1 when ttf == BLOCK_SIZE
    /// (EverythingEnum.reset :789-797).
    last_pos_block_fp: i64,
    pos_delta_buffer: [u32; BLOCK_SIZE],
    pos_buffer_upto: usize,
    /// How many positions "behind" we are; next_position catches up
    /// (:675-679).
    pos_pending_count: u64,
    position: u32,
}

impl EnumCore {
    /// reset (:413-446).
    fn new(
        doc_in: IndexInput,
        entry: &TermEntry,
        has_freqs: bool,
        decode_freqs: bool,
    ) -> io::Result<EnumCore> {
        let mut c = EnumCore {
            doc_in,
            doc_freq: entry.doc_freq,
            total_term_freq: entry.total_term_freq,
            singleton_doc_id: entry.state.singleton_doc_id,
            has_freqs,
            decode_freqs: decode_freqs && has_freqs,
            doc: -1,
            prev_doc_id: -1,
            doc_count_upto: 0,
            level0_last_doc: -1,
            level1_last_doc: -1,
            level1_doc_end_fp: 0,
            level1_doc_count_upto: 0,
            doc_buffer: [0; BLOCK_SIZE + 1],
            freq_buffer: [0; BLOCK_SIZE],
            doc_buffer_upto: BLOCK_SIZE,
            has_positions: false,
            level0_pos_end_fp: 0,
            level0_block_pos_upto: 0,
            level1_pos_end_fp: 0,
            level1_block_pos_upto: 0,
            pos: None,
        };
        if entry.doc_freq < LEVEL1_NUM_DOCS {
            c.level1_last_doc = NO_MORE_DOCS as i64;
            c.level1_doc_end_fp = c.doc_in.length(); // guard: stray seek lands at EOF
            if entry.doc_freq > 1 {
                c.doc_in.seek(entry.state.doc_start_fp)?;
            }
        } else {
            c.level1_last_doc = -1;
            c.level1_doc_end_fp = entry.state.doc_start_fp;
        }
        // Sentinel (BlockDocsEnum.java:395): refillFullBlock never writes
        // index BLOCK_SIZE, refillRemainder plants its own sentinel at
        // `left`; this guarantees the advance buffer scan terminates.
        c.doc_buffer[BLOCK_SIZE] = NO_MORE_DOCS as u64;
        Ok(c)
    }

    /// nextDoc (:589-596; EverythingEnum pos bookkeeping :940-952).
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS as i64 {
            return Ok(NO_MORE_DOCS);
        }
        if self.doc == self.level0_last_doc {
            self.move_to_next_level0_block()?;
        }
        self.doc = self.doc_buffer[self.doc_buffer_upto] as i64;
        self.doc_buffer_upto += 1;
        if self.doc != NO_MORE_DOCS as i64 && self.pos.is_some() {
            let f = self.freq() as u64; // :948 — freq of the doc just returned
            if let Some(pos) = &mut self.pos {
                pos.pos_pending_count += f;
                pos.position = 0; // :951
            }
        }
        Ok(self.doc as i32)
    }

    /// 批量版 next_doc（spec 2026-07-26 Task 2）：仅 docs / docs+freqs
    /// profile（pos.is_none()）——把 doc_buffer 里已解码的绝对 doc 窗口
    /// 直接拷出，跨 128-block 边界透明 refill。freqs = Some 时同步拷
    /// freq_buffer 同窗口（调用方保证 decode_freqs）。EverythingEnum 的
    /// position 簿记不走这里——PositionsEnum 保持逐 doc（phrase 两阶段）。
    /// 返回 0 = 耗尽。
    fn next_docs(&mut self, docs: &mut [u32], mut freqs: Option<&mut [u32]>) -> io::Result<usize> {
        debug_assert!(self.pos.is_none());
        let mut n = 0;
        while n < docs.len() {
            if self.doc == NO_MORE_DOCS as i64 {
                break;
            }
            if self.doc == self.level0_last_doc {
                self.move_to_next_level0_block()?;
            }
            let upto = self.doc_buffer_upto;
            // 窗口 = 当前缓冲到哨兵（NO_MORE_DOCS 占位）或 dst 填满
            let mut take = 0;
            while take < docs.len() - n && self.doc_buffer[upto + take] != NO_MORE_DOCS as u64 {
                take += 1;
            }
            if take == 0 {
                // 缓冲首槽即哨兵（df 恰为 128 倍数后的空 refill）：
                // 镜像 next_doc 读哨兵一步，置耗尽态。
                self.doc = self.doc_buffer[upto] as i64;
                self.doc_buffer_upto = upto + 1;
                break;
            }
            for j in 0..take {
                docs[n + j] = self.doc_buffer[upto + j] as u32;
            }
            if let Some(f) = freqs.as_deref_mut() {
                for j in 0..take {
                    f[n + j] = self.freq_buffer[upto + j];
                }
            }
            self.doc_buffer_upto = upto + take;
            self.doc = self.doc_buffer[upto + take - 1] as i64;
            n += take;
        }
        Ok(n)
    }

    /// EverythingEnum.reset (:770-826) for the positions profile: freqs
    /// always decoded (phrase needs them), pos state initialized from the
    /// term state.
    fn new_with_positions(
        doc_in: IndexInput,
        mut pos_in: IndexInput,
        entry: &TermEntry,
    ) -> io::Result<EnumCore> {
        let mut c = EnumCore::new(doc_in, entry, true, true)?;
        c.has_positions = true;
        c.level0_pos_end_fp = entry.state.pos_start_fp;
        c.level1_pos_end_fp = entry.state.pos_start_fp;
        // lastPosBlockFP (:789-797): tail block fp; -1 when ttf == BLOCK_SIZE
        let last_pos_block_fp = if entry.total_term_freq < BLOCK_SIZE as u64 {
            entry.state.pos_start_fp as i64
        } else if entry.total_term_freq == BLOCK_SIZE as u64 {
            -1
        } else {
            entry.state.pos_start_fp as i64 + entry.state.last_pos_block_offset
        };
        pos_in.seek(entry.state.pos_start_fp)?;
        c.pos = Some(PosCore {
            pos_in,
            last_pos_block_fp,
            pos_delta_buffer: [0; BLOCK_SIZE],
            pos_buffer_upto: BLOCK_SIZE,
            pos_pending_count: 0,
            position: 0,
        });
        Ok(c)
    }

    /// EverythingEnum block-boundary resync (:908-917 / :962-970): when the
    /// .pos decode cursor has not passed the upcoming doc block's start fp,
    /// seek it there and account the already-consumed positions of the
    /// pos-block containing the boundary.
    fn resync_pos_stream(&mut self) -> io::Result<()> {
        if let Some(pos) = &mut self.pos {
            if self.level0_pos_end_fp >= pos.pos_in.file_pointer() {
                pos.pos_in.seek(self.level0_pos_end_fp)?;
                pos.pos_pending_count = self.level0_block_pos_upto;
                pos.pos_buffer_upto = BLOCK_SIZE;
            }
        }
        Ok(())
    }

    /// moveToNextLevel0Block (:573-587) + EverythingEnum's positions variant
    /// (:899-937): the has_positions branch parses the level-0 skip entry
    /// instead of skipping it wholesale and resyncs the .pos stream first.
    fn move_to_next_level0_block(&mut self) -> io::Result<()> {
        if self.doc == self.level1_last_doc {
            self.skip_level1_to(self.doc + 1)?;
        }
        self.prev_doc_id = self.level0_last_doc;
        if self.has_positions {
            // resync BEFORE parsing the new skip entry (:908-917 uses the
            // boundary fp of the block being entered)
            self.resync_pos_stream()?;
            if self.doc_freq - self.doc_count_upto >= BLOCK_SIZE as u32 {
                let _skip0_num_bytes = self.doc_in.read_vlong()?;
                let doc_delta = read_vint15(&mut self.doc_in)?;
                self.level0_last_doc += doc_delta as i64;
                let _block_total_bytes = read_vlong15(&mut self.doc_in)?;
                let impact_bytes = self.doc_in.read_vlong()? as u64;
                self.doc_in.skip_bytes(impact_bytes)?;
                self.level0_pos_end_fp += self.doc_in.read_vlong()? as u64; // :926
                self.level0_block_pos_upto = self.doc_in.read_byte()? as u64; // :927
                self.refill_full_block()?;
            } else {
                self.level0_last_doc = NO_MORE_DOCS as i64;
                self.refill_remainder()?;
            }
            return Ok(());
        }
        if self.doc_freq - self.doc_count_upto >= BLOCK_SIZE as u32 {
            let skip0_num_bytes = self.doc_in.read_vlong()? as u64;
            self.doc_in.skip_bytes(skip0_num_bytes)?;
            self.refill_full_block()?;
            self.level0_last_doc = self.doc_buffer[BLOCK_SIZE - 1] as i64;
        } else {
            self.level0_last_doc = NO_MORE_DOCS as i64;
            self.refill_remainder()?;
        }
        Ok(())
    }

    /// skipLevel1To (:522-546): parses the level-1 record inline (VInt
    /// docDelta, hasFreqs 时 VLong level1TotalBytes + Short numSkipBytes),
    /// skipping impacts/pos sections without interpreting them.
    fn skip_level1_to(&mut self, target: i64) -> io::Result<()> {
        loop {
            self.prev_doc_id = self.level1_last_doc;
            self.level0_last_doc = self.level1_last_doc;
            if self.has_positions {
                // carry level-1 pos state into level 0 (:854-856)
                self.level0_pos_end_fp = self.level1_pos_end_fp;
                self.level0_block_pos_upto = self.level1_block_pos_upto;
            }
            self.doc_in.seek(self.level1_doc_end_fp)?;
            self.doc_count_upto = self.level1_doc_count_upto;
            self.level1_doc_count_upto += LEVEL1_NUM_DOCS;
            if self.doc_freq - self.doc_count_upto < LEVEL1_NUM_DOCS {
                self.level1_last_doc = NO_MORE_DOCS as i64;
                break;
            }
            self.level1_last_doc += self.doc_in.read_vint()? as i64;
            self.level1_doc_end_fp = self.doc_in.read_vlong()? as u64 + self.doc_in.file_pointer();
            if self.has_freqs && self.has_positions {
                // parse the numSkipBytes section EVERY record (:883-886):
                // Short numSkipBytes, Short impactBytes + impacts,
                // VLong posFpDelta, Byte posBufferUpto
                let num_skip_bytes = self.doc_in.read_short()? as u16 as u64;
                let skip1_end_fp = num_skip_bytes + self.doc_in.file_pointer();
                let impact_bytes = self.doc_in.read_short()? as u16 as u64;
                self.doc_in.skip_bytes(impact_bytes)?;
                self.level1_pos_end_fp += self.doc_in.read_vlong()? as u64;
                self.level1_block_pos_upto = self.doc_in.read_byte()? as u64;
                debug_assert_eq!(self.doc_in.file_pointer(), skip1_end_fp); // :891
            } else if self.has_freqs && self.level1_last_doc >= target {
                let num_skip_bytes = self.doc_in.read_short()? as u16 as u64;
                self.doc_in.skip_bytes(num_skip_bytes)?;
            }
            if self.level1_last_doc >= target {
                break;
            }
        }
        Ok(())
    }

    /// skipLevel0To (:548-571): consumes level-0 skip entries (VLong
    /// skip0NumBytes, VInt15 docDelta, VLong15 blockTotalBytes — impacts/pos
    /// sections skipped via skip0NumBytes), skipping every full block whose
    /// last doc < target without decoding it. On return either the stream is
    /// positioned at the data of the block that may contain target
    /// (level0_last_doc >= target), or the full blocks are exhausted
    /// (level0_last_doc == NO_MORE_DOCS, stream at the VInt tail).
    /// skipLevel0To (:548-571) + EverythingEnum's positions variant
    /// (:954-1003): the has_positions branch parses impacts/pos fields of
    /// every skip entry (pos fp deltas chain across blocks) and resyncs the
    /// .pos stream per skipped block.
    fn skip_level0_to(&mut self, target: i64) -> io::Result<()> {
        loop {
            self.prev_doc_id = self.level0_last_doc;
            if self.has_positions {
                // :958-975 — resync to the block boundary, or (positions
                // already decoded past it) accumulate the remaining docs'
                // freqs of the current buffer instead of seeking backwards
                if self.level0_pos_end_fp >= self.pos.as_ref().unwrap().pos_in.file_pointer() {
                    self.resync_pos_stream()?;
                } else {
                    let upto = self.doc_buffer_upto;
                    let pos = self.pos.as_mut().unwrap();
                    for i in upto..BLOCK_SIZE {
                        pos.pos_pending_count += self.freq_buffer[i] as u64;
                    }
                }
            }
            if self.doc_freq - self.doc_count_upto >= BLOCK_SIZE as u32 {
                if self.has_positions {
                    let _skip0_num_bytes = self.doc_in.read_vlong()?;
                    let doc_delta = read_vint15(&mut self.doc_in)?;
                    self.level0_last_doc += doc_delta as i64;
                    let block_total_bytes = read_vlong15(&mut self.doc_in)? as u64;
                    // blockTotalBytes counts from HERE (after the vlong15)
                    // to the end of the packed data (:983); the impacts/pos
                    // fields parsed below are part of it, so skipping the
                    // block must seek to blockEndFP (:997), not skip_bytes
                    // (which would overshoot by the parsed fields' length)
                    let block_end_fp = self.doc_in.file_pointer() + block_total_bytes;
                    let impact_bytes = self.doc_in.read_vlong()? as u64;
                    self.doc_in.skip_bytes(impact_bytes)?;
                    self.level0_pos_end_fp += self.doc_in.read_vlong()? as u64; // :986
                    self.level0_block_pos_upto = self.doc_in.read_byte()? as u64; // :987
                    if target <= self.level0_last_doc {
                        break;
                    }
                    self.doc_in.seek(block_end_fp)?;
                    self.doc_count_upto += BLOCK_SIZE as u32;
                } else {
                    let skip0_num_bytes = self.doc_in.read_vlong()? as u64;
                    // end offset of skip data (before the actual data starts)
                    let skip0_end_fp = self.doc_in.file_pointer() + skip0_num_bytes;
                    let doc_delta = read_vint15(&mut self.doc_in)?;
                    self.level0_last_doc += doc_delta as i64;
                    if target <= self.level0_last_doc {
                        self.doc_in.seek(skip0_end_fp)?;
                        break;
                    }
                    // skip block
                    let block_total_bytes = read_vlong15(&mut self.doc_in)?;
                    self.doc_in.skip_bytes(block_total_bytes)?;
                    self.doc_count_upto += BLOCK_SIZE as u32;
                }
            } else {
                self.level0_last_doc = NO_MORE_DOCS as i64;
                break;
            }
        }
        Ok(())
    }

    /// refillFullBlock (:484-499): ForDelta decode + prefix sum; freq block
    /// decoded eagerly (Java defers it to the first freq() call via freqFP),
    /// or stepped over byte-wise in no-freq mode (same bytes consumed, so the
    /// stream stays positioned exactly as the decode path leaves it).
    fn refill_full_block(&mut self) -> io::Result<()> {
        let mut deltas = [0u64; BLOCK_SIZE];
        for_delta_util_decode(&mut self.doc_in, &mut deltas)?;
        prefix_sum(&mut deltas, self.prev_doc_id);
        self.doc_buffer[..BLOCK_SIZE].copy_from_slice(&deltas);
        if self.has_freqs {
            if self.decode_freqs {
                let mut freqs = [0u64; BLOCK_SIZE];
                pfor_util_decode(&mut self.doc_in, &mut freqs)?;
                for (dst, src) in self.freq_buffer.iter_mut().zip(freqs) {
                    *dst = src as u32;
                }
            } else {
                pfor_util_skip(&mut self.doc_in)?;
            }
        }
        self.doc_count_upto += BLOCK_SIZE as u32;
        self.prev_doc_id = self.doc_buffer[BLOCK_SIZE - 1] as i64;
        self.doc_buffer_upto = 0;
        Ok(())
    }

    /// refillRemainder (:501-520) + PostingsUtil.readVIntBlock (:30-52):
    /// singleton (no file bytes at all), or a group-vint tail with
    /// freq==1 folded into the delta's low bit. The (docDelta, freq) vints
    /// are interleaved, so no-freq mode still parses every freq vint but
    /// discards it instead of filling freq_buffer.
    fn refill_remainder(&mut self) -> io::Result<()> {
        let left = (self.doc_freq - self.doc_count_upto) as usize;
        if self.doc_freq == 1 {
            self.doc_buffer[0] = self.singleton_doc_id as u64;
            self.freq_buffer[0] = self.total_term_freq as u32;
            self.doc_buffer[1] = NO_MORE_DOCS as u64;
            self.doc_count_upto += 1;
        } else {
            let mut values = [0u32; BLOCK_SIZE];
            read_group_vints(&mut self.doc_in, &mut values[..left])?;
            if self.has_freqs {
                for i in 0..left {
                    let freq_is_one = values[i] & 1;
                    self.doc_buffer[i] = (values[i] >> 1) as u64;
                    let freq = if freq_is_one == 1 {
                        1
                    } else {
                        self.doc_in.read_vint()? as u32
                    };
                    if self.decode_freqs {
                        self.freq_buffer[i] = freq;
                    }
                }
            } else {
                for i in 0..left {
                    self.doc_buffer[i] = values[i] as u64;
                }
            }
            prefix_sum(&mut self.doc_buffer[..left], self.prev_doc_id);
            self.doc_buffer[left] = NO_MORE_DOCS as u64;
            self.doc_count_upto += left as u32;
        }
        self.doc_buffer_upto = 0;
        Ok(())
    }

    fn freq(&self) -> u32 {
        if self.has_freqs {
            assert!(
                self.decode_freqs,
                "freq() on a no-freq enum: the codec was asked to skip freq decoding"
            );
            self.freq_buffer[self.doc_buffer_upto - 1]
        } else {
            1
        }
    }
}

/// Lucene912PostingsReader.prefixSum (:208-213): buffer[0] += base, then
/// running sum (matches ForDeltaUtil.decodeAndPrefixSum's net result
/// :276-283 — the SIMD-structured prefixSum8/16/32 are math-equivalent).
fn prefix_sum(buffer: &mut [u64], base: i64) {
    if buffer.is_empty() {
        return;
    }
    buffer[0] = buffer[0].wrapping_add(base as u64);
    for i in 1..buffer.len() {
        buffer[i] = buffer[i].wrapping_add(buffer[i - 1]);
    }
}

/// BlockDocsEnum.advance (:598-619): when target lies beyond the current
/// block, skip whole blocks via the level-1/level-0 skip entries instead of
/// decoding them, decode only the block that may contain target, then scan
/// the buffer (findFirstGreater :215-222). The `doc >= target` early return
/// is a lenient extension over the DISI contract (existing callers rely on
/// it); NO_MORE_DOCS stays sticky. Output sequence is identical to the
/// previous linear-advance fallback.
fn advance(core: &mut EnumCore, target: i32) -> io::Result<i32> {
    let t = target as i64;
    if core.doc >= t {
        return Ok(core.doc as i32);
    }
    if core.doc == NO_MORE_DOCS as i64 {
        return Ok(NO_MORE_DOCS);
    }
    if t > core.level0_last_doc {
        // advance skip data on level 1, then level 0
        if t > core.level1_last_doc {
            core.skip_level1_to(t)?;
        }
        core.skip_level0_to(t)?;
        if core.doc_freq - core.doc_count_upto >= BLOCK_SIZE as u32 {
            core.refill_full_block()?;
        } else {
            core.refill_remainder()?;
        }
    }
    // First buffer entry >= target, starting at doc_buffer_upto; the
    // NO_MORE_DOCS sentinel guarantees termination.
    let mut upto = core.doc_buffer_upto;
    let from = upto;
    while (core.doc_buffer[upto] as i64) < t {
        upto += 1;
    }
    core.doc = core.doc_buffer[upto] as i64;
    core.doc_buffer_upto = upto + 1;
    if let Some(pos) = &mut core.pos {
        if core.doc != NO_MORE_DOCS as i64 {
            // :1020-1025 — positions of the docs skipped inside the buffer,
            // plus the landed doc's own freq, become pending
            for i in from..=upto {
                pos.pos_pending_count += core.freq_buffer[i] as u64;
            }
            pos.position = 0;
        }
    }
    Ok(core.doc as i32)
}

/// Docs iterator (no frequencies; for IndexOptions.DOCS fields).
pub struct DocsEnum {
    core: EnumCore,
}

impl DocsEnum {
    pub fn doc_id(&self) -> i32 {
        self.core.doc as i32
    }

    pub fn next_doc(&mut self) -> io::Result<i32> {
        self.core.next_doc()
    }

    pub fn advance(&mut self, target: i32) -> io::Result<i32> {
        advance(&mut self.core, target)
    }

    /// 批量产出已解码 doc（spec 2026-07-26）：0 = 耗尽。
    pub fn next_docs(&mut self, docs: &mut [u32]) -> io::Result<usize> {
        self.core.next_docs(docs, None)
    }
}

/// Docs + freqs iterator (IndexOptions >= DOCS_AND_FREQS fields).
pub struct DocsFreqsEnum {
    core: EnumCore,
}

impl DocsFreqsEnum {
    pub fn doc_id(&self) -> i32 {
        self.core.doc as i32
    }

    pub fn next_doc(&mut self) -> io::Result<i32> {
        self.core.next_doc()
    }

    pub fn advance(&mut self, target: i32) -> io::Result<i32> {
        advance(&mut self.core, target)
    }

    /// PostingsEnum.freq(): current doc's term frequency.
    pub fn freq(&self) -> u32 {
        self.core.freq()
    }

    /// Whether `freq()` will succeed (i.e., freq blocks were decoded).
    /// Returns false for no-freq enums created via `docs_and_freqs_no_freq`.
    pub fn decodes_freqs(&self) -> bool {
        self.core.decode_freqs
    }

    /// 批量产出 doc（不解 freq）：0 = 耗尽。
    pub fn next_docs(&mut self, docs: &mut [u32]) -> io::Result<usize> {
        self.core.next_docs(docs, None)
    }

    /// 批量产出 doc + freq 同窗口。调用方先查 decodes_freqs()——
    /// no-freq 模式（docs_and_freqs_no_freq 构造）下 freq_buffer 未
    /// 物化，调用即 panic（同 freq() 的 no-freq 契约）。
    pub fn next_docs_and_freqs(
        &mut self,
        docs: &mut [u32],
        freqs: &mut [u32],
    ) -> io::Result<usize> {
        assert!(
            self.core.decode_freqs,
            "next_docs_and_freqs on no-freq enum"
        );
        self.core.next_docs(docs, Some(freqs))
    }
}

/// EverythingEnum.skipPositions (:1031-1082), positions-only profile:
/// steps over the `pos_pending_count - freq` deltas that precede the
/// current doc's positions in the .pos stream.
fn skip_positions(pos: &mut PosCore, freq: u64, total_term_freq: u64) -> io::Result<()> {
    let mut to_skip = pos.pos_pending_count - freq;
    let left_in_block = (BLOCK_SIZE - pos.pos_buffer_upto) as u64;
    if to_skip < left_in_block {
        pos.pos_buffer_upto += to_skip as usize;
    } else {
        to_skip -= left_in_block;
        while to_skip >= BLOCK_SIZE as u64 {
            pfor_util_skip(&mut pos.pos_in)?;
            to_skip -= BLOCK_SIZE as u64;
        }
        refill_positions(pos, total_term_freq)?;
        pos.pos_buffer_upto = to_skip as usize;
    }
    pos.position = 0;
    Ok(())
}

/// EverythingEnum.refillPositions (:1084-1153) without payloads/offsets:
/// tail block (fp == last_pos_block_fp) = per-delta VInts, else a PFOR
/// block (mirrors writer write_positions, postings.rs:501-521).
fn refill_positions(pos: &mut PosCore, total_term_freq: u64) -> io::Result<()> {
    if pos.pos_in.file_pointer() as i64 == pos.last_pos_block_fp {
        let count = (total_term_freq % BLOCK_SIZE as u64) as usize;
        for slot in pos.pos_delta_buffer.iter_mut().take(count) {
            *slot = pos.pos_in.read_vint()? as u32;
        }
    } else {
        let mut deltas = [0u64; BLOCK_SIZE];
        pfor_util_decode(&mut pos.pos_in, &mut deltas)?;
        for (dst, src) in pos.pos_delta_buffer.iter_mut().zip(deltas) {
            *dst = src as u32;
        }
    }
    Ok(())
}

/// Docs+freqs+positions iterator (EverythingEnum), produced by
/// [`PostingsReader::positions`].
pub struct PositionsEnum {
    core: EnumCore,
}

impl PositionsEnum {
    pub fn doc_id(&self) -> i32 {
        self.core.doc as i32
    }

    pub fn next_doc(&mut self) -> io::Result<i32> {
        self.core.next_doc()
    }

    pub fn advance(&mut self, target: i32) -> io::Result<i32> {
        advance(&mut self.core, target)
    }

    /// PostingsEnum.freq(): current doc's term frequency.
    pub fn freq(&self) -> u32 {
        self.core.freq()
    }

    /// EverythingEnum.nextPosition (:1156-1187): current doc's next
    /// position (absolute, per-doc base reset).
    pub fn next_position(&mut self) -> io::Result<u32> {
        let freq = self.freq() as u64;
        let total_term_freq = self.core.total_term_freq;
        let pos = self
            .core
            .pos
            .as_mut()
            .expect("PositionsEnum without pos state");
        assert!(
            pos.pos_pending_count > 0,
            "next_position called more than freq() times in the current doc (:1157)"
        );
        if pos.pos_pending_count > freq {
            skip_positions(pos, freq, total_term_freq)?;
            pos.pos_pending_count = freq;
        }
        if pos.pos_buffer_upto == BLOCK_SIZE {
            refill_positions(pos, total_term_freq)?;
            pos.pos_buffer_upto = 0;
        }
        pos.position += pos.pos_delta_buffer[pos.pos_buffer_upto];
        pos.pos_buffer_upto += 1;
        pos.pos_pending_count -= 1;
        Ok(pos.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::FSDirectory;
    use crate::field_infos::{FieldInfo, FieldInfos, IndexOptions};
    use crate::postings::PostingsWriter;
    use crate::terms_read::{TermEntry, TermState};
    use std::fs;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codec-lucene9-postings-{}-{}",
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

    /// kw (DOCS): "big" df=200 dense docs; "tail" df=3
    /// tx (DOCS_AND_FREQS): "hot" df=5000 dense, freqs all 1 (level-1 跨界 + 常量块);
    ///                      "warm" df=200 docs step 3, freqs 含 PFor 异常值;
    ///                      "one" df=1 singleton doc 42 freq 7
    fn write_segment(dir: &FSDirectory) -> (FieldInfos, Vec<u32>, Vec<u32>) {
        let id = [4u8; 16];
        let kw = indexed("kw", 0, IndexOptions::Docs);
        let tx = indexed("tx", 1, IndexOptions::DocsAndFreqs);
        let mut w = PostingsWriter::new(dir, "_0", &id).unwrap();
        w.start_field(&kw, 6000).unwrap();
        let big: Vec<u32> = (0..200).collect();
        w.write_term(b"big", &big, &vec![1; 200], None).unwrap();
        w.write_term(b"tail", &[10, 20, 30], &[1, 1, 1], None)
            .unwrap();
        w.finish_field().unwrap();
        w.start_field(&tx, 6000).unwrap();
        let hot: Vec<u32> = (0..5000).collect();
        w.write_term(b"hot", &hot, &vec![1; 5000], None).unwrap();
        w.write_term(b"one", &[42], &[7], None).unwrap();
        let warm_docs: Vec<u32> = (0..200).map(|i| i * 3).collect();
        let mut warm_freqs: Vec<u32> = (0..200).map(|i| (i % 5) + 1).collect();
        warm_freqs[3] = 3000;
        warm_freqs[77] = 65535;
        warm_freqs[100] = 999;
        w.write_term(b"warm", &warm_docs, &warm_freqs, None)
            .unwrap();
        w.finish_field().unwrap();
        w.finish().unwrap();
        let fis = FieldInfos::new(vec![kw, tx]);
        fis.write(dir, "_0", &id, "").unwrap();
        (fis, warm_docs, warm_freqs)
    }

    /// 直接按 TermState 手工构造 TermEntry（不走 terms dict，隔离 T6）。
    fn entry(doc_freq: u32, ttf: u64, doc_start_fp: u64, singleton: i64) -> TermEntry {
        TermEntry {
            doc_freq,
            total_term_freq: ttf,
            state: TermState {
                doc_start_fp,
                pos_start_fp: 0,
                last_pos_block_offset: -1,
                singleton_doc_id: singleton,
            },
        }
    }

    /// 从 .tim 查 term 的 TermEntry（端到端走 T6 reader）。
    fn seek(dir: &FSDirectory, fis: &FieldInfos, field: &str, term: &[u8]) -> TermEntry {
        let mut dict = crate::terms_read::TermsDict::open(dir, "_0", &[4u8; 16], fis).unwrap();
        let fi = fis.by_name(field).unwrap();
        dict.seek_exact(fi, term).unwrap().expect("term must exist")
    }

    #[test]
    fn docs_enum_sequences() {
        let root = temp_dir("docs");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, _, _) = write_segment(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        // dense 200 docs, 1 full block + tail 72
        let e = seek(&dir, &fis, "kw", b"big");
        let mut en = postings.docs(&e).unwrap();
        let mut got = Vec::new();
        loop {
            let d = en.next_doc().unwrap();
            if d == NO_MORE_DOCS {
                break;
            }
            got.push(d);
        }
        assert_eq!(got, (0..200).collect::<Vec<i32>>());
        // tail-only
        let e = seek(&dir, &fis, "kw", b"tail");
        let mut en = postings.docs(&e).unwrap();
        assert_eq!(en.next_doc().unwrap(), 10);
        assert_eq!(en.next_doc().unwrap(), 20);
        assert_eq!(en.next_doc().unwrap(), 30);
        assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn docs_freqs_across_level1_boundary() {
        let root = temp_dir("hot");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, _, _) = write_segment(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        // df=5000 dense: 39 full blocks (all-ones deltas) + tail 8, 跨过 4096 的 level-1 记录
        let e = seek(&dir, &fis, "tx", b"hot");
        let mut en = postings.docs_and_freqs(&e).unwrap();
        for expected in 0..5000 {
            assert_eq!(en.next_doc().unwrap(), expected);
            assert_eq!(en.freq(), 1);
        }
        assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn docs_freqs_with_pfor_exceptions_and_tail() {
        let root = temp_dir("warm");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, warm_docs, warm_freqs) = write_segment(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        let e = seek(&dir, &fis, "tx", b"warm");
        let mut en = postings.docs_and_freqs(&e).unwrap();
        for i in 0..200 {
            assert_eq!(en.next_doc().unwrap(), warm_docs[i] as i32, "doc {i}");
            assert_eq!(en.freq(), warm_freqs[i], "freq {i}");
        }
        assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
        // singleton: 不读 .doc，freq = totalTermFreq
        let e = seek(&dir, &fis, "tx", b"one");
        assert_eq!(e.state.singleton_doc_id, 42);
        let mut en = postings.docs_and_freqs(&e).unwrap();
        assert_eq!(en.next_doc().unwrap(), 42);
        assert_eq!(en.freq(), 7);
        assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn singleton_termstate_entry_constructed_by_hand() {
        // df==1 时 .doc 中没有任何字节（写侧 write_term 对 df==1 不写 postings）
        let root = temp_dir("single");
        let dir = FSDirectory::open(&root).unwrap();
        write_segment(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        let e = entry(1, 3, 0, 17);
        let mut en = postings.docs(&e).unwrap();
        assert_eq!(en.next_doc().unwrap(), 17);
        assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn advance_block_local_and_tail() {
        let root = temp_dir("advance");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, _, _) = write_segment(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        let e = seek(&dir, &fis, "kw", b"big");
        let mut en = postings.docs(&e).unwrap();
        assert_eq!(en.advance(57).unwrap(), 57);
        assert_eq!(en.advance(57).unwrap(), 57); // 已在目标上不动
        // target == level0_last_doc（块 0 末 doc 127）：块内扫描，不跳块
        assert_eq!(en.advance(127).unwrap(), 127);
        // target 在 tail（remainder）首 doc：跨块边界
        assert_eq!(en.advance(128).unwrap(), 128);
        assert_eq!(en.advance(199).unwrap(), 199);
        // advance 到尾块之后：NO_MORE_DOCS 且粘滞
        assert_eq!(en.advance(200).unwrap(), NO_MORE_DOCS);
        assert_eq!(en.advance(5000).unwrap(), NO_MORE_DOCS);
        fs::remove_dir_all(&root).unwrap();
    }

    /// 跨多块 skip：命中/落空/边界，与线性扫描的期望逐位一致。
    /// hot df=5000 dense（39 整块的 level-0 跳块 + 跨 4096 的 level-1）。
    #[test]
    fn advance_skips_whole_blocks() {
        let root = temp_dir("advskip");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, _, _) = write_segment(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        let e = seek(&dir, &fis, "tx", b"hot");
        // 单枚枚举上单调推进（conjunction 的使用形态）
        let mut en = postings.docs_and_freqs(&e).unwrap();
        assert_eq!(en.advance(1300).unwrap(), 1300); // 跳过块 0..10
        assert_eq!(en.freq(), 1);
        assert_eq!(en.advance(4095).unwrap(), 4095); // level-1 组末 doc
        assert_eq!(en.advance(4096).unwrap(), 4096); // 跨 level-1 边界
        assert_eq!(en.advance(4999).unwrap(), 4999); // tail 末 doc
        assert_eq!(en.advance(5000).unwrap(), NO_MORE_DOCS);
        assert_eq!(en.advance(9999).unwrap(), NO_MORE_DOCS); // 粘滞
        // 全新枚举逐 target 验证（含正好落在跳过块之后首块的 target）
        for target in [0, 1, 127, 128, 129, 4223, 4224, 4998, 4999] {
            let mut en = postings.docs_and_freqs(&e).unwrap();
            assert_eq!(en.advance(target).unwrap(), target, "target {target}");
        }
        for target in [5000, 6000, NO_MORE_DOCS - 1] {
            let mut en = postings.docs_and_freqs(&e).unwrap();
            assert_eq!(en.advance(target).unwrap(), NO_MORE_DOCS, "target {target}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    /// 落空（target 不在 postings 中）+ 跳块后 freq 指针不变式。
    /// warm df=200 docs step 3，freqs 含 PFor 异常值（i=3/77/100）。
    #[test]
    fn advance_miss_lands_on_next_doc_and_freqs_stay_aligned() {
        let root = temp_dir("advmiss");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, warm_docs, warm_freqs) = write_segment(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        let e = seek(&dir, &fis, "tx", b"warm");
        let mut en = postings.docs_and_freqs(&e).unwrap();
        assert_eq!(en.advance(9).unwrap(), 9); // PFor 异常 doc（块 0）
        assert_eq!(en.freq(), 3000);
        assert_eq!(en.advance(301).unwrap(), 303); // 落空 → 下一个 doc（块内扫描）
        assert_eq!(en.freq(), warm_freqs[101]);
        assert_eq!(en.advance(597).unwrap(), 597); // 最末 doc
        assert_eq!(en.freq(), warm_freqs[199]);
        assert_eq!(en.advance(598).unwrap(), NO_MORE_DOCS);
        // 每个 target 的 advance 结果 == 线性扫描第一个 >= target 的 doc
        for target in [0, 2, 3, 299, 300, 596, 597] {
            let mut en = postings.docs_and_freqs(&e).unwrap();
            let want = warm_docs
                .iter()
                .find(|&&d| d >= target as u32)
                .map(|&d| d as i32)
                .unwrap_or(NO_MORE_DOCS);
            assert_eq!(en.advance(target).unwrap(), want, "target {target}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    /// advance 与 next_doc 交替：跳块后 next_doc 从命中 doc 之后继续。
    #[test]
    fn advance_interleaved_with_next_doc() {
        let root = temp_dir("advmix");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, _, _) = write_segment(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        let e = seek(&dir, &fis, "tx", b"hot");
        let mut en = postings.docs_and_freqs(&e).unwrap();
        assert_eq!(en.advance(200).unwrap(), 200);
        assert_eq!(en.next_doc().unwrap(), 201);
        assert_eq!(en.advance(4096).unwrap(), 4096); // 跨 level-1
        assert_eq!(en.next_doc().unwrap(), 4097);
        assert_eq!(en.freq(), 1);
        assert_eq!(en.advance(4999).unwrap(), 4999);
        assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
        fs::remove_dir_all(&root).unwrap();
    }

    /// singleton（df=1，.doc 无字节）：advance 命中、落空、越过。
    #[test]
    fn advance_singleton() {
        let root = temp_dir("advsingle");
        let dir = FSDirectory::open(&root).unwrap();
        write_segment(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        let e = entry(1, 3, 0, 17);
        let mut en = postings.docs(&e).unwrap();
        assert_eq!(en.advance(5).unwrap(), 17);
        assert_eq!(en.advance(17).unwrap(), 17);
        assert_eq!(en.advance(18).unwrap(), NO_MORE_DOCS);
        let e = entry(1, 3, 0, 17);
        let mut en = postings.docs(&e).unwrap();
        assert_eq!(en.advance(100).unwrap(), NO_MORE_DOCS);
        fs::remove_dir_all(&root).unwrap();
    }

    /// no-freq 模式（多块 + tail）：doc 序列与 freq 解码模式逐 doc 一致，
    /// 且迭代结束后流位置相同（freq 字节记账不错位）。
    #[test]
    fn no_freq_mode_matches_decoding_mode_doc_sequence() {
        let root = temp_dir("nofreq");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, warm_docs, _) = write_segment(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();

        // hot df=5000：39 整块（跨 level-1）+ tail 8
        let e = seek(&dir, &fis, "tx", b"hot");
        let mut skip_en = postings.docs_and_freqs_no_freq(&e).unwrap();
        let mut dec_en = postings.docs_and_freqs(&e).unwrap();
        loop {
            let (a, b) = (skip_en.next_doc().unwrap(), dec_en.next_doc().unwrap());
            assert_eq!(a, b);
            if a == NO_MORE_DOCS {
                break;
            }
        }
        assert_eq!(
            skip_en.core.doc_in.file_pointer(),
            dec_en.core.doc_in.file_pointer(),
            "stream positions must agree after full iteration"
        );

        // warm df=200：1 整块（含 PFor 异常）+ tail 72
        let e = seek(&dir, &fis, "tx", b"warm");
        let mut en = postings.docs_and_freqs_no_freq(&e).unwrap();
        for i in 0..200 {
            assert_eq!(en.next_doc().unwrap(), warm_docs[i] as i32, "doc {i}");
        }
        assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
        // singleton：df=1 无文件字节，doc 命中
        let e = seek(&dir, &fis, "tx", b"one");
        let mut en = postings.docs_and_freqs_no_freq(&e).unwrap();
        assert_eq!(en.next_doc().unwrap(), 42);
        assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
        fs::remove_dir_all(&root).unwrap();
    }

    /// no-freq 模式下 advance（跳块、跨 level-1、落空）命中序列与解码模式一致，
    /// 跳块后与 next_doc 交替不错位。
    #[test]
    fn no_freq_mode_advance_matches_decoding_mode() {
        let root = temp_dir("nofreqadv");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, warm_docs, _) = write_segment(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();

        // hot：跨多块 + 跨 level-1 的目标序列，两种模式逐点一致
        let e = seek(&dir, &fis, "tx", b"hot");
        for target in [0, 1, 127, 128, 1300, 4095, 4096, 4224, 4999, 5000, 9999] {
            let mut skip_en = postings.docs_and_freqs_no_freq(&e).unwrap();
            let mut dec_en = postings.docs_and_freqs(&e).unwrap();
            assert_eq!(
                skip_en.advance(target).unwrap(),
                dec_en.advance(target).unwrap(),
                "target {target}"
            );
        }
        // advance 与 next_doc 交替：跳块后继续迭代，字节位置保持一致
        let mut skip_en = postings.docs_and_freqs_no_freq(&e).unwrap();
        let mut dec_en = postings.docs_and_freqs(&e).unwrap();
        for op in [200, 4096, 4999] {
            assert_eq!(skip_en.advance(op).unwrap(), dec_en.advance(op).unwrap());
            assert_eq!(skip_en.next_doc().unwrap(), dec_en.next_doc().unwrap());
            assert_eq!(
                skip_en.core.doc_in.file_pointer(),
                dec_en.core.doc_in.file_pointer(),
                "stream positions must agree after advance({op})"
            );
        }
        // warm：落空落在下一 doc，与线性扫描期望逐位一致
        let e = seek(&dir, &fis, "tx", b"warm");
        for target in [0, 2, 3, 299, 300, 596, 597, 598] {
            let mut en = postings.docs_and_freqs_no_freq(&e).unwrap();
            let want = warm_docs
                .iter()
                .find(|&&d| d >= target as u32)
                .map(|&d| d as i32)
                .unwrap_or(NO_MORE_DOCS);
            assert_eq!(en.advance(target).unwrap(), want, "target {target}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    /// no-freq 模式下 freq() 必须 panic（防止误用）。
    #[test]
    #[should_panic(expected = "no-freq enum")]
    fn no_freq_mode_freq_panics() {
        let root = temp_dir("nofreqpanic");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, _, _) = write_segment(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        let e = seek(&dir, &fis, "tx", b"warm");
        let mut en = postings.docs_and_freqs_no_freq(&e).unwrap();
        assert_eq!(en.next_doc().unwrap(), 0);
        fs::remove_dir_all(&root).unwrap();
        let _ = en.freq();
    }

    /// px (DOCS_AND_FREQS_AND_POSITIONS):
    /// "hot" df=5000 dense, freqs (i%3)+1, per-doc positions [0,2,..] —
    ///     ttf=9999: 78 full .pos PFOR blocks + tail 15, level-0/level-1 skips
    /// "warm" df=200 docs step 3, freqs (i%4)+1, positions [1,3,5,..] — varied deltas
    /// "one" df=1 singleton doc 42 freq 7, positions [0,1,2,3,4,5,6]
    fn write_segment_pos(dir: &FSDirectory) -> (FieldInfos, Vec<Vec<u32>>, Vec<Vec<u32>>) {
        let id = [5u8; 16];
        let px = indexed("px", 0, IndexOptions::DocsAndFreqsAndPositions);
        let mut w = PostingsWriter::new(dir, "_0", &id).unwrap();
        w.start_field(&px, 6000).unwrap();
        let hot_docs: Vec<u32> = (0..5000).collect();
        let hot_freqs: Vec<u32> = (0..5000).map(|i| (i % 3) + 1).collect();
        let hot_pos: Vec<Vec<u32>> = hot_freqs
            .iter()
            .map(|&f| (0..f).map(|k| k * 2).collect())
            .collect();
        w.write_term(b"hot", &hot_docs, &hot_freqs, Some(&hot_pos))
            .unwrap();
        let one_pos: Vec<Vec<u32>> = vec![(0..7).collect()];
        w.write_term(b"one", &[42], &[7], Some(&one_pos)).unwrap();
        let warm_docs: Vec<u32> = (0..200).map(|i| i * 3).collect();
        let warm_freqs: Vec<u32> = (0..200).map(|i| (i % 4) + 1).collect();
        let warm_pos: Vec<Vec<u32>> = warm_freqs
            .iter()
            .map(|&f| (0..f).map(|k| k * 2 + 1).collect())
            .collect();
        w.write_term(b"warm", &warm_docs, &warm_freqs, Some(&warm_pos))
            .unwrap();
        w.finish_field().unwrap();
        w.finish().unwrap();
        let fis = FieldInfos::new(vec![px]);
        fis.write(dir, "_0", &id, "").unwrap();
        (fis, hot_pos, warm_pos)
    }

    fn seek_pos(dir: &FSDirectory, fis: &FieldInfos, term: &[u8]) -> TermEntry {
        let mut dict = crate::terms_read::TermsDict::open(dir, "_0", &[5u8; 16], fis).unwrap();
        let fi = fis.by_name("px").unwrap();
        dict.seek_exact(fi, term).unwrap().expect("term must exist")
    }

    /// Drive (next_doc + freq × next_position) over the whole list.
    fn drain_positions(en: &mut PositionsEnum) -> Vec<(i32, Vec<u32>)> {
        let mut out = Vec::new();
        loop {
            let d = en.next_doc().unwrap();
            if d == NO_MORE_DOCS {
                break;
            }
            let f = en.freq();
            let mut ps = Vec::with_capacity(f as usize);
            for _ in 0..f {
                ps.push(en.next_position().unwrap());
            }
            out.push((d, ps));
        }
        out
    }

    #[test]
    fn positions_sequential_round_trip() {
        let root = temp_dir("posseq");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, hot_pos, warm_pos) = write_segment_pos(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[5u8; 16]).unwrap();
        // hot: 5000 docs dense, crosses level-1 (4096) and 38 level-0 boundaries
        let e = seek_pos(&dir, &fis, b"hot");
        assert_eq!(e.total_term_freq, 9999);
        let mut en = postings.positions(&e).unwrap();
        let got = drain_positions(&mut en);
        assert_eq!(got.len(), 5000);
        for (d, ps) in &got {
            assert_eq!(ps, &hot_pos[*d as usize], "doc {d}");
        }
        // warm: varied deltas
        let e = seek_pos(&dir, &fis, b"warm");
        let mut en = postings.positions(&e).unwrap();
        let got = drain_positions(&mut en);
        assert_eq!(got.len(), 200);
        for (i, (d, ps)) in got.iter().enumerate() {
            assert_eq!(*d, (i * 3) as i32);
            assert_eq!(ps, &warm_pos[i], "warm doc index {i}");
        }
        // singleton: no .doc bytes, positions straight from pos_start_fp
        let e = seek_pos(&dir, &fis, b"one");
        let mut en = postings.positions(&e).unwrap();
        assert_eq!(en.next_doc().unwrap(), 42);
        assert_eq!(en.freq(), 7);
        for p in 0..7 {
            assert_eq!(en.next_position().unwrap(), p);
        }
        assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn positions_advance_resync() {
        let root = temp_dir("posadv");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, hot_pos, _) = write_segment_pos(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[5u8; 16]).unwrap();
        let e = seek_pos(&dir, &fis, b"hot");
        // fresh enum, advance deep (level-1 + level-0 skip) then read positions
        let mut en = postings.positions(&e).unwrap();
        assert_eq!(en.advance(1300).unwrap(), 1300);
        assert_eq!(en.freq(), 2);
        let ps: Vec<u32> = (0..en.freq())
            .map(|_| en.next_position().unwrap())
            .collect();
        assert_eq!(ps, hot_pos[1300]);
        // advance to the level-1 boundary doc and past it
        assert_eq!(en.advance(4095).unwrap(), 4095);
        let _: Vec<u32> = (0..en.freq())
            .map(|_| en.next_position().unwrap())
            .collect();
        assert_eq!(en.advance(4096).unwrap(), 4096);
        let ps: Vec<u32> = (0..en.freq())
            .map(|_| en.next_position().unwrap())
            .collect();
        assert_eq!(ps, hot_pos[4096]);
        // into the doc tail (df % 128 != 0 region)
        assert_eq!(en.advance(4999).unwrap(), 4999);
        let ps: Vec<u32> = (0..en.freq())
            .map(|_| en.next_position().unwrap())
            .collect();
        assert_eq!(ps, hot_pos[4999]);
        assert_eq!(en.advance(5000).unwrap(), NO_MORE_DOCS);
        assert_eq!(en.advance(9999).unwrap(), NO_MORE_DOCS); // sticky
        // advance to the same doc twice must not consume positions
        let mut en = postings.positions(&e).unwrap();
        assert_eq!(en.advance(200).unwrap(), 200);
        assert_eq!(en.advance(200).unwrap(), 200);
        let ps: Vec<u32> = (0..en.freq())
            .map(|_| en.next_position().unwrap())
            .collect();
        assert_eq!(ps, hot_pos[200]);
        // every target: advance == linear scan, positions of the landed doc
        for target in [0i32, 1, 127, 128, 4223, 4224, 4998] {
            let mut en = postings.positions(&e).unwrap();
            assert_eq!(en.advance(target).unwrap(), target, "target {target}");
            let ps: Vec<u32> = (0..en.freq())
                .map(|_| en.next_position().unwrap())
                .collect();
            assert_eq!(ps, hot_pos[target as usize], "target {target}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn positions_skip_positions_catch_up() {
        // docs consumed WITHOUT reading their positions: the next
        // next_position must skip the backlog (skipPositions :1031-1082)
        let root = temp_dir("posskip");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, hot_pos, _) = write_segment_pos(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[5u8; 16]).unwrap();
        let e = seek_pos(&dir, &fis, b"hot");
        let mut en = postings.positions(&e).unwrap();
        for _ in 0..205 {
            en.next_doc().unwrap();
        }
        // now at doc 204, never read a single position
        assert_eq!(en.doc_id(), 204);
        let ps: Vec<u32> = (0..en.freq())
            .map(|_| en.next_position().unwrap())
            .collect();
        assert_eq!(ps, hot_pos[204]);
        // move on to doc 205 and read it fully, then skip 206-209's
        // positions via advance (buffer-local catch-up)
        assert_eq!(en.next_doc().unwrap(), 205);
        let ps205: Vec<u32> = (0..en.freq())
            .map(|_| en.next_position().unwrap())
            .collect();
        assert_eq!(ps205, hot_pos[205]);
        assert_eq!(en.advance(210).unwrap(), 210);
        let ps: Vec<u32> = (0..en.freq())
            .map(|_| en.next_position().unwrap())
            .collect();
        assert_eq!(ps, hot_pos[210]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn positions_missing_pos_file_is_error() {
        // a segment written without any positions field has no .pos file
        let root = temp_dir("posnone");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, _, _) = write_segment(&dir); // kw/tx, no positions
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        let e = seek(&dir, &fis, "kw", b"big");
        assert!(postings.positions(&e).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    /// 与 write_segment 同形，但开 bitmap（threshold=4096）：hot df=5000 命中，
    /// warm df=200 与 big/tail/one 不命中。
    fn write_segment_bitmap(dir: &FSDirectory) -> FieldInfos {
        let id = [4u8; 16];
        let kw = indexed("kw", 0, IndexOptions::Docs);
        let tx = indexed("tx", 1, IndexOptions::DocsAndFreqs);
        let mut w = PostingsWriter::new(dir, "_0", &id)
            .unwrap()
            .with_bitmap_threshold(Some(4096));
        w.start_field(&kw, 6000).unwrap();
        let big: Vec<u32> = (0..200).collect();
        w.write_term(b"big", &big, &vec![1; 200], None).unwrap();
        w.write_term(b"tail", &[10, 20, 30], &[1, 1, 1], None)
            .unwrap();
        w.finish_field().unwrap();
        w.start_field(&tx, 6000).unwrap();
        let hot: Vec<u32> = (0..5000).collect();
        w.write_term(b"hot", &hot, &vec![1; 5000], None).unwrap();
        w.write_term(b"one", &[42], &[7], None).unwrap();
        let warm_docs: Vec<u32> = (0..200).map(|i| i * 3).collect();
        let warm_freqs: Vec<u32> = (0..200).map(|i| (i % 5) + 1).collect();
        w.write_term(b"warm", &warm_docs, &warm_freqs, None)
            .unwrap();
        w.finish_field().unwrap();
        w.finish().unwrap();
        let fis = FieldInfos::new(vec![kw, tx]);
        fis.write(dir, "_0", &id, "").unwrap();
        fis
    }

    /// 不经任何 M3 helper，手工按布局从 .doc 原始字节定位 bitmap region：
    /// fp-4 读 len，region = [fp-4-len, fp-4)。
    fn raw_bitmap_region(dir: &FSDirectory, doc_start_fp: u64) -> Option<Vec<u8>> {
        let mut input = dir
            .open_input(&crate::postings::file_name("_0", "doc"))
            .unwrap();
        if doc_start_fp < 4 {
            return None;
        }
        input.seek(doc_start_fp - 4).unwrap();
        let len = input.read_int().unwrap() as u32;
        if len == 0 || len as u64 > crate::roaring::max_bitmap_len(6000) {
            return None;
        }
        if doc_start_fp - 4 < len as u64 {
            return None;
        }
        input.seek(doc_start_fp - 4 - len as u64).unwrap();
        let mut buf = vec![0u8; len as usize];
        input.read_bytes(&mut buf).unwrap();
        Some(buf)
    }

    #[test]
    fn inline_bitmap_region_round_trip() {
        let root = temp_dir("bitmap");
        let dir = FSDirectory::open(&root).unwrap();
        let fis = write_segment_bitmap(&dir);
        // postings 读侧零感知：open 的头/长度/footer 结构校验照常通过，
        // 命中 term 的 postings 逐 doc 不变（缝隙字节不可见，spec §4a.1）。
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        let e = seek(&dir, &fis, "tx", b"hot");
        let mut en = postings.docs_and_freqs(&e).unwrap();
        for expected in 0..5000 {
            assert_eq!(en.next_doc().unwrap(), expected);
            assert_eq!(en.freq(), 1);
        }
        assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);

        // 命中 term：docStartFP-4 处有合法 v3 bitmap region（v3 头 + cookie
        // 断言：magic/version==3/df/card、尾部 FROZEN_COOKIE、len ≤ 上界）
        let region = raw_bitmap_region(&dir, e.state.doc_start_fp).expect("hot has a bitmap");
        assert_eq!(&region[..4], b"RLBM");
        assert_eq!(region[4], 3, "format v3");
        let mut input = IndexInput::in_memory(region[5..].to_vec());
        assert_eq!(input.read_vint().unwrap(), 5000);
        assert_eq!(input.read_vint().unwrap(), 5000);
        let header = u32::from_le_bytes(region[region.len() - 4..].try_into().unwrap());
        assert_eq!(header & 0x7FFF, 13766, "FROZEN_COOKIE");
        assert!((header >> 15) >= 1, "num_containers");
        assert!(region.len() as u64 <= crate::roaring::max_bitmap_len(6000));

        // 未命中 term（df=200 < 4096）：同一手工定位流程必须校验失败
        let e = seek(&dir, &fis, "tx", b"warm");
        if let Some(region) = raw_bitmap_region(&dir, e.state.doc_start_fp) {
            assert!(
                crate::roaring::parse_region(&region, e.doc_freq).is_none(),
                "random postings bytes must not validate as a bitmap"
            );
        }

        // .doc 全流 CRC（含 bitmap 字节）与 footer 记录一致：
        // ChecksumIndexInput 顺序读完整个文件后 check_footer 通过
        // （CodecUtil.writeCRC :643-650，正确性硬性要求 (d)）。
        let raw = dir
            .open_input(&crate::postings::file_name("_0", "doc"))
            .unwrap();
        let len = raw.length();
        let mut input = crate::io::ChecksumIndexInput::new(raw);
        input.skip_bytes(len - 16).unwrap();
        crate::codec_util::check_footer(&mut input).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn no_bitmap_written_below_threshold_or_by_default() {
        // 默认构造（不开 bitmap）：hot 的 docStartFP-4 处校验必失败
        let root = temp_dir("bitmap-off");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, _, _) = write_segment(&dir);
        let e = seek(&dir, &fis, "tx", b"hot");
        if let Some(region) = raw_bitmap_region(&dir, e.state.doc_start_fp) {
            assert!(crate::roaring::parse_region(&region, e.doc_freq).is_none());
        }
        fs::remove_dir_all(&root).unwrap();
    }

    /// M5 §3 校验③：头内 df != termState.doc_freq → Ok(None) 落档
    ///（df 门的直接用例；版本门/cardinality 门各有专项）。
    #[test]
    fn open_term_bitmap_rejects_df_mismatch() {
        let root = temp_dir("bitmap-df-mismatch");
        let dir = FSDirectory::open(&root).unwrap();
        let fis = write_segment_bitmap(&dir);
        let mut e = seek(&dir, &fis, "tx", b"hot");
        e.doc_freq = 4096; // 真实 df 是 5000；4096 >= BITMAP_MIN_DF 故会走到校验③
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        assert!(postings.open_term_bitmap(&e, 6000).unwrap().is_none());
    }

    /// M5 §2/§3 frozen view 打开：view 迭代与 postings 逐 doc 一致，
    /// contains 抽样一致，cardinality == doc_freq（write_segment_bitmap
    /// 语料：tx "hot" df=5000，docs = 0..5000）。
    #[test]
    fn open_term_bitmap_matches_postings() {
        let root = temp_dir("bitmap-open");
        let dir = FSDirectory::open(&root).unwrap();
        let fis = write_segment_bitmap(&dir);
        let e = seek(&dir, &fis, "tx", b"hot");
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        let bm = postings
            .open_term_bitmap(&e, 6000)
            .unwrap()
            .expect("v3 bitmap opens");
        assert_eq!(bm.cardinality(), 5000);
        // batched iteration == postings enumeration (tx 有 freqs：docs()
        // 是 DOCS 字段布局，freq 字段须走 no-freq 枚举——doc 序列相同)
        let mut en = postings.docs_and_freqs_no_freq(&e).unwrap();
        let mut buf = [0u32; 1024];
        let mut from = 0u32;
        loop {
            let n = bm.docs_from(from, &mut buf);
            if n == 0 {
                break;
            }
            for &d in &buf[..n] {
                assert_eq!(en.next_doc().unwrap(), d as i32);
            }
            from = buf[n - 1] + 1;
        }
        assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
        // contains sampling
        for d in (0..6000u32).step_by(7) {
            assert_eq!(bm.contains(d), d < 5000, "doc {d}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    /// M5 §3 版本即迁移：v3 写侧产出的 region doctor 回 v2/v1 版本字节后，
    /// 读侧必须静默落档且 postings 逐 doc 不变（T1 期间读侧钉死落档——
    /// 本测试在 T1 确定性通过，T2 起走真实版本门，断言一字不改）。
    #[test]
    fn open_term_bitmap_falls_back_on_legacy_version() {
        let root = temp_dir("bitmap-legacy");
        let dir = FSDirectory::open(&root).unwrap();
        let fis = write_segment_bitmap(&dir);
        let e = seek(&dir, &fis, "tx", b"hot");
        let fp = e.state.doc_start_fp;
        let doc_file = root.join(crate::postings::file_name("_0", "doc"));
        let orig = fs::read(&doc_file).unwrap();
        let len =
            u32::from_le_bytes(orig[(fp - 4) as usize..fp as usize].try_into().unwrap()) as u64;
        let region_start = (fp - 4 - len) as usize;
        assert_eq!(&orig[region_start..region_start + 4], b"RLBM");
        assert_eq!(orig[region_start + 4], 3, "write side must emit v3");
        for legacy in [2u8, 1] {
            let mut bytes = orig.clone();
            bytes[region_start + 4] = legacy;
            fs::write(&doc_file, &bytes).unwrap();
            let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
            assert!(
                postings.open_term_bitmap(&e, 6000).unwrap().is_none(),
                "v{legacy} region must fall back"
            );
            // fallback correctness: the postings themselves are untouched
            let mut en = postings.docs_and_freqs(&e).unwrap();
            for expected in 0..5000 {
                assert_eq!(en.next_doc().unwrap(), expected);
                assert_eq!(en.freq(), 1);
            }
            assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
        }
        fs::remove_dir_all(&root).unwrap();
    }

    /// 批读 vs 逐 doc 全量对拍：kw:big（df=200 稠密）/ kw:tail（df=3 尾块）/
    /// tx:hot（df=5000，跨 level-1 边界 4096）/ tx:warm（df=200 步长3 +
    /// freq 异常值）/ tx:one（singleton）。多种 dst 尺寸含 1（退化）与
    /// 4096（超 level-1 组）。
    fn drain_next_docs(en: &mut DocsEnum, step: usize) -> Vec<u32> {
        let mut docs = Vec::new();
        let mut buf = vec![0u32; step];
        loop {
            let n = en.next_docs(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            docs.extend_from_slice(&buf[..n]);
        }
        docs
    }

    fn drain_per_doc(en: &mut DocsEnum) -> Vec<u32> {
        let mut docs = Vec::new();
        loop {
            let d = en.next_doc().unwrap();
            if d == NO_MORE_DOCS {
                break;
            }
            docs.push(d as u32);
        }
        docs
    }

    fn drain_per_doc_freqs(en: &mut DocsFreqsEnum) -> Vec<u32> {
        let mut docs = Vec::new();
        loop {
            let d = en.next_doc().unwrap();
            if d == NO_MORE_DOCS {
                break;
            }
            docs.push(d as u32);
        }
        docs
    }

    #[test]
    fn next_docs_matches_next_doc_all_terms() {
        let dir = temp_dir("nextdocs");
        fs::create_dir_all(&dir).unwrap();
        let fsdir = FSDirectory::open(&dir).unwrap();
        let (fis, warm_docs, warm_freqs) = write_segment(&fsdir);
        let reader = PostingsReader::open(&fsdir, "_0", &[4u8; 16]).unwrap();

        // (field, term, expect_docs, expect_freqs)
        let big: Vec<u32> = (0..200).collect();
        let hot: Vec<u32> = (0..5000).collect();
        // kw 字段（DOCS only）：reader.docs()
        let docs_cases: Vec<(&str, &[u8], Vec<u32>)> = vec![
            ("kw", b"big", big.clone()),
            ("kw", b"tail", vec![10, 20, 30]),
        ];
        for (field, term, expect_docs) in docs_cases {
            let entry = seek(&fsdir, &fis, field, term);
            for step in [1usize, 7, 128, 200, 4096] {
                let mut en = reader.docs(&entry).unwrap();
                assert_eq!(
                    drain_next_docs(&mut en, step),
                    expect_docs,
                    "{field}:{term:?} step={step} docs"
                );
            }
            let mut en = reader.docs(&entry).unwrap();
            assert_eq!(drain_per_doc(&mut en), expect_docs);
        }
        // tx 字段（DOCS_AND_FREQS）：no-freq 模式用 docs_and_freqs_no_freq
        let freqs_cases: Vec<(&str, &[u8], Vec<u32>, Option<Vec<u32>>)> = vec![
            ("tx", b"hot", hot, Some(vec![1; 5000])),
            ("tx", b"warm", warm_docs, Some(warm_freqs)),
            ("tx", b"one", vec![42], Some(vec![7])),
        ];
        for (field, term, expect_docs, expect_freqs) in freqs_cases {
            let entry = seek(&fsdir, &fis, field, term);
            for step in [1usize, 7, 128, 200, 4096] {
                let mut en = reader.docs_and_freqs_no_freq(&entry).unwrap();
                assert_eq!(
                    drain_next_docs_enum(&mut en, step),
                    expect_docs,
                    "{field}:{term:?} step={step} docs"
                );
            }
            // 逐 doc 参照路径同集
            let mut en = reader.docs_and_freqs_no_freq(&entry).unwrap();
            assert_eq!(drain_per_doc_freqs(&mut en), expect_docs);
            // freqs 对拍（仅 has_freqs 字段）
            if let Some(expect_f) = expect_freqs {
                let mut en = reader.docs_and_freqs(&entry).unwrap();
                assert!(en.decodes_freqs());
                let mut docs = Vec::new();
                let mut freqs = Vec::new();
                let (mut db, mut fb) = (vec![0u32; 64], vec![0u32; 64]);
                loop {
                    let n = en.next_docs_and_freqs(&mut db, &mut fb).unwrap();
                    if n == 0 {
                        break;
                    }
                    docs.extend_from_slice(&db[..n]);
                    freqs.extend_from_slice(&fb[..n]);
                }
                assert_eq!(docs, expect_docs, "{field}:{term:?} freq-mode docs");
                assert_eq!(freqs, expect_f, "{field}:{term:?} freqs");
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_freq_enum_does_not_decode_freqs() {
        let dir = temp_dir("nofreqbatch");
        fs::create_dir_all(&dir).unwrap();
        let fsdir = FSDirectory::open(&dir).unwrap();
        let (fis, warm_docs, _) = write_segment(&fsdir);
        let reader = PostingsReader::open(&fsdir, "_0", &[4u8; 16]).unwrap();
        let entry = seek(&fsdir, &fis, "tx", b"warm");
        let mut en = reader.docs_and_freqs_no_freq(&entry).unwrap();
        assert!(!en.decodes_freqs());
        assert_eq!(drain_next_docs_enum(&mut en, 128), warm_docs);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn drain_next_docs_enum(en: &mut DocsFreqsEnum, step: usize) -> Vec<u32> {
        let mut docs = Vec::new();
        let mut buf = vec![0u32; step];
        loop {
            let n = en.next_docs(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            docs.extend_from_slice(&buf[..n]);
        }
        docs
    }
}
