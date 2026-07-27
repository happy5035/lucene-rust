use std::io;

use rustlucene_core::document::{Document, FieldValue};
use rustlucene_core::index_writer::IndexWriter;

use crate::algo::gorilla;
use crate::algo::series_hash::{build_labels_str, series_hash};

/// 将一个 series 写入 IndexWriter（1 个 doc = 1 个 series 的全部时间点）。
/// 见 V5 Format Reference R3。
pub fn write_series(
    writer: &mut IndexWriter,
    name: &str,
    sorted_labels: &[(String, String)],
    times: &[i64],
    values: &[f64],
) -> io::Result<()> {
    let labels_str = build_labels_str(sorted_labels);
    let hash = series_hash(name, sorted_labels);
    let gorilla_data = gorilla::encode(times, values);
    let time_min = *times.iter().min().unwrap();
    let time_max = *times.iter().max().unwrap();
    let sample_count = times.len() as i64;

    let mut doc = Document::new();
    doc.add("metric_name", FieldValue::Keyword(name.to_string()));
    doc.add("metric_labels", FieldValue::Text(labels_str));
    doc.add("series_hash", FieldValue::Long(hash as i64));
    doc.add("time_min", FieldValue::Long(time_min));
    doc.add("time_max", FieldValue::Long(time_max));
    doc.add("sample_count", FieldValue::Long(sample_count));
    doc.add("gorilla_data", FieldValue::Bytes(gorilla_data));
    writer.add_document(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::schema::v5_schema;
    use codec_lucene9::FSDirectory;
    use rustlucene_core::index_writer::IndexWriterConfig;
    use rustlucene_core::search::reader::Reader;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("rustlucene-metric-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn test_write_series_and_read_back() {
        let root = temp_dir("write_series");
        let schema = v5_schema();
        let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();

        let times = vec![1000i64, 2000, 3000];
        let values = vec![1.0f64, 2.0, 3.0];
        let labels = vec![("host".to_string(), "h1".to_string())];

        write_series(&mut w, "cpu.usage", &labels, &times, &values).unwrap();
        w.commit().unwrap();
        drop(w);

        // 读回验证
        let dir = FSDirectory::open(&root).unwrap();
        let mut reader = Reader::open(&dir).unwrap();
        for (_base, seg) in reader.leaves() {
            // 读 series_hash
            let nums = seg.numeric_values("series_hash").unwrap();
            assert_eq!(nums.len(), 1);
            let expected_hash = series_hash("cpu.usage", &labels);
            assert_eq!(nums[0].1 as u64, expected_hash);

            // 读 sample_count
            let counts = seg.numeric_values("sample_count").unwrap();
            assert_eq!(counts[0].1, 3);

            // 读 gorilla_data 并解码
            let bins = seg.binary_values("gorilla_data").unwrap();
            assert_eq!(bins.len(), 1);
            let (t, v) = gorilla::decode(&bins[0].1, 3).unwrap();
            assert_eq!(t, vec![1000, 2000, 3000]);
            for (a, b) in v.iter().zip(values.iter()) {
                assert!((a - b).abs() < 1e-15);
            }
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
