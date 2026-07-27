// ShardMetadata — shard 目录下 metadata.json 的读写（设计文档 §4.6）。
//
// 每个 shard 目录根部有一个 metadata.json，包含 ~25 个字段，描述 shard 的
// 身份、统计信息、状态、时间范围、HLL 基数估计和合并谱系。
//
// 序列化约定：
//   - camelCase 字段名（与 Java 侧对齐）
//   - pretty-print（便于人工检查）
//   - Option<T> 为 None 时省略
//   - Vec 为空时省略

use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

/// metadata.json 文件名。
const METADATA_FILE: &str = "metadata.json";

/// Shard 元数据（对应 metadata.json）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShardMetadata {
    // ── Identity ──
    pub org_id: String,
    /// 日期字符串，如 "2026-07-27"。
    pub date: String,
    pub shard_id: u64,

    // ── Statistics ──
    pub total_samples: u64,
    pub series_count: u64,
    pub min_time: i64,
    pub max_time: i64,
    pub size_bytes: u64,

    // ── State ──
    pub sealed: bool,
    pub merged: bool,
    /// "raw" | "5m" | "1h"
    pub resolution: String,
    pub downsample_done: bool,
    /// "L0" | "COMPACT"
    pub level: String,
    /// "ACTIVE" | "COVERED" | "DELETED"
    pub state: String,

    // ── Time range ──
    pub write_start_time: i64,
    pub write_end_time: i64,

    // ── HLL ──
    /// Base64 编码的 HLL compact 字节。
    pub series_hll: String,
    /// ceil(sketch.estimate())
    pub series_estimate: u64,

    // ── Lineage (merge tracking) ──
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merged_from: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_shard_ids: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub covered_shard_ids: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covered_by: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covered_at: Option<i64>,

    // ── Source stats (for compact shards) ──
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_shard_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_series_estimate: Option<u64>,

    // ── Timestamps ──
    pub created_at: i64,
    pub updated_at: i64,
}

impl ShardMetadata {
    /// 从 shard 目录读取 metadata.json。
    pub fn read(shard_dir: &Path) -> io::Result<Self> {
        let path = shard_dir.join(METADATA_FILE);
        let content = std::fs::read_to_string(&path)?;
        serde_json::from_str(&content).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// 将 metadata.json 写入 shard 目录（pretty-print）。
    pub fn write(&self, shard_dir: &Path) -> io::Result<()> {
        let path = shard_dir.join(METADATA_FILE);
        let content =
            serde_json::to_string_pretty(self).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        std::fs::write(&path, content)
    }

    /// 为 flush 后的新 raw shard 创建元数据（各字段取默认值）。
    pub fn new_raw(org_id: &str, date: &str, shard_id: u64) -> Self {
        Self {
            org_id: org_id.to_string(),
            date: date.to_string(),
            shard_id,

            total_samples: 0,
            series_count: 0,
            min_time: 0,
            max_time: 0,
            size_bytes: 0,

            sealed: false,
            merged: false,
            resolution: "raw".to_string(),
            downsample_done: false,
            level: "L0".to_string(),
            state: "ACTIVE".to_string(),

            write_start_time: 0,
            write_end_time: 0,

            series_hll: String::new(),
            series_estimate: 0,

            merged_from: Vec::new(),
            source_shard_ids: Vec::new(),
            covered_shard_ids: Vec::new(),
            covered_by: None,
            covered_at: None,

            source_size_bytes: None,
            source_shard_count: None,
            source_series_estimate: None,

            created_at: 0,
            updated_at: 0,
        }
    }

    /// 从 HllSketch 更新 HLL 相关字段。
    ///
    /// - `series_hll` = base64(sketch.to_compact_bytes())
    /// - `series_estimate` = ceil(sketch.estimate())
    pub fn set_hll(&mut self, sketch: &crate::algo::hll::HllSketch) {
        use base64::Engine;
        let bytes = sketch.to_compact_bytes();
        self.series_hll = base64::engine::general_purpose::STANDARD.encode(&bytes);
        self.series_estimate = sketch.estimate().ceil() as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 返回测试用临时目录（基于 PID 避免冲突）。
    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("metric_metadata_test_{}_{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 构造一个所有字段都有值的完整 metadata（用于 roundtrip 测试）。
    fn full_metadata() -> ShardMetadata {
        ShardMetadata {
            org_id: "org-001".to_string(),
            date: "2026-07-27".to_string(),
            shard_id: 42,

            total_samples: 1_000_000,
            series_count: 5000,
            min_time: 1_753_574_400_000,
            max_time: 1_753_660_800_000,
            size_bytes: 10_485_760,

            sealed: true,
            merged: true,
            resolution: "5m".to_string(),
            downsample_done: true,
            level: "COMPACT".to_string(),
            state: "COVERED".to_string(),

            write_start_time: 1_753_574_400_000,
            write_end_time: 1_753_660_800_000,

            series_hll: "AAECAwQ=".to_string(),
            series_estimate: 5100,

            merged_from: vec![10, 11, 12],
            source_shard_ids: vec![1, 2, 3],
            covered_shard_ids: vec![4, 5],
            covered_by: Some(99),
            covered_at: Some(1_753_700_000_000),

            source_size_bytes: Some(20_971_520),
            source_shard_count: Some(3),
            source_series_estimate: Some(4800),

            created_at: 1_753_574_400_000,
            updated_at: 1_753_700_000_000,
        }
    }

    #[test]
    fn test_metadata_roundtrip() {
        let dir = test_dir("roundtrip");
        let meta = full_metadata();
        meta.write(&dir).unwrap();

        let loaded = ShardMetadata::read(&dir).unwrap();
        assert_eq!(meta, loaded);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_metadata_new_raw() {
        let meta = ShardMetadata::new_raw("org-x", "2026-01-15", 7);

        assert_eq!(meta.org_id, "org-x");
        assert_eq!(meta.date, "2026-01-15");
        assert_eq!(meta.shard_id, 7);

        assert_eq!(meta.total_samples, 0);
        assert_eq!(meta.series_count, 0);
        assert_eq!(meta.min_time, 0);
        assert_eq!(meta.max_time, 0);
        assert_eq!(meta.size_bytes, 0);

        assert!(!meta.sealed);
        assert!(!meta.merged);
        assert_eq!(meta.resolution, "raw");
        assert!(!meta.downsample_done);
        assert_eq!(meta.level, "L0");
        assert_eq!(meta.state, "ACTIVE");

        assert_eq!(meta.write_start_time, 0);
        assert_eq!(meta.write_end_time, 0);

        assert_eq!(meta.series_hll, "");
        assert_eq!(meta.series_estimate, 0);

        assert!(meta.merged_from.is_empty());
        assert!(meta.source_shard_ids.is_empty());
        assert!(meta.covered_shard_ids.is_empty());
        assert_eq!(meta.covered_by, None);
        assert_eq!(meta.covered_at, None);

        assert_eq!(meta.source_size_bytes, None);
        assert_eq!(meta.source_shard_count, None);
        assert_eq!(meta.source_series_estimate, None);

        assert_eq!(meta.created_at, 0);
        assert_eq!(meta.updated_at, 0);
    }

    #[test]
    fn test_metadata_hll() {
        use crate::algo::hll::HllSketch;

        let mut sketch = HllSketch::new();
        for i in 0..1000u64 {
            // 简单确定性 hash
            let h = i.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            sketch.update(h);
        }

        let mut meta = ShardMetadata::new_raw("org-hll", "2026-03-01", 1);
        meta.set_hll(&sketch);

        // series_hll 应为有效 base64
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&meta.series_hll)
            .expect("series_hll must be valid base64");
        assert!(!decoded.is_empty());

        // series_estimate > 0
        assert!(meta.series_estimate > 0, "series_estimate should be > 0");

        // 验证 estimate 与 sketch 一致
        assert_eq!(meta.series_estimate, sketch.estimate().ceil() as u64);
    }

    #[test]
    fn test_metadata_json_format() {
        let meta = ShardMetadata::new_raw("org-fmt", "2026-07-27", 1);
        let json = serde_json::to_string_pretty(&meta).unwrap();

        // pretty-print: 包含换行和缩进
        assert!(json.contains('\n'), "JSON should be pretty-printed");
        assert!(json.contains("  "), "JSON should have indentation");

        // camelCase 字段名
        assert!(json.contains("\"orgId\""), "expected camelCase orgId");
        assert!(json.contains("\"shardId\""), "expected camelCase shardId");
        assert!(json.contains("\"totalSamples\""), "expected camelCase totalSamples");
        assert!(json.contains("\"seriesCount\""), "expected camelCase seriesCount");
        assert!(json.contains("\"minTime\""), "expected camelCase minTime");
        assert!(json.contains("\"maxTime\""), "expected camelCase maxTime");
        assert!(json.contains("\"sizeBytes\""), "expected camelCase sizeBytes");
        assert!(json.contains("\"downsampleDone\""), "expected camelCase downsampleDone");
        assert!(json.contains("\"writeStartTime\""), "expected camelCase writeStartTime");
        assert!(json.contains("\"writeEndTime\""), "expected camelCase writeEndTime");
        assert!(json.contains("\"seriesHll\""), "expected camelCase seriesHll");
        assert!(json.contains("\"seriesEstimate\""), "expected camelCase seriesEstimate");
        assert!(json.contains("\"createdAt\""), "expected camelCase createdAt");
        assert!(json.contains("\"updatedAt\""), "expected camelCase updatedAt");

        // 不应出现 snake_case
        assert!(!json.contains("\"org_id\""), "should not have snake_case");
        assert!(!json.contains("\"shard_id\""), "should not have snake_case");
    }

    #[test]
    fn test_metadata_optional_fields() {
        // new_raw 的 Option 字段均为 None，Vec 字段均为空 → JSON 中应省略
        let meta = ShardMetadata::new_raw("org-opt", "2026-07-27", 1);
        let json = serde_json::to_string_pretty(&meta).unwrap();

        assert!(!json.contains("coveredBy"), "None coveredBy should be omitted");
        assert!(!json.contains("coveredAt"), "None coveredAt should be omitted");
        assert!(!json.contains("sourceSizeBytes"), "None sourceSizeBytes should be omitted");
        assert!(!json.contains("sourceShardCount"), "None sourceShardCount should be omitted");
        assert!(!json.contains("sourceSeriesEstimate"), "None sourceSeriesEstimate should be omitted");
        assert!(!json.contains("mergedFrom"), "empty mergedFrom should be omitted");
        assert!(!json.contains("sourceShardIds"), "empty sourceShardIds should be omitted");
        assert!(!json.contains("coveredShardIds"), "empty coveredShardIds should be omitted");

        // 有值时应出现
        let full = full_metadata();
        let json_full = serde_json::to_string_pretty(&full).unwrap();
        assert!(json_full.contains("\"coveredBy\""), "Some coveredBy should be present");
        assert!(json_full.contains("\"mergedFrom\""), "non-empty mergedFrom should be present");
    }
}
