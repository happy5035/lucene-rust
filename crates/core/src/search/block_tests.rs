//! 块路径对拍电池（spec §7）：block 流 == per-doc 流逐点一致。
//! block_enabled() 是进程级 OnceLock 不可在测试内翻转——对拍直调
//! drive_blocks vs per-doc 循环两个显式路径。

use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::roaring::MaterializedBitmap;

use super::collector::{Collector, CountCollector};
#[allow(unused_imports)] // DOC_BLOCK reserved for Task 2+ block helpers
use super::doc_iter::{DocBlockBuf, DocIter, MaterializedDocIter, SegmentDocIter, DOC_BLOCK};
use super::searcher::drive_blocks;

/// 小步长 LCG（core 无 rand dev-dep；测试专用，勿用于生产）。
struct Lcg(u64);
impl Lcg {
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    /// 升序去重 doc 集：density 控制命中率，universe 内随机选取。
    fn doc_set(&mut self, universe: u32, count: usize) -> Vec<u32> {
        let mut v: Vec<u32> = Vec::new();
        while v.len() < count {
            let d = self.next_u32() % universe;
            if v.last() != Some(&d) {
                v.push(d);
            }
        }
        v.sort_unstable();
        v.dedup();
        v
    }
}

pub(super) fn leaf(docs: &[u32]) -> SegmentDocIter {
    SegmentDocIter::Materialized(MaterializedDocIter::new(MaterializedBitmap::of(docs)))
}

#[allow(dead_code)] // reserved for Task 8 per-doc vs block parity battery
pub(super) fn stream_per_doc(it: &mut SegmentDocIter) -> Vec<u32> {
    let mut v = Vec::new();
    loop {
        let d = it.next_doc().unwrap();
        if d == NO_MORE_DOCS {
            break;
        }
        if it.matches().unwrap() {
            v.push(d as u32);
        }
    }
    v
}

pub(super) fn stream_block(it: &mut SegmentDocIter) -> Vec<u32> {
    let mut v = Vec::new();
    let mut buf = DocBlockBuf::new();
    loop {
        let n = it.next_block(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        v.extend_from_slice(&buf.docs[..n]);
    }
    v
}

/// 记录每个 doc 的 VecCollector（collect_block 默认回退路径验证）。
#[derive(Default)]
struct VecCollector {
    docs: Vec<i32>,
    freqs: Vec<u32>,
}
impl Collector for VecCollector {
    fn collect(&mut self, doc: i32, freq: u32) {
        self.docs.push(doc);
        self.freqs.push(freq);
    }
}

#[test]
fn default_fill_tail_semantics() {
    // 默认 fill 在 127/128/129/255/256 doc 集上的尾块语义
    for n in [0usize, 1, 127, 128, 129, 255, 256, 1000] {
        let docs: Vec<u32> = (0..n as u32).map(|i| i * 2).collect();
        let mut it = leaf(&docs);
        assert_eq!(stream_block(&mut it), docs, "n={n}");
    }
}

#[test]
fn drive_blocks_count_parity_single_segment() {
    // drive_blocks（块驱动）vs per-doc 循环，CountCollector 计数一致
    let mut lcg = Lcg(42);
    let docs = lcg.doc_set(100_000, 5_000);
    let mut it = leaf(&docs);
    let mut count = CountCollector::default();
    drive_blocks(&mut it, 0, false, &mut count).unwrap();
    assert_eq!(count.count, docs.len() as u64);

    let mut it2 = leaf(&docs);
    let mut per_doc = 0u64;
    loop {
        if it2.next_doc().unwrap() == NO_MORE_DOCS {
            break;
        }
        if it2.matches().unwrap() {
            per_doc += 1;
        }
    }
    assert_eq!(count.count, per_doc);
}

#[test]
fn drive_blocks_doc_base_offset() {
    let docs: Vec<u32> = (0..300).map(|i| i * 3).collect();
    let mut it = leaf(&docs);
    let mut vc = VecCollector::default();
    drive_blocks(&mut it, 1000, false, &mut vc).unwrap();
    let expect: Vec<i32> = docs.iter().map(|&d| d as i32 + 1000).collect();
    assert_eq!(vc.docs, expect);
    assert!(vc.freqs.iter().all(|&f| f == 1));
}

#[test]
fn collect_block_default_matches_per_doc() {
    // collect_block 默认实现 == 逐 doc collect（外部 collector 兜底契约）
    let mut vc = VecCollector::default();
    let docs = [1u32, 5, 9, 200];
    vc.collect_block(&docs, None);
    assert_eq!(vc.docs, vec![1, 5, 9, 200]);
    assert_eq!(vc.freqs, vec![1, 1, 1, 1]);
    let mut vc2 = VecCollector::default();
    vc2.collect_block(&docs, Some(&[2, 3, 4, 5]));
    assert_eq!(vc2.freqs, vec![2, 3, 4, 5]);
}
