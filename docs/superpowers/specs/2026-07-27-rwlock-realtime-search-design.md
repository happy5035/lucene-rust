# RwLock 实时搜索设计

## 1. 设计目标

改造 IndexWriter，使其内部的 DocWriter（未 flush 数据）可被多线程并发搜索。
Java 通过 JNI 在同一个 RustIndexWriter 对象上完成写入和搜索，无需 flush/commit 即可查到最新数据。

## 2. 约束与决策

| 决策项 | 选择 | 理由 |
|--------|------|------|
| 并发模型 | `Arc<RwLock<IndexWriter>>` | 查询低频（<100 QPS）、耗时短（~1ms），RwLock 最简单 |
| 架构位置 | 改造 IndexWriter 本身 | 保留 flush/commit 能力，统一视图天然成立 |
| Stored Fields | 实时读 .fdt 磁盘文件 | 不在内存保留 Vec<Document>，内存零开销 |
| 搜索范围 | 统一视图：内存 + 磁盘 segment | 一次搜索返回全部数据 |
| JNI 接口 | 搜索方法加在 RustIndexWriter 上 | 一个对象搞定写入+搜索 |
| 返回格式 | doc_id + count，按需 nativeDocument(docId) | 灵活，不是每次都需要原文 |
| 查询传递 | JSON byte[] | 灵活，支持所有查询类型 |
| 排序 | 只按时间（Numeric long）单字段 | 场景够用 |

## 3. 整体架构

```
Java Thread (writer)                    Java Threads (readers)
─────────────────                       ──────────────────────
nativeEndDocument()                     nativeSearch(jsonBytes)
nativeAddJsonBatch()                    nativeDocument(docId)
        │                                       │
        ▼                                       ▼
┌─────────────────────────────────────────────────────────┐
│              Arc<RwLock<IndexWriter>>                     │
│                                                          │
│  write_lock (~10μs)             read_lock (~1ms)         │
│  ┌──────────────┐              ┌───────────────────┐    │
│  │ add_document │              │ Unified Search     │    │
│  │ → DocWriter  │              │                    │    │
│  │ → SFW (.fdt) │              │ ┌───────────────┐ │    │
│  └──────────────┘              │ │ Memory Leaf    │ │    │
│                                │ │ (DocWriter)    │ │    │
│                                │ └───────────────┘ │    │
│                                │ ┌───────────────┐ │    │
│                                │ │ Disk Segments  │ │    │
│                                │ │ (SegmentReader)│ │    │
│                                │ └───────────────┘ │    │
│                                │   merge + sort    │    │
│                                └───────────────────┘    │
│                                                          │
│  nativeDocument(docId):                                  │
│  ┌─────────────────────────────────────────────────┐    │
│  │ 锁内（~100ns）：取定位信息                        │    │
│  │ 锁外（无锁）：读 .fdt 或 SFW buffer              │    │
│  └─────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘
```

## 4. IndexWriter 改造

### 4.1 方法分类

| 方法 | 借用 | 锁 | 说明 |
|------|------|-----|------|
| `add_document()` | `&mut self` | write_lock | 写入 DocWriter + SFW |
| `flush()` | `&mut self` | write_lock | 冻结 buffer → 磁盘 segment |
| `commit()` | `&mut self` | write_lock | flush + fsync + segments_N |
| `search()` | `&self` | read_lock | 统一搜索（新增） |
| `document_location()` | `&self` | read_lock | 取文档定位（新增） |

### 4.2 search() 实现

```rust
pub fn search(&self, query: &Query, sort: Option<(&str, bool)>, top_n: usize)
    -> io::Result<SearchResults>
{
    let mut heap = SortedTopN::new(sort, top_n);

    // 1. 磁盘 segments（已 commit）
    let mut doc_base = 0;
    for seg in &self.infos.segments {
        let reader = SegmentReader::open(&self.dir, seg)?;
        let local_docs = reader.search(query)?;
        for local_id in local_docs {
            let global_id = doc_base + local_id;
            let sort_val = match sort {
                Some((field, _)) => reader.numeric_dv(field_number, local_id).unwrap_or(i64::MIN),
                None => 0,
            };
            heap.collect(global_id, sort_val);
        }
        doc_base += seg.max_doc;
    }

    // 2. 内存 buffer（未 flush）
    if let Some(builder) = &self.builder {
        let mem = MemoryLeafReader::new(&builder.dw, &self.schema);
        let local_docs = mem.exec_query(query)?;
        for local_id in local_docs {
            let global_id = doc_base + local_id as i32;
            let sort_val = match sort {
                Some((field, _)) => builder.dw.numeric_dv(field, local_id).unwrap_or(i64::MIN),
                None => 0,
            };
            heap.collect(global_id, sort_val);
        }
    }

    Ok(heap.results())
}
```

### 4.3 doc_id 全局编号

```
全局 doc_id 空间：

┌─────────────────────────────────────────────────────────┐
│ infos.segments（flush 后即注册，search 可见）             │
│ seg_0: [0..999]  seg_1: [1000..2499]  seg_2: [2500..3999]│
├─────────────────────────────────────────────────────────┤
│ in-memory buffer (builder.dw，未 flush)                  │
│ [4000..4123]                                            │
└─────────────────────────────────────────────────────────┘
```

buffer_doc_base = sum(infos.segments 所有 segment 的 max_doc)。
flush() 将 buffer 转为新 segment 并 push 到 infos.segments，buffer 清空重建。

### 4.4 flush 交互

flush() 在 write_lock 内执行：
1. `builder.take()` → DocWriter + SFW 被移出
2. `finalize()` → 写 .tim/.doc/.pos/.dvd/.fdt 到磁盘
3. `infos.segments.push(new_seg)` → 注册新 segment
4. `builder = None` → 下次 add_document 时创建新 builder

无数据丢失窗口：旧 buffer 消失 = 新 segment 出现，在同一把锁内原子切换。
Reader 被 write_lock 阻塞 10-50ms（flush 频率极低，每 128MB 一次）。

## 5. StoredFieldsLiveReader（无锁读 .fdt）

### 5.1 设计原则

- `.fdt` 是 append-only 文件，已刷盘的 chunk 永远不会被修改
- 读已刷盘数据 = 读不可变文件，天然线程安全，不需要加锁
- 锁内只取定位信息（~100ns），文件 IO 在锁外完成（~50μs）

### 5.2 读取路径

```
nativeDocument(docId):

  1. read_lock（极短，~100ns）：
     ├─ 判断 docId 落在哪个 segment / buffer
     ├─ 已刷盘 → 取 chunk_index 中对应的 filePointer
     └─ 未刷盘 → clone SFW buffer 中对应 doc 的字节（几 KB）
     释放 read_lock

  2. 无锁：
     ├─ 已刷盘 → seek .fdt + LZ4 解压（~50μs）
     └─ 未刷盘 → 解码内存字节（~1μs）
```

### 5.3 StoredFieldsWriter 新增只读接口

```rust
impl StoredFieldsWriter {
    /// 已刷盘 doc 数
    pub fn flushed_doc_count(&self) -> i32;

    /// 根据 doc_id 获取 chunk 的 file pointer（用于磁盘定位）
    pub fn chunk_file_pointer(&self, doc_id: u32) -> u64;

    /// 未刷盘 buffer 中第 n 个 doc 的原始字节（用于内存解码）
    pub fn buffered_doc_bytes(&self, n: u32) -> &[u8];
}
```

### 5.4 document_location 反向映射

```rust
pub fn document_location(&self, global_id: u32) -> DocLocation {
    let mut base = 0;
    for seg in &self.infos.segments {
        if global_id < base + seg.max_doc as u32 {
            return DocLocation::CommittedSegment { seg: seg.clone(), local_id: global_id - base };
        }
        base += seg.max_doc as u32;
    }
    DocLocation::Buffer { local_id: global_id - base }
}
```

## 6. DocValues 读取 + 排序

### 6.1 内存侧

```rust
impl DocWriter {
    pub fn numeric_dv(&self, field: &str, doc: u32) -> Option<i64> {
        let buf = self.field_buf(field)?;
        let dv = buf.numeric_dv.as_ref()?;
        let idx = dv.docs.binary_search(&doc).ok()?;
        Some(dv.values[idx])
    }
}
```

### 6.2 磁盘侧

SegmentReader 新增 DocValuesReader 字段：

```rust
pub struct SegmentReader {
    max_doc: i32,
    field_infos: FieldInfos,
    terms: TermsDict,
    postings: PostingsReader,
    points: Option<PointsReader>,
    doc_values: Option<DocValuesReader>,  // 新增
}

impl SegmentReader {
    pub fn numeric_dv(&self, field_number: u32, doc: u32) -> Option<i64>;
}
```

DocValuesReader 已存在于 `codec-lucene9/src/doc_values_read.rs`，接入 SegmentReader 即可。

### 6.3 排序收集器

```rust
pub struct SortedTopN {
    desc: bool,
    n: usize,
    heap: BinaryHeap<(i64, i32)>,  // (sort_value, global_doc_id)
    total: u64,
}

impl SortedTopN {
    pub fn new(sort: Option<(&str, bool)>, n: usize) -> Self;
    pub fn collect(&mut self, doc: i32, sort_value: i64);
    pub fn results(self) -> SearchResults;  // (total, Vec<i32>)
}
```

- 缺失值：`sort_value = i64::MIN`（desc 时排最后）
- 无 sort 时：跳过 DV 读取，INDEXORDER，可提前终止

## 7. JNI 层改造

### 7.1 WriterHandle 改为 RwLock

```rust
struct WriterHandle {
    index: Arc<RwLock<IndexWriter>>,
    current: Option<Document>,
    binder: JsonBinder,
}
```

### 7.2 新增 JNI 方法

| 方法 | 签名 | 说明 |
|------|------|------|
| `nativeSearch` | `(long handle, byte[] queryJson) → byte[]` | 统一搜索，返回 JSON `{total, docs}` |
| `nativeDocument` | `(long handle, int docId) → byte[]` | 取原始文档，返回 JSON 字段 |

### 7.3 JSON 查询格式

```json
{"query": {"type":"term", "field":"level", "value":"ERROR"}, "sort": {"field":"ts","order":"desc"}, "top_n": 100}

{"query": {"type":"bool", "clauses":[
    {"occur":"must", "query":{"type":"term","field":"level","value":"ERROR"}},
    {"occur":"must", "query":{"type":"range","field":"ts","low":1722124800,"high":1722125000}},
    {"occur":"must_not", "query":{"type":"term","field":"host","value":"test-01"}}
]}, "sort": {"field":"ts", "order":"desc"}, "top_n": 50}

{"query": {"type":"prefix", "field":"path", "value":"/api/v2"}}
{"query": {"type":"wildcard", "field":"tid", "value":"req-*"}}
{"query": {"type":"phrase", "field":"message", "terms":["connection","timeout"]}}
{"query": {"type":"match_all"}, "sort": {"field":"ts","order":"desc"}, "top_n": 10}
```

### 7.4 Rust 侧解析

```rust
#[derive(Deserialize)]
pub struct SearchRequest {
    pub query: QuerySpec,
    pub sort: Option<SortSpec>,
    #[serde(default = "default_top_n")]
    pub top_n: usize,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuerySpec {
    Term { field: String, value: String },
    Bool { clauses: Vec<ClauseSpec> },
    Range { field: String, low: i64, high: i64 },
    Prefix { field: String, value: String },
    Wildcard { field: String, value: String },
    Phrase { field: String, terms: Vec<String> },
    MatchAll,
}

#[derive(Deserialize)]
pub struct SortSpec {
    pub field: String,
    #[serde(default = "default_desc")]
    pub order: String,
}
```

## 8. 文件清单

```
crates/core/src/
├── index_writer.rs          ← 修改：新增 search() / document_location() / &self 方法
├── memory_reader.rs         ← 修改：MemoryLeafReader 去掉 stored 依赖，扩展 DV 读取
├── stored_live_reader.rs    ← 新增：StoredFieldsLiveReader（无锁读 .fdt）
├── search/
│   ├── sorted_collector.rs  ← 新增：SortedTopN 堆排收集器
│   ├── segment_reader.rs    ← 修改：接入 DocValuesReader
│   └── mod.rs               ← 修改：导出新类型
├── lib.rs                   ← 修改：pub mod stored_live_reader

crates/codec-lucene9/src/
├── stored_fields.rs         ← 修改：SFW 暴露 chunk_index() / buffered_doc_bytes()

crates/jni-binding/src/
├── lib.rs                   ← 修改：Mutex → RwLock，新增 nativeSearch / nativeDocument
├── query_parser.rs          ← 新增：JSON → Query 解析（serde）
```

## 9. 测试策略

| 层级 | 测试内容 | 方式 |
|------|---------|------|
| 单元测试 | SortedTopN 堆排正确性（asc/desc/missing） | #[test] |
| 单元测试 | StoredFieldsLiveReader 跨 flushed/buffer 边界读取 | #[test] |
| 单元测试 | JSON 查询解析（各 type + 畸形输入） | #[test] |
| 单元测试 | DocWriter.numeric_dv() 二分查找 | #[test] |
| 集成测试 | IndexWriter.search() 统一视图：写入→搜索→flush→搜索（结果一致） | #[test] |
| 集成测试 | 并发：1 writer + 4 reader 线程，RwLock 下无 panic/死锁 | #[test] |
| 集成测试 | flush 期间搜索阻塞后恢复，doc_id 连续无丢失 | #[test] |
| 集成测试 | nativeDocument 跨 segment/buffer 取原文正确 | #[test] |
| 回归 | 现有 99 个测试全绿 | cargo test |

## 10. 性能预期

| 操作 | 延迟 | 说明 |
|------|------|------|
| 写入（add_document） | ~10μs + 锁获取 ~20ns | write_lock 无争用（单写者） |
| 搜索（无 sort） | ~1ms | read_lock，内存集合物化 |
| 搜索（有 sort） | ~1-2ms | 额外 DV 二分查找 |
| 取文档（已刷盘） | ~50μs | 无锁，seek + LZ4 解压 |
| 取文档（未刷盘） | ~1μs | 无锁，内存解码 |
| 写入被 reader 阻塞概率 | ~5%（100 QPS × 1ms） | 期望等待 ~25μs |
| Flush 阻塞 reader | 10-50ms，频率极低 | 每 128MB 一次 |
