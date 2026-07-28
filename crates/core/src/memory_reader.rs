//! In-memory searchable reader (方案B): documents are searchable as soon as
//! they are written to the DocWriter's RAM buffers — no flush/commit needed.
//!
//! Defines the `LeafReader` trait as the common interface that both the
//! disk-backed `SegmentReader` and the in-memory `MemoryLeafReader` satisfy.
//! The `MemorySearcher` provides the same query API (`count`, `top_docs`,
//! `search`) as the disk `Searcher`, operating directly on DocWriter data.

use std::io;

use crate::doc_writer::{DocWriter, FieldBuf};
use crate::document::{Document, FieldValue};
use crate::schema::Schema;
use crate::search::query::{Occur, Query};
use crate::search::Collector;

// ── LeafReader trait (common interface) ──────────────────────────────

/// Common per-leaf read interface. Both disk segments and in-memory buffers
/// implement this, allowing the search layer to be source-agnostic.
pub trait LeafReader {
    fn max_doc(&self) -> i32;

    /// Term lookup: returns (has_freqs, doc_freq, total_term_freq).
    /// None = unknown field / non-indexed / term not present.
    fn seek_term(&self, field: &str, term: &[u8]) -> io::Result<Option<TermMeta>>;

    /// Postings for a term: ascending doc IDs.
    fn postings_docs(&self, field: &str, term: &[u8]) -> io::Result<Vec<u32>>;

    /// Postings with freqs: (doc, freq) pairs, ascending doc.
    fn postings_docs_freqs(&self, field: &str, term: &[u8]) -> io::Result<Vec<(u32, u32)>>;

    /// All terms in a field (sorted), for prefix/wildcard enumeration.
    fn terms_in_field(&self, field: &str) -> io::Result<Vec<Vec<u8>>>;

    /// Point range: docs whose point value falls in [low, high].
    fn point_range_docs(&self, field: &str, low: i64, high: i64) -> io::Result<Vec<u32>>;

    /// Whether the field has positions indexed.
    fn field_has_positions(&self, field: &str) -> bool;

    /// Positions for a term in a doc: sorted position list.
    fn positions_for_doc(&self, field: &str, term: &[u8], doc: u32) -> io::Result<Vec<u32>>;

    /// Stored field value for a doc (None = not stored / missing).
    fn stored_value(&self, doc: u32, field: &str) -> Option<FieldValue>;
}

/// Term metadata returned by seek_term.
#[derive(Debug, Clone, Copy)]
pub struct TermMeta {
    pub has_freqs: bool,
    pub doc_freq: u32,
    pub total_term_freq: u64,
}

// ── MemoryLeafReader ─────────────────────────────────────────────────

/// In-memory leaf reader over a DocWriter's buffers. Zero-copy: borrows the
/// DocWriter directly. Documents are visible immediately after add_document.
pub struct MemoryLeafReader<'a> {
    dw: &'a DocWriter,
    schema: &'a Schema,
    /// Stored documents (parallel to doc IDs).
    stored: &'a [Document],
}

impl<'a> MemoryLeafReader<'a> {
    pub fn new(dw: &'a DocWriter, schema: &'a Schema, stored: &'a [Document]) -> Self {
        Self { dw, schema, stored }
    }

    fn field_buf(&self, field: &str) -> Option<&'a FieldBuf> {
        let number = self.dw.fields().iter().position(|f| f.name == field)?;
        self.dw.field_buffer(number as u32)
    }

    fn field_spec(&self, field: &str) -> Option<&crate::schema::FieldSpec> {
        self.schema.get(field)
    }
}

impl<'a> LeafReader for MemoryLeafReader<'a> {
    fn max_doc(&self) -> i32 {
        self.dw.max_doc as i32
    }

    fn seek_term(&self, field: &str, term: &[u8]) -> io::Result<Option<TermMeta>> {
        let Some(spec) = self.field_spec(field) else {
            return Ok(None);
        };
        if !spec.is_indexed() {
            return Ok(None);
        }
        let Some(buf) = self.field_buf(field) else {
            return Ok(None);
        };
        let Some(dict) = &buf.dict else {
            return Ok(None);
        };
        let Some(id) = dict.find(term) else {
            return Ok(None);
        };
        let pb = dict.postings(id);
        let has_freqs = spec.index_options != codec_lucene9::IndexOptions::Docs;
        let doc_freq = pb.docs.len() as u32;
        let total_term_freq: u64 = pb.freqs.iter().map(|&f| f as u64).sum();
        Ok(Some(TermMeta {
            has_freqs,
            doc_freq,
            total_term_freq,
        }))
    }

    fn postings_docs(&self, field: &str, term: &[u8]) -> io::Result<Vec<u32>> {
        let Some(buf) = self.field_buf(field) else {
            return Ok(Vec::new());
        };
        let Some(dict) = &buf.dict else {
            return Ok(Vec::new());
        };
        let Some(id) = dict.find(term) else {
            return Ok(Vec::new());
        };
        Ok(dict.postings(id).docs.clone())
    }

    fn postings_docs_freqs(&self, field: &str, term: &[u8]) -> io::Result<Vec<(u32, u32)>> {
        let Some(buf) = self.field_buf(field) else {
            return Ok(Vec::new());
        };
        let Some(dict) = &buf.dict else {
            return Ok(Vec::new());
        };
        let Some(id) = dict.find(term) else {
            return Ok(Vec::new());
        };
        let pb = dict.postings(id);
        Ok(pb.docs.iter().zip(pb.freqs.iter()).map(|(&d, &f)| (d, f)).collect())
    }

    fn terms_in_field(&self, field: &str) -> io::Result<Vec<Vec<u8>>> {
        let Some(buf) = self.field_buf(field) else {
            return Ok(Vec::new());
        };
        let Some(dict) = &buf.dict else {
            return Ok(Vec::new());
        };
        let ids = dict.sorted_ids();
        Ok(ids.into_iter().map(|id| dict.bytes_of(id).to_vec()).collect())
    }

    fn point_range_docs(&self, field: &str, low: i64, high: i64) -> io::Result<Vec<u32>> {
        if low > high {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("point range low ({low}) > high ({high})"),
            ));
        }
        let Some(buf) = self.field_buf(field) else {
            return Ok(Vec::new());
        };
        let Some(points) = &buf.points else {
            return Ok(Vec::new());
        };
        let mut docs: Vec<u32> = points
            .points
            .iter()
            .filter(|&&(v, _)| v >= low && v <= high)
            .map(|&(_, d)| d)
            .collect();
        docs.sort_unstable();
        docs.dedup();
        Ok(docs)
    }

    fn field_has_positions(&self, field: &str) -> bool {
        self.field_spec(field).map_or(false, |s| s.has_positions())
    }

    fn positions_for_doc(&self, field: &str, term: &[u8], doc: u32) -> io::Result<Vec<u32>> {
        let Some(buf) = self.field_buf(field) else {
            return Ok(Vec::new());
        };
        let Some(dict) = &buf.dict else {
            return Ok(Vec::new());
        };
        let Some(id) = dict.find(term) else {
            return Ok(Vec::new());
        };
        let pb = dict.postings(id);
        // Binary search for the doc in the sorted docs array
        match pb.docs.binary_search(&doc) {
            Ok(idx) if idx < pb.positions.len() => Ok(pb.positions[idx].clone()),
            _ => Ok(Vec::new()),
        }
    }

    fn stored_value(&self, doc: u32, field: &str) -> Option<FieldValue> {
        let d = self.stored.get(doc as usize)?;
        d.fields
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, v)| v.clone())
    }
}

// ── MemorySearcher ───────────────────────────────────────────────────

/// In-memory searcher: same query API as the disk Searcher, but operates
/// directly on DocWriter RAM buffers. No flush/commit required — documents
/// are searchable the instant they are written.
pub struct MemorySearcher<'a> {
    reader: MemoryLeafReader<'a>,
}

impl<'a> MemorySearcher<'a> {
    pub fn new(dw: &'a DocWriter, schema: &'a Schema, stored: &'a [Document]) -> Self {
        Self {
            reader: MemoryLeafReader::new(dw, schema, stored),
        }
    }

    pub fn max_doc(&self) -> i32 {
        self.reader.max_doc()
    }

    /// Count matching documents.
    pub fn count(&self, query: &Query) -> io::Result<u64> {
        let docs = self.exec_query(query)?;
        Ok(docs.len() as u64)
    }

    /// (total hits, first n docIDs ascending).
    pub fn top_docs(&self, query: &Query, n: usize) -> io::Result<(u64, Vec<i32>)> {
        let docs = self.exec_query(query)?;
        let total = docs.len() as u64;
        let top: Vec<i32> = docs.iter().take(n).map(|&d| d as i32).collect();
        Ok((total, top))
    }

    /// Drive a collector over matching docs.
    pub fn search<C: Collector>(&self, query: &Query, collector: &mut C) -> io::Result<()> {
        let needs_freq = collector.needs_freq();
        if needs_freq {
            // For freq-aware collectors, we need per-term freqs (only Term queries)
            if let Query::Term { field, term } = query {
                let pairs = self.reader.postings_docs_freqs(field, term)?;
                for (doc, freq) in pairs {
                    collector.collect(doc as i32, freq);
                }
                return Ok(());
            }
        }
        let docs = self.exec_query(query)?;
        for d in docs {
            collector.collect(d as i32, 1);
        }
        Ok(())
    }

    /// Total term frequency for a Term query.
    pub fn freq_sum(&self, query: &Query) -> io::Result<u64> {
        match query {
            Query::Term { field, term } => {
                match self.reader.seek_term(field, term)? {
                    Some(meta) => Ok(meta.total_term_freq),
                    None => Ok(0),
                }
            }
            Query::MatchAll => Ok(self.reader.max_doc() as u64),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "freq_sum is only defined for Term queries",
            )),
        }
    }

    /// Retrieve stored fields for a doc.
    pub fn document(&self, doc_id: i32) -> Option<&'a Document> {
        self.reader.stored.get(doc_id as usize)
    }

    // ── Query execution engine ───────────────────────────────────────

    /// Execute a query and return sorted matching doc IDs.
    pub(crate) fn exec_query(&self, query: &Query) -> io::Result<Vec<u32>> {
        match query {
            Query::MatchAll => Ok((0..self.reader.max_doc() as u32).collect()),

            Query::Term { field, term } => self.reader.postings_docs(field, term),

            Query::And { field, terms } => {
                if terms.is_empty() {
                    return Ok(Vec::new());
                }
                if terms.len() == 1 {
                    return self.reader.postings_docs(field, &terms[0]);
                }
                // Intersect all term doc sets
                let mut sets: Vec<Vec<u32>> = Vec::with_capacity(terms.len());
                for t in terms {
                    let docs = self.reader.postings_docs(field, t)?;
                    if docs.is_empty() {
                        return Ok(Vec::new()); // empty intersection
                    }
                    sets.push(docs);
                }
                // Sort by length (smallest first) for efficiency
                sets.sort_by_key(|s| s.len());
                let mut result = sets[0].clone();
                for set in &sets[1..] {
                    result = intersect_sorted(&result, set);
                    if result.is_empty() {
                        break;
                    }
                }
                Ok(result)
            }

            Query::Or { field, terms } | Query::Terms { field, terms } => {
                if terms.is_empty() {
                    return Ok(Vec::new());
                }
                if terms.len() == 1 {
                    return self.reader.postings_docs(field, &terms[0]);
                }
                let mut result: Vec<u32> = Vec::new();
                for t in terms {
                    let docs = self.reader.postings_docs(field, t)?;
                    result = union_sorted(&result, &docs);
                }
                Ok(result)
            }

            Query::Prefix { field, prefix } => {
                let all_terms = self.reader.terms_in_field(field)?;
                let mut result: Vec<u32> = Vec::new();
                for t in &all_terms {
                    if t.starts_with(prefix.as_slice()) {
                        let docs = self.reader.postings_docs(field, t)?;
                        result = union_sorted(&result, &docs);
                    }
                }
                Ok(result)
            }

            Query::Wildcard { field, pattern } => {
                let pat = WildcardPattern::parse(pattern);
                let all_terms = self.reader.terms_in_field(field)?;
                let mut result: Vec<u32> = Vec::new();
                for t in &all_terms {
                    if pat.matches(t) {
                        let docs = self.reader.postings_docs(field, t)?;
                        result = union_sorted(&result, &docs);
                    }
                }
                Ok(result)
            }

            Query::Phrase { field, terms } => {
                if terms.is_empty() {
                    return Ok(Vec::new());
                }
                if terms.len() == 1 {
                    return self.reader.postings_docs(field, &terms[0]);
                }
                if !self.reader.field_has_positions(field) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("field '{field}' does not have positions"),
                    ));
                }
                self.exec_phrase(field, terms)
            }

            Query::PointRange { field, low, high } => {
                self.reader.point_range_docs(field, *low, *high)
            }

            Query::Bool { clauses } => self.exec_bool(clauses),
        }
    }

    fn exec_phrase(&self, field: &str, terms: &[Vec<u8>]) -> io::Result<Vec<u32>> {
        // Get candidate docs: intersection of all term postings
        let mut candidates: Option<Vec<u32>> = None;
        for t in terms {
            let docs = self.reader.postings_docs(field, t)?;
            if docs.is_empty() {
                return Ok(Vec::new());
            }
            candidates = Some(match candidates {
                None => docs,
                Some(prev) => intersect_sorted(&prev, &docs),
            });
            if candidates.as_ref().unwrap().is_empty() {
                return Ok(Vec::new());
            }
        }
        let candidates = candidates.unwrap();

        // Position verification: for each candidate doc, check phrase alignment
        let mut result = Vec::new();
        for &doc in &candidates {
            if self.phrase_matches_doc(field, terms, doc)? {
                result.push(doc);
            }
        }
        Ok(result)
    }

    fn phrase_matches_doc(&self, field: &str, terms: &[Vec<u8>], doc: u32) -> io::Result<bool> {
        // Get positions for each term in this doc
        let mut pos_lists: Vec<Vec<u32>> = Vec::with_capacity(terms.len());
        for t in terms {
            let positions = self.reader.positions_for_doc(field, t, doc)?;
            if positions.is_empty() {
                return Ok(false);
            }
            pos_lists.push(positions);
        }
        // Check: exists p0 in pos_lists[0] such that for all i,
        // p0 - 0 + i is in pos_lists[i]
        for &p0 in &pos_lists[0] {
            let mut matched = true;
            for (i, positions) in pos_lists.iter().enumerate().skip(1) {
                let expected = p0 as i64 + i as i64;
                if expected < 0 || positions.binary_search(&(expected as u32)).is_err() {
                    matched = false;
                    break;
                }
            }
            if matched {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn exec_bool(&self, clauses: &[(Occur, Query)]) -> io::Result<Vec<u32>> {
        if clauses.is_empty() {
            return Ok(Vec::new());
        }
        let mut musts: Vec<Vec<u32>> = Vec::new();
        let mut shoulds: Vec<Vec<u32>> = Vec::new();
        let mut nots: Vec<Vec<u32>> = Vec::new();
        let mut has_must_not = false;

        for (occur, q) in clauses {
            let docs = self.exec_query(q)?;
            match occur {
                Occur::Must => {
                    if docs.is_empty() {
                        return Ok(Vec::new()); // MUST with no hits → empty
                    }
                    musts.push(docs);
                }
                Occur::Should => {
                    if !docs.is_empty() {
                        shoulds.push(docs);
                    }
                }
                Occur::MustNot => {
                    has_must_not = true;
                    if !docs.is_empty() {
                        nots.push(docs);
                    }
                }
            }
        }

        // Positive set
        let mut positive: Vec<u32>;
        if !musts.is_empty() {
            positive = musts[0].clone();
            for s in &musts[1..] {
                positive = intersect_sorted(&positive, s);
                if positive.is_empty() {
                    return Ok(Vec::new());
                }
            }
        } else if !shoulds.is_empty() {
            positive = Vec::new();
            for s in &shoulds {
                positive = union_sorted(&positive, s);
            }
        } else if has_must_not {
            // Pure MUST_NOT: MatchAll minus prohibited
            positive = (0..self.reader.max_doc() as u32).collect();
        } else {
            return Ok(Vec::new());
        }

        // Apply exclusions
        if !nots.is_empty() {
            let mut prohibited = Vec::new();
            for s in &nots {
                prohibited = union_sorted(&prohibited, s);
            }
            positive = difference_sorted(&positive, &prohibited);
        }

        Ok(positive)
    }
}

// ── Sorted set operations ────────────────────────────────────────────

fn intersect_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    result
}

fn union_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => {
                result.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                result.push(b[j]);
                j += 1;
            }
        }
    }
    result.extend_from_slice(&a[i..]);
    result.extend_from_slice(&b[j..]);
    result
}

fn difference_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(a.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() {
        if j >= b.len() || a[i] < b[j] {
            result.push(a[i]);
            i += 1;
        } else if a[i] == b[j] {
            i += 1;
            j += 1;
        } else {
            j += 1;
        }
    }
    result
}

// ── Wildcard pattern matching ────────────────────────────────────────

struct WildcardPattern {
    segments: Vec<WildSeg>,
}

enum WildSeg {
    Any,       // *
    OneChar,   // ?
    Literal(Vec<u8>),
}

impl WildcardPattern {
    fn parse(pattern: &[u8]) -> Self {
        let mut segments = Vec::new();
        let mut lit: Vec<u8> = Vec::new();
        for &b in pattern {
            match b {
                b'*' => {
                    if !lit.is_empty() {
                        segments.push(WildSeg::Literal(std::mem::take(&mut lit)));
                    }
                    segments.push(WildSeg::Any);
                }
                b'?' => {
                    if !lit.is_empty() {
                        segments.push(WildSeg::Literal(std::mem::take(&mut lit)));
                    }
                    segments.push(WildSeg::OneChar);
                }
                _ => lit.push(b),
            }
        }
        if !lit.is_empty() {
            segments.push(WildSeg::Literal(lit));
        }
        Self { segments }
    }

    fn matches(&self, text: &[u8]) -> bool {
        self.match_recursive(&self.segments, text)
    }

    fn match_recursive(&self, segs: &[WildSeg], text: &[u8]) -> bool {
        if segs.is_empty() {
            return text.is_empty();
        }
        match &segs[0] {
            WildSeg::Any => {
                // Try matching * against 0, 1, 2, ... chars
                for i in 0..=text.len() {
                    if self.match_recursive(&segs[1..], &text[i..]) {
                        return true;
                    }
                }
                false
            }
            WildSeg::OneChar => {
                if text.is_empty() {
                    return false;
                }
                // Handle multi-byte UTF-8: consume one char
                let ch_len = utf8_char_len(text[0]);
                if text.len() < ch_len {
                    return false;
                }
                self.match_recursive(&segs[1..], &text[ch_len..])
            }
            WildSeg::Literal(lit) => {
                if text.len() < lit.len() || &text[..lit.len()] != lit.as_slice() {
                    return false;
                }
                self.match_recursive(&segs[1..], &text[lit.len()..])
            }
        }
    }
}

fn utf8_char_len(first_byte: u8) -> usize {
    if first_byte < 0x80 {
        1
    } else if first_byte < 0xE0 {
        2
    } else if first_byte < 0xF0 {
        3
    } else {
        4
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc_writer::DocWriter;
    use crate::document::{Document, FieldValue};
    use crate::schema::{FieldSpec, Schema};
    use crate::search::query::Query;

    fn test_schema() -> Schema {
        let mut s = Schema::new();
        s.add(FieldSpec::keyword("level"));
        s.add(FieldSpec::keyword("tid"));
        s.add(FieldSpec::text("message"));
        s.add(FieldSpec::long_point("ts").with_numeric_dv());
        s
    }

    fn build_test_data() -> (DocWriter, Schema, Vec<Document>) {
        let schema = test_schema();
        let mut dw = DocWriter::new();
        let mut stored = Vec::new();

        for i in 0..10u32 {
            let level = if i % 2 == 0 { "INFO" } else { "WARN" };
            let mut doc = Document::new();
            doc.add("level", FieldValue::Keyword(level.to_string()));
            doc.add("tid", FieldValue::Keyword(format!("tid-{i}")));
            doc.add("message", FieldValue::Text(format!("w{} common", i % 3)));
            doc.add("ts", FieldValue::Long(1000 + i as i64));

            dw.add_document(&schema, doc.clone(), None).unwrap();
            stored.push(doc);
        }
        (dw, schema, stored)
    }

    #[test]
    fn memory_term_query() {
        let (dw, schema, stored) = build_test_data();
        let s = MemorySearcher::new(&dw, &schema, &stored);

        assert_eq!(s.max_doc(), 10);
        assert_eq!(s.count(&Query::term("level", "INFO")).unwrap(), 5);
        assert_eq!(s.count(&Query::term("level", "WARN")).unwrap(), 5);
        assert_eq!(s.count(&Query::term("level", "DEBUG")).unwrap(), 0);
        assert_eq!(s.count(&Query::term("tid", "tid-7")).unwrap(), 1);
        assert_eq!(s.count(&Query::term("message", "common")).unwrap(), 10);
        assert_eq!(s.count(&Query::term("message", "w0")).unwrap(), 4); // docs 0,3,6,9
    }

    #[test]
    fn memory_matchall() {
        let (dw, schema, stored) = build_test_data();
        let s = MemorySearcher::new(&dw, &schema, &stored);
        assert_eq!(s.count(&Query::MatchAll).unwrap(), 10);
        let (total, docs) = s.top_docs(&Query::MatchAll, 5).unwrap();
        assert_eq!(total, 10);
        assert_eq!(docs, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn memory_and_or() {
        let (dw, schema, stored) = build_test_data();
        let s = MemorySearcher::new(&dw, &schema, &stored);

        // AND: w0 ∩ common = docs with both = {0,3,6,9}
        let q = Query::and("message", &["w0", "common"]);
        assert_eq!(s.count(&q).unwrap(), 4);

        // AND with missing term → 0
        let q = Query::and("message", &["w0", "nosuch"]);
        assert_eq!(s.count(&q).unwrap(), 0);

        // OR: w0 ∪ w1 = {0,1,3,4,6,7,9} (w0: 0,3,6,9; w1: 1,4,7)
        let q = Query::or("message", &["w0", "w1"]);
        assert_eq!(s.count(&q).unwrap(), 7);
    }

    #[test]
    fn memory_prefix() {
        let (dw, schema, stored) = build_test_data();
        let s = MemorySearcher::new(&dw, &schema, &stored);

        // prefix "w" matches w0, w1, w2 → all 10 docs
        assert_eq!(s.count(&Query::prefix("message", "w")).unwrap(), 10);
        // prefix "w0" matches only w0 → 4 docs
        assert_eq!(s.count(&Query::prefix("message", "w0")).unwrap(), 4);
        // prefix "tid-1" matches tid-1 only (tid-10..19 don't exist)
        assert_eq!(s.count(&Query::prefix("tid", "tid-1")).unwrap(), 1);
    }

    #[test]
    fn memory_wildcard() {
        let (dw, schema, stored) = build_test_data();
        let s = MemorySearcher::new(&dw, &schema, &stored);

        assert_eq!(s.count(&Query::wildcard("message", "w*")).unwrap(), 10);
        assert_eq!(s.count(&Query::wildcard("message", "w?")).unwrap(), 10);
        assert_eq!(s.count(&Query::wildcard("level", "INF*")).unwrap(), 5);
        assert_eq!(s.count(&Query::wildcard("level", "????")).unwrap(), 10); // INFO=4chars, WARN=4chars → all 10
    }

    #[test]
    fn memory_point_range() {
        let (dw, schema, stored) = build_test_data();
        let s = MemorySearcher::new(&dw, &schema, &stored);

        // ts values: 1000..1009
        let q = Query::point_range("ts", 1002, 1005);
        assert_eq!(s.count(&q).unwrap(), 4); // docs 2,3,4,5
        let (_, docs) = s.top_docs(&q, 10).unwrap();
        assert_eq!(docs, vec![2, 3, 4, 5]);

        // Full range
        let q = Query::point_range("ts", 0, 9999);
        assert_eq!(s.count(&q).unwrap(), 10);

        // Empty range
        let q = Query::point_range("ts", 2000, 3000);
        assert_eq!(s.count(&q).unwrap(), 0);
    }

    #[test]
    fn memory_bool_query() {
        let (dw, schema, stored) = build_test_data();
        let s = MemorySearcher::new(&dw, &schema, &stored);

        // MUST level=INFO AND message=w0 → docs 0,6 (even AND i%3==0)
        let q = Query::bool(vec![
            (Occur::Must, Query::term("level", "INFO")),
            (Occur::Must, Query::term("message", "w0")),
        ]);
        assert_eq!(s.count(&q).unwrap(), 2);
        let (_, docs) = s.top_docs(&q, 10).unwrap();
        assert_eq!(docs, vec![0, 6]);

        // SHOULD: level=INFO OR level=WARN → all 10
        let q = Query::bool(vec![
            (Occur::Should, Query::term("level", "INFO")),
            (Occur::Should, Query::term("level", "WARN")),
        ]);
        assert_eq!(s.count(&q).unwrap(), 10);

        // MUST + MUST_NOT: message=common NOT level=WARN → INFO docs = 5
        let q = Query::bool(vec![
            (Occur::Must, Query::term("message", "common")),
            (Occur::MustNot, Query::term("level", "WARN")),
        ]);
        assert_eq!(s.count(&q).unwrap(), 5);
    }

    #[test]
    fn memory_phrase_query() {
        let mut schema = Schema::new();
        schema.add(FieldSpec::text_with_positions("message"));
        let mut dw = DocWriter::new();
        let mut stored = Vec::new();

        let docs_text = [
            "quick brown fox",   // 0: "quick brown" hit
            "quick fox brown",   // 1: not adjacent
            "quick quick brown", // 2: 2nd quick aligns
        ];
        for text in &docs_text {
            let mut doc = Document::new();
            doc.add("message", FieldValue::Text(text.to_string()));
            dw.add_document(&schema, doc.clone(), None).unwrap();
            stored.push(doc);
        }

        let s = MemorySearcher::new(&dw, &schema, &stored);
        let q = Query::phrase("message", &["quick", "brown"]);
        assert_eq!(s.count(&q).unwrap(), 2);
        let (_, docs) = s.top_docs(&q, 10).unwrap();
        assert_eq!(docs, vec![0, 2]);

        let q = Query::phrase("message", &["quick", "fox"]);
        let (_, docs) = s.top_docs(&q, 10).unwrap();
        assert_eq!(docs, vec![1]);
    }

    #[test]
    fn memory_freq_sum() {
        let (dw, schema, stored) = build_test_data();
        let s = MemorySearcher::new(&dw, &schema, &stored);

        // "common" appears once in each of 10 docs → total_term_freq = 10
        assert_eq!(s.freq_sum(&Query::term("message", "common")).unwrap(), 10);
        // "w0" appears once in each of 4 docs → 4
        assert_eq!(s.freq_sum(&Query::term("message", "w0")).unwrap(), 4);
    }

    #[test]
    fn memory_stored_fields() {
        let (dw, schema, stored) = build_test_data();
        let s = MemorySearcher::new(&dw, &schema, &stored);

        let doc = s.document(3).unwrap();
        let level = doc.fields.iter().find(|(n, _)| n == "level").unwrap();
        assert_eq!(level.1, FieldValue::Keyword("WARN".to_string()));
    }

    #[test]
    fn memory_unknown_field_returns_zero() {
        let (dw, schema, stored) = build_test_data();
        let s = MemorySearcher::new(&dw, &schema, &stored);
        assert_eq!(s.count(&Query::term("nope", "x")).unwrap(), 0);
        assert_eq!(s.count(&Query::prefix("nope", "x")).unwrap(), 0);
    }

    #[test]
    fn memory_searchable_immediately_after_each_write() {
        let schema = test_schema();
        let mut dw = DocWriter::new();
        let mut stored = Vec::new();

        // After each write, the doc is immediately searchable
        for i in 0..5u32 {
            let mut doc = Document::new();
            doc.add("level", FieldValue::Keyword("INFO".to_string()));
            doc.add("tid", FieldValue::Keyword(format!("tid-{i}")));
            doc.add("message", FieldValue::Text("hello world".to_string()));
            doc.add("ts", FieldValue::Long(i as i64));
            dw.add_document(&schema, doc.clone(), None).unwrap();
            stored.push(doc);

            let s = MemorySearcher::new(&dw, &schema, &stored);
            assert_eq!(s.count(&Query::term("level", "INFO")).unwrap(), (i + 1) as u64);
            assert_eq!(s.count(&Query::MatchAll).unwrap(), (i + 1) as u64);
        }
    }
}
