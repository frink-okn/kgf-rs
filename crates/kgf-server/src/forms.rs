//! Native HTML forms for the GET operations.
//!
//! These are deliberately ordinary forms: submitting one navigates to the
//! operation URL and content negotiation returns the result page. There is no
//! client-side request layer and no hidden `format` parameter. The server
//! treats blank optional controls as omitted; the shared page shell also
//! removes them from submitted form data so JavaScript-capable browsers get a
//! clean address-bar URL.

use kgf_store::Capability;
use kgf_store::manifest::Manifest;
use maud::{Markup, html};

use crate::answer::Offers;
use crate::url::{Mount, Params};

/// All runnable GET forms for one bundle manifest.
pub(crate) fn manifest_forms(
    mount: &Mount,
    dataset: &str,
    version: &str,
    manifest: &Manifest,
) -> Markup {
    let empty = Params::default();
    let offers = Offers {
        search: manifest.declares(Capability::Search),
        graphs: manifest.declares(Capability::Graphs),
    };
    let search = offers.search;

    html! {
        h2 { "Try a query" }
        p."note" {
            "Blank subject, predicate and object fields mean any term. Terms accept the CURIEs "
            "listed above, bracketed IRIs, blank nodes and quoted literals."
        }
        div."query-stack" {
            (fragment(mount, dataset, version, &empty, offers, true))
            (tpf(mount, dataset, version, &empty, offers, false))
            (count(mount, dataset, version, &empty, offers, false))
            (describe(mount, dataset, version, &empty, false))
            (sample(mount, dataset, version, &empty, false))
            @if search {
                (search_form(mount, dataset, version, &empty, false))
            }
            (terms(mount, dataset, version, &empty, false))
        }
    }
}

/// The editor shown above an existing GET answer.
pub(crate) fn operation_form(
    mount: &Mount,
    dataset: &str,
    version: &str,
    operation: &str,
    params: &Params,
    offers: Offers,
) -> Option<Markup> {
    let form = match operation {
        "fragment" => Some(fragment(mount, dataset, version, params, offers, false)),
        "tpf" => Some(tpf(mount, dataset, version, params, offers, false)),
        "count" => Some(count(mount, dataset, version, params, offers, false)),
        "describe" => Some(describe(mount, dataset, version, params, false)),
        "sample" => Some(sample(mount, dataset, version, params, false)),
        "search" => Some(search_form(mount, dataset, version, params, false)),
        "terms" => Some(terms(mount, dataset, version, params, false)),
        _ => None,
    }?;
    Some(html! { div."query-stack" { (form) } })
}

fn tpf(
    mount: &Mount,
    dataset: &str,
    version: &str,
    params: &Params,
    offers: Offers,
    open: bool,
) -> Markup {
    let mut controls = vec![
        term_control(
            "tpf",
            "subject",
            "Subject",
            params.get("subject"),
            "http://example.org/subject",
        ),
        term_control(
            "tpf",
            "predicate",
            "Predicate",
            params.get("predicate"),
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
        ),
        term_control(
            "tpf",
            "object",
            "Object",
            params.get("object"),
            "http://example.org/object or \"text\"@en",
        ),
    ];
    if offers.graphs {
        controls.push(text_control(
            "tpf",
            "graph",
            "Graph",
            params.get("graph"),
            "urn:x-kgf:union, a graph IRI, or blank for every graph",
            false,
        ));
    }
    controls.push(number_control(
        "tpf",
        "limit",
        "Rows",
        params.get("limit"),
        1,
        "server default",
    ));
    form(
        "TPF",
        "Browse the standard Triple Pattern Fragments interface. IRIs are bare, not CURIEs or angle-bracketed.",
        mount.operation(dataset, version, "tpf"),
        controls,
        "Find triples",
        open,
    )
}

fn fragment(
    mount: &Mount,
    dataset: &str,
    version: &str,
    params: &Params,
    offers: Offers,
    open: bool,
) -> Markup {
    let mut controls = vec![
        term_control("fragment", "s", "Subject", params.get("s"), "ex:subject"),
        term_control("fragment", "p", "Predicate", params.get("p"), "rdf:type"),
        term_control(
            "fragment",
            "o",
            "Object",
            params.get("o"),
            "ex:object or \"text\"@en",
        ),
    ];
    if offers.search {
        controls.push(text_control(
            "fragment",
            "o.text",
            "Object text",
            params.get("o.text"),
            "matching words",
            false,
        ));
    }
    if offers.graphs {
        controls.push(graph_control("fragment", params.get("g")));
    }
    controls.push(number_control(
        "fragment",
        "limit",
        "Rows",
        params.get("limit"),
        1,
        "server default",
    ));
    form(
        "Fragment",
        "Browse one page of a triple pattern. Bind the object or search its text, not both.",
        mount.operation(dataset, version, "fragment"),
        controls,
        "Find triples",
        open,
    )
}

fn count(
    mount: &Mount,
    dataset: &str,
    version: &str,
    params: &Params,
    offers: Offers,
    open: bool,
) -> Markup {
    let mut controls = vec![
        term_control("count", "s", "Subject", params.get("s"), "ex:subject"),
        term_control("count", "p", "Predicate", params.get("p"), "rdf:type"),
        term_control(
            "count",
            "o",
            "Object",
            params.get("o"),
            "ex:object or \"text\"@en",
        ),
    ];
    if offers.search {
        controls.push(text_control(
            "count",
            "o.text",
            "Object text",
            params.get("o.text"),
            "matching words",
            false,
        ));
    }
    if offers.graphs {
        controls.push(graph_control("count", params.get("g")));
    }
    form(
        "Count",
        "Count a triple pattern without transferring its rows.",
        mount.operation(dataset, version, "count"),
        controls,
        "Count triples",
        open,
    )
}

fn describe(mount: &Mount, dataset: &str, version: &str, params: &Params, open: bool) -> Markup {
    let direction = params.get("direction").unwrap_or("both");
    form(
        "Describe",
        "Browse the incoming and outgoing statements around one RDF term.",
        mount.operation(dataset, version, "describe"),
        vec![
            text_control(
                "describe",
                "iri",
                "Resource",
                params.get("iri"),
                "ex:resource or <https://example.org/resource>",
                true,
            ),
            html! {
                label for="describe-direction" {
                    span."control-label" { "Direction " code { "direction" } }
                    select id="describe-direction" name="direction" {
                        option value="both" selected[direction == "both"] { "both" }
                        option value="out" selected[direction == "out"] { "out" }
                        option value="in" selected[direction == "in"] { "in" }
                    }
                }
            },
            number_control(
                "describe",
                "limit",
                "Rows",
                params.get("limit"),
                1,
                "server default",
            ),
        ],
        "Describe resource",
        open,
    )
}

fn sample(mount: &Mount, dataset: &str, version: &str, params: &Params, open: bool) -> Markup {
    form(
        "Sample",
        "Draw deterministic pseudo-random members of a triple pattern.",
        mount.operation(dataset, version, "sample"),
        vec![
            term_control("sample", "s", "Subject", params.get("s"), "ex:subject"),
            term_control("sample", "p", "Predicate", params.get("p"), "rdf:type"),
            term_control(
                "sample",
                "o",
                "Object",
                params.get("o"),
                "ex:object or \"text\"@en",
            ),
            number_control(
                "sample",
                "n",
                "Members",
                params.get("n"),
                1,
                "server default",
            ),
            number_control("sample", "seed", "Seed", params.get("seed"), 0, "0"),
        ],
        "Draw sample",
        open,
    )
}

fn search_form(mount: &Mount, dataset: &str, version: &str, params: &Params, open: bool) -> Markup {
    let labels = params.get("labels").unwrap_or("true");
    form(
        "Search",
        "Find entities through their matching literal values. Roles and predicates are optional scopes.",
        mount.operation(dataset, version, "search"),
        vec![
            text_control(
                "search",
                "q",
                "Text",
                params.get("q"),
                "atrazine degradation",
                true,
            ),
            text_control(
                "search",
                "role",
                "Roles",
                params.get("role"),
                "label,synonym",
                false,
            ),
            text_control(
                "search",
                "predicate",
                "Predicates",
                params.get("predicate"),
                "rdfs:label,skos:prefLabel",
                false,
            ),
            html! {
                fieldset."choice" {
                    legend { "Preferred labels " code { "labels" } }
                    label for="search-labels-yes" {
                        input id="search-labels-yes" type="radio" name="labels" value="true"
                            checked[labels != "false"];
                        " yes"
                    }
                    label for="search-labels-no" {
                        input id="search-labels-no" type="radio" name="labels" value="false"
                            checked[labels == "false"];
                        " no"
                    }
                }
            },
            number_control(
                "search",
                "limit",
                "Results",
                params.get("limit"),
                1,
                "server default",
            ),
        ],
        "Search entities",
        open,
    )
}

/// The dictionary prefix scan.
///
/// No `count` control, deliberately. A count and a page are two answers, and
/// `count=true` refuses the page-shaped parameters rather than ignoring them —
/// so a control here would make the ordinary case of an echoed `limit` a
/// refusal. The count is reached from the answer page instead, where the link is
/// built from the two parameters that determine it.
fn terms(mount: &Mount, dataset: &str, version: &str, params: &Params, open: bool) -> Markup {
    let role = params.get("role").unwrap_or("any");
    let labels = params.get("labels").unwrap_or("true");
    form(
        "Terms",
        "List the sorted dictionary under one byte prefix. An IRI prefix is written bare, \
         without angle brackets, and a literal prefix carries its opening quote.",
        mount.operation(dataset, version, "terms"),
        vec![
            text_control(
                "terms",
                "prefix",
                "Prefix",
                params.get("prefix"),
                "http://purl.obolibrary.org/obo/MONDO_",
                false,
            ),
            html! {
                label for="terms-role" {
                    span."control-label" { "Position " code { "role" } }
                    select id="terms-role" name="role" {
                        @for (value, text) in [
                            ("any", "any"),
                            ("subject", "subject"),
                            ("predicate", "predicate"),
                            ("object", "object"),
                        ] {
                            option value=(value) selected[role == value] { (text) }
                        }
                    }
                }
            },
            html! {
                fieldset."choice" {
                    legend { "Preferred labels " code { "labels" } }
                    label for="terms-labels-yes" {
                        input id="terms-labels-yes" type="radio" name="labels" value="true"
                            checked[labels != "false"];
                        " yes"
                    }
                    label for="terms-labels-no" {
                        input id="terms-labels-no" type="radio" name="labels" value="false"
                            checked[labels == "false"];
                        " no"
                    }
                }
            },
            number_control(
                "terms",
                "limit",
                "Terms",
                params.get("limit"),
                1,
                "server default",
            ),
        ],
        "List terms",
        open,
    )
}

fn form(
    title: &str,
    description: &str,
    action: String,
    controls: Vec<Markup>,
    submit: &str,
    open: bool,
) -> Markup {
    html! {
        details."query-form" open[open] {
            summary { (title) }
            form method="get" action=(action) {
                p."note" { (description) }
                div."form-grid" {
                    @for control in controls {
                        (control)
                    }
                }
                button type="submit" { (submit) }
            }
        }
    }
}

/// The `g` control: a graph IRI, one of the two reserved names, or `*`.
fn graph_control(operation: &str, value: Option<&str>) -> Markup {
    text_control(
        operation,
        "g",
        "Graph",
        value,
        "<urn:x-kgf:unnamed>, a graph IRI, or *",
        false,
    )
}

fn term_control(
    operation: &str,
    name: &str,
    label: &str,
    value: Option<&str>,
    placeholder: &str,
) -> Markup {
    text_control(operation, name, label, value, placeholder, false)
}

fn text_control(
    operation: &str,
    name: &str,
    label: &str,
    value: Option<&str>,
    placeholder: &str,
    required: bool,
) -> Markup {
    let id = format!("{operation}-{}", name.replace('.', "-"));
    html! {
        label for=(id) {
            span."control-label" { (label) " " code { (name) } }
            input
                id=(id)
                type="text"
                name=(name)
                value=(value.unwrap_or(""))
                placeholder=(placeholder)
                autocomplete="off"
                spellcheck="false"
                required[required];
        }
    }
}

fn number_control(
    operation: &str,
    name: &str,
    label: &str,
    value: Option<&str>,
    min: u64,
    placeholder: &str,
) -> Markup {
    let id = format!("{operation}-{name}");
    html! {
        label for=(id) {
            span."control-label" { (label) " " code { (name) } }
            input
                id=(id)
                type="number"
                name=(name)
                value=(value.unwrap_or(""))
                min=(min)
                step="1"
                placeholder=(placeholder)
                inputmode="numeric";
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use kgf_store::manifest::{Counts, Formats};

    use super::*;

    fn manifest(capabilities: &[Capability]) -> Manifest {
        Manifest {
            id: "tox".to_owned(),
            dataset_iri: None,
            version: "v1".to_owned(),
            content_digest: "sha256:0123456789abcdef".to_owned(),
            created: None,
            formats: Formats::default(),
            title: None,
            description: None,
            license: None,
            homepage: None,
            publisher: None,
            counts: Counts {
                triples: 0,
                subjects: 0,
                predicates: 0,
                objects: 0,
            },
            capabilities: capabilities
                .iter()
                .map(|capability| (capability.as_str().to_owned(), serde_json::json!({})))
                .collect(),
            prefixes: BTreeMap::new(),
            predicate_roles: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            previous_version: None,
            components: Vec::new(),
            source: None,
        }
    }

    #[test]
    fn manifest_forms_follow_capabilities() {
        // Every form whose operation needs no sidecar is offered whatever the
        // manifest says, because no bundle can fail to answer it. Only the ones
        // reading bytes a bundle may not carry follow a declaration.
        let core = manifest_forms(&Mount::default(), "tox", "v1", &manifest(&[])).into_string();
        assert!(core.contains("class=\"query-stack\""));
        for operation in ["fragment", "tpf", "count", "describe", "sample", "terms"] {
            assert!(
                core.contains(&format!("action=\"/tox/v/v1/{operation}\"")),
                "{operation}"
            );
        }
        assert!(!core.contains("action=\"/tox/v/v1/search\""));
        assert!(!core.contains("name=\"o.text\""));

        let searchable = manifest_forms(
            &Mount::default(),
            "tox",
            "v1",
            &manifest(&[Capability::Search]),
        )
        .into_string();
        assert!(searchable.contains("action=\"/tox/v/v1/search\""));
        assert!(searchable.contains("name=\"o.text\""));
    }

    #[test]
    fn an_answer_form_is_prefilled_but_never_carries_paging_or_format() {
        let params = Params::parse(Some("p=ex%3Aknows&limit=7&cursor=opaque&format=html")).unwrap();
        let rendered = operation_form(
            &Mount::default(),
            "tox",
            "v1",
            "fragment",
            &params,
            Offers::default(),
        )
        .unwrap()
        .into_string();
        assert!(rendered.contains("name=\"p\" value=\"ex:knows\""));
        assert!(rendered.contains("name=\"limit\" value=\"7\""));
        assert!(!rendered.contains("name=\"cursor\""));
        assert!(!rendered.contains("name=\"format\""));
    }

    #[test]
    fn a_mounted_deployment_submits_its_forms_under_the_prefix() {
        // The action is what the browser requests on submit; under a gateway
        // that strips `/kgf`, an action without it leaves the route entirely.
        let mounted = "https://apps.okn.us/kgf"
            .parse::<crate::PublicBase>()
            .unwrap()
            .mount();
        let rendered = manifest_forms(
            &mounted,
            "tox",
            "v1",
            &manifest(&[Capability::Sample, Capability::Search, Capability::Terms]),
        )
        .into_string();
        for operation in [
            "fragment", "tpf", "count", "describe", "sample", "search", "terms",
        ] {
            assert!(
                rendered.contains(&format!("action=\"/kgf/tox/v/v1/{operation}\"")),
                "{operation}"
            );
        }
        assert!(!rendered.contains("action=\"/tox/"));
    }

    #[test]
    fn form_values_and_actions_are_escaped_by_maud() {
        let params = Params::parse(Some("s=%22%3E%3Cscript%3E")).unwrap();
        let rendered = operation_form(
            &Mount::default(),
            "a b",
            "v?1",
            "fragment",
            &params,
            Offers::default(),
        )
        .unwrap()
        .into_string();
        assert!(rendered.contains("action=\"/a%20b/v/v%3F1/fragment\""));
        assert!(!rendered.contains("<script>"));
        assert!(rendered.contains("&lt;script&gt;"));
    }
}
