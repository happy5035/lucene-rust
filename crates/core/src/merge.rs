//! forceMerge(1) —— 格式级段归并（M6 spec §4）。总装见 `force_merge`；
//! 本模块同时承载归并用的纯函数（先单测后接格式层，spec §5.1）。

use std::io;

use codec_lucene9::field_infos::{FieldInfo, FieldInfos};
use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::segment_info::SegmentInfo;
use codec_lucene9::{DocValuesType, FSDirectory, IndexOptions};

use crate::IndexWriterConfig;

use codec_lucene9::io::DataInput;

#[cfg(test)]
use crate::search::{Query, Searcher};
#[cfg(test)]
use crate::{Document, FieldSpec, FieldValue, IndexWriter, Schema};
#[cfg(test)]
use std::fs;

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

/// 删除指定 segment 名下所有可能产生的文件（`_N.*` 与 `_N_*`）。
/// 用于 force_merge 失败/提交失败时的尽力孤儿清理，覆盖子 writer 写到
/// 一半就失败的 partial 文件（这些文件不会进入 `written` 清单）。
fn cleanup_segment_files(dir: &FSDirectory, segment_name: &str) -> io::Result<()> {
    let prefix_dot = format!("{segment_name}.");
    let prefix_underscore = format!("{segment_name}_");
    let names = dir.list_all()?;
    for name in names {
        if name.starts_with(&prefix_dot) || name.starts_with(&prefix_underscore) {
            dir.delete(&name)?;
        }
    }
    Ok(())
}

/// forceMerge(1)（M6 spec §4.1）：读当前 segments_N → 逐格式归并出一个新段 →
/// 两段式提交（复用 index_writer::commit_infos 的 fsync + pending/rename 路径）→
/// 成功后删旧段文件与全部旧 segments_N（Java on-commit 清理同款）。
/// 中途失败：旧提交点完好；已写出的新段文件按 known-filename 清单 + 新段名
/// prefix 扫描尽力清理（SegmentMerger abort 语义，包含 partial 文件）。单线程。
pub fn force_merge(dir: &FSDirectory, config: &IndexWriterConfig) -> io::Result<()> {
    use codec_lucene9::segment_infos::{random_id, SegmentCommitInfo, SegmentInfos, SEGMENTS};

    let (old_infos, old_gen) = SegmentInfos::read_latest(dir)?;
    if old_infos.segments.is_empty() {
        return Ok(()); // 空索引 no-op（关键代码事实 10）
    }

    // 新段名在读取/校验输入前确定，这样即使 field infos 不一致等早期失败，
    // 也能按前缀清理该段名对应的 partial 孤儿文件。
    let new_name = format!(
        "_{}",
        crate::segment_builder::to_base36(old_infos.counter as u64)
    );

    // 失败清理清单：逐格式产出即记录（spec §4.1 abort 语义）。written 留在
    // 外层作用域（闭包只 &mut 借用），失败分支与提交失败分支都要消费它。
    let mut written: Vec<String> = Vec::new();
    let result = (|| -> io::Result<SegmentCommitInfo> {
        // 归并输入（按提交序累加 doc_base）
        let mut doc_base = 0u32;
        let mut sources: Vec<SegmentMergeSource> = Vec::with_capacity(old_infos.segments.len());
        let mut all_fis: Vec<FieldInfos> = Vec::with_capacity(old_infos.segments.len());
        for sci in &old_infos.segments {
            let fis = FieldInfos::read(dir, &sci.info.name, &sci.info.id, "")?;
            sources.push(SegmentMergeSource {
                name: sci.info.name.clone(),
                id: sci.info.id,
                doc_base,
                field_infos: FieldInfos::new(fis.fields.clone()),
            });
            all_fis.push(fis);
            doc_base += sci.info.doc_count as u32;
        }
        assert_field_infos_consistent(&all_fis)?;
        let merged_fis = FieldInfos::new(all_fis[0].fields.clone());
        let total_max_doc = doc_base;
        let new_id = random_id();

        // .fnm 先行（全部文件同一 new_id）
        let fnm = merged_fis.write(dir, &new_name, &new_id, "")?;
        written.push(fnm.clone());
        // stored → postings → DV → points（关键代码事实 9）
        let stored = merge_stored(dir, &sources, &new_name, &new_id, total_max_doc as i32)?;
        written.extend(stored.iter().cloned());
        let bitmap_threshold = config.bitmap.then_some(
            config
                .bitmap_threshold
                .max(codec_lucene9::roaring::BITMAP_MIN_DF),
        );
        let postings = merge_postings(
            dir,
            &sources,
            &merged_fis,
            &new_name,
            &new_id,
            bitmap_threshold,
        )?;
        written.extend(postings.iter().cloned());
        let dv = merge_doc_values(
            dir,
            &sources,
            &merged_fis,
            &new_name,
            &new_id,
            total_max_doc,
        )?;
        written.extend(dv.iter().cloned());
        let points = merge_points(dir, &sources, &merged_fis, &new_name, &new_id)?;
        written.extend(points.iter().cloned());
        // .si（diagnostics 只写稳定键，关键代码事实 7）
        let mut si = SegmentInfo::new(&new_name, new_id, total_max_doc as i32);
        si.diagnostics.insert("source".into(), "merge".into());
        si.diagnostics
            .insert("lucene.version".into(), "9.12.3".into());
        si.diagnostics
            .insert("mergeFactor".into(), sources.len().to_string());
        si.attributes.insert(
            "Lucene90StoredFieldsFormat.mode".into(),
            "BEST_SPEED".into(),
        );
        si.files.insert(fnm);
        si.files.extend(stored);
        si.files.extend(postings);
        si.files.extend(dv);
        si.files.extend(points);
        si.files.insert(format!("{new_name}.si"));
        si.write(dir, "")?;
        written.push(format!("{new_name}.si"));
        Ok(SegmentCommitInfo::new(si, random_id()))
    })();
    let new_sci = match result {
        Ok(sci) => sci,
        Err(e) => {
            for f in &written {
                let _ = dir.delete(f); // 尽力而为（Java abort 同款）
            }
            let _ = cleanup_segment_files(dir, &new_name); // 尽力；保留原错误
            return Err(e);
        }
    };

    // 两段式提交（复用现有路径：fsync 段文件 → pending → rename → dir fsync）。
    // 提交失败同样清理新段文件——失败语义与归并中途一致（spec §4.1）。
    let mut new_infos = SegmentInfos::new();
    new_infos.version = old_infos.version;
    new_infos.counter = old_infos.counter + 1;
    new_infos.index_created_version_major = old_infos.index_created_version_major;
    new_infos.min_segment_version = old_infos.min_segment_version;
    new_infos.user_data = old_infos.user_data.clone();
    new_infos.segments.push(new_sci);
    if let Err(e) = crate::index_writer::commit_infos(dir, &mut new_infos, old_gen + 1) {
        for f in &written {
            let _ = dir.delete(f);
        }
        let _ = cleanup_segment_files(dir, &new_name); // 尽力；保留原错误
        return Err(e);
    }

    // 提交成功 ⇒ 删旧段文件 + 全部旧代 segments_N（gen ≤ old_gen）
    let mut stale: Vec<String> = Vec::new();
    for sci in &old_infos.segments {
        stale.extend(sci.info.files.iter().cloned());
    }
    for name in dir.list_all()? {
        if !name.starts_with(SEGMENTS) || name == "segments.gen" {
            continue;
        }
        let Some(gen_str) = name[SEGMENTS.len()..].strip_prefix('_') else {
            continue;
        };
        let Ok(g) = i64::from_str_radix(gen_str, 36) else {
            continue;
        };
        if g <= old_gen {
            stale.push(name);
        }
    }
    stale.sort();
    stale.dedup();
    for f in &stale {
        dir.delete(f)?;
    }
    Ok(())
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
    use codec_lucene9::segment_infos::{random_id, SegmentCommitInfo, SegmentInfos};
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
        // df=4 低于读侧 BITMAP_MIN_DF(4096)，所以对该 merged term 调用
        // open_term_bitmap 会返回 None；这不代表 writer 没写 bitmap 文件——
        // threshold=2 仍会让 writer 输出内联 bitmap，命中逻辑由 Step 18 覆盖。
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

    /// 全段归并 → Searcher 侧：segment_count()==1、maxDoc 不变、查询结果与
    /// 归并前逐条一致、旧段文件与旧 segments_N 已删。
    #[test]
    fn force_merge_end_to_end() {
        let root = temp_dir("fm");
        let dir = FSDirectory::open(&root).unwrap();
        let mut schema = Schema::new();
        schema.add(FieldSpec::keyword("level"));
        schema.add(FieldSpec::text_with_positions("message"));
        schema.add(
            FieldSpec::long_point("ts")
                .with_numeric_dv()
                .with_stored(true),
        );
        schema.add(FieldSpec::sorted_dv("host"));
        // 两个 commit → 两段（段 0 docs 0..3，段 1 docs 0..2）
        let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();
        let put = |w: &mut IndexWriter, i: u64, level: &str, msg: &str, host: &str| {
            let mut d = Document::new();
            d.add("level", FieldValue::Keyword(level.to_string()));
            d.add("message", FieldValue::Text(msg.to_string()));
            d.add("ts", FieldValue::Long(1000 + i as i64));
            d.add("host", FieldValue::Keyword(host.to_string()));
            w.add_document(d).unwrap();
        };
        put(&mut w, 0, "INFO", "alpha beta", "h1");
        put(&mut w, 1, "WARN", "alpha", "h2");
        put(&mut w, 2, "INFO", "beta gamma", "h1");
        w.commit().unwrap();
        put(&mut w, 3, "ERROR", "alpha delta", "h3");
        put(&mut w, 4, "INFO", "alpha beta", "h2");
        w.commit().unwrap();
        drop(w);

        // 归并前基线
        let pre = {
            let mut s = Searcher::open(&dir).unwrap();
            assert_eq!(s.segment_count(), 2);
            let mut lines = Vec::new();
            lines.push(format!("maxDoc={}", s.max_doc()));
            for (field, term) in [
                ("level", "INFO"),
                ("level", "WARN"),
                ("level", "ERROR"),
                ("message", "alpha"),
                ("message", "beta"),
                ("message", "delta"),
            ] {
                let c = s.count(&Query::term(field, term)).unwrap();
                lines.push(format!("term {field}={term} count={c}"));
            }
            let c = s
                .count(&Query::phrase("message", &["alpha", "beta"]))
                .unwrap();
            lines.push(format!("phrase alpha,beta count={c}"));
            let (_, docs) = s.top_docs(&Query::MatchAll, 20).unwrap();
            lines.push(format!("matchall first20={docs:?}"));
            lines
        };
        let files_before: std::collections::BTreeSet<String> =
            dir.list_all().unwrap().into_iter().collect();

        force_merge(&dir, &IndexWriterConfig::default()).unwrap();

        // 归并后：单段、查询逐条一致
        let mut s = Searcher::open(&dir).unwrap();
        assert_eq!(s.segment_count(), 1);
        let mut post = Vec::new();
        post.push(format!("maxDoc={}", s.max_doc()));
        for (field, term) in [
            ("level", "INFO"),
            ("level", "WARN"),
            ("level", "ERROR"),
            ("message", "alpha"),
            ("message", "beta"),
            ("message", "delta"),
        ] {
            let c = s.count(&Query::term(field, term)).unwrap();
            post.push(format!("term {field}={term} count={c}"));
        }
        let c = s
            .count(&Query::phrase("message", &["alpha", "beta"]))
            .unwrap();
        post.push(format!("phrase alpha,beta count={c}"));
        let (_, docs) = s.top_docs(&Query::MatchAll, 20).unwrap();
        post.push(format!("matchall first20={docs:?}"));
        assert_eq!(pre, post, "pre/post-merge query diff must be empty");

        // 旧文件清理：旧段文件（_0/_1 前缀）与旧 segments_1/segments_2 全删，
        // 只剩新段（_2 前缀，第三段名）+ segments_3
        let files_after: Vec<String> = dir.list_all().unwrap();
        for f in &files_after {
            assert!(!f.starts_with("_0") && !f.starts_with("_1"), "stale {f}");
        }
        assert!(files_after.iter().any(|f| f == "segments_3"));
        assert!(!files_before.is_empty());
        assert_eq!(
            files_after
                .iter()
                .filter(|f| f.starts_with("segments_"))
                .count(),
            1,
            "exactly one commit file: {files_after:?}"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// 单段退化（spec §4.4）：归并 = 重打包，结果与归并前查询一致。
    #[test]
    fn force_merge_single_segment_degenerates() {
        let root = temp_dir("fm1");
        let dir = FSDirectory::open(&root).unwrap();
        let mut schema = Schema::new();
        schema.add(FieldSpec::keyword("level"));
        let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();
        for i in 0..5 {
            let mut d = Document::new();
            d.add("level", FieldValue::Keyword(format!("L{}", i % 2)));
            w.add_document(d).unwrap();
        }
        w.commit().unwrap();
        drop(w);
        force_merge(&dir, &IndexWriterConfig::default()).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        assert_eq!(s.segment_count(), 1);
        assert_eq!(s.max_doc(), 5);
        assert_eq!(s.count(&Query::term("level", "L0")).unwrap(), 3);
        let files: Vec<String> = dir.list_all().unwrap();
        assert!(files.iter().all(|f| !f.starts_with("_0.")));
        assert!(files.iter().any(|f| f == "segments_2"));
        fs::remove_dir_all(&root).unwrap();
    }

    /// 中途失败清理（spec §4.1）：field infos 不一致 ⇒ Err；目录里不留
    /// 新段孤儿文件；旧提交点完好（仍 2 段可查）。
    #[test]
    fn force_merge_failure_cleans_orphans() {
        let root = temp_dir("fmfail");
        let dir = FSDirectory::open(&root).unwrap();
        // 段 0：level；段 1：level + extra（schema 动态增长 ⇒ .fnm 分歧）
        let mut w =
            IndexWriter::create(&root, Schema::new(), IndexWriterConfig::default()).unwrap();
        w.schema_mut().add(FieldSpec::keyword("level"));
        let mut d = Document::new();
        d.add("level", FieldValue::Keyword("INFO".into()));
        w.add_document(d).unwrap();
        w.commit().unwrap();
        w.schema_mut().add(FieldSpec::keyword("extra"));
        let mut d = Document::new();
        d.add("level", FieldValue::Keyword("WARN".into()));
        d.add("extra", FieldValue::Keyword("x".into()));
        w.add_document(d).unwrap();
        w.commit().unwrap();
        drop(w);

        // 预置一些以新段名 _2 开头的 partial/完整孤儿文件，模拟 writer 写到
        // 一半崩溃后残留；force_merge 失败时必须把它们全部清掉。
        fs::write(root.join("_2.partial"), b"partial").unwrap();
        fs::write(root.join("_2.fdt"), b"stored").unwrap();
        fs::write(root.join("_2_Lucene90_0.dvd"), b"dv").unwrap();

        let err = force_merge(&dir, &IndexWriterConfig::default()).unwrap_err();
        assert!(err.to_string().contains("field infos mismatch"), "{err}");
        // 无 _2 前缀孤儿；旧 segments_2 仍是当前提交点
        let files: Vec<String> = dir.list_all().unwrap();
        assert!(
            files.iter().all(|f| !f.starts_with("_2")),
            "orphans: {files:?}"
        );
        let s = Searcher::open(&dir).unwrap();
        assert_eq!(s.segment_count(), 2);
        assert_eq!(s.max_doc(), 2);
        fs::remove_dir_all(&root).unwrap();
    }

    /// cleanup_segment_files 直接删除目标 segment 的所有前缀文件，但不碰其他 segment
    /// 与提交点文件。
    #[test]
    fn cleanup_segment_files_deletes_by_prefix() {
        let root = temp_dir("cleanprefix");
        let dir = FSDirectory::open(&root).unwrap();
        fs::write(root.join("_0.fdt"), b"a").unwrap();
        fs::write(root.join("_0.fdx"), b"b").unwrap();
        fs::write(root.join("_0_Lucene90_0.dvd"), b"c").unwrap();
        fs::write(root.join("_1.fdt"), b"d").unwrap();
        fs::write(root.join("segments_1"), b"e").unwrap();
        cleanup_segment_files(&dir, "_0").unwrap();
        let files: std::collections::BTreeSet<String> =
            dir.list_all().unwrap().into_iter().collect();
        assert!(!files.contains("_0.fdt"));
        assert!(!files.contains("_0.fdx"));
        assert!(!files.contains("_0_Lucene90_0.dvd"));
        assert!(files.contains("_1.fdt"));
        assert!(files.contains("segments_1"));
        fs::remove_dir_all(&root).unwrap();
    }

    /// spec §4.4：空索引 no-op（0-doc 段本系统不存在，关键代码事实 10）。
    #[test]
    fn force_merge_empty_index_noop() {
        let root = temp_dir("fmempty");
        let dir = FSDirectory::open(&root).unwrap();
        // 手工提交一个 0 段 commit（本系统唯一构造空索引的方式；
        // Searcher::open 对 0 段 commit 已有先例：search/mod.rs:222）
        let infos = SegmentInfos::new();
        infos.commit(&dir, 1).unwrap();
        force_merge(&dir, &IndexWriterConfig::default()).unwrap();
        assert_eq!(Searcher::open(&dir).unwrap().segment_count(), 0);
        fs::remove_dir_all(&root).unwrap();
    }

    /// spec §4.4：字段级空——全稀疏 DV（段 1 latency/host 全缺）+ 无 points
    /// 字段（schema 不含 points ⇒ merge_points 空返回）。注：points 字段
    /// "部分段有数据"在本系统会让 .fnm 分歧（point flags 数据依赖，
    /// segment_builder.rs:136-142）——那是 field infos 断言的拒绝场景
    /// （Step 10/15 已覆盖），不属于归并路径。
    #[test]
    fn force_merge_field_level_empties() {
        let root = temp_dir("fmsparse");
        let dir = FSDirectory::open(&root).unwrap();
        let mut schema = Schema::new();
        schema.add(FieldSpec::keyword("level"));
        schema.add(FieldSpec::text("message"));
        schema.add(FieldSpec::numeric_dv("latency"));
        schema.add(FieldSpec::sorted_dv("host"));
        let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();
        // 段 0：200 docs 全字段（latency/host 有值）
        for i in 0..200u64 {
            let mut d = Document::new();
            d.add("level", FieldValue::Keyword("INFO".into()));
            d.add("message", FieldValue::Text(format!("m{}", i % 5)));
            d.add("latency", FieldValue::Long(i as i64));
            d.add("host", FieldValue::Keyword(format!("h{}", i % 3)));
            w.add_document(d).unwrap();
        }
        w.commit().unwrap();
        // 段 1：50 docs 只有 level/message（latency/host 字段级全缺）
        for i in 0..50u64 {
            let mut d = Document::new();
            d.add("level", FieldValue::Keyword("WARN".into()));
            d.add("message", FieldValue::Text(format!("m{}", i % 5)));
            w.add_document(d).unwrap();
        }
        w.commit().unwrap();
        drop(w);

        force_merge(&dir, &IndexWriterConfig::default()).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        assert_eq!(s.segment_count(), 1);
        assert_eq!(s.max_doc(), 250);
        assert_eq!(s.count(&Query::term("level", "INFO")).unwrap(), 200);
        assert_eq!(s.count(&Query::term("level", "WARN")).unwrap(), 50);
        for i in 0..5 {
            assert_eq!(
                s.count(&Query::term("message", &format!("m{i}"))).unwrap(),
                50
            );
        }
        // DV 层断言：latency = 段 0 的 200 条原样（docs 0..199，无偏移）
        let (infos, _gen) = SegmentInfos::read_latest(&dir).unwrap();
        let sci = &infos.segments[0];
        let dvr = codec_lucene9::doc_values_read::DocValuesReader::open(
            &dir,
            &sci.info.name,
            &sci.info.id,
            "Lucene90_0",
        )
        .unwrap();
        let fis = FieldInfos::read(&dir, &sci.info.name, &sci.info.id, "").unwrap();
        let lat = fis.by_name("latency").unwrap();
        let vals = dvr.numeric_values(lat.number).unwrap();
        assert_eq!(vals.len(), 200);
        assert_eq!(vals[42], (42, 42));
        let host = fis.by_name("host").unwrap();
        let dict = dvr.sorted_dict(host.number).unwrap();
        assert_eq!(dict, vec![b"h0".to_vec(), b"h1".to_vec(), b"h2".to_vec()]);
        fs::remove_dir_all(&root).unwrap();
    }

    /// spec §4.4：bitmap on/off × positions 四组合。
    #[test]
    fn force_merge_bitmap_positions_matrix() {
        for (bitmap, positions) in [(false, false), (true, false), (false, true), (true, true)] {
            let root = temp_dir(&format!("fmedge-{bitmap}-{positions}"));
            let dir = FSDirectory::open(&root).unwrap();
            let mut schema = Schema::new();
            schema.add(FieldSpec::keyword("level"));
            schema.add(if positions {
                FieldSpec::text_with_positions("message")
            } else {
                FieldSpec::text("message")
            });
            schema.add(FieldSpec::numeric_dv("latency")); // 段 1 全缺（稀疏）
            schema.add(FieldSpec::sorted_dv("host")); // 段 1 全缺
            let mut config = IndexWriterConfig::default();
            config.bitmap = bitmap;
            config.bitmap_threshold = 4096;
            let mut w = IndexWriter::create(&root, schema, config).unwrap();
            // 段 0：df 做厚一点让 bitmap 门真命中（重复 level=INFO ≥4096 docs）
            for i in 0..5000u64 {
                let mut d = Document::new();
                d.add("level", FieldValue::Keyword("INFO".into()));
                d.add("message", FieldValue::Text(format!("m{}", i % 10)));
                d.add("latency", FieldValue::Long(i as i64));
                d.add("host", FieldValue::Keyword(format!("h{}", i % 3)));
                w.add_document(d).unwrap();
            }
            w.commit().unwrap();
            // 段 1：level/message 有值，latency/host 全缺（字段级空）
            for i in 0..100u64 {
                let mut d = Document::new();
                d.add("level", FieldValue::Keyword("WARN".into()));
                d.add("message", FieldValue::Text(format!("m{}", i % 10)));
                w.add_document(d).unwrap();
            }
            w.commit().unwrap();
            drop(w);

            let count_battery = |s: &mut Searcher, positions: bool| {
                let mut counts = Vec::new();
                for t in ["INFO", "WARN"] {
                    counts.push(s.count(&Query::term("level", t)).unwrap());
                }
                for i in 0..10 {
                    counts.push(s.count(&Query::term("message", &format!("m{i}"))).unwrap());
                }
                if positions {
                    counts.push(s.count(&Query::phrase("message", &["m1"])).unwrap());
                }
                counts
            };
            let pre_counts = {
                let mut s = Searcher::open(&dir).unwrap();
                count_battery(&mut s, positions)
            };
            force_merge(
                &dir,
                &IndexWriterConfig {
                    bitmap,
                    bitmap_threshold: 4096,
                    ..IndexWriterConfig::default()
                },
            )
            .unwrap();
            let mut s = Searcher::open(&dir).unwrap();
            assert_eq!(s.segment_count(), 1);
            assert_eq!(s.max_doc(), 5100);
            assert_eq!(
                pre_counts,
                count_battery(&mut s, positions),
                "bitmap={bitmap} positions={positions}"
            );
            fs::remove_dir_all(&root).unwrap();
        }
    }
}
