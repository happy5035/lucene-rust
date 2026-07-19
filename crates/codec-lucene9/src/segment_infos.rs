//! `segments_N` writing with the two-phase commit protocol, mirroring
//! `index/SegmentInfos.java` (write :596-699, prepareCommit :915-921,
//! finishCommit :945-977) and `index/IndexFileNames.java` (9.12.3).

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use crate::codec_util::{write_be_int, write_be_long, write_footer, write_index_header};
use crate::directory::FSDirectory;
use crate::io::ChecksumIndexOutput;
use crate::segment_info::SegmentInfo;

const CODEC_NAME: &str = "segments";
/// SegmentInfos.VERSION_CURRENT (:120-131).
pub const VERSION_CURRENT: u32 = 10; // VERSION_86
/// IndexFileNames.SEGMENTS / PENDING_SEGMENTS (:40-43).
pub const SEGMENTS: &str = "segments";
pub const PENDING_SEGMENTS: &str = "pending_segments";

/// The Lucene version triple written into the header (util/Version.java:347,363).
pub const LUCENE_VERSION: (i32, i32, i32) = (9, 12, 3);
/// IndexFileNames uses base36 for generations (Character.MAX_RADIX).
const BASE36: u32 = 36;

/// IndexFileNames.fileNameFromGeneration (:55-75): no suffix for gen 0.
pub fn file_name_from_generation(base: &str, generation: i64) -> String {
    if generation == 0 {
        base.to_string()
    } else {
        format!("{base}_{}", to_base36(generation))
    }
}

fn to_base36(mut v: i64) -> String {
    debug_assert!(v >= 0);
    // Long.toString(v, 36); "0" for 0
    if v == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    while v > 0 {
        let d = (v % BASE36 as i64) as u8;
        digits.push(if d < 10 { b'0' + d } else { b'a' + d - 10 });
        v /= BASE36 as i64;
    }
    digits.reverse();
    String::from_utf8(digits).unwrap()
}

/// One segment entry in the commit (index/SegmentCommitInfo.java).
pub struct SegmentCommitInfo {
    pub info: SegmentInfo,
    /// -1 = no deletes (BE long).
    pub del_gen: i64,
    pub del_count: i32,
    /// -1 = no field infos updates (BE long).
    pub field_infos_gen: i64,
    /// -1 = no docvalues updates (BE long).
    pub doc_values_gen: i64,
    pub soft_del_count: i32,
    /// SegmentCommitInfo id; written as marker byte 1 + 16 raw bytes (:681-688).
    pub id: Option<[u8; 16]>,
    pub field_infos_files: BTreeSet<String>,
    /// field number -> files (empty for a fresh segment).
    pub doc_values_updates: BTreeMap<i32, BTreeSet<String>>,
}

impl SegmentCommitInfo {
    /// A fresh segment: no deletes, no updates, fresh SCI id.
    pub fn new(info: SegmentInfo, id: [u8; 16]) -> Self {
        SegmentCommitInfo {
            info,
            del_gen: -1,
            del_count: 0,
            field_infos_gen: -1,
            doc_values_gen: -1,
            soft_del_count: 0,
            id: Some(id),
            field_infos_files: BTreeSet::new(),
            doc_values_updates: BTreeMap::new(),
        }
    }
}

/// index/SegmentInfos.java (writer side).
pub struct SegmentInfos {
    /// Index-wide version counter (BE long); semantics belong to IndexWriter,
    /// readers do not validate the absolute value.
    pub version: i64,
    /// Next segment name counter (VLong).
    pub counter: i64,
    pub index_created_version_major: i32,
    pub min_segment_version: Option<(i32, i32, i32)>,
    pub segments: Vec<SegmentCommitInfo>,
    pub user_data: BTreeMap<String, String>,
}

impl SegmentInfos {
    pub fn new() -> Self {
        SegmentInfos {
            version: 0,
            counter: 0,
            index_created_version_major: 9,
            min_segment_version: None,
            segments: Vec::new(),
            user_data: BTreeMap::new(),
        }
    }

    /// SegmentInfos.write (:596-699).
    pub fn write(&self, out: &mut ChecksumIndexOutput, generation: i64) -> io::Result<()> {
        let id = random_id();
        // header suffix = Long.toString(generation, 36) (:597-602)
        write_index_header(out, CODEC_NAME, VERSION_CURRENT, &id, &to_base36(generation))?;
        let (major, minor, bugfix) = LUCENE_VERSION;
        out.write_vint(major)?;
        out.write_vint(minor)?;
        out.write_vint(bugfix)?;
        out.write_vint(self.index_created_version_major)?;
        write_be_long(out, self.version as u64)?;
        out.write_vlong(self.counter)?;
        write_be_int(out, self.segments.len() as u32)?;

        if !self.segments.is_empty() {
            let (major, minor, bugfix) = self.min_segment_version.unwrap_or(LUCENE_VERSION);
            out.write_vint(major)?;
            out.write_vint(minor)?;
            out.write_vint(bugfix)?;
        }

        for sci in &self.segments {
            let si = &sci.info;
            if self.index_created_version_major >= 7 && si.min_version.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "segments must record minVersion (SegmentInfos.java:636-640)",
                ));
            }
            out.write_string(&si.name)?;
            out.write_bytes(&si.id)?;
            out.write_string("Lucene912")?; // Codec name, Codec.forName on read (:521-524)
            write_be_long(out, sci.del_gen as u64)?;
            write_be_int(out, sci.del_count as u32)?;
            write_be_long(out, sci.field_infos_gen as u64)?;
            write_be_long(out, sci.doc_values_gen as u64)?;
            write_be_int(out, sci.soft_del_count as u32)?;
            match &sci.id {
                Some(id) => {
                    out.write_byte(1)?;
                    out.write_bytes(id)?;
                }
                None => out.write_byte(0)?,
            }
            out.write_set_of_strings(&sci.field_infos_files)?;
            write_be_int(out, sci.doc_values_updates.len() as u32)?;
            for (field_number, files) in &sci.doc_values_updates {
                write_be_int(out, *field_number as u32)?;
                out.write_set_of_strings(files)?;
            }
        }
        out.write_map_of_strings(&self.user_data)?;
        write_footer(out)
    }

    /// Phase 1 of the commit (SegmentInfos.prepareCommit :915-921 + write
    /// :563-593): sync directory metadata, write `pending_segments_N`, fsync it.
    /// Returns the pending file name.
    pub fn prepare_commit(&self, dir: &FSDirectory, generation: i64) -> io::Result<String> {
        dir.sync_metadata()?;
        let pending_name = file_name_from_generation(PENDING_SEGMENTS, generation);
        let mut out = dir.create_output(&pending_name)?;
        let result = self.write(&mut out, generation).and_then(|_| out.flush());
        if let Err(e) = result {
            // rollback like SegmentInfos.finishCommit's failure path (:891-905)
            drop(out);
            let _ = dir.delete(&pending_name);
            return Err(e);
        }
        dir.sync(&[&pending_name])?;
        Ok(pending_name)
    }

    /// Phase 2 (SegmentInfos.finishCommit :945-977): rename pending to
    /// `segments_N` and fsync directory metadata again.
    pub fn finish_commit(dir: &FSDirectory, generation: i64) -> io::Result<()> {
        let pending_name = file_name_from_generation(PENDING_SEGMENTS, generation);
        let final_name = file_name_from_generation(SEGMENTS, generation);
        dir.rename(&pending_name, &final_name)?;
        dir.sync_metadata()
    }

    /// Convenience: full two-phase commit with rollback of the pending file on
    /// failure.
    pub fn commit(&self, dir: &FSDirectory, generation: i64) -> io::Result<()> {
        let pending_name = self.prepare_commit(dir, generation)?;
        if let Err(e) = Self::finish_commit(dir, generation) {
            let _ = dir.delete(&pending_name);
            return Err(e);
        }
        Ok(())
    }
}

/// 16 random bytes, equivalent of StringHelper.randomId()
/// (util/StringHelper.java:297-329).
pub fn random_id() -> [u8; 16] {
    rand::random()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_file_names() {
        assert_eq!(file_name_from_generation("segments", 0), "segments");
        assert_eq!(file_name_from_generation("segments", 1), "segments_1");
        assert_eq!(file_name_from_generation("segments", 35), "segments_z");
        assert_eq!(file_name_from_generation("segments", 36), "segments_10");
        assert_eq!(
            file_name_from_generation("pending_segments", 2),
            "pending_segments_2"
        );
    }

    #[test]
    fn base36() {
        assert_eq!(to_base36(1), "1");
        assert_eq!(to_base36(10), "a");
        assert_eq!(to_base36(36), "10");
        assert_eq!(to_base36(36 * 36 + 35), "10z");
    }
}
