//! End-to-end integration tests for the metric storage stack.
//!
//! These exercise the FULL write → store → read path:
//! `SeriesBuffer` → `write_series` → `IndexWriter` commit → `Reader` reopen,
//! validating every V5 schema field that is reachable through the public
//! segment-reader API (`series_hash`, `time_min`, `time_max`, `sample_count`,
//! `gorilla_data`). `metric_name` is a sorted-DV field with no public reader
//! accessor, so it is covered via doc-count instead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use codec_lucene9::FSDirectory;
use rustlucene_core::index_writer::IndexWriterConfig;
use rustlucene_core::search::reader::Reader;

use rustlucene_metric::algo::gorilla;
use rustlucene_metric::algo::series_hash::series_hash;
use rustlucene_metric::runtime::buffer::SeriesBuffer;
use rustlucene_metric::store::schema::v5_schema;

/// Per-test unique temp dir: `<tmp>/rustlucene-metric-it-<tag>-<pid>`.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rustlucene-metric-it-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Convenience: build owned, key-sorted labels from string pairs.
fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// One stored series, gathered across all DV fields of a single doc.
#[derive(Debug)]
struct StoredSeries {
    hash: u64,
    time_min: i64,
    time_max: i64,
    sample_count: i64,
    gorilla: Vec<u8>,
}

/// Reopen the committed index at `root` and collect every doc's fields,
/// keyed by segment-local docID then flattened. Docs are correlated across
/// the per-field DV vectors by docID.
fn read_all_series(root: &Path) -> Vec<StoredSeries> {
    let dir = FSDirectory::open(root).unwrap();
    let mut reader = Reader::open(&dir).unwrap();
    let mut out = Vec::new();
    for (_base, seg) in reader.leaves() {
        let hashes = seg.numeric_values("series_hash").unwrap();
        let tmins = seg.numeric_values("time_min").unwrap();
        let tmaxs = seg.numeric_values("time_max").unwrap();
        let counts = seg.numeric_values("sample_count").unwrap();
        let bins = seg.binary_values("gorilla_data").unwrap();

        let n = seg.max_doc() as usize;
        // Every field is present on every doc, so each vector covers all docs.
        assert_eq!(hashes.len(), n, "series_hash DV should cover all docs");
        assert_eq!(tmins.len(), n, "time_min DV should cover all docs");
        assert_eq!(tmaxs.len(), n, "time_max DV should cover all docs");
        assert_eq!(counts.len(), n, "sample_count DV should cover all docs");
        assert_eq!(bins.len(), n, "gorilla_data DV should cover all docs");

        let mut by_doc: HashMap<u32, StoredSeries> = HashMap::new();
        for (doc, v) in hashes {
            by_doc.insert(
                doc,
                StoredSeries {
                    hash: v as u64,
                    time_min: 0,
                    time_max: 0,
                    sample_count: 0,
                    gorilla: Vec::new(),
                },
            );
        }
        for (doc, v) in tmins {
            by_doc.get_mut(&doc).unwrap().time_min = v;
        }
        for (doc, v) in tmaxs {
            by_doc.get_mut(&doc).unwrap().time_max = v;
        }
        for (doc, v) in counts {
            by_doc.get_mut(&doc).unwrap().sample_count = v;
        }
        for (doc, v) in bins {
            by_doc.get_mut(&doc).unwrap().gorilla = v;
        }

        let mut docs: Vec<u32> = by_doc.keys().copied().collect();
        docs.sort_unstable();
        for doc in docs {
            out.push(by_doc.remove(&doc).unwrap());
        }
    }
    out
}

/// Find the single stored series matching `hash`.
fn find_by_hash<'a>(series: &'a [StoredSeries], hash: u64) -> &'a StoredSeries {
    let matches: Vec<&StoredSeries> = series.iter().filter(|s| s.hash == hash).collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly 1 doc with hash {hash:#x}, found {}",
        matches.len()
    );
    matches[0]
}

/// Verify a stored series against expected identity/times/values, decoding
/// the Gorilla blob and requiring bit-exact f64 equality.
fn verify_series(
    stored: &StoredSeries,
    name: &str,
    lbls: &[(String, String)],
    times: &[i64],
    values: &[f64],
) {
    // series_hash: recomputed independently must match the stored numeric DV.
    let expected_hash = series_hash(name, lbls);
    assert_eq!(
        stored.hash, expected_hash,
        "series_hash mismatch for {name}"
    );

    // time_min / time_max
    let exp_min = *times.iter().min().unwrap();
    let exp_max = *times.iter().max().unwrap();
    assert_eq!(stored.time_min, exp_min, "time_min mismatch for {name}");
    assert_eq!(stored.time_max, exp_max, "time_max mismatch for {name}");

    // sample_count
    assert_eq!(
        stored.sample_count,
        times.len() as i64,
        "sample_count mismatch for {name}"
    );

    // gorilla_data: decode and require exact times + bit-exact values.
    let (dt, dv) = gorilla::decode(&stored.gorilla, times.len()).unwrap();
    assert_eq!(dt, times, "decoded times mismatch for {name}");
    assert_eq!(dv.len(), values.len(), "decoded value count for {name}");
    for (i, (a, b)) in dv.iter().zip(values.iter()).enumerate() {
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "value[{i}] not bit-exact for {name}: {a} vs {b}"
        );
    }
}

#[test]
fn test_full_write_read_verification() {
    // Schema sanity: V5 has exactly the 7 documented fields.
    let schema = v5_schema();
    let mut names: Vec<&str> = schema.fields().iter().map(|f| f.name.as_str()).collect();
    names.sort_unstable();
    let mut expected = [
        "gorilla_data",
        "metric_labels",
        "metric_name",
        "sample_count",
        "series_hash",
        "time_max",
        "time_min",
    ];
    expected.sort_unstable();
    assert_eq!(names, expected, "v5_schema must expose the 7 V5 fields");

    let root = temp_dir("full_write_read");
    let buf = SeriesBuffer::create_v5(&root, IndexWriterConfig::default()).unwrap();

    // Series 1: cpu.usage, 2 labels, 5 regular points.
    let l1 = labels(&[("host", "h1"), ("region", "us")]);
    let t1 = [1000i64, 2000, 3000, 4000, 5000];
    let v1 = [1.0f64, 2.5, 3.7, 2.1, 4.0];
    for (t, v) in t1.iter().zip(v1.iter()) {
        buf.write_point("cpu.usage", &l1, *t, *v);
    }

    // Series 2: mem.free, 1 label, 3 irregular points.
    let l2 = labels(&[("host", "h2")]);
    let t2 = [1000i64, 3000, 5000];
    let v2 = [1024.0f64, 2048.0, 512.0];
    for (t, v) in t2.iter().zip(v2.iter()) {
        buf.write_point("mem.free", &l2, *t, *v);
    }

    // Series 3: disk.io, no labels, single point.
    let l3: Vec<(String, String)> = Vec::new();
    buf.write_point("disk.io", &l3, 9999, 42.0);

    buf.commit().unwrap();
    drop(buf);

    let series = read_all_series(&root);
    // metric_name has no public sorted-DV reader; cover it via doc count.
    assert_eq!(series.len(), 3, "expected 3 committed series docs");

    verify_series(
        find_by_hash(&series, series_hash("cpu.usage", &l1)),
        "cpu.usage",
        &l1,
        &t1,
        &v1,
    );
    verify_series(
        find_by_hash(&series, series_hash("mem.free", &l2)),
        "mem.free",
        &l2,
        &t2,
        &v2,
    );
    verify_series(
        find_by_hash(&series, series_hash("disk.io", &l3)),
        "disk.io",
        &l3,
        &[9999i64],
        &[42.0f64],
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn test_gorilla_through_lucene_roundtrip() {
    let root = temp_dir("gorilla_roundtrip");
    let buf = SeriesBuffer::create_v5(&root, IndexWriterConfig::default()).unwrap();

    let lbl = labels(&[("edge", "tricky")]);
    // Irregular intervals to exercise every DoD branch. Deltas:
    //   1000, 1000, 2000, 500, 94500, 500, 9_899_500, 1000
    // so delta0=1000 goes through the varint path and the DoD sequence is:
    //   0 ('0'), +1000 (14b), -1500 (14b), +94000 (20b), -94000 (20b),
    //   +9_899_000 (64b), -9_898_500 (64b).
    let times = [
        1000i64, 2000, 3000, 5000, 5500, 100_000, 100_500, 10_000_000, 10_001_000,
    ];
    // Tricky payloads: NaN, ±inf, -0.0, +0.0, extremes, subnormal, then a
    // repeated value to hit the XOR==0 path.
    let values = [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -0.0,
        0.0,
        f64::MAX,
        f64::MIN_POSITIVE / 2.0, // subnormal
        f64::MIN_POSITIVE / 2.0, // identical to previous → XOR == 0
        1.5e308,
    ];
    assert_eq!(times.len(), values.len());

    for (t, v) in times.iter().zip(values.iter()) {
        buf.write_point("edge.case", &lbl, *t, *v);
    }
    buf.commit().unwrap();
    drop(buf);

    let series = read_all_series(&root);
    assert_eq!(series.len(), 1);
    let s = &series[0];

    assert_eq!(s.hash, series_hash("edge.case", &lbl));
    assert_eq!(s.time_min, 1000);
    assert_eq!(s.time_max, 10_001_000);
    assert_eq!(s.sample_count, times.len() as i64);

    let (dt, dv) = gorilla::decode(&s.gorilla, times.len()).unwrap();
    assert_eq!(dt, times, "times must survive the Lucene roundtrip exactly");
    assert_eq!(dv.len(), values.len());
    for (i, (a, b)) in dv.iter().zip(values.iter()).enumerate() {
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "value[{i}] not bit-exact through storage: {a:?} vs {b:?}"
        );
    }
    // Explicitly prove the nasty cases are preserved bit-for-bit.
    assert!(dv[0].is_nan());
    assert_eq!(dv[1], f64::INFINITY);
    assert_eq!(dv[2], f64::NEG_INFINITY);
    assert_eq!(dv[3].to_bits(), (-0.0f64).to_bits()); // negative zero sign bit kept
    assert_eq!(dv[4].to_bits(), 0.0f64.to_bits());
    assert_ne!(dv[3].to_bits(), dv[4].to_bits(), "-0.0 and +0.0 differ by bits");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn test_series_hash_stability() {
    let root = temp_dir("hash_stability");
    let buf = SeriesBuffer::create_v5(&root, IndexWriterConfig::default()).unwrap();

    let same = labels(&[("host", "h1")]);
    let other = labels(&[("host", "h2")]);

    // Same name+labels, first batch of points, flushed (not committed).
    buf.write_point("cpu.usage", &same, 1000, 1.0);
    buf.write_point("cpu.usage", &same, 2000, 2.0);
    buf.flush().unwrap();

    // Same name+labels, second batch → a distinct doc with the SAME hash.
    buf.write_point("cpu.usage", &same, 3000, 3.0);
    buf.flush().unwrap();

    // Same name, different labels → different hash.
    buf.write_point("cpu.usage", &other, 1000, 9.0);
    buf.commit().unwrap();
    drop(buf);

    let series = read_all_series(&root);
    assert_eq!(series.len(), 3, "two same-hash docs + one different-label doc");

    let hash_same = series_hash("cpu.usage", &same);
    let hash_other = series_hash("cpu.usage", &other);
    assert_ne!(
        hash_same, hash_other,
        "different labels must yield different series_hash"
    );

    let same_docs: Vec<&StoredSeries> = series.iter().filter(|s| s.hash == hash_same).collect();
    assert_eq!(
        same_docs.len(),
        2,
        "same name+labels written in two flushes → 2 docs sharing one hash"
    );
    let other_docs: Vec<&StoredSeries> = series.iter().filter(|s| s.hash == hash_other).collect();
    assert_eq!(other_docs.len(), 1);

    // Each stored hash must equal the independently computed series_hash, and
    // the two same-hash docs must carry the correct per-batch point sets.
    let mut counts: Vec<i64> = same_docs.iter().map(|s| s.sample_count).collect();
    counts.sort_unstable();
    assert_eq!(counts, vec![1, 2], "batches had 2 and 1 points respectively");

    for s in &same_docs {
        assert_eq!(s.hash, hash_same);
        let (dt, dv) = gorilla::decode(&s.gorilla, s.sample_count as usize).unwrap();
        assert_eq!(dt.len(), s.sample_count as usize);
        assert_eq!(dv.len(), s.sample_count as usize);
        if s.sample_count == 2 {
            assert_eq!(dt, vec![1000, 2000]);
        } else {
            assert_eq!(dt, vec![3000]);
        }
    }
    assert_eq!(other_docs[0].hash, hash_other);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn test_multiple_flushes_accumulate() {
    let root = temp_dir("multi_flush");
    let buf = SeriesBuffer::create_v5(&root, IndexWriterConfig::default()).unwrap();

    let la = labels(&[("svc", "a")]);
    let lb = labels(&[("svc", "b")]);

    // Flush #1 (no commit): series A, two points.
    buf.write_point("req.count", &la, 1000, 10.0);
    buf.write_point("req.count", &la, 2000, 20.0);
    buf.flush().unwrap();

    // Flush #2 (no commit): series B, one point.
    buf.write_point("req.count", &lb, 1500, 99.0);
    buf.flush().unwrap();

    // Before commit there is no segments_N, so the index is not yet readable —
    // proving flush() buffers docs without committing prematurely.
    {
        let dir = FSDirectory::open(&root).unwrap();
        assert!(
            Reader::open(&dir).is_err(),
            "reader must not open before commit (flush is not a commit)"
        );
    }

    buf.commit().unwrap();
    drop(buf);

    // After commit, both flushes contributed docs.
    let series = read_all_series(&root);
    assert_eq!(series.len(), 2, "both flushed series must be committed");

    verify_series(
        find_by_hash(&series, series_hash("req.count", &la)),
        "req.count",
        &la,
        &[1000i64, 2000],
        &[10.0f64, 20.0],
    );
    verify_series(
        find_by_hash(&series, series_hash("req.count", &lb)),
        "req.count",
        &lb,
        &[1500i64],
        &[99.0f64],
    );

    let _ = std::fs::remove_dir_all(&root);
}
