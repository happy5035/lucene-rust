# M3 设计：高 df term 的 Roaring bitmap（.doc 内联）

日期：2026-07-23。状态：已获用户批准（brainstorming 流程确认；关键分叉均经用户拍板：
独立 M3 立项、直接上写时构建、覆盖 df≥4096、三档执行规则含混合场景查询时物化）。
前置：M2（`2026-07-23-rust-search-m2-multiterm-phrase-design.md`）完成后启动。
**2026-07-24 重大修订（用户拍板）**：从独立 sidecar 文件改为 **.doc 内联方案**——bitmap
写在每个 term postings 之前、由 FST 现有 docStartFP 定位（§4/§4a 重写）。原 sidecar
文件契约（命名禁区/Java 删除器交互/孤儿 GC）整体作废，历史版本见 git 记录。

## 1. 动机（已有定量数据）

M1 bench 已证实：高 df term 迭代瓶颈在 **decode CPU**（iterm 7.5–8.4 ns/doc vs Java 4.2–4.6，
--no-cache 口径），跳表只对 df 比 ≳128x 的 AND 有效。bitmap 路径把布尔运算变成容器级 SIMD
操作，比逐块 PFOR 解码 + 对齐快一个数量级以上；count = cardinality，O(1)。

参考：[阿里云 10 PB+/天日志系统](https://www.infoq.cn/article/wgiy55h3raqdy0ci-823)提到"混合
Bitmap 结构，直接在 encoding 后做 and/not/or，无需反序列化"——即 Roaring 思想（该文为架构
综述无算法细节；算法以 RoaringBitmap 论文为准：Chambi et al., *Better bitmap performance
with Roaring bitmaps*）。

## 2. 根本约束

- **postings 主格式不动**。bitmap 不带 freq/positions（phrase 必须要 .pos），postings 永远
  保留，bitmap 只是**附加字节**。
- **不新增任何文件**：bitmap 内联在 `.doc` 流内、每个 term 的 postings 之前（兼容性论证
  见 §4a）。Java 读写该索引零感知，CheckIndex 仍 "No problems"，写侧 interop 与 diff
  电池不受影响。
- 项目根基"写字节兼容 Lucene 9.12.3 的索引"不变。

## 3. 范围

- 写侧：segment flush 时对 **df ≥ 4096**（对齐 level-1 skip 粒度 32×128；`--bitmap-threshold N`
  可调）的 term 构建 RoaringBitmap，内联写入 `.doc`（postings 之前）。实验期 `--bitmap`
  默认 off。
- 读侧：Term/And/Or 按 §5 三档规则走 roaring 路径。
- **明确不做**：freq/positions；multi-term（M2 的 >16 bitset 路径）的 roaring 集成（二期，
  物化 helper 同源合流）；**纯低 df 布尔查询统一走 roaring**（明确否决——丢跳读红利、OR
  严格更多工作、M1 合取已调优，见 §5 档 3）；查询结果缓存；为 Java 写的索引提供 bitmap
  （无内联 bitmap 即自然落档 3）。

## 4. 写侧

- **构建时机**：term flush 时 docs 数组本就在内存（`write_doc_postings` 收全量切片），
  df≥4096 时顺手构建 roaring——零额外 IO，CPU 成本 O(df)。
- **容器语义**（照 Roaring 论文）：doc 按高 16 位分桶；桶内 cardinality < 4096 → array 容器
  （u16 有序数组），否则 bitset 容器（8KB = 65536 bits）；构建后 runOptimize（连续区间转
  run 容器；日志语料高 df term 大面积命中，run 收益大）。
- **内联布局**：对每个符合条件的 term，在 capture docStartFP **之前**往 docOut 依次写
  `[bitmap 头 + payload + crc32][len: 定长 4 字节 LE]`，然后照常写 postings——docStartFP
  指向 postings 起点，FST output 编码不变。
  - bitmap 头：`magic(4B) + version(1B) + df(vInt) + cardinality(vInt)` → **count 查询只读头**。
  - len = 头+payload+crc32 总字节数 → 读侧 `docStartFP-4` 取 len，回退 len 字节即 bitmap 区。
- **term 如何找到 bitmap（用户两轮拍板的最终答案）**：BlockTree FST output 是 Java 端固定
  schema（docStartFP/posStartFP/…，`FieldReader.readVLongOutput` 按固定格式解码），改它即
  破坏 Java 兼容；但**无需改**——docStartFP 本身就成了 bitmap 的定位器（bitmap 紧邻其前，
  `docStartFP-4-len` 处）。sidecar 方案的"有序 term 表 + 二分"整个不需要了。
- **无 bitmap 判定**：Java 写的索引 / `--bitmap` off / 未达门槛的 term，`docStartFP-4` 处是
  上一 term postings 的任意字节。四重校验：len 有界（`0 < len ≤ 上限`，上限按 maxDoc 推算）
  → magic 匹配 → `df == termState.df` → crc32 匹配。误判概率实际为零，任一失败**静默落档
  postings**，查询永不报错。

## 4a. 兼容性论证（2026-07-24，Lucene 9.12.3 源码核实）

**问题**：往 `.doc` 流里 term 之间塞字节，Java 会不会读坏？
**答案**：不会——postings reader 是纯 seek 式的，term 间缝隙字节对 Java 完全隐形：

1. **纯 seek 访问**。读 term 永远 `docIn.seek(termState.docStartFP)` 起手
   （`Lucene912PostingsReader.java:436,809`），skip 导航也 seek 到算好的 fp
   （`level1DocEndFP`/`skip0EndFP`/`blockEndFP`，:526,559,997），读完 df 个 doc 即停——
   **从不顺序扫描，从不假设"term N+1 起点 == term N 结尾"**。bitmap 位于 docStartFP
   之前的缝隙，永不被 Java 触及。
2. **CRC 有效**。footer CRC 覆盖全流（`CodecUtil.java:402-413`）；bitmap 在流内、footer
   之前经同一 CRC 输出流写入，checkIntegrity 照常通过。
3. **CheckIndex 零感知**：term 枚举与 postings 抽查全走 seek 路径，目录零新增文件 →
   "No problems were detected"（`CheckIndex.java:916-917`；电池实证）。
4. **merge 安全**。Java merge 经 PostingsEnum 逐 doc 重编码 postings → 新 segment 无
   bitmap、自然落档（§5），无孤儿字节；CFS 打包 / addIndexes 是字节级拷贝 → bitmap
   原样存活。Rust 自身无 merge，无 GC 问题。
5. **认账**：这依赖"reader 纯 seek、不假设连续"这一实现属性而非格式承诺。我们锁死
   9.12.3，行为已源码逐条核实，并由 `make log-test` 的 Java CheckIndex、Java↔Rust
   对拍、Java forceMerge 三道实证钉死（§8）。

## 5. 读侧：三档执行规则（用户拍板）

对 Term/And/Or 的每个 term 子句：df≥4096 且内联 bitmap 校验通过 → bitmap 源；否则 →
postings 源。规则**按段独立生效**（M1 既定 per-segment 执行）：无 bitmap 的段（未开
--bitmap 写入、Java 写的索引、Java merge 产物）自然落 postings 源。

1. **全部子句有 bitmap** → 纯 roaring 容器运算（主收益路径）。
2. **部分有** → 无 bitmap 的子句**查询时物化**成内存 roaring（DocsEnum 全扫置位；
   df<4096 故成本 ≤4095 doc ≈ 32 个 PFOR 块，有界微秒级），统一走 roaring 运算。
   ——用户拍板的简化：布尔执行只有一套引擎，不维护 membership-check 混合源。
3. **全无（纯低 df）** → 现有 PFOR 合取/析取，不动（M1 合取已调优，跳读红利保留）。

Term 单查询：有 bitmap → cardinality O(1)（只读 bitmap 头）/ 容器迭代；无 → PFOR（低 df
走 bitmap 无收益）。**freq/freq_sum 永远走 postings**（bitmap 无 freq）。

- `RoaringDocIter` 实现 DocIter 协议：array 二分、bitset next_set_bit、run 区间跳。
- And：容器对分发——array∩array galloping、bitset∩bitset AVX2（512 bit/指令）+ popcount、
  run∩run 双指针；Or 对偶。结果仍是 roaring，直接迭代，不物化成数组。
- 惰性加载：open 不读任何 bitmap；term 首次触及才读（len+头一次小读，payload 按需）。

## 6. SIMD 纪律（照总 spec §4a）

先标量参考实现（容器交/并/popcount），AVX2 快路径作等价追加：
`is_x86_feature_detected!` 运行时分发 + "标量 vs SIMD 逐位相等"对拍单测 + bench 数据门槛。

## 7. 自研容器子集（不引 roaring crate）

三容器 + build/and/or/cardinality/iter，估 500–700 行含测试。项目依赖极精简
（crc32fast/lz4/rand/serde_json），且只需 4 个操作、SIMD 分发要自控，不引外部 crate。
**落点 codec-lucene9**：core 是 `#![forbid(unsafe_code)]` 放不下 AVX2 intrinsics；codec
有 `postings_ll/simd.rs` 模块级 allow 先例，容器库与内联读写 helper 都放 codec，读侧集成
在 core/search。

## 8. 验证

- **Rust 对拍**：同一查询电池 bitmap on/off 结果逐位一致（单测 + log 语料）。
- **Java 兼容终验**：`make log-test` 加 `--bitmap` 变体——Java CheckIndex 必须仍
  "No problems"；Java↔Rust 搜索对拍 diff 为空；**新增 Java forceMerge 该索引后再对拍**
  （merge 路径实证：merge 产物无 bitmap 自然落档，结果必须不变）。
- **bench（--no-cache）**：高 df AND/OR/count 三路对比（roaring vs PFOR vs Java）；
  同时量写侧吞吐损失与磁盘增量，出报告。预期高 df AND 提速一个数量级。

## 9. 实施顺序与工作量

M2 完成后启动。任务切分：

1. roaring 容器库（codec-lucene9 新模块 `roaring.rs` + `roaring/simd.rs`）
2. 写侧 inline bitmap 产出（docOut 前置写 + len 尾缀；`--bitmap` / `--bitmap-threshold`）
3. 读侧 bitmap 定位/校验 + RoaringDocIter + Term 接入（档 1）
4. And/Or 接入 + 查询时物化 helper（档 1/2）
5. diff 电池变体（含 forceMerge 项）+ bench 报告

| 部分 | 估计 |
|---|---|
| 容器库 | ~600 行 |
| 写侧 | ~150 行 |
| 读侧 | ~300 行 |
| 电池/bench | ~200 行 |
| **合计** | **~1.25k 行（含测试）** |
