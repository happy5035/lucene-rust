use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Heap-based top-N collector sorted by a numeric sort value.
/// For desc order, keeps the N largest values; for asc, the N smallest.
/// Missing values (i64::MIN) sort last in desc order.
pub struct SortedTopN {
    desc: bool,
    n: usize,
    // For desc: min-heap of (value, doc) — evict smallest when full.
    // For asc: max-heap of (Reverse(value), doc) — evict largest when full.
    heap_desc: BinaryHeap<(Reverse<i64>, i32)>,
    heap_asc: BinaryHeap<(i64, i32)>,
    total: u64,
}

pub struct SearchResults {
    pub total: u64,
    pub docs: Vec<i32>,
}

impl SortedTopN {
    pub fn new(desc: bool, n: usize) -> Self {
        Self {
            desc,
            n,
            heap_desc: BinaryHeap::new(),
            heap_asc: BinaryHeap::new(),
            total: 0,
        }
    }

    pub fn collect(&mut self, doc: i32, sort_value: i64) {
        self.total += 1;
        if self.n == 0 {
            return;
        }
        if self.desc {
            if self.heap_desc.len() < self.n {
                self.heap_desc.push((Reverse(sort_value), doc));
            } else if let Some(&(Reverse(min_val), _)) = self.heap_desc.peek() {
                if sort_value > min_val {
                    self.heap_desc.pop();
                    self.heap_desc.push((Reverse(sort_value), doc));
                }
            }
        } else {
            if self.heap_asc.len() < self.n {
                self.heap_asc.push((sort_value, doc));
            } else if let Some(&(max_val, _)) = self.heap_asc.peek() {
                if sort_value < max_val {
                    self.heap_asc.pop();
                    self.heap_asc.push((sort_value, doc));
                }
            }
        }
    }

    pub fn results(self) -> SearchResults {
        let mut docs: Vec<(i64, i32)> = if self.desc {
            self.heap_desc
                .into_iter()
                .map(|(Reverse(v), d)| (v, d))
                .collect()
        } else {
            self.heap_asc.into_iter().collect()
        };
        // Sort: desc → largest first; asc → smallest first
        if self.desc {
            docs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        } else {
            docs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        }
        SearchResults {
            total: self.total,
            docs: docs.into_iter().map(|(_, d)| d).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desc_top3() {
        let mut c = SortedTopN::new(true, 3);
        // values: doc0=10, doc1=50, doc2=30, doc3=90, doc4=20
        c.collect(0, 10);
        c.collect(1, 50);
        c.collect(2, 30);
        c.collect(3, 90);
        c.collect(4, 20);
        let r = c.results();
        assert_eq!(r.total, 5);
        assert_eq!(r.docs, vec![3, 1, 2]); // 90, 50, 30
    }

    #[test]
    fn asc_top3() {
        let mut c = SortedTopN::new(false, 3);
        c.collect(0, 10);
        c.collect(1, 50);
        c.collect(2, 30);
        c.collect(3, 90);
        c.collect(4, 20);
        let r = c.results();
        assert_eq!(r.total, 5);
        assert_eq!(r.docs, vec![0, 4, 2]); // 10, 20, 30
    }

    #[test]
    fn missing_sorts_last_desc() {
        let mut c = SortedTopN::new(true, 3);
        c.collect(0, i64::MIN); // missing
        c.collect(1, 50);
        c.collect(2, 30);
        c.collect(3, 90);
        let r = c.results();
        assert_eq!(r.total, 4);
        assert_eq!(r.docs, vec![3, 1, 2]); // 90, 50, 30 — missing excluded from top3
    }

    #[test]
    fn n_zero_counts_only() {
        let mut c = SortedTopN::new(true, 0);
        c.collect(0, 10);
        c.collect(1, 20);
        let r = c.results();
        assert_eq!(r.total, 2);
        assert!(r.docs.is_empty());
    }

    #[test]
    fn n_larger_than_hits() {
        let mut c = SortedTopN::new(true, 100);
        c.collect(0, 10);
        c.collect(1, 20);
        let r = c.results();
        assert_eq!(r.total, 2);
        assert_eq!(r.docs, vec![1, 0]); // 20, 10
    }

    #[test]
    fn tie_breaking_by_doc_id_asc() {
        let mut c = SortedTopN::new(true, 3);
        c.collect(5, 50);
        c.collect(2, 50);
        c.collect(8, 50);
        let r = c.results();
        assert_eq!(r.docs, vec![2, 5, 8]); // same value → doc id asc
    }
}
