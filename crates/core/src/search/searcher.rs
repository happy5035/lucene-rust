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
    /// collector (leaf-level execution + docBase mapping). Collectors that
    /// don't consume freqs get no-freq postings enums and are passed freq=1.
    pub fn search<C: Collector>(&mut self, query: &Query, collector: &mut C) -> io::Result<()> {
        let needs_freq = collector.needs_freq();
        for (doc_base, seg) in self.reader.leaves() {
            let Some(mut iter) = query.segment_iterator(seg, needs_freq)? else {
                continue;
            };
            loop {
                let doc = iter.next_doc()?;
                if doc == NO_MORE_DOCS {
                    break;
                }
                let freq = if needs_freq { iter.freq() } else { 1 };
                collector.collect(doc_base + doc, freq);
            }
        }
        Ok(())
    }

    /// ConstantScore TermQuery: count = sum of per-segment doc_freq (no
    /// postings iteration needed — doc_freq is in the TermEntry after
    /// seek_exact). Fallback to iteration for MatchAll and unknown terms.
    /// Multi-term queries count per segment: popcount on the bitset path
    /// (spec §4), plain iteration on the OR path.
    pub fn count(&mut self, query: &Query) -> io::Result<u64> {
        if let Query::Term { field, term } = query {
            let mut total = 0u64;
            for (_doc_base, seg) in self.reader.leaves() {
                if let Some((_, entry)) = seg.seek_term(field, term)? {
                    total += entry.doc_freq as u64;
                }
            }
            return Ok(total);
        }
        if query.is_multi_term() {
            let mut total = 0u64;
            for (_doc_base, seg) in self.reader.leaves() {
                if let Some(c) = query.bitset_count(seg)? {
                    total += c;
                    continue;
                }
                if let Some(mut iter) = query.segment_iterator(seg, false)? {
                    loop {
                        let doc = iter.next_doc()?;
                        if doc == NO_MORE_DOCS {
                            break;
                        }
                        total += 1;
                    }
                }
            }
            return Ok(total);
        }
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

    /// ConstantScore TermQuery: freq_sum = total_term_freq from TermEntry
    /// (no postings iteration needed). Only defined for Term (and MatchAll,
    /// where it degenerates to the doc count); multi-term and Boolean
    /// queries have no well-defined freq sum and are rejected.
    pub fn freq_sum(&mut self, query: &Query) -> io::Result<u64> {
        if query.is_multi_term() || matches!(query, Query::And { .. } | Query::Or { .. }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "freq_sum is only defined for Term queries (MatchAll degenerates to \
                 doc count); multi-term and And/Or queries have no well-defined freq sum",
            ));
        }
        if let Query::Term { field, term } = query {
            let mut total = 0u64;
            for (_doc_base, seg) in self.reader.leaves() {
                if let Some((_, entry)) = seg.seek_term(field, term)? {
                    total += entry.total_term_freq;
                }
            }
            return Ok(total);
        }
        let mut c = FreqSumCollector::default();
        self.search(query, &mut c)?;
        Ok(c.total_freq)
    }
}
