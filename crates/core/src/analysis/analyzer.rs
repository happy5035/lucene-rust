//! Analyzer: a compiled chain of one tokenizer + ordered filters.
//! Stateless template — `analyze` builds a fresh stream per call
//! (built-in construction is allocation-free), so one `&Analyzer` serves
//! both the index hot path and read-lock query analysis.

use std::borrow::Cow;
use std::sync::Arc;

use super::filter::{LowercaseFilter, TokenFilter};
use super::tokenizer::{KeywordTokens, LetterTokens, Tokenizer, WhitespaceTokens};

/// Factory for externally registered tokenizers (方案 A 逃生舱).
pub type TokenizerFactory =
    Arc<dyn for<'a> Fn(&'a str) -> Box<dyn Tokenizer<'a> + 'a> + Send + Sync>;

/// Which tokenizer a chain starts from. Built-ins dispatch statically.
#[derive(Clone)]
pub enum TokenizerTemplate {
    Whitespace,
    Letter,
    Keyword,
    Custom(TokenizerFactory),
}

/// Compiled filter chain entry: built-ins dispatch statically; custom
/// filters hang off the registry.
#[derive(Clone)]
pub enum FilterKind {
    Lowercase,
    Custom(Arc<dyn TokenFilter>),
}

impl FilterKind {
    fn apply<'a>(&self, token: Cow<'a, [u8]>) -> Option<Cow<'a, [u8]>> {
        match self {
            FilterKind::Lowercase => LowercaseFilter.filter(token),
            FilterKind::Custom(f) => f.filter(token),
        }
    }

    fn normalizes(&self) -> bool {
        match self {
            FilterKind::Lowercase => true,
            FilterKind::Custom(f) => f.normalizes(),
        }
    }
}

enum ActiveTokenizer<'a> {
    Whitespace(WhitespaceTokens<'a>),
    Letter(LetterTokens<'a>),
    Keyword(KeywordTokens<'a>),
    Custom(Box<dyn Tokenizer<'a> + 'a>),
}

impl<'a> ActiveTokenizer<'a> {
    fn next(&mut self) -> Option<&'a [u8]> {
        match self {
            ActiveTokenizer::Whitespace(t) => t.next_token(),
            ActiveTokenizer::Letter(t) => t.next_token(),
            ActiveTokenizer::Keyword(t) => t.next_token(),
            ActiveTokenizer::Custom(t) => t.next_token(),
        }
    }
}

/// A compiled analyzer chain (spec: static enum dispatch + Custom escape
/// hatch). pos_incr is always 1 for built-ins; positions are the running
/// token counter at the call site.
#[derive(Clone)]
pub struct Analyzer {
    pub(crate) tokenizer: TokenizerTemplate,
    pub(crate) filters: Vec<FilterKind>,
}

impl Analyzer {
    /// Full token stream (Lucene `Analyzer.tokenStream`): indexing and
    /// Term/Phrase/Terms query rewriting.
    pub fn analyze<'a, 'b>(&'b self, input: &'a str) -> TokenStream<'a, 'b> {
        let tokenizer = match &self.tokenizer {
            TokenizerTemplate::Whitespace => {
                ActiveTokenizer::Whitespace(WhitespaceTokens::new(input))
            }
            TokenizerTemplate::Letter => ActiveTokenizer::Letter(LetterTokens::new(input)),
            TokenizerTemplate::Keyword => ActiveTokenizer::Keyword(KeywordTokens::new(input)),
            TokenizerTemplate::Custom(f) => ActiveTokenizer::Custom(f(input)),
        };
        TokenStream {
            tokenizer,
            filters: &self.filters,
        }
    }

    /// Normalize channel (Lucene `Analyzer.normalize`): the whole input is
    /// passed through the normalizing filters as a single token — no
    /// tokenization. Used for Prefix/Wildcard query rewriting.
    pub fn normalize<'a>(&self, input: &'a str) -> Cow<'a, str> {
        let mut tok: Cow<'a, [u8]> = Cow::Borrowed(input.as_bytes());
        for f in &self.filters {
            if f.normalizes() {
                match f.apply(tok) {
                    Some(t) => tok = t,
                    None => return Cow::Owned(String::new()),
                }
            }
        }
        match tok {
            Cow::Borrowed(_) => Cow::Borrowed(input),
            Cow::Owned(bytes) => Cow::Owned(String::from_utf8_lossy(&bytes).into_owned()),
        }
    }
}

pub struct TokenStream<'a, 'b> {
    tokenizer: ActiveTokenizer<'a>,
    filters: &'b [FilterKind],
}

impl<'a> Iterator for TokenStream<'a, '_> {
    type Item = Cow<'a, [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        'outer: while let Some(raw) = self.tokenizer.next() {
            let mut tok = Cow::Borrowed(raw);
            for f in self.filters {
                tok = match f.apply(tok) {
                    Some(t) => t,
                    None => continue 'outer,
                };
            }
            return Some(tok);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze_to_vec(an: &Analyzer, input: &str) -> Vec<Vec<u8>> {
        an.analyze(input).map(|t| t.into_owned()).collect()
    }

    #[test]
    fn whitespace_lowercase_normalizes_case() {
        let an = Analyzer {
            tokenizer: TokenizerTemplate::Whitespace,
            filters: vec![FilterKind::Lowercase],
        };
        assert_eq!(
            analyze_to_vec(&an, "ERROR Failed error"),
            vec![b"error".to_vec(), b"failed".to_vec(), b"error".to_vec()]
        );
    }

    #[test]
    fn no_filter_passes_bytes_through() {
        let an = Analyzer {
            tokenizer: TokenizerTemplate::Whitespace,
            filters: vec![],
        };
        assert_eq!(
            analyze_to_vec(&an, "ERROR error"),
            vec![b"ERROR".to_vec(), b"error".to_vec()]
        );
    }

    #[test]
    fn lowercase_unicode_semantics() {
        // Non-ASCII uppercase goes through Unicode to_lowercase
        // (Lucene LowerCaseFilter semantics).
        let an = Analyzer {
            tokenizer: TokenizerTemplate::Whitespace,
            filters: vec![FilterKind::Lowercase],
        };
        assert_eq!(analyze_to_vec(&an, "ÄBC"), vec!["äbc".as_bytes().to_vec()]);
    }

    #[test]
    fn normalize_applies_filters_without_tokenizing() {
        let an = Analyzer {
            tokenizer: TokenizerTemplate::Whitespace,
            filters: vec![FilterKind::Lowercase],
        };
        // whole input as one token, lowercased — for Prefix/Wildcard
        assert_eq!(an.normalize("Err*"), std::borrow::Cow::Borrowed("err*"));
        assert_eq!(an.normalize("err*"), std::borrow::Cow::Borrowed("err*"));
    }

    #[test]
    fn lowercase_ascii_fast_path_is_borrowed() {
        // zero-alloc guarantee: pure-ASCII lowercase token stays borrowed
        let an = Analyzer {
            tokenizer: TokenizerTemplate::Whitespace,
            filters: vec![FilterKind::Lowercase],
        };
        let toks: Vec<_> = an.analyze("ok").collect();
        assert!(matches!(toks[0], std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn lowercase_ascii_uppercase_single_alloc() {
        // ASCII with uppercase: one allocation via to_ascii_lowercase,
        // no Unicode tables (ERROR, NullPointerException path)
        let an = Analyzer {
            tokenizer: TokenizerTemplate::Whitespace,
            filters: vec![FilterKind::Lowercase],
        };
        let toks: Vec<_> = an.analyze("ERROR").collect();
        assert_eq!(&*toks[0], b"error");
        assert!(matches!(toks[0], std::borrow::Cow::Owned(_)));
    }
}
