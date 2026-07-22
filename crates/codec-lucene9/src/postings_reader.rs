//! Postings reader for Lucene912 format (.tip / .tim / .doc / .pos).
//! Mirrors `codecs/lucene912/Lucene912PostingsReader.java` and
//! blocktree/Lucene90BlockTreeTermsReader.java (9.12.3).
//!
//! Reads segments written by [`crate::postings::PostingsWriter`]; the
//! PostingsReader loads FSTs and IndexInputs, looks up terms via FST
//! traversal + .tim block parsing, and returns a [`PostingsEnum`] that
//! decodes .doc/.pos blocks on demand.

use std::collections::BTreeMap;
use std::io;

use crate::field_infos::{FieldInfos, IndexOptions};
use crate::fst::Fst;
use crate::io::IndexInput;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const OUTPUT_FLAG_IS_FLOOR: u64 = 0x1;
const OUTPUT_FLAG_HAS_TERMS: u64 = 0x2;

/// Sentinel matching DocIdSetIterator.NO_MORE_DOCS (Integer.MAX_VALUE).
pub const NO_MORE_DOCS: i32 = i32::MAX;

const BLOCK_SIZE: usize = 128;

// ---------------------------------------------------------------------------
// Byte-slice helpers
// ---------------------------------------------------------------------------

fn read_msb_vlong(bytes: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    loop {
        let b = bytes[*pos];
        *pos += 1;
        v = (v << 7) | ((b & 0x7F) as u64);
        if b & 0x80 == 0 {
            break;
        }
    }
    v
}

fn read_slice_vlong(bytes: &[u8], pos: &mut usize) -> i64 {
    let b = bytes[*pos];
    *pos += 1;
    if b & 0x80 == 0 {
        return b as i64;
    }
    let mut v = (b & 0x7F) as i64;
    let mut shift = 7;
    loop {
        let b = bytes[*pos];
        *pos += 1;
        v |= ((b & 0x7F) as i64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            break;
        }
    }
    v
}

fn read_slice_vint(bytes: &[u8], pos: &mut usize) -> i32 {
    read_slice_vlong(bytes, pos) as i32
}

// ---------------------------------------------------------------------------
// IndexInput helpers
// ---------------------------------------------------------------------------

fn input_read_vint15(input: &mut dyn IndexInput) -> io::Result<u32> {
    let lo = input.read_byte()? as u32;
    let hi = input.read_byte()? as u32;
    let v = lo | (hi << 8);
    if (v & 0x8000) != 0 {
        let high = input.read_vlong()? as u64;
        Ok((v & 0x7FFF) | ((high as u32) << 15))
    } else {
        Ok(v as u32)
    }
}

fn input_read_vlong15(input: &mut dyn IndexInput) -> io::Result<u64> {
    let lo = input.read_byte()? as u64;
    let hi = input.read_byte()? as u64;
    let v = lo | (hi << 8);
    if (v & 0x8000) != 0 {
        let high = input.read_vlong()? as u64;
        Ok((v & 0x7FFF) | (high << 15))
    } else {
        Ok(v)
    }
}

// ---------------------------------------------------------------------------
// Term metadata
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TermState {
    doc_start_fp: u64,
    pos_start_fp: u64,
    last_pos_block_offset: i64,
    singleton_doc_id: i64,
}

impl Default for TermState {
    fn default() -> Self {
        TermState {
            doc_start_fp: 0,
            pos_start_fp: 0,
            last_pos_block_offset: 0,
            singleton_doc_id: -1, // EMPTY_STATE sentinel, matches writer
        }
    }
}

struct TermStats {
    doc_freq: u32,
    total_term_freq: u64,
}

// ---------------------------------------------------------------------------
// FST output decoding
// ---------------------------------------------------------------------------

struct BlockEntry {
    fp: u64,
    has_terms: bool,
    is_floor: bool,
    floor_lead_labels: Vec<u8>,
    floor_fps: Vec<u64>,
}

fn decode_fst_output(output: &[u8]) -> BlockEntry {
    let mut pos = 0;
    let encoded = read_msb_vlong(output, &mut pos);
    let fp = encoded >> 2;
    let has_terms = (encoded & OUTPUT_FLAG_HAS_TERMS) != 0;
    let is_floor = (encoded & OUTPUT_FLAG_IS_FLOOR) != 0;

    let mut floor_lead_labels = Vec::new();
    let mut floor_fps = Vec::new();

    if is_floor && pos < output.len() {
        let num_sub = read_slice_vint(output, &mut pos) as usize + 1;
        for _ in 1..num_sub {
            if pos >= output.len() {
                break;
            }
            let lead = output[pos];
            pos += 1;
            let delta = read_slice_vlong(output, &mut pos);
            let sub_fp = fp.wrapping_add(((delta >> 1) as i64) as u64);
            floor_lead_labels.push(lead);
            floor_fps.push(sub_fp);
        }
    }

    BlockEntry {
        fp,
        has_terms,
        is_floor,
        floor_lead_labels,
        floor_fps,
    }
}

// ---------------------------------------------------------------------------
// .tim block parsing
// ---------------------------------------------------------------------------

/// Decodes one encode_term record from the metadata blob, mutating `last`
/// and returning the decoded state (which equals `last` after update).
fn decode_one_term_meta(
    bytes: &[u8],
    pos: &mut usize,
    last: &mut TermState,
    has_positions: bool,
) -> TermState {
    let v = read_slice_vlong(bytes, pos);

    if last.singleton_doc_id != -1 && (v & 1) != 0 {
        // Consecutive singletons sharing same docStartFP: zigzag delta.
        let zigzag = (v >> 1) as i64;
        let delta = (zigzag >> 1) ^ -(zigzag & 1);
        last.singleton_doc_id += delta;
    } else {
        last.doc_start_fp = last.doc_start_fp.wrapping_add((v >> 1) as u64);
        if (v & 1) != 0 {
            last.singleton_doc_id = read_slice_vint(bytes, pos) as i64;
        } else {
            last.singleton_doc_id = -1;
        }
    }

    if has_positions {
        last.pos_start_fp = last
            .pos_start_fp
            .wrapping_add(read_slice_vlong(bytes, pos) as u64);
        // lastPosBlockOffset is always written (0 sentinel when absent),
        // so no ambiguous peek is needed.
        let offset = read_slice_vlong(bytes, pos);
        last.last_pos_block_offset = if offset == 0 { -1 } else { offset };
    }

    last.clone()
}

/// Stats decoder: handles singleton runs inline so that each call to `.next()`
/// yields the stats for exactly one term entry.
struct StatsDecoder<'a> {
    bytes: &'a [u8],
    pos: usize,
    has_freqs: bool,
    singleton_remaining: u32,
}

impl<'a> StatsDecoder<'a> {
    fn new(bytes: &'a [u8], has_freqs: bool) -> Self {
        StatsDecoder {
            bytes,
            pos: 0,
            has_freqs,
            singleton_remaining: 0,
        }
    }

    fn next(&mut self) -> TermStats {
        if self.singleton_remaining > 0 {
            self.singleton_remaining -= 1;
            return TermStats {
                doc_freq: 1,
                total_term_freq: if self.has_freqs { 1 } else { 1 },
            };
        }
        if self.pos >= self.bytes.len() {
            return TermStats {
                doc_freq: 0,
                total_term_freq: 0,
            };
        }
        let v = read_slice_vint(self.bytes, &mut self.pos);
        if (v & 1) != 0 {
            let count = ((v >> 1) as u32) + 1;
            self.singleton_remaining = count - 1;
            TermStats {
                doc_freq: 1,
                total_term_freq: if self.has_freqs { 1 } else { 1 },
            }
        } else {
            let df = (v >> 1) as u32;
            let ttf = if self.has_freqs {
                (df as u64).wrapping_add(read_slice_vlong(self.bytes, &mut self.pos) as u64)
            } else {
                df as u64
            };
            TermStats {
                doc_freq: df,
                total_term_freq: ttf,
            }
        }
    }
}

/// Read one .tim block and look up `term_suffix`.
fn read_tim_block_for_term(
    input: &mut dyn IndexInput,
    fp: u64,
    term_suffix: &[u8],
    has_freqs: bool,
    has_positions: bool,
) -> io::Result<Option<(TermStats, TermState)>> {
    input.seek(fp)?;

    // Block header
    let code = input.read_vint()? as usize;
    let num_entries = code >> 1;
    if num_entries == 0 {
        return Ok(None);
    }

    // Suffix blob token + bytes
    let token = input.read_vlong()? as u64;
    let suffix_bytes_len = (token >> 3) as usize;
    let is_leaf = (token & 0x04) != 0;

    let mut suffix_bytes = vec![0u8; suffix_bytes_len];
    input.read_bytes(&mut suffix_bytes, 0, suffix_bytes_len)?;

    // Suffix lengths (raw bytes, possibly compressed)
    let lengths_raw = {
        let code_or_len = input.read_vint()?;
        if code_or_len & 1 != 0 {
            let n = (code_or_len >> 1) as usize;
            let b = input.read_byte()?;
            vec![b; n]
        } else {
            let n = (code_or_len >> 1) as usize;
            if n == 0 {
                Vec::new()
            } else {
                let mut raw = vec![0u8; n];
                input.read_bytes(&mut raw, 0, n)?;
                raw
            }
        }
    };

    // Stats blob
    let stats_len = input.read_vint()? as usize;
    let mut stats_bytes = vec![0u8; stats_len];
    input.read_bytes(&mut stats_bytes, 0, stats_len)?;

    // Metadata blob
    let meta_len = input.read_vint()? as usize;
    let mut meta_bytes = vec![0u8; meta_len];
    input.read_bytes(&mut meta_bytes, 0, meta_len)?;

    // Iterate entries
    let mut stats_decoder = StatsDecoder::new(&stats_bytes, has_freqs);
    let mut suffix_cursor: usize = 0;
    let mut len_pos: usize = 0;
    let mut meta_pos: usize = 0;
    let mut last_state = TermState::default();

    for _entry_idx in 0..num_entries {
        // Read suffix length from lengths_raw
        let len_encoded = read_slice_vint(&lengths_raw, &mut len_pos);
        let is_sub_block = !is_leaf && (len_encoded & 1) != 0;
        let suffix_len = if is_leaf {
            len_encoded as usize
        } else {
            (len_encoded >> 1) as usize
        };

        let suffix = &suffix_bytes[suffix_cursor..suffix_cursor + suffix_len];
        suffix_cursor += suffix_len;

        if is_sub_block {
            // Skip back-pointer VLong
            read_slice_vlong(&lengths_raw, &mut len_pos);
            continue;
        }

        // Term entry
        let stats = stats_decoder.next();
        let state =
            decode_one_term_meta(&meta_bytes, &mut meta_pos, &mut last_state, has_positions);

        if suffix == term_suffix {
            return Ok(Some((stats, state)));
        }

        // Entries are sorted; if we passed the term, it doesn't exist
        if suffix.as_ref() > term_suffix {
            return Ok(None);
        }
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// .doc block reading
// ---------------------------------------------------------------------------

struct DocBlock {
    docs: Vec<u32>,
    freqs: Vec<u32>,
}

impl DocBlock {
    fn new() -> Self {
        DocBlock {
            docs: Vec::new(),
            freqs: Vec::new(),
        }
    }
}

fn skip_level1_header(
    input: &mut dyn IndexInput,
    has_freqs: bool,
    has_positions: bool,
) -> io::Result<()> {
    input.read_vint()?; // doc_delta
    if has_freqs {
        let _level1_len = input.read_vlong()?;
        // scratch_len is LE short (2 bytes)
        let slo = input.read_byte()? as u64;
        let shi = input.read_byte()? as u64;
        let scratch_len = slo | (shi << 8);
        // num_impacts is LE short
        let _nlo = input.read_byte()?;
        let _nhi = input.read_byte()?;
        let impact_bytes = scratch_len.saturating_sub(2);
        let mut skip = vec![0u8; impact_bytes as usize];
        input.read_bytes(&mut skip, 0, impact_bytes as usize)?;
        if has_positions {
            input.read_vlong()?;
            input.read_byte()?;
        }
    } else {
        input.read_vlong()?;
    }
    Ok(())
}

fn read_full_block(
    input: &mut dyn IndexInput,
    block: &mut DocBlock,
    last_doc: &mut i32,
    has_freqs: bool,
    has_positions: bool,
) -> io::Result<()> {
    let _num_skip = input.read_vlong()?;
    let _doc_delta = input_read_vint15(input)?;
    let _level0_block_len = input_read_vlong15(input)?;

    if has_freqs {
        let impact_bytes = input.read_vlong()?;
        let mut skip = vec![0u8; impact_bytes as usize];
        input.read_bytes(&mut skip, 0, impact_bytes as usize)?;
        if has_positions {
            input.read_vlong()?;
            input.read_byte()?;
        }
    }

    // FOR doc deltas
    let for_header = input.read_byte()?;
    let mut doc_deltas = [0u32; 128];
    if for_header == 0 {
        doc_deltas.fill(1);
    } else {
        let for_size = for_header as usize * 16;
        let mut for_bytes = vec![0u8; for_size];
        input.read_bytes(&mut for_bytes, 0, for_size)?;
        crate::postings_ll::for_util_decode(&for_bytes, for_header, &mut doc_deltas);
    }

    // PFOR freqs
    let mut freq_values = [0u32; 128];
    if has_freqs {
        let token = input.read_byte()?;
        let num_exceptions = (token >> 5) as usize;
        let patched = token & 0x1F;
        if patched > 0 {
            let body_len = patched as usize * 16;
            let mut for_bytes = vec![0u8; body_len];
            input.read_bytes(&mut for_bytes, 0, body_len)?;
            crate::postings_ll::for_util_decode(&for_bytes, patched, &mut freq_values);
            for _ in 0..num_exceptions {
                let exc_pos = input.read_byte()? as usize;
                let high_bits = input.read_byte()? as u32;
                freq_values[exc_pos] |= high_bits << patched;
            }
        } else {
            let base = input.read_vlong()? as u32;
            freq_values.fill(base);
            for _ in 0..num_exceptions {
                let exc_pos = input.read_byte()? as usize;
                let high_bits = input.read_byte()? as u32;
                freq_values[exc_pos] |= high_bits;
            }
        }
    } else {
        freq_values.fill(1);
    }

    let mut d = *last_doc;
    for i in 0..128 {
        d = d.wrapping_add(doc_deltas[i] as i32);
        block.docs.push(d as u32);
        block.freqs.push(freq_values[i]);
    }
    *last_doc = d;

    Ok(())
}

fn read_tail_block(
    input: &mut dyn IndexInput,
    block: &mut DocBlock,
    count: usize,
    last_doc: &mut i32,
    has_freqs: bool,
) -> io::Result<()> {
    let full_groups = count / 4;
    let tail = count % 4;
    let mut encoded = Vec::with_capacity(count);

    for _ in 0..full_groups {
        let flag = input.read_byte()?;
        for i in 0..4 {
            let size = ((flag >> (6 - 2 * i)) & 3) as usize + 1;
            let mut raw: u32 = 0;
            let mut buf = [0u8; 4];
            input.read_bytes(&mut buf, 0, size)?;
            for j in 0..size {
                raw |= (buf[j] as u32) << (j * 8);
            }
            encoded.push(raw);
        }
    }
    for _ in 0..tail {
        encoded.push(input.read_vint()? as u32);
    }

    let mut d = *last_doc;
    for &enc in &encoded {
        if has_freqs {
            let delta = enc >> 1;
            let freq_is_one = (enc & 1) != 0;
            d = d.wrapping_add(delta as i32);
            block.docs.push(d as u32);
            if freq_is_one {
                block.freqs.push(1);
            } else {
                block.freqs.push(input.read_vint()? as u32);
            }
        } else {
            d = d.wrapping_add(enc as i32);
            block.docs.push(d as u32);
            block.freqs.push(1);
        }
    }
    *last_doc = d;

    Ok(())
}

/// Read all doc/freq data for a term.
fn read_all_docs(
    input: &mut dyn IndexInput,
    doc_start_fp: u64,
    singleton_doc_id: i64,
    doc_freq: u32,
    total_term_freq: u64,
    has_freqs: bool,
    has_positions: bool,
) -> io::Result<(Vec<u32>, Vec<u32>)> {
    if singleton_doc_id != -1 {
        let freq = if has_freqs {
            total_term_freq as u32
        } else {
            1
        };
        return Ok((vec![singleton_doc_id as u32], vec![freq]));
    }

    input.seek(doc_start_fp)?;

    let mut docs = Vec::with_capacity(doc_freq as usize);
    let mut freqs = Vec::with_capacity(doc_freq as usize);
    let mut last_doc: i32 = -1;
    let mut docs_read = 0u32;

    while docs_read < doc_freq {
        if docs_read > 0 && docs_read % 4096 == 0 {
            skip_level1_header(input, has_freqs, has_positions)?;
        }

        let remaining = (doc_freq - docs_read) as usize;
        let mut block = DocBlock::new();

        if remaining >= 128 {
            read_full_block(input, &mut block, &mut last_doc, has_freqs, has_positions)?;
        } else {
            read_tail_block(input, &mut block, remaining, &mut last_doc, has_freqs)?;
        }

        docs_read += block.docs.len() as u32;
        docs.extend(block.docs);
        freqs.extend(block.freqs);
    }

    Ok((docs, freqs))
}

// ---------------------------------------------------------------------------
// Position reading (.pos)
// ---------------------------------------------------------------------------

/// Read all positions for a term into a flat array. Positions are absolute
/// within each doc (deltas reset per doc in the writer).
fn read_all_positions(
    input: &mut dyn IndexInput,
    pos_start_fp: u64,
    total_term_freq: u64,
    freqs: &[u32],
) -> io::Result<Vec<u32>> {
    if total_term_freq == 0 || freqs.is_empty() {
        return Ok(Vec::new());
    }

    input.seek(pos_start_fp)?;

    let ttf = total_term_freq as usize;
    let full_chunks = ttf / BLOCK_SIZE;
    let tail_count = ttf % BLOCK_SIZE;

    let mut deltas = Vec::with_capacity(ttf);

    // Decode PFOR blocks (each produces 128 position deltas)
    for _ in 0..full_chunks {
        let token = input.read_byte()?;
        let num_exceptions = (token >> 5) as usize;
        let patched = token & 0x1F;

        let mut block = [0u32; BLOCK_SIZE];
        if patched > 0 {
            let body_len = patched as usize * 16;
            let mut for_bytes = vec![0u8; body_len];
            input.read_bytes(&mut for_bytes, 0, body_len)?;
            crate::postings_ll::for_util_decode(&for_bytes, patched, &mut block);
            for _ in 0..num_exceptions {
                let exc_pos = input.read_byte()? as usize;
                let high_bits = input.read_byte()? as u32;
                block[exc_pos] |= high_bits << patched;
            }
        } else {
            let base = input.read_vlong()? as u32;
            block.fill(base);
            for _ in 0..num_exceptions {
                let exc_pos = input.read_byte()? as usize;
                let high_bits = input.read_byte()? as u32;
                block[exc_pos] |= high_bits;
            }
        }
        deltas.extend_from_slice(&block);
    }

    // Tail VInts
    for _ in 0..tail_count {
        deltas.push(input.read_vint()? as u32);
    }

    // Decode deltas to absolute positions, resetting `last` per doc.
    let mut positions = Vec::with_capacity(ttf);
    let mut delta_idx = 0usize;
    for &freq in freqs {
        let f = freq as usize;
        let mut last = 0u32;
        for _ in 0..f {
            if delta_idx < deltas.len() {
                last += deltas[delta_idx];
                positions.push(last);
                delta_idx += 1;
            }
        }
    }

    Ok(positions)
}

// ---------------------------------------------------------------------------
// PostingsEnum
// ---------------------------------------------------------------------------

/// Iterator over one term's postings.
pub enum PostingsEnum {
    Docs {
        docs: Vec<u32>,
        cursor: usize,
    },
    DocsAndFreqs {
        docs: Vec<u32>,
        freqs: Vec<u32>,
        cursor: usize,
    },
    DocsFreqsPositions {
        docs: Vec<u32>,
        freqs: Vec<u32>,
        positions: Vec<u32>,
        cursor: usize,
        pos_idx: usize,
        pos_base: usize,
    },
}

impl PostingsEnum {
    /// Current document, or -1 if not yet positioned.
    pub fn doc_id(&self) -> i32 {
        let (docs, cursor) = match self {
            PostingsEnum::Docs { docs, cursor } => (docs, *cursor),
            PostingsEnum::DocsAndFreqs { docs, cursor, .. } => (docs, *cursor),
            PostingsEnum::DocsFreqsPositions { docs, cursor, .. } => (docs, *cursor),
        };
        if cursor == 0 || cursor > docs.len() {
            -1
        } else {
            docs[cursor - 1] as i32
        }
    }

    /// Advance to next doc. Returns NO_MORE_DOCS when exhausted.
    pub fn next_doc(&mut self) -> io::Result<i32> {
        match self {
            PostingsEnum::Docs { docs, cursor } => {
                if *cursor >= docs.len() {
                    return Ok(NO_MORE_DOCS);
                }
                let doc = docs[*cursor] as i32;
                *cursor += 1;
                Ok(doc)
            }
            PostingsEnum::DocsAndFreqs { docs, cursor, .. } => {
                if *cursor >= docs.len() {
                    return Ok(NO_MORE_DOCS);
                }
                let doc = docs[*cursor] as i32;
                *cursor += 1;
                Ok(doc)
            }
            PostingsEnum::DocsFreqsPositions {
                docs,
                freqs,
                cursor,
                pos_idx,
                pos_base,
                ..
            } => {
                if *cursor >= docs.len() {
                    return Ok(NO_MORE_DOCS);
                }
                if *cursor > 0 {
                    *pos_base += freqs[*cursor - 1] as usize;
                }
                *pos_idx = 0;
                let doc = docs[*cursor] as i32;
                *cursor += 1;
                Ok(doc)
            }
        }
    }

    /// Skip to first doc >= target.
    pub fn advance(&mut self, target: i32) -> io::Result<i32> {
        loop {
            let doc = self.next_doc()?;
            if doc == NO_MORE_DOCS || doc >= target {
                return Ok(doc);
            }
        }
    }

    /// Term frequency of current doc (1 for Docs variant).
    pub fn freq(&self) -> u32 {
        match self {
            PostingsEnum::Docs { .. } => 1,
            PostingsEnum::DocsAndFreqs { freqs, cursor, .. }
            | PostingsEnum::DocsFreqsPositions { freqs, cursor, .. } => {
                if *cursor > 0 && *cursor <= freqs.len() {
                    freqs[*cursor - 1]
                } else {
                    0
                }
            }
        }
    }

    /// Next position in current doc (DocsFreqsPositions only).
    pub fn next_position(&mut self) -> io::Result<u32> {
        match self {
            PostingsEnum::Docs { .. } | PostingsEnum::DocsAndFreqs { .. } => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "next_position called on variant without positions",
            )),
            PostingsEnum::DocsFreqsPositions {
                positions,
                pos_idx,
                pos_base,
                ..
            } => {
                let abs = *pos_base + *pos_idx;
                if abs >= positions.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "no more positions for current document",
                    ));
                }
                let pos = positions[abs];
                *pos_idx += 1;
                Ok(pos)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PostingsReader
// ---------------------------------------------------------------------------

struct FieldReader {
    fst: Fst,
    has_freqs: bool,
    has_positions: bool,
}

/// Reader for Lucene912 postings format.
pub struct PostingsReader {
    doc_input: Box<dyn IndexInput>,
    pos_input: Option<Box<dyn IndexInput>>,
    tim_input: Box<dyn IndexInput>,
    fields: BTreeMap<String, FieldReader>,
}

impl PostingsReader {
    /// Open postings reader for `segment`.
    ///
    /// Parses `.tmd` field records, loads FSTs from `.tip`, opens `.doc`,
    /// `.pos` (optional), and `.tim`.
    pub fn open(
        dir: &crate::directory::FSDirectory,
        segment: &str,
        suffix: &str,
        field_infos: &FieldInfos,
    ) -> io::Result<Self> {
        let file_prefix = format!("{segment}{suffix}");

        let doc_input = dir.open_input(&format!("{file_prefix}.doc"))?;
        let pos_input = dir.open_input(&format!("{file_prefix}.pos")).ok();
        let tim_input = dir.open_input(&format!("{file_prefix}.tim"))?;
        let mut tip_input = dir.open_input(&format!("{file_prefix}.tip"))?;
        let mut tmd_input = dir.open_input(&format!("{file_prefix}.tmd"))?;

        let fields = Self::parse_tmd(&mut tmd_input, &mut tip_input, field_infos)?;

        Ok(PostingsReader {
            doc_input,
            pos_input,
            tim_input,
            fields,
        })
    }

    /// Parse .tmd to extract field records and load FSTs.
    fn parse_tmd(
        tmd: &mut Box<dyn IndexInput>,
        tip: &mut Box<dyn IndexInput>,
        field_infos: &FieldInfos,
    ) -> io::Result<BTreeMap<String, FieldReader>> {
        // .tmd layout (Lucene90BlockTreeTermsWriter):
        //   [TMD_CODEC index header]
        //   [TERMS_CODEC index header (PostingsHeader)]
        //   [VInt: blockSize]
        //   [VInt: numFields]
        //   [field record 0] ... [field record N-1]
        //   [VLong: indexLength]
        //   [VLong: termsLength]
        //   [footer]
        crate::codec_util::skip_index_header(tmd.as_mut())?;
        crate::codec_util::skip_index_header(tmd.as_mut())?;
        let _block_size = tmd.read_vint()?;
        let num_fields = tmd.read_vint()? as usize;
        let mut fields = BTreeMap::new();

        for _ in 0..num_fields {
            let field_number = tmd.read_vint()?;
            let _num_terms = tmd.read_vlong()? as u64;

            let root_code_len = tmd.read_vint()? as usize;
            let mut _root_code = vec![0u8; root_code_len];
            tmd.read_bytes(&mut _root_code, 0, root_code_len)?;

            let (field_name, has_freqs, has_positions) = field_infos
                .by_number(field_number)
                .map(|fi| {
                    let hf = fi.index_options != IndexOptions::Docs
                        && fi.index_options != IndexOptions::None;
                    let hp = matches!(
                        fi.index_options,
                        IndexOptions::DocsAndFreqsAndPositions
                            | IndexOptions::DocsAndFreqsAndPositionsAndOffsets
                    );
                    (fi.name.clone(), hf, hp)
                })
                .unwrap_or_else(|| (format!("_field_{field_number}"), false, false));

            if has_freqs {
                let _sum_ttf = tmd.read_vlong()? as u64;
            }
            let _sum_doc_freq = tmd.read_vlong()? as u64;
            let _doc_count = tmd.read_vint()? as u32;

            let min_len = tmd.read_vint()? as usize;
            let mut _min_term = vec![0u8; min_len];
            tmd.read_bytes(&mut _min_term, 0, min_len)?;

            let max_len = tmd.read_vint()? as usize;
            let mut _max_term = vec![0u8; max_len];
            tmd.read_bytes(&mut _max_term, 0, max_len)?;

            let index_start_fp = tmd.read_vlong()? as u64;

            // Read FST metadata from .tmd
            let fst = Self::load_fst_from_tmd(tmd, tip, index_start_fp)?;

            fields.insert(
                field_name,
                FieldReader {
                    fst,
                    has_freqs,
                    has_positions,
                },
            );
        }

        Ok(fields)
    }

    /// Read FST metadata from the .tmd field record and load bytes from .tip.
    fn load_fst_from_tmd(
        tmd: &mut Box<dyn IndexInput>,
        tip: &mut Box<dyn IndexInput>,
        index_start_fp: u64,
    ) -> io::Result<Fst> {
        // Codec header: BE magic + string codec + BE version
        let mut magic = [0u8; 4];
        tmd.read_bytes(&mut magic, 0, 4)?;

        let codec_len = tmd.read_vint()? as usize;
        let mut _codec_bytes = vec![0u8; codec_len];
        tmd.read_bytes(&mut _codec_bytes, 0, codec_len)?;

        let mut ver = [0u8; 4];
        tmd.read_bytes(&mut ver, 0, 4)?;

        // Empty output
        let has_empty = tmd.read_byte()?;
        let empty_output = if has_empty != 0 {
            let len = tmd.read_vint()? as usize;
            let mut buf = vec![0u8; len];
            tmd.read_bytes(&mut buf, 0, len)?;
            buf.reverse();
            let mut pos = 0usize;
            let elen = read_slice_vint(&buf, &mut pos) as usize;
            if elen > 0 && pos + elen <= buf.len() {
                Some(buf[pos..pos + elen].to_vec())
            } else {
                Some(Vec::new())
            }
        } else {
            None
        };

        let _input_type = tmd.read_byte()?;
        let start_node = tmd.read_vlong()? as u64;
        let num_bytes = tmd.read_vlong()? as u64;

        // Load FST bytes from .tip
        tip.seek(index_start_fp)?;
        let mut fst_bytes = vec![0u8; num_bytes as usize];
        tip.read_bytes(&mut fst_bytes, 0, num_bytes as usize)?;

        Ok(Fst::from_parts(
            fst_bytes,
            start_node,
            num_bytes,
            empty_output,
        ))
    }

    /// Look up a term and return a PostingsEnum.
    pub fn read_term(
        &mut self,
        field: &str,
        term: &[u8],
    ) -> io::Result<Option<PostingsEnum>> {
        let fr = match self.fields.get(field) {
            Some(fr) => fr,
            None => return Ok(None),
        };

        // Walk FST to find block containing this term.
        let block_out = match find_fst_block(&fr.fst, term) {
            Some(o) => o,
            None => return Ok(None),
        };

        let entry = decode_fst_output(&block_out);
        if !entry.has_terms {
            return Ok(None);
        }

        let prefix_len = find_block_prefix_len(&fr.fst, term);
        let suffix = if prefix_len < term.len() {
            &term[prefix_len..]
        } else {
            &[]
        };

        // Resolve floor sub-block if applicable
        let fp = if entry.is_floor && !suffix.is_empty() {
            let lead = suffix[0];
            entry
                .floor_lead_labels
                .iter()
                .position(|&l| l == lead)
                .map(|i| entry.floor_fps[i])
                .unwrap_or(entry.fp)
        } else {
            entry.fp
        };

        let (stats, term_state) = match read_tim_block_for_term(
            self.tim_input.as_mut(),
            fp,
            suffix,
            fr.has_freqs,
            fr.has_positions,
        )? {
            Some(t) => t,
            None => return Ok(None),
        };

        // Read .doc data
        let (docs, freqs) = read_all_docs(
            self.doc_input.as_mut(),
            term_state.doc_start_fp,
            term_state.singleton_doc_id,
            stats.doc_freq,
            stats.total_term_freq,
            fr.has_freqs,
            fr.has_positions,
        )?;

        if fr.has_positions && stats.total_term_freq > 1 {
            let pos_input = self.pos_input.as_mut().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "positions requested but .pos file missing",
                )
            })?;
            let positions = read_all_positions(
                pos_input.as_mut(),
                term_state.pos_start_fp,
                stats.total_term_freq,
                &freqs,
            )?;
            Ok(Some(PostingsEnum::DocsFreqsPositions {
                docs,
                freqs,
                positions,
                cursor: 0,
                pos_idx: 0,
                pos_base: 0,
            }))
        } else if fr.has_freqs {
            Ok(Some(PostingsEnum::DocsAndFreqs {
                docs,
                freqs,
                cursor: 0,
            }))
        } else {
            Ok(Some(PostingsEnum::Docs { docs, cursor: 0 }))
        }
    }
}

// ---------------------------------------------------------------------------
// FST block search
// ---------------------------------------------------------------------------

/// Walk FST along `term`, returning the output of the deepest final arc
/// encountered, or the FST's empty_output when no final arc matches (the term
/// falls under the root block whose prefix is empty).
fn find_fst_block(fst: &Fst, term: &[u8]) -> Option<Vec<u8>> {
    use crate::fst::read_node;

    if term.is_empty() {
        return fst.empty_output().map(|o| o.to_vec());
    }

    let mut node = fst.start_node() as i64;
    let mut out = Vec::new();
    let mut last: Option<Vec<u8>> = None;

    for &b in term {
        if node <= 0 {
            break;
        }
        let (arcs, _) = read_node(fst.bytes(), node as u64);
        let arc = match arcs.iter().find(|a| a.label == b) {
            Some(a) => a,
            None => break,
        };
        if let Some(o) = &arc.output {
            out.extend_from_slice(o);
        }
        if arc.is_final {
            let mut final_out = out.clone();
            if let Some(fo) = &arc.final_output {
                final_out.extend_from_slice(fo);
            }
            last = Some(final_out);
        }
        node = arc.target;
    }

    // Fall back to the root block (empty prefix) when no finer-grained block
    // matches. The root block's pointer is stored as the FST's empty_output.
    last.or_else(|| fst.empty_output().map(|o| o.to_vec()))
}

/// Walk FST along `term`, returning the length of the longest prefix that
/// matched a final arc (block prefix).
fn find_block_prefix_len(fst: &Fst, term: &[u8]) -> usize {
    use crate::fst::read_node;

    if term.is_empty() {
        return 0;
    }

    let mut node = fst.start_node() as i64;
    let mut last = 0usize;

    for (i, &b) in term.iter().enumerate() {
        if node <= 0 {
            break;
        }
        let (arcs, _) = read_node(fst.bytes(), node as u64);
        let arc = match arcs.iter().find(|a| a.label == b) {
            Some(a) => a,
            None => break,
        };
        if arc.is_final {
            last = i + 1;
        }
        node = arc.target;
    }

    last
}

// ============================================================================
// Round-trip tests: PostingsWriter → PostingsReader
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::FSDirectory;
    use crate::field_infos::{FieldInfo, FieldInfos, IndexOptions};
    use crate::postings::PostingsWriter;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codec-lucene9-prt-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Helper: collect all doc IDs from a PostingsEnum.
    fn collect_docs(pe: &mut PostingsEnum) -> Vec<u32> {
        let mut docs = Vec::new();
        loop {
            match pe.next_doc().unwrap() {
                NO_MORE_DOCS => break,
                d => docs.push(d as u32),
            }
        }
        docs
    }

    /// Helper: collect (doc, freq) pairs from a PostingsEnum.
    fn collect_docs_and_freqs(pe: &mut PostingsEnum) -> Vec<(u32, u32)> {
        let mut result = Vec::new();
        loop {
            match pe.next_doc().unwrap() {
                NO_MORE_DOCS => break,
                d => {
                    let freq = pe.freq();
                    result.push((d as u32, freq));
                }
            }
        }
        result
    }

    /// Helper: collect (doc, freq, positions) from a PostingsEnum.
    fn collect_docs_freqs_positions(pe: &mut PostingsEnum) -> Vec<(u32, u32, Vec<u32>)> {
        let mut result = Vec::new();
        loop {
            match pe.next_doc().unwrap() {
                NO_MORE_DOCS => break,
                d => {
                    let freq = pe.freq();
                    let mut positions = Vec::new();
                    for _ in 0..freq {
                        positions.push(pe.next_position().unwrap());
                    }
                    result.push((d as u32, freq, positions));
                }
            }
        }
        result
    }

    /// Build a PostingsReader from in-memory postings data.
    ///
    /// Writes postings to a temp directory via `PostingsWriter`, then opens a
    /// `PostingsReader` pointing at the same files.
    fn build_reader(
        tag: &str,
        index_options: IndexOptions,
        postings: &BTreeMap<Vec<u8>, (Vec<u32>, Vec<u32>, Option<Vec<Vec<u32>>>)>,
    ) -> (FSDirectory, PostingsReader) {
        let root = temp_dir(tag);
        let dir = FSDirectory::open(&root).unwrap();
        let segment_id = [0x42u8; 16];
        let segment = "test";

        let mut writer = PostingsWriter::new(&dir, segment, &segment_id).unwrap();

        let field_info = FieldInfo {
            name: "message".to_string(),
            number: 0,
            index_options,
            ..FieldInfo::stored("message", 0)
        };

        // Estimate doc_count: max doc id + 1.
        let max_doc = postings
            .values()
            .flat_map(|(docs, _, _)| docs.last().copied())
            .max()
            .unwrap_or(0);
        let doc_count = max_doc + 1;

        writer.start_field(&field_info, doc_count).unwrap();

        // Terms must be written in sorted order (BTreeMap iteration is sorted).
        for (term, (docs, freqs, positions)) in postings.iter() {
            let pos_slice: Option<Vec<Vec<u32>>> = positions.clone();
            let pos_ref: Option<&[Vec<u32>]> = pos_slice.as_ref().map(|v| v.as_slice());
            writer
                .write_term(term, docs, freqs, pos_ref)
                .unwrap();
        }

        writer.finish_field().unwrap();
        let _files = writer.finish().unwrap();

        // Build FieldInfos for the reader.
        let field_infos = FieldInfos::new(vec![field_info]);

        let reader = PostingsReader::open(&dir, segment, "_Lucene912_0", &field_infos).unwrap();

        (dir, reader)
    }

    // -- Docs only ------------------------------------------------------------

    #[test]
    fn test_postings_round_trip_docs_only() {
        let mut postings: BTreeMap<Vec<u8>, (Vec<u32>, Vec<u32>, Option<Vec<Vec<u32>>>)> =
            BTreeMap::new();
        postings.insert(b"foo".to_vec(), (vec![0, 1, 2, 3, 4], vec![1; 5], None));
        postings.insert(b"hello".to_vec(), (vec![0, 5, 10], vec![1; 3], None));
        postings.insert(b"world".to_vec(), (vec![1, 3, 7], vec![1; 3], None));

        let (_dir, mut reader) = build_reader("docs-only", IndexOptions::Docs, &postings);

        for (term, (expected_docs, _, _)) in &postings {
            let mut pe = reader
                .read_term("message", term)
                .unwrap()
                .unwrap_or_else(|| panic!("term {:?} not found", String::from_utf8_lossy(term)));
            assert!(
                matches!(pe, PostingsEnum::Docs { .. }),
                "expected Docs variant for {:?}",
                String::from_utf8_lossy(term)
            );
            let docs = collect_docs(&mut pe);
            assert_eq!(
                docs, *expected_docs,
                "docs mismatch for {:?}",
                String::from_utf8_lossy(term)
            );
            // For the Docs variant, freq() is always 1 (even after exhaustion).
        }

        // Test advance
        let mut pe = reader.read_term("message", b"foo").unwrap().unwrap();
        assert_eq!(pe.advance(3).unwrap(), 3);
        assert_eq!(pe.next_doc().unwrap(), 4);

        // Test missing term
        assert!(reader.read_term("message", b"nonexistent").unwrap().is_none());
        // Test missing field
        assert!(reader.read_term("nonexistent", b"hello").unwrap().is_none());
    }

    // -- Docs and freqs -------------------------------------------------------

    #[test]
    fn test_postings_round_trip_docs_and_freqs() {
        let mut postings: BTreeMap<Vec<u8>, (Vec<u32>, Vec<u32>, Option<Vec<Vec<u32>>>)> =
            BTreeMap::new();
        postings.insert(b"alpha".to_vec(), (vec![0, 2], vec![1, 3], None));
        postings.insert(
            b"beta".to_vec(),
            (vec![0, 1, 5, 10], vec![2, 1, 1, 4], None),
        );
        postings.insert(b"gamma".to_vec(), (vec![3, 7, 9], vec![5, 2, 3], None));

        let (_dir, mut reader) =
            build_reader("docs-freqs", IndexOptions::DocsAndFreqs, &postings);

        for (term, (expected_docs, expected_freqs, _)) in &postings {
            let mut pe = reader
                .read_term("message", term)
                .unwrap()
                .unwrap_or_else(|| panic!("term {:?} not found", String::from_utf8_lossy(term)));
            assert!(
                matches!(pe, PostingsEnum::DocsAndFreqs { .. }),
                "expected DocsAndFreqs variant for {:?}",
                String::from_utf8_lossy(term)
            );
            let result = collect_docs_and_freqs(&mut pe);
            let expected: Vec<(u32, u32)> = expected_docs
                .iter()
                .zip(expected_freqs.iter())
                .map(|(&d, &f)| (d, f))
                .collect();
            assert_eq!(
                result, expected,
                "docs+freqs mismatch for {:?}",
                String::from_utf8_lossy(term)
            );
        }

        // Verify we cannot call next_position on DocsAndFreqs.
        let mut pe = reader.read_term("message", b"beta").unwrap().unwrap();
        pe.next_doc().unwrap();
        assert!(pe.next_position().is_err());
    }

    // -- Docs, freqs, and positions -------------------------------------------

    #[test]
    fn test_postings_round_trip_docs_freqs_positions() {
        let mut postings: BTreeMap<Vec<u8>, (Vec<u32>, Vec<u32>, Option<Vec<Vec<u32>>>)> =
            BTreeMap::new();
        postings.insert(
            b"cat".to_vec(),
            (
                vec![0, 3],
                vec![2, 3],
                Some(vec![vec![0, 5], vec![1, 3, 7]]),
            ),
        );
        postings.insert(
            b"dog".to_vec(),
            (
                vec![1, 4, 8],
                vec![1, 2, 3],
                Some(vec![vec![0], vec![2, 4], vec![0, 1, 3]]),
            ),
        );
        postings.insert(
            b"emu".to_vec(),
            (vec![2], vec![4], Some(vec![vec![0, 2, 5, 9]])),
        );

        let (_dir, mut reader) = build_reader(
            "docs-freqs-pos",
            IndexOptions::DocsAndFreqsAndPositions,
            &postings,
        );

        for (term, (expected_docs, expected_freqs, expected_positions)) in &postings {
            let mut pe = reader
                .read_term("message", term)
                .unwrap()
                .unwrap_or_else(|| panic!("term {:?} not found", String::from_utf8_lossy(term)));
            assert!(
                matches!(pe, PostingsEnum::DocsFreqsPositions { .. }),
                "expected DocsFreqsPositions variant for {:?}",
                String::from_utf8_lossy(term)
            );
            let result = collect_docs_freqs_positions(&mut pe);
            let expected: Vec<(u32, u32, Vec<u32>)> = expected_docs
                .iter()
                .zip(expected_freqs.iter())
                .zip(expected_positions.as_ref().unwrap().iter())
                .map(|((&d, &f), p)| (d, f, p.clone()))
                .collect();
            assert_eq!(
                result, expected,
                "docs+freqs+positions mismatch for {:?}",
                String::from_utf8_lossy(term)
            );
        }

        // Verify next_position fails on DocsFreqsPositions after exhaustion.
        let mut pe = reader.read_term("message", b"emu").unwrap().unwrap();
        pe.next_doc().unwrap(); // doc 2, freq 4
        for _ in 0..4 {
            pe.next_position().unwrap();
        }
        assert!(pe.next_position().is_err());
    }

    // -- Singleton / small terms ----------------------------------------------

    #[test]
    fn test_postings_round_trip_singleton() {
        let mut postings: BTreeMap<Vec<u8>, (Vec<u32>, Vec<u32>, Option<Vec<Vec<u32>>>)> =
            BTreeMap::new();
        // Single-doc terms (singletons) test the special-case encoding path.
        postings.insert(b"a".to_vec(), (vec![5], vec![1], None));
        postings.insert(b"b".to_vec(), (vec![10], vec![1], None));
        postings.insert(b"c".to_vec(), (vec![15], vec![1], None));

        let (_dir, mut reader) = build_reader("singleton", IndexOptions::Docs, &postings);

        for (term, (expected_docs, _, _)) in &postings {
            let mut pe = reader.read_term("message", term).unwrap().unwrap();
            assert!(matches!(pe, PostingsEnum::Docs { .. }));
            let docs = collect_docs(&mut pe);
            assert_eq!(docs, *expected_docs);
        }

        // Singleton with positions
        let mut postings2: BTreeMap<Vec<u8>, (Vec<u32>, Vec<u32>, Option<Vec<Vec<u32>>>)> =
            BTreeMap::new();
        postings2.insert(
            b"only".to_vec(),
            (vec![0], vec![3], Some(vec![vec![1, 2, 3]])),
        );
        let (_dir2, mut reader2) = build_reader(
            "singleton-pos",
            IndexOptions::DocsAndFreqsAndPositions,
            &postings2,
        );
        let mut pe = reader2.read_term("message", b"only").unwrap().unwrap();
        assert!(matches!(pe, PostingsEnum::DocsFreqsPositions { .. }));
        let result = collect_docs_freqs_positions(&mut pe);
        assert_eq!(result, vec![(0, 3, vec![1, 2, 3])]);
    }

    // -- Large block test (exercises skip data and full 128-doc blocks) --------

    #[test]
    fn test_postings_round_trip_large_block() {
        // 300 docs: 2 full FOR blocks (128+128) + 44-doc tail.
        let docs: Vec<u32> = (0u32..300).collect();
        let freqs: Vec<u32> = (0u32..300).map(|i| (i % 5) + 1).collect();

        let mut postings: BTreeMap<Vec<u8>, (Vec<u32>, Vec<u32>, Option<Vec<Vec<u32>>>)> =
            BTreeMap::new();
        postings.insert(b"bulk".to_vec(), (docs.clone(), freqs.clone(), None));

        let (_dir, mut reader) = build_reader("large", IndexOptions::DocsAndFreqs, &postings);

        let mut pe = reader.read_term("message", b"bulk").unwrap().unwrap();
        assert!(matches!(pe, PostingsEnum::DocsAndFreqs { .. }));
        let result = collect_docs_and_freqs(&mut pe);
        let expected: Vec<(u32, u32)> =
            docs.iter().zip(freqs.iter()).map(|(&d, &f)| (d, f)).collect();
        assert_eq!(result, expected);

        // Test advance across block boundaries
        let mut pe2 = reader.read_term("message", b"bulk").unwrap().unwrap();
        assert_eq!(pe2.advance(200).unwrap(), 200);
        assert_eq!(pe2.advance(250).unwrap(), 250);
        assert_eq!(pe2.next_doc().unwrap(), 251);
    }
}
