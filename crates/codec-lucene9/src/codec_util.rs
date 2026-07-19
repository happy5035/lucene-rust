//! Codec header/footer helpers mirroring `codecs/CodecUtil.java` (9.12.3).
//!
//! Unlike `io::IndexOutput` primitives, the `write_be_*` helpers here are
//! **big-endian** (CodecUtil.writeBEInt:653-658 / writeBELong:672-677).

use std::io;

use crate::io::ChecksumIndexOutput;

/// CodecUtil.CODEC_MAGIC (:46).
pub const CODEC_MAGIC: u32 = 0x3fd76c17;
/// CodecUtil.FOOTER_MAGIC = ~CODEC_MAGIC (:49).
pub const FOOTER_MAGIC: u32 = 0xC02893E8;
/// CodecUtil.footerLength() (:421): magic(4) + algorithmID(4) + crc(8).
pub const FOOTER_LENGTH: usize = 16;
/// Footer checksum algorithm id: 0 = zlib CRC32 (writeFooter javadoc:401).
const FOOTER_ALGORITHM_ID: u32 = 0;

/// Big-endian int (CodecUtil.writeBEInt:653-658).
pub fn write_be_int(out: &mut ChecksumIndexOutput, v: u32) -> io::Result<()> {
    out.write_bytes(&v.to_be_bytes())
}

/// Big-endian long (CodecUtil.writeBELong:672-677).
pub fn write_be_long(out: &mut ChecksumIndexOutput, v: u64) -> io::Result<()> {
    out.write_bytes(&v.to_be_bytes())
}

/// CodecUtil.headerLength (:144) = magic(4) + codec VInt-len(1) + codec bytes + version(4).
pub fn header_length(codec: &str) -> usize {
    9 + codec.len()
}

/// CodecUtil.indexHeaderLength (:155).
pub fn index_header_length(codec: &str, suffix: &str) -> usize {
    header_length(codec) + 16 + 1 + suffix.len()
}

/// CodecUtil.writeIndexHeader (:121-135): BE magic, codec string, BE version,
/// raw 16-byte object id, 1-byte suffix length + suffix bytes.
pub fn write_index_header(
    out: &mut ChecksumIndexOutput,
    codec: &str,
    version: u32,
    id: &[u8; 16],
    suffix: &str,
) -> io::Result<()> {
    assert!(codec.len() < 128, "codec name too long");
    assert!(suffix.len() < 256, "suffix too long");
    write_be_int(out, CODEC_MAGIC)?;
    out.write_string(codec)?;
    write_be_int(out, version)?;
    out.write_bytes(id)?;
    out.write_byte(suffix.len() as u8)?;
    out.write_bytes(suffix.as_bytes())
}

/// CodecUtil.writeFooter (:409-413): BE FOOTER_MAGIC + BE algorithmID(0) +
/// BE long crc. The CRC covers every byte from offset 0 through algorithmID
/// inclusive (writeCRC:643-650).
pub fn write_footer(out: &mut ChecksumIndexOutput) -> io::Result<()> {
    write_be_int(out, FOOTER_MAGIC)?;
    write_be_int(out, FOOTER_ALGORITHM_ID)?;
    let crc = out.get_checksum();
    write_be_long(out, crc)
}

/// Computes the footer CRC for a whole in-memory file image (test helper).
#[cfg(test)]
pub(crate) fn crc32(bytes: &[u8]) -> u64 {
    let mut h = crc32fast::Hasher::new();
    h.update(bytes);
    h.finalize() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::IndexOutput;

    #[test]
    fn index_header_byte_layout() {
        let id = [0xABu8; 16];
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        write_index_header(&mut out, "Lucene90SegmentInfo", 0, &id, "").unwrap();
        let bytes = out.into_bytes();

        let mut expected = vec![0x3f, 0xd7, 0x6c, 0x17]; // BE magic
        expected.push(19); // codec name length
        expected.extend_from_slice(b"Lucene90SegmentInfo");
        expected.extend_from_slice(&[0, 0, 0, 0]); // BE version 0
        expected.extend_from_slice(&[0xAB; 16]); // object id
        expected.push(0); // suffix length
        assert_eq!(bytes, expected);
        assert_eq!(bytes.len(), index_header_length("Lucene90SegmentInfo", ""));
        assert_eq!(bytes.len(), 45);
    }

    #[test]
    fn index_header_with_suffix() {
        let id = [0u8; 16];
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        write_index_header(&mut out, "segments", 10, &id, "1").unwrap();
        let bytes = out.into_bytes();
        assert_eq!(bytes.len(), index_header_length("segments", "1"));
        assert_eq!(*bytes.last().unwrap(), b'1');
        assert_eq!(bytes[bytes.len() - 2], 1); // suffix length byte
    }

    #[test]
    fn footer_layout_and_crc_coverage() {
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        out.write_bytes(b"abc").unwrap();
        write_footer(&mut out).unwrap();
        let bytes = out.into_bytes();

        assert_eq!(bytes.len(), 3 + FOOTER_LENGTH);
        // BE footer magic + BE algorithmID 0
        assert_eq!(&bytes[3..11], &[0xc0, 0x28, 0x93, 0xe8, 0, 0, 0, 0]);
        // CRC covers offset 0..=algorithmID inclusive (i.e. first 11 bytes here)
        let expected_crc = crc32(&bytes[..11]);
        let actual_crc = u64::from_be_bytes(bytes[11..19].try_into().unwrap());
        assert_eq!(actual_crc, expected_crc);
        assert_eq!(actual_crc >> 32, 0, "crc high 32 bits must be 0");
    }
}
