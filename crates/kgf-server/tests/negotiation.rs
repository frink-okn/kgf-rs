//! `Accept` negotiation, differentially against an independent implementation.
//!
//! The test compares against an implementation that did not come from the same
//! code path, applied to the one piece of RFC parsing
//! this crate could not get from a library. `headers-accept` is a separate
//! reading of RFC 9110 §12.5.1 by a separate author; where the two agree, both
//! are probably right, and where they differ this file says which of us is and
//! why.
//!
//! It has already earned its place: it caught `mime` silently dropping a media
//! range with the optional whitespace §5.6.3 allows around `;`, which made
//! `Accept: text/html ; q=0.5, application/json;q=0.4` serve JSON.

use std::str::FromStr;

use headers_accept::Accept;
use kgf_server::representation::{Representation, negotiate};
use mediatype_021::MediaType;
use mediatype_021::names::{APPLICATION, HTML, JSON, TEXT};

/// The same two representations the server offers, in the same order.
const AVAILABLE: &[MediaType] = &[
    MediaType::new(APPLICATION, JSON),
    MediaType::new(TEXT, HTML),
];

fn independent(header: &str) -> Option<String> {
    Accept::from_str(header)
        .ok()?
        .negotiate(AVAILABLE)
        .map(|media| media.to_string())
}

fn ours(header: &str) -> Option<String> {
    negotiate(None, Some(header), Representation::ALL)
        .ok()
        .map(|representation| representation.media_type().to_owned())
}

/// Headers where the two implementations must agree.
const AGREED: &[&str] = &[
    "*/*",
    "application/json",
    "text/html",
    "application/json, text/html",
    "text/html;q=0.9, application/json;q=0.8",
    "text/html;q=0.8, application/json;q=0.9",
    "application/*",
    "text/*",
    "application/json-seq",
    "text/csv",
    "image/png, image/*",
    // The §12.5.1 rule that specificity decides before quality: `q=0` on the
    // exact type excludes it even though the wildcard would have allowed it.
    "*/*;q=0.1, application/json;q=0",
    "*/*;q=0.1, text/html;q=0",
    "application/json;q=0.2, */*;q=0",
    "application/json;q=0, text/html;q=0",
    "*/*;q=0",
    // What a browser sends, and the whole reason the HTML story works.
    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
    "application/json;q=1, text/html;q=1",
    "APPLICATION/JSON",
    "application/json;q=0.000",
    "application/json;q=1.000",
    // The optional whitespace RFC 9110 §5.6.3 allows. `mime` rejected these,
    // which dropped the range and negotiated from what was left.
    "text/html ; q=0.9, application/json;q=0.4",
    "text/html  ;  q=0.5  , application/json;q=0.4",
];

#[test]
fn the_two_implementations_agree_on_the_grammar() {
    let disagreements: Vec<_> = AGREED
        .iter()
        .filter(|header| ours(header) != independent(header))
        .map(|header| (header, ours(header), independent(header)))
        .collect();
    assert!(disagreements.is_empty(), "{disagreements:#?}");
}

#[test]
fn where_the_two_differ_it_is_a_decision_and_not_a_bug() {
    // An equal-quality list. RFC 9110 §12.5.1 orders media ranges by `q` and
    // gives the header's own order no meaning, so a tie is the server's to
    // break; `headers-accept` breaks it on the client's listing order and this
    // server breaks it on its own preference. Ours is what makes "JSON unless
    // you asked for HTML" a rule a client can predict rather than a property of
    // how it happened to spell its header.
    for header in [
        "text/html, application/json",
        "text/html, application/json;q=1.0",
    ] {
        assert_eq!(
            ours(header).as_deref(),
            Some("application/json"),
            "{header}"
        );
        assert_eq!(
            independent(header).as_deref(),
            Some("text/html"),
            "{header}"
        );
    }

    // `*/json` is not a media range. §12.5.1's grammar admits `*/*`, `type/*`
    // and `type/subtype`; a wildcard type with a concrete subtype is none of
    // them, and we refuse it rather than guess what was meant.
    assert_eq!(ours("*/json"), None);
    assert_eq!(independent("*/json").as_deref(), Some("application/json"));

    // A media range carrying a non-`q` parameter. Read strictly, a parameter
    // makes the range *more specific*, so it matches only a type that carries
    // it — which means answering 406 to `Accept: application/json;charset=utf-8`.
    // That header comes from clients that are confused rather than picky, and
    // refusing it helps nobody, so non-`q` parameters are ignored for matching.
    for header in ["application/json;charset=utf-8", "text/html;level=1"] {
        assert!(ours(header).is_some(), "{header}");
        assert_eq!(independent(header), None, "{header}");
    }

    // Same rule, reached through a quoted parameter value — which is here
    // because the *framing* is what was broken. §5.6.4 makes the comma part of
    // the value, and splitting on it produced two unparseable ranges and a 406.
    // Both implementations now see one range and two; they still differ on what
    // to do with its parameter, which is the decision above.
    assert_eq!(
        Accept::from_str(r#"text/html;note="a,b", application/json;q=0.1"#)
            .expect("parses")
            .media_types()
            .count(),
        2,
        "the oracle reads the quoted comma as part of the value"
    );
    assert_eq!(
        ours(r#"text/html;note="a,b", application/json;q=0.1"#).as_deref(),
        Some("text/html"),
        "so must we, or the quoted range is lost and the weaker one wins"
    );
    assert_eq!(
        ours(r#"application/json;note="a,b""#).as_deref(),
        Some("application/json")
    );
    assert_eq!(independent(r#"application/json;note="a,b""#), None);
}
