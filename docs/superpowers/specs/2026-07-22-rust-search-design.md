# Rust 搜索读路径设计（方案 C：算法语义照抄，对象结构 Rust 化）

日期：2026-07-22。状态：已获用户批准（范围与架构经 brainstorming 流程确认）。

## 1. 目标与范围

为 rustlucene 增加**搜索读路径**：Rust 直接读取自己写出的 Lucene 9.12.3 兼容索引并执行查询，
通过 JNI 暴露给 Java 上层作为真实查询能力（非验证工具、非 benchmark 专用）。

**查询能力范围（由写入能力严格限定，全部 ConstantScore 语义）：**

| 查询 | 对应写入能力 | 备注 |
|---|---|---|
| Term | text / keyword 字段 postings | docs+freqs |
| Phrase（slop=0） | text_with_positions | position 合取 |
| Boolean must/should | 上述组合 | lead-iterator 对齐 / 堆合并 |
| Terms（IN 语义） | term 的批量入口 | Boolean SHOULD 语法糖 |
| Prefix | terms dict（FST） | FST seek + 顺序扫 |
| Wildcard（`*` 与 `?`） | terms dict | 按 pattern 形状分类：前缀形走 FST 前缀扫 + 尾过滤，其余走字典扫描 + 模式匹配；自动机求交留作优化 |
| PointRange（1D） | LongPoint / IntPoint BKD | 边界包含语义对齐 Java |
| MatchAll | 段元数据 | [0..maxDoc) 扫描，构造验证基线用 |
| Sort by NumericDV / SortedDV | DocValues | MISSING 规则照抄 FieldComparator |
| stored 取回 | stored fields（LZ4） | 命中后批量取回 |

**明确不做（YAGNI）**：评分 / norms / impact / Block-Max；模糊查询（Levenshtein 自动机）；
聚合 / facet；delete / merge；NRT 原地 refresh（open 即快照，重开即刷新）；查询并发
（第一版单线程逐段，段间并行仅留接口）；mmap（先 buffered FileChannel 读，留优化点）。

**前提假设**：读侧只保证读**本系统写出的**索引（无 delete、无 .liv、无 norms、无 vector 等）。
读 Java 写的索引不在本期范围——若将来需要，至少补 LiveDocs（.liv FixedBitSet）与
skip 数据之外的删除语义，届时单独立项。

## 2. 选型理由（A/B/C 比较结论）

- A（照抄 Query/Weight/Scorer/Collector 框架）：语义保真但约 1/3 代码服务于不存在的评分体系，
  继承体系与 Rust ownership 冲突。否决。
- B（完全自实现）：代码最少但语义对齐风险高——Lucene 正确性藏在 advance 协议、position 合取、
  BKD 边界、MISSING 排序等细节里，验收标准要求与 Java 逐条 diff 一致，偏差即失败。否决。
- **C（采纳）**：执行语义逐行对照 9.12.3 源码（保持 docs 中 file:line 引用的项目惯例），
  对象结构用 `enum Query` + `trait DocIter` 表达；JNI 层做 Lucene 形状的可替换门面。
  估计总量 7–8k 行（含测试 ~1.5k）。

## 3. 架构

```
┌─ Java ─────────────────────────────────────────────┐
│ RustIndexReader / RustIndexSearcher（JNI 门面）     │
│   open(indexPath) -> handle                        │
│   search(handle, queryJson, topN, sort) -> TopDocs │
└──────────────┬─────────────────────────────────────┘
               │ JNI（扩展现有 crates/jni-binding）
┌──────────────▼─────────────────────────────────────┐
│ rustlucene-core 新增 search 模块                    │
│   Query enum / parse_query(json)                   │
│   Searcher: per-segment 迭代 -> merge -> topN      │
│   Hit = { docID, stored fields, sort values }      │
├────────────────────────────────────────────────────┤
│ codec-lucene9 新增读侧                              │
│   IndexInput（buffered）/ segments_N / .si / .fnm  │
│   postings 读: FST terms dict + .doc/.pos + skip   │
│   points 读: BKDReader.intersect（1D）             │
│   docvalues 读: Numeric/Sorted + IndexedDISI       │
│   stored 读: LZ4 块 + 文档重建                      │
└────────────────────────────────────────────────────┘
```

### 组件职责

- **IndexInput 读抽象**：buffered reader（对齐 `BufferedIndexInput`，8KB 缓冲），FileChannel 实现。
  一切格式读的地基。
- **SegmentReader**：打开一个段的全部文件（.fnm/.tim/.tip/.doc/.pos/.kdd/.kdi/.dvd/.dvm/.fdx/.fdt），
  持有各 format reader。查询按段执行（对齐 Lucene leaf-level 执行）。
- **Query 与执行**：`enum Query { Term, Phrase, Boolean, Terms, Prefix, Wildcard, PointRange, MatchAll }`；
  `trait DocIter { doc_id, next_doc, advance }`（DISI 语义照抄，继承体系不要）。
  conjunction 用 lead-iterator 两两对齐；phrase 用 position 合取（对齐 slop=0 路径）。
- **Collector**：topN by docID / NumericDV / SortedDV 三种堆；MISSING 值规则照抄 9.12.3
  `FieldComparator`（long 默认排尾、string ord 空排尾）。collector 采用**块级批处理**接口
  （一次消费一个 128-doc 解码块，而非 Lucene 式 per-doc `collect()`），与解码层 SIMD 协同——
  详见 §4b。
- **JNI 门面**：reader 句柄 = `Arc<Searcher>` + 句柄表（沿用写侧 IndexWriter 句柄模式）；
  query 以 JSON 字符串传入（复用 core 的 json 基础设施）。Java 类方法签名对齐 Lucene 常用子集
  （`open/close`、`search(query, n, sort) -> TopDocs` 形状），上层替换 Java Lucene 只改 import。

### 惰性加载

open 时只读 segments_N + .si + .fnm；FST / BKD / DV 索引在首次触及该字段时加载。

### 模块分解（镜像写侧的格式边界）

原则：codec 只懂字节，一种文件格式一个解码器；聚合层（SegmentReader / IndexSearcher）放 core，
因为它是搜索概念而非格式概念。terms dict 与 postings 枚举分两层——prefix / wildcard / terms
只碰 terms dict 不碰 postings，分层后这些查询的实现更干净。

**codec-lucene9（在既有写侧文件上追加 / 新增 `*_read.rs`）：**

| 模块 | 位置 | 职责 |
|---|---|---|
| IndexInput | `io.rs` 追加 | 随机访问读：`read_vlong/vint/zint`、`slice`、`file_pointer`；buffered FileChannel 实现 |
| `open_input` | `directory.rs` 追加 | FSDirectory 打开输入流 |
| FST 读 | `fst.rs` 追加 | `lookup(term)` / `prefix_scan(prefix)` / `scan_all()`，供 terms dict 定位与通配枚举 |
| 解码原语 | `postings_ll.rs` / `packed.rs` 追加 | `for_util_decode` / `pfor_util_decode`（写侧 encode 的镜像）；`DirectReader` / `DirectMonotonicReader` |
| terms dict 读 | `terms_read.rs` 新增 | .tip（FST）+ .tim（TermState 元数据）：term → postings 入口 fp/df 元数据；枚举接口 |
| postings 枚举 | `postings_read.rs` 新增 | .doc / .pos：Docs / DocsAndFreqs / DocsFreqsPositions 三种枚举，含 df=1 singleton 捷径、skip list、PFOR 块解码 |
| DV 读 | `doc_values_read.rs` 新增 | .dvm/.dvd：Numeric（gcd/min 重建 + IndexedDISI 三分支）与 Sorted（ords + LZ4 terms dict + reverse index） |
| BKD 读 | `points_read.rs` 新增 | .kdi/.kdd：1D 树遍历 + `intersect(lower, upper)`，DocIdsWriter 五分支解码 |
| stored 读 | `stored_fields_read.rs` 新增 | .fdt/.fdx/.fdm：LZ4 块解压 + 六种值类型文档重建 |
| 提交点读 | `segment_infos.rs` / `segment_info.rs` / `field_infos.rs` 追加 | segments_N / .si / .fnm 解析 |

**rustlucene-core `search/` 模块：**

| 模块 | 职责 |
|---|---|
| `segment_reader.rs` | 段聚合：持有一个段的各 format reader + FieldInfos + max_doc，对查询层暴露字段视图 |
| `reader.rs` | 多段聚合：解析 segments_N，持有 SegmentReader 列表 |
| `query.rs` | Query enum + JSON 解析 + wildcard 分类（前缀形 → FST 前缀扫 + 尾过滤；其余 → 字典扫描 + 模式匹配） |
| `doc_iter.rs` | `trait DocIter { doc_id, next_doc, advance, next_block }` 及各实现：term/postings、conjunction（lead-iterator 对齐）、disjunction（堆合并去重）、phrase（合取 + position 校验）、MatchAll、BKD 结果 |
| `collector.rs` | DocBlock + 三类块级 collector（§4b）+ 段间 merge |
| `searcher.rs` | IndexSearcher：query × 段 → collector → TopDocs |

### 实施顺序（薄切片先行，验证设施尽早转起来）

与"先读完全部格式再做搜索"的顺序不同——先打通 term 查询端到端，让 Java diff 验证设施
尽早生效，之后每个阶段都新增一类可 diff 的查询能力：

1. **地基**：IndexInput + open_input + 解码原语（for/pfor decode、DirectReader、DirectMonotonicReader）
   + segments_N/.si/.fnm 读 —— round-trip 单测
2. **Term 查询端到端**：terms dict 读 + postings 枚举 + SegmentReader/reader + DocIter +
   docID collector + MatchAll —— 第一版 Java diff 电池（term count / 精确命中 / MatchAll）
3. **Boolean + Terms**：conjunction / disjunction —— diff 电池扩展
4. **DV 读 + 排序**：Numeric/Sorted 读 + 两种 DV collector —— diff 电池扩展（sort、MISSING）
5. **BKD 读 + PointRange** —— diff 电池扩展（范围 count / docID 序列）
6. **Phrase**（positions 枚举 + position 校验）—— diff 电池扩展
7. **Prefix / Wildcard**（FST 枚举 + 分类 rewrite）—— diff 电池扩展
8. **stored 取回 + JNI 门面** —— TopDocs 带字段返回；Java 类 + 句柄表
9. **SIMD 快路径**（§4a/§4b，bench 驱动逐项启用）—— 对拍单测 + bench 报告

每个阶段交付 = 代码 + 对应测试绿 + diff 电池增量全绿。

## 4a. 解码性能与 SIMD 策略

写侧编码事实（`codec-lucene9/src/postings.rs`、`packed.rs`）决定读侧热点全部是
**固定位宽整数块解码**，这正是 SIMD 的主战场（Lucene 10 对同一格式引入 VectorizedForUtil，
证明格式与 SIMD 兼容——解码侧自由，字节格式不变）：

| 热点 | 格式来源 | SIMD 方案 |
|---|---|---|
| postings .doc/.pos 的 128 块（doc delta + freq + position） | `pfor_util_encode`（ForUtil/PForDelta） | AVX2/SSE2 位移+or 做 128 值 bit-unpack，再标量 patch 异常值 |
| NumericDV / SortedDV ords 块 | `DirectWriter` 单块、gcd=1（写侧既定简化） | 同一套 bit-unpack 核；写侧简化保证块对齐、无跨块接缝 |
| BKD 叶 DocIdsWriter BPV24/BPV32 | `points.rs` | 同一套 bit-unpack 核 |
| doc delta 前缀和 | postings .doc | 标量先行；SIMD prefix-sum 作为二阶优化（收益待 bench 验证） |
| stored LZ4 块 | `stored_fields.rs` | 不自研——用 `lz4` crate 成熟解码 |

**落地纪律（防止 SIMD 引入格式偏差）：**

1. 先写**标量参考实现**，通过三层测试（round-trip + Java diff 终验）锁定正确性；
2. SIMD 快路径作为等价实现追加：`is_x86_feature_detected!` 运行时分发（注意
   x86_64-unknown-linux-musl target 下的 target_feature 检测），每条快路径配
   "标量 vs SIMD 输出逐值相等"的对拍单测；
3. SIMD 以 bench 数据为门槛——只对 profile 证实的热点启用，无数据不优化。
   预期主要收益：高命中 term/boolean 查询的 .doc 全块扫描、DV 排序的列式取值。

## 4b. 批处理 collector（SIMD 协同）

Lucene 的 `LeafCollector.collect(doc)` 是逐文档回调，每次调用都要过一遍堆比较；
而 postings 天然按 128 块解码，**整块在手时再逐 doc 喂堆是对解码成果的浪费**。
设计为块级批处理：

**接口**：`DocIter` 在 `next_doc/advance` 之外暴露 `next_block() -> Option<DocBlock>`
（`DocBlock { docs: [u32; 128], len, freqs: Option<&[u32]> }`，tail 块 len<128）。
非 postings 来源（BKD、phrase 校验后的结果）把命中物化进同一 `DocBlock` 形状，
collector 对来源无感知。

**collector 侧的块级优化（按排序类型分）：**

- **topN by docID**：天然批处理——docID 递增时 topN 就是尾部窗口，整块比较堆顶阈值，
  全块小于阈值则一次跳过（零堆操作）；块内命中走批量替换
- **count 查询**（验证电池的主力形态）：短路——`docs` 全块直接 `len += block.len`；
  DOCS 字段的稠密场景可进一步退化为 bitset popcount（AVX2 nibble 查表 popcount；
  有 AVX512VPOPCNTDQ 时切换原生指令）
- **topN by DV**：两阶段过滤——先对块做 SIMD 预筛（块的 DV 值域与堆顶比较，
  值域来自 DV 块头 min/max 或块采样），过不了筛的块不进堆；过筛选出候选 doc
  再取向量化的 DV gather（AVX2 gather 指令，收益待 bench 验证，可退化为标量批量取）
- **Boolean 合取**：两块 128-doc 已解码块的交集走 SIMD intersect
  （`_mm_shuffle_epi8` 查表法，Lemire 式；标量 galloping 作为对照实现）

**正确性纪律**：批处理只是消费形态变化，命中集合与排序结果必须与 per-doc 标量路径
**逐位一致**——每个批处理 collector 配"标量 collect 路径对拍"单测，并随三层测试
（round-trip / 语义电池 / Java diff 终验）锁定。

## 4. 数据流（一次查询）

```
Java: search(handle, queryJson, topN, sortSpec)
  -> JNI: parse_query(json) -> Query
  -> Searcher: 对每个 SegmentReader 顺序执行
       Query::iterator(seg) -> DocIter
         Term/Terms/Prefix/Wildcard: FST terms dict 定位 -> postings 迭代
         Phrase: 各 term postings 合取 + position 校验
         Boolean: must 合取 / should 析取
         PointRange: BKD intersect -> DocIdSet
       -> collector 堆（by docID / NumericDV / SortedDV）
  -> merge 各段结果 -> topN -> stored fields 批量取回
  -> TopDocs { total, hits[{docID, fields, sortValues}] } -> JNI -> Java
```

## 5. 错误处理

- codec 读侧统一 `io::Result` + 损坏检测：每个格式读入口校验 magic / codec header / version
  （对齐 `CodecUtil.checkHeader`），footer checksum 校验默认开启，损坏即 CorruptIndex 错误。
- JNI 边界：Rust panic 不穿越 FFI（`catch_unwind` + 错误码/Java 异常）；非法句柄返回明确错误。
- 查询错误：不存在的字段、对无 positions 字段发 phrase、对无 DV 字段排序——parse 期 fail-fast，
  与 Java 行为一致。

## 6. 测试策略（三层）

1. **Round-trip 单测**（codec crate，延续现有惯例）：写侧产出 -> 读侧解码 -> 断言重建。
   覆盖 FST seek/scan、BKD intersect 边界、IndexedDISI 各分支、stored 块。
2. **Rust 内部语义测试**：固定语料 + 固定查询电池，断言 docID 序列。
3. **Java diff 终验**（决定性验收，延续 m2 模式）：同语料双写 -> `VerifyLogIndex` 扩展
   查询电池（新增 prefix / wildcard / terms 项）-> Rust 与 Java 结果逐条 diff，
   纳入 `make log-test`。

**边界语料矩阵**（贯穿三层，每层都要覆盖）：

| 语料 | 触发路径 |
|---|---|
| 空索引（0 文档） | 所有 reader 返回空，查询返回空 TopDocs |
| 单文档 / 单 term（df=1） | postings singleton 捷径 |
| 高 df term（>128 多块 + tail） | PFOR 块边界、tail 路径 |
| 稀疏 DV（`--sparse` 语料） | IndexedDISI SPARSE/DENSE 分支、MISSING 排序 |
| 大字典（`--bigdict` 语料） | SortedDV terms dict 多块、FST 深层遍历 |
| 多段索引 | 段间结果合并、全局 docID 映射 |

## 7. 工作量粗估

| 部分 | 估计 |
|---|---|
| 读路径（postings+FST ~1.2k、BKD ~600、DV ~700、stored ~400、indexinput/segments ~300） | 3.5–4k 行 |
| 执行层（Query/DocIter/conjunction/phrase） | ~1k 行 |
| 批处理 collector（DocBlock 接口 + 三类块级 collector + 标量对拍） | ~600 行 |
| SIMD bit-unpack 核 + 运行时分发 + 对拍测试 | ~500 行 |
| JNI 门面 | ~400 行 |
| 测试 | ~1.5k 行 |
| **合计** | **7–8k 行** |

## 8. 后续优化点（不在本期）

- Wildcard 的自动机与 FST 求交（替代字典扫描）
- doc delta 的 SIMD prefix-sum（待 bench 数据）
- mmap IndexInput
- NRT 原地 refresh（段文件 refcount + writer 删除协议，~800 行）
- 段间并行搜索
- 读 Java 写的索引（LiveDocs/.liv、norms 容忍等，前提见 §1）
