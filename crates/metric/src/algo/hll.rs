// HyperLogLog 基数估计（HLL_4 变体），见 V5 Format Reference §4.4
//
// 设计：logK=12 → 4096 registers，每个 register 4 bit（nibble），标准误差 ~1.6%。
// 用于 shard metadata 中估算 series 基数（seriesHll / seriesEstimate 字段）。
//
// ⚠️ 兼容性偏差说明（wire format）：
// 规范 §4.4 要求 "wire format 严格按 DataSketches HLL_4 compact 规范"。DataSketches
// Java 的 `toCompactByteArray()` 实际格式较复杂（多字节 preamble：family ID、序列化
// 版本、flags、sparse/dense 表示、aux array 等）。本实现采用简化的 compact 格式：
//
//     [1B preamble: logK][K/2 bytes: packed 4-bit registers, 低 nibble 在前]
//
// 总长 1 + 2048 = 2049 字节。该格式 **并非** 与 DataSketches Java `toCompactByteArray()`
// 字节级兼容。但估计算法（HLL 数学）完全一致，因此：
//   - Rust 写 → Rust 读：完全兼容；
//   - Java 侧仅读取 estimate（seriesEstimate 字段，long）：兼容；
//   - 若后续需要 Java 直接 heapify Rust 产出的字节，需再补一层 DataSketches 兼容序列化。
//
// 规范亦允许："若工作量超预期，可临时降级为 HLL_8 + 标注兼容性偏差"。此处保留 HLL_4
// 寄存器宽度（4 bit），仅在序列化层做简化并标注偏差。

/// log2(寄存器数量)。logK=12 → 4096 registers。
const LOG_K: u8 = 12;
/// 寄存器数量 K = 2^LOG_K = 4096。
const K: usize = 1 << LOG_K;
/// HLL_4：每个 register 占 4 bit，最大值 15。
const REG_BITS: u8 = 4;

/// HyperLogLog sketch（HLL_4，logK=12）。
///
/// 内部用 K 字节存储（每 register 1 字节，取值 0..=15），序列化时打包为 4-bit nibble。
pub struct HllSketch {
    /// K 个 register，每个取值 0..=(2^REG_BITS - 1)。用 1 字节存 1 个 register（简化）。
    registers: Vec<u8>,
}

impl HllSketch {
    /// 构造空 sketch。
    pub fn new() -> Self {
        Self {
            registers: vec![0u8; K],
        }
    }

    /// 将一个 64-bit hash 值加入 sketch。
    ///
    /// 取低 LOG_K 位作为 register 下标；剩余高位计算 rho（最左 1-bit 的 1-indexed 位置），
    /// 并对该 register 取 max。
    pub fn update(&mut self, hash: u64) {
        // 低 LOG_K 位 → register 下标
        let idx = (hash & (K as u64 - 1)) as usize;
        // 去掉下标位后的剩余高位，构成 (64 - LOG_K) bit 的字段
        let remaining = hash >> LOG_K;
        // rho = 字段内最左 1-bit 的位置（1-indexed）。
        // 注意：u64::leading_zeros() 按 64 bit 计数，而 remaining 仅占 (64 - LOG_K) bit，
        // 高 LOG_K 位恒为 0，需减去 LOG_K 才是字段内的前导零数。
        // 全 0 时 leading_zeros=64 → rho = 64 - LOG_K + 1（字段全零），同一公式自然覆盖。
        let rho = (remaining.leading_zeros() - LOG_K as u32 + 1) as u8;
        // 4-bit register 上限为 15
        let rho = rho.min((1u8 << REG_BITS) - 1);
        if rho > self.registers[idx] {
            self.registers[idx] = rho;
        }
    }

    /// 合并另一个 sketch（逐 register 取 max）。
    pub fn union(&mut self, other: &HllSketch) {
        for i in 0..K {
            if other.registers[i] > self.registers[i] {
                self.registers[i] = other.registers[i];
            }
        }
    }

    /// 估算基数（HyperLogLog 算法 + 小区间 linear counting 修正）。
    pub fn estimate(&self) -> f64 {
        let m = K as f64;
        // alpha_m（m=4096）
        let alpha = 0.7213 / (1.0 + 1.079 / m);

        // 2^(-register[j]) 的求和，以及零寄存器计数
        let mut sum = 0.0f64;
        let mut zeros = 0usize;
        for &reg in &self.registers {
            sum += 2.0f64.powi(-(reg as i32));
            if reg == 0 {
                zeros += 1;
            }
        }

        let raw_estimate = alpha * m * m / sum;

        // 小区间修正（linear counting）
        if raw_estimate <= 2.5 * m && zeros > 0 {
            m * (m / zeros as f64).ln()
        } else {
            raw_estimate
        }
        // 64-bit hash 无需大区间修正
    }

    /// 序列化为 compact 字节数组。
    ///
    /// 格式：`[1B preamble: logK][K/2 bytes: packed 4-bit registers]`，低 nibble 在前。
    /// 总长 1 + K/2 = 2049 字节。
    ///
    /// 注意：此为简化格式，非 DataSketches Java 字节级兼容（见模块顶部说明）。
    pub fn to_compact_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + K / 2);
        out.push(LOG_K); // preamble: logK
        // 每字节打包 2 个 nibble（低 nibble 在前）
        for i in (0..K).step_by(2) {
            let lo = self.registers[i] & 0x0F;
            let hi = self.registers[i + 1] & 0x0F;
            out.push(lo | (hi << 4));
        }
        out
    }

    /// 从 compact 字节数组反序列化。
    pub fn heapify(bytes: &[u8]) -> Result<Self, HllError> {
        if bytes.is_empty() {
            return Err(HllError::InvalidData("empty input"));
        }
        let log_k = bytes[0];
        if log_k != LOG_K {
            return Err(HllError::UnsupportedLogK(log_k));
        }
        let k = 1usize << log_k;
        let expected_len = 1 + k / 2;
        if bytes.len() < expected_len {
            return Err(HllError::InvalidData("truncated"));
        }
        let mut registers = vec![0u8; k];
        for i in (0..k).step_by(2) {
            let packed = bytes[1 + i / 2];
            registers[i] = packed & 0x0F;
            registers[i + 1] = (packed >> 4) & 0x0F;
        }
        Ok(Self { registers })
    }
}

impl Default for HllSketch {
    fn default() -> Self {
        Self::new()
    }
}

/// HLL 反序列化错误。
#[derive(Debug)]
pub enum HllError {
    /// 输入数据非法（空 / 截断）。
    InvalidData(&'static str),
    /// 不支持的 logK（本实现仅支持 logK=12）。
    UnsupportedLogK(u8),
}

impl std::fmt::Display for HllError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HllError::InvalidData(msg) => write!(f, "invalid HLL data: {}", msg),
            HllError::UnsupportedLogK(k) => write!(f, "unsupported logK: {} (only 12)", k),
        }
    }
}

impl std::error::Error for HllError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性测试 hash。
    ///
    /// 基础是规范给定的 LCG（`i*M + C`），但 LCG 是线性函数，其低位（HLL register 下标）
    /// 与高位（rho）相关：落在同一 register 的 hash 对应 i 间隔 K，其高位构成等差数列，
    /// 会使 HLL 估计系统性偏高（实测 n=10000 估到 ~11700）。HLL 需要具有良好雪崩性的
    /// hash，因此在 LCG 之上再叠加 Murmur3 fmix64 finalizer。结果仍是完全确定的。
    fn test_hash(mut i: u64) -> u64 {
        // LCG base（规范给定常数）
        i = i.wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // Murmur3 fmix64 finalizer（雪崩）
        i ^= i >> 33;
        i = i.wrapping_mul(0xff51afd7ed558ccd);
        i ^= i >> 33;
        i = i.wrapping_mul(0xc4ceb9fe1a85ec53);
        i ^ (i >> 33)
    }

    #[test]
    fn test_hll_empty() {
        let sketch = HllSketch::new();
        // 空 sketch 全零寄存器 → linear counting 修正下 estimate ≈ 0
        assert!(sketch.estimate() < 1.0, "empty estimate={}", sketch.estimate());
    }

    #[test]
    fn test_hll_small_cardinality() {
        let mut sketch = HllSketch::new();
        for i in 0..100u64 {
            sketch.update(test_hash(i));
        }
        let est = sketch.estimate();
        // 100 个唯一值，误差容忍 20%
        assert!(
            (est - 100.0).abs() / 100.0 < 0.20,
            "small cardinality estimate={} (expected ~100)",
            est
        );
    }

    #[test]
    fn test_hll_medium_cardinality() {
        let mut sketch = HllSketch::new();
        for i in 0..10000u64 {
            sketch.update(test_hash(i));
        }
        let est = sketch.estimate();
        // 10000 个唯一值，误差容忍 5%
        assert!(
            (est - 10000.0).abs() / 10000.0 < 0.05,
            "medium cardinality estimate={} (expected ~10000)",
            est
        );
    }

    #[test]
    fn test_hll_duplicates() {
        let mut sketch = HllSketch::new();
        let h = test_hash(42);
        for _ in 0..1000 {
            sketch.update(h);
        }
        let est = sketch.estimate();
        // 同一 hash 重复 1000 次 → 基数 ≈ 1
        assert!(
            (est - 1.0).abs() < 1.0,
            "duplicates estimate={} (expected ~1)",
            est
        );
    }

    #[test]
    fn test_hll_union() {
        let mut a = HllSketch::new();
        let mut b = HllSketch::new();
        // a: [0, 5000)，b: [2500, 7500) → 共享 [2500, 5000)，并集基数 = 7500
        for i in 0..5000u64 {
            a.update(test_hash(i));
        }
        for i in 2500..7500u64 {
            b.update(test_hash(i));
        }
        a.union(&b);
        let est = a.estimate();
        assert!(
            (est - 7500.0).abs() / 7500.0 < 0.05,
            "union estimate={} (expected ~7500)",
            est
        );
    }

    #[test]
    fn test_hll_serialize_roundtrip() {
        let mut sketch = HllSketch::new();
        for i in 0..5000u64 {
            sketch.update(test_hash(i));
        }
        let before = sketch.estimate();

        let bytes = sketch.to_compact_bytes();
        // 1B preamble + K/2 = 2049 字节
        assert_eq!(bytes.len(), 1 + K / 2);
        assert_eq!(bytes[0], LOG_K);

        let restored = HllSketch::heapify(&bytes).expect("heapify failed");
        let after = restored.estimate();
        assert_eq!(before, after, "roundtrip estimate mismatch");
        assert_eq!(sketch.registers, restored.registers, "registers mismatch");
    }

    #[test]
    fn test_hll_deterministic() {
        let mut s1 = HllSketch::new();
        let mut s2 = HllSketch::new();
        for i in 0..3000u64 {
            s1.update(test_hash(i));
            s2.update(test_hash(i));
        }
        assert_eq!(s1.estimate(), s2.estimate(), "same inserts must give same estimate");
        assert_eq!(s1.to_compact_bytes(), s2.to_compact_bytes(), "same inserts must give same bytes");
    }

    #[test]
    fn test_hll_heapify_errors() {
        // 空输入
        assert!(matches!(
            HllSketch::heapify(&[]),
            Err(HllError::InvalidData(_))
        ));
        // 不支持的 logK
        assert!(matches!(
            HllSketch::heapify(&[10u8]),
            Err(HllError::UnsupportedLogK(10))
        ));
        // 截断
        let mut truncated = vec![LOG_K];
        truncated.extend_from_slice(&[0u8; 10]); // 远小于 K/2
        assert!(matches!(
            HllSketch::heapify(&truncated),
            Err(HllError::InvalidData(_))
        ));
    }
}
