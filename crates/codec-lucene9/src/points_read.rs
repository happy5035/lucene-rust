//! Lucene 9.12.3-compatible 1D Points (BKD tree) reader
//! (`codecs/lucene90/Lucene90PointsReader.java`): `.kdm` per-field metadata,
//! `.kdi` packed inner-node index (fully resident at open), `.kdd` leaf
//! blocks read on demand. Reverse of `points.rs` (the writer); format
//! citations point at `util/bkd/BKDReader.java` / `util/bkd/DocIdsWriter.java`.
//!
//! Scope mirrors the writer: numDims == numIndexDims == 1, long (8) and
//! int (4) bytes-per-dim. All comparisons run on unpacked i64 values:
//! sortable-byte unsigned order == signed i64 order
//! (NumericUtils.longToSortableBytes :210-214), so the 1D cell relation
//! (PointRangeQuery.relate :145-167) on unpacked values is identical to
//! Lucene's unsigned byte compare.
//!
//! Deviation (deliberate): CELL_INSIDE subtrees decode whole leaves
//! including values (Lucene's addAll :562-586 decodes only doc ids) —
//! the pinned visitor carries (value, doc) for T-C point enumeration.
//! The dominant I/O saving is preserved: disjoint subtrees are never
//! read from `.kdd` (leaf fps come from the in-memory packed index).
//!
//! Header checks follow the codec read-side convention
//! (postings_read.rs:35-90): each of the three files goes through
//! `check_index_header(codec, VERSION, VERSION, segment_id, "")` at open
//! (Lucene90PointsReader :63-93).

use std::io;

use crate::codec_util::{
    check_footer, check_footer_structure, check_header, check_index_header, corrupt,
};
use crate::directory::FSDirectory;
use crate::field_infos::FieldInfos;
use crate::io::{ChecksumIndexInput, DataInput, IndexInput};
use crate::points::{
    BKD_CODEC_NAME, BKD_VERSION, DATA_CODEC_NAME, FORMAT_VERSION, INDEX_CODEC_NAME,
    MAX_POINTS_IN_LEAF_NODE, META_CODEC_NAME, file_names, get_num_left_leaf_nodes,
};

/// One field's `.kdm` entry + fully decoded packed index (BKDReader ctor
/// :56-113 + BKDPointTree readNodeData :657-717, decoded eagerly at open).
struct FieldMeta {
    field_number: i32,
    bytes_per_dim: usize,
    num_leaves: usize,
    min_value: i64,
    max_value: i64,
    point_count: u64,
    doc_count: u32,
    data_start_fp: u64,
    index_start_fp: u64,
    packed_index_byte_length: usize,
    /// Leaf block fp per leaf, in leaves_offset (in-order) sequence.
    leaf_fps: Vec<u64>,
    /// Split value per inner-node boundary, indexed by
    /// `split_offset == right_offset - 1`; len == num_leaves - 1.
    splits: Vec<i64>,
}

/// Per-segment points reader: `.kdd` random-access stream + one fully
/// resident packed index per point field (1D trees are tiny).
/// `Ok(None)` from `open` when the segment has no points files.
pub struct PointsReader {
    data_in: IndexInput,
    /// (field name, meta) — names resolved from `field_infos` at open.
    fields: Vec<(String, FieldMeta)>,
}

/// NumericUtils.sortableBytesToLong (:221-227) / sortableBytesToInt
/// (:198-202; the writer zero-pads int values into the high 4 bytes,
/// points.rs:70-76): packed big-endian sortable bytes → signed value.
fn unpack_value(packed: &[u8; 8], bytes_per_dim: usize) -> i64 {
    if bytes_per_dim == 8 {
        (u64::from_be_bytes(*packed) ^ 0x8000_0000_0000_0000) as i64
    } else {
        debug_assert_eq!(bytes_per_dim, 4);
        ((u32::from_be_bytes(packed[..4].try_into().unwrap()) ^ 0x8000_0000) as i32) as i64
    }
}

impl PointsReader {
    /// Lucene90PointsReader ctor (:41-123): opens the three files, checks
    /// each index header against `segment_id` (CodecUtil.checkIndexHeader
    /// :246-258, postings_read.rs:35-90 convention), parses every `.kdm`
    /// field entry, reconciles the recorded `.kdi`/`.kdd` lengths, and
    /// decodes each field's packed index fully into memory.
    pub fn open(
        dir: &FSDirectory,
        segment: &str,
        segment_id: &[u8; 16],
        field_infos: &FieldInfos,
    ) -> io::Result<Option<PointsReader>> {
        let [data_name, index_name, meta_name] = file_names(segment);
        if !dir.file_exists(&meta_name) {
            return Ok(None);
        }

        // .kdm: full sequential checksum read (Lucene90PointsReader :83-112)
        let mut meta_in = dir.open_checksum_input(&meta_name)?;
        check_index_header(
            &mut meta_in,
            META_CODEC_NAME,
            FORMAT_VERSION,
            FORMAT_VERSION,
            segment_id,
            "",
        )?;
        let mut fields: Vec<(String, FieldMeta)> = Vec::new();
        loop {
            let field_number = meta_in.read_int()?; // :96
            if field_number == -1 {
                break;
            }
            if field_number < 0 {
                return Err(corrupt(format!(
                    "illegal field number {field_number} (:99-100)"
                )));
            }
            fields.push(read_field_meta(&mut meta_in, field_infos, field_number)?);
        }
        // footer reconciliation (:105-106)
        let index_length = meta_in.read_long()? as u64;
        let data_length = meta_in.read_long()? as u64;
        check_footer(&mut meta_in)?;

        // .kdi: header + recorded length; each field's packed index is
        // sliced out and decoded eagerly (retrieveChecksum :115)
        let mut index_in = dir.open_input(&index_name)?;
        check_index_header(
            &mut index_in,
            INDEX_CODEC_NAME,
            FORMAT_VERSION,
            FORMAT_VERSION,
            segment_id,
            "",
        )?;
        check_footer_structure(&index_in, index_length)?;
        for (_, m) in &mut fields {
            let packed = index_in.slice(m.index_start_fp, m.packed_index_byte_length as u64)?;
            decode_packed_index(packed, m)?;
        }

        // .kdd: header + recorded length; leaves are random-access reads (:116)
        let mut data_in = dir.open_input(&data_name)?;
        check_index_header(
            &mut data_in,
            DATA_CODEC_NAME,
            FORMAT_VERSION,
            FORMAT_VERSION,
            segment_id,
            "",
        )?;
        check_footer_structure(&data_in, data_length)?;

        Ok(Some(PointsReader { data_in, fields }))
    }
}

/// One `.kdm` field entry (Lucene90PointsWriter.writeField :144-148 +
/// BKDWriter finalizer :1236-1264, reversed; BKDReader ctor :56-113).
fn read_field_meta(
    meta_in: &mut ChecksumIndexInput,
    field_infos: &FieldInfos,
    field_number: i32,
) -> io::Result<(String, FieldMeta)> {
    // CodecUtil.checkHeader("BKD", 9) (:57-59); our writer always emits
    // VERSION_CURRENT == VERSION_META_FILE == 9 (BKDWriter.java:88-89)
    check_header(meta_in, BKD_CODEC_NAME, BKD_VERSION, BKD_VERSION)?;
    let num_dims = meta_in.read_vint()?; // :60
    let num_index_dims = meta_in.read_vint()?; // :63 (version >= SELECTIVE_INDEXING)
    if num_dims != 1 || num_index_dims != 1 {
        return Err(corrupt(format!(
            "only 1D points are supported: numDims={num_dims} numIndexDims={num_index_dims}"
        )));
    }
    let max_points_in_leaf = meta_in.read_vint()?; // :67
    if max_points_in_leaf as usize != MAX_POINTS_IN_LEAF_NODE {
        return Err(corrupt(format!(
            "maxPointsInLeafNode {max_points_in_leaf} != {MAX_POINTS_IN_LEAF_NODE}"
        )));
    }
    let bytes_per_dim = meta_in.read_vint()? as usize; // :68
    if bytes_per_dim != 4 && bytes_per_dim != 8 {
        return Err(corrupt(format!("unsupported bytesPerDim {bytes_per_dim}")));
    }
    let num_leaves = meta_in.read_vint()? as usize; // :72
    if num_leaves == 0 {
        return Err(corrupt("numLeaves == 0"));
    }
    let mut min_packed = [0u8; 8]; // :75-79
    meta_in.read_bytes(&mut min_packed[..bytes_per_dim])?;
    let mut max_packed = [0u8; 8];
    meta_in.read_bytes(&mut max_packed[..bytes_per_dim])?;
    if min_packed[..bytes_per_dim] > max_packed[..bytes_per_dim] {
        return Err(corrupt("minPackedValue > maxPackedValue (:82-95)"));
    }
    let point_count = meta_in.read_vlong()? as u64; // :97
    let doc_count = meta_in.read_vint()? as u32; // :98
    let packed_index_byte_length = meta_in.read_vint()? as usize; // :100
    let data_start_fp = meta_in.read_long()? as u64; // :102 (version >= META_FILE)
    let index_start_fp = meta_in.read_long()? as u64; // :103

    // 字段名解析 + 与 .fnm 交叉校验（Lucene90PointsReader.getValues
    // :131-141 的写侧对照：.kdm 条目一定来自有数据的 point 字段）
    let fi = field_infos
        .by_number(field_number)
        .ok_or_else(|| corrupt(format!("points field number {field_number} not in .fnm")))?;
    if fi.point_dimension_count != 1 || fi.point_num_bytes as usize != bytes_per_dim {
        return Err(corrupt(format!(
            "field {}: .fnm point config (dims={}, bytes={}) != .kdm entry (bytes={bytes_per_dim})",
            fi.name, fi.point_dimension_count, fi.point_num_bytes
        )));
    }

    Ok((
        fi.name.clone(),
        FieldMeta {
            field_number,
            bytes_per_dim,
            num_leaves,
            min_value: unpack_value(&min_packed, bytes_per_dim),
            max_value: unpack_value(&max_packed, bytes_per_dim),
            point_count,
            doc_count,
            data_start_fp,
            index_start_fp,
            packed_index_byte_length,
            leaf_fps: vec![0; num_leaves],
            splits: vec![0; num_leaves - 1],
        },
    ))
}

/// BKDPointTree packed-index decode (readNodeData :657-717 + the pre-order
/// child recursion :253-311), filling `leaf_fps` / `splits` in place.
/// Consumes the packed bytes exactly; trailing bytes are corruption.
fn decode_packed_index(mut input: IndexInput, m: &mut FieldMeta) -> io::Result<()> {
    // BKDPointTree ctor: nodeID=1, isLeft=false, minBlockFP=0, lastSplitValues
    // all-zero, negativeDeltas all-false (:253-254; writer packIndex :1025-1037)
    decode_node(&mut input, m, 0, [0u8; 8], false, false, 0, m.num_leaves)?;
    if input.file_pointer() != input.length() {
        return Err(corrupt("packed index has trailing bytes"));
    }
    Ok(())
}

/// readNodeData (:657-717) for one node covering
/// `leaves_offset..leaves_offset + num_leaves`, then pre-order recursion
/// into children. `min_block_fp` / `last_split_value` / `negative_delta`
/// are leafBlockFPStack[level-1] / splitValuesStack[level-1] /
/// negativeDeltas as set by the parent (:658-684); `total_num_leaves`
/// (the reader's leafNodeOffset, :481-483) is `m.num_leaves`.
#[allow(clippy::too_many_arguments)]
fn decode_node(
    input: &mut IndexInput,
    m: &mut FieldMeta,
    min_block_fp: u64,
    last_split_value: [u8; 8],
    negative_delta: bool,
    is_left: bool,
    leaves_offset: usize,
    num_leaves: usize,
) -> io::Result<()> {
    // leafBlockFPStack[level] = stack[level-1] (+ VLong delta if right) (:658-662)
    let mut fp = min_block_fp;
    if !is_left {
        fp += input.read_vlong()? as u64;
    }
    if num_leaves == 1 {
        m.leaf_fps[leaves_offset] = fp;
        return Ok(());
    }

    let code = input.read_vint()?; // :687
    // numIndexDims == 1 ⇒ splitDim == 0 and code stays whole (:688-690)
    let prefix = (code % (1 + m.bytes_per_dim as i32)) as usize; // :691
    let suffix = m.bytes_per_dim - prefix; // :692
    // splitValuesStack[level] starts as a copy of the parent's (:675-684)
    let mut split_value = last_split_value;
    if suffix > 0 {
        let mut first_diff_byte_delta = code / (1 + m.bytes_per_dim as i32); // :695
        if negative_delta {
            first_diff_byte_delta = -first_diff_byte_delta; // :696-698
        }
        let old_byte = i32::from(split_value[prefix]); // :700
        split_value[prefix] = (old_byte + first_diff_byte_delta) as u8; // :701
        input.read_bytes(&mut split_value[prefix + 1..prefix + 1 + (suffix - 1)])?; // :702
    }
    // else: split == last split on this dim (many duplicate values) (:703-706)

    let num_left = get_num_left_leaf_nodes(num_leaves);
    let right_offset = leaves_offset + num_left;
    m.splits[right_offset - 1] = unpack_value(&split_value, m.bytes_per_dim);

    // leftNumBytes present iff the left child is an inner node
    // (nodeID*2 < leafNodeOffset ⇔ num_left > 1) (:708-713)
    let left_num_bytes = if num_left > 1 {
        input.read_vint()? as u64 // :709
    } else {
        0
    };
    // rightNodePositions[level] (:714): the right subtree follows the left one
    let right_node_position = input.file_pointer() + left_num_bytes;
    decode_node(
        input,
        m,
        fp,
        split_value,
        true,
        true,
        leaves_offset,
        num_left,
    )?;
    if input.file_pointer() != right_node_position {
        return Err(corrupt("leftNumBytes measures a different left subtree"));
    }
    decode_node(
        input,
        m,
        fp,
        split_value,
        false,
        false,
        right_offset,
        num_leaves - num_left,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field_infos::FieldInfo;
    use crate::points::PointsWriter;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;

    // ---------- deterministic RNG (SplitMix64, points.rs 同式) ----------

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codec-lucene9-pointsread-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// PointsWriter 造段 + 配套 .fnm（postings_read.rs 测试同式：先写后读）。
    /// `long_fields`/`int_fields`: (field_number, field_name, points)。
    fn write_segment(
        tag: &str,
        long_fields: &[(i32, &str, Vec<(i64, u32)>)],
        int_fields: &[(i32, &str, Vec<(i32, u32)>)],
    ) -> (PathBuf, FieldInfos) {
        let root = temp_dir(tag);
        let dir = FSDirectory::open(&root).unwrap();
        let seg_id = [7u8; 16];
        let mut fis_vec = Vec::new();
        for &(number, name, _) in long_fields {
            let mut fi = FieldInfo::stored(name, number);
            fi.point_dimension_count = 1;
            fi.point_index_dimension_count = 1;
            fi.point_num_bytes = 8;
            fis_vec.push(fi);
        }
        for &(number, name, _) in int_fields {
            let mut fi = FieldInfo::stored(name, number);
            fi.point_dimension_count = 1;
            fi.point_index_dimension_count = 1;
            fi.point_num_bytes = 4;
            fis_vec.push(fi);
        }
        let fis = FieldInfos::new(fis_vec);
        fis.write(&dir, "_0", &seg_id, "").unwrap();
        let mut w = PointsWriter::new(&dir, "_0", &seg_id).unwrap();
        for (number, _, pts) in long_fields {
            w.write_field_long(*number, &mut pts.clone()).unwrap();
        }
        for (number, _, pts) in int_fields {
            w.write_field_int(*number, &mut pts.clone()).unwrap();
        }
        w.finish().unwrap();
        (root, fis)
    }

    fn open(root: &PathBuf, fis: &FieldInfos) -> PointsReader {
        let dir = FSDirectory::open(root).unwrap();
        PointsReader::open(&dir, "_0", &[7u8; 16], fis)
            .unwrap()
            .expect("has points")
    }

    fn gen_long_points(rng: &mut Rng, n: usize, doc_range: u64) -> Vec<(i64, u32)> {
        (0..n)
            .map(|_| {
                let value = match rng.below(6) {
                    0 => (rng.below(1_000)) as i64,
                    1 => (rng.below(100)) as i64 - 50,
                    2 => rng.next() as i64,
                    3 => [i64::MIN, i64::MAX, 0, -1, 1][rng.below(5) as usize],
                    4 => (rng.next() % 1_000_000) as i64,
                    _ => (rng.below(10)) as i64,
                };
                (value, rng.below(doc_range) as u32)
            })
            .collect()
    }

    #[test]
    fn open_parses_meta_and_decodes_index() {
        // 1200 点 → 3 叶（512+512+176）；doc 0..1200 全 distinct
        let points: Vec<(i64, u32)> = (0..1200u32).map(|i| (i as i64 * 7 - 3000, i)).collect();
        let (root, fis) = write_segment("open-meta", &[(0, "ts", points.clone())], &[]);
        let reader = open(&root, &fis);
        assert_eq!(reader.fields.len(), 1);
        let (name, m) = &reader.fields[0];
        assert_eq!(name, "ts");
        assert_eq!(m.field_number, 0);
        assert_eq!(m.bytes_per_dim, 8);
        assert_eq!(m.num_leaves, 3);
        assert_eq!(m.point_count, 1200);
        assert_eq!(m.doc_count, 1200);
        assert_eq!(m.min_value, -3000);
        assert_eq!(m.max_value, 1199 * 7 - 3000);
        // leaf fp 递增、首叶 fp == dataStartFP
        assert_eq!(m.leaf_fps.len(), 3);
        assert_eq!(m.leaf_fps[0], m.data_start_fp);
        assert!(m.leaf_fps[0] < m.leaf_fps[1] && m.leaf_fps[1] < m.leaf_fps[2]);
        // splits（split_offset 序）== 叶 1/叶 2 的首值（写侧 leaf_block_start_values,
        // points.rs:182-199）；值按 (value, doc) 排序后叶 i 首点即第 512*i 个点
        let mut sorted = points.clone();
        sorted.sort();
        assert_eq!(m.splits, vec![sorted[512].0, sorted[1024].0]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn open_multi_field_and_doc_count_dedup() {
        let mut rng = Rng(11);
        let longs = gen_long_points(&mut rng, 5000, 3000); // 多值点 + 重复值
        let ints: Vec<(i32, u32)> = (0..700u32).map(|i| ((i % 97) as i32, i / 2)).collect();
        let (root, fis) = write_segment(
            "open-multi",
            &[(2, "ts", longs.clone())],
            &[(5, "lvl", ints.clone())],
        );
        let reader = open(&root, &fis);
        assert_eq!(reader.fields.len(), 2);
        let (n0, m0) = &reader.fields[0];
        assert_eq!(
            (n0.as_str(), m0.field_number, m0.bytes_per_dim),
            ("ts", 2, 8)
        );
        assert_eq!(m0.point_count, 5000);
        assert_eq!(
            m0.doc_count as usize,
            longs.iter().map(|p| p.1).collect::<BTreeSet<_>>().len(),
            "docCount counts distinct docs (BKDWriter :1256)"
        );
        assert_eq!(m0.num_leaves, 10); // ceil(5000/512)
        assert_eq!(m0.splits.len(), 9);
        let (n1, m1) = &reader.fields[1];
        assert_eq!(
            (n1.as_str(), m1.field_number, m1.bytes_per_dim),
            ("lvl", 5, 4)
        );
        assert_eq!(m1.num_leaves, 2); // ceil(700/512)
        assert_eq!(m1.min_value, 0);
        assert_eq!(m1.max_value, 96);
        // 第二个字段的 dataStartFP 紧随第一个字段的数据（fields tightly packed）
        assert!(m1.data_start_fp > m0.data_start_fp);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn open_single_leaf_tree() {
        let points: Vec<(i64, u32)> = (0..100u32).map(|i| (i as i64, i)).collect();
        let (root, fis) = write_segment("open-single", &[(0, "ts", points)], &[]);
        let reader = open(&root, &fis);
        let (_, m) = &reader.fields[0];
        assert_eq!(m.num_leaves, 1);
        assert!(m.splits.is_empty());
        assert_eq!(m.leaf_fps.len(), 1);
        assert_eq!(m.leaf_fps[0], m.data_start_fp);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn open_without_points_returns_none() {
        // 无 .kdm 的段目录 → Ok(None)（写侧只在有 point 数据时创建三文件，
        // segment_builder.rs:240-245）
        let root = temp_dir("open-none");
        let dir = FSDirectory::open(&root).unwrap();
        let fis = FieldInfos::new(vec![FieldInfo::stored("body", 0)]);
        assert!(
            PointsReader::open(&dir, "_0", &[7u8; 16], &fis)
                .unwrap()
                .is_none()
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
