//! Index sort: compute a docID permutation (`DocMap`) that physically orders
//! a segment's documents by a sort key — the Rust analog of Lucene's
//! `index/Sorter.java` (flush path). The permutation is applied to the RAM
//! indexing buffers before `SegmentBuilder::finalize` encodes them, so the
//! existing format writers (which all assume ascending docIDs) stay unchanged.
//!
//! Scope (design §4.1 phase A): single sort field, numeric (NumericDocValues)
//! or string (SortedDocValues) keys, configurable reverse + missing placement.

/// Where docs that lack the sort field land (Lucene `SortField.missingValue`:
/// numeric uses a MIN/MAX sentinel, string sorts missing first/last).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    First,
    Last,
}

/// One index-sort field declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSortField {
    pub field: String,
    pub reverse: bool,
    pub missing: Missing,
}

impl IndexSortField {
    pub fn new(field: &str) -> Self {
        Self {
            field: field.to_string(),
            reverse: false,
            missing: Missing::Last,
        }
    }

    pub fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }

    pub fn missing_first(mut self) -> Self {
        self.missing = Missing::First;
        self
    }
}

/// A per-doc sort key. `None` = the doc has no value for the sort field
/// (placed per [`Missing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// NumericDocValues key (i64).
    Num(i64),
    /// SortedDocValues key: ordinal into the field's sorted dictionary, so
    /// comparison is by term bytes (Lucene StringSorter compares ords).
    Ord(u32),
}

impl Key {
    fn cmp(self, other: Key) -> std::cmp::Ordering {
        match (self, other) {
            (Key::Num(a), Key::Num(b)) => a.cmp(&b),
            (Key::Ord(a), Key::Ord(b)) => a.cmp(&b),
            // mixing numeric and string keys is a schema error caught before
            // compute(); treat as equal so the tie-break (docID) decides.
            _ => std::cmp::Ordering::Equal,
        }
    }
}

/// docID permutation. `old_to_new[oldDoc] = newDoc` and
/// `new_to_old[newDoc] = oldDoc` are strict inverses.
#[derive(Debug, Clone)]
pub struct DocMap {
    pub old_to_new: Vec<u32>,
    pub new_to_old: Vec<u32>,
}

impl DocMap {
    #[inline]
    pub fn old_to_new(&self, doc: u32) -> u32 {
        self.old_to_new[doc as usize]
    }

    #[inline]
    pub fn new_to_old(&self, new_doc: u32) -> u32 {
        self.new_to_old[new_doc as usize]
    }

    pub fn len(&self) -> usize {
        self.new_to_old.len()
    }

    pub fn is_empty(&self) -> bool {
        self.new_to_old.is_empty()
    }
}

/// Compares two sort keys honoring reverse and missing placement, without any
/// docID tie-break. Shared by flush sorting and the merge K-way merge.
pub fn cmp_keys(
    ka: Option<Key>,
    kb: Option<Key>,
    reverse: bool,
    missing: Missing,
) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (ka, kb) {
        (Some(x), Some(y)) => {
            let c = x.cmp(y);
            if reverse {
                c.reverse()
            } else {
                c
            }
        }
        (None, None) => Equal,
        (Some(_), None) => match missing {
            Missing::First => Greater, // missing (b) sorts before a
            Missing::Last => Less,     // missing (b) sorts after a
        },
        (None, Some(_)) => match missing {
            Missing::First => Less,
            Missing::Last => Greater,
        },
    }
}

/// Ties (including both-missing) break by ascending oldDoc for a stable,
/// deterministic order (Lucene `Sorter.sort` tie-breaks with
/// `Integer.compare(docID1, docID2)`).
fn cmp_docs(
    a: u32,
    b: u32,
    keys: &[Option<Key>],
    reverse: bool,
    missing: Missing,
) -> std::cmp::Ordering {
    cmp_keys(keys[a as usize], keys[b as usize], reverse, missing).then_with(|| a.cmp(&b))
}

/// Compute the permutation that orders `0..max_doc` by `keys`. Returns `None`
/// when the docs are already in sorted order (Lucene `Sorter.sort` returns a
/// null DocMap then, and the segment is written as-is).
pub fn compute(
    max_doc: u32,
    keys: &[Option<Key>],
    reverse: bool,
    missing: Missing,
) -> Option<DocMap> {
    debug_assert_eq!(keys.len(), max_doc as usize);
    if max_doc <= 1 {
        return None;
    }
    // already sorted? linear scan of adjacent pairs (Sorter.java:131-145)
    let mut sorted = true;
    for d in 0..(max_doc - 1) {
        if cmp_docs(d, d + 1, keys, reverse, missing) == std::cmp::Ordering::Greater {
            sorted = false;
            break;
        }
    }
    if sorted {
        return None;
    }
    // newToOld: position i holds the oldDoc that belongs at new position i.
    let mut new_to_old: Vec<u32> = (0..max_doc).collect();
    new_to_old.sort_by(|&a, &b| cmp_docs(a, b, keys, reverse, missing));
    let mut old_to_new = vec![0u32; max_doc as usize];
    for (new, &old) in new_to_old.iter().enumerate() {
        old_to_new[old as usize] = new as u32;
    }
    Some(DocMap {
        old_to_new,
        new_to_old,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nums(vs: &[Option<i64>]) -> Vec<Option<Key>> {
        vs.iter().map(|v| v.map(Key::Num)).collect()
    }

    #[test]
    fn already_sorted_returns_none() {
        let keys = nums(&[Some(1), Some(2), Some(3)]);
        assert!(compute(3, &keys, false, Missing::Last).is_none());
    }

    #[test]
    fn single_or_empty_is_none() {
        assert!(compute(0, &[], false, Missing::Last).is_none());
        let keys = nums(&[Some(5)]);
        assert!(compute(1, &keys, false, Missing::Last).is_none());
    }

    #[test]
    fn ascending_numeric() {
        // oldDocs: 0->30, 1->10, 2->20  => sorted order 1,2,0
        let keys = nums(&[Some(30), Some(10), Some(20)]);
        let m = compute(3, &keys, false, Missing::Last).unwrap();
        assert_eq!(m.new_to_old, vec![1, 2, 0]);
        assert_eq!(m.old_to_new, vec![2, 0, 1]);
        // inverses
        for new in 0..3u32 {
            assert_eq!(m.old_to_new[m.new_to_old(new) as usize], new);
        }
    }

    #[test]
    fn reverse_numeric() {
        let keys = nums(&[Some(30), Some(10), Some(20)]);
        let m = compute(3, &keys, true, Missing::Last).unwrap();
        // descending values: 30,20,10 => oldDocs 0,2,1
        assert_eq!(m.new_to_old, vec![0, 2, 1]);
    }

    #[test]
    fn missing_last() {
        // oldDocs: 0->Some(2), 1->None, 2->Some(1); missing last
        let keys = nums(&[Some(2), None, Some(1)]);
        let m = compute(3, &keys, false, Missing::Last).unwrap();
        // values asc: 1(old2), 2(old0), missing(old1)
        assert_eq!(m.new_to_old, vec![2, 0, 1]);
    }

    #[test]
    fn missing_first() {
        let keys = nums(&[Some(2), None, Some(1)]);
        let m = compute(3, &keys, false, Missing::First).unwrap();
        // missing(old1) first, then 1(old2), 2(old0)
        assert_eq!(m.new_to_old, vec![1, 2, 0]);
    }

    #[test]
    fn tie_breaks_by_olddoc_stable() {
        // all equal keys => identity order (stable by docID)
        let keys = nums(&[Some(7), Some(7), Some(7)]);
        // already sorted (all equal, adjacent cmp == Equal not Greater)
        assert!(compute(3, &keys, false, Missing::Last).is_none());
        // force a sort by mixing one different value, equal pair keeps doc order
        let keys = nums(&[Some(7), Some(7), Some(1)]);
        let m = compute(3, &keys, false, Missing::Last).unwrap();
        // 1(old2) first, then the two 7s in oldDoc order: old0, old1
        assert_eq!(m.new_to_old, vec![2, 0, 1]);
    }

    #[test]
    fn ord_keys_compare_by_ordinal() {
        // ords: old0->2, old1->0, old2->1 => sorted old1,old2,old0
        let keys: Vec<Option<Key>> = vec![Some(Key::Ord(2)), Some(Key::Ord(0)), Some(Key::Ord(1))];
        let m = compute(3, &keys, false, Missing::Last).unwrap();
        assert_eq!(m.new_to_old, vec![1, 2, 0]);
    }

    #[test]
    fn reverse_with_missing_first() {
        // reverse flips value order but missing placement is independent
        let keys = nums(&[Some(1), None, Some(3)]);
        let m = compute(3, &keys, true, Missing::First).unwrap();
        // missing(old1) first, then desc values: 3(old2), 1(old0)
        assert_eq!(m.new_to_old, vec![1, 2, 0]);
    }
}
