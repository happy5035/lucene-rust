### Task 3: 名字注册表 + `Analyzer::parse`

**Files:**
- Create: `crates/core/src/analysis/registry.rs`
- Modify: `crates/core/src/analysis/mod.rs`

**Interfaces:**
- Consumes: Task 2 的 `Analyzer` / `TokenizerTemplate` / `FilterKind` / `TokenizerFactory` / `TokenFilter`
- Produces:
  - `Analyzer::parse(spec: &str) -> Result<Analyzer, String>`（`"tokenizer|filter|..."`）
  - `pub fn register_tokenizer(name: &str, factory: TokenizerFactory)`
  - `pub fn register_filter(name: &str, filter: Arc<dyn TokenFilter>)`

- [ ] **Step 1: Write the failing test**

`crates/core/src/analysis/registry.rs` 测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_builtin_chains() {
        let an = Analyzer::parse("whitespace|lowercase").unwrap();
        assert_eq!(an.normalize("ABC"), std::borrow::Cow::Borrowed("abc"));
        let an = Analyzer::parse("letter").unwrap();
        let toks: Vec<_> = an.analyze("a1b").map(|t| t.into_owned()).collect();
        assert_eq!(toks, vec![b"a".to_vec(), b"b".to_vec()]);
        let an = Analyzer::parse("keyword|lowercase").unwrap();
        let toks: Vec<_> = an.analyze("Hello World").map(|t| t.into_owned()).collect();
        assert_eq!(toks, vec![b"hello world".to_vec()]);
    }

    #[test]
    fn rejects_unknown_names_and_empty_specs() {
        assert!(Analyzer::parse("").is_err());
        assert!(Analyzer::parse("nosuchtok").is_err());
        assert!(Analyzer::parse("whitespace|nosuchfilter").is_err());
        // a tokenizer name in filter position is unknown there
        assert!(Analyzer::parse("whitespace|letter").is_err());
    }

    #[test]
    fn custom_components_register_by_name() {
        struct Reverse;
        impl crate::analysis::TokenFilter for Reverse {
            fn filter<'a>(&self, t: std::borrow::Cow<'a, [u8]>) -> Option<std::borrow::Cow<'a, [u8]>> {
                let mut v = t.into_owned();
                v.reverse();
                Some(std::borrow::Cow::Owned(v))
            }
            fn normalizes(&self) -> bool {
                false
            }
        }
        register_filter("reverse", std::sync::Arc::new(Reverse));
        let an = Analyzer::parse("whitespace|reverse").unwrap();
        let toks: Vec<_> = an.analyze("abc").map(|t| t.into_owned()).collect();
        assert_eq!(toks, vec![b"cba".to_vec()]);
        // normalizes() = false: normalize channel skips it
        assert_eq!(an.normalize("abc"), std::borrow::Cow::Borrowed("abc"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rustlucene-core analysis 2>&1 | tail -5`
Expected: FAIL（`Analyzer::parse` / `register_filter` 不存在）

- [ ] **Step 3: Write minimal implementation**

`crates/core/src/analysis/registry.rs`：

```rust
//! Name registry: built-in component names plus user-registered custom
//! components (spec §配置语法). Java/JNI only ever passes the spec string;
//! everything resolves here.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use super::analyzer::{Analyzer, FilterKind, TokenizerFactory, TokenizerTemplate};
use super::filter::TokenFilter;

fn custom_tokenizers() -> &'static RwLock<HashMap<String, TokenizerFactory>> {
    static REG: OnceLock<RwLock<HashMap<String, TokenizerFactory>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

fn custom_filters() -> &'static RwLock<HashMap<String, Arc<dyn TokenFilter>>> {
    static REG: OnceLock<RwLock<HashMap<String, Arc<dyn TokenFilter>>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Registers a custom tokenizer under `name` (spec string first component).
/// Must be called before any schema referencing the name is parsed.
pub fn register_tokenizer(name: &str, factory: TokenizerFactory) {
    custom_tokenizers()
        .write()
        .unwrap()
        .insert(name.to_string(), factory);
}

/// Registers a custom token filter under `name` (spec string 2nd+ component).
pub fn register_filter(name: &str, filter: Arc<dyn TokenFilter>) {
    custom_filters()
        .write()
        .unwrap()
        .insert(name.to_string(), filter);
}

impl Analyzer {
    /// Parses `"tokenizer|filter|filter..."`: first component must be a
    /// tokenizer, the rest filters; chain order is declaration order.
    pub fn parse(spec: &str) -> Result<Analyzer, String> {
        let mut parts = spec.split('|');
        let tok = parts
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "empty analyzer spec".to_string())?;
        let tokenizer = match tok {
            "whitespace" => TokenizerTemplate::Whitespace,
            "letter" => TokenizerTemplate::Letter,
            "keyword" => TokenizerTemplate::Keyword,
            other => custom_tokenizers()
                .read()
                .unwrap()
                .get(other)
                .cloned()
                .map(TokenizerTemplate::Custom)
                .ok_or_else(|| format!("unknown tokenizer: {other}"))?,
        };
        let mut filters = Vec::new();
        for name in parts {
            let f = match name {
                "lowercase" => FilterKind::Lowercase,
                other => custom_filters()
                    .read()
                    .unwrap()
                    .get(other)
                    .cloned()
                    .map(FilterKind::Custom)
                    .ok_or_else(|| format!("unknown token filter: {other}"))?,
            };
            filters.push(f);
        }
        Ok(Analyzer { tokenizer, filters })
    }
}
```

`crates/core/src/analysis/mod.rs` 增加：

```rust
mod registry;

pub use registry::{register_filter, register_tokenizer};
```

（`Analyzer::parse` 是 inherent impl，自动随 `Analyzer` 导出。）

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rustlucene-core analysis`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/analysis
git commit -m "core: analyzer name registry and Analyzer::parse"
```

---

