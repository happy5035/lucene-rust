//! Full-day metric performance benchmark simulating a production workload.
//!
//! Scenario:
//! - 1 million unique series (adaptive: falls back to 100K if too slow)
//! - 15-second scrape interval
//! - 5-minute flush cycle (20 points per series per flush)
//! - Write phase: 12 flush cycles (1 hour of data), each shard index-sorted
//!   by series_hash (SeriesBuffer::create_v5 sets index_sort)
//! - Merge phase: streaming k-way merge of all 12 shards into 1 compact shard
//!   (BinaryHeap over per-shard series_hash cursors — only one series's points
//!   are decoded at a time, so the full merge fits in memory)
//! - Read phase: read and decompress all samples from compact shard

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use codec_lucene9::FSDirectory;
use rustlucene_core::index_writer::IndexWriterConfig;
use rustlucene_core::search::reader::Reader;
use rustlucene_metric::algo::gorilla;
use rustlucene_metric::runtime::buffer::SeriesBuffer;
use rustlucene_metric::runtime::merge_op::merge_cross_shard;

/// Read available memory from /proc/meminfo (Linux). Returns MB.
fn available_memory_mb() -> u64 {
    if let Ok(content) = std::fs::read_to_string("/proc/meminfo") {
        for line in content.lines() {
            if line.starts_with("MemAvailable:") {
                let kb: u64 = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                return kb / 1024;
            }
        }
    }
    4096 // default assumption: 4 GB
}

fn main() {
    let mut num_series: usize = 1_000_000;
    let flush_cycles: usize = 12; // 1 hour (12 × 5min)
    let points_per_flush: usize = 20; // 15s × 20 = 300s = 5min
    let scrape_interval_ms: i64 = 15_000;

    let avail_mb = available_memory_mb();
    println!("=== Full-Day Metric Benchmark ===");
    println!(
        "Target series: {} | Flush cycles: {} (1 hour) | Points/series/flush: {}",
        num_series, flush_cycles, points_per_flush
    );
    println!(
        "Total points (target): {} | Total docs (target): {}",
        num_series * flush_cycles * points_per_flush,
        num_series * flush_cycles
    );
    println!("Available memory: {} MB", avail_mb);
    println!();

    let base_dir = std::env::temp_dir().join(format!("metric-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(&base_dir).unwrap();

    // === Phase 1: Write (12 flush cycles, each producing 1 shard) ===
    let mut shard_dirs: Vec<PathBuf> = Vec::new();
    let mut total_points: u64 = 0;
    let write_start = Instant::now();
    let mut scaled_down = false;

    for cycle in 0..flush_cycles {
        let shard_dir = base_dir.join(format!("shard_{}", cycle));
        let buf = SeriesBuffer::create_v5(&shard_dir, IndexWriterConfig::default()).unwrap();

        let base_time = (cycle * points_per_flush) as i64 * scrape_interval_ms;

        for series_id in 0..num_series {
            let name_idx = series_id % 100;
            let host_idx = series_id / 100;
            let name = format!("metric_{}", name_idx);
            let labels = format!("$#$host=h{}$#$id=s{}$#$", host_idx, series_id);

            for p in 0..points_per_flush {
                let time = base_time + p as i64 * scrape_interval_ms;
                let value = (series_id * 1000 + p) as f64 * 0.001;
                buf.write_point_with_labels_str(&name, &labels, time, value);
            }
            total_points += points_per_flush as u64;
        }

        buf.commit().unwrap();
        drop(buf);
        shard_dirs.push(shard_dir);

        let elapsed = write_start.elapsed();
        println!(
            "  Flush {}/{} done ({:.1}s elapsed, {:.0}K pts/s)",
            cycle + 1,
            flush_cycles,
            elapsed.as_secs_f64(),
            total_points as f64 / elapsed.as_secs_f64() / 1000.0
        );

        // Adaptive scaling: if first flush takes > 30s, scale down to 100K
        if cycle == 0 && elapsed.as_secs_f64() > 30.0 && num_series > 100_000 {
            println!();
            println!(
                "  NOTE: First flush took {:.1}s (>30s). Scaling down to 100K series.",
                elapsed.as_secs_f64()
            );
            println!("  Scaling factor: 10x (results extrapolated to 1M series)");
            println!();
            num_series = 100_000;
            scaled_down = true;
            // Restart cleanly at 100K
            let _ = std::fs::remove_dir_all(&base_dir);
            std::fs::create_dir_all(&base_dir).unwrap();
            shard_dirs.clear();
            total_points = 0;
            let shard_dir = base_dir.join("shard_0");
            let buf = SeriesBuffer::create_v5(&shard_dir, IndexWriterConfig::default()).unwrap();
            for series_id in 0..num_series {
                let name_idx = series_id % 100;
                let host_idx = series_id / 100;
                let name = format!("metric_{}", name_idx);
                let labels = format!("$#$host=h{}$#$id=s{}$#$", host_idx, series_id);
                for p in 0..points_per_flush {
                    let time = p as i64 * scrape_interval_ms;
                    let value = (series_id * 1000 + p) as f64 * 0.001;
                    buf.write_point_with_labels_str(&name, &labels, time, value);
                }
                total_points += points_per_flush as u64;
            }
            buf.commit().unwrap();
            drop(buf);
            shard_dirs.push(shard_dir);
            println!(
                "  Flush 1/{} redone at 100K series ({:.1}s elapsed)",
                flush_cycles,
                write_start.elapsed().as_secs_f64()
            );
        }
    }

    let write_time = write_start.elapsed();
    let write_throughput = total_points as f64 / write_time.as_secs_f64();
    let total_docs = num_series * flush_cycles;

    println!();
    println!("=== Write Phase Complete ===");
    println!("  Time: {:.1}s", write_time.as_secs_f64());
    println!(
        "  Points: {} ({:.2}M)",
        total_points,
        total_points as f64 / 1e6
    );
    println!(
        "  Docs: {} ({:.2}M)",
        total_docs,
        total_docs as f64 / 1e6
    );
    println!(
        "  Throughput: {:.0} points/sec ({:.2}M pts/s)",
        write_throughput,
        write_throughput / 1e6
    );
    let full_day_min = if scaled_down {
        write_time.as_secs_f64() / flush_cycles as f64 * 288.0 * 10.0 / 60.0
    } else {
        write_time.as_secs_f64() / flush_cycles as f64 * 288.0 / 60.0
    };
    if scaled_down {
        println!("  [Scaled down 10x from 1M to 100K series]");
        println!("  Extrapolated full day at 1M series (288 cycles): {:.0} min", full_day_min);
    } else {
        println!("  Extrapolated full day (288 cycles): {:.0} min", full_day_min);
    }
    println!();

    // === Phase 2: Merge shards into one ===
    // The streaming k-way merge holds each shard's docs in encoded form and
    // only decodes one series's points at a time, so the full 12-shard merge
    // fits in memory regardless of series count.
    let merge_shard_count = flush_cycles;

    let compact_dir = base_dir.join("compact");
    let ds5m_dir = base_dir.join("ds5m");
    let ds1h_dir = base_dir.join("ds1h");

    println!(
        "  Merging all {} shards (streaming k-way merge, index_sort by series_hash)",
        merge_shard_count
    );
    println!();

    let merge_start = Instant::now();
    let merge_dirs: Vec<&std::path::Path> = shard_dirs
        .iter()
        .take(merge_shard_count)
        .map(|p| p.as_path())
        .collect();

    let merge_result = merge_cross_shard(&merge_dirs, &compact_dir, &ds5m_dir, &ds1h_dir);
    let merge_time = merge_start.elapsed();

    match merge_result {
        Ok(stats) => {
            println!("=== Merge Phase Complete ===");
            println!("  Time: {:.1}s", merge_time.as_secs_f64());
            println!(
                "  Input: {} shards, {} docs, {} points ({:.2}M)",
                stats.input_shards,
                stats.input_docs,
                stats.input_points,
                stats.input_points as f64 / 1e6
            );
            println!(
                "  Output: {} docs (compact), {} points ({:.2}M, after dedup)",
                stats.output_docs,
                stats.output_points,
                stats.output_points as f64 / 1e6
            );
            println!(
                "  Downsample: {} docs (5m), {} docs (1h)",
                stats.downsample_5m_docs, stats.downsample_1h_docs
            );
            let merge_tp = stats.input_points as f64 / merge_time.as_secs_f64();
            println!(
                "  Merge throughput: {:.0} points/sec ({:.2}M pts/s)",
                merge_tp,
                merge_tp / 1e6
            );
            println!();

            // === Phase 3: Read samples from compact shard ===
            let read_start = Instant::now();
            let dir = FSDirectory::open(&compact_dir).unwrap();
            let mut reader = Reader::open(&dir).unwrap();

            let mut total_docs_read: u64 = 0;
            let mut total_samples_decoded: u64 = 0;

            for (_doc_base, seg) in reader.leaves() {
                let counts = seg.numeric_values("sample_count").unwrap();
                let gorilla_datas = seg.binary_values("gorilla_data").unwrap();

                let count_map: HashMap<u32, i64> = counts.into_iter().collect();

                for (doc_id, gorilla_bytes) in &gorilla_datas {
                    let sample_count = count_map.get(doc_id).copied().unwrap_or(0) as usize;
                    match gorilla::decode(gorilla_bytes, sample_count) {
                        Ok((times, _values)) => {
                            total_samples_decoded += times.len() as u64;
                        }
                        Err(e) => {
                            eprintln!("  WARNING: decode error on doc {}: {:?}", doc_id, e);
                        }
                    }
                    total_docs_read += 1;
                }
            }

            let read_time = read_start.elapsed();
            println!(
                "=== Read Phase Complete ({:.1} hours of samples, {} flush cycles) ===",
                merge_shard_count as f64 * 5.0 / 60.0,
                merge_shard_count
            );
            println!("  Time: {:.2}s", read_time.as_secs_f64());
            println!(
                "  Docs read: {} ({:.2}M)",
                total_docs_read,
                total_docs_read as f64 / 1e6
            );
            println!(
                "  Samples decoded: {} ({:.2}M)",
                total_samples_decoded,
                total_samples_decoded as f64 / 1e6
            );
            let read_tp = total_samples_decoded as f64 / read_time.as_secs_f64();
            println!(
                "  Decode throughput: {:.0} samples/sec ({:.2}M/s)",
                read_tp,
                read_tp / 1e6
            );
            println!();

            // === Summary ===
            println!("=== SUMMARY ===");
            println!(
                "  Series: {} | Flush cycles: {} | Points/flush/series: {}",
                num_series, flush_cycles, points_per_flush
            );
            if scaled_down {
                println!("  [NOTE: Ran at 100K series (10x scale-down from 1M target)]");
            }
            println!(
                "  Write:   {:.2}M pts/s | Full day (288 cycles) estimate: {:.0} min{}",
                write_throughput / 1e6,
                full_day_min,
                if scaled_down { " (extrapolated to 1M)" } else { "" }
            );
            println!(
                "  Merge:   {:.2}M pts/s ({} shards, streaming k-way)",
                merge_tp / 1e6,
                merge_shard_count
            );
            println!("  Read:    {:.2}M samples/s (gorilla decode)", read_tp / 1e6);
        }
        Err(e) => {
            println!("=== Merge Phase FAILED ===");
            println!("  Error: {}", e);
            println!("  Time before failure: {:.1}s", merge_time.as_secs_f64());
            println!("  NOTE: streaming k-way merge failed unexpectedly ({} docs).", total_docs);
            println!();
            println!("=== SUMMARY (partial - merge failed) ===");
            println!("  Write:   {:.2}M pts/s", write_throughput / 1e6);
            println!("  Merge:   FAILED ({})", e);
        }
    }

    // Cleanup
    let _ = std::fs::remove_dir_all(&base_dir);
}
