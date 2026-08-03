//! JSON batch binding: parses flat JSON objects (one document per line) and
//! binds values to schema fields by name, with lenient type coercion. All
//! parsing/coercion/filtering happens in Rust so a JNI caller can hand over
//! raw JSON bytes (e.g. a Kafka payload as consumed) in one call per batch.
//!
//! Schema spec strings (shared with the JNI bridge, see [`Schema::parse`]):
//! comma-separated `name:type[+modifier...]` entries, an optional
//! `lucene名:type+mods@json键` alias suffix per entry, and a leading
//! `$policy=strict|dynamic|stored-only` directive for unknown JSON fields.
//! `text` fields accept an `analyzer=tokenizer|filter|...` modifier
//! (e.g. `message:text+analyzer=whitespace|lowercase`).

use std::collections::HashMap;

use serde_json::Value;

use crate::document::{Document, FieldValue};
use crate::schema::{FieldSpec, Schema};

/// How [`JsonBinder`] treats JSON fields absent from the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FieldPolicy {
    /// Drop unknown fields (default).
    #[default]
    Strict,
    /// Register unknown fields by value shape: string/bool/float -> keyword,
    /// integer -> long point + NumericDocValues.
    Dynamic,
    /// Store unknown fields without indexing them.
    StoredOnly,
}

/// Result of binding one JSON line.
pub enum BindOutcome {
    /// A bound document plus the field specs newly registered into the
    /// schema while binding it (Dynamic/StoredOnly policies).
    Doc(Document, Vec<FieldSpec>),
    /// Unparseable JSON or a non-object top level: the line is dropped.
    Skip,
}

/// Binds JSON object keys to schema fields. Built once from the initial
/// schema plus the spec's aliases; the schema itself grows dynamically
/// (append-only) so field numbers stay Vec indices, matching Lucene's
/// FieldInfos numbering.
pub struct JsonBinder {
    /// JSON key -> lucene field name (identity unless the spec aliases it).
    map: HashMap<String, String>,
    /// Field names declared at build time; names registered later
    /// (Dynamic/StoredOnly) bind under their own JSON key.
    declared: std::collections::HashSet<String>,
    policy: FieldPolicy,
}

impl JsonBinder {
    pub fn new(schema: &Schema, aliases: &[(String, String)], policy: FieldPolicy) -> Self {
        let mut map: HashMap<String, String> = schema
            .fields()
            .iter()
            .map(|f| (f.name.clone(), f.name.clone()))
            .collect();
        for (lucene, json) in aliases {
            assert!(
                schema.get(lucene).is_some(),
                "alias target {lucene} is not a schema field"
            );
            map.remove(lucene);
            map.insert(json.clone(), lucene.clone());
        }
        let declared = schema.fields().iter().map(|f| f.name.clone()).collect();
        Self {
            map,
            declared,
            policy,
        }
    }

    pub fn policy(&self) -> FieldPolicy {
        self.policy
    }

    /// Parses one JSON line and binds it to a document. Unknown fields follow
    /// the policy; nested object/array values are always skipped (MVP does
    /// not flatten). Coercion failures skip only the offending field.
    pub fn bind(&self, schema: &mut Schema, line: &[u8]) -> BindOutcome {
        let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(line) else {
            return BindOutcome::Skip;
        };
        let mut doc = Document::new();
        let mut new_fields: Vec<FieldSpec> = Vec::new();
        for (key, value) in obj {
            let lucene_name = match self.map.get(&key) {
                Some(n) => Some(n.clone()),
                None if !self.declared.contains(&key) && schema.get(&key).is_some() => {
                    Some(key.clone())
                }
                None => match self.policy {
                    FieldPolicy::Strict => None,
                    FieldPolicy::StoredOnly => {
                        let spec = FieldSpec::stored(&key);
                        schema.add(spec.clone());
                        new_fields.push(spec);
                        Some(key.clone())
                    }
                    FieldPolicy::Dynamic => {
                        let spec = match infer_spec(&key, &value) {
                            Some(s) => s,
                            None => continue, // null / nested values cannot be inferred
                        };
                        schema.add(spec.clone());
                        new_fields.push(spec);
                        Some(key.clone())
                    }
                },
            };
            let Some(lucene_name) = lucene_name else {
                continue;
            };
            let spec = schema.get(&lucene_name).expect("field just resolved");
            if let Some(v) = coerce(spec, &value) {
                doc.add(&lucene_name, v);
            }
        }
        BindOutcome::Doc(doc, new_fields)
    }
}

/// Infers a field spec from a JSON value (Dynamic policy). Integers get the
/// range-query profile (long point + NumericDocValues); there is no double
/// point support in [`FieldSpec`], so floats degrade to keyword strings.
fn infer_spec(name: &str, value: &Value) -> Option<FieldSpec> {
    match value {
        Value::String(_) | Value::Bool(_) => Some(FieldSpec::keyword(name)),
        Value::Number(n) if n.is_i64() || n.is_u64() => {
            Some(FieldSpec::long_point(name).with_numeric_dv())
        }
        Value::Number(_) => Some(FieldSpec::keyword(name)),
        _ => None,
    }
}

/// Leniently coerces a JSON value to the field's declared kind. Numbers
/// stringify into text/keyword fields; numeric strings parse into
/// point/doc-values fields. Returns None when the value cannot fit
/// (e.g. a 64-bit value into an int point, or a float into a long point).
fn coerce(spec: &FieldSpec, value: &Value) -> Option<FieldValue> {
    let as_string = |v: &Value| -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    };
    let as_int = |v: &Value| -> Option<i64> {
        match v {
            Value::Number(n) => n
                .as_i64()
                .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok())),
            Value::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        }
    };
    if spec.tokenized {
        return as_string(value).map(FieldValue::Text);
    }
    if spec.is_indexed() {
        return as_string(value).map(FieldValue::Keyword);
    }
    if spec.points.is_some() || spec.doc_values != codec_lucene9::DocValuesType::None {
        let width = spec.points.map(|p| p.bytes_per_dim).unwrap_or(8);
        let v = as_int(value)?;
        return match width {
            4 => i32::try_from(v).ok().map(FieldValue::Int),
            _ => Some(FieldValue::Long(v)),
        };
    }
    // stored-only field: keep the natural representation
    match value {
        Value::Number(n) => n
            .as_i64()
            .map(FieldValue::Long)
            .or_else(|| as_string(value).map(FieldValue::Text)),
        _ => as_string(value).map(FieldValue::Text),
    }
}

impl Schema {
    /// Parses a schema spec string: comma-separated `name:type[+modifier...]`
    /// entries, each optionally suffixed with `@json键` to bind a differently
    /// named JSON key to this lucene field. A `$policy=strict|dynamic|
    /// stored-only` entry sets the unknown-field policy (default strict).
    ///
    /// Returns the schema, the `(lucene名, json键)` alias list, and the policy.
    pub fn parse(spec: &str) -> Result<(Schema, Vec<(String, String)>, FieldPolicy), String> {
        let mut schema = Schema::new();
        let mut aliases: Vec<(String, String)> = Vec::new();
        let mut policy = FieldPolicy::default();
        for entry in spec.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if let Some(directive) = entry.strip_prefix('$') {
                match directive.split_once('=') {
                    Some(("policy", v)) => {
                        policy = match v.trim() {
                            "strict" => FieldPolicy::Strict,
                            "dynamic" => FieldPolicy::Dynamic,
                            "stored-only" => FieldPolicy::StoredOnly,
                            other => return Err(format!("unknown field policy: {other}")),
                        };
                    }
                    _ => return Err(format!("unknown directive: ${directive}")),
                }
                continue;
            }
            let (decl, json_key) = match entry.split_once('@') {
                Some((d, k)) => (d.trim(), Some(k.trim())),
                None => (entry, None),
            };
            let mut parts = decl.split(':');
            let name = parts.next().ok_or("missing field name")?.trim();
            let mut ty = parts.next().ok_or("missing field type")?.trim();
            let mut modifiers = "";
            if let Some((t, m)) = ty.split_once('+') {
                ty = t.trim();
                modifiers = m;
            }
            let has = |m: &str| modifiers.split('+').any(|x| x.trim() == m);
            let analyzer = modifiers
                .split('+')
                .find_map(|m| m.trim().strip_prefix("analyzer="));
            if analyzer.is_some() && ty != "text" {
                return Err(format!("field {name}: analyzer= is only valid on text fields"));
            }
            let spec = match ty {
                "text" => {
                    let mut s = if has("positions") {
                        FieldSpec::text_with_positions(name)
                    } else {
                        FieldSpec::text(name)
                    };
                    if let Some(a) = analyzer {
                        s = s.with_analyzer(a);
                    }
                    s
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
            if let Some(a) = analyzer {
                crate::analysis::Analyzer::parse(a)
                    .map_err(|e| format!("field {name}: invalid analyzer spec: {e}"))?;
            }
            schema.add(spec);
            if let Some(k) = json_key {
                if k.is_empty() {
                    return Err(format!("empty json key in entry: {entry}"));
                }
                aliases.push((name.to_string(), k.to_string()));
            }
        }
        Ok((schema, aliases, policy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_schema() -> Schema {
        let (s, _, _) = Schema::parse(
            "timestamp:longpoint+numericdv+stored,level:keyword+sorteddv,trace_id:keyword,\
             message:text+positions,latency_ms:intpoint+numericdv+stored",
        )
        .unwrap();
        s
    }

    fn bind_ok(b: &JsonBinder, s: &mut Schema, line: &str) -> Document {
        match b.bind(s, line.as_bytes()) {
            BindOutcome::Doc(d, _) => d,
            BindOutcome::Skip => panic!("expected a document for: {line}"),
        }
    }

    #[test]
    fn binds_known_fields_with_coercion() {
        let mut schema = log_schema();
        let b = JsonBinder::new(&schema, &[], FieldPolicy::Strict);
        let doc = bind_ok(
            &b,
            &mut schema,
            r#"{"timestamp":1700000000123,"level":"INFO","trace_id":"t-1",
                "message":"hello world","latency_ms":"42"}"#,
        );
        assert!(doc
            .fields
            .contains(&("timestamp".into(), FieldValue::Long(1700000000123))));
        assert!(doc
            .fields
            .contains(&("level".into(), FieldValue::Keyword("INFO".into()))));
        assert!(doc
            .fields
            .contains(&("message".into(), FieldValue::Text("hello world".into()))));
        // numeric string coerces into an int point
        assert!(doc
            .fields
            .contains(&("latency_ms".into(), FieldValue::Int(42))));
    }

    #[test]
    fn numbers_stringify_into_text_and_keyword() {
        let mut schema = log_schema();
        let b = JsonBinder::new(&schema, &[], FieldPolicy::Strict);
        let doc = bind_ok(&b, &mut schema, r#"{"level":200,"message":12345}"#);
        assert!(doc
            .fields
            .contains(&("level".into(), FieldValue::Keyword("200".into()))));
        assert!(doc
            .fields
            .contains(&("message".into(), FieldValue::Text("12345".into()))));
    }

    #[test]
    fn alias_maps_json_key_to_lucene_field() {
        let (mut schema, aliases, _) =
            Schema::parse("message_raw:keyword+sorteddv@message").unwrap();
        let b = JsonBinder::new(&schema, &aliases, FieldPolicy::Strict);
        let doc = bind_ok(
            &b,
            &mut schema,
            r#"{"message":"raw payload","message_raw":"ignored"}"#,
        );
        assert_eq!(
            doc.fields,
            vec![(
                "message_raw".to_string(),
                FieldValue::Keyword("raw payload".into())
            )]
        );
    }

    #[test]
    fn bad_json_and_non_object_lines_skip() {
        let mut schema = log_schema();
        let b = JsonBinder::new(&schema, &[], FieldPolicy::Strict);
        assert!(matches!(
            b.bind(&mut schema, b"{not json"),
            BindOutcome::Skip
        ));
        assert!(matches!(b.bind(&mut schema, b"[1,2,3]"), BindOutcome::Skip));
        assert!(matches!(b.bind(&mut schema, b"42"), BindOutcome::Skip));
        assert!(matches!(b.bind(&mut schema, b""), BindOutcome::Skip));
    }

    #[test]
    fn strict_drops_unknown_fields() {
        let mut schema = log_schema();
        let b = JsonBinder::new(&schema, &[], FieldPolicy::Strict);
        let doc = bind_ok(&b, &mut schema, r#"{"level":"INFO","noise_payload":7}"#);
        assert_eq!(doc.fields.len(), 1);
        assert!(schema.get("noise_payload").is_none());
    }

    #[test]
    fn stored_only_registers_unindexed_fields() {
        let mut schema = log_schema();
        let b = JsonBinder::new(&schema, &[], FieldPolicy::StoredOnly);
        let doc = bind_ok(&b, &mut schema, r#"{"noise_payload":"abc"}"#);
        let spec = schema.get("noise_payload").unwrap();
        assert!(!spec.is_indexed() && spec.stored);
        assert!(doc
            .fields
            .contains(&("noise_payload".into(), FieldValue::Text("abc".into()))));
    }

    #[test]
    fn dynamic_infers_and_reuses_registered_types() {
        let mut schema = log_schema();
        let b = JsonBinder::new(&schema, &[], FieldPolicy::Dynamic);
        let (doc1, new1) = match b.bind(
            &mut schema,
            br#"{"extra_num":7,"extra_str":"x","extra_bool":true,"extra_float":1.5}"#,
        ) {
            BindOutcome::Doc(d, n) => (d, n),
            BindOutcome::Skip => panic!(),
        };
        assert_eq!(new1.len(), 4);
        assert!(schema.get("extra_num").unwrap().points.is_some());
        assert!(schema.get("extra_str").unwrap().is_indexed());
        assert!(schema.get("extra_bool").unwrap().is_indexed());
        // no double-point support: floats degrade to keyword strings
        assert!(schema.get("extra_float").unwrap().is_indexed());
        assert!(doc1
            .fields
            .contains(&("extra_num".into(), FieldValue::Long(7))));
        assert!(doc1
            .fields
            .contains(&("extra_float".into(), FieldValue::Keyword("1.5".into()))));

        // second document reuses the registered types (no re-registration)
        let (doc2, new2) = match b.bind(&mut schema, br#"{"extra_num":"9","extra_float":2.5}"#) {
            BindOutcome::Doc(d, n) => (d, n),
            BindOutcome::Skip => panic!(),
        };
        assert!(new2.is_empty());
        assert!(doc2
            .fields
            .contains(&("extra_num".into(), FieldValue::Long(9))));
        assert!(doc2
            .fields
            .contains(&("extra_float".into(), FieldValue::Keyword("2.5".into()))));
    }

    #[test]
    fn nested_values_and_bad_coercions_skip_the_field_only() {
        let mut schema = log_schema();
        let b = JsonBinder::new(&schema, &[], FieldPolicy::Dynamic);
        let doc = bind_ok(
            &b,
            &mut schema,
            r#"{"level":"WARN","obj":{"a":1},"arr":[1],"timestamp":"not-a-number","latency_ms":9999999999999}"#,
        );
        assert_eq!(doc.fields.len(), 1);
        assert!(doc
            .fields
            .contains(&("level".into(), FieldValue::Keyword("WARN".into()))));
        assert!(schema.get("obj").is_none());
        assert!(schema.get("arr").is_none());
    }

    #[test]
    fn spec_parser_reads_policy_and_aliases() {
        let (s, aliases, policy) = Schema::parse(
            "$policy=dynamic,message_raw:keyword+sorteddv@message,ts:longpoint+numericdv+stored@timestamp",
        )
        .unwrap();
        assert_eq!(policy, FieldPolicy::Dynamic);
        assert_eq!(
            aliases,
            vec![
                ("message_raw".to_string(), "message".to_string()),
                ("ts".to_string(), "timestamp".to_string())
            ]
        );
        assert_eq!(s.fields().len(), 2);
        // old syntax stays valid and defaults to strict
        let (s2, a2, p2) = Schema::parse("message:text+positions").unwrap();
        assert_eq!(p2, FieldPolicy::Strict);
        assert!(a2.is_empty());
        assert!(s2.get("message").unwrap().has_positions());
        assert!(Schema::parse("$policy=bogus,x:keyword").is_err());
        assert!(Schema::parse("x:keyword@").is_err());
    }

    #[test]
    fn analyzer_modifier_attaches_to_text_fields() {
        let (s, _, _) = Schema::parse("message:text+positions+analyzer=whitespace|lowercase").unwrap();
        assert_eq!(
            s.get("message").unwrap().analyzer.as_deref(),
            Some("whitespace|lowercase")
        );
        // plain text without analyzer keeps None (legacy behavior)
        let (s2, _, _) = Schema::parse("message:text").unwrap();
        assert!(s2.get("message").unwrap().analyzer.is_none());
    }

    #[test]
    fn analyzer_modifier_rejected_on_non_text_and_unknown_components() {
        assert!(Schema::parse("level:keyword+analyzer=whitespace").is_err());
        assert!(Schema::parse("ts:longpoint+analyzer=whitespace").is_err());
        assert!(Schema::parse("message:text+analyzer=nosuchtok").is_err());
        assert!(Schema::parse("message:text+analyzer=whitespace|nosuchfilter").is_err());
    }
}
