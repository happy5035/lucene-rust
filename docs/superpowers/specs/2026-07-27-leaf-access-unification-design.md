# 统一 LeafAccess Trait 设计——消除内存搜索的查询逻辑重复

## 1. 问题

当前 `memory_reader.rs` 的 `MemorySearcher::exec_query()` 把 `search/query.rs` 的查询执行逻辑重新实现了一遍（~200 行）：Term/And/Or/Bool/Phrase/Prefix/Wildcard/PointRange 全部用物化集合运算（intersect_sorted/union_sorted/difference_sorted）重写。磁盘路径的迭代器引擎（两阶段确认、提前终止、堆化 OR、roaring 三档、fast_segment_count 快路径）完全没复用。

新增查询类型或修改执行语义时，必须同步改两处，且两处行为可能不一致。

## 2. 设计目标

- Writer 的内存 buffer 抽象为 reader 接口，直接复用 `Query::segment_iterator` 执行引擎
- 内存侧只负责提供数据（postings、positions、terms、points、DV），不包含任何查询组合逻辑
- 未来新增数据通道（远程 segment、mmap 直读等）只需实现 trait，query 引擎零改动

## 3. 核心决策

| 决策项 | 选择 | 理由 |
|--------|------|------|
| 抽象方式 | Trait（`LeafAccess`） | 新增数据源 O(1)，零改 query.rs |
| 方法签名 | `&mut self` | 与 SegmentReader 一致；局部对象可 mut，不影响并发 |
| TermHandle | 关联类型 | 零运行时开销，磁盘=TermEntry，内存=MemTermHandle |
| Postings 返回 | 统一为 `SegmentDocIter` enum | 改动最小，新增 MemDocs/MemFreqs 变体 |
| Phrase 位置 | MemPositionsEnum 适配 PositionsEnum 接口 | 复用现有 PhraseDocIter 两阶段逻辑，不新增变体 |
| Bitmap | 内存侧 `open_term_bitmap` 永远返回 None | 自动走 tier 3（Vec 迭代器），无 roaring |
| 替换策略 | 一步到位删除旧 exec_query | 干净，不留两套代码 |

## 4. LeafAccess Trait 定义

```rust
pub trait LeafAccess {
    type TermHandle;

    fn max_doc(&self) -> i32;

    fn seek_term(&mut self, field: &str, term: &[u8])
        -> io::Result<Option<(bool, Self::TermHandle)>>;

    fn docs_enum(&self, entry: &Self::TermHandle)
        -> io::Result<SegmentDocIter>;

    fn docs_freqs_enum(&self, entry: &Self::TermHandle, needs_freq: bool)
        -> io::Result<SegmentDocIter>;

    fn positions_enum(&self, entry: &Self::TermHandle)
        -> io::Result<SegmentDocIter>;

    fn open_term_bitmap(&self, entry: &Self::TermHandle)
        -> io::Result<Option<FrozenBitmap>>;

    fn field_info(&self, name: &str) -> Option<&FieldInfo>;

    fn field_has_freqs(&self, field: &str) -> Option<bool>;

    fn terms_iter(&mut self, field: &str) -> Option<Box<dyn TermsIterAccess + '_>>;

    fn points_reader(&self) -> Option<&dyn PointsAccess>;

    fn numeric_dv(&self, field: &str, doc: u32) -> Option<i64>;
}
```

辅助 trait：

```rust
/// 统一 term 枚举接口（磁盘 FST 流式 / 内存排序数组）
pub trait TermsIterAccess {
    fn seek_ceil(&mut self, target: &[u8]) -> io::Result<bool>;
    fn next(&mut self) -> io::Result<Option<(Vec<u8>, TermEntryLike)>>;
}

/// 统一 points 接口（磁盘 BKD / 内存线性扫描）
pub trait PointsAccess {
    fn intersect(&self, field: &str, low: i64, high: i64,
                 visitor: &mut dyn FnMut(i64, i32)) -> io::Result<()>;
}
```

`TermEntryLike`：轻量结构，包含 `doc_freq: u32`、`total_term_freq: u64` 和内部句柄。磁盘侧包装 `TermEntry`，内存侧包装 `term_id: u32`。

## 5. SegmentDocIter 新增变体

```rust
pub enum SegmentDocIter {
    // 现有 14 个变体不动
    Docs(DocsEnum),
    Freqs(DocsFreqsEnum),
    All(MatchAllIter),
    And(ConjunctionDocIter),
    Or(DisjunctionDocIter),
    Bitset(BitsetDocIter),
    Phrase(PhraseDocIter),
    Roaring(RoaringDocIter),
    RoaringAnd(RoaringAndDocIter),
    RoaringOr(RoaringOrDocIter),
    Points(PointsDocIter),
    ConjOver(ConjOverDocIter),
    DisjOver(DisjOverDocIter),
    Excluding(ExcludingDocIter),

    // 新增：内存叶节点
    MemDocs(MemDocsIter),
    MemFreqs(MemFreqsIter),
}
```

Phrase 不新增变体——内存侧 `positions_enum` 返回 `MemPositionsEnum`（适配 PositionsEnum 接口），包装进现有 `PhraseDocIter`。

## 6. MemoryLeafAccess 实现

```rust
pub struct MemoryLeafAccess<'a> {
    dw: &'a DocWriter,
    schema: &'a Schema,
    field_infos: FieldInfos,
}

pub struct MemTermHandle {
    pub term_id: u32,
    pub doc_freq: u32,
    pub total_term_freq: u64,
}
```

各方法实现：

| 方法 | 内存实现 |
|------|---------|
| `seek_term` | `dict.find(term)` → term_id → PostingBuf → MemTermHandle |
| `docs_enum` | `PostingBuf.docs.clone()` → `MemDocsIter` → `SegmentDocIter::MemDocs` |
| `docs_freqs_enum` | needs_freq → `MemFreqsIter`；否则 → `MemDocsIter` |
| `positions_enum` | `PostingBuf.positions` → `MemPositionsEnum` → `PhraseDocIter` → `SegmentDocIter::Phrase` |
| `open_term_bitmap` | `Ok(None)` |
| `field_info` | `self.field_infos.by_name(name)` |
| `field_has_freqs` | 从 FieldInfo.index_options 判断 |
| `terms_iter` | `dict.sorted_ids()` → `MemTermsIter`（排序数组 + 游标） |
| `points_reader` | `MemoryPointsAccess`（线性扫描 PointsBuf） |
| `numeric_dv` | `DocWriter::numeric_dv(field, doc)` |

### MemTermsIter

```rust
struct MemTermsIter<'a> {
    dict: &'a TermDict,
    sorted_ids: Vec<u32>,
    pos: usize,
}

impl TermsIterAccess for MemTermsIter<'_> {
    fn seek_ceil(&mut self, target: &[u8]) -> io::Result<bool> {
        // 二分查找 sorted_ids 中第一个 bytes >= target 的位置
    }
    fn next(&mut self) -> io::Result<Option<(Vec<u8>, TermEntryLike)>> {
        // 返回当前 term 字节 + TermEntryLike
    }
}
```

### MemoryPointsAccess

```rust
struct MemoryPointsAccess<'a> {
    dw: &'a DocWriter,
}

impl PointsAccess for MemoryPointsAccess<'_> {
    fn intersect(&self, field: &str, low: i64, high: i64,
                 visitor: &mut dyn FnMut(i64, i32)) -> io::Result<()> {
        // 线性扫描 PointsBuf.points，filter(v >= low && v <= high)
    }
}
```

### MemPositionsEnum

```rust
struct MemPositionsEnum {
    positions: Vec<Vec<u32>>,  // per-doc positions（从 PostingBuf.positions clone）
    doc_idx: usize,
    pos_idx: usize,
}
// 实现 PositionsEnum 接口（next_doc / next_position / freq）
// 包装进 PhraseDocIter 复用两阶段确认逻辑
```

## 7. query.rs 泛型化

```rust
// 之前：
pub(crate) fn segment_iterator(&self, seg: &mut SegmentReader, needs_freq: bool)
    -> io::Result<Option<SegmentDocIter>>

// 之后：
pub(crate) fn segment_iterator<L: LeafAccess>(&self, seg: &mut L, needs_freq: bool)
    -> io::Result<Option<SegmentDocIter>>
```

`fast_segment_count` 同理：

```rust
pub(crate) fn fast_segment_count<L: LeafAccess>(seg: &mut L, query: &Query)
    -> io::Result<Option<u64>>
```

`multi_term.rs`、`roaring_exec.rs` 中接收 `&mut SegmentReader` 的辅助函数也改为 `<L: LeafAccess>`。

内部执行逻辑零改动——所有方法调用走 trait 分派。

## 8. SegmentReader 实现 LeafAccess

```rust
impl LeafAccess for SegmentReader {
    type TermHandle = TermEntry;

    fn max_doc(&self) -> i32 { self.max_doc }
    fn seek_term(&mut self, field, term) -> ... { self.seek_term(field, term) }
    fn docs_enum(&self, entry) -> ... { Ok(SegmentDocIter::Docs(self.postings.docs(entry)?)) }
    fn docs_freqs_enum(&self, entry, needs_freq) -> ... { /* 现有逻辑 */ }
    fn positions_enum(&self, entry) -> ... { /* 现有逻辑 */ }
    fn open_term_bitmap(&self, entry) -> ... { /* 现有逻辑 */ }
    fn field_info(&self, name) -> ... { self.field_infos.by_name(name) }
    fn field_has_freqs(&self, field) -> ... { /* 现有逻辑 */ }
    fn terms_iter(&mut self, field) -> ... { /* 包装为 Box<dyn TermsIterAccess> */ }
    fn points_reader(&self) -> ... { /* 包装为 &dyn PointsAccess */ }
    fn numeric_dv(&self, field, doc) -> ... { /* 现有 DocValuesReader 逻辑 */ }
}
```

## 9. IndexWriter::search() 统一驱动

```rust
pub fn search(&self, query: &Query, sort_field: Option<(&str, bool)>, top_n: usize)
    -> io::Result<SearchResults>
{
    let mut collector = SortedTopN::new(desc, n);

    // 1. 磁盘 segments
    let mut doc_base: i32 = 0;
    for sci in &self.infos.segments {
        let mut reader = SegmentReader::open(&self.dir, sci)?;
        Self::drive_segment(&mut reader, query, &mut collector, doc_base, sort_field)?;
        doc_base += sci.info.doc_count;
    }

    // 2. 内存 buffer（同一条路径）
    if let Some(builder) = &self.builder {
        let mut mem = MemoryLeafAccess::new(builder.doc_writer(), &self.schema);
        Self::drive_segment(&mut mem, query, &mut collector, doc_base, sort_field)?;
    }

    Ok(collector.results())
}

fn drive_segment<L: LeafAccess>(
    seg: &mut L,
    query: &Query,
    collector: &mut SortedTopN,
    doc_base: i32,
    sort_field: Option<(&str, bool)>,
) -> io::Result<()> {
    if let Some(mut iter) = query.segment_iterator(seg, false)? {
        loop {
            let doc = iter.next_doc()?;
            if doc == NO_MORE_DOCS { break; }
            if !iter.matches()? { continue; }
            let global_id = doc_base + doc;
            let sv = match sort_field {
                Some((field, _)) => seg.numeric_dv(field, doc as u32).unwrap_or(i64::MIN),
                None => 0,
            };
            collector.collect(global_id, sv);
        }
    }
    Ok(())
}
```

## 10. 删除的代码

- `MemorySearcher::exec_query()` 及全部物化集合运算
- `intersect_sorted` / `union_sorted` / `difference_sorted`
- `MemorySearcher` 的 `count()` / `top_docs()` / `search()` / `freq_sum()`
- `LeafReader` trait（被 `LeafAccess` 取代）
- `WildcardPattern`（multi_term.rs 已有等价实现）
- `MemoryLeafReader`（被 `MemoryLeafAccess` 取代）

## 11. 保留/移动的代码

- `MemDocsIter` / `MemFreqsIter`：从 memory_reader.rs 移入 doc_iter.rs，接入 SegmentDocIter enum
- `MemorySearcher` 的 12 个单元测试：改为通过 `IndexWriter::search()` 驱动

## 12. 文件清单

```
crates/core/src/search/
├── leaf_access.rs       ← 新增：LeafAccess trait + TermsIterAccess + PointsAccess + TermEntryLike
├── doc_iter.rs          ← 修改：新增 MemDocs/MemFreqs 变体 + MemDocsIter/MemFreqsIter 移入
├── query.rs             ← 修改：segment_iterator / fast_segment_count 泛型化
├── multi_term.rs        ← 修改：辅助函数泛型化
├── roaring_exec.rs      ← 修改：辅助函数泛型化
├── segment_reader.rs    ← 修改：impl LeafAccess for SegmentReader
├── searcher.rs          ← 修改：drive 循环改用 LeafAccess 泛型
├── mod.rs               ← 修改：pub mod leaf_access

crates/core/src/
├── memory_access.rs     ← 新增：MemoryLeafAccess + MemTermsIter + MemoryPointsAccess + MemPositionsEnum
├── memory_reader.rs     ← 删除（或大幅瘦身为 re-export）
├── index_writer.rs      ← 修改：search() 用 drive_segment 统一驱动
├── lib.rs               ← 修改：pub mod memory_access，移除 pub mod memory_reader
```

## 13. 测试策略

| 测试 | 内容 |
|------|------|
| 等价性电池 | 同一组文档，新旧路径所有 Query 类型结果逐位一致 |
| 现有 12 个 memory_reader 测试 | 改为通过 IndexWriter::search() 驱动 |
| 现有 99+ 磁盘搜索测试 | 不动，验证 SegmentReader impl LeafAccess 无回归 |
| Phrase 两阶段 | 内存 positions 通过 MemPositionsEnum 适配后结果一致 |
| Prefix/Wildcard | MemTermsIter 的 seek_ceil/next 与旧枚举结果一致 |
| PointRange | MemoryPointsAccess 与旧 point_range_docs 结果一致 |
| fast_segment_count | 内存侧 Term→doc_freq、MatchAll→max_doc、PointRange→物化计数 |
| 并发测试 | 现有 concurrent 测试不动 |

## 14. 重构顺序

1. 实现 `LeafAccess` trait + `SegmentReader impl`（纯重构，行为不变）
2. query.rs / multi_term.rs / roaring_exec.rs 泛型化（编译通过 = 正确）
3. 实现 `MemoryLeafAccess` + MemDocsIter/MemFreqsIter 移入 doc_iter.rs
4. `IndexWriter::search()` 切换到统一 drive_segment
5. 删除旧 MemorySearcher 执行逻辑
6. 全量测试通过

## 15. 扩展性

未来新增数据通道只需：

```rust
impl LeafAccess for RemoteSegmentReader {
    type TermHandle = RemoteTermRef;
    // ... 实现 10 个方法
}
```

query 执行引擎、Bool 组合器、两阶段确认、fast_segment_count 全部自动可用。
