//! Inline per-term bitmap in the .doc stream, format v3 (M5 spec §3):
//! `[ magic "RLBM" + version=3 + df(vInt) + cardinality(vInt) + Frozen
//! payload ][ len: u32 LE ]`, written ahead of a term's postings. The
//! engine is croaring (CRoaring 4.7.1 via croaring-sys): the write side
//! builds `Bitmap::of` → `run_optimize` → `shrink_to_fit` → Frozen
//! serialize; the read side opens zero-copy frozen views over a 32B-aligned
//! buffer copy (`roaring/frozen.rs`, the crate's third module-level
//! `#[allow(unsafe_code)]`). The self-built container library below
//! (Container/RoaringBitmap/RoaringCursor, wire v2 pinned at
//! SELF_BUILT_WIRE_VERSION) is transitional — it only serves its own
//! tests until T4 deletes it together with roaring/simd.rs and
//! roaring/view.rs (M5 §2 删除, 关键设计事实 16).

use std::io;

use crate::io::{DataInput, DataOutput, IndexInput, IndexOutput};

mod simd;
pub mod view;

pub use view::{RoaringView, ViewCursor};

/// Bitmap-source read gate (spec §5): only terms with df >= this threshold
/// attempt the inline-bitmap path.
pub const BITMAP_MIN_DF: u32 = 4096;
/// Cardinality below which a container stays/becomes an array
/// (Roaring's ARRAY_DEFAULT_MAX_SIZE).
pub const ARRAY_THRESHOLD: usize = 4096;
/// Bits (and u64 words) per container.
const BITSET_BITS: usize = 1 << 16;
const BITSET_WORDS: usize = BITSET_BITS / 64;

/// One container: the low-16-bit values of one high-16-bit bucket.
pub enum Container {
    /// Sorted values; len < ARRAY_THRESHOLD after optimization.
    Array(Vec<u16>),
    /// 65536 bits + cached cardinality.
    Bitset(Box<[u64; BITSET_WORDS]>, u32),
    /// Inclusive [start, end] ranges, sorted and non-overlapping, + cached card.
    Run(Vec<(u16, u16)>, u32),
}

impl Container {
    fn card(&self) -> u32 {
        match self {
            Container::Array(v) => v.len() as u32,
            Container::Bitset(_, c) => *c,
            Container::Run(_, c) => *c,
        }
    }

    /// runOptimize: convert to a run container when 4B/run strictly beats
    /// the current representation; a bitset with few values degrades to an
    /// array (Roaring's container-size invariants, spec §4 容器语义).
    fn optimize(self) -> Container {
        match self {
            Container::Array(v) => {
                let runs = array_run_count(&v) as usize;
                if 4 * runs < 2 * v.len() {
                    let card = v.len() as u32;
                    Container::Run(array_to_runs(&v), card)
                } else {
                    Container::Array(v)
                }
            }
            Container::Bitset(w, card) => {
                if (card as usize) < ARRAY_THRESHOLD {
                    return Container::Array(bitset_to_array(&w, card)).optimize();
                }
                let runs = bitset_run_count(&w) as usize;
                if 4 * runs < 8 * BITSET_WORDS {
                    Container::Run(bitset_to_runs(&w), card)
                } else {
                    Container::Bitset(w, card)
                }
            }
            Container::Run(runs, card) => {
                if (card as usize) < ARRAY_THRESHOLD && (card as usize) < 2 * runs.len() {
                    let mut v = Vec::with_capacity(card as usize);
                    for &(s, e) in &runs {
                        v.extend(s..=e);
                    }
                    Container::Array(v)
                } else {
                    Container::Run(runs, card)
                }
            }
        }
    }
}

/// Sets bits [s, e] (inclusive) in the word image.
fn set_range(w: &mut [u64; BITSET_WORDS], s: u16, e: u16) {
    let (lo_w, hi_w) = (s as usize >> 6, e as usize >> 6);
    let lo_bit = s as usize & 63;
    let hi_bit = e as usize & 63;
    if lo_w == hi_w {
        w[lo_w] |= (u64::MAX << lo_bit) & (u64::MAX >> (63 - hi_bit));
        return;
    }
    w[lo_w] |= u64::MAX << lo_bit;
    for word in w.iter_mut().take(hi_w).skip(lo_w + 1) {
        *word = u64::MAX;
    }
    w[hi_w] |= u64::MAX >> (63 - hi_bit);
}

fn bit(w: &[u64; BITSET_WORDS], v: u16) -> bool {
    w[v as usize >> 6] >> (v & 63) & 1 == 1
}

/// First set bit at position >= `from` in the 65536-bit image.
fn next_set_bit(w: &[u64; BITSET_WORDS], from: u32) -> Option<u32> {
    if from >= BITSET_BITS as u32 {
        return None;
    }
    let mut wi = from as usize >> 6;
    let mut word = w[wi] & (u64::MAX << (from & 63));
    loop {
        if word != 0 {
            return Some((wi * 64 + word.trailing_zeros() as usize) as u32);
        }
        wi += 1;
        if wi == BITSET_WORDS {
            return None;
        }
        word = w[wi];
    }
}

fn bitset_to_array(w: &[u64; BITSET_WORDS], card: u32) -> Vec<u16> {
    let mut v = Vec::with_capacity(card as usize);
    for (i, &word) in w.iter().enumerate() {
        let mut word = word;
        while word != 0 {
            let b = word.trailing_zeros() as usize;
            v.push((i * 64 + b) as u16);
            word &= word - 1;
        }
    }
    v
}

/// Number of maximal consecutive runs in an ascending array.
fn array_run_count(v: &[u16]) -> u32 {
    let mut n = 1u32;
    for i in 1..v.len() {
        if v[i] as u32 != v[i - 1] as u32 + 1 {
            n += 1;
        }
    }
    n
}

/// Number of maximal consecutive runs in the 65536-bit image (0→1
/// transitions across word boundaries).
fn bitset_run_count(w: &[u64; BITSET_WORDS]) -> u32 {
    let mut runs = 0u32;
    let mut prev_top = 0u64;
    for &word in w {
        if word != 0 {
            // run starts inside this word (bit i set, bit i-1 clear, bit -1 := 0)
            runs += (word & !(word << 1)).count_ones();
            if word & 1 == 1 && prev_top == 1 {
                runs -= 1; // continuation of the previous word's last run
            }
            prev_top = word >> 63;
        } else {
            prev_top = 0;
        }
    }
    runs
}

fn array_to_runs(v: &[u16]) -> Vec<(u16, u16)> {
    let mut runs: Vec<(u16, u16)> = Vec::new();
    for &x in v {
        match runs.last_mut() {
            Some((_, e)) if x as u32 == *e as u32 + 1 => *e = x,
            _ => runs.push((x, x)),
        }
    }
    runs
}

fn bitset_to_runs(w: &[u64; BITSET_WORDS]) -> Vec<(u16, u16)> {
    let mut runs = Vec::new();
    let mut start: Option<u16> = None;
    for b in 0..BITSET_BITS as u32 {
        let set = w[b as usize >> 6] >> (b & 63) & 1 == 1;
        match (start, set) {
            (None, true) => start = Some(b as u16),
            (Some(s), false) => {
                runs.push((s, (b - 1) as u16));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, u16::MAX));
    }
    runs
}

// ------------------------------------------------------------------
// container boolean ops (spec §5: array∩array galloping,
// bitset∩bitset AVX2 + popcount, run∩run 双指针; Or 对偶)
// ------------------------------------------------------------------

/// Intersection of two sorted arrays: galloping from the smaller into the
/// larger when sizes are skewed, linear merge otherwise.
fn array_intersect(x: &[u16], y: &[u16]) -> Vec<u16> {
    if x.len() * 32 <= y.len() {
        return galloping_intersect(x, y);
    }
    if y.len() * 32 <= x.len() {
        return galloping_intersect(y, x);
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < x.len() && j < y.len() {
        match x[i].cmp(&y[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(x[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// Every value of `small` looked up in `large` by exponential + binary
/// search (galloping).
fn galloping_intersect(small: &[u16], large: &[u16]) -> Vec<u16> {
    let mut out = Vec::new();
    let mut lo = 0usize;
    for &v in small {
        // exponential search for the window of `large` that may hold v
        let mut bound = 1usize;
        while lo + bound <= large.len() && large[lo + bound - 1] < v {
            bound <<= 1;
        }
        let hi = (lo + bound).min(large.len());
        let idx = lo + large[lo..hi].partition_point(|&x| x < v);
        if idx == large.len() {
            break;
        }
        if large[idx] == v {
            out.push(v);
        }
        lo = idx;
    }
    out
}

/// Values of `x` covered by the runs (both ascending).
fn array_run_intersect(x: &[u16], runs: &[(u16, u16)]) -> Vec<u16> {
    let mut out = Vec::new();
    let mut ri = 0usize;
    for &v in x {
        while ri < runs.len() && runs[ri].1 < v {
            ri += 1;
        }
        if ri == runs.len() {
            break;
        }
        if runs[ri].0 <= v {
            out.push(v);
        }
    }
    out
}

/// Scalar reference for bitset ∩ bitset (the semantic definition the AVX2
/// kernel is pinned against, spec §6).
fn bitset_and_scalar(
    x: &[u64; BITSET_WORDS],
    y: &[u64; BITSET_WORDS],
    out: &mut [u64; BITSET_WORDS],
) -> u32 {
    let mut card = 0u32;
    for i in 0..BITSET_WORDS {
        let w = x[i] & y[i];
        out[i] = w;
        card += w.count_ones();
    }
    card
}

/// Scalar reference for bitset ∪ bitset.
fn bitset_or_scalar(
    x: &[u64; BITSET_WORDS],
    y: &[u64; BITSET_WORDS],
    out: &mut [u64; BITSET_WORDS],
) -> u32 {
    let mut card = 0u32;
    for i in 0..BITSET_WORDS {
        let w = x[i] | y[i];
        out[i] = w;
        card += w.count_ones();
    }
    card
}

/// Bitset ∩ run: copies the run ranges' bits out of the bitset.
fn bitset_run_intersect(w: &[u64; BITSET_WORDS], runs: &[(u16, u16)]) -> Container {
    let mut out = Box::new([0u64; BITSET_WORDS]);
    let mut card = 0u32;
    for &(s, e) in runs {
        let (lo_w, hi_w) = (s as usize >> 6, e as usize >> 6);
        for wi in lo_w..=hi_w {
            let lo_bit = if wi == lo_w { s as usize & 63 } else { 0 };
            let hi_bit = if wi == hi_w { e as usize & 63 } else { 63 };
            let mask = (u64::MAX << lo_bit) & (u64::MAX >> (63 - hi_bit));
            let v = w[wi] & mask;
            out[wi] = v;
            card += v.count_ones();
        }
    }
    Container::Bitset(out, card).optimize()
}

/// Interval intersection, two pointers (spec §5: run∩run 双指针).
fn run_run_intersect(x: &[(u16, u16)], y: &[(u16, u16)]) -> Container {
    let mut out: Vec<(u16, u16)> = Vec::new();
    let mut card = 0u32;
    let (mut i, mut j) = (0usize, 0usize);
    while i < x.len() && j < y.len() {
        let s = x[i].0.max(y[j].0);
        let e = x[i].1.min(y[j].1);
        if s <= e {
            out.push((s, e));
            card += e as u32 - s as u32 + 1;
        }
        if x[i].1 < y[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    Container::Run(out, card).optimize()
}

fn container_and(a: &Container, b: &Container) -> Container {
    match (a, b) {
        (Container::Array(x), Container::Array(y)) => {
            Container::Array(array_intersect(x, y)).optimize()
        }
        (Container::Array(x), Container::Bitset(w, _)) => {
            Container::Array(x.iter().copied().filter(|&v| bit(w, v)).collect()).optimize()
        }
        (Container::Bitset(..), Container::Array(_)) => container_and(b, a),
        (Container::Array(x), Container::Run(runs, _)) => {
            Container::Array(array_run_intersect(x, runs)).optimize()
        }
        (Container::Run(_, _), Container::Array(_)) => container_and(b, a),
        (Container::Bitset(x, _), Container::Bitset(y, _)) => {
            let mut out = Box::new([0u64; BITSET_WORDS]);
            let card = match simd::try_bitset_and(x, y, &mut out) {
                Some(c) => c,
                None => bitset_and_scalar(x, y, &mut out),
            };
            Container::Bitset(out, card).optimize()
        }
        (Container::Bitset(w, _), Container::Run(runs, _)) => bitset_run_intersect(w, runs),
        (Container::Run(_, _), Container::Bitset(_, _)) => container_and(b, a),
        (Container::Run(x, _), Container::Run(y, _)) => run_run_intersect(x, y),
    }
}

/// Union of two sorted arrays.
fn array_union(x: &[u16], y: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(x.len() + y.len());
    let (mut i, mut j) = (0, 0);
    while i < x.len() && j < y.len() {
        if x[i] < y[j] {
            out.push(x[i]);
            i += 1;
        } else if x[i] > y[j] {
            out.push(y[j]);
            j += 1;
        } else {
            out.push(x[i]);
            i += 1;
            j += 1;
        }
    }
    out.extend_from_slice(&x[i..]);
    out.extend_from_slice(&y[j..]);
    out
}

fn bitset_card(w: &[u64; BITSET_WORDS]) -> u32 {
    w.iter().map(|word| word.count_ones()).sum()
}

/// Interval union with coalescing of adjacent ranges.
fn run_run_union(x: &[(u16, u16)], y: &[(u16, u16)]) -> Container {
    let mut out: Vec<(u16, u16)> = Vec::with_capacity(x.len() + y.len());
    let mut card = 0u32;
    let (mut i, mut j) = (0usize, 0usize);
    while i < x.len() || j < y.len() {
        let iv = if j == y.len() || (i < x.len() && x[i].0 <= y[j].0) {
            let v = x[i];
            i += 1;
            v
        } else {
            let v = y[j];
            j += 1;
            v
        };
        match out.last_mut() {
            Some((_, e)) if iv.0 as u32 <= *e as u32 + 1 => {
                if iv.1 > *e {
                    card += iv.1 as u32 - *e as u32;
                    *e = iv.1;
                }
            }
            _ => {
                card += iv.1 as u32 - iv.0 as u32 + 1;
                out.push(iv);
            }
        }
    }
    Container::Run(out, card).optimize()
}

fn container_or(a: &Container, b: &Container) -> Container {
    match (a, b) {
        (Container::Array(x), Container::Array(y)) => {
            let merged = array_union(x, y);
            if merged.len() >= ARRAY_THRESHOLD {
                let mut w = Box::new([0u64; BITSET_WORDS]);
                for &v in &merged {
                    w[v as usize >> 6] |= 1u64 << (v & 63);
                }
                Container::Bitset(w, merged.len() as u32).optimize()
            } else {
                Container::Array(merged).optimize()
            }
        }
        (Container::Array(x), Container::Bitset(w, _))
        | (Container::Bitset(w, _), Container::Array(x)) => {
            let mut out = w.clone();
            for &v in x {
                out[v as usize >> 6] |= 1u64 << (v & 63);
            }
            let card = bitset_card(&out);
            Container::Bitset(out, card).optimize()
        }
        (Container::Array(x), Container::Run(runs, _)) => {
            let mut w = Box::new([0u64; BITSET_WORDS]);
            for &v in x {
                w[v as usize >> 6] |= 1u64 << (v & 63);
            }
            for &(s, e) in runs {
                set_range(&mut w, s, e);
            }
            let card = bitset_card(&w);
            Container::Bitset(w, card).optimize()
        }
        (Container::Run(_, _), Container::Array(_)) => container_or(b, a),
        (Container::Bitset(w, _), Container::Run(runs, _))
        | (Container::Run(runs, _), Container::Bitset(w, _)) => {
            let mut out = w.clone();
            for &(s, e) in runs {
                set_range(&mut out, s, e);
            }
            let card = bitset_card(&out);
            Container::Bitset(out, card).optimize()
        }
        (Container::Bitset(x, _), Container::Bitset(y, _)) => {
            let mut out = Box::new([0u64; BITSET_WORDS]);
            let card = match simd::try_bitset_or(x, y, &mut out) {
                Some(c) => c,
                None => bitset_or_scalar(x, y, &mut out),
            };
            Container::Bitset(out, card).optimize()
        }
        (Container::Run(x, _), Container::Run(y, _)) => run_run_union(x, y),
    }
}

/// A roaring bitmap: containers keyed by the high 16 bits of the docIDs,
/// sorted by key, at most one container per key (Roaring 论文 §2).
pub struct RoaringBitmap {
    containers: Vec<(u16, Container)>,
    card: u64,
}

impl RoaringBitmap {
    /// Builds from an ascending doc list (the write-side `docs` slice) and
    /// run-optimizes every container (spec §4: 构建后 runOptimize).
    pub fn from_sorted_docs(docs: &[u32]) -> RoaringBitmap {
        debug_assert!(docs.windows(2).all(|w| w[0] < w[1]), "docs must ascend");
        let mut containers: Vec<(u16, Container)> = Vec::new();
        let mut i = 0usize;
        while i < docs.len() {
            let key = (docs[i] >> 16) as u16;
            let mut j = i + 1;
            while j < docs.len() && (docs[j] >> 16) as u16 == key {
                j += 1;
            }
            let lows: Vec<u16> = docs[i..j].iter().map(|&d| d as u16).collect();
            let c = if lows.len() < ARRAY_THRESHOLD {
                Container::Array(lows)
            } else {
                let mut w = Box::new([0u64; BITSET_WORDS]);
                for &v in &lows {
                    w[v as usize >> 6] |= 1u64 << (v & 63);
                }
                Container::Bitset(w, lows.len() as u32)
            };
            containers.push((key, c.optimize()));
            i = j;
        }
        RoaringBitmap {
            containers,
            card: docs.len() as u64,
        }
    }

    /// Total number of docs (== df of the term the bitmap was built for).
    pub fn cardinality(&self) -> u64 {
        self.card
    }

    pub fn is_empty(&self) -> bool {
        self.card == 0
    }

    /// Container-level intersection (spec §5 档 1/2): merge the key sets,
    /// intersect per shared key; the result is itself a roaring bitmap.
    /// Empty result containers are dropped (deserialize 的 card ≥ 1 不变式).
    pub fn and(&self, other: &RoaringBitmap) -> RoaringBitmap {
        let mut containers = Vec::new();
        let mut card = 0u64;
        let (mut i, mut j) = (0usize, 0usize);
        while i < self.containers.len() && j < other.containers.len() {
            let (ka, ca) = &self.containers[i];
            let (kb, cb) = &other.containers[j];
            match ka.cmp(kb) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    let c = container_and(ca, cb);
                    if c.card() > 0 {
                        card += c.card() as u64;
                        containers.push((*ka, c));
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
        RoaringBitmap { containers, card }
    }

    /// Container-level union; single-side containers are carried over.
    pub fn or(&self, other: &RoaringBitmap) -> RoaringBitmap {
        let mut containers = Vec::new();
        let mut card = 0u64;
        let (mut i, mut j) = (0usize, 0usize);
        while i < self.containers.len() || j < other.containers.len() {
            let ka = self.containers.get(i);
            let kb = other.containers.get(j);
            match (ka, kb) {
                (Some((ka, ca)), Some((kb, cb))) => match ka.cmp(kb) {
                    std::cmp::Ordering::Less => {
                        card += ca.card() as u64;
                        containers.push((*ka, clone_container(ca)));
                        i += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        card += cb.card() as u64;
                        containers.push((*kb, clone_container(cb)));
                        j += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        let c = container_or(ca, cb);
                        card += c.card() as u64;
                        containers.push((*ka, c));
                        i += 1;
                        j += 1;
                    }
                },
                (Some((ka, ca)), None) => {
                    card += ca.card() as u64;
                    containers.push((*ka, clone_container(ca)));
                    i += 1;
                }
                (None, Some((kb, cb))) => {
                    card += cb.card() as u64;
                    containers.push((*kb, clone_container(cb)));
                    j += 1;
                }
                (None, None) => unreachable!(),
            }
        }
        RoaringBitmap { containers, card }
    }
}

fn clone_container(c: &Container) -> Container {
    match c {
        Container::Array(v) => Container::Array(v.clone()),
        Container::Bitset(w, card) => Container::Bitset(w.clone(), *card),
        Container::Run(runs, card) => Container::Run(runs.clone(), *card),
    }
}

/// Plain-data iteration cursor (no borrow of the bitmap, so iterators can
/// own both). State per current container type: Array → a = next element
/// index; Bitset → a = next bit to check; Run → a = run index, b = offset
/// of the next value within the run.
#[derive(Clone, Copy, Default)]
pub struct RoaringCursor {
    ci: u32,
    a: u32,
    b: u32,
}

impl RoaringBitmap {
    pub fn cursor(&self) -> RoaringCursor {
        RoaringCursor::default()
    }

    /// Next doc at/after the cursor position, or None when exhausted.
    pub fn cursor_next(&self, cur: &mut RoaringCursor) -> Option<u32> {
        loop {
            let (key, c) = self.containers.get(cur.ci as usize)?;
            match c {
                Container::Array(v) => {
                    if (cur.a as usize) < v.len() {
                        let r = v[cur.a as usize];
                        cur.a += 1;
                        return Some(((*key as u32) << 16) | r as u32);
                    }
                }
                Container::Bitset(w, _) => {
                    if let Some(bit) = next_set_bit(w, cur.a) {
                        cur.a = bit + 1;
                        return Some(((*key as u32) << 16) | bit);
                    }
                }
                Container::Run(runs, _) => {
                    if (cur.a as usize) < runs.len() {
                        let (s, e) = runs[cur.a as usize];
                        let v = s as u32 + cur.b;
                        if v < e as u32 {
                            cur.b += 1;
                        } else {
                            cur.a += 1;
                            cur.b = 0;
                        }
                        return Some(((*key as u32) << 16) | v);
                    }
                }
            }
            cur.ci += 1;
            cur.a = 0;
            cur.b = 0;
        }
    }

    /// First doc >= target; the cursor ends positioned past it. Forward
    /// only: callers pass targets greater than the last returned doc (the
    /// DocIter advance contract guarantees this).
    pub fn cursor_advance(&self, cur: &mut RoaringCursor, target: u32) -> Option<u32> {
        let key = (target >> 16) as u16;
        let low = target as u16;
        let ci = self.containers.partition_point(|(k, _)| *k < key);
        if ci > cur.ci as usize {
            cur.ci = ci as u32;
            cur.a = 0;
            cur.b = 0;
        }
        let (k, c) = self.containers.get(cur.ci as usize)?;
        if *k > key {
            // target's bucket absent: first value of the current container
            cur.a = 0;
            cur.b = 0;
            return self.cursor_next(cur);
        }
        match c {
            Container::Array(v) => {
                let p = v.partition_point(|&x| x < low);
                cur.a = cur.a.max(p as u32);
            }
            Container::Bitset(_, _) => {
                cur.a = cur.a.max(low as u32);
            }
            Container::Run(runs, _) => {
                let p = runs.partition_point(|&(_, e)| e < low);
                if p as u32 > cur.a {
                    // target lands in a later run: the partially consumed
                    // run's in-run offset must not leak into the new run
                    cur.a = p as u32;
                    cur.b = 0;
                }
                if (cur.a as usize) < runs.len() {
                    let (s, _) = runs[cur.a as usize];
                    cur.b = cur.b.max(low.saturating_sub(s) as u32);
                }
            }
        }
        self.cursor_next(cur)
    }
}

// ------------------------------------------------------------------
// wire format (spec §4; 布局逐项见关键设计事实 2)
// ------------------------------------------------------------------

pub const BITMAP_MAGIC: [u8; 4] = *b"RLBM";
/// Wire format version. v3 (M5 §3): payload = CRoaring Frozen format
/// (engine replaced by croaring; the self-built container payload is
/// gone). The version byte is the only migration gate: != 3 (v1/v2
/// included) silently falls back to postings.
pub const BITMAP_VERSION: u8 = 3;

/// Wire version of the transitional self-built container format (M4 v2),
/// used only by `serialize`/`deserialize` and their own tests until T4
/// deletes them (关键设计事实 10). Never written to an index by M5.
const SELF_BUILT_WIRE_VERSION: u8 = 2;

const TYPE_ARRAY: u8 = 0;
const TYPE_BITSET: u8 = 1;
const TYPE_RUN: u8 = 2;

/// Upper bound of the bitmap region length (header+payload) for a segment
/// with `max_doc` docs. Derivation (spec §3 len 有界校验; 关键设计事实 1):
/// CRoaring frozen payload = 每 container 数据区 ≤ 8192B（array ≤ 4096×2、
/// bitset = 1024×8、run 只在严格更小时转换 ⇒ < 8192B，convert_run_optimize
/// roaring.c:9915-9970）+ 每 container 5B（keys 2 + counts 2 + typecodes 1）
/// + 4B header（frozen_size_in_bytes roaring.c:18017-18039）；
/// container 数 ≤ ceil(maxDoc/65536)；我方头 ≤ 4+1+5+5 = 15B：
///   max_bitmap_len = 19 + ceil(max_doc / 65536) * 8197
pub fn max_bitmap_len(max_doc: u32) -> u64 {
    19 + (max_doc as u64).div_ceil(65536) * 8197
}

impl RoaringBitmap {
    /// header+payload (without the trailing len, which the writer appends;
    /// without the v1 crc32, M4 §3). `df` is the term's docFreq and must
    /// equal the cardinality (docs-only bitmap).
    /// T4 删除；与索引写侧 v3 解耦（SELF_BUILT_WIRE_VERSION）。
    pub fn serialize(&self, df: u32) -> Vec<u8> {
        debug_assert_eq!(df as u64, self.card);
        let mut out = IndexOutput::in_memory();
        // in-memory writes never fail (Vec sink)
        out.write_bytes(&BITMAP_MAGIC).unwrap();
        out.write_byte(SELF_BUILT_WIRE_VERSION).unwrap();
        out.write_vint(df as i32).unwrap();
        out.write_vint(self.card as i32).unwrap();
        out.write_vint(self.containers.len() as i32).unwrap();
        for (key, c) in &self.containers {
            out.write_short(*key as i16).unwrap();
            match c {
                Container::Array(v) => {
                    out.write_byte(TYPE_ARRAY).unwrap();
                    out.write_vint(v.len() as i32).unwrap();
                    for &x in v {
                        out.write_short(x as i16).unwrap();
                    }
                }
                Container::Bitset(w, card) => {
                    out.write_byte(TYPE_BITSET).unwrap();
                    out.write_vint(*card as i32).unwrap();
                    for &word in w.iter() {
                        out.write_long(word as i64).unwrap();
                    }
                }
                Container::Run(runs, card) => {
                    out.write_byte(TYPE_RUN).unwrap();
                    out.write_vint(*card as i32).unwrap();
                    out.write_vint(runs.len() as i32).unwrap();
                    for &(s, e) in runs {
                        out.write_short(s as i16).unwrap();
                        out.write_short(e as i16).unwrap();
                    }
                }
            }
        }
        out.into_bytes()
    }

    /// Parses + validates a bitmap region (the `len` bytes preceding
    /// docStartFP-4). Returns None on ANY deviation — magic/version
    /// mismatch (v1 included: the version gate is the whole migration
    /// story, M4 §3), df != expected_df, cardinality != df, structural
    /// violation, or trailing bytes — the read side's silent-fallback
    /// signal. v2 drops the crc32 check (triple gate: len bound →
    /// magic/version → header df/card; 关键设计事实 3).
    /// T4 删除；与索引写侧 v3 解耦（SELF_BUILT_WIRE_VERSION）。
    pub fn deserialize(bytes: &[u8], expected_df: u32) -> Option<RoaringBitmap> {
        if bytes.len() < 12 {
            return None;
        }
        let mut input = IndexInput::in_memory(bytes.to_vec());
        let mut magic = [0u8; 4];
        input.read_bytes(&mut magic).ok()?;
        if magic != BITMAP_MAGIC {
            return None;
        }
        if input.read_byte().ok()? != SELF_BUILT_WIRE_VERSION {
            return None;
        }
        let df = input.read_vint().ok()? as u32;
        if df != expected_df {
            return None;
        }
        let card = input.read_vint().ok()? as u32;
        if card != df {
            return None; // docs-only bitmap: cardinality == df
        }
        let num_containers = input.read_vint().ok()?;
        if !(1..=65536).contains(&num_containers) {
            return None;
        }
        let mut containers = Vec::with_capacity(num_containers as usize);
        let mut card_sum = 0u64;
        let mut last_key: Option<u16> = None;
        for _ in 0..num_containers {
            let key = input.read_short().ok()? as u16;
            if let Some(lk) = last_key
                && key <= lk
            {
                return None;
            }
            last_key = Some(key);
            let ty = input.read_byte().ok()?;
            let card = input.read_vint().ok()?;
            if card < 1 {
                return None;
            }
            let c = match ty {
                TYPE_ARRAY => {
                    let n = card as usize;
                    if n > BITSET_BITS {
                        return None;
                    }
                    let mut v = Vec::with_capacity(n);
                    let mut last: Option<u16> = None;
                    for _ in 0..n {
                        let x = input.read_short().ok()? as u16;
                        if let Some(l) = last
                            && x <= l
                        {
                            return None;
                        }
                        last = Some(x);
                        v.push(x);
                    }
                    Container::Array(v)
                }
                TYPE_BITSET => {
                    let mut w = Box::new([0u64; BITSET_WORDS]);
                    for word in w.iter_mut() {
                        *word = input.read_long().ok()? as u64;
                    }
                    if bitset_card(&w) != card as u32 {
                        return None;
                    }
                    Container::Bitset(w, card as u32)
                }
                TYPE_RUN => {
                    let num_runs = input.read_vint().ok()?;
                    // at most 2^15 disjoint runs over a 2^16-value domain;
                    // the bound also caps the with_capacity allocation
                    if !(1..=32768).contains(&num_runs) {
                        return None;
                    }
                    let mut runs = Vec::with_capacity(num_runs as usize);
                    let mut sum = 0u64;
                    let mut last: Option<(u16, u16)> = None;
                    for _ in 0..num_runs {
                        let s = input.read_short().ok()? as u16;
                        let e = input.read_short().ok()? as u16;
                        if s > e {
                            return None;
                        }
                        if let Some((_, le)) = last
                            && s <= le
                        {
                            return None; // overlapping / unsorted
                        }
                        sum += e as u64 - s as u64 + 1;
                        last = Some((s, e));
                        runs.push((s, e));
                    }
                    if sum != card as u64 {
                        return None;
                    }
                    Container::Run(runs, card as u32)
                }
                _ => return None,
            };
            card_sum += c.card() as u64;
            containers.push((key, c));
        }
        if card_sum != card as u64 {
            return None;
        }
        if input.file_pointer() != bytes.len() as u64 {
            return None; // trailing bytes: not one of our bitmaps
        }
        Some(RoaringBitmap {
            containers,
            card: card_sum,
        })
    }
}

/// Builds the bitmap for one term and writes `[region][len: u32 LE]` into
/// the .doc stream, immediately before the term's postings (M5 §3). Engine:
/// croaring — `Bitmap::of` (bulk append, sorted input) → `run_optimize` →
/// `shrink_to_fit` → Frozen serialize (`Bitmap::serialize::<Frozen>` does
/// not exist — Frozen is not NoAlign, imp.rs:871; `serialize_into_vec`
/// carves a 32B-aligned slice inside the Vec, probe main.rs:60-77). Must go
/// through the same checksumming output as the rest of .doc so the footer
/// CRC stays valid (spec §4a.2; CodecUtil.writeCRC :643-650).
pub fn write_term_bitmap(out: &mut impl DataOutput, docs: &[u32]) -> io::Result<()> {
    debug_assert!(!docs.is_empty());
    let mut bitmap = croaring::Bitmap::of(docs);
    bitmap.run_optimize();
    bitmap.shrink_to_fit();
    let mut buf = Vec::new();
    let payload = bitmap.serialize_into_vec::<croaring::Frozen>(&mut buf);
    let mut region = IndexOutput::in_memory();
    // in-memory writes never fail (Vec sink)
    region.write_bytes(&BITMAP_MAGIC).unwrap();
    region.write_byte(BITMAP_VERSION).unwrap();
    region.write_vint(docs.len() as i32).unwrap();
    // docs-only bitmap: cardinality == df (asserted by construction)
    debug_assert_eq!(bitmap.cardinality(), docs.len() as u64);
    region.write_vint(docs.len() as i32).unwrap();
    region.write_bytes(payload).unwrap();
    let bytes = region.into_bytes();
    out.write_bytes(&bytes)?;
    out.write_int(bytes.len() as i32)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift64* — deterministic, same as the other codec test modules.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u32) -> u32 {
            (self.next() % n as u64) as u32
        }
    }

    fn to_vec(b: &RoaringBitmap) -> Vec<u32> {
        let mut cur = b.cursor();
        let mut out = Vec::new();
        while let Some(d) = b.cursor_next(&mut cur) {
            out.push(d);
        }
        out
    }

    fn ref_and(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut out = Vec::new();
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            match a[i].cmp(&b[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    out.push(a[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        out
    }

    fn ref_or(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut out = Vec::with_capacity(a.len() + b.len());
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            if a[i] < b[j] {
                out.push(a[i]);
                i += 1;
            } else if a[i] > b[j] {
                out.push(b[j]);
                j += 1;
            } else {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
        out.extend_from_slice(&a[i..]);
        out.extend_from_slice(&b[j..]);
        out
    }

    /// Sorted unique docs: `n` random low-16 values per listed bucket.
    fn shaped_docs(rng: &mut Rng, buckets: &[(u16, usize)]) -> Vec<u32> {
        let mut docs: Vec<u32> = Vec::new();
        for &(key, n) in buckets {
            for _ in 0..n {
                docs.push(((key as u32) << 16) | rng.below(65536));
            }
            docs.sort_unstable();
            docs.dedup();
        }
        docs
    }

    #[test]
    fn build_chooses_container_types() {
        // sparse (< 4096 in one bucket) -> Array
        let sparse = shaped_docs(&mut Rng(1), &[(3, 100)]);
        let b = RoaringBitmap::from_sorted_docs(&sparse);
        assert!(matches!(b.containers[0].1, Container::Array(_)));
        assert_eq!(b.cardinality(), sparse.len() as u64);

        // 5000 consecutive values -> runOptimize -> single Run
        let dense: Vec<u32> = (100..5100).collect();
        let b = RoaringBitmap::from_sorted_docs(&dense);
        assert!(matches!(&b.containers[0].1, Container::Run(runs, _) if runs.len() == 1));
        assert_eq!(b.cardinality(), 5000);

        // 6000 scattered values in one bucket -> Bitset (runs too many)
        let scattered = shaped_docs(&mut Rng(7), &[(9, 6000)]);
        let b = RoaringBitmap::from_sorted_docs(&scattered);
        assert!(matches!(b.containers[0].1, Container::Bitset(_, _)));
        assert_eq!(b.cardinality(), scattered.len() as u64);
    }

    #[test]
    fn iteration_round_trip_mixed_containers() {
        let mut docs = shaped_docs(&mut Rng(11), &[(0, 300), (1, 5000), (2, 3)]);
        docs.extend(70_000..75_000u32); // dense consecutive stretch in bucket 1
        docs.sort_unstable();
        docs.dedup();
        let b = RoaringBitmap::from_sorted_docs(&docs);
        assert_eq!(to_vec(&b), docs);
        assert_eq!(b.cardinality(), docs.len() as u64);
        assert!(!b.is_empty());
        assert!(RoaringBitmap::from_sorted_docs(&[]).is_empty());
    }

    #[test]
    fn cursor_advance_matches_linear_scan() {
        let docs = shaped_docs(&mut Rng(13), &[(0, 2000), (1, 6000), (4, 17)]);
        let b = RoaringBitmap::from_sorted_docs(&docs);
        let mut rng = Rng(99);
        // fresh cursor per target: first doc >= target
        for _ in 0..2000 {
            let t = rng.below(300_000);
            let want = docs.iter().find(|&&d| d >= t).copied();
            let mut cur = b.cursor();
            assert_eq!(b.cursor_advance(&mut cur, t), want, "target {t}");
        }
        // interleaved next/advance on one cursor (forward-only contract)
        let mut cur = b.cursor();
        let after = docs.iter().find(|&&d| d >= 10).copied().unwrap();
        assert_eq!(b.cursor_advance(&mut cur, 10), Some(after));
        let want_next = docs.iter().find(|&&d| d > after).copied();
        assert_eq!(b.cursor_next(&mut cur), want_next);
        // advance past the end -> None, sticky
        let mut cur = b.cursor();
        assert_eq!(b.cursor_advance(&mut cur, u32::MAX), None);
        assert_eq!(b.cursor_next(&mut cur), None);
    }

    #[test]
    fn and_or_match_reference_sets() {
        let mut rng = Rng(42);
        // operand pairs exercising every container pair type
        let cases: Vec<(Vec<u32>, Vec<u32>)> = vec![
            // array x array (similar sizes -> merge path)
            (
                shaped_docs(&mut rng, &[(0, 100)]),
                shaped_docs(&mut rng, &[(0, 120)]),
            ),
            // array x array (skewed -> galloping path)
            (
                shaped_docs(&mut rng, &[(0, 5)]),
                shaped_docs(&mut rng, &[(0, 4000)]),
            ),
            // run x run (dense consecutive)
            ((0..6000u32).collect(), (3000..9000u32).collect()),
            // bitset x bitset (scattered dense)
            (
                shaped_docs(&mut rng, &[(2, 6000)]),
                shaped_docs(&mut rng, &[(2, 7000)]),
            ),
            // array x bitset, array x run, bitset x run, multi-bucket
            (
                shaped_docs(&mut rng, &[(3, 800), (4, 50)]),
                shaped_docs(&mut rng, &[(3, 6000), (5, 20)]),
            ),
            (
                (10_000..20_000u32).collect(),
                shaped_docs(&mut rng, &[(0, 5000), (1, 3000)]),
            ),
            // disjoint keys, empty operand
            (
                shaped_docs(&mut rng, &[(7, 100)]),
                shaped_docs(&mut rng, &[(9, 100)]),
            ),
            (vec![], shaped_docs(&mut rng, &[(1, 100)])),
        ];
        for (ci, (a, b)) in cases.iter().enumerate() {
            let ba = RoaringBitmap::from_sorted_docs(a);
            let bb = RoaringBitmap::from_sorted_docs(b);
            let and = ba.and(&bb);
            assert_eq!(to_vec(&and), ref_and(a, b), "and case {ci}");
            assert_eq!(
                and.cardinality(),
                ref_and(a, b).len() as u64,
                "and card {ci}"
            );
            let or = ba.or(&bb);
            assert_eq!(to_vec(&or), ref_or(a, b), "or case {ci}");
            assert_eq!(or.cardinality(), ref_or(a, b).len() as u64, "or card {ci}");
        }
    }

    /// Test-only v1-layout writer (M3 format): the v2 image with the
    /// version byte rewound to 1 and crc32fast(header+payload) appended —
    /// byte-for-byte what M3's serialize produced (the crc only trailed,
    /// so all header/payload offsets are unchanged).
    /// pub(crate): T2 的 `roaring::view::tests` 经
    /// `use super::super::tests::serialize_v1_for_test` 跨模块复用——
    /// 非 `roaring::tests` 的后代模块，私有 fn 不可见。
    pub(crate) fn serialize_v1_for_test(b: &RoaringBitmap, df: u32) -> Vec<u8> {
        let mut bytes = b.serialize(df);
        bytes[4] = 1; // BITMAP_VERSION v1
        let crc = crc32fast::hash(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes
    }

    #[test]
    fn v2_serialize_has_no_crc_and_rejects_v1() {
        let docs = shaped_docs(&mut Rng(5), &[(0, 100), (1, 5000)]);
        let b = RoaringBitmap::from_sorted_docs(&docs);
        let v2 = b.serialize(docs.len() as u32);
        assert_eq!(&v2[..4], b"RLBM");
        assert_eq!(v2[4], 2, "format v2");
        // round-trips under v2
        let back = RoaringBitmap::deserialize(&v2, docs.len() as u32).unwrap();
        assert_eq!(to_vec(&back), docs);
        // the M3 v1 image (valid crc!) must be rejected at the version gate
        let v1 = serialize_v1_for_test(&b, docs.len() as u32);
        assert_eq!(v1[4], 1);
        assert!(
            RoaringBitmap::deserialize(&v1, docs.len() as u32).is_none(),
            "v1 layout must be rejected even with a valid v1 crc"
        );
    }

    #[test]
    fn serialize_deserialize_round_trip() {
        let mut rng = Rng(5);
        let cases: Vec<Vec<u32>> = vec![
            shaped_docs(&mut rng, &[(0, 100)]),
            (0..5000u32).collect(),
            shaped_docs(&mut rng, &[(1, 6000), (2, 10), (9, 4500)]),
        ];
        for (ci, docs) in cases.iter().enumerate() {
            let b = RoaringBitmap::from_sorted_docs(docs);
            let bytes = b.serialize(docs.len() as u32);
            let back = RoaringBitmap::deserialize(&bytes, docs.len() as u32)
                .unwrap_or_else(|| panic!("case {ci} must deserialize"));
            assert_eq!(to_vec(&back), *docs, "case {ci}");
            assert_eq!(back.cardinality(), docs.len() as u64, "case {ci}");
            // wrong expected df -> None
            assert!(RoaringBitmap::deserialize(&bytes, docs.len() as u32 + 1).is_none());
            // corrupted magic / version -> None (v2: payload integrity is
            // the .doc footer CRC's job; structural violations are covered
            // by deserialize_rejects_structural_violations)
            for (pos, tag) in [(0usize, "magic"), (4, "version")] {
                let mut bad = bytes.clone();
                bad[pos] ^= 0xFF;
                assert!(
                    RoaringBitmap::deserialize(&bad, docs.len() as u32).is_none(),
                    "case {ci} corrupt {tag}"
                );
            }
            // truncated -> None
            assert!(
                RoaringBitmap::deserialize(&bytes[..bytes.len() / 2], docs.len() as u32).is_none()
            );
        }
    }

    #[test]
    fn max_bitmap_len_bound_holds() {
        assert_eq!(max_bitmap_len(200_000), 19 + 4 * 8197);
        assert_eq!(max_bitmap_len(1), 19 + 8197);
        // v3: the bound applies to the region length write_term_bitmap emits
        let docs = shaped_docs(&mut Rng(3), &[(0, 4096), (1, 5000), (2, 6000), (3, 100)]);
        let mut out = IndexOutput::in_memory();
        write_term_bitmap(&mut out, &docs).unwrap();
        let bytes = out.into_bytes();
        let len = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap()) as u64;
        assert_eq!(len as usize + 4, bytes.len());
        assert!(len <= max_bitmap_len(200_000), "len {len} vs bound");
    }

    /// v3 layout (M5 §3): [magic + version=3 + df + card + Frozen payload]
    /// [len u32 LE]; payload tail = frozen header (FROZEN_COOKIE 低 15 位).
    #[test]
    fn write_term_bitmap_appends_len_suffix() {
        let docs: Vec<u32> = (0..5000u32).collect();
        let mut out = IndexOutput::in_memory();
        write_term_bitmap(&mut out, &docs).unwrap();
        let bytes = out.into_bytes();
        let len = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap()) as usize;
        assert_eq!(len + 4, bytes.len());
        let region = &bytes[..len];
        assert_eq!(&region[..4], b"RLBM");
        assert_eq!(region[4], 3, "format v3");
        // df + card 两个 vInt（5000 = 0x88 0x27 两字节编码）
        let mut input = IndexInput::in_memory(region[5..].to_vec());
        assert_eq!(input.read_vint().unwrap(), 5000);
        assert_eq!(input.read_vint().unwrap(), 5000);
        // frozen payload 尾部 4B header：低 15 位 == FROZEN_COOKIE（roaring.h:7935）
        let header = u32::from_le_bytes(region[region.len() - 4..].try_into().unwrap());
        assert_eq!(header & 0x7FFF, 13766, "FROZEN_COOKIE");
        assert!((header >> 15) >= 1, "num_containers");
        assert!(len as u64 <= max_bitmap_len(5000));
    }

    #[test]
    fn run_container_advance_drops_stale_offset() {
        // one bucket, two runs (0,100) and (200,300) -> Run container
        let docs: Vec<u32> = (0..=100u32).chain(200..=300u32).collect();
        let b = RoaringBitmap::from_sorted_docs(&docs);
        assert!(matches!(&b.containers[0].1, Container::Run(runs, _) if runs.len() == 2));

        // partially consume the first run (docs 0..=49, cursor at a=0, b=50),
        // then advance past its end: the stale in-run offset must not leak
        // into the next run (the bug returned 250, silently skipping 210-249)
        let mut cur = b.cursor();
        for i in 0..50 {
            assert_eq!(b.cursor_next(&mut cur), Some(i));
        }
        assert_eq!(b.cursor_advance(&mut cur, 210), Some(210));
        assert_eq!(b.cursor_next(&mut cur), Some(211));

        // advance landing INSIDE the partially consumed run: the offset is
        // clamped forward to the target, not reset
        let mut cur = b.cursor();
        for i in 0..50 {
            assert_eq!(b.cursor_next(&mut cur), Some(i));
        }
        assert_eq!(b.cursor_advance(&mut cur, 60), Some(60));
        assert_eq!(b.cursor_next(&mut cur), Some(61));

        // target behind the in-run offset: forward-only, no rewind
        let mut cur = b.cursor();
        for i in 0..50 {
            assert_eq!(b.cursor_next(&mut cur), Some(i));
        }
        assert_eq!(b.cursor_advance(&mut cur, 30), Some(50));
    }

    #[test]
    fn cursor_next_advance_interleaved_matches_reference() {
        // mixed containers across buckets: array(0), run(1), bitset(2),
        // run(3), array(4), multi-run(5) — runs of 4000/5000 consecutive,
        // scattered dense, and short separated stretches (4 runs/100 values)
        let mut docs = shaped_docs(&mut Rng(17), &[(0, 100), (2, 6000), (4, 30)]);
        docs.extend(65_536..70_536u32); // bucket 1: consecutive -> Run
        docs.extend(200_000..205_000u32); // bucket 3: consecutive -> Run
        let base = 5u32 << 16;
        for off in [0u32, 1000, 5000, 30000] {
            docs.extend(base + off..base + off + 100); // bucket 5: 4-run Run
        }
        docs.sort_unstable();
        docs.dedup();
        let b = RoaringBitmap::from_sorted_docs(&docs);

        // single cursor, random interleaving of next and (forward-only)
        // advance, checked against a linear-scan reference model
        let mut cur = b.cursor();
        let mut idx = 0usize; // reference: next unconsumed position in docs
        let mut last: Option<u32> = None;
        let mut rng = Rng(23);
        for step in 0..800 {
            let do_next = rng.below(2) == 0;
            let target = match last {
                None => rng.below(1000),
                Some(l) => l + 1 + rng.below(500),
            };
            let got = if do_next {
                b.cursor_next(&mut cur)
            } else {
                b.cursor_advance(&mut cur, target)
            };
            if !do_next {
                idx = idx.max(docs.partition_point(|&d| d < target));
            }
            let want = docs.get(idx).copied();
            if want.is_some() {
                idx += 1;
            }
            assert_eq!(got, want, "step {step}");
            if got.is_some() {
                last = got;
            }
        }
    }

    #[test]
    fn deserialize_rejects_structural_violations() {
        // array with non-ascending elements (sole guard: the ascending
        // check) — layout: magic[0..4] ver[4] df[5] card[6] ncont[7]
        // key[8..10] type[10] card[11] elems[12..]
        let b = RoaringBitmap::from_sorted_docs(&[10, 20, 30]);
        let bytes = b.serialize(3);
        assert_eq!(bytes[10], TYPE_ARRAY);
        let mut bad = bytes.clone();
        bad[14] = 5; // second element 20 -> 5: sequence 10,5,30
        assert!(RoaringBitmap::deserialize(&bad, 3).is_none());

        // overlapping runs with the cardinality sum preserved (sole guard:
        // the overlap check) — header as above but 2-byte vints (202) and a
        // num_runs byte: runs start at [16], run 1 at [20..24]
        let docs: Vec<u32> = (0..=100u32).chain(200..=300u32).collect();
        let b = RoaringBitmap::from_sorted_docs(&docs);
        let bytes = b.serialize(202);
        assert_eq!(bytes[12], TYPE_RUN);
        assert_eq!(bytes[15], 2); // num_runs
        let mut bad = bytes.clone();
        // second run (200,300) -> (50,150): overlaps the first, sum == 202
        bad[20..22].copy_from_slice(&50u16.to_le_bytes());
        bad[22..24].copy_from_slice(&150u16.to_le_bytes());
        assert!(RoaringBitmap::deserialize(&bad, 202).is_none());

        // bitset with a flipped word (sole guard: popcount == card)
        let scattered = shaped_docs(&mut Rng(7), &[(9, 6000)]);
        let b = RoaringBitmap::from_sorted_docs(&scattered);
        let n = scattered.len();
        let bytes = b.serialize(n as u32);
        assert_eq!(bytes[12], TYPE_BITSET);
        let mut bad = bytes.clone();
        bad[16] ^= 0xFF; // inside the first word of the bitset image
        assert!(RoaringBitmap::deserialize(&bad, n as u32).is_none());
    }
}
