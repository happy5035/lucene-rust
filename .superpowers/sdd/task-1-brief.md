### Task 1: 骨架——DocBlockBuf + trait 默认 fill + 开关 + driver + CountCollector

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（trait :17-41；SegmentDocIter impl :1514-1583）
- Modify: `crates/core/src/search/segment_reader.rs`（:131 之后追加）
- Modify: `crates/core/src/search/collector.rs`（trait :4-14；CountCollector :22-26）
- Modify: `crates/core/src/search/searcher.rs`（search :37-56；count :60-80）
- Modify: `crates/core/src/search/mod.rs`（:13 后声明测试模块）
- Create: `crates/core/src/search/block_tests.rs`

**Interfaces:**
- Consumes: 现有 `DocIter`（doc_id/next_doc/advance/freq/matches）、`SegmentDocIter` 14 变体、`Collector::collect`、`NO_MORE_DOCS`（`codec_lucene9::postings_read`）
- Produces: `DocBlockBuf`、`DocIter::next_block`（默认 fill）、`Collector::collect_block`（默认回退）、`CountCollector::collect_block` 覆写、`block_enabled()`、`drive_blocks()`——后续任务 2-7 全部基于这些名字

- [ ] **Step 1: 写失败测试——驱动级对拍骨架**

创建 `crates/core/src/search/block_tests.rs`：

```rust
//! 块路径对拍电池（spec §7）：block 流 == per-doc 流逐点一致。
//! block_enabled() 是进程级 OnceLock 不可在测试内翻转——对拍直调
//! drive_blocks vs per-doc 循环两个显式路径。

use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::roaring::MaterializedBitmap;

use super::collector::{CollectBlock, Collector, CountCollector};
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
```

在 `crates/core/src/search/mod.rs` 的 `pub use segment_reader::SegmentReader;` 行（:21）之后加：

```rust
#[cfg(test)]
mod block_tests;
```

注：测试 import 里的 `CollectBlock` 是笔误防线——实际只 import `Collector`；删掉 `CollectBlock`。最终 import 行为：

```rust
use super::collector::{Collector, CountCollector};
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p rustlucene-core block_ 2>&1 | tail -5`
Expected: 编译错误——`DocBlockBuf` / `next_block` / `drive_blocks` / `collect_block` 未定义

- [ ] **Step 3: doc_iter.rs 加 DOC_BLOCK / DocBlockBuf / trait 默认 fill**

在 `doc_iter.rs` 的 `use super::segment_reader::SegmentReader;`（:15）之后插入：

```rust
/// 块级迭代尺寸（spec 2026-07-26 §3）= PFOR PackedBlock 尺寸。
pub const DOC_BLOCK: usize = 128;

/// 调用方拥有的块填充缓冲（spec §3 偏差说明：填充式而非 §4b 原设计
/// 的借出式，避开跨 &mut self 生命周期纠缠）。freqs 仅产出 freq 的
/// 迭代器（DocsFreqsEnum 解码路径）填充；其余保持未定义，驱动方按
/// needs_freq 决定是否透传。
pub struct DocBlockBuf {
    pub docs: [u32; DOC_BLOCK],
    pub freqs: [u32; DOC_BLOCK],
    pub len: usize,
}

impl DocBlockBuf {
    pub fn new() -> DocBlockBuf {
        DocBlockBuf {
            docs: [0; DOC_BLOCK],
            freqs: [0; DOC_BLOCK],
            len: 0,
        }
    }
}
```

在 trait `DocIter` 的 `matches` 方法（:38-40）之后追加：

```rust
    /// 块级产出（spec 2026-07-26）：填充 out，返回产出数（0 = 耗尽，
    /// 此后永不再产块）。默认实现循环 next_doc()+matches() 填充——
    /// 两阶段确认在产出侧吸收，block driver 不再调 matches()。
    /// 覆写者契约：(1) out.docs[..n] 已 matches 过滤；(2) 块内严格升序；
    /// (3) 跨块单调递增；(4) 携带 freq 的迭代器同步填 out.freqs[..n]。
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut n = 0;
        while n < DOC_BLOCK {
            let d = self.next_doc()?;
            if d == NO_MORE_DOCS {
                break;
            }
            if !self.matches()? {
                continue;
            }
            out.docs[n] = d as u32;
            n += 1;
        }
        out.len = n;
        Ok(n)
    }
```

- [ ] **Step 4: SegmentDocIter 加 next_block 分发（14 臂）**

在 `impl DocIter for SegmentDocIter` 的 `matches` 方法（:1577-1582）之后追加：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        match self {
            // Docs/Freqs 是 codec 类型（无 DocIter impl）：Task 1 内联
            // 逐 doc 填充，Task 3 换成 codec 窗口批读。
            Self::Docs(d) => {
                let mut n = 0;
                while n < DOC_BLOCK {
                    let doc = d.next_doc()?;
                    if doc == NO_MORE_DOCS {
                        break;
                    }
                    out.docs[n] = doc as u32;
                    n += 1;
                }
                out.len = n;
                Ok(n)
            }
            Self::Freqs(f) => {
                let mut n = 0;
                while n < DOC_BLOCK {
                    let doc = f.next_doc()?;
                    if doc == NO_MORE_DOCS {
                        break;
                    }
                    out.docs[n] = doc as u32;
                    n += 1;
                }
                out.len = n;
                Ok(n)
            }
            Self::All(a) => a.next_block(out),
            Self::And(a) => a.next_block(out),
            Self::Or(o) => o.next_block(out),
            Self::Bitset(b) => b.next_block(out),
            Self::Phrase(p) => p.next_block(out),
            Self::Roaring(r) => r.next_block(out),
            Self::RoaringAnd(a) => a.next_block(out),
            Self::RoaringOr(o) => o.next_block(out),
            Self::Materialized(p) => p.next_block(out),
            Self::ConjOver(c) => c.next_block(out),
            Self::DisjOver(d) => d.next_block(out),
            Self::Excluding(e) => e.next_block(out),
        }
    }
```

- [ ] **Step 5: segment_reader.rs 加 block_enabled()**

在 `bitmap_enabled()`（:128-131）之后追加：

```rust
/// 块级迭代路径开关（spec 2026-07-26 §5）：默认 ON；`RL_BLOCK=0`
/// 三个 driver 逐行回到 per-doc 路径。镜像 bitmap_enabled() 的
/// OnceLock 形态——env 只作 runtime/bench 逃生门，测试直调显式
/// drive 函数对拍（spec §7-2）。
pub(crate) fn block_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("RL_BLOCK").map_or(true, |v| v != "0"))
}
```

- [ ] **Step 6: collector.rs 加 collect_block**

`Collector` trait 的 `needs_freq` 方法（:11-13）之后追加：

```rust
    /// 块级消费（spec 2026-07-26）：默认逐 doc 回退兜底外部 collector。
    /// `freqs = None` 时 freq 恒 1（needs_freq=false 驱动或无 freq 迭代器）。
    fn collect_block(&mut self, docs: &[u32], freqs: Option<&[u32]>) {
        match freqs {
            Some(f) => {
                for (i, &d) in docs.iter().enumerate() {
                    self.collect(d as i32, f[i]);
                }
            }
            None => {
                for &d in docs {
                    self.collect(d as i32, 1);
                }
            }
        }
    }
```

`CountCollector` 的 impl（:22-26）内 `collect` 之后追加覆写：

```rust
    fn collect_block(&mut self, docs: &[u32], _freqs: Option<&[u32]>) {
        self.count += docs.len() as u64;
    }
```

- [ ] **Step 7: searcher.rs 加 drive_blocks + search/count 分流**

import 行（:10-13）改为：

```rust
use super::collector::{Collector, FreqSumCollector};
use super::doc_iter::{DocBlockBuf, DocIter, SegmentDocIter};
use super::query::{self, Query};
use super::reader::Reader;
use super::segment_reader::block_enabled;
```

在 `impl Searcher` 块之前（:14 处）加自由函数：

```rust
/// 块驱动循环（spec §5）：每 128 docs 一次 enum 分发 + 一次 Result
/// 检查。matches() 由 next_block 产出侧吸收，此处不调。doc_base 整体
/// 加宽（Phase 2 可 SIMD）。pub(crate) 供 block_tests 直调对拍。
pub(crate) fn drive_blocks<C: Collector>(
    iter: &mut SegmentDocIter,
    doc_base: u32,
    needs_freq: bool,
    collector: &mut C,
) -> std::io::Result<()> {
    let mut out = DocBlockBuf::new();
    loop {
        let n = iter.next_block(&mut out)?;
        if n == 0 {
            break;
        }
        if doc_base != 0 {
            for d in out.docs[..n].iter_mut() {
                *d += doc_base;
            }
        }
        collector.collect_block(&out.docs[..n], needs_freq.then(|| &out.freqs[..n]));
    }
    Ok(())
}
```

`search` 方法（:37-56）的分段循环改为：

```rust
    pub fn search<C: Collector>(&mut self, query: &Query, collector: &mut C) -> io::Result<()> {
        let needs_freq = collector.needs_freq();
        for (doc_base, seg) in self.reader.leaves() {
            let Some(mut iter) = query.segment_iterator(seg, needs_freq)? else {
                continue;
            };
            if block_enabled() {
                drive_blocks(&mut iter, doc_base as u32, needs_freq, collector)?;
                continue;
            }
            loop {
                let doc = iter.next_doc()?;
                if doc == NO_MORE_DOCS {
                    break;
                }
                if !iter.matches()? {
                    continue;
                }
                let freq = if needs_freq { iter.freq() } else { 1 };
                collector.collect(doc_base + doc, freq);
            }
        }
        Ok(())
    }
```

`count` 方法（:60-80）的迭代回退分支改为：

```rust
            if let Some(mut iter) = query.segment_iterator(seg, false)? {
                if block_enabled() {
                    let mut out = DocBlockBuf::new();
                    loop {
                        let n = iter.next_block(&mut out)?;
                        if n == 0 {
                            break;
                        }
                        total += n as u64;
                    }
                    continue;
                }
                loop {
                    if iter.next_doc()? == NO_MORE_DOCS {
                        break;
                    }
                    if !iter.matches()? {
                        continue;
                    }
                    total += 1;
                }
            }
```

（`top_docs` 本任务不动——仍 per-doc，Task 7 加块路径；正确性不受影响。）

- [ ] **Step 8: 跑测试确认通过**

Run: `cargo test -p rustlucene-core block_ 2>&1 | tail -5`
Expected: 4 个 block_ 测试 PASS

Run: `cargo test --workspace 2>&1 | tail -3`
Expected: 全绿，273 + 4 = 277 passed（0 failed）

- [ ] **Step 9: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/search/segment_reader.rs \
  crates/core/src/search/collector.rs crates/core/src/search/searcher.rs \
  crates/core/src/search/mod.rs crates/core/src/search/block_tests.rs
git commit -m "$(cat <<'EOF'
feat(batch): 块迭代骨架——DocBlockBuf + next_block 默认 fill + RL_BLOCK 开关 + drive_blocks

spec 2026-07-26 §3/§5：trait 默认 fill 吸收两阶段确认，search/count
driver 块分流（top_docs Task 7），CountCollector::collect_block 覆写。
叶子/组合器覆写前全链路走默认 fill，先锁正确性。对拍电池 4 测试。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

