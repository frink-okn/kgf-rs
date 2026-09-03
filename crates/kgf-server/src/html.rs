//! The page a browser gets, for every resource that has one.
//!
//! The API is usable by an agent without a client library; the same should be
//! true for a person with a browser, and the two need not
//! be different endpoints. Every route here answers both, chosen by `Accept`
//! alone (see [`crate::representation`]) — a page when a browser navigates to
//! it, JSON when anything else fetches it, at one URL.
//!
//! # One browser workbench
//!
//! The browser representation is an application surface, not an editorial
//! rendering of the JSON. Neutral panels establish the service, dataset, query
//! and result hierarchy; monospace is reserved for terms and protocol values;
//! and the content column uses the available viewport instead of making data
//! tables escape a prose-shaped page. The catalog and operation pages still
//! share one stylesheet, so the visual language cannot drift between routes.
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

use crate::representation::Representation;
use crate::url::Mount;

/// What the pages call themselves.
pub const SITE: &str = "Knowledge Graph Fragments";

/// A resource this server can serve in every representation it offers.
pub trait Resource {
    /// The canonical machine-readable form.
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
/// Pretty-printed: these documents are read by people as often as by programs,
/// and descriptors are the first resource a client reads. A
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
/// `mount` is where the deployment lives, which the masthead's brand link
/// points at; a caller's own links are already built against the same mount.
/// `canonical` is this resource's own URL, which the footer turns into a link
/// to its JSON. It is the resource's canonical URL rather than the one the
/// request arrived on, so a page reached through `latest` links to the version
/// it actually resolved to.
pub fn page(
    mount: &Mount,
    title: &str,
    crumbs: &[Crumb<'_>],
    canonical: Option<&str>,
    body: Markup,
) -> String {
    page_document(
        mount,
        title,
        None,
        crumbs,
        canonical,
        Representation::Json,
        body,
    )
}

/// An operation page whose route and release are quiet context above the
/// page's actual subject.
pub fn operation_page(
    mount: &Mount,
    title: &str,
    context: &str,
    crumbs: &[Crumb<'_>],
    canonical: Option<&str>,
    body: Markup,
) -> String {
    operation_page_with_format(
        mount,
        title,
        context,
        crumbs,
        canonical,
        Representation::Json,
        body,
    )
}

/// An operation page whose machine-readable alternate is not ordinary JSON.
pub fn operation_page_with_format(
    mount: &Mount,
    title: &str,
    context: &str,
    crumbs: &[Crumb<'_>],
    canonical: Option<&str>,
    alternate: Representation,
    body: Markup,
) -> String {
    debug_assert_ne!(
        alternate,
        Representation::Html,
        "a page alternate must be a machine representation"
    );
    page_document(
        mount,
        title,
        Some(context),
        crumbs,
        canonical,
        alternate,
        body,
    )
}

fn page_document(
    mount: &Mount,
    title: &str,
    context: Option<&str>,
    crumbs: &[Crumb<'_>],
    canonical: Option<&str>,
    alternate: Representation,
    body: Markup,
) -> String {
    // The service descriptor's own title *is* the site name, and a tab reading
    // "Knowledge Graph Fragments — Knowledge Graph Fragments" is the classic
    // template seam.
    let full_title = if title == SITE {
        SITE.to_owned()
    } else {
        format!("{title} — {SITE}")
    };
    let machine = canonical.map(|url| {
        let separator = if url.contains('?') { '&' } else { '?' };
        format!("{url}{separator}format={}", alternate.token())
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
                header."app-header" {
                    nav."crumbs" aria-label="Breadcrumb" {
                        a."brand" href=(mount.root()) {
                            span."brand-mark" aria-hidden="true" { "KGF" }
                            span."brand-name" { (SITE) }
                        }
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
                    div."page-heading" {
                        @if let Some(context) = context {
                            div."page-title" {
                                p."operation-context" { (context) }
                                h1 { (title) }
                            }
                        } @else {
                            h1 { (title) }
                        }
                        @if let Some(machine) = &machine {
                            a."json-action" href=(machine) rel="nofollow" {
                                "View " (alternate.label())
                            }
                        }
                    }
                    (body)
                }
                footer {
                    div."footer-inner" {
                        @if let Some(machine) = &machine {
                            a href=(machine) rel="nofollow" {
                                "This page as " (alternate.label())
                            }
                            span."sep" { "·" }
                        }
                        span { "one URL for people and software, selected by Accept" }
                    }
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
    /// An RDF term that is intentionally not a link — notably the focus term
    /// repeated in a `/describe` row.
    Term {
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
                    a."term" href=(href) rel="nofollow" title=[term.full_iri] {
                        (term.primary)
                        @if let Some(qualifier) = term.qualifier {
                            span."t-qual" { (qualifier) }
                        }
                        @if let Some(annotation) = term.annotation {
                            span."t-label" { (annotation) }
                        }
                    }
                },
                Value::Term { term } => {
                    span."term"."term-static" title=[term.full_iri] {
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
        dl."fields" {
            @for (label, value) in rows {
                @if !matches!(value, Value::Absent) {
                    div."field" {
                        dt { (label) }
                        dd { (value) }
                    }
                }
            }
        }
    }
}

/// A descriptor table, horizontally scrollable so a wide row cannot widen the
/// page.
pub fn table(headers: &[&str], rows: &[Vec<Value<'_>>]) -> Markup {
    table_markup(headers, rows, false)
}

/// A results table using the full workbench width — the primary registry
/// surface for row data.
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

/// A link that leads further into the data: a continuation, or the same query
/// carried to another operation.
///
/// Marked `nofollow` for the reason a term link is. Everything such a link
/// reaches narrows the resource, and a narrowed page is served `noindex`, so
/// following one can never arrive anywhere an index would keep it — the fetch
/// is spent for nothing. Carrying the mark on the link rather than on the
/// response is what makes it usable here at all: a page-wide directive would
/// take the breadcrumbs and the catalog links with it, and those are the ones
/// worth following, being how an operation page is tied to the dataset it
/// belongs to.
pub fn pager(href: &str, label: &str) -> Markup {
    html! { p."pager" { a href=(href) rel="nofollow" { (label) } } }
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
/// entry and another thing that can 404 while the page still renders. System
/// sans and mono stacks keep the page a single request.
const STYLE: &str = r#"
:root {
  color-scheme: light dark;
  --bg: #f4f7fb;
  --surface: #ffffff;
  --surface-raised: #ffffff;
  --surface-muted: #f8fafc;
  --fg: #172033;
  --dim: #657189;
  --muted: #8a96aa;
  --rule: #dce3ed;
  --rule-strong: #c9d3e1;
  --code: #eef2f7;
  --chip: #f2edff;
  --brand: #2f204d;
  --accent: #6d28d9;
  --accent-hover: #5b21b6;
  --accent-soft: #f2eaff;
  --accent-ink: #ffffff;
  --hover: #f6f9ff;
  --shadow: 0 1px 2px rgba(15, 23, 42, .04), 0 8px 24px rgba(15, 23, 42, .05);
  --sans: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Helvetica, Arial, sans-serif;
  --mono: ui-monospace, "SF Mono", SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0b1220;
    --surface: #111b2d;
    --surface-raised: #152136;
    --surface-muted: #0e1829;
    --fg: #e6edf7;
    --dim: #9aa8bd;
    --muted: #718099;
    --rule: #27354a;
    --rule-strong: #36465e;
    --code: #1a2639;
    --chip: #2d2149;
    --brand: #24173d;
    --accent: #b795ff;
    --accent-hover: #ccb6ff;
    --accent-soft: #2b2044;
    --accent-ink: #081120;
    --hover: #142238;
    --shadow: 0 1px 2px rgba(0, 0, 0, .24), 0 10px 28px rgba(0, 0, 0, .16);
  }
}
* { box-sizing: border-box; }
body {
  margin: 0;
  background: var(--bg);
  color: var(--fg);
  font: 15px/1.6 var(--sans);
  -webkit-font-smoothing: antialiased;
  text-rendering: optimizeLegibility;
}
.app-header {
  position: sticky;
  top: 0;
  z-index: 20;
  border-bottom: 1px solid color-mix(in srgb, white 14%, transparent);
  background: color-mix(in srgb, var(--brand) 96%, transparent);
  backdrop-filter: blur(12px);
}
.crumbs, main, .footer-inner {
  width: min(calc(100% - 3rem), 82rem);
  margin-inline: auto;
}
.crumbs {
  min-height: 3.65rem;
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0;
  font-size: .8125rem;
  color: rgba(255, 255, 255, .7);
}
.brand {
  display: inline-flex;
  align-items: center;
  gap: .65rem;
  color: white;
  font-weight: 650;
  letter-spacing: -.01em;
  text-decoration: none;
}
.brand-mark {
  display: grid;
  place-items: center;
  width: 2rem;
  height: 2rem;
  border-radius: .55rem;
  border: 1px solid rgba(255, 255, 255, .24);
  background: rgba(255, 255, 255, .1);
  color: white;
  font: 700 .67rem/1 var(--sans);
  letter-spacing: .07em;
  box-shadow: 0 4px 12px color-mix(in srgb, var(--accent) 24%, transparent);
}
.brand:hover .brand-name { color: white; opacity: .82; }
.crumbs a:not(.brand) { color: rgba(255, 255, 255, .72); text-decoration: none; }
.crumbs a:not(.brand):hover { color: white; }
.crumbs .here { color: white; font-weight: 550; }
.sep { padding: 0 .55rem; color: rgba(255, 255, 255, .38); }
main { min-height: calc(100vh - 9rem); padding-block: 2.25rem 4.5rem; }
.page-heading {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 1.5rem;
  margin-bottom: 1.5rem;
}
h1 {
  margin: 0;
  font-size: clamp(1.75rem, 1.45rem + 1.25vw, 2.45rem);
  line-height: 1.18;
  font-weight: 680;
  letter-spacing: -.035em;
}
h2 {
  margin: 0 0 .45rem;
  font-size: 1rem;
  line-height: 1.35;
  font-weight: 680;
  letter-spacing: -.01em;
  color: var(--fg);
}
h3 { margin: 0; }
p { margin: 0 0 1rem; }
.lede { max-width: 52rem; margin-bottom: 1.4rem; color: var(--dim); font-size: 1.05rem; line-height: 1.65; }
.note { max-width: 58rem; color: var(--dim); font-size: .875rem; }
a { color: var(--accent); }
.json-action, .pager a {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  min-height: 2.25rem;
  border: 1px solid var(--rule-strong);
  border-radius: .55rem;
  background: var(--surface);
  color: var(--fg);
  padding: .38rem .75rem;
  font-size: .8125rem;
  font-weight: 600;
  text-decoration: none;
  white-space: nowrap;
}
.json-action:hover, .pager a:hover { border-color: var(--accent); color: var(--accent); }
.schema-actions { margin-top: 1rem; }
.schema-actions ul {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(16rem, 100%), 1fr));
  gap: .65rem;
  margin: 0;
  padding: 0;
  list-style: none;
}
.schema-actions a {
  display: flex;
  min-height: 4.5rem;
  flex-direction: column;
  gap: .25rem;
  border: 1px solid var(--rule);
  border-radius: .7rem;
  background: var(--surface-muted);
  padding: .8rem .9rem;
  color: var(--fg);
  text-decoration: none;
}
.schema-actions a:hover { border-color: var(--accent); background: var(--accent-soft); }
.schema-actions strong { color: var(--accent); font-size: .86rem; }
.schema-actions span { color: var(--dim); font-size: .78rem; line-height: 1.45; }
code {
  border-radius: .3rem;
  background: var(--code);
  padding: .1em .34em;
  font: .875em/1.5 var(--mono);
  word-break: break-all;
}
pre {
  overflow-x: auto;
  margin: .85rem 0 0;
  border: 1px solid var(--rule);
  border-radius: .65rem;
  background: var(--surface-muted);
  padding: .9rem 1rem;
  font-size: .8125rem;
  line-height: 1.6;
}
pre code { background: none; padding: 0; word-break: normal; }
.overview, .panel, .workbench, .answer-summary {
  border: 1px solid var(--rule);
  border-radius: .85rem;
  background: var(--surface);
  box-shadow: var(--shadow);
}
.overview {
  position: relative;
  overflow: hidden;
  margin-bottom: 2rem;
  padding: 1.5rem 1.6rem;
  background: linear-gradient(135deg, var(--surface) 0%, var(--surface) 58%, var(--accent-soft) 160%);
}
.section-block { margin-top: 2.2rem; }
.section-heading { display: flex; align-items: baseline; justify-content: space-between; gap: 1rem; margin-bottom: .85rem; }
.dashboard-grid { display: grid; grid-template-columns: minmax(0, 1.1fr) minmax(19rem, .9fr); gap: 1rem; margin-top: 2rem; }
.panel { min-width: 0; padding: 1.25rem 1.35rem; }
.panel > h2:not(:first-child) { margin-top: 1.65rem; }
.fields {
  display: grid;
  gap: .5rem;
  margin: 0;
}
.field { display: grid; grid-template-columns: minmax(8.5rem, auto) minmax(0, 1fr); gap: 1.25rem; }
dt { color: var(--dim); font-size: .8125rem; }
dd { min-width: 0; margin: 0; overflow-wrap: anywhere; font-size: .9rem; }
.answer-summary { margin: 0 0 1rem; padding: .9rem 1.05rem; box-shadow: none; }
.answer-summary .fields { grid-template-columns: repeat(3, minmax(0, 1fr)); gap: .8rem 1.25rem; }
.answer-summary .field { display: block; min-width: 0; }
.answer-summary dt { margin-bottom: .14rem; font-size: .7rem; font-weight: 650; letter-spacing: .055em; text-transform: uppercase; }
.answer-summary dd { font-size: .94rem; font-weight: 580; }
.scroll {
  overflow-x: auto;
  margin: .75rem 0 1rem;
  border: 1px solid var(--rule);
  border-radius: .75rem;
  background: var(--surface);
}
.scroll.wide { width: 100%; }
table { width: 100%; border-collapse: separate; border-spacing: 0; font-size: .85rem; }
th {
  background: var(--surface-muted);
  color: var(--dim);
  font-size: .7rem;
  font-weight: 700;
  letter-spacing: .065em;
  text-align: left;
  text-transform: uppercase;
  white-space: nowrap;
}
th, td { padding: .68rem .85rem; border-bottom: 1px solid var(--rule); vertical-align: top; }
tbody tr:last-child td { border-bottom: 0; }
tbody tr:hover { background: var(--hover); }
td.num { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
a.term, span.term {
  display: inline-block;
  max-width: 38rem;
  font: .8rem/1.45 var(--mono);
  overflow-wrap: anywhere;
}
a.term {
  color: var(--accent);
  text-decoration: none;
}
a.term:hover { color: var(--accent-hover); text-decoration: underline; text-underline-offset: .18em; }
span.term-static { color: var(--dim); }
.t-qual { padding-left: .1em; color: var(--dim); font-size: .9em; }
.t-label { display: block; margin-top: .12rem; color: var(--dim); font: .78rem/1.4 var(--sans); }
.chips { display: flex; flex-wrap: wrap; gap: .38rem; margin: 0 0 1rem; padding: 0; list-style: none; }
.chip {
  border: 1px solid color-mix(in srgb, var(--accent) 18%, var(--rule));
  border-radius: 999px;
  background: var(--chip);
  color: var(--accent);
  padding: .14rem .55rem;
  font: 650 .68rem/1.45 var(--mono);
  letter-spacing: .015em;
}
.stats { display: flex; flex-wrap: wrap; gap: .8rem 2.75rem; margin: .25rem 0 .25rem; }
.stat { display: flex; flex-direction: column; min-width: 5rem; }
.stat-value { color: var(--fg); font-size: 1.35rem; font-weight: 700; line-height: 1.25; letter-spacing: -.025em; }
.stat-label { color: var(--dim); font-size: .74rem; }
.cards {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(21rem, 100%), 1fr));
  gap: 1rem;
  margin: 0;
  padding: 0;
  list-style: none;
}
.card {
  position: relative;
  display: flex;
  min-width: 0;
  min-height: 12rem;
  flex-direction: column;
  gap: .65rem;
  border: 1px solid var(--rule);
  border-radius: .85rem;
  background: var(--surface);
  padding: 1.25rem;
  box-shadow: 0 1px 2px rgba(15, 23, 42, .035);
  transition: border-color .15s ease, box-shadow .15s ease, transform .15s ease;
}
.card:hover { border-color: color-mix(in srgb, var(--accent) 46%, var(--rule)); box-shadow: var(--shadow); transform: translateY(-1px); }
.card h3 { font-size: 1.12rem; font-weight: 680; line-height: 1.35; letter-spacing: -.018em; }
.card h3 a { color: var(--fg); text-decoration: none; }
.card h3 a::after { position: absolute; inset: 0; content: ""; }
.card h3 a:hover { color: var(--accent); }
.card .card-desc { display: -webkit-box; overflow: hidden; margin: 0; color: var(--dim); font-size: .85rem; line-height: 1.5; -webkit-box-orient: vertical; -webkit-line-clamp: 3; }
.card .chips { position: relative; margin: 0; pointer-events: none; }
.card .card-meta { margin: auto 0 0; padding-top: .5rem; color: var(--dim); font-size: .78rem; font-variant-numeric: tabular-nums; }
.card .card-meta strong { color: var(--fg); font-weight: 700; }
.pager { display: flex; flex-wrap: wrap; gap: .5rem; margin: 1rem 0; }
.operation-context { margin: 0 0 .3rem; color: var(--dim); font-size: .82rem; }
.focus-identifier { margin: -1rem 0 1.15rem; }
.focus-identifier code { font-size: .78rem; }
.workbench { margin: 2rem 0; padding: 1.25rem 1.35rem; }
.query-editor { margin: .75rem 0 1.5rem; }
.query-editor .query-stack { margin-top: 0; }
.query-stack { display: flex; flex-wrap: wrap; gap: .45rem; margin-top: .9rem; }
.query-form { min-width: 8rem; margin: 0; }
.query-form summary {
  cursor: pointer;
  border: 1px solid var(--rule-strong);
  border-radius: .5rem;
  background: var(--surface);
  color: var(--dim);
  padding: .42rem .7rem;
  font-size: .82rem;
  font-weight: 650;
  list-style-position: inside;
}
.query-form summary:hover { border-color: var(--accent); color: var(--accent); }
.query-form[open] { order: 10; flex: 1 0 100%; margin-top: .2rem; border: 1px solid var(--rule); border-radius: .7rem; background: var(--surface-muted); }
.query-form[open] summary { border: 0; border-bottom: 1px solid var(--rule); border-radius: .7rem .7rem 0 0; background: var(--accent-soft); color: var(--accent); }
.query-form form { padding: 1rem; }
.query-form form > .note { margin-bottom: .9rem; }
.form-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(12rem, 1fr)); gap: .85rem 1rem; margin-bottom: 1rem; }
.form-grid > label { display: flex; min-width: 0; flex-direction: column; gap: .3rem; }
.control-label, .choice legend { color: var(--dim); font-size: .78rem; font-weight: 550; }
.choice { min-width: 0; margin: 0; border: 0; padding: 0; }
.choice label { margin-right: 1rem; }
input, select, button { color: var(--fg); font: inherit; }
input[type=text], input[type=number], select {
  width: 100%;
  height: 2.65rem;
  min-width: 0;
  border: 1px solid var(--rule-strong);
  border-radius: .48rem;
  background: var(--surface);
  padding: .5rem .62rem;
}
input:focus-visible, select:focus-visible { border-color: var(--accent); outline: 3px solid color-mix(in srgb, var(--accent) 18%, transparent); }
input::placeholder { color: var(--muted); opacity: .85; }
button {
  border: 1px solid var(--accent);
  border-radius: .5rem;
  background: var(--accent);
  color: var(--accent-ink);
  padding: .5rem .9rem;
  font-weight: 650;
  cursor: pointer;
}
button:hover { background: var(--accent-hover); }
footer { border-top: 1px solid var(--rule); color: var(--dim); font-size: .78rem; }
.footer-inner { padding-block: 1rem 2rem; }
footer a { color: var(--accent); text-decoration: none; }
footer a:hover { text-decoration: underline; }
@media (max-width: 760px) {
  .crumbs, main, .footer-inner { width: min(calc(100% - 2rem), 82rem); }
  .brand-name { display: none; }
  .crumbs { min-height: 3.25rem; }
  .page-heading { align-items: flex-start; flex-direction: column; gap: .75rem; }
  main { padding-block: 1.5rem 3rem; }
  .overview, .panel, .workbench { padding: 1rem; }
  .dashboard-grid { grid-template-columns: 1fr; }
  .answer-summary .fields { grid-template-columns: 1fr; gap: .7rem; }
  .field { grid-template-columns: 1fr; gap: .1rem; }
  .fields dd { margin-bottom: .55rem; }
  .stats { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: .9rem; }
  .form-grid { grid-template-columns: 1fr; }
  th, td { padding: .6rem .7rem; }
}
"#;

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
            &Mount::default(),
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
                    }, Value::Term {
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
        let rendered = page(&Mount::default(), "Title", &[], None, html! {});
        assert!(rendered.starts_with("<!DOCTYPE html>"));
        assert!(rendered.contains("<meta charset=\"utf-8\">"));
        assert!(rendered.contains("<title>Title — Knowledge Graph Fragments</title>"));
        assert!(rendered.trim_end().ends_with("</html>"));
        // The site's own front page is not "X — X".
        assert!(
            page(&Mount::default(), SITE, &[], None, html! {})
                .contains("<title>Knowledge Graph Fragments</title>")
        );
    }

    #[test]
    fn operation_context_precedes_the_page_subject() {
        let rendered = operation_page(
            &Mount::default(),
            "circulating cell",
            "Describe · ubergraph 2026-05-31",
            &[],
            None,
            html! {},
        );
        let context = rendered
            .find("Describe · ubergraph 2026-05-31")
            .expect("operation context");
        let subject = rendered
            .find("<h1>circulating cell</h1>")
            .expect("page subject");
        assert!(context < subject);
    }

    #[test]
    fn the_masthead_brand_points_at_the_mount() {
        let mounted = "https://apps.okn.us/kgf"
            .parse::<crate::PublicBase>()
            .unwrap()
            .mount();
        let rendered = page(&mounted, "t", &[], Some("/kgf/tox"), html! {});
        assert!(rendered.contains("class=\"brand\" href=\"/kgf/\""));
        assert!(rendered.contains("href=\"/kgf/tox?format=json\""));
    }

    #[test]
    fn the_masthead_brand_is_the_root_link_and_crumbs_follow_it() {
        let rendered = page(
            &Mount::default(),
            "fragment — tox v1",
            &[Crumb::to("tox", "/tox".to_owned()), Crumb::here("v1")],
            None,
            html! {},
        );
        assert!(rendered.contains("class=\"brand\" href=\"/\""));
        assert!(rendered.contains("class=\"brand-mark\""));
        assert!(rendered.contains("class=\"page-heading\""));
        assert!(rendered.contains("href=\"/tox\""));
        assert!(rendered.contains("<span class=\"here\">v1</span>"));
    }

    #[test]
    fn the_json_affordance_survives_a_url_that_already_has_a_query() {
        let plain = page(&Mount::default(), "t", &[], Some("/a/b"), html! {});
        assert!(plain.contains("href=\"/a/b?format=json\""));

        let queried = page(&Mount::default(), "t", &[], Some("/a/b?s=x"), html! {});
        assert!(queried.contains("href=\"/a/b?s=x&amp;format=json\""));
    }

    #[test]
    fn an_operation_can_name_a_non_json_machine_alternate() {
        let rendered = operation_page_with_format(
            &Mount::default(),
            "VoID",
            "tox v1",
            &[],
            Some("/tox/v/v1/void"),
            Representation::JsonLd,
            html! {},
        );
        assert!(rendered.contains("View JSON-LD"));
        assert!(rendered.contains("href=\"/tox/v/v1/void?format=jsonld\""));
        assert!(!rendered.contains("/tox/v/v1/void?format=json\""));
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
