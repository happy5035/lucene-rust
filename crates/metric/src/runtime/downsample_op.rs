//! Downsample shard operation: reads a raw V5 shard, decodes gorilla data
//! per doc, applies downsample aggregation, and writes to an output shard
//! with the downsample schema.

use std::io;
use std::path::Path;

use codec_lucene9::FSDirectory;
use rustlucene_core::document::{Document, FieldValue};
use rustlucene_core::index_writer::{IndexWriter, IndexWriterConfig};
use rustlucene_core::search::reader::Reader;

use crate::algo::downsample::downsample;
use crate::algo::gorilla;
use crate::store::schema::downsample_schema;

/// Statistics from a downsample_shard operation.
pub struct DownsampleStats {
    pub input_docs: usize,
    pub output_docs: usize,
    pub input_points: usize,
}

/// Read a raw shard, decode gorilla data per doc, apply downsample,
/// write results to output shard directory.
///
/// `granularity_ms` is the downsample interval (e.g. 300_000 for 5m).
///
/// Known limitation: metric_labels is written as empty string because
/// the V5 schema stores labels as indexed text (no DV), making them
/// unreadable at merge/downsample time. The downsample shard's primary
/// use is query-time aggregation (series_hash + time range + downsample_data).
pub fn downsample_shard(
    input_raw_dir: &Path,
    output_downsample_dir: &Path,
    granularity_ms: i64,
) -> io::Result<DownsampleStats> {
    // 1. Open input shard with Reader
    let in_dir = FSDirectory::open(input_raw_dir)?;
    let mut reader = Reader::open(&in_dir)?;

    // 2. Create output IndexWriter with downsample_schema
    let schema = downsample_schema();
    let mut writer = IndexWriter::create(output_downsample_dir, schema, IndexWriterConfig::default())?;

    let mut input_docs = 0usize;
    let mut input_points = 0usize;
    let mut output_docs = 0usize;

    // 3. Iterate all docs in input shard
    for (_doc_base, seg) in reader.leaves() {
        let hashes = seg.numeric_values("series_hash")?;
        let time_mins = seg.numeric_values("time_min")?;
        let time_maxs = seg.numeric_values("time_max")?;
        let counts = seg.numeric_values("sample_count")?;
        let gorilla_datas = seg.binary_values("gorilla_data")?;
        let metric_names = seg.sorted_values("metric_name")?;

        input_docs += seg.max_doc() as usize;

        // Build lookup maps for sparse doc-value access
        let hash_map: std::collections::HashMap<u32, i64> = hashes.into_iter().collect();
        let tmin_map: std::collections::HashMap<u32, i64> = time_mins.into_iter().collect();
        let tmax_map: std::collections::HashMap<u32, i64> = time_maxs.into_iter().collect();
        let count_map: std::collections::HashMap<u32, i64> = counts.into_iter().collect();
        let name_map: std::collections::HashMap<u32, Vec<u8>> = metric_names.into_iter().collect();

        for (doc_id, gorilla_bytes) in &gorilla_datas {
            let doc_id = *doc_id;
            let sample_count = count_map.get(&doc_id).copied().unwrap_or(0) as usize;
            let series_hash = hash_map.get(&doc_id).copied().unwrap_or(0);

            // Decode gorilla
            let (times, values) = gorilla::decode(gorilla_bytes, sample_count)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

            input_points += times.len();

            // Downsample
            let points: Vec<(i64, f64)> = times.into_iter().zip(values.into_iter()).collect();
            let ds_data = downsample(&points, granularity_ms);

            // Extract bucket_count and first_bucket_time from ds_data header
            let bucket_count = if ds_data.len() >= 12 {
                i32::from_le_bytes(ds_data[8..12].try_into().unwrap())
            } else {
                0
            };
            let ds_time_min = if ds_data.len() >= 8 {
                i64::from_le_bytes(ds_data[0..8].try_into().unwrap())
            } else {
                tmin_map.get(&doc_id).copied().unwrap_or(0)
            };
            let ds_time_max = if bucket_count > 0 {
                ds_time_min + (bucket_count as i64 - 1) * granularity_ms
            } else {
                tmax_map.get(&doc_id).copied().unwrap_or(0)
            };

            // Read metric_name from sorted DV
            let metric_name = name_map
                .get(&doc_id)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();

            // Write output doc
            let mut doc = Document::new();
            doc.add("metric_name", FieldValue::Keyword(metric_name));
            // Known limitation: metric_labels not recoverable from input shard
            doc.add("metric_labels", FieldValue::Text(String::new()));
            doc.add("series_hash", FieldValue::Long(series_hash));
            doc.add("time_min", FieldValue::Long(ds_time_min));
            doc.add("time_max", FieldValue::Long(ds_time_max));
            doc.add("bucket_count", FieldValue::Long(bucket_count as i64));
            doc.add("downsample_data", FieldValue::Bytes(ds_data));

            writer.add_document(doc)?;
            output_docs += 1;
        }
    }

    writer.commit()?;

    Ok(DownsampleStats {
        input_docs,
        output_docs,
        input_points,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::downsample::{decode_downsample, COL_COUNT, COL_SUM};
    use crate::runtime::buffer::SeriesBuffer;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rustlucene-dsop-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn test_downsample_shard_basic() {
        let raw_dir = temp_dir("raw");
        let ds_dir = temp_dir("ds");

        // Create a raw shard with 2 series, 10 points each spanning multiple 5m buckets
        let buf = SeriesBuffer::create_v5(&raw_dir, IndexWriterConfig::default()).unwrap();

        let labels1 = vec![("host".to_string(), "h1".to_string())];
        let labels2 = vec![("host".to_string(), "h2".to_string())];

        // Series 1: 10 points across 3 buckets (5m intervals starting at 300_000)
        for i in 0..10i64 {
            let t = 300_000 + i * 60_000; // 60s apart → spans 300000..840000
            buf.write_point("cpu.usage", &labels1, t, (i + 1) as f64);
        }
        // Series 2: 10 points across 3 buckets
        for i in 0..10i64 {
            let t = 300_000 + i * 60_000;
            buf.write_point("mem.free", &labels2, t, (i + 1) as f64 * 10.0);
        }

        buf.commit().unwrap();
        drop(buf);

        // Run downsample
        let stats = downsample_shard(&raw_dir, &ds_dir, 300_000).unwrap();
        assert_eq!(stats.input_docs, 2);
        assert_eq!(stats.output_docs, 2);
        assert_eq!(stats.input_points, 20);

        // Verify output shard
        let dir = FSDirectory::open(&ds_dir).unwrap();
        let mut reader = Reader::open(&dir).unwrap();
        let mut doc_count = 0;
        for (_base, seg) in reader.leaves() {
            let bucket_counts = seg.numeric_values("bucket_count").unwrap();
            let ds_datas = seg.binary_values("downsample_data").unwrap();
            let names = seg.sorted_values("metric_name").unwrap();

            assert_eq!(bucket_counts.len(), 2);
            assert_eq!(ds_datas.len(), 2);
            assert_eq!(names.len(), 2);

            for (i, (doc_id, ds_bytes)) in ds_datas.iter().enumerate() {
                let bc = bucket_counts[i].1;
                assert!(bc > 0, "bucket_count should be > 0");

                // Decode and verify downsample data
                let (first_bt, buckets) = decode_downsample(ds_bytes, 300_000).unwrap();
                assert_eq!(first_bt, 300_000);
                assert_eq!(buckets.len() as i64, bc);

                // Verify total count across buckets = 10 points
                let total_count: f64 = buckets.iter().map(|(_, cols)| cols[COL_COUNT]).sum();
                assert!((total_count - 10.0).abs() < 1e-15);

                // Verify metric_name is preserved
                let name = String::from_utf8_lossy(&names[i].1).to_string();
                assert!(
                    name == "cpu.usage" || name == "mem.free",
                    "unexpected metric_name: {name}"
                );

                let _ = doc_id;
            }
            doc_count += seg.max_doc() as usize;
        }
        assert_eq!(doc_count, 2);

        let _ = std::fs::remove_dir_all(&raw_dir);
        let _ = std::fs::remove_dir_all(&ds_dir);
    }

    #[test]
    fn test_downsample_shard_verifies_sum() {
        let raw_dir = temp_dir("raw_sum");
        let ds_dir = temp_dir("ds_sum");

        let buf = SeriesBuffer::create_v5(&raw_dir, IndexWriterConfig::default()).unwrap();
        let labels = vec![("job".to_string(), "test".to_string())];

        // All 5 points in one bucket [300_000, 600_000)
        for i in 0..5i64 {
            buf.write_point("requests", &labels, 300_000 + i * 1000, (i + 1) as f64);
        }
        buf.commit().unwrap();
        drop(buf);

        let stats = downsample_shard(&raw_dir, &ds_dir, 300_000).unwrap();
        assert_eq!(stats.input_docs, 1);
        assert_eq!(stats.output_docs, 1);
        assert_eq!(stats.input_points, 5);

        // Verify sum = 1+2+3+4+5 = 15
        let dir = FSDirectory::open(&ds_dir).unwrap();
        let mut reader = Reader::open(&dir).unwrap();
        for (_base, seg) in reader.leaves() {
            let ds_datas = seg.binary_values("downsample_data").unwrap();
            assert_eq!(ds_datas.len(), 1);
            let (_fbt, buckets) = decode_downsample(&ds_datas[0].1, 300_000).unwrap();
            assert_eq!(buckets.len(), 1);
            let cols = &buckets[0].1;
            assert!((cols[COL_COUNT] - 5.0).abs() < 1e-15);
            assert!((cols[COL_SUM] - 15.0).abs() < 1e-15);
        }

        let _ = std::fs::remove_dir_all(&raw_dir);
        let _ = std::fs::remove_dir_all(&ds_dir);
    }
}
