//! AVX2 fast paths for bitset-container boolean ops (M3 spec §6). Filled
//! in Task 2; until then both shims decline and the scalar reference runs.
//!
//! ## Safety argument (module-level `allow(unsafe_code)`)
//!
//! Same pattern as `postings_ll/simd.rs`: every unsafe op will be an AVX2
//! intrinsic inside a `#[target_feature(enable = "avx2")]` fn, reached only
//! through these shims gated on a cached `is_x86_feature_detected!("avx2")`.
//! All memory access is unaligned load/store on the three fixed
//! `[u64; 1024]` arrays (256 iterations of 4 words each — in bounds by
//! construction). Differential tests pin scalar-vs-SIMD word-level equality.

#![allow(unsafe_code)]

use super::BITSET_WORDS;

/// out = a & b, returning cardinality; None when AVX2 is unavailable (the
/// caller then runs the scalar reference).
pub(super) fn try_bitset_and(
    _a: &[u64; BITSET_WORDS],
    _b: &[u64; BITSET_WORDS],
    _out: &mut [u64; BITSET_WORDS],
) -> Option<u32> {
    None
}

/// out = a | b, returning cardinality; None when AVX2 is unavailable.
pub(super) fn try_bitset_or(
    _a: &[u64; BITSET_WORDS],
    _b: &[u64; BITSET_WORDS],
    _out: &mut [u64; BITSET_WORDS],
) -> Option<u32> {
    None
}
