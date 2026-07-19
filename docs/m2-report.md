# RustLucene M2 交付报告：日志场景字段全家桶（keyword / LongPoint·IntPoint BKD / Sorted·Numeric DocValues）+ M3/M4 性能复测

日期：2026-07-19。里程碑范围：在 M1（text + postings + stored）之上补齐日志场景字段类型——
`StringField`（keyword，DOCS）、`LongPoint/IntPoint`（1D BKD）、`SortedDocValues`、`NumericDocValues`
（含稀疏字段 IndexedDISI）、message 字段可选 positions、LZ4 stored fields（M1 已交付）。
验收口径：Java Lucene 9.12.3 对 Rust 写出的索引做 term / point range / sort by doc values / 短语查询，
结果与同语料 Java 写出的索引**逐条 diff 一致**，且双侧 `CheckIndex` 零错误。

## 1. 新增格式层（codec-lucene9）

| 模块 | 覆盖文件 | 格式事实来源（逐 file:line 引用） |
|---|---|---|
| `doc_values.rs`（~560 行实现 + ~840 行测试） | `_N_Lucene90_0.dvd/.dvm` | `docs/format-notes-docvalues.md` |
| `points.rs`（~1600 行含测试） | `_N.kdd/.kdi/.kdm` | `docs/format-notes-points.md` |

**DocValues（Lucene90DocValuesFormat，版本 0）**：NUMERIC 恒 gcd=1/单块 DirectWriter（合法简化，见笔记 §11），
docsWithField 三分支全实现（稠密 / 全缺失 / IndexedDISI——SPARSE≤4095、DENSE rank+bitset、ALL=65536、
哨兵块、多块跳表含"单真实块不写跳表"特例）；SORTED 的 ords 走数值路径（min=0/gcd=1）、terms dict
64 项/块前缀压缩 + 裸 LZ4 流（不带字典压缩，对读侧预置字典合法——与 stored fields dict=0 同款论证，
已经 Java 读取实证）、块地址与 reverse index 均 DirectMonotonic(shift=16)。`.fnm` 写
`PerFieldDocValuesFormat.format=Lucene90`/`suffix=0`、dvGen=-1。

**Points（Lucene90PointsFormat，版本 0；内层 BKD header 版本 9）**：1D long/int 全内存
(value, docID) 排序（等价 Java `writeField1Dim` 主路径，无需 OfflinePointWriter/BKDRadixSelector）、
512 点/叶（仅最右叶余数——读侧 `size()`/`estimatePointCount` 的硬假设）、DocIdsWriter 五分支全实现
（CONTINUOUS/BITSET/DELTA16/BPV24/BPV32）、values 块全等叶(-1) + 高基数 run-length(0) 两分支、
`getNumLeftLeafNodes` 固定树形 + `recursePackIndex` 逐字节复刻（fp-delta 链、split 前缀压缩、
negative-delta、leftNumBytes 回填、numLeaves==1 单 VLong 特例）。`.fnm` 记 (1,1,8/4) 三元组，
无 attributes（PointsFormat 不经 PerField 包装，与 postings/DV 不同）。

单元测试：codec crate 90 项全绿（含 M1 的 67 项）。新增测试用自写解码器（模拟 BKDReader /
Lucene90DocValuesProducer 语义）做 round-trip：DirectWriter/DirectMonotonic 位流、IndexedDISI 各
cardinality 分支与跳表、terms dict LZ4 解压重建、packed index 树遍历（叶 nodeID 覆盖校验、
split 链不变式、leftNumBytes 跳读游标校验）。

## 2. 核心写入链路（rustlucene-core）

- `FieldValue::{Text, Keyword, Long, Int}`；`FieldSpec` 扩展 `tokenized` / `doc_values` / `points`
  三个维度，可组合（如 timestamp = LongPoint + NumericDV + stored，对应 Java 同名三字段）。
- `DocWriter` 按 spec 建缓冲：postings（keyword 整串一词）、NumericDV（每 doc 单值校验，
  对齐 NumericDocValuesWriter.java:51-57）、SortedDV（字典 + term_ids，flush 时按排序字典重映射 ord，
  对齐 SortedDocValuesWriter.java:113-125）、points（(i64, doc) 缓冲，flush 时排序）。
- `SegmentBuilder::finalize` 依次写 .fnm → stored → postings → .dvd/.dvm → .kdd/.kdi/.kdm → .si
  （files 集合按实际产出登记）；无 DV/points 字段时不产生对应文件（与 Java 一致）。
- **M3 落地**：RAM 增量估算（O(1)/doc）+ `IndexWriterConfig.max_ram_bytes`（默认 512MB）触发 flush。

## 3. 格式兼容验证（Java 9.12.3 终验，`make log-test` 一键四轮）

工具：双侧同语料生成器（`rustlucene-cli logwrite` ↔ `interop/java/JavaLogBench.java`，
xorshift64* 同流、rng 调用序列一致）、`interop/java/VerifyLogIndex.java`（固定查询电池，
全部 ConstantScore 语义）、`interop/verify-log.sh`。

| 场景 | CheckIndex（Rust 侧） | CheckIndex（Java 侧） | 查询 dump diff |
|---|---|---|---|
| dense 200k docs（seed 42） | **No problems** | No problems | **完全一致** |
| --positions 200k docs（seed 43） | **No problems** | No problems | **完全一致**（含 phrase） |
| --sparse 200k docs（seed 44：latency 缺 1/7、bytes 缺 1/11、level 缺 1/13、status 仅存 1/17 → 4 块 IndexedDISI，DENSE+SPARSE 分支、多块跳表） | **No problems** | No problems | **完全一致**（含 dv_card 与 MISSING 标记） |
| --bigdict 200k docs（seed 45：trace_id_sdv 20 万项 → 3125 个 terms-dict 块 + 196 条 reverse index 抽样） | **No problems** | No problems | **完全一致**（全字典 hash 相等） |

diff 的查询项：5 个 level 的 term count、trace_id 精确命中、timestamp 范围 count + 前 20 docID、
全范围 count、sort by timestamp DV top10（docID+值）、level SortedDV 字典（unsigned 升序
DEBUG,ERROR,INFO,TRACE,WARN）、大字典 size/bounds/hash、4 个数值 DV 字段 docsWithField 基数、
latency 前 9 文档逐值、message 首词 term count（+positions 时 phrase count）。

补充验证：
- **RAM 触发 flush**（M3）：`logwrite` 2M docs → 3 个段（512MB 估算阈值两次触发），
  CheckIndex `numSegments=3` **No problems**。
- **索引体积**（bigdict 200k 同语料）：Rust 47.73 MB vs Java 48.16 MB（小 0.9%）。
- **M1 回归**：`make interop-test` 继续通过（VERIFY_OK / INTEROP_OK）；`cargo test` 98 项全绿。

## 4. 性能（M4 口径：日志 schema，同语料双侧，`bench/run-log-bench.sh`，3 轮取优）

语料/文档：timestamp（LongPoint+NumericDV+stored）、level（keyword+SortedDV）、trace_id（keyword）、
message（~200B text）、latency_ms/bytes_sent/status（NumericDV）——7 字段全索引。
Java 对照：`JavaLogBench`，同 FieldType 配置（omitNorms、DOCS_AND_FREQS）、非 compound、RAMBuffer 256MB。
环境：4 核云主机，OpenJDK 21，Rust 1.97（同 M1）。

| 场景 | Rust docs/s | Java docs/s | 加速比（MB/s 同口径） |
|---|---|---|---|
| 1M docs，单线程 | **182,249** | 62,873 | **2.90x** |
| 1M docs，8 线程 | **581,058** | 103,756 | **5.60x** |
| 5M docs，单线程 | **176,298** | 69,431 | **2.54x** |

- add 延迟（双侧同机采样，µs）：Rust p50 2.0–2.2 / p99 5.5–11.3；Java p50 6.2–9.7 / p99 16.7–77.2。
- flush 停顿（单线程末段一次性 flush 含 postings/DV/BKD/stored 收尾）：1M docs 1.66s，5M docs 8.65s；
  commit（两段式 + fsync）≤ 9ms。Java 因 256MB RAMBuffer 多次中途 flush，摊入总时长。
- 8 线程对自身单线程扩展 3.19x（4 物理核 + SMT；Java 1.65x）。任务书"8 核加速比 ≥ 6"以 8 物理核为前提，
  本机 4 核下对 Java 的相对吞吐 5.60x 远超 ≥2x 门槛。

### 火焰图/剖析解读（perf cpu-clock:u，500k docs 单线程，自耗时 Top）

- `SegmentBuilder::add_document` 16.1% —— 索引驱动循环（分派 + stored 序列化 + RAM 记账）。
- `TermDict::lookup_or_insert_flag` 14.9% —— postings 词典开放寻址（message 每 doc ~33 个 token）。
- `LZ4_compress_fast_extState` 12.5% —— stored fields 压缩（7 字段全 stored 的 3 个，chunk 密度高于 M1）。
- libc memcpy/memcmp ~9.7%、malloc/cfree ~4.4% —— 词条 arena/数组扩展与语料 String。
- `PostingsWriter::write_term` 4.6% + `SegmentBuilder::finalize` 3.3% —— flush 期编码与段收尾。
- 语料生成（gen_message/gen_log_document）4.7% —— 双侧对称成本。
- `TermDict::sorted_ids` 排序 2.8% —— flush 期词典排序（block-tree 需全局有序）。
- `PointsWriter::write_field_1d` 1.4%、DocValues 写出 <1% —— BKD 排序+叶/index 编码与 DV 单块
  DirectWriter 成本均很低；7 字段全开下新增字段类型合计仅占总时长 ~4%。

结论：日志 schema 下瓶颈仍在 postings 热路径（分词 + 词典，~31%）与 LZ4（12.5%），与 M1 同构；
BKD/DV 写入几乎免费。单核继续压榨的空间在分词扫描 SIMD 化与词典探测 cache 布局（预估 10–20%）。

## 5. JNI 交付形态

`crates/jni-binding`（cdylib `librustlucene_jni.so`，`jni` crate 0.21）：
`RustIndexWriter(String path, String schemaSpec)` + `beginDocument/addText/addKeyword/addLong/addInt/endDocument/flush/commit/close`；
schema 规格串（`timestamp:longpoint+numericdv+stored,level:keyword+sorteddv,...`）。
Java 侧冒烟 `JniLogBench` 50k docs：写入 → CheckIndex **No problems** → VerifyLogIndex 查询正确。
（JNI 路径 ~10 次 native 调用/doc，吞吐 ~55k docs/s，受逐次调用开销主导；生产用法建议批量化
接口（单调用多文档）或直接嵌入 Rust 侧 shard writer。）

## 6. 瓶颈分析与下一步

- postings 热路径与 LZ4 合计 ~44%，与 M1 结论一致；M3 的零拷贝/arena/流式 stored 已就位，
  剩余单核空间在 SIMD 分词与 postings 追加的 cache 布局。
- flush 停顿随段大小线性（5M docs 8.65s）；日志管道可接受（段即提交单位），若需更低尾延迟，
  用 max_ram_bytes 把单段压小（吞吐换停顿）。
- merge 接口预留：`commit_segments(dir, infos, generation)` 已支持把多个私有 builder 的段并成
  一个提交点（8 线程 bench 即用此路径）；段合并（merge）按任务书不在本期范围。
- 后续可选项：SortedSet/Binary DV、多值 SortedNumeric、ZSTD BEST_COMPRESSION 模式、
  批量化 JNI 接口、100M 级数据集的 NVMe 轮次（本机盘为云盘，tmpfs 容量受限，未测 1 亿条全量）。
