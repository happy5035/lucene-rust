//! Shard merge operations:
//! - `merge_intra_shard`: merges multiple docs of the same series within a
//!   single shard into one doc (sort + dedup + re-encode). See design doc §6.2.
//! - `merge_cross_shard`: k-way merge of multiple input shards into one compact
//!   output, simultaneously generating 5m and 1h downsample shards. See §6.3.

use std::collections::{BinaryHeap, HashMap};
use std::io;
use std::path::Path;

use codec_lucene9::FSDirectory;
use rustlucene_core::document::{Document, FieldValue};
use rustlucene_core::index_writer::{IndexWriter, IndexWriterConfig};
use rustlucene_core::search::reader::Reader;
use rustlucene_core::IndexSortField;

use crate::algo::downsample::{downsample, INTERVAL_1H, INTERVAL_5M};
use crate::algo::gorilla;
use crate::store::schema::{downsample_schema, v5_schema};
use crate::store::series_writer::write_series;

/// Statistics from a merge_intra_shard operation.
pub struct MergeStats {
    pub input_docs: usize,
    pub output_docs: usize,
    pub input_points: usize,
    pub output_points: usize,
    /// Groups with >1 input doc (actual merges that happened).
    pub merged_groups: usize,
}

struct SeriesData {
    name: String,
    labels: String,
    points: Vec<(i64, f64)>,
    doc_count: usize,
}

/// Merge same-series docs within a single shard.
/// Reads input_shard_dir, writes merged output to output_shard_dir.
pub fn merge_intra_shard(
    input_shard_dir: &Path,
    output_shard_dir: &Path,
) -> io::Result<MergeStats> {
    let in_dir = FSDirectory::open(input_shard_dir)?;
    let mut reader = Reader::open(&in_dir)?;

    // Collect all docs grouped by series_hash
    let mut groups: HashMap<i64, SeriesData> = HashMap::new();
    let mut input_docs = 0usize;
    let mut input_points = 0usize;

    for (_doc_base, seg) in reader.leaves() {
        let hashes = seg.numeric_values("series_hash")?;
        let counts = seg.numeric_values("sample_count")?;
        let gorilla_datas = seg.binary_values("gorilla_data")?;
        let names = seg.sorted_values("metric_name")?;
        let labels = seg.binary_values("metric_labels")?;

        // Build lookup maps (doc_id -> value)
        let hash_map: HashMap<u32, i64> = hashes.into_iter().collect();
        let count_map: HashMap<u32, i64> = counts.into_iter().collect();
        let name_map: HashMap<u32, Vec<u8>> = names.into_iter().collect();
        let label_map: HashMap<u32, Vec<u8>> = labels.into_iter().collect();

        for (doc_id, gorilla_bytes) in &gorilla_datas {
            let doc_id = *doc_id;
            let sample_count = count_map.get(&doc_id).copied().unwrap_or(0) as usize;
            let hash = hash_map.get(&doc_id).copied().unwrap_or(0);

            let (times, values) = gorilla::decode(gorilla_bytes, sample_count)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

            input_points += times.len();
            input_docs += 1;

            let name = name_map
                .get(&doc_id)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            let labels_str = label_map
                .get(&doc_id)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();

            let entry = groups.entry(hash).or_insert_with(|| SeriesData {
                name,
                labels: labels_str,
                points: Vec::new(),
                doc_count: 0,
            });
            entry.points.extend(times.into_iter().zip(values.into_iter()));
            entry.doc_count += 1;
        }
    }

    // Write merged output
    let schema = v5_schema();
    let mut writer = IndexWriter::create(output_shard_dir, schema, IndexWriterConfig::default())?;
    let mut output_docs = 0usize;
    let mut output_points = 0usize;
    let mut merged_groups = 0usize;

    for (_hash, data) in &groups {
        if data.doc_count > 1 {
            merged_groups += 1;
        }

        // Sort by time, dedup (keep first occurrence)
        let mut points = data.points.clone();
        points.sort_by_key(|(t, _)| *t);
        points.dedup_by_key(|(t, _)| *t);

        output_points += points.len();

        if points.is_empty() {
            continue;
        }

        // Parse labels_str back to sorted_labels for write_series
        let sorted_labels = parse_labels_str(&data.labels);
        let times: Vec<i64> = points.iter().map(|(t, _)| *t).collect();
        let values: Vec<f64> = points.iter().map(|(_, v)| *v).collect();

        write_series(&mut writer, &data.name, &sorted_labels, &times, &values)?;
        output_docs += 1;
    }

    writer.commit()?;

    Ok(MergeStats {
        input_docs,
        output_docs,
        input_points,
        output_points,
        merged_groups,
    })
}

/// Parse "$#$k1=v1$#$k2=v2$#$" back to sorted label pairs.
fn parse_labels_str(s: &str) -> Vec<(String, String)> {
    s.split("$#$")
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let mut it = p.splitn(2, '=');
            let k = it.next()?.to_string();
            let v = it.next()?.to_string();
            Some((k, v))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Cross-shard merge (§6.3)
// ---------------------------------------------------------------------------

/// Statistics from a merge_cross_shard operation.
pub struct CrossMergeStats {
    pub input_shards: usize,
    pub input_docs: usize,
    pub output_docs: usize,
    pub input_points: usize,
    pub output_points: usize,
    pub downsample_5m_docs: usize,
    pub downsample_1h_docs: usize,
}

// ---------------------------------------------------------------------------
// Streaming k-way merge helpers
// ---------------------------------------------------------------------------

/// A shard's docs laid out in ascending series_hash order with a read cursor.
/// Gorilla bytes are held in ONE contiguous `gorilla` buffer; `offsets[i]`
/// gives doc i's `(start, end)` byte range, parallel to `hashes`/`counts`.
/// This avoids one heap allocation per doc (12M docs/shard-set otherwise).
/// name/labels are NOT stored here — they live once in the global series map.
struct ShardCursor {
    hashes: Vec<i64>,
    counts: Vec<usize>,
    gorilla: Vec<u8>,
    offsets: Vec<(u32, u32)>,
    pos: usize,
}

/// Min-heap entry. BinaryHeap is a max-heap, so the Ord impl is inverted to
/// pop the smallest (series_hash, shard) first.
#[derive(PartialEq, Eq)]
struct HeapEntry {
    hash: i64,
    shard: usize,
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .hash
            .cmp(&self.hash)
            .then_with(|| other.shard.cmp(&self.shard))
    }
}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Build the global series_hash → (name, labels) map across all input shards.
/// A series's name/labels are identical wherever it appears, so we store them
/// once here instead of once per doc (12M docs would duplicate ~1 GB of
/// strings). Returns (map, total_docs, total_points).
fn load_series_metadata(
    input_shard_dirs: &[&Path],
) -> io::Result<(HashMap<i64, (String, String)>, usize, usize)> {
    let mut map: HashMap<i64, (String, String)> = HashMap::new();
    let mut total_docs = 0usize;
    let mut total_points = 0usize;

    for shard_dir in input_shard_dirs {
        let dir = FSDirectory::open(shard_dir)?;
        let mut reader = Reader::open(&dir)?;
        for (_doc_base, seg) in reader.leaves() {
            let hashes = seg.numeric_values("series_hash")?;
            let counts = seg.numeric_values("sample_count")?;
            let names = seg.sorted_values("metric_name")?;
            let labels = seg.binary_values("metric_labels")?;

            let count_map: HashMap<u32, i64> = counts.into_iter().collect();
            let name_map: HashMap<u32, Vec<u8>> = names.into_iter().collect();
            let label_map: HashMap<u32, Vec<u8>> = labels.into_iter().collect();

            for (doc_id, hash) in hashes {
                total_docs += 1;
                total_points += count_map.get(&doc_id).copied().unwrap_or(0) as usize;
                map.entry(hash).or_insert_with(|| {
                    let name = name_map
                        .get(&doc_id)
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                        .unwrap_or_default();
                    let labels = label_map
                        .get(&doc_id)
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                        .unwrap_or_default();
                    (name, labels)
                });
            }
        }
    }
    Ok((map, total_docs, total_points))
}

/// Read one shard's gorilla_data + series_hash + sample_count into a
/// series_hash-sorted cursor (encoded form; gorilla NOT decoded). The segment
/// is index-sorted by series_hash so doc order is hash order; we sort again
/// defensively (multi-segment shards / unsorted inputs). Returns (cursor,
/// doc_count).
fn load_shard_cursor(shard_dir: &Path) -> io::Result<(ShardCursor, usize)> {
    let dir = FSDirectory::open(shard_dir)?;
    let mut reader = Reader::open(&dir)?;

    let mut rows: Vec<(i64, usize, usize, usize)> = Vec::new(); // (hash, count, off, end)
    let mut gorilla: Vec<u8> = Vec::new();
    let mut base = 0usize;

    for (_doc_base, seg) in reader.leaves() {
        let hashes = seg.numeric_values("series_hash")?;
        let counts = seg.numeric_values("sample_count")?;
        let (doc_ids, data, offsets) = seg.binary_values_packed("gorilla_data")?;

        let hash_map: HashMap<u32, i64> = hashes.into_iter().collect();
        let count_map: HashMap<u32, i64> = counts.into_iter().collect();

        for (i, doc_id) in doc_ids.iter().enumerate() {
            let hash = hash_map.get(doc_id).copied().unwrap_or(0);
            let count = count_map.get(doc_id).copied().unwrap_or(0) as usize;
            let (s, e) = offsets[i];
            rows.push((hash, count, base + s, base + e));
        }
        gorilla.extend_from_slice(&data);
        base += data.len();
    }

    rows.sort_by_key(|r| r.0);
    let doc_count = rows.len();
    let hashes = rows.iter().map(|r| r.0).collect();
    let counts = rows.iter().map(|r| r.1).collect();
    let offsets = rows.iter().map(|r| (r.2 as u32, r.3 as u32)).collect();

    Ok((
        ShardCursor {
            hashes,
            counts,
            gorilla,
            offsets,
            pos: 0,
        },
        doc_count,
    ))
}

/// IndexWriterConfig that keeps output segments sorted by series_hash, so a
/// merge's output is itself a valid sorted input for any later merge. The RAM
/// buffer is capped small (64 MB) so the three merge writers flush often and
/// don't each hold hundreds of MB of buffered docs during a large merge.
fn sorted_v5_config() -> IndexWriterConfig {
    IndexWriterConfig {
        index_sort: Some(IndexSortField::new("series_hash")),
        max_ram_bytes: 64 * 1024 * 1024,
        ..IndexWriterConfig::default()
    }
}

/// Streaming k-way merge of multiple shards into one compact shard +
/// downsample shards.
///
/// Each input shard must be sorted by series_hash (written via
/// [`crate::runtime::buffer::SeriesBuffer::create_v5`], which sets index_sort).
/// We hold each shard's docs in encoded form and use a BinaryHeap min-heap to
/// walk the union in series_hash order, expanding gorilla points for only one
/// series at a time. For each unique series_hash we merge (sort + dedup) its
/// points across all shards, then write:
/// - compact output (v5_schema) with raw merged points
/// - downsample_5m output (downsample_schema) at 5-minute granularity
/// - downsample_1h output (downsample_schema) at 1-hour granularity
///
/// Peak decoded-point memory is O(points in one series × number of shards)
/// rather than the old O(total points across all shards).
pub fn merge_cross_shard(
    input_shard_dirs: &[&Path],
    output_compact_dir: &Path,
    output_downsample_5m_dir: &Path,
    output_downsample_1h_dir: &Path,
) -> io::Result<CrossMergeStats> {
    // 1. Global series_hash → (name, labels) map (stored once, not per doc);
    //    also gives input totals. Then load each shard's gorilla cursor and
    //    seed the heap with its first (smallest) series_hash.
    let (series_meta, input_docs, input_points) = load_series_metadata(input_shard_dirs)?;

    let mut cursors: Vec<ShardCursor> = Vec::with_capacity(input_shard_dirs.len());
    let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::new();

    for (shard_idx, shard_dir) in input_shard_dirs.iter().enumerate() {
        let (cursor, _doc_count) = load_shard_cursor(shard_dir)?;
        if let Some(&first) = cursor.hashes.first() {
            heap.push(HeapEntry {
                hash: first,
                shard: shard_idx,
            });
        }
        cursors.push(cursor);
    }

    // 2. Output writers (all index-sorted by series_hash).
    let mut compact_writer =
        IndexWriter::create(output_compact_dir, v5_schema(), sorted_v5_config())?;
    let mut ds5m_writer = IndexWriter::create(
        output_downsample_5m_dir,
        downsample_schema(),
        sorted_v5_config(),
    )?;
    let mut ds1h_writer = IndexWriter::create(
        output_downsample_1h_dir,
        downsample_schema(),
        sorted_v5_config(),
    )?;

    let mut output_docs = 0usize;
    let mut output_points = 0usize;
    let mut ds5m_docs = 0usize;
    let mut ds1h_docs = 0usize;

    // 3. K-way merge: repeatedly pop the smallest series_hash, gather every
    //    doc at that hash across all shards, merge + write one series.
    loop {
        // Short-lived peek to read the next smallest hash (borrow ends here so
        // the inner peek_mut loop can mutably borrow the heap).
        let current_hash = match heap.peek() {
            Some(top) => top.hash,
            None => break,
        };

        let mut points: Vec<(i64, f64)> = Vec::new();

        // Drain all cursors currently positioned at current_hash.
        while let Some(mut top) = heap.peek_mut() {
            if top.hash != current_hash {
                break;
            }
            let shard = top.shard;
            let cursor = &mut cursors[shard];
            // Consume the contiguous run of docs at current_hash in this shard.
            while cursor.pos < cursor.hashes.len() && cursor.hashes[cursor.pos] == current_hash {
                let (start, end) = cursor.offsets[cursor.pos];
                let count = cursor.counts[cursor.pos];
                let (times, values) =
                    gorilla::decode(&cursor.gorilla[start as usize..end as usize], count)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                points.extend(times.into_iter().zip(values.into_iter()));
                cursor.pos += 1;
            }
            // Advance this shard's heap entry to its next series_hash (or drop
            // it when exhausted). pop() re-sifts the heap correctly.
            if cursor.pos < cursor.hashes.len() {
                top.hash = cursor.hashes[cursor.pos];
            } else {
                std::collections::binary_heap::PeekMut::pop(top);
            }
        }

        // Sort by time, dedup (keep first occurrence).
        points.sort_by_key(|(t, _)| *t);
        points.dedup_by_key(|(t, _)| *t);
        output_points += points.len();

        if points.is_empty() {
            continue;
        }

        // name/labels live once in the global map (identical for a given hash).
        let (name, labels) = match series_meta.get(&current_hash) {
            Some((n, l)) => (n.as_str(), l.as_str()),
            None => ("", ""),
        };

        let sorted_labels = parse_labels_str(labels);
        let times: Vec<i64> = points.iter().map(|(t, _)| *t).collect();
        let values: Vec<f64> = points.iter().map(|(_, v)| *v).collect();

        // Compact (raw merged series).
        write_series(&mut compact_writer, name, &sorted_labels, &times, &values)?;
        output_docs += 1;

        // Downsample 5m.
        let ds5m_data = downsample(&points, INTERVAL_5M);
        write_downsample_doc(
            &mut ds5m_writer,
            name,
            labels,
            current_hash,
            &times,
            &ds5m_data,
        )?;
        ds5m_docs += 1;

        // Downsample 1h.
        let ds1h_data = downsample(&points, INTERVAL_1H);
        write_downsample_doc(
            &mut ds1h_writer,
            name,
            labels,
            current_hash,
            &times,
            &ds1h_data,
        )?;
        ds1h_docs += 1;
    }

    // 4. Commit all three writers.
    compact_writer.commit()?;
    ds5m_writer.commit()?;
    ds1h_writer.commit()?;

    Ok(CrossMergeStats {
        input_shards: input_shard_dirs.len(),
        input_docs,
        output_docs,
        input_points,
        output_points,
        downsample_5m_docs: ds5m_docs,
        downsample_1h_docs: ds1h_docs,
    })
}

/// Helper: write a downsample doc to a downsample IndexWriter.
fn write_downsample_doc(
    writer: &mut IndexWriter,
    name: &str,
    labels_str: &str,
    series_hash: i64,
    times: &[i64],
    ds_data: &[u8],
) -> io::Result<()> {
    let bucket_count = if ds_data.len() >= 12 {
        i32::from_le_bytes(ds_data[8..12].try_into().unwrap())
    } else {
        0
    };
    let ds_time_min = if ds_data.len() >= 8 {
        i64::from_le_bytes(ds_data[0..8].try_into().unwrap())
    } else {
        0
    };
    let ds_time_max = times.last().copied().unwrap_or(0);

    let mut doc = Document::new();
    doc.add("metric_name", FieldValue::Keyword(name.to_string()));
    doc.add("metric_labels", FieldValue::Text(labels_str.to_string()));
    doc.add("series_hash", FieldValue::Long(series_hash));
    doc.add("time_min", FieldValue::Long(ds_time_min));
    doc.add("time_max", FieldValue::Long(ds_time_max));
    doc.add("bucket_count", FieldValue::Long(bucket_count as i64));
    doc.add("downsample_data", FieldValue::Bytes(ds_data.to_vec()));
    writer.add_document(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::buffer::SeriesBuffer;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rustlucene-merge-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn test_merge_intra_shard_basic() {
        let input_dir = temp_dir("basic-in");
        let output_dir = temp_dir("basic-out");

        // Create input shard: series A (3 pts), flush, series A (2 pts), flush, series B (4 pts), commit
        let buf = SeriesBuffer::create_v5(&input_dir, IndexWriterConfig::default()).unwrap();
        let labels_a = vec![("host".to_string(), "h1".to_string())];
        let labels_b = vec![("host".to_string(), "h2".to_string())];

        // Series A first batch: 3 points
        buf.write_point("cpu.usage", &labels_a, 1000, 1.0);
        buf.write_point("cpu.usage", &labels_a, 2000, 2.0);
        buf.write_point("cpu.usage", &labels_a, 3000, 3.0);
        buf.flush().unwrap();

        // Series A second batch: 2 points
        buf.write_point("cpu.usage", &labels_a, 4000, 4.0);
        buf.write_point("cpu.usage", &labels_a, 5000, 5.0);
        buf.flush().unwrap();

        // Series B: 4 points
        buf.write_point("mem.free", &labels_b, 1000, 10.0);
        buf.write_point("mem.free", &labels_b, 2000, 20.0);
        buf.write_point("mem.free", &labels_b, 3000, 30.0);
        buf.write_point("mem.free", &labels_b, 4000, 40.0);
        buf.commit().unwrap();
        drop(buf);

        // Run merge
        let stats = merge_intra_shard(&input_dir, &output_dir).unwrap();

        assert_eq!(stats.input_docs, 3);
        assert_eq!(stats.output_docs, 2);
        assert_eq!(stats.input_points, 9);
        assert_eq!(stats.output_points, 9);
        assert_eq!(stats.merged_groups, 1); // only series A had >1 doc

        // Verify output shard contents
        let dir = FSDirectory::open(&output_dir).unwrap();
        let mut reader = Reader::open(&dir).unwrap();
        let mut total_docs = 0;
        for (_base, seg) in reader.leaves() {
            let counts = seg.numeric_values("sample_count").unwrap();
            let gorilla_datas = seg.binary_values("gorilla_data").unwrap();
            let names = seg.sorted_values("metric_name").unwrap();

            total_docs += seg.max_doc() as usize;

            let name_map: HashMap<u32, Vec<u8>> = names.into_iter().collect();
            let count_map: HashMap<u32, i64> = counts.into_iter().collect();

            for (doc_id, gorilla_bytes) in &gorilla_datas {
                let doc_id = *doc_id;
                let sample_count = count_map.get(&doc_id).copied().unwrap_or(0) as usize;
                let name = name_map
                    .get(&doc_id)
                    .map(|b| String::from_utf8_lossy(b).to_string())
                    .unwrap_or_default();

                let (times, values) = gorilla::decode(gorilla_bytes, sample_count).unwrap();

                if name == "cpu.usage" {
                    // Merged: 5 points
                    assert_eq!(times.len(), 5);
                    assert_eq!(times, vec![1000, 2000, 3000, 4000, 5000]);
                    for (i, v) in values.iter().enumerate() {
                        assert!((*v - (i + 1) as f64).abs() < 1e-15);
                    }
                } else if name == "mem.free" {
                    // Not merged: 4 points
                    assert_eq!(times.len(), 4);
                    assert_eq!(times, vec![1000, 2000, 3000, 4000]);
                } else {
                    panic!("unexpected metric_name: {name}");
                }
            }
        }
        assert_eq!(total_docs, 2);

        let _ = std::fs::remove_dir_all(&input_dir);
        let _ = std::fs::remove_dir_all(&output_dir);
    }

    #[test]
    fn test_merge_intra_shard_dedup() {
        let input_dir = temp_dir("dedup-in");
        let output_dir = temp_dir("dedup-out");

        let buf = SeriesBuffer::create_v5(&input_dir, IndexWriterConfig::default()).unwrap();
        let labels = vec![("job".to_string(), "test".to_string())];

        // First doc: times [1000, 2000, 3000]
        buf.write_point("requests", &labels, 1000, 1.0);
        buf.write_point("requests", &labels, 2000, 2.0);
        buf.write_point("requests", &labels, 3000, 3.0);
        buf.flush().unwrap();

        // Second doc: times [2000, 3000, 4000] (overlapping)
        buf.write_point("requests", &labels, 2000, 99.0);
        buf.write_point("requests", &labels, 3000, 99.0);
        buf.write_point("requests", &labels, 4000, 4.0);
        buf.commit().unwrap();
        drop(buf);

        let stats = merge_intra_shard(&input_dir, &output_dir).unwrap();

        assert_eq!(stats.input_docs, 2);
        assert_eq!(stats.output_docs, 1);
        assert_eq!(stats.input_points, 6);
        assert_eq!(stats.output_points, 4); // deduped: 1000,2000,3000,4000
        assert_eq!(stats.merged_groups, 1);

        // Verify dedup keeps first occurrence
        let dir = FSDirectory::open(&output_dir).unwrap();
        let mut reader = Reader::open(&dir).unwrap();
        for (_base, seg) in reader.leaves() {
            let gorilla_datas = seg.binary_values("gorilla_data").unwrap();
            let counts = seg.numeric_values("sample_count").unwrap();
            assert_eq!(gorilla_datas.len(), 1);

            let sample_count = counts[0].1 as usize;
            let (times, values) = gorilla::decode(&gorilla_datas[0].1, sample_count).unwrap();

            assert_eq!(times, vec![1000, 2000, 3000, 4000]);
            // First occurrence kept: t=2000 -> 2.0 (not 99.0), t=3000 -> 3.0 (not 99.0)
            assert!((values[0] - 1.0).abs() < 1e-15);
            assert!((values[1] - 2.0).abs() < 1e-15);
            assert!((values[2] - 3.0).abs() < 1e-15);
            assert!((values[3] - 4.0).abs() < 1e-15);
        }

        let _ = std::fs::remove_dir_all(&input_dir);
        let _ = std::fs::remove_dir_all(&output_dir);
    }

    #[test]
    fn test_merge_intra_shard_empty() {
        let input_dir = temp_dir("empty-in");
        let output_dir = temp_dir("empty-out");

        // Create an empty committed shard
        let schema = v5_schema();
        let mut writer =
            IndexWriter::create(&input_dir, schema, IndexWriterConfig::default()).unwrap();
        writer.commit().unwrap();
        drop(writer);

        let stats = merge_intra_shard(&input_dir, &output_dir).unwrap();

        assert_eq!(stats.input_docs, 0);
        assert_eq!(stats.output_docs, 0);
        assert_eq!(stats.input_points, 0);
        assert_eq!(stats.output_points, 0);
        assert_eq!(stats.merged_groups, 0);

        let _ = std::fs::remove_dir_all(&input_dir);
        let _ = std::fs::remove_dir_all(&output_dir);
    }

    #[test]
    fn test_merge_cross_shard_basic() {
        use crate::algo::downsample::decode_downsample;

        let shard1_dir = temp_dir("cross-s1");
        let shard2_dir = temp_dir("cross-s2");
        let compact_dir = temp_dir("cross-compact");
        let ds5m_dir = temp_dir("cross-ds5m");
        let ds1h_dir = temp_dir("cross-ds1h");

        let labels_a = vec![("host".to_string(), "h1".to_string())];
        let labels_b = vec![("host".to_string(), "h2".to_string())];
        let labels_c = vec![("host".to_string(), "h3".to_string())];

        // Shard 1: series A (3 points), series B (2 points)
        // Use timestamps spanning multiple 5m buckets for meaningful downsample
        let buf1 = SeriesBuffer::create_v5(&shard1_dir, IndexWriterConfig::default()).unwrap();
        buf1.write_point("cpu.usage", &labels_a, 0, 1.0);
        buf1.write_point("cpu.usage", &labels_a, 60_000, 2.0);
        buf1.write_point("cpu.usage", &labels_a, 300_000, 3.0);
        buf1.write_point("mem.free", &labels_b, 0, 10.0);
        buf1.write_point("mem.free", &labels_b, 600_000, 20.0);
        buf1.commit().unwrap();
        drop(buf1);

        // Shard 2: series A (2 more points, one overlapping at t=300_000), series C (4 points)
        let buf2 = SeriesBuffer::create_v5(&shard2_dir, IndexWriterConfig::default()).unwrap();
        buf2.write_point("cpu.usage", &labels_a, 300_000, 99.0); // overlap with shard1
        buf2.write_point("cpu.usage", &labels_a, 600_000, 4.0);
        buf2.write_point("disk.io", &labels_c, 0, 100.0);
        buf2.write_point("disk.io", &labels_c, 300_000, 200.0);
        buf2.write_point("disk.io", &labels_c, 600_000, 300.0);
        buf2.write_point("disk.io", &labels_c, 900_000, 400.0);
        buf2.commit().unwrap();
        drop(buf2);

        // Run cross-shard merge
        let stats = merge_cross_shard(
            &[&shard1_dir, &shard2_dir],
            &compact_dir,
            &ds5m_dir,
            &ds1h_dir,
        )
        .unwrap();

        // Verify stats
        assert_eq!(stats.input_shards, 2);
        assert_eq!(stats.input_docs, 4); // A, B in shard1; A, C in shard2
        assert_eq!(stats.output_docs, 3); // A merged, B, C
        assert_eq!(stats.input_points, 11); // 3 + 2 + 2 + 4
        // A: 3 + 2 = 5 points, dedup t=300_000 → 4 unique; B: 2; C: 4 → total 10
        assert_eq!(stats.output_points, 10);
        assert_eq!(stats.downsample_5m_docs, 3);
        assert_eq!(stats.downsample_1h_docs, 3);

        // Verify compact shard: 3 docs
        let dir = FSDirectory::open(&compact_dir).unwrap();
        let mut reader = Reader::open(&dir).unwrap();
        let mut compact_doc_count = 0;
        for (_base, seg) in reader.leaves() {
            let gorilla_datas = seg.binary_values("gorilla_data").unwrap();
            let counts = seg.numeric_values("sample_count").unwrap();
            let names = seg.sorted_values("metric_name").unwrap();

            compact_doc_count += seg.max_doc() as usize;
            let name_map: HashMap<u32, Vec<u8>> = names.into_iter().collect();
            let count_map: HashMap<u32, i64> = counts.into_iter().collect();

            for (doc_id, gorilla_bytes) in &gorilla_datas {
                let doc_id = *doc_id;
                let sample_count = count_map.get(&doc_id).copied().unwrap_or(0) as usize;
                let name = name_map
                    .get(&doc_id)
                    .map(|b| String::from_utf8_lossy(b).to_string())
                    .unwrap_or_default();
                let (times, _values) = gorilla::decode(gorilla_bytes, sample_count).unwrap();

                if name == "cpu.usage" {
                    // Merged + deduped: [0, 60000, 300000, 600000]
                    assert_eq!(times, vec![0, 60_000, 300_000, 600_000]);
                } else if name == "mem.free" {
                    assert_eq!(times, vec![0, 600_000]);
                } else if name == "disk.io" {
                    assert_eq!(times, vec![0, 300_000, 600_000, 900_000]);
                } else {
                    panic!("unexpected metric_name: {name}");
                }
            }
        }
        assert_eq!(compact_doc_count, 3);

        // Verify ds5m shard: 3 docs with downsample_data
        let dir5 = FSDirectory::open(&ds5m_dir).unwrap();
        let mut reader5 = Reader::open(&dir5).unwrap();
        let mut ds5m_doc_count = 0;
        for (_base, seg) in reader5.leaves() {
            let ds_datas = seg.binary_values("downsample_data").unwrap();
            let bucket_counts = seg.numeric_values("bucket_count").unwrap();
            ds5m_doc_count += seg.max_doc() as usize;

            for (i, (_doc_id, ds_bytes)) in ds_datas.iter().enumerate() {
                let bc = bucket_counts[i].1;
                assert!(bc > 0, "bucket_count should be > 0");
                let (_fbt, buckets) = decode_downsample(ds_bytes, INTERVAL_5M).unwrap();
                assert_eq!(buckets.len() as i64, bc);
            }
        }
        assert_eq!(ds5m_doc_count, 3);

        // Verify ds1h shard: 3 docs
        let dir1h = FSDirectory::open(&ds1h_dir).unwrap();
        let mut reader1h = Reader::open(&dir1h).unwrap();
        let mut ds1h_doc_count = 0;
        for (_base, seg) in reader1h.leaves() {
            let ds_datas = seg.binary_values("downsample_data").unwrap();
            let bucket_counts = seg.numeric_values("bucket_count").unwrap();
            ds1h_doc_count += seg.max_doc() as usize;

            for (i, (_doc_id, ds_bytes)) in ds_datas.iter().enumerate() {
                let bc = bucket_counts[i].1;
                assert!(bc > 0, "bucket_count should be > 0");
                let (_fbt, buckets) = decode_downsample(ds_bytes, INTERVAL_1H).unwrap();
                assert_eq!(buckets.len() as i64, bc);
            }
        }
        assert_eq!(ds1h_doc_count, 3);

        let _ = std::fs::remove_dir_all(&shard1_dir);
        let _ = std::fs::remove_dir_all(&shard2_dir);
        let _ = std::fs::remove_dir_all(&compact_dir);
        let _ = std::fs::remove_dir_all(&ds5m_dir);
        let _ = std::fs::remove_dir_all(&ds1h_dir);
    }

    #[test]
    fn test_merge_cross_shard_single_input() {
        use crate::algo::downsample::decode_downsample;

        let shard_dir = temp_dir("cross-single-in");
        let compact_dir = temp_dir("cross-single-compact");
        let ds5m_dir = temp_dir("cross-single-ds5m");
        let ds1h_dir = temp_dir("cross-single-ds1h");

        let labels = vec![("job".to_string(), "test".to_string())];

        // Single shard with 1 series, 5 points spanning multiple 5m buckets
        let buf = SeriesBuffer::create_v5(&shard_dir, IndexWriterConfig::default()).unwrap();
        buf.write_point("requests", &labels, 0, 1.0);
        buf.write_point("requests", &labels, 60_000, 2.0);
        buf.write_point("requests", &labels, 120_000, 3.0);
        buf.write_point("requests", &labels, 300_000, 4.0);
        buf.write_point("requests", &labels, 600_000, 5.0);
        buf.commit().unwrap();
        drop(buf);

        let stats = merge_cross_shard(
            &[&shard_dir],
            &compact_dir,
            &ds5m_dir,
            &ds1h_dir,
        )
        .unwrap();

        assert_eq!(stats.input_shards, 1);
        assert_eq!(stats.input_docs, 1);
        assert_eq!(stats.output_docs, 1);
        assert_eq!(stats.input_points, 5);
        assert_eq!(stats.output_points, 5);
        assert_eq!(stats.downsample_5m_docs, 1);
        assert_eq!(stats.downsample_1h_docs, 1);

        // Verify compact output has all 5 points
        let dir = FSDirectory::open(&compact_dir).unwrap();
        let mut reader = Reader::open(&dir).unwrap();
        for (_base, seg) in reader.leaves() {
            let gorilla_datas = seg.binary_values("gorilla_data").unwrap();
            let counts = seg.numeric_values("sample_count").unwrap();
            assert_eq!(gorilla_datas.len(), 1);
            let sample_count = counts[0].1 as usize;
            let (times, values) = gorilla::decode(&gorilla_datas[0].1, sample_count).unwrap();
            assert_eq!(times, vec![0, 60_000, 120_000, 300_000, 600_000]);
            for (i, v) in values.iter().enumerate() {
                assert!((*v - (i + 1) as f64).abs() < 1e-15);
            }
        }

        // Verify ds5m: points at 0,60000,120000 fall in bucket 0; 300000 in bucket 300000; 600000 in bucket 600000
        let dir5 = FSDirectory::open(&ds5m_dir).unwrap();
        let mut reader5 = Reader::open(&dir5).unwrap();
        for (_base, seg) in reader5.leaves() {
            let ds_datas = seg.binary_values("downsample_data").unwrap();
            assert_eq!(ds_datas.len(), 1);
            let (fbt, buckets) = decode_downsample(&ds_datas[0].1, INTERVAL_5M).unwrap();
            assert_eq!(fbt, 0);
            assert_eq!(buckets.len(), 3); // buckets at 0, 300000, 600000
        }

        // Verify ds1h: all points fall in bucket 0 (all < 3_600_000)
        let dir1h = FSDirectory::open(&ds1h_dir).unwrap();
        let mut reader1h = Reader::open(&dir1h).unwrap();
        for (_base, seg) in reader1h.leaves() {
            let ds_datas = seg.binary_values("downsample_data").unwrap();
            assert_eq!(ds_datas.len(), 1);
            let (fbt, buckets) = decode_downsample(&ds_datas[0].1, INTERVAL_1H).unwrap();
            assert_eq!(fbt, 0);
            assert_eq!(buckets.len(), 1); // all in one 1h bucket
        }

        let _ = std::fs::remove_dir_all(&shard_dir);
        let _ = std::fs::remove_dir_all(&compact_dir);
        let _ = std::fs::remove_dir_all(&ds5m_dir);
        let _ = std::fs::remove_dir_all(&ds1h_dir);
    }
}
