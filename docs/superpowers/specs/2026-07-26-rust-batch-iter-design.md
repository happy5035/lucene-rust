# 批量迭代（块级 DocIter）设计 — M1 spec §4b 落地

日期：2026-07-26
状态：设计已评审通过，待实施计划
关联：`docs/superpowers/specs/2026-07-22-rust-search-design.md` §4b（本设计的原始规划）、`docs/bool-bench-report.md`（三路 bench 现状）

## 0. 目标 / 非目标 / 硬约束

**目标**：实现 M1 spec §4b 的块级迭代路径（Phase 1 全标量），把 driver 侧每 doc 的 enum 分发 / `io::Result` 检查 / 分支开销摊薄到每 128 docs 一次；在 **5M doc 单段**索引上 bench 驱动量化收益，产出 block 开/关 × 三路 × 三模式数据 + profile，作为 Phase 2 SIMD 的决策输入。

**非目标**：
- SIMD（Phase 2，§9 决策机制，看 Phase 1 数据与 profile 再定）
- topN by DV / sort / score（引擎无算分，非目标场景）
- Java 侧改造（Lucene 9.12.3 无块级 API，它是基线；Lucene 10 的向量化是格式兼容基线之外的事）
- `SegmentDocIter` 枚举 18.7KB 瘦身（bool-bench-report §11 P2 工程税，独立议题）
- WAND / minCompetitive 早停

**硬约束**：
1. per-doc 路径逐行不变：`RL_BLOCK=0` 回到今天行为，一行不动
2. 命中集合与 per-doc 路径**逐位一致**（顺序、计数、topN docs 列表）
3. P1-1 / P1-3 战果零回归（1M 电池重点行 ±10% 噪声内）
4. 默认 ON 影响所有消费方（CLI + JNI）→ 四路对账 + 逃生门兜底

## 1. 动机与 demo 验证

### 1.1 现状成本结构

逐 doc 路径（`searcher.rs:43-53` driver 循环）每 doc 的固定税：

| 环节 | 出处 | 每 doc 成本 |
|---|---|---|
| `next_doc` enum 分发 | `SegmentDocIter` 14 臂 match（`doc_iter.rs:1495-1512`） | 间接跳转 |
| `io::Result` 检查 | trait 签名 `doc_iter.rs:17-41` | 分支 |
| `NO_MORE_DOCS` 判断 | driver 循环 | 分支 |
| `matches()` 二阶段检查 | driver 循环（非 phrase 恒 true） | 分发 + 分支 |
| `collect` 回调 | `collector.rs` Collector trait | 泛型调用（通常内联） |
| 叶子游标推进 + 越界检查 | 各叶子 `next_doc` | 分支 |

组合器形状（PFOR 路径，`RL_BITMAP=0`）更重：`ConjunctionDocIter` 每候选对其他 child 调 `advance`（skip 表 O(log) 级跳）；`ExcludingDocIter` 每候选对 prohibited 调 `advance`——bool-bench-report §9 记录的 P1-1 病理同源结构。

叶子层**本质已是批量的**：`RoaringDocIter` 内部 `BitmapCursor` 512-doc `next_many` 缓冲逐 doc 弹出；PFOR `DocsEnum/DocsFreqsEnum` 内部 128-block 解码缓冲逐 doc 弹出。块接口对叶子 ≈ 把已有缓冲整体搬出，零新增解码。

### 1.2 demo 实测（本 spec 评审前置验证）

`/tmp/blockdemo/demo.rs`（rustc -O；忠实复刻 14 变体 enum 分发 + `io::Result` 包裹 + driver 循环同构；5M doc 域；hits 逐形状 assert 校验；中位数）：

| 形状 | per-doc | block(128) | 提速 | ns/hit（per-doc → block） |
|---|---|---|---|---|
| term（1M hits，纯分发税） | 3,624 µs | 233 µs | **15.5×** | 3.62 → 0.23 |
| conj（833k hits） | 114,173 µs | 6,626 µs | **17.2×** | 137 → 7.95 |
| excl（1.25M hits） | 85,478 µs | 4,535 µs | **18.8×** | 68.4 → 3.63 |

**保真度交叉验证**：demo per-doc term 3.6 ns/hit vs 真实引擎 PFOR no-fast term high ≈ 3.2 ns/doc（1M 索引实测，bool-bench-report §9）——同量级，term 的 15.5× 是最可迁移的数。

**折扣项**（写进预期管理）：
- conj/excl 的 17–19× 含算法分量（逐候选 advance → 单趟 slice 代数）；真引擎 skip 表比 demo 的二分 advance 略快 → 组合器实际倍数估 **5–12×**
- 真实引擎稀释因素：PFOR 解码两路径共有；roaring bool 走 P1-1 materialize fold（不经迭代）；phrase 两阶段保持 per-doc
- **综合预估真实收益：PFOR 迭代路径 3–10×，roaring 迭代路径 1.5–4×**

**局限**：当前 bench 机 VM 的 PMU 不可用（perf 硬件事件 `not supported`），branch-misses 量化证据缺位；§6 的 perf 环节在有 PMU 的机器执行，否则退化为软件事件火焰图 + 墙钟差。

## 2. 架构总览

```
Searcher::search / count 迭代回退 / top_docs
  ├─ block_enabled() == true（默认；RL_BLOCK=0 关）：
  │    drive_blocks：每 128 docs 一次 enum 分发 + 一次 Result 检查
  │      └─ SegmentDocIter::next_block(&mut DocBlockBuf) → 14 臂 match，每块一次
  │           ├─ 叶子覆写：内部缓冲直接搬出
  │           │    RoaringDocIter = BitmapCursor::next_many 填 out（零中转）
  │           │    DocsEnum/DocsFreqsEnum = 128 解码块切片拷出（freqs 按需）
  │           │    Materialized = docs_from 填 out
  │           │    MatchAll = 算术填充；Bitset = 64-bit 字展开
  │           └─ 组合器覆写：真块代数（共享内核，§4）
  │                Conj/ConjOver = slice intersect（双指针/gallop）
  │                Excl = slice andnot（prohibited 块级跳过，替代逐候选 advance）
  │                Disj/DisjOver = k 路 slice 归并
  │                RoaringAnd/RoaringOr = BitmapCursor 块上复用同一套代数
  │      └─ Collector::collect_block(docs, freqs?)
  │           CountCollector: count += len
  │           TopDocCollector: total += len + 取块前缀补满 n
  │           FreqSumCollector: freqs 求和
  │           默认实现：per-doc collect 回退（外部 collector 兜底）
  └─ block_enabled() == false：今天的 per-doc 循环，一字不动
```

分发次数：**每 doc 2–3 次 → 每 128 docs 1 次**（~100× 摊薄）；`io::Result` / `NO_MORE_DOCS` / `matches` 分支同比例摊薄；块边界规则 → 分支预测友好。

`PhraseDocIter` **不覆写** `next_block`：走 trait 默认 fill（循环 `next_doc + matches` 填 out）——两阶段确认在产出侧吸收，driver 分发仍摊薄，位置解码本就不是分发税。凡含 phrase child 的组合器：child 的 `next_block`（默认 fill）输出已确认块，组合器代数无感知，正确性天然保持。

## 3. trait 与块缓冲形状

```rust
// doc_iter.rs
pub const DOC_BLOCK: usize = 128;   // = PFOR PackedBlock 尺寸（format-notes-postings §level-0）

/// 调用方拥有的填充缓冲（设计偏差说明见下）。
pub struct DocBlockBuf {
    pub docs: [u32; DOC_BLOCK],
    pub freqs: [u32; DOC_BLOCK],   // 仅 needs_freq 的驱动填
    pub len: usize,
}

pub trait DocIter {
    // 现有 doc_id / next_doc / advance / freq / matches 不动 …

    /// 填充 out，返回产出数（0 = 耗尽，此后永不再产块）。
    /// 默认实现：循环 next_doc() + matches() 填 out.docs（两阶段确认在此吸收）。
    /// 覆写者契约：
    ///   (1) out.docs[..n] 已 matches 过滤
    ///   (2) 块内严格升序
    ///   (3) 跨块单调递增（本块首 doc > 上一块末 doc）
    ///   (4) needs_freq 时 out.freqs[..n] 与 docs 对齐填充
    /// debug_assert 锁 (1)-(3)（cfg(debug_assertions) 下抽样校验）。
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize>;
}
```

```rust
// collector.rs
pub trait Collector {
    // 现有 collect / needs_freq 不动 …
    /// 默认 per-doc 回退；块感知 collector 覆写。
    fn collect_block(&mut self, docs: &[u32], freqs: Option<&[u32]>) {
        match freqs {
            Some(f) => for (i, &d) in docs.iter().enumerate() { self.collect(d as i32, f[i]) },
            None => for &d in docs { self.collect(d as i32, 1) },
        }
    }
}
```

**与 §4b 原设计的偏差**：块是**调用方拥有的填充缓冲**（`&mut DocBlockBuf`，填充式返回 `usize`）而非迭代器借出的 `Option<DocBlock { docs: [u32;128], len, freqs: Option<&[u32]> }>`。理由：借出式跨 `&mut self` 调用有生命周期纠缠（组合器需同时持有多个 child 的借出块）；填充式对 Phase 2 SIMD 原地运算更顺手；语义与 codec 已有 `MaterializedBitmap::docs_from` / `FrozenBitmap::docs_from`（`materialized.rs:46-50`）一致。

**栈预算**：`DocBlockBuf` = 128×4×2 + 8 ≈ 1032 B；组合器 child 缓冲随结构体内联（Disj k=8 ≈ 8 KB），`SegmentDocIter` 枚举仍由 Freqs 变体 18.7 KB 封顶（`doc_iter.rs:1507` 注），不新增栈占用；`ConjOver/DisjOver` 的 `Vec<Box>` child 缓冲在堆。

## 4. 组合器块代数（Phase 1 全标量）

每个组合器持有各 child 的 `BlockCursor { buf: DocBlockBuf, pos: usize, len: usize }`。核心简化：**intersect / andnot 输出长度 ≤ 左输入长度 ≤ 128 → conj/excl 每轮"各拉一块算一块"，无超块输出；仅 disj 的 union 可达 k×128，需 child 游标留存与 out 满即停**。

| 组合器 | 块算法（标量） | 复杂度/块 |
|---|---|---|
| Conj / ConjOver | 两 slice 双指针 intersect（长度悬殊时 gallop 跳）；n 元 = 左折叠成对 intersect；空交 → 拉小 max 侧下一块 | O(len_a + len_b) |
| Excl（MUST/MUST_NOT） | must 拉一块；prohibited 拉块直到 `block.max ≥ must.min`（**块级跳过，替代逐候选 advance 病理**）；slice andnot；prohibited 块跨调用留存；prohibited 耗尽 → must 剩余全命中 | O(len_must + 跳过块数) |
| Disj / DisjOver | k 个 child 游标头线性扫最小（k 小，不上堆——堆化是 bool-bench-report §11 P2 议题）；相等头去重；out 满 128 或全 child 耗尽停 | O(k·128) |
| RoaringAnd / RoaringOr | 复用同一套 slice 代数（child = BitmapCursor 块） | 同上 |
| Bitset | 扫 64-bit 字展开 set bits 填 out | O(扫描字数) |
| MatchAll | `docs[i] = cursor + i` 算术填充 | O(128) |

**共享内核**（自由函数，`doc_iter.rs` 内）：

```rust
/// 双指针 intersect，部分消费：返回 (消费 a 数, 消费 b 数, 产出数)。
fn block_intersect(a: &[u32], b: &[u32], out: &mut [u32; DOC_BLOCK]) -> (usize, usize, usize);
/// slice 差集，部分消费：返回 (消费 a 数, 消费 b 数, 产出数)。
fn block_andnot(a: &[u32], b: &[u32], out: &mut [u32; DOC_BLOCK]) -> (usize, usize, usize);
/// k 路有序 slice 归并去重，out 满即停：返回各 child 消费数 + 产出数。
fn kway_union(heads: &mut [(&[u32], usize)], out: &mut [u32; DOC_BLOCK]) -> (Vec<usize>, usize);
```

demo（§1.2）已把这三个内核的缩略版（含部分消费状态机）跑通且 hits 精确对齐。Phase 2 SIMD（shuffle-intersect / SIMD andnot）只替换这三个函数内核，标量版留作对拍参照（M1 §4a 纪律）。

**不变量**（trait doc + debug_assert）：`next_block` 输出 (1) 已 matches 过滤；(2) 块内严格升序；(3) 跨块单调递增；(4) n=0 后永不再产块。

## 5. 驱动与回退语义

```rust
// searcher.rs：三个 driver（search / count 迭代回退 / top_docs）共用的块循环
fn drive_blocks(
    iter: &mut SegmentDocIter, doc_base: u32, needs_freq: bool,
    out: &mut DocBlockBuf, collector: &mut impl Collector,
) -> io::Result<()> {
    loop {
        let n = iter.next_block(out)?;        // 每 128 docs：1 次 enum 分发 + 1 次 Result 检查
        if n == 0 { break; }
        if doc_base != 0 {
            for d in &mut out.docs[..n] { *d += doc_base; }   // Phase 2 可 SIMD 加宽
        }
        collector.collect_block(&out.docs[..n], needs_freq.then(|| &out.freqs[..n]));
    }
    Ok(())
}
```

- **开关**：`block_enabled()` = `OnceLock<bool>` over `RL_BLOCK` 环境变量（仿 `bitmap_enabled()`，`segment_reader.rs:128`），**默认 ON**；`RL_BLOCK=0` 回退。三个 driver 入口 if 分流，per-doc 分支一字不动。
- **matches() 归属**：block driver **不调** `matches()`——两阶段确认由产出侧吸收（叶子无 matches；phrase 走默认 fill；组合器递归继承 child 已确认块）。
- **collector 覆写**：`CountCollector::collect_block = count += len`（`--no-fast-count` 路径直接受益）；`TopDocCollector` = `total += len` + 块前缀补满 n；`FreqSumCollector` = freqs 求和；默认实现兜底外部 collector。
- **top_docs 短路**：段级 fast-count（`searcher.rs:90-98`）+ INDEXORDER "收满 topN 停" 保留；块循环内每块产出后检查 `docs.len() >= top_n && fast.is_some()`（最多少产一块即停）。
- **advance() 不在块路径使用**：driver 纯顺序；Excl 的 prohibited 对齐改为块级拉取跳过。trait `advance` 保留给 per-doc 路径。
- **`drive_materialize`（`query.rs:673`，fold 物化）保持 per-doc**：fold 路径是容器级运算，非测量热点。

## 6. 5M 单段 bench 计划

### 6.1 索引（零新工具）

```bash
# 与 1M boolbench 索引同命令（bool-bench-report.md:185 记录），仅 num_docs 改 5M：
rustlucene-cli logwrite /tmp/boolbench-5m 5000000 42 --bitmap
# force_merge（merge.rs:462）设计上即全并一段；--bitmap 必带——
# 否则合并段不写 RLBM 内联 bitmap，5M roaring 路径测量口径失真
rustlucene-cli forcemerge /tmp/boolbench-5m --bitmap
```

验收：单个 `_N.cfs/.si`、`.si` maxDoc = 5,000,000、delCount = 0、RLBM 文件存在（df ≥ 4096 词有内联 bitmap）；体积 ≈ 1.1–1.2 GB（1M = 227 MB 线性外推）。Java 侧读同一目录（格式兼容既有事实，Java 忽略 RLBM）。

注：1M 索引以同一命令写出即得 2 段（RAM flush 护栏 `max_ram_bytes = 512 MB`，`index_writer.rs:32`）；5M 写入自然产生多段，forcemerge 合并为 1 段。

### 6.2 查询集

复用 `/tmp/boolq.txt`（715 条、14 形状桶）。同 vocab/生成器 → 词 df 等比 ×5（level 词 df ≈ 1M，选择率 ~20% 不变），hits 在报告按桶重列——顺带得到 1M↔5M 标度行为。

### 6.3 测量矩阵（15 cells）

| 引擎 | block 状态 | 模式 |
|---|---|---|
| Rust roaring（默认） | ON / `RL_BLOCK=0` | count、`--no-fast-count`、`--topn` |
| Rust PFOR（`RL_BITMAP=0`） | ON / OFF | count、`--no-fast-count`、`--topn` |
| Java 9.12.3（`--no-cache`） | —（基线，无块能力） | count、`--no-fast-count`、`--topn` |

warmup 3 / iter 10（按 5× 规模从 1M 的 10/30 降档，报告注明）；预估 15 cells × ~1 min ≈ 15 min。TSV 落 `/tmp/bench-5m-{roaring,pfor}{,-iter,-topn}{,-noblock}.tsv` 及 Java 对应件。

### 6.4 机理证据

PFOR 热点桶（no-fast mnfhh / multinot / term high）block 开/关各跑：
- **有 PMU 的机器**：`perf stat -e cycles,instructions,branches,branch-misses` 四项差——直接量化"分支预测失败 + 调用链"机理
- **当前 VM（PMU 不可用）**：退化为 `perf record -e cpu-clock -g` 软件火焰图 + 墙钟差，报告注明证据等级
- 火焰图定位剩余成本（解码 vs 代数 vs collect）→ Phase 2 决策输入

### 6.5 报告

`bool-bench-report.md` 追加 §12：5M 单段方法论、block on/off 对照表（三模式 × 三引擎）、ns/doc 归一化（hits 基准）、branch-miss 差（或证据等级说明）、1M↔5M 标度对比、Phase 2 决策备忘。

## 7. 正确性电池

1. **单元对拍**（核心）：内存测试迭代器（随机 df；含空集、尾块、128 整数倍、交错/不相交/全等 slice）上，每个组合器与三个代数函数的 **block 流 == per-doc 流逐点一致**；collector 块/逐 doc 等价（Count/TopDoc/FreqSum）。
2. **OnceLock 测试冲突处理**：`block_enabled()` 进程级 OnceLock 测试内不可翻转——对拍测试直接调用 `drive_blocks` vs per-doc drive 两个显式函数；**env 开关只是 runtime/bench 逃生门，不进测试路径**。
3. **5M 四路对账**：Rust-block / Rust-noblock / Rust-PFOR / Java 逐 query hits diff 为空 × 三模式（bench 时顺带产出，复用现有 diff 脚本）。
4. **1M 回归电池**：现有 1M 三路三模式在 block ON 下重跑——逐 query 计数 diff 为空；bool-bench-report §9/§10 重点行（mnfhh / multinot / rngmust / rngnot）±10% 噪声内；`RL_BLOCK=0` 数字与 c51e27b 基线逐行一致。
5. `cargo test` 全套绿（core + codec，273+ 基线）。

## 8. 交付物与实施排序

| # | 交付 | 规模 |
|---|---|---|
| 1 | 骨架：`DocBlockBuf` + trait `next_block` 默认 fill + `RL_BLOCK` 开关 + 三 driver 块循环 + `collect_block` 默认回退 + CountCollector 覆写（此步后全链路可跑——叶子未覆写时走默认 fill，先锁正确性） | ~300 行 |
| 2 | 叶子覆写（Materialized / RoaringDocIter / MatchAll / Bitset / DocsEnum / DocsFreqsEnum）+ 对拍单测 | ~250 行 |
| 3 | 共享代数三函数 + 单测；组合器覆写（Conj / Excl / Disj / ConjOver / DisjOver / RoaringAnd / RoaringOr）+ 对拍单测 | ~450 行 |
| 4 | top_docs / FreqSumCollector 块路径 + 单测 | ~80 行 |
| 5 | 5M 索引构建 + 15-cell 矩阵 + perf/火焰图 + 1M 回归电池 | 脚本 + 跑数 |
| 6 | 报告 §12 + Phase 2 决策备忘 | docs |

排序 1→6，每步电池全绿再下一步。步骤 1 结束时已可跑 5M 墙钟初测（默认 fill 只有 driver 侧摊薄），提前暴露集成问题。

## 9. 风险

| 风险 | 缓解 |
|---|---|
| 低 df 小集合形状块开销 ≥ 收益 | 尾块语义天然兜底（len<128 合法）；iterm / low 桶监视；driver 无论块/逐 doc 都是一次调用起步，下行空间有限 |
| 默认 ON 影响 JNI 消费方 | 四路对账（§7-3）+ `RL_BLOCK=0` 逃生门 + 1M 回归逐 query diff |
| 组合器部分消费状态机 bug | demo 已验证缩略版；单元测试覆盖空/尾/整数倍/交错四象限；5M 四路对账终验 |
| 5M 索引构建内存 | `max_ram_bytes` 512 MB 默认 flush 护栏（`index_writer.rs:32`），5×1M 段已有先例 |
| /tmp 磁盘（≈1.2 GB + TSV） | 构建前 df 检查；TSV 体量 <100 MB |
| bench 机 PMU 不可用 | §6.4 证据等级降级方案 |

## 10. Phase 2（SIMD）决策机制

不预设数字门槛。Phase 1 数据 + profile 出来后评审：
- **输入**：block on/off 各形状提速表；火焰图中三代数内核占 PFOR 迭代总时间的比例；（有 PMU 时）branch-miss 差
- **候选动作**：`block_intersect` 换 `_mm_shuffle_epi8` Lemire 式查表交（标量版留作对拍）；`block_andnot` SIMD 化；driver 的 doc_base 加法 SIMD 加宽；dense 形状 count 退化 bitset popcount（§4b 原设计）
- **纪律**：M1 §4a——只对 profile 证实的热点启用；每个 SIMD 内核配标量对拍单测；格式字节零变动

## 附录 A：demo 复现

```bash
cd /tmp/blockdemo
rustc -O --edition 2021 demo.rs -o demo
for d in perdoc block; do for c in term conj excl; do ./demo $d $c; done; done
# 每形状输出 13 次运行（3 预热 + 10 测量）+ SUMMARY 中位数；hits assert 校验
```

demo 结构：`VecIter` 叶子（5M 域等差 doc 集）+ 14 变体 `Iter` enum（Leaf / Conj / Excl + 11 stub 变体凑分发表规模）；per-doc driver 与 `searcher.rs::search` 逐行同构；block driver = `next_block` + `count += n`；组合器块代数 = §4 内核缩略版（含部分消费游标状态机）。
