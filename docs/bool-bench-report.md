# Bool 查询能力研究与 Benchmark 报告

日期：2026-07-25。范围：嵌套 Boolean 查询（M6 §2）读路径的能力梳理、三路 benchmark
（rust-roaring / rust-PFOR（`RL_BITMAP=0`）/ Java 9.12.3 `--no-cache`）、perf 热点归因、
优化方案。查询集 = Java `SearchBench --dump-queries`（tasks=50, seed=42）+ 23 条手工 BOOL
扩展行（纯 NOT / MUST+NOT 跨字段 / 跨字段 OR / RANGE 组合），Rust 与 Java 逐条重放同一文件。

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
