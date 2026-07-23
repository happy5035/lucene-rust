//! `segments_N` writing with the two-phase commit protocol, mirroring
//! `index/SegmentInfos.java` (write :596-699, prepareCommit :915-921,
//! finishCommit :945-977) and `index/IndexFileNames.java` (9.12.3).

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use crate::codec_util::{
    check_footer, check_header, check_index_header_suffix, corrupt, read_be_int, read_be_long,
    write_be_int, write_be_long, write_footer, write_index_header,
};
use crate::directory::FSDirectory;
use crate::io::{ChecksumIndexOutput, DataInput};
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
        write_index_header(
            out,
            CODEC_NAME,
            VERSION_CURRENT,
            &id,
            &to_base36(generation),
        )?;
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

    /// SegmentInfos.readCommit (:327-389) + parseSegmentInfos (:391-519).
    /// Reads `segments_<base36 generation>`; the commit id is parsed but not
    /// validated (:341-342 reads it without comparison). Enforces the spec §1
    /// premise: no deletes, no field-info/docvalues updates.
    pub fn read_commit(dir: &FSDirectory, generation: i64) -> io::Result<SegmentInfos> {
        let file_name = file_name_from_generation(SEGMENTS, generation);
        let mut input = dir.open_checksum_input(&file_name)?;
        let _format = check_header(&mut input, CODEC_NAME, 7, VERSION_CURRENT)?; // VERSION_70..=VERSION_86
        let mut commit_id = [0u8; 16];
        input.read_bytes(&mut commit_id)?; // :341-342, unverified by design
        check_index_header_suffix(&mut input, &to_base36(generation))?; // :343
        let _lucene_version = (input.read_vint()?, input.read_vint()?, input.read_vint()?); // :345-346
        let index_created_version_major = input.read_vint()?; // :347
        let version = read_be_long(&mut input)? as i64; // :393
        let counter = input.read_vlong()?; // :395-399
        let num_segments = read_be_int(&mut input)? as usize; // :400
        let min_segment_version = if num_segments > 0 {
            Some((input.read_vint()?, input.read_vint()?, input.read_vint()?)) // :405-410
        } else {
            None
        };
        let mut segments = Vec::with_capacity(num_segments);
        for _ in 0..num_segments {
            let seg_name = input.read_string()?; // :414
            let mut seg_id = [0u8; 16];
            input.read_bytes(&mut seg_id)?; // :415-416
            let codec = input.read_string()?; // :417
            if codec != "Lucene912" {
                return Err(corrupt(format!("unsupported codec {codec}")));
            }
            let del_gen = read_be_long(&mut input)? as i64; // :422
            let del_count = read_be_int(&mut input)? as i32; // :423
            let field_infos_gen = read_be_long(&mut input)? as i64; // :428
            let doc_values_gen = read_be_long(&mut input)? as i64; // :429
            let soft_del_count = read_be_int(&mut input)? as i32; // :430
            let id = match input.read_byte()? {
                // :441-457
                1 => {
                    let mut b = [0u8; 16];
                    input.read_bytes(&mut b)?;
                    Some(b)
                }
                0 => None,
                b => return Err(corrupt(format!("invalid SCI id marker {b}"))),
            };
            let field_infos_files = input.read_set_of_strings()?; // :460
            let num_dv_fields = read_be_int(&mut input)?; // :462-471
            let mut doc_values_updates = BTreeMap::new();
            for _ in 0..num_dv_fields {
                let field_number = read_be_int(&mut input)? as i32;
                let files = input.read_set_of_strings()?;
                doc_values_updates.insert(field_number, files);
            }
            // spec §1 premise: our own indexes only (no deletes/updates)
            if del_gen != -1
                || del_count != 0
                || field_infos_gen != -1
                || doc_values_gen != -1
                || soft_del_count != 0
                || !doc_values_updates.is_empty()
            {
                return Err(corrupt(
                    "unsupported: live docs / field-info / docvalues updates (spec §1)",
                ));
            }
            // codec.segmentInfoFormat().read (:418-422): the .si file is
            // parsed here, between codec name and delGen in stream order.
            let info = SegmentInfo::read(dir, &seg_name, &seg_id, "")?;
            segments.push(SegmentCommitInfo {
                info,
                del_gen,
                del_count,
                field_infos_gen,
                doc_values_gen,
                soft_del_count,
                id,
                field_infos_files,
                doc_values_updates,
            });
        }
        let user_data = input.read_map_of_strings()?; // :508
        check_footer(&mut input)?; // :379-387
        Ok(SegmentInfos {
            version,
            counter,
            index_created_version_major,
            min_segment_version,
            segments,
            user_data,
        })
    }

    /// SegmentInfos.readLatestCommit (:539-557) over
    /// getLastCommitGeneration (:201-215): highest base36 generation among
    /// `segments_*` files (excluding `segments.gen` and pending commits).
    pub fn read_latest(dir: &FSDirectory) -> io::Result<(SegmentInfos, i64)> {
        let mut best: Option<i64> = None;
        for name in dir.list_all()? {
            if !name.starts_with(SEGMENTS) || name == "segments.gen" {
                continue;
            }
            let Some(gen_str) = name[SEGMENTS.len()..].strip_prefix('_') else {
                continue; // bare "segments" is not a commit file
            };
            // generationFromSegmentsFileName (:254-266)
            let Ok(gen_val) = i64::from_str_radix(gen_str, BASE36) else {
                continue;
            };
            best = Some(best.map_or(gen_val, |b: i64| b.max(gen_val)));
        }
        let generation = best
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no segments_N commit found"))?;
        Ok((Self::read_commit(dir, generation)?, generation))
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
    use crate::segment_info::SegmentInfo;
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("codec-lucene9-sis-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn commit_one(
        dir: &FSDirectory,
        infos: &mut SegmentInfos,
        name: &str,
        id: [u8; 16],
        generation: i64,
    ) {
        let mut si = SegmentInfo::new(name, id, 10);
        si.files.insert(format!("{name}.si"));
        si.write(dir, "").unwrap();
        infos.segments.push(SegmentCommitInfo::new(si, id));
        infos.commit(dir, generation).unwrap();
    }

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

    #[test]
    fn segments_round_trip_and_latest_generation() {
        let root = temp_dir("read_commit");
        let dir = FSDirectory::open(&root).unwrap();
        let mut infos = SegmentInfos::new();
        infos
            .user_data
            .insert("commit".to_string(), "first".to_string());
        commit_one(&dir, &mut infos, "_0", [1u8; 16], 1);
        commit_one(&dir, &mut infos, "_1", [2u8; 16], 2);

        // read_latest picks the highest generation and parses both segments
        let (back, gen_val) = SegmentInfos::read_latest(&dir).unwrap();
        assert_eq!(gen_val, 2);
        assert_eq!(back.segments.len(), 2);
        assert_eq!(back.segments[0].info.name, "_0");
        assert_eq!(back.segments[0].info.doc_count, 10);
        assert_eq!(back.segments[0].id, Some([1u8; 16]));
        assert_eq!(back.segments[1].info.name, "_1");
        assert_eq!(back.segments[1].del_gen, -1);
        assert_eq!(back.segments[1].field_infos_gen, -1);
        assert_eq!(back.segments[1].doc_values_gen, -1);
        assert_eq!(back.user_data.get("commit").unwrap(), "first");

        // read_commit reads a specific older generation
        let gen1 = SegmentInfos::read_commit(&dir, 1).unwrap();
        assert_eq!(gen1.segments.len(), 1);
        assert_eq!(gen1.segments[0].info.name, "_0");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn corrupted_commit_rejected() {
        let root = temp_dir("corrupt_commit");
        let dir = FSDirectory::open(&root).unwrap();
        let mut infos = SegmentInfos::new();
        commit_one(&dir, &mut infos, "_0", [1u8; 16], 1);
        // flip a byte in the middle of segments_1
        let path = root.join("segments_1");
        let mut bytes = std::fs::read(&path).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&path, bytes).unwrap();
        assert!(SegmentInfos::read_commit(&dir, 1).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
