//! Write throughput benchmark: realistic metric workloads.
//!
//! Models production metric systems where:
//! - Each series writes 1 point every ~15s (scrape interval)
//! - Buffer accumulates multiple cycles before flushing
//! - Aggregation benefit: N points per series → 1 doc (gorilla-encoded)
//!   vs log-style: N points → N docs
//!
//! Scenarios:
//!   1. Metric buffered (100K series × 4 pts = 400K points → 100K docs)
//!   2. Log-style (400K points → 400K docs)
//!   3. Metric buffered (100K series × 16 pts = 1.6M points → 100K docs)
//!   4. High-cardinality stress (500K series × 4 pts = 2M points → 500K docs)
//!   5. Pure write_point throughput (no IO, 1M calls across 100K series)

use std::time::Instant;

use rustlucene_core::document::{Document, FieldValue};
use rustlucene_core::index_writer::{IndexWriter, IndexWriterConfig};
use rustlucene_metric::algo::gorilla;
use rustlucene_metric::algo::series_hash::{build_labels_str, series_hash};
use rustlucene_metric::runtime::buffer::SeriesBuffer;
use rustlucene_metric::store::schema::v5_schema;

const SCRAPE_INTERVAL_MS: i64 = 15_000;
const BASE_TIME: i64 = 1_700_000_000_000;

const METRIC_NAMES_2: [&str; 2] = ["cpu.usage", "mem.free"];
const METRIC_NAMES_10: [&str; 10] = [
    "cpu.usage", "cpu.system", "mem.free", "mem.used", "disk.io",
    "disk.read", "disk.write", "net.rx", "net.tx", "net.errors",
];

const NUM_HOSTS: usize = 1000;
const NUM_REGIONS: usize = 10;
const NUM_DCS: usize = 5;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rustlucene-bench-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn format_number(n: usize) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (idx, ch) in s.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result.chars().rev().collect()
}

fn format_number_u128(n: u128) -> String {
    format_number(n as usize)
}

/// Pre-generated series identity: (name, sorted_labels).
/// Labels are sorted by key: dc, host, region (alphabetical).
struct SeriesIdentity {
    name: String,
    labels: Vec<(String, String)>,
}

/// Generate `count` unique series identities from the cartesian product of
/// metric names × hosts × regions × dcs.
fn generate_series(count: usize, metric_names: &[&str]) -> Vec<SeriesIdentity> {
    let mut series = Vec::with_capacity(count);
    'outer: for name in metric_names {
        for h in 0..NUM_HOSTS {
            for r in 0..NUM_REGIONS {
                for d in 0..NUM_DCS {
                    if series.len() >= count {
                        break 'outer;
                    }
                    let labels = vec![
                        ("dc".to_string(), format!("d{}", d)),
                        ("host".to_string(), format!("h{}", h)),
                        ("region".to_string(), format!("r{}", r)),
                    ];
                    series.push(SeriesIdentity {
                        name: name.to_string(),
                        labels,
                    });
                }
            }
        }
    }
    series
}

/// Deterministic value for series `i`, cycle `c`.
fn point_value(i: usize, c: usize) -> f64 {
    ((i * 7 + c * 13) % 10000) as f64 * 0.01
}

/// Time for a given scrape cycle.
fn cycle_time(cycle: usize) -> i64 {
    BASE_TIME + (cycle as i64) * SCRAPE_INTERVAL_MS
}

/// Scenario 1/3/4: Metric buffered write (SeriesBuffer → 1 doc per series).
/// Returns (elapsed_ms, total_points, docs_written).
fn bench_metric_buffered(
    tag: &str,
    series: &[SeriesIdentity],
    cycles: usize,
) -> (u128, usize, usize) {
    let root = temp_dir(tag);
    let buf = SeriesBuffer::create_v5(&root, IndexWriterConfig::default()).unwrap();

    let start = Instant::now();

    for cycle in 0..cycles {
        let t = cycle_time(cycle);
        for (i, s) in series.iter().enumerate() {
            buf.write_point(&s.name, &s.labels, t, point_value(i, cycle));
        }
    }
    buf.commit().unwrap();

    let elapsed = start.elapsed().as_millis();
    let total_points = series.len() * cycles;
    let docs = series.len();
    drop(buf);
    let _ = std::fs::remove_dir_all(&root);

    (elapsed, total_points, docs)
}

/// Scenario 2: Log-style write (1 doc per point via IndexWriter).
/// Returns (elapsed_ms, total_points, docs_written).
fn bench_log_style(
    tag: &str,
    series: &[SeriesIdentity],
    cycles: usize,
) -> (u128, usize, usize) {
    let root = temp_dir(tag);
    let schema = v5_schema();
    let config = IndexWriterConfig::default();
    let mut writer = IndexWriter::create(&root, schema, config).unwrap();

    let start = Instant::now();

    for cycle in 0..cycles {
        let t = cycle_time(cycle);
        for (i, s) in series.iter().enumerate() {
            let v = point_value(i, cycle);
            let labels_str = build_labels_str(&s.labels);
            let hash = series_hash(&s.name, &s.labels);
            let gorilla_data = gorilla::encode(&[t], &[v]);

            let mut doc = Document::new();
            doc.add("metric_name", FieldValue::Keyword(s.name.clone()));
            doc.add("metric_labels", FieldValue::Text(labels_str));
            doc.add("series_hash", FieldValue::Long(hash as i64));
            doc.add("time_min", FieldValue::Long(t));
            doc.add("time_max", FieldValue::Long(t));
            doc.add("sample_count", FieldValue::Long(1));
            doc.add("gorilla_data", FieldValue::Bytes(gorilla_data));
            writer.add_document(doc).unwrap();
        }
    }
    writer.commit().unwrap();

    let elapsed = start.elapsed().as_millis();
    let total_points = series.len() * cycles;
    let docs = total_points; // 1 doc per point
    drop(writer);
    let _ = std::fs::remove_dir_all(&root);

    (elapsed, total_points, docs)
}

/// Scenario 5: Pure write_point throughput (no IO).
/// Measures only the in-memory buffering cost (HashMap insert + Vec push).
/// Returns (elapsed_ms, total_calls).
fn bench_pure_write_point(series: &[SeriesIdentity], total_calls: usize) -> (u128, usize) {
    let root = temp_dir("pure_wp");
    let buf = SeriesBuffer::create_v5(&root, IndexWriterConfig::default()).unwrap();

    let cycles = total_calls / series.len();
    let start = Instant::now();

    for cycle in 0..cycles {
        let t = cycle_time(cycle);
        for (i, s) in series.iter().enumerate() {
            buf.write_point(&s.name, &s.labels, t, point_value(i, cycle));
        }
    }

    let elapsed = start.elapsed().as_millis();
    let calls = series.len() * cycles;
    drop(buf);
    let _ = std::fs::remove_dir_all(&root);

    (elapsed, calls)
}

/// Scenario 6: Pure write_points batch throughput (no IO).
/// Each call writes `batch_size` points for one series in a single lock acquisition.
/// Models: 1M series × 20 pts via 1M batch calls vs 20M single calls.
/// Returns (elapsed_ms, total_points, batch_calls).
fn bench_pure_write_points_batch(series: &[SeriesIdentity], batch_size: usize) -> (u128, usize, usize) {
    let root = temp_dir("pure_wp_batch");
    let buf = SeriesBuffer::create_v5(&root, IndexWriterConfig::default()).unwrap();

    let times: Vec<i64> = (0..batch_size).map(|c| cycle_time(c)).collect();

    let start = Instant::now();

    for (i, s) in series.iter().enumerate() {
        let values: Vec<f64> = (0..batch_size).map(|c| point_value(i, c)).collect();
        buf.write_points(&s.name, &s.labels, &times, &values);
    }

    let elapsed = start.elapsed().as_millis();
    let total_points = series.len() * batch_size;
    let batch_calls = series.len();
    drop(buf);
    let _ = std::fs::remove_dir_all(&root);

    (elapsed, total_points, batch_calls)
}

/// Compute average gorilla-encoded size for a doc with `n` points.
fn avg_gorilla_size(n: usize) -> usize {
    // Encode a representative series with n points at 15s intervals
    let times: Vec<i64> = (0..n).map(|c| BASE_TIME + (c as i64) * SCRAPE_INTERVAL_MS).collect();
    let values: Vec<f64> = (0..n).map(|c| point_value(42, c)).collect();
    gorilla::encode(&times, &values).len()
}

fn main() {
    println!("=== Metric Write Throughput (Realistic Workload) ===");
    println!(
        "Scrape interval: {}s | Label space: {} hosts x {} regions x {} dcs",
        SCRAPE_INTERVAL_MS / 1000,
        NUM_HOSTS,
        NUM_REGIONS,
        NUM_DCS
    );
    println!();

    // Pre-generate series identities (outside timer)
    println!("Pre-generating series identities...");
    let gen_start = Instant::now();
    let series_100k = generate_series(100_000, &METRIC_NAMES_2);
    let series_500k = generate_series(500_000, &METRIC_NAMES_10);
    let gen_elapsed = gen_start.elapsed();
    println!(
        "  Generated {} + {} series in {:.1}s",
        format_number(series_100k.len()),
        format_number(series_500k.len()),
        gen_elapsed.as_secs_f64()
    );
    println!();

    // --- Scenario 1: Metric buffered (100K series × 4 pts) ---
    let s1_cycles = 4;
    let s1_points = series_100k.len() * s1_cycles;
    let s1_docs = series_100k.len();
    println!(
        "Scenario 1: Metric buffered ({} series x {} pts = {} points -> {} docs)",
        format_number(series_100k.len()),
        s1_cycles,
        format_number(s1_points),
        format_number(s1_docs)
    );
    let (time1, points1, docs1) = bench_metric_buffered("s1", &series_100k, s1_cycles);
    let tp1 = if time1 > 0 { (points1 as u128 * 1000) / time1 } else { u128::MAX };
    println!("  Time: {} ms", format_number_u128(time1));
    println!("  Write throughput: {} points/sec", format_number_u128(tp1));
    println!("  Docs: {} | Points/doc: {}", format_number(docs1), s1_cycles);
    println!();

    // --- Scenario 2: Log-style (same 400K points, 1 doc each) ---
    let s2_cycles = 4;
    let s2_points = series_100k.len() * s2_cycles;
    println!(
        "Scenario 2: Log-style ({} points -> {} docs)",
        format_number(s2_points),
        format_number(s2_points)
    );
    let (time2, points2, docs2) = bench_log_style("s2", &series_100k, s2_cycles);
    let tp2 = if time2 > 0 { (points2 as u128 * 1000) / time2 } else { u128::MAX };
    println!("  Time: {} ms", format_number_u128(time2));
    println!("  Write throughput: {} points/sec", format_number_u128(tp2));
    println!("  Docs: {} | Points/doc: 1", format_number(docs2));
    println!();

    // --- Scenario 3: Metric buffered (100K series × 16 pts) ---
    let s3_cycles = 16;
    let s3_points = series_100k.len() * s3_cycles;
    let s3_docs = series_100k.len();
    println!(
        "Scenario 3: Metric buffered ({} series x {} pts = {} points -> {} docs)",
        format_number(series_100k.len()),
        s3_cycles,
        format_number(s3_points),
        format_number(s3_docs)
    );
    let (time3, points3, docs3) = bench_metric_buffered("s3", &series_100k, s3_cycles);
    let tp3 = if time3 > 0 { (points3 as u128 * 1000) / time3 } else { u128::MAX };
    println!("  Time: {} ms", format_number_u128(time3));
    println!("  Write throughput: {} points/sec", format_number_u128(tp3));
    println!("  Docs: {} | Points/doc: {}", format_number(docs3), s3_cycles);
    println!();

    // --- Scenario 4: High-cardinality stress (500K series × 4 pts) ---
    let s4_cycles = 4;
    let s4_points = series_500k.len() * s4_cycles;
    let s4_docs = series_500k.len();
    println!(
        "Scenario 4: High-cardinality stress ({} series x {} pts = {} points -> {} docs)",
        format_number(series_500k.len()),
        s4_cycles,
        format_number(s4_points),
        format_number(s4_docs)
    );
    let (time4, points4, docs4) = bench_metric_buffered("s4", &series_500k, s4_cycles);
    let tp4 = if time4 > 0 { (points4 as u128 * 1000) / time4 } else { u128::MAX };
    println!("  Time: {} ms", format_number_u128(time4));
    println!("  Write throughput: {} points/sec", format_number_u128(tp4));
    println!("  Docs: {} | Points/doc: {}", format_number(docs4), s4_cycles);
    println!();

    // --- Scenario 5: Pure write_point throughput (no IO) ---
    let s5_total = 1_000_000;
    println!(
        "Scenario 5: Pure write_point ({} calls across {} series, no flush/IO)",
        format_number(s5_total),
        format_number(series_100k.len())
    );
    let (time5, calls5) = bench_pure_write_point(&series_100k, s5_total);
    let tp5 = if time5 > 0 { (calls5 as u128 * 1000) / time5 } else { u128::MAX };
    println!("  Time: {} ms", format_number_u128(time5));
    println!("  Throughput: {} write_point calls/sec", format_number_u128(tp5));
    println!();

    // --- Scenario 6: Batch write_point vs write_points (JNI-crossing-equivalent) ---
    let s6_batch_size = 20;
    let s6_series_count = series_100k.len();
    let s6_total_points = s6_series_count * s6_batch_size;
    println!(
        "Scenario 6: Batch comparison ({} series x {} pts = {} points)",
        format_number(s6_series_count),
        s6_batch_size,
        format_number(s6_total_points)
    );
    println!("  A) write_point: {} individual calls (simulates {}M JNI crossings)",
        format_number(s6_total_points), s6_total_points / 1_000_000);
    let (time6a, calls6a) = bench_pure_write_point(&series_100k, s6_total_points);
    let tp6a = if time6a > 0 { (calls6a as u128 * 1000) / time6a } else { u128::MAX };
    println!("     Time: {} ms | Throughput: {} pts/sec", format_number_u128(time6a), format_number_u128(tp6a));

    println!("  B) write_points: {} batch calls ({} pts/batch, 1 lock each)",
        format_number(s6_series_count), s6_batch_size);
    let (time6b, points6b, batches6b) = bench_pure_write_points_batch(&series_100k, s6_batch_size);
    let tp6b = if time6b > 0 { (points6b as u128 * 1000) / time6b } else { u128::MAX };
    println!("     Time: {} ms | Throughput: {} pts/sec | Batches: {}",
        format_number_u128(time6b), format_number_u128(tp6b), format_number(batches6b));

    if time6b > 0 && time6a > 0 {
        let speedup = time6a as f64 / time6b as f64;
        println!("  Speedup: {:.2}x (batch vs single) | Lock acquisitions: {} vs {}",
            speedup, format_number(s6_series_count), format_number(s6_total_points));
    }
    println!();

    // --- Analysis ---
    println!("=== Analysis ===");

    // Doc reduction
    let doc_reduction_s1 = s1_points as f64 / docs1 as f64;
    let doc_reduction_s3 = s3_points as f64 / docs3 as f64;
    println!(
        "Doc reduction ratio: {:.0}x (4 pts/series), {:.0}x (16 pts/series)",
        doc_reduction_s1, doc_reduction_s3
    );

    // Write speedup (metric vs log for same point count)
    if tp2 > 0 {
        let speedup_s1 = tp1 as f64 / tp2 as f64;
        println!("Write speedup (S1 metric vs S2 log): {:.2}x", speedup_s1);
    }
    if time2 > 0 && time1 > 0 {
        let time_speedup = time2 as f64 / time1 as f64;
        println!("Time ratio (S2 log / S1 metric): {:.2}x faster", time_speedup);
    }

    // Gorilla compression sizes
    let gorilla_4 = avg_gorilla_size(4);
    let gorilla_16 = avg_gorilla_size(16);
    let gorilla_1 = avg_gorilla_size(1);
    println!(
        "Gorilla encoding: avg {} bytes/doc (4 pts), {} bytes/doc (16 pts), {} bytes/doc (1 pt log)",
        gorilla_4, gorilla_16, gorilla_1
    );
    println!(
        "  Bytes per point: {:.1} (4 pts/doc) vs {:.1} (16 pts/doc) vs {:.1} (1 pt/doc)",
        gorilla_4 as f64 / 4.0,
        gorilla_16 as f64 / 16.0,
        gorilla_1 as f64
    );

    // Memory estimate for buffer
    // Each series entry: name (~10B) + labels (~3 pairs × ~12B) + times (8B/pt) + values (8B/pt)
    // + HashMap overhead (~64B per entry)
    let per_series_4pts = 10 + 36 + 8 * 4 + 8 * 4 + 64;
    let mem_100k_4 = series_100k.len() * per_series_4pts;
    let per_series_16pts = 10 + 36 + 8 * 16 + 8 * 16 + 64;
    let mem_100k_16 = series_100k.len() * per_series_16pts;
    let mem_500k_4 = series_500k.len() * per_series_4pts;
    println!(
        "Memory: buffer peak ~{} MB (100K series x 4 pts), ~{} MB (100K x 16 pts), ~{} MB (500K x 4 pts)",
        mem_100k_4 / (1024 * 1024),
        mem_100k_16 / (1024 * 1024),
        mem_500k_4 / (1024 * 1024)
    );
}
