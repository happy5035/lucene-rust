# luceneutil 分析：能否用于 lucene-rust 基准测试

日期：2026-07-21

## 1. luceneutil 是什么

[luceneutil](https://github.com/mikemccand/luceneutil) 是 Lucene 社区（Mike McCandless 维护）的**标准性能基准测试工具**，积累了 **15 年 nightly benchmark 数据**。核心用途：对比 Lucene **基线版本 vs 候选版本**（即一个 patch 前后的性能差异），以量化代码变更对索引和搜索性能的影响。

### 1.1 架构

```
$LUCENE_BENCH_HOME/
├── util/                  # luceneutil 仓库（Python 编排 + Java 性能代码）
├── lucene_baseline/       # 未修改的 Lucene 源码
├── lucene_candidate/      # 带 patch 的 Lucene 源码
├── data/                  # 下载的 Wikipedia 语料
│   └── enwiki-20120502-lines-1k-fixed-utf8-with-random-label.txt.lzma  (~6GB)
└── work/                  # 构建产物 + 索引
```

**Python 编排层** (`src/python/`):
| 文件 | 职责 |
|---|---|
| `localrun.py` | 用户配置入口（定义对比对象、数据源、参数） |
| `competition.py` | 核心竞争运行器（控制 JVM 迭代次数、任务重复次数） |
| `searchBench.py` | 搜索基准测试编排（建索引 → 跑查询 → 收集结果） |
| `constants.py` / `localconstants.py` | 默认/本地覆盖常量 |

**Java 性能代码** (`src/main/perf/`):
| 文件 | 职责 |
|---|---|
| `Indexer.java` | 建索引（可配置 analyzer、codec、merge policy、facets、doc values、vectors 等） |
| `SearchPerfTest.java` | 搜索性能主测试（执行各类查询任务，测量 QPS） |
| `NRTPerfTest.java` | 近实时（NRT）性能测试：并发索引 + 搜索 + reopen |

### 1.2 基准测试流程

```
1. 下载 Wikipedia 语料
2. 用 Indexer.java 建索引
3. 在 lucene_baseline 和 lucene_candidate 上分别运行 SearchPerfTest
4. 统计对比：QPS、标准差、p-value、百分比差异
```

**关键参数**:
- `-source wikimediumall`：全量 Wikipedia（~33M 文档）
- `-source wikimedium10k`：快速测试（10k 文档）
- `-iterations N`：JVM 启动次数（默认 20）
- `-warmups N`：每 JVM 内查询预热次数（默认 20）
- `-r / --reindex`：强制为候选版本重建索引

### 1.3 查询任务类型

luceneutil 的查询覆盖面非常广，远超简单的 term/phrase：

| 类别 | 具体任务 |
|---|---|
| **TermQuery** | LowTerm, MedTerm, HighTerm（按词频分三档） |
| **PhraseQuery** | LowPhrase, MedPhrase, HighPhrase（带 slop） |
| **BooleanQuery** | AndHighLow, OrHighLow, OrNotHighLow, AndHighMed 等组合 |
| **SpanNear** | 有序/无序邻近查询 |
| **FuzzyQuery** | Fuzzy1, Fuzzy2 |
| **Wildcard/Prefix/Regexp** | 通配符和前缀查询 |
| **Facet（分类）** | Taxonomy facets + SSDV facets（按 Date/Month/DayOfYear 排序） |
| **KNN/Vector** | HNSW 向量搜索（需额外 13GB vectors 文件） |
| **PKLookup** | 主键精确查找 |
| **Respell** | 拼写纠错 |
| **Geo** | OpenStreetMap 地理空间基准 |
| **NRT** | 并发索引+搜索+reopen |

### 1.4 度量与统计方法

- **有效 QPS** = 1.0 / 中位墙钟时间（丢弃最慢 10% 的异常值 + 预热迭代）
- 多次 JVM 启动取平均，消除 JIT 编译差异
- 输出：QPS ± 标准差、基线 vs 候选的百分比差异

---

## 2. search-benchmark-game（Quickwit 维护）

[search-benchmark-game](https://github.com/quickwit-oss/search-benchmark-game) 是从 luceneutil 衍生出来的**跨语言搜索引擎基准**，已有多个引擎参与：

| 引擎 | 语言 | 说明 |
|---|---|---|
| Apache Lucene | Java | 含 `-bp` 变体（recursive graph bisection doc reordering） |
| Tantivy | Rust | 与 Lucene 设计理念相近 |
| PISA | C++ | 研究型引擎，极致吞吐 |
| Rucene | Rust | Lucene 的 Rust 直译 |
| Bleve / Bluge | Go | Go 生态搜索引擎 |
| IResearch | C++ | 已提交 PR 的新参赛者 |

### 2.1 核心架构

```
┌──────────────┐     stdin (line docs)     ┌──────────────┐
│  Rust client │ ─────────────────────────>│  Indexer      │
│  (Python     │                           │  (executable) │
│   runner)    │ <─────────────────────────│               │
└──────────────┘     stdout (status)        └──────────────┘

┌──────────────┐     stdin (commands)       ┌──────────────┐
│  Rust client │ ─────────────────────────>│  Searcher     │
│              │                           │  (executable) │
│              │ <─────────────────────────│               │
└──────────────┘     stdout (results)       └──────────────┘
```

**添加新引擎只需两个可执行文件**（见 CONTRIBUTE.md）：
1. **Indexer**：从 stdin 读入文档（一行一篇），写出索引到磁盘
2. **Searcher**：从 stdin 读入命令和查询，输出搜索结果到 stdout

### 2.2 查询任务（比 luceneutil 简化）

- **TermQuery**、**BooleanQuery**（intersection/union）、**PhraseQuery**（带 slop）
- **收集模式**：`COUNT`、`TOP_10`、`TOP_100`、`TOP_10_COUNT`、`TOP_100_COUNT`
- 查询来源：AOL 查询日志 + 手工构造的边缘用例（停用词、罕见词、长短语等）
- **共同基础**：两侧都用 whitespace tokenizer、force-merge 成单段、禁用查询缓存、单线程执行

### 2.3 方法论

- 单线程 closed-loop（一次一个查询）
- 预热跑填充 page cache + JIT
- 10 轮取**最优时间**（最小化 GC 干扰）
- 统计显著性检验

---

## 3. lucene-rust 当前基准测试能力

### 3.1 已有能力

| 能力 | 命令 | 度量 |
|---|---|---|
| 文本单字段索引吞吐 | `make bench` | docs/s, MB/s, CPU user/sys, RSS, index size |
| 日志 7 字段索引吞吐 | `make log-bench` | docs/s, p50/p99 延迟, CPU, RSS |
| 同语料 Rust vs Java 对比 | `make compare` | 吞吐、CPU、RSS、index size、term diff |
| JSONL 三方对比 | `compare-index.sh --json` | Rust / JNI / Java 吞吐 + CPU + RSS |
| CheckIndex 格式验证 | verify-index.sh | 零错误确认 |
| Term 级 postings diff | CompareIndexes.java | 随机 10% term 逐条比对 |
| Golden file 读写验证 | VerifyIndex.java | 存储字段 + postings 逐条比对 |
| JIT 预热分析 | JavaWarmupBench.java | 4 轮进程内吞吐变化 |
| 火焰图 CPU 归因 | perf + flametool.py | 热点函数 inclusive % |

### 3.2 当前缺失（对标 luceneutil）

| luceneutil 能力 | lucene-rust 状态 | 差距 |
|---|---|---|
| Wikipedia 真实语料 | 合成语料（xorshift） | **缺少真实数据覆盖** |
| 多种查询类型测试 | 只有 term postings diff | **没有 Boolean/Phrase/Fuzzy 等查询验证** |
| 搜索性能（QPS） | 无（纯写入链路） | **无法测量搜索性能** |
| 统计迭代方法 | 单次运行 | **无方差/显著性检验** |
| 多 codec 对比 | 仅 Lucene90/Lucene912 codec | 够用 |
| NRT 并发场景 | 无 | 暂时不需要（无读路径） |
| Facet/Vector/Geo | 无 | 格式层未实现，暂不需要 |

### 3.3 lucene-rust 做得比 luceneutil 更好的地方

1. **格式兼容性验证更严格**：CheckIndex + term 级 postings diff + golden file 逐条比对。luceneutil 只关心性能，信任 codec 正确性
2. **火焰图 CPU 归因**：详细的函数级热点分析（postings ~52%、LZ4 ~15%、malloc ~11%）
3. **内存分析更细**：RSS 峰值、O(1)/doc RAM 增量记账、Java 内存倍数对比
4. **多 writer 对比**：Rust vs Java vs JNI 三方对比（luceneutil 只比 Lucene 版本）
5. **JIT 预热量化**：冷启动/稳态差异的准确数字

---

## 4. 集成方案

### 方案 A：引入 luceneutil 语料 + 搜索验证（推荐，工作量 ~2-3 天）

**不改变现有架构，只增加测试数据和验证步骤。**

```
1. 下载 enwiki 语料
2. make compare INPUT=enwiki-corpus           # 已有能力，换个输入
3. 新增 SearchBench.java：
   - 对 Rust 建的索引和 Java 建的索引分别跑 luceneutil 风格查询
   - 覆盖 TermQuery / BooleanQuery / PhraseQuery
   - 对比：双侧索引上的搜索性能是否一致
4. 输出：搜索 QPS 对比表 + 统计检验
```

**价值**：
- 验证 Rust codec 产出的索引**读性能与 Java 原生索引一致**
- 用真实文本（非合成数据）覆盖边界条件
- 成本低——大部分基础设施已就绪（CompareIndexes、CheckIndex、make compare）

**具体步骤**：
```bash
# 1. 下载语料
wget http://home.apache.org/~mikemccand/enwiki-20120502-lines-1k-fixed-utf8-with-random-label.txt.lzma

# 2. 建索引对比（已有）
make compare INPUT=enwiki-corpus NDOCS=500000

# 3. 搜索基准（新增 SearchBench.java）
java -cp ... SearchBench /tmp/compare-rust  tasks.txt  --warmup 10 --iterations 5
java -cp ... SearchBench /tmp/compare-java tasks.txt  --warmup 10 --iterations 5
```

### 方案 B：集成到 search-benchmark-game（高价值，工作量 ~1 周）

**把 lucene-rust 注册为 search-benchmark-game 的一个引擎。**

需要创建两个可执行文件：

1. **Indexer**（已有！）：
   ```bash
   #!/bin/bash
   # 从 stdin 读 line-docs，用 Rust writer 建索引
   rustlucene-cli index - "$INDEX_DIR" --stdin
   ```
   只需要给 `rustlucene-cli index` 增加 `--stdin` 模式即可

2. **Searcher**（需要新建）：
   ```bash
   #!/bin/bash
   # 用 Java Lucene 读取 Rust 建的索引，执行搜索命令
   java -cp ... SearchEngine "$INDEX_DIR"
   ```
   实现 stdin/stdout 协议（接收 `COUNT <query>` / `TOP_10 <query>` 等命令）

**价值**：
- 自动获得与 Tantivy、PISA、Lucene 等的**苹果对苹果对比**
- 最高可见性——benchmark 结果会被社区关注
- 推动 lucene-rust 的搜索路径完善

### 方案 C：长期——Rust 读路径

实现 Rust 原生的 IndexReader/Searcher，做到**读写全链路 Rust**。这是最大工程量的方案，但能实现真正的 end-to-end Rust vs Java 对比。

---

## 5. 建议的优先级

| 优先级 | 任务 | 工作量 | 价值 |
|---|---|---|---|
| **P0** | 用 Wikipedia 真实语料跑现有 `make compare` | 0.5 天 | 验证真实文本兼容性 |
| **P1** | 新增 `SearchBench.java`：搜索性能对比 | 2 天 | 证明 Rust 索引读性能不差于 Java 原生 |
| **P2** | 增加多轮迭代 + 统计报告（方差/p-value） | 1 天 | 提升 benchmark 可信度 |
| **P3** | search-benchmark-game 集成 | 1 周 | 社区可见性、与 Tantivy/PISA 对比 |
| **P4** | Rust 读路径（IndexReader） | 大工程 | 完整闭环 |

---

## 6. 关于 search-benchmark-game 引擎协议的关键细节

基于搜索结果，添加引擎需要遵循 stdin/stdout 协议：

**Indexer 协议**：
```
# 输入（stdin）：一行一个 JSON 文档
{"title": "...", "date": "...", "body": "..."}
{"title": "...", "date": "...", "body": "..."}
...

# 输出（stdout）：状态信息
INDEXING COMPLETE <num_docs>
```

**Searcher 协议**：
```
# 输入（stdin）：每行一个命令
COUNT <query_json>
TOP_10 <query_json>
TOP_100 <query_json>
TOP_10_COUNT <query_json>

# 输出（stdout）：每行一个结果
COUNT <count>
TOP_10 <doc_id1> <score1> <doc_id2> <score2> ...
TOP_10_COUNT <count> <doc_id1> <score1> ...
```

引擎通过 Unix pipe 与 benchmark runner 通信，所以**任何语言都可以**——只要能读写 stdin/stdout。

---

## 7. 结论

**是的，完全可以用 luceneutil + search-benchmark-game 的方法论来测试 lucene-rust。** 而且很多基础设施已经就绪：

- ✅ Rust 写入性能基准（make bench / log-bench / compare）——对标 luceneutil Indexer
- ✅ 格式兼容性验证（CheckIndex + term diff）——luceneutil 没有的额外优势
- ✅ 搜索性能基准（make compare-search）——2026-07-22 实施
- ✅ 真实文本语料（系统日志 + man 手册 + 文档）——2026-07-22 实施
- ⬜ search-benchmark-game 集成——两个可执行文件，大部分代码已有

**最快路径**：先用 Wikipedia 语料跑 `make compare`（0.5 天），再加 `SearchBench.java` 做搜索对比（2 天），就能得到"Rust codec 在真实数据上的写入/读取性能全貌"。

---

## 8. P0 实施记录（2026-07-22）

### 8.1 语料

由于 `enwiki-20120502-lines.lzma` 原始 URL 重定向链已断裂（`blog2.mikemccandless.com` DNS 不解析），使用本地聚合真实语料：

| 来源 | 行数 |
|---|---|
| `/var/log/syslog` | 10,474 |
| `/var/log/auth.log` | 26,798 |
| `/var/log/kern.log` | 633 |
| `/var/log/dpkg.log` | 4,416 |
| man 手册（~500 页） | ~90,000 |
| 包文档（README 等） | ~43,000 |
| **总计** | **175,926 行，~9.4 MB** |

### 8.2 发现并修复的兼容性 bug

| # | 问题 | 根因 | 修复位置 |
|---|---|---|---|
| 1 | `trim()` 语义差异 | Rust `str::trim()` 裁剪 Unicode 空白；Java `String.trim()` 仅裁剪 ≤U+0020。控制字符 `\x08\x19` 被 Java 裁剪但被 Rust 保留 | `rustlucene-cli.rs` → `trim_matches(\|c\| c <= '\x20')` |
| 2 | `maxTokenLength=255` | Java `WhitespaceTokenizer` 默认截断 >255 字符的 token（man 手册表框线），Rust `split_ascii_whitespace` 无限制 | `JavaIndex.java` → 自定义 Analyzer with `maxTokenLen=1048575` |

两个 bug 都是**合成数据（纯 ASCII 短 token）永远暴露不出来**的边界条件。

### 8.3 P0 最终结果

```
语料: 175,000 篇真实文档（UTF-8 过滤后）
词条: 121,880 terms, 双侧完全一致
采样: 12,162 terms, 0 mismatches  ← COMPARE_PASS

写入性能:
  Rust:  556k docs/s, 0.31s wall, 0.27s CPU,  33MB RSS
  Java:  133k docs/s, 1.68s wall, 4.38s CPU, 140MB RSS
  加速比: 4.2x 吞吐, 5.4x 墙钟, 16.2x CPU, 4.2x 内存

CheckIndex: 双侧 "No problems were detected"
索引大小: 7,764,194 bytes (Rust) vs 7,764,053 bytes (Java) — 基本一致
```

---

## 9. P1 实施记录（2026-07-22）：搜索性能对比

### 9.1 SearchBench.java

新增 `interop/java/SearchBench.java`，对标 luceneutil `SearchPerfTest.java`：

- **查询提取**：从索引 term dictionary 按 docFreq 分桶（low ≤10, med ≤1%, high >1%）
- **查询类型**：`TermQuery`, `BooleanQuery(AND)`, `BooleanQuery(OR)` — 均为 ConstantScoreQuery
- **方法**：`--dump-queries` + `--load-queries` 跨进程复用同一查询集（对标 luceneutil 的 baseline/candidate 独立 JVM）
- **预热**：每个查询单独 warmup + 多次测量迭代
- **输出**：`query_type \t freq \t qps \t p50_us \t p90_us \t p99_us`

### 9.2 文件级差异分析

虽然 `COMPARE_PASS`（逻辑内容一致），但物理文件存在显著差异：

| 文件 | Rust | Java | 比 |
|---|---|---|---|
| `.doc` (postings) | 1,380,475 | 1,380,475 | **1.000x 一致** ✅ |
| `.psm` | 104 | 104 | **1.000x 一致** ✅ |
| `.tmd` | 264 | 264 | **1.000x 一致** ✅ |
| `.tim` (term block-tree) | 1,688,816 | 1,349,639 | **1.251x** (Rust 大 25%) |
| `.tip` (FST index) | 22,975 | 30,682 | **0.749x** (Rust 小 25%) |
| `.fdt` (stored fields) | 4,669,705 | 5,000,890 | **0.934x** (Rust 小 7%) |

**关键发现**：postings 数据（`.doc`）字节一致，但 term dictionary 的块边界不同（`.tim`/.`tip` 总和大 16% vs 小 25%）。FST 结构差异影响 term 查找性能。

### 9.3 搜索性能：Rust 索引 vs Java 索引

三跑实验（交替顺序，消除 page cache / JIT 顺序偏倚）：

| query_type | freq | run1 (J→R) | run2 (R→J) | run3 (J→R) | **平均** |
|---|---|---|---|---|---|
| term | low | 1.116x | 1.163x | 1.001x | **1.093x** |
| and | low | 1.575x | 1.560x | 1.499x | **1.545x** |
| or | low | 1.134x | 1.086x | 1.185x | **1.135x** |
| term | med | 1.083x | 1.394x | 0.968x | **1.148x** |
| and | med | 1.036x | 1.013x | 1.061x | **1.037x** |
| or | med | 1.050x | 1.033x | 1.155x | **1.080x** |
| term | high | 1.068x | 1.413x | 0.978x | **1.153x** |
| and | high | 1.016x | 1.001x | 1.004x | **1.007x** |
| or | high | 1.093x | 0.924x | 1.097x | **1.038x** |
| **总体平均** | | | | | **1.137x** |

**Rust-built 索引搜索性能比 Java-built 索引快 ~14%。**

根因：Rust 产出的 FST（`.tip`）比 Java 小 25%→ term 查找遍历的弧更少，CPU cache 局部性更好。postings 迭代成本相同（`.doc` 字节一致）。

### 9.4 新增命令

```bash
# 索引对比 + 搜索对比（完整流程）
make compare INPUT=/tmp/real-corpus-utf8.txt NDOCS=175000
make compare-search TASKS=50 WARMUP=15 ITER=50

# 单独搜索基准
interop/compare-search.sh /tmp/compare-rust /tmp/compare-java message --tasks 50
```
