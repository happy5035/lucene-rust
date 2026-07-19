//! Lucene 9.12.3-compatible index writer format layer.
//!
//! Writes segments that Java Lucene 9.12.3 (`DirectoryReader` / `CheckIndex`)
//! can read: stored fields (Lucene90 BEST_SPEED), field infos (Lucene94),
//! segment info (Lucene99/"Lucene90SegmentInfo") and the `segments_N` commit.
//! All format details follow the 9.12.3 sources, cited per item.

#![forbid(unsafe_code)]

pub mod codec_util;
pub mod directory;
pub mod doc_values;
pub mod field_infos;
pub mod fst;
pub mod io;
pub mod packed;
pub mod points;
pub mod postings;
pub mod postings_ll;
pub mod segment_info;
pub mod segment_infos;
pub mod stored_fields;

pub use directory::FSDirectory;
pub use field_infos::{DocValuesType, FieldInfo, FieldInfos, IndexOptions};
pub use io::{ChecksumIndexOutput, IndexOutput};
pub use segment_info::SegmentInfo;
pub use segment_infos::{SegmentCommitInfo, SegmentInfos};
pub use stored_fields::{StoredField, StoredFieldsWriter};
