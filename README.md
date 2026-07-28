# RustLucene

用 Rust 重写的 **Lucene 9.12.3 格式兼容索引**：追加写（append-only）链路，产出 Java Lucene 9.12.3 可读的段文件与 `segments_N` 提交点；并提供 **Rust 搜索读路径**（Term / Phrase / Boolean / multi-term / 范围 / 排序，ConstantScore 无打分），高 df term 可选内联 Roaring bitmap 加速。**实时内存搜索**：未 flush 写入缓冲通过 `LeafAccess` 统一 trait 直接可搜，RwLock 单写多读并发，磁盘与内存共用同一查询执行引擎。段合并不在范围内——Java Lucene 担任交叉校验（`CheckIndex` + 查询结果逐条 diff）。

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
| 二进制列存 | `BinaryDocValues` | 变长 DirectMonotonic 地址 + IndexedDISI，Lucene90 兼容 |
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

### 搜索读路径（`crates/core/src/search`，M2）

- **查询能力**（由写入能力严格限定，全部 ConstantScore、无打分 / norms / impact）：Term、Phrase（slop=0）、**嵌套 Boolean（MUST/SHOULD/MUST_NOT，跨字段、跨子查询）**、Boolean must/should（同字段平铺保留）、Terms（IN 语义）、Prefix、Wildcard（`*`/`?`，前缀形走 FST 前缀扫）、**1D PointRange（LongPoint/IntPoint）**、MatchAll、top-N（INDEXORDER）。M6 详细设计见 `docs/superpowers/specs/2026-07-24-rust-m6-bool-point-forcemerge-design.md`。
- **架构**：方案 C——执行语义逐行对照 Lucene 9.12.3 源码（advance 协议、position 合取、BKD 边界、MISSING 排序），对象结构 Rust 化：`enum Query` + `trait DocIter`，不做 Java 式 Query/Weight/Scorer 继承体系
- **快照语义**：open 即快照，重开即刷新（无 NRT 原地 refresh）；单线程逐段执行
- 只保证读**本系统写出的**索引（无 delete / `.liv` / norms）；Java 写的索引可读但不做删除语义

### 实时内存搜索（RwLock 并发 + LeafAccess 统一执行）

写入缓冲（未 flush 文档）**可直接搜索**，无需先落盘。设计目标：单写线程（JNI 调用方）+ 多读线程（查询方），<100 QPS 场景下查询延迟 1ms 级。

**并发模型**：`Arc<RwLock<IndexWriter>>`——写端持 `write()` 锁追加文档，读端持 `read()` 锁搜索。RwLock 保证写时互斥、读时共享；写入缓冲为 append-only 结构（`Vec` 追加、哈希表插入），读锁内无可变借用，安全共享。搜索全程持读锁（~1ms 级），不阻塞其他读者。

**统一执行引擎（LeafAccess trait）**：磁盘段与内存缓冲共用同一套查询执行代码，消除 ~200 行重复逻辑：

```
trait LeafAccess {
    type TermHandle;                        // 磁盘: TermEntry, 内存: MemTermHandle
    fn seek_term(...)  -> Option<(bool, Self::TermHandle)>;
    fn docs_enum(...)  -> SegmentDocIter;   // 磁盘: PFOR 解码, 内存: Vec<u32> 迭代
    fn positions_enum(...) -> SegmentDocIter;
    fn open_term_bitmap(...) -> Option<FrozenBitmap>;  // 内存永远 None (tier 3)
    fn terms_iter(...) -> Box<dyn TermsIterAccess>;     // 磁盘: FST 流, 内存: 排序数组
    fn points_reader(...) -> Option<&dyn PointsAccess>; // 磁盘: BKD 树, 内存: 线性扫描
    fn numeric_dv(...) -> Option<i64>;      // 排序键
    ...
}
```

- `Query::segment_iterator<L: LeafAccess>` 泛型执行——Term / Phrase / Bool / Prefix / Wildcard / PointRange / MatchAll 全部查询类型对磁盘和内存透明
- `IndexWriter::search()` 内 `drive_segment<L: LeafAccess>` 统一驱动：先遍历已提交磁盘段（`SegmentReader`），再遍历内存缓冲（`MemoryLeafAccess`），doc_base 累加保证全局 docID 唯一
- 内存 postings 通过 `SegmentDocIter::MemDocs` / `MemFreqs` 变体接入同一枚举分发；phrase positions 通过 `MemPositionsEnum` 适配器复用 `PhraseDocIter`
- 排序搜索：`SortedTopN` 堆收集器 + `numeric_dv` 排序键，支持按时间字段降序 top-N

**JNI 搜索接口**：`nativeSearch(byte[] queryJson, int topN, String sortField, boolean sortDesc)` —— JSON 查询解析（serde）+ 统一搜索 + 结果序列化（docID 数组 + total），一次 JNI 穿越。`nativeDocument(int docId)` 两阶段取文档：读锁内定位段/偏移，解锁后读 `.fdt` 磁盘数据。

### 高 df term 内联 Roaring bitmap（M3–M5）

- **格式**（Lucene 兼容附加字节）：写侧 `--bitmap` 开启后，对 df ≥ 4096 的 term 在 `.doc` 流内、该 term postings **之前**内联 `[magic "RLBM" + version + df + cardinality + Frozen payload][len u32]`。FST output（docStartFP）不动——bitmap 紧邻其前，`docStartFP-4` 取 len 回退定位；Java 读写该索引零感知，CheckIndex 照常通过。无 bitmap / 版本不符 → 四重校验失败静默落档 postings
- **引擎**：croaring（CRoaring C 库）2.7.0。v3 格式直接写 **Frozen 序列化**（Lucene 离线场景的标准用法），读侧 `FrozenBitmap::view()` 对齐缓冲零拷贝打开（~60ns），AND/OR 走 croaring 的 SIMD C 容器算子；count 命中 `and_cardinality` 快路径不物化
- **三档执行**：档 1 全部子句有 bitmap → 纯 roaring fold（AND 按 df 升序 fold，df 偏斜 ≥256x 时改 probe：小侧迭代 + 大侧 `contains`）；档 2 混合 → bitmap 侧物化后与 postings 迭代器对齐；档 3 纯低 df → 原生 PFOR + 跳表（保留 Lucene 跳读红利）
- **演进结论**（M3→M5 三次引擎迭代的教训）：M3 自研容器 + 每查询全量反序列化 → 稠密快（8.7x/15.2x）但稀疏倒挂（反序列化税 ~13ns/doc）；M4 零拷贝字节游标视图去掉了反序列化税，却丢了容器级折叠 → 稠密回退；M5 Frozen 格式让"零拷贝视图"与"容器级 SIMD 算子"两全，稀疏稠密全面转正
- 读侧总开关 `RL_BITMAP=0`（落档纯 PFOR，用于 A/B 对拍）

### 工具与集成

- CLI `rustlucene-cli`：`write` / `bench` / `index <文件或目录> [--positions] [--docs N]` / `logwrite [--bitmap]` / `logbench` / `searchbench <indexDir> <field> [--load-queries F] [--warmup N] [--iter N]` / `jsonindex <jsonlFile> <indexDir> <schemaSpec>` / `jsongen`
- JNI 绑定（`crates/jni-binding`，cdylib）：`RustIndexWriter` 供 Java 进程内调用；除逐字段 API 外提供**批量 JSON 写入** `addJsonBatch(byte[][])`——原始 JSON 字节整批一次 JNI 穿越，解析 / 强转 / 过滤 / 绑定全在 Rust 内闭环
- JSON 绑定层（`core/src/json.rs`）：schema spec 声明类型与索引配置（`name:type+mods[@json键]`、`$policy=` 指令），未知字段三策略：`strict`（过滤）/ `dynamic`（按值类型推断并自动注册，对齐 Lucene 动态字段语义）/ `stored-only`
- 互操作脚本：`interop/verify-index.sh`、`interop/verify-log.sh`、`interop/compare-index.sh`（同语料双侧建索引 + CheckIndex + term 级 diff，`--json` 模式为 rust / java-jni / java 三方对比）；`make interop-test` / `log-test` / `bench` / `log-bench` / `compare`

### 时序指标存储（`crates/metric`）

字节级兼容 lts-metric V5 格式的时序指标写入/降采样/合并引擎，纯库 + JNI 暴露。

**模块架构**：

```
crates/metric/src/
├── algo/           # 纯算法层（零 IO）
│   ├── gorilla.rs      # Gorilla 压缩（DoD + XOR，MSB-first bitstream）
│   ├── series_hash.rs  # xxhash64(seed=0) + UTF-16 LE，两段 32-bit 拼接
│   ├── downsample.rs   # 5 列聚合（count/sum/min/max/delta）+ 列式 Gorilla
│   └── hll.rs          # HLL_4 基数估计（logK=12, ~1.6% SE）
├── store/          # 存储层
│   ├── schema.rs       # V5 Schema（7 字段）+ downsample Schema
│   ├── series_writer.rs # series → Document → IndexWriter
│   └── metadata.rs     # ShardMetadata serde（25 字段, camelCase, pretty-print）
├── runtime/        # 运行时层
│   ├── buffer.rs       # SeriesBuffer（HashMap 缓冲 + tokio 自动刷盘）
│   ├── downsample_op.rs # downsample_shard（读 raw → 聚合 → 写 downsample）
│   └── merge_op.rs     # merge_intra_shard + merge_cross_shard（流式 k-way 归并）
└── jni.rs          # JNI 入口（feature = "jni"）
```

**核心能力**：

| 能力 | 说明 |
|------|------|
| 写入 | SeriesBuffer 内存缓冲 → 按 series_hash 聚合 → Gorilla 压缩 → BinaryDocValues 存储 |
| 降采样 | 5m/1h 粒度，5 列统计（count/sum/min/max/delta），Counter Reset 感知 |
| 段内合并 | 同 shard 内相同 series 合并（排序 + 去重 + 重编码） |
| 跨 shard 合并 | 流式 k-way 归并（index_sort 有序），同时生成 compact + 5m + 1h 三路输出 |
| 基数估计 | HLL_4 sketch，用于 metadata.json 的 seriesEstimate |
| 索引排序 | 写入时 index_sort(series_hash)，merge 输出保持有序 |

**性能（1M series, 15s scrape, 5min flush, release mode）**：

| 操作 | 吞吐 | 说明 |
|------|------|------|
| 写入 | ~1M pts/s | 含 Gorilla 编码 + IndexWriter flush |
| 合并 | ~2.6M pts/s | 流式 k-way，峰值内存 ~1GB |
| 读取解压 | ~18M samples/s | Gorilla decode |

### Java JNI 使用教程

**1. 编译 native 库**：

```bash
cargo build --release -p rustlucene-metric --features jni
# 产出: target/release/librustlucene_metric.so (Linux) / .dylib (macOS)
```

**2. Java 侧声明 native 类**：

```java
package com.metric;

public class RustMetric {
    static {
        System.loadLibrary("rustlucene_metric");
    }

    // 写入侧
    public static native long openMetricWriter(String shardDir);
    public static native boolean writePoint(long handle, String name, String labels, long time, double value);

    // 批量写入：同一 series 多个点（推荐：减少 JNI 穿越）
    public static native boolean writePoints(long handle, String name, String labels, long[] times, double[] values);

    // 批量写入：多个 series 各一个点
    public static native boolean writePointsMulti(long handle, String[] names, String[] labels, long[] times, double[] values);

    // 推荐：二进制打包批量写入（1 次 JNI 穿越 = N 个点，跨多个 series）
    // 返回成功写入的点数，出错返回 -1
    public static native long writeBatch(long handle, byte[] packedPoints);

    public static native boolean flushBuffer(long handle);
    public static native void closeMetricWriter(long handle);

    // 降采样（Java 调度，Rust 执行）
    public static native boolean downsample(String inputDir, String outputDir, long granularityMs);

    // 合并（Java 调度，Rust 执行）
    public static native boolean mergeIntraShard(String inputDir, String outputDir);
    public static native boolean mergeCrossShard(String[] inputDirs, String compactDir, String ds5mDir, String ds1hDir);
}
```

**3. 写入指标数据**：

```java
// 打开 writer（创建 shard 目录 + 初始化 SeriesBuffer）
long handle = RustMetric.openMetricWriter("/data/metrics/2026-07-27/raw/shard_0");

// 写入数据点
// labels 格式: "$#$key1=val1$#$key2=val2$#$"（按 key 字典序，首尾各有 $#$）
// 空 labels: "$#$"
RustMetric.writePoint(handle, "cpu.usage", "$#$host=h1$#$region=us$#$", 1753596000000L, 72.5);
RustMetric.writePoint(handle, "cpu.usage", "$#$host=h1$#$region=us$#$", 1753596015000L, 73.1);
RustMetric.writePoint(handle, "mem.free",  "$#$host=h2$#$",           1753596000000L, 4096.0);

// 刷盘（flush + commit，数据持久化到 shard 目录）
RustMetric.flushBuffer(handle);

// 继续写入下一个周期的数据...
// （生产环境：每 5 分钟调一次 flushBuffer，产生一个新 segment）

// 关闭（best-effort 最终 flush + 释放内存）
RustMetric.closeMetricWriter(handle);
```

**推荐：二进制打包批量写入**（实时收到来自多个 series 的点时，攒一批后单次 JNI 穿越写入）：

```java
// 二进制协议（小端）每个点：
//   name_len: u16 (2 字节) + name (UTF-8)
//   labels_len: u16 (2 字节) + labels (UTF-8)
//   time: i64 (8 字节)
//   value: f64 (8 字节)

// Java 侧打包工具
public static byte[] packPoints(List<MetricPoint> points) {
    ByteBuffer buf = ByteBuffer.allocate(points.size() * 64).order(ByteOrder.LITTLE_ENDIAN);
    for (MetricPoint p : points) {
        byte[] nameBytes = p.name.getBytes(StandardCharsets.UTF_8);
        byte[] labelBytes = p.labels.getBytes(StandardCharsets.UTF_8);
        buf.putShort((short) nameBytes.length);
        buf.put(nameBytes);
        buf.putShort((short) labelBytes.length);
        buf.put(labelBytes);
        buf.putLong(p.time);
        buf.putDouble(p.value);
    }
    return Arrays.copyOf(buf.array(), buf.position());
}

// 使用
byte[] batch = packPoints(collectedPoints);        // 收集的一批点（跨多个 series）
long written = RustMetric.writeBatch(handle, batch);  // 一次 JNI 穿越 = N 个点
```

**4. 降采样（Java 决定时机，Rust 执行）**：

```java
// 对已 sealed 的 raw shard 做 5 分钟降采样
RustMetric.downsample(
    "/data/metrics/2026-07-27/raw/shard_0",
    "/data/metrics/2026-07-27/downsample_5m/shard_0",
    300000L  // 5 分钟 = 300,000 ms
);

// 1 小时降采样
RustMetric.downsample(
    "/data/metrics/2026-07-27/raw/shard_0",
    "/data/metrics/2026-07-27/downsample_1h/shard_0",
    3600000L  // 1 小时
);
```

**5. 合并（Java 决定哪些 shard，Rust 执行）**：

```java
// 段内合并：同一 shard 内重复 series 合并
RustMetric.mergeIntraShard(
    "/data/metrics/2026-07-27/raw/shard_0",
    "/data/metrics/2026-07-27/raw/shard_0_merged"
);

// 跨 shard 合并：多个 L0 shard → 1 compact + 5m + 1h（三路输出）
RustMetric.mergeCrossShard(
    new String[]{
        "/data/metrics/2026-07-27/raw/shard_0",
        "/data/metrics/2026-07-27/raw/shard_1",
        "/data/metrics/2026-07-27/raw/shard_2"
    },
    "/data/metrics/2026-07-27/raw/compact_0",
    "/data/metrics/2026-07-27/downsample_5m/compact_0",
    "/data/metrics/2026-07-27/downsample_1h/compact_0"
);
```

**6. 调度边界**：

| 职责 | 归属 |
|------|------|
| 写入缓冲 + 刷盘 | Rust（SeriesBuffer + tokio 阈值触发） |
| 降采样调度（何时、对哪个 shard） | Java |
| 合并调度（何时、合并哪些 shard） | Java |
| 目录管理（命名、复制、原子替换、清理） | Java |
| 降采样/合并执行 | Rust（无状态，给路径→产路径） |

**V5 存储格式**：每个 series 的全部时间点 Gorilla 压缩后存入一个 Lucene BinaryDocValues 字段（`gorilla_data`），配合 `series_hash`（NumericDV + LongPoint）实现按 series 检索。Schema 7 字段：`metric_name`(SortedDV) / `metric_labels`(Text+BinaryDV) / `series_hash`(LongPoint+NumericDV) / `time_min` / `time_max`(LongPoint+NumericDV) / `sample_count`(NumericDV) / `gorilla_data`(BinaryDV)。

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

### 搜索性能（M5 终值，1M docs 日志语料，--no-cache 同口径，qps 比）

roaring（`--bitmap` 索引）vs 同索引纯 PFOR（`RL_BITMAP=0`）vs Java Lucene 9.12.3：

| 分组 | roaring / PFOR | roaring / Java |
|---|---|---|
| term high | 1.06 | 1.38 |
| and high | **7.02** | **2.28** |
| or high | **12.70** | **1.88** |
| iterm high | 1.23 | 0.67 |
| and / or 稠密（df≈200k，bitset 容器） | **23.1 / 43.8** | — |

- 高 df 布尔是 bitmap 的主场：M3 时稀疏 AND 倒挂 0.55x，M5 修到 7.02x（对自身 PFOR）并反超 Java 2.28x；稠密 regime and/or 提升 23–44 倍
- term count = doc_freq 直读，bitmap 零开销（~1.0x 校验）；iterm 靠 frozen view 批量迭代小幅领先 PFOR
- 写侧代价：吞吐损失 3.6–3.8%、索引体积 +15%（2262 个高 df term 各配一份 bitmap）
- **诚实残留**：vs Java 仍有 med 桶（df 4096–数万）未反超——or med 0.38、iterm med 0.62、and med 0.94；Java 的 DocIdSetIterator + 跳表 + JIT 热循环在该区间仍快 1.5–2.6x

详细口径与逐组数字见 `docs/m3-bench-report.md`（M3 基线）与 `.superpowers/sdd/m5-bench-report.md`（M5 终值，gitignored）。

### 实时内存搜索性能（LeafAccess 统一路径，4 核云主机，release profile）

写入吞吐（`add_document`，LeafAccess 重构前后无变化）：

| 规模 | 吞吐 |
|---|---|
| 10K–1M docs | **~130 万 docs/sec**（~260 MB/s @ 200B/doc） |

内存搜索延迟（未 flush 数据，`IndexWriter::search()`，avg/20 轮）：

| 查询类型 | 10K | 100K | 500K | 1M |
|---|---|---|---|---|
| Term | 17 µs | 150 µs | 718 µs | 1.5 ms |
| MatchAll | 73 µs | 717 µs | 3.5 ms | 7.7 ms |
| And(2 词) | 80 µs | 749 µs | 3.6 ms | 7.4 ms |
| Or(2 词) | 82 µs | 679 µs | 3.2 ms | 6.7 ms |
| Phrase | 364 µs | 4.9 ms | 30 ms | 60 ms |
| Prefix | 73 µs | 535 µs | 2.5 ms | 5.0 ms |
| Wildcard | 72 µs | 464 µs | 2.2 ms | 4.3 ms |
| PointRange | 27 µs | 252 µs | 1.1 ms | 2.6 ms |
| Bool(MUST+MUST_NOT) | 72 µs | 743 µs | 4.0 ms | 8.3 ms |
| Term+SortDesc(top10) | 109 µs | 1.3 ms | 6.8 ms | 14.5 ms |

内存 vs 磁盘（1M docs，同查询同口径）：

| 查询 | 内存 | 磁盘 | 比值 |
|---|---|---|---|
| Term | 1.5 ms | 2.1 ms | **0.7x（内存更快）** |
| PointRange | 2.6 ms | 3.3 ms | **0.8x（内存更快）** |
| Prefix | 5.0 ms | 6.1 ms | **0.8x（内存更快）** |
| MatchAll | 7.7 ms | 4.4 ms | 1.7x |
| Bool | 8.3 ms | 4.4 ms | 1.9x |
| Phrase | 60 ms | 25 ms | 2.4x |

- Term / PointRange / Prefix：内存无 I/O 开销，直接内存访问优于磁盘
- Phrase / Bool / MatchAll：内存始终走 tier 3（线性扫描），磁盘有 roaring bitmap 折叠 + 优化编码；Phrase 瓶颈在内存 positions 的 `Vec<Vec<u32>>` 逐 doc 遍历
- 写入+搜索交替：1M 缓冲时 Term 查询 ~1.5 ms，写入吞吐不受搜索影响

基准复现：`cargo run --release --example bench_leaf_access [num_docs]`

## 格式兼容验证

- 双侧 `CheckIndex` 零错误（单段、8 段并发、稀疏、大字典、positions、bitmap 等场景全覆盖，`make log-test` 五变体 11 次 "No problems"）
- 查询结果与同语料 Java 索引**逐条 diff 一致**：term count、boolean and/or、prefix / wildcard / terms、point range、sort by DV、phrase、DV 基数 / 字典 hash
- bitmap A/B 对拍：roaring 路径 vs 纯 PFOR 路径（`RL_BITMAP=0`）hit-counts 逐位一致；v1/v2 旧格式索引自动落档
- 低层编码字节级 golden vectors 对齐 `ForUtil` / `ForDeltaUtil` / `PForUtil`；FST `.tip` 由 Java `FST.read` 读回逐条比对
- `cargo test`：codec-lucene9 192 项 + rustlucene-core 128 项 + rustlucene-metric 48 项全绿

## 快速开始

```bash
make build          # cargo build --release + javac interop 工具
make interop-test   # M1 文本链路：Rust 写 → Java CheckIndex + 查询 diff
make log-test       # 日志 schema 七变体互操作验证（含 --bitmap、Rust forceMerge、Java forceMerge）
make compare INPUT=/path/to/logs NDOCS=500000   # 同语料 Rust/Java 对比
make log-bench LOGDOCS=1000000 LOGTHREADS=8     # 日志场景写入基准
# 搜索基准：先建 bitmap 索引，再三路对拍（roaring / RL_BITMAP=0 / Java --no-cache）
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- logwrite /tmp/idx 1000000 42 --bitmap
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- searchbench /tmp/idx message --warmup 10 --iter 30
# forceMerge(1)：把多段压成一个段（bitmap 按配置重建）
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- forcemerge /tmp/idx --bitmap
# 实时内存搜索性能基准（写入吞吐 + 内存搜索延迟 + 磁盘对比）
cargo run --release --example bench_leaf_access 100000
```

## 范围与限制

- **段合并**：已实现 `forceMerge(1)`，会把当前全部段归并成一个新段、重建 `segments_N`、删除旧段文件；delete / 更新不在范围内。写侧仅 CREATE（空目录建索引），暂不支持追加打开已有索引
- 无打分：写侧一律 omitNorms、不写 `.nvm/.nvd/.nrm`；读侧全部 ConstantScore
- 读侧只保证读本系统写出的索引（无 `.liv` / norms / vector）；NRT 为 open 即快照、重开即刷新
- **实时搜索**：写入缓冲（未 flush）可通过 `IndexWriter::search()` 直接搜索，RwLock 单写多读；内存搜索始终走 tier 3（无 roaring bitmap），Phrase / 高 df Bool 延迟高于磁盘路径。缓冲上限由 `max_buffered_docs`（默认 100 万）/ `max_ram_bytes`（默认 512MB）控制
- 暂不支持：SortedSet / SortedNumeric DV、多维 points、compound file、BEST_COMPRESSION（ZSTD）、模糊查询（Levenshtein 自动机）、聚合 / facet、可配目标段数的 merge 策略
- bitmap 为实验性写侧开关（`--bitmap` 默认 off）：只加速 docs 维度，phrase / freq 永远落档 postings；multi-term 的 roaring 集成未做
- **指标存储**：HLL wire format 为简化版（非 DataSketches 字节兼容，估计算法兼容）；metric 查询路径（按 metric_name+labels 拉取 series）暂未实现

里程碑与设计文档：写入链路 `docs/m1-report.md`、`docs/m2-report.md`；搜索读路径 / bitmap 各阶段 spec 在 `docs/superpowers/specs/`（2026-07-22 搜索设计、M2 multi-term、M3/M4/M5 bitmap 三部曲、M6 嵌套 Bool + Point + forceMerge、2026-07-27 RwLock 实时搜索设计、2026-07-27 LeafAccess 统一设计）；指标存储设计 `docs/superpowers/specs/rust-metric-storage-design.md`、实现计划 `docs/superpowers/plans/rust-metric-storage.md`；bitmap bench 基线 `docs/m3-bench-report.md`。格式笔记见 `docs/format-notes-*.md`。
