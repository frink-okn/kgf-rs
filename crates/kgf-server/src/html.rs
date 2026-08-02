//! The page a browser gets, for every resource that has one.
//!
//! Doc 01 argues for an interface an agent can use without a client library;
//! the same argument applies to a person with a browser, and the two need not
//! be different endpoints. Every route here answers both, chosen by `Accept`
//! alone (see [`crate::representation`]) — a page when a browser navigates to
//! it, JSON when anything else fetches it, at one URL.
//!
//! # Escaping is the templating engine's job
//!
//! Markup is written with `maud`, whose `html!` macro escapes every
//! interpolation and requires [`maud::PreEscaped`] to opt out.
//! Nothing in this crate concatenates HTML. That matters more than it might
//! look: the data on these pages is a published bundle's own manifest and
//! dictionary, so "someone published a dataset whose title contains a
//! `<script>` tag" is an ordinary case rather than an attack.
//!
//! This replaced a hand-written builder, which had exactly the bug the macro
//! makes unavailable — an `&` written raw into an `href` while the URL around
//! it was escaped. The tests that caught it are still here.
//!
//! # One shell, one trait
//!
//! [`page`] is the shared document: head, breadcrumbs, the `<h1>`, and a footer
//! linking the same resource's JSON. Routes supply only the body, so no page
//! can lose the charset declaration or the machine-readable link.
//!
//! [`Resource`] pairs the two renderings. A new route implements it and gets
//! both, or does not compile — the same reason [`Representation`] is an enum
//! rather than a string.
//!
//! [`Representation`]: crate::representation::Representation

use maud::{DOCTYPE, Markup, PreEscaped, html};

/// What the pages call themselves.
pub const SITE: &str = "Knowledge Graph Fragments";

/// A resource this server can serve in every representation it offers.
pub trait Resource {
    /// The canonical machine-readable form (doc 03 §3.4.1).
    fn to_json(&self) -> Vec<u8>;

    /// The same resource, as a page.
    fn to_html(&self) -> String;
}

/// Serialize a `#[derive(Serialize)]` document as a response body.
///
/// Pretty-printed: these documents are read by people as often as by programs —
/// doc 03 §3.1 makes the descriptors the thing a client reads first — and a
/// `curl` of a one-line 4 KB manifest is not that.
pub fn json_body(value: &impl serde::Serialize) -> Vec<u8> {
    let mut body = serde_json::to_vec_pretty(value)
        // A descriptor is a tree of owned strings, maps and numbers, so the
        // only way this fails is a `Serialize` impl that errors — none of ours
        // does, and a broken one should be loud rather than served empty.
        .expect("descriptors serialize");
    body.push(b'\n');
    body
}

/// One step of the breadcrumb trail. The last has no link: it is this page.
pub struct Crumb<'a> {
    /// What the step is called.
    pub label: &'a str,
    /// Where it goes, or `None` for the current page.
    pub href: Option<String>,
}

impl<'a> Crumb<'a> {
    /// A step that links somewhere.
    pub fn to(label: &'a str, href: String) -> Self {
        Self {
            label,
            href: Some(href),
        }
    }

    /// The step the reader is on.
    pub fn here(label: &'a str) -> Self {
        Self { label, href: None }
    }
}

/// The shared document: everything but the body.
///
/// `canonical` is this resource's own URL, which the footer turns into a link
/// to its JSON. It is the resource's canonical URL rather than the one the
/// request arrived on, so a page reached through `latest` links to the version
/// it actually resolved to.
pub fn page(title: &str, crumbs: &[Crumb<'_>], canonical: Option<&str>, body: Markup) -> String {
    // The service descriptor's own title *is* the site name, and a tab reading
    // "Knowledge Graph Fragments — Knowledge Graph Fragments" is the classic
    // template seam.
    let full_title = if title == SITE {
        SITE.to_owned()
    } else {
        format!("{title} — {SITE}")
    };
    let json = canonical.map(|url| {
        let separator = if url.contains('?') { '&' } else { '?' };
        format!("{url}{separator}format=json")
    });

    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (full_title) }
                style { (PreEscaped(STYLE)) }
            }
            body {
                header {
                    nav {
                        @for (index, crumb) in crumbs.iter().enumerate() {
                            @if index > 0 {
                                span."sep" { "/" }
                            }
                            @match &crumb.href {
                                Some(href) => a href=(href) { (crumb.label) },
                                None => span { (crumb.label) },
                            }
                        }
                    }
                }
                main {
                    h1 { (title) }
                    (body)
                }
                footer {
                    @if let Some(json) = &json {
                        a href=(json) { "This page as JSON" }
                        span."sep" { "·" }
                    }
                    span { "the same URL answers JSON to anything that does not ask for HTML" }
                }
            }
        }
    }
    .into_string()
}

/// One cell or field value on a page.
#[derive(Debug, Clone)]
pub enum Value<'a> {
    /// Prose.
    Text(&'a str),
    /// An IRI, digest, count or other machine-facing string, set in monospace.
    Code(&'a str),
    /// A number, right-aligned and grouped.
    Number(u64),
    /// A link to another page of this server.
    Link {
        /// Target, already URL-encoded by [`crate::url`].
        href: String,
        /// What the link says.
        label: &'a str,
    },
    /// A field the resource does not carry.
    Absent,
}

impl<'a> Value<'a> {
    /// A link whose label is its target.
    pub fn self_link(href: String, label: &'a str) -> Self {
        Self::Link { href, label }
    }
}

impl maud::Render for Value<'_> {
    fn render(&self) -> Markup {
        html! {
            @match self {
                Value::Text(text) => (text),
                Value::Code(text) => code { (text) },
                Value::Number(number) => (group_digits(*number)),
                Value::Link { href, label } => a href=(href) { (label) },
                Value::Absent => {}
            }
        }
    }
}

/// A field list: a label and a value per row. Absent values leave no row.
pub fn fields(rows: &[(&str, Value<'_>)]) -> Markup {
    html! {
        dl {
            @for (label, value) in rows {
                @if !matches!(value, Value::Absent) {
                    dt { (label) }
                    dd { (value) }
                }
            }
        }
    }
}

/// A table, horizontally scrollable so a wide row cannot widen the page.
pub fn table(headers: &[&str], rows: &[Vec<Value<'_>>]) -> Markup {
    html! {
        div."scroll" {
            table {
                thead { tr { @for header in headers { th { (header) } } } }
                tbody {
                    @for row in rows {
                        tr {
                            @for cell in row {
                                @if let Value::Number(_) = cell {
                                    td."num" { (cell) }
                                } @else {
                                    td { (cell) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// An aside: the explanation under a heading, in smaller type.
pub fn note(text: &str) -> Markup {
    html! { p."note" { (text) } }
}

/// `1234567` as `1 234 567`.
///
/// A thin space rather than a comma: these are triple counts read next to IRIs,
/// and a comma inside a number that sits beside a comma-separated list is one
/// more thing to disambiguate.
fn group_digits(number: u64) -> String {
    let digits = number.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push('\u{202f}');
        }
        grouped.push(digit);
    }
    grouped
}

/// Inline, because a stylesheet at its own URL is another route, another cache
/// entry and another thing that can 404 while the page still renders.
const STYLE: &str = "\
:root{--bg:#fdfdfc;--fg:#1a1a19;--dim:#6b6b66;--rule:#e3e3df;--accent:#2f5d8a;--code:#f4f4f1}
@media(prefers-color-scheme:dark){:root{--bg:#16161a;--fg:#e8e8e4;--dim:#9a9a94;--rule:#2c2c33;--accent:#8ab4e8;--code:#1e1e24}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.6 ui-sans-serif,system-ui,-apple-system,Segoe UI,Helvetica,Arial,sans-serif}
header,main,footer{max-width:56rem;margin:0 auto;padding:0 1.5rem}
header{border-bottom:1px solid var(--rule);padding-top:1.25rem;padding-bottom:.75rem}
nav{font-size:.875rem;color:var(--dim)}
nav a{color:var(--accent);text-decoration:none}
nav a:hover{text-decoration:underline}
.sep{padding:0 .5rem;color:var(--dim)}
main{padding-top:2rem;padding-bottom:3rem}
h1{font-size:1.75rem;line-height:1.25;margin:0 0 1.5rem;font-weight:600}
h2{font-size:1.05rem;margin:2.5rem 0 .75rem;font-weight:600;letter-spacing:.02em;text-transform:uppercase;color:var(--dim)}
p{margin:0 0 1rem}
.note{color:var(--dim);font-size:.9rem}
code{font:.875em/1.5 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;background:var(--code);padding:.1em .35em;border-radius:3px;word-break:break-all}
pre{background:var(--code);padding:1rem;border-radius:6px;overflow-x:auto}
pre code{background:none;padding:0;word-break:normal}
dl{display:grid;grid-template-columns:minmax(8rem,auto) 1fr;gap:.5rem 1.5rem;margin:0}
dt{color:var(--dim);font-size:.9rem}
dd{margin:0;min-width:0;overflow-wrap:anywhere}
.scroll{overflow-x:auto}
table{border-collapse:collapse;width:100%;font-size:.9375rem}
th{text-align:left;font-weight:600;color:var(--dim);font-size:.8125rem;text-transform:uppercase;letter-spacing:.03em}
th,td{padding:.5rem .75rem .5rem 0;border-bottom:1px solid var(--rule);vertical-align:top}
td.num{text-align:right;font-variant-numeric:tabular-nums}
a{color:var(--accent)}
footer{border-top:1px solid var(--rule);padding-top:1rem;padding-bottom:2rem;font-size:.8125rem;color:var(--dim)}
footer a{color:var(--accent);text-decoration:none}
footer a:hover{text-decoration:underline}
";

#[cfg(test)]
mod tests {
    use super::*;

    /// The escaping test, kept verbatim in spirit across the move from a
    /// hand-written builder to `maud`. It caught a real bug against the
    /// builder — an `&` written raw into an `href` — and its job now is to
    /// prove the property did not quietly change hands.
    #[test]
    fn every_channel_that_takes_data_escapes_it() {
        // A dataset whose title is hostile is an ordinary case: the strings on
        // these pages come from a published bundle's manifest and dictionary.
        let hostile = "<script>alert('x')</script>";
        let rendered = page(
            hostile,
            &[Crumb::to(hostile, "/a\"b".to_owned())],
            Some("/a?q=\""),
            html! {
                p { (hostile) }
                (note(hostile))
                h2 { (hostile) }
                (fields(&[(hostile, Value::Code(hostile))]))
                (table(
                    &[hostile],
                    &[vec![Value::Link {
                        href: "/x\"onmouseover=alert(1)".to_owned(),
                        label: hostile,
                    }]],
                ))
            },
        );

        assert!(
            !rendered.contains("<script>"),
            "unescaped markup reached the page"
        );
        assert!(rendered.contains("&lt;script&gt;"));
        // `alert('x')` survives as text and that is fine — the tags around it
        // are escaped, so it is inert. `maud` escapes `&`, `<`, `>` and `"`
        // and not `'`, which is sound because it always emits attributes
        // double-quoted, so an apostrophe can never end one. The builder this
        // replaced escaped `'` too, which was belt-and-braces rather than the
        // property; the property is the two assertions above and the four
        // below.
        // Attribute values are the half a three-character escape would miss.
        // The payload text may survive — escaped, it is inert — but the quote
        // that would end the attribute and start a new one may not.
        assert!(rendered.contains("/a&quot;b"));
        assert!(rendered.contains("/x&quot;onmouseover=alert(1)"));
        assert!(
            !rendered.contains("\" onmouseover") && !rendered.contains("\"onmouseover"),
            "an attribute value broke out of its quotes"
        );
        // And `&` in an attribute is a character reference unless escaped.
        assert!(!rendered.contains("?q=\"&format"));
    }

    #[test]
    fn a_page_is_a_whole_document() {
        let rendered = page("Title", &[], None, html! {});
        assert!(rendered.starts_with("<!DOCTYPE html>"));
        assert!(rendered.contains("<meta charset=\"utf-8\">"));
        assert!(rendered.contains("<title>Title — Knowledge Graph Fragments</title>"));
        assert!(rendered.trim_end().ends_with("</html>"));
        // The site's own front page is not "X — X".
        assert!(
            page(SITE, &[], None, html! {}).contains("<title>Knowledge Graph Fragments</title>")
        );
    }

    #[test]
    fn the_json_affordance_survives_a_url_that_already_has_a_query() {
        let plain = page("t", &[], Some("/a/b"), html! {});
        assert!(plain.contains("href=\"/a/b?format=json\""));

        let queried = page("t", &[], Some("/a/b?s=x"), html! {});
        assert!(queried.contains("href=\"/a/b?s=x&amp;format=json\""));
    }

    #[test]
    fn numbers_are_grouped_for_reading() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1\u{202f}000");
        assert_eq!(group_digits(606_342_307), "606\u{202f}342\u{202f}307");
    }

    #[test]
    fn absent_fields_leave_no_empty_row() {
        let rendered =
            fields(&[("present", Value::Text("yes")), ("missing", Value::Absent)]).into_string();
        assert!(rendered.contains("present"));
        assert!(!rendered.contains("missing"));
    }
}
