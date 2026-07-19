/// Tokenizes like Lucene's `WhitespaceTokenizer` for the log corpora we
/// target: splits on ASCII whitespace, no lowercasing, terms keep original
/// bytes.
///
/// (`WhitespaceTokenizer` splits on `Character.isWhitespace`;
/// `split_ascii_whitespace` agrees with it on ASCII input and differs only
/// for exotic Unicode spaces (U+00A0, U+2028, ...) — treated as term bytes
/// here, which is the behavior we want for log messages.)
pub struct WhitespaceTokens<'a> {
    inner: std::str::SplitAsciiWhitespace<'a>,
}

impl<'a> WhitespaceTokens<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            inner: text.split_ascii_whitespace(),
        }
    }
}

impl<'a> Iterator for WhitespaceTokens<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_whitespace() {
        let toks: Vec<&str> = WhitespaceTokens::new("  ab  c\td ef\n").collect();
        assert_eq!(toks, vec!["ab", "c", "d", "ef"]);
    }
}
