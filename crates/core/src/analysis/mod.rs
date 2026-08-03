//! Per-field analysis framework (spec:
//! docs/superpowers/specs/2026-07-31-analyzer-framework-design.md).

mod analyzer;
mod filter;
mod tokenizer;

pub use analyzer::{Analyzer, FilterKind, TokenStream, TokenizerFactory, TokenizerTemplate};
pub use filter::{LowercaseFilter, TokenFilter};
pub use tokenizer::{KeywordTokens, LetterTokens, Tokenizer, WhitespaceTokens};
