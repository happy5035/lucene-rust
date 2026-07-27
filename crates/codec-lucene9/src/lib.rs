//! Lucene 9.12.3-compatible index format layer.
//!
//! Writes segments that Java Lucene 9.12.3 (`DirectoryReader` / `CheckIndex`)
//! can read: stored fields (Lucene90 BEST_SPEED), field infos (Lucene94),
//! segment info (Lucene99/"Lucene90SegmentInfo") and the `segments_N` commit.
//! Reads the same files back (`DataInput` / `IndexInput` / `ChecksumIndexInput`,
//! `FSDirectory::open_input`) for the search read path.
//! All format details follow the 9.12.3 sources, cited per item.

// `deny` rather than `forbid` so the two module-level exceptions —
// postings_ll/simd.rs and roaring/frozen.rs — can opt back in with a
// module-level `allow`; see each module for the safety argument and the
// exact boundary of the unsafe code.
#![deny(unsafe_code)]

pub mod codec_util;
pub mod directory;
pub mod doc_values;
pub mod doc_values_read;
pub mod field_infos;
pub mod fst;
pub mod io;
pub mod packed;
pub mod points;
pub mod points_read;
pub mod postings;
pub mod postings_ll;
pub mod postings_read;
pub mod roaring;
pub mod segment_info;
pub mod segment_infos;
pub mod stored_fields;
pub mod terms_read;

pub use directory::FSDirectory;
pub use field_infos::{DocValuesType, FieldInfo, FieldInfos, IndexOptions};
pub use io::{ChecksumIndexInput, ChecksumIndexOutput, DataInput, IndexInput, IndexOutput};
pub use postings_read::{DocsEnum, DocsFreqsEnum, NO_MORE_DOCS, PostingsReader};
pub use segment_info::{IndexSortFieldInfo, MissingValue, SegmentInfo, SortFieldType};
pub use segment_infos::{SegmentCommitInfo, SegmentInfos};
pub use stored_fields::{StoredField, StoredFieldsReader, StoredFieldsWriter};
pub use terms_read::{TermEntry, TermState, TermsDict};
