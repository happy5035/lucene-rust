# 方案B：内存写入即可搜索 — 实现方案与详细流程

## 1. 设计目标

数据写入内存（DocWriter）后，无需 flush/commit 到磁盘 segment，即可直接搜索。
抽象出通用 `LeafReader` 接口，使磁盘 segment 和内存缓冲都能实现同一套读路径。

## 2. 整体架构

```
┌─────────────────────────────────────────────────────────┐
│                      Query Layer                         │
│   Query::Term / And / Or / Prefix / Wildcard /          │
│   Phrase / PointRange / Bool / MatchAll                 │
└──────────────────────┬──────────────────────────────────┘
                       │
        ┌──────────────┴──────────────┐
        ▼                             ▼
┌──────────────┐            ┌──────────────────┐
│  Searcher    │            │ MemorySearcher   │
│  (磁盘路径)  │            │  (内存路径)       │
└──────┬───────┘            └────────┬─────────┘
       │                             │
       ▼                             ▼
┌──────────────┐            ┌──────────────────┐
│SegmentReader │            │MemoryLeafReader  │
│  (文件IO)    │            │  (零拷贝借用)     │
└──────┬───────┘            └────────┬─────────┘
       │                             │
       ▼                             ▼
┌──────────────┐            ┌──────────────────┐
│ 磁盘文件     │            │ DocWriter 内存    │
│ .tim/.doc/   │            │ TermDict /        │
│ .pos/.dvd    │            │ PostingBuf /      │
│              │            │ PointsBuf         │
└──────────────┘            └──────────────────┘
```

## 3. 通用接口：LeafReader trait

文件位置：`crates/core/src/memory_reader.rs`

```rust
pub trait LeafReader {
    fn max_doc(&self) -> i32;

    // 倒排索引
    fn seek_term(&self, field: &str, term: &[u8]) -> io::Result<Option<TermMeta>>;
    fn postings_docs(&self, field: &str, term: &[u8]) -> io::Result<Vec<u32>>;
    fn postings_docs_freqs(&self, field: &str, term: &[u8]) -> io::Result<Vec<(u32, u32)>>;

    // Term 枚举（prefix/wildcard 用）
    fn terms_in_field(&self, field: &str) -> io::Result<Vec<Vec<u8>>>;

    // 数值范围（Point/BKD）
    fn point_range_docs(&self, field: &str, low: i64, high: i64) -> io::Result<Vec<u32>>;

    // 位置信息（Phrase 用）
    fn field_has_positions(&self, field: &str) -> bool;
    fn positions_for_doc(&self, field: &str, term: &[u8], doc: u32) -> io::Result<Vec<u32>>;

    // Stored Fields
    fn stored_value(&self, doc: u32, field: &str) -> Option<FieldValue>;
}
```

### 3.1 TermMeta

```rust
pub struct TermMeta {
    pub has_freqs: bool,       // 字段是否索引了词频
    pub doc_freq: u32,         // 包含该 term 的文档数
    pub total_term_freq: u64,  // 该 term 在所有文档中的总出现次数
}
```

## 4. 内存实现：MemoryLeafReader

### 4.1 数据来源

直接借用 `DocWriter` 的内部结构，零拷贝：

| 数据 | 来源 | 结构 |
|------|------|------|
| 倒排索引 | `FieldBuf.dict: TermDict` | 开放寻址哈希表 + arena 字节存储 |
| Postings | `TermRec.postings: PostingBuf` | `docs: Vec<u32>` + `freqs: Vec<u32>` + `positions: Vec<Vec<u32>>` |
| Points | `FieldBuf.points: PointsBuf` | `points: Vec<(i64, u32)>` (value, doc_id) |
| Stored | 外部传入 `&[Document]` | 按 doc_id 下标直接寻址 |

### 4.2 各操作的内存实现

**seek_term**:
```
field name → schema.get() 确认 indexed
           → DocWriter.fields() 线性查找 field_number
           → FieldBuf.dict.find(term_bytes)  // O(1) 哈希查找
           → PostingBuf.docs.len() = doc_freq
           → PostingBuf.freqs.sum() = total_term_freq
```

**postings_docs**:
```
同上定位到 PostingBuf → 直接 clone docs: Vec<u32>（已升序）
```

**terms_in_field**:
```
FieldBuf.dict.sorted_ids() → 按字节序排序的 term ID 列表
→ 逐个 dict.bytes_of(id) 取出 term 字节
```

**point_range_docs**:
```
FieldBuf.points.points 线性扫描
→ filter(value >= low && value <= high)
→ 收集 doc_id → sort + dedup
```
注：内存态数据量有限（flush 前），线性扫描足够；
若需优化可后续加排序数组 + 二分。

**positions_for_doc**:
```
PostingBuf.docs.binary_search(doc) → idx
→ PostingBuf.positions[idx]  // 已排序的位置列表
```

**stored_value**:
```
stored[doc_id].fields.find(name) → FieldValue
```

## 5. 查询执行引擎：MemorySearcher

### 5.1 执行模型

与磁盘 Searcher 的迭代器模型不同，内存搜索采用 **集合物化** 模型：
每个查询返回一个 `Vec<u32>`（升序 doc ID 集合），组合查询通过集合运算完成。

原因：
- 内存数据量有限（flush 阈值前），物化成本可控
- 集合运算（交/并/差）实现简单、正确性易验证
- 避免了迭代器生命周期与借用检查的复杂性

### 5.2 各查询类型的执行流程

**Term**:
```
postings_docs(field, term) → Vec<u32>
```

**MatchAll**:
```
(0..max_doc).collect()
```

**And (同字段多 term 交集)**:
```
for each term: postings_docs → sets[]
sets.sort_by_key(len)  // 最小集优先
result = sets[0]
for set in sets[1..]:
    result = intersect_sorted(result, set)
    if empty: break
```

**Or / Terms (同字段多 term 并集)**:
```
result = []
for each term:
    docs = postings_docs(term)
    result = union_sorted(result, docs)
```

**Prefix**:
```
all_terms = terms_in_field(field)  // 已排序
for term in all_terms:
    if term.starts_with(prefix):
        result = union_sorted(result, postings_docs(term))
    // 优化：排序后可在 prefix 不匹配时 break
```

**Wildcard**:
```
pattern = WildcardPattern::parse(pattern_bytes)
all_terms = terms_in_field(field)
for term in all_terms:
    if pattern.matches(term):
        result = union_sorted(result, postings_docs(term))
```

**Phrase (slop=0)**:
```
// 1. 候选集 = 所有 term 的交集
candidates = intersect(postings_docs(t) for t in terms)

// 2. 位置验证
for doc in candidates:
    pos_lists = [positions_for_doc(field, t, doc) for t in terms]
    // 检查是否存在 p0 ∈ pos_lists[0] 使得
    // 对所有 i: (p0 + i) ∈ pos_lists[i]
    if phrase_matches: emit doc
```

**PointRange**:
```
point_range_docs(field, low, high) → Vec<u32>
```

**Bool (MUST/SHOULD/MUST_NOT)**:
```
// 分派
for (occur, sub_query) in clauses:
    docs = exec_query(sub_query)
    match occur:
        Must    → musts.push(docs)   // 空则整体空
        Should  → shoulds.push(docs)
        MustNot → nots.push(docs)

// 正集
positive = if musts: fold(intersect, musts)
           elif shoulds: fold(union, shoulds)
           elif has_must_not: MatchAll
           else: empty

// 排除
if nots:
    prohibited = fold(union, nots)
    positive = difference(positive, prohibited)
```

### 5.3 集合运算

三个核心操作，均基于升序数组的双指针归并：

- `intersect_sorted(a, b)` — O(n+m)，交集
- `union_sorted(a, b)` — O(n+m)，并集（去重）
- `difference_sorted(a, b)` — O(n+m)，a 减 b

## 6. 写入即可搜索的流程

```rust
use rustlucene_core::doc_writer::DocWriter;
use rustlucene_core::memory_reader::MemorySearcher;
use rustlucene_core::search::Query;
use rustlucene_core::{Document, FieldValue, FieldSpec, Schema};

// 1. 定义 schema
let mut schema = Schema::new();
schema.add(FieldSpec::keyword("level"));
schema.add(FieldSpec::text_with_positions("message"));
schema.add(FieldSpec::long_point("ts"));

// 2. 创建内存写入器
let mut dw = DocWriter::new();
let mut stored: Vec<Document> = Vec::new();

// 3. 写入文档
let mut doc = Document::new();
doc.add("level", FieldValue::Keyword("INFO".into()));
doc.add("message", FieldValue::Text("hello world".into()));
doc.add("ts", FieldValue::Long(1722124800));
dw.add_document(&schema, doc.clone(), None).unwrap();
stored.push(doc);

// 4. 立即可搜索（无需 flush/commit）
let searcher = MemorySearcher::new(&dw, &schema, &stored);
assert_eq!(searcher.count(&Query::term("level", "INFO")).unwrap(), 1);
assert_eq!(searcher.count(&Query::term("message", "hello")).unwrap(), 1);

// 5. 继续写入，搜索自动包含新文档
let mut doc2 = Document::new();
doc2.add("level", FieldValue::Keyword("ERROR".into()));
doc2.add("message", FieldValue::Text("hello error".into()));
doc2.add("ts", FieldValue::Long(1722124900));
dw.add_document(&schema, doc2.clone(), None).unwrap();
stored.push(doc2);

let searcher = MemorySearcher::new(&dw, &schema, &stored);
assert_eq!(searcher.count(&Query::term("message", "hello")).unwrap(), 2);
assert_eq!(searcher.count(&Query::point_range("ts", 1722124850, 1722125000)).unwrap(), 1);
```

## 7. 与磁盘路径的对比

| 维度 | 磁盘 Searcher | 内存 MemorySearcher |
|------|--------------|-------------------|
| 数据可见性 | commit 后 | 写入后立刻 |
| 读取方式 | 文件 IO + 缓存 | 直接内存访问 |
| 执行模型 | 迭代器（流式） | 集合物化（批量） |
| Term 查找 | FST seek | 哈希表 O(1) |
| Postings | PFOR 解码 / Roaring | Vec<u32> 直接读 |
| Points | BKD 树遍历 | 线性扫描 + filter |
| Phrase | PositionsEnum 流式 | positions[idx] 直接读 |
| Stored Fields | 压缩块解码 | Document 数组下标 |
| 适用场景 | 大规模持久化索引 | flush 前的实时缓冲 |

## 8. 后续扩展方向

1. **SegmentReader 实现 LeafReader** — 让磁盘路径也走统一接口，
   Searcher 可以混合内存 leaf + 磁盘 leaf（NRT 场景）。

2. **列存（DocValues）** — 在 LeafReader 中增加：
   ```rust
   fn numeric_dv(&self, field: &str, doc: u32) -> Option<i64>;
   fn sorted_dv(&self, field: &str, doc: u32) -> Option<&[u8]>;
   ```
   内存实现直接读 NumericDvBuf / SortedDvBuf。

3. **并发安全** — 当前 MemorySearcher 借用 &DocWriter（只读），
   写入和搜索在不同线程时需要 RwLock 或 snapshot 机制。

4. **Points 优化** — 数据量增大后可在内存中维护排序数组，
   point_range 用二分查找代替线性扫描。

5. **Composite Reader** — 多个内存 leaf（多次 flush 间）+ 磁盘 segment
   组合成统一视图，docBase 映射与现有 Reader 一致。

## 9. 文件清单

```
crates/core/src/memory_reader.rs   — 本方案的全部实现
crates/core/src/lib.rs             — 新增 pub mod memory_reader
```

测试：12 个单元测试覆盖全部查询类型 + 写入即可搜索语义。
全量测试：99 passed, 0 failed（无回归）。
