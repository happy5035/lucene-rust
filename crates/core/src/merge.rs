//! forceMerge(1) —— 格式级段归并（M6 spec §4）。总装见 `force_merge`；
//! 本模块同时承载归并用的纯函数（先单测后接格式层，spec §5.1）。

use std::io;

use codec_lucene9::field_infos::{FieldInfo, FieldInfos};
use codec_lucene9::postings_read::NO_MORE_DOCS;
use codec_lucene9::segment_info::SegmentInfo;
use codec_lucene9::segment_infos::{SegmentCommitInfo, SegmentInfos};
use codec_lucene9::{DocValuesType, FSDirectory, IndexOptions};

use crate::IndexWriterConfig;

#[cfg(test)]
use crate::search::{Query, Searcher};
#[cfg(test)]
use crate::{Document, FieldSpec, FieldValue, IndexWriter, Schema};
#[cfg(test)]
use codec_lucene9::io::DataInput;
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
pub(crate) fn merge_postings(
    _dir: &FSDirectory,
    _readers: &[SegmentMergeSource],
    _field_infos: &FieldInfos,
    _new_segment: &str,
    _new_segment_id: &[u8; 16],
    _bitmap_threshold: Option<u32>,
) -> io::Result<Vec<String>> {
    todo!("Step 12")
}

/// stored 块级裸拷贝（spec §4.2 修正后方案；Lucene90CompressingStoredFieldsWriter
/// .copyChunks :520-595 主路径——同 codec、无 delete ⇒ 恒可裸拷）。
pub(crate) fn merge_stored(
    _dir: &FSDirectory,
    _sources: &[SegmentMergeSource],
    _new_segment: &str,
    _new_segment_id: &[u8; 16],
    _total_max_doc: i32,
) -> io::Result<[String; 3]> {
    todo!("Step 14")
}

/// NumericDV：顺序读 + base 重映射 + 现有 writer 重写（spec §4.2）。
/// SortedDV：字典读 + merge_sorted_dicts 全局归并 + build_ord_remap 重映射 +
/// 逐 doc 重写 ord（spec §4.2/§5.1）。返回 [_N_Lucene90_0.{dvd,dvm}] 或空。
pub(crate) fn merge_doc_values(
    _dir: &FSDirectory,
    _sources: &[SegmentMergeSource],
    _field_infos: &FieldInfos,
    _new_segment: &str,
    _new_segment_id: &[u8; 16],
    _total_max_doc: u32,
) -> io::Result<Vec<String>> {
    todo!("Step 14")
}

/// points：T-B BKD 读路径全区间（i64::MIN..=i64::MAX）全量遍历 +
/// base 重映射，灌回现有 BKD writer（全内存排序吸收多段输入；
/// PointsWriter.mergeOneField 同款朴素归并，PointsWriter.java:42）。
pub(crate) fn merge_points(
    _dir: &FSDirectory,
    _sources: &[SegmentMergeSource],
    _field_infos: &FieldInfos,
    _new_segment: &str,
    _new_segment_id: &[u8; 16],
) -> io::Result<Vec<String>> {
    todo!("Step 14")
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
}
