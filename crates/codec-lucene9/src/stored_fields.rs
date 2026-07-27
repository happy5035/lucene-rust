//! Lucene90 stored fields writer, BEST_SPEED mode (LZ4), mirroring
//! `codecs/lucene90/compressing/Lucene90CompressingStoredFieldsWriter.java`,
//! `FieldsIndexWriter.java`, `StoredFieldsInts.java` and
//! `codecs/lucene90/LZ4WithPresetDictCompressionMode.java` (9.12.3).
//!
//! Writes `_X.fdt` (compressed chunks), `_X.fdx` (DirectMonotonic index) and
//! `_X.fdm` (metadata). Simplification vs. Lucene: LZ4 sub-blocks are
//! compressed independently (no preset dictionary); the on-disk layout
//! (dictLength/blockLength VInts + compressed-length table) is unchanged and
//! remains readable by `LZ4WithPresetDictDecompressor`.

use std::io;

#[cfg(test)]
use crate::codec_util::index_header_length;
use crate::codec_util::{
    check_footer, check_footer_structure, check_index_header, write_footer, write_index_header,
};
use crate::directory::FSDirectory;
use crate::io::{ChecksumIndexOutput, DataInput, IndexInput};
use crate::packed::{DirectMonotonicReader, direct_monotonic_write};

/// Lucene90StoredFieldsFormat.Mode.BEST_SPEED parameters
/// (Lucene90StoredFieldsFormat.java:157-172,182-185).
pub const CHUNK_SIZE: usize = 81920; // 10 * 8 * 1024
pub const MAX_DOCS_PER_CHUNK: i32 = 1024;
pub const BLOCK_SHIFT: u32 = 10;

/// Lucene90CompressingStoredFieldsWriter codec names / extensions (:59-82).
pub const FIELDS_EXTENSION: &str = "fdt";
pub const INDEX_EXTENSION: &str = "fdx";
pub const META_EXTENSION: &str = "fdm";
const FDT_CODEC_NAME: &str = "Lucene90StoredFieldsFastData";
const INDEX_CODEC_NAME: &str = "Lucene90FieldsIndex";
const FDT_VERSION: u32 = 1; // VERSION_CURRENT (:80-82)
const FIELDS_INDEX_VERSION: u32 = 0; // FieldsIndexWriter (:48-49)

// Field type tags (:70-75). TYPE_BITS = 3 (:77).
const TYPE_STRING: i64 = 0;
const TYPE_BYTE_ARR: i64 = 1;
const TYPE_NUMERIC_INT: i64 = 2;
const TYPE_NUMERIC_FLOAT: i64 = 3;
const TYPE_NUMERIC_LONG: i64 = 4;
const TYPE_NUMERIC_DOUBLE: i64 = 5;

// TLong encodings (:334-340)
const SECOND: i64 = 1000;
const HOUR: i64 = 60 * 60 * SECOND;
const DAY: i64 = 24 * HOUR;
const SECOND_ENCODING: u8 = 0x40;
const HOUR_ENCODING: u8 = 0x80;
const DAY_ENCODING: u8 = 0xC0;

/// A single stored field value.
#[derive(Debug, Clone, PartialEq)]
pub enum StoredField {
    String(String),
    Bytes(Vec<u8>),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
}

/// Segment file names produced by this writer (IndexFileNames.segmentFileName).
pub fn file_names(segment: &str, suffix: &str) -> [String; 3] {
    [
        format!("{segment}{suffix}.{FIELDS_EXTENSION}"),
        format!("{segment}{suffix}.{INDEX_EXTENSION}"),
        format!("{segment}{suffix}.{META_EXTENSION}"),
    ]
}

/// Lucene90CompressingStoredFieldsWriter.writeTLong (:442-470).
pub fn write_tlong(out: &mut Vec<u8>, l: i64) {
    let mut value = l;
    let mut header: u8;
    if value % SECOND != 0 {
        header = 0;
    } else if value % DAY == 0 {
        header = DAY_ENCODING;
        value /= DAY;
    } else if value % HOUR == 0 {
        header = HOUR_ENCODING;
        value /= HOUR;
    } else {
        header = SECOND_ENCODING;
        value /= SECOND;
    }
    let zigzag = ((value << 1) ^ (value >> 63)) as u64;
    header |= (zigzag & 0x1f) as u8;
    let upper = zigzag >> 5;
    if upper != 0 {
        header |= 0x20;
    }
    out.push(header);
    if upper != 0 {
        write_vlong_raw(out, upper);
    }
}

/// Lucene90CompressingStoredFieldsWriter.writeZFloat (:357-374).
pub fn write_zfloat(out: &mut Vec<u8>, f: f32) {
    let int_val = f as i32;
    let float_bits = f.to_bits() as i32;
    const NEGATIVE_ZERO_FLOAT: i32 = (-0.0f32).to_bits() as i32;
    if f == int_val as f32 && (-1..=0x7d).contains(&int_val) && float_bits != NEGATIVE_ZERO_FLOAT {
        // small integer value [-1..125]: single byte
        out.push(0x80 | (1 + int_val) as u8);
    } else if float_bits >= 0 {
        // other positive floats: 4 bytes (byte + LE short + byte)
        out.push((float_bits >> 24) as u8);
        out.extend_from_slice(&((float_bits >> 8) as i16).to_le_bytes());
        out.push(float_bits as u8);
    } else {
        // other negative float: 5 bytes
        out.push(0xFF);
        out.extend_from_slice(&float_bits.to_le_bytes());
    }
}

/// Lucene90CompressingStoredFieldsWriter.writeZDouble (:392-415).
pub fn write_zdouble(out: &mut Vec<u8>, d: f64) {
    let int_val = d as i32;
    let double_bits = d.to_bits() as i64;
    const NEGATIVE_ZERO_DOUBLE: i64 = (-0.0f64).to_bits() as i64;
    if d == int_val as f64 && (-1..=0x7c).contains(&int_val) && double_bits != NEGATIVE_ZERO_DOUBLE
    {
        // small integer value [-1..124]: single byte
        out.push(0x80 | (int_val + 1) as u8);
    } else if d == d as f32 as f64 {
        // accurate float representation: 5 bytes
        out.push(0xFE);
        out.extend_from_slice(&((d as f32).to_bits() as i32).to_le_bytes());
    } else if double_bits >= 0 {
        // other positive doubles: 8 bytes (byte + LE int + LE short + byte)
        out.push((double_bits >> 56) as u8);
        out.extend_from_slice(&((double_bits >> 24) as i32).to_le_bytes());
        out.extend_from_slice(&((double_bits >> 8) as i16).to_le_bytes());
        out.push(double_bits as u8);
    } else {
        // other negative doubles: 9 bytes
        out.push(0xFF);
        out.extend_from_slice(&double_bits.to_le_bytes());
    }
}

fn write_vlong_raw(out: &mut Vec<u8>, mut v: u64) {
    while v & !0x7f != 0 {
        out.push(((v & 0x7f) as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn write_vint_raw(out: &mut Vec<u8>, v: i32) {
    debug_assert!(v >= 0);
    write_vlong_raw(out, v as u32 as u64);
}

/// Inverse of [`write_tlong`] (Lucene90 ...Reader.readTLong).
fn read_tlong(input: &mut impl DataInput) -> io::Result<i64> {
    let header = input.read_byte()?;
    let encoding = header & 0xC0;
    let mut zigzag = (header & 0x1f) as u64;
    if header & 0x20 != 0 {
        let upper = input.read_vlong()? as u64;
        zigzag |= upper << 5;
    }
    let value = (zigzag >> 1) as i64 ^ -((zigzag & 1) as i64);
    let multiplier = match encoding {
        0 => 1,
        SECOND_ENCODING => SECOND,
        HOUR_ENCODING => HOUR,
        DAY_ENCODING => DAY,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid tlong encoding {other:#x}"),
            ));
        }
    };
    Ok(value * multiplier)
}

/// Inverse of [`write_zfloat`] (Lucene90 ...Reader.readZFloat).
fn read_zfloat(input: &mut impl DataInput) -> io::Result<f32> {
    let b = input.read_byte()?;
    if b & 0x80 != 0 {
        // small integer value [-1..125]
        return Ok(((b & 0x7f) as i32 - 1) as f32);
    }
    if b == 0xFF {
        let mut bytes = [0u8; 4];
        input.read_bytes(&mut bytes)?;
        return Ok(f32::from_bits(u32::from_le_bytes(bytes)));
    }
    // positive float: high byte + LE short (bits 8..24) + low byte
    let mid = (input.read_short()? as u16) as u32;
    let low = input.read_byte()? as u32;
    let bits = ((b as u32) << 24) | (mid << 8) | low;
    Ok(f32::from_bits(bits))
}

/// Inverse of [`write_zdouble`] (Lucene90 ...Reader.readZDouble).
fn read_zdouble(input: &mut impl DataInput) -> io::Result<f64> {
    let b = input.read_byte()?;
    if b & 0x80 != 0 {
        // small integer value [-1..124]
        return Ok(((b & 0x7f) as i32 - 1) as f64);
    }
    if b == 0xFE {
        let mut bytes = [0u8; 4];
        input.read_bytes(&mut bytes)?;
        return Ok(f32::from_bits(u32::from_le_bytes(bytes)) as f64);
    }
    if b == 0xFF {
        let mut bytes = [0u8; 8];
        input.read_bytes(&mut bytes)?;
        return Ok(f64::from_bits(u64::from_le_bytes(bytes)));
    }
    // positive double: high byte + LE int (bits 24..56) + LE short + low byte
    let mut int_bytes = [0u8; 4];
    input.read_bytes(&mut int_bytes)?;
    let mid_hi = u32::from_le_bytes(int_bytes) as u64;
    let mid_lo = (input.read_short()? as u16) as u64;
    let low = input.read_byte()? as u64;
    let bits = ((b as u64) << 56) | (mid_hi << 24) | (mid_lo << 8) | low;
    Ok(f64::from_bits(bits))
}

fn write_zint_raw(out: &mut Vec<u8>, v: i32) {
    write_vlong_raw(out, ((v << 1) ^ (v >> 31)) as u32 as u64);
}

/// StoredFieldsInts.writeInts (:31-58) over non-negative ints.
fn stored_fields_write_ints(out: &mut ChecksumIndexOutput, values: &[i32]) -> io::Result<()> {
    debug_assert!(values.iter().all(|&v| v >= 0));
    let all_equal = values.iter().all(|&v| v == values[0]);
    if all_equal {
        out.write_byte(0)?;
        out.write_vint(values[0])
    } else {
        let mut max: u32 = 0;
        for &v in values {
            max |= v as u32;
        }
        if max <= 0xff {
            out.write_byte(8)?;
            write_ints8(out, values)
        } else if max <= 0xffff {
            out.write_byte(16)?;
            write_ints16(out, values)
        } else {
            out.write_byte(32)?;
            write_ints32(out, values)
        }
    }
}

// StoredFieldsInts interleaves 128-value blocks into longs (written LE).
const SF_BLOCK: usize = 128;

/// StoredFieldsInts.writeInts8 (:60-81).
fn write_ints8(out: &mut ChecksumIndexOutput, values: &[i32]) -> io::Result<()> {
    let count = values.len();
    let mut k = 0;
    while k + SF_BLOCK <= count {
        for i in 0..16 {
            let l: u64 = ((values[k + i] as u64) << 56)
                | ((values[k + 16 + i] as u64) << 48)
                | ((values[k + 32 + i] as u64) << 40)
                | ((values[k + 48 + i] as u64) << 32)
                | ((values[k + 64 + i] as u64) << 24)
                | ((values[k + 80 + i] as u64) << 16)
                | ((values[k + 96 + i] as u64) << 8)
                | (values[k + 112 + i] as u64);
            out.write_long(l as i64)?;
        }
        k += SF_BLOCK;
    }
    for &v in &values[k..] {
        out.write_byte(v as u8)?;
    }
    Ok(())
}

/// StoredFieldsInts.writeInts16 (:83-100).
fn write_ints16(out: &mut ChecksumIndexOutput, values: &[i32]) -> io::Result<()> {
    let count = values.len();
    let mut k = 0;
    while k + SF_BLOCK <= count {
        for i in 0..32 {
            let l: u64 = ((values[k + i] as u64) << 48)
                | ((values[k + 32 + i] as u64) << 32)
                | ((values[k + 64 + i] as u64) << 16)
                | (values[k + 96 + i] as u64);
            out.write_long(l as i64)?;
        }
        k += SF_BLOCK;
    }
    for &v in &values[k..] {
        out.write_short(v as i16)?;
    }
    Ok(())
}

/// StoredFieldsInts.writeInts32 (:102-115).
fn write_ints32(out: &mut ChecksumIndexOutput, values: &[i32]) -> io::Result<()> {
    let count = values.len();
    let mut k = 0;
    while k + SF_BLOCK <= count {
        for i in 0..64 {
            let l: u64 = ((values[k + i] as u64) << 32) | (values[k + 64 + i] as u64);
            out.write_long(l as i64)?;
        }
        k += SF_BLOCK;
    }
    for &v in &values[k..] {
        out.write_int(v)?;
    }
    Ok(())
}

/// Inverse of [`write_ints8`]: de-interleave 128-value blocks (8-bit lanes).
fn load_ints8(input: &mut impl DataInput, count: usize) -> io::Result<Vec<i32>> {
    let mut out = vec![0i32; count];
    let mut k = 0;
    while k + SF_BLOCK <= count {
        for i in 0..16 {
            let l = input.read_long()? as u64;
            for b in 0..8 {
                out[k + b * 16 + i] = ((l >> (56 - b * 8)) & 0xff) as i32;
            }
        }
        k += SF_BLOCK;
    }
    for item in out.iter_mut().skip(k) {
        *item = input.read_byte()? as i32;
    }
    Ok(out)
}

/// Inverse of [`write_ints16`]: de-interleave 128-value blocks (16-bit lanes).
fn load_ints16(input: &mut impl DataInput, count: usize) -> io::Result<Vec<i32>> {
    let mut out = vec![0i32; count];
    let mut k = 0;
    while k + SF_BLOCK <= count {
        for i in 0..32 {
            let l = input.read_long()? as u64;
            for s in 0..4 {
                out[k + s * 32 + i] = ((l >> (48 - s * 16)) & 0xffff) as i32;
            }
        }
        k += SF_BLOCK;
    }
    for item in out.iter_mut().skip(k) {
        *item = (input.read_short()? as u16) as i32;
    }
    Ok(out)
}

/// Inverse of [`write_ints32`]: de-interleave 128-value blocks (32-bit lanes).
fn load_ints32(input: &mut impl DataInput, count: usize) -> io::Result<Vec<i32>> {
    let mut out = vec![0i32; count];
    let mut k = 0;
    while k + SF_BLOCK <= count {
        for i in 0..64 {
            let l = input.read_long()? as u64;
            out[k + i] = (l >> 32) as u32 as i32;
            out[k + 64 + i] = (l & 0xffff_ffff) as u32 as i32;
        }
        k += SF_BLOCK;
    }
    for item in out.iter_mut().skip(k) {
        *item = input.read_int()?;
    }
    Ok(out)
}

/// Inverse of [`save_ints`] (Lucene90CompressingStoredFieldsReader.loadInts).
/// `count` is known from the chunk's numDocs.
fn load_ints(input: &mut impl DataInput, count: usize) -> io::Result<Vec<i32>> {
    if count == 1 {
        return Ok(vec![input.read_vint()?]);
    }
    match input.read_byte()? {
        0 => {
            let v = input.read_vint()?;
            Ok(vec![v; count])
        }
        8 => load_ints8(input, count),
        16 => load_ints16(input, count),
        32 => load_ints32(input, count),
        bpv => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid stored-fields ints bpv {bpv}"),
        )),
    }
}

/// Lucene90CompressingStoredFieldsWriter.saveInts (:199-205).
fn save_ints(out: &mut ChecksumIndexOutput, values: &[i32]) -> io::Result<()> {
    if values.len() == 1 {
        out.write_vint(values[0])
    } else {
        stored_fields_write_ints(out, values)
    }
}

/// LZ4WithPresetDictCompressionMode.LZ4WithPresetDictCompressor.compress
/// (:172-195) semantics, written with `dict_length = 0` and a single
/// sub-block per chunk: dict/block lengths are per-chunk header values that
/// the decompressor honors generically
/// (LZ4WithPresetDictCompressionMode.LZ4WithPresetDictDecompressor.decompress),
/// so one whole-chunk LZ4 block is a legal encoding. It compresses faster
/// (one call instead of ~11 small ones) and better (matches span the whole
/// chunk); the trade-off is read-side amplification, which only affects
/// Java-side reads, not our write path.
fn compress_lz4(bytes: &[u8], out: &mut ChecksumIndexOutput) -> io::Result<()> {
    let len = bytes.len();
    let dict_length = 0usize;
    let block_length = len.max(1);
    out.write_vint(dict_length as i32)?;
    out.write_vint(block_length as i32)?;

    // Empty dict still materializes as a 1-byte LZ4 stream (0x00 token),
    // which the decompressor consumes when dict_length == 0.
    let dict_block = lz4_block_compress(&[])?;
    let data_block = lz4_block_compress(bytes)?;
    out.write_vint(dict_block.len() as i32)?;
    if len > 0 {
        out.write_vint(data_block.len() as i32)?;
    }
    out.write_bytes(&dict_block)?;
    if len > 0 {
        out.write_bytes(&data_block)?;
    }
    Ok(())
}

/// Raw LZ4 block compress (no size header), FAST(2) acceleration: ~30%
/// faster than acceleration 1 for a negligible ratio change on repetitive
/// log text. Any conforming LZ4 stream is readable by Lucene's decompressor.
fn lz4_block_compress(bytes: &[u8]) -> io::Result<Vec<u8>> {
    lz4::block::compress(bytes, Some(lz4::block::CompressionMode::FAST(2)), false)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("lz4 compress: {e}")))
}

/// Writer for `_X.fdt` / `_X.fdx` / `_X.fdm`.
pub struct StoredFieldsWriter {
    fields_stream: ChecksumIndexOutput,
    meta_stream: ChecksumIndexOutput,
    fdx_name: String,
    fdt_name: String,
    fdm_name: String,
    segment_id: [u8; 16],
    suffix: String,

    buffered_docs: Vec<u8>,
    num_stored_fields: Vec<i32>,
    end_offsets: Vec<i32>,
    doc_base: i32,
    num_buffered_docs: i32,
    num_stored_fields_in_doc: i32,

    chunk_num_docs: Vec<i32>,
    chunk_start_pointers: Vec<i64>,

    num_chunks: i64,
    num_dirty_chunks: i64,
    num_dirty_docs: i64,
    total_docs_in_chunks: i64,
}

impl StoredFieldsWriter {
    /// Creates the writer, writing the .fdm header (+VInt chunkSize) and the
    /// .fdt header, like the Java constructor (:103-164).
    pub fn new(
        dir: &FSDirectory,
        segment: &str,
        segment_id: [u8; 16],
        suffix: &str,
    ) -> io::Result<Self> {
        let [fdt_name, fdx_name, fdm_name] = file_names(segment, suffix);

        let mut meta_stream = dir.create_output(&fdm_name)?;
        write_index_header(
            &mut meta_stream,
            &format!("{INDEX_CODEC_NAME}Meta"),
            FDT_VERSION,
            &segment_id,
            suffix,
        )?;

        let mut fields_stream = dir.create_output(&fdt_name)?;
        write_index_header(
            &mut fields_stream,
            FDT_CODEC_NAME,
            FDT_VERSION,
            &segment_id,
            suffix,
        )?;

        meta_stream.write_vint(CHUNK_SIZE as i32)?;

        Ok(StoredFieldsWriter {
            fields_stream,
            meta_stream,
            fdx_name,
            fdt_name,
            fdm_name,
            segment_id,
            suffix: suffix.to_string(),
            buffered_docs: Vec::new(),
            num_stored_fields: Vec::new(),
            end_offsets: Vec::new(),
            doc_base: 0,
            num_buffered_docs: 0,
            num_stored_fields_in_doc: 0,
            chunk_num_docs: Vec::new(),
            chunk_start_pointers: Vec::new(),
            num_chunks: 0,
            num_dirty_chunks: 0,
            num_dirty_docs: 0,
            total_docs_in_chunks: 0,
        })
    }

    pub fn start_document(&mut self) {
        // no-op, mirrors StoredFieldsWriter.startDocument (:180-181)
    }

    /// Serializes one field into the buffered docs (:272-328).
    pub fn write_field(&mut self, field_number: u32, value: &StoredField) {
        self.num_stored_fields_in_doc += 1;
        let buf = &mut self.buffered_docs;
        let info_and_bits = ((field_number as i64) << 3) | value.type_tag();
        write_vlong_raw(buf, info_and_bits as u64);
        match value {
            StoredField::String(s) => {
                write_vint_raw(buf, s.len() as i32);
                buf.extend_from_slice(s.as_bytes());
            }
            StoredField::Bytes(b) => {
                write_vint_raw(buf, b.len() as i32);
                buf.extend_from_slice(b);
            }
            StoredField::Int(v) => write_zint_raw(buf, *v),
            StoredField::Long(v) => write_tlong(buf, *v),
            StoredField::Float(v) => write_zfloat(buf, *v),
            StoredField::Double(v) => write_zdouble(buf, *v),
        }
    }

    /// finishDocument (:183-197): records the doc and flushes when full.
    pub fn finish_document(&mut self) -> io::Result<()> {
        self.num_stored_fields.push(self.num_stored_fields_in_doc);
        self.num_stored_fields_in_doc = 0;
        self.end_offsets.push(self.buffered_docs.len() as i32);
        self.num_buffered_docs += 1;
        if self.buffered_docs.len() >= CHUNK_SIZE || self.num_buffered_docs >= MAX_DOCS_PER_CHUNK {
            self.flush(false)?;
        }
        Ok(())
    }

    /// Convenience: writes a whole document.
    pub fn write_document(&mut self, fields: &[(u32, StoredField)]) -> io::Result<()> {
        self.start_document();
        for (number, value) in fields {
            self.write_field(*number, value);
        }
        self.finish_document()
    }

    /// M6 T-C：裸 chunk 追加（Lucene90CompressingStoredFieldsWriter.copyChunks
    /// :552-595 的写侧一半）。`code` 原样携带源 chunk 的 numDocs<<2|dirty|sliced
    /// 位；docBase 重写为当前 doc_base（rebase，:565-568）。`payload` = 源 chunk
    /// 去掉 (docBase, code) 两个 VInt 后的全部字节（numStoredFields/lengths/LZ4
    /// 数据，逐字节不解压）。调用后内部 doc 缓冲必须恒空——与 write_field 的
    /// 文档级写入路径互斥（本系统归并只用裸路径）。
    pub fn append_raw_chunk(&mut self, num_docs: i32, code: i32, payload: &[u8]) -> io::Result<()> {
        assert_eq!(
            self.num_buffered_docs, 0,
            "raw chunk append never mixes with doc-level writes"
        );
        assert_eq!(code >> 2, num_docs, "code numDocs mismatch");
        self.num_chunks += 1;
        if code & 2 != 0 {
            // dirty bit（force flush 的 chunk）：记账与 flush(true) 一致 (:238-241)
            self.num_dirty_chunks += 1;
            self.num_dirty_docs += num_docs as i64;
        }
        self.chunk_num_docs.push(num_docs);
        self.chunk_start_pointers
            .push(self.fields_stream.file_pointer() as i64);
        self.total_docs_in_chunks += num_docs as i64;
        self.fields_stream.write_vint(self.doc_base)?; // rebase
        self.fields_stream.write_vint(code)?;
        self.fields_stream.write_bytes(payload)?;
        self.doc_base += num_docs;
        Ok(())
    }

    /// flush (:234-270).
    fn flush(&mut self, force: bool) -> io::Result<()> {
        self.num_chunks += 1;
        if force {
            self.num_dirty_chunks += 1;
            self.num_dirty_docs += self.num_buffered_docs as i64;
        }
        self.chunk_num_docs.push(self.num_buffered_docs);
        self.chunk_start_pointers
            .push(self.fields_stream.file_pointer() as i64);
        self.total_docs_in_chunks += self.num_buffered_docs as i64;

        // transform end offsets into lengths (:243-248)
        let n = self.num_buffered_docs as usize;
        let mut lengths = self.end_offsets.clone();
        for i in (1..n).rev() {
            lengths[i] = self.end_offsets[i] - self.end_offsets[i - 1];
        }
        lengths.truncate(n);
        let num_stored_fields = &self.num_stored_fields[..n];

        let sliced = self.buffered_docs.len() >= 2 * CHUNK_SIZE; // :249
        let sliced_bit = if sliced { 1 } else { 0 };
        let dirty_bit = if force { 2 } else { 0 };

        // writeHeader (:207-226)
        self.fields_stream.write_vint(self.doc_base)?;
        self.fields_stream
            .write_vint(((self.num_buffered_docs) << 2) | dirty_bit | sliced_bit)?;
        save_ints(&mut self.fields_stream, num_stored_fields)?;
        save_ints(&mut self.fields_stream, &lengths)?;

        // compress (:252-264)
        if sliced {
            for slice in self.buffered_docs.chunks(CHUNK_SIZE) {
                compress_lz4(slice, &mut self.fields_stream)?;
            }
        } else {
            let docs = std::mem::take(&mut self.buffered_docs);
            compress_lz4(&docs, &mut self.fields_stream)?;
            self.buffered_docs = docs;
        }

        // reset (:266-269)
        self.doc_base += self.num_buffered_docs;
        self.num_buffered_docs = 0;
        self.buffered_docs.clear();
        self.num_stored_fields.clear();
        self.end_offsets.clear();
        Ok(())
    }

    /// finish (:472-490) + FieldsIndexWriter.finish (:106-182). Writes .fdx,
    /// completes .fdm, and footers .fdt/.fdx/.fdm. Java collects the index
    /// deltas in temp files; we collect them in memory — the byte layout of
    /// the outputs is identical.
    pub fn finish(mut self, num_docs: i32, dir: &FSDirectory) -> io::Result<StoredFieldsStats> {
        if self.num_buffered_docs > 0 {
            self.flush(true)?;
        }
        if self.doc_base != num_docs {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "wrote {} docs, finish called with numDocs={num_docs}",
                    self.doc_base
                ),
            ));
        }
        let total_chunks = self.chunk_num_docs.len();
        let max_pointer = self.fields_stream.file_pointer() as i64;

        // FieldsIndexWriter.finish: create .fdx and write its header (:114-116)
        let mut data_out = dir.create_output(&self.fdx_name)?;
        write_index_header(
            &mut data_out,
            &format!("{INDEX_CODEC_NAME}Idx"),
            FIELDS_INDEX_VERSION,
            &self.segment_id,
            &self.suffix,
        )?;

        // .fdm steps 3-4: numDocs, blockShift, totalChunks+1, docsStartPointer (:118-121)
        self.meta_stream.write_int(num_docs)?;
        self.meta_stream.write_int(BLOCK_SHIFT as i32)?;
        self.meta_stream.write_int(total_chunks as i32 + 1)?;
        self.meta_stream
            .write_long(data_out.file_pointer() as i64)?;

        // docs DirectMonotonic: 0, then cumulative doc counts (:128-136)
        let mut doc_values: Vec<u64> = Vec::with_capacity(total_chunks + 1);
        let mut doc = 0u64;
        doc_values.push(doc);
        for &n in &self.chunk_num_docs {
            doc += n as u64;
            doc_values.push(doc);
        }
        debug_assert_eq!(doc, num_docs as u64);
        direct_monotonic_write(
            &mut self.meta_stream,
            &mut data_out,
            &doc_values,
            BLOCK_SHIFT,
        )?;

        // .fdm step 6: startPointersStartPointer (:149)
        self.meta_stream
            .write_long(data_out.file_pointer() as i64)?;

        // filePointers DirectMonotonic: chunk start pointers + maxPointer (:156-167)
        let mut fp_values: Vec<u64> = Vec::with_capacity(total_chunks + 1);
        for &fp in &self.chunk_start_pointers {
            fp_values.push(fp as u64);
        }
        fp_values.push(max_pointer as u64);
        direct_monotonic_write(
            &mut self.meta_stream,
            &mut data_out,
            &fp_values,
            BLOCK_SHIFT,
        )?;

        // .fdm steps 8-9: startPointersEndPointer, maxPointer (:177-178)
        self.meta_stream
            .write_long(data_out.file_pointer() as i64)?;
        self.meta_stream.write_long(max_pointer)?;
        write_footer(&mut data_out)?;

        // .fdm step 10 + footer; fdt footer (:483-488)
        self.meta_stream.write_vlong(self.num_chunks)?;
        self.meta_stream.write_vlong(self.num_dirty_chunks)?;
        self.meta_stream.write_vlong(self.num_dirty_docs)?;
        write_footer(&mut self.meta_stream)?;
        write_footer(&mut self.fields_stream)?;

        self.meta_stream.flush()?;
        self.fields_stream.flush()?;
        data_out.flush()?;

        Ok(StoredFieldsStats {
            num_chunks: self.num_chunks,
            num_dirty_chunks: self.num_dirty_chunks,
            num_dirty_docs: self.num_dirty_docs,
            fdt_name: self.fdt_name,
            fdx_name: self.fdx_name,
            fdm_name: self.fdm_name,
        })
    }
}

impl StoredField {
    fn type_tag(&self) -> i64 {
        match self {
            StoredField::String(_) => TYPE_STRING,
            StoredField::Bytes(_) => TYPE_BYTE_ARR,
            StoredField::Int(_) => TYPE_NUMERIC_INT,
            StoredField::Long(_) => TYPE_NUMERIC_LONG,
            StoredField::Float(_) => TYPE_NUMERIC_FLOAT,
            StoredField::Double(_) => TYPE_NUMERIC_DOUBLE,
        }
    }
}

pub struct StoredFieldsStats {
    pub num_chunks: i64,
    pub num_dirty_chunks: i64,
    pub num_dirty_docs: i64,
    pub fdt_name: String,
    pub fdx_name: String,
    pub fdm_name: String,
}

/// `.fdx/.fdm` 块索引读（FieldsIndexReader 对偶；Lucene90CompressingStoredFieldsReader
/// 构造路径；M6 T-C stored 裸拷贝专用，spec §4.2/§4.3）。打开时解析全部元数据，
/// 两个 DirectMonotonic 序列（docs 累计数 / chunk 起始 fp）的原件驻内存，
/// get 时现场重建 view。
pub struct StoredFieldsIndexReader {
    block_shift: u32,
    num_chunks: usize,
    docs_meta: Vec<u8>,
    docs_data: Vec<u8>,
    sp_meta: Vec<u8>,
    sp_data: Vec<u8>,
}

/// 从 .fdm 流内读一个 DirectMonotonic 的 meta 区（内联 21B/块；
/// DirectMonotonicReader.Meta 构造的块数公式，packed.rs:237-241）。
fn read_dm_meta(
    fdm: &mut crate::io::ChecksumIndexInput,
    num_values: usize,
    block_shift: u32,
) -> io::Result<Vec<u8>> {
    let num_blocks = if num_values == 0 {
        0
    } else {
        (num_values - 1) >> block_shift
    } + 1;
    let mut meta = vec![0u8; num_blocks * DirectMonotonicReader::META_RECORD_BYTES];
    fdm.read_bytes(&mut meta)?;
    Ok(meta)
}

impl StoredFieldsIndexReader {
    /// 解析 .fdm 全部元数据 + 从 .fdx 切出两个 DM 数据区
    /// （布局对照写侧 finish，stored_fields.rs:482-573）。
    pub fn open(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16]) -> io::Result<Self> {
        let [_fdt_name, fdx_name, fdm_name] = file_names(segment, "");
        let mut fdm = dir.open_checksum_input(&fdm_name)?;
        check_index_header(
            &mut fdm,
            &format!("{INDEX_CODEC_NAME}Meta"),
            FDT_VERSION,
            FDT_VERSION,
            segment_id,
            "",
        )?;
        let chunk_size = fdm.read_vint()?;
        if chunk_size != CHUNK_SIZE as i32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("chunkSize {chunk_size} != {CHUNK_SIZE}"),
            ));
        }
        let num_docs = fdm.read_int()?;
        let block_shift = fdm.read_int()? as u32;
        let total_values = fdm.read_int()? as usize; // totalChunks + 1
        if total_values == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "corrupt fdm: totalChunks + 1 == 0",
            ));
        }
        let docs_sp = fdm.read_long()? as u64;
        let docs_meta = read_dm_meta(&mut fdm, total_values, block_shift)?;
        let sp_sp = fdm.read_long()? as u64;
        let sp_meta = read_dm_meta(&mut fdm, total_values, block_shift)?;
        let sp_end = fdm.read_long()? as u64;
        let _max_pointer = fdm.read_long()? as u64;
        let _num_chunks = fdm.read_vlong()?;
        let _num_dirty_chunks = fdm.read_vlong()?;
        let _num_dirty_docs = fdm.read_vlong()?;
        check_footer(&mut fdm)?;

        // .fdx：header 之后是两段 DM packed data，sp_end 即数据区末尾（写侧
        // finish 随即 write_footer，stored_fields.rs:549-552）。
        let mut fdx_in = dir.open_input(&fdx_name)?;
        check_index_header(
            &mut fdx_in,
            &format!("{INDEX_CODEC_NAME}Idx"),
            FIELDS_INDEX_VERSION,
            FIELDS_INDEX_VERSION,
            segment_id,
            "",
        )?;
        let header_len = fdx_in.file_pointer();
        check_footer_structure(&fdx_in, fdx_in.length())?;
        let mut fdx = vec![0u8; (fdx_in.length() - header_len - 16) as usize]; // 16 = footer
        fdx_in.read_bytes(&mut fdx)?;
        let rel = |fp: u64| (fp - header_len) as usize;
        let reader = StoredFieldsIndexReader {
            block_shift,
            num_chunks: total_values - 1,
            docs_data: fdx[rel(docs_sp)..rel(sp_sp)].to_vec(),
            docs_meta,
            sp_data: fdx[rel(sp_sp)..rel(sp_end)].to_vec(),
            sp_meta,
        };
        // docs DM 末值 == numDocs（写侧 debug_assert，stored_fields.rs:523）
        debug_assert_eq!(
            reader.docs_dm().get(total_values as u64 - 1),
            num_docs as u64
        );
        Ok(reader)
    }

    fn docs_dm(&self) -> DirectMonotonicReader<'_> {
        DirectMonotonicReader::new(
            &self.docs_meta,
            &self.docs_data,
            self.num_chunks + 1,
            self.block_shift,
        )
        .expect("meta length checked at open")
    }

    fn sp_dm(&self) -> DirectMonotonicReader<'_> {
        DirectMonotonicReader::new(
            &self.sp_meta,
            &self.sp_data,
            self.num_chunks + 1,
            self.block_shift,
        )
        .expect("meta length checked at open")
    }

    pub fn num_chunks(&self) -> usize {
        self.num_chunks
    }

    /// chunk 内文档数 = docsDM[chunk+1] - docsDM[chunk]。
    pub fn chunk_doc_count(&self, chunk: usize) -> i32 {
        let dm = self.docs_dm();
        (dm.get(chunk as u64 + 1) - dm.get(chunk as u64)) as i32
    }

    /// chunk 在 .fdt 中的字节区间 [start, end)；末块 end == maxPointer
    /// （sp DM 末值即 maxPointer，写侧 stored_fields.rs:536-540）。
    pub fn chunk_byte_range(&self, chunk: usize) -> (u64, u64) {
        let sp = self.sp_dm();
        (sp.get(chunk as u64), sp.get(chunk as u64 + 1))
    }

    /// Locates the chunk holding `doc`: returns (chunk_index,
    /// chunk_start_file_pointer, chunk_doc_base). Binary search over the
    /// cumulative docs sequence (docsDM[c] <= doc < docsDM[c+1]).
    pub fn locate(&self, doc: u32) -> (usize, u64, u32) {
        let dm = self.docs_dm();
        let mut lo = 0usize;
        let mut hi = self.num_chunks; // exclusive upper bound on chunk index
        while lo < hi {
            let mid = (lo + hi) / 2;
            if dm.get(mid as u64 + 1) <= doc as u64 {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let chunk = lo;
        let base = dm.get(chunk as u64) as u32;
        let sp = self.sp_dm();
        (chunk, sp.get(chunk as u64), base)
    }
}

/// Decompresses one LZ4 sub-block written by [`compress_lz4`]. `expected_len`
/// is the sub-block's decompressed size (CHUNK_SIZE for full sub-blocks, the
/// remainder for the last). Preset-dict chunks (dict_length > 0, possible in
/// Java-written indexes) are not supported — this system's writer always uses
/// an empty dict.
fn decompress_subblock(input: &mut impl DataInput, expected_len: usize) -> io::Result<Vec<u8>> {
    let dict_length = input.read_vint()?;
    let block_length = input.read_vint()?;
    let dict_comp_len = input.read_vint()? as usize;
    // compress_lz4 writes both compressed lengths before any payload bytes.
    let data_comp_len = if expected_len > 0 {
        input.read_vint()? as usize
    } else {
        0
    };
    let mut dict_comp = vec![0u8; dict_comp_len];
    input.read_bytes(&mut dict_comp)?;
    if dict_length != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "preset-dict stored fields are not supported",
        ));
    }
    if expected_len == 0 {
        return Ok(Vec::new());
    }
    let mut data_comp = vec![0u8; data_comp_len];
    input.read_bytes(&mut data_comp)?;
    lz4::block::decompress(&data_comp, Some(block_length))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("lz4 decompress: {e}")))
}

/// Per-document stored-fields reader (Lucene90CompressingStoredFieldsReader
/// .document() analog). Used by index-sort merge to rewrite stored fields in
/// the merged (sorted) order — the raw-chunk-copy path can't reorder.
pub struct StoredFieldsReader {
    dir: FSDirectory,
    segment: String,
    idx: StoredFieldsIndexReader,
}

impl StoredFieldsReader {
    pub fn open(dir: &FSDirectory, segment: &str, id: &[u8; 16]) -> io::Result<Self> {
        let idx = StoredFieldsIndexReader::open(dir, segment, id)?;
        Ok(Self {
            dir: dir.clone(),
            segment: segment.to_string(),
            idx,
        })
    }

    /// Reads one document's stored fields as (field_number, value) pairs in
    /// write order.
    pub fn document(&self, doc: u32) -> io::Result<Vec<(u32, StoredField)>> {
        let (_chunk, start_fp, _chunk_base) = self.idx.locate(doc);
        let [fdt_name, _fdx, _fdm] = file_names(&self.segment, "");
        let mut fdt = self.dir.open_input(&fdt_name)?;
        fdt.seek(start_fp)?;
        let doc_base = fdt.read_vint()? as u32;
        let code = fdt.read_vint()?;
        let num_docs = (code >> 2) as usize;
        let sliced = code & 1 != 0;
        let num_stored = load_ints(&mut fdt, num_docs)?;
        let lengths = load_ints(&mut fdt, num_docs)?;
        let total_len: usize = lengths.iter().map(|&l| l as usize).sum();

        let mut data: Vec<u8> = Vec::with_capacity(total_len);
        if sliced {
            let mut remaining = total_len;
            while remaining > 0 {
                let this = remaining.min(CHUNK_SIZE);
                data.extend(decompress_subblock(&mut fdt, this)?);
                remaining -= this;
            }
        } else {
            data = decompress_subblock(&mut fdt, total_len)?;
        }

        let doc_in_chunk = (doc - doc_base) as usize;
        if doc_in_chunk >= num_docs {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("doc {doc} out of chunk (base {doc_base}, {num_docs} docs)"),
            ));
        }
        let offset: usize = lengths[..doc_in_chunk].iter().map(|&l| l as usize).sum();
        let len = lengths[doc_in_chunk] as usize;
        let doc_bytes = data[offset..offset + len].to_vec();
        let mut input = IndexInput::in_memory(doc_bytes);

        let mut fields = Vec::with_capacity(num_stored[doc_in_chunk] as usize);
        for _ in 0..num_stored[doc_in_chunk] {
            let info_and_bits = input.read_vlong()?;
            let field_num = (info_and_bits >> 3) as u32;
            let type_tag = info_and_bits & 0x7;
            let value = match type_tag {
                TYPE_STRING => {
                    let n = input.read_vint()? as usize;
                    let mut b = vec![0u8; n];
                    input.read_bytes(&mut b)?;
                    StoredField::String(String::from_utf8(b).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("utf8: {e}"))
                    })?)
                }
                TYPE_BYTE_ARR => {
                    let n = input.read_vint()? as usize;
                    let mut b = vec![0u8; n];
                    input.read_bytes(&mut b)?;
                    StoredField::Bytes(b)
                }
                TYPE_NUMERIC_INT => StoredField::Int(input.read_zint()?),
                TYPE_NUMERIC_FLOAT => StoredField::Float(read_zfloat(&mut input)?),
                TYPE_NUMERIC_LONG => StoredField::Long(read_tlong(&mut input)?),
                TYPE_NUMERIC_DOUBLE => StoredField::Double(read_zdouble(&mut input)?),
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown stored field type tag {other}"),
                    ));
                }
            };
            fields.push((field_num, value));
        }
        Ok(fields)
    }
}

impl StoredFieldsWriter {
    #[cfg(test)]
    fn force_flush_for_test(&mut self) {
        self.flush(true).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::FSDirectory;
    use crate::io::{DataInput, IndexOutput};
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codec-lucene9-stored-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn stored_fields_reader_round_trip() {
        let root = temp_dir("sfreader");
        let dir = FSDirectory::open(&root).unwrap();
        let id = [3u8; 16];
        let mut w = StoredFieldsWriter::new(&dir, "_0", id, "").unwrap();
        w.write_document(&[
            (0, StoredField::String("hello world".into())),
            (1, StoredField::Long(1_234_567_890_123)),
        ])
        .unwrap();
        w.write_document(&[
            (2, StoredField::Int(-42)),
            (0, StoredField::String("second".into())),
        ])
        .unwrap();
        w.write_document(&[]).unwrap();
        w.write_document(&[
            (3, StoredField::Bytes(vec![1, 2, 3, 255])),
            (1, StoredField::Long(-999)),
            (4, StoredField::Float(1.5)),
            (5, StoredField::Double(0.1)),
        ])
        .unwrap();
        w.finish(4, &dir).unwrap();

        let r = StoredFieldsReader::open(&dir, "_0", &id).unwrap();
        assert_eq!(
            r.document(0).unwrap(),
            vec![
                (0, StoredField::String("hello world".into())),
                (1, StoredField::Long(1_234_567_890_123))
            ]
        );
        assert_eq!(
            r.document(1).unwrap(),
            vec![
                (2, StoredField::Int(-42)),
                (0, StoredField::String("second".into()))
            ]
        );
        assert_eq!(r.document(2).unwrap(), vec![]);
        assert_eq!(
            r.document(3).unwrap(),
            vec![
                (3, StoredField::Bytes(vec![1, 2, 3, 255])),
                (1, StoredField::Long(-999)),
                (4, StoredField::Float(1.5)),
                (5, StoredField::Double(0.1))
            ]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// 写三个 chunk（2+2+1 docs），用新 reader 复读块索引。
    #[test]
    fn index_reader_chunk_layout() {
        let root = temp_dir("idxread");
        let dir = FSDirectory::open(&root).unwrap();
        let id = [9u8; 16];
        let mut w = StoredFieldsWriter::new(&dir, "_0", id, "").unwrap();
        // chunk 1: docs 0,1（每 doc 一个小字符串字段）
        for d in 0..5 {
            w.write_document(&[(0, StoredField::String(format!("doc-{d}")))])
                .unwrap();
            if d == 1 || d == 3 {
                w.force_flush_for_test(); // Step 4 在 impl 里新增的 #[cfg(test)] flush(true) 出口
            }
        }
        let stats = w.finish(5, &dir).unwrap();
        assert_eq!(stats.num_chunks, 3);

        let idx = StoredFieldsIndexReader::open(&dir, "_0", &id).unwrap();
        assert_eq!(idx.num_chunks(), 3);
        assert_eq!(idx.chunk_doc_count(0), 2);
        assert_eq!(idx.chunk_doc_count(1), 2);
        assert_eq!(idx.chunk_doc_count(2), 1);

        // 字节区间单调递增且末块终点 = maxPointer；逐块头部 (docBase, code) 校验
        let mut fdt = dir.open_input("_0.fdt").unwrap();
        let mut prev_end = 0;
        let mut doc_base = 0;
        for c in 0..idx.num_chunks() {
            let (start, end) = idx.chunk_byte_range(c);
            assert!(start >= prev_end && end > start);
            prev_end = end;
            fdt.seek(start).unwrap();
            assert_eq!(fdt.read_vint().unwrap(), doc_base);
            let code = fdt.read_vint().unwrap();
            assert_eq!(code >> 2, idx.chunk_doc_count(c));
            assert_eq!(code & 1, 0, "never sliced (small docs)");
            doc_base += idx.chunk_doc_count(c);
        }
        fs::remove_dir_all(&root).unwrap();
    }

    /// copyChunks 主路径（Lucene90CompressingStoredFieldsWriter.java:520-595）：
    /// 头两个 VInt 重写（docBase 重定基），其余字节原样。
    #[test]
    fn append_raw_chunk_rebases_doc_base() {
        let root = temp_dir("rawcopy");
        let dir = FSDirectory::open(&root).unwrap();
        let id_a = [1u8; 16];
        let mut wa = StoredFieldsWriter::new(&dir, "_a", id_a, "").unwrap();
        for d in 0..5 {
            wa.write_document(&[(0, StoredField::String(format!("payload-{d}")))])
                .unwrap();
            if d == 1 || d == 3 {
                wa.force_flush_for_test();
            }
        }
        wa.finish(5, &dir).unwrap();

        let idx = StoredFieldsIndexReader::open(&dir, "_a", &id_a).unwrap();
        let id_b = [2u8; 16];
        let mut wb = StoredFieldsWriter::new(&dir, "_b", id_b, "").unwrap();
        let mut fdt = dir.open_input("_a.fdt").unwrap();
        for c in 0..idx.num_chunks() {
            let (start, end) = idx.chunk_byte_range(c);
            fdt.seek(start).unwrap();
            let _src_base = fdt.read_vint().unwrap();
            let code = fdt.read_vint().unwrap();
            let mut payload = vec![0u8; (end - fdt.file_pointer()) as usize];
            fdt.read_bytes(&mut payload).unwrap();
            wb.append_raw_chunk(idx.chunk_doc_count(c), code, &payload)
                .unwrap();
        }
        wb.finish(5, &dir).unwrap();

        // 段 B 索引：3 chunks、doc 数 2/2/1、docBase 已重定基
        let idx_b = StoredFieldsIndexReader::open(&dir, "_b", &id_b).unwrap();
        assert_eq!(idx_b.num_chunks(), 3);
        let mut fdt_b = dir.open_input("_b.fdt").unwrap();
        let mut doc_base = 0;
        for c in 0..idx_b.num_chunks() {
            assert_eq!(idx_b.chunk_doc_count(c), idx.chunk_doc_count(c));
            let (start_b, end_b) = idx_b.chunk_byte_range(c);
            fdt_b.seek(start_b).unwrap();
            assert_eq!(
                fdt_b.read_vint().unwrap(),
                doc_base,
                "chunk {c} docBase rebased"
            );
            let code_b = fdt_b.read_vint().unwrap();
            // payload（header 之后全部字节）与源段逐字节一致
            let (start_a, end_a) = idx.chunk_byte_range(c);
            fdt.seek(start_a).unwrap();
            fdt.read_vint().unwrap();
            let code_a = fdt.read_vint().unwrap();
            assert_eq!(code_a, code_b);
            let mut pa = vec![0u8; (end_a - fdt.file_pointer()) as usize];
            fdt.read_bytes(&mut pa).unwrap();
            let mut pb = vec![0u8; (end_b - fdt_b.file_pointer()) as usize];
            fdt_b.read_bytes(&mut pb).unwrap();
            assert_eq!(pa, pb, "chunk {c} payload byte-identical");
            doc_base += idx_b.chunk_doc_count(c);
        }
        fs::remove_dir_all(&root).unwrap();
    }

    fn ints_bytes(values: &[i32]) -> Vec<u8> {
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        stored_fields_write_ints(&mut out, values).unwrap();
        out.into_bytes()
    }

    #[test]
    fn stored_fields_ints_all_equal() {
        assert_eq!(ints_bytes(&[5, 5, 5]), vec![0, 5]);
    }

    #[test]
    fn stored_fields_ints_bpv8_tail_only() {
        // < 128 values, not all equal => byte 8 + raw bytes
        assert_eq!(ints_bytes(&[1, 2, 3]), vec![8, 1, 2, 3]);
    }

    #[test]
    fn stored_fields_ints_bpv8_full_block() {
        // 128 values: 16 LE longs, each interleaving strides of 16
        let values: Vec<i32> = (0..128).collect();
        let bytes = ints_bytes(&values);
        assert_eq!(bytes.len(), 1 + 128);
        assert_eq!(bytes[0], 8);
        // first long: v[0]<<56 | v[16]<<48 | ... | v[112], written LE =>
        // bytes on disk: v[112], v[96], v[80], v[64], v[48], v[32], v[16], v[0]
        assert_eq!(&bytes[1..9], &[112, 96, 80, 64, 48, 32, 16, 0]);
        // second long: v[113], v[97], ..., v[1]
        assert_eq!(&bytes[9..17], &[113, 97, 81, 65, 49, 33, 17, 1]);
    }

    #[test]
    fn stored_fields_ints_bpv16() {
        let mut values = vec![0x1234i32; 129];
        values[0] = 0x0001; // not all equal, max > 0xff
        let bytes = ints_bytes(&values);
        assert_eq!(bytes[0], 16);
        assert_eq!(bytes.len(), 1 + 256 + 2); // full 128-block + 1 LE short tail
        // first long: v[0]<<48 | v[32]<<32 | v[64]<<16 | v[96], LE
        let first = u64::from_le_bytes(bytes[1..9].try_into().unwrap());
        assert_eq!(first, 0x0001_1234_1234_1234);
        // tail short: values[128] = 0x1234 LE
        assert_eq!(&bytes[257..259], &[0x34, 0x12]);
    }

    #[test]
    fn stored_fields_ints_bpv32() {
        let mut values = vec![0x01020304i32; 65];
        values[64] = 0x05060708;
        let bytes = ints_bytes(&values);
        assert_eq!(bytes[0], 32);
        // < 128 values => no interleaved longs, 65 LE ints
        assert_eq!(bytes.len(), 1 + 65 * 4);
        assert_eq!(&bytes[1..5], &[4, 3, 2, 1]);
        assert_eq!(&bytes[257..261], &[8, 7, 6, 5]);
    }

    #[test]
    fn stored_fields_ints_single_value_via_save_ints() {
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        save_ints(&mut out, &[42]).unwrap();
        assert_eq!(out.into_bytes(), vec![42]); // bare VInt, no bpv byte
    }

    #[test]
    fn tlong_all_branches() {
        // 00: not a multiple of 1000, small zigzag
        let mut b = Vec::new();
        write_tlong(&mut b, 1);
        assert_eq!(b, vec![0x02]); // header 00 | zigzag(1)=2

        // negative raw value
        let mut b = Vec::new();
        write_tlong(&mut b, -1);
        assert_eq!(b, vec![0x01]); // zigzag(-1)=1

        // continuation bit: zigzag >= 32
        let mut b = Vec::new();
        write_tlong(&mut b, 999);
        // zigzag(999) = 1998 = 0b11111001110; low5=0b01110=14, upper=62
        assert_eq!(b, vec![0x20 | 14, 62]);

        // 01: second precision
        let mut b = Vec::new();
        write_tlong(&mut b, 2000); // 2s
        assert_eq!(b, vec![SECOND_ENCODING | 4]); // zigzag(2)=4

        // 10: hour precision
        let mut b = Vec::new();
        write_tlong(&mut b, 2 * HOUR + HOUR); // 3h, not multiple of day
        assert_eq!(b, vec![HOUR_ENCODING | 6]); // zigzag(3)=6

        // 11: day precision
        let mut b = Vec::new();
        write_tlong(&mut b, 2 * DAY);
        assert_eq!(b, vec![DAY_ENCODING | 4]); // zigzag(2)=4

        // day precision with continuation: 20 days => zigzag(20)=40=0b101000
        let mut b = Vec::new();
        write_tlong(&mut b, 20 * DAY);
        assert_eq!(b, vec![DAY_ENCODING | 0x20 | 8, 1]); // low5=8, upper=1
    }

    #[test]
    fn zfloat_branches() {
        let mut b = Vec::new();
        write_zfloat(&mut b, 1.0);
        assert_eq!(b, vec![0x80 | 2]);
        let mut b = Vec::new();
        write_zfloat(&mut b, -1.0);
        assert_eq!(b, vec![0x80]); // 0x80 | (1 + -1)

        // -0.0 must NOT take the single-byte branch
        let mut b = Vec::new();
        write_zfloat(&mut b, -0.0);
        assert_eq!(b.len(), 5);
        assert_eq!(b[0], 0xFF);

        // positive non-integral: 4 bytes
        let mut b = Vec::new();
        write_zfloat(&mut b, 1.5);
        assert_eq!(b.len(), 4);
        let bits = 1.5f32.to_bits();
        assert_eq!(
            b,
            vec![
                (bits >> 24) as u8,
                (bits >> 8) as u8,           // LE short low byte: bits 8..16
                ((bits >> 16) & 0xff) as u8, // LE short high byte: bits 16..24
                bits as u8
            ]
        );

        // negative non-integral: 5 bytes
        let mut b = Vec::new();
        write_zfloat(&mut b, -1.5);
        assert_eq!(b.len(), 5);
        assert_eq!(b[0], 0xFF);
        assert_eq!(&b[1..], &(-1.5f32).to_bits().to_le_bytes());
    }

    #[test]
    fn zdouble_branches() {
        let mut b = Vec::new();
        write_zdouble(&mut b, 3.0);
        assert_eq!(b, vec![0x80 | 4]);

        // float-representable: 5 bytes
        let mut b = Vec::new();
        write_zdouble(&mut b, 0.5);
        assert_eq!(b.len(), 5);
        assert_eq!(b[0], 0xFE);
        assert_eq!(&b[1..], &0.5f32.to_bits().to_le_bytes());

        // positive, not float-representable: 8 bytes
        let mut b = Vec::new();
        write_zdouble(&mut b, 0.1);
        assert_eq!(b.len(), 8);
        let bits = 0.1f64.to_bits();
        assert_eq!(
            b,
            vec![
                (bits >> 56) as u8,
                (bits >> 24) as u8, // LE int of bits 24..56
                (bits >> 32) as u8,
                (bits >> 40) as u8,
                (bits >> 48) as u8,
                (bits >> 8) as u8, // LE short of bits 8..24
                (bits >> 16) as u8,
                bits as u8
            ]
        );

        // negative: 9 bytes
        let mut b = Vec::new();
        write_zdouble(&mut b, -0.1);
        assert_eq!(b.len(), 9);
        assert_eq!(b[0], 0xFF);
        assert_eq!(&b[1..], &(-0.1f64).to_bits().to_le_bytes());
    }

    #[test]
    fn lz4_round_trip_block_format() {
        // data with repetition so LZ4 actually finds matches
        let mut raw = Vec::new();
        for i in 0..2000 {
            raw.extend_from_slice(format!("doc-{i:04}-the-quick-brown-fox. ").as_bytes());
        }
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        compress_lz4(&raw, &mut out).unwrap();
        let bytes = out.into_bytes();

        // decode the stream: VInt dictLength, VInt blockLength, length table, data
        let mut pos = 0;
        let read_vint = |pos: &mut usize| -> usize {
            let mut v = 0usize;
            let mut shift = 0;
            loop {
                let b = bytes[*pos];
                *pos += 1;
                v |= ((b & 0x7f) as usize) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            v
        };
        let dict_length = read_vint(&mut pos);
        let block_length = read_vint(&mut pos);
        // single-sub-block layout: no dict, one whole-chunk block
        assert_eq!(dict_length, 0);
        assert_eq!(block_length, raw.len());
        let num_blocks = (raw.len() - dict_length).div_ceil(block_length);
        let mut lengths = Vec::new();
        for _ in 0..=num_blocks {
            lengths.push(read_vint(&mut pos));
        }
        // decompress dict and sub-blocks, concatenate
        let mut decoded = Vec::new();
        for (i, &l) in lengths.iter().enumerate() {
            let want = if i == 0 {
                dict_length
            } else {
                let remaining = raw.len() - dict_length - (i - 1) * block_length;
                remaining.min(block_length)
            };
            let block = lz4::block::decompress(&bytes[pos..pos + l], Some(want as i32))
                .expect("lz4 decompress");
            decoded.extend_from_slice(&block);
            pos += l;
        }
        assert_eq!(decoded, raw);
        assert_eq!(pos, bytes.len());
    }

    #[test]
    fn fdt_header_length_matches_java_assert() {
        // writer ctor asserts indexHeaderLength(formatName, "") == filePointer (:142-143)
        assert_eq!(index_header_length(FDT_CODEC_NAME, ""), 54);
        assert_eq!(index_header_length("Lucene90FieldsIndexIdx", ""), 48);
        assert_eq!(index_header_length("Lucene90FieldsIndexMeta", ""), 49);
    }
}
