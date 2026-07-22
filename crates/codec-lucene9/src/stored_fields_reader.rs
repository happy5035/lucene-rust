//! Stored fields reader (LZ4 decompress), mirroring
//! `codecs/lucene90/compressing/Lucene90CompressingStoredFieldsReader.java`
//! and `codecs/lucene90/LZ4WithPresetDictDecompressor.java` (9.12.3).
//!
//! Reads `_X.fdt` (compressed chunks), `_X.fdx` (DirectMonotonic index),
//! and `_X.fdm` (metadata). Supports `binary_search` to locate the chunk
//! containing a doc_id, then LZ4 decompresses the chunk and iterates
//! stored fields via a visitor.

use std::io;

use crate::codec_util::{check_footer, skip_index_header};
use crate::directory::FSDirectory;
use crate::io::{HeapIndexInput, IndexInput};
use crate::packed::DirectReader;
use crate::stored_fields::file_names;

// ---------------------------------------------------------------------------
// Constants mirroring stored_fields.rs
// ---------------------------------------------------------------------------

/// Lucene90StoredFieldsFormat.Mode.BEST_SPEED parameters
pub const CHUNK_SIZE: usize = 81920;

// Field type tags (TYPE_BITS = 3).
const TYPE_STRING: i64 = 0;
const TYPE_BYTE_ARR: i64 = 1;
const TYPE_NUMERIC_INT: i64 = 2;
const TYPE_NUMERIC_FLOAT: i64 = 3;
const TYPE_NUMERIC_LONG: i64 = 4;
const TYPE_NUMERIC_DOUBLE: i64 = 5;

// TLong encodings
const SECOND: i64 = 1000;
const HOUR: i64 = 60 * 60 * SECOND;
const DAY: i64 = 24 * HOUR;
const SECOND_ENCODING: u8 = 0x40;
const HOUR_ENCODING: u8 = 0x80;
const DAY_ENCODING: u8 = 0xC0;

// StoredFieldsInts block size
const SF_BLOCK: usize = 128;

// ---------------------------------------------------------------------------
// Low-level LE read helpers (IndexInput only exposes read_byte / read_bytes)
// ---------------------------------------------------------------------------

fn read_le_i16(input: &mut dyn IndexInput) -> io::Result<i16> {
    let mut buf = [0u8; 2];
    input.read_bytes(&mut buf, 0, 2)?;
    Ok(i16::from_le_bytes(buf))
}

fn read_le_i32(input: &mut dyn IndexInput) -> io::Result<i32> {
    let mut buf = [0u8; 4];
    input.read_bytes(&mut buf, 0, 4)?;
    Ok(i32::from_le_bytes(buf))
}

fn read_le_u32(input: &mut dyn IndexInput) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    input.read_bytes(&mut buf, 0, 4)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_le_i64(input: &mut dyn IndexInput) -> io::Result<i64> {
    let mut buf = [0u8; 8];
    input.read_bytes(&mut buf, 0, 8)?;
    Ok(i64::from_le_bytes(buf))
}

fn read_le_u64(input: &mut dyn IndexInput) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    input.read_bytes(&mut buf, 0, 8)?;
    Ok(u64::from_le_bytes(buf))
}

// ---------------------------------------------------------------------------
// Raw VInt/VLong reading from byte slices (decompressed chunk data)
// ---------------------------------------------------------------------------

/// Read a raw VLong from a byte slice (no sign check, used for field metadata).
/// Returns the decoded value and advances `pos`.
fn read_raw_vlong(buf: &[u8], pos: &mut usize) -> u64 {
    let mut v: u64 = 0;
    let mut shift = 0;
    loop {
        let b = buf[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    v
}

/// Read a raw VInt (non-negative) from a byte slice.
fn read_raw_vint(buf: &[u8], pos: &mut usize) -> i32 {
    read_raw_vlong(buf, pos) as i32
}

/// Read a zigzag-encoded ZInt from a byte slice.
fn read_raw_zint(buf: &[u8], pos: &mut usize) -> i32 {
    let v = read_raw_vlong(buf, pos);
    ((v >> 1) as i64 ^ -((v & 1) as i64)) as i32
}

// ---------------------------------------------------------------------------
// Compressed numeric decoding (TLong, ZFloat, ZDouble)
// ---------------------------------------------------------------------------

/// Read a TLong-encoded value from a byte slice (mirrors write_tlong).
fn read_tlong(buf: &[u8], pos: &mut usize) -> i64 {
    let header = buf[*pos];
    *pos += 1;
    let encoding = header & 0xC0; // top 2 bits
    let mut zigzag = (header & 0x1f) as u64;
    if header & 0x20 != 0 {
        // continuation bits
        let upper = read_raw_vlong(buf, pos);
        zigzag |= upper << 5;
    }
    let value = ((zigzag >> 1) as i64) ^ -((zigzag & 1) as i64);
    match encoding {
        0 => value,
        SECOND_ENCODING => value * SECOND,
        HOUR_ENCODING => value * HOUR,
        DAY_ENCODING => value * DAY,
        _ => unreachable!(),
    }
}

/// Read a ZFloat-encoded value from a byte slice (mirrors write_zfloat).
fn read_zfloat(buf: &[u8], pos: &mut usize) -> f32 {
    let b = buf[*pos];
    *pos += 1;
    if b == 0xFF {
        // negative non-integral: 4 raw bytes follow
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&buf[*pos..*pos + 4]);
        *pos += 4;
        f32::from_le_bytes(raw)
    } else if b & 0x80 != 0 {
        // single-byte encoding: value = (b & 0x7F) - 1
        ((b & 0x7f) as i32 - 1) as f32
    } else {
        // positive non-integral: 3 more bytes follow
        let b1 = buf[*pos];
        let b2 = buf[*pos + 1];
        let b3 = buf[*pos + 2];
        *pos += 3;
        let bits = (b as u32) << 24 | (b1 as u32) << 8 | (b2 as u32) << 16 | (b3 as u32);
        f32::from_bits(bits)
    }
}

/// Read a ZDouble-encoded value from a byte slice (mirrors write_zdouble).
fn read_zdouble(buf: &[u8], pos: &mut usize) -> f64 {
    let b = buf[*pos];
    *pos += 1;
    if b == 0xFF {
        // negative non-integral: 8 raw bytes follow
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&buf[*pos..*pos + 8]);
        *pos += 8;
        f64::from_le_bytes(raw)
    } else if b == 0xFE {
        // float-representable: 4 float bytes follow
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&buf[*pos..*pos + 4]);
        *pos += 4;
        f32::from_le_bytes(raw) as f64
    } else if b & 0x80 != 0 {
        // single-byte encoding: value = (b & 0x7F) - 1
        ((b & 0x7f) as i32 - 1) as f64
    } else {
        // positive, not float-representable: 7 more bytes follow
        let b1 = buf[*pos];
        let b2 = buf[*pos + 1];
        let b3 = buf[*pos + 2];
        let b4 = buf[*pos + 3];
        let b5 = buf[*pos + 4];
        let b6 = buf[*pos + 5];
        let b7 = buf[*pos + 6];
        *pos += 7;
        let bits = (b as u64) << 56
            | (b1 as u64) << 24
            | (b2 as u64) << 32
            | (b3 as u64) << 40
            | (b4 as u64) << 48
            | (b5 as u64) << 8
            | (b6 as u64) << 16
            | b7 as u64;
        f64::from_bits(bits)
    }
}

// ---------------------------------------------------------------------------
// StoredFieldsInts reading (reverse of stored_fields_write_ints / save_ints)
// ---------------------------------------------------------------------------

/// Read values encoded by `stored_fields_write_ints` from an IndexInput.
/// `count` is the known number of values.
fn read_stored_fields_ints(input: &mut dyn IndexInput, count: usize) -> io::Result<Vec<i32>> {
    let bpv_byte = input.read_byte()?;
    match bpv_byte {
        0 => {
            // All equal: single VInt
            let val = input.read_vint()?;
            Ok(vec![val; count])
        }
        8 => read_ints8(input, count),
        16 => read_ints16(input, count),
        32 => read_ints32(input, count),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected stored fields ints bpv: {other}"),
        )),
    }
}

/// Read values encoded by `save_ints` (which is either a bare VInt for count==1
/// or `stored_fields_write_ints` for count>1).
fn read_save_ints(input: &mut dyn IndexInput, count: usize) -> io::Result<Vec<i32>> {
    if count == 1 {
        Ok(vec![input.read_vint()?])
    } else {
        read_stored_fields_ints(input, count)
    }
}

/// Read bpv-8 interleaved values (16 longs per 128-value block, then raw tail).
fn read_ints8(input: &mut dyn IndexInput, count: usize) -> io::Result<Vec<i32>> {
    let mut values = vec![0i32; count];
    let mut k = 0;
    while k + SF_BLOCK <= count {
        for i in 0..16 {
            let l = read_le_u64(input)?;
            values[k + i] = ((l >> 56) & 0xFF) as i32;
            values[k + 16 + i] = ((l >> 48) & 0xFF) as i32;
            values[k + 32 + i] = ((l >> 40) & 0xFF) as i32;
            values[k + 48 + i] = ((l >> 32) & 0xFF) as i32;
            values[k + 64 + i] = ((l >> 24) & 0xFF) as i32;
            values[k + 80 + i] = ((l >> 16) & 0xFF) as i32;
            values[k + 96 + i] = ((l >> 8) & 0xFF) as i32;
            values[k + 112 + i] = (l & 0xFF) as i32;
        }
        k += SF_BLOCK;
    }
    for v in &mut values[k..] {
        *v = input.read_byte()? as i32;
    }
    Ok(values)
}

/// Read bpv-16 interleaved values (32 longs per 128-value block, then LE short tail).
fn read_ints16(input: &mut dyn IndexInput, count: usize) -> io::Result<Vec<i32>> {
    let mut values = vec![0i32; count];
    let mut k = 0;
    while k + SF_BLOCK <= count {
        for i in 0..32 {
            let l = read_le_u64(input)?;
            values[k + i] = ((l >> 48) & 0xFFFF) as i32;
            values[k + 32 + i] = ((l >> 32) & 0xFFFF) as i32;
            values[k + 64 + i] = ((l >> 16) & 0xFFFF) as i32;
            values[k + 96 + i] = (l & 0xFFFF) as i32;
        }
        k += SF_BLOCK;
    }
    for v in &mut values[k..] {
        *v = read_le_i16(input)? as i32;
    }
    Ok(values)
}

/// Read bpv-32 interleaved values (64 longs per 128-value block, then LE int tail).
fn read_ints32(input: &mut dyn IndexInput, count: usize) -> io::Result<Vec<i32>> {
    let mut values = vec![0i32; count];
    let mut k = 0;
    while k + SF_BLOCK <= count {
        for i in 0..64 {
            let l = read_le_u64(input)?;
            values[k + i] = ((l >> 32) & 0xFFFFFFFF) as i32;
            values[k + 64 + i] = (l & 0xFFFFFFFF) as i32;
        }
        k += SF_BLOCK;
    }
    for v in &mut values[k..] {
        *v = read_le_i32(input)?;
    }
    Ok(values)
}

// ---------------------------------------------------------------------------
// DirectMonotonic decoding from split meta/data streams
// ---------------------------------------------------------------------------

/// Compute DirectWriter padding bytes for a given bits-per-value.
fn direct_writer_padding(bpv: u32) -> usize {
    let padding_bits = if bpv > 32 {
        64 - bpv
    } else if bpv > 16 {
        32 - bpv
    } else if bpv > 8 {
        16 - bpv
    } else {
        0
    };
    padding_bits.div_ceil(8) as usize
}

/// Packed byte count (PackedInts.Format.PACKED.byteCount).
fn packed_byte_count(num_values: usize, bits_per_value: u32) -> usize {
    (num_values * bits_per_value as usize).div_ceil(8)
}

/// Decode a DirectMonotonic sequence from split streams.
///
/// `meta_input` points to the start of the meta block sequence in .fdm.
/// `data_input` is the .fdx file (seekable).
/// `data_base` is the absolute position in `data_input` where packed data starts
/// (e.g., `docs_start_pointer`).
///
/// Reads all meta blocks from `meta_input`, then reads corresponding packed
/// deltas from `data_input`, and reconstructs the monotonic values.
fn decode_direct_monotonic(
    meta_input: &mut dyn IndexInput,
    data_input: &mut dyn IndexInput,
    value_count: usize,
    block_shift: u32,
    data_base: u64,
) -> io::Result<Vec<u64>> {
    if value_count == 0 {
        return Ok(Vec::new());
    }

    let block_size = 1usize << block_shift;
    let num_blocks = (value_count + block_size - 1) / block_size;

    // Phase 1: read meta blocks
    struct MetaBlock {
        min: i64,
        avg_inc: f32,
        offset: u64,
        bpv: u8,
    }
    let mut meta_blocks = Vec::with_capacity(num_blocks);
    for _ in 0..num_blocks {
        let min = read_le_i64(meta_input)?;
        let avg_inc_bits = read_le_u32(meta_input)?;
        let avg_inc = f32::from_bits(avg_inc_bits);
        let offset = read_le_u64(meta_input)?;
        let bpv = meta_input.read_byte()?;
        meta_blocks.push(MetaBlock { min, avg_inc, offset, bpv });
    }

    // Phase 2: read delta data and reconstruct values
    let mut values = vec![0u64; value_count];
    for (block_idx, mb) in meta_blocks.iter().enumerate() {
        let block_start = block_idx * block_size;
        let block_end = (block_start + block_size).min(value_count);
        let block_len = block_end - block_start;

        if mb.bpv == 0 {
            // All deltas are zero
            for i in block_start..block_end {
                let in_block = (i - block_start) as u64;
                let expected = (mb.avg_inc * in_block as f32) as i64;
                values[i] = expected.wrapping_add(mb.min) as u64;
            }
        } else {
            let bpv = mb.bpv as u32;
            let delta_byte_count = packed_byte_count(block_len, bpv);
            let padding = direct_writer_padding(bpv);
            let total_delta_bytes = delta_byte_count + padding;

            data_input.seek(data_base + mb.offset)?;
            let mut buf = vec![0u8; total_delta_bytes];
            data_input.read_bytes(&mut buf, 0, total_delta_bytes)?;

            let mut dr = DirectReader::new(
                Box::new(HeapIndexInput::new(buf)),
                bpv,
                block_len,
                0,
            );

            for i in block_start..block_end {
                let in_block = (i - block_start) as u64;
                let delta = dr.get(i - block_start)?;
                let expected = (mb.avg_inc * in_block as f32) as i64;
                values[i] = expected.wrapping_add(mb.min).wrapping_add(delta as i64) as u64;
            }
        }
    }

    Ok(values)
}

// ---------------------------------------------------------------------------
// LZ4 decompression of a single compress_lz4 block
// ---------------------------------------------------------------------------

/// Decompress one `compress_lz4` block from `input`.
/// `expected_uncompressed` is the total uncompressed size that this block
/// should produce (needed to compute the number of sub-block length entries
/// when the data could be empty).
/// Returns the decompressed bytes.
fn decompress_lz4_block(
    input: &mut dyn IndexInput,
    expected_uncompressed: usize,
) -> io::Result<Vec<u8>> {
    let dict_length = input.read_vint()? as usize;
    let block_length = input.read_vint()? as usize;

    // Compute number of sub-blocks (including dict block).
    // With dict_length=0 and block_length=max(len,1), this gives:
    //   len>0 => 2 blocks (dict + 1 data sub-block)
    //   len=0 => 1 block  (dict only)
    let total_len = expected_uncompressed;
    let num_blocks = if total_len == 0 && dict_length == 0 {
        1
    } else {
        (total_len - dict_length + block_length - 1) / block_length + 1
    };

    // Read compressed length table
    let mut compressed_lengths = Vec::with_capacity(num_blocks);
    for _ in 0..num_blocks {
        compressed_lengths.push(input.read_vint()? as usize);
    }

    // Decompress each block and concatenate
    let mut result = Vec::with_capacity(total_len);
    for (i, &clen) in compressed_lengths.iter().enumerate() {
        let mut compressed = vec![0u8; clen];
        input.read_bytes(&mut compressed, 0, clen)?;

        let block_uncompressed = if i == 0 {
            dict_length
        } else {
            let remaining = total_len - dict_length - (i - 1) * block_length;
            remaining.min(block_length)
        };

        if block_uncompressed > 0 {
            let decompressed = lz4::block::decompress(&compressed, Some(block_uncompressed as i32))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("lz4 decompress: {e}")))?;
            result.extend_from_slice(&decompressed);
        }
        // dict block (i==0 and dict_length==0) is a no-op
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// StoredFieldVisitor trait
// ---------------------------------------------------------------------------

/// Visitor for stored field values, mirroring
/// `index/StoredFieldVisitor.java` (9.12.3).
pub trait StoredFieldVisitor {
    /// Called for each string field.
    fn string_field(&mut self, field_number: u32, value: &str) -> io::Result<()>;
    /// Called for each binary field.
    fn binary_field(&mut self, field_number: u32, value: &[u8]) -> io::Result<()>;
    /// Called for each int field.
    fn int_field(&mut self, field_number: u32, value: i32) -> io::Result<()>;
    /// Called for each long field.
    fn long_field(&mut self, field_number: u32, value: i64) -> io::Result<()>;
    /// Called for each float field.
    fn float_field(&mut self, field_number: u32, value: f32) -> io::Result<()>;
    /// Called for each double field.
    fn double_field(&mut self, field_number: u32, value: f64) -> io::Result<()>;
}

// ---------------------------------------------------------------------------
// StoredFieldsReader
// ---------------------------------------------------------------------------

/// Reader for `_X.fdt` / `_X.fdx` / `_X.fdm` stored fields files.
///
/// Binary-searches the chunk index to locate a document's compressed data,
/// decompresses the chunk with LZ4, and iterates stored fields via a
/// [`StoredFieldVisitor`].
pub struct StoredFieldsReader {
    fdt_input: Box<dyn IndexInput>,
    num_chunks: usize,
    doc_bases: Vec<u64>,
    file_pointers: Vec<u64>,
}

impl StoredFieldsReader {
    /// Opens the stored fields files for a segment.
    ///
    /// Reads `.fdm` (metadata), `.fdx` (index), and `.fdt` (data),
    /// validates headers and footers, and decodes the chunk index.
    pub fn open(dir: &FSDirectory, segment: &str, suffix: &str) -> io::Result<Self> {
        let [fdt_name, fdx_name, fdm_name] = file_names(segment, suffix);

        // --- .fdm: metadata ---
        let mut fdm_input = dir.open_input(&fdm_name)?;
        skip_index_header(&mut *fdm_input)?;

        let _chunk_size = fdm_input.read_vint()?; // CHUNK_SIZE, not needed for reading
        let _num_docs = read_le_i32(&mut *fdm_input)?;
        let block_shift = read_le_i32(&mut *fdm_input)? as u32;
        let total_chunks_plus_1 = read_le_i32(&mut *fdm_input)? as usize;

        if total_chunks_plus_1 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stored fields: total_chunks+1 is zero",
            ));
        }

        let total_chunks = total_chunks_plus_1 - 1;
        let docs_start_pointer = read_le_u64(&mut *fdm_input)?;

        // Decode doc_bases DirectMonotonic: meta from .fdm, deltas from .fdx
        let mut fdx_input = dir.open_input(&fdx_name)?;
        skip_index_header(&mut *fdx_input)?;

        let doc_bases_count = total_chunks + 1;
        let doc_bases = decode_direct_monotonic(
            &mut *fdm_input,
            &mut *fdx_input,
            doc_bases_count,
            block_shift,
            docs_start_pointer,
        )?;

        // Read startPointersStartPointer (after doc_bases meta blocks)
        let fps_start_pointer = read_le_u64(&mut *fdm_input)?;

        // Decode file_pointers DirectMonotonic
        let file_pointers = decode_direct_monotonic(
            &mut *fdm_input,
            &mut *fdx_input,
            doc_bases_count,
            block_shift,
            fps_start_pointer,
        )?;

        // Remaining .fdm fields
        let _start_pointers_end = read_le_u64(&mut *fdm_input)?;
        let _max_pointer = read_le_u64(&mut *fdm_input)?;
        let num_chunks = fdm_input.read_vlong()? as usize;
        let _num_dirty_chunks = fdm_input.read_vlong()?;
        let _num_dirty_docs = fdm_input.read_vlong()?;

        if num_chunks != total_chunks {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "stored fields: numChunks mismatch: meta says {num_chunks}, index says {total_chunks}"
                ),
            ));
        }

        check_footer(&mut *fdm_input)?;
        check_footer(&mut *fdx_input)?;

        // --- .fdt: data ---
        let mut fdt_input = dir.open_input(&fdt_name)?;
        skip_index_header(&mut *fdt_input)?;

        Ok(StoredFieldsReader {
            fdt_input,
            num_chunks,
            doc_bases,
            file_pointers,
        })
    }

    /// Visit the stored fields of a single document.
    ///
    /// Binary-searches to find the chunk containing `doc_id`, seeks into the
    /// `.fdt` data file, decompresses the chunk with LZ4, and calls the
    /// [`StoredFieldVisitor`] for each field of the target document.
    pub fn visit_document(
        &mut self,
        doc_id: u32,
        visitor: &mut dyn StoredFieldVisitor,
    ) -> io::Result<()> {
        // 1. Binary search chunk containing doc_id
        let doc_id_u64 = doc_id as u64;
        let chunk_idx = binary_search_chunk(&self.doc_bases, doc_id_u64)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("doc_id {doc_id} not found in stored fields"),
                )
            })?;

        // 2. Seek to chunk offset in .fdt
        self.fdt_input
            .seek(self.file_pointers[chunk_idx])?;

        // 3. Read chunk header
        let chunk_doc_base = self.fdt_input.read_vint()?;
        let num_docs_with_flags = self.fdt_input.read_vint()?;
        let num_docs = (num_docs_with_flags >> 2) as usize;
        let sliced = (num_docs_with_flags & 1) != 0;
        // dirty_bit (bit 1) is not needed for reading

        if num_docs == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stored fields chunk with zero docs",
            ));
        }

        let num_stored_fields = read_save_ints(&mut *self.fdt_input, num_docs)?;
        let lengths = read_save_ints(&mut *self.fdt_input, num_docs)?;

        // 4. LZ4 decompress chunk
        let total_uncompressed: usize = lengths.iter().map(|&l| l as usize).sum();
        let decompressed = if sliced {
            // Multiple LZ4 blocks concatenated
            let num_slices = (total_uncompressed + CHUNK_SIZE - 1) / CHUNK_SIZE;
            let mut result = Vec::with_capacity(total_uncompressed);
            for slice_idx in 0..num_slices {
                let expected = if slice_idx + 1 < num_slices {
                    CHUNK_SIZE
                } else {
                    total_uncompressed - slice_idx * CHUNK_SIZE
                };
                let block = decompress_lz4_block(&mut *self.fdt_input, expected)?;
                result.extend_from_slice(&block);
            }
            result
        } else {
            decompress_lz4_block(&mut *self.fdt_input, total_uncompressed)?
        };

        // 5. Find and iterate target doc's fields
        let target_offset_in_chunk = (doc_id_u64 - chunk_doc_base as u64) as usize;
        if target_offset_in_chunk >= num_docs {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "doc_id {doc_id} not in chunk [{}..{})",
                    chunk_doc_base,
                    chunk_doc_base as usize + num_docs
                ),
            ));
        }

        // Compute byte range of the target doc within decompressed data:
        // lengths[0] is the byte length of doc at offset 0,
        // end_offsets[i] = sum of lengths[0..=i]
        let mut start_offset: usize = 0;
        for i in 0..target_offset_in_chunk {
            start_offset += lengths[i] as usize;
        }
        let doc_length = lengths[target_offset_in_chunk] as usize;
        let end_offset = start_offset + doc_length;

        if end_offset > decompressed.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "stored fields: doc data exceeds decompressed chunk (offset {end_offset} > len {})",
                    decompressed.len()
                ),
            ));
        }

        let doc_data = &decompressed[start_offset..end_offset];
        let nf = num_stored_fields[target_offset_in_chunk] as usize;

        // 6. Decode each field, call visitor
        decode_fields(doc_data, nf, visitor)
    }

    /// Number of chunks in this segment.
    pub fn num_chunks(&self) -> usize {
        self.num_chunks
    }
}

// ---------------------------------------------------------------------------
// Binary search helper
// ---------------------------------------------------------------------------

/// Binary search in a monotonically increasing `doc_bases` array.
/// `doc_bases[i]` is the first doc_id in chunk i; `doc_bases[i+1]` is the
/// first doc_id in chunk i+1. Returns the chunk index for `doc_id`, or None.
fn binary_search_chunk(doc_bases: &[u64], doc_id: u64) -> Option<usize> {
    if doc_bases.len() < 2 {
        return None;
    }
    let mut lo = 0usize;
    let mut hi = doc_bases.len() - 2; // last chunk index
    while lo <= hi {
        let mid = (lo + hi) / 2;
        if doc_id >= doc_bases[mid] {
            if doc_id < doc_bases[mid + 1] {
                return Some(mid);
            }
            lo = mid + 1;
        } else {
            if mid == 0 {
                return None;
            }
            hi = mid - 1;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Field decoding (from a decompressed byte slice)
// ---------------------------------------------------------------------------

/// Decode fields from a raw byte slice (one document's data).
fn decode_fields(
    data: &[u8],
    num_fields: usize,
    visitor: &mut dyn StoredFieldVisitor,
) -> io::Result<()> {
    let mut pos: usize = 0;
    for _ in 0..num_fields {
        if pos >= data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stored fields: unexpected end of document data",
            ));
        }

        let info_and_bits = read_raw_vlong(data, &mut pos);
        let field_number = (info_and_bits >> 3) as u32;
        let type_tag = (info_and_bits & 0x7) as i64;

        match type_tag {
            TYPE_STRING => {
                let len = read_raw_vint(data, &mut pos) as usize;
                let end = pos + len;
                if end > data.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "stored fields: string extends past document data",
                    ));
                }
                let s = std::str::from_utf8(&data[pos..end])
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                pos = end;
                visitor.string_field(field_number, s)?;
            }
            TYPE_BYTE_ARR => {
                let len = read_raw_vint(data, &mut pos) as usize;
                let end = pos + len;
                if end > data.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "stored fields: bytes extends past document data",
                    ));
                }
                let bytes = &data[pos..end];
                pos = end;
                visitor.binary_field(field_number, bytes)?;
            }
            TYPE_NUMERIC_INT => {
                let v = read_raw_zint(data, &mut pos);
                visitor.int_field(field_number, v)?;
            }
            TYPE_NUMERIC_LONG => {
                let v = read_tlong(data, &mut pos);
                visitor.long_field(field_number, v)?;
            }
            TYPE_NUMERIC_FLOAT => {
                let v = read_zfloat(data, &mut pos);
                visitor.float_field(field_number, v)?;
            }
            TYPE_NUMERIC_DOUBLE => {
                let v = read_zdouble(data, &mut pos);
                visitor.double_field(field_number, v)?;
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("stored fields: unknown type tag {other}"),
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stored_fields::{StoredField, StoredFieldsWriter};

    /// Test visitor that collects fields into a Vec.
    struct CollectingVisitor {
        fields: Vec<(u32, String)>,
    }

    impl CollectingVisitor {
        fn new() -> Self {
            CollectingVisitor { fields: Vec::new() }
        }
    }

    impl StoredFieldVisitor for CollectingVisitor {
        fn string_field(&mut self, field_number: u32, value: &str) -> io::Result<()> {
            self.fields
                .push((field_number, format!("s:{value}")));
            Ok(())
        }
        fn binary_field(&mut self, field_number: u32, value: &[u8]) -> io::Result<()> {
            self.fields
                .push((field_number, format!("b:{value:?}")));
            Ok(())
        }
        fn int_field(&mut self, field_number: u32, value: i32) -> io::Result<()> {
            self.fields
                .push((field_number, format!("i:{value}")));
            Ok(())
        }
        fn long_field(&mut self, field_number: u32, value: i64) -> io::Result<()> {
            self.fields
                .push((field_number, format!("l:{value}")));
            Ok(())
        }
        fn float_field(&mut self, field_number: u32, value: f32) -> io::Result<()> {
            self.fields
                .push((field_number, format!("f:{value}")));
            Ok(())
        }
        fn double_field(&mut self, field_number: u32, value: f64) -> io::Result<()> {
            self.fields
                .push((field_number, format!("d:{value}")));
            Ok(())
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codec-lucene9-sfr-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn round_trip_single_doc_all_types() {
        let root = temp_dir("rt-single");
        let dir = FSDirectory::open(&root).unwrap();
        let seg_id = [0x01u8; 16];

        // Write one document with all field types
        let mut writer = StoredFieldsWriter::new(&dir, "_0", seg_id, "").unwrap();
        writer
            .write_document(&[
                (0, StoredField::String("hello world".to_string())),
                (1, StoredField::Bytes(b"binary-data".to_vec())),
                (2, StoredField::Int(42)),
                (3, StoredField::Long(12345678901234i64)),
                (4, StoredField::Float(3.14f32)),
                (5, StoredField::Double(2.718281828459045f64)),
                (6, StoredField::Int(-100)),
                (7, StoredField::Long(-9999999999i64)),
                (8, StoredField::Float(-1.5f32)),
                (9, StoredField::Double(-0.5f64)),
                (10, StoredField::String("".to_string())),
            ])
            .unwrap();
        let _stats = writer.finish(1, &dir).unwrap();

        // Read back
        let mut reader = StoredFieldsReader::open(&dir, "_0", "").unwrap();
        let mut visitor = CollectingVisitor::new();
        reader.visit_document(0, &mut visitor).unwrap();

        assert_eq!(
            visitor.fields,
            vec![
                (0, "s:hello world".to_string()),
                (1, "b:[98, 105, 110, 97, 114, 121, 45, 100, 97, 116, 97]".to_string()),
                (2, "i:42".to_string()),
                (3, "l:12345678901234".to_string()),
                (4, "f:3.14".to_string()),
                (5, "d:2.718281828459045".to_string()),
                (6, "i:-100".to_string()),
                (7, "l:-9999999999".to_string()),
                (8, "f:-1.5".to_string()),
                (9, "d:-0.5".to_string()),
                (10, "s:".to_string()),
            ]
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn round_trip_many_docs_many_fields() {
        let root = temp_dir("rt-many");
        let dir = FSDirectory::open(&root).unwrap();
        let seg_id = [0x02u8; 16];

        let mut writer = StoredFieldsWriter::new(&dir, "_0", seg_id, "").unwrap();

        let num_docs: i32 = 500;
        for doc in 0..num_docs {
            writer.start_document();
            // Each doc gets 2-5 fields
            let field_count = 2 + (doc % 4) as u32;
            for f in 0..field_count {
                let field_num = f;
                let value = match (doc + f as i32) as usize % 6 {
                    0 => StoredField::String(format!("doc{doc}-f{f}")),
                    1 => StoredField::Int(doc as i32 * 100 + f as i32),
                    2 => StoredField::Long(doc as i64 * 1000000 + f as i64),
                    3 => StoredField::Float((doc as f32) * 1.5 + f as f32),
                    4 => StoredField::Double((doc as f64) * 1.5 + f as f64),
                    5 => StoredField::Bytes(vec![doc as u8, f as u8]),
                    _ => unreachable!(),
                };
                writer.write_field(field_num, &value);
            }
            writer.finish_document().unwrap();
        }
        let _stats = writer.finish(num_docs, &dir).unwrap();

        // Read back and verify all documents
        let mut reader = StoredFieldsReader::open(&dir, "_0", "").unwrap();

        for doc in 0..num_docs as u32 {
            let mut visitor = CollectingVisitor::new();
            reader.visit_document(doc, &mut visitor).unwrap();

            let field_count = 2 + (doc % 4);
            assert_eq!(
                visitor.fields.len(),
                field_count as usize,
                "doc {doc}: wrong field count"
            );

            for (fi, (field_num, display)) in visitor.fields.iter().enumerate() {
                let f = fi as u32;
                assert_eq!(*field_num, f, "doc {doc}: wrong field number");
                match (doc as usize + fi) % 6 {
                    0 => assert_eq!(display, &format!("s:doc{doc}-f{f}")),
                    1 => assert_eq!(display, &format!("i:{}", doc as i32 * 100 + f as i32)),
                    2 => assert_eq!(
                        display,
                        &format!("l:{}", doc as i64 * 1000000 + f as i64)
                    ),
                    3 => assert_eq!(
                        display,
                        &format!("f:{}", doc as f32 * 1.5 + f as f32)
                    ),
                    4 => assert_eq!(
                        display,
                        &format!("d:{}", doc as f64 * 1.5 + f as f64)
                    ),
                    5 => assert_eq!(display, &format!("b:[{}, {}]", doc as u8, f as u8)),
                    _ => unreachable!(),
                }
            }
        }

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn round_trip_large_sliced_chunk() {
        // Write enough data to trigger slicing (2 * CHUNK_SIZE bytes)
        let root = temp_dir("rt-sliced");
        let dir = FSDirectory::open(&root).unwrap();
        let seg_id = [0x03u8; 16];

        let mut writer = StoredFieldsWriter::new(&dir, "_0", seg_id, "").unwrap();

        // Each doc has a ~10KB string, so 20 docs = ~200KB > 2 * CHUNK_SIZE
        let big_string = "x".repeat(10_000);
        let num_docs = 20;
        for doc in 0..num_docs {
            writer
                .write_document(&[
                    (0, StoredField::Int(doc)),
                    (1, StoredField::String(big_string.clone())),
                ])
                .unwrap();
        }
        let _stats = writer.finish(num_docs, &dir).unwrap();

        // Verify
        let mut reader = StoredFieldsReader::open(&dir, "_0", "").unwrap();
        assert!(reader.num_chunks() >= 2, "should have multiple chunks");

        for doc in 0..num_docs as u32 {
            let mut visitor = CollectingVisitor::new();
            reader.visit_document(doc, &mut visitor).unwrap();
            assert_eq!(visitor.fields.len(), 2);
            assert_eq!(visitor.fields[0], (0, format!("i:{doc}")));
            assert_eq!(visitor.fields[1], (1, format!("s:{big_string}")));
        }

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn round_trip_tlong_all_branches() {
        // Test TLong encoding round-trip for all precision branches
        let root = temp_dir("rt-tlong");
        let dir = FSDirectory::open(&root).unwrap();
        let seg_id = [0x04u8; 16];

        let test_values: &[(i64, &str)] = &[
            (1, "raw small"),
            (-1, "raw negative"),
            (999, "continuation bit"),
            (2000, "second"),
            (3 * HOUR, "hour"),
            (2 * DAY, "day"),
            (20 * DAY, "day with continuation"),
        ];

        let num_docs = test_values.len() as i32;
        let mut writer = StoredFieldsWriter::new(&dir, "_0", seg_id, "").unwrap();
        for (i, &(val, _)) in test_values.iter().enumerate() {
            writer
                .write_document(&[(0, StoredField::Int(i as i32)), (1, StoredField::Long(val))])
                .unwrap();
        }
        let _stats = writer.finish(num_docs, &dir).unwrap();

        let mut reader = StoredFieldsReader::open(&dir, "_0", "").unwrap();
        for (i, &(expected, _label)) in test_values.iter().enumerate() {
            let mut visitor = CollectingVisitor::new();
            reader.visit_document(i as u32, &mut visitor).unwrap();
            assert_eq!(
                visitor.fields[1],
                (1, format!("l:{expected}")),
                "tlong mismatch for {_label}"
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn round_trip_zfloat_branches() {
        let root = temp_dir("rt-zfloat");
        let dir = FSDirectory::open(&root).unwrap();
        let seg_id = [0x05u8; 16];

        let values: &[f32] = &[1.0, -1.0, -0.0, 1.5, -1.5, 126.0, 0.0];
        let num_docs = values.len() as i32;

        let mut writer = StoredFieldsWriter::new(&dir, "_0", seg_id, "").unwrap();
        for &v in values {
            writer
                .write_document(&[(0, StoredField::Float(v))])
                .unwrap();
        }
        let _stats = writer.finish(num_docs, &dir).unwrap();

        let mut reader = StoredFieldsReader::open(&dir, "_0", "").unwrap();
        for (i, &expected) in values.iter().enumerate() {
            let mut visitor = CollectingVisitor::new();
            reader.visit_document(i as u32, &mut visitor).unwrap();
            // Round to 6 decimal places for comparison (float precision)
            let expected_str = format!("f:{expected}");
            assert_eq!(
                visitor.fields[0], (0, expected_str),
                "zfloat mismatch for {expected}"
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn round_trip_zdouble_branches() {
        let root = temp_dir("rt-zdouble");
        let dir = FSDirectory::open(&root).unwrap();
        let seg_id = [0x06u8; 16];

        let values: &[f64] = &[
            3.0,
            0.5,     // float-representable
            0.1,     // not float-representable, positive
            -0.1,    // negative
            125.0,   // single-byte boundary
            -1.0,    // single-byte negative
            0.0,
            -0.25,   // negative float-representable? no, -0.25 = -0.25f32 which is exactly representable
        ];

        let num_docs = values.len() as i32;
        let mut writer = StoredFieldsWriter::new(&dir, "_0", seg_id, "").unwrap();
        for &v in values {
            writer
                .write_document(&[(0, StoredField::Double(v))])
                .unwrap();
        }
        let _stats = writer.finish(num_docs, &dir).unwrap();

        let mut reader = StoredFieldsReader::open(&dir, "_0", "").unwrap();
        for (i, &expected) in values.iter().enumerate() {
            let mut visitor = CollectingVisitor::new();
            reader.visit_document(i as u32, &mut visitor).unwrap();
            // Check the value — specific tests for different encoding branches
            let display = &visitor.fields[0].1;
            assert!(
                display.starts_with("d:"),
                "doc {i}: expected double display, got {display}"
            );
            let parsed: f64 = display[2..].parse().unwrap();
            assert!(
                (parsed - expected).abs() < 1e-10,
                "doc {i}: double mismatch: got {parsed}, expected {expected}"
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn empty_string_round_trip() {
        let root = temp_dir("rt-empty-str");
        let dir = FSDirectory::open(&root).unwrap();
        let seg_id = [0x07u8; 16];

        let mut writer = StoredFieldsWriter::new(&dir, "_0", seg_id, "").unwrap();
        writer
            .write_document(&[
                (0, StoredField::String("".to_string())),
                (1, StoredField::String("non-empty".to_string())),
                (2, StoredField::Bytes(vec![])),
                (3, StoredField::Bytes(vec![1, 2, 3])),
            ])
            .unwrap();
        let _stats = writer.finish(1, &dir).unwrap();

        let mut reader = StoredFieldsReader::open(&dir, "_0", "").unwrap();
        let mut visitor = CollectingVisitor::new();
        reader.visit_document(0, &mut visitor).unwrap();

        assert_eq!(
            visitor.fields,
            vec![
                (0, "s:".to_string()),
                (1, "s:non-empty".to_string()),
                (2, "b:[]".to_string()),
                (3, "b:[1, 2, 3]".to_string()),
            ]
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn binary_search_edges() {
        let bases = vec![0, 10, 20, 30, 50];
        // Chunks: [0..10), [10..20), [20..30), [30..50)
        assert_eq!(binary_search_chunk(&bases, 0), Some(0));
        assert_eq!(binary_search_chunk(&bases, 5), Some(0));
        assert_eq!(binary_search_chunk(&bases, 9), Some(0));
        assert_eq!(binary_search_chunk(&bases, 10), Some(1));
        assert_eq!(binary_search_chunk(&bases, 19), Some(1));
        assert_eq!(binary_search_chunk(&bases, 20), Some(2));
        assert_eq!(binary_search_chunk(&bases, 29), Some(2));
        assert_eq!(binary_search_chunk(&bases, 30), Some(3));
        assert_eq!(binary_search_chunk(&bases, 49), Some(3));
        // Out of bounds
        assert_eq!(binary_search_chunk(&bases, 50), None);
        assert_eq!(binary_search_chunk(&bases, 100), None);
        // Single chunk
        let bases2 = vec![0, 100];
        assert_eq!(binary_search_chunk(&bases2, 0), Some(0));
        assert_eq!(binary_search_chunk(&bases2, 99), Some(0));
        assert_eq!(binary_search_chunk(&bases2, 100), None);
        // Empty
        assert_eq!(binary_search_chunk(&[], 0), None);
    }

    #[test]
    fn stored_fields_ints_reader_matches_writer_output() {
        // Verify that read_stored_fields_ints correctly reads what the writer wrote.
        // We use the writer's stored_fields_write_ints to produce bytes, then read back.
        use crate::io::{ChecksumIndexOutput, IndexOutput};

        let test_cases: &[&[i32]] = &[
            &[5, 5, 5],
            &[1, 2, 3],
            &(0..128).collect::<Vec<i32>>(),
            &(0..200).collect::<Vec<i32>>(),
        ];

        for values in test_cases {
            let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
            crate::stored_fields::stored_fields_write_ints(&mut out, values).unwrap();
            let bytes = out.into_bytes();

            let mut input = HeapIndexInput::new(bytes);
            let decoded = read_stored_fields_ints(&mut input, values.len()).unwrap();
            assert_eq!(&decoded, values, "stored fields ints round-trip failed");
        }
    }
}
