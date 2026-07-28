//! Query enum (search spec §3 + M6 §2.1): Term, MatchAll, And, Or, Terms,
//! Prefix, Wildcard, Phrase, Bool. All queries have ConstantScore semantics.

use std::io;

use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::roaring::MaterializedBitmap;

use super::doc_iter::{
    ConjOverDocIter, ConjunctionDocIter, DisjOverDocIter, DisjunctionDocIter, DocIter,
    ExcludingDocIter, MatchAllIter, MaterializedDocIter, PhraseDocIter, PostingsIter,
    RoaringDocIter, SegmentDocIter,
};
use super::leaf_access::LeafAccess;
use super::multi_term;
use super::roaring_exec;
use super::segment_reader::bitmap_enabled;

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
    pub(crate) fn bitset_count<L: LeafAccess>(&self, seg: &mut L) -> io::Result<Option<u64>> {
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

    pub(crate) fn segment_iterator<L: LeafAccess>(
        &self,
        seg: &mut L,
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
                    Ok(Some(seg.docs_freqs_enum(&entry, needs_freq)?))
                } else {
                    Ok(Some(seg.docs_enum(&entry)?))
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
                Ok(Some(SegmentDocIter::Materialized(
                    MaterializedDocIter::new(bm),
                )))
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
pub(crate) fn point_range_bitmap<L: LeafAccess>(
    seg: &mut L,
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
    // P1-3：doc-only 遍历（Inside 叶跳过值解码）+ 容器级增量插入。
    // BKD 访问序按 (value, doc) 非 doc 序——旧路径 Vec+sort_unstable+dedup
    // 再 Bitmap::of（排序 ~10ns/doc，1M 点 ~10ms）；croaring add 无序幂等
    // （~5ns/doc），多值点重复回调由 bitmap 集合语义天然去重（spec §3.2
    // 去重契约不变，从调用方显式 dedup 移入 bitmap 构造）。
    let mut bm = MaterializedBitmap::empty();
    points.intersect_docs(field, low, high, &mut |doc| bm.add(doc))?;
    if bm.cardinality() == 0 {
        return Ok(None);
    }
    Ok(Some(bm))
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
fn and_segment_iterator<L: LeafAccess, T: AsRef<[u8]>>(
    seg: &mut L,
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
    // 档 3：经 trait 逐 clause 建 postings 迭代器，from_iters 装配合取。
    let mut sub = Vec::with_capacity(entries.len());
    for (_, entry) in &entries {
        let it = if has_freqs {
            seg.docs_freqs_enum(entry, needs_freq)?
        } else {
            seg.docs_enum(entry)?
        };
        sub.push(PostingsIter::from_segment_iter(it));
    }
    Ok(Some(SegmentDocIter::And(ConjunctionDocIter::from_iters(sub)?)))
}

/// See `and_segment_iterator` — the Or half of the same rule.
fn or_segment_iterator<L: LeafAccess, T: AsRef<[u8]>>(
    seg: &mut L,
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
    // 档 3：经 trait 逐 clause 建 postings 迭代器，from_iters 装配析取。
    let mut sub = Vec::with_capacity(entries.len());
    for (_, entry) in &entries {
        let it = if has_freqs {
            seg.docs_freqs_enum(entry, needs_freq)?
        } else {
            seg.docs_enum(entry)?
        };
        sub.push(PostingsIter::from_segment_iter(it));
    }
    Ok(Some(SegmentDocIter::Or(DisjunctionDocIter::from_iters(sub)?)))
}

/// Bool 分派体（outline 自由函数，与 and/or_segment_iterator 同因：
/// SegmentDocIter 18.7KB，分派帧内联多分支临时量会在 debug build 溢出
/// 2MiB 测试线程栈——见 and_segment_iterator 上方注释）。needs_freq 按
/// spec §2.3 恒 false 处理：freq_sum 已拒绝 Bool，组合语义下 freq 无
/// 定义，子句一律按 no-freq 打开（bitmap/roaring 路径不受限）。
fn bool_segment_iterator<L: LeafAccess>(
    seg: &mut L,
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
    // P1-1 物化先行：通用组合器形状先试全树物化 fold（count 路径同款
    // materialize_bool_bitmap，正集三态与组合语义逐条镜像迭代路径，
    // Phrase 叶子经 drive_materialize 走 matches() 两阶段协议）。
    // 动机：ExcludingDocIter 逐候选 prohibited.advance(d)——bitmap 禁集上
    // BitmapCursor::advance 每次丢 512-doc 批缓存重 seek，稠密禁集
    // （level 词 df≈200k）实测 10–20× 慢于 PFOR 同路径（nothi 257ms vs
    // 18.6ms）。容器级 and/andnot fold + MaterializedDocIter 顺序迭代
    // 消除该病理，ConjOver/DisjOver 同形状一并受益。超预算 → 回落下方
    // 通用装配（保持惰性，topN 早停形状不受损）。
    //
    // 门控：物化 ROI 来自 bitmap/BKD 容器级 fold。纯 postings 读路径
    // （RL_BITMAP=0 kill-switch，bitmap_enabled()==false）下物化要全扫
    // 各子句，小 MUST + 大 NOT 形状反而失去惰性优势（实测 mnfmh/
    // multinot 回归 2–4×），而惰性二指针在 PFOR 禁集上已近优 → 跳过
    // 物化走下方通用装配。PointRange 叶子恒物化（BKD 路径与位图开关
    // 无关），且 Points 二指针同受批游标 advance 丢批病理 → 恒放行。
    let bitmap_source = bitmap_enabled()
        || clauses
            .iter()
            .any(|(_, q)| matches!(q, Query::PointRange { .. }));
    if bitmap_source {
        let budget = FOLD_COST_FACTOR * seg.max_doc() as u64;
        let mut cost = 0u64;
        match materialize_bool_bitmap(seg, clauses, budget, &mut cost)? {
            MatOutcome::Hits(bm) => {
                if bm.cardinality() == 0 {
                    // 段内空，与通用装配的三种 None 形态同语义（顺带对齐
                    // count 路径与 Lucene minShouldMatch=1：SHOULD 全缺 +
                    // MUST_NOT 存在 → 空，而非 MatchAll − prohibited）。
                    return Ok(None);
                }
                return Ok(Some(SegmentDocIter::Materialized(
                    MaterializedDocIter::new(bm),
                )));
            }
            MatOutcome::OverBudget => {}
        }
    }
    // 通用装配（spec §2.2 三态 + §2.3 组合器）——物化超预算的回落路径。
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
        _ => {
            its.sort_by_key(|it| it.cost_estimate());
            Ok(Some(SegmentDocIter::ConjOver(ConjOverDocIter::new(its)?)))
        }
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

/// M7 §3.2 fold 成本护栏：估计物化成本（各叶子 Σdf；Phrase 叶子 = 各
/// term df 和；PointRange 叶子 = maxDoc）> FOLD_COST_FACTOR × maxDoc 时
/// 回落 drive_count（防病态形状回退）。bench 校准后可调。
const FOLD_COST_FACTOR: u64 = 4;

/// T-B 递归物化结果（spec §3.1）。
pub(crate) enum MatOutcome {
    /// 命中集（可为空 bitmap：未知字段 / 缺子句 / 零命中的统一形状）
    Hits(MaterializedBitmap),
    /// 估计物化成本超预算——调用方回落 drive_count
    OverBudget,
}

/// 单 term 叶子物化：有内联 bitmap → 容器级拷贝；无 → postings 全量
/// 扫描（O(df)）。cost 累加 df，超 budget → OverBudget。
fn term_entry_bitmap<L: LeafAccess>(
    seg: &L,
    entry: &L::TermHandle,
    has_freqs: bool,
    budget: u64,
    cost: &mut u64,
) -> io::Result<MatOutcome> {
    let df = seg.term_doc_freq(entry) as u64;
    *cost += df;
    if *cost > budget {
        return Ok(MatOutcome::OverBudget);
    }
    if let Some(f) = seg.open_term_bitmap(entry)? {
        return Ok(MatOutcome::Hits(f.to_materialized()));
    }
    let mut docs = Vec::with_capacity(df as usize);
    multi_term::for_each_doc(seg, entry, has_freqs, &mut |d| docs.push(d))?;
    Ok(MatOutcome::Hits(MaterializedBitmap::of(&docs)))
}

/// 递归把任意查询物化为段内 doc bitmap（M7 §3.1，**仅服务 count**：无
/// 提前终止，物化不亏——这是与迭代路径的本质区别）。成本经共享的
/// `cost` 累加器记账，任一叶子超 budget 全树 OverBudget。
fn materialize_query_bitmap<L: LeafAccess>(
    seg: &mut L,
    query: &Query,
    budget: u64,
    cost: &mut u64,
) -> io::Result<MatOutcome> {
    match query {
        Query::MatchAll => Ok(MatOutcome::Hits(MaterializedBitmap::full(
            seg.max_doc() as u32
        ))),
        Query::Term { field, term } => match seg.seek_term(field, term)? {
            None => Ok(MatOutcome::Hits(MaterializedBitmap::of(&[]))),
            Some((has_freqs, entry)) => term_entry_bitmap(seg, &entry, has_freqs, budget, cost),
        },
        Query::And { field, terms } | Query::Or { field, terms } => {
            let is_and = matches!(query, Query::And { .. });
            let Some((has_freqs, entries)) =
                roaring_exec::collect_bool_entries(seg, field, terms, is_and)?
            else {
                return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[])));
            };
            fold_term_entries(seg, &entries, has_freqs, is_and, budget, cost)
        }
        Query::Bool { clauses } => materialize_bool_bitmap(seg, clauses, budget, cost),
        Query::Terms { field, terms } => {
            let Some((has_freqs, collected)) = multi_term::collect_direct(seg, field, terms)?
            else {
                return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[])));
            };
            fold_term_entries(seg, &collected.entries, has_freqs, false, budget, cost)
        }
        Query::Prefix { field, prefix } => {
            let Some((has_freqs, collected)) = multi_term::collect_prefix(seg, field, prefix)?
            else {
                return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[])));
            };
            fold_term_entries(seg, &collected.entries, has_freqs, false, budget, cost)
        }
        Query::Wildcard { field, pattern } => {
            let pat = multi_term::WildcardPattern::parse(pattern);
            let Some((has_freqs, collected)) = multi_term::collect_wildcard(seg, field, &pat)?
            else {
                return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[])));
            };
            fold_term_entries(seg, &collected.entries, has_freqs, false, budget, cost)
        }
        Query::PointRange { field, low, high } => {
            // P1-3：区间包含值域且全段 doc 有值 → 命中集 = 全段，.kdm
            // 元数据判定，full(maxDoc) 免 BKD 物化（服务 Bool fold count：
            // rngmust/rngnot 形状由此脱离 maxDoc 级物化）。doc_count <
            // maxDoc 时哪些 doc 有值未知 → 落下方全量物化。
            if let Some(points) = seg.points_reader() {
                if let Some((min, max, doc_count)) = points.field_bounds(field) {
                    if *low <= min && max <= *high && doc_count == seg.max_doc() as u32 {
                        *cost += 1;
                        return Ok(MatOutcome::Hits(MaterializedBitmap::full(
                            seg.max_doc() as u32
                        )));
                    }
                }
            }
            *cost += seg.max_doc() as u64; // BKD 无法预估命中，保守计
            if *cost > budget {
                return Ok(MatOutcome::OverBudget);
            }
            let bm = point_range_bitmap(seg, field, *low, *high)?;
            Ok(MatOutcome::Hits(
                bm.unwrap_or_else(|| MaterializedBitmap::of(&[])),
            ))
        }
        Query::Phrase { field, terms } => {
            // 成本近似 = 各 term df 和（doc 合取扫描量）；缺 term → 空
            for t in terms {
                match seg.seek_term(field, t)? {
                    None => return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[]))),
                    Some((_, entry)) => {
                        *cost += seg.term_doc_freq(&entry) as u64;
                        if *cost > budget {
                            return Ok(MatOutcome::OverBudget);
                        }
                    }
                }
            }
            drive_materialize(seg, query)
        }
    }
}

/// 驱动查询的 segment_iterator 全量收集 docs 物化（Phrase 叶子用；
/// 必须调 matches()——T-A 协议）。
fn drive_materialize<L: LeafAccess>(seg: &mut L, query: &Query) -> io::Result<MatOutcome> {
    let mut docs = Vec::new();
    if let Some(mut it) = query.segment_iterator(seg, false)? {
        loop {
            let d = it.next_doc()?;
            if d == NO_MORE_DOCS {
                break;
            }
            if !it.matches()? {
                continue;
            }
            docs.push(d as u32);
        }
    }
    Ok(MatOutcome::Hits(MaterializedBitmap::of(&docs)))
}

/// term 集 fold：is_and → 交（零 cardinality 短路），否则 → 并。
fn fold_term_entries<L: LeafAccess>(
    seg: &L,
    entries: &[(u32, L::TermHandle)],
    has_freqs: bool,
    is_and: bool,
    budget: u64,
    cost: &mut u64,
) -> io::Result<MatOutcome> {
    let mut acc: Option<MaterializedBitmap> = None;
    for (_, entry) in entries {
        let child = match term_entry_bitmap(seg, entry, has_freqs, budget, cost)? {
            MatOutcome::OverBudget => return Ok(MatOutcome::OverBudget),
            MatOutcome::Hits(bm) => bm,
        };
        acc = Some(match (acc, is_and) {
            (None, _) => child,
            (Some(a), true) => a.and(&child),
            (Some(a), false) => a.or(&child),
        });
        if is_and && acc.as_ref().unwrap().cardinality() == 0 {
            return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[]))); // 交集已空，短路
        }
    }
    Ok(MatOutcome::Hits(
        acc.unwrap_or_else(|| MaterializedBitmap::of(&[])),
    ))
}

/// Bool 子句 fold（M7 §3.1）：正集三态与迭代语义逐条对应——MUST 交 /
/// 纯 SHOULD 并 / 纯 MUST_NOT 的 MatchAll；排除集先并后 andnot。
/// 任一子树 OverBudget → 全树 OverBudget。
pub(crate) fn materialize_bool_bitmap<L: LeafAccess>(
    seg: &mut L,
    clauses: &[(Occur, Query)],
    budget: u64,
    cost: &mut u64,
) -> io::Result<MatOutcome> {
    let mut musts: Vec<&Query> = Vec::new();
    let mut shoulds: Vec<&Query> = Vec::new();
    let mut nots: Vec<&Query> = Vec::new();
    for (occur, q) in clauses {
        match occur {
            Occur::Must => musts.push(q),
            Occur::Should => shoulds.push(q),
            Occur::MustNot => nots.push(q),
        }
    }
    fn fold_group<L: LeafAccess>(
        seg: &mut L,
        group: &[&Query],
        is_and: bool,
        budget: u64,
        cost: &mut u64,
    ) -> io::Result<MatOutcome> {
        let mut acc: Option<MaterializedBitmap> = None;
        for q in group {
            let child = match materialize_query_bitmap(seg, q, budget, cost)? {
                MatOutcome::OverBudget => return Ok(MatOutcome::OverBudget),
                MatOutcome::Hits(bm) => bm,
            };
            acc = Some(match (acc, is_and) {
                (None, _) => child,
                (Some(a), true) => a.and(&child),
                (Some(a), false) => a.or(&child),
            });
            if is_and && acc.as_ref().unwrap().cardinality() == 0 {
                return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[])));
            }
        }
        Ok(MatOutcome::Hits(
            acc.unwrap_or_else(|| MaterializedBitmap::of(&[])),
        ))
    }
    let mut positive = if !musts.is_empty() {
        match fold_group(seg, &musts, true, budget, cost)? {
            MatOutcome::OverBudget => return Ok(MatOutcome::OverBudget),
            MatOutcome::Hits(bm) => bm,
        }
    } else if !shoulds.is_empty() {
        match fold_group(seg, &shoulds, false, budget, cost)? {
            MatOutcome::OverBudget => return Ok(MatOutcome::OverBudget),
            MatOutcome::Hits(bm) => bm,
        }
    } else if !nots.is_empty() {
        MaterializedBitmap::full(seg.max_doc() as u32)
    } else {
        return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[])));
    };
    if !nots.is_empty() {
        let prohibited = match fold_group(seg, &nots, false, budget, cost)? {
            MatOutcome::OverBudget => return Ok(MatOutcome::OverBudget),
            MatOutcome::Hits(bm) => bm,
        };
        positive = positive.andnot(&prohibited);
    }
    Ok(MatOutcome::Hits(positive))
}

/// M7 §5.1 Bool 段级 count 快路径：Some = 快路径结果（拍平 roaring /
/// 纯 MUST_NOT / T-B fold）；None = 无快路径（调用方迭代）。
pub(crate) fn bool_segment_fast_count<L: LeafAccess>(
    seg: &mut L,
    clauses: &[(Occur, Query)],
) -> io::Result<Option<u64>> {
    // 拍平快路径（§2.4 同形状）：roaring count
    let refs: Vec<(Occur, &Query)> = clauses.iter().map(|(o, q)| (*o, q)).collect();
    if let Some((is_and, field, terms)) = flatten_bool(&refs) {
        if terms.len() >= 2 {
            let Some((has_freqs, entries)) =
                roaring_exec::collect_bool_entries(seg, field, &terms, is_and)?
            else {
                return Ok(Some(0)); // 未知字段 / AND 缺子句 / OR 全缺 → 段内空
            };
            if let Some(c) = roaring_exec::count(seg, &entries, has_freqs, is_and)? {
                return Ok(Some(c));
            }
        }
    }
    // 纯 MUST_NOT（§2.2/§2.5）：MatchAll 排除，count = maxDoc − prohibited。
    if !clauses.is_empty() && clauses.iter().all(|(o, _)| *o == Occur::MustNot) {
        let prohibited = prohibited_count(seg, clauses)?;
        return Ok(Some(seg.max_doc() as u64 - prohibited));
    }
    // T-B（M7 §3）：通用形状 bitmap fold；超预算 None 回落迭代。
    if !clauses.is_empty() {
        let budget = FOLD_COST_FACTOR * seg.max_doc() as u64;
        let mut cost = 0u64;
        match materialize_bool_bitmap(seg, clauses, budget, &mut cost)? {
            MatOutcome::Hits(bm) => return Ok(Some(bm.cardinality())),
            MatOutcome::OverBudget => {}
        }
    }
    Ok(None)
}

/// M7 §5.1 段级 count 快路径统一入口：Some(c) = 快路径结果；None = 无
/// 快路径（调用方迭代计数）。归并既有全部捷径（Term doc_freq 直读 /
/// PointRange bitmap cardinality / multi-term bitset popcount / And-Or
/// roaring count / Bool 快路径族）。Searcher::count 与 top_docs 共用。
pub(crate) fn fast_segment_count<L: LeafAccess>(seg: &mut L, query: &Query) -> io::Result<Option<u64>> {
    match query {
        Query::MatchAll => Ok(Some(seg.max_doc() as u64)),
        Query::Term { field, term } => Ok(Some(match seg.seek_term(field, term)? {
            Some((_, entry)) => seg.term_doc_freq(&entry) as u64,
            None => 0,
        })),
        Query::PointRange { field, low, high } => {
            // P1-3 count 捷径：区间包含字段值域 → 命中 = 全部有值 doc，
            // .kdm 元数据 O(1) 直读（Java PointWeight.count :86-105 的
            // min/max/getDocCount 同款）。low>high 时包含关系不可能成立，
            // 自然落下方 point_range_bitmap 保持 Err(InvalidInput) 语义。
            if let Some(points) = seg.points_reader() {
                if let Some((min, max, doc_count)) = points.field_bounds(field) {
                    if *low <= min && max <= *high {
                        return Ok(Some(doc_count as u64));
                    }
                }
            }
            let bm = point_range_bitmap(seg, field, *low, *high)?;
            Ok(Some(bm.map_or(0, |b| b.cardinality())))
        }
        q if q.is_multi_term() => q.bitset_count(seg),
        Query::And { field, terms } | Query::Or { field, terms } if terms.len() >= 2 => {
            let is_and = matches!(query, Query::And { .. });
            let Some((has_freqs, entries)) =
                roaring_exec::collect_bool_entries(seg, field, terms, is_and)?
            else {
                return Ok(Some(0));
            };
            roaring_exec::count(seg, &entries, has_freqs, is_and)
        }
        Query::Bool { clauses } => bool_segment_fast_count(seg, clauses),
        _ => Ok(None), // Phrase 等：无快路径
    }
}

/// 纯 MUST_NOT 的 prohibited 侧 count：子句换 SHOULD 视角取并集——拍平
/// OR 形命中 roaring count 快路径，否则驱动 DisjOver（单子句直接驱动）。
fn prohibited_count<L: LeafAccess>(seg: &mut L, clauses: &[(Occur, Query)]) -> io::Result<u64> {
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
            if !it.matches()? {
                continue;
            }
            n += 1;
        }
    }
    Ok(n)
}
