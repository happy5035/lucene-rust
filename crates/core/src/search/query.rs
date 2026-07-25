//! Query enum (search spec §3 + M6 §2.1): Term, MatchAll, And, Or, Terms,
//! Prefix, Wildcard, Phrase, Bool. All queries have ConstantScore semantics.

use std::io;

use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::roaring::MaterializedBitmap;

use super::doc_iter::{
    ConjOverDocIter, ConjunctionDocIter, DisjOverDocIter, DisjunctionDocIter, DocIter,
    ExcludingDocIter, MatchAllIter, PhraseDocIter, PointsDocIter, RoaringDocIter, SegmentDocIter,
};
use super::multi_term;
use super::roaring_exec;
use super::segment_reader::SegmentReader;

/// Boolean clause occur (spec M6 §2.1)：MUST / SHOULD / MUST_NOT；
/// FILTER 不做（ConstantScore 下与 MUST 等价，spec §0 拍板）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Occur {
    Must,
    Should,
    MustNot,
}

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
    /// Nested Boolean query (spec M6 §2.1)：子查询可为任意变体（含 Bool
    /// 自身），跨字段。执行语义见 §2.2，拍平规则见 §2.4。
    Bool {
        clauses: Vec<(Occur, Query)>,
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

    /// Nested Boolean query (spec M6 §2.1)。And/Or 平铺变体保留不删
    /// （bench 与电池在用）；新查询一律用 Bool。
    pub fn bool(clauses: Vec<(Occur, Query)>) -> Query {
        Query::Bool { clauses }
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
            Query::Bool { clauses } => bool_segment_iterator(seg, clauses, needs_freq),
        }
    }
}

/// spec §2.4 拍平形状判定：全部子句同一 occur（全 MUST 或全 SHOULD），
/// 且递归展开**同形**嵌套 Bool 后全部叶子是同字段 Term →
/// Some((is_and, field, terms))（field/terms 借自原查询，零克隆）；
/// 其余形状（混合 occur / 跨字段 / 非 Term 叶子 / 异形嵌套 / 空子树 /
/// 首子句 MUST_NOT）→ None。嵌套 Bool 的 occur 必须与外层一致：
/// `(AND t1 (OR t2 t3))` 是 t1 ∧ (t2∨t3)，不是平 AND，不得拍平。
pub(crate) fn flatten_bool<'q>(
    clauses: &[(Occur, &'q Query)],
) -> Option<(bool, &'q str, Vec<&'q [u8]>)> {
    let is_and = match clauses.first()?.0 {
        Occur::Must => true,
        Occur::Should => false,
        Occur::MustNot => return None,
    };
    let mut field: Option<&'q str> = None;
    let mut terms: Vec<&'q [u8]> = Vec::new();
    for &(occur, q) in clauses {
        let same = matches!(
            (occur, is_and),
            (Occur::Must, true) | (Occur::Should, false)
        );
        if !same {
            return None;
        }
        match q {
            Query::Term { field: f, term } => {
                if field.is_some_and(|hf| hf != f.as_str()) {
                    return None;
                }
                field = Some(f.as_str());
                terms.push(term.as_slice());
            }
            Query::Bool { clauses: sub } => {
                let sub_refs: Vec<(Occur, &Query)> = sub.iter().map(|(o, q)| (*o, q)).collect();
                let (sub_and, sub_field, sub_terms) = flatten_bool(&sub_refs)?;
                if sub_and != is_and {
                    return None;
                }
                if field.is_some_and(|hf| hf != sub_field) {
                    return None;
                }
                field = Some(sub_field);
                terms.extend(sub_terms);
            }
            _ => return None,
        }
    }
    Some((is_and, field?, terms))
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
/// M6 T-A：泛型化 `T: AsRef<[u8]>`——Bool 拍平（spec §2.4）借引用 terms
/// 与 And/Or 的 `Vec<u8>` 共用同一函数体。
fn and_segment_iterator<T: AsRef<[u8]>>(
    seg: &mut SegmentReader,
    field: &str,
    terms: &[T],
    needs_freq: bool,
) -> io::Result<Option<SegmentDocIter>> {
    if terms.len() < 2 {
        return if let Some(t) = terms.first() {
            Query::Term {
                field: field.to_string(),
                term: t.as_ref().to_vec(),
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
fn or_segment_iterator<T: AsRef<[u8]>>(
    seg: &mut SegmentReader,
    field: &str,
    terms: &[T],
    needs_freq: bool,
) -> io::Result<Option<SegmentDocIter>> {
    if terms.len() < 2 {
        return if let Some(t) = terms.first() {
            Query::Term {
                field: field.to_string(),
                term: t.as_ref().to_vec(),
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

/// Bool 分派体（outline 自由函数，与 and/or_segment_iterator 同因：
/// SegmentDocIter 18.7KB，分派帧内联多分支临时量会在 debug build 溢出
/// 2MiB 测试线程栈——见 and_segment_iterator 上方注释）。needs_freq 按
/// spec §2.3 恒 false 处理：freq_sum 已拒绝 Bool，组合语义下 freq 无
/// 定义，子句一律按 no-freq 打开（bitmap/roaring 路径不受限）。
fn bool_segment_iterator(
    seg: &mut SegmentReader,
    clauses: &[(Occur, Query)],
    _needs_freq: bool,
) -> io::Result<Option<SegmentDocIter>> {
    if clauses.is_empty() {
        return Ok(None);
    }
    // 拍平（spec §2.4）：纯 MUST / 纯 SHOULD 同字段 Term 子树 → And/Or
    // 三档（roaring 档 1/2、PFOR 档 3），与平铺变体同一引擎。
    let refs: Vec<(Occur, &Query)> = clauses.iter().map(|(o, q)| (*o, q)).collect();
    if let Some((is_and, field, terms)) = flatten_bool(&refs) {
        return if is_and {
            and_segment_iterator(seg, field, &terms, false)
        } else {
            or_segment_iterator(seg, field, &terms, false)
        };
    }
    // 通用装配（spec §2.2 三态 + §2.3 组合器）。
    let mut musts: Vec<SegmentDocIter> = Vec::new();
    let mut shoulds: Vec<SegmentDocIter> = Vec::new();
    let mut nots: Vec<SegmentDocIter> = Vec::new();
    let mut has_must_not = false;
    for (occur, q) in clauses {
        let it = q.segment_iterator(seg, false)?;
        match (occur, it) {
            (Occur::Must, Some(it)) => musts.push(it),
            // MUST 子句段内缺失 → 全段空（spec §2.2/§2.4 collect 同款语义）
            (Occur::Must, None) => return Ok(None),
            (Occur::Should, it) => {
                if let Some(it) = it {
                    shoulds.push(it);
                }
            }
            (Occur::MustNot, it) => {
                has_must_not = true;
                if let Some(it) = it {
                    nots.push(it);
                }
            }
        }
    }
    // 正集三态：MUST 合取 / 纯 SHOULD 并集 / 纯 MUST_NOT 的 MatchAll。
    // 注意用 has_must_not 而非 nots.is_empty()：NOT 一个段内缺失的 term
    // 语义是 MatchAll − ∅ = MatchAll（Lucene 同款），不是空。
    let positive = if !musts.is_empty() {
        conj_over(musts)?.expect("musts non-empty")
    } else if !shoulds.is_empty() {
        disj_over(shoulds)?.expect("shoulds non-empty")
    } else if has_must_not {
        SegmentDocIter::All(MatchAllIter::new(seg.max_doc()))
    } else {
        return Ok(None); // SHOULD 全缺且无 MUST/MUST_NOT → 段内空
    };
    // 排除集：多个 MUST_NOT 先析取合成一个 prohibited（spec §2.3）。
    Ok(Some(match disj_over(nots)? {
        Some(prohibited) => SegmentDocIter::Excluding(ExcludingDocIter::new(positive, prohibited)),
        None => positive,
    }))
}

/// Vec 装配：≥2 子句包 ConjOver，单子句直接返回（零包装税），空 → None。
fn conj_over(mut its: Vec<SegmentDocIter>) -> io::Result<Option<SegmentDocIter>> {
    match its.len() {
        0 => Ok(None),
        1 => Ok(its.pop()),
        _ => Ok(Some(SegmentDocIter::ConjOver(ConjOverDocIter::new(its)?))),
    }
}

/// Vec 装配：≥2 子句包 DisjOver，单子句直接返回，空 → None。
fn disj_over(mut its: Vec<SegmentDocIter>) -> io::Result<Option<SegmentDocIter>> {
    match its.len() {
        0 => Ok(None),
        1 => Ok(its.pop()),
        _ => Ok(Some(SegmentDocIter::DisjOver(DisjOverDocIter::new(its)?))),
    }
}

/// spec §2.5 Bool per-segment count：与迭代同一结构——拍平形命中
/// roaring count 快路径（档 1 cardinality 折叠 / 档 2 驱动计数）；纯
/// MUST_NOT 走 maxDoc − prohibited count（避免全量迭代）；其余形状驱动
/// 组合迭代器逐 doc 计数。
pub(crate) fn bool_segment_count(
    seg: &mut SegmentReader,
    clauses: &[(Occur, Query)],
) -> io::Result<u64> {
    // 拍平快路径（§2.4 同形状）：roaring count；档 3 落档（None）与非
    // 拍平形继续向下走通用路径。
    let refs: Vec<(Occur, &Query)> = clauses.iter().map(|(o, q)| (*o, q)).collect();
    if let Some((is_and, field, terms)) = flatten_bool(&refs) {
        if terms.len() >= 2 {
            let Some((has_freqs, entries)) =
                roaring_exec::collect_bool_entries(seg, field, &terms, is_and)?
            else {
                return Ok(0); // 未知字段 / AND 缺子句 / OR 全缺 → 段内空
            };
            if let Some(c) = roaring_exec::count(seg, &entries, has_freqs, is_and)? {
                return Ok(c);
            }
        }
    }
    // 纯 MUST_NOT（§2.2/§2.5）：MatchAll 排除，count = maxDoc − prohibited。
    if !clauses.is_empty() && clauses.iter().all(|(o, _)| *o == Occur::MustNot) {
        let prohibited = prohibited_count(seg, clauses)?;
        return Ok(seg.max_doc() as u64 - prohibited);
    }
    // 通用：组合迭代器逐 doc 计数。
    drive_count(bool_segment_iterator(seg, clauses, false)?)
}

/// 纯 MUST_NOT 的 prohibited 侧 count：子句换 SHOULD 视角取并集——拍平
/// OR 形命中 roaring count 快路径，否则驱动 DisjOver（单子句直接驱动）。
fn prohibited_count(seg: &mut SegmentReader, clauses: &[(Occur, Query)]) -> io::Result<u64> {
    let as_should: Vec<(Occur, &Query)> = clauses.iter().map(|(_, q)| (Occur::Should, q)).collect();
    if let Some((_, field, terms)) = flatten_bool(&as_should) {
        if terms.len() >= 2 {
            let Some((has_freqs, entries)) =
                roaring_exec::collect_bool_entries(seg, field, &terms, false)?
            else {
                return Ok(0);
            };
            if let Some(c) = roaring_exec::count(seg, &entries, has_freqs, false)? {
                return Ok(c);
            }
        }
    }
    let mut nots: Vec<SegmentDocIter> = Vec::new();
    for (_, q) in clauses {
        if let Some(it) = q.segment_iterator(seg, false)? {
            nots.push(it);
        }
    }
    drive_count(disj_over(nots)?)
}

/// 驱动迭代器到穷尽计数（None = 段内空）。
fn drive_count(it: Option<SegmentDocIter>) -> io::Result<u64> {
    let mut n = 0u64;
    if let Some(mut it) = it {
        loop {
            if it.next_doc()? == NO_MORE_DOCS {
                break;
            }
            n += 1;
        }
    }
    Ok(n)
}
