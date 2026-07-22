//! IndexOutput abstraction mirroring Lucene's `store/DataOutput.java` and
//! `store/ChecksumIndexOutput` (9.12.3).
//!
//! All multi-byte primitives written through `IndexOutput` are **little-endian**
//! (DataOutput.writeInt/writeLong are LE in Lucene); big-endian helpers live in
//! `codec_util` (CodecUtil.writeBEInt/writeBELong).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};

/// Large output buffer; avoids per-byte syscalls (cf. BufferedIndexOutput).
const BUFFER_CAPACITY: usize = 1 << 16;

enum Sink {
    File(File),
    Memory(Vec<u8>),
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Sink::File(f) => f.write(buf),
            Sink::Memory(v) => v.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Sink::File(f) => f.flush(),
            Sink::Memory(v) => v.flush(),
        }
    }
}

/// Buffered, position-tracking data output with Lucene `DataOutput` primitives.
pub struct IndexOutput {
    inner: BufWriter<Sink>,
    file_pointer: u64,
}

impl IndexOutput {
    /// Creates an output over a file (truncating), with a large buffer.
    pub fn from_file(file: File) -> Self {
        IndexOutput {
            inner: BufWriter::with_capacity(BUFFER_CAPACITY, Sink::File(file)),
            file_pointer: 0,
        }
    }

    /// Creates an in-memory output (used by unit tests).
    pub fn in_memory() -> Self {
        IndexOutput {
            inner: BufWriter::with_capacity(BUFFER_CAPACITY, Sink::Memory(Vec::new())),
            file_pointer: 0,
        }
    }

    /// Bytes written so far (DataOutput.getFilePointer).
    pub fn file_pointer(&self) -> u64 {
        self.file_pointer
    }

    /// Flushes the buffer and the underlying file to the OS.
    pub fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()?;
        if let Sink::File(f) = self.inner.get_mut() {
            f.sync_data()?;
        }
        Ok(())
    }

    /// Consumes the output, returning the written bytes (in-memory outputs only).
    pub fn into_bytes(mut self) -> Vec<u8> {
        self.inner.flush().expect("flush memory sink");
        match std::mem::replace(&mut self.inner, BufWriter::new(Sink::Memory(Vec::new())))
            .into_inner()
        {
            Ok(Sink::Memory(v)) => v,
            _ => unreachable!(),
        }
    }

    pub fn write_byte(&mut self, b: u8) -> io::Result<()> {
        self.inner.write_all(&[b])?;
        self.file_pointer += 1;
        Ok(())
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_all(bytes)?;
        self.file_pointer += bytes.len() as u64;
        Ok(())
    }

    /// Little-endian (DataOutput.writeShort, DataOutput.java:183-186).
    pub fn write_short(&mut self, v: i16) -> io::Result<()> {
        self.write_bytes(&v.to_le_bytes())
    }

    /// Little-endian (DataOutput.writeInt, DataOutput.java:73-78).
    pub fn write_int(&mut self, v: i32) -> io::Result<()> {
        self.write_bytes(&v.to_le_bytes())
    }

    /// Little-endian (DataOutput.writeLong, DataOutput.java:223-226).
    pub fn write_long(&mut self, v: i64) -> io::Result<()> {
        self.write_bytes(&v.to_le_bytes())
    }

    /// 7 bits per group, low groups first (DataOutput.writeVInt:197-203).
    /// Negative ints are written as their unsigned 32-bit pattern (like Java).
    pub fn write_vint(&mut self, v: i32) -> io::Result<()> {
        let mut v = v as u32;
        while v & !0x7f != 0 {
            self.write_byte(((v & 0x7f) as u8) | 0x80)?;
            v >>= 7;
        }
        self.write_byte(v as u8)
    }

    /// 7 bits per group, low groups first (DataOutput.writeVLong:234-243).
    /// Negative values are rejected, like the Java version.
    pub fn write_vlong(&mut self, v: i64) -> io::Result<()> {
        if v < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cannot write negative vLong (got: {v})"),
            ));
        }
        self.write_signed_vlong(v)
    }

    /// DataOutput.writeSignedVLong (:246-252): bit-pattern VLong, allows
    /// negatives (used by writeZLong).
    fn write_signed_vlong(&mut self, v: i64) -> io::Result<()> {
        let mut v = v as u64;
        while v & !0x7f != 0 {
            self.write_byte(((v & 0x7f) as u8) | 0x80)?;
            v >>= 7;
        }
        self.write_byte(v as u8)
    }

    /// zigzag + VInt (DataOutput.writeZInt:213-215).
    pub fn write_zint(&mut self, v: i32) -> io::Result<()> {
        self.write_vint((v << 1) ^ (v >> 31))
    }

    /// zigzag + VLong (DataOutput.writeZLong:259-261).
    pub fn write_zlong(&mut self, v: i64) -> io::Result<()> {
        self.write_signed_vlong((v << 1) ^ (v >> 63))
    }

    /// VInt **byte** length + standard UTF-8 (DataOutput.writeString:271-275;
    /// UnicodeUtil.UTF16toUTF8, not modified UTF-8).
    pub fn write_string(&mut self, s: &str) -> io::Result<()> {
        let bytes = s.as_bytes();
        self.write_vint(bytes.len() as i32)?;
        self.write_bytes(bytes)
    }

    /// VInt size + (key, value) string pairs (DataOutput.writeMapOfStrings:304-310).
    /// BTreeMap iteration keeps the byte output deterministic.
    pub fn write_map_of_strings(&mut self, map: &BTreeMap<String, String>) -> io::Result<()> {
        self.write_vint(map.len() as i32)?;
        for (k, v) in map {
            self.write_string(k)?;
            self.write_string(v)?;
        }
        Ok(())
    }

    /// VInt size + strings (DataOutput.writeSetOfStrings:321-326).
    pub fn write_set_of_strings(&mut self, set: &BTreeSet<String>) -> io::Result<()> {
        self.write_vint(set.len() as i32)?;
        for s in set {
            self.write_string(s)?;
        }
        Ok(())
    }
}

/// Wraps an `IndexOutput` with a running CRC32 over every byte written
/// (store/ChecksumIndexOutput; CRC32 algorithm = java.util.zip.CRC32,
/// store/BufferedChecksumIndexInput.java:20,34).
pub struct ChecksumIndexOutput {
    out: IndexOutput,
    digest: crc32fast::Hasher,
}

impl ChecksumIndexOutput {
    pub fn new(out: IndexOutput) -> Self {
        ChecksumIndexOutput {
            out,
            digest: crc32fast::Hasher::new(),
        }
    }

    /// CRC32 of everything written so far (CodecUtil.writeCRC:643-650 takes
    /// this value *after* the footer magic + algorithmID have been written).
    pub fn get_checksum(&self) -> u64 {
        self.digest.clone().finalize() as u64
    }

    pub fn file_pointer(&self) -> u64 {
        self.out.file_pointer()
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.out.into_bytes()
    }

    fn update(&mut self, bytes: &[u8]) {
        self.digest.update(bytes);
    }

    pub fn write_byte(&mut self, b: u8) -> io::Result<()> {
        self.out.write_byte(b)?;
        self.update(&[b]);
        Ok(())
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_bytes(bytes)?;
        self.update(bytes);
        Ok(())
    }

    pub fn write_short(&mut self, v: i16) -> io::Result<()> {
        self.write_bytes(&v.to_le_bytes())
    }

    pub fn write_int(&mut self, v: i32) -> io::Result<()> {
        self.write_bytes(&v.to_le_bytes())
    }

    pub fn write_long(&mut self, v: i64) -> io::Result<()> {
        self.write_bytes(&v.to_le_bytes())
    }

    pub fn write_vint(&mut self, v: i32) -> io::Result<()> {
        let mut v = v as u32;
        while v & !0x7f != 0 {
            self.write_byte(((v & 0x7f) as u8) | 0x80)?;
            v >>= 7;
        }
        self.write_byte(v as u8)
    }

    pub fn write_vlong(&mut self, v: i64) -> io::Result<()> {
        if v < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cannot write negative vLong (got: {v})"),
            ));
        }
        self.write_signed_vlong(v)
    }

    fn write_signed_vlong(&mut self, v: i64) -> io::Result<()> {
        let mut v = v as u64;
        while v & !0x7f != 0 {
            self.write_byte(((v & 0x7f) as u8) | 0x80)?;
            v >>= 7;
        }
        self.write_byte(v as u8)
    }

    pub fn write_zint(&mut self, v: i32) -> io::Result<()> {
        self.write_vint((v << 1) ^ (v >> 31))
    }

    pub fn write_zlong(&mut self, v: i64) -> io::Result<()> {
        self.write_signed_vlong((v << 1) ^ (v >> 63))
    }

    pub fn write_string(&mut self, s: &str) -> io::Result<()> {
        let bytes = s.as_bytes();
        self.write_vint(bytes.len() as i32)?;
        self.write_bytes(bytes)
    }

    pub fn write_map_of_strings(&mut self, map: &BTreeMap<String, String>) -> io::Result<()> {
        self.write_vint(map.len() as i32)?;
        for (k, v) in map {
            self.write_string(k)?;
            self.write_string(v)?;
        }
        Ok(())
    }

    pub fn write_set_of_strings(&mut self, set: &BTreeSet<String>) -> io::Result<()> {
        self.write_vint(set.len() as i32)?;
        for s in set {
            self.write_string(s)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(f: impl FnOnce(&mut IndexOutput)) -> Vec<u8> {
        let mut out = IndexOutput::in_memory();
        f(&mut out);
        out.into_bytes()
    }

    #[test]
    fn vint_known_vectors() {
        assert_eq!(bytes(|o| o.write_vint(0).unwrap()), [0x00]);
        assert_eq!(bytes(|o| o.write_vint(1).unwrap()), [0x01]);
        assert_eq!(bytes(|o| o.write_vint(127).unwrap()), [0x7f]);
        assert_eq!(bytes(|o| o.write_vint(128).unwrap()), [0x80, 0x01]);
        assert_eq!(bytes(|o| o.write_vint(300).unwrap()), [0xac, 0x02]);
        assert_eq!(
            bytes(|o| o.write_vint(i32::MAX).unwrap()),
            [0xff, 0xff, 0xff, 0xff, 0x07]
        );
    }

    #[test]
    fn vlong_known_vectors() {
        assert_eq!(bytes(|o| o.write_vlong(0).unwrap()), [0x00]);
        assert_eq!(bytes(|o| o.write_vlong(128).unwrap()), [0x80, 0x01]);
        assert_eq!(
            bytes(|o| o.write_vlong(1 << 35).unwrap()),
            [0x80, 0x80, 0x80, 0x80, 0x80, 0x01]
        );
        assert_eq!(
            bytes(|o| o.write_vlong(i64::MAX).unwrap()),
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]
        );
    }

    #[test]
    fn zint_zlong_known_vectors() {
        assert_eq!(bytes(|o| o.write_zint(0).unwrap()), [0x00]);
        assert_eq!(bytes(|o| o.write_zint(-1).unwrap()), [0x01]);
        assert_eq!(bytes(|o| o.write_zint(1).unwrap()), [0x02]);
        assert_eq!(bytes(|o| o.write_zint(-64).unwrap()), [0x7f]);
        assert_eq!(bytes(|o| o.write_zint(64).unwrap()), [0x80, 0x01]);
        assert_eq!(bytes(|o| o.write_zlong(-1).unwrap()), [0x01]);
        assert_eq!(
            bytes(|o| o.write_zlong(i64::MIN).unwrap()),
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]
        );
    }

    #[test]
    fn int_long_are_little_endian() {
        assert_eq!(bytes(|o| o.write_int(0x01020304).unwrap()), [4, 3, 2, 1]);
        assert_eq!(
            bytes(|o| o.write_long(0x0102030405060708).unwrap()),
            [8, 7, 6, 5, 4, 3, 2, 1]
        );
        assert_eq!(bytes(|o| o.write_int(-1).unwrap()), [0xff; 4]);
        assert_eq!(bytes(|o| o.write_long(-1).unwrap()), [0xff; 8]);
    }

    #[test]
    fn string_layout() {
        assert_eq!(bytes(|o| o.write_string("").unwrap()), [0x00]);
        assert_eq!(bytes(|o| o.write_string("ab").unwrap()), [0x02, b'a', b'b']);
        // 2-byte UTF-8: byte length, not char count
        assert_eq!(bytes(|o| o.write_string("é").unwrap()), [0x02, 0xc3, 0xa9]);
    }

    #[test]
    fn map_and_set_layout() {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), "v".to_string());
        assert_eq!(
            bytes(|o| o.write_map_of_strings(&m).unwrap()),
            [0x01, 0x01, b'k', 0x01, b'v']
        );
        let mut s = BTreeSet::new();
        s.insert("a".to_string());
        s.insert("b".to_string());
        assert_eq!(
            bytes(|o| o.write_set_of_strings(&s).unwrap()),
            [0x02, 0x01, b'a', 0x01, b'b']
        );
    }

    #[test]
    fn checksum_matches_java_crc32() {
        // CRC32("123456789") == 0xCBF43926 (classic check value)
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        out.write_bytes(b"123456789").unwrap();
        assert_eq!(out.get_checksum(), 0xCBF43926);
        assert_eq!(out.file_pointer(), 9);
    }
}

/// DataOutput-equivalent shared by raw and checksummed outputs, so encoders
/// (postings_ll etc.) can write through either without bypassing the CRC.
pub trait DataOutput {
    fn write_byte(&mut self, b: u8) -> io::Result<()>;
    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn write_short(&mut self, v: i16) -> io::Result<()>;
    fn write_int(&mut self, v: i32) -> io::Result<()>;
    fn write_long(&mut self, v: i64) -> io::Result<()>;
    fn write_vint(&mut self, v: i32) -> io::Result<()>;
    fn write_vlong(&mut self, v: i64) -> io::Result<()>;
    fn write_zint(&mut self, v: i32) -> io::Result<()>;
    fn write_zlong(&mut self, v: i64) -> io::Result<()>;
}

impl DataOutput for IndexOutput {
    fn write_byte(&mut self, b: u8) -> io::Result<()> {
        IndexOutput::write_byte(self, b)
    }
    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        IndexOutput::write_bytes(self, bytes)
    }
    fn write_short(&mut self, v: i16) -> io::Result<()> {
        IndexOutput::write_short(self, v)
    }
    fn write_int(&mut self, v: i32) -> io::Result<()> {
        IndexOutput::write_int(self, v)
    }
    fn write_long(&mut self, v: i64) -> io::Result<()> {
        IndexOutput::write_long(self, v)
    }
    fn write_vint(&mut self, v: i32) -> io::Result<()> {
        IndexOutput::write_vint(self, v)
    }
    fn write_vlong(&mut self, v: i64) -> io::Result<()> {
        IndexOutput::write_vlong(self, v)
    }
    fn write_zint(&mut self, v: i32) -> io::Result<()> {
        IndexOutput::write_zint(self, v)
    }
    fn write_zlong(&mut self, v: i64) -> io::Result<()> {
        IndexOutput::write_zlong(self, v)
    }
}

impl DataOutput for ChecksumIndexOutput {
    fn write_byte(&mut self, b: u8) -> io::Result<()> {
        ChecksumIndexOutput::write_byte(self, b)
    }
    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        ChecksumIndexOutput::write_bytes(self, bytes)
    }
    fn write_short(&mut self, v: i16) -> io::Result<()> {
        ChecksumIndexOutput::write_short(self, v)
    }
    fn write_int(&mut self, v: i32) -> io::Result<()> {
        ChecksumIndexOutput::write_int(self, v)
    }
    fn write_long(&mut self, v: i64) -> io::Result<()> {
        ChecksumIndexOutput::write_long(self, v)
    }
    fn write_vint(&mut self, v: i32) -> io::Result<()> {
        ChecksumIndexOutput::write_vint(self, v)
    }
    fn write_vlong(&mut self, v: i64) -> io::Result<()> {
        ChecksumIndexOutput::write_vlong(self, v)
    }
    fn write_zint(&mut self, v: i32) -> io::Result<()> {
        ChecksumIndexOutput::write_zint(self, v)
    }
    fn write_zlong(&mut self, v: i64) -> io::Result<()> {
        ChecksumIndexOutput::write_zlong(self, v)
    }
}

// ============================================================================
// IndexInput — read counterpart to IndexOutput
// ============================================================================

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
        Ok(((v >> 1) as i64 ^ -((v & 1) as i64)) as i32)
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
            if total < buf.len() && self.pos < self.file_len {
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
        Ok(((v >> 1) as i64 ^ -((v & 1) as i64)) as i32)
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
        file.seek(SeekFrom::Start(_offset))?;
        file.read_exact(&mut buf)?;
        Ok(Box::new(HeapIndexInput::new(buf)))
    }
}

#[cfg(test)]
mod tests_read {
    use super::*;

    #[test]
    fn test_vlong_round_trip() {
        // write_vlong rejects negatives, so only test non-negative values
        let test_values: &[i64] = &[0, 1, 127, 128, 16383, 16384, i64::MAX];
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
