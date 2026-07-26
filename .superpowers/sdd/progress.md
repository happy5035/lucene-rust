# M6/M7 进度 ledger

## Task A: 嵌套 Boolean 查询 —— 完成

- 状态：DONE_WITH_CONCERNS
- 实现范围：Occur 三态、Query::Bool、flatten_bool 拍平、ConjOver/DisjOver/Excluding 通用组合器、Searcher::count Bool 分支、freq_sum 拒绝 Bool、rustlucene-cli/SearchBench BOOL 行解析与 searchbench 重放、searchdump/VerifySearchIndex Bool 电池。
- 验收：`cargo test -p codec-lucene9` 167 passed / `cargo test -p rustlucene-core` 59 passed + 2 bin tests passed；`make log-test` 5 变体全绿；searchbench BOOL 行三路 hit-counts diff 为空。
- Concern：brief Step 10 给出的 Java `parseBoolSexpr` 中 `NOT` 与纯 MUST_NOT `BOOL` 节点直接生成 Lucene 纯 MUST_NOT BooleanQuery，该查询在 Lucene 语义下返回 0 文档，与 Rust 侧 `Bool[MustNot x] = MatchAll − x` 不等价，导致 searchdump/VerifySearchIndex diff 失败。已按 Rust 语义在 Java 侧补入 `MatchAllDocsQuery` 正集，diff 通过并 green。此为对 brief 字面代码的一处必要修正。
Task A: complete (commits b8d8ae9..b484277, review clean)
  verified: cargo fmt clean; codec-lucene9 167 passed / 1 ignored; rustlucene-core 59 passed (+ 2 bin tests); make log-test → LOG_INTEROP_OK + SEARCH_INTEROP_OK (含 BOOL 行输出)
  deferred Minor: bool_segment_count 单 term flat Bool 未走 doc_freq 捷径；RUST_MIN_STACK=4M 项目级；SearchBench loadedBool 命名略混
Task C: complete (commits b484277..95dc18b, review clean after two fix rounds)
  verified: cargo fmt clean; codec-lucene9 179 passed / 1 ignored; rustlucene-core 74 passed; make log-test 7 variants green; FORCEMERGE_INTEROP_OK + bitmap A/B diff empty
  important fixes resolved: orphan partial files cleanup on failure; DocValuesReader corrupt input returns InvalidData instead of panic; cleanup errors propagated
  deferred Minor: cleanup error swallowed in two abort paths to preserve original error (acceptable best-effort); future hardening of other usize casts in doc_values_read.rs; postings::file_name widened to pub
M6 Task D: complete (commits 95dc18b..182c3b8)
  verified: cargo fmt clean; codec-lucene9 179 passed / 1 ignored; rustlucene-core 75 passed; make log-test 7 variants green; bench re-run roaring/pfor within ±15% of M5, counts diff empty
  final whole-branch review: Approved with fixes; applied fix: force_merge stale deletion best-effort after successful commit
  deferred Minor: cleanup error swallowed in abort paths (acceptable); RUST_MIN_STACK=4M global; SearchBench loadedBool naming; doc_values_read.rs other usize casts; postings::file_name pub; cleanup_segment_files prefix broad; no fsync after stale deletion

## M7 进度

Task A1 (T-A1): complete (commits 5b3f1a6..585f641, review clean)
  verified: rustlucene-core 76 passed; behavior-preserving two-phase protocol skeleton
Task A2 (T-A2): complete (commits 585f641..62f4653, review clean)
  verified: rustlucene-core 77 passed; PhraseDocIter two-phase split with necessary initial-lead-positioning fix
Task A3 (T-A3): complete (commits 62f4653..27823b7, review clean)
  verified: rustlucene-core 78 passed; Phrase bitmap approximation fast path with roaring AND, postings fallback
Task B1 (T-B1): complete (commits 27823b7..b90dddc, review clean)
  verified: codec-lucene9 182 passed; MaterializedBitmap and/or/andnot/full + FrozenBitmap::to_materialized
Task B2 (T-B2): complete (commits b90dddc..37651be, review clean)
  verified: rustlucene-core 80 passed; generic Bool count bitmap fold with cost guard, A/B equivalence pinned
Task D (T-D): complete (commits 37651be..3b940a2, review clean)
  verified: rustlucene-core 81 passed; Top-N early termination with fast_segment_count, count/topN separation, byte-equivalent pinning test
Task C (T-C): complete (commits 3b940a2..f8781f6, review clean after fix)
  verified: rustlucene-core 81 passed / 1 ignored; OR heap adopted at k≥32 (≥20% threshold), bench restored

Task 8 (M7 终验): DONE
  verified:
    - cargo test -p codec-lucene9 --lib → 182 passed / 1 ignored
    - RUST_MIN_STACK=4194304 cargo test -p rustlucene-core --lib → 81 passed / 1 ignored
    - make log-test → 7 variants green, 15× INTEROP_OK (SEARCH_INTEROP_OK / LOG_INTEROP_OK / FORCEMERGE_INTEROP_OK)
    - searchbench three-way (1M docs, --positions, m7-q.txt):
      * roaring vs pfor counts diff → empty (COUNTS_R_P_OK)
      * roaring vs java non-term+iterm+phrase+bool counts diff → empty
      * Java side --no-cache discipline observed
      * Rust phrase high/med QPS = 2013.5 / 4732.9 (roaring), 2036.8 / 4850.7 (pfor)
      * Rust bool high/med QPS = 1358.8 / 2446.2 (roaring), 1395.3 / 2512.7 (pfor)
      * roaring/pfor phrase & bool QPS within ±3%, no regression between bitmap modes
  note: pre-M7 main baseline for phrase/bool count QPS is unavailable (phrase two-phase/bitmap and generic Bool count fold were introduced in M7); regression vs main could not be measured and is noted in the task report.
  deferred Minor (carried forward): cleanup error swallowed in abort paths (acceptable best-effort); RUST_MIN_STACK=4M global; SearchBench loadedBool naming; doc_values_read.rs other usize casts; postings::file_name pub; cleanup_segment_files prefix broad; no fsync after stale deletion; DisjOverHeapDocIter/DisjOverLinearDocIter dead-code warnings in release build.

M7 final verification: complete (commits f8781f6..371d2f0, final whole-branch review approved)
  verified: codec-lucene9 182 passed / 1 ignored; rustlucene-core 85 passed / 1 ignored; make log-test 7 variants green / 15 INTEROP_OK; searchbench three-way diff empty; release build clean
  critical fixes resolved: DisjOverDocIter::advance two-phase confirmation; ExcludingDocIter prohibited matches confirmation
Task 1: complete (commits d089782..2435f5d, review clean — I-1 fixed: Freqs arm freqs fill + decodes_freqs accessor; review r2 approved)
  deferred Minor: DocBlockBuf no Default derive; #[allow] attrs on reserved items (clean up when consumed)
  Task 2 需知: DocsFreqsEnum::decodes_freqs 已随 Task 1 fix 落地（postings_read.rs:699），勿重复添加
Task 2: complete (commits 2435f5d..50b3e1b, review clean, 279 green) — DONE_WITH_CONCERNS 裁决：brief 测试代码对 DOCS_AND_FREQS 字段误用 reader.docs()（per-doc 路径同样损坏，证明是测试构造器误用而非生产 bug），实现者拆分为 docs()/docs_and_freqs_no_freq()/docs_and_freqs() 三路，对拍意图完整保留且更强，裁决接受
  deferred Minor: copy 循环可换 copy_from_slice（cosmetic）; 无 df=exact-128 测试词（take==0 经 step=4096 间接覆盖）
Task 3: complete (commits 50b3e1b..0bb726d, review clean, 281 green) — DONE_WITH_CONCERNS 裁决：brief Step 3 next_many_to 的 next_from 更新时机 bug（中循环 refill 时 stale → 重叠重拉；末批 <128 docs 时真实可触发，非纯防御），实现者修正为 refill 前更新，裁决接受；BitsetDocIter 保持默认 fill 为 spec 已载偏差
  deferred Minor: Roaring 叶子无直测（FrozenBitmap 难构造，Task 8 电池覆盖；cursor 逻辑经 Materialized 泛型同测）
Task 4: complete (commits 0bb726d..50c02a3, review clean r2, 284 green) — I-1 fixed: consumed 坐标直钉测试（andnot 实际坐标 (3,1,2)：ia+=1 无条件消费过产出元素，续跑验证不丢不重）
  deferred Minor: kway_union 线性找 min（k 小，Phase 2 profile 定）
  deferred Minor: kway_union 线性找 min（k 小，Phase 2 profile 再定）；andnot resume 仅间接覆盖（直钉 tuple 已够）
Task 5: WIP checkpoint (commit 见下条) — 交接给另一 agent 完成
  状态: Excl/ConjOver 覆写 + BlockCursor + position_seg 已落地且对拍绿；DisjOverDocIter::next_block k 路块归并乱序（2 测试红）
  红测试: block_tests::combinator_block_vs_per_doc_random（"disj round=0"）+ block_tests::disj_debug_round0（复现用，agent 调试时加的，可保留可并）
  症状: 升序流到某点后，某 child 的剩余块被整体先行产出再回落（refill 后 heads/consumed 簿记错误疑似——kway_union 契约：consumed 每轮调用前清零，heads 传各游标未消费余量）
  接手清单: 修 DisjOver → cargo test 全绿（284+2）→ 走 task-5 评审（brief/report 在 .superpowers/sdd/）→ T6-T10 继续（plan: docs/superpowers/plans/2026-07-26-batch-iter.md）
  T1-T4 已完成且评审干净（2435f5d / 50b3e1b / 0bb726d / 50c02a3）
提交纪律追加（用户指示）：.superpowers/sdd/ 已纳入版本控制（根 .gitignore 改为 .superpowers/* + !.superpowers/sdd/）——T5-T10 每个任务的 brief/report/review/review-package 与代码同提交，不再留本地 scratch
