//! AVX2 fast paths for bitset-container boolean ops (M3 spec §6).
//!
//! ## Safety argument (module-level `allow(unsafe_code)`)
//!
//! Same pattern as `postings_ll/simd.rs`: every unsafe op is an AVX2
//! intrinsic inside a `#[target_feature(enable = "avx2")]` fn, reached only
//! through these shims gated on a cached `is_x86_feature_detected!("avx2")`.
//! All memory access is unaligned load/store on the three fixed
//! `[u64; 1024]` arrays (256 iterations of 4 words each — in bounds by
//! construction). Differential tests pin scalar-vs-SIMD word-level equality.

#![allow(unsafe_code)]

use std::sync::OnceLock;

use super::BITSET_WORDS;

/// Cached one-time AVX2 detection; `RL_SIMD=0` forces the scalar fallback
/// (same kill switch as postings_ll/simd.rs:56-62, also enables
/// same-binary A/B benchmarking).
#[inline]
fn avx2_available() -> bool {
    static DETECTED: OnceLock<bool> = OnceLock::new();
    *DETECTED.get_or_init(|| {
        std::env::var_os("RL_SIMD").map_or(true, |v| v != "0")
            && std::is_x86_feature_detected!("avx2")
    })
}

/// out = a & b, returning cardinality; None when AVX2 is unavailable (the
/// caller then runs the scalar reference).
pub(super) fn try_bitset_and(
    a: &[u64; BITSET_WORDS],
    b: &[u64; BITSET_WORDS],
    out: &mut [u64; BITSET_WORDS],
) -> Option<u32> {
    if !avx2_available() {
        return None;
    }
    // SAFETY: `avx2_available()` just returned true, so this CPU may
    // execute AVX2 instructions.
    Some(unsafe { bitset_binop_avx2(a, b, out, true) })
}

/// out = a | b, returning cardinality; None when AVX2 is unavailable.
pub(super) fn try_bitset_or(
    a: &[u64; BITSET_WORDS],
    b: &[u64; BITSET_WORDS],
    out: &mut [u64; BITSET_WORDS],
) -> Option<u32> {
    if !avx2_available() {
        return None;
    }
    // SAFETY: see `try_bitset_and`.
    Some(unsafe { bitset_binop_avx2(a, b, out, false) })
}

/// Vector AND/OR + vertical popcount: the result vector's bytes are counted
/// via the nibble-LUT (`_mm256_shuffle_epi8`) + `_mm256_sad_epu8` idiom —
/// per-lane identical math to the scalar `count_ones` loop, pinned by the
/// differential test below (spec §6 等价追加).
#[target_feature(enable = "avx2")]
fn bitset_binop_avx2(
    a: &[u64; BITSET_WORDS],
    b: &[u64; BITSET_WORDS],
    out: &mut [u64; BITSET_WORDS],
    is_and: bool,
) -> u32 {
    use std::arch::x86_64::*;
    // SAFETY: loads/stores stay within the three fixed [u64; 1024] arrays
    // (256 iterations of exactly 4 words); see module docs.
    unsafe {
        let lut = _mm256_setr_epi8(
            0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4, 0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2,
            3, 3, 4,
        );
        let low_mask = _mm256_set1_epi8(0x0f);
        let mut acc = _mm256_setzero_si256();
        for i in (0..BITSET_WORDS).step_by(4) {
            let va = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
            let vb = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
            let v = if is_and {
                _mm256_and_si256(va, vb)
            } else {
                _mm256_or_si256(va, vb)
            };
            _mm256_storeu_si256(out.as_mut_ptr().add(i) as *mut __m256i, v);
            let lo = _mm256_and_si256(v, low_mask);
            let hi = _mm256_and_si256(_mm256_srli_epi16(v, 4), low_mask);
            let cnt = _mm256_add_epi8(_mm256_shuffle_epi8(lut, lo), _mm256_shuffle_epi8(lut, hi));
            acc = _mm256_add_epi64(acc, _mm256_sad_epu8(cnt, _mm256_setzero_si256()));
        }
        let mut lanes = [0u64; 4];
        _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, acc);
        (lanes[0] + lanes[1] + lanes[2] + lanes[3]) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roaring::{bitset_and_scalar, bitset_or_scalar};

    /// xorshift64* — deterministic, same as the other codec test modules.
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

    /// Operand pairs: all-zero/all-one, ones/ones, alternating,
    /// single-bit, and several seeded densities.
    fn patterns() -> Vec<([u64; BITSET_WORDS], [u64; BITSET_WORDS])> {
        let mut out: Vec<([u64; BITSET_WORDS], [u64; BITSET_WORDS])> = Vec::new();
        let zero = [0u64; BITSET_WORDS];
        let ones = [u64::MAX; BITSET_WORDS];
        out.push((zero, ones));
        out.push((ones, ones));
        let mut alt_a = [0u64; BITSET_WORDS];
        let mut alt_b = [0u64; BITSET_WORDS];
        for i in 0..BITSET_WORDS {
            alt_a[i] = 0xAAAA_AAAA_AAAA_AAAA;
            alt_b[i] = 0x5555_5555_5555_5555;
        }
        out.push((alt_a, alt_b));
        let mut single = [0u64; BITSET_WORDS];
        single[513] = 1 << 37;
        out.push((single, ones));
        for (seed, shift) in [(1u64, 1u32), (7, 7), (42, 32), (0xDEAD, 63)] {
            let mut rng = Rng(seed);
            let mut a = [0u64; BITSET_WORDS];
            let mut b = [0u64; BITSET_WORDS];
            for i in 0..BITSET_WORDS {
                a[i] = rng.next() >> shift;
                b[i] = rng.next() >> shift;
            }
            out.push((a, b));
        }
        out
    }

    /// SIMD 纪律对拍（spec §6）：每对操作数，AVX2 与标量参考的输出
    /// 逐 u64 相等、cardinality 相等。
    #[test]
    fn avx2_bitset_and_or_match_scalar() {
        if !avx2_available() {
            eprintln!("AVX2 unavailable on this host, skipping differential test");
            return;
        }
        for (pi, (a, b)) in patterns().iter().enumerate() {
            let mut out_s = [0u64; BITSET_WORDS];
            let mut out_x = [0u64; BITSET_WORDS];
            let c_s = bitset_and_scalar(a, b, &mut out_s);
            let c_x = try_bitset_and(a, b, &mut out_x)
                .unwrap_or_else(|| panic!("pattern {pi}: AVX2 path must engage"));
            assert_eq!(c_s, c_x, "and card pattern {pi}");
            assert_eq!(out_s, out_x, "and words pattern {pi}");

            let mut out_s = [0u64; BITSET_WORDS];
            let mut out_x = [0u64; BITSET_WORDS];
            let c_s = bitset_or_scalar(a, b, &mut out_s);
            let c_x = try_bitset_or(a, b, &mut out_x)
                .unwrap_or_else(|| panic!("pattern {pi}: AVX2 path must engage"));
            assert_eq!(c_s, c_x, "or card pattern {pi}");
            assert_eq!(out_s, out_x, "or words pattern {pi}");
        }
    }
}
