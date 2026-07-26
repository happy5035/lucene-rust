//! DocIdSetIterator semantics (docID starts at -1, ascends, ends at
//! NO_MORE_DOCS) with a Rust object shape (search spec §3: enum Query +
//! trait DocIter, no inheritance). M1 adds AND/OR Boolean iterators; M6
//! §2.3 adds the generic SegmentDocIter combinators (ConjOver/DisjOver/
//! Excluding) for nested Bool.

use std::io;

use codec_lucene9::field_infos::IndexOptions;
use codec_lucene9::postings_read::{DocsEnum, DocsFreqsEnum, PositionsEnum, NO_MORE_DOCS};
use codec_lucene9::roaring::{FrozenBitmap, MaterializedBitmap};
use codec_lucene9::terms_read::TermEntry;

use super::bitset::FixedBitSet;
use super::segment_reader::SegmentReader;

/// 块级迭代尺寸（spec 2026-07-26 §3）= PFOR PackedBlock 尺寸。
pub const DOC_BLOCK: usize = 128;

/// 调用方拥有的块填充缓冲（spec §3 偏差说明：填充式而非 §4b 原设计
/// 的借出式，避开跨 &mut self 生命周期纠缠）。freqs 仅产出 freq 的
/// 迭代器（DocsFreqsEnum 解码路径）填充；其余保持未定义，驱动方按
/// needs_freq 决定是否透传。
pub struct DocBlockBuf {
    pub docs: [u32; DOC_BLOCK],
    pub freqs: [u32; DOC_BLOCK],
    pub len: usize,
}

impl DocBlockBuf {
    pub fn new() -> DocBlockBuf {
        DocBlockBuf {
            docs: [0; DOC_BLOCK],
            freqs: [0; DOC_BLOCK],
            len: 0,
        }
    }
}

pub trait DocIter {
    fn doc_id(&self) -> i32;
    fn next_doc(&mut self) -> io::Result<i32>;
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc_id() >= target {
            return Ok(self.doc_id());
        }
        loop {
            let d = self.next_doc()?;
            if d >= target {
                return Ok(d);
            }
        }
    }
    fn freq(&self) -> u32 {
        1
    }
    /// 两阶段确认（M7 §2.1，Lucene TwoPhaseIterator.matches）：对
    /// next_doc/advance 返回的当前候选做昂贵验证；默认 Ok(true) = 单阶段
    /// 迭代器。返回 false 后调用方以 next_doc() 推进（候选已消费）；
    /// 对同一候选 doc 至多调用一次。
    fn matches(&mut self) -> io::Result<bool> {
        Ok(true)
    }
    /// 块级产出（spec 2026-07-26）：填充 out，返回产出数（0 = 耗尽，
    /// 此后永不再产块）。默认实现循环 next_doc()+matches() 填充——
    /// 两阶段确认在产出侧吸收，block driver 不再调 matches()。
    /// 覆写者契约：(1) out.docs[..n] 已 matches 过滤；(2) 块内严格升序；
    /// (3) 跨块单调递增；(4) 携带 freq 的迭代器同步填 out.freqs[..n]。
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut n = 0;
        while n < DOC_BLOCK {
            let d = self.next_doc()?;
            if d == NO_MORE_DOCS {
                break;
            }
            if !self.matches()? {
                continue;
            }
            out.docs[n] = d as u32;
            n += 1;
        }
        out.len = n;
        Ok(n)
    }
}

// ── MatchAll ──────────────────────────────────────────────────────────

pub struct MatchAllIter {
    doc: i32,
    max_doc: i32,
}
impl MatchAllIter {
    pub fn new(max_doc: i32) -> Self {
        MatchAllIter { doc: -1, max_doc }
    }
}
impl DocIter for MatchAllIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        self.doc += 1;
        if self.doc >= self.max_doc {
            self.doc = NO_MORE_DOCS;
        }
        Ok(self.doc)
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if target > self.doc {
            self.doc = if target >= self.max_doc {
                NO_MORE_DOCS
            } else {
                target
            };
        }
        Ok(self.doc)
    }
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        if self.doc == NO_MORE_DOCS {
            out.len = 0;
            return Ok(0);
        }
        let start = (self.doc + 1).max(0) as u32;
        let n = (self.max_doc as u32).saturating_sub(start).min(DOC_BLOCK as u32) as usize;
        for i in 0..n {
            out.docs[i] = start + i as u32;
        }
        self.doc = if n == 0 || start + n as u32 >= self.max_doc as u32 {
            NO_MORE_DOCS
        } else {
            (start + n as u32 - 1) as i32
        };
        out.len = n;
        Ok(n)
    }
}

// ── Internal postings wrapper ─────────────────────────────────────────

enum PostingsIter {
    Docs(DocsEnum),
    Freqs(DocsFreqsEnum),
}
impl PostingsIter {
    /// `needs_freq == false` over a DOCS_AND_FREQS field yields a no-freq
    /// enum: freq blocks are skipped byte-wise and `freq()` panics — only
    /// count-only consumers (which never call freq) may take that path.
    fn new(
        seg: &SegmentReader,
        entry: &TermEntry,
        has_freqs: bool,
        needs_freq: bool,
    ) -> io::Result<Self> {
        if has_freqs {
            Ok(PostingsIter::Freqs(seg.docs_freqs_enum(entry, needs_freq)?))
        } else {
            Ok(PostingsIter::Docs(seg.docs_enum(entry)?))
        }
    }
    fn doc_id(&self) -> i32 {
        match self {
            Self::Docs(d) => d.doc_id(),
            Self::Freqs(f) => f.doc_id(),
        }
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        match self {
            Self::Docs(d) => d.next_doc(),
            Self::Freqs(f) => f.next_doc(),
        }
    }
    fn advance(&mut self, t: i32) -> io::Result<i32> {
        match self {
            Self::Docs(d) => d.advance(t),
            Self::Freqs(f) => f.advance(t),
        }
    }
    fn freq(&self) -> u32 {
        match self {
            Self::Freqs(f) => f.freq(),
            _ => 1,
        }
    }
}

// ── Conjunction (AND) ─────────────────────────────────────────────────

pub struct ConjunctionDocIter {
    sub: Vec<PostingsIter>,
    doc: i32,
    lead: usize,
}

impl ConjunctionDocIter {
    pub fn new(
        seg: &SegmentReader,
        field: &str,
        sorted_entries: &[(u32, TermEntry)],
        needs_freq: bool,
    ) -> io::Result<Self> {
        let fi = seg.field_info(field);
        let has_freqs = fi
            .map(|f| f.index_options != IndexOptions::Docs)
            .unwrap_or(false);
        let mut sub = Vec::with_capacity(sorted_entries.len());
        for (_, entry) in sorted_entries {
            sub.push(PostingsIter::new(seg, entry, has_freqs, needs_freq)?);
        }
        for s in &mut sub {
            if s.next_doc()? == NO_MORE_DOCS {
                return Ok(ConjunctionDocIter {
                    sub,
                    doc: NO_MORE_DOCS,
                    lead: 0,
                });
            }
        }
        Ok(ConjunctionDocIter {
            sub,
            doc: -1,
            lead: 0,
        })
    }
    fn advance_all_past(&mut self, doc: i32) -> io::Result<bool> {
        for s in &mut self.sub {
            if s.doc_id() == doc && s.next_doc()? == NO_MORE_DOCS {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl DocIter for ConjunctionDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        if self.doc >= 0 && !self.advance_all_past(self.doc)? {
            self.doc = NO_MORE_DOCS;
            return Ok(NO_MORE_DOCS);
        }
        loop {
            let candidate = self.sub[self.lead].doc_id();
            if candidate == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
            let target = candidate;
            let mut matched = true;
            for i in 0..self.sub.len() {
                if i == self.lead {
                    continue;
                }
                let d = self.sub[i].advance(target)?;
                if d == NO_MORE_DOCS {
                    self.doc = NO_MORE_DOCS;
                    return Ok(NO_MORE_DOCS);
                }
                if d > target {
                    self.lead = i;
                    matched = false;
                    break;
                }
            }
            if matched {
                self.doc = target;
                return Ok(target);
            }
        }
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target {
            return Ok(self.doc);
        }
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        self.sub[self.lead].advance(target)?;
        self.doc = -1;
        self.next_doc()
    }
    // ConstantScore: no consumer calls freq() on a conjunction and AND-freq is
    // undefined anyway; the sum of sub freqs is a placeholder.
    fn freq(&self) -> u32 {
        self.sub.iter().map(|s| s.freq()).sum()
    }
}

// ── Disjunction (OR) ──────────────────────────────────────────────────

pub struct DisjunctionDocIter {
    sub: Vec<PostingsIter>,
    doc: i32,
}

impl DisjunctionDocIter {
    pub fn new(
        seg: &SegmentReader,
        field: &str,
        sorted_entries: &[(u32, TermEntry)],
        needs_freq: bool,
    ) -> io::Result<Self> {
        let fi = seg.field_info(field);
        let has_freqs = fi
            .map(|f| f.index_options != IndexOptions::Docs)
            .unwrap_or(false);
        let mut sub = Vec::with_capacity(sorted_entries.len());
        for (_, entry) in sorted_entries {
            sub.push(PostingsIter::new(seg, entry, has_freqs, needs_freq)?);
        }
        for s in &mut sub {
            s.next_doc()?;
        }
        Ok(DisjunctionDocIter { sub, doc: -1 })
    }
}

impl DocIter for DisjunctionDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        if self.doc >= 0 {
            for s in &mut self.sub {
                if s.doc_id() == self.doc {
                    s.next_doc()?;
                }
            }
        }
        let mut best = NO_MORE_DOCS;
        for s in &self.sub {
            let d = s.doc_id();
            if d != NO_MORE_DOCS && d < best {
                best = d;
            }
        }
        self.doc = best;
        Ok(best)
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target {
            return Ok(self.doc);
        }
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        for s in &mut self.sub {
            if s.doc_id() < target {
                s.advance(target)?;
            }
        }
        let mut best = NO_MORE_DOCS;
        for s in &self.sub {
            let d = s.doc_id();
            if d != NO_MORE_DOCS && d < best {
                best = d;
            }
        }
        self.doc = best;
        Ok(best)
    }
    // ConstantScore: no consumer calls freq() on a disjunction and OR-freq is
    // undefined anyway; the first matching sub's freq is a placeholder.
    fn freq(&self) -> u32 {
        for s in &self.sub {
            if s.doc_id() == self.doc {
                return s.freq();
            }
        }
        1
    }
}

// ── Bitset (multi-term materialization) ───────────────────────────────

/// DocIter over a materialized FixedBitSet (spec §4 bitset path): next_doc /
/// advance are next_set_bit scans. freq() is 1 (doc-set semantics).
pub struct BitsetDocIter {
    bits: FixedBitSet,
    doc: i32,
}

impl BitsetDocIter {
    pub fn new(bits: FixedBitSet) -> Self {
        BitsetDocIter { bits, doc: -1 }
    }
}

impl DocIter for BitsetDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }

    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        self.doc = match self.bits.next_set_bit((self.doc + 1) as usize) {
            Some(d) => d as i32,
            None => NO_MORE_DOCS,
        };
        Ok(self.doc)
    }

    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if target > self.doc {
            self.doc = match self.bits.next_set_bit(target.max(0) as usize) {
                Some(d) => d as i32,
                None => NO_MORE_DOCS,
            };
        }
        Ok(self.doc)
    }
}

// ── Phrase (slop=0) ─────────────────────────────────────────────────────

/// phrase approximation 源（M7 §2.2）：全 term 有内联 bitmap 时 roaring
/// AND 物化候选序列（µs 级，比 PFOR 合取快）；否则 postings 合取舞蹈。
enum PhraseApprox {
    Postings,
    Bitmap { docs: Vec<u32>, cursor: usize },
}

/// One phrase term occurrence: an independent EverythingEnum + its offset
/// in the phrase. Repeated terms get independent enums, which makes them
/// naturally correct (spec M2 §6; PhrasePositions :24-58).
struct Occurrence {
    en: PositionsEnum,
    offset: u32,
}

/// Exact-phrase iterator (slop=0): conjunction over the occurrences'
/// postings enums, then per-doc position verification — there must be a
/// lead position p0 with p0 - offset[0] + offset[i] present in every
/// occurrence's positions (ExactPhraseMatcher :138-167).
pub struct PhraseDocIter {
    occ: Vec<Occurrence>, // df-ascending (conjunction cost order)
    approx: PhraseApprox,
    doc: i32,
    lead: usize,
}

impl PhraseDocIter {
    /// Builds the iterator: `Ok(None)` for unknown/non-indexed field or an
    /// absent term (no hits, TermQuery semantics); `Err` when the field has
    /// no positions — fail-fast, mirroring Java's execution-time error of
    /// PhraseQuery on such fields (Lucene912PostingsReader.postings :280-309
    /// downgrades to a docs-only enum whose nextPosition throws).
    pub fn new(
        seg: &mut SegmentReader,
        field: &str,
        terms: &[Vec<u8>],
    ) -> io::Result<Option<PhraseDocIter>> {
        let Some(fi) = seg.field_info(field) else {
            return Ok(None);
        };
        if fi.index_options == IndexOptions::None {
            return Ok(None);
        }
        if !matches!(
            fi.index_options,
            IndexOptions::DocsAndFreqsAndPositions
                | IndexOptions::DocsAndFreqsAndPositionsAndOffsets
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "field '{field}' does not have positions (phrase query requires \
                     IndexOptions >= DOCS_AND_FREQS_AND_POSITIONS)"
                ),
            ));
        }
        let mut sought: Vec<(u32, u32, TermEntry)> = Vec::with_capacity(terms.len());
        for (i, t) in terms.iter().enumerate() {
            let Some((_, entry)) = seg.seek_term(field, t)? else {
                return Ok(None); // absent term: no hits (PhraseWeight null scorer)
            };
            sought.push((entry.doc_freq, i as u32, entry));
        }
        sought.sort_by_key(|(df, _, _)| *df);
        // M7 §2.2 bitmap 候选快路径：全部 term 有内联 bitmap → roaring
        // AND 物化候选序列；任一缺失 → postings 合取 approximation。
        let mut views: Vec<FrozenBitmap> = Vec::with_capacity(sought.len());
        let mut all_bitmap = true;
        for (_, _, entry) in &sought {
            match seg.open_term_bitmap(entry)? {
                Some(v) => views.push(v),
                None => {
                    all_bitmap = false;
                    break;
                }
            }
        }
        let approx = if all_bitmap {
            let refs: Vec<&FrozenBitmap> = views.iter().collect();
            PhraseApprox::Bitmap {
                docs: codec_lucene9::roaring::intersect_docs(&refs),
                cursor: 0,
            }
        } else {
            PhraseApprox::Postings
        };
        let mut occ: Vec<Occurrence> = Vec::with_capacity(sought.len());
        for (_, offset, entry) in sought {
            occ.push(Occurrence {
                en: seg.positions_enum(&entry)?,
                offset,
            });
        }
        Ok(Some(PhraseDocIter {
            occ,
            approx,
            doc: -1,
            lead: 0,
        }))
    }

    /// ExactPhraseMatcher (:138-167): collects each occurrence's positions
    /// in the current doc (freq × nextPosition, PhrasePositions.firstPosition
    /// :42-45) and checks for a common phrasePos = pos - offset.
    fn positions_match(&mut self) -> io::Result<bool> {
        let mut lists: Vec<Vec<u32>> = Vec::with_capacity(self.occ.len());
        for o in &mut self.occ {
            let f = o.en.freq() as usize;
            let mut v = Vec::with_capacity(f);
            for _ in 0..f {
                v.push(o.en.next_position()?);
            }
            lists.push(v);
        }
        let base_off = self.occ[0].offset as i64;
        'outer: for &p0 in &lists[0] {
            let phrase_pos = p0 as i64 - base_off; // :145
            for (i, l) in lists.iter().enumerate().skip(1) {
                let expected = phrase_pos + self.occ[i].offset as i64; // :148
                if expected < 0 || l.binary_search(&(expected as u32)).is_err() {
                    continue 'outer;
                }
            }
            return Ok(true);
        }
        Ok(false)
    }
}

impl PhraseDocIter {
    /// approximation 推进（M7 §2.2）：只做 postings 合取对齐
    /// （ConjunctionDISI 舞蹈），**不解码位置**——候选直接返回，位置
    /// 验证推迟到 matches()。
    fn next_candidate(&mut self) -> io::Result<i32> {
        debug_assert!(matches!(self.approx, PhraseApprox::Postings));
        if self.doc >= 0 {
            for o in &mut self.occ {
                if o.en.doc_id() == self.doc && o.en.next_doc()? == NO_MORE_DOCS {
                    self.doc = NO_MORE_DOCS;
                    return Ok(NO_MORE_DOCS);
                }
            }
        }
        loop {
            let candidate = self.occ[self.lead].en.doc_id();
            if candidate == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
            if candidate < 0 {
                // lead enum 尚未定位（初始状态）：推进一次，让合取舞蹈
                // 在真实 doc ID 上工作。旧实现在此会 positions_match() 失败
                // 并自动把全部 occurrence 移过 -1；拆分后由驱动方重试，
                // 因此这里直接定位到首个真实 doc。
                let d = self.occ[self.lead].en.next_doc()?;
                if d == NO_MORE_DOCS {
                    self.doc = NO_MORE_DOCS;
                    return Ok(NO_MORE_DOCS);
                }
                continue;
            }
            let mut matched = true;
            for i in 0..self.occ.len() {
                if i == self.lead {
                    continue;
                }
                let d = self.occ[i].en.advance(candidate)?;
                if d == NO_MORE_DOCS {
                    self.doc = NO_MORE_DOCS;
                    return Ok(NO_MORE_DOCS);
                }
                if d > candidate {
                    self.lead = i;
                    matched = false;
                    break;
                }
            }
            if matched {
                self.doc = candidate;
                return Ok(candidate);
            }
        }
    }
}

impl DocIter for PhraseDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }

    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        match &mut self.approx {
            PhraseApprox::Postings => self.next_candidate(),
            PhraseApprox::Bitmap { docs, cursor } => {
                if self.doc >= 0 {
                    *cursor += 1;
                }
                match docs.get(*cursor) {
                    Some(&d) => {
                        self.doc = d as i32;
                        Ok(self.doc)
                    }
                    None => {
                        self.doc = NO_MORE_DOCS;
                        Ok(NO_MORE_DOCS)
                    }
                }
            }
        }
    }

    /// 两阶段确认（M7 §2.2）：对当前候选解码位置并验证
    /// （ExactPhraseMatcher :138-167，逻辑从旧 next_doc 原样搬入）。
    fn matches(&mut self) -> io::Result<bool> {
        debug_assert!(self.doc >= 0 && self.doc != NO_MORE_DOCS);
        for o in &mut self.occ {
            if o.en.doc_id() != self.doc {
                // bitmap approximation：positions enum 尚未定位——advance
                // 对齐（跳表，非逐 doc）；近似集 ⊆ 各 term doc 集，必命中。
                let d = o.en.advance(self.doc)?;
                if d != self.doc {
                    return Ok(false); // 防御（不变量破坏时宁可漏不可错）
                }
            }
        }
        self.positions_match()
    }
    // advance: trait default（线性 next_candidate 循环，approximation
    // 语义）；freq: 1（ConstantScore，trait default）。
}

// ── Roaring (inline term bitmap, M5 §2 croaring frozen view) ─────────

/// Bitmap doc sources for `BitmapCursor` (M5 §2 FrozenBitmap zero-copy
/// view; M6 §3.3 MaterializedBitmap owned hits materialization). The only
/// surface the cursor needs.
pub trait DocsBitmap {
    /// Fills `dst` with the first docs >= `from`, returns the count read
    /// (0 = exhausted) — croaring `reset_at_or_after` + `next_many`.
    fn docs_from(&self, from: u32, dst: &mut [u32]) -> usize;
}

impl DocsBitmap for FrozenBitmap {
    fn docs_from(&self, from: u32, dst: &mut [u32]) -> usize {
        FrozenBitmap::docs_from(self, from, dst)
    }
}

impl DocsBitmap for MaterializedBitmap {
    fn docs_from(&self, from: u32, dst: &mut [u32]) -> usize {
        MaterializedBitmap::docs_from(self, from, dst)
    }
}

/// Batch-refill cursor over a bitmap source (M5 T2, 关键设计事实 5):
/// the ~60ns frozen-view create / owned-iter create is amortized over a
/// 512-doc batch; each refill is a `reset_at_or_after` seek + bulk
/// `next_many`. docs are < max_doc <= i32::MAX, so `d + 1` never
/// overflows u32.
pub struct BitmapCursor<B: DocsBitmap> {
    bitmap: B,
    buf: Vec<u32>,
    pos: usize,
    end: usize,
    next_from: u32,
    exhausted: bool,
}

const BITMAP_ITER_BATCH: usize = 512;

impl<B: DocsBitmap> BitmapCursor<B> {
    fn new(bitmap: B) -> BitmapCursor<B> {
        BitmapCursor {
            bitmap,
            buf: vec![0; BITMAP_ITER_BATCH],
            pos: 0,
            end: 0,
            next_from: 0,
            exhausted: false,
        }
    }

    fn refill(&mut self) -> bool {
        if self.exhausted {
            return false;
        }
        self.end = self.bitmap.docs_from(self.next_from, &mut self.buf);
        self.pos = 0;
        if self.end == 0 {
            self.exhausted = true;
            return false;
        }
        true
    }

    fn next(&mut self) -> Option<u32> {
        if self.pos >= self.end && !self.refill() {
            return None;
        }
        let d = self.buf[self.pos];
        self.pos += 1;
        self.next_from = d + 1;
        Some(d)
    }

    /// First doc >= target. Forward-only: discards the buffered tail and
    /// re-seeks (the merge dance's resync pattern, M4 关键设计事实 8).
    fn advance(&mut self, target: u32) -> Option<u32> {
        self.pos = self.end;
        self.next_from = target;
        self.next()
    }

    /// 批量产出（spec 2026-07-26 Task 3）：把缓冲与后续 refill 的 doc
    /// 拷入 dst 至填满或耗尽，返回产出数。维护 next_from 不变量
    /// （= 最后产出 doc + 1），保证批读后 advance() 语义不变。
    /// docs < max_doc <= i32::MAX，+1 不溢出。
    fn next_many_to(&mut self, dst: &mut [u32]) -> usize {
        let mut n = 0;
        while n < dst.len() {
            if self.pos >= self.end {
                // 维护 next_from 不变量（= 已产出最后 doc + 1），
                // 使 refill 的 docs_from seek 越过已消费区。
                // brief 原版仅在循环尾更新，跨 refill 时 stale → 重复拉取。
                if n > 0 {
                    self.next_from = dst[n - 1] + 1;
                }
                if !self.refill() {
                    break;
                }
            }
            let take = (self.end - self.pos).min(dst.len() - n);
            dst[n..n + take].copy_from_slice(&self.buf[self.pos..self.pos + take]);
            self.pos += take;
            n += take;
        }
        if n > 0 {
            self.next_from = dst[n - 1] + 1;
        }
        n
    }
}

/// BitmapCursor 叶子共用的块产出（RoaringDocIter / MaterializedDocIter）：
/// 游标批读 + doc 游标簿记。耗尽后 doc 钉 NO_MORE_DOCS。
fn cursor_next_block<B: DocsBitmap>(
    cur: &mut BitmapCursor<B>,
    doc: &mut i32,
    out: &mut DocBlockBuf,
) -> io::Result<usize> {
    if *doc == NO_MORE_DOCS {
        out.len = 0;
        return Ok(0);
    }
    let n = cur.next_many_to(&mut out.docs);
    *doc = if n == 0 { NO_MORE_DOCS } else { out.docs[n - 1] as i32 };
    out.len = n;
    Ok(n)
}

/// DocIter over a term's inline bitmap (M5 §2 Term 路径): wraps the
/// frozen view's batch cursor. freq() is 1 — the bitmap carries no freqs,
/// and needs_freq paths never get this iterator (correctness
/// requirement (e)).
pub struct RoaringDocIter {
    cur: BitmapCursor<FrozenBitmap>,
    doc: i32,
}

impl RoaringDocIter {
    pub fn new(bitmap: FrozenBitmap) -> RoaringDocIter {
        RoaringDocIter {
            cur: BitmapCursor::new(bitmap),
            doc: -1,
        }
    }
}

impl DocIter for RoaringDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }

    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        self.doc = match self.cur.next() {
            Some(d) => d as i32,
            None => NO_MORE_DOCS,
        };
        Ok(self.doc)
    }

    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if target > self.doc {
            self.doc = match self.cur.advance(target.max(0) as u32) {
                Some(d) => d as i32,
                None => NO_MORE_DOCS,
            };
        }
        Ok(self.doc)
    }

    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        cursor_next_block(&mut self.cur, &mut self.doc, out)
    }
}

// ── Materialized (owned bitmap over a materialized hit set) ──────────

/// DocIter over an owned materialized bitmap: PointRange 段内命中
/// (M6 spec §3.3) 与 Bool 物化 fold 结果（P1-1：排除形容器级 andnot
/// 后顺序迭代，替代 ExcludingDocIter 逐候选 advance 丢批病理）共用。
/// 同 RoaringDocIter 的批量游标形状。freq() is 1 — 物化集不携带 freq，
/// `needs_freq` 路径不会路由到此。
pub struct MaterializedDocIter {
    cur: BitmapCursor<MaterializedBitmap>,
    doc: i32,
}

impl MaterializedDocIter {
    pub fn new(bitmap: MaterializedBitmap) -> MaterializedDocIter {
        MaterializedDocIter {
            cur: BitmapCursor::new(bitmap),
            doc: -1,
        }
    }
}

impl DocIter for MaterializedDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }

    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        self.doc = match self.cur.next() {
            Some(d) => d as i32,
            None => NO_MORE_DOCS,
        };
        Ok(self.doc)
    }

    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if target > self.doc {
            self.doc = match self.cur.advance(target.max(0) as u32) {
                Some(d) => d as i32,
                None => NO_MORE_DOCS,
            };
        }
        Ok(self.doc)
    }

    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        cursor_next_block(&mut self.cur, &mut self.doc, out)
    }
}

// ── AND over views (M4 §5) ─────────────────────────────────────────────

/// One merge-intersect / merge-union source (M5 §2): a frozen-view batch
/// cursor, or a materialized doc slice (low-df clause or tier-1 fold
/// result). Both yield ascending docs with a forward-only advance.
pub enum DocSource {
    Bitmap {
        cur: BitmapCursor<FrozenBitmap>,
        doc: Option<u32>,
    },
    Slice {
        docs: Vec<u32>,
        pos: usize,
    },
}

impl DocSource {
    /// Frozen-view source, primed to its first doc.
    pub fn bitmap(bitmap: FrozenBitmap) -> DocSource {
        let mut cur = BitmapCursor::new(bitmap);
        let doc = cur.next();
        DocSource::Bitmap { cur, doc }
    }

    /// Materialized ascending-doc source: a low-df clause (spec §5 档 2:
    /// df<4096 → ≤4095 docs, ascending by enum construction) or a tier-1
    /// fold result (M5 §2 物化 and/or fold, ascending by croaring
    /// iteration).
    pub fn slice(docs: Vec<u32>) -> DocSource {
        DocSource::Slice { docs, pos: 0 }
    }

    fn current(&self) -> Option<u32> {
        match self {
            DocSource::Bitmap { doc, .. } => *doc,
            DocSource::Slice { docs, pos } => docs.get(*pos).copied(),
        }
    }

    fn next(&mut self) -> Option<u32> {
        match self {
            DocSource::Bitmap { cur, doc } => {
                *doc = cur.next();
                *doc
            }
            DocSource::Slice { docs, pos } => {
                if *pos < docs.len() {
                    *pos += 1;
                }
                docs.get(*pos).copied()
            }
        }
    }

    /// First doc >= target. Forward-only; idempotent when the current doc
    /// is already at/past target (the merge dance re-syncs on agreement).
    fn advance(&mut self, target: u32) -> Option<u32> {
        match self {
            DocSource::Bitmap { cur, doc } => {
                if doc.is_some_and(|d| d >= target) {
                    return *doc;
                }
                *doc = cur.advance(target);
                *doc
            }
            DocSource::Slice { docs, pos } => {
                *pos += docs[*pos..].partition_point(|&d| d < target);
                docs.get(*pos).copied()
            }
        }
    }
}

/// AND execution (M5 §2): merge-intersect over `sources`; every agreed
/// candidate is point-probed against each `probes` view (contains).
/// Tier shapes: 档 1 偏斜 → sources=[最小侧 bitmap], probes=其余；档 1
/// 非偏斜 → sources=[物化 `and` fold 结果 slice], probes=[]（croaring
/// materialized fold, 关键设计事实 8）；档 2 → sources=物化 low-df
/// slices, probes=bitmap 子句. freq() is 1 (ConstantScore, trait
/// default).
pub struct RoaringAndDocIter {
    sources: Vec<DocSource>,
    probes: Vec<FrozenBitmap>,
    doc: i32,
}

impl RoaringAndDocIter {
    pub fn new(sources: Vec<DocSource>, probes: Vec<FrozenBitmap>) -> RoaringAndDocIter {
        debug_assert!(!sources.is_empty());
        RoaringAndDocIter {
            sources,
            probes,
            doc: -1,
        }
    }
}

impl DocIter for RoaringAndDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }

    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        if self.doc >= 0 {
            // move every source sitting on the last emitted doc past it
            let last = self.doc as u32;
            for s in &mut self.sources {
                if s.current() == Some(last) && s.next().is_none() {
                    self.doc = NO_MORE_DOCS;
                    return Ok(NO_MORE_DOCS);
                }
            }
        }
        'outer: loop {
            // conjunction: agree on the max current doc
            let mut target = 0u32;
            for s in &self.sources {
                let Some(d) = s.current() else {
                    self.doc = NO_MORE_DOCS;
                    return Ok(NO_MORE_DOCS);
                };
                target = target.max(d);
            }
            let mut agreed = true;
            for s in &mut self.sources {
                if s.current() < Some(target) {
                    match s.advance(target) {
                        Some(d) if d == target => {}
                        Some(_) => agreed = false, // overshot: new candidate, re-sync
                        None => {
                            self.doc = NO_MORE_DOCS;
                            return Ok(NO_MORE_DOCS);
                        }
                    }
                }
            }
            if !agreed {
                continue 'outer;
            }
            // point-probe the agreed candidate against every bitmap clause
            // (croaring contains: direct memory probe, 8.5–28.7 ns)
            let mut pass = true;
            for p in &self.probes {
                if !p.contains(target) {
                    pass = false;
                    break;
                }
            }
            if pass {
                self.doc = target as i32;
                return Ok(self.doc);
            }
            // rejected: move the first (cheapest) source past the candidate
            if self.sources[0].next().is_none() {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
        }
    }

    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target || self.doc == NO_MORE_DOCS {
            return Ok(self.doc);
        }
        for s in &mut self.sources {
            s.advance(target.max(0) as u32);
        }
        self.doc = -1;
        self.next_doc()
    }
}

/// OR execution (M5 §2): k-way merge-union over `sources` with
/// min-current dedup. 全 bitmap → sources=[物化 `or` fold 结果 slice]
/// （croaring materialized fold, 关键设计事实 8）；混合 → bitmap batch
/// cursors + 物化 low-df slices. freq() is 1 (ConstantScore, trait
/// default).
pub struct RoaringOrDocIter {
    sources: Vec<DocSource>,
    doc: i32,
}

impl RoaringOrDocIter {
    pub fn new(sources: Vec<DocSource>) -> RoaringOrDocIter {
        debug_assert!(!sources.is_empty());
        RoaringOrDocIter { sources, doc: -1 }
    }
}

impl DocIter for RoaringOrDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }

    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        if self.doc >= 0 {
            // union dedup: move every source sitting on the last emitted doc
            let last = self.doc as u32;
            for s in &mut self.sources {
                if s.current() == Some(last) {
                    s.next();
                }
            }
        }
        let mut best: Option<u32> = None;
        for s in &self.sources {
            if let Some(d) = s.current() {
                if best.is_none_or(|b| d < b) {
                    best = Some(d);
                }
            }
        }
        self.doc = match best {
            Some(d) => d as i32,
            None => NO_MORE_DOCS,
        };
        Ok(self.doc)
    }

    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target || self.doc == NO_MORE_DOCS {
            return Ok(self.doc);
        }
        for s in &mut self.sources {
            s.advance(target.max(0) as u32);
        }
        self.doc = -1;
        self.next_doc()
    }
}

// ── Generic Boolean combinators over SegmentDocIter (M6 §2.3) ─────────

/// Conjunction over arbitrary per-segment iterators (spec M6 §2.3):
/// the ConjunctionDocIter alignment dance (Lucene ConjunctionDISI
/// protocol) lifted from postings-only PostingsIter to SegmentDocIter
/// children. Children live in a heap Vec — SegmentDocIter is 18.7KB,
/// so no inline child array ever lands in a stack frame.
pub struct ConjOverDocIter {
    sub: Vec<SegmentDocIter>,
    doc: i32,
    lead: usize,
}

impl ConjOverDocIter {
    /// Primes every child (same contract as ConjunctionDocIter::new); any
    /// exhausted child empties the whole conjunction.
    pub fn new(sub: Vec<SegmentDocIter>) -> io::Result<ConjOverDocIter> {
        debug_assert!(sub.len() >= 2);
        let mut it = ConjOverDocIter {
            sub,
            doc: -1,
            lead: 0,
        };
        for s in &mut it.sub {
            if s.next_doc()? == NO_MORE_DOCS {
                it.doc = NO_MORE_DOCS;
                return Ok(it);
            }
        }
        Ok(it)
    }
}

impl DocIter for ConjOverDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        if self.doc >= 0 {
            // move every child sitting on the last emitted doc past it
            for i in 0..self.sub.len() {
                if self.sub[i].doc_id() == self.doc && self.sub[i].next_doc()? == NO_MORE_DOCS {
                    self.doc = NO_MORE_DOCS;
                    return Ok(NO_MORE_DOCS);
                }
            }
        }
        loop {
            let candidate = self.sub[self.lead].doc_id();
            if candidate == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
            let mut matched = true;
            for i in 0..self.sub.len() {
                if i == self.lead {
                    continue;
                }
                let d = self.sub[i].advance(candidate)?;
                if d == NO_MORE_DOCS {
                    self.doc = NO_MORE_DOCS;
                    return Ok(NO_MORE_DOCS);
                }
                if d > candidate {
                    self.lead = i;
                    matched = false;
                    break;
                }
            }
            if matched {
                // M7 §2.3：approximation 对齐后逐个 confirmation（Lucene
                // ConjunctionScorer 同款）；任一 false → 推进停在
                // candidate 的子句（含已确认的）后重新对齐。
                let mut all_match = true;
                for s in &mut self.sub {
                    if !s.matches()? {
                        all_match = false;
                        break;
                    }
                }
                if all_match {
                    self.doc = candidate;
                    return Ok(candidate);
                }
                for s in &mut self.sub {
                    if s.doc_id() == candidate && s.next_doc()? == NO_MORE_DOCS {
                        self.doc = NO_MORE_DOCS;
                        return Ok(NO_MORE_DOCS);
                    }
                }
            }
        }
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target {
            return Ok(self.doc);
        }
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        self.sub[self.lead].advance(target)?;
        self.doc = -1;
        self.next_doc()
    }
    // freq: 1（ConstantScore，spec §2.3 Bool 路径恒 needs_freq=false）
}

/// T-C bench 原型（M7 §4）：DisjOver 的索引堆版本——堆内只放 sub 下标，
/// 18.7KB 的 SegmentDocIter 不挪动。堆序 = sub[i].doc_id() 小顶。
/// 子句恒单阶段故不吸收 matches()；仅用于被忽略的 micro bench，
/// 用 `#[cfg(test)]` 门控避免 release 构建的 dead-code 警告。
#[cfg(test)]
pub(crate) struct DisjOverHeapDocIter {
    sub: Vec<SegmentDocIter>,
    heap: Vec<usize>,
    doc: i32,
}

#[cfg(test)]
impl DisjOverHeapDocIter {
    pub(crate) fn new(sub: Vec<SegmentDocIter>) -> io::Result<DisjOverHeapDocIter> {
        debug_assert!(sub.len() >= 2);
        let n = sub.len();
        let mut it = DisjOverHeapDocIter {
            sub,
            heap: (0..n).collect(),
            doc: -1,
        };
        for s in &mut it.sub {
            s.next_doc()?;
        }
        for i in (0..it.heap.len() / 2).rev() {
            it.sift_down(i); // heapify
        }
        Ok(it)
    }
    fn less(&self, a: usize, b: usize) -> bool {
        self.sub[self.heap[a]].doc_id() < self.sub[self.heap[b]].doc_id()
    }
    fn sift_down(&mut self, mut i: usize) {
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < self.heap.len() && self.less(l, m) {
                m = l;
            }
            if r < self.heap.len() && self.less(r, m) {
                m = r;
            }
            if m == i {
                break;
            }
            self.heap.swap(i, m);
            i = m;
        }
    }
}

#[cfg(test)]
impl DocIter for DisjOverHeapDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        loop {
            let top = self.heap[0];
            let d = self.sub[top].doc_id();
            if d == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
            if self.doc < d {
                self.doc = d;
                return Ok(d);
            }
            // 堆顶停在已消费的 doc → 推进并下滤（NO_MORE_DOCS 自然沉底）
            self.sub[top].next_doc()?;
            self.sift_down(0);
        }
    }
}

/// Linear-scan baseline for the M7 T-C micro bench: same semantics as
/// the heap `DisjOverDocIter` below, but finds the minimum doc ID by
/// scanning all sub-iterators every step.仅用于被忽略的 micro bench，
/// 用 `#[cfg(test)]` 门控避免 release 构建的 dead-code 警告。
#[cfg(test)]
pub(crate) struct DisjOverLinearDocIter {
    sub: Vec<SegmentDocIter>,
    doc: i32,
}

#[cfg(test)]
impl DisjOverLinearDocIter {
    pub(crate) fn new(sub: Vec<SegmentDocIter>) -> io::Result<DisjOverLinearDocIter> {
        debug_assert!(sub.len() >= 2);
        let mut it = DisjOverLinearDocIter { sub, doc: -1 };
        for s in &mut it.sub {
            s.next_doc()?;
        }
        Ok(it)
    }
}

#[cfg(test)]
impl DocIter for DisjOverLinearDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        if self.doc >= 0 {
            for s in &mut self.sub {
                if s.doc_id() == self.doc {
                    s.next_doc()?;
                }
            }
        }
        loop {
            let mut best = NO_MORE_DOCS;
            for s in &self.sub {
                let d = s.doc_id();
                if d != NO_MORE_DOCS && d < best {
                    best = d;
                }
            }
            if best == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
            // M7 §2.3：对停在 best 的子句逐个 confirmation；至少一个
            // true → 命中（短路，省掉其余子句的确认成本）。
            let mut any = false;
            for s in &mut self.sub {
                if s.doc_id() == best && s.matches()? {
                    any = true;
                    break;
                }
            }
            if any {
                self.doc = best;
                return Ok(best);
            }
            for s in &mut self.sub {
                if s.doc_id() == best {
                    s.next_doc()?;
                }
            }
        }
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target {
            return Ok(self.doc);
        }
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        for s in &mut self.sub {
            if s.doc_id() < target {
                s.advance(target)?;
            }
        }
        let mut best = NO_MORE_DOCS;
        for s in &self.sub {
            let d = s.doc_id();
            if d != NO_MORE_DOCS && d < best {
                best = d;
            }
        }
        self.doc = best;
        Ok(best)
    }
}

/// Disjunction over arbitrary per-segment iterators (spec M6 §2.3):
/// k 路最小值归并，使用索引堆：堆内只存 sub 下标，18.7KB 的
/// SegmentDocIter 不挪动；堆顶即当前最小 doc。保留 M7 §2.3 的
/// matches() 吸收逻辑——停在 best 的子句逐个 confirmation，任一命中
/// 即返回（短路）。
pub struct DisjOverDocIter {
    sub: Vec<SegmentDocIter>,
    heap: Vec<usize>, // heap[h] = index into sub
    pos: Vec<usize>,  // pos[i] = heap position of sub i
    doc: i32,
}

impl DisjOverDocIter {
    pub fn new(sub: Vec<SegmentDocIter>) -> io::Result<DisjOverDocIter> {
        debug_assert!(sub.len() >= 2);
        let n = sub.len();
        let mut it = DisjOverDocIter {
            sub,
            heap: (0..n).collect(),
            pos: (0..n).collect(),
            doc: -1,
        };
        for s in &mut it.sub {
            s.next_doc()?;
        }
        for i in (0..it.heap.len() / 2).rev() {
            it.sift_down(i); // heapify
        }
        Ok(it)
    }

    fn less(&self, a: usize, b: usize) -> bool {
        self.sub[self.heap[a]].doc_id() < self.sub[self.heap[b]].doc_id()
    }

    fn swap(&mut self, a: usize, b: usize) {
        self.heap.swap(a, b);
        self.pos[self.heap[a]] = a;
        self.pos[self.heap[b]] = b;
    }

    fn sift_down(&mut self, mut i: usize) {
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < self.heap.len() && self.less(l, m) {
                m = l;
            }
            if r < self.heap.len() && self.less(r, m) {
                m = r;
            }
            if m == i {
                break;
            }
            self.swap(i, m);
            i = m;
        }
    }
}

impl DocIter for DisjOverDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        loop {
            // 先把仍停在上一个已返回 doc 的子句推进
            if self.doc >= 0 {
                for i in 0..self.sub.len() {
                    if self.sub[i].doc_id() == self.doc {
                        self.sub[i].next_doc()?;
                        self.sift_down(self.pos[i]);
                    }
                }
            }
            let best = self.sub[self.heap[0]].doc_id();
            if best == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
            // M7 §2.3：对停在 best 的子句逐个 confirmation；至少一个
            // true → 命中（短路，省掉其余子句的确认成本）。
            let mut any = false;
            for i in 0..self.sub.len() {
                if self.sub[i].doc_id() == best && self.sub[i].matches()? {
                    any = true;
                    break;
                }
            }
            if any {
                self.doc = best;
                return Ok(best);
            }
            // 全部未命中：推进所有停在 best 的子句并继续
            for i in 0..self.sub.len() {
                if self.sub[i].doc_id() == best {
                    self.sub[i].next_doc()?;
                    self.sift_down(self.pos[i]);
                }
            }
        }
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target || self.doc == NO_MORE_DOCS {
            return Ok(self.doc);
        }
        // M7 §2.3 / M7-review：advance 必须先把所有落后于 target 的子句推到
        // >= target，否则堆顶可能仍 < target（next_doc 只推进等于 self.doc 的
        // 子句，而 target-1 处未必有子句）。每次 advance 后下滤维持堆序。
        for i in 0..self.sub.len() {
            if self.sub[i].doc_id() < target {
                self.sub[i].advance(target)?;
                self.sift_down(self.pos[i]);
            }
        }
        // 现在堆顶 >= target；把当前状态设为 target 前一个 doc，
        // 复用 next_doc() 的 confirmation 循环返回候选。
        self.doc = target - 1;
        self.next_doc()
    }
}

/// Exclusion (spec M6 §2.3, Lucene ReqExclScorer two-pointer): main
/// candidates are probe-advanced against prohibited; a collision drops
/// the candidate. 多个 MUST_NOT 由装配方先 DisjOver 合成一个 prohibited。
pub struct ExcludingDocIter {
    main: Box<SegmentDocIter>,
    prohibited: Box<SegmentDocIter>,
    doc: i32,
}

impl ExcludingDocIter {
    pub fn new(main: SegmentDocIter, prohibited: SegmentDocIter) -> ExcludingDocIter {
        ExcludingDocIter {
            main: Box::new(main),
            prohibited: Box::new(prohibited),
            doc: -1,
        }
    }
    /// Emit main's current doc if not prohibited; else advance main past
    /// the collision and retry.
    fn next_non_excluded(&mut self) -> io::Result<i32> {
        loop {
            let d = self.main.doc_id();
            if d == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
            // M7 §2.3：prohibited 可能是两阶段迭代器（Phrase），advance(d)==d
            // 只是 approximation 命中，必须再调 matches() 确认才排除。
            let prohibited_candidate = self.prohibited.advance(d)? == d;
            let excluded = if prohibited_candidate {
                self.prohibited.matches()?
            } else {
                false
            };
            if !excluded && self.main.matches()? {
                self.doc = d;
                return Ok(d);
            }
            if self.main.next_doc()? == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
        }
    }
}

impl DocIter for ExcludingDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        if self.main.next_doc()? == NO_MORE_DOCS {
            self.doc = NO_MORE_DOCS;
            return Ok(NO_MORE_DOCS);
        }
        self.next_non_excluded()
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target {
            return Ok(self.doc);
        }
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        self.main.advance(target)?;
        self.next_non_excluded()
    }
}

// ── SegmentDocIter ────────────────────────────────────────────────────

pub enum SegmentDocIter {
    Docs(DocsEnum),
    Freqs(DocsFreqsEnum),
    All(MatchAllIter),
    And(ConjunctionDocIter),
    Or(DisjunctionDocIter),
    Bitset(BitsetDocIter),
    Phrase(PhraseDocIter),
    Roaring(RoaringDocIter),
    RoaringAnd(RoaringAndDocIter),
    RoaringOr(RoaringOrDocIter),
    Materialized(MaterializedDocIter),
    // M6 §2.3 通用组合器：全是 Vec/Box 小 payload（≤40B），enum 尺寸
    // 不变（仍由 Freqs 的 18.7KB 决定），栈预算不受新变体影响。
    ConjOver(ConjOverDocIter),
    DisjOver(DisjOverDocIter),
    Excluding(ExcludingDocIter),
}

impl DocIter for SegmentDocIter {
    fn doc_id(&self) -> i32 {
        match self {
            Self::Docs(d) => d.doc_id(),
            Self::Freqs(f) => f.doc_id(),
            Self::All(a) => a.doc_id(),
            Self::And(a) => a.doc_id(),
            Self::Or(o) => o.doc_id(),
            Self::Bitset(b) => b.doc_id(),
            Self::Phrase(p) => p.doc_id(),
            Self::Roaring(r) => r.doc_id(),
            Self::RoaringAnd(a) => a.doc_id(),
            Self::RoaringOr(o) => o.doc_id(),
            Self::Materialized(p) => p.doc_id(),
            Self::ConjOver(c) => c.doc_id(),
            Self::DisjOver(d) => d.doc_id(),
            Self::Excluding(e) => e.doc_id(),
        }
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        match self {
            Self::Docs(d) => d.next_doc(),
            Self::Freqs(f) => f.next_doc(),
            Self::All(a) => a.next_doc(),
            Self::And(a) => a.next_doc(),
            Self::Or(o) => o.next_doc(),
            Self::Bitset(b) => b.next_doc(),
            Self::Phrase(p) => p.next_doc(),
            Self::Roaring(r) => r.next_doc(),
            Self::RoaringAnd(a) => a.next_doc(),
            Self::RoaringOr(o) => o.next_doc(),
            Self::Materialized(p) => p.next_doc(),
            Self::ConjOver(c) => c.next_doc(),
            Self::DisjOver(d) => d.next_doc(),
            Self::Excluding(e) => e.next_doc(),
        }
    }
    fn advance(&mut self, t: i32) -> io::Result<i32> {
        match self {
            Self::Docs(d) => d.advance(t),
            Self::Freqs(f) => f.advance(t),
            Self::All(a) => a.advance(t),
            Self::And(a) => a.advance(t),
            Self::Or(o) => o.advance(t),
            Self::Bitset(b) => b.advance(t),
            Self::Phrase(p) => p.advance(t),
            Self::Roaring(r) => r.advance(t),
            Self::RoaringAnd(a) => a.advance(t),
            Self::RoaringOr(o) => o.advance(t),
            Self::Materialized(p) => p.advance(t),
            Self::ConjOver(c) => c.advance(t),
            Self::DisjOver(d) => d.advance(t),
            Self::Excluding(e) => e.advance(t),
        }
    }
    fn freq(&self) -> u32 {
        match self {
            Self::Freqs(f) => f.freq(),
            Self::And(a) => a.freq(),
            Self::Or(o) => o.freq(),
            _ => 1,
        }
    }
    fn matches(&mut self) -> io::Result<bool> {
        match self {
            Self::Phrase(p) => p.matches(),
            _ => Ok(true),
        }
    }
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        match self {
            // Docs/Freqs 是 codec 类型（无 DocIter impl）：Task 1 内联
            // 逐 doc 填充，Task 3 换成 codec 窗口批读。
            Self::Docs(d) => {
                let n = d.next_docs(&mut out.docs)?;
                out.len = n;
                Ok(n)
            }
            Self::Freqs(f) => {
                let n = if f.decodes_freqs() {
                    f.next_docs_and_freqs(&mut out.docs, &mut out.freqs)?
                } else {
                    f.next_docs(&mut out.docs)?
                };
                out.len = n;
                Ok(n)
            }
            Self::All(a) => a.next_block(out),
            Self::And(a) => a.next_block(out),
            Self::Or(o) => o.next_block(out),
            Self::Bitset(b) => b.next_block(out),
            Self::Phrase(p) => p.next_block(out),
            Self::Roaring(r) => r.next_block(out),
            Self::RoaringAnd(a) => a.next_block(out),
            Self::RoaringOr(o) => o.next_block(out),
            Self::Materialized(p) => p.next_block(out),
            Self::ConjOver(c) => c.next_block(out),
            Self::DisjOver(d) => d.next_block(out),
            Self::Excluding(e) => e.next_block(out),
        }
    }
}
