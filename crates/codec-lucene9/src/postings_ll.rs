//! Low-level postings encodings for Lucene912PostingsFormat (Lucene 9.12.3).
//!
//! All byte layouts follow the 9.12.3 sources, cited per item. Output goes
//! through [`IndexOutput`], whose write_short/write_long are little-endian
//! (matching DataOutput).

use std::io;

use crate::io::{DataInput, DataOutput};

/// AVX2 fast path for the ForUtil-family block decode (spec §4a) — see the
/// module docs for the equivalence and safety arguments. x86_64-only; every
/// other target takes the scalar reference path below.
#[cfg(target_arch = "x86_64")]
mod simd;

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

// ---------------------------------------------------------------------------
// Read side — exact mirrors of the encoders above.
// ---------------------------------------------------------------------------

/// GroupVIntUtil.readGroupVInt (:42-54) + DataInput.readGroupVInts (:110-118):
/// full groups of 4 via the flag byte (2 bits per value = byte count - 1,
/// most significant bits first), tail as plain VInts. Mirrors
/// [`write_group_vints`].
pub fn read_group_vints(input: &mut impl DataInput, values: &mut [u32]) -> io::Result<()> {
    let full_groups = values.len() / 4;
    for g in 0..full_groups {
        let flag = input.read_byte()?;
        for (i, v) in values[g * 4..g * 4 + 4].iter_mut().enumerate() {
            let size = ((flag >> (6 - 2 * i)) & 0x3) + 1;
            let mut value = 0u32;
            for j in 0..size {
                value |= (input.read_byte()? as u32) << (8 * j);
            }
            *v = value;
        }
    }
    for v in &mut values[full_groups * 4..] {
        *v = input.read_vint()? as u32;
    }
    Ok(())
}

/// ForUtil.expand8 (:59-71): inverse of [`collapse8`].
fn expand8(collapsed: &[u64; BLOCK_SIZE], values: &mut [u64; BLOCK_SIZE]) {
    for i in 0..16 {
        let l = collapsed[i];
        for j in 0..8 {
            values[16 * j + i] = (l >> (56 - 8 * j)) & 0xFF;
        }
    }
}

/// ForUtil.expand16 (:87-95): inverse of [`collapse16`].
fn expand16(collapsed: &[u64; BLOCK_SIZE], values: &mut [u64; BLOCK_SIZE]) {
    for i in 0..32 {
        let l = collapsed[i];
        for j in 0..4 {
            values[32 * j + i] = (l >> (48 - 16 * j)) & 0xFFFF;
        }
    }
}

/// ForUtil.expand32 (:103-109): inverse of [`collapse32`].
fn expand32(collapsed: &[u64; BLOCK_SIZE], values: &mut [u64; BLOCK_SIZE]) {
    for i in 0..64 {
        values[i] = collapsed[i] >> 32;
        values[64 + i] = collapsed[i] & 0xFFFF_FFFF;
    }
}

/// ForUtil.decode (:291-394) via a single generic routine covering every
/// (bpv, primitive) pair — the exact inverse of [`for_util_encode_primitive`]
/// (Java's per-bpv specializations are unrolled forms of the same layout;
/// decodeSlow :199-223 proves a generic decoder exists).
fn for_util_decode_primitive(
    input: &mut impl DataInput,
    values: &mut [u64; BLOCK_SIZE],
    bpv: u32,
    primitive: u32,
) -> io::Result<()> {
    debug_assert!(bpv >= 1 && bpv <= 32);
    #[cfg(target_arch = "x86_64")]
    {
        // AVX2 fast path (spec §4a): bit-for-bit equivalent; returns false
        // without touching the input when the CPU lacks AVX2.
        if simd::try_decode(input, values, bpv, primitive)? {
            return Ok(());
        }
    }
    let num_longs = BLOCK_SIZE * primitive as usize / 64;
    let num_longs_per_shift = (bpv * 2) as usize;
    let mut tmp = [0u64; BLOCK_SIZE / 2];
    for lane in tmp.iter_mut().take(num_longs_per_shift) {
        *lane = input.read_long()? as u64;
    }

    let mut longs = [0u64; BLOCK_SIZE];
    let mut idx = 0usize;
    let value_mask = primitive_mask(primitive, bpv);
    // Whole bpv planes (inverse of the encode shift loops, ForUtil.java:142-149).
    let mut shift = primitive as i64 - bpv as i64;
    loop {
        for i in 0..num_longs_per_shift {
            longs[idx] = (tmp[i] >> shift) & value_mask;
            idx += 1;
        }
        shift -= bpv as i64;
        if shift < 0 {
            break;
        }
    }
    // Remainder planes, split across tmp slots (inverse of ForUtil.java:151-187).
    let remaining_bits_per_long = (shift + bpv as i64) as u32;
    if idx < num_longs {
        let mask_remaining = primitive_mask(primitive, remaining_bits_per_long);
        let mut tmp_idx = 0usize;
        let mut remaining_bits_per_value = bpv;
        while idx < num_longs {
            if remaining_bits_per_value >= remaining_bits_per_long {
                remaining_bits_per_value -= remaining_bits_per_long;
                longs[idx] |= (tmp[tmp_idx] & mask_remaining) << remaining_bits_per_value;
                tmp_idx += 1;
                if remaining_bits_per_value == 0 {
                    idx += 1;
                    remaining_bits_per_value = bpv;
                }
            } else {
                let mask1 = primitive_mask(primitive, remaining_bits_per_value);
                let mask2 =
                    primitive_mask(primitive, remaining_bits_per_long - remaining_bits_per_value);
                longs[idx] |=
                    (tmp[tmp_idx] >> (remaining_bits_per_long - remaining_bits_per_value)) & mask1;
                idx += 1;
                remaining_bits_per_value =
                    bpv - remaining_bits_per_long + remaining_bits_per_value;
                longs[idx] |= (tmp[tmp_idx] & mask2) << remaining_bits_per_value;
                tmp_idx += 1;
            }
        }
    }

    match primitive {
        8 => expand8(&longs, values),
        16 => expand16(&longs, values),
        32 => expand32(&longs, values),
        _ => unreachable!(),
    }
    Ok(())
}

/// ForUtil.decode public entry (:291-394). `bpv == 0` encodes nothing
/// (mirrors [`for_util_encode`]); primitive thresholds :118-133.
pub fn for_util_decode(
    input: &mut impl DataInput,
    values: &mut [u64; BLOCK_SIZE],
    bpv: u8,
) -> io::Result<()> {
    if bpv == 0 {
        values.fill(0);
        return Ok(());
    }
    if bpv > 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ForUtil packs at most 32 bits per value, got {bpv}"),
        ));
    }
    let primitive = if bpv <= 8 { 8 } else if bpv <= 16 { 16 } else { 32 };
    for_util_decode_primitive(input, values, bpv as u32, primitive)
}

/// ForDeltaUtil.decodeDeltas (:276-283 without the prefix-sum step): a 0
/// byte means all-ones (dense postings); otherwise 1 byte bpv followed by
/// the ForUtil bit stream. Note the primitive thresholds (:261-269) differ
/// from ForUtil's. The caller applies the prefix sum
/// (Lucene912PostingsReader.prefixSum :208-213).
pub fn for_delta_util_decode(
    input: &mut impl DataInput,
    deltas: &mut [u64; BLOCK_SIZE],
) -> io::Result<()> {
    let bpv = input.read_byte()?;
    if bpv == 0 {
        deltas.fill(1);
        return Ok(());
    }
    if bpv > 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ForDeltaUtil packs at most 32 bits per value, got {bpv}"),
        ));
    }
    let primitive = if bpv <= 4 { 8 } else if bpv <= 11 { 16 } else { 32 };
    for_util_decode_primitive(input, deltas, bpv as u32, primitive)
}

/// PForUtil.decode (:117-130): token byte = numExceptions<<5 | bitsPerValue;
/// bpv 0 = constant block (one VLong); then (position, high-byte) exception
/// pairs appended after the packed data. Mirrors [`pfor_util_encode`].
pub fn pfor_util_decode(
    input: &mut impl DataInput,
    values: &mut [u64; BLOCK_SIZE],
) -> io::Result<()> {
    let token = input.read_byte()?;
    let bits_per_value = token & 0x1f;
    let num_exceptions = token >> 5;
    if bits_per_value == 0 {
        let v = input.read_vlong()? as u64;
        values.fill(v);
    } else {
        for_util_decode(input, values, bits_per_value)?;
    }
    for _ in 0..num_exceptions {
        let pos = input.read_byte()? as usize;
        let high = input.read_byte()? as u64;
        values[pos] |= high << bits_per_value;
    }
    Ok(())
}

/// PForUtil block skip: consumes exactly the bytes [`pfor_util_decode`]
/// would read — token, then the packed data (bpv*2 longs = bpv*16 bytes) or
/// the constant-block VLong, then the (position, high-byte) exception pairs —
/// without decoding anything. The token's 5 bpv bits cap bpv at 31, so the
/// bpv > 32 guard of the decode path cannot fire here.
pub fn pfor_util_skip(input: &mut impl DataInput) -> io::Result<()> {
    let token = input.read_byte()?;
    let bits_per_value = token & 0x1f;
    let num_exceptions = token >> 5;
    if bits_per_value == 0 {
        let _ = input.read_vlong()?;
    } else {
        input.skip_bytes(bits_per_value as u64 * 16)?;
    }
    input.skip_bytes(num_exceptions as u64 * 2)?;
    Ok(())
}

/// Lucene912PostingsReader.readVInt15 (:2043-2047): LE short; when the top
/// bit is set, a VInt carries the high bits. Mirrors [`write_vint15`].
pub fn read_vint15(input: &mut impl DataInput) -> io::Result<u32> {
    let s = input.read_short()?;
    if s >= 0 {
        Ok(s as u32)
    } else {
        Ok(((s as u16 & 0x7FFF) as u32) | ((input.read_vint()? as u32) << 15))
    }
}

/// Lucene912PostingsReader.readVLong15 (:2055-2062). Mirrors [`write_vlong15`].
pub fn read_vlong15(input: &mut impl DataInput) -> io::Result<u64> {
    let s = input.read_short()?;
    if s >= 0 {
        Ok(s as u64)
    } else {
        Ok(((s as u16 & 0x7FFF) as u64) | ((input.read_vlong()? as u64) << 15))
    }
}

/// FieldReader.readMSBVLong (FieldReader.java:126-136): 7-bit groups in
/// MSB-first order, continuation bit on all but the last byte. Mirrors
/// [`write_msb_vlong`].
pub fn read_msb_vlong(input: &mut impl DataInput) -> io::Result<u64> {
    let mut l = 0u64;
    loop {
        let b = input.read_byte()?;
        l = (l << 7) | ((b & 0x7f) as u64);
        if b & 0x80 == 0 {
            return Ok(l);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::IndexInput;

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

    fn dec<T>(bytes: &[u8], f: impl FnOnce(&mut IndexInput) -> io::Result<T>) -> T {
        f(&mut IndexInput::in_memory(bytes.to_vec())).unwrap()
    }

    #[test]
    fn group_vints_decode_round_trip() {
        // full group + tail (len 6) and the exact layout vector of the write side
        let values = [0x12, 0x3456, 0x789ABC, 0xF0DE1A2B, 1, 300];
        let bytes = enc(|o| write_group_vints(o, &values));
        let mut back = [0u32; 6];
        dec(&bytes, |i| read_group_vints(i, &mut back));
        assert_eq!(back, values);
    }

    #[test]
    fn for_util_decode_round_trip_all_bpv() {
        for bpv in [1u8, 2, 3, 4, 5, 7, 8, 9, 11, 12, 16, 17, 24, 25, 31, 32] {
            let mask = if bpv == 64 { u64::MAX } else { (1u64 << bpv) - 1 };
            let v = java_vector(|x, i| *x = (i as u64 * 37 + 5) & mask);
            let bytes = enc(|o| for_util_encode(o, &v, bpv));
            let mut back = [0u64; BLOCK_SIZE];
            dec(&bytes, |i| for_util_decode(i, &mut back, bpv));
            assert_eq!(back, v, "bpv {bpv}");
        }
    }

    #[test]
    fn for_util_decode_matches_java_vectors() {
        // decode the reference vectors dumped from real Lucene (see encode tests)
        let v9 = java_vector(|x, i| *x = (i as u64 * 37) % 512);
        let bytes = unhex("1ef076a04e502600c902dfb2d562cb127b1578c57e757b258827c8d78987c937743a24ea549a044ae14ccdfcf9ace55c695f640f6fbf6a6fce71cd21ccd1ce815884383418e47894df96b7468ff6e7a63aa9305926093cb997bb946b921b97cb0fce4f7e0e2e4edef6e0a690d64086f002f32ea31a53060391059cb59765921558185bc85a785828db2abbda9b8afb3a");
        let mut back = [0u64; BLOCK_SIZE];
        dec(&bytes, |i| for_util_decode(i, &mut back, 9));
        assert_eq!(back, v9);
        let v16 = java_vector(|x, i| *x = (i as u64 * 37) % 65536);
        let bytes = unhex("e00d4009a0040000050e6509c50425002a0e8a09ea044a004f0eaf090f056f00740ed40934059400990ef9095905b900be0e1e0a7e05de00e30e430aa3050301080f680ac80528012d0f8d0aed054d01520fb20a12067201770fd70a370697019c0ffc0a5c06bc01c10f210b8106e101e60f460ba60606020b106b0bcb062b023010900bf00650025510b50b150775027a10da0b3a079a029f10ff0b5f07bf02c410240c8407e402e910490ca90709030e116e0cce072e033311930cf30753035811b80c180878037d11dd0c3d089d03a211020d6208c203c711270d8708e703ec114c0dac080c041112710dd10831043612960df60856045b12bb0d1b097b04");
        let mut back = [0u64; BLOCK_SIZE];
        dec(&bytes, |i| for_util_decode(i, &mut back, 16));
        assert_eq!(back, v16);
    }

    #[test]
    fn for_delta_decode_all_ones_and_mixed() {
        let mut back = [0u64; BLOCK_SIZE];
        // all-ones collapses to a single 0 byte
        dec(&[0x00], |i| for_delta_util_decode(i, &mut back));
        assert_eq!(back, [1u64; BLOCK_SIZE]);
        // mixed deltas (Java reference vector from the encode test)
        let mixed = java_vector(|x, i| *x = (i as u64 * 37) % 50 + 1);
        let bytes = unhex("06fa0ed14cd58dd90625a22e1b0c590d9a5c6d5cae3a243a65833887798bba6330dc06bd44be85b7c6029a0213e250e291396531a6151c195d6d30667164b24528bac69a3c987d98bee391e70acb48c289175d159e1617ff544228406940aa2020");
        dec(&bytes, |i| for_delta_util_decode(i, &mut back));
        assert_eq!(back, mixed);
    }

    #[test]
    fn pfor_decode_matches_java_vectors() {
        let mut back = [0u64; BLOCK_SIZE];
        // plain
        let freqs = java_vector(|x, i| *x = (i % 5 + 1) as u64);
        let bytes = unhex("03724e29a594724e2996714f28a696714fa595704f2aa5957029a496724d29a4964c2aa695734c2aa6734e29a594734e29");
        dec(&bytes, |i| pfor_util_decode(i, &mut back));
        assert_eq!(back, freqs);
        // exceptions at 3, 77, 100
        let mut v = freqs;
        v[3] = 3000;
        v[77] = 65535;
        v[100] = 999;
        let bytes = unhex("6803020105040302010403020105040302050403020105040301050403020105b802e705040302010503020105040302010403020105040302050403020105040301050403020105040201050403020105030201050403020104030201050403020504030201050403010504ff0201050402010504030201050302010504030201030b4dff6403");
        dec(&bytes, |i| pfor_util_decode(i, &mut back));
        assert_eq!(back, v);
        // constant branch with one exception
        let mut values = [1u64; BLOCK_SIZE];
        values[3] = 255;
        let bytes = enc(|o| pfor_util_encode(o, &values));
        dec(&bytes, |i| pfor_util_decode(i, &mut back));
        assert_eq!(back, values);
    }

    #[test]
    fn pfor_skip_consumes_exact_block_bytes() {
        // plain / exceptions / constant-with-exception / pure-constant: the
        // skip walk must land exactly at the end of each encoded block.
        let mut with_exceptions = java_vector(|x, i| *x = (i % 5 + 1) as u64);
        with_exceptions[3] = 3000;
        with_exceptions[77] = 65535;
        with_exceptions[100] = 999;
        let mut constant_with_exception = [1u64; BLOCK_SIZE];
        constant_with_exception[3] = 255;
        let cases = [
            enc(|o| pfor_util_encode(o, &java_vector(|x, i| *x = (i % 5 + 1) as u64))),
            enc(|o| pfor_util_encode(o, &with_exceptions)),
            enc(|o| pfor_util_encode(o, &constant_with_exception)),
            enc(|o| pfor_util_encode(o, &[1u64; BLOCK_SIZE])),
        ];
        for bytes in cases {
            let len = bytes.len() as u64;
            let mut input = IndexInput::in_memory(bytes);
            pfor_util_skip(&mut input).unwrap();
            assert_eq!(input.file_pointer(), len);
        }
    }

    #[test]
    fn vint15_vlong15_round_trip() {
        for v in [0u32, 1, 0x7FFF, 0x8000, 0x1_FFFF, u32::MAX] {
            let bytes = enc(|o| write_vint15(o, v));
            assert_eq!(dec(&bytes, read_vint15), v, "vint15 {v}");
        }
        for v in [0u64, 0x7FFF, 0x8000, 1 << 40] {
            let bytes = enc(|o| write_vlong15(o, v));
            assert_eq!(dec(&bytes, read_vlong15), v, "vlong15 {v}");
        }
    }

    #[test]
    fn msb_vlong_round_trip() {
        for v in [0u64, 1, 0x80, 0x7FFF, 1 << 35, u64::MAX >> 1] {
            let bytes = enc(|o| write_msb_vlong(o, v));
            assert_eq!(dec(&bytes, read_msb_vlong), v, "msb_vlong {v}");
        }
    }
}
