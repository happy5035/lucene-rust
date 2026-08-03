//! Per-field analysis framework (spec:
//! docs/superpowers/specs/2026-07-31-analyzer-framework-design.md).

mod analyzer;
mod filter;
mod registry;
mod tokenizer;

pub use analyzer::{Analyzer, FilterKind, TokenStream, TokenizerFactory, TokenizerTemplate};
pub use filter::{LowercaseFilter, TokenFilter};
pub use registry::{register_filter, register_tokenizer};
pub use tokenizer::{KeywordTokens, LetterTokens, Tokenizer, WhitespaceTokens};
