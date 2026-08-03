### Task 7: JNI `terms` 类型 + nativeSearch 接线 analyze_query

**Files:**
- Modify: `crates/jni-binding/src/query_parser.rs`（QuerySpec 加 Terms）
- Modify: `crates/jni-binding/src/lib.rs:354-365`（nativeSearch 调 analyze_query）

**Interfaces:**
- Consumes: Task 6 `rustlucene_core::analysis::analyze_query`、`Query::terms(field, &[&str])`
- Produces:
  - JSON：`{"query":{"type":"terms","field":"level","values":["ERROR","WARN"]}}`
  - `nativeSearch` 对所有查询在 `spec_to_query` 之后执行 analyze_query 重写

- [ ] **Step 1: Write the failing test**

`crates/jni-binding/src/query_parser.rs` 测试模块追加：

```rust
#[test]
fn parse_terms_query() {
    let json = br#"{"query":{"type":"terms","field":"level","values":["ERROR","WARN"]},"top_n":5}"#;
    let req = parse_search_request(json).unwrap();
    assert_eq!(req.to_query().unwrap(), Query::terms("level", &["ERROR", "WARN"]));
}

#[test]
fn terms_nested_in_bool() {
    let json = br#"{"query":{"type":"bool","clauses":[
        {"occur":"must","query":{"type":"terms","field":"level","values":["ERROR"]}},
        {"occur":"must","query":{"type":"match_all"}}
    ]}}"#;
    let req = parse_search_request(json).unwrap();
    match req.to_query().unwrap() {
        Query::Bool { clauses } => {
            assert_eq!(clauses.len(), 2);
            assert_eq!(clauses[0].1, Query::terms("level", &["ERROR"]));
        }
        _ => panic!("expected Bool"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rustlucene-jni-binding 2>&1 | tail -5`（包名以 `cargo metadata --no-deps --format-version 1 | jq -r '.packages[].name'` 确认为准）
Expected: FAIL（`type":"terms"` 反序列化失败：`unknown variant`）

- [ ] **Step 3: Write minimal implementation**

`crates/jni-binding/src/query_parser.rs` 的 `QuerySpec`（line 35-43）增加变体：

```rust
    Terms { field: String, values: Vec<String> },
```

`spec_to_query` 增加分支（放在 `QuerySpec::Term` 分支后）：

```rust
        QuerySpec::Terms { field, values } => {
            let refs: Vec<&str> = values.iter().map(String::as_str).collect();
            Ok(Query::terms(field, &refs))
        }
```

`crates/jni-binding/src/lib.rs` 的 `nativeSearch`（line 354-365）改为：

```rust
    let req = jni_try_obj!(&mut env, query_parser::parse_search_request(&json));
    let query = jni_try_obj!(&mut env, req.to_query());
    let sort_field = req.sort_field();

    let guard = h.index.read().unwrap();
    // Query-side analysis (spec §查询侧双通道): rewrite term bytes to match
    // analyzer-configured fields before execution. No-analyzer fields and
    // indexes built without analyzers pass through unchanged.
    let query = jni_try_obj!(
        &mut env,
        rustlucene_core::analysis::analyze_query(&query, guard.schema())
    );
    let results = jni_try_obj!(
        &mut env,
        guard
            .search(&query, sort_field, req.top_n)
            .map_err(|e| e.to_string())
    );
    drop(guard);
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rustlucene-jni-binding`
Expected: PASS（新增 2 项 + 现有 query_parser 测试全绿）

- [ ] **Step 5: Commit**

```bash
git add crates/jni-binding/src/query_parser.rs crates/jni-binding/src/lib.rs
git commit -m "jni: terms (IN) query type + analyze_query wiring in nativeSearch"
```

---

