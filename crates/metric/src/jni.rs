//! JNI bindings exposing the metric storage write/read/downsample/merge path to Java.
//!
//! Java side: `com.metric.RustMetric`. The native handle is a boxed
//! [`SeriesBuffer`] cast to `jlong` — same pattern as `crates/jni-binding`.

#![cfg(feature = "jni")]

use std::path::Path;

use jni::objects::{JClass, JObjectArray, JString};
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
