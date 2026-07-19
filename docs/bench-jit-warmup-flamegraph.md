# Rust vs Java 索引写入 CPU 对比：JIT 预热效应 + 火焰图分析

日期：2026-07-19。目的：量化 Java（Lucene 9.12.3）与 Rust（rustlucene-core）在**相同语料、相同文档形状、
相同线程数**下的索引写入吞吐与 CPU 消耗差异，用火焰图定位差异来源，并测量 Java JIT 预热的实际收益。

## 1. 实验设置

- 机器：4 vCPU / 3 GB RAM 虚拟机（KVM，**硬件 PMU 不可用**，见 §6 注意事项）
- JDK：OpenJDK 21.0.11（G1 GC，`-Xmx2g`）；Rust：rustc 1.97.1，`--release`
- 负载：500,000 篇文档 × 200 字节 message 字段（WhitespaceAnalyzer，DOCS_AND_FREQS + omitNorms + stored），
  4 线程，固定 seed=42。双侧语料生成器为同一种 xorshift64* 算法，**生成内容逐字节一致**
- Java 侧程序：新增 `interop/java/JavaWarmupBench.java`，在**同一 JVM 进程内**跑多轮（每轮全新索引目录），
  让 HotSpot 在测量轮之前完成分层编译
- Rust 侧程序：`rustlucene-cli bench`（每轮独立进程）
- 采样：Java 用 JFR `jdk.ExecutionSample`（settings=profile）；Rust 用 `perf record -e cpu-clock -g --call-graph dwarf`
  （软件时钟事件）。火焰图由 `bench/flametool.py` 生成，产物在 `bench/results/`

## 2. 吞吐与预热效应

| 轮次（同进程） | Java docs/s | Rust docs/s（独立进程） |
|---|---|---|
| 第 1 轮（冷） | 231,589 ~ 241,429 | 1,091,703 |
| 第 2 轮 | 260,552 | 1,111,111 |
| 第 3 轮 | 344,590 | 1,101,322 |
| 第 4 轮 | 331,126 | 1,070,664 |

- **Java JIT 预热收益 ≈ +45~49%**（231k → 344k docs/s），第 3 轮达到稳态。生产上这意味着 Java 索引进程
  启动后的前几分钟（或前几十 GB 数据）处于"半速"状态；压测 Java 必须进程内预热，否则结论失真一倍。
- **Rust 无预热效应**：四轮方差 < 4%，AOT 编译首轮即峰值。Rust 世界里与 JIT 对应的优化手段是 PGO
  （profile-guided optimization），本次未启用——也就是说 3.2x 的差距还有压缩空间。
- **稳态对比：Rust ≈ 3.2× Java（1.10M vs 0.34M docs/s）；对冷启动 Java ≈ 4.7×。**

## 3. CPU 消耗对比（/usr/bin/time 口径）

| | user CPU | sys CPU | 说明 |
|---|---|---|---|
| Rust（500k 文档，单次进程） | 1.28 s | 0.22 s | ≈ 3.0 µs CPU/篇 |
| Java（JVM 全程，4 轮共 200 万篇） | 26.34 s | 0.48 s | 含 JVM 启动、JIT 编译线程、GC |
| Java 测量轮折算（~1.6 s 墙钟 × ~3.0 CPU 占用） | ≈ 4.7 s/500k | — | ≈ 9.4 µs CPU/篇 |

**单位 CPU 成本比 ≈ 3.1x**，与吞吐比一致——差距是真实计算量差异，不是并行度差异（两侧都是 4 线程、
CPU 占用都 ~300%+）。Java 侧另有不在 `user` 里体现的开销：JFR 记录本次运行 4 轮共 51 次 GC 暂停
（约 100 ms STW）+ GC 并行阶段约 170 ms 的工作线程 CPU，以及 C1/C2 编译线程本身的 CPU。

## 4. 火焰图热点对比

火焰图：`bench/results/java-flame.svg`、`bench/results/rust-flame.svg`（浏览器打开，可点击下钻）。
按 inclusive 样本占比归类（Java 1855 个样本 / Rust 1538 个样本）：

| 阶段 | Java | Rust |
|---|---|---|
| 语料生成（压测自带开销，双侧同算法） | 15.2% | 10.9% |
| postings 构建（分词 → 倒排） | **46.4%** + UTF16→UTF8 12.5% + 分词器 14.2% | **52.0%** |
| stored fields LZ4 压缩 | 14.0% | 15.6% |
| 内存管理（malloc/realloc/madvise） | —（被 GC 掩盖，见下） | 11.4% |
| finalize/flush IO | 0.4% | 9.4% |

### Java 侧 CPU 去哪了

1. **倒排链 `IndexingChain` 占 46%**：`CharTokenizer.incrementToken`（10.2% self）、`BytesRefHash.findHash`
   （7.3%）、`TermsHashPerField.writeVInt`（6.6%）——token 逐个经 analyzer 链传递，每 token 多次虚调用。
2. **UTF-16 → UTF-8 转码 12.5% inclusive**（`UnicodeUtil.UTF16toUTF8`，内含 `String.charAt` 7.7% self）：
   Java String 是 UTF-16，写入前必须整串转码；**Rust 字符串原生 UTF-8，这项成本为零**。
3. **对象分配与 GC**：每篇文档产生 `Document`/`Field`/`String` 等短生命周期对象
   （`AbstractStringBuilder.ensureCapacityInternal` 6.6% self 可见一斑），4 轮共 51 次 GC。
4. LZ4 stored 压缩 12.5%（`LZ4WithPresetDictCompressionMode`）——与 Rust 侧同为原生 LZ4 算法，占比接近，
   说明这一项是格式固有的不可省成本。

### Rust 侧 CPU 去哪了

1. `SegmentBuilder::add_document` 52%（内含 `TermDict::lookup_or_insert_flag` 18.5% self——
   FST 前的内存词表哈希查找），路径短、无虚调用、无转码。
2. LZ4 15.6%（与 Java 相同的算法本质，走原生 `lz4` 绑定）。
3. **内存管理 11.4%**（`realloc`/`madvise`/`mprotect`/`RawVec::grow_one`）：postings buffer 增长拷贝。
   这是 Rust 侧最值得优化的一项——预分配或 slab 化可再挤出几个百分点。
4. 无 GC、无 JIT 编译线程、无转码层。

### 一句话结论

两者在"真正的倒排构建"上都花约一半 CPU，差距主要来自 Java 的**伴生开销**：UTF-16 转码（~12%）、
分词器虚调用链、对象分配 + GC + JIT 编译线程；Rust 把这些全部消掉了，所以单位文档 CPU 约为 Java 的 1/3。

## 5. 对生产部署的含义

- Java 索引进程：**必须预热**（warmup 写入或压测时丢弃前几轮），否则 JIT 未就绪时吞吐打 5~7 折。
  长稳态下 JIT 能追回一部分差距（本轮 +49%），但追不平内存模型与转码的结构性成本。
- Rust 侧无需预热，冷启动即满速，适合短生命周期/弹性伸缩的写入 worker。
- 若 Java 侧必须保留，缩小差距的方向：堆外 buffer、复用 Document/Field 实例、换更轻的分词器——
  但都改不掉 UTF-16 转码和 GC。

## 6. 注意事项与口径声明

- 本机为虚拟机，硬件 PMU 计数器不可用（`perf stat` 的 cycles/instructions 输出为垃圾值），
  故 CPU 对比采用 `/usr/bin/time` 的 user/sys 时间 + 软件时钟采样火焰图，未使用 IPC 类指标。
- 吞吐数字含语料生成开销（双侧同一算法、同一 seed，公平对冲；Java 15.2% vs Rust 10.9%）。
- Rust 火焰图二进制为 legacy 符号重建（stable 工具链默认 v0 混淆名不便离线 demangle），
  与性能数据所用的 release 构建除符号表外完全一致。
- 复现：`bench/run-bench.sh`（无预热对比）、`java JavaWarmupBench <dir> 500000 200 4 42 3`（预热实验）、
  `bench/flametool.py`（火焰图生成，依赖 `jfr`/`perf`/`c++filt`）。
