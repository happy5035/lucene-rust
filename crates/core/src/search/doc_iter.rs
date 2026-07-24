//! DocIdSetIterator semantics (docID starts at -1, ascends, ends at
//! NO_MORE_DOCS) with a Rust object shape (search spec §3: enum Query +
//! trait DocIter, no inheritance). M1 adds AND/OR Boolean iterators.

use std::io;

use codec_lucene9::field_infos::IndexOptions;
use codec_lucene9::postings_read::{DocsEnum, DocsFreqsEnum, PositionsEnum, NO_MORE_DOCS};
use codec_lucene9::roaring::{RoaringView, ViewCursor};
use codec_lucene9::terms_read::TermEntry;

use super::bitset::FixedBitSet;
use super::segment_reader::SegmentReader;

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
        let mut sought: Vec<(u32, u32, PositionsEnum)> = Vec::with_capacity(terms.len());
        for (i, t) in terms.iter().enumerate() {
            let Some((_, entry)) = seg.seek_term(field, t)? else {
                return Ok(None); // absent term: no hits (PhraseWeight null scorer)
            };
            sought.push((entry.doc_freq, i as u32, seg.positions_enum(&entry)?));
        }
        // conjunction lead = cheapest enum first; offsets travel with their
        // enum, so phrase semantics are unaffected
        sought.sort_by_key(|(df, _, _)| *df);
        let occ = sought
            .into_iter()
            .map(|(_, offset, en)| Occurrence { en, offset })
            .collect();
        Ok(Some(PhraseDocIter {
            occ,
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

impl DocIter for PhraseDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }

    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        if self.doc >= 0 {
            for o in &mut self.occ {
                if o.en.doc_id() == self.doc && o.en.next_doc()? == NO_MORE_DOCS {
                    self.doc = NO_MORE_DOCS;
                    return Ok(NO_MORE_DOCS);
                }
            }
        }
        loop {
            // conjunction over the position enums (ConjunctionScorer shape,
            // same dance as ConjunctionDocIter)
            let candidate = self.occ[self.lead].en.doc_id();
            if candidate == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
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
            if !matched {
                continue;
            }
            if self.positions_match()? {
                self.doc = candidate;
                return Ok(candidate);
            }
            // no positional match in this doc: move every occurrence past it
            for o in &mut self.occ {
                if o.en.doc_id() == candidate && o.en.next_doc()? == NO_MORE_DOCS {
                    self.doc = NO_MORE_DOCS;
                    return Ok(NO_MORE_DOCS);
                }
            }
        }
    }
    // advance: trait default (linear next_doc loop) — the search drive only
    // calls next_doc; freq: 1 (ConstantScore, trait default).
}

// ── Roaring (inline term bitmap view, M4 §6) ─────────────────────────

/// DocIter over a term's inline bitmap (M4 §6): wraps the zero-copy
/// full-mode view's byte cursor — next_doc/advance delegate to the
/// view's `cursor_next`/`cursor_advance` (the M3 `RoaringCursor`
/// contract, 关键设计事实 8). The view is boxed: `SegmentDocIter` size
/// discipline (关键设计事实 6). freq() is 1 — the bitmap carries no
/// freqs, and needs_freq paths never get this iterator (correctness
/// requirement (e)).
pub struct RoaringDocIter {
    view: Box<RoaringView>,
    cursor: ViewCursor,
    doc: i32,
}

impl RoaringDocIter {
    pub fn new(view: RoaringView) -> RoaringDocIter {
        let cursor = view.cursor(); // full-mode contract (open_term_bitmap)
        RoaringDocIter {
            view: Box::new(view),
            cursor,
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
        self.doc = match self.view.cursor_next(&mut self.cursor) {
            Some(d) => d as i32,
            None => NO_MORE_DOCS,
        };
        Ok(self.doc)
    }

    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if target > self.doc {
            self.doc = match self
                .view
                .cursor_advance(&mut self.cursor, target.max(0) as u32)
            {
                Some(d) => d as i32,
                None => NO_MORE_DOCS,
            };
        }
        Ok(self.doc)
    }
}

// ── AND over views (M4 §5) ─────────────────────────────────────────────

/// One merge-intersect / merge-union source (M4 §5): a full-mode bitmap
/// view byte cursor, or a materialized low-df clause (df<4096, bounded).
/// Both yield ascending docs with a forward-only advance.
pub enum DocSource {
    View {
        view: Box<RoaringView>,
        cur: ViewCursor,
        doc: Option<u32>,
    },
    Slice {
        docs: Vec<u32>,
        pos: usize,
    },
}

impl DocSource {
    /// Full-mode view source, primed to its first doc. The view is boxed —
    /// a probe/full view owns an IndexInput with an 8KB inline buffer
    /// (关键设计事实 6).
    pub fn view(view: RoaringView) -> DocSource {
        let mut cur = view.cursor(); // full-mode contract (open_term_bitmap)
        let doc = view.cursor_next(&mut cur);
        DocSource::View {
            view: Box::new(view),
            cur,
            doc,
        }
    }

    /// Materialized low-df clause source (spec §5 档 2: df<4096 → ≤4095
    /// docs, ascending by enum construction).
    pub fn slice(docs: Vec<u32>) -> DocSource {
        DocSource::Slice { docs, pos: 0 }
    }

    fn current(&self) -> Option<u32> {
        match self {
            DocSource::View { doc, .. } => *doc,
            DocSource::Slice { docs, pos } => docs.get(*pos).copied(),
        }
    }

    fn next(&mut self) -> Option<u32> {
        match self {
            DocSource::View { view, cur, doc } => {
                *doc = view.cursor_next(cur);
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
            DocSource::View { view, cur, doc } => {
                // guard: cursor_advance requires target > last returned
                if doc.is_some_and(|d| d >= target) {
                    return *doc;
                }
                *doc = view.cursor_advance(cur, target);
                *doc
            }
            DocSource::Slice { docs, pos } => {
                *pos += docs[*pos..].partition_point(|&d| d < target);
                docs.get(*pos).copied()
            }
        }
    }
}

/// AND execution over views (M4 §5): merge-intersect over `sources`
/// (full views + materialized slices); every agreed candidate is
/// point-probed against each `probes` view (contains). Tier shapes:
/// 档 1 偏斜 → sources=[最小侧 full], probes=其余；档 1 非偏斜 →
/// sources=全部 full, probes=[]；档 2 → sources=物化 slices,
/// probes=bitmap 子句. freq() is 1 (ConstantScore, trait default).
pub struct RoaringAndDocIter {
    sources: Vec<DocSource>,
    probes: Vec<Box<RoaringView>>,
    doc: i32,
}

impl RoaringAndDocIter {
    pub fn new(sources: Vec<DocSource>, probes: Vec<RoaringView>) -> RoaringAndDocIter {
        debug_assert!(!sources.is_empty());
        RoaringAndDocIter {
            sources,
            probes: probes.into_iter().map(Box::new).collect(),
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
            let mut pass = true;
            for p in &mut self.probes {
                if !p.contains(target)? {
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

/// OR execution over views (M4 §6): k-way merge-union over `sources`
/// (full-mode bitmap byte cursors + materialized low-df slices) with
/// min-current dedup — zero container rebuilds. The full byte read is
/// unavoidable (spec §2 明确不做); the rebuild tax is gone. freq() is 1
/// (ConstantScore, trait default).
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
}
