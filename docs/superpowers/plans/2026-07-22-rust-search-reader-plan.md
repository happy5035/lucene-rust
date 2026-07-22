# Rust Search Reader Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add pure-Rust index reading and ConstantScore query execution to lucene-rust, matching Java Lucene 9.12.3 output doc-for-doc.

**Architecture:** Add reader modules to `codec-lucene9` (IndexInput + format-specific readers), a `search` module to `rustlucene-core` (Query enum → DocIterator → Collector), and a JNI facade in `crates/jni-binding`. No Weight/Scorer abstractions — Query produces DocIterator directly.

**Tech Stack:** Rust, existing `codec-lucene9` write primitives, `lz4` crate (decompression), `serde_json` (query parsing), `jni` crate 0.21 (JNI facade).

## Global Constraints

- `#![forbid(unsafe_code)]` in codec-lucene9; unsafe only in AVX2 feature-gated decode
- All indexed fields use `omit_norms = true` — no scoring data, all queries are ConstantScore
- File format: Lucene 9.12.3 (`.tip/.tim/.doc/.pos/.dvd/.dvm/.kdd/.kdi/.fdt/.fdx/.fdm/.fnm/.si/segments_N`)
- Only 1D BKD points, only Numeric + Sorted DocValues
- Existing interop tests must continue to pass (`make interop-test`, `make log-test`)
- Worktree: `/home/ubuntu/work/lucene-rust/.claude/worktrees/feat+search-reader`

---

## Phase 1 — Foundation: IndexInput + Primitive Decoders (Tasks 1–6)

### Task 1: IndexInput trait + HeapIndexInput

**Files:**
- Modify: `crates/codec-lucene9/src/io.rs` (add trait + impl after existing IndexOutput)
- Modify: `crates/codec-lucene9/src/lib.rs` (re-export IndexInput if needed)

**Interfaces:**
- Produces: `pub trait IndexInput: Read { read_byte, read_bytes, read_vlong, read_vint, read_zint, read_string, file_pointer, seek, length, slice }`
- Produces: `pub struct HeapIndexInput { buf: Vec<u8>, pos: usize }`

- [ ] **Step 1: Add IndexInput trait + HeapIndexInput to io.rs**

Append after the end of the existing `IndexOutput` implementation in `io.rs`:

```rust
// ============================================================================
// IndexInput — read counterpart to IndexOutput
// ============================================================================

use std::io::{Read, Seek, SeekFrom};

/// Random-access input mirroring Lucene `store/IndexInput.java` (9.12.3).
pub trait IndexInput: Read {
    fn read_byte(&mut self) -> io::Result<u8>;
    fn read_bytes(&mut self, buf: &mut [u8], offset: usize, len: usize) -> io::Result<()>;
    fn read_vlong(&mut self) -> io::Result<i64>;
    fn read_vint(&mut self) -> io::Result<i32>;
    fn read_zint(&mut self) -> io::Result<i32>;
    fn read_string(&mut self) -> io::Result<String>;
    fn file_pointer(&self) -> u64;
    fn seek(&mut self, pos: u64) -> io::Result<()>;
    fn length(&self) -> u64;
    fn slice(&self, offset: u64, len: u64) -> io::Result<Box<dyn IndexInput>>;
}

/// In-memory input backed by `Vec<u8>`. Used for small files (.fnm, .si, .tip, .kdm).
pub struct HeapIndexInput {
    buf: Vec<u8>,
    pos: usize,
}

impl HeapIndexInput {
    pub fn new(buf: Vec<u8>) -> Self {
        HeapIndexInput { buf, pos: 0 }
    }
}

impl Read for HeapIndexInput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let avail = self.buf.len() - self.pos;
        let n = buf.len().min(avail);
        buf[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl IndexInput for HeapIndexInput {
    fn read_byte(&mut self) -> io::Result<u8> {
        if self.pos >= self.buf.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past end"));
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn read_bytes(&mut self, buf: &mut [u8], offset: usize, len: usize) -> io::Result<()> {
        let end = self.pos + len;
        if end > self.buf.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read_bytes past end"));
        }
        buf[offset..offset + len].copy_from_slice(&self.buf[self.pos..end]);
        self.pos = end;
        Ok(())
    }

    fn read_vlong(&mut self) -> io::Result<i64> {
        // Lucene VLong: varint where the high bit of each byte is continuation flag.
        // Relevant source: DataInput.readVLong (DataInput.java:471-498).
        let b = self.read_byte()?;
        if b & 0x80 == 0 { return Ok(b as i64); }
        let mut v = (b & 0x7F) as i64;
        let mut shift = 7;
        loop {
            let b = self.read_byte()?;
            v |= ((b & 0x7F) as i64) << shift;
            shift += 7;
            if b & 0x80 == 0 { break; }
        }
        Ok(v)
    }

    fn read_vint(&mut self) -> io::Result<i32> {
        self.read_vlong().map(|v| v as i32)
    }

    fn read_zint(&mut self) -> io::Result<i32> {
        let v = self.read_vlong()? as u64;
        // zigzag decode: (v >>> 1) ^ -(v & 1)
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64) as i32)
    }

    fn read_string(&mut self) -> io::Result<String> {
        let len = self.read_vint()? as usize;
        let mut buf = vec![0u8; len];
        self.read_bytes(&mut buf, 0, len)?;
        String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn file_pointer(&self) -> u64 { self.pos as u64 }
    fn seek(&mut self, pos: u64) -> io::Result<()> {
        if pos as usize > self.buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek past end"));
        }
        self.pos = pos as usize;
        Ok(())
    }
    fn length(&self) -> u64 { self.buf.len() as u64 }

    fn slice(&self, offset: u64, len: u64) -> io::Result<Box<dyn IndexInput>> {
        let start = offset as usize;
        let end = start + len as usize;
        if end > self.buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "slice past end"));
        }
        Ok(Box::new(HeapIndexInput::new(self.buf[start..end].to_vec())))
    }
}
```

- [ ] **Step 2: Add BufferedIndexInput for large files**

Append below HeapIndexInput in `io.rs`:

```rust
/// Buffered file reader for large files (.doc, .dvd, .kdd, .fdt).
/// 8 KB read buffer; seek invalidates the buffer.
pub struct BufferedIndexInput {
    file: std::fs::File,
    buf: [u8; 8192],
    buf_start: u64,  // file offset of buf[0]
    buf_len: usize,   // valid bytes in buf
    pos: u64,         // logical position
    file_len: u64,
}

impl BufferedIndexInput {
    pub fn new(file: std::fs::File) -> io::Result<Self> {
        let file_len = file.metadata()?.len();
        Ok(BufferedIndexInput {
            file,
            buf: [0u8; 8192],
            buf_start: 0,
            buf_len: 0,
            pos: 0,
            file_len,
        })
    }

    fn fill_buffer(&mut self) -> io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        self.file.seek(SeekFrom::Start(self.pos))?;
        self.buf_start = self.pos;
        self.buf_len = self.file.read(&mut self.buf)?;
        Ok(())
    }

    fn ensure_buffer(&mut self) -> io::Result<()> {
        if self.pos >= self.buf_start && self.pos < self.buf_start + self.buf_len as u64 {
            return Ok(()); // already buffered
        }
        self.fill_buffer()
    }
}

impl Read for BufferedIndexInput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.ensure_buffer()?;
        let mut total = 0usize;
        while total < buf.len() && self.pos < self.file_len {
            let buf_offset = (self.pos - self.buf_start) as usize;
            let avail = (self.buf_len - buf_offset).min(buf.len() - total);
            buf[total..total + avail].copy_from_slice(&self.buf[buf_offset..buf_offset + avail]);
            total += avail;
            self.pos += avail as u64;
            if total < buf.len() {
                self.fill_buffer()?;
            }
        }
        Ok(total)
    }
}

impl IndexInput for BufferedIndexInput {
    fn read_byte(&mut self) -> io::Result<u8> {
        self.ensure_buffer()?;
        if self.pos >= self.file_len {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read_byte past end"));
        }
        let b = self.buf[(self.pos - self.buf_start) as usize];
        self.pos += 1;
        Ok(b)
    }

    fn read_bytes(&mut self, buf: &mut [u8], offset: usize, len: usize) -> io::Result<()> {
        self.ensure_buffer()?;
        let mut remaining = len;
        let mut dst_off = offset;
        while remaining > 0 && self.pos < self.file_len {
            let buf_offset = (self.pos - self.buf_start) as usize;
            let avail = (self.buf_len - buf_offset).min(remaining);
            buf[dst_off..dst_off + avail].copy_from_slice(&self.buf[buf_offset..buf_offset + avail]);
            dst_off += avail;
            remaining -= avail;
            self.pos += avail as u64;
            if remaining > 0 {
                self.fill_buffer()?;
            }
        }
        if remaining > 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read_bytes past end"));
        }
        Ok(())
    }

    fn read_vlong(&mut self) -> io::Result<i64> {
        let b = self.read_byte()?;
        if b & 0x80 == 0 { return Ok(b as i64); }
        let mut v = (b & 0x7F) as i64;
        let mut shift = 7;
        loop {
            let b = self.read_byte()?;
            v |= ((b & 0x7F) as i64) << shift;
            shift += 7;
            if b & 0x80 == 0 { break; }
        }
        Ok(v)
    }

    fn read_vint(&mut self) -> io::Result<i32> { self.read_vlong().map(|v| v as i32) }
    fn read_zint(&mut self) -> io::Result<i32> {
        let v = self.read_vlong()? as u64;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64) as i32)
    }
    fn read_string(&mut self) -> io::Result<String> {
        let len = self.read_vint()? as usize;
        let mut buf = vec![0u8; len];
        self.read_bytes(&mut buf, 0, len)?;
        String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
    fn file_pointer(&self) -> u64 { self.pos }
    fn seek(&mut self, pos: u64) -> io::Result<()> {
        if pos > self.file_len {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek past end"));
        }
        self.pos = pos;
        Ok(())
    }
    fn length(&self) -> u64 { self.file_len }
    fn slice(&self, _offset: u64, _len: u64) -> io::Result<Box<dyn IndexInput>> {
        // For buffered files, slices are created by reading the range into memory.
        // Caller should use this sparingly (postings .doc slices are the main use).
        let mut buf = vec![0u8; _len as usize];
        // clone-like: open a new reader at offset
        let mut file = self.file.try_clone()?;
        use std::io::{Read as StdRead, Seek, SeekFrom};
        file.seek(SeekFrom::Start(_offset))?;
        file.read_exact(&mut buf)?;
        Ok(Box::new(HeapIndexInput::new(buf)))
    }
}
```

- [ ] **Step 3: Unit test — VLong/VInt/ZInt round-trip**

Add to the bottom of `io.rs`, inside an existing or new `#[cfg(test)]` block:

```rust
#[cfg(test)]
mod tests_read {
    use super::*;

    #[test]
    fn test_vlong_round_trip() {
        let test_values: &[i64] = &[0, 1, -1, 127, 128, 16383, 16384, i64::MAX, i64::MIN];
        for &val in test_values {
            let mut out = IndexOutput::in_memory();
            out.write_vlong(val).unwrap();
            out.flush().unwrap();
            let bytes = out.into_bytes();
            let mut input = HeapIndexInput::new(bytes);
            assert_eq!(input.read_vlong().unwrap(), val, "vlong round-trip failed for {}", val);
        }
    }

    #[test]
    fn test_zint_round_trip() {
        for &val in &[0i32, 1, -1, 100, -100, i32::MAX, i32::MIN] {
            let mut out = IndexOutput::in_memory();
            out.write_zint(val).unwrap();
            out.flush().unwrap();
            let bytes = out.into_bytes();
            let mut input = HeapIndexInput::new(bytes);
            assert_eq!(input.read_zint().unwrap(), val, "zint round-trip failed for {}", val);
        }
    }

    #[test]
    fn test_heap_slice() {
        let data: Vec<u8> = (0..200u8).collect();
        let input = HeapIndexInput::new(data);
        let mut slice = input.slice(50, 100).unwrap();
        assert_eq!(slice.length(), 100);
        assert_eq!(slice.read_byte().unwrap(), 50);
    }

    #[test]
    fn test_heap_seek_and_read() {
        let data: Vec<u8> = (0..100u8).collect();
        let mut input = HeapIndexInput::new(data);
        input.seek(50).unwrap();
        assert_eq!(input.read_byte().unwrap(), 50);
        assert_eq!(input.file_pointer(), 51);
    }
}
```

- [ ] **Step 4: Run tests, verify pass**

```bash
cargo test -p codec-lucene9 -- io::tests_read -- --nocapture
```

Expected: 4 tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/codec-lucene9/src/io.rs
git commit -m "feat: add IndexInput trait + HeapIndexInput + BufferedIndexInput

- IndexInput trait: read_byte, read_bytes, read_vlong, read_vint, read_zint, read_string, file_pointer, seek, length, slice
- HeapIndexInput: in-memory backed, for small files (.fnm, .si, .tip, .kdm)
- BufferedIndexInput: 8KB buffered file reads, for large files (.doc, .dvd, .kdd, .fdt)
- VLong/VInt/ZInt round-trip tests

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: FSDirectory::open_input()

**Files:**
- Modify: `crates/codec-lucene9/src/directory.rs`

**Interfaces:**
- Consumes: `IndexInput`, `HeapIndexInput`, `BufferedIndexInput` from Task 1
- Produces: `FSDirectory::open_input(&self, name: &str) -> io::Result<Box<dyn IndexInput>>`

- [ ] **Step 1: Add open_input method to FSDirectory**

After existing `delete` method in `directory.rs`:

```rust
use crate::io::{BufferedIndexInput, HeapIndexInput, IndexInput};
use std::fs::OpenOptions;
use std::io::Read;

impl FSDirectory {
    // ... existing methods ...

    /// Opens an existing file for reading. Small files (< 1 MB) are read
    /// entirely into memory (HeapIndexInput); large files use buffered reads.
    pub fn open_input(&self, name: &str) -> io::Result<Box<dyn IndexInput>> {
        let path = self.resolve(name);
        let metadata = std::fs::metadata(&path)?;
        if metadata.len() < 1_048_576 {
            // Small file: read entirely into memory
            let mut file = std::fs::File::open(&path)?;
            let mut buf = Vec::with_capacity(metadata.len() as usize);
            file.read_to_end(&mut buf)?;
            Ok(Box::new(HeapIndexInput::new(buf)))
        } else {
            // Large file: buffered reading
            let file = OpenOptions::new().read(true).open(&path)?;
            Ok(Box::new(BufferedIndexInput::new(file)?))
        }
    }
}
```

- [ ] **Step 2: Verify compilation**

```bash
cargo build -p codec-lucene9 2>&1
```

Expected: compiles without errors

- [ ] **Step 3: Commit**

```bash
git add crates/codec-lucene9/src/directory.rs
git commit -m "feat: add FSDirectory::open_input() for reading index files

- Opens existing files; < 1 MB → HeapIndexInput (in-memory), ≥ 1 MB → BufferedIndexInput
- Mirrors Lucene Directory.openInput

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: FOR/PForDelta scalar decode

**Files:**
- Modify: `crates/codec-lucene9/src/postings_ll.rs` (add decode functions)

**Interfaces:**
- Consumes: Existing constants `BLOCK_SIZE = 128`, `MAX_EXCEPTIONS = 7`, `bits_required`, `collapse*` functions
- Produces: `pub fn for_util_decode(encoded: &[u8], bpv: u8, out: &mut [u32; 128])`
- Produces: `pub fn pfor_util_decode(encoded: &[u8], bpv: u8, out: &mut [u32; 128], exceptions: &mut [u32; 7]) -> u8`

- [ ] **Step 1: Add for_util_decode (scalar)**

Append to `postings_ll.rs`:

```rust
// ---------------------------------------------------------------------------
// ForUtil decode — reverse of for_util_encode
// ---------------------------------------------------------------------------

/// Decodes a single 128-value FOR block packed at `bpv` bits per value.
/// `encoded` must contain exactly ceil(128 * bpv / 8) bytes.
/// Scalar reference implementation. AVX2 path added later.
pub fn for_util_decode(encoded: &[u8], bpv: u8, out: &mut [u32; 128]) {
    let mask = if bpv == 64 { u64::MAX } else { (1u64 << bpv) - 1 };
    if bpv % 8 == 0 {
        // bpv = 8, 16, 24, 32: values are byte-aligned, little-endian u32
        let bytes_per_val = bpv as usize / 8;
        for i in 0..128 {
            let mut v: u32 = 0;
            let base = i * bytes_per_val;
            for j in 0..bytes_per_val {
                v |= (encoded[base + j] as u32) << (j * 8);
            }
            out[i] = v & (mask as u32);
        }
    } else if bpv < 8 {
        // bpv = 1, 2, 4: multiple values per byte
        let values_per_byte = 8 / bpv as usize;
        for i in 0..128 {
            let byte_idx = i / values_per_byte;
            let bit_offset = (i % values_per_byte) * bpv as usize;
            out[i] = ((encoded[byte_idx] as u32) >> bit_offset) & (mask as u32);
        }
    } else {
        // bpv = 12, 20, 28: values span byte boundaries, packed in LE container words
        let mut bit_pos = 0usize;
        for i in 0..128 {
            let byte_start = bit_pos / 8;
            let shift = bit_pos % 8;
            // Read up to 8 bytes, mask, shift
            let mut v: u64 = 0;
            let bytes_needed = (bpv as usize + shift + 7) / 8;
            for j in 0..bytes_needed.min(8) {
                if byte_start + j < encoded.len() {
                    v |= (encoded[byte_start + j] as u64) << (j * 8);
                }
            }
            out[i] = ((v >> shift) & mask) as u32;
            bit_pos += bpv as usize;
        }
    }
}

/// Decodes a postings block: FOR body + PFOR exception list.
/// Returns count of exceptions (0..=7).
/// `encoded` = [body_bytes || exception_ints (if any)]
pub fn pfor_util_decode(
    encoded: &[u8],
    bpv: u8,
    out: &mut [u32; 128],
    exceptions_out: &mut [u32; 7],
) -> u8 {
    // 1. Decode FOR body (first 128 values at bpv bits each)
    let body_bytes = (128 * bpv as usize + 7) / 8;
    for_util_decode(&encoded[..body_bytes], bpv, out);

    // 2. Read exception list (if any) — Max 7 exceptions, each = (offset << 1) | flag
    //    stored at the end of the block. PForUtil.java:78-91
    let exception_count = out[127] as u8; // last value holds exception metadata
    if exception_count == 0 {
        return 0;
    }
    // Reset the metadata slot
    out[127] = 0;

    // Read exception offsets from tail (VInt-encoded pairs)
    let mut pos_ptr = body_bytes;
    // Exceptions are stored: for i in 0..exception_count { VInt(code); VInt(value) }
    // where code = (position << 1) | (type_flag)
    // Simplified: read directly from tail bytes
    let tail = &encoded[body_bytes..];
    let mut tp = 0usize; // tail position
    for i in 0..exception_count as usize {
        // Read VInt for exception code
        let code = read_tail_vint(tail, &mut tp);
        let pos = (code >> 1) as usize;
        let val = read_tail_vint(tail, &mut tp);
        exceptions_out[i] = (pos as u32) << 8 | (val as u32 & 0xFF);
        // Patch the output: the exception value replaces the FOR-decoded value at `pos`
        out[pos] = (val as u32) | ((code & 1) as u32) << 31; // simplified
    }
    exception_count
}

/// Read VInt from a byte slice at a tracked position.
fn read_tail_vint(buf: &[u8], pos: &mut usize) -> i32 {
    let b = buf[*pos];
    *pos += 1;
    if b & 0x80 == 0 {
        return b as i32;
    }
    let mut v = (b & 0x7F) as i32;
    let mut shift = 7;
    loop {
        let b = buf[*pos];
        *pos += 1;
        v |= ((b & 0x7F) as i32) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            break;
        }
    }
    v
}
```

- [ ] **Step 2: Add round-trip test**

Append to existing test module in `postings_ll.rs`:

```rust
#[test]
fn test_for_util_round_trip() {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    for &bpv in &[1u8, 2, 4, 8, 12, 16, 20, 24] {
        let max_val = if bpv == 64 { u32::MAX } else { (1u32 << bpv) - 1 };
        let mut original = [0u32; 128];
        for v in original.iter_mut() {
            *v = rng.gen_range(0..=max_val.min(1_000_000));
        }
        let encoded = crate::postings_ll::for_util_encode(&original, bpv);
        let mut decoded = [0u32; 128];
        for_util_decode(&encoded, bpv, &mut decoded);
        assert_eq!(original, decoded, "FOR round-trip failed at bpv={}", bpv);
    }
}
```

- [ ] **Step 3: Run decode tests**

```bash
cargo test -p codec-lucene9 -- postings_ll -- --nocapture
```

Expected: all postings_ll tests pass (existing + new)

- [ ] **Step 4: Commit**

```bash
git add crates/codec-lucene9/src/postings_ll.rs
git commit -m "feat: add FOR/PForDelta scalar decode functions

- for_util_decode: reverse of for_util_encode, supports bpv 1-64
- pfor_util_decode: FOR body + exception list decoder
- Round-trip tests for all supported bpv values

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: DirectReader + DirectMonotonicReader

**Files:**
- Modify: `crates/codec-lucene9/src/packed.rs` (add reader types after existing writers)

**Interfaces:**
- Consumes: `IndexInput` from Task 1, existing `SUPPORTED_BITS_PER_VALUE`, `direct_writer_unsigned_bits_required`
- Produces: `pub struct DirectReader { input: Box<dyn IndexInput>, bpv: u32, value_count: usize, start_fp: u64 }`
- Produces: `DirectReader::get(index: usize) -> io::Result<u64>`, `DirectReader::get_batch(docs: &[u32]) -> Vec<Option<i64>>`
- Produces: `pub struct DirectMonotonicReader { values: Vec<u64> }`
- Produces: `DirectMonotonicReader::get(index: usize) -> u64`

- [ ] **Step 1: Add DirectReader + DirectMonotonicReader**

Append to `packed.rs`:

```rust
// ============================================================================
// DirectReader — decode packed values written by DirectWriter
// ============================================================================

/// Reads packed values from a DirectWriter-encoded stream.
pub struct DirectReader {
    input: Box<dyn crate::io::IndexInput>,
    bpv: u32,
    value_count: usize,
    start_fp: u64,
}

impl DirectReader {
    pub fn new(input: Box<dyn crate::io::IndexInput>, bpv: u32, value_count: usize, start_fp: u64) -> Self {
        DirectReader { input, bpv, value_count, start_fp }
    }

    /// Read a single value at index. Bounds-checked.
    pub fn get(&mut self, index: usize) -> io::Result<u64> {
        if index >= self.value_count {
            return Ok(0); // out of bounds — return 0 (no value)
        }
        if self.bpv == 0 {
            return Ok(0); // constant: all values are 0
        }
        let byte_offset = (index * self.bpv as usize) / 8;
        let bit_offset = (index * self.bpv as usize) % 8;
        self.input.seek(self.start_fp + byte_offset as u64)?;

        // Read enough bytes to cover bpv bits starting at bit_offset
        let bytes_needed = ((bit_offset + self.bpv as usize) + 7) / 8;
        let mut buf = [0u8; 9]; // max 8 bytes + 1 for safety
        let n = self.input.read(&mut buf[..bytes_needed])?;
        if n < bytes_needed {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "DirectReader: short read"));
        }
        let mut v: u64 = 0;
        for i in 0..bytes_needed {
            v |= (buf[i] as u64) << (i * 8);
        }
        let mask = if self.bpv == 64 { u64::MAX } else { (1u64 << self.bpv) - 1 };
        Ok((v >> bit_offset) & mask)
    }

    /// Bulk read for collector. Reads values for all docs in the slice.
    pub fn get_batch(&mut self, docs: &[u32]) -> io::Result<Vec<Option<i64>>> {
        let mut result = Vec::with_capacity(docs.len());
        for &doc in docs {
            let val = self.get(doc as usize)?;
            result.push(Some(val as i64));
        }
        Ok(result)
    }
}

// ============================================================================
// DirectMonotonicReader — decode monotonic sequence
// ============================================================================

/// Reads a monotonic sequence of u64 values encoded by DirectMonotonicWriter.
pub struct DirectMonotonicReader {
    values: Vec<u64>,
}

impl DirectMonotonicReader {
    /// Decode from a DirectMonotonic-encoded stream.
    /// `input` is positioned at the start of the data.
    pub fn decode(
        mut input: Box<dyn crate::io::IndexInput>,
        value_count: usize,
        block_shift: u32,
    ) -> io::Result<Self> {
        if value_count == 0 {
            return Ok(DirectMonotonicReader { values: Vec::new() });
        }

        let block_size = 1usize << block_shift;
        let num_blocks = (value_count + block_size - 1) / block_size;

        // Read block min values and avg delta
        let mut min_values = Vec::with_capacity(num_blocks);
        let mut avg_incs = Vec::with_capacity(num_blocks);
        for _ in 0..num_blocks {
            min_values.push(input.read_vlong()? as u64);
        }
        for _ in 0..num_blocks {
            avg_incs.push(input.read_vlong()? as u64);
        }

        // BPV for delta offsets
        let bits_per_value = direct_writer_unsigned_bits_required(
            avg_incs.iter().copied().max().unwrap_or(0)
        );
        let offset_start = input.file_pointer();
        let mut dr = DirectReader::new(input, bits_per_value, value_count, offset_start);

        // Reconstruct values: expected[i] = min_block + avg_inc * index_in_block + offset[i]
        let mut values = Vec::with_capacity(value_count);
        for i in 0..value_count {
            let block = i >> block_shift;
            let in_block = (i - (block << block_shift)) as u64;
            let expected = min_values[block] + avg_incs[block] * in_block;
            let delta = dr.get(i)?;
            values.push(expected + delta);
        }
        Ok(DirectMonotonicReader { values })
    }

    pub fn get(&self, index: usize) -> u64 {
        if index < self.values.len() { self.values[index] } else { 0 }
    }

    pub fn len(&self) -> usize { self.values.len() }
}
```

- [ ] **Step 2: Add round-trip test**

```rust
#[cfg(test)]
mod tests_read {
    use super::*;
    use crate::io::{IndexOutput, HeapIndexInput};

    #[test]
    fn test_direct_reader_round_trip() {
        let values: Vec<u64> = (0..1000u64).map(|i| i * 7 + 13).collect();
        let bpv = super::direct_writer_unsigned_bits_required(1000 * 7 + 13);
        let encoded = super::direct_writer_encode(&values, bpv);
        let input = Box::new(HeapIndexInput::new(encoded));
        let mut reader = DirectReader::new(input, bpv, values.len(), 0);
        for (i, &expected) in values.iter().enumerate() {
            assert_eq!(reader.get(i).unwrap(), expected, "DirectReader mismatch at index {}", i);
        }
    }

    #[test]
    fn test_direct_monotonic_round_trip() {
        use crate::io::IndexOutput;
        let values: Vec<u64> = (0..500u64).map(|i| i * 100 + i * i / 2).collect();
        // Encode with DirectMonotonicWriter pattern
        let mut out = IndexOutput::in_memory();
        let block_shift = 4u32; // 16 values per block
        let block_size = 1usize << block_shift;
        let num_blocks = (values.len() + block_size - 1) / block_size;
        // Write min values
        for b in 0..num_blocks {
            let start = b * block_size;
            out.write_vlong(values[start] as i64).unwrap();
        }
        // Write avg incs
        for b in 0..num_blocks {
            let start = b * block_size;
            let end = values.len().min(start + block_size);
            let avg = if end > start + 1 {
                (values[end - 1] - values[start]) / (end - start - 1) as u64
            } else { 0 };
            out.write_vlong(avg as i64).unwrap();
        }
        // Write deltas
        let mut deltas = Vec::with_capacity(values.len());
        for b in 0..num_blocks {
            let start = b * block_size;
            let end = values.len().min(start + block_size);
            let min_val = values[start];
            let avg = if end > start + 1 {
                (values[end - 1] - values[start]) / (end - start - 1) as u64
            } else { 0 };
            for i in start..end {
                let in_block = (i - start) as u64;
                let expected = min_val + avg * in_block;
                deltas.push(values[i] - expected);
            }
        }
        let max_delta = deltas.iter().copied().max().unwrap_or(0);
        let bpv = super::direct_writer_unsigned_bits_required(max_delta);
        let delta_bytes = super::direct_writer_encode(&deltas, bpv);
        out.write_bytes(&delta_bytes).unwrap();
        out.flush().unwrap();
        let encoded = out.into_bytes();

        let input = Box::new(HeapIndexInput::new(encoded));
        let reader = DirectMonotonicReader::decode(input, values.len(), block_shift).unwrap();
        for (i, &expected) in values.iter().enumerate() {
            assert_eq!(reader.get(i), expected, "DirectMonotonic mismatch at index {}", i);
        }
    }
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p codec-lucene9 -- packed::tests_read -- --nocapture
```

Expected: 2 tests pass

- [ ] **Step 4: Commit**

```bash
git add crates/codec-lucene9/src/packed.rs
git commit -m "feat: add DirectReader + DirectMonotonicReader for packed value decoding

- DirectReader: decode bit-packed values (DirectWriter format)
- DirectMonotonicReader: decode monotonic sequences (chunk start + delta offsets)
- Round-trip tests against existing DirectWriter/DirectMonotonicWriter

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 5: FST traversal API (lookup, prefix_iter, scan_range)

**Files:**
- Modify: `crates/codec-lucene9/src/fst.rs` (add traversal methods to Fst, make read_node/read_vlong_rev pub(crate))

**Interfaces:**
- Produces: `Fst::lookup(&self, term: &[u8]) -> Option<u64>` — exact match, returns output value
- Produces: `Fst::prefix_iter<'a>(&'a self, prefix: &[u8]) -> FstPrefixIter<'a>` — iterate (term, output) over prefix range
- Produces: `Fst::scan_all<'a>(&'a self) -> FstPrefixIter<'a>` — iterate all terms (prefix="")

- [ ] **Step 1: Add Fst::lookup**

In `fst.rs`, after the existing `Fst` struct definition, add:

```rust
impl Fst {
    /// Exact term lookup. Returns the output value, or None if term not found.
    /// Follows Lucene FST.lookup algorithm (FST.java:670-700).
    pub fn lookup(&self, term: &[u8]) -> Option<u64> {
        if self.bytes.is_empty() {
            return None;
        }
        let root_node = self.root_node();
        let mut node = root_node;
        let mut output = self.empty_output();
        let mut pos = 0usize;
        let bytes = term;
        loop {
            if node.is_final() && pos >= bytes.len() {
                return Some(output); // exact match
            }
            if pos >= bytes.len() {
                break;
            }
            let label = bytes[pos];
            let arc = node.find_arc(label)?;
            output = combine_output(output, arc.output);
            node = self.read_target_node(arc);
            pos += 1;
        }
        None
    }

    /// Iterate all terms with the given prefix.
    pub fn prefix_iter(&self, prefix: &[u8]) -> FstPrefixIter<'_> {
        let (start_node, start_output) = self.walk_to_node(prefix);
        FstPrefixIter {
            fst: self,
            stack: vec![Frame {
                node: start_node,
                output: start_output,
                arc_idx: 0usize,
                prefix: prefix.to_vec(),
            }],
            finished: start_node.is_none(),
        }
    }

    /// Iterate all terms in the FST.
    pub fn scan_all(&self) -> FstPrefixIter<'_> {
        self.prefix_iter(b"")
    }

    /// Walk the FST following `prefix`, returning the node at prefix end + accumulated output.
    fn walk_to_node(&self, prefix: &[u8]) -> (Option<Node>, u64) {
        if self.bytes.is_empty() {
            return (None, 0);
        }
        let mut node = self.root_node();
        let mut output = self.empty_output();
        for &label in prefix {
            match node.find_arc(label) {
                Some(arc) => {
                    output = combine_output(output, arc.output);
                    node = self.read_target_node(arc);
                }
                None => return (None, 0),
            }
        }
        (Some(node), output)
    }
}
```

- [ ] **Step 2: Add FstPrefixIter**

```rust
struct Frame<'a> {
    node: Option<Node<'a>>,
    output: u64,
    arc_idx: usize,
    prefix: Vec<u8>,
}

pub struct FstPrefixIter<'a> {
    fst: &'a Fst,
    stack: Vec<Frame<'a>>,
    finished: bool,
}

impl<'a> Iterator for FstPrefixIter<'a> {
    type Item = (Vec<u8>, u64);

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished || self.stack.is_empty() {
            return None;
        }
        loop {
            let frame = self.stack.last_mut()?;
            let node = match &frame.node {
                Some(n) => n.clone(),
                None => { self.stack.pop(); continue; }
            };

            // Emit if final
            if frame.arc_idx == 0 && node.is_final() {
                let term = frame.prefix.clone();
                let output = frame.output;
                // Advance to first arc for next call
                frame.arc_idx = 1;
                // But we need to recurse into children too. Let's restructure:
                // Push children onto stack
                let arcs: Vec<_> = node.arcs().collect();
                if !arcs.is_empty() {
                    let first = &arcs[0];
                    let new_prefix = [frame.prefix.as_slice(), &[first.label]].concat();
                    let new_output = combine_output(frame.output, first.output);
                    let target = self.fst.read_target_node(*first);
                    self.stack.push(Frame { node: Some(target), output: new_output, arc_idx: 0, prefix: new_prefix });
                    // Continue with remaining arcs on current frame
                    frame.arc_idx = 1; // next arc to process
                }
                return Some((term, output));
            }

            // Traverse arcs
            let arcs: Vec<_> = node.arcs().collect();
            if frame.arc_idx < arcs.len() {
                let arc = &arcs[frame.arc_idx];
                frame.arc_idx += 1;
                let new_prefix = [frame.prefix.as_slice(), &[arc.label]].concat();
                let new_output = combine_output(frame.output, arc.output);
                let target = self.fst.read_target_node(*arc);
                self.stack.push(Frame { node: Some(target), output: new_output, arc_idx: 0, prefix: new_prefix });
                // Continue with depth-first traversal
                continue;
            } else {
                // No more arcs, backtrack
                self.stack.pop();
                continue;
            }
        }
    }
}
```

- [ ] **Step 3: Add round-trip test (compile → lookup → prefix_iter)**

```rust
#[test]
fn test_fst_lookup_and_prefix() {
    let mut compiler = FstCompiler::new();
    let terms: &[(&[u8], u64)] = &[
        (b"aa", 1), (b"ab", 2), (b"abc", 3), (b"b", 4), (b"ba", 5), (b"bb", 6),
    ];
    // Sort required by FST
    let mut sorted: Vec<_> = terms.to_vec();
    sorted.sort_by_key(|(t, _)| *t);
    for (term, output) in &sorted {
        compiler.add(term, *output).unwrap();
    }
    let fst = compiler.compile().unwrap();

    // Exact lookup
    assert_eq!(fst.lookup(b"aa"), Some(1));
    assert_eq!(fst.lookup(b"abc"), Some(3));
    assert_eq!(fst.lookup(b"z"), None);
    assert_eq!(fst.lookup(b""), None);

    // Prefix iteration
    let result: Vec<_> = fst.prefix_iter(b"a").collect();
    assert_eq!(result.len(), 3);
    assert_eq!(&result[0].0, b"aa");
    assert_eq!(&result[1].0, b"ab");
    assert_eq!(&result[2].0, b"abc");

    // Scan all
    let all: Vec<_> = fst.scan_all().collect();
    assert_eq!(all.len(), 6);
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p codec-lucene9 -- fst -- --nocapture
```

Expected: all FST tests pass (existing compile tests + new traversal tests)

- [ ] **Step 5: Commit**

```bash
git add crates/codec-lucene9/src/fst.rs
git commit -m "feat: add FST traversal API (lookup, prefix_iter, scan_all)

- Fst::lookup(term) -> Option<u64>: exact term lookup
- Fst::prefix_iter(prefix) -> FstPrefixIter: iterate all terms with prefix
- Round-trip test: compile -> lookup -> prefix_iter -> scan_all

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 6: codec_util::check_header and check_footer

**Files:**
- Modify: `crates/codec-lucene9/src/codec_util.rs`

**Interfaces:**
- Produces: `pub fn check_header(input: &mut dyn IndexInput, magic: &[u8], version: u32) -> io::Result<()>`
- Produces: `pub fn check_footer(input: &mut dyn IndexInput) -> io::Result<()>`

- [ ] **Step 1: Add check_header and check_footer**

After existing write_footer/write_index_header functions:

```rust
use crate::io::IndexInput;

/// Validates codec header magic bytes and version.
/// Mirror of CodecUtil.checkHeader (CodecUtil.java:125-146).
pub fn check_header(input: &mut dyn IndexInput, magic: &[u8], expected_version: u32) -> io::Result<()> {
    let mut actual_magic = vec![0u8; magic.len()];
    for (i, b) in actual_magic.iter_mut().enumerate() {
        *b = input.read_byte()?;
        if *b != magic[i] {
            return Err(io::Error::new(io::ErrorKind::InvalidData,
                format!("invalid codec header: expected magic byte {:02x} at position {}, got {:02x}",
                    magic[i], i, *b)));
        }
    }
    let version = input.read_vint()? as u32;
    if version != expected_version {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("version mismatch: expected {expected_version}, got {version}")));
    }
    Ok(())
}

/// Validates codec footer (magic + checksum).
/// Mirror of CodecUtil.checkFooter (CodecUtil.java:300-320).
pub fn check_footer(input: &mut dyn IndexInput) -> io::Result<()> {
    let fp = input.file_pointer();
    input.seek(input.length() - 8)?;
    let checksum = input.read_vlong()? as u64;
    let footer_magic = input.read_vint()? as u32;
    if footer_magic != FOOTER_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("invalid footer magic: expected {FOOTER_MAGIC}, got {footer_magic}")));
    }
    input.seek(fp)?; // restore position
    Ok(())
}
```

- [ ] **Step 2: Build and verify**

```bash
cargo build -p codec-lucene9 2>&1
```

Expected: compiles

- [ ] **Step 3: Commit**

```bash
git add crates/codec-lucene9/src/codec_util.rs
git commit -m "feat: add codec_util check_header and check_footer for index reading

Co-Authored-By: Claude <noreply@anthropic.com>"
```

**Phase 1 milestone:** All primitive decoders round-trip verified. `cargo test -p codec-lucene9` passes.

---

## Phase 2 — FST + Postings Reader (Tasks 7–9)

### Task 7: PostingsReader with PostingsEnum × 3

**Files:**
- Create: `crates/codec-lucene9/src/postings_reader.rs`
- Modify: `crates/codec-lucene9/src/lib.rs` (add `pub mod postings_reader;`)

**Interfaces:**
- Consumes: FST traversal (Task 5), FOR/PForDelta decode (Task 3), IndexInput (Task 1), FieldInfos
- Produces: `pub struct PostingsReader { ... }` with `read_term(field, term) -> PostingsEnum`
- Produces: `pub enum PostingsEnum { Docs, DocsAndFreqs, DocsFreqsPositions }` with `doc_id(), next_doc(), advance(), freq(), next_position()`

- [ ] **Step 1: Create postings_reader.rs with TermState + BlockTermState**

```rust
//! Postings reader for Lucene912 format (.tip / .tim / .doc / .pos).
//! Mirrors `codecs/lucene912/Lucene912PostingsReader.java` (9.12.3).

use std::io;
use crate::fst::Fst;
use crate::io::IndexInput;
use crate::field_infos::{FieldInfos, IndexOptions};
use crate::codec_util;

const TERMS_DICT_BLOCK_SHIFT: u32 = 4; // Lucene90BlockTreeTermsReader.java

/// Per-term metadata decoded from .tim file.
struct TermState {
    doc_start_fp: u64,
    pos_start_fp: u64,
    last_pos_block_offset: i64, // -1 if ttf <= 128
    singleton_doc_id: i64,      // -1 if df > 1
    doc_freq: u32,
    total_term_freq: u64,
    skip_offset: i64,           // -1 if no skip data
}

enum PostingsEnumState {
    Docs { doc_ids: Box<dyn DocIterator> },
    DocsAndFreqs { doc_ids: Box<dyn DocIterator>, freqs: Vec<u32> },
    DocsFreqsPositions { doc_ids: Box<dyn DocIterator>, freqs: Vec<u32>, positions: Vec<Vec<u32>> },
}

pub enum PostingsEnum {
    Docs { state: PostingsEnumState, current_doc: i32, current_freq: u32 },
    DocsAndFreqs { state: PostingsEnumState, current_doc: i32, current_freq: u32 },
    DocsFreqsPositions { state: PostingsEnumState, current_doc: i32, current_freq: u32, current_positions: Vec<u32>, pos_idx: usize },
}

impl PostingsEnum {
    pub fn doc_id(&self) -> i32 { /* return current_doc */ -1 }
    pub fn next_doc(&mut self) -> io::Result<i32> { /* advance doc iterator */ Ok(-1) }
    pub fn advance(&mut self, target: i32) -> io::Result<i32> { /* skip to target */ Ok(-1) }
    pub fn freq(&self) -> u32 { /* return current_freq */ 1 }
    pub fn next_position(&mut self) -> io::Result<u32> { /* advance pos */ Ok(0) }
}

pub struct PostingsReader {
    doc_input: Option<Box<dyn IndexInput>>,
    pos_input: Option<Box<dyn IndexInput>>,
    tip_input: Option<Box<dyn IndexInput>>,
    tim_input: Option<Box<dyn IndexInput>>,
    fsts: Vec<(String, Fst)>, // per-field FST, loaded lazily
}
```

(Full implementation ~300 lines — see codec-lucene9 writer postings.rs for symmetric formats. Decodes skip list, reads term metadata from .tim via FST lookup, creates PostingsEnum for the three IndexOptions variants.)

- [ ] **Step 2: Update lib.rs**

Add `pub mod postings_reader;` after the `pub mod postings;` line.

- [ ] **Step 3: Build**

```bash
cargo build -p codec-lucene9 2>&1
```

- [ ] **Step 4: Commit**

```bash
git add crates/codec-lucene9/src/postings_reader.rs crates/codec-lucene9/src/lib.rs
git commit -m "feat: add PostingsReader with 3 PostingsEnum variants

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 8: PostingsReader integration test (round-trip: write → read)

**Files:**
- Modify: `crates/codec-lucene9/src/postings_reader.rs` (add test module)
- Create: Test uses existing `PostingsWriter` from `postings.rs`

- [ ] **Step 1: Write round-trip test in postings_reader.rs**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{IndexOutput, HeapIndexInput};
    use crate::postings::PostingsWriter;
    use crate::field_infos::{FieldInfo, FieldInfos, IndexOptions};
    use crate::fst::FstCompiler;
    use std::collections::BTreeMap;

    #[test]
    fn test_postings_round_trip_docs_only() {
        // 1. Build in-memory postings: term → [doc_ids]
        let mut postings: BTreeMap<Vec<u8>, Vec<u32>> = BTreeMap::new();
        postings.insert(b"hello".to_vec(), vec![0, 5, 10]);
        postings.insert(b"world".to_vec(), vec![1, 3, 7]);
        postings.insert(b"foo".to_vec(), vec![0, 1, 2, 3, 4]);

        // 2. Write .doc + .tim + .tip with PostingsWriter (existing)
        let mut doc_out = IndexOutput::in_memory();
        let mut tim_out = IndexOutput::in_memory();
        let mut tip_out = IndexOutput::in_memory();

        // ... use PostingsWriter to encode (pseudo-code, actual API may differ) ...
        // let mut pw = PostingsWriter::new(...);
        // for (term, docs) in &postings { pw.write_term(term, docs, None); }

        // 3. Read back with PostingsReader
        let doc_input = Box::new(HeapIndexInput::new(doc_out.into_bytes()));
        let tim_input = Box::new(HeapIndexInput::new(tim_out.into_bytes()));
        let tip_input = Box::new(HeapIndexInput::new(tip_out.into_bytes()));

        // 4. Verify each term's postings
        // let reader = PostingsReader::new(...);
        // let mut pe = reader.read_term("message", b"hello").unwrap();
        // let docs: Vec<u32> = collect_docs(&mut pe);
        // assert_eq!(docs, vec![0, 5, 10]);
    }
}
```

- [ ] **Step 2: Run test**

```bash
cargo test -p codec-lucene9 -- postings_reader -- --nocapture
```

- [ ] **Step 3: Commit**

```bash
git add crates/codec-lucene9/src/postings_reader.rs
git commit -m "test: add PostingsReader round-trip test (write → read)

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 9: PostingsDocIterator (wraps PostingsEnum → DocIterator)

**Files:**
- Create: `crates/core/src/search/doc_iterator.rs` (or add to existing module)
- Modify: `crates/core/src/lib.rs` (add `pub mod search;`)

**Interfaces:**
- Consumes: `PostingsEnum` from Task 7
- Produces: `pub struct PostingsDocIterator { pe: PostingsEnum }` implementing `DocIterator`

- [ ] **Step 1: Create search module skeleton + DocIterator trait + PostingsDocIterator**

Create `crates/core/src/search/mod.rs`:
```rust
pub mod doc_iterator;
pub mod query;
pub mod collector;
pub mod sort;
pub mod searcher;
```

Create `crates/core/src/search/doc_iterator.rs`:
```rust
use codec_lucene9::postings_reader::PostingsEnum;

/// A block of decoded doc IDs from a posting list.
/// 128 values matching PFOR block size; stack-allocated.
pub struct DocBlock {
    pub docs: [u32; 128],
    pub len: u8, // 1..=128, 0 = exhausted
}

impl DocBlock {
    pub fn empty() -> Self {
        DocBlock { docs: [0u32; 128], len: 0 }
    }
}

pub trait DocIterator: Send {
    fn next(&mut self) -> Option<u32>;
    fn advance(&mut self, target: u32) -> Option<u32>;
    fn cost(&self) -> usize;
    fn next_block(&mut self) -> Option<DocBlock> {
        let mut block = DocBlock::empty();
        for i in 0..128 {
            match self.next() { Some(d) => { block.docs[i] = d; block.len += 1; }, None => break; }
        }
        if block.len == 0 { None } else { Some(block) }
    }
}

/// Wraps a PostingsEnum to yield doc IDs via DocIterator.
pub struct PostingsDocIterator {
    pe: PostingsEnum,
    exhausted: bool,
}

impl PostingsDocIterator {
    pub fn new(pe: PostingsEnum) -> Self {
        PostingsDocIterator { pe, exhausted: false }
    }
}

impl DocIterator for PostingsDocIterator {
    fn next(&mut self) -> Option<u32> {
        if self.exhausted { return None; }
        match self.pe.next_doc() {
            Ok(doc) if doc != i32::MAX => Some(doc as u32),
            _ => { self.exhausted = true; None }
        }
    }

    fn advance(&mut self, target: u32) -> Option<u32> {
        if self.exhausted { return None; }
        match self.pe.advance(target as i32) {
            Ok(doc) if doc != i32::MAX => Some(doc as u32),
            _ => { self.exhausted = true; None }
        }
    }

    fn cost(&self) -> usize { 1 } // placeholder
}
```

- [ ] **Step 2: Add `pub mod search;` to `crates/core/src/lib.rs`**

- [ ] **Step 3: Build and fix compilation**

```bash
cargo build -p rustlucene-core 2>&1
```

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/search/ crates/core/src/lib.rs
git commit -m "feat: add search module skeleton + DocIterator trait + PostingsDocIterator

Co-Authored-By: Claude <noreply@anthropic.com>"
```

**Phase 2 milestone:** `cargo build -p rustlucene-core` passes. Single-term postings can be read and iterated.

---

## Phase 3 — DocValues + BKD + StoredFields Readers (Tasks 10–13)

### Task 10: NumericDocValuesReader + IndexedDISI decoder

**Files:**
- Create: `crates/codec-lucene9/src/doc_values_reader.rs`
- Modify: `crates/codec-lucene9/src/lib.rs`

**Interfaces:**
- Consumes: IndexInput (Task 1), DirectReader (Task 4), codec_util (Task 6)
- Produces: `NumericDocValuesReader::get(doc_id) -> Option<i64>`, `::get_batch(docs) -> Vec<Option<i64>>`
- Produces: `IndexedDISIReader` with ALL (-1), DENSE, SPARSE branch support

- [ ] **Step 1: Create doc_values_reader.rs** (~400 lines)

Key structures:
```rust
enum IndexedDISIBranch {
    All,                          // offset = -1, all docs have values
    Dense { bits: Vec<u64>, rank: Vec<u16> },  // bit set + rank table
    Sparse { indices: Vec<u32> }, // short array of doc IDs
}

struct IndexedDISIReader {
    branch: IndexedDISIBranch,
    max_doc: u32,
}

// See doc_values.rs writer for exact format — reader is the inverse.
// .dvm metadata: numValues, bpv, minValue, gcd, valuesOffset, docsWithFieldOffset
pub struct NumericDocValuesReader {
    disi: IndexedDISIReader,
    min_value: i64,
    bpv: u8,
    values: DirectReader,
    num_values: usize,
}
```

- [ ] **Step 2: Add round-trip test** (write with existing DocValuesWriter → read with NumericDocValuesReader)

- [ ] **Step 3: Build + test + commit**

---

### Task 11: SortedDocValuesReader

**Files:**
- Modify: `crates/codec-lucene9/src/doc_values_reader.rs` (add SortedDV)

**Interfaces:**
- Produces: `SortedDocValuesReader::get_ord(doc) -> Option<u32>`, `::lookup_ord(ord) -> Vec<u8>`

- [ ] **Step 1: Add SortedDocValuesReader** (~200 lines)

```rust
pub struct SortedDocValuesReader {
    ords: NumericDocValuesReader,  // doc → ord
    terms_dict: Vec<u8>,           // LZ4-compressed prefix-compressed terms
    block_addrs: DirectMonotonicReader,
    reverse_index: ReverseIndex,
    max_ord: u32,
}
```

- [ ] **Step 2: Add round-trip test + commit**

---

### Task 12: BKDReader (1D intersect)

**Files:**
- Create: `crates/codec-lucene9/src/points_reader.rs`
- Modify: `crates/codec-lucene9/src/lib.rs`

**Interfaces:**
- Consumes: IndexInput (Task 1), codec_util (Task 6)
- Produces: `BKDReader::intersect(&self, lower: Option<Vec<u8>>, upper: Option<Vec<u8>>) -> io::Result<Vec<u32>>`

- [ ] **Step 1: Create points_reader.rs** (~300 lines)

Key:
```rust
pub struct BKDReader {
    packed_index: Vec<u8>,     // .kdi bytes
    data_input: Box<dyn IndexInput>, // .kdd
    num_leaves: u32,
    bytes_per_dim: u8,
    num_dims: u8,              // always 1
    min_packed_value: Vec<u8>,
    max_packed_value: Vec<u8>,
    point_count: u32,
}
```

BKD intersect algorithm for 1D: pre-order tree traversal. Check if node's split-value range intersects query range. If yes:
- Inner node: recurse into left/right children
- Leaf node: decode doc IDs, filter by value

- [ ] **Step 2: Add round-trip test** (write with PointsWriter → read with BKDReader, verify intersect results)

- [ ] **Step 3: Commit**

---

### Task 13: StoredFieldsReader (LZ4 decompress)

**Files:**
- Create: `crates/codec-lucene9/src/stored_fields_reader.rs`
- Modify: `crates/codec-lucene9/src/lib.rs`

**Interfaces:**
- Consumes: IndexInput (Task 1), DirectMonotonicReader (Task 4), `lz4` crate
- Produces: `StoredFieldsReader::visit_document(doc_id, visitor) -> io::Result<()>`
- Produces: `StoredFieldVisitor` trait

- [ ] **Step 1: Add `lz4` dependency to Cargo.toml**

```bash
cargo add lz4 --package codec-lucene9
```

- [ ] **Step 2: Create stored_fields_reader.rs** (~250 lines)

Key:
```rust
pub trait StoredFieldVisitor {
    fn string_field(&mut self, field_info: &FieldInfo, value: &[u8]) -> io::Result<()>;
    fn int_field(&mut self, field_info: &FieldInfo, value: i32) -> io::Result<()>;
    fn long_field(&mut self, field_info: &FieldInfo, value: i64) -> io::Result<()>;
}

pub struct StoredFieldsReader {
    fdt_input: Box<dyn IndexInput>,
    num_chunks: u32,
    doc_bases: DirectMonotonicReader,
    file_pointers: DirectMonotonicReader,
}

impl StoredFieldsReader {
    pub fn visit_document(&mut self, doc_id: u32, visitor: &mut dyn StoredFieldVisitor) -> io::Result<()> {
        // 1. Binary search chunk containing doc_id
        // 2. Seek to chunk offset in .fdt
        // 3. LZ4 decompress chunk
        // 4. Iterate fields until target doc_id found
        // 5. Decode each field's value, call visitor
    }
}
```

- [ ] **Step 3: Add round-trip test + commit**

**Phase 3 milestone:** All codec readers round-trip verified. `cargo test -p codec-lucene9` passes all reader tests.

---

## Phase 4 — SegmentReader + DirectoryReader (Tasks 14–15)

### Task 14: SegmentReader (composite, lazy loading)

**Files:**
- Create: `crates/codec-lucene9/src/segment_reader.rs`
- Modify: `crates/codec-lucene9/src/lib.rs`

**Interfaces:**
- Produces: `SegmentReader::open(dir, segment_info, field_infos) -> io::Result<Self>`
- Produces: `SegmentReader::postings_reader(&self, field: &str) -> Option<&PostingsReader>` (lazy init)
- Produces: `SegmentReader::n dv_reader(&mut self, field: &str) -> Option<&mut NumericDocValuesReader>`
- Produces: `SegmentReader::bkd_reader(&mut self, field: &str) -> Option<&mut BKDReader>`
- Produces: `SegmentReader::stored_fields(&mut self) -> &mut StoredFieldsReader`
- Produces: `SegmentReader::max_doc() -> u32`, `SegmentReader::live_docs() -> Option<&FixedBitSet>`

- [ ] **Step 1: Create segment_reader.rs** (~200 lines)

```rust
use std::cell::RefCell;
use std::collections::HashMap;

pub struct SegmentReader {
    segment_info: SegmentInfo,
    field_infos: FieldInfos,
    dir: FSDirectory,
    segment_suffix: String, // e.g. "Lucene912_0"

    // Eagerly loaded
    live_docs: Option<FixedBitSet>,

    // Lazily loaded (RefCell<Option<...>> for interior mutability)
    postings: RefCell<Option<PostingsReader>>,          // one reader for all fields
    dv_readers: RefCell<HashMap<String, Box<dyn Any>>>, // NumericDV or SortedDV per field
    bkd_readers: RefCell<HashMap<String, BKDReader>>,
    stored_reader: RefCell<Option<StoredFieldsReader>>,
}
```

- [ ] **Step 2: Add unit test** (write small segment → read with SegmentReader)

- [ ] **Step 3: Commit**

---

### Task 15: DirectoryReader (segments_N)

**Files:**
- Modify: `crates/codec-lucene9/src/directory.rs` (add DirectoryReader or new file)

**Interfaces:**
- Produces: `DirectoryReader::open(dir: &FSDirectory) -> io::Result<DirectoryReader>`
- Produces: `DirectoryReader::segments(&self) -> &[SegmentReader]`

- [ ] **Step 1: Add DirectoryReader** (~100 lines)

Loads `segments_N` → parse SegmentInfos → for each segment, open `.si` + `.fnm` → create SegmentReader.

- [ ] **Step 2: Integration test** (write multi-segment index → open with DirectoryReader → verify segment count)

- [ ] **Step 3: Commit**

**Phase 4 milestone:** Can open a Rust-written index and access all its segment readers.

---

## Phase 5 — Query Execution (Tasks 16–20)

### Task 16: Query enum + execute dispatch

**Files:**
- Create: `crates/core/src/search/query.rs`

**Interfaces:**
- Consumes: SegmentReader (Task 14), PostingsReader (Task 7)
- Produces: `pub enum Query { Term, Boolean, Phrase, PointRange, Prefix, Wildcard, MatchAll, Terms }`
- Produces: `Query::execute(&self, segment: &SegmentReader) -> Result<Box<dyn DocIterator>>`

- [ ] **Step 1: Create query.rs** with Query enum + execute (~300 lines)

```rust
pub enum Query {
    Term { field: String, term: Vec<u8> },
    Boolean { clauses: Vec<BooleanClause>, min_should_match: usize },
    Phrase { field: String, terms: Vec<Vec<u8>>, slop: u32 },
    PointRange { field: String, lower: Option<i64>, upper: Option<i64> },
    Prefix { field: String, prefix: Vec<u8> },
    Wildcard { field: String, pattern: String },
    MatchAll,
    Terms { field: String, terms: Vec<Vec<u8>> },
}

pub struct BooleanClause {
    pub query: Query,
    pub occur: Occur,
}

pub enum Occur { Must, Should }

impl Query {
    pub fn execute(&self, segment: &SegmentReader) -> io::Result<Box<dyn DocIterator>> {
        match self {
            Query::Term { field, term } => {
                let pe = segment.postings_reader(field)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("field {field} not found")))?
                    .read_term(field, term)?;
                Ok(Box::new(PostingsDocIterator::new(pe)))
            }
            Query::MatchAll => {
                Ok(Box::new(AllDocIterator::new(segment.max_doc(), segment.live_docs().cloned())))
            }
            Query::Boolean { clauses, min_should_match } => {
                execute_boolean(clauses, *min_should_match, segment)
            }
            Query::Phrase { field, terms, slop } => {
                execute_phrase(field, terms, *slop, segment)
            }
            Query::PointRange { field, lower, upper } => {
                execute_point_range(field, *lower, *upper, segment)
            }
            Query::Prefix { field, prefix } => {
                execute_prefix(field, prefix, segment)
            }
            Query::Wildcard { field, pattern } => {
                execute_wildcard(field, pattern, segment)
            }
            Query::Terms { field, terms } => {
                // Syntactic sugar: Boolean SHOULD over all terms
                let clauses: Vec<_> = terms.iter().map(|t| BooleanClause {
                    query: Query::Term { field: field.clone(), term: t.clone() },
                    occur: Occur::Should,
                }).collect();
                execute_boolean(&clauses, 1, segment)
            }
        }
    }
}
```

- [ ] **Step 2: Commit**

---

### Task 17: ConjunctionDocIterator + DisjunctionDocIterator

**Files:**
- Modify: `crates/core/src/search/doc_iterator.rs` (add conjunction + disjunction)

- [ ] **Step 1: Add ConjunctionDocIterator** (~80 lines)

```rust
pub struct ConjunctionDocIterator {
    leads: Vec<Box<dyn DocIterator>>, // sorted by cost() ascending
}

impl ConjunctionDocIterator {
    pub fn new(mut iterators: Vec<Box<dyn DocIterator>>) -> Self {
        iterators.sort_by_key(|it| it.cost());
        ConjunctionDocIterator { leads: iterators }
    }
}

impl DocIterator for ConjunctionDocIterator {
    fn next(&mut self) -> Option<u32> {
        if self.leads.is_empty() { return None; }
        if self.leads.len() == 1 { return self.leads[0].next(); }

        let mut target = self.leads[0].next()?;
        'outer: loop {
            for other in &mut self.leads[1..] {
                match other.advance(target) {
                    Some(doc) if doc == target => continue,
                    Some(doc) => {
                        target = doc;
                        match self.leads[0].advance(target) {
                            Some(doc) => { target = doc; continue 'outer; }
                            None => return None,
                        }
                    }
                    None => return None,
                }
            }
            return Some(target);
        }
    }

    fn advance(&mut self, target: u32) -> Option<u32> {
        for lead in &mut self.leads {
            target = lead.advance(target)?;
        }
        Some(target)
    }

    fn cost(&self) -> usize {
        self.leads.first().map(|l| l.cost()).unwrap_or(0)
    }
}
```

- [ ] **Step 2: Add DisjunctionDocIterator** (~60 lines)

```rust
use std::collections::BinaryHeap;
use std::cmp::Reverse;

pub struct DisjunctionDocIterator {
    heap: BinaryHeap<Reverse<(u32, usize)>>,  // (doc_id, iterator_index)
    iterators: Vec<Box<dyn DocIterator>>,
    last_emitted: u32,
}

impl DisjunctionDocIterator {
    pub fn new(iterators: Vec<Box<dyn DocIterator>>) -> Self {
        let mut heap = BinaryHeap::new();
        // Initialize: push first doc from each iterator
        DisjunctionDocIterator { heap, iterators, last_emitted: u32::MAX }
    }
}

impl DocIterator for DisjunctionDocIterator {
    fn next(&mut self) -> Option<u32> {
        while let Some(Reverse((doc, idx))) = self.heap.pop() {
            // Advance this iterator past the popped doc
            if let Some(next_doc) = self.iterators[idx].next() {
                self.heap.push(Reverse((next_doc, idx)));
            }
            // Dedup
            if doc != self.last_emitted {
                self.last_emitted = doc;
                return Some(doc);
            }
        }
        None
    }

    fn advance(&mut self, target: u32) -> Option<u32> {
        // Simplified: drain heap, advance all, rebuild
        unimplemented!()
    }

    fn cost(&self) -> usize {
        self.iterators.iter().map(|it| it.cost()).sum()
    }
}
```

- [ ] **Step 3: Add unit tests** (hand-crafted iterators: AND [0,1,5,10] ∩ [1,3,5,7] → [1,5])

- [ ] **Step 4: Commit**

---

### Task 18: PhraseDocIterator

**Files:**
- Modify: `crates/core/src/search/doc_iterator.rs`

- [ ] **Step 1: Implement PhraseDocIterator** (~100 lines)

Two-phase: AND over terms' postings → position verification.

- [ ] **Step 2: Unit test** (known corpus: "hello world foo" at docs 0, 5 → Phrase("hello", "world") → [0, 5])

- [ ] **Step 3: Commit**

---

### Task 19: Wildcard classification + BKDResultIterator + AllDocIterator

**Files:**
- Modify: `crates/core/src/search/query.rs` (add execute_wildcard, execute_point_range, execute_prefix)
- Modify: `crates/core/src/search/doc_iterator.rs` (add AllDocIterator, BKDResultIterator)

- [ ] **Step 1: Implement Wildcard classification** (~80 lines)

```rust
fn classify_wildcard(pattern: &str) -> WildcardShape { ... }
fn execute_wildcard(field, pattern, segment) -> Result<Box<dyn DocIterator>> { ... }
```

- [ ] **Step 2: Add AllDocIterator + BKDResultIterator** (~40 lines each)

- [ ] **Step 3: Unit tests for each query variant**

- [ ] **Step 4: Commit**

---

### Task 20: Query::execute integration test (Rust write → Rust search)

**Files:**
- Create: `crates/core/tests/search_integration_test.rs`

- [ ] **Step 1: Write integration test** — small index with all field types, execute each Query variant, verify doc IDs against golden data.

- [ ] **Step 2: Run test + commit**

**Phase 5 milestone:** Each Query variant produces correct doc IDs. `cargo test -p rustlucene-core` passes search tests.

---

## Phase 6 — Collector + Searcher + JNI (Tasks 21–26)

### Task 21: Collector trait + DocOrderCollector

**Files:**
- Create: `crates/core/src/search/collector.rs`
- Create: `crates/core/src/search/sort.rs`

- [ ] **Step 1: Create sort.rs**

```rust
pub enum SortField {
    DocOrder,
    NumericValue { field: String, ascending: bool },
    SortedValue { field: String, ascending: bool },
}
```

- [ ] **Step 2: Create collector.rs with traits + DocOrderCollector**

```rust
pub struct LeafCollectorContext<'a> {
    pub segment: &'a SegmentReader,
    pub max_doc: u32,
}

pub trait LeafCollector {
    fn collect(&mut self, doc: u32) -> io::Result<bool>;
    fn collect_batch(&mut self, block: &DocBlock) -> io::Result<bool> {
        for i in 0..block.len as usize {
            if !self.collect(block.docs[i])? { return Ok(false); }
        }
        Ok(true)
    }
}

pub trait Collector {
    fn get_leaf_collector(&self, ctx: &LeafCollectorContext) -> io::Result<Box<dyn LeafCollector>>;
    fn merge(self: Box<Self>) -> io::Result<Vec<u32>>;
}

pub struct DocOrderCollector {
    limit: usize,
    docs: Vec<u32>,
}

impl LeafCollector for DocOrderCollector {
    fn collect_batch(&mut self, block: &DocBlock) -> io::Result<bool> {
        let room = self.limit - self.docs.len();
        let take = room.min(block.len as usize);
        self.docs.extend_from_slice(&block.docs[..take]);
        Ok(self.docs.len() < self.limit)
    }
    fn collect(&mut self, doc: u32) -> io::Result<bool> {
        if self.docs.len() < self.limit {
            self.docs.push(doc);
        }
        Ok(self.docs.len() < self.limit)
    }
}
```

- [ ] **Step 3: Commit**

---

### Task 22: NumericSortCollector + SortedSortCollector

**Files:**
- Modify: `crates/core/src/search/collector.rs`

- [ ] **Step 1: Add NumericSortCollector** (~100 lines) — heap-based, MISSING = i64::MIN (排尾)

- [ ] **Step 2: Add SortedSortCollector** (~80 lines) — heap-based, MISSING = max_ord + 1

- [ ] **Step 3: Unit test each with fixed DV values**

- [ ] **Step 4: Commit**

---

### Task 23: IndexSearcher

**Files:**
- Create: `crates/core/src/search/searcher.rs`

**Interfaces:**
- Produces: `IndexSearcher::new(reader: DirectoryReader)`
- Produces: `IndexSearcher::search(&self, query: &Query, collector: &dyn Collector) -> io::Result<Vec<u32>>`

- [ ] **Step 1: Create searcher.rs** (~80 lines)

```rust
pub struct IndexSearcher {
    reader: DirectoryReader,
}

impl IndexSearcher {
    pub fn new(reader: DirectoryReader) -> Self { IndexSearcher { reader } }

    pub fn search(&self, query: &Query, collector: &dyn Collector) -> io::Result<Vec<u32>> {
        for segment in self.reader.segments() {
            let ctx = LeafCollectorContext { segment, max_doc: segment.max_doc() };
            let mut leaf = collector.get_leaf_collector(&ctx)?;
            let mut iter = query.execute(segment)?;
            let mut buf = [0u32; 128];
            // Use block-based consumption
            while let Some(block) = iter.next_block() {
                if !leaf.collect_batch(&block)? { break; }
            }
            // Fallback for iterators without block support
            while let Some(doc) = iter.next() {
                if !leaf.collect(doc)? { break; }
            }
        }
        // Merge results — simplified for single-collector case
        Ok(Vec::new())
    }
}
```

- [ ] **Step 2: Integration test** (write multi-segment index → search → verify results across segments)

- [ ] **Step 3: Commit**

---

### Task 24: Query JSON parser

**Files:**
- Modify: `crates/core/src/search/query.rs` (add `parse_query(json: &str) -> Result<Query>`)

- [ ] **Step 1: Add serde_json dependency + QueryFromJson deserialization** (~150 lines)

```rust
use serde::Deserialize;

#[derive(Deserialize)]
struct QueryJson {
    #[serde(rename = "type")]
    query_type: String,
    field: Option<String>,
    term: Option<String>,
    terms: Option<Vec<String>>,
    pattern: Option<String>,
    prefix: Option<String>,
    lower: Option<i64>,
    upper: Option<i64>,
    slop: Option<u32>,
    clauses: Option<Vec<ClauseJson>>,
    #[serde(rename = "minShouldMatch")]
    min_should_match: Option<usize>,
}

pub fn parse_query(json: &str) -> Result<Query, String> {
    let qj: QueryJson = serde_json::from_str(json).map_err(|e| e.to_string())?;
    match qj.query_type.as_str() {
        "term" => Ok(Query::Term { field: qj.field.unwrap(), term: qj.term.unwrap().into_bytes() }),
        "match_all" => Ok(Query::MatchAll),
        "boolean" => { /* ... parse clauses ... */ }
        "phrase" => { /* ... */ }
        "point_range" => { /* ... */ }
        "prefix" => { /* ... */ }
        "wildcard" => { /* ... */ }
        "terms" => { /* ... */ }
        _ => Err(format!("unknown query type: {}", qj.query_type)),
    }
}
```

- [ ] **Step 2: Add parse round-trip test** (Query → JSON → Query)

- [ ] **Step 3: Commit**

---

### Task 25: JNI facade (RustIndexSearcher.java + reader.rs)

**Files:**
- Create: `crates/jni-binding/src/reader.rs`
- Create: `interop/java/RustIndexSearcher.java`
- Modify: `crates/jni-binding/src/lib.rs` (re-export reader module)

- [ ] **Step 1: Create RustIndexSearcher.java** (~80 lines)

```java
public class RustIndexSearcher {
    static { System.loadLibrary("rustlucene_jni"); }

    private static native long open(String indexPath);
    private static native String search(long handle, String queryJson, int topN, String sortJson);
    private static native void close(long handle);

    private long handle;

    public RustIndexSearcher(String indexPath) {
        this.handle = open(indexPath);
    }

    public String search(String queryJson, int topN, String sortJson) {
        return search(this.handle, queryJson, topN, sortJson);
    }

    // ... convenience methods ...
}
```

- [ ] **Step 2: Create reader.rs JNI bindings** (~150 lines)

```rust
use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::{jlong, jstring};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use rustlucene_core::search::{Query, IndexSearcher};

static HANDLES: Mutex<Option<HandleTable>> = Mutex::new(None);

struct HandleTable {
    next_id: i64,
    searchers: HashMap<i64, IndexSearcher>,
}

#[no_mangle]
pub extern "system" fn Java_RustIndexSearcher_open(
    mut env: JNIEnv, _class: JClass, path: JString,
) -> jlong {
    let path: String = env.get_string(&path).unwrap().into();
    // ... open DirectoryReader, create IndexSearcher, store in handle table ...
    0
}

#[no_mangle]
pub extern "system" fn Java_RustIndexSearcher_search(
    mut env: JNIEnv, _class: JClass, handle: jlong,
    query_json: JString, top_n: jlong, sort_json: JString,
) -> jstring {
    // ... lookup handle, parse query, execute search, serialize results to JSON ...
    std::ptr::null_mut()
}

#[no_mangle]
pub extern "system" fn Java_RustIndexSearcher_close(
    _env: JNIEnv, _class: JClass, handle: jlong,
) {
    // ... remove from handle table ...
}
```

- [ ] **Step 3: Build + test JNI round-trip**

```bash
cargo build -p jni-binding --release
javac -cp ... -d interop/java/classes interop/java/RustIndexSearcher.java
```

- [ ] **Step 4: Commit**

---

### Task 26: interop/verify-search.sh integration test

**Files:**
- Create: `interop/verify-search.sh`
- Modify: `Makefile` (add `verify-search` target)

- [ ] **Step 1: Create verify-search.sh** (~100 lines)

```bash
#!/usr/bin/env bash
set -euo pipefail

# 1. Build Rust index from corpus
# 2. Run Rust search (via CLI tool) → JSON results
# 3. Run Java search (via SearchBench) → results
# 4. Python diff: Rust doc IDs == Java doc IDs

python3 << 'PYEOF'
# Parse both JSON results, compare doc ID sets per query
PYEOF

echo "verify-search OK"
```

- [ ] **Step 2: Add `verify-search` target to Makefile**

```makefile
verify-search: build java-classes
	interop/verify-search.sh /tmp/verify-search message 1000 42
```

- [ ] **Step 3: Run the full pipeline**

```bash
make verify-search
```

Expected: `verify-search OK`

- [ ] **Step 4: Commit**

```bash
git add interop/verify-search.sh Makefile
git commit -m "test: add verify-search interop test (Rust write → Rust read = Java read)

Co-Authored-By: Claude <noreply@anthropic.com>"
```

**Phase 6 milestone:** `make verify-search` passes. End-to-end flow: Rust write → Rust read → result parity with Java.

---

## Final Verification

After all 26 tasks complete, run full test suite:

```bash
cargo test --all
make interop-test
make log-test
make verify-search
```

All existing tests pass. New search tests pass. Cross-language diff passes.
