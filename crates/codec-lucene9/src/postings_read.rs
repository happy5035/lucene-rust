//! Postings (.doc) reader: Docs and DocsAndFreqs enumeration
//! (Lucene912PostingsReader BlockDocsEnum, Lucene 9.12.3).

use std::io;

use crate::codec_util::{check_footer, check_footer_structure, check_index_header};
use crate::directory::FSDirectory;
use crate::io::{DataInput, IndexInput};
use crate::postings::{file_name, DOC_CODEC, POSTINGS_VERSION, PSM_CODEC, SEGMENT_SUFFIX};
use crate::postings_ll::{for_delta_util_decode, pfor_util_decode, read_group_vints, BLOCK_SIZE};
use crate::terms_read::TermEntry;

/// DocIdSetIterator.NO_MORE_DOCS.
pub const NO_MORE_DOCS: i32 = i32::MAX;

/// Lucene912PostingsFormat.java:347-352.
const LEVEL1_NUM_DOCS: u32 = 4096;

/// Owns the segment's .doc stream (Lucene912PostingsReader :83-206).
pub struct PostingsReader {
    doc_in: IndexInput,
}

impl PostingsReader {
    /// Opens .psm + .doc, validating headers and exact lengths
    /// (Lucene912PostingsReader constructor :83-185).
    pub fn open(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16]) -> io::Result<PostingsReader> {
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
        if dir.file_exists(&file_name(segment, "pos")) {
            let _pos_len = psm.read_long()?;
        }
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
        Ok(PostingsReader { doc_in })
    }

    /// Docs iterator over a DOCS field's postings (no freq blocks on disk).
    pub fn docs(&self, entry: &TermEntry) -> io::Result<DocsEnum> {
        Ok(DocsEnum {
            core: EnumCore::new(self.fresh_input()?, entry, false)?,
        })
    }

    /// Docs+freqs iterator over a field with frequencies
    /// (IndexOptions >= DOCS_AND_FREQS).
    pub fn docs_and_freqs(&self, entry: &TermEntry) -> io::Result<DocsFreqsEnum> {
        Ok(DocsFreqsEnum {
            core: EnumCore::new(self.fresh_input()?, entry, true)?,
        })
    }

    /// An independent positioned stream over .doc (enums own their cursor).
    fn fresh_input(&self) -> io::Result<IndexInput> {
        self.doc_in.slice(0, self.doc_in.length())
    }
}

/// BlockDocsEnum state machine (:345-625), shared by the two public enums.
/// `has_freqs` selects the freq-block decode (DocsFreqsEnum) — Java decodes
/// freqs lazily on first `freq()` call; M1 decodes eagerly per block
/// (identical output, simpler control flow).
struct EnumCore {
    doc_in: IndexInput,
    doc_freq: u32,
    total_term_freq: u64,
    singleton_doc_id: i64,
    has_freqs: bool,
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
}

impl EnumCore {
    /// reset (:413-446).
    fn new(doc_in: IndexInput, entry: &TermEntry, has_freqs: bool) -> io::Result<EnumCore> {
        let mut c = EnumCore {
            doc_in,
            doc_freq: entry.doc_freq,
            total_term_freq: entry.total_term_freq,
            singleton_doc_id: entry.state.singleton_doc_id,
            has_freqs,
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
        Ok(c)
    }

    /// nextDoc (:589-596).
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS as i64 {
            return Ok(NO_MORE_DOCS);
        }
        if self.doc == self.level0_last_doc {
            self.move_to_next_level0_block()?;
        }
        self.doc = self.doc_buffer[self.doc_buffer_upto] as i64;
        self.doc_buffer_upto += 1;
        Ok(self.doc as i32)
    }

    /// moveToNextLevel0Block (:573-587).
    fn move_to_next_level0_block(&mut self) -> io::Result<()> {
        if self.doc == self.level1_last_doc {
            self.skip_level1_to(self.doc + 1)?;
        }
        self.prev_doc_id = self.level0_last_doc;
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
            self.doc_in.seek(self.level1_doc_end_fp)?;
            self.doc_count_upto = self.level1_doc_count_upto;
            self.level1_doc_count_upto += LEVEL1_NUM_DOCS;
            if self.doc_freq - self.doc_count_upto < LEVEL1_NUM_DOCS {
                self.level1_last_doc = NO_MORE_DOCS as i64;
                break;
            }
            self.level1_last_doc += self.doc_in.read_vint()? as i64;
            self.level1_doc_end_fp =
                self.doc_in.read_vlong()? as u64 + self.doc_in.file_pointer();
            if self.level1_last_doc >= target {
                if self.has_freqs {
                    let num_skip_bytes = self.doc_in.read_short()? as u16 as u64;
                    self.doc_in.skip_bytes(num_skip_bytes)?;
                }
                break;
            }
        }
        Ok(())
    }

    /// refillFullBlock (:484-499): ForDelta decode + prefix sum; freq block
    /// decoded eagerly (Java defers it to the first freq() call via freqFP).
    fn refill_full_block(&mut self) -> io::Result<()> {
        let mut deltas = [0u64; BLOCK_SIZE];
        for_delta_util_decode(&mut self.doc_in, &mut deltas)?;
        prefix_sum(&mut deltas, self.prev_doc_id);
        self.doc_buffer[..BLOCK_SIZE].copy_from_slice(&deltas);
        if self.has_freqs {
            let mut freqs = [0u64; BLOCK_SIZE];
            pfor_util_decode(&mut self.doc_in, &mut freqs)?;
            for (dst, src) in self.freq_buffer.iter_mut().zip(freqs) {
                *dst = src as u32;
            }
        }
        self.doc_count_upto += BLOCK_SIZE as u32;
        self.prev_doc_id = self.doc_buffer[BLOCK_SIZE - 1] as i64;
        self.doc_buffer_upto = 0;
        Ok(())
    }

    /// refillRemainder (:501-520) + PostingsUtil.readVIntBlock (:30-52):
    /// singleton (no file bytes at all), or a group-vint tail with
    /// freq==1 folded into the delta's low bit.
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
                    self.freq_buffer[i] = if freq_is_one == 1 {
                        1
                    } else {
                        self.doc_in.read_vint()? as u32
                    };
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

/// M1: linear advance (next_doc loop). Skip-data-driven advance arrives
/// with Boolean conjunction (search spec phase 3).
fn advance_linear(core: &mut EnumCore, target: i32) -> io::Result<i32> {
    if core.doc >= target as i64 {
        return Ok(core.doc as i32);
    }
    loop {
        let d = core.next_doc()?;
        if d >= target {
            return Ok(d);
        }
    }
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
        advance_linear(&mut self.core, target)
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
        advance_linear(&mut self.core, target)
    }

    /// PostingsEnum.freq(): current doc's term frequency.
    pub fn freq(&self) -> u32 {
        self.core.freq()
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
        w.write_term(b"tail", &[10, 20, 30], &[1, 1, 1], None).unwrap();
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
        w.write_term(b"warm", &warm_docs, &warm_freqs, None).unwrap();
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
    fn advance_is_linear_but_correct() {
        let root = temp_dir("advance");
        let dir = FSDirectory::open(&root).unwrap();
        let (fis, _, _) = write_segment(&dir);
        let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
        let e = seek(&dir, &fis, "kw", b"big");
        let mut en = postings.docs(&e).unwrap();
        assert_eq!(en.advance(57).unwrap(), 57);
        assert_eq!(en.advance(57).unwrap(), 57); // 已在目标上不动
        assert_eq!(en.advance(199).unwrap(), 199);
        assert_eq!(en.advance(200).unwrap(), NO_MORE_DOCS);
        fs::remove_dir_all(&root).unwrap();
    }
}
