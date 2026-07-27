//! JNI bindings exposing the metric storage write/read/downsample/merge path to Java.
//!
//! Java side: `com.metric.RustMetric`. The native handle is a boxed
//! [`SeriesBuffer`] cast to `jlong` — same pattern as `crates/jni-binding`.

#![cfg(feature = "jni")]

use std::path::Path;

use jni::objects::{JClass, JDoubleArray, JLongArray, JObjectArray, JString, ReleaseMode};
use jni::sys::{jboolean, jdouble, jlong};
use jni::JNIEnv;

use rustlucene_core::index_writer::IndexWriterConfig;

use crate::runtime::buffer::SeriesBuffer;
use crate::runtime::downsample_op::downsample_shard;
use crate::runtime::merge_op::{merge_cross_shard, merge_intra_shard};

/// Open a metric writer for the given shard directory.
/// Returns a native handle (non-zero) on success, 0 on failure.
#[no_mangle]
pub extern "system" fn Java_com_metric_RustMetric_openMetricWriter(
    mut env: JNIEnv,
    _class: JClass,
    shard_dir: JString,
) -> jlong {
    let dir: String = match env.get_string(&shard_dir) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let path = Path::new(&dir);
    match SeriesBuffer::create_v5(path, IndexWriterConfig::default()) {
        Ok(buf) => Box::into_raw(Box::new(buf)) as jlong,
        Err(_) => 0,
    }
}

/// Write a single data point into the buffer.
/// `labels` is in "$#$k1=v1$#$k2=v2$#$" format.
/// Returns 1 on success, 0 if handle is null.
#[no_mangle]
pub extern "system" fn Java_com_metric_RustMetric_writePoint(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    name: JString,
    labels: JString,
    time: jlong,
    value: jdouble,
) -> jboolean {
    if handle == 0 {
        return 0;
    }
    let buf = unsafe { &*(handle as *const SeriesBuffer) };
    let name: String = match env.get_string(&name) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let labels: String = match env.get_string(&labels) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    buf.write_point_with_labels_str(&name, &labels, time, value);
    1
}

/// Batch write: same series, multiple points.
/// Reduces JNI crossings from N to 1 for a series with N points.
/// `labels` is in "$#$k1=v1$#$k2=v2$#$" format.
/// Returns 1 on success, 0 on failure.
#[no_mangle]
pub extern "system" fn Java_com_metric_RustMetric_writePoints(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    name: JString,
    labels: JString,
    times: JLongArray,
    values: JDoubleArray,
) -> jboolean {
    if handle == 0 {
        return 0;
    }
    let buf = unsafe { &*(handle as *const SeriesBuffer) };
    let name: String = match env.get_string(&name) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let labels: String = match env.get_string(&labels) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };

    let times_len = match env.get_array_length(&times) {
        Ok(l) => l as usize,
        Err(_) => return 0,
    };
    let values_len = match env.get_array_length(&values) {
        Ok(l) => l as usize,
        Err(_) => return 0,
    };
    if times_len != values_len || times_len == 0 {
        return 0;
    }

    let times_elems = match unsafe { env.get_array_elements(&times, ReleaseMode::NoCopyBack) } {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let values_elems = match unsafe { env.get_array_elements(&values, ReleaseMode::NoCopyBack) } {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let times_slice = unsafe { std::slice::from_raw_parts(times_elems.as_ptr(), times_elems.len()) };
    let values_slice = unsafe { std::slice::from_raw_parts(values_elems.as_ptr(), values_elems.len()) };

    buf.write_points_with_labels_str(&name, &labels, times_slice, values_slice);
    1
}

/// Batch write: multiple series, one point each (fan-out pattern).
/// names[i], labels[i], times[i], values[i] form one point.
/// Returns 1 on success, 0 on failure.
#[no_mangle]
pub extern "system" fn Java_com_metric_RustMetric_writePointsMulti(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    names: JObjectArray,
    labels: JObjectArray,
    times: JLongArray,
    values: JDoubleArray,
) -> jboolean {
    if handle == 0 {
        return 0;
    }
    let buf = unsafe { &*(handle as *const SeriesBuffer) };

    let count = match env.get_array_length(&times) {
        Ok(l) => l as usize,
        Err(_) => return 0,
    };
    if count == 0 {
        return 0;
    }

    let times_elems = match unsafe { env.get_array_elements(&times, ReleaseMode::NoCopyBack) } {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let values_elems = match unsafe { env.get_array_elements(&values, ReleaseMode::NoCopyBack) } {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let times_slice = unsafe { std::slice::from_raw_parts(times_elems.as_ptr(), times_elems.len()) };
    let values_slice = unsafe { std::slice::from_raw_parts(values_elems.as_ptr(), values_elems.len()) };

    for i in 0..count {
        let name_obj = match env.get_object_array_element(&names, i as i32) {
            Ok(o) => o,
            Err(_) => continue,
        };
        let labels_obj = match env.get_object_array_element(&labels, i as i32) {
            Ok(o) => o,
            Err(_) => continue,
        };
        let name: String = match env.get_string(&JString::from(name_obj)) {
            Ok(s) => s.into(),
            Err(_) => continue,
        };
        let labels: String = match env.get_string(&JString::from(labels_obj)) {
            Ok(s) => s.into(),
            Err(_) => continue,
        };
        buf.write_point_with_labels_str(&name, &labels, times_slice[i], values_slice[i]);
    }
    1
}

/// Binary-packed batch write: multiple series, one point each.
/// One JNI crossing for N points. Java packs all points into a single byte[].
///
/// Binary protocol per point:
///   name_len: u16 LE (2 bytes)
///   name: UTF-8 bytes (name_len bytes)
///   labels_len: u16 LE (2 bytes)
///   labels: UTF-8 bytes (labels_len bytes)
///   time: i64 LE (8 bytes)
///   value: f64 LE (8 bytes)
///
/// Total per point: 2 + name_len + 2 + labels_len + 8 + 8 = 20 + name_len + labels_len
///
/// All points are parsed first, then buffered via a single lock acquisition
/// ([`SeriesBuffer::write_batch_raw`]). Returns number of points successfully
/// buffered, or -1 on error.
#[no_mangle]
pub extern "system" fn Java_com_metric_RustMetric_writeBatch(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    data: jni::objects::JByteArray,
) -> jlong {
    if handle == 0 {
        return -1;
    }
    let buf = unsafe { &*(handle as *const SeriesBuffer) };

    // One copy from JVM heap to Rust
    let bytes = match env.convert_byte_array(&data) {
        Ok(b) => b,
        Err(_) => return -1,
    };

    // Parse all points first (no lock held during parsing)
    let mut points: Vec<(String, String, i64, f64)> = Vec::new();
    let mut pos = 0;
    while pos + 4 <= bytes.len() {
        // name
        let name_len = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;
        if pos + name_len > bytes.len() {
            break;
        }
        let name = std::str::from_utf8(&bytes[pos..pos + name_len]).unwrap_or("");
        pos += name_len;

        // labels
        if pos + 2 > bytes.len() {
            break;
        }
        let labels_len = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;
        if pos + labels_len > bytes.len() {
            break;
        }
        let labels = std::str::from_utf8(&bytes[pos..pos + labels_len]).unwrap_or("");
        pos += labels_len;

        // time + value
        if pos + 16 > bytes.len() {
            break;
        }
        let time = i64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let value = f64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        pos += 8;

        points.push((name.to_string(), labels.to_string(), time, value));
    }

    let count = points.len() as jlong;
    // Single lock acquisition for all points
    buf.write_batch_raw(&points);
    count
}

/// Flush buffered data and commit to disk.
/// Returns 1 on success, 0 on failure or null handle.
#[no_mangle]
pub extern "system" fn Java_com_metric_RustMetric_flushBuffer(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jboolean {
    if handle == 0 {
        return 0;
    }
    let buf = unsafe { &*(handle as *const SeriesBuffer) };
    match buf.commit() {
        Ok(()) => 1,
        Err(_) => 0,
    }
}

/// Close the metric writer: best-effort final commit, then free memory.
#[no_mangle]
pub extern "system" fn Java_com_metric_RustMetric_closeMetricWriter(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    unsafe {
        let buf = Box::from_raw(handle as *mut SeriesBuffer);
        let _ = buf.commit(); // best-effort final flush
    }
}

/// Downsample a raw shard into a downsample shard at the given granularity.
/// Returns 1 on success, 0 on failure.
#[no_mangle]
pub extern "system" fn Java_com_metric_RustMetric_downsample(
    mut env: JNIEnv,
    _class: JClass,
    input_shard_dir: JString,
    output_shard_dir: JString,
    granularity_ms: jlong,
) -> jboolean {
    let input: String = match env.get_string(&input_shard_dir) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let output: String = match env.get_string(&output_shard_dir) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    match downsample_shard(Path::new(&input), Path::new(&output), granularity_ms) {
        Ok(_) => 1,
        Err(_) => 0,
    }
}

/// Merge same-series docs within a single shard.
/// Returns 1 on success, 0 on failure.
#[no_mangle]
pub extern "system" fn Java_com_metric_RustMetric_mergeIntraShard(
    mut env: JNIEnv,
    _class: JClass,
    input_shard_dir: JString,
    output_shard_dir: JString,
) -> jboolean {
    let input: String = match env.get_string(&input_shard_dir) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let output: String = match env.get_string(&output_shard_dir) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    match merge_intra_shard(Path::new(&input), Path::new(&output)) {
        Ok(_) => 1,
        Err(_) => 0,
    }
}

/// K-way merge multiple shards into one compact shard + 5m/1h downsample shards.
/// Returns 1 on success, 0 on failure.
#[no_mangle]
pub extern "system" fn Java_com_metric_RustMetric_mergeCrossShard(
    mut env: JNIEnv,
    _class: JClass,
    input_shard_dirs: JObjectArray,
    output_compact_dir: JString,
    output_downsample_5m_dir: JString,
    output_downsample_1h_dir: JString,
) -> jboolean {
    let compact: String = match env.get_string(&output_compact_dir) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let ds5m: String = match env.get_string(&output_downsample_5m_dir) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let ds1h: String = match env.get_string(&output_downsample_1h_dir) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };

    let len = match env.get_array_length(&input_shard_dirs) {
        Ok(l) => l,
        Err(_) => return 0,
    };
    let mut dirs: Vec<String> = Vec::with_capacity(len as usize);
    for i in 0..len {
        let obj = match env.get_object_array_element(&input_shard_dirs, i) {
            Ok(o) => o,
            Err(_) => return 0,
        };
        let s: String = match env.get_string(&JString::from(obj)) {
            Ok(s) => s.into(),
            Err(_) => return 0,
        };
        dirs.push(s);
    }
    let dir_refs: Vec<&Path> = dirs.iter().map(|s| Path::new(s.as_str())).collect();

    match merge_cross_shard(
        &dir_refs,
        Path::new(&compact),
        Path::new(&ds5m),
        Path::new(&ds1h),
    ) {
        Ok(_) => 1,
        Err(_) => 0,
    }
}
