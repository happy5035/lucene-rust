# Task 6 报告：查询侧 `analyze_query` 双通道重写

## 状态

DONE

## 变更文件

- 新建 `crates/core/src/analysis/query_analysis.rs`：`pub fn analyze_query(query: &Query, schema: &Schema) -> Result<Query, String>` + 7 个单元测试。
- 修改 `crates/core/src/analysis/mod.rs`：`mod query_analysis;` + `pub use query_analysis::analyze_query;`。

实现代码逐字采用 brief（含模块文档注释）。无新依赖，`#![forbid(unsafe_code)]` 不受影响。

## TDD 过程

1. 先写测试模块（含 brief Step 1 全部 6 条 + 补充的 letter 0-token 场景 `term_with_zero_tokens_is_an_error`），`mod.rs` 只挂 `mod query_analysis;`。
2. `RUST_MIN_STACK=4194304 cargo test -p rustlucene-core analysis` → 编译失败（`analyze_query` 不存在，E0425/E0433），确认红灯。
3. 写入实现 + `pub use` re-export → `cargo test -p rustlucene-core analysis`：18 passed / 0 failed（其中本任务 7 条全绿）。

## 重写规则自审（对照 brief 规则表）

- Term 0 token → Err：`term_with_zero_tokens_is_an_error`（`letter` analyzer 对 `"!!!"` 产出 0 token）✓
- Term 1 token → 替换：`term_is_lowercased_for_analyzed_field` ✓
- Term 多 token → Bool SHOULD：`term_with_multiple_tokens_becomes_bool_should` ✓
- Terms/And/Or/Phrase 每个 value 恰好 1 token 否则 Err：`terms_and_phrase_require_exactly_one_token_per_value`（Terms/Phrase 显式测；And/Or 走同一 `analyze_each` 路径）✓
- Prefix/Wildcard 走 `normalize` 不 tokenize：`prefix_and_wildcard_are_normalized_not_tokenized`；Wildcard 经 `Query::wildcard` 构造函数重建（DFA 随新 pattern 重编译，未直接改字段）✓
- Bool 递归重写子查询、Occur 保留：`bool_recurses_and_other_variants_pass_through` ✓
- MatchAll/PointRange 原样透传：同上测试 ✓
- 无 analyzer 字段字节级透传：`term_passes_through_for_plain_field`（`level` 无 analyzer）；`analyze_each` 的 None 分支返回 `values.to_vec()` ✓

## 验证

- `RUST_MIN_STACK=4194304 cargo test -p rustlucene-core`：**186 passed, 0 failed, 1 ignored**（lib）+ 其余 target 全绿；既有测试零改动。

## 提交

- commit message 按 brief：`core: query-side dual-channel analysis (analyze_query)`
- 包含：`crates/core/src/analysis/{query_analysis.rs,mod.rs}`、`.superpowers/sdd/task-6-brief.md`、本报告（`git add -f`）。

## 备注 / 疑虑

- 无。调用方（JNI search 入口）接入属于后续任务，不在本任务范围。
