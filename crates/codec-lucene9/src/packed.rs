//! Packed integer writers mirroring `util/packed/DirectWriter.java` and
//! `util/packed/DirectMonotonicWriter.java` (9.12.3). Used for the .fdx index
//! of stored fields.

use std::io;

use crate::io::ChecksumIndexOutput;

/// DirectWriter.SUPPORTED_BITS_PER_VALUE (:225-226).
const SUPPORTED_BITS_PER_VALUE: [u32; 14] = [1, 2, 4, 8, 12, 16, 20, 24, 28, 32, 40, 48, 56, 64];

/// PackedInts.unsignedBitsRequired (:804-806): at least 1.
fn unsigned_bits_required(bits: u64) -> u32 {
    (64 - bits.leading_zeros()).max(1)
}

/// DirectWriter.roundBits (:193-201): next supported bpv >= the given one.
fn round_bits(bits_required: u32) -> u32 {
    SUPPORTED_BITS_PER_VALUE
        .iter()
        .copied()
        .find(|&b| b >= bits_required)
        .expect("bitsRequired <= 64")
}

/// DirectWriter.unsignedBitsRequired (:221-223).
pub fn direct_writer_unsigned_bits_required(max_value: u64) -> u32 {
    round_bits(unsigned_bits_required(max_value))
}

/// PackedInts.Format.PACKED.byteCount (:77-79).
fn packed_byte_count(num_values: usize, bits_per_value: u32) -> usize {
    (num_values * bits_per_value as usize).div_ceil(8)
}

/// Encodes `values` exactly like DirectWriter (encode :101-142 + flush :88-99):
/// little-endian packing, stream truncated to PACKED.byteCount bytes, then the
/// final container padding of DirectWriter.finish (:145-174).
pub fn direct_writer_encode(values: &[u64], bits_per_value: u32) -> Vec<u8> {
    debug_assert!(SUPPORTED_BITS_PER_VALUE.contains(&bits_per_value));
    let num_values = values.len();
    let byte_count = packed_byte_count(num_values, bits_per_value);
    let mut blocks = Vec::new();

    if bits_per_value % 8 == 0 {
        // bpv 8,16,24,32,40,48,56,64: plain little-endian values
        let bytes_per_value = (bits_per_value / 8) as usize;
        for &v in values {
            blocks.extend_from_slice(&v.to_le_bytes()[..bytes_per_value]);
        }
    } else if bits_per_value < 8 {
        // bpv 1,2,4: pack valuesPerLong values into one LE long
        let values_per_long = 64 / bits_per_value as usize;
        let mut i = 0;
        while i < num_values {
            let mut v: u64 = 0;
            for j in 0..values_per_long {
                let val = if i + j < num_values { values[i + j] } else { 0 };
                v |= val << (bits_per_value as usize * j);
            }
            blocks.extend_from_slice(&v.to_le_bytes());
            i += values_per_long;
        }
    } else {
        // bpv 12,20,28: values two by two; successive container ints are
        // written numBytesFor2Values (= bpv*2/8) apart and OVERLAP by design
        // (DirectWriter.encode :125-141 + flush's PACKED.byteCount truncation).
        let num_bytes_for_2 = (bits_per_value * 2 / 8) as usize;
        let mut i = 0;
        let mut o = 0;
        while i < num_values {
            let l1 = values[i];
            let l2 = if i + 1 < num_values { values[i + 1] } else { 0 };
            let merged = l1 | (l2 << bits_per_value);
            let container_bytes = if bits_per_value <= 16 { 4 } else { 8 };
            if blocks.len() < o + container_bytes {
                blocks.resize(o + container_bytes, 0);
            }
            if bits_per_value <= 16 {
                blocks[o..o + 4].copy_from_slice(&(merged as u32).to_le_bytes());
            } else {
                blocks[o..o + 8].copy_from_slice(&merged.to_le_bytes());
            }
            i += 2;
            o += num_bytes_for_2;
        }
    }
    blocks.truncate(byte_count);

    // DirectWriter.finish padding (:156-173): enough zero bytes that any value
    // can be read with a single container-width read.
    let padding_bits = if bits_per_value > 32 {
        64 - bits_per_value
    } else if bits_per_value > 16 {
        32 - bits_per_value
    } else if bits_per_value > 8 {
        16 - bits_per_value
    } else {
        0
    };
    let padding_bytes = padding_bits.div_ceil(8) as usize;
    blocks.extend(std::iter::repeat_n(0u8, padding_bytes));
    blocks
}

/// Writes a monotonically-increasing sequence as DirectMonotonicWriter does:
/// blocks of 2^blockShift values; per block a meta record (LE long min, LE int
/// float bits of avgInc, LE long data offset relative to `base_data_pointer`,
/// byte bpv) goes to `meta`, packed deltas go to `data`
/// (DirectMonotonicWriter.flush :77-114).
///
/// Returns the meta/data streams untouched apart from the appended bytes.
pub fn direct_monotonic_write(
    meta: &mut ChecksumIndexOutput,
    data: &mut ChecksumIndexOutput,
    values: &[u64],
    block_shift: u32,
) -> io::Result<()> {
    assert!((2..=22).contains(&block_shift), "blockShift out of range");
    let block_size = 1usize << block_shift;
    let base_data_pointer = data.file_pointer();

    for block in values.chunks(block_size) {
        // avgInc: double division, then narrowed to float (:80-81)
        let avg_inc =
            ((block[block.len() - 1] - block[0]) as f64 / (block.len() - 1).max(1) as f64) as f32;

        let mut min = i64::MAX;
        let mut deltas = Vec::with_capacity(block.len());
        for (i, &v) in block.iter().enumerate() {
            // Java: (long)(avgInc * (long)i) — f32 multiply, truncate toward zero.
            // Wrapping arithmetic mirrors Java long overflow for huge values.
            let expected = (avg_inc * i as f32) as i64;
            let delta = (v as i64).wrapping_sub(expected);
            deltas.push(delta);
            min = min.min(delta);
        }
        let mut max_delta: u64 = 0;
        for d in &mut deltas {
            *d = d.wrapping_sub(min);
            max_delta |= *d as u64;
        }

        meta.write_long(min)?;
        meta.write_int(avg_inc.to_bits() as i32)?;
        meta.write_long((data.file_pointer() - base_data_pointer) as i64)?;
        if max_delta == 0 {
            meta.write_byte(0)?;
        } else {
            let bpv = direct_writer_unsigned_bits_required(max_delta);
            let u64_deltas: Vec<u64> = deltas.iter().map(|&d| d as u64).collect();
            let encoded = direct_writer_encode(&u64_deltas, bpv);
            data.write_bytes(&encoded)?;
            meta.write_byte(bpv as u8)?;
        }
    }
    Ok(())
}

// ============================================================================
// DirectReader — decode packed values written by DirectWriter
// ============================================================================

/// Reads packed values from a DirectWriter-encoded stream.
pub struct DirectReader {
    input: Box<dyn crate::io::IndexInput>,
    bpv: u32,
    value_count: usize,
    start_fp: u64,
}

impl DirectReader {
    pub fn new(
        input: Box<dyn crate::io::IndexInput>,
        bpv: u32,
        value_count: usize,
        start_fp: u64,
    ) -> Self {
        DirectReader { input, bpv, value_count, start_fp }
    }

    /// Read a single value at index. Bounds-checked.
    pub fn get(&mut self, index: usize) -> io::Result<u64> {
        if index >= self.value_count {
            return Ok(0); // out of bounds — return 0 (no value)
        }
        if self.bpv == 0 {
            return Ok(0); // constant: all values are 0
        }
        let byte_offset = (index * self.bpv as usize) / 8;
        let bit_offset = (index * self.bpv as usize) % 8;
        self.input.seek(self.start_fp + byte_offset as u64)?;

        // Read enough bytes to cover bpv bits starting at bit_offset
        let bytes_needed = ((bit_offset + self.bpv as usize) + 7) / 8;
        let mut buf = [0u8; 9]; // max 8 bytes + 1 for safety
        let n = self.input.read(&mut buf[..bytes_needed])?;
        if n < bytes_needed {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "DirectReader: short read",
            ));
        }
        let mut v: u64 = 0;
        for i in 0..bytes_needed {
            v |= (buf[i] as u64) << (i * 8);
        }
        let mask = if self.bpv == 64 {
            u64::MAX
        } else {
            (1u64 << self.bpv) - 1
        };
        Ok((v >> bit_offset) & mask)
    }

    /// Bulk read for collector. Reads values for all docs in the slice.
    pub fn get_batch(&mut self, docs: &[u32]) -> io::Result<Vec<Option<i64>>> {
        let mut result = Vec::with_capacity(docs.len());
        for &doc in docs {
            let val = self.get(doc as usize)?;
            result.push(Some(val as i64));
        }
        Ok(result)
    }
}

// ============================================================================
// DirectMonotonicReader — decode monotonic sequence
// ============================================================================

/// Reads a monotonic sequence of u64 values encoded by DirectMonotonicWriter.
pub struct DirectMonotonicReader {
    values: Vec<u64>,
}

impl DirectMonotonicReader {
    /// Decode from a DirectMonotonic-encoded stream.
    /// `input` is positioned at the start of the data.
    pub fn decode(
        mut input: Box<dyn crate::io::IndexInput>,
        value_count: usize,
        block_shift: u32,
    ) -> io::Result<Self> {
        if value_count == 0 {
            return Ok(DirectMonotonicReader { values: Vec::new() });
        }

        let block_size = 1usize << block_shift;
        let num_blocks = (value_count + block_size - 1) / block_size;

        // Read block min values and avg delta
        let mut min_values = Vec::with_capacity(num_blocks);
        let mut avg_incs = Vec::with_capacity(num_blocks);
        for _ in 0..num_blocks {
            min_values.push(input.read_vlong()? as u64);
        }
        for _ in 0..num_blocks {
            avg_incs.push(input.read_vlong()? as u64);
        }

        // BPV for delta offsets
        let bits_per_value = direct_writer_unsigned_bits_required(
            avg_incs.iter().copied().max().unwrap_or(0),
        );
        let offset_start = input.file_pointer();
        let mut dr = DirectReader::new(input, bits_per_value, value_count, offset_start);

        // Reconstruct values: expected[i] = min_block + avg_inc * index_in_block + offset[i]
        let mut values = Vec::with_capacity(value_count);
        for i in 0..value_count {
            let block = i >> block_shift;
            let in_block = (i - (block << block_shift)) as u64;
            let expected = min_values[block] + avg_incs[block] * in_block;
            let delta = dr.get(i)?;
            values.push(expected + delta);
        }
        Ok(DirectMonotonicReader { values })
    }

    pub fn get(&self, index: usize) -> u64 {
        if index < self.values.len() {
            self.values[index]
        } else {
            0
        }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::IndexOutput;

    /// Decoder mirroring DirectMonotonicReader.get (:161-167) for round-trip tests.
    struct MonotonicReader<'a> {
        mins: Vec<i64>,
        avgs: Vec<f32>,
        offsets: Vec<u64>,
        bpvs: Vec<u8>,
        data: &'a [u8],
        block_shift: u32,
    }

    impl<'a> MonotonicReader<'a> {
        fn parse(meta: &'a [u8], data: &'a [u8], num_values: usize, block_shift: u32) -> Self {
            let num_blocks = if num_values == 0 {
                0
            } else {
                (num_values - 1) >> block_shift
            } + 1;
            let mut mins = Vec::new();
            let mut avgs = Vec::new();
            let mut offsets = Vec::new();
            let mut bpvs = Vec::new();
            let mut pos = 0;
            for _ in 0..num_blocks {
                mins.push(i64::from_le_bytes(meta[pos..pos + 8].try_into().unwrap()));
                avgs.push(f32::from_bits(u32::from_le_bytes(
                    meta[pos + 8..pos + 12].try_into().unwrap(),
                )));
                offsets.push(u64::from_le_bytes(meta[pos + 12..pos + 20].try_into().unwrap()));
                bpvs.push(meta[pos + 20]);
                pos += 21;
            }
            MonotonicReader {
                mins,
                avgs,
                offsets,
                bpvs,
                data,
                block_shift,
            }
        }

        fn get(&self, index: usize) -> u64 {
            let block = index >> self.block_shift;
            let block_index = (index as u64) & ((1u64 << self.block_shift) - 1);
            let bpv = self.bpvs[block] as usize;
            let delta = if bpv == 0 {
                0
            } else {
                let bit_offset = self.offsets[block] as usize * 8 + block_index as usize * bpv;
                let byte_offset = bit_offset / 8;
                let shift = bit_offset % 8;
                let mut buf = [0u8; 8];
                let available = self.data.len() - byte_offset;
                let take = available.min(8);
                buf[..take].copy_from_slice(&self.data[byte_offset..byte_offset + take]);
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

    fn round_trip(values: &[u64], block_shift: u32) {
        let mut meta = ChecksumIndexOutput::new(IndexOutput::in_memory());
        let mut data = ChecksumIndexOutput::new(IndexOutput::in_memory());
        direct_monotonic_write(&mut meta, &mut data, values, block_shift).unwrap();
        let meta_bytes = meta.into_bytes();
        let data_bytes = data.into_bytes();
        let reader = MonotonicReader::parse(&meta_bytes, &data_bytes, values.len(), block_shift);
        for (i, &v) in values.iter().enumerate() {
            assert_eq!(reader.get(i), v, "value {i} of {values:?}");
        }
    }

    #[test]
    fn single_block_constant() {
        round_trip(&[7, 7, 7, 7], 2);
    }

    #[test]
    fn single_block_linear() {
        let values: Vec<u64> = (0..100).map(|i| 10 + 3 * i).collect();
        round_trip(&values, 4);
    }

    #[test]
    fn cross_block_jitter() {
        // 3 blocks (blockShift 4 => 16 values per block), non-linear values
        let values: Vec<u64> = (0..40u64).map(|i| i * i + 5).collect();
        round_trip(&values, 4);
    }

    #[test]
    fn large_values_bpv64() {
        // monotonic u64 values spanning the i64 sign boundary (wrapping
        // arithmetic, as Java longs do)
        let values: Vec<u64> = (0..8u64).map(|i| (i + 8) << 60).collect();
        round_trip(&values, 3);
    }

    #[test]
    fn block_shift_10_like_stored_fields() {
        // 1025 values => 2 blocks at blockShift 10
        let values: Vec<u64> = (0..1025u64).map(|i| i * 81920 + i % 7).collect();
        round_trip(&values, 10);
    }

    #[test]
    fn direct_writer_known_layouts() {
        // bpv 8: plain LE bytes
        assert_eq!(direct_writer_encode(&[1, 2, 255], 8), vec![1, 2, 255]);
        // bpv 1: 3 bits packed low-first, then truncated to 1 byte
        assert_eq!(direct_writer_encode(&[1, 0, 1], 1), vec![0b101]);
        // bpv 16: LE shorts, no padding
        assert_eq!(direct_writer_encode(&[0x0201], 16), vec![1, 2]);
        // bpv 12: two values merged into 3 bytes (l1 | l2<<12), +1 padding byte
        assert_eq!(
            direct_writer_encode(&[0xabc, 0xdef], 12),
            vec![0xbc, 0xfa, 0xde, 0x00]
        );
        // bpv 12 with 6 values: container ints overlap (numBytesFor2Values=3),
        // bytes are the continuous LSB-first bit stream + padding
        // (DirectWriter.encode :125-141). Regression test for the overlap bug.
        assert_eq!(
            direct_writer_encode(&[0, 0x16C, 0x38D, 0x521, 0x665, 0], 12),
            vec![0x00, 0xC0, 0x16, 0x8D, 0x13, 0x52, 0x65, 0x06, 0x00, 0x00]
        );
        // bpv 20: same overlap structure (numBytesFor2Values=5), 8-byte containers
        assert_eq!(
            direct_writer_encode(&[0x12345, 0xABCDE, 0x55], 20),
            vec![0x45, 0x23, 0xE1, 0xCD, 0xAB, 0x55, 0x00, 0x00, 0x00, 0x00]
        );
        // bpv 32: LE int, no padding
        assert_eq!(
            direct_writer_encode(&[0x04030201], 32),
            vec![1, 2, 3, 4]
        );
        // bpv 40: 5 LE bytes + 3 padding bytes
        assert_eq!(
            direct_writer_encode(&[0x0504030201], 40),
            vec![1, 2, 3, 4, 5, 0, 0, 0]
        );
    }

    #[test]
    fn bits_required_rounding() {
        assert_eq!(direct_writer_unsigned_bits_required(0), 1);
        assert_eq!(direct_writer_unsigned_bits_required(1), 1);
        assert_eq!(direct_writer_unsigned_bits_required(2), 2);
        assert_eq!(direct_writer_unsigned_bits_required(3), 2);
        assert_eq!(direct_writer_unsigned_bits_required(4), 4);
        assert_eq!(direct_writer_unsigned_bits_required(0xff), 8);
        assert_eq!(direct_writer_unsigned_bits_required(0x100), 12);
        assert_eq!(direct_writer_unsigned_bits_required(0xffff), 16);
        assert_eq!(direct_writer_unsigned_bits_required(0x1_0000), 20);
        assert_eq!(direct_writer_unsigned_bits_required(u64::MAX), 64);
    }
}

#[cfg(test)]
mod tests_read {
    use super::*;
    use crate::io::{HeapIndexInput, IndexOutput};

    #[test]
    fn test_direct_reader_round_trip() {
        let values: Vec<u64> = (0..1000u64).map(|i| i * 7 + 13).collect();
        let bpv = super::direct_writer_unsigned_bits_required(1000 * 7 + 13);
        let encoded = super::direct_writer_encode(&values, bpv);
        let input = Box::new(HeapIndexInput::new(encoded));
        let mut reader = DirectReader::new(input, bpv, values.len(), 0);
        for (i, &expected) in values.iter().enumerate() {
            assert_eq!(
                reader.get(i).unwrap(),
                expected,
                "DirectReader mismatch at index {}",
                i
            );
        }
    }

    #[test]
    fn test_direct_monotonic_round_trip() {
        // Each block consists of identical values; blocks increase stepwise.
        // avg_inc = 0 for every block, all deltas = 0, bpv = 1 on both sides.
        let block_shift = 4u32; // 16 values per block
        let block_size = 1usize << block_shift;
        let num_blocks = 32usize; // 32 full blocks = 512 values
        let values: Vec<u64> = (0..num_blocks)
            .flat_map(|blk| std::iter::repeat((blk + 1) as u64).take(block_size))
            .take(500) // partial last block
            .collect();

        let mut out = IndexOutput::in_memory();
        // Write min values
        for b in 0..((values.len() + block_size - 1) / block_size) {
            let start = b * block_size;
            out.write_vlong(values[start] as i64).unwrap();
        }
        // Write avg incs (all zero for constant-per-block)
        for b in 0..((values.len() + block_size - 1) / block_size) {
            let start = b * block_size;
            let end = values.len().min(start + block_size);
            let avg = if end > start + 1 {
                (values[end - 1] - values[start]) / (end - start - 1) as u64
            } else {
                0
            };
            out.write_vlong(avg as i64).unwrap();
        }
        // Write deltas (all zero)
        let mut deltas = Vec::with_capacity(values.len());
        for b in 0..((values.len() + block_size - 1) / block_size) {
            let start = b * block_size;
            let end = values.len().min(start + block_size);
            let min_val = values[start];
            let avg = if end > start + 1 {
                (values[end - 1] - values[start]) / (end - start - 1) as u64
            } else {
                0
            };
            for i in start..end {
                let in_block = (i - start) as u64;
                let expected = min_val + avg * in_block;
                deltas.push(values[i].wrapping_sub(expected));
            }
        }
        let max_delta = deltas.iter().copied().max().unwrap_or(0);
        let bpv = super::direct_writer_unsigned_bits_required(max_delta);
        let delta_bytes = super::direct_writer_encode(&deltas, bpv);
        out.write_bytes(&delta_bytes).unwrap();
        out.flush().unwrap();
        let encoded = out.into_bytes();

        let input = Box::new(HeapIndexInput::new(encoded));
        let reader = DirectMonotonicReader::decode(input, values.len(), block_shift).unwrap();
        assert_eq!(reader.len(), values.len());
        for (i, &expected) in values.iter().enumerate() {
            assert_eq!(
                reader.get(i),
                expected,
                "DirectMonotonic mismatch at index {}",
                i
            );
        }
    }
}

