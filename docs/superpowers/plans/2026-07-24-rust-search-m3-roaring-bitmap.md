# M3 高 df term 的 Roaring bitmap（.doc 内联）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 M2（multi-term + phrase）之上新增 M3：segment flush 时对 df ≥ 4096 的 term 构建三容器 RoaringBitmap（array <4096 / bitset ≥4096 / runOptimize 后 run），**内联写入 `.doc` 流**——每个 term 的 postings 之前、capture `docStartFP` 之前写 `[bitmap 头+payload+crc32][len: 4B LE]`，读侧由现有 FST output 的 `docStartFP` 定位（`docStartFP-4-len` 处），不改 FST output schema、不新增任何文件。读侧 Term/And/Or 按 spec §5 三档规则走 roaring 容器运算（档 1 全 bitmap、档 2 混合查询时物化、档 3 纯低 df 走既有 PFOR 不动）。自研容器子集（不引 roaring crate），标量先行、AVX2 等价追加；`make log-test` 加 `--bitmap` 第五变体全绿 + bitmap on/off searchdump diff 为空 + Java CheckIndex "No problems" + **Java forceMerge 后再对拍**收尾。对应已批准 spec `docs/superpowers/specs/2026-07-23-rust-search-m3-roaring-bitmap-design.md` 的全部范围（2026-07-24 内联修订版；旧 sidecar 契约整体作废，不再引用）。

**Architecture:** 延续方案 C（算法语义照抄 9.12.3、对象结构 Rust 化）。roaring 容器库（`crates/codec-lucene9/src/roaring.rs` + `roaring/simd.rs`）与存储方案无关；codec 同时拥有内联块 FORMAT：写侧 `PostingsWriter::write_term` 在 `doc_start_fp` capture 之前把 bitmap 块写进同一个 `ChecksumIndexOutput`（`.doc` footer CRC 因此自然覆盖 bitmap 字节，`CodecUtil.java:402-413`），读侧 `PostingsReader::inline_bitmap` 经 `docStartFP-4` 取 len、回退 len 字节取块，四重校验（len 有界 → magic → df==termState.df → crc32）任一失败**静默落档 postings**。core 只拥有执行（三档规则、`RoaringDocIter`、查询时物化、And/Or 粘合），core 无任何 unsafe。配置链：CLI/电池 → `IndexWriterConfig{bitmap, bitmap_threshold}` → `SegmentBuilder::with_bitmap` → `PostingsWriter::set_bitmap_threshold`。读侧自动探测（bitmap 块在不在由四重校验回答，不依赖任何元数据）；Java merge 经 PostingsEnum 重编码 → 产物天然无 bitmap、per-segment 自然落档（Rust 无 merge，无 GC 问题）。验证三层不变：容器/内联块 round-trip 单测（codec）→ 三档语义测试（core `search/mod.rs`）→ Java diff 终验 + forceMerge 实证 + 三路 bench 报告（`--no-cache`）。

**Tech Stack:** Rust（codec crate edition 2024、core crate edition 2021；codec `#![deny(unsafe_code)]` + 仅 `postings_ll/simd.rs`、`roaring/simd.rs` 两个模块级 `#[allow(unsafe_code)]`，core `#![forbid(unsafe_code)]`，统一 `io::Result`）；不新增依赖（crc32fast 已是 codec 依赖，bitmap crc32 与 footer CRC 同算法）；Java 9.12.3（`interop/java/lib/lucene-core-9.12.3.jar`）做 diff 基准、CheckIndex 与 forceMerge 实证；格式语义以 `reference/lucene-9.12.3/` 源码为准。

## Global Constraints

（摘自 spec §2/§3/§4/§4a/§5/§6/§7 与既有项目惯例，逐字或就近转述；所有 Task 共同遵守）

- **postings 主格式不动、不新增任何文件**（spec §2）：bitmap 不带 freq/positions，postings 永远保留，bitmap 只是附加字节，内联在 `.doc` 流内每个 term 的 postings 之前。Java 读写零感知，写侧 interop 与 diff 电池不受影响。
- **内联布局不变量**（spec §4/§4a，绑定）：bitmap 块 `[magic(4B LE) + version(1B) + df(vInt) + cardinality(vInt) + payload + crc32(4B LE)]` + 4B LE len（= 头+payload+crc32 总字节）写于 `docStartFP` capture **之前**；`docStartFP` 仍指向 postings 起点，**FST output schema 不动**；读侧四重校验（len 有界按 maxDoc 推算 + 块不越入 header 区 → magic → `df == termState.df` → crc32），任一失败**静默落档 postings**，查询永不报错；Java merge 产物自然无 bitmap；CFS/addIndexes 字节拷贝 bitmap 存活；Rust 无 merge，无 GC。
- **df 阈值 4096**：`DEFAULT_BITMAP_THRESHOLD = 4096`（对齐 level-1 skip 粒度 32×128，spec §3），`--bitmap-threshold N` 可调。**`--bitmap` 默认 off**（实验期开关）；读侧自动探测，CLI `--no-bitmap` 只用于同一索引上的 bench A/B。
- **不新增 crate 依赖**（spec §7：自研容器子集 ~500–700 行含测试；现有依赖 crc32fast/lz4/rand/serde_json 不动）。
- **unsafe 边界**：codec crate 保持 `#![deny(unsafe_code)]`，模块级 `#[allow(unsafe_code)]` 只出现在既有 `crates/codec-lucene9/src/postings_ll/simd.rs` 与新增 `crates/codec-lucene9/src/roaring/simd.rs`（分发/安全论证模式照搬前者）；core crate 保持 `#![forbid(unsafe_code)]`——core 任何新代码不得引入 unsafe。
- **commit message 前缀**：`feat:` / `test:` / `bench:` / `docs:`（沿用 git log 现有风格）。
- **验收门槛**：`make log-test` 全部变体绿（既有四变体 seed 42 默认 / 43 `--positions` / 44 `--sparse` / 45 `--bigdict`，加新第五变体 seed 46 `--bitmap`）+ bitmap on/off searchdump diff 为空 + Java CheckIndex 对带内联 bitmap 的索引 "No problems" + **Java forceMerge 该索引后 Java↔Rust 再对拍**（merge 产物无 bitmap 自然落档、结果不变，spec §8）。
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

1. **构建钩子点**（spec §4）：flush 时 term 的 docs 全量切片本就在内存——`crates/codec-lucene9/src/postings.rs:338` `PostingsWriter::write_term` 收 `docs: &[u32]`（`postings.rs:346` `debug_assert` 升序）；`postings.rs:354` `let doc_start_fp = self.doc_out.file_pointer();` 在 `.pos` 写出（:349-351）之后、`.doc` postings 写出（:357）之前。**bitmap 钩子就在 capture 之前**：`df >= threshold` 时 `write_inline_bitmap(docs)` 往同一个 `doc_out` 写 `[头+payload+crc32][len]`，O(df) CPU、零额外 IO。core 侧对应 `crates/core/src/segment_builder.rs:161-169` 的 term 循环调用点。
2. **缝隙字节对 Java 隐形**（§4a.1）：postings reader 是纯 seek 式——读 term 永远 `docIn.seek(termState.docStartFP)` 起手（`Lucene912PostingsReader.java:436,809`），skip 导航也 seek 到算好的 fp（`level1DocEndFP`/`skip0EndFP`/`blockEndFP`，:526,559,997），读完 df 个 doc 即停；**从不顺序扫描，从不假设"term N+1 起点 == term N 结尾"**。bitmap 位于 docStartFP 之前的缝隙，永不被 Java 触及；Rust 自己的 `EnumCore` 同样纯 seek（`postings_read.rs:227` `c.doc_in.seek(entry.state.doc_start_fp)` + skip fp 自算），也不受缝隙影响。
3. **CRC 与 CheckIndex**（§4a.2/3）：footer CRC 覆盖全流（`CodecUtil.java:402-413`）——bitmap 在流内、footer 之前经同一 `ChecksumIndexOutput` 写入，checkIntegrity 照常通过；`.psm` 的 `docLen` 在 `postings.rs:440` 于 footer 前 capture，自然包含 bitmap 字节。CheckIndex 的 term 枚举与 postings 抽查全走 seek 路径、目录零新增文件 → "No problems were detected"（`CheckIndex.java:916-917`；电池实证）。
4. **FST output schema 不动 + len 尾缀定位**（spec §4 用户拍板）：BlockTree FST output 是 Java 端固定 schema（docStartFP/posStartFP/…），改它即破坏 Java 兼容；但无需改——`docStartFP` 本身就是定位器：读 `docStartFP-4` 处 4B LE len，回退 len 字节即 bitmap 区。写侧 `encode_term`（`postings.rs:87`）的 docStartFP delta 编码自然吸收增大的 fp（delta 变大仍是合法 VLong）。sidecar 方案的"有序 term 表 + 二分"整个不需要。
5. **四重校验与静默落档**（spec §4/§5）：无 bitmap 时 `docStartFP-4` 处是上一 term postings 的任意字节。校验链：`0 < len ≤ max_bitmap_len(maxDoc)`（按 maxDoc 推算上限）且块不越入 `.doc` header 区 → magic 匹配 → `df == termState.df`（cardinality 同值构建）→ crc32 匹配。误判概率实际为零，任一失败静默落档 postings。**读侧不设 df 门槛**：threshold 是写时配置（`--bitmap-threshold` 可调），读侧无从得知，探测交给四重校验回答（§5 的"df≥4096"描述的是写侧覆盖面）。惰性：非 bitmap term 的探测只花 4B（+15B 头）小读，payload 只有校验通过后才读——open 不读任何 bitmap（§5 惰性加载）。
6. **merge 安全**（§4a.4）：Java merge 经 PostingsEnum 逐 doc 重编码 postings → 新 segment 无 bitmap、自然落档（§5），无孤儿字节；CFS 打包 / addIndexes 是字节级拷贝 → bitmap 原样存活。Rust 自身无 merge，无 GC 问题。**注意**：Java 默认 merge 可能产出 CFS（compound file），Rust 读侧无 CFS 支持——电池的 ForceMerge 工具显式 `setUseCompoundFile(false)`（见事实 13）。
7. **三档钩子点**（spec §5）：`crates/core/src/search/query.rs` Term 分支（`query.rs:126-137`）、And 分支（:138-161）、Or 分支（:162-187）——档判定在每个分支 seek 完 term 之后、构造现有 iterator 之前；档 3（无任何子句有 bitmap）完全走既有 `ConjunctionDocIter`/`DisjunctionDocIter`（df 升序排序、lead 选择保持不动，M1 合取已调优）。规则 per-segment 独立生效（M1 既定 per-segment 执行）。
8. **needs_freq 纪律**：bitmap 不带 freq（spec §2/§5 "freq/freq_sum 永远走 postings"）——roaring 分支只在 `!needs_freq` 时进入。`Searcher::freq_sum` 的 Term 短路走 `total_term_freq`（`crates/core/src/search/searcher.rs:113-121`）本就不需要迭代；And/Or 的 `freq()` 本来就是 placeholder（`crates/core/src/search/doc_iter.rs:218-223,301-310`），唯一需要 freq 的 collector（FreqSumCollector）拒收 And/Or（`searcher.rs:106-112`）。
9. **count 路径**：Term 的 `Searcher::count` 已是 O(1)（`searcher.rs:61-69` 直接加 `TermEntry.doc_freq`），roaring 不改变它——bitmap 头的 df/cardinality 字段服务于四重校验而非 count 读取（spec §4 "count 查询只读头" 在本读路径下无额外消费者，如实记录）。收益在迭代路径（top_docs/search 驱动、searchbench iterm）与 And/Or 的 CountCollector 迭代。
10. **查询时物化同源**（spec §5 档 2）：M2 的 `multi_term::materialize`（`crates/core/src/search/multi_term.rs:275-303`）用 no-freq enum 升序全扫置位；M3 的 `materialize_roaring` 用**同一扫描纪律**（no-freq enum、升序收集）产出 `RoaringBitmap`。档 2 中被物化的子句 df < threshold → 成本 ≤4095 doc ≈ 32 个 PFOR 块，有界微秒级。
11. **容器语义**（spec §4，照 Roaring 论文 Chambi et al.）：doc 按高 16 位分桶；桶内 cardinality < 4096 → array 容器（u16 有序数组），≥ 4096 → bitset 容器（1024 u64 = 8KB）；构建后与每次 and/or 后 runOptimize（连续区间转 run 容器；按序列化体积判定：run 2+4R bytes vs array 2C bytes vs bitset 8192 bytes，小者胜）。交/并容器对分发（spec §5）：array∩array galloping、bitset∩bitset 字运算 + popcount、run∩run 双指针；Or 对偶。结果仍是 roaring，直接迭代，不物化成数组；空容器立即丢弃。
12. **AVX2 纪律**（spec §6）：先标量参考实现（容器交/并/popcount），AVX2 快路径作等价追加。x86 AVX2 **无 VPOPCNT**（那是 AVX-512）→ popcount 用 nibble-LUT + PSADBW（Muła 惯用法）。运行时分发 + `RL_SIMD=0` kill switch + OnceLock 缓存照抄 `crates/codec-lucene9/src/postings_ll/simd.rs:52-62`；对拍单测钉死"标量 vs SIMD 逐位相等"；bench 数据门槛见 T6。
13. **log 语料 df 量级**（估计，决定电池/bench 覆盖面）：`gen_message` 200 字节 ≈ 25–33 token/doc，词表 60×40+5 = 2405 → 200k 文档时 message term df ≈ 2–3k < 4096，`make log-test` 的 `--bitmap` 变体的 bitmap 只覆盖 level 字段（df ≈ 40k ≥ 4096；term level=X 的 top20 与 and level=INFO,WARN 走 roaring）。档 2 混合场景由 T5 单测覆盖（小阈值人造语料）。bench 用 1M 文档（message term df ≈ 13.7k ≥ 4096）做 message 高 df AND/OR/iterm 三路对比。
14. **电池接线**：`interop/verify-log.sh` 目前把第三参（变体旗标）同时透传给 logwrite、JavaLogBench 与 verify-search.sh。`--bitmap` 是 Rust 私有旗标：JavaLogBench 对未知旗标静默忽略（`interop/java/JavaLogBench.java:73-76`，仅 if-equals 判断、无 else 报错），但 verify-search.sh 会把它传给 searchdump → 在 verify-log.sh 内把传给 verify-search.sh 的旗标过滤为仅 `--positions`。`interop/java/` 现有工具**没有** forceMerge 入口（已逐一核实 16 个 .java 文件）→ 新增最小 `ForceMerge.java`（`Makefile:9-11` 的 `javac interop/java/*.java` 通配自动编译），并显式 `setUseCompoundFile(false)`（事实 6）。

---
## Task 1: roaring 容器库标量实现（`crates/codec-lucene9/src/roaring.rs` 新建）

三容器 + build/and/or/cardinality/迭代访问 + payload 序列化，全标量（spec §6 先标量）。AVX2 在 T2 追加。

**Files:**
- Create: `crates/codec-lucene9/src/roaring.rs`
- Modify: `crates/codec-lucene9/src/lib.rs`（`pub mod roaring;`）
- Test: `crates/codec-lucene9/src/roaring.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: `crate::io::{DataInput, DataOutput}`（`io.rs:519/779`）、`crate::codec_util::corrupt`（`codec_util.rs:69`）、测试用 `crate::io::{IndexInput, IndexOutput}`。
- Produces（T2 的 dispatch、T3 的 .doc 内联写/读、T4/T5 的 core 执行层依赖这些名字，不得改名）:
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

  /// spec §3: terms with df >= this threshold get an inline bitmap at flush
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

      /// Payload layout (consumed by the .doc inline block, spec §4): VInt num_containers,
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
      /// postings (spec §4/§5: 任一校验失败静默落档).
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


---

## Task 3: 写侧 .doc 内联 bitmap 产出（`postings.rs` 钩子 + 配置透传）

codec 拥有内联块 FORMAT：常量 + 写出；core 只做配置透传。round-trip 字节级测试在 codec 层闭环。

**Files:**
- Modify: `crates/codec-lucene9/src/postings.rs`（BITMAP 常量 + `bitmap_crc32` + `set_bitmap_threshold` + `write_inline_bitmap` + `write_term` 钩子）
- Modify: `crates/core/src/index_writer.rs`（`IndexWriterConfig` 两个字段 + `#[cfg(test)]` 模块）
- Modify: `crates/core/src/segment_builder.rs`（`with_bitmap` + finalize 透传 setter）
- Test: `crates/codec-lucene9/src/postings.rs` 与 `crates/core/src/index_writer.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T1 的 `RoaringBitmap::{from_sorted_docs, cardinality, serialize}`；`IndexOutput::in_memory` / `ChecksumIndexOutput`；既有 `crc32fast` 依赖。
- Produces（T4 的读侧校验、T6 的 CLI/电池依赖这些名字，不得改名）:
  ```rust
  // postings.rs
  pub(crate) const BITMAP_MAGIC: u32 = 0x614D4252; // "RBMa" as LE bytes
  pub(crate) const BITMAP_VERSION: u8 = 1;
  /// zlib CRC32（crc32fast，与 CodecUtil footer CRC 同算法）
  pub(crate) fn bitmap_crc32(bytes: &[u8]) -> u32;

  impl PostingsWriter {
      /// M3（spec §3/§4）：df >= threshold 的 term 在 docStartFP capture
      /// 之前内联写 bitmap 块。0 = 关闭（默认）。
      pub fn set_bitmap_threshold(&mut self, threshold: u32);
  }

  // core index_writer.rs
  pub struct IndexWriterConfig { .., pub bitmap: bool, pub bitmap_threshold: u32 } // default false / 4096
  // core segment_builder.rs
  impl SegmentBuilder {
      pub fn with_bitmap(self, enabled: bool, threshold: u32) -> Self;
  }
  ```

  内联块布局（spec §4，写于 `docStartFP` capture 之前）：`magic(4B LE) + version(1B) + df(vInt) + cardinality(vInt) + payload(roaring.rs serialize) + crc32(4B LE)`，随后 `len: 4B LE`（= 头+payload+crc32 总字节）。读侧 `docStartFP-4` 取 len、回退 len 字节即块起点。

### Steps

- [ ] **Step 3.1: 写 codec 失败测试** — `crates/codec-lucene9/src/postings.rs` 的 `mod tests` 追加（`set_bitmap_threshold` / `BITMAP_MAGIC` 尚不存在，编译失败即失败测试成立）。现有 `mod tests` 只有 `use super::*;`——在其 use 区补测试所需 import，然后追加测试：

  ```rust
      use crate::directory::FSDirectory;
      use crate::field_infos::{FieldInfo, FieldInfos, IndexOptions};
      use crate::io::IndexInput;
      use crate::postings_read::{NO_MORE_DOCS, PostingsReader};
      use crate::roaring::{Container, RoaringBitmap};
      use crate::terms_read::TermsDict;
      use std::fs;

      fn temp_dir_rbm(tag: &str) -> std::path::PathBuf {
          let dir = std::env::temp_dir()
              .join(format!("codec-lucene9-rbmwrite-{}-{}", tag, std::process::id()));
          let _ = fs::remove_dir_all(&dir);
          dir
      }

      fn indexed(name: &str, number: i32, opts: IndexOptions) -> FieldInfo {
          FieldInfo {
              name: name.to_string(),
              number,
              omit_norms: true,
              index_options: opts,
              ..FieldInfo::stored(name, number)
          }
      }

      /// spec §4 inline layout, byte by byte: [magic + version + df +
      /// cardinality + payload + crc32][len u32 LE] written immediately
      /// before docStartFP; postings decode is unaffected by the gap bytes.
      #[test]
      fn inline_bitmap_byte_layout_and_postings_intact() {
          let root = temp_dir_rbm("layout");
          let dir = FSDirectory::open(&root).unwrap();
          let id = [3u8; 16];
          let fis = FieldInfos::new(vec![indexed("kw", 0, IndexOptions::Docs)]);
          let mut w = PostingsWriter::new(&dir, "_0", &id).unwrap();
          w.set_bitmap_threshold(128);
          w.start_field(fis.by_name("kw").unwrap(), 6000).unwrap();
          let big: Vec<u32> = (0..200).collect();
          w.write_term(b"big", &big, &vec![1; 200], None).unwrap();
          w.write_term(b"tail", &[10, 20, 30], &[1, 1, 1], None).unwrap();
          w.finish_field().unwrap();
          w.finish().unwrap();
          fis.write(&dir, "_0", &id, "").unwrap();

          // locate "big" via the terms dict (FST output schema untouched:
          // doc_start_fp still points at the postings start)
          let mut dict = TermsDict::open(&dir, "_0", &id, &fis).unwrap();
          let e = dict
              .seek_exact(fis.by_name("kw").unwrap(), b"big")
              .unwrap()
              .unwrap();
          let fp = e.state.doc_start_fp;

          // len suffix at docStartFP-4, block at docStartFP-4-len
          let mut input = dir.open_input(&file_name("_0", "doc")).unwrap();
          input.seek(fp - 4).unwrap();
          let len = input.read_int().unwrap() as u32 as u64;
          let block_start = fp - 4 - len;
          let mut block = vec![0u8; len as usize];
          input.seek(block_start).unwrap();
          input.read_bytes(&mut block).unwrap();

          assert_eq!(&block[0..4], &BITMAP_MAGIC.to_le_bytes());
          assert_eq!(block[4], BITMAP_VERSION);
          let mut cur = IndexInput::in_memory(block.clone());
          cur.seek(5).unwrap();
          assert_eq!(cur.read_vint().unwrap(), 200, "df field");
          assert_eq!(cur.read_vint().unwrap(), 200, "cardinality field");
          let bm = RoaringBitmap::deserialize(&mut cur).unwrap();
          assert_eq!(cur.file_pointer() + 4, len, "payload must end 4B before len");
          let stored = u32::from_le_bytes(block[len as usize - 4..].try_into().unwrap());
          assert_eq!(bitmap_crc32(&block[..len as usize - 4]), stored);

          // bitmap content == input docs; 200 contiguous -> one Run container
          assert_eq!(bm.cardinality(), 200);
          let mut docs = Vec::new();
          for ci in 0..bm.num_containers() {
              let key = bm.container_key(ci) as u32;
              let c = bm.container_at(ci);
              for i in 0..c.cardinality() {
                  docs.push((key << 16) | c.value_at(i) as u32);
              }
          }
          assert_eq!(docs, big);
          assert!(matches!(bm.container_at(0), Container::Run(_)));

          // the inline block is invisible to the postings enum (pure seek)
          let postings = PostingsReader::open(&dir, "_0", &id).unwrap();
          let mut en = postings.docs(&e).unwrap();
          for d in 0..200 {
              assert_eq!(en.next_doc().unwrap(), d);
          }
          assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
          // "tail" (df=3 < 128) got no bitmap; its postings still decode
          let e2 = dict
              .seek_exact(fis.by_name("kw").unwrap(), b"tail")
              .unwrap()
              .unwrap();
          let mut en2 = postings.docs(&e2).unwrap();
          assert_eq!(en2.next_doc().unwrap(), 10);
          assert_eq!(en2.next_doc().unwrap(), 20);
          assert_eq!(en2.next_doc().unwrap(), 30);
          assert_eq!(en2.next_doc().unwrap(), NO_MORE_DOCS);
          fs::remove_dir_all(&root).unwrap();
      }
  ```

- [ ] **Step 3.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 inline_bitmap 2>&1 | tail -5
  error[E0599]: no method named `set_bitmap_threshold` found for struct `PostingsWriter`
  ...（BITMAP_MAGIC / bitmap_crc32 同样未定义）
  ```

- [ ] **Step 3.3: 实现写侧** — `crates/codec-lucene9/src/postings.rs` 三处修改：

  (a) `SEGMENT_SUFFIX` 常量声明之后加：

  ```rust
  // Lucene912PostingsFormat.java:347-352
  pub(crate) const SEGMENT_SUFFIX: &str = "Lucene912_0";

  /// M3 (spec §4): inline bitmap block magic, 4 bytes written little-endian
  /// ("RBMa" in the byte stream). The quad-validation in
  /// postings_read::inline_bitmap rejects any gap block whose first 4 bytes
  /// do not match.
  pub(crate) const BITMAP_MAGIC: u32 = 0x614D4252;
  /// M3 (spec §4): inline bitmap block format version (1 byte).
  pub(crate) const BITMAP_VERSION: u8 = 1;

  /// zlib CRC32 over the inline bitmap block (crc32fast — same algorithm as
  /// the CodecUtil footer CRC, codec_util.rs write_footer).
  pub(crate) fn bitmap_crc32(bytes: &[u8]) -> u32 {
      crc32fast::hash(bytes)
  }
  ```

  （`SEGMENT_SUFFIX` 已在原处，仅为定位锚点；常量与函数紧随其后插入。）

  (b) `PostingsWriter` 结构体加字段 + setter + `write_inline_bitmap`：

  ```rust
  pub struct PostingsWriter {
      dir: FSDirectory,
      segment: String,
      segment_id: [u8; 16],
      doc_out: ChecksumIndexOutput,
      pos_out: Option<ChecksumIndexOutput>,
      tim_out: ChecksumIndexOutput,
      tip_out: ChecksumIndexOutput,
      tmd_out: ChecksumIndexOutput,
      psm_out: ChecksumIndexOutput,
      field: Option<FieldState>,
      /// Serialized per-field .tmd records (Lucene90BlockTreeTermsWriter.fields).
      field_records: Vec<Vec<u8>>,
      max_num_impacts_level0: i32,
      max_impact_bytes_level0: i32,
      max_num_impacts_level1: i32,
      max_impact_bytes_level1: i32,
      /// M3 (spec §3/§4): terms with df >= this get an inline bitmap block
      /// before their postings; 0 = off (default, --bitmap 默认 off).
      bitmap_threshold: u32,
      files: Vec<String>,
  }
  ```

  `PostingsWriter::new` 的 `Ok(Self { ... })` 初始化列表加 `bitmap_threshold: 0,`；`impl PostingsWriter` 内（`start_field` 之前）加：

  ```rust
      /// M3 (spec §3/§4): terms with df >= `threshold` get an inline bitmap
      /// block written immediately before their docStartFP is captured.
      /// 0 disables the feature (default).
      pub fn set_bitmap_threshold(&mut self, threshold: u32) {
          self.bitmap_threshold = threshold;
      }
  ```

  `write_doc_postings` 之前加：

  ```rust
      /// spec §4 inline layout, written immediately before docStartFP is
      /// captured: [magic(4B LE) + version(1B) + df(VInt) + cardinality(VInt)
      /// + roaring payload + crc32(4B LE)][len: 4B LE], len = 头+payload+crc32
      /// 总字节. The block goes through the same ChecksumIndexOutput as the
      /// postings, so the .doc footer CRC covers it for free
      /// (CodecUtil.java:402-413) and the reader finds it at
      /// docStartFP-4-len (§4a: the inter-term gap is invisible to Java's
      /// pure-seek reader, Lucene912PostingsReader.java:436,809).
      fn write_inline_bitmap(&mut self, docs: &[u32]) -> io::Result<()> {
          let bm = RoaringBitmap::from_sorted_docs(docs);
          let mut block = IndexOutput::in_memory();
          block.write_int(BITMAP_MAGIC as i32)?; // write_int is LE (io.rs)
          block.write_byte(BITMAP_VERSION)?;
          block.write_vint(docs.len() as i32)?;
          block.write_vint(bm.cardinality() as i32)?;
          bm.serialize(&mut block)?;
          let mut bytes = block.into_bytes();
          let crc = bitmap_crc32(&bytes);
          bytes.extend_from_slice(&crc.to_le_bytes());
          let len = bytes.len() as u32;
          self.doc_out.write_bytes(&bytes)?;
          self.doc_out.write_int(len as i32)?; // LE
          Ok(())
      }
  ```

  use 区加 `use crate::roaring::RoaringBitmap;`。

  (c) `write_term` 的 `.pos` 写出与 docStartFP capture 之间加钩子（`postings.rs:349-354` 现状）：

  ```rust
          // --- .pos: full pfor chunks + tail (before .doc so skip fps are known)
          let (pos_start_fp, last_pos_block_offset, pos_block_index) =
              self.write_positions(docs, freqs, positions)?;

          // --- M3 inline bitmap (spec §4): df >= threshold 的 term 在
          // docStartFP capture 之前内联写 bitmap 块；docStartFP 仍指向
          // postings 起点，FST output schema 不变
          if self.bitmap_threshold != 0 && doc_freq >= self.bitmap_threshold {
              self.write_inline_bitmap(docs)?;
          }

          // --- .doc
          let doc_start_fp = self.doc_out.file_pointer();
  ```

- [ ] **Step 3.4: 跑测试确认通过（新测试 + 全量回归）**

  ```
  $ cargo test -p codec-lucene9 inline_bitmap 2>&1 | tail -3
  test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p codec-lucene9 2>&1 | tail -3
  test result: ok. 151 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
  ```

  （151 = 基线 142 + T1 的 7 + T2 的 1 + 本任务 1；既有测试全绿 = 默认 threshold=0 时字节零变化。）

- [ ] **Step 3.5: 提交（codec 部分）**

  ```
  git add crates/codec-lucene9/src/postings.rs
  git commit -m "feat: inline roaring bitmap blocks in .doc at flush (M3 §4)"
  ```

- [ ] **Step 3.6: 写 core 失败测试** — `crates/core/src/index_writer.rs` 末尾新建 `#[cfg(test)]` 模块（`IndexWriterConfig.bitmap` 等字段尚不存在，编译失败即失败测试成立）：

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::document::{Document, FieldValue};
      use crate::schema::{FieldSpec, Schema};
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

      fn write_corpus(root: &Path, bitmap: bool) {
          let mut cfg = IndexWriterConfig::default();
          cfg.bitmap = bitmap;
          cfg.bitmap_threshold = 4;
          let mut w = IndexWriter::create(root, schema(), cfg).unwrap();
          for i in 0..10 {
              w.add_document(doc("INFO", &format!("common w{i}"))).unwrap();
          }
          w.commit().unwrap();
          drop(w);
      }

      /// _0_Lucene912_0.doc 的字节数（postings.rs file_name 布局）。
      fn doc_bytes(root: &Path) -> u64 {
          fs::metadata(root.join("_0_Lucene912_0.doc")).unwrap().len()
      }

      /// spec §2/§4: bitmap on 不新增任何文件，bitmap 字节内联进 .doc；
      /// 写侧集成后索引照常可读可搜（roaring 读侧接线在 T4，本步走既有
      /// postings 路径——inline 字节对纯 seek reader 隐形，spec §4a.1）。
      #[test]
      fn inline_bitmap_write_adds_bytes_but_no_files() {
          let root = temp_dir("rbmwrite");
          write_corpus(&root, true);
          let off_root = temp_dir("rbmwriteoff");
          write_corpus(&off_root, false);

          // spec §2 不新增任何文件：同一语料 on/off 文件名集合完全一致
          let on_dir = FSDirectory::open(&root).unwrap();
          let off_dir = FSDirectory::open(&off_root).unwrap();
          assert_eq!(on_dir.list_all().unwrap(), off_dir.list_all().unwrap());

          // bitmap 字节内联在 .doc：on 的 .doc 严格大于 off 的，且差值有界
          // （本语料 2 个命中 term：level/INFO df=5 与 message/common df=10，
          //   各一个 [头+payload+crc32+len] 小块）
          let (on_len, off_len) = (doc_bytes(&root), doc_bytes(&off_root));
          assert!(on_len > off_len, "inline bytes must inflate .doc");
          assert!(on_len - off_len < 4096, "two tiny bitmap blocks + len suffixes");

          // 写侧集成后索引照常可读（既有 postings 路径）
          let mut s = crate::search::Searcher::open(&on_dir).unwrap();
          let q = crate::search::Query::term("message", "common");
          assert_eq!(s.count(&q).unwrap(), 10);
          fs::remove_dir_all(&root).unwrap();
          fs::remove_dir_all(&off_root).unwrap();
      }

      /// spec §3: --bitmap 默认 off —— 默认 config 与显式 off 的 .doc 字节数一致。
      #[test]
      fn bitmap_default_off_writes_no_inline_bytes() {
          let root = temp_dir("rbmdefault");
          let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
          for i in 0..10 {
              w.add_document(doc("INFO", &format!("common w{i}"))).unwrap();
          }
          w.commit().unwrap();
          drop(w);
          let off_root = temp_dir("rbmdefaultoff");
          write_corpus(&off_root, false);
          assert_eq!(doc_bytes(&root), doc_bytes(&off_root));
          fs::remove_dir_all(&root).unwrap();
          fs::remove_dir_all(&off_root).unwrap();
      }
  }
  ```

- [ ] **Step 3.7: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core index_writer 2>&1 | tail -5
  error[E0609]: no field `bitmap` on type `IndexWriterConfig`
  ```

- [ ] **Step 3.8: 实现 core 配置透传** — 两处修改：

  (a) `crates/core/src/index_writer.rs`：`IndexWriterConfig` 加两个字段（默认值保证 `--bitmap` 默认 off、既有 `::default()` 调用方不受影响）：

  ```rust
  pub struct IndexWriterConfig {
      /// Flush when this many docs are buffered (Lucene default: disabled / RAM-based).
      pub max_buffered_docs: u32,
      /// Flush when the buffered indexing data (postings/docvalues/points
      /// arenas, approximate) exceeds this many bytes. Default 512MB, per the
      /// project spec; Lucene's fair-comparison counterpart is
      /// IndexWriterConfig.setRAMBufferSizeMB.
      pub max_ram_bytes: usize,
      /// M3 (spec §3): write inline roaring-bitmap blocks into .doc at
      /// segment flush (df >= bitmap_threshold terms). Experimental,
      /// default off.
      pub bitmap: bool,
      /// M3 (spec §3): minimum doc_freq for a term to get an inline bitmap.
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

  (b) `crates/core/src/segment_builder.rs`：`SegmentBuilder` 加两个字段 + `with_bitmap`；`finalize` 在 `PostingsWriter::new` 之后透传 setter：

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

      /// M3 (spec §3): opt into inline roaring-bitmap output at finalize
      /// (default off; IndexWriterConfig::bitmap / --bitmap-threshold).
      pub fn with_bitmap(mut self, enabled: bool, threshold: u32) -> Self {
          self.bitmap_enabled = enabled;
          self.bitmap_threshold = threshold;
          self
      }
  ```

  `finalize` 开头的解构带上新字段：

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

  `if has_indexed` 块整体替换为（只在 `PostingsWriter::new` 之后多三行 setter 透传，term 循环逐行不变，完整列出便于核对）：

  ```rust
          if has_indexed {
              let mut pw = PostingsWriter::new(&dir, &seg_name, &seg_id)?;
              // M3 (spec §3/§4): the inline bitmap builds inside write_term
              // — the docs slices are already in RAM, so this is O(df) CPU
              // and zero extra IO
              if bitmap_enabled {
                  pw.set_bitmap_threshold(bitmap_threshold);
              }
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
                  }
                  pw.finish_field()?;
              }
              postings_files = pw.finish()?;
          }
  ```

- [ ] **Step 3.9: 跑测试确认通过（core 2 个新测试 + 全量回归）**

  ```
  $ cargo test -p rustlucene-core index_writer 2>&1 | tail -3
  test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p rustlucene-core 2>&1 | tail -3
  test result: ok. 41 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 3.10: 提交（core 部分）**

  ```
  git add crates/core/src/index_writer.rs crates/core/src/segment_builder.rs
  git commit -m "feat: wire --bitmap/--bitmap-threshold through IndexWriterConfig to PostingsWriter"
  ```

---

## Task 4: 读侧定位/校验 + RoaringDocIter + Term 接入（spec §5 档 1）

codec 提供 `PostingsReader::inline_bitmap`（定位 + 四重校验 + 反序列化）；core 接线三档规则的档 1。

**Files:**
- Modify: `crates/codec-lucene9/src/postings_read.rs`（`inline_bitmap` + `max_bitmap_len`）
- Modify: `crates/core/src/search/doc_iter.rs`（`RoaringDocIter` + `SegmentDocIter::Roaring`）
- Modify: `crates/core/src/search/segment_reader.rs`（`bitmap_enabled` 字段 + `open_with_bitmap` + `roaring_bitmap`）
- Modify: `crates/core/src/search/reader.rs`（`Reader::open_with_bitmap`）
- Modify: `crates/core/src/search/searcher.rs`（`Searcher::open_with_bitmap`）
- Modify: `crates/core/src/search/query.rs`（Term 分支档 1）
- Test: `crates/codec-lucene9/src/postings_read.rs` 与 `crates/core/src/search/mod.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T1 `RoaringBitmap` + `Container::{cardinality, value_at, lower_bound}` + `RoaringBitmap::deserialize`；T3 的 `BITMAP_MAGIC` / `BITMAP_VERSION` / `bitmap_crc32`；`crate::codec_util::index_header_length`（`codec_util.rs:35`）；既有 `PostingsReader::fresh_input`（`postings_read.rs:135`）、`TermEntry.state.doc_start_fp`（`terms_read.rs:26-31`）、`SegmentReader::seek_term` / `docs_enum` / `docs_freqs_enum`、`DocIter` 协议（`doc_iter.rs:14-31`）。
- Produces（T5/T6 依赖这些名字，不得改名）:
  ```rust
  // postings_read.rs
  /// spec §4/§5：定位 + 四重校验 docStartFP 前的内联 bitmap；任一失败
  /// Ok(None) 静默落档。非 bitmap term 只花 4B（+15B 头）小读。
  pub fn PostingsReader::inline_bitmap(&self, entry: &TermEntry, max_doc: i32) -> io::Result<Option<RoaringBitmap>>;

  // doc_iter.rs
  pub struct RoaringDocIter { .. }
  impl RoaringDocIter {
      pub fn new(bm: RoaringBitmap) -> Self;
      pub fn cardinality(&self) -> u64;
  }
  // SegmentDocIter 新增变体 Roaring(RoaringDocIter)，DocIter 全协议实现

  // segment_reader.rs
  pub(crate) fn open_with_bitmap(dir: &FSDirectory, sci: &SegmentCommitInfo, bitmap: bool) -> io::Result<SegmentReader>;
  pub(crate) fn roaring_bitmap(&self, entry: &TermEntry) -> io::Result<Option<RoaringBitmap>>;

  // reader.rs / searcher.rs
  pub fn Reader::open_with_bitmap(dir: &FSDirectory, bitmap: bool) -> io::Result<Reader>;
  pub fn Searcher::open_with_bitmap(dir: &FSDirectory, bitmap: bool) -> io::Result<Searcher>;
  ```

  语义决定：读侧自动探测——`docStartFP-4` 的 len 尾缀 + 四重校验回答"有没有 bitmap"（无任何元数据依赖）；`bitmap == false`（`--no-bitmap`）直接短路探测，用于 bench A/B。`needs_freq == true` 时 Term 分支不进 roaring（bitmap 无 freq，spec §2/§5）。

### Steps

- [ ] **Step 4.1: 写 codec 失败测试** — `crates/codec-lucene9/src/postings_read.rs` 的 `mod tests` 追加（`inline_bitmap` 尚不存在，编译失败即失败测试成立）。先加与 `write_segment` 同语料的 bitmap 变体 helper（不改既有 helper，既有测试零触动）：

  ```rust
      /// Same corpus as write_segment, with the inline-bitmap threshold set
      /// (None = bitmap off, the Java-written-index analog: the gap before
      /// docStartFP then holds the previous term's arbitrary postings bytes).
      /// kw "big" df=200, "tail" df=3; tx "hot" df=5000, "one" df=1,
      /// "warm" df=200 step 3.
      fn write_bitmap_segment(dir: &FSDirectory, threshold: Option<u32>) -> FieldInfos {
          let id = [4u8; 16];
          let kw = indexed("kw", 0, IndexOptions::Docs);
          let tx = indexed("tx", 1, IndexOptions::DocsAndFreqs);
          let mut w = PostingsWriter::new(dir, "_0", &id).unwrap();
          if let Some(t) = threshold {
              w.set_bitmap_threshold(t);
          }
          w.start_field(&kw, 6000).unwrap();
          let big: Vec<u32> = (0..200).collect();
          w.write_term(b"big", &big, &vec![1; 200], None).unwrap();
          w.write_term(b"tail", &[10, 20, 30], &[1, 1, 1], None).unwrap();
          w.finish_field().unwrap();
          w.start_field(&tx, 6000).unwrap();
          let hot: Vec<u32> = (0..5000).collect();
          w.write_term(b"hot", &hot, &vec![1; 5000], None).unwrap();
          w.write_term(b"one", &[42], &[7], None).unwrap();
          let warm_docs: Vec<u32> = (0..200).map(|i| i * 3).collect();
          let warm_freqs: Vec<u32> = (0..200).map(|i| (i % 5) + 1).collect();
          w.write_term(b"warm", &warm_docs, &warm_freqs, None).unwrap();
          w.finish_field().unwrap();
          w.finish().unwrap();
          let fis = FieldInfos::new(vec![kw, tx]);
          fis.write(dir, "_0", &id, "").unwrap();
          fis
      }

      fn hot_entry(dir: &FSDirectory, fis: &FieldInfos) -> TermEntry {
          seek(dir, fis, "tx", b"hot")
      }

      /// spec §4/§5: locate + quad-validate; content round-trips; terms
      /// below the threshold and the bitmap-off case degrade to None.
      #[test]
      fn inline_bitmap_round_trip_and_fallbacks() {
          let root = temp_dir("rbmread");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_bitmap_segment(&dir, Some(4096));
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();

          // "hot" (df=5000 >= 4096) -> bitmap; docs preserved
          let e = hot_entry(&dir, &fis);
          let bm = postings.inline_bitmap(&e, 6000).unwrap().unwrap();
          assert_eq!(bm.cardinality(), 5000);
          let mut docs = Vec::new();
          for ci in 0..bm.num_containers() {
              let key = bm.container_key(ci) as u32;
              let c = bm.container_at(ci);
              for i in 0..c.cardinality() {
                  docs.push((key << 16) | c.value_at(i) as u32);
              }
          }
          assert_eq!(docs, (0..5000).collect::<Vec<u32>>());

          // "big"/"warm" (df=200 < 4096) and the singleton -> None
          let e = seek(&dir, &fis, "kw", b"big");
          assert!(postings.inline_bitmap(&e, 6000).unwrap().is_none());
          let e = seek(&dir, &fis, "tx", b"warm");
          assert!(postings.inline_bitmap(&e, 6000).unwrap().is_none());
          let e = seek(&dir, &fis, "tx", b"one");
          assert!(postings.inline_bitmap(&e, 6000).unwrap().is_none());

          // postings enums unaffected by the gap bytes (pure seek)
          let e = hot_entry(&dir, &fis);
          let mut en = postings.docs_and_freqs(&e).unwrap();
          assert_eq!(en.next_doc().unwrap(), 0);
          assert_eq!(en.next_doc().unwrap(), 1);
          assert_eq!(en.advance(4999).unwrap(), 4999);
          assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);

          // bitmap off: docStartFP-4 holds the previous term's arbitrary
          // bytes -> quad validation must reject, never error
          let root2 = temp_dir("rbmreadoff");
          let dir2 = FSDirectory::open(&root2).unwrap();
          let fis2 = write_bitmap_segment(&dir2, None);
          let postings2 = PostingsReader::open(&dir2, "_0", &[4u8; 16]).unwrap();
          let e = hot_entry(&dir2, &fis2);
          assert!(postings2.inline_bitmap(&e, 6000).unwrap().is_none());
          fs::remove_dir_all(&root).unwrap();
          fs::remove_dir_all(&root2).unwrap();
      }

      /// 四重校验逐项：magic / version / df / crc32 任一损坏 -> None（静默）。
      #[test]
      fn inline_bitmap_validation_failures_degrade_to_none() {
          let root = temp_dir("rbmcorrupt");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_bitmap_segment(&dir, Some(4096));
          let e = hot_entry(&dir, &fis);
          let fp = e.state.doc_start_fp;
          let doc_path = root.join(crate::postings::file_name("_0", "doc"));
          let len = {
              let bytes = fs::read(&doc_path).unwrap();
              let at = (fp - 4) as usize;
              u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as u64
          };
          let block_start = fp - 4 - len;

          for (off_in_block, tag) in [
              (0u64, "magic"),
              (4, "version"),
              (5, "df vint first byte"),
              (len - 1, "crc last byte"),
          ] {
              let good = fs::read(&doc_path).unwrap();
              let mut bad = good.clone();
              bad[(block_start + off_in_block) as usize] ^= 0xFF;
              fs::write(&doc_path, &bad).unwrap();
              let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
              assert!(
                  postings.inline_bitmap(&e, 6000).unwrap().is_none(),
                  "corrupted {tag} must degrade to None"
              );
              fs::write(&doc_path, &good).unwrap(); // restore for the next case
          }
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  （df=5000 的 VInt 首字节在块内偏移 5 处；翻转它使 df 变为 5000±128 ≠ termState.df。）

- [ ] **Step 4.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 inline_bitmap 2>&1 | tail -5
  error[E0599]: no method named `inline_bitmap` found for struct `PostingsReader`
  ```

- [ ] **Step 4.3: 实现 codec 读侧** — `crates/codec-lucene9/src/postings_read.rs`：use 区的 `crate::postings::{...}` 列表加 `BITMAP_MAGIC, BITMAP_VERSION, bitmap_crc32`，`crate::codec_util::{...}` 列表加 `index_header_length`，新增 `use crate::roaring::RoaringBitmap;`；`impl PostingsReader` 内（`fresh_input` 之后）加：

  ```rust
      /// spec §4/§5: locate + quad-validate the inline bitmap immediately
      /// before docStartFP — len 有界（按 maxDoc 推算）且块不越入 .doc
      /// header 区 → magic → df == termState.df → crc32. Any failure
      /// degrades to `Ok(None)`: the caller falls back to the postings
      /// path and a garbage gap block must never cause an error. Lazy by
      /// construction (§5 惰性加载): a non-bitmap term costs only a 4B
      /// (+15B header) read; the payload is read only for real bitmaps.
      pub fn inline_bitmap(
          &self,
          entry: &TermEntry,
          max_doc: i32,
      ) -> io::Result<Option<RoaringBitmap>> {
          let fp = entry.state.doc_start_fp;
          if fp < 4 {
              return Ok(None);
          }
          let mut in_ = self.fresh_input()?;
          in_.seek(fp - 4)?;
          let len = in_.read_int()? as u32 as u64;
          if len < 16 || len > max_bitmap_len(max_doc) || len + 4 > fp {
              return Ok(None);
          }
          let block_start = fp - 4 - len;
          if block_start < index_header_length(DOC_CODEC, SEGMENT_SUFFIX) as u64 {
              return Ok(None);
          }
          in_.seek(block_start)?;
          let mut block = vec![0u8; len as usize];
          in_.read_bytes(&mut block)?;
          // magic + version (cheap reject before any further work)
          if u32::from_le_bytes(block[0..4].try_into().unwrap()) != BITMAP_MAGIC {
              return Ok(None);
          }
          if block[4] != BITMAP_VERSION {
              return Ok(None);
          }
          // df == termState.df (cardinality 同值构建, spec §4)
          let mut cur = IndexInput::in_memory(block.clone());
          cur.seek(5)?;
          if cur.read_vint()? != entry.doc_freq as i32 {
              return Ok(None);
          }
          if cur.read_vint()? != entry.doc_freq as i32 {
              return Ok(None);
          }
          // payload, then exactly 4 crc bytes must remain
          let bm = match RoaringBitmap::deserialize(&mut cur) {
              Ok(bm) => bm,
              Err(_) => return Ok(None),
          };
          if bm.cardinality() != entry.doc_freq as u64 {
              return Ok(None);
          }
          if cur.file_pointer() + 4 != len {
              return Ok(None);
          }
          // crc32 over header+payload (spec §4 四重校验的最后一项)
          let stored = u32::from_le_bytes(block[len as usize - 4..].try_into().unwrap());
          if bitmap_crc32(&block[..len as usize - 4]) != stored {
              return Ok(None);
          }
          Ok(Some(bm))
      }
  ```

  文件级（`impl PostingsReader` 之外）加：

  ```rust
  /// spec §4 四重校验的 len 上限：maxDoc 个 doc 至多 ceil(maxDoc/65536)
  /// 个容器，每个至多 8KB bitset + 数字节头（key/tag/cardinality ≈ 8B）；
  /// 24 = 头(≤15B) + numContainers(≤3B) + crc(4B) 的余量。超出即非 bitmap 块。
  fn max_bitmap_len(max_doc: i32) -> u64 {
      let containers = (max_doc.max(0) as u64 + 65535) / 65536;
      24 + containers * 8200
  }
  ```

- [ ] **Step 4.4: 跑测试确认通过（codec 2 个新测试 + 全量回归 + T3 前置锁定的 core 测试）**

  ```
  $ cargo test -p codec-lucene9 inline_bitmap 2>&1 | tail -3
  test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p codec-lucene9 2>&1 | tail -3
  test result: ok. 153 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
  $ cargo test -p rustlucene-core index_writer 2>&1 | tail -3
  test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

  （codec 3 个 = T3 的 1 + 本任务 2；153 = 151 + 2；core 的 T3 测试在本步接口落地后转绿。）

- [ ] **Step 4.5: 提交（codec 读侧）**

  ```
  git add crates/codec-lucene9/src/postings_read.rs
  git commit -m "feat: locate and quad-validate inline term bitmaps on the read path (M3 §4/§5)"
  ```

- [ ] **Step 4.6: 写 core 失败测试** — 追加到 `crates/core/src/search/mod.rs` 的 `mod tests`（复用模块内已有 `temp_dir` / `schema` / `doc` 助手；`Searcher::open_with_bitmap`、`SegmentDocIter::Roaring` 尚不存在，编译失败即失败测试成立）：

  ```rust
      /// 10 docs, threshold 4: message "common" (df=10) and level INFO/WARN
      /// (df=5) get inline bitmaps; w{i} (df=1) stay postings-only.
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
          // freq_sum unaffected (total_term_freq shortcut; spec §5 freq 永远走 postings)
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
      fn no_bitmap_segment_stays_on_postings() {
          let root = temp_dir("rbmnone");
          write_bitmap_corpus(&root, false); // bitmap off at write time
          let dir = FSDirectory::open(&root).unwrap();
          // read side auto-detects: no inline bitmap -> postings (spec §5 档规则按段独立)
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

- [ ] **Step 4.7: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core roaring 2>&1 | tail -5
  error[E0425]: cannot find function `open_with_bitmap` in `Searcher`
  ...（SegmentDocIter::Roaring 同样未定义）
  ```

- [ ] **Step 4.8: 最小实现（core 接线）** — 五处修改：

  (a) `crates/core/src/search/doc_iter.rs`：`use codec_lucene9::roaring::RoaringBitmap;` 加到 use 块；`RoaringDocIter` 插在 `// ── SegmentDocIter` 注释之前：

  ```rust
  // ── Roaring (M3 inline bitmap) ────────────────────────────────────────

  /// DocIter over a (inline-loaded or query-materialized) roaring bitmap
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

  `SegmentDocIter` 枚举加变体与三个 match 分支：

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

  (b) `crates/core/src/search/segment_reader.rs`：use 块加 `use codec_lucene9::roaring::RoaringBitmap;`；结构体加字段 + 两个方法：

  ```rust
  pub struct SegmentReader {
      max_doc: i32,
      field_infos: FieldInfos,
      terms: TermsDict,
      postings: PostingsReader,
      bitmap_enabled: bool,
  }

  impl SegmentReader {
      pub fn open(dir: &FSDirectory, sci: &SegmentCommitInfo) -> io::Result<SegmentReader> {
          Self::open_with_bitmap(dir, sci, true)
      }

      /// `bitmap == false` disables the inline-bitmap read path (bench A/B
      /// switch, spec M3 §5); with `true` bitmaps are auto-detected via the
      /// docStartFP-4 len suffix + quad validation.
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
          Ok(SegmentReader {
              max_doc: sci.info.doc_count,
              field_infos,
              terms,
              postings,
              bitmap_enabled: bitmap,
          })
      }

      /// M3 §5 bitmap source: the codec probe answers "is there an inline
      /// bitmap for this term" with quad validation (§4/§5); `bitmap ==
      /// false` (--no-bitmap) short-circuits for bench A/B.
      pub(crate) fn roaring_bitmap(&self, entry: &TermEntry) -> io::Result<Option<RoaringBitmap>> {
          if !self.bitmap_enabled {
              return Ok(None);
          }
          self.postings.inline_bitmap(entry, self.max_doc)
      }
  ```

  (c) `crates/core/src/search/reader.rs`：`open` 委托 + 新方法：

  ```rust
      /// DirectoryReader.open: reads the latest commit and opens every
      /// segment in commit order (global docID = docBase + segment docID).
      pub fn open(dir: &FSDirectory) -> io::Result<Reader> {
          Self::open_with_bitmap(dir, true)
      }

      /// `bitmap == false` disables inline-bitmap reads in every segment
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
      /// has inline bitmaps — bench A/B on one and the same index (spec M3 §5).
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
                  // M3 §5 tier 1: inline bitmap -> roaring iteration (freq
                  // consumers keep the postings enum — the bitmap carries
                  // no freqs, spec §2/§5)
                  if !needs_freq {
                      if let Some(bm) = seg.roaring_bitmap(&entry)? {
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

- [ ] **Step 4.9: 跑测试确认通过（3 个新测试 + 全量回归）**

  ```
  $ cargo test -p rustlucene-core roaring 2>&1 | tail -3
  test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p rustlucene-core 2>&1 | tail -3
  test result: ok. 44 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 4.10: 提交**

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

  档规则（spec §5，per-segment 独立）：子句逐一 `roaring_bitmap(&entry)` 探测 → 全无 bitmap → 档 3 既有 PFOR 路径**逐字节不动**；≥1 个有 → 无 bitmap 的子句 `materialize_roaring` 物化，全部走 roaring and/or 折叠，结果仍是 roaring，直接迭代（不物化成数组）。And 遇 absent term → `Ok(None)`（无命中，既有语义）；Or 跳过 absent term（既有语义）。`needs_freq == true` 时整支不进 roaring（bitmap 无 freq）。

### Steps

- [ ] **Step 5.1: 写失败测试** — 追加到 `crates/core/src/search/mod.rs` 的 `mod tests`（`materialize_roaring` 与档判定尚不存在，变体断言编译失败即失败测试成立）：

  ```rust
      fn tier_schema() -> Schema {
          let mut s = Schema::new();
          s.add(FieldSpec::text("message"));
          s
      }

      /// 20 docs: ha df=12 (0..12), hb df=12 (4..16), lo df=2 {0,1},
      /// lo2 df=2 {1,2}; threshold 4 puts ha/hb inline, lo/lo2 stay
      /// postings-only. u{i} keeps every doc's message non-empty.
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
                  // carries no freqs, spec §2/§5); with no inline bitmap,
                  // roaring_bitmap returns None after the cheap len/header
                  // checks.
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
                          seg.roaring_bitmap(&entry)?
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
                  // tiers 1/2: one roaring engine; clauses without an inline
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
                              seg.roaring_bitmap(&entry)?
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
  test result: ok. 45 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 5.5: 提交**

  ```
  git add crates/core/src/search/doc_iter.rs crates/core/src/search/query.rs crates/core/src/search/mod.rs
  git commit -m "feat: And/Or three-tier roaring execution + query-time materialization (M3 §5)"
  ```

---

## Task 6: diff 电池 `--bitmap` 变体（含 Java forceMerge 实证）+ CLI 接线 + bench 三路报告

**Files:**
- Modify: `crates/core/src/bin/rustlucene-cli.rs`（logwrite `--bitmap`/`--bitmap-threshold`、searchdump/searchbench `--no-bitmap`、usage 文本）
- Create: `interop/java/ForceMerge.java`（最小 forceMerge 工具，CFS 关）
- Modify: `interop/verify-log.sh`（变体参数改名 + `--bitmap` 分支：on/off searchdump diff + Java forceMerge 后再对拍；verify-search.sh 旗标过滤）
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
  java -cp <cp> ForceMerge <indexDir>
  interop/verify-log.sh [numDocs] [seed] [--positions|--sparse|--bigdict|--bitmap]
  ```
  bench 数据/报告：`.superpowers/sdd/m3-{write-on,write-off,disk}.out`、`.superpowers/sdd/m3-q.txt`、`.superpowers/sdd/m3-bench-{roaring,pfor,java}.out`、`.superpowers/sdd/m3-counts-*`、`.superpowers/sdd/m3-bitmap-bench-report.md`（gitignored，不进 git）。

### Steps

- [ ] **Step 6.1: CLI 接线** — `crates/core/src/bin/rustlucene-cli.rs` 五处修改：

  (a) `logwrite` 函数签名加两个参数、config 用可变实例（完整替换现有函数）：

  ```rust
  /// Single-writer log-schema indexing (the interop counterpart of JavaLogBench).
  /// `bitmap` writes inline roaring blocks for terms with df >= bitmap_threshold
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

- [ ] **Step 6.2: 新增 `interop/java/ForceMerge.java`** — `interop/java/` 现有 16 个工具无 forceMerge 入口（已核实），新增最小工具。**必须 `setUseCompoundFile(false)`**：Java 默认 merge 可能产出 CFS 复合文件，Rust 读侧无 CFS 支持（关键设计事实 6）；这也顺带实证 §4a.4"CFS 无关"不在本电池路径上。`Makefile:9-11` 的 `javac interop/java/*.java` 通配自动编译，无需改 Makefile 编译段：

  ```java
  import java.nio.file.*;
  import org.apache.lucene.index.*;
  import org.apache.lucene.store.*;

  /**
   * M3 battery tool (spec §8): force-merge an index in place to one segment.
   * The merged segment is re-encoded through PostingsEnum, so it carries no
   * inline roaring bitmap and per-segment execution naturally falls back to
   * the postings tier (spec §4a.4). Compound files are disabled because the
   * Rust reader has no CFS support. Used by interop/verify-log.sh --bitmap:
   * after the merge, Rust and Java search dumps must still be identical.
   *
   * Usage: ForceMerge <indexDir>
   */
  public class ForceMerge {
      public static void main(String[] args) throws Exception {
          try (Directory dir = FSDirectory.open(Paths.get(args[0]));
               IndexWriter w = new IndexWriter(dir, new IndexWriterConfig().setUseCompoundFile(false))) {
              w.forceMerge(1);
          }
          System.out.println("FORCEMERGE_OK");
      }
  }
  ```

- [ ] **Step 6.3: `interop/verify-log.sh` 改造** — 全文件替换为（要点：变体参数改名 VARIANT；传给 verify-search.sh 的旗标过滤为仅 `--positions`；`--bitmap` 分支做 bitmap on/off searchdump diff + Java forceMerge 后 CheckIndex + 合并后 Java↔Rust 对拍 + 合并前后结果不变对拍，覆盖 spec §8 全部三项终验）：

  ```bash
  #!/usr/bin/env bash
  # M2 interop: Rust writes a log-schema index -> Java CheckIndex + query dump;
  # Java writes the same corpus with stock Lucene -> CheckIndex + query dump;
  # the two dumps must be identical.
  # M3: with --bitmap the Rust logwrite also inlines roaring bitmap blocks in
  # .doc (spec §4); the variant additionally checks that bitmap on/off
  # searchdumps are identical (§8), and force-merges the index with Java to
  # prove the merged (bitmap-free) segment still searches identically (§8).
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

    echo "== Java forceMerge: merged segment has no bitmap, results unchanged (spec §8)"
    java -cp "$CP" ForceMerge "$RUST_DIR"
    echo "== CheckIndex $RUST_DIR (post-merge)"
    java -cp "$CP" org.apache.lucene.index.CheckIndex "$RUST_DIR" 2>&1 \
      | grep -E "No problems|FAILED|error" || true
    java -cp "$CP" org.apache.lucene.index.CheckIndex "$RUST_DIR" > /dev/null 2>&1
    cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" > /tmp/rl-search-merged-rust.out
    java -cp "$CP" VerifySearchIndex "$RUST_DIR" > /tmp/rl-search-merged-java.out
    echo "== Post-merge: Rust dump vs Java dump"
    diff -u /tmp/rl-search-merged-rust.out /tmp/rl-search-merged-java.out
    echo "== Post-merge vs pre-merge: results unchanged (natural fallback)"
    diff -u /tmp/rl-search-rust.out /tmp/rl-search-merged-rust.out
  fi

  echo "LOG_INTEROP_OK"
  ```

  （`interop/verify-search.sh` 不改——它只在第五参收到 `--positions` 或空。）

- [ ] **Step 6.4: `Makefile` 加第五变体** — `log-test` 目标改为（注释同步更新；recipe 行必须是制表符缩进）：

  ```make
  # M2 log-schema interop: Rust logwrite vs JavaLogBench, CheckIndex + query diff;
  # M3 adds the --bitmap inline-bitmap variant (on/off diff + Java forceMerge)
  log-test: build java-classes
  	interop/verify-log.sh 200000 42
  	interop/verify-log.sh 200000 43 --positions
  	interop/verify-log.sh 200000 44 --sparse
  	interop/verify-log.sh 200000 45 --bigdict
  	interop/verify-log.sh 200000 46 --bitmap
  ```

- [ ] **Step 6.5: 编译 + 单变体烟测**

  ```
  $ cargo build --release 2>&1 | tail -1
      Finished `release` profile [optimized] target(s) in ...
  $ interop/verify-log.sh 200000 46 --bitmap 2>&1 | tail -16
  ...
  == CheckIndex /tmp/rl-log-rust
  No problems were detected with this index.
  ...
  == Bitmap on/off searchdump diff (spec §8: 逐位一致)
  （diff 无输出）
  == Java forceMerge: merged segment has no bitmap, results unchanged (spec §8)
  FORCEMERGE_OK
  == CheckIndex /tmp/rl-log-rust (post-merge)
  No problems were detected with this index.
  == Post-merge: Rust dump vs Java dump
  （diff 无输出）
  == Post-merge vs pre-merge: results unchanged (natural fallback)
  （diff 无输出）
  SEARCH_INTEROP_OK
  LOG_INTEROP_OK
  ```

  排错提示：on/off diff 非空 → roaring 路径结果与 postings 不一致，回 T4/T5 语义测试；forceMerge 后 Rust dump 报错 →  merged segment 是 CFS 或 Java 写出的 commit 文件读侧有缺口（回 Step 6.2 的 setUseCompoundFile(false)，再查 Rust 读侧对 Java-written .si 的解析）；合并前后 diff 非空 → merge 改变了 doc 集（不该发生，查 docID 映射）。

- [ ] **Step 6.6: `make log-test` 五变体全绿（本任务验收门槛）**

  ```
  $ make log-test 2>&1 | grep -E "LOG_INTEROP_OK|No problems|FAILED|diff|FORCEMERGE_OK"
  （五段各一次 LOG_INTEROP_OK；每个 Rust/Java 索引各一次 "No problems"；--bitmap 段另有
     post-merge 的 "No problems" 与 FORCEMERGE_OK；无任何 diff 输出、无 FAILED）
  ```

  既有四变体（无内联 bitmap）全绿 = 写侧默认 off 字节零变化 + 读侧探测对无 bitmap 索引零影响（档 3 回归）；`--bitmap` 变体绿 = spec §8 三项终验（on/off 逐位一致、CheckIndex、forceMerge 后再对拍）。

- [ ] **Step 6.7: 提交（CLI + 电池 + ForceMerge 工具）**

  ```
  git add crates/core/src/bin/rustlucene-cli.rs interop/java/ForceMerge.java interop/verify-log.sh Makefile
  git commit -m "feat: log-test --bitmap variant with Java forceMerge proof + CLI bitmap switches (M3 §8)"
  ```

- [ ] **Step 6.8: bench 脚本 + 执行** — 新建 `bench/run-bitmap-bench.sh`（chmod +x）：

  ```bash
  #!/usr/bin/env bash
  # M3 inline roaring bitmap bench (spec §8): three-way comparison on the
  # SAME bitmap index — Rust roaring (default) vs Rust PFOR (--no-bitmap)
  # vs Java Lucene 9.12.3 (--no-cache) — plus write-throughput loss and
  # .doc size overhead. 1M docs so message terms cross df 4096 (see
  # 关键设计事实 13). Data + report land in .superpowers/sdd/ (gitignored).
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
  ls -l "$IDX_ON"/_*_Lucene912_0.doc "$IDX_OFF"/_*_Lucene912_0.doc | tee -a "$OUT/m3-disk.out"

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

- [ ] **Step 6.9: bench 报告落盘** — 写 `.superpowers/sdd/m3-bitmap-bench-report.md`（gitignored，不进 git），内容必须含：
  - 口径行：`--no-cache`（Java 旗标；Rust 本无 query cache）、1000000 docs、seed 42、`--warmup 10 --iter 30`、tasks 100、单线程 logwrite。
  - 写侧：从 `m3-write-{on,off}.out` 取 `docs_per_sec` 双侧数值与 bitmap 开销百分比；从 `m3-disk.out` 取两索引总字节与 `.doc` 文件字节差（内联增量占比）。
  - 读侧三路表：按 `m3-bench-{roaring,pfor,java}.out` 的分组行（query_type × freq），列出 qps 与 p50/p90/p99 三方数值，重点给出 **and/high、or/high、iterm/high** 三组的 roaring vs PFOR 提速比与 roaring vs Java 比。
  - 结论一句：对照 spec §8 预期（高 df AND 提速一个数量级；count 早已 O(1) 无变化空间）判定达标与否；未达标的组只记录不追责。
  - AVX2 有效性（spec §6 "bench 数据门槛"）：`RL_SIMD=0` 重跑一次 Rust roaring searchbench 取同组 qps，与默认（AVX2 分发）对比一句话。

- [ ] **Step 6.10: 提交（bench 脚本；`.superpowers/` 已被 gitignore）**

  ```
  git add bench/run-bitmap-bench.sh
  git commit -m "bench: M3 roaring vs PFOR vs Java three-way bench script"
  ```

---

## 收尾检查单（全部 Task 完成后逐项核对）

- [ ] `cargo test -p codec-lucene9` 全绿（T1/T2/T3/T4 共 11 个新测试）
- [ ] `cargo test -p rustlucene-core` 全绿（T3/T4/T5 共 6 个新测试）
- [ ] `RL_SIMD=0 cargo test -p codec-lucene9 roaring` 全绿（kill switch 标量路径）
- [ ] `make log-test` 五变体全绿，CheckIndex 全部 "No problems"（含 forceMerge 后）
- [ ] `make log-test` 的 `--bitmap` 变体：on/off searchdump diff 为空、post-merge Java↔Rust diff 为空、合并前后 diff 为空
- [ ] `grep -rn "unsafe" crates/core/src/` 无新增（`#![forbid(unsafe_code)]` 兜底）；`#[allow(unsafe_code)]` 仍只出现在 `postings_ll/simd.rs` 与 `roaring/simd.rs`
- [ ] `Cargo.toml` 两个 crate 均无新依赖
- [ ] bitmap on/off 同语料文件名集合一致（T3 core 测试覆盖，spec §2 不新增任何文件）
- [ ] bench 报告在 `.superpowers/sdd/m3-bitmap-bench-report.md`，含写侧吞吐/.doc 增量/三路读侧数值
