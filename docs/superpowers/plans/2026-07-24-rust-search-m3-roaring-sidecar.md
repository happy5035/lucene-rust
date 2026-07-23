# M3 高 df term 的 Roaring bitmap sidecar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 M2（multi-term + phrase）之上新增 M3：segment flush 时对 df ≥ 4096 的 term 构建三容器 RoaringBitmap（array <4096 / bitset ≥4096 / runOptimize 后 run），写 per-segment sidecar 文件 `rbm<seg>.bin`（永不列入 segments_N/.si）；搜索时 Term/And/Or 按 spec §5 三档规则走 roaring 容器运算（档 1 全 bitmap、档 2 混合查询时物化、档 3 纯低 df 走既有 PFOR 不动）。自研容器子集（不引 roaring crate），标量先行、AVX2 等价追加；`make log-test` 加 `--bitmap` 第五变体全绿 + bitmap on/off searchdump diff 为空 + Java CheckIndex "No problems" 收尾。对应已批准 spec `docs/superpowers/specs/2026-07-23-rust-search-m3-roaring-sidecar-design.md` 的全部范围（含 §4a sidecar 兼容性契约——每条命名/GC/校验规则均为绑定）。

**Architecture:** 延续方案 C（算法语义照抄 9.12.3、对象结构 Rust 化）。roaring 容器库与 sidecar 文件 FORMAT（写 + 读 + 校验 + GC）都在 codec 层（`crates/codec-lucene9/src/roaring.rs` + `roaring/simd.rs` + `bitmap_sidecar.rs`），core 层只拥有执行（三档规则、`RoaringDocIter`、查询时物化、And/Or 粘合），core 无任何 unsafe。sidecar 布局（自定格式，无需跨实现兼容）：CodecUtil 惯例 header（magic/version/segment id）+ segment name + maxDoc + field 分区表 + 每 field 有序 term 表 {term bytes, df, cardinality, payload offset/len} + payload 区 + footer crc32；open 只读 header + term 表（KB 级），payload 首触及才加载，count 查询只读 term 表。读侧自动探测 sidecar 存在性；Java merge 产出的 segment 无 sidecar → per-segment 自然落档 postings（Rust 无 merge，不做 merge 侧 bitmap 构建）。验证三层不变：容器/文件 round-trip 单测（codec）→ 三档语义测试（core `search/mod.rs`）→ Java diff 终验 + 三路 bench 报告（`--no-cache`）。

**Tech Stack:** Rust（codec crate edition 2024、core crate edition 2021；codec `#![deny(unsafe_code)]` + 仅 `postings_ll/simd.rs`、`roaring/simd.rs` 两个模块级 `#[allow(unsafe_code)]`，core `#![forbid(unsafe_code)]`，统一 `io::Result`）；不新增依赖（容器子集自研，spec §7）；Java 9.12.3（`interop/java/lib/lucene-core-9.12.3.jar`）做 diff 基准与 CheckIndex 终验；格式语义以 `reference/lucene-9.12.3/` 源码为准。

## Global Constraints

（摘自 spec §3/§4a/§5/§6/§7 与既有项目惯例，逐字或就近转述；所有 Task 共同遵守）

- **df 阈值 4096**：`DEFAULT_BITMAP_THRESHOLD = 4096`（对齐 level-1 skip 粒度 32×128，spec §3），`--bitmap-threshold N` 可调。
- **`--bitmap` 默认 off**（spec §3 实验期开关）；读侧自动探测 sidecar 存在性，CLI `--no-bitmap` 只用于同一索引上的 bench A/B。
- **不新增 crate 依赖**（spec §7：自研容器子集 ~500–700 行含测试；现有依赖 crc32fast/lz4/rand/serde_json 不动）。
- **sidecar 命名 `rbm<seg>.bin`**（segment `_7` → `rbm_7.bin`，spec §4a.3）；**永不列入 `.si` files 集合**（§4a.2）；**Rust 侧 GC** 按 §4a.4：IndexWriter open 时与 flush 后 listAll，删除 `rbm` 前缀且 segment 名不在 live segments 的文件。
- **unsafe 边界**：codec crate 保持 `#![deny(unsafe_code)]`，模块级 `#[allow(unsafe_code)]` 只出现在既有 `crates/codec-lucene9/src/postings_ll/simd.rs` 与新增 `crates/codec-lucene9/src/roaring/simd.rs`（分发/安全论证模式照搬前者）；core crate 保持 `#![forbid(unsafe_code)]`——core 任何新代码不得引入 unsafe。
- **commit message 前缀**：`feat:` / `test:` / `bench:` / `docs:`（沿用 git log 现有风格）。
- **验收门槛**：`make log-test` 全部变体绿（既有四变体 seed 42 默认 / 43 `--positions` / 44 `--sparse` / 45 `--bigdict`，加新第五变体 seed 46 `--bitmap`）+ bitmap on/off searchdump diff 为空 + Java CheckIndex 对带 sidecar 的索引 "No problems"（且 CheckIndex 运行后 sidecar 文件仍在、字节不变，§4a.3/5）。
- **Rust edition 分工**：`crates/codec-lucene9` edition 2024，`crates/core` edition 2021（各自 Cargo.toml 已声明，新增代码遵守，不得改动）。
- **测试命令**：codec 层 `cargo test -p codec-lucene9 <test名>`，core 层 `cargo test -p rustlucene-core <test名>`；`make log-test` 较慢，只在 T6 电池任务与最终收尾使用。
- **Lucene 语义照抄**：执行语义逐行对照 9.12.3 源码，关键决策在代码注释中给 `File.java:line` 引用（Java 源码在 `reference/lucene-9.12.3/lucene/core/src/java/org/apache/lucene/`，下文引用省略该前缀；§4a 的引用逐条沿用 spec）。
- **不做**（spec §3 明确否决/二期）：bitmap 带 freq/positions；multi-term（M2 >16 FixedBitSet 路径）的 roaring 集成；纯低 df 布尔查询统一走 roaring；查询结果缓存；为 Java 写的索引提供 bitmap；merge 侧 bitmap 构建。
- **实验/报告文件**：bench 数据与报告写 `.superpowers/sdd/`（gitignored），不进 git；bench 一律 `--no-cache` 口径。

## Pre-checks（已执行，基线绿）

```
$ cargo build
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.04s
$ cargo test -p codec-lucene9
test result: ok. 142 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
$ cargo test -p rustlucene-core
test result: ok. 39 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## 关键设计事实（本计划全部代码的字节级依据，已逐项对照 spec §4a 引用与本仓库源码核实）

1. **构建钩子点**（spec §4）：flush 时 term 的 docs 全量切片本就在内存——`crates/core/src/segment_builder.rs:161-169` 的 term 循环把 `&pb.docs` 交给 `PostingsWriter::write_term`（`crates/codec-lucene9/src/postings.rs:338-346`，`debug_assert` 升序）。M3 在同一循环体内、`df >= threshold` 时顺手 `RoaringBitmap::from_sorted_docs(&pb.docs)`，O(df) CPU、零额外 IO。字段按 field number 顺序、term 按字典序到达（`dict.sorted_ids()`），sidecar term 表天然有序。
2. **命名/托管命名空间契约**（§4a.2/3，绑定）：`rbm<seg>.bin` 不匹配 `IndexFileNames.java:199` 的 `CODEC_FILE_PATTERN = _[a-z0-9]+(_.*)?\..*`（首字符不是 `_`），也不以 `segments`/`pending_segments` 开头 → `IndexFileDeleter.java:146-151` 完全不 track（先例：write.lock 正是靠在命名空间之外而存活）。`.si` files 集合被持久化并被 Java 用于 CFS 打包与引用计数（`IndexWriter.java:5876-5900`）→ sidecar 永不列入。反例警戒：`_7.rbm` 会被下一次 Java IndexWriter open 删除（`IndexFileDeleter.java:226-236`），`_7_rust.bitmap` 还会 gen 膨胀（`IndexFileDeleter.java:314-321`）。
3. **必须独立文件 + CheckIndex 零感知**（§4a.1/5）：codec 文件 footer CRC 覆盖全文件且 `validateFooter` 显式拒绝尾随字节（`CodecUtil.java:572-580`），header magic 拒绝前置字节（`CodecUtil.java:46,182`）——向任何现有文件追加/嵌入 = Java 读即 CorruptIndexException。CheckIndex 只枚举 `segments*` 文件、per-segment 只查 `.si` 引用集（`CheckIndex.java:599,618-623,986-1025`），自身从不删除文件 → 带 sidecar 的索引仍 "No problems were detected"（`CheckIndex.java:916-917`）。
4. **绑定与失效兜底**（§4a.6）：sidecar header 记 {magic, version, segment name, maxDoc}，footer crc32；term 表记 df/cardinality。读侧 open 校验 segment 名 + maxDoc；term 级要求 cardinality == postings df（读侧本就知道 df）。任一不匹配 → 静默丢弃该 bitmap、落档 postings（§5 档 2/3），查询永不受坏 sidecar 影响。
5. **CodecUtil 惯例**：本项目 codec 文件 header/footer 走 `crates/codec-lucene9/src/codec_util.rs` 的 `write_index_header`（BE magic + codec 名 + BE version + 16B id + suffix）与 `write_footer`（BE FOOTER_MAGIC + algorithmID 0 + CRC32，覆盖全文件）；文件 body 一律 LE（`io.rs` 的 write_short/write_long）。sidecar 同惯例（codec 名 `RustRbmSidecar`，version 0，suffix `""`）；读侧用 `check_index_header` + `check_footer_structure`（`crates/codec-lucene9/src/postings_read.rs:62-72` 同模式）。payload offset 相对 payload 区起点 → term 表可先写、无循环依赖。
6. **三档钩子点**（spec §5）：`crates/core/src/search/query.rs` Term 分支（`query.rs:126-137`）、And 分支（:138-161）、Or 分支（:162-187）——档判定在每个分支 seek 完 term 之后、构造现有 iterator 之前；档 3（无任何子句有 bitmap）完全走既有 `ConjunctionDocIter`/`DisjunctionDocIter`（df 升序排序、lead 选择保持不动，M1 合取已调优）。规则 per-segment 独立生效（M1 既定 per-segment 执行）。
7. **needs_freq 纪律**：bitmap 不带 freq（spec §2）——roaring 分支只在 `!needs_freq` 时进入。`Searcher::freq_sum` 的 Term 短路走 `total_term_freq`（`crates/core/src/search/searcher.rs:113-121`）本就不需要迭代；And/Or 的 `freq()` 本来就是 placeholder（`crates/core/src/search/doc_iter.rs:218-223,301-310`），唯一需要 freq 的 collector（FreqSumCollector）拒收 And/Or（`searcher.rs:106-112`）。
8. **count 路径**：Term 的 `Searcher::count` 已是 O(1)（`searcher.rs:61-69` 直接加 doc_freq），roaring 不改变它；收益在迭代路径（top_docs/search 驱动、searchbench iterm）与 And/Or 的 CountCollector 迭代。term 表的 cardinality 让 sidecar 的 count 连 payload 都不用加载（spec §4）。
9. **查询时物化同源**（spec §5 档 2）：M2 的 `multi_term::materialize`（`crates/core/src/search/multi_term.rs:275-303`）用 no-freq enum 升序全扫置位；M3 的 `materialize_roaring` 用**同一扫描纪律**（no-freq enum、升序收集）产出 `RoaringBitmap`。档 2 中被物化的子句 df < 4096 → 成本 ≤4095 doc ≈ 32 个 PFOR 块，有界微秒级。
10. **容器语义**（spec §4，照 Roaring 论文 Chambi et al.）：doc 按高 16 位分桶；桶内 cardinality < 4096 → array 容器（u16 有序数组），≥ 4096 → bitset 容器（1024 u64 = 8KB）；构建后与每次 and/or 后 runOptimize（连续区间转 run 容器；按序列化体积判定：run 2+4R bytes vs array 2C bytes vs bitset 8192 bytes，小者胜）。交/并容器对分发（spec §5）：array∩array galloping、bitset∩bitset 字运算 + popcount、run∩run 双指针；Or 对偶。结果仍是 roaring，直接迭代，不物化成数组；空容器立即丢弃。
11. **AVX2 纪律**（spec §6）：先标量参考实现（容器交/并/popcount），AVX2 快路径作等价追加。x86 AVX2 **无 VPOPCNT**（那是 AVX-512）→ popcount 用 nibble-LUT + PSADBW（Muła 惯用法）。运行时分发 + `RL_SIMD=0` kill switch + OnceLock 缓存照抄 `crates/codec-lucene9/src/postings_ll/simd.rs:52-62`；对拍单测钉死"标量 vs SIMD 逐位相等"；bench 数据门槛见 T6。
12. **log 语料 df 量级**（估计，决定电池/bench 覆盖面）：`gen_message` 200 字节 ≈ 25–33 token/doc，词表 60×40+5 = 2405 → 200k 文档时 message term df ≈ 2–3k < 4096，`make log-test` 的 `--bitmap` 变体的 sidecar 只覆盖 level 字段（df ≈ 40k ≥ 4096；term level=X 的 top20 与 and level=INFO,WARN 走 roaring）。档 2 混合场景由 T5 单测覆盖（小阈值人造语料）。bench 用 1M 文档（message term df ≈ 13.7k ≥ 4096）做 message 高 df AND/OR/iterm 三路对比。
13. **电池接线**：`interop/verify-log.sh` 目前把第三参（变体旗标）同时透传给 logwrite、JavaLogBench 与 verify-search.sh。`--bitmap` 是 Rust 私有旗标：JavaLogBench 对未知旗标静默忽略（`interop/java/JavaLogBench.java:73-76`，仅 if-equals 判断、无 else 报错），但 verify-search.sh 会把它传给 searchdump → 必须在 verify-log.sh 内把传给 verify-search.sh 的旗标过滤为仅 `--positions`。VerifyLogIndex 不枚举目录文件（已核实源码无 listAll/文件集比对）→ rbm 文件对 Java 读侧零感知。

---

## Task 1: roaring 容器库标量实现（`crates/codec-lucene9/src/roaring.rs` 新建）

三容器 + build/and/or/cardinality/迭代访问 + payload 序列化，全标量（spec §6 先标量）。AVX2 在 T2 追加。

**Files:**
- Create: `crates/codec-lucene9/src/roaring.rs`
- Modify: `crates/codec-lucene9/src/lib.rs`（`pub mod roaring;`）
- Test: `crates/codec-lucene9/src/roaring.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: `crate::io::{DataInput, DataOutput}`（`io.rs:519/779`）、`crate::codec_util::corrupt`（`codec_util.rs:69`）、测试用 `crate::io::{IndexInput, IndexOutput}`。
- Produces（T2 的 dispatch、T3 的 sidecar 写/读、T4/T5 的 core 执行层依赖这些名字，不得改名）:
  ```rust
  // roaring.rs
  pub const DEFAULT_BITMAP_THRESHOLD: u32 = 4096;   // spec §3
  pub const ARRAY_CONTAINER_THRESHOLD: usize = 4096; // spec §4

  pub enum Container {
      Array(Vec<u16>),                  // 有序，1..4096 个
      Bitset(Box<[u64; 1024]>),         // cardinality >= 4096
      Run(Vec<(u16, u16)>),             // (start, len) 闭区间，有序不相交不相邻
  }
  impl Container {
      pub fn cardinality(&self) -> usize;
      pub fn value_at(&self, i: usize) -> u16;      // 第 i 小（0 起）
      pub fn lower_bound(&self, x: u16) -> usize;   // 首个 >= x 的下标
  }

  pub struct RoaringBitmap { .. }                    // key 升序 (u16, Container)，无空容器
  impl RoaringBitmap {
      pub fn new() -> Self;
      pub fn from_sorted_docs(docs: &[u32]) -> Self;
      pub fn is_empty(&self) -> bool;
      pub fn cardinality(&self) -> u64;
      pub fn num_containers(&self) -> usize;
      pub fn container_key(&self, ci: usize) -> u16;
      pub fn container_at(&self, ci: usize) -> &Container;
      pub fn and(&self, other: &RoaringBitmap) -> RoaringBitmap;
      pub fn or(&self, other: &RoaringBitmap) -> RoaringBitmap;
      pub(crate) fn serialize(&self, out: &mut impl DataOutput) -> io::Result<()>;
      pub(crate) fn deserialize(input: &mut impl DataInput) -> io::Result<RoaringBitmap>;
  }
  ```

### Steps

- [ ] **Step 1.1: 写失败测试（构建/容器选择/迭代访问）** — 新建 `crates/codec-lucene9/src/roaring.rs`，先只放模块文档与测试（`RoaringBitmap`/`Container` 尚不存在，编译失败即失败测试成立）：

  ```rust
  //! Roaring bitmap containers (M3 spec §4/§7 — self-built subset, no
  //! external crate): three-container model per Chambi et al., *Better
  //! bitmap performance with Roaring bitmaps*. Docs bucket by high 16 bits;
  //! a bucket with cardinality < 4096 becomes an array container (sorted
  //! u16), >= 4096 a bitset container (1024 u64 words = 8KB); after build
  //! and after every and/or, runOptimize converts containers whose run
  //! encoding is smaller (log corpora: high-df terms hit long contiguous
  //! doc ranges, spec §4).

  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::io::IndexOutput;

      /// Flattens the bitmap through the public iteration surface
      /// (the same surface RoaringDocIter uses in core).
      fn collect(bm: &RoaringBitmap) -> Vec<u32> {
          let mut out = Vec::new();
          for ci in 0..bm.num_containers() {
              let key = bm.container_key(ci) as u32;
              let c = bm.container_at(ci);
              for i in 0..c.cardinality() {
                  out.push((key << 16) | c.value_at(i) as u32);
              }
          }
          out
      }

      fn range_docs(start: u32, end: u32) -> Vec<u32> {
          (start..end).collect()
      }

      fn stride_docs(start: u32, stride: u32, n: u32) -> Vec<u32> {
          (0..n).map(|i| start + i * stride).collect()
      }

      #[test]
      fn build_selects_expected_container_kinds() {
          // sparse small buckets -> Array; also crosses a key boundary
          let bm = RoaringBitmap::from_sorted_docs(&[1, 2, 3, 70_000, 131_071]);
          assert_eq!(bm.num_containers(), 2);
          assert_eq!(bm.container_key(0), 0);
          assert!(matches!(bm.container_at(0), Container::Array(_)));
          assert_eq!(bm.container_key(1), 1);
          assert!(matches!(bm.container_at(1), Container::Array(_)));
          assert_eq!(bm.cardinality(), 5);

          // contiguous -> runOptimize picks Run
          let bm = RoaringBitmap::from_sorted_docs(&range_docs(100, 201));
          assert_eq!(bm.num_containers(), 1);
          assert!(matches!(bm.container_at(0), Container::Run(_)));
          assert_eq!(bm.cardinality(), 101);

          // dense scattered (5000 evens) -> Bitset
          let bm = RoaringBitmap::from_sorted_docs(&stride_docs(0, 2, 5000));
          assert_eq!(bm.num_containers(), 1);
          assert!(matches!(bm.container_at(0), Container::Bitset(_)));
          assert_eq!(bm.cardinality(), 5000);
      }

      #[test]
      fn cardinality_and_value_iteration_across_keys() {
          let docs: Vec<u32> = [
              range_docs(0, 100),            // key 0, contiguous -> Run
              stride_docs(65_536, 2, 5000),  // key 1, dense evens -> Bitset
              vec![200_000],                 // key 3, singleton -> Array
          ]
          .concat();
          let bm = RoaringBitmap::from_sorted_docs(&docs);
          assert_eq!(bm.cardinality(), docs.len() as u64);
          assert_eq!(collect(&bm), docs);
          assert!(matches!(bm.container_at(0), Container::Run(_)));
          assert!(matches!(bm.container_at(1), Container::Bitset(_)));
          assert!(matches!(bm.container_at(2), Container::Array(_)));
      }

      #[test]
      fn container_value_at_and_lower_bound() {
          let arr = Container::Array(vec![3, 5, 9, 100]);
          assert_eq!(arr.value_at(2), 9);
          assert_eq!(arr.lower_bound(0), 0);
          assert_eq!(arr.lower_bound(3), 0);
          assert_eq!(arr.lower_bound(4), 1);
          assert_eq!(arr.lower_bound(9), 2);
          assert_eq!(arr.lower_bound(10), 3);
          assert_eq!(arr.lower_bound(100), 3);
          assert_eq!(arr.lower_bound(101), 4);

          let run = Container::Run(vec![(10, 2), (100, 0)]); // {10,11,12} and {100}
          assert_eq!(run.cardinality(), 4);
          assert_eq!(run.value_at(0), 10);
          assert_eq!(run.value_at(2), 12);
          assert_eq!(run.value_at(3), 100);
          assert_eq!(run.lower_bound(0), 0);
          assert_eq!(run.lower_bound(10), 0);
          assert_eq!(run.lower_bound(11), 1);
          assert_eq!(run.lower_bound(13), 3);
          assert_eq!(run.lower_bound(99), 3);
          assert_eq!(run.lower_bound(100), 3);
          assert_eq!(run.lower_bound(101), 4);

          let mut words = Box::new([0u64; BITSET_WORDS]);
          words[0] = 0b10110; // {1,2,4}
          words[1] = 1; // {64}
          let bs = Container::Bitset(words);
          assert_eq!(bs.cardinality(), 4);
          assert_eq!(bs.value_at(0), 1);
          assert_eq!(bs.value_at(2), 4);
          assert_eq!(bs.value_at(3), 64);
          assert_eq!(bs.lower_bound(0), 0);
          assert_eq!(bs.lower_bound(1), 0);
          assert_eq!(bs.lower_bound(2), 1);
          assert_eq!(bs.lower_bound(3), 2);
          assert_eq!(bs.lower_bound(5), 3);
          assert_eq!(bs.lower_bound(64), 3);
          assert_eq!(bs.lower_bound(65), 4);
      }
  }
  ```

  同时 `crates/codec-lucene9/src/lib.rs` 在 `pub mod postings_read;` 之后、`pub mod segment_info;` 之前插入一行 `pub mod roaring;`（否则模块未声明，测试无法编译）。

- [ ] **Step 1.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 roaring 2>&1 | tail -5
  error[E0433]: failed to resolve: use of unresolved module or unlinked crate `roaring`
  ...（Container / RoaringBitmap / BITSET_WORDS 未定义）
  ```

- [ ] **Step 1.3: 最小实现（构建 + 迭代访问）** — `crates/codec-lucene9/src/roaring.rs` 在模块文档之后、`#[cfg(test)]` 之前插入：

  ```rust
  use std::io;

  use crate::codec_util::corrupt;
  use crate::io::{DataInput, DataOutput};

  /// spec §3: terms with df >= this threshold get a sidecar bitmap at flush
  /// (4096 = 32×128, the level-1 skip granularity,
  /// Lucene912PostingsFormat.java:347-352); `--bitmap-threshold N` overrides.
  pub const DEFAULT_BITMAP_THRESHOLD: u32 = 4096;

  /// spec §4: bucket cardinality < 4096 -> array container, >= 4096 -> bitset.
  pub const ARRAY_CONTAINER_THRESHOLD: usize = 4096;

  /// Bits per container bucket (65536) in u64 words.
  const BITSET_WORDS: usize = 1024;
  const BITSET_BYTES: usize = BITSET_WORDS * 8;

  /// One bucket of the bitmap: the low 16 bits of a doc range.
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub enum Container {
      /// Sorted u16 values, 1..4096 entries (spec §4).
      Array(Vec<u16>),
      /// 8192 bytes of bits, cardinality >= 4096 (spec §4).
      Bitset(Box<[u64; BITSET_WORDS]>),
      /// (start, len) inclusive ranges, sorted, disjoint, non-adjacent
      /// (runOptimize, spec §4).
      Run(Vec<(u16, u16)>),
  }

  impl Container {
      pub fn cardinality(&self) -> usize {
          match self {
              Container::Array(v) => v.len(),
              Container::Bitset(w) => words_popcount(w),
              Container::Run(runs) => runs.iter().map(|&(_, l)| l as usize + 1).sum(),
          }
      }

      /// i-th smallest value (0-based); panics out of range.
      pub fn value_at(&self, i: usize) -> u16 {
          match self {
              Container::Array(v) => v[i],
              Container::Bitset(w) => {
                  let mut seen = 0usize;
                  for (wi, &word) in w.iter().enumerate() {
                      let c = word.count_ones() as usize;
                      if seen + c > i {
                          let mut rest = i - seen;
                          let mut word = word;
                          loop {
                              let b = word.trailing_zeros();
                              if rest == 0 {
                                  return (wi as u16) * 64 + b as u16;
                              }
                              word &= word - 1;
                              rest -= 1;
                          }
                      }
                      seen += c;
                  }
                  panic!("value_at index {i} out of range")
              }
              Container::Run(runs) => {
                  let mut seen = 0usize;
                  for &(s, l) in runs {
                      let len = l as usize + 1;
                      if seen + len > i {
                          return s + (i - seen) as u16;
                      }
                      seen += len;
                  }
                  panic!("value_at index {i} out of range")
              }
          }
      }

      /// Index of the first value >= x (== cardinality() when none).
      pub fn lower_bound(&self, x: u16) -> usize {
          match self {
              Container::Array(v) => v.partition_point(|&y| y < x),
              Container::Bitset(w) => {
                  let wi = (x >> 6) as usize;
                  let mut count: usize =
                      w[..wi].iter().map(|y| y.count_ones() as usize).sum();
                  count += (w[wi] & ((1u64 << (x & 63)) - 1)).count_ones() as usize;
                  count
              }
              Container::Run(runs) => {
                  let x = x as u32;
                  let mut count = 0usize;
                  for &(s, l) in runs {
                      let (s, e) = (s as u32, s as u32 + l as u32);
                      if x < s {
                          break; // first value >= x is this run's start, at `count`
                      }
                      if x > e {
                          count += (e - s + 1) as usize;
                      } else {
                          count += (x - s) as usize;
                          break;
                      }
                  }
                  count
              }
          }
      }
  }

  /// A sorted doc set as key-ascending (high-16-bits, container) pairs.
  /// Invariant: no empty containers, keys strictly ascending.
  #[derive(Clone, Debug)]
  pub struct RoaringBitmap {
      containers: Vec<(u16, Container)>,
  }

  impl RoaringBitmap {
      pub fn new() -> Self {
          RoaringBitmap { containers: Vec::new() }
      }

      /// Build from an ascending doc list (the flush-time postings layout,
      /// postings.rs:346 debug_assert). O(docs) CPU, zero IO (spec §4).
      pub fn from_sorted_docs(docs: &[u32]) -> Self {
          debug_assert!(docs.windows(2).all(|w| w[0] < w[1]), "docs must ascend");
          let mut containers: Vec<(u16, Container)> = Vec::new();
          let mut i = 0;
          while i < docs.len() {
              let key = (docs[i] >> 16) as u16;
              let mut j = i + 1;
              while j < docs.len() && (docs[j] >> 16) as u16 == key {
                  j += 1;
              }
              let vals: Vec<u16> = docs[i..j].iter().map(|&d| (d & 0xFFFF) as u16).collect();
              containers.push((key, container_from_sorted_values(vals)));
              i = j;
          }
          RoaringBitmap { containers }
      }

      pub fn is_empty(&self) -> bool {
          self.containers.is_empty()
      }

      pub fn cardinality(&self) -> u64 {
          self.containers.iter().map(|(_, c)| c.cardinality() as u64).sum()
      }

      pub fn num_containers(&self) -> usize {
          self.containers.len()
      }

      pub fn container_key(&self, ci: usize) -> u16 {
          self.containers[ci].0
      }

      pub fn container_at(&self, ci: usize) -> &Container {
          &self.containers[ci].1
      }
  }

  // ── container selection / runOptimize (spec §4) ──────────────────────

  /// Container choice for a freshly built bucket: array below 4096 values,
  /// bitset at/above, run whenever its encoding is smaller (runOptimize).
  /// `vals` is non-empty, sorted, deduplicated.
  fn container_from_sorted_values(vals: Vec<u16>) -> Container {
      debug_assert!(!vals.is_empty());
      if vals.len() < ARRAY_CONTAINER_THRESHOLD {
          let runs = runs_from_sorted_values(&vals);
          if run_cost(&runs) < 2 * vals.len() {
              return Container::Run(runs);
          }
          return Container::Array(vals);
      }
      let words = words_from_sorted_values(&vals);
      container_from_bitset(Box::new(words), vals.len()).expect("fresh bucket is non-empty")
  }

  /// Serialized-size proxy: run pays 2 + 4*num_runs "bytes", an array
  /// 2*card, a bitset a fixed 8192 (runOptimize 按体积判定).
  fn run_cost(runs: &[(u16, u16)]) -> usize {
      2 + 4 * runs.len()
  }

  /// Normalizes a computed bitset result: array when the cardinality
  /// dropped below the threshold, run when cheaper than the 8KB bitset.
  fn container_from_bitset(words: Box<[u64; BITSET_WORDS]>, card: usize) -> Option<Container> {
      if card == 0 {
          return None;
      }
      if card < ARRAY_CONTAINER_THRESHOLD {
          return Some(Container::Array(collect_values(&words)));
      }
      let runs = runs_from_bitset(&words);
      if run_cost(&runs) < BITSET_BYTES {
          return Some(Container::Run(runs));
      }
      Some(Container::Bitset(words))
  }

  fn runs_from_sorted_values(vals: &[u16]) -> Vec<(u16, u16)> {
      debug_assert!(!vals.is_empty());
      let mut runs = Vec::new();
      let mut start = vals[0];
      let mut prev = vals[0];
      for &v in &vals[1..] {
          if v as u32 == prev as u32 + 1 {
              prev = v;
          } else {
              runs.push((start, prev - start));
              start = v;
              prev = v;
          }
      }
      runs.push((start, prev - start));
      runs
  }

  fn words_from_sorted_values(vals: &[u16]) -> [u64; BITSET_WORDS] {
      let mut words = [0u64; BITSET_WORDS];
      for &v in vals {
          words[(v >> 6) as usize] |= 1u64 << (v & 63);
      }
      words
  }

  fn collect_values(words: &[u64; BITSET_WORDS]) -> Vec<u16> {
      let mut out = Vec::new();
      for (wi, &w) in words.iter().enumerate() {
          let mut w = w;
          while w != 0 {
              out.push((wi as u16) * 64 + w.trailing_zeros() as u16);
              w &= w - 1;
          }
      }
      out
  }

  fn runs_from_bitset(words: &[u64; BITSET_WORDS]) -> Vec<(u16, u16)> {
      let mut runs: Vec<(u16, u16)> = Vec::new();
      let mut cur: Option<(u16, u16)> = None; // (start, last set value)
      for (wi, &w) in words.iter().enumerate() {
          let mut w = w;
          while w != 0 {
              let v = (wi as u16) * 64 + w.trailing_zeros() as u16;
              match &mut cur {
                  Some((_, last)) if v as u32 == *last as u32 + 1 => *last = v,
                  _ => {
                      if let Some((s, l)) = cur.replace((v, v)) {
                          runs.push((s, l - s));
                      }
                  }
              }
              w &= w - 1;
          }
      }
      if let Some((s, l)) = cur {
          runs.push((s, l - s));
      }
      runs
  }

  // ── scalar bitset kernels (T2 adds the AVX2 dispatch) ────────────────

  fn popcount_scalar(w: &[u64; BITSET_WORDS]) -> usize {
      w.iter().map(|x| x.count_ones() as usize).sum()
  }

  /// Word popcount; T2 adds the AVX2 fast path as an equivalent dispatch.
  fn words_popcount(w: &[u64; BITSET_WORDS]) -> usize {
      popcount_scalar(w)
  }
  ```

- [ ] **Step 1.4: 跑测试确认通过（本轮三个测试）**

  ```
  $ cargo test -p codec-lucene9 roaring 2>&1 | tail -3
  test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 1.5: 写失败测试（and/or 全容器对 + 序列化 round-trip + 损坏校验）** — 追加到 `crates/codec-lucene9/src/roaring.rs` 的 `mod tests`（`and`/`or`/`serialize`/`deserialize` 尚不存在，编译失败即失败测试成立）：

  ```rust
      /// Merge-based reference for the expected doc set.
      fn ref_merge(a: &[u32], b: &[u32], intersect: bool) -> Vec<u32> {
          let mut out = Vec::new();
          let (mut i, mut j) = (0, 0);
          while i < a.len() && j < b.len() {
              match a[i].cmp(&b[j]) {
                  std::cmp::Ordering::Less => {
                      if !intersect {
                          out.push(a[i]);
                      }
                      i += 1;
                  }
                  std::cmp::Ordering::Greater => {
                      if !intersect {
                          out.push(b[j]);
                      }
                      j += 1;
                  }
                  std::cmp::Ordering::Equal => {
                      out.push(a[i]);
                      i += 1;
                      j += 1;
                  }
              }
          }
          if !intersect {
              out.extend_from_slice(&a[i..]);
              out.extend_from_slice(&b[j..]);
          }
          out
      }

      fn bm(docs: &[u32]) -> RoaringBitmap {
          RoaringBitmap::from_sorted_docs(docs)
      }

      #[test]
      fn and_matches_reference_across_container_kinds() {
          let run_a = range_docs(0, 5000); // Run(0..5000)
          let run_b = range_docs(2500, 7500); // Run(2500..7500)
          let arr_a: Vec<u32> = vec![1, 2500, 2501, 4999, 65_536, 65_540]; // Array x2 keys
          let bits_a = stride_docs(0, 2, 5000); // Bitset(evens 0..9998)
          let bits_b = stride_docs(1, 2, 5000); // Bitset(odds 1..9999), disjoint
          let cases: [(&[u32], &[u32]); 6] = [
              (&run_a, &run_b),                // Run x Run
              (&run_a, &arr_a),                // Run x Array
              (&run_a, &bits_a),               // Run x Bitset
              (&arr_a, &bits_a),               // Array x Bitset
              (&bits_a, &bits_b),              // Bitset x Bitset -> empty
              (&arr_a, &[1, 4999, 200_000]),   // Array x Array, cross-key
          ];
          for (a, b) in cases {
              let expect = ref_merge(a, b, true);
              let got = bm(a).and(&bm(b));
              assert_eq!(got.cardinality(), expect.len() as u64, "card {a:?} x {b:?}");
              assert_eq!(collect(&got), expect, "iter {a:?} x {b:?}");
          }
      }

      #[test]
      fn or_matches_reference_across_container_kinds() {
          let run_a = range_docs(0, 5000);
          let run_b = range_docs(2500, 7500); // overlapping -> merged Run(0..7500)
          let arr_a: Vec<u32> = vec![1, 2500, 2501, 65_536, 65_540];
          let bits_a = stride_docs(0, 2, 5000);
          let bits_b = stride_docs(1, 2, 5000);
          let cases: [(&[u32], &[u32]); 5] = [
              (&run_a, &run_b),   // Run u Run
              (&run_a, &arr_a),   // Run u Array
              (&arr_a, &bits_a),  // Array u Bitset
              (&bits_a, &bits_b), // Bitset u Bitset = 0..9999 contiguous
              (&arr_a, &[2, 200_000]),
          ];
          for (a, b) in cases {
              let expect = ref_merge(a, b, false);
              let got = bm(a).or(&bm(b));
              assert_eq!(got.cardinality(), expect.len() as u64, "card {a:?} u {b:?}");
              assert_eq!(collect(&got), expect, "iter {a:?} u {b:?}");
          }
          // evens u odds is one contiguous range -> runOptimize picks Run
          let u = bm(&bits_a).or(&bm(&bits_b));
          assert!(matches!(u.container_at(0), Container::Run(_)));
      }

      #[test]
      fn serialize_deserialize_round_trip() {
          let docs: Vec<u32> = [
              range_docs(0, 101),           // Run
              stride_docs(65_536, 2, 5000), // Bitset
              vec![200_000],                // Array
          ]
          .concat();
          let bitmap = bm(&docs);
          let mut out = IndexOutput::in_memory();
          bitmap.serialize(&mut out).unwrap();
          let bytes = out.into_bytes();
          let mut input = crate::io::IndexInput::in_memory(bytes);
          let back = RoaringBitmap::deserialize(&mut input).unwrap();
          assert_eq!(back.cardinality(), bitmap.cardinality());
          assert_eq!(collect(&back), docs);
      }

      #[test]
      fn deserialize_rejects_corruption() {
          // unknown type tag: layout [n=1][key=0][tag][card=3][vals...]
          let bitmap = bm(&[1, 2, 3]);
          let mut out = IndexOutput::in_memory();
          bitmap.serialize(&mut out).unwrap();
          let bytes = out.into_bytes();
          let mut bad = bytes.clone();
          bad[2] = 9; // the tag byte of container 0
          assert!(RoaringBitmap::deserialize(&mut crate::io::IndexInput::in_memory(bad)).is_err());

          // truncated payload
          let short = bytes[..bytes.len() - 1].to_vec();
          assert!(RoaringBitmap::deserialize(&mut crate::io::IndexInput::in_memory(short)).is_err());

          // descending container keys: two single-value containers at keys
          // 0 and 1; layout [n=2][k0=0][tag][card][val lo,hi][k1=1]...
          let bitmap = bm(&[1, 65_536]);
          let mut out = IndexOutput::in_memory();
          bitmap.serialize(&mut out).unwrap();
          let mut b2 = out.into_bytes();
          assert_eq!((b2[1], b2[6]), (0, 1), "single-byte VInt keys");
          b2.swap(1, 6); // keys become 1, 0 -> strictly ascending violated
          assert!(RoaringBitmap::deserialize(&mut crate::io::IndexInput::in_memory(b2)).is_err());

          // bitset popcount != cardinality
          let bitmap = bm(&stride_docs(0, 2, 5000));
          let mut out = IndexOutput::in_memory();
          bitmap.serialize(&mut out).unwrap();
          let mut b3 = out.into_bytes();
          let last = b3.len() - 1;
          b3[last] ^= 0x01; // flip one bit in the last word
          assert!(RoaringBitmap::deserialize(&mut crate::io::IndexInput::in_memory(b3)).is_err());
      }
  ```

- [ ] **Step 1.6: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 roaring 2>&1 | tail -5
  error[E0599]: no method named `and` found for struct `RoaringBitmap`
  ...（or / serialize / deserialize 同样未定义）
  ```

- [ ] **Step 1.7: 实现 and/or + 序列化** — `crates/codec-lucene9/src/roaring.rs` 在 `words_popcount` 之后、`#[cfg(test)]` 之前插入：

  ```rust
  impl RoaringBitmap {
      /// Container-pair intersection merged on ascending keys; the result
      /// stays a bitmap (spec §5: 不物化成数组), empty containers dropped.
      pub fn and(&self, other: &RoaringBitmap) -> RoaringBitmap {
          let mut out = Vec::new();
          let (mut i, mut j) = (0, 0);
          while i < self.containers.len() && j < other.containers.len() {
              match self.containers[i].0.cmp(&other.containers[j].0) {
                  std::cmp::Ordering::Less => i += 1,
                  std::cmp::Ordering::Greater => j += 1,
                  std::cmp::Ordering::Equal => {
                      if let Some(c) =
                          and_containers(&self.containers[i].1, &other.containers[j].1)
                      {
                          out.push((self.containers[i].0, c));
                      }
                      i += 1;
                      j += 1;
                  }
              }
          }
          RoaringBitmap { containers: out }
      }

      /// Container-pair union (dual of [`Self::and`]).
      pub fn or(&self, other: &RoaringBitmap) -> RoaringBitmap {
          let mut out = Vec::with_capacity(self.containers.len().max(other.containers.len()));
          let (mut i, mut j) = (0, 0);
          loop {
              match (i < self.containers.len(), j < other.containers.len()) {
                  (false, false) => break,
                  (true, false) => {
                      out.push(self.containers[i].clone());
                      i += 1;
                  }
                  (false, true) => {
                      out.push(other.containers[j].clone());
                      j += 1;
                  }
                  (true, true) => match self.containers[i].0.cmp(&other.containers[j].0) {
                      std::cmp::Ordering::Less => {
                          out.push(self.containers[i].clone());
                          i += 1;
                      }
                      std::cmp::Ordering::Greater => {
                          out.push(other.containers[j].clone());
                          j += 1;
                      }
                      std::cmp::Ordering::Equal => {
                          out.push((
                              self.containers[i].0,
                              or_containers(&self.containers[i].1, &other.containers[j].1),
                          ));
                          i += 1;
                          j += 1;
                      }
                  },
              }
          }
          RoaringBitmap { containers: out }
      }

      /// Payload layout (consumed by bitmap_sidecar.rs): VInt num_containers,
      /// then per container VInt key + Byte type tag (0=array, 1=bitset,
      /// 2=run) + VInt cardinality + body (array: card x Short; bitset:
      /// 1024 x Long; run: VInt num_runs + runs x (Short start, Short len)).
      /// All little-endian — the project convention for file bodies
      /// (codec_util.rs: BE 只用于 header/footer).
      pub(crate) fn serialize(&self, out: &mut impl DataOutput) -> io::Result<()> {
          out.write_vint(self.containers.len() as i32)?;
          for (key, c) in &self.containers {
              out.write_vint(*key as i32)?;
              match c {
                  Container::Array(vals) => {
                      out.write_byte(0)?;
                      out.write_vint(vals.len() as i32)?;
                      for &v in vals {
                          out.write_short(v as i16)?;
                      }
                  }
                  Container::Bitset(words) => {
                      out.write_byte(1)?;
                      out.write_vint(c.cardinality() as i32)?;
                      for &w in words.iter() {
                          out.write_long(w as i64)?;
                      }
                  }
                  Container::Run(runs) => {
                      out.write_byte(2)?;
                      out.write_vint(c.cardinality() as i32)?;
                      out.write_vint(runs.len() as i32)?;
                      for &(s, l) in runs {
                          out.write_short(s as i16)?;
                          out.write_short(l as i16)?;
                      }
                  }
              }
          }
          Ok(())
      }

      /// Validates the construction invariants while decoding (keys strictly
      /// ascending, array values ascending and < 4096, bitset popcount ==
      /// cardinality, runs disjoint/non-adjacent with card == sum) — any
      /// violation is a corrupt payload and the caller falls back to
      /// postings (§4a.6).
      pub(crate) fn deserialize(input: &mut impl DataInput) -> io::Result<RoaringBitmap> {
          let n = input.read_vint()?;
          if !(0..=65536).contains(&n) {
              return Err(corrupt(format!("roaring container count {n} out of range")));
          }
          let mut containers = Vec::with_capacity(n as usize);
          let mut prev_key: Option<u16> = None;
          for _ in 0..n {
              let key = input.read_vint()?;
              if !(0..=65535).contains(&key) {
                  return Err(corrupt(format!("roaring container key {key} out of range")));
              }
              let key = key as u16;
              if prev_key.map_or(false, |p| key <= p) {
                  return Err(corrupt("roaring container keys must strictly ascend"));
              }
              prev_key = Some(key);
              let tag = input.read_byte()?;
              let card = input.read_vint()?;
              if card <= 0 {
                  return Err(corrupt("roaring container cardinality must be positive"));
              }
              let card = card as usize;
              let c = match tag {
                  0 => {
                      if card >= ARRAY_CONTAINER_THRESHOLD {
                          return Err(corrupt("roaring array container cardinality >= 4096"));
                      }
                      let mut vals = Vec::with_capacity(card);
                      let mut prev: Option<u16> = None;
                      for _ in 0..card {
                          let v = input.read_short()? as u16;
                          if prev.map_or(false, |p| v <= p) {
                              return Err(corrupt("roaring array values must strictly ascend"));
                          }
                          prev = Some(v);
                          vals.push(v);
                      }
                      Container::Array(vals)
                  }
                  1 => {
                      if card < ARRAY_CONTAINER_THRESHOLD {
                          return Err(corrupt("roaring bitset container cardinality < 4096"));
                      }
                      let mut words = Box::new([0u64; BITSET_WORDS]);
                      for w in words.iter_mut() {
                          *w = input.read_long()? as u64;
                      }
                      if popcount_scalar(&words) != card {
                          return Err(corrupt("roaring bitset popcount != cardinality"));
                      }
                      Container::Bitset(words)
                  }
                  2 => {
                      let nruns = input.read_vint()?;
                      if nruns <= 0 {
                          return Err(corrupt("roaring run container must have runs"));
                      }
                      let mut runs = Vec::with_capacity(nruns as usize);
                      let mut total = 0usize;
                      let mut prev_end: Option<u32> = None;
                      for _ in 0..nruns {
                          let s = input.read_short()? as u16;
                          let l = input.read_short()? as u16;
                          let e = s as u32 + l as u32;
                          if prev_end.map_or(false, |p| s as u32 <= p + 1) {
                              return Err(corrupt(
                                  "roaring runs must be disjoint and non-adjacent",
                              ));
                          }
                          prev_end = Some(e);
                          total += l as usize + 1;
                          runs.push((s, l));
                      }
                      if total != card {
                          return Err(corrupt("roaring run lengths sum != cardinality"));
                      }
                      Container::Run(runs)
                  }
                  t => return Err(corrupt(format!("unknown roaring container tag {t}"))),
              };
              containers.push((key, c));
          }
          Ok(RoaringBitmap { containers })
      }
  }

  // ── container pair ops (spec §5 容器对分发) ──────────────────────────

  /// array∩array galloping, bitset∩bitset word ops + popcount, run∩run
  /// two-pointer; mixed pairs per the Roaring paper.
  fn and_containers(a: &Container, b: &Container) -> Option<Container> {
      use Container::*;
      match (a, b) {
          (Array(x), Array(y)) => container_from_small_sorted(and_array_array(x, y)),
          (Array(x), Bitset(y)) | (Bitset(y), Array(x)) => {
              container_from_small_sorted(and_array_bitset(x, y))
          }
          (Array(x), Run(r)) | (Run(r), Array(x)) => {
              container_from_small_sorted(and_array_run(x, r))
          }
          (Bitset(x), Bitset(y)) => and_bitset_bitset(x, y),
          (Bitset(x), Run(r)) | (Run(r), Bitset(x)) => and_bitset_run(x, r),
          (Run(x), Run(y)) => and_run_run(x, y),
      }
  }

  fn or_containers(a: &Container, b: &Container) -> Container {
      use Container::*;
      match (a, b) {
          (Array(x), Array(y)) => or_array_array(x, y),
          (Array(x), Bitset(y)) | (Bitset(y), Array(x)) => or_array_bitset(x, y),
          (Array(x), Run(r)) | (Run(r), Array(x)) => {
              or_array_run(x, r).expect("union of non-empty containers")
          }
          (Bitset(x), Bitset(y)) => or_bitset_bitset(x, y),
          (Bitset(x), Run(r)) | (Run(r), Bitset(x)) => or_bitset_run(x, r),
          (Run(x), Run(y)) => or_run_run(x, y).expect("union of non-empty containers"),
      }
  }

  /// Result normalization for intersections that produce sorted values with
  /// card < 4096 by construction (array-involving pairs).
  fn container_from_small_sorted(vals: Vec<u16>) -> Option<Container> {
      if vals.is_empty() {
          return None;
      }
      debug_assert!(vals.len() < ARRAY_CONTAINER_THRESHOLD);
      let runs = runs_from_sorted_values(&vals);
      if run_cost(&runs) < 2 * vals.len() {
          return Some(Container::Run(runs));
      }
      Some(Container::Array(vals))
  }

  /// Normalization for run-producing ops: array when smaller, run when
  /// cheaper than a bitset, bitset otherwise.
  fn container_from_runs(runs: Vec<(u16, u16)>) -> Option<Container> {
      if runs.is_empty() {
          return None;
      }
      let card: usize = runs.iter().map(|&(_, l)| l as usize + 1).sum();
      if card < ARRAY_CONTAINER_THRESHOLD && 2 * card <= run_cost(&runs) {
          return Some(Container::Array(expand_runs(&runs)));
      }
      if run_cost(&runs) < BITSET_BYTES {
          return Some(Container::Run(runs));
      }
      container_from_bitset(Box::new(words_from_runs(&runs)), card)
  }

  fn expand_runs(runs: &[(u16, u16)]) -> Vec<u16> {
      let mut out = Vec::with_capacity(runs.iter().map(|&(_, l)| l as usize + 1).sum());
      for &(s, l) in runs {
          for v in s as u32..=s as u32 + l as u32 {
              out.push(v as u16);
          }
      }
      out
  }

  fn words_from_runs(runs: &[(u16, u16)]) -> [u64; BITSET_WORDS] {
      let mut words = [0u64; BITSET_WORDS];
      for &(s, l) in runs {
          let (first, last) = (s as usize, s as usize + l as usize);
          for wi in first / 64..=last / 64 {
              words[wi] |= word_range_mask(wi, first, last);
          }
      }
      words
  }

  /// Bits of word `wi` inside the inclusive value range [first, last].
  fn word_range_mask(wi: usize, first: usize, last: usize) -> u64 {
      let lo = if wi == first / 64 { first % 64 } else { 0 };
      let hi = if wi == last / 64 { last % 64 } else { 63 };
      (u64::MAX << lo) & if hi == 63 { u64::MAX } else { (1u64 << (hi + 1)) - 1 }
  }

  /// Galloping intersection (spec §5): exponential probe + binary refine,
  /// iterating the smaller side.
  fn and_array_array(a: &[u16], b: &[u16]) -> Vec<u16> {
      let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
      let mut out = Vec::new();
      let mut base = 0usize;
      for &v in small {
          if base >= large.len() {
              break;
          }
          base = galloping_search(large, base, v);
          if base < large.len() && large[base] == v {
              out.push(v);
              base += 1;
          }
      }
      out
  }

  /// First index >= `from` with `hay[idx] >= needle` (== hay.len() when none).
  fn galloping_search(hay: &[u16], from: usize, needle: u16) -> usize {
      if hay[from] >= needle {
          return from;
      }
      // gallop: invariant hay[prev] < needle; find the window holding needle
      let mut step = 1usize;
      let mut prev = from;
      while prev + step < hay.len() && hay[prev + step] < needle {
          prev += step;
          step <<= 1;
      }
      // binary refine inside (prev, min(prev + step + 1, len))
      let mut lo = prev + 1;
      let mut hi = (prev + step + 1).min(hay.len());
      while lo < hi {
          let mid = lo + (hi - lo) / 2;
          if hay[mid] < needle {
              lo = mid + 1;
          } else {
              hi = mid;
          }
      }
      lo
  }

  fn and_array_bitset(a: &[u16], words: &[u64; BITSET_WORDS]) -> Vec<u16> {
      a.iter()
          .copied()
          .filter(|&v| words[(v >> 6) as usize] & (1u64 << (v & 63)) != 0)
          .collect()
  }

  /// Merge-join of sorted points against disjoint inclusive ranges.
  fn and_array_run(a: &[u16], runs: &[(u16, u16)]) -> Vec<u16> {
      let mut out = Vec::new();
      let mut ri = 0usize;
      for &v in a {
          while ri < runs.len() && runs[ri].0 as u32 + runs[ri].1 as u32 < v as u32 {
              ri += 1;
          }
          if ri == runs.len() {
              break;
          }
          if runs[ri].0 <= v {
              out.push(v);
          }
      }
      out
  }

  fn and_bitset_bitset(a: &[u64; BITSET_WORDS], b: &[u64; BITSET_WORDS]) -> Option<Container> {
      let mut out = Box::new([0u64; BITSET_WORDS]);
      let card = bitset_and(a, b, &mut *out);
      container_from_bitset(out, card)
  }

  /// AND of words with inclusive ranges: keep only the in-range bits.
  fn and_bitset_run(words: &[u64; BITSET_WORDS], runs: &[(u16, u16)]) -> Option<Container> {
      let mut out = Box::new([0u64; BITSET_WORDS]);
      let mut card = 0usize;
      for &(s, l) in runs {
          let (first, last) = (s as usize, s as usize + l as usize);
          for wi in first / 64..=last / 64 {
              let v = words[wi] & word_range_mask(wi, first, last);
              out[wi] |= v;
              card += v.count_ones() as usize;
          }
      }
      container_from_bitset(out, card)
  }

  /// Interval intersection two-pointer (spec §5 run∩run 双指针).
  fn and_run_run(a: &[(u16, u16)], b: &[(u16, u16)]) -> Option<Container> {
      let mut out: Vec<(u16, u16)> = Vec::new();
      let (mut i, mut j) = (0, 0);
      while i < a.len() && j < b.len() {
          let (as_, ae) = (a[i].0 as u32, a[i].0 as u32 + a[i].1 as u32);
          let (bs, be) = (b[j].0 as u32, b[j].0 as u32 + b[j].1 as u32);
          let s = as_.max(bs);
          let e = ae.min(be);
          if s <= e {
              out.push((s as u16, (e - s) as u16));
          }
          if ae < be {
              i += 1;
          } else {
              j += 1;
          }
      }
      container_from_runs(out)
  }

  fn or_array_array(a: &[u16], b: &[u16]) -> Container {
      let mut out: Vec<u16> = Vec::with_capacity(a.len() + b.len());
      let (mut i, mut j) = (0, 0);
      while i < a.len() && j < b.len() {
          if a[i] < b[j] {
              out.push(a[i]);
              i += 1;
          } else if a[i] > b[j] {
              out.push(b[j]);
              j += 1;
          } else {
              out.push(a[i]);
              i += 1;
              j += 1;
          }
      }
      out.extend_from_slice(&a[i..]);
      out.extend_from_slice(&b[j..]);
      container_from_sorted_values(out)
  }

  fn or_array_bitset(a: &[u16], words: &[u64; BITSET_WORDS]) -> Container {
      let mut out = Box::new(*words);
      for &v in a {
          out[(v >> 6) as usize] |= 1u64 << (v & 63);
      }
      let card = words_popcount(&out);
      container_from_bitset(out, card).expect("union of non-empty containers")
  }

  /// Union of sorted points and disjoint ranges, keeping the run shape
  /// (intervals are never expanded to points).
  fn or_array_run(a: &[u16], runs: &[(u16, u16)]) -> Option<Container> {
      let mut out: Vec<(u16, u16)> = Vec::new();
      let mut cur: Option<(u32, u32)> = None; // open union interval [start, end]
      let mut ai = 0usize;
      for &(rs, rl) in runs {
          let (rs, re) = (rs as u32, rs as u32 + rl as u32);
          while ai < a.len() && (a[ai] as u32) < rs {
              absorb(&mut cur, &mut out, a[ai] as u32);
              ai += 1;
          }
          absorb(&mut cur, &mut out, rs);
          if let Some((_, e)) = &mut cur {
              *e = (*e).max(re);
          }
          let e = cur.unwrap().1;
          while ai < a.len() && (a[ai] as u32) <= e + 1 {
              absorb(&mut cur, &mut out, a[ai] as u32);
              ai += 1;
          }
      }
      while ai < a.len() {
          absorb(&mut cur, &mut out, a[ai] as u32);
          ai += 1;
      }
      flush_interval(&mut cur, &mut out);
      container_from_runs(out)
  }

  /// Adds one value to the open union interval: extend when adjacent or
  /// inside, flush and reopen otherwise.
  fn absorb(cur: &mut Option<(u32, u32)>, out: &mut Vec<(u16, u16)>, v: u32) {
      match cur {
          Some((_, e)) if v <= *e + 1 => *e = (*e).max(v),
          _ => {
              flush_interval(cur, out);
              *cur = Some((v, v));
          }
      }
  }

  fn flush_interval(cur: &mut Option<(u32, u32)>, out: &mut Vec<(u16, u16)>) {
      if let Some((s, e)) = cur.take() {
          out.push((s as u16, (e - s) as u16));
      }
  }

  fn or_bitset_bitset(a: &[u64; BITSET_WORDS], b: &[u64; BITSET_WORDS]) -> Container {
      let mut out = Box::new([0u64; BITSET_WORDS]);
      let card = bitset_or(a, b, &mut *out);
      container_from_bitset(out, card).expect("union of non-empty containers")
  }

  fn or_bitset_run(words: &[u64; BITSET_WORDS], runs: &[(u16, u16)]) -> Container {
      let mut out = Box::new(*words);
      for &(s, l) in runs {
          let (first, last) = (s as usize, s as usize + l as usize);
          for wi in first / 64..=last / 64 {
              out[wi] |= word_range_mask(wi, first, last);
          }
      }
      let card = words_popcount(&out);
      container_from_bitset(out, card).expect("union of non-empty containers")
  }

  fn or_run_run(a: &[(u16, u16)], b: &[(u16, u16)]) -> Option<Container> {
      let mut out: Vec<(u16, u16)> = Vec::new();
      let mut cur: Option<(u32, u32)> = None;
      let (mut i, mut j) = (0, 0);
      while i < a.len() || j < b.len() {
          let next = if j == b.len() || (i < a.len() && a[i] <= b[j]) {
              let r = a[i];
              i += 1;
              r
          } else {
              let r = b[j];
              j += 1;
              r
          };
          absorb(&mut cur, &mut out, next.0 as u32);
          if let Some((_, e)) = &mut cur {
              *e = (*e).max(next.0 as u32 + next.1 as u32);
          }
      }
      flush_interval(&mut cur, &mut out);
      container_from_runs(out)
  }

  // ── scalar bitset kernels + dispatch stubs (T2 hooks the AVX2 path) ──

  fn bitset_and_scalar(
      a: &[u64; BITSET_WORDS],
      b: &[u64; BITSET_WORDS],
      out: &mut [u64; BITSET_WORDS],
  ) -> usize {
      let mut card = 0usize;
      for i in 0..BITSET_WORDS {
          let v = a[i] & b[i];
          out[i] = v;
          card += v.count_ones() as usize;
      }
      card
  }

  fn bitset_or_scalar(
      a: &[u64; BITSET_WORDS],
      b: &[u64; BITSET_WORDS],
      out: &mut [u64; BITSET_WORDS],
  ) -> usize {
      let mut card = 0usize;
      for i in 0..BITSET_WORDS {
          let v = a[i] | b[i];
          out[i] = v;
          card += v.count_ones() as usize;
      }
      card
  }

  /// Word-AND + popcount; T2 adds the AVX2 fast path as an equivalent dispatch.
  fn bitset_and(a: &[u64; BITSET_WORDS], b: &[u64; BITSET_WORDS], out: &mut [u64; BITSET_WORDS]) -> usize {
      bitset_and_scalar(a, b, out)
  }

  /// Word-OR + popcount; T2 adds the AVX2 fast path as an equivalent dispatch.
  fn bitset_or(a: &[u64; BITSET_WORDS], b: &[u64; BITSET_WORDS], out: &mut [u64; BITSET_WORDS]) -> usize {
      bitset_or_scalar(a, b, out)
  }
  ```

- [ ] **Step 1.8: 跑测试确认通过（全部 7 个 roaring 测试）**

  ```
  $ cargo test -p codec-lucene9 roaring 2>&1 | tail -3
  test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 1.9: 提交**

  ```
  git add crates/codec-lucene9/src/roaring.rs crates/codec-lucene9/src/lib.rs
  git commit -m "feat: roaring three-container bitmap library (scalar reference, M3)"
  ```

---

## Task 2: AVX2 快路径 + 标量对拍（`crates/codec-lucene9/src/roaring/simd.rs` 新建）

spec §6：AVX2 作等价追加——运行时分发 + 标量 vs SIMD 逐位对拍。模式照搬 `postings_ll/simd.rs`（模块级 allow、OnceLock 缓存、`RL_SIMD=0` kill switch）。

**Files:**
- Create: `crates/codec-lucene9/src/roaring/simd.rs`
- Modify: `crates/codec-lucene9/src/roaring.rs`（`mod simd;` 声明 + 三个 dispatch 函数换体）
- Modify: `crates/codec-lucene9/src/lib.rs`（unsafe 边界注释更新为两个 SIMD 模块）
- Test: `crates/codec-lucene9/src/roaring/simd.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T1 的 `bitset_and_scalar` / `bitset_or_scalar` / `popcount_scalar` / `BITSET_WORDS`（子模块可见父模块私有项）。
- Produces（仅 `roaring.rs` 的 dispatch 使用，x86_64-only）:
  ```rust
  // roaring/simd.rs
  pub(super) fn try_and_words(a: &[u64; BITSET_WORDS], b: &[u64; BITSET_WORDS], out: &mut [u64; BITSET_WORDS]) -> Option<usize>;
  pub(super) fn try_or_words(a: &[u64; BITSET_WORDS], b: &[u64; BITSET_WORDS], out: &mut [u64; BITSET_WORDS]) -> Option<usize>;
  pub(super) fn try_popcount(w: &[u64; BITSET_WORDS]) -> Option<usize>;
  ```

### Steps

- [ ] **Step 2.1: 写失败测试（标量 vs SIMD 逐位对拍）** — 新建 `crates/codec-lucene9/src/roaring/simd.rs`，先放模块文档、`#![allow(unsafe_code)]` 与测试（kernel 尚不存在，编译失败即失败测试成立）：

  ```rust
  //! AVX2 fast path for the roaring bitset-container word ops (spec §6
  //! "先标量参考实现，AVX2 快路径作等价追加"): `bitset_and` / `bitset_or` /
  //! `words_popcount` in [`super`] dispatch here first.
  //!
  //! 256-bit vectors process 4 u64 words per instruction; the 1024-word
  //! container is an exact multiple of 4, so there is no tail. Popcount
  //! uses the nibble-LUT + PSADBW idiom (Muła) — AVX2 has no VPOPCNT
  //! (that's AVX-512).
  //!
  //! ## Safety argument (module-level `allow(unsafe_code)`)
  //!
  //! The crate is `#![deny(unsafe_code)]`; this module and
  //! postings_ll/simd.rs are the only exceptions, and every unsafe
  //! operation is an AVX2 intrinsic inside a `#[target_feature(enable =
  //! "avx2")]` function. Those functions are only reached through the
  //! `try_*` shims, which gate on a cached `is_x86_feature_detected!
  //! ("avx2")`, so no AVX2 instruction can execute on a CPU without
  //! support. All memory access uses unaligned intrinsics (`loadu`/`storeu`)
  //! on caller-owned `[u64; 1024]` arrays with `i + 4 <= 1024` by
  //! construction, so every access is in bounds. Bit-for-bit equivalence
  //! with the scalar reference is pinned by the differential tests in this
  //! file over adversarial patterns (zeros/ones/alternating/xorshift/
  //! sparse), and end-to-end by the bitmap on/off searchdump diff in the
  //! log battery (interop/verify-log.sh --bitmap).

  #![allow(unsafe_code)]

  #[cfg(test)]
  mod tests {
      use super::super::{bitset_and_scalar, bitset_or_scalar, popcount_scalar, BITSET_WORDS};
      use super::{and_words_avx2, or_words_avx2, popcount_avx2};

      fn xorshift_words(seed: u64) -> Box<[u64; BITSET_WORDS]> {
          let mut s = if seed == 0 { 0x9E3779B97F4A7C15 } else { seed };
          let mut out = Box::new([0u64; BITSET_WORDS]);
          for w in out.iter_mut() {
              s ^= s >> 12;
              s ^= s << 25;
              s ^= s >> 27;
              *w = s.wrapping_mul(0x2545F4914F6CDD1D);
          }
          out
      }

      fn patterns() -> Vec<Box<[u64; BITSET_WORDS]>> {
          let mut v: Vec<Box<[u64; BITSET_WORDS]>> = vec![
              Box::new([0u64; BITSET_WORDS]),
              Box::new([u64::MAX; BITSET_WORDS]),
              Box::new([0xAAAA_AAAA_AAAA_AAAA; BITSET_WORDS]),
              xorshift_words(42),
              xorshift_words(0xDEAD_BEEF),
          ];
          let mut sparse = Box::new([0u64; BITSET_WORDS]);
          sparse[BITSET_WORDS - 1] = 1 << 63;
          v.push(sparse);
          v
      }

      /// spec §6: scalar vs SIMD bitwise equality on adversarial patterns.
      #[test]
      fn avx2_matches_scalar_bitwise() {
          if !std::arch::is_x86_feature_detected!("avx2") {
              return; // scalar-only host: nothing to differential-test
          }
          let pats = patterns();
          for a in &pats {
              assert_eq!(popcount_scalar(a), unsafe { popcount_avx2(a) }, "popcount");
              for b in &pats {
                  let mut scalar_out = Box::new([0u64; BITSET_WORDS]);
                  let mut simd_out = Box::new([0u64; BITSET_WORDS]);
                  let c_scalar = bitset_and_scalar(a, b, &mut scalar_out);
                  // SAFETY: guarded by the runtime feature check above.
                  let c_simd = unsafe { and_words_avx2(a, b, &mut simd_out) };
                  assert_eq!(c_scalar, c_simd, "and cardinality");
                  assert_eq!(scalar_out, simd_out, "and words");

                  let mut scalar_out = Box::new([0u64; BITSET_WORDS]);
                  let mut simd_out = Box::new([0u64; BITSET_WORDS]);
                  let c_scalar = bitset_or_scalar(a, b, &mut scalar_out);
                  let c_simd = unsafe { or_words_avx2(a, b, &mut simd_out) };
                  assert_eq!(c_scalar, c_simd, "or cardinality");
                  assert_eq!(scalar_out, simd_out, "or words");
              }
          }
      }
  }
  ```

  同时 `crates/codec-lucene9/src/roaring.rs` 在 `use crate::io::{DataInput, DataOutput};` 之后插入：

  ```rust
  /// AVX2 fast path for the bitset word ops (spec §6) — see the module
  /// docs for the equivalence and safety arguments. x86_64-only; every
  /// other target takes the scalar reference path.
  #[cfg(target_arch = "x86_64")]
  mod simd;
  ```

- [ ] **Step 2.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 roaring 2>&1 | tail -5
  error[E0432]: unresolved imports `super::and_words_avx2`, `super::or_words_avx2`, `super::popcount_avx2`
  ```

- [ ] **Step 2.3: 实现 kernel + dispatch** — `crates/codec-lucene9/src/roaring/simd.rs` 在 `#![allow(unsafe_code)]` 之后、`#[cfg(test)]` 之前插入：

  ```rust
  use super::BITSET_WORDS;
  use std::sync::OnceLock;

  /// Cached one-time AVX2 detection (same discipline as
  /// postings_ll/simd.rs:52-62): `RL_SIMD=0` in the environment forces the
  /// scalar path (kill switch for the fast path; also enables same-binary
  /// A/B benchmarking).
  #[inline]
  fn avx2_available() -> bool {
      static DETECTED: OnceLock<bool> = OnceLock::new();
      *DETECTED.get_or_init(|| {
          std::env::var_os("RL_SIMD").map_or(true, |v| v != "0")
              && std::is_x86_feature_detected!("avx2")
      })
  }

  /// Dispatch shim for [`super::bitset_and`]: `Some(popcount)` via the AVX2
  /// kernel, `None` when the CPU lacks AVX2 (caller runs the scalar path).
  pub(super) fn try_and_words(
      a: &[u64; BITSET_WORDS],
      b: &[u64; BITSET_WORDS],
      out: &mut [u64; BITSET_WORDS],
  ) -> Option<usize> {
      if !avx2_available() {
          return None;
      }
      // SAFETY: `avx2_available()` just returned true, so this CPU may
      // execute AVX2 instructions.
      Some(unsafe { and_words_avx2(a, b, out) })
  }

  /// Dispatch shim for [`super::bitset_or`].
  pub(super) fn try_or_words(
      a: &[u64; BITSET_WORDS],
      b: &[u64; BITSET_WORDS],
      out: &mut [u64; BITSET_WORDS],
  ) -> Option<usize> {
      if !avx2_available() {
          return None;
      }
      // SAFETY: see try_and_words.
      Some(unsafe { or_words_avx2(a, b, out) })
  }

  /// Dispatch shim for [`super::words_popcount`].
  pub(super) fn try_popcount(w: &[u64; BITSET_WORDS]) -> Option<usize> {
      if !avx2_available() {
          return None;
      }
      // SAFETY: see try_and_words.
      Some(unsafe { popcount_avx2(w) })
  }

  /// Per-byte popcount of a 256-bit vector via the nibble LUT (Muła): low
  /// and high nibbles index a 16-entry popcount table; the byte sums are
  /// horizontally added by PSADBW at the call site.
  #[target_feature(enable = "avx2")]
  fn byte_popcounts(v: std::arch::x86_64::__m256i) -> std::arch::x86_64::__m256i {
      use std::arch::x86_64::*;
      // SAFETY: intrinsics inside a #[target_feature(enable = "avx2")] fn
      // only reached from sibling kernels after the runtime feature check.
      unsafe {
          let lookup = _mm256_setr_epi8(
              0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4,
              0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4,
          );
          let low_mask = _mm256_set1_epi8(0x0f);
          let lo = _mm256_and_si256(v, low_mask);
          let hi = _mm256_and_si256(_mm256_srli_epi16(v, 4), low_mask);
          _mm256_add_epi8(
              _mm256_shuffle_epi8(lookup, lo),
              _mm256_shuffle_epi8(lookup, hi),
          )
      }
  }

  /// AVX2 kernel for [`super::bitset_and`]; see module docs.
  #[target_feature(enable = "avx2")]
  fn and_words_avx2(
      a: &[u64; BITSET_WORDS],
      b: &[u64; BITSET_WORDS],
      out: &mut [u64; BITSET_WORDS],
  ) -> usize {
      use std::arch::x86_64::*;
      // SAFETY: i + 4 <= BITSET_WORDS by the loop condition; loadu/storeu
      // on caller-owned fixed-size arrays, every access in bounds (module
      // docs). Each sad lane is <= 64 and there are 256 iterations, so the
      // u64 accumulator lanes stay <= 16384 (no overflow).
      unsafe {
          let zero = _mm256_setzero_si256();
          let mut total = _mm256_setzero_si256();
          let mut i = 0usize;
          while i < BITSET_WORDS {
              let va = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
              let vb = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
              let v = _mm256_and_si256(va, vb);
              _mm256_storeu_si256(out.as_mut_ptr().add(i) as *mut __m256i, v);
              total = _mm256_add_epi64(total, _mm256_sad_epu8(byte_popcounts(v), zero));
              i += 4;
          }
          let mut lanes = [0u64; 4];
          _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, total);
          lanes.iter().sum()
      }
  }

  /// AVX2 kernel for [`super::bitset_or`]; see module docs.
  #[target_feature(enable = "avx2")]
  fn or_words_avx2(
      a: &[u64; BITSET_WORDS],
      b: &[u64; BITSET_WORDS],
      out: &mut [u64; BITSET_WORDS],
  ) -> usize {
      use std::arch::x86_64::*;
      // SAFETY: same bounds argument as and_words_avx2 (module docs).
      unsafe {
          let zero = _mm256_setzero_si256();
          let mut total = _mm256_setzero_si256();
          let mut i = 0usize;
          while i < BITSET_WORDS {
              let va = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
              let vb = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
              let v = _mm256_or_si256(va, vb);
              _mm256_storeu_si256(out.as_mut_ptr().add(i) as *mut __m256i, v);
              total = _mm256_add_epi64(total, _mm256_sad_epu8(byte_popcounts(v), zero));
              i += 4;
          }
          let mut lanes = [0u64; 4];
          _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, total);
          lanes.iter().sum()
      }
  }

  /// AVX2 kernel for [`super::words_popcount`]; see module docs.
  #[target_feature(enable = "avx2")]
  fn popcount_avx2(w: &[u64; BITSET_WORDS]) -> usize {
      use std::arch::x86_64::*;
      // SAFETY: same bounds argument as and_words_avx2 (module docs).
      unsafe {
          let zero = _mm256_setzero_si256();
          let mut total = _mm256_setzero_si256();
          let mut i = 0usize;
          while i < BITSET_WORDS {
              let v = _mm256_loadu_si256(w.as_ptr().add(i) as *const __m256i);
              total = _mm256_add_epi64(total, _mm256_sad_epu8(byte_popcounts(v), zero));
              i += 4;
          }
          let mut lanes = [0u64; 4];
          _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, total);
          lanes.iter().sum()
      }
  }
  ```

  然后 `crates/codec-lucene9/src/roaring.rs` 的三个 dispatch 函数换体（旧体分别为 `popcount_scalar(w)` / `bitset_and_scalar(a, b, out)` / `bitset_or_scalar(a, b, out)` 单一调用）：

  ```rust
  /// Word popcount; AVX2 fast path first, scalar reference otherwise (spec §6).
  fn words_popcount(w: &[u64; BITSET_WORDS]) -> usize {
      #[cfg(target_arch = "x86_64")]
      if let Some(c) = simd::try_popcount(w) {
          return c;
      }
      popcount_scalar(w)
  }

  /// Word-AND + popcount; AVX2 fast path first, scalar otherwise (spec §6).
  fn bitset_and(a: &[u64; BITSET_WORDS], b: &[u64; BITSET_WORDS], out: &mut [u64; BITSET_WORDS]) -> usize {
      #[cfg(target_arch = "x86_64")]
      if let Some(c) = simd::try_and_words(a, b, out) {
          return c;
      }
      bitset_and_scalar(a, b, out)
  }

  /// Word-OR + popcount; AVX2 fast path first, scalar otherwise (spec §6).
  fn bitset_or(a: &[u64; BITSET_WORDS], b: &[u64; BITSET_WORDS], out: &mut [u64; BITSET_WORDS]) -> usize {
      #[cfg(target_arch = "x86_64")]
      if let Some(c) = simd::try_or_words(a, b, out) {
          return c;
      }
      bitset_or_scalar(a, b, out)
  }
  ```

  同时 `crates/codec-lucene9/src/lib.rs` 顶部 unsafe 边界注释由"the single AVX2 kernel module"改为：

  ```rust
  // `deny` rather than `forbid` so the AVX2 kernel modules can opt back
  // in with a module-level `allow` — see postings_ll/simd.rs and
  // roaring/simd.rs for the safety argument and the exact boundary of the
  // unsafe code.
  ```

- [ ] **Step 2.4: 跑测试确认通过（对拍 + T1 全部回归）**

  ```
  $ cargo test -p codec-lucene9 roaring 2>&1 | tail -3
  test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ RL_SIMD=0 cargo test -p codec-lucene9 roaring 2>&1 | tail -3
  test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

  （8 = T1 的 7 个 + 本任务 1 个对拍；`RL_SIMD=0` 强制标量路径也应全绿——kill switch 生效。）

- [ ] **Step 2.5: 提交**

  ```
  git add crates/codec-lucene9/src/roaring/simd.rs crates/codec-lucene9/src/roaring.rs crates/codec-lucene9/src/lib.rs
  git commit -m "feat: AVX2 fast paths for roaring bitset word ops + scalar differential tests"
  ```

---

## Task 3: 写侧 sidecar 产出（`bitmap_sidecar.rs` + flush 钩子 + 命名/GC/校验 §4a）

codec 拥有 sidecar 文件 FORMAT（写 + 读 + 校验 + GC）；core 只在 flush 的 term 循环里喂 docs 切片。round-trip 测试在 codec 层闭环。

**Files:**
- Create: `crates/codec-lucene9/src/bitmap_sidecar.rs`
- Modify: `crates/codec-lucene9/src/lib.rs`（`pub mod bitmap_sidecar;`）
- Modify: `crates/core/src/index_writer.rs`（`IndexWriterConfig` 两个字段 + create/flush 的 GC 调用 + `#[cfg(test)]` 模块）
- Modify: `crates/core/src/segment_builder.rs`（`with_bitmap` + finalize 钩子 + sidecar fsync）
- Test: `crates/codec-lucene9/src/bitmap_sidecar.rs` 与 `crates/core/src/index_writer.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T1 的 `RoaringBitmap::{from_sorted_docs, cardinality, serialize, deserialize}`；`crate::codec_util::{write_index_header, write_footer, check_index_header, check_footer_structure, corrupt}`；`FSDirectory::{create_output, open_input, open_checksum_input, file_exists, list_all, delete, sync}`。
- Produces（T4 的 SegmentReader、T6 的 CLI/电池依赖这些名字，不得改名）:
  ```rust
  // bitmap_sidecar.rs
  pub fn file_name(segment: &str) -> String; // rbm<seg>.bin（§4a.3）

  pub struct SidecarBuilder { .. }
  impl SidecarBuilder {
      pub fn new(segment: &str, max_doc: i32) -> Self;
      pub fn add_term(&mut self, field: &str, term: &[u8], docs: &[u32]);
      pub fn is_empty(&self) -> bool;
      /// 写出 rbm<seg>.bin，返回文件名（不写入任何 .si files 集合，§4a.2）
      pub fn write(self, dir: &FSDirectory, segment_id: &[u8; 16]) -> io::Result<String>;
  }

  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct SidecarTerm {
      pub doc_freq: u32,
      pub cardinality: u64,
      // payload_off/payload_len 为 pub(crate)，仅 load_bitmap 使用
  }

  pub struct SidecarReader { .. }
  impl SidecarReader {
      /// 文件不存在或绑定/校验失败 → Ok(None)（§4a.6 静默落档）
      pub fn open(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16], max_doc: i32) -> io::Result<Option<SidecarReader>>;
      pub fn term_entry(&self, field: &str, term: &[u8]) -> Option<SidecarTerm>;
      pub fn load_bitmap(&self, entry: &SidecarTerm) -> io::Result<RoaringBitmap>;
  }

  /// §4a.4：删除 rbm 前缀且 segment 不在 live 集合的文件，返回删除数
  pub fn gc_sidecars(dir: &FSDirectory, live_segments: &[String]) -> io::Result<usize>;

  // core index_writer.rs
  pub struct IndexWriterConfig { .., pub bitmap: bool, pub bitmap_threshold: u32 } // default false / 4096
  // core segment_builder.rs
  impl SegmentBuilder {
      pub fn with_bitmap(self, enabled: bool, threshold: u32) -> Self;
  }
  ```

### Steps

- [ ] **Step 3.1: 写 codec 失败测试** — 新建 `crates/codec-lucene9/src/bitmap_sidecar.rs`，先只放模块文档与测试（`SidecarBuilder`/`SidecarReader`/`gc_sidecars` 尚不存在，编译失败即失败测试成立）：

  ```rust
  //! Roaring-bitmap sidecar file (`rbm<seg>.bin`) — M3 spec §4 layout and
  //! the §4a compatibility contract:
  //!
  //! - independent directory-level file, never referenced by segments_N/.si
  //!   (§4a.1/2: codec footers reject trailing bytes, CodecUtil.java:572-580;
  //!   the .si files set drives CFS packing and refcounts,
  //!   IndexWriter.java:5876-5900);
  //! - named outside Lucene's managed namespace (§4a.3:
  //!   IndexFileNames.CODEC_FILE_PATTERN requires a leading '_',
  //!   IndexFileNames.java:199; IndexFileDeleter.java:146-151 only tracks
  //!   matching names and segments*/pending_segments* — write.lock survives
  //!   the same way);
  //! - header binds {magic, version, segment name, maxDoc}, term rows bind
  //!   df/cardinality; any mismatch degrades to "no bitmap" (§4a.6) — a bad
  //!   sidecar must never affect queries;
  //! - Rust-side GC deletes orphaned sidecars (§4a.4): Java merge/delete
  //!   leaves them behind and Java never cleans them up.
  //!
  //! Layout (spec §4; CodecUtil conventions per codec_util.rs):
  //!   index header (codec "RustRbmSidecar", version 0, segment id, "")
  //!   VInt-len string segment_name
  //!   VInt max_doc
  //!   VInt num_fields, then per field:
  //!     VInt-len string field_name
  //!     VInt num_terms, then per term (bytes strictly ascending):
  //!       VInt term_len + term bytes
  //!       VInt doc_freq
  //!       VLong cardinality
  //!       VLong payload_off  (relative to the payload region start)
  //!       VInt payload_len
  //!   payload region (concatenated roaring payloads, roaring.rs serialize)
  //!   footer (CRC32 over the whole file)

  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::codec_util::check_footer;
      use crate::io::DataInput;
      use std::fs;

      fn temp_dir(tag: &str) -> std::path::PathBuf {
          let dir = std::env::temp_dir()
              .join(format!("codec-lucene9-rbm-{}-{}", tag, std::process::id()));
          let _ = fs::remove_dir_all(&dir);
          dir
      }

      const SEG_ID: [u8; 16] = [7u8; 16];

      /// level/INFO: 0..5000 contiguous (Run); level/WARN: i*13 scattered
      /// (Bitset); message/apple: 4 docs across two keys (Array).
      fn write_test_sidecar(dir: &FSDirectory) {
          let mut b = SidecarBuilder::new("_0", 70_000);
          b.add_term("level", b"INFO", &(0..5000u32).collect::<Vec<_>>());
          b.add_term("level", b"WARN", &(0..5000u32).map(|i| i * 13).collect::<Vec<_>>());
          b.add_term("message", b"apple", &[1, 2, 3, 69_999]);
          b.write(dir, &SEG_ID).unwrap();
      }

      #[test]
      fn naming_stays_outside_lucene_namespace() {
          assert_eq!(file_name("_7"), "rbm_7.bin");
          // §4a.3: must NOT match CODEC_FILE_PATTERN (IndexFileNames.java:199,
          // `_[a-z0-9]+(_.*)?\..*` — requires a leading '_') and must not
          // start with segments/pending_segments; non-matching files are
          // never touched by IndexFileDeleter (:146-151).
          let n = file_name("_7");
          assert!(!n.starts_with('_'), "leading '_' would match CODEC_FILE_PATTERN");
          assert!(!n.starts_with("segments"));
          assert!(!n.starts_with("pending_segments"));
          assert!(n.starts_with("rbm") && n.ends_with(".bin"));
      }

      #[test]
      fn round_trip_write_open_lookup_load() {
          let root = temp_dir("roundtrip");
          let dir = FSDirectory::open(&root).unwrap();
          write_test_sidecar(&dir);
          assert!(dir.file_exists("rbm_0.bin"));

          let r = SidecarReader::open(&dir, "_0", &SEG_ID, 70_000).unwrap().unwrap();
          let info = r.term_entry("level", b"INFO").unwrap();
          assert_eq!(info.doc_freq, 5000);
          assert_eq!(info.cardinality, 5000);
          // count queries read only the term table (spec §4) — distinct rows
          let warn = r.term_entry("level", b"WARN").unwrap();
          assert_eq!(warn.cardinality, 5000);
          assert_ne!(info, warn);
          // lazy payload load reproduces the doc set
          let bm = r.load_bitmap(&info).unwrap();
          assert_eq!(bm.cardinality(), 5000);
          let apple = r.term_entry("message", b"apple").unwrap();
          assert_eq!(apple.doc_freq, 4);
          let bm = r.load_bitmap(&apple).unwrap();
          assert_eq!(bm.cardinality(), 4);
          let mut docs = Vec::new();
          for ci in 0..bm.num_containers() {
              let key = bm.container_key(ci) as u32;
              let c = bm.container_at(ci);
              for i in 0..c.cardinality() {
                  docs.push((key << 16) | c.value_at(i) as u32);
              }
          }
          assert_eq!(docs, vec![1, 2, 3, 69_999]);
          // absent field / term
          assert!(r.term_entry("nope", b"x").is_none());
          assert!(r.term_entry("level", b"DEBUG").is_none());
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn open_validates_binding_and_degrades_silently() {
          let root = temp_dir("binding");
          let dir = FSDirectory::open(&root).unwrap();
          write_test_sidecar(&dir);
          // §4a.6: segment name / maxDoc / segment id mismatch -> None
          assert!(SidecarReader::open(&dir, "_1", &SEG_ID, 70_000).unwrap().is_none());
          assert!(SidecarReader::open(&dir, "_0", &SEG_ID, 60_000).unwrap().is_none());
          assert!(SidecarReader::open(&dir, "_0", &[9u8; 16], 70_000).unwrap().is_none());
          // missing file -> None
          assert!(SidecarReader::open(&dir, "_9", &SEG_ID, 70_000).unwrap().is_none());
          // corrupted payload: open still succeeds (footer structure intact),
          // load fails cardinality/popcount validation -> caller falls back
          let path = root.join("rbm_0.bin");
          let mut bytes = fs::read(&path).unwrap();
          let i = bytes.len() * 3 / 5; // inside WARN's 8KB bitset payload
          bytes[i] ^= 0xFF;
          fs::write(&path, &bytes).unwrap();
          let r = SidecarReader::open(&dir, "_0", &SEG_ID, 70_000).unwrap().unwrap();
          let e = r.term_entry("level", b"WARN").unwrap();
          assert!(r.load_bitmap(&e).is_err(), "corrupt bitset payload must be detected");
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn footer_crc_covers_whole_file() {
          let root = temp_dir("crc");
          let dir = FSDirectory::open(&root).unwrap();
          write_test_sidecar(&dir);
          // whole-file CRC validates on the untouched file (CodecUtil
          // writeFooter semantics, codec_util.rs write_footer)
          let mut input = dir.open_checksum_input("rbm_0.bin").unwrap();
          let len = input.length();
          let mut buf = vec![0u8; (len - 16) as usize];
          input.read_bytes(&mut buf).unwrap();
          check_footer(&mut input).unwrap();
          // flip a payload byte -> CRC mismatch detected
          let path = root.join("rbm_0.bin");
          let mut bytes = fs::read(&path).unwrap();
          let i = bytes.len() * 3 / 5;
          bytes[i] ^= 0xFF;
          fs::write(&path, &bytes).unwrap();
          let mut input = dir.open_checksum_input("rbm_0.bin").unwrap();
          input.read_bytes(&mut buf).unwrap();
          assert!(check_footer(&mut input).is_err());
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn gc_removes_only_orphaned_sidecars() {
          let root = temp_dir("gc");
          let dir = FSDirectory::open(&root).unwrap();
          for name in ["rbm_0.bin", "rbm_1.bin", "rbmx.bin", "_0.si", "segments_1", "rbm_2.bak"] {
              fs::write(root.join(name), b"x").unwrap();
          }
          // §4a.4: rbm_1.bin's segment "_1" is not live -> deleted;
          // rbmx.bin (seg part lacks '_') and rbm_2.bak (not .bin) are not
          // sidecar names; .si/segments_N untouched
          let deleted = gc_sidecars(&dir, &["_0".to_string()]).unwrap();
          assert_eq!(deleted, 1);
          assert!(dir.file_exists("rbm_0.bin"));
          assert!(!dir.file_exists("rbm_1.bin"));
          assert!(dir.file_exists("rbmx.bin"));
          assert!(dir.file_exists("rbm_2.bak"));
          assert!(dir.file_exists("_0.si"));
          assert!(dir.file_exists("segments_1"));
          fs::remove_dir_all(&root).unwrap();
      }
  }
  ```

  同时 `crates/codec-lucene9/src/lib.rs` 在 `pub mod codec_util;` 之前插入一行 `pub mod bitmap_sidecar;`（否则模块未声明，测试无法编译）。

- [ ] **Step 3.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 bitmap_sidecar 2>&1 | tail -5
  error[E0433]: failed to resolve: use of unresolved module or unlinked crate `bitmap_sidecar`
  ...（SidecarBuilder / SidecarReader / gc_sidecars 未定义）
  ```

- [ ] **Step 3.3: 实现 bitmap_sidecar.rs** — `crates/codec-lucene9/src/bitmap_sidecar.rs` 在模块文档之后、`#[cfg(test)]` 之前插入：

  ```rust
  use std::io;

  use crate::codec_util::{
      check_footer_structure, check_index_header, corrupt, write_footer, write_index_header,
  };
  use crate::directory::FSDirectory;
  use crate::io::{DataInput, IndexInput, IndexOutput};
  use crate::roaring::RoaringBitmap;

  /// Codec name in the sidecar's index header (project CodecUtil convention).
  pub(crate) const SIDECAR_CODEC: &str = "RustRbmSidecar";
  pub(crate) const SIDECAR_VERSION: u32 = 0;

  /// §4a.3: rbm<seg>.bin — outside Lucene's managed namespace
  /// (IndexFileNames.CODEC_FILE_PATTERN requires a leading '_';
  /// IndexFileDeleter.java:146-151 never tracks non-matching names).
  pub fn file_name(segment: &str) -> String {
      format!("rbm{segment}.bin")
  }

  // ── write side ─────────────────────────────────────────────────────────

  /// Accumulates (field, term, bitmap) rows at segment flush; terms arrive
  /// in dictionary order per field (the block-tree flush order), so the
  /// term table is sorted for free (spec §4).
  pub struct SidecarBuilder {
      segment: String,
      max_doc: i32,
      fields: Vec<FieldBitmaps>,
  }

  struct FieldBitmaps {
      name: String,
      /// (term bytes, doc_freq, bitmap); term bytes strictly ascending
      terms: Vec<(Vec<u8>, u32, RoaringBitmap)>,
  }

  impl SidecarBuilder {
      pub fn new(segment: &str, max_doc: i32) -> Self {
          SidecarBuilder {
              segment: segment.to_string(),
              max_doc,
              fields: Vec::new(),
          }
      }

      /// spec §4: the docs slice is already in RAM at term flush — building
      /// the bitmap here is O(df) CPU, zero extra IO. Fields must arrive as
      /// one contiguous run each; terms ascending within a field.
      pub fn add_term(&mut self, field: &str, term: &[u8], docs: &[u32]) {
          if self.fields.last().map(|f| f.name.as_str()) != Some(field) {
              self.fields.push(FieldBitmaps {
                  name: field.to_string(),
                  terms: Vec::new(),
              });
          }
          let f = self.fields.last_mut().unwrap();
          debug_assert!(
              f.terms.last().map(|(t, _, _)| t.as_slice()) < Some(term),
              "terms must ascend within a field"
          );
          f.terms
              .push((term.to_vec(), docs.len() as u32, RoaringBitmap::from_sorted_docs(docs)));
      }

      pub fn is_empty(&self) -> bool {
          self.fields.is_empty()
      }

      /// Writes rbm<seg>.bin and returns the file name. The caller never
      /// lists it in the .si files set (§4a.2). Payload offsets are relative
      /// to the payload region start, so the term tables (which carry the
      /// offsets) can be written before the blobs without a size fixpoint.
      pub fn write(self, dir: &FSDirectory, segment_id: &[u8; 16]) -> io::Result<String> {
          let name = file_name(&self.segment);
          let mut out = dir.create_output(&name)?;
          write_index_header(&mut out, SIDECAR_CODEC, SIDECAR_VERSION, segment_id, "")?;
          out.write_string(&self.segment)?;
          out.write_vint(self.max_doc)?;
          out.write_vint(self.fields.len() as i32)?;
          // serialize payloads first (offsets relative to the payload region)
          let mut payload = IndexOutput::in_memory();
          let mut offsets: Vec<Vec<(u64, u32)>> = Vec::with_capacity(self.fields.len());
          for f in &self.fields {
              let mut field_offsets = Vec::with_capacity(f.terms.len());
              for (_, _, bm) in &f.terms {
                  let off = payload.file_pointer();
                  bm.serialize(&mut payload)?;
                  field_offsets.push((off, (payload.file_pointer() - off) as u32));
              }
              offsets.push(field_offsets);
          }
          for (f, field_offsets) in self.fields.iter().zip(&offsets) {
              out.write_string(&f.name)?;
              out.write_vint(f.terms.len() as i32)?;
              for ((term, df, bm), &(off, len)) in f.terms.iter().zip(field_offsets) {
                  out.write_vint(term.len() as i32)?;
                  out.write_bytes(term)?;
                  out.write_vint(*df as i32)?;
                  out.write_vlong(bm.cardinality() as i64)?;
                  out.write_vlong(off as i64)?;
                  out.write_vint(len as i32)?;
              }
          }
          out.write_bytes(&payload.into_bytes())?;
          write_footer(&mut out)?;
          out.flush()?;
          Ok(name)
      }
  }

  // ── read side ──────────────────────────────────────────────────────────

  /// Term-table row (spec §4: cardinality lives in the table, so count
  /// queries never touch the payload).
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct SidecarTerm {
      pub doc_freq: u32,
      pub cardinality: u64,
      pub(crate) payload_off: u64,
      pub(crate) payload_len: u32,
  }

  struct SidecarField {
      name: String,
      /// rows sorted by term bytes (binary-searched)
      terms: Vec<(Vec<u8>, SidecarTerm)>,
  }

  /// Read view of one segment's `rbm<seg>.bin`: open reads only the header
  /// + term tables (KB-scale); payloads load lazily per term (spec §5
  /// 惰性加载: bitmap payload 首次触及才加载).
  pub struct SidecarReader {
      input: IndexInput,
      payload_base: u64,
      fields: Vec<SidecarField>,
  }

  impl SidecarReader {
      /// §4a.6: any binding/validation failure (absent file, bad header,
      /// wrong segment name or maxDoc, bad footer structure) degrades to
      /// `Ok(None)` — a bad sidecar must never affect queries.
      pub fn open(
          dir: &FSDirectory,
          segment: &str,
          segment_id: &[u8; 16],
          max_doc: i32,
      ) -> io::Result<Option<SidecarReader>> {
          let name = file_name(segment);
          if !dir.file_exists(&name) {
              return Ok(None);
          }
          let input = dir.open_input(&name)?;
          Ok(Self::parse(input, segment, segment_id, max_doc).ok())
      }

      fn parse(
          mut input: IndexInput,
          segment: &str,
          segment_id: &[u8; 16],
          max_doc: i32,
      ) -> io::Result<SidecarReader> {
          check_index_header(
              &mut input,
              SIDECAR_CODEC,
              SIDECAR_VERSION,
              SIDECAR_VERSION,
              segment_id,
              "",
          )?;
          if input.read_string()? != segment {
              return Err(corrupt("sidecar segment name mismatch"));
          }
          if input.read_vint()? != max_doc {
              return Err(corrupt("sidecar maxDoc mismatch"));
          }
          let num_fields = input.read_vint()?;
          if !(0..=65536).contains(&num_fields) {
              return Err(corrupt("sidecar field count out of range"));
          }
          let mut fields = Vec::with_capacity(num_fields as usize);
          for _ in 0..num_fields {
              let name = input.read_string()?;
              let num_terms = input.read_vint()?;
              if !(0..=(1 << 26)).contains(&num_terms) {
                  return Err(corrupt("sidecar term count out of range"));
              }
              let mut terms = Vec::with_capacity((num_terms as usize).min(1 << 16));
              let mut prev: Option<Vec<u8>> = None;
              for _ in 0..num_terms {
                  let tlen = input.read_vint()?;
                  if !(0..=(1 << 20)).contains(&tlen) {
                      return Err(corrupt("sidecar term length out of range"));
                  }
                  let mut t = vec![0u8; tlen as usize];
                  input.read_bytes(&mut t)?;
                  if prev.as_deref() >= Some(t.as_slice()) {
                      return Err(corrupt("sidecar terms must strictly ascend"));
                  }
                  prev = Some(t.clone());
                  let doc_freq = input.read_vint()?;
                  if doc_freq <= 0 {
                      return Err(corrupt("sidecar df must be positive"));
                  }
                  let cardinality = input.read_vlong()?;
                  if cardinality <= 0 {
                      return Err(corrupt("sidecar cardinality must be positive"));
                  }
                  let payload_off = input.read_vlong()?;
                  if payload_off < 0 {
                      return Err(corrupt("sidecar payload offset negative"));
                  }
                  let payload_len = input.read_vint()?;
                  if payload_len <= 0 {
                      return Err(corrupt("sidecar payload length must be positive"));
                  }
                  terms.push((
                      t,
                      SidecarTerm {
                          doc_freq: doc_freq as u32,
                          cardinality: cardinality as u64,
                          payload_off: payload_off as u64,
                          payload_len: payload_len as u32,
                      },
                  ));
              }
              fields.push(SidecarField { name, terms });
          }
          let payload_base = input.file_pointer();
          // Footer structure (magic/algorithmID) is validated now; the
          // whole-file CRC is exercised by the codec tests and payloads are
          // validated per load via cardinality/popcount (§4a.6).
          check_footer_structure(&input, input.length())?;
          Ok(SidecarReader {
              input,
              payload_base,
              fields,
          })
      }

      /// Term-table lookup (binary search on the sorted rows). No payload IO.
      pub fn term_entry(&self, field: &str, term: &[u8]) -> Option<SidecarTerm> {
          let f = self.fields.iter().find(|f| f.name == field)?;
          f.terms
              .binary_search_by(|(t, _)| t.as_slice().cmp(term))
              .ok()
              .map(|i| f.terms[i].1.clone())
      }

      /// Lazy payload load + deserialize; cardinality is cross-checked
      /// against the term table (§4a.6 term-level validation).
      pub fn load_bitmap(&self, entry: &SidecarTerm) -> io::Result<RoaringBitmap> {
          let mut slice = self
              .input
              .slice(self.payload_base + entry.payload_off, entry.payload_len as u64)?;
          let bm = RoaringBitmap::deserialize(&mut slice)?;
          if bm.cardinality() != entry.cardinality {
              return Err(corrupt("sidecar payload cardinality mismatch"));
          }
          Ok(bm)
      }
  }

  // ── GC (§4a.4) ─────────────────────────────────────────────────────────

  /// Rust-side GC: Java merge/delete leaves orphaned sidecars behind (Java
  /// never touches them, IndexFileDeleter.java:146-151). Deletes every
  /// `rbm<seg>.bin` whose segment is not in `live_segments`; returns the
  /// number of deleted files. Called at IndexWriter open and after flush.
  pub fn gc_sidecars(dir: &FSDirectory, live_segments: &[String]) -> io::Result<usize> {
      let mut deleted = 0;
      for name in dir.list_all()? {
          let Some(seg) = name.strip_prefix("rbm").and_then(|n| n.strip_suffix(".bin")) else {
              continue;
          };
          // sidecar names are rbm<seg>.bin and segment names always start
          // with '_' (SegmentBuilder: format!("_{}", base36)) — anything
          // else with an rbm prefix is not ours
          if !seg.starts_with('_') {
              continue;
          }
          if !live_segments.iter().any(|s| s == seg) {
              dir.delete(&name)?;
              deleted += 1;
          }
      }
      Ok(deleted)
  }
  ```

- [ ] **Step 3.4: 跑测试确认通过（codec 5 个 sidecar 测试 + 全量回归）**

  ```
  $ cargo test -p codec-lucene9 bitmap_sidecar 2>&1 | tail -3
  test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p codec-lucene9 2>&1 | tail -3
  test result: ok. 155 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 3.5: 提交（codec 部分）**

  ```
  git add crates/codec-lucene9/src/bitmap_sidecar.rs crates/codec-lucene9/src/lib.rs
  git commit -m "feat: roaring bitmap sidecar file format (write/read/validate/GC, M3 §4a)"
  ```

- [ ] **Step 3.6: 写 core 失败测试** — `crates/core/src/index_writer.rs` 末尾新建 `#[cfg(test)]` 模块（`IndexWriterConfig.bitmap` 等字段尚不存在，编译失败即失败测试成立）：

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::document::{Document, FieldValue};
      use crate::schema::{FieldSpec, Schema};
      use codec_lucene9::bitmap_sidecar::SidecarReader;
      use std::fs;
      use std::path::PathBuf;

      fn temp_dir(tag: &str) -> PathBuf {
          let dir = std::env::temp_dir()
              .join(format!("rustlucene-writer-{}-{}", tag, std::process::id()));
          let _ = fs::remove_dir_all(&dir);
          dir
      }

      fn schema() -> Schema {
          let mut s = Schema::new();
          s.add(FieldSpec::keyword("level"));
          s.add(FieldSpec::text("message"));
          s
      }

      fn doc(level: &str, message: &str) -> Document {
          let mut d = Document::new();
          d.add("level", FieldValue::Keyword(level.to_string()));
          d.add("message", FieldValue::Text(message.to_string()));
          d
      }

      /// M3 §4/§4a: with bitmap on, flush emits rbm_0.bin covering terms
      /// with df >= threshold; the .si files set must never reference it.
      #[test]
      fn bitmap_sidecar_written_outside_si_files_set() {
          let root = temp_dir("rbmwrite");
          let mut cfg = IndexWriterConfig::default();
          cfg.bitmap = true;
          cfg.bitmap_threshold = 4;
          let mut w = IndexWriter::create(&root, schema(), cfg).unwrap();
          for i in 0..10 {
              w.add_document(doc("INFO", &format!("common w{i}"))).unwrap();
          }
          w.commit().unwrap();
          drop(w);

          let dir = FSDirectory::open(&root).unwrap();
          let names = dir.list_all().unwrap();
          assert_eq!(
              names.iter().filter(|n| n.starts_with("rbm")).count(),
              1,
              "one segment -> one sidecar"
          );
          assert!(names.iter().any(|n| n == "rbm_0.bin"));

          // §4a.2: the .si files set must never reference the sidecar
          let (infos, _) = SegmentInfos::read_latest(&dir).unwrap();
          assert_eq!(infos.segments.len(), 1);
          let sci = &infos.segments[0];
          assert!(
              !sci.info.files.iter().any(|f| f.starts_with("rbm")),
              "sidecar must stay out of the .si files set (§4a.2)"
          );

          // term table: high-df terms present, low-df absent
          let r = SidecarReader::open(&dir, &sci.info.name, &sci.info.id, sci.info.doc_count)
              .unwrap()
              .unwrap();
          let common = r.term_entry("message", b"common").unwrap();
          assert_eq!(common.doc_freq, 10);
          assert_eq!(common.cardinality, 10);
          assert!(r.term_entry("level", b"INFO").is_some());
          assert!(r.term_entry("message", b"w3").is_none(), "df=1 < threshold 4");
          fs::remove_dir_all(&root).unwrap();
      }

      /// §4a.4: writer-open GC removes orphaned sidecars (a fresh CREATE
      /// has no live segments), and leaves non-sidecar rbm* files alone.
      #[test]
      fn gc_on_create_removes_orphan_sidecars() {
          let root = temp_dir("rbmgc");
          fs::create_dir_all(&root).unwrap();
          fs::write(root.join("rbm_7.bin"), b"orphan").unwrap();
          fs::write(root.join("rbmx.bin"), b"not a sidecar name").unwrap();
          let w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
          drop(w);
          assert!(!root.join("rbm_7.bin").exists(), "§4a.4: orphan GC at writer open");
          assert!(root.join("rbmx.bin").exists());
          fs::remove_dir_all(&root).unwrap();
      }

      /// spec §3: --bitmap default off -> no sidecar files at all.
      #[test]
      fn bitmap_off_writes_no_sidecar() {
          let root = temp_dir("rbmoff");
          let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
          for i in 0..10 {
              w.add_document(doc("INFO", &format!("common w{i}"))).unwrap();
          }
          w.commit().unwrap();
          drop(w);
          let dir = FSDirectory::open(&root).unwrap();
          assert!(!dir.list_all().unwrap().iter().any(|n| n.starts_with("rbm")));
          fs::remove_dir_all(&root).unwrap();
      }
  }
  ```

- [ ] **Step 3.7: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core index_writer 2>&1 | tail -5
  error[E0609]: no field `bitmap` on type `IndexWriterConfig`
  ```

- [ ] **Step 3.8: 实现 core 写侧钩子** — 三处修改：

  (a) `crates/core/src/index_writer.rs`：`IndexWriterConfig` 加两个字段（默认值保证 `--bitmap` 默认 off、既有 `::default()` 调用方不受影响）；`create` 与 `flush` 加 GC：

  ```rust
  pub struct IndexWriterConfig {
      /// Flush when this many docs are buffered (Lucene default: disabled / RAM-based).
      pub max_buffered_docs: u32,
      /// Flush when the buffered indexing data (postings/docvalues/points
      /// arenas, approximate) exceeds this many bytes. Default 512MB, per the
      /// project spec; Lucene's fair-comparison counterpart is
      /// IndexWriterConfig.setRAMBufferSizeMB.
      pub max_ram_bytes: usize,
      /// M3 (spec §3): build roaring-bitmap sidecar files (rbm<seg>.bin) at
      /// segment flush. Experimental, default off.
      pub bitmap: bool,
      /// M3 (spec §3): minimum doc_freq for a term to get a sidecar bitmap.
      /// Default 4096 (codec_lucene9::roaring::DEFAULT_BITMAP_THRESHOLD,
      /// the 32×128 level-1 skip granularity); --bitmap-threshold tunes it.
      pub bitmap_threshold: u32,
  }

  impl Default for IndexWriterConfig {
      fn default() -> Self {
          Self {
              max_buffered_docs: 1_000_000,
              max_ram_bytes: 512 * 1024 * 1024,
              bitmap: false,
              bitmap_threshold: codec_lucene9::roaring::DEFAULT_BITMAP_THRESHOLD,
          }
      }
  }
  ```

  `create()` 在 `segments_` 存在性检查之后、`Ok(Self { ... })` 之前插入：

  ```rust
          // §4a.4: GC orphaned sidecars at writer open (a fresh CREATE has
          // no live segments, so every leftover rbm<seg>.bin goes away)
          codec_lucene9::bitmap_sidecar::gc_sidecars(&dir, &[])?;
  ```

  `flush()` 在 builder finalize 块之后、`Ok(())` 之前插入：

  ```rust
          // §4a.4: GC orphaned sidecars after flush (Rust has no merge —
          // orphans only ever come from Java-side merges/deletes)
          let live: Vec<String> = self.infos.segments.iter().map(|s| s.info.name.clone()).collect();
          codec_lucene9::bitmap_sidecar::gc_sidecars(&self.dir, &live)?;
  ```

  `add_document` 的 builder 创建改为：

  ```rust
          if self.builder.is_none() {
              self.builder = Some(
                  SegmentBuilder::new(self.dir.clone(), self.segment_counter)
                      .with_bitmap(self.config.bitmap, self.config.bitmap_threshold),
              );
              self.segment_counter += 1;
          }
  ```

  (b) `crates/core/src/segment_builder.rs`：`SegmentBuilder` 加两个字段 + `with_bitmap`；`finalize` 的 postings 块加 sidecar 构建与 fsync：

  ```rust
  pub struct SegmentBuilder {
      dir: FSDirectory,
      seg_name: String,
      seg_id: [u8; 16],
      dw: DocWriter,
      sfw: Option<StoredFieldsWriter>,
      bitmap_enabled: bool,
      bitmap_threshold: u32,
  }

  impl SegmentBuilder {
      pub fn new(dir: FSDirectory, name_counter: u64) -> Self {
          Self {
              dir,
              seg_name: format!("_{}", to_base36(name_counter)),
              seg_id: random_id(),
              dw: DocWriter::new(),
              sfw: None,
              bitmap_enabled: false,
              bitmap_threshold: codec_lucene9::roaring::DEFAULT_BITMAP_THRESHOLD,
          }
      }

      /// M3 (spec §3): opt into roaring-bitmap sidecar output at finalize
      /// (default off; IndexWriterConfig::bitmap / --bitmap-threshold).
      pub fn with_bitmap(mut self, enabled: bool, threshold: u32) -> Self {
          self.bitmap_enabled = enabled;
          self.bitmap_threshold = threshold;
          self
      }
  ```

  `finalize` 开头的解构要带上新字段（否则会漏）：

  ```rust
          let Self {
              dir,
              seg_name,
              seg_id,
              mut dw,
              sfw,
              bitmap_enabled,
              bitmap_threshold,
          } = self;
  ```

  `if has_indexed` 块改为（在 `PostingsWriter::new` 之后建 builder、`write_term` 之后喂 docs、`pw.finish()` 之后写文件 + fsync）：

  ```rust
          if has_indexed {
              let mut pw = PostingsWriter::new(&dir, &seg_name, &seg_id)?;
              // M3 (spec §4): the sidecar builds alongside the term loop —
              // the docs slices are already in RAM, so this is O(df) CPU
              // and zero extra IO
              let mut sidecar =
                  bitmap_enabled.then(|| codec_lucene9::bitmap_sidecar::SidecarBuilder::new(&seg_name, max_doc));
              for (number, spec) in dw.fields().iter().enumerate() {
                  if !spec.is_indexed() || !field_has_terms(&dw, number) {
                      continue;
                  }
                  let buf = dw.field_buffer(number as u32).unwrap();
                  let dict = buf.dict.as_ref().unwrap();
                  let fi = field_infos.by_name(&spec.name).unwrap();
                  pw.start_field(fi, buf.doc_count)?;
                  for id in dict.sorted_ids() {
                      let pb = dict.postings(id);
                      let positions = if spec.has_positions() {
                          Some(pb.positions.as_slice())
                      } else {
                          None
                      };
                      pw.write_term(dict.bytes_of(id), &pb.docs, &pb.freqs, positions)?;
                      if let Some(sb) = sidecar.as_mut() {
                          if pb.docs.len() as u32 >= bitmap_threshold {
                              sb.add_term(&spec.name, dict.bytes_of(id), &pb.docs);
                          }
                      }
                  }
                  pw.finish_field()?;
              }
              postings_files = pw.finish()?;
              // §4a.2/3: the sidecar is a directory-level file outside
              // Lucene's managed namespace — never listed in .si files.
              // fsync it here because commit_infos only syncs .si-listed
              // files; a torn sidecar after a crash degrades to "no
              // bitmap" at read time (§4a.6), never to a query error.
              if let Some(sb) = sidecar {
                  if !sb.is_empty() {
                      let name = sb.write(&dir, &seg_id)?;
                      dir.sync(&[name.as_str()])?;
                  }
              }
          }
  ```

- [ ] **Step 3.9: 跑测试确认通过（core 3 个新测试 + 全量回归）**

  ```
  $ cargo test -p rustlucene-core index_writer 2>&1 | tail -3
  test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p rustlucene-core 2>&1 | tail -3
  test result: ok. 42 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 3.10: 提交（core 部分）**

  ```
  git add crates/core/src/index_writer.rs crates/core/src/segment_builder.rs
  git commit -m "feat: build roaring sidecars at segment flush behind --bitmap (M3 §4/§4a)"
  ```

---

## Task 4: 读侧 RoaringDocIter + Term 接入（spec §5 档 1）

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（`RoaringDocIter` + `SegmentDocIter::Roaring`）
- Modify: `crates/core/src/search/segment_reader.rs`（sidecar 字段 + `open_with_bitmap` + `roaring_bitmap`）
- Modify: `crates/core/src/search/reader.rs`（`Reader::open_with_bitmap`）
- Modify: `crates/core/src/search/searcher.rs`（`Searcher::open_with_bitmap`）
- Modify: `crates/core/src/search/query.rs`（Term 分支档 1）
- Test: `crates/core/src/search/mod.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T1 `RoaringBitmap` + `Container::{cardinality, value_at, lower_bound}`；T3 `SidecarReader`/`SidecarTerm`；既有 `SegmentReader::seek_term` / `docs_enum` / `docs_freqs_enum`、`DocIter` 协议（`doc_iter.rs:14-31`）。
- Produces（T5/T6 依赖这些名字，不得改名）:
  ```rust
  // doc_iter.rs
  pub struct RoaringDocIter { .. }
  impl RoaringDocIter {
      pub fn new(bm: RoaringBitmap) -> Self;
      pub fn cardinality(&self) -> u64;
  }
  // SegmentDocIter 新增变体 Roaring(RoaringDocIter)，DocIter 全协议实现

  // segment_reader.rs
  pub(crate) fn open_with_bitmap(dir: &FSDirectory, sci: &SegmentCommitInfo, bitmap: bool) -> io::Result<SegmentReader>;
  /// §5 位图源 + §4a.6 term 级校验（cardinality == postings df），任何
  /// 不匹配/加载错误 → Ok(None) 静默落档
  pub(crate) fn roaring_bitmap(&self, field: &str, term: &[u8], doc_freq: u32) -> io::Result<Option<RoaringBitmap>>;

  // reader.rs / searcher.rs
  pub fn Reader::open_with_bitmap(dir: &FSDirectory, bitmap: bool) -> io::Result<Reader>;
  pub fn Searcher::open_with_bitmap(dir: &FSDirectory, bitmap: bool) -> io::Result<Searcher>;
  ```

  语义决定：读侧自动探测——segment 有 sidecar 且 term 命中 term 表且校验通过 → roaring；否则原路径。`needs_freq == true` 时 Term 分支不进 roaring（bitmap 无 freq，spec §2；FreqSumCollector 只用于 Term，且 freq_sum 本就走 `total_term_freq` 短路，`searcher.rs:113-121`）。

### Steps

- [ ] **Step 4.1: 写失败测试** — 追加到 `crates/core/src/search/mod.rs` 的 `mod tests`（复用模块内已有 `temp_dir` / `schema` / `doc` 助手；`Searcher::open_with_bitmap`、`SegmentDocIter::Roaring` 尚不存在，编译失败即失败测试成立）：

  ```rust
      /// 10 docs, threshold 4: message "common" (df=10) and level INFO/WARN
      /// (df=5) get sidecar bitmaps; w{i} (df=1) stay postings-only.
      fn write_bitmap_corpus(root: &std::path::Path, bitmap: bool) {
          let mut cfg = IndexWriterConfig::default();
          cfg.bitmap = bitmap;
          cfg.bitmap_threshold = 4;
          let mut w = IndexWriter::create(root, schema(), cfg).unwrap();
          for i in 0..10 {
              let level = if i % 2 == 0 { "INFO" } else { "WARN" };
              w.add_document(doc(level, &format!("tid-{i}"), &format!("common w{i}")))
                  .unwrap();
          }
          w.commit().unwrap();
          drop(w);
      }

      #[test]
      fn roaring_term_path_matches_postings_path() {
          let root = temp_dir("rbmterm");
          write_bitmap_corpus(&root, true);
          let dir = FSDirectory::open(&root).unwrap();
          let mut on = Searcher::open(&dir).unwrap();
          let mut off = Searcher::open_with_bitmap(&dir, false).unwrap();
          // spec §8: 同一查询电池 bitmap on/off 结果逐位一致
          for q in [
              Query::term("message", "common"), // bitmap
              Query::term("message", "w3"),     // df=1, postings
              Query::term("level", "INFO"),     // keyword field, bitmap
              Query::term("message", "absent"), // miss
          ] {
              assert_eq!(on.count(&q).unwrap(), off.count(&q).unwrap(), "count {q:?}");
              assert_eq!(on.top_docs(&q, 20).unwrap(), off.top_docs(&q, 20).unwrap(), "docs {q:?}");
          }
          // freq_sum unaffected (total_term_freq shortcut)
          assert_eq!(
              on.freq_sum(&Query::term("message", "common")).unwrap(),
              off.freq_sum(&Query::term("message", "common")).unwrap()
          );

          // tier-1 proof: the bitmapped term takes the Roaring iterator
          let mut reader = Reader::open(&dir).unwrap();
          let (_, seg) = reader.leaves().next().unwrap();
          let it = Query::term("message", "common")
              .segment_iterator(seg, false)
              .unwrap()
              .unwrap();
          assert!(matches!(it, SegmentDocIter::Roaring(_)));
          // low-df term stays on postings
          let it = Query::term("message", "w3")
              .segment_iterator(seg, false)
              .unwrap()
              .unwrap();
          assert!(matches!(it, SegmentDocIter::Freqs(_)));
          // freq consumer keeps the postings enum even with a bitmap present
          let it = Query::term("message", "common")
              .segment_iterator(seg, true)
              .unwrap()
              .unwrap();
          assert!(matches!(it, SegmentDocIter::Freqs(_)));
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn roaring_doc_iter_iteration_and_advance() {
          let root = temp_dir("rbmiter");
          write_bitmap_corpus(&root, true);
          let dir = FSDirectory::open(&root).unwrap();
          let mut reader = Reader::open(&dir).unwrap();
          let (_, seg) = reader.leaves().next().unwrap();
          let mut it = Query::term("message", "common")
              .segment_iterator(seg, false)
              .unwrap()
              .unwrap();
          // "common" hits every doc 0..10
          for d in 0..10 {
              assert_eq!(it.next_doc().unwrap(), d);
          }
          assert_eq!(it.next_doc().unwrap(), NO_MORE_DOCS);
          assert_eq!(it.next_doc().unwrap(), NO_MORE_DOCS, "NO_MORE_DOCS is sticky");

          let mut it = Query::term("message", "common")
              .segment_iterator(seg, false)
              .unwrap()
              .unwrap();
          assert_eq!(it.advance(3).unwrap(), 3);
          assert_eq!(it.advance(3).unwrap(), 3, "advance idempotent");
          assert_eq!(it.next_doc().unwrap(), 4);
          assert_eq!(it.advance(9).unwrap(), 9);
          assert_eq!(it.advance(10).unwrap(), NO_MORE_DOCS);
          assert_eq!(it.next_doc().unwrap(), NO_MORE_DOCS);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn no_sidecar_segment_stays_on_postings() {
          let root = temp_dir("rbmnone");
          write_bitmap_corpus(&root, false); // bitmap off at write time
          let dir = FSDirectory::open(&root).unwrap();
          // read side auto-detects: no sidecar -> postings (spec §5 档规则按段独立)
          let mut reader = Reader::open(&dir).unwrap();
          let (_, seg) = reader.leaves().next().unwrap();
          let it = Query::term("message", "common")
              .segment_iterator(seg, false)
              .unwrap()
              .unwrap();
          assert!(matches!(it, SegmentDocIter::Freqs(_)));
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  同时在 `mod tests` 的 use 块加一行 `use codec_lucene9::postings_read::NO_MORE_DOCS;`。

- [ ] **Step 4.2: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core roaring 2>&1 | tail -5
  error[E0425]: cannot find function `open_with_bitmap` in `Searcher`
  ...（SegmentDocIter::Roaring 同样未定义）
  ```

- [ ] **Step 4.3: 最小实现** — 五处修改：

  (a) `crates/core/src/search/doc_iter.rs`：`use codec_lucene9::roaring::RoaringBitmap;` 加到 use 块；`RoaringDocIter` 插在 `// ── SegmentDocIter` 注释之前：

  ```rust
  // ── Roaring (M3 bitmap sidecar) ───────────────────────────────────────

  /// DocIter over a (sidecar-loaded or query-materialized) roaring bitmap
  /// (spec M3 §5): next_doc walks (container, index-in-container); advance
  /// is a container lookup + intra-container lower_bound. freq() is 1 —
  /// the bitmap carries no freqs (doc-set semantics, ConstantScore).
  pub struct RoaringDocIter {
      bm: RoaringBitmap,
      ci: usize, // current container index
      vi: usize, // next index within the container
      doc: i32,
  }

  impl RoaringDocIter {
      pub fn new(bm: RoaringBitmap) -> Self {
          RoaringDocIter {
              bm,
              ci: 0,
              vi: 0,
              doc: -1,
          }
      }

      /// spec §5: count = cardinality（容器基数和，O(容器数)）.
      pub fn cardinality(&self) -> u64 {
          self.bm.cardinality()
      }

      fn current_doc(&self) -> i32 {
          ((self.bm.container_key(self.ci) as i32) << 16)
              | self.bm.container_at(self.ci).value_at(self.vi) as i32
      }
  }

  impl DocIter for RoaringDocIter {
      fn doc_id(&self) -> i32 {
          self.doc
      }

      fn next_doc(&mut self) -> io::Result<i32> {
          if self.doc == NO_MORE_DOCS {
              return Ok(NO_MORE_DOCS);
          }
          if self.doc >= 0 {
              self.vi += 1;
          }
          while self.ci < self.bm.num_containers()
              && self.vi >= self.bm.container_at(self.ci).cardinality()
          {
              self.ci += 1;
              self.vi = 0;
          }
          self.doc = if self.ci == self.bm.num_containers() {
              NO_MORE_DOCS
          } else {
              self.current_doc()
          };
          Ok(self.doc)
      }

      fn advance(&mut self, target: i32) -> io::Result<i32> {
          if self.doc >= target {
              return Ok(self.doc);
          }
          if self.doc == NO_MORE_DOCS {
              return Ok(NO_MORE_DOCS);
          }
          let t = target.max(0) as u32;
          let key = (t >> 16) as u16;
          let lo16 = (t & 0xFFFF) as u16;
          let n = self.bm.num_containers();
          // containers before self.ci hold only docs < current doc < target
          let mut ci = self.ci;
          while ci < n && self.bm.container_key(ci) < key {
              ci += 1;
          }
          if ci == n {
              self.ci = n;
              self.doc = NO_MORE_DOCS;
              return Ok(NO_MORE_DOCS);
          }
          let c = self.bm.container_at(ci);
          let vi = if self.bm.container_key(ci) == key {
              c.lower_bound(lo16)
          } else {
              0
          };
          if vi == c.cardinality() {
              // target is beyond this container: land on the next one's first
              self.ci = ci + 1;
              self.vi = 0;
              if self.ci == n {
                  self.doc = NO_MORE_DOCS;
                  return Ok(NO_MORE_DOCS);
              }
          } else {
              self.ci = ci;
              self.vi = vi;
          }
          self.doc = self.current_doc();
          Ok(self.doc)
      }
      // freq: 1 (trait default) — the bitmap carries no freqs (spec M3 §2)
  }
  ```

  `SegmentDocIter` 枚举加变体与四个 match 分支：

  ```rust
  pub enum SegmentDocIter {
      Docs(DocsEnum),
      Freqs(DocsFreqsEnum),
      All(MatchAllIter),
      And(ConjunctionDocIter),
      Or(DisjunctionDocIter),
      Bitset(BitsetDocIter),
      Phrase(PhraseDocIter),
      Roaring(RoaringDocIter),
  }
  ```

  （`doc_id`/`next_doc`/`advance` 三个 match 各加 `Self::Roaring(r) => r.xxx(...)`；`freq` 的 match 走 `_ => 1` 既有分支，无需改。）

  (b) `crates/core/src/search/segment_reader.rs`：use 块加 `use codec_lucene9::bitmap_sidecar::SidecarReader;` 与 `use codec_lucene9::roaring::RoaringBitmap;`；结构体加字段 + 两个方法：

  ```rust
  pub struct SegmentReader {
      max_doc: i32,
      field_infos: FieldInfos,
      terms: TermsDict,
      postings: PostingsReader,
      sidecar: Option<SidecarReader>,
  }

  impl SegmentReader {
      pub fn open(dir: &FSDirectory, sci: &SegmentCommitInfo) -> io::Result<SegmentReader> {
          Self::open_with_bitmap(dir, sci, true)
      }

      /// `bitmap == false` disables the roaring sidecar read path (bench
      /// A/B switch, spec M3 §5); with `true` the sidecar is auto-detected.
      pub(crate) fn open_with_bitmap(
          dir: &FSDirectory,
          sci: &SegmentCommitInfo,
          bitmap: bool,
      ) -> io::Result<SegmentReader> {
          let segment = &sci.info.name;
          let segment_id = &sci.info.id;
          let field_infos = FieldInfos::read(dir, segment, segment_id, "")?;
          let terms = TermsDict::open(dir, segment, segment_id, &field_infos)?;
          let postings = PostingsReader::open(dir, segment, segment_id)?;
          let sidecar = if bitmap {
              SidecarReader::open(dir, segment, segment_id, sci.info.doc_count)?
          } else {
              None
          };
          Ok(SegmentReader {
              max_doc: sci.info.doc_count,
              field_infos,
              terms,
              postings,
              sidecar,
          })
      }

      /// M3 §5 bitmap source + §4a.6 term-level validation: the sidecar
      /// row's df/cardinality must equal the postings df the reader
      /// already knows; any mismatch or payload error degrades to
      /// `Ok(None)` (silent fallback to the postings path — a bad sidecar
      /// must never affect queries).
      pub(crate) fn roaring_bitmap(
          &self,
          field: &str,
          term: &[u8],
          doc_freq: u32,
      ) -> io::Result<Option<RoaringBitmap>> {
          let Some(sidecar) = &self.sidecar else {
              return Ok(None);
          };
          let Some(entry) = sidecar.term_entry(field, term) else {
              return Ok(None);
          };
          if entry.doc_freq != doc_freq || entry.cardinality != doc_freq as u64 {
              return Ok(None);
          }
          match sidecar.load_bitmap(&entry) {
              Ok(bm) if bm.cardinality() == doc_freq as u64 => Ok(Some(bm)),
              _ => Ok(None),
          }
      }
  ```

  (c) `crates/core/src/search/reader.rs`：`open` 委托 + 新方法：

  ```rust
      /// DirectoryReader.open: reads the latest commit and opens every
      /// segment in commit order (global docID = docBase + segment docID).
      pub fn open(dir: &FSDirectory) -> io::Result<Reader> {
          Self::open_with_bitmap(dir, true)
      }

      /// `bitmap == false` disables roaring sidecar reads in every segment
      /// (bench A/B on one and the same index, spec M3 §5).
      pub fn open_with_bitmap(dir: &FSDirectory, bitmap: bool) -> io::Result<Reader> {
          let (infos, _generation) = SegmentInfos::read_latest(dir)?;
          let mut segments = Vec::with_capacity(infos.segments.len());
          let mut doc_bases = Vec::with_capacity(infos.segments.len());
          let mut base = 0i32;
          for sci in &infos.segments {
              let seg = SegmentReader::open_with_bitmap(dir, sci, bitmap)?;
              doc_bases.push(base);
              base += seg.max_doc();
              segments.push(seg);
          }
          Ok(Reader {
              segments,
              doc_bases,
              max_doc: base,
          })
      }
  ```

  (d) `crates/core/src/search/searcher.rs`：

  ```rust
      pub fn open(dir: &FSDirectory) -> io::Result<Searcher> {
          Ok(Searcher {
              reader: Reader::open(dir)?,
          })
      }

      /// `bitmap == false` disables the roaring read path on an index that
      /// has sidecars — bench A/B on one and the same index (spec M3 §5).
      pub fn open_with_bitmap(dir: &FSDirectory, bitmap: bool) -> io::Result<Searcher> {
          Ok(Searcher {
              reader: Reader::open_with_bitmap(dir, bitmap)?,
          })
      }
  ```

  (e) `crates/core/src/search/query.rs`：use 块加 `RoaringDocIter`（doc_iter use 列表内）与 `use codec_lucene9::roaring::RoaringBitmap;`（T5 也用）；Term 分支改为：

  ```rust
              Query::Term { field, term } => {
                  let Some((has_freqs, entry)) = seg.seek_term(field, term)? else {
                      return Ok(None);
                  };
                  // M3 §5 tier 1: sidecar bitmap -> roaring iteration (freq
                  // consumers keep the postings enum — the bitmap carries
                  // no freqs, spec §2)
                  if !needs_freq {
                      if let Some(bm) = seg.roaring_bitmap(field, term, entry.doc_freq)? {
                          return Ok(Some(SegmentDocIter::Roaring(RoaringDocIter::new(bm))));
                      }
                  }
                  if has_freqs {
                      Ok(Some(SegmentDocIter::Freqs(
                          seg.docs_freqs_enum(&entry, needs_freq)?,
                      )))
                  } else {
                      Ok(Some(SegmentDocIter::Docs(seg.docs_enum(&entry)?)))
                  }
              }
  ```

- [ ] **Step 4.4: 跑测试确认通过（3 个新测试 + 全量回归）**

  ```
  $ cargo test -p rustlucene-core roaring 2>&1 | tail -3
  test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p rustlucene-core 2>&1 | tail -3
  test result: ok. 45 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 4.5: 提交**

  ```
  git add crates/core/src/search/doc_iter.rs crates/core/src/search/segment_reader.rs crates/core/src/search/reader.rs crates/core/src/search/searcher.rs crates/core/src/search/query.rs crates/core/src/search/mod.rs
  git commit -m "feat: RoaringDocIter + Term-query bitmap source (M3 tier 1)"
  ```

---

## Task 5: And/Or 接入 + 查询时物化（spec §5 档 1/2/3）

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（`materialize_roaring`）
- Modify: `crates/core/src/search/query.rs`（And/Or 分支三档判定）
- Test: `crates/core/src/search/mod.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T4 的 `RoaringDocIter` / `SegmentReader::roaring_bitmap`；T1 的 `RoaringBitmap::{from_sorted_docs, and, or, cardinality}`；既有 `SegmentReader::{docs_enum, docs_freqs_enum, seek_term, field_info}`、`ConjunctionDocIter` / `DisjunctionDocIter`。
- Produces（无新公开签名；行为契约供 T6 电池验证）:
  ```rust
  // doc_iter.rs
  /// spec §5 档 2 查询时物化：df < threshold，成本有界（≤4095 doc ≈ 32 PFOR 块）
  pub(crate) fn materialize_roaring(seg: &SegmentReader, entry: &TermEntry, has_freqs: bool) -> io::Result<RoaringBitmap>;
  ```

  档规则（spec §5，per-segment 独立）：子句逐一 `roaring_bitmap` 探测 → 全无 bitmap → 档 3 既有 PFOR 路径**逐字节不动**；≥1 个有 → 无 bitmap 的子句 `materialize_roaring` 物化，全部走 roaring and/or 折叠，结果仍是 roaring，直接迭代（不物化成数组）。And 遇 absent term → `Ok(None)`（无命中，既有语义）；Or 跳过 absent term（既有语义）。`needs_freq == true` 时整支不进 roaring（bitmap 无 freq）。

### Steps

- [ ] **Step 5.1: 写失败测试** — 追加到 `crates/core/src/search/mod.rs` 的 `mod tests`（`materialize_roaring` 与档判定尚不存在，变体断言编译失败即失败测试成立）：

  ```rust
      fn tier_schema() -> Schema {
          let mut s = Schema::new();
          s.add(FieldSpec::text("message"));
          s
      }

      /// 20 docs: ha df=12 (0..12), hb df=12 (4..16), lo df=2 {0,1},
      /// lo2 df=2 {1,2}; threshold 4 puts ha/hb in the sidecar, lo/lo2
      /// stay postings-only. u{i} keeps every doc's message non-empty.
      fn write_tier_corpus(root: &std::path::Path) {
          let mut cfg = IndexWriterConfig::default();
          cfg.bitmap = true;
          cfg.bitmap_threshold = 4;
          let mut w = IndexWriter::create(root, tier_schema(), cfg).unwrap();
          for i in 0..20 {
              let mut msg = String::new();
              if i < 12 {
                  msg.push_str("ha ");
              }
              if (4..16).contains(&i) {
                  msg.push_str("hb ");
              }
              if i < 2 {
                  msg.push_str("lo ");
              }
              if (1..3).contains(&i) {
                  msg.push_str("lo2 ");
              }
              msg.push_str(&format!("u{i}"));
              let mut d = Document::new();
              d.add("message", FieldValue::Text(msg));
              w.add_document(d).unwrap();
          }
          w.commit().unwrap();
          drop(w);
      }

      fn variant_name(it: &Option<SegmentDocIter>) -> &'static str {
          match it {
              None => "None",
              Some(SegmentDocIter::Docs(_)) => "Docs",
              Some(SegmentDocIter::Freqs(_)) => "Freqs",
              Some(SegmentDocIter::All(_)) => "All",
              Some(SegmentDocIter::And(_)) => "And",
              Some(SegmentDocIter::Or(_)) => "Or",
              Some(SegmentDocIter::Bitset(_)) => "Bitset",
              Some(SegmentDocIter::Phrase(_)) => "Phrase",
              Some(SegmentDocIter::Roaring(_)) => "Roaring",
          }
      }

      /// spec §8 对拍：同一查询 bitmap on/off 逐位一致；并经由
      /// SegmentDocIter 变体断言执行档（roaring=true 期望档 1/2 的
      /// Roaring 迭代器，false 期望档 3 的 And/Or postings 迭代器；
      /// absent-term AND 短路为 None，两档都允许）。
      fn assert_tier_and_equal(dir: &FSDirectory, q: &Query, expect_docs: &[i32], roaring: bool) {
          let mut on = Searcher::open(dir).unwrap();
          let mut off = Searcher::open_with_bitmap(dir, false).unwrap();
          let (on_total, on_docs) = on.top_docs(q, 100).unwrap();
          let (off_total, off_docs) = off.top_docs(q, 100).unwrap();
          assert_eq!(
              (on_total, &on_docs),
              (off_total, &off_docs),
              "bitmap on/off divergence for {q:?}"
          );
          assert_eq!(&on_docs, expect_docs, "doc set for {q:?}");
          assert_eq!(on.count(q).unwrap(), expect_docs.len() as u64, "count for {q:?}");

          let mut reader = Reader::open(dir).unwrap();
          let (_, seg) = reader.leaves().next().unwrap();
          let it = q.segment_iterator(seg, false).unwrap();
          match (roaring, &it) {
              (true, Some(SegmentDocIter::Roaring(_))) | (true, None) => {}
              (false, Some(SegmentDocIter::And(_) | SegmentDocIter::Or(_))) | (false, None) => {}
              _ => panic!("tier mismatch for {q:?}: got {}", variant_name(&it)),
          }
      }

      #[test]
      fn and_or_three_tier_execution_matches_postings() {
          let root = temp_dir("rbmtier");
          write_tier_corpus(&root);
          let dir = FSDirectory::open(&root).unwrap();

          // tier 1: all clauses bitmapped -> pure roaring container ops
          assert_tier_and_equal(&dir, &Query::and("message", &["ha", "hb"]), &(4..12).collect::<Vec<i32>>(), true);
          assert_tier_and_equal(&dir, &Query::or("message", &["ha", "hb"]), &(0..16).collect::<Vec<i32>>(), true);
          // tier 2: mixed -> the low-df clause is materialized at query time
          assert_tier_and_equal(&dir, &Query::and("message", &["ha", "lo"]), &[0, 1], true);
          assert_tier_and_equal(&dir, &Query::or("message", &["ha", "lo"]), &(0..12).collect::<Vec<i32>>(), true);
          // tier 3: no clause bitmapped -> the M1 PFOR path, untouched
          assert_tier_and_equal(&dir, &Query::and("message", &["lo", "lo2"]), &[1], false);
          assert_tier_and_equal(&dir, &Query::or("message", &["lo", "lo2"]), &[0, 1, 2], false);
          // absent terms: AND has no hits, OR degrades to the present term
          assert_tier_and_equal(&dir, &Query::and("message", &["ha", "absent"]), &[], true);
          assert_tier_and_equal(&dir, &Query::or("message", &["ha", "absent"]), &(0..12).collect::<Vec<i32>>(), true);
          fs::remove_dir_all(&root).unwrap();
      }
  ```

- [ ] **Step 5.2: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core three_tier 2>&1 | tail -8
  ... panicked at 'tier mismatch for And { field: "message", terms: [[104, 97], [104, 98]] }: got And'
  （档判定尚未实现，全走既有 And/Or）
  ```

- [ ] **Step 5.3: 最小实现** — 两处修改：

  (a) `crates/core/src/search/doc_iter.rs`：`materialize_roaring` 追加在 `RoaringDocIter` 的 `impl DocIter` 之后：

  ```rust
  /// Query-time materialization of one clause into an in-memory roaring
  /// bitmap (spec M3 §5 tier 2): full ascending postings scan with a
  /// no-freq enum — the same scan discipline as multi_term::materialize
  /// (M2, multi_term.rs:275-303), kept same-source deliberately. The
  /// materialized clause's df is below the bitmap threshold, so the cost
  /// is bounded (≤4095 docs ≈ 32 PFOR blocks, spec §5).
  pub(crate) fn materialize_roaring(
      seg: &SegmentReader,
      entry: &TermEntry,
      has_freqs: bool,
  ) -> io::Result<RoaringBitmap> {
      let mut docs = Vec::with_capacity(entry.doc_freq as usize);
      if has_freqs {
          let mut en = seg.docs_freqs_enum(entry, false)?;
          loop {
              let d = en.next_doc()?;
              if d == NO_MORE_DOCS {
                  break;
              }
              docs.push(d as u32);
          }
      } else {
          let mut en = seg.docs_enum(entry)?;
          loop {
              let d = en.next_doc()?;
              if d == NO_MORE_DOCS {
                  break;
              }
              docs.push(d as u32);
          }
      }
      Ok(RoaringBitmap::from_sorted_docs(&docs))
  }
  ```

  (b) `crates/core/src/search/query.rs`：use 块的 doc_iter 列表加 `materialize_roaring`；And 分支（terms.len() >= 2 的部分）整体替换为：

  ```rust
              Query::And { field, terms } => {
                  if terms.len() < 2 {
                      return if let Some(t) = terms.first() {
                          Query::Term {
                              field: field.clone(),
                              term: t.clone(),
                          }
                          .segment_iterator(seg, needs_freq)
                      } else {
                          Ok(None)
                      };
                  }
                  // M3 §5: the per-clause bitmap probe decides the tier.
                  // Probing is skipped for freq consumers (the bitmap
                  // carries no freqs, spec §2); with no sidecar,
                  // roaring_bitmap returns None immediately.
                  let mut clauses: Vec<(u32, codec_lucene9::terms_read::TermEntry, Option<RoaringBitmap>)> =
                      Vec::new();
                  let mut has_freqs = false;
                  let mut any_bitmap = false;
                  for t in terms {
                      let Some((hf, entry)) = seg.seek_term(field, t)? else {
                          return Ok(None); // absent term: AND has no hits
                      };
                      has_freqs = hf;
                      let bm = if needs_freq {
                          None
                      } else {
                          seg.roaring_bitmap(field, t, entry.doc_freq)?
                      };
                      any_bitmap |= bm.is_some();
                      clauses.push((entry.doc_freq, entry, bm));
                  }
                  if !any_bitmap {
                      // tier 3 (spec §5): the M1 PFOR conjunction, untouched
                      let mut entries: Vec<(u32, codec_lucene9::terms_read::TermEntry)> =
                          clauses.into_iter().map(|(df, e, _)| (df, e)).collect();
                      entries.sort_by_key(|(df, _)| *df);
                      return Ok(Some(SegmentDocIter::And(ConjunctionDocIter::new(
                          seg, field, &entries, needs_freq,
                      )?)));
                  }
                  // tiers 1/2: one roaring engine; clauses without a sidecar
                  // bitmap are materialized at query time (spec §5 tier 2,
                  // df < threshold bounded)
                  let mut bms: Vec<RoaringBitmap> = Vec::with_capacity(clauses.len());
                  for (_, entry, bm) in clauses {
                      bms.push(match bm {
                          Some(b) => b,
                          None => materialize_roaring(seg, &entry, has_freqs)?,
                      });
                  }
                  bms.sort_by_key(|b| b.cardinality()); // intersect smallest first
                  let mut acc = bms.remove(0);
                  for b in &bms {
                      acc = acc.and(b);
                  }
                  Ok(Some(SegmentDocIter::Roaring(RoaringDocIter::new(acc))))
              }
  ```

  Or 分支（terms.len() >= 2 的部分）整体替换为：

  ```rust
              Query::Or { field, terms } => {
                  if terms.len() < 2 {
                      return if let Some(t) = terms.first() {
                          Query::Term {
                              field: field.clone(),
                              term: t.clone(),
                          }
                          .segment_iterator(seg, needs_freq)
                      } else {
                          Ok(None)
                      };
                  }
                  let mut clauses: Vec<(u32, codec_lucene9::terms_read::TermEntry, Option<RoaringBitmap>)> =
                      Vec::new();
                  let mut has_freqs = false;
                  let mut any_bitmap = false;
                  for t in terms {
                      if let Some((hf, entry)) = seg.seek_term(field, t)? {
                          has_freqs = hf;
                          let bm = if needs_freq {
                              None
                          } else {
                              seg.roaring_bitmap(field, t, entry.doc_freq)?
                          };
                          any_bitmap |= bm.is_some();
                          clauses.push((entry.doc_freq, entry, bm));
                      }
                  }
                  if clauses.is_empty() {
                      return Ok(None);
                  }
                  if !any_bitmap {
                      // tier 3 (spec §5): the M1 PFOR disjunction, untouched
                      let mut entries: Vec<(u32, codec_lucene9::terms_read::TermEntry)> =
                          clauses.into_iter().map(|(df, e, _)| (df, e)).collect();
                      entries.sort_by_key(|(df, _)| *df);
                      return Ok(Some(SegmentDocIter::Or(DisjunctionDocIter::new(
                          seg, field, &entries, needs_freq,
                      )?)));
                  }
                  // tiers 1/2 (spec §5): materialize missing clauses, union
                  // via roaring container ops, iterate the bitmap directly
                  let mut bms: Vec<RoaringBitmap> = Vec::with_capacity(clauses.len());
                  for (_, entry, bm) in clauses {
                      bms.push(match bm {
                          Some(b) => b,
                          None => materialize_roaring(seg, &entry, has_freqs)?,
                      });
                  }
                  let mut acc = bms.remove(0);
                  for b in &bms {
                      acc = acc.or(b);
                  }
                  Ok(Some(SegmentDocIter::Roaring(RoaringDocIter::new(acc))))
              }
  ```

- [ ] **Step 5.4: 跑测试确认通过（三档测试 + 全量回归）**

  ```
  $ cargo test -p rustlucene-core three_tier 2>&1 | tail -3
  test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p rustlucene-core 2>&1 | tail -3
  test result: ok. 46 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 5.5: 提交**

  ```
  git add crates/core/src/search/doc_iter.rs crates/core/src/search/query.rs crates/core/src/search/mod.rs
  git commit -m "feat: And/Or three-tier roaring execution + query-time materialization (M3 §5)"
  ```

---

## Task 6: diff 电池 `--bitmap` 变体 + CLI 接线 + bench 三路报告

**Files:**
- Modify: `crates/core/src/bin/rustlucene-cli.rs`（logwrite `--bitmap`/`--bitmap-threshold`、searchdump/searchbench `--no-bitmap`、usage 文本）
- Modify: `interop/verify-log.sh`（变体参数改名 + `--bitmap` 分支：sidecar 存在性、CheckIndex 前后 sha256、on/off searchdump diff；verify-search.sh 旗标过滤）
- Modify: `Makefile`（log-test 加第五变体）
- Create: `bench/run-bitmap-bench.sh`
- Test: 端到端即测试（`make log-test` 五变体 + 三路 counts diff）

**Interfaces:**
- Consumes: T3 的 `IndexWriterConfig::{bitmap, bitmap_threshold}`；T4 的 `Searcher::open_with_bitmap`；T5 的三档执行。
- Produces:
  ```
  rustlucene-cli logwrite <indexDir> <numDocs> <seed> [--positions] [--sparse] [--bigdict] [--bitmap] [--bitmap-threshold N]
  rustlucene-cli searchdump <indexDir> <numDocs> <seed> [--positions] [--no-bitmap]
  rustlucene-cli searchbench <indexDir> <field> [...] [--load-queries FILE] [--no-bitmap]
  interop/verify-log.sh [numDocs] [seed] [--positions|--sparse|--bigdict|--bitmap]
  ```
  bench 数据/报告：`.superpowers/sdd/m3-{write-on,write-off,disk}.out`、`.superpowers/sdd/m3-q.txt`、`.superpowers/sdd/m3-bench-{roaring,pfor,java}.out`、`.superpowers/sdd/m3-counts-*`、`.superpowers/sdd/m3-bitmap-bench-report.md`（gitignored，不进 git）。

### Steps

- [ ] **Step 6.1: CLI 接线** — `crates/core/src/bin/rustlucene-cli.rs` 五处修改：

  (a) `logwrite` 函数签名加两个参数、config 用可变实例（完整替换现有函数）：

  ```rust
  /// Single-writer log-schema indexing (the interop counterpart of JavaLogBench).
  /// `bitmap` builds roaring sidecars for terms with df >= bitmap_threshold
  /// (M3 spec §3; CLI flags --bitmap / --bitmap-threshold N).
  #[allow(clippy::too_many_arguments)]
  fn logwrite(
      index_dir: &Path,
      num_docs: u32,
      seed: u64,
      positions: bool,
      sparse: bool,
      bigdict: bool,
      bitmap: bool,
      bitmap_threshold: u32,
  ) -> std::io::Result<()> {
      let vocab = vocab();
      let mut config = IndexWriterConfig::default();
      config.bitmap = bitmap;
      config.bitmap_threshold = bitmap_threshold;
      let mut w = IndexWriter::create(index_dir, log_schema(positions, bigdict), config)?;
      let mut rng = XorShift::new(seed);
      let t0 = Instant::now();
      for doc_id in 0..num_docs {
          w.add_document(gen_log_document(
              &mut rng,
              &vocab,
              doc_id as u64,
              sparse,
              bigdict,
          ))?;
      }
      w.commit()?;
      let ms = t0.elapsed().as_millis().max(1);
      println!(
          "WROTE docs={num_docs} elapsed_ms={ms} docs_per_sec={:.0}",
          num_docs as f64 * 1000.0 / ms as f64
      );
      Ok(())
  }
  ```

  (b) main 的 `"logwrite"` 分支改为：

  ```rust
          "logwrite" => {
              if args.len() < 5 {
                  usage();
              }
              let positions = args[5..].iter().any(|a| a == "--positions");
              let sparse = args[5..].iter().any(|a| a == "--sparse");
              let bigdict = args[5..].iter().any(|a| a == "--bigdict");
              let bitmap = args[5..].iter().any(|a| a == "--bitmap");
              let bitmap_threshold = args[5..]
                  .iter()
                  .position(|a| a == "--bitmap-threshold")
                  .map(|i| args[5 + i + 1].parse::<u32>().unwrap_or_else(|_| usage()))
                  .unwrap_or(codec_lucene9::roaring::DEFAULT_BITMAP_THRESHOLD);
              logwrite(
                  Path::new(&args[2]),
                  args[3].parse().unwrap(),
                  args[4].parse().unwrap(),
                  positions,
                  sparse,
                  bigdict,
                  bitmap,
                  bitmap_threshold,
              )
          }
  ```

  (c) `searchdump` 函数签名加 `bitmap: bool`（放在 `positions: bool` 之后），函数体首两行改为：

  ```rust
      let dir = FSDirectory::open(index_dir)?;
      let mut searcher = Searcher::open_with_bitmap(&dir, bitmap)?;
  ```

  main 的 `"searchdump"` 分支改为：

  ```rust
          "searchdump" => {
              if args.len() < 5 {
                  usage();
              }
              let positions = args[5..].iter().any(|a| a == "--positions");
              let bitmap = !args[5..].iter().any(|a| a == "--no-bitmap");
              searchdump(
                  Path::new(&args[2]),
                  args[3].parse().unwrap(),
                  args[4].parse().unwrap(),
                  positions,
                  bitmap,
              )
          }
  ```

  (d) `searchbench` 函数签名加 `bitmap: bool`（放在 `load_queries: Option<String>` 之后），函数体 Searcher 打开行改为 `let mut searcher = Searcher::open_with_bitmap(&dir, bitmap)?;`；main 的 `"searchbench"` 分支：声明区加 `let mut bitmap = true;`，while 解析循环的 match 加一支：

  ```rust
                      "--no-bitmap" => {
                          bitmap = false;
                          i += 1;
                      }
  ```

  调用点改为：

  ```rust
              searchbench(
                  Path::new(&args[2]),
                  &args[3],
                  warmup,
                  iter,
                  tasks,
                  seed,
                  load_queries,
                  bitmap,
              )
  ```

  (e) `usage()` 三行改为：

  ```rust
      eprintln!("  rustlucene-cli logwrite <indexDir> <numDocs> <seed> [--positions] [--sparse] [--bigdict] [--bitmap] [--bitmap-threshold N]");
      eprintln!("  rustlucene-cli searchdump <indexDir> <numDocs> <seed> [--positions] [--no-bitmap]");
      eprintln!("  rustlucene-cli searchbench <indexDir> <field> [--warmup N] [--iter N] [--tasks N] [--seed S] [--load-queries FILE] [--no-bitmap]");
  ```

- [ ] **Step 6.2: `interop/verify-log.sh` 改造** — 全文件替换为（要点：变体参数改名 VARIANT；传给 verify-search.sh 的旗标过滤为仅 `--positions`；`--bitmap` 分支做 sidecar 存在性检查、CheckIndex 前后 sha256 比对、bitmap on/off searchdump diff；JavaLogBench 对未知旗标虽静默忽略——`JavaLogBench.java:73-76`——仍显式过滤掉 `--bitmap`）：

  ```bash
  #!/usr/bin/env bash
  # M2 interop: Rust writes a log-schema index -> Java CheckIndex + query dump;
  # Java writes the same corpus with stock Lucene -> CheckIndex + query dump;
  # the two dumps must be identical.
  # M3: with --bitmap the Rust logwrite also emits roaring sidecar files
  # (rbm<seg>.bin, spec §4a); the variant additionally checks that the
  # sidecars are present, that Java CheckIndex leaves them byte-identical
  # (§4a.3/5), and that bitmap on/off searchdumps are identical (§8).
  # Usage: interop/verify-log.sh [numDocs] [seed] [--positions|--sparse|--bigdict|--bitmap]
  set -euo pipefail

  ROOT="$(cd "$(dirname "$0")/.." && pwd)"
  NUM_DOCS="${1:-200000}"
  SEED="${2:-42}"
  VARIANT="${3:-}"
  RUST_DIR=/tmp/rl-log-rust
  JAVA_DIR=/tmp/rl-log-java
  CP="$ROOT/interop/java/classes:$ROOT/interop/java/lib/lucene-core-9.12.3.jar:$ROOT/interop/java/lib/lucene-analysis-common-9.12.3.jar"

  rm -rf "$RUST_DIR" "$JAVA_DIR"
  mkdir -p "$RUST_DIR" "$JAVA_DIR"

  echo "== Rust: logwrite ($NUM_DOCS docs, seed $SEED $VARIANT)"
  cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
    logwrite "$RUST_DIR" "$NUM_DOCS" "$SEED" $VARIANT

  if [ "$VARIANT" = "--bitmap" ]; then
    echo "== Sidecar files present (rbm<seg>.bin, outside the .si files set)"
    ls "$RUST_DIR"/rbm_*.bin
    sha256sum "$RUST_DIR"/rbm_*.bin > /tmp/rl-rbm-before.sha256
  fi

  echo "== Java: JavaLogBench (same corpus)"
  JAVA_VARIANT="$VARIANT"
  [ "$VARIANT" = "--bitmap" ] && JAVA_VARIANT=""
  java -cp "$CP" JavaLogBench "$JAVA_DIR" "$NUM_DOCS" 1 "$SEED" $JAVA_VARIANT
  for side in "$RUST_DIR" "$JAVA_DIR"; do
    echo "== CheckIndex $side"
    java -cp "$CP" org.apache.lucene.index.CheckIndex "$side" 2>&1 \
      | grep -E "No problems|FAILED|error" || true
    java -cp "$CP" org.apache.lucene.index.CheckIndex "$side" > /dev/null 2>&1
  done

  if [ "$VARIANT" = "--bitmap" ]; then
    echo "== Sidecar survives Java CheckIndex byte-identical (§4a.3/5)"
    sha256sum -c /tmp/rl-rbm-before.sha256
  fi

  echo "== VerifyLogIndex: Rust vs Java dumps"
  EXPECT_POSITIONS=false
  [ "$VARIANT" = "--positions" ] && EXPECT_POSITIONS=true
  java -cp "$CP" VerifyLogIndex "$RUST_DIR" "$EXPECT_POSITIONS" > /tmp/rl-log-rust.out
  java -cp "$CP" VerifyLogIndex "$JAVA_DIR" "$EXPECT_POSITIONS" > /tmp/rl-log-java.out
  diff -u /tmp/rl-log-rust.out /tmp/rl-log-java.out
  cat /tmp/rl-log-rust.out

  echo "== Search diff: searchdump vs VerifySearchIndex"
  # --bitmap is Rust-private; verify-search.sh only understands --positions
  SEARCH_POSITIONS=""
  [ "$VARIANT" = "--positions" ] && SEARCH_POSITIONS="--positions"
  "$ROOT/interop/verify-search.sh" "$RUST_DIR" "$JAVA_DIR" "$NUM_DOCS" "$SEED" "$SEARCH_POSITIONS"

  if [ "$VARIANT" = "--bitmap" ]; then
    echo "== Bitmap on/off searchdump diff (spec §8: 逐位一致)"
    cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" --no-bitmap > /tmp/rl-search-nobitmap.out
    diff -u /tmp/rl-search-rust.out /tmp/rl-search-nobitmap.out
  fi

  echo "LOG_INTEROP_OK"
  ```

  （`interop/verify-search.sh` 不改——它只在第五参收到 `--positions` 或空。）

- [ ] **Step 6.3: `Makefile` 加第五变体** — `log-test` 目标改为（注释同步更新）：

  ```make
  # M2 log-schema interop: Rust logwrite vs JavaLogBench, CheckIndex + query diff;
  # M3 adds the --bitmap roaring-sidecar variant (sidecar survival + on/off diff)
  log-test: build java-classes
  	interop/verify-log.sh 200000 42
  	interop/verify-log.sh 200000 43 --positions
  	interop/verify-log.sh 200000 44 --sparse
  	interop/verify-log.sh 200000 45 --bigdict
  	interop/verify-log.sh 200000 46 --bitmap
  ```

- [ ] **Step 6.4: 编译 + 单变体烟测**

  ```
  $ cargo build --release 2>&1 | tail -1
      Finished `release` profile [optimized] target(s) in ...
  $ interop/verify-log.sh 200000 46 --bitmap 2>&1 | tail -12
  == Sidecar files present (rbm<seg>.bin, outside the .si files set)
  -rw-r--r-- ... /tmp/rl-log-rust/rbm_0.bin
  ...
  == CheckIndex /tmp/rl-log-rust
  No problems were detected with this index.
  == Sidecar survives Java CheckIndex byte-identical (§4a.3/5)
  /tmp/rl-log-rust/rbm_0.bin: OK
  ...
  == Bitmap on/off searchdump diff (spec §8: 逐位一致)
  （diff 无输出）
  SEARCH_INTEROP_OK
  LOG_INTEROP_OK
  ```

  排错提示：on/off diff 非空 → roaring 路径结果与 postings 不一致，回 T4/T5 语义测试；sha256  mismatch → CheckIndex 动了 sidecar（违反 §4a.3/5，回 T3 命名）；`ls` 找不到 rbm → logwrite 的 `--bitmap` 没生效（Step 6.1）。

- [ ] **Step 6.5: `make log-test` 五变体全绿（本任务验收门槛）**

  ```
  $ make log-test 2>&1 | grep -E "LOG_INTEROP_OK|No problems|FAILED|diff"
  （五段各一次 LOG_INTEROP_OK；每个 Rust/Java 索引各一次 "No problems"；无任何 diff 输出、无 FAILED）
  ```

  既有四变体（无 sidecar）全绿 = 读侧自动探测对无 sidecar 索引零影响（档 3 回归）；`--bitmap` 变体绿 = §4a 全契约 + §8 对拍。

- [ ] **Step 6.6: 提交（CLI + 电池）**

  ```
  git add crates/core/src/bin/rustlucene-cli.rs interop/verify-log.sh Makefile
  git commit -m "feat: log-test --bitmap variant + CLI bitmap switches (M3 §8)"
  ```

- [ ] **Step 6.7: bench 脚本 + 执行** — 新建 `bench/run-bitmap-bench.sh`（chmod +x）：

  ```bash
  #!/usr/bin/env bash
  # M3 roaring sidecar bench (spec §8): three-way comparison on the SAME
  # bitmap index — Rust roaring (default) vs Rust PFOR (--no-bitmap) vs
  # Java Lucene 9.12.3 (--no-cache) — plus write-throughput loss and disk
  # overhead. 1M docs so message terms cross df 4096 (see 关键设计事实 12).
  # Data + report land in .superpowers/sdd/ (gitignored).
  # Usage: bench/run-bitmap-bench.sh [numDocs] [seed] [tasks]
  set -euo pipefail

  ROOT="$(cd "$(dirname "$0")/.." && pwd)"
  DOCS="${1:-1000000}"
  SEED="${2:-42}"
  TASKS="${3:-100}"
  OUT="$ROOT/.superpowers/sdd"
  CP="$ROOT/interop/java/classes:$ROOT/interop/java/lib/lucene-core-9.12.3.jar:$ROOT/interop/java/lib/lucene-analysis-common-9.12.3.jar"
  CLI="$ROOT/target/release/rustlucene-cli"
  IDX_ON=/tmp/bench-rbm-on
  IDX_OFF=/tmp/bench-rbm-off

  cd "$ROOT"
  cargo build -q --release -p rustlucene-core
  mkdir -p "$OUT"

  echo "== [1/4] write throughput + disk overhead (docs=$DOCS seed=$SEED)"
  rm -rf "$IDX_ON" "$IDX_OFF"
  mkdir -p "$IDX_ON" "$IDX_OFF"
  "$CLI" logwrite "$IDX_OFF" "$DOCS" "$SEED" | tee "$OUT/m3-write-off.out"
  "$CLI" logwrite "$IDX_ON" "$DOCS" "$SEED" --bitmap | tee "$OUT/m3-write-on.out"
  du -sb "$IDX_ON" "$IDX_OFF" | tee "$OUT/m3-disk.out"
  ls -l "$IDX_ON"/rbm_*.bin | tee -a "$OUT/m3-disk.out"

  echo "== [2/4] dump query set from the bitmap index (Java SearchBench)"
  java -cp "$CP" SearchBench "$IDX_ON" message \
    --dump-queries "$OUT/m3-q.txt" --tasks "$TASKS" --seed "$SEED" 2>/dev/null
  wc -l "$OUT/m3-q.txt"

  echo "== [3/4] three-way search bench (--no-cache 口径)"
  "$CLI" searchbench "$IDX_ON" message --load-queries "$OUT/m3-q.txt" \
    --warmup 10 --iter 30 --seed "$SEED" \
    > "$OUT/m3-bench-roaring.out" 2> "$OUT/m3-counts-roaring.txt"
  "$CLI" searchbench "$IDX_ON" message --load-queries "$OUT/m3-q.txt" \
    --warmup 10 --iter 30 --seed "$SEED" --no-bitmap \
    > "$OUT/m3-bench-pfor.out" 2> "$OUT/m3-counts-pfor.txt"
  java -cp "$CP" SearchBench "$IDX_ON" message \
    --load-queries "$OUT/m3-q.txt" --warmup 10 --iter 30 --seed "$SEED" --no-cache \
    > "$OUT/m3-bench-java.out" 2> "$OUT/m3-counts-java.txt"

  echo "== [4/4] correctness: per-query hit counts identical across the three"
  PAT='^(term=|and t1=|or t1=|iterm=|prefix=|wildcard=|terms=|phrase t1=)'
  grep -E "$PAT" "$OUT/m3-counts-roaring.txt" | sort > "$OUT/m3-counts-roaring.sorted"
  grep -E "$PAT" "$OUT/m3-counts-pfor.txt" | sort > "$OUT/m3-counts-pfor.sorted"
  grep -E "$PAT" "$OUT/m3-counts-java.txt" | sort > "$OUT/m3-counts-java.sorted"
  diff "$OUT/m3-counts-roaring.sorted" "$OUT/m3-counts-pfor.sorted"
  diff "$OUT/m3-counts-roaring.sorted" "$OUT/m3-counts-java.sorted"
  echo "BITMAP_BENCH_OK"
  ```

  执行：

  ```
  $ chmod +x bench/run-bitmap-bench.sh && bench/run-bitmap-bench.sh 1000000 42 100 2>&1 | tail -8
  ...
  == [4/4] correctness: per-query hit counts identical across the three
  BITMAP_BENCH_OK
  ```

  （counts 三路 diff 为空 = spec §8 "同一查询电池 bitmap on/off 结果逐位一致" + Java 终验在 bench 语料上成立。）

- [ ] **Step 6.8: bench 报告落盘** — 写 `.superpowers/sdd/m3-bitmap-bench-report.md`（gitignored，不进 git），内容必须含：
  - 口径行：`--no-cache`（Java 旗标；Rust 本无 query cache）、1000000 docs、seed 42、`--warmup 10 --iter 30`、tasks 100、单线程 logwrite。
  - 写侧：从 `m3-write-{on,off}.out` 取 `docs_per_sec` 双侧数值与 bitmap 开销百分比；从 `m3-disk.out` 取两索引总字节、rbm 文件总字节与占比。
  - 读侧三路表：按 `m3-bench-{roaring,pfor,java}.out` 的分组行（query_type × freq），列出 qps 与 p50/p90/p99 三方数值，重点给出 **and/high、or/high、iterm/high** 三组的 roaring vs PFOR 提速比与 roaring vs Java 比。
  - 结论一句：对照 spec §8 预期（高 df AND 提速一个数量级；count 早已 O(1) 无变化空间）判定达标与否；未达标的组只记录不追责。
  - AVX2 有效性（spec §6 "bench 数据门槛"）：`RL_SIMD=0` 重跑一次 Rust roaring searchbench 取同组 qps，与默认（AVX2 分发）对比一句话。

- [ ] **Step 6.9: 提交（bench 脚本；`.superpowers/` 已被 gitignore）**

  ```
  git add bench/run-bitmap-bench.sh
  git commit -m "bench: M3 roaring vs PFOR vs Java three-way bench script"
  ```

---

## 收尾检查单（全部 Task 完成后逐项核对）

- [ ] `cargo test -p codec-lucene9` 全绿（T1/T2/T3 共 13 个新测试）
- [ ] `cargo test -p rustlucene-core` 全绿（T3/T4/T5 共 7 个新测试）
- [ ] `RL_SIMD=0 cargo test -p codec-lucene9 roaring` 全绿（kill switch 标量路径）
- [ ] `make log-test` 五变体全绿，CheckIndex 全部 "No problems"
- [ ] `grep -rn "unsafe" crates/core/src/` 无新增（`#![forbid(unsafe_code)]` 兜底）；`#[allow(unsafe_code)]` 仍只出现在 `postings_ll/simd.rs` 与 `roaring/simd.rs`
- [ ] `Cargo.toml` 两个 crate 均无新依赖
- [ ] `.si` files 集合不含 rbm（T3 core 测试覆盖）；`make log-test` 的 `--bitmap` 变体 sha256 检查证明 CheckIndex 后 sidecar 字节不变
- [ ] bench 报告在 `.superpowers/sdd/m3-bitmap-bench-report.md`，含写侧吞吐/磁盘增量/三路读侧数值
