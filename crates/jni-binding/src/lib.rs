//! JNI bindings exposing the RustLucene write path to Java.
//!
//! Java side: `interop/java/RustIndexWriter.java`. The native handle is a
//! boxed [`WriterHandle`] behind a `Mutex` so multi-threaded Java callers get
//! serialized access (a single IndexWriter is single-threaded by design; use
//! one writer per thread/shard for parallel ingestion).
//!
//! Schema spec string: comma-separated `name:type[+modifier...]` entries.
//! Types: `text`, `keyword`, `longpoint`, `intpoint`, `numericdv`,
//! `sorteddv`, `stored`. Modifiers: `positions` (text), `stored` (point/DV
//! fields), `numericdv` (longpoint/intpoint), `sorteddv` (keyword).
//! Example:
//! `timestamp:longpoint+numericdv+stored,level:keyword+sorteddv,message:text+positions,latency_ms:numericdv`

use std::path::Path;
use std::sync::Mutex;

use jni::objects::{JClass, JString};
use jni::sys::{jint, jlong};
use jni::JNIEnv;
use rustlucene_core::{Document, FieldSpec, FieldValue, IndexWriter, IndexWriterConfig, Schema};

struct WriterHandle {
    writer: IndexWriter,
    current: Option<Document>,
}

fn parse_schema(spec: &str) -> Result<Schema, String> {
    let mut schema = Schema::new();
    for entry in spec.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut parts = entry.split(':');
        let name = parts.next().ok_or("missing field name")?.trim();
        let mut ty = parts.next().ok_or("missing field type")?.trim();
        let mut modifiers = "";
        if let Some((t, m)) = ty.split_once('+') {
            ty = t.trim();
            modifiers = m;
        }
        let has = |m: &str| modifiers.split('+').any(|x| x.trim() == m);
        let spec = match ty {
            "text" => {
                if has("positions") {
                    FieldSpec::text_with_positions(name)
                } else {
                    FieldSpec::text(name)
                }
            }
            "keyword" => {
                let mut s = FieldSpec::keyword(name);
                if has("sorteddv") {
                    s = s.with_sorted_dv();
                }
                s
            }
            "longpoint" => {
                let mut s = FieldSpec::long_point(name);
                if has("numericdv") {
                    s = s.with_numeric_dv();
                }
                s.with_stored(has("stored"))
            }
            "intpoint" => {
                let mut s = FieldSpec::int_point(name);
                if has("numericdv") {
                    s = s.with_numeric_dv();
                }
                s.with_stored(has("stored"))
            }
            "numericdv" => FieldSpec::numeric_dv(name).with_stored(has("stored")),
            "sorteddv" => FieldSpec::sorted_dv(name).with_stored(has("stored")),
            "stored" => FieldSpec::stored(name),
            other => return Err(format!("unknown field type: {other}")),
        };
        schema.add(spec);
    }
    Ok(schema)
}

fn handle<'a>(ptr: jlong) -> Result<&'a Mutex<WriterHandle>, String> {
    if ptr == 0 {
        return Err("null writer handle".into());
    }
    Ok(unsafe { &*(ptr as *const Mutex<WriterHandle>) })
}

fn throw(env: &mut JNIEnv, class: &str, msg: &str) {
    let _ = env.throw_new(class, msg);
}

macro_rules! jni_try {
    ($env:expr, $e:expr) => {
        match $e {
            Ok(v) => v,
            Err(msg) => {
                throw($env, "java/io/IOException", &msg.to_string());
                return 0.into();
            }
        }
    };
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeCreate(
    mut env: JNIEnv,
    _class: JClass,
    path: JString,
    schema_spec: JString,
) -> jlong {
    let path: String = jni_try!(&mut env, env.get_string(&path).map(|s| s.to_string_lossy().into_owned()));
    let spec: String = jni_try!(&mut env, env.get_string(&schema_spec).map(|s| s.to_string_lossy().into_owned()));
    let schema = jni_try!(&mut env, parse_schema(&spec));
    let writer = jni_try!(
        &mut env,
        IndexWriter::create(Path::new(&path), schema, IndexWriterConfig::default())
    );
    let handle = Box::new(Mutex::new(WriterHandle {
        writer,
        current: None,
    }));
    Box::into_raw(handle) as jlong
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeBeginDocument(mut env: JNIEnv, _class: JClass, ptr: jlong) -> jlong {
    let h = jni_try!(&mut env, handle(ptr));
    let mut g = jni_try!(&mut env, h.lock().map_err(|_| "poisoned lock".to_string()));
    g.current = Some(Document::new());
    0
}

fn add_field(env: &mut JNIEnv, ptr: jlong, field: JString, value: FieldValue) -> jlong {
    let name: String = jni_try!(env, env.get_string(&field).map(|s| s.to_string_lossy().into_owned()));
    let h = jni_try!(env, handle(ptr));
    let mut g = jni_try!(env, h.lock().map_err(|_| "poisoned lock".to_string()));
    match g.current.as_mut() {
        Some(doc) => {
            doc.add(&name, value);
            0
        }
        None => {
            throw(env, "java/lang/IllegalStateException", "beginDocument not called");
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeAddText(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
    field: JString,
    value: JString,
) -> jlong {
    let v: String = jni_try!(&mut env, env.get_string(&value).map(|s| s.to_string_lossy().into_owned()));
    add_field(&mut env, ptr, field, FieldValue::Text(v))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeAddKeyword(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
    field: JString,
    value: JString,
) -> jlong {
    let v: String = jni_try!(&mut env, env.get_string(&value).map(|s| s.to_string_lossy().into_owned()));
    add_field(&mut env, ptr, field, FieldValue::Keyword(v))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeAddLong(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
    field: JString,
    value: jlong,
) -> jlong {
    add_field(&mut env, ptr, field, FieldValue::Long(value))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeAddInt(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
    field: JString,
    value: jint,
) -> jlong {
    add_field(&mut env, ptr, field, FieldValue::Int(value))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeEndDocument(mut env: JNIEnv, _class: JClass, ptr: jlong) -> jlong {
    let h = jni_try!(&mut env, handle(ptr));
    let mut g = jni_try!(&mut env, h.lock().map_err(|_| "poisoned lock".to_string()));
    match g.current.take() {
        Some(doc) => {
            jni_try!(&mut env, g.writer.add_document(doc).map_err(|e| e.to_string()));
            0
        }
        None => {
            throw(&mut env, "java/lang/IllegalStateException", "beginDocument not called");
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeFlush(mut env: JNIEnv, _class: JClass, ptr: jlong) -> jlong {
    let h = jni_try!(&mut env, handle(ptr));
    let mut g = jni_try!(&mut env, h.lock().map_err(|_| "poisoned lock".to_string()));
    jni_try!(&mut env, g.writer.flush().map_err(|e| e.to_string()));
    0
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeCommit(mut env: JNIEnv, _class: JClass, ptr: jlong) -> jlong {
    let h = jni_try!(&mut env, handle(ptr));
    let mut g = jni_try!(&mut env, h.lock().map_err(|_| "poisoned lock".to_string()));
    jni_try!(&mut env, g.writer.commit().map_err(|e| e.to_string()));
    0
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeClose(mut env: JNIEnv, _class: JClass, ptr: jlong) -> jlong {
    if ptr == 0 {
        return 0;
    }
    let handle = unsafe { Box::from_raw(ptr as *mut Mutex<WriterHandle>) };
    if let Ok(mut g) = handle.lock() {
        if g.current.is_some() {
            throw(&mut env, "java/lang/IllegalStateException", "uncommitted document discarded");
        }
        g.current = None;
    }
    drop(handle);
    0
}

#[cfg(test)]
mod tests {
    use super::parse_schema;

    #[test]
    fn parses_log_schema() {
        let s = parse_schema(
            "timestamp:longpoint+numericdv+stored,level:keyword+sorteddv,trace_id:keyword,\
             message:text+positions,latency_ms:numericdv",
        )
        .unwrap();
        assert_eq!(s.fields().len(), 5);
        assert!(s.get("timestamp").unwrap().points.is_some());
        assert!(s.get("timestamp").unwrap().stored);
        assert!(s.get("level").unwrap().is_indexed());
        assert!(s.get("message").unwrap().has_positions());
        assert!(!s.get("latency_ms").unwrap().is_indexed());
    }
}
