//! Query enum (search spec §3): Term, MatchAll, And, Or, Terms, Prefix,
//! Wildcard, Phrase. All queries have ConstantScore semantics.

use std::io;

use codec_lucene9::roaring::MaterializedBitmap;

use super::doc_iter::{
    ConjunctionDocIter, DisjunctionDocIter, MatchAllIter, PhraseDocIter, PointsDocIter,
    RoaringDocIter, SegmentDocIter,
};
use super::multi_term;
use super::roaring_exec;
use super::segment_reader::SegmentReader;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    Term {
        field: String,
        term: Vec<u8>,
    },
    MatchAll,
    And {
        field: String,
        terms: Vec<Vec<u8>>,
    },
    Or {
        field: String,
        terms: Vec<Vec<u8>>,
    },
    Terms {
        field: String,
        terms: Vec<Vec<u8>>,
    },
    Prefix {
        field: String,
        prefix: Vec<u8>,
    },
    Wildcard {
        field: String,
        pattern: Vec<u8>,
    },
    Phrase {
        field: String,
        terms: Vec<Vec<u8>>,
    },
    /// 1D point range (M6 spec §3.1), LongPoint/IntPoint `newRangeQuery`
    /// semantics: both ends inclusive; `low > high` is rejected with
    /// `Err(InvalidInput)` at execution (跨任务钉死接口).
    PointRange {
        field: String,
        low: i64,
        high: i64,
    },
}

impl Query {
    pub fn term(field: &str, term: &str) -> Query {
        Query::Term {
            field: field.to_string(),
            term: term.as_bytes().to_vec(),
        }
    }
    pub fn and(field: &str, terms: &[&str]) -> Query {
        Query::And {
            field: field.to_string(),
            terms: terms.iter().map(|t| t.as_bytes().to_vec()).collect(),
        }
    }
    pub fn or(field: &str, terms: &[&str]) -> Query {
        Query::Or {
            field: field.to_string(),
            terms: terms.iter().map(|t| t.as_bytes().to_vec()).collect(),
        }
    }

    /// Terms(IN) — Boolean SHOULD sugar (spec M2 §1): the doc union of the
    /// term set, executed via the <=16/>16 dual path (spec M2 §4).
    pub fn terms(field: &str, terms: &[&str]) -> Query {
        Query::Terms {
            field: field.to_string(),
            terms: terms.iter().map(|t| t.as_bytes().to_vec()).collect(),
        }
    }

    /// Prefix query (spec M2 §3): all docs whose term starts with `prefix`.
    pub fn prefix(field: &str, prefix: &str) -> Query {
        Query::Prefix {
            field: field.to_string(),
            prefix: prefix.as_bytes().to_vec(),
        }
    }

    /// Wildcard query with '*' and '?' (spec M2 §5 classification).
    pub fn wildcard(field: &str, pattern: &str) -> Query {
        Query::Wildcard {
            field: field.to_string(),
            pattern: pattern.as_bytes().to_vec(),
        }
    }

    /// Exact phrase query (slop=0, spec M2 §6): consecutive offsets.
    pub fn phrase(field: &str, terms: &[&str]) -> Query {
        Query::Phrase {
            field: field.to_string(),
            terms: terms.iter().map(|t| t.as_bytes().to_vec()).collect(),
        }
    }

    /// 1D point range query (M6 §3.1): inclusive both ends. 排他边界由
    /// 调用方 ±1 调整（避免 MIN/MAX 溢出特例）。
    pub fn point_range(field: &str, low: i64, high: i64) -> Query {
        Query::PointRange {
            field: field.to_string(),
            low,
            high,
        }
    }

    /// Multi-term queries (Terms/Prefix/Wildcard) share the Searcher::count
    /// dual path (popcount on the bitset path, iteration otherwise).
    pub(crate) fn is_multi_term(&self) -> bool {
        matches!(
            self,
            Query::Terms { .. } | Query::Prefix { .. } | Query::Wildcard { .. }
        )
    }

    /// Per-segment count shortcut: `Some(popcount)` when this query takes
    /// the bitset path in this segment, `None` otherwise (caller iterates).
    pub(crate) fn bitset_count(&self, seg: &mut SegmentReader) -> io::Result<Option<u64>> {
        match self {
            Query::Terms { field, terms } => {
                if terms.len() < 2 {
                    return Ok(None); // degenerate: empty set / single-term Term path
                }
                let Some((has_freqs, collected)) = multi_term::collect_direct(seg, field, terms)?
                else {
                    return Ok(Some(0)); // unknown field: empty hit set
                };
                multi_term::bitset_count(seg, has_freqs, &collected)
            }
            Query::Prefix { field, prefix } => {
                let Some((has_freqs, collected)) = multi_term::collect_prefix(seg, field, prefix)?
                else {
                    return Ok(Some(0));
                };
                multi_term::bitset_count(seg, has_freqs, &collected)
            }
            Query::Wildcard { field, pattern } => {
                let pat = multi_term::WildcardPattern::parse(pattern);
                let Some((has_freqs, collected)) = multi_term::collect_wildcard(seg, field, &pat)?
                else {
                    return Ok(Some(0));
                };
                multi_term::bitset_count(seg, has_freqs, &collected)
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn segment_iterator(
        &self,
        seg: &mut SegmentReader,
        needs_freq: bool,
    ) -> io::Result<Option<SegmentDocIter>> {
        match self {
            Query::MatchAll => Ok(Some(SegmentDocIter::All(MatchAllIter::new(seg.max_doc())))),
            Query::Term { field, term } => {
                let Some((has_freqs, entry)) = seg.seek_term(field, term)? else {
                    return Ok(None);
                };
                // M4 §6 Term 路径：校验通过的零拷贝 full 视图 → 字节游标
                // 迭代；needs_freq（bitmap 无 freq）与任何校验失败保持
                // postings 枚举。
                if !needs_freq {
                    if let Some(v) = seg.open_term_bitmap(&entry)? {
                        return Ok(Some(SegmentDocIter::Roaring(RoaringDocIter::new(v))));
                    }
                }
                if has_freqs {
                    Ok(Some(SegmentDocIter::Freqs(
                        seg.docs_freqs_enum(&entry, needs_freq)?,
                    )))
                } else {
                    Ok(Some(SegmentDocIter::Docs(seg.docs_enum(&entry)?)))
                }
            }
            Query::And { field, terms } => and_segment_iterator(seg, field, terms, needs_freq),
            Query::Or { field, terms } => or_segment_iterator(seg, field, terms, needs_freq),
            Query::Terms { field, terms } => {
                if terms.is_empty() {
                    return Ok(None);
                }
                if terms.len() == 1 {
                    return Query::Term {
                        field: field.clone(),
                        term: terms[0].clone(),
                    }
                    .segment_iterator(seg, needs_freq);
                }
                let Some((has_freqs, collected)) = multi_term::collect_direct(seg, field, terms)?
                else {
                    return Ok(None);
                };
                multi_term::segment_iterator(seg, field, has_freqs, &collected, needs_freq)
            }
            Query::Prefix { field, prefix } => {
                let Some((has_freqs, collected)) = multi_term::collect_prefix(seg, field, prefix)?
                else {
                    return Ok(None);
                };
                multi_term::segment_iterator(seg, field, has_freqs, &collected, needs_freq)
            }
            Query::Wildcard { field, pattern } => {
                let pat = multi_term::WildcardPattern::parse(pattern);
                let Some((has_freqs, collected)) = multi_term::collect_wildcard(seg, field, &pat)?
                else {
                    return Ok(None);
                };
                multi_term::segment_iterator(seg, field, has_freqs, &collected, needs_freq)
            }
            Query::Phrase { field, terms } => {
                if terms.is_empty() {
                    return Ok(None);
                }
                if terms.len() == 1 {
                    return Query::Term {
                        field: field.clone(),
                        term: terms[0].clone(),
                    }
                    .segment_iterator(seg, needs_freq);
                }
                Ok(PhraseDocIter::new(seg, field, terms)?.map(SegmentDocIter::Phrase))
            }
            Query::PointRange { field, low, high } => {
                let Some(bm) = point_range_bitmap(seg, field, *low, *high)? else {
                    return Ok(None);
                };
                Ok(Some(SegmentDocIter::Points(PointsDocIter::new(bm))))
            }
        }
    }
}

/// PointRange 共享物化入口（`segment_iterator` 与 `Searcher::count` 同一
/// 物化，spec §3.3）：`low > high` → `Err(InvalidInput)`（跨任务钉死接口；
/// Lucene 9.12.3 对该情形不报错而返回 0 命中，PointRangeQuery.checkArgs
/// :100-110——此处是钉死的 Rust 显式错误面）。段内命中经 visitor 收集、
/// sort + dedup 后建 `MaterializedBitmap`；`None` = 未知字段 / 非 point
/// 字段 / 段无 points / 空命中。
pub(crate) fn point_range_bitmap(
    seg: &mut SegmentReader,
    field: &str,
    low: i64,
    high: i64,
) -> io::Result<Option<MaterializedBitmap>> {
    if low > high {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("point range low ({low}) > high ({high})"),
        ));
    }
    let Some(points) = seg.points_reader() else {
        return Ok(None);
    };
    let mut docs: Vec<u32> = Vec::new();
    // 多值点逐值回调（spec §3.2）；BKD 访问序按 (value, doc) 非 doc 序，
    // 统一 sort + dedup 再建 bitmap（Bitmap::of 快路径要求升序，去重同时
    // 解决多值点重复计数）。
    points.intersect(field, low, high, &mut |_value, doc| docs.push(doc as u32))?;
    docs.sort_unstable();
    docs.dedup();
    if docs.is_empty() {
        return Ok(None);
    }
    Ok(Some(MaterializedBitmap::of(&docs)))
}

/// And/Or arm bodies of `Query::segment_iterator`, outlined into their own
/// frames: `SegmentDocIter` is 18.7KB (every postings enum owns an
/// `IndexInput` with an 8KB inline buffer) and a debug build gives each
/// arm's temporaries disjoint slots in the one dispatcher frame, so inlining
/// the M3 roaring temporaries into both arms pushed the
/// Terms/Prefix/Wildcard → Or → Term rewrite chain over the 2MiB default
/// test-thread stack. The bodies are the spec §5 three-tier rule, per arm.
fn and_segment_iterator(
    seg: &mut SegmentReader,
    field: &String,
    terms: &[Vec<u8>],
    needs_freq: bool,
) -> io::Result<Option<SegmentDocIter>> {
    if terms.len() < 2 {
        return if let Some(t) = terms.first() {
            Query::Term {
                field: field.clone(),
                term: t.clone(),
            }
            .segment_iterator(seg, needs_freq)
        } else {
            Ok(None)
        };
    }
    let Some((has_freqs, entries)) = roaring_exec::collect_bool_entries(seg, field, terms, true)?
    else {
        return Ok(None);
    };
    // M3 §5 档 1/2（bitmap 无 freq：needs_freq 永远档 3）
    if !needs_freq {
        if let Some(it) = roaring_exec::segment_iterator(seg, &entries, has_freqs, true)? {
            return Ok(Some(it));
        }
    }
    Ok(Some(SegmentDocIter::And(ConjunctionDocIter::new(
        seg, field, &entries, needs_freq,
    )?)))
}

/// See `and_segment_iterator` — the Or half of the same rule.
fn or_segment_iterator(
    seg: &mut SegmentReader,
    field: &String,
    terms: &[Vec<u8>],
    needs_freq: bool,
) -> io::Result<Option<SegmentDocIter>> {
    if terms.len() < 2 {
        return if let Some(t) = terms.first() {
            Query::Term {
                field: field.clone(),
                term: t.clone(),
            }
            .segment_iterator(seg, needs_freq)
        } else {
            Ok(None)
        };
    }
    let Some((has_freqs, entries)) = roaring_exec::collect_bool_entries(seg, field, terms, false)?
    else {
        return Ok(None);
    };
    // M3 §5 档 1/2
    if !needs_freq {
        if let Some(it) = roaring_exec::segment_iterator(seg, &entries, has_freqs, false)? {
            return Ok(Some(it));
        }
    }
    Ok(Some(SegmentDocIter::Or(DisjunctionDocIter::new(
        seg, field, &entries, needs_freq,
    )?)))
}
