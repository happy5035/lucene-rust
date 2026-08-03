//! Token filters: one token in, one token out (or dropped). Filters are
//! stateless and shared by reference, so `&Analyzer` works under a read
//! lock (query-side analysis) as well as in the write hot path.

use std::borrow::Cow;

/// One token in, one token out; `None` drops the token (reserved for
/// future stop filters — not implemented this round).
pub trait TokenFilter: Send + Sync {
    fn filter<'a>(&self, token: Cow<'a, [u8]>) -> Option<Cow<'a, [u8]>>;
    /// Whether this filter participates in the normalize channel
    /// (Lucene `MultiTermAwareComponent` semantics; `LowerCaseFilter` does).
    fn normalizes(&self) -> bool;
}

/// Lucene `LowerCaseFilter` analog. ASCII fast path: a pure-ASCII token
/// with no uppercase bytes passes through borrowed (zero allocation);
/// anything else goes through Unicode `to_lowercase`.
pub struct LowercaseFilter;

impl TokenFilter for LowercaseFilter {
    fn filter<'a>(&self, token: Cow<'a, [u8]>) -> Option<Cow<'a, [u8]>> {
        if token.is_ascii() && !token.iter().any(u8::is_ascii_uppercase) {
            return Some(token);
        }
        Some(Cow::Owned(
            String::from_utf8_lossy(&token)
                .into_owned()
                .to_lowercase()
                .into_bytes(),
        ))
    }

    fn normalizes(&self) -> bool {
        true
    }
}
