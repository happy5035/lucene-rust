### Task 4: FieldSpec.analyzer + Schema 校验 + spec 字符串语法

**Files:**
- Modify: `crates/core/src/schema.rs`（FieldSpec 加字段、构造函数、`Schema::add` 校验、`with_analyzer`）
- Modify: `crates/core/src/json.rs:229-263`（`Schema::parse` 的 `analyzer=` modifier）

**Interfaces:**
- Consumes: Task 3 的 `Analyzer::parse`
- Produces:
  - `FieldSpec.analyzer: Option<String>`（规格文本，如 `"whitespace|lowercase"`）
  - `FieldSpec::with_analyzer(self, spec: &str) -> Self`
  - spec 语法：`message:text+positions+analyzer=whitespace|lowercase`（`analyzer=` 只对 `text` 类型合法）

- [ ] **Step 1: Write the failing test**

`crates/core/src/json.rs` 测试模块追加：

```rust
#[test]
fn analyzer_modifier_attaches_to_text_fields() {
    let (s, _, _) = Schema::parse("message:text+positions+analyzer=whitespace|lowercase").unwrap();
    assert_eq!(
        s.get("message").unwrap().analyzer.as_deref(),
        Some("whitespace|lowercase")
    );
    // plain text without analyzer keeps None (legacy behavior)
    let (s2, _, _) = Schema::parse("message:text").unwrap();
    assert!(s2.get("message").unwrap().analyzer.is_none());
}

#[test]
fn analyzer_modifier_rejected_on_non_text_and_unknown_components() {
    assert!(Schema::parse("level:keyword+analyzer=whitespace").is_err());
    assert!(Schema::parse("ts:longpoint+analyzer=whitespace").is_err());
    assert!(Schema::parse("message:text+analyzer=nosuchtok").is_err());
    assert!(Schema::parse("message:text+analyzer=whitespace|nosuchfilter").is_err());
}
```

`crates/core/src/schema.rs` 测试模块追加（若无测试模块则新建）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzer_only_on_indexed_tokenized_fields() {
        let (result, _) = {
            let mut s = Schema::new();
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                s.add(FieldSpec::keyword("level").with_analyzer("whitespace"));
            }))
        };
        assert!(result.is_err(), "keyword field with analyzer must be rejected");
    }

    #[test]
    fn with_analyzer_roundtrip() {
        let f = FieldSpec::text("message").with_analyzer("whitespace|lowercase");
        assert_eq!(f.analyzer.as_deref(), Some("whitespace|lowercase"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rustlucene-core schema json 2>&1 | tail -5`
Expected: FAIL（`analyzer` 字段 / `with_analyzer` 不存在）

- [ ] **Step 3: Write minimal implementation**

`crates/core/src/schema.rs`：

`FieldSpec` 增加字段（struct 定义处，约 line 20-32）：

```rust
    /// Analyzer spec text (e.g. "whitespace|lowercase"), only legal on
    /// indexed tokenized text fields. Not persisted — analyzer config is
    /// application-level, same as Lucene.
    pub analyzer: Option<String>,
```

各构造函数补 `analyzer: None`：`text()`、`keyword()`、`base()`（其余构造函数走 `..Self::base(name)` 自动继承）。

新增链式方法（放在 `with_stored` 之后）：

```rust
    /// Attaches an analyzer chain spec ("tokenizer|filter|...") to this
    /// field. Only valid on indexed tokenized text fields (enforced by
    /// `Schema::add`).
    pub fn with_analyzer(mut self, spec: &str) -> Self {
        self.analyzer = Some(spec.to_string());
        self
    }
```

`Schema::add` 的 `if spec.is_indexed()` assert 块之后追加：

```rust
        if let Some(a) = &spec.analyzer {
            assert!(
                spec.tokenized && spec.is_indexed(),
                "field {}: analyzer requires an indexed tokenized text field",
                spec.name
            );
            if let Err(e) = crate::analysis::Analyzer::parse(a) {
                panic!("field {}: invalid analyzer spec: {e}", spec.name);
            }
        }
```

`crates/core/src/json.rs` 的 `Schema::parse`（约 line 228-263）：在 `let has = ...` 之后、match 之前提取 analyzer modifier：

```rust
            let analyzer = modifiers
                .split('+')
                .find_map(|m| m.trim().strip_prefix("analyzer="));
            if analyzer.is_some() && ty != "text" {
                return Err(format!("field {name}: analyzer= is only valid on text fields"));
            }
```

`"text"` 分支改为：

```rust
                "text" => {
                    let mut s = if has("positions") {
                        FieldSpec::text_with_positions(name)
                    } else {
                        FieldSpec::text(name)
                    };
                    if let Some(a) = analyzer {
                        s = s.with_analyzer(a);
                    }
                    s
                }
```

（未知名称/非法链不能依赖 `Schema::add` 的 assert——`Schema::parse` 是 Result 风格且走 JNI，panic 不可接受。因此在 match 之后、**`schema.add(spec);` 之前**插入显式校验：）

```rust
            if let Some(a) = analyzer {
                crate::analysis::Analyzer::parse(a)
                    .map_err(|e| format!("field {name}: invalid analyzer spec: {e}"))?;
            }
```

注意：`Schema::add` 里对非法 analyzer 用 panic 是为了与现有 assert 风格一致（直接 Rust 调用方）；`Schema::parse` 路径必须先转成 `Err` 返回（JNI 侧 fail fast）。

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rustlucene-core`
Expected: PASS（新增 4 项 + 现有全绿）

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/schema.rs crates/core/src/json.rs
git commit -m "core: per-field analyzer spec on FieldSpec and schema spec syntax"
```

---

