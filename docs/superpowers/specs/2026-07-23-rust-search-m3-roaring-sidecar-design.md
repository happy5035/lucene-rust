# M3 设计：高 df term 的 Roaring bitmap sidecar

日期：2026-07-23。状态：已获用户批准（brainstorming 流程确认；关键分叉均经用户拍板：
独立 M3 立项、直接上 sidecar 写时构建、覆盖 df≥4096、三档执行规则含混合场景查询时物化）。
前置：M2（`2026-07-23-rust-search-m2-multiterm-phrase-design.md`）完成后启动。

## 1. 动机（已有定量数据）

M1 bench 已证实：高 df term 迭代瓶颈在 **decode CPU**（iterm 7.5–8.4 ns/doc vs Java 4.2–4.6，
--no-cache 口径），跳表只对 df 比 ≳128x 的 AND 有效。bitmap 路径把布尔运算变成容器级 SIMD
操作，比逐块 PFOR 解码 + 对齐快一个数量级以上；count = cardinality，O(1)。

参考：[阿里云 10 PB+/天日志系统](https://www.infoq.cn/article/wgiy55h3raqdy0ci-823)提到"混合
Bitmap 结构，直接在 encoding 后做 and/not/or，无需反序列化"——即 Roaring 思想（该文为架构
综述无算法细节；算法以 RoaringBitmap 论文为准：Chambi et al., *Better bitmap performance
with Roaring bitmaps*）。

## 2. 根本约束

- **postings 主格式不动**。bitmap 不带 freq/positions（phrase 必须要 .pos），postings 永远保留，
  bitmap 只是**附加结构**。
- sidecar 文件不被 segments_N 引用，Java CheckIndex 忽略孤儿文件——写侧 interop（Java
  CheckIndex/VerifyLogIndex）与 diff 电池不受影响。
- 项目根基"写字节兼容 Lucene 9.12.3 的索引"不变。

## 3. 范围

- 写侧：segment flush 时对 **df ≥ 4096**（对齐 level-1 skip 粒度 32×128；`--bitmap-threshold N`
  可调）的 term 构建 RoaringBitmap，写 per-segment sidecar 文件。实验期 `--bitmap` 默认 off。
- 读侧：Term/And/Or 按 §5 三档规则走 roaring 路径。
- **明确不做**：freq/positions；multi-term（M2 的 >16 bitset 路径）的 roaring 集成（二期，
  物化 helper 同源合流）；**纯低 df 布尔查询统一走 roaring**（明确否决——丢跳读红利、OR
  严格更多工作、M1 合取已调优，见 §5 档 3）；查询结果缓存；为 Java 写的索引提供 bitmap
  （无 sidecar 即自然落档 3）。

## 4. 写侧

- **构建时机**：term flush 时 docs 数组本就在内存（`write_doc_postings` 收全量切片），
  df≥4096 时顺手构建 roaring——零额外 IO，CPU 成本 O(df)。
- **容器语义**（照 Roaring 论文）：doc 按高 16 位分桶；桶内 cardinality < 4096 → array 容器
  （u16 有序数组），否则 bitset 容器（8KB = 65536 bits）；构建后 runOptimize（连续区间转
  run 容器；日志语料高 df term 大面积命中，run 收益大）。
- **sidecar 布局**（自定格式，无需跨实现兼容；magic/version/footer crc32 遵循项目
  CodecUtil 惯例）：header + field 分区表 + 每 field **有序 term 表**（term bytes +
  cardinality + payload offset/len；flush 天然字典序）+ payload 区。
  - cardinality 存 term 表 → **count 查询连 payload 都不用加载**。
  - term 表有序 → 读侧二分定位；前缀连续区间为将来 multi-term 集成留口。

## 5. 读侧：三档执行规则（用户拍板）

对 Term/And/Or 的每个 term 子句：df≥4096 且有 sidecar bitmap → bitmap 源；否则 → postings 源。
规则**按段独立生效**（M1 既定 per-segment 执行）：无 sidecar 的段（未开 --bitmap 写入、或
Java 写的索引）其自然落 postings 源。

1. **全部子句有 bitmap** → 纯 roaring 容器运算（主收益路径）。
2. **部分有** → 无 bitmap 的子句**查询时物化**成内存 roaring（DocsEnum 全扫置位；
   df<4096 故成本 ≤4095 doc ≈ 32 个 PFOR 块，有界微秒级），统一走 roaring 运算。
   ——用户拍板的简化：布尔执行只有一套引擎，不维护 membership-check 混合源。
3. **全无（纯低 df）** → 现有 PFOR 合取/析取，不动（M1 合取已调优，跳读红利保留）。

Term 单查询：有 bitmap → cardinality O(1) / 容器迭代；无 → PFOR（低 df 走 bitmap 无收益）。

- `RoaringDocIter` 实现 DocIter 协议：array 二分、bitset next_set_bit、run 区间跳。
- And：容器对分发——array∩array galloping、bitset∩bitset AVX2（512 bit/指令）+ popcount、
  run∩run 双指针；Or 对偶。结果仍是 roaring，直接迭代，不物化成数组。
- 惰性加载：open 只读 header + term 表索引（KB 级）；bitmap payload 首次触及才加载。

## 6. SIMD 纪律（照总 spec §4a）

先标量参考实现（容器交/并/popcount），AVX2 快路径作等价追加：
`is_x86_feature_detected!` 运行时分发 + "标量 vs SIMD 逐位相等"对拍单测 + bench 数据门槛。

## 7. 自研容器子集（不引 roaring crate）

三容器 + build/and/or/cardinality/iter，估 500–700 行含测试。项目依赖极精简
（crc32fast/lz4/rand/serde_json），且只需 4 个操作、SIMD 分发要自控，不引外部 crate。

## 8. 验证

- **Rust 对拍**：同一查询电池 bitmap on/off 结果逐位一致（单测 + log 语料）。
- **Java diff 终验**：`make log-test` 加 `--bitmap` 变体；Java CheckIndex 对带 sidecar 的
  索引必须仍 "No problems"。
- **bench（--no-cache）**：高 df AND/OR/count 三路对比（roaring vs PFOR vs Java）；
  同时量写侧吞吐损失与磁盘增量，出报告。预期高 df AND 提速一个数量级。

## 9. 实施顺序与工作量

M2 完成后启动。任务切分：

1. roaring 容器库（codec 或 core 新模块）
2. 写侧 sidecar 产出
3. 读侧 RoaringDocIter + Term 接入（档 1）
4. And/Or 接入 + 查询时物化 helper（档 1/2）
5. diff 电池变体 + bench 报告

| 部分 | 估计 |
|---|---|
| 容器库 | ~600 行 |
| 写侧 | ~200 行 |
| 读侧 | ~300 行 |
| 电池/bench | ~200 行 |
| **合计** | **~1.3k 行（含测试）** |
