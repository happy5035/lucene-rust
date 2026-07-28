//! JNI bindings exposing the RustLucene write path to Java.
//!
//! Java side: `interop/java/RustIndexWriter.java`. The native handle is a
//! boxed [`WriterHandle`] behind a `Mutex` so multi-threaded Java callers get
//! serialized access (a single IndexWriter is single-threaded by design; use
//! one writer per thread/shard for parallel ingestion).
//!
//! Schema spec string: comma-separated `name:type[+modifier...]` entries,
//! each optionally suffixed with `@json键` (bind a differently named JSON
//! key), plus a `$policy=strict|dynamic|stored-only` directive for unknown
//! JSON fields. Types: `text`, `keyword`, `longpoint`, `intpoint`,
//! `numericdv`, `sorteddv`, `stored`. Modifiers: `positions` (text),
//! `stored` (point/DV fields), `numericdv` (longpoint/intpoint), `sorteddv`
//! (keyword). The parser lives in `rustlucene_core::Schema::parse` so the
//! CLI and this bridge share one syntax.
//! Example:
//! `timestamp:longpoint+numericdv+stored,level:keyword+sorteddv,message:text+positions,latency_ms:numericdv`

mod query_parser;

use std::path::Path;
use std::sync::Mutex;

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObjectArray, JString};
use jni::sys::{jint, jlong};
use rustlucene_core::{
    BindOutcome, Document, FieldValue, IndexWriter, IndexWriterConfig, JsonBinder, Schema,
};

struct WriterHandle {
    writer: IndexWriter,
    current: Option<Document>,
    binder: JsonBinder,
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
    let path: String = jni_try!(
        &mut env,
        env.get_string(&path)
            .map(|s| s.to_string_lossy().into_owned())
    );
    let spec: String = jni_try!(
        &mut env,
        env.get_string(&schema_spec)
            .map(|s| s.to_string_lossy().into_owned())
    );
    let (schema, aliases, policy) = jni_try!(&mut env, Schema::parse(&spec));
    let binder = JsonBinder::new(&schema, &aliases, policy);
    let writer = jni_try!(
        &mut env,
        IndexWriter::create(Path::new(&path), schema, IndexWriterConfig::default())
    );
    let handle = Box::new(Mutex::new(WriterHandle {
        writer,
        current: None,
        binder,
    }));
    Box::into_raw(handle) as jlong
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeBeginDocument(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) -> jlong {
    let h = jni_try!(&mut env, handle(ptr));
    let mut g = jni_try!(&mut env, h.lock().map_err(|_| "poisoned lock".to_string()));
    g.current = Some(Document::new());
    0
}

fn add_field(env: &mut JNIEnv, ptr: jlong, field: JString, value: FieldValue) -> jlong {
    let name: String = jni_try!(
        env,
        env.get_string(&field)
            .map(|s| s.to_string_lossy().into_owned())
    );
    let h = jni_try!(env, handle(ptr));
    let mut g = jni_try!(env, h.lock().map_err(|_| "poisoned lock".to_string()));
    match g.current.as_mut() {
        Some(doc) => {
            doc.add(&name, value);
            0
        }
        None => {
            throw(
                env,
                "java/lang/IllegalStateException",
                "beginDocument not called",
            );
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
    let v: String = jni_try!(
        &mut env,
        env.get_string(&value)
            .map(|s| s.to_string_lossy().into_owned())
    );
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
    let v: String = jni_try!(
        &mut env,
        env.get_string(&value)
            .map(|s| s.to_string_lossy().into_owned())
    );
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
pub extern "system" fn Java_RustIndexWriter_nativeEndDocument(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) -> jlong {
    let h = jni_try!(&mut env, handle(ptr));
    let mut g = jni_try!(&mut env, h.lock().map_err(|_| "poisoned lock".to_string()));
    match g.current.take() {
        Some(doc) => {
            jni_try!(
                &mut env,
                g.writer.add_document(doc).map_err(|e| e.to_string())
            );
            0
        }
        None => {
            throw(
                &mut env,
                "java/lang/IllegalStateException",
                "beginDocument not called",
            );
            0
        }
    }
}

/// JSON batch write: each element of `docs` is one raw JSON document
/// (UTF-8 bytes, flat object). Parsing, schema binding, coercion and
/// unknown-field policy all happen here, so a batch crosses JNI once.
/// A bad line or a failing document never aborts the batch.
///
/// Returns a packed long: `(ok_count << 32) | (failed_count & 0xffffffff)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeAddJsonBatch(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
    docs: JObjectArray,
) -> jlong {
    let h = jni_try!(&mut env, handle(ptr));
    let len = jni_try!(
        &mut env,
        env.get_array_length(&docs).map_err(|e| e.to_string())
    );
    // one lock for the whole batch
    let mut g = jni_try!(&mut env, h.lock().map_err(|_| "poisoned lock".to_string()));
    let WriterHandle { writer, binder, .. } = &mut *g;
    let (mut ok, mut failed) = (0i64, 0i64);
    for i in 0..len {
        let elem = match env.get_object_array_element(&docs, i) {
            Ok(e) => e,
            Err(e) => {
                throw(&mut env, "java/io/IOException", &e.to_string());
                return (ok << 32) | (failed & 0xffff_ffff);
            }
        };
        let bytes = match env.convert_byte_array(JByteArray::from(elem)) {
            Ok(b) => b,
            Err(e) => {
                throw(&mut env, "java/io/IOException", &e.to_string());
                return (ok << 32) | (failed & 0xffff_ffff);
            }
        };
        match binder.bind(writer.schema_mut(), &bytes) {
            // newly registered fields (Dynamic/StoredOnly) are already in the
            // schema; the writer picks them up on add_document
            BindOutcome::Doc(doc, _new_fields) => match writer.add_document(doc) {
                Ok(()) => ok += 1,
                Err(_) => failed += 1,
            },
            BindOutcome::Skip => failed += 1,
        }
    }
    (ok << 32) | (failed & 0xffff_ffff)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeFlush(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) -> jlong {
    let h = jni_try!(&mut env, handle(ptr));
    let mut g = jni_try!(&mut env, h.lock().map_err(|_| "poisoned lock".to_string()));
    jni_try!(&mut env, g.writer.flush().map_err(|e| e.to_string()));
    0
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeCommit(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) -> jlong {
    let h = jni_try!(&mut env, handle(ptr));
    let mut g = jni_try!(&mut env, h.lock().map_err(|_| "poisoned lock".to_string()));
    jni_try!(&mut env, g.writer.commit().map_err(|e| e.to_string()));
    0
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeClose(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) -> jlong {
    if ptr == 0 {
        return 0;
    }
    let handle = unsafe { Box::from_raw(ptr as *mut Mutex<WriterHandle>) };
    if let Ok(mut g) = handle.lock() {
        if g.current.is_some() {
            throw(
                &mut env,
                "java/lang/IllegalStateException",
                "uncommitted document discarded",
            );
        }
        g.current = None;
    }
    drop(handle);
    0
}

#[cfg(test)]
mod tests {
    use rustlucene_core::Schema;

    fn parse_schema(spec: &str) -> Result<Schema, String> {
        Schema::parse(spec).map(|(s, _, _)| s)
    }

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
