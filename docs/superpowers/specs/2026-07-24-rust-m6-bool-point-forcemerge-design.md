# M6 设计：嵌套 Boolean 查询 + Point 区间查询 + forceMerge(1)

日期：2026-07-24。状态：已获用户批准（brainstorming 流程：四个需求分叉、方案选型、三节设计
逐节确认；§3 stored 归并方式经一次事实修正后确认）。前置：M5（croaring 引擎替换）已完成。

## 0. 需求与拍板记录

用户提出三件欠账：① 现有 `Query::And`/`Query::Or` 只是同字段平铺 term 列表，不是真正的
BooleanQuery；② Point 查询（M1 spec 列出但未实现）；③ forceMerge——把多个段合并成一个。

四问拍板：

1. **Bool 子句类型**：MUST / SHOULD / MUST_NOT 三种；**FILTER 不做**（ConstantScore 下与
   MUST 等价）。
2. **minimum_should_match**：只实现 Lucene 默认语义（无 MUST 时 msm=1；有 MUST 时 SHOULD
   纯可选），不支持可配置 msm。
3. **Point 范围**：只做 1D 区间（`LongPoint`/`IntPoint`，`newRangeQuery` 语义），等值由
   `[v,v]` 退化覆盖；不做 `newSetQuery`。
4. **forceMerge**：只做 `forceMerge(1)`——全部段归并为一个，Java 可读、CheckIndex 零错误，
   成功后删旧段文件，bitmap 按 `--bitmap` 重建；不做目标段数可配。

方案选型（三选一，用户选方案一）：一个 M6 三任务 **Boolean → Point → forceMerge**，后者
复用前者新建的读路径。否决项：Boolean 彻底 roaring-native 重写（翻 M5 引擎，风险大收益小）；
forceMerge 走 stored 重放重索引（unstored 字段无法重建，语义错误）。

**§3 事实修正（用户确认）**：设计初稿假设"stored / DV 读路径已存在可复用"——核实后 codec
的 `stored_fields.rs`、`doc_values.rs` 均只有写方向，搜索侧无 stored 取回、无 sort-by-DV。
因此 stored 归并改走**块级裸拷贝**（不建文档级解析器），DV 读路径与 `.fdx` 块索引读为本
里程碑新建交付物。README 查询能力清单的过度声明（Sort by DV / stored 取回 / PointRange）
已同步修正。

## 1. 范围总览

| 任务 | 交付 | 主要新代码 |
|---|---|---|
| T-A 嵌套 Boolean | `Query::Bool` + 通用组合迭代器 + roaring 拍平 | `crates/core/src/search/`（query.rs、doc_iter.rs、roaring_exec.rs 接口复用） |
| T-B Point 区间 | `Query::PointRange` + BKD 读路径 | `codec-lucene9/src/points_read.rs`（新建）、search 侧变体 |
| T-C forceMerge(1) | `force_merge(dir, config)` + CLI + 各格式归并 | `core/src/merge.rs`（新建）、`codec-lucene9` DV 读 / `.fdx` 读 |

**明确不做**（YAGNI）：FILTER 子句；可配置 minimum_should_match；`newSetQuery` / 多维
points；forceMerge(N) 目标段数可配与段挑选策略；delete / `.liv` / norms；stored 文档级
解析器与 stored 取回查询能力；sort-by-DV 查询（本里程碑只做 DV 顺序读，仅供归并）；嵌套
Bool 全树 roaring-native 化（只做 §2 的拍平规则）；查询结果缓存。

**前提假设不变**：只读本系统写出的索引（无 delete / norms / vector）；写侧 CREATE；
schema 在各段间一致（同源 IndexWriter 产物的既有不变量）。

## 2. T-A：嵌套 Boolean 查询

### 2.1 Query 模型

```rust
pub enum Occur { Must, Should, MustNot }

pub enum Query {
    // ……现有变体全部保留（And/Or 平铺变体不动，bench 与电池在用）
    Bool { clauses: Vec<(Occur, Query)> },
}
```

子查询可为任意变体（Term / Phrase / Terms / Prefix / Wildcard / PointRange / MatchAll /
Bool），跨字段天然支持。新查询一律用 `Bool`；`And`/`Or` 作为平铺特例保留（内部可视同
语法糖，但不删）。

### 2.2 执行语义（ConstantScore 化简，对照 Lucene BooleanWeight）

- **有 MUST**：命中 = 全部 MUST 的合取，再排除 MUST_NOT 的并集；SHOULD **不参与执行**
  （无打分时 SHOULD 在有 MUST 的情况下对命中集零贡献——Lucene 语义原样，此处是化简而非
  偏差）。
- **无 MUST、有 SHOULD**：命中 = SHOULD 的析取（msm=1 即并集），再排除 MUST_NOT。
- **只有 MUST_NOT**：命中 = MatchAll 排除 MUST_NOT（Lucene 同款：纯否定子句以
  MatchAllDocsQuery 为正集）。
- 空 clauses 或全部子句在某段缺失：该段空迭代器（`None`），与现有语义一致。

### 2.3 迭代器组合（`doc_iter.rs` 新增，现有 postings 专用组合器不动）

- `ConjOver(Vec<SegmentDocIter>)`：首个为 lead，候选对其余逐个 `advance` 对齐，全齐即命中
  （Lucene ConjunctionDISI 协议）。
- `DisjOver(Vec<SegmentDocIter>)`：k 路归并——k 小（<8）用排序扫描，否则二叉堆；参照现有
  `DisjunctionDocIter` 的堆实现。
- `Excluding { main, prohibited }`：两指针——main 候选对 prohibited 做 `advance` 探测，
  撞上即弃（Lucene ReqExclScorer 协议）。多个 MUST_NOT 先 `DisjOver` 合成一个
  prohibited。
- `needs_freq`：Bool 路径恒 false；`freq_sum` 对 Bool 拒绝（同现有 And/Or / multi-term）。

### 2.4 roaring 拍平规则（保 M5 加速）

两层保加速：

1. **叶子层**：Term 叶子在通用组合中仍各自走 bitmap 视图（`RoaringDocIter`，现有逻辑），
   高 df 子句不吃亏。
2. **拍平层**：Bool（或嵌套 Bool 子树）满足以下形状时，整体改写进现有 roaring 三档引擎
   （`roaring_exec::segment_iterator` / `count`）：
   - **纯合取形**：全部子句 MUST、无 SHOULD/MUST_NOT，且递归展开后全部叶子是同字段
     Term → 按 And 三档执行；
   - **纯析取形**：无 MUST/MUST_NOT、全部子句 SHOULD，且递归展开后全部叶子是同字段
     Term → 按 Or 三档执行。
   其余形状走 §2.3 通用组合。拍平在 `Query::segment_iterator` 入口做形状判定，不改
   roaring_exec 接口。

### 2.5 count()

与迭代同一结构：拍平形命中 roaring count 快路径（`and_cardinality` / 物化 fold
cardinality）；非拍平形走组合迭代器逐 doc 计数。`MatchAll` 排除形 count = maxDoc −
prohibited count（避免全量迭代，纯 MUST_NOT 的常规优化）。

### 2.6 验收

- Java 对拍：`SearchBench` 查询文件扩展嵌套 bool 行（S 表达式风格单行，如
  `BOOL (AND (TERM message error) (OR (TERM level INFO WARN)) (NOT (TERM source tmp)))`），
  Java 侧构造同形 BooleanQuery，hit counts 逐条 diff。
- 形状覆盖：纯 MUST / 纯 SHOULD / MUST+SHOULD / MUST+MUST_NOT / 纯 MUST_NOT / 三层嵌套 /
  跨字段 / 含 Phrase·Prefix·Wildcard·PointRange 子句 / 拍平形命中 roaring 路径的 A/B
  （`RL_BITMAP=0`）。
- `make log-test` 电池接入。

## 3. T-B：Point 区间查询（1D BKD 读路径）

### 3.1 Query 模型

```rust
Query::PointRange { field: String, low: i64, high: i64 }  // 双闭区间
```

Lucene `newRangeQuery` 原样语义；排他边界由调用方 ±1 调整（避免 MIN/MAX 溢出特例）。
IntPoint 字段复用同一变体：读侧按 field infos 的 point dimension bytes（4/8）解包；
`low/high` 超出字段类型值域时 clamp，整区间出界返回空迭代器。

### 3.2 codec：BKD 读路径（`codec-lucene9/src/points_read.rs`，新建）

与写侧 `points.rs` 逐行对照反向（保持 file:line 引用惯例）：

- `.kdm`：解析元数据（字段数、每字段 root 位置 / 点数 / 每叶 512 / 深度）。
- `.kdi`：内部节点索引全量读入内存（1D 树小）。
- `.kdd`：叶数据按需读；节点 min/max 与查询区间比较，三分支——**不相交跳过 / 全包含整叶
  收 / 部分相交逐点过滤**。
- 叶内 docIDs 解码：写侧 `DocIdsWriter` 五分支（`points.rs` 已逐字节复刻写方向）的反向
  实现。

API 形态：`PointsReader::open(dir, segment, field) -> ...`；
`intersect(range, &mut FnMut(packed_value, doc_id))`（visitor，对照 Lucene
`PointValues.intersect`）。多值点（同 doc 多值）由调用方物化集合去重，天然正确。

### 3.3 执行

段内命中文档**物化成内存 croaring::Bitmap**：

- 迭代：包装为 `SegmentDocIter` 新变体（游标复用 `RoaringDocIter` 的批量迭代形态）。
- count：cardinality 直读。
- Bool 组合：普通子迭代器进 §2.3 组合器；**不参与 roaring 拍平**（拍平仅限同字段 Term
  叶子，§2.4）。
- `needs_freq` 恒 false。

**为什么物化而非增量遍历**：Lucene PointRangeQuery 对 count 本身就是 visitor 全量收集；
1M docs 单字段命中集物化 ≤128KB（bitset 容器）；换来 count O(1)、Bool 组合零特例、
forceMerge 全量取点的现成通道。增量 merge-sort DocIdSet 的收益只在"超大命中 + top-N
提前退出"，本项目无此场景（YAGNI）。

### 3.4 验收

- 查询文件加 `RANGE field low high` 行；Java `LongPoint.newRangeQuery` 逐条 diff。
- 边界四类：不相交区间（命中 0；`low>high` 与 Lucene 一致直接报错）、全区间（MIN..MAX）、
  单边贴 MIN/MAX、IntPoint 字段。
- `make log-test` 电池接入（log schema 的 timestamp 为 LongPoint）。

## 4. T-C：forceMerge(1) —— 格式级段归并

### 4.1 入口与总流程

```rust
pub fn force_merge(dir: &FSDirectory, config: &IndexWriterConfig) -> io::Result<()>
```

读当前 `segments_N` 指向的全部段 → 逐格式归并写出一个新段 → 新 `.si` → 两段式
`segments_N` 提交 + fsync（复用 `index_writer.rs` 现有 `commit_infos` 路径）→ 成功后删除
被替换的旧段文件与旧 `segments_N`（Java 同款行为）。单线程。CLI：
`rustlucene-cli forcemerge <indexDir> [--bitmap] [--bitmap-threshold N]`。

**文档序**：新段 = 各段按提交顺序**顺序拼接**，`doc_new = doc_base + doc_old`（doc_base
为前序各段 maxDoc 累加）。由此 postings 归并**无需交错合并**——每个 term 的文档流即各段
顺序拼接 + 偏移，天然升序。

**中途失败**：旧提交点完好（新 `segments_N` 未 rename）；已写出的新段文件按已知文件名
清单清理（与 Java 的 abort 语义对齐，尽力而为）。

### 4.2 逐格式归并

- **field infos**：各段 `.fnm` 逐字段断言一致（同源写出的既有不变量），直接复用；不一致
  报错，不做全局重编号。
- **postings**（核心）：按字段对各段词典 k-way 归并；每个 term 依次读各段 postings 枚举
  （doc += base、freq 原样、positions 按 doc 序原样拼接——复用 `postings_read.rs` 的
  `positions(&entry)`），**走 flush 同款低层编码器重新编码**（FOR/PForDelta、4096 跳表
  自动重建）；df / totalTermFreq 直接累加；`--bitmap` 且归并后 df ≥ 阈值的 term 按 M3
  格式重建 bitmap（旧 bitmap 不复用——df 已变）。新词典走现有 FST builder 重建
  `.tim/.tip/.tmd`。
- **stored fields（块级裸拷贝，修正后方案）**：不建文档级解析器。读源段 `.fdx` 块索引
  （DirectMonotonic 读，本里程碑新建的读方向）定位每个压缩 chunk 的 `.fdt` 字节区间，
  整 chunk 字节直接追加到目标 `.fdt`，同步重建目标 `.fdx/.fdm`（doc base 偏移、块地址
  重记录）。对照 `SegmentMerger.mergeStoredFields` 的 bulk-copy 主路径：同源 codec、无
  delete，满足裸拷贝前提。不解压、不解析文档内容。
- **NumericDV**：新建 `.dvm/.dvd` 顺序读（数值块 + IndexedDISI docsWithField 的读方向），
  逐 doc 读值 + base 重映射，走现有 `doc_values.rs` writer 重写。
- **SortedDV**（经典难点）：读各段 SortedDV 字典（64 项/块前缀压缩 terms dict 的读方向）
  与逐 doc ord；各段字典各自有序 → k-way 归并出**全局有序字典**，预计算每段
  `ord_old → ord_new` 重映射表，逐 doc 重写 ord。
- **points**：用 §3.2 的 BKD 读路径全量遍历 `(value, docID)` + base 重映射，灌回现有
  BKD writer（全内存排序，天然吸收多段输入）。
- **segment info**：新段名取下一段号（沿用现有命名规则），maxDoc = 各段之和，
  diagnostics 标 `source=merge`；attributes / id 生成规则与 Java `SegmentMerger` 对齐。

### 4.3 新建读路径清单（本里程碑 codec 交付物）

| 读路径 | 文件 | 服务 |
|---|---|---|
| BKD 读 | `points_read.rs`（新建） | T-B 查询 + T-C 取点 |
| DV 顺序读（Numeric + Sorted 字典/ord） | `doc_values.rs` 内加读方向或新 `doc_values_read.rs` | T-C |
| `.fdx` 块索引读（DirectMonotonic 读方向） | `stored_fields.rs` 内加读方向 | T-C 裸拷 |

stored 文档级解析（LZ4 解压 + 字段解析）**不做**——forceMerge 不需要，stored 取回查询
能力不在本里程碑。

### 4.4 验收

- `make log-test` 增加 Rust-forceMerge 变体：多段索引（8 线程写入产物）→ forceMerge →
  ① Java CheckIndex 零错误；② 合并前后全量查询 diff（term / bool / multi-term / range /
  phrase / bitmap A/B 两侧）逐条一致；③ 与同语料 Java forceMerge(1) 产物查询 diff 交叉
  一致。
- 单测边界：单段（退化为重打包）、空段（0 docs）、全稀疏 DV 字段、无 points 字段、
  bitmap on/off、positions 字段。
- 归并后 `Searcher.segment_count() == 1`，旧文件已清理（目录清单断言）。

## 5. 风险与对策

1. **SortedDV ord 重映射**：全里程碑最易错点。对策：先写"字典归并 + 重映射表"纯函数
   单测（手工构造 2-3 段小字典，含重复值、段内空值），再接格式层。
2. **positions 拼接**：pos 无需偏移但 doc 序必须严格升序，靠 §4.1 拼接性质保证；单测
   断言归并产物与"一次写入同语料"的 phrase 查询结果一致（不必字节一致——块边界允许
   不同，查询语义一致即可）。
3. **Bool 拍平形状判定漏拍**：仅以 hit counts 对拍无法发现性能回退。对策：拍平形查询
   在 bench 查询集中保留，M6 末尾复跑一次三路 bench 确认无回退（对照 M5 基线）。
4. **范围外文件的并发改动**：另有可能的并发会话活动（M4 曾遇 Cargo.lock 污染）。每个
   任务开工前 `git status` 核实。

## 6. 任务拆分建议（供 writing-plans 参考）

T-A（Bool）与 T-B（Point）无依赖可并行；T-C 依赖 T-B 的 BKD 读路径，排最后。每个任务
SDD 双闸门（task-brief → implementer → review-package → reviewer → 记账），终审串联。
