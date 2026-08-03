//! DocValues write/read performance benchmark.
//!
//! Measures:
//!   1. Write throughput: Numeric / Sorted / Binary DV indexing (docs/sec)
//!   2. Read throughput: full-scan retrieval per DV type (MB/s, docs/sec)
//!   3. Column-store scenarios:
//!      - Dense vs Sparse (100% vs 10% docs carry a value)
//!      - Wide table (16 DV columns simultaneously)
//!      - Sorted cardinality (10 / 1K / 100K distinct terms)
//!      - Binary payload size (16B / 256B / 4KB)
//!      - Numeric range (affects bit-packing bpv: 8 / 32 / 64 bits)
//!
//! Usage: cargo run --release --example bench_docvalues [num_docs]
//!   num_docs: default 500_000

use codec_lucene9::directory::FSDirectory;
use rustlucene_core::search::reader::Reader;
use rustlucene_core::*;
use std::time::Instant;

const WARMUP: usize = 2;
const ITERS: usize = 5;

fn default_docs() -> u32 {
    std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(500_000)
}

fn clean(path: &str) {
    let _ = std::fs::remove_dir_all(path);
    std::fs::create_dir_all(path).unwrap();
}

struct Stats {
    avg: f64,
    min: f64,
    p50: f64,
}

fn stats(times: &mut [f64]) -> Stats {
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = times.iter().sum::<f64>() / times.len() as f64;
    let min = times[0];
    let p50 = times[times.len() / 2];
    Stats { avg, min, p50 }
}

// ─── Write Benchmarks ───────────────────────────────────────────────────────

fn bench_write_numeric(num_docs: u32) -> f64 {
    let path = "/tmp/bench-dv-w-num";
    clean(path);
    let mut s = Schema::new();
    s.add(FieldSpec::numeric_dv("val"));

    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = num_docs + 1;

    let mut w = IndexWriter::create(std::path::Path::new(path), s, config).unwrap();
    let t0 = Instant::now();
    for i in 0..num_docs {
        let mut d = Document::new();
        d.add("val", FieldValue::Long((i as i64) * 7 + 13));
        w.add_document(d).unwrap();
    }
    w.flush().unwrap();
    let elapsed = t0.elapsed().as_secs_f64();
    let _ = std::fs::remove_dir_all(path);
    num_docs as f64 / elapsed
}

fn bench_write_sorted(num_docs: u32, cardinality: u32) -> f64 {
    let path = "/tmp/bench-dv-w-sorted";
    clean(path);
    let mut s = Schema::new();
    s.add(FieldSpec::sorted_dv("tag"));

    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = num_docs + 1;

    let terms: Vec<String> = (0..cardinality).map(|i| format!("term_{:08}", i)).collect();

    let mut w = IndexWriter::create(std::path::Path::new(path), s, config).unwrap();
    let t0 = Instant::now();
    for i in 0..num_docs {
        let mut d = Document::new();
        d.add("tag", FieldValue::Keyword(terms[(i % cardinality) as usize].clone()));
        w.add_document(d).unwrap();
    }
    w.flush().unwrap();
    let elapsed = t0.elapsed().as_secs_f64();
    let _ = std::fs::remove_dir_all(path);
    num_docs as f64 / elapsed
}

fn bench_write_binary(num_docs: u32, payload_size: usize) -> f64 {
    let path = "/tmp/bench-dv-w-bin";
    clean(path);
    let mut s = Schema::new();
    s.add(FieldSpec::binary_dv("blob"));

    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = num_docs + 1;

    let payloads: Vec<Vec<u8>> = (0..64)
        .map(|k| {
            (0..payload_size)
                .map(|j| ((k * payload_size + j) % 251) as u8)
                .collect()
        })
        .collect();

    let mut w = IndexWriter::create(std::path::Path::new(path), s, config).unwrap();
    let t0 = Instant::now();
    for i in 0..num_docs {
        let mut d = Document::new();
        d.add("blob", FieldValue::Bytes(payloads[(i as usize) % 64].clone()));
        w.add_document(d).unwrap();
    }
    w.flush().unwrap();
    let elapsed = t0.elapsed().as_secs_f64();
    let _ = std::fs::remove_dir_all(path);
    num_docs as f64 / elapsed
}

fn bench_write_wide(num_docs: u32, num_fields: usize) -> f64 {
    let path = "/tmp/bench-dv-w-wide";
    clean(path);
    let mut s = Schema::new();
    for f in 0..num_fields {
        match f % 3 {
            0 => s.add(FieldSpec::numeric_dv(&format!("num_{}", f))),
            1 => s.add(FieldSpec::sorted_dv(&format!("str_{}", f))),
            _ => s.add(FieldSpec::binary_dv(&format!("bin_{}", f))),
        };
    }

    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = num_docs + 1;

    let blob: Vec<u8> = (0..64).map(|j| (j % 251) as u8).collect();
    let terms: Vec<String> = (0..1000).map(|i| format!("t{:04}", i)).collect();

    let mut w = IndexWriter::create(std::path::Path::new(path), s, config).unwrap();
    let t0 = Instant::now();
    for i in 0..num_docs {
        let mut d = Document::new();
        for f in 0..num_fields {
            match f % 3 {
                0 => d.add(&format!("num_{}", f), FieldValue::Long(i as i64 * 3 + f as i64)),
                1 => d.add(
                    &format!("str_{}", f),
                    FieldValue::Keyword(terms[(i as usize + f) % 1000].clone()),
                ),
                _ => d.add(&format!("bin_{}", f), FieldValue::Bytes(blob.clone())),
            };
        }
        w.add_document(d).unwrap();
    }
    w.flush().unwrap();
    let elapsed = t0.elapsed().as_secs_f64();
    let _ = std::fs::remove_dir_all(path);
    num_docs as f64 / elapsed
}

fn bench_write_sparse(num_docs: u32, density_pct: u32) -> f64 {
    let path = "/tmp/bench-dv-w-sparse";
    clean(path);
    let mut s = Schema::new();
    s.add(FieldSpec::numeric_dv("val"));

    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = num_docs + 1;

    let mut w = IndexWriter::create(std::path::Path::new(path), s, config).unwrap();
    let t0 = Instant::now();
    for i in 0..num_docs {
        let mut d = Document::new();
        if i % 100 < density_pct {
            d.add("val", FieldValue::Long(i as i64));
        }
        w.add_document(d).unwrap();
    }
    w.flush().unwrap();
    let elapsed = t0.elapsed().as_secs_f64();
    let _ = std::fs::remove_dir_all(path);
    num_docs as f64 / elapsed
}

// ─── Read Benchmarks ────────────────────────────────────────────────────────

fn build_read_index_numeric(path: &str, num_docs: u32, range_bits: u32) {
    clean(path);
    let mut s = Schema::new();
    s.add(FieldSpec::keyword("_id"));
    s.add(FieldSpec::numeric_dv("val"));
    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = num_docs + 1;
    let mask: i64 = if range_bits >= 63 { i64::MAX } else { (1i64 << range_bits) - 1 };
    let mut w = IndexWriter::create(std::path::Path::new(path), s, config).unwrap();
    for i in 0..num_docs {
        let mut d = Document::new();
        d.add("_id", FieldValue::Keyword("x".to_string()));
        d.add("val", FieldValue::Long(((i as i64).wrapping_mul(2654435761)) & mask));
        w.add_document(d).unwrap();
    }
    w.commit().unwrap();
}

fn build_read_index_sorted(path: &str, num_docs: u32, cardinality: u32) {
    clean(path);
    let mut s = Schema::new();
    s.add(FieldSpec::keyword("_id"));
    s.add(FieldSpec::sorted_dv("tag"));
    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = num_docs + 1;
    let terms: Vec<String> = (0..cardinality).map(|i| format!("term_{:08}", i)).collect();
    let mut w = IndexWriter::create(std::path::Path::new(path), s, config).unwrap();
    for i in 0..num_docs {
        let mut d = Document::new();
        d.add("_id", FieldValue::Keyword("x".to_string()));
        d.add("tag", FieldValue::Keyword(terms[(i % cardinality) as usize].clone()));
        w.add_document(d).unwrap();
    }
    w.commit().unwrap();
}

fn build_read_index_binary(path: &str, num_docs: u32, payload_size: usize) {
    clean(path);
    let mut s = Schema::new();
    s.add(FieldSpec::keyword("_id"));
    s.add(FieldSpec::binary_dv("blob"));
    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = num_docs + 1;
    let payloads: Vec<Vec<u8>> = (0..64)
        .map(|k| (0..payload_size).map(|j| ((k * payload_size + j) % 251) as u8).collect())
        .collect();
    let mut w = IndexWriter::create(std::path::Path::new(path), s, config).unwrap();
    for i in 0..num_docs {
        let mut d = Document::new();
        d.add("_id", FieldValue::Keyword("x".to_string()));
        d.add("blob", FieldValue::Bytes(payloads[(i as usize) % 64].clone()));
        w.add_document(d).unwrap();
    }
    w.commit().unwrap();
}

fn build_read_index_sparse(path: &str, num_docs: u32, density_pct: u32) {
    clean(path);
    let mut s = Schema::new();
    s.add(FieldSpec::keyword("_id"));
    s.add(FieldSpec::numeric_dv("val"));
    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = num_docs + 1;
    let mut w = IndexWriter::create(std::path::Path::new(path), s, config).unwrap();
    for i in 0..num_docs {
        let mut d = Document::new();
        d.add("_id", FieldValue::Keyword("x".to_string()));
        if i % 100 < density_pct {
            d.add("val", FieldValue::Long(i as i64));
        }
        w.add_document(d).unwrap();
    }
    w.commit().unwrap();
}

fn bench_read_numeric(path: &str, num_docs: u32) -> (f64, f64) {
    let dir = FSDirectory::open(std::path::Path::new(path)).unwrap();
    let mut times = Vec::with_capacity(WARMUP + ITERS);
    let mut total_values = 0usize;
    for iter in 0..(WARMUP + ITERS) {
        let t0 = Instant::now();
        let mut reader = Reader::open(&dir).unwrap();
        let mut count = 0usize;
        for (_base, seg) in &mut reader.leaves() {
            let vals = seg.numeric_values("val").unwrap();
            count += vals.len();
            std::hint::black_box(&vals);
        }
        let elapsed = t0.elapsed().as_secs_f64() * 1e6;
        total_values = count;
        if iter >= WARMUP {
            times.push(elapsed);
        }
    }
    let s = stats(&mut times);
    let docs_per_sec = num_docs as f64 / (s.avg * 1e-6);
    let mb_per_sec = (total_values * 8) as f64 / (s.avg * 1e-6) / 1e6;
    (docs_per_sec, mb_per_sec)
}

fn bench_read_sorted(path: &str, num_docs: u32) -> (f64, f64) {
    let dir = FSDirectory::open(std::path::Path::new(path)).unwrap();
    let mut times = Vec::with_capacity(WARMUP + ITERS);
    let mut total_bytes = 0usize;
    for iter in 0..(WARMUP + ITERS) {
        let t0 = Instant::now();
        let mut reader = Reader::open(&dir).unwrap();
        let mut bytes = 0usize;
        let mut acc: u64 = 0;
        for (_base, seg) in &mut reader.leaves() {
            let mut vals = seg.sorted_doc_values("tag").unwrap();
            while let Some((_doc, v)) = vals.next() {
                bytes += v.len();
                // 对齐 Java bench 消费方式（length + 首字节），防 DCE
                acc += v.first().copied().unwrap_or(0) as u64;
            }
        }
        std::hint::black_box(acc);
        let elapsed = t0.elapsed().as_secs_f64() * 1e6;
        total_bytes = bytes;
        if iter >= WARMUP {
            times.push(elapsed);
        }
    }
    let s = stats(&mut times);
    let docs_per_sec = num_docs as f64 / (s.avg * 1e-6);
    let mb_per_sec = total_bytes as f64 / (s.avg * 1e-6) / 1e6;
    (docs_per_sec, mb_per_sec)
}

fn bench_read_binary(path: &str, num_docs: u32, payload_size: usize) -> (f64, f64) {
    let dir = FSDirectory::open(std::path::Path::new(path)).unwrap();
    let mut times = Vec::with_capacity(WARMUP + ITERS);
    for iter in 0..(WARMUP + ITERS) {
        let t0 = Instant::now();
        let mut reader = Reader::open(&dir).unwrap();
        let mut acc: u64 = 0;
        for (_base, seg) in &mut reader.leaves() {
            let mut vals = seg.binary_doc_values("blob").unwrap();
            while let Some((doc, bytes)) = vals.next() {
                std::hint::black_box(doc);
                // 对齐 Java bench 消费方式（length + 首字节），强制触页防 DCE
                acc += bytes.len() as u64 + bytes.first().copied().unwrap_or(0) as u64;
            }
        }
        std::hint::black_box(acc);
        let elapsed = t0.elapsed().as_secs_f64() * 1e6;
        if iter >= WARMUP {
            times.push(elapsed);
        }
    }
    let s = stats(&mut times);
    let docs_per_sec = num_docs as f64 / (s.avg * 1e-6);
    let mb_per_sec = (num_docs as usize * payload_size) as f64 / (s.avg * 1e-6) / 1e6;
    (docs_per_sec, mb_per_sec)
}

fn bench_read_binary_packed(path: &str, num_docs: u32, payload_size: usize) -> (f64, f64) {
    let dir = FSDirectory::open(std::path::Path::new(path)).unwrap();
    let mut times = Vec::with_capacity(WARMUP + ITERS);
    for iter in 0..(WARMUP + ITERS) {
        let t0 = Instant::now();
        let mut reader = Reader::open(&dir).unwrap();
        for (_base, seg) in &mut reader.leaves() {
            let packed = seg.binary_values_packed("blob").unwrap();
            std::hint::black_box(&packed);
        }
        let elapsed = t0.elapsed().as_secs_f64() * 1e6;
        if iter >= WARMUP {
            times.push(elapsed);
        }
    }
    let s = stats(&mut times);
    let docs_per_sec = num_docs as f64 / (s.avg * 1e-6);
    let mb_per_sec = (num_docs as usize * payload_size) as f64 / (s.avg * 1e-6) / 1e6;
    (docs_per_sec, mb_per_sec)
}

fn bench_read_sparse(path: &str, num_docs: u32, density_pct: u32) -> f64 {
    let dir = FSDirectory::open(std::path::Path::new(path)).unwrap();
    let mut times = Vec::with_capacity(WARMUP + ITERS);
    for iter in 0..(WARMUP + ITERS) {
        let t0 = Instant::now();
        let mut reader = Reader::open(&dir).unwrap();
        for (_base, seg) in &mut reader.leaves() {
            let vals = seg.numeric_values("val").unwrap();
            std::hint::black_box(&vals);
        }
        let elapsed = t0.elapsed().as_secs_f64() * 1e6;
        if iter >= WARMUP {
            times.push(elapsed);
        }
    }
    let s = stats(&mut times);
    let actual_docs = (num_docs as f64 * density_pct as f64 / 100.0) as f64;
    actual_docs / (s.avg * 1e-6)
}

// ─── Main ───────────────────────────────────────────────────────────────────

fn main() {
    let num_docs = default_docs();
    println!("╔══════════════════════════════════════════════════════════════════════╗");
    println!("║          DocValues Benchmark — {} docs                    ║", format!("{:>9}", num_docs));
    println!("╚══════════════════════════════════════════════════════════════════════╝");

    // ── Section 1: Write throughput ──
    println!("\n═══ 1. WRITE THROUGHPUT (docs/sec, includes flush) ═══\n");
    println!("{:<40} {:>14}", "scenario", "docs/sec");
    println!("{}", "─".repeat(56));

    let wps = bench_write_numeric(num_docs);
    println!("{:<40} {:>14.0}", "numeric (i64, full range)", wps);

    for card in [10u32, 1_000, 100_000] {
        let wps = bench_write_sorted(num_docs, card);
        println!("{:<40} {:>14.0}", format!("sorted (cardinality={})", card), wps);
    }

    for sz in [16usize, 256, 4096] {
        let wps = bench_write_binary(num_docs, sz);
        println!("{:<40} {:>14.0}", format!("binary (payload={}B)", sz), wps);
    }

    let wps = bench_write_wide(num_docs, 16);
    println!("{:<40} {:>14.0}", "wide table (16 mixed columns)", wps);

    for density in [100u32, 50, 10] {
        let wps = bench_write_sparse(num_docs, density);
        println!("{:<40} {:>14.0}", format!("numeric sparse ({}% density)", density), wps);
    }

    // ── Section 2: Read throughput — Numeric ──
    println!("\n═══ 2. READ — NUMERIC (full-scan) ═══\n");
    println!("{:<40} {:>14} {:>12}", "scenario", "docs/sec", "MB/s");
    println!("{}", "─".repeat(68));

    for bits in [8u32, 32, 63] {
        let path = "/tmp/bench-dv-r-num";
        build_read_index_numeric(path, num_docs, bits);
        let (dps, mbps) = bench_read_numeric(path, num_docs);
        println!("{:<40} {:>14.0} {:>12.1}", format!("range={}bit (bpv≈{})", bits, bits), dps, mbps);
        let _ = std::fs::remove_dir_all(path);
    }

    // ── Section 3: Read throughput — Sorted ──
    println!("\n═══ 3. READ — SORTED (full-scan, ord→bytes) ═══\n");
    println!("{:<40} {:>14} {:>12}", "scenario", "docs/sec", "MB/s");
    println!("{}", "─".repeat(68));

    for card in [10u32, 1_000, 100_000] {
        let path = "/tmp/bench-dv-r-sorted";
        build_read_index_sorted(path, num_docs, card);
        let (dps, mbps) = bench_read_sorted(path, num_docs);
        println!("{:<40} {:>14.0} {:>12.1}", format!("cardinality={}", card), dps, mbps);
        let _ = std::fs::remove_dir_all(path);
    }

    // ── Section 4: Read throughput — Binary ──
    println!("\n═══ 4. READ — BINARY (full-scan) ═══\n");
    println!("{:<40} {:>14} {:>12} {:>14}", "scenario", "docs/sec", "MB/s", "packed MB/s");
    println!("{}", "─".repeat(82));

    for sz in [16usize, 256, 4096] {
        let path = "/tmp/bench-dv-r-bin";
        build_read_index_binary(path, num_docs, sz);
        let (dps, mbps) = bench_read_binary(path, num_docs, sz);
        let (_, mbps_packed) = bench_read_binary_packed(path, num_docs, sz);
        println!(
            "{:<40} {:>14.0} {:>12.1} {:>14.1}",
            format!("payload={}B", sz),
            dps,
            mbps,
            mbps_packed
        );
        let _ = std::fs::remove_dir_all(path);
    }

    // ── Section 5: Sparse read ──
    println!("\n═══ 5. READ — SPARSE NUMERIC (IndexedDISI skip) ═══\n");
    println!("{:<40} {:>14}", "scenario", "valued-docs/sec");
    println!("{}", "─".repeat(56));

    for density in [100u32, 50, 10, 1] {
        let path = "/tmp/bench-dv-r-sparse";
        build_read_index_sparse(path, num_docs, density);
        let dps = bench_read_sparse(path, num_docs, density);
        println!("{:<40} {:>14.0}", format!("{}% density", density), dps);
        let _ = std::fs::remove_dir_all(path);
    }

    println!("\n✓ done");
}
