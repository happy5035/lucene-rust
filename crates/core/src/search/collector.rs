//! Collectors (search spec §3). M1 is per-doc callback only; the block-level
//! batch interface (spec §4b DocBlock) arrives with the SIMD phase.

pub trait Collector {
    /// `doc` is the global docID (docBase applied), `freq` the term freq
    /// (1 for docs-only iterators).
    fn collect(&mut self, doc: i32, freq: u32);
    /// Whether `collect` consumes the freq argument. When false the search
    /// drive requests no-freq postings enums (freq PFOR blocks are skipped,
    /// never decoded) and passes 1 — the count-only fast path.
    fn needs_freq(&self) -> bool {
        false
    }
    /// 块级消费（spec 2026-07-26）：默认逐 doc 回退兜底外部 collector。
    /// `freqs = None` 时 freq 恒 1（needs_freq=false 驱动或无 freq 迭代器）。
    fn collect_block(&mut self, docs: &[u32], freqs: Option<&[u32]>) {
        match freqs {
            Some(f) => {
                for (i, &d) in docs.iter().enumerate() {
                    self.collect(d as i32, f[i]);
                }
            }
            None => {
                for &d in docs {
                    self.collect(d as i32, 1);
                }
            }
        }
    }
}

/// Total hit count (diff battery workhorse).
#[derive(Default)]
pub struct CountCollector {
    pub count: u64,
}

impl Collector for CountCollector {
    fn collect(&mut self, _doc: i32, _freq: u32) {
        self.count += 1;
    }
    fn collect_block(&mut self, docs: &[u32], _freqs: Option<&[u32]>) {
        self.count += docs.len() as u64;
    }
}

/// Top-N by docID (Sort.INDEXORDER semantics): hits ascend in docID during
/// the drive, so the top N is exactly the first N hits; `total` still
/// counts everything.
pub struct TopDocCollector {
    top_n: usize,
    pub total: u64,
    pub docs: Vec<i32>,
}

impl TopDocCollector {
    pub fn new(top_n: usize) -> Self {
        TopDocCollector {
            top_n,
            total: 0,
            docs: Vec::with_capacity(top_n.min(1024)),
        }
    }
}

impl Collector for TopDocCollector {
    fn collect(&mut self, doc: i32, _freq: u32) {
        self.total += 1;
        if self.docs.len() < self.top_n {
            self.docs.push(doc);
        }
    }
}

/// Sum of term freqs over all hits — exercises the PFor freq decode end to
/// end (used by the Java diff battery).
#[derive(Default)]
pub struct FreqSumCollector {
    pub total_freq: u64,
}

impl Collector for FreqSumCollector {
    fn collect(&mut self, _doc: i32, freq: u32) {
        self.total_freq += freq as u64;
    }
    fn needs_freq(&self) -> bool {
        true
    }
}
