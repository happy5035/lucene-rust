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
    disi: IndexedDISIReader,
    min_value: i64,
    gcd: i64,
    #[allow(dead_code)]
    bpv: u8,
    values: DirectReader,
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

    // Terms-dict addresses: DirectMonotonic meta
    let num_dict_blocks = if dict_size == 0 {
        0
    } else {
        ((dict_size as usize - 1) >> DM_BLOCK_SHIFT) + 1
    };
    skip_dm_meta(input, num_dict_blocks)?;

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

    // Reverse-index addresses: DirectMonotonic meta
    let num_index_records = if dict_size == 0 {
        1
    } else {
        1 + (dict_size as usize).div_ceil(TERMS_DICT_REVERSE_INDEX_SIZE)
    };
    skip_dm_meta(input, num_index_records)?;

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

    fn read_le_u16(&mut self) -> u16 {
        u16::from_le_bytes(self.read_bytes(2).try_into().unwrap())
    }

    fn read_le_u64(&mut self) -> u64 {
        u64::from_le_bytes(self.read_bytes(8).try_into().unwrap())
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }
}

// ============================================================================
// Tests
// ============================================================================

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
}
