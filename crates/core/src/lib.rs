//! RustLucene core: Lucene 9.12.3-compatible index write and read paths.
//!
//! Append-only writer: buffers documents in RAM, flushes them into segment
//! files via `codec-lucene9`, and publishes commit points (`segments_N`)
//! with Lucene's two-phase commit protocol. The `search` module reads back
//! those indexes (Term/MatchAll queries, ConstantScore semantics; see
//! docs/superpowers/specs/2026-07-22-rust-search-design.md).

#![forbid(unsafe_code)]

pub mod doc_writer;
pub mod document;
pub mod index_writer;
pub mod json;
pub mod memory_access;
pub mod memory_reader;
pub mod merge;
pub mod schema;
pub mod search;
pub mod segment_builder;
pub mod tokenizer;

pub use document::{Document, FieldValue};
pub use index_writer::{commit_segments, DocLocation, IndexWriter, IndexWriterConfig};
pub use json::{BindOutcome, FieldPolicy, JsonBinder};
pub use schema::{FieldSpec, PointSpec, Schema};
pub use segment_builder::SegmentBuilder;
