//! JNI bindings exposing the RustLucene write + read path to Java.
//!
//! Java side: `interop/java/RustIndexWriter.java`. The native handle is a
//! boxed [`WriterHandle`] with an `RwLock<IndexWriter>` for concurrent reads
//! (search, document retrieval) and a `Mutex<WriteState>` for serialized
//! document assembly. Write operations lock `write_state` then `index.write()`;
//! read operations only lock `index.read()`.
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
use std::sync::{Mutex, RwLock};

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JObjectArray, JString};
use jni::sys::{jint, jlong};
use rustlucene_core::{
    BindOutcome, DocLocation, Document, FieldValue, IndexWriter, IndexWriterConfig, JsonBinder,
    Schema,
};

struct WriteState {
    current: Option<Document>,
    binder: JsonBinder,
}

struct WriterHandle {
    index: RwLock<IndexWriter>,
    write_state: Mutex<WriteState>,
}

fn handle<'a>(ptr: jlong) -> Result<&'a WriterHandle, String> {
    if ptr == 0 {
        return Err("null writer handle".into());
    }
    Ok(unsafe { &*(ptr as *const WriterHandle) })
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

/// Like jni_try! but returns a null JObject (for JByteArray-returning fns).
macro_rules! jni_try_obj {
    ($env:expr, $e:expr) => {
        match $e {
            Ok(v) => v,
            Err(msg) => {
                throw($env, "java/io/IOException", &msg.to_string());
                return JObject::null().into();
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
    let handle = Box::new(WriterHandle {
        index: RwLock::new(writer),
        write_state: Mutex::new(WriteState {
            current: None,
            binder,
        }),
    });
    Box::into_raw(handle) as jlong
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeBeginDocument(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) -> jlong {
    let h = jni_try!(&mut env, handle(ptr));
    let mut ws = jni_try!(
        &mut env,
        h.write_state.lock().map_err(|_| "poisoned lock".to_string())
    );
    ws.current = Some(Document::new());
    0
}

fn add_field(env: &mut JNIEnv, ptr: jlong, field: JString, value: FieldValue) -> jlong {
    let name: String = jni_try!(
        env,
        env.get_string(&field)
            .map(|s| s.to_string_lossy().into_owned())
    );
    let h = jni_try!(env, handle(ptr));
    let mut ws = jni_try!(
        env,
        h.write_state.lock().map_err(|_| "poisoned lock".to_string())
    );
    match ws.current.as_mut() {
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
    let doc = {
        let mut ws = jni_try!(
            &mut env,
            h.write_state.lock().map_err(|_| "poisoned lock".to_string())
        );
        match ws.current.take() {
            Some(d) => d,
            None => {
                throw(
                    &mut env,
                    "java/lang/IllegalStateException",
                    "beginDocument not called",
                );
                return 0;
            }
        }
    };
    let mut guard = h.index.write().unwrap();
    jni_try!(&mut env, guard.add_document(doc).map_err(|e| e.to_string()));
    0
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
    // Lock write_state for binder access, then index.write() for the batch.
    let ws = jni_try!(
        &mut env,
        h.write_state.lock().map_err(|_| "poisoned lock".to_string())
    );
    let mut guard = h.index.write().unwrap();
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
        match ws.binder.bind(guard.schema_mut(), &bytes) {
            BindOutcome::Doc(doc, _new_fields) => match guard.add_document(doc) {
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
    let mut guard = h.index.write().unwrap();
    jni_try!(&mut env, guard.flush().map_err(|e| e.to_string()));
    0
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeCommit(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) -> jlong {
    let h = jni_try!(&mut env, handle(ptr));
    let mut guard = h.index.write().unwrap();
    jni_try!(&mut env, guard.commit().map_err(|e| e.to_string()));
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
    let handle = unsafe { Box::from_raw(ptr as *mut WriterHandle) };
    if let Ok(ws) = handle.write_state.lock() {
        if ws.current.is_some() {
            throw(
                &mut env,
                "java/lang/IllegalStateException",
                "uncommitted document discarded",
            );
        }
    }
    drop(handle);
    0
}

// ---------------------------------------------------------------------------
// Read path: nativeSearch + nativeDocument
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeSearch<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
    ptr: jlong,
    query_bytes: JByteArray<'a>,
) -> JByteArray<'a> {
    let h = jni_try_obj!(&mut env, handle(ptr));
    let json = jni_try_obj!(
        &mut env,
        env.convert_byte_array(&query_bytes)
            .map_err(|e| e.to_string())
    );
    let req = jni_try_obj!(&mut env, query_parser::parse_search_request(&json));
    let query = jni_try_obj!(&mut env, req.to_query());
    let sort_field = req.sort_field();

    let guard = h.index.read().unwrap();
    let results = jni_try_obj!(
        &mut env,
        guard
            .search(&query, sort_field, req.top_n)
            .map_err(|e| e.to_string())
    );
    drop(guard);

    // Serialize: {"total":N,"docs":[id,...]}
    let json_out = serde_json::json!({
        "total": results.total,
        "docs": results.docs,
    });
    let bytes = json_out.to_string().into_bytes();
    jni_try_obj!(
        &mut env,
        env.byte_array_from_slice(&bytes).map_err(|e| e.to_string())
    )
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_RustIndexWriter_nativeDocument<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
    ptr: jlong,
    doc_id: jint,
) -> JByteArray<'a> {
    let h = jni_try_obj!(&mut env, handle(ptr));

    // Phase 1: lock to get location + schema field names (~100ns)
    let (loc, dir_path, field_names, raw_bytes) = {
        let guard = h.index.read().unwrap();
        let loc = guard.document_location(doc_id as u32);
        let dir_path = guard.dir_path().to_path_buf();
        let field_names: Vec<String> = guard
            .schema()
            .fields()
            .iter()
            .map(|f| f.name.clone())
            .collect();
        // For unflushed buffer docs, grab the raw bytes while holding the lock
        let raw = match &loc {
            DocLocation::Buffer { local_id, flushed } if !flushed => {
                guard.buffered_stored_bytes(*local_id).map(|b| b.to_vec())
            }
            _ => None,
        };
        (loc, dir_path, field_names, raw)
    };
    // Lock dropped here

    // Phase 2: decode stored fields without lock
    let fields = jni_try_obj!(
        &mut env,
        read_stored_fields(&dir_path, &loc, &field_names, raw_bytes.as_deref())
    );

    let json_out = serde_json::to_string(&fields).unwrap_or_default();
    let bytes = json_out.into_bytes();
    jni_try_obj!(
        &mut env,
        env.byte_array_from_slice(&bytes).map_err(|e| e.to_string())
    )
}

/// Reads stored fields for a document given its location.
/// - Buffer (unflushed): decodes from raw SFW bytes
/// - CommittedSegment / Buffer (flushed): returns error (not yet implemented)
fn read_stored_fields(
    _dir_path: &Path,
    loc: &DocLocation,
    field_names: &[String],
    raw_bytes: Option<&[u8]>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    match loc {
        DocLocation::Buffer { flushed, .. } if !flushed => {
            let bytes = raw_bytes.ok_or("no buffered bytes available")?;
            decode_stored_doc(bytes, field_names)
        }
        DocLocation::Buffer { flushed: true, .. } => {
            Err("stored field retrieval from flushed buffer chunks not yet implemented".into())
        }
        DocLocation::Buffer { .. } => {
            // Unreachable: flushed==false is handled above
            Err("internal error: unexpected buffer state".into())
        }
        DocLocation::CommittedSegment { .. } => {
            Err("stored field retrieval from committed segments not yet implemented".into())
        }
        DocLocation::NotFound => Err("document not found".into()),
    }
}

/// Decodes raw stored-field bytes (Lucene90 per-doc format) into a JSON map.
/// Format: repeated [VLong(field_number<<3 | type_tag), value...]
fn decode_stored_doc(
    mut data: &[u8],
    field_names: &[String],
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    use serde_json::{Map, Value};
    let mut map = Map::new();
    while !data.is_empty() {
        let info = read_vlong(&mut data)?;
        let field_num = (info >> 3) as usize;
        let type_tag = (info & 0x7) as u8;
        let name = field_names
            .get(field_num)
            .cloned()
            .unwrap_or_else(|| format!("_field_{field_num}"));
        let value = match type_tag {
            0 => {
                // String
                let len = read_vint(&mut data)? as usize;
                if data.len() < len {
                    return Err("truncated stored string".into());
                }
                let s = String::from_utf8_lossy(&data[..len]).into_owned();
                data = &data[len..];
                Value::String(s)
            }
            1 => {
                // Bytes
                let len = read_vint(&mut data)? as usize;
                if data.len() < len {
                    return Err("truncated stored bytes".into());
                }
                // Encode as base64-ish hex for JSON safety
                let hex: String = data[..len]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                data = &data[len..];
                Value::String(hex)
            }
            2 => {
                // Int (zigzag)
                let v = read_vlong(&mut data)?;
                let zigzag = ((v >> 1) as i32) ^ (-((v & 1) as i32));
                Value::Number(zigzag.into())
            }
            3 => {
                // Float (ZFloat encoding)
                let f = read_zfloat(&mut data)?;
                serde_json::Number::from_f64(f as f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            }
            4 => {
                // Long (TLong encoding)
                let v = read_tlong(&mut data)?;
                Value::Number(v.into())
            }
            5 => {
                // Double (ZDouble encoding)
                let v = read_zdouble(&mut data)?;
                serde_json::Number::from_f64(v)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            }
            other => return Err(format!("unknown stored field type tag: {other}")),
        };
        map.insert(name, value);
    }
    Ok(map)
}

// --- VLong / VInt / TLong / ZDouble readers (Lucene90 stored fields) ---

fn read_vlong(data: &mut &[u8]) -> Result<u64, String> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        if data.is_empty() {
            return Err("truncated vlong".into());
        }
        let b = data[0];
        *data = &data[1..];
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 64 {
            return Err("vlong too long".into());
        }
    }
}

fn read_vint(data: &mut &[u8]) -> Result<i32, String> {
    read_vlong(data).map(|v| v as i32)
}

/// Inverse of `write_tlong` (stored_fields.rs:74-98).
/// Header: bits 0-4 = low 5 bits of zigzag(value), bit 5 = has upper,
/// bits 6-7 = time encoding (0x40=SECOND, 0x80=HOUR, 0xC0=DAY).
fn read_tlong(data: &mut &[u8]) -> Result<i64, String> {
    if data.is_empty() {
        return Err("truncated tlong".into());
    }
    let header = data[0];
    *data = &data[1..];

    let encoding = header & 0xc0;
    let low = (header & 0x1f) as u64;
    let has_upper = header & 0x20 != 0;
    let upper = if has_upper { read_vlong(data)? } else { 0 };
    let zigzag = (upper << 5) | low;
    // zigzag decode: (n >> 1) ^ -(n & 1)
    let mut value = ((zigzag >> 1) as i64) ^ (-((zigzag & 1) as i64));

    // Apply time encoding multiplier
    match encoding {
        0x40 => value *= 1_000,        // SECOND
        0x80 => value *= 3_600_000,    // HOUR
        0xc0 => value *= 86_400_000,   // DAY
        _ => {}
    }
    Ok(value)
}

/// Inverse of `write_zdouble` (stored_fields.rs:122-145).
fn read_zdouble(data: &mut &[u8]) -> Result<f64, String> {
    if data.is_empty() {
        return Err("truncated zdouble".into());
    }
    let first = data[0];
    *data = &data[1..];

    if first == 0xFE {
        // Float-accurate: 4 bytes LE f32 bits
        if data.len() < 4 {
            return Err("truncated zdouble float".into());
        }
        let bits = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        *data = &data[4..];
        Ok(f32::from_bits(bits) as f64)
    } else if first == 0xFF {
        // Negative double: 8 bytes LE
        if data.len() < 8 {
            return Err("truncated zdouble negative".into());
        }
        let bits = u64::from_le_bytes([
            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
        ]);
        *data = &data[8..];
        Ok(f64::from_bits(bits))
    } else if first >= 0x80 {
        // Small integer [-1..124]: single byte
        Ok((first & 0x7f) as i32 as f64 - 1.0)
    } else {
        // Positive double: first byte is bits>>56, then LE int (bits>>24),
        // LE short (bits>>8), byte (bits & 0xFF)
        if data.len() < 7 {
            return Err("truncated zdouble positive".into());
        }
        let b0 = first as u64;
        let mid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as u64;
        let lo_short = u16::from_le_bytes([data[4], data[5]]) as u64;
        let lo_byte = data[6] as u64;
        *data = &data[7..];
        let bits = (b0 << 56) | (mid << 24) | ((lo_short as u64) << 8) | lo_byte;
        Ok(f64::from_bits(bits))
    }
}

/// Inverse of `write_zfloat` (stored_fields.rs:102-118).
fn read_zfloat(data: &mut &[u8]) -> Result<f32, String> {
    if data.is_empty() {
        return Err("truncated zfloat".into());
    }
    let first = data[0];
    *data = &data[1..];

    if first == 0xFF {
        // Negative float: 4 bytes LE f32 bits
        if data.len() < 4 {
            return Err("truncated zfloat negative".into());
        }
        let bits = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        *data = &data[4..];
        Ok(f32::from_bits(bits))
    } else if first >= 0x80 {
        // Small integer [-1..125]: single byte
        Ok((first & 0x7f) as i32 as f32 - 1.0)
    } else {
        // Positive float: first byte is bits>>24, then LE short (bits>>8), byte (bits & 0xFF)
        if data.len() < 3 {
            return Err("truncated zfloat".into());
        }
        let mid = u16::from_le_bytes([data[0], data[1]]) as u32;
        let lo = data[2] as u32;
        *data = &data[3..];
        let bits = ((first as u32) << 24) | (mid << 8) | lo;
        Ok(f32::from_bits(bits))
    }
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

    #[test]
    fn decode_stored_doc_string_and_long() {
        use super::*;
        // Simulate: field 0 = String("hello"), field 1 = Long(42)
        let mut buf = Vec::new();
        // field 0, type STRING (0): info = (0 << 3) | 0 = 0
        buf.push(0x00); // vlong 0
        buf.push(5); // vint len=5
        buf.extend_from_slice(b"hello");
        // field 1, type LONG (4): info = (1 << 3) | 4 = 12
        buf.push(12); // vlong 12
        // TLong encoding of 42: 42 % 1000 != 0 → encoding = 0
        // zigzag(42) = 84 = 0b1010100
        // low 5 bits = 84 & 0x1f = 20, upper = 84 >> 5 = 2
        // header = 0 | 20 | 0x20 (has upper) = 0x34
        buf.push(0x34);
        buf.push(2); // vlong upper = 2
        let names = vec!["msg".to_string(), "ts".to_string()];
        let result = decode_stored_doc(&buf, &names).unwrap();
        assert_eq!(result.get("msg").unwrap(), &serde_json::Value::String("hello".into()));
        assert_eq!(result.get("ts").unwrap(), &serde_json::json!(42));
    }

    #[test]
    fn decode_stored_doc_int() {
        use super::*;
        let mut buf = Vec::new();
        // field 0, type INT (2): info = (0 << 3) | 2 = 2
        buf.push(2); // vlong 2
        // zigzag(7) = 14
        buf.push(14); // vlong 14
        let names = vec!["count".to_string()];
        let result = decode_stored_doc(&buf, &names).unwrap();
        assert_eq!(result.get("count").unwrap(), &serde_json::json!(7));
    }

    #[test]
    fn decode_stored_doc_float_double_roundtrip() {
        use super::*;

        // --- Helper: encode zfloat the same way as write_zfloat ---
        fn encode_zfloat(out: &mut Vec<u8>, f: f32) {
            let int_val = f as i32;
            let float_bits = f.to_bits() as i32;
            let neg_zero = (-0.0f32).to_bits() as i32;
            if f == int_val as f32 && (-1..=0x7d).contains(&int_val) && float_bits != neg_zero {
                out.push(0x80 | (1 + int_val) as u8);
            } else if float_bits >= 0 {
                out.push((float_bits >> 24) as u8);
                out.extend_from_slice(&((float_bits >> 8) as i16).to_le_bytes());
                out.push(float_bits as u8);
            } else {
                out.push(0xFF);
                out.extend_from_slice(&float_bits.to_le_bytes());
            }
        }

        // --- Helper: encode zdouble the same way as write_zdouble ---
        fn encode_zdouble(out: &mut Vec<u8>, d: f64) {
            let int_val = d as i32;
            let double_bits = d.to_bits() as i64;
            let neg_zero = (-0.0f64).to_bits() as i64;
            if d == int_val as f64 && (-1..=0x7c).contains(&int_val) && double_bits != neg_zero {
                out.push(0x80 | (int_val + 1) as u8);
            } else if d == d as f32 as f64 {
                out.push(0xFE);
                out.extend_from_slice(&((d as f32).to_bits() as i32).to_le_bytes());
            } else if double_bits >= 0 {
                out.push((double_bits >> 56) as u8);
                out.extend_from_slice(&((double_bits >> 24) as i32).to_le_bytes());
                out.extend_from_slice(&((double_bits >> 8) as i16).to_le_bytes());
                out.push(double_bits as u8);
            } else {
                out.push(0xFF);
                out.extend_from_slice(&double_bits.to_le_bytes());
            }
        }

        let mut buf = Vec::new();

        // field 0: negative float -3.14 (type FLOAT=3), info = (0<<3)|3 = 3
        buf.push(3);
        encode_zfloat(&mut buf, -3.14f32);

        // field 1: float-accurate double 1.5 (type DOUBLE=5), info = (1<<3)|5 = 13
        buf.push(13);
        encode_zdouble(&mut buf, 1.5f64);

        // field 2: negative double -1e100 (type DOUBLE=5), info = (2<<3)|5 = 21
        buf.push(21);
        encode_zdouble(&mut buf, -1e100f64);

        // field 3: positive non-small double 1e100 (type DOUBLE=5), info = (3<<3)|5 = 29
        buf.push(29);
        encode_zdouble(&mut buf, 1e100f64);

        let names = vec![
            "neg_f".to_string(),
            "flt_d".to_string(),
            "neg_d".to_string(),
            "pos_d".to_string(),
        ];
        let result = decode_stored_doc(&buf, &names).unwrap();

        // Negative float: -3.14f32 as f64
        let neg_f = result.get("neg_f").unwrap().as_f64().unwrap();
        assert!((neg_f - (-3.14f32 as f64)).abs() < 1e-6, "neg_f={neg_f}");

        // Float-accurate double: 1.5
        let flt_d = result.get("flt_d").unwrap().as_f64().unwrap();
        assert_eq!(flt_d, 1.5f64);

        // Negative double: -1e100
        let neg_d = result.get("neg_d").unwrap().as_f64().unwrap();
        assert_eq!(neg_d, -1e100f64);

        // Positive non-small double: 1e100
        let pos_d = result.get("pos_d").unwrap().as_f64().unwrap();
        assert_eq!(pos_d, 1e100f64);
    }
}
