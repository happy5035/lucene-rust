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
| Wildcard（仅 `*`，不含 `?`） | terms dict | 第一版：字典扫描 + glob 匹配；自动机求交留作优化 |
| PointRange（1D） | LongPoint / IntPoint BKD | 边界包含语义对齐 Java |
| Sort by NumericDV / SortedDV | DocValues | MISSING 规则照抄 FieldComparator |
| stored 取回 | stored fields（LZ4） | 命中后批量取回 |

**明确不做（YAGNI）**：评分 / norms / impact / Block-Max；模糊查询（Levenshtein 自动机）；
聚合 / facet；delete / merge；NRT 原地 refresh（open 即快照，重开即刷新）；查询并发
（第一版单线程逐段，段间并行仅留接口）；mmap（先 buffered FileChannel 读，留优化点）。

## 2. 选型理由（A/B/C 比较结论）

- A（照抄 Query/Weight/Scorer/Collector 框架）：语义保真但约 1/3 代码服务于不存在的评分体系，
  继承体系与 Rust ownership 冲突。否决。
- B（完全自实现）：代码最少但语义对齐风险高——Lucene 正确性藏在 advance 协议、position 合取、
  BKD 边界、MISSING 排序等细节里，验收标准要求与 Java 逐条 diff 一致，偏差即失败。否决。
- **C（采纳）**：执行语义逐行对照 9.12.3 源码（保持 docs 中 file:line 引用的项目惯例），
  对象结构用 `enum Query` + `trait DocIter` 表达；JNI 层做 Lucene 形状的可替换门面。
  估计总量 6–7k 行（含测试 ~1.5k）。

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
- **Query 与执行**：`enum Query { Term, Phrase, Boolean, Terms, Prefix, Wildcard, PointRange }`；
  `trait DocIter { doc_id, next_doc, advance }`（DISI 语义照抄，继承体系不要）。
  conjunction 用 lead-iterator 两两对齐；phrase 用 position 合取（对齐 slop=0 路径）。
- **Collector**：topN by docID / NumericDV / SortedDV 三种堆；MISSING 值规则照抄 9.12.3
  `FieldComparator`（long 默认排尾、string ord 空排尾）。collector 本身是堆操作，不是热点；
  查询耗时集中在**喂给它的解码循环**上，性能优化放在解码层（见 §5a SIMD 策略）。
- **JNI 门面**：reader 句柄 = `Arc<Searcher>` + 句柄表（沿用写侧 IndexWriter 句柄模式）；
  query 以 JSON 字符串传入（复用 core 的 json 基础设施）。Java 类方法签名对齐 Lucene 常用子集
  （`open/close`、`search(query, n, sort) -> TopDocs` 形状），上层替换 Java Lucene 只改 import。

### 惰性加载

open 时只读 segments_N + .si + .fnm；FST / BKD / DV 索引在首次触及该字段时加载。

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

## 7. 工作量粗估

| 部分 | 估计 |
|---|---|
| 读路径（postings+FST ~1.2k、BKD ~600、DV ~700、stored ~400、indexinput/segments ~300） | 3.5–4k 行 |
| 执行层（Query/DocIter/conjunction/phrase/collector） | ~1k 行 |
| SIMD bit-unpack 核 + 运行时分发 + 对拍测试 | ~500 行 |
| JNI 门面 | ~400 行 |
| 测试 | ~1.5k 行 |
| **合计** | **6.5–7.5k 行** |

## 8. 后续优化点（不在本期）

- Wildcard 的自动机与 FST 求交（替代字典扫描）
- doc delta 的 SIMD prefix-sum（待 bench 数据）
- mmap IndexInput
- NRT 原地 refresh（段文件 refcount + writer 删除协议，~800 行）
- 段间并行搜索
