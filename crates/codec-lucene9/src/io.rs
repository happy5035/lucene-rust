//! IndexOutput abstraction mirroring Lucene's `store/DataOutput.java` and
//! `store/ChecksumIndexOutput` (9.12.3).
//!
//! All multi-byte primitives written through `IndexOutput` are **little-endian**
//! (DataOutput.writeInt/writeLong are LE in Lucene); big-endian helpers live in
//! `codec_util` (CodecUtil.writeBEInt/writeBELong).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufWriter, Write};

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
