//! Frozen-view read side of the v3 inline term bitmap (M5 spec §2/§3):
//! one sequential region read into a 32B-aligned buffer, then zero-copy
//! `croaring::BitmapView::deserialize::<Frozen>` views (~60ns create,
//! probe REPORT §Bench). `FrozenBitmap` owns the buffer; views/iterators
//! borrow it, so they are rebuilt per call instead of stored — no
//! self-referential lifetimes, no second unsafe (关键设计事实 5).
//!
//! Validation = the v3 triple gate (len bound → magic/version → header
//! df/card == doc_freq, done by `parse_region` before `open`) plus a safe
//! re-implementation of every structural check CRoaring's
//! `roaring_bitmap_frozen_view` performs (cookie, typecodes, exact length —
//! roaring.c:18153-18203): the C side signals invalid bytes with NULL, and
//! the croaring wrapper asserts non-null (croaring-2.7.0
//! src/bitmap/view.rs:34), so without the pre-check corrupt bytes would
//! panic instead of falling back. Post-open, `cardinality == df` is
//! rechecked (validation ③). Any failure → None, the silent postings
//! fallback (查询永不报错).
//!
//! ## Safety argument (module-level `allow(unsafe_code)`)
//!
//! The crate is `#![deny(unsafe_code)]`; this module is the third narrow
//! exception (postings_ll/simd.rs, roaring/simd.rs — the latter deleted in
//! T4). The single unsafe operation is `FrozenBitmap::view`'s
//! `BitmapView::deserialize::<Frozen>`, whose contract (32B-aligned start,
//! exact frozen length, valid frozen bytes — croaring-2.7.0
//! src/bitmap/serialization.rs `impl ViewDeserializer for Frozen`) is
//! fully discharged by `open`: the aligned slot is asserted at
//! construction, the length is the region's payload length, and
//! `validate_frozen_layout` rejects every byte pattern the C side would
//! return NULL for.
#![allow(unsafe_code)]

use croaring::{BitmapView, Frozen};

/// CRoaring's frozen format cookie (roaring.h:7935 `FROZEN_COOKIE =
/// 13766`): low 15 bits of the trailing 4-byte header; num_containers in
/// the high 17 bits (roaring.c:18000-18004).
const FROZEN_COOKIE: u32 = 13766;

/// CRoaring container typecodes (roaring.h:5237-5239).
const TYPE_BITSET: u8 = 1;
const TYPE_ARRAY: u8 = 2;
const TYPE_RUN: u8 = 3;

/// Safe mirror of `roaring_bitmap_frozen_view`'s structural validation
/// (roaring.c:18153-18203): cookie → num_containers → typecodes ∈
/// {1,2,3} → exact length from the counts array (array/bitset counts =
/// card-1, run counts = n_runs, roaring.c:17993-17998). None = reject.
fn validate_frozen_layout(payload: &[u8]) -> Option<()> {
    let n = payload.len();
    if n < 4 {
        return None;
    }
    let header = u32::from_le_bytes(payload[n - 4..].try_into().unwrap());
    if header & 0x7FFF != FROZEN_COOKIE {
        return None;
    }
    let num = (header >> 15) as usize;
    if num < 1 || n < 4 + num * 5 {
        return None;
    }
    // zones at the tail: keys[num] counts[num] typecodes[num] header(4)
    let counts_off = n - 4 - num * 3;
    let typecodes_off = n - 4 - num;
    let mut size = 4 + 5 * num;
    for i in 0..num {
        let count = u16::from_le_bytes(
            payload[counts_off + 2 * i..counts_off + 2 * i + 2]
                .try_into()
                .unwrap(),
        ) as usize;
        size += match payload[typecodes_off + i] {
            TYPE_BITSET => 8192,           // 1024 * u64
            TYPE_ARRAY => (count + 1) * 2, // counts = cardinality - 1
            TYPE_RUN => count * 4,         // counts = n_runs; rle16_t
            _ => return None,
        };
    }
    if size != n {
        return None; // not exactly one frozen bitmap
    }
    Some(())
}

/// Opened + validated frozen payload of a v3 bitmap region (M5 §2). Owns
/// the payload copy in a 32B-aligned slot; all reads go through freshly
/// created `BitmapView`s (~60ns, amortized over batches by `docs_from`).
/// 32 bytes (Vec + usize) — no boxing needed (M4 事实 6 的 8KB 定位流
/// 顾虑随 probe 模式消失).
pub struct FrozenBitmap {
    buf: Vec<u8>, // payload.len() + 31; the aligned slice starts at `off`
    off: usize,
}

impl FrozenBitmap {
    /// Validates + copies a region's frozen payload (region minus the
    /// [magic+version+df+card] header, parsed by `parse_region`). None on
    /// ANY deviation — the silent-fallback signal.
    pub fn open(payload: &[u8], expected_df: u32) -> Option<FrozenBitmap> {
        validate_frozen_layout(payload)?;
        let mut buf = vec![0u8; payload.len() + Frozen::REQUIRED_ALIGNMENT - 1];
        let off = buf.as_ptr().align_offset(Frozen::REQUIRED_ALIGNMENT);
        // Vec<u8> alignment is 1; with 31 spare bytes an aligned slot
        // always exists (align_offset fails only when it cannot prove one)
        assert!(off != usize::MAX && off + payload.len() <= buf.len());
        buf[off..off + payload.len()].copy_from_slice(payload);
        // drop the spare tail: the frozen view contract requires
        // data.len() == the frozen bitmap's exact size (croaring-2.7.0
        // serialization.rs `impl ViewDeserializer for Frozen`)
        buf.truncate(off + payload.len());
        let bm = FrozenBitmap { buf, off };
        // validation ③ (spec §3): cardinality == termState.doc_freq
        if bm.cardinality() != expected_df as u64 {
            return None;
        }
        Some(bm)
    }

    fn payload(&self) -> &[u8] {
        &self.buf[self.off..]
    }

    fn view(&self) -> BitmapView<'_> {
        // SAFETY: the slice starts at a 32-byte-aligned offset (asserted
        // in `open`) and its length is exactly the frozen bitmap's length;
        // `validate_frozen_layout` has already rejected every byte pattern
        // CRoaring's `roaring_bitmap_frozen_view` returns NULL for
        // (roaring.c:18153-18203), so the wrapper's non-null assert cannot
        // fire. The view borrows `self` and never outlives this call's
        // caller.
        unsafe { BitmapView::deserialize::<Frozen>(self.payload()) }
    }

    /// Membership test (8.5–28.7 ns/probe, probe REPORT §Bench) — the
    /// skew-AND probe and tier-2 filter primitive.
    pub fn contains(&self, doc: u32) -> bool {
        self.view().contains(doc)
    }

    pub fn cardinality(&self) -> u64 {
        self.view().cardinality()
    }

    /// Non-materializing intersection count (SIMD C path, µs级, M5 §2).
    pub fn and_cardinality(&self, other: &FrozenBitmap) -> u64 {
        self.view().and_cardinality(&other.view())
    }

    /// Non-materializing union count (M5 §2).
    pub fn or_cardinality(&self, other: &FrozenBitmap) -> u64 {
        self.view().or_cardinality(&other.view())
    }

    /// Batch ascending-doc read: fills `dst` with the first docs >=
    /// `from`, returns the count read (0 = exhausted).
    /// `reset_at_or_after` + `next_many` include the current value
    /// (croaring-2.7.0 src/bitmap/iter.rs `BitmapIterator`), so a caller
    /// resuming after doc d passes d+1. View creation (~60ns) is
    /// amortized over the batch — the wrapper stays free of
    /// self-referential lifetimes (关键设计事实 5).
    pub fn docs_from(&self, from: u32, dst: &mut [u32]) -> usize {
        let view = self.view();
        let mut it = view.iter();
        it.reset_at_or_after(from);
        it.next_many(dst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use croaring::Bitmap;

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
        fn below(&mut self, n: u32) -> u32 {
            (self.next() % n as u64) as u32
        }
    }

    /// Sorted unique docs: `n` random low-16 values per listed bucket.
    fn shaped(seed: u64, buckets: &[(u16, usize)]) -> Vec<u32> {
        let mut rng = Rng(seed);
        let mut docs: Vec<u32> = Vec::new();
        for &(key, n) in buckets {
            for _ in 0..n {
                docs.push(((key as u32) << 16) | rng.below(65536));
            }
            docs.sort_unstable();
            docs.dedup();
        }
        docs
    }

    /// Frozen payload via the write-side pipeline (T1, probe main.rs:60-77).
    fn frozen_payload(docs: &[u32]) -> Vec<u8> {
        let mut b = Bitmap::of(docs);
        b.run_optimize();
        b.shrink_to_fit();
        let mut buf = Vec::new();
        let s = b.serialize_into_vec::<Frozen>(&mut buf);
        s.to_vec()
    }

    fn open_shaped(seed: u64, buckets: &[(u16, usize)]) -> (Vec<u32>, FrozenBitmap) {
        let docs = shaped(seed, buckets);
        let bm = FrozenBitmap::open(&frozen_payload(&docs), docs.len() as u32).unwrap();
        (docs, bm)
    }

    #[test]
    fn open_aligns_buffer_and_iterates_in_batches() {
        let docs: Vec<u32> = (0..10_000u32).map(|i| i * 7).collect();
        let bm = FrozenBitmap::open(&frozen_payload(&docs), docs.len() as u32).unwrap();
        assert_eq!(bm.payload().as_ptr() as usize % 32, 0, "32B alignment");
        assert_eq!(bm.cardinality(), docs.len() as u64);
        // batch 1
        let mut buf = [0u32; 4096];
        let n = bm.docs_from(0, &mut buf);
        assert_eq!(n, 4096);
        assert_eq!(&buf[..n], &docs[..4096]);
        // batch 2 resumes strictly after the last emitted doc
        let n2 = bm.docs_from(buf[n - 1] + 1, &mut buf);
        assert_eq!(&buf[..n2], &docs[4096..4096 + n2]);
        // seek lands on the first doc >= from; past-the-end yields 0
        let n3 = bm.docs_from(63_000, &mut buf);
        assert_eq!(&buf[..n3], &docs[9000..]);
        assert_eq!(bm.docs_from(70_000, &mut buf), 0);
    }

    #[test]
    fn contains_and_cardinality_match_reference() {
        let (docs, bm) = open_shaped(7, &[(0, 5000), (3, 9000)]);
        for d in 0..300_000u32 {
            assert_eq!(bm.contains(d), docs.binary_search(&d).is_ok(), "doc {d}");
        }
        let (docs2, bm2) = open_shaped(11, &[(0, 4000), (3, 8000)]);
        let (a, b) = (Bitmap::of(&docs), Bitmap::of(&docs2));
        assert_eq!(bm.and_cardinality(&bm2), a.and_cardinality(&b));
        assert_eq!(bm.or_cardinality(&bm2), a.or_cardinality(&b));
    }

    #[test]
    fn open_rejects_structural_deviations() {
        let docs = shaped(5, &[(0, 100), (1, 5000)]);
        let payload = frozen_payload(&docs);
        let df = docs.len() as u32;
        let n = payload.len();
        // cardinality != expected_df (validation ③)
        assert!(FrozenBitmap::open(&payload, df + 1).is_none());
        // bad cookie / num_containers (tail 4B header)
        let mut bad = payload.clone();
        bad[n - 1] ^= 0xFF;
        assert!(FrozenBitmap::open(&bad, df).is_none());
        // bad typecode
        let header = u32::from_le_bytes(payload[n - 4..].try_into().unwrap());
        let num = (header >> 15) as usize;
        let mut bad = payload.clone();
        bad[n - 4 - num] = 9; // typecodes zone: n - 4 - num .. n - 4
        assert!(FrozenBitmap::open(&bad, df).is_none());
        // exact-length contract: truncated / padded
        assert!(FrozenBitmap::open(&payload[..n - 1], df).is_none());
        let mut padded = payload.clone();
        padded.push(0);
        assert!(FrozenBitmap::open(&padded, df).is_none());
        // too small for a header
        assert!(FrozenBitmap::open(&payload[..3], df).is_none());
    }
}
