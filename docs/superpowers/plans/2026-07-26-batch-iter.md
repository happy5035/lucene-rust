# 批量迭代（块级 DocIter）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 `DocIter` 加 `next_block`（128-doc 填充式块接口），组合器走标量块代数，driver 每 128 docs 一次分发，量化全量迭代提速（demo 实测 15.5–18.8×），并在 5M 单段索引上三路三模式出 bench 报告。

**Architecture:** trait 加默认 fill 的 `next_block`（所有迭代器自动块可用，phrase 两阶段在 fill 内吸收）；叶子覆写真块产出（codec `EnumCore::next_docs` 窗口直拷、`BitmapCursor::next_many_to`）；组合器覆写共享代数（`block_intersect` / `block_andnot` / `kway_union`）；`RL_BLOCK` OnceLock 开关，per-doc 路径一字不动。

**Tech Stack:** Rust（core + codec-lucene9 两 crate）、cargo test、rustc -O demo（`/tmp/blockdemo/demo.rs`）、searchbench/SearchBench bench 管线、perf（有 PMU 时）。

## Global Constraints

来自 spec（`docs/superpowers/specs/2026-07-26-rust-batch-iter-design.md`，commit 738ed91）§0，逐字生效于每个任务：

1. per-doc 路径逐行不变：`RL_BLOCK=0` 回到今天行为，一行不动
2. 命中集合与 per-doc 路径**逐位一致**（顺序、计数、topN docs 列表）
3. P1-1 / P1-3 战果零回归（1M 电池重点行 ±10% 噪声内）
4. 默认 ON 影响所有消费方（CLI + JNI）→ 四路对账 + 逃生门兜底

**命名契约**（跨任务一致，勿改名）：

| 名字 | 位置 | 签名 |
|---|---|---|
| `DOC_BLOCK` | `doc_iter.rs` | `pub const DOC_BLOCK: usize = 128;` |
| `DocBlockBuf` | `doc_iter.rs` | `pub struct { docs: [u32; DOC_BLOCK], freqs: [u32; DOC_BLOCK], len: usize }` + `pub fn new()` |
| `DocIter::next_block` | `doc_iter.rs` trait | `fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize>`（默认 fill） |
| `Collector::collect_block` | `collector.rs` trait | `fn collect_block(&mut self, docs: &[u32], freqs: Option<&[u32]>)`（默认 per-doc） |
| `block_enabled` | `segment_reader.rs` | `pub(crate) fn block_enabled() -> bool`（`RL_BLOCK`，默认 true） |
| `drive_blocks` | `searcher.rs` | `pub(crate) fn drive_blocks<C: Collector>(iter: &mut SegmentDocIter, doc_base: u32, needs_freq: bool, collector: &mut C) -> io::Result<()>` |
| `EnumCore::next_docs` | codec `postings_read.rs` | `fn next_docs(&mut self, docs: &mut [u32], freqs: Option<&mut [u32]>) -> io::Result<usize>` |
| `DocsEnum::next_docs` | codec | `pub fn next_docs(&mut self, docs: &mut [u32]) -> io::Result<usize>` |
| `DocsFreqsEnum::next_docs_and_freqs` | codec | `pub fn next_docs_and_freqs(&mut self, docs: &mut [u32], freqs: &mut [u32]) -> io::Result<usize>` |
| `DocsFreqsEnum::decodes_freqs` | codec | `pub fn decodes_freqs(&self) -> bool` |
| `BitmapCursor::next_many_to` | `doc_iter.rs` | `fn next_many_to(&mut self, dst: &mut [u32]) -> usize` |
| `DocSource::next_docs` | `doc_iter.rs` | `fn next_docs(&mut self, dst: &mut [u32]) -> usize` |
| `block_intersect` / `block_andnot` | `doc_iter.rs` | `fn (a: &[u32], b: &[u32], out: &mut [u32]) -> (usize, usize, usize)`（消费a, 消费b, 产出） |
| `kway_union` | `doc_iter.rs` | `fn kway_union(heads: &[&[u32]], consumed: &mut [usize], out: &mut [u32]) -> usize` |
| `BlockCursor` | `doc_iter.rs` | `struct BlockCursor { buf: Box<DocBlockBuf>, pos: usize, len: usize }` + `refill`/`remaining`/`consume` |

**测试纪律**：每任务 `cargo test --workspace` 全绿（基线 273：core 88 + codec 185）；对拍测试直调 `drive_blocks` vs per-doc 循环（`block_enabled()` 是进程级 OnceLock，测试内不可翻转——env 开关只作 runtime 逃生门）。

**提交纪律**：每任务一个 commit，中文 subject，尾部 `Co-Authored-By: Claude <noreply@anthropic.com>`。

## File Structure

| 文件 | 改动 | 责任 |
|---|---|---|
| `crates/core/src/search/doc_iter.rs` | 改 | DOC_BLOCK/DocBlockBuf、trait next_block 默认 fill、叶子覆写、共享代数、组合器覆写、SegmentDocIter 14 臂 next_block 分发 |
| `crates/core/src/search/segment_reader.rs` | 改 | `block_enabled()`（镜像 `bitmap_enabled()` :128-131） |
| `crates/core/src/search/collector.rs` | 改 | `collect_block` 默认 + 三 collector 覆写 |
| `crates/core/src/search/searcher.rs` | 改 | `drive_blocks` + search/count/top_docs 块分流 |
| `crates/core/src/search/mod.rs` | 改 | 声明 `#[cfg(test)] mod block_tests;` |
| `crates/core/src/search/block_tests.rs` | 新建 | 引擎级对拍测试（驱动 + 组合器 + collector） |
| `crates/codec-lucene9/src/postings_read.rs` | 改 | `EnumCore::next_docs` 窗口批读 + DocsEnum/DocsFreqsEnum 包装 + codec 对拍测试 |
| `docs/bool-bench-report.md` | 改 | §12：5M 单段 block on/off 结果 + Phase 2 决策备忘 |

---

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

### Task 2: codec 窗口批读——EnumCore::next_docs

**Files:**
- Modify: `crates/codec-lucene9/src/postings_read.rs`（EnumCore impl :245-630 区间；DocsEnum :656-672；DocsFreqsEnum :675-698；tests mod :791+）

**Interfaces:**
- Consumes: `EnumCore` 私有状态（`doc_buffer: [u64; BLOCK_SIZE+1]`、`doc_buffer_upto`、`freq_buffer`、`level0_last_doc`、`move_to_next_level0_block()`、NO_MORE_DOCS 哨兵）
- Produces: `EnumCore::next_docs`、`DocsEnum::next_docs`、`DocsFreqsEnum::{next_docs, next_docs_and_freqs, decodes_freqs}`——Task 3 的 `SegmentDocIter::Docs/Freqs` 覆写依赖这些

- [ ] **Step 1: 写失败测试——codec 批读对拍**

在 `postings_read.rs` 的 `mod tests` 尾部追加（helper `write_segment`/`seek`/`temp_dir` 已存在 :800-860）：

```rust
    /// 批读 vs 逐 doc 全量对拍：kw:big（df=200 稠密）/ kw:tail（df=3 尾块）/
    /// tx:hot（df=5000，跨 level-1 边界 4096）/ tx:warm（df=200 步长3 +
    /// freq 异常值）/ tx:one（singleton）。多种 dst 尺寸含 1（退化）与
    /// 4096（超 level-1 组）。
    fn drain_next_docs(en: &mut DocsEnum, step: usize) -> Vec<u32> {
        let mut docs = Vec::new();
        let mut buf = vec![0u32; step];
        loop {
            let n = en.next_docs(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            docs.extend_from_slice(&buf[..n]);
        }
        docs
    }

    fn drain_per_doc(en: &mut DocsEnum) -> Vec<u32> {
        let mut docs = Vec::new();
        loop {
            let d = en.next_doc().unwrap();
            if d == NO_MORE_DOCS {
                break;
            }
            docs.push(d as u32);
        }
        docs
    }

    #[test]
    fn next_docs_matches_next_doc_all_terms() {
        let dir = temp_dir("nextdocs");
        fs::create_dir_all(&dir).unwrap();
        let fsdir = FSDirectory::open(&dir).unwrap();
        let (fis, warm_docs, warm_freqs) = write_segment(&fsdir);
        let reader = PostingsReader::open(&fsdir, "_0", &[4u8; 16]).unwrap();

        // (field, term, expect_docs, expect_freqs)
        let big: Vec<u32> = (0..200).collect();
        let hot: Vec<u32> = (0..5000).collect();
        let cases: Vec<(&str, &[u8], Vec<u32>, Option<Vec<u32>>)> = vec![
            ("kw", b"big", big.clone(), None),
            ("kw", b"tail", vec![10, 20, 30], None),
            ("tx", b"hot", hot, Some(vec![1; 5000])),
            ("tx", b"warm", warm_docs, Some(warm_freqs)),
            ("tx", b"one", vec![42], Some(vec![7])),
        ];
        for (field, term, expect_docs, expect_freqs) in cases {
            let entry = seek(&fsdir, &fis, field, term);
            for step in [1usize, 7, 128, 200, 4096] {
                let mut en = reader.docs(&entry).unwrap();
                assert_eq!(drain_next_docs(&mut en, step), expect_docs,
                    "{field}:{term:?} step={step} docs");
            }
            // 逐 doc 参照路径同集
            let mut en = reader.docs(&entry).unwrap();
            assert_eq!(drain_per_doc(&mut en), expect_docs);
            // freqs 对拍（仅 has_freqs 字段）
            if let Some(expect_f) = expect_freqs {
                let mut en = reader.docs_and_freqs(&entry).unwrap();
                assert!(en.decodes_freqs());
                let mut docs = Vec::new();
                let mut freqs = Vec::new();
                let (mut db, mut fb) = (vec![0u32; 64], vec![0u32; 64]);
                loop {
                    let n = en.next_docs_and_freqs(&mut db, &mut fb).unwrap();
                    if n == 0 {
                        break;
                    }
                    docs.extend_from_slice(&db[..n]);
                    freqs.extend_from_slice(&fb[..n]);
                }
                assert_eq!(docs, expect_docs, "{field}:{term:?} freq-mode docs");
                assert_eq!(freqs, expect_f, "{field}:{term:?} freqs");
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_freq_enum_does_not_decode_freqs() {
        let dir = temp_dir("nofreqbatch");
        fs::create_dir_all(&dir).unwrap();
        let fsdir = FSDirectory::open(&dir).unwrap();
        let (fis, warm_docs, _) = write_segment(&fsdir);
        let reader = PostingsReader::open(&fsdir, "_0", &[4u8; 16]).unwrap();
        let entry = seek(&fsdir, &fis, "tx", b"warm");
        let mut en = reader.docs_and_freqs_no_freq(&entry).unwrap();
        assert!(!en.decodes_freqs());
        assert_eq!(drain_next_docs_enum(&mut en, 128), warm_docs);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn drain_next_docs_enum(en: &mut DocsFreqsEnum, step: usize) -> Vec<u32> {
        let mut docs = Vec::new();
        let mut buf = vec![0u32; step];
        loop {
            let n = en.next_docs(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            docs.extend_from_slice(&buf[..n]);
        }
        docs
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p codec-lucene9 next_docs 2>&1 | tail -5`
Expected: 编译错误——`next_docs` / `decodes_freqs` / `next_docs_and_freqs` 未定义

- [ ] **Step 3: EnumCore::next_docs 实现**

在 `impl EnumCore` 块内 `next_doc` 方法（:295-309）之后插入：

```rust
    /// 批量版 next_doc（spec 2026-07-26 Task 2）：仅 docs / docs+freqs
    /// profile（pos.is_none()）——把 doc_buffer 里已解码的绝对 doc 窗口
    /// 直接拷出，跨 128-block 边界透明 refill。freqs = Some 时同步拷
    /// freq_buffer 同窗口（调用方保证 decode_freqs）。EverythingEnum 的
    /// position 簿记不走这里——PositionsEnum 保持逐 doc（phrase 两阶段）。
    /// 返回 0 = 耗尽。
    fn next_docs(&mut self, docs: &mut [u32], mut freqs: Option<&mut [u32]>) -> io::Result<usize> {
        debug_assert!(self.pos.is_none());
        let mut n = 0;
        while n < docs.len() {
            if self.doc == NO_MORE_DOCS as i64 {
                break;
            }
            if self.doc == self.level0_last_doc {
                self.move_to_next_level0_block()?;
            }
            let upto = self.doc_buffer_upto;
            // 窗口 = 当前缓冲到哨兵（NO_MORE_DOCS 占位）或 dst 填满
            let mut take = 0;
            while take < docs.len() - n
                && self.doc_buffer[upto + take] != NO_MORE_DOCS as u64
            {
                take += 1;
            }
            if take == 0 {
                // 缓冲首槽即哨兵（df 恰为 128 倍数后的空 refill）：
                // 镜像 next_doc 读哨兵一步，置耗尽态。
                self.doc = self.doc_buffer[upto] as i64;
                self.doc_buffer_upto = upto + 1;
                break;
            }
            for j in 0..take {
                docs[n + j] = self.doc_buffer[upto + j] as u32;
            }
            if let Some(f) = freqs.as_deref_mut() {
                for j in 0..take {
                    f[n + j] = self.freq_buffer[upto + j];
                }
            }
            self.doc_buffer_upto = upto + take;
            self.doc = self.doc_buffer[upto + take - 1] as i64;
            n += take;
        }
        Ok(n)
    }
```

- [ ] **Step 4: DocsEnum / DocsFreqsEnum 公开包装**

`impl DocsEnum`（:659-672）尾部追加：

```rust
    /// 批量产出已解码 doc（spec 2026-07-26）：0 = 耗尽。
    pub fn next_docs(&mut self, docs: &mut [u32]) -> io::Result<usize> {
        self.core.next_docs(docs, None)
    }
```

`impl DocsFreqsEnum`（:677-698）尾部追加：

```rust
    /// 批量产出 doc（不解 freq）：0 = 耗尽。
    pub fn next_docs(&mut self, docs: &mut [u32]) -> io::Result<usize> {
        self.core.next_docs(docs, None)
    }

    /// 批量产出 doc + freq 同窗口。调用方先查 decodes_freqs()——
    /// no-freq 模式（docs_and_freqs_no_freq 构造）下 freq_buffer 未
    /// 物化，调用即 panic（同 freq() 的 no-freq 契约）。
    pub fn next_docs_and_freqs(
        &mut self,
        docs: &mut [u32],
        freqs: &mut [u32],
    ) -> io::Result<usize> {
        assert!(self.core.decode_freqs, "next_docs_and_freqs on no-freq enum");
        self.core.next_docs(docs, Some(freqs))
    }

    /// freq 块是否实际解码（needs_freq 构造时为真）。
    pub fn decodes_freqs(&self) -> bool {
        self.core.decode_freqs
    }
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test -p codec-lucene9 2>&1 | tail -3`
Expected: 全绿（185 + 2 = 187 passed）

- [ ] **Step 6: Commit**

```bash
git add crates/codec-lucene9/src/postings_read.rs
git commit -m "$(cat <<'EOF'
feat(codec): EnumCore::next_docs 窗口批读（128 解码块直拷，跨块透明 refill）

spec 2026-07-26 Task 2：DocsEnum::next_docs / DocsFreqsEnum::
next_docs_and_freqs + decodes_freqs。pos profile 不走批读（phrase 保持
逐 doc）。对拍：5 term × 5 dst 尺寸（1/7/128/200/4096）+ freq 异常值 +
singleton + no-freq 模式，与逐 doc 逐点一致。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: 叶子覆写——MatchAll / Bitmap 游标 / Docs / Freqs

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（MatchAllIter :54-78；BitmapCursor :640-682；RoaringDocIter impl :702-727；MaterializedDocIter impl :750-775；SegmentDocIter::next_block 的 Docs/Freqs 臂——Task 1 内联版换批读）
- Modify: `crates/core/src/search/block_tests.rs`（追加叶子对拍）

**Interfaces:**
- Consumes: Task 2 的 `DocsEnum::next_docs` / `DocsFreqsEnum::{next_docs_and_freqs, decodes_freqs}`；`BitmapCursor` 私有字段（buf/pos/end/refill——同文件内可访问）
- Produces: 叶子 `next_block` 覆写（RoaringDocIter / MaterializedDocIter 经 `BitmapCursor::next_many_to`；MatchAllIter 算术填充；SegmentDocIter Docs/Freqs 臂批读）；`BitmapCursor::next_many_to`（Task 6 的 DocSource 复用）

**设计偏差记录**：`BitsetDocIter` 不加专用覆写——`FixedBitSet` 无公开 word 访问（bitset.rs :7-60 只有 next_set_bit/get/popcount），而 next_set_bit 本就是 word 级 trailing_zeros 扫描，专用覆写无增量收益；保持默认 fill。若 Phase 1 profile 证实 bitset 路径热点再议（记入报告 §12）。

- [ ] **Step 1: 写失败测试——叶子对拍**

`block_tests.rs` 尾部追加：

```rust
use super::doc_iter::{MatchAllIter, RoaringDocIter};

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
```

注：`RoaringDocIter` 需要 `FrozenBitmap`（codec 内部视图，测试不易构造）——其覆写与 Materialized 共用 `BitmapCursor::next_many_to`，Materialized 对拍即覆盖游标批读逻辑；Roaring 路径由 Task 8 的 1M 电池（roaring 引擎全形状逐 query 对账）终验。import 行若编译器报未使用则删 `RoaringDocIter`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p rustlucene-core matchall_block 2>&1 | tail -5`
Expected: FAIL——MatchAll 走默认 fill 时 `stream_block` 其实已正确（默认 fill 对 MatchAll 语义正确）→ 此测试可能直接 PASS；`leaves_block_vs_per_doc_random` 同理 PASS（默认 fill 正确）。**本任务测试是回归守卫而非红灯起步**：确认当前 PASS 后继续（覆写只改性能不改行为）。

- [ ] **Step 3: BitmapCursor::next_many_to**

在 `impl<B: DocsBitmap> BitmapCursor<B>`（:640-682）的 `advance` 方法之后追加：

```rust
    /// 批量产出（spec 2026-07-26 Task 3）：把缓冲与后续 refill 的 doc
    /// 拷入 dst 至填满或耗尽，返回产出数。维护 next_from 不变量
    /// （= 最后产出 doc + 1），保证批读后 advance() 语义不变。
    /// docs < max_doc <= i32::MAX，+1 不溢出。
    fn next_many_to(&mut self, dst: &mut [u32]) -> usize {
        let mut n = 0;
        while n < dst.len() {
            if self.pos >= self.end && !self.refill() {
                break;
            }
            let take = (self.end - self.pos).min(dst.len() - n);
            dst[n..n + take].copy_from_slice(&self.buf[self.pos..self.pos + take]);
            self.pos += take;
            n += take;
        }
        if n > 0 {
            self.next_from = dst[n - 1] + 1;
        }
        n
    }
```

- [ ] **Step 4: RoaringDocIter / MaterializedDocIter 覆写**

在 `impl DocIter for RoaringDocIter`（:702-727）的 `advance` 之后追加：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        if self.doc == NO_MORE_DOCS {
            out.len = 0;
            return Ok(0);
        }
        let n = self.cur.next_many_to(&mut out.docs);
        if n == 0 {
            self.doc = NO_MORE_DOCS;
        } else {
            self.doc = out.docs[n - 1] as i32;
        }
        out.len = n;
        Ok(n)
    }
```

`impl DocIter for MaterializedDocIter`（:750-775）追加完全相同的覆写（同文件内复制，勿抽公共宏——两个 impl 块独立演进）：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        if self.doc == NO_MORE_DOCS {
            out.len = 0;
            return Ok(0);
        }
        let n = self.cur.next_many_to(&mut out.docs);
        if n == 0 {
            self.doc = NO_MORE_DOCS;
        } else {
            self.doc = out.docs[n - 1] as i32;
        }
        out.len = n;
        Ok(n)
    }
```

- [ ] **Step 5: MatchAllIter 覆写**

`impl DocIter for MatchAllIter`（:54-78）的 `advance` 之后追加：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        if self.doc == NO_MORE_DOCS {
            out.len = 0;
            return Ok(0);
        }
        let start = (self.doc + 1).max(0) as u32;
        let n = (self.max_doc as u32).saturating_sub(start).min(DOC_BLOCK as u32) as usize;
        for i in 0..n {
            out.docs[i] = start + i as u32;
        }
        self.doc = if n == 0 || start + n as u32 >= self.max_doc as u32 {
            NO_MORE_DOCS
        } else {
            (start + n as u32 - 1) as i32
        };
        out.len = n;
        Ok(n)
    }
```

- [ ] **Step 6: SegmentDocIter Docs/Freqs 臂换 codec 批读**

替换 Task 1 在 `SegmentDocIter::next_block` 里写的两个内联填充臂：

```rust
            Self::Docs(d) => {
                let n = d.next_docs(&mut out.docs)?;
                out.len = n;
                Ok(n)
            }
            Self::Freqs(f) => {
                let n = if f.decodes_freqs() {
                    f.next_docs_and_freqs(&mut out.docs, &mut out.freqs)?
                } else {
                    f.next_docs(&mut out.docs)?
                };
                out.len = n;
                Ok(n)
            }
```

- [ ] **Step 7: 跑测试确认通过**

Run: `cargo test -p rustlucene-core 2>&1 | tail -3`
Expected: 全绿（277 + 2 = 279 passed）

- [ ] **Step 8: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/search/block_tests.rs
git commit -m "$(cat <<'EOF'
feat(batch): 叶子 next_block 覆写——bitmap 游标 next_many_to / MatchAll 算术填充 / Docs-Freqs codec 批读

spec 2026-07-26 Task 3：叶子批读 = 内部缓冲直拷，零新增解码。
BitsetDocIter 保持默认 fill（FixedBitSet 无 word 公开访问，
next_set_bit 已 word 级，记入报告 §12 设计偏差）。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: 共享块代数——intersect / andnot / kway-union

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（`SegmentDocIter` 定义 :1495 之前新增代数区）
- Modify: `crates/core/src/search/block_tests.rs`（代数单测）

**Interfaces:**
- Consumes: 无（纯函数）
- Produces: `block_intersect(a, b, out) -> (usize, usize, usize)`、`block_andnot(a, b, out) -> (usize, usize, usize)`、`kway_union(heads, consumed, out) -> usize`——Task 5/6 组合器覆写的内核；Phase 2 SIMD 只换这三个函数内核，签名不动

- [ ] **Step 1: 写失败测试——代数四象限**

`block_tests.rs` 尾部追加：

```rust
use super::doc_iter::{block_andnot, block_intersect, kway_union};

fn run(f: impl Fn(&[u32], &[u32], &mut [u32]) -> (usize, usize, usize), a: &[u32], b: &[u32]) -> Vec<u32> {
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
        (vec![1, 3, 5], vec![2, 4, 6]),          // 不相交
        (vec![1, 2, 3], vec![1, 2, 3]),          // 全等
        (vec![2, 4], vec![1, 2, 3, 4, 5]),       // 包含
        ((0..300).step_by(2).collect(), (0..300).step_by(3).collect()), // 交错跨块
        ((0..128).collect(), (0..256).collect()),                      // 128 整数倍
        ((0..127).collect(), (0..129).collect()),                      // 尾块
    ];
    for (a, b) in cases {
        assert_eq!(run(block_intersect, &a, &b), expect_intersect(&a, &b), "intersect {a:?} {b:?}");
        assert_eq!(run(block_andnot, &a, &b), expect_andnot(&a, &b), "andnot {a:?} {b:?}");
        // andnot 反对称
        assert_eq!(run(block_andnot, &b, &a), expect_andnot(&b, &a), "andnot rev {a:?} {b:?}");
    }
}

#[test]
fn kway_union_dedup_and_order() {
    let mut lcg = Lcg(99);
    for _ in 0..30 {
        let k = 2 + (lcg.next_u32() % 5) as usize;
        let sets: Vec<Vec<u32>> = (0..k)
            .map(|_| lcg.doc_set(3_000, (lcg.next_u32() % 400) as usize))
            .collect();
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
            let n = kway_union(&heads, &mut consumed, &mut out);
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p rustlucene-core algebra_ 2>&1 | tail -5; cargo test -p rustlucene-core kway_ 2>&1 | tail -5`
Expected: 编译错误——三个函数未定义

- [ ] **Step 3: 实现三个代数内核**

在 `doc_iter.rs` 的 `// ── SegmentDocIter ──` 注释行（:1493）之前插入：

```rust
// ── 块代数内核（spec 2026-07-26 §4；Phase 2 SIMD 只换这里） ─────────

/// 双指针 intersect，部分消费语义：对 a/b 前缀求交写入 out（至多
/// out.len() 个），返回 (消费 a 数, 消费 b 数, 产出数)。产出满或
/// 某侧耗尽即停——调用方按返回值推进游标跨调用续算。
/// 输入要求：a/b 升序（块契约）。
fn block_intersect(a: &[u32], b: &[u32], out: &mut [u32]) -> (usize, usize, usize) {
    let (mut ia, mut ib, mut n) = (0, 0, 0);
    while ia < a.len() && ib < b.len() && n < out.len() {
        let (x, y) = (a[ia], b[ib]);
        if x == y {
            out[n] = x;
            n += 1;
            ia += 1;
            ib += 1;
        } else if x < y {
            ia += 1;
        } else {
            ib += 1;
        }
    }
    (ia, ib, n)
}

/// slice 差集 a \ b，部分消费语义同 block_intersect。注意：b 侧消费
/// 只推进到"已确认 < a 当前尾"的前缀——b 游标跨调用留存（Excl 的
/// prohibited 块语义）。
fn block_andnot(a: &[u32], b: &[u32], out: &mut [u32]) -> (usize, usize, usize) {
    let (mut ia, mut ib, mut n) = (0, 0, 0);
    while ia < a.len() && n < out.len() {
        let x = a[ia];
        while ib < b.len() && b[ib] < x {
            ib += 1;
        }
        if ib < b.len() && b[ib] == x {
            ib += 1; // 排除；b 该元素已消费
        } else {
            out[n] = x;
            n += 1;
        }
        ia += 1;
    }
    (ia, ib, n)
}

/// k 路有序 slice 归并去重，out 满即停。consumed[i] 写回各 head 消费
/// 数（调用方初始化长度 = heads.len()）。k 小（bool 子句数）→ 线性扫
/// 最小头，不上堆（堆化是 bool-bench-report §11 P2 议题）。
fn kway_union(heads: &[&[u32]], consumed: &mut [usize], out: &mut [u32]) -> usize {
    debug_assert_eq!(heads.len(), consumed.len());
    let mut n = 0;
    let mut last: Option<u32> = None;
    while n < out.len() {
        // 选最小头
        let mut best: Option<(usize, u32)> = None;
        for (i, h) in heads.iter().enumerate() {
            let rest = &h[consumed[i]..];
            if let Some(&d) = rest.first() {
                if best.is_none_or(|(_, bd)| d < bd) {
                    best = Some((i, d));
                }
            }
        }
        let Some((_, d)) = best else { break };
        // 推进所有等于 d 的头（去重）
        for (i, h) in heads.iter().enumerate() {
            let rest = &h[consumed[i]..];
            if rest.first() == Some(&d) {
                consumed[i] += 1;
            }
        }
        if last != Some(d) {
            out[n] = d;
            n += 1;
            last = Some(d);
        }
    }
    n
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p rustlucene-core algebra_ kway_ 2>&1 | tail -4`
Expected: 2 测试 PASS

Run: `cargo test --workspace 2>&1 | tail -3`
Expected: 全绿（281 passed）

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/search/block_tests.rs
git commit -m "$(cat <<'EOF'
feat(batch): 共享块代数内核 block_intersect / block_andnot / kway_union

spec 2026-07-26 §4：部分消费语义（组合器跨调用游标状态机基础），
标量双指针/线性归并——Phase 2 SIMD 只换内核不改签名。四象限 +
随机 30 组 k 路（k=2..6，out=64 故意触发多轮）对拍。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: 组合器覆写（一）——Excluding / ConjOver / DisjOver

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（BlockCursor 辅助 + ExcludingDocIter :1424-1491 / ConjOverDocIter :1021-1122 / DisjOverDocIter :1301-1419 的 impl 追加 next_block）
- Modify: `crates/core/src/search/block_tests.rs`（组合器对拍）

**Interfaces:**
- Consumes: Task 4 三代数函数；子迭代器 `SegmentDocIter::next_block`（叶子已真块，phrase 默认 fill——输出都已 matches 确认）
- Produces: 三个 Over/Excl 组合器的 `next_block` 覆写 + `BlockCursor` 复用件（Task 6 PostingsIter 组合器复用同款游标模式）

- [ ] **Step 1: 写失败测试——组合器对拍**

`block_tests.rs` 尾部追加：

```rust
use super::doc_iter::{ConjOverDocIter, DisjOverDocIter, ExcludingDocIter};

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
            .map(|_| lcg.doc_set(4_000, (lcg.next_u32() % 500) as usize))
            .collect();
        // ConjOver
        let mut a = conj_over(sets.clone());
        let mut b = conj_over(sets.clone());
        assert_eq!(stream_block(&mut a), stream_per_doc(&mut b), "conj round={round}");
        assert_eq!(stream_block(&mut conj_over(sets.clone())), expect_conj(&sets));
        // DisjOver
        let mut a = disj_over(sets.clone());
        let mut b = disj_over(sets.clone());
        assert_eq!(stream_block(&mut a), stream_per_doc(&mut b), "disj round={round}");
        assert_eq!(stream_block(&mut disj_over(sets.clone())), expect_disj(&sets));
        // Excluding
        let (m, p) = (sets[0].clone(), sets[1].clone());
        let expect: Vec<u32> = m.iter().filter(|x| !p.contains(x)).copied().collect();
        let mut a = excl(m.clone(), p.clone());
        let mut b = excl(m, p);
        assert_eq!(stream_block(&mut a), stream_per_doc(&mut b), "excl round={round}");
        assert_eq!(stream_block(&mut a), expect);
    }
}

#[test]
fn combinator_edge_shapes() {
    // 空交 / 空集子句 / 128 整数倍 / 全等 / 尾块
    let e: Vec<u32> = vec![];
    assert_eq!(stream_block(&mut conj_over(vec![vec![1, 2], e.clone()])), vec![]);
    assert_eq!(stream_block(&mut disj_over(vec![e.clone(), vec![3]])), vec![3]);
    let full: Vec<u32> = (0..256).collect();
    assert_eq!(stream_block(&mut excl(full.clone(), e)), full);
    assert_eq!(stream_block(&mut excl(e.clone(), full.clone())), vec![]);
    let a: Vec<u32> = (0..300).step_by(2).collect();
    let b: Vec<u32> = (0..300).step_by(3).collect();
    let expect: Vec<u32> = a.iter().filter(|x| !b.contains(x)).copied().collect();
    assert_eq!(stream_block(&mut excl(a, b)), expect);
}
```

注：`leaf(&[])` —— `MaterializedBitmap::of(&[])` 空集合法（materialized.rs:117-123 已有测试）；`ConjOverDocIter::new` 对空集子句返回 doc=NO_MORE_DOCS（:1037-1041）→ 空交语义 ✓。`ExcludingDocIter` 的 main 为空：首次 next_doc 即 NO_MORE ✓。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p rustlucene-core combinator_ 2>&1 | tail -5`
Expected: FAIL 或编译通过但断言挂——组合器尚无 next_block 覆写，走默认 fill……**默认 fill 语义正确**（循环 next_doc+matches），对拍可能 PASS。与 Task 3 同理：本测试是覆写后的回归守卫；先确认当前 PASS（锁定语义基线），覆写后必须继续 PASS。

- [ ] **Step 3: BlockCursor 辅助件**

在 Task 4 代数区（`block_andnot` 之后、`kway_union` 之前或之后均可）追加：

```rust
/// 组合器块路径的 child 游标（spec §4）：持有 child 的当前块切片窗口。
/// 18.7KB 的 SegmentDocIter 子迭代器不进此结构——只存 1KB 块缓冲（堆）。
struct BlockCursor {
    buf: Box<DocBlockBuf>,
    pos: usize,
    len: usize,
    exhausted: bool,
}

impl BlockCursor {
    fn new() -> BlockCursor {
        BlockCursor {
            buf: Box::new(DocBlockBuf::new()),
            pos: 0,
            len: 0,
            exhausted: false,
        }
    }

    /// 拉 child 下一块。返回 false = child 耗尽（此后 remaining() 恒空）。
    fn refill<I: DocIter>(&mut self, it: &mut I) -> io::Result<bool> {
        if self.exhausted {
            return Ok(false);
        }
        let n = it.next_block(&mut self.buf)?;
        self.pos = 0;
        self.len = n;
        if n == 0 {
            self.exhausted = true;
            return Ok(false);
        }
        Ok(true)
    }

    /// 当前块未消费切片。
    fn remaining(&self) -> &[u32] {
        &self.buf.docs[self.pos..self.len]
    }

    /// 当前块尾 doc（空切片 = None）。
    fn max_remaining(&self) -> Option<u32> {
        (self.pos < self.len).then(|| self.buf.docs[self.len - 1])
    }

    /// 推进游标 n 个（代数内核返回值）。
    fn consume(&mut self, n: usize) {
        self.pos += n;
    }
}

/// 窗口定位（n 元合取逐元素路径用）：消费块内 < e 的前缀，跨块
/// refill 直到首元素 >= e。返回 `Some(首元素 == e)`（contains 判定）/
/// `None` = child 耗尽。前向单调，对同一 child 以递增 e 序列调用。
fn position_seg(
    curs: &mut BlockCursor,
    child: &mut SegmentDocIter,
    e: u32,
) -> io::Result<Option<bool>> {
    loop {
        while !curs.remaining().is_empty() && curs.remaining()[0] < e {
            curs.consume(1);
        }
        if let Some(head) = curs.remaining().first() {
            return Ok(Some(*head == e));
        }
        if !curs.refill(child)? {
            return Ok(None);
        }
    }
}
```

- [ ] **Step 4: ExcludingDocIter::next_block**

给 `ExcludingDocIter` 结构体（:1424-1428）加游标字段：

```rust
pub struct ExcludingDocIter {
    main: Box<SegmentDocIter>,
    prohibited: Box<SegmentDocIter>,
    doc: i32,
    // 块路径游标（per-doc 路径不读；18.7KB 子迭代器仍在 Box 里）
    mcur: BlockCursor,
    pcur: BlockCursor,
}
```

`ExcludingDocIter::new`（:1431-1437）补字段：

```rust
    pub fn new(main: SegmentDocIter, prohibited: SegmentDocIter) -> ExcludingDocIter {
        ExcludingDocIter {
            main: Box::new(main),
            prohibited: Box::new(prohibited),
            doc: -1,
            mcur: BlockCursor::new(),
            pcur: BlockCursor::new(),
        }
    }
```

`impl DocIter for ExcludingDocIter`（:1467-1491）尾部追加覆写：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut prod = 0;
        while prod < DOC_BLOCK {
            // must 块就位
            if self.mcur.remaining().is_empty() && !self.mcur.refill(&mut self.main)? {
                break; // must 耗尽
            }
            // prohibited 块推进到与 must 当前块重叠（块级跳过——替代
            // per-doc 逐候选 advance 病理，spec §1.1 / P1-1 同源）
            let m_lo = self.mcur.remaining()[0];
            loop {
                match self.pcur.max_remaining() {
                    Some(p_hi) if p_hi >= m_lo => break,
                    _ => {
                        if !self.pcur.refill(&mut self.prohibited)? {
                            // prohibited 耗尽：must 剩余全部命中
                            let rest = self.mcur.remaining();
                            let take = rest.len().min(DOC_BLOCK - prod);
                            out.docs[prod..prod + take].copy_from_slice(&rest[..take]);
                            self.mcur.consume(take);
                            prod += take;
                            self.doc = out.docs[prod - 1] as i32;
                            continue;
                        }
                    }
                }
            }
            let (cm, cp, n) = block_andnot(
                self.mcur.remaining(),
                self.pcur.remaining(),
                &mut out.docs[prod..],
            );
            self.mcur.consume(cm);
            self.pcur.consume(cp);
            prod += n;
        }
        if prod > 0 {
            self.doc = out.docs[prod - 1] as i32;
        } else if self.mcur.exhausted {
            self.doc = NO_MORE_DOCS;
        }
        out.len = prod;
        Ok(prod)
    }
```

- [ ] **Step 5: ConjOverDocIter::next_block**

`ConjOverDocIter` 结构体（:1021-1025）加字段 `curs: Vec<BlockCursor>`，`new`（:1030-1044）初始化 `curs: sub.iter().map(|_| BlockCursor::new()).collect()`。impl 尾部追加：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut prod = 0;
        'blk: while prod < DOC_BLOCK {
            // child0 块就位（耗尽 → 整体结束）
            if self.curs[0].remaining().is_empty() && !self.curs[0].refill(&mut self.sub[0])? {
                break 'blk;
            }
            if self.sub.len() == 2 {
                // 二元快路径（bench 主导形状）：slice intersect，ca/cb
                // 直接是两侧块消费数，部分消费语义天然正确。
                while self.curs[1].max_remaining().is_none_or(|hi| hi < self.curs[0].remaining()[0]) {
                    if !self.curs[1].refill(&mut self.sub[1])? {
                        break 'blk; // child1 耗尽 → 交耗尽
                    }
                }
                let (ca, cb, n) = block_intersect(
                    self.curs[0].remaining(),
                    self.curs[1].remaining(),
                    &mut out.docs[prod..],
                );
                self.curs[0].consume(ca);
                self.curs[1].consume(cb);
                prod += n;
                continue; // n=0 时 ca/cb 已推进，安全续环（两侧非空时必有推进）
            }
            // n 元通用路径：逐元素定位其余 child 窗口（正确性优先于
            // 二元 slice 快路径——scratch 折叠的消费坐标映射易错）。
            let c0_len = self.curs[0].remaining().len();
            let mut used = 0;
            for idx in 0..c0_len {
                let e = self.curs[0].buf.docs[self.curs[0].pos + idx];
                used += 1;
                let mut hit = true;
                for i in 1..self.sub.len() {
                    let (curs, sub) = (&mut self.curs, &mut self.sub);
                    match position_seg(&mut curs[i], &mut sub[i], e)? {
                        None => {
                            // child 耗尽 → 交耗尽
                            self.curs[0].consume(used);
                            self.doc = NO_MORE_DOCS;
                            out.len = prod;
                            return Ok(prod);
                        }
                        Some(false) => {
                            hit = false;
                            break; // 后续 child 无需定位（前向单调，下轮 e' > e 续推）
                        }
                        Some(true) => {}
                    }
                }
                if hit {
                    // 命中：各 child 首元素 == e（position_seg 保证），消费
                    for i in 1..self.sub.len() {
                        self.curs[i].consume(1);
                    }
                    out.docs[prod] = e;
                    prod += 1;
                    if prod == DOC_BLOCK {
                        break;
                    }
                }
            }
            self.curs[0].consume(used);
        }
        if prod > 0 {
            self.doc = out.docs[prod - 1] as i32;
        } else if self.curs.iter().all(|c| c.exhausted || c.remaining().is_empty()) {
            self.doc = NO_MORE_DOCS;
        }
        out.len = prod;
        Ok(prod)
    }
```

正确性要点：`position_seg` 把 child 窗口推进到首元素 ≥ e（块内 consume + 跨块 refill），`Some(head == e)` 即 contains；hit=false 时不消费任何 child（e 不属交集），后续 child 的定位缺口由下一个更大的 e 前向补齐；child0 的消费按 `used` 整块结算。二元形状（and 两词项，bench 主力）走 `block_intersect` 快路径。

- [ ] **Step 6: DisjOverDocIter::next_block**

`DisjOverDocIter` 结构体（:1301-1306）加 `curs: Vec<BlockCursor>`，`new` 初始化。impl 尾部追加：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        // k 路块归并：heads = 各 child 未消费切片，kway_union 满 128 即停。
        // matches() 已由 child 的 next_block 吸收（子句恒单阶段亦无害）。
        let mut prod = 0;
        while prod < DOC_BLOCK {
            // 耗尽 child 的游标补块
            let mut any = false;
            for i in 0..self.sub.len() {
                if self.curs[i].remaining().is_empty() && !self.curs[i].exhausted {
                    self.curs[i].refill(&mut self.sub[i])?;
                }
                if !self.curs[i].remaining().is_empty() {
                    any = true;
                }
            }
            if !any {
                break;
            }
            let heads: Vec<&[u32]> = (0..self.sub.len())
                .map(|i| self.curs[i].remaining())
                .collect();
            let mut consumed = vec![0usize; self.sub.len()];
            let n = kway_union(&heads, &mut consumed, &mut out.docs[prod..]);
            for i in 0..self.sub.len() {
                self.curs[i].consume(consumed[i]);
            }
            prod += n;
            if n == 0 {
                break;
            }
        }
        if prod > 0 {
            self.doc = out.docs[prod - 1] as i32;
        } else {
            self.doc = NO_MORE_DOCS;
        }
        out.len = prod;
        Ok(prod)
    }
```

注：`heads` 借用 `self.curs` 的同时 `consume` 需要 `&mut self.curs`——借用冲突。解法：`kway_union` 调用结束（heads 生命周期终止）后再 consume 循环 ✓（上面代码已是此序）；`Vec<&[u32]>` 的构造在循环内每轮新建。`consumed` 的 Vec 分配每轮一次——k 小可接受；若 profile 显示分配开销，Phase 2 换栈数组。

- [ ] **Step 7: 跑测试确认通过**

Run: `cargo test -p rustlucene-core 2>&1 | tail -3`
Expected: 全绿（281 + 2 = 283 passed）

- [ ] **Step 8: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/search/block_tests.rs
git commit -m "$(cat <<'EOF'
feat(batch): 组合器覆写（一）Excluding/ConjOver/DisjOver 块代数

spec 2026-07-26 §4：Excl prohibited 块级跳过（替代逐候选 advance）、
ConjOver 成对 slice intersect、DisjOver k 路块归并。BlockCursor 游标
（1KB 堆块缓冲，18.7KB 子迭代器不挪动）。40 轮随机 + 边界形状对拍。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: 组合器覆写（二）——Conjunction / Disjunction / RoaringAnd / RoaringOr

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（PostingsIter :82-126 加 next_block；ConjunctionDocIter :130-233 / DisjunctionDocIter :237-321 覆写；DocSource :782-848 加 next_docs；RoaringAndDocIter :857-950 / RoaringOrDocIter :957-1012 覆写）
- Modify: `crates/core/src/search/block_tests.rs`（RoaringOr 对拍——DocSource::slice 路径可纯内存构造）

**Interfaces:**
- Consumes: Task 2 codec 批读（PostingsIter 包装）；Task 4 代数；Task 5 BlockCursor 模式
- Produces: 剩余四个组合器的 `next_block`——至此所有 SegmentDocIter 变体除 Phrase/Bitset（默认 fill，设计记录见 Task 3）外全部真块

- [ ] **Step 1: 写失败测试——RoaringOr slice 源对拍**

`block_tests.rs` 尾部追加：

```rust
use super::doc_iter::{DocSource, RoaringOrDocIter};

#[test]
fn roaring_or_slice_sources_block_vs_per_doc() {
    let mut lcg = Lcg(5678);
    for round in 0..30 {
        let k = 2 + (lcg.next_u32() % 4) as usize;
        let sets: Vec<Vec<u32>> = (0..k)
            .map(|_| lcg.doc_set(8_000, (lcg.next_u32() % 700) as usize))
            .collect();
        let mk = || {
            let sources: Vec<DocSource> = sets.iter().map(|s| DocSource::slice(s.clone())).collect();
            SegmentDocIter::RoaringOr(RoaringOrDocIter::new(sources))
        };
        let mut a = mk();
        let mut b = mk();
        assert_eq!(stream_block(&mut a), stream_per_doc(&mut b), "round={round}");
        assert_eq!(stream_block(&mut mk()), expect_disj(&sets));
    }
}
```

注：`RoaringAndDocIter` 需要 `FrozenBitmap` probes（codec 视图，测试不易构造）+ DocSource::Bitmap——其覆写逻辑（成对 intersect + contains 过滤）由 Task 8 的 1M roaring 电池（含 and 高/中桶）终验；slice 源路径与 RoaringOr 共享归并/交内核，已测。Conjunction/Disjunction（PostingsIter 子）需要真实索引——同样由 Task 8 的 PFOR 电池终验（and/or 桶逐 query 对账），加 Step 6 的 searcher 级集成测试覆盖。

- [ ] **Step 2: 跑测试确认当前状态**

Run: `cargo test -p rustlucene-core roaring_or 2>&1 | tail -5`
Expected: PASS（默认 fill 语义正确——回归守卫基线）

- [ ] **Step 3: PostingsIter::next_block**

`impl PostingsIter`（:86-126）尾部追加：

```rust
    /// 块级产出（spec Task 6）：codec 窗口批读。freqs 仅 decode_freqs
    /// 路径填充（needs_freq 构造）；no-freq enum 只填 docs。
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let n = match self {
            Self::Docs(d) => d.next_docs(&mut out.docs)?,
            Self::Freqs(f) => {
                if f.decodes_freqs() {
                    f.next_docs_and_freqs(&mut out.docs, &mut out.freqs)?
                } else {
                    f.next_docs(&mut out.docs)?
                }
            }
        };
        out.len = n;
        Ok(n)
    }
```

- [ ] **Step 4: ConjunctionDocIter::next_block**

`ConjunctionDocIter` 结构体（:130-134）加 `curs: Vec<BlockCursor>`，`new` 的两处构造（:153-157 与 :160-164）都补 `curs: sub.iter().map(|_| BlockCursor::new()).collect()`——注意 :153 的早退分支在 `sub` 已构造后，`curs` 表达式同。impl 尾部追加（PostingsIter 实现了本 crate的 DocIter？——**没有**：PostingsIter 只有固有方法。故游标 refill 不能走 `DocIter::next_block` 泛型）：

BlockCursor::refill 的 `<I: DocIter>` 约束不适用 PostingsIter。补一个固有方法到 BlockCursor：

```rust
impl BlockCursor {
    /// PostingsIter 专用 refill（PostingsIter 无 DocIter impl，固有方法分发）。
    fn refill_postings(&mut self, it: &mut PostingsIter) -> io::Result<bool> {
        if self.exhausted {
            return Ok(false);
        }
        let n = it.next_block(&mut self.buf)?;
        self.pos = 0;
        self.len = n;
        if n == 0 {
            self.exhausted = true;
            return Ok(false);
        }
        Ok(true)
    }
}
```

`position_seg` 的 PostingsIter 孪生版（PostingsIter 无 DocIter impl），与 `BlockCursor::refill_postings` 同区追加：

```rust
/// position_seg 的 PostingsIter 版（语义逐字一致）。
fn position_postings(
    curs: &mut BlockCursor,
    child: &mut PostingsIter,
    e: u32,
) -> io::Result<Option<bool>> {
    loop {
        while !curs.remaining().is_empty() && curs.remaining()[0] < e {
            curs.consume(1);
        }
        if let Some(head) = curs.remaining().first() {
            return Ok(Some(*head == e));
        }
        if !curs.refill_postings(child)? {
            return Ok(None);
        }
    }
}
```

`impl DocIter for ConjunctionDocIter`（:176-233）尾部追加（Task 5 ConjOver 覆写同构：二元 slice 快路径 + n 元逐元素定位）：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut prod = 0;
        'blk: while prod < DOC_BLOCK {
            if self.curs[0].remaining().is_empty() && !self.curs[0].refill_postings(&mut self.sub[0])? {
                break 'blk;
            }
            if self.sub.len() == 2 {
                while self.curs[1].max_remaining().is_none_or(|hi| hi < self.curs[0].remaining()[0]) {
                    if !self.curs[1].refill_postings(&mut self.sub[1])? {
                        break 'blk;
                    }
                }
                let (ca, cb, n) = block_intersect(
                    self.curs[0].remaining(),
                    self.curs[1].remaining(),
                    &mut out.docs[prod..],
                );
                self.curs[0].consume(ca);
                self.curs[1].consume(cb);
                prod += n;
                continue;
            }
            let c0_len = self.curs[0].remaining().len();
            let mut used = 0;
            for idx in 0..c0_len {
                let e = self.curs[0].buf.docs[self.curs[0].pos + idx];
                used += 1;
                let mut hit = true;
                for i in 1..self.sub.len() {
                    let (curs, sub) = (&mut self.curs, &mut self.sub);
                    match position_postings(&mut curs[i], &mut sub[i], e)? {
                        None => {
                            self.curs[0].consume(used);
                            self.doc = NO_MORE_DOCS;
                            out.len = prod;
                            return Ok(prod);
                        }
                        Some(false) => {
                            hit = false;
                            break;
                        }
                        Some(true) => {}
                    }
                }
                if hit {
                    for i in 1..self.sub.len() {
                        self.curs[i].consume(1);
                    }
                    out.docs[prod] = e;
                    prod += 1;
                    if prod == DOC_BLOCK {
                        break;
                    }
                }
            }
            self.curs[0].consume(used);
        }
        if prod > 0 {
            self.doc = out.docs[prod - 1] as i32;
        } else if self.curs.iter().all(|c| c.exhausted || c.remaining().is_empty()) {
            self.doc = NO_MORE_DOCS;
        }
        out.len = prod;
        Ok(prod)
    }
```

- [ ] **Step 5: DisjunctionDocIter::next_block**

`DisjunctionDocIter` 结构体（:237-240）加 `curs: Vec<BlockCursor>`，`new`（:260）补 `curs: sub.iter().map(|_| BlockCursor::new()).collect()`。impl 尾部追加（Task 5 DisjOver 覆写同构，refill 改 `refill_postings`）：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut prod = 0;
        while prod < DOC_BLOCK {
            let mut any = false;
            for i in 0..self.sub.len() {
                if self.curs[i].remaining().is_empty() && !self.curs[i].exhausted {
                    self.curs[i].refill_postings(&mut self.sub[i])?;
                }
                if !self.curs[i].remaining().is_empty() {
                    any = true;
                }
            }
            if !any {
                break;
            }
            let heads: Vec<&[u32]> = (0..self.sub.len()).map(|i| self.curs[i].remaining()).collect();
            let mut consumed = vec![0usize; self.sub.len()];
            let n = kway_union(&heads, &mut consumed, &mut out.docs[prod..]);
            for i in 0..self.sub.len() {
                self.curs[i].consume(consumed[i]);
            }
            prod += n;
            if n == 0 {
                break;
            }
        }
        self.doc = if prod > 0 { out.docs[prod - 1] as i32 } else { NO_MORE_DOCS };
        out.len = prod;
        Ok(prod)
    }
```

- [ ] **Step 6: DocSource::next_docs + RoaringOr / RoaringAnd 覆写**

`impl DocSource`（:793-848）尾部追加：

```rust
    /// 批量产出（spec Task 6）：先吐预拉的 current doc，再续批。
    /// 契约：next_docs 之后 current()/advance() 不得再调（块/逐 doc
    /// 模式不混用——driver 只选其一）。
    fn next_docs(&mut self, dst: &mut [u32]) -> usize {
        if dst.is_empty() {
            return 0;
        }
        let mut n = 0;
        if let Some(d) = self.current() {
            dst[0] = d;
            n = 1;
        }
        match self {
            DocSource::Bitmap { cur, doc } => {
                n += cur.next_many_to(&mut dst[n..]);
                *doc = None;
            }
            DocSource::Slice { docs, pos } => {
                let take = (docs.len() - *pos).min(dst.len() - n);
                dst[n..n + take].copy_from_slice(&docs[*pos..*pos + take]);
                *pos += take;
                n += take;
            }
        }
        n
    }
```

DocSource 块游标（复用 Box<[u32; DOC_BLOCK]> 轻量版——DocSource 子不带 freq）：

```rust
/// DocSource 专用块游标（RoaringAnd/Or 覆写用）。
struct SourceCursor {
    buf: Box<[u32; DOC_BLOCK]>,
    pos: usize,
    len: usize,
    exhausted: bool,
}

impl SourceCursor {
    fn new() -> SourceCursor {
        SourceCursor { buf: Box::new([0; DOC_BLOCK]), pos: 0, len: 0, exhausted: false }
    }
    fn refill(&mut self, src: &mut DocSource) -> bool {
        if self.exhausted {
            return false;
        }
        self.len = src.next_docs(&mut self.buf);
        self.pos = 0;
        if self.len == 0 {
            self.exhausted = true;
            return false;
        }
        true
    }
    fn remaining(&self) -> &[u32] {
        &self.buf[self.pos..self.len]
    }
    fn max_remaining(&self) -> Option<u32> {
        (self.pos < self.len).then(|| self.buf[self.len - 1])
    }
    fn consume(&mut self, n: usize) {
        self.pos += n;
    }
}
```

`RoaringOrDocIter` 结构体（:957-960）加 `curs: Vec<SourceCursor>`，`new`（:963-966）补 `curs: sources.iter().map(|_| SourceCursor::new()).collect()`。`impl DocIter for RoaringOrDocIter`（:969-1012）尾部追加（kway_union 归并，结构同 DisjOver，refill 改 SourceCursor::refill(&mut self.sources[i])）：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut prod = 0;
        while prod < DOC_BLOCK {
            let mut any = false;
            for i in 0..self.sources.len() {
                if self.curs[i].remaining().is_empty() && !self.curs[i].exhausted {
                    self.curs[i].refill(&mut self.sources[i]);
                }
                if !self.curs[i].remaining().is_empty() {
                    any = true;
                }
            }
            if !any {
                break;
            }
            let heads: Vec<&[u32]> = (0..self.sources.len()).map(|i| self.curs[i].remaining()).collect();
            let mut consumed = vec![0usize; self.sources.len()];
            let n = kway_union(&heads, &mut consumed, &mut out.docs[prod..]);
            for i in 0..self.sources.len() {
                self.curs[i].consume(consumed[i]);
            }
            prod += n;
            if n == 0 {
                break;
            }
        }
        self.doc = if prod > 0 { out.docs[prod - 1] as i32 } else { NO_MORE_DOCS };
        out.len = prod;
        Ok(prod)
    }
```

`RoaringAndDocIter` 结构体（:857-861）加 `curs: Vec<SourceCursor>`，`new` 补初始化。`position_source`（无 io 版，随 SourceCursor 同区）：

```rust
/// position_seg 的 DocSource 版（无 io）。
fn position_source(curs: &mut SourceCursor, src: &mut DocSource, e: u32) -> Option<bool> {
    loop {
        while !curs.remaining().is_empty() && curs.remaining()[0] < e {
            curs.consume(1);
        }
        if let Some(head) = curs.remaining().first() {
            return Some(*head == e);
        }
        if !curs.refill(src) {
            return None;
        }
    }
}
```

覆写（二元 slice 快路径 + n 元逐元素定位，probes 点探过滤——contains 8.5–28.7 ns/探测，spec §4 记录）：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut prod = 0;
        'blk: while prod < DOC_BLOCK {
            if self.curs[0].remaining().is_empty() && !self.curs[0].refill(&mut self.sources[0]) {
                break 'blk;
            }
            if self.sources.len() == 2 {
                while self.curs[1].max_remaining().is_none_or(|hi| hi < self.curs[0].remaining()[0]) {
                    if !self.curs[1].refill(&mut self.sources[1]) {
                        break 'blk;
                    }
                }
                let (ca, cb, n) = block_intersect(
                    self.curs[0].remaining(),
                    self.curs[1].remaining(),
                    &mut out.docs[prod..],
                );
                self.curs[0].consume(ca);
                self.curs[1].consume(cb);
                // probes 原地过滤（保序）
                let mut kept = 0;
                for j in 0..n {
                    let d = out.docs[prod + j];
                    if self.probes.iter().all(|p| p.contains(d)) {
                        out.docs[prod + kept] = d;
                        kept += 1;
                    }
                }
                prod += kept;
                continue;
            }
            let c0_len = self.curs[0].remaining().len();
            let mut used = 0;
            for idx in 0..c0_len {
                let e = self.curs[0].buf[self.curs[0].pos + idx];
                used += 1;
                let mut hit = true;
                for i in 1..self.sources.len() {
                    let (curs, srcs) = (&mut self.curs, &mut self.sources);
                    match position_source(&mut curs[i], &mut srcs[i], e) {
                        None => {
                            self.curs[0].consume(used);
                            self.doc = NO_MORE_DOCS;
                            out.len = prod;
                            return Ok(prod);
                        }
                        Some(false) => {
                            hit = false;
                            break;
                        }
                        Some(true) => {}
                    }
                }
                // probes 拒绝时不消费 child：e 留给下一轮 position 前向跳过
                if hit && self.probes.iter().all(|p| p.contains(e)) {
                    for i in 1..self.sources.len() {
                        self.curs[i].consume(1);
                    }
                    out.docs[prod] = e;
                    prod += 1;
                    if prod == DOC_BLOCK {
                        break;
                    }
                }
            }
            self.curs[0].consume(used);
        }
        if prod > 0 {
            self.doc = out.docs[prod - 1] as i32;
        }
        out.len = prod;
        Ok(prod)
    }
```

- [ ] **Step 7: searcher 级集成对拍（含索引）**

`block_tests.rs` 尾部追加（用 crate 公开 API 建小索引；block 路径默认 ON 下 `Searcher::search` 结果 vs 显式 per-doc segment_iterator 驱动）：

```rust
use crate::{Document, FieldSpec, FieldValue, IndexWriter, IndexWriterConfig, Schema, Schema as _};
use super::query::Query;
use super::searcher::Searcher;
use codec_lucene9::directory::FSDirectory;

fn tiny_index(tag: &str, num_docs: u32, flush_every: u32) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rustlucene-block-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = Schema::new();
    s.add(FieldSpec::keyword("kw"));
    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = flush_every;
    let mut w = IndexWriter::create(&dir, s, config).unwrap();
    let terms = ["alpha", "beta", "gamma", "delta"];
    for i in 0..num_docs {
        let mut d = Document::new();
        d.add(FieldValue::keyword("kw", terms[(i % 4) as usize]));
        if i % 3 == 0 {
            d.add(FieldValue::keyword("kw", "extra"));
        }
        w.add_document(d).unwrap();
    }
    w.commit().unwrap();
    dir
}

#[test]
fn searcher_block_vs_explicit_per_doc_multi_segment() {
    // 300 docs，flush 每 100 → 3 段（跨段 doc_base 加宽覆盖）
    let dir = tiny_index("searcher", 300, 100);
    let fsdir = FSDirectory::open(&dir).unwrap();
    let mut searcher = Searcher::open(&fsdir).unwrap();
    assert!(searcher.segment_count() >= 2);

    let queries = vec![
        Query::term("kw", "alpha"),
        Query::and(vec![Query::term("kw", "alpha"), Query::term("kw", "extra")]),
        Query::or(vec![Query::term("kw", "alpha"), Query::term("kw", "beta")]),
        Query::bool(vec![
            (super::query::Occur::Must, Query::term("kw", "alpha")),
            (super::query::Occur::MustNot, Query::term("kw", "extra")),
        ]),
    ];
    for q in queries {
        // 实际：Searcher::search（block 路径，默认 ON）
        let mut c = CountCollector::default();
        searcher.search(&q, &mut c).unwrap();
        // 期望：显式 per-doc 逐段驱动（复刻旧 driver，含 matches）
        let mut expect = 0u64;
        {
            let mut s2 = Searcher::open(&fsdir).unwrap();
            for (_base, seg) in s2.leaves_for_test() {
                if let Some(mut it) = q.segment_iterator(seg, false).unwrap() {
                    loop {
                        if it.next_doc().unwrap() == NO_MORE_DOCS {
                            break;
                        }
                        if it.matches().unwrap() {
                            expect += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(c.count, expect, "query {q:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
```

注：`Reader::leaves()` 是 pub(crate)（reader.rs）——`Searcher` 无公开 leaves 访问。解法二选一（实现时按代码现状选）：(a) `Searcher` 加 `#[cfg(test)] pub fn leaves_for_test(&mut self)` 暴露 `self.reader.leaves()` 迭代；(b) 用 `Searcher::count`（per-doc 回退？count 也走 block……）——用 (a)。同时确认 `Query::term/and/or/bool` 构造器签名（query.rs 公开 API）与 `FieldValue::keyword` 存在——按编译器报错修正 import/签名，语义不变。

- [ ] **Step 8: 跑测试确认通过**

Run: `cargo test -p rustlucene-core 2>&1 | tail -3`
Expected: 全绿（283 + 2 = 285 passed）

Run: `cargo test --workspace 2>&1 | tail -3`
Expected: 全绿（287 passed：core 102 + codec 185... 以实际基线为准，0 failed）

- [ ] **Step 9: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/search/block_tests.rs crates/core/src/search/searcher.rs
git commit -m "$(cat <<'EOF'
feat(batch): 组合器覆写（二）Conjunction/Disjunction/RoaringAnd/RoaringOr

spec 2026-07-26 Task 6：PostingsIter::next_block 接 codec 批读；
DocSource::next_docs + SourceCursor 供 Roaring 组合器块归并/交 +
probes contains 过滤。至此除 Phrase/Bitset（默认 fill，设计记录）外
全部 SegmentDocIter 变体真块。searcher 级多段集成对拍。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: top_docs 块路径 + TopDocCollector / FreqSumCollector 覆写

**Files:**
- Modify: `crates/core/src/search/searcher.rs`（top_docs :86-122）
- Modify: `crates/core/src/search/collector.rs`（TopDocCollector :47-54；FreqSumCollector :63-70）
- Modify: `crates/core/src/search/block_tests.rs`（collector 块语义测试）

**Interfaces:**
- Consumes: `drive_blocks`、叶子/组合器覆写（Task 1-6）
- Produces: 三 driver 全部块化；`TopDocCollector::collect_block`（total += len + 块前缀补满 n）、`FreqSumCollector::collect_block`（freqs 求和）

- [ ] **Step 1: 写失败测试——collector 块语义**

`block_tests.rs` 尾部追加：

```rust
use super::collector::{FreqSumCollector, TopDocCollector};

#[test]
fn top_doc_collector_block_per_doc_parity() {
    for top_n in [1usize, 5, 128, 129, 300, 1000] {
        let docs: Vec<u32> = (0..300).map(|i| i * 7).collect();
        // 逐 doc 参照
        let mut ref_c = TopDocCollector::new(top_n);
        for &d in &docs {
            ref_c.collect(d as i32, 1);
        }
        // 块路径（128 一块 + 尾块）
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
    assert_eq!(c2.total_freq, 3); // freq 恒 1
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p rustlucene-core top_doc_collector 2>&1 | tail -5`
Expected: 编译错误——`TopDocCollector::collect_block` / `FreqSumCollector::collect_block` 未定义（默认回退存在但测试需要覆写？——默认回退语义正确，测试可能 PASS；确认 PASS 后把覆写当性能改动加，测试作回归守卫）

- [ ] **Step 3: collector 覆写**

`impl Collector for TopDocCollector`（collector.rs :47-54）内 `collect` 之后追加：

```rust
    fn collect_block(&mut self, docs: &[u32], _freqs: Option<&[u32]>) {
        self.total += docs.len() as u64;
        let room = self.top_n.saturating_sub(self.docs.len());
        let take = room.min(docs.len());
        self.docs.extend(docs[..take].iter().map(|&d| d as i32));
    }
```

`impl Collector for FreqSumCollector`（:63-70）内追加：

```rust
    fn collect_block(&mut self, _docs: &[u32], freqs: Option<&[u32]>) {
        match freqs {
            Some(f) => self.total_freq += f.iter().map(|&x| x as u64).sum::<u64>(),
            None => self.total_freq += _docs.len() as u64,
        }
    }
```

- [ ] **Step 4: top_docs 块路径**

`top_docs` 方法（searcher.rs :86-122）的迭代段循环改为（保留 fast-count 短路语义逐条）：

```rust
    pub fn top_docs(&mut self, query: &Query, n: usize) -> io::Result<(u64, Vec<i32>)> {
        let mut total = 0u64;
        let mut docs: Vec<i32> = Vec::with_capacity(n.min(1024));
        for (doc_base, seg) in self.reader.leaves() {
            let fast = query::fast_segment_count(seg, query)?;
            if let Some(c) = fast {
                total += c;
            }
            if docs.len() >= n && fast.is_some() {
                continue;
            }
            let Some(mut iter) = query.segment_iterator(seg, false)? else {
                continue;
            };
            if block_enabled() {
                let mut out = DocBlockBuf::new();
                loop {
                    if docs.len() >= n && fast.is_some() {
                        break;
                    }
                    let cnt = iter.next_block(&mut out)?;
                    if cnt == 0 {
                        break;
                    }
                    if fast.is_none() {
                        total += cnt as u64;
                    }
                    for &d in &out.docs[..cnt] {
                        if docs.len() >= n {
                            break;
                        }
                        docs.push(doc_base + d as i32);
                    }
                }
                continue;
            }
            loop {
                if docs.len() >= n && fast.is_some() {
                    break;
                }
                let doc = iter.next_doc()?;
                if doc == NO_MORE_DOCS {
                    break;
                }
                if !iter.matches()? {
                    continue;
                }
                if fast.is_none() {
                    total += 1;
                }
                if docs.len() < n {
                    docs.push(doc_base + doc);
                }
            }
        }
        Ok((total, docs))
    }
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test --workspace 2>&1 | tail -3`
Expected: 全绿（基线 + 本计划新增全部，0 failed）

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/search/searcher.rs crates/core/src/search/collector.rs crates/core/src/search/block_tests.rs
git commit -m "$(cat <<'EOF'
feat(batch): top_docs 块路径 + TopDocCollector/FreqSumCollector 块覆写

spec 2026-07-26 Task 7：INDEXORDER 块前缀补满 n + fast-count 短路逐条
保留；三 driver 全部块化完成。collector 块/逐 doc 对拍（top_n 跨块
边界 1/5/128/129/300/1000）。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: 1M 回归电池（block ON 零回归 + RL_BLOCK=0 逐位一致）

**Files:**
- 产出（不入库）：`/tmp/regress-1m-*.tsv`
- Modify（仅当发现回归时）：对应引擎文件

**Interfaces:**
- Consumes: 现有 `/tmp/boolbench-idx`（1M，2 段）、`/tmp/boolq.txt`（715 条）、报告附录复现命令
- Produces: 回归结论文本（贴入 Task 10 报告 §12 附录）；P1-1/P1-3 重点行零回归证据（spec §0 硬约束 3）

- [ ] **Step 1: release 构建**

Run: `cargo build --release 2>&1 | tail -2`
Expected: 构建成功

- [ ] **Step 2: block ON × roaring × 三模式**

Run:
```bash
cd /home/yjw/lucene-rust
./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 > /tmp/regress-1m-roaring.tsv 2>&1
./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 --no-fast-count > /tmp/regress-1m-roaring-iter.tsv 2>&1
./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 --topn 10 > /tmp/regress-1m-roaring-topn.tsv 2>&1
```
Expected: 三个 TSV 生成，无报错

- [ ] **Step 3: block OFF × roaring × no-fast（逃生门逐位一致验证）**

Run:
```bash
RL_BLOCK=0 ./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 --no-fast-count > /tmp/regress-1m-roaring-iter-noblock.tsv 2>&1
```

逐 query hits diff（必须为空——spec 硬约束 1）：
```bash
diff <(grep 'bucket=' /tmp/regress-1m-roaring-iter.tsv | awk -F'\t' '{print $1"\t"$3}' | sort) \
     <(grep 'bucket=' /tmp/regress-1m-roaring-iter-noblock.tsv | awk -F'\t' '{print $1"\t"$3}' | sort)
```
Expected: 无输出（0 差异）

- [ ] **Step 4: PFOR 路径 block ON/OFF 对账**

Run:
```bash
RL_BITMAP=0 ./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 --no-fast-count > /tmp/regress-1m-pfor-iter.tsv 2>&1
RL_BITMAP=0 RL_BLOCK=0 ./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 --no-fast-count > /tmp/regress-1m-pfor-iter-noblock.tsv 2>&1
diff <(grep 'bucket=' /tmp/regress-1m-pfor-iter.tsv | awk -F'\t' '{print $1"\t"$3}' | sort) \
     <(grep 'bucket=' /tmp/regress-1m-pfor-iter-noblock.tsv | awk -F'\t' '{print $1"\t"$3}' | sort)
```
Expected: 无输出

- [ ] **Step 5: P1-1/P1-3 重点行性能比对**

提取重点桶 p50（与报告 §9/§10 基线 c51e27b 比，±10% 内）：

```bash
for f in /tmp/regress-1m-roaring-iter.tsv /tmp/bench-rust-roaring-iter.tsv; do
  echo "== $f"; grep -E "^(mnfhh|multinot|nothi|rngmust|rngnot|bool|orcross)" "$f" | awk -F'\t' '{print $1, $2, $4}'
done
```

Expected: block ON 新跑数 vs `/tmp/bench-rust-roaring-iter.tsv`（c51e27b 基线）：mnfhh/mnfmh/multinot/nothi/rngmust/rngnot 各桶 p50 变化 ±10% 内（roaring 路径本就走 materialize fold，块化影响应近零）；PFOR 桶（`/tmp/regress-1m-pfor-iter.tsv` vs `/tmp/bench-rust-pfor-iter.tsv`）预期**下降**（块化收益）——记录倍数，Task 10 报告引用。

- [ ] **Step 6: count 模式逐 query 对账**

```bash
diff <(grep 'bucket=' /tmp/regress-1m-roaring.tsv | awk -F'\t' '{print $1"\t"$3}' | sort) \
     <(grep 'bucket=' /tmp/bench-rust-roaring.tsv | awk -F'\t' '{print $1"\t"$3}' | sort)
```
Expected: 无输出

- [ ] **Step 7: 记录结论 + Commit（仅数据记录，无代码改动则跳过 commit）**

把 Step 3-6 的 diff 结果与 Step 5 对照表写入临时笔记 `/tmp/regress-1m-result.txt`（Task 10 报告引用）。若本任务触发任何代码修复，单独 commit 并在 message 注明回归修复。

---

### Task 9: 5M 单段索引 + 15-cell 矩阵 + 对账 + perf

**Files:**
- 产出（不入库）：`/tmp/boolbench-5m/`（索引）、`/tmp/bench-5m-*.tsv`、`/tmp/bench-5m-*.txt`（Java detail）
- 前置检查：`df -h /tmp`（≥5GB 空闲）

**Interfaces:**
- Consumes: 全部引擎改动（Task 1-7）；`logwrite`/`forcemerge`/`searchbench` CLI；`interop/java/classes` 的 SearchBench
- Produces: 5M 三路三模式 × block on/off 原始数据（Task 10 报告输入）；5M 四路对账（spec §7-3）

- [ ] **Step 1: 磁盘检查 + 构建 5M 索引**

Run:
```bash
df -h /tmp | tail -1
cd /home/yjw/lucene-rust
time ./target/release/rustlucene-cli logwrite /tmp/boolbench-5m 5000000 42 --bitmap
```
Expected: 写入成功（预计 2-5 分钟）；多段产生（默认 flush 护栏）

- [ ] **Step 2: forcemerge 成单段（必带 --bitmap）**

Run:
```bash
time ./target/release/rustlucene-cli forcemerge /tmp/boolbench-5m --bitmap
ls /tmp/boolbench-5m/*.si
```
Expected: 单个 `_N.si`（spec §6.1 验收）。`--bitmap` 必带——否则合并段无 RLBM 内联 bitmap，roaring 口径失真。

- [ ] **Step 3: 索引验收（maxDoc / delCount / RLBM 存在）**

Run:
```bash
./target/release/rustlucene-cli searchbench /tmp/boolbench-5m message \
  --load-queries /tmp/boolq.txt --warmup 1 --iter 1 2>&1 | head -3
ls /tmp/boolbench-5m/ | grep -c RLBM
du -sh /tmp/boolbench-5m
```
Expected: searchbench 头部显示 1 segment / maxDoc=5000000（若 CLI 不打印段数，用 `ls *.si | wc -l` = 1 验证）；RLBM 文件存在（df≥4096 词有内联 bitmap）；体积 ≈1.1-1.2GB

- [ ] **Step 4: Rust roaring × 三模式 × block on/off（6 cells）**

Run:
```bash
B=/home/yjw/lucene-rust/target/release/rustlucene-cli
for mode in "" "--no-fast-count" "--topn 10"; do
  suf=$(echo "$mode" | sed 's/--no-fast-count/iter/; s/--topn 10/topn/; s/ //')
  $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt \
    --warmup 3 --iter 10 $mode > /tmp/bench-5m-roaring${suf:+-$suf}.tsv 2>&1
  RL_BLOCK=0 $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt \
    --warmup 3 --iter 10 $mode > /tmp/bench-5m-roaring${suf:+-$suf}-noblock.tsv 2>&1
done
```
Expected: 6 个 TSV 生成

- [ ] **Step 5: Rust PFOR × 三模式 × block on/off（6 cells）**

Run: 同 Step 4，前置 `RL_BITMAP=0`（输出文件名 `bench-5m-pfor-*`）：
```bash
for mode in "" "--no-fast-count" "--topn 10"; do
  suf=$(echo "$mode" | sed 's/--no-fast-count/iter/; s/--topn 10/topn/; s/ //')
  RL_BITMAP=0 $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt \
    --warmup 3 --iter 10 $mode > /tmp/bench-5m-pfor${suf:+-$suf}.tsv 2>&1
  RL_BITMAP=0 RL_BLOCK=0 $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt \
    --warmup 3 --iter 10 $mode > /tmp/bench-5m-pfor${suf:+-$suf}-noblock.tsv 2>&1
done
```

- [ ] **Step 6: Rust 块开/关逐 query 对账（必须 0 差异 × 12 文件对）**

Run:
```bash
for base in roaring roaring-iter roaring-topn pfor pfor-iter pfor-topn; do
  echo "== $base"
  diff <(grep 'bucket=' /tmp/bench-5m-$base.tsv | awk -F'\t' '{print $1"\t"$3}' | sort) \
       <(grep 'bucket=' /tmp/bench-5m-$base-noblock.tsv | awk -F'\t' '{print $1"\t"$3}' | sort) | head -5
done
```
Expected: 六个 `==` 行全部无 diff 输出（spec 硬约束 2）

- [ ] **Step 7: Java × 三模式（3 cells）**

Run:
```bash
CP=/home/yjw/lucene-rust/interop/java/classes:$(ls /home/yjw/lucene-rust/interop/java/lucene-*.jar | tr '\n' ':')
JB=/home/yjw/lucene-rust/interop/java
for mode in "" "--no-fast-count" "--topn 10"; do
  suf=$(echo "$mode" | sed 's/--no-fast-count/iter/; s/--topn 10/topn/; s/ //')
  java -Xmx2g -cp "$CP" SearchBench /tmp/boolbench-5m message \
    --load-queries /tmp/boolq.txt --no-cache --warmup 3 --iter 10 $mode \
    > /tmp/bench-5m-java${suf:+-$suf}.tsv 2> /tmp/bench-5m-java${suf:+-$suf}-detail.txt
done
```
Expected: 3 TSV + 3 detail 文件；Java 堆 5M 索引给 2g（1M 用 512m，线性放大）

- [ ] **Step 8: Rust↔Java 逐 query 对账（排除已知 term 采样差异）**

Run:
```bash
for suf in "" "-iter" "-topn"; do
  echo "== mode$suf"
  diff <(grep 'bucket=' /tmp/bench-5m-roaring$suf.tsv | awk -F'\t' '{print $1"\t"$3}' | sort) \
       <(grep 'bucket=' /tmp/bench-5m-java$suf-detail.txt | awk -F'\t' '{print $1"\t"$3}' | sort) \
    | grep -v '^term[s]*=' | head -10
done
```
Expected: 过滤 term 桶后 0 差异（term 差异 = Java 采样 count 已知行为，报告 §7 记录）。有非 term 差异 → 停，排查（正确性优先于 bench）。

- [ ] **Step 9: perf 尝试（PMU 可用时；否则记录降级）**

Run:
```bash
perf stat -e cycles,instructions,branches,branch-misses \
  $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt \
  --warmup 1 --iter 5 --no-fast-count 2>&1 | grep -E "cycles|instructions|branch|elapsed"
```
Expected: 若输出 `<not supported>` → 记录"bench 机 PMU 不可用，证据等级 = 墙钟 + 软件火焰图"（spec §6.4），改跑：
```bash
perf record -e cpu-clock -g -o /tmp/perf-5m-block.data -- \
  $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt --warmup 1 --iter 5 --no-fast-count
RL_BLOCK=0 perf record -e cpu-clock -g -o /tmp/perf-5m-noblock.data -- \
  $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt --warmup 1 --iter 5 --no-fast-count
perf report -i /tmp/perf-5m-block.data --stdio | head -30 > /tmp/perf-5m-block-top.txt
perf report -i /tmp/perf-5m-noblock.data --stdio | head -30 > /tmp/perf-5m-noblock-top.txt
```
PMU 可用时：PFOR 热点桶（`RL_BITMAP=0`，no-fast）block on/off 各跑 `perf stat`，记录 branches/branch-misses 差。

- [ ] **Step 10: 汇总数据摘要**

Run:
```bash
for f in /tmp/bench-5m-*.tsv; do echo "== $f"; head -12 "$f" | tail -10; done > /tmp/bench-5m-summary.txt
```
Expected: 摘要文件生成，供 Task 10 报告引用

---

### Task 10: 报告 §12 + Phase 2 决策备忘

**Files:**
- Modify: `docs/bool-bench-report.md`（追加 §12）

**Interfaces:**
- Consumes: Task 8/9 全部数据文件
- Produces: 报告 §12（5M 方法论 + block on/off 表 + 1M↔5M 标度 + Phase 2 决策）；spec §6.5 交付完成

- [ ] **Step 1: 汇总关键数字**

从 `/tmp/bench-5m-summary.txt` + Task 8 笔记提取：每桶 p50 × 15 cells；hits × 桶（5M 规模）；PFOR block on/off 提速倍数；perf 数据（或降级说明）。

- [ ] **Step 2: 写 §12**

在 `docs/bool-bench-report.md` 尾部追加 §12，结构：

```markdown
## 12. 批量迭代（块级 DocIter）——5M 单段 bench（2026-07-xx）

### 12.1 方法论
- 索引：logwrite 5M seed=42 --bitmap + forcemerge --bitmap → 单段 5,000,000 docs / ≈X GB
- 查询集：/tmp/boolq.txt 715 条复用（df 等比 ×5，level 词 df≈1M，选择率 ~20% 不变）
- 矩阵：roaring/pfor × block on/off × 三模式 + Java 基线；warmup 3 / iter 10
- 引擎改动：spec 2026-07-26（commit 链）；RL_BLOCK=0 逃生门逐 query 对账 0 差异
- perf 证据等级：[PMU 数据 | 墙钟+软件火焰图降级说明]

### 12.2 block on/off 对照（no-fast 全量迭代，p50 µs）
[表格：形状桶 × {roaring on, roaring off, 倍数, pfor on, pfor off, 倍数, java}]

### 12.3 count / topN 模式
[同构两表]

### 12.4 1M↔5M 标度
[重点桶 1M→5M 放大系数 vs 线性 5× 的偏离分析]

### 12.5 与 demo 预估对照
demo 预估 PFOR 3-10× / roaring 1.5-4×（spec §1.2）→ 实测 [X]；差异归因。

### 12.6 Phase 2 决策备忘
- profile 热点：[代数内核占比 / 解码占比 / collect 占比]
- 决策：[做/不做 SIMD；做哪几个内核；理由]
- 设计偏差记录：BitsetDocIter 默认 fill（Task 3）实测影响 [有/无]

### 12.7 复现命令
[Task 9 命令精简版]
```

注：数据缺失处用实测填充，勿留占位符；跑数日期写实际日期。

- [ ] **Step 3: 提交报告**

```bash
git add docs/bool-bench-report.md
git commit -m "$(cat <<'EOF'
docs: bool bench 报告 §12——5M 单段批量迭代 bench 结果 + Phase 2 决策备忘

spec 2026-07-26 收口：block on/off × 三路三模式对照、1M↔5M 标度、
demo 预估对照、RL_BLOCK 逃生门 0 差异证据。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 4: 收尾检查**

Run:
```bash
cargo test --workspace 2>&1 | tail -3
git log --oneline -12
git status --short
```
Expected: 测试全绿；commit 链 Task 1-7 + 10 完整；工作区仅剩有意排除项（`.cargo/config.toml`、`rust-toolchain.toml`——历史遗留，不属本计划）

---

## Self-Review 记录

**Spec 覆盖**：§0 硬约束 → Task 8（约束 1/3）、Task 9 Step 6/8（约束 2/4）；§2 架构 → Task 1/3/5/6；§3 trait 形状 → Task 1（+ DocBlockBuf::new 构造器补强）；§4 代数 → Task 4/5/6（Bitset 偏差 Task 3 记录，RoaringAnd probes 过滤 Task 6）；§5 driver → Task 1/7；§6 bench → Task 9/10；§7 电池 → Task 1-7 单测 + Task 8/9 集成；§8 排序 → Task 1→10 一致；§9 风险 → Task 8 Step 5（低 df 桶监视）、Task 9 Step 1（磁盘）、Task 9 Step 9（PMU 降级）；§10 Phase 2 → Task 10 §12.6。

**已知实现期决策点**（非占位符，已在任务内注明解法）：Task 6 Step 7 leaves_for_test 暴露方式；Task 6 借用冲突（heads/consume 时序、curs/sub split）解法已内嵌代码。

**自审修正记录**：(1) n 元合取初版的 scratch 左折叠有消费坐标 bug（第 2 轮起 `block_intersect` 的 ca 是 scratch 坐标而非 child0 块坐标，游标会欠消费）——已改为"二元 slice 快路径 + n 元逐元素 `position_*` 窗口定位"，正确性由前向单调性保证；(2) spec §4 的 (1)-(3) 不变量由对拍测试（40 轮随机 + 四象限 + searcher 级多段）差分执行，不加运行时 debug_assert（跨调用单调性断言要给 trait 加状态字段，YAGNI）；(3) `block_intersect` 的 Phase 1 消费方 = 三处二元快路径（and 两子句是 bench 主力形状）。
