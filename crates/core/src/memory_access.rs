//! In-memory LeafAccess implementation: borrows DocWriter's RAM buffers
//! and provides data to the generic query execution engine.

use std::io;

use codec_lucene9::automaton::WildcardDfa;
use codec_lucene9::field_infos::{FieldInfo, FieldInfos, IndexOptions};
use codec_lucene9::roaring::FrozenBitmap;

use crate::doc_writer::DocWriter;
use crate::schema::Schema;
use crate::search::doc_iter::{
    MemDocsIter, MemFreqsIter, MemPositionsEnum, PhraseDocIter, PositionsEnumLike, SegmentDocIter,
};
use crate::search::leaf_access::{LeafAccess, PointsAccess, TermsIterAccess};

/// Term handle for in-memory postings.
#[derive(Clone, Debug)]
pub struct MemTermHandle {
    pub field_number: u32,
    pub term_id: u32,
    pub doc_freq: u32,
    pub total_term_freq: u64,
}

/// In-memory leaf: borrows DocWriter's RAM buffers, implements LeafAccess.
pub struct MemoryLeafAccess<'a> {
    dw: &'a DocWriter,
    field_infos: FieldInfos,
    points: Option<MemoryPointsAccess<'a>>,
}

impl<'a> MemoryLeafAccess<'a> {
    pub fn new(dw: &'a DocWriter, schema: &Schema) -> Self {
        let field_infos = build_field_infos(dw, schema);
        let points = Some(MemoryPointsAccess { dw });
        MemoryLeafAccess { dw, field_infos, points }
    }

    fn field_buf(&self, field: &str) -> Option<&crate::doc_writer::FieldBuf> {
        let number = self.dw.fields().iter().position(|f| f.name == field)?;
        self.dw.field_buffer(number as u32)
    }

    fn postings_docs(&self, entry: &MemTermHandle) -> Vec<u32> {
        let buf = self.dw.field_buffer(entry.field_number).unwrap();
        let dict = buf.dict.as_ref().unwrap();
        dict.postings(entry.term_id).docs.clone()
    }

    fn postings_docs_freqs(&self, entry: &MemTermHandle) -> (Vec<u32>, Vec<u32>) {
        let buf = self.dw.field_buffer(entry.field_number).unwrap();
        let dict = buf.dict.as_ref().unwrap();
        let pb = dict.postings(entry.term_id);
        (pb.docs.clone(), pb.freqs.clone())
    }

    fn postings_positions(&self, entry: &MemTermHandle) -> (Vec<u32>, Vec<Vec<u32>>) {
        let buf = self.dw.field_buffer(entry.field_number).unwrap();
        let dict = buf.dict.as_ref().unwrap();
        let pb = dict.postings(entry.term_id);
        (pb.docs.clone(), pb.positions.clone())
    }
}

impl<'a> LeafAccess for MemoryLeafAccess<'a> {
    type TermHandle = MemTermHandle;

    fn max_doc(&self) -> i32 {
        self.dw.max_doc as i32
    }

    fn seek_term(&mut self, field: &str, term: &[u8]) -> io::Result<Option<(bool, MemTermHandle)>> {
        let Some(spec) = self.field_infos.by_name(field) else {
            return Ok(None);
        };
        if spec.index_options == IndexOptions::None {
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
        let field_number = self
            .dw
            .fields()
            .iter()
            .position(|f| f.name == field)
            .unwrap() as u32;
        let pb = dict.postings(id);
        let has_freqs = spec.index_options != IndexOptions::Docs;
        let doc_freq = pb.docs.len() as u32;
        let total_term_freq: u64 = pb.freqs.iter().map(|&f| f as u64).sum();
        Ok(Some((
            has_freqs,
            MemTermHandle { field_number, term_id: id, doc_freq, total_term_freq },
        )))
    }

    fn docs_enum(&self, entry: &MemTermHandle) -> io::Result<SegmentDocIter> {
        let docs = self.postings_docs(entry);
        Ok(SegmentDocIter::MemDocs(MemDocsIter::new(docs)))
    }

    fn docs_freqs_enum(&self, entry: &MemTermHandle, needs_freq: bool) -> io::Result<SegmentDocIter> {
        if needs_freq {
            let (docs, freqs) = self.postings_docs_freqs(entry);
            Ok(SegmentDocIter::MemFreqs(MemFreqsIter::new(docs, freqs)))
        } else {
            let docs = self.postings_docs(entry);
            Ok(SegmentDocIter::MemDocs(MemDocsIter::new(docs)))
        }
    }

    fn positions_enum(&self, entry: &MemTermHandle) -> io::Result<SegmentDocIter> {
        let (docs, positions) = self.postings_positions(entry);
        let pen = MemPositionsEnum::new(docs, positions);
        Ok(SegmentDocIter::Phrase(PhraseDocIter::from_entries(
            vec![(entry.doc_freq, 0, PositionsEnumLike::Mem(pen))],
            None,
        )))
    }

    fn open_term_bitmap(&self, _entry: &MemTermHandle) -> io::Result<Option<FrozenBitmap>> {
        Ok(None) // memory never has roaring bitmaps
    }

    fn field_info(&self, name: &str) -> Option<&FieldInfo> {
        self.field_infos.by_name(name)
    }

    fn field_has_freqs(&self, field: &str) -> Option<bool> {
        self.field_infos.by_name(field).map(|fi| {
            fi.index_options != IndexOptions::Docs && fi.index_options != IndexOptions::None
        })
    }

    fn terms_iter(
        &mut self,
        field: &str,
    ) -> Option<Box<dyn TermsIterAccess<TermHandle = MemTermHandle> + '_>> {
        let field_number = self.dw.fields().iter().position(|f| f.name == field)? as u32;
        let buf = self.dw.field_buffer(field_number)?;
        let dict = buf.dict.as_ref()?;
        Some(Box::new(MemTermsIter::new(dict, field_number)))
    }

    fn intersect_terms(
        &mut self,
        field: &str,
        dfa: &WildcardDfa,
    ) -> io::Result<Option<Vec<(Vec<u8>, MemTermHandle)>>> {
        let field_number = match self.dw.fields().iter().position(|f| f.name == field) {
            Some(n) => n as u32,
            None => return Ok(None),
        };
        let buf = match self.dw.field_buffer(field_number) {
            Some(b) => b,
            None => return Ok(Some(Vec::new())),
        };
        let dict = match buf.dict.as_ref() {
            Some(d) => d,
            None => return Ok(Some(Vec::new())),
        };
        let mut results = Vec::new();
        for id in dict.sorted_ids() {
            let bytes = dict.bytes_of(id);
            if dfa.accepts(bytes) {
                let pb = dict.postings(id);
                let doc_freq = pb.docs.len() as u32;
                let total_term_freq: u64 = pb.freqs.iter().map(|&f| f as u64).sum();
                results.push((
                    bytes.to_vec(),
                    MemTermHandle { field_number, term_id: id, doc_freq, total_term_freq },
                ));
            }
        }
        Ok(Some(results))
    }

    fn points_reader(&self) -> Option<&dyn PointsAccess> {
        self.points.as_ref().map(|p| p as &dyn PointsAccess)
    }

    fn numeric_dv(&self, field: &str, doc: u32) -> Option<i64> {
        self.dw.numeric_dv(field, doc)
    }

    fn term_doc_freq(&self, entry: &MemTermHandle) -> u32 {
        entry.doc_freq
    }
}

// ── MemTermsIter ─────────────────────────────────────────────────────

/// Iterates a field's in-memory term dictionary in sorted order.
struct MemTermsIter<'a> {
    dict: &'a crate::doc_writer::TermDict,
    sorted_ids: Vec<u32>,
    field_number: u32,
    pos: usize,
}

impl<'a> MemTermsIter<'a> {
    fn new(dict: &'a crate::doc_writer::TermDict, field_number: u32) -> Self {
        let sorted_ids = dict.sorted_ids();
        MemTermsIter { dict, sorted_ids, field_number, pos: 0 }
    }
}

impl TermsIterAccess for MemTermsIter<'_> {
    type TermHandle = MemTermHandle;

    fn seek_ceil(&mut self, target: &[u8]) -> io::Result<bool> {
        let result = self.sorted_ids[self.pos..].binary_search_by(|&id| {
            self.dict.bytes_of(id).cmp(target)
        });
        match result {
            Ok(i) => {
                self.pos += i;
                Ok(true)
            }
            Err(i) => {
                self.pos += i;
                Ok(self.pos < self.sorted_ids.len())
            }
        }
    }

    fn next(&mut self) -> io::Result<Option<(Vec<u8>, MemTermHandle)>> {
        if self.pos >= self.sorted_ids.len() {
            return Ok(None);
        }
        let id = self.sorted_ids[self.pos];
        self.pos += 1;
        let bytes = self.dict.bytes_of(id).to_vec();
        let pb = self.dict.postings(id);
        let doc_freq = pb.docs.len() as u32;
        let total_term_freq: u64 = pb.freqs.iter().map(|&f| f as u64).sum();
        Ok(Some((
            bytes,
            MemTermHandle {
                field_number: self.field_number,
                term_id: id,
                doc_freq,
                total_term_freq,
            },
        )))
    }
}

// ── MemoryPointsAccess ───────────────────────────────────────────────

/// Linear-scan points access over in-memory point buffers.
struct MemoryPointsAccess<'a> {
    dw: &'a DocWriter,
}

impl PointsAccess for MemoryPointsAccess<'_> {
    fn intersect(
        &self,
        field: &str,
        low: i64,
        high: i64,
        visitor: &mut dyn FnMut(i64, i32),
    ) -> io::Result<()> {
        let number = self.dw.fields().iter().position(|f| f.name == field);
        let Some(number) = number else { return Ok(()) };
        let Some(buf) = self.dw.field_buffer(number as u32) else {
            return Ok(());
        };
        let Some(points) = &buf.points else { return Ok(()) };
        for &(v, doc) in &points.points {
            if v >= low && v <= high {
                visitor(v, doc as i32);
            }
        }
        Ok(())
    }
}

// ── build_field_infos ────────────────────────────────────────────────

/// Build FieldInfos from the DocWriter's field table, mirroring how
/// segment_builder creates FieldInfos from the schema.
fn build_field_infos(dw: &DocWriter, _schema: &Schema) -> FieldInfos {
    let mut infos = Vec::new();
    for (number, spec) in dw.fields().iter().enumerate() {
        let mut fi = FieldInfo::stored(&spec.name, number as i32);
        if spec.is_indexed() {
            fi.index_options = spec.index_options;
        }
        if spec.points.is_some() {
            fi.point_dimension_count = 1;
            fi.point_index_dimension_count = 1;
            fi.point_num_bytes = spec.points.unwrap().bytes_per_dim as i32;
        }
        fi.doc_values_type = spec.doc_values;
        infos.push(fi);
    }
    FieldInfos::new(infos)
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc_writer::DocWriter;
    use crate::document::{Document, FieldValue};
    use crate::schema::{FieldSpec, Schema};
    use crate::search::doc_iter::DocIter;
    use codec_lucene9::postings_read::NO_MORE_DOCS;

    fn test_schema_and_writer() -> (Schema, DocWriter) {
        let mut schema = Schema::new();
        schema.add(FieldSpec::text_with_positions("message"));
        schema.add(FieldSpec::keyword("level"));
        schema.add(FieldSpec::long_point("ts").with_numeric_dv());

        let mut dw = DocWriter::new();
        // doc 0: "hello world" INFO ts=1000
        let mut d0 = Document::new();
        d0.add("message", FieldValue::Text("hello world".to_string()));
        d0.add("level", FieldValue::Keyword("INFO".to_string()));
        d0.add("ts", FieldValue::Long(1000));
        dw.add_document(&schema, d0, None).unwrap();

        // doc 1: "hello rust" ERROR ts=2000
        let mut d1 = Document::new();
        d1.add("message", FieldValue::Text("hello rust".to_string()));
        d1.add("level", FieldValue::Keyword("ERROR".to_string()));
        d1.add("ts", FieldValue::Long(2000));
        dw.add_document(&schema, d1, None).unwrap();

        // doc 2: "world peace" INFO ts=3000
        let mut d2 = Document::new();
        d2.add("message", FieldValue::Text("world peace".to_string()));
        d2.add("level", FieldValue::Keyword("INFO".to_string()));
        d2.add("ts", FieldValue::Long(3000));
        dw.add_document(&schema, d2, None).unwrap();

        (schema, dw)
    }

    #[test]
    fn seek_term_finds_existing_term() {
        let (schema, dw) = test_schema_and_writer();
        let mut la = MemoryLeafAccess::new(&dw, &schema);

        let result = la.seek_term("message", b"hello").unwrap();
        assert!(result.is_some());
        let (has_freqs, handle) = result.unwrap();
        assert!(has_freqs);
        assert_eq!(handle.doc_freq, 2);
        assert_eq!(handle.total_term_freq, 2);
    }

    #[test]
    fn seek_term_returns_none_for_missing() {
        let (schema, dw) = test_schema_and_writer();
        let mut la = MemoryLeafAccess::new(&dw, &schema);

        assert!(la.seek_term("message", b"nonexistent").unwrap().is_none());
        assert!(la.seek_term("nofield", b"hello").unwrap().is_none());
    }

    #[test]
    fn docs_enum_iterates_correctly() {
        let (schema, dw) = test_schema_and_writer();
        let mut la = MemoryLeafAccess::new(&dw, &schema);

        let (_, handle) = la.seek_term("message", b"hello").unwrap().unwrap();
        let mut iter = la.docs_enum(&handle).unwrap();

        assert_eq!(iter.next_doc().unwrap(), 0);
        assert_eq!(iter.next_doc().unwrap(), 1);
        assert_eq!(iter.next_doc().unwrap(), NO_MORE_DOCS);
    }

    #[test]
    fn docs_freqs_enum_iterates_correctly() {
        let (schema, dw) = test_schema_and_writer();
        let mut la = MemoryLeafAccess::new(&dw, &schema);

        let (_, handle) = la.seek_term("message", b"hello").unwrap().unwrap();
        let mut iter = la.docs_freqs_enum(&handle, true).unwrap();

        assert_eq!(iter.next_doc().unwrap(), 0);
        assert_eq!(iter.freq(), 1);
        assert_eq!(iter.next_doc().unwrap(), 1);
        assert_eq!(iter.freq(), 1);
        assert_eq!(iter.next_doc().unwrap(), NO_MORE_DOCS);
    }

    #[test]
    fn terms_iter_enumerates_sorted() {
        let (schema, dw) = test_schema_and_writer();
        let mut la = MemoryLeafAccess::new(&dw, &schema);

        let mut iter = la.terms_iter("message").unwrap();
        let mut terms = Vec::new();
        while let Some((bytes, entry)) = iter.next().unwrap() {
            terms.push((String::from_utf8(bytes).unwrap(), entry.doc_freq));
        }
        // sorted: hello(2), peace(1), rust(1), world(2)
        assert_eq!(
            terms,
            vec![
                ("hello".to_string(), 2),
                ("peace".to_string(), 1),
                ("rust".to_string(), 1),
                ("world".to_string(), 2),
            ]
        );
    }

    #[test]
    fn terms_iter_seek_ceil() {
        let (schema, dw) = test_schema_and_writer();
        let mut la = MemoryLeafAccess::new(&dw, &schema);

        // seek to "rust" — should find it exactly
        {
            let mut iter = la.terms_iter("message").unwrap();
            assert!(iter.seek_ceil(b"rust").unwrap());
            let (bytes, _) = iter.next().unwrap().unwrap();
            assert_eq!(bytes, b"rust");
        }

        // seek to "q" — should land on "rust" (first >= "q")
        {
            let mut iter = la.terms_iter("message").unwrap();
            assert!(iter.seek_ceil(b"q").unwrap());
            let (bytes, _) = iter.next().unwrap().unwrap();
            assert_eq!(bytes, b"rust");
        }
    }

    #[test]
    fn points_reader_intersects() {
        let (schema, dw) = test_schema_and_writer();
        let la = MemoryLeafAccess::new(&dw, &schema);

        let points = la.points_reader().unwrap();
        let mut hits = Vec::new();
        points
            .intersect("ts", 1500, 2500, &mut |v, doc| hits.push((v, doc)))
            .unwrap();
        assert_eq!(hits, vec![(2000, 1)]);
    }

    #[test]
    fn numeric_dv_reads() {
        let (schema, dw) = test_schema_and_writer();
        let la = MemoryLeafAccess::new(&dw, &schema);

        assert_eq!(la.numeric_dv("ts", 0), Some(1000));
        assert_eq!(la.numeric_dv("ts", 1), Some(2000));
        assert_eq!(la.numeric_dv("ts", 2), Some(3000));
        assert_eq!(la.numeric_dv("ts", 3), None);
        assert_eq!(la.numeric_dv("level", 0), None);
    }

    #[test]
    fn field_info_and_has_freqs() {
        let (schema, dw) = test_schema_and_writer();
        let la = MemoryLeafAccess::new(&dw, &schema);

        let fi = la.field_info("message").unwrap();
        assert_eq!(fi.index_options, IndexOptions::DocsAndFreqsAndPositions);

        assert_eq!(la.field_has_freqs("message"), Some(true));
        assert_eq!(la.field_has_freqs("level"), Some(false)); // Docs only
        assert_eq!(la.field_has_freqs("nofield"), None);
    }

    #[test]
    fn positions_enum_phrase_query() {
        let mut schema = Schema::new();
        schema.add(FieldSpec::text_with_positions("body"));
        let mut dw = DocWriter::new();

        // doc 0: "the quick brown fox"
        let mut d0 = Document::new();
        d0.add("body", FieldValue::Text("the quick brown fox".to_string()));
        dw.add_document(&schema, d0, None).unwrap();

        // doc 1: "quick brown dog"
        let mut d1 = Document::new();
        d1.add("body", FieldValue::Text("quick brown dog".to_string()));
        dw.add_document(&schema, d1, None).unwrap();

        // doc 2: "brown quick fox" (not a phrase match for "quick brown")
        let mut d2 = Document::new();
        d2.add("body", FieldValue::Text("brown quick fox".to_string()));
        dw.add_document(&schema, d2, None).unwrap();

        let mut la = MemoryLeafAccess::new(&dw, &schema);

        // Phrase "quick brown" should match docs 0 and 1, not 2
        let phrase = PhraseDocIter::new(&mut la, "body", &[b"quick".to_vec(), b"brown".to_vec()])
            .unwrap()
            .unwrap();
        let mut iter = SegmentDocIter::Phrase(phrase);

        let mut hits = Vec::new();
        loop {
            let d = iter.next_doc().unwrap();
            if d == NO_MORE_DOCS {
                break;
            }
            if iter.matches().unwrap() {
                hits.push(d);
            }
        }
        assert_eq!(hits, vec![0, 1]);
    }

    #[test]
    fn open_term_bitmap_always_none() {
        let (schema, dw) = test_schema_and_writer();
        let mut la = MemoryLeafAccess::new(&dw, &schema);

        let (_, handle) = la.seek_term("message", b"hello").unwrap().unwrap();
        assert!(la.open_term_bitmap(&handle).unwrap().is_none());
    }

    #[test]
    fn max_doc_correct() {
        let (schema, dw) = test_schema_and_writer();
        let la = MemoryLeafAccess::new(&dw, &schema);
        assert_eq!(la.max_doc(), 3);
    }

    #[test]
    fn keyword_field_no_freqs() {
        let (schema, dw) = test_schema_and_writer();
        let mut la = MemoryLeafAccess::new(&dw, &schema);

        let (has_freqs, handle) = la.seek_term("level", b"INFO").unwrap().unwrap();
        assert!(!has_freqs);
        assert_eq!(handle.doc_freq, 2);

        let mut iter = la.docs_enum(&handle).unwrap();
        assert_eq!(iter.next_doc().unwrap(), 0);
        assert_eq!(iter.next_doc().unwrap(), 2);
        assert_eq!(iter.next_doc().unwrap(), NO_MORE_DOCS);
    }
}
