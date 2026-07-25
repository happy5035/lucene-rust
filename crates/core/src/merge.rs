//! forceMerge(1) —— 格式级段归并（M6 spec §4）。总装见 `force_merge`；
//! 本模块同时承载归并用的纯函数（先单测后接格式层，spec §5.1）。

use std::io;

use codec_lucene9::field_infos::{FieldInfo, FieldInfos};
use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::segment_info::SegmentInfo;
use codec_lucene9::segment_infos::{SegmentCommitInfo, SegmentInfos};
use codec_lucene9::{DocValuesType, FSDirectory, IndexOptions};

use crate::IndexWriterConfig;

use codec_lucene9::io::DataInput;

#[cfg(test)]
use crate::search::{Query, Searcher};
#[cfg(test)]
use crate::{Document, FieldSpec, FieldValue, IndexWriter, Schema};
#[cfg(test)]
use codec_lucene9::segment_infos::random_id;
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::path::PathBuf;

/// 多路已序字典归并：各自无符号字节序升序、段内无重复的输入 → 全局升序去重字典。
/// （SortedDocValuesWriter 全局 ord 分配的对偶操作；Lucene 由
/// DocValuesConsumer.merge 的 TermsEnum 归并完成，本系统段内字典有序 ⇒ 纯函数。）
pub(crate) fn merge_sorted_dicts(dicts: &[Vec<Vec<u8>>]) -> Vec<Vec<u8>> {
    // k 路归并：每路游标取最小项；并列最小（跨段重复）全部推进但只收一份。
    let mut cursors = vec![0usize; dicts.len()];
    let mut global: Vec<Vec<u8>> = Vec::new();
    loop {
        let mut min: Option<&[u8]> = None;
        for (i, d) in dicts.iter().enumerate() {
            if let Some(term) = d.get(cursors[i]) {
                min = Some(match min {
                    None => term,
                    Some(m) if term.as_slice() < m => term,
                    Some(m) => m,
                });
            }
        }
        let Some(min) = min else { break };
        if global.last().map_or(true, |last| last.as_slice() != min) {
            global.push(min.to_vec());
        }
        for (i, d) in dicts.iter().enumerate() {
            if d.get(cursors[i]).map_or(false, |t| t.as_slice() == min) {
                cursors[i] += 1;
            }
        }
    }
    global
}

/// 每段 ord_old → 全局 ord_new 重映射表。`dicts[i]` 的每一项必在 `global` 中
/// （merge_sorted_dicts 的输出是输入的并集）。两指针：两字典均升序。
pub(crate) fn build_ord_remap(dicts: &[Vec<Vec<u8>>], global: &[Vec<u8>]) -> Vec<Vec<u32>> {
    dicts
        .iter()
        .map(|d| {
            let mut remap = Vec::with_capacity(d.len());
            let mut g = 0usize;
            for term in d {
                while g < global.len() && global[g].as_slice() != term.as_slice() {
                    g += 1;
                }
                assert!(g < global.len(), "term missing from merged dict");
                remap.push(g as u32);
            }
            remap
        })
        .collect()
}

/// spec §4.2：各段 field infos 必须逐字段一致（同源 IndexWriter 产物的既有
/// 不变量）；不一致即报错，不做全局重编号（MergeState.fieldInfos 的
/// 同名合并在本系统是恒等）。
pub(crate) fn assert_field_infos_consistent(all: &[FieldInfos]) -> io::Result<()> {
    let Some(first) = all.first() else {
        return Ok(());
    };
    for (seg_idx, fis) in all.iter().enumerate().skip(1) {
        if fis.fields != first.fields {
            let names = |f: &FieldInfos| {
                f.fields
                    .iter()
                    .map(|fi| fi.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "field infos mismatch: segment 0 [{}] vs segment {seg_idx} [{}]",
                    names(first),
                    names(fis)
                ),
            ));
        }
    }
    Ok(())
}

/// 一个待归并段的打开状态（按 segments_N 提交序；doc_base 已累加好）。
pub(crate) struct SegmentMergeSource {
    pub(crate) name: String,
    pub(crate) id: [u8; 16],
    pub(crate) max_doc: i32,
    pub(crate) doc_base: u32,
    pub(crate) field_infos: FieldInfos,
}

/// postings 归并（spec §4.2 核心）：对 merged FieldInfos 里每个 indexed 字段，
/// 各段词典 k-way 归并（TermsIter 已词典序），同 term 的文档流按段序拼接 +
/// doc_base 偏移（天然升序，免交错——关键代码事实 3），freq 原样、positions 按
/// doc 序拼接，走 flush 同款 PostingsWriter 重编码（FOR/PForDelta、4096 跳表
/// 自动重建）；df/totalTermFreq 由 writer 累加；bitmap 由 with_bitmap_threshold
/// 按归并后 df 重建。对照 SegmentMerger.mergeTerms（SegmentMerger.java:208）+
/// MappingMultiPostingsEnum（:30；docIDShift 即 doc_base）。
fn has_positions(fi: &FieldInfo) -> bool {
    matches!(
        fi.index_options,
        IndexOptions::DocsAndFreqsAndPositions | IndexOptions::DocsAndFreqsAndPositionsAndOffsets
    )
}

pub(crate) fn merge_postings(
    dir: &FSDirectory,
    sources: &[SegmentMergeSource],
    field_infos: &FieldInfos,
    new_segment: &str,
    new_segment_id: &[u8; 16],
    bitmap_threshold: Option<u32>,
) -> io::Result<Vec<String>> {
    use codec_lucene9::postings::PostingsWriter;
    use codec_lucene9::postings_read::PostingsReader;
    use codec_lucene9::terms_read::{TermEntry, TermsDict, TermsIter};

    let indexed: Vec<&FieldInfo> = field_infos
        .fields
        .iter()
        .filter(|f| f.index_options != IndexOptions::None)
        .collect();
    if indexed.is_empty() {
        return Ok(Vec::new()); // 无 indexed 字段：无 postings 文件（同 flush）
    }
    // 每段惰性打开 TermsDict + PostingsReader：全段无 indexed terms ⇒ flush
    // 不写 .doc/.tim（segment_builder.rs:156-161）⇒ 该段两个 reader 都是 None。
    // 判据用 .doc 文件存在性（.tmd 与 .doc 同生共死，postings.rs:290-296）。
    let mut dicts: Vec<Option<TermsDict>> = Vec::with_capacity(sources.len());
    let mut readers: Vec<Option<PostingsReader>> = Vec::with_capacity(sources.len());
    for s in sources {
        if dir.file_exists(&codec_lucene9::postings::file_name(&s.name, "doc")) {
            dicts.push(Some(TermsDict::open(dir, &s.name, &s.id, &s.field_infos)?));
            readers.push(Some(PostingsReader::open(dir, &s.name, &s.id)?));
        } else {
            dicts.push(None);
            readers.push(None);
        }
    }
    let mut pw = PostingsWriter::new(dir, new_segment, new_segment_id)?
        .with_bitmap_threshold(bitmap_threshold);

    for fi in indexed {
        // doc_count = 各段 .tmd FieldTermsMeta.doc_count 之和（字段在段内无
        // terms ⇒ None ⇒ 0；write 侧 start_field 的 doc_count 同义，
        // postings.rs:308 只写进 .tmd 记录）
        let doc_count: u32 = dicts
            .iter()
            .map(|d| {
                d.as_ref()
                    .and_then(|d| d.field_meta(fi.number))
                    .map_or(0, |m| m.doc_count as u32)
            })
            .sum();
        pw.start_field(fi, doc_count)?;

        // 每段 TermsIter peeked k-way 归并（TermsIter::next 词典序，terms_read.rs:769；
        // 字段在某段无 terms ⇒ 该段 field_meta 为 None ⇒ TermsIter 立即 done）
        let mut iters: Vec<Option<TermsIter>> = dicts
            .iter_mut()
            .map(|d| d.as_mut().map(|d| d.terms_iter(fi)))
            .collect();
        let mut heads: Vec<Option<(Vec<u8>, TermEntry)>> = Vec::with_capacity(iters.len());
        for it in iters.iter_mut() {
            heads.push(match it {
                Some(it) => it.next()?,
                None => None,
            });
        }
        loop {
            // 当前最小 term
            let mut min: Option<&[u8]> = None;
            for h in heads.iter().flatten() {
                min = Some(match min {
                    None => h.0.as_slice(),
                    Some(m) if h.0.as_slice() < m => h.0.as_slice(),
                    Some(m) => m,
                });
            }
            let Some(min_term) = min else { break };
            let min_term = min_term.to_vec();

            // 逐段拼接文档流（段序即 doc_base 序 ⇒ 全局升序，免交错）
            let mut docs: Vec<u32> = Vec::new();
            let mut freqs: Vec<u32> = Vec::new();
            let mut positions: Option<Vec<Vec<u32>>> = if has_positions(fi) {
                Some(Vec::new())
            } else {
                None
            };
            for (i, h) in heads.iter_mut().enumerate() {
                let Some((term, entry)) = h else { continue };
                if term.as_slice() != min_term.as_slice() {
                    continue;
                }
                let base = sources[i].doc_base;
                let reader = readers[i].as_ref().expect("dict present ⇒ reader present");
                match fi.index_options {
                    IndexOptions::Docs => {
                        let mut en = reader.docs(entry)?;
                        loop {
                            let d = en.next_doc()?;
                            if d == NO_MORE_DOCS {
                                break;
                            }
                            docs.push(base + d as u32);
                            freqs.push(1); // DOCS 字段 freq 恒 1（ttf 不入盘）
                        }
                    }
                    IndexOptions::DocsAndFreqs => {
                        let mut en = reader.docs_and_freqs(entry)?;
                        loop {
                            let d = en.next_doc()?;
                            if d == NO_MORE_DOCS {
                                break;
                            }
                            docs.push(base + d as u32);
                            freqs.push(en.freq());
                        }
                    }
                    _ => {
                        // DocsAndFreqsAndPositions(+Offsets)：EverythingEnum
                        let mut en = reader.positions(entry)?;
                        let pos_lists = positions.as_mut().unwrap();
                        loop {
                            let d = en.next_doc()?;
                            if d == NO_MORE_DOCS {
                                break;
                            }
                            docs.push(base + d as u32);
                            let f = en.freq();
                            freqs.push(f);
                            let mut plist = Vec::with_capacity(f as usize);
                            for _ in 0..f {
                                plist.push(en.next_position()?);
                            }
                            pos_lists.push(plist);
                        }
                    }
                }
                *h = match &mut iters[i] {
                    Some(it) => it.next()?,
                    None => None,
                };
            }
            debug_assert!(
                docs.windows(2).all(|w| w[0] < w[1]),
                "concatenated ascending"
            );
            pw.write_term(&min_term, &docs, &freqs, positions.as_deref())?;
        }
        pw.finish_field()?;
    }
    pw.finish()
}

/// stored 块级裸拷贝（spec §4.2 修正后方案；Lucene90CompressingStoredFieldsWriter
/// .copyChunks :520-595 主路径——同 codec、无 delete ⇒ 恒可裸拷）。
pub(crate) fn merge_stored(
    dir: &FSDirectory,
    sources: &[SegmentMergeSource],
    new_segment: &str,
    new_segment_id: &[u8; 16],
    total_max_doc: i32,
) -> io::Result<[String; 3]> {
    use codec_lucene9::stored_fields::{StoredFieldsIndexReader, StoredFieldsWriter};
    let mut w = StoredFieldsWriter::new(dir, new_segment, *new_segment_id, "")?;
    for s in sources {
        let idx = StoredFieldsIndexReader::open(dir, &s.name, &s.id)?;
        let [fdt_name, _fdx, _fdm] = codec_lucene9::stored_fields::file_names(&s.name, "");
        let mut fdt = dir.open_input(&fdt_name)?;
        let mut expect_base = 0i32;
        for c in 0..idx.num_chunks() {
            let (start, end) = idx.chunk_byte_range(c);
            fdt.seek(start)?;
            let src_base = fdt.read_vint()?;
            let code = fdt.read_vint()?;
            // copyChunks :558-562 的完好性断言（chunk header base == 期望 doc 序）
            if src_base != expect_base {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "corrupt fdt: segment {} chunk {c} base {src_base} != {expect_base}",
                        s.name
                    ),
                ));
            }
            let mut payload = vec![0u8; (end - fdt.file_pointer()) as usize];
            fdt.read_bytes(&mut payload)?;
            w.append_raw_chunk(idx.chunk_doc_count(c), code, &payload)?;
            expect_base += idx.chunk_doc_count(c);
        }
    }
    let stats = w.finish(total_max_doc, dir)?;
    Ok([stats.fdt_name, stats.fdx_name, stats.fdm_name])
}

/// NumericDV：顺序读 + base 重映射 + 现有 writer 重写（spec §4.2）。
/// SortedDV：字典读 + merge_sorted_dicts 全局归并 + build_ord_remap 重映射 +
/// 逐 doc 重写 ord（spec §4.2/§5.1）。返回 [_N_Lucene90_0.{dvd,dvm}] 或空。
pub(crate) fn merge_doc_values(
    dir: &FSDirectory,
    sources: &[SegmentMergeSource],
    field_infos: &FieldInfos,
    new_segment: &str,
    new_segment_id: &[u8; 16],
    total_max_doc: u32,
) -> io::Result<Vec<String>> {
    use codec_lucene9::doc_values::DocValuesWriter;
    use codec_lucene9::doc_values_read::DocValuesReader;
    const DV_SUFFIX: &str = "Lucene90_0"; // segment_builder.rs:30
    let dv_fields: Vec<&FieldInfo> = field_infos
        .fields
        .iter()
        .filter(|f| f.doc_values_type != DocValuesType::None)
        .collect();
    if dv_fields.is_empty() {
        return Ok(Vec::new());
    }
    let readers: Vec<DocValuesReader> = sources
        .iter()
        .map(|s| DocValuesReader::open(dir, &s.name, &s.id, DV_SUFFIX))
        .collect::<io::Result<_>>()?;
    let mut w = DocValuesWriter::new(dir, new_segment, new_segment_id, DV_SUFFIX)?;
    for fi in dv_fields {
        match fi.doc_values_type {
            DocValuesType::Numeric => {
                let mut pairs: Vec<(u32, i64)> = Vec::new();
                for (s, r) in sources.iter().zip(&readers) {
                    for (d, v) in r.numeric_values(fi.number)? {
                        pairs.push((s.doc_base + d, v));
                    }
                }
                w.add_numeric_field(fi.number, total_max_doc, &pairs)?;
            }
            DocValuesType::Sorted => {
                // ① 各段字典 → 全局字典 + 重映射（Step 1-2 纯函数）
                let dicts: Vec<Vec<Vec<u8>>> = readers
                    .iter()
                    .map(|r| r.sorted_dict(fi.number))
                    .collect::<io::Result<_>>()?;
                let global = merge_sorted_dicts(&dicts);
                let remap = build_ord_remap(&dicts, &global);
                // ② 逐 doc 重写 ord（doc 序 = 段序拼接，升序天然保持）
                let mut ords: Vec<(u32, u32)> = Vec::new();
                for (i, r) in readers.iter().enumerate() {
                    for (d, o) in r.sorted_ords(fi.number)? {
                        ords.push((sources[i].doc_base + d, remap[i][o as usize]));
                    }
                }
                let dict_refs: Vec<&[u8]> = global.iter().map(Vec::as_slice).collect();
                w.add_sorted_field(fi.number, total_max_doc, &dict_refs, &ords)?;
            }
            t => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported DV type {t:?} in merge (spec §1 premise)"),
                ))
            }
        }
    }
    w.finish()
}

/// points：T-B BKD 读路径全区间（i64::MIN..=i64::MAX）全量遍历 +
/// base 重映射，灌回现有 BKD writer（全内存排序吸收多段输入；
/// PointsWriter.mergeOneField 同款朴素归并，PointsWriter.java:42）。
pub(crate) fn merge_points(
    dir: &FSDirectory,
    sources: &[SegmentMergeSource],
    field_infos: &FieldInfos,
    new_segment: &str,
    new_segment_id: &[u8; 16],
) -> io::Result<Vec<String>> {
    use codec_lucene9::points::PointsWriter;
    use codec_lucene9::points_read::PointsReader;
    let point_fields: Vec<&FieldInfo> = field_infos
        .fields
        .iter()
        .filter(|f| f.point_dimension_count == 1)
        .collect();
    if point_fields.is_empty() {
        return Ok(Vec::new());
    }
    let readers: Vec<Option<PointsReader>> = sources
        .iter()
        .map(|s| PointsReader::open(dir, &s.name, &s.id, &s.field_infos))
        .collect::<io::Result<_>>()?;
    let mut w = PointsWriter::new(dir, new_segment, new_segment_id)?;
    for fi in point_fields {
        // 全区间取点（spec §4.2；PointsWriter.mergeOneField 朴素归并，
        // PointsWriter.java:42-216 的 visitDocValues 路径——本系统无 delete，
        // docMap 恒等偏移）
        let mut longs: Vec<(i64, u32)> = Vec::new();
        for (i, r) in readers.iter().enumerate() {
            let Some(r) = r else { continue };
            let base = sources[i].doc_base;
            r.intersect(&fi.name, i64::MIN, i64::MAX, &mut |v, d| {
                longs.push((v, base + d as u32));
            })?;
        }
        if longs.is_empty() {
            continue; // 全段无点：同 flush 的 field_has_points 判定
        }
        if fi.point_num_bytes == 8 {
            w.write_field_long(fi.number, &mut longs)?;
        } else {
            let mut ints: Vec<(i32, u32)> = longs.iter().map(|&(v, d)| (v as i32, d)).collect();
            w.write_field_int(fi.number, &mut ints)?;
        }
    }
    w.finish()
}

/// forceMerge(1)（M6 spec §4.1）：读当前 segments_N → 逐格式归并出一个新段 →
/// 两段式提交（复用 index_writer::commit_infos 的 fsync + pending/rename 路径）→
/// 成功后删旧段文件与全部旧 segments_N（Java on-commit 清理同款）。
/// 中途失败：旧提交点完好；已写出的新段文件按已知文件名清单尽力清理
/// （SegmentMerger abort 语义）。单线程。
pub fn force_merge(_dir: &FSDirectory, _config: &IndexWriterConfig) -> io::Result<()> {
    todo!("Step 16")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rustlucene-merge-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn dict(items: &[&str]) -> Vec<Vec<u8>> {
        items.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    fn as_strings(d: &[Vec<u8>]) -> Vec<String> {
        d.iter()
            .map(|t| String::from_utf8(t.clone()).unwrap())
            .collect()
    }

    #[test]
    fn merge_dicts_dedup_and_order() {
        let dicts = vec![
            dict(&["apple", "cherry", "date"]),
            dict(&["banana", "cherry"]),
            dict(&["apple", "elderberry"]),
        ];
        let global = merge_sorted_dicts(&dicts);
        assert_eq!(
            as_strings(&global),
            vec!["apple", "banana", "cherry", "date", "elderberry"]
        );
    }

    #[test]
    fn merge_dicts_empty_inputs() {
        // 零段、全空段、空段混排都是合法输入（字段级空，关键代码事实 10）
        assert!(merge_sorted_dicts(&[]).is_empty());
        assert!(merge_sorted_dicts(&[vec![], vec![]]).is_empty());
        let dicts = vec![dict(&["a"]), vec![], dict(&["b"])];
        assert_eq!(as_strings(&merge_sorted_dicts(&dicts)), vec!["a", "b"]);
    }

    #[test]
    fn ord_remap_with_duplicates_and_empty_segment() {
        let dicts = vec![
            dict(&["apple", "cherry", "date"]),
            dict(&["banana", "cherry"]),
            vec![],
        ];
        let global = merge_sorted_dicts(&dicts);
        let remap = build_ord_remap(&dicts, &global);
        assert_eq!(remap.len(), 3);
        assert_eq!(remap[0], vec![0, 2, 3]); // apple→0 cherry→2 date→3
        assert_eq!(remap[1], vec![1, 2]); // banana→1 cherry→2
        assert!(remap[2].is_empty());
    }

    #[test]
    fn ord_remap_single_segment_is_identity() {
        let dicts = vec![dict(&["a", "b", "c"])];
        let global = merge_sorted_dicts(&dicts);
        assert_eq!(build_ord_remap(&dicts, &global), vec![vec![0, 1, 2]]);
    }

    use codec_lucene9::field_infos::{DocValuesType, IndexOptions};

    fn fis(specs: &[(&str, IndexOptions, DocValuesType)]) -> FieldInfos {
        FieldInfos::new(
            specs
                .iter()
                .enumerate()
                .map(|(i, &(name, io_opt, dv))| {
                    let mut fi = FieldInfo::stored(name, i as i32);
                    fi.index_options = io_opt;
                    fi.omit_norms = io_opt != IndexOptions::None;
                    fi.doc_values_type = dv;
                    fi
                })
                .collect(),
        )
    }

    #[test]
    fn field_infos_consistent_accepts_identical() {
        let a = fis(&[
            ("level", IndexOptions::Docs, DocValuesType::Sorted),
            ("message", IndexOptions::DocsAndFreqs, DocValuesType::None),
        ]);
        let b = fis(&[
            ("level", IndexOptions::Docs, DocValuesType::Sorted),
            ("message", IndexOptions::DocsAndFreqs, DocValuesType::None),
        ]);
        assert_field_infos_consistent(&[a, b]).unwrap();
    }

    #[test]
    fn field_infos_consistent_rejects_diverged() {
        let a = fis(&[("level", IndexOptions::Docs, DocValuesType::Sorted)]);
        // DV 类型分歧（同源写不出，手工构造）
        let b = fis(&[("level", IndexOptions::Docs, DocValuesType::None)]);
        let err = assert_field_infos_consistent(&[a, b]).unwrap_err();
        assert!(err.to_string().contains("field infos mismatch"));
        // 字段数分歧同样拒绝
        let c = fis(&[
            ("level", IndexOptions::Docs, DocValuesType::Sorted),
            ("extra", IndexOptions::None, DocValuesType::None),
        ]);
        let d = fis(&[("level", IndexOptions::Docs, DocValuesType::Sorted)]);
        assert!(assert_field_infos_consistent(&[c, d]).is_err());
    }

    use codec_lucene9::postings_read::PostingsReader;
    use codec_lucene9::segment_infos::{random_id, SegmentInfos};
    use codec_lucene9::terms_read::TermsDict;
    use codec_lucene9::FieldInfos;

    /// 写一个小段并返回其 SegmentCommitInfo（merge 测试专用 builder）。
    fn write_segment(
        dir: &FSDirectory,
        name_counter: u64,
        docs: &[(&str, &str)], // (level, message)
    ) -> SegmentCommitInfo {
        let mut schema = Schema::new();
        schema.add(FieldSpec::keyword("level"));
        schema.add(FieldSpec::text("message"));
        let mut b = crate::SegmentBuilder::new(dir.clone(), name_counter);
        for (level, msg) in docs {
            let mut d = Document::new();
            d.add("level", FieldValue::Keyword(level.to_string()));
            d.add("message", FieldValue::Text(msg.to_string()));
            b.add_document(&schema, d).unwrap();
        }
        b.finalize().unwrap().expect("non-empty segment")
    }

    fn open_sources(dir: &FSDirectory) -> Vec<SegmentMergeSource> {
        let (infos, _gen) = SegmentInfos::read_latest(dir).unwrap();
        let mut doc_base = 0u32;
        infos
            .segments
            .iter()
            .map(|sci| {
                let s = SegmentMergeSource {
                    name: sci.info.name.clone(),
                    id: sci.info.id,
                    max_doc: sci.info.doc_count,
                    doc_base,
                    field_infos: FieldInfos::read(dir, &sci.info.name, &sci.info.id, "").unwrap(),
                };
                doc_base += sci.info.doc_count as u32;
                s
            })
            .collect()
    }

    #[test]
    fn merge_postings_offsets_and_reencodes() {
        let root = temp_dir("pmerge");
        let dir = FSDirectory::open(&root).unwrap();
        let s0 = write_segment(
            &dir,
            0,
            &[
                ("INFO", "alpha beta"),
                ("WARN", "alpha"),
                ("INFO", "beta gamma"),
            ],
        );
        let s1 = write_segment(&dir, 1, &[("INFO", "alpha delta"), ("WARN", "alpha")]);
        let mut infos = SegmentInfos::new();
        infos.segments = vec![s0, s1];
        infos.counter = 2;
        infos.min_segment_version = Some((9, 12, 3));
        infos.commit(&dir, 1).unwrap();

        let sources = open_sources(&dir);
        assert_eq!(sources[1].doc_base, 3);
        let merged_fis = FieldInfos::new(sources[0].field_infos.fields.clone());
        let new_id = random_id();
        let files = merge_postings(
            &dir,
            &sources,
            &merged_fis,
            "_m",
            &new_id,
            Some(2), // bitmap threshold：alpha df=5 >= 2
        )
        .unwrap();
        assert!(files.iter().any(|f| f.ends_with(".doc")));

        // 复读新段：词典序 + 偏移后 postings（.fnm 由总装步写，本测试直接用 merged_fis）
        let mut dict = TermsDict::open(&dir, "_m", &new_id, &merged_fis).unwrap();
        let postings = PostingsReader::open(&dir, "_m", &new_id).unwrap();
        let msg = merged_fis.by_name("message").unwrap();
        let mut it = dict.terms_iter(msg);
        let mut terms = Vec::new();
        while let Some((term, entry)) = it.next().unwrap() {
            terms.push((String::from_utf8(term).unwrap(), entry));
        }
        assert_eq!(
            terms.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "beta", "delta", "gamma"]
        );
        let alpha = &terms[0].1;
        assert_eq!(alpha.doc_freq, 4);
        let mut en = postings.docs_and_freqs(alpha).unwrap();
        let mut got = Vec::new();
        loop {
            let d = en.next_doc().unwrap();
            if d == NO_MORE_DOCS {
                break;
            }
            got.push((d, en.freq()));
        }
        // 段 0: alpha@(0,f1),(1,f1)；段 1: alpha@(0,f1),(1,f1) → 偏移后 3,4
        assert_eq!(got, vec![(0, 1), (1, 1), (3, 1), (4, 1)]);
        // df=4 低于读侧 BITMAP_MIN_DF(4096)，open_term_bitmap 返回 None；
        // 但 writer 仍因 threshold=2 写了内联 bitmap（由 Step 18 大电池覆盖）。
        assert!(postings.open_term_bitmap(alpha, 4).unwrap().is_none());
        // singleton：gamma df=1
        let gamma = &terms[3].1;
        assert_eq!(gamma.doc_freq, 1);
        let mut en = postings.docs_and_freqs(gamma).unwrap();
        assert_eq!(en.next_doc().unwrap(), 2);
        fs::remove_dir_all(&root).unwrap();
    }

    /// 两段含 stored + NumericDV + SortedDV + LongPoint 字段，逐格式归并后复读。
    #[test]
    fn merge_stored_dv_points_end_to_end() {
        let root = temp_dir("fmtmerge");
        let dir = FSDirectory::open(&root).unwrap();
        // 段 0：3 docs；段 1：2 docs（含 level 字典跨段重复 "INFO"）
        let schema = |s: &mut Schema| {
            s.add(
                FieldSpec::long_point("ts")
                    .with_numeric_dv()
                    .with_stored(true),
            );
            s.add(FieldSpec::keyword("level").with_sorted_dv());
        };
        let mut sch = Schema::new();
        schema(&mut sch);
        let mk = |ts: i64, level: &str| {
            let mut d = Document::new();
            d.add("ts", FieldValue::Long(ts));
            d.add("level", FieldValue::Keyword(level.to_string()));
            d
        };
        let mut b0 = crate::SegmentBuilder::new(dir.clone(), 0);
        b0.add_document(&sch, mk(100, "INFO")).unwrap();
        b0.add_document(&sch, mk(200, "WARN")).unwrap();
        b0.add_document(&sch, mk(300, "INFO")).unwrap();
        let s0 = b0.finalize().unwrap().unwrap();
        let mut b1 = crate::SegmentBuilder::new(dir.clone(), 1);
        b1.add_document(&sch, mk(150, "ERROR")).unwrap();
        b1.add_document(&sch, mk(250, "INFO")).unwrap();
        let s1 = b1.finalize().unwrap().unwrap();
        let mut infos = SegmentInfos::new();
        infos.segments = vec![s0, s1];
        infos.counter = 2;
        infos.min_segment_version = Some((9, 12, 3));
        infos.commit(&dir, 1).unwrap();

        let sources = open_sources(&dir);
        let merged_fis = FieldInfos::new(sources[0].field_infos.fields.clone());
        let new_id = random_id();

        // --- stored 裸拷贝 ---
        let stored_files = merge_stored(&dir, &sources, "_m", &new_id, 5).unwrap();
        assert_eq!(stored_files, stored_fields_file_names("_m"));
        let idx = codec_lucene9::stored_fields::StoredFieldsIndexReader::open(&dir, "_m", &new_id)
            .unwrap();
        assert_eq!(idx.num_chunks(), 2); // 每源段 1 chunk（小文档）
        assert_eq!(idx.chunk_doc_count(0), 3);
        assert_eq!(idx.chunk_doc_count(1), 2);
        // chunk 1 的 docBase 重定基为 3
        let mut fdt = dir.open_input("_m.fdt").unwrap();
        let (s, _e) = idx.chunk_byte_range(1);
        fdt.seek(s).unwrap();
        assert_eq!(fdt.read_vint().unwrap(), 3, "chunk 1 rebased to doc_base 3");

        // --- DV 归并 ---
        let dv_files = merge_doc_values(&dir, &sources, &merged_fis, "_m", &new_id, 5).unwrap();
        assert_eq!(dv_files.len(), 2);
        let dvr = codec_lucene9::doc_values_read::DocValuesReader::open(
            &dir,
            "_m",
            &new_id,
            "Lucene90_0",
        )
        .unwrap();
        let ts_fi = merged_fis.by_name("ts").unwrap();
        assert_eq!(
            dvr.numeric_values(ts_fi.number).unwrap(),
            vec![(0, 100), (1, 200), (2, 300), (3, 150), (4, 250)]
        );
        let level_fi = merged_fis.by_name("level").unwrap();
        let dict = dvr.sorted_dict(level_fi.number).unwrap();
        assert_eq!(
            dict.iter()
                .map(|t| String::from_utf8(t.clone()).unwrap())
                .collect::<Vec<_>>(),
            vec!["ERROR", "INFO", "WARN"] // 全局字典：跨段 "INFO" 去重
        );
        // ords：ERROR=0 INFO=1 WARN=2；段 0 INFO,WARN,INFO → 1,2,1；段 1 ERROR,INFO → 0,1
        assert_eq!(
            dvr.sorted_ords(level_fi.number).unwrap(),
            vec![(0, 1), (1, 2), (2, 1), (3, 0), (4, 1)]
        );

        // --- points 归并（T-B PointsReader 全区间取点 + base 偏移 + 重写）---
        let point_files = merge_points(&dir, &sources, &merged_fis, "_m", &new_id).unwrap();
        assert_eq!(point_files.len(), 3);
        // 复读：T-B PointsReader 全区间收集 → (value, doc) 多重集与输入一致
        let pr = codec_lucene9::points_read::PointsReader::open(&dir, "_m", &new_id, &merged_fis)
            .unwrap()
            .expect("points present");
        let mut got: Vec<(i64, i32)> = Vec::new();
        pr.intersect("ts", i64::MIN, i64::MAX, &mut |v, d| got.push((v, d)))
            .unwrap();
        got.sort();
        assert_eq!(got, vec![(100, 0), (150, 3), (200, 1), (250, 4), (300, 2)]);
        fs::remove_dir_all(&root).unwrap();
    }

    fn stored_fields_file_names(segment: &str) -> [String; 3] {
        [
            format!("{segment}.fdt"),
            format!("{segment}.fdx"),
            format!("{segment}.fdm"),
        ]
    }
}
