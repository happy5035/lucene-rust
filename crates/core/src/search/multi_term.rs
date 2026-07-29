//! Multi-term query execution (search spec M2 §4): collect the term set of a
//! Terms/Prefix/Wildcard query for one segment, then dispatch on the Lucene 9
//! blended threshold — at most 16 terms rewrite to `Query::Or`
//! (AbstractMultiTermQueryConstantScoreWrapper.java:43-44), more than 16
//! materialize a per-segment FixedBitSet (Lucene's DocIdSet rewrite,
//! MultiTermQueryConstantScoreBlendedWrapper.java:55-120). No enumeration cap
//! (Lucene 9 dropped TooManyClauses for MTQs).

use std::io;

use codec_lucene9::automaton::WildcardDfa;
use codec_lucene9::postings_read::NO_MORE_DOCS;

use super::bitset::FixedBitSet;
use super::doc_iter::{BitsetDocIter, DisjunctionDocIter, DocIter, PostingsIter, SegmentDocIter};
use super::leaf_access::LeafAccess;

/// AbstractMultiTermQueryConstantScoreWrapper.java:44.
pub(crate) const BOOLEAN_REWRITE_THRESHOLD: usize = 16;

/// Terms of one query present in one segment's dictionary, df-sorted
/// (spec §4: 集合收集后按 df 排序交给双路). Generic over the leaf's term
/// handle type (`L::TermHandle`). The term bytes themselves are not kept:
/// no consumer reads them (postings work off the handles), and collecting
/// them cost one Vec per matched term on multi-term queries.
pub(crate) struct CollectedTerms<H> {
    pub entries: Vec<(u32, H)>,
}

impl<H> CollectedTerms<H> {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn sort_by_df(&mut self) {
        self.entries.sort_by_key(|(df, _)| *df);
    }
}

/// Direct term-set collection (Terms/IN): seek each term, keep the present
/// ones. `None` = unknown field (empty-hit semantics, same as TermQuery).
pub(crate) fn collect_direct<L: LeafAccess>(
    seg: &mut L,
    field: &str,
    terms: &[Vec<u8>],
) -> io::Result<Option<(bool, CollectedTerms<L::TermHandle>)>> {
    let Some(has_freqs) = seg.field_has_freqs(field) else {
        return Ok(None);
    };
    let mut collected = CollectedTerms {
        entries: Vec::new(),
    };
    for t in terms {
        if let Some((_, entry)) = seg.seek_term(field, t)? {
            let df = seg.term_doc_freq(&entry);
            collected.entries.push((df, entry));
        }
    }
    collected.sort_by_df();
    Ok(Some((has_freqs, collected)))
}

/// Prefix collection (spec §3): seek_ceil(prefix) then next() until
/// !starts_with(prefix). `None` = unknown field (empty-hit semantics).
///
/// The iterator yields concrete `L::TermHandle` directly — no re-seek needed.
pub(crate) fn collect_prefix<L: LeafAccess>(
    seg: &mut L,
    field: &str,
    prefix: &[u8],
) -> io::Result<Option<(bool, CollectedTerms<L::TermHandle>)>> {
    let Some(has_freqs) = seg.field_has_freqs(field) else {
        return Ok(None);
    };
    let mut handles: Vec<L::TermHandle> = Vec::new();
    {
        let Some(mut it) = seg.terms_iter(field) else {
            return Ok(None);
        };
        it.seek_ceil(prefix)?;
        while let Some((term, handle)) = it.next()? {
            if !term.starts_with(prefix) {
                break;
            }
            handles.push(handle);
        }
    }
    let mut collected = CollectedTerms {
        entries: Vec::with_capacity(handles.len()),
    };
    for handle in handles {
        let df = seg.term_doc_freq(&handle);
        collected.entries.push((df, handle));
    }
    collected.sort_by_df();
    Ok(Some((has_freqs, collected)))
}

/// Wildcard classification (search spec M2 §5 / 总 spec §3): cut the pattern
/// at the first '*'/'?' for the fixed prefix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WildcardClass {
    /// No wildcard chars: Term semantics.
    Exact,
    /// prefix + trailing '*' only: prefix enumeration, zero filtering.
    PurePrefix,
    /// Fixed prefix + wildcards later: prefix enumeration + tail filtering.
    PrefixFilter,
    /// Starts with a wildcard: full dictionary scan + filtering.
    FullScan,
}

pub(crate) struct WildcardPattern {
    pub pattern: Vec<u8>,
    pub prefix: Vec<u8>,
    pub class: WildcardClass,
    pattern_chars: Vec<char>,
}

impl WildcardPattern {
    pub(crate) fn parse(pattern: &[u8]) -> WildcardPattern {
        let cut = pattern
            .iter()
            .position(|&b| b == b'*' || b == b'?')
            .unwrap_or(pattern.len());
        let prefix = pattern[..cut].to_vec();
        let rest = &pattern[cut..];
        let class = if rest.is_empty() {
            WildcardClass::Exact
        } else if rest == b"*" {
            WildcardClass::PurePrefix
        } else if prefix.is_empty() {
            WildcardClass::FullScan
        } else {
            WildcardClass::PrefixFilter
        };
        let pattern_chars = match std::str::from_utf8(pattern) {
            Ok(s) => s.chars().collect(),
            Err(_) => Vec::new(),
        };
        WildcardPattern {
            pattern: pattern.to_vec(),
            prefix,
            class,
            pattern_chars,
        }
    }

    pub(crate) fn matches(&self, term: &[u8]) -> bool {
        match self.class {
            WildcardClass::Exact => term == self.pattern.as_slice(),
            WildcardClass::PurePrefix => term.starts_with(&self.prefix),
            _ => glob_match(&self.pattern_chars, term),
        }
    }
}

/// Two-pointer glob with '*' backtracking over chars. '?' matches exactly
/// one code point. Pattern chars are pre-collected by the caller (once per
/// query); text is iterated lazily via from_utf8 + char_indices (no Vec).
pub(crate) fn glob_match(pattern: &[char], text: &[u8]) -> bool {
    let t: &str = match std::str::from_utf8(text) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let tchars: Vec<char> = t.chars().collect();
    let (mut i, mut j) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while j < tchars.len() {
        if i < pattern.len() && (pattern[i] == '?' || pattern[i] == tchars[j]) {
            i += 1;
            j += 1;
        } else if i < pattern.len() && pattern[i] == '*' {
            star = Some((i + 1, j));
            i += 1;
        } else if let Some((si, sj)) = star {
            i = si;
            j = sj + 1;
            star = Some((si, sj + 1));
        } else {
            return false;
        }
    }
    while i < pattern.len() && pattern[i] == '*' {
        i += 1;
    }
    i == pattern.len()
}

/// Wildcard collection (spec §5): Exact → direct seek; PurePrefix → prefix
/// enumeration with zero filtering; PrefixFilter → prefix enumeration +
/// glob tail filter; FullScan → whole-dictionary scan + glob filter.
pub(crate) fn collect_wildcard<L: LeafAccess>(
    seg: &mut L,
    field: &str,
    pat: &WildcardPattern,
    dfa: &WildcardDfa,
) -> io::Result<Option<(bool, CollectedTerms<L::TermHandle>)>> {
    match pat.class {
        WildcardClass::Exact => collect_direct(seg, field, std::slice::from_ref(&pat.pattern)),
        WildcardClass::PurePrefix => collect_prefix(seg, field, &pat.prefix),
        WildcardClass::PrefixFilter | WildcardClass::FullScan => {
            let Some(has_freqs) = seg.field_has_freqs(field) else {
                return Ok(None);
            };
            let Some(results) = seg.intersect_terms(field, dfa)? else {
                return Ok(None);
            };
            let mut collected = CollectedTerms {
                entries: Vec::with_capacity(results.len()),
            };
            for (_term, handle) in results {
                let df = seg.term_doc_freq(&handle);
                collected.entries.push((df, handle));
            }
            collected.sort_by_df();
            Ok(Some((has_freqs, collected)))
        }
    }
}

/// Threshold dispatch (spec §4): <=16 terms build a DisjunctionDocIter
/// directly from collected handles (no re-seek); >16 materialize a FixedBitSet.
pub(crate) fn segment_iterator<L: LeafAccess>(
    seg: &mut L,
    _field: &str,
    has_freqs: bool,
    collected: &CollectedTerms<L::TermHandle>,
    needs_freq: bool,
) -> io::Result<Option<SegmentDocIter>> {
    if collected.is_empty() {
        return Ok(None);
    }
    if collected.len() <= BOOLEAN_REWRITE_THRESHOLD {
        let mut sub = Vec::with_capacity(collected.len());
        for (_, entry) in &collected.entries {
            let it = if has_freqs {
                seg.docs_freqs_enum(entry, needs_freq)?
            } else {
                seg.docs_enum(entry)?
            };
            sub.push(PostingsIter::from_segment_iter(it));
        }
        return Ok(Some(SegmentDocIter::Or(DisjunctionDocIter::from_iters(sub)?)));
    }
    let bits = materialize(seg, &collected.entries, has_freqs)?;
    Ok(Some(SegmentDocIter::Bitset(BitsetDocIter::new(bits))))
}

/// Count fast path (spec §4: count 路径直接 popcount): `Some(popcount)` on
/// the bitset path, `None` when the OR path applies (caller iterates).
/// For <=16 terms the caller's disjunction iteration wins at small df
/// (no bitset alloc/popcount fixed cost), but loses at large df: the
/// k-way merge costs ~28ns/emitted doc while bitset materialization costs
/// ~5ns/decoded doc + ~20us fixed — measured on enwiki (terms high:
/// 4 terms, 11470 df-sum, 315us disjunction vs ~80us bitset). Crossover
/// is at ~1-2k df-sum, so above a threshold even few-term counts take
/// the bitset path (identical OR-set semantics).
pub(crate) const BITSET_COUNT_MIN_DF_SUM: u64 = 2048;

pub(crate) fn bitset_count<L: LeafAccess>(
    seg: &L,
    has_freqs: bool,
    collected: &CollectedTerms<L::TermHandle>,
) -> io::Result<Option<u64>> {
    if collected.len() == 1 {
        return Ok(Some(collected.entries[0].0 as u64));
    }
    if collected.len() <= BOOLEAN_REWRITE_THRESHOLD {
        let df_sum: u64 = collected.entries.iter().map(|(df, _)| *df as u64).sum();
        if df_sum < BITSET_COUNT_MIN_DF_SUM {
            return Ok(None);
        }
    }
    Ok(Some(
        materialize(seg, &collected.entries, has_freqs)?.popcount(),
    ))
}

/// Per-term postings doc scan shared by the M2 bitset materialization and
/// the M3 tier-2 roaring materialization: feeds every doc of `entry`'s
/// postings to `f` in ascending order. Uses no-freq enums on freqs fields —
/// the materialized sets carry no per-doc freq (ConstantScore).
pub(crate) fn for_each_doc<L: LeafAccess>(
    seg: &L,
    entry: &L::TermHandle,
    has_freqs: bool,
    f: &mut impl FnMut(u32),
) -> io::Result<()> {
    if has_freqs {
        let mut en = seg.docs_freqs_enum(entry, false)?;
        loop {
            let d = en.next_doc()?;
            if d == NO_MORE_DOCS {
                break;
            }
            f(d as u32);
        }
    } else {
        let mut en = seg.docs_enum(entry)?;
        loop {
            let d = en.next_doc()?;
            if d == NO_MORE_DOCS {
                break;
            }
            f(d as u32);
        }
    }
    Ok(())
}

/// Bitset materialization (spec §4): per-term full postings scan, one bit
/// per hit doc.
pub(crate) fn materialize<L: LeafAccess>(
    seg: &L,
    entries: &[(u32, L::TermHandle)],
    has_freqs: bool,
) -> io::Result<FixedBitSet> {
    let mut bits = FixedBitSet::new(seg.max_doc() as usize);
    for (_, entry) in entries {
        for_each_doc(seg, entry, has_freqs, &mut |d| bits.set(d as usize))?;
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gm(pattern: &str, text: &[u8]) -> bool {
        let chars: Vec<char> = pattern.chars().collect();
        glob_match(&chars, text)
    }

    #[test]
    fn glob_match_basics() {
        assert!(gm("foo*", b"foo"));
        assert!(gm("foo*", b"foobar"));
        assert!(!gm("foo*", b"fo"));
        assert!(gm("fo?o", b"fooo"));
        assert!(!gm("fo?o", b"foo")); // ? matches exactly one char
        assert!(!gm("fo?o", b"fooxo"));
        assert!(gm("*foo", b"foo"));
        assert!(gm("*foo", b"barfoo"));
        assert!(!gm("*foo", b"foob"));
        assert!(gm("*", b"anything"));
        assert!(gm("*", b""));
        assert!(gm("a*b*c", b"aXbYc"));
        assert!(gm("a*b*c", b"abc"));
        assert!(!gm("a*b*c", b"acb"));
        assert!(gm("que?y3*", b"query39"));
        assert!(!gm("que?y3*", b"queue39"));
        // consecutive stars collapse semantically
        assert!(gm("a**b", b"aXXb"));
        // literal star-less patterns are exact
        assert!(gm("abc", b"abc"));
        assert!(!gm("abc", b"abd"));
    }

    #[test]
    fn glob_match_chars_semantics() {
        // '?' is one Unicode code point (WildcardQuery.toAutomaton :96-97),
        // not one byte
        assert!(gm("h?llo", "héllo".as_bytes()));
        assert!(!gm("h?llx", "héllo".as_bytes()));
        assert!(gm("h*llo", "héllo".as_bytes()));
        // multi-byte star content
        assert!(gm("*", "héllo".as_bytes()));
        // invalid UTF-8 term bytes never match a filter pattern
        assert!(!gm("h?llo", b"h\xffllo"));
    }

    #[test]
    fn wildcard_classification() {
        let p = WildcardPattern::parse(b"foo*");
        assert_eq!(p.class, WildcardClass::PurePrefix);
        assert_eq!(p.prefix, b"foo");
        let p = WildcardPattern::parse(b"fo?o*");
        assert_eq!(p.class, WildcardClass::PrefixFilter);
        assert_eq!(p.prefix, b"fo");
        let p = WildcardPattern::parse(b"*foo");
        assert_eq!(p.class, WildcardClass::FullScan);
        assert_eq!(p.prefix, b"");
        let p = WildcardPattern::parse(b"*");
        assert_eq!(p.class, WildcardClass::PurePrefix);
        assert_eq!(p.prefix, b"");
        let p = WildcardPattern::parse(b"?foo");
        assert_eq!(p.class, WildcardClass::FullScan);
        let p = WildcardPattern::parse(b"foo");
        assert_eq!(p.class, WildcardClass::Exact);
        // matches() shortcuts
        assert!(WildcardPattern::parse(b"foo*").matches(b"foobar"));
        assert!(WildcardPattern::parse(b"foo").matches(b"foo"));
        assert!(!WildcardPattern::parse(b"foo").matches(b"fooo"));
    }
}
