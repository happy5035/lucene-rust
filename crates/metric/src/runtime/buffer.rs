use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tokio::task::JoinHandle;
use rustlucene_core::index_writer::{IndexWriter, IndexWriterConfig};
use rustlucene_core::schema::Schema;
use xxhash_rust::xxh64::xxh64;

use crate::algo::series_hash::series_hash;
use crate::store::schema::v5_schema;
use crate::store::series_writer::write_series;

struct SeriesEntry {
    name: String,
    sorted_labels: Vec<(String, String)>,
    times: Vec<i64>,
    values: Vec<f64>,
}

/// Buffer 刷盘配置
pub struct BufferConfig {
    pub max_series: usize,
    pub max_points_per_series: usize,
    pub max_total_points: usize,
    pub max_age_ms: u64,
    pub tick_interval_ms: u64,
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            max_series: 10_000,
            max_points_per_series: 10_000,
            max_total_points: 1_000_000,
            max_age_ms: 60_000,
            tick_interval_ms: 1_000,
        }
    }
}

/// SeriesBuffer：内存缓冲 series 写入，显式 flush 到 IndexWriter。
pub struct SeriesBuffer {
    buffer: RwLock<HashMap<u64, SeriesEntry>>,
    writer: parking_lot::Mutex<IndexWriter>,
}

impl SeriesBuffer {
    pub fn create(path: &std::path::Path, schema: Schema, config: IndexWriterConfig) -> io::Result<Self> {
        let writer = IndexWriter::create(path, schema, config)?;
        Ok(Self {
            buffer: RwLock::new(HashMap::new()),
            writer: parking_lot::Mutex::new(writer),
        })
    }

    /// 用 V5 schema 创建。
    pub fn create_v5(path: &std::path::Path, config: IndexWriterConfig) -> io::Result<Self> {
        Self::create(path, v5_schema(), config)
    }

    /// 高层 API：接收结构化 labels（需已按 key 排序），内部计算 series_hash。
    pub fn write_point(&self, name: &str, sorted_labels: &[(String, String)], time: i64, value: f64) {
        let hash = series_hash(name, sorted_labels);
        let mut buf = self.buffer.write();
        let entry = buf.entry(hash).or_insert_with(|| SeriesEntry {
            name: name.to_string(),
            sorted_labels: sorted_labels.to_vec(),
            times: Vec::new(),
            values: Vec::new(),
        });
        entry.times.push(time);
        entry.values.push(value);
    }

    /// 低层 API：接收已格式化的 labelsStr（JNI 层调用）。
    pub fn write_point_with_labels_str(&self, name: &str, labels_str: &str, time: i64, value: f64) {
        // 从 labelsStr 直接计算 hash（与 series_hash 逻辑一致，但跳过 build_labels_str）
        let name_hash = xxh64(&encode_utf16_le(name), 0);
        let label_hash = xxh64(&encode_utf16_le(labels_str), 0);
        let hash = ((name_hash as u32 as u64) << 32) | (label_hash as u32 as u64);

        let mut buf = self.buffer.write();
        // labels_str 反解析为 sorted_labels
        let sorted_labels = parse_labels_str(labels_str);
        let entry = buf.entry(hash).or_insert_with(|| SeriesEntry {
            name: name.to_string(),
            sorted_labels,
            times: Vec::new(),
            values: Vec::new(),
        });
        entry.times.push(time);
        entry.values.push(value);
    }

    /// 显式 flush：把所有缓冲的 series 写入 IndexWriter。
    pub fn flush(&self) -> io::Result<()> {
        let mut buf = self.buffer.write();
        let entries: Vec<SeriesEntry> = buf.drain().map(|(_, v)| v).collect();
        drop(buf);

        let mut writer = self.writer.lock();
        for entry in entries {
            write_series(&mut writer, &entry.name, &entry.sorted_labels, &entry.times, &entry.values)?;
        }
        Ok(())
    }

    pub fn commit(&self) -> io::Result<()> {
        self.flush()?;
        let mut writer = self.writer.lock();
        writer.commit()
    }

    /// 启动 tokio task 定时检查阈值并 flush。
    /// 返回 JoinHandle，调用方 abort() 停止。
    pub fn start_auto_flush(self: &Arc<Self>, config: BufferConfig) -> JoinHandle<()> {
        let buf = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(config.tick_interval_ms));
            loop {
                interval.tick().await;
                let should_flush = {
                    let b = buf.buffer.read();
                    if b.len() > config.max_series {
                        true
                    } else if b.values().map(|e| e.times.len()).sum::<usize>() > config.max_total_points {
                        true
                    } else {
                        false
                    }
                };
                if should_flush {
                    let _ = buf.flush();
                }
            }
        })
    }
}

fn encode_utf16_le(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2);
    for unit in s.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

fn parse_labels_str(s: &str) -> Vec<(String, String)> {
    // "$#$k1=v1$#$k2=v2$#$" → [(k1,v1), (k2,v2)]
    let parts: Vec<&str> = s.split("$#$").filter(|p| !p.is_empty()).collect();
    parts.iter().filter_map(|p| {
        let mut it = p.splitn(2, '=');
        let k = it.next()?.to_string();
        let v = it.next()?.to_string();
        Some((k, v))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codec_lucene9::FSDirectory;
    use rustlucene_core::search::reader::Reader;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("rustlucene-buf-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn test_buffer_flush_roundtrip() {
        let root = temp_dir("flush");
        let buf = SeriesBuffer::create_v5(&root, IndexWriterConfig::default()).unwrap();

        // 写入两个 series 的多个点
        let labels1 = vec![("host".to_string(), "h1".to_string())];
        let labels2 = vec![("host".to_string(), "h2".to_string())];

        buf.write_point("cpu.usage", &labels1, 1000, 1.0);
        buf.write_point("cpu.usage", &labels1, 2000, 2.0);
        buf.write_point("cpu.usage", &labels2, 1000, 10.0);
        buf.write_point("cpu.usage", &labels2, 2000, 20.0);

        buf.commit().unwrap();
        drop(buf);

        // 读回验证：应有 2 个 doc（2 个 series）
        let dir = FSDirectory::open(&root).unwrap();
        let mut reader = Reader::open(&dir).unwrap();
        let mut total_docs = 0;
        for (_base, seg) in reader.leaves() {
            total_docs += seg.max_doc() as usize;
        }
        assert_eq!(total_docs, 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_buffer_write_point_with_labels_str() {
        let root = temp_dir("labels_str");
        let buf = SeriesBuffer::create_v5(&root, IndexWriterConfig::default()).unwrap();

        buf.write_point_with_labels_str("cpu.usage", "$#$host=h1$#$", 1000, 1.0);
        buf.write_point_with_labels_str("cpu.usage", "$#$host=h1$#$", 2000, 2.0);

        buf.commit().unwrap();
        drop(buf);

        let dir = FSDirectory::open(&root).unwrap();
        let mut reader = Reader::open(&dir).unwrap();
        let mut total_docs = 0;
        for (_base, seg) in reader.leaves() {
            let bins = seg.binary_values("gorilla_data").unwrap();
            assert_eq!(bins.len(), 1); // 1 个 series
            let (t, _v) = crate::algo::gorilla::decode(&bins[0].1, 2).unwrap();
            assert_eq!(t, vec![1000, 2000]);
            total_docs += 1;
        }
        assert_eq!(total_docs, 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_auto_flush_triggers() {
        use std::time::Duration;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let root = temp_dir("auto_flush");
            let buf = std::sync::Arc::new(
                SeriesBuffer::create_v5(&root, IndexWriterConfig::default()).unwrap()
            );

            // 配置：1 个 series 即触发刷盘
            let config = BufferConfig {
                max_series: 1,
                max_points_per_series: 100,
                max_total_points: 1000,
                max_age_ms: 60_000,
                tick_interval_ms: 50,
            };
            let buf2 = buf.clone();
            let handle = buf2.start_auto_flush(config);

            // 写入 2 个 series（超过 max_series=1）
            buf.write_point("m1", &[("k".into(), "v1".into())], 1000, 1.0);
            buf.write_point("m2", &[("k".into(), "v2".into())], 1000, 2.0);

            // 等待 tick
            tokio::time::sleep(Duration::from_millis(200)).await;

            // 验证 buffer 已被刷空
            {
                let b = buf.buffer.read();
                assert_eq!(b.len(), 0, "buffer should be flushed");
            }

            handle.abort();
            drop(buf);
            let _ = std::fs::remove_dir_all(&root);
        });
    }
}
