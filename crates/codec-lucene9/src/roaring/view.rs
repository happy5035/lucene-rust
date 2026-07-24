//! Zero-copy read view over a v2 inline term bitmap (M4 spec §4): the
//! query path never rebuilds container objects. Two open modes —
//!
//! - **probe** (`open_probe`, AND high-df side): scans only the container
//!   directory (key/type/card headers; data sections are seeked over,
//!   their length derived from type+card/numRuns). `contains(doc)` reads
//!   at most the one bucket that could hold the doc (bitset: a single u64
//!   word at `data_off + (low>>6)*8`).
//! - **full** (`open_full`, OR / Term iteration / AND small side): one
//!   sequential region read into a buffer, then byte-cursor iteration
//!   with the M3 `RoaringCursor` contract.
//!
//! Validation is the v2 triple gate (spec §3): len bound (done by the
//! caller, `locate_bitmap_region`) → magic+version → header df/card ==
//! doc_freq, plus cheap structural bounds (container keys ascending, data
//! sections inside the region, card sum == df). Payload integrity stays
//! with the .doc footer CRC. Any failure → `Ok(None)`, the silent
//! postings fallback. All safe code — no unsafe is added for this module.

use std::io;

use crate::io::{DataInput, IndexInput};

use super::{
    BITMAP_MAGIC, BITSET_BITS, BITSET_WORDS, SELF_BUILT_WIRE_VERSION, TYPE_ARRAY, TYPE_BITSET,
    TYPE_RUN,
};

/// One container-directory entry. `data_off`/`data_len` are relative to
/// the region start (the bitmap's first byte), so they index both the
/// probe stream (region_start + off) and the full-mode buffer.
#[derive(Clone, Copy, Debug)]
struct ContainerMeta {
    key: u16,
    ty: u8,
    card: u32,
    data_off: u64,
    data_len: u64,
}

/// Parsed + validated bitmap view (M4 §4). Owns either the positioned
/// .doc stream (probe) or the region bytes (full). Iterators must box it —
/// an IndexInput carries an 8KB inline buffer (io.rs:597, 关键设计事实 6).
pub enum RoaringView {
    Probe(ProbeView),
    Full(FullView),
}

pub struct ProbeView {
    input: IndexInput, // independently positioned .doc stream (fresh_input)
    region_start: u64,
    dir: Vec<ContainerMeta>,
    card: u64,
    scratch: Vec<u8>, // per-probe bucket data (array/run), reused
}

pub struct FullView {
    buf: Vec<u8>, // the whole region, one sequential read
    dir: Vec<ContainerMeta>,
    card: u64,
}

/// LE u16 at index `i` of a byte image (array element i / run-pair half i).
fn u16_at(data: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([data[2 * i], data[2 * i + 1]])
}

/// partition_point over an ascending LE u16 image: first index whose
/// value >= `low`.
fn partition_point_u16(data: &[u8], n: usize, low: u16) -> usize {
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = (lo + hi) / 2;
        if u16_at(data, mid) < low {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// First run index whose end >= `low` (runs are ascending, non-overlapping).
fn partition_point_run_end(data: &[u8], runs: usize, low: u16) -> usize {
    let mut lo = 0usize;
    let mut hi = runs;
    while lo < hi {
        let mid = (lo + hi) / 2;
        if u16_at(data, 2 * mid + 1) < low {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// First set bit at position >= `from` in a 65536-bit LE byte image
/// (mirrors `next_set_bit` over `[u64; BITSET_WORDS]`, roaring.rs:104).
fn next_set_bit_in(data: &[u8], from: u32) -> Option<u32> {
    if from >= BITSET_BITS as u32 {
        return None;
    }
    let mut wi = from as usize >> 6;
    let mut word = u64::from_le_bytes(data[wi * 8..wi * 8 + 8].try_into().unwrap())
        & (u64::MAX << (from & 63));
    loop {
        if word != 0 {
            return Some((wi * 64 + word.trailing_zeros() as usize) as u32);
        }
        wi += 1;
        if wi == BITSET_WORDS {
            return None;
        }
        word = u64::from_le_bytes(data[wi * 8..wi * 8 + 8].try_into().unwrap());
    }
}

/// Membership test inside one container's data image (array: card×u16 LE;
/// bitset: 1024×u64 LE; run: numRuns×(start,end) u16 LE pairs).
fn contains_in(data: &[u8], ty: u8, card: u32, low: u16) -> bool {
    match ty {
        TYPE_ARRAY => {
            let n = card as usize;
            let p = partition_point_u16(data, n, low);
            p < n && u16_at(data, p) == low
        }
        TYPE_BITSET => {
            let w = (low >> 6) as usize * 8;
            let word = u64::from_le_bytes(data[w..w + 8].try_into().unwrap());
            word >> (low & 63) & 1 == 1
        }
        TYPE_RUN => {
            let runs = data.len() / 4;
            let p = partition_point_run_end(data, runs, low);
            p < runs && u16_at(data, 2 * p) <= low
        }
        _ => false, // unreachable: the directory scan rejected unknown types
    }
}

impl RoaringView {
    /// Shared container-directory scan (M4 §4): validates the v2 header
    /// (magic, version == 2 — the only migration gate, df == expected_df,
    /// card == df) and walks the per-container headers, deriving each
    /// data section's [data_off, data_off+data_len) WITHOUT reading it
    /// (array: 2·card B; bitset: 8192 B; run: the numRuns vInt is part of
    /// the header walk, then 4·numRuns B — 关键设计事实 7). None on any
    /// deviation (the silent-fallback signal).
    fn scan_directory(
        input: &mut IndexInput,
        start: u64,
        len: u32,
        expected_df: u32,
    ) -> io::Result<Option<(u64, Vec<ContainerMeta>)>> {
        input.seek(start)?;
        let mut magic = [0u8; 4];
        input.read_bytes(&mut magic)?;
        if magic != BITMAP_MAGIC {
            return Ok(None);
        }
        if input.read_byte()? != SELF_BUILT_WIRE_VERSION {
            return Ok(None); // v1 (or anything else): version is the migration gate
        }
        if input.read_vint()? as u32 != expected_df {
            return Ok(None);
        }
        let card = input.read_vint()? as u32;
        if card != expected_df {
            return Ok(None); // docs-only bitmap: cardinality == df
        }
        let num_containers = input.read_vint()?;
        if !(1..=65536).contains(&num_containers) {
            return Ok(None);
        }
        let mut dir = Vec::with_capacity(num_containers as usize);
        let mut card_sum = 0u64;
        let mut last_key: Option<u16> = None;
        for _ in 0..num_containers {
            let key = input.read_short()? as u16;
            if let Some(lk) = last_key
                && key <= lk
            {
                return Ok(None); // unsorted / duplicate container keys
            }
            last_key = Some(key);
            let ty = input.read_byte()?;
            let c = input.read_vint()?;
            if c < 1 || c > 65536 {
                return Ok(None); // empty container, or beyond the 2^16 domain
            }
            let card = c as u32;
            let data_len = match ty {
                TYPE_ARRAY => 2 * card as u64,
                TYPE_BITSET => 8192,
                TYPE_RUN => {
                    let num_runs = input.read_vint()?;
                    // at most 2^15 disjoint runs over a 2^16-value domain
                    if !(1..=32768).contains(&num_runs) {
                        return Ok(None);
                    }
                    4 * num_runs as u64
                }
                _ => return Ok(None),
            };
            let data_off = input.file_pointer() - start;
            if data_off + data_len > len as u64 {
                return Ok(None); // data section escapes the region: not ours
            }
            card_sum += card as u64;
            dir.push(ContainerMeta {
                key,
                ty,
                card,
                data_off,
                data_len,
            });
            input.seek(start + data_off + data_len)?; // skip the data section
        }
        if card_sum != card as u64 {
            return Ok(None);
        }
        if input.file_pointer() != start + len as u64 {
            return Ok(None); // trailing bytes: not one of our bitmaps
        }
        Ok(Some((card_sum, dir)))
    }

    /// Probe-mode open (M4 §4, AND high-df side): container-directory
    /// scan only — data sections are never read until `contains` needs
    /// one bucket. `input` is an independently positioned .doc stream
    /// (fresh_input); [start, start+len) is the located bitmap region.
    pub fn open_probe(
        mut input: IndexInput,
        start: u64,
        len: u32,
        expected_df: u32,
    ) -> io::Result<Option<RoaringView>> {
        let Some((card, dir)) = Self::scan_directory(&mut input, start, len, expected_df)? else {
            return Ok(None);
        };
        Ok(Some(RoaringView::Probe(ProbeView {
            input,
            region_start: start,
            dir,
            card,
            scratch: Vec::new(),
        })))
    }

    /// Full-mode open (M4 §4, OR / Term iteration / AND small side): the
    /// validated region is read once, sequentially, into memory — no
    /// per-element validation.
    pub fn open_full(
        mut input: IndexInput,
        start: u64,
        len: u32,
        expected_df: u32,
    ) -> io::Result<Option<RoaringView>> {
        let Some((card, dir)) = Self::scan_directory(&mut input, start, len, expected_df)? else {
            return Ok(None);
        };
        input.seek(start)?;
        let mut buf = vec![0u8; len as usize];
        input.read_bytes(&mut buf)?;
        Ok(Some(RoaringView::Full(FullView { buf, dir, card })))
    }

    /// Total docs (== df of the term, pinned by the v2 header checks).
    pub fn cardinality(&self) -> u64 {
        match self {
            RoaringView::Probe(p) => p.card,
            RoaringView::Full(f) => f.card,
        }
    }

    /// Membership test. Probe mode reads at most the target bucket's data
    /// (bitset: one 8B word at data_off + (low>>6)×8, spec §4); full mode
    /// is a pure in-memory test.
    pub fn contains(&mut self, doc: u32) -> io::Result<bool> {
        let (key, low) = ((doc >> 16) as u16, doc as u16);
        match self {
            RoaringView::Probe(p) => {
                let ci = p.dir.partition_point(|m| m.key < key);
                let Some(m) = p.dir.get(ci).copied() else {
                    return Ok(false);
                };
                if m.key != key {
                    return Ok(false);
                }
                if m.ty == TYPE_BITSET {
                    p.input
                        .seek(p.region_start + m.data_off + (low >> 6) as u64 * 8)?;
                    let word = p.input.read_long()? as u64;
                    return Ok(word >> (low & 63) & 1 == 1);
                }
                p.scratch.clear();
                p.scratch.resize(m.data_len as usize, 0);
                p.input.seek(p.region_start + m.data_off)?;
                p.input.read_bytes(&mut p.scratch)?;
                Ok(contains_in(&p.scratch, m.ty, m.card, low))
            }
            RoaringView::Full(f) => {
                let ci = f.dir.partition_point(|m| m.key < key);
                let Some(m) = f.dir.get(ci) else {
                    return Ok(false);
                };
                if m.key != key {
                    return Ok(false);
                }
                let data = &f.buf[m.data_off as usize..(m.data_off + m.data_len) as usize];
                Ok(contains_in(data, m.ty, m.card, low))
            }
        }
    }
}

/// Byte-image iteration cursor with the M3 `RoaringCursor` contract
/// (roaring.rs:608-617, 关键设计事实 8): plain data, forward-only;
/// `cursor_advance` targets must exceed the last returned doc (the
/// DocIter advance contract). State per container type: array → a = next
/// element index; bitset → a = next bit to check; run → a = run index,
/// b = in-run offset of the next value.
#[derive(Clone, Copy, Default)]
pub struct ViewCursor {
    ci: u32,
    a: u32,
    b: u32,
}

impl RoaringView {
    /// Fresh cursor. Full-mode views only (probe views have no byte image
    /// in memory); the mode is debug-asserted.
    pub fn cursor(&self) -> ViewCursor {
        debug_assert!(
            matches!(self, RoaringView::Full(_)),
            "cursor() requires open_full"
        );
        ViewCursor::default()
    }

    /// Next doc at/after the cursor position, or None when exhausted.
    /// Full-mode only: probe views return None (contract: never called).
    pub fn cursor_next(&self, cur: &mut ViewCursor) -> Option<u32> {
        let RoaringView::Full(f) = self else {
            return None;
        };
        loop {
            let m = f.dir.get(cur.ci as usize)?;
            let data = &f.buf[m.data_off as usize..(m.data_off + m.data_len) as usize];
            match m.ty {
                TYPE_ARRAY => {
                    let i = cur.a as usize;
                    if i < m.card as usize {
                        cur.a += 1;
                        return Some(((m.key as u32) << 16) | u16_at(data, i) as u32);
                    }
                }
                TYPE_BITSET => {
                    if let Some(bit) = next_set_bit_in(data, cur.a) {
                        cur.a = bit + 1;
                        return Some(((m.key as u32) << 16) | bit);
                    }
                }
                TYPE_RUN => {
                    let runs = m.data_len as usize / 4;
                    let r = cur.a as usize;
                    if r < runs {
                        let (s, e) = (u16_at(data, 2 * r), u16_at(data, 2 * r + 1));
                        let v = s as u32 + cur.b;
                        if v < e as u32 {
                            cur.b += 1;
                        } else {
                            cur.a += 1;
                            cur.b = 0;
                        }
                        return Some(((m.key as u32) << 16) | v);
                    }
                }
                _ => return None, // unreachable: scan rejected unknown types
            }
            cur.ci += 1;
            cur.a = 0;
            cur.b = 0;
        }
    }

    /// First doc >= target; the cursor ends positioned past it. Forward
    /// only (same contract as `RoaringBitmap::cursor_advance`).
    pub fn cursor_advance(&self, cur: &mut ViewCursor, target: u32) -> Option<u32> {
        let RoaringView::Full(f) = self else {
            return None;
        };
        let key = (target >> 16) as u16;
        let low = target as u16;
        let ci = f.dir.partition_point(|m| m.key < key);
        if ci > cur.ci as usize {
            cur.ci = ci as u32;
            cur.a = 0;
            cur.b = 0;
        }
        let m = f.dir.get(cur.ci as usize)?;
        if m.key > key {
            // target's bucket absent: first value of the current container
            cur.a = 0;
            cur.b = 0;
            return self.cursor_next(cur);
        }
        let data = &f.buf[m.data_off as usize..(m.data_off + m.data_len) as usize];
        match m.ty {
            TYPE_ARRAY => {
                let p = partition_point_u16(data, m.card as usize, low);
                cur.a = cur.a.max(p as u32);
            }
            TYPE_BITSET => {
                cur.a = cur.a.max(low as u32);
            }
            TYPE_RUN => {
                let runs = m.data_len as usize / 4;
                let p = partition_point_run_end(data, runs, low);
                if p as u32 > cur.a {
                    // target lands in a later run: the partially consumed
                    // run's in-run offset must not leak into the new run
                    cur.a = p as u32;
                    cur.b = 0;
                }
                if (cur.a as usize) < runs {
                    let s = u16_at(data, 2 * cur.a as usize);
                    cur.b = cur.b.max(low.saturating_sub(s) as u32);
                }
            }
            _ => return None, // unreachable
        }
        self.cursor_next(cur)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::serialize_v1_for_test;
    use super::*;
    use crate::roaring::RoaringBitmap;

    /// Region bytes for a doc set (v2 image, no len suffix).
    fn region(docs: &[u32]) -> Vec<u8> {
        RoaringBitmap::from_sorted_docs(docs).serialize(docs.len() as u32)
    }

    fn open_full(bytes: &[u8], df: u32) -> Option<RoaringView> {
        RoaringView::open_full(
            IndexInput::in_memory(bytes.to_vec()),
            0,
            bytes.len() as u32,
            df,
        )
        .unwrap()
    }

    fn open_probe(bytes: &[u8], df: u32) -> Option<RoaringView> {
        RoaringView::open_probe(
            IndexInput::in_memory(bytes.to_vec()),
            0,
            bytes.len() as u32,
            df,
        )
        .unwrap()
    }

    /// One fixture per container type + a mixed multi-bucket one
    /// (array: 100 values step 3; bitset: 6000 values step 10 — scattered,
    /// 4*runs > 8192 so runOptimize keeps the bitset; run: 5000
    /// consecutive).
    fn fixtures() -> Vec<Vec<u32>> {
        vec![
            (0..100u32).map(|i| i * 3).collect(),
            (0..6000u32).map(|i| i * 10).collect(),
            (0..5000u32).collect(),
            {
                let mut v: Vec<u32> = (0..100u32).map(|i| i * 3).collect();
                v.extend(65_536..70_536u32); // bucket 1: run
                v.extend((0..6000u32).map(|i| 2 * 65536 + i * 10)); // bucket 2: bitset
                v
            },
        ]
    }

    #[test]
    fn full_view_cursor_matches_docs() {
        for docs in fixtures() {
            let bytes = region(&docs);
            let v = open_full(&bytes, docs.len() as u32).expect("must open");
            assert_eq!(v.cardinality(), docs.len() as u64);
            let mut cur = v.cursor();
            let mut out = Vec::new();
            while let Some(d) = v.cursor_next(&mut cur) {
                out.push(d);
            }
            assert_eq!(out, docs);
        }
    }

    #[test]
    fn full_view_advance_matches_linear_scan() {
        for docs in fixtures() {
            let bytes = region(&docs);
            let v = open_full(&bytes, docs.len() as u32).unwrap();
            // fresh-cursor advance per sampled target
            for t in (0..=*docs.last().unwrap() + 1).step_by(61) {
                let want = docs.iter().find(|&&d| d >= t).copied();
                let mut cur = v.cursor();
                assert_eq!(v.cursor_advance(&mut cur, t), want, "target {t}");
            }
            // interleaved next/advance on one cursor (forward-only:
            // targets are the remaining docs themselves, always > last)
            let mut cur = v.cursor();
            let mut idx = 0usize;
            while idx < docs.len() {
                assert_eq!(v.cursor_next(&mut cur), Some(docs[idx]));
                idx += 1;
                if idx < docs.len() {
                    assert_eq!(v.cursor_advance(&mut cur, docs[idx]), Some(docs[idx]));
                    idx += 1;
                }
            }
            assert_eq!(v.cursor_next(&mut cur), None);
            // advance past the end -> None, sticky
            let mut cur = v.cursor();
            assert_eq!(v.cursor_advance(&mut cur, u32::MAX), None);
            assert_eq!(v.cursor_next(&mut cur), None);
        }
    }

    #[test]
    fn probe_contains_matches_full_contains() {
        for docs in fixtures() {
            let bytes = region(&docs);
            let df = docs.len() as u32;
            let mut probe = open_probe(&bytes, df).expect("probe opens");
            let mut full = open_full(&bytes, df).expect("full opens");
            let space = *docs.last().unwrap() + 2;
            let samples = (0..space)
                .step_by(97)
                .chain(docs.iter().copied())
                .chain(docs.iter().map(|d| d + 1));
            for d in samples {
                let want = docs.binary_search(&d).is_ok();
                assert_eq!(probe.contains(d).unwrap(), want, "probe {d}");
                assert_eq!(full.contains(d).unwrap(), want, "full {d}");
            }
        }
    }

    #[test]
    fn open_rejects_v1_wrong_df_garbage_and_truncation() {
        let docs: Vec<u32> = (0..5000).collect();
        let bytes = region(&docs);
        // v1 layout (valid v1 crc) -> rejected at the version gate
        let v1 = serialize_v1_for_test(&RoaringBitmap::from_sorted_docs(&docs), 5000);
        assert!(open_full(&v1, 5000).is_none());
        assert!(open_probe(&v1, 5000).is_none());
        // wrong expected df -> None
        assert!(open_full(&bytes, 5001).is_none());
        assert!(open_probe(&bytes, 5001).is_none());
        // garbage -> None
        assert!(open_full(&[0xAA; 64], 5000).is_none());
        assert!(open_probe(&[0xAA; 64], 5000).is_none());
        // truncated (data section escapes the region) -> None
        let cut = &bytes[..bytes.len() - 1];
        assert!(open_full(cut, 5000).is_none());
        assert!(open_probe(cut, 5000).is_none());
        // probe of an absent bucket key -> false (no panic, no read)
        let mut p = open_probe(&bytes, 5000).unwrap();
        assert!(!p.contains(9 << 16).unwrap());
    }
}
