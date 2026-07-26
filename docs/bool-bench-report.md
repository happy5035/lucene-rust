# Bool 查询能力研究与 Benchmark 报告

日期：2026-07-25 初版；2026-07-26 更新（§7–§11：M7 合入复测、三模式口径扩展、
P1-1/P1-3 修复与复测）。范围：嵌套 Boolean 查询（M6 §2）读路径的能力梳理、三路 benchmark
（rust-roaring / rust-PFOR（`RL_BITMAP=0`）/ Java 9.12.3 `--no-cache`）、perf 热点归因、
优化方案。查询集 = Java `SearchBench --dump-queries`（tasks=50, seed=42）+ 23 条手工 BOOL
扩展行（纯 NOT / MUST+NOT 跨字段 / 跨字段 OR / RANGE 组合），Rust 与 Java 逐条重放同一文件；
7-26 起扩展至 715 条（boolq-raw + boolq-extra 合并集，同索引 `/tmp/boolbench-idx`）。

**进展速览**：§6 的 P0（count set-algebra）已由 M7 里程碑实现（T-B bitmap fold，§7）；
P1-1（排除形迭代路径，§9）与 P1-3（PointRange，§10）已修复。no-fast 全量迭代口径下
Rust 对 Java 全面反超（排除形 2–4.5×、range 2.3×、flat/多词项 1.4–3.9×）。

## 1. 能力现状（读代码梳理）

`Query::Bool { clauses: Vec<(Occur, Query)> }`，MUST/SHOULD/MUST_NOT 三态，子查询可为任意
变体（Term/Phrase/Terms/Prefix/Wildcard/PointRange/MatchAll/Bool），跨字段、任意嵌套。
每段执行两条路径（`query.rs` `bool_segment_iterator`）：

1. **拍平快路径**（spec §2.4 `flatten_bool`）：纯 MUST 或纯 SHOULD、递归展开后叶子全为
   同字段 Term → 改写进 `roaring_exec` 三档引擎：
   - 档 1（全 bitmap）：AND 偏斜比 ≥256 → 最小侧迭代 + `contains` 探测；非偏斜 → croaring
     物化 `and` fold；OR → 物化 `or` fold。count 走 `and/or_cardinality`（k=2 不物化）。
   - 档 2（混合）：低 df 子句查询时物化 ≤4095 docs + bitmap 侧 k 路归并。
   - 档 3（无 bitmap）：PFOR `ConjunctionDocIter`/`DisjunctionDocIter` + 跳表。
2. **通用组合路径**（spec §2.3）：`ConjOver`（ConjunctionDISI 对齐舞）/ `DisjOver`
   （线性 min-scan k 路归并）/ `Excluding`（ReqExclScorer 两指针，多 MUST_NOT 先 DisjOver
   合成一个 prohibited）。叶子各自走自己的段迭代器（Term 叶子仍可享受 `RoaringDocIter`）。

count 侧（`bool_segment_count`）：拍平形 → roaring count 快路径；纯 MUST_NOT → maxDoc −
prohibited count（prohibited 侧拍平 OR 形可再命中 roaring count）；其余形状 → **驱动组合
迭代器逐 doc 计数**。

语义与 Lucene BooleanWeight 一致（ConstantScore 化简：有 MUST 时 SHOULD 不参与执行；
纯否定以 MatchAll 为正集），hit-counts 与 Java 逐条 diff 通过（本报告 §4 门槛）。

## 2. Benchmark 口径

- **host**：2 vCPU / 1GB RAM（弱于 M3 报告的 4 vCPU/3GB；绝对数字偏低，比值结论可迁移）。
- **语料**：`logwrite /tmp/boolbench-idx 1000000 42 --bitmap`，227MB，2 段（_0 大 / _1 小，
  与 M3 同形）。df 分布：`level` 五个词 df≈200k（20% 密度 → bitset 容器，roaring 主场）；
  `message` 数字变体词 df≈9–19k（~1–2% 密度 → array 容器）。
- **三路同索引**：Java 直接读 Rust 写出的索引（内联 bitmap 字节对 Java 零感知 → Java 天然
  纯 PFOR 路径）；rust-pfor = 同 binary `RL_BITMAP=0`。Java 侧 `--no-cache`（无 query cache，
  与 Rust 无缓存等价）。计数口径：两侧均走 count 语义（Java `TotalHitCountCollector`，
  Rust `Searcher::count`），iterm 强制全迭代。
- **参数**：`--warmup 10 --iter 30`（RANGE 扩展组 5/20），三路串行避免争用。
  分组行 = 组内各 query 的 qps/p50 平均，qps = 1e9/单 query 延迟中位数（与 M3 同口径）。

## 3. 三路对比表

单位：p50 µs / qps（越高越好）。比值 = qps 比。

### 3.1 既有核心组（dump 查询集）

| 分组 | rust-roaring p50/qps | rust-pfor p50/qps | java p50/qps | roaring/pfor | roaring/java |
|---|---|---|---|---|---|
| term high | 6.2 / 163,656 | 6.3 / 162,349 | 21.5 / 49,735 | 1.00 | **3.29** |
| term med | 6.9 / 148,655 | 8.2 / 128,121 | 15.4 / 65,686 | 1.19 | **2.26** |
| and high | 155.3 / 9,755 | 495.3 / 2,117 | 433.3 / 4,774 | **4.61** | **2.04** |
| and med | 128.8 / 8,383 | 286.0 / 3,939 | 265.2 / 5,253 | **2.13** | 1.60 |
| or high | 130.0 / 10,753 | 1047.0 / 964 | 373.8 / 4,246 | **11.15** | **2.53** |
| or med | 232.4 / 4,944 | 372.6 / 2,915 | 228.2 / 8,940 | 1.70 | 0.55 |
| iterm high | 172.9 / 6,251 | 227.0 / 4,467 | 99.7 / 10,384 | 1.40 | 0.60 |
| iterm med | 100.4 / 11,341 | 121.5 / 9,408 | 59.2 / 18,709 | 1.21 | 0.61 |
| bool high（dump 四形状混合） | 4007 / 3,449 | 1553 / 874 | 794 / 1,530 | qps 3.9x，p50 反劣 | — |
| bool med | 1746 / 2,132 | 666 / 2,071 | 459 / 2,847 | 1.03 | 0.75 |

- 拍平同字段 AND/OR 与 README M5 结论一致（and/or 对自身 PFOR 4.6–11x，对 Java 2–2.5x）。
- `bool high/med` 的 qps 与 p50 矛盾是**双峰分布**：拍平形状极快（百 µs）、含 MUST_NOT 的
  通用形状极慢（ms 级），qps 均值被快查询拉高、p50 被慢查询主导。修掉 §5 的 P0 后该组
  会整体收敛。

### 3.2 扩展形状组（手工 BOOL 行，本报告新增）

| 分组（形状） | rust-roaring p50/qps | rust-pfor p50/qps | java p50/qps | roaring/pfor | roaring/java |
|---|---|---|---|---|---|
| mnfhh（high ∧ ¬high，bitset×bitset） | **171,641** / 5.8 | 19,880 / 50.8 | 5,713 / 175 | **0.12** | **0.033** |
| mnfhm（high ∧ ¬med） | 18,067 / 55.8 | 10,265 / 97 | 2,541 / 393 | 0.57 | 0.14 |
| mnfmh（med ∧ ¬high） | 19,373 / 51.8 | 1,109 / 889 | 738 / 1,320 | **0.058** | 0.039 |
| multinot（2MUST+2NOT） | 24,216 / 42.4 | 1,208 / 827 | 2,220 / 486 | **0.051** | 0.087 |
| nothi（纯 ¬high） | 1,536 / 647 | 2,303 / 434 | **9.9 / 101,278** | 1.50 | **0.006** |
| notmed（纯 ¬med） | 82.8 / 12,177 | 125.9 / 7,962 | 10.2 / 115,588 | 1.53 | 0.105 |
| orcross（3 路跨字段 OR） | 8,864 / 132 | 7,503 / 151 | 1,179 / 915 | 0.88 | 0.144 |
| rngmust（RANGE ∧ term） | 19,789 / 50.5 | 19,500 / 51.2 | 4,398 / 296 | 1.02 | 0.17 |
| rngnot（term ∧ ¬RANGE） | 27,985 / 35.7 | 27,690 / 35.9 | 4,970 / 200 | 1.01 | 0.14 |
| rngandnot（RANGE ∧ term ∧ ¬term） | 100,935 / 9.9 | 98,519 / 10.1 | 3,723 / 265 | 1.02 | 0.037 |

（RANGE = timestamp 100k/500k doc 区间；counts 与 Java 逐条一致，如 rngnot=179,999
= 199,987 − 19,988。）

### 3.3 perf 热点（bool 混合负载，8,396 samples）

| 占比 | 符号 | 归因 |
|---|---|---|
| **75.2%** | `container_iterator_read_into_uint32`（croaring `next_many`） | 排除路径每候选 advance 丢弃 512 批量缓冲重填（mnfhh/mnfhm 占运行时长 ~90%，按排除法归因） |
| 6.2% | `SegmentDocIter::next_doc` | enum 分派（14 变体，18.7KB 大 enum） |
| 3.5% | `roaring_bitmap_frozen_view` | 每次 `contains`/advance 重建视图（~60ns/次） |
| 3.1% / 2.7% / 2.2% | ConjOver / advance / `ExcludingDocIter::next_non_excluded` | 组合器本体 |

拍平 AND/OR 单独压测（低样本，定性）：`RoaringOrDocIter::next_doc` 22%、分派 10%、
memmove 7.5%（region 拷贝）、malloc/free ~7%、`roaring_bitmap_contains` 6%、pread 2.6%
——分布均匀，无单点瓶颈，已调优到位。

## 4. 正确性

三路 hit-counts 逐条 diff：扩展组 23 条 + dump 组 and/or/bool/prefix/wildcard/terms 全部
一致（iterm 覆盖全部 89 个 TERM 行）。`term=` 行两侧抽样机制不同（M2 以来既有行为，
非计数错误）。RANGE 边界曾有一版手抄位数错误（1.7e14，实为全区间退化形），已用算术
生成正确边界复测，两版数字均保留（§3.2 为正确版；全区间退化形 roaring/pfor 同为
117–231ms，暴露 PointRange 全量物化最坏情形）。

## 5. 瓶颈根因（实测支撑）

1. **排除（MUST_NOT）在 roaring 路径是灾难**：`ExcludingDocIter` 对 prohibited 每候选调
   `advance(d)`，而 `RoaringDocIter.advance` = 丢弃 512 批量缓冲 + 重建 frozen view +
   `reset_at_or_after` + 重填 512 docs 只用 1 个（~850ns/候选）。PFOR 侧 advance 走跳表
   （~100ns），Java 侧跳表 + JIT（~28ns）——roaring 反而比自家 PFOR 慢 8.5–17x。
   perf 75% CPU 即此处。
2. **count 缺 set-algebra 快捷**：Java `TotalHitCountCollector` 逐叶咨询 `Weight.count()`
   （TermQuery → docFreq O(1)；PointWeight → BKD cell 精确计数；BooleanWeight 单否定等
   形状化简）。Rust `Searcher::count` 只有 Term / multi-term / 拍平 bool / ≥2 子句纯否定
   四个快捷，**缺**：单 MUST_NOT（maxDoc−df）、MUST±MUST_NOT（`andnot_cardinality`）、
   跨字段 OR（`or_cardinality`）——这些 croaring 算子全部现成，只是没接进通用 bool count。
   nothi 156x、notmed 9.5x 的差距纯算法层面。
3. **PointRange 物化带全局排序**：visitor → `Vec<u32>` → `sort_unstable` + `dedup` →
   `MaterializedBitmap::of`。500k 区间排序 ~40–90ms；Java 直接建 FixedBitSet（O(1)/doc，
   无排序）。rngandnot 101ms vs Java 3.7ms，roaring/pfor 同慢（瓶颈不在 bitmap 引擎）。
4. **`FrozenBitmap::contains` 每次调用建视图**（~60ns）叠加探测本体（8.5–28.7ns）——
   探测循环放大约 3x。
5. 工程税：`open_term_bitmap` 双拷贝（region Vec + 对齐缓冲再拷）、18.7KB `SegmentDocIter`
   分派帧、通用路径 MUST 子句未按 df 排序、DisjOver 线性 min-scan。

## 6. 优化方案（按收益排序）

> **状态（2026-07-26）**：P0 → **已完成**（M7 T-B，见 §7）；P1-1 → **已完成**（§9）；
> P1-3 → **已完成**（§10，实现与原方案略有出入：count 走 .kdm 元数据捷径而非
> FixedBitSet，迭代用 croaring 增量 add 而非 FixedBitSet）；P1-2 / P2 → 未做。

### P0 — count set-algebra（最大收益面，纯快路径追加）

**P0-1 通用 bool count 走 roaring 集合代数**。`bool_segment_count` 装配阶段：凡叶子能解析
为 doc 集（Term frozen view / 低 df 物化 slice → `Bitmap::of` / PointRange 物化 / multi-term
bitset），正集按 MUST 合取 / 纯 SHOULD 析取 fold，prohibited 并集后 `andnot_cardinality`
（k=2 不物化，µs 级）；叶子含 Phrase 等无 bitmap 形状 → 落回现有驱动迭代（行为不变）。
预期：mnfhh 171ms → ~30–50µs（~4,000x）、mnfmh 19.4ms → ~30µs（~600x）、multinot
24ms → ~40µs、orcross 8.9ms → ~150µs（~60x）。`RL_BITMAP=0` 自动落回现路径。

**P0-2 单 MUST_NOT count = maxDoc − 子句 count**。Term 叶子直读 docFreq（与 Term count
同款 O(1)），bitmap 叶子读 cardinality。预期：nothi 1.54ms → ~6µs（~250x）、notmed
83µs → ~6µs（~14x），对齐 Java 的 BooleanWeight 单否定化简。

### P1 — 迭代路径（top_docs/search 受益）

**P1-1 ExcludingDocIter bitmap 感知**：prohibited 为 bitmap 背衬时改 `contains` 探测
（或 main/prohibited 双 bitmap → `andnot` 物化 fold 再迭代，µs 级）。mnfhh 迭代
171ms → ~5–8ms（~25x），向 Java 5.7ms 靠拢。

**P1-2 字节级 `frozen_contains`**（无视图构建）：frozen 布局已全量校验过，直接在字节上
keys 二分 → 容器分派（array 二分 / bitset 位测 / run 扫描），~20–30ns vs 现 ~80ns。
连带收益：档 1 偏斜探测路径，`SKEW_RATIO` 门槛可重新校准（现 256 是带 60ns 视图税 calibrate 的）。

**P1-3 PointRange 去排序**：count 用 `FixedBitSet`（1M docs = 125KB，set O(1)/doc +
popcount，~2ms vs 排序 ~40–90ms）；迭代用无序 roaring add 或 FixedBitSet 迭代。
rngandnot 101ms → ~6–10ms。对 Java 的 BKD cell 精确 count（`PointWeight.count()`）
仍需后续 visitor 计数扩展才能追平，列为 P2+。

### P2 — 工程税（个位数～两位数百分比）

- `open_term_bitmap` 单次对齐读（去掉 region 中转拷贝）——拍平路径 memmove 7.5%；
- `SegmentDocIter` 大变体（Docs/Freqs，18.7KB）Box 化，分派函数瘦身、debug 栈友好；
- 通用路径 MUST 子句按 df 升序装配（零成本，省首轮候选浪费）；DisjOver k>4 堆化；
- 档 2 count 直接在物化 slice 上计数，不驱动迭代器。

### 验证路径

P0/P1 均为快路径追加、落回语义不变：hit-counts 三路逐条 diff（本报告 §2 查询集即可复用）
+ `cargo test`（codec 153 + core 46）+ `RL_BITMAP=0` A/B 同数。

## 附：复现命令

```bash
CP="interop/java/classes:interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar"
cargo build --release && javac -cp "$CP" -d interop/java/classes interop/java/*.java
./target/release/rustlucene-cli logwrite /tmp/boolbench-idx 1000000 42 --bitmap
java -Xmx512m -cp "$CP" SearchBench /tmp/boolbench-idx message \
  --dump-queries /tmp/boolq-raw.txt --tasks 50 --seed 42
cat /tmp/boolq-raw.txt /tmp/boolq-extra.txt > /tmp/boolq.txt   # extra = 23 条扩展 BOOL 行
./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 > /tmp/bench-rust-roaring.tsv 2>&1
RL_BITMAP=0 ./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 > /tmp/bench-rust-pfor.tsv 2>&1
java -Xmx512m -cp "$CP" SearchBench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --no-cache --warmup 10 --iter 30 > /tmp/bench-java.tsv
perf record -g --call-graph dwarf -F 999 ./target/release/rustlucene-cli searchbench ...
```

扩展查询集要点：`nothi/notmed`（纯 MUST_NOT 高低 df）、`mnfhh/mnfhm/mnfmh`（MUST+NOT
同字段/跨字段三向组合）、`multinot`（2MUST+2NOT）、`orcross`（3 路跨字段 OR）、
`rngmust/rngnot/rngandnot`（timestamp RANGE 100k/500k 区间组合，边界 = TS_BASE +
99,999,999 / 499,999,999，TS_BASE=1,700,000,000,000）。

---

# 2026-07-26 更新

## 7. M7 合入后复测（count 口径）

M7 里程碑（T-A 两阶段迭代协议 / T-B codec roaring fold 原语 + 通用 Bool count bitmap
fold / T-C OR 堆化 / T-D top-N 早停）已自 main 合入 dev。§6-P0 的两项提案（通用 bool
count set-algebra、单 MUST_NOT = maxDoc−df）由 T-B 的 `materialize_bool_bitmap`
（正集三态 fold + prohibited andnot，`FOLD_COST_FACTOR×maxDoc` 预算护栏）完整覆盖。
270 测试全绿（codec 182 + core 85 + cli 2 + jni 1），三路 715 条计数逐条一致。

count 口径关键行（p50 µs，§3.2 为合入前数字）：

| 形状 | 合入前 roaring | **合入后 roaring** | Java | Rust vs Java |
|---|---|---|---|---|
| mnfhh | 171,641 | **141.8**（1210×） | 6,919 | **49× 快** |
| mnfhm | 18,067 | **141.6**（127×） | 3,154 | 22× 快 |
| mnfmh | 19,373 | **141.7**（136×） | 699 | 4.9× 快 |
| multinot | 24,216 | **267.9**（90×） | 1,593 | 5.9× 快 |
| orcross | 8,864 | **199.4**（44×） | 1,177 | 5.9× 快 |
| bool high（双峰） | 4,007 | **179.1** | 761 | 4.3× 快 |
| nothi / notmed | 1,536 / 82.8 | 1,498 / 76.0 | **8.7 / 10.9** | Java O(1) 捷径（§11） |
| term high | 6.2 | 10.2（**−36%**） | 17.3 | 1.7× 快（回归但仍领先） |

49× 机理核验（mnfhh）：Java count 该形状落 `BooleanWeight.count()` 三态格的 −1
（正集/禁集均非平凡），ReqExclScorer 逐 doc 枚举 ~200k MUST docs（~33ns/doc ≈ 6.9ms）；
Rust fold = 16 个 bitset 容器级 `andnot_cardinality`（~128KB SIMD 扫描）+ 每子句
~120µs 开启税。交叉验证：`RL_BITMAP=0` 下 Rust 同形 4,911µs ≈ Java 6,919µs（同为
枚举类别），证明 49× 是位图集合代数 vs 逐 doc 枚举的算法代差，非测量假象。

回归：term high count 6.2→10.2µs（−36%），M7 两阶段协议（`matches()` 分派）的固定税；
PFOR multinot −77%（fold 回落路径变化）。量级小，未回滚。

## 8. 口径扩展：count / --topn / --no-fast-count 三模式

§3–§7 均为 count 口径。Java top-N 有 minCompetitive / block-max WAND 早停
（mnfhh count→topN 快 51×），而实际日志检索场景是 `WHERE ... ORDER BY sort字段 LIMIT N`：
索引未 index-sort 时 `TopFieldCollector` 必须访问全部命中，**Java 的 score 早停失效**。
两侧 bench harness 因此扩展两个开关（互斥，同现 exit 2）：

- **`--topn N`**：Rust `top_docs`（INDEXORDER，fast count 直读 total + 收满 n 即停）；
  Java `searcher.search(q, N)`（TopScoreDocCollector，BM25 + WAND 早停）。detail 输出
  docID 列表供逐条对账。
- **`--no-fast-count`**（sort-topN 场景代理）：强制全量迭代——Rust 所有 work item 走
  `searcher.search` + `CountCollector`（从不调 `fast_segment_count`）；Java 用
  `ForceIterQuery` 包装（`FilterWeight.count()` 恒 −1，`TotalHitCountCollector` 被迫
  逐命中迭代）。两侧均"遍历所有命中 doc、逐个计数、不早停、不返回 doc 数据"，
  以 count 计数为代理指标，后续 sort/PQ 处理不在测量范围。

top-N 结果要点（Java WAND 主场，Rust 无算分）：Java 在 mnfhh（135 vs 241µs）、
orcross（41 vs 286µs）等早停形状反超；Rust 在 bool high/and/term/多词项仍领先。
该模式非目标场景（早停在 sort-topN 下失效），优先级最低，数据留档备查。

## 9. P1-1：排除形迭代路径物化先行（已完成）

**病理**（no-fast 口径实测）：`ExcludingDocIter` 逐候选调 `prohibited.advance(d)`，
bitmap 禁集上 `BitmapCursor::advance` 每次丢 512-doc 批缓存重 seek——**代价与禁集
bitmap 密度正相关**：notmed（禁集 9k doc）roaring 仅比自家 PFOR 慢 1.14×；
nothi/mnfhh（禁集 200k doc）roaring 比 PFOR 慢 10–14×（nothi 257ms vs 18.6ms，
Java 12.8ms，Rust 20× 慢于 Java）。mnfmh/multinot 候选仅 7.4k/1.8k 却同样耗时
~16.4ms——per-advance 成本（~2.2µs）主导，与候选数无关。

**修复**（commit d69161d）：`bool_segment_iterator` 非拍平形状先试
`materialize_bool_bitmap` 全树容器级 fold（count 路径同款，语义已验证），and/andnot
结果经 `MaterializedDocIter`（原 `PointsDocIter` 改名）顺序迭代；空集 → `None`；
超预算 → 回落原通用组合器。门控：`RL_BITMAP=0` 纯 postings 读路径跳过物化
（PFOR 下全扫物化丢惰性，小 MUST + 大 NOT 形状回归 2–4×，惰性二指针已近优）；
PointRange 叶子恒放行（BKD 恒物化，Points 二指针同受丢批病理）。

**no-fast 结果**（p50 µs，修前 → 修后 / Java）：

| 形状 | roaring 修前 | **修后** | 提速 | Java | Rust vs Java |
|---|---|---|---|---|---|
| mnfhh | 164,208 | **1,629** | 101× | 6,716 | **4.1× 快** |
| mnfmh | 16,453 | **198** | 83× | 841 | 4.2× 快 |
| multinot | 16,739 | **295** | 57× | 1,323 | 4.5× 快 |
| nothi | 257,468 | **5,620** | 46× | 12,263 | **2.2× 快** |
| mnfhm | 16,453 | **1,651** | 10× | 3,597 | 2.2× 快 |
| orcross | 11,189 | **2,236** | 5.0× | 5,841 | 2.6× 快 |
| bool high | 3,622 | **421** | 8.6× | 1,267 | 3.0× 快 |
| notmed | 19,953 | **6,981** | 2.9× | 8,549 | 1.2× 快 |
| flat and/or/term/prefix/wildcard/termsbig | — | 全部 1.0× | — | — | 1.2–3.9× 快（不变） |

零回归验证：count 模式 ±5% 内（快路径未动）；topN 模式无回归（multinot/bool high
反而快 1.2–1.3×，rngnot 266→59ms——空集物化 → `None` 顺带消灭 `top_docs` 对空结果
的盲迭代）；PFOR 路径全形状回到修前（fold 1.0–1.1×，kill-switch 可用性完整）。
语义顺带对齐：SHOULD 全缺 + MUST_NOT 存在 → 空集（原迭代路径为 MatchAll − prohibited，
现与 count 路径及 Lucene minShouldMatch=1 一致）。

## 10. P1-3：PointRange count 元数据捷径 + 迭代去排序（已完成）

**三层根因**：
1. **count 无元数据捷径**：`fast_segment_count` 的 PointRange 臂恒全量物化
   （rngmust count 28ms），而 Java `PointWeight.count()` 在查询区间包含字段 min/max 时
   直读 `.kdm` 的 `getDocCount`（O(1)，27µs 完成整个 Bool 格化简）；
2. **Inside 子树无用值解码**：`intersect_node` 的 Inside 分支逐点解码 (value, doc)
   （代码注释自认与 Java `visitDocIDs` 的偏差），而调用方只需要 doc；
3. **物化带全局排序**：`point_range_bitmap` = Vec push + `sort_unstable`（1M 点 ~10ms）
   + dedup + `Bitmap::of`。

**修复**：
- `PointsReader::field_bounds`（.kdm min/max/doc_count，O(1) 常驻）：
  `fast_segment_count` PointRange 臂区间包含值域 → 直返 doc_count；
  `materialize_query_bitmap` PointRange 臂区间包含值域且 doc_count == maxDoc →
  `MaterializedBitmap::full(maxDoc)` 免 BKD 物化（Bool fold count 由此脱离 maxDoc 级成本）；
- `PointsReader::intersect_docs`：doc-only 遍历，Inside 叶只读 docID 块（跳过
  commonPrefix/values 解码），Crosses 叶解码值过滤；
- `MaterializedBitmap::empty/add`：增量容器插入替代 Vec+sort+dedup（无序幂等，
  多值点去重由 bitmap 集合语义天然承担）。

**结果**（rngmust / rngnot，p50 µs）：

| 模式 | rngmust 修前 → 修后 | rngnot 修前 → 修后 | Java（修后同期） |
|---|---|---|---|
| count | 28,059 → **56.9**（493×） | 29,121 → **52.4**（556×） | 27.6 / 19.0（O(1) 格化简，Rust 2–3× 慢） |
| no-fast | 29,289 → **842**（35×） | 28,317 → **52.1**（543×） | 1,901 / 6,690 → **Rust 2.3× / 128× 快** |
| topN | 50,268 → **87.9**（572×） | 58,782 → **107.5**（547×） | 93.9 / 6,629 → 打平 / 62× 快 |

rngnot no-fast 128× 注脚：该形状 hits=0（RANGE 覆盖全值域，MUST 被全排除）。Rust 的
物化 fold 以 andnot 集合代数证明空集（52µs = 纯 fold 成本，不枚举 doc）；Java 被
ForceIter 剥夺 `Weight.count()` 捷径后必须物理遍历 200k MUST docs 才能确认排除
（6.7ms）。两者都是"真实执行"——sort-topN 现实中 Rust 侧结果集为空、无 doc 可排序，
差异是执行机械本身的代差。

其余全部形状三模式 ±20% 内（多为运行间噪声），P1-1 战果完整保留。

## 11. 剩余差距与新优先级

no-fast（目标场景）Rust 已对 Java 全面反超：排除形 2.2–4.5×、range 2.3–128×、
flat and/or 1.4–3.9×、多词项 2.7–3.4×、单词打平（9.9 vs 8.6 ns/doc）。剩余：

1. **Java count O(1) 捷径族**（nothi 8.7µs / notmed 10.9µs vs Rust 1,498 / 76µs）：
   `BooleanWeight.count()` 三态格 + Term docFreq 直读，纯 count 口径的算法捷径。
   count 语义下 Rust 的迭代级数字无实际意义（生产 count 走 `fast_segment_count`，
   nothi/notmed 的 Rust 快路径同样 O(1)——差距仅在 no-fast 强制迭代口径显现）。
   优先级低。
2. **topN 模式 Java WAND 早停形状**（nothi 65µs vs Rust 1,510µs）：非目标场景
   （sort-topN 无早停），优先级最低。
3. **P1-2 字节级 frozen_contains**（未做）：档 1 偏斜探测路径 ~3× 放大，工程税级别。
4. **P2 工程税**（未做）：open_term_bitmap 双拷贝、大 enum 分派、MUST 按 df 装配等。
5. Java no-fast 下界本身已很硬（notmed 8.5 ns/doc = DefaultBulkScorer JIT 热循环峰值），
   Rust 在 notmed 上 1.2× 的差距属常数项打磨空间（物化 fold 的 ~7ms 固定成本 vs
   Java 零物化直扫）。

## 附：三模式复现命令（2026-07-26 起）

```bash
# count（默认）/ --topn 10 / --no-fast-count 三模式，两侧同 flag
./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 [--topn 10 | --no-fast-count]
RL_BITMAP=0 <同上>
java -Xmx512m -cp "$CP" SearchBench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --no-cache --warmup 10 --iter 30 [--topn 10 | --no-fast-count]
```
