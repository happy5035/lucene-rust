//! FieldInfo/FieldInfos and `.fnm` writing, mirroring
//! `codecs/lucene94/Lucene94FieldInfosFormat.java` (write :367-416) and
//! `index/FieldInfo.java` (9.12.3).

use std::collections::BTreeMap;
use std::io;

use crate::codec_util::{write_footer, write_index_header};
use crate::directory::FSDirectory;

const CODEC_NAME: &str = "Lucene94FieldInfos"; // :422
const FORMAT_CURRENT: u32 = 1; // FORMAT_PARENT_FIELD (:423-426)
pub const EXTENSION: &str = "fnm"; // :419

// bits byte flags (:429-433)
const STORE_TERMVECTOR: u8 = 0x1;
const OMIT_NORMS: u8 = 0x2;
const STORE_PAYLOADS: u8 = 0x4;
const SOFT_DELETES_FIELD: u8 = 0x8;
const PARENT_FIELD_FIELD: u8 = 0x10;

/// index/IndexOptions.java ordinals (serialized as a single byte, :331-347).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndexOptions {
    None = 0,
    Docs = 1,
    DocsAndFreqs = 2,
    DocsAndFreqsAndPositions = 3,
    DocsAndFreqsAndPositionsAndOffsets = 4,
}

/// index/DocValuesType.java ordinals (single byte, :243-261).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DocValuesType {
    None = 0,
    Numeric = 1,
    Binary = 2,
    Sorted = 3,
    SortedSet = 4,
    SortedNumeric = 5,
}

/// index/VectorEncoding.java ordinals.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VectorEncoding {
    Byte = 0,
    Float32 = 1,
}

/// index/VectorSimilarityFunction.java ordinals (:301-322).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VectorSimilarity {
    Euclidean = 0,
    DotProduct = 1,
    Cosine = 2,
    MaximumInnerProduct = 3,
}

/// A single field's metadata (index/FieldInfo.java).
pub struct FieldInfo {
    pub name: String,
    pub number: i32,
    pub store_termvector: bool,
    pub omit_norms: bool,
    pub store_payloads: bool,
    pub soft_deletes: bool,
    pub parent_field: bool,
    pub index_options: IndexOptions,
    pub doc_values_type: DocValuesType,
    /// -1 when there are no docvalues updates (LE long on disk).
    pub doc_values_gen: i64,
    pub attributes: BTreeMap<String, String>,
    pub point_dimension_count: i32,
    pub point_index_dimension_count: i32,
    pub point_num_bytes: i32,
    pub vector_dimension: i32,
    pub vector_encoding: VectorEncoding,
    pub vector_similarity: VectorSimilarity,
}

impl FieldInfo {
    /// A stored-only field (not indexed, no docvalues): the common case for
    /// `StoredField` in Lucene. Note `omit_norms` must stay false for
    /// non-indexed fields (FieldInfo.checkConsistency:147-149).
    pub fn stored(name: &str, number: i32) -> Self {
        FieldInfo {
            name: name.to_string(),
            number,
            store_termvector: false,
            omit_norms: false,
            store_payloads: false,
            soft_deletes: false,
            parent_field: false,
            index_options: IndexOptions::None,
            doc_values_type: DocValuesType::None,
            doc_values_gen: -1,
            attributes: BTreeMap::new(),
            point_dimension_count: 0,
            point_index_dimension_count: 0,
            point_num_bytes: 0,
            vector_dimension: 0,
            vector_encoding: VectorEncoding::Float32,
            vector_similarity: VectorSimilarity::Euclidean,
        }
    }

    fn bits(&self) -> u8 {
        let mut bits = 0u8;
        if self.store_termvector {
            bits |= STORE_TERMVECTOR;
        }
        if self.omit_norms {
            bits |= OMIT_NORMS;
        }
        if self.store_payloads {
            bits |= STORE_PAYLOADS;
        }
        if self.soft_deletes {
            bits |= SOFT_DELETES_FIELD;
        }
        if self.parent_field {
            bits |= PARENT_FIELD_FIELD;
        }
        bits
    }
}

/// index/FieldInfos.java (writer side: a simple ordered collection).
pub struct FieldInfos {
    pub fields: Vec<FieldInfo>,
}

impl FieldInfos {
    pub fn new(fields: Vec<FieldInfo>) -> Self {
        FieldInfos { fields }
    }

    pub fn by_name(&self, name: &str) -> Option<&FieldInfo> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Lucene94FieldInfosFormat.write (:367-416).
    pub fn write(
        &self,
        dir: &FSDirectory,
        segment: &str,
        segment_id: &[u8; 16],
        suffix: &str,
    ) -> io::Result<String> {
        let file_name = format!("{segment}{suffix}.{EXTENSION}");
        let mut out = dir.create_output(&file_name)?;
        write_index_header(&mut out, CODEC_NAME, FORMAT_CURRENT, segment_id, suffix)?;
        out.write_vint(self.fields.len() as i32)?;
        for fi in &self.fields {
            out.write_string(&fi.name)?;
            out.write_vint(fi.number)?;
            out.write_byte(fi.bits())?;
            out.write_byte(fi.index_options as u8)?;
            out.write_byte(fi.doc_values_type as u8)?;
            out.write_long(fi.doc_values_gen)?;
            out.write_map_of_strings(&fi.attributes)?;
            out.write_vint(fi.point_dimension_count)?;
            if fi.point_dimension_count != 0 {
                out.write_vint(fi.point_index_dimension_count)?;
                out.write_vint(fi.point_num_bytes)?;
            }
            out.write_vint(fi.vector_dimension)?;
            out.write_byte(fi.vector_encoding as u8)?;
            out.write_byte(fi.vector_similarity as u8)?;
        }
        write_footer(&mut out)?;
        out.flush()?;
        Ok(file_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{ChecksumIndexOutput, IndexOutput};

    #[test]
    fn enum_ordinals() {
        assert_eq!(IndexOptions::None as u8, 0);
        assert_eq!(IndexOptions::DocsAndFreqsAndPositionsAndOffsets as u8, 4);
        assert_eq!(DocValuesType::None as u8, 0);
        assert_eq!(DocValuesType::SortedNumeric as u8, 5);
        assert_eq!(VectorEncoding::Byte as u8, 0);
        assert_eq!(VectorEncoding::Float32 as u8, 1);
        assert_eq!(VectorSimilarity::Euclidean as u8, 0);
        assert_eq!(VectorSimilarity::MaximumInnerProduct as u8, 3);
    }

    #[test]
    fn bits_byte() {
        let fi = FieldInfo::stored("body", 0);
        assert_eq!(fi.bits(), 0);
        let fi = FieldInfo {
            omit_norms: true,
            index_options: IndexOptions::Docs,
            ..FieldInfo::stored("f", 1)
        };
        assert_eq!(fi.bits(), OMIT_NORMS);
        let fi = FieldInfo {
            store_termvector: true,
            store_payloads: true,
            index_options: IndexOptions::DocsAndFreqsAndPositions,
            ..FieldInfo::stored("f", 1)
        };
        assert_eq!(fi.bits(), STORE_TERMVECTOR | STORE_PAYLOADS);
    }

    #[test]
    fn stored_field_layout() {
        // hand-serialize one stored-only field the way FieldInfos::write does
        let fi = FieldInfo::stored("body", 3);
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        out.write_string(&fi.name).unwrap();
        out.write_vint(fi.number).unwrap();
        out.write_byte(fi.bits()).unwrap();
        out.write_byte(fi.index_options as u8).unwrap();
        out.write_byte(fi.doc_values_type as u8).unwrap();
        out.write_long(fi.doc_values_gen).unwrap();
        out.write_map_of_strings(&fi.attributes).unwrap();
        out.write_vint(fi.point_dimension_count).unwrap();
        out.write_vint(fi.vector_dimension).unwrap();
        out.write_byte(fi.vector_encoding as u8).unwrap();
        out.write_byte(fi.vector_similarity as u8).unwrap();
        let bytes = out.into_bytes();
        assert_eq!(
            bytes,
            vec![
                4, b'b', b'o', b'd', b'y', // name
                3,    // number
                0,    // bits
                0,    // indexOptions NONE
                0,    // docValuesType NONE
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // dvGen -1 LE
                0,    // attributes
                0,    // pointDimensionCount
                0,    // vectorDimension
                1,    // vectorEncoding FLOAT32
                0,    // vectorSimilarity EUCLIDEAN
            ]
        );
    }
}
