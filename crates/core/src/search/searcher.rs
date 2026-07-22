//! IndexSearcher (search spec §3): query × segment → collector. Single
//! threaded, segment-sequential (spec §1: query concurrency deferred; inter-segment
//! parallelism interface reserved).

use std::io;

use codec_lucene9::directory::FSDirectory;
use codec_lucene9::postings_read::NO_MORE_DOCS;

use super::collector::{Collector, CountCollector, FreqSumCollector, TopDocCollector};
use super::doc_iter::DocIter;
use super::query::Query;
use super::reader::Reader;

pub struct Searcher {
    reader: Reader,
}

impl Searcher {
    pub fn open(dir: &FSDirectory) -> io::Result<Searcher> {
        Ok(Searcher {
            reader: Reader::open(dir)?,
        })
    }

    pub fn max_doc(&self) -> i32 {
        self.reader.max_doc()
    }

    pub fn segment_count(&self) -> usize {
        self.reader.segment_count()
    }

    /// Drives the query per segment and feeds global docIDs to the
    /// collector (leaf-level execution + docBase mapping).
    pub fn search<C: Collector>(&mut self, query: &Query, collector: &mut C) -> io::Result<()> {
        for (doc_base, seg) in self.reader.leaves() {
            let Some(mut iter) = query.segment_iterator(seg)? else {
                continue;
            };
            loop {
                let doc = iter.next_doc()?;
                if doc == NO_MORE_DOCS {
                    break;
                }
                collector.collect(doc_base + doc, iter.freq());
            }
        }
        Ok(())
    }

    pub fn count(&mut self, query: &Query) -> io::Result<u64> {
        let mut c = CountCollector::default();
        self.search(query, &mut c)?;
        Ok(c.count)
    }

    /// (total hits, first `n` docIDs ascending) — Sort.INDEXORDER topN.
    pub fn top_docs(&mut self, query: &Query, n: usize) -> io::Result<(u64, Vec<i32>)> {
        let mut c = TopDocCollector::new(n);
        self.search(query, &mut c)?;
        Ok((c.total, c.docs))
    }

    pub fn freq_sum(&mut self, query: &Query) -> io::Result<u64> {
        let mut c = FreqSumCollector::default();
        self.search(query, &mut c)?;
        Ok(c.total_freq)
    }
}
