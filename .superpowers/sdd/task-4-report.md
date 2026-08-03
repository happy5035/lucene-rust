# Task 4 报告：FieldSpec.analyzer + Schema 校验 + spec 字符串语法

## 状态

DONE

## 变更文件

- `crates/core/src/schema.rs`
- `crates/core/src/json.rs`

## TDD 过程

1. **先写失败测试**：json.rs 测试模块追加 `analyzer_modifier_attaches_to_text_fields` 和 `analyzer_modifier_rejected_on_non_text_and_unknown_components`（按 brief 逐字）；schema.rs 新建 `#[cfg(test)] mod tests`，含 `analyzer_only_on_indexed_tokenized_fields`（catch_unwind 验证 panic 拒绝）和 `with_analyzer_roundtrip`。
2. **确认失败**：`RUST_MIN_STACK=4194304 cargo test -p rustlucene-core` 编译失败（E0599 method not found / E0609 no field `analyzer`），符合预期。
3. **最小实现**：
   - `FieldSpec` 新增 `pub analyzer: Option<String>`（含 brief 给的 doc comment）。
   - `text()`、`keyword()`、`base()` 三个直接构造函数补 `analyzer: None`；其余构造函数经 `..Self::text(name)` / `..Self::base(name)` 自动继承。
   - `with_analyzer(mut self, spec: &str) -> Self` 放在 `with_stored` 之后（逐字）。
   - `Schema::add` 在 `is_indexed()` assert 块之后追加 analyzer 校验（逐字）：非 tokenized/非 indexed 字段带 analyzer → assert panic；`Analyzer::parse` 失败 → panic。
   - `json.rs` `Schema::parse`：在 `let has = ...` 之后、match 之前提取 `analyzer=` modifier；非 text 类型携带时返回 `Err`；`"text"` 分支按 brief 改写；在 match 之后、**`schema.add(spec);` 之前**插入显式 `Analyzer::parse` 校验，失败返回 `Err`（不走 `Schema::add` 的 panic 路径，JNI 侧 fail fast）。
4. **确认通过**：全量测试绿。

## 与 brief 的偏差

一处：`analyzer_only_on_indexed_tokenized_fields` 测试中 brief 写的是
`let (result, _) = { ... catch_unwind(...) };`，但 `catch_unwind` 返回
`Result<(), Box<dyn Any + Send>>`，不能按元组解构（E0308 编译错误）。已最小修正为
`let result = { ... };`，测试语义（catch_unwind + AssertUnwindSafe + 断言 is_err）与 brief 完全一致。其余代码均逐字采用 brief。

## 验证

```
RUST_MIN_STACK=4194304 cargo test -p rustlucene-core
```

结果：177 passed; 0 failed; 1 ignored（lib）+ 其余 target 全绿。新增 4 个测试全部通过：

- `json::tests::analyzer_modifier_attaches_to_text_fields`
- `json::tests::analyzer_modifier_rejected_on_non_text_and_unknown_components`
- `schema::tests::analyzer_only_on_indexed_tokenized_fields`
- `schema::tests::with_analyzer_roundtrip`

既有测试零改动，无失败。3 个 dead-code warning（`pattern_chars`/`matches`/`glob_match`）为既有代码遗留，与本任务无关；无新依赖，`#![forbid(unsafe_code)]` 未受影响。

## 疑虑

无。
