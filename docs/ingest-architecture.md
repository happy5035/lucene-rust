# 多日志流不均衡写入：分区调度 ingest 架构（32C/64G 节点）

日期：2026-07-19。状态：设计文档（未实现）。回答的问题：单节点 32 核 / 64GB，多个日志流
按小时切分、每小时多 shard，流量高度不均衡时，RustLucene 应采用什么写入架构与线程模型；
与 Java（线程池 + 批量缓存摊薄锁竞争）的路线对比。

## 1. 为什么不能沿用 bench 的静态切分

现有 CLI bench（`rustlucene-cli bench/logbench`）是**静态切分**：`num_docs / threads`，每线程
一个私有 `SegmentBuilder`，语料均匀时近线性扩展（4 核 3.3x）。但流量倾斜时静态绑定
"分区→线程"必然出现热点线程拖尾：大流量流的 builder 打满一个核，小流量流的线程空转，
总吞吐被最热线程封顶。倾斜随时间漂移时静态重分也来不及。

约束（来自 Lucene 段语义，也是现状代码的结构事实）：

- `SegmentBuilder`/`DocWriter` 单线程独占：docID 递增赋值、postings 平行数组升序追加，
  段内索引本质串行。**段级分片是唯一干净的并行单位**。
- `IndexWriter` 一个目录一个提交域（两段式 `segments_N`）；`commit_segments` 可把多个
  builder 的段并成一个提交点。
- 每个 builder 自带 O(1) 增量 RAM 记账（`ram_bytes()`），可直接做预算调度。

## 2. 分区模型

**分区 partition =（stream, hour_bucket, shard_id）→ 一个独立索引目录**（等价 ES shard）。

- 提交域互相独立：各自两段式提交，互不惊扰；
- 按小时保留/删除 = 删目录；故障爆炸半径最小；
- 同一热流的多个 shard 提供流内并行度（shard 数建议 2–4/流/小时，按该流峰值速率配）。

## 3. 线程模型：分区 actor + 微批调度（索引路径零锁）

```
producers                registry                    workers (32)
 stream 1 ──┐          ┌──────────────┐        ┌────────────────────┐
 stream 2 ──┼─ push ──▶│ Partition A  │◀──────▶│  pop token         │
   ...      │ (bounded)│  queue[8k]   │  take/ │  drain micro-batch │
 stream N ──┘          │  state slot  │ return │  return/requeue    │
                       └──────────────┘        └────────────────────┘
                              ▲            ready queue (MPMC, tokens)
                       token requeue if backlog
```

- **32 个 worker 线程**（= 物理核数）共消费**一个全局 MPMC ready 队列**；队列里放的是
  **分区令牌**，不是文档。
- 每个分区 = **有界 MPSC 输入队列**（4k–16k docs）+ `Option<Box<PartitionState>>` 槽位，
  `PartitionState` 持有该分区的 `SegmentBuilder`（含全部 postings/DV/points RAM 缓冲）。
- worker 循环：弹令牌 → `take` 出 PartitionState（**builder 所有权随令牌移动，索引路径
  无任何锁**）→ 从该分区队列 drain 一个**微批**（min(积压, K=512–2048) 条或 5ms 上限）
  喂给 builder → 归还 PartitionState；仍有积压则令牌重新入 ready 队，否则挂起分区
  （首条文档到达时由生产者重新激活）。
- **倾斜吸收**：热分区令牌更频繁地重入队，worker 时间按队列深度自动流向大流量流；
  单一 ready 队列天然 work-conserving，无需静态绑定、无需 work-stealing 特例。
- **背压**：分区队列满 → 生产者阻塞或被拒（等价 ES 的 429）。
- **微批 = Java 的批量缓存**：令牌交接摊薄到 ~µs/doc，与 bulk 摊薄锁竞争同效，但段内
  索引完全没有锁——`DocWriter.add_document` 热路径与单线程 bench 完全一致
  （M1/M2 实测 388k/182k docs/s/核 不变）。

被拒绝的备选：Java 式共享写入器 + 条带锁（同一 DocWriter 内按字段/词加锁分区）。
docID 赋值与 postings 追加在段内本质串行，锁分区只省字典不省追加，收益小复杂度高。

### 3.1 三个关键澄清

1. **独占但不绑定**：一个分区（shard 目录）同一时刻只有一个 worker 持有 builder
   （所有权排他 = 无锁前提）；但每个微批结束所有权即归还，下一个微批可由任意空闲
   worker 接手——不是"分区绑线程"。
2. **线程时间是自适应分配的**：令牌重入队机制使 worker 的 wall-clock 时间按各分区
   积压深度成比例自动分配；冷分区令牌挂起、零 CPU 消耗；倾斜随时间漂移也无需重配。
3. **段数与线程数无关**：单 builder 模型下，一个分区的段数 = flush 次数（RAM 触发 /
   周期提交 / 小时封存）。多个线程先后经手同一分区**不会**产生额外段。
   - 边界情况：代码库本身支持一个目录多 builder（bench 的 8 线程 8 段 +
     `commit_segments` 联合提交，CheckIndex `numSegments=8` 验证过）。那才是
     "单 shard 多线程 → 多段"模式：写更快但段更多、读侧代价高。
   - 结论：正常靠每小时多 shard 分摊流量，把单 shard 速率配在单核吞吐
     （日志 schema ~180k docs/s）以内；仅当单流尖峰超过单核时才启用多 builder 模式。

## 4. 内存控制：64GB 预算制

- 全局索引 RAM 预算 **32GB**；其余留给 OS page cache（stored fields 流式写盘本就靠
  page cache 吸收 flush 突发）。无 JVM 堆，不需要 ES 式 Xmx 对半切。
- 复用现有 O(1) 记账：各 builder `ram_bytes()` 增量汇总到全局 `AtomicUsize`
  （近似值，与 Lucene 同口径）。
- **两级 flush**：
  1. 分区级：`max_ram_bytes`（256–512MB）或 `max_buffered_docs` 触发；
  2. 全局压力 >80%：按"RAM 最大者优先、封存/冷分区永远最先"选 flush 对象。
  flush 排他复用令牌机制（分区标记 flushing，worker 跳过）。
- **小时 rollover**：边界封存旧分区（flush + commit + 关闭，RAM 归零）；新分区首条
  文档到达时惰性创建（注册表插入走一把冷路径 Mutex，与热路径无关）。

## 5. 提交与持久性

每分区独立两段式提交（复用 `SegmentInfos`/`commit_segments`）：周期提交（如 60s）+
封存时提交。崩溃丢未提交段（与现状一致）；WAL/translog、段合并 merge 不在本期范围。

## 6. 与 Java/ES 线程池模型对比

| | Java/ES（bulk 线程池 + 批量缓存） | 本方案 |
|---|---|---|
| 并行单位 | shard 内共享 IndexWriter，DocumentsWriter 锁分配 DWPT | 分区独占 builder，所有权随令牌移动 |
| 索引路径锁 | bulk 摊薄后仍存在 | 无（仅冷路径注册表锁） |
| 倾斜吸收 | 线程池调度，但 shard 内有串行点 | 队列深度比例分配 worker 时间 + 流内多 shard |
| 内存 | 堆对象 + GC，Xmx ~50% 物理内存 | arena/平行数组 + 显式预算，无 GC |
| 单核热路径 | ~130k–190k docs/s（本仓库实测口径） | 388k（text）/ 182k（日志 7 字段）docs/s |

## 7. 落地路径（如需实现）

1. `crates/core/src/ingest/mod.rs`：`PartitionKey`、`Partition`（有界队列 + state 槽）、
   `IngestService`（注册表 + ready 队列 + worker 循环 + RAM 预算/flush 选择 + rollover）。
2. 单测：令牌排他、背压、rollover 释放 RAM、预算触发选最大分区。
3. CLI `skewbench`：多流 Zipf 倾斜语料，对比静态绑定模式，验证总吞吐与各分区
   CheckIndex（复用 `interop/verify-log.sh` 查询电池）。
