//! Name registry: built-in component names plus user-registered custom
//! components (spec §配置语法). Java/JNI only ever passes the spec string;
//! everything resolves here.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use super::analyzer::{Analyzer, FilterKind, TokenizerFactory, TokenizerTemplate};
use super::filter::TokenFilter;

fn custom_tokenizers() -> &'static RwLock<HashMap<String, TokenizerFactory>> {
    static REG: OnceLock<RwLock<HashMap<String, TokenizerFactory>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

fn custom_filters() -> &'static RwLock<HashMap<String, Arc<dyn TokenFilter>>> {
    static REG: OnceLock<RwLock<HashMap<String, Arc<dyn TokenFilter>>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Registers a custom tokenizer under `name` (spec string first component).
/// Must be called before any schema referencing the name is parsed.
/// Built-in names cannot be overridden.
pub fn register_tokenizer(name: &str, factory: TokenizerFactory) {
    assert!(
        !matches!(name, "whitespace" | "letter" | "keyword"),
        "cannot override built-in tokenizer: {name}"
    );
    custom_tokenizers()
        .write()
        .unwrap()
        .insert(name.to_string(), factory);
}

/// Registers a custom token filter under `name` (spec string 2nd+ component).
/// Built-in names cannot be overridden.
pub fn register_filter(name: &str, filter: Arc<dyn TokenFilter>) {
    assert!(
        !matches!(name, "lowercase"),
        "cannot override built-in token filter: {name}"
    );
    custom_filters()
        .write()
        .unwrap()
        .insert(name.to_string(), filter);
}

impl Analyzer {
    /// Parses `"tokenizer|filter|filter..."`: first component must be a
    /// tokenizer, the rest filters; chain order is declaration order.
    pub fn parse(spec: &str) -> Result<Analyzer, String> {
        let mut parts = spec.split('|');
        let tok = parts
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "empty analyzer spec".to_string())?;
        let tokenizer = match tok {
            "whitespace" => TokenizerTemplate::Whitespace,
            "letter" => TokenizerTemplate::Letter,
            "keyword" => TokenizerTemplate::Keyword,
            other => custom_tokenizers()
                .read()
                .unwrap()
                .get(other)
                .cloned()
                .map(TokenizerTemplate::Custom)
                .ok_or_else(|| format!("unknown tokenizer: {other}"))?,
        };
        let mut filters = Vec::new();
        for name in parts {
            let f = match name {
                "lowercase" => FilterKind::Lowercase,
                other => custom_filters()
                    .read()
                    .unwrap()
                    .get(other)
                    .cloned()
                    .map(FilterKind::Custom)
                    .ok_or_else(|| format!("unknown token filter: {other}"))?,
            };
            filters.push(f);
        }
        Ok(Analyzer { tokenizer, filters })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_builtin_chains() {
        let an = Analyzer::parse("whitespace|lowercase").unwrap();
        assert_eq!(an.normalize("ABC"), std::borrow::Cow::Borrowed("abc"));
        let an = Analyzer::parse("letter").unwrap();
        let toks: Vec<_> = an.analyze("a1b").map(|t| t.into_owned()).collect();
        assert_eq!(toks, vec![b"a".to_vec(), b"b".to_vec()]);
        let an = Analyzer::parse("keyword|lowercase").unwrap();
        let toks: Vec<_> = an.analyze("Hello World").map(|t| t.into_owned()).collect();
        assert_eq!(toks, vec![b"hello world".to_vec()]);
    }

    #[test]
    fn rejects_unknown_names_and_empty_specs() {
        assert!(Analyzer::parse("").is_err());
        assert!(Analyzer::parse("nosuchtok").is_err());
        assert!(Analyzer::parse("whitespace|nosuchfilter").is_err());
        // a tokenizer name in filter position is unknown there
        assert!(Analyzer::parse("whitespace|letter").is_err());
    }

    #[test]
    fn custom_components_register_by_name() {
        struct Reverse;
        impl crate::analysis::TokenFilter for Reverse {
            fn filter<'a>(&self, t: std::borrow::Cow<'a, [u8]>) -> Option<std::borrow::Cow<'a, [u8]>> {
                let mut v = t.into_owned();
                v.reverse();
                Some(std::borrow::Cow::Owned(v))
            }
            fn normalizes(&self) -> bool {
                false
            }
        }
        register_filter("reverse", std::sync::Arc::new(Reverse));
        let an = Analyzer::parse("whitespace|reverse").unwrap();
        let toks: Vec<_> = an.analyze("abc").map(|t| t.into_owned()).collect();
        assert_eq!(toks, vec![b"cba".to_vec()]);
        // normalizes() = false: normalize channel skips it
        assert_eq!(an.normalize("abc"), std::borrow::Cow::Borrowed("abc"));
    }

    #[test]
    fn builtin_tokenizer_names_cannot_be_overridden() {
        for name in ["whitespace", "letter", "keyword"] {
            let result = std::panic::catch_unwind(|| {
                register_tokenizer(name, std::sync::Arc::new(|_| {
                    Box::new(crate::analysis::WhitespaceTokens::new(""))
                }));
            });
            assert!(result.is_err(), "registering built-in tokenizer {name} must be rejected");
        }
    }

    #[test]
    fn builtin_filter_names_cannot_be_overridden() {
        struct Noop;
        impl crate::analysis::TokenFilter for Noop {
            fn filter<'a>(&self, t: std::borrow::Cow<'a, [u8]>) -> Option<std::borrow::Cow<'a, [u8]>> {
                Some(t)
            }
            fn normalizes(&self) -> bool {
                false
            }
        }
        let result = std::panic::catch_unwind(|| {
            register_filter("lowercase", std::sync::Arc::new(Noop));
        });
        assert!(result.is_err(), "registering built-in filter lowercase must be rejected");
    }
}
