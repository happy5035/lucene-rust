//! Lucene90 DocValues writer (NUMERIC + SORTED), mirroring
//! `codecs/lucene90/Lucene90DocValuesConsumer.java`,
//! `codecs/lucene90/IndexedDISI.java` and
//! `codecs/lucene90/Lucene90DocValuesFormat.java` (9.12.3).
//!
//! Writes `{segment}_{suffix}.dvd` (data) / `.dvm` (metadata); see
//! docs/format-notes-docvalues.md for the format ground truth.
//!
//! Documented simplifications vs. Lucene (all read-side compatible,
//! format-notes §11):
//! - NUMERIC: gcd is always 1, table compression / 16384-value blocks / min
//!   normalization are never used (Lucene90DocValuesConsumer.java:275-315);
//!   every non-constant field is a single DirectWriter stream with
//!   valueJumpTableOffset = -1.
//! - terms dict blocks are LZ4-compressed *without* the preset dictionary
//!   (the block's first term). A stream compressed without a dictionary only
//!   contains matches within the decompressed content, so decoding it with
//!   the dictionary preset (Lucene90DocValuesProducer.java:1250-1251) yields
//!   identical bytes. Same argument as stored_fields.rs (lz4_block_compress).

use std::io;

use crate::codec_util::{write_footer, write_index_header};
use crate::directory::FSDirectory;
use crate::io::{ChecksumIndexOutput, IndexOutput};
use crate::packed::{
    direct_monotonic_write, direct_writer_encode, direct_writer_unsigned_bits_required,
};

/// Lucene90DocValuesFormat DATA_CODEC / META_CODEC / extensions (:158-161).
pub(crate) const DATA_CODEC: &str = "Lucene90DocValuesData";
pub(crate) const META_CODEC: &str = "Lucene90DocValuesMetadata";
const DATA_EXTENSION: &str = "dvd";
const META_EXTENSION: &str = "dvm";
/// VERSION_START == VERSION_CURRENT == 0 (:162-163).
pub(crate) const VERSION: u32 = 0;

/// .dvm type bytes (:166-170).
pub(crate) const TYPE_NUMERIC: u8 = 0;
pub(crate) const TYPE_BINARY: u8 = 1;
pub(crate) const TYPE_SORTED: u8 = 2;

/// DIRECT_MONOTONIC_BLOCK_SHIFT (:172): all DirectMonotonic sequences here.
pub(crate) const DIRECT_MONOTONIC_BLOCK_SHIFT: u32 = 16;
/// TERMS_DICT_BLOCK_LZ4_SHIFT (:177-179): 64 terms per dict block.
const TERMS_DICT_BLOCK_LZ4_SHIFT: u32 = 6;
pub(crate) const TERMS_DICT_BLOCK_SIZE: usize = 1 << TERMS_DICT_BLOCK_LZ4_SHIFT;
/// TERMS_DICT_REVERSE_INDEX_SHIFT (:180-183): sample every 1024 terms.
const TERMS_DICT_REVERSE_INDEX_SHIFT: u32 = 10;
pub(crate) const TERMS_DICT_REVERSE_INDEX_SIZE: usize = 1 << TERMS_DICT_REVERSE_INDEX_SHIFT;

/// IndexedDISI constants (:102-108).
pub(crate) const DISI_BLOCK_SIZE: u32 = 65536;
const DISI_BLOCK_LONGS: usize = 1024; // DISI_BLOCK_SIZE / 64
pub(crate) const DISI_MAX_ARRAY_LENGTH: u32 = 4095;
const DISI_DENSE_RANK_POWER: u8 = 9; // DEFAULT_DENSE_RANK_POWER
/// NO_MORE_DOCS >>> 16 (:251): sentinel block id.
pub(crate) const DISI_SENTINEL_BLOCK: u32 = 0x7FFF_FFFF >> 16;

/// Segment file names (IndexFileNames.segmentFileName :90-106):
/// `segment + "_" + suffix + "." + ext`, without the "_" when suffix is empty.
pub fn file_names(segment: &str, suffix: &str) -> [String; 2] {
    let base = if suffix.is_empty() {
        segment.to_string()
    } else {
        format!("{segment}_{suffix}")
    };
    [
        format!("{base}.{DATA_EXTENSION}"),
        format!("{base}.{META_EXTENSION}"),
    ]
}

/// Writer for the `.dvd` / `.dvm` file pair. Fields are appended in call
/// order (callers use ascending field numbers; the reader looks entries up
/// by field number, Lucene90DocValuesProducer.java:168-189).
pub struct DocValuesWriter {
    data: ChecksumIndexOutput,
    meta: ChecksumIndexOutput,
    dvd_name: String,
    dvm_name: String,
}

impl DocValuesWriter {
    /// Creates both files and immediately writes their index headers
    /// (Lucene90DocValuesConsumer.java:66-103): codec name, version 0,
    /// segment id, suffix.
    pub fn new(
        dir: &FSDirectory,
        segment: &str,
        segment_id: &[u8; 16],
        suffix: &str,
    ) -> io::Result<Self> {
        let [dvd_name, dvm_name] = file_names(segment, suffix);

        let mut data = dir.create_output(&dvd_name)?;
        write_index_header(&mut data, DATA_CODEC, VERSION, segment_id, suffix)?;

        let mut meta = dir.create_output(&dvm_name)?;
        write_index_header(&mut meta, META_CODEC, VERSION, segment_id, suffix)?;

        Ok(DocValuesWriter {
            data,
            meta,
            dvd_name,
            dvm_name,
        })
    }

    /// NUMERIC field: `values` are `(doc_id, value)` pairs sorted by doc,
    /// one value per doc; may be empty (no doc has a value).
    pub fn add_numeric_field(
        &mut self,
        field_number: i32,
        max_doc: u32,
        values: &[(u32, i64)],
    ) -> io::Result<()> {
        debug_assert!(values.windows(2).all(|w| w[0].0 < w[1].0));
        debug_assert!(values.iter().all(|&(d, _)| d < max_doc));
        debug_assert!(max_doc <= i32::MAX as u32);
        self.meta.write_int(field_number)?;
        self.meta.write_byte(TYPE_NUMERIC)?;
        self.write_values(max_doc, values, false)
    }

    /// SORTED field: `dict` holds the terms in unsigned byte order (each
    /// ≤ 32766 bytes), `ords` are `(doc_id, ord)` pairs sorted by doc.
    /// Every ord in `0..dict.len()` must be referenced (caller guarantee,
    /// SortedDocValuesWriter.java:113-125).
    pub fn add_sorted_field(
        &mut self,
        field_number: i32,
        max_doc: u32,
        dict: &[&[u8]],
        ords: &[(u32, u32)],
    ) -> io::Result<()> {
        debug_assert!(dict.windows(2).all(|w| w[0] < w[1]));
        debug_assert!(dict.iter().all(|t| t.len() <= 32766));
        debug_assert!(ords.windows(2).all(|w| w[0].0 < w[1].0));
        debug_assert!(
            ords.iter()
                .all(|&(d, o)| d < max_doc && (o as usize) < dict.len())
        );
        #[cfg(debug_assertions)]
        {
            let mut seen = vec![false; dict.len()];
            for &(_, o) in ords {
                seen[o as usize] = true;
            }
            debug_assert!(seen.iter().all(|&s| s), "every ord must be used");
        }
        self.meta.write_int(field_number)?;
        self.meta.write_byte(TYPE_SORTED)?;
        // Ords go through the numeric path with ords=true
        // (doAddSortedField :495-540): min must be 0, gcd stays 1.
        let ord_values: Vec<(u32, i64)> = ords.iter().map(|&(d, o)| (d, o as i64)).collect();
        self.write_values(max_doc, &ord_values, true)?;
        self.add_terms_dict(dict)
    }

    /// BINARY field: `values` are `(doc_id, bytes)` pairs sorted by doc.
    /// Mirrors Lucene90DocValuesConsumer.addBinaryField (:421-486).
    pub fn add_binary_field(
        &mut self,
        field_number: i32,
        max_doc: u32,
        values: &[(u32, Vec<u8>)],
    ) -> io::Result<()> {
        debug_assert!(values.windows(2).all(|w| w[0].0 < w[1].0));
        debug_assert!(values.iter().all(|&(d, _)| d < max_doc));

        self.meta.write_int(field_number)?;
        self.meta.write_byte(TYPE_BINARY)?;

        // 1. Value data: concatenated bytes (.dvd :435)
        let data_offset = self.data.file_pointer();
        self.meta.write_long(data_offset as i64)?; // dataOffset (:427)
        let mut min_length = i32::MAX;
        let mut max_length = 0i32;
        for (_, bytes) in values {
            self.data.write_bytes(bytes)?;
            min_length = min_length.min(bytes.len() as i32);
            max_length = max_length.max(bytes.len() as i32);
        }
        if values.is_empty() {
            min_length = 0;
        }
        let data_length = self.data.file_pointer() - data_offset;
        self.meta.write_long(data_length as i64)?; // dataLength (:440)

        // 2. docsWithField DISI (:442-461)
        let num_docs_with_field = values.len();
        if num_docs_with_field == 0 {
            self.meta.write_long(-2)?;
            self.meta.write_long(0)?;
            self.meta.write_short(-1)?;
            self.meta.write_byte(-1i8 as u8)?;
        } else if num_docs_with_field == max_doc as usize {
            self.meta.write_long(-1)?;
            self.meta.write_long(0)?;
            self.meta.write_short(-1)?;
            self.meta.write_byte(-1i8 as u8)?;
        } else {
            let offset = self.data.file_pointer();
            self.meta.write_long(offset as i64)?;
            let docs: Vec<u32> = values.iter().map(|&(d, _)| d).collect();
            let jump_table_entry_count = write_indexed_disi(&mut self.data, &docs)?;
            let length = self.data.file_pointer() - offset;
            self.meta.write_long(length as i64)?;
            self.meta.write_short(jump_table_entry_count)?;
            self.meta.write_byte(DISI_DENSE_RANK_POWER)?;
        }

        // 3. numDocsWithField + minLength + maxLength (:463-465)
        self.meta.write_int(num_docs_with_field as i32)?;
        self.meta.write_int(min_length)?;
        self.meta.write_int(max_length)?;

        // 4. Addresses (DirectMonotonic), only for variable-length (:466-485)
        if max_length > min_length {
            let addr_offset = self.data.file_pointer();
            self.meta.write_long(addr_offset as i64)?; // addressesOffset (:468)
            self.meta.write_vint(DIRECT_MONOTONIC_BLOCK_SHIFT as i32)?; // (:469)

            // Build addresses: [0, len0, len0+len1, ...]
            let mut addresses: Vec<u64> = Vec::with_capacity(num_docs_with_field + 1);
            let mut addr: u64 = 0;
            addresses.push(0);
            for (_, bytes) in values {
                addr += bytes.len() as u64;
                addresses.push(addr);
            }
            // DM meta into self.meta, packed data into temp buffer then append to .dvd
            let mut addr_data = ChecksumIndexOutput::new(IndexOutput::in_memory());
            direct_monotonic_write(
                &mut self.meta,
                &mut addr_data,
                &addresses,
                DIRECT_MONOTONIC_BLOCK_SHIFT,
            )?;
            let addr_bytes = addr_data.into_bytes();
            self.data.write_bytes(&addr_bytes)?;
            self.meta
                .write_long((self.data.file_pointer() - addr_offset) as i64)?; // addressesLength (:484)
        }

        Ok(())
    }

    /// close (:106-125): .dvm EOF marker int(-1), footers for both files.
    /// Returns the [.dvd, .dvm] file names.
    pub fn finish(mut self) -> io::Result<Vec<String>> {
        self.meta.write_int(-1)?;
        write_footer(&mut self.meta)?;
        write_footer(&mut self.data)?;
        self.meta.flush()?;
        self.data.flush()?;
        Ok(vec![self.dvd_name, self.dvm_name])
    }

    /// writeValues (:186-332) restricted to the single-block / constant
    /// branches (gcd == 1, tableSize == -1, no doBlocks, no min
    /// normalization; format-notes §11.1-2).
    fn write_values(&mut self, max_doc: u32, values: &[(u32, i64)], ords: bool) -> io::Result<()> {
        let num_docs_with_value = values.len() as u64;
        let mut min = i64::MAX;
        let mut max = i64::MIN;
        for &(_, v) in values {
            min = min.min(v);
            max = max.max(v);
        }
        if ords && !values.is_empty() {
            // writeValues self-check (:234-243); gcd is 1 by construction.
            debug_assert_eq!(min, 0, "ordinals always start at 0");
        }

        // docsWithField (:250-269)
        if num_docs_with_value == 0 {
            self.meta.write_long(-2)?; // docsWithFieldOffset
            self.meta.write_long(0)?; // docsWithFieldLength
            self.meta.write_short(-1)?; // jumpTableEntryCount
            self.meta.write_byte(-1i8 as u8)?; // denseRankPower
        } else if num_docs_with_value == max_doc as u64 {
            self.meta.write_long(-1)?;
            self.meta.write_long(0)?;
            self.meta.write_short(-1)?;
            self.meta.write_byte(-1i8 as u8)?;
        } else {
            let offset = self.data.file_pointer();
            self.meta.write_long(offset as i64)?;
            let docs: Vec<u32> = values.iter().map(|&(d, _)| d).collect();
            let jump_table_entry_count = write_indexed_disi(&mut self.data, &docs)?;
            let length = self.data.file_pointer() - offset;
            self.meta.write_long(length as i64)?;
            self.meta.write_short(jump_table_entry_count)?;
            self.meta.write_byte(DISI_DENSE_RANK_POWER)?;
        }

        self.meta.write_long(values.len() as i64)?; // numValues (:271)

        // min >= max (incl. empty) → constant encoding: bpv 0, tableSize -1,
        // no .dvd value data (:275-277). Otherwise a single DirectWriter
        // stream of (v - min) with bpv from the value range (:304-313).
        let num_bits_per_value = if min >= max {
            0
        } else {
            // Wrapping subtraction mirrors Java long overflow; the bit
            // pattern is what DirectWriter.unsignedBitsRequired sees.
            direct_writer_unsigned_bits_required(max.wrapping_sub(min) as u64)
        };
        self.meta.write_int(-1)?; // tableSize
        self.meta.write_byte(num_bits_per_value as u8)?;
        self.meta.write_long(min)?; // minValue (empty: Long.MAX_VALUE, :246)
        self.meta.write_long(1)?; // gcd (unused by the reader when bpv == 0)

        let values_offset = self.data.file_pointer();
        self.meta.write_long(values_offset as i64)?;
        if num_bits_per_value != 0 {
            let packed: Vec<u64> = values
                .iter()
                .map(|&(_, v)| v.wrapping_sub(min) as u64)
                .collect();
            let encoded = direct_writer_encode(&packed, num_bits_per_value);
            self.data.write_bytes(&encoded)?;
        }
        self.meta
            .write_long((self.data.file_pointer() - values_offset) as i64)?; // valuesLength
        self.meta.write_long(-1)?; // valueJumpTableOffset (single block)
        Ok(())
    }

    /// addTermsDict (:542-626): 64 terms per block; the first term of each
    /// block is stored verbatim (VInt length + bytes), the rest as
    /// prefix-compressed entries in a `VInt uncompressedLength + LZ4 stream`
    /// section (a block with a single term has no such section, :607).
    fn add_terms_dict(&mut self, dict: &[&[u8]]) -> io::Result<()> {
        let size = dict.len();
        self.meta.write_vlong(size as i64)?; // termsDictSize (:544)
        self.meta.write_int(DIRECT_MONOTONIC_BLOCK_SHIFT as i32)?; // :549

        let num_blocks = size.div_ceil(TERMS_DICT_BLOCK_SIZE);
        let mut block_addresses: Vec<u64> = Vec::with_capacity(num_blocks);

        let terms_data_offset = self.data.file_pointer();
        let mut max_term_length = 0i32;
        let mut max_block_length = 0i32;

        let mut prev: &[u8] = b"";
        // Buffer holding the block's first term (LZ4 dictionary in Lucene)
        // followed by the prefix-compressed entries (:565-600).
        let mut buf: Vec<u8> = Vec::new();
        let mut dict_len = 0usize;

        for (ord, &term) in dict.iter().enumerate() {
            if ord & (TERMS_DICT_BLOCK_SIZE - 1) == 0 {
                if ord != 0 {
                    // Flush the previous block; middle blocks are always full
                    // (64 terms), so the buffer always holds entries (:571-576).
                    debug_assert!(buf.len() > dict_len);
                    max_block_length = max_block_length.max(self.flush_dict_block(&buf, dict_len)?);
                    buf.clear();
                }
                // Block address: fp relative to termsDataOffset (:578).
                block_addresses.push(self.data.file_pointer() - terms_data_offset);
                self.data.write_vint(term.len() as i32)?;
                self.data.write_bytes(term)?;
                buf.extend_from_slice(term);
                dict_len = term.len();
            } else {
                // token = min(prefix,15) | min(15, suffix-1) << 4 (:592-599)
                let prefix_length = bytes_difference(prev, term);
                let suffix_length = term.len() - prefix_length;
                debug_assert!(suffix_length > 0, "terms are unique");
                buf.push((prefix_length.min(15) | ((suffix_length - 1).min(15) << 4)) as u8);
                if prefix_length >= 15 {
                    write_vint_to_vec(&mut buf, prefix_length - 15);
                }
                if suffix_length >= 16 {
                    write_vint_to_vec(&mut buf, suffix_length - 16);
                }
                buf.extend_from_slice(&term[prefix_length..]);
            }
            max_term_length = max_term_length.max(term.len() as i32);
            prev = term;
        }
        // Last block: only flushed when it holds more than the first term
        // (:607-611).
        if buf.len() > dict_len {
            max_block_length = max_block_length.max(self.flush_dict_block(&buf, dict_len)?);
        }

        // terms addresses: DirectMonotonic over block start offsets, meta
        // records into .dvm, packed data into a memory buffer (base fp 0,
        // like Lucene's ByteBuffersIndexOutput) appended to .dvd (:619-622).
        let mut addr_data = ChecksumIndexOutput::new(IndexOutput::in_memory());
        direct_monotonic_write(
            &mut self.meta,
            &mut addr_data,
            &block_addresses,
            DIRECT_MONOTONIC_BLOCK_SHIFT,
        )?;

        self.meta.write_int(max_term_length)?; // :614
        self.meta.write_int(max_block_length)?; // :615-616
        self.meta.write_long(terms_data_offset as i64)?; // :617
        self.meta
            .write_long((self.data.file_pointer() - terms_data_offset) as i64)?; // :618

        let terms_addresses_offset = self.data.file_pointer();
        let addr_bytes = addr_data.into_bytes();
        self.data.write_bytes(&addr_bytes)?;
        self.meta.write_long(terms_addresses_offset as i64)?; // :621
        self.meta
            .write_long((self.data.file_pointer() - terms_addresses_offset) as i64)?; // :622

        self.write_terms_index(dict)
    }

    /// compressAndGetTermsDictBlockLength (:628-635): writes
    /// `VInt uncompressedLength` + LZ4 stream of the buffer content after
    /// the dictionary term. Returns the uncompressed length.
    fn flush_dict_block(&mut self, buf: &[u8], dict_len: usize) -> io::Result<i32> {
        let uncompressed_length = buf.len() - dict_len;
        self.data.write_vint(uncompressed_length as i32)?;
        let compressed = lz4_block_compress(&buf[dict_len..])?;
        self.data.write_bytes(&compressed)?;
        Ok(uncompressed_length as i32)
    }

    /// writeTermsIndex (:646-693): samples every 1024 terms (ord 0, 1024,
    /// ...), storing a sort key per sample (empty for ord 0, otherwise the
    /// first `bytesDifference(prev, term) + 1` bytes of the term), then a
    /// DirectMonotonic address table of `1 + ceil(size/1024)` offsets.
    fn write_terms_index(&mut self, dict: &[&[u8]]) -> io::Result<()> {
        let size = dict.len();
        self.meta.write_int(TERMS_DICT_REVERSE_INDEX_SHIFT as i32)?; // :648

        let terms_index_offset = self.data.file_pointer();
        let num_addresses = 1 + size.div_ceil(TERMS_DICT_REVERSE_INDEX_SIZE);
        let mut index_addresses: Vec<u64> = Vec::with_capacity(num_addresses);
        let mut offset = 0u64;
        let mut prev: &[u8] = b"";
        for (ord, &term) in dict.iter().enumerate() {
            if ord & (TERMS_DICT_REVERSE_INDEX_SIZE - 1) == 0 {
                index_addresses.push(offset);
                // StringHelper.sortKeyLength (:62-64); empty for ord 0 (:669-672).
                let sort_key_length = if ord == 0 {
                    0
                } else {
                    bytes_difference(prev, term) + 1
                };
                debug_assert!(sort_key_length <= term.len());
                offset += sort_key_length as u64;
                self.data.write_bytes(&term[..sort_key_length])?;
            } else if ord & (TERMS_DICT_REVERSE_INDEX_SIZE - 1) == TERMS_DICT_REVERSE_INDEX_SIZE - 1
            {
                prev = term;
            }
        }
        index_addresses.push(offset); // total index length (:684)
        let terms_index_length = self.data.file_pointer() - terms_index_offset;

        // Index addresses DM: blockShift 16 here as well (:659-661), meta
        // into .dvm, data appended to .dvd after the sort keys (:688-691).
        let mut addr_data = ChecksumIndexOutput::new(IndexOutput::in_memory());
        direct_monotonic_write(
            &mut self.meta,
            &mut addr_data,
            &index_addresses,
            DIRECT_MONOTONIC_BLOCK_SHIFT,
        )?;

        self.meta.write_long(terms_index_offset as i64)?; // :686
        self.meta.write_long(terms_index_length as i64)?; // :687

        let terms_index_addresses_offset = self.data.file_pointer();
        let addr_bytes = addr_data.into_bytes();
        self.data.write_bytes(&addr_bytes)?;
        self.meta.write_long(terms_index_addresses_offset as i64)?; // :690
        self.meta
            .write_long((self.data.file_pointer() - terms_index_addresses_offset) as i64)?; // :691
        Ok(())
    }
}

/// StringHelper.bytesDifference (:52-60): length of the common prefix.
fn bytes_difference(a: &[u8], b: &[u8]) -> usize {
    let lim = a.len().min(b.len());
    for k in 0..lim {
        if a[k] != b[k] {
            return k;
        }
    }
    lim
}

/// VInt into a byte Vec (ByteArrayDataOutput.writeVInt equivalent).
fn write_vint_to_vec(out: &mut Vec<u8>, v: usize) {
    let mut v = v;
    while v & !0x7f != 0 {
        out.push(((v & 0x7f) as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Raw LZ4 block compress (no size header), FAST(2): same helper as
/// stored_fields.rs — any conforming LZ4 stream is readable by Lucene.
fn lz4_block_compress(bytes: &[u8]) -> io::Result<Vec<u8>> {
    lz4::block::compress(bytes, Some(lz4::block::CompressionMode::FAST(2)), false)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("lz4 compress: {e}")))
}

/// IndexedDISI.writeBitSet (:189-254) over ascending, non-empty doc ids.
/// Returns the jump-table entry count (0 when the table is omitted because
/// there is a single real block, flushBlockJumps :271-285).
fn write_indexed_disi(data: &mut ChecksumIndexOutput, docs: &[u32]) -> io::Result<i16> {
    debug_assert!(!docs.is_empty());
    let origo = data.file_pointer();
    // Per logical block: (index, offset) — index = docs with value before
    // the block, offset = block header position relative to `origo`
    // (addJumps :257-266).
    let mut jumps: Vec<(i32, i32)> = Vec::new();
    let mut total_cardinality = 0i32;
    let mut buffer = [0u64; DISI_BLOCK_LONGS];
    let mut block_cardinality = 0u32;
    let mut prev_block: i64 = -1;
    let mut jump_block_index = 0u32;

    for &doc in docs {
        let block = doc >> 16;
        if prev_block != -1 && block as i64 != prev_block {
            add_jumps(
                &mut jumps,
                (data.file_pointer() - origo) as i32,
                total_cardinality,
                jump_block_index,
                prev_block as u32 + 1,
            );
            jump_block_index = prev_block as u32 + 1;
            flush_disi_block(data, prev_block as u32, &buffer, block_cardinality)?;
            buffer = [0; DISI_BLOCK_LONGS];
            total_cardinality += block_cardinality as i32;
            block_cardinality = 0;
        }
        // FixedBitSet.set: bit i lives in long i>>6 at 1<<(i&63) (:123-125)
        buffer[((doc & 0xFFFF) >> 6) as usize] |= 1u64 << (doc & 63);
        block_cardinality += 1;
        prev_block = block as i64;
    }
    if block_cardinality > 0 {
        add_jumps(
            &mut jumps,
            (data.file_pointer() - origo) as i32,
            total_cardinality,
            jump_block_index,
            prev_block as u32 + 1,
        );
        total_cardinality += block_cardinality as i32;
        flush_disi_block(data, prev_block as u32, &buffer, block_cardinality)?;
        prev_block += 1;
    }
    // There is always at least one real block (docs is non-empty).
    let last_block = if prev_block == -1 {
        0
    } else {
        prev_block as u32
    };
    // Sentinel block: SPARSE, blockID 32767, cardinality 1, doc 65535
    // (= NO_MORE_DOCS & 0xFFFF), :243-251.
    add_jumps(
        &mut jumps,
        (data.file_pointer() - origo) as i32,
        total_cardinality,
        last_block,
        last_block + 1,
    );
    data.write_short(DISI_SENTINEL_BLOCK as i16)?;
    data.write_short(0)?; // cardinality - 1
    data.write_short(-1)?; // (short) 65535

    // flushBlockJumps (:271-285): lastBlock+1 entries, each LE int index +
    // LE int offset; skipped entirely when there is only one real block.
    let block_count = last_block + 1;
    if block_count == 2 {
        return Ok(0);
    }
    for b in 0..block_count as usize {
        let (index, offset) = jumps[b];
        data.write_int(index)?;
        data.write_int(offset)?;
    }
    Ok(block_count as i16)
}

/// addJumps (:257-266): entries [start_block, end_block) all point at the
/// same upcoming block header (`offset`) with the running `index`; empty
/// blocks therefore point to the next non-empty block.
fn add_jumps(
    jumps: &mut Vec<(i32, i32)>,
    offset: i32,
    index: i32,
    start_block: u32,
    end_block: u32,
) {
    for _ in start_block..end_block {
        jumps.push((index, offset));
    }
}

/// IndexedDISI block flush (:110-133). Block type is fully determined by
/// cardinality: ≤ 4095 SPARSE, 4096..=65535 DENSE, 65536 ALL.
fn flush_disi_block(
    data: &mut ChecksumIndexOutput,
    block: u32,
    buffer: &[u64; DISI_BLOCK_LONGS],
    cardinality: u32,
) -> io::Result<()> {
    debug_assert!(block < DISI_BLOCK_SIZE && cardinality > 0 && cardinality <= DISI_BLOCK_SIZE);
    data.write_short(block as i16)?;
    data.write_short((cardinality - 1) as u16 as i16)?;
    if cardinality > DISI_MAX_ARRAY_LENGTH {
        if cardinality != DISI_BLOCK_SIZE {
            // DENSE: rank table then bitset. createRank (:138-153) with
            // denseRankPower 9: one big-endian u16 entry per 8 longs (512
            // docs), holding the set-bit count before that sub-block.
            let mut rank = [0u8; DISI_BLOCK_LONGS >> 2];
            let mut bit_count = 0u32;
            for (word, &bits) in buffer.iter().enumerate() {
                if word & 7 == 0 {
                    rank[word >> 2] = (bit_count >> 8) as u8;
                    rank[(word >> 2) + 1] = (bit_count & 0xFF) as u8;
                }
                bit_count += bits.count_ones();
            }
            data.write_bytes(&rank)?;
            for &word in buffer.iter() {
                data.write_long(word as i64)?;
            }
        }
        // ALL: header only (:117-118).
    } else {
        // SPARSE: ascending low-16-bit doc ids as LE shorts (:127-132).
        for (word_index, &word) in buffer.iter().enumerate() {
            let mut w = word;
            while w != 0 {
                let bit = w.trailing_zeros();
                data.write_short(((word_index << 6) | bit as usize) as u16 as i16)?;
                w &= w - 1;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec_util::{CODEC_MAGIC, FOOTER_MAGIC, crc32, index_header_length};
    use std::fs;
    use std::path::PathBuf;

    const SEGMENT_ID: [u8; 16] = [0x5A; 16];
    const SUFFIX: &str = "Lucene90_0";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("codec-lucene9-dv-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Writes a segment with the given fields and returns (.dvd, .dvm) bytes.
    fn run_writer(tag: &str, f: impl FnOnce(&mut DocValuesWriter)) -> (Vec<u8>, Vec<u8>) {
        let root = temp_dir(tag);
        let dir = FSDirectory::open(&root).unwrap();
        let mut w = DocValuesWriter::new(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        f(&mut w);
        let names = w.finish().unwrap();
        assert_eq!(names, file_names("_0", SUFFIX).to_vec());
        let dvd = fs::read(root.join(&names[0])).unwrap();
        let dvm = fs::read(root.join(&names[1])).unwrap();
        fs::remove_dir_all(&root).unwrap();
        (dvd, dvm)
    }

    struct ByteReader<'a> {
        bytes: &'a [u8],
        pos: usize,
    }

    impl<'a> ByteReader<'a> {
        fn new(bytes: &'a [u8]) -> Self {
            ByteReader { bytes, pos: 0 }
        }
        fn read_u8(&mut self) -> u8 {
            let b = self.bytes[self.pos];
            self.pos += 1;
            b
        }
        fn read_bytes(&mut self, n: usize) -> &'a [u8] {
            let b = &self.bytes[self.pos..self.pos + n];
            self.pos += n;
            b
        }
        fn read_le_u16(&mut self) -> u16 {
            u16::from_le_bytes(self.read_bytes(2).try_into().unwrap())
        }
        fn read_le_i16(&mut self) -> i16 {
            self.read_le_u16() as i16
        }
        fn read_le_i32(&mut self) -> i32 {
            i32::from_le_bytes(self.read_bytes(4).try_into().unwrap())
        }
        fn read_le_i64(&mut self) -> i64 {
            i64::from_le_bytes(self.read_bytes(8).try_into().unwrap())
        }
        fn read_be_u32(&mut self) -> u32 {
            u32::from_be_bytes(self.read_bytes(4).try_into().unwrap())
        }
        fn read_vint(&mut self) -> i32 {
            let mut v = 0u32;
            let mut shift = 0;
            loop {
                let b = self.read_u8();
                v |= ((b & 0x7f) as u32) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            v as i32
        }
        fn read_vlong(&mut self) -> i64 {
            let mut v = 0u64;
            let mut shift = 0;
            loop {
                let b = self.read_u8();
                v |= ((b & 0x7f) as u64) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            v as i64
        }
    }

    fn parse_index_header(r: &mut ByteReader, codec: &str) {
        assert_eq!(r.read_be_u32(), CODEC_MAGIC);
        let len = r.read_vint() as usize;
        assert_eq!(r.read_bytes(len), codec.as_bytes());
        assert_eq!(r.read_be_u32(), 0, "version must be 0");
        assert_eq!(r.read_bytes(16), &SEGMENT_ID[..]);
        let suffix_len = r.read_u8() as usize;
        assert_eq!(r.read_bytes(suffix_len), SUFFIX.as_bytes());
    }

    fn check_footer(bytes: &[u8]) {
        let n = bytes.len();
        assert!(n >= 16);
        assert_eq!(
            u32::from_be_bytes(bytes[n - 16..n - 12].try_into().unwrap()),
            FOOTER_MAGIC
        );
        assert_eq!(&bytes[n - 12..n - 8], &[0, 0, 0, 0]);
        let crc = u64::from_be_bytes(bytes[n - 8..].try_into().unwrap());
        assert_eq!(crc, crc32(&bytes[..n - 8]));
    }

    #[derive(Debug)]
    struct NumericMeta {
        docs_offset: i64,
        docs_length: i64,
        jump_count: i16,
        dense_rank_power: i8,
        num_values: i64,
        table_size: i32,
        bpv: u8,
        min: i64,
        gcd: i64,
        values_offset: i64,
        values_length: i64,
        value_jump_table_offset: i64,
    }

    /// Lucene90DocValuesProducer.readNumeric (:197-224).
    fn read_numeric_meta(r: &mut ByteReader) -> NumericMeta {
        let docs_offset = r.read_le_i64();
        let docs_length = r.read_le_i64();
        let jump_count = r.read_le_i16();
        let dense_rank_power = r.read_u8() as i8;
        let num_values = r.read_le_i64();
        let table_size = r.read_le_i32();
        assert!(table_size <= 256);
        for _ in 0..table_size.max(0) {
            r.read_le_i64();
        }
        let bpv = r.read_u8();
        let min = r.read_le_i64();
        let gcd = r.read_le_i64();
        let values_offset = r.read_le_i64();
        let values_length = r.read_le_i64();
        let value_jump_table_offset = r.read_le_i64();
        NumericMeta {
            docs_offset,
            docs_length,
            jump_count,
            dense_rank_power,
            num_values,
            table_size,
            bpv,
            min,
            gcd,
            values_offset,
            values_length,
            value_jump_table_offset,
        }
    }

    /// DirectReader-style value access over the continuous LSB-first bit
    /// stream (DirectReader.getInstance + padding makes this exact).
    fn direct_reader_get(slice: &[u8], bpv: u32, index: usize) -> u64 {
        let bit_offset = index * bpv as usize;
        let byte_offset = bit_offset / 8;
        let shift = bit_offset % 8;
        let mut buf = [0u8; 8];
        let take = (slice.len() - byte_offset).min(8);
        buf[..take].copy_from_slice(&slice[byte_offset..byte_offset + take]);
        let raw = u64::from_le_bytes(buf) >> shift;
        if bpv == 64 {
            raw
        } else {
            raw & ((1u64 << bpv) - 1)
        }
    }

    /// Reader-side value decode: `gcd * packed + min` (producer :527-534),
    /// constant minValue when bpv == 0 (:487-493).
    fn decode_values(dvd: &[u8], m: &NumericMeta) -> Vec<i64> {
        if m.bpv == 0 {
            return vec![m.min; m.num_values as usize];
        }
        let slice = &dvd[m.values_offset as usize..(m.values_offset + m.values_length) as usize];
        (0..m.num_values as usize)
            .map(|i| {
                (direct_reader_get(slice, m.bpv as u32, i) as i64)
                    .wrapping_mul(m.gcd)
                    .wrapping_add(m.min)
            })
            .collect()
    }

    fn decode_ords(dvd: &[u8], m: &NumericMeta) -> Vec<u32> {
        decode_values(dvd, m).iter().map(|&v| v as u32).collect()
    }

    /// DirectMonotonicReader.loadMeta (:56-67) + get (:160-165).
    struct DmMeta {
        mins: Vec<i64>,
        avgs: Vec<f32>,
        offsets: Vec<u64>,
        bpvs: Vec<u8>,
        block_shift: u32,
        num_values: usize,
    }

    fn read_dm(r: &mut ByteReader, num_values: usize, block_shift: u32) -> DmMeta {
        let num_blocks = if num_values == 0 {
            0
        } else {
            ((num_values - 1) >> block_shift) + 1
        };
        let mut dm = DmMeta {
            mins: Vec::new(),
            avgs: Vec::new(),
            offsets: Vec::new(),
            bpvs: Vec::new(),
            block_shift,
            num_values,
        };
        for _ in 0..num_blocks {
            dm.mins.push(r.read_le_i64());
            dm.avgs.push(f32::from_bits(r.read_le_i32() as u32));
            dm.offsets.push(r.read_le_i64() as u64);
            dm.bpvs.push(r.read_u8());
        }
        dm
    }

    impl DmMeta {
        fn get(&self, data: &[u8], index: usize) -> u64 {
            let block = index >> self.block_shift;
            let block_index = index & ((1 << self.block_shift) - 1);
            let bpv = self.bpvs[block] as usize;
            let delta = if bpv == 0 {
                0
            } else {
                let bit_offset = self.offsets[block] as usize * 8 + block_index * bpv;
                let byte_offset = bit_offset / 8;
                let shift = bit_offset % 8;
                let mut buf = [0u8; 8];
                let take = (data.len() - byte_offset).min(8);
                buf[..take].copy_from_slice(&data[byte_offset..byte_offset + take]);
                let raw = u64::from_le_bytes(buf) >> shift;
                if bpv == 64 {
                    raw
                } else {
                    raw & ((1u64 << bpv) - 1)
                }
            };
            self.mins[block]
                .wrapping_add((self.avgs[block] * block_index as f32) as i64)
                .wrapping_add(delta as i64) as u64
        }
    }

    struct DisiBlock {
        block_id: u32,
        cardinality: u32,
        header_offset: u32,
        docs: Vec<u32>,
    }

    /// Decodes the IndexedDISI region, verifying block headers, the DENSE
    /// rank table, the sentinel block and the jump table; returns the real
    /// blocks (sentinel excluded).
    fn decode_disi(dvd: &[u8], offset: usize, length: usize, jump_count: i16) -> Vec<DisiBlock> {
        let region = &dvd[offset..offset + length];
        let mut r = ByteReader::new(region);
        let mut blocks: Vec<DisiBlock> = Vec::new();
        let sentinel_offset = loop {
            let header_offset = r.pos as u32;
            let block_id = r.read_le_u16() as u32;
            let cardinality = r.read_le_u16() as u32 + 1;
            if cardinality <= DISI_MAX_ARRAY_LENGTH {
                let mut docs = Vec::with_capacity(cardinality as usize);
                for _ in 0..cardinality {
                    docs.push((block_id << 16) | r.read_le_u16() as u32);
                }
                assert!(docs.windows(2).all(|w| w[0] < w[1]));
                if block_id == DISI_SENTINEL_BLOCK {
                    assert_eq!(cardinality, 1);
                    // full doc id = NO_MORE_DOCS (32767 << 16 | 65535)
                    assert_eq!(docs, vec![0x7FFF_FFFF]);
                    break header_offset;
                }
                blocks.push(DisiBlock {
                    block_id,
                    cardinality,
                    header_offset,
                    docs,
                });
            } else if cardinality == DISI_BLOCK_SIZE {
                assert_ne!(block_id, DISI_SENTINEL_BLOCK);
                let docs = (0..DISI_BLOCK_SIZE).map(|i| (block_id << 16) | i).collect();
                blocks.push(DisiBlock {
                    block_id,
                    cardinality,
                    header_offset,
                    docs,
                });
            } else {
                assert_ne!(block_id, DISI_SENTINEL_BLOCK);
                let rank = r.read_bytes(256);
                let mut bits = [0u64; DISI_BLOCK_LONGS];
                for w in bits.iter_mut() {
                    *w = r.read_le_i64() as u64;
                }
                // rank entry k = BE u16 = set bits before long 8k (:138-153)
                let mut count = 0u32;
                for (word, &b) in bits.iter().enumerate() {
                    if word & 7 == 0 {
                        let entry = ((rank[word >> 2] as u32) << 8) | rank[(word >> 2) + 1] as u32;
                        assert_eq!(entry, count, "rank entry {}", word >> 3);
                    }
                    count += b.count_ones();
                }
                assert_eq!(count, cardinality);
                let mut docs = Vec::with_capacity(cardinality as usize);
                for (word_index, &word) in bits.iter().enumerate() {
                    let mut w = word;
                    while w != 0 {
                        let bit = w.trailing_zeros();
                        docs.push((block_id << 16) | ((word_index as u32) << 6) | bit);
                        w &= w - 1;
                    }
                }
                blocks.push(DisiBlock {
                    block_id,
                    cardinality,
                    header_offset,
                    docs,
                });
            }
        };

        // Jump table: lastRealBlock + 2 entries, omitted (count 0) when the
        // only real block is block 0 (flushBlockJumps :271-285).
        let last_real = blocks.last().unwrap().block_id as usize;
        let expected_entries = if last_real == 0 { 0 } else { last_real + 2 };
        assert_eq!(jump_count as usize, expected_entries);
        let mut jumps = Vec::new();
        for _ in 0..expected_entries {
            jumps.push((r.read_le_i32(), r.read_le_i32()));
        }
        assert_eq!(r.pos, region.len(), "DISI region fully consumed");

        if !jumps.is_empty() {
            // Entry b: index = real docs before block b, offset = header of
            // the first real block >= b (addJumps :257-266).
            let mut real_idx = 0;
            let mut cum = 0i32;
            for b in 0..=last_real {
                while real_idx < blocks.len() && (blocks[real_idx].block_id as usize) < b {
                    cum += blocks[real_idx].cardinality as i32;
                    real_idx += 1;
                }
                let (index, offset) = jumps[b];
                assert_eq!(index, cum, "jump index for block {b}");
                assert_eq!(
                    offset, blocks[real_idx].header_offset as i32,
                    "jump offset for block {b}"
                );
            }
            // Sentinel entry: (total cardinality, sentinel header offset).
            let total: i32 = blocks.iter().map(|b| b.cardinality as i32).sum();
            assert_eq!(jumps[last_real + 1], (total, sentinel_offset as i32));
        }
        blocks
    }

    struct TermsMeta {
        dict_size: i64,
        addresses: DmMeta,
        max_term_length: i32,
        max_block_length: i32,
        terms_data_offset: i64,
        terms_data_length: i64,
        terms_addresses_offset: i64,
        terms_addresses_length: i64,
        index_addresses: DmMeta,
        terms_index_offset: i64,
        terms_index_length: i64,
        terms_index_addresses_offset: i64,
        terms_index_addresses_length: i64,
    }

    /// Lucene90DocValuesProducer.readTermDict (:278-299).
    fn read_terms_meta(r: &mut ByteReader) -> TermsMeta {
        let dict_size = r.read_vlong();
        let block_shift = r.read_le_i32();
        assert_eq!(block_shift, DIRECT_MONOTONIC_BLOCK_SHIFT as i32);
        let addresses = read_dm(r, (dict_size as usize).div_ceil(64), block_shift as u32);
        let max_term_length = r.read_le_i32();
        let max_block_length = r.read_le_i32();
        let terms_data_offset = r.read_le_i64();
        let terms_data_length = r.read_le_i64();
        let terms_addresses_offset = r.read_le_i64();
        let terms_addresses_length = r.read_le_i64();
        let index_shift = r.read_le_i32();
        assert_eq!(index_shift, TERMS_DICT_REVERSE_INDEX_SHIFT as i32);
        // The index addresses DM reuses the terms blockShift (16), :294.
        let index_addresses = read_dm(
            r,
            1 + (dict_size as usize).div_ceil(1024),
            block_shift as u32,
        );
        let terms_index_offset = r.read_le_i64();
        let terms_index_length = r.read_le_i64();
        let terms_index_addresses_offset = r.read_le_i64();
        let terms_index_addresses_length = r.read_le_i64();
        TermsMeta {
            dict_size,
            addresses,
            max_term_length,
            max_block_length,
            terms_data_offset,
            terms_data_length,
            terms_addresses_offset,
            terms_addresses_length,
            index_addresses,
            terms_index_offset,
            terms_index_length,
            terms_index_addresses_offset,
            terms_index_addresses_length,
        }
    }

    /// Decodes every term via the block structure (first term verbatim +
    /// prefix-compressed entries from the LZ4 section), verifying block
    /// addresses and maxBlockLength.
    fn decode_terms(dvd: &[u8], tm: &TermsMeta) -> Vec<Vec<u8>> {
        let data = &dvd
            [tm.terms_data_offset as usize..(tm.terms_data_offset + tm.terms_data_length) as usize];
        let addr = &dvd[tm.terms_addresses_offset as usize
            ..(tm.terms_addresses_offset + tm.terms_addresses_length) as usize];
        let size = tm.dict_size as usize;
        let num_blocks = size.div_ceil(TERMS_DICT_BLOCK_SIZE);
        let mut terms = Vec::with_capacity(size);
        let mut actual_max_block_length = 0;
        if num_blocks > 0 {
            assert_eq!(tm.addresses.get(addr, 0), 0, "first block at offset 0");
        }
        for b in 0..num_blocks {
            let start = tm.addresses.get(addr, b) as usize;
            let end = if b + 1 < num_blocks {
                tm.addresses.get(addr, b + 1) as usize
            } else {
                data.len()
            };
            assert!(start <= end && end <= data.len());
            let region = &data[start..end];
            let mut r = ByteReader::new(region);
            let first_len = r.read_vint() as usize;
            let first = r.read_bytes(first_len).to_vec();
            terms.push(first.clone());
            let block_count = (size - b * TERMS_DICT_BLOCK_SIZE).min(TERMS_DICT_BLOCK_SIZE);
            if block_count > 1 {
                let uncompressed = r.read_vint() as usize;
                actual_max_block_length = actual_max_block_length.max(uncompressed);
                let decompressed =
                    lz4::block::decompress(&region[r.pos..], Some(uncompressed as i32)).unwrap();
                assert_eq!(decompressed.len(), uncompressed);
                let mut dr = ByteReader::new(&decompressed);
                let mut prev = first;
                for _ in 1..block_count {
                    let token = dr.read_u8() as usize;
                    let mut prefix = token & 0x0F;
                    let mut suffix = 1 + (token >> 4);
                    if prefix == 15 {
                        prefix += dr.read_vint() as usize;
                    }
                    if suffix == 16 {
                        suffix += dr.read_vint() as usize;
                    }
                    let sfx = dr.read_bytes(suffix);
                    let mut term = prev[..prefix].to_vec();
                    term.extend_from_slice(sfx);
                    prev = term.clone();
                    terms.push(term);
                }
                assert_eq!(dr.pos, decompressed.len());
            } else {
                assert_eq!(r.pos, region.len(), "single-term block has no LZ4 section");
            }
        }
        assert_eq!(tm.max_block_length as usize, actual_max_block_length);
        terms
    }

    /// Verifies the reverse index: sampled sort keys and DM addresses
    /// (writeTermsIndex :646-693).
    fn check_reverse_index(dvd: &[u8], tm: &TermsMeta, dict: &[&[u8]]) {
        let index_data = &dvd[tm.terms_index_offset as usize
            ..(tm.terms_index_offset + tm.terms_index_length) as usize];
        let addr = &dvd[tm.terms_index_addresses_offset as usize
            ..(tm.terms_index_addresses_offset + tm.terms_index_addresses_length) as usize];
        let size = dict.len();
        let num_samples = size.div_ceil(TERMS_DICT_REVERSE_INDEX_SIZE);
        assert_eq!(tm.index_addresses.num_values, num_samples + 1);
        for i in 0..num_samples {
            let ord = i * TERMS_DICT_REVERSE_INDEX_SIZE;
            let start = tm.index_addresses.get(addr, i) as usize;
            let end = tm.index_addresses.get(addr, i + 1) as usize;
            let key = &index_data[start..end];
            let expected_len = if ord == 0 {
                0
            } else {
                bytes_difference(dict[ord - 1], dict[ord]) + 1
            };
            assert_eq!(key, &dict[ord][..expected_len], "sort key for ord {ord}");
        }
        let total = tm.index_addresses.get(addr, num_samples) as usize;
        assert_eq!(total, index_data.len(), "last index address = index length");
    }

    #[test]
    fn file_names_with_and_without_suffix() {
        assert_eq!(
            file_names("_0", "Lucene90_0"),
            [
                "_0_Lucene90_0.dvd".to_string(),
                "_0_Lucene90_0.dvm".to_string()
            ]
        );
        assert_eq!(
            file_names("_0", ""),
            ["_0.dvd".to_string(), "_0.dvm".to_string()]
        );
    }

    #[test]
    fn numeric_dense_constant_and_direct() {
        let max_doc = 1000u32;
        // bpv == 0 constant branch
        let constant: Vec<(u32, i64)> = (0..max_doc).map(|d| (d, 42)).collect();
        // ordinary single-block branch (bpv 16)
        let ramp: Vec<(u32, i64)> = (0..max_doc).map(|d| (d, d as i64 * 7 - 3000)).collect();
        // i64 extremes: (max - min) wraps, bpv 64
        let mut extreme_vals = vec![i64::MIN, i64::MAX, 0, -1, 1];
        extreme_vals.extend((5..max_doc).map(|d| (d as i64) << 40));
        let extreme: Vec<(u32, i64)> = extreme_vals
            .iter()
            .enumerate()
            .map(|(d, &v)| (d as u32, v))
            .collect();

        let (dvd, dvm) = run_writer("num-dense", |w| {
            w.add_numeric_field(0, max_doc, &constant).unwrap();
            w.add_numeric_field(1, max_doc, &ramp).unwrap();
            w.add_numeric_field(2, max_doc, &extreme).unwrap();
        });

        check_footer(&dvd);
        check_footer(&dvm);
        let header_len = index_header_length(DATA_CODEC, SUFFIX) as i64;

        let mut r = ByteReader::new(&dvm);
        parse_index_header(&mut r, "Lucene90DocValuesMetadata");
        let inputs = [&constant, &ramp, &extreme];
        let mut metas = Vec::new();
        for (field, input) in inputs.iter().enumerate() {
            assert_eq!(r.read_le_i32(), field as i32);
            assert_eq!(r.read_u8(), TYPE_NUMERIC);
            let m = read_numeric_meta(&mut r);
            // all docs have values: dense, no DISI
            assert_eq!(
                (
                    m.docs_offset,
                    m.docs_length,
                    m.jump_count,
                    m.dense_rank_power
                ),
                (-1, 0, -1, -1)
            );
            assert_eq!(m.num_values, max_doc as i64);
            assert_eq!(m.table_size, -1);
            assert_eq!(m.value_jump_table_offset, -1);
            let decoded = decode_values(&dvd, &m);
            let want: Vec<i64> = input.iter().map(|&(_, v)| v).collect();
            assert_eq!(decoded, want, "field {field} values");
            metas.push(m);
        }
        assert_eq!(r.read_le_i32(), -1, "dvm EOF marker");
        assert_eq!(r.pos, dvm.len() - 16, "dvm fully consumed");

        // .dvd layout: constant field wrote no data, then ramp, then extreme
        assert_eq!(metas[0].bpv, 0);
        assert_eq!(metas[0].min, 42);
        assert_eq!(metas[0].gcd, 1);
        assert_eq!(metas[0].values_offset, header_len);
        assert_eq!(metas[0].values_length, 0);
        assert_eq!(metas[1].bpv, 16);
        assert_eq!(metas[1].values_offset, header_len);
        assert_eq!(metas[1].values_length, max_doc as i64 * 2);
        assert_eq!(metas[2].bpv, 64);
        assert_eq!(metas[2].values_offset, header_len + max_doc as i64 * 2);
        assert_eq!(metas[2].values_length, max_doc as i64 * 8);
        assert_eq!(
            metas[2].values_offset + metas[2].values_length,
            dvd.len() as i64 - 16
        );
    }

    #[test]
    fn numeric_sparse_disi_block_types_and_jumps() {
        let max_doc = 200_000u32;
        // field 0: SPARSE (3 docs, block 0) + DENSE (5000 docs, block 1) +
        // ALL (block 2) + SPARSE (1 doc, block 3) → jump table with 5 entries
        let mut values: Vec<(u32, i64)> = Vec::new();
        for d in [5u32, 100, 4095] {
            values.push((d, d as i64 * 11 - 20_000));
        }
        for i in 0..5000u32 {
            let d = 65536 + i;
            values.push((d, d as i64 * 11 - 20_000));
        }
        for i in 0..65536u32 {
            let d = 131072 + i;
            values.push((d, d as i64 * 11 - 20_000));
        }
        values.push((196608 + 42, (196608 + 42) * 11 - 20_000));

        // field 1: single real block (block 0) → jumpTableEntryCount == 0;
        // constant value → bpv == 0 with a DISI
        let single: Vec<(u32, i64)> = (0..100u32).map(|d| (d * 2, 7)).collect();

        // field 2: single real block != block 0 → jump table exists and
        // covers the empty leading block
        let block1_only: Vec<(u32, i64)> =
            (0..10u32).map(|d| (70000 + d, 1000 + d as i64)).collect();

        let (dvd, dvm) = run_writer("num-sparse", |w| {
            w.add_numeric_field(0, max_doc, &values).unwrap();
            w.add_numeric_field(1, max_doc, &single).unwrap();
            w.add_numeric_field(2, max_doc, &block1_only).unwrap();
        });

        check_footer(&dvd);
        check_footer(&dvm);
        let mut r = ByteReader::new(&dvm);
        parse_index_header(&mut r, "Lucene90DocValuesMetadata");

        let inputs = [&values, &single, &block1_only];
        let mut prev_end = index_header_length(DATA_CODEC, SUFFIX) as i64;
        for (field, input) in inputs.iter().enumerate() {
            assert_eq!(r.read_le_i32(), field as i32);
            assert_eq!(r.read_u8(), TYPE_NUMERIC);
            let m = read_numeric_meta(&mut r);
            assert_eq!(m.dense_rank_power, 9);
            assert_eq!(m.docs_offset, prev_end, "DISI follows previous data");
            assert_eq!(m.num_values, input.len() as i64);
            assert_eq!(m.table_size, -1);

            let blocks = decode_disi(
                &dvd,
                m.docs_offset as usize,
                m.docs_length as usize,
                m.jump_count,
            );
            let docs: Vec<u32> = blocks.iter().flat_map(|b| b.docs.iter().copied()).collect();
            let want_docs: Vec<u32> = input.iter().map(|&(d, _)| d).collect();
            assert_eq!(docs, want_docs, "field {field} DISI docs");

            assert_eq!(m.values_offset, m.docs_offset + m.docs_length);
            let decoded = decode_values(&dvd, &m);
            let want: Vec<i64> = input.iter().map(|&(_, v)| v).collect();
            assert_eq!(decoded, want, "field {field} values");
            prev_end = m.values_offset + m.values_length;

            if field == 0 {
                assert_eq!(m.jump_count, 5, "lastRealBlock 3 → 5 entries");
                assert_eq!(blocks.len(), 4);
                assert_eq!(blocks[0].cardinality, 3); // SPARSE
                assert_eq!(blocks[1].cardinality, 5000); // DENSE
                assert_eq!(blocks[2].cardinality, 65536); // ALL
                assert_eq!(blocks[3].cardinality, 1); // SPARSE
            } else if field == 1 {
                assert_eq!(m.jump_count, 0, "single real block 0 → no jump table");
                assert_eq!(blocks.len(), 1);
                assert_eq!(m.bpv, 0);
                assert_eq!(m.min, 7);
                assert_eq!(m.values_length, 0);
            } else {
                assert_eq!(m.jump_count, 3, "lastRealBlock 1 → 3 entries");
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks[0].block_id, 1);
            }
        }
        assert_eq!(r.read_le_i32(), -1);
        assert_eq!(r.pos, dvm.len() - 16);
        assert_eq!(prev_end, dvd.len() as i64 - 16);
    }

    #[test]
    fn sorted_terms_dict_multi_block_and_reverse_index() {
        // 150 terms → 3 dict blocks; reverse index has 1 sample (ord 0)
        let dict150: Vec<String> = (0..150).map(|i| format!("term-{i:04}")).collect();
        let dict150_refs: Vec<&[u8]> = dict150.iter().map(|s| s.as_bytes()).collect();
        let ords150: Vec<(u32, u32)> = (0..300u32).map(|d| (d, d % 150)).collect();

        // 1030 terms → 17 dict blocks; reverse index samples ord 0 and 1024.
        // First group shares a > 15-byte common prefix (prefix VInt
        // extension), second group leaves ≥ 16-byte suffixes (suffix VInt).
        let mut dict1030: Vec<String> = (0..700)
            .map(|i| format!("shared-prefix-is-here-{i:04}"))
            .collect();
        dict1030.extend((0..330).map(|i| format!("z{i:03}tail-padding-padding")));
        let dict1030_refs: Vec<&[u8]> = dict1030.iter().map(|s| s.as_bytes()).collect();
        let ords1030: Vec<(u32, u32)> = (0..2060u32).map(|d| (d, d % 1030)).collect();

        let (dvd, dvm) = run_writer("sorted", |w| {
            w.add_sorted_field(0, 300, &dict150_refs, &ords150).unwrap();
            w.add_sorted_field(2, 2060, &dict1030_refs, &ords1030)
                .unwrap();
        });

        check_footer(&dvd);
        check_footer(&dvm);
        let mut r = ByteReader::new(&dvm);
        parse_index_header(&mut r, "Lucene90DocValuesMetadata");

        // --- field 0 ---
        assert_eq!(r.read_le_i32(), 0);
        assert_eq!(r.read_u8(), TYPE_SORTED);
        let om0 = read_numeric_meta(&mut r);
        assert_eq!(
            (
                om0.docs_offset,
                om0.docs_length,
                om0.jump_count,
                om0.dense_rank_power
            ),
            (-1, 0, -1, -1)
        );
        assert_eq!(om0.num_values, 300);
        assert_eq!(om0.min, 0);
        assert_eq!(om0.gcd, 1);
        assert_eq!(om0.bpv, 8); // ubr(149)
        let got_ords0 = decode_ords(&dvd, &om0);
        let want_ords0: Vec<u32> = ords150.iter().map(|&(_, o)| o).collect();
        assert_eq!(got_ords0, want_ords0);

        let tm0 = read_terms_meta(&mut r);
        assert_eq!(tm0.dict_size, 150);
        assert_eq!(tm0.max_term_length, 9);
        assert_eq!(tm0.terms_data_offset, om0.values_offset + om0.values_length);
        let terms0 = decode_terms(&dvd, &tm0);
        assert_eq!(terms0.len(), 150);
        for (got, want) in terms0.iter().zip(dict150.iter()) {
            assert_eq!(got, want.as_bytes());
        }
        check_reverse_index(&dvd, &tm0, &dict150_refs);

        // --- field 2 ---
        assert_eq!(r.read_le_i32(), 2);
        assert_eq!(r.read_u8(), TYPE_SORTED);
        let om1 = read_numeric_meta(&mut r);
        assert_eq!(om1.num_values, 2060);
        assert_eq!(om1.bpv, 12); // ubr(1029)
        assert_eq!(
            om1.values_offset,
            tm0.terms_index_addresses_offset + tm0.terms_index_addresses_length
        );
        let got_ords1 = decode_ords(&dvd, &om1);
        let want_ords1: Vec<u32> = ords1030.iter().map(|&(_, o)| o).collect();
        assert_eq!(got_ords1, want_ords1);

        let tm1 = read_terms_meta(&mut r);
        assert_eq!(tm1.dict_size, 1030);
        assert_eq!(tm1.max_term_length, 26);
        assert_eq!(tm1.terms_data_offset, om1.values_offset + om1.values_length);
        let terms1 = decode_terms(&dvd, &tm1);
        assert_eq!(terms1.len(), 1030);
        for (got, want) in terms1.iter().zip(dict1030.iter()) {
            assert_eq!(got, want.as_bytes());
        }
        check_reverse_index(&dvd, &tm1, &dict1030_refs);

        assert_eq!(r.read_le_i32(), -1);
        assert_eq!(r.pos, dvm.len() - 16);
        assert_eq!(
            tm1.terms_index_addresses_offset + tm1.terms_index_addresses_length,
            dvd.len() as i64 - 16
        );
    }

    #[test]
    fn empty_fields_are_legal() {
        let (dvd, dvm) = run_writer("empty", |w| {
            w.add_numeric_field(0, 10, &[]).unwrap();
            w.add_sorted_field(1, 10, &[], &[]).unwrap();
        });

        check_footer(&dvd);
        check_footer(&dvm);
        // .dvd holds only header + footer
        assert_eq!(dvd.len(), index_header_length(DATA_CODEC, SUFFIX) + 16);

        let mut r = ByteReader::new(&dvm);
        parse_index_header(&mut r, "Lucene90DocValuesMetadata");

        // empty NUMERIC: docsWithField (-2, 0, -1, -1), bpv 0
        assert_eq!(r.read_le_i32(), 0);
        assert_eq!(r.read_u8(), TYPE_NUMERIC);
        let m = read_numeric_meta(&mut r);
        assert_eq!(
            (
                m.docs_offset,
                m.docs_length,
                m.jump_count,
                m.dense_rank_power
            ),
            (-2, 0, -1, -1)
        );
        assert_eq!(m.num_values, 0);
        assert_eq!(m.table_size, -1);
        assert_eq!(m.bpv, 0);
        assert_eq!(
            m.min,
            i64::MAX,
            "empty field keeps Long.MAX_VALUE min (:246)"
        );
        assert_eq!(m.values_length, 0);
        assert_eq!(m.value_jump_table_offset, -1);

        // empty SORTED: ords entry + termsDictSize 0
        assert_eq!(r.read_le_i32(), 1);
        assert_eq!(r.read_u8(), TYPE_SORTED);
        let om = read_numeric_meta(&mut r);
        assert_eq!((om.docs_offset, om.num_values, om.bpv), (-2, 0, 0));
        let tm = read_terms_meta(&mut r);
        assert_eq!(tm.dict_size, 0);
        assert_eq!(tm.addresses.num_values, 0, "zero blocks → zero DM records");
        assert_eq!(tm.max_term_length, 0);
        assert_eq!(tm.max_block_length, 0);
        assert_eq!(tm.terms_data_length, 0);
        assert_eq!(tm.terms_addresses_length, 0);
        assert_eq!(tm.index_addresses.num_values, 1, "single all-zero record");
        assert_eq!(tm.index_addresses.mins, vec![0]);
        assert_eq!(tm.index_addresses.bpvs, vec![0]);
        assert_eq!(tm.terms_index_length, 0);
        assert_eq!(tm.terms_index_addresses_length, 0);

        assert_eq!(r.read_le_i32(), -1);
        assert_eq!(r.pos, dvm.len() - 16);
    }
}
