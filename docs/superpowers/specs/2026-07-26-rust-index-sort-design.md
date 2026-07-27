# Index Sort 调研与设计：段内文档按 SortKey 物理排序

日期：2026-07-26。状态：**调研草案（未批准）**——本文先记录现状调研与 Lucene 对照，再给出
实现方案选型，供后续立项拍板。尚未走 brainstorming 批准流程，不构成里程碑。

## 0. 背景与目标

Lucene 的 `IndexWriterConfig.setIndexSort(Sort)` 让每个段内文档按指定 SortKey **物理有序**，
flush 与 merge 全程维持。其核心搜索收益是 **early termination**：当查询的结果排序是 index
sort 的前缀时，每段收满 top-N 即可停扫（`TopFieldCollector.canEarlyTerminate`）。本项目当前
完全不具备该能力，且读侧主动拒绝带 index sort 的段。

本文目标：① 摸清 Rust 侧现状；② 对照 Lucene 9.12.3 核心逻辑；③ 给出落地方案与代价评估。

## 1. Rust 侧现状（调研结论）

**结论：完全没有 Lucene 意义上的 index sort（段内文档按 SortKey 物理重排）。**

| 维度 | 现状 | 出处 |
|---|---|---|
| 写入顺序 | 纯 append-only，docID = 写入序号 | `core/src/doc_writer.rs:355`（`let doc_id = self.max_doc`）、`:457`（`self.max_doc += 1`） |
| 配置项 | `IndexWriterConfig` 无 `index_sort` 字段 | `core/src/index_writer.rs:11-26`（仅 `max_buffered_docs`/`max_ram_bytes`/`bitmap`/`bitmap_threshold`） |
| `.si` 写入 | 恒写 `numSortFields = 0` | `codec-lucene9/src/segment_info.rs:113-114`（`// no index sort` + `write_vint(0)`） |
| `.si` 读取 | 遇 `num_sort_fields != 0` 直接报错 | `codec-lucene9/src/segment_info.rs:154-158`（`corrupt("unsupported: index sort …")`）——Java 用 `setIndexSort` 写的段本系统读不了 |
| 段合并 | 已实现 `forceMerge(1)`，但**保序拼接不重排** | `core/src/merge.rs:462`（`force_merge`）；docID 用 `doc_base` 偏移，`debug_assert` 验证升序（`:264-267`）；注释 `:420` "无 delete，docMap 恒等偏移" |

**需严格区分的两类"排序"**（避免与 index sort 混淆）：

- **查询结果排序**：主树仅 INDEXORDER top-N（`core/src/search/collector.rs:47-79`
  `TopDocCollector`、`search/searcher.rs:123` `top_docs`），按 docID 升序输出命中，不是按列排序，
  更不是索引物理排序。按 DV 列的结果排序 collector（`NumericSortCollector`/`SortedSortCollector`/
  `SortField`）**只存在于未合入的 worktree** `.claude/worktrees/feat+search-reader/`，主树无
  `search/sort.rs`。
- **SortedDocValues**：是列存数据结构（`codec-lucene9/src/doc_values.rs` `TYPE_SORTED`），与
  index sort 无直接关系。

**实现 index sort 所需的列存基础已具备**（这是好消息）：

- RAM 缓冲：`core/src/doc_writer.rs` `NumericDvBuf`（`:207-223`，`docs: Vec<u32>` + `values: Vec<i64>`，
  doc 升序）、`SortedDvBuf`（`:229-254`，插入序字典 + 每 doc `term_ids`）
- 落盘写：`codec-lucene9/src/doc_values.rs` `add_numeric_field`（`:111`）、`add_sorted_field`（`:129`）
- 读路径：`codec-lucene9/src/doc_values_read.rs` `numeric_values`（`:347`）、`sorted_ords`（`:367`）、
  `sorted_dict`（`:387`）

缺的是**按 sort 列求 docID 排列（DocMap）并按排列重写全格式**的核心逻辑——当前所有缓冲与落盘
都假设 doc 升序。

`docs/` 中无 index sort 规划，反而多处明确"刻意不支持"：`docs/superpowers/plans/2026-07-22-rust-search-m1-foundation-term-query.md:1626`
（"The index sort must be absent (we never write one)"）、`docs/bool-bench-report.md:241`
（承认"索引未 index-sort 时 TopFieldCollector 必须访问全部命中，Java 的 score 早停失效"）。

## 2. Lucene index sort 核心逻辑（对照参考）

路径基于 `reference/lucene-9.12.3/lucene/core/src/java/org/apache/lucene/`。

**核心设计：不在 RAM 物理移动文档，而是算出 docID 排列（DocMap），落盘时按排列重写所有格式。**

### 2.1 配置与约束

- `index/IndexWriterConfig.setIndexSort(Sort)`（:475）：要求每个 `SortField.getIndexSorter() != null`，
  否则 `IllegalArgumentException`；存 `indexSort` + `indexSortFields`。
- `search/SortField.getIndexSorter()`（:595）：仅 `STRING/INT/LONG/DOUBLE/FLOAT` 映射到具体
  `IndexSorter`；`SCORE/DOC/CUSTOM/STRING_VAL` 返回 null（**不可用于 index sort**）。
- `index/Sorter` 构造器：`sort.needsScores()` 为真则抛异常（**不能按相关性分数排索引**）。
- 持久化：`codecs/lucene99/Lucene99SegmentInfoFormat`（:52-55, :223）把 Sort 序列化进 `.si`
  （`numSortFields` + 每个 `sorter.getProviderName()` + `SortFieldProvider.write`）。

### 2.2 flush 路径

`DocumentsWriterPerThread.flush()` → `IndexingChain.flush(state)`（:261）：

1. `maybeSortSegment(state)`（:219）：`getDocValuesLeafReader()`（:150）把**内存中的 docvalues
   writer 包成 LeafReader**，让 `IndexSorter` 直接读 RAM；对每个 SortField 调
   `getIndexSorter().getDocComparator(reader, maxDoc)`；`new Sorter(indexSort).sort(maxDoc, comparators)`
   返回 `Sorter.DocMap`（**已有序则返回 null**）。block（父子文档）用 `parents.nextSetBit(docID)`
   把子 doc 映射到 parent 再比较，保证 block 不拆散（:229-242）。
2. `index/Sorter.sort(maxDoc, comparator)`（:131）：先线性扫描查是否已有序（是→null）；填
   `docs[i]=i`，用 `DocValueSorter`（继承 `util/TimSorter`）排序得 **newToOld**；`PackedLongValues.monotonic`
   压缩存储，再反推 **oldToNew**，组成双向 `DocMap`（:49）。
3. 把 `sortMap` 传给**全部**写入路径按下表重写：

| 组件 | 类/方法 | 重排方式 |
|---|---|---|
| Postings | `FreqProxTermsWriter.flush`（:83）→ `SortingTerms`/`SortingDocsEnum` | 逐 term `docs[i]=docMap.oldToNew(doc)`，再 `LSBRadixSorter.sort` 重排（位置用 `DocOffsetSorter` 并行排） |
| DocValues | `SortedDocValuesWriter.sortDocValues`（:134） | `ords[sortMap.oldToNew(docID)] = ord`，包成 `SortingSortedDocValues`；Numeric 同理 |
| Stored Fields | `SortingStoredFieldsConsumer.flush`（:96） | 先写**临时无压缩文件**（原序），再按新序读回：`reader.document(sortMap.newToOld(docID))`（:108-113） |
| Points | `PointValuesWriter.flush`（:92） | `MutableSortingPointValues(points, sortMap)` 重映射 docID |
| Norms | `NormValuesWriter.flush`（:70） | 复用 `NumericDocValuesWriter.sortDocValues` |
| Term Vectors | `SortingTermVectorsConsumer` | 同 stored fields 的临时文件 + 重排读回 |
| KNN Vectors | `VectorValuesConsumer.flush(state, sortMap)` | `SortingFloatVectorValues` 用 `oldToNew` |

### 2.3 merge 路径（多路归并）

- `index/MergeState` 构造器（:166）→ `buildDocMaps`（:204）：`indexSort != null` →
  `MultiSorter.sort(indexSort, readers)`（:213）；返回 null（已全局有序）退回 deletion-only DocMap。
- `index/MultiSorter.sort`（:39）：每段 `IndexSorter.getComparableProviders(readers)` 返回每 doc 一个
  可比 `long`（`StringSorter` 用 `OrdinalMap.build` 映射到全局 ord 空间，:401）；`PriorityQueue<LeafAndDocID>`
  **最小堆 K-way 归并**（:75），tie-break 先 readerIndex 后 docID；弹出顺序 readerIndex 单调不减则
  返回 null（:134）。产出每段一个单向 `MergeState.DocMap`（`get(doc)` → 新 docID 或 `-1` 删除）。
- postings 归并：`FieldsConsumer.merge`（:72）→ `MappingMultiPostingsEnum` → `DocIDMerger`；
  `needsIndexSort` 时用 `SortedDocIDMerger`（`PriorityQueue` 按 mappedDocID K-way，:135），否则顺序 merge。
- `IndexWriter.mergeMiddle`（~:5220）：**有 index sort 时禁用** `SortingCodecReader` 的 bulk-merge
  优化（:5233 条件 `hasIndexSort == false`）。

### 2.4 搜索收益（early termination）

`search/TopFieldCollector`：构造器（:52）读 `context.reader().getMetaData().getSort()`，算
`canEarlyTerminate(sort, indexSort)`（:149）——搜索 sort 第一字段是 `FIELD_DOC`，**或**搜索 sort
字段数组是 index sort 字段数组的**前缀**（:158）。`thresholdCheck`（:90）：hit 不再 competitive 且
可早停且达阈值 → 抛 `CollectionTerminatedException`（:98），**整段提前结束**。

### 2.5 整体数据流

```
setIndexSort(Sort)
  ├─ 校验 SortField.getIndexSorter()!=null；存 indexSort/indexSortFields；序列化进 .si
  ├─[FLUSH] IndexingChain.flush
  │    ├─ maybeSortSegment: RAM docvalues 包成 LeafReader → getDocComparator
  │    │    → index/Sorter.sort: TimSort 求 newToOld → PackedLongValues 压缩双向 DocMap（有序→null）
  │    └─ sortMap 传给 norms/docvalues/points/vectors/storedfields(临时文件+newToOld)/postings(SortingTerms)
  ├─[MERGE] MergeState.buildDocMaps → MultiSorter: ComparableProvider + PriorityQueue K-way → 单向 DocMap
  │    → SegmentMerger.merge: 各 codec writer.merge(mergeState) 消费 docMaps；postings 走 SortedDocIDMerger
  └─[SEARCH] TopFieldCollector.canEarlyTerminate(搜索sort ⊑ indexSort) → 段内收满 top-N 即停
```

## 3. 差距分析

| 能力 | Lucene | Rust 现状 | 缺口 |
|---|---|---|---|
| 配置入口 | `setIndexSort` | 无 | 新增 `IndexWriterConfig.index_sort` |
| flush 求排列 | `index/Sorter`（TimSort + DocMap） | 无（doc 升序直写） | **核心新增**：求 DocMap |
| flush 按排列重写 | 全格式 `Sorting*` | 缓冲假设 doc 升序 | 改造 postings/DV/stored/points 缓冲落盘 |
| `.si` sort 字段 | 序列化 Sort | 恒写 0 / 读侧报错 | 写 sort 字段 + 解除读侧拒绝 |
| merge 重排 | `MultiSorter` K-way | 保序拼接 | 引入 K-way 归并 DocMap |
| 搜索早停 | `canEarlyTerminate` | INDEXORDER 全扫 | top-N collector 利用段有序性 |

## 4. 实现方案设计（草案）

### 4.1 范围与分阶段

| 阶段 | 交付 | 主要新代码 |
|---|---|---|
| T-A flush 排序 | `IndexWriterConfig.index_sort` + flush 时求 DocMap 并按排列重写全格式 | `core/src/sort.rs`（新建：DocMap + 排列求解）、`segment_builder.rs` 改造、`doc_writer.rs` 缓冲重排 |
| T-B `.si` 持久化 | 写 `numSortFields` + SortField；读侧解除 "unsupported" | `codec-lucene9/src/segment_info.rs` |
| T-C merge 重排 | `force_merge` 按 index sort K-way 归并 | `core/src/merge.rs` 引入 DocMap |
| T-D 搜索早停 | top-N collector 段有序时提前终止 | `core/src/search/collector.rs`、`searcher.rs` |

建议顺序 **A → B → C → D**：A 是核心且独立可验（单段即可 CheckIndex + 查询 diff）；B 让 Java
可读可验；C 复用 A 的 DocMap；D 是性能动机兑现。

### 4.2 关键设计点（对照 Lucene 化简）

- **DocMap 表示**：flush 用双向 `oldToNew`/`newToOld`（`Vec<u32>` 即可，无需 Lucene 的
  `PackedLongValues` 压缩——本项目段规模下内存可接受，YAGNI）。merge 用单向 `get(doc)->newDoc`。
- **排序列读取**：复用已有 `NumericDvBuf`/`SortedDvBuf`（`doc_writer.rs:207/229`）作为比较器输入，
  对应 Lucene `getDocValuesLeafReader` 把 RAM 暴露给 sorter 的思路。
- **postings 重排**：本系统 postings 缓冲（`PostingBuf.docs` 升序）按 `oldToNew` 映射后需重新升序
  排列（Lucene 用 LSBRadixSorter；Rust 可用排序 + 稳定 tie-break by oldDoc）。positions 需随 doc
  并行移动。
- **stored fields 重排**：本系统 stored 走 LZ4 块压缩落盘。化简方案：flush 时先在 RAM 持有每 doc
  的 stored 字节（或临时缓冲），按 `newToOld` 顺序重写——避免 Lucene 的临时文件二次读回。
- **MISSING 语义**：sort 列缺失值的排首/排尾规则照抄 Lucene `SortField.missingValue`（设计 spec
  `2026-07-22-rust-search-design.md:22` 已为结果排序定下"MISSING 规则照抄 FieldComparator"，可复用）。

### 4.3 明确不做（YAGNI，初稿）

- 按 SCORE 排序（Lucene 本身禁止）；`CUSTOM`/`DOC`/`STRING_VAL` 排序类型
- 多字段复合 index sort 的任意组合（先做单字段，再评估前缀扩展）
- block（父子文档）排序保护（本系统无父子文档）
- `updateDocValues` 对 sort 字段的禁写校验（本系统无 update/delete）
- merge 目标段数可配（沿用现有 `forceMerge(1)`）
- term vectors / KNN vectors 重排（本系统未实现这两类）

### 4.4 方案选型（待拍板）

| 分叉 | 选项 | 倾向 |
|---|---|---|
| DocMap 压缩 | `Vec<u32>` 双向 / `PackedLongValues` 等价压缩 | `Vec<u32>`（简单，段规模可控） |
| stored 重排 | RAM 持有按 newToOld 重写 / 临时文件二次读回（Lucene 式） | RAM 重写（避免临时文件 I/O） |
| postings 重排 | 映射后整体排序 / 桶式按新 doc 分发 | 整体排序（实现简单，tie-break 稳定） |
| 排序算法 | TimSort（近有序友好）/ 标准 sort_unstable | TimSort 等价（flush 数据常近有序） |

## 5. 约束与风险

- **格式兼容**：`.si` 写 sort 字段后必须仍被 Java `Lucene99SegmentInfoFormat` 正确读取，且
  `SortFieldProvider` SPI 名要与 Java 一致（`IntSorter`/`LongSorter`/`StringSorter` 的 provider name），
  否则 Java 读不了。需逐项对照 `SortFieldProvider` 序列化格式。
- **全格式一致性**：postings/DV/stored/points 任一重排错位都会导致 CheckIndex 失败或查询结果 diff，
  验证成本高（见 §6）。
- **内存代价**：排序期间需持有 sort 列全量值数组（Lucene `IndexSorter.getDocComparator` 同样如此）；
  大段下需评估 RAM 峰值（本系统 flush 默认 512MB 缓冲）。
- **merge 性能**：有 index sort 时 Lucene 禁用 bulk-merge 优化、走 K-way 堆归并，比顺序 merge 贵；
  本系统 `forceMerge(1)` 需评估退步幅度。

## 6. 验证方案

沿用项目既有互操作范式（`interop/`）：

1. **单元**：DocMap 求解（已有序→null、逆序、含 MISSING、tie-break 稳定）。
2. **格式兼容**：Rust 写 index-sort 段 → Java `CheckIndex` 零错误；Java 用 `setIndexSort` 写段 →
   Rust 读侧不再报错且可查。
3. **结果 diff**：同语料双侧建 index-sort 索引，复用 `interop/compare-index.sh` 的 term 级 diff
   与 `compare-search.sh` 的查询逐条 diff，要求命中集完全一致。
4. **早停收益**：`searchbench --topn` 在 index-sort 索引上对比开/关早停的 QPS（对照
   `docs/bool-bench-report.md:241` 提到的"未 index-sort 时 Java 早停失效"）。

## 7. 参考出处

- Lucene 9.12.3 源码：`reference/lucene-9.12.3/lucene/core/src/java/org/apache/lucene/`
  （`index/IndexWriterConfig.java`、`index/IndexSorter.java`、`index/Sorter.java`、`index/MultiSorter.java`、
  `index/MergeState.java`、`index/IndexingChain.java`、`index/SortingCodecReader.java`、
  `search/SortField.java`、`search/TopFieldCollector.java`、`codecs/lucene99/Lucene99SegmentInfoFormat.java`）
- 本项目现状出处见 §1 各表 `文件:行号`。
