// Gorilla 编解码，见 V5 Format Reference R2

use std::convert::TryInto;

/// MSB-first bit writer。bitstream 末尾 0 padding。
pub struct BitWriter {
    buf: Vec<u8>,
    current: u64, // 当前正在填充的 64-bit buffer，MSB first
    bit_pos: u32, // 已写入 current 的 bit 数 (0..64)
}

impl BitWriter {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            current: 0,
            bit_pos: 0,
        }
    }

    /// 写入 n bit（value 的低 n 位，MSB first）
    pub fn write_bits(&mut self, value: u64, n: u32) {
        if n == 0 {
            return;
        }
        let value = if n == 64 { value } else { value & ((1u64 << n) - 1) };

        let mut remaining = n;
        let mut val = value;

        while remaining > 0 {
            let space = 64 - self.bit_pos;
            if remaining <= space {
                self.current |= val << (space - remaining);
                self.bit_pos += remaining;
                remaining = 0;
            } else {
                // Fill current buffer with high bits of val
                self.current |= val >> (remaining - space);
                self.bit_pos = 64;
                remaining -= space;
                val &= if remaining == 64 { u64::MAX } else { (1u64 << remaining) - 1 };
            }
            // Flush complete bytes
            while self.bit_pos >= 8 {
                self.buf.push((self.current >> 56) as u8);
                self.current <<= 8;
                self.bit_pos -= 8;
            }
        }
    }

    /// 写入 1 bit
    pub fn write_bit(&mut self, bit: bool) {
        self.write_bits(if bit { 1 } else { 0 }, 1);
    }

    pub fn finish(mut self) -> Vec<u8> {
        // flush 剩余 bit（0 padding）
        if self.bit_pos > 0 {
            self.buf.push((self.current >> 56) as u8);
        }
        self.buf
    }
}

/// MSB-first bit reader。
pub struct BitReader<'a> {
    buf: &'a [u8],
    pos: u64, // 当前 bit 位置
}

impl<'a> BitReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// 读 n bit，返回 u64（低 n 位有效）
    pub fn read_bits(&mut self, n: u32) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut result: u64 = 0;
        for _ in 0..n {
            let byte_idx = (self.pos / 8) as usize;
            let bit_idx = 7 - (self.pos % 8); // MSB first
            let bit = if byte_idx < self.buf.len() {
                (self.buf[byte_idx] >> bit_idx) & 1
            } else {
                0
            };
            result = (result << 1) | bit as u64;
            self.pos += 1;
        }
        result
    }

    pub fn read_bit(&mut self) -> bool {
        self.read_bits(1) == 1
    }
}

#[derive(Debug)]
pub enum GorillaError {
    EmptyInput,
    InvalidBitstream,
    LengthMismatch { expected: usize, actual: usize },
}

impl std::fmt::Display for GorillaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GorillaError::EmptyInput => write!(f, "empty input"),
            GorillaError::InvalidBitstream => write!(f, "invalid bitstream"),
            GorillaError::LengthMismatch { expected, actual } => {
                write!(f, "length mismatch: expected {}, actual {}", expected, actual)
            }
        }
    }
}

impl std::error::Error for GorillaError {}

/// 编码 times/values 为 Gorilla 二进制（含 20B header）。
/// 见 V5 Format Reference R2。
pub fn encode(times: &[i64], values: &[f64]) -> Vec<u8> {
    assert_eq!(times.len(), values.len());
    let n = times.len();
    assert!(n > 0, "encode requires at least 1 point");

    let mut out = Vec::with_capacity(20 + n * 4);
    // header: first_ts(8B LE) + first_value(8B LE) + bitstream_len(4B LE)
    out.extend_from_slice(&times[0].to_le_bytes());
    out.extend_from_slice(&values[0].to_le_bytes());
    let bitstream_len_pos = out.len();
    out.extend_from_slice(&0u32.to_le_bytes()); // placeholder

    if n == 1 {
        return out; // bitstream_len=0
    }

    let mut bw = BitWriter::new();

    // 第 2 点：time_delta 作 unsigned varint
    let delta0 = (times[1] - times[0]) as u64; // 假设时间递增
    write_varint(&mut bw, delta0);
    // 第 2 点 value xor
    let prev_val_bits = values[0].to_bits();
    let mut prev_leading: u32 = 0xFF; // 哨兵
    let mut prev_trailing: u32 = 0;
    write_value_xor(
        &mut bw,
        values[1].to_bits(),
        prev_val_bits,
        &mut prev_leading,
        &mut prev_trailing,
    );
    let mut prev_delta = (times[1] - times[0]) as i64;

    // 第 3+ 点
    for i in 2..n {
        let delta = times[i] - times[i - 1];
        let dod = delta - prev_delta;
        write_dod(&mut bw, dod);
        write_value_xor(
            &mut bw,
            values[i].to_bits(),
            values[i - 1].to_bits(),
            &mut prev_leading,
            &mut prev_trailing,
        );
        prev_delta = delta;
    }

    let bitstream = bw.finish();
    out.extend_from_slice(&bitstream);
    // 回填 bitstream_len
    let len = bitstream.len() as u32;
    out[bitstream_len_pos..bitstream_len_pos + 4].copy_from_slice(&len.to_le_bytes());
    out
}

fn write_varint(bw: &mut BitWriter, mut value: u64) {
    // 7-bit groups, LSB first, MSB continuation
    // 注意：varint 是字节级编码，不是 bit 级。用 write_bits 按 8 bit 写。
    while value >= 0x80 {
        bw.write_bits((value & 0x7F) | 0x80, 8);
        value >>= 7;
    }
    bw.write_bits(value, 8);
}

fn write_dod(bw: &mut BitWriter, dod: i64) {
    // 见 V5 Format Reference R2 DoD 表
    if dod == 0 {
        bw.write_bit(false); // '0'
    } else if (-8191..=8191).contains(&dod) {
        bw.write_bits(0b10, 2);
        bw.write_bits((dod as u64) & 0x3FFF, 14); // 14 bit 有符号补码
    } else if (-65535..=65535).contains(&dod) {
        bw.write_bits(0b110, 3);
        bw.write_bits((dod as u64) & 0x1FFFF, 17);
    } else if (-524287..=524287).contains(&dod) {
        bw.write_bits(0b1110, 4);
        bw.write_bits((dod as u64) & 0xFFFFF, 20); // 20 bit 有符号补码
    } else {
        bw.write_bits(0b1111, 4);
        bw.write_bits(dod as u64, 64);
    }
}

fn write_value_xor(
    bw: &mut BitWriter,
    val_bits: u64,
    prev_bits: u64,
    prev_leading: &mut u32,
    prev_trailing: &mut u32,
) {
    let xor = val_bits ^ prev_bits;
    if xor == 0 {
        bw.write_bit(false); // '0'
        return;
    }
    bw.write_bit(true); // '1'
    let leading = xor.leading_zeros();
    let trailing = xor.trailing_zeros();
    let meaningful = 64 - leading - trailing;
    // 复用 prev leading/trailing 的条件
    if *prev_leading != 0xFF && leading >= *prev_leading && trailing >= *prev_trailing {
        bw.write_bit(false); // '0'
        // 写 prev window 大小的 bits（decoder 读 64 - prev_leading - prev_trailing 位）
        let prev_meaningful = 64 - *prev_leading - *prev_trailing;
        bw.write_bits(xor >> *prev_trailing, prev_meaningful);
    } else {
        bw.write_bit(true); // '1'
        bw.write_bits(leading as u64, 6);
        bw.write_bits((meaningful - 1) as u64, 6); // meaningful-1 装 6 bit
        bw.write_bits(xor >> trailing, meaningful);
        *prev_leading = leading;
        *prev_trailing = trailing;
    }
}

pub fn decode(bytes: &[u8], sample_count: usize) -> Result<(Vec<i64>, Vec<f64>), GorillaError> {
    if bytes.len() < 20 {
        return Err(GorillaError::InvalidBitstream);
    }
    let first_ts = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let first_val = f64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let bitstream_len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;

    let mut times = Vec::with_capacity(sample_count);
    let mut values = Vec::with_capacity(sample_count);
    times.push(first_ts);
    values.push(first_val);

    if sample_count <= 1 {
        return Ok((times, values));
    }

    if bytes.len() < 20 + bitstream_len {
        return Err(GorillaError::InvalidBitstream);
    }
    let bitstream = &bytes[20..20 + bitstream_len];
    let mut br = BitReader::new(bitstream);

    // 第 2 点：time_delta varint
    let delta0 = read_varint(&mut br);
    let t1 = first_ts + delta0 as i64;
    times.push(t1);
    let mut prev_leading: u32 = 0xFF;
    let mut prev_trailing: u32 = 0;
    let v1 = read_value_xor(&mut br, first_val.to_bits(), &mut prev_leading, &mut prev_trailing);
    values.push(f64::from_bits(v1));
    let mut prev_delta = delta0 as i64;

    // 第 3+ 点
    for _ in 2..sample_count {
        let dod = read_dod(&mut br);
        let delta = prev_delta + dod;
        let t = *times.last().unwrap() + delta;
        times.push(t);
        let prev_bits = values.last().unwrap().to_bits();
        let v = read_value_xor(&mut br, prev_bits, &mut prev_leading, &mut prev_trailing);
        values.push(f64::from_bits(v));
        prev_delta = delta;
    }

    Ok((times, values))
}

fn read_varint(br: &mut BitReader) -> u64 {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let byte = br.read_bits(8) as u8;
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    result
}

fn read_dod(br: &mut BitReader) -> i64 {
    // 见 V5 Format Reference R2 DoD 表
    if !br.read_bit() {
        // '0'
        return 0;
    }
    if !br.read_bit() {
        // '10'
        let v = br.read_bits(14) as i64;
        return sign_extend(v, 14);
    }
    if !br.read_bit() {
        // '110'
        let v = br.read_bits(17) as i64;
        return sign_extend(v, 17);
    }
    if !br.read_bit() {
        // '1110'
        let v = br.read_bits(20) as i64;
        return sign_extend(v, 20);
    }
    // '1111'
    br.read_bits(64) as i64
}

fn sign_extend(v: i64, n: u32) -> i64 {
    let sign_bit = 1i64 << (n - 1);
    if v & sign_bit != 0 {
        v - (1i64 << n)
    } else {
        v
    }
}

fn read_value_xor(
    br: &mut BitReader,
    prev_bits: u64,
    prev_leading: &mut u32,
    prev_trailing: &mut u32,
) -> u64 {
    if !br.read_bit() {
        // xor == 0
        return prev_bits;
    }
    if !br.read_bit() {
        // 复用 prev leading/trailing
        let meaningful = 64 - *prev_leading - *prev_trailing;
        let meaningful_bits = br.read_bits(meaningful);
        let xor = meaningful_bits << *prev_trailing;
        return prev_bits ^ xor;
    }
    // 新 leading/trailing
    let leading = br.read_bits(6) as u32;
    let meaningful = br.read_bits(6) as u32 + 1; // meaningful-1 存储，+1 还原
    let trailing = 64 - leading - meaningful;
    let meaningful_bits = br.read_bits(meaningful);
    let xor = meaningful_bits << trailing;
    *prev_leading = leading;
    *prev_trailing = trailing;
    prev_bits ^ xor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_point_fixture() {
        // V5 Format Reference R2 单点 fixture
        // 输入：times=[1000], values=[1.0]
        // 输出（20 字节）：
        //   1000 LE = E8 03 00 00 00 00 00 00
        //   1.0 LE  = 00 00 00 00 00 00 F0 3F
        //   len=0 LE= 00 00 00 00
        let times = vec![1000i64];
        let values = vec![1.0f64];
        let encoded = encode(&times, &values);
        let expected: Vec<u8> = vec![
            0xE8, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xF0, 0x3F, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(encoded, expected, "single point fixture mismatch");

        let (t, v) = decode(&encoded, 1).unwrap();
        assert_eq!(t, vec![1000]);
        assert!((v[0] - 1.0).abs() < 1e-15);
    }

    #[test]
    fn test_roundtrip_multipoint() {
        let times = vec![1000i64, 2000, 3000, 4000, 5000];
        let values = vec![1.0f64, 2.0, 3.0, 3.0, 5.0]; // 含 xor==0 的点（第3→4点值相同）
        let encoded = encode(&times, &values);
        let (t, v) = decode(&encoded, times.len()).unwrap();
        assert_eq!(t, times);
        for (a, b) in v.iter().zip(values.iter()) {
            assert!((a - b).abs() < 1e-15, "value mismatch: {} vs {}", a, b);
        }
    }

    #[test]
    fn test_roundtrip_variable_interval() {
        let times = vec![1000i64, 2000, 4000, 8000, 16000]; // delta 翻倍，dod 非零
        let values = vec![1.0f64, 1.5, 2.5, 4.5, 8.5];
        let encoded = encode(&times, &values);
        let (t, v) = decode(&encoded, times.len()).unwrap();
        assert_eq!(t, times);
        for (a, b) in v.iter().zip(values.iter()) {
            assert!((a - b).abs() < 1e-15);
        }
    }

    #[test]
    fn test_roundtrip_large_dod() {
        // dod 超出 524287，走 64-bit 分支
        let times = vec![0i64, 1000, 1_000_000]; // delta0=1000, delta1=999000, dod=998000
        let values = vec![1.0f64, 2.0, 3.0];
        let encoded = encode(&times, &values);
        let (t, v) = decode(&encoded, times.len()).unwrap();
        assert_eq!(t, times);
        for (a, b) in v.iter().zip(values.iter()) {
            assert!((a - b).abs() < 1e-15);
        }
    }

    #[test]
    fn test_roundtrip_negative_dod() {
        // Regression: negative dod in every width branch must sign-extend
        // back. delta0 = 1000 (varint); the DoD sequence for points 2..=10 is
        //   0, -2000, +500, -50500, +60000, -100000, +500000, -900000, +10_450_000
        // covering: '0', 14b (neg+pos), 17b (neg+pos), 20b (neg+pos — the
        // negative-20b path was previously corrupted by a 19-bit mask), and
        // 64b (neg+pos).
        let times = vec![
            1_000_000i64,
            1_001_000,  // delta 1000
            1_002_000,  // dod 0
            1_001_000,  // dod -2000   (14b)
            1_000_500,  // dod +500    (14b)
            949_500,    // dod -50500  (17b)
            958_500,    // dod +60000  (17b)
            867_500,    // dod -100000 (20b)
            1_276_500,  // dod +500000 (20b)
            785_500,    // dod -900000 (64b)
            10_744_500, // dod +10_450_000 (64b)
        ];
        let values = vec![1.0f64; times.len()];
        let encoded = encode(&times, &values);
        let (t, v) = decode(&encoded, times.len()).unwrap();
        assert_eq!(t, times);
        for (a, b) in v.iter().zip(values.iter()) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    #[test]
    fn test_roundtrip_xor_reuse() {
        // Values crafted so consecutive XORs have increasing leading zeros,
        // triggering the reuse branch ('10') in write_value_xor.
        let times = vec![0i64, 1000, 2000, 3000, 4000];
        let values = vec![
            0.0f64,
            1.0,
            1.0 + 1e-300,
            1.0 + 1e-300 + 1e-310,
            1.0 + 1e-300 + 1e-310 + 1e-315,
        ];
        let encoded = encode(&times, &values);
        let (t, v) = decode(&encoded, times.len()).unwrap();
        assert_eq!(t, times);
        for (a, b) in v.iter().zip(values.iter()) {
            assert_eq!(a.to_bits(), b.to_bits(), "value mismatch: {a} vs {b}");
        }
    }
}
