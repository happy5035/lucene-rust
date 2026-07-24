//! Multi-term query execution (search spec M2 §4): collect the term set of a
//! Terms/Prefix/Wildcard query for one segment, then dispatch on the Lucene 9
//! blended threshold — at most 16 terms rewrite to `Query::Or`
//! (AbstractMultiTermQueryConstantScoreWrapper.java:43-44), more than 16
//! materialize a per-segment FixedBitSet (Lucene's DocIdSet rewrite,
//! MultiTermQueryConstantScoreBlendedWrapper.java:55-120). No enumeration cap
//! (Lucene 9 dropped TooManyClauses for MTQs).

use std::io;

use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::terms_read::TermEntry;

use super::bitset::FixedBitSet;
use super::doc_iter::{BitsetDocIter, SegmentDocIter};
use super::query::Query;
use super::segment_reader::SegmentReader;

/// AbstractMultiTermQueryConstantScoreWrapper.java:44.
pub(crate) const BOOLEAN_REWRITE_THRESHOLD: usize = 16;

/// Terms of one query present in one segment's dictionary, df-sorted
/// (spec §4: 集合收集后按 df 排序交给双路).
pub(crate) struct CollectedTerms {
    pub terms: Vec<Vec<u8>>,
    pub entries: Vec<(u32, TermEntry)>,
}

impl CollectedTerms {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn sort_by_df(&mut self) {
        let mut pairs: Vec<(Vec<u8>, (u32, TermEntry))> =
            self.terms.drain(..).zip(self.entries.drain(..)).collect();
        pairs.sort_by_key(|(_, (df, _))| *df);
        for (t, e) in pairs {
            self.terms.push(t);
            self.entries.push(e);
        }
    }
}

/// Direct term-set collection (Terms/IN): seek each term, keep the present
/// ones. `None` = unknown field (empty-hit semantics, same as TermQuery).
pub(crate) fn collect_direct(
    seg: &mut SegmentReader,
    field: &str,
    terms: &[Vec<u8>],
) -> io::Result<Option<(bool, CollectedTerms)>> {
    let Some(has_freqs) = seg.field_has_freqs(field) else {
        return Ok(None);
    };
    let mut collected = CollectedTerms {
        terms: Vec::new(),
        entries: Vec::new(),
    };
    for t in terms {
        if let Some((_, entry)) = seg.seek_term(field, t)? {
            collected.terms.push(t.clone());
            collected.entries.push((entry.doc_freq, entry));
        }
    }
    collected.sort_by_df();
    Ok(Some((has_freqs, collected)))
}

/// Prefix collection (spec §3): seek_ceil(prefix) then next() until
/// !starts_with(prefix). `None` = unknown field (empty-hit semantics).
pub(crate) fn collect_prefix(
    seg: &mut SegmentReader,
    field: &str,
    prefix: &[u8],
) -> io::Result<Option<(bool, CollectedTerms)>> {
    let Some(has_freqs) = seg.field_has_freqs(field) else {
        return Ok(None);
    };
    let mut collected = CollectedTerms {
        terms: Vec::new(),
        entries: Vec::new(),
    };
    {
        let Some(mut it) = seg.terms_iter(field) else {
            return Ok(None);
        };
        it.seek_ceil(prefix)?;
        while let Some((term, entry)) = it.next()? {
            if !term.starts_with(prefix) {
                break;
            }
            collected.entries.push((entry.doc_freq, entry));
            collected.terms.push(term);
        }
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
        WildcardPattern {
            pattern: pattern.to_vec(),
            prefix,
            class,
        }
    }

    pub(crate) fn matches(&self, term: &[u8]) -> bool {
        match self.class {
            WildcardClass::Exact => term == self.pattern.as_slice(),
            WildcardClass::PurePrefix => term.starts_with(&self.prefix),
            _ => match std::str::from_utf8(&self.pattern) {
                Ok(p) => glob_match(p, term),
                Err(_) => false, // non-UTF-8 pattern: matches nothing
            },
        }
    }
}

/// Classic two-pointer glob with '*' backtracking, iterated over `char`s so
/// '?' matches exactly one code point (WildcardQuery.toAutomaton :84-114:
/// WILDCARD_CHAR → Automata.makeAnyChar, WILDCARD_STRING → makeAnyString).
pub(crate) fn glob_match(pattern: &str, text: &[u8]) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = match std::str::from_utf8(text) {
        Ok(s) => s.chars().collect(),
        Err(_) => return false,
    };
    let (mut i, mut j) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None; // (pattern idx after '*', text retry idx)
    while j < t.len() {
        if i < p.len() && (p[i] == '?' || p[i] == t[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
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
    while i < p.len() && p[i] == '*' {
        i += 1;
    }
    i == p.len()
}

/// Wildcard collection (spec §5): Exact → direct seek; PurePrefix → prefix
/// enumeration with zero filtering; PrefixFilter → prefix enumeration +
/// glob tail filter; FullScan → whole-dictionary scan + glob filter.
pub(crate) fn collect_wildcard(
    seg: &mut SegmentReader,
    field: &str,
    pat: &WildcardPattern,
) -> io::Result<Option<(bool, CollectedTerms)>> {
    match pat.class {
        WildcardClass::Exact => collect_direct(seg, field, std::slice::from_ref(&pat.pattern)),
        WildcardClass::PurePrefix => collect_prefix(seg, field, &pat.prefix),
        WildcardClass::PrefixFilter | WildcardClass::FullScan => {
            let Some(has_freqs) = seg.field_has_freqs(field) else {
                return Ok(None);
            };
            let mut collected = CollectedTerms {
                terms: Vec::new(),
                entries: Vec::new(),
            };
            {
                let Some(mut it) = seg.terms_iter(field) else {
                    return Ok(None);
                };
                if pat.class == WildcardClass::PrefixFilter {
                    it.seek_ceil(&pat.prefix)?;
                }
                while let Some((term, entry)) = it.next()? {
                    if pat.class == WildcardClass::PrefixFilter && !term.starts_with(&pat.prefix) {
                        break;
                    }
                    if pat.matches(&term) {
                        collected.entries.push((entry.doc_freq, entry));
                        collected.terms.push(term);
                    }
                }
            }
            collected.sort_by_df();
            Ok(Some((has_freqs, collected)))
        }
    }
}

/// Threshold dispatch (spec §4): <=16 terms rewrite to `Query::Or` (heap
/// merge, zero new execution code); >16 materialize a FixedBitSet.
pub(crate) fn segment_iterator(
    seg: &mut SegmentReader,
    field: &str,
    has_freqs: bool,
    collected: &CollectedTerms,
    needs_freq: bool,
) -> io::Result<Option<SegmentDocIter>> {
    if collected.is_empty() {
        return Ok(None);
    }
    if collected.len() <= BOOLEAN_REWRITE_THRESHOLD {
        return Query::Or {
            field: field.to_string(),
            terms: collected.terms.clone(),
        }
        .segment_iterator(seg, needs_freq);
    }
    let bits = materialize(seg, &collected.entries, has_freqs)?;
    Ok(Some(SegmentDocIter::Bitset(BitsetDocIter::new(bits))))
}

/// Count fast path (spec §4: count 路径直接 popcount): `Some(popcount)` on
/// the bitset path, `None` when the OR path applies (caller iterates).
pub(crate) fn bitset_count(
    seg: &SegmentReader,
    has_freqs: bool,
    collected: &CollectedTerms,
) -> io::Result<Option<u64>> {
    if collected.len() <= BOOLEAN_REWRITE_THRESHOLD {
        return Ok(None);
    }
    Ok(Some(
        materialize(seg, &collected.entries, has_freqs)?.popcount(),
    ))
}

/// Per-term postings doc scan shared by the M2 bitset materialization and
/// the M3 tier-2 roaring materialization: feeds every doc of `entry`'s
/// postings to `f` in ascending order. Uses no-freq enums on freqs fields —
/// the materialized sets carry no per-doc freq (ConstantScore).
pub(crate) fn for_each_doc(
    seg: &SegmentReader,
    entry: &TermEntry,
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
pub(crate) fn materialize(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
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

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("foo*", b"foo"));
        assert!(glob_match("foo*", b"foobar"));
        assert!(!glob_match("foo*", b"fo"));
        assert!(glob_match("fo?o", b"fooo"));
        assert!(!glob_match("fo?o", b"foo")); // ? matches exactly one char
        assert!(!glob_match("fo?o", b"fooxo"));
        assert!(glob_match("*foo", b"foo"));
        assert!(glob_match("*foo", b"barfoo"));
        assert!(!glob_match("*foo", b"foob"));
        assert!(glob_match("*", b"anything"));
        assert!(glob_match("*", b""));
        assert!(glob_match("a*b*c", b"aXbYc"));
        assert!(glob_match("a*b*c", b"abc"));
        assert!(!glob_match("a*b*c", b"acb"));
        assert!(glob_match("que?y3*", b"query39"));
        assert!(!glob_match("que?y3*", b"queue39"));
        // consecutive stars collapse semantically
        assert!(glob_match("a**b", b"aXXb"));
        // literal star-less patterns are exact
        assert!(glob_match("abc", b"abc"));
        assert!(!glob_match("abc", b"abd"));
    }

    #[test]
    fn glob_match_chars_semantics() {
        // '?' is one Unicode code point (WildcardQuery.toAutomaton :96-97),
        // not one byte
        assert!(glob_match("h?llo", "héllo".as_bytes()));
        assert!(!glob_match("h?llx", "héllo".as_bytes()));
        assert!(glob_match("h*llo", "héllo".as_bytes()));
        // multi-byte star content
        assert!(glob_match("*", "héllo".as_bytes()));
        // invalid UTF-8 term bytes never match a filter pattern
        assert!(!glob_match("h?llo", b"h\xffllo"));
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
