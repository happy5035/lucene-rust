//! RustLucene core: Lucene 9.12.3-compatible index write path.
//!
//! Append-only writer: buffers documents in RAM, flushes them into segment
//! files via `codec-lucene9`, and publishes commit points (`segments_N`)
//! with Lucene's two-phase commit protocol. Read/search/merge are out of
//! scope (delegated to Java Lucene).

#![forbid(unsafe_code)]

pub mod doc_writer;
pub mod document;
pub mod index_writer;
pub mod json;
pub mod schema;
pub mod search;
pub mod segment_builder;
pub mod tokenizer;

pub use document::{Document, FieldValue};
pub use index_writer::{commit_segments, IndexWriter, IndexWriterConfig};
pub use json::{BindOutcome, FieldPolicy, JsonBinder};
pub use schema::{FieldSpec, PointSpec, Schema};
pub use segment_builder::SegmentBuilder;
