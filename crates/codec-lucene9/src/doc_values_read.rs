//! Lucene90 DocValues 顺序读（Lucene90DocValuesProducer :197-299 的归并子集；
//! M6 T-C，spec §4.3）。只服务 forceMerge 的全量顺序遍历：无随机点查、
//! 无跳表加速（IndexedDISI jump table 与 terms reverse index 解析但跳过）。
//! 布局 ground truth：docs/format-notes-docvalues.md + doc_values.rs 写侧注释。

use std::io;

use crate::codec_util::{check_footer, check_footer_structure, check_index_header, corrupt};
use crate::directory::FSDirectory;
use crate::doc_values::{
    DATA_CODEC, DIRECT_MONOTONIC_BLOCK_SHIFT, DISI_BLOCK_SIZE, DISI_MAX_ARRAY_LENGTH,
    DISI_SENTINEL_BLOCK, META_CODEC, TERMS_DICT_BLOCK_SIZE, TERMS_DICT_REVERSE_INDEX_SIZE,
    TYPE_NUMERIC, TYPE_SORTED, VERSION,
};
use crate::io::{ChecksumIndexInput, DataInput, IndexInput};
use crate::packed::{DirectMonotonicReader, DirectReader};

/// Lucene90DocValuesProducer.readNumeric (:197-224) 的归并子集。
#[derive(Debug)]
struct NumericMeta {
    docs_offset: i64, // -2 = 全空；-1 = 稠密；否则 DISI 区起点（.dvd 绝对 fp）
    docs_length: i64,
    num_values: u64,
    bpv: u8,
    min: i64,
    gcd: i64,
    values_offset: i64,
    values_length: i64,
}

/// readTermDict (:278-299)：ords 子条目 + terms dict 元数据。
/// terms reverse index 的偏移量读入即弃（归并不需要点查）。
#[derive(Debug)]
struct SortedMeta {
    ords: NumericMeta,
    dict_size: u64,
    block_shift: u32,
    addresses_meta: Vec<u8>, // DirectMonotonic meta（.dvm 内联 21B/块）
    terms_data_offset: i64,
    terms_data_length: i64,
    terms_addresses_offset: i64,
    terms_addresses_length: i64,
}

#[derive(Debug)]
enum DvEntry {
    Numeric(NumericMeta),
    Sorted(SortedMeta),
}

#[derive(Debug)]
pub struct DocValuesReader {
    /// .dvd header 之后的全部字节（footer 除外）；归并逐字段顺序消费。
    dvd: Vec<u8>,
    /// .dvd index header 长度：meta 里的 offset 是绝对 fp，切片时减之。
    header_len: u64,
    entries: Vec<(i32, DvEntry)>,
}

/// readNumeric (:197-224)。tableSize 恒 -1、valueJumpTableOffset 恒 -1
/// （写侧 doc_values.rs:221/238）；其他值是非本系统产物，拒绝。
fn read_numeric_meta(dvm: &mut ChecksumIndexInput) -> io::Result<NumericMeta> {
    let docs_offset = dvm.read_long()?;
    let docs_length = dvm.read_long()?;
    let _jump_table_entry_count = dvm.read_short()?;
    let _dense_rank_power = dvm.read_byte()?;
    let num_values = dvm.read_long()? as u64;
    let table_size = dvm.read_int()?;
    if table_size != -1 {
        return Err(corrupt(format!(
            "tableSize {table_size} != -1 (not our writer)"
        )));
    }
    let bpv = dvm.read_byte()?;
    let min = dvm.read_long()?;
    let gcd = dvm.read_long()?;
    let values_offset = dvm.read_long()?;
    let values_length = dvm.read_long()?;
    let value_jump_table_offset = dvm.read_long()?;
    if value_jump_table_offset != -1 {
        return Err(corrupt("valueJumpTable present (not our writer)"));
    }
    Ok(NumericMeta {
        docs_offset,
        docs_length,
        num_values,
        bpv,
        min,
        gcd,
        values_offset,
        values_length,
    })
}

/// readTermDict (:278-299)。reverse index 的 DM meta 内联在 .dvm——
/// 必须读过（字节数按块数公式推出）才能到下一字段条目。
fn read_terms_meta(dvm: &mut ChecksumIndexInput, ords: NumericMeta) -> io::Result<SortedMeta> {
    let dict_size = dvm.read_vlong()? as u64;
    let block_shift = dvm.read_int()? as u32;
    if block_shift != DIRECT_MONOTONIC_BLOCK_SHIFT {
        return Err(corrupt(format!("terms dict blockShift {block_shift}")));
    }
    let num_addr = (dict_size as usize).div_ceil(TERMS_DICT_BLOCK_SIZE);
    let mut addresses_meta = vec![0u8; dm_meta_len(num_addr, block_shift)];
    dvm.read_bytes(&mut addresses_meta)?;
    let _max_term_length = dvm.read_int()?;
    let _max_block_length = dvm.read_int()?;
    let terms_data_offset = dvm.read_long()?;
    let terms_data_length = dvm.read_long()?;
    let terms_addresses_offset = dvm.read_long()?;
    let terms_addresses_length = dvm.read_long()?;
    let index_shift = dvm.read_int()? as u32;
    if index_shift != 10 {
        return Err(corrupt(format!("terms index shift {index_shift}")));
    }
    let num_index = 1 + (dict_size as usize).div_ceil(TERMS_DICT_REVERSE_INDEX_SIZE);
    let mut skipped = vec![0u8; dm_meta_len(num_index, block_shift)];
    dvm.read_bytes(&mut skipped)?; // reverse index DM meta，读入即弃
    let _terms_index_offset = dvm.read_long()?;
    let _terms_index_length = dvm.read_long()?;
    let _terms_index_addresses_offset = dvm.read_long()?;
    let _terms_index_addresses_length = dvm.read_long()?;
    Ok(SortedMeta {
        ords,
        dict_size,
        block_shift,
        addresses_meta,
        terms_data_offset,
        terms_data_length,
        terms_addresses_offset,
        terms_addresses_length,
    })
}

/// DirectMonotonic meta 内联字节数（块数公式，packed.rs:237-241 × 21B/块）。
fn dm_meta_len(num_values: usize, block_shift: u32) -> usize {
    let num_blocks = if num_values == 0 {
        0
    } else {
        ((num_values - 1) >> block_shift) + 1
    };
    num_blocks * DirectMonotonicReader::META_RECORD_BYTES
}

impl DocValuesReader {
    /// 解析 .dvm 全部字段条目 + 校验 .dvd header/footer
    /// （Lucene90DocValuesProducer 构造 :168-195 的精简版）。
    pub fn open(
        dir: &FSDirectory,
        segment: &str,
        segment_id: &[u8; 16],
        suffix: &str,
    ) -> io::Result<Self> {
        let [dvd_name, dvm_name] = crate::doc_values::file_names(segment, suffix);
        let mut dvm = dir.open_checksum_input(&dvm_name)?;
        check_index_header(&mut dvm, META_CODEC, VERSION, VERSION, segment_id, suffix)?;
        let mut entries = Vec::new();
        loop {
            let field_number = dvm.read_int()?;
            if field_number == -1 {
                break; // EOF marker（写侧 doc_values.rs:163）
            }
            match dvm.read_byte()? {
                TYPE_NUMERIC => {
                    let m = read_numeric_meta(&mut dvm)?;
                    entries.push((field_number, DvEntry::Numeric(m)));
                }
                TYPE_SORTED => {
                    let ords = read_numeric_meta(&mut dvm)?;
                    let m = read_terms_meta(&mut dvm, ords)?;
                    entries.push((field_number, DvEntry::Sorted(m)));
                }
                t => return Err(corrupt(format!("unsupported DV type {t}"))),
            }
        }
        check_footer(&mut dvm)?;

        let mut dvd_in = dir.open_input(&dvd_name)?;
        check_index_header(
            &mut dvd_in,
            DATA_CODEC,
            VERSION,
            VERSION,
            segment_id,
            suffix,
        )?;
        let header_len = dvd_in.file_pointer();
        check_footer_structure(&dvd_in, dvd_in.length())?;
        let mut dvd = vec![0u8; (dvd_in.length() - header_len - 16) as usize]; // 16 = footer
        dvd_in.read_bytes(&mut dvd)?;
        Ok(DocValuesReader {
            dvd,
            header_len,
            entries,
        })
    }

    /// meta 里的 offset 是 .dvd 绝对 fp；self.dvd 以 header 末尾为 0 基。
    fn slice(&self, offset: i64, length: i64) -> io::Result<&[u8]> {
        if offset < 0 || length < 0 {
            return Err(corrupt("negative DV slice bounds"));
        }
        let start = (offset as u64)
            .checked_sub(self.header_len)
            .ok_or_else(|| corrupt("DV offset before data"))? as usize;
        let end = start
            .checked_add(length as usize)
            .ok_or_else(|| corrupt("DV slice end overflow"))?;
        if end > self.dvd.len() {
            return Err(corrupt(format!(
                "DV slice [{}, {}) exceeds dvd length {}",
                start,
                end,
                self.dvd.len()
            )));
        }
        Ok(&self.dvd[start..end])
    }

    /// 读取 region 中 [p, p+2) 的 LE u16，越界时返回 InvalidData。
    fn read_u16_le(region: &[u8], p: usize) -> io::Result<u16> {
        if p + 2 > region.len() {
            return Err(corrupt("truncated DISI u16"));
        }
        Ok(u16::from_le_bytes(region[p..p + 2].try_into().unwrap()))
    }

    /// 读取 region 中 [p, p+8) 的 LE u64，越界时返回 InvalidData。
    fn read_u64_le(region: &[u8], p: usize) -> io::Result<u64> {
        if p + 8 > region.len() {
            return Err(corrupt("truncated DISI u64"));
        }
        Ok(u64::from_le_bytes(region[p..p + 8].try_into().unwrap()))
    }

    fn numeric_meta(&self, field_number: i32) -> Option<&NumericMeta> {
        self.entries
            .iter()
            .find(|(n, _)| *n == field_number)
            .and_then(|(_, e)| match e {
                DvEntry::Numeric(m) => Some(m),
                DvEntry::Sorted(_) => None,
            })
    }

    fn sorted_meta(&self, field_number: i32) -> Option<&SortedMeta> {
        self.entries
            .iter()
            .find(|(n, _)| *n == field_number)
            .and_then(|(_, e)| match e {
                DvEntry::Sorted(m) => Some(m),
                DvEntry::Numeric(_) => None,
            })
    }

    /// docsWithField（IndexedDISI 顺序解码，IndexedDISI.java:102-254）：
    /// docs_offset==-2 → 空；==-1 → 0..num_values（稠密）；否则逐块——块头
    /// LE short blockID + LE short cardinality-1；SPARSE（≤4095：LE short
    /// 低 16 位）、DENSE（256B rank 跳过 + 1024 LE long 位图展开）、
    /// ALL（==65536：无 payload）；sentinel 块（blockID == 0x7FFF）止；
    /// jump table 在块区末尾，顺序读不消费。
    fn read_docs_with_field(&self, m: &NumericMeta) -> io::Result<Vec<u32>> {
        if m.docs_offset == -2 {
            return Ok(Vec::new());
        }
        if m.docs_offset == -1 {
            return Ok((0..m.num_values as u32).collect());
        }
        let region = self.slice(m.docs_offset, m.docs_length)?;
        let mut docs = Vec::with_capacity(m.num_values as usize);
        let mut pos = 0usize;
        loop {
            if pos + 4 > region.len() {
                return Err(corrupt("truncated DISI block header"));
            }
            let block_id = Self::read_u16_le(region, pos)? as u32;
            let cardinality = Self::read_u16_le(region, pos + 2)? as u32 + 1;
            pos += 4;
            if block_id == DISI_SENTINEL_BLOCK {
                break;
            }
            if cardinality <= DISI_MAX_ARRAY_LENGTH {
                for _ in 0..cardinality {
                    docs.push((block_id << 16) | Self::read_u16_le(region, pos)? as u32);
                    pos += 2;
                }
            } else if cardinality == DISI_BLOCK_SIZE {
                docs.extend((0..DISI_BLOCK_SIZE).map(|i| (block_id << 16) | i));
            } else {
                // DENSE: 256B rank table + 1024 LE longs
                if pos + 256 + 1024 * 8 > region.len() {
                    return Err(corrupt("truncated DISI dense block"));
                }
                pos += 256; // rank table
                for word_index in 0..1024usize {
                    let mut w = Self::read_u64_le(region, pos)?;
                    pos += 8;
                    while w != 0 {
                        let bit = w.trailing_zeros();
                        docs.push((block_id << 16) | ((word_index as u32) << 6) | bit);
                        w &= w - 1;
                    }
                }
            }
        }
        if docs.len() as u64 != m.num_values {
            return Err(corrupt(format!(
                "DISI docs count mismatch: expected {}, got {}",
                m.num_values,
                docs.len()
            )));
        }
        Ok(docs)
    }

    /// 值流：bpv==0 → vec![min; num_values]（producer :487-493）；否则
    /// DirectReader 逐值 `min + gcd * get(i)`（:527-534；gcd 恒 1 按通用解）。
    fn read_values(&self, m: &NumericMeta) -> io::Result<Vec<i64>> {
        if m.bpv == 0 {
            return Ok(vec![m.min; m.num_values as usize]);
        }
        let data = self.slice(m.values_offset, m.values_length)?;
        // 校验数据区足够容纳 num_values 个值（bpv 位/值，offset=0）。
        let bits_needed = m
            .num_values
            .checked_mul(m.bpv as u64)
            .ok_or_else(|| corrupt("numeric values size overflow"))?;
        let bytes_needed = (bits_needed + 7) / 8;
        if data.len() < bytes_needed as usize {
            return Err(corrupt(format!(
                "numeric values truncated: need {} bytes, have {}",
                bytes_needed,
                data.len()
            )));
        }
        let reader = DirectReader::new(data, m.bpv as u32, 0)?;
        Ok((0..m.num_values)
            .map(|i| {
                (reader.get(i) as i64)
                    .wrapping_mul(m.gcd)
                    .wrapping_add(m.min)
            })
            .collect())
    }

    /// 逐 doc (doc, value)，doc 升序。全空 → 空 Vec。
    pub fn numeric_values(&self, field_number: i32) -> io::Result<Vec<(u32, i64)>> {
        let Some(m) = self.numeric_meta(field_number) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no NUMERIC DV entry for field {field_number}"),
            ));
        };
        let docs = self.read_docs_with_field(m)?;
        let values = self.read_values(m)?;
        if docs.len() != values.len() {
            return Err(corrupt(format!(
                "numeric docs/values length mismatch: {} vs {}",
                docs.len(),
                values.len()
            )));
        }
        Ok(docs.into_iter().zip(values).collect())
    }

    /// 逐 doc (doc, ord)，doc 升序：ords 子条目走 numeric 同一路径。
    pub fn sorted_ords(&self, field_number: i32) -> io::Result<Vec<(u32, u32)>> {
        let Some(s) = self.sorted_meta(field_number) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no SORTED DV entry for field {field_number}"),
            ));
        };
        let docs = self.read_docs_with_field(&s.ords)?;
        let values = self.read_values(&s.ords)?;
        Ok(docs
            .into_iter()
            .zip(values)
            .map(|(d, o)| (d, o as u32))
            .collect())
    }

    /// terms dict 全量展开：64 项/块，块首词 verbatim（VInt 长度 + 字节），
    /// 其余在 `VInt uncompressedLength + LZ4 流` 内前缀压缩（token 低 4 位
    /// prefix（15 ⇒ +VInt 续）、高 4 位 suffix-1（=15 ⇒ suffix = 16+VInt）——
    /// 写侧 doc_values.rs:280-291 的逆；块地址 DirectMonotonic :578）。
    pub fn sorted_dict(&self, field_number: i32) -> io::Result<Vec<Vec<u8>>> {
        let Some(s) = self.sorted_meta(field_number) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no SORTED DV entry for field {field_number}"),
            ));
        };
        if s.dict_size == 0 {
            return Ok(Vec::new());
        }
        let num_blocks = (s.dict_size as usize).div_ceil(TERMS_DICT_BLOCK_SIZE);
        let addrs = DirectMonotonicReader::new(
            &s.addresses_meta,
            self.slice(s.terms_addresses_offset, s.terms_addresses_length)?,
            num_blocks,
            s.block_shift,
        )?;
        let data = self.slice(s.terms_data_offset, s.terms_data_length)?;
        let mut terms = Vec::with_capacity(s.dict_size as usize);
        for b in 0..num_blocks {
            let start = addrs.get(b as u64) as usize;
            let end = if b + 1 < num_blocks {
                addrs.get(b as u64 + 1) as usize
            } else {
                data.len()
            };
            if start > end || end > data.len() {
                return Err(corrupt("terms dict block bounds"));
            }
            let region = &data[start..end];
            let mut r = IndexInput::in_memory(region.to_vec());
            let first_len = r.read_vint()? as usize;
            let mut first = vec![0u8; first_len];
            r.read_bytes(&mut first)?;
            terms.push(first.clone());
            let block_count =
                (s.dict_size as usize - b * TERMS_DICT_BLOCK_SIZE).min(TERMS_DICT_BLOCK_SIZE);
            if block_count > 1 {
                let uncompressed = r.read_vint()? as usize;
                let consumed = r.file_pointer() as usize;
                if consumed > region.len() {
                    return Err(corrupt("terms dict compressed length overflow"));
                }
                let mut compressed = vec![0u8; region.len() - consumed];
                r.read_bytes(&mut compressed)?;
                let decompressed = lz4::block::decompress(&compressed, Some(uncompressed as i32))
                    .map_err(|e| corrupt(format!("terms dict lz4: {e}")))?;
                let mut dr = IndexInput::in_memory(decompressed);
                let mut prev = first;
                for _ in 1..block_count {
                    let token = dr.read_byte()? as usize;
                    let mut prefix = token & 0x0F;
                    let mut suffix = 1 + (token >> 4);
                    if prefix == 15 {
                        prefix += dr.read_vint()? as usize;
                    }
                    if suffix == 16 {
                        suffix += dr.read_vint()? as usize;
                    }
                    let mut sfx = vec![0u8; suffix];
                    dr.read_bytes(&mut sfx)?;
                    if prefix > prev.len() {
                        return Err(corrupt("terms dict prefix exceeds prev term"));
                    }
                    let mut term = prev[..prefix].to_vec();
                    term.extend_from_slice(&sfx);
                    prev = term.clone();
                    terms.push(term);
                }
            }
        }
        Ok(terms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc_values::DocValuesWriter;
    use std::fs;
    use std::path::PathBuf;

    const SEGMENT_ID: [u8; 16] = [0x5A; 16];
    const SUFFIX: &str = "Lucene90_0";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("codec-lucene9-dvr-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn write_index(tag: &str, f: impl FnOnce(&mut DocValuesWriter)) -> PathBuf {
        let root = temp_dir(tag);
        let dir = FSDirectory::open(&root).unwrap();
        let mut w = DocValuesWriter::new(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        f(&mut w);
        w.finish().unwrap();
        root
    }

    #[test]
    fn numeric_dense_sparse_empty() {
        let max_doc = 200_000u32;
        // field 0: 稠密常量（bpv 0、无 DISI）
        let constant: Vec<(u32, i64)> = (0..1000).map(|d| (d, 42)).collect();
        // field 1: 稀疏三形态 DISI（照抄 doc_values.rs 既有测试的分布）
        let mut sparse: Vec<(u32, i64)> = vec![(5, -7), (100, 8), (4095, 9)];
        for i in 0..5000u32 {
            sparse.push((65536 + i, i as i64 * 3));
        }
        for i in 0..65536u32 {
            sparse.push((131072 + i, -1));
        }
        // field 2: 全空（docsWithField = -2 分支）
        let root = write_index("num", |w| {
            w.add_numeric_field(0, max_doc, &constant).unwrap();
            w.add_numeric_field(1, max_doc, &sparse).unwrap();
            w.add_numeric_field(2, max_doc, &[]).unwrap();
        });
        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        assert_eq!(r.numeric_values(0).unwrap(), constant);
        assert_eq!(r.numeric_values(1).unwrap(), sparse);
        assert_eq!(r.numeric_values(2).unwrap(), Vec::<(u32, i64)>::new());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sorted_dict_and_ords() {
        // 150 词 → 3 个 64 项块；ords 部分 doc 无值（docsWithField 稀疏）
        let dict: Vec<String> = (0..150).map(|i| format!("term-{i:04}")).collect();
        let dict_refs: Vec<&[u8]> = dict.iter().map(|s| s.as_bytes()).collect();
        let ords: Vec<(u32, u32)> = (0..300u32)
            .filter(|d| d % 3 != 0) // 200/300 docs 有值 → DISI 稀疏路径
            .enumerate()
            .map(|(i, d)| (d, (i % 150) as u32)) // 200 个有值 doc 覆盖 150 个 ord
            .collect();
        let root = write_index("sorted", |w| {
            w.add_sorted_field(0, 300, &dict_refs, &ords).unwrap();
        });
        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let got_dict = r.sorted_dict(0).unwrap();
        assert_eq!(got_dict.len(), 150);
        for (got, want) in got_dict.iter().zip(dict.iter()) {
            assert_eq!(got, want.as_bytes());
        }
        assert_eq!(r.sorted_ords(0).unwrap(), ords);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn empty_sorted_field() {
        let root = write_index("empty", |w| {
            w.add_sorted_field(1, 10, &[], &[]).unwrap();
        });
        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        assert!(r.sorted_dict(1).unwrap().is_empty());
        assert!(r.sorted_ords(1).unwrap().is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    // ---- corrupt-data tests: must return InvalidData, not panic ----

    use crate::codec_util::index_header_length;

    fn dvm_bpv_offset() -> usize {
        // header + field_number(i32) + type(u8) + docs_offset(i64) + docs_length(i64)
        // + jump_table_entry_count(i16) + dense_rank_power(i8) + num_values(i64) + table_size(i32)
        index_header_length(META_CODEC, SUFFIX) + 4 + 1 + 8 + 8 + 2 + 1 + 8 + 4
    }

    /// 修改 .dvm 内容后重算 footer CRC（覆盖 0..len-8）。
    fn rewrite_dvm_crc(bytes: &mut [u8]) {
        let n = bytes.len();
        let crc = crate::codec_util::crc32(&bytes[..n - 8]);
        bytes[n - 8..].copy_from_slice(&crc.to_be_bytes());
    }

    fn dvm_num_values_offset() -> usize {
        // header + field_number(i32) + type(u8) + docs_offset(i64) + docs_length(i64)
        // + jump_table_entry_count(i16) + dense_rank_power(i8)
        index_header_length(META_CODEC, SUFFIX) + 4 + 1 + 8 + 8 + 2 + 1
    }

    fn dvm_docs_offset() -> usize {
        // header + field_number(i32) + type(u8)
        index_header_length(META_CODEC, SUFFIX) + 4 + 1
    }

    fn dvm_values_offset_offset() -> usize {
        // bpv(1) + min(i64) + gcd(i64)
        dvm_bpv_offset() + 1 + 8 + 8
    }

    fn dvm_values_length_offset() -> usize {
        dvm_values_offset_offset() + 8
    }

    #[test]
    fn corrupt_dvd_truncated_open_fails() {
        let root = write_index("corrupt-open", |w| {
            w.add_numeric_field(0, 100, &[(0, 1), (50, 2)]).unwrap();
        });
        let [dvd_name, _dvm_name] = crate::doc_values::file_names("_0", SUFFIX);
        let dvd_path = root.join(&dvd_name);
        let header_len = index_header_length(DATA_CODEC, SUFFIX) as u64;
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&dvd_path)
            .unwrap();
        f.set_len(header_len + 1).unwrap();
        let dir = FSDirectory::open(&root).unwrap();
        let err = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn corrupt_bpv_numeric_values_fails() {
        let root = write_index("corrupt-bpv", |w| {
            w.add_numeric_field(0, 100, &[(0, 1), (50, 2)]).unwrap();
        });
        let [_dvd_name, dvm_name] = crate::doc_values::file_names("_0", SUFFIX);
        let dvm_path = root.join(&dvm_name);
        let mut bytes = fs::read(&dvm_path).unwrap();
        bytes[dvm_bpv_offset()] = 7; // unsupported bpv
        rewrite_dvm_crc(&mut bytes);
        fs::write(&dvm_path, bytes).unwrap();

        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let err = r.numeric_values(0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn corrupt_docs_length_numeric_values_fails() {
        let root = write_index("corrupt-disi", |w| {
            w.add_numeric_field(0, 100, &[(0, 1), (50, 2)]).unwrap();
        });
        let [_dvd_name, dvm_name] = crate::doc_values::file_names("_0", SUFFIX);
        let dvm_path = root.join(&dvm_name);
        let mut bytes = fs::read(&dvm_path).unwrap();
        // docs_length is right after docs_offset in the numeric meta.
        let docs_length_offset = index_header_length(META_CODEC, SUFFIX) + 4 + 1 + 8;
        bytes[docs_length_offset..docs_length_offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        rewrite_dvm_crc(&mut bytes);
        fs::write(&dvm_path, bytes).unwrap();

        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let err = r.numeric_values(0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn corrupt_bpv_sorted_ords_fails() {
        let dict: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let dict_refs: Vec<&[u8]> = dict.iter().map(|s| s.as_bytes()).collect();
        let ords: Vec<(u32, u32)> = (0..20u32).map(|d| (d, d % 10)).collect();
        let root = write_index("corrupt-sorted", |w| {
            w.add_sorted_field(0, 20, &dict_refs, &ords).unwrap();
        });
        let [_dvd_name, dvm_name] = crate::doc_values::file_names("_0", SUFFIX);
        let dvm_path = root.join(&dvm_name);
        let mut bytes = fs::read(&dvm_path).unwrap();
        bytes[dvm_bpv_offset()] = 7; // ords numeric meta bpv
        rewrite_dvm_crc(&mut bytes);
        fs::write(&dvm_path, bytes).unwrap();

        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let err = r.sorted_ords(0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn corrupt_num_values_overflow_fails() {
        // num_values * bpv 溢出 → read_values 返回 InvalidData 而非 panic。
        let root = write_index("corrupt-overflow", |w| {
            w.add_numeric_field(0, 100, &[(0, 1), (50, 2)]).unwrap();
        });
        let [_dvd_name, dvm_name] = crate::doc_values::file_names("_0", SUFFIX);
        let dvm_path = root.join(&dvm_name);
        let mut bytes = fs::read(&dvm_path).unwrap();
        bytes[dvm_docs_offset()..dvm_docs_offset() + 8].copy_from_slice(&(-2i64).to_le_bytes());
        bytes[dvm_num_values_offset()..dvm_num_values_offset() + 8]
            .copy_from_slice(&u64::MAX.to_le_bytes());
        bytes[dvm_bpv_offset()] = 64;
        rewrite_dvm_crc(&mut bytes);
        fs::write(&dvm_path, bytes).unwrap();

        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let err = r.numeric_values(0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn corrupt_values_bounds_overflow_fails() {
        // values_offset/length 极大 → slice end 溢出/越界，返回 InvalidData。
        let root = write_index("corrupt-bounds", |w| {
            w.add_numeric_field(0, 100, &[(0, 1), (50, 2)]).unwrap();
        });
        let [_dvd_name, dvm_name] = crate::doc_values::file_names("_0", SUFFIX);
        let dvm_path = root.join(&dvm_name);
        let mut bytes = fs::read(&dvm_path).unwrap();
        bytes[dvm_values_offset_offset()..dvm_values_offset_offset() + 8]
            .copy_from_slice(&i64::MAX.to_le_bytes());
        bytes[dvm_values_length_offset()..dvm_values_length_offset() + 8]
            .copy_from_slice(&i64::MAX.to_le_bytes());
        rewrite_dvm_crc(&mut bytes);
        fs::write(&dvm_path, bytes).unwrap();

        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let err = r.numeric_values(0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn corrupt_docs_count_mismatch_fails() {
        // 稀疏 numeric 字段的 num_values 与实际 DISI docs 数量不符，
        // read_docs_with_field 运行时检查返回 InvalidData。
        let root = write_index("corrupt-docs-count", |w| {
            w.add_numeric_field(0, 100, &[(0, 1), (50, 2)]).unwrap();
        });
        let [_dvd_name, dvm_name] = crate::doc_values::file_names("_0", SUFFIX);
        let dvm_path = root.join(&dvm_name);
        let mut bytes = fs::read(&dvm_path).unwrap();
        bytes[dvm_num_values_offset()..dvm_num_values_offset() + 8]
            .copy_from_slice(&1u64.to_le_bytes());
        rewrite_dvm_crc(&mut bytes);
        fs::write(&dvm_path, bytes).unwrap();

        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let err = r.numeric_values(0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn corrupt_terms_dict_prefix_overflow_fails() {
        // terms dict LZ4 解压流的 prefix 超过前一词长度 → sorted_dict 返回
        // InvalidData 而非 slice panic（.dvd 数据区无 CRC 兜底，bit-rot 直达）。
        let dict: Vec<String> = vec!["aaa".into(), "aab".into()];
        let dict_refs: Vec<&[u8]> = dict.iter().map(|s| s.as_bytes()).collect();
        let ords: Vec<(u32, u32)> = vec![(0, 0), (1, 1)];
        let root = write_index("corrupt-prefix", |w| {
            w.add_sorted_field(0, 2, &dict_refs, &ords).unwrap();
        });
        // 借 meta 定位 terms dict 区域（单块）：VInt first_len + first +
        // VInt uncompressed + LZ4 块；原位替换为恶意流（prefix=5 > "aaa".len()=3）。
        let dir0 = FSDirectory::open(&root).unwrap();
        let r0 = DocValuesReader::open(&dir0, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let (off, len) = match &r0.entries[0].1 {
            DvEntry::Sorted(m) => (
                m.terms_data_offset as usize, // meta offset 是 .dvd 绝对 fp
                m.terms_data_length as usize,
            ),
            _ => panic!("expected sorted field"),
        };
        drop(r0);
        drop(dir0);
        let [dvd_name, _] = crate::doc_values::file_names("_0", SUFFIX);
        let dvd_path = root.join(&dvd_name);
        let mut bytes = fs::read(&dvd_path).unwrap();
        let region = &bytes[off..off + len];
        assert_eq!(region[0], 3, "first term len"); // "aaa"
        let p = 1 + 3;
        assert_eq!(region[p], 2, "uncompressed length"); // 1 entry: token + suffix
        let old_compressed = &region[p + 1..];
        // token = prefix 5 | (suffix-1)=0 << 4；解压后恰 2 字节。
        let malicious = lz4::block::compress(
            &[0x05u8, b'z'],
            Some(lz4::block::CompressionMode::FAST(2)),
            false,
        )
        .unwrap();
        assert_eq!(
            malicious.len(),
            old_compressed.len(),
            "等长原位替换保持后续区域偏移不变"
        );
        bytes[off + p + 1..off + len].copy_from_slice(&malicious);
        fs::write(&dvd_path, bytes).unwrap();

        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let err = r.sorted_dict(0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(&root).unwrap();
    }
}
