# M3 Roaring Bitmap 三路 Bench 报告（--no-cache 口径）

日期：2026-07-24。任务：M3 Task 7 —— 高 df AND/OR/count 三路对比（rust-roaring vs rust-PFOR（`RL_BITMAP=0`）vs Java），
写侧吞吐损失与磁盘增量测量，hit-counts 三路对拍。原始产物（查询集、counts、原始输出）在 `.superpowers/sdd/`（gitignored）。

**复测说明**：本报告数字为 **HEAD 3f6492f**（M3 终审一行清理：cli threshold clamp≥4096 + 两处注释 +
verify-log.sh `env -u RL_BITMAP`，均不触热路径）上按同一口径的复测结果；Rust 索引由该 HEAD 的
release binary 重建，Java 索引同语料复用。新旧两轮数字均在运行噪声内，**结论不变**。

## 1. 口径

- **HEAD**：3f6492f（三路同一 binary；roaring 与 pfor 由 `RL_BITMAP=0` 环境变量切换，同二进制同索引）。
- **语料**：log schema，`logwrite / JavaLogBench 1000000 docs，seed 42`。Rust 索引带 `--bitmap`
  （`/tmp/rl-bench3-rust`），Java 索引同语料（`/tmp/rl-bench3-java`，天然无内联 bitmap → 全走 PFOR 档 3 等价路径）。
- **缓存口径**：Java 侧 `--no-cache`（`setQueryCache(null)` + `NEVER_CACHE`）；Rust 无 query cache，天然等价。
- **查询集**：`.superpowers/sdd/m3-q.txt`（Java `SearchBench --dump-queries --tasks 50 --seed 42` 产出，
  按 brief 守卫剔除了 5 条 df<4096 的 TERM 行（med 桶截断词：conne/buffer/chec/buffe/evict，df 1766–1921），
  原始 dump 存 `m3-q-raw.txt`；剔除后守卫通过 `ALL TERM LINES df>=4096`，`AND high` 行数=50）。
  low 桶在本语料为空（无 df≤10 的 term）；med 桶存在（df 4096–9898）。
- **bench 参数**：`--warmup 10 --iter 30`，三路串行跑（避免 CPU 争用）。统计口径两侧一致：
  分组行 = 组内各 query 的 qps/p50/p90/p99 之平均，qps = 1e9/单 query 延迟中位数。
- **host**：AMD EPYC 7K62（4 vCPU），3GB RAM，Linux 6.8.0-124-generic，rustc 1.97.1，OpenJDK 21.0.11。

## 2. 三路对比表

单位：qps（越高越好）；p50/p90/p99 单位 µs（越低越好）。比值为 qps 比。
（数字 = HEAD 3f6492f 复测；与上一轮（e09b55c）相比各分组 qps 偏差 ≤ ~6%（Java 侧 ≤ ~25%，
JVM 运行间噪声），全部排序关系与量级不变。）

### term / and / or / iterm（M3 核心四类）

| 分组 | rust-roaring qps | p50 | p90 | p99 | rust-pfor qps | p50 | p90 | p99 | java qps | p50 | p90 | p99 | roaring/pfor | roaring/java |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| term high | 105162.8 | 9.7 | 10.1 | 12.5 | 175571.9 | 5.8 | 5.9 | 7.1 | 109039.0 | 9.2 | 11.6 | 19.1 | **0.60** | **0.96** |
| and high | 1462.6 | 696.1 | 704.4 | 722.7 | 2777.2 | 364.2 | 373.8 | 388.6 | 11168.6 | 262.5 | 277.6 | 368.8 | **0.53** | **0.13** |
| or high | 1398.4 | 724.0 | 731.0 | 750.8 | 1604.0 | 634.7 | 652.4 | 675.5 | 9229.0 | 209.2 | 218.9 | 271.2 | **0.87** | **0.15** |
| iterm high | 3113.5 | 330.2 | 337.3 | 352.2 | 7774.3 | 130.2 | 136.7 | 144.2 | 13483.1 | 75.3 | 81.2 | 90.9 | **0.40** | **0.23** |
| term med | 115540.1 | 8.7 | 8.9 | 10.8 | 177256.6 | 5.7 | 5.9 | 7.4 | 98805.1 | 10.2 | 14.6 | 119.8 | 0.65 | 1.17 |
| and med | 3473.0 | 308.8 | 317.7 | 331.3 | 5519.6 | 195.6 | 205.0 | 213.8 | 11429.8 | 134.8 | 150.3 | 247.0 | 0.63 | 0.30 |
| or med | 3197.3 | 334.9 | 342.9 | 355.9 | 3499.5 | 310.5 | 323.7 | 336.6 | 21728.3 | 120.2 | 132.7 | 147.3 | 0.91 | 0.15 |
| iterm med | 6675.2 | 149.8 | 156.5 | 164.4 | 13297.4 | 75.2 | 80.0 | 86.7 | 24165.2 | 41.4 | 47.7 | 55.5 | 0.50 | 0.28 |

注：low 桶在本语料为空（已注明）。med 桶的 AND/OR/TERMS/PREFIX/WILDCARD 行引用的 term 中约 5 个
（conne/buffer/chec/buffe/evict，df<4096）无 bitmap，相应查询走 T5 档 2（查询时物化）混合路径；
high 桶全部为纯档 1（容器折叠）。med 的 term/iterm 行在剔除 5 条低 df TERM 行后全部 bitmap 命中。

### M2 查询类型（参考行，同口径同次运行）

| 分组 | rust-roaring qps | p50 | rust-pfor qps | p50 | java qps | p50 |
|---|---|---|---|---|---|---|
| prefix high | 224.4 | 5275.8 | 229.0 | 5182.2 | 42.1 | 24084.7 |
| prefix med | 422.5 | 2446.7 | 425.8 | 2431.4 | 110.5 | 11814.1 |
| wildcard high | 649.6 | 3275.3 | 932.5 | 3055.8 | 5956.6 | 12132.3 |
| wildcard med | 1468.1 | 1432.3 | 1966.1 | 1349.1 | 7623.0 | 5890.8 |
| terms high | 375.2 | 2665.2 | 595.6 | 1678.5 | 2387.4 | 418.8 |
| terms med | 1037.9 | 963.5 | 1300.6 | 768.8 | 4746.3 | 210.7 |
| termsbig high | 382.7 | 2613.0 | 388.8 | 2572.1 | 459.4 | 2176.2 |
| termsbig med | 669.5 | 1493.2 | 673.6 | 1484.0 | 764.8 | 1307.4 |

（M2 类型不走 roaring 路径，roaring/pfor 差异为测量噪声；termsbig 三路几乎相等可作噪声基准。）

## 3. 结论：对照 spec §1 预期

**spec §1 预期「高 df AND 提速一个数量级」在本 bench 口径下未兑现：实测 roaring/pfor = 0.53，
roaring/java = 0.13。** and/or/iterm/term 四类 high 桶 roaring 全面慢于 PFOR（0.40–0.87x）。

**根因（有实测支撑）**： roaring 读侧每次查询都把 inline bitmap 从 .doc **全量反序列化一遍**
（`PostingsReader::read_term_bitmap` → `RoaringBitmap::deserialize`：crc32 校验 + 整段拷贝 +
逐 u16 `read_short()` 校验重建容器），无任何跨查询缓存。用同形状合成 bitmap（1M doc 空间、
df≈18800、array 容器 2.01 B/doc）在本机做的隔离测量（/tmp/roarbench，reps=2000，同机）：

- `deserialize ×2 + and + cardinality` = **722.8 µs/次** —— 与 bench 实测 roaring `and high`
  p50（696.1 µs）几乎重合：roaring AND 的成本 ≈ 反序列化 ×2（~490 µs，占 ~2/3）+ 折叠（~240 µs）；
- 单个 bitmap `deserialize` = 244.7 µs（≈13 ns/doc，逐元素校验重建所致）；
- 预反序列化后的 `and` 折叠 = **242.1 µs**（`or` = 230.5 µs）——注意这只比 PFOR 合取全程
  （实测 364 µs）快 ~1.5 倍，**array 容器 regime 下折叠本身并没有数量级优势**
  （merge intersect + 逐容器分配，~6.4 ns/元素 vs PFOR 解码 ~7.5 ns/doc）；
- **纯迭代（`cursor_next`，不重复反序列化）= 3.1 ns/doc** —— 比 PFOR 解码（本 bench 实测
  iterm pfor ≈ 7.5 ns/doc）快约 2.4 倍，容器层迭代本身成立。

**决定性变量是密度 regime（array vs bitset 容器）**：上述 message high 桶 df≈18.8k/1M ≈ 1.9%
密度 → array 容器。用本索引 `level` 字段（keyword，df≈200k/1M = 20% 密度 → bitset 容器，
同 binary 同 bench 参数，`--warmup 5 --iter 20`；数字为 HEAD 3f6492f 复测）实测：

| level 字段分组 | rust-roaring qps | p50_µs | rust-pfor qps | p50_µs | roaring/pfor |
|---|---|---|---|---|---|
| and high（INFO∩WARN 等，count=0） | 2436.1 | 410.0 | 281.2 | 3554.5 | **8.7x 快** |
| or high | 2228.5 | 448.4 | 146.6 | 6815.6 | **15.2x 快** |
| iterm high | 630.0 | 1586.1 | 781.4 | 1279.3 | 0.81 |
| term high | 187958.3 | 5.3 | 646532.7 | 1.5 | 0.29 |

即 **spec §1「高 df AND 提速一个数量级」在稠密 term（bitset 容器）上成立**（and 8.7x、or 15.2x，
且是带着每查询反序列化税之后的净比值——稠密 AND 里 roaring 全程 410 µs ≈ 反序列化 ~200 µs +
bitset 折叠 ~213 µs（合成微基准实测），PFOR 合取对两个 20% 稠密表无跳读红利要走 ~3550 µs）；
**在 message 这种 1.9% 密度的 array 容器 regime 不成立**（净 0.53x）——该 regime 下折叠只快
1.5x，再叠加反序列化税即倒挂。

逐类说明（message 字段主 bench）：

- **`term`（count）**：roaring 走 `read_term_bitmap_header` 只读头（locate + magic/version +
  header df/card 校验，O(1)），PFOR 直接取 FST `TermEntry.doc_freq`（同为 O(1) 但零额外 I/O）。
  实测 9.7 µs vs 5.8 µs（p50）：roaring 多付 2 次 page-cache 定位读，在 µs 级操作上占 +67%，
  qps 比 0.60。两侧都是 O(1)，差异是固定开销而非量级。
- **`iterm`（强制迭代）**：roaring = 全量反序列化（~245 µs，占 ~75%）+ 容器迭代（~60 µs）≈ 330 µs；
  PFOR = 纯解码 ~130 µs。即「容器迭代比 PFOR 解码快 2.4 倍」的红利被每迭代一次的反序列化
  （30 次测量迭代每次重新读 + 重建）吞掉并倒挂，qps 比 0.40。
- **`and`/`or`**：每查询反序列化 2 个 bitmap（IO stats 实测 roaring 每 AND 查询读 ~127 KB，
  PFOR 仅 ~62 KB）+ 折叠。反序列化 ~490 µs 占主导，折叠 ~240 µs 次之，qps 比 0.53/0.87。
  此外 `and high` 样本含低交叠对（count_min=109）：PFOR 合取可跳读，roaring 必须折叠全部
  容器对，进一步拉低比值。

**对 Java 的差距**：Java（无 bitmap，纯 PFOR + 跳表）and/or high qps 是 Rust 任一路径的
3–7 倍；与 M1 已知差距（Rust 解码 ~1.7x 慢）相比进一步扩大，主因同上（roaring 侧反序列化、
PFOR 侧本 bench 语料 df 更高、解码总量更大）。

**结论**：inline roaring bitmap 的收益由 **term 密度**决定：稠密 term（容器达 bitset 阈值，
约 >6.25%/容器）的 AND/OR 有接近一个数量级的净收益（已实测）；本 bench 主目标语料
（message 高 df 桶，~2% 密度、array 容器）下净收益为负（0.53x），瓶颈在每查询全量反序列化
（crc32 + 逐元素校验重建，~13 ns/doc）——读侧引入 bitmap 缓存或零拷贝（mmap 视图直接
迭代/折叠，不做逐元素重建）后，array regime 也只剩 ~1.5x 的折叠优势，仍达不到 spec §1 的
数量级预期。spec §1 的预期实质对应稠密 regime，已在 level 字段验证。正确性（§5）与
Java 兼容性（T6）不受影响：bitmap 只是附加字节，关掉（`RL_BITMAP=0`）即回原路径。

## 4. 写侧开销

命令与原始输出见 `.superpowers/sdd/m3-write-throughput.txt`（Step 7.4 逐字命令；数字为 HEAD 3f6492f 复测）。

| 指标 | with `--bitmap` | without | 比值/增量 |
|---|---|---|---|
| 写吞吐 docs/s | 183184 | 187617 | **0.9764（损失 2.4%）** |
| 索引总大小（du -sb） | 236961891 | 206038600 | **+15.0%**（+30,923,291 B） |
| `.doc` 段 _0 | 51,523,450 | 21,232,348 | **+142.7%** |
| `.doc` 段 _1 | 9,106,012 | 8,473,932 | +7.5% |

- **写侧 CPU 损失 2.4% < 5% 预期**：达标（上一轮同口径 0.78%，两轮差为秒级运行噪声；
  Step 7.1 索引重建同参数 185322 docs/s，一致）。
- **磁盘增量 +15.0%，远超 brief 的 < 5% 预期——预期被语料形状证伪**：1M 语料下 message 字段
  词典共 **2542 个 term，其中 2216 个 df ≥ 4096**（Java `TermsEnum` 实测；
  df 合计 20,782,371）。根因是 log 语料生成器的词表为 53 个基词 × 40 个数字变体
  （"rollback" 在基词表中出现两次）均匀抽取：每个数字变体词 df≈9300–18000，**全部**越过
  4096 阈值各配一个内联 bitmap（约 2–3.5 B/doc）。200k 语料时这些词 df≈1900–2700 < 4096
  （仅 5 个 level 词命中），brief 的 < 5% 预期正是基于 200k 形状外推所致。
  小段 _1 多数 term 的段内 df < 4096 不配 bitmap（+7.5%），大段 _0 几乎全量命中（+142.7%）。
  两轮磁盘数字逐字节相同（清理不触格式路径）。

## 5. 正确性（hit-counts 三路对拍）

原始产物：`m3-counts-{rust-roaring,rust-pfor,java}.txt`（各行 `detail<TAB>count`；HEAD 3f6492f 复测）。

**门槛 1（brief 逐字命令，plain diff 为空）——通过**：

```
$ diff .superpowers/sdd/m3-counts-rust-roaring.txt .superpowers/sdd/m3-counts-rust-pfor.txt \
    && echo "COUNTS_MATCH: roaring == pfor"
COUNTS_MATCH: roaring == pfor
```

（bitmap on/off 全量 582 条 per-query count 逐位一致——spec §8「同一查询电池 bitmap on/off
结果逐位一致」在 1M 语料端到端成立。）

**门槛 2（roaring vs Java）——以排序多集对比通过，含一处已查明的口径偏差**：

brief 的逐字 `diff` 对 Java 侧不可能为空，原因有二（均已定位，均非计数错误）：

1. Java stderr 混入 JVM 启动日志（WARNING / MemorySegment / Vectorization 提示），已按行过滤
   （产物 `m3-counts-java.clean.txt`）。
2. **`term=` 行两侧抽样机制不同**：Rust 侧 Fisher-Yates shuffle 后取全量（桶内 45/44 个 term
   各一次、无放回、乱序），Java 侧 `rng.nextInt()` 有放回抽 45/44 次（有重复、有遗漏、
   RNG 算法不同）——这是 M2 以来就存在的既有行为，`and/or/iterm/prefix/wildcard/terms`
   六类为逐字重放不受影响。

等效验收（命令与结果，HEAD 3f6492f 复测全部通过）：

```
# 非 term= 行（493 行/侧：and 100 + or 100 + iterm 89 + prefix 100 + terms 4 + wildcard 100）
$ diff <(grep -v '^term=' c-rust.txt | sort) <(grep -v '^term=' c-java.txt | sort)
（空）→ NON_TERM_LINES_MATCH: rust == java

# term= 重合行（59 个不同 term，148 行）逐行 count 对比
（awk join 校验：0 处不一致）→ TERM_COINCIDING_MATCH
```

其中 **iterm 块覆盖查询文件全部 89 个 TERM 行**（两侧同序同集），与 `term=` 同为 TermQuery
计数语义——即每个 term 的 hit-count 实际上已被 iterm 块全量验证三路一致；`term=` 行的
抽样差异不影响正确性结论。两侧内部 `term=` 与 `iterm=` 同名计数亦各自一致。

**结论**：roaring == pfor == java 的 hit-counts 对拍在所有可比查询上全部一致，验收通过。

## 附：本报告全部数字的来源命令

```bash
CP="interop/java/classes:interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar"
# Step 7.1 建索引（复测：HEAD 3f6492f release binary 重建）
cargo build --release -p rustlucene-core --bin rustlucene-cli
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- logwrite /tmp/rl-bench3-rust 1000000 42 --bitmap
java -cp "$CP" JavaLogBench /tmp/rl-bench3-java 1000000 1 42
# Step 7.2 查询文件（+ df 守卫与低 df 行剔除）
java -cp "$CP" SearchBench /tmp/rl-bench3-java message --dump-queries .superpowers/sdd/m3-q.txt --tasks 50 --seed 42
awk -F'\t' '!($1=="TERM" && $4<4096)' .superpowers/sdd/m3-q-raw.txt > .superpowers/sdd/m3-q.txt
# Step 7.3 三路 bench（串行）
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- searchbench /tmp/rl-bench3-rust message \
  --load-queries .superpowers/sdd/m3-q.txt --warmup 10 --iter 30 \
  > .superpowers/sdd/m3-bench-rust-roaring.out 2> .superpowers/sdd/m3-counts-rust-roaring.txt
RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- searchbench /tmp/rl-bench3-rust message \
  --load-queries .superpowers/sdd/m3-q.txt --warmup 10 --iter 30 \
  > .superpowers/sdd/m3-bench-rust-pfor.out 2> .superpowers/sdd/m3-counts-rust-pfor.txt
java -cp "$CP" SearchBench /tmp/rl-bench3-java message \
  --load-queries .superpowers/sdd/m3-q.txt --no-cache --warmup 10 --iter 30 \
  > .superpowers/sdd/m3-bench-java.out 2> .superpowers/sdd/m3-counts-java.txt
# Step 7.4 写侧 + 磁盘（见 m3-write-throughput.txt 逐字命令）
# 词典统计（/tmp/CountTerms.java，一次性诊断程序，不在仓库内）
java -cp "/tmp:$CP" CountTerms /tmp/rl-bench3-wt-bm message
# → field=message total_terms=2542 terms_df_ge_4096=2216 df_sum_ge4096=20782371
# 反序列化/折叠成本隔离测量：/tmp/roarbench（一次性合成 bitmap 微基准，path 依赖
# codec-lucene9，reps=2000）：array 形状 = 1M doc 空间 / df≈18800（2.01 B/doc）；
# bitset 形状 = df≈200000（0.65 B/doc）
# level 字段稠密 regime A/B（§3 第二张表）：
printf 'TERM\thigh\tINFO\t200000\nTERM\thigh\tWARN\t200000\nTERM\thigh\tERROR\t200000\nAND\thigh\tINFO\tWARN\nAND\thigh\tERROR\tWARN\nOR\thigh\tINFO\tWARN\nOR\thigh\tERROR\tWARN\n' > /tmp/q-level.txt
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- searchbench /tmp/rl-bench3-rust level \
  --load-queries /tmp/q-level.txt --warmup 5 --iter 20          # roaring
RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- searchbench /tmp/rl-bench3-rust level \
  --load-queries /tmp/q-level.txt --warmup 5 --iter 20          # pfor
# IO stats 辅助测量（§3 and/or 字节数）：RL_IO_STATS=1，10 条 AND high + 5 条 TERM high 的小查询文件
```
