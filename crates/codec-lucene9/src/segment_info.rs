//! SegmentInfo and `.si` writing, mirroring
//! `codecs/lucene99/Lucene99SegmentInfoFormat.java` (writeSegmentInfo :185-235)
//! and `index/SegmentInfo.java` (9.12.3).

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use crate::codec_util::{check_footer, check_index_header, corrupt, write_footer, write_index_header};
use crate::directory::FSDirectory;
use crate::io::DataInput;

const CODEC_NAME: &str = "Lucene90SegmentInfo"; // :83
const VERSION_CURRENT: u32 = 0; // VERSION_START (:84-85)
pub const EXTENSION: &str = "si";

/// SegmentInfo.YES (:45-48).
const YES: i8 = 1;
/// SegmentInfo.NO (:45-48).
const NO: i8 = -1;

/// Attribute key required by Lucene90StoredFieldsFormat on read
/// (Lucene90StoredFieldsFormat.java:113-114,131-137,142).
pub const STORED_FIELDS_MODE_ATTRIBUTE: &str = "Lucene90StoredFieldsFormat.mode";
pub const STORED_FIELDS_MODE_BEST_SPEED: &str = "BEST_SPEED";

/// index/SegmentInfo.java (writer side).
pub struct SegmentInfo {
    /// e.g. "_0" ("_" + base36(counter), IndexWriter.java:2051-2064).
    pub name: String,
    /// 16-byte StringHelper.randomId()-equivalent.
    pub id: [u8; 16],
    /// Lucene version that created the segment, (major, minor, bugfix).
    pub version: (i32, i32, i32),
    /// Minimum Lucene version that contributed docs; must be Some when
    /// indexCreatedVersionMajor >= 7 (SegmentInfos.java:636-640).
    pub min_version: Option<(i32, i32, i32)>,
    pub doc_count: i32,
    pub is_compound_file: bool,
    pub has_blocks: bool,
    pub diagnostics: BTreeMap<String, String>,
    /// All files of this segment; every one must carry the segment name as
    /// prefix (checked, :214-219).
    pub files: BTreeSet<String>,
    pub attributes: BTreeMap<String, String>,
}

impl SegmentInfo {
    pub fn new(name: &str, id: [u8; 16], doc_count: i32) -> Self {
        SegmentInfo {
            name: name.to_string(),
            id,
            version: (9, 12, 3),
            min_version: Some((9, 12, 3)),
            doc_count,
            is_compound_file: false,
            has_blocks: false,
            diagnostics: BTreeMap::new(),
            files: BTreeSet::new(),
            attributes: BTreeMap::new(),
        }
    }

    fn file_name(&self, suffix: &str) -> String {
        format!("{}{suffix}.{EXTENSION}", self.name)
    }

    /// Lucene99SegmentInfoFormat.write/writeSegmentInfo (:165-235).
    pub fn write(&self, dir: &FSDirectory, suffix: &str) -> io::Result<String> {
        // validate files prefix, IndexFileNames.indexOfSegmentName (:119-127):
        // segment name ends at the second '_' (suffix files) or the first '.'
        for file in &self.files {
            let idx = file[1..]
                .find('_')
                .map(|i| i + 1)
                .or_else(|| file.find('.'))
                .unwrap_or(file.len());
            let segment_of_file = &file[..idx];
            if segment_of_file != self.name {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "invalid files: expected segment={}, file={}",
                        self.name, file
                    ),
                ));
            }
        }

        let file_name = self.file_name(suffix);
        let mut out = dir.create_output(&file_name)?;
        write_index_header(&mut out, CODEC_NAME, VERSION_CURRENT, &self.id, suffix)?;
        let (major, minor, bugfix) = self.version;
        out.write_int(major)?;
        out.write_int(minor)?;
        out.write_int(bugfix)?;
        match self.min_version {
            Some((major, minor, bugfix)) => {
                out.write_byte(1)?;
                out.write_int(major)?;
                out.write_int(minor)?;
                out.write_int(bugfix)?;
            }
            None => out.write_byte(0)?,
        }
        out.write_int(self.doc_count)?;
        out.write_byte(if self.is_compound_file { YES } else { NO } as u8)?;
        out.write_byte(if self.has_blocks { YES } else { NO } as u8)?;
        out.write_map_of_strings(&self.diagnostics)?;
        out.write_set_of_strings(&self.files)?;
        out.write_map_of_strings(&self.attributes)?;
        // no index sort
        out.write_vint(0)?;
        write_footer(&mut out)?;
        out.flush()?;
        Ok(file_name)
    }

    /// Lucene99SegmentInfoFormat.read/parseSegmentInfo (:91-168): mirror of
    /// [`SegmentInfo::write`]. The index sort must be absent (we never
    /// write one).
    pub fn read(
        dir: &FSDirectory,
        name: &str,
        expected_id: &[u8; 16],
        suffix: &str,
    ) -> io::Result<SegmentInfo> {
        let file_name = format!("{name}{suffix}.{EXTENSION}");
        let mut input = dir.open_checksum_input(&file_name)?;
        check_index_header(
            &mut input,
            CODEC_NAME,
            VERSION_CURRENT,
            VERSION_CURRENT,
            expected_id,
            suffix,
        )?;
        let version = (input.read_int()?, input.read_int()?, input.read_int()?);
        let min_version = match input.read_byte()? {
            0 => None,
            1 => Some((input.read_int()?, input.read_int()?, input.read_int()?)),
            b => return Err(corrupt(format!("invalid minVersion marker {b}"))),
        };
        let doc_count = input.read_int()?;
        if doc_count < 0 {
            return Err(corrupt(format!("invalid docCount {doc_count}")));
        }
        let is_compound_file = input.read_byte()? == YES as u8;
        let has_blocks = input.read_byte()? == YES as u8;
        let diagnostics = input.read_map_of_strings()?;
        let files = input.read_set_of_strings()?;
        let attributes = input.read_map_of_strings()?;
        let num_sort_fields = input.read_vint()?;
        if num_sort_fields != 0 {
            return Err(corrupt(format!(
                "unsupported: index sort ({num_sort_fields} fields)"
            )));
        }
        check_footer(&mut input)?;
        Ok(SegmentInfo {
            name: name.to_string(),
            id: *expected_id,
            version,
            min_version,
            doc_count,
            is_compound_file,
            has_blocks,
            diagnostics,
            files,
            attributes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("codec-lucene9-si-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn writes_and_validates_files_prefix() {
        let root = temp_dir("prefix");
        let dir = FSDirectory::open(&root).unwrap();
        let mut si = SegmentInfo::new("_0", [7u8; 16], 3);
        si.files.insert("_0.fdt".to_string());
        si.files.insert("_0.si".to_string());
        si.attributes.insert(
            STORED_FIELDS_MODE_ATTRIBUTE.to_string(),
            STORED_FIELDS_MODE_BEST_SPEED.to_string(),
        );
        let name = si.write(&dir, "").unwrap();
        assert_eq!(name, "_0.si");
        assert!(dir.file_exists("_0.si"));

        let mut bad = SegmentInfo::new("_0", [7u8; 16], 3);
        bad.files.insert("_1.fdt".to_string());
        assert!(bad.write(&dir, "").is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn si_round_trip() {
        let root = temp_dir("si_read");
        let dir = FSDirectory::open(&root).unwrap();
        let mut si = SegmentInfo::new("_0", [7u8; 16], 42);
        si.files.insert("_0.si".to_string());
        si.diagnostics.insert("os".to_string(), "Linux".to_string());
        si.attributes.insert(
            STORED_FIELDS_MODE_ATTRIBUTE.to_string(),
            STORED_FIELDS_MODE_BEST_SPEED.to_string(),
        );
        si.write(&dir, "").unwrap();

        let back = SegmentInfo::read(&dir, "_0", &[7u8; 16], "").unwrap();
        assert_eq!(back.name, "_0");
        assert_eq!(back.id, [7u8; 16]);
        assert_eq!(back.version, (9, 12, 3));
        assert_eq!(back.min_version, Some((9, 12, 3)));
        assert_eq!(back.doc_count, 42);
        assert!(!back.is_compound_file && !back.has_blocks);
        assert_eq!(back.diagnostics.get("os").unwrap(), "Linux");
        assert!(back.files.contains("_0.si"));
        assert_eq!(
            back.attributes.get(STORED_FIELDS_MODE_ATTRIBUTE).unwrap(),
            STORED_FIELDS_MODE_BEST_SPEED
        );
        // wrong id rejected
        assert!(SegmentInfo::read(&dir, "_0", &[9u8; 16], "").is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
