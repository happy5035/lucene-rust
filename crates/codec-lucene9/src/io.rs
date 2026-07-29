//! IndexOutput/IndexInput abstractions mirroring Lucene's `store/DataOutput.java`,
//! `store/DataInput.java` and the buffered/checksummed wrappers
//! (`ChecksumIndexOutput`, `BufferedIndexInput`, `ChecksumIndexInput`) (9.12.3).
//!
//! All multi-byte primitives through `IndexOutput`/`IndexInput` are
//! **little-endian** (DataOutput.writeInt/writeLong, DataInput.readInt/readLong
//! are LE in Lucene); big-endian helpers live in `codec_util`
//! (CodecUtil.writeBEInt/writeBELong, CodecUtil.readBEInt/readBELong).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::sync::Arc;

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

    // ---- read side (DataInput / IndexInput / ChecksumIndexInput) ----

    #[test]
    fn read_primitives_round_trip() {
        let bytes = bytes(|o| {
            o.write_byte(0xAB).unwrap();
            o.write_short(-2).unwrap();
            o.write_int(0x01020304).unwrap();
            o.write_long(-1).unwrap();
            o.write_vint(300).unwrap();
            o.write_vlong(1 << 35).unwrap();
            o.write_zint(-64).unwrap();
            o.write_string("héllo").unwrap();
            let mut m = BTreeMap::new();
            m.insert("k".to_string(), "v".to_string());
            o.write_map_of_strings(&m).unwrap();
            let mut s = BTreeSet::new();
            s.insert("a".to_string());
            s.insert("b".to_string());
            o.write_set_of_strings(&s).unwrap();
        });
        let mut i = IndexInput::in_memory(bytes);
        assert_eq!(i.read_byte().unwrap(), 0xAB);
        assert_eq!(i.read_short().unwrap(), -2);
        assert_eq!(i.read_int().unwrap(), 0x01020304);
        assert_eq!(i.read_long().unwrap(), -1);
        assert_eq!(i.read_vint().unwrap(), 300);
        assert_eq!(i.read_vlong().unwrap(), 1 << 35);
        assert_eq!(i.read_zint().unwrap(), -64);
        assert_eq!(i.read_string().unwrap(), "héllo");
        let m = i.read_map_of_strings().unwrap();
        assert_eq!(m.get("k").unwrap(), "v");
        let s = i.read_set_of_strings().unwrap();
        assert!(s.contains("a") && s.contains("b"));
        assert_eq!(i.file_pointer(), i.length());
    }

    /// zigZagDecode must use an **unsigned** shift like Java (`i >>> 1`,
    /// BitUtil.java:299): an arithmetic shift breaks values whose zigzag
    /// encoding sets bit 31 (|n| >= 2^30).
    #[test]
    fn read_zint_large_magnitudes_round_trip() {
        let values = [i32::MIN, 1 << 30, -(1 << 30), 0, -1, i32::MAX];
        let bytes = bytes(|o| {
            for &n in &values {
                o.write_zint(n).unwrap();
            }
        });
        let mut i = IndexInput::in_memory(bytes);
        for &n in &values {
            assert_eq!(i.read_zint().unwrap(), n);
        }
    }

    #[test]
    fn read_beyond_end_is_eof() {
        let mut i = IndexInput::in_memory(vec![1]);
        assert_eq!(i.read_byte().unwrap(), 1);
        let err = i.read_byte().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn skip_bytes_moves_pointer() {
        let bytes = bytes(|o| o.write_bytes(&[7; 100]).unwrap());
        let mut i = IndexInput::in_memory(bytes);
        i.skip_bytes(90).unwrap();
        assert_eq!(i.file_pointer(), 90);
        assert_eq!(i.read_byte().unwrap(), 7);
        assert!(i.skip_bytes(11).is_err(), "skip past end must fail");
    }

    #[test]
    fn memory_slice_independent_positions() {
        let bytes = bytes(|o| o.write_bytes(&(0u8..100).collect::<Vec<_>>()).unwrap());
        let i = IndexInput::in_memory(bytes);
        let mut a = i.slice(10, 20).unwrap();
        let mut b = i.slice(50, 10).unwrap();
        assert_eq!(a.length(), 20);
        assert_eq!(a.read_byte().unwrap(), 10);
        assert_eq!(b.read_byte().unwrap(), 50);
        assert_eq!(a.read_byte().unwrap(), 11);
        assert!(i.slice(90, 20).is_err(), "slice past end must fail");
    }

    #[test]
    fn checksum_input_footer_round_trip() {
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        out.write_bytes(b"payload").unwrap();
        crate::codec_util::write_footer(&mut out).unwrap();
        let bytes = out.into_bytes();
        let mut input = ChecksumIndexInput::new(IndexInput::in_memory(bytes));
        let mut payload = [0u8; 7];
        input.read_bytes(&mut payload).unwrap();
        assert_eq!(&payload, b"payload");
        crate::codec_util::check_footer(&mut input).unwrap();
    }

    #[test]
    fn corrupted_payload_fails_footer() {
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        out.write_bytes(b"payload").unwrap();
        crate::codec_util::write_footer(&mut out).unwrap();
        let mut bytes = out.into_bytes();
        bytes[2] ^= 0xFF;
        let mut input = ChecksumIndexInput::new(IndexInput::in_memory(bytes));
        let mut payload = [0u8; 7];
        input.read_bytes(&mut payload).unwrap();
        assert!(crate::codec_util::check_footer(&mut input).is_err());
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

// ===========================================================================
// Read side (DataInput / IndexInput / ChecksumIndexInput), mirroring
// store/DataInput.java and store/BufferedIndexInput.java (9.12.3).
// ===========================================================================

/// Read buffer capacity (spec §3: buffered FileChannel 读, 8KB; cf.
/// BufferedIndexInput.BUFFER_SIZE :32).
const INPUT_BUFFER_CAPACITY: usize = 1 << 13;

/// Optional process-wide IO counters (observability feature for benchmarks):
/// when the `RL_IO_STATS` env var is set to anything but "0", every `refill`
/// from a file source adds its byte count here — the logical read volume the
/// process pulls through `IndexInput` (page-cache hits included, like rchar).
/// Disabled cost is one relaxed atomic load per refill.
pub mod io_stats {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static ENABLED: AtomicBool = AtomicBool::new(false);
    static INIT: std::sync::Once = std::sync::Once::new();
    pub static READ_BYTES: AtomicU64 = AtomicU64::new(0);
    pub static READ_CALLS: AtomicU64 = AtomicU64::new(0);

    pub fn enabled() -> bool {
        INIT.call_once(|| {
            let on = std::env::var_os("RL_IO_STATS")
                .map(|v| v != "0")
                .unwrap_or(false);
            ENABLED.store(on, Ordering::Relaxed);
        });
        ENABLED.load(Ordering::Relaxed)
    }

    /// (bytes, calls) read so far.
    pub fn snapshot() -> (u64, u64) {
        (
            READ_BYTES.load(Ordering::Relaxed),
            READ_CALLS.load(Ordering::Relaxed),
        )
    }
}

enum InputSource {
    /// Positional reads at `base + pos`; slices share the file handle via
    /// Arc clone (no dup syscall) and shift `base`.
    File {
        file: Arc<File>,
        base: u64,
    },
    /// In-memory image; slices share the data via Arc clone + offset (no copy).
    Memory {
        data: Arc<Vec<u8>>,
        offset: u64,
    },
    /// Memory-mapped file; reads are pointer dereferences (zero syscall).
    Mmap {
        data: Arc<memmap2::Mmap>,
        offset: u64,
    },
}

/// Buffered, position-tracking data input with Lucene `DataInput` primitives
/// (BufferedIndexInput). All multi-byte primitives are little-endian
/// (DataInput.readInt/readLong, DataInput.java:94-100,183-185).
pub struct IndexInput {
    source: InputSource,
    length: u64,
    // Only used for File source; Memory reads bypass this entirely.
    buffer: [u8; INPUT_BUFFER_CAPACITY],
    buffer_start: u64, // absolute position of buffer[0]
    buffer_len: usize, // valid bytes in buffer
    position: u64,     // absolute position of the next byte to read
}

impl IndexInput {
    /// An input over `file[0..length]` (FSDirectory.openInput).
    pub fn from_file(file: File, length: u64) -> Self {
        IndexInput {
            source: InputSource::File { file: Arc::new(file), base: 0 },
            length,
            buffer: [0; INPUT_BUFFER_CAPACITY],
            buffer_start: 0,
            buffer_len: 0,
            position: 0,
        }
    }

    /// An input over an in-memory image (unit tests, in-memory blob parsing).
    pub fn in_memory(bytes: Vec<u8>) -> Self {
        let length = bytes.len() as u64;
        IndexInput {
            source: InputSource::Memory { data: Arc::new(bytes), offset: 0 },
            length,
            buffer: [0; INPUT_BUFFER_CAPACITY],
            buffer_start: 0,
            buffer_len: 0,
            position: 0,
        }
    }

    /// An input over a memory-mapped file (zero-syscall reads after page-in).
    pub fn from_mmap(mmap: Arc<memmap2::Mmap>) -> Self {
        let length = mmap.len() as u64;
        IndexInput {
            source: InputSource::Mmap { data: mmap, offset: 0 },
            length,
            buffer: [0; INPUT_BUFFER_CAPACITY],
            buffer_start: 0,
            buffer_len: 0,
            position: 0,
        }
    }

    /// IndexInput.length (IndexInput.java:79).
    pub fn length(&self) -> u64 {
        self.length
    }

    /// BufferedIndexInput.getFilePointer (:371-374).
    pub fn file_pointer(&self) -> u64 {
        self.position
    }

    /// BufferedIndexInput.seek (:376-385).
    pub fn seek(&mut self, pos: u64) -> io::Result<()> {
        if pos > self.length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("seek past EOF: {pos} > {}", self.length),
            ));
        }
        self.position = pos;
        Ok(())
    }

    /// IndexInput.slice (:121-122): an independent reader over
    /// `[offset, offset + length)` of this input.
    pub fn slice(&self, offset: u64, length: u64) -> io::Result<IndexInput> {
        let end = offset.checked_add(length).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("slice [{offset}, +{length}) overflows u64"),
            )
        })?;
        if end > self.length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("slice [{offset}, +{length}) past EOF {}", self.length),
            ));
        }
        let source = match &self.source {
            InputSource::File { file, base } => InputSource::File {
                file: Arc::clone(file),
                base: base + offset,
            },
            InputSource::Memory { data, offset: parent_off } => InputSource::Memory {
                data: Arc::clone(data),
                offset: parent_off + offset,
            },
            InputSource::Mmap { data, offset: parent_off } => InputSource::Mmap {
                data: Arc::clone(data),
                offset: parent_off + offset,
            },
        };
        Ok(IndexInput {
            source,
            length,
            buffer: [0; INPUT_BUFFER_CAPACITY],
            buffer_start: 0,
            buffer_len: 0,
            position: 0,
        })
    }

    /// BufferedIndexInput.refill (:340-362).
    fn refill(&mut self) -> io::Result<()> {
        if self.position >= self.length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("read past EOF: {}", self.length),
            ));
        }
        let n = INPUT_BUFFER_CAPACITY.min((self.length - self.position) as usize);
        match &self.source {
            InputSource::File { file, base } => {
                if io_stats::enabled() {
                    use std::sync::atomic::Ordering::Relaxed;
                    io_stats::READ_BYTES.fetch_add(n as u64, Relaxed);
                    io_stats::READ_CALLS.fetch_add(1, Relaxed);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    file.read_at(&mut self.buffer[..n], base + self.position)?;
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::FileExt;
                    file.seek_read(&mut self.buffer[..n], base + self.position)?;
                }
            }
            InputSource::Memory { data, offset } => {
                let start = (offset + self.position) as usize;
                self.buffer[..n].copy_from_slice(&data[start..start + n]);
            }
            InputSource::Mmap { data, offset } => {
                let start = (offset + self.position) as usize;
                self.buffer[..n].copy_from_slice(&data[start..start + n]);
            }
        }
        self.buffer_start = self.position;
        self.buffer_len = n;
        Ok(())
    }

    /// Bytes available in the buffer at `position`; 0 when a seek moved the
    /// position outside the buffered window (forces a refill on next read,
    /// cf. BufferedIndexInput.seek invalidating the buffer).
    fn buffered(&self) -> usize {
        let end = self.buffer_start + self.buffer_len as u64;
        if self.position >= self.buffer_start && self.position < end {
            (end - self.position) as usize
        } else {
            0
        }
    }
}

/// DataInput-equivalent shared by raw and checksummed inputs (mirror of
/// [`DataOutput`]), so decoders read through either without bypassing CRC.
/// Composite readers have default implementations over `read_bytes`.
pub trait DataInput {
    fn read_byte(&mut self) -> io::Result<u8>;
    fn read_bytes(&mut self, buf: &mut [u8]) -> io::Result<()>;

    /// Little-endian (DataInput.readShort, DataInput.java:82-86).
    fn read_short(&mut self) -> io::Result<i16> {
        let mut b = [0u8; 2];
        self.read_bytes(&mut b)?;
        Ok(i16::from_le_bytes(b))
    }

    /// Little-endian (DataInput.readInt, DataInput.java:94-100).
    fn read_int(&mut self) -> io::Result<i32> {
        let mut b = [0u8; 4];
        self.read_bytes(&mut b)?;
        Ok(i32::from_le_bytes(b))
    }

    /// Little-endian (DataInput.readLong, DataInput.java:183-185).
    fn read_long(&mut self) -> io::Result<i64> {
        let mut b = [0u8; 8];
        self.read_bytes(&mut b)?;
        Ok(i64::from_le_bytes(b))
    }

    /// 7 bits per group, low groups first; at most 5 bytes
    /// (DataInput.readVInt :136-165).
    fn read_vint(&mut self) -> io::Result<i32> {
        let mut v = 0u32;
        for i in 0..5 {
            let b = self.read_byte()?;
            v |= ((b & 0x7f) as u32) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v as i32);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "vInt too long (DataInput.readVInt)",
        ))
    }

    /// 7 bits per group, low groups first; at most 9 bytes
    /// (DataInput.readVLong :235-286, negative values rejected like Java).
    fn read_vlong(&mut self) -> io::Result<i64> {
        let mut v = 0u64;
        for i in 0..9 {
            let b = self.read_byte()?;
            v |= ((b & 0x7f) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v as i64);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "vLong too long (DataInput.readVLong)",
        ))
    }

    /// zigzag + VInt (DataInput.readZInt :173-175, BitUtil.zigZagDecode :299).
    /// Java shifts **unsigned** (`i >>> 1`); decoding the VInt bit pattern as
    /// u32 first keeps |n| >= 2^30 (bit 31 set) correct.
    fn read_zint(&mut self) -> io::Result<i32> {
        let v = self.read_vint()?;
        Ok(((v as u32 >> 1) as i32) ^ -(v & 1))
    }

    /// VInt **byte** length + UTF-8 (DataInput.readString :303-308).
    fn read_string(&mut self) -> io::Result<String> {
        let len = self.read_vint()? as usize;
        let mut bytes = vec![0u8; len];
        self.read_bytes(&mut bytes)?;
        String::from_utf8(bytes).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid UTF-8 string: {e}"),
            )
        })
    }

    /// VInt size + (key, value) string pairs (DataInput.readMapOfStrings :334-349).
    fn read_map_of_strings(&mut self) -> io::Result<BTreeMap<String, String>> {
        let count = self.read_vint()? as usize;
        let mut map = BTreeMap::new();
        for _ in 0..count {
            let k = self.read_string()?;
            let v = self.read_string()?;
            map.insert(k, v);
        }
        Ok(map)
    }

    /// VInt size + strings (DataInput.readSetOfStrings :356-369).
    fn read_set_of_strings(&mut self) -> io::Result<BTreeSet<String>> {
        let count = self.read_vint()? as usize;
        let mut set = BTreeSet::new();
        for _ in 0..count {
            set.insert(self.read_string()?);
        }
        Ok(set)
    }

    /// IndexInput.skipBytes (:83-90). The default reads through (so the
    /// checksum wrapper stays correct); `IndexInput` overrides with a seek.
    fn skip_bytes(&mut self, mut n: u64) -> io::Result<()> {
        let mut scratch = [0u8; 4096];
        while n > 0 {
            let chunk = (n as usize).min(scratch.len());
            self.read_bytes(&mut scratch[..chunk])?;
            n -= chunk as u64;
        }
        Ok(())
    }
}

impl DataInput for IndexInput {
    /// BufferedIndexInput.readByte (:52-58).
    fn read_byte(&mut self) -> io::Result<u8> {
        let direct: Option<(&[u8], u64)> = match &self.source {
            InputSource::Memory { data, offset } => Some((data.as_slice(), *offset)),
            InputSource::Mmap { data, offset } => Some((data.as_ref(), *offset)),
            _ => None,
        };
        if let Some((bytes, offset)) = direct {
            if self.position >= self.length {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("read past EOF: {}", self.length),
                ));
            }
            let b = bytes[(offset + self.position) as usize];
            self.position += 1;
            return Ok(b);
        }
        if self.buffered() == 0 {
            self.refill()?;
        }
        let b = self.buffer[(self.position - self.buffer_start) as usize];
        self.position += 1;
        Ok(b)
    }

    /// BufferedIndexInput.readBytes (:91-133) — Memory reads directly, File through buffer.
    fn read_bytes(&mut self, mut buf: &mut [u8]) -> io::Result<()> {
        let direct: Option<(&[u8], u64)> = match &self.source {
            InputSource::Memory { data, offset } => Some((data.as_slice(), *offset)),
            InputSource::Mmap { data, offset } => Some((data.as_ref(), *offset)),
            _ => None,
        };
        if let Some((bytes, offset)) = direct {
            let start = (offset + self.position) as usize;
            let end = start + buf.len();
            if end > (offset + self.length) as usize {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("read past EOF: {}", self.length),
                ));
            }
            buf.copy_from_slice(&bytes[start..end]);
            self.position += buf.len() as u64;
            return Ok(());
        }
        while !buf.is_empty() {
            if self.buffered() == 0 {
                self.refill()?;
            }
            let n = self.buffered().min(buf.len());
            let start = (self.position - self.buffer_start) as usize;
            buf[..n].copy_from_slice(&self.buffer[start..start + n]);
            self.position += n as u64;
            let rest = std::mem::take(&mut buf);
            buf = &mut rest[n..];
        }
        Ok(())
    }

    /// IndexInput.skipBytes (:83-90) = seek(getFilePointer() + numBytes).
    fn skip_bytes(&mut self, n: u64) -> io::Result<()> {
        let target = self.position.checked_add(n).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "skip {n} bytes from position {} overflows u64",
                    self.position
                ),
            )
        })?;
        self.seek(target)
    }
}

/// Wraps an `IndexInput` with a running CRC32 over every byte read
/// (store/ChecksumIndexInput; CRC32 algorithm = java.util.zip.CRC32,
/// BufferedChecksumIndexInput.java:20,34). Sequential reads only — segments_N,
/// .si, .fnm, .tmd, .psm are parsed straight through.
pub struct ChecksumIndexInput {
    input: IndexInput,
    digest: crc32fast::Hasher,
}

impl ChecksumIndexInput {
    pub fn new(input: IndexInput) -> Self {
        ChecksumIndexInput {
            input,
            digest: crc32fast::Hasher::new(),
        }
    }

    /// CRC32 of everything read so far (CodecUtil.writeCRC :643-650 takes this
    /// value *after* the footer magic + algorithmID have been read).
    pub fn get_checksum(&self) -> u64 {
        self.digest.clone().finalize() as u64
    }

    pub fn file_pointer(&self) -> u64 {
        self.input.file_pointer()
    }

    pub fn length(&self) -> u64 {
        self.input.length()
    }
}

impl DataInput for ChecksumIndexInput {
    fn read_byte(&mut self) -> io::Result<u8> {
        let b = self.input.read_byte()?;
        self.digest.update(&[b]);
        Ok(b)
    }

    fn read_bytes(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.input.read_bytes(buf)?;
        self.digest.update(buf);
        Ok(())
    }
}
