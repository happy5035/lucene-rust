# Task 7 报告：JNI `terms` 查询类型 + nativeSearch 接线 analyze_query

## 状态：DONE

## 改动内容

### 1. `crates/jni-binding/src/query_parser.rs`
- `QuerySpec`（原 line 35-43）新增变体（按 brief 逐字）：
  ```rust
  Terms { field: String, values: Vec<String> },
  ```
  位置：紧跟 `Phrase` 之后、`MatchAll` 之前（`#[serde(tag = "type", rename_all = "snake_case")]` 使 `"type":"terms"` 自动映射）。
- `spec_to_query` 新增分支（按 brief 逐字，放在 `QuerySpec::Term` 分支后）：
  ```rust
  QuerySpec::Terms { field, values } => {
      let refs: Vec<&str> = values.iter().map(String::as_str).collect();
      Ok(Query::terms(field, &refs))
  }
  ```
- 测试模块追加 brief 逐字的两个测试：`parse_terms_query`（顶层 terms + top_n=5）、`terms_nested_in_bool`（terms 嵌套在 bool must 子句中）。

### 2. `crates/jni-binding/src/lib.rs`
`nativeSearch`（原 line 354-365）按 brief 逐字改造：在 `guard = h.index.read()` 之后、`guard.search` 之前插入 analyze_query 重写：

```rust
let guard = h.index.read().unwrap();
// Query-side analysis (spec §查询侧双通道): rewrite term bytes to match
// analyzer-configured fields before execution. No-analyzer fields and
// indexes built without analyzers pass through unchanged.
let query = jni_try_obj!(
    &mut env,
    rustlucene_core::analysis::analyze_query(&query, guard.schema())
);
let results = jni_try_obj!(...guard.search(&query, sort_field, req.top_n)...);
```

对所有查询类型统一生效；无 analyzer 的字段/索引由 `analyze_query` 内部原样透传。

## TDD 过程

1. **先失败**：只加两个测试后运行 `cargo test -p rustlucene-jni`，结果 `10 passed; 2 failed`——`parse_terms_query` 与 `terms_nested_in_bool` 均因 `unknown variant 'terms'` 反序列化失败，与 brief 预期一致。
2. **再实现**：加 `Terms` 变体 + `spec_to_query` 分支 + lib.rs 接线后，全部通过。

## 验证

- 包名确认：`cargo metadata --no-deps --format-version 1` 输出 `['rustlucene-core', 'codec-lucene9', 'rustlucene-jni', 'rustlucene-metric']`，jni 包名为 `rustlucene-jni`（brief 中的 `rustlucene-jni-binding` 不准确，以 metadata 为准）。
- `cargo test -p rustlucene-jni`：**12 passed; 0 failed**（既有 10 项 + 新增 2 项，既有测试零改动全绿）。
- `RUST_MIN_STACK=4194304 cargo test -p rustlucene-core`：**186 passed; 0 failed; 1 ignored**（lib）+ 集成测试 1 + 2 全绿。
- 无新依赖（`Cargo.toml` 未动）。

## 自审对照 brief

- [x] Step 1 失败测试：逐字追加，先跑确认失败（unknown variant）。
- [x] Step 2 失败确认：`2 failed`，原因符合预期。
- [x] Step 3 最小实现：`Terms` 变体与分支逐字；lib.rs 插入位置在 guard 获取之后、`guard.search` 之前，与 brief 一致。
- [x] Step 4 全部通过：新增 2 项 + 现有 query_parser 测试全绿。
- [x] Step 5 提交：commit message 用 brief 原文。
- [x] 硬约束：既有测试零改动；无新依赖。

## 疑虑

无。
