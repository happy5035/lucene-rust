//! rustlucene-cli: corpus generation, index writing, benchmark and golden-file
//! emission for the Java interop harness.
//!
//! Usage:
//!   rustlucene-cli write <indexDir> <numDocs> <docBytes> <seed> [goldenFile]
//!   rustlucene-cli bench <indexDir> <numDocs> <docBytes> <seed>
//!   rustlucene-cli index <inputFileOrDir> <indexDir> [--positions] [--docs N]
//!
//! The corpus generator mirrors interop/java/JavaLuceneBench.java exactly
//! (same xorshift64* stream and vocabulary => identical corpora).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use codec_lucene9::segment_infos::{SegmentCommitInfo, SegmentInfos};
use codec_lucene9::FSDirectory;
use rustlucene_core::search::{CountCollector, Query, Searcher};
use rustlucene_core::{
    commit_segments, BindOutcome, Document, FieldSpec, FieldValue, IndexWriter, IndexWriterConfig,
    JsonBinder, Schema, SegmentBuilder,
};

/// xorshift64* — keep in sync with JavaLuceneBench.XorShift.
struct XorShift {
    s: u64,
}

impl XorShift {
    fn new(seed: u64) -> Self {
        Self {
            s: if seed == 0 { 0x9E3779B97F4A7C15 } else { seed },
        }
    }
    fn next(&mut self) -> u64 {
        self.s ^= self.s >> 12;
        self.s ^= self.s << 25;
        self.s ^= self.s >> 27;
        self.s.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn next_int(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn vocab() -> Vec<String> {
    let levels = ["INFO", "WARN", "ERROR", "DEBUG", "TRACE"];
    let words: Vec<&str> = "connection timeout retry backoff socket buffer stream packet request response \
        client server upstream downstream latency throughput commit rollback segment flush merge \
        index query filter cache eviction compaction snapshot replica shard leader follower election \
        heartbeat protocol handshake encrypt decrypt token session expire renew validate schema \
        migrate upgrade downgrade rollback checkpoint journal wal fsync sync async batch queue"
        .split(' ')
        .collect();
    let mut v = Vec::with_capacity(words.len() * 40 + levels.len());
    // entries carry a trailing space so gen_message needs a single push_str
    // per token (byte-identical to pushing word + space separately).
    for w in words {
        for i in 0..40 {
            v.push(format!("{w}{i} "));
        }
    }
    for l in levels {
        v.push(format!("{l} "));
    }
    v
}

fn gen_message(rng: &mut XorShift, vocab: &[String], target_bytes: usize) -> String {
    let mut sb = String::with_capacity(target_bytes + 16);
    while sb.len() < target_bytes {
        sb.push_str(&vocab[rng.next_int(vocab.len() as u64) as usize]);
    }
    sb.truncate(target_bytes);
    sb
}

fn schema() -> Schema {
    let mut s = Schema::new();
    s.add(FieldSpec::text("message"));
    s
}

/// The M2 log-scenario schema: timestamp (LongPoint + NumericDV + stored),
/// level (keyword + SortedDV + stored), trace_id (keyword), message (text,
/// positions optional), and three NumericDocValues.
/// `bigdict` adds a high-cardinality SortedDocValues field (trace_id_sdv)
/// to exercise the terms-dict multi-block + reverse-index paths end to end.
fn log_schema(positions: bool, bigdict: bool) -> Schema {
    let mut s = Schema::new();
    s.add(
        FieldSpec::long_point("timestamp")
            .with_numeric_dv()
            .with_stored(true),
    );
    s.add(FieldSpec::keyword("level").with_sorted_dv());
    s.add(FieldSpec::keyword("trace_id"));
    if bigdict {
        s.add(FieldSpec::sorted_dv("trace_id_sdv"));
    }
    s.add(if positions {
        FieldSpec::text_with_positions("message")
    } else {
        FieldSpec::text("message")
    });
    s.add(FieldSpec::numeric_dv("latency_ms"));
    s.add(FieldSpec::numeric_dv("bytes_sent"));
    s.add(FieldSpec::numeric_dv("status"));
    s
}

const LEVELS: [&str; 5] = ["INFO", "WARN", "ERROR", "DEBUG", "TRACE"];
const STATUSES: [i64; 5] = [200, 200, 200, 404, 500];
const TS_BASE: i64 = 1_700_000_000_000;

/// One synthetic log document. The rng call sequence is fixed and must stay
/// byte-identical with interop/java/JavaLogBench.java.
///
/// With `sparse`, some fields are deterministically absent (drawn from the rng
/// regardless, so the stream stays aligned): latency_ms missing every 7th doc,
/// bytes_sent every 11th, level every 13th, status present only every 17th —
/// exercising IndexedDISI's DENSE/SPARSE branches and multi-block jump tables.
fn gen_log_document(
    rng: &mut XorShift,
    vocab: &[String],
    doc_id: u64,
    sparse: bool,
    bigdict: bool,
) -> Document {
    let ts = TS_BASE + doc_id as i64 * 1000 + rng.next_int(1000) as i64;
    let level = LEVELS[rng.next_int(5) as usize];
    let trace_id = format!("{:016x}{:016x}", rng.next(), rng.next());
    let message = gen_message(rng, vocab, 200);
    let latency = rng.next_int(10_000) as i64;
    let bytes = rng.next_int(1_000_000) as i64;
    let status = STATUSES[rng.next_int(5) as usize];
    let mut doc = Document::new();
    doc.add("timestamp", FieldValue::Long(ts));
    if !sparse || doc_id % 13 != 0 {
        doc.add("level", FieldValue::Keyword(level.to_string()));
    }
    doc.add("trace_id", FieldValue::Keyword(trace_id.clone()));
    if bigdict {
        doc.add("trace_id_sdv", FieldValue::Keyword(trace_id));
    }
    doc.add("message", FieldValue::Text(message));
    if !sparse || doc_id % 7 != 0 {
        doc.add("latency_ms", FieldValue::Long(latency));
    }
    if !sparse || doc_id % 11 != 0 {
        doc.add("bytes_sent", FieldValue::Long(bytes));
    }
    if !sparse || doc_id % 17 == 0 {
        doc.add("status", FieldValue::Long(status));
    }
    doc
}

/// Search battery over a log-corpus index, printed line by line in the exact
/// format of interop/java/VerifySearchIndex.java — the two outputs are
/// diffed by interop/verify-search.sh (make log-test).
fn searchdump(index_dir: &Path, num_docs: u32, seed: u64, positions: bool) -> std::io::Result<()> {
    let dir = FSDirectory::open(index_dir)?;
    let mut searcher = Searcher::open(&dir)?;
    let mut out = String::new();
    out.push_str(&format!("maxDoc={}\n", searcher.max_doc()));

    for level in LEVELS {
        let count = searcher.count(&Query::term("level", level))?;
        out.push_str(&format!("term level={level} count={count}\n"));
    }
    let (_, docs) = searcher.top_docs(&Query::term("level", "INFO"), 20)?;
    out.push_str(&format!("term level=INFO first20={}\n", doc_csv(&docs)));

    for w in ["connection0", "query23", "queue39"] {
        let q = Query::term("message", w);
        let count = searcher.count(&q)?;
        let freqsum = searcher.freq_sum(&q)?;
        out.push_str(&format!(
            "term message={w} count={count} freqsum={freqsum}\n"
        ));
    }
    let count = searcher.count(&Query::term("message", "nosuchterm42"))?;
    out.push_str(&format!("term message=nosuchterm42 count={count}\n"));

    if num_docs > 7 {
        let tid = trace_id_of_doc(seed, 7);
        let count = searcher.count(&Query::term("trace_id", &tid))?;
        out.push_str(&format!("term trace_id(doc7)={tid} count={count}\n"));
    }

    let count = searcher.count(&Query::MatchAll)?;
    out.push_str(&format!("matchall count={count}\n"));
    let (_, docs) = searcher.top_docs(&Query::MatchAll, 20)?;
    out.push_str(&format!("matchall first20={}\n", doc_csv(&docs)));

    // M6 §3.4 range battery (timestamp = LongPoint)：边界确定性推导——
    // ts(doc i) ∈ [TS_BASE + i*1000, TS_BASE + i*1000 + 999]
    // (gen_log_document :132)，所以 [base+100000, base+199999] 恰好命中
    // docs 100..=199。逐行镜像在 VerifySearchIndex.java。
    if num_docs >= 200 {
        let range_battery: [(i64, i64); 5] = [
            (TS_BASE + 100_000, TS_BASE + 199_999), // docs 100..=199 → 100
            (TS_BASE - 1_000_000, TS_BASE - 1),     // 不相交 → 0
            (i64::MIN, i64::MAX),                   // 全区间 → num_docs
            (i64::MIN, TS_BASE + 49_999),           // 贴 MIN → docs 0..=49 → 50
            (TS_BASE + 150_000, i64::MAX),          // 贴 MAX → docs 150.. → num_docs-150
        ];
        for (low, high) in range_battery {
            let q = Query::point_range("timestamp", low, high);
            let count = searcher.count(&q)?;
            let (_, docs) = searcher.top_docs(&q, 20)?;
            out.push_str(&format!(
                "range timestamp=[{low},{high}] count={count} first20={}\n",
                doc_csv(&docs)
            ));
        }
        // 点查询退化 [v,v] 恰好命中 doc 100
        let q = Query::point_range("timestamp", TS_BASE + 100_000, TS_BASE + 100_999);
        let count = searcher.count(&q)?;
        out.push_str(&format!(
            "range timestamp=[{},{}] count={count}\n",
            TS_BASE + 100_000,
            TS_BASE + 100_999
        ));
    }

    // Boolean battery (search spec phase 3): "and" = BooleanQuery MUST+MUST,
    // "or" = SHOULD+SHOULD. The message pair has a non-empty intersection in
    // the log corpus (co-occurring tokens); level is single-valued per doc so
    // INFO ∩ WARN is empty by construction. Mirrored in VerifySearchIndex.java.
    let boolean_battery: [(&str, &str, [&str; 2]); 5] = [
        ("and", "message", ["connection0", "query23"]),
        ("and", "level", ["INFO", "WARN"]),
        ("or", "message", ["connection0", "query23"]),
        ("or", "message", ["connection0", "nosuchterm42"]),
        ("and", "message", ["connection0", "nosuchterm42"]),
    ];
    for (op, field, terms) in boolean_battery {
        let q = match op {
            "and" => Query::and(field, &terms),
            _ => Query::or(field, &terms),
        };
        let count = searcher.count(&q)?;
        let (_, docs) = searcher.top_docs(&q, 20)?;
        out.push_str(&format!(
            "{op} {field}={} count={count} first20={}\n",
            terms.join(","),
            doc_csv(&docs)
        ));
    }

    // M2 Terms(IN) battery (search spec M2 §4): 3-term sets take the <=16 OR
    // rewrite path, the 17-term set the >16 bitset path; the mixed
    // present/missing item locks the empty-term tolerance. Mirrored in
    // VerifySearchIndex.java.
    let terms17: Vec<String> = (0..17).map(|i| format!("connection{i}")).collect();
    let terms_battery: [(&str, Vec<String>); 4] = [
        ("level", vec!["INFO".into(), "WARN".into(), "DEBUG".into()]),
        (
            "message",
            vec!["connection0".into(), "query23".into(), "queue39".into()],
        ),
        ("message", vec!["connection0".into(), "nosuchterm42".into()]),
        ("message", terms17),
    ];
    for (field, terms) in &terms_battery {
        let term_refs: Vec<&str> = terms.iter().map(String::as_str).collect();
        let q = Query::terms(field, &term_refs);
        let count = searcher.count(&q)?;
        let (_, docs) = searcher.top_docs(&q, 20)?;
        out.push_str(&format!(
            "terms {field}={} count={count} first20={}\n",
            terms.join(","),
            doc_csv(&docs)
        ));
    }

    // M2 prefix battery (search spec M2 §3): "connection3" expands to 10
    // terms (<=16 OR path), "conn" to 40 (>16 bitset path); "zzzz" is the
    // zero-hit case and the trace_id prefix hits the df=1 singleton path.
    // Mirrored in VerifySearchIndex.java.
    let prefix_battery: [(&str, &str); 4] = [
        ("level", "IN"),
        ("message", "connection3"),
        ("message", "conn"),
        ("message", "zzzz"),
    ];
    for (field, prefix) in prefix_battery {
        let q = Query::prefix(field, prefix);
        let count = searcher.count(&q)?;
        let (_, docs) = searcher.top_docs(&q, 20)?;
        out.push_str(&format!(
            "prefix {field}={prefix} count={count} first20={}\n",
            doc_csv(&docs)
        ));
    }
    if num_docs > 7 {
        let tid8 = trace_id_of_doc(seed, 7)[..8].to_string();
        let q = Query::prefix("trace_id", &tid8);
        let count = searcher.count(&q)?;
        out.push_str(&format!("prefix trace_id={tid8} count={count}\n"));
    }

    // M2 wildcard battery (search spec M2 §5): "connection*" is the
    // pure-prefix shape (>16 expansion -> bitset), "que?y3*" the
    // prefix+filter shape (<=16 -> OR), "*onnection1" the no-prefix
    // full-scan shape; "*zzz" is the zero-hit case. Mirrored in
    // VerifySearchIndex.java.
    let wildcard_battery: [(&str, &str); 4] = [
        ("message", "connection*"),
        ("message", "que?y3*"),
        ("message", "*onnection1"),
        ("message", "*zzz"),
    ];
    for (field, pattern) in wildcard_battery {
        let q = Query::wildcard(field, pattern);
        let count = searcher.count(&q)?;
        let (_, docs) = searcher.top_docs(&q, 20)?;
        out.push_str(&format!(
            "wildcard {field}={pattern} count={count} first20={}\n",
            doc_csv(&docs)
        ));
    }
    // M2 phrase battery (search spec M2 §6), positions variant only: the
    // two/three-term phrases come from doc7's real adjacent tokens (a
    // guaranteed hit), the reversed pair exercises the not-adjacent case,
    // "query23 query23" the repeated-term case, the missing-term and
    // single-term items lock the degenerate behaviors. Mirrored in
    // VerifySearchIndex.java (gated on the same flag).
    if positions && num_docs > 7 {
        let toks = message_tokens_of_doc(seed, 7, 3);
        let (t0, t1, t2) = (toks[0].as_str(), toks[1].as_str(), toks[2].as_str());
        let q = Query::phrase("message", &[t0, t1]);
        let count = searcher.count(&q)?;
        let (_, docs) = searcher.top_docs(&q, 20)?;
        out.push_str(&format!(
            "phrase message={t0},{t1} count={count} first20={}\n",
            doc_csv(&docs)
        ));
        let q = Query::phrase("message", &[t0, t1, t2]);
        let count = searcher.count(&q)?;
        out.push_str(&format!("phrase message={t0},{t1},{t2} count={count}\n"));
        let q = Query::phrase("message", &[t1, t0]);
        let count = searcher.count(&q)?;
        out.push_str(&format!("phrase message={t1},{t0} count={count}\n"));
        let degenerate: [&[&str]; 3] = [
            &["query23", "query23"],
            &["connection0", "nosuchterm42"],
            &["connection0"],
        ];
        for terms in degenerate {
            let q = Query::phrase("message", terms);
            let count = searcher.count(&q)?;
            out.push_str(&format!(
                "phrase message={} count={count}\n",
                terms.join(",")
            ));
        }
    }
    print!("{out}");
    Ok(())
}

fn doc_csv(docs: &[i32]) -> String {
    let mut s = String::new();
    for d in docs {
        s.push_str(&d.to_string());
        s.push(',');
    }
    s
}

/// Search benchmark following luceneutil methodology: open index, run term
/// queries against sampled terms (bucketed by docFreq), measure QPS and
/// latency percentiles. AND/OR term pairs recorded by Java SearchBench
/// --dump-queries are replayed verbatim (no sampling) so both sides run the
/// exact same Boolean query set. ITERM work items (every TERM line, bucket
/// order then file order, no sampling) force full postings iteration via
/// DocIter instead of the doc_freq count shortcut — the pure-iteration
/// baseline, mirroring Java SearchBench's iterm type.
///
/// Uses Java SearchBench --dump-queries output as query source.
/// Output is tab-separated: query_type freq qps p50_us p90_us p99_us count
fn searchbench(
    index_dir: &Path,
    field: &str,
    warmup: u32,
    iter: u32,
    tasks: usize,
    seed: u64,
    load_queries: Option<String>,
) -> std::io::Result<()> {
    let dir = FSDirectory::open(index_dir)?;
    let mut searcher = Searcher::open(&dir)?;
    let max_doc = searcher.max_doc();

    // Read term file (format: TERM\t<freq>\t<term>\t<docFreq> from Java SearchBench --dump-queries)
    let query_file = match load_queries {
        Some(f) => f,
        None => {
            eprintln!("searchbench: --load-queries FILE is required (use Java SearchBench --dump-queries to generate)");
            std::process::exit(2);
        }
    };
    let content = std::fs::read_to_string(&query_file)?;
    let terms: Vec<(String, String, u32)> = content
        .lines()
        .filter(|l| l.starts_with("TERM\t"))
        .filter_map(|l| {
            let parts: Vec<&str> = l.split('\t').collect();
            if parts.len() >= 4 {
                Some((
                    parts[1].to_string(),
                    parts[2].to_string(),
                    parts[3].parse::<u32>().unwrap_or(0),
                ))
            } else {
                None
            }
        })
        .collect();
    if terms.is_empty() {
        eprintln!("searchbench: no TERM lines in {query_file}");
        std::process::exit(2);
    }

    // AND/OR term pairs dumped by Java SearchBench: (op, bucket, term1, term2)
    let bool_tasks: Vec<(String, String, String, String)> = content
        .lines()
        .filter(|l| l.starts_with("AND\t") || l.starts_with("OR\t"))
        .filter_map(|l| {
            let parts: Vec<&str> = l.split('\t').collect();
            if parts.len() >= 4 {
                Some((
                    parts[0].to_lowercase(),
                    parts[1].to_string(),
                    parts[2].to_string(),
                    parts[3].to_string(),
                ))
            } else {
                None
            }
        })
        .collect();

    // M2 line types (same file, replayed verbatim like AND/OR)
    let prefix_tasks: Vec<(String, String)> = content
        .lines()
        .filter(|l| l.starts_with("PREFIX\t"))
        .filter_map(|l| {
            let parts: Vec<&str> = l.split('\t').collect();
            if parts.len() >= 3 {
                Some((parts[1].to_string(), parts[2].to_string()))
            } else {
                None
            }
        })
        .collect();
    let wildcard_tasks: Vec<(String, String)> = content
        .lines()
        .filter(|l| l.starts_with("WILDCARD\t"))
        .filter_map(|l| {
            let parts: Vec<&str> = l.split('\t').collect();
            if parts.len() >= 3 {
                Some((parts[1].to_string(), parts[2].to_string()))
            } else {
                None
            }
        })
        .collect();
    let terms_tasks: Vec<(String, Vec<String>)> = content
        .lines()
        .filter(|l| l.starts_with("TERMS\t"))
        .filter_map(|l| {
            let parts: Vec<&str> = l.split('\t').collect();
            if parts.len() >= 3 {
                Some((
                    parts[1].to_string(),
                    parts[2].split(',').map(str::to_string).collect(),
                ))
            } else {
                None
            }
        })
        .collect();
    let phrase_tasks: Vec<(String, String, String)> = content
        .lines()
        .filter(|l| l.starts_with("PHRASE\t"))
        .filter_map(|l| {
            let parts: Vec<&str> = l.split('\t').collect();
            if parts.len() >= 4 {
                Some((
                    parts[1].to_string(),
                    parts[2].to_string(),
                    parts[3].to_string(),
                ))
            } else {
                None
            }
        })
        .collect();

    // M6 RANGE lines (same file, replayed verbatim like AND/OR/PHRASE):
    // RANGE\t<field>\t<low>\t<high>；low>high 行会让查询在执行期
    // Err(InvalidInput)（跨任务钉死接口），dump 侧不写这种行。
    let range_tasks: Vec<(String, i64, i64)> = content
        .lines()
        .filter(|l| l.starts_with("RANGE\t"))
        .filter_map(|l| {
            let parts: Vec<&str> = l.split('\t').collect();
            if parts.len() >= 4 {
                Some((
                    parts[1].to_string(),
                    parts[2].parse::<i64>().unwrap_or(0),
                    parts[3].parse::<i64>().unwrap_or(0),
                ))
            } else {
                None
            }
        })
        .collect();

    // Classify into freq buckets (luceneutil convention)
    let low_limit = 10u32;
    let med_limit = (max_doc as u32 / 100).max(11);

    let mut low_terms: Vec<&(String, String, u32)> =
        terms.iter().filter(|t| t.2 <= low_limit).collect();
    let mut med_terms: Vec<&(String, String, u32)> = terms
        .iter()
        .filter(|t| t.2 > low_limit && t.2 <= med_limit)
        .collect();
    let mut high_terms: Vec<&(String, String, u32)> =
        terms.iter().filter(|t| t.2 > med_limit).collect();

    // Shuffle and sample per bucket
    let mut rng = XorShift::new(seed);
    let mut shuffle_sample = |v: &mut Vec<&(String, String, u32)>| {
        // Fisher-Yates shuffle then truncate
        for i in (1..v.len()).rev() {
            let j = rng.next_int(i as u64 + 1) as usize;
            v.swap(i, j);
        }
        v.truncate(v.len().min(tasks));
    };
    shuffle_sample(&mut low_terms);
    shuffle_sample(&mut med_terms);
    shuffle_sample(&mut high_terms);

    // Build work list: (label, item)
    enum WorkItem {
        Term(String),
        And(String, String),
        Or(String, String),
        ITerm(String),
        Prefix(String),
        Wildcard(String),
        Terms(Vec<String>),
        Phrase(String, String),
        Range(String, i64, i64),
    }
    let mut work: Vec<(String, WorkItem)> = Vec::new();
    for t in &low_terms {
        work.push(("term\tlow".to_string(), WorkItem::Term(t.1.clone())));
    }
    for t in &med_terms {
        work.push(("term\tmed".to_string(), WorkItem::Term(t.1.clone())));
    }
    for t in &high_terms {
        work.push(("term\thigh".to_string(), WorkItem::Term(t.1.clone())));
    }
    // AND/OR items are used verbatim, in file order (no sampling), so they
    // line up one-to-one with the Java SearchBench --load-queries run.
    for (op, bucket, t1, t2) in &bool_tasks {
        let item = if op == "and" {
            WorkItem::And(t1.clone(), t2.clone())
        } else {
            WorkItem::Or(t1.clone(), t2.clone())
        };
        work.push((format!("{op}\t{bucket}"), item));
    }
    // ITERM items: every TERM line, grouped by bucket (low, med, high) then
    // file order within a bucket — mirroring the Java SearchBench iterm block
    // one-to-one. No sampling. Forced full postings iteration (see run_once),
    // bypassing Searcher::count's doc_freq shortcut.
    for bucket in ["low", "med", "high"] {
        for (_, t, _) in terms.iter().filter(|(b, _, _)| b == bucket) {
            work.push((format!("iterm\t{bucket}"), WorkItem::ITerm(t.clone())));
        }
    }
    // M2 line types, replayed verbatim like AND/OR. (Both sides append them
    // after the ITERM block — and the order is irrelevant anyway: the
    // per-query count lines are sorted before diffing and group aggregation
    // is order-independent.)
    for (bucket, p) in &prefix_tasks {
        work.push((format!("prefix\t{bucket}"), WorkItem::Prefix(p.clone())));
    }
    for (bucket, p) in &wildcard_tasks {
        work.push((format!("wildcard\t{bucket}"), WorkItem::Wildcard(p.clone())));
    }
    for (bucket, ts) in &terms_tasks {
        let type_label = if ts.len() > 16 { "termsbig" } else { "terms" };
        work.push((
            format!("{type_label}\t{bucket}"),
            WorkItem::Terms(ts.clone()),
        ));
    }
    for (bucket, t1, t2) in &phrase_tasks {
        work.push((
            format!("phrase\t{bucket}"),
            WorkItem::Phrase(t1.clone(), t2.clone()),
        ));
    }
    // RANGE lines: replayed verbatim, appended after the M2 line types.
    for (f, low, high) in &range_tasks {
        work.push((
            "range\tall".to_string(),
            WorkItem::Range(f.clone(), *low, *high),
        ));
    }

    if work.is_empty() {
        eprintln!("searchbench: no terms after sampling");
        std::process::exit(2);
    }

    let build_query = |item: &WorkItem| -> Query {
        match item {
            WorkItem::Term(t) | WorkItem::ITerm(t) => Query::term(field, t),
            WorkItem::And(a, b) => Query::and(field, &[a.as_str(), b.as_str()]),
            WorkItem::Or(a, b) => Query::or(field, &[a.as_str(), b.as_str()]),
            WorkItem::Prefix(p) => Query::prefix(field, p),
            WorkItem::Wildcard(p) => Query::wildcard(field, p),
            WorkItem::Terms(ts) => {
                let refs: Vec<&str> = ts.iter().map(String::as_str).collect();
                Query::terms(field, &refs)
            }
            WorkItem::Phrase(t1, t2) => Query::phrase(field, &[t1.as_str(), t2.as_str()]),
            WorkItem::Range(f, low, high) => Query::point_range(f, *low, *high),
        }
    };
    // One measured execution. ITERM forces a full DocIter walk through the
    // postings (CountCollector over Searcher::search) instead of the
    // doc_freq O(1) shortcut that Searcher::count takes for term queries —
    // the pure-iteration counterpart of Java SearchBench's iterm type.
    let run_once = |searcher: &mut Searcher, item: &WorkItem| -> std::io::Result<u64> {
        match item {
            WorkItem::ITerm(t) => {
                let q = Query::term(field, t);
                let mut c = CountCollector::default();
                searcher.search(&q, &mut c)?;
                Ok(c.count)
            }
            _ => searcher.count(&build_query(item)),
        }
    };
    // Correctness line matching the Java SearchBench stderr format verbatim.
    let detail_of = |label: &str, item: &WorkItem| -> String {
        match item {
            WorkItem::Term(t) => format!("term={t} bucket={label}"),
            WorkItem::And(a, b) => format!("and t1={a} t2={b} bucket={label}"),
            WorkItem::Or(a, b) => format!("or t1={a} t2={b} bucket={label}"),
            WorkItem::ITerm(t) => format!("iterm={t} bucket={label}"),
            WorkItem::Prefix(p) => format!("prefix={p} bucket={label}"),
            WorkItem::Wildcard(p) => format!("wildcard={p} bucket={label}"),
            WorkItem::Terms(ts) => format!("terms={} bucket={label}", ts.join(",")),
            WorkItem::Phrase(t1, t2) => format!("phrase t1={t1} t2={t2} bucket={label}"),
            WorkItem::Range(f, low, high) => {
                format!("range field={f} low={low} high={high} bucket={label}")
            }
        }
    };

    // Per-group aggregation
    let mut group_qps: std::collections::BTreeMap<String, Vec<f64>> =
        std::collections::BTreeMap::new();
    let mut group_p50: std::collections::BTreeMap<String, Vec<f64>> =
        std::collections::BTreeMap::new();
    let mut group_p90: std::collections::BTreeMap<String, Vec<f64>> =
        std::collections::BTreeMap::new();
    let mut group_p99: std::collections::BTreeMap<String, Vec<f64>> =
        std::collections::BTreeMap::new();
    let mut group_counts: std::collections::BTreeMap<String, Vec<u64>> =
        std::collections::BTreeMap::new();
    // Per-group logical file-read volume during measured iterations
    // (RL_IO_STATS-gated counters in the IndexInput layer).
    let mut group_io: std::collections::BTreeMap<String, (u64, u64)> =
        std::collections::BTreeMap::new();

    // Global warmup: run each query once to prime page cache
    for (_, item) in &work {
        let _ = run_once(&mut searcher, item)?;
    }

    let mut query_counts: Vec<(String, u64)> = Vec::with_capacity(work.len());

    for (label, item) in &work {
        // Warmup iterations
        for _ in 0..warmup {
            let _ = run_once(&mut searcher, item)?;
        }

        // Measurement iterations
        let mut latencies_ns = Vec::with_capacity(iter as usize);
        let io0 = codec_lucene9::io::io_stats::snapshot();
        for _ in 0..iter {
            let t0 = Instant::now();
            let count = run_once(&mut searcher, item)?;
            latencies_ns.push(t0.elapsed().as_nanos() as u64);
            // Store count from last iteration for correctness check
            if latencies_ns.len() == iter as usize {
                query_counts.push((detail_of(label, item), count));
            }
        }
        let io1 = codec_lucene9::io::io_stats::snapshot();
        let e = group_io.entry(label.clone()).or_default();
        e.0 += io1.0 - io0.0;
        e.1 += io1.1 - io0.1;

        latencies_ns.sort_unstable();
        let n = latencies_ns.len();
        let median_ns = if n % 2 == 0 {
            (latencies_ns[n / 2 - 1] + latencies_ns[n / 2]) as f64 / 2.0
        } else {
            latencies_ns[n / 2] as f64
        };
        let qps = 1_000_000_000.0 / median_ns;
        let p50 = percentile(&latencies_ns, 50.0) / 1000.0;
        let p90 = percentile(&latencies_ns, 90.0) / 1000.0;
        let p99 = percentile(&latencies_ns, 99.0) / 1000.0;

        group_qps.entry(label.clone()).or_default().push(qps);
        group_p50.entry(label.clone()).or_default().push(p50);
        group_p90.entry(label.clone()).or_default().push(p90);
        group_p99.entry(label.clone()).or_default().push(p99);
        group_counts
            .entry(label.clone())
            .or_default()
            .push(query_counts.last().unwrap().1);
    }

    // Print results header
    println!("query_type\tfreq\tqps\tp50_us\tp90_us\tp99_us\tcount_min\tcount_max");
    for group in group_qps.keys() {
        let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        let parts: Vec<&str> = group.split('\t').collect();
        let counts = &group_counts[group];
        let count_min = counts.iter().min().unwrap_or(&0);
        let count_max = counts.iter().max().unwrap_or(&0);
        println!(
            "{}\t{}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{}\t{}",
            parts[0],
            parts[1],
            avg(&group_qps[group]),
            avg(&group_p50[group]),
            avg(&group_p90[group]),
            avg(&group_p99[group]),
            count_min,
            count_max,
        );
    }

    // Print per-query counts for correctness diff (compare with Java SearchBench)
    eprintln!("\n# Per-query hit counts (for correctness verification vs Java)");
    for (label, count) in &query_counts {
        eprintln!("{label}\t{count}");
    }

    // Logical file-read volume per group during measured iterations (IndexInput
    // refills; includes page-cache hits, like /proc rchar).
    if codec_lucene9::io::io_stats::enabled() {
        eprintln!(
            "\n# IO stats (RL_IO_STATS): logical bytes read from files, measured iterations only"
        );
        for (group, (bytes, calls)) in &group_io {
            eprintln!("io\t{group}\tread_bytes={bytes}\tread_calls={calls}");
        }
    }

    Ok(())
}

fn percentile(sorted: &[u64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((pct / 100.0 * sorted.len() as f64).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)] as f64
}

/// Replays the log corpus generator (same RNG stream as logwrite; the
/// sparse/bigdict flags only gate whether fields are *added*, the draws are
/// identical) to recover doc `n`'s trace_id without reading stored fields.
fn trace_id_of_doc(seed: u64, n: u64) -> String {
    let vocab = vocab();
    let mut rng = XorShift::new(seed);
    let mut tid = String::new();
    for doc_id in 0..=n {
        let doc = gen_log_document(&mut rng, &vocab, doc_id, false, false);
        tid = match doc.fields.iter().find(|(name, _)| name == "trace_id") {
            Some((_, FieldValue::Keyword(k))) => k.clone(),
            _ => panic!("trace_id must be a keyword field"),
        };
    }
    tid
}

/// Replays the log corpus generator (same RNG stream as logwrite) to recover
/// the first `k` whitespace tokens of doc `n`'s message without reading
/// stored fields — the phrase battery's guaranteed-hit phrase source.
fn message_tokens_of_doc(seed: u64, n: u64, k: usize) -> Vec<String> {
    let vocab = vocab();
    let mut rng = XorShift::new(seed);
    let mut toks = Vec::new();
    for doc_id in 0..=n {
        let doc = gen_log_document(&mut rng, &vocab, doc_id, false, false);
        if let Some((_, FieldValue::Text(m))) =
            doc.fields.iter().find(|(name, _)| name == "message")
        {
            toks = m
                .split_ascii_whitespace()
                .take(k)
                .map(str::to_string)
                .collect();
        }
    }
    toks
}

/// Sharded log-schema bench, mirroring `bench` (private SegmentBuilder per
/// thread, unioned commit).
fn logbench(
    index_dir: &Path,
    num_docs: u32,
    seed: u64,
    threads: u32,
    positions: bool,
) -> std::io::Result<()> {
    let vocab = vocab();
    let per_thread = num_docs / threads;
    let name_counter = AtomicU64::new(0);
    let t0 = Instant::now();

    let results: Vec<(Vec<SegmentCommitInfo>, u64, u128, Vec<u64>)> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for tid in 0..threads {
            let vocab = &vocab;
            let name_counter = &name_counter;
            handles.push(scope.spawn(
                move || -> std::io::Result<(Vec<SegmentCommitInfo>, u64, u128, Vec<u64>)> {
                    let dir = FSDirectory::open(index_dir)?;
                    let schema = log_schema(positions, false);
                    let name = name_counter.fetch_add(1, Ordering::Relaxed);
                    let mut builder = SegmentBuilder::new(dir, name);
                    let mut rng = XorShift::new(seed.wrapping_add(tid as u64 * 0x9E3779B97F4A7C15));
                    let mut indexed_bytes = 0u64;
                    let mut scis = Vec::new();
                    // add-latency samples (every 16th add) for p50/p99 reporting
                    let mut samples: Vec<u64> = Vec::with_capacity(per_thread as usize / 16 + 2);
                    for doc_id in 0..per_thread {
                        let doc = gen_log_document(&mut rng, vocab, doc_id as u64, false, false);
                        indexed_bytes += 200 + 40; // message + trace_id payload approximation
                        let t_add = Instant::now();
                        builder.add_document(&schema, doc)?;
                        if doc_id % 16 == 0 {
                            samples.push(t_add.elapsed().as_nanos() as u64);
                        }
                    }
                    let t_flush = Instant::now();
                    if let Some(sci) = builder.finalize()? {
                        scis.push(sci);
                    }
                    Ok((scis, indexed_bytes, t_flush.elapsed().as_millis(), samples))
                },
            ));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("worker panicked"))
            .collect::<std::io::Result<Vec<_>>>()
            .expect("worker io error")
    });

    let t_commit = Instant::now();
    let mut scis: Vec<SegmentCommitInfo> = Vec::new();
    let mut indexed_bytes = 0u64;
    let mut flush_ms = 0u128;
    let mut all_samples: Vec<u64> = Vec::new();
    for (s, b, f, smp) in results {
        scis.extend(s);
        indexed_bytes += b;
        flush_ms = flush_ms.max(f);
        all_samples.extend(smp);
    }
    all_samples.sort_unstable();
    let pct = |p: usize| -> f64 {
        if all_samples.is_empty() {
            return 0.0;
        }
        all_samples[(all_samples.len() * p / 100).min(all_samples.len() - 1)] as f64 / 1000.0
    };
    let (p50_us, p99_us, max_us) = (pct(50), pct(99), pct(100));
    scis.sort_by(|a, b| a.info.name.cmp(&b.info.name));
    let mut infos = SegmentInfos::new();
    infos.segments = scis;
    infos.counter = name_counter.load(Ordering::Relaxed) as i64;
    infos.min_segment_version = Some((9, 12, 3));
    commit_segments(index_dir, infos, 1)?;
    let commit_ms = t_commit.elapsed().as_millis();

    let ms = t0.elapsed().as_millis().max(1);
    let docs = per_thread * threads;
    let docs_per_sec = docs as f64 * 1000.0 / ms as f64;
    let mb_per_sec = indexed_bytes as f64 / 1024.0 / 1024.0 / (ms as f64 / 1000.0);
    println!(
        "BENCH elapsed_ms={ms} docs_per_sec={docs_per_sec:.0} mb_per_sec={mb_per_sec:.1} indexed_bytes={indexed_bytes} flush_ms={flush_ms} commit_ms={commit_ms} add_p50_us={p50_us:.1} add_p99_us={p99_us:.1} add_max_us={max_us:.1}"
    );
    Ok(())
}

/// Single-writer log-schema indexing (the interop counterpart of JavaLogBench).
/// `bitmap` = Some(threshold) → M3 §4 inline roaring bitmaps (experimental).
fn logwrite(
    index_dir: &Path,
    num_docs: u32,
    seed: u64,
    positions: bool,
    sparse: bool,
    bigdict: bool,
    bitmap: Option<u32>,
) -> std::io::Result<()> {
    let vocab = vocab();
    let mut config = IndexWriterConfig::default();
    if let Some(t) = bitmap {
        config.bitmap = true;
        config.bitmap_threshold = t;
    }
    let mut w = IndexWriter::create(index_dir, log_schema(positions, bigdict), config)?;
    let mut rng = XorShift::new(seed);
    let t0 = Instant::now();
    for doc_id in 0..num_docs {
        w.add_document(gen_log_document(
            &mut rng,
            &vocab,
            doc_id as u64,
            sparse,
            bigdict,
        ))?;
    }
    w.commit()?;
    let ms = t0.elapsed().as_millis().max(1);
    println!(
        "WROTE docs={num_docs} elapsed_ms={ms} docs_per_sec={:.0}",
        num_docs as f64 * 1000.0 / ms as f64
    );
    Ok(())
}

/// Multi-threaded sharded bench: each thread owns a private SegmentBuilder
/// (no shared mutable state), flushed segments are unioned into a single
/// commit. Mirrors the M3 architecture; threads=1 degenerates to one builder.
fn bench(
    index_dir: &Path,
    num_docs: u32,
    doc_bytes: usize,
    seed: u64,
    threads: u32,
) -> std::io::Result<()> {
    let vocab = vocab();
    let per_thread = num_docs / threads;
    let name_counter = AtomicU64::new(0);
    let t0 = Instant::now();

    let results: Vec<(Vec<SegmentCommitInfo>, u64, u128)> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for tid in 0..threads {
            let vocab = &vocab;
            let name_counter = &name_counter;
            handles.push(scope.spawn(
                move || -> std::io::Result<(Vec<SegmentCommitInfo>, u64, u128)> {
                    let dir = FSDirectory::open(index_dir)?;
                    let schema = schema();
                    let name = name_counter.fetch_add(1, Ordering::Relaxed);
                    let mut builder = SegmentBuilder::new(dir, name);
                    let mut rng = XorShift::new(seed.wrapping_add(tid as u64 * 0x9E3779B97F4A7C15));
                    let mut indexed_bytes = 0u64;
                    let mut scis = Vec::new();
                    for _ in 0..per_thread {
                        let msg = gen_message(&mut rng, vocab, doc_bytes);
                        indexed_bytes += msg.len() as u64;
                        let mut doc = Document::new();
                        doc.add("message", FieldValue::Text(msg));
                        builder.add_document(&schema, doc)?;
                    }
                    let t_flush = Instant::now();
                    if let Some(sci) = builder.finalize()? {
                        scis.push(sci);
                    }
                    Ok((scis, indexed_bytes, t_flush.elapsed().as_millis()))
                },
            ));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("worker panicked"))
            .collect::<std::io::Result<Vec<_>>>()
            .expect("worker io error")
    });

    let t_commit = Instant::now();
    let mut scis: Vec<SegmentCommitInfo> = Vec::new();
    let mut indexed_bytes = 0u64;
    let mut flush_ms = 0u128;
    for (s, b, f) in results {
        scis.extend(s);
        indexed_bytes += b;
        flush_ms = flush_ms.max(f);
    }
    scis.sort_by(|a, b| a.info.name.cmp(&b.info.name));
    let mut infos = SegmentInfos::new();
    infos.segments = scis;
    infos.counter = name_counter.load(Ordering::Relaxed) as i64;
    infos.min_segment_version = Some((9, 12, 3));
    commit_segments(index_dir, infos, 1)?;
    let commit_ms = t_commit.elapsed().as_millis();

    let ms = t0.elapsed().as_millis().max(1);
    let docs = per_thread * threads;
    let docs_per_sec = docs as f64 * 1000.0 / ms as f64;
    let mb_per_sec = indexed_bytes as f64 / 1024.0 / 1024.0 / (ms as f64 / 1000.0);
    println!(
        "BENCH elapsed_ms={ms} docs_per_sec={docs_per_sec:.0} mb_per_sec={mb_per_sec:.1} indexed_bytes={indexed_bytes} flush_ms={flush_ms} commit_ms={commit_ms}"
    );
    Ok(())
}

fn write_docs(
    index_dir: &Path,
    num_docs: u32,
    doc_bytes: usize,
    seed: u64,
    mut golden: Option<&mut dyn Write>,
) -> std::io::Result<(u128, u64)> {
    let vocab = vocab();
    let mut w = IndexWriter::create(index_dir, schema(), IndexWriterConfig::default())?;
    let mut rng = XorShift::new(seed);
    let mut indexed_bytes = 0u64;
    let t0 = Instant::now();
    for doc_id in 0..num_docs {
        let msg = gen_message(&mut rng, &vocab, doc_bytes);
        indexed_bytes += msg.len() as u64;
        if let Some(g) = golden.as_deref_mut() {
            writeln!(g, "{doc_id}\tmessage={msg}")?;
        }
        let mut doc = Document::new();
        doc.add("message", FieldValue::Text(msg));
        w.add_document(doc)?;
    }
    w.commit()?;
    Ok((t0.elapsed().as_millis(), indexed_bytes))
}

fn write_golden_terms(
    golden: &mut dyn Write,
    num_docs: u32,
    doc_bytes: usize,
    seed: u64,
) -> std::io::Result<()> {
    // Re-generate the same corpus and accumulate postings for a sample of terms.
    use std::collections::BTreeMap;
    let vocab = vocab();
    let mut rng = XorShift::new(seed);
    let mut postings: BTreeMap<Vec<u8>, Vec<u32>> = BTreeMap::new();
    for doc_id in 0..num_docs {
        let msg = gen_message(&mut rng, &vocab, doc_bytes);
        for token in msg.split_whitespace() {
            let docs = postings.entry(token.as_bytes().to_vec()).or_default();
            if docs.last() != Some(&doc_id) {
                docs.push(doc_id);
            }
        }
    }
    // Evenly spaced sample of at most 200 terms.
    let terms: Vec<&Vec<u8>> = postings.keys().collect();
    let n = terms.len().min(200);
    writeln!(golden, "TERMS message {n}")?;
    for i in 0..n {
        let idx = if n > 1 {
            i * (terms.len() - 1) / (n - 1)
        } else {
            0
        };
        let term = terms[idx];
        let docs = &postings[term];
        let ids: Vec<String> = docs.iter().map(u32::to_string).collect();
        writeln!(
            golden,
            "{}\t{}",
            String::from_utf8_lossy(term),
            ids.join(",")
        )?;
    }
    Ok(())
}

/// Indexes text files (a single file or a directory, walked recursively in
/// sorted order): each non-empty line becomes one document with fields
/// `message` (indexed + stored, positions optional), `source` (stored-only
/// relative file path) and `line` (stored-only 1-based line number).
///
/// With `target_docs = Some(n)`, exactly n documents are written: the corpus
/// is re-read from the first file as many times as needed (cycled); without
/// it, the corpus is indexed once.
fn index_files(
    input: &Path,
    index_dir: &Path,
    positions: bool,
    target_docs: Option<u64>,
) -> std::io::Result<()> {
    let mut schema = Schema::new();
    schema.add(if positions {
        FieldSpec::text_with_positions("message")
    } else {
        FieldSpec::text("message")
    });
    schema.add(FieldSpec::stored("source"));
    schema.add(FieldSpec::stored("line"));

    let mut files = Vec::new();
    collect_files(input, &mut files)?;
    files.sort();
    if files.is_empty() {
        eprintln!("no input files under {}", input.display());
        std::process::exit(2);
    }

    let mut w = IndexWriter::create(index_dir, schema, IndexWriterConfig::default())?;
    let t0 = Instant::now();
    let (mut docs, mut skipped) = (0u64, 0u64);
    let target = target_docs.unwrap_or(u64::MAX);
    'passes: loop {
        let pass_start_docs = docs;
        for f in &files {
            let source = if input.is_dir() {
                f.strip_prefix(input)
                    .unwrap_or(f)
                    .to_string_lossy()
                    .into_owned()
            } else {
                f.file_name().unwrap().to_string_lossy().into_owned()
            };
            let reader = std::io::BufReader::new(File::open(f)?);
            let mut lineno = 0u64;
            for line in std::io::BufRead::lines(reader) {
                if docs >= target {
                    break 'passes;
                }
                // non-UTF-8 lines are skipped (lossy decoding would merge terms)
                let line = match line {
                    Ok(l) => l,
                    Err(_) => {
                        skipped += 1;
                        continue;
                    }
                };
                lineno += 1;
                // Java String.trim() strips all chars ≤ U+0020;
                // Rust str::trim() strips Unicode white_space.
                // Align with Java semantics for index compatibility.
                let text = line.trim_matches(|c: char| c <= '\x20');
                if text.is_empty() {
                    continue;
                }
                let mut doc = Document::new();
                doc.add("message", FieldValue::Text(text.to_string()));
                doc.add("source", FieldValue::Text(source.clone()));
                doc.add("line", FieldValue::Text(lineno.to_string()));
                w.add_document(doc)?;
                docs += 1;
            }
        }
        if target_docs.is_none() || docs >= target {
            break;
        }
        if docs == pass_start_docs {
            // a full pass added nothing (all lines empty/non-UTF-8):
            // cycling would never reach the target
            eprintln!(
                "input under {} has no indexable lines; cannot reach --docs {target}",
                input.display()
            );
            std::process::exit(2);
        }
    }
    w.commit()?;
    let ms = t0.elapsed().as_millis().max(1);
    println!(
        "INDEXED files={} docs={} skipped_lines={} elapsed_ms={} docs_per_sec={:.0} positions={}",
        files.len(),
        docs,
        skipped,
        ms,
        docs as f64 * 1000.0 / ms as f64,
        positions
    );
    Ok(())
}
/// Indexes a JSONL file (one flat JSON object per line) through the shared
/// schema-spec parser + JsonBinder: the same path the JNI batch API uses.
/// Unparseable/non-object lines are counted as skipped.
fn json_index(
    jsonl_file: &Path,
    index_dir: &Path,
    schema_spec: &str,
    target_docs: Option<u64>,
) -> std::io::Result<()> {
    let (schema, aliases, policy) = Schema::parse(schema_spec)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let binder = JsonBinder::new(&schema, &aliases, policy);
    let mut w = IndexWriter::create(index_dir, schema, IndexWriterConfig::default())?;
    let reader = std::io::BufReader::new(File::open(jsonl_file)?);
    let t0 = Instant::now();
    let (mut docs, mut skipped) = (0u64, 0u64);
    let target = target_docs.unwrap_or(u64::MAX);
    for line in std::io::BufRead::split(reader, b'\n') {
        if docs >= target {
            break;
        }
        let line = line?;
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        match binder.bind(w.schema_mut(), &line) {
            BindOutcome::Doc(doc, _) => {
                w.add_document(doc)?;
                docs += 1;
            }
            BindOutcome::Skip => skipped += 1,
        }
    }
    w.commit()?;
    let ms = t0.elapsed().as_millis().max(1);
    println!(
        "INDEXED docs={} skipped={} elapsed_ms={} docs_per_sec={:.0}",
        docs,
        skipped,
        ms,
        docs as f64 * 1000.0 / ms as f64
    );
    Ok(())
}

/// Deterministic JSONL corpus generator (same XorShift stream + vocab as the
/// other generators). One flat object per line, hand-assembled — message
/// characters are [a-z0-9 ] so no JSON escaping is needed. `noise_payload`
/// is deliberately absent from every schema spec: it exercises the
/// strict/dynamic unknown-field policies.
fn json_gen(out_file: &Path, num_docs: u64, seed: u64) -> std::io::Result<()> {
    let vocab = vocab();
    let mut rng = XorShift::new(seed);
    let mut out = BufWriter::new(File::create(out_file)?);
    for doc_id in 0..num_docs {
        let ts = TS_BASE + doc_id as i64 * 1000 + rng.next_int(1000) as i64;
        let level = LEVELS[rng.next_int(5) as usize];
        let trace_id = format!("t-{:016x}", rng.next());
        let message = gen_message(&mut rng, &vocab, 200);
        let latency = rng.next_int(10_000);
        let noise = rng.next_int(1_000_000);
        writeln!(
            out,
            "{{\"timestamp\":{ts},\"level\":\"{level}\",\"trace_id\":\"{trace_id}\",\"message\":\"{message}\",\"latency_ms\":{latency},\"noise_payload\":{noise}}}"
        )?;
    }
    out.flush()
}

fn collect_files(path: &Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
    if path.is_file() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        let p = entry?.path();
        if p.is_dir() {
            collect_files(&p, out)?;
        } else if p.is_file() {
            out.push(p);
        }
    }
    Ok(())
}

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  rustlucene-cli write <indexDir> <numDocs> <docBytes> <seed> [goldenFile]");
    eprintln!("  rustlucene-cli bench <indexDir> <numDocs> <docBytes> <seed> [threads]");
    eprintln!("  rustlucene-cli index <inputFileOrDir> <indexDir> [--positions] [--docs N]");
    eprintln!("  rustlucene-cli logwrite <indexDir> <numDocs> <seed> [--positions] [--sparse] [--bigdict] [--bitmap [--bitmap-threshold N]]");
    eprintln!("  rustlucene-cli logbench <indexDir> <numDocs> <seed> [threads] [--positions]");
    eprintln!("  rustlucene-cli jsonindex <jsonlFile> <indexDir> <schemaSpec> [--docs N]");
    eprintln!("  rustlucene-cli jsongen <outFile> <numDocs> <seed>");
    eprintln!("  rustlucene-cli searchdump <indexDir> <numDocs> <seed> [--positions]");
    eprintln!("  rustlucene-cli searchbench <indexDir> <field> [--warmup N] [--iter N] [--tasks N] [--seed S] [--load-queries FILE]");
    std::process::exit(2);
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }
    match args[1].as_str() {
        "write" | "bench" => {
            if args.len() < 6 {
                usage();
            }
            let index_dir = Path::new(&args[2]);
            let num_docs: u32 = args[3].parse().unwrap();
            let doc_bytes: usize = args[4].parse().unwrap();
            let seed: u64 = args[5].parse().unwrap();
            if args[1] == "write" {
                let mut golden = args
                    .get(6)
                    .map(|p| BufWriter::new(File::create(p).unwrap()));
                if let Some(g) = golden.as_mut() {
                    writeln!(g, "DOCS {num_docs}")?;
                }
                let (ms, bytes) = write_docs(
                    index_dir,
                    num_docs,
                    doc_bytes,
                    seed,
                    golden.as_mut().map(|g| g as &mut dyn Write),
                )?;
                if let Some(g) = golden.as_mut() {
                    write_golden_terms(g, num_docs, doc_bytes, seed)?;
                    g.flush()?;
                }
                println!("WROTE docs={num_docs} elapsed_ms={ms} indexed_bytes={bytes}");
                Ok(())
            } else {
                let threads: u32 = args.get(6).map(|s| s.parse().unwrap()).unwrap_or(1);
                bench(index_dir, num_docs, doc_bytes, seed, threads)
            }
        }
        "index" => {
            if args.len() < 4 {
                usage();
            }
            let mut positions = false;
            let mut docs = None;
            let mut rest = args[4..].iter();
            while let Some(a) = rest.next() {
                match a.as_str() {
                    "--positions" => positions = true,
                    "--docs" => {
                        let v = rest.next().unwrap_or_else(|| usage());
                        docs = Some(v.parse::<u64>().unwrap_or_else(|_| usage()));
                    }
                    _ => usage(),
                }
            }
            index_files(Path::new(&args[2]), Path::new(&args[3]), positions, docs)
        }
        "logwrite" => {
            if args.len() < 5 {
                usage();
            }
            let positions = args[5..].iter().any(|a| a == "--positions");
            let sparse = args[5..].iter().any(|a| a == "--sparse");
            let bigdict = args[5..].iter().any(|a| a == "--bigdict");
            let bitmap = if args[5..].iter().any(|a| a == "--bitmap") {
                let threshold = args[5..]
                    .windows(2)
                    .find_map(|w| {
                        (w[0] == "--bitmap-threshold")
                            .then(|| w[1].parse::<u32>().unwrap_or_else(|_| usage()))
                    })
                    .unwrap_or(4096)
                    // 读侧门槛固定 BITMAP_MIN_DF=4096：低于它的阈值只产永不探测的
                    // 死字节（终审 Minor 1）；clamp 到 >=4096。高于 4096 会放宽档 2
                    // 物化上界（df∈[4096,t) 的子句），正确性不受影响。
                    .max(4096);
                Some(threshold)
            } else {
                None
            };
            logwrite(
                Path::new(&args[2]),
                args[3].parse().unwrap(),
                args[4].parse().unwrap(),
                positions,
                sparse,
                bigdict,
                bitmap,
            )
        }
        "jsonindex" => {
            if args.len() < 5 {
                usage();
            }
            let mut docs = None;
            let mut rest = args[5..].iter();
            while let Some(a) = rest.next() {
                match a.as_str() {
                    "--docs" => {
                        let v = rest.next().unwrap_or_else(|| usage());
                        docs = Some(v.parse::<u64>().unwrap_or_else(|_| usage()));
                    }
                    _ => usage(),
                }
            }
            json_index(Path::new(&args[2]), Path::new(&args[3]), &args[4], docs)
        }
        "jsongen" => {
            if args.len() < 5 {
                usage();
            }
            json_gen(
                Path::new(&args[2]),
                args[3].parse().unwrap(),
                args[4].parse().unwrap(),
            )
        }
        "logbench" => {
            if args.len() < 5 {
                usage();
            }
            let threads: u32 = args.get(5).map(|s| s.parse().unwrap()).unwrap_or(1);
            let positions = args[5..].iter().any(|a| a == "--positions");
            logbench(
                Path::new(&args[2]),
                args[3].parse().unwrap(),
                args[4].parse().unwrap(),
                threads,
                positions,
            )
        }
        "searchdump" => {
            if args.len() < 5 {
                usage();
            }
            let positions = args[5..].iter().any(|a| a == "--positions");
            searchdump(
                Path::new(&args[2]),
                args[3].parse().unwrap(),
                args[4].parse().unwrap(),
                positions,
            )
        }
        "searchbench" => {
            if args.len() < 4 {
                usage();
            }
            let mut warmup = 10u32;
            let mut iter = 20u32;
            let mut tasks = 100usize;
            let mut seed = 42u64;
            let mut load_queries: Option<String> = None;
            let mut i = 4;
            while i < args.len() {
                match args[i].as_str() {
                    "--warmup" => {
                        warmup = args[i + 1].parse().unwrap();
                        i += 2;
                    }
                    "--iter" => {
                        iter = args[i + 1].parse().unwrap();
                        i += 2;
                    }
                    "--tasks" => {
                        tasks = args[i + 1].parse().unwrap();
                        i += 2;
                    }
                    "--seed" => {
                        seed = args[i + 1].parse().unwrap();
                        i += 2;
                    }
                    "--load-queries" => {
                        load_queries = Some(args[i + 1].clone());
                        i += 2;
                    }
                    _ => {
                        eprintln!("unknown arg: {}", args[i]);
                        usage();
                    }
                }
            }
            searchbench(
                Path::new(&args[2]),
                &args[3],
                warmup,
                iter,
                tasks,
                seed,
                load_queries,
            )
        }
        _ => usage(),
    }
}
