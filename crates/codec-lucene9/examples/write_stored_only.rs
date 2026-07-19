//! Writes a complete single-segment index with 3 stored-only documents to
//! /tmp/rustlucene-m1a, exercising the full format layer:
//! `_0.si` / `_0.fnm` / `_0.fdt` / `_0.fdx` / `_0.fdm` + two-phase `segments_1`.
//!
//! Verified by interop/java/VerifyStored.java (DirectoryReader + CheckIndex).

use codec_lucene9::field_infos::FieldInfos;
use codec_lucene9::segment_info::{STORED_FIELDS_MODE_ATTRIBUTE, STORED_FIELDS_MODE_BEST_SPEED};
use codec_lucene9::segment_infos::random_id;
use codec_lucene9::{
    FSDirectory, FieldInfo, SegmentCommitInfo, SegmentInfo, SegmentInfos, StoredField,
    StoredFieldsWriter,
};

const OUTPUT_DIR: &str = "/tmp/rustlucene-m1a";

fn main() -> std::io::Result<()> {
    let dir = FSDirectory::open(OUTPUT_DIR)?;
    let segment = "_0";
    let suffix = "";
    let segment_id = random_id();

    let bodies = [
        "the quick brown fox jumps over the lazy dog",
        "lucene stored fields best speed lz4 chunk format",
        "rust writes, java reads: ロシア語ではないが UTF-8 も OK — Grüße!",
    ];

    // FieldInfos: one stored-only field (indexOptions NONE, omitNorms must
    // stay false on non-indexed fields, FieldInfo.checkConsistency:147-149).
    let field_infos = FieldInfos::new(vec![FieldInfo::stored("body", 0)]);
    let fnm_name = field_infos.write(&dir, segment, &segment_id, suffix)?;

    // Stored fields: fdt/fdx/fdm.
    let mut writer = StoredFieldsWriter::new(&dir, segment, segment_id, suffix)?;
    for body in bodies {
        writer.write_document(&[(0, StoredField::String(body.to_string()))])?;
    }
    let stats = writer.finish(bodies.len() as i32, &dir)?;

    // SegmentInfo: non-compound, BEST_SPEED attribute is mandatory for the
    // stored fields reader (Lucene90StoredFieldsFormat.java:113-114).
    let mut si = SegmentInfo::new(segment, segment_id, bodies.len() as i32);
    si.files
        .extend([fnm_name, stats.fdt_name, stats.fdx_name, stats.fdm_name]);
    si.files.insert(format!("{segment}.si"));
    si.attributes.insert(
        STORED_FIELDS_MODE_ATTRIBUTE.to_string(),
        STORED_FIELDS_MODE_BEST_SPEED.to_string(),
    );
    si.diagnostics
        .insert("source".to_string(), "flush".to_string());
    let si_name = si.write(&dir, suffix)?;

    // Two-phase commit of segments_1.
    let mut infos = SegmentInfos::new();
    infos.version = 1;
    infos.counter = 1;
    infos.min_segment_version = si.min_version;
    infos.segments.push(SegmentCommitInfo::new(si, random_id()));
    infos.commit(&dir, 1)?;

    let mut files = dir.list_all()?;
    files.sort();
    println!("wrote {OUTPUT_DIR}:");
    for f in &files {
        let len = std::fs::metadata(dir.path().join(f))?.len();
        println!("  {f} ({len} bytes)");
    }
    assert!(files.contains(&"segments_1".to_string()));
    assert!(files.contains(&si_name));
    Ok(())
}
