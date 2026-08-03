//! rl-index: standalone index & search tool for directories / log files.
//!
//! Subcommands:
//!   rl-index index  <inputFileOrDir> <indexDir> [options]
//!   rl-index search <indexDir> <query> [options]
//!   rl-index stats  <indexDir>
//!
//! Indexing model: every non-empty line becomes one document with fields
//!   message  — indexed text (+ stored), positions optional
//!   source   — stored relative file path
//!   line     — stored 1-based line number
//!
//! The produced index is a standard Lucene90-format segment set readable by
//! any tool in this project (Searcher, forceMerge, Java Lucene 9.12.3).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Instant;

use codec_lucene9::segment_infos::SegmentInfos;
use codec_lucene9::stored_fields::{StoredField, StoredFieldsReader};
use codec_lucene9::{field_infos::FieldInfos, FSDirectory};
use rustlucene_core::search::{Query, Searcher};
use rustlucene_core::{Document, FieldSpec, FieldValue, IndexWriter, IndexWriterConfig, Schema};

// ─── index ──────────────────────────────────────────────────────────────────

fn cmd_index(args: &[String]) -> std::io::Result<()> {
    if args.len() < 2 {
        usage();
    }
    let input = Path::new(&args[0]);
    let index_dir = Path::new(&args[1]);
    let mut positions = false;
    let mut overwrite = false;
    let mut max_docs: Option<u64> = None;
    let mut includes: Vec<String> = Vec::new();
    let mut max_buffered: u32 = 1_000_000;

    let mut rest = args[2..].iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--positions" => positions = true,
            "--overwrite" => overwrite = true,
            "--docs" => max_docs = Some(parse_next(&mut rest, "--docs")),
            "--include" => {
                let v = next_str(&mut rest, "--include");
                includes.extend(v.split(',').map(|s| s.trim().to_string()));
            }
            "--max-buffered" => max_buffered = parse_next(&mut rest, "--max-buffered"),
            _ => {
                eprintln!("unknown option: {a}");
                usage();
            }
        }
    }

    if !input.exists() {
        eprintln!("error: input path does not exist: {}", input.display());
        std::process::exit(2);
    }

    // Collect input files
    let mut files = Vec::new();
    collect_files(input, &mut files)?;
    if !includes.is_empty() {
        files.retain(|f| {
            let name = f.file_name().unwrap().to_string_lossy();
            includes.iter().any(|p| glob_match(&name.to_lowercase(), &p.to_lowercase()))
        });
    }
    files.sort();
    if files.is_empty() {
        eprintln!(
            "error: no input files under {} (check --include patterns)",
            input.display()
        );
        std::process::exit(2);
    }

    // Prepare index dir
    if overwrite && index_dir.exists() {
        for entry in std::fs::read_dir(index_dir)? {
            let p = entry?.path();
            if p.is_file() {
                std::fs::remove_file(p)?;
            }
        }
    }
    std::fs::create_dir_all(index_dir)?;

    let mut schema = Schema::new();
    schema.add(if positions {
        FieldSpec::text_with_positions("message")
    } else {
        FieldSpec::text("message")
    });
    schema.add(FieldSpec::stored("source"));
    schema.add(FieldSpec::stored("line"));

    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = max_buffered;

    let mut w = IndexWriter::create(index_dir, schema, config)?;
    let t0 = Instant::now();
    let mut docs = 0u64;
    let mut skipped = 0u64;
    let mut bytes = 0u64;
    let target = max_docs.unwrap_or(u64::MAX);

    'outer: for (fi, f) in files.iter().enumerate() {
        let source = if input.is_dir() {
            f.strip_prefix(input).unwrap_or(f).to_string_lossy().into_owned()
        } else {
            f.file_name().unwrap().to_string_lossy().into_owned()
        };
        let reader = BufReader::with_capacity(1 << 20, File::open(f)?);
        let mut lineno = 0u64;
        for line in reader.lines() {
            if docs >= target {
                break 'outer;
            }
            let line = match line {
                Ok(l) => l,
                Err(_) => {
                    skipped += 1; // non-UTF-8 line
                    continue;
                }
            };
            lineno += 1;
            let text = line.trim_matches(|c: char| c <= '\x20');
            if text.is_empty() {
                continue;
            }
            let mut doc = Document::new();
            doc.add("message", FieldValue::Text(text.to_string()));
            doc.add("source", FieldValue::Text(source.clone()));
            doc.add("line", FieldValue::Text(lineno.to_string()));
            bytes += text.len() as u64;
            w.add_document(doc)?;
            docs += 1;
            if docs % 1_000_000 == 0 {
                eprintln!("... {docs} docs indexed");
            }
        }
        if (fi + 1) % 1000 == 0 {
            eprintln!("... {}/{} files", fi + 1, files.len());
        }
    }
    w.commit()?;
    let ms = t0.elapsed().as_millis().max(1);
    println!(
        "INDEXED files={} docs={} skipped_lines={} bytes={} elapsed_ms={} docs_per_sec={:.0}",
        files.len(),
        docs,
        skipped,
        bytes,
        ms,
        docs as f64 * 1000.0 / ms as f64
    );
    println!("index dir: {}", index_dir.display());
    Ok(())
}

// ─── search ─────────────────────────────────────────────────────────────────

fn cmd_search(args: &[String]) -> std::io::Result<()> {
    if args.len() < 2 {
        usage();
    }
    let index_dir = Path::new(&args[0]);
    let query_str = &args[1];
    let mut top = 10usize;
    let mut field = "message".to_string();
    let mut mode = String::from("and"); // and | or | phrase | term

    let mut rest = args[2..].iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--top" => top = parse_next::<u64>(&mut rest, "--top") as usize,
            "--field" => field = next_str(&mut rest, "--field"),
            "--mode" => {
                mode = next_str(&mut rest, "--mode");
                if !["and", "or", "phrase", "term"].contains(&mode.as_str()) {
                    eprintln!("--mode must be and|or|phrase|term");
                    std::process::exit(2);
                }
            }
            _ => {
                eprintln!("unknown option: {a}");
                usage();
            }
        }
    }

    let dir = FSDirectory::open(index_dir)?;
    let mut searcher = Searcher::open(&dir)?;

    let terms: Vec<&str> = query_str.split_whitespace().collect();
    if terms.is_empty() {
        eprintln!("error: empty query");
        std::process::exit(2);
    }
    let q = if terms.len() == 1 && mode != "term" {
        Query::term(&field, terms[0])
    } else {
        match mode.as_str() {
            "or" => Query::or(&field, &terms),
            "phrase" => Query::phrase(&field, &terms),
            "term" => {
                // whole-string term (keyword semantics on a text field: only
                // matches if the exact token sequence was indexed as one term)
                Query::term(&field, query_str)
            }
            _ => Query::and(&field, &terms),
        }
    };

    let t0 = Instant::now();
    let (total, docs) = searcher.top_docs(&q, top)?;
    let q_us = t0.elapsed().as_secs_f64() * 1e6;
    println!(
        "query=\"{}\" field={} mode={} total_hits={} top_shown={} query_us={:.1}",
        query_str,
        field,
        mode,
        total,
        docs.len(),
        q_us
    );

    // Stored-field retrieval: map global docIDs back to segments.
    let (infos, _) = SegmentInfos::read_latest(&dir)?;
    let mut bases: Vec<(i32, i32, usize)> = Vec::new(); // (base, doc_count, seg_idx)
    let mut base = 0i32;
    for (i, sci) in infos.segments.iter().enumerate() {
        bases.push((base, sci.info.doc_count, i));
        base += sci.info.doc_count;
    }
    // Lazily opened per-segment readers
    let mut sfr_cache: Vec<Option<StoredFieldsReader>> =
        (0..infos.segments.len()).map(|_| None).collect();
    let mut fis_cache: Vec<Option<FieldInfos>> =
        (0..infos.segments.len()).map(|_| None).collect();

    for (rank, &global_doc) in docs.iter().enumerate() {
        let Some(&(seg_base, _doc_count, seg_idx)) = bases
            .iter()
            .find(|(b, c, _)| global_doc >= *b && global_doc < b + c)
        else {
            println!("[{rank}] doc={global_doc} (segment not found)");
            continue;
        };
        let local = (global_doc - seg_base) as u32;
        let sci = &infos.segments[seg_idx];
        if sfr_cache[seg_idx].is_none() {
            sfr_cache[seg_idx] =
                Some(StoredFieldsReader::open(&dir, &sci.info.name, &sci.info.id)?);
            fis_cache[seg_idx] =
                Some(FieldInfos::read(&dir, &sci.info.name, &sci.info.id, "")?);
        }
        let sfr = sfr_cache[seg_idx].as_ref().unwrap();
        let fis = fis_cache[seg_idx].as_ref().unwrap();
        let fields = sfr.document(local)?;
        let get = |name: &str| -> String {
            for (num, val) in &fields {
                if let Some(fi) = fis.by_number(*num as i32) {
                    if fi.name == name {
                        return match val {
                            StoredField::String(s) => s.clone(),
                            StoredField::Long(v) => v.to_string(),
                            StoredField::Int(v) => v.to_string(),
                            StoredField::Float(v) => v.to_string(),
                            StoredField::Double(v) => v.to_string(),
                            StoredField::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                        };
                    }
                }
            }
            String::new()
        };
        let source = get("source");
        let line = get("line");
        let message = get("message");
        let snippet: String = message.chars().take(200).collect();
        println!("[{rank}] doc={global_doc} {source}:{line}");
        println!("    {snippet}");
    }
    Ok(())
}

// ─── stats ──────────────────────────────────────────────────────────────────

fn cmd_stats(args: &[String]) -> std::io::Result<()> {
    if args.is_empty() {
        usage();
    }
    let index_dir = Path::new(&args[0]);
    let dir = FSDirectory::open(index_dir)?;
    let searcher = Searcher::open(&dir)?;
    let (infos, generation) = SegmentInfos::read_latest(&dir)?;
    println!("index: {}", index_dir.display());
    println!("commit: segments_{generation}");
    println!("max_doc: {}", searcher.max_doc());
    println!("segments: {}", infos.segments.len());
    for sci in &infos.segments {
        println!(
            "  {} docs={}",
            sci.info.name, sci.info.doc_count
        );
    }
    // File listing with sizes
    let mut total_bytes = 0u64;
    let mut entries: Vec<(String, u64)> = Vec::new();
    for name in dir.list_all()? {
        let p = dir.path().join(&name);
        let sz = std::fs::metadata(&p)?.len();
        total_bytes += sz;
        entries.push((name, sz));
    }
    entries.sort();
    println!("files: {}  total: {:.2} MB", entries.len(), total_bytes as f64 / 1e6);
    for (name, sz) in entries {
        println!("  {name:<32} {sz:>12}");
    }
    Ok(())
}

// ─── helpers ────────────────────────────────────────────────────────────────

/// Simple wildcard match supporting `*` (any sequence) and `?` (one char).
fn glob_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star_t, mut star_p) = (0usize, usize::MAX);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = pi;
            star_t = ti;
            pi += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn collect_files(path: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
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

fn parse_next<T: std::str::FromStr>(rest: &mut std::slice::Iter<'_, String>, flag: &str) -> T {
    match rest.next().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => {
            eprintln!("missing or invalid value for {flag}");
            std::process::exit(2);
        }
    }
}

fn next_str<'a>(rest: &mut std::slice::Iter<'a, String>, flag: &str) -> String {
    match rest.next() {
        Some(s) => s.clone(),
        None => {
            eprintln!("missing value for {flag}");
            std::process::exit(2);
        }
    }
}

fn usage() -> ! {
    eprintln!(
        r#"rl-index — 目录/日志文件索引工具（Lucene90 格式）

用法:
  rl-index index  <文件或目录> <索引目录> [选项]
  rl-index search <索引目录> <查询词> [选项]
  rl-index stats  <索引目录>

index 选项:
  --include <模式>     文件名过滤，逗号分隔，如 "*.log,*.txt"（支持 * 和 ?）
  --positions          启用 positions（支持短语查询）
  --docs <N>           最多索引 N 个文档
  --max-buffered <N>   内存缓冲文档数（默认 1000000）
  --overwrite          清空索引目录中已有文件后重建

search 选项:
  --field <名称>       查询字段（默认 message）
  --mode <模式>        and | or | phrase | term（默认 and）
  --top <N>            返回前 N 条（默认 10）

示例:
  rl-index index ./logs ./idx --include "*.log"
  rl-index search ./idx "connection timeout" --mode and --top 5
  rl-index stats ./idx

索引模型: 每行一个文档 — message(全文索引+存储), source(相对路径), line(行号)。
注意: phrase 模式需要索引时使用 --positions。
"#
    );
    std::process::exit(2);
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    match args[0].as_str() {
        "index" => cmd_index(&args[1..]),
        "search" => cmd_search(&args[1..]),
        "stats" => cmd_stats(&args[1..]),
        "-h" | "--help" | "help" => usage(),
        other => {
            eprintln!("unknown subcommand: {other}");
            usage();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("app.log", "*.log"));
        assert!(glob_match("app.log", "app.*"));
        assert!(glob_match("app.log", "a?p.log"));
        assert!(glob_match("app.log", "*"));
        assert!(!glob_match("app.txt", "*.log"));
        assert!(!glob_match("app.log", "*.txt"));
        assert!(glob_match("2024-01-01.log", "2024-*.log"));
        assert!(!glob_match("2023-12-31.log", "2024-*.log"));
    }
}
