# M2 搜索读路径（multi-term 查询 + Phrase）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 M1（Term/MatchAll/And/Or + skip-driven advance + SIMD 解码）之上新增 Terms(IN)/Prefix/Wildcard/Phrase(slop=0) 四类查询（全部 ConstantScore、无评分），配套 FixedBitSet 物化执行路径、block-tree 顺序枚举器（TermsIter）、positions 读侧（PositionsEnum），并把 diff 电池与 searchbench 扩展到新查询类型，`make log-test` 四变体全绿收尾。对应已批准 spec `docs/superpowers/specs/2026-07-23-rust-search-m2-multiterm-phrase-design.md` 的全部范围（§9 用户拍板顺序）。

**Architecture:** 延续方案 C（算法语义照抄 9.12.3、对象结构 Rust 化）。multi-term 归一为"term 集合 → doc 集合"：展开 ≤16 个 term 时 rewrite 成现有 `Query::Or`（DisjunctionDocIter 合并，零新执行代码），>16 时 per-segment `FixedBitSet` 物化 + `BitsetDocIter`（count 走 popcount），不设枚举上限（spec §4）。Prefix/Wildcard 由 codec 层新追加的 `TermsIter`（block-tree frame 栈枚举器，照 `SegmentTermsEnum`）收集 term 集合后走同一双路。Phrase 走新追加的 `PositionsEnum`（照 `Lucene912PostingsReader.EverythingEnum`，含 skip entry 的 `(pos_fp delta, pos_buffer_upto)` 重同步），doc 合取命中后按 `pos[i] - pos[0] == offset[i]` 验证（slop=0）。验证三层不变：round-trip 单测（codec）→ Rust 语义测试（core `search/mod.rs`）→ Java diff 终验（`searchdump` ↔ `VerifySearchIndex`，挂 `make log-test`）。

**Tech Stack:** Rust（codec crate edition 2024、core crate edition 2021；codec `#![deny(unsafe_code)]` + `postings_ll/simd.rs` 单模块 `#[allow(unsafe_code)]`，core `#![forbid(unsafe_code)]`，统一 `io::Result`）；不新增依赖；Java 9.12.3（`interop/java/lib/lucene-core-9.12.3.jar`）做 diff 基准；格式语义以 `reference/lucene-9.12.3/` 源码为准（该目录只在主 checkout 存在，worktree 内引用按仓库根相对路径书写）。

## Global Constraints

（摘自 spec 与既有项目惯例，逐字或就近转述；所有 Task 共同遵守）

- **Rust edition 分工**：`crates/codec-lucene9` edition 2024，`crates/core` edition 2021（各自 Cargo.toml 已声明，新增代码遵守，不得改动）。
- **测试命令**：codec 层 `cargo test -p codec-lucene9 <test名>`，core 层 `cargo test -p rustlucene-core <test名>`；收尾门槛为 `make log-test`（200000 文档四变体：seed 42 默认 / 43 `--positions` / 44 `--sparse` / 45 `--bigdict`，Makefile:17-22），较慢，只在 T3/T9 这类电池任务与最终收尾使用；T5/T6 的电池增量用单变体 `interop/verify-log.sh` 验证。
- **Lucene 语义照抄**：执行语义逐行对照 9.12.3 源码，关键决策在代码注释中给 `File.java:line` 引用（Java 源码在 `reference/lucene-9.12.3/lucene/core/src/java/org/apache/lucene/`，下文引用省略该前缀）。找不到精确行号时引用类名 + 方法名，不编行号。
- **commit message 前缀**：`feat:` / `test:` / `docs:`（沿用 git log 现有风格）。
- **全部 ConstantScore 语义**：不做评分 / norms / impact / Block-Max；不做 fuzzy；不做 NRT；不做 query cache（bench 一律 `--no-cache` 口径）。bitset 路径的 `freq()` 恒为 1（doc 集语义，`freq_sum` 不对 multi-term 查询使用）。
- **unsafe_code**：除既有 `crates/codec-lucene9/src/postings_ll/simd.rs` 的模块级 `#[allow(unsafe_code)]` SIMD 核外，任何新代码不得引入 unsafe；core 保持 `#![forbid(unsafe_code)]`。
- **前提假设**：读侧只保证读**本系统写出的**索引（无 delete、无 payload、无 offsets）。`.pos` 读侧只需处理"满 128 块 PFOR + tail per-delta VInt"的无 payload/offset 布局（写侧 `write_positions` 的全部产出形态）。
- **格式对齐纪律**：每处字节级决策（skip entry 内 pos 字段顺序、tail 判定、frame 栈 push/pop）必须对照写侧 Rust 源码（`postings.rs`）与 9.12.3 Java 源码双确认。
- **实验/报告文件**：bench 数据与报告写到 `.superpowers/sdd/`（T10 起 gitignored），不进 git。

## Pre-checks（已执行，基线绿）

```
$ cargo build
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.81s
$ cargo test -p codec-lucene9
test result: ok. 135 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
$ cargo test -p rustlucene-core
test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## 关键设计事实（本计划全部代码的字节级依据，已逐项对照 9.12.3 源码与本仓库写侧源码核实）

1. **16 阈值**：`AbstractMultiTermQueryConstantScoreWrapper.java:43-44`——`BOOLEAN_REWRITE_TERM_COUNT_THRESHOLD = 16`，"mtq that matches 16 terms or less will be executed as a regular disjunction"。spec §4 拍板：≤16 rewrite 到 `Query::Or`，>16 bitset 物化（与 Lucene 的 DocIdSet rewrite 同构），不设枚举上限。
2. **.pos 布局**（写侧 `postings.rs:478-541` `write_positions`，镜像 Lucene912PostingsWriter.addPosition:287-342 + finishTerm:546-587）：position delta 按 doc 内 lastPosition=0 重置逐 doc 展开；满 128 的块 PFOR 编码；tail 逐 delta 一个 VInt（无 payload 时 code 就是 delta 本身）。`last_pos_block_offset` = ttf>128 时"满块区结束 fp − posStartFP"，否则 −1；读侧换算 `last_pos_block_fp`：ttf<128 → `pos_start_fp`，ttf==128 → −1（永不命中 tail 分支），ttf>128 → `pos_start_fp + last_pos_block_offset`（EverythingEnum.reset :789-797）。
3. **skip entry 的 pos 字段**（写侧已写出，当前读侧整段跳过，T7 改为解析）：
   - level-0（写侧 `postings.rs:576-631`，顺序）：`VLong skip0NumBytes` → `VInt15 docDelta` → `VLong15 blockTotalBytes` → `VLong impactBytes + impacts` → `VLong posFpDelta` + `Byte posBufferUpto`（后两项仅 has_positions）。读侧对照 EverythingEnum.moveToNextLevel0Block :919-931 / skipLevel0To :977-998。pos fp delta 逐块链式累加，首块基准 = term 的 posStartFP（写侧 `postings.rs:566-568`）。
   - level-1（写侧 `postings.rs:662-695`，在 has_freqs 的 numSkipBytes 段内）：`VInt docDelta` → `VLong level1TotalBytes` → `Short numSkipBytes` → `Short impactBytes + impacts` → `VLong posFpDelta` + `Byte posBufferUpto`。读侧对照 EverythingEnum.skipLevel1To :860-897，**每条 level-1 记录都要解析**（pos fp 是链式 delta，跳过即丢失）。
4. **pos 流重同步机制**（EverythingEnum，T7 照抄）：`level0PosEndFP` = 即将进入的 doc 块的起始 .pos fp，`level0BlockPosUpto` = 该边界在所在 .pos 128 块内已消费的 position 数（写侧 `write_positions` 的 `pos_block_index`，`postings.rs:523-538`）。进入新 doc 块时若 `level0PosEndFP >= posIn.filePointer()` 则 seek + `posPendingCount = level0BlockPosUpto` + `posBufferUpto = BLOCK_SIZE`（:908-917）；否则（positions 已被超前解码）改为累加当前 doc 块剩余 doc 的 freq（:971-975）。`nextDoc` 每返回一 doc `posPendingCount += freq`（:940-952）；`advance` 对跳过的缓冲区窗口 `[oldUpto, next]` 累加 freq（:1020-1027）。`nextPosition`：`posPendingCount > freq` 时先 `skipPositions`（跳过 `posPendingCount − freq` 个 delta，:1031-1082），`posBufferUpto == BLOCK_SIZE` 时 `refillPositions`（fp == lastPosBlockFP 走 tail VInt，否则 PFOR，:1084-1153），随后 `position += delta`、`posPendingCount--`（:1156-1187）。
5. **TermsIter 帧栈语义**（T4 照抄）：`SegmentTermsEnum.next` :960-1051——当前 frame 扫尽后，`!isLastInFloor` 则 `loadNextFloorBlock`（:126-134，floor 兄弟块在 .tim 中连续，`fp = fpEnd`），否则 pop 到父 frame（父未装载则 reload 后 `scanToSubBlock(lastFP)` :497-525 定位）；`Frame.next` 遇 sub-block 项则 push 子 frame（:291-356）。`seekCeil` :581——FST 下降 + `scanToFloorFrame` + `scanToTerm(exactOnly=false)`（scanToTermLeaf :547-660 / scanToTermNonLeaf :732-830；首个 > target 的项若是 sub-block，下降取其首 term :805-813）。写侧保证每个 block-group 都是 FST 输入（compileIndex :490-578，`postings.rs:845-861`），所以精确命中 sub-block 前缀的情形不会出现（与 `scan_block` 的既有断言一致）。
6. **Phrase slop=0 校验**（T8 照抄）：doc 合取命中后，每个 term occurrence 一个独立 PositionsEnum（重复 term 如 "foo foo" 天然正确），读齐各 occurrence 本 doc 的 `freq` 个 position，存在 `p0 ∈ list[0]` 使 `p0 − offset[0] + offset[i] ∈ list[i]` 对所有 i 成立（ExactPhraseMatcher :138-167，`phrasePos = lead.pos − lead.offset` :145、`expectedPos = phrasePos + posting.offset` :148；PhrasePositions.nextPosition :52-58）。对无 positions 字段发 phrase 在**构造期 fail-fast**（Java 在无 positions 字段上执行 PhraseQuery 抛异常；对齐为构造期错误而非空结果）。
7. **Wildcard 语义**：`WildcardQuery.toAutomaton` :84-114——`*` → `Automata.makeAnyString()`，`?` → `Automata.makeAnyChar()`（**单 code point**，按 `codePointAt` 迭代 :90-91）。分类照 spec §5 / 总 spec §3：pattern 截到第一个 `*`/`?` 得固定前缀；纯前缀形（`foo*`）零过滤；有前缀含通配（`fo?o*`）前缀枚举 + 尾过滤；无前缀（`*foo`）全字典扫 + 过滤。
8. **FixedBitSet 语义**：`util/FixedBitSet.java`——`set(int)` :239、`nextSetBit(int)` :274（找不到返回 −1，Rust 侧 `Option<usize>`）、`cardinality()` :193。

---

## Task 1: FixedBitSet（`crates/core/src/search/bitset.rs` 新建）

**Files:**
- Create: `crates/core/src/search/bitset.rs`
- Modify: `crates/core/src/search/mod.rs`（`pub mod bitset;` + re-export）
- Test: `crates/core/src/search/bitset.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: 无（纯数据结构）。
- Produces（T2 的 `BitsetDocIter` 与 `multi_term::materialize` 依赖这些名字，不得改名）:
  ```rust
  // bitset.rs
  pub struct FixedBitSet { .. }
  impl FixedBitSet {
      pub fn new(num_bits: usize) -> Self;
      pub fn num_bits(&self) -> usize;
      pub fn set(&mut self, index: usize);
      pub fn get(&self, index: usize) -> bool;
      pub fn next_set_bit(&self, from: usize) -> Option<usize>;
      pub fn popcount(&self) -> u64;
  }
  // mod.rs
  pub use bitset::FixedBitSet;
  ```

### Steps

- [ ] **Step 1.1: 写失败测试** — 新建 `crates/core/src/search/bitset.rs`，先只放测试（`FixedBitSet` 尚不存在，编译失败即失败测试成立）：

  ```rust
  //! FixedBitSet for the multi-term bitset-materialization path (search spec
  //! M2 §4): per-segment doc set of `max_doc` bits. Mirrors the slice of
  //! util/FixedBitSet.java we need (set :239, nextSetBit :274, cardinality :193).

  #[cfg(test)]
  mod tests {
      use super::FixedBitSet;

      #[test]
      fn new_set_all_clear() {
          let b = FixedBitSet::new(130);
          assert_eq!(b.num_bits(), 130);
          assert_eq!(b.popcount(), 0);
          assert!(!b.get(0));
          assert!(!b.get(129));
          assert_eq!(b.next_set_bit(0), None);
      }

      #[test]
      fn set_get_round_trip_across_words() {
          let mut b = FixedBitSet::new(130);
          for i in [0usize, 1, 63, 64, 65, 127, 128, 129] {
              b.set(i);
          }
          for i in 0..130 {
              assert_eq!(b.get(i), [0, 1, 63, 64, 65, 127, 128, 129].contains(&i), "bit {i}");
          }
          assert_eq!(b.popcount(), 8);
      }

      #[test]
      fn next_set_bit_scans_gaps_and_words() {
          let mut b = FixedBitSet::new(200);
          b.set(5);
          b.set(64);
          b.set(199);
          assert_eq!(b.next_set_bit(0), Some(5));
          assert_eq!(b.next_set_bit(5), Some(5)); // inclusive
          assert_eq!(b.next_set_bit(6), Some(64));
          assert_eq!(b.next_set_bit(64), Some(64));
          assert_eq!(b.next_set_bit(65), Some(199));
          assert_eq!(b.next_set_bit(199), Some(199));
          assert_eq!(b.next_set_bit(200), None);
      }

      #[test]
      fn next_set_bit_respects_num_bits() {
          // bits beyond num_bits must never be returned even though the
          // backing word has room for them
          let mut b = FixedBitSet::new(3);
          b.set(2);
          assert_eq!(b.next_set_bit(3), None);
          assert_eq!(b.popcount(), 1);
      }

      #[test]
      fn popcount_dense() {
          let mut b = FixedBitSet::new(128);
          for i in 0..128 {
              b.set(i);
          }
          assert_eq!(b.popcount(), 128);
          assert_eq!(b.next_set_bit(127), Some(127));
          assert_eq!(b.next_set_bit(128), None);
      }
  }
  ```

  同时 `crates/core/src/search/mod.rs` 在 `pub mod collector;` 之前插入一行 `pub mod bitset;`，并在 `pub use collector::{...}` 之前插入一行 `pub use bitset::FixedBitSet;`（否则模块未声明，测试无法编译）。

- [ ] **Step 1.2: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core bitset 2>&1 | tail -5
  error[E0432]: unresolved import `crate::search::bitset::FixedBitSet`
  ```

- [ ] **Step 1.3: 最小实现** — `crates/core/src/search/bitset.rs` 在模块文档注释之后、`#[cfg(test)]` 之前插入：

  ```rust
  /// A fixed-size bit set over `[0, num_bits)`, backed by u64 words
  /// (FixedBitSet.java:124).
  pub struct FixedBitSet {
      bits: Vec<u64>,
      num_bits: usize,
  }

  impl FixedBitSet {
      /// All bits clear.
      pub fn new(num_bits: usize) -> Self {
          FixedBitSet {
              bits: vec![0u64; (num_bits + 63) / 64],
              num_bits,
          }
      }

      /// FixedBitSet.numBits.
      pub fn num_bits(&self) -> usize {
          self.num_bits
      }

      /// FixedBitSet.set(int) (:239).
      pub fn set(&mut self, index: usize) {
          debug_assert!(index < self.num_bits);
          self.bits[index >> 6] |= 1u64 << (index & 63);
      }

      /// FixedBitSet.get(int).
      pub fn get(&self, index: usize) -> bool {
          debug_assert!(index < self.num_bits);
          self.bits[index >> 6] & (1u64 << (index & 63)) != 0
      }

      /// FixedBitSet.nextSetBit(int) (:274): smallest set bit >= `from`,
      /// or None. Bits at index >= num_bits are never returned.
      pub fn next_set_bit(&self, from: usize) -> Option<usize> {
          if from >= self.num_bits {
              return None;
          }
          let mut word_idx = from >> 6;
          let mut word = self.bits[word_idx] & (u64::MAX << (from & 63));
          loop {
              if word != 0 {
                  let idx = (word_idx << 6) + word.trailing_zeros() as usize;
                  return if idx < self.num_bits { Some(idx) } else { None };
              }
              word_idx += 1;
              if word_idx == self.bits.len() {
                  return None;
              }
              word = self.bits[word_idx];
          }
      }

      /// FixedBitSet.cardinality() (:193).
      pub fn popcount(&self) -> u64 {
          self.bits.iter().map(|w| w.count_ones() as u64).sum()
      }
  }
  ```

- [ ] **Step 1.4: 跑测试确认通过**

  ```
  $ cargo test -p rustlucene-core bitset 2>&1 | tail -3
  test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 1.5: 提交**

  ```
  git add crates/core/src/search/bitset.rs crates/core/src/search/mod.rs
  git commit -m "feat: FixedBitSet for the multi-term bitset-materialization path"
  ```

---

## Task 2: Terms(IN) 查询 + 阈值双路执行（`multi_term.rs` 新建）

**Files:**
- Create: `crates/core/src/search/multi_term.rs`
- Modify: `crates/core/src/search/query.rs`（`Query::Terms` 变体 + `is_multi_term` + `bitset_count`）
- Modify: `crates/core/src/search/doc_iter.rs`（`BitsetDocIter` + `SegmentDocIter::Bitset`）
- Modify: `crates/core/src/search/segment_reader.rs`（`field_has_freqs`）
- Modify: `crates/core/src/search/searcher.rs`（`count` 的 multi-term 分支）
- Modify: `crates/core/src/search/mod.rs`（`pub mod multi_term;` + 语义测试）
- Test: `crates/core/src/search/mod.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T1 的 `FixedBitSet`；现有 `SegmentReader::seek_term` / `docs_enum` / `docs_freqs_enum(entry, needs_freq)` / `field_info`、`Query::Or` 的既有 segment_iterator 分支、`DisjunctionDocIter`。
- Produces（T5/T6 依赖这些签名，不得改名）:
  ```rust
  // multi_term.rs
  pub(crate) const BOOLEAN_REWRITE_THRESHOLD: usize = 16; // AbstractMultiTermQueryConstantScoreWrapper.java:44
  pub(crate) struct CollectedTerms {
      pub terms: Vec<Vec<u8>>,                 // 与 entries 平行，df 升序
      pub entries: Vec<(u32, TermEntry)>,      // (doc_freq, entry)
  }
  impl CollectedTerms {
      pub(crate) fn len(&self) -> usize;
      pub(crate) fn is_empty(&self) -> bool;
      pub(crate) fn sort_by_df(&mut self);
  }
  pub(crate) fn collect_direct(seg: &mut SegmentReader, field: &str, terms: &[Vec<u8>]) -> io::Result<Option<(bool, CollectedTerms)>>;
  pub(crate) fn segment_iterator(seg: &mut SegmentReader, field: &str, has_freqs: bool, collected: &CollectedTerms, needs_freq: bool) -> io::Result<Option<SegmentDocIter>>;
  pub(crate) fn bitset_count(seg: &SegmentReader, has_freqs: bool, collected: &CollectedTerms) -> io::Result<Option<u64>>;
  pub(crate) fn materialize(seg: &SegmentReader, entries: &[(u32, TermEntry)], has_freqs: bool) -> io::Result<FixedBitSet>;
  // doc_iter.rs
  pub struct BitsetDocIter { .. }
  impl BitsetDocIter {
      pub fn new(bits: FixedBitSet) -> Self;
      pub fn popcount(&self) -> u64;
  }
  // SegmentDocIter 新增变体 Bitset(BitsetDocIter)
  // segment_reader.rs
  pub(crate) fn field_has_freqs(&self, field: &str) -> Option<bool>;
  // query.rs
  pub enum Query { .., Terms { field: String, terms: Vec<Vec<u8>> } }
  impl Query {
      pub fn terms(field: &str, terms: &[&str]) -> Query;
      pub(crate) fn is_multi_term(&self) -> bool;
      pub(crate) fn bitset_count(&self, seg: &mut SegmentReader) -> io::Result<Option<u64>>;
  }
  ```

  语义决定（与 Java 对齐）：term 集合按"段内实际存在的 term"计数阈值（Lucene collectTerms 只收集字典中命中的 term，AbstractMultiTermQueryConstantScoreWrapper.java:170-200）；未知字段 / 无 postings 字段 → 空结果（同 TermQuery）；重复 term 不去重（OR/bitset 两种路径语义都天然去重命中）；`Terms` 单 term 退化为 `Query::Term`，空集合 → 无命中。

### Steps

- [ ] **Step 2.1: 写失败测试** — 追加到 `crates/core/src/search/mod.rs` 的 `mod tests`（复用模块内已有 `temp_dir` / `schema` / `doc` 助手）：

  ```rust
      /// 40 docs; doc i carries tokens t(i%20) and t((i+7)%20) — every t-term
      /// has df=4 and the term sets overlap, so union sizes are non-trivial.
      fn write_terms_corpus(root: &std::path::Path) {
          let mut w = IndexWriter::create(root, schema(), IndexWriterConfig::default()).unwrap();
          for i in 0..40 {
              let m = format!("t{:02} t{:02}", i % 20, (i + 7) % 20);
              w.add_document(doc("INFO", &format!("tid-{i}"), &m)).unwrap();
          }
          w.commit().unwrap();
          drop(w);
      }

      fn t_terms(range: std::ops::Range<usize>) -> Vec<String> {
          range.map(|i| format!("t{i:02}")).collect()
      }

      #[test]
      fn terms_query_matches_or_on_both_paths() {
          let root = temp_dir("termsdual");
          write_terms_corpus(&root);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          let all = t_terms(0..20);
          let all_ref: Vec<&str> = all.iter().map(String::as_str).collect();

          // >16 terms -> bitset path; result must equal the literal OR query
          let or_q = Query::or("message", &all_ref);
          let (or_total, or_docs) = s.top_docs(&or_q, 100).unwrap();
          let terms_q = Query::terms("message", &all_ref);
          let (t_total, t_docs) = s.top_docs(&terms_q, 100).unwrap();
          assert_eq!((or_total, or_docs.clone()), (t_total, t_docs));
          assert_eq!(s.count(&terms_q).unwrap(), or_total);
          assert_eq!(s.count(&terms_q).unwrap(), 40); // every doc has two t-terms

          // exactly 16 terms -> OR rewrite path, still equal to OR
          let t16: Vec<&str> = all_ref[..16].to_vec();
          let or16 = Query::or("message", &t16);
          let terms16 = Query::terms("message", &t16);
          let (a_total, a_docs) = s.top_docs(&or16, 100).unwrap();
          let (b_total, b_docs) = s.top_docs(&terms16, 100).unwrap();
          assert_eq!((a_total, a_docs), (b_total, b_docs));
          assert_eq!(s.count(&terms16).unwrap(), a_total);

          // 17 terms -> bitset path boundary
          let t17: Vec<&str> = all_ref[..17].to_vec();
          let or17 = Query::or("message", &t17);
          let terms17 = Query::terms("message", &t17);
          let (a_total, a_docs) = s.top_docs(&or17, 100).unwrap();
          let (b_total, b_docs) = s.top_docs(&terms17, 100).unwrap();
          assert_eq!((a_total, a_docs), (b_total, b_docs));
          assert_eq!(s.count(&terms17).unwrap(), a_total);

          // dedup: docs carrying two of the terms are counted once
          let dup = Query::terms("message", &["t00", "t07"]); // doc 0 has both
          let (total, docs) = s.top_docs(&dup, 100).unwrap();
          assert_eq!(docs.iter().filter(|&&d| d == 0).count(), 1);
          assert_eq!(total as usize, docs.len());
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn terms_query_edge_cases() {
          let root = temp_dir("termsedge");
          write_terms_corpus(&root);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          // all terms missing -> 0
          assert_eq!(s.count(&Query::terms("message", &["zz1", "zz2"])).unwrap(), 0);
          // mixed present/missing
          let (total, _) = s.top_docs(&Query::terms("message", &["t00", "zz1"]), 100).unwrap();
          assert_eq!(total, 4); // df(t00) = 4
          // empty term set -> 0
          assert_eq!(s.count(&Query::terms("message", &[])).unwrap(), 0);
          // single term degenerates to a Term query
          assert_eq!(s.count(&Query::terms("message", &["t00"])).unwrap(), 4);
          // keyword field (DOCS layout)
          assert_eq!(s.count(&Query::terms("level", &["INFO", "WARN"])).unwrap(), 40);
          // unknown / stored-only fields -> empty
          assert_eq!(s.count(&Query::terms("nope", &["x"])).unwrap(), 0);
          assert_eq!(s.count(&Query::terms("title", &["stored"])).unwrap(), 0);
          // duplicated input terms are harmless
          assert_eq!(s.count(&Query::terms("message", &["t00", "t00"])).unwrap(), 4);
          // >16 on the keyword field too (bitset over DOCS postings)
          let tids: Vec<String> = (0..18).map(|i| format!("tid-{i}")).collect();
          let tids_ref: Vec<&str> = tids.iter().map(String::as_str).collect();
          assert_eq!(s.count(&Query::terms("tid", &tids_ref)).unwrap(), 18);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn terms_query_multi_segment() {
          let root = temp_dir("termsseg");
          let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
          for i in 0..3 {
              w.add_document(doc("INFO", &format!("tid-{i}"), "alpha")).unwrap();
          }
          w.commit().unwrap();
          for i in 3..7 {
              w.add_document(doc("WARN", &format!("tid-{i}"), "beta")).unwrap();
          }
          w.commit().unwrap();
          drop(w);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          assert_eq!(s.segment_count(), 2);
          let q = Query::terms("level", &["INFO", "WARN"]);
          assert_eq!(s.count(&q).unwrap(), 7);
          let (_, docs) = s.top_docs(&q, 20).unwrap();
          assert_eq!(docs, vec![0, 1, 2, 3, 4, 5, 6]);
          let q = Query::terms("message", &["alpha", "beta"]);
          assert_eq!(s.count(&q).unwrap(), 7);
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  同时 `crates/core/src/search/mod.rs` 在 `pub mod doc_iter;` 之后插入 `pub mod multi_term;`（否则 T2 实现无处安放；此刻该文件不存在，编译失败）。

- [ ] **Step 2.2: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core search::tests::terms 2>&1 | tail -5
  error[E0599]: no function or associated item named `terms` found for enum `Query`
  ```

- [ ] **Step 2.3: 最小实现** —

  `crates/core/src/search/multi_term.rs`（完整新文件）：

  ```rust
  //! Multi-term query execution (search spec M2 §4): collect the term set of a
  //! Terms/Prefix/Wildcard query for one segment, then dispatch on the Lucene 9
  //! blended threshold — at most 16 terms rewrite to `Query::Or`
  //! (AbstractMultiTermQueryConstantScoreWrapper.java:43-44), more than 16
  //! materialize a per-segment FixedBitSet (Lucene's DocIdSet rewrite,
  //! MultiTermQueryConstantScoreBlendedWrapper.java:55-120). No enumeration cap
  //! (Lucene 9 dropped TooManyClauses for MTQs).

  use std::io;

  use codec_lucene9::postings_read::NO_MORE_DOCS;
  use codec_lucene9::terms_read::TermEntry;

  use super::bitset::FixedBitSet;
  use super::doc_iter::{BitsetDocIter, SegmentDocIter};
  use super::query::Query;
  use super::segment_reader::SegmentReader;

  /// AbstractMultiTermQueryConstantScoreWrapper.java:44.
  pub(crate) const BOOLEAN_REWRITE_THRESHOLD: usize = 16;

  /// Terms of one query present in one segment's dictionary, df-sorted
  /// (spec §4: 集合收集后按 df 排序交给双路).
  pub(crate) struct CollectedTerms {
      pub terms: Vec<Vec<u8>>,
      pub entries: Vec<(u32, TermEntry)>,
  }

  impl CollectedTerms {
      pub(crate) fn len(&self) -> usize {
          self.entries.len()
      }

      pub(crate) fn is_empty(&self) -> bool {
          self.entries.is_empty()
      }

      pub(crate) fn sort_by_df(&mut self) {
          let mut pairs: Vec<(Vec<u8>, (u32, TermEntry))> = self
              .terms
              .drain(..)
              .zip(self.entries.drain(..))
              .collect();
          pairs.sort_by_key(|(_, (df, _))| *df);
          for (t, e) in pairs {
              self.terms.push(t);
              self.entries.push(e);
          }
      }
  }

  /// Direct term-set collection (Terms/IN): seek each term, keep the present
  /// ones. `None` = unknown field (empty-hit semantics, same as TermQuery).
  pub(crate) fn collect_direct(
      seg: &mut SegmentReader,
      field: &str,
      terms: &[Vec<u8>],
  ) -> io::Result<Option<(bool, CollectedTerms)>> {
      let Some(has_freqs) = seg.field_has_freqs(field) else {
          return Ok(None);
      };
      let mut collected = CollectedTerms {
          terms: Vec::new(),
          entries: Vec::new(),
      };
      for t in terms {
          if let Some((_, entry)) = seg.seek_term(field, t)? {
              collected.terms.push(t.clone());
              collected.entries.push((entry.doc_freq, entry));
          }
      }
      collected.sort_by_df();
      Ok(Some((has_freqs, collected)))
  }

  /// Threshold dispatch (spec §4): <=16 terms rewrite to `Query::Or` (heap
  /// merge, zero new execution code); >16 materialize a FixedBitSet.
  pub(crate) fn segment_iterator(
      seg: &mut SegmentReader,
      field: &str,
      has_freqs: bool,
      collected: &CollectedTerms,
      needs_freq: bool,
  ) -> io::Result<Option<SegmentDocIter>> {
      if collected.is_empty() {
          return Ok(None);
      }
      if collected.len() <= BOOLEAN_REWRITE_THRESHOLD {
          return Query::Or {
              field: field.to_string(),
              terms: collected.terms.clone(),
          }
          .segment_iterator(seg, needs_freq);
      }
      let bits = materialize(seg, &collected.entries, has_freqs)?;
      Ok(Some(SegmentDocIter::Bitset(BitsetDocIter::new(bits))))
  }

  /// Count fast path (spec §4: count 路径直接 popcount): `Some(popcount)` on
  /// the bitset path, `None` when the OR path applies (caller iterates).
  pub(crate) fn bitset_count(
      seg: &SegmentReader,
      has_freqs: bool,
      collected: &CollectedTerms,
  ) -> io::Result<Option<u64>> {
      if collected.len() <= BOOLEAN_REWRITE_THRESHOLD {
          return Ok(None);
      }
      Ok(Some(materialize(seg, &collected.entries, has_freqs)?.popcount()))
  }

  /// Bitset materialization (spec §4): per-term full postings scan, one bit
  /// per hit doc. Uses no-freq enums on freqs fields — the bitset carries no
  /// per-doc freq (ConstantScore).
  pub(crate) fn materialize(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
  ) -> io::Result<FixedBitSet> {
      let mut bits = FixedBitSet::new(seg.max_doc() as usize);
      for (_, entry) in entries {
          if has_freqs {
              let mut en = seg.docs_freqs_enum(entry, false)?;
              loop {
                  let d = en.next_doc()?;
                  if d == NO_MORE_DOCS {
                      break;
                  }
                  bits.set(d as usize);
              }
          } else {
              let mut en = seg.docs_enum(entry)?;
              loop {
                  let d = en.next_doc()?;
                  if d == NO_MORE_DOCS {
                      break;
                  }
                  bits.set(d as usize);
              }
          }
      }
      Ok(bits)
  }
  ```

  `crates/core/src/search/doc_iter.rs`：`use super::bitset::FixedBitSet;` 追加到顶部 use 区；在 `// ── SegmentDocIter ──` 分隔注释之前插入：

  ```rust
  // ── Bitset (multi-term materialization) ─────────────────────────────────

  /// DocIter over a materialized FixedBitSet (spec §4 bitset path): next_doc /
  /// advance are next_set_bit scans. freq() is 1 (doc-set semantics).
  pub struct BitsetDocIter {
      bits: FixedBitSet,
      doc: i32,
  }

  impl BitsetDocIter {
      pub fn new(bits: FixedBitSet) -> Self {
          BitsetDocIter { bits, doc: -1 }
      }

      /// FixedBitSet.cardinality — the count fast path (spec §4).
      pub fn popcount(&self) -> u64 {
          self.bits.popcount()
      }
  }

  impl DocIter for BitsetDocIter {
      fn doc_id(&self) -> i32 {
          self.doc
      }

      fn next_doc(&mut self) -> io::Result<i32> {
          if self.doc == NO_MORE_DOCS {
              return Ok(NO_MORE_DOCS);
          }
          self.doc = match self.bits.next_set_bit((self.doc + 1) as usize) {
              Some(d) => d as i32,
              None => NO_MORE_DOCS,
          };
          Ok(self.doc)
      }

      fn advance(&mut self, target: i32) -> io::Result<i32> {
          if target > self.doc {
              self.doc = match self.bits.next_set_bit(target.max(0) as usize) {
                  Some(d) => d as i32,
                  None => NO_MORE_DOCS,
              };
          }
          Ok(self.doc)
      }
  }
  ```

  `SegmentDocIter` enum 增加变体 `Bitset(BitsetDocIter)`，`impl DocIter for SegmentDocIter` 的 `doc_id` / `next_doc` / `advance` 各加一臂 `Self::Bitset(b) => b.doc_id()` / `b.next_doc()` / `b.advance(t)`（`freq` 已被既有 `_ => 1` 覆盖）。

  `crates/core/src/search/segment_reader.rs`：`field_info` 方法之后追加：

  ```rust
      /// Whether the field indexes freqs (IndexOptions >= DOCS_AND_FREQS);
      /// None = unknown field (empty-hit semantics).
      pub(crate) fn field_has_freqs(&self, field: &str) -> Option<bool> {
          self.field_infos
              .by_name(field)
              .map(|fi| fi.index_options != IndexOptions::Docs && fi.index_options != IndexOptions::None)
      }
  ```

  `crates/core/src/search/query.rs`：顶部 `use super::doc_iter::{ConjunctionDocIter, DisjunctionDocIter, MatchAllIter, SegmentDocIter};` 之后加一行 `use super::multi_term;`。enum `Query` 在 `Or { field: String, terms: Vec<Vec<u8>> },` 一行之后追加变体：

  ```rust
      Terms { field: String, terms: Vec<Vec<u8>> },
  ```

  `impl Query` 的 `or(...)` 构造器之后追加三个方法（完整新代码）：

  ```rust
      /// Terms(IN) — Boolean SHOULD sugar (spec M2 §1): the doc union of the
      /// term set, executed via the <=16/>16 dual path (spec M2 §4).
      pub fn terms(field: &str, terms: &[&str]) -> Query {
          Query::Terms {
              field: field.to_string(),
              terms: terms.iter().map(|t| t.as_bytes().to_vec()).collect(),
          }
      }

      /// Multi-term queries (Terms/Prefix/Wildcard) share the Searcher::count
      /// dual path (popcount on the bitset path, iteration otherwise).
      pub(crate) fn is_multi_term(&self) -> bool {
          matches!(self, Query::Terms { .. })
      }

      /// Per-segment count shortcut: `Some(popcount)` when this query takes
      /// the bitset path in this segment, `None` otherwise (caller iterates).
      pub(crate) fn bitset_count(&self, seg: &mut SegmentReader) -> io::Result<Option<u64>> {
          match self {
              Query::Terms { field, terms } => {
                  if terms.len() < 2 {
                      return Ok(None); // degenerate: empty set / single-term Term path
                  }
                  let Some((has_freqs, collected)) = multi_term::collect_direct(seg, field, terms)? else {
                      return Ok(Some(0)); // unknown field: empty hit set
                  };
                  multi_term::bitset_count(seg, has_freqs, &collected)
              }
              _ => Ok(None),
          }
      }
  ```

  `segment_iterator` 的 `match self` 在 `Query::Or { .. } => { .. }` 分支之后追加一臂（完整新代码）：

  ```rust
              Query::Terms { field, terms } => {
                  if terms.is_empty() {
                      return Ok(None);
                  }
                  if terms.len() == 1 {
                      return Query::Term {
                          field: field.clone(),
                          term: terms[0].clone(),
                      }
                      .segment_iterator(seg, needs_freq);
                  }
                  let Some((has_freqs, collected)) = multi_term::collect_direct(seg, field, terms)? else {
                      return Ok(None);
                  };
                  multi_term::segment_iterator(seg, field, has_freqs, &collected, needs_freq)
              }
  ```

  `crates/core/src/search/searcher.rs`：`count` 整个方法替换为（Term 快路径逐字保留 + 新增 multi-term 分支）：

  ```rust
      /// ConstantScore TermQuery: count = sum of per-segment doc_freq (no
      /// postings iteration needed — doc_freq is in the TermEntry after
      /// seek_exact). Fallback to iteration for MatchAll and unknown terms.
      /// Multi-term queries count per segment: popcount on the bitset path
      /// (spec §4), plain iteration on the OR path.
      pub fn count(&mut self, query: &Query) -> io::Result<u64> {
          if let Query::Term { field, term } = query {
              let mut total = 0u64;
              for (_doc_base, seg) in self.reader.leaves() {
                  if let Some((_, entry)) = seg.seek_term(field, term)? {
                      total += entry.doc_freq as u64;
                  }
              }
              return Ok(total);
          }
          if query.is_multi_term() {
              let mut total = 0u64;
              for (_doc_base, seg) in self.reader.leaves() {
                  if let Some(c) = query.bitset_count(seg)? {
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
          let mut c = CountCollector::default();
          self.search(query, &mut c)?;
          Ok(c.count)
      }
  ```

  `crates/core/src/search/mod.rs`：模块文档头从 "Term and MatchAll queries" 更新为 "Term/MatchAll/Boolean/multi-term queries (ConstantScore semantics)"；`pub mod multi_term;` 已在 Step 2.1 加入。

- [ ] **Step 2.4: 跑测试确认通过**

  ```
  $ cargo test -p rustlucene-core terms_ 2>&1 | tail -3
  test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test 2>&1 | grep -E "^test result"
  test result: ok. 135 passed; 0 failed; 1 ignored; ...（codec 回归）
  test result: ok. 30 passed; 0 failed; ...（core：22 基线 + 5 bitset + 3 terms）
  ```

- [ ] **Step 2.5: 提交**

  ```
  git add crates/core/src/search/multi_term.rs crates/core/src/search/query.rs crates/core/src/search/doc_iter.rs crates/core/src/search/segment_reader.rs crates/core/src/search/searcher.rs crates/core/src/search/mod.rs
  git commit -m "feat: Terms(IN) query with <=16 OR / >16 bitset dual-path execution"
  ```

---

## Task 3: diff 电池 Terms 项（双侧硬编码，纳入 make log-test）

电池新增 Terms(IN) 项，**同时覆盖两条执行路径**：3-term 集合（≤16，OR 路径）与 17-term 集合（>16，bitset 路径），外加全缺失零命中与混合缺失。双侧输出逐行 diff，挂在既有 `interop/verify-log.sh` → `interop/verify-search.sh` 链路里（无需改脚本）。

**Files:**
- Modify: `crates/core/src/bin/rustlucene-cli.rs`（`searchdump` 追加 terms 电池段）
- Modify: `interop/java/VerifySearchIndex.java`（追加同款 terms 电池段）
- Test: 端到端即测试（`make log-test` 全绿为本任务验收）

**Interfaces:**
- Consumes: T2 的 `Query::terms` / `Searcher::count` / `top_docs`；CLI 既有 `doc_csv`；Java 侧既有 BooleanQuery SHOULD 电池模式。
- Produces:
  ```
  # 两侧一致的新增输出行（格式）：
  terms level=INFO,WARN,DEBUG count=<n> first20=<csv>
  terms message=connection0,query23,queue39 count=<n> first20=<csv>
  terms message=connection0,nosuchterm42 count=<n> first20=<csv>
  terms message=connection0,...,connection16 count=<n> first20=<csv>
  ```

### Steps

- [ ] **Step 3.1: Rust 侧 `searchdump` 追加 terms 电池** — `crates/core/src/bin/rustlucene-cli.rs`，在 boolean_battery 循环之后、`print!("{out}")` 之前插入：

  ```rust
      // M2 Terms(IN) battery (search spec M2 §4): 3-term sets take the <=16 OR
      // rewrite path, the 17-term set the >16 bitset path; the mixed
      // present/missing item locks the empty-term tolerance. Mirrored in
      // VerifySearchIndex.java.
      let terms17: Vec<String> = (0..17).map(|i| format!("connection{i}")).collect();
      let terms_battery: [(&str, Vec<String>); 4] = [
          ("level", vec!["INFO".into(), "WARN".into(), "DEBUG".into()]),
          ("message", vec!["connection0".into(), "query23".into(), "queue39".into()]),
          ("message", vec!["connection0".into(), "nosuchterm42".into()]),
          ("message", terms17),
      ];
      for (field, terms) in &terms_battery {
          let term_refs: Vec<&str> = terms.iter().map(String::as_str).collect();
          let q = Query::terms(field, &term_refs);
          let count = searcher.count(&q)?;
          let (_, docs) = searcher.top_docs(&q, 20)?;
          out.push_str(&format!(
              "terms {field}={} count={count} first20={}\n",
              terms.join(","),
              doc_csv(&docs)
          ));
      }
  ```

- [ ] **Step 3.2: Java 侧镜像** — `interop/java/VerifySearchIndex.java`，在 Boolean battery 循环之后、`System.out.print(out)` 之前插入：

  ```java
            // M2 Terms(IN) battery: same items/format as the terms_battery in
            // rustlucene-cli searchdump. Boolean SHOULD of TermQuery is the
            // Java counterpart of Rust's Terms dual-path execution.
            String[][][] termsBattery = {
                {{"level"}, {"INFO", "WARN", "DEBUG"}},
                {{"message"}, {"connection0", "query23", "queue39"}},
                {{"message"}, {"connection0", "nosuchterm42"}},
                {{"message"}, {"connection0","connection1","connection2","connection3",
                                "connection4","connection5","connection6","connection7",
                                "connection8","connection9","connection10","connection11",
                                "connection12","connection13","connection14","connection15",
                                "connection16"}},
            };
            for (String[][] item : termsBattery) {
                String field = item[0][0];
                BooleanQuery.Builder bq = new BooleanQuery.Builder();
                for (String t : item[1])
                    bq.add(new TermQuery(new Term(field, t)), BooleanClause.Occur.SHOULD);
                Query q = new ConstantScoreQuery(bq.build());
                TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                StringBuilder b = new StringBuilder();
                for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                out.append("terms ").append(field).append('=')
                   .append(String.join(",", item[1]))
                   .append(" count=").append(s.count(q))
                   .append(" first20=").append(b).append('\n');
            }
  ```

- [ ] **Step 3.3: 编译 + 单变体烟测**

  ```
  $ cargo build --release 2>&1 | tail -1
      Finished `release` profile [optimized] target(s)
  $ make java-classes 2>&1 | tail -1
      （javac 无输出即成功）
  $ interop/verify-log.sh 20000 42
  ...
  terms level=INFO,WARN,DEBUG count=... first20=...
  terms message=connection0,query23,queue39 count=... first20=...
  terms message=connection0,nosuchterm42 count=... first20=...
  terms message=connection0,connection1,...,connection16 count=... first20=...
  SEARCH_INTEROP_OK
  LOG_INTEROP_OK
  ```

- [ ] **Step 3.4: `make log-test` 四变体全绿（本任务验收门槛）**

  ```
  $ make log-test
      （4 条 verify-log.sh 变体依次跑过：默认 / --positions / --sparse / --bigdict，
        每条含 CheckIndex + VerifyLogIndex diff + SEARCH_INTEROP_OK）
  ```
  注意 terms 电池在所有变体都跑（不依赖 positions）；`--bigdict` 变体的高基数 trace_id_sdv 字段不经本电池，但 message/level 的 terms 项照常 diff。

- [ ] **Step 3.5: 提交**

  ```
  git add crates/core/src/bin/rustlucene-cli.rs interop/java/VerifySearchIndex.java
  git commit -m "feat: diff battery Terms items covering OR and bitset execution paths"
  ```

---

## Task 4: TermsIter——block-tree 顺序枚举器（`terms_read.rs` 追加）

`TermsIter` 是 spec §3 的核心组件（本阶段最大单组件）：frame 栈式 block-tree 枚举器，`seek_ceil(term) -> bool` 定位 + `next()` 顺序取 term。只碰 terms dict（.tim/.tip/.tmd），不碰 postings（总 spec 既定分层）。

**Files:**
- Modify: `crates/codec-lucene9/src/terms_read.rs`（追加 `TermsIter` + 私有 frame 机制 + `TermsDict::terms_iter`）
- Test: `crates/codec-lucene9/src/terms_read.rs` 的 `#[cfg(test)]` 模块（复用既有 `write_segment` / `temp_dir` / `indexed` 助手）

**Interfaces:**
- Consumes: 现有 `TermsDict`（`fields` / `tim_in` / `fst()`）、`TermEntry` / `TermState`、`read_msb_vlong`、`zigzag_decode`、`field_has_positions`、`OUTPUT_FLAG_IS_FLOOR`、`BLOCK_SIZE`、`corrupt`；`FstReader::trace_path(&self, input: &[u8]) -> io::Result<Vec<(usize, Vec<u8>)>>`。
- Produces（T5 的 `SegmentReader::terms_iter` 直接转交这些签名，不得改名）:
  ```rust
  // terms_read.rs
  impl TermsDict {
      pub fn terms_iter(&mut self, field: &FieldInfo) -> TermsIter<'_>;
  }
  pub struct TermsIter<'a> { .. }
  impl<'a> TermsIter<'a> {
      /// SegmentTermsEnum.seekCeil: positions so the next `next()` yields the
      /// first term >= target; true on exact hit.
      pub fn seek_ceil(&mut self, target: &[u8]) -> io::Result<bool>;
      /// Take semantics: yields the positioned term and advances. Fresh
      /// iterator starts at the dictionary's first term; None when exhausted.
      pub fn next(&mut self) -> io::Result<Option<(Vec<u8>, TermEntry)>>;
  }
  ```
  语义决定：take 语义——`seek_ceil` 后首次 `next()` 返回 ceiling term 本身（与 Lucene 的"positioned ON ceiling"等价，只是经 `next()` 取出）；target 低于 minTerm → 从字典首 term 开始（返回 false）；高于 maxTerm → 立即枯竭（返回 false）；字段无 .tmd 记录 → 构造即枯竭。frame 栈只保证**字典序全枚举正确性**与 **seek_ceil 落点正确性**，不保留 Lucene 的 seek 状态复用优化（`targetBeforeCurrentLength` / rewind），每次 seek_ceil 重新下降（spec §3 允许）。

### Steps

- [ ] **Step 4.1: 写失败测试** — 追加到 `crates/codec-lucene9/src/terms_read.rs` 的 `mod tests`（既有 `write_segment` 写 kw 字段 "a"(df=1) / "b"(df=3) / t000..t299(df=2)，tx 字段 "hello"(df=200) / "world"(df=2)；该字典含多级内部块与 floor 兄弟块）：

  ```rust
      fn collect_all(dict: &mut TermsDict, field: &FieldInfo) -> Vec<(Vec<u8>, u32)> {
          let mut it = dict.terms_iter(field);
          let mut out = Vec::new();
          while let Some((t, e)) = it.next().unwrap() {
              out.push((t, e.doc_freq));
          }
          out
      }

      #[test]
      fn terms_iter_full_enumeration_round_trip() {
          let root = temp_dir("iterall");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_segment(&dir);
          let mut dict = TermsDict::open(&dir, "_0", &[3u8; 16], &fis).unwrap();
          let kw = fis.by_name("kw").unwrap();
          // kw write-side input order is ascending: a, b, t000..t299
          let mut expected: Vec<(Vec<u8>, u32)> = vec![(b"a".to_vec(), 1), (b"b".to_vec(), 3)];
          for i in 0..300 {
              expected.push((format!("t{i:03}").into_bytes(), 2));
          }
          assert_eq!(collect_all(&mut dict, kw), expected);
          // tx: freqs field, stats/ttf decoded per term
          let tx = fis.by_name("tx").unwrap();
          let mut it = dict.terms_iter(tx);
          let (t, e) = it.next().unwrap().expect("hello");
          assert_eq!(t, b"hello");
          assert_eq!(e.doc_freq, 200);
          let expected_ttf: u64 = (0..200).map(|i| (i % 7) + 1).sum::<u32>() as u64;
          assert_eq!(e.total_term_freq, expected_ttf);
          let (t, e) = it.next().unwrap().expect("world");
          assert_eq!(t, b"world");
          assert_eq!(e.doc_freq, 2);
          assert!(it.next().unwrap().is_none());
          // exhaustion is sticky
          assert!(it.next().unwrap().is_none());
          // field without a .tmd record yields an empty iterator
          let ghost = indexed("ghost", 99, IndexOptions::Docs);
          let mut it = dict.terms_iter(&ghost);
          assert!(it.next().unwrap().is_none());
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn terms_iter_seek_ceil_positions_on_ceiling() {
          let root = temp_dir("iterceil");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_segment(&dir);
          let mut dict = TermsDict::open(&dir, "_0", &[3u8; 16], &fis).unwrap();
          let kw = fis.by_name("kw").unwrap();
          // exact hit: positioned ON the term, iteration continues in order
          let mut it = dict.terms_iter(kw);
          assert!(it.seek_ceil(b"t150").unwrap());
          let (t, e) = it.next().unwrap().expect("t150");
          assert_eq!(t, b"t150");
          assert_eq!(e.doc_freq, 2);
          assert_eq!(it.next().unwrap().unwrap().0, b"t151");
          // miss: ceiling is the next greater term
          let mut it = dict.terms_iter(kw);
          assert!(!it.seek_ceil(b"t150x").unwrap());
          assert_eq!(it.next().unwrap().unwrap().0, b"t151");
          // prefix-of-a-term target: ceiling is the term itself
          let mut it = dict.terms_iter(kw);
          assert!(!it.seek_ceil(b"t15").unwrap());
          assert_eq!(it.next().unwrap().unwrap().0, b"t150");
          // below min -> first term of the dictionary
          let mut it = dict.terms_iter(kw);
          assert!(!it.seek_ceil(b"0").unwrap());
          assert_eq!(it.next().unwrap().unwrap().0, b"a");
          // above max -> exhausted
          let mut it = dict.terms_iter(kw);
          assert!(!it.seek_ceil(b"zzz").unwrap());
          assert!(it.next().unwrap().is_none());
          // exact on the first / last term
          let mut it = dict.terms_iter(kw);
          assert!(it.seek_ceil(b"a").unwrap());
          assert_eq!(it.next().unwrap().unwrap().0, b"a");
          let mut it = dict.terms_iter(kw);
          assert!(it.seek_ceil(b"t299").unwrap());
          assert_eq!(it.next().unwrap().unwrap().0, b"t299");
          assert!(it.next().unwrap().is_none());
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn terms_iter_seek_ceil_then_enumerate_to_end() {
          // popping seek-created (unloaded) ancestor frames across internal
          // sub-blocks and floor siblings: t150 -> t299 is 150 terms. The
          // seek on "t150" floor-adjusts the landing frame into a sibling
          // block, so the pop back to the "t1"/"t" parents exercises the
          // fpOrig group-anchor reload path (Frame.fpOrig vs fp).
          let root = temp_dir("iterpop");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_segment(&dir);
          let mut dict = TermsDict::open(&dir, "_0", &[3u8; 16], &fis).unwrap();
          let kw = fis.by_name("kw").unwrap();
          let mut it = dict.terms_iter(kw);
          it.seek_ceil(b"t150").unwrap();
          let mut got = Vec::new();
          while let Some((t, _)) = it.next().unwrap() {
              got.push(t);
          }
          assert_eq!(got.len(), 150);
          for (i, t) in got.iter().enumerate() {
              assert_eq!(t, &format!("t{:03}", 150 + i).into_bytes(), "position {i}");
          }
          // block-prefix target lands on the subtree's first term
          let mut it = dict.terms_iter(kw);
          assert!(!it.seek_ceil(b"t").unwrap());
          assert_eq!(it.next().unwrap().unwrap().0, b"t000");
          // t099..t299 = 201 terms
          let mut it = dict.terms_iter(kw);
          it.seek_ceil(b"t099").unwrap();
          let mut n = 0;
          while it.next().unwrap().is_some() {
              n += 1;
          }
          assert_eq!(n, 201);
          fs::remove_dir_all(&root).unwrap();
      }
  ```

- [ ] **Step 4.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 terms_iter 2>&1 | tail -5
  error[E0599]: no method named `terms_iter` found for struct `TermsDict`
  ```

- [ ] **Step 4.3: 最小实现** — `crates/codec-lucene9/src/terms_read.rs`：文件头部模块文档中 "Only the seekExact path is implemented" 一句更新为 "seekExact + sequential enumeration (TermsIter)"；在 `impl TermsDict` 末尾（`scan_block` 之后）与 `#[cfg(test)]` 之前追加（无需新增 use，全部符号已在文件内）：

  ```rust
      /// SegmentTermsEnum over one field (search spec M2 §3): sequential
      /// enumeration + seek_ceil, touching only the terms dict (.tim/.tip),
      /// never postings. Terms arrive in dictionary (byte) order.
      pub fn terms_iter(&mut self, field: &FieldInfo) -> TermsIter<'_> {
          TermsIter::new(self, field)
      }
  ```

  然后在 `impl TermsDict` 块结束之后追加（以下为完整新代码）：

  ```rust
  // ===========================================================================
  // TermsIter: sequential block-tree enumerator (SegmentTermsEnum next/seekCeil)
  // ===========================================================================

  /// One block-tree frame (SegmentTermsEnumFrame): prefix length, current block
  /// fp (+ fp_end chaining of floor siblings) and the decoded block blobs.
  /// stats/meta are decoded incrementally as the cursor advances — the same
  /// decoding steps as [`TermsDict::scan_block`], kept across calls.
  struct IterFrame {
      prefix_len: usize,
      fp: u64,      // fp of the block to (re)load
      fp_orig: u64, // fp of the group's first block (scanToSubBlock anchor on pop)
      fp_end: u64,  // end of the loaded block = next floor sibling's fp
      loaded: bool,
      ent_count: usize,
      is_last_in_floor: bool,
      is_leaf: bool,
      suffix_bytes: Vec<u8>,
      suffix_lengths: IndexInput,
      stats: IndexInput,
      meta: IndexInput,
      suffix_pos: usize,
      next_ent: usize,
      singleton_run: u32,
      last_state: TermState,
  }

  impl IterFrame {
      fn new(prefix_len: usize, fp: u64) -> IterFrame {
          IterFrame {
              prefix_len,
              fp,
              fp_orig: fp,
              fp_end: 0,
              loaded: false,
              ent_count: 0,
              is_last_in_floor: true,
              is_leaf: true,
              suffix_bytes: Vec::new(),
              suffix_lengths: IndexInput::in_memory(Vec::new()),
              stats: IndexInput::in_memory(Vec::new()),
              meta: IndexInput::in_memory(Vec::new()),
              suffix_pos: 0,
              next_ent: 0,
              singleton_run: 0,
              // EMPTY_STATE (Lucene912PostingsWriter.java:425-457)
              last_state: TermState {
                  doc_start_fp: 0,
                  pos_start_fp: 0,
                  last_pos_block_offset: -1,
                  singleton_doc_id: -1,
              },
          }
      }
  }

  /// One decoded block entry (SegmentTermsEnumFrame.next :291-298).
  enum NextEntry {
      Term(TermEntry),
      SubBlock(u64),
  }

  /// SegmentTermsEnumFrame.loadBlock (:145-240) into an IterFrame; fp_end
  /// chains floor siblings ("Sub-blocks of a single floor block are always
  /// written one after another", :231-234; writer postings.rs write_block).
  fn load_frame_block(tim_in: &mut IndexInput, frame: &mut IterFrame) -> io::Result<()> {
      tim_in.seek(frame.fp)?;
      let code = tim_in.read_vint()?;
      frame.ent_count = (code >> 1) as usize;
      frame.is_last_in_floor = code & 1 != 0;
      let code_l = tim_in.read_vlong()? as u64;
      frame.is_leaf = code_l & 0x04 != 0;
      let num_suffix_bytes = (code_l >> 3) as usize;
      let compression = code_l & 0x03;
      if compression != 0 {
          return Err(corrupt(format!(
              "unsupported suffix compression {compression} (writer emits NO_COMPRESSION)"
          )));
      }
      frame.suffix_bytes = vec![0u8; num_suffix_bytes];
      tim_in.read_bytes(&mut frame.suffix_bytes)?;
      let mut num_sl_bytes = tim_in.read_vint()? as usize;
      let all_equal = num_sl_bytes & 1 != 0;
      num_sl_bytes >>= 1;
      let mut sl_bytes = vec![0u8; num_sl_bytes];
      if all_equal {
          let b = tim_in.read_byte()?;
          sl_bytes.fill(b);
      } else {
          tim_in.read_bytes(&mut sl_bytes)?;
      }
      let num_stat_bytes = tim_in.read_vint()? as usize;
      let mut stat_bytes = vec![0u8; num_stat_bytes];
      tim_in.read_bytes(&mut stat_bytes)?;
      let num_meta_bytes = tim_in.read_vint()? as usize;
      let mut meta_bytes = vec![0u8; num_meta_bytes];
      tim_in.read_bytes(&mut meta_bytes)?;
      frame.suffix_lengths = IndexInput::in_memory(sl_bytes);
      frame.stats = IndexInput::in_memory(stat_bytes);
      frame.meta = IndexInput::in_memory(meta_bytes);
      frame.suffix_pos = 0;
      frame.next_ent = 0;
      frame.singleton_run = 0;
      frame.last_state = TermState {
          doc_start_fp: 0,
          pos_start_fp: 0,
          last_pos_block_offset: -1,
          singleton_doc_id: -1,
      };
      frame.fp_end = tim_in.file_pointer();
      frame.loaded = true;
      Ok(())
  }

  /// Reads the next entry of a loaded block: suffix (offset, len) + decoded
  /// payload. nextLeaf :300-312 / nextNonLeaf :314-356 for the entry shape,
  /// decodeMetaData :433-481 for stats, decodeTerm :235-277 for metadata.
  fn next_frame_entry(
      frame: &mut IterFrame,
      has_freqs: bool,
      has_positions: bool,
  ) -> io::Result<Option<(usize, usize, NextEntry)>> {
      if frame.next_ent == frame.ent_count {
          return Ok(None);
      }
      frame.next_ent += 1;
      let (suffix_len, is_sub_block) = if frame.is_leaf {
          (frame.suffix_lengths.read_vint()? as usize, false)
      } else {
          let c = frame.suffix_lengths.read_vint()?;
          ((c >> 1) as usize, c & 1 != 0)
      };
      let off = frame.suffix_pos;
      frame.suffix_pos += suffix_len;
      if is_sub_block {
          // back-pointer lives in the suffixLengths stream (:348-349)
          let sub_fp = frame.fp - frame.suffix_lengths.read_vlong()? as u64;
          return Ok(Some((off, suffix_len, NextEntry::SubBlock(sub_fp))));
      }
      // stats (decodeMetaData :433-481)
      let (doc_freq, total_term_freq) = if frame.singleton_run > 0 {
          frame.singleton_run -= 1;
          (1u32, 1u64)
      } else {
          let token = frame.stats.read_vint()?;
          if token & 1 != 0 {
              frame.singleton_run = (token >> 1) as u32;
              (1u32, 1u64)
          } else {
              let df = (token >> 1) as u32;
              let ttf = if has_freqs {
                  df as u64 + frame.stats.read_vlong()? as u64
              } else {
                  df as u64
              };
              (df, ttf)
          }
      };
      // metadata (Lucene912PostingsReader.decodeTerm :235-277)
      let l = frame.meta.read_vlong()? as u64;
      if l & 1 == 0 {
          frame.last_state.doc_start_fp += l >> 1;
          frame.last_state.singleton_doc_id = if doc_freq == 1 {
              frame.meta.read_vint()? as i64
          } else {
              -1
          };
      } else {
          let delta = zigzag_decode(l >> 1);
          frame.last_state.singleton_doc_id += delta;
      }
      if has_positions {
          frame.last_state.pos_start_fp += frame.meta.read_vlong()? as u64;
          frame.last_state.last_pos_block_offset = if total_term_freq > BLOCK_SIZE as u64 {
              frame.meta.read_vlong()?
          } else {
              -1
          };
      }
      Ok(Some((
          off,
          suffix_len,
          NextEntry::Term(TermEntry {
              doc_freq,
              total_term_freq,
              state: frame.last_state,
          }),
      )))
  }

  /// pushFrame (:245-259) + scanToFloorFrame (:361-431) on one FST output:
  /// returns `(group_anchor_fp, floor_adjusted_fp)` — the anchor is the FST
  /// output's raw fp (what the parent's sub-block entry points at, kept as
  /// the frame's `fp_orig` pop anchor), the adjusted fp is the block to scan
  /// (Frame.fp vs Frame.fpOrig in Lucene). Same logic as the inline descent
  /// in [`TermsDict::seek_exact`], factored for `seek_ceil`.
  fn fst_output_block_fp(output: &[u8], term: &[u8], depth: usize) -> io::Result<(u64, u64)> {
      let mut out_in = IndexInput::in_memory(output.to_vec());
      let code = read_msb_vlong(&mut out_in)?;
      let fp_anchor = code >> 2;
      let mut fp = fp_anchor;
      let is_floor = code & OUTPUT_FLAG_IS_FLOOR != 0;
      if is_floor && depth < term.len() {
          let target_label = term[depth];
          let num_follow = out_in.read_vint()? as u32;
          let mut next_label = out_in.read_byte()?;
          if target_label >= next_label {
              for i in 0..num_follow {
                  let sub_code = out_in.read_vlong()? as u64;
                  fp = fp_anchor + (sub_code >> 1);
                  if i + 1 == num_follow {
                      break;
                  }
                  next_label = out_in.read_byte()?;
                  if target_label < next_label {
                      break;
                  }
              }
          }
      }
      Ok((fp_anchor, fp))
  }

  /// Reloads a parent frame after a pop and walks its floor chain to the
  /// sub-block entry pointing at `child_fp_orig`, leaving the cursor just past
  /// it (SegmentTermsEnum.next :1005-1010: scanToFloorFrame + loadBlock +
  /// scanToSubBlock :497-525; the walk is linear because the writer lays a
  /// group's blocks out consecutively — Lucene90BlockTreeTermsWriter
  /// .writeBlocks :661-783).
  fn reload_and_scan_to_sub(
      tim_in: &mut IndexInput,
      frame: &mut IterFrame,
      child_fp_orig: u64,
      has_freqs: bool,
      has_positions: bool,
  ) -> io::Result<()> {
      frame.fp = frame.fp_orig;
      loop {
          load_frame_block(tim_in, frame)?;
          while let Some((_off, _len, entry)) = next_frame_entry(frame, has_freqs, has_positions)? {
              if let NextEntry::SubBlock(fp) = entry {
                  if fp == child_fp_orig {
                      return Ok(());
                  }
              }
          }
          if frame.is_last_in_floor {
              return Err(corrupt("sub-block entry not found while popping the block-tree frame"));
          }
          frame.fp = frame.fp_end;
      }
  }

  /// Sequential block-tree terms enumerator (search spec M2 §3;
  /// SegmentTermsEnum.next :960-1051 / seekCeil :581-837). Take semantics:
  /// after construction the first `next()` yields the dictionary's first
  /// term; after `seek_ceil(t)` the first `next()` yields the first term
  /// >= t. Only the terms dict is touched — postings are never read.
  pub struct TermsIter<'a> {
      dict: &'a mut TermsDict,
      field_index: Option<usize>,
      has_freqs: bool,
      has_positions: bool,
      frames: Vec<IterFrame>,
      term: Vec<u8>,
      pending: Option<(Vec<u8>, TermEntry)>,
      started: bool,
      done: bool,
  }

  impl<'a> TermsIter<'a> {
      fn new(dict: &'a mut TermsDict, field: &FieldInfo) -> TermsIter<'a> {
          let field_index = dict
              .fields
              .iter()
              .position(|f| f.field_number == field.number);
          TermsIter {
              has_freqs: field.index_options != IndexOptions::Docs,
              has_positions: field_has_positions(field),
              field_index,
              dict,
              frames: Vec::new(),
              term: Vec::new(),
              pending: None,
              started: false,
              done: field_index.is_none(),
          }
      }

      /// SegmentTermsEnum.seekCeil (:581-837): FST descent + floor navigation
      /// + block scan with exactOnly=false. Positions the iterator so the next
      /// [`TermsIter::next`] yields the first term >= `target`. Returns true
      /// on an exact hit (SeekStatus.FOUND vs NOT_FOUND/END).
      pub fn seek_ceil(&mut self, target: &[u8]) -> io::Result<bool> {
          self.frames.clear();
          self.term.clear();
          self.pending = None;
          self.started = true;
          self.done = false;
          let Some(field_index) = self.field_index else {
              self.done = true;
              return Ok(false);
          };
          {
              let meta = &self.dict.fields[field_index];
              if target < meta.min_term.as_slice() {
                  // below the dictionary: enumerate from the first term
                  self.init_root()?;
                  return Ok(false);
              }
              if target > meta.max_term.as_slice() {
                  self.done = true;
                  return Ok(false);
              }
          }
          // FST descent (same as seek_exact): (depth, output) candidates,
          // deepest last.
          let mut outs: Vec<(usize, Vec<u8>)> =
              vec![(0, self.dict.fields[field_index].root_code.clone())];
          {
              let traced = self.dict.fst(field_index)?.trace_path(target)?;
              outs.extend(traced);
          }
          let depth = outs.last().unwrap().0;
          let (fp_anchor, fp) = fst_output_block_fp(&outs.last().unwrap().1, target, depth)?;
          // Ancestor frames stay unloaded (reloaded lazily on pop); the
          // deepest frame is floor-adjusted and loaded for the scan. Its
          // fp_orig stays the group anchor (Frame.fpOrig) so that popping
          // back finds the parent's sub-block entry.
          for (d, o) in &outs[..outs.len() - 1] {
              let mut oi = IndexInput::in_memory(o.clone());
              let code = read_msb_vlong(&mut oi)?;
              self.frames.push(IterFrame::new(*d, code >> 2));
          }
          let mut deepest = IterFrame::new(depth, fp);
          deepest.fp_orig = fp_anchor;
          self.frames.push(deepest);
          self.term = target[..depth].to_vec();
          let fi = self.frames.len() - 1;
          load_frame_block(&mut self.dict.tim_in, &mut self.frames[fi])?;
          self.scan_for_ceil(target)
      }

      /// Take the positioned term and advance (SegmentTermsEnum.next
      /// :960-1051).
      pub fn next(&mut self) -> io::Result<Option<(Vec<u8>, TermEntry)>> {
          if self.done {
              return Ok(None);
          }
          if !self.started {
              self.started = true;
              self.init_root()?;
          }
          let out = self.pending.take();
          if out.is_some() {
              self.advance()?;
          }
          Ok(out)
      }

      /// Loads the root block and positions on the first term.
      fn init_root(&mut self) -> io::Result<()> {
          let Some(field_index) = self.field_index else {
              self.done = true;
              return Ok(());
          };
          let root_code = self.dict.fields[field_index].root_code.clone();
          let mut oi = IndexInput::in_memory(root_code);
          let code = read_msb_vlong(&mut oi)?;
          self.frames.clear();
          self.term.clear();
          self.frames.push(IterFrame::new(0, code >> 2));
          let fi = self.frames.len() - 1;
          load_frame_block(&mut self.dict.tim_in, &mut self.frames[fi])?;
          self.advance()
      }

      /// scanToTerm with exactOnly=false (scanToTermLeaf :547-660,
      /// scanToTermNonLeaf :732-830): positions `pending` on the first
      /// term >= target. When the landing block is exhausted, the ceiling is
      /// the next term in dictionary order — `advance` finds it.
      fn scan_for_ceil(&mut self, target: &[u8]) -> io::Result<bool> {
          loop {
              let fi = self.frames.len() - 1;
              let prefix_len = self.frames[fi].prefix_len;
              let mut descended = false;
              while let Some((off, len, entry)) =
                  next_frame_entry(&mut self.frames[fi], self.has_freqs, self.has_positions)?
              {
                  let suffix = self.frames[fi].suffix_bytes[off..off + len].to_vec();
                  let t = if prefix_len <= target.len() {
                      &target[prefix_len..]
                  } else {
                      &[][..]
                  };
                  match suffix.as_slice().cmp(t) {
                      Ordering::Less => continue,
                      Ordering::Equal => match entry {
                          NextEntry::Term(te) => {
                              self.take_pending(&suffix, te);
                              return Ok(true);
                          }
                          // the FST descent consumes every exact sub-block
                          // prefix (compileIndex :490-578), like scan_block
                          NextEntry::SubBlock(_) => {
                              return Err(corrupt("ceil scan hit an exact sub-block match"));
                          }
                      },
                      Ordering::Greater => match entry {
                          NextEntry::Term(te) => {
                              self.take_pending(&suffix, te);
                              return Ok(false);
                          }
                          // the ceiling is the first term of this sub-block
                          // group (scanToTermNonLeaf :805-813): descend
                          NextEntry::SubBlock(sub_fp) => {
                              self.term.truncate(prefix_len);
                              self.term.extend_from_slice(&suffix);
                              self.frames.push(IterFrame::new(self.term.len(), sub_fp));
                              let ni = self.frames.len() - 1;
                              load_frame_block(&mut self.dict.tim_in, &mut self.frames[ni])?;
                              descended = true;
                              break;
                          }
                      },
                  }
              }
              if !descended {
                  // landing block exhausted without finding >= target
                  self.advance()?;
                  return Ok(false);
              }
          }
      }

      /// Records (term, entry) as the positioned (`pending`) term.
      fn take_pending(&mut self, suffix: &[u8], entry: TermEntry) {
          let prefix_len = self.frames.last().unwrap().prefix_len;
          self.term.truncate(prefix_len);
          self.term.extend_from_slice(suffix);
          self.pending = Some((self.term.clone(), entry));
      }

      /// Refills `pending` with the next term in dictionary order
      /// (SegmentTermsEnum.next :960-1051): pops exhausted frames — floor
      /// siblings chain via fp_end (loadNextFloorBlock :126-134) — and pushes
      /// into sub-blocks. Sets `done` at dictionary end.
      fn advance(&mut self) -> io::Result<()> {
          loop {
              // pop exhausted blocks
              loop {
                  let Some(frame) = self.frames.last() else {
                      self.done = true;
                      return Ok(());
                  };
                  if frame.next_ent < frame.ent_count {
                      break;
                  }
                  if !frame.is_last_in_floor {
                      // floor sibling: blocks of a group are consecutive in .tim
                      let fp = frame.fp_end;
                      self.frames.last_mut().unwrap().fp = fp;
                      let fi = self.frames.len() - 1;
                      load_frame_block(&mut self.dict.tim_in, &mut self.frames[fi])?;
                      continue;
                  }
                  // pop to the parent frame
                  let child_fp_orig = self.frames.last().unwrap().fp_orig;
                  self.frames.pop();
                  let Some(parent) = self.frames.last_mut() else {
                      self.done = true;
                      return Ok(());
                  };
                  if !parent.loaded {
                      // seek_ceil-produced ancestor: reload and walk to the
                      // sub-block entry pointing at the child
                      let has_freqs = self.has_freqs;
                      let has_positions = self.has_positions;
                      reload_and_scan_to_sub(
                          &mut self.dict.tim_in,
                          parent,
                          child_fp_orig,
                          has_freqs,
                          has_positions,
                      )?;
                  }
                  let plen = self.frames.last().unwrap().prefix_len;
                  self.term.truncate(plen);
              }
              // consume one entry
              let fi = self.frames.len() - 1;
              let prefix_len = self.frames[fi].prefix_len;
              match next_frame_entry(&mut self.frames[fi], self.has_freqs, self.has_positions)? {
                  None => continue, // raced empty; the pop loop above handles it
                  Some((off, len, NextEntry::SubBlock(sub_fp))) => {
                      self.term.truncate(prefix_len);
                      let suffix = self.frames[fi].suffix_bytes[off..off + len].to_vec();
                      self.term.extend_from_slice(&suffix);
                      self.frames.push(IterFrame::new(self.term.len(), sub_fp));
                      let ni = self.frames.len() - 1;
                      load_frame_block(&mut self.dict.tim_in, &mut self.frames[ni])?;
                  }
                  Some((off, len, NextEntry::Term(te))) => {
                      self.term.truncate(prefix_len);
                      let suffix = self.frames[fi].suffix_bytes[off..off + len].to_vec();
                      self.term.extend_from_slice(&suffix);
                      self.pending = Some((self.term.clone(), te));
                      return Ok(());
                  }
              }
          }
      }
  }
  ```

- [ ] **Step 4.4: 跑测试确认通过**

  ```
  $ cargo test -p codec-lucene9 terms_iter 2>&1 | tail -3
  test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p codec-lucene9 2>&1 | grep -E "^test result"
  test result: ok. 138 passed; 0 failed; 1 ignored; ...（135 基线 + 3 新）
  ```

- [ ] **Step 4.5: 提交**

  ```
  git add crates/codec-lucene9/src/terms_read.rs
  git commit -m "feat: TermsIter block-tree sequential enumerator (seek_ceil + next)"
  ```

---

## Task 5: Prefix 查询（`Query::Prefix` + 电池 prefix 项）

Prefix = `seek_ceil(prefix)` 后 `next()` 直到 `!starts_with(prefix)`（spec §3），收集后走 T2 阈值双路。

**Files:**
- Modify: `crates/core/src/search/segment_reader.rs`（`terms_iter` 转发）
- Modify: `crates/core/src/search/multi_term.rs`（`collect_prefix`）
- Modify: `crates/core/src/search/query.rs`（`Query::Prefix` 变体 + 双臂）
- Modify: `crates/core/src/search/mod.rs`（语义测试）
- Modify: `crates/core/src/bin/rustlucene-cli.rs`（searchdump prefix 电池段）
- Modify: `interop/java/VerifySearchIndex.java`（镜像 prefix 电池段）
- Test: `crates/core/src/search/mod.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T4 的 `TermsDict::terms_iter` / `TermsIter::{seek_ceil, next}`；T2 的 `collect_direct` 返回形状 / `segment_iterator` / `bitset_count` / `CollectedTerms`。
- Produces（T6 复用同一收集形态）:
  ```rust
  // segment_reader.rs
  pub(crate) fn terms_iter(&mut self, field: &str) -> Option<TermsIter<'_>>;
  // multi_term.rs
  pub(crate) fn collect_prefix(seg: &mut SegmentReader, field: &str, prefix: &[u8]) -> io::Result<Option<(bool, CollectedTerms)>>;
  // query.rs
  pub enum Query { .., Prefix { field: String, prefix: Vec<u8> } }
  impl Query { pub fn prefix(field: &str, prefix: &str) -> Query; }
  ```
  语义决定：空 prefix = 枚举全字段字典（field-restricted match-all，等价 Java `PrefixQuery(Term(field, ""))`）；`TermsIter` 借 `&mut SegmentReader`，不跨函数逃逸（collect 在函数内完成）。

### Steps

- [ ] **Step 5.1: 写失败测试** — 追加到 `crates/core/src/search/mod.rs` 的 `mod tests`（复用 T2 的 `write_terms_corpus` / `t_terms`）：

  ```rust
      #[test]
      fn prefix_query_dual_path() {
          let root = temp_dir("prefixdual");
          write_terms_corpus(&root);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          // "t1" -> t10..t19 (10 terms, <=16 OR path); equal to the literal OR
          let t1 = t_terms(10..20);
          let t1_ref: Vec<&str> = t1.iter().map(String::as_str).collect();
          let or_q = Query::or("message", &t1_ref);
          let (a_total, a_docs) = s.top_docs(&or_q, 100).unwrap();
          let pq = Query::prefix("message", "t1");
          let (b_total, b_docs) = s.top_docs(&pq, 100).unwrap();
          assert_eq!((a_total, a_docs), (b_total, b_docs));
          assert_eq!(s.count(&pq).unwrap(), a_total);
          // "t" -> all 20 terms (>16 bitset path); every doc has two t-terms
          let pq = Query::prefix("message", "t");
          assert_eq!(s.count(&pq).unwrap(), 40);
          // empty prefix enumerates the whole field dictionary
          let pq = Query::prefix("message", "");
          assert_eq!(s.count(&pq).unwrap(), 40);
          // exact-term prefix
          let pq = Query::prefix("message", "t07");
          assert_eq!(s.count(&pq).unwrap(), 4);
          // zero-hit prefix / unknown field
          assert_eq!(s.count(&Query::prefix("message", "zzz")).unwrap(), 0);
          assert_eq!(s.count(&Query::prefix("nope", "t")).unwrap(), 0);
          // keyword field (DOCS layout)
          assert_eq!(s.count(&Query::prefix("level", "INF")).unwrap(), 40);
          let pq = Query::prefix("tid", "tid-1");
          let (total, docs) = s.top_docs(&pq, 100).unwrap();
          assert_eq!(total as usize, docs.len());
          assert!(docs.contains(&1) && docs.iter().all(|&d| d == 1 || (10..=19).contains(&d)));
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  注：`level` 字段在语料里全为 "INFO"，`prefix("level","INF")` 命中 40；`tid-1` 前缀命中 tid-1 与 tid-10..tid-19（11 docs）。

- [ ] **Step 5.2: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core prefix_ 2>&1 | tail -5
  error[E0599]: no function or associated item named `prefix` found for enum `Query`
  ```

- [ ] **Step 5.3: 最小实现** —

  `crates/core/src/search/segment_reader.rs`：`use codec_lucene9::terms_read::{TermEntry, TermsDict};` 改为 `use codec_lucene9::terms_read::{TermEntry, TermsDict, TermsIter};`，`field_has_freqs` 之后追加：

  ```rust
      /// TermsIter over a field by name (None = unknown field; a field with no
      /// .tmd record yields an immediately-exhausted iterator). Borrows the
      /// terms dict mutably for the iterator's lifetime.
      pub(crate) fn terms_iter(&mut self, field: &str) -> Option<TermsIter<'_>> {
          let fi = self.field_infos.by_name(field)?;
          Some(self.terms.terms_iter(fi))
      }
  ```

  `crates/core/src/search/multi_term.rs`：`collect_direct` 之后追加：

  ```rust
  /// Prefix collection (spec §3): seek_ceil(prefix) then next() until
  /// !starts_with(prefix). `None` = unknown field (empty-hit semantics).
  pub(crate) fn collect_prefix(
      seg: &mut SegmentReader,
      field: &str,
      prefix: &[u8],
  ) -> io::Result<Option<(bool, CollectedTerms)>> {
      let Some(has_freqs) = seg.field_has_freqs(field) else {
          return Ok(None);
      };
      let mut collected = CollectedTerms {
          terms: Vec::new(),
          entries: Vec::new(),
      };
      {
          let Some(mut it) = seg.terms_iter(field) else {
              return Ok(None);
          };
          it.seek_ceil(prefix)?;
          while let Some((term, entry)) = it.next()? {
              if !term.starts_with(prefix) {
                  break;
              }
              collected.entries.push((entry.doc_freq, entry));
              collected.terms.push(term);
          }
      }
      collected.sort_by_df();
      Ok(Some((has_freqs, collected)))
  }
  ```

  `crates/core/src/search/query.rs`：enum 增加 `Prefix { field: String, prefix: Vec<u8> }`；构造器：

  ```rust
      /// Prefix query (spec M2 §3): all docs whose term starts with `prefix`.
      pub fn prefix(field: &str, prefix: &str) -> Query {
          Query::Prefix {
              field: field.to_string(),
              prefix: prefix.as_bytes().to_vec(),
          }
      }
  ```

  `is_multi_term` 的 matches! 增加 `Query::Prefix { .. }`；`bitset_count` 增加一臂：

  ```rust
              Query::Prefix { field, prefix } => {
                  let Some((has_freqs, collected)) = multi_term::collect_prefix(seg, field, prefix)? else {
                      return Ok(Some(0));
                  };
                  multi_term::bitset_count(seg, has_freqs, &collected)
              }
  ```

  `segment_iterator` 增加一臂：

  ```rust
              Query::Prefix { field, prefix } => {
                  let Some((has_freqs, collected)) = multi_term::collect_prefix(seg, field, prefix)? else {
                      return Ok(None);
                  };
                  multi_term::segment_iterator(seg, field, has_freqs, &collected, needs_freq)
              }
  ```

- [ ] **Step 5.4: 跑测试确认通过**

  ```
  $ cargo test -p rustlucene-core prefix_ 2>&1 | tail -3
  test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test 2>&1 | grep -E "^test result"
      （全 workspace 回归绿）
  ```

- [ ] **Step 5.5: 电池 prefix 项（Rust 侧）** — `crates/core/src/bin/rustlucene-cli.rs` searchdump 的 terms 电池循环之后插入：

  ```rust
      // M2 prefix battery (search spec M2 §3): "connection3" expands to 10
      // terms (<=16 OR path), "conn" to 40 (>16 bitset path); "zzzz" is the
      // zero-hit case and the trace_id prefix hits the df=1 singleton path.
      // Mirrored in VerifySearchIndex.java.
      let prefix_battery: [(&str, &str); 4] = [
          ("level", "IN"),
          ("message", "connection3"),
          ("message", "conn"),
          ("message", "zzzz"),
      ];
      for (field, prefix) in prefix_battery {
          let q = Query::prefix(field, prefix);
          let count = searcher.count(&q)?;
          let (_, docs) = searcher.top_docs(&q, 20)?;
          out.push_str(&format!(
              "prefix {field}={prefix} count={count} first20={}\n",
              doc_csv(&docs)
          ));
      }
      if num_docs > 7 {
          let tid8 = trace_id_of_doc(seed, 7)[..8].to_string();
          let q = Query::prefix("trace_id", &tid8);
          let count = searcher.count(&q)?;
          out.push_str(&format!("prefix trace_id={tid8} count={count}\n"));
      }
  ```

- [ ] **Step 5.6: 电池 prefix 项（Java 侧）** — `interop/java/VerifySearchIndex.java` terms 电池循环之后插入：

  ```java
            // M2 prefix battery: same items/format as searchdump. PrefixQuery
            // is Lucene's counterpart of the Rust TermsIter-driven expansion.
            String[][] prefixBattery = {
                {"level", "IN"},
                {"message", "connection3"},
                {"message", "conn"},
                {"message", "zzzz"},
            };
            for (String[] item : prefixBattery) {
                Query q = new ConstantScoreQuery(new PrefixQuery(new Term(item[0], item[1])));
                TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                StringBuilder b = new StringBuilder();
                for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                out.append("prefix ").append(item[0]).append('=').append(item[1])
                   .append(" count=").append(s.count(q))
                   .append(" first20=").append(b).append('\n');
            }
            if (r.maxDoc() > 7) {
                String tid8 = stored.document(7).get("trace_id").substring(0, 8);
                Query q = new ConstantScoreQuery(new PrefixQuery(new Term("trace_id", tid8)));
                out.append("prefix trace_id=").append(tid8)
                   .append(" count=").append(s.count(q)).append('\n');
            }
  ```

  （`stored` 变量已在 df=1 singleton 段定义，直接复用。）

- [ ] **Step 5.7: 编译 + 电池验证（默认 + bigdict 两变体）**

  ```
  $ cargo build --release 2>&1 | tail -1
  $ make java-classes 2>&1 | tail -1
  $ interop/verify-log.sh 200000 42
  ...
  prefix level=IN count=... first20=...
  prefix message=connection3 count=... first20=...
  prefix message=conn count=... first20=...
  prefix message=zzzz count=0 first20=
  prefix trace_id=........ count=1
  SEARCH_INTEROP_OK
  LOG_INTEROP_OK
  $ interop/verify-log.sh 200000 45 --bigdict
      （高基数字典上 TermsIter 走深层 block-tree，prefix 项照常 diff 全绿）
  ```

- [ ] **Step 5.8: 提交**

  ```
  git add crates/core/src/search/segment_reader.rs crates/core/src/search/multi_term.rs crates/core/src/search/query.rs crates/core/src/search/mod.rs crates/core/src/bin/rustlucene-cli.rs interop/java/VerifySearchIndex.java
  git commit -m "feat: Prefix query via TermsIter + dual-path execution + battery items"
  ```

---

## Task 6: Wildcard 查询（分类 + glob 匹配 + 电池项）

Wildcard 按 spec §5 分类执行：pattern 截到第一个 `*`/`?` 得固定前缀；纯前缀形零过滤；有前缀含通配走前缀枚举 + 尾过滤；无前缀全字典扫 + 过滤。匹配用双指针回溯 glob，chars 迭代（`?` = 单 code point，WildcardQuery.toAutomaton :84-114）。

**Files:**
- Modify: `crates/core/src/search/multi_term.rs`（`WildcardPattern` / `WildcardClass` / `glob_match` / `collect_wildcard` + glob 单测）
- Modify: `crates/core/src/search/query.rs`（`Query::Wildcard` 变体 + 双臂）
- Modify: `crates/core/src/search/mod.rs`（语义测试）
- Modify: `crates/core/src/bin/rustlucene-cli.rs`（searchdump wildcard 电池段）
- Modify: `interop/java/VerifySearchIndex.java`（镜像 wildcard 电池段）
- Test: `crates/core/src/search/multi_term.rs` 与 `crates/core/src/search/mod.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T5 的 `collect_prefix` / `terms_iter`；T2 的双路。
- Produces:
  ```rust
  // multi_term.rs
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub(crate) enum WildcardClass { Exact, PurePrefix, PrefixFilter, FullScan }
  pub(crate) struct WildcardPattern {
      pub pattern: Vec<u8>,
      pub prefix: Vec<u8>,
      pub class: WildcardClass,
  }
  impl WildcardPattern {
      pub(crate) fn parse(pattern: &[u8]) -> WildcardPattern;
      pub(crate) fn matches(&self, term: &[u8]) -> bool;
  }
  pub(crate) fn glob_match(pattern: &str, text: &[u8]) -> bool;
  pub(crate) fn collect_wildcard(seg: &mut SegmentReader, field: &str, pat: &WildcardPattern) -> io::Result<Option<(bool, CollectedTerms)>>;
  // query.rs
  pub enum Query { .., Wildcard { field: String, pattern: Vec<u8> } }
  impl Query { pub fn wildcard(field: &str, pattern: &str) -> Query; }
  ```
  语义决定：无通配符的 pattern（`Exact`）退化为 Term 语义（Lucene WildcardQuery("foo") 同样只精确命中）；pattern 非合法 UTF-8 时过滤类恒不匹配（构造侧 `Query::wildcard(&str)` 保证合法，字节构造的非法输入按不匹配处理，不报错）。

### Steps

- [ ] **Step 6.1: 写失败测试（glob 匹配器）** — 追加到 `crates/core/src/search/multi_term.rs` 末尾新建 `#[cfg(test)] mod tests`：

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn glob_match_basics() {
          assert!(glob_match("foo*", b"foo"));
          assert!(glob_match("foo*", b"foobar"));
          assert!(!glob_match("foo*", b"fo"));
          assert!(glob_match("fo?o", b"fooo"));
          assert!(!glob_match("fo?o", b"foo")); // ? matches exactly one char
          assert!(!glob_match("fo?o", b"fooxo"));
          assert!(glob_match("*foo", b"foo"));
          assert!(glob_match("*foo", b"barfoo"));
          assert!(!glob_match("*foo", b"foob"));
          assert!(glob_match("*", b"anything"));
          assert!(glob_match("*", b""));
          assert!(glob_match("a*b*c", b"aXbYc"));
          assert!(glob_match("a*b*c", b"abc"));
          assert!(!glob_match("a*b*c", b"acb"));
          assert!(glob_match("que?y3*", b"query39"));
          assert!(!glob_match("que?y3*", b"queue39"));
          // consecutive stars collapse semantically
          assert!(glob_match("a**b", b"aXXb"));
          // literal star-less patterns are exact
          assert!(glob_match("abc", b"abc"));
          assert!(!glob_match("abc", b"abd"));
      }

      #[test]
      fn glob_match_chars_semantics() {
          // '?' is one Unicode code point (WildcardQuery.toAutomaton :96-97),
          // not one byte
          assert!(glob_match("h?llo", "héllo".as_bytes()));
          assert!(!glob_match("h?llx", "héllo".as_bytes()));
          assert!(glob_match("h*llo", "héllo".as_bytes()));
          // multi-byte star content
          assert!(glob_match("*", "héllo".as_bytes()));
          // invalid UTF-8 term bytes never match a filter pattern
          assert!(!glob_match("h?llo", b"h\xffllo"));
      }

      #[test]
      fn wildcard_classification() {
          let p = WildcardPattern::parse(b"foo*");
          assert_eq!(p.class, WildcardClass::PurePrefix);
          assert_eq!(p.prefix, b"foo");
          let p = WildcardPattern::parse(b"fo?o*");
          assert_eq!(p.class, WildcardClass::PrefixFilter);
          assert_eq!(p.prefix, b"fo");
          let p = WildcardPattern::parse(b"*foo");
          assert_eq!(p.class, WildcardClass::FullScan);
          assert_eq!(p.prefix, b"");
          let p = WildcardPattern::parse(b"*");
          assert_eq!(p.class, WildcardClass::PurePrefix);
          assert_eq!(p.prefix, b"");
          let p = WildcardPattern::parse(b"?foo");
          assert_eq!(p.class, WildcardClass::FullScan);
          let p = WildcardPattern::parse(b"foo");
          assert_eq!(p.class, WildcardClass::Exact);
          // matches() shortcuts
          assert!(WildcardPattern::parse(b"foo*").matches(b"foobar"));
          assert!(WildcardPattern::parse(b"foo").matches(b"foo"));
          assert!(!WildcardPattern::parse(b"foo").matches(b"fooo"));
      }
  }
  ```

- [ ] **Step 6.2: 写失败测试（语义层）** — 追加到 `crates/core/src/search/mod.rs` 的 `mod tests`：

  ```rust
      #[test]
      fn wildcard_query_classes() {
          let root = temp_dir("wildcards");
          let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
          for i in 0..40 {
              let m = format!("t{:02} t{:02}", i % 20, (i + 7) % 20);
              w.add_document(doc("INFO", &format!("tid-{i}"), &m)).unwrap();
          }
          w.add_document(doc("INFO", "tid-x", "héllo world")).unwrap();
          w.commit().unwrap();
          drop(w);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();

          // pure-prefix shape == the prefix query result (zero filtering)
          let wq = Query::wildcard("message", "t1*");
          let pq = Query::prefix("message", "t1");
          assert_eq!(s.count(&wq).unwrap(), s.count(&pq).unwrap());
          let (a, ad) = s.top_docs(&wq, 100).unwrap();
          let (b, bd) = s.top_docs(&pq, 100).unwrap();
          assert_eq!((a, ad), (b, bd));
          // prefix + wildcard filter: t?7 matches t07,t17 (and t27... but dict has t00..t19)
          let wq = Query::wildcard("message", "t?7");
          let t = t_terms(0..20);
          let hits: Vec<&str> = t.iter().map(String::as_str).filter(|x| x.len() == 3 && x.ends_with('7')).collect();
          let or_q = Query::or("message", &hits);
          assert_eq!(s.count(&wq).unwrap(), s.count(&or_q).unwrap());
          // no-prefix full scan: *7 same term set
          let wq = Query::wildcard("message", "*7");
          assert_eq!(s.count(&wq).unwrap(), s.count(&or_q).unwrap());
          // "*" matches every term in the field dictionary
          let wq = Query::wildcard("message", "*");
          assert_eq!(s.count(&wq).unwrap(), 41);
          // exact degenerate
          let wq = Query::wildcard("message", "t07");
          assert_eq!(s.count(&wq).unwrap(), 4);
          // '?' over a multi-byte char (héllo)
          let wq = Query::wildcard("message", "h?llo");
          assert_eq!(s.count(&wq).unwrap(), 1);
          let wq = Query::wildcard("message", "h?ll");
          assert_eq!(s.count(&wq).unwrap(), 0);
          // zero hit / unknown field
          assert_eq!(s.count(&Query::wildcard("message", "zzz*")).unwrap(), 0);
          assert_eq!(s.count(&Query::wildcard("nope", "*")).unwrap(), 0);
          fs::remove_dir_all(&root).unwrap();
      }
  ```

- [ ] **Step 6.3: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core wildcard 2>&1 | tail -5
  error[E0425]: cannot find function `glob_match` in this scope
  error[E0599]: no function or associated item named `wildcard` found for enum `Query`
  ```

- [ ] **Step 6.4: 最小实现** —

  `crates/core/src/search/multi_term.rs`：`collect_prefix` 之后追加：

  ```rust
  /// Wildcard classification (search spec M2 §5 / 总 spec §3): cut the pattern
  /// at the first '*'/'?' for the fixed prefix.
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub(crate) enum WildcardClass {
      /// No wildcard chars: Term semantics.
      Exact,
      /// prefix + trailing '*' only: prefix enumeration, zero filtering.
      PurePrefix,
      /// Fixed prefix + wildcards later: prefix enumeration + tail filtering.
      PrefixFilter,
      /// Starts with a wildcard: full dictionary scan + filtering.
      FullScan,
  }

  pub(crate) struct WildcardPattern {
      pub pattern: Vec<u8>,
      pub prefix: Vec<u8>,
      pub class: WildcardClass,
  }

  impl WildcardPattern {
      pub(crate) fn parse(pattern: &[u8]) -> WildcardPattern {
          let cut = pattern
              .iter()
              .position(|&b| b == b'*' || b == b'?')
              .unwrap_or(pattern.len());
          let prefix = pattern[..cut].to_vec();
          let rest = &pattern[cut..];
          let class = if rest.is_empty() {
              WildcardClass::Exact
          } else if rest == b"*" {
              WildcardClass::PurePrefix
          } else if prefix.is_empty() {
              WildcardClass::FullScan
          } else {
              WildcardClass::PrefixFilter
          };
          WildcardPattern {
              pattern: pattern.to_vec(),
              prefix,
              class,
          }
      }

      pub(crate) fn matches(&self, term: &[u8]) -> bool {
          match self.class {
              WildcardClass::Exact => term == self.pattern.as_slice(),
              WildcardClass::PurePrefix => term.starts_with(&self.prefix),
              _ => match std::str::from_utf8(&self.pattern) {
                  Ok(p) => glob_match(p, term),
                  Err(_) => false, // non-UTF-8 pattern: matches nothing
              },
          }
      }
  }

  /// Classic two-pointer glob with '*' backtracking, iterated over `char`s so
  /// '?' matches exactly one code point (WildcardQuery.toAutomaton :84-114:
  /// WILDCARD_CHAR → Automata.makeAnyChar, WILDCARD_STRING → makeAnyString).
  pub(crate) fn glob_match(pattern: &str, text: &[u8]) -> bool {
      let p: Vec<char> = pattern.chars().collect();
      let t: Vec<char> = match std::str::from_utf8(text) {
          Ok(s) => s.chars().collect(),
          Err(_) => return false,
      };
      let (mut i, mut j) = (0usize, 0usize);
      let mut star: Option<(usize, usize)> = None; // (pattern idx after '*', text retry idx)
      while j < t.len() {
          if i < p.len() && (p[i] == '?' || p[i] == t[j]) {
              i += 1;
              j += 1;
          } else if i < p.len() && p[i] == '*' {
              star = Some((i + 1, j));
              i += 1;
          } else if let Some((si, sj)) = star {
              i = si;
              j = sj + 1;
              star = Some((si, sj + 1));
          } else {
              return false;
          }
      }
      while i < p.len() && p[i] == '*' {
          i += 1;
      }
      i == p.len()
  }

  /// Wildcard collection (spec §5): Exact → direct seek; PurePrefix → prefix
  /// enumeration with zero filtering; PrefixFilter → prefix enumeration +
  /// glob tail filter; FullScan → whole-dictionary scan + glob filter.
  pub(crate) fn collect_wildcard(
      seg: &mut SegmentReader,
      field: &str,
      pat: &WildcardPattern,
  ) -> io::Result<Option<(bool, CollectedTerms)>> {
      match pat.class {
          WildcardClass::Exact => collect_direct(seg, field, std::slice::from_ref(&pat.pattern)),
          WildcardClass::PurePrefix => collect_prefix(seg, field, &pat.prefix),
          WildcardClass::PrefixFilter | WildcardClass::FullScan => {
              let Some(has_freqs) = seg.field_has_freqs(field) else {
                  return Ok(None);
              };
              let mut collected = CollectedTerms {
                  terms: Vec::new(),
                  entries: Vec::new(),
              };
              {
                  let Some(mut it) = seg.terms_iter(field) else {
                      return Ok(None);
                  };
                  if pat.class == WildcardClass::PrefixFilter {
                      it.seek_ceil(&pat.prefix)?;
                  }
                  while let Some((term, entry)) = it.next()? {
                      if pat.class == WildcardClass::PrefixFilter && !term.starts_with(&pat.prefix) {
                          break;
                      }
                      if pat.matches(&term) {
                          collected.entries.push((entry.doc_freq, entry));
                          collected.terms.push(term);
                      }
                  }
              }
              collected.sort_by_df();
              Ok(Some((has_freqs, collected)))
          }
      }
  }
  ```

  `crates/core/src/search/query.rs`：enum 增加 `Wildcard { field: String, pattern: Vec<u8> }`；构造器：

  ```rust
      /// Wildcard query with '*' and '?' (spec M2 §5 classification).
      pub fn wildcard(field: &str, pattern: &str) -> Query {
          Query::Wildcard {
              field: field.to_string(),
              pattern: pattern.as_bytes().to_vec(),
          }
      }
  ```

  `is_multi_term` 增加 `Query::Wildcard { .. }`；`bitset_count` 增加一臂：

  ```rust
              Query::Wildcard { field, pattern } => {
                  let pat = multi_term::WildcardPattern::parse(pattern);
                  let Some((has_freqs, collected)) = multi_term::collect_wildcard(seg, field, &pat)? else {
                      return Ok(Some(0));
                  };
                  multi_term::bitset_count(seg, has_freqs, &collected)
              }
  ```

  `segment_iterator` 增加一臂：

  ```rust
              Query::Wildcard { field, pattern } => {
                  let pat = multi_term::WildcardPattern::parse(pattern);
                  let Some((has_freqs, collected)) = multi_term::collect_wildcard(seg, field, &pat)? else {
                      return Ok(None);
                  };
                  multi_term::segment_iterator(seg, field, has_freqs, &collected, needs_freq)
              }
  ```

- [ ] **Step 6.5: 跑测试确认通过**

  ```
  $ cargo test -p rustlucene-core glob_ 2>&1 | tail -3
  test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p rustlucene-core wildcard 2>&1 | tail -3
  test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test 2>&1 | grep -E "^test result"
      （全 workspace 回归绿）
  ```

- [ ] **Step 6.6: 电池 wildcard 项（Rust 侧）** — searchdump 的 prefix 电池段之后插入：

  ```rust
      // M2 wildcard battery (search spec M2 §5): "connection*" is the
      // pure-prefix shape (>16 expansion -> bitset), "que?y3*" the
      // prefix+filter shape (<=16 -> OR), "*onnection1" the no-prefix
      // full-scan shape; "*zzz" is the zero-hit case. Mirrored in
      // VerifySearchIndex.java.
      let wildcard_battery: [(&str, &str); 4] = [
          ("message", "connection*"),
          ("message", "que?y3*"),
          ("message", "*onnection1"),
          ("message", "*zzz"),
      ];
      for (field, pattern) in wildcard_battery {
          let q = Query::wildcard(field, pattern);
          let count = searcher.count(&q)?;
          let (_, docs) = searcher.top_docs(&q, 20)?;
          out.push_str(&format!(
              "wildcard {field}={pattern} count={count} first20={}\n",
              doc_csv(&docs)
          ));
      }
  ```

- [ ] **Step 6.7: 电池 wildcard 项（Java 侧）** — prefix 电池段之后插入：

  ```java
            // M2 wildcard battery: same items/format as searchdump.
            String[][] wildcardBattery = {
                {"message", "connection*"},
                {"message", "que?y3*"},
                {"message", "*onnection1"},
                {"message", "*zzz"},
            };
            for (String[] item : wildcardBattery) {
                Query q = new ConstantScoreQuery(new WildcardQuery(new Term(item[0], item[1])));
                TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                StringBuilder b = new StringBuilder();
                for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                out.append("wildcard ").append(item[0]).append('=').append(item[1])
                   .append(" count=").append(s.count(q))
                   .append(" first20=").append(b).append('\n');
            }
  ```

- [ ] **Step 6.8: 编译 + 电池验证（默认 + bigdict 两变体）**

  ```
  $ cargo build --release 2>&1 | tail -1
  $ make java-classes 2>&1 | tail -1
  $ interop/verify-log.sh 200000 42
  ...
  wildcard message=connection* count=... first20=...
  wildcard message=que?y3* count=... first20=...
  wildcard message=*onnection1 count=... first20=...
  wildcard message=*zzz count=0 first20=
  SEARCH_INTEROP_OK
  LOG_INTEROP_OK
  $ interop/verify-log.sh 200000 45 --bigdict
      （全绿；bigdict 变体同样覆盖 wildcard 三形态）
  ```

- [ ] **Step 6.9: 提交**

  ```
  git add crates/core/src/search/multi_term.rs crates/core/src/search/query.rs crates/core/src/search/mod.rs crates/core/src/bin/rustlucene-cli.rs interop/java/VerifySearchIndex.java
  git commit -m "feat: Wildcard query (prefix classification + glob matcher) + battery items"
  ```

---

## Task 7: PositionsEnum——.pos 读侧 + advance 重同步（`postings_read.rs` 追加）

读 `.pos`（满 128 块 PFOR + tail per-delta VInt，逐字节镜像写侧 `write_positions`），与 DocsFreqsEnum 并行消费；**advance 重同步**照 EverythingEnum 的 posPendingCount/payFP 机制：skip entry 里的 `(pos_fp delta, pos_buffer_upto)` 从"整段跳过"改为解析（关键设计事实 3/4）。这是本阶段最精细的任务。

**Files:**
- Modify: `crates/codec-lucene9/src/postings_read.rs`（`PostingsReader` 增 `pos_in`、`EnumCore` 增 pos 状态、`PositionsEnum`）
- Test: `crates/codec-lucene9/src/postings_read.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: 写侧 `write_positions`（`postings.rs:478-541`）与 level-0/level-1 skip 写出（`postings.rs:576-695`）；现有 `EnumCore` / `advance` / `pfor_util_skip` / `pfor_util_decode` / `read_vint15` / `read_vlong15`；`codec_util::corrupt`。
- Produces（T8 的 `PhraseDocIter` 依赖这些签名，不得改名）:
  ```rust
  // postings_read.rs
  impl PostingsReader {
      /// EverythingEnum: docs+freqs+positions over a field with
      /// IndexOptions >= DOCS_AND_FREQS_AND_POSITIONS. Errors when the
      /// segment has no .pos file.
      pub fn positions(&self, entry: &TermEntry) -> io::Result<PositionsEnum>;
  }
  pub struct PositionsEnum { .. }
  impl PositionsEnum {
      pub fn doc_id(&self) -> i32;
      pub fn next_doc(&mut self) -> io::Result<i32>;
      pub fn advance(&mut self, target: i32) -> io::Result<i32>;
      pub fn freq(&self) -> u32;
      /// EverythingEnum.nextPosition (:1156-1187): current doc's next
      /// position. Panics if called more than freq() times per doc.
      pub fn next_position(&mut self) -> io::Result<u32>;
  }
  ```
  语义决定：`PositionsEnum` 只服务 has_positions 字段，内部 `decode_freqs` 恒 true（phrase 需要 freq 指导 position 消费）；df=1 singleton 无 .doc 字节也无 skip entry，pos 流从 `pos_start_fp` 顺序读（EverythingEnum.reset :770-826）；本系统写侧无 payload/offset，refillPositions 只实现其无 payload 分支。

### Steps

- [ ] **Step 7.1: 写失败测试** — 追加到 `crates/codec-lucene9/src/postings_read.rs` 的 `mod tests`：

  ```rust
      /// px (DOCS_AND_FREQS_AND_POSITIONS):
      /// "hot" df=5000 dense, freqs (i%3)+1, per-doc positions [0,2,..] —
      ///     ttf=9999: 78 full .pos PFOR blocks + tail 15, level-0/level-1 skips
      /// "warm" df=200 docs step 3, freqs (i%4)+1, positions [1,3,5,..] — varied deltas
      /// "one" df=1 singleton doc 42 freq 7, positions [0,1,2,3,4,5,6]
      fn write_segment_pos(dir: &FSDirectory) -> (FieldInfos, Vec<Vec<u32>>, Vec<Vec<u32>>) {
          let id = [5u8; 16];
          let px = indexed("px", 0, IndexOptions::DocsAndFreqsAndPositions);
          let mut w = PostingsWriter::new(dir, "_0", &id).unwrap();
          w.start_field(&px, 6000).unwrap();
          let hot_docs: Vec<u32> = (0..5000).collect();
          let hot_freqs: Vec<u32> = (0..5000).map(|i| (i % 3) + 1).collect();
          let hot_pos: Vec<Vec<u32>> = hot_freqs
              .iter()
              .map(|&f| (0..f).map(|k| k * 2).collect())
              .collect();
          w.write_term(b"hot", &hot_docs, &hot_freqs, Some(&hot_pos)).unwrap();
          let one_pos: Vec<Vec<u32>> = vec![(0..7).collect()];
          w.write_term(b"one", &[42], &[7], Some(&one_pos)).unwrap();
          let warm_docs: Vec<u32> = (0..200).map(|i| i * 3).collect();
          let warm_freqs: Vec<u32> = (0..200).map(|i| (i % 4) + 1).collect();
          let warm_pos: Vec<Vec<u32>> = warm_freqs
              .iter()
              .map(|&f| (0..f).map(|k| k * 2 + 1).collect())
              .collect();
          w.write_term(b"warm", &warm_docs, &warm_freqs, Some(&warm_pos)).unwrap();
          w.finish_field().unwrap();
          w.finish().unwrap();
          let fis = FieldInfos::new(vec![px]);
          fis.write(dir, "_0", &id).unwrap();
          (fis, hot_pos, warm_pos)
      }

      fn seek_pos(dir: &FSDirectory, fis: &FieldInfos, term: &[u8]) -> TermEntry {
          let mut dict = crate::terms_read::TermsDict::open(dir, "_0", &[5u8; 16], fis).unwrap();
          let fi = fis.by_name("px").unwrap();
          dict.seek_exact(fi, term).unwrap().expect("term must exist")
      }

      /// Drive (next_doc + freq × next_position) over the whole list.
      fn drain_positions(en: &mut PositionsEnum) -> Vec<(i32, Vec<u32>)> {
          let mut out = Vec::new();
          loop {
              let d = en.next_doc().unwrap();
              if d == NO_MORE_DOCS {
                  break;
              }
              let f = en.freq();
              let mut ps = Vec::with_capacity(f as usize);
              for _ in 0..f {
                  ps.push(en.next_position().unwrap());
              }
              out.push((d, ps));
          }
          out
      }

      #[test]
      fn positions_sequential_round_trip() {
          let root = temp_dir("posseq");
          let dir = FSDirectory::open(&root).unwrap();
          let (fis, hot_pos, warm_pos) = write_segment_pos(&dir);
          let postings = PostingsReader::open(&dir, "_0", &[5u8; 16]).unwrap();
          // hot: 5000 docs dense, crosses level-1 (4096) and 38 level-0 boundaries
          let e = seek_pos(&dir, &fis, b"hot");
          assert_eq!(e.total_term_freq, 9999);
          let mut en = postings.positions(&e).unwrap();
          let got = drain_positions(&mut en);
          assert_eq!(got.len(), 5000);
          for (d, ps) in &got {
              assert_eq!(ps, &hot_pos[*d as usize], "doc {d}");
          }
          // warm: varied deltas
          let e = seek_pos(&dir, &fis, b"warm");
          let mut en = postings.positions(&e).unwrap();
          let got = drain_positions(&mut en);
          assert_eq!(got.len(), 200);
          for (i, (d, ps)) in got.iter().enumerate() {
              assert_eq!(*d, (i * 3) as i32);
              assert_eq!(ps, &warm_pos[i], "warm doc index {i}");
          }
          // singleton: no .doc bytes, positions straight from pos_start_fp
          let e = seek_pos(&dir, &fis, b"one");
          let mut en = postings.positions(&e).unwrap();
          assert_eq!(en.next_doc().unwrap(), 42);
          assert_eq!(en.freq(), 7);
          for p in 0..7 {
              assert_eq!(en.next_position().unwrap(), p);
          }
          assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn positions_advance_resync() {
          let root = temp_dir("posadv");
          let dir = FSDirectory::open(&root).unwrap();
          let (fis, hot_pos, _) = write_segment_pos(&dir);
          let postings = PostingsReader::open(&dir, "_0", &[5u8; 16]).unwrap();
          let e = seek_pos(&dir, &fis, b"hot");
          // fresh enum, advance deep (level-1 + level-0 skip) then read positions
          let mut en = postings.positions(&e).unwrap();
          assert_eq!(en.advance(1300).unwrap(), 1300);
          assert_eq!(en.freq(), 2);
          let ps: Vec<u32> = (0..en.freq()).map(|_| en.next_position().unwrap()).collect();
          assert_eq!(ps, hot_pos[1300]);
          // advance to the level-1 boundary doc and past it
          assert_eq!(en.advance(4095).unwrap(), 4095);
          let _: Vec<u32> = (0..en.freq()).map(|_| en.next_position().unwrap()).collect();
          assert_eq!(en.advance(4096).unwrap(), 4096);
          let ps: Vec<u32> = (0..en.freq()).map(|_| en.next_position().unwrap()).collect();
          assert_eq!(ps, hot_pos[4096]);
          // into the doc tail (df % 128 != 0 region)
          assert_eq!(en.advance(4999).unwrap(), 4999);
          let ps: Vec<u32> = (0..en.freq()).map(|_| en.next_position().unwrap()).collect();
          assert_eq!(ps, hot_pos[4999]);
          assert_eq!(en.advance(5000).unwrap(), NO_MORE_DOCS);
          assert_eq!(en.advance(9999).unwrap(), NO_MORE_DOCS); // sticky
          // advance to the same doc twice must not consume positions
          let mut en = postings.positions(&e).unwrap();
          assert_eq!(en.advance(200).unwrap(), 200);
          assert_eq!(en.advance(200).unwrap(), 200);
          let ps: Vec<u32> = (0..en.freq()).map(|_| en.next_position().unwrap()).collect();
          assert_eq!(ps, hot_pos[200]);
          // every target: advance == linear scan, positions of the landed doc
          for target in [0i32, 1, 127, 128, 4223, 4224, 4998] {
              let mut en = postings.positions(&e).unwrap();
              assert_eq!(en.advance(target).unwrap(), target, "target {target}");
              let ps: Vec<u32> = (0..en.freq()).map(|_| en.next_position().unwrap()).collect();
              assert_eq!(ps, hot_pos[target as usize], "target {target}");
          }
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn positions_skip_positions_catch_up() {
          // docs consumed WITHOUT reading their positions: the next
          // next_position must skip the backlog (skipPositions :1031-1082)
          let root = temp_dir("posskip");
          let dir = FSDirectory::open(&root).unwrap();
          let (fis, hot_pos, _) = write_segment_pos(&dir);
          let postings = PostingsReader::open(&dir, "_0", &[5u8; 16]).unwrap();
          let e = seek_pos(&dir, &fis, b"hot");
          let mut en = postings.positions(&e).unwrap();
          for _ in 0..205 {
              en.next_doc().unwrap();
          }
          // now at doc 204, never read a single position
          assert_eq!(en.doc_id(), 204);
          let ps: Vec<u32> = (0..en.freq()).map(|_| en.next_position().unwrap()).collect();
          assert_eq!(ps, hot_pos[204]);
          // move on to doc 205 and read it fully, then skip 206-209's
          // positions via advance (buffer-local catch-up)
          assert_eq!(en.next_doc().unwrap(), 205);
          let ps205: Vec<u32> = (0..en.freq()).map(|_| en.next_position().unwrap()).collect();
          assert_eq!(ps205, hot_pos[205]);
          assert_eq!(en.advance(210).unwrap(), 210);
          let ps: Vec<u32> = (0..en.freq()).map(|_| en.next_position().unwrap()).collect();
          assert_eq!(ps, hot_pos[210]);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn positions_missing_pos_file_is_error() {
          // a segment written without any positions field has no .pos file
          let root = temp_dir("posnone");
          let dir = FSDirectory::open(&root).unwrap();
          let (fis, _, _) = write_segment(&dir); // kw/tx, no positions
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
          let e = seek(&dir, &fis, "kw", b"big");
          assert!(postings.positions(&e).is_err());
          fs::remove_dir_all(&root).unwrap();
      }
  ```

- [ ] **Step 7.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 positions_ 2>&1 | tail -5
  error[E0599]: no method named `positions` found for struct `PostingsReader`
  ```

- [ ] **Step 7.3: 最小实现** — `crates/codec-lucene9/src/postings_read.rs`：

  (a) `use crate::codec_util::{check_footer, check_footer_structure, check_index_header};` 改为 `use crate::codec_util::{check_footer, check_footer_structure, check_index_header, corrupt};`。

  (b) `PostingsReader` 增 `pos_in`：

  ```rust
  /// Owns the segment's .doc stream (Lucene912PostingsReader :83-206) plus
  /// the .pos stream when the segment has any positions field (:135-177).
  pub struct PostingsReader {
      doc_in: IndexInput,
      pos_in: Option<IndexInput>,
  }
  ```

  `open` 中把 `let _pos_len = psm.read_long()?;`（当前在 `if dir.file_exists(...)` 分支内）改为读入 `pos_len`，并在既有 `doc_in` 校验之后打开 `.pos`——整个 `open` 方法体从 `let doc_len = ...` 起替换为：

  ```rust
      let doc_len = psm.read_long()? as u64;
      // posLen is present iff the writer created a .pos file (:106).
      let pos_len = if dir.file_exists(&file_name(segment, "pos")) {
          Some(psm.read_long()? as u64)
      } else {
          None
      };
      check_footer(&mut psm)?;
      let mut doc_in = dir.open_input(&file_name(segment, "doc"))?;
      check_index_header(
          &mut doc_in,
          DOC_CODEC,
          POSTINGS_VERSION,
          POSTINGS_VERSION,
          segment_id,
          SEGMENT_SUFFIX,
      )?;
      // retrieveChecksum (:150-172): length + trailing footer structure
      check_footer_structure(&doc_in, doc_len)?;
      let pos_in = match pos_len {
          Some(len) => {
              let mut pos_in = dir.open_input(&file_name(segment, "pos"))?;
              check_index_header(
                  &mut pos_in,
                  POS_CODEC,
                  POSTINGS_VERSION,
                  POSTINGS_VERSION,
                  segment_id,
                  SEGMENT_SUFFIX,
              )?;
              // retrieveChecksum (:160-161)
              check_footer_structure(&pos_in, len)?;
              Some(pos_in)
          }
          None => None,
      };
      Ok(PostingsReader { doc_in, pos_in })
  ```

  `use crate::postings::{file_name, DOC_CODEC, POSTINGS_VERSION, PSM_CODEC, SEGMENT_SUFFIX};` 改为 `use crate::postings::{file_name, DOC_CODEC, POS_CODEC, POSTINGS_VERSION, PSM_CODEC, SEGMENT_SUFFIX};`（`POS_CODEC` 需从 `postings.rs` 的 `pub(crate)` 常量引入——该常量已存在）。

  `positions` 构造器（`docs_and_freqs_no_freq` 之后追加）：

  ```rust
      /// Docs+freqs+positions iterator (EverythingEnum), for fields with
      /// IndexOptions >= DOCS_AND_FREQS_AND_POSITIONS.
      pub fn positions(&self, entry: &TermEntry) -> io::Result<PositionsEnum> {
          let pos_in = self
              .pos_in
              .as_ref()
              .ok_or_else(|| corrupt("positions enum requested but the segment has no .pos file"))?;
          Ok(PositionsEnum {
              core: EnumCore::new_with_positions(
                  self.fresh_input()?,
                  pos_in.slice(0, pos_in.length())?,
                  entry,
              )?,
          })
      }
  ```

  (c) `EnumCore` 增 pos 状态（关键设计事实 3/4）。在结构体 `EnumCore` 的既有字段 `doc_buffer_upto: usize,` 之后追加字段：

  ```rust
      has_positions: bool,
      // level-0/level-1 pos skip state (EverythingEnum :696-710); the pos fp
      // deltas chain across blocks, first block's base = posTermStartFP
      // (writer postings.rs:566-568).
      level0_pos_end_fp: u64,
      level0_block_pos_upto: u64,
      level1_pos_end_fp: u64,
      level1_block_pos_upto: u64,
      pos: Option<PosCore>,
  ```

  并在 `EnumCore` 定义之后新增 `PosCore`：

  ```rust
  /// EverythingEnum position state (:650-710): .pos stream + pending
  /// bookkeeping. No payloads/offsets exist in this system's indexes, so only
  /// the delta buffer is kept.
  struct PosCore {
      pos_in: IndexInput,
      /// File pointer of the tail (VInt) block; -1 when ttf == BLOCK_SIZE
      /// (EverythingEnum.reset :789-797).
      last_pos_block_fp: i64,
      pos_delta_buffer: [u32; BLOCK_SIZE],
      pos_buffer_upto: usize,
      /// How many positions "behind" we are; next_position catches up
      /// (:675-679).
      pos_pending_count: u64,
      position: u32,
  }
  ```

  `EnumCore::new` 签名不变，初始化追加 `has_positions: false, level0_pos_end_fp: 0, level0_block_pos_upto: 0, level1_pos_end_fp: 0, level1_block_pos_upto: 0, pos: None`。新增：

  ```rust
      /// EverythingEnum.reset (:770-826) for the positions profile: freqs
      /// always decoded (phrase needs them), pos state initialized from the
      /// term state.
      fn new_with_positions(
          doc_in: IndexInput,
          mut pos_in: IndexInput,
          entry: &TermEntry,
      ) -> io::Result<EnumCore> {
          let mut c = EnumCore::new(doc_in, entry, true, true)?;
          c.has_positions = true;
          c.level0_pos_end_fp = entry.state.pos_start_fp;
          c.level1_pos_end_fp = entry.state.pos_start_fp;
          // lastPosBlockFP (:789-797): tail block fp; -1 when ttf == BLOCK_SIZE
          let last_pos_block_fp = if entry.total_term_freq < BLOCK_SIZE as u64 {
              entry.state.pos_start_fp as i64
          } else if entry.total_term_freq == BLOCK_SIZE as u64 {
              -1
          } else {
              entry.state.pos_start_fp as i64 + entry.state.last_pos_block_offset
          };
          pos_in.seek(entry.state.pos_start_fp)?;
          c.pos = Some(PosCore {
              pos_in,
              last_pos_block_fp,
              pos_delta_buffer: [0; BLOCK_SIZE],
              pos_buffer_upto: BLOCK_SIZE,
              pos_pending_count: 0,
              position: 0,
          });
          Ok(c)
      }

      /// EverythingEnum block-boundary resync (:908-917 / :962-970): when the
      /// .pos decode cursor has not passed the upcoming doc block's start fp,
      /// seek it there and account the already-consumed positions of the
      /// pos-block containing the boundary.
      fn resync_pos_stream(&mut self) -> io::Result<()> {
          if let Some(pos) = &mut self.pos {
              if self.level0_pos_end_fp >= pos.pos_in.file_pointer() {
                  pos.pos_in.seek(self.level0_pos_end_fp)?;
                  pos.pos_pending_count = self.level0_block_pos_upto;
                  pos.pos_buffer_upto = BLOCK_SIZE;
              }
          }
          Ok(())
      }
  ```

  (d) `next_doc` 增 pos 记账（EverythingEnum.nextDoc :940-952；freq 先取出再借 pos，避免借用冲突）：

  ```rust
      fn next_doc(&mut self) -> io::Result<i32> {
          if self.doc == NO_MORE_DOCS as i64 {
              return Ok(NO_MORE_DOCS);
          }
          if self.doc == self.level0_last_doc {
              self.move_to_next_level0_block()?;
          }
          self.doc = self.doc_buffer[self.doc_buffer_upto] as i64;
          self.doc_buffer_upto += 1;
          if self.pos.is_some() && self.doc != NO_MORE_DOCS as i64 {
              let f = self.freq() as u64; // :948 — freq of the doc just returned
              let pos = self.pos.as_mut().unwrap();
              pos.pos_pending_count += f;
              pos.position = 0; // :951
          }
          Ok(self.doc as i32)
      }
  ```

  (e) `move_to_next_level0_block` 整个方法替换为（EverythingEnum.moveToNextLevel0Block :899-937 的 positions 分支 + 既有路径逐字保留）：

  ```rust
      /// moveToNextLevel0Block (:573-587) + EverythingEnum's positions variant
      /// (:899-937): the has_positions branch parses the level-0 skip entry
      /// instead of skipping it wholesale and resyncs the .pos stream first.
      fn move_to_next_level0_block(&mut self) -> io::Result<()> {
          if self.doc == self.level1_last_doc {
              self.skip_level1_to(self.doc + 1)?;
          }
          self.prev_doc_id = self.level0_last_doc;
          if self.has_positions {
              // resync BEFORE parsing the new skip entry (:908-917 uses the
              // boundary fp of the block being entered)
              self.resync_pos_stream()?;
              if self.doc_freq - self.doc_count_upto >= BLOCK_SIZE as u32 {
                  let _skip0_num_bytes = self.doc_in.read_vlong()?;
                  let doc_delta = read_vint15(&mut self.doc_in)?;
                  self.level0_last_doc += doc_delta as i64;
                  let _block_total_bytes = read_vlong15(&mut self.doc_in)?;
                  let impact_bytes = self.doc_in.read_vlong()? as u64;
                  self.doc_in.skip_bytes(impact_bytes)?;
                  self.level0_pos_end_fp += self.doc_in.read_vlong()? as u64; // :926
                  self.level0_block_pos_upto = self.doc_in.read_byte()? as u64; // :927
                  self.refill_full_block()?;
              } else {
                  self.level0_last_doc = NO_MORE_DOCS as i64;
                  self.refill_remainder()?;
              }
              return Ok(());
          }
          if self.doc_freq - self.doc_count_upto >= BLOCK_SIZE as u32 {
              let skip0_num_bytes = self.doc_in.read_vlong()? as u64;
              self.doc_in.skip_bytes(skip0_num_bytes)?;
              self.refill_full_block()?;
              self.level0_last_doc = self.doc_buffer[BLOCK_SIZE - 1] as i64;
          } else {
              self.level0_last_doc = NO_MORE_DOCS as i64;
              self.refill_remainder()?;
          }
          Ok(())
      }
  ```

  (f) `skip_level1_to` 的 has_positions 解析（EverythingEnum.skipLevel1To :860-897；pos delta 链每条记录都必须解析）：

  ```rust
      fn skip_level1_to(&mut self, target: i64) -> io::Result<()> {
          loop {
              self.prev_doc_id = self.level1_last_doc;
              self.level0_last_doc = self.level1_last_doc;
              if self.has_positions {
                  // carry level-1 pos state into level 0 (:854-856)
                  self.level0_pos_end_fp = self.level1_pos_end_fp;
                  self.level0_block_pos_upto = self.level1_block_pos_upto;
              }
              self.doc_in.seek(self.level1_doc_end_fp)?;
              self.doc_count_upto = self.level1_doc_count_upto;
              self.level1_doc_count_upto += LEVEL1_NUM_DOCS;
              if self.doc_freq - self.doc_count_upto < LEVEL1_NUM_DOCS {
                  self.level1_last_doc = NO_MORE_DOCS as i64;
                  break;
              }
              self.level1_last_doc += self.doc_in.read_vint()? as i64;
              self.level1_doc_end_fp =
                  self.doc_in.read_vlong()? as u64 + self.doc_in.file_pointer();
              if self.has_freqs && self.has_positions {
                  // parse the numSkipBytes section EVERY record (:883-886):
                  // Short numSkipBytes, Short impactBytes + impacts,
                  // VLong posFpDelta, Byte posBufferUpto
                  let num_skip_bytes = self.doc_in.read_short()? as u16 as u64;
                  let skip1_end_fp = num_skip_bytes + self.doc_in.file_pointer();
                  let impact_bytes = self.doc_in.read_short()? as u16 as u64;
                  self.doc_in.skip_bytes(impact_bytes)?;
                  self.level1_pos_end_fp += self.doc_in.read_vlong()? as u64;
                  self.level1_block_pos_upto = self.doc_in.read_byte()? as u64;
                  debug_assert_eq!(self.doc_in.file_pointer(), skip1_end_fp); // :891
              } else if self.has_freqs && self.level1_last_doc >= target {
                  let num_skip_bytes = self.doc_in.read_short()? as u16 as u64;
                  self.doc_in.skip_bytes(num_skip_bytes)?;
              }
              if self.level1_last_doc >= target {
                  break;
              }
          }
          Ok(())
      }
  ```

  注：非 positions 路径行为与现状逐字节一致（`numSkipBytes` 段只在 break 记录上跳过）；has_positions 路径逐条解析以保证 `level1_pos_end_fp` 的 delta 链完整。

  (g) `skip_level0_to` 整个方法替换为（EverythingEnum.skipLevel0To :954-1003 的 positions 解析 + 既有路径逐字保留）：

  ```rust
      /// skipLevel0To (:548-571) + EverythingEnum's positions variant
      /// (:954-1003): the has_positions branch parses impacts/pos fields of
      /// every skip entry (pos fp deltas chain across blocks) and resyncs the
      /// .pos stream per skipped block.
      fn skip_level0_to(&mut self, target: i64) -> io::Result<()> {
          loop {
              self.prev_doc_id = self.level0_last_doc;
              if self.has_positions {
                  // :958-975 — resync to the block boundary, or (positions
                  // already decoded past it) accumulate the remaining docs'
                  // freqs of the current buffer instead of seeking backwards
                  if self.level0_pos_end_fp >= self.pos.as_ref().unwrap().pos_in.file_pointer() {
                      self.resync_pos_stream()?;
                  } else {
                      let upto = self.doc_buffer_upto;
                      let pos = self.pos.as_mut().unwrap();
                      for i in upto..BLOCK_SIZE {
                          pos.pos_pending_count += self.freq_buffer[i] as u64;
                      }
                  }
              }
              if self.doc_freq - self.doc_count_upto >= BLOCK_SIZE as u32 {
                  if self.has_positions {
                      let _skip0_num_bytes = self.doc_in.read_vlong()?;
                      let doc_delta = read_vint15(&mut self.doc_in)?;
                      self.level0_last_doc += doc_delta as i64;
                      let block_total_bytes = read_vlong15(&mut self.doc_in)? as u64;
                      // blockTotalBytes counts from HERE (after the vlong15)
                      // to the end of the packed data (:983); the impacts/pos
                      // fields parsed below are part of it, so skipping the
                      // block must seek to blockEndFP (:997), not skip_bytes
                      // (which would overshoot by the parsed fields' length)
                      let block_end_fp = self.doc_in.file_pointer() + block_total_bytes;
                      let impact_bytes = self.doc_in.read_vlong()? as u64;
                      self.doc_in.skip_bytes(impact_bytes)?;
                      self.level0_pos_end_fp += self.doc_in.read_vlong()? as u64; // :986
                      self.level0_block_pos_upto = self.doc_in.read_byte()? as u64; // :987
                      if target <= self.level0_last_doc {
                          break;
                      }
                      self.doc_in.seek(block_end_fp)?;
                      self.doc_count_upto += BLOCK_SIZE as u32;
                  } else {
                      let skip0_num_bytes = self.doc_in.read_vlong()? as u64;
                      // end offset of skip data (before the actual data starts)
                      let skip0_end_fp = self.doc_in.file_pointer() + skip0_num_bytes;
                      let doc_delta = read_vint15(&mut self.doc_in)?;
                      self.level0_last_doc += doc_delta as i64;
                      if target <= self.level0_last_doc {
                          self.doc_in.seek(skip0_end_fp)?;
                          break;
                      }
                      // skip block
                      let block_total_bytes = read_vlong15(&mut self.doc_in)?;
                      self.doc_in.skip_bytes(block_total_bytes)?;
                      self.doc_count_upto += BLOCK_SIZE as u32;
                  }
              } else {
                  self.level0_last_doc = NO_MORE_DOCS as i64;
                  break;
              }
          }
          Ok(())
      }
  ```

  (h) 自由函数 `advance` 的 pos 记账（EverythingEnum.advance :1007-1029）：

  ```rust
  fn advance(core: &mut EnumCore, target: i32) -> io::Result<i32> {
      let t = target as i64;
      if core.doc >= t {
          return Ok(core.doc as i32);
      }
      if core.doc == NO_MORE_DOCS as i64 {
          return Ok(NO_MORE_DOCS);
      }
      if t > core.level0_last_doc {
          if t > core.level1_last_doc {
              core.skip_level1_to(t)?;
          }
          core.skip_level0_to(t)?;
          if core.doc_freq - core.doc_count_upto >= BLOCK_SIZE as u32 {
              core.refill_full_block()?;
          } else {
              core.refill_remainder()?;
          }
      }
      let mut upto = core.doc_buffer_upto;
      let from = upto;
      while (core.doc_buffer[upto] as i64) < t {
          upto += 1;
      }
      core.doc = core.doc_buffer[upto] as i64;
      core.doc_buffer_upto = upto + 1;
      if let Some(pos) = &mut core.pos {
          if core.doc != NO_MORE_DOCS as i64 {
              // :1020-1025 — positions of the docs skipped inside the buffer,
              // plus the landed doc's own freq, become pending
              for i in from..=upto {
                  pos.pos_pending_count += core.freq_buffer[i] as u64;
              }
              pos.position = 0;
          }
      }
      Ok(core.doc as i32)
  }
  ```

  (i) `PositionsEnum` + `next_position` / `skip_positions` / `refill_positions`（`DocsFreqsEnum` 之后追加）：

  ```rust
  /// EverythingEnum.skipPositions (:1031-1082), positions-only profile:
  /// steps over the `pos_pending_count - freq` deltas that precede the
  /// current doc's positions in the .pos stream.
  fn skip_positions(pos: &mut PosCore, freq: u64, total_term_freq: u64) -> io::Result<()> {
      let mut to_skip = pos.pos_pending_count - freq;
      let left_in_block = (BLOCK_SIZE - pos.pos_buffer_upto) as u64;
      if to_skip < left_in_block {
          pos.pos_buffer_upto += to_skip as usize;
      } else {
          to_skip -= left_in_block;
          while to_skip >= BLOCK_SIZE as u64 {
              pfor_util_skip(&mut pos.pos_in)?;
              to_skip -= BLOCK_SIZE as u64;
          }
          refill_positions(pos, total_term_freq)?;
          pos.pos_buffer_upto = to_skip as usize;
      }
      pos.position = 0;
      Ok(())
  }

  /// EverythingEnum.refillPositions (:1084-1153) without payloads/offsets:
  /// tail block (fp == last_pos_block_fp) = per-delta VInts, else a PFOR
  /// block (mirrors writer write_positions, postings.rs:501-521).
  fn refill_positions(pos: &mut PosCore, total_term_freq: u64) -> io::Result<()> {
      if pos.pos_in.file_pointer() as i64 == pos.last_pos_block_fp {
          let count = (total_term_freq % BLOCK_SIZE as u64) as usize;
          for slot in pos.pos_delta_buffer.iter_mut().take(count) {
              *slot = pos.pos_in.read_vint()? as u32;
          }
      } else {
          let mut deltas = [0u64; BLOCK_SIZE];
          pfor_util_decode(&mut pos.pos_in, &mut deltas)?;
          for (dst, src) in pos.pos_delta_buffer.iter_mut().zip(deltas) {
              *dst = src as u32;
          }
      }
      Ok(())
  }

  /// Docs+freqs+positions iterator (EverythingEnum), produced by
  /// [`PostingsReader::positions`].
  pub struct PositionsEnum {
      core: EnumCore,
  }

  impl PositionsEnum {
      pub fn doc_id(&self) -> i32 {
          self.core.doc as i32
      }

      pub fn next_doc(&mut self) -> io::Result<i32> {
          self.core.next_doc()
      }

      pub fn advance(&mut self, target: i32) -> io::Result<i32> {
          advance(&mut self.core, target)
      }

      /// PostingsEnum.freq(): current doc's term frequency.
      pub fn freq(&self) -> u32 {
          self.core.freq()
      }

      /// EverythingEnum.nextPosition (:1156-1187): current doc's next
      /// position (absolute, per-doc base reset).
      pub fn next_position(&mut self) -> io::Result<u32> {
          let freq = self.freq() as u64;
          let total_term_freq = self.core.total_term_freq;
          let pos = self.core.pos.as_mut().expect("PositionsEnum without pos state");
          assert!(
              pos.pos_pending_count > 0,
              "next_position called more than freq() times in the current doc (:1157)"
          );
          if pos.pos_pending_count > freq {
              skip_positions(pos, freq, total_term_freq)?;
              pos.pos_pending_count = freq;
          }
          if pos.pos_buffer_upto == BLOCK_SIZE {
              refill_positions(pos, total_term_freq)?;
              pos.pos_buffer_upto = 0;
          }
          pos.position += pos.pos_delta_buffer[pos.pos_buffer_upto];
          pos.pos_buffer_upto += 1;
          pos.pos_pending_count -= 1;
          Ok(pos.position)
      }
  }
  ```

  注：`next_position` 需要 `core.total_term_freq` —— 该字段已存在于 `EnumCore`（`new` 从 `entry.total_term_freq` 初始化），无需新增。

- [ ] **Step 7.4: 跑测试确认通过**

  ```
  $ cargo test -p codec-lucene9 positions_ 2>&1 | tail -5
  test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p codec-lucene9 2>&1 | grep -E "^test result"
  test result: ok. 142 passed; 0 failed; 1 ignored; ...（138 + 4 新）
  ```

- [ ] **Step 7.5: 提交**

  ```
  git add crates/codec-lucene9/src/postings_read.rs
  git commit -m "feat: PositionsEnum (.pos read path with skip-entry advance resync)"
  ```

---

## Task 8: PhraseDocIter + `Query::Phrase`（slop=0）

doc 合取命中后，每个 term occurrence 一个独立 PositionsEnum，验证 `pos[i] - pos[0] == offset[i]`（ExactPhraseMatcher :138-167）。无 positions 字段构造期 fail-fast（对齐 Java 执行期抛错）。

**Files:**
- Modify: `crates/core/src/search/segment_reader.rs`（`positions_enum` 转发）
- Modify: `crates/core/src/search/doc_iter.rs`（`PhraseDocIter` + `SegmentDocIter::Phrase`）
- Modify: `crates/core/src/search/query.rs`（`Query::Phrase` 变体 + 臂）
- Modify: `crates/core/src/search/mod.rs`（语义测试）
- Test: `crates/core/src/search/mod.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T7 的 `PostingsReader::positions` / `PositionsEnum::{next_doc, advance, freq, next_position}`；现有 `seek_term` / `field_info`。
- Produces:
  ```rust
  // segment_reader.rs
  pub(crate) fn positions_enum(&self, entry: &TermEntry) -> io::Result<PositionsEnum>;
  // doc_iter.rs
  pub struct PhraseDocIter { .. }
  impl PhraseDocIter {
      /// None = unknown field / non-indexed field / a term absent (no hits);
      /// Err = the field has no positions (fail-fast, Java-aligned).
      pub fn new(seg: &mut SegmentReader, field: &str, terms: &[Vec<u8>]) -> io::Result<Option<PhraseDocIter>>;
  }
  // SegmentDocIter 新增变体 Phrase(PhraseDocIter)
  // query.rs
  pub enum Query { .., Phrase { field: String, terms: Vec<Vec<u8>> } }
  impl Query { pub fn phrase(field: &str, terms: &[&str]) -> Query; }
  ```
  语义决定：occurrence 按 df 升序驱动合取（ConjunctionScorer 的 cost 序，offset 随 enum 走，语义不变）；单 term phrase 在 query.rs 层退化为 `Query::Term`（Lucene PhraseQuery 单 term 同 doc 集）；空 terms → 无命中；phrase 的 `freq()` = 1（ConstantScore）；`advance` 用 trait 默认线性版（搜索驱动只用 `next_doc`）。

### Steps

- [ ] **Step 8.1: 写失败测试** — 追加到 `crates/core/src/search/mod.rs` 的 `mod tests`：

  ```rust
      fn schema_pos() -> Schema {
          let mut s = Schema::new();
          s.add(FieldSpec::keyword("level"));
          s.add(FieldSpec::text_with_positions("message"));
          s
      }

      fn pos_doc(level: &str, tid: &str, message: &str) -> Document {
          let mut d = Document::new();
          d.add("level", FieldValue::Keyword(level.to_string()));
          d.add("tid", FieldValue::Keyword(tid.to_string()));
          d.add("message", FieldValue::Text(message.to_string()));
          d
      }

      fn write_phrase_corpus(root: &std::path::Path) {
          let mut w = IndexWriter::create(root, schema_pos(), IndexWriterConfig::default()).unwrap();
          let docs = [
              "quick brown fox",       // 0: "quick brown" hit
              "quick fox brown",       // 1: not adjacent
              "quick quick brown",     // 2: only the 2nd quick aligns
              "foo foo bar",           // 3: "foo foo" hit
              "foo bar foo",           // 4: "foo foo" miss
              "a b c",                 // 5: 3-term phrase hit
              "a b",                   // 6
              "c a b",                 // 7: "a b" hit
          ];
          for (i, m) in docs.iter().enumerate() {
              w.add_document(pos_doc("INFO", &format!("tid-{i}"), m)).unwrap();
          }
          w.commit().unwrap();
          drop(w);
      }

      #[test]
      fn phrase_query_positions() {
          let root = temp_dir("phrase");
          write_phrase_corpus(&root);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          // adjacent / not-adjacent
          let q = Query::phrase("message", &["quick", "brown"]);
          assert_eq!(s.count(&q).unwrap(), 2);
          let (_, docs) = s.top_docs(&q, 10).unwrap();
          assert_eq!(docs, vec![0, 2]);
          let q = Query::phrase("message", &["quick", "fox"]);
          let (_, docs) = s.top_docs(&q, 10).unwrap();
          assert_eq!(docs, vec![1]);
          // same doc, multiple candidate occurrences, only one aligns
          let q = Query::phrase("message", &["quick", "quick", "brown"]);
          let (_, docs) = s.top_docs(&q, 10).unwrap();
          assert_eq!(docs, vec![2]);
          // repeated term needs two adjacent occurrences
          let q = Query::phrase("message", &["foo", "foo"]);
          let (_, docs) = s.top_docs(&q, 10).unwrap();
          assert_eq!(docs, vec![3]);
          // 3-term phrase
          let q = Query::phrase("message", &["a", "b", "c"]);
          let (_, docs) = s.top_docs(&q, 10).unwrap();
          assert_eq!(docs, vec![5]);
          // cross-doc terms never merge
          let q = Query::phrase("message", &["a", "b"]);
          let (_, docs) = s.top_docs(&q, 10).unwrap();
          assert_eq!(docs, vec![5, 6, 7]);
          // reversed order misses
          assert_eq!(s.count(&Query::phrase("message", &["brown", "quick"])).unwrap(), 0);
          // single term degenerates to a Term query
          let q = Query::phrase("message", &["quick"]);
          assert_eq!(s.count(&q).unwrap(), 3);
          // missing term -> no hits (not an error)
          assert_eq!(s.count(&Query::phrase("message", &["quick", "nosuch"])).unwrap(), 0);
          // unknown field -> no hits (not an error)
          assert_eq!(s.count(&Query::phrase("message", &[])).unwrap(), 0);
          assert_eq!(s.count(&Query::phrase("nope", &["a", "b"])).unwrap(), 0);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn phrase_query_requires_positions() {
          // the M1 schema() has message as DOCS_AND_FREQS (no positions):
          // phrase must fail fast, mirroring Java's execution-time error
          let root = temp_dir("phrasefail");
          let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
          w.add_document(doc("INFO", "tid-0", "quick brown")).unwrap();
          w.commit().unwrap();
          drop(w);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          let err = s.count(&Query::phrase("message", &["quick", "brown"])).unwrap_err();
          assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
          // keyword field (DOCS) also fails
          let err = s.count(&Query::phrase("level", &["INFO", "WARN"])).unwrap_err();
          assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn phrase_query_multi_segment() {
          let root = temp_dir("phraseseg");
          let mut w = IndexWriter::create(&root, schema_pos(), IndexWriterConfig::default()).unwrap();
          w.add_document(pos_doc("INFO", "tid-0", "quick brown")).unwrap();
          w.commit().unwrap();
          w.add_document(pos_doc("INFO", "tid-1", "brown quick")).unwrap();
          w.add_document(pos_doc("INFO", "tid-2", "quick brown fox")).unwrap();
          w.commit().unwrap();
          drop(w);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          assert_eq!(s.segment_count(), 2);
          let q = Query::phrase("message", &["quick", "brown"]);
          let (total, docs) = s.top_docs(&q, 10).unwrap();
          assert_eq!(total, 2);
          assert_eq!(docs, vec![0, 2]);
          fs::remove_dir_all(&root).unwrap();
      }
  ```

- [ ] **Step 8.2: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core phrase_ 2>&1 | tail -5
  error[E0599]: no function or associated item named `phrase` found for enum `Query`
  ```

- [ ] **Step 8.3: 最小实现** —

  `crates/core/src/search/segment_reader.rs`：`use codec_lucene9::postings_read::{DocsEnum, DocsFreqsEnum, PostingsReader};` 改为 `use codec_lucene9::postings_read::{DocsEnum, DocsFreqsEnum, PositionsEnum, PostingsReader};`，`docs_freqs_enum` 之后追加：

  ```rust
      /// EverythingEnum over a positions field (PhraseDocIter construction).
      pub(crate) fn positions_enum(&self, entry: &TermEntry) -> io::Result<PositionsEnum> {
          self.postings.positions(entry)
      }
  ```

  `crates/core/src/search/doc_iter.rs`：`use codec_lucene9::postings_read::{DocsEnum, DocsFreqsEnum, NO_MORE_DOCS};` 改为 `use codec_lucene9::postings_read::{DocsEnum, DocsFreqsEnum, PositionsEnum, NO_MORE_DOCS};`；在 `// ── SegmentDocIter ──` 之前插入：

  ```rust
  // ── Phrase (slop=0) ─────────────────────────────────────────────────────

  /// One phrase term occurrence: an independent EverythingEnum + its offset
  /// in the phrase. Repeated terms get independent enums, which makes them
  /// naturally correct (spec M2 §6; PhrasePositions :24-58).
  struct Occurrence {
      en: PositionsEnum,
      offset: u32,
  }

  /// Exact-phrase iterator (slop=0): conjunction over the occurrences'
  /// postings enums, then per-doc position verification — there must be a
  /// lead position p0 with p0 - offset[0] + offset[i] present in every
  /// occurrence's positions (ExactPhraseMatcher :138-167).
  pub struct PhraseDocIter {
      occ: Vec<Occurrence>, // df-ascending (conjunction cost order)
      doc: i32,
      lead: usize,
  }

  impl PhraseDocIter {
      /// Builds the iterator: `Ok(None)` for unknown/non-indexed field or an
      /// absent term (no hits, TermQuery semantics); `Err` when the field has
      /// no positions — fail-fast, mirroring Java's execution-time error of
      /// PhraseQuery on such fields (Lucene912PostingsReader.postings :280-309
      /// downgrades to a docs-only enum whose nextPosition throws).
      pub fn new(
          seg: &mut SegmentReader,
          field: &str,
          terms: &[Vec<u8>],
      ) -> io::Result<Option<PhraseDocIter>> {
          let Some(fi) = seg.field_info(field) else {
              return Ok(None);
          };
          if fi.index_options == IndexOptions::None {
              return Ok(None);
          }
          if !matches!(
              fi.index_options,
              IndexOptions::DocsAndFreqsAndPositions | IndexOptions::DocsAndFreqsAndPositionsAndOffsets
          ) {
              return Err(io::Error::new(
                  io::ErrorKind::InvalidInput,
                  format!(
                      "field '{field}' does not have positions (phrase query requires \
                       IndexOptions >= DOCS_AND_FREQS_AND_POSITIONS)"
                  ),
              ));
          }
          let mut sought: Vec<(u32, u32, PositionsEnum)> = Vec::with_capacity(terms.len());
          for (i, t) in terms.iter().enumerate() {
              let Some((_, entry)) = seg.seek_term(field, t)? else {
                  return Ok(None); // absent term: no hits (PhraseWeight null scorer)
              };
              sought.push((entry.doc_freq, i as u32, seg.positions_enum(&entry)?));
          }
          // conjunction lead = cheapest enum first; offsets travel with their
          // enum, so phrase semantics are unaffected
          sought.sort_by_key(|(df, _, _)| *df);
          let occ = sought
              .into_iter()
              .map(|(_, offset, en)| Occurrence { en, offset })
              .collect();
          Ok(Some(PhraseDocIter { occ, doc: -1, lead: 0 }))
      }

      /// ExactPhraseMatcher (:138-167): collects each occurrence's positions
      /// in the current doc (freq × nextPosition, PhrasePositions.firstPosition
      /// :42-45) and checks for a common phrasePos = pos - offset.
      fn positions_match(&mut self) -> io::Result<bool> {
          let mut lists: Vec<Vec<u32>> = Vec::with_capacity(self.occ.len());
          for o in &mut self.occ {
              let f = o.en.freq() as usize;
              let mut v = Vec::with_capacity(f);
              for _ in 0..f {
                  v.push(o.en.next_position()?);
              }
              lists.push(v);
          }
          let base_off = self.occ[0].offset as i64;
          'outer: for &p0 in &lists[0] {
              let phrase_pos = p0 as i64 - base_off; // :145
              for (i, l) in lists.iter().enumerate().skip(1) {
                  let expected = phrase_pos + self.occ[i].offset as i64; // :148
                  if expected < 0 || l.binary_search(&(expected as u32)).is_err() {
                      continue 'outer;
                  }
              }
              return Ok(true);
          }
          Ok(false)
      }
  }

  impl DocIter for PhraseDocIter {
      fn doc_id(&self) -> i32 {
          self.doc
      }

      fn next_doc(&mut self) -> io::Result<i32> {
          if self.doc == NO_MORE_DOCS {
              return Ok(NO_MORE_DOCS);
          }
          if self.doc >= 0 {
              for o in &mut self.occ {
                  if o.en.doc_id() == self.doc && o.en.next_doc()? == NO_MORE_DOCS {
                      self.doc = NO_MORE_DOCS;
                      return Ok(NO_MORE_DOCS);
                  }
              }
          }
          loop {
              // conjunction over the position enums (ConjunctionScorer shape,
              // same dance as ConjunctionDocIter)
              let candidate = self.occ[self.lead].en.doc_id();
              if candidate == NO_MORE_DOCS {
                  self.doc = NO_MORE_DOCS;
                  return Ok(NO_MORE_DOCS);
              }
              let mut matched = true;
              for i in 0..self.occ.len() {
                  if i == self.lead {
                      continue;
                  }
                  let d = self.occ[i].en.advance(candidate)?;
                  if d == NO_MORE_DOCS {
                      self.doc = NO_MORE_DOCS;
                      return Ok(NO_MORE_DOCS);
                  }
                  if d > candidate {
                      self.lead = i;
                      matched = false;
                      break;
                  }
              }
              if !matched {
                  continue;
              }
              if self.positions_match()? {
                  self.doc = candidate;
                  return Ok(candidate);
              }
              // no positional match in this doc: move every occurrence past it
              for o in &mut self.occ {
                  if o.en.doc_id() == candidate && o.en.next_doc()? == NO_MORE_DOCS {
                      self.doc = NO_MORE_DOCS;
                      return Ok(NO_MORE_DOCS);
                  }
              }
          }
      }
      // advance: trait default (linear next_doc loop) — the search drive only
      // calls next_doc; freq: 1 (ConstantScore, trait default).
  }
  ```

  `SegmentDocIter` enum 增加变体 `Phrase(PhraseDocIter)`；`doc_id` / `next_doc` / `advance` 各加一臂 `Self::Phrase(p) => p.doc_id()` / `p.next_doc()` / `p.advance(t)`（`freq` 由既有 `_ => 1` 覆盖）。

  `crates/core/src/search/query.rs`：enum 增加 `Phrase { field: String, terms: Vec<Vec<u8>> }`；构造器：

  ```rust
      /// Exact phrase query (slop=0, spec M2 §6): consecutive offsets.
      pub fn phrase(field: &str, terms: &[&str]) -> Query {
          Query::Phrase {
              field: field.to_string(),
              terms: terms.iter().map(|t| t.as_bytes().to_vec()).collect(),
          }
      }
  ```

  `segment_iterator` 增加一臂：

  ```rust
              Query::Phrase { field, terms } => {
                  if terms.is_empty() {
                      return Ok(None);
                  }
                  if terms.len() == 1 {
                      return Query::Term {
                          field: field.clone(),
                          term: terms[0].clone(),
                      }
                      .segment_iterator(seg, needs_freq);
                  }
                  Ok(PhraseDocIter::new(seg, field, terms)?.map(SegmentDocIter::Phrase))
              }
  ```

  `use super::doc_iter::{ConjunctionDocIter, DisjunctionDocIter, MatchAllIter, SegmentDocIter};` 改为 `use super::doc_iter::{ConjunctionDocIter, DisjunctionDocIter, MatchAllIter, PhraseDocIter, SegmentDocIter};`。

- [ ] **Step 8.4: 跑测试确认通过**

  ```
  $ cargo test -p rustlucene-core phrase_ 2>&1 | tail -3
  test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test 2>&1 | grep -E "^test result"
      （全 workspace 回归绿）
  ```

- [ ] **Step 8.5: 提交**

  ```
  git add crates/core/src/search/segment_reader.rs crates/core/src/search/doc_iter.rs crates/core/src/search/query.rs crates/core/src/search/mod.rs
  git commit -m "feat: Phrase query (slop=0) via PositionsEnum conjunction"
  ```

---

## Task 9: Phrase diff 电池 + positions 标志透传（`make log-test` 四变体收尾）

phrase 电池只在 `--positions` 变体跑（无 positions 的索引上 phrase 双侧都会报错/无意义）。`verify-log.sh:41` 当前**没有**把 `$POSITIONS` 传给 `verify-search.sh`——本任务补上透传，两侧 dump 器按同一标志门控 phrase 段。电池短语取自 doc7 message 的真实相邻 token（保证 ≥1 命中，Java 侧读 stored、Rust 侧重放生成器，语料同源）。

**Files:**
- Modify: `crates/core/src/bin/rustlucene-cli.rs`（searchdump 增 `positions` 参数 + phrase 电池段）
- Modify: `interop/java/VerifySearchIndex.java`（args[1] 可选 `--positions` + 镜像 phrase 段）
- Modify: `interop/verify-search.sh`（第 5 参 positions 透传到两侧）
- Modify: `interop/verify-log.sh`（:41 把 `$POSITIONS` 传给 verify-search.sh）
- Test: 端到端即测试（`make log-test` 四变体全绿为本任务与 M2 收尾门槛）

**Interfaces:**
- Consumes: T8 的 `Query::phrase`；CLI 既有 `trace_id_of_doc` 的重放模式（`gen_log_document` + `XorShift`）。
- Produces:
  ```
  rustlucene-cli searchdump <indexDir> <numDocs> <seed> [--positions]
  interop/verify-search.sh <rustIndexDir> <javaIndexDir> <numDocs> <seed> [positionsFlag]
  # 两侧一致的新增输出行（仅 positions 变体；<t0..t2> 为 doc7 message 的前三个 token）：
  phrase message=<t0>,<t1> count=<n> first20=<csv>
  phrase message=<t0>,<t1>,<t2> count=<n>
  phrase message=<t1>,<t0> count=<n>
  phrase message=query23,query23 count=<n>
  phrase message=connection0,nosuchterm42 count=0
  phrase message=connection0 count=<n>
  ```

### Steps

- [ ] **Step 9.1: Rust 侧 searchdump 增 positions 参数 + phrase 电池** — `crates/core/src/bin/rustlucene-cli.rs`：

  `searchdump` 签名改为 `fn searchdump(index_dir: &Path, num_docs: u32, seed: u64, positions: bool) -> std::io::Result<()>`；在 wildcard 电池段之后、`print!("{out}")` 之前插入：

  ```rust
      // M2 phrase battery (search spec M2 §6), positions variant only: the
      // two/three-term phrases come from doc7's real adjacent tokens (a
      // guaranteed hit), the reversed pair exercises the not-adjacent case,
      // "query23 query23" the repeated-term case, the missing-term and
      // single-term items lock the degenerate behaviors. Mirrored in
      // VerifySearchIndex.java (gated on the same flag).
      if positions && num_docs > 7 {
          let toks = message_tokens_of_doc(seed, 7, 3);
          let (t0, t1, t2) = (toks[0].as_str(), toks[1].as_str(), toks[2].as_str());
          let q = Query::phrase("message", &[t0, t1]);
          let count = searcher.count(&q)?;
          let (_, docs) = searcher.top_docs(&q, 20)?;
          out.push_str(&format!(
              "phrase message={t0},{t1} count={count} first20={}\n",
              doc_csv(&docs)
          ));
          let q = Query::phrase("message", &[t0, t1, t2]);
          let count = searcher.count(&q)?;
          out.push_str(&format!("phrase message={t0},{t1},{t2} count={count}\n"));
          let q = Query::phrase("message", &[t1, t0]);
          let count = searcher.count(&q)?;
          out.push_str(&format!("phrase message={t1},{t0} count={count}\n"));
          let degenerate: [&[&str]; 3] = [
              &["query23", "query23"],
              &["connection0", "nosuchterm42"],
              &["connection0"],
          ];
          for terms in degenerate {
              let q = Query::phrase("message", terms);
              let count = searcher.count(&q)?;
              out.push_str(&format!("phrase message={} count={count}\n", terms.join(",")));
          }
      }
  ```

  `trace_id_of_doc` 之后追加同款重放助手：

  ```rust
  /// Replays the log corpus generator (same RNG stream as logwrite) to recover
  /// the first `k` whitespace tokens of doc `n`'s message without reading
  /// stored fields — the phrase battery's guaranteed-hit phrase source.
  fn message_tokens_of_doc(seed: u64, n: u64, k: usize) -> Vec<String> {
      let vocab = vocab();
      let mut rng = XorShift::new(seed);
      let mut toks = Vec::new();
      for doc_id in 0..=n {
          let doc = gen_log_document(&mut rng, &vocab, doc_id, false, false);
          if let Some((_, FieldValue::Text(m))) = doc.fields.iter().find(|(name, _)| name == "message") {
              toks = m.split_ascii_whitespace().take(k).map(str::to_string).collect();
          }
      }
      toks
  }
  ```

  `fn main()` 的 `"searchdump"` 分支改为：

  ```rust
          "searchdump" => {
              if args.len() < 5 {
                  usage();
              }
              let positions = args[5..].iter().any(|a| a == "--positions");
              searchdump(
                  Path::new(&args[2]),
                  args[3].parse().unwrap(),
                  args[4].parse().unwrap(),
                  positions,
              )
          }
  ```

  `usage()` 中 searchdump 行改为：

  ```rust
      eprintln!("  rustlucene-cli searchdump <indexDir> <numDocs> <seed> [--positions]");
  ```

- [ ] **Step 9.2: Java 侧镜像** — `interop/java/VerifySearchIndex.java`：main 开头 `Path indexDir = ...` 之后加 `boolean positions = args.length > 1 && args[1].equals("--positions");`；wildcard 电池段之后插入：

  ```java
            // M2 phrase battery (positions variant only): same items/format
            // as searchdump. The phrase terms are doc7's real adjacent tokens
            // (a guaranteed hit), read from stored fields.
            if (positions && r.maxDoc() > 7) {
                String[] toks = stored.document(7).get("message").split(" ");
                String t0 = toks[0], t1 = toks[1], t2 = toks[2];
                {
                    Query q = new ConstantScoreQuery(new PhraseQuery("message", t0, t1));
                    TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                    StringBuilder b = new StringBuilder();
                    for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                    out.append("phrase message=").append(t0).append(',').append(t1)
                       .append(" count=").append(s.count(q))
                       .append(" first20=").append(b).append('\n');
                }
                {
                    Query q = new ConstantScoreQuery(new PhraseQuery("message", t0, t1, t2));
                    out.append("phrase message=").append(t0).append(',').append(t1).append(',').append(t2)
                       .append(" count=").append(s.count(q)).append('\n');
                }
                {
                    Query q = new ConstantScoreQuery(new PhraseQuery("message", t1, t0));
                    out.append("phrase message=").append(t1).append(',').append(t0)
                       .append(" count=").append(s.count(q)).append('\n');
                }
                String[][] degenerate = {
                    {"query23", "query23"},
                    {"connection0", "nosuchterm42"},
                    {"connection0"},
                };
                for (String[] terms : degenerate) {
                    Query q = new ConstantScoreQuery(new PhraseQuery("message", terms));
                    out.append("phrase message=").append(String.join(",", terms))
                       .append(" count=").append(s.count(q)).append('\n');
                }
            }
  ```

- [ ] **Step 9.3: `interop/verify-search.sh` 透传** — 头部注释 Usage 行改为 `# Usage: interop/verify-search.sh <rustIndexDir> <javaIndexDir> <numDocs> <seed> [positionsFlag]`，`SEED="$4"` 之后加 `POSITIONS="${5:-}"`，两条 dump 命令改为：

  ```bash
  cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
    searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" $POSITIONS > /tmp/rl-search-rust.out
  java -cp "$CP" VerifySearchIndex "$JAVA_DIR" $POSITIONS > /tmp/rl-search-java.out
  ```

- [ ] **Step 9.4: `interop/verify-log.sh` 透传** — 第 41 行改为：

  ```bash
  "$ROOT/interop/verify-search.sh" "$RUST_DIR" "$JAVA_DIR" "$NUM_DOCS" "$SEED" "$POSITIONS"
  ```

- [ ] **Step 9.5: 编译 + 两个关键变体验证**

  ```
  $ cargo build --release 2>&1 | tail -1
  $ make java-classes 2>&1 | tail -1
  $ interop/verify-log.sh 200000 43 --positions
  ...
  phrase message=<t0>,<t1> count=... first20=...
  phrase message=<t0>,<t1>,<t2> count=...
  phrase message=<t1>,<t0> count=...
  phrase message=query23,query23 count=...
  phrase message=connection0,nosuchterm42 count=0
  phrase message=connection0 count=...
  SEARCH_INTEROP_OK
  LOG_INTEROP_OK
  $ interop/verify-log.sh 200000 42
      （非 positions 变体：两侧都不输出 phrase 行，diff 照常全绿）
  ```

- [ ] **Step 9.6: `make log-test` 四变体全绿（M2 收尾门槛）**

  ```
  $ make log-test
      （4 条变体全绿；phrase 段只在 seed 43 --positions 变体出现且 diff 一致）
  ```

- [ ] **Step 9.7: 提交**

  ```
  git add crates/core/src/bin/rustlucene-cli.rs interop/java/VerifySearchIndex.java interop/verify-search.sh interop/verify-log.sh
  git commit -m "feat: phrase diff battery + positions flag passthrough in verify-search"
  ```

---

## Task 10: searchbench 扩展（PREFIX/WILDCARD/TERMS/PHRASE 行类型 + bench 报告）

q.txt 查询文件加四种行类型，Java `SearchBench.java --dump-queries` 生成、`--load-queries` 原样回放，Rust searchbench 同文件同格式回放；`--no-cache` 口径出 bench 对比报告。数据与报告写到 `.superpowers/sdd/`（本任务起加入 .gitignore），不进 git。

**Files:**
- Modify: `interop/java/SearchBench.java`（dump 增四种行类型 + load 回放 + 新 query type）
- Modify: `crates/core/src/bin/rustlucene-cli.rs`（searchbench 解析新行类型 + WorkItem/labels/details）
- Modify: `.gitignore`（追加 `.superpowers/`）
- Test: 端到端即测试（双侧 per-query count 行 diff 一致 + bench 报告落盘）

**Interfaces:**
- Consumes: T2/T5/T6/T8 的 `Query::{terms, prefix, wildcard, phrase}`；searchbench 既有 `WorkItem` / `build_query` / `run_once` / `detail_of` 结构；SearchBench 既有 dump/load 框架。
- Produces（q.txt 新增行格式，两侧共享）:
  ```
  PREFIX\t<bucket>\t<prefix>
  WILDCARD\t<bucket>\t<pattern>
  TERMS\t<bucket>\t<term1,term2,...,termN>   # N=4（label terms）与 N=25（label termsbig，>16 规则双侧一致）
  PHRASE\t<bucket>\t<term1>\t<term2>        # 仅当字段有 positions 时生成
  ```
  bench 数据/报告：`.superpowers/sdd/m2-q.txt`、`.superpowers/sdd/m2-bench-{java,rust}.out`、`.superpowers/sdd/m2-counts-{java,rust}.txt`、`.superpowers/sdd/m2-searchbench-report.md`。

### Steps

- [ ] **Step 10.1: `.gitignore`** — 追加一行：

  ```
  .superpowers/
  ```

- [ ] **Step 10.2: Java `SearchBench.java` dump 侧** — 类文档的 query-file format 注释追加四行格式说明；`--dump-queries` 分支在 OR 行 dump 之后追加：

  ```java
                    // M2 multi-term query types, replayed verbatim by both
                    // sides (Rust searchbench reads the same file).
                    for (FreqBucket bucket : FreqBucket.values()) {
                        List<TermStats> sample = buckets.get(bucket);
                        if (sample.size() < 2) continue;
                        for (int i = 0; i < tasks; i++) {
                            String t = sample.get(rng.nextInt(sample.size())).term.utf8ToString();
                            // PREFIX: leading 4 chars of a sampled term
                            pw.printf(Locale.ROOT, "PREFIX\t%s\t%s%n",
                                    bucketLabel(bucket), t.substring(0, Math.min(4, t.length())));
                            // WILDCARD: alternate prefix* and prefix?+suffix shapes
                            int keep = Math.max(1, t.length() - 2);
                            String pattern = (i % 2 == 0)
                                    ? t.substring(0, keep) + "*"
                                    : t.substring(0, keep) + "?" + t.substring(t.length() - 1);
                            pw.printf(Locale.ROOT, "WILDCARD\t%s\t%s%n", bucketLabel(bucket), pattern);
                        }
                        // TERMS: one 4-term line (OR path) and one 25-term line
                        // (bitset path); label derived from the csv size (>16
                        // -> termsbig) on both sides
                        if (sample.size() >= 4) {
                            StringBuilder csv = new StringBuilder();
                            for (int k = 0; k < 4; k++) {
                                if (k > 0) csv.append(',');
                                csv.append(sample.get(rng.nextInt(sample.size())).term.utf8ToString());
                            }
                            pw.printf(Locale.ROOT, "TERMS\t%s\t%s%n", bucketLabel(bucket), csv);
                        }
                        if (sample.size() >= 25) {
                            StringBuilder csv = new StringBuilder();
                            for (int k = 0; k < 25; k++) {
                                if (k > 0) csv.append(',');
                                csv.append(sample.get(rng.nextInt(sample.size())).term.utf8ToString());
                            }
                            pw.printf(Locale.ROOT, "TERMS\t%s\t%s%n", bucketLabel(bucket), csv);
                        }
                    }
                    // PHRASE pairs — only when the field has positions (the
                    // Rust side fail-fasts phrase on non-positions fields)
                    FieldInfo benchFi = FieldInfos.getMergedFieldInfos(reader).fieldInfo(field);
                    if (benchFi != null
                            && benchFi.getIndexOptions().compareTo(IndexOptions.DOCS_AND_FREQS_AND_POSITIONS) >= 0) {
                        for (FreqBucket bucket : FreqBucket.values()) {
                            List<TermStats> sample = buckets.get(bucket);
                            if (sample.size() < 2) continue;
                            for (int i = 0; i < tasks; i++) {
                                String t1 = sample.get(rng.nextInt(sample.size())).term.utf8ToString();
                                String t2 = sample.get(rng.nextInt(sample.size())).term.utf8ToString();
                                pw.printf(Locale.ROOT, "PHRASE\t%s\t%s\t%s%n", bucketLabel(bucket), t1, t2);
                            }
                        }
                    }
  ```

- [ ] **Step 10.3: Java `SearchBench.java` load 侧** — 类文档 query types 注释追加 prefix/wildcard/terms/termsbig/phrase 说明；`--load-queries` 分支的解析循环增加四个列表（与 `loadedBool` 并列）：

  ```java
                List<String[]> loadedPrefix = new ArrayList<>();
                List<String[]> loadedWildcard = new ArrayList<>();
                List<String[]> loadedTermSets = new ArrayList<>();
                List<String[]> loadedPhrase = new ArrayList<>();
  ```

  解析循环的 else-if 链追加：

  ```java
                    } else if (parts[0].equals("PREFIX") && parts.length >= 3) {
                        loadedPrefix.add(new String[]{parts[1], parts[2]});
                    } else if (parts[0].equals("WILDCARD") && parts.length >= 3) {
                        loadedWildcard.add(new String[]{parts[1], parts[2]});
                    } else if (parts[0].equals("TERMS") && parts.length >= 3) {
                        loadedTermSets.add(new String[]{parts[1], parts[2]});
                    } else if (parts[0].equals("PHRASE") && parts.length >= 4) {
                        loadedPhrase.add(new String[]{parts[1], parts[2], parts[3]});
                    }
  ```

  ITERM 块之后、`runAndPrint(searcher, iterSearcher, ...)` 调用之前追加回放构造（与 Rust 侧 work 列表的追加顺序一致）：

  ```java
                // M2 line types, replayed verbatim like the AND/OR lines
                for (String[] p : loadedPrefix) {
                    queries.add(new ConstantScoreQuery(new PrefixQuery(new Term(field, p[1]))));
                    labels.add("prefix\t" + p[0]);
                    details.add("prefix=" + p[1] + " bucket=prefix\t" + p[0]);
                }
                for (String[] p : loadedWildcard) {
                    queries.add(new ConstantScoreQuery(new WildcardQuery(new Term(field, p[1]))));
                    labels.add("wildcard\t" + p[0]);
                    details.add("wildcard=" + p[1] + " bucket=wildcard\t" + p[0]);
                }
                for (String[] p : loadedTermSets) {
                    String[] ts = p[1].split(",");
                    BooleanQuery.Builder b = new BooleanQuery.Builder();
                    for (String t : ts)
                        b.add(new ConstantScoreQuery(new TermQuery(new Term(field, t))), BooleanClause.Occur.SHOULD);
                    b.setMinimumNumberShouldMatch(1);
                    queries.add(new ConstantScoreQuery(b.build()));
                    String type = ts.length > 16 ? "termsbig" : "terms";
                    labels.add(type + "\t" + p[0]);
                    details.add("terms=" + p[1] + " bucket=" + type + "\t" + p[0]);
                }
                for (String[] p : loadedPhrase) {
                    queries.add(new ConstantScoreQuery(new PhraseQuery(field, p[1], p[2])));
                    labels.add("phrase\t" + p[0]);
                    details.add("phrase t1=" + p[1] + " t2=" + p[2] + " bucket=phrase\t" + p[0]);
                }
  ```

- [ ] **Step 10.4: Rust searchbench 解析 + WorkItem** — `crates/core/src/bin/rustlucene-cli.rs` 的 `searchbench` 函数：

  (a) `bool_tasks` 解析之后追加：

  ```rust
      // M2 line types (same file, replayed verbatim like AND/OR)
      let prefix_tasks: Vec<(String, String)> = content
          .lines()
          .filter(|l| l.starts_with("PREFIX\t"))
          .filter_map(|l| {
              let parts: Vec<&str> = l.split('\t').collect();
              if parts.len() >= 3 { Some((parts[1].to_string(), parts[2].to_string())) } else { None }
          })
          .collect();
      let wildcard_tasks: Vec<(String, String)> = content
          .lines()
          .filter(|l| l.starts_with("WILDCARD\t"))
          .filter_map(|l| {
              let parts: Vec<&str> = l.split('\t').collect();
              if parts.len() >= 3 { Some((parts[1].to_string(), parts[2].to_string())) } else { None }
          })
          .collect();
      let terms_tasks: Vec<(String, Vec<String>)> = content
          .lines()
          .filter(|l| l.starts_with("TERMS\t"))
          .filter_map(|l| {
              let parts: Vec<&str> = l.split('\t').collect();
              if parts.len() >= 3 {
                  Some((parts[1].to_string(), parts[2].split(',').map(str::to_string).collect()))
              } else {
                  None
              }
          })
          .collect();
      let phrase_tasks: Vec<(String, String, String)> = content
          .lines()
          .filter(|l| l.starts_with("PHRASE\t"))
          .filter_map(|l| {
              let parts: Vec<&str> = l.split('\t').collect();
              if parts.len() >= 4 {
                  Some((parts[1].to_string(), parts[2].to_string(), parts[3].to_string()))
              } else {
                  None
              }
          })
          .collect();
  ```

  (b) `WorkItem` enum 增加变体：

  ```rust
      enum WorkItem {
          Term(String),
          And(String, String),
          Or(String, String),
          ITerm(String),
          Prefix(String),
          Wildcard(String),
          Terms(Vec<String>),
          Phrase(String, String),
      }
  ```

  (c) work 列表构建在 ITERM 块之后追加（Java 侧回放顺序是 M2 各行在 ITERM 之前，Rust 在其后——顺序不影响验收：per-query count 行是 sort 后 diff，分组聚合与顺序无关）：

  ```rust
      for (bucket, p) in &prefix_tasks {
          work.push((format!("prefix\t{bucket}"), WorkItem::Prefix(p.clone())));
      }
      for (bucket, p) in &wildcard_tasks {
          work.push((format!("wildcard\t{bucket}"), WorkItem::Wildcard(p.clone())));
      }
      for (bucket, ts) in &terms_tasks {
          let type_label = if ts.len() > 16 { "termsbig" } else { "terms" };
          work.push((format!("{type_label}\t{bucket}"), WorkItem::Terms(ts.clone())));
      }
      for (bucket, t1, t2) in &phrase_tasks {
          work.push((format!("phrase\t{bucket}"), WorkItem::Phrase(t1.clone(), t2.clone())));
      }
  ```

  (d) `build_query` 增加四臂：

  ```rust
          WorkItem::Prefix(p) => Query::prefix(field, p),
          WorkItem::Wildcard(p) => Query::wildcard(field, p),
          WorkItem::Terms(ts) => {
              let refs: Vec<&str> = ts.iter().map(String::as_str).collect();
              Query::terms(field, &refs)
          }
          WorkItem::Phrase(t1, t2) => Query::phrase(field, &[t1.as_str(), t2.as_str()]),
  ```

  (e) `detail_of` 增加四臂（格式与 Java details 逐字一致）：

  ```rust
          WorkItem::Prefix(p) => format!("prefix={p} bucket={label}"),
          WorkItem::Wildcard(p) => format!("wildcard={p} bucket={label}"),
          WorkItem::Terms(ts) => format!("terms={} bucket={label}", ts.join(",")),
          WorkItem::Phrase(t1, t2) => format!("phrase t1={t1} t2={t2} bucket={label}"),
  ```

- [ ] **Step 10.5: 编译 + 端到端 bench（--no-cache 口径）**

  ```
  $ cargo build --release 2>&1 | tail -1
  $ make java-classes 2>&1 | tail -1
  $ CP="interop/java/classes:interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar"
  $ mkdir -p .superpowers/sdd
  $ rm -rf /tmp/rl-bench2-rust /tmp/rl-bench2-java && mkdir -p /tmp/rl-bench2-rust /tmp/rl-bench2-java
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- logwrite /tmp/rl-bench2-rust 200000 42 --positions
  $ java -cp "$CP" JavaLogBench /tmp/rl-bench2-java 200000 1 42 --positions
  $ java -cp "$CP" SearchBench /tmp/rl-bench2-java message --tasks 50 --seed 42 --dump-queries .superpowers/sdd/m2-q.txt
  DUMPED terms to .superpowers/sdd/m2-q.txt
  $ grep -c "^PREFIX" .superpowers/sdd/m2-q.txt   # 各新行类型非空
  $ grep -c "^WILDCARD" .superpowers/sdd/m2-q.txt
  $ grep -c "^TERMS" .superpowers/sdd/m2-q.txt
  $ grep -c "^PHRASE" .superpowers/sdd/m2-q.txt
  $ java -cp "$CP" SearchBench /tmp/rl-bench2-java message --load-queries .superpowers/sdd/m2-q.txt --no-cache --warmup 10 --iter 30 > .superpowers/sdd/m2-bench-java.out 2> .superpowers/sdd/m2-counts-java.txt
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- searchbench /tmp/rl-bench2-rust message --load-queries .superpowers/sdd/m2-q.txt --warmup 10 --iter 30 > .superpowers/sdd/m2-bench-rust.out 2> .superpowers/sdd/m2-counts-rust.txt
  ```

- [ ] **Step 10.6: 正确性 diff（per-query counts 双侧一致）**

  ```
  $ diff <(sort .superpowers/sdd/m2-counts-java.txt) <(sort .superpowers/sdd/m2-counts-rust.txt)
      （无输出 = 全部查询（含 prefix/wildcard/terms/termsbig/phrase）命中数一致）
  $ grep -E "^(prefix|wildcard|terms|termsbig|phrase)" .superpowers/sdd/m2-bench-rust.out
      （新 query type 的分组行都在）
  ```
  若 diff 非空：先定位行（`grep` 该 detail 行对比两侧），按 T2/T5/T6/T8 的实现查语义分歧，修到一致为止——这是新查询类型在大语料上的端到端验收。

- [ ] **Step 10.7: bench 报告落盘** — 写 `.superpowers/sdd/m2-searchbench-report.md`（gitignored，不进 git）：对照 `m2-bench-java.out` 与 `m2-bench-rust.out` 的分组行，列出每个 query_type（term/and/or/iterm/prefix/wildcard/terms/termsbig/phrase）的 qps、p50/p90/p99 双侧数值与 Rust/Java 比值，并记一句结论（慢于 Java 的类型只记录不追责——全文形 Wildcard 全字典扫是 spec §8 已接受的差距）。报告必须标注：`--no-cache` 口径、200000 docs、seed 42、`--warmup 10 --iter 30`。

- [ ] **Step 10.8: 提交**（只提交代码与 .gitignore；`.superpowers/` 已被忽略）

  ```
  git add interop/java/SearchBench.java crates/core/src/bin/rustlucene-cli.rs .gitignore
  git commit -m "feat: searchbench PREFIX/WILDCARD/TERMS/PHRASE query types (M2 bench)"
  ```

---

## 完成定义（M2）

1. `cargo build` 与 `cargo test`（全 workspace）绿：codec-lucene9 基线 135 + T4 新增 3 + T7 新增 4 = 142；core 基线 22 + T1 新增 5 + T2 新增 3 + T5 新增 1 + T6 新增 4 + T8 新增 3 = 38。
2. `make log-test` 四变体全绿：terms/prefix/wildcard 电池项四变体全跑，phrase 电池项只在 seed 43 `--positions` 变体出现，双侧逐行 diff 一致（`SEARCH_INTEROP_OK` × 4）。
3. spec §8 覆盖矩阵落实：两条执行路径（≤16 OR / >16 bitset）在单测（T2/T5/T6 的 16/17 边界）与电池（terms 17 项、prefix conn、wildcard connection*）双侧覆盖；零命中、df=1 singleton、跨块枚举、advance 后 positions 正确性、phrase 相邻/不相邻/同 doc 多次出现只命中一次/重复 term 均有对应断言。
4. `.superpowers/sdd/m2-searchbench-report.md` 落盘（--no-cache 口径，per-query counts 双侧 diff 为空）。

## 后续阶段接口预留（不在本计划实现，仅记录锚点）

- `FixedBitSet` 的 popcount 当前为标量 `count_ones` 循环；AVX2 popcount 是 bench 驱动的优化项（总 spec §4b），接口形状不变。
- 全文形 Wildcard 的全字典扫慢于 Java 自动机求交（总 spec §8 留项）；`WildcardPattern` 的分类边界即未来自动机路径的接入点。
- `TermsIter` 不保留 Lucene 的 seek 状态复用（rewind / targetBeforeCurrentLength）；如需高频 seek_ceil 再引入（SegmentTermsEnum.pushFrame :263-296 的 fpOrig/nextEnt 复用条件）。
- `PositionsEnum` 未实现 payload/offset 分支（写侧不产出）；如未来支持，refillPositions/skipPositions 的对应分支按 EverythingEnum :1084-1153 补齐。
- sloppy phrase（slop>0）不做；`PhraseDocIter` 的 `positions_match` 即 SloppyPhraseMatcher 的替换点。
