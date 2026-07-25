//! Owned in-memory `croaring::Bitmap` for materialized query hit sets
//! (M6 spec §3.3): the points read path materializes per-segment range
//! hits; croaring types stay inside the codec crate (M5 关键设计事实 7 —
//! core has no croaring dependency), exposing the same minimal surface
//! as `super::frozen::FrozenBitmap` (cardinality + docs_from batch reads).
//! No unsafe: `Bitmap::of` / `iter` are safe croaring APIs.

/// Owned materialized bitmap over ascending, deduplicated docs.
pub struct MaterializedBitmap {
    bm: croaring::Bitmap,
}

impl MaterializedBitmap {
    /// Bulk-build from ascending, deduplicated docs (`Bitmap::of` fast
    /// path requires sorted input — same contract as `write_term_bitmap`,
    /// roaring.rs:54-58).
    pub fn of(sorted_dedup_docs: &[u32]) -> MaterializedBitmap {
        MaterializedBitmap {
            bm: croaring::Bitmap::of(sorted_dedup_docs),
        }
    }

    pub fn cardinality(&self) -> u64 {
        self.bm.cardinality()
    }

    /// Batch ascending-doc read: fills `dst` with the first docs >=
    /// `from`, returns the count (0 = exhausted). Same semantics as
    /// `FrozenBitmap::docs_from` (frozen.rs:163-168): croaring
    /// `reset_at_or_after` + `next_many` include the current value.
    pub fn docs_from(&self, from: u32, dst: &mut [u32]) -> usize {
        let mut it = self.bm.iter();
        it.reset_at_or_after(from);
        it.next_many(dst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_iteration_and_seek() {
        let docs: Vec<u32> = (0..10_000u32).map(|i| i * 3).collect();
        let bm = MaterializedBitmap::of(&docs);
        assert_eq!(bm.cardinality(), docs.len() as u64);
        // 从头批量拉全量
        let mut buf = [0u32; 512];
        let mut got: Vec<u32> = Vec::new();
        let mut from = 0;
        loop {
            let n = bm.docs_from(from, &mut buf);
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
            from = got.last().unwrap() + 1;
        }
        assert_eq!(got, docs);
        // 落在缝隙上的 seek → 下一个存在的 doc
        let n = bm.docs_from(100, &mut buf);
        assert!(n > 0);
        assert_eq!(buf[0], 102);
        // 越过最大值 → 0
        assert_eq!(bm.docs_from(docs[docs.len() - 1] + 1, &mut buf), 0);
    }

    #[test]
    fn empty_bitmap() {
        let bm = MaterializedBitmap::of(&[]);
        assert_eq!(bm.cardinality(), 0);
        let mut buf = [0u32; 8];
        assert_eq!(bm.docs_from(0, &mut buf), 0);
    }
}
