//! The page a browser gets, for every resource that has one.
//!
//! Doc 01 argues for an interface an agent can use without a client library;
//! the same argument applies to a person with a browser, and the two need not
//! be different endpoints. Every route here answers both, chosen by `Accept`
//! alone (see [`crate::representation`]) — a page when a browser navigates to
//! it, JSON when anything else fetches it, at one URL.
//!
//! # Two registers, one voice
//!
//! The pages speak in two registers on purpose. The *chrome* — masthead,
//! headings, prose — is editorial: a serif display face, a warm paper palette,
//! generous rhythm. The *data* — tables, term cells, chips, stats — is a
//! registry: monospace identifiers, tabular numerals, compact rows. Catalog
//! pages lean editorial; operation pages lean registry; both come from the one
//! stylesheet below, so they cannot drift apart.
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
//! [`page`] is the shared document: head, the masthead with its breadcrumb
//! trail, the `<h1>`, and a footer linking the same resource's JSON. Routes
//! supply only the body, so no page can lose the charset declaration or the
//! machine-readable link. The masthead renders the site brand itself; callers
//! pass only the steps *below* the root.
//!
//! [`Resource`] pairs the two renderings. A new route implements it and gets
//! both, or does not compile — the same reason [`Representation`] is an enum
//! rather than a string.
//!
//! [`Representation`]: crate::representation::Representation

use bytes::Bytes;
use maud::{DOCTYPE, Markup, PreEscaped, html};

/// What the pages call themselves.
pub const SITE: &str = "Knowledge Graph Fragments";

/// A resource this server can serve in every representation it offers.
pub trait Resource {
    /// The canonical machine-readable form (doc 03 §3.4.1).
    ///
    /// [`Bytes`] rather than `Vec<u8>` so a resource that already holds its
    /// serialization — the bundle manifest holds the published file — hands it
    /// on by refcount instead of copying it per request.
    fn to_json(&self) -> Bytes;

    /// The same resource, as a page.
    fn to_html(&self) -> String;
}

/// Serialize a `#[derive(Serialize)]` document as a response body.
///
/// Pretty-printed: these documents are read by people as often as by programs —
/// doc 03 §3.1 makes the descriptors the thing a client reads first — and a
/// `curl` of a one-line 4 KB manifest is not that.
pub fn json_body(value: &impl serde::Serialize) -> Bytes {
    let mut body = serde_json::to_vec_pretty(value)
        // A descriptor is a tree of owned strings, maps and numbers, so the
        // only way this fails is a `Serialize` impl that errors — none of ours
        // does, and a broken one should be loud rather than served empty.
        .expect("descriptors serialize");
    body.push(b'\n');
    Bytes::from(body)
}

/// One step of the breadcrumb trail. The last has no link: it is this page.
///
/// The trail starts *below* the site root — [`page`] renders the brand link to
/// `/` itself, so no caller can spell it differently.
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
                script { (PreEscaped(FORM_SCRIPT)) }
            }
            body {
                header {
                    nav."crumbs" {
                        a."brand" href="/" { (SITE) }
                        @for crumb in crumbs {
                            span."sep" { "/" }
                            @match &crumb.href {
                                Some(href) => a href=(href) { (crumb.label) },
                                None => span."here" { (crumb.label) },
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

/// How one RDF term is spelled on a page, structured so the stylesheet can
/// give each part its own weight.
///
/// `primary` is the reading spelling — a CURIE, a bracketed IRI, a blank-node
/// label, or a literal's quoted lexical form. `qualifier` is the part set
/// small and dim beside it: a literal's `@lang` or `^^datatype`. `annotation`
/// is a resolved human label, shown under the term. `full_iri` feeds the
/// native tooltip.
#[derive(Debug, Clone, Default)]
pub struct TermText<'a> {
    /// The reading spelling.
    pub primary: &'a str,
    /// `@lang` or `^^datatype`, dim and inline.
    pub qualifier: Option<&'a str>,
    /// A resolved display label, under the term.
    pub annotation: Option<&'a str>,
    /// The full IRI revealed by the native browser tooltip.
    pub full_iri: Option<&'a str>,
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
    /// An RDF term linking deeper into the bundle.
    TermLink {
        /// Target, carrying the term's full request spelling.
        href: String,
        /// The structured spelling.
        term: TermText<'a>,
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
                Value::TermLink { href, term } => {
                    a."term" href=(href) title=[term.full_iri] {
                        (term.primary)
                        @if let Some(qualifier) = term.qualifier {
                            span."t-qual" { (qualifier) }
                        }
                        @if let Some(annotation) = term.annotation {
                            span."t-label" { (annotation) }
                        }
                    }
                },
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

/// A table in the reading column, horizontally scrollable so a wide row
/// cannot widen the page.
pub fn table(headers: &[&str], rows: &[Vec<Value<'_>>]) -> Markup {
    table_markup(headers, rows, false)
}

/// A results table that breaks out of the reading column and takes the
/// window's width — the registry surface for row data.
pub fn results_table(headers: &[&str], rows: &[Vec<Value<'_>>]) -> Markup {
    table_markup(headers, rows, true)
}

fn table_markup(headers: &[&str], rows: &[Vec<Value<'_>>], wide: bool) -> Markup {
    html! {
        div.scroll.wide[wide] {
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

/// A row of small monospace capability tags.
pub fn chips<T: AsRef<str>>(items: &[T]) -> Markup {
    html! {
        ul."chips" {
            @for item in items {
                li."chip" { (item.as_ref()) }
            }
        }
    }
}

/// A row of stat tiles: a figure and what it counts.
///
/// Values are preformatted strings — grouped exact counts read best here —
/// set in the sans face with proportional figures, per the house rule that
/// display numbers are never tabular and never serif.
pub fn stats(items: &[(&str, String)]) -> Markup {
    html! {
        div."stats" {
            @for (label, value) in items {
                div."stat" {
                    span."stat-value" { (value) }
                    span."stat-label" { (label) }
                }
            }
        }
    }
}

/// `1234567` as `1 234 567`.
///
/// A thin space rather than a comma: these are triple counts read next to IRIs,
/// and a comma inside a number that sits beside a comma-separated list is one
/// more thing to disambiguate.
pub fn group_digits(number: u64) -> String {
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

/// `606342307` as `606.3 M`: the catalog-card form of a count.
///
/// One decimal, truncated rather than rounded so a card never claims more
/// than the bundle holds. Exact counts stay on the manifest page.
pub fn compact_number(number: u64) -> String {
    let scaled = |divisor: u64, unit: &str| {
        let whole = number / divisor;
        let tenth = (number % divisor) / (divisor / 10);
        if whole >= 100 || tenth == 0 {
            format!("{whole}{unit}")
        } else {
            format!("{whole}.{tenth}{unit}")
        }
    };
    match number {
        0..=9_999 => group_digits(number),
        10_000..=999_999 => scaled(1_000, "\u{202f}K"),
        1_000_000..=999_999_999 => scaled(1_000_000, "\u{202f}M"),
        _ => scaled(1_000_000_000, "\u{202f}B"),
    }
}

/// Inline, because a stylesheet at its own URL is another route, another cache
/// entry and another thing that can 404 while the page still renders. No
/// webfont for the same reason: the serif and mono stacks below are system
/// faces, so a page is one request.
const STYLE: &str = "\
:root{--bg:#faf7f1;--surface:#fffdf8;--fg:#241f16;--dim:#6e6557;--rule:#e5ddca;--rule-soft:#efe9da;\
--code:#f2ecdd;--chip:#f0e9d8;--accent:#8f3e24;--accent-ink:#fdf9f2;--hover:rgba(36,31,22,.045);\
--serif:\"Iowan Old Style\",\"Palatino Linotype\",Palatino,\"Book Antiqua\",Georgia,serif;\
--sans:ui-sans-serif,system-ui,-apple-system,\"Segoe UI\",Helvetica,Arial,sans-serif;\
--mono:ui-monospace,\"SF Mono\",SFMono-Regular,Menlo,Consolas,\"Liberation Mono\",monospace}
@media(prefers-color-scheme:dark){:root{--bg:#1b1813;--surface:#221e17;--fg:#eae3d4;--dim:#a49982;\
--rule:#39321f;--rule-soft:#2c261b;--code:#272219;--chip:#2c261b;--accent:#e28e60;--accent-ink:#231508;\
--hover:rgba(234,227,212,.05)}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.65 var(--sans);\
-webkit-font-smoothing:antialiased;text-rendering:optimizeLegibility}
header{border-bottom:1px solid var(--rule);background:var(--surface)}
.crumbs{max-width:102rem;margin:0 auto;padding:.85rem 1.5rem;font-size:.875rem;color:var(--dim);\
display:flex;flex-wrap:wrap;align-items:baseline}
.brand{font-family:var(--serif);font-size:1rem;font-weight:600;letter-spacing:.01em;\
color:var(--fg);text-decoration:none}
.brand:hover{color:var(--accent)}
.crumbs a:not(.brand){color:var(--accent);text-decoration:none}
.crumbs a:not(.brand):hover{text-decoration:underline}
.crumbs .here{color:var(--fg)}
.sep{padding:0 .5rem;color:var(--dim)}
main{display:grid;grid-template-columns:minmax(1.25rem,1fr) minmax(0,48rem) minmax(1.25rem,1fr);\
align-content:start;padding-top:2.25rem;padding-bottom:4rem}
main>*{grid-column:2;min-width:0}
main>.wide{grid-column:1/-1;width:min(100%,102rem);margin:0 auto;padding:0 1.5rem}
h1{font-family:var(--serif);font-size:clamp(1.7rem,1.3rem + 1.6vw,2.4rem);line-height:1.2;\
margin:0 0 1.25rem;font-weight:600;letter-spacing:-.01em}
h2{font-family:var(--sans);font-size:.8125rem;margin:2.75rem 0 .6rem;font-weight:650;\
letter-spacing:.08em;text-transform:uppercase;color:var(--dim)}
p{margin:0 0 1rem}
.lede{font-family:var(--serif);font-size:1.2rem;line-height:1.55;color:var(--fg);margin:0 0 1.25rem}
.note{color:var(--dim);font-size:.875rem}
a{color:var(--accent)}
code{font:.875em/1.5 var(--mono);background:var(--code);padding:.1em .35em;border-radius:4px;\
word-break:break-all}
pre{background:var(--code);border:1px solid var(--rule-soft);padding:.9rem 1.1rem;border-radius:8px;\
overflow-x:auto;font-size:.8125rem;line-height:1.6}
pre code{background:none;padding:0;word-break:normal}
dl{display:grid;grid-template-columns:minmax(8rem,auto) 1fr;gap:.45rem 1.5rem;margin:0 0 1rem}
dt{color:var(--dim);font-size:.875rem}
dd{margin:0;min-width:0;overflow-wrap:anywhere;font-size:.9375rem}
.scroll{overflow-x:auto;margin:.5rem 0 1rem}
table{border-collapse:collapse;width:100%;font-size:.875rem}
th{text-align:left;font-weight:600;color:var(--dim);font-size:.75rem;text-transform:uppercase;\
letter-spacing:.05em;white-space:nowrap}
th,td{padding:.5rem 1.25rem .5rem 0;border-bottom:1px solid var(--rule-soft);vertical-align:top}
thead tr{border-bottom:1px solid var(--rule)}
tbody tr:hover{background:var(--hover)}
td.num{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}
a.term{font-family:var(--mono);font-size:.8125rem;text-decoration:none;color:var(--accent);\
display:inline-block;max-width:38rem;overflow-wrap:anywhere}
a.term:hover{text-decoration:underline}
.t-qual{color:var(--dim);font-size:.9em;padding-left:.1em}
.t-label{display:block;font-family:var(--sans);font-size:.8125rem;color:var(--dim);margin-top:.1rem}
.chips{list-style:none;display:flex;flex-wrap:wrap;gap:.4rem;margin:0 0 1rem;padding:0}
.chip{font:600 .71875rem/1.6 var(--mono);letter-spacing:.02em;padding:.05em .6em;\
border:1px solid var(--rule);border-radius:999px;background:var(--chip);color:var(--dim)}
.stats{display:flex;flex-wrap:wrap;gap:1rem 3rem;margin:.5rem 0 1.25rem}
.stat{display:flex;flex-direction:column}
.stat-value{font-size:1.45rem;font-weight:600;line-height:1.3}
.stat-label{font-size:.8125rem;color:var(--dim)}
.cards{list-style:none;display:grid;grid-template-columns:repeat(auto-fill,minmax(19rem,1fr));\
gap:1rem;margin:.75rem auto 1rem;padding:0}
.card{position:relative;display:flex;flex-direction:column;gap:.4rem;border:1px solid var(--rule);\
border-radius:10px;background:var(--surface);padding:1.1rem 1.25rem;min-width:0}
.card:hover{border-color:var(--dim)}
.card h3{font-family:var(--serif);font-size:1.2rem;font-weight:600;line-height:1.3;margin:0}
.card h3 a{color:var(--fg);text-decoration:none}
.card h3 a::after{content:\"\";position:absolute;inset:0}
.card h3 a:hover{color:var(--accent)}
.card .card-desc{color:var(--dim);font-size:.875rem;line-height:1.5;margin:0;display:-webkit-box;\
-webkit-line-clamp:3;-webkit-box-orient:vertical;overflow:hidden}
.card .chips{margin:0}
.card .card-meta{margin-top:auto;padding-top:.4rem;font-size:.8125rem;color:var(--dim);\
font-variant-numeric:tabular-nums}
.card .card-meta strong{color:var(--fg);font-weight:600}
.pager{margin:1.25rem 0}
.pager a{display:inline-block;border:1px solid var(--rule);border-radius:8px;padding:.45rem .9rem;\
text-decoration:none;font-weight:550;font-size:.9375rem;background:var(--surface)}
.pager a:hover{border-color:var(--accent)}
.resource-head{margin:0 0 1.5rem}
.resource-head .r-label{font-family:var(--serif);font-size:1.35rem;font-weight:600;display:block}
.resource-head code{font-size:.8125rem}
.query-form{border:1px solid var(--rule);border-radius:10px;margin:.75rem 0;background:var(--surface)}
.query-form summary{cursor:pointer;font-weight:600;font-size:.9375rem;padding:.7rem 1.1rem;\
color:var(--fg)}
.query-form summary:hover{color:var(--accent)}
.query-form[open] summary{border-bottom:1px solid var(--rule-soft)}
.query-form form{padding:1.1rem}
.form-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(13rem,1fr));gap:.9rem 1rem;\
margin-bottom:1rem}
.form-grid>label{display:flex;flex-direction:column;gap:.3rem;min-width:0}
.control-label,.choice legend{font-size:.8125rem;color:var(--dim)}
.choice{border:0;padding:0;margin:0;min-width:0}
.choice label{margin-right:1rem}
input,select,button{font:inherit;color:var(--fg)}
input[type=text],input[type=number],select{width:100%;min-width:0;padding:.45rem .6rem;\
border:1px solid var(--rule);border-radius:6px;background:var(--bg)}
input:focus-visible,select:focus-visible{outline:2px solid var(--accent);outline-offset:1px}
input::placeholder{color:var(--dim);opacity:.6}
button{border:1px solid var(--accent);border-radius:6px;padding:.45rem .95rem;\
background:var(--accent);color:var(--accent-ink);font-weight:600;cursor:pointer}
button:hover{filter:brightness(1.08)}
footer{border-top:1px solid var(--rule);font-size:.8125rem;color:var(--dim)}
footer{max-width:102rem;margin:0 auto;padding:1rem 1.5rem 2rem}
footer a{color:var(--accent);text-decoration:none}
footer a:hover{text-decoration:underline}
";

/// Native forms include untouched optional controls as empty values. The
/// server gives those the same meaning as omission; removing them here is a
/// progressive enhancement that also keeps the browser's address-bar URL
/// canonical. Required controls are never empty after browser validation.
///
/// This is intentionally the whole client-side layer: navigation and response
/// rendering remain ordinary HTTP, and fixed source contains no interpolated
/// bundle data.
const FORM_SCRIPT: &str = "\
document.addEventListener('formdata',event=>{\
for(const [name,value] of Array.from(event.formData.entries())){\
if(typeof value==='string'&&value==='')event.formData.delete(name);\
}\
});";

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
                (chips(&[hostile]))
                (stats(&[(hostile, hostile.to_owned())]))
                (results_table(
                    &[hostile],
                    &[vec![Value::Link {
                        href: "/x\"onmouseover=alert(1)".to_owned(),
                        label: hostile,
                    }, Value::TermLink {
                        href: "/term".to_owned(),
                        term: TermText {
                            primary: hostile,
                            qualifier: Some(hostile),
                            annotation: Some(hostile),
                            full_iri: Some(hostile),
                        },
                    }]],
                ))
            },
        );

        assert!(
            !rendered.contains("<script>alert"),
            "unescaped bundle data reached executable markup"
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
        assert!(rendered.contains("title=\"&lt;script&gt;"));
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
    fn the_masthead_brand_is_the_root_link_and_crumbs_follow_it() {
        let rendered = page(
            "fragment — tox v1",
            &[Crumb::to("tox", "/tox".to_owned()), Crumb::here("v1")],
            None,
            html! {},
        );
        assert!(rendered.contains("class=\"brand\" href=\"/\""));
        assert!(rendered.contains("href=\"/tox\""));
        assert!(rendered.contains("<span class=\"here\">v1</span>"));
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
    fn compact_numbers_truncate_and_never_inflate() {
        assert_eq!(compact_number(0), "0");
        assert_eq!(compact_number(9_999), "9\u{202f}999");
        assert_eq!(compact_number(10_000), "10\u{202f}K");
        assert_eq!(compact_number(17_900), "17.9\u{202f}K");
        assert_eq!(compact_number(999_999), "999\u{202f}K");
        assert_eq!(compact_number(1_500_000), "1.5\u{202f}M");
        // Truncated, not rounded: a card must not claim more than the bundle.
        assert_eq!(compact_number(606_342_307), "606\u{202f}M");
        assert_eq!(compact_number(2_145_008_412), "2.1\u{202f}B");
    }

    #[test]
    fn absent_fields_leave_no_empty_row() {
        let rendered =
            fields(&[("present", Value::Text("yes")), ("missing", Value::Absent)]).into_string();
        assert!(rendered.contains("present"));
        assert!(!rendered.contains("missing"));
    }

    #[test]
    fn a_results_table_is_wide_and_a_descriptor_table_is_not() {
        let wide = results_table(&["h"], &[vec![Value::Text("x")]]).into_string();
        assert!(wide.contains("class=\"scroll wide\""));
        let narrow = table(&["h"], &[vec![Value::Text("x")]]).into_string();
        assert!(narrow.contains("class=\"scroll\""));
        assert!(!narrow.contains("wide"));
    }
}
