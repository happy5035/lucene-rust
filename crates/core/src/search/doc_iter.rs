//! DocIdSetIterator semantics (docID starts at -1, ascends, ends at
//! NO_MORE_DOCS) with a Rust object shape (search spec §3: enum Query +
//! trait DocIter, no inheritance). M1 adds AND/OR Boolean iterators.

use std::io;

use codec_lucene9::field_infos::IndexOptions;
use codec_lucene9::postings_read::{DocsEnum, DocsFreqsEnum, NO_MORE_DOCS};
use codec_lucene9::terms_read::TermEntry;

use super::segment_reader::SegmentReader;

pub trait DocIter {
    fn doc_id(&self) -> i32;
    fn next_doc(&mut self) -> io::Result<i32>;
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc_id() >= target { return Ok(self.doc_id()); }
        loop { let d = self.next_doc()?; if d >= target { return Ok(d); } }
    }
    fn freq(&self) -> u32 { 1 }
}

// ── MatchAll ──────────────────────────────────────────────────────────

pub struct MatchAllIter { doc: i32, max_doc: i32 }
impl MatchAllIter {
    pub fn new(max_doc: i32) -> Self { MatchAllIter { doc: -1, max_doc } }
}
impl DocIter for MatchAllIter {
    fn doc_id(&self) -> i32 { self.doc }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS { return Ok(NO_MORE_DOCS); }
        self.doc += 1;
        if self.doc >= self.max_doc { self.doc = NO_MORE_DOCS; }
        Ok(self.doc)
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if target > self.doc { self.doc = if target >= self.max_doc { NO_MORE_DOCS } else { target }; }
        Ok(self.doc)
    }
}

// ── Internal postings wrapper ─────────────────────────────────────────

enum PostingsIter {
    Docs(DocsEnum),
    Freqs(DocsFreqsEnum),
}
impl PostingsIter {
    fn new(seg: &SegmentReader, entry: &TermEntry, has_freqs: bool) -> io::Result<Self> {
        if has_freqs { Ok(PostingsIter::Freqs(seg.docs_freqs_enum(entry)?)) }
        else { Ok(PostingsIter::Docs(seg.docs_enum(entry)?)) }
    }
    fn doc_id(&self) -> i32 { match self { Self::Docs(d) => d.doc_id(), Self::Freqs(f) => f.doc_id() } }
    fn next_doc(&mut self) -> io::Result<i32> { match self { Self::Docs(d) => d.next_doc(), Self::Freqs(f) => f.next_doc() } }
    fn advance(&mut self, t: i32) -> io::Result<i32> { match self { Self::Docs(d) => d.advance(t), Self::Freqs(f) => f.advance(t) } }
    fn freq(&self) -> u32 { match self { Self::Freqs(f) => f.freq(), _ => 1 } }
}

// ── Conjunction (AND) ─────────────────────────────────────────────────

pub struct ConjunctionDocIter { sub: Vec<PostingsIter>, doc: i32, lead: usize }

impl ConjunctionDocIter {
    pub fn new(seg: &SegmentReader, field: &str, sorted_entries: &[(u32, TermEntry)]) -> io::Result<Self> {
        let fi = seg.field_info(field);
        let has_freqs = fi.map(|f| f.index_options != IndexOptions::Docs).unwrap_or(false);
        let mut sub = Vec::with_capacity(sorted_entries.len());
        for (_, entry) in sorted_entries { sub.push(PostingsIter::new(seg, entry, has_freqs)?); }
        for s in &mut sub { if s.next_doc()? == NO_MORE_DOCS { return Ok(ConjunctionDocIter { sub, doc: NO_MORE_DOCS, lead: 0 }); } }
        Ok(ConjunctionDocIter { sub, doc: -1, lead: 0 })
    }
    fn advance_all_past(&mut self, doc: i32) -> io::Result<bool> {
        for s in &mut self.sub { if s.doc_id() == doc && s.next_doc()? == NO_MORE_DOCS { return Ok(false); } }
        Ok(true)
    }
}

impl DocIter for ConjunctionDocIter {
    fn doc_id(&self) -> i32 { self.doc }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS { return Ok(NO_MORE_DOCS); }
        if self.doc >= 0 && !self.advance_all_past(self.doc)? { self.doc = NO_MORE_DOCS; return Ok(NO_MORE_DOCS); }
        loop {
            let candidate = self.sub[self.lead].doc_id();
            if candidate == NO_MORE_DOCS { self.doc = NO_MORE_DOCS; return Ok(NO_MORE_DOCS); }
            let target = candidate; let mut matched = true;
            for i in 0..self.sub.len() {
                if i == self.lead { continue; }
                let d = self.sub[i].advance(target)?;
                if d == NO_MORE_DOCS { self.doc = NO_MORE_DOCS; return Ok(NO_MORE_DOCS); }
                if d > target { self.lead = i; matched = false; break; }
            }
            if matched { self.doc = target; return Ok(target); }
        }
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target { return Ok(self.doc); }
        if self.doc == NO_MORE_DOCS { return Ok(NO_MORE_DOCS); }
        self.sub[self.lead].advance(target)?;
        self.doc = -1;
        self.next_doc()
    }
    // ConstantScore: no consumer calls freq() on a conjunction and AND-freq is
    // undefined anyway; the sum of sub freqs is a placeholder.
    fn freq(&self) -> u32 { self.sub.iter().map(|s| s.freq()).sum() }
}

// ── Disjunction (OR) ──────────────────────────────────────────────────

pub struct DisjunctionDocIter { sub: Vec<PostingsIter>, doc: i32 }

impl DisjunctionDocIter {
    pub fn new(seg: &SegmentReader, field: &str, sorted_entries: &[(u32, TermEntry)]) -> io::Result<Self> {
        let fi = seg.field_info(field);
        let has_freqs = fi.map(|f| f.index_options != IndexOptions::Docs).unwrap_or(false);
        let mut sub = Vec::with_capacity(sorted_entries.len());
        for (_, entry) in sorted_entries { sub.push(PostingsIter::new(seg, entry, has_freqs)?); }
        for s in &mut sub { s.next_doc()?; }
        Ok(DisjunctionDocIter { sub, doc: -1 })
    }
}

impl DocIter for DisjunctionDocIter {
    fn doc_id(&self) -> i32 { self.doc }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS { return Ok(NO_MORE_DOCS); }
        if self.doc >= 0 { for s in &mut self.sub { if s.doc_id() == self.doc { s.next_doc()?; } } }
        let mut best = NO_MORE_DOCS;
        for s in &self.sub { let d = s.doc_id(); if d != NO_MORE_DOCS && d < best { best = d; } }
        self.doc = best;
        Ok(best)
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target { return Ok(self.doc); }
        if self.doc == NO_MORE_DOCS { return Ok(NO_MORE_DOCS); }
        for s in &mut self.sub { if s.doc_id() < target { s.advance(target)?; } }
        let mut best = NO_MORE_DOCS;
        for s in &self.sub { let d = s.doc_id(); if d != NO_MORE_DOCS && d < best { best = d; } }
        self.doc = best;
        Ok(best)
    }
    // ConstantScore: no consumer calls freq() on a disjunction and OR-freq is
    // undefined anyway; the first matching sub's freq is a placeholder.
    fn freq(&self) -> u32 { for s in &self.sub { if s.doc_id() == self.doc { return s.freq(); } } 1 }
}

// ── SegmentDocIter ────────────────────────────────────────────────────

pub enum SegmentDocIter {
    Docs(DocsEnum), Freqs(DocsFreqsEnum), All(MatchAllIter),
    And(ConjunctionDocIter), Or(DisjunctionDocIter),
}

impl DocIter for SegmentDocIter {
    fn doc_id(&self) -> i32 { match self { Self::Docs(d) => d.doc_id(), Self::Freqs(f) => f.doc_id(), Self::All(a) => a.doc_id(), Self::And(a) => a.doc_id(), Self::Or(o) => o.doc_id() } }
    fn next_doc(&mut self) -> io::Result<i32> { match self { Self::Docs(d) => d.next_doc(), Self::Freqs(f) => f.next_doc(), Self::All(a) => a.next_doc(), Self::And(a) => a.next_doc(), Self::Or(o) => o.next_doc() } }
    fn advance(&mut self, t: i32) -> io::Result<i32> { match self { Self::Docs(d) => d.advance(t), Self::Freqs(f) => f.advance(t), Self::All(a) => a.advance(t), Self::And(a) => a.advance(t), Self::Or(o) => o.advance(t) } }
    fn freq(&self) -> u32 { match self { Self::Freqs(f) => f.freq(), Self::And(a) => a.freq(), Self::Or(o) => o.freq(), _ => 1 } }
}
