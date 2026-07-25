# M7 设计：两阶段迭代协议 + Bool count fold + Top-N 提前终止

日期：2026-07-25。状态：已获用户批准（brainstorming 流程：五问调研——Bool 全 bitmap
化、Phrase 两阶段、Lucene 快速迭代机制对照、topn 现状、批量读取价值——三方案选型，
用户选方案一 A+B+C+D）。前置：M6（嵌套 Bool + Point + forceMerge）已完成并审查收口。

## 0. 需求与拍板记录

用户五问调研的代码级结论（对照 Lucene 9.12.3 源码核实）：

1. **Bool 不全转 bitmap**（维持）：Lucene 对任意 Bool 树同样是 doc-at-a-time 迭代，bitmap
   只出现在 query cache（已拒绝）与 multi-term >16 重写；歪斜 AND 物化大侧是纯预付浪费；
   Phrase/PointRange 物化 = 把最贵的确认步骤预付。**唯一例外是 count-only**：无提前终止，
   本来就要摸所有 doc，全量物化 + roaring fold 稳赢逐 doc 对齐。
2. **Phrase 应做两阶段**（确认差距）：Lucene `PhraseScorer.java:47-58` 是 TwoPhaseIterator
   ——approximation 只做 doc 合取不解码位置，`matches()` 才验证位置；嵌套时外层合取只对齐
   approximation（`ConjunctionScorer.java:43-44` `TwoPhaseIterator.unwrap`）。我们的
   `PhraseDocIter` 单阶段，嵌进 Bool 时对被兄弟子句拒绝的 doc 白做位置解码。
3. **Lucene 快速迭代机制对照**：跳表 advance 与子句 cost 排序已持平；BooleanScorer 窗口
   批量 / BlockMax / WAND / ImpactsDISI 全是打分剪枝机器，constant-score 模型**不抄**；
   LRUQueryCache 已拒绝（内联 term bitmap 是我们独有的持久化等价物）。真实差距：两阶段
   协议（#1）、通用 Bool count 逐 doc 驱动（#2，roaring fold 可反超）、多子句 OR 线性
   扫描 O(k) vs 堆 O(log k)（#3，bench 门槛）。
4. **topn 现状**：`top_docs` 走通用驱动全程迭代，`TopDocCollector` 逐命中计数、前 N 个
   入列——Sort.INDEXORDER 语义（无打分，docID 升序即确定顺序，是设计选择）。**无提前
   终止**：只要前 10 个也迭代全部命中，因为 total 靠逐 doc 数出。
5. **批量读取**：底层已批量（PFOR 128-doc block 解码、roaring fold 物化 `Vec<u32>`），
   per-doc 回调只是 ns 级常数项。top-N 的数量级收益在 **count/topN 分离 + 提前终止**
   （O(总命中) → O(N)+µs count），不在批量接口。DocBlock 批量接口定位为"必须全迭代
   形状"的二阶常数项优化，deferred（工作项 E，不进本里程碑）。

方案选型（三选一，用户选方案一）：**A+B+C+D**。否决项：方案二（Bool 迭代路径也按成本
模型 bitmap 化——歪斜 AND 回退风险，成本模型不可靠）；方案三（只做 A+D，放弃 count
fold 这个反超点）。

## 1. 范围总览

| 任务 | 交付 | 主要改动 |
|---|---|---|
| T-A 两阶段协议 + Phrase 拆分 | `DocIter::matches()` 协议 + Phrase approximation/confirmation 分离 + bitmap 候选快路径 | `search/doc_iter.rs`、`search/query.rs`、顶层驱动 |
| T-B Bool count bitmap fold | 通用形状 count 全量物化 + roaring fold，带成本护栏 | `search/query.rs`（bool_segment_count）、`roaring` 模块接口复用 |
| T-C 多子句 OR 堆化（bench 门槛） | `DisjOverDocIter` 线性扫描 → 二叉堆（≥20% 收益才落地） | `search/doc_iter.rs` |
| T-D Top-N 提前终止 | count/topN 分离：total 走 count 快路径，迭代收满 N 即停 | `search/searcher.rs`、`search/collector.rs` |

**明确不做**（YAGNI）：打分与任何打分剪枝（BlockMax/WAND/ImpactsDISI/BooleanScorer
窗口）；查询缓存；Bool 迭代路径 bitmap 化；DocBlock 批量 collect 接口（工作项 E，
deferred 到实测确认常数项收益后）；top-N 的非 docID 排序（无打分，维持 INDEXORDER）；
slop>0 phrase。

**前提假设不变**：只读本系统写出的索引（无 delete / norms / vector）；写侧 CREATE；
schema 各段一致。

## 2. T-A：两阶段迭代协议 + Phrase 拆分

### 2.1 协议（`DocIter::matches()`）

```rust
pub trait DocIter {
    // ……现有 doc_id / next_doc / advance / freq 不动
    /// 两阶段确认（Lucene TwoPhaseIterator.matches）：对 next_doc/advance
    /// 返回的**当前候选**做昂贵验证。默认 Ok(true) = 单阶段迭代器。
    /// 返回 false 后调用方以 next_doc() 推进（候选已被消费）。
    fn matches(&mut self) -> io::Result<bool> {
        Ok(true)
    }
}
```

**协议不变量**：任何驱动方（顶层 search 循环、ConjOver/DisjOver/Excluding、count 驱动）
拿到候选 doc 后必须调 `matches()`；false 则 `next_doc()` 推进重试。这正是 Lucene
`TwoPhaseIterator.asDocIdSetIterator` 的循环形状。所有现有迭代器走默认实现，行为不变。

### 2.2 PhraseDocIter 拆分

- **approximation**（`next_doc`/`advance`）：只做 postings 合取对齐（现有 ConjunctionDISI
  舞蹈），**不解码位置**——候选直接返回。
- **confirmation**（`matches()`）：对当前候选做 `positions_match()`（现有逻辑原样搬入）。
- **bitmap 候选快路径**：构造时若全部 term 都有内联 bitmap（`open_term_bitmap` 全 Some），
  approximation = roaring `intersect_docs` 物化 doc 序列（µs 级，比 PFOR 合取更快）；
  positions enum 照常打开（开流不解码，廉价），`matches()` 把各 enum `advance` 到候选
  doc 再读 freq 个位置验证。任一 term 无 bitmap → 回落 postings 合取 approximation。
- 顶层驱动调用 `matches()` 后，phrase 单用结果与现状逐 doc 一致（语义不变量）。

### 2.3 组合器改造

- **ConjOverDocIter**：approximation 对齐舞蹈不变；对齐到 doc d 后逐个调子句
  `matches()`，任一 false → 所有停在 d 的子句 `next_doc()` 推进后重新对齐（Lucene
  ConjunctionScorer 同款）。
- **DisjOverDocIter**：d = 各子句最小 doc；对所有停在 d 的子句调 `matches()`，至少一个
  true → 命中；全 false → 推进这些子句继续。
- **ExcludingDocIter**：排除检查通过后再调 `main.matches()`。
- `ConjunctionDocIter`/`DisjunctionDocIter`（平铺 And/Or 的 tier-3）：子项是 postings
  enum，恒单阶段，不动。

## 3. T-B：Bool count 路径 bitmap fold

`bool_segment_count` 在现有两条快路径（拍平 roaring count、纯 MUST_NOT 的
`maxDoc − prohibited`）之后、`drive_count` 之前插入 fold 路径：

### 3.1 递归物化 `materialize_query_bitmap(seg, query) -> io::Result<Option<MaterializedBitmap>>`

- **Term**：有内联 bitmap → frozen 视图转 croaring bitmap（容器级拷贝，SIMD 快，非逐
  doc）；无 bitmap → postings 物化（O(df)）。
- **And/Or/Bool**：递归——MUST 子句 and-fold、SHOULD 子句 or-fold、MUST_NOT 子句
  or-fold 后对正集 andnot；正集三态（MUST 合取 / 纯 SHOULD 并集 / 纯 MUST_NOT 的
  MatchAll）与 §2.2 迭代语义逐条对应。MatchAll 物化为全位 bitmap（仅在纯 MUST_NOT
  子树出现，且该情形已被既有 `maxDoc − prohibited` 快路径拦截，fold 路径实际遇不到，
  作防御处理）。
- **Terms/Prefix/Wildcard**：term 集收集后按 SHOULD 语义 or-fold（≤16 逐 term、>16 的
  bitset 路径同样物化后并入 fold）。
- **Phrase/PointRange**：驱动其 `segment_iterator` 全量收集 docs 物化（fold 路径只服务
  count，无提前终止，物化不亏——这是与迭代路径的本质区别）。
- `None` = 段内空（未知字段 / AND 缺子句 / OR 全缺），语义与迭代路径一致。

### 3.2 成本护栏

物化前估算：`estimate = Σ 各叶子的 Σdf`（Term/多 term 叶子用 TermEntry.doc_freq；
Phrase 叶子用其各 term df 之和——近似 doc 合取扫描成本；PointRange 无法预估，按
maxDoc 计）。`estimate > 4 × maxDoc` → 返回 None，调用方回落 `drive_count`（防病态
形状回退）。阈值常量 `FOLD_COST_FACTOR = 4`，bench 校准后可调。

### 3.3 语义不变量

count 结果与 `drive_count` 逐 doc 驱动**完全一致**（含 MUST_NOT、嵌套、跨字段、混合
occur、Phrase/PointRange 叶子）；这是电池与新增单元测试的断言对象。

## 4. T-C：多子句 OR 堆化（bench 门槛）

- 先做 micro bench：构造无 bitmap 语料（df 全部 < 4096），k ∈ {8, 32, 128} 子句的
  SHOULD OR，现行线性扫描 vs 二叉堆原型，各跑三轮取中位。
- **≥20% 收益才落地**：`DisjOverDocIter` 的"每 doc 线性取最小"改为二叉堆（ Rust
  `BinaryHeap` 反向序或手写索引堆，子句 `SegmentDocIter` 18.7KB 不挪动、堆内只放
  索引）。<20% 则记录数据、放弃本项（YAGNI）。
- 平铺 Or 的 `DisjunctionDocIter` 同病，若落地一并改（同一堆实现）。

## 5. T-D：Top-N 提前终止 + count/topN 分离

### 5.1 段级 count 快路径统一

抽 `try_segment_count(seg, query) -> io::Result<Option<u64>>`：归并现有全部 count 捷径
（Term→doc_freq 直读、PointRange→bitmap cardinality、multi-term→bitset popcount、
And/Or→roaring count、Bool→拍平 roaring / 纯 MUST_NOT / **T-B fold**），`None` = 无快
路径。`Searcher::count` 重构为逐段调它、None 回落迭代——消除 count 逻辑重复，T-D 与
count 共用同一入口。

### 5.2 top_docs 新驱动

```text
for (doc_base, seg) in leaves:            # 段按 docBase 升序
    match try_segment_count(seg, query)?:
        Some(c) => total += c             # count 快路径：µs 级
        None    => 需迭代计数（见下）
    if docs.len() < n:                    # 仍需补 doc
        打开 segment_iterator，逐 doc 驱动：
            每命中：若 count 无快路径则 total += 1；docs.len() < n 则 push(doc_base+doc)
            docs.len() == n → break        # 段内提前终止
```

- INDEXORDER 下 global docID = docBase + segDoc 且段有序：收满 N 后后续段只取 count
  不再迭代（全局短路）。
- count 有快路径的形状：O(N) 迭代 + µs 级 count；无快路径的形状回落现状（全程迭代，
  行为不变）。

### 5.3 语义不变量

`(total, docs)` 与现状实现**逐字节一致**——同查询同索引，total 相同、docs 为 docID
升序前 N。边界：N=0（只 count）、N>total、命中恰好跨段、某段无快路径另一段有。

## 6. 验证

- 互操作电池 `make log-test` 全绿（15× INTEROP_OK）。
- searchbench 三路（roaring / pfor / java --no-cache）：counts diff 为空；phrase 嵌套
  bool、通用 bool count、top-N 的 qps 提升，其余不回归（±15%）。T-C 附 micro bench
  数据（无论落地与否）。
- 新增单元测试：
  - 两阶段 phrase 与单阶段逐 doc 结果一致（单用 / 嵌套 MUST / 嵌套 SHOULD / 跨段）；
  - phrase bitmap 快路径与 postings 合取 approximation 结果一致（df≥4096 语料）；
  - bool count fold 与 drive_count 一致（MUST_NOT、嵌套、跨字段、Phrase/PointRange
    叶子、护栏触发回落）；
  - top-N 新旧实现 (total, docs) 完全一致（N=0、N>total、跨段边界、混合快路径）；
  - 组合器 matches() 协议：人造一个会失败的 confirmation 迭代器，验证 ConjOver/
    DisjOver/Excluding 驱动正确。

## 7. 执行顺序与分解

T-A → T-B → T-D → T-C（C 依赖最少、有 bench 门槛，放最后；D 复用 B 的 fold 与统一
count 入口）。全部落 `crates/core/src/search/`，无 codec 改动、无格式改动——索引
兼容性零影响。subagent-driven 执行，每任务完成后代码审查再进下一任务。
