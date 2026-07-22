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
pub(crate) const FOOTER_ALGORITHM_ID: u32 = 0;

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

/// CorruptIndexException equivalent (spec §5: 损坏即 CorruptIndex 错误).
pub(crate) fn corrupt(msg: impl Into<String>) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("corrupt index: {}", msg.into()),
    )
}

use crate::io::{ChecksumIndexInput, DataInput, IndexInput};

/// Big-endian int (CodecUtil.readBEInt :667-672). Header/footer ints are BE
/// while file bodies are LE — do not mix up.
pub fn read_be_int(input: &mut impl DataInput) -> io::Result<u32> {
    let mut b = [0u8; 4];
    input.read_bytes(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

/// Big-endian long (CodecUtil.readBELong :675-677).
pub fn read_be_long(input: &mut impl DataInput) -> io::Result<u64> {
    let mut b = [0u8; 8];
    input.read_bytes(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

/// CodecUtil.checkHeader (:182-195) + checkHeaderNoMagic (:201-218).
/// Returns the version found.
pub fn check_header(
    input: &mut impl DataInput,
    codec: &str,
    min_version: u32,
    max_version: u32,
) -> io::Result<u32> {
    let magic = read_be_int(input)?;
    if magic != CODEC_MAGIC {
        return Err(corrupt(format!("bad codec magic {magic:#x}")));
    }
    let name = input.read_string()?;
    if name != codec {
        return Err(corrupt(format!("expected codec {codec}, found {name}")));
    }
    let version = read_be_int(input)?;
    if !(min_version..=max_version).contains(&version) {
        return Err(corrupt(format!(
            "codec {codec} version {version} outside [{min_version}, {max_version}]"
        )));
    }
    Ok(version)
}

/// CodecUtil.checkIndexHeaderID (:363-375).
pub fn check_index_header_id(
    input: &mut impl DataInput,
    expected_id: &[u8; 16],
) -> io::Result<()> {
    let mut id = [0u8; 16];
    input.read_bytes(&mut id)?;
    if &id != expected_id {
        return Err(corrupt("index header ID mismatch"));
    }
    Ok(())
}

/// CodecUtil.checkIndexHeaderSuffix (:378-389).
pub fn check_index_header_suffix(input: &mut impl DataInput, expected: &str) -> io::Result<()> {
    let len = input.read_byte()? as usize;
    let mut bytes = vec![0u8; len];
    input.read_bytes(&mut bytes)?;
    if bytes != expected.as_bytes() {
        return Err(corrupt(format!(
            "index header suffix mismatch: expected {expected:?}"
        )));
    }
    Ok(())
}

/// CodecUtil.checkIndexHeader (:246-258) = checkHeader + ID + suffix.
pub fn check_index_header(
    input: &mut impl DataInput,
    codec: &str,
    min_version: u32,
    max_version: u32,
    expected_id: &[u8; 16],
    suffix: &str,
) -> io::Result<u32> {
    let version = check_header(input, codec, min_version, max_version)?;
    check_index_header_id(input, expected_id)?;
    check_index_header_suffix(input, suffix)?;
    Ok(version)
}

/// CodecUtil.checkFooter (:432-445) + validateFooter (:560-598): the stream
/// must have exactly `FOOTER_LENGTH` bytes left; the CRC covers every byte
/// through algorithmID inclusive (writeCRC :643-650).
pub fn check_footer(input: &mut ChecksumIndexInput) -> io::Result<()> {
    let remaining = input.length() - input.file_pointer();
    if remaining != FOOTER_LENGTH as u64 {
        return Err(corrupt(format!(
            "expected {FOOTER_LENGTH} footer bytes, {remaining} remaining"
        )));
    }
    let magic = read_be_int(input)?;
    if magic != FOOTER_MAGIC {
        return Err(corrupt(format!("bad footer magic {magic:#x}")));
    }
    let algorithm_id = read_be_int(input)?;
    if algorithm_id != FOOTER_ALGORITHM_ID {
        return Err(corrupt(format!("bad footer algorithmID {algorithm_id}")));
    }
    let expected = input.get_checksum();
    let actual = read_be_long(input)?;
    if actual != expected {
        return Err(corrupt(format!(
            "footer CRC mismatch: expected {expected:#x}, found {actual:#x}"
        )));
    }
    Ok(())
}

/// CodecUtil.retrieveChecksum (:623-647) as used on the normal open path of
/// the big data files (.tim/.tip/.doc): exact length + trailing footer
/// structure, without recomputing the CRC (Lucene90BlockTreeTermsReader
/// .java:328-336, Lucene912PostingsReader.java:150-172).
pub fn check_footer_structure(input: &IndexInput, expected_length: u64) -> io::Result<()> {
    if input.length() != expected_length {
        return Err(corrupt(format!(
            "length mismatch: expected {expected_length}, found {}",
            input.length()
        )));
    }
    if expected_length < FOOTER_LENGTH as u64 {
        return Err(corrupt("file shorter than footer"));
    }
    let mut tail = input.slice(expected_length - FOOTER_LENGTH as u64, FOOTER_LENGTH as u64)?;
    if read_be_int(&mut tail)? != FOOTER_MAGIC {
        return Err(corrupt("bad footer magic"));
    }
    if read_be_int(&mut tail)? != FOOTER_ALGORITHM_ID {
        return Err(corrupt("bad footer algorithmID"));
    }
    Ok(())
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

    #[test]
    fn check_index_header_round_trip() {
        let id = [0x5Au8; 16];
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        write_index_header(&mut out, "Lucene90SegmentInfo", 0, &id, "").unwrap();
        let bytes = out.into_bytes();
        let mut input = ChecksumIndexInput::new(IndexInput::in_memory(bytes));
        let v = check_index_header(&mut input, "Lucene90SegmentInfo", 0, 0, &id, "").unwrap();
        assert_eq!(v, 0);
    }

    #[test]
    fn check_index_header_rejects_mismatch() {
        let id = [0x5Au8; 16];
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        write_index_header(&mut out, "segments", 10, &id, "2").unwrap();
        let bytes = out.into_bytes();
        // wrong codec name
        let mut i1 = ChecksumIndexInput::new(IndexInput::in_memory(bytes.clone()));
        assert!(check_index_header(&mut i1, "Segments", 10, 10, &id, "2").is_err());
        // wrong version range
        let mut i2 = ChecksumIndexInput::new(IndexInput::in_memory(bytes.clone()));
        assert!(check_index_header(&mut i2, "segments", 11, 12, &id, "2").is_err());
        // wrong id
        let mut i3 = ChecksumIndexInput::new(IndexInput::in_memory(bytes.clone()));
        assert!(check_index_header(&mut i3, "segments", 10, 10, &[0u8; 16], "2").is_err());
        // wrong suffix
        let mut i4 = ChecksumIndexInput::new(IndexInput::in_memory(bytes));
        assert!(check_index_header(&mut i4, "segments", 10, 10, &id, "3").is_err());
    }
}
