//! Per-field analysis framework (spec:
//! docs/superpowers/specs/2026-07-31-analyzer-framework-design.md).

mod tokenizer;

pub use tokenizer::{KeywordTokens, LetterTokens, Tokenizer, WhitespaceTokens};
