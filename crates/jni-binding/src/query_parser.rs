use serde::Deserialize;
use rustlucene_core::search::query::{Occur, Query};

#[derive(Deserialize)]
pub struct SearchRequest {
    pub query: QuerySpec,
    pub sort: Option<SortSpec>,
    #[serde(default = "default_top_n")]
    pub top_n: usize,
}

fn default_top_n() -> usize {
    10
}

#[derive(Deserialize)]
pub struct SortSpec {
    pub field: String,
    #[serde(default = "default_desc")]
    pub order: String,
}

fn default_desc() -> String {
    "desc".to_string()
}

#[derive(Deserialize)]
pub struct ClauseSpec {
    pub occur: String,
    pub query: QuerySpec,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuerySpec {
    Term { field: String, value: String },
    Bool { clauses: Vec<ClauseSpec> },
    Range { field: String, low: i64, high: i64 },
    Prefix { field: String, value: String },
    Wildcard { field: String, value: String },
    Phrase { field: String, terms: Vec<String> },
    Terms { field: String, values: Vec<String> },
    MatchAll,
}

impl SearchRequest {
    pub fn to_query(&self) -> Result<Query, String> {
        spec_to_query(&self.query)
    }

    pub fn sort_field(&self) -> Option<(&str, bool)> {
        self.sort.as_ref().map(|s| {
            let desc = s.order != "asc";
            (s.field.as_str(), desc)
        })
    }
}

fn spec_to_query(spec: &QuerySpec) -> Result<Query, String> {
    match spec {
        QuerySpec::Term { field, value } => Ok(Query::term(field, value)),
        QuerySpec::Terms { field, values } => {
            let refs: Vec<&str> = values.iter().map(String::as_str).collect();
            Ok(Query::terms(field, &refs))
        }
        QuerySpec::MatchAll => Ok(Query::MatchAll),
        QuerySpec::Range { field, low, high } => Ok(Query::point_range(field, *low, *high)),
        QuerySpec::Prefix { field, value } => Ok(Query::prefix(field, value)),
        QuerySpec::Wildcard { field, value } => Ok(Query::wildcard(field, value)),
        QuerySpec::Phrase { field, terms } => {
            let refs: Vec<&str> = terms.iter().map(String::as_str).collect();
            Ok(Query::phrase(field, &refs))
        }
        QuerySpec::Bool { clauses } => {
            let mut out = Vec::with_capacity(clauses.len());
            for c in clauses {
                let occur = match c.occur.as_str() {
                    "must" => Occur::Must,
                    "should" => Occur::Should,
                    "must_not" => Occur::MustNot,
                    other => return Err(format!("unknown occur: {other}")),
                };
                out.push((occur, spec_to_query(&c.query)?));
            }
            Ok(Query::bool(out))
        }
    }
}

pub fn parse_search_request(json: &[u8]) -> Result<SearchRequest, String> {
    serde_json::from_slice(json).map_err(|e| format!("query parse error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_term_query() {
        let json = br#"{"query":{"type":"term","field":"level","value":"ERROR"},"top_n":50}"#;
        let req = parse_search_request(json).unwrap();
        assert_eq!(req.top_n, 50);
        assert!(req.sort.is_none());
        let q = req.to_query().unwrap();
        assert_eq!(q, Query::term("level", "ERROR"));
    }

    #[test]
    fn parse_bool_with_sort() {
        let json = br#"{
            "query":{"type":"bool","clauses":[
                {"occur":"must","query":{"type":"term","field":"level","value":"ERROR"}},
                {"occur":"must","query":{"type":"range","field":"ts","low":100,"high":200}},
                {"occur":"must_not","query":{"type":"term","field":"host","value":"test"}}
            ]},
            "sort":{"field":"ts","order":"desc"},
            "top_n":100
        }"#;
        let req = parse_search_request(json).unwrap();
        assert_eq!(req.top_n, 100);
        let (field, desc) = req.sort_field().unwrap();
        assert_eq!(field, "ts");
        assert!(desc);
        let q = req.to_query().unwrap();
        match q {
            Query::Bool { clauses } => assert_eq!(clauses.len(), 3),
            _ => panic!("expected Bool"),
        }
    }

    #[test]
    fn parse_match_all_default_top_n() {
        let json = br#"{"query":{"type":"match_all"}}"#;
        let req = parse_search_request(json).unwrap();
        assert_eq!(req.top_n, 10); // default
        assert_eq!(req.to_query().unwrap(), Query::MatchAll);
    }

    #[test]
    fn parse_phrase_prefix_wildcard() {
        let json = br#"{"query":{"type":"phrase","field":"msg","terms":["hello","world"]}}"#;
        let req = parse_search_request(json).unwrap();
        assert_eq!(req.to_query().unwrap(), Query::phrase("msg", &["hello", "world"]));

        let json = br#"{"query":{"type":"prefix","field":"path","value":"/api"}}"#;
        let req = parse_search_request(json).unwrap();
        assert_eq!(req.to_query().unwrap(), Query::prefix("path", "/api"));

        let json = br#"{"query":{"type":"wildcard","field":"tid","value":"req-*"}}"#;
        let req = parse_search_request(json).unwrap();
        assert_eq!(req.to_query().unwrap(), Query::wildcard("tid", "req-*"));
    }

    #[test]
    fn parse_terms_query() {
        let json = br#"{"query":{"type":"terms","field":"level","values":["ERROR","WARN"]},"top_n":5}"#;
        let req = parse_search_request(json).unwrap();
        assert_eq!(req.to_query().unwrap(), Query::terms("level", &["ERROR", "WARN"]));
    }

    #[test]
    fn terms_nested_in_bool() {
        let json = br#"{"query":{"type":"bool","clauses":[
            {"occur":"must","query":{"type":"terms","field":"level","values":["ERROR"]}},
            {"occur":"must","query":{"type":"match_all"}}
        ]}}"#;
        let req = parse_search_request(json).unwrap();
        match req.to_query().unwrap() {
            Query::Bool { clauses } => {
                assert_eq!(clauses.len(), 2);
                assert_eq!(clauses[0].1, Query::terms("level", &["ERROR"]));
            }
            _ => panic!("expected Bool"),
        }
    }

    #[test]
    fn parse_invalid_json() {
        assert!(parse_search_request(b"not json").is_err());
        assert!(parse_search_request(br#"{"query":{"type":"unknown"}}"#).is_err());
    }

    #[test]
    fn parse_asc_sort() {
        let json = br#"{"query":{"type":"match_all"},"sort":{"field":"ts","order":"asc"}}"#;
        let req = parse_search_request(json).unwrap();
        let (_, desc) = req.sort_field().unwrap();
        assert!(!desc);
    }
}
