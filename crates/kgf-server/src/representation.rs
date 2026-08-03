//! Which serialization a response carries, and how it may be cached.
//!
//! Two things doc 03 ties together and this module keeps together: one URL
//! serves many formats (§3.1), so the choice of format is part of the response's
//! identity — which is why §3.6 makes `ETag` representation-specific and pairs
//! it with `Vary: Accept`. An `ETag` that ignored the representation would let a
//! cache answer a CSV request from a JSON entry.
//!
//! Nothing here touches HTTP machinery; it takes header *values* as strings and
//! returns values to send. That keeps the negotiation rules testable without a
//! request, and it is why the `Accept` grammar below is implemented rather than
//! approximated: serving JSON to a client that asked for `text/csv` is a silent
//! wrong answer of exactly the kind this project refuses, and "does the header
//! contain `json`" is how that happens.
//!
//! # Two representations, and why the order matters
//!
//! Every route answers JSON *and* HTML, and which one a client gets is decided
//! by `Accept` alone — no user-agent sniffing, no separate `/html` URL space.
//! It works because the two clients ask differently: a browser navigating sends
//! `text/html` at `q=1` with `*/*;q=0.8` behind it, while `curl`, a library, and
//! an agent send `*/*` or nothing.
//!
//! So the rule falls out of RFC 9110 §12.5.1 plus one decision — **JSON is
//! listed first, and ties go to the first offer**. A browser's exact `text/html`
//! beats its own `*/*;q=0.8`, and everything else ties at `*/*` and takes JSON.
//! That is the LDF/QPF behaviour (a page in the browser, data from `curl`) with
//! the machine-readable form as the default rather than the exception, which is
//! the right way round for an API doc 01 argues should be agent-friendly.
//!
//! The CSV/Parquet/Arrow/RDF serializations arrive with M2. Because the choice
//! is an enum, adding one is a change the compiler routes through every place a
//! format is named — including every resource's HTML rendering, which is why
//! [`Resource`](crate::html::Resource) exists rather than a match per handler.

use headers::{CacheControl, ETag};
use mediatype::{MediaType, MediaTypeBuf, ReadParams, names};
use sha2::{Digest, Sha256};
use std::time::Duration;

use crate::envelope::{ErrorCode, Problem, reflected};

/// A serialization this server can produce.
///
/// Declaration order is the server's own preference, which RFC 9110 §12.5.1
/// leaves to it: [`negotiate`] breaks ties by taking the earliest variant, so
/// `Accept: */*` is answered with JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Representation {
    /// KGF's own JSON envelope (doc 03 §3.4.1), and the manifest as published.
    Json,
    /// A page, for reading the same resource in a browser.
    Html,
}

impl Representation {
    /// Every representation, in the server's preference order.
    pub const ALL: &'static [Representation] = &[Representation::Json, Representation::Html];

    /// The media type `Accept` names it by, without parameters.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Json => "application/json",
            Self::Html => "text/html",
        }
    }

    /// The same, parsed, for matching against a request's media ranges.
    fn media_range(self) -> MediaType<'static> {
        match self {
            Self::Json => MediaType::new(names::APPLICATION, names::JSON),
            Self::Html => MediaType::new(names::TEXT, names::HTML),
        }
    }

    /// The full `Content-Type`, parameters included.
    ///
    /// JSON needs none — RFC 8259 fixes UTF-8 and defines no `charset`
    /// parameter — while `text/html` without one is decoded by a browser's
    /// guess, which for a page containing a Turkish IRI is a wrong guess.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Json => "application/json",
            Self::Html => "text/html; charset=utf-8",
        }
    }

    /// The token `format=` names it by (doc 03 §3.1).
    ///
    /// Also the discriminator in an [`ETag`], which is why it must be stable and
    /// header-safe: changing it silently invalidates every cached entry, and a
    /// token containing a quote would produce an unparseable `ETag`.
    pub fn token(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Html => "html",
        }
    }

    /// The representation `format=token` names, if this server has it.
    fn from_token(token: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|candidate| candidate.token() == token)
    }

    /// The representation to render an error in.
    ///
    /// Separate from [`negotiate`] because it cannot fail: a request whose
    /// `Accept` nothing satisfies still has to be told so, and answering a
    /// negotiation failure with another negotiation failure is a loop. So this
    /// takes the client's preference where there is one and JSON otherwise —
    /// which is also what RFC 9457 expects, since `application/problem+json` is
    /// the media type it defines.
    pub fn for_problem(format: Option<&str>, accept: Option<&str>) -> Self {
        negotiate(format, accept, Self::ALL).unwrap_or(Self::Json)
    }
}

/// Choose the representation to answer with (doc 03 §3.1).
///
/// `offered` is the operation's own list, most-preferred first; `/manifest`
/// offers JSON alone, while unit 14's operations will offer several.
///
/// **`format=` wins outright when present.** It is the client naming a
/// serialization, not expressing a preference, and §3.6.1 gives the two failures
/// different codes and statuses — `unsupported_format` 400 for a `format=` this
/// operation does not offer, `not_acceptable` 406 for an `Accept` nothing
/// satisfies — which only makes sense if they are evaluated separately. So a
/// request with both gets `format=`, and an `Accept` that contradicts it is
/// ignored rather than turned into a 406 the client cannot act on.
///
/// An absent or empty `Accept` is `*/*`, per RFC 9110 §12.5.1.
pub fn negotiate(
    format: Option<&str>,
    accept: Option<&str>,
    offered: &[Representation],
) -> Result<Representation, Problem> {
    debug_assert!(!offered.is_empty(), "an operation must offer a format");

    if let Some(format) = format {
        return Representation::from_token(format)
            .filter(|representation| offered.contains(representation))
            .ok_or_else(|| {
                Problem::new(
                    ErrorCode::UnsupportedFormat,
                    format!(
                        "format={} is not a serialization this operation offers; it offers {}",
                        reflected(format),
                        tokens(offered)
                    ),
                )
            });
    }

    let accept = accept.unwrap_or("*/*").trim();
    let ranges = parse_accept(if accept.is_empty() { "*/*" } else { accept });

    // Best acceptable, ties to the server's preference: `max_by_key` keeps the
    // *last* maximum, so the list is walked in reverse to keep the first.
    offered
        .iter()
        .rev()
        .filter_map(|representation| {
            acceptability(&ranges, &representation.media_range())
                .map(|quality| (quality, *representation))
        })
        .max_by_key(|(quality, _)| *quality)
        .map(|(_, representation)| representation)
        .ok_or_else(|| {
            Problem::new(
                ErrorCode::NotAcceptable,
                format!(
                    "no representation satisfies Accept: {}; this operation offers {}",
                    reflected(accept),
                    media_types(offered)
                ),
            )
        })
}

fn tokens(offered: &[Representation]) -> String {
    join(offered.iter().map(|representation| representation.token()))
}

fn media_types(offered: &[Representation]) -> String {
    join(
        offered
            .iter()
            .map(|representation| representation.media_type()),
    )
}

fn join<'a>(items: impl Iterator<Item = &'a str>) -> String {
    items.collect::<Vec<_>>().join(", ")
}

/// One media range from an `Accept` header.
///
/// The media type is parsed by `mediatype`, which owns RFC 9110 §8.3's grammar
/// — case folding, the optional whitespace around `;`, quoted parameter values.
/// `mime` was here first and was wrong for it: `mime` rejects `text/html ;
/// q=0.5`, which §5.6.3's `OWS ";" OWS` permits, so a legal range was dropped
/// and the response was negotiated from what remained.
///
/// What is *not* in any library in this stack is everything below: neither axum
/// nor `tower-http` parses `Accept` at all, and the `headers` crate stops short
/// of a type for it, so the selection rule is written here against RFC 9110
/// §12.5.1 and checked against an independent implementation in
/// `tests/negotiation.rs`.
struct MediaRange {
    mime: MediaTypeBuf,
    /// Quality scaled to thousandths, so ranges compare as integers: `q` has at
    /// most three decimal digits (RFC 9110 §12.4.2), and floats would make the
    /// ordering below depend on rounding.
    quality: u32,
}

impl MediaRange {
    /// How specifically this range names `media_type`, or `None` if it does not.
    ///
    /// RFC 9110 §12.5.1: the most specific matching range decides, and only then
    /// does its `q` apply. Getting this backwards makes `Accept: */*;q=0.1,
    /// application/json;q=0` serve JSON at q=0.1 rather than refusing.
    fn specificity(&self, media_type: &MediaType<'_>) -> Option<u8> {
        let (type_, subtype) = (self.mime.ty(), self.mime.subty());
        let (any_type, any_subtype) = (type_ == names::_STAR, subtype == names::_STAR);
        match (any_type, any_subtype) {
            (true, true) => Some(0),
            (false, true) if type_ == media_type.ty => Some(1),
            (false, false) if type_ == media_type.ty && subtype == media_type.subty => Some(2),
            // `*/json` is not a media range: RFC 9110 §12.5.1's grammar admits
            // `*/*`, `type/*` and `type/subtype`, and nothing else.
            _ => None,
        }
    }
}

fn parse_accept(header: &str) -> Vec<MediaRange> {
    split_ranges(header)
        .into_iter()
        .filter_map(|entry| {
            let mime: MediaTypeBuf = entry.trim().parse().ok()?;
            let quality = mime
                .get_param(names::Q)
                // An unparseable `q` is an ignored parameter, not a rejection
                // (RFC 9110 §12.4.2), which for the default 1 means the range
                // still applies.
                .and_then(|quality: mediatype::Value<'_>| {
                    quality_thousandths(&quality.unquoted_str())
                })
                .unwrap_or(1000);
            Some(MediaRange { mime, quality })
        })
        .collect()
}

/// Split an `Accept` header into its media ranges.
///
/// A comma inside a quoted parameter value is not a separator — RFC 9110
/// §5.6.4 makes `application/json;note="a,b"` one range, and splitting it makes
/// two unparseable ones, which are dropped, which is a 406 for a request that
/// named a type this server has.
///
/// This is the seam between the two halves of the header: `mediatype` owns the
/// grammar of one range and cannot see the list, and nothing in this stack owns
/// the list without also owning the selection. Fifteen lines and an oracle test
/// is the smaller of those two costs.
fn split_ranges(header: &str) -> Vec<&str> {
    let mut ranges = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (index, byte) in header.bytes().enumerate() {
        match byte {
            _ if escaped => escaped = false,
            b'\\' if quoted => escaped = true,
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                ranges.push(&header[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    ranges.push(&header[start..]);
    ranges
}

/// `q=0.812` as 812. Rejects anything outside RFC 9110 §12.4.2's grammar rather
/// than clamping, so `q=9` is an ignored parameter rather than a huge weight.
fn quality_thousandths(value: &str) -> Option<u32> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let scaled = match whole {
        "0" => 0,
        "1" => 1000,
        _ => return None,
    };
    let fraction: u32 = format!("{fraction:0<3}").parse().ok()?;
    (scaled + fraction <= 1000).then_some(scaled + fraction)
}

/// The quality `media_type` is acceptable at, or `None` if it is not acceptable
/// at all — which includes an explicit `q=0`, RFC 9110's way of excluding a type.
fn acceptability(ranges: &[MediaRange], media_type: &MediaType<'_>) -> Option<u32> {
    let best = ranges
        .iter()
        .filter_map(|range| Some((range.specificity(media_type)?, range.quality)))
        .max_by_key(|(specificity, _)| *specificity)?;
    (best.1 > 0).then_some(best.1)
}

// ---------------------------------------------------------------------------
// Caching
// ---------------------------------------------------------------------------

/// How long a response may be reused (doc 03 §3.6).
///
/// Three policies, because the URL space has exactly three kinds of resource: a
/// versioned bundle URL, whose bytes cannot change while the version exists
/// (doc 04 §4.6); a mutable document — the descriptors and the `latest`
/// redirect — which is a snapshot of something that moves; and an error, which
/// describes this attempt rather than a resource.
///
/// The header itself is built by the `headers` crate rather than written as a
/// string, so the directive names and their order are its problem and not this
/// server's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// A versioned GET. §3.6: "versioned GETs immutable".
    Immutable,
    /// A document that can change under the same URL. §3.6 fixes `max-age=300`
    /// for the `latest` redirect; the descriptors take the same window because
    /// they change on the same event — a new version being published.
    Mutable,
    /// A problem document. Caching one would answer a later, different request
    /// with this attempt's failure.
    Uncacheable,
}

impl CachePolicy {
    /// A year, the longest `max-age` §3.6 names.
    const A_YEAR: Duration = Duration::from_secs(31_536_000);
    /// Long enough that a `latest` redirect is not re-resolved per request,
    /// short enough that a new release is picked up in minutes (§3.6).
    const A_WHILE: Duration = Duration::from_secs(300);

    /// The typed `Cache-Control` header this policy sends.
    pub fn header(self) -> CacheControl {
        match self {
            Self::Immutable => CacheControl::new()
                .with_public()
                .with_max_age(Self::A_YEAR)
                .with_immutable(),
            Self::Mutable => CacheControl::new()
                .with_public()
                .with_max_age(Self::A_WHILE),
            Self::Uncacheable => CacheControl::new().with_no_store(),
        }
    }
}

/// The entity tag for a bundle version rendered as one representation by this
/// deployment.
///
/// Doc 03 §3.6 makes the ETag the artifact checksum and requires it to be
/// representation-specific, and those two are the first and last components
/// here. The middle one is what §3.6 leaves out and a **strong** validator
/// cannot: a response's bytes are a function of the data, the *configuration*
/// and the *code*, not of the data alone.
///
/// The case that makes it necessary is ordinary. `GET /fragment` with no
/// `limit` returns `caps.default_limit` rows; raise that number and restart,
/// and the same URL serves different bytes — under an `immutable` policy and a
/// year of `max-age`, so a client holding the old tag is told 304 for a year.
/// A rendering change between builds does the same. `deployment` is
/// [`Service::descriptor_digest`](crate::service::Service::descriptor_digest),
/// which already covers the caps, the budgets and this crate's version, so
/// mixing it in closes both. The cost is one revalidation per cached URL after
/// a deploy, which is the right direction to be wrong in.
///
/// The comparison against `If-None-Match` is `headers`', which is RFC 9110
/// §13.1.2's weak comparison — `*`, a comma list, and a `W/` prefix on the
/// client's side all handled there rather than here.
pub fn etag(
    digest: &ContentDigest,
    deployment: &ContentDigest,
    representation: Representation,
) -> ETag {
    let tag = format!(
        "\"{}.{}.{}\"",
        digest.as_str(),
        deployment.short(),
        representation.token()
    );
    tag.parse().unwrap_or_else(|error| {
        // Unreachable by construction, and worth saying why rather than
        // silently omitting the validator: `ContentDigest` is parsed to
        // `{algorithm}:{lowercase hex}` and a token is a `&'static str` from
        // this module, so every byte is `etagc` (RFC 9110 §8.8.3).
        unreachable!("a digest and a format token are a valid entity tag: {error}")
    })
}

/// A body-addressed operation's strong validator.
///
/// The operation and raw body are included because QUERY has one URL for many
/// entities, and the same body shape may be accepted by more than one
/// operation. Two JSON spellings of the same semantic request may receive
/// different tags; that only forgoes a cache hit, while omitting either input
/// would make one tag claim that different result sets are the same entity.
pub fn etag_for_body(
    digest: &ContentDigest,
    deployment: &ContentDigest,
    operation: &str,
    representation: Representation,
    body: &[u8],
) -> ETag {
    let mut request = Sha256::new();
    let operation_bytes = operation.as_bytes();
    let operation_length = u64::try_from(operation_bytes.len())
        .expect("an operation name fits in a u64 on every supported platform");
    request.update(operation_length.to_be_bytes());
    request.update(operation_bytes);
    request.update(body);
    let request = request.finalize();
    let tag = format!(
        "\"{}.{}.{:x}.{}\"",
        digest.as_str(),
        deployment.short(),
        request,
        representation.token()
    );
    tag.parse().unwrap_or_else(|error| {
        unreachable!("digests and a format token are a valid entity tag: {error}")
    })
}

/// A bundle version's canonical identity: `sha256:` and lowercase hex
/// (doc 04 §4.3).
///
/// Parsed rather than carried as a string because it is put in an `ETag`, and a
/// header value has a grammar: a digest containing a quote or a control
/// character would produce a tag no cache can parse, and one containing CR/LF
/// would be header injection from a file on disk. Checking the shape once, at
/// the point a manifest is read, is cheaper than remembering to escape it at
/// every use — and a manifest whose digest is not a digest is a broken manifest
/// worth refusing outright.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentDigest(String);

impl ContentDigest {
    /// Parse `{algorithm}:{lowercase hex}`.
    ///
    /// The algorithm is not restricted to `sha256`: doc 04 §4.3 prefixes the
    /// digest with its algorithm precisely so it can change, and nothing here
    /// recomputes it — this layer only carries and compares it.
    pub fn parse(text: &str) -> Option<Self> {
        let (algorithm, hex) = text.split_once(':')?;
        let named = !algorithm.is_empty()
            && algorithm
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        let hexadecimal = hex.len() >= 16
            && hex.len().is_multiple_of(2)
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        (named && hexadecimal).then(|| Self(text.to_owned()))
    }

    /// The digest as written in the manifest, algorithm prefix included.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first eight hex digits, for discriminating rather than identifying.
    ///
    /// Enough for an [`etag`]'s deployment component, which distinguishes one
    /// build-and-configuration from another in a cache key. Not a substitute
    /// for [`as_str`](Self::as_str) anywhere identity matters: a prefix is a
    /// prefix, and doc 04 §4.3's `content_digest` is the whole thing.
    pub fn short(&self) -> &str {
        let hex = self
            .0
            .split_once(':')
            .map_or(self.0.as_str(), |(_, hex)| hex);
        &hex[..hex.len().min(8)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: &[Representation] = Representation::ALL;
    const JSON: &[Representation] = &[Representation::Json];

    /// What a browser sends on a top-level navigation.
    const BROWSER: &str =
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";

    fn digest() -> ContentDigest {
        ContentDigest::parse(
            "sha256:1f0e3dad99908345f7439f8ffabdffc4de5f7439f8ffabdffc41f0e3dad99908",
        )
        .expect("a well-formed digest")
    }

    #[test]
    fn an_absent_accept_takes_the_operations_first_offer() {
        assert_eq!(negotiate(None, None, JSON), Ok(Representation::Json));
        assert_eq!(negotiate(None, Some(""), JSON), Ok(Representation::Json));
        assert_eq!(negotiate(None, Some("*/*"), JSON), Ok(Representation::Json));
    }

    #[test]
    fn a_browser_gets_the_page_and_everything_else_gets_the_data() {
        // The whole of the HTML story's routing, and it is `Accept` alone: no
        // user-agent sniffing and no second URL space. A browser's exact
        // `text/html` outranks its own trailing `*/*;q=0.8`; `curl`, a library
        // and an agent all tie at `*/*` and fall to the first offer.
        assert_eq!(
            negotiate(None, Some(BROWSER), BOTH),
            Ok(Representation::Html)
        );
        assert_eq!(negotiate(None, Some("*/*"), BOTH), Ok(Representation::Json));
        assert_eq!(negotiate(None, None, BOTH), Ok(Representation::Json));
        assert_eq!(
            negotiate(None, Some("application/json"), BOTH),
            Ok(Representation::Json)
        );
        assert_eq!(
            negotiate(None, Some("text/html"), BOTH),
            Ok(Representation::Html)
        );

        // And either can be pinned explicitly, which is what a "view as JSON"
        // link on the page is.
        assert_eq!(
            negotiate(Some("html"), Some("application/json"), BOTH),
            Ok(Representation::Html)
        );
        assert_eq!(
            negotiate(Some("json"), Some(BROWSER), BOTH),
            Ok(Representation::Json)
        );
    }

    #[test]
    fn an_error_is_always_renderable() {
        // A negotiation failure still has to be reported, and reporting it with
        // another negotiation failure is a loop.
        assert_eq!(
            Representation::for_problem(None, Some("text/csv")),
            Representation::Json
        );
        assert_eq!(
            Representation::for_problem(Some("parquet"), None),
            Representation::Json
        );
        // A browser that mistypes a URL should still get a page.
        assert_eq!(
            Representation::for_problem(None, Some(BROWSER)),
            Representation::Html
        );
    }

    #[test]
    fn accept_is_matched_by_the_grammar_and_not_by_substring() {
        // The bug this exists to prevent: a client asking for CSV and getting
        // JSON because the header "mentions json somewhere".
        assert_eq!(
            negotiate(None, Some("application/json"), JSON),
            Ok(Representation::Json)
        );
        assert_eq!(
            negotiate(None, Some("application/*"), JSON),
            Ok(Representation::Json)
        );
        // A subtype that merely contains the token is a different media type.
        let refused = negotiate(None, Some("application/json-seq"), JSON).unwrap_err();
        assert_eq!(refused.code(), ErrorCode::NotAcceptable);
        assert_eq!(refused.status(), 406);

        assert_eq!(
            negotiate(None, Some("text/csv"), JSON).unwrap_err().code(),
            ErrorCode::NotAcceptable
        );
    }

    #[test]
    fn the_most_specific_range_decides_before_its_quality_does() {
        // RFC 9110 §12.5.1. Read the other way round — highest `q` first —
        // this header would serve JSON, because `*/*` matches it at 0.1.
        assert_eq!(
            negotiate(None, Some("*/*;q=0.1, application/json;q=0"), JSON)
                .unwrap_err()
                .code(),
            ErrorCode::NotAcceptable,
            "an explicit q=0 excludes the type the wildcard would have allowed"
        );
        assert_eq!(
            negotiate(None, Some("application/json;q=0.2, */*;q=0"), JSON),
            Ok(Representation::Json),
            "and the exclusion runs the other way too"
        );
    }

    #[test]
    fn a_comma_inside_a_quoted_parameter_is_not_a_separator() {
        // RFC 9110 §5.6.4. Split naively, this becomes two unparseable ranges,
        // both dropped, and a 406 for a request that named a type we serve.
        assert_eq!(
            negotiate(None, Some(r#"application/json;note="a,b""#), BOTH),
            Ok(Representation::Json)
        );
        assert_eq!(
            negotiate(
                None,
                Some(r#"text/html;note="a,b", application/json;q=0.1"#),
                BOTH
            ),
            Ok(Representation::Html),
            "the quoted range must survive and outrank the one after it"
        );
        // An escaped quote inside the value does not end it.
        assert_eq!(
            negotiate(None, Some(r#"application/json;note="a\",b""#), BOTH),
            Ok(Representation::Json)
        );
        // An unterminated quote consumes the rest rather than resplitting it.
        assert_eq!(
            split_ranges(r#"text/html;note="a, application/json"#).len(),
            1
        );
        assert_eq!(split_ranges("text/html, application/json").len(), 2);
    }

    #[test]
    fn a_quality_outside_the_grammar_is_an_ignored_parameter() {
        // RFC 9110 §12.4.2 gives `q` at most three decimals in 0..=1. Anything
        // else is malformed, and a malformed parameter is dropped — which is
        // not the same as dropping the range that carried it.
        assert_eq!(quality_thousandths("0.812"), Some(812));
        assert_eq!(quality_thousandths("1"), Some(1000));
        assert_eq!(quality_thousandths("0.5"), Some(500));
        assert_eq!(quality_thousandths("1.000"), Some(1000));
        assert_eq!(quality_thousandths("1.001"), None);
        assert_eq!(quality_thousandths("0.1234"), None);
        assert_eq!(quality_thousandths("9"), None);
        assert_eq!(quality_thousandths("x"), None);

        assert_eq!(
            negotiate(None, Some("application/json;q=nonsense"), JSON),
            Ok(Representation::Json)
        );
    }

    #[test]
    fn format_decides_alone_when_it_is_present() {
        // The two failures are different codes with different statuses and
        // different remedies (§3.6.1), which is only coherent if `format=` and
        // `Accept` are evaluated separately rather than intersected.
        assert_eq!(
            negotiate(Some("json"), Some("text/csv"), JSON),
            Ok(Representation::Json)
        );

        let refused = negotiate(Some("csv"), Some("*/*"), JSON).unwrap_err();
        assert_eq!(refused.code(), ErrorCode::UnsupportedFormat);
        assert_eq!(refused.status(), 400);
        let detail = serde_json::to_value(&refused).unwrap();
        assert!(
            detail["detail"].as_str().unwrap().contains("json"),
            "the detail must name what the operation does offer: {detail}"
        );
    }

    #[test]
    fn an_etag_changes_with_the_data_the_deployment_and_the_representation() {
        let deployment = ContentDigest::parse("sha256:aaaaaaaaaaaaaaaabbbbbbbbbbbbbbbb").unwrap();
        let tag = etag(&digest(), &deployment, Representation::Json);
        let rendered = format!("{tag:?}");
        assert!(rendered.contains(digest().as_str()));
        assert!(rendered.contains(Representation::Json.token()));

        let other = ContentDigest::parse("sha256:0000000000000000abcdef0123456789").unwrap();
        assert_ne!(tag, etag(&other, &deployment, Representation::Json));

        // The half §3.6 asks for by name. Without it a shared cache holding the
        // page could answer an agent's `Accept: application/json` from it, and
        // `Vary: Accept` alone would not stop that — the tags would be equal.
        assert_ne!(tag, etag(&digest(), &deployment, Representation::Html));

        // And the half §3.6 leaves out. A response is a function of the data,
        // the configuration *and* the code: `GET /fragment` with no `limit`
        // returns `default_limit` rows, so raising it changes the bytes at a
        // URL whose data did not move. Under `immutable` and a year of
        // `max-age`, a validator that missed that would answer 304 for a year.
        assert_ne!(tag, etag(&digest(), &other, Representation::Json));

        let body_tag = etag_for_body(
            &digest(),
            &deployment,
            "fragment",
            Representation::Json,
            br#"{"pattern":1}"#,
        );
        assert_ne!(tag, body_tag);
        assert_ne!(
            body_tag,
            etag_for_body(
                &digest(),
                &deployment,
                "fragment",
                Representation::Json,
                br#"{"pattern":2}"#,
            )
        );
        assert_ne!(
            body_tag,
            etag_for_body(
                &digest(),
                &deployment,
                "count",
                Representation::Json,
                br#"{"pattern":1}"#,
            ),
            "the same body can name different entities at different operations"
        );
    }

    #[test]
    fn every_digest_and_format_makes_a_sendable_entity_tag() {
        // `etag` is infallible by construction and says so with `unreachable!`,
        // which is only honest if the construction really does cover the space:
        // a `ContentDigest` is `{algorithm}:{lowercase hex}` and a token is one
        // of this module's own strings, so every byte is RFC 9110 §8.8.3's
        // `etagc`. Checked here rather than trusted.
        let mut map = axum::http::HeaderMap::new();
        for text in [
            "sha256:0123456789abcdef",
            "sha512-256:0123456789abcdef0123456789abcdef",
            "b3:00112233445566778899aabbccddeeff",
        ] {
            let digest = ContentDigest::parse(text).expect(text);
            for representation in Representation::ALL {
                let tag = etag(&digest, &digest, *representation);
                headers::HeaderMapExt::typed_insert(&mut map, tag.clone());
                assert_eq!(
                    headers::HeaderMapExt::typed_get::<ETag>(&map).as_ref(),
                    Some(&tag),
                    "{text} as {}",
                    representation.token()
                );
            }
        }
    }

    #[test]
    fn a_digest_that_could_not_go_in_a_header_is_not_a_digest() {
        assert!(ContentDigest::parse("sha256:0123456789abcdef").is_some());
        // The reason this is parsed at all: these reach an `ETag` otherwise.
        assert!(ContentDigest::parse("sha256:0123456789abcdef\"").is_none());
        assert!(ContentDigest::parse("sha256:0123456789ab\r\nX-Evil: 1").is_none());
        // And the ordinary malformations.
        assert!(ContentDigest::parse("0123456789abcdef").is_none());
        assert!(ContentDigest::parse("sha256:").is_none());
        assert!(ContentDigest::parse("sha256:0123ABCDEF456789").is_none());
        assert!(ContentDigest::parse("sha256:0123456789abcde").is_none());
    }

    #[test]
    fn the_cache_policies_are_the_ones_doc_03_names() {
        // Asserted on the directives rather than on the rendered string: the
        // header is `headers`' to format, and pinning its token order here
        // would be testing that crate rather than this decision.
        let immutable = CachePolicy::Immutable.header();
        assert!(immutable.public() && immutable.immutable());
        assert_eq!(immutable.max_age(), Some(Duration::from_secs(31_536_000)));

        let mutable = CachePolicy::Mutable.header();
        assert!(mutable.public() && !mutable.immutable());
        assert_eq!(mutable.max_age(), Some(Duration::from_secs(300)));

        assert!(CachePolicy::Uncacheable.header().no_store());
    }
}
