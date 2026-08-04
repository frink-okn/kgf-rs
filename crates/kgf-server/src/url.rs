//! Reading and writing the URLs of doc 03 §3.2's space.
//!
//! Separate from the router because both directions have callers that are not
//! the router: [`crate::html`] writes links into pages, and unit 14's
//! operations read the same query parameters this module parses.
//!
//! A dataset or version label is a *directory name* on the server, so it can
//! hold characters that mean something in a URL — a `?` would turn the rest of
//! the path into a query string, a `#` would truncate it at the client, and a
//! `%` would make the next two characters an escape.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use crate::envelope::{ErrorCode, Problem, reflected};

/// RFC 3986 §2.3's `unreserved` set, kept; everything else escaped.
///
/// Conservative: this escapes the `sub-delims` a path segment is technically
/// allowed to carry. There is nothing to gain from a prettier URL here, and
/// every character left unescaped is one whose interaction with a proxy, a
/// cache key or a client's own parser has to be reasoned about.
const RESERVED_IN_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encode one path segment (RFC 3986 §3.3).
pub fn encode_segment(segment: &str) -> String {
    utf8_percent_encode(segment, RESERVED_IN_SEGMENT).to_string()
}

/// Percent-encode one query parameter name or value.
///
/// The same conservative set: a query value must escape `&` and `=` to stay one
/// value, `%` to stay literal, and `+` because [`decode_component`] reads a bare
/// one as a space. Everything else the set escapes is escaped for free.
pub fn encode_value(value: &str) -> String {
    utf8_percent_encode(value, RESERVED_IN_SEGMENT).to_string()
}

/// The base of a bundle version: `/{dataset}/v/{version}/`.
pub fn bundle_base(dataset: &str, version: &str) -> String {
    format!(
        "/{}/v/{}/",
        encode_segment(dataset),
        encode_segment(version)
    )
}

/// An operation under a bundle version: `/{dataset}/v/{version}/{operation}`.
pub fn operation(dataset: &str, version: &str, operation: &str) -> String {
    format!("{}{operation}", bundle_base(dataset, version))
}

/// A dataset descriptor: `/{dataset}`.
pub fn dataset(name: &str) -> String {
    format!("/{}", encode_segment(name))
}

// ---------------------------------------------------------------------------
// Reading a query string
// ---------------------------------------------------------------------------

/// A request's query parameters, each appearing at most once.
///
/// Doc 03 §3.6.1 makes "a parameter is missing, **repeated**, or unparseable" a
/// `malformed_request`, and this is where repetition is caught. It has to be
/// caught: the alternative is a rule about which of `?limit=10&limit=99999`
/// wins, which differs between the server, an intermediary, and the client's
/// own URL builder — so a request that looks capped to one of them is
/// uncapped to another. Refusing costs a client nothing, since no KGF
/// parameter is a list.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Params(std::collections::BTreeMap<String, String>);

impl Params {
    /// Parse a raw query string, percent-decoding names and values.
    pub fn parse(query: Option<&str>) -> Result<Self, Problem> {
        let mut params = std::collections::BTreeMap::new();
        let Some(query) = query.filter(|query| !query.is_empty()) else {
            return Ok(Self(params));
        };

        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let name = decode_component(name).ok_or_else(|| malformed(pair))?;
            let value = decode_component(value).ok_or_else(|| malformed(pair))?;
            if params.insert(name.clone(), value).is_some() {
                return Err(Problem::new(
                    ErrorCode::MalformedRequest,
                    format!(
                        "parameter {:?} appears more than once; \
                         send it once, since no KGF parameter takes a list",
                        reflected(&name)
                    ),
                ));
            }
        }
        Ok(Self(params))
    }

    /// One parameter's value.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Every parameter name the request carried, in sorted order.
    pub fn names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// The same parameters with `name` set to `value`, replacing any it had.
    #[must_use]
    pub fn with(&self, name: &str, value: &str) -> Self {
        let mut params = self.0.clone();
        params.insert(name.to_owned(), value.to_owned());
        Self(params)
    }

    /// The same parameters without `name`.
    #[must_use]
    pub fn without(&self, name: &str) -> Self {
        let mut params = self.0.clone();
        params.remove(name);
        Self(params)
    }

    /// The same parameters without empty values under any of `names`.
    ///
    /// Used after a typed GET request has named the controls for which omission
    /// selects a default. It is deliberately not part of generic parsing: empty
    /// required, unknown, cursor, representation, and selection parameters must
    /// remain visible so their own parsers can reject them.
    #[must_use]
    pub fn without_empty(&self, names: &[&str]) -> Self {
        let mut params = self.0.clone();
        params.retain(|name, value| !value.is_empty() || !names.contains(&name.as_str()));
        Self(params)
    }

    /// Whether the request carried any parameters.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Re-encode as a query string, for a link back into this API.
    ///
    /// **Normalized**: names are sorted and every value is escaped by the same
    /// rule, so two requests that differ only in parameter order produce one
    /// URL. Doc 03 §3.6 asks for exactly that — "normalized parameter ordering
    /// documented so caches hit" — and a next-page link is where this server
    /// gets to choose the spelling.
    pub fn to_query(&self) -> String {
        self.0
            .iter()
            .map(|(name, value)| format!("{}={}", encode_value(name), encode_value(value)))
            .collect::<Vec<_>>()
            .join("&")
    }
}

fn malformed(pair: &str) -> Problem {
    Problem::new(
        ErrorCode::MalformedRequest,
        format!(
            "query parameter {:?} is not valid percent-encoded UTF-8 (RFC 3986 §2.1); \
             escape `%` as `%25`",
            reflected(pair)
        ),
    )
}

/// Decode one `application/x-www-form-urlencoded` name or value.
///
/// `None` on a truncated or non-hex escape, or on bytes that are not UTF-8.
///
/// **Stricter than the ecosystem's default, deliberately.** `form_urlencoded`
/// and everything built on it (including `serde_urlencoded`, and so axum's own
/// `Query` extractor) decode with `decode_utf8_lossy` and pass a malformed
/// escape through as literal text. Both are wrong here in the same way: a term
/// parameter that lost a byte to U+FFFD is a *different term*, which resolves
/// against the dictionary, misses, and is answered "no rows" — a wrong answer
/// where an error was available. Refusing costs a correct client nothing.
///
/// The decoding itself is `percent-encoding`'s; what is added is the shape
/// check and the non-lossy UTF-8.
pub fn decode_component(text: &str) -> Option<String> {
    // The `+`-for-space convention of form encoding, applied before decoding so
    // that an escaped `%2B` stays a plus. HTML forms produce it, so a browser's
    // address bar can, and doc 03 §3.3 lists `+` among the characters a term
    // must escape — which is only necessary if a bare one means something else.
    let text = if text.contains('+') {
        std::borrow::Cow::Owned(text.replace('+', " "))
    } else {
        std::borrow::Cow::Borrowed(text)
    };
    escapes_are_well_formed(&text).then_some(())?;
    percent_decode_str(&text)
        .decode_utf8()
        .ok()
        .map(std::borrow::Cow::into_owned)
}

/// Whether every `%` in `text` begins a complete two-hex-digit escape.
fn escapes_are_well_formed(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    while let Some(offset) = bytes[index..].iter().position(|byte| *byte == b'%') {
        let start = index + offset;
        match bytes.get(start + 1..start + 3) {
            Some(escape) if escape.iter().all(u8::is_ascii_hexdigit) => index = start + 3,
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_cannot_escape_its_segment() {
        // Directory names, so all of these are possible on disk.
        assert_eq!(encode_segment("2026-06-01"), "2026-06-01");
        assert_eq!(encode_segment("a b"), "a%20b");
        assert_eq!(encode_segment("q?x=1"), "q%3Fx%3D1");
        assert_eq!(encode_segment("frag#ment"), "frag%23ment");
        assert_eq!(encode_segment("100%"), "100%25");
        assert_eq!(encode_segment("a/b"), "a%2Fb");
        // Non-ASCII goes out as UTF-8 octets, which is what RFC 3987 requires.
        assert_eq!(encode_segment("é"), "%C3%A9");
    }

    #[test]
    fn the_url_space_is_doc_03_s() {
        assert_eq!(dataset("tox"), "/tox");
        assert_eq!(bundle_base("tox", "2026-06-01"), "/tox/v/2026-06-01/");
        assert_eq!(
            operation("tox", "2026-06-01", "manifest"),
            "/tox/v/2026-06-01/manifest"
        );
        assert_eq!(bundle_base("a b", "c?d"), "/a%20b/v/c%3Fd/");
    }

    #[test]
    fn a_repeated_parameter_is_refused_rather_than_resolved() {
        // `?limit=10&limit=99999` has no right answer: server, proxy and client
        // URL builder each pick a different one, so a request that looks capped
        // to one is uncapped to another.
        let repeated = Params::parse(Some("limit=10&limit=99999")).unwrap_err();
        assert_eq!(repeated.code(), ErrorCode::MalformedRequest);
        assert!(
            serde_json::to_value(&repeated).unwrap()["detail"]
                .as_str()
                .unwrap()
                .contains("limit")
        );
    }

    #[test]
    fn parameters_are_percent_decoded() {
        let params = Params::parse(Some(
            "s=%3Chttp%3A%2F%2Fex.org%2Fa%3E&o=%22a+b%22%40en&bare",
        ))
        .unwrap();
        assert_eq!(params.get("s"), Some("<http://ex.org/a>"));
        assert_eq!(params.get("o"), Some("\"a b\"@en"));
        assert_eq!(params.get("bare"), Some(""));
        assert_eq!(params.get("absent"), None);

        assert_eq!(Params::parse(None).unwrap().get("s"), None);
        assert_eq!(Params::parse(Some("")).unwrap().get("s"), None);
    }

    #[test]
    fn empty_values_are_removed_only_when_the_caller_names_them() {
        let params = Params::parse(Some("s=&p=ex%3Ap&limit=")).unwrap();
        let normalized = params.without_empty(&["s", "p", "o"]);
        assert_eq!(normalized.get("s"), None);
        assert_eq!(normalized.get("p"), Some("ex:p"));
        assert_eq!(normalized.get("limit"), Some(""));
    }

    #[test]
    fn a_broken_escape_is_an_error_and_not_a_replacement_character() {
        // U+FFFD in a term parameter would be looked up, missed, and answered
        // "no such term" — a wrong answer where an error was available.
        for query in ["s=%", "s=%zz", "s=%C3%28", "s=%2"] {
            assert_eq!(
                Params::parse(Some(query)).unwrap_err().code(),
                ErrorCode::MalformedRequest,
                "{query}"
            );
        }
        assert_eq!(decode_component("%C3%A9"), Some("é".to_owned()));
    }
}
