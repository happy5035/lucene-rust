# M3 高 df term 内联 Roaring bitmap（.doc 缝隙字节）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 M2（multi-term/phrase 读路径）之上，为 df ≥ 4096 的 term 在 segment flush 时构建 RoaringBitmap 并**内联写入 .doc 流**（每个 term 的 postings 之前、`docStartFP` 由现有 FST output 定位，`--bitmap` 实验开关默认 off、`--bitmap-threshold N` 可调），读侧按 spec §5 三档规则为 Term/And/Or 提供 roaring 执行路径（count = cardinality O(1)，迭代走容器游标），同时保持 postings 主格式字节不变、Java 读写零感知（CheckIndex 仍 "No problems"）、Java forceMerge 产物自然落档。对应已批准 spec `docs/superpowers/specs/2026-07-23-rust-search-m3-roaring-bitmap-design.md` 的全部范围（§9 任务切分，本计划把容器库的标量核与 AVX2 快路径拆成两个任务，共 7 个任务）。

**Architecture:** 延续方案 C（算法语义照抄 9.12.3、对象结构 Rust 化）。容器子库（array/bitset/run 三容器 + build/and/or/cardinality/iter + runOptimize，Roaring 论文语义）落在 `codec-lucene9/src/roaring.rs`，AVX2 快路径落在 `roaring/simd.rs`（模块级 `#[allow(unsafe_code)]`，照 `postings_ll/simd.rs` 先例）。写侧：`PostingsWriter::write_term` 在 capture `doc_start_fp` **之前**往同一个 `ChecksumIndexOutput` 写 `[bitmap 头+payload+crc32][len: 4B LE]`（footer CRC 因此自然覆盖 bitmap 字节），`--bitmap`/`--bitmap-threshold` 经 cli → `IndexWriterConfig` → `SegmentBuilder` → `PostingsWriter` 流动。读侧：codec 层 `PostingsReader::read_term_bitmap`（全量读+四重校验）与 `read_term_bitmap_header`（count 专用，只读头）经 `docStartFP-4` 定位，任一校验失败 `Ok(None)` **静默落档 postings**；core 层 `RoaringDocIter` 实现 `DocIter` 协议接入 Term，And/Or 经 `roaring_exec.rs` 三档分发（全无 bitmap → 现有 PFOR 不动；部分有 → 查询时物化低 df 子句；全有 → 纯容器运算），`RL_BITMAP=0` 环境变量提供同二进制 A/B（照 `RL_SIMD=0` 先例）。验证三层不变：单测（codec 容器/读写 round-trip、core 语义）→ Rust bitmap on/off 逐位一致 → Java diff 终验（`make log-test` 加第 5 变体 `--bitmap` + Java forceMerge 后再对拍）。

**Tech Stack:** Rust（codec crate edition 2024、core crate edition 2021；codec `#![deny(unsafe_code)]` + `postings_ll/simd.rs` 与新增 `roaring/simd.rs` 两个模块级 `#[allow(unsafe_code)]`，core `#![forbid(unsafe_code)]`，统一 `io::Result`）；不新增依赖（crc32fast 已是 codec 依赖）；Java 9.12.3（`interop/java/lib/lucene-core-9.12.3.jar`）做 diff 基准与 forceMerge 工具；格式语义以 `reference/lucene-9.12.3/` 源码为准（该目录只在主 checkout 存在，worktree 内引用按仓库根相对路径书写）。

## Global Constraints

（摘自 spec 与既有项目惯例，逐字或就近转述；所有 Task 共同遵守）

- **postings 主格式字节不动**。bitmap 字节是纯增量，只出现在 term 之间的缝隙（每个命中 term 的 postings 之前）；`.tim/.tip/.tmd/.pos/.psm` 及 FST output schema 零改动。`--bitmap` 默认 off：不写 `--bitmap` 的索引与 M2 字节级一致。
- **Rust edition 分工**：`crates/codec-lucene9` edition 2024，`crates/core` edition 2021（各自 Cargo.toml 已声明，新增代码遵守，不得改动）。codec 是 `#![deny(unsafe_code)]`（`lib.rs:13`），除既有 `postings_ll/simd.rs` 外只新增 `roaring/simd.rs` 一个模块级 `#[allow(unsafe_code)]`；core 保持 `#![forbid(unsafe_code)]`。
- **不新增外部 crate**。crc32fast 已在 `crates/codec-lucene9/Cargo.toml`（bitmap crc32 直接复用，与 footer CRC 同算法 = java.util.zip.CRC32）。
- **测试命令**：codec 层 `cargo test -p codec-lucene9 <test名>`，core 层 `cargo test -p rustlucene-core <test名>`；收尾门槛为 `make log-test`（200000 文档**五**变体：seed 42 默认 / 43 `--positions` / 44 `--sparse` / 45 `--bigdict` / 46 `--bitmap`，Makefile:17-22，本计划在 T6 追加第 5 行），较慢，只在 T6 电池任务与最终收尾使用；T3–T5 的增量用 `cargo test` 覆盖。
- **Lucene 语义照抄**：执行语义逐行对照 9.12.3 源码，关键决策在代码注释中给 `File.java:line` 引用（Java 源码在 `reference/lucene-9.12.3/lucene/core/src/java/org/apache/lucene/`，下文引用省略该前缀）。找不到精确行号时引用类名 + 方法名，不编行号。
- **commit message 前缀**：`feat:` / `fix:` / `docs:` / `bench:` / `test:`（沿用 git log 现有风格）。
- **全部 ConstantScore 语义**：不做评分 / norms / impact / Block-Max；bitmap 不带 freq/positions——**任何需要 freq/positions 的查询路径永远走 postings**（`needs_freq == true` 时不构造 roaring 迭代器；`freq_sum` 走 `TermEntry.total_term_freq`；phrase 走 `PositionsEnum`）。roaring 路径 `freq()` 恒为 1。
- **前提假设**：读侧只保证读**本系统写出的**索引（无 delete、无 payload、无 offsets）。bitmap 读侧对本系统未开 `--bitmap` 的索引、Java 写的索引、Java merge 产物一律经四重校验失败**自然落档 postings**（spec §3 YAGNI：不为它们提供 bitmap）。本系统从不写 CFS（`segment_info.rs:57` `is_compound_file: false`，读侧也无 CFS reader），`--bitmap` 不改变这一点；Java forceMerge 工具必须 `setUseCompoundFile(false)`（对齐 `JavaLogBench.java:91`）。
- **YAGNI（spec §3 明确不做）**：bitmap 不存 freq/positions；multi-term（M2 的 >16 bitset 路径）不做 roaring 集成（二期，物化 helper 本计划已同源）；纯低 df 布尔查询不走 roaring（保留跳读红利）；不做查询结果缓存；不做 bitmap 的磁盘格式演进承诺（version 字节占坑，实验期只有 v1）。
- **实验/报告文件**：bench 数据与报告写到 `.superpowers/sdd/`（M2 起已 gitignored），不进 git。
- **验收门槛**：`cargo fmt --check` 干净；`cargo test` 两 crate 全绿；`make log-test` 五变体（含新 `--bitmap`）全绿且每次 CheckIndex 输出 "No problems"；`--bitmap` 变体内 Rust bitmap on/off searchdump diff 为空、Java forceMerge 后 Java↔Rust searchdump diff 为空；T7 bench 的 per-query hit-counts 与 Java 侧 diff 为空。

## Pre-checks（已执行，基线绿）

```
$ cargo build
    Finished `dev` profile [unoptimized + debuginfo] target(s)
$ cargo test -p codec-lucene9
test result: ok. 142 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
$ cargo test -p rustlucene-core
test result: ok. 39 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## 关键设计事实（本计划全部代码的字节级依据，已逐项对照 9.12.3 源码与本仓库源码核实）

1. **写侧 hook 点**：`crates/codec-lucene9/src/postings.rs:354`——`write_term` 中 `let doc_start_fp = self.doc_out.file_pointer();` 之前。此时 `docs: &[u32]` 全量在内存（升序，`debug_assert` 于 postings.rs:346），df = `docs.len()`。bitmap 字节经**同一个** `self.doc_out`（`ChecksumIndexOutput`，io.rs:189）写入，footer CRC 覆盖全流（`CodecUtil.writeCRC` :643-650；spec §4a.2），`finish()` 的 `write_footer(&mut self.doc_out)`（postings.rs:439）与其后记录的 `doc_len`（写进 .psm，reader 端 `check_footer_structure` 按全长校验，postings_read.rs:72）都无需任何改动。df==1 的 singleton 在 .doc 中本就没有 postings（format-notes-postings.md §1），bitmap 同样不写（`doc_freq >= threshold && doc_freq > 1`）。
2. **内联布局**（spec §4 拍板项 + 本计划自定义的 payload 小节，读侧按此严格解析）：
   ```
   [ magic(4B) = "RLBM" (0x52 0x4C 0x42 0x4D)
     version(1B) = 1
     df(vInt)           —— 必须 == termState.doc_freq（四重校验第 3 条）
     cardinality(vInt)  —— docs-only bitmap 恒 == df；count 查询只读到这（spec §4）
     payload: numContainers(vInt)，随后逐 container：
       key(u16 LE) type(u8: 0=array,1=bitset,2=run) card(vInt) data
       array:  card × u16 LE，严格升序
       bitset: 8192B = 1024 × u64 LE，bit b 表示低 16 位值 b
       run:    numRuns(vInt) + numRuns × (start u16 LE, end u16 LE)，闭区间、升序、不重叠
     crc32(u32 LE)      —— crc32fast(header+payload)，与 footer 同算法
   ][ len: u32 LE ]     —— = 上面整个 region（头+payload+crc32）的字节数，不含自身
   ```
   docStartFP 照常指向 postings 起点；bitmap region = `[docStartFP-4-len, docStartFP-4)`。
3. **len 上界推导**（spec §4 校验第 1 条"len 有界"的精确公式；正确性硬性要求 (a)）：runOptimize 后每个 container 的 data ≤ 8192B——array 恒 < 4096 个值（≤ 8190B）、bitset 恒 8192B、run 只在 4B/run 严格小于原表示时才转换（4×runs < 8192 ⇒ runs ≤ 2047 ⇒ ≤ 8188B）；每 container 头部开销 ≤ key 2B + type 1B + card vInt ≤ 3B + numRuns vInt ≤ 3B = 9B；container 数 ≤ ⌈maxDoc/65536⌉；公共头 ≤ 4+1+5+5 = 15B，numContainers vInt ≤ 5B，crc 4B。故：
   **`max_bitmap_len(max_doc) = 24 + ceil(max_doc / 65536) × 8201`**
   （即 spec 的"len ≤ maxDoc/8 + slack"：8201/65536 ≈ 1/7.995；maxDoc=200000 时上界 32828B。）该上界只依赖 maxDoc 与 container 尺寸上限，与 term 的 df、写侧 threshold 无关——任意 `--bitmap-threshold` 写出的索引共用同一读侧上界。
4. **读侧定位与四重校验**（spec §4；正确性硬性要求 (b)）：仅当 `entry.doc_freq >= BITMAP_MIN_DF (4096)`（spec §5 的读侧门槛，常量化）才尝试：`fp = docStartFP`，先查 `fp >= 4`，`seek(fp-4)` 读 len（LE u32，`DataInput::read_int`），校验① `0 < len ≤ max_bitmap_len(max_doc)`；再查 `fp - 4 >= len`（**永不 seek 到 0 以下**，u64 无下溢）后 `seek(fp-4-len)` 读全 region；校验② magic+version；③ 头内 df == `termState.doc_freq`（且 cardinality == df）；④ crc32。任一失败 → `Ok(None)` 静默落档 postings，**查询永不报错**（spec §4"误判概率实际为零"：随机字节通过 len 上界 + magic + df 匹配的组合概率 ≈ 2^-40 量级）。`docStartFP` 紧邻上一个 term 的 postings 尾部（或上一个 term 的 bitmap len 后缀），那些字节就是"无 bitmap"判定的输入。
5. **count 只读头**（spec §4/§5）：`read_term_bitmap_header` 只做 locate（含 len 上界）+ magic/version + df 匹配后即返回头内 cardinality，不读 payload（crc 校验不了，接受 len+magic+df ≈ 2^-40 的组合误判率；校验失败回落 `doc_freq`，值天然正确）。全量路径 `read_term_bitmap` 读整个 region 做完整四重校验（含 crc32），供迭代与 And/Or 使用。
6. **.doc 读流与 compound**：`PostingsReader::open` 直接按文件名开 `.doc`（postings_read.rs:62），所有枚举经 `fresh_input()` = `self.doc_in.slice(0, self.doc_in.length())`（postings_read.rs:135-137）拿到独立定位的流。bitmap 读路径复用同一模式 → 与 postings 枚举互不干扰。本系统从不产生 CFS（`segment_info.rs:57`），读侧也无 CFS reader，"compound-file slicing" 在此等价于"普通文件 + slice"，天然成立；Java 侧对 Rust 索引做 forceMerge 时工具必须 `setUseCompoundFile(false)` 以保持 Rust 可读（对齐 `JavaLogBench.java:91`）。
7. **Java 不可见论证**（spec §4a，已核实源码）：Java 读 term 永远 `docIn.seek(termState.docStartFP)` 起手（`Lucene912PostingsReader.java:436,809`），skip 导航也 seek 到算好的 fp（:526,559,997），读完 df 个 doc 即停——term 间缝隙字节对 Java 完全隐形；CheckIndex 的 term 枚举与 postings 抽查全走 seek 路径（`CheckIndex.java:916-917` "No problems were detected"）；Java merge 经 PostingsEnum 逐 doc 重编码 → 合并产物**无 bitmap**、自然落档（spec §4a.4）。由 `make log-test --bitmap` 变体的 Java CheckIndex、Java↔Rust 对拍、Java forceMerge 三道实证钉死（spec §8）。
8. **三档执行规则**（spec §5，**按段独立**生效，M1 既定 per-segment 执行）：对 And/Or 的每个 term 子句 `read_term_bitmap`——全部 Some → 档 1 纯容器运算；部分 Some → 档 2 无 bitmap 子句**查询时物化**（postings 全扫置位，df < 4096 ⇒ ≤ 4095 doc ≈ 32 个 PFOR 块，有界）后统一容器运算；全 None → 档 3 现有 PFOR 合取/析取不动。Term：有 bitmap → 迭代走 `RoaringDocIter`、count 走头内 cardinality；无 → 现状。**`needs_freq == true` 一律档 3**（bitmap 无 freq；`FreqSumCollector` 是唯一 `needs_freq` 消费者且 `freq_sum()` 本就不接受 And/Or）。`RL_BITMAP=0` 环境变量在读侧统一关掉 roaring（档 3），与 `RL_SIMD=0`（postings_ll/simd.rs:56-62）同先例，供同二进制 A/B。
9. **容器语义**（Roaring 论文，spec §4）：doc 按高 16 位分桶；桶内 card < 4096 → array（u16 升序），否则 bitset（1024 × u64 = 8KB）；构建后与每次布尔运算后 runOptimize（连续区间转 run：array→run 当 4×runs < 2×card，bitset→run 当 4×runs < 8192，bitset→array 当 card < 4096，run→array 当 card < 4096 且 card < 2×runs）。运算分发（spec §5）：array∩array galloping、bitset∩bitset AVX2（256 bit/指令）+ popcount、run∩run 双指针，Or 对偶；混合对走各自的专用路径（array∩bitset 逐值测位、array∩run 逐 run 二分、bitset∩run 按区间拷字、array∪run 物化成 bitset 等）。
10. **游标协议**：`RoaringCursor` 是纯数据（`{ci, a, b}`，不借用 bitmap），`cursor_next` / `cursor_advance`（只前进；`advance` 语义要求 target > 上次返回值，`DocIter` 的默认 advance 契约保证）。容器内：array 二分（`partition_point`）、bitset `next_set_bit`、run 区间跳。
11. **与 M2 同源物化**：`multi_term::materialize`（>16 terms FixedBitSet 路径）重构到新助手 `multi_term::for_each_doc`（逐 term postings 全扫回调升序 doc），档 2 的 `materialize_clause` 复用同一助手收集 ≤4095 个 doc 后 `RoaringBitmap::from_sorted_docs`（升序前提由枚举器保证）。
12. **写侧参数流**：cli `logwrite --bitmap [--bitmap-threshold N]` → `IndexWriterConfig { bitmap: bool (默认 false), bitmap_threshold: u32 (默认 4096) }` → `IndexWriter::add_document` 建 `SegmentBuilder` 时 `set_bitmap_threshold(Option<u32>)` → `SegmentBuilder::finalize` 里 `PostingsWriter::new(...)?.with_bitmap_threshold(...)`。`logbench`/`bench`/JNI 等直建 `SegmentBuilder` 的路径不传 → 恒 off，零行为变化。

---

## Task 1: Roaring 容器子库标量核（`crates/codec-lucene9/src/roaring.rs` 新建）

**Files:**
- Create: `crates/codec-lucene9/src/roaring.rs`
- Create: `crates/codec-lucene9/src/roaring/simd.rs`（本任务只放空壳 + `pub(super) fn try_bitset_and/or` 返回 `None`，T2 填 AVX2；先建目录使 `mod simd;` 可编译）
- Modify: `crates/codec-lucene9/src/lib.rs`（`pub mod roaring;` + re-export）
- Test: `crates/codec-lucene9/src/roaring.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: `crate::io::{DataOutput, IndexInput, IndexOutput}`（io.rs:518/653/39）；`crc32fast`（codec 已有依赖）。
- Produces（T2/T3/T4/T5 依赖这些名字，不得改名）:
  ```rust
  // roaring.rs
  pub const BITMAP_MAGIC: [u8; 4] = *b"RLBM";
  pub const BITMAP_VERSION: u8 = 1;
  pub const BITMAP_MIN_DF: u32 = 4096;      // spec §5 读侧门槛
  pub const ARRAY_THRESHOLD: usize = 4096;  // Roaring ARRAY_DEFAULT_MAX_SIZE
  pub enum Container { Array(Vec<u16>), Bitset(Box<[u64; 1024]>, u32), Run(Vec<(u16, u16)>, u32) }
  pub struct RoaringBitmap { .. }
  impl RoaringBitmap {
      pub fn from_sorted_docs(docs: &[u32]) -> RoaringBitmap;
      pub fn cardinality(&self) -> u64;
      pub fn is_empty(&self) -> bool;
      pub fn and(&self, other: &RoaringBitmap) -> RoaringBitmap;
      pub fn or(&self, other: &RoaringBitmap) -> RoaringBitmap;
      pub fn cursor(&self) -> RoaringCursor;
      pub fn cursor_next(&self, cur: &mut RoaringCursor) -> Option<u32>;
      pub fn cursor_advance(&self, cur: &mut RoaringCursor, target: u32) -> Option<u32>;
      pub fn serialize(&self, df: u32) -> Vec<u8>;               // 头+payload+crc32（无 len 后缀）
      pub fn deserialize(bytes: &[u8], expected_df: u32) -> Option<RoaringBitmap>;
  }
  #[derive(Clone, Copy, Default)]
  pub struct RoaringCursor { ci: u32, a: u32, b: u32 }
  pub fn max_bitmap_len(max_doc: u32) -> u64;
  pub fn write_term_bitmap(out: &mut impl DataOutput, docs: &[u32]) -> io::Result<()>;
  // lib.rs
  pub mod roaring;
  pub use roaring::RoaringBitmap;
  // roaring/simd.rs（T2 填充；T1 先给标量回落壳）
  pub(super) fn try_bitset_and(a: &[u64; 1024], b: &[u64; 1024], out: &mut [u64; 1024]) -> Option<u32>;
  pub(super) fn try_bitset_or(a: &[u64; 1024], b: &[u64; 1024], out: &mut [u64; 1024]) -> Option<u32>;
  ```

  语义决定：bitmap 只存 doc 集（无 freq/positions，spec §3）；`cardinality == df` 恒成立（写侧保证），`deserialize` 把它当一致性校验执行；`from_sorted_docs` 要求升序输入（写侧 `docs` 切片与物化收集都天然升序，`debug_assert` 钉死）；`and/or` 结果容器同样过 `optimize`（runOptimize 语义，事实 9），且 `and` 不产生空容器（fold 时跳过 card==0 的结果容器，保持 deserialize 的 card ≥ 1 不变式）；空 bitmap（`containers` 为空）合法，`cursor_next` 直接 `None`。

### Steps

- [ ] **Step 1.1: 写失败测试** — 新建 `crates/codec-lucene9/src/roaring.rs`，先只放模块文档注释 + 测试（`RoaringBitmap` 等尚不存在，编译失败即失败测试成立）。同时在 `crates/codec-lucene9/src/lib.rs` 的 `pub mod postings_read;` 之后插入一行 `pub mod roaring;`，在 `pub use postings_read::{...}` 之后插入一行 `pub use roaring::RoaringBitmap;`。新建空壳 `crates/codec-lucene9/src/roaring/simd.rs`：

  ```rust
  //! AVX2 fast paths for bitset-container boolean ops (M3 spec §6). Filled
  //! in Task 2; until then both shims decline and the scalar reference runs.
  //!
  //! ## Safety argument (module-level `allow(unsafe_code)`)
  //!
  //! Same pattern as `postings_ll/simd.rs`: every unsafe op will be an AVX2
  //! intrinsic inside a `#[target_feature(enable = "avx2")]` fn, reached only
  //! through these shims gated on a cached `is_x86_feature_detected!("avx2")`.
  //! All memory access is unaligned load/store on the three fixed
  //! `[u64; 1024]` arrays (256 iterations of 4 words each — in bounds by
  //! construction). Differential tests pin scalar-vs-SIMD word-level equality.

  #![allow(unsafe_code)]

  use super::BITSET_WORDS;

  /// out = a & b, returning cardinality; None when AVX2 is unavailable (the
  /// caller then runs the scalar reference).
  pub(super) fn try_bitset_and(
      _a: &[u64; BITSET_WORDS],
      _b: &[u64; BITSET_WORDS],
      _out: &mut [u64; BITSET_WORDS],
  ) -> Option<u32> {
      None
  }

  /// out = a | b, returning cardinality; None when AVX2 is unavailable.
  pub(super) fn try_bitset_or(
      _a: &[u64; BITSET_WORDS],
      _b: &[u64; BITSET_WORDS],
      _out: &mut [u64; BITSET_WORDS],
  ) -> Option<u32> {
      None
  }
  ```

  `roaring.rs` 的测试模块（完整内容）：

  ```rust
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

      fn to_vec(b: &RoaringBitmap) -> Vec<u32> {
          let mut cur = b.cursor();
          let mut out = Vec::new();
          while let Some(d) = b.cursor_next(&mut cur) {
              out.push(d);
          }
          out
      }

      fn ref_and(a: &[u32], b: &[u32]) -> Vec<u32> {
          let mut out = Vec::new();
          let (mut i, mut j) = (0, 0);
          while i < a.len() && j < b.len() {
              match a[i].cmp(&b[j]) {
                  std::cmp::Ordering::Less => i += 1,
                  std::cmp::Ordering::Greater => j += 1,
                  std::cmp::Ordering::Equal => {
                      out.push(a[i]);
                      i += 1;
                      j += 1;
                  }
              }
          }
          out
      }

      fn ref_or(a: &[u32], b: &[u32]) -> Vec<u32> {
          let mut out = Vec::with_capacity(a.len() + b.len());
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
          out
      }

      /// Sorted unique docs: `n` random low-16 values per listed bucket.
      fn shaped_docs(rng: &mut Rng, buckets: &[(u16, usize)]) -> Vec<u32> {
          let mut docs: Vec<u32> = Vec::new();
          for &(key, n) in buckets {
              for _ in 0..n {
                  docs.push(((key as u32) << 16) | rng.below(65536));
              }
          }
          docs.sort_unstable();
          docs.dedup();
          docs
      }

      #[test]
      fn build_chooses_container_types() {
          // sparse (< 4096 in one bucket) -> Array
          let sparse = shaped_docs(&mut Rng(1), &[(3, 100)]);
          let b = RoaringBitmap::from_sorted_docs(&sparse);
          assert!(matches!(b.containers[0].1, Container::Array(_)));
          assert_eq!(b.cardinality(), sparse.len() as u64);

          // 5000 consecutive values -> runOptimize -> single Run
          let dense: Vec<u32> = (100..5100).collect();
          let b = RoaringBitmap::from_sorted_docs(&dense);
          assert!(matches!(&b.containers[0].1, Container::Run(runs, _) if runs.len() == 1));
          assert_eq!(b.cardinality(), 5000);

          // 6000 scattered values in one bucket -> Bitset (runs too many)
          let scattered = shaped_docs(&mut Rng(7), &[(9, 6000)]);
          let b = RoaringBitmap::from_sorted_docs(&scattered);
          assert!(matches!(b.containers[0].1, Container::Bitset(_, _)));
          assert_eq!(b.cardinality(), scattered.len() as u64);
      }

      #[test]
      fn iteration_round_trip_mixed_containers() {
          let mut docs = shaped_docs(&mut Rng(11), &[(0, 300), (1, 5000), (2, 3)]);
          docs.extend(70_000..75_000u32); // dense consecutive stretch in bucket 1
          docs.sort_unstable();
          docs.dedup();
          let b = RoaringBitmap::from_sorted_docs(&docs);
          assert_eq!(to_vec(&b), docs);
          assert_eq!(b.cardinality(), docs.len() as u64);
          assert!(!b.is_empty());
          assert!(RoaringBitmap::from_sorted_docs(&[]).is_empty());
      }

      #[test]
      fn cursor_advance_matches_linear_scan() {
          let docs = shaped_docs(&mut Rng(13), &[(0, 2000), (1, 6000), (4, 17)]);
          let b = RoaringBitmap::from_sorted_docs(&docs);
          let mut rng = Rng(99);
          // fresh cursor per target: first doc >= target
          for _ in 0..2000 {
              let t = rng.below(300_000);
              let want = docs.iter().find(|&&d| d >= t).copied();
              let mut cur = b.cursor();
              assert_eq!(b.cursor_advance(&mut cur, t), want, "target {t}");
          }
          // interleaved next/advance on one cursor (forward-only contract)
          let mut cur = b.cursor();
          let after = docs.iter().find(|&&d| d >= 10).copied().unwrap();
          assert_eq!(b.cursor_advance(&mut cur, 10), Some(after));
          let want_next = docs.iter().find(|&&d| d > after).copied();
          assert_eq!(b.cursor_next(&mut cur), want_next);
          // advance past the end -> None, sticky
          let mut cur = b.cursor();
          assert_eq!(b.cursor_advance(&mut cur, u32::MAX), None);
          assert_eq!(b.cursor_next(&mut cur), None);
      }

      #[test]
      fn and_or_match_reference_sets() {
          let mut rng = Rng(42);
          // operand pairs exercising every container pair type
          let cases: Vec<(Vec<u32>, Vec<u32>)> = vec![
              // array x array (similar sizes -> merge path)
              (shaped_docs(&mut rng, &[(0, 100)]), shaped_docs(&mut rng, &[(0, 120)])),
              // array x array (skewed -> galloping path)
              (shaped_docs(&mut rng, &[(0, 5)]), shaped_docs(&mut rng, &[(0, 4000)])),
              // run x run (dense consecutive)
              ((0..6000u32).collect(), (3000..9000u32).collect()),
              // bitset x bitset (scattered dense)
              (shaped_docs(&mut rng, &[(2, 6000)]), shaped_docs(&mut rng, &[(2, 7000)])),
              // array x bitset, array x run, bitset x run, multi-bucket
              (shaped_docs(&mut rng, &[(3, 800), (4, 50)]), shaped_docs(&mut rng, &[(3, 6000), (5, 20)])),
              ((10_000..20_000u32).collect(), shaped_docs(&mut rng, &[(0, 5000), (1, 3000)])),
              // disjoint keys, empty operand
              (shaped_docs(&mut rng, &[(7, 100)]), shaped_docs(&mut rng, &[(9, 100)])),
              (vec![], shaped_docs(&mut rng, &[(1, 100)])),
          ];
          for (ci, (a, b)) in cases.iter().enumerate() {
              let ba = RoaringBitmap::from_sorted_docs(a);
              let bb = RoaringBitmap::from_sorted_docs(b);
              let and = ba.and(&bb);
              assert_eq!(to_vec(&and), ref_and(a, b), "and case {ci}");
              assert_eq!(and.cardinality(), ref_and(a, b).len() as u64, "and card {ci}");
              let or = ba.or(&bb);
              assert_eq!(to_vec(&or), ref_or(a, b), "or case {ci}");
              assert_eq!(or.cardinality(), ref_or(a, b).len() as u64, "or card {ci}");
          }
      }

      #[test]
      fn serialize_deserialize_round_trip() {
          let mut rng = Rng(5);
          let cases: Vec<Vec<u32>> = vec![
              shaped_docs(&mut rng, &[(0, 100)]),
              (0..5000u32).collect(),
              shaped_docs(&mut rng, &[(1, 6000), (2, 10), (9, 4500)]),
          ];
          for (ci, docs) in cases.iter().enumerate() {
              let b = RoaringBitmap::from_sorted_docs(docs);
              let bytes = b.serialize(docs.len() as u32);
              let back = RoaringBitmap::deserialize(&bytes, docs.len() as u32)
                  .unwrap_or_else(|| panic!("case {ci} must deserialize"));
              assert_eq!(to_vec(&back), *docs, "case {ci}");
              assert_eq!(back.cardinality(), docs.len() as u64, "case {ci}");
              // wrong expected df -> None
              assert!(RoaringBitmap::deserialize(&bytes, docs.len() as u32 + 1).is_none());
              // corrupted magic / version / crc / payload -> None
              for (pos, tag) in [(0usize, "magic"), (4, "version"), (bytes.len() - 1, "crc"), (bytes.len() / 2, "payload")] {
                  let mut bad = bytes.clone();
                  bad[pos] ^= 0xFF;
                  assert!(RoaringBitmap::deserialize(&bad, docs.len() as u32).is_none(), "case {ci} corrupt {tag}");
              }
              // truncated -> None
              assert!(RoaringBitmap::deserialize(&bytes[..bytes.len() / 2], docs.len() as u32).is_none());
          }
      }

      #[test]
      fn max_bitmap_len_bound_holds() {
          assert_eq!(max_bitmap_len(200_000), 24 + 4 * 8201);
          assert_eq!(max_bitmap_len(1), 24 + 8201);
          let mut rng = Rng(31);
          // 3 containers of scattered dense values (all stay bitsets): bound
          // must hold for every max_doc >= the bitmap's doc space (3*65536)
          let docs = shaped_docs(&mut rng, &[(0, 20_000), (1, 20_000), (2, 20_000)]);
          let b = RoaringBitmap::from_sorted_docs(&docs);
          let len = b.serialize(docs.len() as u32).len() as u64;
          for max_doc in [196_608u32, 200_000, 1_000_000] {
              assert!(len <= max_bitmap_len(max_doc), "len {len} vs max_doc {max_doc}");
          }
          // single bucket (< 65536 docs): bound with exactly 1 container
          let docs = shaped_docs(&mut rng, &[(0, 20_000)]);
          let b = RoaringBitmap::from_sorted_docs(&docs);
          let len = b.serialize(docs.len() as u32).len() as u64;
          assert!(len <= max_bitmap_len(65_536), "len {len} vs single-container bound");
      }

      #[test]
      fn write_term_bitmap_appends_len_suffix() {
          let docs: Vec<u32> = (0..5000).collect();
          let mut out = IndexOutput::in_memory();
          write_term_bitmap(&mut out, &docs).unwrap();
          let bytes = out.into_bytes();
          let n = bytes.len();
          let len = u32::from_le_bytes(bytes[n - 4..].try_into().unwrap()) as usize;
          assert_eq!(len, n - 4);
          let back = RoaringBitmap::deserialize(&bytes[..len], 5000).unwrap();
          assert_eq!(to_vec(&back), docs);
      }
  }
  ```

- [ ] **Step 1.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 roaring 2>&1 | tail -5
  error[E0433]: failed to resolve: use of unresolved module or unlinked crate `roaring`
  （或类似 unresolved 错误：RoaringBitmap 尚不存在）
  ```

- [ ] **Step 1.3: 容器与 bitmap 实现** — `crates/codec-lucene9/src/roaring.rs` 在模块文档注释之后、`#[cfg(test)]` 之前插入（完整实现）：

  ```rust
  use std::io;

  use crate::io::{DataOutput, IndexInput, IndexOutput};

  mod simd;

  /// Bitmap-source read gate (spec §5): only terms with df >= this threshold
  /// attempt the inline-bitmap path.
  pub const BITMAP_MIN_DF: u32 = 4096;
  /// Cardinality below which a container stays/becomes an array
  /// (Roaring's ARRAY_DEFAULT_MAX_SIZE).
  pub const ARRAY_THRESHOLD: usize = 4096;
  /// Bits (and u64 words) per container.
  const BITSET_BITS: usize = 1 << 16;
  const BITSET_WORDS: usize = BITSET_BITS / 64;

  /// One container: the low-16-bit values of one high-16-bit bucket.
  pub enum Container {
      /// Sorted values; len < ARRAY_THRESHOLD after optimization.
      Array(Vec<u16>),
      /// 65536 bits + cached cardinality.
      Bitset(Box<[u64; BITSET_WORDS]>, u32),
      /// Inclusive [start, end] ranges, sorted and non-overlapping, + cached card.
      Run(Vec<(u16, u16)>, u32),
  }

  impl Container {
      fn card(&self) -> u32 {
          match self {
              Container::Array(v) => v.len() as u32,
              Container::Bitset(_, c) => *c,
              Container::Run(_, c) => *c,
          }
      }

      /// runOptimize: convert to a run container when 4B/run strictly beats
      /// the current representation; a bitset with few values degrades to an
      /// array (Roaring's container-size invariants, spec §4 容器语义).
      fn optimize(self) -> Container {
          match self {
              Container::Array(v) => {
                  let runs = array_run_count(&v) as usize;
                  if 4 * runs < 2 * v.len() {
                      let card = v.len() as u32;
                      Container::Run(array_to_runs(&v), card)
                  } else {
                      Container::Array(v)
                  }
              }
              Container::Bitset(w, card) => {
                  if (card as usize) < ARRAY_THRESHOLD {
                      return Container::Array(bitset_to_array(&w, card)).optimize();
                  }
                  let runs = bitset_run_count(&w) as usize;
                  if 4 * runs < 8 * BITSET_WORDS {
                      Container::Run(bitset_to_runs(&w), card)
                  } else {
                      Container::Bitset(w, card)
                  }
              }
              Container::Run(runs, card) => {
                  if (card as usize) < ARRAY_THRESHOLD && (card as usize) < 2 * runs.len() {
                      let mut v = Vec::with_capacity(card as usize);
                      for &(s, e) in &runs {
                          v.extend(s..=e);
                      }
                      Container::Array(v)
                  } else {
                      Container::Run(runs, card)
                  }
              }
          }
      }
  }

  /// Sets bits [s, e] (inclusive) in the word image.
  fn set_range(w: &mut [u64; BITSET_WORDS], s: u16, e: u16) {
      let (lo_w, hi_w) = (s as usize >> 6, e as usize >> 6);
      let lo_bit = s as usize & 63;
      let hi_bit = e as usize & 63;
      if lo_w == hi_w {
          w[lo_w] |= (u64::MAX << lo_bit) & (u64::MAX >> (63 - hi_bit));
          return;
      }
      w[lo_w] |= u64::MAX << lo_bit;
      for word in w.iter_mut().take(hi_w).skip(lo_w + 1) {
          *word = u64::MAX;
      }
      w[hi_w] |= u64::MAX >> (63 - hi_bit);
  }

  fn bit(w: &[u64; BITSET_WORDS], v: u16) -> bool {
      w[v as usize >> 6] >> (v & 63) & 1 == 1
  }

  /// First set bit at position >= `from` in the 65536-bit image.
  fn next_set_bit(w: &[u64; BITSET_WORDS], from: u32) -> Option<u32> {
      if from >= BITSET_BITS as u32 {
          return None;
      }
      let mut wi = from as usize >> 6;
      let mut word = w[wi] & (u64::MAX << (from & 63));
      loop {
          if word != 0 {
              return Some((wi * 64 + word.trailing_zeros() as usize) as u32);
          }
          wi += 1;
          if wi == BITSET_WORDS {
              return None;
          }
          word = w[wi];
      }
  }

  fn bitset_to_array(w: &[u64; BITSET_WORDS], card: u32) -> Vec<u16> {
      let mut v = Vec::with_capacity(card as usize);
      for (i, &word) in w.iter().enumerate() {
          let mut word = word;
          while word != 0 {
              let b = word.trailing_zeros() as usize;
              v.push((i * 64 + b) as u16);
              word &= word - 1;
          }
      }
      v
  }

  /// Number of maximal consecutive runs in an ascending array.
  fn array_run_count(v: &[u16]) -> u32 {
      let mut n = 1u32;
      for i in 1..v.len() {
          if v[i] as u32 != v[i - 1] as u32 + 1 {
              n += 1;
          }
      }
      n
  }

  /// Number of maximal consecutive runs in the 65536-bit image (0→1
  /// transitions across word boundaries).
  fn bitset_run_count(w: &[u64; BITSET_WORDS]) -> u32 {
      let mut runs = 0u32;
      let mut prev_top = 0u64;
      for &word in w {
          if word != 0 {
              // run starts inside this word (bit i set, bit i-1 clear, bit -1 := 0)
              runs += (word & !(word << 1)).count_ones();
              if word & 1 == 1 && prev_top == 1 {
                  runs -= 1; // continuation of the previous word's last run
              }
              prev_top = word >> 63;
          } else {
              prev_top = 0;
          }
      }
      runs
  }

  fn array_to_runs(v: &[u16]) -> Vec<(u16, u16)> {
      let mut runs: Vec<(u16, u16)> = Vec::new();
      for &x in v {
          match runs.last_mut() {
              Some((_, e)) if x as u32 == *e as u32 + 1 => *e = x,
              _ => runs.push((x, x)),
          }
      }
      runs
  }

  fn bitset_to_runs(w: &[u64; BITSET_WORDS]) -> Vec<(u16, u16)> {
      let mut runs = Vec::new();
      let mut start: Option<u16> = None;
      for b in 0..BITSET_BITS as u32 {
          let set = w[b as usize >> 6] >> (b & 63) & 1 == 1;
          match (start, set) {
              (None, true) => start = Some(b as u16),
              (Some(s), false) => {
                  runs.push((s, (b - 1) as u16));
                  start = None;
              }
              _ => {}
          }
      }
      if let Some(s) = start {
          runs.push((s, u16::MAX));
      }
      runs
  }

  // ------------------------------------------------------------------
  // container boolean ops (spec §5: array∩array galloping,
  // bitset∩bitset AVX2 + popcount, run∩run 双指针; Or 对偶)
  // ------------------------------------------------------------------

  /// Intersection of two sorted arrays: galloping from the smaller into the
  /// larger when sizes are skewed, linear merge otherwise.
  fn array_intersect(x: &[u16], y: &[u16]) -> Vec<u16> {
      if x.len() * 32 <= y.len() {
          return galloping_intersect(x, y);
      }
      if y.len() * 32 <= x.len() {
          return galloping_intersect(y, x);
      }
      let mut out = Vec::new();
      let (mut i, mut j) = (0, 0);
      while i < x.len() && j < y.len() {
          match x[i].cmp(&y[j]) {
              std::cmp::Ordering::Less => i += 1,
              std::cmp::Ordering::Greater => j += 1,
              std::cmp::Ordering::Equal => {
                  out.push(x[i]);
                  i += 1;
                  j += 1;
              }
          }
      }
      out
  }

  /// Every value of `small` looked up in `large` by exponential + binary
  /// search (galloping).
  fn galloping_intersect(small: &[u16], large: &[u16]) -> Vec<u16> {
      let mut out = Vec::new();
      let mut lo = 0usize;
      for &v in small {
          // exponential search for the window of `large` that may hold v
          let mut bound = 1usize;
          while lo + bound <= large.len() && large[lo + bound - 1] < v {
              bound <<= 1;
          }
          let hi = (lo + bound).min(large.len());
          let idx = lo + large[lo..hi].partition_point(|&x| x < v);
          if idx == large.len() {
              break;
          }
          if large[idx] == v {
              out.push(v);
          }
          lo = idx;
      }
      out
  }

  /// Values of `x` covered by the runs (both ascending).
  fn array_run_intersect(x: &[u16], runs: &[(u16, u16)]) -> Vec<u16> {
      let mut out = Vec::new();
      let mut ri = 0usize;
      for &v in x {
          while ri < runs.len() && runs[ri].1 < v {
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

  /// Scalar reference for bitset ∩ bitset (the semantic definition the AVX2
  /// kernel is pinned against, spec §6).
  fn bitset_and_scalar(
      x: &[u64; BITSET_WORDS],
      y: &[u64; BITSET_WORDS],
      out: &mut [u64; BITSET_WORDS],
  ) -> u32 {
      let mut card = 0u32;
      for i in 0..BITSET_WORDS {
          let w = x[i] & y[i];
          out[i] = w;
          card += w.count_ones();
      }
      card
  }

  /// Scalar reference for bitset ∪ bitset.
  fn bitset_or_scalar(
      x: &[u64; BITSET_WORDS],
      y: &[u64; BITSET_WORDS],
      out: &mut [u64; BITSET_WORDS],
  ) -> u32 {
      let mut card = 0u32;
      for i in 0..BITSET_WORDS {
          let w = x[i] | y[i];
          out[i] = w;
          card += w.count_ones();
      }
      card
  }

  /// Bitset ∩ run: copies the run ranges' bits out of the bitset.
  fn bitset_run_intersect(w: &[u64; BITSET_WORDS], runs: &[(u16, u16)]) -> Container {
      let mut out = Box::new([0u64; BITSET_WORDS]);
      let mut card = 0u32;
      for &(s, e) in runs {
          let (lo_w, hi_w) = (s as usize >> 6, e as usize >> 6);
          for wi in lo_w..=hi_w {
              let lo_bit = if wi == lo_w { s as usize & 63 } else { 0 };
              let hi_bit = if wi == hi_w { e as usize & 63 } else { 63 };
              let mask = (u64::MAX << lo_bit) & (u64::MAX >> (63 - hi_bit));
              let v = w[wi] & mask;
              out[wi] = v;
              card += v.count_ones();
          }
      }
      Container::Bitset(out, card).optimize()
  }

  /// Interval intersection, two pointers (spec §5: run∩run 双指针).
  fn run_run_intersect(x: &[(u16, u16)], y: &[(u16, u16)]) -> Container {
      let mut out: Vec<(u16, u16)> = Vec::new();
      let mut card = 0u32;
      let (mut i, mut j) = (0usize, 0usize);
      while i < x.len() && j < y.len() {
          let s = x[i].0.max(y[j].0);
          let e = x[i].1.min(y[j].1);
          if s <= e {
              out.push((s, e));
              card += e as u32 - s as u32 + 1;
          }
          if x[i].1 < y[j].1 {
              i += 1;
          } else {
              j += 1;
          }
      }
      Container::Run(out, card).optimize()
  }

  fn container_and(a: &Container, b: &Container) -> Container {
      match (a, b) {
          (Container::Array(x), Container::Array(y)) => {
              Container::Array(array_intersect(x, y)).optimize()
          }
          (Container::Array(x), Container::Bitset(w, _)) => {
              Container::Array(x.iter().copied().filter(|&v| bit(w, v)).collect()).optimize()
          }
          (Container::Bitset(_), Container::Array(_)) => container_and(b, a),
          (Container::Array(x), Container::Run(runs, _)) => {
              Container::Array(array_run_intersect(x, runs)).optimize()
          }
          (Container::Run(_, _), Container::Array(_)) => container_and(b, a),
          (Container::Bitset(x, _), Container::Bitset(y, _)) => {
              let mut out = Box::new([0u64; BITSET_WORDS]);
              let card = match simd::try_bitset_and(x, y, &mut out) {
                  Some(c) => c,
                  None => bitset_and_scalar(x, y, &mut out),
              };
              Container::Bitset(out, card).optimize()
          }
          (Container::Bitset(w, _), Container::Run(runs, _)) => bitset_run_intersect(w, runs),
          (Container::Run(_, _), Container::Bitset(_, _)) => container_and(b, a),
          (Container::Run(x, _), Container::Run(y, _)) => run_run_intersect(x, y),
      }
  }

  /// Union of two sorted arrays.
  fn array_union(x: &[u16], y: &[u16]) -> Vec<u16> {
      let mut out = Vec::with_capacity(x.len() + y.len());
      let (mut i, mut j) = (0, 0);
      while i < x.len() && j < y.len() {
          if x[i] < y[j] {
              out.push(x[i]);
              i += 1;
          } else if x[i] > y[j] {
              out.push(y[j]);
              j += 1;
          } else {
              out.push(x[i]);
              i += 1;
              j += 1;
          }
      }
      out.extend_from_slice(&x[i..]);
      out.extend_from_slice(&y[j..]);
      out
  }

  fn bitset_card(w: &[u64; BITSET_WORDS]) -> u32 {
      w.iter().map(|word| word.count_ones()).sum()
  }

  /// Interval union with coalescing of adjacent ranges.
  fn run_run_union(x: &[(u16, u16)], y: &[(u16, u16)]) -> Container {
      let mut out: Vec<(u16, u16)> = Vec::with_capacity(x.len() + y.len());
      let mut card = 0u32;
      let (mut i, mut j) = (0usize, 0usize);
      while i < x.len() || j < y.len() {
          let iv = if j == y.len() || (i < x.len() && x[i].0 <= y[j].0) {
              let v = x[i];
              i += 1;
              v
          } else {
              let v = y[j];
              j += 1;
              v
          };
          match out.last_mut() {
              Some((_, e)) if iv.0 as u32 <= *e as u32 + 1 => {
                  if iv.1 > *e {
                      card += iv.1 as u32 - *e as u32;
                      *e = iv.1;
                  }
              }
              _ => {
                  card += iv.1 as u32 - iv.0 as u32 + 1;
                  out.push(iv);
              }
          }
      }
      Container::Run(out, card).optimize()
  }

  fn container_or(a: &Container, b: &Container) -> Container {
      match (a, b) {
          (Container::Array(x), Container::Array(y)) => {
              let merged = array_union(x, y);
              if merged.len() >= ARRAY_THRESHOLD {
                  let mut w = Box::new([0u64; BITSET_WORDS]);
                  for &v in &merged {
                      w[v as usize >> 6] |= 1u64 << (v & 63);
                  }
                  Container::Bitset(w, merged.len() as u32).optimize()
              } else {
                  Container::Array(merged).optimize()
              }
          }
          (Container::Array(x), Container::Bitset(w, _))
          | (Container::Bitset(w, _), Container::Array(x)) => {
              let mut out = w.clone();
              for &v in x {
                  out[v as usize >> 6] |= 1u64 << (v & 63);
              }
              let card = bitset_card(&out);
              Container::Bitset(out, card).optimize()
          }
          (Container::Array(x), Container::Run(runs, _)) => {
              let mut w = Box::new([0u64; BITSET_WORDS]);
              for &v in x {
                  w[v as usize >> 6] |= 1u64 << (v & 63);
              }
              for &(s, e) in runs {
                  set_range(&mut w, s, e);
              }
              let card = bitset_card(&w);
              Container::Bitset(w, card).optimize()
          }
          (Container::Run(_, _), Container::Array(_)) => container_or(b, a),
          (Container::Bitset(w, _), Container::Run(runs, _))
          | (Container::Run(runs, _), Container::Bitset(w, _)) => {
              let mut out = w.clone();
              for &(s, e) in runs {
                  set_range(&mut out, s, e);
              }
              let card = bitset_card(&out);
              Container::Bitset(out, card).optimize()
          }
          (Container::Bitset(x, _), Container::Bitset(y, _)) => {
              let mut out = Box::new([0u64; BITSET_WORDS]);
              let card = match simd::try_bitset_or(x, y, &mut out) {
                  Some(c) => c,
                  None => bitset_or_scalar(x, y, &mut out),
              };
              Container::Bitset(out, card).optimize()
          }
          (Container::Run(x, _), Container::Run(y, _)) => run_run_union(x, y),
      }
  }
  ```

- [ ] **Step 1.4: RoaringBitmap + 游标 + 序列化实现** — 紧接 Step 1.3 的代码之后（`#[cfg(test)]` 之前）插入（完整实现）：

  ```rust
  /// A roaring bitmap: containers keyed by the high 16 bits of the docIDs,
  /// sorted by key, at most one container per key (Roaring 论文 §2).
  pub struct RoaringBitmap {
      containers: Vec<(u16, Container)>,
      card: u64,
  }

  impl RoaringBitmap {
      /// Builds from an ascending doc list (the write-side `docs` slice) and
      /// run-optimizes every container (spec §4: 构建后 runOptimize).
      pub fn from_sorted_docs(docs: &[u32]) -> RoaringBitmap {
          debug_assert!(docs.windows(2).all(|w| w[0] < w[1]), "docs must ascend");
          let mut containers: Vec<(u16, Container)> = Vec::new();
          let mut i = 0usize;
          while i < docs.len() {
              let key = (docs[i] >> 16) as u16;
              let mut j = i + 1;
              while j < docs.len() && (docs[j] >> 16) as u16 == key {
                  j += 1;
              }
              let lows: Vec<u16> = docs[i..j].iter().map(|&d| d as u16).collect();
              let c = if lows.len() < ARRAY_THRESHOLD {
                  Container::Array(lows)
              } else {
                  let mut w = Box::new([0u64; BITSET_WORDS]);
                  for &v in &lows {
                      w[v as usize >> 6] |= 1u64 << (v & 63);
                  }
                  Container::Bitset(w, lows.len() as u32)
              };
              containers.push((key, c.optimize()));
              i = j;
          }
          RoaringBitmap {
              containers,
              card: docs.len() as u64,
          }
      }

      /// Total number of docs (== df of the term the bitmap was built for).
      pub fn cardinality(&self) -> u64 {
          self.card
      }

      pub fn is_empty(&self) -> bool {
          self.card == 0
      }

      /// Container-level intersection (spec §5 档 1/2): merge the key sets,
      /// intersect per shared key; the result is itself a roaring bitmap.
      /// Empty result containers are dropped (deserialize 的 card ≥ 1 不变式).
      pub fn and(&self, other: &RoaringBitmap) -> RoaringBitmap {
          let mut containers = Vec::new();
          let mut card = 0u64;
          let (mut i, mut j) = (0usize, 0usize);
          while i < self.containers.len() && j < other.containers.len() {
              let (ka, ca) = &self.containers[i];
              let (kb, cb) = &other.containers[j];
              match ka.cmp(kb) {
                  std::cmp::Ordering::Less => i += 1,
                  std::cmp::Ordering::Greater => j += 1,
                  std::cmp::Ordering::Equal => {
                      let c = container_and(ca, cb);
                      if c.card() > 0 {
                          card += c.card() as u64;
                          containers.push((*ka, c));
                      }
                      i += 1;
                      j += 1;
                  }
              }
          }
          RoaringBitmap { containers, card }
      }

      /// Container-level union; single-side containers are carried over.
      pub fn or(&self, other: &RoaringBitmap) -> RoaringBitmap {
          let mut containers = Vec::new();
          let mut card = 0u64;
          let (mut i, mut j) = (0usize, 0usize);
          while i < self.containers.len() || j < other.containers.len() {
              let (ka, ca) = self.containers.get(i);
              let (kb, cb) = other.containers.get(j);
              match (ka, kb) {
                  (Some((ka, ca)), Some((kb, cb))) => match ka.cmp(kb) {
                      std::cmp::Ordering::Less => {
                          card += ca.card() as u64;
                          containers.push((*ka, clone_container(ca)));
                          i += 1;
                      }
                      std::cmp::Ordering::Greater => {
                          card += cb.card() as u64;
                          containers.push((*kb, clone_container(cb)));
                          j += 1;
                      }
                      std::cmp::Ordering::Equal => {
                          let c = container_or(ca, cb);
                          card += c.card() as u64;
                          containers.push((*ka, c));
                          i += 1;
                          j += 1;
                      }
                  },
                  (Some((ka, ca)), None) => {
                      card += ca.card() as u64;
                      containers.push((*ka, clone_container(ca)));
                      i += 1;
                  }
                  (None, Some((kb, cb))) => {
                      card += cb.card() as u64;
                      containers.push((*kb, clone_container(cb)));
                      j += 1;
                  }
                  (None, None) => unreachable!(),
              }
          }
          RoaringBitmap { containers, card }
      }
  }

  fn clone_container(c: &Container) -> Container {
      match c {
          Container::Array(v) => Container::Array(v.clone()),
          Container::Bitset(w, card) => Container::Bitset(w.clone(), *card),
          Container::Run(runs, card) => Container::Run(runs.clone(), *card),
      }
  }

  /// Plain-data iteration cursor (no borrow of the bitmap, so iterators can
  /// own both). State per current container type: Array → a = next element
  /// index; Bitset → a = next bit to check; Run → a = run index, b = offset
  /// of the next value within the run.
  #[derive(Clone, Copy, Default)]
  pub struct RoaringCursor {
      ci: u32,
      a: u32,
      b: u32,
  }

  impl RoaringBitmap {
      pub fn cursor(&self) -> RoaringCursor {
          RoaringCursor::default()
      }

      /// Next doc at/after the cursor position, or None when exhausted.
      pub fn cursor_next(&self, cur: &mut RoaringCursor) -> Option<u32> {
          loop {
              let (key, c) = self.containers.get(cur.ci as usize)?;
              match c {
                  Container::Array(v) => {
                      if (cur.a as usize) < v.len() {
                          let r = v[cur.a as usize];
                          cur.a += 1;
                          return Some(((*key as u32) << 16) | r as u32);
                      }
                  }
                  Container::Bitset(w, _) => {
                      if let Some(bit) = next_set_bit(w, cur.a) {
                          cur.a = bit + 1;
                          return Some(((*key as u32) << 16) | bit);
                      }
                  }
                  Container::Run(runs, _) => {
                      if (cur.a as usize) < runs.len() {
                          let (s, e) = runs[cur.a as usize];
                          let v = s as u32 + cur.b;
                          if v < e as u32 {
                              cur.b += 1;
                          } else {
                              cur.a += 1;
                              cur.b = 0;
                          }
                          return Some(((*key as u32) << 16) | v);
                      }
                  }
              }
              cur.ci += 1;
              cur.a = 0;
              cur.b = 0;
          }
      }

      /// First doc >= target; the cursor ends positioned past it. Forward
      /// only: callers pass targets greater than the last returned doc (the
      /// DocIter advance contract guarantees this).
      pub fn cursor_advance(&self, cur: &mut RoaringCursor, target: u32) -> Option<u32> {
          let key = (target >> 16) as u16;
          let low = target as u16;
          let ci = self.containers.partition_point(|(k, _)| *k < key);
          if ci > cur.ci as usize {
              cur.ci = ci as u32;
              cur.a = 0;
              cur.b = 0;
          }
          let (k, c) = self.containers.get(cur.ci as usize)?;
          if *k > key {
              // target's bucket absent: first value of the current container
              cur.a = 0;
              cur.b = 0;
              return self.cursor_next(cur);
          }
          match c {
              Container::Array(v) => {
                  let p = v.partition_point(|&x| x < low);
                  cur.a = cur.a.max(p as u32);
              }
              Container::Bitset(_, _) => {
                  cur.a = cur.a.max(low as u32);
              }
              Container::Run(runs, _) => {
                  let p = runs.partition_point(|&(_, e)| e < low);
                  cur.a = cur.a.max(p as u32);
                  if (cur.a as usize) < runs.len() {
                      let (s, _) = runs[cur.a as usize];
                      cur.b = cur.b.max(low.saturating_sub(s) as u32);
                  }
              }
          }
          self.cursor_next(cur)
      }
  }

  // ------------------------------------------------------------------
  // wire format (spec §4; 布局逐项见关键设计事实 2)
  // ------------------------------------------------------------------

  pub const BITMAP_MAGIC: [u8; 4] = *b"RLBM";
  pub const BITMAP_VERSION: u8 = 1;

  const TYPE_ARRAY: u8 = 0;
  const TYPE_BITSET: u8 = 1;
  const TYPE_RUN: u8 = 2;

  /// Upper bound of the bitmap region length (header+payload+crc32) for a
  /// segment with `max_doc` docs. Derivation (spec §4 len 有界校验; 关键设计
  /// 事实 3): runOptimize 后每 container data ≤ 8192B、头部开销 ≤ 9B，
  /// container 数 ≤ ceil(maxDoc/65536)，公共头 ≤ 15B + numContainers vInt
  /// ≤ 5B + crc 4B：
  ///   max_bitmap_len = 24 + ceil(max_doc / 65536) * 8201
  pub fn max_bitmap_len(max_doc: u32) -> u64 {
      24 + (max_doc as u64).div_ceil(65536) * 8201
  }

  impl RoaringBitmap {
      /// header+payload+crc32 (without the trailing len, which the writer
      /// appends). `df` is the term's docFreq and must equal the cardinality
      /// (docs-only bitmap, spec §4).
      pub fn serialize(&self, df: u32) -> Vec<u8> {
          debug_assert_eq!(df as u64, self.card);
          let mut out = IndexOutput::in_memory();
          // in-memory writes never fail (Vec sink)
          out.write_bytes(&BITMAP_MAGIC).unwrap();
          out.write_byte(BITMAP_VERSION).unwrap();
          out.write_vint(df as i32).unwrap();
          out.write_vint(self.card as i32).unwrap();
          out.write_vint(self.containers.len() as i32).unwrap();
          for (key, c) in &self.containers {
              out.write_short(*key as i16).unwrap();
              match c {
                  Container::Array(v) => {
                      out.write_byte(TYPE_ARRAY).unwrap();
                      out.write_vint(v.len() as i32).unwrap();
                      for &x in v {
                          out.write_short(x as i16).unwrap();
                      }
                  }
                  Container::Bitset(w, card) => {
                      out.write_byte(TYPE_BITSET).unwrap();
                      out.write_vint(*card as i32).unwrap();
                      for &word in w.iter() {
                          out.write_long(word as i64).unwrap();
                      }
                  }
                  Container::Run(runs, card) => {
                      out.write_byte(TYPE_RUN).unwrap();
                      out.write_vint(*card as i32).unwrap();
                      out.write_vint(runs.len() as i32).unwrap();
                      for &(s, e) in runs {
                          out.write_short(s as i16).unwrap();
                          out.write_short(e as i16).unwrap();
                      }
                  }
              }
          }
          let mut bytes = out.into_bytes();
          let crc = crc32fast::hash(&bytes);
          bytes.extend_from_slice(&crc.to_le_bytes());
          bytes
      }

      /// Parses + validates a bitmap region (the `len` bytes preceding
      /// docStartFP-4). Returns None on ANY deviation — magic/version
      /// mismatch, df != expected_df, cardinality != df, structural
      /// violation, trailing bytes, or crc32 mismatch — the read side's
      /// silent-fallback signal (spec §4 四重校验).
      pub fn deserialize(bytes: &[u8], expected_df: u32) -> Option<RoaringBitmap> {
          if bytes.len() < 16 {
              return None;
          }
          let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
          let stored_crc = u32::from_le_bytes(crc_bytes.try_into().ok()?);
          if crc32fast::hash(body) != stored_crc {
              return None;
          }
          let mut input = IndexInput::in_memory(body.to_vec());
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
          let num_containers = input.read_vint().ok()?;
          if num_containers < 1 || num_containers > 65536 {
              return None;
          }
          let mut containers = Vec::with_capacity(num_containers as usize);
          let mut card_sum = 0u64;
          let mut last_key: Option<u16> = None;
          for _ in 0..num_containers {
              let key = input.read_short().ok()? as u16;
              if let Some(lk) = last_key {
                  if key <= lk {
                      return None;
                  }
              }
              last_key = Some(key);
              let ty = input.read_byte().ok()?;
              let card = input.read_vint().ok()?;
              if card < 1 {
                  return None;
              }
              let c = match ty {
                  TYPE_ARRAY => {
                      let n = card as usize;
                      if n > BITSET_BITS {
                          return None;
                      }
                      let mut v = Vec::with_capacity(n);
                      let mut last: Option<u16> = None;
                      for _ in 0..n {
                          let x = input.read_short().ok()? as u16;
                          if let Some(l) = last {
                              if x <= l {
                                  return None;
                              }
                          }
                          last = Some(x);
                          v.push(x);
                      }
                      Container::Array(v)
                  }
                  TYPE_BITSET => {
                      let mut w = Box::new([0u64; BITSET_WORDS]);
                      for word in w.iter_mut() {
                          *word = input.read_long().ok()? as u64;
                      }
                      if bitset_card(&w) != card as u32 {
                          return None;
                      }
                      Container::Bitset(w, card as u32)
                  }
                  TYPE_RUN => {
                      let num_runs = input.read_vint().ok()?;
                      if num_runs < 1 {
                          return None;
                      }
                      let mut runs = Vec::with_capacity(num_runs as usize);
                      let mut sum = 0u64;
                      let mut last: Option<(u16, u16)> = None;
                      for _ in 0..num_runs {
                          let s = input.read_short().ok()? as u16;
                          let e = input.read_short().ok()? as u16;
                          if s > e {
                              return None;
                          }
                          if let Some((_, le)) = last {
                              if s <= le {
                                  return None; // overlapping / unsorted
                              }
                          }
                          sum += e as u64 - s as u64 + 1;
                          last = Some((s, e));
                          runs.push((s, e));
                      }
                      if sum != card as u64 {
                          return None;
                      }
                      Container::Run(runs, card as u32)
                  }
                  _ => return None,
              };
              card_sum += c.card() as u64;
              containers.push((key, c));
          }
          if card_sum != card as u64 {
              return None;
          }
          if input.file_pointer() != body.len() as u64 {
              return None; // trailing bytes: not one of our bitmaps
          }
          Some(RoaringBitmap {
              containers,
              card: card_sum,
          })
      }
  }

  /// Builds the bitmap for one term and writes `[region][len: u32 LE]` into
  /// the .doc stream, immediately before the term's postings (spec §4). Must
  /// go through the same checksumming output as the rest of .doc so the
  /// footer CRC stays valid (spec §4a.2; CodecUtil.writeCRC :643-650).
  pub fn write_term_bitmap(out: &mut impl DataOutput, docs: &[u32]) -> io::Result<()> {
      debug_assert!(!docs.is_empty());
      let bitmap = RoaringBitmap::from_sorted_docs(docs);
      let bytes = bitmap.serialize(docs.len() as u32);
      out.write_bytes(&bytes)?;
      out.write_int(bytes.len() as i32)?;
      Ok(())
  }
  ```

  注意：`Container` 的三个变体在 `RoaringBitmap` 里以 `(u16, Container)` 元组按键排序存储；`deserialize` 严格执行结构校验（升序 key、升序 array、不重叠 run、bitset popcount == card、总 card == 头、无尾字节），任何偏差 `None`（随机 postings 字节不可能通过——这正是"无 bitmap 判定"的实现）。

- [ ] **Step 1.5: 跑测试确认通过**

  ```
  $ cargo test -p codec-lucene9 roaring 2>&1 | tail -3
  test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p codec-lucene9 2>&1 | tail -3
  test result: ok. 149 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
  $ cargo fmt --check && echo FMT_OK
  ```

- [ ] **Step 1.6: 提交**

  ```
  git add crates/codec-lucene9/src/roaring.rs crates/codec-lucene9/src/roaring/simd.rs crates/codec-lucene9/src/lib.rs
  git commit -m "feat: roaring container library scalar core (M3 inline bitmap)"
  ```

---

## Task 2: AVX2 bitset 快路径（`crates/codec-lucene9/src/roaring/simd.rs` 填充）

**Files:**
- Modify: `crates/codec-lucene9/src/roaring/simd.rs`（替换 T1 空壳为完整实现 + 对拍测试）
- Test: `crates/codec-lucene9/src/roaring/simd.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T1 的 `BITSET_WORDS`、`bitset_and_scalar` / `bitset_or_scalar`（语义基准；同模块私有项，子模块直接 `use super::` 可见）。
- Produces（签名与 T1 空壳完全一致，不得改名；`container_and/or` 内的调用点不变）:
  ```rust
  pub(super) fn try_bitset_and(a: &[u64; 1024], b: &[u64; 1024], out: &mut [u64; 1024]) -> Option<u32>;
  pub(super) fn try_bitset_or(a: &[u64; 1024], b: &[u64; 1024], out: &mut [u64; 1024]) -> Option<u32>;
  ```

### Steps

- [ ] **Step 2.1: 写失败测试** — 在 `crates/codec-lucene9/src/roaring/simd.rs` 末尾追加测试模块（此时 `try_bitset_and/or` 恒返回 `None`，测试断言 `Some` 路径即失败）：

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::roaring::{bitset_and_scalar, bitset_or_scalar};

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
      }

      /// Operand pairs: all-zero/all-one, ones/ones, alternating,
      /// single-bit, and several seeded densities.
      fn patterns() -> Vec<([u64; BITSET_WORDS], [u64; BITSET_WORDS])> {
          let mut out: Vec<([u64; BITSET_WORDS], [u64; BITSET_WORDS])> = Vec::new();
          let zero = [0u64; BITSET_WORDS];
          let ones = [u64::MAX; BITSET_WORDS];
          out.push((zero, ones));
          out.push((ones, ones));
          let mut alt_a = [0u64; BITSET_WORDS];
          let mut alt_b = [0u64; BITSET_WORDS];
          for i in 0..BITSET_WORDS {
              alt_a[i] = 0xAAAA_AAAA_AAAA_AAAA;
              alt_b[i] = 0x5555_5555_5555_5555;
          }
          out.push((alt_a, alt_b));
          let mut single = [0u64; BITSET_WORDS];
          single[513] = 1 << 37;
          out.push((single, ones));
          for (seed, shift) in [(1u64, 1u32), (7, 7), (42, 32), (0xDEAD, 63)] {
              let mut rng = Rng(seed);
              let mut a = [0u64; BITSET_WORDS];
              let mut b = [0u64; BITSET_WORDS];
              for i in 0..BITSET_WORDS {
                  a[i] = rng.next() >> shift;
                  b[i] = rng.next() >> shift;
              }
              out.push((a, b));
          }
          out
      }

      /// SIMD 纪律对拍（spec §6）：每对操作数，AVX2 与标量参考的输出
      /// 逐 u64 相等、cardinality 相等。
      #[test]
      fn avx2_bitset_and_or_match_scalar() {
          if !avx2_available() {
              eprintln!("AVX2 unavailable on this host, skipping differential test");
              return;
          }
          for (pi, (a, b)) in patterns().iter().enumerate() {
              let mut out_s = [0u64; BITSET_WORDS];
              let mut out_x = [0u64; BITSET_WORDS];
              let c_s = bitset_and_scalar(a, b, &mut out_s);
              let c_x = try_bitset_and(a, b, &mut out_x)
                  .unwrap_or_else(|| panic!("pattern {pi}: AVX2 path must engage"));
              assert_eq!(c_s, c_x, "and card pattern {pi}");
              assert_eq!(out_s, out_x, "and words pattern {pi}");

              let mut out_s = [0u64; BITSET_WORDS];
              let mut out_x = [0u64; BITSET_WORDS];
              let c_s = bitset_or_scalar(a, b, &mut out_s);
              let c_x = try_bitset_or(a, b, &mut out_x)
                  .unwrap_or_else(|| panic!("pattern {pi}: AVX2 path must engage"));
              assert_eq!(c_s, c_x, "or card pattern {pi}");
              assert_eq!(out_s, out_x, "or words pattern {pi}");
          }
      }
  }
  ```

- [ ] **Step 2.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 avx2_bitset 2>&1 | tail -5
  thread 'roaring::simd::tests::avx2_bitset_and_or_match_scalar' panicked at:
  pattern 0: AVX2 path must engage
  ```

- [ ] **Step 2.3: AVX2 实现** — 把 `crates/codec-lucene9/src/roaring/simd.rs` 的两个 shim 替换为完整实现（保留文件头文档注释与 `#![allow(unsafe_code)]`）：

  ```rust
  use std::sync::OnceLock;

  use super::BITSET_WORDS;

  /// Cached one-time AVX2 detection; `RL_SIMD=0` forces the scalar fallback
  /// (same kill switch as postings_ll/simd.rs:56-62, also enables
  /// same-binary A/B benchmarking).
  #[inline]
  fn avx2_available() -> bool {
      static DETECTED: OnceLock<bool> = OnceLock::new();
      *DETECTED.get_or_init(|| {
          std::env::var_os("RL_SIMD").map_or(true, |v| v != "0")
              && std::is_x86_feature_detected!("avx2")
      })
  }

  /// out = a & b, returning cardinality; None when AVX2 is unavailable (the
  /// caller then runs the scalar reference).
  pub(super) fn try_bitset_and(
      a: &[u64; BITSET_WORDS],
      b: &[u64; BITSET_WORDS],
      out: &mut [u64; BITSET_WORDS],
  ) -> Option<u32> {
      if !avx2_available() {
          return None;
      }
      // SAFETY: `avx2_available()` just returned true, so this CPU may
      // execute AVX2 instructions.
      Some(unsafe { bitset_binop_avx2(a, b, out, true) })
  }

  /// out = a | b, returning cardinality; None when AVX2 is unavailable.
  pub(super) fn try_bitset_or(
      a: &[u64; BITSET_WORDS],
      b: &[u64; BITSET_WORDS],
      out: &mut [u64; BITSET_WORDS],
  ) -> Option<u32> {
      if !avx2_available() {
          return None;
      }
      // SAFETY: see `try_bitset_and`.
      Some(unsafe { bitset_binop_avx2(a, b, out, false) })
  }

  /// Vector AND/OR + vertical popcount: the result vector's bytes are counted
  /// via the nibble-LUT (`_mm256_shuffle_epi8`) + `_mm256_sad_epu8` idiom —
  /// per-lane identical math to the scalar `count_ones` loop, pinned by the
  /// differential test below (spec §6 等价追加).
  #[target_feature(enable = "avx2")]
  fn bitset_binop_avx2(
      a: &[u64; BITSET_WORDS],
      b: &[u64; BITSET_WORDS],
      out: &mut [u64; BITSET_WORDS],
      is_and: bool,
  ) -> u32 {
      use std::arch::x86_64::*;
      // SAFETY: loads/stores stay within the three fixed [u64; 1024] arrays
      // (256 iterations of exactly 4 words); see module docs.
      unsafe {
          let lut = _mm256_setr_epi8(
              0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4, 0, 1, 1, 2, 1, 2, 2, 3, 1, 2,
              2, 3, 2, 3, 3, 4,
          );
          let low_mask = _mm256_set1_epi8(0x0f);
          let mut acc = _mm256_setzero_si256();
          for i in (0..BITSET_WORDS).step_by(4) {
              let va = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
              let vb = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
              let v = if is_and {
                  _mm256_and_si256(va, vb)
              } else {
                  _mm256_or_si256(va, vb)
              };
              _mm256_storeu_si256(out.as_mut_ptr().add(i) as *mut __m256i, v);
              let lo = _mm256_and_si256(v, low_mask);
              let hi = _mm256_and_si256(_mm256_srli_epi16(v, 4), low_mask);
              let cnt = _mm256_add_epi8(
                  _mm256_shuffle_epi8(lut, lo),
                  _mm256_shuffle_epi8(lut, hi),
              );
              acc = _mm256_add_epi64(acc, _mm256_sad_epu8(cnt, _mm256_setzero_si256()));
          }
          let mut lanes = [0u64; 4];
          _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, acc);
          (lanes[0] + lanes[1] + lanes[2] + lanes[3]) as u32
      }
  }
  ```

- [ ] **Step 2.4: 跑测试确认通过 + 全量回归**

  ```
  $ cargo test -p codec-lucene9 avx2_bitset 2>&1 | tail -3
  test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ RL_SIMD=0 cargo test -p codec-lucene9 roaring 2>&1 | tail -3
  test result: ok. 7 passed; 0 failed; ...（标量回落下容器库全绿）
  $ cargo test -p codec-lucene9 2>&1 | tail -3
  test result: ok. 150 passed; 0 failed; 1 ignored; ...
  $ cargo fmt --check && echo FMT_OK
  ```

- [ ] **Step 2.5: 提交**

  ```
  git add crates/codec-lucene9/src/roaring/simd.rs
  git commit -m "feat: AVX2 bitset and/or kernels for roaring containers"
  ```

---

## Task 3: 写侧 inline bitmap 产出（postings.rs hook + `--bitmap`/`--bitmap-threshold` 参数流）

**Files:**
- Modify: `crates/codec-lucene9/src/postings.rs`（`PostingsWriter` 加 `bitmap_threshold` 字段 + `with_bitmap_threshold` + `write_term` 内的 bitmap hook）
- Modify: `crates/codec-lucene9/src/postings_read.rs`（仅 `#[cfg(test)]` 追加写侧 round-trip 测试）
- Modify: `crates/core/src/index_writer.rs`（`IndexWriterConfig` 加两个字段 + `add_document` 传递）
- Modify: `crates/core/src/segment_builder.rs`（`bitmap_threshold` 字段 + `set_bitmap_threshold` + finalize 传递）
- Modify: `crates/core/src/bin/rustlucene-cli.rs`（`logwrite` 的 `--bitmap`/`--bitmap-threshold` 解析 + usage）
- Modify: `crates/core/src/search/mod.rs`（`#[cfg(test)]` 追加 bitmap on/off 等价测试）
- Test: `crates/codec-lucene9/src/postings_read.rs` 与 `crates/core/src/search/mod.rs` 的测试模块

**Interfaces:**
- Consumes: T1 的 `crate::roaring::write_term_bitmap` / `RoaringBitmap::deserialize` / `max_bitmap_len`。
- Produces（T4/T6 依赖这些名字，不得改名）:
  ```rust
  // postings.rs
  impl PostingsWriter {
      pub fn with_bitmap_threshold(self, threshold: Option<u32>) -> Self;
  }
  // index_writer.rs
  pub struct IndexWriterConfig {
      pub max_buffered_docs: u32,   // 既有
      pub max_ram_bytes: usize,     // 既有
      pub bitmap: bool,             // 新增，默认 false
      pub bitmap_threshold: u32,    // 新增，默认 4096
  }
  // segment_builder.rs
  impl SegmentBuilder {
      pub fn set_bitmap_threshold(&mut self, threshold: Option<u32>);
  }
  // rustlucene-cli.rs
  //   logwrite <indexDir> <numDocs> <seed> [--positions] [--sparse] [--bigdict]
  //                                        [--bitmap [--bitmap-threshold N]]
  ```

  语义决定：bitmap 只对 `doc_freq >= threshold && doc_freq > 1` 的 term 写出（df==1 的 singleton 在 .doc 中本无 postings，format-notes-postings.md §1）；hook 点严格在 `doc_start_fp` capture 之前（关键设计事实 1）；bitmap 字节经 `self.doc_out`（`ChecksumIndexOutput`）写入，footer CRC 与 .psm `doc_len` 零改动（正确性硬性要求 (d)）；**未传 threshold 时 .doc 字节与 M2 完全一致**（`bitmap_threshold: Option<u32>` 默认 `None`）。

### Steps

- [ ] **Step 3.1: 写失败测试（codec 层）** — 在 `crates/codec-lucene9/src/postings_read.rs` 的 `mod tests` 末尾追加（`with_bitmap_threshold` 尚不存在，编译失败即失败测试成立）：

  ```rust
      /// 与 write_segment 同形，但开 bitmap（threshold=4096）：hot df=5000 命中，
      /// warm df=200 与 big/tail/one 不命中。
      fn write_segment_bitmap(dir: &FSDirectory) -> FieldInfos {
          let id = [4u8; 16];
          let kw = indexed("kw", 0, IndexOptions::Docs);
          let tx = indexed("tx", 1, IndexOptions::DocsAndFreqs);
          let mut w = PostingsWriter::new(dir, "_0", &id)
              .unwrap()
              .with_bitmap_threshold(Some(4096));
          w.start_field(&kw, 6000).unwrap();
          let big: Vec<u32> = (0..200).collect();
          w.write_term(b"big", &big, &vec![1; 200], None).unwrap();
          w.write_term(b"tail", &[10, 20, 30], &[1, 1, 1], None)
              .unwrap();
          w.finish_field().unwrap();
          w.start_field(&tx, 6000).unwrap();
          let hot: Vec<u32> = (0..5000).collect();
          w.write_term(b"hot", &hot, &vec![1; 5000], None).unwrap();
          w.write_term(b"one", &[42], &[7], None).unwrap();
          let warm_docs: Vec<u32> = (0..200).map(|i| i * 3).collect();
          let warm_freqs: Vec<u32> = (0..200).map(|i| (i % 5) + 1).collect();
          w.write_term(b"warm", &warm_docs, &warm_freqs, None)
              .unwrap();
          w.finish_field().unwrap();
          w.finish().unwrap();
          let fis = FieldInfos::new(vec![kw, tx]);
          fis.write(dir, "_0", &id, "").unwrap();
          fis
      }

      /// 不经任何 M3 helper，手工按布局从 .doc 原始字节定位 bitmap region：
      /// fp-4 读 len，region = [fp-4-len, fp-4)。
      fn raw_bitmap_region(dir: &FSDirectory, doc_start_fp: u64) -> Option<Vec<u8>> {
          let mut input = dir.open_input(&crate::postings::file_name("_0", "doc")).unwrap();
          if doc_start_fp < 4 {
              return None;
          }
          input.seek(doc_start_fp - 4).unwrap();
          let len = input.read_int().unwrap() as u32;
          if len == 0 || len as u64 > crate::roaring::max_bitmap_len(6000) {
              return None;
          }
          if doc_start_fp - 4 < len as u64 {
              return None;
          }
          input.seek(doc_start_fp - 4 - len as u64).unwrap();
          let mut buf = vec![0u8; len as usize];
          input.read_bytes(&mut buf).unwrap();
          Some(buf)
      }

      #[test]
      fn inline_bitmap_region_round_trip() {
          let root = temp_dir("bitmap");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_segment_bitmap(&dir);
          // postings 读侧零感知：open 的头/长度/footer 结构校验照常通过，
          // 命中 term 的 postings 逐 doc 不变（缝隙字节不可见，spec §4a.1）。
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
          let e = seek(&dir, &fis, "tx", b"hot");
          let mut en = postings.docs_and_freqs(&e).unwrap();
          for expected in 0..5000 {
              assert_eq!(en.next_doc().unwrap(), expected);
              assert_eq!(en.freq(), 1);
          }
          assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);

          // 命中 term：docStartFP-4 处有合法 bitmap，内容 == postings
          let region = raw_bitmap_region(&dir, e.state.doc_start_fp).expect("hot has a bitmap");
          let bitmap = crate::roaring::RoaringBitmap::deserialize(&region, e.doc_freq)
              .expect("hot bitmap must validate");
          assert_eq!(bitmap.cardinality(), 5000);
          let mut cur = bitmap.cursor();
          for expected in 0..5000u32 {
              assert_eq!(bitmap.cursor_next(&mut cur), Some(expected));
          }
          assert_eq!(bitmap.cursor_next(&mut cur), None);

          // 未命中 term（df=200 < 4096）：同一手工定位流程必须校验失败
          let e = seek(&dir, &fis, "tx", b"warm");
          if let Some(region) = raw_bitmap_region(&dir, e.state.doc_start_fp) {
              assert!(
                  crate::roaring::RoaringBitmap::deserialize(&region, e.doc_freq).is_none(),
                  "random postings bytes must not validate as a bitmap"
              );
          }

          // .doc 全流 CRC（含 bitmap 字节）与 footer 记录一致：
          // ChecksumIndexInput 顺序读完整个文件后 check_footer 通过
          // （CodecUtil.writeCRC :643-650，正确性硬性要求 (d)）。
          let raw = dir.open_input(&crate::postings::file_name("_0", "doc")).unwrap();
          let len = raw.length();
          let mut input = crate::io::ChecksumIndexInput::new(raw);
          input.skip_bytes(len - 16).unwrap();
          crate::codec_util::check_footer(&mut input).unwrap();
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn no_bitmap_written_below_threshold_or_by_default() {
          // 默认构造（不开 bitmap）：hot 的 docStartFP-4 处校验必失败
          let root = temp_dir("bitmap-off");
          let dir = FSDirectory::open(&root).unwrap();
          let (fis, _, _) = write_segment(&dir);
          let e = seek(&dir, &fis, "tx", b"hot");
          if let Some(region) = raw_bitmap_region(&dir, e.state.doc_start_fp) {
              assert!(crate::roaring::RoaringBitmap::deserialize(&region, e.doc_freq).is_none());
          }
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  注：`raw_bitmap_region` 与 CRC 校验直接复用 `FSDirectory::open_input` / `IndexInput::length` / `ChecksumIndexInput` / `codec_util::check_footer` 既有 API（directory.rs:45、io.rs:679、io.rs:944、codec_util.rs）。测试内的 `seek`/`indexed`/`write_segment`/`temp_dir` 均为该测试模块既有 helper。

- [ ] **Step 3.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 inline_bitmap 2>&1 | tail -5
  error[E0599]: no function or associated item named `with_bitmap_threshold` found for struct `PostingsWriter`
  ```

- [ ] **Step 3.3: PostingsWriter 实现** — `crates/codec-lucene9/src/postings.rs` 三处修改：

  ① `PostingsWriter` 结构体（postings.rs:200-218）加字段：

  ```rust
      /// M3 §4: Some(t) → 对 df >= t 的 term 在 postings 之前内联写 roaring
      /// bitmap；None（默认）→ .doc 字节与 M2 完全一致。
      bitmap_threshold: Option<u32>,
  ```

  ② `PostingsWriter::new` 的 `Ok(Self { ... })` 初始化尾部加 `bitmap_threshold: None,`，并紧随 `new` 之后加：

  ```rust
      /// Enables inline roaring bitmaps for terms with df >= threshold
      /// (M3 §4, experimental; off by default). Chain right after `new`.
      pub fn with_bitmap_threshold(mut self, threshold: Option<u32>) -> Self {
          self.bitmap_threshold = threshold;
          self
      }
  ```

  ③ `write_term`（postings.rs:338-370）在 `// --- .doc` 注释之前插入 hook：

  ```rust
          // --- inline roaring bitmap (M3 §4): written BEFORE docStartFP is
          // captured, through the same ChecksumIndexOutput as the rest of
          // .doc, so the footer CRC covers it (spec §4a.2). df==1 singletons
          // have no .doc postings (finishTerm:518-525) and get no bitmap.
          if let Some(t) = self.bitmap_threshold {
              if doc_freq >= t && doc_freq > 1 {
                  crate::roaring::write_term_bitmap(&mut self.doc_out, docs)?;
              }
          }

          // --- .doc
          let doc_start_fp = self.doc_out.file_pointer();
  ```

- [ ] **Step 3.4: 跑 codec 测试确认通过**

  ```
  $ cargo test -p codec-lucene9 bitmap 2>&1 | tail -4
  test result: ok. 2 passed; 0 failed; ...（inline_bitmap_region_round_trip + no_bitmap_written_below_threshold_or_by_default）
  $ cargo test -p codec-lucene9 2>&1 | tail -3
  test result: ok. 152 passed; 0 failed; 1 ignored; ...
  ```

- [ ] **Step 3.5: 提交（codec 写侧）**

  ```
  git add crates/codec-lucene9/src/postings.rs crates/codec-lucene9/src/postings_read.rs
  git commit -m "feat: inline roaring bitmap writer hook in .doc before docStartFP"
  ```

- [ ] **Step 3.6: 写失败测试（core 层）** — 在 `crates/core/src/search/mod.rs` 的 `mod tests` 末尾追加（`IndexWriterConfig` 尚无 `bitmap` 字段，编译失败即失败测试成立）：

  ```rust
      /// M3 写侧：同一语料 bitmap off/on 两个索引，所有查询路径结果逐位一致；
      /// bitmap 索引的 .doc 严格更大（缝隙字节确实写入）。读侧 roaring 接入在
      /// T4/T5，这里验证的是"开了 --bitmap 写，既有读路径（校验失败自然落档
      /// postings）结果完全不变"。
      fn write_bitmap_corpus(root: &std::path::Path, bitmap: bool) {
          let mut cfg = IndexWriterConfig::default();
          cfg.bitmap = bitmap;
          let mut w = IndexWriter::create(root, schema(), cfg).unwrap();
          for i in 0..5000u32 {
              // hot: df=5000 ≥ 4096 → 命中；t0..t6: df≈714 不命中
              w.add_document(doc("INFO", &format!("tid-{i}"), &format!("hot t{}", i % 7)))
                  .unwrap();
          }
          w.commit().unwrap();
          drop(w);
      }

      #[test]
      fn bitmap_write_keeps_all_results_identical() {
          let root_off = temp_dir("bmoff");
          let root_on = temp_dir("bmon");
          write_bitmap_corpus(&root_off, false);
          write_bitmap_corpus(&root_on, true);

          // .doc 尺寸：on > off（hot 的 bitmap + len 后缀）
          let doc_size = |root: &std::path::Path| -> u64 {
              std::fs::read_dir(root)
                  .unwrap()
                  .map(|e| e.unwrap().path())
                  .find(|p| p.extension().map(|x| x == "doc").unwrap_or(false))
                  .map(|p| std::fs::metadata(p).unwrap().len())
                  .unwrap()
          };
          assert!(doc_size(&root_on) > doc_size(&root_off));

          let dir_off = FSDirectory::open(&root_off).unwrap();
          let dir_on = FSDirectory::open(&root_on).unwrap();
          let mut s_off = Searcher::open(&dir_off).unwrap();
          let mut s_on = Searcher::open(&dir_on).unwrap();
          let battery: Vec<Query> = vec![
              Query::term("message", "hot"),
              Query::term("message", "t3"),
              Query::term("level", "INFO"),
              Query::term("tid", "tid-7"),
              Query::and("message", &["hot", "t3"]),
              Query::or("message", &["hot", "t3"]),
              Query::or("message", &["t0", "t1", "t2"]),
              Query::terms("message", &["hot", "t3", "nosuch"]),
              Query::prefix("message", "ho"),
              Query::MatchAll,
          ];
          for q in &battery {
              let (a_total, a_docs) = s_off.top_docs(q, 6000).unwrap();
              let (b_total, b_docs) = s_on.top_docs(q, 6000).unwrap();
              assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
              assert_eq!(s_off.count(q).unwrap(), s_on.count(q).unwrap(), "count {q:?}");
          }
          assert_eq!(
              s_off.freq_sum(&Query::term("message", "hot")).unwrap(),
              s_on.freq_sum(&Query::term("message", "hot")).unwrap()
          );
          fs::remove_dir_all(&root_off).unwrap();
          fs::remove_dir_all(&root_on).unwrap();
      }
  ```

- [ ] **Step 3.7: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core bitmap_write 2>&1 | tail -5
  error[E0609]: no field `bitmap` on type `&mut IndexWriterConfig`
  ```

- [ ] **Step 3.8: 参数流实现（config → builder → postings + CLI）** — 四处修改：

  ① `crates/core/src/index_writer.rs`：`IndexWriterConfig` 加字段 + Default + `add_document` 传递：

  ```rust
  pub struct IndexWriterConfig {
      /// Flush when this many docs are buffered (Lucene default: disabled / RAM-based).
      pub max_buffered_docs: u32,
      /// Flush when the buffered indexing data (postings/docvalues/points
      /// arenas, approximate) exceeds this many bytes. Default 512MB, per the
      /// project spec; Lucene's fair-comparison counterpart is
      /// IndexWriterConfig.setRAMBufferSizeMB.
      pub max_ram_bytes: usize,
      /// M3 §4 (experimental, default off): build inline roaring bitmaps for
      /// terms with df >= `bitmap_threshold` at segment flush. Off ⇒ the
      /// written index is byte-identical to M2.
      pub bitmap: bool,
      /// df threshold for the inline bitmap (spec §4: 4096 对齐 level-1 skip
      /// 粒度 32×128).
      pub bitmap_threshold: u32,
  }

  impl Default for IndexWriterConfig {
      fn default() -> Self {
          Self {
              max_buffered_docs: 1_000_000,
              max_ram_bytes: 512 * 1024 * 1024,
              bitmap: false,
              bitmap_threshold: 4096,
          }
      }
  }
  ```

  `add_document`（index_writer.rs:73-86）的 builder 创建段改为：

  ```rust
      pub fn add_document(&mut self, doc: Document) -> io::Result<()> {
          if self.builder.is_none() {
              let mut b = SegmentBuilder::new(self.dir.clone(), self.segment_counter);
              b.set_bitmap_threshold(if self.config.bitmap {
                  Some(self.config.bitmap_threshold)
              } else {
                  None
              });
              self.builder = Some(b);
              self.segment_counter += 1;
          }
          let b = self.builder.as_mut().unwrap();
          b.add_document(&self.schema, doc)?;
          if b.buffered_docs() >= self.config.max_buffered_docs
              || b.ram_bytes() >= self.config.max_ram_bytes
          {
              self.flush()?;
          }
          Ok(())
      }
  ```

  ② `crates/core/src/segment_builder.rs`：结构体加字段 + `new` 初始化 + setter + finalize 传递：

  ```rust
  pub struct SegmentBuilder {
      dir: FSDirectory,
      seg_name: String,
      seg_id: [u8; 16],
      dw: DocWriter,
      sfw: Option<StoredFieldsWriter>,
      /// M3 §4: Some(t) → finalize 时对 df >= t 的 term 写内联 bitmap。
      bitmap_threshold: Option<u32>,
  }
  ```

  `SegmentBuilder::new` 的 `Self { ... }` 加 `bitmap_threshold: None,`；`buffered_docs` 之后加：

  ```rust
      /// M3 §4: `Some(t)` → write inline roaring bitmaps for terms with
      /// df >= t at finalize; None (default) keeps .doc byte-identical to M2.
      pub fn set_bitmap_threshold(&mut self, threshold: Option<u32>) {
          self.bitmap_threshold = threshold;
      }
  ```

  `finalize` 的解构（segment_builder.rs:90-96）改为：

  ```rust
          let Self {
              dir,
              seg_name,
              seg_id,
              mut dw,
              sfw,
              bitmap_threshold,
          } = self;
  ```

  postings 写出段（segment_builder.rs:152）改为：

  ```rust
              let mut pw = PostingsWriter::new(&dir, &seg_name, &seg_id)?
                  .with_bitmap_threshold(bitmap_threshold);
  ```

  ③ `crates/core/src/bin/rustlucene-cli.rs`：`logwrite` 函数（rustlucene-cli.rs:862-894）改为完整形态：

  ```rust
  /// Single-writer log-schema indexing (the interop counterpart of JavaLogBench).
  /// `bitmap` = Some(threshold) → M3 §4 inline roaring bitmaps (experimental).
  fn logwrite(
      index_dir: &Path,
      num_docs: u32,
      seed: u64,
      positions: bool,
      sparse: bool,
      bigdict: bool,
      bitmap: Option<u32>,
  ) -> std::io::Result<()> {
      let vocab = vocab();
      let mut config = IndexWriterConfig::default();
      if let Some(t) = bitmap {
          config.bitmap = true;
          config.bitmap_threshold = t;
      }
      let mut w = IndexWriter::create(index_dir, log_schema(positions, bigdict), config)?;
      let mut rng = XorShift::new(seed);
      let t0 = Instant::now();
      for doc_id in 0..num_docs {
          w.add_document(gen_log_document(
              &mut rng,
              &vocab,
              doc_id,
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

  `main` 的 `"logwrite"` 分支（rustlucene-cli.rs:1302-1317）改为：

  ```rust
          "logwrite" => {
              if args.len() < 5 {
                  usage();
              }
              let positions = args[5..].iter().any(|a| a == "--positions");
              let sparse = args[5..].iter().any(|a| a == "--sparse");
              let bigdict = args[5..].iter().any(|a| a == "--bigdict");
              let bitmap = if args[5..].iter().any(|a| a == "--bitmap") {
                  let threshold = args[5..]
                      .windows(2)
                      .find_map(|w| {
                          (w[0] == "--bitmap-threshold")
                              .then(|| w[1].parse::<u32>().unwrap_or_else(|_| usage()))
                      })
                      .unwrap_or(4096);
                  Some(threshold)
              } else {
                  None
              };
              logwrite(
                  Path::new(&args[2]),
                  args[3].parse().unwrap(),
                  args[4].parse().unwrap(),
                  positions,
                  sparse,
                  bigdict,
                  bitmap,
              )
          }
  ```

  ④ `usage()` 的 logwrite 行改为：

  ```rust
      eprintln!("  rustlucene-cli logwrite <indexDir> <numDocs> <seed> [--positions] [--sparse] [--bigdict] [--bitmap [--bitmap-threshold N]]");
  ```

- [ ] **Step 3.9: 跑 core 测试确认通过 + CLI 冒烟**

  ```
  $ cargo test -p rustlucene-core bitmap_write 2>&1 | tail -3
  test result: ok. 1 passed; 0 failed; ...
  $ cargo test -p rustlucene-core 2>&1 | tail -3
  test result: ok. 40 passed; 0 failed; ...
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      logwrite /tmp/rl-m3-smoke 200000 42 --bitmap
  WROTE docs=200000 elapsed_ms=... docs_per_sec=...
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      logwrite /tmp/rl-m3-smoke-off 200000 42
  WROTE docs=200000 elapsed_ms=... docs_per_sec=...
  $ du -sb /tmp/rl-m3-smoke /tmp/rl-m3-smoke-off
  （--bitmap 索引严格更大；level 五个 df≈40000 term 的 bitmap 在位）
  $ rm -rf /tmp/rl-m3-smoke /tmp/rl-m3-smoke-off
  $ cargo fmt --check && echo FMT_OK
  ```

- [ ] **Step 3.10: 提交（core 参数流 + CLI）**

  ```
  git add crates/core/src/index_writer.rs crates/core/src/segment_builder.rs crates/core/src/bin/rustlucene-cli.rs crates/core/src/search/mod.rs
  git commit -m "feat: --bitmap/--bitmap-threshold plumbing cli->config->builder->postings"
  ```

---

## Task 4: 读侧 bitmap 定位/校验/读取 + RoaringDocIter + Term 接入（档 1 单查询）

**Files:**
- Modify: `crates/codec-lucene9/src/postings_read.rs`（`read_term_bitmap` / `read_term_bitmap_header` + 私有 `locate_bitmap_region` + 测试）
- Modify: `crates/core/src/search/segment_reader.rs`（`bitmap_enabled` + 两个 `read_term_bitmap*` 包装）
- Modify: `crates/core/src/search/doc_iter.rs`（`RoaringDocIter` + `SegmentDocIter::Roaring` 变体）
- Modify: `crates/core/src/search/query.rs`（Term 分支 roaring 优先）
- Modify: `crates/core/src/search/searcher.rs`（Term count 走头内 cardinality）
- Test: `crates/codec-lucene9/src/postings_read.rs`、`crates/core/src/search/mod.rs` 的测试模块

**Interfaces:**
- Consumes: T1 的 `RoaringBitmap` / `RoaringCursor` / `deserialize` / `max_bitmap_len` / `BITMAP_MAGIC` / `BITMAP_VERSION` / `BITMAP_MIN_DF`；T3 的 `with_bitmap_threshold`（测试造数）。
- Produces（T5/T6/T7 依赖这些名字，不得改名）:
  ```rust
  // postings_read.rs
  impl PostingsReader {
      pub fn read_term_bitmap(&self, entry: &TermEntry, max_doc: u32)
          -> io::Result<Option<RoaringBitmap>>;
      pub fn read_term_bitmap_header(&self, entry: &TermEntry, max_doc: u32)
          -> io::Result<Option<u64>>;
  }
  // segment_reader.rs
  pub(crate) fn bitmap_enabled() -> bool;
  impl SegmentReader {
      pub(crate) fn read_term_bitmap(&self, entry: &TermEntry)
          -> io::Result<Option<RoaringBitmap>>;
      pub(crate) fn read_term_bitmap_header(&self, entry: &TermEntry)
          -> io::Result<Option<u64>>;
  }
  // doc_iter.rs
  pub struct RoaringDocIter { .. }
  impl RoaringDocIter {
      pub fn new(bitmap: RoaringBitmap) -> Self;
  }
  // SegmentDocIter 新增变体 Roaring(RoaringDocIter)
  ```

  语义决定：四重校验任一失败 → `Ok(None)`（静默落档，**永不报错**，spec §4）；只读头路径不做 crc（读不到 payload），接受 len 上界+magic+df 的 ~2^-40 组合误判率（关键设计事实 5）；`RL_BITMAP=0` 时两个 `SegmentReader` 包装函数直接 `Ok(None)` → 全读侧落档 postings（档 3），同二进制 A/B（关键设计事实 8）；bitmap 不含 freq——`needs_freq == true` 的 Term 迭代与 `freq_sum` 完全不走 bitmap（正确性硬性要求 (e)）。

### Steps

- [ ] **Step 4.1: 写失败测试（codec 层）** — 在 `crates/codec-lucene9/src/postings_read.rs` 的 `mod tests` 末尾追加（两个 read 方法尚不存在，编译失败即失败测试成立；`write_segment_bitmap` / `raw_bitmap_region` 复用 T3 的）：

  ```rust
      #[test]
      fn read_term_bitmap_validates_and_reads() {
          let root = temp_dir("bitmap-read");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_segment_bitmap(&dir);
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();

          // 命中 term：全量读 + 四重校验通过，内容 == postings
          let e = seek(&dir, &fis, "tx", b"hot");
          let bitmap = postings
              .read_term_bitmap(&e, 6000)
              .unwrap()
              .expect("hot has a valid bitmap");
          assert_eq!(bitmap.cardinality(), 5000);
          let mut cur = bitmap.cursor();
          for expected in 0..5000u32 {
              assert_eq!(bitmap.cursor_next(&mut cur), Some(expected));
          }
          // count 专用只读头：cardinality == df
          assert_eq!(postings.read_term_bitmap_header(&e, 6000).unwrap(), Some(5000));

          // 未命中 term（df < 4096 读侧门槛）：None，且不做任何读
          let e = seek(&dir, &fis, "tx", b"warm");
          assert!(postings.read_term_bitmap(&e, 6000).unwrap().is_none());
          assert_eq!(postings.read_term_bitmap_header(&e, 6000).unwrap(), None);

          // df 不符（entry 的 df 与 bitmap 头内 df 不同，但 ≥ 门槛以越过
          // 读侧 gate）：校验③失败 → None
          let e = seek(&dir, &fis, "tx", b"hot");
          let mut bad = e;
          bad.doc_freq = 4096; // 真实 df 是 5000；4096 ≥ BITMAP_MIN_DF 故会走到校验③
          assert!(postings.read_term_bitmap(&bad, 6000).unwrap().is_none());
          assert_eq!(postings.read_term_bitmap_header(&bad, 6000).unwrap(), None);

          // 不开 bitmap 写的索引：自然 None（校验①/②失败）
          let root2 = temp_dir("bitmap-read-off");
          let dir2 = FSDirectory::open(&root2).unwrap();
          let (fis2, _, _) = write_segment(&dir2);
          let postings2 = PostingsReader::open(&dir2, "_0", &[4u8; 16]).unwrap();
          let e2 = seek(&dir2, &fis2, "tx", b"hot");
          assert!(postings2.read_term_bitmap(&e2, 6000).unwrap().is_none());
          assert_eq!(postings2.read_term_bitmap_header(&e2, 6000).unwrap(), None);

          fs::remove_dir_all(&root).unwrap();
          fs::remove_dir_all(&root2).unwrap();
      }
  ```

- [ ] **Step 4.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 read_term_bitmap 2>&1 | tail -5
  error[E0599]: no method named `read_term_bitmap` found for struct `PostingsReader`
  ```

- [ ] **Step 4.3: codec 读侧实现** — `crates/codec-lucene9/src/postings_read.rs`：

  ① 头部 import 追加：

  ```rust
  use crate::roaring::{self, RoaringBitmap};
  ```

  ② `impl PostingsReader` 内（`fresh_input` 之后）追加：

  ```rust
      /// Locates + bounds-checks the inline bitmap region for `entry`
      /// (`[docStartFP-4-len, docStartFP-4)`, M3 §4). Returns
      /// Some((region_start, len)). Implements the read gate (df >=
      /// BITMAP_MIN_DF, spec §5) and validation ① (len bound); never seeks
      /// below 0 — `docStartFP >= 4 + len` is required before any read
      /// (正确性硬性要求 b). Shares the caller's positioned stream.
      fn locate_bitmap_region(
          input: &mut IndexInput,
          entry: &TermEntry,
          max_doc: u32,
      ) -> io::Result<Option<(u64, u32)>> {
          if entry.doc_freq < roaring::BITMAP_MIN_DF {
              return Ok(None);
          }
          let fp = entry.state.doc_start_fp;
          if fp < 4 {
              return Ok(None);
          }
          input.seek(fp - 4)?;
          let len = input.read_int()? as u32;
          if len == 0 || len as u64 > roaring::max_bitmap_len(max_doc) {
              return Ok(None);
          }
          if fp - 4 < len as u64 {
              return Ok(None);
          }
          Ok(Some((fp - 4 - len as u64, len)))
      }

      /// Reads + fully validates the inline roaring bitmap preceding this
      /// term's postings (M3 §4): len bound → magic/version → header df ==
      /// termState.doc_freq (+ cardinality == df) → crc32. ANY failure
      /// yields `Ok(None)`, the caller's silent-fallback signal (spec §4:
      /// 静默落档 postings，查询永不报错). Uses its own positioned slice of
      /// the .doc stream (fresh_input pattern, postings_read.rs:135).
      pub fn read_term_bitmap(
          &self,
          entry: &TermEntry,
          max_doc: u32,
      ) -> io::Result<Option<RoaringBitmap>> {
          let mut input = self.fresh_input()?;
          let Some((start, len)) = Self::locate_bitmap_region(&mut input, entry, max_doc)? else {
              return Ok(None);
          };
          input.seek(start)?;
          let mut buf = vec![0u8; len as usize];
          input.read_bytes(&mut buf)?;
          Ok(RoaringBitmap::deserialize(&buf, entry.doc_freq))
      }

      /// Header-only read for count queries (M3 §5: count 查询只读头):
      /// locate (len bound) + magic + version + header df, then return the
      /// header cardinality. The payload crc32 is unreachable without
      /// reading the payload; the combined false-positive probability of the
      /// three applicable checks is ~2^-40 (spec §4 误判概率实际为零), and a
      /// failed check falls back to the caller's doc_freq path.
      pub fn read_term_bitmap_header(
          &self,
          entry: &TermEntry,
          max_doc: u32,
      ) -> io::Result<Option<u64>> {
          let mut input = self.fresh_input()?;
          let Some((start, _len)) = Self::locate_bitmap_region(&mut input, entry, max_doc)? else {
              return Ok(None);
          };
          input.seek(start)?;
          let mut magic = [0u8; 4];
          input.read_bytes(&mut magic)?;
          if magic != roaring::BITMAP_MAGIC {
              return Ok(None);
          }
          if input.read_byte()? != roaring::BITMAP_VERSION {
              return Ok(None);
          }
          if input.read_vint()? as u32 != entry.doc_freq {
              return Ok(None);
          }
          let card = input.read_vint()? as u32;
          if card != entry.doc_freq {
              return Ok(None);
          }
          Ok(Some(card as u64))
      }
  ```

- [ ] **Step 4.4: 跑 codec 测试确认通过**

  ```
  $ cargo test -p codec-lucene9 read_term_bitmap 2>&1 | tail -3
  test result: ok. 1 passed; 0 failed; ...
  $ cargo test -p codec-lucene9 2>&1 | tail -3
  test result: ok. 153 passed; 0 failed; 1 ignored; ...
  ```

- [ ] **Step 4.5: 提交（codec 读侧）**

  ```
  git add crates/codec-lucene9/src/postings_read.rs
  git commit -m "feat: inline bitmap locate/validate/read helpers in PostingsReader"
  ```

- [ ] **Step 4.6: 写失败测试（core 层）** — 在 `crates/core/src/search/mod.rs` 的 `mod tests` 末尾追加（`RoaringDocIter` / `SegmentDocIter::Roaring` 尚不存在，编译失败即失败测试成立；`write_bitmap_corpus` 复用 T3 的）：

  ```rust
      /// M3 档 1 Term 接入：bitmap 索引上 Term 的迭代/count 走 roaring
      /// （迭代器变体断言钉死路径选择），结果与 bitmap off 索引逐位一致；
      /// needs_freq（freq_sum）与未命中 term 永远走 postings。
      #[test]
      fn term_query_uses_roaring_when_bitmap_present() {
          let root_off = temp_dir("bmtoff");
          let root_on = temp_dir("bmton");
          write_bitmap_corpus(&root_off, false);
          write_bitmap_corpus(&root_on, true);

          // 路径断言：bitmap 索引上 hot 的 segment iterator 是 Roaring 变体，
          // needs_freq=true 时回落 postings 变体；off 索引上永远不是 Roaring。
          let dir_on = FSDirectory::open(&root_on).unwrap();
          let mut reader = Reader::open(&dir_on).unwrap();
          let (_base, seg) = reader.leaves().next().unwrap();
          let q = Query::term("message", "hot");
          let it = q.segment_iterator(seg, false).unwrap().unwrap();
          assert!(
              matches!(it, SegmentDocIter::Roaring(_)),
              "hot on bitmap index must take the roaring path"
          );
          let it = q.segment_iterator(seg, true).unwrap().unwrap();
          assert!(
              !matches!(it, SegmentDocIter::Roaring(_)),
              "needs_freq must stay on postings (bitmap carries no freq)"
          );
          let q_low = Query::term("message", "t3");
          let it = q_low.segment_iterator(seg, false).unwrap().unwrap();
          assert!(
              !matches!(it, SegmentDocIter::Roaring(_)),
              "df<4096 term has no bitmap: postings path"
          );
          drop(reader);

          // 全量结果等价（迭代序列、count、freq_sum）
          let dir_off = FSDirectory::open(&root_off).unwrap();
          let mut s_off = Searcher::open(&dir_off).unwrap();
          let dir_on2 = FSDirectory::open(&root_on).unwrap();
          let mut s_on = Searcher::open(&dir_on2).unwrap();
          let q = Query::term("message", "hot");
          let (a_total, a_docs) = s_off.top_docs(&q, 6000).unwrap();
          let (b_total, b_docs) = s_on.top_docs(&q, 6000).unwrap();
          assert_eq!((a_total, a_docs), (b_total, b_docs));
          assert_eq!(b_total, 5000);
          assert_eq!(s_on.count(&q).unwrap(), 5000); // roaring cardinality 路径
          assert_eq!(s_off.count(&q).unwrap(), s_on.count(&q).unwrap());
          assert_eq!(s_on.freq_sum(&q).unwrap(), 5000); // postings，不经 bitmap
          // 未命中 term 与未知 term 不受影响
          assert_eq!(
              s_on.count(&Query::term("message", "t3")).unwrap(),
              s_off.count(&Query::term("message", "t3")).unwrap()
          );
          assert_eq!(s_on.count(&Query::term("message", "nosuch")).unwrap(), 0);
          fs::remove_dir_all(&root_off).unwrap();
          fs::remove_dir_all(&root_on).unwrap();
      }
  ```

- [ ] **Step 4.7: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core term_query_uses_roaring 2>&1 | tail -5
  error[E0599]: no variant or associated item named `Roaring` found for enum `SegmentDocIter`
  ```

- [ ] **Step 4.8: core 实现（segment_reader + doc_iter + query + searcher）** — 四处修改：

  ① `crates/core/src/search/segment_reader.rs`：import 追加 + 三个函数：

  ```rust
  use codec_lucene9::roaring::RoaringBitmap;
  ```

  `impl SegmentReader` 内（`positions_enum` 之后）追加：

  ```rust
      /// Inline-bitmap read for the roaring execution paths (M3 §5): full
      /// four-way validation, None → postings fallback. &self: the bitmap
      /// read uses its own positioned slice of the .doc stream.
      pub(crate) fn read_term_bitmap(&self, entry: &TermEntry) -> io::Result<Option<RoaringBitmap>> {
          if !bitmap_enabled() {
              return Ok(None);
          }
          self.postings.read_term_bitmap(entry, self.max_doc as u32)
      }

      /// Header-only bitmap cardinality for Term count (M3 §5: count 查询
      /// 只读头). None → caller falls back to entry.doc_freq.
      pub(crate) fn read_term_bitmap_header(&self, entry: &TermEntry) -> io::Result<Option<u64>> {
          if !bitmap_enabled() {
              return Ok(None);
          }
          self.postings.read_term_bitmap_header(entry, self.max_doc as u32)
      }
  ```

  文件级（impl 之外）加：

  ```rust
  /// Process-wide kill switch for the roaring read path (M3 §6 A/B
  /// discipline; mirrors RL_SIMD=0 in postings_ll/simd.rs:56-62):
  /// `RL_BITMAP=0` forces the postings fallback everywhere with the same
  /// binary and index.
  pub(crate) fn bitmap_enabled() -> bool {
      static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
      *ENABLED.get_or_init(|| std::env::var_os("RL_BITMAP").map_or(true, |v| v != "0"))
  }
  ```

  ② `crates/core/src/search/doc_iter.rs`：import 追加 + `RoaringDocIter` + `SegmentDocIter` 变体与三个 match 分支：

  ```rust
  use codec_lucene9::roaring::{RoaringBitmap, RoaringCursor};
  ```

  在 `// ── SegmentDocIter ──...` 节之前插入：

  ```rust
  // ── Roaring (inline term bitmap, M3 §5) ───────────────────────────────

  /// DocIter over a validated inline roaring bitmap (M3 §5): next_doc walks
  /// the container cursor; advance hops containers by high-16-bit key and
  /// seeks inside (array partition_point / bitset next_set_bit / run range
  /// skip). freq() is 1 — the bitmap carries no freqs, and needs_freq paths
  /// never get this iterator (correctness requirement (e)).
  pub struct RoaringDocIter {
      bitmap: RoaringBitmap,
      cursor: RoaringCursor,
      doc: i32,
  }

  impl RoaringDocIter {
      pub fn new(bitmap: RoaringBitmap) -> Self {
          let cursor = bitmap.cursor();
          RoaringDocIter {
              bitmap,
              cursor,
              doc: -1,
          }
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
          self.doc = match self.bitmap.cursor_next(&mut self.cursor) {
              Some(d) => d as i32,
              None => NO_MORE_DOCS,
          };
          Ok(self.doc)
      }

      fn advance(&mut self, target: i32) -> io::Result<i32> {
          if target > self.doc {
              self.doc = match self
                  .bitmap
                  .cursor_advance(&mut self.cursor, target.max(0) as u32)
              {
                  Some(d) => d as i32,
                  None => NO_MORE_DOCS,
              };
          }
          Ok(self.doc)
      }
  }
  ```

  `SegmentDocIter` 枚举加变体：

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

  `impl DocIter for SegmentDocIter` 的 `doc_id` / `next_doc` / `advance` 三个 match 各加一支 `Self::Roaring(r) => r.xxx(...)`；`freq` 的 `_ => 1` 臂已覆盖（不加分支）。

  ③ `crates/core/src/search/query.rs`：import 追加 + Term 分支：

  ```rust
  use super::doc_iter::{
      ConjunctionDocIter, DisjunctionDocIter, MatchAllIter, PhraseDocIter, RoaringDocIter,
      SegmentDocIter,
  };
  ```

  Term 分支（query.rs:126-137）改为：

  ```rust
              Query::Term { field, term } => {
                  let Some((has_freqs, entry)) = seg.seek_term(field, term)? else {
                      return Ok(None);
                  };
                  // M3 §5 档 1 term 路径：校验通过的内联 bitmap → roaring
                  // 迭代；needs_freq（bitmap 无 freq）与任何校验失败保持
                  // postings 枚举。
                  if !needs_freq {
                      if let Some(b) = seg.read_term_bitmap(&entry)? {
                          return Ok(Some(SegmentDocIter::Roaring(RoaringDocIter::new(b))));
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

  ④ `crates/core/src/search/searcher.rs`：Term count 分支（searcher.rs:61-69）改为：

  ```rust
          if let Query::Term { field, term } = query {
              let mut total = 0u64;
              for (_doc_base, seg) in self.reader.leaves() {
                  if let Some((_, entry)) = seg.seek_term(field, term)? {
                      // M3 §5: count = bitmap cardinality（只读头）；校验保证
                      // cardinality == df，校验失败回落 doc_freq 短路——两种
                      // 路径的值必然相同，bitmap 路径同时充当线上校验。
                      total += match seg.read_term_bitmap_header(&entry)? {
                          Some(card) => card,
                          None => entry.doc_freq as u64,
                      };
                  }
              }
              return Ok(total);
          }
  ```

- [ ] **Step 4.9: 跑 core 测试确认通过 + 全量回归**

  ```
  $ cargo test -p rustlucene-core term_query_uses_roaring 2>&1 | tail -3
  test result: ok. 1 passed; 0 failed; ...
  $ cargo test -p rustlucene-core 2>&1 | tail -3
  test result: ok. 41 passed; 0 failed; ...
  $ cargo test -p codec-lucene9 2>&1 | tail -3
  test result: ok. 153 passed; 0 failed; 1 ignored; ...
  $ cargo fmt --check && echo FMT_OK
  ```

- [ ] **Step 4.10: 提交**

  ```
  git add crates/core/src/search/segment_reader.rs crates/core/src/search/doc_iter.rs crates/core/src/search/query.rs crates/core/src/search/searcher.rs crates/core/src/search/mod.rs
  git commit -m "feat: RoaringDocIter + Term query roaring integration (tier 1)"
  ```

---

## Task 5: And/Or 三档接入 + 查询时物化 helper（档 1/2）+ roaring count

**Files:**
- Create: `crates/core/src/search/roaring_exec.rs`
- Modify: `crates/core/src/search/multi_term.rs`（抽出 `for_each_doc`，`materialize` 重构复用）
- Modify: `crates/core/src/search/query.rs`（And/Or 分支先走 roaring_exec）
- Modify: `crates/core/src/search/searcher.rs`（And/Or count 三档）
- Modify: `crates/core/src/search/mod.rs`（`pub(crate) mod roaring_exec;` + 语义测试）
- Test: `crates/core/src/search/mod.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T1 的 `RoaringBitmap::{from_sorted_docs, and, or, cardinality}`；T4 的 `SegmentReader::read_term_bitmap`（`RL_BITMAP=0` 开关已内建于此，roaring_exec 不再重复判定）/ `RoaringDocIter` / `SegmentDocIter::Roaring`；既有 `ConjunctionDocIter` / `DisjunctionDocIter` / `multi_term::materialize`。
- Produces（query.rs/searcher.rs 本任务内消费；T6/T7 间接经电池/bench 覆盖）:
  ```rust
  // multi_term.rs
  pub(crate) fn for_each_doc(
      seg: &SegmentReader,
      entry: &TermEntry,
      has_freqs: bool,
      f: &mut impl FnMut(u32),
  ) -> io::Result<()>;
  // roaring_exec.rs（全部 pub(crate)）
  pub(crate) fn collect_bool_entries(
      seg: &mut SegmentReader,
      field: &str,
      terms: &[Vec<u8>],
      is_and: bool,
  ) -> io::Result<Option<(bool, Vec<(u32, TermEntry)>)>>;
  pub(crate) fn segment_iterator(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
      is_and: bool,
  ) -> io::Result<Option<SegmentDocIter>>;
  pub(crate) fn count(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
      is_and: bool,
  ) -> io::Result<Option<u64>>;
  ```

  语义决定（spec §5 逐条落实）：三档判定**按段独立**；`collect_bool_entries` 的 None 覆盖"未知字段 / AND 有缺失子句 / OR 全部缺失"三种空命中（与既有 query.rs 行为一致）；`segment_iterator`/`count` 返回 `None` = 档 3（全子句无 bitmap），调用方回落既有 PFOR 路径；档 2 物化只在 `df < 4096` 的子句上发生（读侧门槛决定无 bitmap 的子句必然 df < 4096），成本有界 ≤4095 doc ≈ 32 个 PFOR 块；`needs_freq == true` 时 query.rs 根本不调 roaring_exec（bitmap 无 freq，正确性硬性要求 (e)）；`freq()` 在 roaring 合取/析取上恒 1（ConstantScore，与 M1 的 And/Or 占位语义一致）。

### Steps

- [ ] **Step 5.1: 写失败测试** — 在 `crates/core/src/search/mod.rs` 的 `mod tests` 末尾追加（`roaring_exec` 尚不存在，编译失败即失败测试成立）。语料：每段 5000 doc，`hot` df=5000（bitmap），`scorching` 在 doc ≥ 500 df=4500（bitmap），`warm{i%7}` df≈714（无 bitmap）：

  ```rust
      /// M3 三档语料：hot 全量、scorching 覆盖 d>=500、warmN 每 7 个一轮。
      /// 每段固定 5000 doc → 多段时各段 df 仍 ≥ 4096（per-segment 判定）。
      fn write_tier_corpus(root: &std::path::Path, bitmap: bool, segments: u32) {
          let mut cfg = IndexWriterConfig::default();
          cfg.bitmap = bitmap;
          let mut w = IndexWriter::create(root, schema(), cfg).unwrap();
          for seg_i in 0..segments {
              for i in 0..5000u32 {
                  let d = seg_i * 5000 + i;
                  let mut msg = String::from("hot");
                  if d >= 500 {
                      msg.push_str(" scorching");
                  }
                  msg.push_str(&format!(" warm{}", d % 7));
                  w.add_document(doc("INFO", &format!("tid-{d}"), &msg)).unwrap();
              }
              w.commit().unwrap(); // 每段独立 flush → per-segment 三档判定
          }
          drop(w);
      }

      /// 三档路径 + on/off 全量等价（spec §8 Rust 对拍的单测形态）。
      #[test]
      fn bool_query_three_tier_roaring() {
          let root_off = temp_dir("tieroff");
          let root_on = temp_dir("tieron");
          write_tier_corpus(&root_off, false, 1);
          write_tier_corpus(&root_on, true, 1);

          // —— 路径断言（bitmap 索引）——
          let dir_on = FSDirectory::open(&root_on).unwrap();
          let mut reader = Reader::open(&dir_on).unwrap();
          let (_base, seg) = reader.leaves().next().unwrap();
          // 档 1：两个子句都有 bitmap
          let q = Query::and("message", &["hot", "scorching"]);
          let it = q.segment_iterator(seg, false).unwrap().unwrap();
          assert!(matches!(it, SegmentDocIter::Roaring(_)), "tier-1 AND must be roaring");
          let q = Query::or("message", &["hot", "scorching"]);
          let it = q.segment_iterator(seg, false).unwrap().unwrap();
          assert!(matches!(it, SegmentDocIter::Roaring(_)), "tier-1 OR must be roaring");
          // 档 2：hot 有 bitmap、warm3 无（df≈714）→ 物化后统一 roaring
          let q = Query::and("message", &["hot", "warm3"]);
          let it = q.segment_iterator(seg, false).unwrap().unwrap();
          assert!(matches!(it, SegmentDocIter::Roaring(_)), "tier-2 mixed AND must be roaring");
          let q = Query::or("message", &["scorching", "warm3"]);
          let it = q.segment_iterator(seg, false).unwrap().unwrap();
          assert!(matches!(it, SegmentDocIter::Roaring(_)), "tier-2 mixed OR must be roaring");
          // 档 3：两个子句都无 bitmap → 既有 PFOR 路径（非 Roaring 变体）
          let q = Query::and("message", &["warm1", "warm3"]);
          let it = q.segment_iterator(seg, false).unwrap().unwrap();
          assert!(!matches!(it, SegmentDocIter::Roaring(_)), "tier-3 stays PFOR conjunction");
          // needs_freq=true：永不走 roaring（bitmap 无 freq）
          let q = Query::and("message", &["hot", "scorching"]);
          let it = q.segment_iterator(seg, true).unwrap().unwrap();
          assert!(!matches!(it, SegmentDocIter::Roaring(_)), "needs_freq stays postings");
          // 缺失子句：AND → None（空结果）；OR → 跳过缺失项后档 1
          let q = Query::and("message", &["hot", "nosuch"]);
          assert!(q.segment_iterator(seg, false).unwrap().is_none());
          let q = Query::or("message", &["scorching", "nosuch"]);
          let it = q.segment_iterator(seg, false).unwrap().unwrap();
          assert!(matches!(it, SegmentDocIter::Roaring(_)), "OR with one present bitmap clause");
          drop(reader);

          // —— on/off 全量等价（count + 完整 doc 序列）——
          let dir_off = FSDirectory::open(&root_off).unwrap();
          let mut s_off = Searcher::open(&dir_off).unwrap();
          let dir_on2 = FSDirectory::open(&root_on).unwrap();
          let mut s_on = Searcher::open(&dir_on2).unwrap();
          let battery: Vec<Query> = vec![
              Query::and("message", &["hot", "scorching"]),          // 档 1 AND
              Query::or("message", &["hot", "scorching"]),           // 档 1 OR
              Query::and("message", &["hot", "warm3"]),              // 档 2 AND
              Query::or("message", &["scorching", "warm3"]),         // 档 2 OR
              Query::or("message", &["hot", "warm0", "warm1"]),      // 档 2 三子句
              Query::and("message", &["hot", "scorching", "warm5"]), // 档 2 三子句
              Query::and("message", &["warm1", "warm3"]),            // 档 3 AND
              Query::or("message", &["warm1", "warm3"]),             // 档 3 OR
              Query::and("message", &["hot", "nosuch"]),             // 空
              Query::or("message", &["scorching", "nosuch"]),        // 单子句有效
          ];
          for q in &battery {
              let (a_total, a_docs) = s_off.top_docs(q, 6000).unwrap();
              let (b_total, b_docs) = s_on.top_docs(q, 6000).unwrap();
              assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
              assert_eq!(s_off.count(q).unwrap(), s_on.count(q).unwrap(), "count {q:?}");
          }
          // 数值锚点（独立推演的期望，防 on/off 同错）：
          let dir_on3 = FSDirectory::open(&root_on).unwrap();
          let mut s = Searcher::open(&dir_on3).unwrap();
          assert_eq!(s.count(&Query::and("message", &["hot", "scorching"])).unwrap(), 4500);
          assert_eq!(s.count(&Query::or("message", &["hot", "scorching"])).unwrap(), 5000);
          assert_eq!(s.count(&Query::and("message", &["hot", "warm3"])).unwrap(), 714);
          fs::remove_dir_all(&root_off).unwrap();
          fs::remove_dir_all(&root_on).unwrap();
      }

      /// 多段：三档判定按段独立（spec §5），docBase 映射不变。
      #[test]
      fn bool_query_roaring_multi_segment() {
          let root_off = temp_dir("tiermsoff");
          let root_on = temp_dir("tiermson");
          // 两段各 5000 doc → 每段 hot df=5000、scorching df=4500，两段都有 bitmap
          write_tier_corpus(&root_off, false, 2);
          write_tier_corpus(&root_on, true, 2);
          let dir_off = FSDirectory::open(&root_off).unwrap();
          let mut s_off = Searcher::open(&dir_off).unwrap();
          let dir_on = FSDirectory::open(&root_on).unwrap();
          let mut s_on = Searcher::open(&dir_on).unwrap();
          assert_eq!(s_on.segment_count(), 2);
          let battery: Vec<Query> = vec![
              Query::and("message", &["hot", "scorching"]),
              Query::or("message", &["hot", "warm3"]),
              Query::and("message", &["hot", "warm3"]),
          ];
          for q in &battery {
              let (a_total, a_docs) = s_off.top_docs(q, 12000).unwrap();
              let (b_total, b_docs) = s_on.top_docs(q, 12000).unwrap();
              assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
              assert_eq!(s_off.count(q).unwrap(), s_on.count(q).unwrap(), "count {q:?}");
          }
          fs::remove_dir_all(&root_off).unwrap();
          fs::remove_dir_all(&root_on).unwrap();
      }
  ```

  注意：`scorching` 只在 d ≥ 500 出现 → 每段内 scorching df = 4500 ≥ 4096 ✓、hot df = 5000 ✓、warmN df ≈ 714 ✓。`hot ∩ warm3`：warm3 的 doc 全部含 hot → 交集 = warm3 的 df = ⌈(5000−3)/7⌉ = 714 ✓。

- [ ] **Step 5.2: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core three_tier 2>&1 | tail -5
  error[E0432]: unresolved import `crate::search::roaring_exec`
  ```

- [ ] **Step 5.3: `for_each_doc` 重构（multi_term.rs）** — 把 `materialize`（multi_term.rs:275-303）重构到共享逐 doc 扫描助手（既有 M2 测试必须保持全绿，行为零变化）：

  ```rust
  /// Per-term postings doc scan shared by the M2 bitset materialization and
  /// the M3 tier-2 roaring materialization: feeds every doc of `entry`'s
  /// postings to `f` in ascending order. Uses no-freq enums on freqs fields —
  /// the materialized sets carry no per-doc freq (ConstantScore).
  pub(crate) fn for_each_doc(
      seg: &SegmentReader,
      entry: &TermEntry,
      has_freqs: bool,
      f: &mut impl FnMut(u32),
  ) -> io::Result<()> {
      if has_freqs {
          let mut en = seg.docs_freqs_enum(entry, false)?;
          loop {
              let d = en.next_doc()?;
              if d == NO_MORE_DOCS {
                  break;
              }
              f(d as u32);
          }
      } else {
          let mut en = seg.docs_enum(entry)?;
          loop {
              let d = en.next_doc()?;
              if d == NO_MORE_DOCS {
                  break;
              }
              f(d as u32);
          }
      }
      Ok(())
  }

  /// Bitset materialization (spec §4): per-term full postings scan, one bit
  /// per hit doc.
  pub(crate) fn materialize(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
  ) -> io::Result<FixedBitSet> {
      let mut bits = FixedBitSet::new(seg.max_doc() as usize);
      for (_, entry) in entries {
          for_each_doc(seg, entry, has_freqs, &mut |d| bits.set(d as usize))?;
      }
      Ok(bits)
  }
  ```

- [ ] **Step 5.4: `roaring_exec.rs` 新建** — 完整内容：

  ```rust
  //! Roaring execution for Boolean queries (spec M3 §5 three-tier rule, per
  //! segment): each term clause's doc source is its validated inline bitmap
  //! when present; clauses without one are materialized at query time (their
  //! df is < 4096 by the read gate, so at most 4095 docs — bounded) and the
  //! whole clause set folds with container-level and/or. When NO clause has
  //! a bitmap the callers fall back to the existing PFOR
  //! conjunction/disjunction untouched (tier 3, M1's tuned path).

  use std::io;

  use codec_lucene9::roaring::RoaringBitmap;
  use codec_lucene9::terms_read::TermEntry;

  use super::doc_iter::{RoaringDocIter, SegmentDocIter};
  use super::multi_term::for_each_doc;
  use super::segment_reader::SegmentReader;

  /// Collects the (df, entry) pairs of an And/Or's term clauses, df-sorted
  /// (conjunction cost order, same as the existing query.rs inline code).
  /// Returns (has_freqs, entries); None = empty segment result: unknown
  /// field, an absent AND clause, or no OR clause present.
  pub(crate) fn collect_bool_entries(
      seg: &mut SegmentReader,
      field: &str,
      terms: &[Vec<u8>],
      is_and: bool,
  ) -> io::Result<Option<(bool, Vec<(u32, TermEntry)>)>> {
      let Some(has_freqs) = seg.field_has_freqs(field) else {
          return Ok(None);
      };
      let mut entries = Vec::with_capacity(terms.len());
      for t in terms {
          match seg.seek_term(field, t)? {
              Some((_, entry)) => entries.push((entry.doc_freq, entry)),
              None => {
                  if is_and {
                      return Ok(None); // missing MUST clause: no hits in this segment
                  }
              }
          }
      }
      if entries.is_empty() {
          return Ok(None);
      }
      entries.sort_by_key(|(df, _)| *df);
      Ok(Some((has_freqs, entries)))
  }

  /// Tier-2 query-time materialization of one low-df clause: full postings
  /// scan into a roaring bitmap (spec §5: df<4096 → ≤4095 docs, bounded).
  /// Docs arrive ascending from the enum, satisfying the build precondition
  /// of `RoaringBitmap::from_sorted_docs`.
  fn materialize_clause(
      seg: &SegmentReader,
      entry: &TermEntry,
      has_freqs: bool,
  ) -> io::Result<RoaringBitmap> {
      let mut docs = Vec::with_capacity(entry.doc_freq as usize);
      for_each_doc(seg, entry, has_freqs, &mut |d| docs.push(d))?;
      Ok(RoaringBitmap::from_sorted_docs(&docs))
  }

  /// Folds clause bitmaps with container and/or (spec §5 档 1/2): probes
  /// every clause's inline bitmap, returns None when NO clause has one
  /// (tier 3), materializes the missing clauses otherwise. The result is a
  /// roaring bitmap — iterated directly, never flattened (spec §5).
  fn fold_clauses(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
      is_and: bool,
  ) -> io::Result<Option<RoaringBitmap>> {
      let mut sources: Vec<Option<RoaringBitmap>> = Vec::with_capacity(entries.len());
      for (_, entry) in entries {
          sources.push(seg.read_term_bitmap(entry)?);
      }
      if sources.iter().all(|s| s.is_none()) {
          return Ok(None); // tier 3: no bitmaps at all in this segment
      }
      for (src, (_, entry)) in sources.iter_mut().zip(entries) {
          if src.is_none() {
              *src = Some(materialize_clause(seg, entry, has_freqs)?);
          }
      }
      let mut it = sources.into_iter().map(Option::unwrap);
      let mut acc = it.next().expect("entries is non-empty");
      for b in it {
          acc = if is_and { acc.and(&b) } else { acc.or(&b) };
      }
      Ok(Some(acc))
  }

  /// Three-tier segment iterator (spec §5): Some = roaring path taken
  /// (tier 1/2); None = tier 3, the caller builds the PFOR iterator.
  pub(crate) fn segment_iterator(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
      is_and: bool,
  ) -> io::Result<Option<SegmentDocIter>> {
      let Some(b) = fold_clauses(seg, entries, has_freqs, is_and)? else {
          return Ok(None);
      };
      Ok(Some(SegmentDocIter::Roaring(RoaringDocIter::new(b))))
  }

  /// Count fast path (spec §5: count = cardinality): cardinality of the
  /// folded bitmap without any doc iteration. None = tier 3, caller iterates.
  pub(crate) fn count(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
      is_and: bool,
  ) -> io::Result<Option<u64>> {
      Ok(fold_clauses(seg, entries, has_freqs, is_and)?.map(|b| b.cardinality()))
  }
  ```

  `crates/core/src/search/mod.rs` 在 `pub mod query;` 之后插入一行 `pub(crate) mod roaring_exec;`。

- [ ] **Step 5.5: query.rs And/Or 分支接入** — `crates/core/src/search/query.rs`：import 追加 + 两个分支替换。

  import 块加：

  ```rust
  use super::roaring_exec;
  ```

  And 分支（query.rs:138-161）改为：

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
                  let Some((has_freqs, entries)) =
                      roaring_exec::collect_bool_entries(seg, field, terms, true)?
                  else {
                      return Ok(None);
                  };
                  // M3 §5 档 1/2（bitmap 无 freq：needs_freq 永远档 3）
                  if !needs_freq {
                      if let Some(it) =
                          roaring_exec::segment_iterator(seg, &entries, has_freqs, true)?
                      {
                          return Ok(Some(it));
                      }
                  }
                  Ok(Some(SegmentDocIter::And(ConjunctionDocIter::new(
                      seg, field, &entries, needs_freq,
                  )?)))
              }
  ```

  Or 分支（query.rs:162-187）改为：

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
                  let Some((has_freqs, entries)) =
                      roaring_exec::collect_bool_entries(seg, field, terms, false)?
                  else {
                      return Ok(None);
                  };
                  // M3 §5 档 1/2
                  if !needs_freq {
                      if let Some(it) =
                          roaring_exec::segment_iterator(seg, &entries, has_freqs, false)?
                      {
                          return Ok(Some(it));
                      }
                  }
                  Ok(Some(SegmentDocIter::Or(DisjunctionDocIter::new(
                      seg, field, &entries, needs_freq,
                  )?)))
              }
  ```

- [ ] **Step 5.6: searcher.rs count 三档** — `crates/core/src/search/searcher.rs`：import 追加 + And/Or 分支（放在 multi-term 分支之后、CountCollector 兜底之前）：

  ```rust
  use super::roaring_exec;
  ```

  ```rust
          // M3 §5: And/Or count 与迭代共用一套三档引擎——任一子句有 bitmap
          // 即折出 cardinality（档 1/2），否则按段迭代（档 3，既有行为）。
          if let Query::And { field, terms } | Query::Or { field, terms } = query {
              if terms.len() >= 2 {
                  let is_and = matches!(query, Query::And { .. });
                  let mut total = 0u64;
                  for (_doc_base, seg) in self.reader.leaves() {
                      let Some((has_freqs, entries)) =
                          roaring_exec::collect_bool_entries(seg, field, terms, is_and)?
                      else {
                          continue; // 空段结果（未知字段 / AND 缺子句 / OR 全缺）
                      };
                      if let Some(c) = roaring_exec::count(seg, &entries, has_freqs, is_and)? {
                          total += c;
                          continue;
                      }
                      if let Some(mut iter) = query.segment_iterator(seg, false)? {
                          loop {
                              let doc = iter.next_doc()?;
                              if doc == NO_MORE_DOCS {
                                  break;
                              }
                              total += 1;
                          }
                      }
                  }
                  return Ok(total);
              }
          }
  ```

  注意：这段必须在 `query.is_multi_term()` 分支**之后**、`let mut c = CountCollector::default();` 兜底**之前**；`is_and` 用 `matches!` 在绑定后取（两个变体共享 `field`/`terms` 绑定）。

- [ ] **Step 5.7: 跑测试确认通过 + 全量回归**

  ```
  $ cargo test -p rustlucene-core tier 2>&1 | tail -3
  test result: ok. 2 passed; 0 failed; ...（three_tier + multi_segment）
  $ cargo test -p rustlucene-core 2>&1 | tail -3
  test result: ok. 43 passed; 0 failed; ...（M2 的 terms/prefix/wildcard 测试
    必须保持全绿——materialize 重构零行为变化）
  $ cargo test -p codec-lucene9 2>&1 | tail -3
  test result: ok. 153 passed; 0 failed; 1 ignored; ...
  $ cargo fmt --check && echo FMT_OK
  ```

- [ ] **Step 5.8: 提交**

  ```
  git add crates/core/src/search/roaring_exec.rs crates/core/src/search/multi_term.rs crates/core/src/search/query.rs crates/core/src/search/searcher.rs crates/core/src/search/mod.rs
  git commit -m "feat: three-tier And/Or roaring execution with query-time materialization"
  ```

---

## Task 6: diff 电池——`make log-test` 第 5 变体 `--bitmap`（含 forceMerge 项）

**Files:**
- Create: `interop/java/ForceMergeIndex.java`
- Modify: `interop/verify-log.sh`（usage 注释 + `--bitmap` 分支块；既有四变体行为零改动）
- Modify: `Makefile`（`log-test` 追加第 5 行，既有四行不动）

**Interfaces:**
- Consumes: T3 的 `logwrite --bitmap`；T4 的 `RL_BITMAP=0` kill switch；T1–T5 全部。
- Produces:
  - `make log-test` 第 5 变体：`interop/verify-log.sh 200000 46 --bitmap`
  - `java -cp "$CP" ForceMergeIndex <indexDir>`（exit 0 + `FORCEMERGE_OK`）

  电池覆盖决定（已核对的语料事实）：200000 文档 log 语料里 level 五个 term df≈40000（≥4096，有 bitmap）、message 各 term df≈2700（<4096，无 bitmap）、trace_id df=1——**单字段 And/Or 在默认阈值下组不出"一高一低"的混合子句**，故档 2 由 T5 单测覆盖，电池覆盖：Term bitmap（`term level=INFO count/first20`）、档 1（`and/or level=INFO,WARN`、`terms level=INFO,WARN,DEBUG` → ≤16 rewrite → `Query::Or` → 档 1）、档 3（`and/or message=connection0,query23`）、无 bitmap 自然落档（Java 侧索引、forceMerge 产物）、以及 bitmap on/off 全电池逐位一致。`JavaLogBench` 收到 `--bitmap` 实参但忽略（其参数循环只识别已知 flag，`JavaLogBench.java:72-76`）→ Java 侧索引与既有变体字节级一致，diff 基准不变。forceMerge 后的 `searchdump` 是 Rust 读侧首次端到端消费 Java 写出的索引：`segments_N` 的 codec 名校验（segment_infos.rs:238-241）只接受 `"Lucene912"`（Java 默认 codec 同名）、`.si/.fnm/.tim/.doc` 布局两侧一致、Searcher 不打开 DV/points/stored 文件——预期一次通过；若失败即为真实 codec 缺口，必须修到通过（验收门槛）。

### Steps

- [ ] **Step 6.1: `interop/java/ForceMergeIndex.java` 新建** — 完整内容：

  ```java
  import java.nio.file.*;
  import org.apache.lucene.analysis.core.WhitespaceAnalyzer;
  import org.apache.lucene.index.*;
  import org.apache.lucene.store.*;

  /**
   * M3 battery tool: forceMerge(1) an existing index in place (used on a
   * Rust-written --bitmap index). The merge re-encodes postings via
   * PostingsEnum, so the merged segment carries no inline bitmaps (spec
   * §4a.4) and the Rust read side naturally falls back to postings (tier 3);
   * the search battery re-run afterwards must produce identical results.
   * Non-compound output (matches JavaLogBench's setUseCompoundFile(false),
   * JavaLogBench.java:91) — the Rust reader has no CFS support.
   *
   * Usage: ForceMergeIndex <indexDir>
   */
  public class ForceMergeIndex {
      public static void main(String[] args) throws Exception {
          Path indexDir = Paths.get(args[0]);
          try (Directory dir = FSDirectory.open(indexDir)) {
              IndexWriterConfig cfg = new IndexWriterConfig(new WhitespaceAnalyzer())
                  .setOpenMode(IndexWriterConfig.OpenMode.APPEND)
                  .setUseCompoundFile(false);
              try (IndexWriter w = new IndexWriter(dir, cfg)) {
                  w.forceMerge(1);
                  w.commit();
              }
          }
          System.out.println("FORCEMERGE_OK");
      }
  }
  ```

- [ ] **Step 6.2: `interop/verify-log.sh` 修改** — 两处：

  ① 头部 usage 注释（第 5 行）改为：

  ```bash
  # Usage: interop/verify-log.sh [numDocs] [seed] [--positions|--sparse|--bigdict|--bitmap]
  ```

  ② `echo "LOG_INTEROP_OK"` 之前插入 `--bitmap` 分支块（既有四变体的 `$3` 取值均非 `--bitmap`，行为零改动）：

  ```bash
  if [ "$POSITIONS" = "--bitmap" ]; then
    echo "== Bitmap A/B: Rust searchdump bitmap on vs off (RL_BITMAP=0)"
    cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" > /tmp/rl-search-bitmap-on.out
    RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" > /tmp/rl-search-bitmap-off.out
    diff -u /tmp/rl-search-bitmap-on.out /tmp/rl-search-bitmap-off.out

    echo "== Java forceMerge on the Rust --bitmap index"
    java -cp "$CP" ForceMergeIndex "$RUST_DIR"
    echo "== CheckIndex post-merge $RUST_DIR"
    java -cp "$CP" org.apache.lucene.index.CheckIndex "$RUST_DIR" 2>&1 \
      | grep -E "No problems|FAILED|error" || true
    java -cp "$CP" org.apache.lucene.index.CheckIndex "$RUST_DIR" > /dev/null 2>&1

    echo "== Post-merge search diff (merged index carries no bitmaps)"
    "$ROOT/interop/verify-search.sh" "$RUST_DIR" "$JAVA_DIR" "$NUM_DOCS" "$SEED" ""
  fi
  ```

  说明：A/B 段在 forceMerge **之前**（bitmap 还在）；`verify-search.sh` 第 5 实参传 `""`（merge 后非 positions 形态）；post-merge 的 CheckIndex 复用本脚本既有的"grep 展示 + 裸跑 gate 退出码"双调用模式（verify-log.sh:27-30）。`verify-search.sh` 与 `VerifySearchIndex.java` 零改动（它们对未知第 5 实参的处理与 `--sparse`/`--bigdict` 变体相同：忽略）。

- [ ] **Step 6.3: `Makefile` 修改** — `log-test` 目标（Makefile:17-21）追加第 5 行（既有四行逐字不动）：

  ```make
  # M2 log-schema interop: Rust logwrite vs JavaLogBench, CheckIndex + query diff
  log-test: build java-classes
  	interop/verify-log.sh 200000 42
  	interop/verify-log.sh 200000 43 --positions
  	interop/verify-log.sh 200000 44 --sparse
  	interop/verify-log.sh 200000 45 --bigdict
  	interop/verify-log.sh 200000 46 --bitmap
  ```

- [ ] **Step 6.4: 编译 + 先单跑 `--bitmap` 变体**

  ```
  $ make java-classes 2>&1 | tail -1
  $ interop/verify-log.sh 200000 46 --bitmap
  == Rust: logwrite (200000 docs, seed 46 --bitmap)
  WROTE docs=200000 elapsed_ms=... docs_per_sec=...
  == Java: JavaLogBench (same corpus)
  == CheckIndex /tmp/rl-log-rust
  No problems were detected with this index.        ← bitmap 内联后 Java 零感知
  == CheckIndex /tmp/rl-log-java
  No problems were detected with this index.
  == VerifyLogIndex: Rust vs Java dumps
  （diff 为空，输出逐行一致）
  == Search diff: searchdump vs VerifySearchIndex
  （diff 为空）
  SEARCH_INTEROP_OK
  == Bitmap A/B: Rust searchdump bitmap on vs off (RL_BITMAP=0)
  （diff 为空——on/off 全电池逐位一致）
  == Java forceMerge on the Rust --bitmap index
  FORCEMERGE_OK
  == CheckIndex post-merge /tmp/rl-log-rust
  No problems were detected with this index.
  == Post-merge search diff (merged index carries no bitmaps)
  （diff 为空——merge 产物无 bitmap 自然落档，结果不变）
  SEARCH_INTEROP_OK
  LOG_INTEROP_OK
  ```

- [ ] **Step 6.5: `make log-test` 五变体全绿（验收门槛）**

  ```
  $ make log-test
      （既有 4 变体输出与 M2 收尾时逐字节一致——--bitmap 默认 off，写侧零变化；
        第 5 变体输出如 Step 6.4。全部 CheckIndex "No problems"。）
  ```

- [ ] **Step 6.6: 提交**

  ```
  git add interop/java/ForceMergeIndex.java interop/verify-log.sh Makefile
  git commit -m "test: log-test --bitmap variant with on/off A/B and Java forceMerge re-verify"
  ```

---

## Task 7: searchbench 三路 bench（--no-cache）+ 写侧开销测量 + 报告

searchbench **无需代码改动**（"if it fits existing structure" 的判定结论）：既有结构已完整覆盖 M3 所需——`term` 行走 `Searcher::count`（T4 后 = roaring 头内 cardinality vs PFOR 的 doc_freq O(1)）、`iterm` 行走强制迭代（T4 后 = `RoaringDocIter` vs PFOR 解码）、`and`/`or` 行走 T5 三档（bitmap 索引上 = 档 1 容器折叠 vs PFOR 合取/析取）；on/off A/B 由 `RL_BITMAP=0` 环境变量提供（照 `RL_SIMD=0` 先例，同二进制同索引）。1000000 文档 log 语料 message 各 term df≈13700（≥4096），`--dump-queries` 产出的 high bucket 行全部 roaring 命中。

**Files:**
- Modify: 无（代码零改动；产物全部 gitignored）
- Produces（`.superpowers/sdd/`，不进 git）:
  ```
  .superpowers/sdd/m3-q.txt
  .superpowers/sdd/m3-bench-{rust-roaring,rust-pfor,java}.out
  .superpowers/sdd/m3-counts-{rust-roaring,rust-pfor,java}.txt
  .superpowers/sdd/m3-write-throughput.txt
  .superpowers/sdd/m3-roaring-bench-report.md
  ```

### Steps

- [ ] **Step 7.1: 建 bench 索引（1M docs，Rust --bitmap + Java）**

  ```
  $ CP="interop/java/classes:interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar"
  $ mkdir -p .superpowers/sdd
  $ rm -rf /tmp/rl-bench3-rust /tmp/rl-bench3-java
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      logwrite /tmp/rl-bench3-rust 1000000 42 --bitmap
  WROTE docs=1000000 elapsed_ms=... docs_per_sec=...
  $ java -cp "$CP" JavaLogBench /tmp/rl-bench3-java 1000000 1 42
  BENCH elapsed_ms=... docs_per_sec=... ...
  ```

- [ ] **Step 7.2: 生成查询文件（Java 侧 dump，high bucket 全部 df≥4096）**

  ```
  $ java -cp "$CP" SearchBench /tmp/rl-bench3-java message \
      --dump-queries .superpowers/sdd/m3-q.txt --tasks 50 --seed 42
  DUMPED terms to .superpowers/sdd/m3-q.txt
  $ awk -F'\t' '$1=="TERM" && $4<4096 {print "LOW-DF LINE:", $0; bad=1} END {exit bad}' \
      .superpowers/sdd/m3-q.txt && echo "ALL TERM LINES df>=4096"
  ALL TERM LINES df>=4096
  $ grep -c $'^AND\thigh' .superpowers/sdd/m3-q.txt
  50
  ```

  （`awk` 守卫：dump 文件里任何 df<4096 的 TERM 行都会让该命令失败——此时应剔除低 df 行或改语料重跑 Step 7.1；1M log 语料下预期不发生。）

- [ ] **Step 7.3: 三路 bench + hit-counts 对拍（验收门槛：两个 diff 皆空）**

  ```
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchbench /tmp/rl-bench3-rust message \
      --load-queries .superpowers/sdd/m3-q.txt --warmup 10 --iter 30 \
      > .superpowers/sdd/m3-bench-rust-roaring.out 2> .superpowers/sdd/m3-counts-rust-roaring.txt
  $ RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchbench /tmp/rl-bench3-rust message \
      --load-queries .superpowers/sdd/m3-q.txt --warmup 10 --iter 30 \
      > .superpowers/sdd/m3-bench-rust-pfor.out 2> .superpowers/sdd/m3-counts-rust-pfor.txt
  $ java -cp "$CP" SearchBench /tmp/rl-bench3-java message \
      --load-queries .superpowers/sdd/m3-q.txt --no-cache --warmup 10 --iter 30 \
      > .superpowers/sdd/m3-bench-java.out 2> .superpowers/sdd/m3-counts-java.txt
  $ diff .superpowers/sdd/m3-counts-rust-roaring.txt .superpowers/sdd/m3-counts-rust-pfor.txt \
      && echo "COUNTS_MATCH: roaring == pfor"
  COUNTS_MATCH: roaring == pfor
  $ diff .superpowers/sdd/m3-counts-rust-roaring.txt .superpowers/sdd/m3-counts-java.txt \
      && echo "COUNTS_MATCH: roaring == java"
  COUNTS_MATCH: roaring == java
  ```

  （Java 侧 stderr 的 per-query count 行格式与 Rust 一致——M2 的 `m2-counts-java.txt` 已验证同格式可对拍。）

- [ ] **Step 7.4: 写侧吞吐损失 + 磁盘增量测量**

  ```
  $ rm -rf /tmp/rl-bench3-wt-bm /tmp/rl-bench3-wt-off
  $ { echo "== with --bitmap:"; \
      cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
        logwrite /tmp/rl-bench3-wt-bm 1000000 42 --bitmap; \
      echo "== without:"; \
      cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
        logwrite /tmp/rl-bench3-wt-off 1000000 42; \
      echo "== index sizes:"; du -sb /tmp/rl-bench3-wt-bm /tmp/rl-bench3-wt-off; \
      echo "== .doc sizes:"; ls -l /tmp/rl-bench3-wt-bm/*_Lucene912_0.doc /tmp/rl-bench3-wt-off/*_Lucene912_0.doc; \
    } | tee .superpowers/sdd/m3-write-throughput.txt
  ```

  报告用数据：两侧 `docs_per_sec` 比值（写侧 CPU 损失，预期 < 5%）、`du -sb` 与 `.doc` 尺寸差（bitmap 磁盘增量：200000 文档时仅 level 5 个 term 命中，1M 文档时 level 5 个 + 无 message 命中——注意 message 各 term df≈13700 ≥ 4096 在 **1M** 语料下也命中 bitmap，磁盘增量预期 < 5%）。

- [ ] **Step 7.5: bench 报告落盘** — 写 `.superpowers/sdd/m3-roaring-bench-report.md`（gitignored，不进 git），结构：

  1. **口径**：`--no-cache`（Java 侧 flag；Rust 无 query cache 天然等价）、1000000 docs、seed 42、`--warmup 10 --iter 30`、host/CPU 一行。
  2. **三路对比表**：每个 query_type × bucket 分组行（term/and/or/iterm × high；low/med 在本语料为空则注明），列 = rust-roaring qps/p50/p90/p99、rust-pfor 同四项、java 同四项，外加 `roaring/pfor` 与 `roaring/java` 两个 qps 比值。
  3. **结论**：对照 spec §1 预期（高 df AND 提速一个数量级）记录实测比值；`term`（count）行说明 roaring 只读头 vs PFOR doc_freq O(1) 的实测差异；`iterm` 行说明容器迭代 vs PFOR 解码的实测差异。
  4. **写侧开销**：Step 7.4 的 docs_per_sec 比值与 .doc/索引尺寸增量百分比。
  5. **正确性**：Step 7.3 两个 `COUNTS_MATCH` 原样引用。

- [ ] **Step 7.6: 最终验收确认（无代码变更，无需提交）**

  ```
  $ cargo fmt --check && echo FMT_OK
  $ cargo test -p codec-lucene9 2>&1 | tail -1
  test result: ok. 153 passed; 0 failed; 1 ignored; ...
  $ cargo test -p rustlucene-core 2>&1 | tail -1
  test result: ok. 43 passed; 0 failed; ...
  $ git status --short
  （只有 .superpowers/（gitignored，不显示）与 bench 临时目录（/tmp）产物；
    工作树干净）
  ```

  验收清单（全部满足才算 M3 完成）：
  - [ ] `cargo fmt --check` 干净；`cargo test` 两 crate 全绿（T1–T5）。
  - [ ] `make log-test` 五变体全绿（T6），每次 CheckIndex 输出 "No problems were detected with this index."。
  - [ ] `--bitmap` 变体内：Rust bitmap on/off searchdump diff 为空；Java forceMerge 后 CheckIndex "No problems" 且 Java↔Rust searchdump diff 为空（T6）。
  - [ ] bench：hit-counts 三路对拍 diff 皆空（T7 Step 7.3）；报告落盘含 roaring vs PFOR vs Java 实测比值与写侧开销（T7 Step 7.5）。
