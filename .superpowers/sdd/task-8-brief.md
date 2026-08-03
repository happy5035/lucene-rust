### Task 8: 端到端测试 + README + 回归验证

**Files:**
- Modify: `crates/core/src/analysis/query_analysis.rs`（测试模块追加 E2E）
- Modify: `README.md`（字段类型、schema spec 语法、范围与限制）

**Interfaces:**
- Consumes: Task 5/6 全部
- Produces: 写入→分析→搜索闭环证明；回归证据（log-test / cargo test / log-bench）

- [ ] **Step 1: Write the E2E test**

`crates/core/src/analysis/query_analysis.rs` 测试模块追加（先通读 `crates/core/src/index_writer.rs:799-822` 的既有测试，复用其目录/写法约定——下面代码中 `IndexWriter::create`、`add_document`、`search` 的签名以该处实际为准）：

```rust
#[test]
fn lowercase_write_search_roundtrip() {
    use crate::index_writer::{IndexWriter, IndexWriterConfig};
    use crate::{Document, FieldValue};

    let dir = std::env::temp_dir().join(format!("rl-analyzer-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (schema, _, _) =
        Schema::parse("message:text+positions+analyzer=whitespace|lowercase,level:keyword")
            .unwrap();
    let mut w = IndexWriter::create(&dir, schema, IndexWriterConfig::default()).unwrap();
    let mut doc = Document::new();
    doc.add("message", FieldValue::Text("ERROR Failed error".to_string()));
    doc.add("level", FieldValue::Keyword("ERROR".to_string()));
    w.add_document(doc).unwrap();

    // analyzed field: mixed-case query hits lowercase-normalized index
    let q = analyze_query(&Query::term("message", "error"), w.schema()).unwrap();
    assert_eq!(w.search(&q, None, 10).unwrap().total, 1);

    // terms IN through the same rewrite
    let q = analyze_query(&Query::terms("level", &["ERROR", "WARN"]), w.schema()).unwrap();
    assert_eq!(w.search(&q, None, 10).unwrap().total, 1);

    // keyword field keeps exact-case semantics (no analyzer configured)
    assert_eq!(w.search(&Query::term("level", "error"), None, 10).unwrap().total, 0);
    assert_eq!(w.search(&Query::term("level", "ERROR"), None, 10).unwrap().total, 1);

    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Run E2E test**

Run: `cargo test -p rustlucene-core analysis::query_analysis`
Expected: PASS

- [ ] **Step 3: README 更新**

`README.md` 三处最小修改：

1. 「字段类型」表后追加一行说明：

```markdown
| 分析器 | per-field analyzer（`analyzer=whitespace\|lowercase`） | whitespace / letter / keyword tokenizer + lowercase filter，注册表可扩展；索引与查询双通道归一化 |
```

（注意表格里 `|` 需转义为 `\|`，与文件内既有写法保持一致——先看该表现有单元格如何处理竖线。）

2. 「JSON 绑定层」一行的 schema spec 说明补 `analyzer=` modifier：

`name:type+mods[@json键]` 的 mods 列表补充：`text` 类型可配 `analyzer=whitespace|lowercase`。

3. 「范围与限制」中把「暂不支持」列表里如有"大小写归一化/分析器"相关描述则更新；并在搜索读路径一节注明：查询侧分析只在 JNI `nativeSearch` 生效，CLI/磁盘 Searcher 直查需调用方自行归一化。

同时更新 `crates/core/src/json.rs` 文件头 doc comment（line 6-9 的 schema spec 说明）补 `analyzer=` modifier 一句。

- [ ] **Step 4: 全量回归**

```bash
cargo test --workspace
make log-test
```

Expected: `cargo test` 全绿（codec 192 + core 128+N + metric 48 + jni 若干）；`make log-test` 五变体 11 次 "No problems"（默认路径字节级不变的硬验证）。

- [ ] **Step 5: 写入吞吐基线**

```bash
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- logwrite /tmp/idx-analyzer-bench 1000000 42
```

对比 README 基线（单线程 182k docs/s 档）：无 analyzer 路径预期持平（±5% 内）；超出则排查 helper 内联（必要时给 `index_token` 加 `#[inline]`）。结果记一行到 commit message。

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/analysis/query_analysis.rs README.md crates/core/src/json.rs
git commit -m "core: analyzer e2e test + docs; log-test regression green, logwrite throughput <实测值>"
```
