# Rust 搜索读路径设计

**日期:** 2026-07-22
**状态:** Draft（待用户 review）
**设计原则:** 执行语义对齐 Lucene 9.12.3 源码（保持 file:line 引用的项目惯例），对象结构用 Rust enum + trait 表达。**不加打分，不加不必要抽象。**

## 1. 目标与范围

为 lucene-rust 增加纯 Rust 搜索读路径：Rust 直接读取自己写出的 Lucene 9.12.3 兼容索引并执行查询。匹配写入侧已支持的所有字段类型。

### 1.1 实现的 Query（8 种，全部 ConstantScore）

| Query | 依赖写入类型 | 实现方式 |
|-------|------------|---------|
| **Term** | Text / TextWithPositions / Keyword | FST 精确查找 → postings 迭代 |
| **Boolean** (AND/OR) | 任意组合 | lead-iterator 对齐（AND）/ 堆合并（OR） |
| **Phrase**（slop ≥ 0） | TextWithPositions | AND 候选 → position 逐项验证相邻性 |
| **Terms**（IN 语义） | Text / TextWithPositions / Keyword | Boolean SHOULD 的语法糖，直接展开为 OR |
| **PointRange**（1D） | LongPoint / IntPoint | BKD 树 intersect，边界包含语义对齐 Lucene |
| **Prefix** | Text / TextWithPositions / Keyword | FST prefix_iter → 隐式 rewrite 为 Boolean SHOULD |
| **Wildcard**（`*` + `?`） | Text / TextWithPositions / Keyword | 模式分类 → 选择最优 FST 扫描策略（无 automaton） |
| **MatchAll** | 无 | [0..maxDoc) 扫描，跳过 liveDocs 已删除 |

### 1.2 实现的读能力（codec 层）

| 能力 | 依赖文件 |
|------|---------|
| Term dictionary 查找 | `.tip` (FST) + `.tim` (term metadata) |
| Postings 解码 | `.doc` (FOR/PForDelta) + `.pos` (positions) |
| Skip list 跳跃 | `.doc` 内的两级 skip data |
| NumericDocValues 读取 | `.dvd` + `.dvm`（DirectWriter + IndexedDISI） |
| SortedDocValues 读取 | `.dvd` + `.dvm`（ords + LZ4 terms dict + reverse index） |
| BKD 1D 范围查询 | `.kdi` (packed index) + `.kdd` (leaf data) |
| Stored fields 取回 | `.fdt` (LZ4 块) + `.fdx`/`.fdm` (DirectMonotonic) |
| LiveDocs 删文过滤 | `.liv` (FixedBitSet) |

### 1.3 实现的排序

| SortField | 对应写入 | 逻辑 |
|-----------|---------|------|
| DocOrder | 无 | 默认，文档原有顺序 |
| NumericValue | NumericDocValues | 按 i64 排序，MISSING 排尾（对齐 Lucene FieldComparator.LongComparator） |
| SortedValue | SortedDocValues | 按 ord 排序，MISSING 排尾 |

### 1.4 JNI 门面

通过 JSON 协议暴露给 Java 上层（扩展 `crates/jni-binding`）：

```
Java: RustIndexSearcher
  open(indexPath) -> handle
  search(handle, queryJson, topN, sortJson) -> resultsJson
  close(handle)
```

Query 以 JSON 传入：`{"type": "term", "field": "message", "term": "error"}` 等。句柄表复用写侧 IndexWriter 的 Arc + HashMap 模式。

目的不是替代 Java Lucene，而是让 Java 上层能验证 Rust 读路径的正确性（跨语言 diff 测试），以及为将来纯 Rust 场景提供读能力。

### 1.5 明确不做

| 不做 | 原因 |
|------|------|
| BM25 / TF-IDF 打分 | 无 norms，无评分数据 |
| Weight / Scorer 抽象层 | 无打分 → 不需要中间层 |
| SpanQuery 系列 | 无 offsets |
| FuzzyQuery / RegexpQuery / TermRangeQuery | 需 FST automaton，复杂度高 |
| 多维 BKD | writer 仅支持 1D |
| SortedNumeric / SortedSet / Binary DV | writer 不写这些类型 |
| Vector KNN | writer 不写 vector |
| QueryParser 文本语法 | 用户确认只需 Query 执行层 |
| Segment 间并行 | 首版单线程逐段，并行留 trait 接口 |
| NRT 原地 refresh | open 即快照，重开即刷新 |
| Soft deletes | 仅硬删除（LiveDocs bitset） |

## 2. 选型：为什么不用 Lucene 的 Query/Weight/Scorer 三层

Lucene 的 `Query → Weight → Scorer → DISI` 层次中，`Weight` 的核心职责是计算 `Scorer.score()`。在没有评分的场景下：

- `Weight` 退化为一个工厂方法
- `Scorer` 退化为一个 `DocIdSetIterator` 的包装
- 约 30-40% 的代码服务于一个永远返回 1.0 的 `score()` 函数

Lucene 本身在 `ConstantScoreQuery` 中也有简化路径——`ConstantScoreScorer` 只转发 iterator，`ConstantScoreWeight` 只产生 scorer。

**本设计直接砍掉 Weight + Scorer。** `Query` 在 segment 上产生 `DocIterator`，`DocIterator` 直接就是 doc ID 流。布尔合取/析取在 `DocIterator` 层用组合模式实现。

对比：

| | Lucene（简化打分） | 本设计 |
|---|---|---|
| 抽象层数 | Query → Weight → Scorer → DISI | Query → DocIterator |
| 每个 TermQuery 的 struct | TermQuery + TermWeight + TermScorer | TermQuery（execute 产 PostingsDocIterator） |
| Boolean AND | ConjunctionDISI（分级 priority queue） | ConjunctionDocIterator（lead-iterator 两两对齐） |
| 代码量 | ~1000 行（仅 Query/Scorer 层） | ~500 行 |

Lucene 的 ConjunctionDISI 用多级 priority queue 是为了和 Impacts（Block-Max 打分跳过）协同——没打分就不需要这层复杂度。用简单的 lead-iterator 算法（详见 §4.4）查出的 doc ID 序列与 Lucene 完全一致，只是迭代方式不同。

## 3. 架构

### 3.1 Crate 组织

```
codec-lucene9/                       ← 现有 write + 新增 reader
├── src/
│   ├── io.rs                        ← ADD: IndexInput trait + BufferedIndexInput + HeapIndexInput
│   ├── directory.rs                 ← ADD: open_input(path)
│   ├── fst.rs                       ← ADD: Fst::lookup / prefix_iter / scan_range
│   ├── postings_ll.rs               ← ADD: FOR/PForDelta decode（标量 + AVX2）
│   ├── postings_reader.rs           ← NEW: PostingsReader + 3 种 PostingsEnum
│   ├── doc_values_reader.rs         ← NEW: NumericDV + SortedDV reader（含 IndexedDISI）
│   ├── points_reader.rs             ← NEW: 1D BKDReader + intersect
│   ├── stored_fields_reader.rs      ← NEW: LZ4 解压 + StoredFieldsReader
│   ├── segment_reader.rs            ← NEW: SegmentReader（组合所有 reader + 惰性加载）
│   ├── packed/                      ← ADD: DirectReader + DirectMonotonicReader
│   └── lib.rs                       ← ADD: pub mod 导出

rustlucene-core/src/
├── search/
│   ├── mod.rs
│   ├── query.rs                     ← Query enum + execute 分发
│   ├── doc_iterator.rs              ← DocIterator trait + 7 种实现
│   ├── collector.rs                 ← LeafCollector / Collector trait + TopDocsCollector（3 路排序）
│   ├── sort.rs                      ← SortField / SortValue
│   └── searcher.rs                  ← IndexSearcher

crates/jni-binding/src/              ← ADD reader.rs（JNI 门面）
```

### 3.2 核心抽象

三个 trait，对标 Lucene 的核心接口但去掉打分：

```
Lucene                         本设计
─────────────────────────────────────────
DocIdSetIterator               DocIterator
  .docID()                       (通过 next()/advance() 返回)
  .nextDoc()                     .next() -> Option<u32>
  .advance(target)               .advance(target) -> Option<u32>
  .cost()                        .cost()
  —                              .next_block() -> Option<DocBlock>

LeafCollector                   LeafCollector
  .collect(doc)                  .collect(doc)
  —                              .collect_batch(block)

CollectorManager                Collector
  .newCollector()                .get_leaf_collector(ctx)
  .reduce()                      .merge()

Query                           Query (enum)
  .createWeight()                .execute(segment) -> DocIterator
```

### 3.3 惰性加载

`DirectoryReader::open()` 只读 `segments_N` + 每个 segment 的 `.si` + `.fnm`。`.fdt` 在内的底层文件按需打开：

- FST（`.tip`）→ 首次 PostingsReader 访问该字段时加载到内存
- BKD（`.kdi` + `.kdd` 文件）→ 首次 BKDReader::intersect 时打开
- DocValues（`.dvd` + `.dvm`）→ 首次 doc_values_reader(field) 时打开
- StoredFields（`.fdt` + `.fdx` + `.fdm`）→ 首次 stored_fields() 时打开

`SegmentReader` 用 `OnceCell`（或 `RefCell<Option<>>` 因为不是 Sync）管理这些惰性资源。这避免了打开一个 100-field 索引时要 mmap 全部 100 个文件——只在查询实际触及字段时才打开。

### 3.4 依赖图

```
IndexSearcher
  ├── DirectoryReader
  │     ├── SegmentReader × N
  │     │     ├── FieldInfos (.fnm)                  ← 热路径，常驻
  │     │     ├── PostingsReader                     ← 惰性
  │     │     │     ├── FST (.tip) → lookup / prefix_iter / scan_range
  │     │     │     ├── TermMetadata (.tim) → TermState
  │     │     │     ├── PostingsDecoder (.doc) → FOR/PForDelta → PostingsEnum
  │     │     │     └── PositionsDecoder (.pos) → PositionsEnum
  │     │     ├── DocValuesReader (.dvd+.dvm)        ← 惰性
  │     │     │     ├── NumericDocValues (DirectWriter + IndexedDISI)
  │     │     │     └── SortedDocValues (ords + LZ4 terms dict + reverse index)
  │     │     ├── BKDReader (.kdi+.kdd)              ← 惰性
  │     │     ├── StoredFieldsReader (.fdt+.fdx+.fdm) ← 惰性
  │     │     └── LiveDocs (.liv, FixedBitSet)
  │     └── SegmentInfos (segments_N)
  └── Query::execute(segment) → DocIterator
       ├── PostingsDocIterator     ← 包装 PostingsEnum
       ├── ConjunctionDocIterator  ← AND
       ├── DisjunctionDocIterator  ← OR
       ├── PhraseDocIterator       ← AND + position 验证
       ├── BKDResultIterator       ← BKD intersect 结果
       └── AllDocIterator          ← [0..maxDoc)
```

### 3.5 搜索数据流

```
输入: queryJson, topN, sortSpec
  1. parse_query(json) → Query
  2. searcher.search(query, collector)
  3. 对每个 SegmentReader:
     a. collector.get_leaf_collector(segment) → LeafCollector
     b. iter = query.execute(segment) → DocIterator
     c. while let Some(block) = iter.next_block():
          leaf_collector.collect_batch(&block)
  4. collector.merge() → topN doc IDs
  5. 批量 stored_fields 取回 → 填充结果
输出: SearchResults { total_hits, hits[{doc_id, fields, sort_values}] }
```

## 4. IndexInput 读抽象

码读侧一切格式读的地基。对标 Lucene `IndexInput`，Rust 化设计：

```rust
/// 对等 IndexOutput — 支持随机读 + 顺序读
pub trait IndexInput: Read {
    fn read_byte(&mut self) -> io::Result<u8>;
    fn read_bytes(&mut self, buf: &mut [u8], offset: usize, len: usize) -> io::Result<()>;
    fn read_vlong(&mut self) -> io::Result<i64>;
    fn read_vint(&mut self) -> io::Result<i32>;
    fn read_zint(&mut self) -> io::Result<i32>;     // zigzag VInt
    fn read_string(&mut self) -> io::Result<String>; // VInt len + UTF-8
    fn file_pointer(&self) -> u64;
    fn seek(&mut self, pos: u64) -> io::Result<()>;
    fn length(&self) -> u64;
    /// 创建子切片（独立 file_pointer，不共享父 reader 状态）
    fn slice(&self, offset: u64, len: u64) -> io::Result<Box<dyn IndexInput>>;
}
```

两种实现：

| 实现 | 适用场景 | 策略 |
|------|---------|------|
| **BufferedIndexInput** | 大文件（.doc / .dvd / .kdd / .fdt） | `File` + 8KB ReadBuf，seek 时 invalidate 缓冲区 |
| **HeapIndexInput** | 小文件（.fnm / .si / .tip / .kdm） | 一次性读到 `Vec<u8>`，零拷贝随机访问 |

首版不做 mmap——BufferedIndexInput 更可移植，32 位也不会炸。MMap 作为后续优化点（收益：跳过用户态 buffer copy，代价：SIGBUS 处理 + 地址空间碎片）。

`slice()` 是关键方法——它让 postings reader 在 term 的 doc 数据段上创建一个独立子 reader，主 reader 继续定位到下一个 term。这样不同 term 的 postings 块解码互不干扰。

## 5. DocIterator + DocBlock 批处理

### 5.1 设计思路

Lucene 的 `LeafCollector.collect(doc)` 是逐文档回调——但 postings 天然按 128 值 block 解码（PFOR 块大小 = 128），**整块在手时逐 doc 喂给 collector 是对解码成果的浪费**。把消费粒度从 "per-doc" 提升到 "per-block"：

```rust
/// 一个解码好的文档块 — 对齐 PFOR 128 值 block
/// 全部栈分配，无堆分配，对 cache 友好
pub struct DocBlock {
    pub docs: [u32; 128],
    pub len: u8,      // 1..=128，tail block < 128
}

pub trait DocIterator: Send {
    /// 单 doc 推进 — conjunction/disjunction 内部使用
    fn next(&mut self) -> Option<u32>;
    fn advance(&mut self, target: u32) -> Option<u32>;
    fn cost(&self) -> usize;

    /// 批量消费 — collector 使用，返回一个解码 block
    /// 默认实现 fallback 到逐 doc next()，postings 来源覆写为直接返回 PFOR 块
    fn next_block(&mut self) -> Option<DocBlock> {
        let mut block = DocBlock { docs: [0u32; 128], len: 0 };
        for i in 0..128 {
            match self.next() { Some(d) => block.docs[i] = d, None => break; }
            block.len += 1;
        }
        if block.len == 0 { None } else { Some(block) }
    }
}
```

关键设计决策：
- **栈分配 `[u32; 128]`** 而非 `Vec<u32>`——128 是固定常数（PFOR block size），栈分配避免每次解码都做堆分配。128 × 4 = 512 字节，恰好 L1 cache line 友好的大小
- **默认 `next_block()` 实现**——BKD/Phrase 等非 postings 来源直接 fallback 到逐 doc 填充，无需额外代码
- **PostingsDocIterator 覆写 `next_block()`**——直接返回 PFOR 解码块，零额外分配

### 5.2 Conjunction（AND）算法

不用 Lucene ConjunctionDISI 的多级 priority queue。只做简单的 lead-iterator 两两对齐：

```
输入: iterators 按 cost() 升序排列
算法:
  1. lead = iterators[0]
  2. 如果 lead.next() == None → 结束
  3. target = lead 的当前 doc
  4. 对每个 other ∈ iterators[1..]:
     a. doc = other.advance(target)
     b. 如果 doc == target → 继续下一个
     c. 如果 doc == None → 结束
     d. 如果 doc > target:
        target = doc
        lead.advance(target) 或从头重试 → 回到步骤 3
  5. 全部匹配 → 返回 target，回到步骤 2
```

与 Lucene ConjunctionDISI 的区别：Lucene 用 max-heap 按 doc ID 排序所有 iterator，每次取堆顶推进。我们的算法等价——都保证 doc ID 递增的合取。lead-iterator 方式代码量少一半，正确性更容易论证。

### 5.3 Disjunction（OR）算法

```
输入: iterators
算法:
  1. 将所有 iterator 放入 min-heap（按当前 doc ID）
  2. 弹出最小 doc
  3. 推进该 iterator 到 > 当前 doc 的位置
  4. 如果下一个 doc ≠ 上次产出的 doc（去重）→ 产出
  5. 如果 iterator 未耗尽 → 推回堆
```

### 5.4 PhraseDocIterator

两阶段：外层 AND conjunction 提供候选 → 内层 position 验证：

```rust
impl DocIterator for PhraseDocIterator {
    fn next(&mut self) -> Option<u32> {
        for doc in &mut self.candidates {
            // 为每个 term 拉取该 doc 的 positions
            for (i, pos_enum) in self.pos_enums.iter_mut().enumerate() {
                pos_enum.advance_to_doc(doc);
            }
            // 以第一个 term 的 position 为锚点
            while let Some(base) = self.pos_enums[0].next_position() {
                let mut matched = true;
                for (offset, pe) in self.pos_enums[1..].iter_mut().enumerate() {
                    let target = base + (offset + 1) as u32;
                    match pe.next_position() {
                        Some(p) if p <= target + self.slop && p >= target.saturating_sub(self.slop) => {},
                        _ => { matched = false; break; }
                    }
                }
                if matched { return Some(doc); }
            }
        }
        None
    }
}
```

Lucene 的 `PhrasePositions` 用 linked list + `ExactPhraseMatcher` / `SloppyPhraseMatcher`。我们的简化版不需要 scorer 接口，直接在 DocIterator 内部做完。slop=0 时就是严格连续；slop>0 时允许位置有少许偏移。

### 5.5 Wildcard 模式分类

不实现 automaton，直接按模式形状选择最优的 FST 扫描策略：

```
"foo*"  → FST prefix_iter("foo")                          // 最优：精确前缀
"*foo"  → FST scan_all → filter ends_with("foo")          // 退而求其次：全扫
"f?o"   → FST prefix_iter("f") + 长度校验 + suffix 校验     // ? 表示恰好一个字符
"f*o*"  → FST prefix_iter("f") + glob 匹配                // 取最早的 prefix，filter suffix
"*f*o*" → FST scan_all + glob 匹配                        // 没有锚点，只能全扫
```

这个策略对常见场景（`foo*` 前缀、`*.log` 后缀）是高效的。唯一的退化情况是 `*contains*` 模式，需要全 FST 扫描——但 Wildcard 本身就是这样，Lucene 也是 automaton 遍历全部 term。

## 6. Collector（按排序类型分三路优化）

### 6.1 接口

```rust
pub trait LeafCollector {
    fn collect(&mut self, doc: u32) -> Result<bool>;
    fn collect_batch(&mut self, block: &DocBlock) -> Result<bool> {
        for i in 0..block.len as usize {
            if !self.collect(block.docs[i])? { return Ok(false); }
        }
        Ok(true)
    }
}

pub trait Collector {
    fn get_leaf_collector(&self, ctx: &LeafCollectorContext) -> Result<Box<dyn LeafCollector>>;
    fn merge(self: Box<Self>) -> Result<SearchResults>;
}

pub struct SearchResults {
    pub total_hits: usize,
    pub doc_ids: Vec<u32>,
}
```

### 6.2 三路优化

**路 1：topN by DocOrder（无排序，默认）**

最简路径——所有文档 score=1.0，前 N 个文档就是 top-N：

```rust
impl LeafCollector for DocOrderCollector {
    fn collect_batch(&mut self, block: &DocBlock) -> Result<bool> {
        // block 内文档天生按 doc ID 递增，直接 push
        let room = self.limit - self.docs.len();
        let take = room.min(block.len as usize);
        self.docs.extend_from_slice(&block.docs[..take]);
        Ok(self.docs.len() < self.limit)  // 满了就停止
    }
}
```

零堆操作，零比较。一次 `extend_from_slice` 完成。

**路 2：topN by NumericDocValues**

需要批量读取 DV 值并维护堆：

```rust
impl LeafCollector for NumericSortCollector {
    fn collect_batch(&mut self, block: &DocBlock) -> Result<bool> {
        // 批量读取 128 个 doc 的 DV 值
        let values = self.dv_reader.get_batch(&block.docs[..block.len as usize])?;
        for (i, &doc) in block.docs[..block.len as usize].iter().enumerate() {
            let val = values[i].unwrap_or(i64::MIN); // MISSING → 排尾
            // 维护 max-heap（容量 limit）
            if self.heap.len() < self.limit {
                self.heap.push(ScoredDoc { doc, sort_value: val });
            } else if val > self.heap.peek().unwrap().sort_value {
                self.heap.pop();
                self.heap.push(ScoredDoc { doc, sort_value: val });
            }
        }
        Ok(true)
    }
}
```

`get_batch()` 在 DirectWriter 上做连续解压，比逐 doc 调用 `get(doc)` 高效得多——一次 virtual call、连续内存访问。

**路 3：topN by SortedDocValues**

与 Numeric 同理，但比较对象是 ord（u32）。MISSING 值 = ord + 1（排尾）。

### 6.3 排序的 MISSING 规则（对齐 Lucene FieldComparator）

| SortField | 有值文档 | 无值文档（MISSING） |
|-----------|---------|-------------------|
| DocOrder | 原始顺序 | 参与，无区别 |
| NumericValue (asc) | 按值升序 | 排尾（i64::MIN） |
| NumericValue (desc) | 按值降序 | 排尾（i64::MIN） |
| SortedValue (asc) | 按 ord 升序 | 排尾（最大值） |
| SortedValue (desc) | 按 ord 降序 | 排尾（最大值） |

为什么 Numeric 用 i64::MIN 而 Sorted 用最大 ord？Lucene 的 `LongComparator` 中 `missingValue = ascending ? Long.MAX_VALUE : Long.MIN_VALUE`——升序时 MISSING 排尾（用 MAX_VALUE），降序时 MISSING 也排尾（用 MIN_VALUE）。SortedDocValues 中 MISSING ord = `reverse_index.len()`（超出范围的 ord）。首版照抄这些规则。

## 7. SIMD 加速

### 7.1 主战场：解码层

写侧编码事实决定了读侧热点全部是**固定位宽整数块解码**——SIMD 的原型场景。Lucene 10 为同一格式引入了 `VectorizedForUtil`（见 `lucene/core/src/java/org/apache/lucene/internal/vectorization/`），证明字节格式不变的前提下解码侧可以自由使用 SIMD。

| 热点 | 编码格式 | SIMD 方案 |
|------|---------|----------|
| postings .doc/.pos 的 128 值块 | `pfor_util_encode`（ForUtil/PForDelta） | AVX2 位 unpack：8 × u32 并行 shift + mask |
| NumericDV / SortedDV ords | `DirectWriter`（gcd=1，写侧既定） | 同一套 bit-unpack 核；gcd=1 保证无跨块接缝 |
| BKD 叶 DocIdsWriter | BPV_24 / BPV_32 / DELTA_BPV_16 | 同一套核 |

### 7.2 实现纪律（防止 SIMD 引入格式偏差）

**规则：先标量，再 SIMD，bench 数据驱动。**

1. **Phase 1：标量参考实现**。通过全部测试（round-trip + Java diff 终验），锁定正确性
2. **Phase 2：SIMD 快路径**。`is_x86_feature_detected!("avx2")` 运行时分发。每条 SIMD 路径配一个标量对拍单测（同一输入 → 同一输出，逐值相等）
3. **Phase 3：bench 验证**。只在 profile 证实热点的模块开启 SIMD。大部分收益集中在：高命中 term/boolean 的 .doc 全块扫描、DV 排序的列式取值

### 7.3 批处理 Collector 中的 SIMD 机会（二阶优化）

以下为设计预留，首版不实现，待 Phase 3 bench 数据决定是否值得：

- **count 查询**：`DocBlock` 全块直接 `total += block.len`。DOCS 字段稠密时进一步退化为 bitset popcount（AVX2 查表 popcount）
- **DV 排序的块级预筛**：块内 DV 值域全部 < 堆顶 → 整块跳过（零堆操作）
- **Boolean 合取的 SIMD intersect**：两块 128-doc block 直接 `_mm_shuffle_epi8` 查表做交集（Lemire 算法），标量 galloping 作为对照

## 8. Stored Fields 取回 + 结果构建

搜索产生 doc ID 列表后，按需取回 stored fields：

```rust
impl IndexSearcher {
    /// 批量取回文档内容
    fn docs(&self, doc_ids: &[u32], fields: &[String]) -> Result<Vec<HashMap<String, FieldValue>>>;
}
```

工作方式：
1. 按 segment 分组 doc_ids（一个 doc 只属于一个 segment）
2. 对每个 segment 调用 `StoredFieldsReader::visit_document(doc_id, visitor)`
3. LZ4 解压用 `lz4` crate（成熟实现，不自己写）
4. 结果按原始 doc 顺序返回（即便跨 segment）

## 9. 错误处理

### 9.1 Codec 层

每个格式读入口校验 magic / codec header / version（对齐 Lucene `CodecUtil.checkHeader`）：

```rust
fn check_header(input: &mut dyn IndexInput, expected: &[u8], version: u32) -> io::Result<()> {
    let mut magic = vec![0u8; expected.len()];
    input.read_bytes(&mut magic, 0, magic.len())?;
    if magic != expected {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("invalid codec header: expected {:?}, got {:?}", expected, magic)));
    }
    let actual = input.read_vint()? as u32;
    if actual != version {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("version mismatch: expected {version}, got {actual}")));
    }
    Ok(())
}
```

Footer checksum 校验默认开启，损坏即返回 `CorruptIndex` 错误。

### 9.2 Query 层

不合法查询在 execute 时 fail-fast：
- 对无 positions 字段发 PhraseQuery → `Err(InvalidInput("field 'msg' has no positions"))`
- 对无 DV 字段排序 → `Err(InvalidInput("field 'latency' has no NumericDocValues"))`
- 对不存在的字段发 TermQuery → 返回 EmptyDocIterator（0 个结果，不报错——对齐 Lucene 行为）
- 对文本字段发 PointRangeQuery → `Err(InvalidInput("field 'message' has no points"))`

### 9.3 JNI 边界

Rust panic 不穿越 FFI：
- JNI 函数体用 `catch_unwind` 包裹
- panic → Java `RuntimeException`
- 正常错误 → 错误码 + Java 异常（`IllegalArgumentException` 等）

句柄表采用与写侧 IndexWriter 同模式：`Arc<Mutex<HashMap<i64, Arc<Searcher>>>>`。

## 10. Query JSON 协议

用于 JNI 和 CLI 测试。例：

```json
// Term
{"type": "term", "field": "message", "term": "error"}

// Boolean (AND error + timeout)
{"type": "boolean", "clauses": [
  {"query": {"type": "term", "field": "message", "term": "error"}, "occur": "must"},
  {"query": {"type": "term", "field": "message", "term": "timeout"}, "occur": "must"}
], "minShouldMatch": 0}

// Boolean (OR error OR timeout)
{"type": "boolean", "clauses": [
  {"query": {"type": "term", "field": "message", "term": "error"}, "occur": "should"},
  {"query": {"type": "term", "field": "message", "term": "timeout"}, "occur": "should"}
], "minShouldMatch": 1}

// Terms (IN ["error", "timeout"])
{"type": "terms", "field": "level", "terms": ["ERROR", "WARN"]}

// Phrase ("hello world", slop=0)
{"type": "phrase", "field": "message", "terms": ["hello", "world"], "slop": 0}

// PointRange (timestamp in [lower, upper])
{"type": "point_range", "field": "timestamp", "lower": 1700000000000, "upper": 1800000000000}

// Prefix (err*)
{"type": "prefix", "field": "message", "prefix": "err"}

// Wildcard (*time*)
{"type": "wildcard", "field": "message", "pattern": "*time*"}

// MatchAll
{"type": "match_all"}
```

Parser 直接用 `serde_json` 反序列化，复用 core 的 JSON 基础设施。

## 11. 测试策略（三层递进 + Java 终验）

### 层 1：Round-trip 单元测试（codec crate）

延续现有惯例，每个 reader 与对应 writer 往返验证：

```
□ VLong/VInt/ZInt encode → decode 往返
□ FOR/PForDelta encode → decode 往返（128 值 block 逐值相等）
□ DirectWriter/DirectMonotonic encode → decode 往返
□ FST compile → lookup / prefix_iter / scan_range 正确性
□ LZ4 compress → decompress 往返
□ PostingsReader: 写 100 个 doc → 读回 3 种 PostingsEnum，doc/freq/pos 逐值相等
□ NumericDV: 写 → 读，涵盖 DENSE / SPARSE / ALL 三种 IndexedDISI 分支
□ SortedDV: 写 → 读，ord 映射 + term 重建正确
□ BKDReader: 写 10K points → intersect 各边界，doc IDs 一致
□ StoredFields: 写 → 读，6 种 FieldValue 类型无损往返
□ SegmentReader: 写 → 读，FieldInfos + LiveDocs 一致
```

### 层 2：搜索语义单元测试

固定语料 + 已知 doc ID 序列，断言每个 Query 的输出：

```
□ TermQuery: df=1 的 term / df=100 的 term / 不存在的 term
□ Boolean AND: 2/3 clause，含空子句
□ Boolean OR: 2/3 clause，含空子句，去重
□ Phrase: slop=0 / slop=1 / slop=2，跨 block 边界
□ Prefix: prefix 无匹配 / 单匹配 / 多匹配
□ Wildcard: "foo*" / "*foo" / "f?o" / "*foo*bar*"
□ Terms: 空列表 / 单 term / 多 term
□ PointRange: [lower, upper] / [lower, ∞) / (-∞, upper] / 无交集
□ MatchAll: 含 deleted docs
□ 多 segment: 跨 segment 结果合并
```

### 层 3：统测（Java diff 终验）

沿用 m2 的 `make log-test` 模式：

```
make verify-search:
  1. Rust write 索引（同语料）
  2. Rust search 每种 Query，输出 JSON 结果
  3. Java SearchBench 同 Query，输出 results
  4. Python diff: Rust doc IDs == Java doc IDs，逐条比对
```

### 现有测试不破坏

```
interop/verify-index.sh  → 不变
interop/verify-log.sh    → 不变
interop/compare-index.sh → 不变
```

新增：

```
interop/verify-search.sh → Rust write → {Rust read, Java read} → diff
```

## 12. 实现顺序（6 阶段）

```
Phase 1 — 地基（io + packed）
  □ IndexInput trait + BufferedIndexInput + HeapIndexInput
  □ FSDirectory::open_input()
  □ FOR/PForDelta 标量解码
  □ DirectReader + DirectMonotonicReader
  里程碑: 所有 primitive 编解码 round-trip 通过

Phase 2 — FST + Postings
  □ FST::lookup / prefix_iter / scan_range
  □ PostingsReader（PostingsEnum × 3 + 两级 skip list + singleton DF=1）
  □ PostingsDocIterator（覆写 next_block 为 PFOR 直出）
  里程碑: 单 term 查询能返回 doc IDs

Phase 3 — DV + BKD + StoredFields
  □ NumericDocValuesReader（IndexedDISI 三种分支）
  □ SortedDocValuesReader（ords + LZ4 terms dict + reverse index）
  □ BKDReader 1D intersect
  □ StoredFieldsReader（LZ4 解压 + 6 种值类型）
  里程碑: 所有 codec reader round-trip 通过

Phase 4 — Segment + Directory
  □ SegmentReader（惰性加载 + 组合所有 reader）
  □ DirectoryReader（segments_N 解析 + 聚合）
  里程碑: 能打开 Rust 写的索引，列出 segment

Phase 5 — Search
  □ Query enum + execute 分发
  □ ConjunctionDocIterator + DisjunctionDocIterator
  □ AllDocIterator + BKDResultIterator
  □ PhraseDocIterator（两阶段：AND + position）
  □ Wildcard 模式分类 + rewrite
  □ Terms 查询（OR 语法糖）
  里程碑: 每个 Query variant 的 doc ID 序列与 Java 一致

Phase 6 — Collector + Searcher + JNI
  □ LeafCollector / Collector trait
  □ DocOrderCollector / NumericSortCollector / SortedSortCollector
  □ IndexSearcher::search(query, collector)
  □ Query JSON parser（serde_json）
  □ JNI 门面（RustIndexSearcher.java + reader.rs）
  □ interop/verify-search.sh 集成测试
  里程碑: make verify-search 通过
```

## 13. 后续优化（不在本期）

- AVX2 SIMD bit-unpack 快路径（待标量通过 + bench 证实热点）
- Collector 块级预筛（DV 排序的 SIMD 预筛、count 查询的 bitset popcount）
- Boolean 合取的 SIMD intersect（Lemire 算法）
- MMap IndexInput（替代 BufferedIndexInput）
- Segment 间并行搜索（Rayon）
- NRT 原地 refresh（段文件 refcount）
- FuzzyQuery Levenshtein 自动机 + FST 求交
- RegexpQuery 正则自动机 + FST 求交
