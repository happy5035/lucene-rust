//! BKD reader for Lucene 90 1D points format, mirroring
//! `util/bkd/BKDReader.java` + `codecs/lucene90/Lucene90PointsReader.java`
//! (9.12.3).
//!
//! Reads `.kdd` leaf blocks and `.kdi` packed index produced by
//! [`crate::points::PointsWriter`], providing pre-order 1D range intersection.

use std::io;

use crate::io::IndexInput;

// DocIdsWriter branch flags (DocIdsWriter.java:30-34).
const CONTINUOUS_IDS: i8 = -2;
const BITSET_IDS: i8 = -1;
const DELTA_BPV_16: u8 = 16;
const BPV_24: u8 = 24;
const BPV_32: u8 = 32;

// ---------------------------------------------------------------------------
// BKDReader
// ---------------------------------------------------------------------------

/// Reader for a single 1D BKD field (always `numDims == numIndexDims == 1`).
///
/// Provides [`intersect`](Self::intersect) which performs a pre-order tree
/// traversal: each node's cell bounds are tested against the query range;
/// inner nodes recurse left / right, leaf nodes decode doc IDs and filter by
/// value.
pub struct BKDReader {
    packed_index: Vec<u8>,
    data_input: Box<dyn IndexInput>,
    num_leaves: u32,
    bytes_per_dim: u8,
    num_dims: u8,
    min_packed_value: Vec<u8>,
    max_packed_value: Vec<u8>,
    point_count: u32,
}

impl BKDReader {
    /// Constructs a reader for one field.
    ///
    /// * `data_input` — the `.kdd` data file (supports random-access `seek`).
    /// * `packed_index` — the byte slice extracted from `.kdi` for this field.
    /// * `bytes_per_dim` — 4 (int) or 8 (long).
    /// * `num_leaves` — leaf count from the `.kdm` meta entry.
    /// * `min_packed_value` / `max_packed_value` — global value bounds
    ///   (sortable bytes, length == `bytes_per_dim`).
    /// * `point_count` — total number of (value, doc) pairs.
    pub fn new(
        data_input: Box<dyn IndexInput>,
        packed_index: Vec<u8>,
        bytes_per_dim: u8,
        num_leaves: u32,
        min_packed_value: Vec<u8>,
        max_packed_value: Vec<u8>,
        point_count: u32,
    ) -> Self {
        Self {
            packed_index,
            data_input,
            num_leaves,
            bytes_per_dim,
            num_dims: 1,
            min_packed_value,
            max_packed_value,
            point_count,
        }
    }

    /// Returns the number of (value, doc) pairs in this field.
    #[allow(dead_code)]
    pub fn point_count(&self) -> u32 {
        self.point_count
    }

    /// Returns the number of leaf blocks.
    #[allow(dead_code)]
    pub fn num_leaves(&self) -> u32 {
        self.num_leaves
    }

    /// Returns bytes per dimension (4 for int, 8 for long).
    #[allow(dead_code)]
    pub fn bytes_per_dim(&self) -> u8 {
        self.bytes_per_dim
    }

    /// Collects all doc IDs whose packed value falls within `[lower, upper]`
    /// (inclusive). Each bound is optional: `None` means unbounded on that
    /// side. Both `None` returns every doc in the field.
    ///
    /// The input values must already be in **sortable byte** form (see
    /// [`super::points::PointsWriter`] for the conversion — sign-bit flip
    /// then big-endian).
    ///
    /// Doc IDs are collected in value order (across leaves) and may repeat
    /// when a doc has multiple values inside the range. The caller is
    /// responsible for deduplication if needed.
    pub fn intersect(
        &mut self,
        lower: Option<Vec<u8>>,
        upper: Option<Vec<u8>>,
    ) -> io::Result<Vec<u32>> {
        let bpd = self.bytes_per_dim as usize;
        let num_leaves = self.num_leaves as usize;

        // Quick reject: field bounds vs. query range.
        if !range_intersects(
            &self.min_packed_value[..bpd],
            &self.max_packed_value[..bpd],
            lower.as_deref(),
            upper.as_deref(),
        ) {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();
        let mut cursor = PackedCursor::new(&self.packed_index);
        let data: &mut dyn IndexInput = &mut *self.data_input;

        intersect_node(
            data,
            &mut cursor,
            &mut results,
            &self.min_packed_value[..bpd],
            &self.max_packed_value[..bpd],
            0,        // min_block_fp (root starts at 0)
            [0u8; 8], // last_split_value (root sees zero)
            false,    // negative_delta
            false,    // is_left (root is treated as a "right child")
            0,        // leaves_offset
            num_leaves,
            bpd,
            lower.as_deref(),
            upper.as_deref(),
        )?;

        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// Packed index cursor: reads unsigned VInt / VLong from a byte slice
// ---------------------------------------------------------------------------

struct PackedCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> PackedCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn byte(&mut self) -> io::Result<u8> {
        self.bytes
            .get(self.pos)
            .copied()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "packed index: unexpected end",
                )
            })
            .map(|b| {
                self.pos += 1;
                b
            })
    }

    /// Unsigned VInt (DataInput.readVInt for non-negative values).
    fn vint(&mut self) -> io::Result<u32> {
        let mut v = 0u32;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            v |= (b as u32 & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    /// Unsigned VLong (DataInput.readVLong for non-negative values).
    fn vlong(&mut self) -> io::Result<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            v |= (b as u64 & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    fn bytes_slice(&mut self, len: usize) -> io::Result<&'a [u8]> {
        if self.pos + len > self.bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "packed index: unexpected end",
            ));
        }
        let slice = &self.bytes[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }
}

// ---------------------------------------------------------------------------
// Tree-shape helper (BKDWriter.getNumLeftLeafNodes, :831-847)
// ---------------------------------------------------------------------------

fn get_num_left_leaf_nodes(num_leaves: usize) -> usize {
    debug_assert!(num_leaves > 1);
    let last_full_level = usize::BITS - 1 - num_leaves.leading_zeros();
    let leaves_full_level = 1usize << last_full_level;
    let num_left = leaves_full_level / 2;
    let unbalanced = num_leaves - leaves_full_level;
    num_left + unbalanced.min(num_left)
}

// ---------------------------------------------------------------------------
// Range predicates on sortable bytes
// ---------------------------------------------------------------------------

/// Checks whether the cell `[cell_min, cell_max]` has any overlap with the
/// query range `[lower, upper]`.
fn range_intersects(
    cell_min: &[u8],
    cell_max: &[u8],
    lower: Option<&[u8]>,
    upper: Option<&[u8]>,
) -> bool {
    if let Some(upper) = upper {
        if cell_min > upper {
            return false;
        }
    }
    if let Some(lower) = lower {
        if cell_max < lower {
            return false;
        }
    }
    true
}

/// Tests whether a single packed `value` falls inside `[lower, upper]`.
fn value_in_range(value: &[u8], lower: Option<&[u8]>, upper: Option<&[u8]>) -> bool {
    if let Some(lower) = lower {
        if value < lower {
            return false;
        }
    }
    if let Some(upper) = upper {
        if value > upper {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// LE primitive readers on IndexInput
// ---------------------------------------------------------------------------

fn read_int_le(input: &mut dyn IndexInput) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    input.read_bytes(&mut buf, 0, 4)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_long_le(input: &mut dyn IndexInput) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    input.read_bytes(&mut buf, 0, 8)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_short_le(input: &mut dyn IndexInput) -> io::Result<u16> {
    let mut buf = [0u8; 2];
    input.read_bytes(&mut buf, 0, 2)?;
    Ok(u16::from_le_bytes(buf))
}

// ---------------------------------------------------------------------------
// DocIds block decoder (mirrors DocIdsWriter.readInts, :182-206)
// ---------------------------------------------------------------------------

fn read_doc_ids(input: &mut dyn IndexInput, count: usize) -> io::Result<Vec<u32>> {
    let flag = input.read_byte()? as i8;
    match flag {
        CONTINUOUS_IDS => {
            let start = input.read_vint()? as u32;
            Ok((0..count as u32).map(|i| start + i).collect())
        }
        BITSET_IDS => {
            let offset_words = input.read_vint()? as u32;
            let word_count = input.read_vint()? as usize;
            let mut docs = Vec::with_capacity(count);
            for w in 0..word_count {
                let mut word = read_long_le(input)?;
                while word != 0 {
                    let bit = word.trailing_zeros();
                    docs.push((offset_words << 6) + 64 * w as u32 + bit);
                    word &= word - 1;
                }
            }
            Ok(docs)
        }
        flag if flag == DELTA_BPV_16 as i8 => {
            let min = input.read_vint()? as u32;
            let half_len = count / 2;
            let mut docs = vec![0u32; count];
            for i in 0..half_len {
                let packed = read_int_le(input)?;
                docs[i] = (packed >> 16) + min;
                docs[half_len + i] = (packed & 0xFFFF) + min;
            }
            if count & 1 == 1 {
                docs[count - 1] = read_short_le(input)? as u32 + min;
            }
            Ok(docs)
        }
        flag if flag == BPV_24 as i8 => {
            let mut docs = vec![0u32; count];
            let mut i = 0;
            while i + 8 <= count {
                let l1 = read_long_le(input)?;
                let l2 = read_long_le(input)?;
                let l3 = read_long_le(input)?;
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
                docs[i] = (read_short_le(input)? as u32) << 8 | input.read_byte()? as u32;
                i += 1;
            }
            Ok(docs)
        }
        flag if flag == BPV_32 as i8 => {
            let mut docs = vec![0u32; count];
            for d in docs.iter_mut() {
                *d = read_int_le(input)?;
            }
            Ok(docs)
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown doc ids flag {other}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Leaf block reader (BKDReader.readDocIDs + visitDocValues*)
// ---------------------------------------------------------------------------

/// Reads one leaf block at `fp` and returns doc IDs whose value falls inside
/// `[lower, upper]`.
fn read_leaf_block(
    input: &mut dyn IndexInput,
    fp: u64,
    bpd: usize,
    lower: Option<&[u8]>,
    upper: Option<&[u8]>,
) -> io::Result<Vec<u32>> {
    input.seek(fp)?;

    let count = input.read_vint()? as usize;
    let docs = read_doc_ids(input, count)?;

    // Common prefix of first and last value in this leaf.
    let common_prefix_len = input.read_vint()? as usize;
    let mut value_base = [0u8; 8];
    if common_prefix_len > 0 {
        input.read_bytes(&mut value_base, 0, common_prefix_len)?;
    }

    let compressed_dim = input.read_byte()? as i8;
    let mut results = Vec::new();

    if compressed_dim == -1 {
        // All values in this leaf are equal to the common prefix (which by
        // definition is the full value).
        if value_in_range(&value_base[..bpd], lower, upper) {
            results.extend_from_slice(&docs);
        }
    } else if compressed_dim == 0 {
        // High-cardinality: run-length on byte at offset common_prefix_len,
        // then per-point suffix bytes.
        let suffix_len = bpd.saturating_sub(common_prefix_len + 1);
        let mut i = 0;
        while i < count {
            let run_byte = input.read_byte()?;
            let run_len = input.read_byte()? as usize;
            let mut v = value_base;
            v[common_prefix_len] = run_byte;

            if suffix_len == 0 {
                // No suffix bytes; all values in this run are identical.
                if value_in_range(&v[..bpd], lower, upper) {
                    results.extend_from_slice(&docs[i..i + run_len]);
                }
            } else {
                // Each point in the run has its own suffix.
                for j in 0..run_len {
                    if suffix_len > 0 {
                        input.read_bytes(
                            &mut v[common_prefix_len + 1..],
                            0,
                            suffix_len,
                        )?;
                    }
                    if value_in_range(&v[..bpd], lower, upper) {
                        results.push(docs[i + j]);
                    }
                }
            }
            i += run_len;
        }
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected compressedDim {compressed_dim}; reader never emits -2"),
        ));
    }

    Ok(results)
}

// ---------------------------------------------------------------------------
// Pre-order tree traversal (BKDReader intersect path)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn intersect_node(
    data_input: &mut dyn IndexInput,
    cursor: &mut PackedCursor<'_>,
    results: &mut Vec<u32>,
    cell_min: &[u8],
    cell_max: &[u8],
    min_block_fp: u64,
    last_split_value: [u8; 8],
    negative_delta: bool,
    is_left: bool,
    leaves_offset: usize,
    num_leaves: usize,
    bpd: usize,
    lower: Option<&[u8]>,
    upper: Option<&[u8]>,
) -> io::Result<()> {
    // Compute this subtree's left-most leaf FP.
    // IMPORTANT: always read the VLong for non-left children so the packed
    // index cursor stays in sync even when the cell is pruned.
    let mut fp = min_block_fp;
    if !is_left {
        fp += cursor.vlong()?;
    }

    let intersects = range_intersects(cell_min, cell_max, lower, upper);

    if num_leaves == 1 {
        // Leaf node: only read the data file if the cell intersects.
        if intersects {
            let leaf_docs = read_leaf_block(data_input, fp, bpd, lower, upper)?;
            results.extend_from_slice(&leaf_docs);
        }
        return Ok(());
    }

    // ── Inner node: decode split value ──
    // Always consume the packed index bytes so the cursor stays in sync.

    let code = cursor.vint()? as i32;
    let div = 1 + bpd as i32;
    let prefix = (code % div) as usize;
    let suffix_len = bpd.wrapping_sub(prefix);

    let mut split_value = last_split_value;
    if suffix_len > 0 {
        let mut first_diff_byte_delta = code / div;
        if negative_delta {
            first_diff_byte_delta = -first_diff_byte_delta;
        }
        let old_byte = i32::from(split_value[prefix]);
        split_value[prefix] = (old_byte + first_diff_byte_delta) as u8;
        if suffix_len > 1 {
            let suffix_bytes = cursor.bytes_slice(suffix_len - 1)?;
            split_value[prefix + 1..prefix + suffix_len]
                .copy_from_slice(suffix_bytes);
        }
    }
    // else: split == last_split on this dim (suffix_len == 0).

    let num_left = get_num_left_leaf_nodes(num_leaves);
    let right_offset = leaves_offset + num_left;

    // leftNumBytes is present only when the left child is an inner node.
    let _left_num_bytes = if num_left > 1 {
        cursor.vlong()? as usize
    } else {
        0
    };

    // ── Always recurse both children to keep the cursor in sync ──
    // Left child: [cell_min, split_value]
    intersect_node(
        data_input,
        cursor,
        results,
        cell_min,
        &split_value[..bpd],
        fp,
        split_value,
        true,  // negative_delta for left
        true,  // is_left
        leaves_offset,
        num_left,
        bpd,
        lower,
        upper,
    )?;

    // Right child: [split_value, cell_max]
    intersect_node(
        data_input,
        cursor,
        results,
        &split_value[..bpd],
        cell_max,
        fp,
        split_value,
        false, // negative_delta for right
        false, // is_left
        right_offset,
        num_leaves - num_left,
        bpd,
        lower,
        upper,
    )?;

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec_util::{index_header_length, CODEC_MAGIC};
    use crate::directory::FSDirectory;
    use crate::io::HeapIndexInput;
    use crate::points::{PointsWriter, MAX_POINTS_IN_LEAF_NODE as WRITER_MAX_LEAF};

    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;

    // --- Deterministic RNG ---

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

    // --- Sortable byte helpers ---

    fn long_to_sortable_bytes(value: i64) -> [u8; 8] {
        ((value as u64) ^ 0x8000_0000_0000_0000).to_be_bytes()
    }

    fn int_to_sortable_bytes(value: i32) -> [u8; 8] {
        let mut bytes = [0u8; 8];
        bytes[..4].copy_from_slice(&((value as u32) ^ 0x8000_0000).to_be_bytes());
        bytes
    }

    // --- Temp dir + segment helpers ---

    struct TestSegment {
        dir_path: PathBuf,
        kdd_bytes: Vec<u8>,
        kdi_bytes: Vec<u8>,
        kdm_bytes: Vec<u8>,
    }

    impl Drop for TestSegment {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir_path);
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codec-lucene9-bkd-reader-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn write_long_field(tag: &str, points: &[(i64, u32)]) -> TestSegment {
        let dir_path = temp_dir(tag);
        let segment = "_0";
        let seg_id = [0xDBu8; 16];
        {
            let dir = FSDirectory::open(&dir_path).unwrap();
            let mut writer = PointsWriter::new(&dir, segment, &seg_id).unwrap();
            writer.write_field_long(0, &mut points.to_vec()).unwrap();
            writer.finish().unwrap();
        }
        TestSegment {
            kdd_bytes: fs::read(dir_path.join(format!("{segment}.kdd"))).unwrap(),
            kdi_bytes: fs::read(dir_path.join(format!("{segment}.kdi"))).unwrap(),
            kdm_bytes: fs::read(dir_path.join(format!("{segment}.kdm"))).unwrap(),
            dir_path,
        }
    }

    fn write_int_field(tag: &str, points: &[(i32, u32)]) -> TestSegment {
        let dir_path = temp_dir(tag);
        let segment = "_0";
        let seg_id = [0xDBu8; 16];
        {
            let dir = FSDirectory::open(&dir_path).unwrap();
            let mut writer = PointsWriter::new(&dir, segment, &seg_id).unwrap();
            writer.write_field_int(0, &mut points.to_vec()).unwrap();
            writer.finish().unwrap();
        }
        TestSegment {
            kdd_bytes: fs::read(dir_path.join(format!("{segment}.kdd"))).unwrap(),
            kdi_bytes: fs::read(dir_path.join(format!("{segment}.kdi"))).unwrap(),
            kdm_bytes: fs::read(dir_path.join(format!("{segment}.kdm"))).unwrap(),
            dir_path,
        }
    }

    /// Parse .kdm into per-field metadata (simplified from points.rs tests).
    fn parse_kdm_meta(kdm: &[u8]) -> Vec<KdmFieldMeta> {
        let _data_codec = "Lucene90PointsFormatData";
        let _index_codec = "Lucene90PointsFormatIndex";
        let meta_codec = "Lucene90PointsFormatMeta";

        let hdr_len = index_header_length(meta_codec, "");
        let mut pos = hdr_len;

        // Read LE field_number until sentinel -1.
        let mut fields = Vec::new();
        loop {
            let field_number = i32::from_le_bytes(
                kdm[pos..pos + 4].try_into().unwrap(),
            );
            pos += 4;
            if field_number == -1 {
                break;
            }
            // BKD header: magic + "BKD" + version (CodecUtil.writeHeader, points.rs:239)
            let magic = u32::from_be_bytes(kdm[pos..pos + 4].try_into().unwrap());
            pos += 4;
            assert_eq!(magic, CODEC_MAGIC);
            let name_len = read_vint_at(kdm, &mut pos);
            let name = &kdm[pos..pos + name_len];
            pos += name_len;
            assert_eq!(name, b"BKD");
            let version = u32::from_be_bytes(kdm[pos..pos + 4].try_into().unwrap());
            pos += 4;
            assert_eq!(version, 9);

            assert_eq!(read_vint_at(kdm, &mut pos), 1); // numDims
            assert_eq!(read_vint_at(kdm, &mut pos), 1); // numIndexDims
            assert_eq!(read_vint_at(kdm, &mut pos), WRITER_MAX_LEAF as usize);
            let bytes_per_dim = read_vint_at(kdm, &mut pos);
            let num_leaves = read_vint_at(kdm, &mut pos);
            assert!(num_leaves > 0);

            let mut min_value = vec![0u8; bytes_per_dim];
            min_value.copy_from_slice(&kdm[pos..pos + bytes_per_dim]);
            pos += bytes_per_dim;

            let mut max_value = vec![0u8; bytes_per_dim];
            max_value.copy_from_slice(&kdm[pos..pos + bytes_per_dim]);
            pos += bytes_per_dim;

            let point_count = read_vlong_at(kdm, &mut pos);
            let _doc_count = read_vint_at(kdm, &mut pos);
            let packed_index_len = read_vint_at(kdm, &mut pos);
            let _data_start_fp = u64::from_le_bytes(
                kdm[pos..pos + 8].try_into().unwrap(),
            );
            pos += 8;
            let index_start_fp = u64::from_le_bytes(
                kdm[pos..pos + 8].try_into().unwrap(),
            );
            pos += 8;

            fields.push(KdmFieldMeta {
                field_number,
                bytes_per_dim: bytes_per_dim as u8,
                num_leaves: num_leaves as u32,
                min_value,
                max_value,
                point_count: point_count as u32,
                packed_index_len,
                index_start_fp,
            });
        }

        fields
    }

    struct KdmFieldMeta {
        field_number: i32,
        bytes_per_dim: u8,
        num_leaves: u32,
        min_value: Vec<u8>,
        max_value: Vec<u8>,
        point_count: u32,
        packed_index_len: usize,
        index_start_fp: u64,
    }

    fn read_vint_at(bytes: &[u8], pos: &mut usize) -> usize {
        let mut v = 0u32;
        let mut shift = 0;
        loop {
            let b = bytes[*pos];
            *pos += 1;
            v |= (b as u32 & 0x7f) << shift;
            if b & 0x80 == 0 {
                return v as usize;
            }
            shift += 7;
        }
    }

    fn read_vlong_at(bytes: &[u8], pos: &mut usize) -> u64 {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = bytes[*pos];
            *pos += 1;
            v |= (b as u64 & 0x7f) << shift;
            if b & 0x80 == 0 {
                return v;
            }
            shift += 7;
        }
    }

    /// Build a BKDReader from a written segment's first (only) field.
    fn reader_from_segment(
        seg: &TestSegment,
    ) -> (BKDReader, KdmFieldMeta) {
        let metas = parse_kdm_meta(&seg.kdm_bytes);
        assert_eq!(metas.len(), 1, "expected exactly one field");
        let meta = metas.into_iter().next().unwrap();

        let idx_start = meta.index_start_fp as usize;
        let idx_end = idx_start + meta.packed_index_len;
        let packed_index = seg.kdi_bytes[idx_start..idx_end].to_vec();

        let data_input = Box::new(HeapIndexInput::new(seg.kdd_bytes.clone()));

        let reader = BKDReader::new(
            data_input,
            packed_index,
            meta.bytes_per_dim,
            meta.num_leaves,
            meta.min_value.clone(),
            meta.max_value.clone(),
            meta.point_count,
        );

        (reader, meta)
    }

    /// Build the sorted, deduplicated set of expected (value, doc) pairs
    /// inside [lower, upper] from the raw input.
    fn expected_matches<T: Copy + Ord>(
        points: &[(T, u32)],
        bytes_per_dim: usize,
        to_sortable: fn(T) -> [u8; 8],
        lower: Option<&[u8]>,
        upper: Option<&[u8]>,
    ) -> Vec<u32> {
        let mut docs = BTreeSet::new();
        for &(value, doc) in points {
            let packed = to_sortable(value);
            if value_in_range(&packed[..bytes_per_dim], lower, upper) {
                docs.insert(doc);
            }
        }
        docs.into_iter().collect()
    }

    // ========================================================================
    // Round-trip tests
    // ========================================================================

    #[test]
    fn intersect_empty_range() {
        // Query range completely outside the field range returns nothing.
        let points: Vec<(i64, u32)> = (0..100).map(|i| (i as i64, i as u32)).collect();
        let seg = write_long_field("empty-range", &points);
        let (mut reader, _meta) = reader_from_segment(&seg);

        // All values are [0, 99]; query [200, 300] — no overlap.
        let got = reader
            .intersect(
                Some(long_to_sortable_bytes(200).to_vec()),
                Some(long_to_sortable_bytes(300).to_vec()),
            )
            .unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn intersect_full_range() {
        // Unbounded query returns every point.
        let points: Vec<(i64, u32)> = (0..1000).map(|i| (i as i64, (i % 50) as u32)).collect();
        let seg = write_long_field("full-range", &points);
        let (mut reader, meta) = reader_from_segment(&seg);

        let got = reader.intersect(None, None).unwrap();
        let expected = expected_matches(
            &points, 8, long_to_sortable_bytes, None, None,
        );
        assert_eq!(got.len(), meta.point_count as usize);
        // Sort both and compare as sets.
        let mut got_sorted = got.clone();
        got_sorted.sort_unstable();
        got_sorted.dedup();
        assert_eq!(got_sorted, expected);
    }

    #[test]
    fn intersect_sub_range_long() {
        let mut rng = Rng(42);
        let n = 5000usize;
        let doc_range = 2000u64;
        let mut points = Vec::with_capacity(n);
        for _ in 0..n {
            let v = (rng.next() % 100_000) as i64 - 50_000;
            points.push((v, rng.below(doc_range) as u32));
        }

        let seg = write_long_field("sub-range-long", &points);
        let (mut reader, _meta) = reader_from_segment(&seg);

        // Query a slice in the middle.
        let lo = -10_000i64;
        let hi = 10_000i64;
        let lower = Some(long_to_sortable_bytes(lo).to_vec());
        let upper = Some(long_to_sortable_bytes(hi).to_vec());

        let got = reader.intersect(lower.clone(), upper.clone()).unwrap();

        let expected = expected_matches(
            &points,
            8,
            long_to_sortable_bytes,
            lower.as_deref().map(|v| &v[..]),
            upper.as_deref().map(|v| &v[..]),
        );
        // got may contain duplicates (multi-value docs), so compare sorted + deduped.
        let mut got_set: Vec<u32> = got.into_iter().collect();
        got_set.sort_unstable();
        got_set.dedup();
        assert_eq!(got_set, expected);
    }

    #[test]
    fn intersect_sub_range_int() {
        let mut rng = Rng(77);
        let n = 3000usize;
        let doc_range = 1000u64;
        let mut points = Vec::with_capacity(n);
        for _ in 0..n {
            let v = (rng.next() % 500_000) as i32 - 250_000;
            points.push((v, rng.below(doc_range) as u32));
        }

        let seg = write_int_field("sub-range-int", &points);
        let (mut reader, _meta) = reader_from_segment(&seg);

        let lo = -50_000i32;
        let hi = 50_000i32;
        let lower = Some(int_to_sortable_bytes(lo)[..4].to_vec());
        let upper = Some(int_to_sortable_bytes(hi)[..4].to_vec());

        let got = reader.intersect(lower.clone(), upper.clone()).unwrap();

        let expected = expected_matches(
            &points,
            4,
            |v| int_to_sortable_bytes(v),
            lower.as_deref().map(|v| &v[..]),
            upper.as_deref().map(|v| &v[..]),
        );
        let mut got_set: Vec<u32> = got.into_iter().collect();
        got_set.sort_unstable();
        got_set.dedup();
        assert_eq!(got_set, expected);
    }

    #[test]
    fn intersect_single_point() {
        // Exactly one point in the field — query that exact value.
        let points = vec![(42i64, 7u32)];
        let seg = write_long_field("single-pt", &points);
        let (mut reader, _meta) = reader_from_segment(&seg);

        let val = long_to_sortable_bytes(42).to_vec();
        let got = reader
            .intersect(Some(val.clone()), Some(val.clone()))
            .unwrap();
        assert_eq!(got, vec![7]);

        // Query a different value → empty.
        let got = reader
            .intersect(
                Some(long_to_sortable_bytes(99).to_vec()),
                Some(long_to_sortable_bytes(99).to_vec()),
            )
            .unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn intersect_boundary_exact() {
        // Query that exactly matches min/max values of the field.
        let points: Vec<(i64, u32)> = vec![
            (-100, 1),
            (-50, 2),
            (0, 3),
            (50, 4),
            (100, 5),
        ];
        let seg = write_long_field("boundary", &points);
        let (mut reader, _meta) = reader_from_segment(&seg);

        // Lower-bound only: [-50, +inf)
        let got = reader
            .intersect(Some(long_to_sortable_bytes(-50).to_vec()), None)
            .unwrap();
        let got_set: BTreeSet<u32> = got.into_iter().collect();
        assert_eq!(got_set, BTreeSet::from([2, 3, 4, 5]));

        // Upper-bound only: (-inf, 50]
        let got = reader
            .intersect(None, Some(long_to_sortable_bytes(50).to_vec()))
            .unwrap();
        let got_set: BTreeSet<u32> = got.into_iter().collect();
        assert_eq!(got_set, BTreeSet::from([1, 2, 3, 4]));

        // Exact: [-50, 50]
        let got = reader
            .intersect(
                Some(long_to_sortable_bytes(-50).to_vec()),
                Some(long_to_sortable_bytes(50).to_vec()),
            )
            .unwrap();
        let got_set: BTreeSet<u32> = got.into_iter().collect();
        assert_eq!(got_set, BTreeSet::from([2, 3, 4]));
    }

    #[test]
    fn intersect_all_equal_values() {
        // All values identical — tests the -1 (all-equal) leaf branch.
        let points: Vec<(i64, u32)> = (0..1200u32).map(|doc| (777, doc)).collect();
        let seg = write_long_field("all-equal", &points);
        let (mut reader, meta) = reader_from_segment(&seg);
        assert_eq!(meta.point_count, 1200);
        // num_leaves > 1 ensures we hit inner nodes with all-equal splits.
        assert!(meta.num_leaves > 1);

        // Query that includes 777.
        let val = long_to_sortable_bytes(777).to_vec();
        let got = reader
            .intersect(Some(val.clone()), Some(val))
            .unwrap();
        let got_set: BTreeSet<u32> = got.into_iter().collect();
        assert_eq!(got_set, (0..1200u32).collect());

        // Query that excludes 777.
        let got = reader
            .intersect(
                Some(long_to_sortable_bytes(0).to_vec()),
                Some(long_to_sortable_bytes(100).to_vec()),
            )
            .unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn intersect_multi_leaf_non_full_last() {
        // Sizes that produce a non-full rightmost leaf.
        for n in [511usize, 513, 1023, 1025, 2047] {
            let points: Vec<(i64, u32)> = (0..n)
                .map(|i| (i as i64 * 3, (i % 200) as u32))
                .collect();
            let seg = write_long_field(&format!("nonfull-{n}"), &points);
            let (mut reader, _meta) = reader_from_segment(&seg);

            // Full range.
            let got = reader.intersect(None, None).unwrap();
            assert_eq!(got.len(), n);

            // Sub-range: middle third.
            let lo = (n / 3) as i64 * 3;
            let hi = (2 * n / 3) as i64 * 3;
            let got = reader
                .intersect(
                    Some(long_to_sortable_bytes(lo).to_vec()),
                    Some(long_to_sortable_bytes(hi).to_vec()),
                )
                .unwrap();
            let got_set: BTreeSet<u32> = got.into_iter().collect();
            assert!(!got_set.is_empty());
        }
    }

    #[test]
    fn intersect_multi_field_segment() {
        // Two fields in one segment: ensure the .kdm + .kdi multi-field layout
        // is parsed correctly and each reader works independently.
        let long_pts: Vec<(i64, u32)> = (0..600u32)
            .map(|i| (i as i64 * 2, i % 100))
            .collect();
        let int_pts: Vec<(i32, u32)> = (0..600u32)
            .map(|i| (i as i32 - 300, (i + 50) % 100))
            .collect();

        let dir_path = temp_dir("multi-field-read");
        let segment = "_0";
        let seg_id = [0xCCu8; 16];
        {
            let dir = FSDirectory::open(&dir_path).unwrap();
            let mut writer = PointsWriter::new(&dir, segment, &seg_id).unwrap();
            writer.write_field_long(1, &mut long_pts.clone()).unwrap();
            writer.write_field_int(2, &mut int_pts.clone()).unwrap();
            writer.finish().unwrap();
        }
        let kdd = fs::read(dir_path.join(format!("{segment}.kdd"))).unwrap();
        let kdi = fs::read(dir_path.join(format!("{segment}.kdi"))).unwrap();
        let kdm = fs::read(dir_path.join(format!("{segment}.kdm"))).unwrap();

        let _seg_dir = TestSegment {
            dir_path,
            kdd_bytes: kdd.clone(),
            kdi_bytes: kdi.clone(),
            kdm_bytes: kdm.clone(),
        };

        let metas = parse_kdm_meta(&kdm);
        assert_eq!(metas.len(), 2);

        // Field 1: long.
        let m0 = &metas[0];
        assert_eq!(m0.field_number, 1);
        let pi0 = kdi[m0.index_start_fp as usize..][..m0.packed_index_len].to_vec();
        let di0 = Box::new(HeapIndexInput::new(kdd.clone()));
        let mut r0 = BKDReader::new(
            di0, pi0, m0.bytes_per_dim, m0.num_leaves,
            m0.min_value.clone(), m0.max_value.clone(), m0.point_count,
        );
        let got0 = r0.intersect(
            Some(long_to_sortable_bytes(100).to_vec()),
            Some(long_to_sortable_bytes(200).to_vec()),
        ).unwrap();
        assert!(!got0.is_empty());

        // Field 2: int.
        let m1 = &metas[1];
        assert_eq!(m1.field_number, 2);
        let pi1 = kdi[m1.index_start_fp as usize..][..m1.packed_index_len].to_vec();
        let di1 = Box::new(HeapIndexInput::new(kdd));
        let mut r1 = BKDReader::new(
            di1, pi1, m1.bytes_per_dim, m1.num_leaves,
            m1.min_value.clone(), m1.max_value.clone(), m1.point_count,
        );
        let got1 = r1.intersect(
            Some(int_to_sortable_bytes(-100)[..4].to_vec()),
            Some(int_to_sortable_bytes(100)[..4].to_vec()),
        ).unwrap();
        assert!(!got1.is_empty());
    }

    #[test]
    fn intersect_5000_sequential() {
        // 5000 sequential points → 10 leaves. Verifies deeper trees.
        let points: Vec<(i64, u32)> = (0..5000)
            .map(|i| ((i as i64 - 2500) * 7, (i % 700) as u32))
            .collect();
        let seg = write_long_field("seq5000", &points);
        let (mut reader, meta) = reader_from_segment(&seg);
        assert_eq!(meta.point_count, 5000);
        assert!(meta.num_leaves > 5, "expected 10 leaves, got {}", meta.num_leaves);

        // Full range returns all points.
        let got = reader.intersect(None, None).unwrap();
        assert_eq!(got.len(), 5000);
    }

    #[test]
    fn intersect_random_larger() {
        // Reproduce the random-test pattern with a fallback seed.
        
        let mut rng = Rng(42);
        let n = 5000usize;
        let doc_range = 2000u64;
        let mut points = Vec::with_capacity(n);
        for _ in 0..n {
            let v = (rng.next() % 100_000) as i64 - 50_000;
            points.push((v, rng.below(doc_range) as u32));
        }

        let seg = write_long_field("random5000", &points);
        let (mut reader, meta) = reader_from_segment(&seg);
        assert_eq!(meta.point_count, 5000);

        // Full range: should return all points without error.
        let got = reader.intersect(None, None).unwrap();
        assert_eq!(got.len(), 5000, "full range should match point count");
    }
}
