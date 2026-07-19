# RustLucene M1 交付报告：格式兼容最小写入器 + 写入性能

日期：2026-07-19。里程碑范围：单字段（text，indexed+stored，`DOCS_AND_FREQS` + omitNorms）写入链路，
Rust 产出 Lucene 9.12.3 可读的段文件与 `segments_N` 提交点，写入吞吐达到 Java 对照组的 2 倍以上。

## 1. 性能对比表

基准：100 万文档，每文档 1 个 ~200B message 字段（WhitespaceAnalyzer 等价分词），语料由同一种子
xorshift64* 生成器双侧逐字节一致生成。取 3 轮最优。环境：4 核云主机（/tmp 为 tmpfs），OpenJDK 21，
Rust 1.97。Java 对照：`lucene-core 9.12.3`，`IndexWriterConfig` 默认 + omitNorms +
DOCS_AND_FREQS + 非 compound + RAMBufferSize 256MB（与默认策略一致的公平配置）。

| 指标 | Rust | Java Lucene 9.12.3 | 加速比 |
|---|---|---|---|
| 单线程 docs/s | 388,500–392,157 | 131,199 | **2.96x** |
| 单线程 MB/s（输入字节） | 74.1–74.8 | 25.0 | **2.96x** |
| 8 线程 docs/s | 1,283,697 | 220,022 | **5.83x** |
| 8 线程 MB/s | 244.8 | 42.0 | **5.83x** |
| 1T→8T 自身扩展（4 物理核） | 3.30x | 1.68x | — |
| 单次 flush 停顿（1M docs 末段） | ~350 ms | —（多次 RAM 触发 flush，摊入总时长） | — |
| commit（两段式提交 + fsync，tmpfs） | ~2 ms | — | — |
| 峰值常驻内存（1M docs 单段） | 266 MB | 177 MB | — |

注：任务书 M4 的"8 核加速比 ≥ 6"以 8 物理核为前提；本机仅 4 核，3.30x 已接近物理上限
（8 线程对 4 核 + SMT 的正常区间），且对 Java 的相对吞吐 5.83x 远超 ≥2x 门槛。
内存侧 Java 更低是因为其 256MB RAMBuffer 触发了多次中途 flush；Rust 侧 M3 将增加
RAM 上限触发 flush（默认 512MB），当前以 max_buffered_docs 控制。

### 优化历程（同口径 1M docs 单线程）

| 轮次 | 改动 | docs/s | 加速比 |
|---|---|---|---|
| 基线 | HashMap<Vec<u8>> 词典 + lz4_flex + stored 全量缓冲 | 252k | 1.96x |
| R1 | arena 词典（开放寻址、零分配查找）+ Document 按值消费（去 200B clone）+ C liblz4 | 279k | 2.12x |
| R2 | ASCII 分词 + u64 块哈希 + LZ4 单子块编码（dict=0） | 342k | 2.60x |
| R3 | stored fields 流式直写（add 时序列化进 chunk，去掉 200MB 中转缓冲） | 372k | 2.82x |
| R4 | 字段号线性查找 + 语料生成单次 push_str | 389–392k | **2.96x** |

### 火焰图/剖析解读（perf record，最终版自耗时 Top）

- `SegmentBuilder::add_document` 41.1% —— 索引热路径：分词扫描（200B/doc）、u64 块哈希 +
  开放寻址探测、postings 追加（docs/freqs 平行数组）。已无可删的分配与拷贝，属近设计下限。
- `LZ4_compress_fast_extState` 17.3% —— stored fields 压缩（C liblz4，FAST(2)，每 80KB chunk 一次调用，
  单子块编码）。已对比 FAST(1/2/4)：加速档在本数据上收益≈0 且 FAST(4) 使 .fdt 增大 4%，取 FAST(2)。
- `gen_message` 7.3% —— 基准语料生成（Java 侧同样计入时长，对称成本）。
- libc memcpy/memcmp ~7.2% —— 词条 arena 扩展、哈希表 memcmp、stored 序列化拷贝。
- `PostingsWriter::write_term` 2.4% —— FOR/PForDelta 块编码（栈数组实现，无逐块分配）。
- malloc 1.5% —— 仅剩语料 String（每 doc 1 次）与 postings 数组摊销增长。

## 2. 格式兼容验证报告

**唯一事实来源**：`reference/lucene-9.12.3/` 源码（未用二进制猜测）；编码层逐文件对照并注释出处。

| 验证项 | 工具/方法 | 结果 |
|---|---|---|
| 单段 50 万文档全量校验 | Java `CheckIndex /tmp/rl-big` | **No problems detected**（2542 terms；10,699,542 term/doc pairs；500,000 stored fields） |
| 8 段并发写出索引校验 | Java `CheckIndex`（8 线程 bench 产出目录） | **No problems detected**（numSegments=8，segments_1 提交点） |
| 查询结果逐条 diff | `VerifyIndex`：全部文档 stored 比对 + 200 个采样 term 的 docID 集合 diff（ConstantScore 语义） | **VERIFY_OK** |
| 一键互操作 | `make interop-test`（Rust 写 → Java CheckIndex + 查询 diff） | **INTEROP_OK** |
| postings 低层编码字节级一致 | ForUtil/ForDeltaUtil/PForUtil golden vectors（同包 Java 转储类生成） | 67 项 codec 单测全绿 |
| FST（.tim 词典索引） | Java `FST.read` 读回 506 条逐条比对 | 全对 |
| positions/payloads 文件 | 目录清单核查 | 无 `.pos`/`.pay`（符合无打分约束：DOCS_AND_FREQS + omitNorms，不写 .nvm/.nvd/.nrm） |

覆盖的关键格式点：CodecUtil 头/尾（魔数、版本、CRC32  footer）、VInt/GroupVInt、FOR（collapse+位平面交错，
与 ForUtil.java:134-191 字面一致）、PForDelta 异常值补丁、DirectWriter bpv 12/20/28 重叠写、
DirectMonotonic .fdx 元数据、Lucene912 .doc/.psm 两级 skip（4096 边界）、block-tree .tim/.tip/.tmd 多层 FST、
LZ4 块存储（dict_length=0 单子块合法编码，由 LZ4WithPresetDictDecompressor 源码确认）、
.si 文件集合自引用、两段式 `pending_segments_N` → `segments_N` 提交（commit 前先 fsync 全部段文件）。

与 Java 写法的两处**合法偏差**（读取语义不变，CheckIndex 确认）：

1. stored fields 每 chunk 用 `dict_length=0`、单个子块编码。Lucene 解压器按头中 dict/block 长度泛化处理；
   换取写入侧单次整 chunk 压缩。代价是 Java 读侧取单文档时需从 chunk 头解到目标文档（读放大，仅影响读）。
2. LZ4 加速档 FAST(2)。任意合法 LZ4 流均可被解压，压缩率损失可忽略（日志文本重复度高）。

## 3. 瓶颈分析与下一步

当前单线程时间分布：索引构建 ~41%、LZ4 ~17%、语料生成 ~7%、postings 编码 ~2.4%、
memcpy/分配 ~9%、其余为 flush 写盘/元数据/提交。结论：

- 索引热路径已零分配（arena 词典 + 平行 postings 数组 + 流式 stored），继续单核压榨的空间在
  SIMD 分词扫描与 postings 追加的 cache 布局，预计还能挤出 10–20%。
- LZ4 是本机 CPU 的硬成本（~465MB/s@80KB 块）；若未来允许牺牲少量压缩率可评估换更弱更快的散列链，但
  格式要求 LZ4，无可回避。
- 多核分片无共享可变状态，扩展近线性（4 核 3.3x）；8 线程吞吐已达 Java 5.83x。

下一步（M2/M3 任务书路线）：keyword/数值/DocValues 字段全家桶与 BKD；RAM 上限触发 flush（默认 512MB）；
NVMe 轮次与 p99 add 延迟采集；merge 接口预留落地。

## 4. 补记（2026-07-19 下午）：positions 路径修复 + 文件索引/检索工具

新增 `rustlucene-cli index <文件或目录> <索引目录> [--positions] [--docs N]`：递归遍历输入，每行一个文档
（message 索引+存储、source/line 仅存储），配套 Java 检索工具 `interop/java/SearchIndex.java`
（term / and / phrase / count，全部 ConstantScore 包裹）。`--docs N` 指定目标文档数：语料行数不足时
从头循环重读直到写满 N 条（重复文档内容一致），N 小于行数时截断；不指定则全量索引一遍。

**该工具首次以 Java 验证了 .pos 写入路径，并抓到一个真 bug**：skip 条目的 pos fp 增量
首块基准写成 0（绝对 fp），而 Lucene 以 `posStartFP` 为基准
（Lucene912PostingsWriter.startTerm:226-227；读取侧 Lucene912PostingsReader:785-787 同口径），
导致 CheckIndex 在 PForUtil.decode 解码到错位字节（AIOOBE）。已修复为以 `pos_start_fp` 初始化
level0/level1 基准。修复后验证（6 万行日志语料，词频真值由 grep 独立统计）：

| 验证项 | 结果 |
|---|---|
| CheckIndex（无 positions 索引） | No problems |
| CheckIndex（--positions 索引，含 .pos） | No problems |
| term connection / timeout / ERROR / 超时 | 161 / 161 / 14842 / 12 — 与 grep 真值一致 |
| and(connection, timeout) | 101 — 一致 |
| phrase("connection timeout") | 48 — 一致（命中行与注入模式完全吻合） |
| phrase 在无 positions 索引上 | 被 Lucene 正确拒绝（字段无 positions） |

## 5. 补记（2026-07-19 下午，二）：Rust/Java 同语料对比脚本

新增 `interop/compare-index.sh <输入> [文档数] [--positions]`（`make compare INPUT=... NDOCS=...`）：
同一语料分别用 Rust（`rustlucene-cli index`）和原生 Java Lucene（新工具 `interop/java/JavaIndex.java`，
与 Rust 完全同构——同样的文件遍历顺序、WhitespaceAnalyzer 分词、字段形状、`--docs` 循环语义、
单段单 flush、无 compound）各建一个索引，对比写入耗时/吞吐、CPU 时间、峰值内存（GNU time 采样）、
索引目录大小；随后双侧 CheckIndex，再用 `interop/java/CompareIndexes.java` 从词典随机抽 10%
（`SAMPLE_PCT` 可调）逐词 diff postings（docID 序列 + 每文档 freq）。

6 万行日志语料实测（`SAMPLE_PCT=100` 全量 diff，46 个词全部一致，0 mismatch）：

| 场景 | 吞吐 Rust / Java | 加速比 | 峰值内存 Java/Rust | 目录大小 Java/Rust |
|---|---|---|---|---|
| `--docs 120000`（循环 2 遍） | 789k / 108k docs/s | 7.3x | 9.2x | 1.02x |
| `--docs 70000 --positions` | 303k / 86k docs/s | 3.5x | 2.5x | 1.02x |
| 单遍 60000（默认） | 706k / 75k docs/s | 9.4x | 14.4x | 1.02x |

两个索引 maxDoc、词典大小、全部词的 postings 完全一致；CPU 总耗时 Rust 低一个数量级
（Java 含 JVM 启动/JIT/GC）。目录大小差 ~2%，主要来自 stored fields chunk 尺寸与元数据实现差异。
