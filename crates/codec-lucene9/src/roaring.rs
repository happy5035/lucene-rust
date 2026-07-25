//! Inline per-term bitmap in the .doc stream, format v3 (M5 spec §3):
//! `[ magic "RLBM" + version=3 + df(vInt) + cardinality(vInt) + Frozen
//! payload ][ len: u32 LE ]`, written ahead of a term's postings. The
//! engine is croaring (CRoaring 4.7.1 via croaring-sys): the write side
//! builds `Bitmap::of` → `run_optimize` → `shrink_to_fit` → Frozen
//! serialize (`write_term_bitmap`); the read side opens zero-copy frozen
//! views over a 32B-aligned buffer copy (`roaring/frozen.rs`, one of the
//! crate's two module-level `#[allow(unsafe_code)]` modules). The
//! self-built container library (M3/M4) was deleted in M5 T4.

use std::io;

use crate::io::{DataInput, DataOutput, IndexInput, IndexOutput};

mod frozen;
mod materialized;

pub use frozen::{FrozenBitmap, and_cardinality, intersect_docs, or_cardinality, union_docs};
pub use materialized::MaterializedBitmap;

/// Bitmap-source read gate (spec §5): only terms with df >= this threshold
/// attempt the inline-bitmap path.
pub const BITMAP_MIN_DF: u32 = 4096;

// ------------------------------------------------------------------
// wire format (spec §4; 布局逐项见关键设计事实 2)
// ------------------------------------------------------------------

pub const BITMAP_MAGIC: [u8; 4] = *b"RLBM";
/// Wire format version. v3 (M5 §3): payload = CRoaring Frozen format
/// (engine replaced by croaring; the self-built container payload is
/// gone). The version byte is the only migration gate: != 3 (v1/v2
/// included) silently falls back to postings.
pub const BITMAP_VERSION: u8 = 3;

/// Upper bound of the bitmap region length (header+payload) for a segment
/// with `max_doc` docs. Derivation (spec §3 len 有界校验; 关键设计事实 1):
/// CRoaring frozen payload = 每 container 数据区 ≤ 8192B（array ≤ 4096×2、
/// bitset = 1024×8、run 只在严格更小时转换 ⇒ < 8192B，convert_run_optimize
/// roaring.c:9915-9970）+ 每 container 5B（keys 2 + counts 2 + typecodes 1）
/// + 4B header（frozen_size_in_bytes roaring.c:18017-18039）；
/// container 数 ≤ ceil(maxDoc/65536)；我方头 ≤ 4+1+5+5 = 15B：
///   max_bitmap_len = 19 + ceil(max_doc / 65536) * 8197
pub fn max_bitmap_len(max_doc: u32) -> u64 {
    19 + (max_doc as u64).div_ceil(65536) * 8197
}

/// Builds the bitmap for one term and writes `[region][len: u32 LE]` into
/// the .doc stream, immediately before the term's postings (M5 §3). Engine:
/// croaring — `Bitmap::of` (bulk append, sorted input) → `run_optimize` →
/// `shrink_to_fit` → Frozen serialize (`Bitmap::serialize::<Frozen>` does
/// not exist — Frozen is not NoAlign, imp.rs:871; `serialize_into_vec`
/// carves a 32B-aligned slice inside the Vec, probe main.rs:60-77). Must go
/// through the same checksumming output as the rest of .doc so the footer
/// CRC stays valid (spec §4a.2; CodecUtil.writeCRC :643-650).
pub fn write_term_bitmap(out: &mut impl DataOutput, docs: &[u32]) -> io::Result<()> {
    debug_assert!(!docs.is_empty());
    let mut bitmap = croaring::Bitmap::of(docs);
    bitmap.run_optimize();
    bitmap.shrink_to_fit();
    let mut buf = Vec::new();
    let payload = bitmap.serialize_into_vec::<croaring::Frozen>(&mut buf);
    let mut region = IndexOutput::in_memory();
    // in-memory writes never fail (Vec sink)
    region.write_bytes(&BITMAP_MAGIC).unwrap();
    region.write_byte(BITMAP_VERSION).unwrap();
    region.write_vint(docs.len() as i32).unwrap();
    // docs-only bitmap: cardinality == df (asserted by construction)
    debug_assert_eq!(bitmap.cardinality(), docs.len() as u64);
    region.write_vint(docs.len() as i32).unwrap();
    region.write_bytes(payload).unwrap();
    let bytes = region.into_bytes();
    out.write_bytes(&bytes)?;
    out.write_int(bytes.len() as i32)?;
    Ok(())
}

/// Parses + validates a v3 bitmap region (the `len` bytes preceding
/// docStartFP-4; M5 §3): magic → version (!= 3, v1/v2 included → None,
/// the whole migration story) → df == expected_df → card == df → frozen
/// payload open (structural pre-validation + cardinality recheck,
/// frozen.rs). None = the read side's silent-fallback signal.
pub fn parse_region(region: &[u8], expected_df: u32) -> Option<FrozenBitmap> {
    // magic 4 + version 1 + df/card ≥ 1B each + frozen header 4
    if region.len() < 11 {
        return None;
    }
    let mut input = IndexInput::in_memory(region.to_vec());
    let mut magic = [0u8; 4];
    input.read_bytes(&mut magic).ok()?;
    if magic != BITMAP_MAGIC {
        return None;
    }
    if input.read_byte().ok()? != BITMAP_VERSION {
        return None;
    }
    let df = input.read_vint().ok()? as u32;
    if df != expected_df {
        return None;
    }
    let card = input.read_vint().ok()? as u32;
    if card != df {
        return None; // docs-only bitmap: cardinality == df
    }
    FrozenBitmap::open(&region[input.file_pointer() as usize..], expected_df)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn shaped_docs(rng: &mut Rng, buckets: &[(u16, usize)]) -> Vec<u32> {
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

    #[test]
    fn max_bitmap_len_bound_holds() {
        assert_eq!(max_bitmap_len(200_000), 19 + 4 * 8197);
        assert_eq!(max_bitmap_len(1), 19 + 8197);
        // v3: the bound applies to the region length write_term_bitmap emits
        let docs = shaped_docs(&mut Rng(3), &[(0, 4096), (1, 5000), (2, 6000), (3, 100)]);
        let mut out = IndexOutput::in_memory();
        write_term_bitmap(&mut out, &docs).unwrap();
        let bytes = out.into_bytes();
        let len = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap()) as u64;
        assert_eq!(len as usize + 4, bytes.len());
        assert!(len <= max_bitmap_len(200_000), "len {len} vs bound");
    }

    /// v3 layout (M5 §3): [magic + version=3 + df + card + Frozen payload]
    /// [len u32 LE]; payload tail = frozen header (FROZEN_COOKIE 低 15 位).
    #[test]
    fn write_term_bitmap_appends_len_suffix() {
        let docs: Vec<u32> = (0..5000u32).collect();
        let mut out = IndexOutput::in_memory();
        write_term_bitmap(&mut out, &docs).unwrap();
        let bytes = out.into_bytes();
        let len = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap()) as usize;
        assert_eq!(len + 4, bytes.len());
        let region = &bytes[..len];
        assert_eq!(&region[..4], b"RLBM");
        assert_eq!(region[4], 3, "format v3");
        // df + card 两个 vInt（5000 = 0x88 0x27 两字节编码）
        let mut input = IndexInput::in_memory(region[5..].to_vec());
        assert_eq!(input.read_vint().unwrap(), 5000);
        assert_eq!(input.read_vint().unwrap(), 5000);
        // frozen payload 尾部 4B header：低 15 位 == FROZEN_COOKIE（roaring.h:7935）
        let header = u32::from_le_bytes(region[region.len() - 4..].try_into().unwrap());
        assert_eq!(header & 0x7FFF, 13766, "FROZEN_COOKIE");
        assert!((header >> 15) >= 1, "num_containers");
        assert!(len as u64 <= max_bitmap_len(5000));
    }
}
