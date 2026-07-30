//! 块路径对拍电池（spec §7）：block 流 == per-doc 流逐点一致。
//! block_enabled() 是进程级 OnceLock 不可在测试内翻转——对拍直调
//! drive_blocks vs per-doc 循环两个显式路径。

use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::roaring::MaterializedBitmap;

use super::collector::{Collector, CountCollector, FreqSumCollector, TopDocCollector};
#[allow(unused_imports)] // DOC_BLOCK reserved for Task 2+ block helpers
use super::doc_iter::{
    DocBlockBuf, DocIter, MatchAllIter, MaterializedDocIter, SegmentDocIter, DOC_BLOCK,
};
use super::searcher::drive_blocks;

/// 小步长 LCG（core 无 rand dev-dep；测试专用，勿用于生产）。
struct Lcg(u64);
impl Lcg {
    fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
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

#[test]
fn matchall_block_stream() {
    for max in [0i32, 1, 127, 128, 129, 300, 1000] {
        let mut it = SegmentDocIter::All(MatchAllIter::new(max));
        let expect: Vec<u32> = (0..max as u32).collect();
        assert_eq!(stream_block(&mut it), expect, "max={max}");
    }
}

#[test]
fn leaves_block_vs_per_doc_random() {
    let mut lcg = Lcg(7);
    for _ in 0..20 {
        let n = (lcg.next_u32() % 600) as usize;
        let docs = lcg.doc_set(50_000, n);
        // Materialized 叶子
        let mut a = leaf(&docs);
        let mut b = leaf(&docs);
        assert_eq!(stream_block(&mut a), stream_per_doc(&mut b), "mat n={n}");
    }
}

use super::doc_iter::{
    block_andnot, block_intersect, kway_union, ConjOverDocIter, DisjOverDocIter, DocSource,
    ExcludingDocIter, RoaringOrDocIter,
};

fn run(
    f: impl Fn(&[u32], &[u32], &mut [u32]) -> (usize, usize, usize),
    a: &[u32],
    b: &[u32],
) -> Vec<u32> {
    // 分片消费至耗尽（模拟组合器跨调用状态机）
    let mut out = vec![0u32; 128];
    let mut res = Vec::new();
    let (mut pa, mut pb) = (0, 0);
    loop {
        let (ca, cb, n) = f(&a[pa..], &b[pb..], &mut out);
        res.extend_from_slice(&out[..n]);
        pa += ca;
        pb += cb;
        if n == 0 || (pa == a.len() && (ca == 0 || cb == 0 && pb == b.len())) {
            break;
        }
        if ca == 0 && cb == 0 {
            break;
        }
    }
    res
}

fn expect_intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    a.iter().filter(|x| b.contains(x)).copied().collect()
}

fn expect_andnot(a: &[u32], b: &[u32]) -> Vec<u32> {
    a.iter().filter(|x| !b.contains(x)).copied().collect()
}

#[test]
fn algebra_quadrants() {
    let cases: Vec<(Vec<u32>, Vec<u32>)> = vec![
        (vec![], vec![]),
        (vec![1, 2, 3], vec![]),
        (vec![], vec![1, 2, 3]),
        (vec![1, 3, 5], vec![2, 4, 6]),    // 不相交
        (vec![1, 2, 3], vec![1, 2, 3]),    // 全等
        (vec![2, 4], vec![1, 2, 3, 4, 5]), // 包含
        ((0..300).step_by(2).collect(), (0..300).step_by(3).collect()), // 交错跨块
        ((0..128).collect(), (0..256).collect()), // 128 整数倍
        ((0..127).collect(), (0..129).collect()), // 尾块
    ];
    for (a, b) in cases {
        assert_eq!(
            run(block_intersect, &a, &b),
            expect_intersect(&a, &b),
            "intersect {a:?} {b:?}"
        );
        assert_eq!(
            run(block_andnot, &a, &b),
            expect_andnot(&a, &b),
            "andnot {a:?} {b:?}"
        );
        // andnot 反对称
        assert_eq!(
            run(block_andnot, &b, &a),
            expect_andnot(&b, &a),
            "andnot rev {a:?} {b:?}"
        );
    }
}

#[test]
fn kway_union_dedup_and_order() {
    let mut lcg = Lcg(99);
    for _ in 0..30 {
        let k = 2 + (lcg.next_u32() % 5) as usize;
        let mut sets: Vec<Vec<u32>> = Vec::with_capacity(k);
        for _ in 0..k {
            let cnt = (lcg.next_u32() % 400) as usize;
            sets.push(lcg.doc_set(3_000, cnt));
        }
        // 期望 = 并集去重升序
        let mut expect: Vec<u32> = sets.iter().flatten().copied().collect();
        expect.sort_unstable();
        expect.dedup();
        // 分片消费
        let mut got = Vec::new();
        let mut pos = vec![0usize; k];
        let mut out = vec![0u32; 64]; // 故意 <128 触发多次归并
        loop {
            let heads: Vec<&[u32]> = sets.iter().enumerate().map(|(i, s)| &s[pos[i]..]).collect();
            let mut consumed = vec![0usize; k];
            let n = kway_union(&heads, &mut consumed, &mut out, u32::MAX);
            got.extend_from_slice(&out[..n]);
            for i in 0..k {
                pos[i] += consumed[i];
            }
            if n == 0 {
                break;
            }
        }
        assert_eq!(got, expect, "k={k}");
    }
}

#[test]
fn algebra_consumed_coordinates_partial_fill() {
    // out 容量小于结果集：消费坐标必须停在"最后参与元素"处，
    // 调用方凭此续跑不丢不重（I-1：直钉坐标，不靠全流装配间接验证）。
    let a = [1u32, 2, 3, 4, 5, 6];
    let b = [2u32, 4, 6, 8];
    let mut out = [0u32; 2];

    // intersect：out=[2,4]，a 消费到 4（含），b 消费到 4（含）
    let (ca_i, cb_i, n_i) = block_intersect(&a, &b, &mut out);
    assert_eq!(&out[..n_i], &[2, 4]);
    assert_eq!((ca_i, cb_i, n_i), (4, 2, 2));

    // andnot：a\b = [1,3,5,...]，out=[1,3]，a 消费 3 个（ia 在 emit 后
    // ++ 到 3），b 消费到 <4 处（即 1 个：2）—— 实现值与 brief 不同，
    // 见 task-4-report Fix I-1 偏差说明。
    let (ca_a, cb_a, n_a) = block_andnot(&a, &b, &mut out);
    assert_eq!(&out[..n_a], &[1, 3]);
    assert_eq!((ca_a, cb_a, n_a), (3, 1, 2));

    // 续跑验证：从 intersect 消费坐标续跑拼出完整结果，不丢不重
    let (ca2, cb2, n2) = block_intersect(&a[ca_i..], &b[cb_i..], &mut out);
    let mut full: Vec<u32> = vec![2, 4];
    full.extend_from_slice(&out[..n2]);
    assert_eq!(full, vec![2, 4, 6]);
    let _ = (ca2, cb2);
}

fn conj_over(children: Vec<Vec<u32>>) -> SegmentDocIter {
    let subs: Vec<SegmentDocIter> = children.iter().map(|c| leaf(c)).collect();
    SegmentDocIter::ConjOver(ConjOverDocIter::new(subs).unwrap())
}

fn disj_over(children: Vec<Vec<u32>>) -> SegmentDocIter {
    let subs: Vec<SegmentDocIter> = children.iter().map(|c| leaf(c)).collect();
    SegmentDocIter::DisjOver(DisjOverDocIter::new(subs).unwrap())
}

fn excl(main: Vec<u32>, prohibited: Vec<u32>) -> SegmentDocIter {
    SegmentDocIter::Excluding(ExcludingDocIter::new(leaf(&main), leaf(&prohibited)))
}

fn expect_conj(sets: &[Vec<u32>]) -> Vec<u32> {
    let mut r: Vec<u32> = sets[0].clone();
    for s in &sets[1..] {
        r.retain(|x| s.contains(x));
    }
    r
}

fn expect_disj(sets: &[Vec<u32>]) -> Vec<u32> {
    let mut r: Vec<u32> = sets.iter().flatten().copied().collect();
    r.sort_unstable();
    r.dedup();
    r
}

#[test]
fn combinator_block_vs_per_doc_random() {
    let mut lcg = Lcg(1234);
    for round in 0..40 {
        let k = 2 + (lcg.next_u32() % 3) as usize;
        let sets: Vec<Vec<u32>> = (0..k)
            .map(|_| {
                let cnt = (lcg.next_u32() % 500) as usize;
                lcg.doc_set(4_000, cnt)
            })
            .collect();
        // ConjOver
        let mut a = conj_over(sets.clone());
        let mut b = conj_over(sets.clone());
        assert_eq!(
            stream_block(&mut a),
            stream_per_doc(&mut b),
            "conj round={round}"
        );
        assert_eq!(
            stream_block(&mut conj_over(sets.clone())),
            expect_conj(&sets)
        );
        // DisjOver
        let mut a = disj_over(sets.clone());
        let mut b = disj_over(sets.clone());
        assert_eq!(
            stream_block(&mut a),
            stream_per_doc(&mut b),
            "disj round={round}"
        );
        assert_eq!(
            stream_block(&mut disj_over(sets.clone())),
            expect_disj(&sets)
        );
        // Excluding
        let (m, p) = (sets[0].clone(), sets[1].clone());
        let expect: Vec<u32> = m.iter().filter(|x| !p.contains(x)).copied().collect();
        let mut a = excl(m.clone(), p.clone());
        let mut b = excl(m.clone(), p.clone());
        assert_eq!(
            stream_block(&mut a),
            stream_per_doc(&mut b),
            "excl round={round}"
        );
        assert_eq!(stream_block(&mut excl(m, p)), expect);
    }
}

#[test]
fn combinator_edge_shapes() {
    // 空交 / 空集子句 / 128 整数倍 / 全等 / 尾块
    let e: Vec<u32> = vec![];
    assert_eq!(
        stream_block(&mut conj_over(vec![vec![1, 2], e.clone()])),
        Vec::<u32>::new()
    );
    assert_eq!(
        stream_block(&mut disj_over(vec![e.clone(), vec![3]])),
        vec![3]
    );
    let full: Vec<u32> = (0..256).collect();
    assert_eq!(stream_block(&mut excl(full.clone(), e.clone())), full);
    assert_eq!(
        stream_block(&mut excl(e.clone(), full.clone())),
        Vec::<u32>::new()
    );
    let a: Vec<u32> = (0..300).step_by(2).collect();
    let b: Vec<u32> = (0..300).step_by(3).collect();
    let expect: Vec<u32> = a.iter().filter(|x| !b.contains(x)).copied().collect();
    assert_eq!(stream_block(&mut excl(a, b)), expect);
}
#[test]
fn disj_debug_round0() {
    // Reproduce round 0 of combinator_block_vs_per_doc_random
    let mut lcg = Lcg(1234);
    let k = 2 + (lcg.next_u32() % 3) as usize;
    let sets: Vec<Vec<u32>> = (0..k)
        .map(|_| {
            let cnt = (lcg.next_u32() % 500) as usize;
            lcg.doc_set(4_000, cnt)
        })
        .collect();
    eprintln!("k={k}");
    for (i, s) in sets.iter().enumerate() {
        eprintln!(
            "  child[{i}]: len={} first10={:?} last5={:?}",
            s.len(),
            &s[..s.len().min(10)],
            &s[s.len().saturating_sub(5)..]
        );
    }
    let expect = expect_disj(&sets);
    eprintln!("expect: len={}", expect.len());

    // stream_block with tracing
    let mut it = disj_over(sets.clone());
    let mut v = Vec::new();
    let mut buf = super::doc_iter::DocBlockBuf::new();
    let mut call = 0;
    loop {
        let n = it.next_block(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        let block: Vec<u32> = buf.docs[..n].to_vec();
        let is_sorted = block.windows(2).all(|w| w[0] <= w[1]);
        let cross_ok = v.last().map_or(true, |&last| block[0] > last);
        eprintln!(
            "  block[{call}]: n={n} sorted={is_sorted} cross_ok={cross_ok} first={} last={}",
            block[0],
            block[n - 1]
        );
        if !is_sorted || !cross_ok {
            eprintln!("    FAILING BLOCK: {block:?}");
            if let Some(&last) = v.last() {
                eprintln!("    prev_last={last}");
            }
        }
        v.extend_from_slice(&buf.docs[..n]);
        call += 1;
    }
    assert_eq!(v, expect);
}

#[test]
fn roaring_or_slice_sources_block_vs_per_doc() {
    let mut lcg = Lcg(5678);
    for round in 0..30 {
        let k = 2 + (lcg.next_u32() % 4) as usize;
        let sets: Vec<Vec<u32>> = (0..k)
            .map(|_| {
                let cnt = (lcg.next_u32() % 700) as usize;
                lcg.doc_set(8_000, cnt)
            })
            .collect();
        let mk = || {
            let sources: Vec<DocSource> =
                sets.iter().map(|s| DocSource::slice(s.clone())).collect();
            SegmentDocIter::RoaringOr(RoaringOrDocIter::new(sources))
        };
        let mut a = mk();
        let mut b = mk();
        assert_eq!(
            stream_block(&mut a),
            stream_per_doc(&mut b),
            "round={round}"
        );
        assert_eq!(stream_block(&mut mk()), expect_disj(&sets));
    }
}

#[test]
fn top_doc_collector_block_per_doc_parity() {
    for top_n in [1usize, 5, 128, 129, 300, 1000] {
        let docs: Vec<u32> = (0..300).map(|i| i * 7).collect();
        let mut ref_c = TopDocCollector::new(top_n);
        for &d in &docs {
            ref_c.collect(d as i32, 1);
        }
        let mut blk_c = TopDocCollector::new(top_n);
        for chunk in docs.chunks(128) {
            blk_c.collect_block(chunk, None);
        }
        assert_eq!(blk_c.total, ref_c.total, "top_n={top_n}");
        assert_eq!(blk_c.docs, ref_c.docs, "top_n={top_n}");
        assert_eq!(blk_c.docs.len(), top_n.min(docs.len()));
    }
}

#[test]
fn freq_sum_collector_block() {
    let mut c = FreqSumCollector::default();
    let docs = [1u32, 2, 3];
    c.collect_block(&docs, Some(&[4, 5, 6]));
    assert_eq!(c.total_freq, 15);
    let mut c2 = FreqSumCollector::default();
    c2.collect_block(&docs, None);
    assert_eq!(c2.total_freq, 3);
}
