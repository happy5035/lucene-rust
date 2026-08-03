# Analyzer Framework Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 lucene-rust 引入 per-field analyzer 框架（whitespace/letter/keyword tokenizer + LowerCaseFilter + 名字注册表），查询侧双通道重写，JNI JSON 增加 `terms`（IN）查询类型。

**Architecture:** 方案 A（spec `docs/superpowers/specs/2026-07-31-analyzer-framework-design.md`）：内置组件编译成 enum 静态分发，`Custom` 变体挂 trait object 扩展；analyzer 是无状态模板（`&self` 即可 `analyze()`/`normalize()`，每次调用现组 stream，内置组件构造零分配），因此索引热路径与读锁下的查询分析共用同一模板。执行引擎（`Query`/postings）完全不感知 analyzer——分析发生在写入时（DocWriter）与查询构建时（`analyze_query`）两层。

**Tech Stack:** Rust 1.97，workspace `crates/core`（rustlucene-core）+ `crates/jni-binding`。无新依赖。

## Global Constraints

- 硬约束：**默认无 analyzer 路径与现状逐字节一致**——`make log-test`（11 次 CheckIndex + 查询 diff）必须全绿，现有 core 128 项测试不得改动即全绿。
- `crates/core/src/lib.rs` 有 `#![forbid(unsafe_code)]`：新代码一律 safe Rust。
- spec 接口草图（`&mut self analyze` / `reset()`）在本计划中细化为**无状态模板**设计；语义（双通道、Cow 零分配、pos_incr 恒 1、filter 一词进一词出）不变。
- analyzer 只合法于 `tokenized && is_indexed` 的 text 字段；keyword 字段挂 analyzer 必须报错。
- 错误信息风格：中文注释 + 英文/中文混合的错误字符串，与现有代码一致；格式对照注释引用 Lucene 类名。
- analyzer 配置不落盘（与 Lucene 一致：`.fnm` 不记 analyzer）。
- 不做：中文分词、stop/synonym filter 实现、query_string 解析器、CLI/磁盘 Searcher 的查询侧分析。

---

### Task 1: analysis 模块骨架 + 三个内置 tokenizer

**Files:**
- Create: `crates/core/src/analysis/tokenizer.rs`
- Create: `crates/core/src/analysis/mod.rs`
- Delete: `crates/core/src/tokenizer.rs`（内容迁入 analysis/tokenizer.rs）
- Modify: `crates/core/src/lib.rs:21`（`pub mod tokenizer;` → `pub mod analysis;`）
- Modify: `crates/core/src/doc_writer.rs:7`（import 路径）

**Interfaces:**
- Produces（后续任务依赖）:
  - `pub trait Tokenizer<'a> { fn next_token(&mut self) -> Option<&'a [u8]>; }`
  - `pub struct WhitespaceTokens<'a>`（`new(&'a str)`，同时保留 `Iterator<Item = &'a str>` 实现供 doc_writer 现状路径使用）
  - `pub struct LetterTokens<'a>`（`new(&'a str)`）
  - `pub struct KeywordTokens<'a>`（`new(&'a str)`）

- [ ] **Step 1: Write the failing test**

写入 `crates/core/src/analysis/tokenizer.rs` 的测试模块（先只写测试和空骨架让编译失败转测试失败）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn collect<'a, T: Tokenizer<'a>>(mut t: T) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(tok) = t.next_token() {
            out.push(tok.to_vec());
        }
        out
    }

    #[test]
    fn whitespace_splits_on_ascii_whitespace() {
        assert_eq!(
            collect(WhitespaceTokens::new("  ab  c\td ef\n")),
            vec![b"ab".to_vec(), b"c".to_vec(), b"d".to_vec(), b"ef".to_vec()]
        );
    }

    #[test]
    fn letter_splits_on_non_alphabetic() {
        // Lucene LetterTokenizer analog: digits/punct are delimiters.
        assert_eq!(
            collect(LetterTokens::new("com.foo.BarException: msg 500")),
            vec![b"com".to_vec(), b"foo".to_vec(), b"BarException".to_vec(), b"msg".to_vec()]
        );
        assert_eq!(collect(LetterTokens::new("500")), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn keyword_yields_whole_input_once() {
        assert_eq!(
            collect(KeywordTokens::new("hello world")),
            vec![b"hello world".to_vec()]
        );
        assert_eq!(collect(KeywordTokens::new("")), Vec::<Vec<u8>>::new());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rustlucene-core analysis::tokenizer 2>&1 | tail -5`
Expected: FAIL（编译错误，`crate::analysis` 不存在）

- [ ] **Step 3: Write minimal implementation**

`crates/core/src/analysis/tokenizer.rs`：

```rust
//! Tokenizers: the source of a token stream. Yields term bytes borrowing
//! from the input (zero-copy); built-ins never allocate.

/// Token stream source (CharTokenizer family analog). `next_token` borrows
/// from the input, not from `&mut self`, so tokens are zero-copy slices.
pub trait Tokenizer<'a> {
    fn next_token(&mut self) -> Option<&'a [u8]>;
}

/// Tokenizes like Lucene's `WhitespaceTokenizer` for the log corpora we
/// target: splits on ASCII whitespace, terms keep original bytes.
///
/// (`WhitespaceTokenizer` splits on `Character.isWhitespace`;
/// `split_ascii_whitespace` agrees with it on ASCII input and differs only
/// for exotic Unicode spaces (U+00A0, U+2028, ...) — treated as term bytes
/// here, which is the behavior we want for log messages.)
pub struct WhitespaceTokens<'a> {
    inner: std::str::SplitAsciiWhitespace<'a>,
}

impl<'a> WhitespaceTokens<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            inner: text.split_ascii_whitespace(),
        }
    }
}

impl<'a> Tokenizer<'a> for WhitespaceTokens<'a> {
    fn next_token(&mut self) -> Option<&'a [u8]> {
        self.inner.next().map(str::as_bytes)
    }
}

/// Iterator impl kept for the legacy doc_writer fast path (fields without
/// a configured analyzer).
impl<'a> Iterator for WhitespaceTokens<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

/// Splits on non-alphabetic characters (Lucene `LetterTokenizer` analog):
/// a token is a maximal run of `char::is_alphabetic`. NOTE: Java's
/// `Character.isLetter` and Rust's `is_alphabetic` agree on virtually all
/// real-world input; Unicode-version drift is accepted and pinned here.
pub struct LetterTokens<'a> {
    input: &'a str,
    offset: usize,
}

impl<'a> LetterTokens<'a> {
    pub fn new(text: &'a str) -> Self {
        Self { input: text, offset: 0 }
    }
}

impl<'a> Tokenizer<'a> for LetterTokens<'a> {
    fn next_token(&mut self) -> Option<&'a [u8]> {
        let rest = &self.input[self.offset..];
        let start = rest.find(char::is_alphabetic)?;
        let tok_start = self.offset + start;
        let tok_end = match rest[start..].find(|c: char| !c.is_alphabetic()) {
            Some(end) => tok_start + end,
            None => self.input.len(),
        };
        self.offset = tok_end;
        Some(self.input[tok_start..tok_end].as_bytes())
    }
}

/// Whole input is one token (Lucene `KeywordTokenizer` analog). Empty input
/// yields nothing, mirroring whitespace behavior for empty text.
pub struct KeywordTokens<'a> {
    pending: Option<&'a str>,
}

impl<'a> KeywordTokens<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            pending: (!text.is_empty()).then_some(text),
        }
    }
}

impl<'a> Tokenizer<'a> for KeywordTokens<'a> {
    fn next_token(&mut self) -> Option<&'a [u8]> {
        self.pending.take().map(str::as_bytes)
    }
}
```

`crates/core/src/analysis/mod.rs`（骨架，后续任务往里加）：

```rust
//! Per-field analysis framework (spec:
//! docs/superpowers/specs/2026-07-31-analyzer-framework-design.md).

mod tokenizer;

pub use tokenizer::{KeywordTokens, LetterTokens, Tokenizer, WhitespaceTokens};
```

`crates/core/src/lib.rs:21`：`pub mod tokenizer;` → `pub mod analysis;`

`crates/core/src/doc_writer.rs:7`：`use crate::tokenizer::WhitespaceTokens;` → `use crate::analysis::WhitespaceTokens;`

删除 `crates/core/src/tokenizer.rs`。

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rustlucene-core`
Expected: PASS（新 3 项 + 现有全部；现有测试不得有任何改动）

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/analysis crates/core/src/lib.rs crates/core/src/doc_writer.rs
git rm crates/core/src/tokenizer.rs
git commit -m "core: analysis module skeleton with whitespace/letter/keyword tokenizers"
```

---

### Task 2: LowercaseFilter + Analyzer 模板与 TokenStream

**Files:**
- Create: `crates/core/src/analysis/filter.rs`
- Create: `crates/core/src/analysis/analyzer.rs`
- Modify: `crates/core/src/analysis/mod.rs`

**Interfaces:**
- Consumes: Task 1 的 `Tokenizer<'a>`、`WhitespaceTokens`、`LetterTokens`、`KeywordTokens`
- Produces:
  - `pub trait TokenFilter: Send + Sync { fn filter<'a>(&self, token: Cow<'a, [u8]>) -> Option<Cow<'a, [u8]>>; fn normalizes(&self) -> bool; }`
  - `pub struct LowercaseFilter;`
  - `pub enum FilterKind { Lowercase, Custom(Arc<dyn TokenFilter>) }`（`Clone`）
  - `pub type TokenizerFactory = Arc<dyn for<'a> Fn(&'a str) -> Box<dyn Tokenizer<'a> + 'a> + Send + Sync>;`
  - `pub enum TokenizerTemplate { Whitespace, Letter, Keyword, Custom(TokenizerFactory) }`（`Clone`）
  - `pub struct Analyzer { pub(crate) tokenizer: TokenizerTemplate, pub(crate) filters: Vec<FilterKind> }`（`Clone`）
  - `Analyzer::analyze<'a, 'b>(&'b self, input: &'a str) -> TokenStream<'a, 'b>`
  - `Analyzer::normalize<'a>(&self, input: &'a str) -> Cow<'a, str>`
  - `pub struct TokenStream<'a, 'b>`，`impl Iterator<Item = Cow<'a, [u8]>>`

- [ ] **Step 1: Write the failing test**

`crates/core/src/analysis/analyzer.rs` 测试模块（骨架先行）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn analyze_to_vec(an: &Analyzer, input: &str) -> Vec<Vec<u8>> {
        an.analyze(input).map(|t| t.into_owned()).collect()
    }

    #[test]
    fn whitespace_lowercase_normalizes_case() {
        let an = Analyzer {
            tokenizer: TokenizerTemplate::Whitespace,
            filters: vec![FilterKind::Lowercase],
        };
        assert_eq!(
            analyze_to_vec(&an, "ERROR Failed error"),
            vec![b"error".to_vec(), b"failed".to_vec(), b"error".to_vec()]
        );
    }

    #[test]
    fn no_filter_passes_bytes_through() {
        let an = Analyzer {
            tokenizer: TokenizerTemplate::Whitespace,
            filters: vec![],
        };
        assert_eq!(
            analyze_to_vec(&an, "ERROR error"),
            vec![b"ERROR".to_vec(), b"error".to_vec()]
        );
    }

    #[test]
    fn lowercase_unicode_semantics() {
        // Non-ASCII uppercase goes through Unicode to_lowercase
        // (Lucene LowerCaseFilter semantics).
        let an = Analyzer {
            tokenizer: TokenizerTemplate::Whitespace,
            filters: vec![FilterKind::Lowercase],
        };
        assert_eq!(analyze_to_vec(&an, "ÄBC"), vec!["äbc".as_bytes().to_vec()]);
    }

    #[test]
    fn normalize_applies_filters_without_tokenizing() {
        let an = Analyzer {
            tokenizer: TokenizerTemplate::Whitespace,
            filters: vec![FilterKind::Lowercase],
        };
        // whole input as one token, lowercased — for Prefix/Wildcard
        assert_eq!(an.normalize("Err*"), std::borrow::Cow::Borrowed("err*"));
        assert_eq!(an.normalize("err*"), std::borrow::Cow::Borrowed("err*"));
    }

    #[test]
    fn lowercase_ascii_fast_path_is_borrowed() {
        // zero-alloc guarantee: pure-ASCII lowercase token stays borrowed
        let an = Analyzer {
            tokenizer: TokenizerTemplate::Whitespace,
            filters: vec![FilterKind::Lowercase],
        };
        let toks: Vec<_> = an.analyze("ok").collect();
        assert!(matches!(toks[0], std::borrow::Cow::Borrowed(_)));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rustlucene-core analysis 2>&1 | tail -5`
Expected: FAIL（编译错误：`analyzer` 模块不存在）

- [ ] **Step 3: Write minimal implementation**

`crates/core/src/analysis/filter.rs`：

```rust
//! Token filters: one token in, one token out (or dropped). Filters are
//! stateless and shared by reference, so `&Analyzer` works under a read
//! lock (query-side analysis) as well as in the write hot path.

use std::borrow::Cow;

/// One token in, one token out; `None` drops the token (reserved for
/// future stop filters — not implemented this round).
pub trait TokenFilter: Send + Sync {
    fn filter<'a>(&self, token: Cow<'a, [u8]>) -> Option<Cow<'a, [u8]>>;
    /// Whether this filter participates in the normalize channel
    /// (Lucene `MultiTermAwareComponent` semantics; `LowerCaseFilter` does).
    fn normalizes(&self) -> bool;
}

/// Lucene `LowerCaseFilter` analog. ASCII fast path: a pure-ASCII token
/// with no uppercase bytes passes through borrowed (zero allocation);
/// anything else goes through Unicode `to_lowercase`.
pub struct LowercaseFilter;

impl TokenFilter for LowercaseFilter {
    fn filter<'a>(&self, token: Cow<'a, [u8]>) -> Option<Cow<'a, [u8]>> {
        if token.is_ascii() && !token.iter().any(u8::is_ascii_uppercase) {
            return Some(token);
        }
        Some(Cow::Owned(
            String::from_utf8_lossy(&token)
                .into_owned()
                .to_lowercase()
                .into_bytes(),
        ))
    }

    fn normalizes(&self) -> bool {
        true
    }
}
```

`crates/core/src/analysis/analyzer.rs`：

```rust
//! Analyzer: a compiled chain of one tokenizer + ordered filters.
//! Stateless template — `analyze` builds a fresh stream per call
//! (built-in construction is allocation-free), so one `&Analyzer` serves
//! both the index hot path and read-lock query analysis.

use std::borrow::Cow;
use std::sync::Arc;

use super::filter::{LowercaseFilter, TokenFilter};
use super::tokenizer::{KeywordTokens, LetterTokens, Tokenizer, WhitespaceTokens};

/// Factory for externally registered tokenizers (方案 A 逃生舱).
pub type TokenizerFactory =
    Arc<dyn for<'a> Fn(&'a str) -> Box<dyn Tokenizer<'a> + 'a> + Send + Sync>;

/// Which tokenizer a chain starts from. Built-ins dispatch statically.
#[derive(Clone)]
pub enum TokenizerTemplate {
    Whitespace,
    Letter,
    Keyword,
    Custom(TokenizerFactory),
}

/// Compiled filter chain entry: built-ins dispatch statically; custom
/// filters hang off the registry.
#[derive(Clone)]
pub enum FilterKind {
    Lowercase,
    Custom(Arc<dyn TokenFilter>),
}

impl FilterKind {
    fn apply<'a>(&self, token: Cow<'a, [u8]>) -> Option<Cow<'a, [u8]>> {
        match self {
            FilterKind::Lowercase => LowercaseFilter.filter(token),
            FilterKind::Custom(f) => f.filter(token),
        }
    }

    fn normalizes(&self) -> bool {
        match self {
            FilterKind::Lowercase => true,
            FilterKind::Custom(f) => f.normalizes(),
        }
    }
}

enum ActiveTokenizer<'a> {
    Whitespace(WhitespaceTokens<'a>),
    Letter(LetterTokens<'a>),
    Keyword(KeywordTokens<'a>),
    Custom(Box<dyn Tokenizer<'a> + 'a>),
}

impl<'a> ActiveTokenizer<'a> {
    fn next(&mut self) -> Option<&'a [u8]> {
        match self {
            ActiveTokenizer::Whitespace(t) => t.next_token(),
            ActiveTokenizer::Letter(t) => t.next_token(),
            ActiveTokenizer::Keyword(t) => t.next_token(),
            ActiveTokenizer::Custom(t) => t.next_token(),
        }
    }
}

/// A compiled analyzer chain (spec: static enum dispatch + Custom escape
/// hatch). pos_incr is always 1 for built-ins; positions are the running
/// token counter at the call site.
#[derive(Clone)]
pub struct Analyzer {
    pub(crate) tokenizer: TokenizerTemplate,
    pub(crate) filters: Vec<FilterKind>,
}

impl Analyzer {
    /// Full token stream (Lucene `Analyzer.tokenStream`): indexing and
    /// Term/Phrase/Terms query rewriting.
    pub fn analyze<'a, 'b>(&'b self, input: &'a str) -> TokenStream<'a, 'b> {
        let tokenizer = match &self.tokenizer {
            TokenizerTemplate::Whitespace => {
                ActiveTokenizer::Whitespace(WhitespaceTokens::new(input))
            }
            TokenizerTemplate::Letter => ActiveTokenizer::Letter(LetterTokens::new(input)),
            TokenizerTemplate::Keyword => ActiveTokenizer::Keyword(KeywordTokens::new(input)),
            TokenizerTemplate::Custom(f) => ActiveTokenizer::Custom(f(input)),
        };
        TokenStream {
            tokenizer,
            filters: &self.filters,
        }
    }

    /// Normalize channel (Lucene `Analyzer.normalize`): the whole input is
    /// passed through the normalizing filters as a single token — no
    /// tokenization. Used for Prefix/Wildcard query rewriting.
    pub fn normalize<'a>(&self, input: &'a str) -> Cow<'a, str> {
        let mut tok: Cow<'a, [u8]> = Cow::Borrowed(input.as_bytes());
        for f in &self.filters {
            if f.normalizes() {
                match f.apply(tok) {
                    Some(t) => tok = t,
                    None => return Cow::Owned(String::new()),
                }
            }
        }
        match tok {
            Cow::Borrowed(_) => Cow::Borrowed(input),
            Cow::Owned(bytes) => Cow::Owned(String::from_utf8_lossy(&bytes).into_owned()),
        }
    }
}

pub struct TokenStream<'a, 'b> {
    tokenizer: ActiveTokenizer<'a>,
    filters: &'b [FilterKind],
}

impl<'a> Iterator for TokenStream<'a, '_> {
    type Item = Cow<'a, [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        'outer: while let Some(raw) = self.tokenizer.next() {
            let mut tok = Cow::Borrowed(raw);
            for f in self.filters {
                tok = match f.apply(tok) {
                    Some(t) => t,
                    None => continue 'outer,
                };
            }
            return Some(tok);
        }
        None
    }
}
```

`crates/core/src/analysis/mod.rs` 更新为：

```rust
//! Per-field analysis framework (spec:
//! docs/superpowers/specs/2026-07-31-analyzer-framework-design.md).

mod analyzer;
mod filter;
mod tokenizer;

pub use analyzer::{Analyzer, FilterKind, TokenStream, TokenizerFactory, TokenizerTemplate};
pub use filter::{LowercaseFilter, TokenFilter};
pub use tokenizer::{KeywordTokens, LetterTokens, Tokenizer, WhitespaceTokens};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rustlucene-core analysis`
Expected: PASS（Task 1 的 3 项 + 本任务 5 项）

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/analysis
git commit -m "core: Analyzer template, TokenStream, LowercaseFilter"
```

---

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

### Task 5: DocWriter 索引侧接入

**Files:**
- Modify: `crates/core/src/doc_writer.rs`（FieldBuf 加 analyzer、写入热路径分支）

**Interfaces:**
- Consumes: Task 2 `Analyzer::analyze`、Task 4 `FieldSpec.analyzer`
- Produces: 带 analyzer 的 text 字段写入即归一化（`ERROR`/`error` 归一到同一 term）；无 analyzer 字段字节级不变

- [ ] **Step 1: Write the failing test**

`crates/core/src/doc_writer.rs` 测试模块（line 687 起，与现有 `builds_postings_with_freqs_and_positions` 同写法）追加：

```rust
    #[test]
    fn analyzer_normalizes_terms_at_index_time() {
        let mut schema = Schema::new();
        schema.add(FieldSpec::text("message").with_analyzer("whitespace|lowercase"));
        let mut dw = DocWriter::new();
        let mut d0 = Document::new();
        d0.add("message", FieldValue::Text("ERROR Failed error".to_string()));
        dw.add_document(&schema, d0, None).unwrap();

        let dict = dw.field_buffer(0).unwrap().dict.as_ref().unwrap();
        assert_eq!(dict.len(), 2); // error, failed
        assert!(dict.find(b"error").is_some());
        assert!(dict.find(b"failed").is_some());
        assert!(dict.find(b"ERROR").is_none());
    }

    #[test]
    fn no_analyzer_keeps_original_bytes() {
        let mut schema = Schema::new();
        schema.add(FieldSpec::text("message"));
        let mut dw = DocWriter::new();
        let mut d0 = Document::new();
        d0.add("message", FieldValue::Text("ERROR Failed".to_string()));
        dw.add_document(&schema, d0, None).unwrap();

        let dict = dw.field_buffer(0).unwrap().dict.as_ref().unwrap();
        assert!(dict.find(b"ERROR").is_some());
        assert!(dict.find(b"error").is_none());
    }
```

（`field_buffer(field_number)`、`dict.find`/`len` 均为现有 API，见 line 703-713 的既有用法。）

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rustlucene-core doc_writer 2>&1 | tail -5`
Expected: FAIL（词典仍是原始字节，`dict.find(b"ERROR")` 命中 / analyzer 字段找不到小写 term）

- [ ] **Step 3: Write minimal implementation**

`crates/core/src/doc_writer.rs`：

1) `FieldBuf`（line 274-283）增加：

```rust
    /// Compiled analyzer chain for analyzer-configured text fields
    /// (spec §索引侧接入): compiled once per field per writer, reused
    /// across docs via its stateless template.
    pub analyzer: Option<crate::analysis::Analyzer>,
```

`FieldBuf::new`（line 286 起）构造：

```rust
        let analyzer = spec.analyzer.as_deref().map(|a| {
            crate::analysis::Analyzer::parse(a).expect("analyzer spec validated by Schema::add")
        });
```

2) 提取共享的 token 入库 helper（放在 `DocWriter` impl 外的自由函数）：

```rust
/// Indexes one analyzed token into the field dictionary; returns the RAM
/// delta. Shared by the legacy whitespace fast path and the analyzer path.
fn index_token(
    dict: &mut TermDict,
    doc_id: u32,
    tok: &[u8],
    has_positions: bool,
    position: u32,
) -> usize {
    let (id, is_new) = dict.lookup_or_insert_flag(tok);
    let new_doc = dict.recs[id as usize]
        .postings
        .add_occurrence(doc_id, if has_positions { Some(position) } else { None });
    (if is_new { TERM_RAM + tok.len() } else { 0 })
        + if new_doc {
            POSTING_NEW_DOC_RAM
        } else {
            POSTING_SAME_DOC_RAM
        }
}
```

3) 写入热路径（现 line 394-417 的循环）替换为：

```rust
                        let buf = self.buffers[number as usize].as_mut().unwrap();
                        let dict = buf.dict.as_mut().unwrap();
                        let has_positions = buf.spec.has_positions();
                        let mut saw_term = false;
                        let mut position = 0u32;
                        match &buf.analyzer {
                            Some(analyzer) => {
                                for tok in analyzer.analyze(&text) {
                                    self.ram_bytes +=
                                        index_token(dict, doc_id, &tok, has_positions, position);
                                    saw_term = true;
                                    position += 1;
                                }
                            }
                            None => {
                                for token in WhitespaceTokens::new(&text) {
                                    self.ram_bytes += index_token(
                                        dict,
                                        doc_id,
                                        token.as_bytes(),
                                        has_positions,
                                        position,
                                    );
                                    saw_term = true;
                                    position += 1;
                                }
                            }
                        }
                        if saw_term {
                            buf.doc_count += 1;
                        }
```

行为说明：无 analyzer 分支与原循环逐语句等价（同一 helper、同一 RAM 公式、同一 position 语义），字节级不变。

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rustlucene-core`
Expected: PASS（新增 2 项 + 现有全绿）

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/doc_writer.rs
git commit -m "core: run configured analyzer chain at index time in DocWriter"
```

---

### Task 6: 查询侧 `analyze_query`

**Files:**
- Create: `crates/core/src/analysis/query_analysis.rs`
- Modify: `crates/core/src/analysis/mod.rs`

**Interfaces:**
- Consumes: Task 3 `Analyzer::parse`、Task 4 `FieldSpec.analyzer`、`crate::search::query::{Query, Occur}`
- Produces: `pub fn analyze_query(query: &Query, schema: &Schema) -> Result<Query, String>`
  - Term：analyze；0 token → Err；1 → 替换；多 → Bool SHOULD
  - Terms/And/Or/Phrase：每个 value 恰好 1 token，否则 Err
  - Prefix/Wildcard：normalize（Wildcard 重建 DFA）
  - Bool：递归；MatchAll/PointRange：原样
  - 无 analyzer 字段：原样透传

- [ ] **Step 1: Write the failing test**

`crates/core/src/analysis/query_analysis.rs` 测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FieldSpec, Schema};

    fn test_schema() -> Schema {
        let mut s = Schema::new();
        s.add(FieldSpec::text("message").with_analyzer("whitespace|lowercase"));
        s.add(FieldSpec::keyword("level"));
        s
    }

    #[test]
    fn term_is_lowercased_for_analyzed_field() {
        let s = test_schema();
        let q = analyze_query(&Query::term("message", "ERROR"), &s).unwrap();
        assert_eq!(q, Query::term("message", "error"));
    }

    #[test]
    fn term_passes_through_for_plain_field() {
        let s = test_schema();
        let q = analyze_query(&Query::term("level", "ERROR"), &s).unwrap();
        assert_eq!(q, Query::term("level", "ERROR"));
    }

    #[test]
    fn term_with_multiple_tokens_becomes_bool_should() {
        let s = test_schema();
        let q = analyze_query(&Query::term("message", "connection FAILED"), &s).unwrap();
        assert_eq!(
            q,
            Query::bool(vec![
                (Occur::Should, Query::term("message", "connection")),
                (Occur::Should, Query::term("message", "failed")),
            ])
        );
    }

    #[test]
    fn terms_and_phrase_require_exactly_one_token_per_value() {
        let s = test_schema();
        let q = analyze_query(&Query::terms("message", &["ERROR", "Warn"]), &s).unwrap();
        assert_eq!(q, Query::terms("message", &["error", "warn"]));
        // two-token value is an error
        assert!(analyze_query(&Query::terms("message", &["two words"]), &s).is_err());
        assert!(analyze_query(&Query::phrase("message", &["two words", "x"]), &s).is_err());
        let q = analyze_query(&Query::phrase("message", &["Connection", "Failed"]), &s).unwrap();
        assert_eq!(q, Query::phrase("message", &["connection", "failed"]));
    }

    #[test]
    fn prefix_and_wildcard_are_normalized_not_tokenized() {
        let s = test_schema();
        let q = analyze_query(&Query::prefix("message", "ERR"), &s).unwrap();
        assert_eq!(q, Query::prefix("message", "err"));
        let q = analyze_query(&Query::wildcard("message", "ERR*"), &s).unwrap();
        assert_eq!(q, Query::wildcard("message", "err*"));
    }

    #[test]
    fn bool_recurses_and_other_variants_pass_through() {
        let s = test_schema();
        let q = Query::bool(vec![
            (Occur::Must, Query::term("message", "ERROR")),
            (Occur::MustNot, Query::term("level", "DEBUG")),
        ]);
        let out = analyze_query(&q, &s).unwrap();
        assert_eq!(
            out,
            Query::bool(vec![
                (Occur::Must, Query::term("message", "error")),
                (Occur::MustNot, Query::term("level", "DEBUG")),
            ])
        );
        let q = analyze_query(&Query::MatchAll, &s).unwrap();
        assert_eq!(q, Query::MatchAll);
        let q = analyze_query(&Query::point_range("ts", 1, 2), &s).unwrap();
        assert_eq!(q, Query::point_range("ts", 1, 2));
    }
}
```

注意：letter tokenizer 的 0-token 场景（`analyze_query(&Query::term("message", "!!!"), ...)` 配 `letter` analyzer）也加一条 Err 断言（用 `FieldSpec::text("m2").with_analyzer("letter")` 的 schema）。

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rustlucene-core analysis 2>&1 | tail -5`
Expected: FAIL（`analyze_query` 不存在）

- [ ] **Step 3: Write minimal implementation**

`crates/core/src/analysis/query_analysis.rs`：

```rust
//! Query-side analysis (spec §查询侧双通道): rewrites a parsed `Query` so
//! its term bytes match what the index holds for analyzer-configured
//! fields. Fields without an analyzer pass through byte-identical.
//! The execution engine never sees analyzers — rewriting happens here, at
//! query-build time (called from the JNI search entry point).

use std::borrow::Cow;

use super::Analyzer;
use crate::schema::Schema;
use crate::search::query::{Occur, Query};

pub fn analyze_query(query: &Query, schema: &Schema) -> Result<Query, String> {
    match query {
        Query::Term { field, term } => match analyzer_for(schema, field)? {
            None => Ok(query.clone()),
            Some(an) => {
                let toks = analyze_text(&an, field, term)?;
                match toks.len() {
                    0 => Err(format!("field {field}: query text analyzes to no tokens")),
                    1 => Ok(Query::Term {
                        field: field.clone(),
                        term: toks.into_iter().next().unwrap().into_owned(),
                    }),
                    // Lucene QueryParser default: multi-token match → OR.
                    _ => Ok(Query::bool(
                        toks.into_iter()
                            .map(|t| {
                                (
                                    Occur::Should,
                                    Query::Term {
                                        field: field.clone(),
                                        term: t.into_owned(),
                                    },
                                )
                            })
                            .collect(),
                    )),
                }
            }
        },
        Query::Terms { field, terms } => Ok(Query::Terms {
            field: field.clone(),
            terms: analyze_each(schema, field, terms, "terms")?,
        }),
        Query::And { field, terms } => Ok(Query::And {
            field: field.clone(),
            terms: analyze_each(schema, field, terms, "and")?,
        }),
        Query::Or { field, terms } => Ok(Query::Or {
            field: field.clone(),
            terms: analyze_each(schema, field, terms, "or")?,
        }),
        Query::Phrase { field, terms } => Ok(Query::Phrase {
            field: field.clone(),
            terms: analyze_each(schema, field, terms, "phrase")?,
        }),
        Query::Prefix { field, prefix } => match analyzer_for(schema, field)? {
            None => Ok(query.clone()),
            Some(an) => Ok(Query::Prefix {
                field: field.clone(),
                prefix: an.normalize(utf8(field, prefix)?).into_owned().into_bytes(),
            }),
        },
        Query::Wildcard { field, pattern, .. } => match analyzer_for(schema, field)? {
            None => Ok(query.clone()),
            // normalize then rebuild via the constructor — the DFA must be
            // recompiled for the rewritten pattern.
            Some(an) => Ok(Query::wildcard(field, &an.normalize(utf8(field, pattern)?))),
        },
        Query::Bool { clauses } => {
            let mut out = Vec::with_capacity(clauses.len());
            for (occur, sub) in clauses {
                out.push((*occur, analyze_query(sub, schema)?));
            }
            Ok(Query::Bool { clauses: out })
        }
        Query::MatchAll | Query::PointRange { .. } => Ok(query.clone()),
    }
}

/// Compiles the field's analyzer spec fresh (stateless components, a few
/// small enums) — no shared mutable state, so this works under the search
/// read lock (spec §查询侧双通道).
fn analyzer_for(schema: &Schema, field: &str) -> Result<Option<Analyzer>, String> {
    match schema.get(field).and_then(|f| f.analyzer.as_deref()) {
        Some(spec) => Ok(Some(Analyzer::parse(spec)?)),
        None => Ok(None),
    }
}

fn utf8<'a>(field: &str, bytes: &'a [u8]) -> Result<&'a str, String> {
    std::str::from_utf8(bytes).map_err(|_| format!("field {field}: query term is not valid UTF-8"))
}

fn analyze_text<'t>(
    an: &Analyzer,
    field: &str,
    value: &'t [u8],
) -> Result<Vec<Cow<'t, [u8]>>, String> {
    Ok(an.analyze(utf8(field, value)?).collect())
}

/// Terms/And/Or/Phrase rule (spec): every value must analyze to exactly
/// one token.
fn analyze_each(
    schema: &Schema,
    field: &str,
    values: &[Vec<u8>],
    ctx: &str,
) -> Result<Vec<Vec<u8>>, String> {
    match analyzer_for(schema, field)? {
        None => Ok(values.to_vec()),
        Some(an) => values
            .iter()
            .map(|v| {
                let toks = analyze_text(&an, field, v)?;
                match toks.len() {
                    1 => Ok(toks.into_iter().next().unwrap().into_owned()),
                    n => Err(format!(
                        "{ctx}: field {field}: value analyzes to {n} tokens, expected exactly 1"
                    )),
                }
            })
            .collect(),
    }
}
```

`crates/core/src/analysis/mod.rs` 增加：

```rust
mod query_analysis;

pub use query_analysis::analyze_query;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rustlucene-core analysis`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/analysis
git commit -m "core: query-side dual-channel analysis (analyze_query)"
```

---

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
