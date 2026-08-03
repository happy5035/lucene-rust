//! Lucene90 DocValues 顺序读（Lucene90DocValuesProducer :197-299 的归并子集；
//! M6 T-C，spec §4.3）。只服务 forceMerge 的全量顺序遍历：无随机点查、
//! 无跳表加速（IndexedDISI jump table 与 terms reverse index 解析但跳过）。
//! 布局 ground truth：docs/format-notes-docvalues.md + doc_values.rs 写侧注释。

use std::io;
use std::sync::Arc;

use crate::codec_util::{check_footer, check_footer_structure, check_index_header, corrupt};
use crate::directory::FSDirectory;
use crate::doc_values::{
    DATA_CODEC, DIRECT_MONOTONIC_BLOCK_SHIFT, DISI_BLOCK_SIZE, DISI_MAX_ARRAY_LENGTH,
    DISI_SENTINEL_BLOCK, META_CODEC, TERMS_DICT_BLOCK_SIZE, TERMS_DICT_REVERSE_INDEX_SIZE,
    TYPE_BINARY, TYPE_NUMERIC, TYPE_SORTED, VERSION,
};
use crate::io::{ChecksumIndexInput, DataInput, SliceInput};
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

/// readBinary (:226-248)：BINARY 字段元数据。
#[derive(Debug)]
struct BinaryMeta {
    data_offset: i64,
    data_length: i64,
    docs_offset: i64, // -2 空, -1 稠密, 或 DISI 区起点
    docs_length: i64,
    num_values: u32,
    min_length: i32,
    max_length: i32,
    addresses_meta: Option<Vec<u8>>, // DM meta（仅变长）
    addresses_offset: i64,           // 仅变长
    addresses_length: i64,           // 仅变长
}

#[derive(Debug)]
enum DvEntry {
    Numeric(NumericMeta),
    Sorted(SortedMeta),
    Binary(BinaryMeta),
}

#[derive(Debug)]
pub struct DocValuesReader {
    /// 整个 .dvd 文件的 mmap（含 header/footer）；meta 里的 offset 即文件内
    /// 绝对偏移，slice 直接按之切，不做堆拷贝。
    dvd: Arc<memmap2::Mmap>,
    /// .dvd index header 长度：数据区起点；slice 校验 offset 不落进 header。
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

/// readBinary (:226-248): parse BINARY field metadata from .dvm.
fn read_binary_meta(dvm: &mut ChecksumIndexInput) -> io::Result<BinaryMeta> {
    let data_offset = dvm.read_long()?;
    let data_length = dvm.read_long()?;
    let docs_offset = dvm.read_long()?;
    let docs_length = dvm.read_long()?;
    let _jump_table_entry_count = dvm.read_short()?;
    let _dense_rank_power = dvm.read_byte()?;
    let num_values = dvm.read_int()? as u32;
    let min_length = dvm.read_int()?;
    let max_length = dvm.read_int()?;
    let (addresses_meta, addresses_offset, addresses_length) = if min_length < max_length {
        let addr_offset = dvm.read_long()?;
        let block_shift = dvm.read_vint()? as u32;
        let num_addr = num_values as usize + 1;
        let mut meta_bytes = vec![0u8; dm_meta_len(num_addr, block_shift)];
        dvm.read_bytes(&mut meta_bytes)?;
        let addr_length = dvm.read_long()?;
        (Some(meta_bytes), addr_offset, addr_length)
    } else {
        (None, 0, 0)
    };
    Ok(BinaryMeta {
        data_offset,
        data_length,
        docs_offset,
        docs_length,
        num_values,
        min_length,
        max_length,
        addresses_meta,
        addresses_offset,
        addresses_length,
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
                TYPE_BINARY => {
                    let m = read_binary_meta(&mut dvm)?;
                    entries.push((field_number, DvEntry::Binary(m)));
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
        drop(dvd_in);
        // 校验通过后持有整文件 mmap，逐字段切片零拷贝消费。
        let dvd = dir.open_mmap(&dvd_name)?;
        Ok(DocValuesReader {
            dvd,
            header_len,
            entries,
        })
    }

    /// meta 里的 offset 是 .dvd 绝对 fp；self.dvd 以整个文件开头为 0 基，
    /// 直接用 offset 切片（不减 header_len）。
    fn slice(&self, offset: i64, length: i64) -> io::Result<&[u8]> {
        if offset < 0 || length < 0 {
            return Err(corrupt("negative DV slice bounds"));
        }
        if (offset as u64) < self.header_len {
            return Err(corrupt("DV offset before data"));
        }
        let start = offset as usize;
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

    fn numeric_meta(&self, field_number: i32) -> Option<&NumericMeta> {
        self.entries
            .iter()
            .find(|(n, _)| *n == field_number)
            .and_then(|(_, e)| match e {
                DvEntry::Numeric(m) => Some(m),
                _ => None,
            })
    }

    fn sorted_meta(&self, field_number: i32) -> Option<&SortedMeta> {
        self.entries
            .iter()
            .find(|(n, _)| *n == field_number)
            .and_then(|(_, e)| match e {
                DvEntry::Sorted(m) => Some(m),
                _ => None,
            })
    }

    fn binary_meta(&self, field_number: i32) -> Option<&BinaryMeta> {
        self.entries
            .iter()
            .find(|(n, _)| *n == field_number)
            .and_then(|(_, e)| match e {
                DvEntry::Binary(m) => Some(m),
                _ => None,
            })
    }
}

/// docsWithField 解码结果：空 → Empty；稠密（docs_offset == -1）用 range
/// 惰性表示，不物化 Vec；稀疏 DISI 解出 Vec<u32>。
enum DocIds {
    Empty,
    Dense(std::ops::Range<u32>),
    Sparse(Vec<u32>),
}

impl DocIds {
    fn len(&self) -> usize {
        match self {
            DocIds::Empty => 0,
            DocIds::Dense(r) => r.len(),
            DocIds::Sparse(v) => v.len(),
        }
    }

    fn get(&self, i: usize) -> u32 {
        match self {
            DocIds::Empty => unreachable!("get on empty DocIds"),
            DocIds::Dense(r) => r.start + i as u32,
            DocIds::Sparse(v) => v[i],
        }
    }

    fn into_vec(self) -> Vec<u32> {
        match self {
            DocIds::Empty => Vec::new(),
            DocIds::Dense(r) => r.collect(),
            DocIds::Sparse(v) => v,
        }
    }
}

/// 稀疏 DISI 区顺序解码（IndexedDISI.java:102-254）：逐块——块头 LE short
/// blockID + LE short cardinality-1；SPARSE（≤4095：LE short 低 16 位）、
/// DENSE（256B rank 跳过 + 1024 LE long 位图展开）、ALL（==65536：无
/// payload）；sentinel 块（blockID == 0x7FFF）止；jump table 在块区末尾，
/// 顺序读不消费。
fn decode_disi_region(region: &[u8], num_values: u64) -> io::Result<Vec<u32>> {
    let mut docs = Vec::with_capacity(num_values as usize);
    let mut pos = 0usize;
    loop {
        if pos + 4 > region.len() {
            return Err(corrupt("truncated DISI block header"));
        }
        let block_id = u16::from_le_bytes(region[pos..pos + 2].try_into().unwrap()) as u32;
        let cardinality =
            u16::from_le_bytes(region[pos + 2..pos + 4].try_into().unwrap()) as u32 + 1;
        pos += 4;
        if block_id == DISI_SENTINEL_BLOCK {
            break;
        }
        if cardinality <= DISI_MAX_ARRAY_LENGTH {
            for _ in 0..cardinality {
                if pos + 2 > region.len() {
                    return Err(corrupt("truncated DISI u16"));
                }
                docs.push(
                    (block_id << 16)
                        | u16::from_le_bytes(region[pos..pos + 2].try_into().unwrap()) as u32,
                );
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
                let mut w = u64::from_le_bytes(region[pos..pos + 8].try_into().unwrap());
                pos += 8;
                while w != 0 {
                    let bit = w.trailing_zeros();
                    docs.push((block_id << 16) | ((word_index as u32) << 6) | bit);
                    w &= w - 1;
                }
            }
        }
    }
    if docs.len() as u64 != num_values {
        return Err(corrupt(format!(
            "DISI docs count mismatch: expected {}, got {}",
            num_values,
            docs.len()
        )));
    }
    Ok(docs)
}

impl DocValuesReader {
    /// docsWithField（IndexedDISI 顺序解码）：docs_offset==-2 → 空；
    /// ==-1 → 0..num_values（稠密，range 表示）；否则解 DISI 区。
    fn decode_doc_ids(
        &self,
        docs_offset: i64,
        docs_length: i64,
        num_values: u64,
    ) -> io::Result<DocIds> {
        if docs_offset == -2 {
            return Ok(DocIds::Empty);
        }
        if docs_offset == -1 {
            return Ok(DocIds::Dense(0..num_values as u32));
        }
        let region = self.slice(docs_offset, docs_length)?;
        Ok(DocIds::Sparse(decode_disi_region(region, num_values)?))
    }

    /// 值流：bpv==0 → vec![min; num_values]（producer :487-493）；否则
    /// 逐值 `min + gcd * get(i)`（:527-534；gcd 恒 1 按通用解）。bpv 字节
    /// 对齐（8/16/32/64）时走 chunks_exact 批量解码，绕开逐位 DirectReader。
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
        let n = m.num_values as usize;
        let apply =
            |v: u64| (v as i64).wrapping_mul(m.gcd).wrapping_add(m.min);
        let mut values = Vec::with_capacity(n);
        match m.bpv {
            8 => values.extend(data[..n].iter().map(|&b| apply(b as u64))),
            16 => values.extend(
                data[..n * 2]
                    .chunks_exact(2)
                    .map(|c| apply(u16::from_le_bytes([c[0], c[1]]) as u64)),
            ),
            32 => values.extend(
                data[..n * 4]
                    .chunks_exact(4)
                    .map(|c| apply(u32::from_le_bytes(c.try_into().unwrap()) as u64)),
            ),
            64 => values.extend(
                data[..n * 8]
                    .chunks_exact(8)
                    .map(|c| apply(u64::from_le_bytes(c.try_into().unwrap()))),
            ),
            _ => {
                let reader = DirectReader::new(data, m.bpv as u32, 0)?;
                values.extend((0..m.num_values).map(|i| apply(reader.get(i))));
            }
        }
        Ok(values)
    }

    /// 逐 doc (doc, value)，doc 升序。全空 → 空 Vec。稠密（docs_offset==-1）
    /// 时 doc_ids 不物化，range 直接与 values zip。
    pub fn numeric_values(&self, field_number: i32) -> io::Result<Vec<(u32, i64)>> {
        let Some(m) = self.numeric_meta(field_number) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no NUMERIC DV entry for field {field_number}"),
            ));
        };
        let docs = self.decode_doc_ids(m.docs_offset, m.docs_length, m.num_values)?;
        let values = self.read_values(m)?;
        if docs.len() != values.len() {
            return Err(corrupt(format!(
                "numeric docs/values length mismatch: {} vs {}",
                docs.len(),
                values.len()
            )));
        }
        Ok(match docs {
            DocIds::Empty => Vec::new(),
            DocIds::Dense(r) => r.zip(values).collect(),
            DocIds::Sparse(v) => v.into_iter().zip(values).collect(),
        })
    }

    /// 逐 doc (doc, ord)，doc 升序：ords 子条目走 numeric 同一路径。
    pub fn sorted_ords(&self, field_number: i32) -> io::Result<Vec<(u32, u32)>> {
        let Some(s) = self.sorted_meta(field_number) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no SORTED DV entry for field {field_number}"),
            ));
        };
        let docs = self.decode_doc_ids(s.ords.docs_offset, s.ords.docs_length, s.ords.num_values)?;
        let values = self.read_values(&s.ords)?;
        Ok(match docs {
            DocIds::Empty => Vec::new(),
            DocIds::Dense(r) => r.zip(values).map(|(d, o)| (d, o as u32)).collect(),
            DocIds::Sparse(v) => v
                .into_iter()
                .zip(values)
                .map(|(d, o)| (d, o as u32))
                .collect(),
        })
    }

    /// terms dict 全量展开：64 项/块，块首词 verbatim（VInt 长度 + 字节），
    /// 其余在 `VInt uncompressedLength + LZ4 流` 内前缀压缩（token 低 4 位
    /// prefix（15 ⇒ +VInt 续）、高 4 位 suffix-1（=15 ⇒ suffix = 16+VInt）——
    /// 写侧 doc_values.rs:280-291 的逆；块地址 DirectMonotonic :578）。
    pub fn sorted_dict(&self, field_number: i32) -> io::Result<Vec<Vec<u8>>> {
        let (buf, offsets) = self.sorted_dict_packed(field_number)?;
        Ok(offsets
            .iter()
            .map(|&(s, l)| buf[s as usize..(s + l) as usize].to_vec())
            .collect())
    }

    /// terms dict 全量展开的 packed 形式：一个连续 buffer + 每词
    /// (start, len)，供 `SortedDocValues` 零分配迭代；解码逻辑与
    /// `sorted_dict` 相同（SliceInput 直接借 mmap 区域，prev 缓冲区复用）。
    pub fn sorted_dict_packed(
        &self,
        field_number: i32,
    ) -> io::Result<(Vec<u8>, Vec<(u32, u32)>)> {
        let Some(s) = self.sorted_meta(field_number) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no SORTED DV entry for field {field_number}"),
            ));
        };
        if s.dict_size == 0 {
            return Ok((Vec::new(), Vec::new()));
        }
        let num_blocks = (s.dict_size as usize).div_ceil(TERMS_DICT_BLOCK_SIZE);
        let addrs = DirectMonotonicReader::new(
            &s.addresses_meta,
            self.slice(s.terms_addresses_offset, s.terms_addresses_length)?,
            num_blocks,
            s.block_shift,
        )?;
        let data = self.slice(s.terms_data_offset, s.terms_data_length)?;
        let mut buf: Vec<u8> = Vec::new();
        let mut offsets: Vec<(u32, u32)> = Vec::with_capacity(s.dict_size as usize);
        let mut prev: Vec<u8> = Vec::new();
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
            let mut r = SliceInput::new(region);
            let first_len = r.read_vint()? as usize;
            prev.clear();
            prev.resize(first_len, 0);
            r.read_bytes(&mut prev)?;
            let term_start = buf.len();
            buf.extend_from_slice(&prev);
            offsets.push((term_start as u32, prev.len() as u32));
            let block_count =
                (s.dict_size as usize - b * TERMS_DICT_BLOCK_SIZE).min(TERMS_DICT_BLOCK_SIZE);
            if block_count > 1 {
                let uncompressed = r.read_vint()? as usize;
                let consumed = r.position();
                if consumed > region.len() {
                    return Err(corrupt("terms dict compressed length overflow"));
                }
                let decompressed =
                    lz4::block::decompress(&region[consumed..], Some(uncompressed as i32))
                        .map_err(|e| corrupt(format!("terms dict lz4: {e}")))?;
                let mut dr = SliceInput::new(&decompressed);
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
                    if prefix > prev.len() {
                        return Err(corrupt("terms dict prefix exceeds prev term"));
                    }
                    let term_start = buf.len();
                    buf.extend_from_slice(&prev[..prefix]);
                    buf.resize(buf.len() + suffix, 0);
                    dr.read_bytes(&mut buf[term_start + prefix..])?;
                    prev.clear();
                    prev.extend_from_slice(&buf[term_start..]);
                    offsets.push((term_start as u32, (buf.len() - term_start) as u32));
                }
            }
        }
        Ok((buf, offsets))
    }

    /// BINARY field: returns (doc_id, bytes) pairs in doc order.
    /// Thin owned-collecting wrapper over [`Self::binary_doc_values`].
    pub fn binary_values(&self, field_number: i32) -> io::Result<Vec<(u32, Vec<u8>)>> {
        let mut it = self.binary_doc_values(field_number)?;
        let mut out = Vec::with_capacity(it.len());
        while let Some((doc, bytes)) = it.next() {
            out.push((doc, bytes.to_vec()));
        }
        Ok(out)
    }

    /// BINARY field zero-copy iterator: each `next` borrows the value bytes
    /// straight out of the .dvd mmap — no per-doc allocation. All bounds are
    /// validated up front (corrupt data → InvalidData at construction), so
    /// iteration itself is infallible.
    pub fn binary_doc_values(&self, field_number: i32) -> io::Result<BinaryDocValues<'_>> {
        let entry = self.binary_meta(field_number).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no BINARY DV entry for field {field_number}"),
            )
        })?;

        if entry.num_values == 0 || entry.docs_offset == -2 {
            return Ok(BinaryDocValues {
                doc_ids: DocIds::Empty,
                data: &[],
                addrs: None,
                fixed_len: 0,
                pos: 0,
            });
        }

        let doc_ids = self.decode_doc_ids(
            entry.docs_offset,
            entry.docs_length,
            entry.num_values as u64,
        )?;
        let data = self.slice(entry.data_offset, entry.data_length)?;

        if entry.min_length == entry.max_length {
            // Fixed-length: value i at [i*len, (i+1)*len).
            let len = entry.min_length as usize;
            let total = (entry.num_values as usize)
                .checked_mul(len)
                .ok_or_else(|| corrupt("binary fixed-length size overflow"))?;
            if total > data.len() {
                return Err(corrupt("binary fixed-length data truncated"));
            }
            Ok(BinaryDocValues {
                doc_ids,
                data,
                addrs: None,
                fixed_len: len,
                pos: 0,
            })
        } else {
            // Variable-length: DirectMonotonic addresses; validate every
            // offset now so `next` never fails.
            let addr_meta = entry
                .addresses_meta
                .as_ref()
                .ok_or_else(|| corrupt("binary variable-length field missing addresses meta"))?;
            let addr_data = self.slice(entry.addresses_offset, entry.addresses_length)?;
            let dm = DirectMonotonicReader::new(
                addr_meta,
                addr_data,
                entry.num_values as usize + 1,
                DIRECT_MONOTONIC_BLOCK_SHIFT,
            )?;
            for i in 0..=entry.num_values as u64 {
                if dm.get(i) as usize > data.len() {
                    return Err(corrupt("binary variable-length data truncated"));
                }
            }
            Ok(BinaryDocValues {
                doc_ids,
                data,
                addrs: Some(dm),
                fixed_len: 0,
                pos: 0,
            })
        }
    }

    /// BINARY field, packed form: `(doc_ids, data, offsets)`. `data` is the
    /// field's contiguous value region (one allocation); `offsets[i]` is the
    /// `(start, end)` byte range of the i-th value within `data`, parallel to
    /// `doc_ids`. Avoids the per-value `Vec` allocation that [`binary_values`]
    /// performs — callers that hold a whole field (e.g. a shard merge) keep one
    /// buffer and slice on demand. Doc order is ascending docID.
    pub fn binary_values_packed(
        &self,
        field_number: i32,
    ) -> io::Result<(Vec<u32>, Vec<u8>, Vec<(usize, usize)>)> {
        let entry = self.binary_meta(field_number).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no BINARY DV entry for field {field_number}"),
            )
        })?;

        if entry.num_values == 0 || entry.docs_offset == -2 {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        }

        let doc_ids = self
            .decode_doc_ids(entry.docs_offset, entry.docs_length, entry.num_values as u64)?
            .into_vec();

        // Contiguous value region (single owned copy of the field's data).
        let data = self
            .slice(entry.data_offset, entry.data_length)?
            .to_vec();

        // Per-value (start, end) offsets into `data`.
        let mut offsets = Vec::with_capacity(entry.num_values as usize);
        if entry.min_length == entry.max_length {
            let len = entry.min_length as usize;
            for i in 0..entry.num_values as usize {
                let start = i * len;
                let end = start + len;
                if end > data.len() {
                    return Err(corrupt("binary fixed-length data truncated"));
                }
                offsets.push((start, end));
            }
        } else {
            let addr_meta = entry
                .addresses_meta
                .as_ref()
                .ok_or_else(|| corrupt("binary variable-length field missing addresses meta"))?;
            let addr_data = self.slice(entry.addresses_offset, entry.addresses_length)?;
            let dm = DirectMonotonicReader::new(
                addr_meta,
                addr_data,
                entry.num_values as usize + 1,
                DIRECT_MONOTONIC_BLOCK_SHIFT,
            )?;
            for i in 0..entry.num_values as usize {
                let start = dm.get(i as u64) as usize;
                let end = dm.get(i as u64 + 1) as usize;
                if end > data.len() {
                    return Err(corrupt("binary variable-length data truncated"));
                }
                offsets.push((start, end));
            }
        }
        Ok((doc_ids, data, offsets))
    }

    /// SORTED field zero-copy iterator: ords decoded once, dict held in
    /// packed form; `next` borrows term bytes from the internal dict buffer.
    pub fn sorted_doc_values(&self, field_number: i32) -> io::Result<SortedDocValues> {
        let doc_ords = self.sorted_ords(field_number)?;
        let (dict, dict_offsets) = self.sorted_dict_packed(field_number)?;
        if doc_ords
            .iter()
            .any(|&(_, ord)| ord as usize >= dict_offsets.len())
        {
            return Err(corrupt("sorted ord out of dict range"));
        }
        Ok(SortedDocValues {
            doc_ords,
            dict,
            dict_offsets,
            pos: 0,
        })
    }
}

/// BINARY DocValues 零拷贝迭代器：value 字节直接借自 .dvd mmap
/// （`next` 返回的切片生命周期为 `'a`，与迭代器借用无关）。构造时已完成
/// 全部边界校验（corrupt → InvalidData），迭代过程不会失败。
pub struct BinaryDocValues<'a> {
    doc_ids: DocIds,
    data: &'a [u8],
    addrs: Option<DirectMonotonicReader<'a>>,
    fixed_len: usize, // 仅定长路径使用；变长以 addrs.is_some() 区分
    pos: usize,
}

impl<'a> BinaryDocValues<'a> {
    /// 下一对 (doc_id, bytes)，doc 升序；耗尽返回 None。
    pub fn next(&mut self) -> Option<(u32, &'a [u8])> {
        if self.pos >= self.doc_ids.len() {
            return None;
        }
        let i = self.pos;
        self.pos += 1;
        let doc = self.doc_ids.get(i);
        let (start, end) = match &self.addrs {
            Some(dm) => (dm.get(i as u64) as usize, dm.get(i as u64 + 1) as usize),
            None => (i * self.fixed_len, (i + 1) * self.fixed_len),
        };
        // 构造时已校验 end <= data.len()，这里直接切。
        Some((doc, &self.data[start..end]))
    }

    pub fn len(&self) -> usize {
        self.doc_ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.doc_ids.len() == 0
    }
}

/// SORTED DocValues 零拷贝迭代器：ords 解码为 (doc, ord) 列表，dict 以
/// packed 形式自持；`next` 返回的 term 切片借自内部 dict buffer
/// （生命周期绑定到本次 `next` 调用）。per-doc 零分配。
pub struct SortedDocValues {
    doc_ords: Vec<(u32, u32)>,
    dict: Vec<u8>,
    dict_offsets: Vec<(u32, u32)>,
    pos: usize,
}

impl SortedDocValues {
    /// 下一对 (doc_id, term)，doc 升序；耗尽返回 None。
    pub fn next(&mut self) -> Option<(u32, &[u8])> {
        let &(doc, ord) = self.doc_ords.get(self.pos)?;
        self.pos += 1;
        // 构造时已校验 ord < dict_offsets.len()。
        let (s, l) = self.dict_offsets[ord as usize];
        Some((doc, &self.dict[s as usize..(s + l) as usize]))
    }

    pub fn len(&self) -> usize {
        self.doc_ords.len()
    }

    pub fn is_empty(&self) -> bool {
        self.doc_ords.is_empty()
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

    #[test]
    fn test_binary_dv_write_read() {
        let root = write_index("bin-var", |w| {
            // Variable-length values (triggers DirectMonotonic addresses)
            let values = vec![
                (0u32, b"hello".to_vec()),
                (1u32, b"world!!".to_vec()),
                (2u32, b"".to_vec()),
            ];
            w.add_binary_field(0, 3, &values).unwrap();
        });
        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let bins = r.binary_values(0).unwrap();
        assert_eq!(bins.len(), 3);
        assert_eq!(bins[0], (0, b"hello".to_vec()));
        assert_eq!(bins[1], (1, b"world!!".to_vec()));
        assert_eq!(bins[2], (2, b"".to_vec()));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_binary_dv_fixed_length() {
        let root = write_index("bin-fixed", |w| {
            // Equal-length values (no addresses, minLength == maxLength)
            let values = vec![
                (0u32, vec![0xDE, 0xAD]),
                (1u32, vec![0xBE, 0xEF]),
            ];
            w.add_binary_field(0, 2, &values).unwrap();
        });
        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let bins = r.binary_values(0).unwrap();
        assert_eq!(bins.len(), 2);
        assert_eq!(bins[0], (0, vec![0xDE, 0xAD]));
        assert_eq!(bins[1], (1, vec![0xBE, 0xEF]));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_binary_dv_sparse() {
        let root = write_index("bin-sparse", |w| {
            // Sparse: max_doc=5 but only docs 1,3 have values (triggers DISI)
            let values = vec![
                (1u32, b"aaa".to_vec()),
                (3u32, b"bbbbb".to_vec()),
            ];
            w.add_binary_field(0, 5, &values).unwrap();
        });
        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let bins = r.binary_values(0).unwrap();
        assert_eq!(bins.len(), 2);
        assert_eq!(bins[0], (1, b"aaa".to_vec()));
        assert_eq!(bins[1], (3, b"bbbbb".to_vec()));
        fs::remove_dir_all(&root).unwrap();
    }

    // ---- zero-copy iterator tests ----

    /// 迭代器收集结果与 binary_values 对拍。
    fn assert_binary_iter_matches(r: &DocValuesReader, field: i32) {
        let want = r.binary_values(field).unwrap();
        let mut it = r.binary_doc_values(field).unwrap();
        assert_eq!(it.len(), want.len());
        assert_eq!(it.is_empty(), want.is_empty());
        let mut got: Vec<(u32, Vec<u8>)> = Vec::with_capacity(it.len());
        while let Some((doc, bytes)) = it.next() {
            got.push((doc, bytes.to_vec()));
        }
        assert_eq!(got, want);
        assert!(it.next().is_none(), "exhausted iterator stays exhausted");
    }

    #[test]
    fn binary_doc_values_variable_length() {
        let root = write_index("biter-var", |w| {
            let values = vec![
                (0u32, b"hello".to_vec()),
                (1u32, b"world!!".to_vec()),
                (2u32, b"".to_vec()),
            ];
            w.add_binary_field(0, 3, &values).unwrap();
        });
        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        assert_binary_iter_matches(&r, 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn binary_doc_values_fixed_length() {
        let root = write_index("biter-fixed", |w| {
            let values = vec![
                (0u32, vec![0xDE, 0xAD]),
                (1u32, vec![0xBE, 0xEF]),
            ];
            w.add_binary_field(0, 2, &values).unwrap();
        });
        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        assert_binary_iter_matches(&r, 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn binary_doc_values_sparse() {
        let root = write_index("biter-sparse", |w| {
            // 稀疏 DISI + 变长地址
            let values = vec![
                (1u32, b"aaa".to_vec()),
                (3u32, b"bbbbb".to_vec()),
                (7u32, b"c".to_vec()),
            ];
            w.add_binary_field(0, 10, &values).unwrap();
        });
        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        assert_binary_iter_matches(&r, 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn binary_doc_values_empty_field() {
        let root = write_index("biter-empty", |w| {
            w.add_binary_field(0, 5, &[]).unwrap();
        });
        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let mut it = r.binary_doc_values(0).unwrap();
        assert_eq!(it.len(), 0);
        assert!(it.is_empty());
        assert!(it.next().is_none());
        assert_binary_iter_matches(&r, 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn binary_doc_values_missing_field_not_found() {
        let root = write_index("biter-missing", |w| {
            w.add_binary_field(0, 1, &[(0u32, b"x".to_vec())]).unwrap();
        });
        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let err = r.binary_doc_values(7).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sorted_doc_values_matches_dict_and_ords() {
        // 150 词 → 3 个 64 项块；ords 稀疏（复用 sorted_dict_and_ords 的分布）
        let dict: Vec<String> = (0..150).map(|i| format!("term-{i:04}")).collect();
        let dict_refs: Vec<&[u8]> = dict.iter().map(|s| s.as_bytes()).collect();
        let ords: Vec<(u32, u32)> = (0..300u32)
            .filter(|d| d % 3 != 0)
            .enumerate()
            .map(|(i, d)| (d, (i % 150) as u32))
            .collect();
        let root = write_index("siter", |w| {
            w.add_sorted_field(0, 300, &dict_refs, &ords).unwrap();
        });
        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();

        // 对拍基准：sorted_ords + sorted_dict 组合
        let want_dict = r.sorted_dict(0).unwrap();
        let want_ords = r.sorted_ords(0).unwrap();

        let mut it = r.sorted_doc_values(0).unwrap();
        assert_eq!(it.len(), want_ords.len());
        assert!(!it.is_empty());
        let mut got: Vec<(u32, Vec<u8>)> = Vec::with_capacity(it.len());
        while let Some((doc, term)) = it.next() {
            got.push((doc, term.to_vec()));
        }
        assert!(it.next().is_none());
        let want: Vec<(u32, Vec<u8>)> = want_ords
            .iter()
            .map(|&(d, o)| (d, want_dict[o as usize].clone()))
            .collect();
        assert_eq!(got, want);

        // sorted_dict_packed 与 sorted_dict 一致
        let (buf, offsets) = r.sorted_dict_packed(0).unwrap();
        assert_eq!(offsets.len(), want_dict.len());
        for (i, &(s, l)) in offsets.iter().enumerate() {
            assert_eq!(&buf[s as usize..(s + l) as usize], want_dict[i].as_slice());
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sorted_doc_values_empty_field() {
        let root = write_index("siter-empty", |w| {
            w.add_sorted_field(1, 10, &[], &[]).unwrap();
        });
        let dir = FSDirectory::open(&root).unwrap();
        let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
        let mut it = r.sorted_doc_values(1).unwrap();
        assert_eq!(it.len(), 0);
        assert!(it.is_empty());
        assert!(it.next().is_none());
        fs::remove_dir_all(&root).unwrap();
    }
}
