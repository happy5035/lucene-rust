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
Task 5: complete (commits 532844a..ca69dd4, review deferred to batch)
  verified: rustlucene-core 97 passed / 1 ignored; 40 轮随机 + 边界形状对拍全绿
  fixes: kway_union 增加 limit 参数（各游标块尾最小值）防跨块逆序；Excl block_andnot 限制 must 切片不超 prohibited 块尾
  deferred Minor: kway_union 线性找 min（k 小，Phase 2 profile 定）
Task 6: complete (commit db7d175)
  verified: rustlucene-core 98 passed / 1 ignored; RoaringOr slice 源 30 轮对拍
  实现: PostingsIter::next_block + BlockCursor::refill_postings + position_postings;
    ConjunctionDocIter/DisjunctionDocIter next_block（lazy priming）;
    DocSource::next_docs + SourceCursor + position_source;
    RoaringAndDocIter/RoaringOrDocIter next_block
Task 7: complete (commit fcac9cd)
  verified: rustlucene-core 100 passed / 1 ignored; collector 块/逐 doc 对拍
  实现: top_docs 块路径（block_enabled 门控）+ TopDocCollector/FreqSumCollector collect_block 覆写
Task 8: complete (1M 回归电池)
  verified: roaring + pfor 两路 block ON/OFF 逐 query hits diff 均为空（0 差异）
  数据: /tmp/regress-1m-*.tsv
Task 9: complete (5M 单段 bench)
  verified: 6 对 block ON/OFF diff 全空；Rust↔Java 非 term 桶 0 差异
  数据: /tmp/bench-5m-*.tsv; 索引 /tmp/boolbench-5m (1.3GB 单段)
  关键数字: roaring iterm 3.2× / bool 1.6× / or 1.7× / and 1.15×; PFOR bool 回归 0.33-0.52×
Task 10: complete (commit ccaaef4)
  报告 §12 已追加至 docs/bool-bench-report.md
  Phase 2 决策: 不做 SIMD；优先修 PFOR bool 回归（叶子真块）

提交纪律追加（用户指示）：.superpowers/sdd/ 已纳入版本控制（根 .gitignore 改为 .superpowers/* + !.superpowers/sdd/）——T5-T10 每个任务的 brief/report/review/review-package 与代码同提交，不再留本地 scratch
坑记录：superpowers 的 task-brief / review-package 脚本每次运行会在 .superpowers/sdd/ 重建 .gitignore（内容 *）——提交新产物前先 rm 该文件，或直接 git add -f

## Analyzer 框架（2026-07-31 plan: docs/superpowers/plans/2026-07-31-analyzer-framework.md）

Task 1: complete (commits 24e9066..1d917a8, review clean)
  verified: RUST_MIN_STACK=4M cargo test -p rustlucene-core → 168 passed（3 新 tokenizer 测试），既有测试零改动
  deferred Minor: lib.rs 模块列表字母序被原位替换打破；task-1-brief.md 混入代码 commit（流程噪音）
Task 2: complete (commits 1d917a8..6ab62c0, review clean)
  verified: RUST_MIN_STACK=4M cargo test -p rustlucene-core → 170 passed（analysis 8 项），既有测试零改动
  deferred Minor: LowercaseFilter 慢路径对纯 ASCII 含大写也两次分配（可 to_ascii_lowercase 特判，性能轮再处理）
Task 3: complete (commits 6ab62c0..d12656c, review clean)
  verified: RUST_MIN_STACK=4M cargo test -p rustlucene-core → 173 passed，既有测试零改动
  deferred Minor: 注册表锁中毒 unwrap；空 filter 名错误消息难看；测试污染全局注册表（唯一名前缀）；内置名可被注册但静默遮蔽
Task 4: complete (commits d12656c..b3d8f5e, review clean)
  verified: RUST_MIN_STACK=4M cargo test -p rustlucene-core → 177 passed（4 新），既有测试零改动
  偏差裁决: brief 测试 catch_unwind 解构笔误 → let result 最小修正，批准
  deferred Minor: catch_unwind 测试 panic 信息打 stderr；非索引字段+非法 spec 的 panic 分支无直测；parse/add 双路径校验重复（有意设计）
Task 5: complete (commits b3d8f5e..16527b1, review clean)
  verified: RUST_MIN_STACK=4M cargo test -p rustlucene-core → 179 passed（2 新），无 analyzer 分支与原循环逐语句等价（RAM/position/doc_count 审查逐项核对）
  deferred Minor: FieldBuf::new 不检查 tokenized 即编译 analyzer（依赖 Schema::add 兜底，仅浪费）；make log-test 字节级实证安排在 Task 8
Task 6: complete (commits 16527b1..576e3dd, review clean)
  verified: RUST_MIN_STACK=4M cargo test -p rustlucene-core → 186 passed（7 新），既有测试零改动
  deferred Minor: And/Or 的 Err 路径无专属测试（共享 analyze_each 间接覆盖）；normalize 产空串语义（filter 丢 token → 空前缀/空 pattern 而非 Err，内置 lowercase 不触发，设计文档可留一句）；Bool 嵌套递归无深度上限
Task 7: complete (commits 576e3dd..5b9d1cb, review clean)
  verified: cargo test -p rustlucene-jni 12/12（2 新），core 186 全绿；包名实为 rustlucene-jni（brief 笔误已注）
Task 8: complete (commits 5b9d1cb..c94aa28, review clean after fix)
  verified: cargo test --workspace 全绿（codec 218 + core 187 + metric 47 + jni）；make log-test exit=0，15× No problems / 15× INTEROP_OK；logwrite 183925 docs/s（+1.0% 持平）
  important fix resolved: E2E 首条断言缺索引侧判别性 → 补两条判别性断言（c94aa28）
  deferred Minor: terms IN 的 E2E 走 keyword 字段只证 plumbing（单测已覆盖真实重写）；report 中 log-test 计数拆分括号注释算错（头条数字可信）；E2E 临时目录 panic 时留残（开头预清理兜底）

最终全分支 review: approved (24e9066..c94aa28, "With fixes" → 修复后 Yes)
  important fixes resolved: LowercaseFilter ASCII 单分配特判（b5ec87e）；注册表拒绝内置名覆盖（b5ec87e）；spec/README 文档同步（5757dbc, 复审两处一行级补漏已修）
  deferred Minor 分诊: 全部可留（lib.rs 字母序、锁中毒 unwrap、空名消息、测试全局态、FieldBuf::new 浪费、And/Or Err 无专属测试、normalize 空串语义、Bool 递归无上限、E2E 目录留残、组件名字符集已文档化）
