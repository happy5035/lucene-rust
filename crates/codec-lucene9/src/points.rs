//! Lucene 9.12.3-compatible 1D Points (BKD tree) writer
//! (`codecs/lucene90/Lucene90PointsFormat.java`): `.kdd` leaf blocks,
//! `.kdi` packed index, `.kdm` per-field metadata.
//!
//! Scope: `numDims == numIndexDims == 1`, long (`bytesPerDim = 8`) and int
//! (`bytesPerDim = 4`) fields. All format details follow the 9.12.3 sources,
//! cited per item (`util/bkd/BKDWriter.java`, `util/bkd/DocIdsWriter.java`,
//! `util/bkd/BKDReader.java`, `codecs/lucene90/Lucene90PointsWriter.java`);
//! see `docs/format-notes-points.md` for the full report.
//!
//! Simplifications vs. Lucene (all legal: the read side accepts them):
//! - always fully in-memory sort, equivalent to the 1D flush path
//!   (`BKDWriter.writeField1Dim`, :565-597) — the Heap/Offline point writers
//!   have zero influence on 1D on-disk bytes;
//! - values blocks use only the all-equal (`-1`) and high-cardinality (`0`)
//!   branches; the low-cardinality branch (`-2`) is a writer-side choice
//!   (`writeLeafBlockPackedValues`, :1287-1318) and is never emitted.

use std::collections::BTreeSet;
use std::io;

use crate::codec_util::{CODEC_MAGIC, write_be_int, write_footer, write_index_header};
use crate::directory::FSDirectory;
use crate::io::ChecksumIndexOutput;

/// Lucene90PointsFormat extensions and codec names (:48-59).
pub const DATA_EXTENSION: &str = "kdd";
pub const INDEX_EXTENSION: &str = "kdi";
pub const META_EXTENSION: &str = "kdm";
pub(crate) const DATA_CODEC_NAME: &str = "Lucene90PointsFormatData";
pub(crate) const INDEX_CODEC_NAME: &str = "Lucene90PointsFormatIndex";
pub(crate) const META_CODEC_NAME: &str = "Lucene90PointsFormatMeta";
/// VERSION_START == VERSION_CURRENT (:61-62).
pub(crate) const FORMAT_VERSION: u32 = 0;

/// BKDConfig.DEFAULT_MAX_POINTS_IN_LEAF_NODE (BKDConfig.java:26). The reader
/// derives per-leaf sizes from this value (BKDReader.size, :519-521), so it
/// is a hard contract, not a tuning knob.
pub const MAX_POINTS_IN_LEAF_NODE: usize = 512;

/// BKDWriter.CODEC_NAME / VERSION_CURRENT == VERSION_META_FILE (:82-89),
/// written as a `CodecUtil.writeHeader` inside each `.kdm` field entry (:1244).
pub(crate) const BKD_CODEC_NAME: &str = "BKD";
pub(crate) const BKD_VERSION: u32 = 9;

// DocIdsWriter branch flags (DocIdsWriter.java:30-34).
const CONTINUOUS_IDS: u8 = (-2i8) as u8;
const BITSET_IDS: u8 = (-1i8) as u8;
const DELTA_BPV_16: u8 = 16;
const BPV_24: u8 = 24;
const BPV_32: u8 = 32;

/// Segment file names for this writer: `{segment}.kdd/.kdi/.kdm`.
/// Points always use an empty segment suffix
/// (IndexFileNames.segmentFileName, IndexFileNames.java:90-106).
pub fn file_names(segment: &str) -> [String; 3] {
    [
        format!("{segment}.{DATA_EXTENSION}"),
        format!("{segment}.{INDEX_EXTENSION}"),
        format!("{segment}.{META_EXTENSION}"),
    ]
}

/// NumericUtils.longToSortableBytes (:210-214): flip the sign bit, then
/// big-endian, so unsigned byte order == signed value order.
fn long_to_sortable_bytes(value: i64) -> [u8; 8] {
    ((value as u64) ^ 0x8000_0000_0000_0000).to_be_bytes()
}

/// NumericUtils.intToSortableBytes (:187-191), zero-padded into a `[u8; 8]`
/// slot; only the first `bytes_per_dim = 4` bytes are ever read or written.
fn int_to_sortable_bytes(value: i32) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&((value as u32) ^ 0x8000_0000).to_be_bytes());
    bytes
}

/// Writer for `_N.kdd` / `_N.kdi` / `_N.kdm` (Lucene90PointsWriter).
pub struct PointsWriter {
    data_out: ChecksumIndexOutput,
    index_out: ChecksumIndexOutput,
    meta_out: ChecksumIndexOutput,
    data_name: String,
    index_name: String,
    meta_name: String,
}

impl PointsWriter {
    /// Lucene90PointsWriter ctor (:59-98): creates the three files and writes
    /// each index header (codec name, version 0, segment id, empty suffix).
    pub fn new(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16]) -> io::Result<Self> {
        let [data_name, index_name, meta_name] = file_names(segment);
        let mut data_out = dir.create_output(&data_name)?;
        write_index_header(
            &mut data_out,
            DATA_CODEC_NAME,
            FORMAT_VERSION,
            segment_id,
            "",
        )?;
        let mut index_out = dir.create_output(&index_name)?;
        write_index_header(
            &mut index_out,
            INDEX_CODEC_NAME,
            FORMAT_VERSION,
            segment_id,
            "",
        )?;
        let mut meta_out = dir.create_output(&meta_name)?;
        write_index_header(
            &mut meta_out,
            META_CODEC_NAME,
            FORMAT_VERSION,
            segment_id,
            "",
        )?;
        Ok(PointsWriter {
            data_out,
            index_out,
            meta_out,
            data_name,
            index_name,
            meta_name,
        })
    }

    /// Writes one 1D long field (`bytesPerDim = 8`). Points may arrive in any
    /// order and may repeat (including several values per doc). An empty
    /// field writes no `.kdm` entry at all
    /// (Lucene90PointsWriter.writeField, :144-148).
    pub fn write_field_long(
        &mut self,
        field_number: i32,
        points: &mut [(i64, u32)],
    ) -> io::Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        let mut packed: Vec<([u8; 8], u32)> = points
            .iter()
            .map(|&(value, doc)| (long_to_sortable_bytes(value), doc))
            .collect();
        self.write_field_1d(field_number, &mut packed, 8)
    }

    /// Same as [`Self::write_field_long`] for an int field (`bytesPerDim = 4`).
    pub fn write_field_int(
        &mut self,
        field_number: i32,
        points: &mut [(i32, u32)],
    ) -> io::Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        let mut packed: Vec<([u8; 8], u32)> = points
            .iter()
            .map(|&(value, doc)| (int_to_sortable_bytes(value), doc))
            .collect();
        self.write_field_1d(field_number, &mut packed, 4)
    }

    /// Shared 1D path: BKDWriter.writeField1Dim (:565-597) +
    /// OneDimensionBKDWriter (:651-829), then the finalizer
    /// (BKDWriter.writeIndex, :1236-1264).
    fn write_field_1d(
        &mut self,
        field_number: i32,
        points: &mut [([u8; 8], u32)],
        bytes_per_dim: usize,
    ) -> io::Result<()> {
        // MutablePointTreeReaderUtils.sort (:40-85): unsigned packedBytes order,
        // docID tie-break; net effect is (value asc, docID asc)
        // (PointValues.IntersectVisitor javadoc, PointValues.java:313-317).
        points.sort_by(|a, b| {
            a.0[..bytes_per_dim]
                .cmp(&b.0[..bytes_per_dim])
                .then_with(|| a.1.cmp(&b.1))
        });

        let data_start_fp = self.data_out.file_pointer(); // :683
        let mut leaf_block_fps: Vec<u64> = Vec::new();
        // First value of every leaf except the first (:790-794); these become
        // the inner-node split values (:1104).
        let mut leaf_block_start_values: Vec<[u8; 8]> = Vec::new();
        let mut min_packed_value = [0u8; 8];
        let mut max_packed_value = [0u8; 8];
        let mut docs_seen: BTreeSet<u32> = BTreeSet::new(); // docsSeen FixedBitSet (:708)

        // Fixed 512-point leaves; only the right-most leaf may be short
        // (:720-737). The reader's size() / estimatePointCount hard-depend on
        // this (BKDReader.java:298-300, :519-521).
        for (leaf_index, leaf) in points.chunks(MAX_POINTS_IN_LEAF_NODE).enumerate() {
            if leaf_index == 0 {
                min_packed_value = leaf[0].0; // global min (:778-780)
            }
            max_packed_value = leaf[leaf.len() - 1].0; // global max (:781-786)
            if leaf_index > 0 {
                leaf_block_start_values.push(leaf[0].0);
            }
            leaf_block_fps.push(self.data_out.file_pointer()); // :795
            write_leaf_block(&mut self.data_out, leaf, bytes_per_dim)?;
            for &(_, doc) in leaf {
                docs_seen.insert(doc);
            }
        }
        let point_count = points.len() as u64;
        let doc_count = docs_seen.len();

        let packed_index = pack_index(&leaf_block_fps, &leaf_block_start_values, bytes_per_dim);

        // Lucene90PointsWriter.writeField: LE field number first (:145), then
        // the finalizer writes the BKD meta entry (:1236-1264).
        self.meta_out.write_int(field_number)?;
        write_header(&mut self.meta_out, BKD_CODEC_NAME, BKD_VERSION)?; // :1244
        self.meta_out.write_vint(1)?; // numDims (:1245)
        self.meta_out.write_vint(1)?; // numIndexDims (:1246)
        self.meta_out.write_vint(MAX_POINTS_IN_LEAF_NODE as i32)?; // (:1247)
        self.meta_out.write_vint(bytes_per_dim as i32)?; // (:1248)
        self.meta_out.write_vint(leaf_block_fps.len() as i32)?; // numLeaves (:1251)
        self.meta_out
            .write_bytes(&min_packed_value[..bytes_per_dim])?; // (:1252)
        self.meta_out
            .write_bytes(&max_packed_value[..bytes_per_dim])?; // (:1253)
        self.meta_out.write_vlong(point_count as i64)?; // (:1255)
        self.meta_out.write_vint(doc_count as i32)?; // docsSeen.cardinality() (:1256)
        self.meta_out.write_vint(packed_index.len() as i32)?; // (:1257)
        self.meta_out.write_long(data_start_fp as i64)?; // LE (:1258)
        let index_start_fp = self.index_out.file_pointer();
        self.meta_out.write_long(index_start_fp as i64)?; // LE (:1261)
        self.index_out.write_bytes(&packed_index)?; // (:1263)
        Ok(())
    }

    /// Lucene90PointsWriter.finish (:281-292): `int(-1)`, footers for
    /// `.kdi`/`.kdd`, their total lengths (footers included), meta footer.
    pub fn finish(mut self) -> io::Result<Vec<String>> {
        self.meta_out.write_int(-1)?;
        write_footer(&mut self.index_out)?;
        write_footer(&mut self.data_out)?;
        // getFilePointer() after writeFooter == full length incl. footer (:289-290)
        let index_length = self.index_out.file_pointer();
        let data_length = self.data_out.file_pointer();
        self.meta_out.write_long(index_length as i64)?;
        self.meta_out.write_long(data_length as i64)?;
        write_footer(&mut self.meta_out)?;

        self.meta_out.flush()?;
        self.index_out.flush()?;
        self.data_out.flush()?;
        Ok(vec![self.data_name, self.index_name, self.meta_name])
    }
}

/// CodecUtil.writeHeader (CodecUtil.java:77-86): BE magic + writeString(codec)
/// + BE version. Unlike `writeIndexHeader` there is no segment id / suffix.
///
/// Used for the per-field `Header("BKD", 9)` entry header inside `.kdm`
/// (BKDWriter.java:1244); `codec_util` only exposes the index variant.
fn write_header(out: &mut ChecksumIndexOutput, codec: &str, version: u32) -> io::Result<()> {
    write_be_int(out, CODEC_MAGIC)?;
    out.write_string(codec)?;
    write_be_int(out, version)
}

/// OneDimensionBKDWriter.writeLeafBlock (:776-828):
/// `VInt count` + docs block + commonPrefix block + values block.
fn write_leaf_block(
    out: &mut ChecksumIndexOutput,
    leaf: &[([u8; 8], u32)],
    bytes_per_dim: usize,
) -> io::Result<()> {
    debug_assert!(!leaf.is_empty());
    // writeLeafBlockDocs (:1266-1271)
    out.write_vint(leaf.len() as i32)?;
    let docs: Vec<u32> = leaf.iter().map(|&(_, doc)| doc).collect();
    write_doc_ids(out, &docs)?;

    // commonPrefixLen: common prefix of the first and last value of the sorted
    // leaf (:799-801); writeCommonPrefixes (:1475-1482), single dim.
    let common_prefix_len = common_prefix(&leaf[0].0, &leaf[leaf.len() - 1].0, bytes_per_dim);
    out.write_vint(common_prefix_len as i32)?;
    out.write_bytes(&leaf[0].0[..common_prefix_len])?;

    // writeLeafBlockPackedValues (:1273-1320). Branch choice is a writer-side
    // freedom (readCompressedDim, BKDReader.java:937-945, accepts all): we
    // emit `-1` for all-equal leaves and the high-cardinality `0` branch
    // otherwise, never the low-cardinality `-2` branch.
    if common_prefix_len == bytes_per_dim {
        // all values in this leaf are equal (:1281-1284)
        out.write_byte((-1i8) as u8)?;
    } else {
        out.write_byte(0)?; // sortedDim (:1315)
        write_high_cardinality_packed_values(out, leaf, bytes_per_dim, common_prefix_len)?;
    }
    Ok(())
}

fn common_prefix(a: &[u8; 8], b: &[u8; 8], max_len: usize) -> usize {
    let mut len = 0;
    while len < max_len && a[len] == b[len] {
        len += 1;
    }
    len
}

/// writeHighCardinalityLeafBlockPackedValues (:1360-1384) for 1D: no actual
/// bounds (`numIndexDims == 1`, :1368-1370); run-length on the byte at
/// `compressedByteOffset == common_prefix_len`, then per-point suffixes.
fn write_high_cardinality_packed_values(
    out: &mut ChecksumIndexOutput,
    leaf: &[([u8; 8], u32)],
    bytes_per_dim: usize,
    common_prefix_len: usize,
) -> io::Result<()> {
    let count = leaf.len();
    let mut i = 0;
    while i < count {
        // runLen (:1460-1473) over [i, min(i + 0xff, count)); runs are ≤ 255.
        let end = (i + 0xff).min(count);
        let run_byte = leaf[i].0[common_prefix_len];
        let mut run_len = 1usize;
        while i + run_len < end && leaf[i + run_len].0[common_prefix_len] == run_byte {
            run_len += 1;
        }
        out.write_byte(run_byte)?;
        out.write_byte(run_len as u8)?;
        // writeLeafBlockPackedValuesRange (:1441-1458) with the dim prefix
        // bumped by one (:1371): suffix = bytes after the run byte.
        for point in &leaf[i..i + run_len] {
            out.write_bytes(&point.0[common_prefix_len + 1..bytes_per_dim])?;
        }
        i += run_len;
    }
    Ok(())
}

/// DocIdsWriter.writeDocIds (:60-146): all five write-side branches, in the
/// same short-circuit order.
fn write_doc_ids(out: &mut ChecksumIndexOutput, doc_ids: &[u32]) -> io::Result<()> {
    let count = doc_ids.len();
    debug_assert!(count > 0);
    let mut strictly_sorted = true;
    let mut min = doc_ids[0];
    let mut max = doc_ids[0];
    for pair in doc_ids.windows(2) {
        if pair[0] >= pair[1] {
            strictly_sorted = false;
        }
        min = min.min(pair[1]);
        max = max.max(pair[1]);
    }
    // :76, widened to u64 so the full u32 doc range cannot overflow.
    let min2max = u64::from(max) - u64::from(min) + 1;

    if strictly_sorted {
        if min2max == count as u64 {
            // CONTINUOUS_IDS (:77-82)
            out.write_byte(CONTINUOUS_IDS)?;
            return out.write_vint(doc_ids[0] as i32);
        }
        if min2max <= (count as u64) << 4 {
            // BITSET_IDS (:83-91)
            out.write_byte(BITSET_IDS)?;
            return write_ids_as_bitset(out, doc_ids);
        }
    }
    if min2max <= 0xFFFF {
        // DELTA_BPV_16 (:94-109): VInt min; then count/2 LE ints packing
        // delta[i] in the high 16 bits and delta[halfLen + i] in the low 16
        // (:102); odd tail as one LE short (:107-109).
        out.write_byte(DELTA_BPV_16)?;
        out.write_vint(min as i32)?;
        let deltas: Vec<u32> = doc_ids.iter().map(|&doc| doc - min).collect();
        let half_len = count / 2;
        for i in 0..half_len {
            let packed = (deltas[i] << 16) | deltas[half_len + i];
            out.write_int(packed as i32)?;
        }
        if count & 1 == 1 {
            out.write_short(deltas[count - 1] as i16)?;
        }
        return Ok(());
    }
    if max <= 0xFF_FFFF {
        // BPV_24 (:111-138): 8 docs per 3 LE longs, MSB-first 24-bit lanes;
        // tail docs as LE short(doc >>> 8) + byte(doc).
        out.write_byte(BPV_24)?;
        let mut groups = doc_ids.chunks_exact(8);
        for g in &mut groups {
            let d = |i: usize| u64::from(g[i]);
            let l1 = (d(0) << 40) | (d(1) << 16) | (d(2) >> 8);
            let l2 = ((d(2) & 0xff) << 56) | (d(3) << 32) | (d(4) << 8) | (d(5) >> 16);
            let l3 = ((d(5) & 0xffff) << 48) | (d(6) << 24) | d(7);
            out.write_long(l1 as i64)?;
            out.write_long(l2 as i64)?;
            out.write_long(l3 as i64)?;
        }
        for &doc in groups.remainder() {
            out.write_short((doc >> 8) as i16)?;
            out.write_byte(doc as u8)?;
        }
        return Ok(());
    }
    // BPV_32 (:139-144)
    out.write_byte(BPV_32)?;
    for &doc in doc_ids {
        out.write_int(doc as i32)?;
    }
    Ok(())
}

/// DocIdsWriter.writeIdsAsBitSet (:148-179). Only called for strictly sorted
/// ids, so min == first and max == last (:150-151).
fn write_ids_as_bitset(out: &mut ChecksumIndexOutput, doc_ids: &[u32]) -> io::Result<()> {
    let min = doc_ids[0];
    let max = doc_ids[doc_ids.len() - 1];
    let offset_words = min >> 6;
    let offset_bits = offset_words << 6;
    // FixedBitSet.bits2words(max - offsetBits + 1) (:155)
    let total_word_count = (max - offset_bits + 1).div_ceil(64);
    out.write_vint(offset_words as i32)?;
    out.write_vint(total_word_count as i32)?;

    let mut current_word: u64 = 0;
    let mut current_word_index = 0;
    for &doc in doc_ids {
        let index = doc - offset_bits;
        let next_word_index = index >> 6;
        if current_word_index < next_word_index {
            out.write_long(current_word as i64)?;
            current_word = 0;
            current_word_index += 1;
            while current_word_index < next_word_index {
                current_word_index += 1;
                out.write_long(0)?;
            }
        }
        current_word |= 1u64 << (index & 63);
    }
    out.write_long(current_word as i64)?;
    debug_assert_eq!(current_word_index + 1, total_word_count);
    Ok(())
}

/// BKDWriter.getNumLeftLeafNodes (:831-847): fixed tree-shape contract shared
/// with the reader (BKDReader.size, :519-521).
pub(crate) fn get_num_left_leaf_nodes(num_leaves: usize) -> usize {
    debug_assert!(num_leaves > 1);
    let last_full_level = usize::BITS - 1 - num_leaves.leading_zeros();
    let leaves_full_level = 1usize << last_full_level;
    let num_left = leaves_full_level / 2;
    let unbalanced = num_leaves - leaves_full_level;
    num_left + unbalanced.min(num_left)
}

/// DataOutput.writeVInt over a byte buffer (non-negative values).
fn write_vint_raw(out: &mut Vec<u8>, mut v: u32) {
    while v & !0x7f != 0 {
        out.push(((v & 0x7f) as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// DataOutput.writeVLong over a byte buffer (fp deltas are never negative).
fn write_vlong_raw(out: &mut Vec<u8>, mut v: u64) {
    while v & !0x7f != 0 {
        out.push(((v & 0x7f) as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// BKDWriter.packIndex (:1019-1049): pre-order recursion encoded into blocks
/// (the `leftNumBytes` placeholders included), compacted into one byte[].
fn pack_index(
    leaf_block_fps: &[u64],
    leaf_block_start_values: &[[u8; 8]],
    bytes_per_dim: usize,
) -> Vec<u8> {
    let mut packer = IndexPacker {
        leaf_block_fps,
        leaf_block_start_values,
        bytes_per_dim,
        blocks: Vec::new(),
    };
    // root: isLeft=false, minBlockFP=0, lastSplitValues all-zero,
    // negativeDeltas all-false (:1025-1037).
    let total_size = packer.recurse(0, [0u8; 8], false, false, 0, leaf_block_fps.len());
    let mut index = Vec::with_capacity(total_size);
    for block in &packer.blocks {
        index.extend_from_slice(block);
    }
    debug_assert_eq!(index.len(), total_size);
    index
}

struct IndexPacker<'a> {
    leaf_block_fps: &'a [u64],
    leaf_block_start_values: &'a [[u8; 8]],
    bytes_per_dim: usize,
    blocks: Vec<Vec<u8>>,
}

impl IndexPacker<'_> {
    /// BKDWriter.recursePackIndex (:1063-1223), specialized to
    /// `numIndexDims == 1` (`splitDim == 0`, :762-764). `last_split_value` and
    /// `negative_delta` are the dim-0 slots of `lastSplitValues` /
    /// `negativeDeltas`, passed by value: a node sees exactly what its parent
    /// set before recursing into it (:1174-1175, :1200), and the writer
    /// restores both after the recursion (:1213-1217). Returns the subtree's
    /// total byte length.
    fn recurse(
        &mut self,
        min_block_fp: u64,
        last_split_value: [u8; 8],
        negative_delta: bool,
        is_left: bool,
        leaves_offset: usize,
        num_leaves: usize,
    ) -> usize {
        if num_leaves == 1 {
            if is_left {
                // left children inherit the fp; nothing is written (:1075-1077)
                debug_assert_eq!(self.leaf_block_fps[leaves_offset], min_block_fp);
                return 0;
            }
            // right child (or root): VLong fp delta (:1078-1084); for the root
            // this is the absolute fp of the first leaf block.
            let delta = self.leaf_block_fps[leaves_offset] - min_block_fp;
            let mut block = Vec::new();
            write_vlong_raw(&mut block, delta);
            let len = block.len();
            self.blocks.push(block);
            return len;
        }

        let mut buf: Vec<u8> = Vec::new();
        let left_block_fp;
        if is_left {
            // the left tree's left-most leaf block FP is always the minimal FP (:1087-1090)
            debug_assert_eq!(self.leaf_block_fps[leaves_offset], min_block_fp);
            left_block_fp = min_block_fp;
        } else {
            left_block_fp = self.leaf_block_fps[leaves_offset];
            write_vlong_raw(&mut buf, left_block_fp - min_block_fp); // :1091-1097
        }

        let num_left = get_num_left_leaf_nodes(num_leaves); // :1099
        let right_offset = leaves_offset + num_left;
        let split_offset = right_offset - 1;
        // splitDim == 0; split value = first value of the right subtree's
        // left-most leaf (:1103-1105).
        let split_value = self.leaf_block_start_values[split_offset];

        // common prefix with the last split value on this dim (:1111-1113)
        let mut prefix = 0;
        while prefix < self.bytes_per_dim && split_value[prefix] == last_split_value[prefix] {
            prefix += 1;
        }

        let first_diff_byte_delta: i32;
        if prefix < self.bytes_per_dim {
            // unsigned byte delta at the first differing byte (:1124-1126)
            let mut delta = i32::from(split_value[prefix]) - i32::from(last_split_value[prefix]);
            if negative_delta {
                delta = -delta; // :1127-1129
            }
            debug_assert!(delta > 0); // :1131
            first_diff_byte_delta = delta;
        } else {
            // split == last split (many duplicate values) (:1132-1134)
            first_diff_byte_delta = 0;
        }

        // code = (delta * (1 + bytesPerDim) + prefix) * numIndexDims + splitDim (:1137-1138)
        let code = first_diff_byte_delta * (1 + self.bytes_per_dim as i32) + prefix as i32;
        write_vint_raw(&mut buf, code as u32); // :1144

        // split value suffix after the first diff byte (:1147-1151)
        if self.bytes_per_dim - prefix > 1 {
            buf.extend_from_slice(&split_value[prefix + 1..self.bytes_per_dim]);
        }

        let num_bytes = buf.len();
        self.blocks.push(std::mem::take(&mut buf)); // appendBlock (:1166)
        let idx_sav = self.blocks.len();
        self.blocks.push(Vec::new()); // leftNumBytes placeholder (:1171-1172)

        let left_num_bytes = self.recurse(
            left_block_fp,
            split_value,
            true, // negativeDeltas[dim] = true while recursing left (:1174-1175)
            true,
            leaves_offset,
            num_left,
        );

        let mut bytes2: Vec<u8> = Vec::new();
        if num_left != 1 {
            write_vint_raw(&mut bytes2, left_num_bytes as u32); // :1189-1190
        } else {
            debug_assert_eq!(left_num_bytes, 0); // :1192
        }
        let bytes2_len = bytes2.len();
        self.blocks[idx_sav] = bytes2; // :1195-1198

        let right_num_bytes = self.recurse(
            left_block_fp,
            split_value,
            false, // negativeDeltas[dim] = false while recursing right (:1200)
            false,
            right_offset,
            num_leaves - num_left,
        );

        num_bytes + bytes2_len + left_num_bytes + right_num_bytes // :1221
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec_util::{FOOTER_LENGTH, FOOTER_MAGIC, crc32, index_header_length};
    use crate::io::IndexOutput;
    use std::fs;
    use std::path::PathBuf;

    // ---------- deterministic RNG (SplitMix64) ----------

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

    // ---------- byte cursor mirroring DataInput (LE primitives, VInt/VLong) ----------

    struct Cursor<'a> {
        buf: &'a [u8],
        pos: usize,
    }

    impl<'a> Cursor<'a> {
        fn at(buf: &'a [u8], pos: usize) -> Self {
            Cursor { buf, pos }
        }
        fn byte(&mut self) -> u8 {
            let b = self.buf[self.pos];
            self.pos += 1;
            b
        }
        fn i8(&mut self) -> i8 {
            self.byte() as i8
        }
        fn bytes(&mut self, n: usize) -> &'a [u8] {
            let s = &self.buf[self.pos..self.pos + n];
            self.pos += n;
            s
        }
        // DataInput.readVInt / readVLong
        fn vint(&mut self) -> u32 {
            let mut v = 0u32;
            let mut shift = 0;
            loop {
                let b = self.byte();
                v |= u32::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    return v;
                }
                shift += 7;
            }
        }
        fn vlong(&mut self) -> u64 {
            let mut v = 0u64;
            let mut shift = 0;
            loop {
                let b = self.byte();
                v |= u64::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    return v;
                }
                shift += 7;
            }
        }
        fn short_le(&mut self) -> u16 {
            u16::from_le_bytes(self.bytes(2).try_into().unwrap())
        }
        fn int_le(&mut self) -> u32 {
            u32::from_le_bytes(self.bytes(4).try_into().unwrap())
        }
        fn long_le(&mut self) -> u64 {
            u64::from_le_bytes(self.bytes(8).try_into().unwrap())
        }
        fn be_int(&mut self) -> u32 {
            u32::from_be_bytes(self.bytes(4).try_into().unwrap())
        }
        fn be_long(&mut self) -> u64 {
            u64::from_be_bytes(self.bytes(8).try_into().unwrap())
        }
    }

    // ---------- file envelope (index header + footer CRC) ----------

    fn check_file_envelope(bytes: &[u8], codec: &str, segment_id: &[u8; 16]) {
        // CodecUtil.writeIndexHeader (:121-135)
        let mut cur = Cursor::at(bytes, 0);
        assert_eq!(cur.be_int(), CODEC_MAGIC);
        let name_len = cur.vint() as usize;
        assert_eq!(cur.bytes(name_len), codec.as_bytes());
        assert_eq!(cur.be_int(), FORMAT_VERSION);
        assert_eq!(cur.bytes(16), segment_id);
        assert_eq!(cur.byte(), 0, "empty suffix");
        assert_eq!(cur.pos, index_header_length(codec, ""));
        // CodecUtil.writeFooter (:409-413)
        let n = bytes.len();
        let mut foot = Cursor::at(bytes, n - FOOTER_LENGTH);
        assert_eq!(foot.be_int(), FOOTER_MAGIC);
        assert_eq!(foot.be_int(), 0, "FOOTER_ALGORITHM_ID");
        assert_eq!(foot.be_long(), crc32(&bytes[..n - 8]));
    }

    // ---------- .kdm parsing (Lucene90PointsReader + BKDReader ctor, :56-113) ----------

    struct FieldMeta {
        field_number: i32,
        bytes_per_dim: usize,
        num_leaves: usize,
        min_value: [u8; 8],
        max_value: [u8; 8],
        point_count: u64,
        doc_count: u32,
        packed_index_byte_length: usize,
        data_start_fp: u64,
        index_start_fp: u64,
    }

    fn parse_meta(bytes: &[u8]) -> (Vec<FieldMeta>, u64, u64) {
        let mut cur = Cursor::at(bytes, index_header_length(META_CODEC_NAME, ""));
        let mut fields = Vec::new();
        loop {
            let field_number = cur.int_le() as i32;
            if field_number == -1 {
                break;
            }
            // CodecUtil.writeHeader("BKD", 9) (BKDWriter.java:1244)
            assert_eq!(cur.be_int(), CODEC_MAGIC);
            assert_eq!(cur.vint(), 3);
            assert_eq!(cur.bytes(3), b"BKD");
            assert_eq!(cur.be_int(), BKD_VERSION);
            assert_eq!(cur.vint(), 1, "numDims");
            assert_eq!(cur.vint(), 1, "numIndexDims");
            assert_eq!(cur.vint(), MAX_POINTS_IN_LEAF_NODE as u32);
            let bytes_per_dim = cur.vint() as usize;
            let num_leaves = cur.vint() as usize;
            assert!(num_leaves > 0);
            let mut min_value = [0u8; 8];
            min_value[..bytes_per_dim].copy_from_slice(cur.bytes(bytes_per_dim));
            let mut max_value = [0u8; 8];
            max_value[..bytes_per_dim].copy_from_slice(cur.bytes(bytes_per_dim));
            assert!(
                min_value[..bytes_per_dim] <= max_value[..bytes_per_dim],
                "meta min <= max (BKDReader.java:82-95)"
            );
            let point_count = cur.vlong();
            let doc_count = cur.vint();
            let packed_index_byte_length = cur.vint() as usize;
            let data_start_fp = cur.long_le();
            let index_start_fp = cur.long_le();
            fields.push(FieldMeta {
                field_number,
                bytes_per_dim,
                num_leaves,
                min_value,
                max_value,
                point_count,
                doc_count,
                packed_index_byte_length,
                data_start_fp,
                index_start_fp,
            });
        }
        let index_len = cur.long_le();
        let data_len = cur.long_le();
        (fields, index_len, data_len)
    }

    // ---------- docs block decoder (DocIdsWriter.readInts, :182-206) ----------

    fn decode_doc_ids(cur: &mut Cursor, count: usize) -> Vec<u32> {
        let mut docs = vec![0u32; count];
        match cur.i8() {
            -2 => {
                // readContinuousIds (:217-222)
                let start = cur.vint();
                for (i, d) in docs.iter_mut().enumerate() {
                    *d = start + i as u32;
                }
            }
            -1 => {
                // readBitSet (:233-240) via readBitSetIterator (:208-215)
                let offset_words = cur.vint();
                let word_count = cur.vint() as usize;
                let mut pos = 0;
                for w in 0..word_count {
                    let mut word = cur.long_le();
                    while word != 0 {
                        let bit = word.trailing_zeros();
                        docs[pos] = (offset_words << 6) + 64 * w as u32 + bit;
                        pos += 1;
                        word &= word - 1;
                    }
                }
                assert_eq!(pos, count, "bitset cardinality == count (:239)");
            }
            16 => {
                // readDelta16 (:242-254)
                let min = cur.vint();
                let half_len = count / 2;
                let mut packed = Vec::with_capacity(half_len);
                for _ in 0..half_len {
                    packed.push(cur.int_le());
                }
                for i in 0..half_len {
                    docs[i] = (packed[i] >> 16) + min;
                    docs[half_len + i] = (packed[i] & 0xFFFF) + min;
                }
                if count & 1 == 1 {
                    docs[count - 1] = u32::from(cur.short_le()) + min;
                }
            }
            24 => {
                // readInts24 (:256-274)
                let mut i = 0;
                while i + 8 <= count {
                    let l1 = cur.long_le();
                    let l2 = cur.long_le();
                    let l3 = cur.long_le();
                    docs[i] = (l1 >> 40) as u32;
                    docs[i + 1] = ((l1 >> 16) & 0xffffff) as u32;
                    docs[i + 2] = (((l1 & 0xffff) << 8) | (l2 >> 56)) as u32;
                    docs[i + 3] = ((l2 >> 32) & 0xffffff) as u32;
                    docs[i + 4] = ((l2 >> 8) & 0xffffff) as u32;
                    docs[i + 5] = (((l2 & 0xff) << 16) | (l3 >> 48)) as u32;
                    docs[i + 6] = ((l3 >> 24) & 0xffffff) as u32;
                    docs[i + 7] = (l3 & 0xffffff) as u32;
                    i += 8;
                }
                while i < count {
                    docs[i] = (u32::from(cur.short_le()) << 8) | u32::from(cur.byte());
                    i += 1;
                }
            }
            32 => {
                // readInts32 (:276-278)
                for d in docs.iter_mut() {
                    *d = cur.int_le();
                }
            }
            other => panic!("unknown doc ids flag {other} (:203-205)"),
        }
        docs
    }

    // ---------- leaf block decoder (BKDReader readDocIDs + visitDocValues*) ----------

    /// Decodes one leaf block at `fp`; returns the (value, doc) sequence in
    /// stored order and the end position (== next leaf's fp).
    fn decode_leaf(data: &[u8], fp: u64, bytes_per_dim: usize) -> (Vec<([u8; 8], u32)>, u64) {
        let mut cur = Cursor::at(data, fp as usize);
        let count = cur.vint() as usize; // :637
        let docs = decode_doc_ids(&mut cur, count); // :639
        // readCommonPrefixes (:947-957), single dim
        let common_prefix_len = cur.vint() as usize;
        assert!(common_prefix_len <= bytes_per_dim);
        let mut value_base = [0u8; 8];
        value_base[..common_prefix_len].copy_from_slice(cur.bytes(common_prefix_len));

        let compressed_dim = cur.i8(); // readCompressedDim (:937-945)
        let mut points: Vec<([u8; 8], u32)> = Vec::with_capacity(count);
        if compressed_dim == -1 {
            // visitUniqueRawDocValues (:893-901): the common prefix IS the value
            for &doc in &docs {
                points.push((value_base, doc));
            }
        } else if compressed_dim == 0 {
            // visitCompressedDocValues (:903-935)
            let suffix_len = bytes_per_dim - common_prefix_len - 1;
            let mut i = 0;
            while i < count {
                let run_byte = cur.byte();
                let run_len = cur.byte() as usize;
                for j in 0..run_len {
                    let mut v = value_base;
                    v[common_prefix_len] = run_byte;
                    v[common_prefix_len + 1..common_prefix_len + 1 + suffix_len]
                        .copy_from_slice(cur.bytes(suffix_len));
                    points.push((v, docs[i + j]));
                }
                i += run_len;
            }
            assert_eq!(i, count, "runs add up to count (:931-934)");
        } else {
            panic!("unexpected compressedDim {compressed_dim}; writer never emits -2");
        }
        (points, cur.pos as u64)
    }

    // ---------- packed index decoder (BKDPointTree.readNodeData, :657-717) ----------

    struct DecodedIndex {
        /// (nodeID, leavesOffset, leafBlockFP) per leaf, in decode (pre-order) order
        leaves: Vec<(u32, usize, u64)>,
        /// (nodeID, splitValue, splitOffset) per inner node
        inners: Vec<(u32, [u8; 8], usize)>,
    }

    fn decode_packed_index(index: &[u8], num_leaves: usize, bytes_per_dim: usize) -> DecodedIndex {
        let mut decoded = DecodedIndex {
            leaves: Vec::new(),
            inners: Vec::new(),
        };
        let mut cur = Cursor::at(index, 0);
        // BKDPointTree ctor: nodeID=1, level=1, readNodeData(false) (:253-254)
        decode_node(
            &mut cur,
            &mut decoded,
            1,
            num_leaves as u32,
            0,
            [0u8; 8],
            false,
            false,
            0,
            num_leaves,
            bytes_per_dim,
        );
        assert_eq!(cur.pos, index.len(), "packed index fully consumed");
        decoded
    }

    /// Mirrors readNodeData (:657-717) plus the pre-order child recursion.
    /// `min_block_fp` / `last_split_value` / `negative_delta` are the reader's
    /// leafBlockFPStack[level-1] / splitValuesStack[level-1] / negativeDeltas
    /// for this node; `total_num_leaves` is the reader's leafNodeOffset.
    #[allow(clippy::too_many_arguments)]
    fn decode_node(
        cur: &mut Cursor,
        decoded: &mut DecodedIndex,
        node_id: u32,
        total_num_leaves: u32,
        min_block_fp: u64,
        last_split_value: [u8; 8],
        negative_delta: bool,
        is_left: bool,
        leaves_offset: usize,
        num_leaves: usize,
        bytes_per_dim: usize,
    ) {
        // leafBlockFPStack[level] = stack[level-1] (+ VLong if right child) (:658-662)
        let mut fp = min_block_fp;
        if !is_left {
            fp += cur.vlong();
        }
        if num_leaves == 1 {
            // isLeafNode(): nodeID >= leafNodeOffset (:481-483)
            assert!(node_id >= total_num_leaves);
            decoded.leaves.push((node_id, leaves_offset, fp));
            return;
        }
        assert!(node_id < total_num_leaves, "inner node ids are < numLeaves");

        let code = cur.vint() as i32; // :687
        // numIndexDims == 1 ⇒ splitDim == code % 1 == 0 and code / 1 == code (:688-690)
        let prefix = (code % (1 + bytes_per_dim as i32)) as usize; // :691
        let suffix = bytes_per_dim - prefix;
        // splitValuesStack[level] starts as a copy of the parent's (:675-684)
        let mut split_value = last_split_value;
        if suffix > 0 {
            let mut first_diff_byte_delta = code / (1 + bytes_per_dim as i32); // :695
            if negative_delta {
                first_diff_byte_delta = -first_diff_byte_delta; // :696-698
            }
            let old_byte = i32::from(split_value[prefix]);
            split_value[prefix] = (old_byte + first_diff_byte_delta) as u8; // :700-701
            split_value[prefix + 1..prefix + 1 + (suffix - 1)]
                .copy_from_slice(cur.bytes(suffix - 1)); // :702
        }
        // else: split == last split on this dim (:704-706)

        let num_left = get_num_left_leaf_nodes(num_leaves);
        let right_offset = leaves_offset + num_left;
        decoded
            .inners
            .push((node_id, split_value, right_offset - 1));

        // leftNumBytes is present iff the left child is an inner node (:708-713)
        let left_num_bytes = if node_id * 2 < total_num_leaves {
            assert!(num_left > 1);
            cur.vlong() as usize // VInt == VLong encoding for non-negative ints
        } else {
            assert_eq!(num_left, 1);
            0
        };
        // rightNodePositions[level] (:714): right subtree follows the left one
        let right_node_position = cur.pos + left_num_bytes;
        decode_node(
            cur,
            decoded,
            node_id * 2,
            total_num_leaves,
            fp,
            split_value,
            true, // negativeDeltas[parentSplitDim] = isLeft (:671-673)
            true,
            leaves_offset,
            num_left,
            bytes_per_dim,
        );
        assert_eq!(
            cur.pos, right_node_position,
            "leftNumBytes measures the left subtree byte length"
        );
        decode_node(
            cur,
            decoded,
            node_id * 2 + 1,
            total_num_leaves,
            fp,
            split_value,
            false,
            false,
            right_offset,
            num_leaves - num_left,
            bytes_per_dim,
        );
    }

    // ---------- segment-level round-trip driver ----------

    enum FieldSpec {
        Long(i32, Vec<(i64, u32)>),
        Int(i32, Vec<(i32, u32)>),
    }

    impl FieldSpec {
        fn field_number(&self) -> i32 {
            match self {
                FieldSpec::Long(n, _) | FieldSpec::Int(n, _) => *n,
            }
        }
        fn point_count(&self) -> usize {
            match self {
                FieldSpec::Long(_, p) => p.len(),
                FieldSpec::Int(_, p) => p.len(),
            }
        }
        fn packed_points(&self) -> Vec<([u8; 8], u32)> {
            match self {
                FieldSpec::Long(_, pts) => pts
                    .iter()
                    .map(|&(v, d)| (long_to_sortable_bytes(v), d))
                    .collect(),
                FieldSpec::Int(_, pts) => pts
                    .iter()
                    .map(|&(v, d)| (int_to_sortable_bytes(v), d))
                    .collect(),
            }
        }
    }

    struct WrittenSegment {
        dir_path: PathBuf,
        kdd: Vec<u8>,
        kdi: Vec<u8>,
        kdm: Vec<u8>,
        segment_id: [u8; 16],
    }

    impl Drop for WrittenSegment {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir_path);
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codec-lucene9-points-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn write_segment(tag: &str, fields: &[FieldSpec]) -> WrittenSegment {
        let dir_path = temp_dir(tag);
        let segment_id = [0xA5u8; 16];
        let (kdd, kdi, kdm);
        {
            let dir = FSDirectory::open(&dir_path).unwrap();
            let mut writer = PointsWriter::new(&dir, "_0", &segment_id).unwrap();
            for field in fields {
                match field {
                    FieldSpec::Long(n, pts) => {
                        writer.write_field_long(*n, &mut pts.clone()).unwrap()
                    }
                    FieldSpec::Int(n, pts) => writer.write_field_int(*n, &mut pts.clone()).unwrap(),
                }
            }
            let names = writer.finish().unwrap();
            assert_eq!(
                names,
                vec![
                    "_0.kdd".to_string(),
                    "_0.kdi".to_string(),
                    "_0.kdm".to_string()
                ]
            );
            kdd = fs::read(dir_path.join("_0.kdd")).unwrap();
            kdi = fs::read(dir_path.join("_0.kdi")).unwrap();
            kdm = fs::read(dir_path.join("_0.kdm")).unwrap();
        }
        WrittenSegment {
            dir_path,
            kdd,
            kdi,
            kdm,
            segment_id,
        }
    }

    fn check_segment(seg: &WrittenSegment, fields: &[FieldSpec]) {
        check_file_envelope(&seg.kdd, DATA_CODEC_NAME, &seg.segment_id);
        check_file_envelope(&seg.kdi, INDEX_CODEC_NAME, &seg.segment_id);
        check_file_envelope(&seg.kdm, META_CODEC_NAME, &seg.segment_id);

        let (metas, index_len, data_len) = parse_meta(&seg.kdm);
        // Lucene90PointsReader footer reconciliation (:95-116)
        assert_eq!(index_len, seg.kdi.len() as u64);
        assert_eq!(data_len, seg.kdd.len() as u64);

        let expected: Vec<&FieldSpec> = fields.iter().filter(|f| f.point_count() > 0).collect();
        assert_eq!(metas.len(), expected.len(), "0-point fields write no entry");

        let mut prev_data_end = None;
        let mut prev_index_end = None;
        for (meta, spec) in metas.iter().zip(expected.iter()) {
            let (data_end, index_end) = check_field(seg, meta, spec);
            if let Some(prev) = prev_data_end {
                assert_eq!(
                    meta.data_start_fp, prev,
                    "fields are tightly packed in .kdd"
                );
            }
            if let Some(prev) = prev_index_end {
                assert_eq!(
                    meta.index_start_fp, prev,
                    "fields are tightly packed in .kdi"
                );
            }
            prev_data_end = Some(data_end);
            prev_index_end = Some(index_end);
        }
        if let Some(end) = prev_data_end {
            assert_eq!(end, seg.kdd.len() as u64 - FOOTER_LENGTH as u64);
        }
        if let Some(end) = prev_index_end {
            assert_eq!(end, seg.kdi.len() as u64 - FOOTER_LENGTH as u64);
        }
    }

    /// Verifies one field end to end: meta values, packed index (reader
    /// semantics), leaf walk (sequence equality), and the split-chain bounds.
    /// Returns (.kdd end fp of this field, .kdi end fp).
    fn check_field(seg: &WrittenSegment, meta: &FieldMeta, spec: &FieldSpec) -> (u64, u64) {
        let bpd = meta.bytes_per_dim;
        assert_eq!(meta.field_number, spec.field_number());
        match spec {
            FieldSpec::Long(..) => assert_eq!(bpd, 8),
            FieldSpec::Int(..) => assert_eq!(bpd, 4),
        }

        let mut expected = spec.packed_points();
        expected.sort_by(|a, b| a.0[..bpd].cmp(&b.0[..bpd]).then_with(|| a.1.cmp(&b.1)));

        assert_eq!(meta.point_count, expected.len() as u64);
        assert_eq!(
            meta.doc_count as usize,
            expected.iter().map(|p| p.1).collect::<BTreeSet<_>>().len(),
            "docCount counts distinct docs (:1256)"
        );
        assert_eq!(&meta.min_value[..bpd], &expected.first().unwrap().0[..bpd]);
        assert_eq!(&meta.max_value[..bpd], &expected.last().unwrap().0[..bpd]);

        // ---- packed index (test 1) ----
        let index_start = meta.index_start_fp as usize;
        let index = &seg.kdi[index_start..index_start + meta.packed_index_byte_length];
        let decoded = decode_packed_index(index, meta.num_leaves, bpd);

        // BFS order == ascending nodeID: leaves cover [numLeaves, 2*numLeaves),
        // inner nodes [1, numLeaves) (isLeafNode, BKDReader.java:481-483).
        let mut bfs: Vec<(u32, usize, u64)> = decoded.leaves.clone();
        bfs.sort_by_key(|&(node, _, _)| node);
        for (i, &(node, _, _)) in bfs.iter().enumerate() {
            assert_eq!(node as usize, meta.num_leaves + i);
        }
        let mut inner_ids: Vec<u32> = decoded.inners.iter().map(|&(node, _, _)| node).collect();
        inner_ids.sort_unstable();
        for (i, &node) in inner_ids.iter().enumerate() {
            assert_eq!(node as usize, 1 + i);
        }

        // ---- leaf walk over .kdd (test 2) ----
        let mut walked_fps = Vec::with_capacity(meta.num_leaves);
        let mut actual: Vec<([u8; 8], u32)> = Vec::new();
        let mut fp = meta.data_start_fp;
        for leaf_index in 0..meta.num_leaves {
            walked_fps.push(fp);
            let (points, end) = decode_leaf(&seg.kdd, fp, bpd);
            // only the right-most leaf may hold fewer than 512 points
            // (BKDReader.size, :519-521)
            let want_count = if leaf_index + 1 == meta.num_leaves {
                expected.len() - (meta.num_leaves - 1) * MAX_POINTS_IN_LEAF_NODE
            } else {
                MAX_POINTS_IN_LEAF_NODE
            };
            assert_eq!(points.len(), want_count);
            actual.extend(points);
            fp = end;
        }
        let data_end = fp;

        // decoded leaf fps == write-time leafBlockFPs order
        let mut by_offset: Vec<(usize, u64)> = decoded
            .leaves
            .iter()
            .map(|&(_, offset, fp)| (offset, fp))
            .collect();
        by_offset.sort_unstable();
        assert_eq!(
            by_offset.iter().map(|&(_, fp)| fp).collect::<Vec<_>>(),
            walked_fps
        );

        // the whole decoded sequence == sorted input
        assert_eq!(&actual[..], &expected[..]);

        // ---- split chain consistency (test 3) ----
        check_split_chain(seg, meta, &decoded, &walked_fps);

        (
            data_end,
            meta.index_start_fp + meta.packed_index_byte_length as u64,
        )
    }

    /// Walks the decoded tree carrying cell bounds: root is [meta.min, meta.max],
    /// left child [min, split], right child [split, max]
    /// (BKDReader.pushBoundsLeft/Right, :375-426). Every leaf value must fall
    /// inside its cell (CheckIndex VerifyPointsVisitor, :2986-3025), and each
    /// decoded split must equal the first value of the right subtree's
    /// left-most leaf (the writer's split contract, BKDWriter.java:1104).
    fn check_split_chain(
        seg: &WrittenSegment,
        meta: &FieldMeta,
        decoded: &DecodedIndex,
        leaf_fps: &[u64],
    ) {
        let split_by_offset: std::collections::BTreeMap<usize, [u8; 8]> = decoded
            .inners
            .iter()
            .map(|&(_, split, offset)| (offset, split))
            .collect();
        check_node_bounds(
            seg,
            meta.bytes_per_dim,
            &split_by_offset,
            leaf_fps,
            0,
            meta.num_leaves,
            meta.min_value,
            meta.max_value,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn check_node_bounds(
        seg: &WrittenSegment,
        bpd: usize,
        splits: &std::collections::BTreeMap<usize, [u8; 8]>,
        leaf_fps: &[u64],
        leaves_offset: usize,
        num_leaves: usize,
        lo: [u8; 8],
        hi: [u8; 8],
    ) {
        assert!(
            lo[..bpd] <= hi[..bpd],
            "cell min <= max (CheckIndex :3061-3135)"
        );
        if num_leaves == 1 {
            let (points, _) = decode_leaf(&seg.kdd, leaf_fps[leaves_offset], bpd);
            for (v, _) in points {
                assert!(
                    lo[..bpd] <= v[..bpd] && v[..bpd] <= hi[..bpd],
                    "leaf value inside its cell bounds"
                );
            }
            return;
        }
        let num_left = get_num_left_leaf_nodes(num_leaves);
        let right_offset = leaves_offset + num_left;
        let split = splits[&(right_offset - 1)];
        assert!(
            lo[..bpd] <= split[..bpd] && split[..bpd] <= hi[..bpd],
            "left <= split <= right (CheckIndex :3027-3058)"
        );
        let (right_leaf, _) = decode_leaf(&seg.kdd, leaf_fps[right_offset], bpd);
        assert_eq!(
            &split[..bpd],
            &right_leaf[0].0[..bpd],
            "split == first value of the right subtree's left-most leaf"
        );
        check_node_bounds(
            seg,
            bpd,
            splits,
            leaf_fps,
            leaves_offset,
            num_left,
            lo,
            split,
        );
        check_node_bounds(
            seg,
            bpd,
            splits,
            leaf_fps,
            right_offset,
            num_leaves - num_left,
            split,
            hi,
        );
    }

    // ---------- data generators ----------

    fn gen_long_points(rng: &mut Rng, n: usize, doc_range: u64) -> Vec<(i64, u32)> {
        (0..n)
            .map(|_| {
                let value = match rng.below(6) {
                    0 => (rng.below(1_000)) as i64,    // duplicates
                    1 => (rng.below(100)) as i64 - 50, // negatives + heavy duplicates
                    2 => rng.next() as i64,            // full-range
                    3 => [i64::MIN, i64::MAX, 0, -1, 1][rng.below(5) as usize],
                    4 => (rng.next() % 1_000_000) as i64,
                    _ => (rng.below(10)) as i64, // very heavy duplicates
                };
                (value, rng.below(doc_range) as u32)
            })
            .collect()
    }

    fn gen_int_points(rng: &mut Rng, n: usize, doc_range: u64) -> Vec<(i32, u32)> {
        (0..n)
            .map(|_| {
                let value = match rng.below(5) {
                    0 => (rng.below(1_000)) as i32,
                    1 => (rng.below(100)) as i32 - 50,
                    2 => rng.next() as u32 as i32, // full-range
                    3 => [i32::MIN, i32::MAX, 0, -1, 1][rng.below(5) as usize],
                    _ => (rng.below(10)) as i32,
                };
                (value, rng.below(doc_range) as u32)
            })
            .collect()
    }

    // ---------- unit tests: sortable conversion ----------

    #[test]
    fn sortable_bytes_order_preserving() {
        assert_eq!(long_to_sortable_bytes(i64::MIN), [0; 8]);
        assert_eq!(long_to_sortable_bytes(i64::MAX), [0xFF; 8]);
        assert_eq!(long_to_sortable_bytes(0), [0x80, 0, 0, 0, 0, 0, 0, 0]);
        assert!(long_to_sortable_bytes(-1) < long_to_sortable_bytes(0));
        assert!(long_to_sortable_bytes(0) < long_to_sortable_bytes(1));
        assert_eq!(&int_to_sortable_bytes(i32::MIN)[..4], &[0; 4]);
        assert_eq!(&int_to_sortable_bytes(i32::MAX)[..4], &[0xFF; 4]);
        assert_eq!(&int_to_sortable_bytes(0)[..4], &[0x80, 0, 0, 0]);
        assert!(int_to_sortable_bytes(-1) < int_to_sortable_bytes(0));
        assert!(int_to_sortable_bytes(0) < int_to_sortable_bytes(1));
    }

    // ---------- unit tests: DocIdsWriter five branches ----------

    fn doc_ids_roundtrip(docs: &[u32], expected_flag: i8) {
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        write_doc_ids(&mut out, docs).unwrap();
        let bytes = out.into_bytes();
        assert_eq!(bytes[0] as i8, expected_flag);
        let mut cur = Cursor::at(&bytes, 0);
        assert_eq!(decode_doc_ids(&mut cur, docs.len()), docs);
        assert_eq!(cur.pos, bytes.len());
    }

    #[test]
    fn doc_ids_continuous() {
        // strictly sorted, min2max == count (:77-82)
        doc_ids_roundtrip(&(100..612).collect::<Vec<_>>(), -2);
        doc_ids_roundtrip(&[7], -2);
    }

    #[test]
    fn doc_ids_bitset() {
        // strictly sorted, count < min2max <= count << 4 (:83-91)
        let docs: Vec<u32> = (0..300).map(|i| i * 3).collect();
        doc_ids_roundtrip(&docs, -1);
        // crosses 64-bit word boundaries with gaps
        let docs: Vec<u32> = (0..100).map(|i| 1000 + i * 5).collect();
        doc_ids_roundtrip(&docs, -1);
    }

    #[test]
    fn doc_ids_delta16() {
        // not strictly sorted (duplicate docs), small spread (:94-109)
        let docs: Vec<u32> = (0..512).map(|i| (i / 2) as u32).collect();
        doc_ids_roundtrip(&docs, 16);
        // odd count → trailing LE short
        let docs: Vec<u32> = (0..511).map(|i| (i / 2) as u32).collect();
        doc_ids_roundtrip(&docs, 16);
        // strictly sorted but too sparse for the bitset
        let docs: Vec<u32> = (0..100).map(|i| i * 100).collect();
        doc_ids_roundtrip(&docs, 16);
    }

    #[test]
    fn doc_ids_bpv24() {
        // min2max > 0xFFFF, max <= 0xFFFFFF (:111-138); multiple of 8
        let docs: Vec<u32> = (0..512).map(|i| i * 1000).collect();
        doc_ids_roundtrip(&docs, 24);
        // tail shorter than 8 → short+byte per doc
        let docs: Vec<u32> = (0..10).map(|i| i * 100_000).collect();
        doc_ids_roundtrip(&docs, 24);
    }

    #[test]
    fn doc_ids_bpv32() {
        let docs = vec![0, 5, 1 << 24, 7, (1 << 25) + 3, 42];
        doc_ids_roundtrip(&docs, 32);
    }

    // ---------- unit tests: leaf block values branches ----------

    fn leaf_roundtrip(points: &mut [([u8; 8], u32)], bpd: usize) -> Vec<u8> {
        points.sort_by(|a, b| a.0[..bpd].cmp(&b.0[..bpd]).then_with(|| a.1.cmp(&b.1)));
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        write_leaf_block(&mut out, points, bpd).unwrap();
        let bytes = out.into_bytes();
        let (decoded, end) = decode_leaf(&bytes, 0, bpd);
        assert_eq!(end as usize, bytes.len());
        assert_eq!(&decoded[..], &points[..]);
        bytes
    }

    #[test]
    fn leaf_all_equal_values_branch_minus1() {
        // commonPrefixLen == bytesPerDim → byte(-1) (:1281-1284)
        let mut points: Vec<([u8; 8], u32)> = (0..300u32)
            .map(|doc| (long_to_sortable_bytes(42), doc))
            .collect();
        let bytes = leaf_roundtrip(&mut points, 8);
        assert_eq!(*bytes.last().unwrap(), (-1i8) as u8);
    }

    #[test]
    fn leaf_single_point() {
        // single-point leaf: prefix == value → also the -1 branch
        let mut points = vec![(long_to_sortable_bytes(-123_456), 9u32)];
        leaf_roundtrip(&mut points, 8);
        let mut points = vec![(int_to_sortable_bytes(i32::MIN), 0u32)];
        leaf_roundtrip(&mut points, 4);
    }

    #[test]
    fn leaf_run_length_split_at_255() {
        // 300 values 0..300 (u64 BE): common prefix 6 bytes, byte[6] = 0 for
        // the first 256 values → runs 255 + 1 + 44 (:1460-1473)
        let mut points: Vec<([u8; 8], u32)> =
            (0..300u32).map(|i| ((i as u64).to_be_bytes(), i)).collect();
        leaf_roundtrip(&mut points, 8);
    }

    #[test]
    fn leaf_zero_common_prefix_and_int() {
        // i64::MIN and i64::MAX in one leaf → commonPrefixLen == 0
        let mut points = vec![
            (long_to_sortable_bytes(i64::MIN), 3u32),
            (long_to_sortable_bytes(0), 1),
            (long_to_sortable_bytes(i64::MAX), 2),
        ];
        leaf_roundtrip(&mut points, 8);
        // int, 1-byte common prefix
        let mut points: Vec<([u8; 8], u32)> = (0..64u32)
            .map(|i| (int_to_sortable_bytes(0x00AB_0000 + i as i32), i))
            .collect();
        leaf_roundtrip(&mut points, 4);
    }

    // ---------- file-level tests ----------

    #[test]
    fn single_leaf_index_is_one_vlong() {
        // numLeaves == 1: the whole packed index is one VLong holding the
        // absolute fp of the first (only) leaf (:1074-1084)
        let seg = write_segment(
            "single-leaf",
            &[FieldSpec::Long(0, gen_long_points(&mut Rng(1), 100, 1000))],
        );
        let (metas, _, _) = parse_meta(&seg.kdm);
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].num_leaves, 1);
        let index = &seg.kdi[metas[0].index_start_fp as usize
            ..(metas[0].index_start_fp + metas[0].packed_index_byte_length as u64) as usize];
        // single-field segment: first leaf fp == dataStartFP == header length (50)
        assert_eq!(index, &[50u8]);
        let decoded = decode_packed_index(index, 1, 8);
        assert!(decoded.inners.is_empty());
        assert_eq!(decoded.leaves, vec![(1, 0, 50)]);
        check_segment(
            &seg,
            &[FieldSpec::Long(0, gen_long_points(&mut Rng(1), 100, 1000))],
        );
    }

    #[test]
    fn empty_field_writes_no_meta_entry() {
        let fields = [
            FieldSpec::Long(0, vec![]), // skipped
            FieldSpec::Int(1, gen_int_points(&mut Rng(2), 700, 500)),
            FieldSpec::Long(2, vec![]), // skipped
        ];
        let seg = write_segment("empty-field", &fields);
        let (metas, _, _) = parse_meta(&seg.kdm);
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].field_number, 1);
        // no stray bytes: the only field's data starts right after the header
        assert_eq!(
            metas[0].data_start_fp,
            index_header_length(DATA_CODEC_NAME, "") as u64
        );
        check_segment(&seg, &fields);
    }

    #[test]
    fn all_equal_field_multi_leaf() {
        // 1200 identical values → 3 leaves, each in the -1 branch; splits with
        // prefix == bytesPerDim (delta 0)
        let points: Vec<(i64, u32)> = (0..1200u32).map(|doc| (777, doc)).collect();
        let seg = write_segment("all-equal", &[FieldSpec::Long(0, points)]);
        check_segment(
            &seg,
            &[FieldSpec::Long(
                0,
                (0..1200u32).map(|doc| (777, doc)).collect(),
            )],
        );
    }

    #[test]
    fn roundtrip_long_various_sizes() {
        for (i, n) in [1usize, 511, 512, 513, 5000, 100_000].iter().enumerate() {
            let tag = format!("long-{n}");
            let mut rng = Rng(0xC0FFEE + i as u64);
            let points = gen_long_points(&mut rng, *n, 3000);
            let seg = write_segment(&tag, &[FieldSpec::Long(0, points.clone())]);
            check_segment(&seg, &[FieldSpec::Long(0, points)]);
        }
    }

    #[test]
    fn roundtrip_long_doc_spreads() {
        // doc spread > 0xFFFF → BPV_24 leaves; > 0xFFFFFF → BPV_32 leaves
        let mut rng = Rng(42);
        let points = gen_long_points(&mut rng, 5000, 20_000_000);
        let seg = write_segment("long-bpv24", &[FieldSpec::Long(0, points.clone())]);
        check_segment(&seg, &[FieldSpec::Long(0, points)]);
        let mut rng = Rng(43);
        let points = gen_long_points(&mut rng, 5000, 1_000_000_000);
        let seg = write_segment("long-bpv32", &[FieldSpec::Long(0, points.clone())]);
        check_segment(&seg, &[FieldSpec::Long(0, points)]);
    }

    #[test]
    fn roundtrip_long_sorted_docs() {
        // monotonically increasing docs with distinct values → strictly sorted
        // leaves (CONTINUOUS_IDS / BITSET_IDS territory)
        let points: Vec<(i64, u32)> = (0..2000u32).map(|i| (i as i64 * 7, i)).collect();
        let seg = write_segment("long-sorted", &[FieldSpec::Long(0, points.clone())]);
        check_segment(&seg, &[FieldSpec::Long(0, points)]);
        // sparse strictly sorted docs → BITSET_IDS
        let points: Vec<(i64, u32)> = (0..2000u32).map(|i| (i as i64, i * 3)).collect();
        let seg = write_segment("long-bitset", &[FieldSpec::Long(0, points.clone())]);
        check_segment(&seg, &[FieldSpec::Long(0, points)]);
    }

    #[test]
    fn roundtrip_int_boundaries() {
        let mut points: Vec<(i32, u32)> = Vec::new();
        for (i, &v) in [i32::MIN, i32::MIN + 1, -1, 0, 1, i32::MAX - 1, i32::MAX]
            .iter()
            .enumerate()
        {
            for _ in 0..100 {
                points.push((v, (i % 50) as u32)); // duplicates + same-doc multi-values
            }
        }
        let mut rng = Rng(7);
        points.extend(gen_int_points(&mut rng, 4000, 500));
        for (n, slice) in [
            (1, &points[..1]),
            (513, &points[..513]),
            (4700, &points[..]),
        ] {
            let tag = format!("int-{n}");
            let seg = write_segment(&tag, &[FieldSpec::Int(5, slice.to_vec())]);
            check_segment(&seg, &[FieldSpec::Int(5, slice.to_vec())]);
        }
    }

    #[test]
    fn multi_field_layout() {
        let mut rng = Rng(99);
        let fields = [
            FieldSpec::Long(0, gen_long_points(&mut rng, 100, 100)),
            FieldSpec::Int(1, gen_int_points(&mut rng, 6000, 1_000_000)),
            FieldSpec::Long(2, gen_long_points(&mut rng, 512, 50)),
            FieldSpec::Int(3, gen_int_points(&mut rng, 1, 1)),
        ];
        let seg = write_segment("multi-field", &fields);
        check_segment(&seg, &fields);
    }
}
