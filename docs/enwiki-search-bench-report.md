# enwiki 查询性能优化报告：从 3–14x 落后到全面平价/反超 Java

日期：2026-07-29。范围：enwiki 130K docs 索引的 9 类查询 × low/med/high 共 27 桶，
Rust（rustlucene-core）vs Java Lucene 9.12.3。三轮优化（commit `da8006c`、`c9021c8`）
后，27 桶中 Rust 更快 14 桶、1.5x 以内 11 桶，最差 1.66x。根因均为热路径常数工程
（堆分配纪律、逐字节读取成本、路径选择），不涉及任何文件格式或架构变更。

## 1. 口径

- **索引**：`output/enwiki.txt`（130,215 docs，`--positions`），Rust 写侧产出 2 段
  （_0 大 / _1 小，无内联 bitmap）；Java 侧同语料自建索引（`/tmp/bench-java-idx`），
  两侧 `--no-cache` 等价口径。
- **查询集**：`/tmp/bench-queries-nobool.txt`（Java `SearchBench --dump-queries --tasks 50
  --seed 42`，906 条：TERM/AND/OR/PHRASE/PREFIX/WILDCARD 各 150 + TERMS 6），
  两侧逐条重放同一文件；统计 = 组内各 query qps/p50 平均，qps = 1e9/单 query 延迟中位数。
- **参数**：`--warmup 10 --iter 30`，串行。host：4 vCPU / 3GB VM（PMU 不可用，
  归因用全局分配计数器 + 微基准替代 perf），rustc 1.97.1，OpenJDK 21.0.11。
- **正确性**：每轮改动后 606 条 per-query hit-count 与基线逐字节一致；430 单测全绿
  （codec 210 / core 163 / jni 10 / metric 47）。

## 2. 最终结果（p50 µs，同轮复测）

| 分组 | Rust 基线¹ | **Rust 最终** | Java | 最终/Java | 基线/Java |
|---|---|---|---|---|---|
| term low | 31.5 | **3.0** | 6.9 | **0.43x** | 4.6x |
| term med | 28.2 | **3.0** | 6.5 | **0.46x** | 4.3x |
| term high | 24.1 | **2.7** | 5.7 | **0.47x** | 4.2x |
| iterm low | 29.6 | **3.4** | 4.4 | **0.77x** | 6.7x |
| iterm med | 30.4 | **3.9** | 4.4 | **0.89x** | 6.9x |
| iterm high | 45.3 | **19.1** | 26.1 | **0.73x** | 1.7x |
| and low | 85.1 | **9.4** | 26.3 | **0.36x** | 3.2x |
| and med | 94.0 | **11.3** | 20.7 | **0.55x** | 4.5x |
| and high | 186.8 | **76.1** | 72.0 | 1.06x | 2.6x |
| or low | 104.2 | **12.2** | 21.8 | **0.56x** | 4.8x |
| or med | 116.5 | **14.7** | 19.2 | **0.77x** | 6.1x |
| or high | 253.6 | **141.2** | 94.2 | 1.50x | 2.7x |
| phrase low | 59.8 | **6.9** | 14.5 | **0.48x** | 4.1x |
| phrase med | 69.3 | **8.9** | 10.9 | **0.82x** | 6.4x |
| phrase high | 206.7 | **132.2** | 117.5 | 1.13x | 1.8x |
| prefix low | 3661.9 | **642.3** | 561.1 | 1.14x | 6.5x |
| prefix med | 1790.6 | **349.9** | 248.9 | 1.41x | 7.2x |
| prefix high | 1278.4 | **296.1** | 441.0 | **0.67x** | 2.9x |
| wildcard low | 45991.7 | **12481.8** | 9908.7 | 1.26x | 4.6x |
| wildcard med | 3483.1 | **736.0** | 700.5 | 1.05x | 5.0x |
| wildcard high | 10299.7 | **2238.6** | 2288.4 | **0.98x** | 4.5x |
| terms low | 182.4 | **19.0** | 17.6 | 1.08x | 10.4x |
| terms med | 294.3 | **32.1** | 19.4 | 1.66x | 15.2x |
| terms high | 528.7 | **106.2** | 92.4 | 1.15x | 5.7x |
| termsbig low | 827.9 | **82.3** | 78.2 | 1.05x | 10.6x |
| termsbig med | 1119.1 | **146.0** | 95.4 | 1.53x | 11.7x |
| termsbig high | 1984.3 | **1248.0** | 1146.5 | 1.09x | 1.7x |

¹ 基线 = HEAD `3dc72a8`（mmap + FST 自动机交叉已合入、本轮优化前）同口径实测
（`/tmp/bench-rust-head.txt`）；Java 数字为 7-29 同轮复测（`/tmp/final-java.txt`）。

**汇总**：Rust 更快 14 桶（加粗），1.5x 以内 11 桶，最差 terms med 1.66x。

## 3. 优化历程与根因

### 3.0 归因方法

PMU/perf 被宿主禁用（perf_event_paranoid=4、ptrace 禁用），改用**全局计数分配器
微基准**（`crates/core/examples/bench_seek_decomp.rs`、`bench_wildcard_decomp.rs`）：
对每类操作同时记录墙钟与堆分配次数，直接锁定病根。

### 3.1 第一轮：词典查找路径去分配（`da8006c` 之一）

**实测**：一次 term count 查询 = 纯词典 lookup，**474 次堆分配 / p50 33.9µs**；
Java 同操作（含 harness）7–8.4µs、零分配。474 次 malloc/free ≈ 15–25µs，即差距主体。

来源：FST 读节点时**每个 arc 的 output 各 `to_vec()` 一次**（宽根节点 50+ arc）；
`trace_path` 每个 final 帧 clone 累积输出；`seek_exact` 再 clone；`scan_block`
每块 4 个 Vec + 3 个 `IndexInput` 包装。

修复（fst.rs / terms_read.rs / io.rs）：

- arc output → 24B 内联 `OutBuf`（不再每 arc 一个 Vec）；
- final output（floor 数据，可达 ~200B）→ 懒解码 span，只有命中 arc 才还原；
- 新增 `trace_deepest`：seek_exact 只需最深帧，不再逐帧 clone；
- `scan_block` 四个 blob 缓冲改 `TermsDict` 常驻 scratch；
- 新增 `SliceInput`：借用切片的零拷贝 `DataInput`，取代 `to_vec() + IndexInput::in_memory`。

**效果**：lookup **474 → 2 分配、33.9µs → 8.0µs**；term 桶 3.4–3.8x → 0.88–0.94x（反超）。

### 3.2 第二轮：扫描路径对齐 Java 设计 + enum 创建成本（`da8006c` 之二）

**实测**：wildcard low 被单条病态查询 `*[[+965*`（全字典扫描、命中 3 docs）主导：
**575ms / 1,200,652 次分配**。

对照 Lucene 9.12.3 源码（`lucene90/blocktree/IntersectTermsEnum.java` +
`SegmentTermsEnumFrame.java`）确认两个结构性差异：

1. **Java 对拒绝的 term 不解码 stats/meta**（`Frame.next()` 只推进 suffix 流，
   `decodeMetaData` 用 metaDataUpto catch-up 且只补到被接受的 term）；Rust 的
   `next_frame_entry` 对每个 term 全量解码（~25 字节 + 状态算术）。
2. Java 帧对象预分配复用；Rust 每个 sub-block push/pop 新帧（每帧 ~10 次分配：
   4 个块 blob Vec + 3 个 `Arc<IndexInput>`），全扫描 ~120 万次分配。

修复：

- `IterFrame` 三条块流改 `ByteCursor`（常驻缓冲 + 位置，无 Arc、无 enum 分派）；
- 拆 `next_frame_suffix` / `decode_frame_term` 实现懒解码（`term_ord` 只计 term
  条目——sub-block 条目无 stats/meta，对齐 Java `termBlockOrd`）；
- `IntersectTermsEnum` 帧池：pop 进池、push 复用缓冲。

**附带发现**：`IndexInput` 内联 `buffer: [u8; 8192]` 在每次 enum/slice 创建时
零初始化 8KB——mmap/内存源根本不经过该 buffer。改懒初始化（File 源才分配）。

**效果**：`*[[+965*` **575ms → 153ms、1.2M → 626 分配**；wildcard low 桶
4.5x → 1.2x；8KB memset 修复外溢提升所有枚举密集路径（prefix、terms 物化等）。

### 3.3 第三轮：FST 流式 arc 扫描 + 多词项 count 路径（`c9021c8`）

**实测**：terms/termsbig 由 N 次词典查找主导，而查找仍慢 Java ~2x：FST 每访问
节点解包全部 arc（`Vec<FstArc>`），Java 是流式扫描遇 `label >= target` 即停。

修复：

- 新增 `find_arc_in_node`：流式 findTargetArc，跳过 arc 不解码 output/target，
  命中即返回 span；`trace_deepest`/`trace_path` 切换。**lookup 8.0µs → 3.5µs、0 分配**
  （起点 33.9µs / 474 分配）。此项外溢提升全部 lookup 相关桶。
- 删除 `CollectedTerms.terms`（term 字节收集了但无消费者，每匹配 term 白付一个 Vec）。
- **terms high 病理**：`TERMS total,special,M.,even`（4 词，df 2,251/1,813/2,421/4,985，
  OR 命中 11,140）。count 语义下走析取归并，kway_union 每产出 doc 两遍扫描 4 子游标
  且每次比较重构切片（~28ns/doc；Java DisjunctionScorer ~8.4ns/doc）。而同命中量级
  的 bitset 物化路径仅 ~5ns/doc + ~20µs 固定成本。修复：`bitset_count` 加 df 总和
  阈值（`BITSET_COUNT_MIN_DF_SUM = 2048`），小 df 仍走析取（免 16KB bitset 固定成本）。
  **terms high 315µs → 104µs**。

## 4. 剩余差距分析

| 桶 | 比值 | 原因 |
|---|---|---|
| terms med | 1.66x | 2–3 词、百级命中的小结果集：析取归并常数项 vs bitset 固定成本之间，已近局部最优 |
| termsbig med | 1.53x | 29 词 × 中 df：N 次查找已平价，差距在逐词 postings 扫描的解码常数 |
| or high | 1.50x | 2 词高 df 析取：PFOR 解码常数项（M1 起已知 ~1.7x），JIT 向量化差距 |
| prefix med | 1.41x | TermsIter 逐 term 物化 + 逐词 enum 扫描；Java DocIdSetBuilder 流式直灌 |
| wildcard low | 1.26x | 全字典扫描残余 per-term 成本（suffix vint + DFA 逐字节），已同构于 Java |

共性：剩余差距全部是**解码/合并循环的常数项**（边界检查、逐字节 vlong、无 JIT
向量化），不再存在结构性/算法性差距。继续压缩的方向：vlong 解码的查表/批量变体、
PGO、or/and high 的块级 intersect/union 内核向量化。预期单项收益 10–40%，优先级递减。
（原首项"postings 块解码批量接口直供 bitset"已于 2026-07-30 落地，见 §4a。）

## 4a. 第四轮：postings 块解码直供物化（2026-07-30）

落实 §4 预告的首个方向。改动（`multi_term.rs` / `doc_iter.rs`，无格式变更）：

- `for_each_doc` 由逐 doc `next_doc` 改为 `next_block` 驱动（codec `next_docs`
  128-doc 窗口直供），物化路径从 ~7.4ns/doc 压向 2–3ns/doc 解码下限；
  bitset 物化与 tier-2 roaring 物化同路受益，所有消费方签名零改动。
- kway 归并（Disjunction/RoaringOr/DisjOver 三处）每轮 `heads`/`consumed`
  两次堆分配 → 栈数组快照（`kway_union_curs`，k ≤ 32 走 slice 内核，
  大 k 退化游标直读）。初版游标直读循环在 or high 回退 ~5%（每 doc 2k 次
  Box 追随 > 省下的分配），栈数组版在交替 A/B 中确认无回退。

结果（p50，counts 与基线逐字节一致，测试 431 全绿）：

| 桶 | 130K 旧 → 新 | 130K vs Java | 5M 旧 → 新 | 5M vs Java |
|---|---|---|---|---|
| terms high | 104 → 72µs（1.44x） | 1.12x → **0.81x** | 23.8ms → 13.9ms（1.71x） | 1.29x → **0.75x** |
| termsbig high | 1234 → 767µs（1.61x） | 1.07x → **0.70x** | 21.7ms → 12.4ms（1.75x） | 1.24x → **0.71x** |
| or high | 147 → 142µs（持平） | 1.50x → 1.55x | 3.58ms → 3.54ms（持平） | 2.02x → 2.00x |

130K 反超 14 → 15 桶（>1.5x 仅剩 or high 1.55 / prefix med 1.55 / terms med 1.58）；
5M 反超 17 → 19 桶。剩余落后桶不变：or/and/phrase high（百万级命中解码常数）、
termsbig low、wildcard low。

产物：`/tmp/ab-{old,new}-{1,2}.txt`（交替 A/B）、`/tmp/n5m2-rust.txt`、
`/tmp/n2-rust.txt`；counts 对拍 `/tmp/n5m2-counts.txt`、`/tmp/n2-counts.txt`。

## 5. 复现命令

```bash
# Rust（查询文件与索引同 M 系报告口径）
./target/release/rustlucene-cli searchbench /tmp/bench-rust-idx message \
  --load-queries /tmp/bench-queries-nobool.txt --tasks 50 --warmup 10 --iter 30 --seed 42
# Java
CP="interop/java/classes:interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar"
java -Xmx1g -cp "$CP" SearchBench /tmp/bench-java-idx message \
  --load-queries /tmp/bench-queries-nobool.txt --no-cache --tasks 50 --warmup 10 --iter 30 --seed 42
# 分配计数微基准（归因工具，未跟踪）
cargo build --release -p rustlucene-core --example bench_seek_decomp
./target/release/examples/bench_seek_decomp /tmp/bench-rust-idx /tmp/q-term-only.txt 20 50
# hit-count 对拍：两侧 stderr 的 detail 行 sort 后 diff（本报告口径逐字节一致）
```

产物：`/tmp/final-rust.txt`、`/tmp/final-java.txt`（同轮复测原始表）、
`/tmp/bench-rust-head.txt`（基线）、`/tmp/c-rust*.sorted`（counts 对拍）。
