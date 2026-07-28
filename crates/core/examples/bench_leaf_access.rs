//! LeafAccess unification performance benchmark.
//!
//! Measures:
//!   1. Write throughput (add_document) — verifies the unification didn't regress writes
//!   2. In-memory search latency — all query types on unflushed data via MemoryLeafAccess
//!   3. Disk search latency — same queries after flush, for comparison
//!
//! Usage: cargo run --release --example bench_leaf_access [num_docs]
//!   num_docs: 10000 | 100000 | 500000 | 1000000 (default 100000)

use rustlucene_core::search::collector::CountCollector;
use rustlucene_core::search::query::{Occur, Query};
use rustlucene_core::search::searcher::Searcher;
use rustlucene_core::*;
use codec_lucene9::directory::FSDirectory;
use std::time::Instant;

const WARMUP: usize = 3;
const ITERS: usize = 20;

fn schema() -> Schema {
    let mut s = Schema::new();
    s.add(FieldSpec::keyword("level"));
    s.add(FieldSpec::keyword("host"));
    s.add(FieldSpec::text_with_positions("message"));
    s.add(FieldSpec::long_point("ts").with_numeric_dv());
    s
}

fn make_doc(i: u32) -> Document {
    let level = match i % 5 {
        0 => "INFO",
        1 => "WARN",
        2 => "ERROR",
        3 => "DEBUG",
        _ => "TRACE",
    };
    let host = format!("host-{}", i % 100);
    let msg = match i % 8 {
        0 => "quick brown fox jumps over lazy dog",
        1 => "the quick brown fox",
        2 => "error connecting to database server",
        3 => "connection timeout after 30 seconds",
        4 => "user login successful from 192.168.1.1",
        5 => "cache miss for key user_session_token",
        6 => "request processed in 42 milliseconds",
        _ => "garbage collection pause detected",
    };
    let mut d = Document::new();
    d.add("level", FieldValue::Keyword(level.to_string()));
    d.add("host", FieldValue::Keyword(host));
    d.add("message", FieldValue::Text(msg.to_string()));
    d.add("ts", FieldValue::Long(1_700_000_000 + i as i64));
    d
}

fn bench_write(num_docs: u32) -> f64 {
    let dir = std::path::Path::new("/tmp/bench-la-write");
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();

    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = num_docs + 1; // never auto-flush

    let t0 = Instant::now();
    let mut w = IndexWriter::create(dir, schema(), config).unwrap();
    for i in 0..num_docs {
        w.add_document(make_doc(i)).unwrap();
    }
    let elapsed = t0.elapsed();
    let docs_per_sec = num_docs as f64 / elapsed.as_secs_f64();
    let _ = std::fs::remove_dir_all(dir);
    docs_per_sec
}

fn bench_memory_search(num_docs: u32) {
    let dir = std::path::Path::new("/tmp/bench-la-mem");
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();

    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = num_docs + 1; // never auto-flush

    let mut w = IndexWriter::create(dir, schema(), config).unwrap();
    for i in 0..num_docs {
        w.add_document(make_doc(i)).unwrap();
    }
    // NO flush — everything in memory buffer

    let cases: Vec<(&str, Query, Option<(&str, bool)>, usize)> = vec![
        ("Term(level=INFO)", Query::term("level", "INFO"), None, 100),
        ("MatchAll", Query::MatchAll, None, 100),
        ("And(quick+brown)", Query::and("message", &["quick", "brown"]), None, 100),
        ("Or(quick+error)", Query::or("message", &["quick", "error"]), None, 100),
        ("Phrase(quick brown)", Query::phrase("message", &["quick", "brown"]), None, 100),
        ("Prefix(host-1)", Query::prefix("host", "host-1"), None, 100),
        ("Wildcard(host-1?)", Query::wildcard("host", "host-1?"), None, 100),
        ("PointRange(ts)", Query::point_range("ts", 1_700_000_000, 1_700_000_000 + (num_docs / 10) as i64), None, 100),
        (
            "Bool(MUST level + MUSTNOT error)",
            Query::bool(vec![
                (Occur::Must, Query::term("level", "INFO")),
                (Occur::MustNot, Query::term("message", "error")),
            ]),
            None,
            100,
        ),
        ("Term+SortDesc(ts)", Query::term("level", "INFO"), Some(("ts", true)), 10),
        ("MatchAll+SortDesc(ts)", Query::MatchAll, Some(("ts", true)), 10),
    ];

    println!("\n=== In-Memory Search ({} docs, unflushed) ===", num_docs);
    println!("{:<35} {:>10} {:>10} {:>10}", "query", "avg(us)", "min(us)", "hits");
    println!("{}", "-".repeat(70));

    for (label, query, sort, top_n) in &cases {
        // warmup
        for _ in 0..WARMUP {
            let _ = w.search(query, *sort, *top_n).unwrap();
        }
        // timed
        let mut times = Vec::with_capacity(ITERS);
        let mut hits = 0u64;
        for _ in 0..ITERS {
            let t0 = Instant::now();
            let r = w.search(query, *sort, *top_n).unwrap();
            times.push(t0.elapsed().as_secs_f64() * 1e6);
            hits = r.total;
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let avg: f64 = times.iter().sum::<f64>() / ITERS as f64;
        let min = times[0];
        println!("{:<35} {:>10.0} {:>10.0} {:>10}", label, avg, min, hits);
    }

    let _ = std::fs::remove_dir_all(dir);
}

fn bench_disk_search(num_docs: u32) {
    let dir = std::path::Path::new("/tmp/bench-la-disk");
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();

    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = num_docs + 1;

    let mut w = IndexWriter::create(dir, schema(), config).unwrap();
    for i in 0..num_docs {
        w.add_document(make_doc(i)).unwrap();
    }
    w.commit().unwrap();

    let fsdir = FSDirectory::open(dir).unwrap();
    rustlucene_core::merge::force_merge(&fsdir, &IndexWriterConfig::default()).unwrap();

    let cases: Vec<(&str, Query, u64)> = vec![
        ("Term(level=INFO)", Query::term("level", "INFO"), (num_docs as u64 + 4) / 5),
        ("MatchAll", Query::MatchAll, num_docs as u64),
        ("And(quick+brown)", Query::and("message", &["quick", "brown"]), 0), // count verified at runtime
        ("Or(quick+error)", Query::or("message", &["quick", "error"]), 0),
        ("Phrase(quick brown)", Query::phrase("message", &["quick", "brown"]), 0),
        ("Prefix(host-1)", Query::prefix("host", "host-1"), 0),
        ("Wildcard(host-1?)", Query::wildcard("host", "host-1?"), 0),
        ("PointRange(ts)", Query::point_range("ts", 1_700_000_000, 1_700_000_000 + (num_docs / 10) as i64), 0),
        (
            "Bool(MUST level + MUSTNOT error)",
            Query::bool(vec![
                (Occur::Must, Query::term("level", "INFO")),
                (Occur::MustNot, Query::term("message", "error")),
            ]),
            0,
        ),
    ];

    println!("\n=== Disk Search ({} docs, flushed+merged) ===", num_docs);
    println!("{:<35} {:>10} {:>10} {:>10}", "query", "avg(us)", "min(us)", "hits");
    println!("{}", "-".repeat(70));

    for (label, query, _expect) in &cases {
        // warmup + get actual count
        let mut hits = 0u64;
        for _ in 0..WARMUP {
            let mut s = Searcher::open(&fsdir).unwrap();
            let mut c = CountCollector::default();
            s.search(query, &mut c).unwrap();
            hits = c.count;
        }
        // timed
        let mut times = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let t0 = Instant::now();
            let mut s = Searcher::open(&fsdir).unwrap();
            let mut c = CountCollector::default();
            s.search(query, &mut c).unwrap();
            times.push(t0.elapsed().as_secs_f64() * 1e6);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let avg: f64 = times.iter().sum::<f64>() / ITERS as f64;
        let min = times[0];
        println!("{:<35} {:>10.0} {:>10.0} {:>10}", label, avg, min, hits);
    }

    let _ = std::fs::remove_dir_all(dir);
}

fn bench_write_with_search_interleaved(num_docs: u32) {
    let dir = std::path::Path::new("/tmp/bench-la-interleave");
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();

    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = num_docs + 1;

    let mut w = IndexWriter::create(dir, schema(), config).unwrap();
    let query = Query::term("level", "INFO");

    println!("\n=== Write + Search Interleaved ({} docs) ===", num_docs);
    println!("{:<20} {:>12} {:>12}", "phase", "docs", "time(ms)");
    println!("{}", "-".repeat(48));

    let batch = num_docs / 10;
    for phase in 0..10 {
        // write batch
        let t0 = Instant::now();
        for i in (phase * batch)..((phase + 1) * batch) {
            w.add_document(make_doc(i)).unwrap();
        }
        let write_ms = t0.elapsed().as_secs_f64() * 1e3;

        // search after each batch
        let t0 = Instant::now();
        let r = w.search(&query, None, 10).unwrap();
        let search_us = t0.elapsed().as_secs_f64() * 1e6;

        println!(
            "batch {:>2}         {:>12} {:>9.1}ms  search={:.0}us hits={}",
            phase,
            (phase + 1) * batch,
            write_ms,
            search_us,
            r.total
        );
    }

    let _ = std::fs::remove_dir_all(dir);
}

fn main() {
    let num_docs: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);

    println!("LeafAccess Performance Benchmark");
    println!("docs={} warmup={} iters={}", num_docs, WARMUP, ITERS);
    println!("{}", "=".repeat(70));

    // 1. Write throughput
    let dps = bench_write(num_docs);
    println!("\n=== Write Throughput ===");
    println!("add_document: {:.0} docs/sec ({:.1} MB/sec @ ~200B/doc)", dps, dps * 200.0 / 1e6);

    // 2. In-memory search
    bench_memory_search(num_docs);

    // 3. Disk search (for comparison)
    bench_disk_search(num_docs);

    // 4. Interleaved write+search
    bench_write_with_search_interleaved(num_docs);

    println!("\nDone.");
}
