//! Intra-shard merge operation: merges multiple docs of the same series
//! within a single shard into one doc (sort + dedup + re-encode).
//! See design doc §6.2.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use codec_lucene9::FSDirectory;
use rustlucene_core::index_writer::{IndexWriter, IndexWriterConfig};
use rustlucene_core::search::reader::Reader;

use crate::algo::gorilla;
use crate::store::schema::v5_schema;
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
}
