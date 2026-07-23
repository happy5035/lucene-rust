# M2 设计：multi-term 查询（Terms/Prefix/Wildcard）+ Phrase

日期：2026-07-23。状态：已获用户批准（brainstorming 流程确认；范围 A+B、阈值双路、实施顺序
调整均经用户拍板）。上游设计：`2026-07-22-rust-search-design.md`（总 spec 的阶段 6/7 + 阶段 3 的
Terms 余项），本文档是该 spec 的 M2 切片细化。

## 1. 目标与范围

在 M1（Term/MatchAll/And/Or + 跳表 + SIMD 解码）之上新增四类查询，全部 ConstantScore、无评分：

| 查询 | 说明 |
|---|---|
| Terms（IN 语义） | term 集合 → doc 集合，Boolean SHOULD 语法糖 |
| Prefix | terms dict 前缀枚举 |
| Wildcard（`*` 与 `?`） | 按 pattern 形状分类执行（§5） |
| Phrase（slop=0） | position 合取，需新 .pos 读侧 |

配套基础设施：terms dict 顺序枚举器（TermsIter）、bitset 物化执行路径、positions 读侧
（PositionsEnum）、diff 电池与 searchbench 扩展。

**明确不做**：自动机与 FST 求交（总 spec §8 留项）；fuzzy；slop>0；payload/offset 读；
评分/norms/impact；NRT；query cache；bitmap postings（独立 M3，见
`2026-07-23-rust-search-m3-roaring-bitmap-design.md`）。

## 2. 前置事实（已核实的代码现状）

- `terms_read.rs`（550 行）只有 `seek_exact`，无任何枚举能力。
- `fst.rs` 读侧有 `lookup` / `trace_path`，无前缀枚举——枚举在 block-tree 层实现，FST 只用于
  定位起始块。
- `postings_read.rs`（813 行）有 `DocsEnum` / `DocsFreqsEnum`（含 level-0/level-1 skip-driven
  advance，`8f490c4`）。无 positions 枚举。
- 写侧 `postings.rs`：`.pos` 布局 = 满 128 块 PFOR + tail per-delta VInt（`write_positions`）；
  level-0/level-1 skip entry **已含** `(pos_fp delta, pos_buffer_upto)`（has_positions 时），
  现有 skip 解析已消费这些字节。
- 查询表达：`Query` enum（`query.rs`），无 JSON 解析层；diff 电池双侧硬编码
  （`VerifySearchIndex.java` ↔ `searchdump`），searchbench 用 TSV 查询文件
  （TERM/AND/OR 行，Java `--dump-queries` 生成）。
- `make log-test` 四变体：seed 42/44/45 无 positions，seed 43 `--positions`；phrase 电池
  只在 positions 变体可跑。`VerifyLogIndex.java` 已有 phrase 行（Java 读 Rust 索引，写侧
  interop），与读侧电池无关。

## 3. TermsIter：block-tree 顺序枚举器

照抄 Lucene `BlockTreeTermsReader.SegmentTermsEnum` 的 frame 栈语义，追加在 `terms_read.rs`：

- `seek_ceil(term) -> bool`：经 .tip FST 定位起始块（复用/小幅扩展现有 seek 路径），块内顺序
  扫到第一个 ≥ term 的项。
- `next() -> Option<(term_bytes, TermEntry)>`：当前 frame 扫完后按 block header 的
  hasTerms/hasSubBlocks/nextEnt 元数据 push/pop frame（叶子块 vs 内部块，对齐 Lucene）。
- 只碰 terms dict（.tim/.tip/.tmd），不碰 postings（总 spec 既定分层：枚举与 postings 枚举两层）。

Prefix = `seek_ceil(prefix)` 后 `next()` 直到 `!starts_with(prefix)`；Wildcard 全文形 =
从头枚举 + 模式过滤。

## 4. multi-term 执行：阈值双路（用户拍板）

四类查询归一为"term 集合 → doc 集合"：

- **term 数 ≤ 16**（对齐 Lucene 9 `BlendedTermQuery` 的 16 阈值）：rewrite 成现有
  `Query::Or`，走堆合并，零新代码。
- **term 数 > 16**：**bitset 物化**——per-segment `FixedBitSet`（max_doc bits），逐 term
  `DocsEnum` 全量 `next_doc` 置位，产出 `BitsetDocIter`（next_set_bit 扫描）；count 路径直接
  popcount。物化成本 = 总命中数次 next_doc，与 Lucene DocIdSet rewrite 同构。
  AVX2 popcount 留作 bench 驱动优化项（总 spec §4b）。
- 不设枚举上限（Lucene 9 不再抛 TooManyClauses）。
- Terms(IN) 直接给集合；Prefix/Wildcard 由 TermsIter 收集。集合收集后按 df 排序交给双路。

## 5. Wildcard 分类与模式匹配（照总 spec §3）

- 分类：pattern 截到第一个 `*`/`?` 得固定前缀。纯前缀形（`foo*`）→ 前缀枚举零过滤；
  有前缀含通配（`fo?o*`）→ 前缀枚举 + 尾过滤；无前缀（`*foo`、`*fo*`）→ 全字典扫 + 过滤。
- 匹配：经典双指针 glob（`*` 回溯），按 `chars` 迭代——`?` 对齐 Lucene 的单 code point
  语义（语料全 ASCII 时与 byte 匹配等价）。
- 已接受的差距：全文形 Wildcard 全字典扫慢于 Java 自动机求交（总 spec §8 留项），bench 只
  记录不追责。

## 6. Phrase（slop=0）

**PositionsEnum**（`postings_read.rs` 追加）：

- 读 .pos：满 128 块 PFOR、tail per-delta VInt（镜像写侧 `write_positions`）。与
  DocsFreqsEnum 并行消费：每 doc 读 freq 个 delta，重建绝对 position（doc 边界 lastPosition
  重置，对齐写侧）。
- **advance 重同步**：doc 流 skip 后，用 skip entry 的 `(pos_fp, pos_buffer_upto)` 重定位 .pos
  流并跳过块内已消费项——照抄 Lucene912PostingsReader 的 posPendingCount/payFP 机制。本阶段
  最精细的点；现有 skip 解析已消费这些字节，改造面可控。

**PhraseDocIter**（`doc_iter.rs` 追加）：

- 复用现有 ConjunctionDocIter 做 doc 合取；命中后每 term occurrence 各持一个 PositionsEnum，
  验证 `pos[i] - pos[0] == offset[i]`（slop=0）。
- 重复 term（如 "foo foo"）用独立 occurrence enum，天然正确。
- 对无 positions 字段发 phrase → 构造期 fail-fast（对齐 Java）。

## 7. 模块分解

codec-lucene9：
- `terms_read.rs` 追加：`TermsIter`（~350 行含测试，本阶段最大单组件）
- `postings_read.rs` 追加：`PositionsEnum`（~300 行）

rustlucene-core `search/`：
- `query.rs`：Query 增 `Terms/Prefix/Wildcard/Phrase` 变体 + wildcard 分类
- `doc_iter.rs`：`BitsetDocIter`、`PhraseDocIter`
- `bitset.rs`（新）：FixedBitSet + popcount
- `multi_term.rs`（新）：枚举收集 + 阈值双路执行

interop：
- `VerifySearchIndex.java` + `searchdump`：双侧电池加 terms/prefix/wildcard/phrase 项
- `verify-search.sh`：透传 positions 标志，phrase 项只在 positions 变体跑
- `SearchBench.java --dump-queries` + Rust searchbench：q.txt 加 PREFIX/WILDCARD/TERMS/PHRASE
  行类型

## 8. 测试与验收

- **diff 电池**刻意覆盖两条执行路径：展开 ≤16 的 prefix（OR 路径）与 >16 的 wildcard
  （bitset 路径）；外加零命中、df=1 singleton、跨 128 块、phrase 相邻/不相邻/同 doc 多次出现
  只命中一次。
- 三层纪律不变：round-trip 单测（TermsIter 枚举结果 = 写侧输入序；PositionsEnum vs 写侧
  positions 数组）→ Rust 语义电池 → Java diff 终验（`make log-test` 四变体全绿为收尾门槛）。
- bench：`--no-cache` 口径记录 Terms/Prefix/Wildcard/Phrase vs Java，出报告。

## 9. 实施顺序（用户确认的调序）

1. **bitset + 阈值双路 + Terms(IN)**——不依赖枚举器，最早把新执行路径置于 diff 验证之下
2. TermsIter 枚举器
3. Prefix
4. Wildcard
5. PositionsEnum
6. Phrase

每步交付 = 代码 + 电池增量全绿；收尾 log-test 四变体。

## 10. 工作量粗估

| 部分 | 估计 |
|---|---|
| TermsIter 枚举器 | ~350 行 |
| bitset + 双路 + Terms 接入 | ~250 行 |
| Prefix/Wildcard 接入 | ~200 行 |
| PositionsEnum | ~300 行 |
| PhraseDocIter | ~200 行 |
| 电池/harness/searchbench | ~300 行 |
| **合计** | **~1.6k 行（含测试）** |
