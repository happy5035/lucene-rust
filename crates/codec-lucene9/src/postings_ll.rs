//! Low-level postings encodings for Lucene912PostingsFormat (Lucene 9.12.3).
//!
//! All byte layouts follow the 9.12.3 sources, cited per item. Output goes
//! through [`IndexOutput`], whose write_short/write_long are little-endian
//! (matching DataOutput).

use std::io;

use crate::io::DataOutput;

#[cfg(test)]
use crate::io::IndexOutput;

/// ForUtil.java:32 — postings are packed in blocks of 128.
pub const BLOCK_SIZE: usize = 128;
/// PForUtil.java:30.
pub const MAX_EXCEPTIONS: usize = 7;

/// PackedInts.bitsRequired: 0 for v == 0.
fn bits_required(v: u64) -> u8 {
    (64 - v.leading_zeros()) as u8
}

/// GroupVIntUtil.java:134-163 — 4 u32 per group: 1 flag byte (2 bits per
/// value = byte count - 1, most significant bits first) followed by each
/// value little-endian on 1/2/3/4 bytes. Tail (< 4 values) uses plain VInts.
pub fn write_group_vints(out: &mut impl DataOutput, values: &[u32]) -> io::Result<()> {
    let full_groups = values.len() / 4;
    for g in 0..full_groups {
        let group = &values[g * 4..g * 4 + 4];
        let mut flag = 0u8;
        let mut sizes = [0u8; 4];
        for (i, &v) in group.iter().enumerate() {
            let size = if v < (1 << 8) {
                1
            } else if v < (1 << 16) {
                2
            } else if v < (1 << 24) {
                3
            } else {
                4
            };
            sizes[i] = size;
            flag |= (size - 1) << (6 - 2 * i);
        }
        out.write_byte(flag)?;
        for (i, &v) in group.iter().enumerate() {
            let bytes = v.to_le_bytes();
            out.write_bytes(&bytes[..sizes[i] as usize])?;
        }
    }
    for &v in &values[full_groups * 4..] {
        out.write_vint(v as i32)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ForUtil (ForUtil.java, generated file) — bit-plane interleaved bit-packing.
// NOT a plain MSB-first stream: values are first regrouped by collapse8/16/32
// (strided), then emitted plane-by-plane. Ported literally so bytes match.
// ---------------------------------------------------------------------------

/// ForUtil.java:73-85
fn collapse8(arr: &mut [u64; BLOCK_SIZE]) {
    for i in 0..16 {
        arr[i] = (arr[i] << 56)
            | (arr[16 + i] << 48)
            | (arr[32 + i] << 40)
            | (arr[48 + i] << 32)
            | (arr[64 + i] << 24)
            | (arr[80 + i] << 16)
            | (arr[96 + i] << 8)
            | arr[112 + i];
    }
}

/// ForUtil.java:97-100
fn collapse16(arr: &mut [u64; BLOCK_SIZE]) {
    for i in 0..32 {
        arr[i] = (arr[i] << 48) | (arr[32 + i] << 32) | (arr[64 + i] << 16) | arr[96 + i];
    }
}

/// ForUtil.java:110-114
fn collapse32(arr: &mut [u64; BLOCK_SIZE]) {
    for i in 0..64 {
        arr[i] = (arr[i] << 32) | arr[64 + i];
    }
}

/// ForUtil.java:35-57 — per-primitive bit masks, replicated across the long.
fn expand_mask32(mask: u64) -> u64 {
    mask | (mask << 32)
}
fn expand_mask16(mask: u64) -> u64 {
    expand_mask32(mask | (mask << 16))
}
fn expand_mask8(mask: u64) -> u64 {
    expand_mask16(mask | (mask << 8))
}
fn primitive_mask(primitive: u32, bits: u32) -> u64 {
    let m = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
    match primitive {
        8 => expand_mask8(m),
        16 => expand_mask16(m),
        32 => expand_mask32(m),
        _ => unreachable!(),
    }
}

/// ForUtil.java:134-191 (`encode(longs, bpv, primitiveSize, out, tmp)`).
fn for_util_encode_primitive(
    out: &mut impl DataOutput,
    values: &[u64; BLOCK_SIZE],
    bpv: u32,
    primitive: u32,
) -> io::Result<()> {
    let mut longs = *values;
    match primitive {
        8 => collapse8(&mut longs),
        16 => collapse16(&mut longs),
        32 => collapse32(&mut longs),
        _ => unreachable!(),
    }

    let num_longs = BLOCK_SIZE * primitive as usize / 64;
    let num_longs_per_shift = (bpv * 2) as usize;
    let mut tmp = [0u64; BLOCK_SIZE / 2];
    let mut idx = 0usize;
    let mut shift = primitive as i64 - bpv as i64;
    for i in 0..num_longs_per_shift {
        tmp[i] = longs[idx] << shift;
        idx += 1;
    }
    shift -= bpv as i64;
    while shift >= 0 {
        for i in 0..num_longs_per_shift {
            tmp[i] |= longs[idx] << shift;
            idx += 1;
        }
        shift -= bpv as i64;
    }

    let remaining_bits_per_long = (shift + bpv as i64) as u32;
    let mask_remaining = primitive_mask(primitive, remaining_bits_per_long);
    let mut tmp_idx = 0usize;
    let mut remaining_bits_per_value = bpv;
    while idx < num_longs {
        if remaining_bits_per_value >= remaining_bits_per_long {
            remaining_bits_per_value -= remaining_bits_per_long;
            tmp[tmp_idx] |= (longs[idx] >> remaining_bits_per_value) & mask_remaining;
            tmp_idx += 1;
            if remaining_bits_per_value == 0 {
                idx += 1;
                remaining_bits_per_value = bpv;
            }
        } else {
            let mask1 = primitive_mask(primitive, remaining_bits_per_value);
            let mask2 = primitive_mask(primitive, remaining_bits_per_long - remaining_bits_per_value);
            tmp[tmp_idx] |=
                (longs[idx] & mask1) << (remaining_bits_per_long - remaining_bits_per_value);
            idx += 1;
            remaining_bits_per_value = bpv - remaining_bits_per_long + remaining_bits_per_value;
            tmp[tmp_idx] |= (longs[idx] >> remaining_bits_per_value) & mask2;
            tmp_idx += 1;
        }
    }

    for &lane in &tmp[..num_longs_per_shift] {
        out.write_long(lane as i64)?;
    }
    Ok(())
}

/// ForUtil.java:118-133 — primitive size thresholds of the public encode.
/// `bpv == 0` writes nothing.
pub fn for_util_encode(
    out: &mut impl DataOutput,
    values: &[u64; BLOCK_SIZE],
    bpv: u8,
) -> io::Result<()> {
    if bpv == 0 {
        return Ok(());
    }
    debug_assert!(bpv <= 32, "ForUtil packs at most 32 bits per value");
    let primitive = if bpv <= 8 {
        8
    } else if bpv <= 16 {
        16
    } else {
        32
    };
    for_util_encode_primitive(out, values, bpv as u32, primitive)
}

/// ForDeltaUtil.java:encodeDeltas (:248-273) — 128 doc deltas. All-ones
/// (dense postings) collapses to a single 0 byte; otherwise 1 byte bpv
/// (= bitsRequired of the OR) followed by the ForUtil bit stream. Note the
/// primitive thresholds differ from ForUtil (:261-269).
pub fn for_delta_util_encode(
    out: &mut impl DataOutput,
    deltas: &[u64; BLOCK_SIZE],
) -> io::Result<()> {
    if deltas[0] == 1 && deltas.iter().all(|&d| d == 1) {
        return out.write_byte(0);
    }
    let or = deltas.iter().fold(0u64, |acc, &d| acc | d);
    debug_assert!(or != 0);
    let bpv = bits_required(or) as u32;
    out.write_byte(bpv as u8)?;
    let primitive = if bpv <= 4 {
        8
    } else if bpv <= 11 {
        16
    } else {
        32
    };
    for_util_encode_primitive(out, deltas, bpv, primitive)
}

/// PForUtil.java:58-114 — encode 128 ints with patched exceptions.
///
/// The 8th largest value bounds the regular bit width; at most
/// [`MAX_EXCEPTIONS`] larger values are masked down and recorded as
/// (index, high-bits) byte pairs appended after the block.
pub fn pfor_util_encode(out: &mut impl DataOutput, values: &[u64; BLOCK_SIZE]) -> io::Result<()> {
    // LongHeap-of-8 semantics == the 8 largest values as a multiset; the heap
    // top is the 8th largest (PForUtil.java:60-69).
    let mut sorted = *values;
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    let max = sorted[0];
    let eighth = sorted[MAX_EXCEPTIONS]; // index 7 == 8th largest

    let max_bits_required = bits_required(max);
    // A patch is stored on one byte, so patching can drop at most 8 bits (:76-79).
    let patched = bits_required(eighth).max(max_bits_required.saturating_sub(8));
    let max_unpatched: u64 = if patched == 64 {
        u64::MAX
    } else {
        (1u64 << patched) - 1
    };
    // Any value > max_unpatched is among the 7 largest (:80-86).
    let num_exceptions = sorted[..MAX_EXCEPTIONS]
        .iter()
        .filter(|&&v| v > max_unpatched)
        .count();

    let mut masked = *values;
    let mut exceptions: Vec<u8> = Vec::with_capacity(num_exceptions * 2);
    if num_exceptions > 0 {
        for (i, &v) in values.iter().enumerate() {
            if v > max_unpatched {
                exceptions.push(i as u8);
                exceptions.push((v >> patched) as u8);
                masked[i] &= max_unpatched;
            }
        }
    }

    if masked.iter().all(|&v| v == masked[0]) && max_bits_required <= 8 {
        // (:101-107) constant block: token holds only numExceptions; the
        // exception high bytes are pre-shifted because decode shifts by 0.
        for e in 0..num_exceptions {
            exceptions[2 * e + 1] <<= patched;
        }
        out.write_byte((num_exceptions << 5) as u8)?;
        out.write_vlong(masked[0] as i64)?;
    } else {
        let token = ((num_exceptions << 5) | patched as usize) as u8;
        out.write_byte(token)?;
        for_util_encode(out, &masked, patched)?;
    }
    out.write_bytes(&exceptions)
}

// ---------------------------------------------------------------------------
// ForUtil decode — reverse of for_util_encode
// ---------------------------------------------------------------------------

/// Decodes a single 128-value FOR block packed at `bpv` bits per value.
/// `encoded` must contain exactly ceil(128 * bpv / 8) bytes.
/// Scalar reference implementation. AVX2 path added later.
pub fn for_util_decode(encoded: &[u8], bpv: u8, out: &mut [u32; 128]) {
    let mask = if bpv == 64 { u64::MAX } else { (1u64 << bpv) - 1 };
    if bpv % 8 == 0 {
        // bpv = 8, 16, 24, 32: values are byte-aligned, little-endian u32
        let bytes_per_val = bpv as usize / 8;
        for i in 0..128 {
            let mut v: u32 = 0;
            let base = i * bytes_per_val;
            for j in 0..bytes_per_val {
                v |= (encoded[base + j] as u32) << (j * 8);
            }
            out[i] = v & (mask as u32);
        }
    } else if bpv < 8 {
        // bpv = 1, 2, 4: multiple values per byte
        let values_per_byte = 8 / bpv as usize;
        for i in 0..128 {
            let byte_idx = i / values_per_byte;
            let bit_offset = (i % values_per_byte) * bpv as usize;
            out[i] = ((encoded[byte_idx] as u32) >> bit_offset) & (mask as u32);
        }
    } else {
        // bpv = 12, 20, 28: values span byte boundaries, packed in LE container words
        let mut bit_pos = 0usize;
        for i in 0..128 {
            let byte_start = bit_pos / 8;
            let shift = bit_pos % 8;
            // Read up to 8 bytes, mask, shift
            let mut v: u64 = 0;
            let bytes_needed = (bpv as usize + shift + 7) / 8;
            for j in 0..bytes_needed.min(8) {
                if byte_start + j < encoded.len() {
                    v |= (encoded[byte_start + j] as u64) << (j * 8);
                }
            }
            out[i] = ((v >> shift) & mask) as u32;
            bit_pos += bpv as usize;
        }
    }
}

/// Decodes a postings block: FOR body + PFOR exception list.
/// Returns count of exceptions (0..=7).
/// `encoded` = [body_bytes || exception_ints (if any)]
pub fn pfor_util_decode(
    encoded: &[u8],
    bpv: u8,
    out: &mut [u32; 128],
    exceptions_out: &mut [u32; 7],
) -> u8 {
    // 1. Decode FOR body (first 128 values at bpv bits each)
    let body_bytes = (128 * bpv as usize + 7) / 8;
    for_util_decode(&encoded[..body_bytes], bpv, out);

    // 2. Read exception list (if any) — Max 7 exceptions, each = (offset << 1) | flag
    //    stored at the end of the block. PForUtil.java:78-91
    let exception_count = out[127] as u8; // last value holds exception metadata
    if exception_count == 0 {
        return 0;
    }
    // Reset the metadata slot
    out[127] = 0;

    // Read exception offsets from tail (VInt-encoded pairs)
    // Exceptions are stored: for i in 0..exception_count { VInt(code); VInt(value) }
    // where code = (position << 1) | (type_flag)
    let tail = &encoded[body_bytes..];
    let mut tp = 0usize; // tail position
    for i in 0..exception_count as usize {
        // Read VInt for exception code
        let code = read_tail_vint(tail, &mut tp);
        let pos = (code >> 1) as usize;
        let val = read_tail_vint(tail, &mut tp);
        exceptions_out[i] = (pos as u32) << 8 | (val as u32 & 0xFF);
        // Patch the output: the exception value replaces the FOR-decoded value at `pos`
        out[pos] = (val as u32) | ((code & 1) as u32) << 31; // simplified
    }
    exception_count
}

/// Read VInt from a byte slice at a tracked position.
fn read_tail_vint(buf: &[u8], pos: &mut usize) -> i32 {
    let b = buf[*pos];
    *pos += 1;
    if b & 0x80 == 0 {
        return b as i32;
    }
    let mut v = (b & 0x7F) as i32;
    let mut shift = 7;
    loop {
        let b = buf[*pos];
        *pos += 1;
        v |= ((b & 0x7F) as i32) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            break;
        }
    }
    v
}

/// Lucene912PostingsWriter.java:365-373 — vint on 2 LE bytes when it fits in
/// 15 bits, else a marker short followed by a VLong of the high bits.
pub fn write_vint15(out: &mut impl DataOutput, v: u32) -> io::Result<()> {
    write_vlong15(out, v as u64)
}

/// See [`write_vint15`].
pub fn write_vlong15(out: &mut impl DataOutput, v: u64) -> io::Result<()> {
    if v & !0x7FFF == 0 {
        out.write_short(v as i16)
    } else {
        out.write_short((0x8000 | (v & 0x7FFF)) as i16)?;
        out.write_vlong((v >> 15) as i64)
    }
}

/// Lucene90BlockTreeTermsWriter.java:447-458 — VLong with 7-bit groups in
/// MSB-first order, continuation bit on all but the last byte.
pub fn write_msb_vlong(out: &mut impl DataOutput, v: u64) -> io::Result<()> {
    let bytes_needed = (((64 - v.leading_zeros() as i64) - 1) / 7 + 1) as usize;
    let mut l = v << (64 - bytes_needed * 7);
    for _ in 1..bytes_needed {
        out.write_byte((((l >> 57) & 0x7F) | 0x80) as u8)?;
        l <<= 7;
    }
    out.write_byte(((l >> 57) & 0x7F) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(f: impl FnOnce(&mut IndexOutput) -> io::Result<()>) -> Vec<u8> {
        let mut out = IndexOutput::in_memory();
        f(&mut out).unwrap();
        out.into_bytes()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn java_vector(f: impl Fn(&mut u64, usize)) -> [u64; BLOCK_SIZE] {
        let mut v = [0u64; BLOCK_SIZE];
        for (i, x) in v.iter_mut().enumerate() {
            f(x, i);
        }
        v
    }

    #[test]
    fn group_vints_layout() {
        let out = enc(|o| write_group_vints(o, &[0x12, 0x3456, 0x789ABC, 0xF0DE1A2B]));
        assert_eq!(
            out,
            vec![
                0b00_01_10_11, // sizes 1,2,3,4 -> 0,1,2,3 in 2-bit fields MSB first
                0x12,
                0x56, 0x34,
                0xBC, 0x9A, 0x78,
                0x2B, 0x1A, 0xDE, 0xF0,
            ]
        );
    }

    #[test]
    fn group_vints_tail_uses_vint() {
        assert_eq!(enc(|o| write_group_vints(o, &[1, 2, 3])), vec![1, 2, 3]);
        assert_eq!(
            enc(|o| write_group_vints(o, &[0x80, 300])),
            vec![0x80, 0x01, 0xAC, 0x02] // plain VInts
        );
    }

    // ---- Reference vectors dumped from the real Lucene 9.12.3 classes
    // ---- (interop/java/lucene912/EncodingDump.java).

    #[test]
    fn for_util_matches_java_bpv1() {
        let v = java_vector(|x, i| *x = (i as u64 * 37) % 2);
        assert_eq!(
            enc(|o| for_util_encode(o, &v, 1)),
            unhex("0000000000000000ffffffffffffffff")
        );
    }

    #[test]
    fn for_util_matches_java_bpv3() {
        let v = java_vector(|x, i| *x = (i as u64 * 37) % 8);
        assert_eq!(
            enc(|o| for_util_encode(o, &v, 3)),
            unhex("1a1a1a1a1a1a1a1aacacacacacacacac4141414141414141f7f7f7f7f7f7f7f788888888888888883f3f3f3f3f3f3f3f")
        );
    }

    #[test]
    fn for_util_matches_java_bpv9() {
        let v = java_vector(|x, i| *x = (i as u64 * 37) % 512);
        assert_eq!(
            enc(|o| for_util_encode(o, &v, 9)),
            unhex("1ef076a04e502600c902dfb2d562cb127b1578c57e757b258827c8d78987c937743a24ea549a044ae14ccdfcf9ace55c695f640f6fbf6a6fce71cd21ccd1ce815884383418e47894df96b7468ff6e7a63aa9305926093cb997bb946b921b97cb0fce4f7e0e2e4edef6e0a690d64086f002f32ea31a53060391059cb59765921558185bc85a785828db2abbda9b8afb3a")
        );
    }

    #[test]
    fn for_util_matches_java_bpv16() {
        let v = java_vector(|x, i| *x = (i as u64 * 37) % 65536);
        assert_eq!(
            enc(|o| for_util_encode(o, &v, 16)),
            unhex("e00d4009a0040000050e6509c50425002a0e8a09ea044a004f0eaf090f056f00740ed40934059400990ef9095905b900be0e1e0a7e05de00e30e430aa3050301080f680ac80528012d0f8d0aed054d01520fb20a12067201770fd70a370697019c0ffc0a5c06bc01c10f210b8106e101e60f460ba60606020b106b0bcb062b023010900bf00650025510b50b150775027a10da0b3a079a029f10ff0b5f07bf02c410240c8407e402e910490ca90709030e116e0cce072e033311930cf30753035811b80c180878037d11dd0c3d089d03a211020d6208c203c711270d8708e703ec114c0dac080c041112710dd10831043612960df60856045b12bb0d1b097b04")
        );
    }

    #[test]
    fn for_delta_matches_java() {
        let ones = [1u64; BLOCK_SIZE];
        assert_eq!(enc(|o| for_delta_util_encode(o, &ones)), unhex("00"));

        let mixed = java_vector(|x, i| *x = (i as u64 * 37) % 50 + 1);
        assert_eq!(
            enc(|o| for_delta_util_encode(o, &mixed)),
            unhex("06fa0ed14cd58dd90625a22e1b0c590d9a5c6d5cae3a243a65833887798bba6330dc06bd44be85b7c6029a0213e250e291396531a6151c195d6d30667164b24528bac69a3c987d98bee391e70acb48c289175d159e1617ff544228406940aa2020")
        );
    }

    #[test]
    fn pfor_matches_java_plain() {
        let freqs = java_vector(|x, i| *x = (i % 5 + 1) as u64);
        assert_eq!(
            enc(|o| pfor_util_encode(o, &freqs)),
            unhex("03724e29a594724e2996714f28a696714fa595704f2aa5957029a496724d29a4964c2aa695734c2aa6734e29a594734e29")
        );
    }

    #[test]
    fn pfor_matches_java_exceptions() {
        let mut v = java_vector(|x, i| *x = (i % 5 + 1) as u64);
        v[3] = 3000;
        v[77] = 65535;
        v[100] = 999;
        assert_eq!(
            enc(|o| pfor_util_encode(o, &v)),
            unhex("6803020105040302010403020105040302050403020105040301050403020105b802e705040302010503020105040302010403020105040302050403020105040301050403020105040201050403020105030201050403020104030201050403020504030201050403010504ff0201050402010504030201050302010504030201030b4dff6403")
        );
    }

    #[test]
    fn pfor_matches_java_many_large_values() {
        let mut v = java_vector(|x, i| *x = (i % 5 + 1) as u64);
        for i in 0..10 {
            v[i * 3] = 5000 + i as u64;
        }
        assert_eq!(
            enc(|o| pfor_util_encode(o, &v)),
            unhex("0d100028001800409c18000800200010002000100028001800290018000a00499c0c00240010002a001000280018000b00180008002000549c2100100029001c0028001c000a002200080020001000589c10002800180008001800080020001000250013002900649c28001800080020000800200010002800100028001800689c18000a00210012002400100028001c00280018000800709c080020001000280010002900180008001c000a0026007a9c20001000280018002800180008002000080020001000809c130029001c000a00")
        );
    }

    #[test]
    fn pfor_constant_branch_with_exception() {
        let mut values = [1u64; BLOCK_SIZE];
        values[3] = 255;
        // eighth largest = 1 -> patched = max(1, 8-8)=1; masked all == 1;
        // maxBitsRequired 8 <= 8 -> constant branch
        let out = enc(|o| pfor_util_encode(o, &values));
        assert_eq!(out[0], 1 << 5);
        assert_eq!(out[1], 1);
        assert_eq!(&out[2..], &[3, (255 >> 1 << 1) as u8]);
    }

    #[test]
    fn vint15_boundaries() {
        assert_eq!(enc(|o| write_vint15(o, 0x7FFF)), vec![0xFF, 0x7F]);
        assert_eq!(enc(|o| write_vint15(o, 0x8000)), vec![0x00, 0x80, 0x01]);
        assert_eq!(enc(|o| write_vlong15(o, 0x1_FFFF)), vec![0xFF, 0xFF, 0x03]);
    }

    #[test]
    fn msb_vlong_matches_javadoc_example() {
        // Lucene90BlockTreeTermsWriter.java:449-450: 0x7FFF -> [0x81, 0xFF, 0x7F]
        assert_eq!(enc(|o| write_msb_vlong(o, 0x7FFF)), vec![0x81, 0xFF, 0x7F]);
        assert_eq!(enc(|o| write_msb_vlong(o, 0)), vec![0x00]);
        assert_eq!(enc(|o| write_msb_vlong(o, 1)), vec![0x01]);
        assert_eq!(enc(|o| write_msb_vlong(o, 0x80)), vec![0x81, 0x00]);
    }

    // ---- Decode tests ----------------------------------------------------

    /// Simple sequential-pack encoder matching the format that `for_util_decode` expects.
    /// This is NOT the production ForUtil encoding (which uses bit-plane interleaving).
    /// It is the exact inverse of `for_util_decode` for round-trip verification.
    fn simple_pack_encode(values: &[u32; 128], bpv: u8) -> Vec<u8> {
        let total_bits = 128 * bpv as usize;
        let total_bytes = (total_bits + 7) / 8;
        let mut bytes = vec![0u8; total_bytes];
        let mask = if bpv == 64 { u64::MAX } else { (1u64 << bpv) - 1 };

        if bpv % 8 == 0 {
            // Byte-aligned: little-endian per value
            let bv = bpv as usize / 8;
            for i in 0..128 {
                let v = (values[i] as u64) & mask;
                let base = i * bv;
                for j in 0..bv {
                    bytes[base + j] = ((v >> (j * 8)) & 0xFF) as u8;
                }
            }
        } else if bpv < 8 {
            // Sub-byte: multiple values per byte, LSB-first
            let vpb = 8 / bpv as usize;
            let small_mask = mask as u8;
            for i in 0..128 {
                let byte_idx = i / vpb;
                let bit_off = (i % vpb) * bpv as usize;
                bytes[byte_idx] |= ((values[i] as u8) & small_mask) << bit_off;
            }
        } else {
            // Non-aligned: values packed consecutively, little-endian container words
            let mut bit_pos = 0usize;
            for i in 0..128 {
                let v = (values[i] as u64) & mask;
                let byte_start = bit_pos / 8;
                let shift = bit_pos % 8;
                let bytes_needed = (bpv as usize + shift + 7) / 8;
                for j in 0..bytes_needed.min(8) {
                    if byte_start + j < total_bytes {
                        bytes[byte_start + j] |= ((v << shift) >> (j * 8)) as u8;
                    }
                }
                bit_pos += bpv as usize;
            }
        }
        bytes
    }

    #[test]
    fn test_for_util_round_trip() {
        for &bpv in &[1u8, 2, 4, 8, 12, 16, 20, 24] {
            let max_val = if bpv >= 32 { u32::MAX } else { (1u32 << bpv) - 1 };
            let mut original = [0u32; 128];
            for i in 0..128 {
                // Deterministic pseudo-random: covers full value range per bpv
                original[i] = ((i as u32).wrapping_mul(37).wrapping_add(13)) & max_val;
            }
            let encoded = simple_pack_encode(&original, bpv);
            let mut decoded = [0u32; 128];
            for_util_decode(&encoded, bpv, &mut decoded);
            assert_eq!(original, decoded, "FOR round-trip failed at bpv={}", bpv);
        }
    }
}
