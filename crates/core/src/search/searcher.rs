//! IndexSearcher (search spec §3): query × segment → collector. Single
//! threaded, segment-sequential (spec §1: query concurrency deferred; inter-segment
//! parallelism interface reserved).

use std::io;

use codec_lucene9::directory::FSDirectory;
use codec_lucene9::postings_read::NO_MORE_DOCS;

use super::collector::{Collector, FreqSumCollector};
use super::doc_iter::{DocBlockBuf, DocIter, SegmentDocIter};
use super::query::{self, Query};
use super::reader::Reader;
use super::segment_reader::block_enabled;

/// 块驱动循环（spec §5）：每 128 docs 一次 enum 分发 + 一次 Result
/// 检查。matches() 由 next_block 产出侧吸收，此处不调。doc_base 整体
/// 加宽（Phase 2 可 SIMD）。pub(crate) 供 block_tests 直调对拍。
pub(crate) fn drive_blocks<C: Collector>(
    iter: &mut SegmentDocIter,
    doc_base: u32,
    needs_freq: bool,
    collector: &mut C,
) -> std::io::Result<()> {
    let mut out = DocBlockBuf::new();
    loop {
        let n = iter.next_block(&mut out)?;
        if n == 0 {
            break;
        }
        if doc_base != 0 {
            for d in out.docs[..n].iter_mut() {
                *d += doc_base;
            }
        }
        collector.collect_block(&out.docs[..n], needs_freq.then(|| &out.freqs[..n]));
    }
    Ok(())
}

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
            if block_enabled() {
                drive_blocks(&mut iter, doc_base as u32, needs_freq, collector)?;
                continue;
            }
            loop {
                let doc = iter.next_doc()?;
                if doc == NO_MORE_DOCS {
                    break;
                }
                if !iter.matches()? {
                    continue;
                }
                let freq = if needs_freq { iter.freq() } else { 1 };
                collector.collect(doc_base + doc, freq);
            }
        }
        Ok(())
    }

    /// 逐段 count：段级快路径（fast_segment_count，M7 §5.1）优先，
    /// None 回落迭代计数。各形状语义与重构前逐条一致。
    pub fn count(&mut self, query: &Query) -> io::Result<u64> {
        let mut total = 0u64;
        for (_doc_base, seg) in self.reader.leaves() {
            if let Some(c) = query::fast_segment_count(seg, query)? {
                total += c;
                continue;
            }
            if let Some(mut iter) = query.segment_iterator(seg, false)? {
                if block_enabled() {
                    let mut out = DocBlockBuf::new();
                    loop {
                        let n = iter.next_block(&mut out)?;
                        if n == 0 {
                            break;
                        }
                        total += n as u64;
                    }
                    continue;
                }
                loop {
                    if iter.next_doc()? == NO_MORE_DOCS {
                        break;
                    }
                    if !iter.matches()? {
                        continue;
                    }
                    total += 1;
                }
            }
        }
        Ok(total)
    }

    /// (total hits, first `n` docIDs ascending) — Sort.INDEXORDER topN。
    /// M7 §5.2：count/topN 分离——段级 count 有快路径时 total 直读
    /// （µs 级），迭代只到收满 n 个命中即停；无快路径的形状回落全程
    /// 迭代（与旧实现一致）。(total, docs) 与旧实现逐字节一致。
    pub fn top_docs(&mut self, query: &Query, n: usize) -> io::Result<(u64, Vec<i32>)> {
        let mut total = 0u64;
        let mut docs: Vec<i32> = Vec::with_capacity(n.min(1024));
        for (doc_base, seg) in self.reader.leaves() {
            let fast = query::fast_segment_count(seg, query)?;
            if let Some(c) = fast {
                total += c;
            }
            // 段按 docBase 升序（INDEXORDER）：收满 n 且 count 已直读 →
            // 后续段只取 count 不再迭代（全局短路）。
            if docs.len() >= n && fast.is_some() {
                continue;
            }
            let Some(mut iter) = query.segment_iterator(seg, false)? else {
                continue; // 段内空（fast=Some(0) 或 None 时均无命中可计）
            };
            loop {
                if docs.len() >= n && fast.is_some() {
                    break; // 段内提前终止
                }
                let doc = iter.next_doc()?;
                if doc == NO_MORE_DOCS {
                    break;
                }
                if !iter.matches()? {
                    continue;
                }
                if fast.is_none() {
                    total += 1;
                }
                if docs.len() < n {
                    docs.push(doc_base + doc);
                }
            }
        }
        Ok((total, docs))
    }

    /// ConstantScore TermQuery: freq_sum = total_term_freq from TermEntry
    /// (no postings iteration needed). Only defined for Term (and MatchAll,
    /// where it degenerates to the doc count); multi-term and Boolean
    /// queries have no well-defined freq sum and are rejected.
    pub fn freq_sum(&mut self, query: &Query) -> io::Result<u64> {
        if query.is_multi_term()
            || matches!(
                query,
                Query::And { .. }
                    | Query::Or { .. }
                    | Query::Bool { .. }
                    | Query::PointRange { .. }
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "freq_sum is only defined for Term queries (MatchAll degenerates to \
                 doc count); multi-term and Boolean (And/Or/Bool) queries have no \
                 well-defined freq sum",
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
