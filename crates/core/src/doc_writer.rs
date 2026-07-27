use codec_lucene9::stored_fields::StoredFieldsWriter;
use codec_lucene9::StoredField;

use crate::document::{Document, FieldValue};
use crate::schema::{FieldSpec, Schema};
use crate::sort::{DocMap, Key};
use crate::tokenizer::WhitespaceTokens;

/// Postings buffer for one term: parallel doc/freq arrays, ascending doc IDs.
/// `positions` is only populated for fields with positions: one sorted
/// position list per doc, parallel to `docs`.
#[derive(Default)]
pub struct PostingBuf {
    pub docs: Vec<u32>,
    pub freqs: Vec<u32>,
    pub positions: Vec<Vec<u32>>,
}

impl PostingBuf {
    /// Returns true when this occurrence starts a new doc.
    fn add_occurrence(&mut self, doc: u32, position: Option<u32>) -> bool {
        if self.docs.last() == Some(&doc) {
            *self.freqs.last_mut().unwrap() += 1;
            if let Some(p) = position {
                self.positions.last_mut().unwrap().push(p);
            }
            false
        } else {
            self.docs.push(doc);
            self.freqs.push(1);
            if let Some(p) = position {
                self.positions.push(vec![p]);
            }
            true
        }
    }
}

struct TermRec {
    off: u32,
    len: u32,
    hash: u32,
    postings: PostingBuf,
}

/// Term dictionary with arena-stored bytes and open addressing (Lucene's
/// BytesRefHash analog): lookup hashes the incoming bytes in place and never
/// allocates; term bytes are copied into the arena only on first sight.
pub struct TermDict {
    arena: Vec<u8>,
    recs: Vec<TermRec>,
    /// open-addressing table: term_id + 1, 0 = empty.
    slots: Vec<u32>,
    mask: usize,
}

const INITIAL_SLOTS: usize = 1024;

impl Default for TermDict {
    fn default() -> Self {
        Self::new()
    }
}

impl TermDict {
    pub fn new() -> Self {
        Self {
            arena: Vec::new(),
            recs: Vec::new(),
            slots: vec![0; INITIAL_SLOTS],
            mask: INITIAL_SLOTS - 1,
        }
    }

    pub fn len(&self) -> usize {
        self.recs.len()
    }

    pub fn bytes_of(&self, id: u32) -> &[u8] {
        let rec = &self.recs[id as usize];
        let off = rec.off as usize;
        &self.arena[off..off + rec.len as usize]
    }

    pub fn postings(&self, id: u32) -> &PostingBuf {
        &self.recs[id as usize].postings
    }

    /// Term ids ordered by term bytes (block-tree write order).
    pub fn sorted_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = (0..self.recs.len() as u32).collect();
        let arena = &self.arena;
        let recs = &self.recs;
        ids.sort_unstable_by(|&a, &b| {
            let ra = &recs[a as usize];
            let rb = &recs[b as usize];
            let sa = &arena[ra.off as usize..ra.off as usize + ra.len as usize];
            let sb = &arena[rb.off as usize..rb.off as usize + rb.len as usize];
            sa.cmp(sb)
        });
        ids
    }

    /// Resolves a term to its id, or None if never seen.
    pub fn find(&self, bytes: &[u8]) -> Option<u32> {
        let h = hash_bytes(bytes);
        let mut slot = (h as usize) & self.mask;
        loop {
            let v = self.slots[slot];
            if v == 0 {
                return None;
            }
            let id = v - 1;
            let rec = &self.recs[id as usize];
            let off = rec.off as usize;
            if rec.hash == h
                && rec.len as usize == bytes.len()
                && &self.arena[off..off + rec.len as usize] == bytes
            {
                return Some(id);
            }
            slot = (slot + 1) & self.mask;
        }
    }

    #[cfg(test)]
    fn lookup_or_insert(&mut self, bytes: &[u8]) -> u32 {
        self.lookup_or_insert_flag(bytes).0
    }

    /// Like `lookup_or_insert`, also reporting whether the term is new (for
    /// RAM accounting).
    fn lookup_or_insert_flag(&mut self, bytes: &[u8]) -> (u32, bool) {
        let h = hash_bytes(bytes);
        let mut slot = (h as usize) & self.mask;
        loop {
            let v = self.slots[slot];
            if v == 0 {
                let id = self.recs.len() as u32;
                let off = self.arena.len() as u32;
                self.arena.extend_from_slice(bytes);
                self.recs.push(TermRec {
                    off,
                    len: bytes.len() as u32,
                    hash: h,
                    postings: PostingBuf::default(),
                });
                self.slots[slot] = id + 1;
                // keep load factor <= 0.75
                if (self.recs.len() + 1) * 4 > (self.mask + 1) * 3 {
                    self.grow();
                }
                return (id, true);
            }
            let id = v - 1;
            let rec = &self.recs[id as usize];
            let off = rec.off as usize;
            if rec.hash == h
                && rec.len as usize == bytes.len()
                && &self.arena[off..off + rec.len as usize] == bytes
            {
                return (id, false);
            }
            slot = (slot + 1) & self.mask;
        }
    }

    fn grow(&mut self) {
        let cap = (self.mask + 1) * 2;
        self.mask = cap - 1;
        self.slots.fill(0);
        self.slots.resize(cap, 0);
        for (i, rec) in self.recs.iter().enumerate() {
            let mut slot = (rec.hash as usize) & self.mask;
            while slots_occupied(&self.slots, slot) {
                slot = (slot + 1) & self.mask;
            }
            self.slots[slot] = i as u32 + 1;
        }
    }
}

fn slots_occupied(slots: &[u32], slot: usize) -> bool {
    slots[slot] != 0
}

/// u64-chunk multiply hash (wyhash-flavored): short tokens hash in ~2
/// multiplies instead of FNV's one serial multiply per byte.
fn hash_bytes(bytes: &[u8]) -> u32 {
    let mut h = (bytes.len() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        let v = u64::from_le_bytes(c.try_into().unwrap());
        h = (h ^ v).wrapping_mul(0xA24B_AED4_963E_E407);
        h ^= h >> 29;
    }
    let mut tail = 0u64;
    for &b in chunks.remainder() {
        tail = (tail << 8) | b as u64;
    }
    h = (h ^ tail).wrapping_mul(0xA24B_AED4_963E_E407);
    (h ^ (h >> 32)) as u32
}

/// NumericDocValues buffer: one value per doc at most, doc IDs ascending
/// (Lucene's NumericDocValuesWriter constraint, NumericDocValuesWriter.java:51-57).
#[derive(Default)]
pub struct NumericDvBuf {
    pub docs: Vec<u32>,
    pub values: Vec<i64>,
}

impl NumericDvBuf {
    fn add(&mut self, doc: u32, value: i64) -> std::io::Result<()> {
        if self.docs.last() == Some(&doc) {
            return Err(err(format!(
                "DocValuesField appears more than once in document {doc} (only one value is allowed per field)"
            )));
        }
        self.docs.push(doc);
        self.values.push(value);
        Ok(())
    }
}

/// SortedDocValues buffer: value dictionary (insertion order) + per-doc term
/// ids; ords are remapped to sorted-term rank at flush time
/// (SortedDocValuesWriter.java:113-125).
#[derive(Default)]
pub struct SortedDvBuf {
    pub dict: TermDict,
    pub docs: Vec<u32>,
    pub term_ids: Vec<u32>,
}

impl SortedDvBuf {
    /// Returns the estimated RAM delta (for the writer's accounting).
    fn add(&mut self, doc: u32, value: &[u8]) -> std::io::Result<usize> {
        if self.docs.last() == Some(&doc) {
            return Err(err(format!(
                "DocValuesField appears more than once in document {doc} (only one value is allowed per field)"
            )));
        }
        if value.len() > 32766 {
            return Err(err(format!(
                "SortedDocValues value too long: {} bytes (max 32766)",
                value.len()
            )));
        }
        let (id, is_new) = self.dict.lookup_or_insert_flag(value);
        self.docs.push(doc);
        self.term_ids.push(id);
        Ok(8 + if is_new { TERM_RAM + value.len() } else { 0 })
    }
}

/// 1D points buffer: (value, doc_id), unsorted; int values are widened to i64
/// and narrowed back at flush time via the field's `PointSpec`.
#[derive(Default)]
pub struct PointsBuf {
    pub points: Vec<(i64, u32)>,
}

/// Binary DV buffer: (doc_id, bytes) pairs in doc order.
#[derive(Default)]
pub struct BinaryDvBuf {
    pub docs: Vec<u32>,
    pub values: Vec<Vec<u8>>,
}

/// Per-field indexing buffer. Which sub-buffers exist is driven by the spec:
/// an inverted index (`dict`) for indexed fields, doc values buffers, and a
/// points buffer — a field may combine several (Lucene same-name fields).
pub struct FieldBuf {
    pub spec: FieldSpec,
    pub dict: Option<TermDict>,
    /// docs with >= 1 indexed term of this field.
    pub doc_count: u32,
    pub numeric_dv: Option<NumericDvBuf>,
    pub sorted_dv: Option<SortedDvBuf>,
    pub binary_dv: Option<BinaryDvBuf>,
    pub points: Option<PointsBuf>,
}

impl FieldBuf {
    fn new(spec: FieldSpec) -> Self {
        let dict = spec.is_indexed().then(TermDict::new);
        let numeric_dv =
            (spec.doc_values == codec_lucene9::DocValuesType::Numeric).then(NumericDvBuf::default);
        let sorted_dv =
            (spec.doc_values == codec_lucene9::DocValuesType::Sorted).then(SortedDvBuf::default);
        let binary_dv =
            (spec.doc_values == codec_lucene9::DocValuesType::Binary).then(BinaryDvBuf::default);
        let points = spec.points.map(|_| PointsBuf::default());
        Self {
            spec,
            dict,
            doc_count: 0,
            numeric_dv,
            sorted_dv,
            binary_dv,
            points,
        }
    }

    fn needs_buffer(spec: &FieldSpec) -> bool {
        spec.is_indexed()
            || spec.doc_values != codec_lucene9::DocValuesType::None
            || spec.points.is_some()
    }
}

/// In-RAM indexing buffer for the segment currently being built.
pub struct DocWriter {
    /// all fields by number (field number == index; names are looked up
    /// linearly — schemas are tiny, this beats a hashed map).
    fields: Vec<FieldSpec>,
    /// field buffers, parallel to `fields` (None for stored-only fields).
    buffers: Vec<Option<FieldBuf>>,
    pub max_doc: u32,
    /// Rough RAM estimate of all buffers, updated incrementally per add
    /// (Lucene's RAM accounting is approximate too). Used by the writer's
    /// max_ram_bytes flush trigger.
    ram_bytes: usize,
}

impl Default for DocWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Estimated bytes of a newly inserted term (TermRec + arena bytes + hash
/// slot amortized + initial posting arrays).
const TERM_RAM: usize = 88;
/// Estimated bytes of a posting that starts a new doc.
const POSTING_NEW_DOC_RAM: usize = 24;
/// Estimated bytes of a repeat posting in the same doc.
const POSTING_SAME_DOC_RAM: usize = 8;

impl DocWriter {
    pub fn new() -> Self {
        Self {
            fields: Vec::new(),
            buffers: Vec::new(),
            max_doc: 0,
            ram_bytes: 0,
        }
    }

    /// Approximate RAM held by all indexing buffers.
    pub fn ram_bytes(&self) -> usize {
        self.ram_bytes
    }

    /// Consumes the document so owned values move into the stored-fields
    /// stream without an extra copy. Stored fields are serialized into `sfw`
    /// as they are consumed (streaming, Lucene's StoredFieldsConsumer model);
    /// pass `None` to drop stored values (tests).
    pub fn add_document(
        &mut self,
        schema: &Schema,
        doc: Document,
        mut sfw: Option<&mut StoredFieldsWriter>,
    ) -> std::io::Result<()> {
        self.sync_schema(schema);
        let doc_id = self.max_doc;
        if let Some(w) = sfw.as_deref_mut() {
            w.start_document();
        }
        for (name, value) in doc.fields {
            let spec = schema
                .get(&name)
                .ok_or_else(|| err(format!("unknown field: {name}")))?;
            let number = self.field_number(spec);
            match value {
                FieldValue::Text(text) => {
                    if spec.is_indexed() {
                        if !spec.tokenized {
                            return Err(err(format!(
                                "field {name}: keyword field expects FieldValue::Keyword"
                            )));
                        }
                        let buf = self.buffers[number as usize].as_mut().unwrap();
                        let dict = buf.dict.as_mut().unwrap();
                        let has_positions = buf.spec.has_positions();
                        let mut saw_term = false;
                        let mut position = 0u32;
                        for token in WhitespaceTokens::new(&text) {
                            let tok = token.as_bytes();
                            let (id, is_new) = dict.lookup_or_insert_flag(tok);
                            let new_doc = dict.recs[id as usize].postings.add_occurrence(
                                doc_id,
                                if has_positions { Some(position) } else { None },
                            );
                            self.ram_bytes += if is_new { TERM_RAM + tok.len() } else { 0 }
                                + if new_doc {
                                    POSTING_NEW_DOC_RAM
                                } else {
                                    POSTING_SAME_DOC_RAM
                                };
                            saw_term = true;
                            position += 1;
                        }
                        if saw_term {
                            buf.doc_count += 1;
                        }
                    }
                    if spec.stored {
                        if let Some(w) = sfw.as_deref_mut() {
                            w.write_field(number, &StoredField::String(text));
                        }
                    }
                }
                FieldValue::Keyword(kw) => {
                    if spec.is_indexed() {
                        if spec.tokenized {
                            return Err(err(format!(
                                "field {name}: tokenized field expects FieldValue::Text"
                            )));
                        }
                        let buf = self.buffers[number as usize].as_mut().unwrap();
                        let dict = buf.dict.as_mut().unwrap();
                        let (id, is_new) = dict.lookup_or_insert_flag(kw.as_bytes());
                        let new_doc = dict.recs[id as usize].postings.add_occurrence(doc_id, None);
                        self.ram_bytes += if is_new { TERM_RAM + kw.len() } else { 0 }
                            + if new_doc {
                                POSTING_NEW_DOC_RAM
                            } else {
                                POSTING_SAME_DOC_RAM
                            };
                        if new_doc {
                            buf.doc_count += 1;
                        }
                    }
                    if let Some(dv) = self.buffers[number as usize]
                        .as_mut()
                        .and_then(|b| b.sorted_dv.as_mut())
                    {
                        self.ram_bytes += dv.add(doc_id, kw.as_bytes())?;
                    }
                    if spec.stored {
                        if let Some(w) = sfw.as_deref_mut() {
                            w.write_field(number, &StoredField::String(kw));
                        }
                    }
                }
                FieldValue::Long(v) => {
                    self.add_numeric_value(number, &name, v, 8)?;
                    if spec.stored {
                        if let Some(w) = sfw.as_deref_mut() {
                            w.write_field(number, &StoredField::Long(v));
                        }
                    }
                }
                FieldValue::Int(v) => {
                    self.add_numeric_value(number, &name, v as i64, 4)?;
                    if spec.stored {
                        if let Some(w) = sfw.as_deref_mut() {
                            w.write_field(number, &StoredField::Int(v));
                        }
                    }
                }
                FieldValue::Bytes(b) => {
                    if let Some(buf) = self.buffers[number as usize].as_mut() {
                        if let Some(dv) = buf.binary_dv.as_mut() {
                            dv.docs.push(doc_id);
                            dv.values.push(b.clone());
                            self.ram_bytes += 12 + b.len();
                        }
                    }
                }
            }
        }
        if let Some(w) = sfw.as_deref_mut() {
            w.finish_document()?;
        }
        self.max_doc += 1;
        Ok(())
    }

    /// Shared path for Long/Int values: points buffer (width-checked against
    /// the field's PointSpec) and/or NumericDocValues buffer.
    fn add_numeric_value(
        &mut self,
        number: u32,
        name: &str,
        v: i64,
        width: u8,
    ) -> std::io::Result<()> {
        let buf = self.buffers[number as usize].as_mut();
        let Some(buf) = buf else {
            if self.fields[number as usize].points.is_some()
                || self.fields[number as usize].doc_values != codec_lucene9::DocValuesType::None
            {
                return Err(err(format!("field {name}: internal buffer missing")));
            }
            return Ok(());
        };
        if let Some(points) = buf.points.as_mut() {
            let expected = buf.spec.points.unwrap().bytes_per_dim;
            if expected != width {
                return Err(err(format!(
                    "field {name}: expected a {}-byte point value",
                    expected
                )));
            }
            points.points.push((v, self.max_doc));
            self.ram_bytes += 12;
        }
        if let Some(dv) = buf.numeric_dv.as_mut() {
            dv.add(self.max_doc, v)?;
            self.ram_bytes += 12;
        }
        Ok(())
    }

    /// The schema is append-only; keep the field table a prefix-copy of it so
    /// field numbers always equal schema indices (Lucene FieldInfos
    /// numbering), including fields registered mid-stream (dynamic JSON
    /// schema). Fields not yet seen in this segment get empty buffers; the
    /// segment builder omits index/point flags for fields with no data.
    fn sync_schema(&mut self, schema: &Schema) {
        while self.fields.len() < schema.fields().len() {
            let spec = schema.fields()[self.fields.len()].clone();
            self.fields.push(spec.clone());
            self.buffers
                .push(FieldBuf::needs_buffer(&spec).then(|| FieldBuf::new(spec)));
        }
    }

    fn field_number(&mut self, spec: &FieldSpec) -> u32 {
        if let Some(n) = self.fields.iter().position(|f| f.name == spec.name) {
            return n as u32;
        }
        let n = self.fields.len() as u32;
        self.fields.push(spec.clone());
        self.buffers
            .push(FieldBuf::needs_buffer(spec).then(|| FieldBuf::new(spec.clone())));
        n
    }

    /// Fields in field-number order.
    pub fn fields(&self) -> &[FieldSpec] {
        &self.fields
    }

    pub fn field_buffer(&self, number: u32) -> Option<&FieldBuf> {
        self.buffers[number as usize].as_ref()
    }

    pub fn field_buffer_mut(&mut self, number: u32) -> Option<&mut FieldBuf> {
        self.buffers[number as usize].as_mut()
    }

    /// Builds the per-oldDoc sort keys for `number` from its DocValues buffer
    /// (index sort phase A: the sort field must carry Numeric or Sorted DV).
    /// Sorted DV keys use the rank in the *sorted* dictionary so key ordering
    /// equals term-byte ordering (Lucene StringSorter compares ords). Docs
    /// without a value map to None (placed per the field's Missing policy).
    pub fn sort_keys(&self, number: u32) -> Option<Vec<Option<Key>>> {
        let buf = self.buffers[number as usize].as_ref()?;
        let mut keys: Vec<Option<Key>> = vec![None; self.max_doc as usize];
        if let Some(dv) = buf.numeric_dv.as_ref() {
            for (&d, &v) in dv.docs.iter().zip(dv.values.iter()) {
                keys[d as usize] = Some(Key::Num(v));
            }
            return Some(keys);
        }
        if let Some(dv) = buf.sorted_dv.as_ref() {
            // insertion-id -> sorted-rank (same remap finalize uses for ords)
            let sorted = dv.dict.sorted_ids();
            let mut remap = vec![0u32; dv.dict.len()];
            for (ord, &id) in sorted.iter().enumerate() {
                remap[id as usize] = ord as u32;
            }
            for (&d, &t) in dv.docs.iter().zip(dv.term_ids.iter()) {
                keys[d as usize] = Some(Key::Ord(remap[t as usize]));
            }
            return Some(keys);
        }
        None
    }

    /// Applies the index-sort permutation to every field buffer in place:
    /// each buffer's docIDs are mapped through `map.old_to_new` and re-sorted
    /// ascending (carrying freqs/positions/values/term_ids along), so the
    /// format writers — which all assume ascending docIDs — encode the
    /// physically reordered segment unchanged. Points only need the docID
    /// remap (the BKD writer sorts internally).
    pub fn apply_doc_map(&mut self, map: &DocMap) {
        for buf in self.buffers.iter_mut().flatten() {
            if let Some(dict) = buf.dict.as_mut() {
                for id in 0..dict.len() {
                    remap_posting(&mut dict.recs[id].postings, map);
                }
            }
            if let Some(dv) = buf.numeric_dv.as_mut() {
                let (docs, values) = remap_parallel(&dv.docs, std::mem::take(&mut dv.values), map);
                dv.docs = docs;
                dv.values = values;
            }
            if let Some(dv) = buf.sorted_dv.as_mut() {
                let (docs, term_ids) =
                    remap_parallel(&dv.docs, std::mem::take(&mut dv.term_ids), map);
                dv.docs = docs;
                dv.term_ids = term_ids;
            }
            if let Some(dv) = buf.binary_dv.as_mut() {
                let (docs, values) =
                    remap_parallel(&dv.docs, std::mem::take(&mut dv.values), map);
                dv.docs = docs;
                dv.values = values;
            }
            if let Some(pts) = buf.points.as_mut() {
                for p in pts.points.iter_mut() {
                    p.1 = map.old_to_new(p.1);
                }
            }
        }
    }
}

/// Remaps a PostingBuf through the permutation: docs go through old_to_new
/// and the (doc, freq, positions) triples are re-sorted ascending by new doc.
fn remap_posting(pb: &mut PostingBuf, map: &DocMap) {
    let has_pos = !pb.positions.is_empty();
    let n = pb.docs.len();
    let mut triples: Vec<(u32, u32, Vec<u32>)> = Vec::with_capacity(n);
    for i in 0..n {
        let new_doc = map.old_to_new(pb.docs[i]);
        let freq = pb.freqs[i];
        let pos = if has_pos {
            std::mem::take(&mut pb.positions[i])
        } else {
            Vec::new()
        };
        triples.push((new_doc, freq, pos));
    }
    triples.sort_by_key(|t| t.0);
    pb.docs = triples.iter().map(|t| t.0).collect();
    pb.freqs = triples.iter().map(|t| t.1).collect();
    if has_pos {
        pb.positions = triples.into_iter().map(|t| t.2).collect();
    }
}

/// Remaps a parallel (docs, values) pair through the permutation and re-sorts
/// ascending by new doc. Old docs are unique within a field buffer, so new
/// docs are too (no tie-breaking needed).
fn remap_parallel<T>(docs: &[u32], values: Vec<T>, map: &DocMap) -> (Vec<u32>, Vec<T>) {
    let mut pairs: Vec<(u32, T)> = docs
        .iter()
        .copied()
        .zip(values)
        .map(|(d, v)| (map.old_to_new(d), v))
        .collect();
    pairs.sort_by_key(|p| p.0);
    pairs.into_iter().unzip()
}

fn err(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_postings_with_freqs_and_positions() {
        let mut schema = Schema::new();
        schema.add(FieldSpec::text_with_positions("message"));
        let mut dw = DocWriter::new();
        let mut d0 = Document::new();
        d0.add("message", FieldValue::Text("b a a".to_string()));
        let mut d1 = Document::new();
        d1.add("message", FieldValue::Text("a".to_string()));
        dw.add_document(&schema, d0, None).unwrap();
        dw.add_document(&schema, d1, None).unwrap();

        let buf = dw.field_buffer(0).unwrap();
        assert_eq!(buf.doc_count, 2);
        let dict = buf.dict.as_ref().unwrap();
        let a = dict.postings(dict.find(b"a").unwrap());
        assert_eq!(a.docs, vec![0, 1]);
        assert_eq!(a.freqs, vec![2, 1]);
        assert_eq!(a.positions, vec![vec![1, 2], vec![0]]);
        let b = dict.postings(dict.find(b"b").unwrap());
        assert_eq!(b.docs, vec![0]);
        assert_eq!(b.freqs, vec![1]);
        assert!(dict.find(b"never-seen").is_none());
    }

    #[test]
    fn term_dict_grows_and_sorts() {
        let mut dict = TermDict::new();
        for round in 0..3 {
            for i in 0..3000u32 {
                let term = format!("term{:05}", (i * 7 + round * 13) % 3000);
                let id = dict.lookup_or_insert(term.as_bytes());
                dict.recs[id as usize].postings.add_occurrence(round, None);
            }
        }
        assert_eq!(dict.recs.len(), 3000);
        let ids = dict.sorted_ids();
        for w in ids.windows(2) {
            assert!(dict.bytes_of(w[0]) < dict.bytes_of(w[1]));
        }
        // every term was seen once per round => freq 3 on the last doc
        for i in 0..3000u32 {
            let term = format!("term{i:05}");
            let pb = dict.postings(dict.find(term.as_bytes()).unwrap());
            assert_eq!(pb.docs, vec![0, 1, 2]);
            assert_eq!(pb.freqs, vec![1, 1, 1]);
        }
    }

    #[test]
    fn keyword_indexes_whole_value() {
        let mut schema = Schema::new();
        schema.add(FieldSpec::keyword("level"));
        let mut dw = DocWriter::new();
        let mut d0 = Document::new();
        d0.add("level", FieldValue::Keyword("INFO A".to_string()));
        let mut d1 = Document::new();
        d1.add("level", FieldValue::Keyword("INFO A".to_string()));
        dw.add_document(&schema, d0, None).unwrap();
        dw.add_document(&schema, d1, None).unwrap();

        let buf = dw.field_buffer(0).unwrap();
        assert_eq!(buf.doc_count, 2);
        let dict = buf.dict.as_ref().unwrap();
        // not tokenized: the whole string is one term
        let pb = dict.postings(dict.find(b"INFO A").unwrap());
        assert_eq!(pb.docs, vec![0, 1]);
        assert!(dict.find(b"INFO").is_none());
    }

    #[test]
    fn schema_growth_syncs_field_numbers_to_schema_indices() {
        let mut schema = Schema::new();
        schema.add(FieldSpec::text("message"));
        let mut dw = DocWriter::new();
        for _ in 0..10 {
            let mut d = Document::new();
            d.add("message", FieldValue::Text("hello world".to_string()));
            dw.add_document(&schema, d, None).unwrap();
        }
        // dynamic registration mid-stream (JsonBinder Dynamic policy)
        schema.add(FieldSpec::keyword("level"));
        for i in 0..10u32 {
            let mut d = Document::new();
            d.add("message", FieldValue::Text("hello".to_string()));
            d.add("level", FieldValue::Keyword(format!("L{}", i % 2)));
            dw.add_document(&schema, d, None).unwrap();
        }
        // field numbers == schema indices, buffers exist for both
        assert_eq!(dw.fields()[0].name, "message");
        assert_eq!(dw.fields()[1].name, "level");
        let level = dw.field_buffer(1).unwrap();
        assert_eq!(level.doc_count, 10);
        let dict = level.dict.as_ref().unwrap();
        let l0 = dict.postings(dict.find(b"L0").unwrap());
        assert_eq!(l0.docs, vec![10, 12, 14, 16, 18]);
    }

    #[test]
    fn numeric_dv_rejects_second_value_in_same_doc() {
        let mut schema = Schema::new();
        schema.add(FieldSpec::numeric_dv("latency"));
        let mut dw = DocWriter::new();
        let mut d0 = Document::new();
        d0.add("latency", FieldValue::Long(7));
        d0.add("latency", FieldValue::Long(8));
        assert!(dw.add_document(&schema, d0, None).is_err());
    }

    #[test]
    fn points_and_numeric_dv_accumulate() {
        let mut schema = Schema::new();
        schema.add(FieldSpec::long_point("ts").with_numeric_dv());
        schema.add(FieldSpec::sorted_dv("level"));
        let mut dw = DocWriter::new();
        for i in 0..3u32 {
            let mut d = Document::new();
            d.add("ts", FieldValue::Long(1000 + i as i64));
            d.add(
                "level",
                FieldValue::Keyword(if i % 2 == 0 { "INFO" } else { "ERROR" }.into()),
            );
            dw.add_document(&schema, d, None).unwrap();
        }
        // missing ts/level in a 4th doc is allowed (sparse)
        dw.add_document(&schema, Document::new(), None).unwrap();

        let ts = dw.field_buffer(0).unwrap();
        assert_eq!(ts.points.as_ref().unwrap().points.len(), 3);
        assert_eq!(
            ts.numeric_dv.as_ref().unwrap().values,
            vec![1000, 1001, 1002]
        );
        let level = dw.field_buffer(1).unwrap();
        let dv = level.sorted_dv.as_ref().unwrap();
        assert_eq!(dv.docs, vec![0, 1, 2]);
        assert_eq!(dv.dict.len(), 2);
    }
}
