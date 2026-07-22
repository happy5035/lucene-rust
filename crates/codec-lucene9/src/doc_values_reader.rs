//! Doc values reader for Lucene 90 format, mirroring
//! `codecs/lucene90/Lucene90DocValuesProducer.java` readNumeric path (9.12.3).
//!
//! Reads `{segment}_{suffix}.dvd` (data) and `.dvm` (metadata) files produced
//! by [`crate::doc_values::DocValuesWriter`], providing per-document numeric
//! values through `IndexedDISIReader` (docs-with-field) + `DirectReader`
//! (packed values).

use std::io;

use crate::codec_util::skip_index_header;
use crate::io::{HeapIndexInput, IndexInput};
use crate::packed::DirectReader;

// ---------------------------------------------------------------------------
// IndexedDISI constants (mirror doc_values.rs:52-57)
// ---------------------------------------------------------------------------
const DISI_BLOCK_SIZE: u32 = 65536;
const DISI_BLOCK_LONGS: usize = 1024;
const DISI_MAX_ARRAY_LENGTH: u32 = 4095;
const DISI_SENTINEL_BLOCK: u32 = 0x7FFF;

// .dvm type bytes (doc_values.rs:39-40)
const TYPE_NUMERIC: u8 = 0;
const TYPE_SORTED: u8 = 2;

/// DirectMonotonic block shift (doc_values.rs:43).
const DM_BLOCK_SHIFT: u32 = 16;
/// Terms dict block size: 64 terms per LZ4 block.
const TERMS_DICT_BLOCK_LZ4_SHIFT: u32 = 6;
const TERMS_DICT_BLOCK_SIZE: usize = 1 << TERMS_DICT_BLOCK_LZ4_SHIFT;
/// Reverse-index sample interval (doc_values.rs:48-49).
const TERMS_DICT_REVERSE_INDEX_SHIFT: u32 = 10;
const TERMS_DICT_REVERSE_INDEX_SIZE: usize = 1 << TERMS_DICT_REVERSE_INDEX_SHIFT;

// ---------------------------------------------------------------------------
// IndexedDISIReader
// ---------------------------------------------------------------------------

/// Branch type for the docs-with-field set.
#[derive(Debug, Clone)]
pub enum IndexedDISIBranch {
    /// All docs have values (offset == -1 in .dvm).
    All,
    /// Only a subset of docs have values; the vector is sorted ascending.
    Docs(Vec<u32>),
}

/// Decoded IndexedDISI structure — knows which documents have a value for a
/// given field.
#[derive(Debug, Clone)]
pub struct IndexedDISIReader {
    branch: IndexedDISIBranch,
    max_doc: u32,
}

impl IndexedDISIReader {
    /// Parse an IndexedDISI region from raw bytes.
    ///
    /// * `dvd_bytes` — the entire .dvd file content.
    /// * `offset` — byte offset into `dvd_bytes` where the DISI data begins,
    ///   or -1 (ALL docs have values), or -2 (NO doc has a value).
    /// * `length` — byte length of the DISI region (ignored for -1/-2).
    /// * `jump_count` — number of jump-table entries; verified but not needed
    ///   for simple lookups.
    pub fn parse(
        dvd_bytes: &[u8],
        offset: i64,
        length: i64,
        jump_count: i16,
        num_values: u32,
    ) -> io::Result<Self> {
        if offset == -1 {
            // ALL: every doc 0..num_values has a value
            return Ok(IndexedDISIReader {
                branch: IndexedDISIBranch::All,
                max_doc: num_values,
            });
        }
        if offset == -2 {
            // NONE
            return Ok(IndexedDISIReader {
                branch: IndexedDISIBranch::Docs(Vec::new()),
                max_doc: 0,
            });
        }

        let start = offset as usize;
        let end = start + length as usize;
        if end > dvd_bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "DISI region extends past .dvd",
            ));
        }
        let region = &dvd_bytes[start..end];
        let mut r = ByteReader::new(region);
        let mut docs: Vec<u32> = Vec::new();

        // Read blocks until sentinel
        let _sentinel_offset = loop {
            let header_offset = r.pos;
            let block_id = r.read_le_u16() as u32;
            let cardinality = r.read_le_u16() as u32 + 1;

            if block_id == DISI_SENTINEL_BLOCK {
                debug_assert_eq!(cardinality, 1);
                // Consume the sentinel's single u16 doc id
                let _sentinel_doc = r.read_le_u16();
                break header_offset;
            }

            if cardinality == DISI_BLOCK_SIZE {
                // ALL block: every intra-block doc has a value
                for i in 0..DISI_BLOCK_SIZE {
                    docs.push((block_id << 16) | i);
                }
            } else if cardinality > DISI_MAX_ARRAY_LENGTH {
                // DENSE block: rank table (256 bytes) + bit set (1024 u64 LE)
                let _rank = r.read_bytes(256);
                let mut bits = [0u64; DISI_BLOCK_LONGS];
                for w in bits.iter_mut() {
                    *w = r.read_le_u64();
                }
                let before_len = docs.len();
                for (word_index, &word) in bits.iter().enumerate() {
                    let mut w = word;
                    while w != 0 {
                        let bit = w.trailing_zeros();
                        docs.push((block_id << 16) | ((word_index as u32) << 6) | bit);
                        w &= w - 1;
                    }
                }
                debug_assert_eq!(
                    docs.len() - before_len,
                    cardinality as usize,
                    "DENSE block cardinality mismatch"
                );
            } else {
                // SPARSE block: `cardinality` u16 LE doc ids
                for _ in 0..cardinality {
                    let low = r.read_le_u16() as u32;
                    docs.push((block_id << 16) | low);
                }
            }
        };

        // Skip jump table: (lastRealBlock + 2) entries, each 8 bytes (i32 LE + i32 LE).
        // Omitted when the only real block is block 0.
        if jump_count > 0 {
            r.skip(jump_count as usize * 8);
        }

        // All bytes consumed
        debug_assert_eq!(r.pos, region.len(), "DISI region fully consumed");

        let max_doc = docs.last().map(|&d| d + 1).unwrap_or(0);
        Ok(IndexedDISIReader {
            branch: IndexedDISIBranch::Docs(docs),
            max_doc,
        })
    }

    /// True when the given doc has a value.
    #[inline]
    pub fn has_doc(&self, doc_id: u32) -> bool {
        match &self.branch {
            IndexedDISIBranch::All => doc_id < self.max_doc,
            IndexedDISIBranch::Docs(docs) => docs.binary_search(&doc_id).is_ok(),
        }
    }

    /// Index into the packed-values array for `doc_id`, or `None` when the doc
    /// has no value.
    #[inline]
    pub fn doc_index(&self, doc_id: u32) -> Option<usize> {
        match &self.branch {
            IndexedDISIBranch::All => {
                if doc_id < self.max_doc {
                    Some(doc_id as usize)
                } else {
                    None
                }
            }
            IndexedDISIBranch::Docs(docs) => docs.binary_search(&doc_id).ok(),
        }
    }
}

// ---------------------------------------------------------------------------
// NumericDocValuesReader
// ---------------------------------------------------------------------------

/// Reads per-document numeric values from `.dvm` / `.dvd` files.
pub struct NumericDocValuesReader {
    pub(crate) disi: IndexedDISIReader,
    pub(crate) min_value: i64,
    pub(crate) gcd: i64,
    #[allow(dead_code)]
    pub(crate) bpv: u8,
    pub(crate) values: DirectReader,
}

impl NumericDocValuesReader {
    /// Open a numeric field from in-memory `.dvm` and `.dvd` bytes.
    ///
    /// Returns an error when `field_number` is not found, is not NUMERIC, or
    /// the on-disk data is malformed.
    pub fn open(dvm_bytes: &[u8], dvd_bytes: &[u8], field_number: i32) -> io::Result<Self> {
        // --- parse .dvm --------------------------------------------------
        let mut dvm = HeapIndexInput::new(dvm_bytes.to_vec());
        skip_index_header(&mut dvm)?;

        loop {
            let fn_field = read_le_i32(&mut dvm)?; // field_number (i32 LE)
            if fn_field == -1 {
                // EOF marker
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("field {field_number} not found in .dvm"),
                ));
            }
            let field_type = dvm.read_byte()?;

            // Read the common numeric metadata prefix
            let docs_offset = read_le_i64(&mut dvm)?;
            let docs_length = read_le_i64(&mut dvm)?;
            let jump_count = read_le_i16(&mut dvm)?;
            let _dense_rank_power = dvm.read_byte()? as i8;
            let num_values = read_le_i64(&mut dvm)?;
            let table_size = read_le_i32(&mut dvm)?;
            for _ in 0..table_size.max(0) {
                read_le_i64(&mut dvm)?; // skip table entry
            }
            let bpv = dvm.read_byte()?;
            let min_value = read_le_i64(&mut dvm)?;
            let gcd = read_le_i64(&mut dvm)?;
            let values_offset = read_le_i64(&mut dvm)?;
            let values_length = read_le_i64(&mut dvm)?;
            let _value_jump_table_offset = read_le_i64(&mut dvm)?; // always -1

            if fn_field != field_number {
                // Skip past any additional metadata for non-NUMERIC types.
                if field_type == TYPE_SORTED {
                    skip_sorted_metadata(&mut dvm)?;
                }
                continue;
            }

            if field_type != TYPE_NUMERIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("field {field_number} is not NUMERIC (type byte {field_type})"),
                ));
            }

            // --- parse IndexedDISI ---------------------------------------
            let disi = IndexedDISIReader::parse(
                dvd_bytes,
                docs_offset,
                docs_length,
                jump_count,
                num_values as u32,
            )?;

            // --- create DirectReader for packed values --------------------
            let num_values = num_values as usize;
            let values_input: Box<dyn IndexInput> = if values_length > 0 {
                Box::new(HeapIndexInput::new(
                    dvd_bytes[values_offset as usize..][..values_length as usize].to_vec(),
                ))
            } else {
                Box::new(HeapIndexInput::new(Vec::new()))
            };
            let values = DirectReader::new(values_input, bpv as u32, num_values, 0);

            return Ok(NumericDocValuesReader {
                disi,
                min_value,
                gcd,
                bpv,
                values,
            });
        }
    }

    /// Returns the numeric value for `doc_id`, or `None` when the doc has no
    /// value for this field.
    pub fn get(&mut self, doc_id: u32) -> io::Result<Option<i64>> {
        match self.disi.doc_index(doc_id) {
            None => Ok(None),
            Some(idx) => {
                let packed = self.values.get(idx)?;
                let value = (packed as i64)
                    .wrapping_mul(self.gcd)
                    .wrapping_add(self.min_value);
                Ok(Some(value))
            }
        }
    }

    /// Bulk read: returns one `Option<i64>` per element in `docs`.
    pub fn get_batch(&mut self, docs: &[u32]) -> io::Result<Vec<Option<i64>>> {
        let mut result = Vec::with_capacity(docs.len());
        for &doc in docs {
            result.push(self.get(doc)?);
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Helper: read LE primitives from an IndexInput
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

fn read_le_i64(input: &mut dyn IndexInput) -> io::Result<i64> {
    let mut buf = [0u8; 8];
    input.read_bytes(&mut buf, 0, 8)?;
    Ok(i64::from_le_bytes(buf))
}

// ---------------------------------------------------------------------------
// Helper: skip SORTED field tail in .dvm
// ---------------------------------------------------------------------------

/// Consume the terms-dict + reverse-index metadata that follows the common
/// numeric prefix in a SORTED field entry.
fn skip_sorted_metadata(input: &mut dyn IndexInput) -> io::Result<()> {
    let dict_size = input.read_vlong()?;
    let block_shift = read_le_i32(input)?;
    debug_assert_eq!(block_shift as u32, DM_BLOCK_SHIFT);

    // Terms-dict addresses: DirectMonotonic meta.
    // First compute the number of DM *values* (64-term block addresses),
    // then the number of DM *blocks* (meta records) from that — matching
    // the formula used by DvDirectMonotonicReader::read_meta.
    let num_dict_dm_values = if dict_size == 0 {
        0
    } else {
        (dict_size as usize).div_ceil(TERMS_DICT_BLOCK_SIZE)
    };
    let num_dict_dm_blocks = if num_dict_dm_values == 0 {
        0
    } else {
        ((num_dict_dm_values - 1) >> DM_BLOCK_SHIFT) + 1
    };
    skip_dm_meta(input, num_dict_dm_blocks)?;

    // maxTermLength, maxBlockLength, termsDataOffset, termsDataLength,
    // termsAddressesOffset, termsAddressesLength
    let _ = read_le_i32(input)?;
    let _ = read_le_i32(input)?;
    let _ = read_le_i64(input)?;
    let _ = read_le_i64(input)?;
    let _ = read_le_i64(input)?;
    let _ = read_le_i64(input)?;

    // Reverse-index shift
    let index_shift = read_le_i32(input)?;
    debug_assert_eq!(index_shift as u32, TERMS_DICT_REVERSE_INDEX_SHIFT);

    // Reverse-index addresses: DirectMonotonic meta.
    // num_index_records is the count of DM *values*; we must compute the
    // number of DM *blocks* (meta records) the same way read_meta does.
    let num_index_records = if dict_size == 0 {
        1
    } else {
        1 + (dict_size as usize).div_ceil(TERMS_DICT_REVERSE_INDEX_SIZE)
    };
    let num_index_dm_blocks = if num_index_records == 0 {
        0
    } else {
        ((num_index_records - 1) >> DM_BLOCK_SHIFT) + 1
    };
    skip_dm_meta(input, num_index_dm_blocks)?;

    // termsIndexOffset, termsIndexLength, termsIndexAddressesOffset,
    // termsIndexAddressesLength
    let _ = read_le_i64(input)?;
    let _ = read_le_i64(input)?;
    let _ = read_le_i64(input)?;
    let _ = read_le_i64(input)?;
    Ok(())
}

/// Skip `count` DirectMonotonic meta records (21 bytes each: min:i64 LE +
/// avgInc:i32 LE + offset:i64 LE + bpv:u8).
fn skip_dm_meta(input: &mut dyn IndexInput, count: usize) -> io::Result<()> {
    for _ in 0..count {
        let _min = read_le_i64(input)?;
        let _avg = read_le_i32(input)?;
        let _off = read_le_i64(input)?;
        let _bpv = input.read_byte()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ByteReader — simple cursor over a &[u8] (for IndexedDISI parsing)
// ---------------------------------------------------------------------------

struct ByteReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        ByteReader { bytes, pos: 0 }
    }

    fn read_bytes(&mut self, n: usize) -> &'a [u8] {
        let b = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        b
    }

    fn read_n_bytes(&mut self, n: usize) -> Vec<u8> {
        let b = self.bytes[self.pos..self.pos + n].to_vec();
        self.pos += n;
        b
    }

    fn read_u8(&mut self) -> u8 {
        let b = self.bytes[self.pos];
        self.pos += 1;
        b
    }

    fn read_le_u16(&mut self) -> u16 {
        u16::from_le_bytes(self.read_bytes(2).try_into().unwrap())
    }

    fn read_le_u64(&mut self) -> u64 {
        u64::from_le_bytes(self.read_bytes(8).try_into().unwrap())
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

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }
}

// ---------------------------------------------------------------------------
// DvDirectMonotonicReader — DocValues-specific DM format
// ---------------------------------------------------------------------------

/// Reads DirectMonotonic values in the DocValues wire format:
/// 21-byte-per-block meta records in .dvm (min:i64 LE, avgInc:f32 LE,
/// offset:i64 LE, bpv:u8), with packed delta data in a separate .dvd slice.
struct DvDirectMonotonicReader {
    mins: Vec<i64>,
    avgs: Vec<f32>,
    offsets: Vec<u64>,
    bpvs: Vec<u8>,
    data: Vec<u8>,
    block_shift: u32,
    num_values: usize,
}

impl DvDirectMonotonicReader {
    /// Read DM meta records from `dvm` (positioned at the first block's min)
    /// and slice the packed delta data from `dvd_bytes`.
    fn read_meta(
        dvm: &mut dyn IndexInput,
        dvd_bytes: &[u8],
        data_offset: i64,
        data_length: i64,
        num_values: usize,
        block_shift: u32,
    ) -> io::Result<Self> {
        let num_blocks = if num_values == 0 {
            0
        } else {
            ((num_values - 1) / (1 << block_shift)) + 1
        };
        let mut mins = Vec::with_capacity(num_blocks);
        let mut avgs = Vec::with_capacity(num_blocks);
        let mut offsets = Vec::with_capacity(num_blocks);
        let mut bpvs = Vec::with_capacity(num_blocks);
        for _ in 0..num_blocks {
            mins.push(read_le_i64(dvm)?);
            let avg_bits = read_le_i32(dvm)?;
            avgs.push(f32::from_bits(avg_bits as u32));
            offsets.push(read_le_i64(dvm)? as u64);
            bpvs.push(dvm.read_byte()?);
        }
        let data = if data_length > 0 {
            dvd_bytes[data_offset as usize..][..data_length as usize].to_vec()
        } else {
            Vec::new()
        };
        Ok(DvDirectMonotonicReader {
            mins,
            avgs,
            offsets,
            bpvs,
            data,
            block_shift,
            num_values,
        })
    }

    fn get(&self, index: usize) -> u64 {
        if index >= self.num_values {
            return 0;
        }
        let block = index >> self.block_shift;
        let in_block = (index - (block << self.block_shift)) as u64;
        let bpv = self.bpvs[block] as usize;
        let delta = if bpv == 0 {
            0u64
        } else {
            let bit_offset = self.offsets[block] as usize * 8 + in_block as usize * bpv;
            let byte_offset = bit_offset / 8;
            let shift = bit_offset % 8;
            let mut buf = [0u8; 8];
            let available = self.data.len().saturating_sub(byte_offset);
            let take = available.min(8);
            buf[..take].copy_from_slice(&self.data[byte_offset..byte_offset + take]);
            let raw = u64::from_le_bytes(buf) >> shift;
            if bpv == 64 {
                raw
            } else {
                raw & ((1u64 << bpv) - 1)
            }
        };
        (self.mins[block]
            .wrapping_add((self.avgs[block] * in_block as f32) as i64)
            .wrapping_add(delta as i64)) as u64
    }
}

// ---------------------------------------------------------------------------
// ReverseTermsIndex
// ---------------------------------------------------------------------------

/// Reverse index for sorted terms: maps ord ranges to sort-key prefixes.
#[allow(dead_code)]
struct ReverseTermsIndex {
    index_data: Vec<u8>,
    addresses: DvDirectMonotonicReader,
}

// ---------------------------------------------------------------------------
// SortedDocValuesReader
// ---------------------------------------------------------------------------

/// Reads per-document sorted values (ordinals + terms dictionary) from
/// `.dvm` / `.dvd` files produced by a SORTED `DocValuesWriter`.
pub struct SortedDocValuesReader {
    ords: NumericDocValuesReader,
    max_ord: u32,
    terms_dict: Vec<u8>,
    block_addrs: DvDirectMonotonicReader,
    #[allow(dead_code)]
    reverse_index: ReverseTermsIndex,
}

impl SortedDocValuesReader {
    /// Open a sorted field from in-memory `.dvm` and `.dvd` bytes.
    pub fn open(dvm_bytes: &[u8], dvd_bytes: &[u8], field_number: i32) -> io::Result<Self> {
        let mut dvm = HeapIndexInput::new(dvm_bytes.to_vec());
        skip_index_header(&mut dvm)?;

        loop {
            let fn_field = read_le_i32(&mut dvm)?;
            if fn_field == -1 {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("field {field_number} not found in .dvm"),
                ));
            }
            let field_type = dvm.read_byte()?;

            // --- common numeric metadata prefix ---------------------------
            let docs_offset = read_le_i64(&mut dvm)?;
            let docs_length = read_le_i64(&mut dvm)?;
            let jump_count = read_le_i16(&mut dvm)?;
            let _dense_rank_power = dvm.read_byte()? as i8;
            let num_values = read_le_i64(&mut dvm)?;
            let table_size = read_le_i32(&mut dvm)?;
            for _ in 0..table_size.max(0) {
                read_le_i64(&mut dvm)?;
            }
            let bpv = dvm.read_byte()?;
            let min_value = read_le_i64(&mut dvm)?;
            let gcd = read_le_i64(&mut dvm)?;
            let values_offset = read_le_i64(&mut dvm)?;
            let values_length = read_le_i64(&mut dvm)?;
            let _value_jump_table_offset = read_le_i64(&mut dvm)?;

            // --- sorted metadata tail -------------------------------
            if fn_field != field_number {
                if field_type == TYPE_SORTED {
                    skip_sorted_metadata(&mut dvm)?;
                }
                continue;
            }

            if field_type != TYPE_SORTED {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "field {field_number} is not SORTED (type byte {field_type})"
                    ),
                ));
            }

            // --- build NumericDocValuesReader for ordinals ----------
            let disi = IndexedDISIReader::parse(
                dvd_bytes,
                docs_offset,
                docs_length,
                jump_count,
                num_values as u32,
            )?;

            let ords_input: Box<dyn IndexInput> = if values_length > 0 {
                Box::new(HeapIndexInput::new(
                    dvd_bytes[values_offset as usize..]
                        [..values_length as usize]
                        .to_vec(),
                ))
            } else {
                Box::new(HeapIndexInput::new(Vec::new()))
            };
            let ords_values =
                DirectReader::new(ords_input, bpv as u32, num_values as usize, 0);

            let ords = NumericDocValuesReader {
                disi,
                min_value,
                gcd,
                bpv,
                values: ords_values,
            };

            // --- terms dict metadata --------------------------------
            let dict_size = dvm.read_vlong()?;
            let max_ord = dict_size as u32;
            let block_shift = read_le_i32(&mut dvm)?;
            debug_assert_eq!(block_shift as u32, DM_BLOCK_SHIFT);

            // Number of DM values = number of 64-term blocks
            let num_dm_values = if dict_size == 0 {
                0usize
            } else {
                (dict_size as usize).div_ceil(TERMS_DICT_BLOCK_SIZE)
            };

            let block_addrs = DvDirectMonotonicReader::read_meta(
                &mut dvm,
                dvd_bytes,
                0, // dummy — we read meta inline from dvm first, data later
                0, // dummy
                num_dm_values,
                block_shift as u32,
            )?;

            // After read_meta consumes the meta records, we continue with
            // the rest of the fields.  The packed data for block addresses
            // comes later (termsAddressesOffset/Length), so we'll rebuild
            // block_addrs once we know those offsets.

            let _max_term_length = read_le_i32(&mut dvm)?;
            let _max_block_length = read_le_i32(&mut dvm)?;
            let terms_data_offset = read_le_i64(&mut dvm)?;
            let terms_data_length = read_le_i64(&mut dvm)?;
            let terms_addresses_offset = read_le_i64(&mut dvm)?;
            let terms_addresses_length = read_le_i64(&mut dvm)?;

            // Rebuild block_addrs with the correct packed data
            let block_addrs = DvDirectMonotonicReader {
                data: if terms_addresses_length > 0 {
                    dvd_bytes[terms_addresses_offset as usize..]
                        [..terms_addresses_length as usize]
                        .to_vec()
                } else {
                    Vec::new()
                },
                ..block_addrs
            };

            // Terms dictionary raw bytes
            let terms_dict = if terms_data_length > 0 {
                dvd_bytes[terms_data_offset as usize..]
                    [..terms_data_length as usize]
                    .to_vec()
            } else {
                Vec::new()
            };

            // --- reverse index metadata ----------------------------
            let index_shift = read_le_i32(&mut dvm)?;
            debug_assert_eq!(index_shift as u32, TERMS_DICT_REVERSE_INDEX_SHIFT);

            let num_index_records = if dict_size == 0 {
                1
            } else {
                1 + (dict_size as usize).div_ceil(TERMS_DICT_REVERSE_INDEX_SIZE)
            };

            let rev_addrs = DvDirectMonotonicReader::read_meta(
                &mut dvm,
                dvd_bytes,
                0,
                0,
                num_index_records,
                block_shift as u32,
            )?;

            let terms_index_offset = read_le_i64(&mut dvm)?;
            let terms_index_length = read_le_i64(&mut dvm)?;
            let terms_index_addresses_offset = read_le_i64(&mut dvm)?;
            let terms_index_addresses_length = read_le_i64(&mut dvm)?;

            let reverse_index = ReverseTermsIndex {
                index_data: if terms_index_length > 0 {
                    dvd_bytes[terms_index_offset as usize..]
                        [..terms_index_length as usize]
                        .to_vec()
                } else {
                    Vec::new()
                },
                addresses: DvDirectMonotonicReader {
                    data: if terms_index_addresses_length > 0 {
                        dvd_bytes[terms_index_addresses_offset as usize..]
                            [..terms_index_addresses_length as usize]
                            .to_vec()
                    } else {
                        Vec::new()
                    },
                    ..rev_addrs
                },
            };

            return Ok(SortedDocValuesReader {
                ords,
                max_ord,
                terms_dict,
                block_addrs,
                reverse_index,
            });
        }
    }

    /// Returns the ordinal for `doc_id`, or `None` when the doc has no value.
    pub fn get_ord(&mut self, doc_id: u32) -> io::Result<Option<u32>> {
        match self.ords.get(doc_id)? {
            None => Ok(None),
            Some(v) => Ok(Some(v as u32)),
        }
    }

    /// Returns the term bytes for the given ordinal.
    ///
    /// Returns an empty vec when `ord` is out of range or the dictionary is
    /// empty.
    pub fn lookup_ord(&self, ord: u32) -> Vec<u8> {
        if self.terms_dict.is_empty() || ord as usize >= self.max_ord as usize {
            return Vec::new();
        }

        let block_index = ord >> TERMS_DICT_BLOCK_LZ4_SHIFT;
        let in_block = ord & (TERMS_DICT_BLOCK_SIZE as u32 - 1);

        let block_start = self.block_addrs.get(block_index as usize) as usize;
        let block_end = if (block_index as usize + 1) < self.block_addrs.num_values {
            self.block_addrs.get(block_index as usize + 1) as usize
        } else {
            self.terms_dict.len()
        };

        let region = &self.terms_dict[block_start..block_end];
        let mut r = ByteReader::new(region);

        // First term — always verbatim: VInt length + bytes
        let first_len = r.read_vint() as usize;
        let mut term = r.read_n_bytes(first_len);
        if in_block == 0 {
            return term;
        }

        // Remaining terms are LZ4-compressed prefix-compressed entries.
        // One-term blocks have no LZ4 section (addTermsDict :607-611).
        if r.pos >= region.len() {
            // Single-term block — but we asked for in_block > 0, should not happen
            return Vec::new();
        }

        let uncompressed = r.read_vint() as usize;
        let decompressed = lz4::block::decompress(
            &region[r.pos..],
            Some(uncompressed as i32),
        )
        .expect("LZ4 decompress should succeed for well-formed blocks");
        let mut dr = ByteReader::new(&decompressed);

        // Walk prefix-compressed entries until the target in-block ordinal
        for _ in 0..in_block {
            let token = dr.read_u8() as usize;
            let mut prefix = token & 0x0F;
            let mut suffix_len = 1 + (token >> 4);
            if prefix == 15 {
                prefix += dr.read_vint() as usize;
            }
            if suffix_len == 16 {
                suffix_len += dr.read_vint() as usize;
            }
            let suffix = dr.read_n_bytes(suffix_len);
            term.truncate(prefix);
            term.extend_from_slice(&suffix);
        }

        term
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::FSDirectory;
    use crate::doc_values::DocValuesWriter;
    use std::fs;

    const SEGMENT_ID: [u8; 16] = [0xA1; 16];
    const SUFFIX: &str = "Lucene90_0";

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codec-lucene9-dvr-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Round-trip: write → read → compare.
    fn round_trip(
        tag: &str,
        max_doc: u32,
        values: &[(u32, i64)],
        check_docs: &[u32],
    ) {
        let root = temp_dir(tag);
        let dir = FSDirectory::open(&root).unwrap();
        let mut w = DocValuesWriter::new(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        w.add_numeric_field(3, max_doc, values).unwrap();
        let names = w.finish().unwrap();
        let dvd = fs::read(root.join(&names[0])).unwrap();
        let dvm = fs::read(root.join(&names[1])).unwrap();
        fs::remove_dir_all(&root).unwrap();

        let mut reader = NumericDocValuesReader::open(&dvm, &dvd, 3).unwrap();

        // Build expected map: doc → value
        let expected: std::collections::BTreeMap<u32, i64> =
            values.iter().copied().collect();

        // Test individual get()
        for &doc in check_docs {
            let got = reader.get(doc).unwrap();
            let want = expected.get(&doc).copied();
            assert_eq!(
                got, want,
                "get({doc}): got {got:?}, want {want:?}"
            );
        }

        // Test get() for all docs 0..max_doc
        for doc in 0..max_doc {
            let got = reader.get(doc).unwrap();
            let want = expected.get(&doc).copied();
            assert_eq!(
                got, want,
                "get({doc}): got {got:?}, want {want:?}"
            );
        }

        // Test get_batch()
        let batch_docs: Vec<u32> = (0..max_doc).collect();
        let batch = reader.get_batch(&batch_docs).unwrap();
        for (doc, got) in batch_docs.iter().zip(batch.iter()) {
            let want = expected.get(doc).copied();
            assert_eq!(*got, want, "get_batch({doc}): got {got:?}, want {want:?}");
        }
    }

    // ------------------------------------------------------------------

    #[test]
    fn all_docs_constant() {
        let max_doc = 100u32;
        let values: Vec<(u32, i64)> = (0..max_doc).map(|d| (d, 42)).collect();
        round_trip("constant", max_doc, &values, &[0, 50, 99]);
    }

    #[test]
    fn all_docs_linear() {
        let max_doc = 1000u32;
        let values: Vec<(u32, i64)> = (0..max_doc).map(|d| (d, d as i64 * 7 - 3000)).collect();
        round_trip("linear", max_doc, &values, &[0, 500, 999]);
    }

    #[test]
    fn all_docs_extreme_i64() {
        let max_doc = 100u32;
        let mut vals = vec![i64::MIN, i64::MAX, 0, -1, 1];
        vals.extend((5..max_doc).map(|d| (d as i64) << 40));
        let values: Vec<(u32, i64)> = vals.iter().enumerate().map(|(d, &v)| (d as u32, v)).collect();
        round_trip("extreme", max_doc, &values, &[0, 1, 2, 3, 4, 50]);
    }

    #[test]
    fn sparse_mixed_blocks() {
        let max_doc = 200_000u32;
        let mut values: Vec<(u32, i64)> = Vec::new();

        // SPARSE block 0: 3 docs
        for d in [5u32, 100, 4095] {
            values.push((d, d as i64 * 11 - 20_000));
        }
        // DENSE block 1: 5000 docs
        for i in 0..5000u32 {
            let d = 65536 + i;
            values.push((d, d as i64 * 11 - 20_000));
        }
        // ALL block 2: 65536 docs
        for i in 0..65536u32 {
            let d = 131072 + i;
            values.push((d, d as i64 * 11 - 20_000));
        }
        // SPARSE block 3: 1 doc
        let last_doc = 196608 + 42;
        values.push((last_doc, last_doc as i64 * 11 - 20_000));

        let check_docs = &[
            0,
            5,
            100,
            65536,
            65536 + 4999,
            131072,
            131072 + 32767,
            196608 - 1,
            last_doc,
            last_doc + 1,
        ];
        round_trip("mixed", max_doc, &values, check_docs);
    }

    #[test]
    fn single_sparse_block() {
        let max_doc = 500u32;
        let values: Vec<(u32, i64)> = (0..100u32).map(|d| (d * 2, 7)).collect();
        round_trip("single", max_doc, &values, &[0, 1, 2, 198, 199]);
    }

    #[test]
    fn block_one_only() {
        let max_doc = 200_000u32;
        let values: Vec<(u32, i64)> = (0..10u32).map(|d| (70000 + d, 1000 + d as i64)).collect();
        round_trip("blkone", max_doc, &values, &[0, 69999, 70000, 70009, 70010]);
    }

    #[test]
    fn empty_field() {
        let max_doc = 10u32;
        let values: Vec<(u32, i64)> = vec![];
        round_trip("empty", max_doc, &values, &[0, 5, 9]);
    }

    #[test]
    fn field_not_found_is_error() {
        let root = temp_dir("notfound");
        let dir = FSDirectory::open(&root).unwrap();
        let mut w = DocValuesWriter::new(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        w.add_numeric_field(1, 10, &[(0, 42)]).unwrap();
        let names = w.finish().unwrap();
        let dvd = fs::read(root.join(&names[0])).unwrap();
        let dvm = fs::read(root.join(&names[1])).unwrap();
        fs::remove_dir_all(&root).unwrap();

        // Field 99 not in the file
        assert!(NumericDocValuesReader::open(&dvm, &dvd, 99).is_err());
    }

    // ------------------------------------------------------------------
    // Sorted doc values round-trip tests
    // ------------------------------------------------------------------

    /// Round-trip for sorted fields: write → read → compare ords + terms.
    fn sorted_round_trip(
        tag: &str,
        max_doc: u32,
        dict: &[&[u8]],
        ords: &[(u32, u32)],
        check_docs: &[u32],
    ) {
        let root = temp_dir(tag);
        let dir = FSDirectory::open(&root).unwrap();
        let mut w = DocValuesWriter::new(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        w.add_sorted_field(1, max_doc, dict, ords).unwrap();
        let names = w.finish().unwrap();
        let dvd = fs::read(root.join(&names[0])).unwrap();
        let dvm = fs::read(root.join(&names[1])).unwrap();
        fs::remove_dir_all(&root).unwrap();

        let mut reader = SortedDocValuesReader::open(&dvm, &dvd, 1).unwrap();

        // Build expected maps
        let ord_map: std::collections::BTreeMap<u32, u32> =
            ords.iter().map(|&(d, o)| (d, o)).collect();

        // Test get_ord() for check_docs
        for &doc in check_docs {
            let got = reader.get_ord(doc).unwrap();
            let want = ord_map.get(&doc).copied();
            assert_eq!(
                got, want,
                "get_ord({doc}): got {got:?}, want {want:?}"
            );
        }

        // Test get_ord() for all docs 0..max_doc
        for doc in 0..max_doc {
            let got = reader.get_ord(doc).unwrap();
            let want = ord_map.get(&doc).copied();
            assert_eq!(
                got, want,
                "get_ord({doc}): got {got:?}, want {want:?}"
            );
        }

        // Test lookup_ord() for all known ords
        for ord in 0..dict.len() as u32 {
            let term = reader.lookup_ord(ord);
            assert_eq!(
                &term, dict[ord as usize],
                "lookup_ord({ord}): got {term:?}, want {:?}",
                dict[ord as usize]
            );
        }
    }

    #[test]
    fn sorted_single_block() {
        let dict: Vec<String> = (0..50).map(|i| format!("term-{i:03}")).collect();
        let dict_refs: Vec<&[u8]> = dict.iter().map(|s| s.as_bytes()).collect();
        let ords: Vec<(u32, u32)> = (0..500u32).map(|d| (d, d % 50)).collect();
        sorted_round_trip(
            "srt-single",
            500,
            &dict_refs,
            &ords,
            &[0, 1, 49, 50, 250, 499],
        );
    }

    #[test]
    fn sorted_multi_block_with_vint_extensions() {
        // 150 terms → 3 dict blocks; tests prefix/suffix VInt extensions
        let dict150: Vec<String> = (0..150).map(|i| format!("term-{i:04}")).collect();
        let dict150_refs: Vec<&[u8]> = dict150.iter().map(|s| s.as_bytes()).collect();
        let ords150: Vec<(u32, u32)> = (0..300u32).map(|d| (d, d % 150)).collect();
        sorted_round_trip(
            "srt-150",
            300,
            &dict150_refs,
            &ords150,
            &[0, 75, 149, 150, 299],
        );

        // 1030 terms → 17 dict blocks; >15-byte common prefix (prefix VInt)
        // and >=16-byte suffixes (suffix VInt)
        let mut dict1030: Vec<String> = (0..700)
            .map(|i| format!("shared-prefix-is-here-{i:04}"))
            .collect();
        dict1030.extend((0..330).map(|i| format!("z{i:03}tail-padding-padding")));
        let dict1030_refs: Vec<&[u8]> = dict1030.iter().map(|s| s.as_bytes()).collect();
        let ords1030: Vec<(u32, u32)> = (0..2060u32).map(|d| (d, d % 1030)).collect();
        sorted_round_trip(
            "srt-1030",
            2060,
            &dict1030_refs,
            &ords1030,
            &[0, 512, 1024, 1025, 1500, 2059],
        );
    }

    #[test]
    fn sorted_sparse_ords() {
        // Sparse ords — every ord must be referenced (writer requirement)
        let dict: Vec<String> = (0..8).map(|i| format!("color-{i}")).collect();
        let dict_refs: Vec<&[u8]> = dict.iter().map(|s| s.as_bytes()).collect();
        let max_doc = 100_000u32;
        let ords: Vec<(u32, u32)> = vec![
            (0, 0),
            (1000, 1),
            (2000, 4),   // ord 4 used
            (3000, 2),
            (4000, 5),   // ord 5 used
            (50000, 3),
            (65536, 6),  // ord 6 used
            (65537, 7),
            (90000, 3),
        ];
        sorted_round_trip("srt-sparse", max_doc, &dict_refs, &ords, &[0, 1, 999, 1000, 65536]);
    }

    #[test]
    fn sorted_all_docs_same_ord() {
        let dict: Vec<String> = vec!["only-term".to_string()];
        let dict_refs: Vec<&[u8]> = dict.iter().map(|s| s.as_bytes()).collect();
        let ords: Vec<(u32, u32)> = (0..1000u32).map(|d| (d, 0)).collect();
        sorted_round_trip("srt-same", 1000, &dict_refs, &ords, &[0, 500, 999]);
    }

    #[test]
    fn sorted_empty_field() {
        let dict: Vec<&[u8]> = vec![];
        let ords: Vec<(u32, u32)> = vec![];
        sorted_round_trip("srt-empty", 10, &dict, &ords, &[0, 5, 9]);
    }

    #[test]
    fn sorted_field_not_found_is_error() {
        let root = temp_dir("srt-notfound");
        let dir = FSDirectory::open(&root).unwrap();
        let mut w = DocValuesWriter::new(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        w.add_sorted_field(1, 10, &[b"hello"], &[(0, 0)])
            .unwrap();
        let names = w.finish().unwrap();
        let dvd = fs::read(root.join(&names[0])).unwrap();
        let dvm = fs::read(root.join(&names[1])).unwrap();
        fs::remove_dir_all(&root).unwrap();

        // Field 99 not in the file
        assert!(SortedDocValuesReader::open(&dvm, &dvd, 99).is_err());
    }
}
