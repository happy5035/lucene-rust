use xxhash_rust::xxh64::xxh64;

/// 计算 series_hash。见 V5 Format Reference R1。
/// name 和 labelsStr 各自 xxhash64(seed=0) 取低 32 位，nameHash 高 32 位，labelHash 低 32 位。
pub fn series_hash(name: &str, sorted_labels: &[(String, String)]) -> u64 {
    let labels_str = build_labels_str(sorted_labels);
    let name_hash = xxh64(&encode_utf16_le(name), 0);
    let label_hash = xxh64(&encode_utf16_le(&labels_str), 0);
    // 取低 32 位拼接：(nameHash32 << 32) | labelHash32
    let name_lo = name_hash as u32 as u64;
    let label_lo = label_hash as u32 as u64;
    (name_lo << 32) | label_lo
}

/// 构造 labelsStr：$#$k1=v1$#$k2=v2$#$（首尾各有 $#$）
pub fn build_labels_str(sorted_labels: &[(String, String)]) -> String {
    if sorted_labels.is_empty() {
        return String::from("$#$");
    }
    let mut s = String::from("$#$");
    for (i, (k, v)) in sorted_labels.iter().enumerate() {
        if i > 0 { s.push_str("$#$"); }
        s.push_str(k);
        s.push('=');
        s.push_str(v);
    }
    s.push_str("$#$");
    s
}

/// 将 &str 编码为 UTF-16 little-endian 字节流（与 Java String.chars() 对齐）
fn encode_utf16_le(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2);
    for unit in s.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_labels_str() {
        let labels = vec![
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ];
        assert_eq!(build_labels_str(&labels), "$#$a=1$#$b=2$#$");
    }

    #[test]
    fn test_build_labels_str_empty() {
        assert_eq!(build_labels_str(&[]), "$#$");
    }

    #[test]
    fn test_series_hash_deterministic() {
        let labels = vec![("host".to_string(), "h1".to_string())];
        let h1 = series_hash("cpu.usage", &labels);
        let h2 = series_hash("cpu.usage", &labels);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_series_hash_different_name() {
        let labels = vec![("host".to_string(), "h1".to_string())];
        let h1 = series_hash("cpu.usage", &labels);
        let h2 = series_hash("mem.usage", &labels);
        assert_ne!(h1, h2); // name 不同，高 32 位不同
    }

    #[test]
    fn test_series_hash_different_labels() {
        let l1 = vec![("host".to_string(), "h1".to_string())];
        let l2 = vec![("host".to_string(), "h2".to_string())];
        let h1 = series_hash("cpu.usage", &l1);
        let h2 = series_hash("cpu.usage", &l2);
        assert_ne!(h1, h2); // labels 不同，低 32 位不同
    }

    #[test]
    fn test_utf16_le_encoding_ascii() {
        // ASCII 字符 UTF-16 LE 与 UTF-8 不同（UTF-16 有高位 0x00）
        let bytes = encode_utf16_le("AB");
        assert_eq!(bytes, vec![0x41, 0x00, 0x42, 0x00]); // 'A'=0x0041 LE, 'B'=0x0042 LE
    }

    #[test]
    fn test_utf16_le_encoding_nonascii() {
        // 中文 "中" U+4E2D，UTF-16 LE = 2D 4E
        let bytes = encode_utf16_le("中");
        assert_eq!(bytes, vec![0x2D, 0x4E]);
    }
}
