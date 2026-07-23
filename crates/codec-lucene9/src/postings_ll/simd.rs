//! AVX2 fast path for the ForUtil-family block decode (spec §4a "SIMD
//! bit-unpack"): `for_util_decode` / `for_delta_util_decode` /
//! `pfor_util_decode` all bottom out in [`super::for_util_decode_primitive`],
//! which dispatches here first.
//!
//! The kernel re-expresses the scalar reference decoder's three phases over
//! 256/128-bit vectors, bit-for-bit:
//!
//! 1. **Block read**: the packed block is exactly `bpv*2` little-endian
//!    longs; one `read_bytes` of `bpv*16` bytes replaces `bpv*2` `read_long`
//!    calls. Byte consumption is identical to the scalar path.
//! 2. **Plane extraction**: the scalar inner loop
//!    `longs[idx] = (tmp[i] >> shift) & value_mask` is a per-u64-lane
//!    operation, so it maps directly onto `_mm256_srl_epi64` +
//!    `_mm256_and_si256` four lanes at a time. The plane length `2*bpv` is
//!    always even, so the tail is at most 2 longs and uses the 128-bit form.
//!    Remainder planes (values straddling tmp slots when `bpv ∤ primitive`)
//!    stay scalar: at most `2*(primitive mod bpv)` values, with
//!    data-dependent control flow that does not vectorize profitably.
//! 3. **expand8/16/32**: the lane re-grouping is a fixed shift+mask per
//!    output group, again lane-parallel.
//!
//! ## Safety argument (module-level `allow(unsafe_code)`)
//!
//! The crate is `#![deny(unsafe_code)]`; this module is the single, narrow
//! exception, and every unsafe operation is an AVX2/SSE intrinsic inside a
//! `#[target_feature(enable = "avx2")]` function. Those functions are only
//! reached through [`try_decode`], which gates on a cached
//! `is_x86_feature_detected!("avx2")` (cpuid-based, also works on the
//! x86_64-unknown-linux-musl target), so no AVX2 instruction can execute on
//! a CPU without support. All memory access uses unaligned intrinsics
//! (`loadu`/`storeu`) on fixed-size stack arrays: the byte buffer is
//! `[u8; 512]` and vector loads reach at most `buf[(bpv*2-4)*8 .. bpv*16]`
//! with `bpv ≤ 32`; `longs`/`values` are `[u64; 128]` and stores reach at
//! most index `num_longs = 2*primitive ≤ 64 < 128` (expand stores stay
//! inside `values[0..128]` by the expand8/16/32 index arithmetic). These
//! bounds hold by construction of the ForUtil layout, and the differential
//! tests in this file pin scalar-vs-SIMD equality (outputs and consumed
//! byte counts) for every reachable `(bpv, primitive)` pair. Equivalence is
//! additionally locked end-to-end by the Java diff battery
//! (interop/verify-log.sh), which exercises this path through real segment
//! files on AVX2 hosts.

#![allow(unsafe_code)]

use std::io;
use std::sync::OnceLock;

use super::{primitive_mask, BLOCK_SIZE};
use crate::io::DataInput;

/// Cached one-time AVX2 detection (spec §4a: runtime dispatch, detected
/// once). `RL_SIMD=0` in the environment forces the scalar path (kill switch
/// for the fast path; also enables same-binary A/B benchmarking).
#[inline]
fn avx2_available() -> bool {
    static DETECTED: OnceLock<bool> = OnceLock::new();
    *DETECTED.get_or_init(|| {
        std::env::var_os("RL_SIMD").map_or(true, |v| v != "0")
            && std::is_x86_feature_detected!("avx2")
    })
}

/// Dispatch shim for [`super::for_util_decode_primitive`]: decodes via the
/// AVX2 kernel and returns `Ok(true)`, or returns `Ok(false)` without
/// touching the input when the CPU lacks AVX2 (the caller then runs the
/// scalar reference).
pub(super) fn try_decode(
    input: &mut impl DataInput,
    values: &mut [u64; BLOCK_SIZE],
    bpv: u32,
    primitive: u32,
) -> io::Result<bool> {
    if !avx2_available() {
        return Ok(false);
    }
    // SAFETY: `avx2_available()` just returned true, so this CPU may execute
    // AVX2 instructions.
    unsafe { decode_block_avx2(input, values, bpv, primitive) }?;
    Ok(true)
}

/// AVX2 kernel for [`super::for_util_decode_primitive`]; see module docs.
/// `bpv` is 1..=32 (the public entries already handled 0 and rejected >32).
#[target_feature(enable = "avx2")]
fn decode_block_avx2(
    input: &mut impl DataInput,
    values: &mut [u64; BLOCK_SIZE],
    bpv: u32,
    primitive: u32,
) -> io::Result<()> {
    use std::arch::x86_64::*;

    debug_assert!((1..=32).contains(&bpv));
    let num_longs = BLOCK_SIZE * primitive as usize / 64;
    let num_longs_per_shift = (bpv * 2) as usize;

    // The block is bpv*2 little-endian longs; read it in one call.
    let mut buf = [0u8; BLOCK_SIZE * 4]; // 512 = max bpv (32) * 16
    input.read_bytes(&mut buf[..num_longs_per_shift * 8])?;

    let value_mask = primitive_mask(primitive, bpv);
    let mut longs = [0u64; BLOCK_SIZE];
    let mut idx = 0usize;

    // Whole bpv planes (vector form of the reference shift loop).
    let mut shift = primitive as i64 - bpv as i64;
    loop {
        debug_assert!(shift >= 0);
        // SAFETY: loads reach buf[(i+4)*8) with i+4 <= 2*bpv <= 64 (buf is
        // 512 bytes); stores reach longs[idx+4) with idx+4 <= num_longs <=
        // 128 (see module docs).
        unsafe {
            let vmask = _mm256_set1_epi64x(value_mask as i64);
            let count = _mm_cvtsi64_si128(shift);
            let mut i = 0usize;
            while i + 4 <= num_longs_per_shift {
                let v = _mm256_loadu_si256(buf.as_ptr().add(i * 8) as *const __m256i);
                let v = _mm256_and_si256(_mm256_srl_epi64(v, count), vmask);
                _mm256_storeu_si256(longs.as_mut_ptr().add(idx) as *mut __m256i, v);
                i += 4;
                idx += 4;
            }
            // 2*bpv is even, so the tail is either empty or exactly 2 longs.
            if i < num_longs_per_shift {
                debug_assert_eq!(num_longs_per_shift - i, 2);
                let v = _mm_loadu_si128(buf.as_ptr().add(i * 8) as *const __m128i);
                let v =
                    _mm_and_si128(_mm_srl_epi64(v, count), _mm_set1_epi64x(value_mask as i64));
                _mm_storeu_si128(longs.as_mut_ptr().add(idx) as *mut __m128i, v);
                idx += 2;
            }
        }
        shift -= bpv as i64;
        if shift < 0 {
            break;
        }
    }

    // Remainder planes (values split across tmp slots): scalar, verbatim
    // from the reference decoder, over a local copy of the tmp longs.
    if idx < num_longs {
        let mut tmp = [0u64; BLOCK_SIZE / 2];
        for (t, b) in tmp
            .iter_mut()
            .zip(buf.chunks_exact(8))
            .take(num_longs_per_shift)
        {
            *t = u64::from_le_bytes(b.try_into().unwrap());
        }
        let remaining_bits_per_long = (shift + bpv as i64) as u32;
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

    // Lane re-grouping (inverse of collapse8/16/32): vector form of
    // super::expand8/16/32. Safe call: same target features as the caller.
    expand_avx2(&longs, values, primitive);
    Ok(())
}

/// Vector form of `super::expand8/16/32` (identical per-lane math).
#[target_feature(enable = "avx2")]
fn expand_avx2(longs: &[u64; BLOCK_SIZE], values: &mut [u64; BLOCK_SIZE], primitive: u32) {
    use std::arch::x86_64::*;
    // SAFETY: loads/stores stay within the two fixed [u64; 128] arrays by the
    // expand8/16/32 index arithmetic (see module docs).
    unsafe {
        match primitive {
            8 => {
                let mask = _mm256_set1_epi64x(0xFF);
                for i in (0..16).step_by(4) {
                    let v = _mm256_loadu_si256(longs.as_ptr().add(i) as *const __m256i);
                    for j in 0..8i64 {
                        let count = _mm_cvtsi64_si128(56 - 8 * j);
                        let g = _mm256_and_si256(_mm256_srl_epi64(v, count), mask);
                        _mm256_storeu_si256(
                            values.as_mut_ptr().add(16 * j as usize + i) as *mut __m256i,
                            g,
                        );
                    }
                }
            }
            16 => {
                let mask = _mm256_set1_epi64x(0xFFFF);
                for i in (0..32).step_by(4) {
                    let v = _mm256_loadu_si256(longs.as_ptr().add(i) as *const __m256i);
                    for j in 0..4i64 {
                        let count = _mm_cvtsi64_si128(48 - 16 * j);
                        let g = _mm256_and_si256(_mm256_srl_epi64(v, count), mask);
                        _mm256_storeu_si256(
                            values.as_mut_ptr().add(32 * j as usize + i) as *mut __m256i,
                            g,
                        );
                    }
                }
            }
            32 => {
                let mask = _mm256_set1_epi64x(0xFFFF_FFFF);
                for i in (0..64).step_by(4) {
                    let v = _mm256_loadu_si256(longs.as_ptr().add(i) as *const __m256i);
                    _mm256_storeu_si256(
                        values.as_mut_ptr().add(i) as *mut __m256i,
                        _mm256_srli_epi64(v, 32),
                    );
                    _mm256_storeu_si256(
                        values.as_mut_ptr().add(64 + i) as *mut __m256i,
                        _mm256_and_si256(v, mask),
                    );
                }
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{IndexInput, IndexOutput};
    use crate::postings_ll::{
        for_delta_util_decode, for_delta_util_encode, for_util_decode, for_util_decode_primitive,
        for_util_encode, for_util_encode_primitive, pfor_util_decode, pfor_util_encode,
    };

    /// xorshift64* — deterministic, no extra deps.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    fn enc(f: impl FnOnce(&mut IndexOutput) -> io::Result<()>) -> Vec<u8> {
        let mut out = IndexOutput::in_memory();
        f(&mut out).unwrap();
        out.into_bytes()
    }

    /// Every (bpv, primitive) pair reachable from the three public decode
    /// entries — ForUtil thresholds ForUtil.java:118-133, ForDeltaUtil
    /// thresholds ForDeltaUtil.java:261-269.
    fn reachable_configs() -> Vec<(u32, u32)> {
        let mut v: Vec<(u32, u32)> = Vec::new();
        for bpv in 1..=32u32 {
            let for_util = if bpv <= 8 {
                8
            } else if bpv <= 16 {
                16
            } else {
                32
            };
            let for_delta = if bpv <= 4 {
                8
            } else if bpv <= 11 {
                16
            } else {
                32
            };
            v.push((bpv, for_util));
            if for_delta != for_util {
                v.push((bpv, for_delta));
            }
        }
        v
    }

    /// Boundary + random patterns: all-zero, all-max, alternating,
    /// stride-37, a single bit walking across byte/lane boundaries, and
    /// seeded random values.
    fn patterns(bpv: u32) -> Vec<[u64; BLOCK_SIZE]> {
        let mask = (1u64 << bpv) - 1;
        let mut out: Vec<[u64; BLOCK_SIZE]> = Vec::new();
        out.push([0u64; BLOCK_SIZE]);
        out.push([mask; BLOCK_SIZE]);
        let mut alt = [0u64; BLOCK_SIZE];
        for (i, x) in alt.iter_mut().enumerate() {
            *x = if i % 2 == 0 { mask } else { 0 };
        }
        out.push(alt);
        let mut stride = [0u64; BLOCK_SIZE];
        for (i, x) in stride.iter_mut().enumerate() {
            *x = (i as u64 * 37 + 5) & mask;
        }
        out.push(stride);
        let mut walking = [0u64; BLOCK_SIZE];
        for (i, x) in walking.iter_mut().enumerate() {
            *x = 1u64 << (i as u32 % bpv);
        }
        out.push(walking);
        for seed in [1u64, 42, 0xDEAD_BEEF] {
            let mut rng = Rng(seed);
            let mut v = [0u64; BLOCK_SIZE];
            for x in v.iter_mut() {
                *x = rng.next() & mask;
            }
            out.push(v);
        }
        out
    }

    fn decode_scalar(bytes: &[u8], bpv: u32, primitive: u32) -> ([u64; BLOCK_SIZE], u64) {
        let mut input = IndexInput::in_memory(bytes.to_vec());
        let mut v = [0u64; BLOCK_SIZE];
        for_util_decode_primitive(&mut input, &mut v, bpv, primitive).unwrap();
        (v, input.file_pointer())
    }

    fn decode_simd(bytes: &[u8], bpv: u32, primitive: u32) -> Option<([u64; BLOCK_SIZE], u64)> {
        let mut input = IndexInput::in_memory(bytes.to_vec());
        let mut v = [0u64; BLOCK_SIZE];
        match try_decode(&mut input, &mut v, bpv, primitive).unwrap() {
            true => Some((v, input.file_pointer())),
            false => None,
        }
    }

    /// The core differential test (spec §4a discipline): for every reachable
    /// (bpv, primitive), SIMD and scalar outputs are elementwise equal and
    /// consume exactly the same bytes.
    #[test]
    fn avx2_matches_scalar_all_configs_and_patterns() {
        if !avx2_available() {
            eprintln!("AVX2 unavailable on this host, skipping differential test");
            return;
        }
        for (bpv, primitive) in reachable_configs() {
            for (pi, values) in patterns(bpv).iter().enumerate() {
                let bytes = enc(|o| for_util_encode_primitive(o, values, bpv, primitive));
                let (sv, sp) = decode_scalar(&bytes, bpv, primitive);
                let (xv, xp) = decode_simd(&bytes, bpv, primitive).unwrap();
                assert_eq!(
                    sp,
                    bytes.len() as u64,
                    "scalar did not consume the whole block bpv={bpv} primitive={primitive}"
                );
                assert_eq!(
                    sp, xp,
                    "consumed byte count differs bpv={bpv} primitive={primitive} pattern={pi}"
                );
                assert_eq!(
                    sv, xv,
                    "decoded values differ bpv={bpv} primitive={primitive} pattern={pi}"
                );
            }
        }
    }

    /// Public-entry equivalence: the dispatched for_util / for_delta / pfor
    /// decodes (which take the AVX2 path on this host) reproduce the exact
    /// inputs, including the all-ones collapse and patched exceptions.
    #[test]
    fn avx2_public_entries_round_trip() {
        if !avx2_available() {
            eprintln!("AVX2 unavailable on this host, skipping public-entry test");
            return;
        }
        let mut rng = Rng(7);

        // for_util_decode, every bpv in 1..=32.
        for bpv in 1..=32u8 {
            let mask = (1u64 << bpv) - 1;
            let mut v = [0u64; BLOCK_SIZE];
            for x in v.iter_mut() {
                *x = rng.next() & mask;
            }
            let bytes = enc(|o| for_util_encode(o, &v, bpv));
            let mut got = [0u64; BLOCK_SIZE];
            for_util_decode(&mut IndexInput::in_memory(bytes), &mut got, bpv).unwrap();
            assert_eq!(got, v, "for_util_decode bpv={bpv}");
        }

        // for_delta_util_decode: random deltas spanning the three primitive
        // classes, plus the all-ones collapse.
        for max_log in [1u32, 4, 5, 11, 12, 20, 31] {
            let mut deltas = [0u64; BLOCK_SIZE];
            for d in deltas.iter_mut() {
                *d = (rng.next() & ((1u64 << max_log) - 1)) + 1;
            }
            let bytes = enc(|o| for_delta_util_encode(o, &deltas));
            let mut got = [0u64; BLOCK_SIZE];
            for_delta_util_decode(&mut IndexInput::in_memory(bytes), &mut got).unwrap();
            assert_eq!(got, deltas, "for_delta_util_decode max_log={max_log}");
        }
        let ones = [1u64; BLOCK_SIZE];
        let bytes = enc(|o| for_delta_util_encode(o, &ones));
        let mut got = [0u64; BLOCK_SIZE];
        for_delta_util_decode(&mut IndexInput::in_memory(bytes), &mut got).unwrap();
        assert_eq!(got, ones, "for_delta_util_decode all-ones");

        // pfor_util_decode: random freqs with up to MAX_EXCEPTIONS large
        // values (patched path) and a constant block.
        for trial in 0..4u32 {
            let mut v = [0u64; BLOCK_SIZE];
            for x in v.iter_mut() {
                *x = rng.next() & 0xF;
            }
            for e in 0..trial {
                v[(rng.next() % BLOCK_SIZE as u64) as usize] = 1 << (20 + e);
            }
            let bytes = enc(|o| pfor_util_encode(o, &v));
            let mut got = [0u64; BLOCK_SIZE];
            pfor_util_decode(&mut IndexInput::in_memory(bytes), &mut got).unwrap();
            assert_eq!(got, v, "pfor_util_decode trial={trial}");
        }
    }

    /// Kernel-level microbenchmark (not a correctness gate): ns/block of the
    /// scalar reference vs the AVX2 path across representative bpv values.
    /// Run with `cargo test -p codec-lucene9 --release -- --ignored
    /// --nocapture avx2_kernel_bench`.
    #[test]
    #[ignore]
    fn avx2_kernel_bench() {
        use std::hint::black_box;
        use std::time::Instant;

        if !avx2_available() {
            eprintln!("AVX2 unavailable on this host, skipping bench");
            return;
        }
        const ROUNDS: u32 = 100_000;
        for bpv in [1u32, 2, 4, 7, 8, 9, 12, 16, 20, 24, 32] {
            let primitive = if bpv <= 8 {
                8
            } else if bpv <= 16 {
                16
            } else {
                32
            };
            let mask = (1u64 << bpv) - 1;
            let mut rng = Rng(99);
            let mut values = [0u64; BLOCK_SIZE];
            for x in values.iter_mut() {
                *x = rng.next() & mask;
            }
            let bytes = enc(|o| for_util_encode_primitive(o, &values, bpv, primitive));

            // Decode-only timing: one input, rewound with seek(0) per round
            // (the whole block stays buffered, so this measures the kernel,
            // not Vec allocation / refill).
            let mut scalar_input = IndexInput::in_memory(bytes.clone());
            let mut simd_input = IndexInput::in_memory(bytes);
            let mut v = [0u64; BLOCK_SIZE];
            let mut sink = 0u64;

            let t0 = Instant::now();
            for _ in 0..ROUNDS {
                scalar_input.seek(0).unwrap();
                for_util_decode_primitive(&mut scalar_input, &mut v, bpv, primitive).unwrap();
                sink = sink.wrapping_add(black_box(v[0]));
            }
            let scalar_ns = t0.elapsed().as_nanos() as f64 / ROUNDS as f64;

            let t0 = Instant::now();
            for _ in 0..ROUNDS {
                simd_input.seek(0).unwrap();
                try_decode(&mut simd_input, &mut v, bpv, primitive).unwrap();
                sink = sink.wrapping_add(black_box(v[0]));
            }
            let simd_ns = t0.elapsed().as_nanos() as f64 / ROUNDS as f64;
            black_box(sink);
            eprintln!(
                "bpv={bpv:2} primitive={primitive:2} scalar={scalar_ns:7.1} ns/block \
                 avx2={simd_ns:7.1} ns/block speedup={:.2}x",
                scalar_ns / simd_ns
            );
        }
    }
}
