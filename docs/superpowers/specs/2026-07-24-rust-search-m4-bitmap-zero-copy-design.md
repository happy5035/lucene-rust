# M4 设计：bitmap 读侧去税（去 crc / 零拷贝视图 / 偏斜 probe / count 回归）

日期：2026-07-24。状态：已获用户批准（用户直接指令：① bitmap 存储的 crc 校验去掉；
② 不要全量读取重建容器对象，AND 低 df×高 df 时高 df 大部分读取无效、完全没必要读；
③ Term count 不该为 doc_freq 之外的 bitmap 头路径付 IO。设计草案经用户"可以"确认）。
前置：M3（`2026-07-23-rust-search-m3-roaring-bitmap-design.md` + `docs/m3-bench-report.md`）已完成。

## 1. 动机（M3 bench 定量，全部实测）

- **稀疏 array regime 倒挂**（df≈18.8k/1M = 1.9% 密度）：and 0.53x / or 0.87x / iterm 0.39x。
  根因：每查询把 inline bitmap 全量反序列化（crc32 全扫 + 逐元素 `read_short` 校验 + 容器对象
  重建，~13ns/doc），占 AND 全程 ~2/3；array 折叠本身只比 PFOR 解码快 ~1.5x，税后倒挂。
- **稠密 bitset regime 已达标**：level 字段（20% 密度）and 8.7x / or 15.2x——容器运算的
  数量级优势真实存在，被读侧税掩盖。
- **term high 0.61x**：根因独立——Term count 走了 bitmap 头读取（docStartFP-4 定位+读头+校验，
  ~3.7µs/查询），而 `TermEntry.doc_freq` 零 IO 且校验③保证两值必然相同。
- 结论：bitmap 的收益兑现 = 去掉读侧税。crc 校验与全量重建是税的全部来源（用户指令①②）。

## 2. 范围

五条（用户拍板）：

1. **格式 v2**：去 per-bitmap crc32。
2. **零拷贝 RoaringView**：读侧不再重建容器对象，操作直接作用于字节。
3. **AND df 偏斜 probe**：小侧迭代 + 大侧定点测位，高 df bitmap 不做全量读取。
4. **OR/纯迭代**：字节游标 merge，去掉重建税（全量字节读取不可避免）。
5. **Term count 回归 doc_freq 直读**，删除 `read_term_bitmap_header` 全链路。

**明确不做**：写侧容器语义/runOptimize 不动（只去 crc 字段）；OR 的全量字节读取（只去重建税）；
跨查询 bitmap 缓存（view 打开成本已降至微秒级，缓存是另一个问题）；multi-term（M2 >16 bitset
路径）roaring 集成（仍二期）；NRT。

## 3. 格式 v2（bitmap region 布局）

```
[ magic(4B "RLBM") + version(1B) = 2 + df(vInt) + cardinality(vInt) + payload ][ len: u32 LE ]
```

- 无 crc32 字段；len = 头+payload 字节数（len 上界公式同步去 4B：`20 + ⌈maxDoc/65536⌉ × 8201`）。
- **版本即迁移**：`version != 2` → `Ok(None)` 落档 postings。M3 的 v1 索引零迁移、零特判。
- 兜底校验三重（全部廉价）：len 有界 → magic/version → 头内 df/cardinality == termState.doc_freq。
  误判率 ~2⁻³⁶ 量级实际为零；.doc footer CRC 仍是 Lucene 级完整性兜底（CheckIndex 全文件校验）。
- 查询路径不再调用 `RoaringBitmap::deserialize`（保留给写侧 round-trip 测试，同步去 crc 字段）。
- 写侧除不写 crc 外零改动（同一 ChecksumIndexOutput，footer CRC 照常覆盖 bitmap 字节）。

## 4. 零拷贝 RoaringView（读侧核心，codec 提供、core 消费）

定位照旧（`docStartFP-4` len 回退 + 三重校验）。两种打开模式：

- **probe 模式（AND 大侧专用，用户指令②）**：**不读数据段**。只顺序扫容器头
  （key/type/card；数据长度由 type+card/numRuns 推出，数据段 seek 跳过）——1M doc 最多 16 个
  容器，头扫描微秒级。`contains(doc)`：按高 16 位定位桶 → array 读该桶数据段内二分 /
  bitset 按 `data_offset + word_idx×8` 读单个 u64 测位（8B/probe）/ run 读 run 对判区间。
  高 df bitmap 的绝大部分字节**从不被读**。
- **全量模式（OR/迭代/AND 小侧）**：区域一次顺序读入 buffer（单次 read，无逐元素校验），
  字节游标 next/advance（契约同 M3 RoaringCursor）直接作用于切片。

API 草案（计划阶段对齐真实代码后定稿）：`RoaringView::open_probe / open_full`、`contains(doc)`、
`cursor()`、`cardinality()`。标量先行（bitset 测位是随机内存读，无 SIMD 需求）。

## 5. AND 执行策略（按段独立，沿用 M3 档判定）

- **档 2**（部分子句有 bitmap）：无 bitmap 子句物化（df<4096 有界，M2 同源 `for_each_doc`），
  候选 doc 逐一对各 bitmap 子句 probe（`contains`），任一不含即剔除。**零 bitmap 全量读**。
- **档 1**（全有 bitmap）：按 cardinality 升序，最小侧全量模式迭代、其余 probe；最小/最大
  df 比值 < 阈值（初始 4x，bench 任务标定）时改用双字节游标 merge-intersect。
- **档 3**（全无 bitmap）：既有 PFOR 合取不动。
- And/Or count 走同一引擎（视图 cardinality / 折叠计数），`needs_freq` 依然全链路禁入。

## 6. OR / 纯迭代 / Term count

- OR：各 bitmap 全量模式游标 + 物化数组共 k 路归并（merge-union），零重建。
- Term 迭代（iterm/top_docs）：单游标零拷贝。`RoaringDocIter` 改包视图游标。
- **Term count = `doc_freq` 直读**（用户指令③）：`searcher.rs` 删 `read_term_bitmap_header`
  调用；codec 层 `PostingsReader::read_term_bitmap_header` 与 `SegmentReader` 包装一并删除
  （死代码）。预期 term high 回 1.0x。

## 7. 验证

- **Rust 对拍**：bitmap on/off 结果逐位一致（单测 + log 语料电池）；**v1 旧索引落档测试**
  （M3 写出的 v1 bitmap 索引在 v2 读侧必须静默落 postings、结果与 PFOR 一致）。
- **Java 兼容终验**：`make log-test` 五变体全绿（v2 仍是缝隙字节，CheckIndex "No problems"
  + forceMerge 实证不变）。
- **bench 复测**（同 M3 口径 + 开源对比数据标定阈值）：预期稀疏 AND ≥1x、AND 低×高 df 超
  PFOR、term high 回 1.0x、稠密保持 ≥8x；写侧因去 crc 略降开销。报告落 `.superpowers/sdd/`。

## 8. 任务切分与工作量

1. 格式 v2：写侧去 crc + 读侧定位/校验调整 + serialize/deserialize 同步 + 版本拒绝测试。
2. RoaringView：两种打开模式 + 容器目录扫描 + contains/cursor + 三容器单测。
3. AND probe：档 2 probe 过滤 + 档 1 偏斜策略 + roaring_exec 改造。
4. OR/迭代游标 + RoaringDocIter 改造 + Term count 清理（删 header 路径）。
5. 电池（含 v1 落档）+ bench 复测报告。

估 ~800 行改动（含测试），复用 M3 全部写侧容器库与电池基础设施。
