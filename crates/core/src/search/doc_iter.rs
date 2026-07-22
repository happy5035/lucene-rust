//! DocIdSetIterator semantics (docID starts at -1, ascends, ends at
//! NO_MORE_DOCS) with a Rust object shape (search spec §3: enum Query +
//! trait DocIter, no inheritance).

use std::io;

use codec_lucene9::postings_read::{DocsEnum, DocsFreqsEnum, NO_MORE_DOCS};

pub trait DocIter {
    fn doc_id(&self) -> i32;
    fn next_doc(&mut self) -> io::Result<i32>;

    /// M1: linear advance (next_doc loop); skip-data-driven advance is a
    /// later phase (search spec phase 3, Boolean conjunction).
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

    /// Current doc's term frequency (1 for docs-only iterators).
    fn freq(&self) -> u32 {
        1
    }
}

/// MatchAllDocsQuery: [0..maxDoc) scan (spec §1: segment metadata construction baseline).
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

/// Per-segment iterators (enum dispatch, no boxing).
pub enum SegmentDocIter {
    Docs(DocsEnum),
    Freqs(DocsFreqsEnum),
    All(MatchAllIter),
}

impl DocIter for SegmentDocIter {
    fn doc_id(&self) -> i32 {
        match self {
            SegmentDocIter::Docs(d) => d.doc_id(),
            SegmentDocIter::Freqs(f) => f.doc_id(),
            SegmentDocIter::All(a) => a.doc_id(),
        }
    }

    fn next_doc(&mut self) -> io::Result<i32> {
        match self {
            SegmentDocIter::Docs(d) => d.next_doc(),
            SegmentDocIter::Freqs(f) => f.next_doc(),
            SegmentDocIter::All(a) => a.next_doc(),
        }
    }

    fn advance(&mut self, target: i32) -> io::Result<i32> {
        match self {
            SegmentDocIter::Docs(d) => d.advance(target),
            SegmentDocIter::Freqs(f) => f.advance(target),
            SegmentDocIter::All(a) => a.advance(target),
        }
    }

    fn freq(&self) -> u32 {
        match self {
            SegmentDocIter::Freqs(f) => f.freq(),
            _ => 1,
        }
    }
}
