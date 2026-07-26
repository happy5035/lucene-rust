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

    /// 空集（增量构造入口，P1-3）。
    pub fn empty() -> MaterializedBitmap {
        MaterializedBitmap {
            bm: croaring::Bitmap::new(),
        }
    }

    /// 单 doc 增量插入：无序、去重天然幂等（P1-3：PointRange 物化
    /// 去掉 Vec+sort+dedup——BKD 访问序是 (value, doc) 非 doc 序，
    /// 旧路径排序开销 ~10ns/doc，容器级插入 ~5ns/doc 且多值点去重
    /// 由 bitmap 语义天然承担）。
    pub fn add(&mut self, doc: u32) {
        self.bm.add(doc);
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

    /// Owned 构造（codec 内部：frozen 容器级拷贝与 set ops 的出口）。
    pub(crate) fn from_bitmap(bm: croaring::Bitmap) -> MaterializedBitmap {
        MaterializedBitmap { bm }
    }

    /// 全位 bitmap [0, max_doc)（M7 §3.1：Bool 内嵌纯 MUST_NOT 子树的
    /// MatchAll 正集防御）。
    pub fn full(max_doc: u32) -> MaterializedBitmap {
        let mut bm = croaring::Bitmap::new();
        bm.add_range(0..max_doc);
        MaterializedBitmap { bm }
    }

    /// 容器级交（M7 §3.1 fold 原语）：新分配 owned 结果。
    pub fn and(&self, other: &MaterializedBitmap) -> MaterializedBitmap {
        MaterializedBitmap {
            bm: self.bm.and(&other.bm),
        }
    }

    /// 容器级并。
    pub fn or(&self, other: &MaterializedBitmap) -> MaterializedBitmap {
        MaterializedBitmap {
            bm: self.bm.or(&other.bm),
        }
    }

    /// 容器级差（MUST_NOT 排除）。
    pub fn andnot(&self, other: &MaterializedBitmap) -> MaterializedBitmap {
        MaterializedBitmap {
            bm: self.bm.andnot(&other.bm),
        }
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

    #[test]
    fn incremental_add_matches_bulk_of() {
        // P1-3：乱序 + 重复的增量 add 与升序 of 结果逐点一致
        // （PointRange 物化路径的正确性契约）。
        let mut bm = MaterializedBitmap::empty();
        let docs: Vec<u32> = [7, 3, 100, 3, 7, 42, 100, 0, 9999, 42].to_vec();
        for d in &docs {
            bm.add(*d);
        }
        let mut sorted: Vec<u32> = docs;
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(bm.cardinality(), sorted.len() as u64);
        let reference = MaterializedBitmap::of(&sorted);
        let mut buf = [0u32; 8];
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let mut from = 0;
        loop {
            let n = bm.docs_from(from, &mut buf);
            if n == 0 {
                break;
            }
            a.extend_from_slice(&buf[..n]);
            from = a.last().unwrap() + 1;
        }
        let mut from = 0;
        loop {
            let n = reference.docs_from(from, &mut buf);
            if n == 0 {
                break;
            }
            b.extend_from_slice(&buf[..n]);
            from = b.last().unwrap() + 1;
        }
        assert_eq!(a, b);
        assert_eq!(a, sorted);
    }

    #[test]
    fn set_ops_full_and_andnot() {
        let a = MaterializedBitmap::of(&[1, 2, 3, 5, 8]);
        let b = MaterializedBitmap::of(&[2, 3, 4, 8, 13]);
        let inter = a.and(&b);
        assert_eq!(inter.cardinality(), 3); // {2,3,8}
        let uni = a.or(&b);
        assert_eq!(uni.cardinality(), 7); // {1,2,3,4,5,8,13}
        let diff = a.andnot(&b);
        assert_eq!(diff.cardinality(), 2); // {1,5}
        let mut buf = [0u32; 8];
        let n = diff.docs_from(0, &mut buf);
        assert_eq!(&buf[..n], [1, 5]);
        // full：0..max_doc 全位
        let full = MaterializedBitmap::full(100);
        assert_eq!(full.cardinality(), 100);
        let n = full.docs_from(98, &mut buf);
        assert_eq!(&buf[..n], [98, 99]);
        // 与空集运算
        let empty = MaterializedBitmap::of(&[]);
        assert_eq!(a.and(&empty).cardinality(), 0);
        assert_eq!(a.or(&empty).cardinality(), 5);
        assert_eq!(a.andnot(&empty).cardinality(), 5);
        assert_eq!(empty.andnot(&a).cardinality(), 0);
    }
}
