//! The three documents that describe a deployment, and the bundle manifest.
//!
//! They are split by mutability: the service descriptor at `/` is
//! host-specific, the dataset descriptor at `/{dataset}` is the logical
//! dataset's release history, and the bundle manifest at
//! `/{dataset}/v/{version}/manifest` is immutable and checksum-identified.
//! Together they provide complete self-description: a
//! client that fetches them knows the capabilities, caps, prefixes and versions
//! without being told anything out of band.
//!
//! Each is a [`Resource`], so each has a JSON form and a page, and the compiler
//! will not let one ship without the other.
//!
//! # The manifest is served as published
//!
//! [`BundleManifest`]'s JSON is the *bytes on disk*, not a re-serialization of
//! the parse. The manifest schema grows over time and a bundle may have been
//! written by a newer builder; round-tripping it through this build's
//! [`Manifest`] would silently drop `source`, `components`, a capability's
//! configuration body — everything a newer writer may define that this build
//! does not yet model. It is also what makes the served document byte-identical to the
//! one the `content_digest` was taken over. The parse is used for the page and
//! for the ETag, where a structured view is what is wanted.

use kgf_store::Capability;
use kgf_store::manifest::{Manifest, Publisher};
use serde::Serialize;

use crate::forms;
use crate::html::{
    Crumb, Resource, SITE, Value, chips, compact_number, fields, group_digits, json_body, note,
    page, stats, table,
};
use crate::service::{Dataset, PredicateRoles, Service};
use crate::url;
use maud::html;

/// The KGF protocol version this server speaks.
pub const PROTOCOL_VERSION: &str = "1";

// ---------------------------------------------------------------------------
// `/` — the service descriptor
// ---------------------------------------------------------------------------

/// This deployment: what it hosts and the limits it applies.
///
/// The caps and budgets published here are the same values the operations
/// enforce — one [`Config`](crate::Config), read by both — rather than a
/// documented number beside a separate constant. Clients can therefore rely on
/// discovery instead of assumptions.
///
/// `datasets` carries a summary rather than a bare name per dataset: title,
/// description, triple count, capabilities and the current
/// version, all read once at startup from manifests already in memory. A
/// catalog a client can *choose from* needs one round trip, not one per
/// dataset. This keeps catalog discovery to one round trip.
#[derive(Debug, Serialize)]
pub struct ServiceDescriptor<'a> {
    datasets: Vec<DatasetSummary<'a>>,
    caps: &'a crate::Caps,
    budgets: &'a crate::Budgets,
    implementation: Implementation,
}

/// One dataset's card in the service catalog.
#[derive(Debug, Serialize)]
pub struct DatasetSummary<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    triples: u64,
    current: &'a str,
    capabilities: Vec<&'a str>,
    /// The dataset descriptor, relative to this origin (see [`ReleaseEntry::url`]).
    url: String,
    /// Direct entry points into the current immutable release.
    links: ReleaseLinks,
}

/// Machine-discoverable resources belonging to one immutable release.
#[derive(Debug, Serialize)]
pub struct ReleaseLinks {
    manifest: String,
    fragment: String,
    count: String,
    describe: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    void: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sample: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    search: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    labels: Option<String>,
}

/// Which build and protocol answered.
#[derive(Debug, Serialize)]
pub struct Implementation {
    kgf: &'static str,
    protocol: &'static str,
}

impl<'a> ServiceDescriptor<'a> {
    /// Describe a running service.
    pub fn of(service: &'a Service) -> Self {
        Self {
            datasets: service
                .datasets()
                .iter()
                .map(|(name, dataset)| DatasetSummary {
                    id: name,
                    title: dataset.title(),
                    description: dataset.description(),
                    triples: dataset.triples(),
                    current: dataset.current(),
                    capabilities: dataset.capabilities().collect(),
                    url: url::dataset(name),
                    links: release_links(name, dataset.current(), dataset.current_release()),
                })
                .collect(),
            caps: &service.config().caps,
            budgets: &service.config().budgets,
            implementation: Implementation {
                kgf: env!("CARGO_PKG_VERSION"),
                protocol: PROTOCOL_VERSION,
            },
        }
    }
}

impl Resource for ServiceDescriptor<'_> {
    fn to_json(&self) -> bytes::Bytes {
        json_body(self)
    }

    fn to_html(&self) -> String {
        let total_triples: u64 = self
            .datasets
            .iter()
            .map(|dataset| dataset.triples)
            .fold(0, u64::saturating_add);

        page(
            SITE,
            &[],
            Some("/"),
            html! {
                section."overview" {
                    p."lede" {
                        "Query federated RDF knowledge graphs at bounded cost — from a browser, "
                        "curl, or an agent, at the same URLs."
                    }
                    (stats(&[
                        ("datasets", group_digits(self.datasets.len() as u64)),
                        ("triples", compact_number(total_triples)),
                        ("protocol", self.implementation.protocol.to_owned()),
                    ]))
                }

                section."section-block" {
                    div."section-heading" { h2 { "Datasets" } }
                    @if self.datasets.is_empty() {
                        (note("This server hosts no datasets."))
                    } @else {
                        ul."cards" {
                            @for dataset in &self.datasets {
                                li."card" {
                                    h3 { a href=(dataset.url) { (dataset.title.unwrap_or(dataset.id)) } }
                                    @if let Some(description) = dataset.description {
                                        p."card-desc" { (description) }
                                    }
                                    @if !dataset.capabilities.is_empty() {
                                        (chips(&dataset.capabilities))
                                    }
                                    p."card-meta" {
                                        strong { (compact_number(dataset.triples)) }
                                        " triples · " (dataset.current)
                                    }
                                }
                            }
                        }
                    }
                }

                div."dashboard-grid" {
                    section."panel" {
                        h2 { "Using this service" }
                        p."note" {
                            "Every dataset page lists its releases; a release's manifest page carries "
                            "runnable query forms, its prefixes, and its capabilities. The same URLs "
                            "answer JSON — with " code { "$KGF" } " as this server's base URL:"
                        }
                        pre {
                            code {
                                "curl \"$KGF/\"                                # this catalog\n"
                                @if let Some(first) = self.datasets.first() {
                                    "curl \"$KGF/" (first.id) "\"                        # release history\n"
                                    "curl \"$KGF/" (first.id) "/latest/fragment?limit=25\" # one page of triples"
                                } @else {
                                    "curl \"$KGF/{dataset}/latest/fragment?limit=25\""
                                }
                            }
                        }
                        h2 { "Implementation" }
                        (fields(&[
                            ("kgf", Value::Code(self.implementation.kgf)),
                            ("protocol", Value::Code(self.implementation.protocol)),
                        ]))
                    }
                    section."panel" {
                        h2 { "Request caps" }
                        (note(
                            "Requests above these published caps are refused, never silently reduced."
                        ))
                        (fields(&[
                            ("max_limit", Value::Number(u64::from(self.caps.max_limit))),
                            ("default_limit", Value::Number(u64::from(self.caps.default_limit))),
                            ("max_sample", Value::Number(u64::from(self.caps.max_sample))),
                            ("max_bindings", Value::Number(u64::from(self.caps.max_bindings))),
                            ("max_star_subjects", Value::Number(u64::from(self.caps.max_star_subjects))),
                            ("max_star_width", Value::Number(u64::from(self.caps.max_star_width))),
                            ("max_search_predicates", Value::Number(u64::from(self.caps.max_search_predicates))),
                            ("max_search_results", Value::Number(u64::from(self.caps.max_search_results))),
                            ("max_label_iris", Value::Number(u64::from(self.caps.max_label_iris))),
                            ("max_schema_items", Value::Number(u64::from(self.caps.max_schema_items))),
                        ]))
                        h2 { "Response budgets" }
                        (note(
                            "Exhausting a budget produces an explicitly incomplete response with a reason."
                        ))
                        (fields(&[
                            ("max_output_rows", Value::Number(self.budgets.max_output_rows)),
                            ("max_output_terms", Value::Number(self.budgets.max_output_terms)),
                            ("max_response_bytes", Value::Number(self.budgets.max_response_bytes)),
                            ("max_request_bytes", Value::Number(self.budgets.max_request_bytes)),
                            ("max_term_bytes", Value::Number(self.budgets.max_term_bytes)),
                            ("candidate_budget", Value::Number(self.budgets.candidate_budget)),
                            ("time_budget_ms", Value::Number(self.budgets.time_budget_ms)),
                        ]))
                    }
                }
            },
        )
    }
}

// ---------------------------------------------------------------------------
// `/{dataset}` — the dataset descriptor
// ---------------------------------------------------------------------------

/// One logical dataset and its release history.
#[derive(Debug, Serialize)]
pub struct DatasetDescriptor<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dataset_iri: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    publisher: Option<&'a Publisher>,
    predicate_roles: &'a PredicateRoles,
    current: &'a str,
    releases: Vec<ReleaseEntry<'a>>,
    /// For the page only: the current release's triple count and capability
    /// chips. The JSON reader takes both from the manifest it will fetch next.
    #[serde(skip)]
    triples: u64,
    #[serde(skip)]
    capabilities: Vec<&'a str>,
}

/// One row of a dataset descriptor's release history.
#[derive(Debug, Serialize)]
pub struct ReleaseEntry<'a> {
    version: &'a str,
    content_digest: &'a str,
    /// Where the bundle is served, as a path relative to this origin.
    ///
    /// This server is not necessarily told its own public origin — it may be
    /// behind any number of proxies — and a
    /// relative reference resolves against the request URI to the same place,
    /// so it says what it knows rather than guessing at a hostname.
    url: String,
    links: ReleaseLinks,
}

impl<'a> DatasetDescriptor<'a> {
    /// Describe one dataset.
    pub fn of(name: &'a str, dataset: &'a Dataset) -> Self {
        Self {
            id: name,
            dataset_iri: dataset.dataset_iri(),
            title: dataset.title(),
            description: dataset.description(),
            publisher: dataset.publisher(),
            predicate_roles: dataset.predicate_roles(),
            current: dataset.current(),
            releases: dataset
                .releases()
                .map(|(version, release)| ReleaseEntry {
                    version,
                    content_digest: release.content_digest().as_str(),
                    url: url::bundle_base(name, version),
                    links: release_links(name, version, release),
                })
                .collect(),
            triples: dataset.triples(),
            capabilities: dataset.capabilities().collect(),
        }
    }
}

impl Resource for DatasetDescriptor<'_> {
    fn to_json(&self) -> bytes::Bytes {
        json_body(self)
    }

    fn to_html(&self) -> String {
        let current_links = &self
            .releases
            .iter()
            .find(|release| release.version == self.current)
            .expect("the current release appears in its release history")
            .links;
        let role_members: Vec<String> = self
            .predicate_roles
            .iter()
            .map(|(_, predicates)| predicates.join(", "))
            .collect();
        let roles: Vec<_> = self
            .predicate_roles
            .iter()
            .zip(&role_members)
            .map(|((role, _), predicates)| vec![Value::Code(role), Value::Code(predicates)])
            .collect();
        let releases: Vec<_> = self
            .releases
            .iter()
            .map(|release| {
                vec![
                    Value::self_link(
                        url::operation(self.id, release.version, "manifest"),
                        release.version,
                    ),
                    if release.version == self.current {
                        Value::Text("latest")
                    } else {
                        Value::Absent
                    },
                    Value::Code(release.content_digest),
                ]
            })
            .collect();

        page(
            self.title.unwrap_or(self.id),
            &[Crumb::here(self.id)],
            Some(&url::dataset(self.id)),
            html! {
                section."overview" {
                    @if let Some(description) = self.description {
                        p."lede" { (description) }
                    }
                    (stats(&[
                        ("triples", group_digits(self.triples)),
                        ("releases", group_digits(self.releases.len() as u64)),
                    ]))
                    @if !self.capabilities.is_empty() {
                        (chips(&self.capabilities))
                    }
                    p."pager" {
                        @if let Some(summary) = &current_links.summary {
                            a href=(summary) { "Understand this graph →" }
                        }
                        @if let Some(schema) = &current_links.schema {
                            a href=(schema) { "Explore its schema →" }
                        }
                        a href=(&current_links.fragment) {
                            "Browse the data →"
                        }
                        a href=(&current_links.manifest) {
                            "Latest manifest →"
                        }
                    }
                    (fields(&[
                        ("id", Value::Code(self.id)),
                        ("dataset_iri", self.dataset_iri.map_or(Value::Absent, Value::Code)),
                        (
                            "publisher",
                            self.publisher
                                .map_or(Value::Absent, |publisher| Value::Text(&publisher.name)),
                        ),
                        (
                            "latest",
                            Value::self_link(
                                url::operation(self.id, self.current, "manifest"),
                                self.current,
                            ),
                        ),
                    ]))
                }
                div."dashboard-grid" {
                    section."panel" {
                        h2 { "Releases" }
                        (note(
                            "A version URL is immutable: the bytes it serves cannot change while it \
                             exists, which is why they are cached for a year."
                        ))
                        (table(&["Version", "", "Content digest"], &releases))
                    }
                    section."panel" {
                        h2 { "Predicate roles" }
                        (note(
                            "The current release's immutable role profile. Versioned search and label \
                             requests use the snapshot in that version's manifest."
                        ))
                        (table(&["Role", "Predicates (strongest first)"], &roles))
                    }
                }
            },
        )
    }
}

fn release_links(dataset: &str, version: &str, release: &crate::service::Release) -> ReleaseLinks {
    let operation = |name| url::operation(dataset, version, name);
    let description = release.carries_description();
    ReleaseLinks {
        manifest: operation("manifest"),
        fragment: operation("fragment"),
        count: operation("count"),
        describe: operation("describe"),
        summary: description.then(|| operation("summary")),
        schema: description.then(|| operation("schema")),
        void: description.then(|| operation("void")),
        sample: release
            .declares(Capability::Sample)
            .then(|| operation("sample")),
        search: release
            .declares(Capability::Search)
            .then(|| operation("search")),
        labels: release
            .declares(Capability::Labels)
            .then(|| operation("labels")),
    }
}

// ---------------------------------------------------------------------------
// `/{dataset}/v/{version}/manifest` — the bundle manifest
// ---------------------------------------------------------------------------

/// A bundle's published manifest.
#[derive(Debug)]
pub struct BundleManifest {
    dataset: String,
    version: String,
    published: bytes::Bytes,
    parsed: std::sync::Arc<Manifest>,
}

impl BundleManifest {
    /// Pair the bytes as published with the parse the page is rendered from.
    pub fn new(
        dataset: &str,
        version: &str,
        published: bytes::Bytes,
        parsed: std::sync::Arc<Manifest>,
    ) -> Self {
        Self {
            dataset: dataset.to_owned(),
            version: version.to_owned(),
            published,
            parsed,
        }
    }
}

impl Resource for BundleManifest {
    /// The bytes as published, so nothing this build does not model is lost and
    /// the response is the exact document its publication ETag covers.
    ///
    /// Handed on by refcount: the file was read once at startup and is not
    /// copied again per request.
    fn to_json(&self) -> bytes::Bytes {
        self.published.clone()
    }

    fn to_html(&self) -> String {
        let manifest = &self.parsed;
        let capabilities: Vec<_> = manifest.capabilities.keys().collect();
        let prefixes: Vec<_> = manifest
            .prefixes
            .iter()
            .map(|(prefix, expansion)| {
                vec![
                    Value::Code(prefix.as_str()),
                    Value::Code(expansion.as_str()),
                ]
            })
            .collect();
        let role_members: Vec<String> = manifest
            .predicate_roles
            .values()
            .map(|predicates| predicates.join(", "))
            .collect();
        let predicate_roles: Vec<_> = manifest
            .predicate_roles
            .keys()
            .zip(&role_members)
            .map(|(role, predicates)| vec![Value::Code(role), Value::Code(predicates)])
            .collect();
        let artifacts: Vec<_> = manifest
            .artifacts
            .iter()
            .map(|(name, entry)| {
                vec![
                    Value::Code(name.as_str()),
                    Value::Number(entry.bytes),
                    Value::Code(&entry.sha256),
                ]
            })
            .collect();

        page(
            &format!("{} — {}", self.dataset, self.version),
            &[
                Crumb::to(&self.dataset, url::dataset(&self.dataset)),
                Crumb::here(&self.version),
            ],
            Some(&url::operation(&self.dataset, &self.version, "manifest")),
            html! {
                section."overview" {
                    @if let Some(description) = &manifest.description {
                        p."lede" { (description) }
                    }
                    (stats(&[
                        ("triples", group_digits(manifest.counts.triples)),
                        ("subjects", group_digits(manifest.counts.subjects)),
                        ("predicates", group_digits(manifest.counts.predicates)),
                        ("objects", group_digits(manifest.counts.objects)),
                    ]))
                    (note(
                        "Subjects and objects are id-space sizes: each counts the shared section \
                         once, so they overlap and do not sum to a distinct-term total."
                    ))
                    @if !capabilities.is_empty() {
                        (chips(&capabilities))
                    }
                }

                section."workbench" {
                    (forms::manifest_forms(&self.dataset, &self.version, manifest))
                }

                div."dashboard-grid" {
                    section."panel" {
                        h2 { "Operations" }
                        (note(
                            "Read operations over this immutable version. Each URL negotiates its \
                             listed machine representation and HTML according to Accept, with $KGF \
                             as this server's base URL."
                        ))
                        (table(
                            &["Operation", "Parameters"],
                            &operations(&self.dataset, &self.version, manifest),
                        ))
                        pre {
                            code {
                                "curl \"$KGF" (url::operation(&self.dataset, &self.version, "fragment"))
                                "?limit=25\"\n"
                                "curl \"$KGF" (url::operation(&self.dataset, &self.version, "count"))
                                "?p=rdf:type\"\n"
                                "curl \"$KGF" (url::operation(&self.dataset, &self.version, "describe"))
                                "?iri=<https://example.org/resource>\""
                            }
                        }
                    }
                    section."panel" {
                        h2 { "Identity" }
                        (fields(&[
                            ("id", Value::Code(&manifest.id)),
                            ("version", Value::Code(&manifest.version)),
                            ("content_digest", Value::Code(&manifest.content_digest)),
                            (
                                "dataset_iri",
                                manifest.dataset_iri.as_deref().map_or(Value::Absent, Value::Code),
                            ),
                            (
                                "created",
                                manifest.created.as_deref().map_or(Value::Absent, Value::Code),
                            ),
                            (
                                "license",
                                manifest.license.as_deref().map_or(Value::Absent, Value::Text),
                            ),
                            (
                                "publisher",
                                manifest
                                    .publisher
                                    .as_ref()
                                    .map_or(Value::Absent, |publisher| Value::Text(&publisher.name)),
                            ),
                            (
                                "previous_version",
                                manifest.previous_version.as_deref().map_or(Value::Absent, |previous| {
                                    Value::self_link(
                                        url::operation(&self.dataset, previous, "manifest"),
                                        previous,
                                    )
                                }),
                            ),
                        ]))
                    }
                }

                div."dashboard-grid" {
                    section."panel" {
                        h2 { "Prefixes" }
                        (note(
                            "CURIE prefixes accepted by this bundle's parameters. Bracket full IRIs; \
                             a bare token must use one of these prefixes."
                        ))
                        @if prefixes.is_empty() {
                            (note("None declared."))
                        } @else {
                            (table(&["Prefix", "Expands to"], &prefixes))
                        }
                    }
                    section."panel" {
                        h2 { "Predicate roles" }
                        (note(
                            "The immutable semantic profile used by role-scoped search and preferred \
                             label resolution for this version."
                        ))
                        @if predicate_roles.is_empty() {
                            (note("The federation label defaults apply."))
                        } @else {
                            (table(&["Role", "Predicates (strongest first)"], &predicate_roles))
                        }
                    }
                }

                section."section-block" {
                    h2 { "Artifacts" }
                    (table(&["Artifact", "Bytes", "SHA-256"], &artifacts))
                }
            },
        )
    }
}

/// The operations a browser can reach from a manifest page.
///
/// Linked where the operation answers something without arguments, which is
/// what makes the page a way *into* the data: `/fragment` with no pattern is
/// the first page of everything, and every term in it links onwards. `/describe`
/// needs a resource, so it is listed with the parameter it wants rather than
/// with a link that would 400.
fn operations(dataset: &str, version: &str, manifest: &Manifest) -> Vec<Vec<Value<'static>>> {
    let search = manifest.declares(Capability::Search);
    let pattern_parameters = if search {
        "s, p, o, o.text, limit, cursor"
    } else {
        "s, p, o, limit, cursor"
    };
    let count_parameters = if search {
        "s, p, o, o.text, cursor"
    } else {
        "s, p, o"
    };
    let mut operations = vec![
        ("fragment", pattern_parameters, true),
        ("count", count_parameters, true),
        ("describe", "iri, direction, limit, cursor", false),
    ];
    if manifest.carries_description_artifacts() {
        operations.extend([
            (
                "schema",
                "class, predicate, datatype, children, projection, view, limit, cursor",
                true,
            ),
            ("void", "format=ttl|jsonld|html", true),
            ("summary", "format=md|json|html", true),
        ]);
    }
    if manifest.declares(Capability::Sample) {
        operations.push(("sample", "s, p, o, n, seed", true));
    }
    if search {
        operations.push(("search", "q, role, predicate, labels, limit", false));
    }
    if manifest.declares(Capability::Labels) {
        operations.push(("labels", "QUERY/POST JSON body: iris", false));
    }
    operations
        .into_iter()
        .map(|(operation, parameters, browsable)| {
            vec![
                if browsable {
                    Value::Link {
                        href: url::operation(dataset, version, operation),
                        label: operation,
                    }
                } else {
                    Value::Code(operation)
                },
                Value::Code(parameters),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgf_store::manifest::{ArtifactEntry, Counts, Formats};
    use std::collections::BTreeMap;

    fn manifest() -> Manifest {
        Manifest {
            id: "tox".to_owned(),
            dataset_iri: Some("https://okn.example/id/tox".to_owned()),
            version: "2026-06-01".to_owned(),
            content_digest: "sha256:0123456789abcdef0123456789abcdef".to_owned(),
            created: Some("2026-06-01T14:03:22Z".to_owned()),
            formats: Formats::default(),
            title: Some("Tox".to_owned()),
            description: Some("A test bundle".to_owned()),
            license: None,
            homepage: None,
            publisher: Some(Publisher {
                name: "OKN".to_owned(),
                contact: None,
            }),
            counts: Counts {
                triples: 606_342_307,
                subjects: 3,
                predicates: 4,
                objects: 6,
            },
            capabilities: BTreeMap::from([("sample".to_owned(), serde_json::json!({}))]),
            prefixes: BTreeMap::from([(
                "rdfs".to_owned(),
                "http://www.w3.org/2000/01/rdf-schema#".to_owned(),
            )]),
            predicate_roles: BTreeMap::new(),
            artifacts: BTreeMap::from([(
                "data.hdt".to_owned(),
                ArtifactEntry::checksum(912, "abc123"),
            )]),
            previous_version: Some("2026-03-01".to_owned()),
            source: None,
        }
    }

    fn bundle_manifest(published: &str) -> BundleManifest {
        BundleManifest::new(
            "tox",
            "2026-06-01",
            bytes::Bytes::copy_from_slice(published.as_bytes()),
            std::sync::Arc::new(manifest()),
        )
    }

    #[test]
    fn the_manifest_is_served_exactly_as_published() {
        // A newer builder's fields must survive being served, and the bytes
        // must stay the ones the content digest was taken over. Re-serializing
        // this build's `Manifest` would drop the unmodeled key below.
        let published = "{\n  \"id\": \"tox\",\n  \"source\": {\"url\": \"https://x\"}\n}\n";
        let resource = bundle_manifest(published);
        assert_eq!(resource.to_json(), published.as_bytes());
        assert!(
            String::from_utf8(resource.to_json().to_vec())
                .unwrap()
                .contains("source")
        );
    }

    #[test]
    fn the_manifest_page_shows_what_a_client_needs_to_query_the_bundle() {
        let page = bundle_manifest("{}").to_html();
        // The human representation is an application workbench, with the
        // release summary and query surface distinct from its metadata.
        assert!(page.contains("class=\"overview\""));
        assert!(page.contains("class=\"workbench\""));
        assert!(page.contains("class=\"dashboard-grid\""));
        // Identity, so a cursor or a mirror check can be reasoned about.
        assert!(page.contains("sha256:0123456789abcdef0123456789abcdef"));
        // Counts, grouped for reading.
        assert!(page.contains("606\u{202f}342\u{202f}307"));
        // Capabilities and prefixes: the two manifest properties on which a
        // request's validity depends.
        assert!(page.contains("sample"));
        assert!(page.contains("http://www.w3.org/2000/01/rdf-schema#"));
        // Navigation up to the dataset, and across to the previous release.
        assert!(page.contains("href=\"/tox\""));
        assert!(page.contains("href=\"/tox/v/2026-03-01/manifest\""));
        // And back out to the machine-readable form.
        assert!(page.contains("href=\"/tox/v/2026-06-01/manifest?format=json\""));
        // And onward into the data: a browser needs one link that works with
        // no arguments, or the API is only reachable by typing URLs.
        assert!(page.contains("href=\"/tox/v/2026-06-01/fragment\""));
        assert!(page.contains("href=\"/tox/v/2026-06-01/count\""));
        // `/describe` is named without a link, because it needs a resource and
        // a link that 400s is worse than none.
        assert!(page.contains("describe"));
        assert!(!page.contains("href=\"/tox/v/2026-06-01/describe\""));
    }

    #[test]
    fn a_description_manifest_links_its_three_description_operations() {
        let mut manifest = manifest();
        for name in kgf_store::store::artifact::DESCRIPTION {
            manifest.artifacts.insert(
                name.to_owned(),
                ArtifactEntry::checksum(1, format!("sha256-{name}")),
            );
        }
        let page = BundleManifest::new(
            "tox",
            "2026-06-01",
            bytes::Bytes::from_static(b"{}"),
            std::sync::Arc::new(manifest),
        )
        .to_html();

        for operation in ["schema", "void", "summary"] {
            assert!(
                page.contains(&format!("href=\"/tox/v/2026-06-01/{operation}\"")),
                "the manifest page must link /{operation}"
            );
        }
    }
}
