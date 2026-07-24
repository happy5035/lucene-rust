# M4 bitmap 读侧去税（格式 v2 去 crc / 零拷贝 RoaringView / AND 偏斜 probe / OR 字节游标 / Term count 回归）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 M3（df ≥ 4096 term 的内联 Roaring bitmap，格式 v1 带 per-bitmap crc32）之上实施 M4 读侧去税，对应已批准 spec `docs/superpowers/specs/2026-07-24-rust-search-m4-bitmap-zero-copy-design.md` 的全部范围（§8 任务切分，共 5 个任务）：① **格式 v2** 去 per-bitmap crc32（version != 2 → 静默落档，v1 零迁移）；② **零拷贝 RoaringView**（probe / full 两种打开模式）取代查询路径上的 deserialize-rebuild；③ **AND df 偏斜 probe**（高 df 侧不做全量读取：档 2 物化候选过 `contains` 过滤、档 1 按 `SKEW_RATIO` 在"最小侧迭代 + probe 其余"与"字节游标 merge-intersect"间选择）；④ **OR/纯迭代字节游标**（去重建税）+ `RoaringDocIter` 改包视图游标；⑤ **Term count 回归 `doc_freq` 直读**，`read_term_bitmap_header` 全链路删除。postings 主格式字节不动、Java 读写零感知（CheckIndex 仍 "No problems"）、`make log-test` 五变体脚本与 Makefile 零改动、bench 按 M3 T7 同口径三路复测并标定 `SKEW_RATIO`。

**Architecture:** codec 层 `roaring.rs` 格式升级 v2（`BITMAP_VERSION = 2`、`max_bitmap_len` 上界去 4B crc、`serialize`/`deserialize` 去 crc 字段；`write_term_bitmap` 与 `postings.rs` 写侧 hook 零改动），新增 `crates/codec-lucene9/src/roaring/view.rs` 零拷贝视图：`scan_directory` 容器目录扫描（数据段长度由 type+card/numRuns 推出、seek 跳过），`open_probe`（只扫目录，`contains(doc)` 按容器类型定点测位：array 读该桶数据段内二分 / bitset 读单个 u64 字 / run 读 run 对判区间）与 `open_full`（区域一次顺序读入 buffer），字节游标 `ViewCursor`（契约逐语义镜像 M3 `RoaringCursor`）；`PostingsReader` 新增 `open_term_bitmap`（full）/ `probe_term_bitmap`（probe），复用 `locate_bitmap_region` + `fresh_input` 模式。core 层 `SegmentReader` 同名包装（`bitmap_enabled()` 门不变）；`doc_iter.rs` 新增 `DocSource`（full view / 物化 slice 统一升序游标）、`RoaringAndDocIter`（merge-intersect + probe 过滤）、`RoaringOrDocIter`（k 路 merge-union），`RoaringDocIter` 改包 `RoaringView` 视图游标；`roaring_exec.rs` AND 走档 2 probe 过滤 / 档 1 `SKEW_RATIO` 策略，OR 走 merge-union，count 与迭代共用同一引擎；`searcher.rs` Term count 删 `read_term_bitmap_header` 调用直读 `doc_freq`；codec `PostingsReader::read_term_bitmap`（deserialize 版）与 `read_term_bitmap_header`、`SegmentReader` 两个包装一并删除。验证三层不变：单测（view 三容器/游标/probe 对拍、v1 拒绝与落档、skew on/off 等价）→ Rust bitmap on/off 逐位一致 → `make log-test` 五变体 + 三路 bench 复测报告（`.superpowers/sdd/m4-bench-report.md`，gitignored）。

**Tech Stack:** Rust（`crates/codec-lucene9` edition 2024、`crates/core` edition 2021；codec `#![deny(unsafe_code)]`（仅既有 `postings_ll/simd.rs` 与 `roaring/simd.rs` 两个模块级 `#[allow(unsafe_code)]`，**本计划不新增任何 unsafe**——view/probe/merge 全部安全代码），core `#![forbid(unsafe_code)]`，统一 `io::Result`）；不新增依赖（crc32fast 保留：footer CRC 在 `codec_util.rs:211`、`io.rs:191,941` 使用；roaring.rs 停止使用）；Java 9.12.3（`interop/java/lib/lucene-core-9.12.3.jar`）电池/bench 工具链不变。

## Global Constraints

（摘自 spec 与既有项目惯例，逐字或就近转述；所有 Task 共同遵守）

- **postings 主格式字节不动**。crc 去除只改 bitmap region 内部（term 间缝隙字节）；`.tim/.tip/.tmd/.pos/.psm` 及 FST output schema 零改动。不写 `--bitmap` 的索引与 M2/M3 字节级一致（`--bitmap` 默认 off）。
- **v2 布局逐字（spec §3）**：
  ```
  [ magic(4B "RLBM") + version(1B) = 2 + df(vInt) + cardinality(vInt) + payload ][ len: u32 LE ]
  ```
  无 crc32 字段；len = 头+payload 字节数（len 上界公式同步去 4B：**`max_bitmap_len(max_doc) = 20 + ⌈max_doc/65536⌉ × 8201`**）。payload 布局与 v1 逐字节相同（numContainers vInt；逐 container：key(u16 LE) type(u8: 0=array,1=bitset,2=run) card(vInt) data；array = card×u16 LE 升序，bitset = 8192B = 1024×u64 LE，run = numRuns(vInt) + numRuns×(start,end) u16 LE 闭区间升序不重叠）。
- **版本即迁移**：`version != 2` → `Ok(None)` 静默落档 postings（M3 v1 索引零迁移、零特判，查询永不报错）。兜底校验三重（全部廉价）：len 有界 → magic/version → 头内 df/cardinality == `termState.doc_freq`；.doc footer CRC 仍是 Lucene 级完整性兜底（CheckIndex 全文件校验）。
- **不新增外部 crate**。
- **unsafe 政策**：core 保持 `#![forbid(unsafe_code)]`；codec 不新增 unsafe（标量先行，bitset 测位是随机内存读，无 SIMD 需求）。
- **`RoaringBitmap::deserialize` 保留**给写侧 round-trip / 结构拒绝测试（同步去 crc），查询路径不再调用（T4 后 grep 验证）；`RoaringBitmap::{and,or,cursor*}` 容器库 API 保留（pub，无 dead_code 风险）。
- **ConstantScore/needs_freq 禁入不变**：bitmap 无 freq/positions；`needs_freq == true` 一律档 3（query.rs 既有门不动）；roaring 路径 `freq()` 恒为 1。
- **`RL_BITMAP=0`** kill switch 保留：两个新 wrapper 内联同一 `bitmap_enabled()` 门（segment_reader.rs:129-132），on/off A/B 同二进制同索引。
- **YAGNI（spec §2 明确不做）**：写侧容器语义/runOptimize 不动（只去 crc 字段）；OR 的全量字节读取不做避免（只去重建税）；不做跨查询 bitmap 缓存（view 打开成本已降至微秒级）；multi-term（M2 >16 bitset 路径）不做 roaring 集成（仍二期）；NRT 不做；除 crc 去除外零格式演进。
- **测试命令**：codec 层 `cargo test -p codec-lucene9 <test名>`，core 层 `cargo test -p rustlucene-core <test名>`；收尾门槛 `make log-test`（200000 文档五变体：seed 42 默认 / 43 `--positions` / 44 `--sparse` / 45 `--bigdict` / 46 `--bitmap`，Makefile:17-22，本计划零改动），较慢，只在 T5 与最终收尾使用；T1–T4 的增量用 `cargo test` 覆盖。
- **Lucene 语义照抄**：关键决策在代码注释中给 `File.java:line` 引用（Java 源码在 `reference/lucene-9.12.3/lucene/core/src/java/org/apache/lucene/`，引用省略该前缀）。找不到精确行号时引用类名 + 方法名，不编行号。
- **commit message 前缀**：`feat:` / `fix:` / `docs:` / `bench:` / `test:`（沿用 git log 现有风格）。
- **实验/报告文件**：bench 数据与报告写到 `.superpowers/sdd/`（M2 起已 gitignored），不进 git。
- **验收门槛**：`cargo fmt --check` 干净；`cargo test` 两 crate 全绿；`make log-test` 五变体全绿且每次 CheckIndex 输出 "No problems"；`--bitmap` 变体内 Rust bitmap on/off searchdump diff 为空；v1 落档 Rust 测试（codec + core 两个）绿；T5 bench 的 per-query hit-counts 按 M3 口径三路对拍通过。

## Pre-checks（已执行，基线绿；HEAD c5f4392）

```
$ cargo build
    Finished `dev` profile [unoptimized + debuginfo] target(s)
$ cargo test -p codec-lucene9
test result: ok. 156 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
$ cargo test -p rustlucene-core
test result: ok. 43 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
$ cargo fmt --check && echo FMT_OK
FMT_OK
```

## 关键设计事实（本计划全部代码的字节级依据，已逐项对照本仓库源码核实）

1. **v2 布局与 len 上界推导**（spec §3；校验①"len 有界"的精确公式）：v1 推导（M3 计划关键设计事实 3）为 `24 + ⌈maxDoc/65536⌉ × 8201` = 公共头 ≤ 15B + numContainers vInt ≤ 5B + crc 4B + 每 container ≤ 8201B；v2 去 crc 4B → **`20 + ⌈maxDoc/65536⌉ × 8201`**（maxDoc=200000 时上界 32824B）。该上界只依赖 maxDoc 与 container 尺寸上限，与 df、threshold 无关。
2. **写侧 hook 零改动**：`crates/codec-lucene9/src/postings.rs:368-372`（`write_term` 中 `doc_start_fp` 捕获之前，`crate::roaring::write_term_bitmap(&mut self.doc_out, docs)?`）。crc 只存在于 `roaring.rs` 的 `serialize`（roaring.rs:769-772）——**去 crc 的全部写侧改动闭合在 roaring.rs 内**（`BITMAP_VERSION`、`max_bitmap_len`、`serialize` 去尾部 4 行、`deserialize` 去 crc 校验），postings.rs / 参数流 / footer CRC（同一 `ChecksumIndexOutput` 顺序流过 bitmap 字节，codec_util.rs:211）一行不动。
3. **版本门 = 唯一迁移机制**：`deserialize` 与 view 的 `scan_directory` 都比对 `BITMAP_VERSION` 常量（改为 2）；v1 字节（M3 布局 = version 1 + 尾部 crc32）在 version 检查即被拒，crc 尾部根本不会被读到。v1 拒绝测试用**测试专用 v1 写入器**构造：v2 序列化输出 → `bytes[4] = 1` → 追加 `crc32fast::hash(bytes)`——与 M3 `serialize` 产出逐字节相同（crc 只追加在尾部，头/payload 偏移不变）。
4. **读侧定位零改动**：`locate_bitmap_region`（postings_read.rs:146-167）：df ≥ `BITMAP_MIN_DF`(4096) 门 → `fp >= 4` → `seek(fp-4)` 读 len → 校验① `0 < len ≤ max_bitmap_len(max_doc)` → `fp - 4 >= len`（永不 seek 到 0 以下）→ `Some((fp-4-len, len))`。len 上界走 `max_bitmap_len` 新公式自动生效。
5. **fresh_input 模式**（postings_read.rs:136-138）：`self.doc_in.slice(0, self.doc_in.length())` 拿独立定位流；`open_term_bitmap` / `probe_term_bitmap` 各持有一份，与 postings 枚举互不干扰。本系统从不写 CFS（segment_info.rs:57），slice 即普通文件 + base offset。
6. **IndexInput 是 8KB 内联缓冲的定位流**（io.rs:597 `INPUT_BUFFER_CAPACITY = 1 << 13`，struct io.rs:644-651）→ (a) 持有 view 的迭代器一律 `Box<RoaringView>`（probe view 内含 IndexInput ≈ 8.3KB，不可内联进 `SegmentDocIter`——query.rs:199-205 的 18.7KB 教训）；(b) probe 的随机 seek 逻辑上是 8B/桶级读取，物理上是一次 ≤8KB refill（与 Lucene BufferedIndexInput 同形），spec §4 "8B/probe" 指逻辑读放量。
7. **容器数据长度可从目录推出**（probe 模式不读数据段的前提，与 `serialize` roaring.rs:741-768 逐项核对）：container 头 = key(2B) + type(1B) + card(vInt)；array 数据 = `2×card` B；bitset = `8192` B；run = 先读 numRuns vInt（目录扫描的一部分），再 `4×numRuns` B。数据段越界（`data_off + data_len > len`）→ None（结构校验，替代 v1 的逐元素校验 + crc）。
8. **ViewCursor 契约 = RoaringCursor 契约逐语义镜像**（roaring.rs:608-704）：纯数据 `{ci, a, b}`（array → a = 下一元素下标；bitset → a = 下一待查 bit；run → a = run 下标、b = run 内偏移）；`cursor_next` 返回游标处 doc 或 None；`cursor_advance` 返回首个 ≥ target 的 doc，**forward-only：target 必须 > 上次返回值**（DocIter 的 advance 契约保证；merge-intersect 中靠 `current < target` 守卫满足，见 T3）。
9. **档判定 / needs_freq / RL_BITMAP 门不变**：`collect_bool_entries`（roaring_exec.rs:22-47）df 升序排序（cardinality == df，校验③保证 → 档 1 排序直接用 entries 顺序）；`and_segment_iterator`/`or_segment_iterator`（query.rs:206-269）的 `if !needs_freq` 门不动；count 的 And/Or 分支（searcher.rs:100-125）不动，只换 `roaring_exec::count` 内部实现。
10. **count 走同一引擎（spec §5 "And/Or count 走同一引擎"）**：M4 没有折叠出的 bitmap 对象，交集/并集计数 = 同一迭代器驱动到尽头的计数循环——正确性由构造保证（count == 迭代结果数）。`roaring_exec::count` 签名不变（`io::Result<Option<u64>>`，None = 档 3）。
11. **`read_term_bitmap_header` 删除集合**（T4，已全部 grep 核实）：codec 定义 postings_read.rs:196-222、core 包装 segment_reader.rs:95-101、唯一消费点 searcher.rs:71（Term count 分支）。其校验逻辑（locate + magic/version + df + card）已被 view 的 `scan_directory` 完全覆盖，无功能损失。
12. **`RoaringBitmap::{and,or,deserialize,cursor*}` 在 T4 后离开查询路径但保留**：写侧 round-trip（roaring.rs:1110 `serialize_deserialize_round_trip`、postings_read.rs:1457 `inline_bitmap_region_round_trip`）与结构拒绝测试（roaring.rs:1267）继续使用；pub API 无 dead_code 警告。容器语义（runOptimize 等）一行不动。
13. **电池语料事实（M3 T6 已核，仍成立）**：200000 文档 log 语料 level 五 term df≈40000（≥4096 有 bitmap）、message df≈2700（无 bitmap）、trace_id df=1 → 电池覆盖 Term bitmap / 档 1 / 档 3 / 无 bitmap 自然落档 / on-off A/B / Java forceMerge；**档 2 与偏斜 probe 由单测覆盖**（电池组不出混合子句，脚本零改动——v1 落档覆盖同样是 Rust 测试，不加电池变体）。
14. **bench 口径 = M3 T7 逐字**（`.superpowers/sdd/task-7-report.md` + `m3-roaring-bench-report.md` 附录命令，均已核对存在）：1M docs seed 42、Java 侧 `--no-cache`、`--warmup 10 --iter 30`、三路串行、查询文件经 **Java 侧** `SearchBench <java索引> message --dump-queries <out> --tasks 50 --seed 42` 产出（498 行）+ `awk -F'\t' '!($1=="TERM" && $4<4096)'` 守卫（剔除同样 5 条 med 截断词 → 493 行）；hit-counts 走 stderr（`2>` 重定向）；Java 侧 stderr 过滤 JVM 噪声 + `term=` 行抽样差异按 M3 报告 §5 的排序多集等效验收（非 term= 行 sorted diff 为空 + term= 重合行 awk join mismatches=0）。df 全景（task-7-report §7.2）：message high df 10016–19206、med df 4096–9898、level df≈200000。**SKEW_RATIO 标定输入 = `.superpowers/sdd/m3-roaring-compare-report.md`**（存在，160 行，已核）：sparse df≈18.8k array——iterate 3.84 ns/doc、deserialize 244.9µs（≈13 ns/doc 全量税；M4 零拷贝 full 读去掉逐元素校验后只剩顺序字节读）；dense df≈200k bitset——iterate 6.46 ns/doc、deserialize 171.6µs；ultra df=990k run——deserialize 1.43µs（税≈0）、iterate 4.62 ns/doc。
15. **SKEW_RATIO 语义决议**（spec §5 措辞解读）：偏斜比 = `max_df / min_df`（entries 已 df 升序 → `entries.last().0 / entries[0].0`）；**≥ SKEW_RATIO（初始 4）→ 偏斜：最小侧全量模式迭代 + 其余 probe**（用户指令②，高 df 侧不做全量读取）；**< SKEW_RATIO → 非偏斜：全部 full 模式 k 路字节游标 merge-intersect**（spec §5 "双字节游标"，2 子句为常见情形，k 路自然泛化）。常量 `pub(crate) const SKEW_RATIO: u64 = 4` 放 roaring_exec.rs，标注 bench 标定（T5）。
16. **T4 删除集合（已全部 grep 核实）**：codec `read_term_bitmap`（postings_read.rs:169-188，含文档注释）的唯一生产消费者是 `SegmentReader::read_term_bitmap` 包装（segment_reader.rs:86-91），该包装的消费者是 query.rs:136（Term 分支）与 roaring_exec.rs:75（`fold_clauses`）——T4 全部切换到视图后，四个符号（codec `read_term_bitmap`/`read_term_bitmap_header` + core 两个包装）一并删除。测试侧：`read_term_bitmap_validates_and_reads`（postings_read.rs:1519-1567）随删（其覆盖 = T2 `open_and_probe_term_bitmap_match_postings` + `inline_bitmap_region_round_trip` 的并集，无损失）；T1 新增的 `read_term_bitmap_falls_back_on_v1_layout` 两条断言改走 `open_term_bitmap`/`probe_term_bitmap`。删除后 `grep -rn "RoaringBitmap\|RoaringCursor" crates/core/src` 为空（core 只碰视图 API）；`postings_read.rs` 的 import 同步从 `use crate::roaring::{self, RoaringBitmap, RoaringView};` 去掉 `RoaringBitmap`（`roaring::` 自引用保留：`locate_bitmap_region` 用 `BITMAP_MIN_DF`/`max_bitmap_len`）；`RoaringBitmap::{and,or,deserialize,cursor*}` 留在 codec 写侧 round-trip/结构拒绝测试（事实 12）。
17. **OR presence 探测 = full 打开本身**：OR 的每个 bitmap 子句本来就要 full 视图做 merge-union，`open_term_bitmap` 返回的 Option 即档判定（全 None → 档 3）——不需要 T3 AND 路径的 `probe_clauses` 预扫（那是为大侧不读数据段才存在的，用户指令②）。OR count 与迭代共用 `or_iterator`（spec §5/§6 同一引擎：驱动同一迭代器到尽头计数，事实 10）；M3 的 cardinality 折叠捷径随 `fold_clauses` 删除退出查询路径（M4 没有折叠出的 bitmap 对象）。

---
## Task 1: 格式 v2（写侧去 crc + serialize/deserialize 同步 + v1 拒绝/落档测试）

**Files:**
- Modify: `crates/codec-lucene9/src/roaring.rs`（模块文档、`BITMAP_VERSION`、`max_bitmap_len`、`serialize`、`deserialize` + 测试）
- Modify: `crates/codec-lucene9/src/postings_read.rs`（`read_term_bitmap` 文档注释四重→三重 + 新增 v1 落档测试）
- Modify: `crates/core/src/search/mod.rs`（新增 core 侧 v1 落档测试 + doctor helper）
- Test: 上述三文件的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: `crc32fast`（codec 已有依赖，测试专用 v1 写入器用）；`crate::io::{DataInput, DataOutput, IndexInput, IndexOutput}`；core 测试用 `codec_lucene9::segment_infos::SegmentInfos::read_latest(&dir) -> io::Result<(SegmentInfos, u64)>`（reader.rs:22 同型）、`FieldInfos::read(dir, segment, segment_id, "")`、`TermsDict::open(dir, segment, segment_id, &fis)`（postings_read.rs:902-906 同型）。
- Produces（T2 依赖这些名字，不得改名）:
  ```rust
  // roaring.rs（签名不变，语义/常量更新）
  pub const BITMAP_MAGIC: [u8; 4] = *b"RLBM";
  pub const BITMAP_VERSION: u8 = 2;                       // 1 → 2
  pub fn max_bitmap_len(max_doc: u32) -> u64;             // 24 + … → 20 + …
  impl RoaringBitmap {
      pub fn serialize(&self, df: u32) -> Vec<u8>;        // 头+payload（无 crc、无 len 后缀）
      pub fn deserialize(bytes: &[u8], expected_df: u32) -> Option<RoaringBitmap>; // 无 crc 校验；version != 2 → None
  }
  pub fn write_term_bitmap(out: &mut impl DataOutput, docs: &[u32]) -> io::Result<()>; // 零改动
  // roaring.rs 测试模块
  pub(crate) fn serialize_v1_for_test(b: &RoaringBitmap, df: u32) -> Vec<u8>; // 测试专用 v1 写入器（T2 view 测试跨模块复用，必须 pub(crate)）
  // postings_read.rs（T1 零签名改动；T2 才加 open/probe）
  pub fn read_term_bitmap(&self, entry: &TermEntry, max_doc: u32) -> io::Result<Option<RoaringBitmap>>;
  pub fn read_term_bitmap_header(&self, entry: &TermEntry, max_doc: u32) -> io::Result<Option<u64>>;
  // core 测试模块
  fn doctor_bitmap_version(root: &std::path::Path, field: &str, term: &[u8], version: u8);
  ```

  语义决定：v2 下 `deserialize` 的校验集 = magic/version/df/card/结构不变式（容器 key 严格升序、type 合法、card ≥ 1 且 ≤ 65536、array 元素升序、bitset popcount == card、run s ≤ e 不重叠且 sum == card、card 总和 == df、无尾部字节）——除 crc 外与 v1 逐项相同；`version != 2`（含 v1）在第一道即 None。`serialize` 输出不再含尾部 4B（调用点 `write_term_bitmap` 的 len 后缀语义不变：len = serialize 输出长度）。写侧 hook（postings.rs:368-372）与参数流零改动。

### Steps

- [ ] **Step 1.1: 改测试为 v2 期望 + 新增 v1 拒绝/落档测试（先失败）**

  ① `crates/codec-lucene9/src/roaring.rs` 测试模块：在 `mod tests` 内新增测试专用 v1 写入器与 v1 拒绝测试：

  ```rust
      /// Test-only v1-layout writer (M3 format): the v2 image with the
      /// version byte rewound to 1 and crc32fast(header+payload) appended —
      /// byte-for-byte what M3's serialize produced (the crc only trailed,
      /// so all header/payload offsets are unchanged).
      /// pub(crate): T2 的 `roaring::view::tests` 经
      /// `use super::super::tests::serialize_v1_for_test` 跨模块复用——
      /// 非 `roaring::tests` 的后代模块，私有 fn 不可见。
      pub(crate) fn serialize_v1_for_test(b: &RoaringBitmap, df: u32) -> Vec<u8> {
          let mut bytes = b.serialize(df);
          bytes[4] = 1; // BITMAP_VERSION v1
          let crc = crc32fast::hash(&bytes);
          bytes.extend_from_slice(&crc.to_le_bytes());
          bytes
      }

      #[test]
      fn v2_serialize_has_no_crc_and_rejects_v1() {
          let docs = shaped_docs(&mut Rng(5), &[(0, 100), (1, 5000)]);
          let b = RoaringBitmap::from_sorted_docs(&docs);
          let v2 = b.serialize(docs.len() as u32);
          assert_eq!(&v2[..4], b"RLBM");
          assert_eq!(v2[4], 2, "format v2");
          // round-trips under v2
          let back = RoaringBitmap::deserialize(&v2, docs.len() as u32).unwrap();
          assert_eq!(to_vec(&back), docs);
          // the M3 v1 image (valid crc!) must be rejected at the version gate
          let v1 = serialize_v1_for_test(&b, docs.len() as u32);
          assert_eq!(v1[4], 1);
          assert!(
              RoaringBitmap::deserialize(&v1, docs.len() as u32).is_none(),
              "v1 layout must be rejected even with a valid v1 crc"
          );
      }
  ```

  ② 同文件 `serialize_deserialize_round_trip`（roaring.rs:1110）的 corruption 段更新——v2 无 crc，尾部字节即 payload，不再有保证可检出的 "crc" 位；payload 结构违规由 `deserialize_rejects_structural_violations` 覆盖。把：

  ```rust
              // corrupted magic / version / crc / payload -> None
              for (pos, tag) in [(0usize, "magic"), (4, "version"), (bytes.len() - 1, "crc"), (bytes.len() / 2, "payload")] {
                  let mut bad = bytes.clone();
                  bad[pos] ^= 0xFF;
                  assert!(RoaringBitmap::deserialize(&bad, docs.len() as u32).is_none(), "case {ci} corrupt {tag}");
              }
  ```

  改为：

  ```rust
              // corrupted magic / version -> None (v2: payload integrity is
              // the .doc footer CRC's job; structural violations are covered
              // by deserialize_rejects_structural_violations)
              for (pos, tag) in [(0usize, "magic"), (4, "version")] {
                  let mut bad = bytes.clone();
                  bad[pos] ^= 0xFF;
                  assert!(RoaringBitmap::deserialize(&bad, docs.len() as u32).is_none(), "case {ci} corrupt {tag}");
              }
  ```

  ③ 同文件 `max_bitmap_len_bound_holds`（roaring.rs:1148）的两个期望值改为：

  ```rust
          assert_eq!(max_bitmap_len(200_000), 20 + 4 * 8201);
          assert_eq!(max_bitmap_len(1), 20 + 8201);
  ```

  ④ 同文件 `deserialize_rejects_structural_violations`（roaring.rs:1267）：删除 `with_fresh_crc` helper 与全部三处调用（v2 无 crc，doctored bytes 直接喂；布局偏移不变——crc 只曾在尾部）。即删除：

  ```rust
          /// Recomputes the trailing crc32 so ONLY a structural check can
          /// reject the bytes (a plain bit-flip is always caught by the crc).
          fn with_fresh_crc(mut bytes: Vec<u8>) -> Vec<u8> {
              let n = bytes.len();
              let crc = crc32fast::hash(&bytes[..n - 4]);
              bytes[n - 4..].copy_from_slice(&crc.to_le_bytes());
              bytes
          }
  ```

  并把三处 `let bad = with_fresh_crc(bad);` 删除（`bad` 直接使用），三个断言原样保留。

  ⑤ `crates/codec-lucene9/src/postings_read.rs` 测试模块：新增 v1 落档测试（doctor 盘上 version 字节 → v2 读侧拒绝 → postings 逐 doc 不变 = fallback-results-equal-PFOR 的 codec 层形态）：

  ```rust
      /// M4 §3 版本即迁移：把盘上 bitmap region 的 version 字节改回 1（v1
      /// 布局的判定点；v2 region 无 crc 可补，也无需补——version 检查先于
      /// 一切），v2 读侧必须静默落档且 postings 本身逐 doc 不变。
      #[test]
      fn read_term_bitmap_falls_back_on_v1_layout() {
          let root = temp_dir("bitmap-v1");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_segment_bitmap(&dir);
          let e = seek(&dir, &fis, "tx", b"hot");
          // doctor the on-disk version byte 2 -> 1 (in place; the doctored
          // index never goes through CheckIndex — footer CRC mismatch is
          // expected and irrelevant here)
          let fp = e.state.doc_start_fp;
          let doc_file = root.join(crate::postings::file_name("_0", "doc"));
          let mut bytes = fs::read(&doc_file).unwrap();
          let len =
              u32::from_le_bytes(bytes[(fp - 4) as usize..fp as usize].try_into().unwrap()) as u64;
          let region_start = (fp - 4 - len) as usize;
          assert_eq!(&bytes[region_start..region_start + 4], b"RLBM");
          assert_eq!(bytes[region_start + 4], 2, "write side must emit v2");
          bytes[region_start + 4] = 1;
          fs::write(&doc_file, &bytes).unwrap();
          // v2 reader: full + header paths both reject at the version gate
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
          assert!(postings.read_term_bitmap(&e, 6000).unwrap().is_none());
          assert_eq!(postings.read_term_bitmap_header(&e, 6000).unwrap(), None);
          // fallback correctness: the postings themselves are untouched
          let mut en = postings.docs_and_freqs(&e).unwrap();
          for expected in 0..5000 {
              assert_eq!(en.next_doc().unwrap(), expected);
              assert_eq!(en.freq(), 1);
          }
          assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  ⑥ `crates/core/src/search/mod.rs` 测试模块：新增 doctor helper 与 core 侧端到端 v1 落档测试（M3 写出的 v1 bitmap 索引在 v2 读侧静默落 postings、结果与 bitmap-off 索引逐位一致）：

  ```rust
      /// M4 v1 落档测试的 doctor helper：经 terms dict 定位 `term` 的
      /// bitmap region（docStartFP-4 读 len 回退），把 version 字节改写为
      /// `version`（codec 的 `postings::file_name` 是 pub(crate)，core 侧
      /// 按 `{segment}_Lucene912_0.doc` 拼名——SEGMENT_SUFFIX 即
      /// "Lucene912_0"，见 postings.rs:45-47）。
      fn doctor_bitmap_version(root: &std::path::Path, field: &str, term: &[u8], version: u8) {
          use codec_lucene9::field_infos::FieldInfos;
          use codec_lucene9::segment_infos::SegmentInfos;
          use codec_lucene9::terms_read::TermsDict;
          let dir = FSDirectory::open(root).unwrap();
          let (infos, _) = SegmentInfos::read_latest(&dir).unwrap();
          let sci = &infos.segments[0];
          let fis = FieldInfos::read(&dir, &sci.info.name, &sci.info.id, "").unwrap();
          let mut dict = TermsDict::open(&dir, &sci.info.name, &sci.info.id, &fis).unwrap();
          let fi = fis.by_name(field).unwrap();
          let entry = dict.seek_exact(fi, term).unwrap().expect("term exists");
          let fp = entry.state.doc_start_fp;
          let doc_path = root.join(format!("{}_Lucene912_0.doc", sci.info.name));
          let mut bytes = std::fs::read(&doc_path).unwrap();
          let len =
              u32::from_le_bytes(bytes[(fp - 4) as usize..fp as usize].try_into().unwrap()) as u64;
          let start = (fp - 4 - len) as usize;
          assert_eq!(&bytes[start..start + 4], b"RLBM");
          bytes[start + 4] = version;
          std::fs::write(&doc_path, &bytes).unwrap();
      }

      /// M4 §3/§7 v1 落档：bitmap 索引 doctor 回 v1（version 字节）后，v2
      /// 读侧静默落 postings——路径断言不再是 Roaring 变体，全部查询结果
      /// 与 bitmap-off 索引逐位一致。
      #[test]
      fn bitmap_v1_index_falls_back_to_postings() {
          let root_off = temp_dir("v1off");
          let root_on = temp_dir("v1on");
          write_bitmap_corpus(&root_off, false);
          write_bitmap_corpus(&root_on, true);
          doctor_bitmap_version(&root_on, "message", b"hot", 1);

          // 路径断言：hot 不再走 roaring（v1 region 被拒）
          let dir = FSDirectory::open(&root_on).unwrap();
          let mut reader = Reader::open(&dir).unwrap();
          let (_base, seg) = reader.leaves().next().unwrap();
          let it = Query::term("message", "hot")
              .segment_iterator(seg, false)
              .unwrap()
              .unwrap();
          assert!(
              !matches!(it, SegmentDocIter::Roaring(_)),
              "v1 region must fall back to postings"
          );
          drop(reader);

          // 全量等价：与 bitmap-off 索引逐位一致（PFOR 结果）
          let mut s_off = Searcher::open(&FSDirectory::open(&root_off).unwrap()).unwrap();
          let mut s_on = Searcher::open(&FSDirectory::open(&root_on).unwrap()).unwrap();
          let battery: Vec<Query> = vec![
              Query::term("message", "hot"),
              Query::term("message", "t3"),
              Query::and("message", &["hot", "t3"]),
              Query::or("message", &["hot", "t3"]),
              Query::terms("message", &["hot", "t3", "nosuch"]),
              Query::prefix("message", "ho"),
          ];
          for q in &battery {
              let (a_total, a_docs) = s_off.top_docs(q, 6000).unwrap();
              let (b_total, b_docs) = s_on.top_docs(q, 6000).unwrap();
              assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
              assert_eq!(s_off.count(q).unwrap(), s_on.count(q).unwrap(), "count {q:?}");
          }
          assert_eq!(s_on.count(&Query::term("message", "hot")).unwrap(), 5000);
          fs::remove_dir_all(&root_off).unwrap();
          fs::remove_dir_all(&root_on).unwrap();
      }
  ```

- [ ] **Step 1.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 roaring 2>&1 | tail -8
  failures: roaring::tests::v2_serialize_has_no_crc_and_rejects_v1
            roaring::tests::max_bitmap_len_bound_holds
  $ cargo test -p codec-lucene9 read_term_bitmap_falls_back_on_v1_layout 2>&1 | tail -3
  FAILED（v1 仍被接受：BITMAP_VERSION 还是 1）
  $ cargo test -p rustlucene-core bitmap_v1_index_falls_back_to_postings 2>&1 | tail -3
  FAILED（hot 仍走 Roaring 变体 / count 路径仍命中）
  ```

- [ ] **Step 1.3: roaring.rs 格式 v2 实现** — 五处编辑（均为精确替换）：

  ① 模块文档注释（roaring.rs:1-7）改为：

  ```rust
  //! Roaring bitmap containers (array / bitset / run) for the inline
  //! per-term bitmap in the .doc stream (M3 spec §3–§6, M4 spec §3 格式 v2):
  //! build from a sorted doc list with runOptimize, container-level boolean
  //! and/or, cardinality, forward-only cursor iteration, and the
  //! `[header+payload]` wire format written ahead of a term's postings.
  //! Format v2 (M4 §3): the per-bitmap crc32 is gone — validation is the
  //! cheap triple gate (len bound → magic/version → header df/card ==
  //! doc_freq) plus structural invariants; payload integrity stays with the
  //! .doc footer CRC. Zero-copy query-path views live in `roaring/view.rs`
  //! (M4 T2); SIMD fast paths live in `roaring/simd.rs`; this file carries
  //! the scalar reference every kernel is pinned against.
  ```

  ② `BITMAP_VERSION`（roaring.rs:712）：

  ```rust
  /// Wire format version. v2 (M4 §3): no per-bitmap crc32. The version byte
  /// is the only migration gate: != 2 (including M3's v1) silently falls
  /// back to postings.
  pub const BITMAP_VERSION: u8 = 2;
  ```

  ③ `max_bitmap_len`（roaring.rs:718-726）：

  ```rust
  /// Upper bound of the bitmap region length (header+payload, no crc since
  /// v2) for a segment with `max_doc` docs. Derivation (spec §3 len 有界校验;
  /// 关键设计事实 1): runOptimize 后每 container data ≤ 8192B、头部开销 ≤ 9B，
  /// container 数 ≤ ceil(maxDoc/65536)，公共头 ≤ 15B + numContainers vInt ≤ 5B：
  ///   max_bitmap_len = 20 + ceil(max_doc / 65536) * 8201
  pub fn max_bitmap_len(max_doc: u32) -> u64 {
      20 + (max_doc as u64).div_ceil(65536) * 8201
  }
  ```

  ④ `serialize`（roaring.rs:729-773）：文档注释与尾部改为：

  ```rust
      /// header+payload (without the trailing len, which the writer appends;
      /// without the v1 crc32, M4 §3). `df` is the term's docFreq and must
      /// equal the cardinality (docs-only bitmap).
      pub fn serialize(&self, df: u32) -> Vec<u8> {
  ```

  函数体尾部四行：

  ```rust
          let mut bytes = out.into_bytes();
          let crc = crc32fast::hash(&bytes);
          bytes.extend_from_slice(&crc.to_le_bytes());
          bytes
  ```

  改为：

  ```rust
          out.into_bytes()
  ```

  ⑤ `deserialize`（roaring.rs:775-901）：文档注释与头部改为：

  ```rust
      /// Parses + validates a bitmap region (the `len` bytes preceding
      /// docStartFP-4). Returns None on ANY deviation — magic/version
      /// mismatch (v1 included: the version gate is the whole migration
      /// story, M4 §3), df != expected_df, cardinality != df, structural
      /// violation, or trailing bytes — the read side's silent-fallback
      /// signal. v2 drops the crc32 check (triple gate: len bound →
      /// magic/version → header df/card; 关键设计事实 3).
      pub fn deserialize(bytes: &[u8], expected_df: u32) -> Option<RoaringBitmap> {
          if bytes.len() < 12 {
              return None;
          }
          let mut input = IndexInput::in_memory(bytes.to_vec());
  ```

  （删除原 `let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);` 至 crc 校验的 6 行；`12` = magic 4 + version 1 + df/card/numContainers 各 ≥1 + 首个 container 头 ≥4 的最小合法长度；后续主体逐行不变，仅结尾的 `input.file_pointer() != body.len() as u64` 改为 `!= bytes.len() as u64`。）

- [ ] **Step 1.4: postings_read.rs 注释同步** — `read_term_bitmap`（postings_read.rs:169-174）文档注释改为：

  ```rust
      /// Reads + fully validates the inline roaring bitmap preceding this
      /// term's postings (M4 §3 格式 v2): len bound → magic/version (v1
      /// rejected at the gate) → header df == termState.doc_freq (+
      /// cardinality == df) → structural invariants. ANY failure yields
      /// `Ok(None)`, the caller's silent-fallback signal (查询永不报错).
      /// Uses its own positioned slice of the .doc stream (fresh_input
      /// pattern, postings_read.rs:135). The query path switches to the
      /// zero-copy view in T2; this stays until T4, which deletes it
      /// together with the header path (T4 删除集合，关键设计事实 16).
  ```

  （`read_term_bitmap_header` 本任务不动——version 检查走 `BITMAP_VERSION` 常量自动变为只接受 v2；T4 删除。）

- [ ] **Step 1.5: codec 测试全绿**

  ```
  $ cargo test -p codec-lucene9 2>&1 | tail -2
  test result: ok. 158 passed; 0 failed; 1 ignored; ...
  ```

  （156 → 158：新增 `v2_serialize_has_no_crc_and_rejects_v1`、`read_term_bitmap_falls_back_on_v1_layout`；三个改造测试数量不变。）

- [ ] **Step 1.6: core 测试全绿 + fmt**

  ```
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. 44 passed; 0 failed; ...
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  ```

  （43 → 44：新增 `bitmap_v1_index_falls_back_to_postings`。）

- [ ] **Step 1.7: 提交**

  ```
  git add crates/codec-lucene9/src/roaring.rs crates/codec-lucene9/src/postings_read.rs crates/core/src/search/mod.rs
  git commit -m "feat: bitmap format v2 — drop per-bitmap crc32, version-gated v1 fallback"
  ```

---
## Task 2: 零拷贝 RoaringView（`crates/codec-lucene9/src/roaring/view.rs` 新建）

**Files:**
- Create: `crates/codec-lucene9/src/roaring/view.rs`（实现 + `#[cfg(test)]` 测试）
- Modify: `crates/codec-lucene9/src/roaring.rs`（`pub mod view;` + re-export，两行）
- Modify: `crates/codec-lucene9/src/postings_read.rs`（`open_term_bitmap` / `probe_term_bitmap` + 新测试；`read_term_bitmap`/`read_term_bitmap_header` 本任务保留）
- Modify: `crates/core/src/search/segment_reader.rs`（两个同名包装；`read_term_bitmap*` 包装本任务保留）

**Interfaces:**
- Consumes: `crate::io::{DataInput, IndexInput}`（io.rs:778/644）；`super::{BITMAP_MAGIC, BITMAP_VERSION, BITSET_BITS, BITSET_WORDS, TYPE_ARRAY, TYPE_BITSET, TYPE_RUN}`（roaring.rs 私有常量，子模块经 `super::` 访问）；`TermEntry`；T1 的 v2 格式（版本门 `BITMAP_VERSION == 2`、len 上界由 `locate_bitmap_region` 保证）。
- Produces（T3/T4 依赖这些名字，不得改名）:
  ```rust
  // roaring/view.rs
  pub enum RoaringView { Probe(ProbeView), Full(FullView) }
  impl RoaringView {
      pub fn open_probe(input: IndexInput, start: u64, len: u32, expected_df: u32) -> io::Result<Option<RoaringView>>;
      pub fn open_full(input: IndexInput, start: u64, len: u32, expected_df: u32) -> io::Result<Option<RoaringView>>;
      pub fn cardinality(&self) -> u64;
      pub fn contains(&mut self, doc: u32) -> io::Result<bool>;   // probe: 按需 IO；full: 纯内存
      pub fn cursor(&self) -> ViewCursor;                          // full-only 契约（debug_assert）
      pub fn cursor_next(&self, cur: &mut ViewCursor) -> Option<u32>;
      pub fn cursor_advance(&self, cur: &mut ViewCursor, target: u32) -> Option<u32>;
  }
  #[derive(Clone, Copy, Default)]
  pub struct ViewCursor { ci: u32, a: u32, b: u32 }   // 状态语义同 RoaringCursor（事实 8）
  // roaring.rs
  pub mod view;
  pub use view::{RoaringView, ViewCursor};
  // postings_read.rs
  pub fn open_term_bitmap(&self, entry: &TermEntry, max_doc: u32) -> io::Result<Option<RoaringView>>;  // full 模式
  pub fn probe_term_bitmap(&self, entry: &TermEntry, max_doc: u32) -> io::Result<Option<RoaringView>>; // probe 模式
  // segment_reader.rs（bitmap_enabled() 门内联）
  pub(crate) fn open_term_bitmap(&self, entry: &TermEntry) -> io::Result<Option<RoaringView>>;
  pub(crate) fn probe_term_bitmap(&self, entry: &TermEntry) -> io::Result<Option<RoaringView>>;
  ```

  语义决定：(a) 两种打开共用 `scan_directory`：magic/version/df/card/numContainers → 逐容器 key 严格升序、type 合法、card ≥ 1 且 ≤ 65536（65536-value 域上界）、数据段 `[data_off, data_off+data_len)` 落在 region 内、card 总和 == 头内 card、扫描终点 == region 终点——全部廉价，无逐元素校验（spec §4 "单次 read，无逐元素校验"）。(b) probe 模式 `contains` 的读取量：array ≤ 8190B（该桶数据段一次读 + 二分）、bitset **8B**（`data_off + (low>>6)×8` 处单个 u64）、run ≤ 4×numRuns B；高 df bitmap 的其余字节从不被读（用户指令②）。(c) 游标只服务 full 模式；probe 模式调用 `cursor*` 返回 None（契约：永不调用，`cursor()` debug_assert 钉死）。(d) `contains(&mut self)` 统一签名（probe 需要 &mut 做 seek/读；full 纯内存也接受 &mut，方便 roaring_exec 统一循环）。(e) 标量实现，零 unsafe、零新依赖。

### Steps

- [ ] **Step 2.1: 写失败测试** — 新建 `crates/codec-lucene9/src/roaring/view.rs`，先只放模块文档注释 + 测试（`RoaringView` 尚不存在，编译失败即失败测试成立）。同时在 `crates/codec-lucene9/src/roaring.rs` 的 `mod simd;` 之后插入 `pub mod view;`，在文件末尾之前任意 pub 区插入 `pub use view::{RoaringView, ViewCursor};`（紧跟 `mod simd;` 两行后即可）。`view.rs` 初始完整内容：

  ```rust
  //! Zero-copy read view over a v2 inline term bitmap (M4 spec §4): the
  //! query path never rebuilds container objects. Two open modes —
  //!
  //! - **probe** (`open_probe`, AND high-df side): scans only the container
  //!   directory (key/type/card headers; data sections are seeked over,
  //!   their length derived from type+card/numRuns). `contains(doc)` reads
  //!   at most the one bucket that could hold the doc (bitset: a single u64
  //!   word at `data_off + (low>>6)*8`).
  //! - **full** (`open_full`, OR / Term iteration / AND small side): one
  //!   sequential region read into a buffer, then byte-cursor iteration
  //!   with the M3 `RoaringCursor` contract.
  //!
  //! Validation is the v2 triple gate (spec §3): len bound (done by the
  //! caller, `locate_bitmap_region`) → magic+version → header df/card ==
  //! doc_freq, plus cheap structural bounds (container keys ascending, data
  //! sections inside the region, card sum == df). Payload integrity stays
  //! with the .doc footer CRC. Any failure → `Ok(None)`, the silent
  //! postings fallback. All safe code — no unsafe is added for this module.

  #[cfg(test)]
  mod tests {
      use super::super::tests::serialize_v1_for_test;
      use super::*;
      use crate::roaring::RoaringBitmap;

      /// Region bytes for a doc set (v2 image, no len suffix).
      fn region(docs: &[u32]) -> Vec<u8> {
          RoaringBitmap::from_sorted_docs(docs).serialize(docs.len() as u32)
      }

      fn open_full(bytes: &[u8], df: u32) -> Option<RoaringView> {
          RoaringView::open_full(
              IndexInput::in_memory(bytes.to_vec()),
              0,
              bytes.len() as u32,
              df,
          )
          .unwrap()
      }

      fn open_probe(bytes: &[u8], df: u32) -> Option<RoaringView> {
          RoaringView::open_probe(
              IndexInput::in_memory(bytes.to_vec()),
              0,
              bytes.len() as u32,
              df,
          )
          .unwrap()
      }

      /// One fixture per container type + a mixed multi-bucket one
      /// (array: 100 values step 3; bitset: 6000 values step 10 — scattered,
      /// 4*runs > 8192 so runOptimize keeps the bitset; run: 5000
      /// consecutive).
      fn fixtures() -> Vec<Vec<u32>> {
          vec![
              (0..100u32).map(|i| i * 3).collect(),
              (0..6000u32).map(|i| i * 10).collect(),
              (0..5000u32).collect(),
              {
                  let mut v: Vec<u32> = (0..100u32).map(|i| i * 3).collect();
                  v.extend(65_536..70_536u32); // bucket 1: run
                  v.extend((0..6000u32).map(|i| 2 * 65536 + i * 10)); // bucket 2: bitset
                  v
              },
          ]
      }

      #[test]
      fn full_view_cursor_matches_docs() {
          for docs in fixtures() {
              let bytes = region(&docs);
              let v = open_full(&bytes, docs.len() as u32).expect("must open");
              assert_eq!(v.cardinality(), docs.len() as u64);
              let mut cur = v.cursor();
              let mut out = Vec::new();
              while let Some(d) = v.cursor_next(&mut cur) {
                  out.push(d);
              }
              assert_eq!(out, docs);
          }
      }

      #[test]
      fn full_view_advance_matches_linear_scan() {
          for docs in fixtures() {
              let bytes = region(&docs);
              let v = open_full(&bytes, docs.len() as u32).unwrap();
              // fresh-cursor advance per sampled target
              for t in (0..=*docs.last().unwrap() + 1).step_by(61) {
                  let want = docs.iter().find(|&&d| d >= t).copied();
                  let mut cur = v.cursor();
                  assert_eq!(v.cursor_advance(&mut cur, t), want, "target {t}");
              }
              // interleaved next/advance on one cursor (forward-only:
              // targets are the remaining docs themselves, always > last)
              let mut cur = v.cursor();
              let mut idx = 0usize;
              while idx < docs.len() {
                  assert_eq!(v.cursor_next(&mut cur), Some(docs[idx]));
                  idx += 1;
                  if idx < docs.len() {
                      assert_eq!(v.cursor_advance(&mut cur, docs[idx]), Some(docs[idx]));
                      idx += 1;
                  }
              }
              assert_eq!(v.cursor_next(&mut cur), None);
              // advance past the end -> None, sticky
              let mut cur = v.cursor();
              assert_eq!(v.cursor_advance(&mut cur, u32::MAX), None);
              assert_eq!(v.cursor_next(&mut cur), None);
          }
      }

      #[test]
      fn probe_contains_matches_full_contains() {
          for docs in fixtures() {
              let bytes = region(&docs);
              let df = docs.len() as u32;
              let mut probe = open_probe(&bytes, df).expect("probe opens");
              let mut full = open_full(&bytes, df).expect("full opens");
              let space = *docs.last().unwrap() + 2;
              let samples = (0..space)
                  .step_by(97)
                  .chain(docs.iter().copied())
                  .chain(docs.iter().map(|d| d + 1));
              for d in samples {
                  let want = docs.binary_search(&d).is_ok();
                  assert_eq!(probe.contains(d).unwrap(), want, "probe {d}");
                  assert_eq!(full.contains(d).unwrap(), want, "full {d}");
              }
          }
      }

      #[test]
      fn open_rejects_v1_wrong_df_garbage_and_truncation() {
          let docs: Vec<u32> = (0..5000).collect();
          let bytes = region(&docs);
          // v1 layout (valid v1 crc) -> rejected at the version gate
          let v1 = serialize_v1_for_test(&RoaringBitmap::from_sorted_docs(&docs), 5000);
          assert!(open_full(&v1, 5000).is_none());
          assert!(open_probe(&v1, 5000).is_none());
          // wrong expected df -> None
          assert!(open_full(&bytes, 5001).is_none());
          assert!(open_probe(&bytes, 5001).is_none());
          // garbage -> None
          assert!(open_full(&[0xAA; 64], 5000).is_none());
          assert!(open_probe(&[0xAA; 64], 5000).is_none());
          // truncated (data section escapes the region) -> None
          let cut = &bytes[..bytes.len() - 1];
          assert!(open_full(cut, 5000).is_none());
          assert!(open_probe(cut, 5000).is_none());
          // probe of an absent bucket key -> false (no panic, no read)
          let mut p = open_probe(&bytes, 5000).unwrap();
          assert!(!p.contains(9 << 16).unwrap());
      }
  }
  ```

- [ ] **Step 2.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 view 2>&1 | tail -5
  error[E0433]: failed to resolve: use of unresolved module or unlinked crate `view`
  （或类似 unresolved 错误：RoaringView 尚不存在）
  ```

- [ ] **Step 2.3: 视图实现** — `crates/codec-lucene9/src/roaring/view.rs` 在模块文档注释之后、`#[cfg(test)]` 之前插入（完整实现）：

  ```rust
  use std::io;

  use crate::io::{DataInput, IndexInput};

  use super::{
      BITMAP_MAGIC, BITMAP_VERSION, BITSET_BITS, BITSET_WORDS, TYPE_ARRAY, TYPE_BITSET, TYPE_RUN,
  };

  /// One container-directory entry. `data_off`/`data_len` are relative to
  /// the region start (the bitmap's first byte), so they index both the
  /// probe stream (region_start + off) and the full-mode buffer.
  #[derive(Clone, Copy, Debug)]
  struct ContainerMeta {
      key: u16,
      ty: u8,
      card: u32,
      data_off: u64,
      data_len: u64,
  }

  /// Parsed + validated bitmap view (M4 §4). Owns either the positioned
  /// .doc stream (probe) or the region bytes (full). Iterators must box it —
  /// an IndexInput carries an 8KB inline buffer (io.rs:597, 关键设计事实 6).
  pub enum RoaringView {
      Probe(ProbeView),
      Full(FullView),
  }

  pub struct ProbeView {
      input: IndexInput, // independently positioned .doc stream (fresh_input)
      region_start: u64,
      dir: Vec<ContainerMeta>,
      card: u64,
      scratch: Vec<u8>, // per-probe bucket data (array/run), reused
  }

  pub struct FullView {
      buf: Vec<u8>, // the whole region, one sequential read
      dir: Vec<ContainerMeta>,
      card: u64,
  }

  /// LE u16 at index `i` of a byte image (array element i / run-pair half i).
  fn u16_at(data: &[u8], i: usize) -> u16 {
      u16::from_le_bytes([data[2 * i], data[2 * i + 1]])
  }

  /// partition_point over an ascending LE u16 image: first index whose
  /// value >= `low`.
  fn partition_point_u16(data: &[u8], n: usize, low: u16) -> usize {
      let mut lo = 0usize;
      let mut hi = n;
      while lo < hi {
          let mid = (lo + hi) / 2;
          if u16_at(data, mid) < low {
              lo = mid + 1;
          } else {
              hi = mid;
          }
      }
      lo
  }

  /// First run index whose end >= `low` (runs are ascending, non-overlapping).
  fn partition_point_run_end(data: &[u8], runs: usize, low: u16) -> usize {
      let mut lo = 0usize;
      let mut hi = runs;
      while lo < hi {
          let mid = (lo + hi) / 2;
          if u16_at(data, 2 * mid + 1) < low {
              lo = mid + 1;
          } else {
              hi = mid;
          }
      }
      lo
  }

  /// First set bit at position >= `from` in a 65536-bit LE byte image
  /// (mirrors `next_set_bit` over `[u64; BITSET_WORDS]`, roaring.rs:104).
  fn next_set_bit_in(data: &[u8], from: u32) -> Option<u32> {
      if from >= BITSET_BITS as u32 {
          return None;
      }
      let mut wi = from as usize >> 6;
      let mut word =
          u64::from_le_bytes(data[wi * 8..wi * 8 + 8].try_into().unwrap()) & (u64::MAX << (from & 63));
      loop {
          if word != 0 {
              return Some((wi * 64 + word.trailing_zeros() as usize) as u32);
          }
          wi += 1;
          if wi == BITSET_WORDS {
              return None;
          }
          word = u64::from_le_bytes(data[wi * 8..wi * 8 + 8].try_into().unwrap());
      }
  }

  /// Membership test inside one container's data image (array: card×u16 LE;
  /// bitset: 1024×u64 LE; run: numRuns×(start,end) u16 LE pairs).
  fn contains_in(data: &[u8], ty: u8, card: u32, low: u16) -> bool {
      match ty {
          TYPE_ARRAY => {
              let n = card as usize;
              let p = partition_point_u16(data, n, low);
              p < n && u16_at(data, p) == low
          }
          TYPE_BITSET => {
              let w = (low >> 6) as usize * 8;
              let word = u64::from_le_bytes(data[w..w + 8].try_into().unwrap());
              word >> (low & 63) & 1 == 1
          }
          TYPE_RUN => {
              let runs = data.len() / 4;
              let p = partition_point_run_end(data, runs, low);
              p < runs && u16_at(data, 2 * p) <= low
          }
          _ => false, // unreachable: the directory scan rejected unknown types
      }
  }

  impl RoaringView {
      /// Shared container-directory scan (M4 §4): validates the v2 header
      /// (magic, version == 2 — the only migration gate, df == expected_df,
      /// card == df) and walks the per-container headers, deriving each
      /// data section's [data_off, data_off+data_len) WITHOUT reading it
      /// (array: 2·card B; bitset: 8192 B; run: the numRuns vInt is part of
      /// the header walk, then 4·numRuns B — 关键设计事实 7). None on any
      /// deviation (the silent-fallback signal).
      fn scan_directory(
          input: &mut IndexInput,
          start: u64,
          len: u32,
          expected_df: u32,
      ) -> io::Result<Option<(u64, Vec<ContainerMeta>)>> {
          input.seek(start)?;
          let mut magic = [0u8; 4];
          input.read_bytes(&mut magic)?;
          if magic != BITMAP_MAGIC {
              return Ok(None);
          }
          if input.read_byte()? != BITMAP_VERSION {
              return Ok(None); // v1 (or anything else): version is the migration gate
          }
          if input.read_vint()? as u32 != expected_df {
              return Ok(None);
          }
          let card = input.read_vint()? as u32;
          if card != expected_df {
              return Ok(None); // docs-only bitmap: cardinality == df
          }
          let num_containers = input.read_vint()?;
          if !(1..=65536).contains(&num_containers) {
              return Ok(None);
          }
          let mut dir = Vec::with_capacity(num_containers as usize);
          let mut card_sum = 0u64;
          let mut last_key: Option<u16> = None;
          for _ in 0..num_containers {
              let key = input.read_short()? as u16;
              if let Some(lk) = last_key
                  && key <= lk
              {
                  return Ok(None); // unsorted / duplicate container keys
              }
              last_key = Some(key);
              let ty = input.read_byte()?;
              let c = input.read_vint()?;
              if c < 1 || c > 65536 {
                  return Ok(None); // empty container, or beyond the 2^16 domain
              }
              let card = c as u32;
              let data_len = match ty {
                  TYPE_ARRAY => 2 * card as u64,
                  TYPE_BITSET => 8192,
                  TYPE_RUN => {
                      let num_runs = input.read_vint()?;
                      // at most 2^15 disjoint runs over a 2^16-value domain
                      if !(1..=32768).contains(&num_runs) {
                          return Ok(None);
                      }
                      4 * num_runs as u64
                  }
                  _ => return Ok(None),
              };
              let data_off = input.file_pointer() - start;
              if data_off + data_len > len as u64 {
                  return Ok(None); // data section escapes the region: not ours
              }
              card_sum += card as u64;
              dir.push(ContainerMeta {
                  key,
                  ty,
                  card,
                  data_off,
                  data_len,
              });
              input.seek(start + data_off + data_len)?; // skip the data section
          }
          if card_sum != card as u64 {
              return Ok(None);
          }
          if input.file_pointer() != start + len as u64 {
              return Ok(None); // trailing bytes: not one of our bitmaps
          }
          Ok(Some((card_sum, dir)))
      }

      /// Probe-mode open (M4 §4, AND high-df side): container-directory
      /// scan only — data sections are never read until `contains` needs
      /// one bucket. `input` is an independently positioned .doc stream
      /// (fresh_input); [start, start+len) is the located bitmap region.
      pub fn open_probe(
          mut input: IndexInput,
          start: u64,
          len: u32,
          expected_df: u32,
      ) -> io::Result<Option<RoaringView>> {
          let Some((card, dir)) = Self::scan_directory(&mut input, start, len, expected_df)? else {
              return Ok(None);
          };
          Ok(Some(RoaringView::Probe(ProbeView {
              input,
              region_start: start,
              dir,
              card,
              scratch: Vec::new(),
          })))
      }

      /// Full-mode open (M4 §4, OR / Term iteration / AND small side): the
      /// validated region is read once, sequentially, into memory — no
      /// per-element validation.
      pub fn open_full(
          mut input: IndexInput,
          start: u64,
          len: u32,
          expected_df: u32,
      ) -> io::Result<Option<RoaringView>> {
          let Some((card, dir)) = Self::scan_directory(&mut input, start, len, expected_df)? else {
              return Ok(None);
          };
          input.seek(start)?;
          let mut buf = vec![0u8; len as usize];
          input.read_bytes(&mut buf)?;
          Ok(Some(RoaringView::Full(FullView { buf, dir, card })))
      }

      /// Total docs (== df of the term, pinned by the v2 header checks).
      pub fn cardinality(&self) -> u64 {
          match self {
              RoaringView::Probe(p) => p.card,
              RoaringView::Full(f) => f.card,
          }
      }

      /// Membership test. Probe mode reads at most the target bucket's data
      /// (bitset: one 8B word at data_off + (low>>6)×8, spec §4); full mode
      /// is a pure in-memory test.
      pub fn contains(&mut self, doc: u32) -> io::Result<bool> {
          let (key, low) = ((doc >> 16) as u16, doc as u16);
          match self {
              RoaringView::Probe(p) => {
                  let ci = p.dir.partition_point(|m| m.key < key);
                  let Some(m) = p.dir.get(ci).copied() else {
                      return Ok(false);
                  };
                  if m.key != key {
                      return Ok(false);
                  }
                  if m.ty == TYPE_BITSET {
                      p.input
                          .seek(p.region_start + m.data_off + (low >> 6) as u64 * 8)?;
                      let word = p.input.read_long()? as u64;
                      return Ok(word >> (low & 63) & 1 == 1);
                  }
                  p.scratch.clear();
                  p.scratch.resize(m.data_len as usize, 0);
                  p.input.seek(p.region_start + m.data_off)?;
                  p.input.read_bytes(&mut p.scratch)?;
                  Ok(contains_in(&p.scratch, m.ty, m.card, low))
              }
              RoaringView::Full(f) => {
                  let ci = f.dir.partition_point(|m| m.key < key);
                  let Some(m) = f.dir.get(ci) else {
                      return Ok(false);
                  };
                  if m.key != key {
                      return Ok(false);
                  }
                  let data = &f.buf[m.data_off as usize..(m.data_off + m.data_len) as usize];
                  Ok(contains_in(data, m.ty, m.card, low))
              }
          }
      }
  }

  /// Byte-image iteration cursor with the M3 `RoaringCursor` contract
  /// (roaring.rs:608-617, 关键设计事实 8): plain data, forward-only;
  /// `cursor_advance` targets must exceed the last returned doc (the
  /// DocIter advance contract). State per container type: array → a = next
  /// element index; bitset → a = next bit to check; run → a = run index,
  /// b = in-run offset of the next value.
  #[derive(Clone, Copy, Default)]
  pub struct ViewCursor {
      ci: u32,
      a: u32,
      b: u32,
  }

  impl RoaringView {
      /// Fresh cursor. Full-mode views only (probe views have no byte image
      /// in memory); the mode is debug-asserted.
      pub fn cursor(&self) -> ViewCursor {
          debug_assert!(
              matches!(self, RoaringView::Full(_)),
              "cursor() requires open_full"
          );
          ViewCursor::default()
      }

      /// Next doc at/after the cursor position, or None when exhausted.
      /// Full-mode only: probe views return None (contract: never called).
      pub fn cursor_next(&self, cur: &mut ViewCursor) -> Option<u32> {
          let RoaringView::Full(f) = self else {
              return None;
          };
          loop {
              let m = f.dir.get(cur.ci as usize)?;
              let data = &f.buf[m.data_off as usize..(m.data_off + m.data_len) as usize];
              match m.ty {
                  TYPE_ARRAY => {
                      let i = cur.a as usize;
                      if i < m.card as usize {
                          cur.a += 1;
                          return Some(((m.key as u32) << 16) | u16_at(data, i) as u32);
                      }
                  }
                  TYPE_BITSET => {
                      if let Some(bit) = next_set_bit_in(data, cur.a) {
                          cur.a = bit + 1;
                          return Some(((m.key as u32) << 16) | bit);
                      }
                  }
                  TYPE_RUN => {
                      let runs = m.data_len as usize / 4;
                      let r = cur.a as usize;
                      if r < runs {
                          let (s, e) = (u16_at(data, 2 * r), u16_at(data, 2 * r + 1));
                          let v = s as u32 + cur.b;
                          if v < e as u32 {
                              cur.b += 1;
                          } else {
                              cur.a += 1;
                              cur.b = 0;
                          }
                          return Some(((m.key as u32) << 16) | v);
                      }
                  }
                  _ => return None, // unreachable: scan rejected unknown types
              }
              cur.ci += 1;
              cur.a = 0;
              cur.b = 0;
          }
      }

      /// First doc >= target; the cursor ends positioned past it. Forward
      /// only (same contract as `RoaringBitmap::cursor_advance`).
      pub fn cursor_advance(&self, cur: &mut ViewCursor, target: u32) -> Option<u32> {
          let RoaringView::Full(f) = self else {
              return None;
          };
          let key = (target >> 16) as u16;
          let low = target as u16;
          let ci = f.dir.partition_point(|m| m.key < key);
          if ci > cur.ci as usize {
              cur.ci = ci as u32;
              cur.a = 0;
              cur.b = 0;
          }
          let m = f.dir.get(cur.ci as usize)?;
          if m.key > key {
              // target's bucket absent: first value of the current container
              cur.a = 0;
              cur.b = 0;
              return self.cursor_next(cur);
          }
          let data = &f.buf[m.data_off as usize..(m.data_off + m.data_len) as usize];
          match m.ty {
              TYPE_ARRAY => {
                  let p = partition_point_u16(data, m.card as usize, low);
                  cur.a = cur.a.max(p as u32);
              }
              TYPE_BITSET => {
                  cur.a = cur.a.max(low as u32);
              }
              TYPE_RUN => {
                  let runs = m.data_len as usize / 4;
                  let p = partition_point_run_end(data, runs, low);
                  if p as u32 > cur.a {
                      // target lands in a later run: the partially consumed
                      // run's in-run offset must not leak into the new run
                      cur.a = p as u32;
                      cur.b = 0;
                  }
                  if (cur.a as usize) < runs {
                      let s = u16_at(data, 2 * cur.a as usize);
                      cur.b = cur.b.max(low.saturating_sub(s) as u32);
                  }
              }
              _ => return None, // unreachable
          }
          self.cursor_next(cur)
      }
  }
  ```

- [ ] **Step 2.4: view 测试全绿**

  ```
  $ cargo test -p codec-lucene9 view 2>&1 | tail -6
  test result: ok. 4 passed; 0 failed; ...
  ```

- [ ] **Step 2.5: postings_read.rs 两个打开函数 + 测试** — 在 `read_term_bitmap_header` 之后插入（`use crate::roaring::RoaringView;` 并入既有 roaring import）：

  ```rust
      /// Zero-copy full-mode view over the term's inline bitmap (M4 §4):
      /// v2 triple validation + one sequential region read; None →
      /// postings fallback. The query path's Term/OR/AND-small-side entry
      /// point (replaces the M3 deserialize-rebuild `read_term_bitmap`,
      /// which stays until T4 for the write-side round-trip tests).
      pub fn open_term_bitmap(
          &self,
          entry: &TermEntry,
          max_doc: u32,
      ) -> io::Result<Option<RoaringView>> {
          let mut input = self.fresh_input()?;
          let Some((start, len)) = Self::locate_bitmap_region(&mut input, entry, max_doc)? else {
              return Ok(None);
          };
          RoaringView::open_full(input, start, len, entry.doc_freq)
      }

      /// Zero-copy probe-mode view (M4 §4, AND 大侧专用): container-
      /// directory header scan only — data sections are read per-probe by
      /// `RoaringView::contains`, never wholesale (用户指令②).
      pub fn probe_term_bitmap(
          &self,
          entry: &TermEntry,
          max_doc: u32,
      ) -> io::Result<Option<RoaringView>> {
          let mut input = self.fresh_input()?;
          let Some((start, len)) = Self::locate_bitmap_region(&mut input, entry, max_doc)? else {
              return Ok(None);
          };
          RoaringView::open_probe(input, start, len, entry.doc_freq)
      }
  ```

  测试模块新增：

  ```rust
      /// M4 §4 两种打开模式：full 视图游标迭代 == postings；probe contains
      /// 与之一致；未命中 / df 不符 / 无 bitmap 索引全部 None。
      #[test]
      fn open_and_probe_term_bitmap_match_postings() {
          let root = temp_dir("bitmap-view");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_segment_bitmap(&dir);
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
          let e = seek(&dir, &fis, "tx", b"hot"); // docs 0..5000 → single run container

          // full mode: cardinality + byte-cursor iteration == postings
          let v = postings
              .open_term_bitmap(&e, 6000)
              .unwrap()
              .expect("hot has a full view");
          assert_eq!(v.cardinality(), 5000);
          let mut cur = v.cursor();
          for expected in 0..5000u32 {
              assert_eq!(v.cursor_next(&mut cur), Some(expected));
          }
          assert_eq!(v.cursor_next(&mut cur), None);

          // probe mode: contains agrees on both sides of the boundary
          let mut p = postings
              .probe_term_bitmap(&e, 6000)
              .unwrap()
              .expect("hot has a probe view");
          for d in [0u32, 1, 42, 4999] {
              assert!(p.contains(d).unwrap(), "probe {d}");
          }
          for d in [5000u32, 5001, 6000, 9 << 16] {
              assert!(!p.contains(d).unwrap(), "probe miss {d}");
          }

          // below the df read gate: None (and no read attempted)
          let e = seek(&dir, &fis, "tx", b"warm");
          assert!(postings.open_term_bitmap(&e, 6000).unwrap().is_none());
          assert!(postings.probe_term_bitmap(&e, 6000).unwrap().is_none());

          // df mismatch (>= gate, so the header check fires): None
          let e = seek(&dir, &fis, "tx", b"hot");
          let mut bad = e;
          bad.doc_freq = 4096; // real df is 5000; 4096 >= BITMAP_MIN_DF
          assert!(postings.open_term_bitmap(&bad, 6000).unwrap().is_none());
          assert!(postings.probe_term_bitmap(&bad, 6000).unwrap().is_none());

          // index written without bitmaps: natural None
          let root2 = temp_dir("bitmap-view-off");
          let dir2 = FSDirectory::open(&root2).unwrap();
          let (fis2, _, _) = write_segment(&dir2);
          let postings2 = PostingsReader::open(&dir2, "_0", &[4u8; 16]).unwrap();
          let e2 = seek(&dir2, &fis2, "tx", b"hot");
          assert!(postings2.open_term_bitmap(&e2, 6000).unwrap().is_none());
          assert!(postings2.probe_term_bitmap(&e2, 6000).unwrap().is_none());

          fs::remove_dir_all(&root).unwrap();
          fs::remove_dir_all(&root2).unwrap();
      }
  ```

- [ ] **Step 2.6: segment_reader.rs 两个包装** — 在 `read_term_bitmap_header` 包装之后插入（`RoaringView` 并入既有 `codec_lucene9::roaring` import）：

  ```rust
      /// Zero-copy full-mode bitmap view (M4 §4) for Term iteration / OR /
      /// the AND small side. None → postings fallback.
      pub(crate) fn open_term_bitmap(&self, entry: &TermEntry) -> io::Result<Option<RoaringView>> {
          if !bitmap_enabled() {
              return Ok(None);
          }
          self.postings.open_term_bitmap(entry, self.max_doc as u32)
      }

      /// Probe-mode bitmap view (M4 §4, AND 大侧定点测位): container
      /// directory scan only, data sections read per-probe.
      pub(crate) fn probe_term_bitmap(&self, entry: &TermEntry) -> io::Result<Option<RoaringView>> {
          if !bitmap_enabled() {
              return Ok(None);
          }
          self.postings.probe_term_bitmap(entry, self.max_doc as u32)
      }
  ```

- [ ] **Step 2.7: 两 crate 全绿 + fmt**

  ```
  $ cargo test -p codec-lucene9 2>&1 | tail -2
  test result: ok. 163 passed; 0 failed; 1 ignored; ...
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. 44 passed; 0 failed; ...
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  ```

  （158 → 163：view.rs 4 个 + postings_read 1 个；core 不变——新包装暂无人调用，T3/T4 接入。）

- [ ] **Step 2.8: 提交**

  ```
  git add crates/codec-lucene9/src/roaring/view.rs crates/codec-lucene9/src/roaring.rs crates/codec-lucene9/src/postings_read.rs crates/core/src/search/segment_reader.rs
  git commit -m "feat: zero-copy RoaringView — probe/full open modes, contains, byte cursor"
  ```

---
## Task 3: AND df 偏斜 probe（roaring_exec 档 2 probe 过滤 + 档 1 SKEW_RATIO 策略）

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（`DocSource`、`RoaringAndDocIter`、`SegmentDocIter::RoaringAnd`）
- Modify: `crates/core/src/search/roaring_exec.rs`（`SKEW_RATIO`、`probe_clauses`、`materialize_docs`、`and_iterator`；`segment_iterator`/`count` AND 分支切换；OR 分支本任务保持 M3 容器折叠引擎）
- Test: `crates/core/src/search/mod.rs`（改造 `bool_query_three_tier_roaring` 的 AND 路径断言 + 新增 skew 语料测试）

**Interfaces:**
- Consumes: T2 的 `SegmentReader::{open_term_bitmap, probe_term_bitmap}`、`RoaringView::{cursor, cursor_next, cursor_advance, contains}`、`ViewCursor`；既有 `multi_term::for_each_doc`（multi_term.rs:276-302）、`collect_bool_entries`（roaring_exec.rs:22-47，零改动）、`RoaringBitmap` 折叠引擎（OR 分支暂留）。
- Produces（T4/T5 依赖这些名字，不得改名）:
  ```rust
  // doc_iter.rs
  pub enum DocSource {                                  // merge-intersect / merge-union 的统一升序源
      View { view: Box<RoaringView>, cur: ViewCursor, doc: Option<u32> },
      Slice { docs: Vec<u32>, pos: usize },
  }
  impl DocSource {
      pub fn view(view: RoaringView) -> DocSource;      // full 模式视图源，构造即 prime 到首个 doc
      pub fn slice(docs: Vec<u32>) -> DocSource;        // 物化低 df 子句（df<4096 有界）
      fn current(&self) -> Option<u32>;
      fn next(&mut self) -> Option<u32>;
      fn advance(&mut self, target: u32) -> Option<u32>; // forward-only；current >= target 时幂等
  }
  pub struct RoaringAndDocIter { sources: Vec<DocSource>, probes: Vec<Box<RoaringView>>, doc: i32 }
  impl RoaringAndDocIter { pub fn new(sources: Vec<DocSource>, probes: Vec<RoaringView>) -> RoaringAndDocIter }
  // SegmentDocIter 新增变体
  RoaringAnd(RoaringAndDocIter),
  // roaring_exec.rs
  pub(crate) const SKEW_RATIO: u64 = 4;                 // bench 标定（T5）
  fn probe_clauses(seg: &SegmentReader, entries: &[(u32, TermEntry)]) -> io::Result<Option<Vec<Option<RoaringView>>>>;
  fn materialize_docs(seg: &SegmentReader, entry: &TermEntry, has_freqs: bool) -> io::Result<Vec<u32>>;
  fn and_iterator(seg: &SegmentReader, entries: &[(u32, TermEntry)], has_freqs: bool) -> io::Result<Option<SegmentDocIter>>;
  pub(crate) fn segment_iterator(seg: &SegmentReader, entries: &[(u32, TermEntry)], has_freqs: bool, is_and: bool) -> io::Result<Option<SegmentDocIter>>; // 签名不变
  pub(crate) fn count(seg: &SegmentReader, entries: &[(u32, TermEntry)], has_freqs: bool, is_and: bool) -> io::Result<Option<u64>>;                        // 签名不变
  ```

  语义决定（spec §5 档判定，按段独立）：(a) **presence 探测用 probe 打开**（每子句一次容器目录扫描，µs 级，零数据段读取）；全部 None → 档 3（返回 None，调用方走既有 PFOR 合取）。(b) **档 2**（部分子句有 bitmap）：无 bitmap 子句经 `for_each_doc` 物化为升序 `Vec<u32>`（df<4096 → ≤4095 docs 有界），候选 doc 逐一对各 bitmap 子句 `contains` probe，任一不含即剔除——**零 bitmap 全量读**。(c) **档 1**（全有 bitmap）：`max_df >= SKEW_RATIO * min_df` → 偏斜：最小子句改开 full 视图迭代、其余保持 probe 视图（高 df 侧不做全量读取）；否则非偏斜：全部改开 full 视图，k 路字节游标 merge-intersect（2 子句为常见情形，k 路自然泛化）。entries 已 df 升序 → sources 构造顺序即 cheapest-first。(d) AND count 驱动同一迭代器计数（事实 10）；OR count/迭代本任务仍走 `fold_clauses` 容器折叠（T4 切换）。(e) `needs_freq == true` 永不进本模块（query.rs 既有门）。

### Steps

- [ ] **Step 3.1: 写失败测试** — `crates/core/src/search/mod.rs` 测试模块两处：

  ① `bool_query_three_tier_roaring`（mod.rs:761-868）的 AND 路径断言改造——档 1/档 2 的 AND 现在产出 `RoaringAnd` 变体（OR 断言本任务不动，仍 `Roaring`）：

  ```rust
          // 档 1：两个子句都有 bitmap（M4：RoaringAnd = 字节游标 + probe）
          let q = Query::and("message", &["hot", "scorching"]);
          let it = q.segment_iterator(seg, false).unwrap().unwrap();
          assert!(
              matches!(it, SegmentDocIter::RoaringAnd(_)),
              "tier-1 AND must be roaring"
          );
  ```

  同理把 `// 档 2：hot 有 bitmap、warm3 无（df≈714）→ 物化后统一 roaring` 段的 AND 断言改为 `matches!(it, SegmentDocIter::RoaringAnd(_))`（消息改 `"tier-2 mixed AND must be roaring"`）；OR 两处断言与档 3/needs_freq/缺失子句断言原样保留。

  ② 新增 skew 语料与偏斜路径测试：

  ```rust
      /// M4 §5 skew 语料：20000 doc；rare df=4500（doc 0..4500）、common
      /// df=20000（全量）——都 ≥4096 有 bitmap，df 比 4.44 ≥ SKEW_RATIO →
      /// 档 1 偏斜：小侧全量模式迭代 + 大侧 contains probe。
      fn write_skew_corpus(root: &std::path::Path, bitmap: bool) {
          let mut cfg = IndexWriterConfig::default();
          cfg.bitmap = bitmap;
          let mut w = IndexWriter::create(root, schema(), cfg).unwrap();
          for d in 0..20000u32 {
              let msg = if d < 4500 { "common rare" } else { "common" };
              w.add_document(doc("INFO", &format!("tid-{d}"), msg)).unwrap();
          }
          w.commit().unwrap();
          drop(w);
      }

      /// 偏斜 probe（RoaringAnd 路径）与 bitmap-off PFOR 结果逐位一致，
      /// 锚点 count 钉死语义（交集 = rare 全集 4500，并集 = common 全集 20000）。
      #[test]
      fn and_skew_probe_matches_pfor() {
          let root_off = temp_dir("skewoff");
          let root_on = temp_dir("skewon");
          write_skew_corpus(&root_off, false);
          write_skew_corpus(&root_on, true);

          // 路径断言：skew AND 走 RoaringAnd（probe 形态由 T5 bench 标定）
          let dir = FSDirectory::open(&root_on).unwrap();
          let mut reader = Reader::open(&dir).unwrap();
          let (_base, seg) = reader.leaves().next().unwrap();
          let it = Query::and("message", &["rare", "common"])
              .segment_iterator(seg, false)
              .unwrap()
              .unwrap();
          assert!(
              matches!(it, SegmentDocIter::RoaringAnd(_)),
              "skew AND must be roaring"
          );
          drop(reader);

          let mut s_off = Searcher::open(&FSDirectory::open(&root_off).unwrap()).unwrap();
          let mut s_on = Searcher::open(&FSDirectory::open(&root_on).unwrap()).unwrap();
          let battery: Vec<Query> = vec![
              Query::and("message", &["rare", "common"]), // skew probe
              Query::and("message", &["common", "rare"]), // 子句顺序无关
              Query::or("message", &["rare", "common"]),  // OR（本任务仍折叠引擎）
              Query::term("message", "rare"),
          ];
          for q in &battery {
              let (a_total, a_docs) = s_off.top_docs(q, 25000).unwrap();
              let (b_total, b_docs) = s_on.top_docs(q, 25000).unwrap();
              assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
              assert_eq!(s_off.count(q).unwrap(), s_on.count(q).unwrap(), "count {q:?}");
          }
          assert_eq!(
              s_on.count(&Query::and("message", &["rare", "common"])).unwrap(),
              4500
          );
          assert_eq!(
              s_on.count(&Query::or("message", &["rare", "common"])).unwrap(),
              20000
          );
          fs::remove_dir_all(&root_off).unwrap();
          fs::remove_dir_all(&root_on).unwrap();
      }
  ```

- [ ] **Step 3.2: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core 2>&1 | tail -5
  error[E0599]: no variant named `RoaringAnd` found for enum `SegmentDocIter`
  ```

- [ ] **Step 3.3: doc_iter.rs——DocSource + RoaringAndDocIter** — 文件头部 import 行

  ```rust
  use codec_lucene9::roaring::{RoaringBitmap, RoaringCursor};
  ```

  改为：

  ```rust
  use codec_lucene9::roaring::{RoaringBitmap, RoaringCursor, RoaringView, ViewCursor};
  ```

  在 `// ── SegmentDocIter ──` 节之前插入（完整实现）：

  ```rust
  // ── AND over views (M4 §5) ─────────────────────────────────────────────

  /// One merge-intersect / merge-union source (M4 §5): a full-mode bitmap
  /// view byte cursor, or a materialized low-df clause (df<4096, bounded).
  /// Both yield ascending docs with a forward-only advance.
  pub enum DocSource {
      View {
          view: Box<RoaringView>,
          cur: ViewCursor,
          doc: Option<u32>,
      },
      Slice { docs: Vec<u32>, pos: usize },
  }

  impl DocSource {
      /// Full-mode view source, primed to its first doc. The view is boxed —
      /// a probe/full view owns an IndexInput with an 8KB inline buffer
      /// (关键设计事实 6).
      pub fn view(view: RoaringView) -> DocSource {
          let mut cur = view.cursor(); // full-mode contract (open_term_bitmap)
          let doc = view.cursor_next(&mut cur);
          DocSource::View {
              view: Box::new(view),
              cur,
              doc,
          }
      }

      /// Materialized low-df clause source (spec §5 档 2: df<4096 → ≤4095
      /// docs, ascending by enum construction).
      pub fn slice(docs: Vec<u32>) -> DocSource {
          DocSource::Slice { docs, pos: 0 }
      }

      fn current(&self) -> Option<u32> {
          match self {
              DocSource::View { doc, .. } => *doc,
              DocSource::Slice { docs, pos } => docs.get(*pos).copied(),
          }
      }

      fn next(&mut self) -> Option<u32> {
          match self {
              DocSource::View { view, cur, doc } => {
                  *doc = view.cursor_next(cur);
                  *doc
              }
              DocSource::Slice { docs, pos } => {
                  if *pos < docs.len() {
                      *pos += 1;
                  }
                  docs.get(*pos).copied()
              }
          }
      }

      /// First doc >= target. Forward-only; idempotent when the current doc
      /// is already at/past target (the merge dance re-syncs on agreement).
      fn advance(&mut self, target: u32) -> Option<u32> {
          match self {
              DocSource::View { view, cur, doc } => {
                  // guard: cursor_advance requires target > last returned
                  if doc.is_some_and(|d| d >= target) {
                      return *doc;
                  }
                  *doc = view.cursor_advance(cur, target);
                  *doc
              }
              DocSource::Slice { docs, pos } => {
                  *pos += docs[*pos..].partition_point(|&d| d < target);
                  docs.get(*pos).copied()
              }
          }
      }
  }

  /// AND execution over views (M4 §5): merge-intersect over `sources`
  /// (full views + materialized slices); every agreed candidate is
  /// point-probed against each `probes` view (contains). Tier shapes:
  /// 档 1 偏斜 → sources=[最小侧 full], probes=其余；档 1 非偏斜 →
  /// sources=全部 full, probes=[]；档 2 → sources=物化 slices,
  /// probes=bitmap 子句. freq() is 1 (ConstantScore, trait default).
  pub struct RoaringAndDocIter {
      sources: Vec<DocSource>,
      probes: Vec<Box<RoaringView>>,
      doc: i32,
  }

  impl RoaringAndDocIter {
      pub fn new(sources: Vec<DocSource>, probes: Vec<RoaringView>) -> RoaringAndDocIter {
          debug_assert!(!sources.is_empty());
          RoaringAndDocIter {
              sources,
              probes: probes.into_iter().map(Box::new).collect(),
              doc: -1,
          }
      }
  }

  impl DocIter for RoaringAndDocIter {
      fn doc_id(&self) -> i32 {
          self.doc
      }

      fn next_doc(&mut self) -> io::Result<i32> {
          if self.doc == NO_MORE_DOCS {
              return Ok(NO_MORE_DOCS);
          }
          if self.doc >= 0 {
              // move every source sitting on the last emitted doc past it
              let last = self.doc as u32;
              for s in &mut self.sources {
                  if s.current() == Some(last) && s.next().is_none() {
                      self.doc = NO_MORE_DOCS;
                      return Ok(NO_MORE_DOCS);
                  }
              }
          }
          'outer: loop {
              // conjunction: agree on the max current doc
              let mut target = 0u32;
              for s in &self.sources {
                  let Some(d) = s.current() else {
                      self.doc = NO_MORE_DOCS;
                      return Ok(NO_MORE_DOCS);
                  };
                  target = target.max(d);
              }
              let mut agreed = true;
              for s in &mut self.sources {
                  if s.current() < Some(target) {
                      match s.advance(target) {
                          Some(d) if d == target => {}
                          Some(_) => agreed = false, // overshot: new candidate, re-sync
                          None => {
                              self.doc = NO_MORE_DOCS;
                              return Ok(NO_MORE_DOCS);
                          }
                      }
                  }
              }
              if !agreed {
                  continue 'outer;
              }
              // point-probe the agreed candidate against every bitmap clause
              let mut pass = true;
              for p in &mut self.probes {
                  if !p.contains(target)? {
                      pass = false;
                      break;
                  }
              }
              if pass {
                  self.doc = target as i32;
                  return Ok(self.doc);
              }
              // rejected: move the first (cheapest) source past the candidate
              if self.sources[0].next().is_none() {
                  self.doc = NO_MORE_DOCS;
                  return Ok(NO_MORE_DOCS);
              }
          }
      }

      fn advance(&mut self, target: i32) -> io::Result<i32> {
          if self.doc >= target || self.doc == NO_MORE_DOCS {
              return Ok(self.doc);
          }
          for s in &mut self.sources {
              s.advance(target.max(0) as u32);
          }
          self.doc = -1;
          self.next_doc()
      }
  }
  ```

  `SegmentDocIter` 枚举增加变体并在三个 match 中接线（doc_id / next_doc / advance；freq 落 `_ => 1` 臂，无需改）：

  ```rust
  pub enum SegmentDocIter {
      Docs(DocsEnum),
      Freqs(DocsFreqsEnum),
      All(MatchAllIter),
      And(ConjunctionDocIter),
      Or(DisjunctionDocIter),
      Bitset(BitsetDocIter),
      Phrase(PhraseDocIter),
      Roaring(RoaringDocIter),
      RoaringAnd(RoaringAndDocIter),
  }
  ```

  （三个 match 各加一行 `Self::RoaringAnd(a) => a.doc_id(),` / `a.next_doc(),` / `a.advance(t),`，与 `Roaring` 臂并列。）

- [ ] **Step 3.4: roaring_exec.rs——AND 切换** — 模块文档注释首段改为：

  ```rust
  //! Roaring execution for Boolean queries (M3 §5 three-tier rule per
  //! segment, M4 §4/§5 zero-copy): each AND clause's doc source is its
  //! validated inline bitmap — a full-mode byte cursor for the small side /
  //! merge-intersect, a probe-mode contains() for the high-df side — or,
  //! for clauses under the bitmap threshold, a query-time materialized doc
  //! vec (df<4096, bounded). OR still folds container bitmaps here (T4
  //! switches it to byte cursors). When NO clause has a bitmap the callers
  //! fall back to the existing PFOR conjunction/disjunction untouched
  //! (tier 3, M1's tuned path).
  ```

  import 区改为：

  ```rust
  use std::io;

  use codec_lucene9::postings_read::NO_MORE_DOCS;
  use codec_lucene9::roaring::{RoaringBitmap, RoaringView};
  use codec_lucene9::terms_read::TermEntry;

  use super::doc_iter::{DocIter, DocSource, RoaringAndDocIter, RoaringDocIter, SegmentDocIter};
  use super::multi_term::for_each_doc;
  use super::segment_reader::SegmentReader;

  /// Tier-1 skew gate (M4 §5): when max/min df reaches this ratio the AND
  /// runs lead-cursor + contains() probes (the high-df side is never read
  /// wholesale, 用户指令②); below it, k-way byte-cursor merge-intersect.
  /// Initial value 4 per spec §5; T5 bench calibrates and records the
  /// chosen value in .superpowers/sdd/m4-bench-report.md.
  pub(crate) const SKEW_RATIO: u64 = 4; // bench-calibrated (T5)
  ```

  `collect_bool_entries` 原样保留。`materialize_clause` / `fold_clauses` 原样保留（OR 分支专用，T4 删除）。在其后插入：

  ```rust
  /// Tier-2 query-time materialization of one low-df clause: full postings
  /// scan into an ascending doc vec (spec §5: df<4096 → ≤4095 docs,
  /// bounded). Docs arrive ascending from the enum.
  fn materialize_docs(
      seg: &SegmentReader,
      entry: &TermEntry,
      has_freqs: bool,
  ) -> io::Result<Vec<u32>> {
      let mut docs = Vec::with_capacity(entry.doc_freq as usize);
      for_each_doc(seg, entry, has_freqs, &mut |d| docs.push(d))?;
      Ok(docs)
  }

  /// Opens every clause's probe view (one container-directory scan each —
  /// cheap, no data-section reads). None = no clause has a bitmap (tier 3).
  fn probe_clauses(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
  ) -> io::Result<Option<Vec<Option<RoaringView>>>> {
      let mut probes = Vec::with_capacity(entries.len());
      for (_, entry) in entries {
          probes.push(seg.probe_term_bitmap(entry)?);
      }
      if probes.iter().all(|p| p.is_none()) {
          return Ok(None); // tier 3
      }
      Ok(Some(probes))
  }

  /// Tier-1/2 AND over views (M4 §5). entries are df-ascending (==
  /// cardinality-ascending, validation ③), so construction order is
  /// cheapest-first.
  fn and_iterator(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
  ) -> io::Result<Option<SegmentDocIter>> {
      let Some(probes) = probe_clauses(seg, entries)? else {
          return Ok(None); // tier 3
      };
      let bitmap_count = probes.iter().filter(|p| p.is_some()).count();
      let mut sources: Vec<DocSource> = Vec::new();
      let mut probe_views: Vec<RoaringView> = Vec::new();
      if bitmap_count < entries.len() {
          // tier 2: materialize the bitmap-less clauses (df<4096, bounded)
          // and point-probe their candidates against every bitmap view —
          // zero wholesale bitmap reads (spec §5 档 2)
          for ((_, entry), probe) in entries.iter().zip(probes.into_iter()) {
              match probe {
                  Some(v) => probe_views.push(v),
                  None => sources.push(DocSource::slice(materialize_docs(seg, entry, has_freqs)?)),
              }
          }
      } else if entries.last().unwrap().0 as u64 >= SKEW_RATIO * entries[0].0 as u64 {
          // tier 1 skewed: smallest side full-mode iteration, the rest
          // contains-probes (the high-df side is never read wholesale)
          let Some(lead) = seg.open_term_bitmap(&entries[0].1)? else {
              return Ok(None); // unreachable: probe succeeded on the same bytes
          };
          sources.push(DocSource::view(lead));
          probe_views.extend(probes.into_iter().skip(1).map(|p| p.expect("tier 1: all present")));
      } else {
          // tier 1 non-skewed: k-way byte-cursor merge-intersect over
          // full-mode views (spec §5 双字节游标 merge-intersect, k 路泛化)
          for (_, entry) in entries {
              let Some(v) = seg.open_term_bitmap(entry)? else {
                  return Ok(None); // unreachable: probe succeeded on the same bytes
              };
              sources.push(DocSource::view(v));
          }
      }
      Ok(Some(SegmentDocIter::RoaringAnd(RoaringAndDocIter::new(
          sources,
          probe_views,
      ))))
  }
  ```

  `segment_iterator` 与 `count` 改为（AND 换引擎、OR 暂留折叠路径）：

  ```rust
  /// Three-tier segment iterator (spec §5): Some = roaring path taken
  /// (tier 1/2); None = tier 3, the caller builds the PFOR iterator.
  /// AND: M4 view engine (and_iterator); OR: M3 container fold (T4 switches).
  pub(crate) fn segment_iterator(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
      is_and: bool,
  ) -> io::Result<Option<SegmentDocIter>> {
      if is_and {
          return and_iterator(seg, entries, has_freqs);
      }
      let Some(b) = fold_clauses(seg, entries, has_freqs, false)? else {
          return Ok(None);
      };
      Ok(Some(SegmentDocIter::Roaring(RoaringDocIter::new(b))))
  }

  /// Count fast path (spec §5: count 走同一引擎). AND: drives the same
  /// view iterator (count == iteration by construction, 关键设计事实 10).
  /// OR: cardinality of the folded bitmap (T4 unifies on the view engine).
  /// None = tier 3, caller iterates.
  pub(crate) fn count(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
      is_and: bool,
  ) -> io::Result<Option<u64>> {
      if is_and {
          let Some(mut it) = and_iterator(seg, entries, has_freqs)? else {
              return Ok(None);
          };
          let mut n = 0u64;
          loop {
              if it.next_doc()? == NO_MORE_DOCS {
                  break;
              }
              n += 1;
          }
          return Ok(Some(n));
      }
      Ok(fold_clauses(seg, entries, has_freqs, false)?.map(|b| b.cardinality()))
  }
  ```

- [ ] **Step 3.5: core 全绿 + codec 回归 + fmt**

  ```
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. 45 passed; 0 failed; ...
  $ cargo test -p codec-lucene9 2>&1 | tail -2
  test result: ok. 163 passed; 0 failed; 1 ignored; ...
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  ```

  （44 → 45：新增 `and_skew_probe_matches_pfor`；`bool_query_three_tier_roaring` 断言改造数量不变。codec 零改动。）

- [ ] **Step 3.6: 提交**

  ```
  git add crates/core/src/search/doc_iter.rs crates/core/src/search/roaring_exec.rs crates/core/src/search/mod.rs
  git commit -m "feat: AND df-skew probe — tier-2 contains filter, tier-1 SKEW_RATIO strategy"
  ```

---

## Task 4: OR/迭代字节游标 + RoaringDocIter 改造 + Term count 清理（删 header 路径）

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（`RoaringDocIter` 改包视图游标；新增 `RoaringOrDocIter`；`SegmentDocIter::RoaringOr` 变体接线；import 去 `RoaringBitmap/RoaringCursor`）
- Modify: `crates/core/src/search/roaring_exec.rs`（删 `materialize_clause`/`fold_clauses`；新增 `or_iterator`；`segment_iterator`/`count` OR 分支切换；模块文档与 import 同步）
- Modify: `crates/core/src/search/query.rs`（Term 分支改走 `open_term_bitmap` + 视图版 `RoaringDocIter`）
- Modify: `crates/core/src/search/searcher.rs`（Term count 直读 `entry.doc_freq`；And/Or count 注释同步）
- Modify: `crates/core/src/search/segment_reader.rs`（删 `read_term_bitmap`/`read_term_bitmap_header` 两个包装；import 去 `RoaringBitmap`）
- Modify: `crates/codec-lucene9/src/postings_read.rs`（删 `read_term_bitmap`/`read_term_bitmap_header` 两个函数；删 `read_term_bitmap_validates_and_reads` 测试；v1 落档测试断言改 open/probe；import 去 `RoaringBitmap`）
- Test: `crates/core/src/search/mod.rs`（`bool_query_three_tier_roaring` 的 OR 断言 ×3 改 `RoaringOr`、负向断言硬化、新增并集锚点 4571）

**Interfaces:**
- Consumes: T2 的 `SegmentReader::open_term_bitmap`、`RoaringView::{cursor, cursor_next, cursor_advance}`、`ViewCursor`；T3 的 `DocSource::{view, slice}` 与 `materialize_docs`（OR 档 2 物化复用）。
- Produces（T5 依赖这些名字，不得改名）:
  ```rust
  // doc_iter.rs
  pub struct RoaringDocIter { view: Box<RoaringView>, cursor: ViewCursor, doc: i32 }
  impl RoaringDocIter { pub fn new(view: RoaringView) -> RoaringDocIter }  // 签名变化：RoaringBitmap → RoaringView
  pub struct RoaringOrDocIter { sources: Vec<DocSource>, doc: i32 }
  impl RoaringOrDocIter { pub fn new(sources: Vec<DocSource>) -> RoaringOrDocIter }
  // SegmentDocIter 新增变体
  RoaringOr(RoaringOrDocIter),
  // roaring_exec.rs
  fn or_iterator(seg: &SegmentReader, entries: &[(u32, TermEntry)], has_freqs: bool) -> io::Result<Option<SegmentDocIter>>;
  pub(crate) fn segment_iterator(seg: &SegmentReader, entries: &[(u32, TermEntry)], has_freqs: bool, is_and: bool) -> io::Result<Option<SegmentDocIter>>; // 签名不变
  pub(crate) fn count(seg: &SegmentReader, entries: &[(u32, TermEntry)], has_freqs: bool, is_and: bool) -> io::Result<Option<u64>>;                        // 签名不变
  ```

  语义决定（spec §6）：(a) **OR = k 路 merge-union**：bitmap 子句各开一个 full 视图（打开即 presence 探测，全 None → 档 3，事实 17），无 bitmap 子句物化 slice（df<4096 有界）——**零容器重建**；OR 的全量字节读取不可避免（spec §2 明确不做避免），去的是重建税。(b) **Term 迭代**（iterm/top_docs）= 单 full 视图游标；`RoaringDocIter::new` 签名从 `RoaringBitmap` 改为 `RoaringView`，两个既有调用点（query.rs Term 分支、roaring_exec OR 分支）本任务内同步改写，编译器钉死无遗漏。(c) **Term count = `entry.doc_freq` 直读**（用户指令③）：校验③保证 cardinality == doc_freq，bitmap 头路径的值永不可能不同——`read_term_bitmap_header` 全链路删除（事实 16）；`RL_BITMAP` kill switch 在 Term count 上不再相关（两路径值相同，A/B 无差异），其余路径门不变。(d) **And/Or count 共用 `segment_iterator` 驱动计数**（spec §5 同一引擎；M3 的 cardinality 折叠捷径退出查询路径）。(e) `needs_freq == true` 永不进本模块（query.rs 既有门不动）。

### Steps

- [ ] **Step 4.1: 写失败测试** — `crates/core/src/search/mod.rs` 的 `bool_query_three_tier_roaring`（T3 改造后的形态）五处编辑：

  ①②③ 三处 OR 路径断言 `SegmentDocIter::Roaring(_)` → `SegmentDocIter::RoaringOr(_)`（档 1 OR、档 2 mixed OR、缺失子句 OR；断言消息不变）：

  ```rust
          let q = Query::or("message", &["hot", "scorching"]);
          let it = q.segment_iterator(seg, false).unwrap().unwrap();
          assert!(
              matches!(it, SegmentDocIter::RoaringOr(_)),
              "tier-1 OR must be roaring"
          );
  ```

  （另两处同理：`Query::or("message", &["scorching", "warm3"])` 消息 `"tier-2 mixed OR must be roaring"`；`Query::or("message", &["scorching", "nosuch"])` 消息 `"OR with one present bitmap clause"`。）

  ④ 两处负向断言硬化（T3/T4 引入 RoaringAnd/RoaringOr 后 `!matches!(Roaring)` 有洞：错误的 roaring 变体也能通过）——档 3 与 needs_freq 改正向变体断言：

  ```rust
          // 档 3：两个子句都无 bitmap → 既有 PFOR 路径
          let q = Query::and("message", &["warm1", "warm3"]);
          let it = q.segment_iterator(seg, false).unwrap().unwrap();
          assert!(
              matches!(it, SegmentDocIter::And(_)),
              "tier-3 stays PFOR conjunction"
          );
          // needs_freq=true：永不走 roaring（bitmap 无 freq）
          let q = Query::and("message", &["hot", "scorching"]);
          let it = q.segment_iterator(seg, true).unwrap().unwrap();
          assert!(
              matches!(it, SegmentDocIter::And(_)),
              "needs_freq stays postings"
          );
  ```

  ⑤ 并集锚点（view+slice 去重钉死；推导：warm3 = {d: d%7==3} 共 714 docs，d≥500 且 d≡3 mod 7 者 643 个（500≡3 mod 7，500…4994 step 7）与 scorching 重叠 → 4500+714−643 = 4571）——加在既有 714 锚点之后：

  ```rust
          assert_eq!(
              s.count(&Query::or("message", &["scorching", "warm3"]))
                  .unwrap(),
              4571
          );
  ```

- [ ] **Step 4.2: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core 2>&1 | tail -5
  error[E0599]: no variant named `RoaringOr` found for enum `SegmentDocIter`
  ```

- [ ] **Step 4.3: doc_iter.rs——RoaringDocIter 改造 + RoaringOrDocIter** — import 行（T3 后形态）

  ```rust
  use codec_lucene9::roaring::{RoaringBitmap, RoaringCursor, RoaringView, ViewCursor};
  ```

  改为：

  ```rust
  use codec_lucene9::roaring::{RoaringView, ViewCursor};
  ```

  `RoaringDocIter` 整段（doc_iter.rs:516-568，含 "// ── Roaring (inline term bitmap, M3 §5) ──" 节标题与文档注释）替换为：

  ```rust
  // ── Roaring (inline term bitmap view, M4 §6) ─────────────────────────

  /// DocIter over a term's inline bitmap (M4 §6): wraps the zero-copy
  /// full-mode view's byte cursor — next_doc/advance delegate to the
  /// view's `cursor_next`/`cursor_advance` (the M3 `RoaringCursor`
  /// contract, 关键设计事实 8). The view is boxed: `SegmentDocIter` size
  /// discipline (关键设计事实 6). freq() is 1 — the bitmap carries no
  /// freqs, and needs_freq paths never get this iterator (correctness
  /// requirement (e)).
  pub struct RoaringDocIter {
      view: Box<RoaringView>,
      cursor: ViewCursor,
      doc: i32,
  }

  impl RoaringDocIter {
      pub fn new(view: RoaringView) -> RoaringDocIter {
          let cursor = view.cursor(); // full-mode contract (open_term_bitmap)
          RoaringDocIter {
              view: Box::new(view),
              cursor,
              doc: -1,
          }
      }
  }

  impl DocIter for RoaringDocIter {
      fn doc_id(&self) -> i32 {
          self.doc
      }

      fn next_doc(&mut self) -> io::Result<i32> {
          if self.doc == NO_MORE_DOCS {
              return Ok(NO_MORE_DOCS);
          }
          self.doc = match self.view.cursor_next(&mut self.cursor) {
              Some(d) => d as i32,
              None => NO_MORE_DOCS,
          };
          Ok(self.doc)
      }

      fn advance(&mut self, target: i32) -> io::Result<i32> {
          if target > self.doc {
              self.doc = match self
                  .view
                  .cursor_advance(&mut self.cursor, target.max(0) as u32)
              {
                  Some(d) => d as i32,
                  None => NO_MORE_DOCS,
              };
          }
          Ok(self.doc)
      }
  }
  ```

  在 T3 的 `RoaringAndDocIter` 的 `impl DocIter` 之后插入（完整实现）：

  ```rust
  /// OR execution over views (M4 §6): k-way merge-union over `sources`
  /// (full-mode bitmap byte cursors + materialized low-df slices) with
  /// min-current dedup — zero container rebuilds. The full byte read is
  /// unavoidable (spec §2 明确不做); the rebuild tax is gone. freq() is 1
  /// (ConstantScore, trait default).
  pub struct RoaringOrDocIter {
      sources: Vec<DocSource>,
      doc: i32,
  }

  impl RoaringOrDocIter {
      pub fn new(sources: Vec<DocSource>) -> RoaringOrDocIter {
          debug_assert!(!sources.is_empty());
          RoaringOrDocIter { sources, doc: -1 }
      }
  }

  impl DocIter for RoaringOrDocIter {
      fn doc_id(&self) -> i32 {
          self.doc
      }

      fn next_doc(&mut self) -> io::Result<i32> {
          if self.doc == NO_MORE_DOCS {
              return Ok(NO_MORE_DOCS);
          }
          if self.doc >= 0 {
              // union dedup: move every source sitting on the last emitted doc
              let last = self.doc as u32;
              for s in &mut self.sources {
                  if s.current() == Some(last) {
                      s.next();
                  }
              }
          }
          let mut best: Option<u32> = None;
          for s in &self.sources {
              if let Some(d) = s.current() {
                  if best.is_none_or(|b| d < b) {
                      best = Some(d);
                  }
              }
          }
          self.doc = match best {
              Some(d) => d as i32,
              None => NO_MORE_DOCS,
          };
          Ok(self.doc)
      }

      fn advance(&mut self, target: i32) -> io::Result<i32> {
          if self.doc >= target || self.doc == NO_MORE_DOCS {
              return Ok(self.doc);
          }
          for s in &mut self.sources {
              s.advance(target.max(0) as u32);
          }
          self.doc = -1;
          self.next_doc()
      }
  }
  ```

  `SegmentDocIter` 枚举加变体并在三个 match 接线（doc_id / next_doc / advance 各加一行 `Self::RoaringOr(o) => o.doc_id(),` / `o.next_doc(),` / `o.advance(t),`，与 `RoaringAnd` 臂并列；freq 落 `_ => 1` 臂无需改）：

  ```rust
  pub enum SegmentDocIter {
      Docs(DocsEnum),
      Freqs(DocsFreqsEnum),
      All(MatchAllIter),
      And(ConjunctionDocIter),
      Or(DisjunctionDocIter),
      Bitset(BitsetDocIter),
      Phrase(PhraseDocIter),
      Roaring(RoaringDocIter),
      RoaringAnd(RoaringAndDocIter),
      RoaringOr(RoaringOrDocIter),
  }
  ```

- [ ] **Step 4.4: roaring_exec.rs——OR 切换 + 折叠引擎删除** — 模块文档注释（T3 形态）中

  ```rust
  //! vec (df<4096, bounded). OR still folds container bitmaps here (T4
  //! switches it to byte cursors). When NO clause has a bitmap the callers
  ```

  改为：

  ```rust
  //! vec (df<4096, bounded). OR runs a k-way merge-union over full-mode
  //! byte cursors + materialized slices (M4 §6) — zero container rebuilds.
  //! When NO clause has a bitmap the callers
  ```

  import 区（T3 形态）改为：

  ```rust
  use std::io;

  use codec_lucene9::postings_read::NO_MORE_DOCS;
  use codec_lucene9::roaring::RoaringView;
  use codec_lucene9::terms_read::TermEntry;

  use super::doc_iter::{
      DocIter, DocSource, RoaringAndDocIter, RoaringOrDocIter, SegmentDocIter,
  };
  use super::multi_term::for_each_doc;
  use super::segment_reader::SegmentReader;
  ```

  **删除** `materialize_clause` 与 `fold_clauses` 两个函数（M3 折叠引擎整体退出查询路径；`RoaringBitmap::{and,or}` 留在 codec 测试，事实 12/16）。`collect_bool_entries`/`SKEW_RATIO`/`materialize_docs`/`probe_clauses`/`and_iterator` 原样保留。新增：

  ```rust
  /// Tier-1/2 OR over views (M4 §6): k-way merge-union — one full-mode
  /// open per clause (the open doubles as the bitmap-presence probe,
  /// 关键设计事实 17), materialized slices for the bitmap-less clauses
  /// (df<4096, bounded). All-None = tier 3.
  fn or_iterator(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
  ) -> io::Result<Option<SegmentDocIter>> {
      let mut sources: Vec<DocSource> = Vec::new();
      let mut any_bitmap = false;
      for (_, entry) in entries {
          match seg.open_term_bitmap(entry)? {
              Some(v) => {
                  any_bitmap = true;
                  sources.push(DocSource::view(v));
              }
              None => sources.push(DocSource::slice(materialize_docs(seg, entry, has_freqs)?)),
          }
      }
      if !any_bitmap {
          return Ok(None); // tier 3
      }
      Ok(Some(SegmentDocIter::RoaringOr(RoaringOrDocIter::new(
          sources,
      ))))
  }
  ```

  `segment_iterator` 与 `count` 整段替换为：

  ```rust
  /// Three-tier segment iterator (spec §5): Some = roaring path taken
  /// (tier 1/2); None = tier 3, the caller builds the PFOR iterator.
  /// AND: view engine with the skew strategy (T3); OR: k-way merge-union
  /// over byte cursors + materialized slices (M4 §6).
  pub(crate) fn segment_iterator(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
      is_and: bool,
  ) -> io::Result<Option<SegmentDocIter>> {
      if is_and {
          and_iterator(seg, entries, has_freqs)
      } else {
          or_iterator(seg, entries, has_freqs)
      }
  }

  /// Count fast path (spec §5: count 走同一引擎): both directions drive
  /// the same view iterator to exhaustion (count == iteration by
  /// construction, 关键设计事实 10). None = tier 3, caller iterates.
  pub(crate) fn count(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
      is_and: bool,
  ) -> io::Result<Option<u64>> {
      let Some(mut it) = segment_iterator(seg, entries, has_freqs, is_and)? else {
          return Ok(None);
      };
      let mut n = 0u64;
      loop {
          if it.next_doc()? == NO_MORE_DOCS {
              break;
          }
          n += 1;
      }
      Ok(Some(n))
  }
  ```

- [ ] **Step 4.5: query.rs / searcher.rs / segment_reader.rs** —

  ① `crates/core/src/search/query.rs` Term 分支（query.rs:132-139）：

  ```rust
                  // M3 §5 档 1 term 路径：校验通过的内联 bitmap → roaring
                  // 迭代；needs_freq（bitmap 无 freq）与任何校验失败保持
                  // postings 枚举。
                  if !needs_freq {
                      if let Some(b) = seg.read_term_bitmap(&entry)? {
                          return Ok(Some(SegmentDocIter::Roaring(RoaringDocIter::new(b))));
                      }
                  }
  ```

  改为：

  ```rust
                  // M4 §6 Term 路径：校验通过的零拷贝 full 视图 → 字节游标
                  // 迭代；needs_freq（bitmap 无 freq）与任何校验失败保持
                  // postings 枚举。
                  if !needs_freq {
                      if let Some(v) = seg.open_term_bitmap(&entry)? {
                          return Ok(Some(SegmentDocIter::Roaring(RoaringDocIter::new(v))));
                      }
                  }
  ```

  ② `crates/core/src/search/searcher.rs` Term count 分支（searcher.rs:64-77）改为：

  ```rust
          if let Query::Term { field, term } = query {
              let mut total = 0u64;
              for (_doc_base, seg) in self.reader.leaves() {
                  if let Some((_, entry)) = seg.seek_term(field, term)? {
                      total += entry.doc_freq as u64;
                  }
              }
              return Ok(total);
          }
  ```

  doc 注释（searcher.rs:56-62）改为：

  ```rust
      /// ConstantScore TermQuery: count = sum of per-segment doc_freq (no
      /// postings iteration needed — doc_freq is in the TermEntry after
      /// seek_exact; M4 §6 用户指令③: the bitmap header read is gone —
      /// validation ③ guarantees cardinality == doc_freq, so the two
      /// values could never differ). Fallback to iteration for MatchAll
      /// and unknown terms. Multi-term queries count per segment: popcount
      /// on the bitset path (spec §4), plain iteration on the OR path.
  ```

  And/Or count 注释（searcher.rs:98-99）改为：

  ```rust
          // M4 §5: And/Or count 与迭代共用同一视图引擎——任一子句有 bitmap
          // 即驱动同一迭代器计数（档 1/2），否则按段迭代（档 3，既有行为）。
  ```

  ③ `crates/core/src/search/segment_reader.rs`：删除 `read_term_bitmap`（segment_reader.rs:83-91，含文档注释）与 `read_term_bitmap_header`（segment_reader.rs:93-101，含文档注释）两个包装；import 行 `use codec_lucene9::roaring::RoaringBitmap;`（segment_reader.rs:10）删除（T2 加的 `RoaringView` import 保留）。

- [ ] **Step 4.6: codec 删除 + 测试同步** — `crates/codec-lucene9/src/postings_read.rs`：

  ① 删除 `read_term_bitmap`（postings_read.rs:169-188，含文档注释）与 `read_term_bitmap_header`（postings_read.rs:190-222，含文档注释）两个函数。
  ② import 行（T2 后形态）`use crate::roaring::{self, RoaringBitmap, RoaringView};` 改为 `use crate::roaring::{self, RoaringView};`（`roaring::` 自引用保留：`locate_bitmap_region` 用 `BITMAP_MIN_DF`/`max_bitmap_len`）。
  ③ 删除测试 `read_term_bitmap_validates_and_reads`（postings_read.rs:1519-1567 整段）——覆盖无损失（事实 16：未命中/df 不符/无 bitmap 索引三类 None 由 T2 `open_and_probe_term_bitmap_match_postings` 同形覆盖，deserialize 校验由 `inline_bitmap_region_round_trip` 与 roaring.rs 测试覆盖）。
  ④ T1 的 `read_term_bitmap_falls_back_on_v1_layout` 中：

  ```rust
          // v2 reader: full + header paths both reject at the version gate
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
          assert!(postings.read_term_bitmap(&e, 6000).unwrap().is_none());
          assert_eq!(postings.read_term_bitmap_header(&e, 6000).unwrap(), None);
  ```

  改为：

  ```rust
          // v2 reader: both view open modes reject at the version gate
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
          assert!(postings.open_term_bitmap(&e, 6000).unwrap().is_none());
          assert!(postings.probe_term_bitmap(&e, 6000).unwrap().is_none());
  ```

  ⑤ T2 的 `open_term_bitmap` 文档注释时态同步："replaces the M3 deserialize-rebuild `read_term_bitmap`, which stays until T4 for the write-side round-trip tests" → "replaced the M3 deserialize-rebuild `read_term_bitmap` (deleted in T4, 事实 16)"。

- [ ] **Step 4.7: 两 crate 全绿 + grep 验证 + fmt**

  ```
  $ cargo test -p codec-lucene9 2>&1 | tail -2
  test result: ok. 162 passed; 0 failed; 1 ignored; ...
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. 45 passed; 0 failed; ...
  $ grep -rn "read_term_bitmap_header" crates/            # → 无输出（全链路删除）
  $ grep -rn "read_term_bitmap\b" crates/ | grep -v "open_term_bitmap\|probe_term_bitmap"   # → 无输出
  $ grep -rn "RoaringBitmap\|RoaringCursor" crates/core/src   # → 无输出（core 只碰视图 API）
  $ grep -rn "deserialize" crates/core/src                # → 无输出（查询路径不再调用，Global Constraints）
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  ```

  （codec 163 → 162：删 `read_term_bitmap_validates_and_reads`；core 45 不变——断言改造/硬化/锚点数量不变。）

- [ ] **Step 4.8: 提交**

  ```
  git add crates/core/src/search/doc_iter.rs crates/core/src/search/roaring_exec.rs crates/core/src/search/query.rs crates/core/src/search/searcher.rs crates/core/src/search/segment_reader.rs crates/core/src/search/mod.rs crates/codec-lucene9/src/postings_read.rs
  git commit -m "feat: OR/iteration byte cursors + view-backed RoaringDocIter; Term count reads doc_freq directly"
  ```

---

## Task 5: 电池复跑 + bench 复测 + SKEW_RATIO 标定 + 报告

**Files:**
- 无 repo 文件改动（全部产物落 `.superpowers/sdd/`，M2 起 gitignored，不提交）；唯一可能的代码改动 = `roaring_exec.rs` 的 `SKEW_RATIO` 终值（若标定越出 4，单独一个 `bench:` commit，见 Step 5.7 判定规则）。
- Create: `.superpowers/sdd/m4-bench-report.md`（报告）、`m4-q-raw.txt`/`m4-q.txt`、`m4-bench-{rust-roaring,rust-pfor,java}.out`、`m4-counts-{rust-roaring,rust-pfor,java}.txt`（+`m4-counts-java.clean.txt`）、`m4-dense-bench-{rust-roaring,rust-pfor}.out`、`m4-skew-micro.txt`、`m4-write-throughput.txt`

**口径（全部与 M3 T7 逐字一致，关键设计事实 14）**：1M docs seed 42；`--warmup 10 --iter 30`；Java 侧 `--no-cache`；三路串行（避免 CPU 争用）；查询文件经 Java 侧 `--dump-queries --tasks 50 --seed 42` 产出 + `awk -F'\t' '!($1=="TERM" && $4<4096)'` df 守卫；hit-counts 走 stderr；三路对拍 = roaring==pfor plain diff 为空 + Java 等效对拍（非 term= 行 sorted diff + term= 重合行 awk join）。M3 基线列取自 task-7-report 复测表（roaring/pfor：term high 0.60 / and high 0.53 / or high 0.87 / iterm high 0.40；稠密 and 8.7x / or 15.2x / iterm 0.81 / term 0.29）。

### Steps

- [ ] **Step 5.1: 全量绿基线（T4 提交后的 HEAD）**

  ```
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ cargo test -p codec-lucene9 2>&1 | tail -2    # 162 passed
  $ cargo test -p rustlucene-core 2>&1 | tail -2  # 45 passed
  ```

  v1 落档验收在此步内完成（**不加电池变体**，Global Constraints）：codec `read_term_bitmap_falls_back_on_v1_layout` + core `bitmap_v1_index_falls_back_to_postings` 两个 Rust 测试绿即通过。

- [ ] **Step 5.2: `make log-test` 五变体**（脚本/Makefile 零改动；200000 文档：seed 42 默认 / 43 `--positions` / 44 `--sparse` / 45 `--bigdict` / 46 `--bitmap`）

  ```
  $ make log-test 2>&1 | tee /tmp/log-test-m4.log; echo "EXIT=$?"
  EXIT=0
  $ grep -c "No problems were detected with this index" /tmp/log-test-m4.log
  11
  $ grep -c "SEARCH_INTEROP_OK" /tmp/log-test-m4.log   # 5
  $ grep -c "LOG_INTEROP_OK" /tmp/log-test-m4.log      # 5
  $ grep "Bitmap A/B\|FORCEMERGE_OK" /tmp/log-test-m4.log
  # --bitmap 变体（seed 46）含 "Bitmap A/B: Rust searchdump bitmap on vs off
  # (RL_BITMAP=0)"（diff 为空）+ FORCEMERGE_OK + Post-merge search diff
  ```

  验收（Global Constraints 逐字）：EXIT=0；每次 CheckIndex 输出 "No problems"（11 次）；5× SEARCH_INTEROP_OK + 5× LOG_INTEROP_OK；`--bitmap` 变体内 Rust bitmap on/off searchdump diff 为空。

- [ ] **Step 5.3: bench 索引（1M docs，seed 42，双路重建——/tmp 可能在机器重启后丢失，不依赖 M3 残留）**

  ```
  $ cargo build --release
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      logwrite /tmp/rl-bench4-rust 1000000 42 --bitmap
  → WROTE docs=1000000 elapsed_ms=... docs_per_sec=...
  $ CP="interop/java/classes:interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar"
  $ make java-classes   # interop/java/classes 不在时
  $ java -cp "$CP" JavaLogBench /tmp/rl-bench4-java 1000000 1 42
  → BENCH elapsed_ms=... docs_per_sec=...
  ```

- [ ] **Step 5.4: 查询文件 + df 守卫**（Java 侧 dump，同 M3）

  ```
  $ java -cp "$CP" SearchBench /tmp/rl-bench4-java message \
      --dump-queries .superpowers/sdd/m4-q-raw.txt --tasks 50 --seed 42   # 预期 498 行
  $ awk -F'\t' '!($1=="TERM" && $4<4096)' .superpowers/sdd/m4-q-raw.txt > .superpowers/sdd/m4-q.txt
  $ wc -l .superpowers/sdd/m4-q.txt   # 预期 493（剔除同样 5 条 med 截断词 conne/buffer/chec/buffe/evict）
  $ awk -F'\t' '$1=="TERM" && $4<4096' .superpowers/sdd/m4-q.txt | wc -l   # 0（ALL TERM LINES df>=4096）
  $ grep -c $'^AND\thigh\t' .superpowers/sdd/m4-q.txt   # 50
  ```

- [ ] **Step 5.5: 三路 bench + hit-counts 对拍**（串行）

  ```
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchbench /tmp/rl-bench4-rust message \
      --load-queries .superpowers/sdd/m4-q.txt --warmup 10 --iter 30 \
      > .superpowers/sdd/m4-bench-rust-roaring.out 2> .superpowers/sdd/m4-counts-rust-roaring.txt
  $ RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchbench /tmp/rl-bench4-rust message \
      --load-queries .superpowers/sdd/m4-q.txt --warmup 10 --iter 30 \
      > .superpowers/sdd/m4-bench-rust-pfor.out 2> .superpowers/sdd/m4-counts-rust-pfor.txt
  $ java -cp "$CP" SearchBench /tmp/rl-bench4-java message \
      --load-queries .superpowers/sdd/m4-q.txt --no-cache --warmup 10 --iter 30 \
      > .superpowers/sdd/m4-bench-java.out 2> .superpowers/sdd/m4-counts-java.txt
  ```

  对拍（口径同 M3 报告 §5）：

  ```
  $ diff .superpowers/sdd/m4-counts-rust-roaring.txt .superpowers/sdd/m4-counts-rust-pfor.txt \
      && echo "COUNTS_MATCH: roaring == pfor"        # 门槛 1：plain diff 必须为空
  # 门槛 2（Java 等效对拍）：stderr 去 JVM 噪声/空行/注释行 → m4-counts-java.clean.txt；
  #   diff <(grep -v '^term=' c-rust | sort) <(grep -v '^term=' c-java | sort)  → 空
  #   term= 重合行 awk join → mismatches=0
  #   （iterm 块覆盖查询文件全部 TERM 行，逐 term 计数三路一致）
  ```

- [ ] **Step 5.6: 稠密 level 字段 A/B**（同 binary RL_BITMAP on/off；spec §7 "稠密保持 ≥8x"）

  ```
  $ printf 'TERM\thigh\tINFO\t200000\nTERM\thigh\tWARN\t200000\nTERM\thigh\tERROR\t200000\nAND\thigh\tINFO\tWARN\nAND\thigh\tERROR\tWARN\nOR\thigh\tINFO\tWARN\nOR\thigh\tERROR\tWARN\n' > /tmp/q-level.txt
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchbench /tmp/rl-bench4-rust level --load-queries /tmp/q-level.txt --warmup 5 --iter 20 \
      > .superpowers/sdd/m4-dense-bench-rust-roaring.out 2>&1
  $ RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchbench /tmp/rl-bench4-rust level --load-queries /tmp/q-level.txt --warmup 5 --iter 20 \
      > .superpowers/sdd/m4-dense-bench-rust-pfor.out 2>&1
  ```

  预期：and ≥8x、or ≥14x（M3：8.7x/15.2x，去税后只应更高）。

- [ ] **Step 5.7: df-skew 微基准 → SKEW_RATIO 标定**（scratch crate `/tmp/m4skew`，path-dep 只读引用 `codec-lucene9` + `rustlucene-core`，repo 零改动——同 M3 `/tmp/roarbench`、`/tmp/densewrite` 先例）

  **索引**（一次性，单 segment 1M docs，text 字段 `f` 空白分词无 positions——同 M3 `message` 字段形状；`IndexWriterConfig{bitmap:true}`，一次 commit）：嵌套 df 阶梯 term `k0..k8`，df ∈ {4096, 8192, 16384, 32768, 65536, 131072, 262144, 524288, 1000000}，term `ki` 的 docs = [0, df_i)。嵌套 ⇒ AND(k0, ki) 精确 = 4096（正确性锚点），ratio r = df_i/4096 ∈ {1, 2, 4, 8, 16, 32, 64, 128, 244}。

  **测量**（每对 (k0, ki)，两策略都用 T2 的真实视图 API，与查询路径 1:1 对应；经 `TermsDict`/`PostingsReader` pub API 定位 term）：

  - **A = merge-intersect**（T3 非偏斜路径）：两个 `open_term_bitmap` full 视图 + `ViewCursor` merge（`cursor_advance` 对舞），驱动到尽头计数；
  - **B = skew probe**（T3 偏斜路径）：k0 `open_term_bitmap` full 游标迭代 + ki `probe_term_bitmap` 逐候选 `contains(doc)`；
  - 计时口径同 m3-compare 微基准（warmup 10 + 按 0.25s 标定 reps 取均值）；**每对先断言 A 计数 == B 计数 == 4096** 再计时（防测量代码写错）。

  **判定规则**：B 自 r≥4 起全胜、r<4 时 A 胜或平（±10% 噪声带）→ `SKEW_RATIO = 4` 维持，报告记录交叉证据；否则把常量改到实测交叉点（向最近的 2 的幂取整），单独提交 `git commit -m "bench: calibrate SKEW_RATIO=<n> from m4 skew micro crossover"` 并复跑 `cargo test -p rustlucene-core`（改动仅常量一行，电池不必复跑——偏斜/非偏斜两路径正确性由 `and_skew_probe_matches_pfor` 与三档测试双向覆盖，与阈值取值无关）。

  **标定先验**（来自 `.superpowers/sdd/m3-roaring-compare-report.md`，事实 14）：sparse array——iterate 3.84 ns/doc、M3 deserialize 244.9µs@18.8k docs（≈13 ns/doc 全量税；M4 零拷贝 full 读去掉逐元素校验，只剩顺序字节读）；dense bitset——iterate 6.46 ns/doc、deserialize 171.6µs@200k；ultra run——deserialize 1.43µs（税≈0）、iterate 4.62 ns/doc。A 成本 ≈ 大侧 full 读 + 迭代（正比大侧 payload 字节）；B 成本 ≈ 4096 次桶内定点测位（bitset 8B/次，page-cache 内随机读）——交叉点预期低个位倍率，与初始值 4 同量级，实测定夺。

  输出 `.superpowers/sdd/m4-skew-micro.txt`（ratio × A µs × B µs × 胜者表）。

- [ ] **Step 5.8: 写侧吞吐 + 磁盘**（命令同 M3 T7.4；产物 `m4-write-throughput.txt`）

  ```
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- logwrite /tmp/rl-bench4-wt-bm 1000000 42 --bitmap
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- logwrite /tmp/rl-bench4-wt-off 1000000 42
  $ du -sb /tmp/rl-bench4-wt-bm /tmp/rl-bench4-wt-off
  ```

  预期（spec §7 "写侧因去 crc 略降开销"）：吞吐损失 ≤ M3 的 0.78–2.4%；磁盘增量略低于 M3 的 +15.0%（每 bitmap 少 4B crc）。

- [ ] **Step 5.9: 报告落盘** `.superpowers/sdd/m4-bench-report.md`（gitignored，不提交），骨架：

  1. 口径（HEAD、语料、缓存、查询集、bench 参数、host/toolchain——Step 5.3-5.5 逐字命令）；
  2. 三路对比表（term/and/or/iterm × high/med，qps/p50/p90/p99；附 M3 复测基线列对照）；
  3. **spec §7 预期逐条对照**：稀疏 AND roaring/pfor ≥1x？、AND 低×高 df（med×high 偏斜对）超 PFOR？、term high 回 ~1.0x？、稠密 ≥8x？——逐条给实测值与结论；
  4. skew 标定表 + `SKEW_RATIO` 终值与依据（Step 5.7 输出）；
  5. 写侧（吞吐/磁盘，M3 对照）；
  6. 正确性证据（COUNTS_MATCH 三路、电池 11× "No problems"、v1 落档两个测试名、fmt）；
  7. 附：全部命令与产物文件清单。

- [ ] **Step 5.10: 收尾验收**

  ```
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ cargo test -p codec-lucene9 2>&1 | tail -2
  test result: ok. 162 passed; 0 failed; 1 ignored; ...
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. 45 passed; 0 failed; ...
  $ git status --short   # 仅任务开始前就存在的未跟踪文件；.superpowers/ ignored（!!）
  $ git log --oneline -6  # T1-T4 四个 feat: commit（+ 可选 bench: SKEW_RATIO 标定）；报告不进 git
  ```

---
