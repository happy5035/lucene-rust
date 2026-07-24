# M5 croaring 引擎替换（Frozen view 零拷贝 + C 优化算子 / 格式 v3 / 删除自研容器库）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 M3（inline bitmap）/ M4（读侧去税）之上实施 M5 引擎整体替换，对应已批准 spec `docs/superpowers/specs/2026-07-24-rust-search-m5-croaring-engine-design.md` 的全部范围（§6 任务切分，共 5 个任务）：① **依赖引入 + 格式 v3 写侧**（`croaring = "2.7"` 仅入 codec-lucene9；`Bitmap::of` → `run_optimize` → `shrink_to_fit` → Frozen 序列化为 payload；`BITMAP_VERSION = 3`；len 上界按 croaring-sys 4.7.1 frozen 布局重推导；`version != 3` → `Ok(None)` 静默落档，v2 doctored-bytes 落档测试）；② **读侧 Frozen view 打开**（区域一次顺序读入 32B 对齐 buffer + 单一 unsafe 调用点封装进新模块 `roaring/frozen.rs`——crate 第三个模块级 `#[allow(unsafe_code)]`——+ 安全预校验复刻 C 侧结构检查 + 对 core 暴露 open/contains/docs_from/cardinality 安全 API）+ Term 接入（迭代 = 批量 refill 游标，count 保持 doc_freq 直读）；③ **AND/OR 引擎**（count = `and_cardinality`/`or_cardinality` fold，微秒级；全非低 df 子句迭代 = croaring 物化 and/or + 结果迭代；档 2 物化低 df 子句 + `view.contains` 过滤不变；`SKEW_RATIO` 按 croaring contains 成本重标定）；④ **删除自研容器库**（`roaring.rs` 的 Container/RoaringBitmap/RoaringCursor/serialize/deserialize/from_sorted_docs/and/or/optimize + `roaring/simd.rs` + `roaring/view.rs` + 全仓引用清零，`cargo check --workspace` 含 jni-binding 干净）；⑤ **电池 + bench 复测**（五变体全绿、on/off A/B、v2 落档、M3/M4 同口径三路 `--no-cache`、SKEW_RATIO 标定、写侧 `Bitmap::of` 开销量化，报告落 `.superpowers/sdd/m5-bench-report.md`，gitignored）。保留不动：.doc 内联外壳（magic/version/df/cardinality 头 + len 尾缀 + `locate_bitmap_region` 定位 + `BITMAP_MIN_DF` 门槛）、档判定骨架、电池/Makefile。

**Architecture:** codec 层 `roaring.rs` 瘦身为纯格式壳（`BITMAP_MAGIC`/`BITMAP_VERSION = 3`/`BITMAP_MIN_DF`/`max_bitmap_len` 新上界/`write_term_bitmap` croaring 构建 + Frozen payload/`parse_region` 头解析），新增 `crates/codec-lucene9/src/roaring/frozen.rs`：`FrozenBitmap`（拥有 32B 对齐 buffer 的 region payload 副本；唯一 unsafe fn `view()` 按需重建 `BitmapView::deserialize::<Frozen>` ~60ns，批摊销，无自引用生命周期、无第二处 unsafe）、安全结构预校验（复刻 `roaring_bitmap_frozen_view` 的 cookie/typecode/精确长度检查，croaring-sys 4.7.1 `roaring.c:18153-18203`——C 侧 NULL 返回在 Rust 包装里是 assert/panic，必须打前站）、`contains`/`cardinality`/`and_cardinality`/`or_cardinality`/`docs_from`（批量 refill）安全 API；T3 在同模块加 `intersect_docs`/`union_docs`/`and_cardinality`/`or_cardinality` 四个 fold（croaring 类型全封闭在 codec——core 无 croaring 依赖，只接 `Vec<u32>`/`u64`/`bool`）。`postings_read.rs` 的 `open_term_bitmap` 改为 locate → 一次顺序读 region → `parse_region`（open/probe 两 fn 合一：frozen view 要求完整精确长度 buffer，contains 是直接内存测位，M4 的 probe 模式失去存在意义，关键设计事实 7）；`probe_term_bitmap` 删除。core 层 `SegmentReader` 同名包装（`bitmap_enabled()` 门不变）；`doc_iter.rs` 新增 `BitmapCursor`（512-doc 批量 refill + `reset_at_or_after` seek），`DocSource::Bitmap` 取代 `DocSource::View`，`RoaringDocIter`/`RoaringAndDocIter`/`RoaringOrDocIter` 换引擎（M4 合并算法骨架保留，probes 的 contains 变为非 io 直接内存测位）；`roaring_exec.rs` 三档骨架不变：档 1 非偏斜 = 物化 fold，档 1 偏斜 = 最小侧迭代 + contains probes（SKEW_RATIO 重标定），档 2 = 物化 slice + contains 过滤，count 全 bitmap 时走 cardinality fold 快路径；`query.rs`/`searcher.rs`/`multi_term.rs` 零行为改动（`for_each_doc` 档 2 物化不变）。

**Tech Stack:** Rust（`crates/codec-lucene9` edition 2024、`crates/core` edition 2021；codec `#![deny(unsafe_code)]`，T1–T3 期间恰好三个模块级 `#[allow(unsafe_code)]`：`postings_ll/simd.rs` + `roaring/simd.rs` + 新 `roaring/frozen.rs`，T4 删 `roaring/simd.rs` 后恢复两个；core `#![forbid(unsafe_code)]` 且只调 codec 安全包装，永不出现 croaring 类型；统一 `io::Result`）；**新增依赖 `croaring = "2.7"` 仅入 `crates/codec-lucene9/Cargo.toml`**（spec 标题行用户认账的依赖政策变更："用户提出'直接考虑使用 croaring'……依赖政策变更——引入 croaring crate——经用户认账"；探针实测 2.7.0 + croaring-sys 4.7.1 内嵌 CRoaring 4.7.1 C 源码编译、无系统库依赖、release +11s、musl target 可编可跑，`/tmp/croaring-probe/REPORT.md` §Versions）；core/jni-binding 不新增任何依赖；Java 9.12.3 电池/bench 工具链不变。

## Global Constraints

（摘自 spec 与既有项目惯例，逐字或就近转述；所有 Task 共同遵守）

- **postings 主格式字节不动**。引擎替换只改 bitmap region 内部（term 间缝隙字节）；`.tim/.tip/.tmd/.pos/.psm` 及 FST output schema 零改动。不写 `--bitmap` 的索引与 M2–M4 字节级一致（`--bitmap` 默认 off）。
- **v3 布局逐字（spec §3）**：
  ```
  [ magic(4B "RLBM") + version(1B) = 3 + df(vInt) + cardinality(vInt) + Frozen payload ][ len: u32 LE ]
  ```
  payload = CRoaring Frozen 格式（布局推导见关键设计事实 1）；len = 头+payload 字节数；len 上界公式：**`max_bitmap_len(max_doc) = 19 + ⌈max_doc/65536⌉ × 8197`**。写侧不对齐（.doc 流内偏移任意），对齐由读侧对齐 buffer 解决（spec §3）；写侧仍经同一 ChecksumIndexOutput（footer CRC 覆盖）。
- **版本即迁移**：`version != 3` → `Ok(None)` 静默落档 postings（v1/v2 索引零迁移、零特判，查询永不报错）。兜底校验三重（全部廉价）：len 有界 → magic/version → 头内 df/cardinality == `termState.doc_freq`（+ 打开后 cardinality 复核，关键设计事实 3）；.doc footer CRC 仍是 Lucene 级完整性兜底。
- **依赖政策**：新增依赖只有 `croaring = "2.7"`，只进 `crates/codec-lucene9/Cargo.toml`（政策变更依据见 Tech Stack 的 spec 引用）；workspace 根 `Cargo.toml`、core、jni-binding 的依赖表零改动。
- **unsafe 政策**：core 保持 `#![forbid(unsafe_code)]`；codec 恰好新增一个模块级 `#[allow(unsafe_code)]`（`roaring/frozen.rs`，crate 第三个；T4 删 `roaring/simd.rs` 后回到两个）；模块内唯一 unsafe 调用点是 `FrozenBitmap::view` 的 `BitmapView::deserialize::<Frozen>`，32B 对齐 + 精确长度契约由构造保证、C 侧结构拒绝路径由安全预校验封堵（关键设计事实 2/3）。
- **ConstantScore/needs_freq 禁入不变**：bitmap 无 freq/positions；`needs_freq == true` 一律档 3（query.rs 既有门不动）；roaring 路径 `freq()` 恒为 1。
- **`RL_BITMAP=0`** kill switch 保留：`SegmentReader::open_term_bitmap` 内联同一 `bitmap_enabled()` 门（segment_reader.rs:127-130），on/off A/B 同二进制同索引。
- **YAGNI（spec §2 明确不做）**：multi-term roaring 集成（仍二期）；跨查询 bitmap 缓存；Java portable 格式转换（payload 是 CRoaring 私有 frozen 格式，Rust 私有无碍）；NRT；除 v2→v3 外零格式演进。
- **测试命令**：codec 层 `cargo test -p codec-lucene9 <test名>`，core 层 `cargo test -p rustlucene-core <test名>`；收尾门槛 `make log-test`（200000 文档五变体：seed 42 默认 / 43 `--positions` / 44 `--sparse` / 45 `--bigdict` / 46 `--bitmap`，Makefile:17-22，本计划零改动），较慢，只在 T5 使用；T1–T4 的增量用 `cargo test` 覆盖。
- **Lucene/CRoaring 引用纪律**：关键决策在代码注释中给 `File.java:line` 或 `roaring.c:line` 引用（Java 源码前缀 `reference/lucene-9.12.3/lucene/core/src/java/org/apache/lucene/` 省略；CRoaring 源码在 `~/.cargo/registry/src/*/croaring-sys-4.7.1/CRoaring/`，引用给文件+行号）。找不到精确行号时引用函数名，不编行号。
- **commit message 前缀**：`feat:` / `fix:` / `docs:` / `bench:` / `test:`（沿用 git log 现有风格）。
- **实验/报告文件**：bench 数据与报告写到 `.superpowers/sdd/`（M2 起已 gitignored），不进 git；skew 标定用 scratch crate `/tmp/m5skew`（path-dep 只读引用两 crate，repo 零改动——同 M3 `/tmp/roarbench`、M4 `/tmp/m4skew` 先例）。
- **验收门槛**：`cargo fmt --check` 干净；`cargo test` 两 crate 全绿；`cargo check --workspace`（含 jni-binding）干净；`make log-test` 五变体全绿且每次 CheckIndex 输出 "No problems"；`--bitmap` 变体内 Rust bitmap on/off searchdump diff 为空；v2 落档 Rust 测试（codec + core 两个）绿；T5 bench 的 per-query hit-counts 按 M3/M4 口径三路对拍通过；spec §5 性能预期逐条对照（稀疏 AND ≥2x PFOR、稠密 ≥8x、term count ~1.0x、iterm ≥1x；写侧 `Bitmap::of` 开销量化）。

## Pre-checks（已执行，基线绿；HEAD 2ec020e）

```
$ cargo build
    Finished `dev` profile [unoptimized + debuginfo] target(s)
$ cargo test -p codec-lucene9
test result: ok. 162 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
$ cargo test -p rustlucene-core
test result: ok. 45 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
$ cargo fmt --check && echo FMT_OK
FMT_OK
```

## 关键设计事实（本计划全部代码的字节级/API 级依据，已逐项对照 croaring-sys 4.7.1、croaring 2.7.0 与本仓库源码核实）

1. **Frozen 布局与 len 上界推导**（spec §3 "len 上界公式按计划阶段对 croaring-sys 源码推导的值"；校验①"len 有界"的精确公式）：frozen payload = 逐 container 数据区（bitset: 1024×u64 = 8192B；run: n_runs×rle16_t = 4B/run；array: card×u16 = 2B/值）+ 每 container 5B（keys 2B + counts 2B + typecodes 1B）+ 尾部 4B header（`roaring.c:17986-18013` 格式注释 + `roaring_bitmap_frozen_size_in_bytes` `roaring.c:18017-18039` 逐项核对）。写侧恒走 `run_optimize`：run container 只在严格小于原表示时转换（`convert_run_optimize` `roaring.c:9915-9970`——array 原大小 ≤ 4096×2 = 8192B（DEFAULT_MAX_SIZE = 4096）、bitset 原大小 = 8192B ⇒ 转换后 run 数据区 < 8192B）；array ≤ 8192B、bitset = 8192B ⇒ **每 container 数据区 ≤ 8192B**。container 数 ≤ ⌈max_doc/65536⌉ ⇒ frozen ≤ `4 + ⌈max_doc/65536⌉ × (8192 + 5)`。我方头 ≤ magic 4 + version 1 + df/card vInt 各 ≤5 = 15B ⇒ **`max_bitmap_len = 19 + ⌈max_doc/65536⌉ × 8197`**（maxDoc=200000 时 32807B；maxDoc=1 时 8216B）。该上界只依赖 maxDoc 与 container 尺寸上限，与 df、threshold 无关。
2. **Frozen view 契约**（`croaring-2.7.0/src/bitmap/serialization.rs` `impl ViewDeserializer for Frozen`）：`unsafe { BitmapView::deserialize::<Frozen>(data) }` → `roaring_bitmap_frozen_view`；要求 ① data 起点 32B 对齐（`Frozen::REQUIRED_ALIGNMENT = 32`，wrapper 内 `assert_eq!(ptr % 32, 0)`）② `data.len()` 精确等于 frozen 尺寸 ③ data 是合法 frozen 输出。读侧用法（探针实测 ~60ns 创建、借用 buffer 后 and/or/contains/iter 全可用，`REPORT.md` §API surface）：region 一次 memcpy 进 `len + 31` 字节的 Vec，`align_offset(32)` 取对齐切片——本计划唯一 unsafe 调用点。
3. **C 侧 NULL = Rust 侧 panic，必须安全预校验**：`roaring_bitmap_frozen_view` 对非法输入返回 NULL（`roaring.c:18153-18203`：cookie 检查 → num_containers → typecodes ∈ {BITSET=1, ARRAY=2, RUN=3}（`roaring.h:5237-5239`）→ 按 counts 推精确长度（array/bitset counts = card−1、run counts = n_runs，`roaring.c:17993-17998`）→ `length != zones + 5n + 4` 拒），而 Rust 包装 `BitmapView::take_heap` 对 NULL `assert!`（`croaring-2.7.0/src/bitmap/view.rs:34`）——非法字节会 panic 而非落档。故 `FrozenBitmap::open` 先用纯安全代码逐项复刻上述检查（cookie = `FROZEN_COOKIE = 13766`，`roaring.h:7935`，尾部 4B header 低 15 位；高 17 位 = num_containers），预校验通过才允许 unsafe 打开；打开后再复核 `view.cardinality() == df`（校验③）。doctored/corrupt 字节因此全部走 `Ok(None)` 落档，零 panic 面。
4. **`Bitmap::serialize::<Frozen>()` 不存在**：`serialize<S: Serializer + NoAlign>`（`imp.rs:871`），Frozen 有对齐要求不是 NoAlign ⇒ 写侧必须 `serialize_into_vec::<Frozen>(&mut vec)`（`imp.rs:903`；crate 在 Vec 内切 32B 对齐切片返回，探针 `main.rs:60-77` 模式）。写侧拿到切片后直接写进 region（对齐对写侧无意义，spec §3）。
5. **view 创建 ~60ns → 批量 refill 游标**：`FrozenBitmap` 拥有 buffer，`BitmapView`/`BitmapIterator` 都借用它——若存 view/iterator 即成自引用结构（需要第二处 unsafe 或新依赖，均不允许）。决议：不存 view，每次操作经私有 `fn view()` 重建（~60ns）；迭代经 `docs_from(from, dst)` 批量出口（`BitmapIterator::reset_at_or_after` = `roaring_uint32_iterator_move_equalorlarger` 容器级 seek + `next_many` bulk 读，`croaring-2.7.0/src/bitmap/iter.rs` 两同名方法），core 侧 `BitmapCursor` 以 512-doc 批摊销到 <1ns/doc（对照：frozen 全量迭代 4.6–7.3 ns/doc，REPORT §Bench）。零自引用、零额外 unsafe。
6. **contains 成本与 probe 模式终结**：frozen view 的 `contains` 8.5–28.7 ns/次（REPORT §Bench：sparse 28.7 / dense 8.5 / run 10.4）且是直接内存测位；frozen 契约要求完整精确长度 buffer ⇒ M4 "大侧不做全量读取"的 probe 模式（目录扫描 + 按 probe 定点读桶）在 frozen 架构下不存在对应物——读侧只剩"区域一次顺序读"一种打开方式（spec §2 读侧条目逐字）。`open_term_bitmap`/`probe_term_bitmap` 合一为 `open_term_bitmap`。
7. **set ops 物化 + core 无 croaring 依赖**：view 上的 `and`/`or` 恒物化新 owned Bitmap（REPORT §API surface "Gaps"）；`and_cardinality`/`or_cardinality` 是仅有的非物化集合算子（k=2）。core 不依赖 croaring（Global Constraints）⇒ 一切 `Bitmap`/`BitmapView` 类型封闭在 codec，对 core 暴露 `Vec<u32>`（物化 fold 结果）/`u64`（cardinality）/`bool`（contains）。k>2 的 cardinality fold 物化中间结果（C API 只有二元算子）。
8. **AND/OR 迭代策略决议**（spec §2 "物化 + 结果迭代，或多视图 merge-iterate——bench 定夺"的计划期定夺）：**选物化 fold**。依据（全部实测）：croaring 物化 `and`+card sparse 10.2µs / ultra 1.4µs（REPORT §Bench）vs M4 字节游标 merge-intersect 实测 50.5µs/op（r=1，`.superpowers/sdd/m4-skew-micro.txt`）；`and_cardinality` 快自研 25–65x；dense 物化 157µs 虽贵，但 dense 结果迭代本身 ~200k×7ns ≈ 1.4ms，物化占比 ~10%。偏斜 AND 保留"最小侧迭代 + 其余 contains probes"形态（SKEW_RATIO 门，T5 用 croaring 成本重标定——两条路径都付了双侧 region 读，交叉点只能实测）。
9. **写侧 hook 与读侧定位零改动**：`postings.rs:368-372`（`write_term` 中 `doc_start_fp` 捕获之前，`crate::roaring::write_term_bitmap(&mut self.doc_out, docs)?`）一行不动；`locate_bitmap_region`（postings_read.rs:146-167：df ≥ `BITMAP_MIN_DF`(4096) 门 → `fp >= 4` → len 有界 → `fp-4 >= len`）一行不动，len 上界走 `max_bitmap_len` 新公式自动生效；`fresh_input` 模式（postings_read.rs:135-138）不变。
10. **T1 读侧钉死（防 misparse 的关键排序）**：T1 把 `BITMAP_VERSION` 改为 3 后，若不动读侧，v3 字节会通过 M4 view 的版本检查再被 v2 容器目录解析器误读——版本门先于一切 payload 解析，但"版本相等"后即入解析。故 T1 将 `open_term_bitmap`/`probe_term_bitmap` 钉为 `Ok(None)`（全部落档），T2 重写为 frozen 打开；T1 的 doctored-bytes 落档测试因此确定性通过，T2 同一测试继续通过（此时走真实版本门）。自研 `serialize`/`deserialize` 钉私有常量 `SELF_BUILT_WIRE_VERSION = 2`（仅供自有 round-trip/拒绝测试，T4 整体删除），与写侧 v3 解耦。
11. **count 语义**：spec §2 "AND/OR count = `and_cardinality`/`or_cardinality`（微秒级，不再需要逐 doc 驱动）"——全 bitmap 子句时走 cardinality fold（k=2 非物化，k>2 物化中间结果，事实 7）；档 2 混合（有 slice）时驱动同一迭代器计数（候选 ≤4095×contains 8.5–28.7ns，有界；count == 迭代结果数由构造保证，同 M4 事实 10）。Term count 保持 M4 的 `doc_freq` 直读（searcher.rs:63-72，不动）。
12. **档判定 / needs_freq / RL_BITMAP 门不变**：`collect_bool_entries`（roaring_exec.rs:38-63）df 升序排序不变；`and_segment_iterator`/`or_segment_iterator`（query.rs:206-269）的 `if !needs_freq` 门不动；count 的 And/Or 分支（searcher.rs:94-120）不动，只换 `roaring_exec::count` 内部实现。
13. **SegmentDocIter 尺寸纪律变化**：M4 事实 6 的 8KB IndexInput 内联缓冲顾虑随 probe 模式消失——`FrozenBitmap` = `Vec<u8>` + `usize`（≈32B），region 字节在打开时一次性进 Vec，迭代器不再持定位流；`RoaringDocIter`/`DocSource`/`RoaringAndDocIter` 不再需要 `Box<RoaringView>`。`BitmapCursor.buf` 用 `Vec<u32>`（堆分配 2KB），不内联进枚举。
14. **电池语料事实（M3 T6/M4 核，仍成立）**：200000 文档 log 语料 level 五 term df≈40000（≥4096 有 bitmap）、message df≈2700（无 bitmap）、trace_id df=1 → 电池覆盖 Term bitmap / 档 1 / 档 3 / 无 bitmap 自然落档 / on-off A/B / Java forceMerge；档 2 与偏斜 probe 由单测覆盖（电池组不出混合子句，脚本零改动——v2 落档覆盖同样是 Rust 测试，不加电池变体）。
15. **bench 口径 = M4 T5 逐字**（`.superpowers/sdd/m4-bench-report.md` §1 已核）：1M docs seed 42、Java 侧 `--no-cache`、`--warmup 10 --iter 30`、三路串行、查询文件经 Java 侧 `SearchBench <java索引> message --dump-queries <out> --tasks 50 --seed 42` 产出（498 行）+ `awk -F'\t' '!($1=="TERM" && $4<4096)'` 守卫（493 行）；hit-counts 走 stderr；Java 侧等效对拍（非 term= 行 sorted diff 为空 + term= 重合行 awk join mismatches=0）。df 全景：message high df 10016–19206、med df 4096–9898、level df≈200000。**SKEW_RATIO 标定输入 = `/tmp/croaring-probe/REPORT.md`**（事实 6/8 的数字）。
16. **T4 删除集合（已 grep 核实）**：`roaring.rs` 的 Container/RoaringBitmap/RoaringCursor/from_sorted_docs/serialize/deserialize/and/or/optimize/clone_container 及全部容器辅助 fn（set_range/bit/next_set_bit/bitset_card/array_to_runs 等）+ `mod simd; pub mod view;` + `pub use view::{RoaringView, ViewCursor};` + 9 个容器测试（`build_chooses_container_types`/`iteration_round_trip_mixed_containers`/`cursor_advance_matches_linear_scan`/`and_or_match_reference_sets`/`v2_serialize_has_no_crc_and_rejects_v1`/`serialize_deserialize_round_trip`/`run_container_advance_drops_stale_offset`/`cursor_next_advance_interleaved_matches_reference`/`deserialize_rejects_structural_violations`，含 `serialize_v1_for_test`）；`roaring/simd.rs`（174 行，1 测试）、`roaring/view.rs`（575 行，4 测试）整文件；引用点：postings_read.rs:16 import、lib.rs:36 `pub use roaring::RoaringBitmap;`、core doc_iter.rs:9 / roaring_exec.rs:15 / segment_reader.rs:10 import（T2/T3 已切走，T4 时仅剩 codec 内部）。保留：`BITMAP_MAGIC`/`BITMAP_VERSION`/`BITMAP_MIN_DF`/`max_bitmap_len`/`write_term_bitmap`/`parse_region` + `max_bitmap_len_bound_holds`、`write_term_bitmap_appends_len_suffix` 两测试 + `mod frozen` 全部。删除后 `grep -rn "RoaringView\|ViewCursor\|RoaringBitmap\|RoaringCursor\|Container" crates/core/src crates/jni-binding/src` 为空。
17. **jni-binding 零耦合**：`grep -rn "roaring\|Roaring" crates/jni-binding/src` 为空——T4 的 `cargo check --workspace` 只需确认编译干净，无符号迁移。

---
## Task 1: 依赖引入 + 格式 v3 写侧（croaring 构建 + Frozen payload + len 上界重推导 + v2 落档测试 + 读侧钉死）

**Files:**
- Modify: `crates/codec-lucene9/Cargo.toml`（新增 `croaring = "2.7"`）
- Modify: `crates/codec-lucene9/src/roaring.rs`（模块文档、`BITMAP_VERSION = 3`、`SELF_BUILT_WIRE_VERSION` 钉版、`max_bitmap_len` 新上界、`write_term_bitmap` croaring 构建 + 测试改造）
- Modify: `crates/codec-lucene9/src/postings_read.rs`（`open_term_bitmap`/`probe_term_bitmap` 钉死落档 + `inline_bitmap_region_round_trip` 改造 + 落档测试改造 + 删 `open_and_probe_term_bitmap_match_postings`）
- Modify: `crates/core/src/search/mod.rs`（`bitmap_v1_index_falls_back_to_postings` → v2 版 + 三个路径断言测试 T1 期翻转为落档期望）
- Test: 上述三文件的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: `croaring::{Bitmap, Frozen}`（`Bitmap::of(&[u32])`、`run_optimize()`、`shrink_to_fit()`、`serialize_into_vec::<Frozen>(&mut Vec<u8>) -> &mut [u8]`、`cardinality() -> u64`——全部与探针 `main.rs:60-77` 工作代码逐字一致）；`crate::io::{DataInput, DataOutput, IndexInput, IndexOutput}`。
- Produces（T2 依赖这些名字与契约，不得改名）:
  ```rust
  // roaring.rs
  pub const BITMAP_MAGIC: [u8; 4] = *b"RLBM";        // 不变
  pub const BITMAP_VERSION: u8 = 3;                  // 2 → 3
  pub const BITMAP_MIN_DF: u32 = 4096;               // 不变
  pub fn max_bitmap_len(max_doc: u32) -> u64;        // 20 + n×8201 → 19 + n×8197
  pub fn write_term_bitmap(out: &mut impl DataOutput, docs: &[u32]) -> io::Result<()>; // 签名不变，body 换 croaring
  // payload 契约（T2 的 aligned-buffer helper 消费）：region = 头（magic+version+df vInt+card vInt）
  //   + Frozen payload（serialize_into_vec 切片，长度 == roaring_bitmap_frozen_size_in_bytes，
  //   尾部 4B = header（低 15 位 FROZEN_COOKIE=13766 | 高 17 位 num_containers））
  // T2 将实现的 aligned-buffer helper（此处冻结签名；放 T2 是因为 T1 无消费者，
  // 提前落地只能是死代码）：
  //   roaring/frozen.rs: pub struct FrozenBitmap { buf: Vec<u8>, off: usize }
  //   impl FrozenBitmap { pub fn open(payload: &[u8], expected_df: u32) -> Option<FrozenBitmap>; }
  // postings_read.rs（T1 钉死，T2 重写返回类型）
  pub fn open_term_bitmap(&self, entry: &TermEntry, max_doc: u32) -> io::Result<Option<RoaringView>>; // T1: 恒 Ok(None)
  pub fn probe_term_bitmap(&self, entry: &TermEntry, max_doc: u32) -> io::Result<Option<RoaringView>>; // T1: 恒 Ok(None)；T2 删除
  ```

  语义决定：v3 写侧 = `Bitmap::of(docs)` → `run_optimize()` → `shrink_to_fit()` → `serialize_into_vec::<Frozen>`；header 的 df 与 cardinality 都写 `docs.len()`（docs-only bitmap，cardinality == df 恒成立）。len 尾缀语义不变（len = region 字节数）。版本门是唯一迁移机制：v3 读侧（T2）见 `version != 3` → None；T1 期间读侧钉死，v3 索引也落档（battery/等价测试不受影响——on/off 两边结果一致）。

### Steps

- [ ] **Step 1.1: 写 v3 期望测试 + 落档/翻转改造（先失败）**

  ① `crates/codec-lucene9/src/roaring.rs` 测试模块：`write_term_bitmap_appends_len_suffix`（roaring.rs:1208）整体替换为 v3 布局断言（保留测试名）：

  ```rust
      /// v3 layout (M5 §3): [magic + version=3 + df + card + Frozen payload]
      /// [len u32 LE]; payload tail = frozen header (FROZEN_COOKIE 低 15 位).
      #[test]
      fn write_term_bitmap_appends_len_suffix() {
          let docs: Vec<u32> = (0..5000u32).collect();
          let mut out = IndexOutput::in_memory();
          write_term_bitmap(&mut out, &docs).unwrap();
          let bytes = out.into_bytes();
          let len = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap()) as usize;
          assert_eq!(len + 4, bytes.len());
          let region = &bytes[..len];
          assert_eq!(&region[..4], b"RLBM");
          assert_eq!(region[4], 3, "format v3");
          // df + card 两个 vInt（5000 = 0x88 0x27 两字节编码）
          let mut input = IndexInput::in_memory(region[5..].to_vec());
          assert_eq!(input.read_vint().unwrap(), 5000);
          assert_eq!(input.read_vint().unwrap(), 5000);
          // frozen payload 尾部 4B header：低 15 位 == FROZEN_COOKIE（roaring.h:7935）
          let header = u32::from_le_bytes(region[region.len() - 4..].try_into().unwrap());
          assert_eq!(header & 0x7FFF, 13766, "FROZEN_COOKIE");
          assert!((header >> 15) >= 1, "num_containers");
          assert!(len as u64 <= max_bitmap_len(5000));
      }
  ```

  ② 同文件 `max_bitmap_len_bound_holds`（roaring.rs:1182）的两个期望值改为：

  ```rust
          assert_eq!(max_bitmap_len(200_000), 19 + 4 * 8197);
          assert_eq!(max_bitmap_len(1), 19 + 8197);
  ```

  （该测试同时断言"真实 bitmap 的 serialize 长度 ≤ 上界"的后半段用自研 `serialize`——v3 下该断言对象改为 `write_term_bitmap` 的 region 长度；把后半段改为：对 `shaped_docs(&mut Rng(3), &[(0, 4096), (1, 5000), (2, 6000), (3, 100)])` 调 `write_term_bitmap` 进 `IndexOutput::in_memory()`，断言 `len ≤ max_bitmap_len(200_000)`。）

  ③ `crates/codec-lucene9/src/postings_read.rs` 测试模块：`read_term_bitmap_falls_back_on_v1_layout`（postings_read.rs:1559）整体替换为：

  ```rust
      /// M5 §3 版本即迁移：v3 写侧产出的 region doctor 回 v2/v1 版本字节后，
      /// 读侧必须静默落档且 postings 逐 doc 不变（T1 期间读侧钉死落档——
      /// 本测试在 T1 确定性通过，T2 起走真实版本门，断言一字不改）。
      #[test]
      fn open_term_bitmap_falls_back_on_legacy_version() {
          let root = temp_dir("bitmap-legacy");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_segment_bitmap(&dir);
          let e = seek(&dir, &fis, "tx", b"hot");
          let fp = e.state.doc_start_fp;
          let doc_file = root.join(crate::postings::file_name("_0", "doc"));
          let orig = fs::read(&doc_file).unwrap();
          let len =
              u32::from_le_bytes(orig[(fp - 4) as usize..fp as usize].try_into().unwrap()) as u64;
          let region_start = (fp - 4 - len) as usize;
          assert_eq!(&orig[region_start..region_start + 4], b"RLBM");
          assert_eq!(orig[region_start + 4], 3, "write side must emit v3");
          for legacy in [2u8, 1] {
              let mut bytes = orig.clone();
              bytes[region_start + 4] = legacy;
              fs::write(&doc_file, &bytes).unwrap();
              let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
              assert!(
                  postings.open_term_bitmap(&e, 6000).unwrap().is_none(),
                  "v{legacy} region must fall back"
              );
              // fallback correctness: the postings themselves are untouched
              let mut en = postings.docs_and_freqs(&e).unwrap();
              for expected in 0..5000 {
                  assert_eq!(en.next_doc().unwrap(), expected);
                  assert_eq!(en.freq(), 1);
              }
              assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
          }
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  ④ 同文件删除 `open_and_probe_term_bitmap_match_postings`（postings_read.rs:1499-1557，整测删除——T2 以 `open_term_bitmap_matches_postings` 重新落地同覆盖）；`inline_bitmap_region_round_trip`（postings_read.rs:1435）的自研 round-trip 段替换为 v3 头 + cookie 断言（region 头 magic/version==3/df/card、尾部 FROZEN_COOKIE、`len ≤ max_bitmap_len`——与 ① 同型，直接复用其断言序列；`raw_bitmap_region` helper 保留）。

  ⑤ `crates/core/src/search/mod.rs` 测试模块：`bitmap_v1_index_falls_back_to_postings`（mod.rs:768）重命名 `bitmap_v2_index_falls_back_to_postings`，doctor 调用改 `doctor_bitmap_version(&root_on, "message", b"hot", 2);`，doctor 前断言 `bytes[start + 4] == 3`（在 doctor helper 返回前加一行 debug 断言不必——在测试内 doctor 前读文件断言，或简单依赖 codec 侧 ③ 的同款断言；选择：测试内不做重复字节断言，注释引用 ③），文档注释改为 "M5 §3 v2 落档：bitmap 索引 doctor 回 v2（version 字节）后读侧静默落 postings，结果与 bitmap-off 索引逐位一致"。

  ⑥ 同文件三个路径断言测试 T1 期翻转（读侧钉死 ⇒ 全部落档；每处加 `// M5 T1: 读侧钉死落档，T2 恢复 roaring 断言` 注释）：
  - `term_query_uses_roaring_when_bitmap_present`（mod.rs:684）：`matches!(it, SegmentDocIter::Roaring(_))` 断言翻转为 `!matches!(...)`（off 索引的两条不变）；
  - `bool_query_three_tier_roaring`（mod.rs:839）：档 1/档 2 四条 RoaringAnd/RoaringOr 断言翻转为 `SegmentDocIter::And(_)`/`Or(_)` 期望（档 3、needs_freq、缺失子句、on/off 等价、数值锚点段全部不变——落档后等价性仍成立）；
  - `and_skew_probe_matches_pfor`（mod.rs:974）与 `bool_query_roaring_multi_segment`（mod.rs:1028）的 roaring 路径断言同样翻转（等价电池与锚点不变）。

- [ ] **Step 1.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 roaring 2>&1 | tail -6
  failures: roaring::tests::write_term_bitmap_appends_len_suffix
            roaring::tests::max_bitmap_len_bound_holds
  $ cargo test -p codec-lucene9 open_term_bitmap_falls_back_on_legacy_version 2>&1 | tail -3
  FAILED（写侧仍发 v2：orig[region_start + 4] == 2 断言失败）
  $ cargo test -p rustlucene-core bitmap_v2_index_falls_back_to_postings 2>&1 | tail -3
  FAILED（doctor 到 2 的 region 仍被 v2 读侧接受 → 路径断言仍见 Roaring 变体）
  ```

- [ ] **Step 1.3: 依赖 + roaring.rs 格式 v3 实现** — 六处编辑：

  ① `crates/codec-lucene9/Cargo.toml` `[dependencies]` 追加：

  ```toml
  croaring = "2.7"
  ```

  （解析结果应为 croaring 2.7.0 + croaring-sys 4.7.1——探针锁定组合；`cargo build` 首次 +~11s C 编译，零警告。）

  ② 模块文档注释（roaring.rs:1-11）改为：

  ```rust
  //! Inline per-term bitmap in the .doc stream, format v3 (M5 spec §3):
  //! `[ magic "RLBM" + version=3 + df(vInt) + cardinality(vInt) + Frozen
  //! payload ][ len: u32 LE ]`, written ahead of a term's postings. The
  //! engine is croaring (CRoaring 4.7.1 via croaring-sys): the write side
  //! builds `Bitmap::of` → `run_optimize` → `shrink_to_fit` → Frozen
  //! serialize; the read side opens zero-copy frozen views over a 32B-aligned
  //! buffer copy (`roaring/frozen.rs`, the crate's third module-level
  //! `#[allow(unsafe_code)]`). The self-built container library below
  //! (Container/RoaringBitmap/RoaringCursor, wire v2 pinned at
  //! SELF_BUILT_WIRE_VERSION) is transitional — it only serves its own
  //! tests until T4 deletes it together with roaring/simd.rs and
  //! roaring/view.rs (M5 §2 删除, 关键设计事实 16).
  ```

  ③ `BITMAP_VERSION`（roaring.rs:719-722）改为：

  ```rust
  /// Wire format version. v3 (M5 §3): payload = CRoaring Frozen format
  /// (engine replaced by croaring; the self-built container payload is
  /// gone). The version byte is the only migration gate: != 3 (v1/v2
  /// included) silently falls back to postings.
  pub const BITMAP_VERSION: u8 = 3;

  /// Wire version of the transitional self-built container format (M4 v2),
  /// used only by `serialize`/`deserialize` and their own tests until T4
  /// deletes them (关键设计事实 10). Never written to an index by M5.
  const SELF_BUILT_WIRE_VERSION: u8 = 2;
  ```

  ④ `serialize`/`deserialize` 钉版：`serialize` 的 `out.write_byte(BITMAP_VERSION).unwrap();`（roaring.rs:746）改为 `out.write_byte(SELF_BUILT_WIRE_VERSION).unwrap();`；`deserialize` 的 `!= BITMAP_VERSION`（roaring.rs:798）改为 `!= SELF_BUILT_WIRE_VERSION`。二者文档注释各加一行 "T4 删除；与索引写侧 v3 解耦（SELF_BUILT_WIRE_VERSION）"。（`v2_serialize_has_no_crc_and_rejects_v1`、`serialize_deserialize_round_trip`、`deserialize_rejects_structural_violations` 三测试因此零改动保绿。）

  ⑤ `max_bitmap_len`（roaring.rs:728-735）改为：

  ```rust
  /// Upper bound of the bitmap region length (header+payload) for a segment
  /// with `max_doc` docs. Derivation (spec §3 len 有界校验; 关键设计事实 1):
  /// CRoaring frozen payload = 每 container 数据区 ≤ 8192B（array ≤ 4096×2、
  /// bitset = 1024×8、run 只在严格更小时转换 ⇒ < 8192B，convert_run_optimize
  /// roaring.c:9915-9970）+ 每 container 5B（keys 2 + counts 2 + typecodes 1）
  /// + 4B header（frozen_size_in_bytes roaring.c:18017-18039）；
  /// container 数 ≤ ceil(maxDoc/65536)；我方头 ≤ 4+1+5+5 = 15B：
  ///   max_bitmap_len = 19 + ceil(max_doc / 65536) * 8197
  pub fn max_bitmap_len(max_doc: u32) -> u64 {
      19 + (max_doc as u64).div_ceil(65536) * 8197
  }
  ```

  ⑥ `write_term_bitmap`（roaring.rs:907-918）body 改为（签名/调用点零改动）：

  ```rust
  /// Builds the bitmap for one term and writes `[region][len: u32 LE]` into
  /// the .doc stream, immediately before the term's postings (M5 §3). Engine:
  /// croaring — `Bitmap::of` (bulk append, sorted input) → `run_optimize` →
  /// `shrink_to_fit` → Frozen serialize (`Bitmap::serialize::<Frozen>` does
  /// not exist — Frozen is not NoAlign, imp.rs:871; `serialize_into_vec`
  /// carves a 32B-aligned slice inside the Vec, probe main.rs:60-77). Must go
  /// through the same checksumming output as the rest of .doc so the footer
  /// CRC stays valid (spec §4a.2; CodecUtil.writeCRC :643-650).
  pub fn write_term_bitmap(out: &mut impl DataOutput, docs: &[u32]) -> io::Result<()> {
      debug_assert!(!docs.is_empty());
      let mut bitmap = croaring::Bitmap::of(docs);
      bitmap.run_optimize();
      bitmap.shrink_to_fit();
      let mut buf = Vec::new();
      let payload = bitmap.serialize_into_vec::<croaring::Frozen>(&mut buf);
      let mut region = IndexOutput::in_memory();
      // in-memory writes never fail (Vec sink)
      region.write_bytes(&BITMAP_MAGIC).unwrap();
      region.write_byte(BITMAP_VERSION).unwrap();
      region.write_vint(docs.len() as i32).unwrap();
      // docs-only bitmap: cardinality == df (asserted by construction)
      debug_assert_eq!(bitmap.cardinality(), docs.len() as u64);
      region.write_vint(docs.len() as i32).unwrap();
      region.write_bytes(payload).unwrap();
      let bytes = region.into_bytes();
      out.write_bytes(&bytes)?;
      out.write_int(bytes.len() as i32)?;
      Ok(())
  }
  ```

- [ ] **Step 1.4: postings_read.rs 读侧钉死** — `open_term_bitmap`（postings_read.rs:169-184）与 `probe_term_bitmap`（:186-199）body 替换为（签名不变，T2 重写/删除）：

  ```rust
      /// M5 T1: 写侧已切 v3（croaring Frozen payload），M4 的 v2 容器目录
      /// 解析器不得见到 v3 字节（版本门之后即是解析，关键设计事实 10）——
      /// 读侧钉死落档直到 T2 接入 frozen view（T2 重写本函数返回
      /// FrozenBitmap；probe 模式随之取消，关键设计事实 6）。
      pub fn open_term_bitmap(
          &self,
          entry: &TermEntry,
          max_doc: u32,
      ) -> io::Result<Option<RoaringView>> {
          let _ = (entry, max_doc);
          Ok(None)
      }
  ```

  （`probe_term_bitmap` 同款，注释指向上条。`RoaringView` import 保留——返回类型仍在用；`locate_bitmap_region` 不动但暂时无消费者，它是 pub(crate) 私有 fn…… 改：保留 locate 不动——`open`/`probe` 钉死后 locate 变 dead code（私有 fn 会告警）。处理：钉死 body 里保留 locate 调用再丢弃结果，保持校验①活着且零告警：

  ```rust
      pub fn open_term_bitmap(
          &self,
          entry: &TermEntry,
          max_doc: u32,
      ) -> io::Result<Option<RoaringView>> {
          let mut input = self.fresh_input()?;
          // T1 钉死：locate 照跑（校验① len 有界），但永不打开（T2 重写）
          let _ = Self::locate_bitmap_region(&mut input, entry, max_doc)?;
          Ok(None)
      }
  ```

  ）

- [ ] **Step 1.5: 两 crate 测试全绿 + fmt + commit**

  ```
  $ cargo test -p codec-lucene9 2>&1 | tail -2
  test result: ok. 161 passed; 0 failed; 1 ignored; ...   # 162 − 1（删 open_and_probe_term_bitmap_match_postings）
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. 45 passed; 0 failed; ...               # 数量不变（翻转型改造）
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ git add -A && git commit -m "feat: bitmap format v3 — croaring build + Frozen payload write side, reader pinned to fallback"
  ```

---
## Task 2: 读侧 Frozen view 打开（对齐 buffer + 单点 unsafe 封装 + 安全 API）+ Term 接入 + 全路径类型切换

**Files:**
- Create: `crates/codec-lucene9/src/roaring/frozen.rs`（`FrozenBitmap` + 安全预校验 + 唯一 unsafe 调用点 + 测试）
- Modify: `crates/codec-lucene9/src/roaring.rs`（`mod frozen; pub use` + `parse_region` + 模块文档一行）
- Modify: `crates/codec-lucene9/src/postings_read.rs`（`open_term_bitmap` 重写返回 `FrozenBitmap`、删 `probe_term_bitmap`、import、新增 `open_term_bitmap_matches_postings`）
- Modify: `crates/core/src/search/segment_reader.rs`（包装改型 + 删 probe 包装）
- Modify: `crates/core/src/search/doc_iter.rs`（`BitmapCursor` + `DocSource::Bitmap` + `RoaringDocIter`/`RoaringAndDocIter`/`RoaringOrDocIter` 换引擎）
- Modify: `crates/core/src/search/roaring_exec.rs`（`probe_clauses` → `open_clauses`，类型切换，M4 策略骨架保留）
- Modify: `crates/core/src/search/mod.rs`（Step 1.1⑥ 的翻转断言全部还原）

**Interfaces:**
- Consumes: T1 的 v3 常量/上界/payload 契约；`croaring::{BitmapView, Frozen}`（`BitmapView::deserialize::<Frozen>`——唯一 unsafe；view 经 `Deref<Target = Bitmap>` 得 `contains(u32) -> bool`、`cardinality() -> u64`、`and_cardinality(&self, &Bitmap) -> u64`、`or_cardinality`、`iter() -> BitmapIterator`（`reset_at_or_after(u32)`、`next_many(&mut [u32]) -> usize`）——全部与探针 `main.rs` 工作用法一致）；`Frozen::REQUIRED_ALIGNMENT = 32`。
- Produces（T3 依赖这些名字，不得改名）:
  ```rust
  // roaring/frozen.rs（模块级 #![allow(unsafe_code)]，crate 第三个）
  pub struct FrozenBitmap { /* buf: Vec<u8>, off: usize */ }
  impl FrozenBitmap {
      pub fn open(payload: &[u8], expected_df: u32) -> Option<FrozenBitmap>;
      pub fn contains(&self, doc: u32) -> bool;
      pub fn cardinality(&self) -> u64;
      pub fn and_cardinality(&self, other: &FrozenBitmap) -> u64;
      pub fn or_cardinality(&self, other: &FrozenBitmap) -> u64;
      pub fn docs_from(&self, from: u32, dst: &mut [u32]) -> usize; // 批量升序读，0 = 尽
  }
  // roaring.rs
  pub use frozen::FrozenBitmap;
  pub fn parse_region(region: &[u8], expected_df: u32) -> Option<FrozenBitmap>;
  // postings_read.rs（probe_term_bitmap 删除）
  pub fn open_term_bitmap(&self, entry: &TermEntry, max_doc: u32) -> io::Result<Option<FrozenBitmap>>;
  // core segment_reader.rs
  pub(crate) fn open_term_bitmap(&self, entry: &TermEntry) -> io::Result<Option<FrozenBitmap>>;
  // core doc_iter.rs
  pub enum DocSource { Bitmap { .. }, Slice { .. } }
  impl DocSource { pub fn bitmap(FrozenBitmap) -> DocSource; pub fn slice(Vec<u32>) -> DocSource; }
  pub struct RoaringDocIter { .. }        // pub fn new(FrozenBitmap)
  pub struct RoaringAndDocIter { .. }     // pub fn new(Vec<DocSource>, Vec<FrozenBitmap>)
  pub struct RoaringOrDocIter { .. }      // pub fn new(Vec<DocSource>)
  ```

  语义决定：`parse_region` 的三重门 = magic → version(!= 3 → None，v1/v2 落档） → df == expected && card == df，然后 `FrozenBitmap::open`（安全结构预校验 + 对齐 memcpy + cardinality 复核）。ANY failure → None（查询永不报错）。T2 的 roaring_exec 保留 M4 策略形态（非偏斜 merge-intersect、偏斜 probes、OR merge-union、count 驱动迭代器）——只换引擎类型；物化 fold 与 cardinality 快路径属 T3。

### Steps

- [ ] **Step 2.1: 写 frozen/读侧测试（先失败）**

  ① 新文件 `crates/codec-lucene9/src/roaring/frozen.rs` 先只放模块壳（`#![allow(unsafe_code)]` + 空 `pub struct FrozenBitmap;` 占位会让 ② 编译失败——TDD 顺序：先写测试文件主体与全部实现外的壳，跑 ② 见红，Step 2.3 填实现）。测试模块完整代码：

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use croaring::Bitmap;

      /// xorshift64* — deterministic, same as the other codec test modules.
      struct Rng(u64);
      impl Rng {
          fn next(&mut self) -> u64 {
              let mut x = self.0;
              x ^= x >> 12;
              x ^= x << 25;
              x ^= x >> 27;
              self.0 = x;
              x.wrapping_mul(0x2545_F491_4F6C_DD1D)
          }
          fn below(&mut self, n: u32) -> u32 {
              (self.next() % n as u64) as u32
          }
      }

      /// Sorted unique docs: `n` random low-16 values per listed bucket.
      fn shaped(seed: u64, buckets: &[(u16, usize)]) -> Vec<u32> {
          let mut rng = Rng(seed);
          let mut docs: Vec<u32> = Vec::new();
          for &(key, n) in buckets {
              for _ in 0..n {
                  docs.push(((key as u32) << 16) | rng.below(65536));
              }
              docs.sort_unstable();
              docs.dedup();
          }
          docs
      }

      /// Frozen payload via the write-side pipeline (T1, probe main.rs:60-77).
      fn frozen_payload(docs: &[u32]) -> Vec<u8> {
          let mut b = Bitmap::of(docs);
          b.run_optimize();
          b.shrink_to_fit();
          let mut buf = Vec::new();
          let s = b.serialize_into_vec::<Frozen>(&mut buf);
          s.to_vec()
      }

      fn open_shaped(seed: u64, buckets: &[(u16, usize)]) -> (Vec<u32>, FrozenBitmap) {
          let docs = shaped(seed, buckets);
          let bm = FrozenBitmap::open(&frozen_payload(&docs), docs.len() as u32).unwrap();
          (docs, bm)
      }

      #[test]
      fn open_aligns_buffer_and_iterates_in_batches() {
          let docs: Vec<u32> = (0..10_000u32).map(|i| i * 7).collect();
          let bm = FrozenBitmap::open(&frozen_payload(&docs), docs.len() as u32).unwrap();
          assert_eq!(bm.payload().as_ptr() as usize % 32, 0, "32B alignment");
          assert_eq!(bm.cardinality(), docs.len() as u64);
          // batch 1
          let mut buf = [0u32; 4096];
          let n = bm.docs_from(0, &mut buf);
          assert_eq!(n, 4096);
          assert_eq!(&buf[..n], &docs[..4096]);
          // batch 2 resumes strictly after the last emitted doc
          let n2 = bm.docs_from(buf[n - 1] + 1, &mut buf);
          assert_eq!(&buf[..n2], &docs[4096..4096 + n2]);
          // seek lands on the first doc >= from; past-the-end yields 0
          let n3 = bm.docs_from(63_000, &mut buf);
          assert_eq!(&buf[..n3], &docs[9000..]);
          assert_eq!(bm.docs_from(70_000, &mut buf), 0);
      }

      #[test]
      fn contains_and_cardinality_match_reference() {
          let (docs, bm) = open_shaped(7, &[(0, 5000), (3, 9000)]);
          for d in 0..300_000u32 {
              assert_eq!(bm.contains(d), docs.binary_search(&d).is_ok(), "doc {d}");
          }
          let (docs2, bm2) = open_shaped(11, &[(0, 4000), (3, 8000)]);
          let (a, b) = (Bitmap::of(&docs), Bitmap::of(&docs2));
          assert_eq!(bm.and_cardinality(&bm2), a.and_cardinality(&b));
          assert_eq!(bm.or_cardinality(&bm2), a.or_cardinality(&b));
      }

      #[test]
      fn open_rejects_structural_deviations() {
          let docs = shaped(5, &[(0, 100), (1, 5000)]);
          let payload = frozen_payload(&docs);
          let df = docs.len() as u32;
          let n = payload.len();
          // cardinality != expected_df (validation ③)
          assert!(FrozenBitmap::open(&payload, df + 1).is_none());
          // bad cookie / num_containers (tail 4B header)
          let mut bad = payload.clone();
          bad[n - 1] ^= 0xFF;
          assert!(FrozenBitmap::open(&bad, df).is_none());
          // bad typecode
          let header = u32::from_le_bytes(payload[n - 4..].try_into().unwrap());
          let num = (header >> 15) as usize;
          let mut bad = payload.clone();
          bad[n - 4 - num] = 9; // typecodes zone: n - 4 - num .. n - 4
          assert!(FrozenBitmap::open(&bad, df).is_none());
          // exact-length contract: truncated / padded
          assert!(FrozenBitmap::open(&payload[..n - 1], df).is_none());
          let mut padded = payload.clone();
          padded.push(0);
          assert!(FrozenBitmap::open(&padded, df).is_none());
          // too small for a header
          assert!(FrozenBitmap::open(&payload[..3], df).is_none());
      }
  }
  ```

  ② `crates/codec-lucene9/src/postings_read.rs` 测试模块：新增（取代 T1 删除的 `open_and_probe_term_bitmap_match_postings`，覆盖 = 迭代等价 + contains 抽样 + df 锚点）：

  ```rust
      /// M5 §2/§3 frozen view 打开：view 迭代与 postings 逐 doc 一致，
      /// contains 抽样一致，cardinality == doc_freq（write_segment_bitmap
      /// 语料：tx "hot" df=5000，docs = 0..5000）。
      #[test]
      fn open_term_bitmap_matches_postings() {
          let root = temp_dir("bitmap-open");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_segment_bitmap(&dir);
          let e = seek(&dir, &fis, "tx", b"hot");
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
          let bm = postings
              .open_term_bitmap(&e, 6000)
              .unwrap()
              .expect("v3 bitmap opens");
          assert_eq!(bm.cardinality(), 5000);
          // batched iteration == postings enumeration
          let mut en = postings.docs(&e).unwrap();
          let mut buf = [0u32; 1024];
          let mut from = 0u32;
          loop {
              let n = bm.docs_from(from, &mut buf);
              if n == 0 {
                  break;
              }
              for &d in &buf[..n] {
                  assert_eq!(en.next_doc().unwrap(), d as i32);
              }
              from = buf[n - 1] + 1;
          }
          assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
          // contains sampling
          for d in (0..6000u32).step_by(7) {
              assert_eq!(bm.contains(d), d < 5000, "doc {d}");
          }
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  ③ `crates/core/src/search/mod.rs`：Step 1.1⑥ 的翻转全部还原（`term_query_uses_roaring_when_bitmap_present` 的 Roaring 断言、`bool_query_three_tier_roaring` 的档 1/档 2 断言、`and_skew_probe_matches_pfor` 与 `bool_query_roaring_multi_segment` 的路径断言），删掉 T1 注释。`bitmap_v2_index_falls_back_to_postings` 一字不改（T2 起走真实版本门，断言不变）。

- [ ] **Step 2.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 frozen 2>&1 | tail -4
  FAILED（FrozenBitmap::open 未实现 / 编译错误——壳与测试签名不符即视为失败）
  $ cargo test -p codec-lucene9 open_term_bitmap_matches_postings 2>&1 | tail -3
  FAILED（open_term_bitmap 仍钉死 None → expect 失败）
  $ cargo test -p rustlucene-core term_query_uses_roaring_when_bitmap_present 2>&1 | tail -3
  FAILED（读侧仍落档，hot 不是 Roaring 变体）
  ```

- [ ] **Step 2.3: `roaring/frozen.rs` 完整实现** — 文件全文（模块文档 + 安全论证 + 唯一 unsafe 调用点 + 安全预校验）：

  ```rust
  //! Frozen-view read side of the v3 inline term bitmap (M5 spec §2/§3):
  //! one sequential region read into a 32B-aligned buffer, then zero-copy
  //! `croaring::BitmapView::deserialize::<Frozen>` views (~60ns create,
  //! probe REPORT §Bench). `FrozenBitmap` owns the buffer; views/iterators
  //! borrow it, so they are rebuilt per call instead of stored — no
  //! self-referential lifetimes, no second unsafe (关键设计事实 5).
  //!
  //! Validation = the v3 triple gate (len bound → magic/version → header
  //! df/card == doc_freq, done by `parse_region` before `open`) plus a safe
  //! re-implementation of every structural check CRoaring's
  //! `roaring_bitmap_frozen_view` performs (cookie, typecodes, exact length —
  //! roaring.c:18153-18203): the C side signals invalid bytes with NULL, and
  //! the croaring wrapper asserts non-null (croaring-2.7.0
  //! src/bitmap/view.rs:34), so without the pre-check corrupt bytes would
  //! panic instead of falling back. Post-open, `cardinality == df` is
  //! rechecked (validation ③). Any failure → None, the silent postings
  //! fallback (查询永不报错).
  //!
  //! ## Safety argument (module-level `allow(unsafe_code)`)
  //!
  //! The crate is `#![deny(unsafe_code)]`; this module is the third narrow
  //! exception (postings_ll/simd.rs, roaring/simd.rs — the latter deleted in
  //! T4). The single unsafe operation is `FrozenBitmap::view`'s
  //! `BitmapView::deserialize::<Frozen>`, whose contract (32B-aligned start,
  //! exact frozen length, valid frozen bytes — croaring-2.7.0
  //! src/bitmap/serialization.rs `impl ViewDeserializer for Frozen`) is
  //! fully discharged by `open`: the aligned slot is asserted at
  //! construction, the length is the region's payload length, and
  //! `validate_frozen_layout` rejects every byte pattern the C side would
  //! return NULL for.
  #![allow(unsafe_code)]

  use croaring::{BitmapView, Frozen};

  /// CRoaring's frozen format cookie (roaring.h:7935 `FROZEN_COOKIE =
  /// 13766`): low 15 bits of the trailing 4-byte header; num_containers in
  /// the high 17 bits (roaring.c:18000-18004).
  const FROZEN_COOKIE: u32 = 13766;

  /// CRoaring container typecodes (roaring.h:5237-5239).
  const TYPE_BITSET: u8 = 1;
  const TYPE_ARRAY: u8 = 2;
  const TYPE_RUN: u8 = 3;

  /// Safe mirror of `roaring_bitmap_frozen_view`'s structural validation
  /// (roaring.c:18153-18203): cookie → num_containers → typecodes ∈
  /// {1,2,3} → exact length from the counts array (array/bitset counts =
  /// card-1, run counts = n_runs, roaring.c:17993-17998). None = reject.
  fn validate_frozen_layout(payload: &[u8]) -> Option<()> {
      let n = payload.len();
      if n < 4 {
          return None;
      }
      let header = u32::from_le_bytes(payload[n - 4..].try_into().unwrap());
      if header & 0x7FFF != FROZEN_COOKIE {
          return None;
      }
      let num = (header >> 15) as usize;
      if num < 1 || n < 4 + num * 5 {
          return None;
      }
      // zones at the tail: keys[num] counts[num] typecodes[num] header(4)
      let counts_off = n - 4 - num * 3;
      let typecodes_off = n - 4 - num;
      let mut size = 4 + 5 * num;
      for i in 0..num {
          let count = u16::from_le_bytes(
              payload[counts_off + 2 * i..counts_off + 2 * i + 2]
                  .try_into()
                  .unwrap(),
          ) as usize;
          size += match payload[typecodes_off + i] {
              TYPE_BITSET => 8192,           // 1024 * u64
              TYPE_ARRAY => (count + 1) * 2, // counts = cardinality - 1
              TYPE_RUN => count * 4,         // counts = n_runs; rle16_t
              _ => return None,
          };
      }
      if size != n {
          return None; // not exactly one frozen bitmap
      }
      Some(())
  }

  /// Opened + validated frozen payload of a v3 bitmap region (M5 §2). Owns
  /// the payload copy in a 32B-aligned slot; all reads go through freshly
  /// created `BitmapView`s (~60ns, amortized over batches by `docs_from`).
  /// 32 bytes (Vec + usize) — no boxing needed (M4 事实 6 的 8KB 定位流
  /// 顾虑随 probe 模式消失).
  pub struct FrozenBitmap {
      buf: Vec<u8>, // payload.len() + 31; the aligned slice starts at `off`
      off: usize,
  }

  impl FrozenBitmap {
      /// Validates + copies a region's frozen payload (region minus the
      /// [magic+version+df+card] header, parsed by `parse_region`). None on
      /// ANY deviation — the silent-fallback signal.
      pub fn open(payload: &[u8], expected_df: u32) -> Option<FrozenBitmap> {
          validate_frozen_layout(payload)?;
          let mut buf = vec![0u8; payload.len() + Frozen::REQUIRED_ALIGNMENT - 1];
          let off = buf.as_ptr().align_offset(Frozen::REQUIRED_ALIGNMENT);
          // Vec<u8> alignment is 1; with 31 spare bytes an aligned slot
          // always exists (align_offset fails only when it cannot prove one)
          assert!(off != usize::MAX && off + payload.len() <= buf.len());
          buf[off..off + payload.len()].copy_from_slice(payload);
          let bm = FrozenBitmap { buf, off };
          // validation ③ (spec §3): cardinality == termState.doc_freq
          if bm.cardinality() != expected_df as u64 {
              return None;
          }
          Some(bm)
      }

      fn payload(&self) -> &[u8] {
          &self.buf[self.off..]
      }

      fn view(&self) -> BitmapView<'_> {
          // SAFETY: the slice starts at a 32-byte-aligned offset (asserted
          // in `open`) and its length is exactly the frozen bitmap's length;
          // `validate_frozen_layout` has already rejected every byte pattern
          // CRoaring's `roaring_bitmap_frozen_view` returns NULL for
          // (roaring.c:18153-18203), so the wrapper's non-null assert cannot
          // fire. The view borrows `self` and never outlives this call's
          // caller.
          unsafe { BitmapView::deserialize::<Frozen>(self.payload()) }
      }

      /// Membership test (8.5–28.7 ns/probe, probe REPORT §Bench) — the
      /// skew-AND probe and tier-2 filter primitive.
      pub fn contains(&self, doc: u32) -> bool {
          self.view().contains(doc)
      }

      pub fn cardinality(&self) -> u64 {
          self.view().cardinality()
      }

      /// Non-materializing intersection count (SIMD C path, µs级, M5 §2).
      pub fn and_cardinality(&self, other: &FrozenBitmap) -> u64 {
          self.view().and_cardinality(&other.view())
      }

      /// Non-materializing union count (M5 §2).
      pub fn or_cardinality(&self, other: &FrozenBitmap) -> u64 {
          self.view().or_cardinality(&other.view())
      }

      /// Batch ascending-doc read: fills `dst` with the first docs >=
      /// `from`, returns the count read (0 = exhausted).
      /// `reset_at_or_after` + `next_many` include the current value
      /// (croaring-2.7.0 src/bitmap/iter.rs `BitmapIterator`), so a caller
      /// resuming after doc d passes d+1. View creation (~60ns) is
      /// amortized over the batch — the wrapper stays free of
      /// self-referential lifetimes (关键设计事实 5).
      pub fn docs_from(&self, from: u32, dst: &mut [u32]) -> usize {
          let view = self.view();
          let mut it = view.iter();
          it.reset_at_or_after(from);
          it.next_many(dst)
      }
  }
  ```

  （测试模块 = Step 2.1① 的全文，追加在实现之后。）

- [ ] **Step 2.4: codec 接线** — 三处：

  ① `roaring.rs`：`mod simd;`（T4 删除）之后加 `mod frozen;`，`pub use view::{RoaringView, ViewCursor};` 之后加 `pub use frozen::FrozenBitmap;`；模块文档加一行 "Frozen view read side lives in `roaring/frozen.rs` (M5 T2)"；文件尾部（`write_term_bitmap` 之后）新增：

  ```rust
  /// Parses + validates a v3 bitmap region (the `len` bytes preceding
  /// docStartFP-4; M5 §3): magic → version (!= 3, v1/v2 included → None,
  /// the whole migration story) → df == expected_df → card == df → frozen
  /// payload open (structural pre-validation + cardinality recheck,
  /// frozen.rs). None = the read side's silent-fallback signal.
  pub fn parse_region(region: &[u8], expected_df: u32) -> Option<FrozenBitmap> {
      // magic 4 + version 1 + df/card ≥ 1B each + frozen header 4
      if region.len() < 11 {
          return None;
      }
      let mut input = IndexInput::in_memory(region.to_vec());
      let mut magic = [0u8; 4];
      input.read_bytes(&mut magic).ok()?;
      if magic != BITMAP_MAGIC {
          return None;
      }
      if input.read_byte().ok()? != BITMAP_VERSION {
          return None;
      }
      let df = input.read_vint().ok()? as u32;
      if df != expected_df {
          return None;
      }
      let card = input.read_vint().ok()? as u32;
      if card != df {
          return None; // docs-only bitmap: cardinality == df
      }
      FrozenBitmap::open(&region[input.file_pointer() as usize..], expected_df)
  }
  ```

  ② `postings_read.rs`：import 行（postings_read.rs:16）改 `use crate::roaring::{self, FrozenBitmap};`；`open_term_bitmap` body 重写为：

  ```rust
      /// Frozen-view open of the term's inline bitmap (M5 §2/§3): v3 triple
      /// validation + one sequential region read into a 32B-aligned buffer
      /// + zero-copy croaring views (single unsafe site, roaring/frozen.rs).
      /// None → postings fallback. The query path's only bitmap entry point
      /// (probe mode is gone — frozen contains is a direct memory probe,
      /// 关键设计事实 6).
      pub fn open_term_bitmap(
          &self,
          entry: &TermEntry,
          max_doc: u32,
      ) -> io::Result<Option<FrozenBitmap>> {
          let mut input = self.fresh_input()?;
          let Some((start, len)) = Self::locate_bitmap_region(&mut input, entry, max_doc)? else {
              return Ok(None);
          };
          input.seek(start)?;
          let mut region = vec![0u8; len as usize];
          input.read_bytes(&mut region)?;
          Ok(roaring::parse_region(&region, entry.doc_freq))
      }
  ```

  删除 `probe_term_bitmap`（:186-199 含文档注释）。

  ③ `segment_reader.rs`：import（:10）改 `use codec_lucene9::roaring::FrozenBitmap;`；`open_term_bitmap` 返回类型改 `io::Result<Option<FrozenBitmap>>`（body 不变）；删除 `probe_term_bitmap` 包装（:92-99 含文档注释）。

- [ ] **Step 2.5: core 引擎类型切换** — `doc_iter.rs` roaring 段（:516-811）整体替换：

  ① import（:9）改 `use codec_lucene9::roaring::FrozenBitmap;`。

  ② `RoaringDocIter` 段（:516-570）替换为：

  ```rust
  // ── Roaring (inline term bitmap, M5 §2 croaring frozen view) ─────────

  /// Batch-refill cursor over a `FrozenBitmap` (M5 T2, 关键设计事实 5):
  /// the ~60ns frozen-view create is amortized over a 512-doc batch; each
  /// refill is a container-level `reset_at_or_after` seek + bulk
  /// `next_many`. docs are < max_doc <= i32::MAX, so `d + 1` never
  /// overflows u32.
  struct BitmapCursor {
      bitmap: FrozenBitmap,
      buf: Vec<u32>,
      pos: usize,
      end: usize,
      next_from: u32,
      exhausted: bool,
  }

  const BITMAP_ITER_BATCH: usize = 512;

  impl BitmapCursor {
      fn new(bitmap: FrozenBitmap) -> BitmapCursor {
          BitmapCursor {
              bitmap,
              buf: vec![0; BITMAP_ITER_BATCH],
              pos: 0,
              end: 0,
              next_from: 0,
              exhausted: false,
          }
      }

      fn refill(&mut self) -> bool {
          if self.exhausted {
              return false;
          }
          self.end = self.bitmap.docs_from(self.next_from, &mut self.buf);
          self.pos = 0;
          if self.end == 0 {
              self.exhausted = true;
              return false;
          }
          true
      }

      fn next(&mut self) -> Option<u32> {
          if self.pos >= self.end && !self.refill() {
              return None;
          }
          let d = self.buf[self.pos];
          self.pos += 1;
          self.next_from = d + 1;
          Some(d)
      }

      /// First doc >= target. Forward-only: discards the buffered tail and
      /// re-seeks (the merge dance's resync pattern, M4 关键设计事实 8).
      fn advance(&mut self, target: u32) -> Option<u32> {
          self.pos = self.end;
          self.next_from = target;
          self.next()
      }
  }

  /// DocIter over a term's inline bitmap (M5 §2 Term 路径): wraps the
  /// frozen view's batch cursor. freq() is 1 — the bitmap carries no freqs,
  /// and needs_freq paths never get this iterator (correctness
  /// requirement (e)).
  pub struct RoaringDocIter {
      cur: BitmapCursor,
      doc: i32,
  }

  impl RoaringDocIter {
      pub fn new(bitmap: FrozenBitmap) -> RoaringDocIter {
          RoaringDocIter {
              cur: BitmapCursor::new(bitmap),
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
          self.doc = match self.cur.next() {
              Some(d) => d as i32,
              None => NO_MORE_DOCS,
          };
          Ok(self.doc)
      }

      fn advance(&mut self, target: i32) -> io::Result<i32> {
          if target > self.doc {
              self.doc = match self.cur.advance(target.max(0) as u32) {
                  Some(d) => d as i32,
                  None => NO_MORE_DOCS,
              };
          }
          Ok(self.doc)
      }
  }
  ```

  ③ `DocSource`（:577-649）的 `View` 变体替换为 `Bitmap`（`Slice` 变体与 `current`/`next`/`advance` 的 Slice 臂逐行不变）：

  ```rust
  /// One merge-intersect / merge-union source (M5 §2): a frozen-view batch
  /// cursor, or a materialized low-df clause (df<4096, bounded). Both yield
  /// ascending docs with a forward-only advance.
  pub enum DocSource {
      Bitmap { cur: BitmapCursor, doc: Option<u32> },
      Slice { docs: Vec<u32>, pos: usize },
  }

  impl DocSource {
      /// Frozen-view source, primed to its first doc.
      pub fn bitmap(bitmap: FrozenBitmap) -> DocSource {
          let mut cur = BitmapCursor::new(bitmap);
          let doc = cur.next();
          DocSource::Bitmap { cur, doc }
      }

      /// Materialized low-df clause source (spec §5 档 2: df<4096 → ≤4095
      /// docs, ascending by enum construction).
      pub fn slice(docs: Vec<u32>) -> DocSource {
          DocSource::Slice { docs, pos: 0 }
      }

      fn current(&self) -> Option<u32> {
          match self {
              DocSource::Bitmap { doc, .. } => *doc,
              DocSource::Slice { docs, pos } => docs.get(*pos).copied(),
          }
      }

      fn next(&mut self) -> Option<u32> {
          match self {
              DocSource::Bitmap { cur, doc } => {
                  *doc = cur.next();
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
              DocSource::Bitmap { cur, doc } => {
                  if doc.is_some_and(|d| d >= target) {
                      return *doc;
                  }
                  *doc = cur.advance(target);
                  *doc
              }
              DocSource::Slice { docs, pos } => {
                  *pos += docs[*pos..].partition_point(|&d| d < target);
                  docs.get(*pos).copied()
              }
          }
      }
  }
  ```

  ④ `RoaringAndDocIter`（:657-749）：`probes: Vec<Box<RoaringView>>` → `probes: Vec<FrozenBitmap>`（FrozenBitmap ≈32B，不再 Box，关键设计事实 13）；`new(sources: Vec<DocSource>, probes: Vec<FrozenBitmap>)`（去掉 `.map(Box::new)`）；probe 循环（:721-726）改非 io 直查：

  ```rust
              // point-probe the agreed candidate against every bitmap clause
              // (croaring contains: direct memory probe, 8.5–28.7 ns)
              let mut pass = true;
              for p in &self.probes {
                  if !p.contains(target) {
                      pass = false;
                      break;
                  }
              }
  ```

  其余逐行不变。⑤ `RoaringOrDocIter`（:756-811）逐行不变（`DocSource` 已换引擎）。⑥ `SegmentDocIter` 枚举与委派（:815-879）逐行不变。

- [ ] **Step 2.6: roaring_exec.rs 类型切换（M4 策略骨架保留）** — 全文替换（与 M4 的差异：probe_clauses → open_clauses 返回 `FrozenBitmap`、`DocSource::view` → `DocSource::bitmap`、probe 模式注释更新；SKEW_RATIO 数值与档形态不动）：

  ```rust
  //! Roaring execution for Boolean queries (M3 §5 three-tier rule per
  //! segment, M5 §2 croaring engine): each AND/OR clause's doc source is its
  //! validated inline bitmap — a frozen-view batch cursor — or, for clauses
  //! under the bitmap threshold, a query-time materialized doc vec (df<4096,
  //! bounded). AND tier-1: skewed → smallest-side iteration + contains
  //! probes (8.5–28.7 ns direct memory probes), non-skewed → k-way
  //! merge-intersect (T2 keeps the M4 shape; T3 swaps in the croaring
  //! materialized fold). OR: k-way merge-union over batch cursors +
  //! materialized slices. When NO clause has a bitmap the callers fall back
  //! to the existing PFOR conjunction/disjunction untouched (tier 3).

  use std::io;

  use codec_lucene9::postings_read::NO_MORE_DOCS;
  use codec_lucene9::roaring::FrozenBitmap;
  use codec_lucene9::terms_read::TermEntry;

  use super::doc_iter::{DocIter, DocSource, RoaringAndDocIter, RoaringOrDocIter, SegmentDocIter};
  use super::multi_term::for_each_doc;
  use super::segment_reader::SegmentReader;

  /// Tier-1 skew gate: when max/min df reaches this ratio the AND runs
  /// lead-cursor + contains() probes; below it, k-way merge-intersect. The
  /// M4 calibration (256) is carried over mechanically — T3 swaps the
  /// non-skewed strategy to the croaring materialized fold and resets the
  /// initial value, T5 recalibrates with croaring costs (关键设计事实 8).
  pub(crate) const SKEW_RATIO: u64 = 256;

  /// Collects the (df, entry) pairs of an And/Or's term clauses, df-sorted
  /// (conjunction cost order, same as the existing query.rs inline code).
  /// Returns (has_freqs, entries); None = empty segment result: unknown
  /// field, an absent AND clause, or no OR clause present.
  pub(crate) fn collect_bool_entries(
      seg: &mut SegmentReader,
      field: &str,
      terms: &[Vec<u8>],
      is_and: bool,
  ) -> io::Result<Option<(bool, Vec<(u32, TermEntry)>)>> {
      let Some(has_freqs) = seg.field_has_freqs(field) else {
          return Ok(None);
      };
      let mut entries = Vec::with_capacity(terms.len());
      for t in terms {
          match seg.seek_term(field, t)? {
              Some((_, entry)) => entries.push((entry.doc_freq, entry)),
              None => {
                  if is_and {
                      return Ok(None); // missing MUST clause: no hits in this segment
                  }
              }
          }
      }
      if entries.is_empty() {
          return Ok(None);
      }
      entries.sort_by_key(|(df, _)| *df);
      Ok(Some((has_freqs, entries)))
  }

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

  /// Opens every clause's frozen view (one sequential region read each —
  /// open/probe 合一, 关键设计事实 6). None = no clause has a bitmap
  /// (tier 3).
  fn open_clauses(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
  ) -> io::Result<Option<Vec<Option<FrozenBitmap>>>> {
      let mut opened = Vec::with_capacity(entries.len());
      for (_, entry) in entries {
          opened.push(seg.open_term_bitmap(entry)?);
      }
      if opened.iter().all(|o| o.is_none()) {
          return Ok(None); // tier 3
      }
      Ok(Some(opened))
  }

  /// Tier-1/2 AND over frozen views (M5 §2). entries are df-ascending (==
  /// cardinality-ascending, validation ③), so construction order is
  /// cheapest-first.
  fn and_iterator(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
  ) -> io::Result<Option<SegmentDocIter>> {
      let Some(opened) = open_clauses(seg, entries)? else {
          return Ok(None); // tier 3
      };
      let bitmap_count = opened.iter().filter(|o| o.is_some()).count();
      let mut sources: Vec<DocSource> = Vec::new();
      let mut probe_bitmaps: Vec<FrozenBitmap> = Vec::new();
      if bitmap_count < entries.len() {
          // tier 2: materialize the bitmap-less clauses (df<4096, bounded)
          // and point-probe their candidates against every bitmap view —
          // contains is a direct memory probe (spec §2 档 2 不变)
          for ((_, entry), opened) in entries.iter().zip(opened.into_iter()) {
              match opened {
                  Some(v) => probe_bitmaps.push(v),
                  None => sources.push(DocSource::slice(materialize_docs(seg, entry, has_freqs)?)),
              }
          }
      } else if entries.last().unwrap().0 as u64 >= SKEW_RATIO * entries[0].0 as u64 {
          // tier 1 skewed: smallest-side iteration, the rest contains-probes
          let mut it = opened.into_iter();
          let lead = it.next().unwrap().expect("tier 1: all present");
          sources.push(DocSource::bitmap(lead));
          probe_bitmaps.extend(it.map(|o| o.expect("tier 1: all present")));
      } else {
          // tier 1 non-skewed (T2: M4 merge-intersect 形态; T3 换 croaring
          // 物化 fold, 关键设计事实 8)
          for opened in opened {
              sources.push(DocSource::bitmap(opened.expect("tier 1: all present")));
          }
      }
      Ok(Some(SegmentDocIter::RoaringAnd(RoaringAndDocIter::new(
          sources,
          probe_bitmaps,
      ))))
  }

  /// Tier-1/2 OR over frozen views (M5 §2): k-way merge-union — one open
  /// per clause (the open doubles as the bitmap-presence probe), batch
  /// cursors + materialized low-df slices. All-None = tier 3. (T3: 全
  /// bitmap 时换 croaring 物化 or fold.)
  fn or_iterator(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
  ) -> io::Result<Option<SegmentDocIter>> {
      let Some(opened) = open_clauses(seg, entries)? else {
          return Ok(None); // tier 3
      };
      let mut sources: Vec<DocSource> = Vec::new();
      for ((_, entry), opened) in entries.iter().zip(opened.into_iter()) {
          match opened {
              Some(v) => sources.push(DocSource::bitmap(v)),
              None => sources.push(DocSource::slice(materialize_docs(seg, entry, has_freqs)?)),
          }
      }
      Ok(Some(SegmentDocIter::RoaringOr(RoaringOrDocIter::new(
          sources,
      ))))
  }

  /// Three-tier segment iterator (spec §5): Some = roaring path taken
  /// (tier 1/2); None = tier 3, the caller builds the PFOR iterator.
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

  /// Count (spec §5: count 走同一引擎): T2 驱动同一迭代器到尽头（count
  /// == 迭代结果数由构造保证）。None = tier 3, caller iterates. (T3: 全
  /// bitmap 时换 and/or_cardinality 快路径, spec §2.)
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

- [ ] **Step 2.7: 两 crate 测试全绿 + fmt + commit**

  ```
  $ cargo test -p codec-lucene9 2>&1 | tail -2
  test result: ok. 165 passed; 0 failed; 1 ignored; ...   # 161 + 3 frozen 测试 + 1 open 测试
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. 45 passed; 0 failed; ...               # 还原后数量不变
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ git add -A && git commit -m "feat: frozen-view read side — aligned buffer + single-unsafe FrozenBitmap, Term/boolean engine swap"
  ```

---
## Task 3: AND/OR 引擎（cardinality 快路径 + 物化 fold 迭代 + SKEW_RATIO 重置待标定）

**Files:**
- Modify: `crates/codec-lucene9/src/roaring/frozen.rs`（新增 `intersect_docs`/`union_docs`/`and_cardinality`/`or_cardinality` 四个自由 fn + fold 测试）
- Modify: `crates/codec-lucene9/src/roaring.rs`（re-export 四个 fold）
- Modify: `crates/core/src/search/roaring_exec.rs`（非偏斜 AND → 物化 fold、全 bitmap OR → 物化 fold、count → cardinality 快路径、SKEW_RATIO = 4 待标定）
- Modify: `crates/core/src/search/mod.rs`（新增 k=3 全 bitmap fold 测试）

**Interfaces:**
- Consumes: T2 的 `FrozenBitmap` API（`open`/`contains`/`cardinality`/`and_cardinality`/`or_cardinality`/`docs_from`）；croaring `BitmapView::to_bitmap()`（croaring-2.7.0 src/bitmap/view.rs:87）、`Bitmap::{and,or}(&self, &Bitmap) -> Bitmap`（imp.rs:420,475）、`Bitmap::iter()`。
- Produces（T5 的标定/bench 直接驱动这些入口）:
  ```rust
  // roaring/frozen.rs（roaring.rs re-export）
  pub fn intersect_docs(bitmaps: &[&FrozenBitmap]) -> Vec<u32>;   // 物化 and fold + collect
  pub fn union_docs(bitmaps: &[&FrozenBitmap]) -> Vec<u32>;       // 物化 or fold + collect
  pub fn and_cardinality(bitmaps: &[&FrozenBitmap]) -> u64;       // k=2 非物化；k>2 物化中间结果
  pub fn or_cardinality(bitmaps: &[&FrozenBitmap]) -> u64;
  // roaring_exec.rs（签名不变，语义更新）
  pub(crate) const SKEW_RATIO: u64 = 4; // T5 用 croaring 成本重标定
  pub(crate) fn segment_iterator(seg, entries, has_freqs, is_and) -> io::Result<Option<SegmentDocIter>>;
  pub(crate) fn count(seg, entries, has_freqs, is_and) -> io::Result<Option<u64>>;
  ```

  语义决定：count 快路径只在全子句有 bitmap 时启用（档 2 混合仍驱动迭代器——slice 不在 bitmap 里，cardinality fold 覆盖不了，事实 11）；k=2 纯 cardinality 非物化（7.7µs 级），k>2 物化中间结果。物化 fold 结果 collect 成 `Vec<u32>` 走 `DocSource::slice`——`RoaringAnd/OrDocIter` 与 `SegmentDocIter` 变体名不变，全部既有路径断言测试零改动保绿。

### Steps

- [ ] **Step 3.1: 写 fold/count 测试（先失败）**

  ① `crates/codec-lucene9/src/roaring/frozen.rs` 测试模块追加：

  ```rust
      #[test]
      fn fold_ops_match_reference() {
          let d1 = shaped(3, &[(0, 5000), (2, 3000)]);
          let d2 = shaped(4, &[(0, 4000), (2, 6000)]);
          let d3 = shaped(5, &[(1, 7000)]);
          let mk = |d: &Vec<u32>| FrozenBitmap::open(&frozen_payload(d), d.len() as u32).unwrap();
          let (b1, b2, b3) = (mk(&d1), mk(&d2), mk(&d3));
          let (r1, r2, r3) = (Bitmap::of(&d1), Bitmap::of(&d2), Bitmap::of(&d3));
          // k=2: pure cardinality fast path + materialized fold
          assert_eq!(and_cardinality(&[&b1, &b2]), r1.and_cardinality(&r2));
          assert_eq!(or_cardinality(&[&b1, &b2]), r1.or_cardinality(&r2));
          assert_eq!(
              intersect_docs(&[&b1, &b2]),
              r1.and(&r2).iter().collect::<Vec<_>>()
          );
          assert_eq!(
              union_docs(&[&b1, &b2]),
              r1.or(&r2).iter().collect::<Vec<_>>()
          );
          // k=3: materialized intermediates (C API is pairwise)
          let r12 = r1.and(&r2);
          assert_eq!(and_cardinality(&[&b1, &b2, &b3]), r12.and_cardinality(&r3));
          let r123 = r1.or(&r2);
          assert_eq!(or_cardinality(&[&b1, &b2, &b3]), r123.or_cardinality(&r3));
          assert_eq!(
              intersect_docs(&[&b1, &b2, &b3]),
              r12.and(&r3).iter().collect::<Vec<_>>()
          );
          // edges
          assert_eq!(and_cardinality(&[&b1]), d1.len() as u64);
          assert_eq!(or_cardinality(&[]), 0);
      }
  ```

  ② `crates/core/src/search/mod.rs` 测试模块追加（k=3 全 bitmap fold 路径——既有语料最多 2 个高 df term，覆盖不到 `[first, rest @ ..]` 物化中间结果臂）：

  ```rust
      /// M5 T3 k=3 全 bitmap fold 语料：fa=[0,6000)、fb=[2000,9000)、
      /// fc=[4000,12000)（df 都 ≥4096 有 bitmap）；交集 = [4000,6000) = 2000，
      /// 并集 = [0,12000) = 12000。
      fn write_fold_corpus(root: &std::path::Path, bitmap: bool) {
          let mut cfg = IndexWriterConfig::default();
          cfg.bitmap = bitmap;
          let mut w = IndexWriter::create(root, schema(), cfg).unwrap();
          for d in 0..12000u32 {
              let mut msg = String::new();
              if d < 6000 {
                  msg.push_str("fa ");
              }
              if (2000..9000).contains(&d) {
                  msg.push_str("fb ");
              }
              if d >= 4000 {
                  msg.push_str("fc");
              }
              w.add_document(doc("INFO", &format!("tid-{d}"), msg.trim()))
                  .unwrap();
          }
          w.commit().unwrap();
          drop(w);
      }

      /// k=3 全 bitmap 子句的物化 fold 迭代 + cardinality 快路径（spec §2），
      /// 与 bitmap-off PFOR 逐位一致；锚点钉死交集/并集数值。
      #[test]
      fn and_or_three_clause_fold_matches_pfor() {
          let root_off = temp_dir("foldoff");
          let root_on = temp_dir("foldon");
          write_fold_corpus(&root_off, false);
          write_fold_corpus(&root_on, true);
          let mut s_off = Searcher::open(&FSDirectory::open(&root_off).unwrap()).unwrap();
          let mut s_on = Searcher::open(&FSDirectory::open(&root_on).unwrap()).unwrap();
          let battery: Vec<Query> = vec![
              Query::and("message", &["fa", "fb", "fc"]),
              Query::or("message", &["fa", "fb", "fc"]),
              Query::and("message", &["fa", "fb"]),
              Query::or("message", &["fa", "fb"]),
          ];
          for q in &battery {
              let (a_total, a_docs) = s_off.top_docs(q, 15000).unwrap();
              let (b_total, b_docs) = s_on.top_docs(q, 15000).unwrap();
              assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
              assert_eq!(s_off.count(q).unwrap(), s_on.count(q).unwrap(), "count {q:?}");
          }
          assert_eq!(
              s_on.count(&Query::and("message", &["fa", "fb", "fc"])).unwrap(),
              2000
          );
          assert_eq!(
              s_on.count(&Query::or("message", &["fa", "fb", "fc"])).unwrap(),
              12000
          );
          fs::remove_dir_all(&root_off).unwrap();
          fs::remove_dir_all(&root_on).unwrap();
      }
  ```

- [ ] **Step 3.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 fold_ops_match_reference 2>&1 | tail -3
  FAILED（intersect_docs/union_docs/and_cardinality/or_cardinality 未实现 → 编译错误）
  $ cargo test -p rustlucene-core and_or_three_clause_fold_matches_pfor 2>&1 | tail -3
  FAILED 或 PASS-但-未走新路径（fold 实现前走 T2 merge 形态结果已一致——
  本测试的真正红绿门是 codec 侧 ①；core 侧防回归）
  ```

- [ ] **Step 3.3: codec fold 实现** — `frozen.rs` 尾部（`impl FrozenBitmap` 之后、测试模块之前）追加：

  ```rust
  /// Materialized pairwise intersection fold + collect (M5 §2: croaring
  /// 物化 and + 结果迭代; strategy rationale 关键设计事实 8). Set ops on
  /// views always allocate a new owned Bitmap (probe REPORT §API surface);
  /// all croaring-typed values stay inside the codec — core has no croaring
  /// dependency (关键设计事实 7).
  pub fn intersect_docs(bitmaps: &[&FrozenBitmap]) -> Vec<u32> {
      debug_assert!(!bitmaps.is_empty());
      let mut acc = bitmaps[0].view().to_bitmap();
      for b in &bitmaps[1..] {
          acc = acc.and(&b.view());
      }
      acc.iter().collect()
  }

  /// Materialized pairwise union fold + collect (M5 §2).
  pub fn union_docs(bitmaps: &[&FrozenBitmap]) -> Vec<u32> {
      debug_assert!(!bitmaps.is_empty());
      let mut acc = bitmaps[0].view().to_bitmap();
      for b in &bitmaps[1..] {
          acc = acc.or(&b.view());
      }
      acc.iter().collect()
  }

  /// Intersection cardinality fold (M5 §2 count 快路径): k=2 never
  /// materializes (SIMD cardinality-only C path, µs级); k>2 materializes
  /// intermediates (the C API is pairwise).
  pub fn and_cardinality(bitmaps: &[&FrozenBitmap]) -> u64 {
      match bitmaps {
          [] => 0,
          [a] => a.cardinality(),
          [a, b] => a.and_cardinality(b),
          [first, rest @ ..] => {
              let mut acc = first.view().to_bitmap();
              for b in rest {
                  acc = acc.and(&b.view());
              }
              acc.cardinality()
          }
      }
  }

  /// Union cardinality fold (M5 §2 count 快路径), same shape as
  /// `and_cardinality`.
  pub fn or_cardinality(bitmaps: &[&FrozenBitmap]) -> u64 {
      match bitmaps {
          [] => 0,
          [a] => a.cardinality(),
          [a, b] => a.or_cardinality(b),
          [first, rest @ ..] => {
              let mut acc = first.view().to_bitmap();
              for b in rest {
                  acc = acc.or(&b.view());
              }
              acc.cardinality()
          }
      }
  }
  ```

  `roaring.rs` 的 re-export 行改为：

  ```rust
  pub use frozen::{FrozenBitmap, and_cardinality, intersect_docs, or_cardinality, union_docs};
  ```

- [ ] **Step 3.4: roaring_exec.rs 引擎切换** — 四处精确替换：

  ① 模块文档头改为：

  ```rust
  //! Roaring execution for Boolean queries (M3 §5 three-tier rule per
  //! segment, M5 §2 croaring engine): each clause's doc source is its
  //! validated inline bitmap — a frozen view — or, for clauses under the
  //! bitmap threshold, a query-time materialized doc vec (df<4096, bounded).
  //! AND tier-1: skewed → smallest-side iteration + contains probes
  //! (8.5–28.7 ns direct memory probes), non-skewed → croaring
  //! materialized `and` fold + result iteration (关键设计事实 8). OR
  //! tier-1: materialized `or` fold; tier-2 mixed: k-way merge-union over
  //! batch cursors + slices. Count over all-bitmap clauses is the
  //! and/or_cardinality fold (µs级, spec §2 — no per-doc driving). When NO
  //! clause has a bitmap the callers fall back to the existing PFOR
  //! conjunction/disjunction untouched (tier 3).
  ```

  ② `SKEW_RATIO`（含文档注释）改为：

  ```rust
  /// Tier-1 skew gate: when max/min df reaches this ratio the AND runs
  /// smallest-side iteration + contains() probes; below it, the croaring
  /// materialized `and` fold. Initial 4 pending T5 recalibration with
  /// croaring costs (contains 8.5–28.7 ns/probe, and_cardinality 7.7µs
  /// sparse / 4.2µs dense / 0.27µs run, materialized and 10.2µs sparse /
  /// 157µs dense — probe REPORT §Bench; method 关键设计事实 15). Both
  /// strategies pay both region reads under frozen (关键设计事实 6), so
  /// the crossover can only be measured.
  pub(crate) const SKEW_RATIO: u64 = 4; // bench-calibrated (T5)
  ```

  ③ `and_iterator` 的非偏斜 else 臂改为：

  ```rust
      } else {
          // tier 1 non-skewed (spec §2): croaring materialized `and` fold
          // + result iteration (关键设计事实 8)
          let views: Vec<&FrozenBitmap> = opened.iter().map(|o| o.as_ref().unwrap()).collect();
          let docs = codec_lucene9::roaring::intersect_docs(&views);
          sources.push(DocSource::slice(docs));
      }
  ```

  ④ `or_iterator` 全 bitmap 快路径 + `count` 快路径：

  ```rust
  /// Tier-1/2 OR (M5 §2): all-bitmap → croaring materialized `or` fold +
  /// result iteration; mixed → k-way merge-union over batch cursors +
  /// materialized low-df slices. All-None = tier 3.
  fn or_iterator(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
  ) -> io::Result<Option<SegmentDocIter>> {
      let Some(opened) = open_clauses(seg, entries)? else {
          return Ok(None); // tier 3
      };
      if opened.iter().all(|o| o.is_some()) {
          let views: Vec<&FrozenBitmap> = opened.iter().map(|o| o.as_ref().unwrap()).collect();
          let docs = codec_lucene9::roaring::union_docs(&views);
          return Ok(Some(SegmentDocIter::RoaringOr(RoaringOrDocIter::new(
              vec![DocSource::slice(docs)],
          ))));
      }
      let mut sources: Vec<DocSource> = Vec::new();
      for ((_, entry), opened) in entries.iter().zip(opened.into_iter()) {
          match opened {
              Some(v) => sources.push(DocSource::bitmap(v)),
              None => sources.push(DocSource::slice(materialize_docs(seg, entry, has_freqs)?)),
          }
      }
      Ok(Some(SegmentDocIter::RoaringOr(RoaringOrDocIter::new(
          sources,
      ))))
  }

  /// Count (spec §2): all-bitmap clauses → and/or_cardinality fold (µs级,
  /// no per-doc driving); tier-2 mixed → drive the same iterator to
  /// exhaustion (bounded candidates; count == iteration by construction).
  /// None = tier 3, caller iterates.
  pub(crate) fn count(
      seg: &SegmentReader,
      entries: &[(u32, TermEntry)],
      has_freqs: bool,
      is_and: bool,
  ) -> io::Result<Option<u64>> {
      let Some(opened) = open_clauses(seg, entries)? else {
          return Ok(None); // tier 3
      };
      if opened.iter().all(|o| o.is_some()) {
          let views: Vec<&FrozenBitmap> = opened.iter().map(|o| o.as_ref().unwrap()).collect();
          return Ok(Some(if is_and {
              codec_lucene9::roaring::and_cardinality(&views)
          } else {
              codec_lucene9::roaring::or_cardinality(&views)
          }));
      }
      // tier 2: same-iterator count (reopening regions is µs级 and keeps
      // the two entry points stateless)
      let Some(mut it) = segment_iterator(seg, entries, has_freqs, is_and)? else {
          return Ok(None); // unreachable: open_clauses succeeded on the same bytes
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

- [ ] **Step 3.5: 两 crate 测试全绿 + fmt + commit**

  ```
  $ cargo test -p codec-lucene9 2>&1 | tail -2
  test result: ok. 166 passed; 0 failed; 1 ignored; ...   # 165 + 1 fold 测试
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. 46 passed; 0 failed; ...               # 45 + 1 k=3 fold 测试
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ git add -A && git commit -m "feat: croaring AND/OR engine — cardinality count fast path, materialized fold iteration, SKEW_RATIO reset for recalibration"
  ```

---
## Task 4: 删除自研容器库 + 全仓引用清零

**Files:**
- Modify: `crates/codec-lucene9/src/roaring.rs`（删容器库全部符号与 9 个容器测试——保留集合见关键设计事实 16）
- Delete: `crates/codec-lucene9/src/roaring/simd.rs`
- Delete: `crates/codec-lucene9/src/roaring/view.rs`
- Modify: `crates/codec-lucene9/src/lib.rs`（删 `pub use roaring::RoaringBitmap;`）
- Modify: `crates/codec-lucene9/src/postings_read.rs`（import 已无 RoaringView——确认无残留）

**Interfaces:**
- Consumes: 无（纯删除）。
- Produces: `roaring.rs` 终态公开面 = `BITMAP_MAGIC`/`BITMAP_VERSION`/`BITMAP_MIN_DF`/`max_bitmap_len`/`write_term_bitmap`/`parse_region` + re-export `frozen::{FrozenBitmap, and_cardinality, intersect_docs, or_cardinality, union_docs}`；crate 模块级 `#[allow(unsafe_code)]` 恢复恰好两个（`postings_ll/simd.rs` + `roaring/frozen.rs`，spec §4）。

### Steps

- [ ] **Step 4.1: 执行删除**

  ① `roaring.rs` 删除（关键设计事实 16 逐项）：`Container` enum + impl、`RoaringBitmap` struct + 全部 impl（`from_sorted_docs`/`cardinality`/`is_empty`/`and`/`or`/`cursor`/`cursor_next`/`cursor_advance`/`serialize`/`deserialize`）、`RoaringCursor`、`clone_container`、全部容器辅助 fn（`set_range`/`bit`/`next_set_bit`/`bitset_card`/`array_to_runs`/`bitset_to_array`/`bitset_to_runs`/`array_run_count`/`bitset_run_count` 等）、`ARRAY_THRESHOLD`/`BITSET_BITS`/`BITSET_WORDS`/`TYPE_ARRAY`/`TYPE_BITSET`/`TYPE_RUN`/`SELF_BUILT_WIRE_VERSION` 常量、`mod simd;`/`pub mod view;`/`pub use view::{RoaringView, ViewCursor};`、9 个容器测试与 `serialize_v1_for_test`/`to_vec`/`ref_and`/`ref_or`/`shaped_docs` 等测试辅助（`Rng` 若仍被保留测试使用则保留——`max_bitmap_len_bound_holds` 用 `shaped_docs`+`Rng`，保留这两个辅助）。模块文档更新为终态：

  ```rust
  //! Inline per-term bitmap in the .doc stream, format v3 (M5 spec §3):
  //! `[ magic "RLBM" + version=3 + df(vInt) + cardinality(vInt) + Frozen
  //! payload ][ len: u32 LE ]`, written ahead of a term's postings. The
  //! engine is croaring (CRoaring 4.7.1 via croaring-sys): the write side
  //! builds `Bitmap::of` → `run_optimize` → `shrink_to_fit` → Frozen
  //! serialize (`write_term_bitmap`); the read side opens zero-copy frozen
  //! views over a 32B-aligned buffer copy (`roaring/frozen.rs`, one of the
  //! crate's two module-level `#[allow(unsafe_code)]` modules). The
  //! self-built container library (M3/M4) was deleted in M5 T4.
  ```

  ② `git rm crates/codec-lucene9/src/roaring/simd.rs crates/codec-lucene9/src/roaring/view.rs`（用文件删除即可，随 commit 记录）。

  ③ `lib.rs`：删 `pub use roaring::RoaringBitmap;`（:36）；crate 文档的 unsafe 政策注释（:10-12）更新："two module-level exceptions — postings_ll/simd.rs and roaring/frozen.rs"。

- [ ] **Step 4.2: 引用清零验证**

  ```
  $ grep -rn "RoaringView\|ViewCursor\|RoaringBitmap\|RoaringCursor" crates/ ; echo "GREP_EMPTY=$?"
  GREP_EMPTY=1   # 无任何匹配
  $ grep -rn "Container\|from_sorted_docs\|cursor_advance" crates/core/src crates/jni-binding/src ; echo "GREP_EMPTY=$?"
  GREP_EMPTY=1
  $ cargo check --workspace 2>&1 | tail -2
  Finished `dev` profile ... （零 error 零 warning；含 jni-binding）
  ```

- [ ] **Step 4.3: 测试全绿 + fmt + commit**

  ```
  $ cargo test -p codec-lucene9 2>&1 | tail -2
  test result: ok. 152 passed; 0 failed; 1 ignored; ...   # 166 − 14（9 容器 + 4 view + 1 simd）
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. 46 passed; 0 failed; ...
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ git add -A && git commit -m "feat: delete self-built roaring container library — engine fully replaced by croaring"
  ```

---
## Task 5: 电池复跑 + bench 复测（三路 --no-cache）+ SKEW_RATIO 标定 + 写侧开销 + 报告

**Files:**
- Modify: `crates/core/src/search/roaring_exec.rs`（仅当标定结果 ≠ 4：`SKEW_RATIO` 常量一行 + 注释更新）
- Create: `.superpowers/sdd/m5-bench-report.md`（报告）、`m5-q-raw.txt`/`m5-q.txt`、`m5-bench-{rust-roaring,rust-pfor,java}.out`、`m5-counts-{rust-roaring,rust-pfor,java}.txt`（+`m5-counts-java.clean.txt`）、`m5-dense-bench-{rust-roaring,rust-pfor}.out`、`m5-skew-micro.txt`、`m5-write-throughput.txt`

**口径（全部与 M4 T5 逐字一致，关键设计事实 15）**：1M docs seed 42；`--warmup 10 --iter 30`；Java 侧 `--no-cache`；三路串行；查询文件经 Java 侧 `--dump-queries --tasks 50 --seed 42` + df 守卫；hit-counts 走 stderr；三路对拍 = roaring==pfor plain diff 为空 + Java 等效对拍（非 term= 行 sorted diff + term= 重合行 awk join）。M4 基线列取自 `.superpowers/sdd/m4-bench-report.md` §2（roaring/pfor：term high 1.16 / and high 0.33 / or high 1.40 / iterm high 0.85；and med 0.48 / or med 1.22 / iterm med 0.87）——M5 的修复目标正是 M4 遗留回退（稀疏 AND 0.33x、稠密 AND/OR 0.92x/1.34x，spec §1）。

### Steps

- [ ] **Step 5.1: 全量绿基线（T4 提交后的 HEAD）**

  ```
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ cargo test -p codec-lucene9 2>&1 | tail -2    # 152 passed
  $ cargo test -p rustlucene-core 2>&1 | tail -2  # 46 passed
  $ cargo check --workspace 2>&1 | tail -1        # Finished（含 jni-binding）
  ```

  v2 落档验收在此步内完成（**不加电池变体**，Global Constraints）：codec `open_term_bitmap_falls_back_on_legacy_version` + core `bitmap_v2_index_falls_back_to_postings` 两个 Rust 测试绿即通过。

- [ ] **Step 5.2: `make log-test` 五变体**（脚本/Makefile 零改动；200000 文档：seed 42 默认 / 43 `--positions` / 44 `--sparse` / 45 `--bigdict` / 46 `--bitmap`）

  ```
  $ make log-test 2>&1 | tee /tmp/log-test-m5.log; echo "EXIT=$?"
  EXIT=0
  $ grep -c "No problems were detected with this index" /tmp/log-test-m5.log
  11
  $ grep -c "SEARCH_INTEROP_OK" /tmp/log-test-m5.log   # 5
  $ grep -c "LOG_INTEROP_OK" /tmp/log-test-m5.log      # 5
  $ grep "Bitmap A/B\|FORCEMERGE_OK" /tmp/log-test-m5.log
  # --bitmap 变体（seed 46）含 "Bitmap A/B: Rust searchdump bitmap on vs off
  # (RL_BITMAP=0)"（diff 为空）+ FORCEMERGE_OK + Post-merge search diff
  ```

  验收（Global Constraints 逐字）：EXIT=0；每次 CheckIndex 输出 "No problems"（11 次）；5× SEARCH_INTEROP_OK + 5× LOG_INTEROP_OK；`--bitmap` 变体内 Rust bitmap on/off searchdump diff 为空（croaring v3 读侧 vs PFOR 逐位一致——本里程碑最核心的对拍）。

- [ ] **Step 5.3: bench 索引（1M docs，seed 42，双路重建）**

  ```
  $ cargo build --release
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      logwrite /tmp/rl-bench5-rust 1000000 42 --bitmap
  → WROTE docs=1000000 elapsed_ms=... docs_per_sec=...
  $ CP="interop/java/classes:interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar"
  $ make java-classes   # interop/java/classes 不在时
  $ java -cp "$CP" JavaLogBench /tmp/rl-bench5-java 1000000 1 42
  → BENCH elapsed_ms=... docs_per_sec=...
  ```

- [ ] **Step 5.4: 查询文件 + df 守卫**（Java 侧 dump，同 M3/M4）

  ```
  $ java -cp "$CP" SearchBench /tmp/rl-bench5-java message \
      --dump-queries .superpowers/sdd/m5-q-raw.txt --tasks 50 --seed 42   # 预期 498 行
  $ awk -F'\t' '!($1=="TERM" && $4<4096)' .superpowers/sdd/m5-q-raw.txt > .superpowers/sdd/m5-q.txt
  $ wc -l .superpowers/sdd/m5-q.txt   # 预期 493（剔除同样 5 条 med 截断词 conne/buffer/chec/buffe/evict）
  $ awk -F'\t' '$1=="TERM" && $4<4096' .superpowers/sdd/m5-q.txt | wc -l   # 0（ALL TERM LINES df>=4096）
  $ grep -c $'^AND\thigh\t' .superpowers/sdd/m5-q.txt   # 50
  ```

- [ ] **Step 5.5: 三路 bench + hit-counts 对拍**（串行）

  ```
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchbench /tmp/rl-bench5-rust message \
      --load-queries .superpowers/sdd/m5-q.txt --warmup 10 --iter 30 \
      > .superpowers/sdd/m5-bench-rust-roaring.out 2> .superpowers/sdd/m5-counts-rust-roaring.txt
  $ RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchbench /tmp/rl-bench5-rust message \
      --load-queries .superpowers/sdd/m5-q.txt --warmup 10 --iter 30 \
      > .superpowers/sdd/m5-bench-rust-pfor.out 2> .superpowers/sdd/m5-counts-rust-pfor.txt
  $ java -cp "$CP" SearchBench /tmp/rl-bench5-java message \
      --load-queries .superpowers/sdd/m5-q.txt --no-cache --warmup 10 --iter 30 \
      > .superpowers/sdd/m5-bench-java.out 2> .superpowers/sdd/m5-counts-java.txt
  ```

  对拍（口径同 M3 报告 §5 / M4 Step 5.5）：

  ```
  $ diff .superpowers/sdd/m5-counts-rust-roaring.txt .superpowers/sdd/m5-counts-rust-pfor.txt \
      && echo "COUNTS_MATCH: roaring == pfor"        # 门槛 1：plain diff 必须为空
  # 门槛 2（Java 等效对拍）：stderr 去 JVM 噪声/空行/注释行 → m5-counts-java.clean.txt；
  #   diff <(grep -v '^term=' c-rust | sort) <(grep -v '^term=' c-java | sort)  → 空
  #   term= 重合行 awk join → mismatches=0
  ```

  预期（spec §5 逐字）：稀疏 AND roaring/pfor **≥2x**（M4 0.33x → 7.7µs 级 count + 物化迭代）、term ~1.0x、iterm ≥1x；roaring/java 全面 ≥1（croaring 快 Java RoaringBitmap 25–65x，REPORT §vs）。

- [ ] **Step 5.6: 稠密 level 字段 A/B**（同 binary RL_BITMAP on/off；spec §5 "稠密 ≥8x"）

  ```
  $ printf 'TERM\thigh\tINFO\t200000\nTERM\thigh\tWARN\t200000\nTERM\thigh\tERROR\t200000\nAND\thigh\tINFO\tWARN\nAND\thigh\tERROR\tWARN\nOR\thigh\tINFO\tWARN\nOR\thigh\tERROR\tWARN\n' > /tmp/q-level.txt
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchbench /tmp/rl-bench5-rust level --load-queries /tmp/q-level.txt --warmup 5 --iter 20 \
      > .superpowers/sdd/m5-dense-bench-rust-roaring.out 2>&1
  $ RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchbench /tmp/rl-bench5-rust level --load-queries /tmp/q-level.txt --warmup 5 --iter 20 \
      > .superpowers/sdd/m5-dense-bench-rust-pfor.out 2>&1
  ```

  预期：and/or **≥8x**（M3 容器折叠曾 8.7x/15.2x；croaring dense and_cardinality 4.2µs + 物化 157µs + 迭代，spec §5）。

- [ ] **Step 5.7: df-skew 微基准 → SKEW_RATIO 标定**（scratch crate `/tmp/m5skew`，path-dep 只读引用 `codec-lucene9` + `rustlucene-core`，repo 零改动——同 M4 `/tmp/m4skew` 先例，`.superpowers/sdd/task-5-report.md:99`）

  **索引**（一次性，单 segment 1M docs，text 字段 `f` 空白分词无 positions；`IndexWriterConfig{bitmap:true}`，一次 commit）：嵌套 df 阶梯 term `k0..k8`，df ∈ {4096, 8192, 16384, 32768, 65536, 131072, 262144, 524288, 1000000}，term `ki` 的 docs = [0, df_i)。嵌套 ⇒ AND(k0, ki) 精确 = 4096（正确性锚点），ratio r = df_i/4096 ∈ {1, 2, 4, 8, 16, 32, 64, 128, 244}。（v3 下 region 是 frozen 格式；阶梯 term docs 连续 ⇒ run containers，region 极小——这正是偏斜场景的良性形态；另加一组 `sj` 散点 term（`i*stride % 1M`，array/bitset containers）做第二梯队，防 run-only 失真。）

  **测量**（每对 (k0, ki)，两策略都用 T2/T3 的真实 pub API，与查询路径 1:1 对应；经 `TermsDict`/`PostingsReader` pub API 定位 term，`open_term_bitmap` 打开）：

  - **A = 物化 fold**（T3 非偏斜路径）：`intersect_docs(&[&a, &b])` 全长迭代计数；
  - **B = skew probe**（T3 偏斜路径）：k0 `docs_from` 批量迭代 + ki `contains(doc)` 逐候选过滤计数；
  - **C = cardinality**（参考行，非候选策略）：`and_cardinality(&a, &b)`——count 快路径成本背景；
  - 计时口径同 M4（warmup 10 + 按 0.25s 标定 reps 取均值）；**每对先断言 A == B == C == 4096** 再计时。

  **判定规则**：交叉点 r*（B 开始稳定胜 A 的最小 r，±10% 噪声带）→ `SKEW_RATIO` 取 ≤ r* 的最近 2 的幂；A 全胜（含 r=244）→ `SKEW_RATIO = 256`（门坐到阶梯max之上，同 M4 处置，注释注明"croaring fold 在全部可测比率胜出"）。改常量单独提交 `git commit -m "bench: calibrate SKEW_RATIO=<n> from m5 skew micro crossover"` 并复跑 `cargo test -p rustlucene-core`（改动仅常量一行，电池不必复跑——偏斜/非偏斜两路径正确性由 `and_skew_probe_matches_pfor` 与三档测试双向覆盖，与阈值取值无关）。

  **标定先验**（`/tmp/croaring-probe/REPORT.md` §Bench）：contains 8.5–28.7 ns/probe；物化 and+card sparse 10.2µs / dense 157µs / run 1.4µs；iter 4.6–7.3 ns/doc。B 成本 ≈ 4096×(iter ~5ns + contains ~10-29ns) ≈ 60–140µs；A 成本 ≈ 物化（1.4–157µs 视 container 形态）+ 4096×~7ns ≈ 30–190µs——交叉点存在性不定，实测定夺（A 全胜不意外，关键设计事实 8）。

  输出 `.superpowers/sdd/m5-skew-micro.txt`（ratio × A µs × B µs × C µs × 胜者表，run 阶梯 + 散点阶梯两段）。

- [ ] **Step 5.8: 写侧吞吐 + `Bitmap::of` 开销 + 磁盘**（命令同 M4 Step 5.8；产物 `m5-write-throughput.txt`）

  ```
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- logwrite /tmp/rl-bench5-wt-bm 1000000 42 --bitmap
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- logwrite /tmp/rl-bench5-wt-off 1000000 42
  $ du -sb /tmp/rl-bench5-wt-bm /tmp/rl-bench5-wt-off
  ```

  写侧开销量化（spec §2/§5 "Bitmap::of ~5.7ns/doc，写侧开销 bench 量化"）：① on/off logwrite 吞吐差对照 M3 的 0.78–2.4% 与 M4 实测；② 探针先验 `Bitmap::of` 107µs @ 18.8k docs ≈ 5.7ns/doc（+ `run_optimize`/`shrink_to_fit`/Frozen 序列化，T1 写路径逐字）；③ 磁盘增量对照 M3 的 +15.0%（v3 frozen 与 v2 自研 payload 尺寸差异逐 region 报告——同 container 语义，预期同量级）。

- [ ] **Step 5.9: 报告落盘** `.superpowers/sdd/m5-bench-report.md`（gitignored，不提交），骨架：

  1. 口径（HEAD、语料、缓存、查询集、bench 参数、host/toolchain——Step 5.3-5.5 逐字命令）；
  2. 三路对比表（term/and/or/iterm × high/med，qps/p50/p90/p99；附 M4/M3 基线列对照）；
  3. **spec §5 预期逐条对照**：稀疏 AND ≥2x PFOR？、稠密 ≥8x？、term count ~1.0x？、iterm ≥1x？——逐条给实测值与结论；
  4. skew 标定表 + `SKEW_RATIO` 终值与依据（Step 5.7 输出，两阶梯）；
  5. 写侧（吞吐/磁盘/`Bitmap::of` 开销，M3/M4 对照）；
  6. 正确性证据（COUNTS_MATCH 三路、电池 11× "No problems"、v2 落档两个测试名、fmt、`cargo check --workspace`）；
  7. 附：全部命令与产物文件清单。

- [ ] **Step 5.10: 收尾验收**

  ```
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ cargo test -p codec-lucene9 2>&1 | tail -2
  test result: ok. 152 passed; 0 failed; 1 ignored; ...
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. 46 passed; 0 failed; ...
  $ cargo check --workspace 2>&1 | tail -1   # Finished（含 jni-binding）
  $ git status --short   # 仅任务开始前就存在的未跟踪文件；.superpowers/ ignored（!!）
  $ git log --oneline -6  # T1-T4 四个 feat: commit（+ 可选 bench: SKEW_RATIO 标定）；报告不进 git
  ```

---
