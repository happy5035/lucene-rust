//! Query-side analysis (spec §查询侧双通道): rewrites a parsed `Query` so
//! its term bytes match what the index holds for analyzer-configured
//! fields. Fields without an analyzer pass through byte-identical.
//! The execution engine never sees analyzers — rewriting happens here, at
//! query-build time (called from the JNI search entry point).

use std::borrow::Cow;

use super::Analyzer;
use crate::schema::Schema;
use crate::search::query::{Occur, Query};

pub fn analyze_query(query: &Query, schema: &Schema) -> Result<Query, String> {
    match query {
        Query::Term { field, term } => match analyzer_for(schema, field)? {
            None => Ok(query.clone()),
            Some(an) => {
                let toks = analyze_text(&an, field, term)?;
                match toks.len() {
                    0 => Err(format!("field {field}: query text analyzes to no tokens")),
                    1 => Ok(Query::Term {
                        field: field.clone(),
                        term: toks.into_iter().next().unwrap().into_owned(),
                    }),
                    // Lucene QueryParser default: multi-token match → OR.
                    _ => Ok(Query::bool(
                        toks.into_iter()
                            .map(|t| {
                                (
                                    Occur::Should,
                                    Query::Term {
                                        field: field.clone(),
                                        term: t.into_owned(),
                                    },
                                )
                            })
                            .collect(),
                    )),
                }
            }
        },
        Query::Terms { field, terms } => Ok(Query::Terms {
            field: field.clone(),
            terms: analyze_each(schema, field, terms, "terms")?,
        }),
        Query::And { field, terms } => Ok(Query::And {
            field: field.clone(),
            terms: analyze_each(schema, field, terms, "and")?,
        }),
        Query::Or { field, terms } => Ok(Query::Or {
            field: field.clone(),
            terms: analyze_each(schema, field, terms, "or")?,
        }),
        Query::Phrase { field, terms } => Ok(Query::Phrase {
            field: field.clone(),
            terms: analyze_each(schema, field, terms, "phrase")?,
        }),
        Query::Prefix { field, prefix } => match analyzer_for(schema, field)? {
            None => Ok(query.clone()),
            Some(an) => Ok(Query::Prefix {
                field: field.clone(),
                prefix: an.normalize(utf8(field, prefix)?).into_owned().into_bytes(),
            }),
        },
        Query::Wildcard { field, pattern, .. } => match analyzer_for(schema, field)? {
            None => Ok(query.clone()),
            // normalize then rebuild via the constructor — the DFA must be
            // recompiled for the rewritten pattern.
            Some(an) => Ok(Query::wildcard(field, &an.normalize(utf8(field, pattern)?))),
        },
        Query::Bool { clauses } => {
            let mut out = Vec::with_capacity(clauses.len());
            for (occur, sub) in clauses {
                out.push((*occur, analyze_query(sub, schema)?));
            }
            Ok(Query::Bool { clauses: out })
        }
        Query::MatchAll | Query::PointRange { .. } => Ok(query.clone()),
    }
}

/// Compiles the field's analyzer spec fresh (stateless components, a few
/// small enums) — no shared mutable state, so this works under the search
/// read lock (spec §查询侧双通道).
fn analyzer_for(schema: &Schema, field: &str) -> Result<Option<Analyzer>, String> {
    match schema.get(field).and_then(|f| f.analyzer.as_deref()) {
        Some(spec) => Ok(Some(Analyzer::parse(spec)?)),
        None => Ok(None),
    }
}

fn utf8<'a>(field: &str, bytes: &'a [u8]) -> Result<&'a str, String> {
    std::str::from_utf8(bytes).map_err(|_| format!("field {field}: query term is not valid UTF-8"))
}

fn analyze_text<'t>(
    an: &Analyzer,
    field: &str,
    value: &'t [u8],
) -> Result<Vec<Cow<'t, [u8]>>, String> {
    Ok(an.analyze(utf8(field, value)?).collect())
}

/// Terms/And/Or/Phrase rule (spec): every value must analyze to exactly
/// one token.
fn analyze_each(
    schema: &Schema,
    field: &str,
    values: &[Vec<u8>],
    ctx: &str,
) -> Result<Vec<Vec<u8>>, String> {
    match analyzer_for(schema, field)? {
        None => Ok(values.to_vec()),
        Some(an) => values
            .iter()
            .map(|v| {
                let toks = analyze_text(&an, field, v)?;
                match toks.len() {
                    1 => Ok(toks.into_iter().next().unwrap().into_owned()),
                    n => Err(format!(
                        "{ctx}: field {field}: value analyzes to {n} tokens, expected exactly 1"
                    )),
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FieldSpec, Schema};

    fn test_schema() -> Schema {
        let mut s = Schema::new();
        s.add(FieldSpec::text("message").with_analyzer("whitespace|lowercase"));
        s.add(FieldSpec::keyword("level"));
        s
    }

    #[test]
    fn term_is_lowercased_for_analyzed_field() {
        let s = test_schema();
        let q = analyze_query(&Query::term("message", "ERROR"), &s).unwrap();
        assert_eq!(q, Query::term("message", "error"));
    }

    #[test]
    fn term_passes_through_for_plain_field() {
        let s = test_schema();
        let q = analyze_query(&Query::term("level", "ERROR"), &s).unwrap();
        assert_eq!(q, Query::term("level", "ERROR"));
    }

    #[test]
    fn term_with_multiple_tokens_becomes_bool_should() {
        let s = test_schema();
        let q = analyze_query(&Query::term("message", "connection FAILED"), &s).unwrap();
        assert_eq!(
            q,
            Query::bool(vec![
                (Occur::Should, Query::term("message", "connection")),
                (Occur::Should, Query::term("message", "failed")),
            ])
        );
    }

    #[test]
    fn term_with_zero_tokens_is_an_error() {
        // letter tokenizer drops non-letters: "!!!" analyzes to no tokens.
        let mut s = Schema::new();
        s.add(FieldSpec::text("m2").with_analyzer("letter"));
        assert!(analyze_query(&Query::term("m2", "!!!"), &s).is_err());
    }

    #[test]
    fn terms_and_phrase_require_exactly_one_token_per_value() {
        let s = test_schema();
        let q = analyze_query(&Query::terms("message", &["ERROR", "Warn"]), &s).unwrap();
        assert_eq!(q, Query::terms("message", &["error", "warn"]));
        // two-token value is an error
        assert!(analyze_query(&Query::terms("message", &["two words"]), &s).is_err());
        assert!(analyze_query(&Query::phrase("message", &["two words", "x"]), &s).is_err());
        let q = analyze_query(&Query::phrase("message", &["Connection", "Failed"]), &s).unwrap();
        assert_eq!(q, Query::phrase("message", &["connection", "failed"]));
    }

    #[test]
    fn prefix_and_wildcard_are_normalized_not_tokenized() {
        let s = test_schema();
        let q = analyze_query(&Query::prefix("message", "ERR"), &s).unwrap();
        assert_eq!(q, Query::prefix("message", "err"));
        let q = analyze_query(&Query::wildcard("message", "ERR*"), &s).unwrap();
        assert_eq!(q, Query::wildcard("message", "err*"));
    }

    #[test]
    fn bool_recurses_and_other_variants_pass_through() {
        let s = test_schema();
        let q = Query::bool(vec![
            (Occur::Must, Query::term("message", "ERROR")),
            (Occur::MustNot, Query::term("level", "DEBUG")),
        ]);
        let out = analyze_query(&q, &s).unwrap();
        assert_eq!(
            out,
            Query::bool(vec![
                (Occur::Must, Query::term("message", "error")),
                (Occur::MustNot, Query::term("level", "DEBUG")),
            ])
        );
        let q = analyze_query(&Query::MatchAll, &s).unwrap();
        assert_eq!(q, Query::MatchAll);
        let q = analyze_query(&Query::point_range("ts", 1, 2), &s).unwrap();
        assert_eq!(q, Query::point_range("ts", 1, 2));
    }
}
