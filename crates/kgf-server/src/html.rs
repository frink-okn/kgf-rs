//! The page a browser gets, for every resource that has one.
//!
//! Doc 01 argues for an interface an agent can use without a client library;
//! the same argument applies to a person with a browser, and the two need not
//! be different endpoints. Every route here answers both, chosen by `Accept`
//! alone (see [`crate::representation`]) — a page when a browser navigates to
//! it, JSON when anything else fetches it, at one URL.
//!
//! # Escaping is structural
//!
//! [`Page`] is a builder, not a template. Nothing writes markup by
//! interpolation: every method that takes caller data escapes it, and the only
//! markup in the output comes from `&'static str` tag names this module wrote.
//! That matters more than it might look — the data on these pages is a bundle's
//! own dictionary and manifest, so "someone published a dataset whose title
//! contains a `<script>` tag" is an ordinary case rather than an attack, and a
//! `format!` with an unescaped `{}` in it is the one bug this design makes
//! unavailable.
//!
//! # One trait, so a route cannot forget
//!
//! [`Resource`] pairs the two renderings. A new route implements it and gets
//! both, or does not compile — the same reason [`Representation`] is an enum
//! rather than a string.
//!
//! [`Representation`]: crate::representation::Representation

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

/// An HTML document under construction.
pub struct Page {
    title: String,
    breadcrumbs: Vec<(String, Option<String>)>,
    body: String,
    /// The canonical URL of this resource, for the "as JSON" affordance.
    canonical: Option<String>,
}

impl Page {
    /// Start a page. `title` becomes the `<title>` and the first heading.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            breadcrumbs: Vec::new(),
            body: String::new(),
            canonical: None,
        }
    }

    /// Add a trail entry; the last one is the current page and is not a link.
    pub fn crumb(mut self, label: impl Into<String>, href: Option<String>) -> Self {
        self.breadcrumbs.push((label.into(), href));
        self
    }

    /// The URL this resource's JSON lives at, linked in the footer.
    pub fn canonical(mut self, url: String) -> Self {
        self.canonical = Some(url);
        self
    }

    /// A paragraph of prose.
    pub fn paragraph(&mut self, text: &str) -> &mut Self {
        self.body.push_str(&format!("<p>{}</p>\n", escape(text)));
        self
    }

    /// A section heading.
    pub fn section(&mut self, heading: &str) -> &mut Self {
        self.body
            .push_str(&format!("<h2>{}</h2>\n", escape(heading)));
        self
    }

    /// A field list: a label and a value per row.
    pub fn fields(&mut self, rows: &[(&str, Value<'_>)]) -> &mut Self {
        self.body.push_str("<dl>\n");
        for (label, value) in rows {
            if matches!(value, Value::Absent) {
                continue;
            }
            self.body.push_str(&format!(
                "<dt>{}</dt><dd>{}</dd>\n",
                escape(label),
                render(value)
            ));
        }
        self.body.push_str("</dl>\n");
        self
    }

    /// A table. `rows` shorter or longer than `headers` is the caller's bug and
    /// renders as written; nothing here pads.
    pub fn table(&mut self, headers: &[&str], rows: &[Vec<Value<'_>>]) -> &mut Self {
        self.body
            .push_str("<div class=\"scroll\"><table>\n<thead><tr>");
        for header in headers {
            self.body.push_str(&format!("<th>{}</th>", escape(header)));
        }
        self.body.push_str("</tr></thead>\n<tbody>\n");
        for row in rows {
            self.body.push_str("<tr>");
            for cell in row {
                let class = if matches!(cell, Value::Number(_)) {
                    " class=\"num\""
                } else {
                    ""
                };
                self.body
                    .push_str(&format!("<td{class}>{}</td>", render(cell)));
            }
            self.body.push_str("</tr>\n");
        }
        self.body.push_str("</tbody>\n</table></div>\n");
        self
    }

    /// A block of already-formatted machine output, such as a manifest.
    pub fn code_block(&mut self, text: &str) -> &mut Self {
        self.body
            .push_str(&format!("<pre><code>{}</code></pre>\n", escape(text)));
        self
    }

    /// An aside: the explanation under a heading, in smaller type.
    pub fn note(&mut self, text: &str) -> &mut Self {
        self.body
            .push_str(&format!("<p class=\"note\">{}</p>\n", escape(text)));
        self
    }

    /// Finish the document.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.body.len() + STYLE.len() + 1024);
        out.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n");
        out.push_str("<meta charset=\"utf-8\">\n");
        out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
        // The service descriptor's own title *is* the site name, and a tab
        // reading "Knowledge Graph Fragments — Knowledge Graph Fragments" is
        // the classic template seam.
        let title = if self.title == SITE {
            escape(SITE)
        } else {
            format!("{} — {}", escape(&self.title), escape(SITE))
        };
        out.push_str(&format!("<title>{title}</title>\n"));
        out.push_str("<style>");
        out.push_str(STYLE);
        out.push_str("</style>\n</head>\n<body>\n<header><nav>");
        for (index, (label, href)) in self.breadcrumbs.iter().enumerate() {
            if index > 0 {
                out.push_str("<span class=\"sep\">/</span>");
            }
            match href {
                Some(href) => out.push_str(&format!(
                    "<a href=\"{}\">{}</a>",
                    escape(href),
                    escape(label)
                )),
                None => out.push_str(&format!("<span>{}</span>", escape(label))),
            }
        }
        out.push_str("</nav></header>\n<main>\n");
        out.push_str(&format!("<h1>{}</h1>\n", escape(&self.title)));
        out.push_str(&self.body);
        out.push_str("</main>\n<footer>");
        if let Some(canonical) = &self.canonical {
            // Built first, escaped once. Escaping the URL and then appending
            // raw markup around it is how an `&` reaches an attribute
            // unescaped — invalid HTML, and the same slip that lets a `"` out.
            let separator = if canonical.contains('?') { '&' } else { '?' };
            let json = format!("{canonical}{separator}format=json");
            out.push_str(&format!(
                "<a href=\"{}\">This page as JSON</a><span class=\"sep\">·</span>",
                escape(&json)
            ));
        }
        out.push_str(
            "<span>the same URL answers JSON to anything that does not ask for HTML</span>",
        );
        out.push_str("</footer>\n</body>\n</html>\n");
        out
    }
}

fn render(value: &Value<'_>) -> String {
    match value {
        Value::Text(text) => escape(text),
        Value::Code(text) => format!("<code>{}</code>", escape(text)),
        Value::Number(number) => escape(&group_digits(*number)),
        Value::Link { href, label } => {
            format!("<a href=\"{}\">{}</a>", escape(href), escape(label))
        }
        Value::Absent => String::new(),
    }
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

/// Escape text for both element content and double-quoted attribute values.
///
/// All five, not the three that element content needs: the same function is
/// used for `href="…"`, and an unescaped quote there ends the attribute.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
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

    #[test]
    fn every_channel_that_takes_data_escapes_it() {
        // A dataset whose title is hostile is an ordinary case: the strings on
        // these pages come from a published bundle's manifest and dictionary.
        let hostile = "<script>alert('x')</script>";
        let mut page = Page::new(hostile).crumb(hostile, Some("/a\"b".to_owned()));
        page.paragraph(hostile)
            .note(hostile)
            .section(hostile)
            .code_block(hostile)
            .fields(&[(hostile, Value::Code(hostile))])
            .table(
                &[hostile],
                &[vec![Value::Link {
                    href: "/x\"onmouseover=alert(1)".to_owned(),
                    label: hostile,
                }]],
            );
        let rendered = page.canonical("/a?q=\"".to_owned()).render();

        assert!(
            !rendered.contains("<script>"),
            "unescaped markup reached the page"
        );
        assert!(!rendered.contains("alert('x')"));
        assert!(rendered.contains("&lt;script&gt;"));
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
        let rendered = Page::new("Title").render();
        assert!(rendered.starts_with("<!doctype html>"));
        assert!(rendered.contains("<meta charset=\"utf-8\">"));
        assert!(rendered.contains("<title>Title — Knowledge Graph Fragments</title>"));
        // The site's own front page is not "X — X".
        assert!(
            Page::new(SITE)
                .render()
                .contains("<title>Knowledge Graph Fragments</title>")
        );
        assert!(rendered.trim_end().ends_with("</html>"));
    }

    #[test]
    fn the_json_affordance_survives_a_url_that_already_has_a_query() {
        let plain = Page::new("t").canonical("/a/b".to_owned()).render();
        assert!(plain.contains("href=\"/a/b?format=json\""));

        let queried = Page::new("t").canonical("/a/b?s=x".to_owned()).render();
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
        let mut page = Page::new("t");
        page.fields(&[("present", Value::Text("yes")), ("missing", Value::Absent)]);
        let rendered = page.render();
        assert!(rendered.contains("present"));
        assert!(!rendered.contains("missing"));
    }
}
