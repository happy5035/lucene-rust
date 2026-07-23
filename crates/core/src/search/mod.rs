//! Search read path (search spec §3): per-segment iteration, docID-ordered
//! and count collectors, Term/MatchAll/Boolean/multi-term queries
//! (ConstantScore semantics).

pub mod bitset;
pub mod collector;
pub mod doc_iter;
pub mod multi_term;
pub mod query;
pub mod reader;
pub mod searcher;
pub mod segment_reader;

pub use bitset::FixedBitSet;
pub use collector::{Collector, CountCollector, FreqSumCollector, TopDocCollector};
pub use doc_iter::{DocIter, MatchAllIter, SegmentDocIter};
pub use query::Query;
pub use reader::Reader;
pub use searcher::Searcher;
pub use segment_reader::SegmentReader;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Document, FieldSpec, FieldValue, IndexWriter, IndexWriterConfig, Schema};
    use codec_lucene9::FSDirectory;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rustlucene-search-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn schema() -> Schema {
        let mut s = Schema::new();
        s.add(FieldSpec::keyword("level"));
        s.add(FieldSpec::keyword("tid"));
        s.add(FieldSpec::text("message"));
        s.add(FieldSpec::stored("title"));
        s
    }

    fn doc(level: &str, tid: &str, message: &str) -> Document {
        let mut d = Document::new();
        d.add("level", FieldValue::Keyword(level.to_string()));
        d.add("tid", FieldValue::Keyword(tid.to_string()));
        d.add("message", FieldValue::Text(message.to_string()));
        d.add("title", FieldValue::Text("stored only".to_string()));
        d
    }

    #[test]
    fn term_and_matchall_single_segment() {
        let root = temp_dir("single");
        let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..10 {
            let level = if i % 2 == 0 { "INFO" } else { "WARN" };
            w.add_document(doc(level, &format!("tid-{i}"), &format!("w{} common", i % 3)))
                .unwrap();
        }
        w.commit().unwrap();
        drop(w);

        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        assert_eq!(s.max_doc(), 10);
        assert_eq!(s.segment_count(), 1);
        // term count (keyword, DOCS)
        assert_eq!(s.count(&Query::term("level", "INFO")).unwrap(), 5);
        assert_eq!(s.count(&Query::term("level", "WARN")).unwrap(), 5);
        assert_eq!(s.count(&Query::term("level", "DEBUG")).unwrap(), 0);
        // singleton
        assert_eq!(s.count(&Query::term("tid", "tid-7")).unwrap(), 1);
        let (total, docs) = s.top_docs(&Query::term("tid", "tid-7"), 10).unwrap();
        assert_eq!(total, 1);
        assert_eq!(docs, vec![7]);
        // text field (DOCS_AND_FREQS) count + topN + freqsum
        assert_eq!(s.count(&Query::term("message", "common")).unwrap(), 10);
        let (total, docs) = s.top_docs(&Query::term("message", "common"), 4).unwrap();
        assert_eq!(total, 10);
        assert_eq!(docs, vec![0, 1, 2, 3]);
        assert_eq!(s.freq_sum(&Query::term("message", "common")).unwrap(), 10);
        // matchall
        assert_eq!(s.count(&Query::MatchAll).unwrap(), 10);
        let (_, docs) = s.top_docs(&Query::MatchAll, 20).unwrap();
        assert_eq!(docs, (0..10).collect::<Vec<i32>>());
        // unknown field / stored-only field -> empty (Java TermQuery semantics)
        assert_eq!(s.count(&Query::term("nope", "x")).unwrap(), 0);
        assert_eq!(s.count(&Query::term("title", "stored")).unwrap(), 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn multi_segment_docbase_mapping() {
        let root = temp_dir("multiseg");
        let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..3 {
            w.add_document(doc("INFO", &format!("tid-{i}"), "alpha")).unwrap();
        }
        w.commit().unwrap();
        for i in 3..7 {
            w.add_document(doc("WARN", &format!("tid-{i}"), "alpha")).unwrap();
        }
        w.commit().unwrap();
        drop(w);

        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        assert_eq!(s.segment_count(), 2);
        assert_eq!(s.max_doc(), 7);
        // cross-segment term query
        assert_eq!(s.count(&Query::term("level", "INFO")).unwrap(), 3);
        assert_eq!(s.count(&Query::term("level", "WARN")).unwrap(), 4);
        let (total, docs) = s.top_docs(&Query::term("message", "alpha"), 20).unwrap();
        assert_eq!(total, 7);
        assert_eq!(docs, vec![0, 1, 2, 3, 4, 5, 6]);
        let (_, docs) = s.top_docs(&Query::MatchAll, 20).unwrap();
        assert_eq!(docs, vec![0, 1, 2, 3, 4, 5, 6]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn high_df_crosses_level1_boundary() {
        let root = temp_dir("bigdf");
        let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..5000 {
            w.add_document(doc("INFO", &format!("tid-{i}"), "alpha")).unwrap();
        }
        w.commit().unwrap();
        drop(w);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        assert_eq!(s.count(&Query::term("level", "INFO")).unwrap(), 5000);
        let (total, docs) = s.top_docs(&Query::term("level", "INFO"), 3).unwrap();
        assert_eq!(total, 5000);
        assert_eq!(docs, vec![0, 1, 2]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn and_or_single_segment() {
        let root = temp_dir("andor");
        let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..20 {
            let level = match i % 4 { 0 => "INFO", 1 => "WARN", 2 => "ERROR", _ => "DEBUG" };
            // Docs 0/8/16 carry a two-token message so message-field terms
            // co-occur and the conjunction match path is exercised; the rest
            // carry a single w{i%5} token.
            let message = if i % 8 == 0 { "w0 w1".to_string() } else { format!("w{}", i % 5) };
            w.add_document(doc(level, &format!("tid-{i}"), &message)).unwrap();
        }
        w.commit().unwrap();
        drop(w);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();

        // Hit sets: w0={0,5,8,10,15,16} w1={0,1,6,8,11,16} w2={2,7,12,17}
        //           w3={3,13,18} (doc 8 now holds "w0 w1") w4={4,9,14,19}
        // single-term AND degenerates to a Term query (level INFO = i%4==0)
        let and_q = Query::and("level", &["INFO"]);
        assert_eq!(s.count(&and_q).unwrap(), 5);
        // AND with non-empty intersection: w0 ∩ w1 = {0,8,16}
        let and_q = Query::and("message", &["w0", "w1"]);
        assert_eq!(s.count(&and_q).unwrap(), 3);
        let (total, docs) = s.top_docs(&and_q, 10).unwrap();
        assert_eq!(total, 3);
        assert_eq!(docs, vec![0, 8, 16]);
        // AND with a duplicated term intersects the term with itself
        let and_q = Query::and("message", &["w0", "w0"]);
        assert_eq!(s.count(&and_q).unwrap(), 6);
        // AND with empty intersection: w0 ∩ w3 = {}
        let and_q = Query::and("message", &["w0", "w3"]);
        assert_eq!(s.count(&and_q).unwrap(), 0);
        // AND with a missing term → empty
        let and_q = Query::and("message", &["w0", "nosuch"]);
        assert_eq!(s.count(&and_q).unwrap(), 0);
        // OR with overlapping terms dedups: |w0 ∪ w1| = 9, not 6+7
        let or_q = Query::or("message", &["w0", "w1"]);
        assert_eq!(s.count(&or_q).unwrap(), 9);
        let (total, docs) = s.top_docs(&or_q, 20).unwrap();
        assert_eq!(total, 9);
        assert_eq!(docs, vec![0, 1, 5, 6, 8, 10, 11, 15, 16]);
        // OR with disjoint terms: |w0 ∪ w2| = 6+4
        let or_q = Query::or("message", &["w0", "w2"]);
        assert_eq!(s.count(&or_q).unwrap(), 10);
        // OR with one missing term → still matches the other
        let or_q = Query::or("message", &["w0", "nosuch"]);
        assert_eq!(s.count(&or_q).unwrap(), 6);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn empty_index() {
        let root = temp_dir("empty");
        let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
        w.commit().unwrap();
        drop(w);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        assert_eq!(s.max_doc(), 0);
        assert_eq!(s.segment_count(), 0);
        assert_eq!(s.count(&Query::MatchAll).unwrap(), 0);
        assert_eq!(s.count(&Query::term("level", "INFO")).unwrap(), 0);
        let (total, docs) = s.top_docs(&Query::MatchAll, 10).unwrap();
        assert_eq!(total, 0);
        assert!(docs.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    /// 40 docs; doc i carries tokens t(i%20) and t((i+7)%20) — every t-term
    /// has df=4 and the term sets overlap, so union sizes are non-trivial.
    fn write_terms_corpus(root: &std::path::Path) {
        let mut w = IndexWriter::create(root, schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..40 {
            let m = format!("t{:02} t{:02}", i % 20, (i + 7) % 20);
            w.add_document(doc("INFO", &format!("tid-{i}"), &m)).unwrap();
        }
        w.commit().unwrap();
        drop(w);
    }

    fn t_terms(range: std::ops::Range<usize>) -> Vec<String> {
        range.map(|i| format!("t{i:02}")).collect()
    }

    #[test]
    fn terms_query_matches_or_on_both_paths() {
        let root = temp_dir("termsdual");
        write_terms_corpus(&root);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        let all = t_terms(0..20);
        let all_ref: Vec<&str> = all.iter().map(String::as_str).collect();

        // >16 terms -> bitset path; result must equal the literal OR query
        let or_q = Query::or("message", &all_ref);
        let (or_total, or_docs) = s.top_docs(&or_q, 100).unwrap();
        let terms_q = Query::terms("message", &all_ref);
        let (t_total, t_docs) = s.top_docs(&terms_q, 100).unwrap();
        assert_eq!((or_total, or_docs.clone()), (t_total, t_docs));
        assert_eq!(s.count(&terms_q).unwrap(), or_total);
        assert_eq!(s.count(&terms_q).unwrap(), 40); // every doc has two t-terms

        // exactly 16 terms -> OR rewrite path, still equal to OR
        let t16: Vec<&str> = all_ref[..16].to_vec();
        let or16 = Query::or("message", &t16);
        let terms16 = Query::terms("message", &t16);
        let (a_total, a_docs) = s.top_docs(&or16, 100).unwrap();
        let (b_total, b_docs) = s.top_docs(&terms16, 100).unwrap();
        assert_eq!((a_total, a_docs), (b_total, b_docs));
        assert_eq!(s.count(&terms16).unwrap(), a_total);

        // 17 terms -> bitset path boundary
        let t17: Vec<&str> = all_ref[..17].to_vec();
        let or17 = Query::or("message", &t17);
        let terms17 = Query::terms("message", &t17);
        let (a_total, a_docs) = s.top_docs(&or17, 100).unwrap();
        let (b_total, b_docs) = s.top_docs(&terms17, 100).unwrap();
        assert_eq!((a_total, a_docs), (b_total, b_docs));
        assert_eq!(s.count(&terms17).unwrap(), a_total);

        // dedup: docs carrying two of the terms are counted once
        let dup = Query::terms("message", &["t00", "t07"]); // doc 0 has both
        let (total, docs) = s.top_docs(&dup, 100).unwrap();
        assert_eq!(docs.iter().filter(|&&d| d == 0).count(), 1);
        assert_eq!(total as usize, docs.len());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn terms_query_edge_cases() {
        let root = temp_dir("termsedge");
        write_terms_corpus(&root);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        // all terms missing -> 0
        assert_eq!(s.count(&Query::terms("message", &["zz1", "zz2"])).unwrap(), 0);
        // mixed present/missing
        let (total, _) = s.top_docs(&Query::terms("message", &["t00", "zz1"]), 100).unwrap();
        assert_eq!(total, 4); // df(t00) = 4
        // empty term set -> 0
        assert_eq!(s.count(&Query::terms("message", &[])).unwrap(), 0);
        // single term degenerates to a Term query
        assert_eq!(s.count(&Query::terms("message", &["t00"])).unwrap(), 4);
        // keyword field (DOCS layout)
        assert_eq!(s.count(&Query::terms("level", &["INFO", "WARN"])).unwrap(), 40);
        // unknown / stored-only fields -> empty
        assert_eq!(s.count(&Query::terms("nope", &["x"])).unwrap(), 0);
        assert_eq!(s.count(&Query::terms("title", &["stored"])).unwrap(), 0);
        // duplicated input terms are harmless
        assert_eq!(s.count(&Query::terms("message", &["t00", "t00"])).unwrap(), 4);
        // >16 on the keyword field too (bitset over DOCS postings)
        let tids: Vec<String> = (0..18).map(|i| format!("tid-{i}")).collect();
        let tids_ref: Vec<&str> = tids.iter().map(String::as_str).collect();
        assert_eq!(s.count(&Query::terms("tid", &tids_ref)).unwrap(), 18);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn terms_query_multi_segment() {
        let root = temp_dir("termsseg");
        let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..3 {
            w.add_document(doc("INFO", &format!("tid-{i}"), "alpha")).unwrap();
        }
        w.commit().unwrap();
        for i in 3..7 {
            w.add_document(doc("WARN", &format!("tid-{i}"), "beta")).unwrap();
        }
        w.commit().unwrap();
        drop(w);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        assert_eq!(s.segment_count(), 2);
        let q = Query::terms("level", &["INFO", "WARN"]);
        assert_eq!(s.count(&q).unwrap(), 7);
        let (_, docs) = s.top_docs(&q, 20).unwrap();
        assert_eq!(docs, vec![0, 1, 2, 3, 4, 5, 6]);
        let q = Query::terms("message", &["alpha", "beta"]);
        assert_eq!(s.count(&q).unwrap(), 7);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prefix_query_dual_path() {
        let root = temp_dir("prefixdual");
        write_terms_corpus(&root);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        // "t1" -> t10..t19 (10 terms, <=16 OR path); equal to the literal OR
        let t1 = t_terms(10..20);
        let t1_ref: Vec<&str> = t1.iter().map(String::as_str).collect();
        let or_q = Query::or("message", &t1_ref);
        let (a_total, a_docs) = s.top_docs(&or_q, 100).unwrap();
        let pq = Query::prefix("message", "t1");
        let (b_total, b_docs) = s.top_docs(&pq, 100).unwrap();
        assert_eq!((a_total, a_docs), (b_total, b_docs));
        assert_eq!(s.count(&pq).unwrap(), a_total);
        // "t" -> all 20 terms (>16 bitset path); every doc has two t-terms
        let pq = Query::prefix("message", "t");
        assert_eq!(s.count(&pq).unwrap(), 40);
        // empty prefix enumerates the whole field dictionary
        let pq = Query::prefix("message", "");
        assert_eq!(s.count(&pq).unwrap(), 40);
        // exact-term prefix
        let pq = Query::prefix("message", "t07");
        assert_eq!(s.count(&pq).unwrap(), 4);
        // zero-hit prefix / unknown field
        assert_eq!(s.count(&Query::prefix("message", "zzz")).unwrap(), 0);
        assert_eq!(s.count(&Query::prefix("nope", "t")).unwrap(), 0);
        // keyword field (DOCS layout)
        assert_eq!(s.count(&Query::prefix("level", "INF")).unwrap(), 40);
        let pq = Query::prefix("tid", "tid-1");
        let (total, docs) = s.top_docs(&pq, 100).unwrap();
        assert_eq!(total as usize, docs.len());
        assert!(docs.contains(&1) && docs.iter().all(|&d| d == 1 || (10..=19).contains(&d)));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn wildcard_query_classes() {
        let root = temp_dir("wildcards");
        let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..40 {
            let m = format!("t{:02} t{:02}", i % 20, (i + 7) % 20);
            w.add_document(doc("INFO", &format!("tid-{i}"), &m)).unwrap();
        }
        w.add_document(doc("INFO", "tid-x", "héllo world")).unwrap();
        w.commit().unwrap();
        drop(w);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();

        // pure-prefix shape == the prefix query result (zero filtering)
        let wq = Query::wildcard("message", "t1*");
        let pq = Query::prefix("message", "t1");
        assert_eq!(s.count(&wq).unwrap(), s.count(&pq).unwrap());
        let (a, ad) = s.top_docs(&wq, 100).unwrap();
        let (b, bd) = s.top_docs(&pq, 100).unwrap();
        assert_eq!((a, ad), (b, bd));
        // prefix + wildcard filter: t?7 matches t07,t17 (and t27... but dict has t00..t19)
        let wq = Query::wildcard("message", "t?7");
        let t = t_terms(0..20);
        let hits: Vec<&str> = t.iter().map(String::as_str).filter(|x| x.len() == 3 && x.ends_with('7')).collect();
        let or_q = Query::or("message", &hits);
        assert_eq!(s.count(&wq).unwrap(), s.count(&or_q).unwrap());
        // no-prefix full scan: *7 same term set
        let wq = Query::wildcard("message", "*7");
        assert_eq!(s.count(&wq).unwrap(), s.count(&or_q).unwrap());
        // "*" matches every term in the field dictionary
        let wq = Query::wildcard("message", "*");
        assert_eq!(s.count(&wq).unwrap(), 41);
        // exact degenerate
        let wq = Query::wildcard("message", "t07");
        assert_eq!(s.count(&wq).unwrap(), 4);
        // '?' over a multi-byte char (héllo)
        let wq = Query::wildcard("message", "h?llo");
        assert_eq!(s.count(&wq).unwrap(), 1);
        let wq = Query::wildcard("message", "h?ll");
        assert_eq!(s.count(&wq).unwrap(), 0);
        // zero hit / unknown field
        assert_eq!(s.count(&Query::wildcard("message", "zzz*")).unwrap(), 0);
        assert_eq!(s.count(&Query::wildcard("nope", "*")).unwrap(), 0);
        fs::remove_dir_all(&root).unwrap();
    }
}
