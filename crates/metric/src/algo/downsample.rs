// Downsample 5 列聚合 + 列式 Gorilla 编码，见设计文档 §4.3

use std::collections::BTreeMap;

use super::gorilla;

/// 列索引常量
pub const COL_COUNT: usize = 0;
pub const COL_SUM: usize = 1;
pub const COL_MIN: usize = 2;
pub const COL_MAX: usize = 3;
pub const COL_DELTA: usize = 4;

const NUM_COLUMNS: usize = 5;

/// 5 分钟间隔 (ms)
pub const INTERVAL_5M: i64 = 300_000;
/// 1 小时间隔 (ms)
pub const INTERVAL_1H: i64 = 3_600_000;

#[derive(Debug, PartialEq)]
pub enum DownsampleError {
    EmptyInput,
    InvalidData,
    LengthMismatch { expected: usize, actual: usize },
}

impl std::fmt::Display for DownsampleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownsampleError::EmptyInput => write!(f, "empty input"),
            DownsampleError::InvalidData => write!(f, "invalid downsample data"),
            DownsampleError::LengthMismatch { expected, actual } => {
                write!(f, "length mismatch: expected {}, actual {}", expected, actual)
            }
        }
    }
}

impl std::error::Error for DownsampleError {}

/// 将原始点聚合为 5 列列式格式。
/// 点无需排序，内部按 bucket 分组。
pub fn downsample(points: &[(i64, f64)], interval_ms: i64) -> Vec<u8> {
    if points.is_empty() {
        // 空输入：12 字节 first_bucket_time=0, bucket_count=0
        let mut out = Vec::with_capacity(12);
        out.extend_from_slice(&0i64.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes());
        return out;
    }

    // 按 bucket 分组（BTreeMap 保证时间有序）
    let mut buckets: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
    for &(t, v) in points {
        let bucket_time = (t / interval_ms) * interval_ms;
        buckets.entry(bucket_time).or_default().push(v);
    }

    let bucket_count = buckets.len();
    let mut bucket_times = Vec::with_capacity(bucket_count);
    let mut columns: [Vec<f64>; NUM_COLUMNS] = [
        Vec::with_capacity(bucket_count),
        Vec::with_capacity(bucket_count),
        Vec::with_capacity(bucket_count),
        Vec::with_capacity(bucket_count),
        Vec::with_capacity(bucket_count),
    ];

    for (bt, values) in &buckets {
        bucket_times.push(*bt);
        let count = values.len() as f64;
        let sum: f64 = values.iter().sum();
        let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let first = values[0];
        let last = values[values.len() - 1];
        // Counter Reset 感知：last < first 时 delta = last
        let delta = if last < first { last } else { last - first };

        columns[COL_COUNT].push(count);
        columns[COL_SUM].push(sum);
        columns[COL_MIN].push(min);
        columns[COL_MAX].push(max);
        columns[COL_DELTA].push(delta);
    }

    encode_columns(&bucket_times, &columns)
}

/// 将 bucket_times + 5 列编码为二进制 layout
fn encode_columns(bucket_times: &[i64], columns: &[Vec<f64>; NUM_COLUMNS]) -> Vec<u8> {
    let bucket_count = bucket_times.len() as i32;
    let first_bucket_time = bucket_times[0];

    // 编码每列：[8B first_value][4B gorilla_bitstream_len][NB bitstream]
    let mut column_data: Vec<Vec<u8>> = Vec::with_capacity(NUM_COLUMNS);
    for col_values in columns {
        let first_value = col_values[0];
        let gorilla_encoded = gorilla::encode(bucket_times, col_values);
        // 跳过 20B gorilla header (first_ts 8B + first_value 8B + bitstream_len 4B)
        let bitstream = &gorilla_encoded[20..];

        let mut col_block = Vec::with_capacity(12 + bitstream.len());
        col_block.extend_from_slice(&first_value.to_le_bytes());
        col_block.extend_from_slice(&(bitstream.len() as i32).to_le_bytes());
        col_block.extend_from_slice(bitstream);
        column_data.push(col_block);
    }

    // 组装二进制 layout: header(12B) + 5 lengths(20B) + column data
    let total_len = 32 + column_data.iter().map(|c| c.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&first_bucket_time.to_le_bytes());
    out.extend_from_slice(&bucket_count.to_le_bytes());
    for col in &column_data {
        out.extend_from_slice(&(col.len() as i32).to_le_bytes());
    }
    for col in &column_data {
        out.extend_from_slice(col);
    }
    out
}

/// 解码 downsample 数据，还原为每 bucket 的 5 列值。
/// 返回 (first_bucket_time, Vec<(bucket_time, [count, sum, min, max, delta])>)
pub fn decode_downsample(
    data: &[u8],
    interval_ms: i64,
) -> Result<(i64, Vec<(i64, [f64; 5])>), DownsampleError> {
    if data.len() < 12 {
        return Err(DownsampleError::InvalidData);
    }

    let first_bucket_time = i64::from_le_bytes(data[0..8].try_into().unwrap());
    let bucket_count = i32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;

    if bucket_count == 0 {
        return Ok((first_bucket_time, Vec::new()));
    }

    if data.len() < 32 {
        return Err(DownsampleError::InvalidData);
    }

    // 读取 5 列长度
    let mut col_lengths = [0usize; NUM_COLUMNS];
    for i in 0..NUM_COLUMNS {
        let offset = 12 + i * 4;
        col_lengths[i] = i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
    }

    // 校验总长度
    let expected_total = 32 + col_lengths.iter().sum::<usize>();
    if data.len() < expected_total {
        return Err(DownsampleError::LengthMismatch {
            expected: expected_total,
            actual: data.len(),
        });
    }

    // 解码每列
    let mut columns: [Vec<f64>; NUM_COLUMNS] =
        [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    let mut offset = 32;
    for i in 0..NUM_COLUMNS {
        let col_data = &data[offset..offset + col_lengths[i]];
        offset += col_lengths[i];

        if col_data.len() < 12 {
            return Err(DownsampleError::InvalidData);
        }
        let first_value = f64::from_le_bytes(col_data[0..8].try_into().unwrap());
        let gorilla_len = i32::from_le_bytes(col_data[8..12].try_into().unwrap()) as usize;

        if col_data.len() < 12 + gorilla_len {
            return Err(DownsampleError::InvalidData);
        }
        let bitstream = &col_data[12..12 + gorilla_len];

        // 重建 gorilla 字节流：first_bucket_time(8B) + first_value(8B) + gorilla_len(4B) + bitstream
        let mut gorilla_bytes = Vec::with_capacity(20 + gorilla_len);
        gorilla_bytes.extend_from_slice(&first_bucket_time.to_le_bytes());
        gorilla_bytes.extend_from_slice(&first_value.to_le_bytes());
        gorilla_bytes.extend_from_slice(&(gorilla_len as u32).to_le_bytes());
        gorilla_bytes.extend_from_slice(bitstream);

        let (_times, values) =
            gorilla::decode(&gorilla_bytes, bucket_count).map_err(|_| DownsampleError::InvalidData)?;

        columns[i] = values;
    }

    // 组装结果：bucket 时间等间隔递增
    let mut result = Vec::with_capacity(bucket_count);
    for i in 0..bucket_count {
        let bucket_time = first_bucket_time + (i as i64) * interval_ms;
        result.push((
            bucket_time,
            [
                columns[COL_COUNT][i],
                columns[COL_SUM][i],
                columns[COL_MIN][i],
                columns[COL_MAX][i],
                columns[COL_DELTA][i],
            ],
        ));
    }

    Ok((first_bucket_time, result))
}

/// 将 5m downsample buckets 聚合为更粗粒度（如 5m → 1h）。
/// 输入：已解码的 5m buckets。输出：以 interval_ms 重新编码。
pub fn aggregate_downsample_buckets(buckets: &[(i64, [f64; 5])], interval_ms: i64) -> Vec<u8> {
    if buckets.is_empty() {
        let mut out = Vec::with_capacity(12);
        out.extend_from_slice(&0i64.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes());
        return out;
    }

    // 按粗粒度 bucket 分组
    let mut groups: BTreeMap<i64, Vec<&[f64; 5]>> = BTreeMap::new();
    for (t, cols) in buckets {
        let coarse_time = (t / interval_ms) * interval_ms;
        groups.entry(coarse_time).or_default().push(cols);
    }

    let bucket_count = groups.len();
    let mut bucket_times = Vec::with_capacity(bucket_count);
    let mut columns: [Vec<f64>; NUM_COLUMNS] = [
        Vec::with_capacity(bucket_count),
        Vec::with_capacity(bucket_count),
        Vec::with_capacity(bucket_count),
        Vec::with_capacity(bucket_count),
        Vec::with_capacity(bucket_count),
    ];

    for (bt, group) in &groups {
        bucket_times.push(*bt);
        let count: f64 = group.iter().map(|c| c[COL_COUNT]).sum();
        let sum: f64 = group.iter().map(|c| c[COL_SUM]).sum();
        let min = group
            .iter()
            .map(|c| c[COL_MIN])
            .fold(f64::INFINITY, f64::min);
        let max = group
            .iter()
            .map(|c| c[COL_MAX])
            .fold(f64::NEG_INFINITY, f64::max);
        // delta 聚合：各子 bucket delta 之和（telescoping 近似）
        let delta: f64 = group.iter().map(|c| c[COL_DELTA]).sum();

        columns[COL_COUNT].push(count);
        columns[COL_SUM].push(sum);
        columns[COL_MIN].push(min);
        columns[COL_MAX].push(max);
        columns[COL_DELTA].push(delta);
    }

    encode_columns(&bucket_times, &columns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_downsample_single_bucket() {
        // 所有点落入同一 bucket
        let base = 300_000i64; // bucket [300000, 600000)
        let points = vec![
            (base + 1000, 10.0),
            (base + 2000, 20.0),
            (base + 3000, 30.0),
        ];
        let encoded = downsample(&points, INTERVAL_5M);

        let (first_bt, buckets) = decode_downsample(&encoded, INTERVAL_5M).unwrap();
        assert_eq!(first_bt, base);
        assert_eq!(buckets.len(), 1);

        let (bt, cols) = &buckets[0];
        assert_eq!(*bt, base);
        assert!((cols[COL_COUNT] - 3.0).abs() < 1e-15);
        assert!((cols[COL_SUM] - 60.0).abs() < 1e-15);
        assert!((cols[COL_MIN] - 10.0).abs() < 1e-15);
        assert!((cols[COL_MAX] - 30.0).abs() < 1e-15);
        assert!((cols[COL_DELTA] - 20.0).abs() < 1e-15); // 30 - 10
    }

    #[test]
    fn test_downsample_multiple_buckets() {
        // 3 个 bucket，每 bucket 2 点
        let points = vec![
            (300_000 + 100, 1.0),
            (300_000 + 200, 2.0),
            (600_000 + 100, 10.0),
            (600_000 + 200, 20.0),
            (900_000 + 100, 100.0),
            (900_000 + 200, 200.0),
        ];
        let encoded = downsample(&points, INTERVAL_5M);
        let (first_bt, buckets) = decode_downsample(&encoded, INTERVAL_5M).unwrap();

        assert_eq!(first_bt, 300_000);
        assert_eq!(buckets.len(), 3);

        // bucket 0
        assert_eq!(buckets[0].0, 300_000);
        assert!((buckets[0].1[COL_COUNT] - 2.0).abs() < 1e-15);
        assert!((buckets[0].1[COL_SUM] - 3.0).abs() < 1e-15);
        assert!((buckets[0].1[COL_MIN] - 1.0).abs() < 1e-15);
        assert!((buckets[0].1[COL_MAX] - 2.0).abs() < 1e-15);
        assert!((buckets[0].1[COL_DELTA] - 1.0).abs() < 1e-15);

        // bucket 1
        assert_eq!(buckets[1].0, 600_000);
        assert!((buckets[1].1[COL_SUM] - 30.0).abs() < 1e-15);
        assert!((buckets[1].1[COL_DELTA] - 10.0).abs() < 1e-15);

        // bucket 2
        assert_eq!(buckets[2].0, 900_000);
        assert!((buckets[2].1[COL_SUM] - 300.0).abs() < 1e-15);
        assert!((buckets[2].1[COL_DELTA] - 100.0).abs() < 1e-15);
    }

    #[test]
    fn test_downsample_roundtrip() {
        // 多点多 bucket，验证 encode → decode 全值匹配
        let points: Vec<(i64, f64)> = (0..20)
            .map(|i| (300_000 + i * 60_000, (i as f64) * 1.5 + 0.5))
            .collect();
        let encoded = downsample(&points, INTERVAL_5M);
        let (first_bt, buckets) = decode_downsample(&encoded, INTERVAL_5M).unwrap();

        assert_eq!(first_bt, 300_000);
        // 20 点跨 4 个 bucket（每 bucket 5 点）
        assert_eq!(buckets.len(), 4);

        // 验证 bucket 0: 点 i=0..4, 值 0.5, 2.0, 3.5, 5.0, 6.5
        let b0 = &buckets[0].1;
        assert!((b0[COL_COUNT] - 5.0).abs() < 1e-15);
        assert!((b0[COL_SUM] - 17.5).abs() < 1e-15);
        assert!((b0[COL_MIN] - 0.5).abs() < 1e-15);
        assert!((b0[COL_MAX] - 6.5).abs() < 1e-15);
        assert!((b0[COL_DELTA] - 6.0).abs() < 1e-15); // 6.5 - 0.5
    }

    #[test]
    fn test_downsample_counter_reset() {
        // Counter reset: values [100, 50, 80]，last(80) < first(100) → delta = last = 80
        let base = 300_000i64;
        let points = vec![
            (base + 1000, 100.0),
            (base + 2000, 50.0),
            (base + 3000, 80.0),
        ];
        let encoded = downsample(&points, INTERVAL_5M);
        let (_, buckets) = decode_downsample(&encoded, INTERVAL_5M).unwrap();

        assert_eq!(buckets.len(), 1);
        let cols = &buckets[0].1;
        assert!((cols[COL_COUNT] - 3.0).abs() < 1e-15);
        assert!((cols[COL_SUM] - 230.0).abs() < 1e-15);
        assert!((cols[COL_MIN] - 50.0).abs() < 1e-15);
        assert!((cols[COL_MAX] - 100.0).abs() < 1e-15);
        // last(80) < first(100) → delta = last = 80
        assert!((cols[COL_DELTA] - 80.0).abs() < 1e-15);
    }

    #[test]
    fn test_downsample_empty() {
        let encoded = downsample(&[], INTERVAL_5M);
        assert_eq!(encoded.len(), 12);

        let first_bt = i64::from_le_bytes(encoded[0..8].try_into().unwrap());
        let count = i32::from_le_bytes(encoded[8..12].try_into().unwrap());
        assert_eq!(first_bt, 0);
        assert_eq!(count, 0);

        let (fbt, buckets) = decode_downsample(&encoded, INTERVAL_5M).unwrap();
        assert_eq!(fbt, 0);
        assert!(buckets.is_empty());
    }

    #[test]
    fn test_aggregate_5m_to_1h() {
        // 12 个 5m bucket → 1 个 1h bucket
        let mut buckets_5m: Vec<(i64, [f64; 5])> = Vec::new();
        for i in 0..12i64 {
            let bt = i * INTERVAL_5M;
            // count=10, sum=100+i, min=i, max=20+i, delta=5
            buckets_5m.push((bt, [10.0, 100.0 + i as f64, i as f64, 20.0 + i as f64, 5.0]));
        }

        let encoded = aggregate_downsample_buckets(&buckets_5m, INTERVAL_1H);
        let (first_bt, result) = decode_downsample(&encoded, INTERVAL_1H).unwrap();

        assert_eq!(first_bt, 0);
        assert_eq!(result.len(), 1);

        let (_, cols) = &result[0];
        // count = 12 * 10 = 120
        assert!((cols[COL_COUNT] - 120.0).abs() < 1e-15);
        // sum = sum(100+i for i in 0..12) = 12*100 + 66 = 1266
        assert!((cols[COL_SUM] - 1266.0).abs() < 1e-15);
        // min = min(0..12) = 0
        assert!((cols[COL_MIN] - 0.0).abs() < 1e-15);
        // max = max(20+i for i in 0..12) = 31
        assert!((cols[COL_MAX] - 31.0).abs() < 1e-15);
        // delta = 12 * 5 = 60
        assert!((cols[COL_DELTA] - 60.0).abs() < 1e-15);
    }

    #[test]
    fn test_downsample_deterministic() {
        let points = vec![
            (300_001, 1.0),
            (300_002, 2.0),
            (600_001, 3.0),
            (600_002, 4.0),
            (900_001, 5.0),
        ];
        let encoded1 = downsample(&points, INTERVAL_5M);
        let encoded2 = downsample(&points, INTERVAL_5M);
        assert_eq!(encoded1, encoded2, "same input must produce same output");
    }
}
