//! Search read path (search spec §3): per-segment iteration, docID-ordered
//! and count collectors, Term/MatchAll/Boolean(And/Or/Bool)/multi-term queries
//! (ConstantScore semantics).

pub mod bitset;
pub mod collector;
pub mod doc_iter;
pub mod multi_term;
pub mod query;
pub mod reader;
pub(crate) mod roaring_exec;
pub mod searcher;
pub mod segment_reader;

pub use bitset::FixedBitSet;
pub use collector::{Collector, CountCollector, FreqSumCollector, TopDocCollector};
pub use doc_iter::{DocIter, MatchAllIter, SegmentDocIter};
pub use query::{Occur, Query};
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
        let dir =
            std::env::temp_dir().join(format!("rustlucene-search-{}-{}", tag, std::process::id()));
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
            w.add_document(doc(
                level,
                &format!("tid-{i}"),
                &format!("w{} common", i % 3),
            ))
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
            w.add_document(doc("INFO", &format!("tid-{i}"), "alpha"))
                .unwrap();
        }
        w.commit().unwrap();
        for i in 3..7 {
            w.add_document(doc("WARN", &format!("tid-{i}"), "alpha"))
                .unwrap();
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
            w.add_document(doc("INFO", &format!("tid-{i}"), "alpha"))
                .unwrap();
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
            let level = match i % 4 {
                0 => "INFO",
                1 => "WARN",
                2 => "ERROR",
                _ => "DEBUG",
            };
            // Docs 0/8/16 carry a two-token message so message-field terms
            // co-occur and the conjunction match path is exercised; the rest
            // carry a single w{i%5} token.
            let message = if i % 8 == 0 {
                "w0 w1".to_string()
            } else {
                format!("w{}", i % 5)
            };
            w.add_document(doc(level, &format!("tid-{i}"), &message))
                .unwrap();
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
            w.add_document(doc("INFO", &format!("tid-{i}"), &m))
                .unwrap();
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
        assert_eq!(
            s.count(&Query::terms("message", &["zz1", "zz2"])).unwrap(),
            0
        );
        // mixed present/missing
        let (total, _) = s
            .top_docs(&Query::terms("message", &["t00", "zz1"]), 100)
            .unwrap();
        assert_eq!(total, 4); // df(t00) = 4
                              // empty term set -> 0
        assert_eq!(s.count(&Query::terms("message", &[])).unwrap(), 0);
        // single term degenerates to a Term query
        assert_eq!(s.count(&Query::terms("message", &["t00"])).unwrap(), 4);
        // keyword field (DOCS layout)
        assert_eq!(
            s.count(&Query::terms("level", &["INFO", "WARN"])).unwrap(),
            40
        );
        // unknown / stored-only fields -> empty
        assert_eq!(s.count(&Query::terms("nope", &["x"])).unwrap(), 0);
        assert_eq!(s.count(&Query::terms("title", &["stored"])).unwrap(), 0);
        // duplicated input terms are harmless
        assert_eq!(
            s.count(&Query::terms("message", &["t00", "t00"])).unwrap(),
            4
        );
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
            w.add_document(doc("INFO", &format!("tid-{i}"), "alpha"))
                .unwrap();
        }
        w.commit().unwrap();
        for i in 3..7 {
            w.add_document(doc("WARN", &format!("tid-{i}"), "beta"))
                .unwrap();
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
            w.add_document(doc("INFO", &format!("tid-{i}"), &m))
                .unwrap();
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
        let hits: Vec<&str> = t
            .iter()
            .map(String::as_str)
            .filter(|x| x.len() == 3 && x.ends_with('7'))
            .collect();
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

    fn schema_pos() -> Schema {
        let mut s = Schema::new();
        s.add(FieldSpec::keyword("level"));
        s.add(FieldSpec::keyword("tid"));
        s.add(FieldSpec::text_with_positions("message"));
        s
    }

    fn pos_doc(level: &str, tid: &str, message: &str) -> Document {
        let mut d = Document::new();
        d.add("level", FieldValue::Keyword(level.to_string()));
        d.add("tid", FieldValue::Keyword(tid.to_string()));
        d.add("message", FieldValue::Text(message.to_string()));
        d
    }

    fn write_phrase_corpus(root: &std::path::Path) {
        let mut w = IndexWriter::create(root, schema_pos(), IndexWriterConfig::default()).unwrap();
        let docs = [
            "quick brown fox",   // 0: "quick brown" hit
            "quick fox brown",   // 1: not adjacent
            "quick quick brown", // 2: only the 2nd quick aligns
            "foo foo bar",       // 3: "foo foo" hit
            "foo bar foo",       // 4: "foo foo" miss
            "a b c",             // 5: 3-term phrase hit
            "a b",               // 6
            "c a b",             // 7: "a b" hit
        ];
        for (i, m) in docs.iter().enumerate() {
            w.add_document(pos_doc("INFO", &format!("tid-{i}"), m))
                .unwrap();
        }
        w.commit().unwrap();
        drop(w);
    }

    #[test]
    fn phrase_query_positions() {
        let root = temp_dir("phrase");
        write_phrase_corpus(&root);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        // adjacent / not-adjacent
        let q = Query::phrase("message", &["quick", "brown"]);
        assert_eq!(s.count(&q).unwrap(), 2);
        let (_, docs) = s.top_docs(&q, 10).unwrap();
        assert_eq!(docs, vec![0, 2]);
        let q = Query::phrase("message", &["quick", "fox"]);
        let (_, docs) = s.top_docs(&q, 10).unwrap();
        assert_eq!(docs, vec![1]);
        // same doc, multiple candidate occurrences, only one aligns
        let q = Query::phrase("message", &["quick", "quick", "brown"]);
        let (_, docs) = s.top_docs(&q, 10).unwrap();
        assert_eq!(docs, vec![2]);
        // repeated term needs two adjacent occurrences
        let q = Query::phrase("message", &["foo", "foo"]);
        let (_, docs) = s.top_docs(&q, 10).unwrap();
        assert_eq!(docs, vec![3]);
        // 3-term phrase
        let q = Query::phrase("message", &["a", "b", "c"]);
        let (_, docs) = s.top_docs(&q, 10).unwrap();
        assert_eq!(docs, vec![5]);
        // cross-doc terms never merge
        let q = Query::phrase("message", &["a", "b"]);
        let (_, docs) = s.top_docs(&q, 10).unwrap();
        assert_eq!(docs, vec![5, 6, 7]);
        // reversed order misses
        assert_eq!(
            s.count(&Query::phrase("message", &["brown", "quick"]))
                .unwrap(),
            0
        );
        // single term degenerates to a Term query
        let q = Query::phrase("message", &["quick"]);
        assert_eq!(s.count(&q).unwrap(), 3);
        // missing term -> no hits (not an error)
        assert_eq!(
            s.count(&Query::phrase("message", &["quick", "nosuch"]))
                .unwrap(),
            0
        );
        // unknown field -> no hits (not an error)
        assert_eq!(s.count(&Query::phrase("message", &[])).unwrap(), 0);
        assert_eq!(s.count(&Query::phrase("nope", &["a", "b"])).unwrap(), 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn phrase_query_requires_positions() {
        // the M1 schema() has message as DOCS_AND_FREQS (no positions):
        // phrase must fail fast, mirroring Java's execution-time error
        let root = temp_dir("phrasefail");
        let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
        w.add_document(doc("INFO", "tid-0", "quick brown")).unwrap();
        w.commit().unwrap();
        drop(w);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        let err = s
            .count(&Query::phrase("message", &["quick", "brown"]))
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        // keyword field (DOCS) also fails
        let err = s
            .count(&Query::phrase("level", &["INFO", "WARN"]))
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn freq_sum_rejects_multi_term_and_boolean_queries() {
        let root = temp_dir("freqsumerr");
        write_terms_corpus(&root);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        let all = t_terms(0..20);
        let all_ref: Vec<&str> = all.iter().map(String::as_str).collect();
        // Terms, <=16 (OR path) and >16 (bitset path)
        let t16: Vec<&str> = all_ref[..16].to_vec();
        let err = s.freq_sum(&Query::terms("message", &t16)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let err = s.freq_sum(&Query::terms("message", &all_ref)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        // Prefix / Wildcard (multi-term too)
        assert!(s.freq_sum(&Query::prefix("message", "t")).is_err());
        assert!(s.freq_sum(&Query::wildcard("message", "t*")).is_err());
        // And / Or
        let err = s
            .freq_sum(&Query::or("message", &["t00", "t01"]))
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let err = s
            .freq_sum(&Query::and("message", &["t00", "t07"]))
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        // Term still works
        assert_eq!(s.freq_sum(&Query::term("message", "t00")).unwrap(), 4);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn phrase_query_multi_segment() {
        let root = temp_dir("phraseseg");
        let mut w = IndexWriter::create(&root, schema_pos(), IndexWriterConfig::default()).unwrap();
        w.add_document(pos_doc("INFO", "tid-0", "quick brown"))
            .unwrap();
        w.commit().unwrap();
        w.add_document(pos_doc("INFO", "tid-1", "brown quick"))
            .unwrap();
        w.add_document(pos_doc("INFO", "tid-2", "quick brown fox"))
            .unwrap();
        w.commit().unwrap();
        drop(w);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        assert_eq!(s.segment_count(), 2);
        let q = Query::phrase("message", &["quick", "brown"]);
        let (total, docs) = s.top_docs(&q, 10).unwrap();
        assert_eq!(total, 2);
        assert_eq!(docs, vec![0, 2]);
        fs::remove_dir_all(&root).unwrap();
    }

    /// M3 写侧：同一语料 bitmap off/on 两个索引，所有查询路径结果逐位一致；
    /// bitmap 索引的 .doc 严格更大（缝隙字节确实写入）。读侧 roaring 接入在
    /// T4/T5，这里验证的是"开了 --bitmap 写，既有读路径（校验失败自然落档
    /// postings）结果完全不变"。
    fn write_bitmap_corpus(root: &std::path::Path, bitmap: bool) {
        let mut cfg = IndexWriterConfig::default();
        cfg.bitmap = bitmap;
        let mut w = IndexWriter::create(root, schema(), cfg).unwrap();
        for i in 0..5000u32 {
            // hot: df=5000 ≥ 4096 → 命中；t0..t6: df≈714 不命中
            w.add_document(doc("INFO", &format!("tid-{i}"), &format!("hot t{}", i % 7)))
                .unwrap();
        }
        w.commit().unwrap();
        drop(w);
    }

    #[test]
    fn bitmap_write_keeps_all_results_identical() {
        let root_off = temp_dir("bmoff");
        let root_on = temp_dir("bmon");
        write_bitmap_corpus(&root_off, false);
        write_bitmap_corpus(&root_on, true);

        // .doc 尺寸：on > off（hot 的 bitmap + len 后缀）
        let doc_size = |root: &std::path::Path| -> u64 {
            std::fs::read_dir(root)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.extension().map(|x| x == "doc").unwrap_or(false))
                .map(|p| std::fs::metadata(p).unwrap().len())
                .unwrap()
        };
        assert!(doc_size(&root_on) > doc_size(&root_off));

        let dir_off = FSDirectory::open(&root_off).unwrap();
        let dir_on = FSDirectory::open(&root_on).unwrap();
        let mut s_off = Searcher::open(&dir_off).unwrap();
        let mut s_on = Searcher::open(&dir_on).unwrap();
        let battery: Vec<Query> = vec![
            Query::term("message", "hot"),
            Query::term("message", "t3"),
            Query::term("level", "INFO"),
            Query::term("tid", "tid-7"),
            Query::and("message", &["hot", "t3"]),
            Query::or("message", &["hot", "t3"]),
            Query::or("message", &["t0", "t1", "t2"]),
            Query::terms("message", &["hot", "t3", "nosuch"]),
            Query::prefix("message", "ho"),
            Query::MatchAll,
        ];
        for q in &battery {
            let (a_total, a_docs) = s_off.top_docs(q, 6000).unwrap();
            let (b_total, b_docs) = s_on.top_docs(q, 6000).unwrap();
            assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
            assert_eq!(
                s_off.count(q).unwrap(),
                s_on.count(q).unwrap(),
                "count {q:?}"
            );
        }
        assert_eq!(
            s_off.freq_sum(&Query::term("message", "hot")).unwrap(),
            s_on.freq_sum(&Query::term("message", "hot")).unwrap()
        );
        fs::remove_dir_all(&root_off).unwrap();
        fs::remove_dir_all(&root_on).unwrap();
    }

    /// M3 档 1 Term 接入：bitmap 索引上 Term 的迭代/count 走 roaring
    /// （迭代器变体断言钉死路径选择），结果与 bitmap off 索引逐位一致；
    /// needs_freq（freq_sum）与未命中 term 永远走 postings。
    #[test]
    fn term_query_uses_roaring_when_bitmap_present() {
        let root_off = temp_dir("bmtoff");
        let root_on = temp_dir("bmton");
        write_bitmap_corpus(&root_off, false);
        write_bitmap_corpus(&root_on, true);

        // 路径断言：bitmap 索引上 hot 的 segment iterator 是 Roaring 变体，
        // needs_freq=true 时回落 postings 变体；off 索引上永远不是 Roaring。
        let dir_on = FSDirectory::open(&root_on).unwrap();
        let mut reader = Reader::open(&dir_on).unwrap();
        let (_base, seg) = reader.leaves().next().unwrap();
        let q = Query::term("message", "hot");
        let it = q.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::Roaring(_)),
            "hot on bitmap index must take the roaring path"
        );
        let it = q.segment_iterator(seg, true).unwrap().unwrap();
        assert!(
            !matches!(it, SegmentDocIter::Roaring(_)),
            "needs_freq must stay on postings (bitmap carries no freq)"
        );
        let q_low = Query::term("message", "t3");
        let it = q_low.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            !matches!(it, SegmentDocIter::Roaring(_)),
            "df<4096 term has no bitmap: postings path"
        );
        drop(reader);

        // 全量结果等价（迭代序列、count、freq_sum）
        let dir_off = FSDirectory::open(&root_off).unwrap();
        let mut s_off = Searcher::open(&dir_off).unwrap();
        let dir_on2 = FSDirectory::open(&root_on).unwrap();
        let mut s_on = Searcher::open(&dir_on2).unwrap();
        let q = Query::term("message", "hot");
        let (a_total, a_docs) = s_off.top_docs(&q, 6000).unwrap();
        let (b_total, b_docs) = s_on.top_docs(&q, 6000).unwrap();
        assert_eq!((a_total, a_docs), (b_total, b_docs));
        assert_eq!(b_total, 5000);
        assert_eq!(s_on.count(&q).unwrap(), 5000); // roaring cardinality 路径
        assert_eq!(s_off.count(&q).unwrap(), s_on.count(&q).unwrap());
        assert_eq!(s_on.freq_sum(&q).unwrap(), 5000); // postings，不经 bitmap
                                                      // 未命中 term 与未知 term 不受影响
        assert_eq!(
            s_on.count(&Query::term("message", "t3")).unwrap(),
            s_off.count(&Query::term("message", "t3")).unwrap()
        );
        assert_eq!(s_on.count(&Query::term("message", "nosuch")).unwrap(), 0);
        fs::remove_dir_all(&root_off).unwrap();
        fs::remove_dir_all(&root_on).unwrap();
    }

    /// M5 v2 落档测试的 doctor helper：经 terms dict 定位 `term` 的
    /// bitmap region（docStartFP-4 读 len 回退），把 version 字节改写为
    /// `version`（codec 的 `postings::file_name` 是 pub(crate)，core 侧
    /// 按 `{segment}_Lucene912_0.doc` 拼名——SEGMENT_SUFFIX 即
    /// "Lucene912_0"，见 postings.rs:45-47）。
    fn doctor_bitmap_version(root: &std::path::Path, field: &str, term: &[u8], version: u8) {
        use codec_lucene9::field_infos::FieldInfos;
        use codec_lucene9::segment_infos::SegmentInfos;
        use codec_lucene9::terms_read::TermsDict;
        let dir = FSDirectory::open(root).unwrap();
        let (infos, _) = SegmentInfos::read_latest(&dir).unwrap();
        let sci = &infos.segments[0];
        let fis = FieldInfos::read(&dir, &sci.info.name, &sci.info.id, "").unwrap();
        let mut dict = TermsDict::open(&dir, &sci.info.name, &sci.info.id, &fis).unwrap();
        let fi = fis.by_name(field).unwrap();
        let entry = dict.seek_exact(fi, term).unwrap().expect("term exists");
        let fp = entry.state.doc_start_fp;
        let doc_path = root.join(format!("{}_Lucene912_0.doc", sci.info.name));
        let mut bytes = std::fs::read(&doc_path).unwrap();
        let len =
            u32::from_le_bytes(bytes[(fp - 4) as usize..fp as usize].try_into().unwrap()) as u64;
        let start = (fp - 4 - len) as usize;
        assert_eq!(&bytes[start..start + 4], b"RLBM");
        bytes[start + 4] = version;
        std::fs::write(&doc_path, &bytes).unwrap();
    }

    /// M5 §3 v2 落档：bitmap 索引 doctor 回 v2（version 字节）后读侧静默落
    /// postings，结果与 bitmap-off 索引逐位一致（doctor 前的 version==3 字节
    /// 断言见 codec 侧 open_term_bitmap_falls_back_on_legacy_version）。
    #[test]
    fn bitmap_v2_index_falls_back_to_postings() {
        let root_off = temp_dir("v2off");
        let root_on = temp_dir("v2on");
        write_bitmap_corpus(&root_off, false);
        write_bitmap_corpus(&root_on, true);
        doctor_bitmap_version(&root_on, "message", b"hot", 2);

        // 路径断言：hot 不再走 roaring（v2 region 被拒）
        let dir = FSDirectory::open(&root_on).unwrap();
        let mut reader = Reader::open(&dir).unwrap();
        let (_base, seg) = reader.leaves().next().unwrap();
        let it = Query::term("message", "hot")
            .segment_iterator(seg, false)
            .unwrap()
            .unwrap();
        assert!(
            !matches!(it, SegmentDocIter::Roaring(_)),
            "v2 region must fall back to postings"
        );
        drop(reader);

        // 全量等价：与 bitmap-off 索引逐位一致（PFOR 结果）
        let mut s_off = Searcher::open(&FSDirectory::open(&root_off).unwrap()).unwrap();
        let mut s_on = Searcher::open(&FSDirectory::open(&root_on).unwrap()).unwrap();
        let battery: Vec<Query> = vec![
            Query::term("message", "hot"),
            Query::term("message", "t3"),
            Query::and("message", &["hot", "t3"]),
            Query::or("message", &["hot", "t3"]),
            Query::terms("message", &["hot", "t3", "nosuch"]),
            Query::prefix("message", "ho"),
        ];
        for q in &battery {
            let (a_total, a_docs) = s_off.top_docs(q, 6000).unwrap();
            let (b_total, b_docs) = s_on.top_docs(q, 6000).unwrap();
            assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
            assert_eq!(
                s_off.count(q).unwrap(),
                s_on.count(q).unwrap(),
                "count {q:?}"
            );
        }
        assert_eq!(s_on.count(&Query::term("message", "hot")).unwrap(), 5000);
        fs::remove_dir_all(&root_off).unwrap();
        fs::remove_dir_all(&root_on).unwrap();
    }

    /// M3 三档语料：hot 全量、scorching 覆盖 d>=500、warmN 每 7 个一轮。
    /// 每段固定 5000 doc → 多段时各段 df 仍 ≥ 4096（per-segment 判定）。
    fn write_tier_corpus(root: &std::path::Path, bitmap: bool, segments: u32) {
        let mut cfg = IndexWriterConfig::default();
        cfg.bitmap = bitmap;
        let mut w = IndexWriter::create(root, schema(), cfg).unwrap();
        for seg_i in 0..segments {
            for i in 0..5000u32 {
                let d = seg_i * 5000 + i;
                let mut msg = String::from("hot");
                if d >= 500 {
                    msg.push_str(" scorching");
                }
                msg.push_str(&format!(" warm{}", d % 7));
                w.add_document(doc("INFO", &format!("tid-{d}"), &msg))
                    .unwrap();
            }
            w.commit().unwrap(); // 每段独立 flush → per-segment 三档判定
        }
        drop(w);
    }

    /// 三档路径 + on/off 全量等价（spec §8 Rust 对拍的单测形态）。
    #[test]
    fn bool_query_three_tier_roaring() {
        let root_off = temp_dir("tieroff");
        let root_on = temp_dir("tieron");
        write_tier_corpus(&root_off, false, 1);
        write_tier_corpus(&root_on, true, 1);

        // —— 路径断言（bitmap 索引）——
        let dir_on = FSDirectory::open(&root_on).unwrap();
        let mut reader = Reader::open(&dir_on).unwrap();
        let (_base, seg) = reader.leaves().next().unwrap();
        // 档 1：两个子句都有 bitmap（M5：RoaringAnd = 物化 fold / 偏斜 probe）
        let q = Query::and("message", &["hot", "scorching"]);
        let it = q.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::RoaringAnd(_)),
            "tier-1 AND must be roaring"
        );
        let q = Query::or("message", &["hot", "scorching"]);
        let it = q.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::RoaringOr(_)),
            "tier-1 OR must be roaring"
        );
        // 档 2：hot 有 bitmap、warm3 无（df≈714）→ 物化后统一 roaring
        let q = Query::and("message", &["hot", "warm3"]);
        let it = q.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::RoaringAnd(_)),
            "tier-2 mixed AND must be roaring"
        );
        let q = Query::or("message", &["scorching", "warm3"]);
        let it = q.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::RoaringOr(_)),
            "tier-2 mixed OR must be roaring"
        );
        // 档 3：两个子句都无 bitmap → 既有 PFOR 路径
        let q = Query::and("message", &["warm1", "warm3"]);
        let it = q.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::And(_)),
            "tier-3 stays PFOR conjunction"
        );
        // needs_freq=true：永不走 roaring（bitmap 无 freq）
        let q = Query::and("message", &["hot", "scorching"]);
        let it = q.segment_iterator(seg, true).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::And(_)),
            "needs_freq stays postings"
        );
        // 缺失子句：AND → None（空结果）；OR → 跳过缺失项后档 1
        let q = Query::and("message", &["hot", "nosuch"]);
        assert!(q.segment_iterator(seg, false).unwrap().is_none());
        let q = Query::or("message", &["scorching", "nosuch"]);
        let it = q.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::RoaringOr(_)),
            "OR with one present bitmap clause"
        );
        drop(reader);

        // —— on/off 全量等价（count + 完整 doc 序列）——
        let dir_off = FSDirectory::open(&root_off).unwrap();
        let mut s_off = Searcher::open(&dir_off).unwrap();
        let dir_on2 = FSDirectory::open(&root_on).unwrap();
        let mut s_on = Searcher::open(&dir_on2).unwrap();
        let battery: Vec<Query> = vec![
            Query::and("message", &["hot", "scorching"]), // 档 1 AND
            Query::or("message", &["hot", "scorching"]),  // 档 1 OR
            Query::and("message", &["hot", "warm3"]),     // 档 2 AND
            Query::or("message", &["scorching", "warm3"]), // 档 2 OR
            Query::or("message", &["hot", "warm0", "warm1"]), // 档 2 三子句
            Query::and("message", &["hot", "scorching", "warm5"]), // 档 2 三子句
            Query::and("message", &["warm1", "warm3"]),   // 档 3 AND
            Query::or("message", &["warm1", "warm3"]),    // 档 3 OR
            Query::and("message", &["hot", "nosuch"]),    // 空
            Query::or("message", &["scorching", "nosuch"]), // 单子句有效
        ];
        for q in &battery {
            let (a_total, a_docs) = s_off.top_docs(q, 6000).unwrap();
            let (b_total, b_docs) = s_on.top_docs(q, 6000).unwrap();
            assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
            assert_eq!(
                s_off.count(q).unwrap(),
                s_on.count(q).unwrap(),
                "count {q:?}"
            );
        }
        // 数值锚点（独立推演的期望，防 on/off 同错）：
        let dir_on3 = FSDirectory::open(&root_on).unwrap();
        let mut s = Searcher::open(&dir_on3).unwrap();
        assert_eq!(
            s.count(&Query::and("message", &["hot", "scorching"]))
                .unwrap(),
            4500
        );
        assert_eq!(
            s.count(&Query::or("message", &["hot", "scorching"]))
                .unwrap(),
            5000
        );
        assert_eq!(
            s.count(&Query::and("message", &["hot", "warm3"])).unwrap(),
            714
        );
        assert_eq!(
            s.count(&Query::or("message", &["scorching", "warm3"]))
                .unwrap(),
            4571
        );
        fs::remove_dir_all(&root_off).unwrap();
        fs::remove_dir_all(&root_on).unwrap();
    }

    /// M4 §5 skew 语料：20000 doc；rare df=4500（doc 0..4500）、common
    /// df=20000（全量）——都 ≥4096 有 bitmap。df 比 4.44：T5 标定前
    /// （SKEW_RATIO=4）走档 1 偏斜 probe；标定后（256，梯级微基准实测
    /// merge 在 r≤244 全胜）走档 1 非偏斜 merge——两形态同为
    /// RoaringAnd 迭代器，下方路径断言与 on/off 等价对两形态均成立。
    fn write_skew_corpus(root: &std::path::Path, bitmap: bool) {
        let mut cfg = IndexWriterConfig::default();
        cfg.bitmap = bitmap;
        let mut w = IndexWriter::create(root, schema(), cfg).unwrap();
        for d in 0..20000u32 {
            let msg = if d < 4500 { "common rare" } else { "common" };
            w.add_document(doc("INFO", &format!("tid-{d}"), msg))
                .unwrap();
        }
        w.commit().unwrap();
        drop(w);
    }

    /// 偏斜 probe（RoaringAnd 路径）与 bitmap-off PFOR 结果逐位一致，
    /// 锚点 count 钉死语义（交集 = rare 全集 4500，并集 = common 全集 20000）。
    #[test]
    fn and_skew_probe_matches_pfor() {
        let root_off = temp_dir("skewoff");
        let root_on = temp_dir("skewon");
        write_skew_corpus(&root_off, false);
        write_skew_corpus(&root_on, true);

        // 路径断言：skew AND 走 RoaringAnd（probe 形态由 T5 bench 标定）
        let dir = FSDirectory::open(&root_on).unwrap();
        let mut reader = Reader::open(&dir).unwrap();
        let (_base, seg) = reader.leaves().next().unwrap();
        let it = Query::and("message", &["rare", "common"])
            .segment_iterator(seg, false)
            .unwrap()
            .unwrap();
        assert!(
            matches!(it, SegmentDocIter::RoaringAnd(_)),
            "skew AND must be roaring"
        );
        drop(reader);

        let mut s_off = Searcher::open(&FSDirectory::open(&root_off).unwrap()).unwrap();
        let mut s_on = Searcher::open(&FSDirectory::open(&root_on).unwrap()).unwrap();
        let battery: Vec<Query> = vec![
            Query::and("message", &["rare", "common"]), // skew probe
            Query::and("message", &["common", "rare"]), // 子句顺序无关
            Query::or("message", &["rare", "common"]),  // OR（本任务仍折叠引擎）
            Query::term("message", "rare"),
        ];
        for q in &battery {
            let (a_total, a_docs) = s_off.top_docs(q, 25000).unwrap();
            let (b_total, b_docs) = s_on.top_docs(q, 25000).unwrap();
            assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
            assert_eq!(
                s_off.count(q).unwrap(),
                s_on.count(q).unwrap(),
                "count {q:?}"
            );
        }
        assert_eq!(
            s_on.count(&Query::and("message", &["rare", "common"]))
                .unwrap(),
            4500
        );
        assert_eq!(
            s_on.count(&Query::or("message", &["rare", "common"]))
                .unwrap(),
            20000
        );
        fs::remove_dir_all(&root_off).unwrap();
        fs::remove_dir_all(&root_on).unwrap();
    }

    /// 多段：三档判定按段独立（spec §5），docBase 映射不变。
    #[test]
    fn bool_query_roaring_multi_segment() {
        let root_off = temp_dir("tiermsoff");
        let root_on = temp_dir("tiermson");
        // 两段各 5000 doc → 每段 hot df=5000、scorching df=4500，两段都有 bitmap
        write_tier_corpus(&root_off, false, 2);
        write_tier_corpus(&root_on, true, 2);
        let dir_off = FSDirectory::open(&root_off).unwrap();
        let mut s_off = Searcher::open(&dir_off).unwrap();
        let dir_on = FSDirectory::open(&root_on).unwrap();
        let mut s_on = Searcher::open(&dir_on).unwrap();
        assert_eq!(s_on.segment_count(), 2);
        let battery: Vec<Query> = vec![
            Query::and("message", &["hot", "scorching"]),
            Query::or("message", &["hot", "warm3"]),
            Query::and("message", &["hot", "warm3"]),
        ];
        for q in &battery {
            let (a_total, a_docs) = s_off.top_docs(q, 12000).unwrap();
            let (b_total, b_docs) = s_on.top_docs(q, 12000).unwrap();
            assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
            assert_eq!(
                s_off.count(q).unwrap(),
                s_on.count(q).unwrap(),
                "count {q:?}"
            );
        }
        fs::remove_dir_all(&root_off).unwrap();
        fs::remove_dir_all(&root_on).unwrap();
    }

    /// M5 T3 k=3 全 bitmap fold 语料：fa=[0,6000)、fb=[2000,9000)、
    /// fc=[4000,12000)（df 都 ≥4096 有 bitmap）；交集 = [4000,6000) = 2000，
    /// 并集 = [0,12000) = 12000。
    fn write_fold_corpus(root: &std::path::Path, bitmap: bool) {
        let mut cfg = IndexWriterConfig::default();
        cfg.bitmap = bitmap;
        let mut w = IndexWriter::create(root, schema(), cfg).unwrap();
        for d in 0..12000u32 {
            let mut msg = String::new();
            if d < 6000 {
                msg.push_str("fa ");
            }
            if (2000..9000).contains(&d) {
                msg.push_str("fb ");
            }
            if d >= 4000 {
                msg.push_str("fc");
            }
            w.add_document(doc("INFO", &format!("tid-{d}"), msg.trim()))
                .unwrap();
        }
        w.commit().unwrap();
        drop(w);
    }

    /// k=3 全 bitmap 子句的物化 fold 迭代 + cardinality 快路径（spec §2），
    /// 与 bitmap-off PFOR 逐位一致；锚点钉死交集/并集数值。
    #[test]
    fn and_or_three_clause_fold_matches_pfor() {
        let root_off = temp_dir("foldoff");
        let root_on = temp_dir("foldon");
        write_fold_corpus(&root_off, false);
        write_fold_corpus(&root_on, true);
        let mut s_off = Searcher::open(&FSDirectory::open(&root_off).unwrap()).unwrap();
        let mut s_on = Searcher::open(&FSDirectory::open(&root_on).unwrap()).unwrap();
        let battery: Vec<Query> = vec![
            Query::and("message", &["fa", "fb", "fc"]),
            Query::or("message", &["fa", "fb", "fc"]),
            Query::and("message", &["fa", "fb"]),
            Query::or("message", &["fa", "fb"]),
        ];
        for q in &battery {
            let (a_total, a_docs) = s_off.top_docs(q, 15000).unwrap();
            let (b_total, b_docs) = s_on.top_docs(q, 15000).unwrap();
            assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
            assert_eq!(
                s_off.count(q).unwrap(),
                s_on.count(q).unwrap(),
                "count {q:?}"
            );
        }
        assert_eq!(
            s_on.count(&Query::and("message", &["fa", "fb", "fc"]))
                .unwrap(),
            2000
        );
        assert_eq!(
            s_on.count(&Query::or("message", &["fa", "fb", "fc"]))
                .unwrap(),
            12000
        );
        fs::remove_dir_all(&root_off).unwrap();
        fs::remove_dir_all(&root_on).unwrap();
    }

    /// M6 T-B: PointsDocIter 批量游标（BitmapCursor<MaterializedBitmap>）——
    /// next/advance 序列与物化集合逐点一致（游标形态同 RoaringDocIter，
    /// M5 关键设计事实 5）。
    #[test]
    fn points_doc_iter_cursor_sequences() {
        use codec_lucene9::postings_read::NO_MORE_DOCS;
        use codec_lucene9::roaring::MaterializedBitmap;
        let docs: Vec<u32> = (0..5000u32).map(|i| i * 2).collect();
        let mut it = doc_iter::PointsDocIter::new(MaterializedBitmap::of(&docs));
        assert_eq!(it.next_doc().unwrap(), 0);
        assert_eq!(it.next_doc().unwrap(), 2);
        assert_eq!(it.advance(101).unwrap(), 102); // 落缝 → 下一个
        assert_eq!(it.advance(102).unwrap(), 102); // 已在目标上不动
        assert_eq!(it.doc_id(), 102);
        let mut last = 102;
        loop {
            let d = it.next_doc().unwrap();
            if d == NO_MORE_DOCS {
                break;
            }
            assert!(d > last);
            last = d;
        }
        assert_eq!(last, 9998);
        assert_eq!(it.next_doc().unwrap(), NO_MORE_DOCS); // 粘滞
    }

    // ── M6 T-B: PointRange ─────────────────────────────────────────

    fn point_schema() -> Schema {
        let mut s = Schema::new();
        s.add(FieldSpec::long_point("ts"));
        s.add(FieldSpec::int_point("lvl"));
        s.add(FieldSpec::keyword("level"));
        s
    }

    /// ts = i*10（LongPoint），lvl = i-50（IntPoint），level 轮转（非 point 字段）
    fn point_doc(i: u32) -> Document {
        let mut d = Document::new();
        d.add("ts", FieldValue::Long(i as i64 * 10));
        d.add("lvl", FieldValue::Int(i as i32 - 50));
        d.add(
            "level",
            FieldValue::Keyword(if i % 2 == 0 { "INFO" } else { "WARN" }.to_string()),
        );
        d
    }

    /// M6 §3.4: 基本语义——count == 物化 cardinality == 迭代数；边界四类。
    #[test]
    fn point_range_query_basic() {
        let root = temp_dir("ptrange");
        let mut w =
            IndexWriter::create(&root, point_schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..100u32 {
            w.add_document(point_doc(i)).unwrap();
        }
        w.commit().unwrap();
        drop(w);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();

        // [100, 250] → docs 10..=25
        let q = Query::point_range("ts", 100, 250);
        assert_eq!(s.count(&q).unwrap(), 16);
        let (total, docs) = s.top_docs(&q, 100).unwrap();
        assert_eq!(total, 16);
        assert_eq!(docs, (10..=25).collect::<Vec<i32>>());
        // 迭代器变体钉死物化路径
        let mut reader = Reader::open(&dir).unwrap();
        let (_b, seg) = reader.leaves().next().unwrap();
        let it = q.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::Points(_)),
            "PointRange must materialize into SegmentDocIter::Points"
        );
        drop(reader);
        // 点查询退化 [v,v]
        assert_eq!(s.count(&Query::point_range("ts", 250, 250)).unwrap(), 1);
        // 不相交 → 0
        assert_eq!(s.count(&Query::point_range("ts", 2000, 3000)).unwrap(), 0);
        // 全区间 MIN..MAX → 100
        assert_eq!(
            s.count(&Query::point_range("ts", i64::MIN, i64::MAX))
                .unwrap(),
            100
        );
        // 贴 MIN / 贴 MAX
        assert_eq!(s.count(&Query::point_range("ts", i64::MIN, 0)).unwrap(), 1);
        assert_eq!(
            s.count(&Query::point_range("ts", 990, i64::MAX)).unwrap(),
            1
        );
        // 未知字段 / 非 point 字段 → 空命中（不报错）
        assert_eq!(s.count(&Query::point_range("nope", 0, 1)).unwrap(), 0);
        assert_eq!(
            s.count(&Query::point_range("level", 0, i64::MAX)).unwrap(),
            0
        );
        // freq_sum 拒绝 PointRange（needs_freq 恒 false，spec §3.3）
        let err = s.freq_sum(&Query::point_range("ts", 0, 1)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        fs::remove_dir_all(&root).unwrap();
    }

    /// M6 §3.1: low>high → Err(InvalidInput)（跨任务钉死接口）。
    /// 注意：Lucene 9.12.3 对 low>high 并不报错（PointRangeQuery.checkArgs
    /// :100-110 仅查 null，自然走成全 Outside → 0 命中）——Err 是本系统
    /// 钉死的显式错误面，不进 Java diff 电池。
    #[test]
    fn point_range_low_gt_high_errors() {
        let root = temp_dir("ptrange-err");
        let mut w =
            IndexWriter::create(&root, point_schema(), IndexWriterConfig::default()).unwrap();
        w.add_document(point_doc(0)).unwrap();
        w.commit().unwrap();
        drop(w);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        let err = s.count(&Query::point_range("ts", 10, 5)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let mut c = CountCollector::default();
        let err = s
            .search(&Query::point_range("ts", 10, 5), &mut c)
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        fs::remove_dir_all(&root).unwrap();
    }

    /// M6 §3.2: 多值点——同 doc 多值逐值回调，物化去重后只计一次。
    #[test]
    fn point_range_multivalued_dedup() {
        let root = temp_dir("ptrange-multi");
        let mut w =
            IndexWriter::create(&root, point_schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..10u32 {
            let mut d = point_doc(i);
            if i == 3 {
                d.add("ts", FieldValue::Long(10_000));
                d.add("ts", FieldValue::Long(20_000));
            }
            if i == 7 {
                d.add("ts", FieldValue::Long(5)); // 与主值 70 同 doc
            }
            w.add_document(d).unwrap();
        }
        w.commit().unwrap();
        drop(w);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        // [10_000, 20_000] 只命中 doc 3，一次（两个值都命中也只算一次）
        let q = Query::point_range("ts", 10_000, 20_000);
        assert_eq!(s.count(&q).unwrap(), 1);
        let (total, docs) = s.top_docs(&q, 10).unwrap();
        assert_eq!((total, docs), (1, vec![3]));
        // [0, 70] 命中 docs 0..=7（doc 7 两个值都在区间内）→ 8 docs
        assert_eq!(s.count(&Query::point_range("ts", 0, 70)).unwrap(), 8);
        // 全区间 → 仍 10 docs（物化集合去重）
        assert_eq!(
            s.count(&Query::point_range("ts", i64::MIN, i64::MAX))
                .unwrap(),
            10
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// M6 §3.1/§3.4: IntPoint 复用同一变体——按 .fnm point_num_bytes
    /// 解包 + clamp 规则（4 字节字段，整区间出界 → 空）。
    #[test]
    fn point_range_int_field_clamp() {
        let root = temp_dir("ptrange-int");
        let mut w =
            IndexWriter::create(&root, point_schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..100u32 {
            w.add_document(point_doc(i)).unwrap(); // lvl = i-50 ∈ [-50, 49]
        }
        w.commit().unwrap();
        drop(w);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        // i64 全域 → clamp 到 i32 全域 → 100
        assert_eq!(
            s.count(&Query::point_range("lvl", i64::MIN, i64::MAX))
                .unwrap(),
            100
        );
        // 部分出界 clamp：[i64::MIN, -40] → lvl ∈ [-50, -40] → docs 0..=10
        assert_eq!(
            s.count(&Query::point_range("lvl", i64::MIN, -40)).unwrap(),
            11
        );
        // 整区间出 i32 域 → 0（clamp-to-empty，非错误）
        assert_eq!(
            s.count(&Query::point_range("lvl", i32::MAX as i64 + 1, i64::MAX))
                .unwrap(),
            0
        );
        assert_eq!(
            s.count(&Query::point_range("lvl", i64::MIN, i32::MIN as i64 - 1))
                .unwrap(),
            0
        );
        // 负值边界
        assert_eq!(s.count(&Query::point_range("lvl", -50, -50)).unwrap(), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    /// M6 §3.3: 多段 doc base 映射 + 段级空结果。
    #[test]
    fn point_range_multi_segment() {
        let root = temp_dir("ptrange-seg");
        let mut w =
            IndexWriter::create(&root, point_schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..10u32 {
            w.add_document(point_doc(i)).unwrap();
        }
        w.commit().unwrap();
        for i in 10..25u32 {
            w.add_document(point_doc(i)).unwrap();
        }
        w.commit().unwrap();
        drop(w);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        // ts = i*10; [50, 200] → docs 5..=20
        let q = Query::point_range("ts", 50, 200);
        assert_eq!(s.count(&q).unwrap(), 16);
        let (total, docs) = s.top_docs(&q, 100).unwrap();
        assert_eq!(total, 16);
        assert_eq!(docs, (5..=20).collect::<Vec<i32>>());
        // 只命中第二段
        let (total, docs) = s
            .top_docs(&Query::point_range("ts", 150, 240), 100)
            .unwrap();
        assert_eq!(total, 10);
        assert_eq!(docs, (15..=24).collect::<Vec<i32>>());
        fs::remove_dir_all(&root).unwrap();
    }

    /// M6 §2.1：Occur 三态 + Bool 变体的模型层（构造/匹配/Clone/Eq）。
    #[test]
    fn bool_query_model() {
        let q = Query::bool(vec![
            (Occur::Must, Query::term("message", "w0")),
            (Occur::Should, Query::term("level", "INFO")),
            (Occur::MustNot, Query::MatchAll),
        ]);
        let Query::Bool { clauses } = &q else {
            panic!("expected Bool variant");
        };
        assert_eq!(clauses.len(), 3);
        assert_eq!(clauses[0].0, Occur::Must);
        assert_eq!(clauses[1].0, Occur::Should);
        assert_eq!(clauses[2].0, Occur::MustNot);
        assert_eq!(clauses[2].1, Query::MatchAll);
        let q2 = q.clone();
        assert_eq!(q, q2);
    }
}
