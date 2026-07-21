# RustLucene

用 Rust 重写的 **Lucene 9.12.3 格式兼容索引写入器**：追加写（append-only）链路，产出 Java Lucene 9.12.3 可读的段文件与 `segments_N` 提交点。读取 / 检索 / 段合并不在范围内——由 Java Lucene 担任读侧做校验（`CheckIndex` + 查询结果逐条 diff）。

全部格式细节以 `reference/lucene-9.12.3/` 源码为唯一事实来源，逐文件对照并注释出处（见 `docs/format-notes-*.md`）。

## 已实现功能

### 字段类型（日志场景全家桶）

| 字段能力 | 对应 Lucene 类型 | 说明 |
|---|---|---|
| 分词文本 | `TextField`（WhitespaceAnalyzer 等价） | `DOCS_AND_FREQS` + omitNorms，可选 positions（phrase 可查） |
| 关键词 | `StringField` | 整串一词，`DOCS` |
| 1D 数值点 | `LongPoint` / `IntPoint` | BKD 树，范围查询 |
| 数值列存 | `NumericDocValues` | 含稀疏字段（IndexedDISI 三分支：SPARSE / DENSE+跳表 / ALL） |
| 排序列存 | `SortedDocValues` | 64 项/块前缀压缩 terms dict + 裸 LZ4 |
| 存储字段 | `StoredField` | LZ4 BEST_SPEED，字符串 / int / long |

同一字段可组合多种能力（如 `timestamp = LongPoint + NumericDV + stored`），对应 Java 同名多字段语义。

### 格式层（`crates/codec-lucene9`）

- **stored fields**（Lucene90）：`.fdt/.fdx/.fdm`，LZ4 块压缩，DirectMonotonic 块地址索引
- **postings**（Lucene912）：`.tim/.tip/.tmd`（block-tree 多层 FST）+ `.doc/.psm/.pos`，FOR / PForDelta 块编码（collapse + 位平面交错，与 `ForUtil.java` 字面一致）、4096 边界两级跳表
- **DocValues**（Lucene90）：`.dvd/.dvm`，DirectWriter 数值块 + IndexedDISI docsWithField
- **Points**（Lucene90 BKD）：`.kdd/.kdi/.kdm`，512 点/叶，DocIdsWriter 五分支全实现
- **元数据**：field infos（Lucene94 `.fnm`）、segment info（`.si`）、`segments_N` 两段式提交（`pending_segments_N` → rename，提交前 fsync 全部段文件）
- CodecUtil 头/尾（魔数、版本、CRC32 footer）、VInt/GroupVInt、DirectWriter/DirectMonotonic 等底层编码

### 写入链路（`crates/core`）

- `Schema` / `FieldSpec`：声明式字段配置（`text` / `keyword` / `long_point` / `int_point` / `numeric_dv` / `sorted_dv` / `stored`，可链式组合）
- `IndexWriter`：`max_buffered_docs` + `max_ram_bytes`（默认 512MB，O(1) 增量 RAM 记账）双触发 flush
- 多线程：文档分片到多个私有 `SegmentBuilder`（无共享可变状态），`commit_segments` 把各分片的段并成一个提交点
- 段命名 / seg id / diagnostics 与 Java 一致；无 DV/points 字段时不产生对应文件

### 工具与集成

- CLI `rustlucene-cli`：`write` / `bench` / `index <文件或目录> [--positions] [--docs N]` / `logwrite` / `logbench` / `jsonindex <jsonlFile> <indexDir> <schemaSpec>` / `jsongen`
- JNI 绑定（`crates/jni-binding`，cdylib）：`RustIndexWriter` 供 Java 进程内调用；除逐字段 API 外提供**批量 JSON 写入** `addJsonBatch(byte[][])`——原始 JSON 字节整批一次 JNI 穿越，解析 / 强转 / 过滤 / 绑定全在 Rust 内闭环
- JSON 绑定层（`core/src/json.rs`）：schema spec 声明类型与索引配置（`name:type+mods[@json键]`、`$policy=` 指令），未知字段三策略：`strict`（过滤）/ `dynamic`（按值类型推断并自动注册，对齐 Lucene 动态字段语义）/ `stored-only`
- 互操作脚本：`interop/verify-index.sh`、`interop/verify-log.sh`、`interop/compare-index.sh`（同语料双侧建索引 + CheckIndex + term 级 diff，`--json` 模式为 rust / java-jni / java 三方对比）；`make interop-test` / `log-test` / `bench` / `log-bench` / `compare`

## 核心数据结构

| 结构 | 位置 | 设计 |
|---|---|---|
| `TermDict` | `doc_writer.rs` | 词典 = **arena（`Vec<u8>`）+ 开放寻址哈希表**：查找时原地哈希、零分配，词字节仅首次出现时拷入 arena；负载因子 0.75，扩容整体重哈希。对应 Lucene 的 `BytesRefHash` |
| `PostingBuf` | `doc_writer.rs` | 每词的 **docs / freqs / positions 平行数组**，doc 升序天然成立（追加即有序）；`add_occurrence` O(1) 区分"新 doc / 同 doc 词频 +1" |
| `hash_bytes` | `doc_writer.rs` | u64 块乘法哈希（wyhash 风）：短 token 约 2 次乘法，替代逐字节串行 FNV |
| `NumericDvBuf` / `SortedDvBuf` / `PointsBuf` | `doc_writer.rs` | 各 DV/points 的 RAM 缓冲：SortedDV = 插入序字典 + 每 doc term_id，flush 时按排序字典重映射 ord（对齐 `SortedDocValuesWriter`）；points 为 `(value, docID)` 数组，flush 时排序 |
| `StoredFieldsWriter` | `stored_fields.rs` | **流式直写**：add 时即序列化进当前压缩 chunk（Lucene `StoredFieldsConsumer` 模型），RAM 只有一个 chunk（~80KB）而非整个语料 |
| `FST` / block-tree | `fst.rs` + `postings.rs` | `.tim` 词典 + `.tip` FST 索引，flush 时全局排序后一次性构建 |
| BKD 树 | `points.rs` | 1D 全内存排序（等价 Java `writeField1Dim` 主路径），固定树形 + `recursePackIndex` 逐字节复刻 |
| `IndexedDISI` | `doc_values.rs` | 稀疏字段 doc 集合：≤4095 用 SPARSE 跳表，≤65535 用 DENSE rank+bitset，=65536 用 ALL |
| `DirectWriter` / `DirectMonotonic` / FOR / PForDelta | `packed.rs` / `postings_ll.rs` | Lucene 底层位编码，栈数组实现、无逐块分配 |

## 对比 Java 的优化

**零分配热路径**（对照 Java 每文档大量临时对象 + GC）：

- arena 词典 + 平行 postings 数组：索引循环内无逐 token 分配；仅剩语料 String 与数组摊销增长（malloc 占比 ~1.5%）
- `Document` 按值消费：owned 字符串直接 move 进 stored 流，去掉 200B/doc 的 clone
- stored fields 流式直写：去掉 Java 侧 DocValues/StoredFields 全量 RAM 中转（同口径单段缓冲下，Java 峰值内存是 Rust 的 **2.5–14.4 倍**）
- C liblz4（`LZ4_compress_fast_extState`，FAST(2)）替代 Java 侧纯 Java LZ4；单子块 `dict_length=0` 合法编码（读侧泛化处理，已实证）
- 字段号**线性查找**替代哈希表（schema 极小，几十个字段内线性扫描更快）
- ASCII 快路径分词（`split_ascii_whitespace` 语义等价 `WhitespaceTokenizer`）

**架构层面**：

- 无 JVM 启动 / JIT 预热 / GC 停顿：CPU 总耗时比 Java 低一个数量级（compare 场景实测）
- 多线程 = 文档分片 × 私有 builder，**无共享可变状态、无锁**，扩展近线性：8 线程在 4 物理核上自身扩 3.3x（Java 1.68x）
- 1D BKD 全内存排序：无需 Java 的 `OfflinePointWriter` / `BKDRadixSelector` 外排路径
- O(1)/doc RAM 增量记账触发 flush（Lucene 的 RAM 记账同样是近似值）
- `IndexWriter` 无 merge 调度、无读路径，单线程内聚：flush = 排序词典 + 顺序编码写盘

**两处与 Java 写法的合法偏差**（读取语义不变，CheckIndex 确认）：

1. stored fields 每 chunk 用 `dict_length=0` 单子块编码 → 写入侧单次整 chunk 压缩；代价仅 Java 读侧单文档取数时的读放大
2. LZ4 加速档 FAST(2)（任意合法 LZ4 流均可解压，压缩率损失可忽略）

## 性能（同语料双侧实测，4 核云主机，OpenJDK 21 / Rust 1.97，3 轮取优）

文本单字段（1M docs，`make bench` 口径）：

| 指标 | Rust | Java 9.12.3 | 加速比 |
|---|---|---|---|
| 单线程吞吐 | 389k docs/s（74 MB/s） | 131k docs/s | **2.96x** |
| 8 线程吞吐 | 1,284k docs/s | 220k docs/s | **5.83x** |

日志 7 字段全索引（1M docs，`make log-bench` 口径）：

| 指标 | Rust | Java 9.12.3 | 加速比 |
|---|---|---|---|
| 单线程吞吐 | 182k docs/s | 63k docs/s | **2.90x** |
| 8 线程吞吐 | 581k docs/s | 104k docs/s | **5.60x** |
| add 延迟 p50 / p99 | 2.0–2.2 / 5.5–11.3 µs | 6.2–9.7 / 16.7–77.2 µs | ~3x / ~7x |

索引体积与 Java 基本相当（差 ~2%，bigdict 场景 47.73MB vs 48.16MB）。瓶颈分布：postings 热路径（分词 + 词典）~31%、LZ4 ~12.5%——BKD/DV 写入合计仅 ~4%。

JSONL 写入（20 万篇 7 字段日志 JSON，`compare-index.sh --json` 口径，单线程，含文件 IO 与 JSON 解析）：

| writer | docs/s | user CPU | maxrss |
|---|---|---|---|
| rust jsonindex（纯 Rust 读文件） | 152k | 1.14 s | 95 MB |
| **java + JNI 批量（`addJsonBatch`，1000 篇/批）** | 127k | 1.69 s | 181 MB |
| java stock Lucene 9.12.3 | 46k | 8.45 s | 185 MB |

JNI 批量路径达到纯 Rust 的 ~84%、stock Java 的 2.8×——每批一次 JNI 穿越摊薄了调用开销，JSON 解析只在 Rust 侧发生一次；剩余差距主要是 `byte[]` 拷贝。JIT 预热 / 火焰图 CPU 归因分析见 `docs/bench-jit-warmup-flamegraph.md`。

## 格式兼容验证

- 双侧 `CheckIndex` 零错误（单段、8 段并发、稀疏、大字典、positions 等场景全覆盖）
- 查询结果与同语料 Java 索引**逐条 diff 一致**：term count、point range、sort by DV、phrase、DV 基数 / 字典 hash
- 低层编码字节级 golden vectors 对齐 `ForUtil` / `ForDeltaUtil` / `PForUtil`；FST `.tip` 由 Java `FST.read` 读回逐条比对
- `cargo test` 98 项全绿

## 快速开始

```bash
make build          # cargo build --release + javac interop 工具
make interop-test   # M1 文本链路：Rust 写 → Java CheckIndex + 查询 diff
make log-test       # M2 日志 schema 四轮互操作验证
make compare INPUT=/path/to/logs NDOCS=500000   # 同语料 Rust/Java 对比
make log-bench LOGDOCS=1000000 LOGTHREADS=8     # 日志场景基准
```

## 范围与限制

- 只写不读：读取 / 检索 / 段合并（merge）委托 Java Lucene；`commit_segments` 已预留多段合并提交接口
- 无打分配置：一律 omitNorms、不写 `.nvm/.nvd/.nrm`
- `IndexWriter::create` 仅支持 CREATE（空目录建索引），暂不支持追加打开已有索引
- 暂不支持：SortedSet / Binary / SortedNumeric DV、多维 points、compound file、BEST_COMPRESSION（ZSTD）

详细里程碑报告见 `docs/m1-report.md`、`docs/m2-report.md`；格式笔记见 `docs/format-notes-*.md`。
