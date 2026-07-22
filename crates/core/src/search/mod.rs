//! Search read path (search spec §3): per-segment iteration, docID-ordered
//! and count collectors, Term and MatchAll queries (ConstantScore semantics).

pub mod collector;
pub mod doc_iter;
pub mod query;
pub mod reader;
pub mod searcher;
pub mod segment_reader;

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
}
