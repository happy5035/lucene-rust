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

