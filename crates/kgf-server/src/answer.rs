//! What the server sends back: read operations, executed and rendered.
//!
//! Each is thin, because units 10–13 did the work: parse terms to ids, resolve
//! a [`Selection`], page it, serialize. What is left here is the part that
//! genuinely belongs to the operations — how a page decides it is complete,
//! where a cursor's position is bounded, and how `/describe` walks two
//! enumerations behind one envelope.
//!
//! # Strings are materialized while serializing, and nowhere else
//!
//! Keeping strings at the serialization edge has a consequence for the blocking boundary: the whole
//! of an operation, *including writing the response body*, runs inside the task
//! that holds the [`Store`]. [`Answer`] holds `Rc<str>` handed out by the
//! request's [`TermCache`], so it is deliberately not `Send`; what crosses back
//! is [`Rendered`] — bytes and the completeness metadata the headers repeat.
//!
//! Doing it the other way — returning rows of owned `String`s and serializing
//! on the reactor — would allocate a string per term per row for no reason
//! other than to move them.
//!
//! # One page, one extra row
//!
//! Every paged operation asks its enumeration for `limit + 1` rows and keeps
//! `limit`. That is how a response knows whether it is complete without a
//! second query and without arithmetic that differs per pattern: for `s ? o`
//! the position is a predicate id rather than an offset, so `offset + returned
//! < count` is not available, and the extra row is. It costs one triple
//! materialization per page.
//!
//! # Cursor positions are bounded here
//!
//! [`crate::cursor`] cannot check that a position is inside a result set — it
//! has no store. This is where that check lands, and it is two rules rather
//! than one, because the position means different things
//! ([`PositionSpace`]): for the three permutation spaces it is a result offset,
//! bounded by the cardinality, while for [`PositionSpace::Predicate`] it is the
//! last predicate id returned, bounded by the predicate id space. Checking the
//! second against a cardinality would reject a live cursor — a one-row `s ? o`
//! answer legitimately resumes at predicate 37.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::rc::Rc;

use bytes::Bytes;
use maud::html;
use oxrdf::{
    BlankNode, GraphName, Literal, NamedNode, NamedOrBlankNode, Quad, Term as RdfTerm, Triple,
};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

use hdtc::format::{TextScanPosition, TextSearcher, parse_literal};
use kgf_store::catalog::BundleId;
use kgf_store::dict::{DictPosition, Dictionary, RoleCounts, ScanFlow, ScannedTerm};
use kgf_store::pattern::{IdPattern, Selection};
use kgf_store::{
    ClassPropertyStop, ClassRelationStop, IdTriple, Role, SchemaCollection,
    SchemaCounts as StoreSchemaCounts, SchemaNode as StoreSchemaNode, SchemaNodeKind, StatsView,
    Store, TermId,
};

use crate::access::AccessOperation;
use crate::cursor::{Cursor, CursorBinding, CursorToken, PositionSpace, StaleCursor};
use crate::envelope::{
    BudgetReason, Cardinality, Completeness, ErrorCode, Problem, TruncationReason,
};
use crate::forms;
use crate::html::{
    Crumb, Resource, TermText, Value, fields, group_digits, json_body, note, operation_page,
    operation_page_with_format, page, pager, results_table, stats, table,
};
use crate::rdf::{GraphFormat, serialize_dataset, serialize_graph};
use crate::representation::{RdfSyntax, Representation};
use crate::request::{
    self, BindingPattern, BindingRow, BoundTerm, Candidates, Direction, Pattern, Position,
    ResponseBytes, SchemaChildren, SchemaQuery, SchemaSelection, TextFilter, role_name,
    term_role_name,
};
use crate::request::{GraphScope, GraphSelector};
use crate::skolem::SkolemScope;
use crate::term::{DictionaryTermError, LiteralKind, PrefixMap, Term, TermCache, serialized_bytes};
use crate::url::{self, Mount, Params};
use kgf_store::graphs::{GraphId, Graphs};
use kgf_store::scope::{QuadSelection, ScopedSelection};

// ---------------------------------------------------------------------------
// Where a response came from
// ---------------------------------------------------------------------------

/// The bundle version and request an answer belongs to, and the links it can
/// build from them.
///
/// Carried by the answer rather than by the handler because the links are part
/// of the *rendering*: a page's next-page link and its term links are built
/// while the rows are, inside the blocking task.
#[derive(Debug, Clone)]
pub struct Target {
    id: BundleId,
    operation: AccessOperation,
    params: Params,
    prefixes: PrefixMap,
    /// Where the deployment is mounted. Every link this answer renders — the
    /// canonical URL, the next page, the crumbs, the term links — is built
    /// against it, from the same place the prefix map is read.
    mount: Mount,
    body: bool,
    offers: Offers,
    /// Logical dataset identity and the description link this release can
    /// actually answer, from the immutable manifest.
    dataset: Option<DatasetMetadata>,
    /// The exact absolute GET URL received over HTTP. Hydra metadata keys its
    /// page controls by this IRI, so a merely equivalent canonical URL is not
    /// enough for an LDF client looking up controls for its request URL.
    request_url: Option<String>,
}

#[derive(Debug, Clone)]
struct DatasetMetadata {
    iri: String,
    void_available: bool,
}

/// The optional capabilities a release declares that a page's controls are
/// shaped by: whether to offer a text constraint, and whether to offer a
/// graph scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Offers {
    /// The release declares `search`, so `o.text` is a control.
    pub search: bool,
    /// The release declares `graphs`, so `g` is a control.
    pub graphs: bool,
}

impl Target {
    /// The version and operation a request addressed, with its parameters, the
    /// version's immutable prefix map for human-facing result labels, and the
    /// mount its links are built against.
    pub fn new(
        id: BundleId,
        operation: AccessOperation,
        params: Params,
        prefixes: PrefixMap,
        mount: Mount,
    ) -> Self {
        Self::get(
            id,
            operation,
            params,
            prefixes,
            mount,
            Offers::default(),
            None,
        )
    }

    /// A GET target with the release capabilities its page may expose.
    pub(crate) fn get(
        id: BundleId,
        operation: AccessOperation,
        params: Params,
        prefixes: PrefixMap,
        mount: Mount,
        offers: Offers,
        request_url: Option<String>,
    ) -> Self {
        Self {
            id,
            operation,
            params,
            prefixes,
            mount,
            body: false,
            offers,
            request_url,
            dataset: None,
        }
    }

    /// Attach the release's logical dataset identity and whether its VoID
    /// description is a real published capability.
    pub(crate) fn with_dataset_metadata(
        mut self,
        dataset_iri: Option<&str>,
        void_available: bool,
    ) -> Self {
        self.dataset = dataset_iri.map(|iri| DatasetMetadata {
            iri: iri.to_owned(),
            void_available,
        });
        self
    }

    /// A body-addressed operation, whose request cannot be reconstructed as a link.
    pub fn body(
        id: BundleId,
        operation: AccessOperation,
        params: Params,
        prefixes: PrefixMap,
        mount: Mount,
    ) -> Self {
        Self {
            id,
            operation,
            params,
            prefixes,
            mount,
            body: true,
            offers: Offers::default(),
            request_url: None,
            dataset: None,
        }
    }

    /// The bundle version to open.
    pub fn id(&self) -> &BundleId {
        &self.id
    }

    /// The mount this answer's links are built against.
    pub fn mount(&self) -> &Mount {
        &self.mount
    }

    /// This operation's own path, prefix included. Composed with
    /// [`origin`](Self::origin) for the absolute form: the origin is
    /// `scheme://authority` alone, so the prefix appears exactly once.
    fn base(&self) -> String {
        self.mount.operation(
            &self.id.dataset,
            &self.id.version,
            self.operation.path_segment(),
        )
    }

    /// The origin on which the request arrived.
    fn origin(&self) -> Option<String> {
        let uri = self
            .request_url
            .as_deref()?
            .parse::<axum::http::Uri>()
            .ok()?;
        Some(format!(
            "{}://{}",
            uri.scheme_str()?,
            uri.authority()?.as_str()
        ))
    }

    fn absolute_base(&self) -> Option<String> {
        Some(format!("{}{}", self.origin()?, self.base()))
    }

    fn absolute_next(&self, token: &str) -> Option<String> {
        Some(format!("{}{}", self.origin()?, self.next(token)?))
    }

    fn tpf_page_url(&self) -> Option<String> {
        self.request_url.as_deref().map(url::encode_rdf_iri)
    }

    /// The canonical TPF fragment identity: the request parameters without
    /// controls that select a representation, page size, or resume position.
    /// Page one and every continuation therefore name the same fragment even
    /// when the client ordered or escaped its original parameters differently.
    fn tpf_fragment_url(&self) -> Option<String> {
        let params = self
            .params
            .without("cursor")
            .without("limit")
            .without("format");
        Some(format!("{}{}", self.origin()?, query(self.base(), &params)))
    }

    fn absolute_void(&self) -> Option<String> {
        Some(format!(
            "{}{}",
            self.origin()?,
            self.mount
                .operation(&self.id.dataset, &self.id.version, "void")
        ))
    }

    fn is_tpf(&self) -> bool {
        self.operation == AccessOperation::Tpf
    }

    /// This response's URL, with the representation selector removed.
    ///
    /// The page appends its machine representation selector to build the
    /// footer link, and a URL that already carried `format=html` would come
    /// back with the parameter twice — which this server's own parser refuses.
    /// Dropping it is also the more honest reading of "canonical": one
    /// resource, several representations.
    fn canonical(&self) -> Option<String> {
        (!self.body).then(|| query(self.base(), &self.params.without("format")))
    }

    /// The same request, resumed at `token`.
    fn next(&self, token: &str) -> Option<String> {
        (!self.body).then(|| query(self.base(), &self.params.with("cursor", token)))
    }

    /// A one-parameter request against another operation of the same bundle,
    /// for a link on a page.
    fn ask(&self, operation: &str, parameter: &str, value: &str) -> String {
        format!(
            "{}?{parameter}={}",
            self.mount
                .operation(&self.id.dataset, &self.id.version, operation),
            url::encode_value(value)
        )
    }

    fn crumbs(&self) -> Vec<Crumb<'_>> {
        vec![
            Crumb::to(&self.id.dataset, self.mount.dataset(&self.id.dataset)),
            // There is no landing page for a version, so the version step goes
            // to its manifest, the version's canonical descriptive document.
            Crumb::to(
                &self.id.version,
                self.mount
                    .operation(&self.id.dataset, &self.id.version, "manifest"),
            ),
            Crumb::here(self.operation.path_segment()),
        ]
    }

    fn title(&self) -> String {
        format!(
            "{} — {} {}",
            self.operation.path_segment(),
            self.id.dataset,
            self.id.version
        )
    }

    /// Compact operation and release context shown under a page's actual
    /// focus. The focus is the term, pattern, or search text; this line keeps
    /// the route name and version available without letting them become the
    /// largest thing on the page.
    fn context(&self) -> String {
        format!(
            "{} · {} {}",
            self.operation_label(),
            self.id.dataset,
            self.id.version
        )
    }

    fn operation_label(&self) -> &'static str {
        match self.operation {
            AccessOperation::Fragment => "Fragment",
            AccessOperation::Tpf => "Triple Pattern Fragment",
            AccessOperation::Count => "Count",
            AccessOperation::Describe => "Describe",
            AccessOperation::Sample => "Sample",
            AccessOperation::Search => "Search",
            AccessOperation::Terms => "Terms",
            AccessOperation::Graphs => "Graphs",
            AccessOperation::Schema => "Schema",
            AccessOperation::Labels => "Labels",
            AccessOperation::Void => "void",
            AccessOperation::Summary => "summary",
            AccessOperation::Manifest => "manifest",
            AccessOperation::Service => "service",
            AccessOperation::Dataset => "dataset",
            AccessOperation::Latest => "latest",
        }
    }

    /// The GET editor for this answer, absent for a body-addressed request.
    fn form(&self) -> Option<maud::Markup> {
        if self.body {
            None
        } else {
            forms::operation_form(
                &self.mount,
                &self.id.dataset,
                &self.id.version,
                self.operation.path_segment(),
                &self.params,
                self.offers,
            )
        }
    }
}

fn query(base: String, params: &Params) -> String {
    if params.is_empty() {
        base
    } else {
        format!("{base}?{}", params.to_query())
    }
}

/// A serialized response and the metadata repeated on its headers.
///
/// The pair is the reason this type exists: the body is produced inside the
/// blocking task, and the headers are set outside it, so the completeness has
/// to travel with the bytes rather than being read back off them.
#[derive(Debug)]
pub struct Rendered {
    /// The response body.
    pub body: Bytes,
    /// `KGF-Complete` and friends.
    pub completeness: Completeness,
    /// Result items materialized into this response.
    pub rows: Option<u64>,
    /// Result cardinality when the operation reports one.
    pub cardinality: Option<Cardinality>,
}

/// An answer that can be serialized into either representation.
///
/// One trait rather than two inherent methods so that [`crate::routes`] can
/// have a single shape for all operations — and so that adding a
/// serialization is a change the compiler routes through every answer, the same
/// reason [`Resource`] exists.
pub trait Renders {
    /// Serialize into `representation`, with the metadata its headers need.
    fn render(self, representation: Representation) -> Result<Rendered, Problem>;

    /// Resolve display labels for the page's IRIs, before an HTML render.
    ///
    /// A no-op for answers that carry no IRI rows and for JSON, whose clients
    /// hydrate labels themselves through `/labels`. `label_predicates` is the
    /// release's frozen `label` role cascade, and `cap` bounds the distinct
    /// terms one page may resolve — the same `max_label_iris` that bounds a
    /// `/labels` request, so a page never does work a client could not ask
    /// for. A page over the cap is served unannotated rather than
    /// half-annotated.
    fn hydrate_labels(
        &mut self,
        _store: &Store,
        _label_predicates: &[String],
        _cap: usize,
        _required: bool,
    ) -> Result<(), Problem> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// The key `/describe` reports an edge's side under.
const DIRECTION: &str = "direction";

/// The key under which a text-ranked row reports its relevance.
const SCORE: &str = "score";

/// The key under which a text-ranked row reports *how* it matched.
const MATCH_KIND: &str = "match_kind";

/// The input-row index carried by a bindings result.
const BINDING: &str = "binding";

/// How a text hit matched, in the public response vocabulary.
///
/// Emitted beside `score` because without it the score is misleading. hdtc
/// ranks exact matches as a class ahead of stemmed ones and its BM25 figures
/// are comparable only *within* a class, so a stemmed row can carry a higher
/// number than the exact row above it — and a client that sorts a page by
/// `score` while merging endpoints would
/// undo the ranking the server computed. With the class present, "by class,
/// then rank" reproduces the order this server sent.
///
/// The index and response vocabularies do not line up, so this reports the
/// public one clients branch on: `exact | normalized | prefix |
/// fuzzy`, while hdtc classifies a hit as exact or stemmed and treats prefix
/// and fuzzy as query *modes* rather than per-hit outcomes. Stemming is a
/// normalization, so `normalized` is the honest member of the published set,
/// though it is wider than what is being said.
fn match_kind(kind: hdtc::format::MatchKind) -> &'static str {
    match kind {
        hdtc::format::MatchKind::Exact => "exact",
        hdtc::format::MatchKind::Stemmed => "normalized",
    }
}

/// A bound parameter that matched nothing, and why.
///
/// Two very different situations produce the same empty answer, and only the
/// server can tell them apart. "This bundle does not hold that term" is a fact
/// about the data, remedied by asking a different bundle. "Blank-node syntax
/// does not address anything here" is a fact about the API, remedied by sending
/// the scoped IRI the response would have carried — and it is the one a client
/// is most likely to hit by copying a term out of a browser page, where a blank
/// node is shown as `_:{section}-{local-id}` for legibility.
#[derive(Debug, Clone, Copy, Serialize)]
struct AbsentTerm {
    parameter: &'static str,
    reason: AbsentReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AbsentReason {
    /// A well-formed term whose spelling this bundle's dictionary does not hold.
    NotInBundle,
    /// Blank-node syntax, which addresses no term in any bundle by design.
    BlankNode,
}

impl AbsentTerm {
    fn new(parameter: &'static str, term: &BoundTerm) -> Self {
        Self {
            parameter,
            reason: if term.denotes_blank_node() {
                AbsentReason::BlankNode
            } else {
                AbsentReason::NotInBundle
            },
        }
    }

    /// The one sentence a browser page says about it.
    fn explanation(&self) -> String {
        match self.reason {
            AbsentReason::NotInBundle => {
                format!(
                    "`{}` names a term this release does not hold",
                    self.parameter
                )
            }
            AbsentReason::BlankNode => format!(
                "`{}` is written as a blank node, which addresses nothing here — \
                 use the scoped IRI this API publishes for it",
                self.parameter
            ),
        }
    }
}

/// One term of a row, in both spellings the server needs.
///
/// They differ for exactly one term shape. A stored blank node is published as
/// this bundle's scoped IRI, because a `_:` label means nothing outside the
/// document it was parsed from — but label lookup still has to find the term in
/// the dictionary, which knows it only by that label. Holding both is what lets
/// the response name a node the way the API does while the page still labels it.
#[derive(Debug, Clone)]
struct RowTerm {
    /// What a response carries, and what its byte accounting weighs.
    published: Rc<str>,
    /// The dictionary spelling, which `locate` matches and labels key on.
    stored: Rc<str>,
}

/// Terms as this API publishes them, memoized for one request.
///
/// [`TermCache`] materializes and measures a term as the dictionary spells it.
/// This adds the one substitution the wire makes, and memoizes that separately
/// so a blank node repeated down a page is formatted and weighed once rather
/// than once per row — the same reason the cache underneath it exists.
struct PublishedTerms {
    blank_nodes: SkolemScope,
    published: HashMap<(Role, u64), (Rc<str>, u64)>,
}

impl PublishedTerms {
    fn new(blank_nodes: SkolemScope) -> Self {
        Self {
            blank_nodes,
            published: HashMap::new(),
        }
    }

    /// The term's two spellings, and the bytes its published term object takes.
    fn measured(
        &mut self,
        cache: &mut TermCache,
        dictionary: &Dictionary<'_>,
        role: Role,
        id: TermId,
    ) -> Result<(RowTerm, u64), DictionaryTermError> {
        let (stored, serialized) = cache.measured(dictionary, role, id)?;
        if let Some((published, serialized)) = self.published.get(&(role, id.0)) {
            return Ok((
                RowTerm {
                    published: Rc::clone(published),
                    stored,
                },
                *serialized,
            ));
        }
        let Some(iri) = self.blank_nodes.iri(role, id, &stored) else {
            return Ok((
                RowTerm {
                    published: Rc::clone(&stored),
                    stored,
                },
                serialized,
            ));
        };
        let published: Rc<str> = Rc::from(iri.as_str());
        let serialized = serialized_bytes(&Term::Iri(Cow::Borrowed(published.as_ref())));
        self.published
            .insert((role, id.0), (Rc::clone(&published), serialized));
        Ok((RowTerm { published, stored }, serialized))
    }
}

/// One result row: a term per variable, and for `/describe` which side of the
/// neighborhood it came from.
#[derive(Debug, Clone)]
pub struct Row {
    cells: Vec<(Position, RowTerm)>,
    /// The graph this membership belongs to, in the quad view.
    graph: Option<Rc<str>>,
    binding: Option<u32>,
    direction: Option<Direction>,
    ranking: Option<Ranking>,
    serialized: u64,
}

/// The key under which a quad-view row reports its graph.
const GRAPH: &str = "g";

/// What a text-ranked row says about how it matched.
///
/// The two travel together because neither is usable alone: a score without its
/// class cannot be compared with the score above it, and a class without a
/// score cannot order within itself.
#[derive(Debug, Clone, Copy)]
pub struct Ranking {
    score: f32,
    kind: &'static str,
}

impl Serialize for Row {
    /// One key per variable, each a term object.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        if let Some(binding) = self.binding {
            map.serialize_entry(BINDING, &binding)?;
        }
        for (position, term) in &self.cells {
            map.serialize_entry(position.as_str(), &Term::from_dictionary(&term.published))?;
        }
        if let Some(graph) = &self.graph {
            map.serialize_entry(GRAPH, &Term::from_dictionary(graph))?;
        }
        if let Some(direction) = self.direction {
            map.serialize_entry(DIRECTION, &direction)?;
        }
        if let Some(ranking) = self.ranking {
            map.serialize_entry(SCORE, &ranking.score)?;
            map.serialize_entry(MATCH_KIND, ranking.kind)?;
        }
        map.end()
    }
}

impl Row {
    /// Assemble a row, and count what it will weigh.
    ///
    /// `terms` is the sum of the cells' own term-object lengths, which
    /// [`TermCache`] measured once per distinct term. What is added here is the
    /// map's punctuation, which is fixed: `serde_json` writes a map as
    /// `{"k":v,"k":v}` with no spaces, so a key costs its length plus the two
    /// quotes and the colon, and the entries are separated by one comma each.
    ///
    /// Counting rather than serializing is the point. The byte budget has to be
    /// weighed once per row, and a page has far more rows than distinct terms —
    /// serializing each row to size it cost a third of the time the response
    /// itself takes to render. The risk is drifting from the `Serialize` impl
    /// directly above, which is why the two sit together and why
    /// `a_row_weighs_exactly_what_it_serializes` compares them for every shape.
    fn new(
        cells: Vec<(Position, RowTerm)>,
        terms: u64,
        graph: Option<(Rc<str>, u64)>,
        binding: Option<u32>,
        direction: Option<Direction>,
        ranking: Option<Ranking>,
    ) -> Self {
        let mut entries = cells.len() as u64;
        let mut serialized = 2 + terms;
        if let Some(binding) = binding {
            entries += 1;
            serialized += quoted_key(BINDING) + binding.to_string().len() as u64;
        }
        for (position, _) in &cells {
            serialized += quoted_key(position.as_str());
        }
        let graph = graph.map(|(graph, measured)| {
            entries += 1;
            serialized += quoted_key(GRAPH) + measured;
            graph
        });
        if let Some(direction) = direction {
            entries += 1;
            serialized += quoted_key(DIRECTION) + direction.as_str().len() as u64 + 2;
        }
        if let Some(ranking) = ranking {
            entries += 2;
            // The score is the one field that is formatted to be measured. A
            // float's shortest round-trip form has no length this can compute,
            // and guessing high would let the budget refuse a page that fits.
            // It is one small number per row against three term objects, which
            // is the cost the rest of this arrangement exists to avoid.
            serialized += quoted_key(SCORE) + serialized_score(ranking.score);
            serialized += quoted_key(MATCH_KIND) + ranking.kind.len() as u64 + 2;
        }
        Self {
            cells,
            graph,
            binding,
            direction,
            ranking,
            serialized: serialized + entries.saturating_sub(1),
        }
    }
}

/// `"key":` — the key, its quotes, and the colon.
fn quoted_key(key: &str) -> u64 {
    key.len() as u64 + 3
}

/// Compact JSON object size for fixed, ASCII keys.
fn serialized_object<const N: usize>(entries: [(&str, u64); N]) -> u64 {
    2 + entries
        .into_iter()
        .map(|(key, value)| quoted_key(key) + value)
        .sum::<u64>()
        + N.saturating_sub(1) as u64
}

/// Exact compact-JSON size of one string, including its quotes.
fn serialized_json_string(value: &str) -> u64 {
    2 + value
        .chars()
        .map(|character| match character {
            '"' | '\\' | '\u{0008}' | '\t' | '\n' | '\u{000c}' | '\r' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => character.len_utf8() as u64,
        })
        .sum::<u64>()
}

/// How many bytes `serde_json` writes for this score.
///
/// Formatted rather than computed because a float's shortest round-trip form
/// has no length that can be derived from the value. A non-finite score is not
/// a special case: JSON has no such literal, so `serde_json` writes `null` on
/// both sides of this — here and in the row — and the two agree without either
/// having to know it.
fn serialized_score(score: f32) -> u64 {
    serde_json::to_string(&score)
        .map(|text| text.len() as u64)
        // Unreachable: every `f32` serializes, finite or not. Weighed as the
        // longest plausible rendering rather than panicked on, because a wrong
        // byte count is a budget that misses by a few bytes and a panic is a
        // dropped connection.
        .unwrap_or(16)
}

// ---------------------------------------------------------------------------
// The envelope
// ---------------------------------------------------------------------------

/// What the response says the request was.
///
/// An enum rather than a struct of optional fields, so that a `/count` cannot
/// acquire a seed and a `/describe` cannot acquire a pattern.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum Echo {
    Fragment {
        pattern: Pattern,
        #[serde(skip_serializing_if = "Option::is_none")]
        g: Option<String>,
    },
    BindingsFragment {
        pattern: BindingPattern,
        #[serde(skip_serializing_if = "Option::is_none")]
        g: Option<String>,
    },
    Describe {
        resource: String,
        direction: Direction,
    },
    Sample {
        pattern: Pattern,
        n: u32,
        seed: u64,
    },
}

/// The keys a page's rows carry: the unbound positions, and `g` in the quad
/// view.
///
/// `g` is a column rather than a position because it is not a term of the
/// triple: a row's graph comes from the membership sidecar, and the same
/// triple recurs once per graph it is in.
#[derive(Debug, Clone)]
pub struct Vars {
    positions: Vec<Position>,
    graph: bool,
}

impl Vars {
    fn new(positions: Vec<Position>, graph: bool) -> Self {
        Self { positions, graph }
    }

    /// The triple positions rows report.
    fn positions(&self) -> &[Position] {
        &self.positions
    }

    /// Whether rows report `g`.
    fn has_graph(&self) -> bool {
        self.graph
    }

    /// Whether a row has nothing to report: every position bound, and no
    /// graph column.
    fn is_empty(&self) -> bool {
        self.positions.is_empty() && !self.graph
    }

    /// Every key, in row order.
    fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.positions
            .iter()
            .map(|position| position.as_str())
            .chain(self.graph.then_some(GRAPH))
    }
}

impl Serialize for Vars {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.keys())
    }
}

/// A page of rows in the envelope shared by `/fragment`,
/// `/describe` and `/sample`.
#[derive(Debug, Clone, Serialize)]
pub struct Answer {
    dataset: String,
    version: String,
    #[serde(flatten)]
    echo: Echo,
    cardinality: Cardinality,
    /// Which bound parameters name terms this bundle's dictionary does not
    /// hold.
    ///
    /// This diagnostic distinguishes an empty answer caused by an absent term
    /// from one caused by a pattern with no matches. They are the
    /// same response with very different remedies, and only the server can tell
    /// them apart, so unusual but valid IRIs are accepted at the edge and
    /// reported here if absent.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    absent_terms: Vec<AbsentTerm>,
    vars: Vars,
    rows: Vec<Row>,
    #[serde(skip)]
    row_resumes: Vec<RowResume>,
    #[serde(skip)]
    row_binding: Option<CursorBinding>,
    /// Cardinality of the distinct RDF projection. The native binding
    /// relation keeps its independently exact row count in `cardinality`.
    #[serde(skip)]
    rdf_cardinality: Option<Cardinality>,
    /// The effective row limit selected for this request, before byte fitting
    /// or distinct RDF projection shortens the serialized page.
    #[serde(skip)]
    page_limit: u32,
    #[serde(skip)]
    byte_budget: u64,
    #[serde(flatten)]
    completeness: Completeness,
    #[serde(skip)]
    directed: bool,
    #[serde(skip)]
    bindings: bool,
    #[serde(skip)]
    target: Target,
    /// Stable, reversible RDF identity for this HDT's data blank nodes.
    #[serde(skip)]
    blank_nodes: SkolemScope,
    /// Display labels for the page's IRIs, resolved only when this answer is
    /// being rendered as HTML — a reading affordance, never response data.
    ///
    /// Keyed by dictionary spelling. Empty for JSON, where a client hydrates
    /// labels itself through `/labels`; a future `labels=true` parameter would
    /// put them in this same envelope field.
    #[serde(skip)]
    page_labels: HashMap<String, String>,
    /// The described term's dictionary spelling, so the page can resolve and
    /// show its label. `None` for every operation but `/describe`.
    #[serde(skip)]
    described: Option<String>,
    /// How an RDF representation tags each statement's graph.
    #[serde(skip)]
    tagging: GraphTagging,
}

impl Renders for Answer {
    fn render(mut self, representation: Representation) -> Result<Rendered, Problem> {
        let body = if representation.rdf_syntax().is_some() {
            self.fit_fragment_rdf(representation)?
        } else {
            standard_body(&self, representation)
        };
        let rows = Some(self.rows.len() as u64);
        let cardinality = Some(self.rdf_cardinality.unwrap_or(self.cardinality));
        Ok(Rendered {
            body,
            completeness: self.completeness,
            rows,
            cardinality,
        })
    }

    /// One bounded cascade per distinct IRI or blank node on the page — the
    /// same probe sequence `/labels` runs, against the same frozen role
    /// profile, bounded by the same cap.
    fn hydrate_labels(
        &mut self,
        store: &Store,
        label_predicates: &[String],
        cap: usize,
        required: bool,
    ) -> Result<(), Problem> {
        if label_predicates.is_empty() {
            return Ok(());
        }
        let mut wanted: Vec<&str> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let named = |text: &str| !text.starts_with('"');
        if let Echo::Fragment { pattern, .. } | Echo::Sample { pattern, .. } = &self.echo {
            for position in Position::ALL {
                if let Some(bound) = pattern.bound(position) {
                    let text = bound.dictionary();
                    if named(text) && seen.insert(text) {
                        wanted.push(text);
                    }
                }
            }
        }
        for row in &self.rows {
            for (_, term) in &row.cells {
                // The dictionary spelling: a published blank-node IRI is not a
                // term this bundle holds, so it would never resolve a label.
                let text = term.stored.as_ref();
                if named(text) && seen.insert(text) {
                    wanted.push(text);
                }
            }
        }
        if let Some(described) = &self.described
            && named(described)
            && seen.insert(described)
        {
            wanted.push(described);
        }
        if wanted.is_empty() {
            return Ok(());
        }
        if wanted.len() > cap {
            if required {
                return Err(Problem::new(
                    ErrorCode::CapExceeded,
                    format!(
                        "this page has {} distinct labelable terms, over this server's max_label_iris of {cap}",
                        wanted.len()
                    ),
                ));
            }
            return Ok(());
        }

        let dictionary = store.dict();
        let predicates: Vec<u64> = label_predicates
            .iter()
            .map(|iri| {
                dictionary
                    .locate(Role::Predicate, iri.as_bytes())
                    .map(|found| found.map(|id| id.0))
                    .map_err(|error| unreadable("looking a label predicate up", &error))
            })
            .filter_map(Result::transpose)
            .collect::<Result<_, _>>()?;
        if predicates.is_empty() {
            return Ok(());
        }

        let mut cache = TermCache::new();
        let mut labels = HashMap::new();
        for text in wanted {
            // A blank node reaches this in whichever spelling its position put
            // on the page: a row cell carries the dictionary label, while a
            // bound position carries the scoped IRI the request named it by.
            // Both are the same node, so both must find the same label —
            // otherwise a term is labelled when a row happens to carry it and
            // bare when the request asked about it.
            let subject = match reverse_scoped(&dictionary, &self.blank_nodes, Role::Subject, text)?
            {
                Some(id) => id,
                None => {
                    let Some(id) = dictionary
                        .locate(Role::Subject, text.as_bytes())
                        .map_err(|error| unreadable("looking a term up", &error))?
                    else {
                        continue;
                    };
                    id.0
                }
            };
            if let Some(label) =
                preferred_label(store, &dictionary, &mut cache, subject, &predicates)?
            {
                labels.insert(text.to_owned(), label);
            }
        }
        self.page_labels = labels;
        Ok(())
    }
}

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const RDF_SUBJECT: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#subject";
const RDF_PREDICATE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#predicate";
const RDF_OBJECT: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#object";
const RDFS_SEE_ALSO: &str = "http://www.w3.org/2000/01/rdf-schema#seeAlso";
const VOID_DATASET: &str = "http://rdfs.org/ns/void#Dataset";
const VOID_SUBSET: &str = "http://rdfs.org/ns/void#subset";
const VOID_IN_DATASET: &str = "http://rdfs.org/ns/void#inDataset";
const FOAF_PRIMARY_TOPIC: &str = "http://xmlns.com/foaf/0.1/primaryTopic";
const HYDRA_SEARCH: &str = "http://www.w3.org/ns/hydra/core#search";
const HYDRA_TEMPLATE: &str = "http://www.w3.org/ns/hydra/core#template";
const HYDRA_MAPPING: &str = "http://www.w3.org/ns/hydra/core#mapping";
const HYDRA_VARIABLE: &str = "http://www.w3.org/ns/hydra/core#variable";
const HYDRA_PROPERTY: &str = "http://www.w3.org/ns/hydra/core#property";
const HYDRA_TOTAL_ITEMS: &str = "http://www.w3.org/ns/hydra/core#totalItems";
const HYDRA_ITEMS_PER_PAGE: &str = "http://www.w3.org/ns/hydra/core#itemsPerPage";
const HYDRA_NEXT: &str = "http://www.w3.org/ns/hydra/core#next";
const HYDRA_VARIABLE_REPRESENTATION: &str =
    "http://www.w3.org/ns/hydra/core#variableRepresentation";
const HYDRA_EXPLICIT_REPRESENTATION: &str =
    "http://www.w3.org/ns/hydra/core#ExplicitRepresentation";
const SD_GRAPH: &str = "http://www.w3.org/ns/sparql-service-description#graph";
const SD_DEFAULT_DATASET: &str = "http://www.w3.org/ns/sparql-service-description#defaultDataset";
const SD_DEFAULT_GRAPH: &str = "http://www.w3.org/ns/sparql-service-description#defaultGraph";

/// How an RDF representation names the graph of each data statement.
///
/// The rule a graph-unbound request follows is the one a SPARQL client's
/// `GRAPH ?g` needs: the unnamed graph's statements go untagged, in the
/// document's default graph, and every other membership is tagged with its
/// graph. A request that named a graph gets every statement tagged with that
/// name, except the union, whose statements are the document's default graph
/// whether the request named the constant or left `graph` out. The union is
/// what a client's default-graph pattern reads, and a paging client matches
/// every page after the first against the pattern's literal graph term, so
/// tagging union rows with the constant would lose them from the second page
/// on. The union constant therefore never appears as a tag in any response.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GraphTagging {
    /// Every statement in the default graph: the union, unnamed.
    Untagged,
    /// Every statement tagged with one graph name.
    Fixed(String),
    /// Each statement tagged with its row's graph, the unnamed graph excepted.
    PerRow,
}

impl GraphTagging {
    fn for_scope(scope: &GraphScope) -> Self {
        match scope.selector() {
            GraphSelector::Union => Self::Untagged,
            GraphSelector::Unnamed => Self::Fixed(kgf_store::UNNAMED_GRAPH_IRI.to_owned()),
            GraphSelector::Named(term) => Self::Fixed(term.dictionary().to_owned()),
            GraphSelector::All => Self::PerRow,
        }
    }
}

struct TpfMetadata {
    page: NamedNode,
    graph_name: NamedNode,
    quads: Vec<Quad>,
}

impl Answer {
    /// Fit a finished RDF document, Hydra metadata included, to the response
    /// budget. One indivisible row may exceed the budget so a client can
    /// always advance past it.
    ///
    /// Worst case is `Z·(1 + log limit)` serialized bytes: one complete
    /// candidate document plus bounded complete-prefix probes. In practice the
    /// probes never run at the default budgets, because [`materialize`] has
    /// already trimmed the page to `max_response_bytes` measured as compact
    /// JSON and every RDF serialization of those rows is smaller than that
    /// measure — so the first complete document fits and returns. The search
    /// below is the guard for the case where it does not: an operator holding
    /// `max_response_bytes` low enough that it, rather than `limit`, is what
    /// ends a page. Admission classes this operation by its page, not by this
    /// worst case; see `WorkClass`.
    fn fit_fragment_rdf(&mut self, representation: Representation) -> Result<Bytes, Problem> {
        let metadata = self
            .target
            .is_tpf()
            .then(|| self.tpf_metadata())
            .transpose()?;
        let body = self.fragment_rdf(representation, metadata.as_ref())?;
        if body.len() as u64 <= self.byte_budget || self.rows.len() <= 1 {
            return Ok(body);
        }

        let total = self.rows.len();
        let encode_prefix = |keep: usize| {
            let next = self.rdf_row_cursor(keep)?;
            self.fragment_rdf_prefix(representation, keep, Some(next.as_str()), metadata.as_ref())
        };

        // Grow from one row until the first complete document that does not
        // fit, then binary-search only inside that bracket. Unlike starting at
        // half of a max-sized page, every probe is proportional to the output
        // prefix the byte budget can plausibly admit.
        let mut best_keep = 1usize;
        let mut best_body = encode_prefix(best_keep)?;
        if best_body.len() as u64 > self.byte_budget {
            self.truncate_rdf_rows(best_keep)?;
            return Ok(best_body);
        }

        let mut first_over = total;
        let mut probe = 2usize;
        while probe < total {
            let candidate = encode_prefix(probe)?;
            if candidate.len() as u64 <= self.byte_budget {
                best_keep = probe;
                best_body = candidate;
                probe = probe.saturating_mul(2).min(total);
            } else {
                first_over = probe;
                break;
            }
        }

        let mut low = best_keep + 1;
        let mut high = first_over;
        while low < high {
            let keep = low + (high - low) / 2;
            let candidate = encode_prefix(keep)?;
            if candidate.len() as u64 <= self.byte_budget {
                best_keep = keep;
                best_body = candidate;
                low = keep + 1;
            } else {
                high = keep;
            }
        }

        self.truncate_rdf_rows(best_keep)?;
        Ok(best_body)
    }

    fn truncate_rdf_rows(&mut self, keep: usize) -> Result<(), Problem> {
        debug_assert!(keep > 0 && keep < self.rows.len());
        let next = self.rdf_row_cursor(keep)?;
        self.rows.truncate(keep);
        self.row_resumes.truncate(keep);
        self.completeness = Completeness::budget_exhausted(BudgetReason::ResponseBytes, next);
        Ok(())
    }

    /// Encode only the logarithmically many cursors RDF byte fitting probes.
    /// JSON and HTML never call this path, so they pay no per-row base64 work.
    fn rdf_row_cursor(&self, index: usize) -> Result<CursorToken, Problem> {
        let Some(resume) = self.row_resumes.get(index) else {
            tracing::error!(
                rows = self.rows.len(),
                row_resumes = self.row_resumes.len(),
                index,
                "an RDF-renderable answer has no cursor for its omitted row"
            );
            return Err(Problem::new(
                ErrorCode::InternalError,
                "the fragment page could not be resumed",
            ));
        };
        let Some(binding) = &self.row_binding else {
            tracing::error!("an RDF-renderable answer has no cursor binding for its omitted row");
            return Err(Problem::new(
                ErrorCode::InternalError,
                "the fragment page could not be resumed",
            ));
        };
        Ok(resume.cursor(binding))
    }

    fn fragment_rdf(
        &self,
        representation: Representation,
        metadata: Option<&TpfMetadata>,
    ) -> Result<Bytes, Problem> {
        self.fragment_rdf_prefix(
            representation,
            self.rows.len(),
            self.completeness.next_cursor(),
            metadata,
        )
    }

    fn fragment_rdf_prefix(
        &self,
        representation: Representation,
        keep: usize,
        next_cursor: Option<&str>,
        metadata: Option<&TpfMetadata>,
    ) -> Result<Bytes, Problem> {
        let syntax = representation.rdf_syntax().ok_or_else(|| {
            Problem::new(
                ErrorCode::InternalError,
                "the caller selected a non-RDF fragment representation",
            )
        })?;
        if matches!(self.echo, Echo::Describe { .. } | Echo::Sample { .. }) {
            return Err(Problem::new(
                ErrorCode::InternalError,
                "RDF was negotiated for an operation that does not publish an RDF graph",
            ));
        }
        if keep > self.rows.len() {
            tracing::error!(
                rows = self.rows.len(),
                keep,
                "an RDF-renderable answer was asked for rows it does not hold"
            );
            return Err(Problem::new(
                ErrorCode::InternalError,
                "the fragment page could not be represented as RDF",
            ));
        }
        // The quad view has no single graph to serve, so a graph syntax cannot
        // carry it; every other form is one graph and serializes untagged.
        if matches!(syntax, RdfSyntax::Graph(_)) && self.tagging == GraphTagging::PerRow {
            return Err(Problem::new(
                ErrorCode::NotAcceptable,
                "the quad view puts each statement in its graph, which a single-graph syntax \
                 cannot represent; ask for N-Quads, TriG or JSON-LD, or scope the request with \
                 a graph",
            ));
        }
        let mut statements = Vec::with_capacity(keep);
        let mut data = HashSet::with_capacity(keep);
        for row in self.rows.iter().take(keep) {
            let cell = |position| {
                let bound = match &self.echo {
                    Echo::Fragment { pattern, .. } => pattern.bound(position),
                    Echo::BindingsFragment { pattern, .. } => pattern.bound(position),
                    Echo::Describe { .. } | Echo::Sample { .. } => None,
                };
                rdf_fragment_cell(bound, row, position)
            };
            let subject =
                cell(Position::Subject).expect("every fragment row binds every triple position");
            let predicate =
                cell(Position::Predicate).expect("every fragment row binds every triple position");
            let object =
                cell(Position::Object).expect("every fragment row binds every triple position");
            // No skolemization here: a row is materialized in its published
            // spelling, so a data blank node is already the scoped IRI and
            // these see a named node. That is what keeps the RDF and native
            // representations naming one node the same way.
            let triple = Triple::new(
                rdf_subject(subject.as_bytes())?,
                NamedNode::new(predicate)
                    .map_err(|error| unreadable("parsing an RDF predicate IRI", &error))?,
                rdf_object(object.as_bytes())?,
            );
            let graph_name = match &self.tagging {
                GraphTagging::Untagged => GraphName::DefaultGraph,
                GraphTagging::Fixed(name) => {
                    GraphName::NamedNode(metadata_iri(name, "a graph name")?)
                }
                GraphTagging::PerRow => match row.graph.as_deref() {
                    None | Some(kgf_store::UNNAMED_GRAPH_IRI) => GraphName::DefaultGraph,
                    Some(name) => GraphName::NamedNode(metadata_iri(name, "a graph name")?),
                },
            };
            let quad = triple.in_graph(graph_name);
            if data.insert(quad.clone()) {
                statements.push(quad);
            }
        }

        let next = match (metadata, next_cursor) {
            (Some(metadata), Some(cursor)) => Some(self.tpf_next(metadata, cursor)?),
            _ => None,
        };
        let prefixes = [("kgfbn", self.blank_nodes.iri_prefix())];
        let serialized = match syntax {
            RdfSyntax::Graph(format) => {
                // One graph, so the statements' graph names are dropped: a
                // scoped or union answer serialized as Turtle is its triples.
                let mut graph: Vec<Triple> = statements
                    .into_iter()
                    .map(|quad| Triple::new(quad.subject, quad.predicate, quad.object))
                    .collect();
                if let Some(metadata) = metadata {
                    graph.reserve(metadata.quads.len() + usize::from(next.is_some()));
                    graph.extend(metadata.quads.iter().map(|quad| {
                        Triple::new(
                            quad.subject.clone(),
                            quad.predicate.clone(),
                            quad.object.clone(),
                        )
                    }));
                }
                graph.extend(next);
                serialize_graph(format, &graph, &prefixes)
            }
            RdfSyntax::Dataset(format) => {
                let metadata_len = metadata.as_ref().map_or(0, |value| value.quads.len());
                let mut quads = statements;
                quads.reserve(metadata_len + usize::from(next.is_some()));
                if let Some(metadata) = metadata {
                    quads.extend(metadata.quads.iter().cloned());
                }
                quads.extend(next.map(|triple| {
                    triple.in_graph(
                        metadata
                            .expect("a TPF next triple has invariant metadata")
                            .graph_name
                            .clone(),
                    )
                }));
                serialize_dataset(format, &quads, &prefixes)
            }
        };
        serialized
            .map(Bytes::from)
            .map_err(|error| unreadable("serializing fragment RDF", &error))
    }

    /// Build the TPF control graph. The caller decides whether the syntax can
    /// preserve its graph name; Turtle necessarily flattens these triples into
    /// its one graph, while N-Quads, TriG, and JSON-LD keep them named.
    fn tpf_metadata(&self) -> Result<TpfMetadata, Problem> {
        let page = metadata_iri(
            &self.target.tpf_page_url().ok_or_else(|| {
                Problem::new(
                    ErrorCode::InternalError,
                    "a TPF response needs the absolute request URL",
                )
            })?,
            "the TPF page URL",
        )?;
        let fragment = metadata_iri(
            &self.target.tpf_fragment_url().ok_or_else(|| {
                Problem::new(
                    ErrorCode::InternalError,
                    "a TPF response needs the absolute request URL",
                )
            })?,
            "the TPF fragment URL",
        )?;
        let dataset = metadata_iri(
            &self.target.absolute_base().ok_or_else(|| {
                Problem::new(
                    ErrorCode::InternalError,
                    "a TPF response needs the request origin",
                )
            })?,
            "the TPF dataset URL",
        )?;
        let metadata_graph = metadata_iri(
            &format!("{}#metadata", page.as_str()),
            "the TPF metadata graph IRI",
        )?;

        // Deterministic labels keep strong validators stable. They are scoped
        // to the named metadata graph and cannot merge with published data
        // blank nodes, which have already been replaced by stable IRIs.
        let mut used = HashSet::new();
        let search = metadata_blank_node("kgf-hydra-search", &mut used);
        let mut mappings = vec![
            (
                "subject",
                RDF_SUBJECT,
                metadata_blank_node("kgf-hydra-subject", &mut used),
            ),
            (
                "predicate",
                RDF_PREDICATE,
                metadata_blank_node("kgf-hydra-predicate", &mut used),
            ),
            (
                "object",
                RDF_OBJECT,
                metadata_blank_node("kgf-hydra-object", &mut used),
            ),
        ];
        // A bundle with memberships publishes the four-position form, and
        // declares the union as its default graph — under a blank node,
        // because that is one of the subjects a client reads the declaration
        // from; the dataset resource itself is not.
        let graphs = self.target.offers.graphs;
        if graphs {
            mappings.push((
                "graph",
                SD_GRAPH,
                metadata_blank_node("kgf-hydra-graph", &mut used),
            ));
        }
        let mut triples = Vec::with_capacity(32);
        triples.push(Triple::new(
            metadata_graph.clone(),
            metadata_iri(FOAF_PRIMARY_TOPIC, "foaf:primaryTopic")?,
            fragment.clone(),
        ));
        triples.push(Triple::new(
            fragment.clone(),
            metadata_iri(VOID_SUBSET, "void:subset")?,
            page.clone(),
        ));
        triples.push(Triple::new(
            dataset.clone(),
            metadata_iri(VOID_SUBSET, "void:subset")?,
            fragment,
        ));
        triples.push(Triple::new(
            dataset.clone(),
            metadata_iri(RDF_TYPE, "rdf:type")?,
            metadata_iri(VOID_DATASET, "void:Dataset")?,
        ));
        triples.push(Triple::new(
            dataset.clone(),
            metadata_iri(HYDRA_SEARCH, "hydra:search")?,
            search.clone(),
        ));
        triples.push(Triple::new(
            search.clone(),
            metadata_iri(HYDRA_TEMPLATE, "hydra:template")?,
            Literal::new_simple_literal(if graphs {
                format!("{}{{?subject,predicate,object,graph}}", dataset.as_str())
            } else {
                format!("{}{{?subject,predicate,object}}", dataset.as_str())
            }),
        ));
        if graphs {
            let default_dataset = metadata_blank_node("kgf-default-dataset", &mut used);
            triples.push(Triple::new(
                dataset.clone(),
                metadata_iri(SD_DEFAULT_DATASET, "sd:defaultDataset")?,
                default_dataset.clone(),
            ));
            triples.push(Triple::new(
                default_dataset,
                metadata_iri(SD_DEFAULT_GRAPH, "sd:defaultGraph")?,
                metadata_iri(kgf_store::UNION_GRAPH_IRI, "the union graph IRI")?,
            ));
        }
        triples.push(Triple::new(
            search.clone(),
            metadata_iri(
                HYDRA_VARIABLE_REPRESENTATION,
                "hydra:variableRepresentation",
            )?,
            metadata_iri(
                HYDRA_EXPLICIT_REPRESENTATION,
                "hydra:ExplicitRepresentation",
            )?,
        ));
        for (variable, property, mapping) in mappings {
            triples.push(Triple::new(
                search.clone(),
                metadata_iri(HYDRA_MAPPING, "hydra:mapping")?,
                mapping.clone(),
            ));
            triples.push(Triple::new(
                mapping.clone(),
                metadata_iri(HYDRA_VARIABLE, "hydra:variable")?,
                Literal::new_simple_literal(variable),
            ));
            triples.push(Triple::new(
                mapping,
                metadata_iri(HYDRA_PROPERTY, "hydra:property")?,
                metadata_iri(property, "an RDF triple-position property")?,
            ));
        }
        triples.push(Triple::new(
            page.clone(),
            metadata_iri(HYDRA_TOTAL_ITEMS, "hydra:totalItems")?,
            Literal::from(self.rdf_cardinality.unwrap_or(self.cardinality).value()),
        ));
        triples.push(Triple::new(
            page.clone(),
            metadata_iri(HYDRA_ITEMS_PER_PAGE, "hydra:itemsPerPage")?,
            Literal::from(u64::from(self.page_limit)),
        ));
        if let Some(dataset_metadata) = &self.target.dataset {
            let logical = metadata_iri(&dataset_metadata.iri, "the manifest dataset IRI")?;
            triples.push(Triple::new(
                page.clone(),
                metadata_iri(VOID_IN_DATASET, "void:inDataset")?,
                logical.clone(),
            ));
            if dataset_metadata.void_available {
                triples.push(Triple::new(
                    logical,
                    metadata_iri(RDFS_SEE_ALSO, "rdfs:seeAlso")?,
                    metadata_iri(
                        &self.target.absolute_void().ok_or_else(|| {
                            Problem::new(
                                ErrorCode::InternalError,
                                "a TPF description link needs the request origin",
                            )
                        })?,
                        "the VoID description URL",
                    )?,
                ));
            }
        }
        let quads = triples
            .into_iter()
            .map(|triple| triple.in_graph(metadata_graph.clone()))
            .collect();
        Ok(TpfMetadata {
            page,
            graph_name: metadata_graph,
            quads,
        })
    }

    fn tpf_next(&self, metadata: &TpfMetadata, cursor: &str) -> Result<Triple, Problem> {
        Ok(Triple::new(
            metadata.page.clone(),
            metadata_iri(HYDRA_NEXT, "hydra:next")?,
            metadata_iri(
                &self.target.absolute_next(cursor).ok_or_else(|| {
                    Problem::new(
                        ErrorCode::InternalError,
                        "a TPF continuation needs the request origin",
                    )
                })?,
                "the TPF continuation URL",
            )?,
        ))
    }
}

/// The term an RDF fragment row carries at `position`, as published.
///
/// A bound position is not a row cell — JSON rows carry variables only — so it
/// comes from the request, which named it in the one spelling that reaches a
/// blank node: the scoped IRI. Everything else is the row's published spelling.
fn rdf_fragment_cell<'a>(
    bound: Option<&'a BoundTerm>,
    row: &'a Row,
    position: Position,
) -> Option<&'a str> {
    bound.map(BoundTerm::dictionary).or_else(|| {
        row.cells
            .iter()
            .find_map(|(found, term)| (*found == position).then_some(term.published.as_ref()))
    })
}

fn id_pattern_matches(pattern: IdPattern, triple: IdTriple) -> bool {
    pattern.subject.is_none_or(|id| id == triple.subject)
        && pattern.predicate.is_none_or(|id| id == triple.predicate)
        && pattern.object.is_none_or(|id| id == triple.object)
}

fn id_pattern_subsumes(general: IdPattern, specific: IdPattern) -> bool {
    general
        .subject
        .is_none_or(|id| specific.subject == Some(id))
        && general
            .predicate
            .is_none_or(|id| specific.predicate == Some(id))
        && general.object.is_none_or(|id| specific.object == Some(id))
}

/// Remove restrictions that cannot add a triple to brTPF's RDF union.
///
/// Equal restrictions keep their first input row. A strictly more general
/// restriction wins regardless of input order: RDF exposes neither binding
/// index, so retaining the narrower phase would only create candidates that a
/// later phase must discard.
fn normalize_rdf_restrictions(restrictions: &mut Vec<(u32, IdPattern)>) {
    let original = restrictions.clone();
    restrictions.retain(|(row, specific)| {
        !original.iter().any(|(other_row, general)| {
            id_pattern_subsumes(*general, *specific) && (general != specific || other_row < row)
        })
    });
}

fn id_patterns_overlap(left: IdPattern, right: IdPattern) -> bool {
    let compatible =
        |left: Option<u64>, right: Option<u64>| left.is_none() || right.is_none() || left == right;
    compatible(left.subject, right.subject)
        && compatible(left.predicate, right.predicate)
        && compatible(left.object, right.object)
}

fn metadata_iri(value: &str, what: &'static str) -> Result<NamedNode, Problem> {
    NamedNode::new(value).map_err(|error| {
        tracing::error!(%error, value, what, "could not construct fragment metadata IRI");
        Problem::new(
            ErrorCode::InternalError,
            "the fragment metadata could not be represented as RDF",
        )
    })
}

fn metadata_blank_node(stem: &str, used: &mut HashSet<String>) -> BlankNode {
    let mut label = stem.to_owned();
    while !used.insert(label.clone()) {
        label.push('_');
    }
    BlankNode::new(label).expect("the metadata blank-node stem is a valid RDF blank-node label")
}

/// `GET /count`'s envelope.
///
/// `count` is an object rather than a bare integer so that it has the same
/// shape as row-page `cardinality` and can represent an
/// interrupted text count needs: `{"value": n, "exact": false, "min": n}`.
/// One field with two shapes would be a client-breaking change waiting to
/// happen.
#[derive(Debug, Serialize)]
pub struct CountAnswer {
    dataset: String,
    version: String,
    pattern: Pattern,
    #[serde(skip_serializing_if = "Option::is_none")]
    g: Option<String>,
    count: Cardinality,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    absent_terms: Vec<AbsentTerm>,
    #[serde(flatten)]
    completeness: Completeness,
    #[serde(skip)]
    target: Target,
}

impl Renders for CountAnswer {
    fn render(self, representation: Representation) -> Result<Rendered, Problem> {
        let body = standard_body(&self, representation);
        Ok(Rendered {
            body,
            completeness: self.completeness,
            rows: None,
            cardinality: Some(self.count),
        })
    }
}

/// One exact count produced for an input binding row.
#[derive(Debug, Serialize)]
struct PerBindingCount {
    binding: u32,
    count: Cardinality,
}

/// `QUERY|POST /count`'s per-binding response.
#[derive(Debug, Serialize)]
pub struct BindingCountAnswer {
    dataset: String,
    version: String,
    pattern: BindingPattern,
    #[serde(skip_serializing_if = "Option::is_none")]
    g: Option<String>,
    counts: Vec<PerBindingCount>,
    #[serde(flatten)]
    completeness: Completeness,
    #[serde(skip)]
    target: Target,
}

/// One entity returned by `/search`.
///
/// `label` has two optional layers on purpose: the outer one says hydration was
/// requested, while the inner one says whether this bundle found a label. This
/// keeps `labels=false` (field absent) distinct from `labels=true` with no label
/// (explicit `null`).
#[derive(Debug)]
struct SearchResult {
    subject: Rc<str>,
    label: Option<Option<String>>,
    evidence: SearchEvidence,
    ranking: Ranking,
    serialized: u64,
}

impl SearchResult {
    fn new(
        subject: Rc<str>,
        subject_serialized: u64,
        label: Option<Option<String>>,
        predicate: Rc<str>,
        literal: Rc<str>,
        ranking: Ranking,
    ) -> Self {
        let evidence = SearchEvidence { predicate, literal };
        let serialized = match &label {
            None => serialized_object([
                ("subject", subject_serialized),
                ("match", evidence.serialized()),
                (MATCH_KIND, serialized_json_string(ranking.kind)),
                (SCORE, serialized_score(ranking.score)),
            ]),
            Some(label) => serialized_object([
                ("subject", subject_serialized),
                ("label", label.as_deref().map_or(4, serialized_json_string)),
                ("match", evidence.serialized()),
                (MATCH_KIND, serialized_json_string(ranking.kind)),
                (SCORE, serialized_score(ranking.score)),
            ]),
        };
        Self {
            subject,
            label,
            evidence,
            ranking,
            serialized,
        }
    }
}

impl Serialize for SearchResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("subject", &Term::from_dictionary(&self.subject))?;
        if let Some(label) = &self.label {
            map.serialize_entry("label", label)?;
        }
        map.serialize_entry("match", &self.evidence)?;
        map.serialize_entry(MATCH_KIND, self.ranking.kind)?;
        map.serialize_entry(SCORE, &self.ranking.score)?;
        map.end()
    }
}

/// The statement that caused one subject to enter a search result.
#[derive(Debug)]
struct SearchEvidence {
    predicate: Rc<str>,
    literal: Rc<str>,
}

impl SearchEvidence {
    fn serialized(&self) -> u64 {
        let predicate = serialized_json_string(&self.predicate);
        match Term::from_dictionary(&self.literal) {
            Term::Literal(literal) => {
                let value = serialized_json_string(literal.value());
                match literal.kind() {
                    LiteralKind::Plain => {
                        serialized_object([("predicate", predicate), ("literal", value)])
                    }
                    LiteralKind::Language(language) => serialized_object([
                        ("predicate", predicate),
                        ("literal", value),
                        ("lang", serialized_json_string(language)),
                    ]),
                    LiteralKind::Datatype(datatype) => serialized_object([
                        ("predicate", predicate),
                        ("literal", value),
                        ("datatype", serialized_json_string(datatype)),
                    ]),
                }
            }
            _ => serialized_object([
                ("predicate", predicate),
                ("literal", serialized_json_string(&self.literal)),
            ]),
        }
    }
}

impl Serialize for SearchEvidence {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("predicate", self.predicate.as_ref())?;
        match Term::from_dictionary(&self.literal) {
            Term::Literal(literal) => {
                map.serialize_entry("literal", literal.value())?;
                match literal.kind() {
                    LiteralKind::Plain => {}
                    LiteralKind::Language(language) => {
                        map.serialize_entry("lang", language.as_ref())?;
                    }
                    LiteralKind::Datatype(datatype) => {
                        map.serialize_entry("datatype", datatype.as_ref())?;
                    }
                }
            }
            // The exhaustive text index contains literals only. Reaching this
            // branch means the index and dictionary disagree, and the request
            // will already have failed while constructing the result.
            _ => map.serialize_entry("literal", self.literal.as_ref())?,
        }
        map.end()
    }
}

/// One graph of a `/graphs` listing.
#[derive(Debug)]
struct GraphEntry {
    published: Rc<str>,
    count: u64,
    serialized: u64,
}

impl GraphEntry {
    fn new(published: Rc<str>, term: u64, count: u64) -> Self {
        let serialized =
            serialized_object([(GRAPH, term), ("count", count.to_string().len() as u64)]);
        Self {
            published,
            count,
            serialized,
        }
    }
}

impl Serialize for GraphEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry(GRAPH, &Term::from_dictionary(&self.published))?;
        map.serialize_entry("count", &self.count)?;
        map.end()
    }
}

/// `GET /graphs`' page of the bundle's graphs.
#[derive(Debug, Serialize)]
pub struct GraphsAnswer {
    dataset: String,
    version: String,
    /// Distinct triples: what the union counts.
    triples: u64,
    /// Memberships over every graph: what the quad view counts. At least
    /// `triples`, and equal to it only when no triple is in two graphs.
    memberships: u64,
    /// The graphs listed across every page: the named graphs, plus the
    /// unnamed graph when it holds any triple.
    cardinality: Cardinality,
    graphs: Vec<GraphEntry>,
    #[serde(flatten)]
    completeness: Completeness,
    #[serde(skip)]
    target: Target,
    #[serde(skip)]
    blank_nodes: SkolemScope,
}

impl Renders for GraphsAnswer {
    fn render(self, representation: Representation) -> Result<Rendered, Problem> {
        let body = standard_body(&self, representation);
        let rows = Some(self.graphs.len() as u64);
        Ok(Rendered {
            body,
            completeness: self.completeness,
            rows,
            cardinality: Some(self.cardinality),
        })
    }
}

/// `GET /search`'s entity-level response.
#[derive(Debug, Serialize)]
pub struct SearchAnswer {
    dataset: String,
    version: String,
    query: String,
    roles: Vec<String>,
    predicates: Vec<String>,
    labels: bool,
    results: Vec<SearchResult>,
    #[serde(flatten)]
    completeness: Completeness,
    #[serde(skip)]
    target: Target,
    /// Only the page needs this: a subject is already published scoped, and
    /// HTML spells such a term `_:{section}-{local-id}` for its reader.
    #[serde(skip)]
    blank_nodes: SkolemScope,
}

impl Renders for SearchAnswer {
    fn render(self, representation: Representation) -> Result<Rendered, Problem> {
        let body = standard_body(&self, representation);
        let rows = Some(self.results.len() as u64);
        Ok(Rendered {
            body,
            completeness: self.completeness,
            rows,
            cardinality: None,
        })
    }
}

/// One term a `/terms` page returned.
///
/// `roles` is what the merge learned for free: a scan visits every covered
/// section standing at the same string at once, so the positions a term occupies
/// come out of the same step that emitted it.
#[derive(Debug)]
struct TermRow {
    published: Rc<str>,
    roles: Vec<&'static str>,
    /// Two optional layers, as in a search result: the outer says hydration was
    /// requested, the inner whether this bundle found a label.
    label: Option<Option<String>>,
    serialized: u64,
}

impl TermRow {
    fn new(
        published: Rc<str>,
        term: u64,
        roles: Vec<&'static str>,
        label: Option<Option<String>>,
    ) -> Self {
        let roles_serialized = 2
            + roles
                .iter()
                .map(|role| serialized_json_string(role))
                .sum::<u64>()
            + roles.len().saturating_sub(1) as u64;
        let serialized = match &label {
            None => serialized_object([("term", term), ("roles", roles_serialized)]),
            Some(label) => serialized_object([
                ("term", term),
                ("roles", roles_serialized),
                ("label", label.as_deref().map_or(4, serialized_json_string)),
            ]),
        };
        Self {
            published,
            roles,
            label,
            serialized,
        }
    }
}

impl Serialize for TermRow {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("term", &Term::from_dictionary(&self.published))?;
        map.serialize_entry("roles", &self.roles)?;
        if let Some(label) = &self.label {
            map.serialize_entry("label", label)?;
        }
        map.end()
    }
}

/// `GET /terms`' page of the dictionary under one byte prefix.
#[derive(Debug, Serialize)]
pub struct TermsPage {
    dataset: String,
    version: String,
    prefix: String,
    role: &'static str,
    /// Distinct terms the prefix matches in this role, exactly.
    ///
    /// Carried with the page for the reason a fragment page carries its own:
    /// bracketing the prefix is what produced the page, so the total is already
    /// known and a client should not have to spend a second request to learn how
    /// far it is through the scan.
    cardinality: Cardinality,
    terms: Vec<TermRow>,
    #[serde(flatten)]
    completeness: Completeness,
    #[serde(skip)]
    target: Target,
    /// Only the page needs this: a scanned term is already published scoped, and
    /// HTML spells such a term `_:{section}-{local-id}` for its reader.
    #[serde(skip)]
    blank_nodes: SkolemScope,
}

/// `GET /terms?count=true`' exact number of distinct terms under one prefix.
#[derive(Debug, Serialize)]
pub struct TermsCount {
    dataset: String,
    version: String,
    prefix: String,
    role: &'static str,
    /// The requested role's count, in the shape `/count` uses.
    count: Cardinality,
    /// Every role's count, always.
    ///
    /// The four numbers come out of the same four bracketing searches, so three
    /// of them are free — and the breakdown is what the question behind this
    /// operation actually asks. "Does this dataset use MONDO" is answered by
    /// *how*: as subjects, as objects it links to, or as predicates. A client
    /// fanning one probe across a federation would otherwise send four.
    counts: RoleBreakdown,
    #[serde(flatten)]
    completeness: Completeness,
    #[serde(skip)]
    target: Target,
}

/// One count per term position, and one over all of them.
#[derive(Debug, Serialize)]
pub struct RoleBreakdown {
    subject: u64,
    predicate: u64,
    object: u64,
    /// Distinct terms over every position, which is not the sum of the other
    /// three: a shared term is both a subject and an object, and a predicate may
    /// repeat either.
    any: u64,
}

impl From<RoleCounts> for RoleBreakdown {
    fn from(counts: RoleCounts) -> Self {
        Self {
            subject: counts.subject(),
            predicate: counts.predicate(),
            object: counts.object(),
            any: counts.any(),
        }
    }
}

/// The two shapes `GET /terms` answers in.
///
/// An enum rather than a page with an optional count, because the count is not a
/// summary of the page: it is the answer to a different question, computed
/// without enumerating anything.
#[derive(Debug)]
pub enum TermsAnswer {
    /// A page of terms.
    Page(TermsPage),
    /// One exact count.
    Count(TermsCount),
}

impl Renders for TermsAnswer {
    fn render(self, representation: Representation) -> Result<Rendered, Problem> {
        match self {
            Self::Page(page) => {
                let body = standard_body(&page, representation);
                let rows = Some(page.terms.len() as u64);
                let cardinality = Some(page.cardinality);
                Ok(Rendered {
                    body,
                    completeness: page.completeness,
                    rows,
                    cardinality,
                })
            }
            Self::Count(count) => {
                let body = standard_body(&count, representation);
                Ok(Rendered {
                    body,
                    completeness: count.completeness,
                    rows: None,
                    cardinality: Some(count.count),
                })
            }
        }
    }
}

/// One requested IRI and its preferred label.
#[derive(Debug)]
struct LabelResult {
    iri: String,
    label: Option<String>,
    serialized: u64,
}

impl LabelResult {
    fn new(iri: String, label: Option<String>) -> Self {
        let iri_term = serialized_object([
            ("type", serialized_json_string("iri")),
            ("value", serialized_json_string(&iri)),
        ]);
        let serialized = serialized_object([
            ("iri", iri_term),
            ("label", label.as_deref().map_or(4, serialized_json_string)),
        ]);
        Self {
            iri,
            label,
            serialized,
        }
    }
}

impl Serialize for LabelResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("iri", &Term::from_dictionary(&self.iri))?;
        map.serialize_entry("label", &self.label)?;
        map.end()
    }
}

/// `QUERY|POST /labels`'s ordered batch response.
#[derive(Debug, Serialize)]
pub struct LabelsAnswer {
    dataset: String,
    version: String,
    labels: Vec<LabelResult>,
    #[serde(flatten)]
    completeness: Completeness,
    #[serde(skip)]
    target: Target,
}

impl Renders for LabelsAnswer {
    fn render(self, representation: Representation) -> Result<Rendered, Problem> {
        let body = standard_body(&self, representation);
        let rows = Some(self.labels.len() as u64);
        Ok(Rendered {
            body,
            completeness: self.completeness,
            rows,
            cardinality: None,
        })
    }
}

impl Renders for BindingCountAnswer {
    fn render(self, representation: Representation) -> Result<Rendered, Problem> {
        let body = standard_body(&self, representation);
        let rows = Some(self.counts.len() as u64);
        Ok(Rendered {
            body,
            completeness: self.completeness,
            rows,
            cardinality: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Schema responses
// ---------------------------------------------------------------------------

/// One materialized term from the description graph.
///
/// It stays in dictionary spelling until `Serialize`, exactly like an ordinary
/// result row, but is wrapped separately because schema nodes are not triples
/// over the primary data dictionary.
#[derive(Debug, Clone)]
struct SchemaTerm(Rc<str>);

impl Serialize for SchemaTerm {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Term::from_dictionary(&self.0).serialize(serializer)
    }
}

#[derive(Debug, Clone, Serialize)]
struct SchemaCounts {
    #[serde(skip_serializing_if = "Option::is_none")]
    entities: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    triples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    distinct_subjects: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    distinct_objects: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    properties: Option<u64>,
}

impl From<StoreSchemaCounts> for SchemaCounts {
    fn from(counts: StoreSchemaCounts) -> Self {
        Self {
            entities: counts.entities,
            triples: counts.triples,
            distinct_subjects: counts.distinct_subjects,
            distinct_objects: counts.distinct_objects,
            properties: counts.properties,
        }
    }
}

/// One selected or shallow child partition in the wire projection.
#[derive(Debug, Clone, Serialize)]
struct SchemaResource {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    term: Option<SchemaTerm>,
    counts: SchemaCounts,
    links: BTreeMap<&'static str, String>,
    /// Raw child offset at which this item begins; absent on the selected node.
    #[serde(skip)]
    position: Option<u64>,
}

/// Metadata for one shallow child collection.
#[derive(Debug, Clone, Serialize)]
struct SchemaCollectionResource {
    kind: &'static str,
    returned: u64,
    /// Cursor order is the immutable order published in the VoID index. It is
    /// stable, but unlike the flat projections it is not a semantic ranking.
    order: &'static str,
}

/// The filters that scope one flat schema projection.
#[derive(Debug, Clone, Serialize)]
struct SchemaProjectionFilters {
    #[serde(skip_serializing_if = "Option::is_none")]
    class: Option<SchemaTerm>,
    #[serde(skip_serializing_if = "Option::is_none")]
    predicate: Option<SchemaTerm>,
}

/// Machine-readable ordering contract for a flat projection.
#[derive(Debug, Clone, Copy, Serialize)]
struct SchemaProjectionOrder {
    by: &'static str,
    direction: &'static str,
    tie_break: &'static [&'static str],
}

const CLASS_RELATION_ORDER: SchemaProjectionOrder = SchemaProjectionOrder {
    by: "triples",
    direction: "descending",
    tie_break: &["subject-class", "predicate", "object-class"],
};

const CLASS_PROPERTY_ORDER: SchemaProjectionOrder = SchemaProjectionOrder {
    by: "triples",
    direction: "descending",
    tie_break: &["class", "predicate"],
};

/// The semantic path that selected a node, independent of its opaque VoID
/// subject. A property or datatype term alone does not reveal whether its
/// counts are dataset-wide or scoped beneath one class.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum SchemaSelectorResource {
    Dataset,
    Class {
        class: SchemaTerm,
    },
    Property {
        #[serde(skip_serializing_if = "Option::is_none")]
        class: Option<SchemaTerm>,
        predicate: SchemaTerm,
    },
    Datatype {
        #[serde(skip_serializing_if = "Option::is_none")]
        class: Option<SchemaTerm>,
        predicate: SchemaTerm,
        datatype: SchemaTerm,
    },
}

impl SchemaSelectorResource {
    fn kind(&self) -> &'static str {
        match self {
            Self::Dataset => "dataset",
            Self::Class { .. } => "class",
            Self::Property { .. } => "property",
            Self::Datatype { .. } => "datatype",
        }
    }

    fn class(&self) -> Option<&SchemaTerm> {
        match self {
            Self::Dataset => None,
            Self::Class { class } => Some(class),
            Self::Property { class, .. } | Self::Datatype { class, .. } => class.as_ref(),
        }
    }

    fn predicate(&self) -> Option<&SchemaTerm> {
        match self {
            Self::Dataset | Self::Class { .. } => None,
            Self::Property { predicate, .. } | Self::Datatype { predicate, .. } => Some(predicate),
        }
    }

    fn datatype(&self) -> Option<&SchemaTerm> {
        match self {
            Self::Datatype { datatype, .. } => Some(datatype),
            Self::Dataset | Self::Class { .. } | Self::Property { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ClassRelationResource {
    subject_class: SchemaTerm,
    predicate: SchemaTerm,
    object_class: SchemaTerm,
    triples: u64,
    #[serde(skip)]
    position: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ClassPropertyResource {
    class: SchemaTerm,
    predicate: SchemaTerm,
    triples: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    distinct_subjects: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    distinct_objects: Option<u64>,
    #[serde(skip)]
    position: u64,
}

impl ClassPropertyResource {
    fn new(
        class: &str,
        predicate: &str,
        triples: u64,
        distinct_subjects: Option<u64>,
        distinct_objects: Option<u64>,
        position: u64,
    ) -> Self {
        Self {
            class: SchemaTerm(Rc::from(class)),
            predicate: SchemaTerm(Rc::from(predicate)),
            triples,
            distinct_subjects,
            distinct_objects,
            position,
        }
    }
}

impl ClassRelationResource {
    fn new(
        subject_class: &str,
        predicate: &str,
        object_class: &str,
        triples: u64,
        position: u64,
    ) -> Self {
        Self {
            subject_class: SchemaTerm(Rc::from(subject_class)),
            predicate: SchemaTerm(Rc::from(predicate)),
            object_class: SchemaTerm(Rc::from(object_class)),
            triples,
            position,
        }
    }
}

/// The node-navigation shape of `GET /schema`.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaNavigationAnswer {
    dataset: String,
    version: String,
    view: String,
    selector: SchemaSelectorResource,
    node: Option<SchemaResource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    collection: Option<SchemaCollectionResource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    items: Option<Vec<SchemaResource>>,
    /// Preferred labels keyed by full IRI. Present only for `labels=true` in
    /// JSON; HTML uses the same bounded hydration internally.
    #[serde(skip_serializing_if = "Option::is_none")]
    labels: Option<BTreeMap<String, Option<String>>>,
    #[serde(flatten)]
    completeness: Completeness,
    #[serde(skip)]
    target: Target,
    #[serde(skip)]
    byte_budget: u64,
    #[serde(skip)]
    byte_binding: CursorBinding,
    #[serde(skip)]
    byte_continuation: Option<u64>,
}

/// The flat observed-class-relation shape of `GET /schema`.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaRelationsAnswer {
    dataset: String,
    version: String,
    view: String,
    projection: &'static str,
    filters: SchemaProjectionFilters,
    order: SchemaProjectionOrder,
    items: Vec<ClassRelationResource>,
    /// Preferred labels keyed by full IRI. See [`SchemaNavigationAnswer`].
    #[serde(skip_serializing_if = "Option::is_none")]
    labels: Option<BTreeMap<String, Option<String>>>,
    #[serde(flatten)]
    completeness: Completeness,
    #[serde(skip)]
    target: Target,
    #[serde(skip)]
    byte_budget: u64,
    #[serde(skip)]
    byte_binding: CursorBinding,
    #[serde(skip)]
    byte_continuation: Option<u64>,
}

/// The count-ranked class-property shape of `GET /schema`.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaClassPropertiesAnswer {
    dataset: String,
    version: String,
    view: String,
    projection: &'static str,
    filters: SchemaProjectionFilters,
    order: SchemaProjectionOrder,
    items: Vec<ClassPropertyResource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    labels: Option<BTreeMap<String, Option<String>>>,
    #[serde(flatten)]
    completeness: Completeness,
    #[serde(skip)]
    target: Target,
    #[serde(skip)]
    byte_budget: u64,
    #[serde(skip)]
    byte_binding: CursorBinding,
    #[serde(skip)]
    byte_continuation: Option<u64>,
}

/// Either response shape selected by one typed `/schema` request.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SchemaAnswer {
    /// One node and optionally one shallow child collection.
    Navigation(SchemaNavigationAnswer),
    /// The count-ranked flat class-relation projection.
    Relations(SchemaRelationsAnswer),
    /// The count-ranked class-property inventory.
    ClassProperties(SchemaClassPropertiesAnswer),
}

impl SchemaAnswer {
    fn completeness(&self) -> &Completeness {
        match self {
            Self::Navigation(answer) => &answer.completeness,
            Self::Relations(answer) => &answer.completeness,
            Self::ClassProperties(answer) => &answer.completeness,
        }
    }

    fn byte_budget(&self) -> u64 {
        match self {
            Self::Navigation(answer) => answer.byte_budget,
            Self::Relations(answer) => answer.byte_budget,
            Self::ClassProperties(answer) => answer.byte_budget,
        }
    }

    fn byte_continuation(&self) -> Option<u64> {
        match self {
            Self::Navigation(answer) => answer.byte_continuation,
            Self::Relations(answer) => answer.byte_continuation,
            Self::ClassProperties(answer) => answer.byte_continuation,
        }
    }

    fn item_count(&self) -> usize {
        match self {
            Self::Navigation(answer) => answer.items.as_ref().map_or(0, Vec::len),
            Self::Relations(answer) => answer.items.len(),
            Self::ClassProperties(answer) => answer.items.len(),
        }
    }

    fn item_position(&self, index: usize) -> Option<u64> {
        match self {
            Self::Navigation(answer) => answer.items.as_ref()?.get(index)?.position,
            Self::Relations(answer) => answer.items.get(index).map(|item| item.position),
            Self::ClassProperties(answer) => answer.items.get(index).map(|item| item.position),
        }
    }

    fn truncate_items(&mut self, keep: usize) {
        match self {
            Self::Navigation(answer) => {
                if let Some(items) = &mut answer.items {
                    items.truncate(keep);
                    if let Some(collection) = &mut answer.collection {
                        collection.returned = items.len() as u64;
                    }
                }
            }
            Self::Relations(answer) => answer.items.truncate(keep),
            Self::ClassProperties(answer) => answer.items.truncate(keep),
        }
    }

    fn set_byte_completeness(&mut self, position: u64) {
        match self {
            Self::Navigation(answer) => {
                answer.byte_continuation = Some(position);
                answer.completeness = Completeness::budget_exhausted(
                    BudgetReason::ResponseBytes,
                    Cursor::at_schema_child(&answer.byte_binding, position).encode(),
                );
            }
            Self::Relations(answer) => {
                answer.byte_continuation = Some(position);
                answer.completeness = Completeness::budget_exhausted(
                    BudgetReason::ResponseBytes,
                    Cursor::at_class_relation(&answer.byte_binding, position).encode(),
                );
            }
            Self::ClassProperties(answer) => {
                answer.byte_continuation = Some(position);
                answer.completeness = Completeness::budget_exhausted(
                    BudgetReason::ResponseBytes,
                    Cursor::at_class_property(&answer.byte_binding, position).encode(),
                );
            }
        }
    }

    fn prune_labels(&mut self) {
        let wanted = self.labelable_terms().into_values().collect::<HashSet<_>>();
        let labels = match self {
            Self::Navigation(answer) => &mut answer.labels,
            Self::Relations(answer) => &mut answer.labels,
            Self::ClassProperties(answer) => &mut answer.labels,
        };
        if let Some(labels) = labels {
            labels.retain(|iri, _| wanted.contains(iri));
        }
    }

    fn rendered_size(&self, representation: Representation) -> u64 {
        standard_body(self, representation).len() as u64
    }

    /// Fit the complete selected representation, including its envelope,
    /// completeness metadata, links, filters, ordering, and hydrated labels.
    ///
    /// The ordinary path serializes once. Only an over-budget response pays a
    /// logarithmic number of additional bounded serializations to find the
    /// largest item prefix that fits. One item is still allowed through when
    /// it alone exceeds the budget, preserving positional progress.
    fn fit_response_bytes(&mut self, representation: Representation) {
        let budget = self.byte_budget();
        if self.rendered_size(representation) <= budget {
            return;
        }

        let original = self.clone();
        let total = original.item_count();
        if total == 0 {
            return;
        }
        if total == 1 {
            if let Some(position) = original.byte_continuation() {
                self.set_byte_completeness(position);
            }
            return;
        }

        let mut low = 1usize;
        let mut high = total - 1;
        let mut best = None;
        while low <= high {
            let keep = low + (high - low) / 2;
            let position = original
                .item_position(keep)
                .expect("every schema page item carries its resume position");
            let mut candidate = original.clone();
            candidate.truncate_items(keep);
            candidate.set_byte_completeness(position);
            candidate.prune_labels();
            if candidate.rendered_size(representation) <= budget {
                best = Some(keep);
                low = keep + 1;
            } else {
                high = keep - 1;
            }
        }

        let keep = best.unwrap_or(1);
        let position = original
            .item_position(keep)
            .expect("a truncated schema page has a first omitted item");
        *self = original;
        self.truncate_items(keep);
        self.set_byte_completeness(position);
        self.prune_labels();
    }
}

impl Renders for SchemaAnswer {
    fn render(mut self, representation: Representation) -> Result<Rendered, Problem> {
        self.fit_response_bytes(representation);
        let completeness = self.completeness().clone();
        let rows = Some(self.item_count() as u64);
        let body = standard_body(&self, representation);
        Ok(Rendered {
            body,
            completeness,
            rows,
            cardinality: None,
        })
    }

    fn hydrate_labels(
        &mut self,
        store: &Store,
        label_predicates: &[String],
        cap: usize,
        required: bool,
    ) -> Result<(), Problem> {
        if label_predicates.is_empty() {
            return Ok(());
        }
        let wanted = self.labelable_terms();
        if wanted.is_empty() {
            return Ok(());
        }
        if wanted.len() > cap {
            if required {
                return Err(Problem::new(
                    ErrorCode::CapExceeded,
                    format!(
                        "this schema page has {} distinct IRIs, over this server's max_label_iris of {cap}",
                        wanted.len()
                    ),
                ));
            }
            return Ok(());
        }

        let dictionary = store.dict();
        let predicates: Vec<u64> = label_predicates
            .iter()
            .map(|iri| {
                dictionary
                    .locate(Role::Predicate, iri.as_bytes())
                    .map(|found| found.map(|id| id.0))
                    .map_err(|error| unreadable("looking a label predicate up", &error))
            })
            .filter_map(Result::transpose)
            .collect::<Result<_, _>>()?;
        let mut cache = TermCache::new();
        let mut labels = BTreeMap::new();
        for (term, iri) in wanted {
            let label = match dictionary
                .locate(Role::Subject, term.as_bytes())
                .map_err(|error| unreadable("looking a schema IRI up", &error))?
            {
                Some(subject) => {
                    preferred_label(store, &dictionary, &mut cache, subject.0, &predicates)?
                }
                None => None,
            };
            labels.insert(iri, label);
        }
        self.set_labels(labels);
        Ok(())
    }
}

impl SchemaAnswer {
    fn labelable_terms(&self) -> BTreeMap<String, String> {
        let mut terms = BTreeMap::new();
        let mut insert = |term: &SchemaTerm| {
            if let Term::Iri(iri) = Term::from_dictionary(&term.0) {
                terms.insert(term.0.to_string(), iri.into_owned());
            }
        };
        match self {
            Self::Navigation(answer) => {
                match &answer.selector {
                    SchemaSelectorResource::Dataset => {}
                    SchemaSelectorResource::Class { class } => insert(class),
                    SchemaSelectorResource::Property { class, predicate } => {
                        if let Some(class) = class {
                            insert(class);
                        }
                        insert(predicate);
                    }
                    SchemaSelectorResource::Datatype {
                        class,
                        predicate,
                        datatype,
                    } => {
                        if let Some(class) = class {
                            insert(class);
                        }
                        insert(predicate);
                        insert(datatype);
                    }
                }
                if let Some(term) = answer.node.as_ref().and_then(|node| node.term.as_ref()) {
                    insert(term);
                }
                for item in answer.items.as_deref().unwrap_or_default() {
                    if let Some(term) = &item.term {
                        insert(term);
                    }
                }
            }
            Self::Relations(answer) => {
                if let Some(class) = &answer.filters.class {
                    insert(class);
                }
                if let Some(predicate) = &answer.filters.predicate {
                    insert(predicate);
                }
                for relation in &answer.items {
                    insert(&relation.subject_class);
                    insert(&relation.predicate);
                    insert(&relation.object_class);
                }
            }
            Self::ClassProperties(answer) => {
                if let Some(class) = &answer.filters.class {
                    insert(class);
                }
                if let Some(predicate) = &answer.filters.predicate {
                    insert(predicate);
                }
                for property in &answer.items {
                    insert(&property.class);
                    insert(&property.predicate);
                }
            }
        }
        terms
    }

    fn set_labels(&mut self, labels: BTreeMap<String, Option<String>>) {
        match self {
            Self::Navigation(answer) => answer.labels = Some(labels),
            Self::Relations(answer) => answer.labels = Some(labels),
            Self::ClassProperties(answer) => answer.labels = Some(labels),
        }
    }
}

fn standard_body(resource: &impl Resource, representation: Representation) -> Bytes {
    match representation {
        Representation::Json => resource.to_json(),
        Representation::Html => Bytes::from(resource.to_html()),
        Representation::NQuads
        | Representation::TriG
        | Representation::Turtle
        | Representation::JsonLd
        | Representation::Markdown => {
            unreachable!("ordinary operations negotiate only JSON and HTML")
        }
    }
}

// ---------------------------------------------------------------------------
// Static description responses
// ---------------------------------------------------------------------------

/// Browser and machine forms of the same VoID graph.
struct VoidResource {
    turtle: Bytes,
    triples: u64,
    completeness: Completeness,
    target: Target,
}

impl VoidResource {
    fn to_html(&self) -> String {
        let canonical = self.target.canonical();
        let context = self.target.context();
        let turtle = String::from_utf8_lossy(&self.turtle);
        operation_page_with_format(
            &self.target.mount,
            "VoID dataset description",
            &context,
            &self.target.crumbs(),
            canonical.as_deref(),
            Representation::JsonLd,
            html! {
                div."answer-summary" {
                    (fields(&[
                        ("triples", Value::Number(self.triples)),
                        ("complete", Value::Text(completeness_text(&self.completeness))),
                    ]))
                }
                section."section-block" {
                    h2 { "Turtle" }
                    p { "This Turtle document is serialized from the published VoID HDT through the shared RDF writer." }
                    pre { code { (turtle) } }
                }
            },
        )
    }
}

/// Exact persisted summary bytes and their browser rendering.
struct SummaryResource {
    markdown: String,
    card: Option<SummaryCard>,
    target: Target,
}

#[derive(Debug, Deserialize)]
struct SummaryCard {
    dataset: SummaryDataset,
    counts: SummaryCounts,
    #[serde(default)]
    links: BTreeMap<String, String>,
    #[serde(default)]
    top_classes: Vec<SummaryClass>,
    #[serde(default)]
    top_properties: Vec<SummaryProperty>,
    #[serde(default)]
    leading_class_relations: Vec<SummaryRelation>,
}

#[derive(Debug, Deserialize)]
struct SummaryDataset {
    id: String,
    version: String,
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SummaryCounts {
    triples: u64,
    subjects: u64,
    predicates: u64,
    objects: u64,
}

#[derive(Debug, Deserialize)]
struct SummaryClass {
    #[serde(rename = "class")]
    class_iri: String,
    entities: u64,
    #[serde(default)]
    links: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct SummaryProperty {
    predicate: String,
    triples: u64,
    #[serde(default)]
    links: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct SummaryRelation {
    subject_class: String,
    predicate: String,
    object_class: String,
    triples: u64,
}

impl SummaryResource {
    fn to_html(&self) -> String {
        let canonical = self.target.canonical();
        let context = self.target.context();
        let Some(card) = &self.card else {
            return operation_page(
                &self.target.mount,
                "Dataset summary",
                &context,
                &self.target.crumbs(),
                canonical.as_deref(),
                html! {
                    section."section-block" {
                        h2 { "Published summary card" }
                        pre { (self.markdown) }
                    }
                },
            );
        };

        let class_cells: Vec<_> = card
            .top_classes
            .iter()
            .map(|class| summary_term_cell(&self.target, &class.class_iri, &class.links))
            .collect();
        let class_rows: Vec<_> = card
            .top_classes
            .iter()
            .zip(&class_cells)
            .map(|(class, cell)| vec![cell.value(), Value::Number(class.entities)])
            .collect();
        let property_cells: Vec<_> = card
            .top_properties
            .iter()
            .map(|property| summary_term_cell(&self.target, &property.predicate, &property.links))
            .collect();
        let property_rows: Vec<_> = card
            .top_properties
            .iter()
            .zip(&property_cells)
            .map(|(property, cell)| vec![cell.value(), Value::Number(property.triples)])
            .collect();
        let relation_cells: Vec<_> = card
            .leading_class_relations
            .iter()
            .map(|relation| {
                [
                    summary_class_cell(&self.target, &relation.subject_class),
                    summary_property_cell(&self.target, &relation.predicate),
                    summary_class_cell(&self.target, &relation.object_class),
                ]
            })
            .collect();
        let relation_rows: Vec<_> = card
            .leading_class_relations
            .iter()
            .zip(&relation_cells)
            .map(|(relation, cells)| {
                vec![
                    cells[0].value(),
                    cells[1].value(),
                    cells[2].value(),
                    Value::Number(relation.triples),
                ]
            })
            .collect();
        let title = card.dataset.title.as_deref().unwrap_or(&card.dataset.id);
        operation_page(
            &self.target.mount,
            title,
            &context,
            &self.target.crumbs(),
            canonical.as_deref(),
            html! {
                section."overview" {
                    p."lede" {
                        "Start here for the graph's major kinds, predicates, and observed typed \
                         connections; follow any term into the bounded schema navigator."
                    }
                    (stats(&[
                        ("triples", group_digits(card.counts.triples)),
                        ("subjects", group_digits(card.counts.subjects)),
                        ("predicates", group_digits(card.counts.predicates)),
                        ("objects", group_digits(card.counts.objects)),
                    ]))
                    (fields(&[
                        ("dataset", Value::Code(&card.dataset.id)),
                        ("version", Value::Code(&card.dataset.version)),
                    ]))
                    (summary_actions(&card.links))
                }
                div."dashboard-grid" {
                    section."panel" {
                        h2 { "Top classes" }
                        (note("Observed classes ranked by entity count in the designed-schema view."))
                        (table(&["Class", "Entities"], &class_rows))
                    }
                    section."panel" {
                        h2 { "Top properties" }
                        (note("Observed predicates ranked by triple count in the designed-schema view."))
                        (table(&["Property", "Triples"], &property_rows))
                    }
                }
                section."section-block" {
                    h2 { "Leading typed class relations" }
                    (note(
                        "Observed typed connections, not declared domain/range axioms. Untyped \
                         targets are absent and multi-typed entities may contribute to several rows."
                    ))
                    (results_table(
                        &["Subject class", "Property", "Object class", "Triples"],
                        &relation_rows,
                    ))
                }
            },
        )
    }
}

fn summary_actions(links: &BTreeMap<String, String>) -> maud::Markup {
    const ACTIONS: [(&str, &str, &str); 7] = [
        (
            "classes",
            "Browse classes",
            "The observed kinds of entities represented in this graph.",
        ),
        (
            "properties",
            "Browse properties",
            "The predicates used to describe and connect those entities.",
        ),
        (
            "class_relations",
            "Map class relations",
            "The count-ranked typed connections between kinds of things.",
        ),
        (
            "class_properties",
            "Compare class properties",
            "The count-ranked predicates used to describe instances of each class.",
        ),
        (
            "schema",
            "Schema overview",
            "The complete bounded drill-down surface and its statistics.",
        ),
        (
            "fragment",
            "Browse triples",
            "Inspect concrete statements from the queryable graph.",
        ),
        (
            "void",
            "Full VoID description",
            "Download the complete RDF statistical description.",
        ),
    ];
    html! {
        nav."schema-actions" aria-label="Dataset discovery" {
            ul {
                @for (relation, label, description) in ACTIONS {
                    @if let Some(href) = links.get(relation) {
                        li {
                            a href=(href) {
                                strong { (label) }
                                span { (description) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn summary_term_cell<'a>(
    target: &Target,
    iri: &'a str,
    links: &BTreeMap<String, String>,
) -> Cell<'a> {
    summary_iri_cell(target, iri, links.get("schema").cloned())
}

fn summary_class_cell<'a>(target: &Target, iri: &'a str) -> Cell<'a> {
    summary_iri_cell(
        target,
        iri,
        Some(summary_schema_href(
            &Params::default()
                .with("class", &format!("<{iri}>"))
                .with("children", "properties")
                .with("view", "design"),
        )),
    )
}

fn summary_property_cell<'a>(target: &Target, iri: &'a str) -> Cell<'a> {
    summary_iri_cell(
        target,
        iri,
        Some(summary_schema_href(
            &Params::default()
                .with("predicate", &format!("<{iri}>"))
                .with("view", "design"),
        )),
    )
}

fn summary_iri_cell<'a>(target: &Target, iri: &'a str, href: Option<String>) -> Cell<'a> {
    let (label, qualifier, full_iri) = Term::from_dictionary(iri)
        .into_display(&target.prefixes)
        .into_structured();
    Cell {
        label,
        qualifier,
        annotation: None,
        href,
        full_iri: full_iri.map(|iri| Cow::Owned(iri.into_owned())),
        structured: true,
    }
}

fn summary_schema_href(params: &Params) -> String {
    format!("schema?{}", params.to_query())
}

/// Serialize `/void` directly from the mapped VoID HDT.
pub fn void(
    store: &Store,
    target: Target,
    request: &request::Void,
    representation: Representation,
) -> Result<Rendered, Problem> {
    let description = store.description().ok_or_else(description_not_built)?;
    let selection = description
        .void_triples()
        .map_err(|error| unreadable("reading the VoID graph", &error))?;
    let total = selection.count().value;
    let dictionary = description.dict();
    let data_dictionary = store.dict();
    let blank_nodes = SkolemScope::new(store.hdt_identity_digest(), *data_dictionary.counts());
    let prefixes = &[("kgfbn", blank_nodes.iri_prefix())];

    let (body, emitted, complete) = match representation {
        Representation::Turtle => {
            let (body, emitted) = serialize_rdf(
                &selection,
                dictionary,
                GraphFormat::Turtle,
                request.bytes.0,
                prefixes,
            )?;
            (body, emitted, emitted == total)
        }
        Representation::JsonLd => {
            let (body, emitted) = serialize_rdf(
                &selection,
                dictionary,
                GraphFormat::JsonLd,
                request.bytes.0,
                prefixes,
            )?;
            (body, emitted, emitted == total)
        }
        Representation::Html => {
            let (turtle, turtle_triples) = serialize_rdf(
                &selection,
                dictionary,
                GraphFormat::Turtle,
                request.bytes.0,
                prefixes,
            )?;
            let complete = turtle_triples == total;
            let completeness = void_completeness(complete);
            let resource = VoidResource {
                turtle,
                triples: turtle_triples,
                completeness: completeness.clone(),
                target,
            };
            return Ok(Rendered {
                body: Bytes::from(resource.to_html()),
                completeness,
                rows: None,
                cardinality: None,
            });
        }
        Representation::Json
        | Representation::NQuads
        | Representation::TriG
        | Representation::Markdown => {
            unreachable!("/void negotiation does not offer this representation")
        }
    };

    Ok(Rendered {
        body,
        completeness: void_completeness(complete && emitted == total),
        rows: None,
        cardinality: None,
    })
}

/// Serve `/summary` from the exact persisted JSON or Markdown document.
pub fn summary(
    store: &Store,
    target: Target,
    _request: &request::Summary,
    representation: Representation,
) -> Result<Rendered, Problem> {
    let description = store.description().ok_or_else(description_not_built)?;
    let body = match representation {
        Representation::Json => Bytes::copy_from_slice(description.summary_json()),
        Representation::Markdown => Bytes::copy_from_slice(
            description
                .summary_markdown()
                .map_err(|error| unreadable("reading the summary card", &error))?
                .as_bytes(),
        ),
        Representation::Html => {
            let json = description.summary_json();
            let resource = SummaryResource {
                card: serde_json::from_slice(json).ok(),
                markdown: description
                    .summary_markdown()
                    .map_err(|error| unreadable("reading the summary card", &error))?
                    .to_owned(),
                target,
            };
            Bytes::from(resource.to_html())
        }
        Representation::NQuads
        | Representation::TriG
        | Representation::Turtle
        | Representation::JsonLd => {
            unreachable!("/summary negotiation does not offer RDF representations")
        }
    };
    Ok(Rendered {
        body,
        completeness: Completeness::complete(),
        rows: None,
        cardinality: None,
    })
}

fn description_not_built() -> Problem {
    Problem::new(
        ErrorCode::CapabilityNotAvailable,
        "this bundle does not carry the complete tier-1 description artifact set",
    )
}

fn void_completeness(complete: bool) -> Completeness {
    if complete {
        Completeness::complete()
    } else {
        Completeness::budget_exhausted_without_resume(BudgetReason::ResponseBytes)
    }
}

fn serialize_rdf(
    selection: &Selection<'_>,
    dictionary: Dictionary<'_>,
    format: GraphFormat,
    byte_limit: u64,
    prefixes: &[(&str, &str)],
) -> Result<(Bytes, u64), Problem> {
    let encode = |triples: &[Triple]| {
        serialize_graph(format, triples, prefixes)
            .map_err(|error| unreadable("serializing the VoID RDF graph", &error))
    };

    // Grow geometrically until a complete serialized document crosses the
    // byte budget, then find the largest fitting prefix. This keeps work
    // bounded by the selected representation's bytes and, unlike interrupting
    // a writer, always lets `oxrdfio` emit its closing syntax.
    let mut triples = Vec::new();
    let mut ids = selection.page(0, usize::MAX);
    let mut best_body = encode(&triples)?;
    let mut best_count = 0usize;
    let mut target = 1usize;

    loop {
        let mut exhausted = false;
        while triples.len() < target {
            let Some(ids) = ids.next() else {
                exhausted = true;
                break;
            };
            triples.push(rdf_triple(dictionary, ids)?);
        }

        if triples.len() == best_count && exhausted {
            return Ok((Bytes::from(best_body), best_count as u64));
        }

        let body = encode(&triples)?;
        if body.len() as u64 <= byte_limit || triples.is_empty() {
            best_count = triples.len();
            best_body = body;
            if exhausted {
                return Ok((Bytes::from(best_body), best_count as u64));
            }
            target = target.saturating_mul(2);
            continue;
        }

        let mut low = best_count;
        let mut high = triples.len();
        while low + 1 < high {
            let middle = low + (high - low) / 2;
            let candidate = encode(&triples[..middle])?;
            if candidate.len() as u64 <= byte_limit {
                low = middle;
                best_body = candidate;
            } else {
                high = middle;
            }
        }
        return Ok((Bytes::from(best_body), low as u64));
    }
}

fn rdf_triple(dictionary: Dictionary<'_>, ids: IdTriple) -> Result<Triple, Problem> {
    let mut buffer = Vec::new();
    let subject = dictionary
        .extract(Role::Subject, TermId(ids.subject), &mut buffer)
        .map_err(|error| unreadable("materializing a VoID subject", &error))?;
    let subject = rdf_subject(subject)?;

    let predicate = dictionary
        .extract(Role::Predicate, TermId(ids.predicate), &mut buffer)
        .map_err(|error| unreadable("materializing a VoID predicate", &error))?;
    let predicate = NamedNode::new(rdf_text(predicate)?)
        .map_err(|error| unreadable("parsing a VoID predicate IRI", &error))?;

    let object = dictionary
        .extract(Role::Object, TermId(ids.object), &mut buffer)
        .map_err(|error| unreadable("materializing a VoID object", &error))?;
    let object = rdf_object(object)?;
    Ok(Triple::new(subject, predicate, object))
}

fn rdf_subject(term: &[u8]) -> Result<NamedOrBlankNode, Problem> {
    let text = rdf_text(term)?;
    if let Some(identifier) = text.strip_prefix("_:") {
        return BlankNode::new(identifier)
            .map(Into::into)
            .map_err(|error| unreadable("parsing a VoID blank-node subject", &error));
    }
    NamedNode::new(text)
        .map(Into::into)
        .map_err(|error| unreadable("parsing a VoID subject IRI", &error))
}

fn rdf_object(term: &[u8]) -> Result<RdfTerm, Problem> {
    if let Some(literal) = parse_literal(term) {
        let value = rdf_text(literal.value)?.to_owned();
        if let Some(language) = literal.language {
            return Literal::new_language_tagged_literal(value, rdf_text(language)?)
                .map(Into::into)
                .map_err(|error| unreadable("parsing a VoID literal language", &error));
        }
        if let Some(datatype) = literal.datatype {
            let datatype = NamedNode::new(rdf_text(datatype)?)
                .map_err(|error| unreadable("parsing a VoID literal datatype", &error))?;
            return Ok(Literal::new_typed_literal(value, datatype).into());
        }
        return Ok(Literal::new_simple_literal(value).into());
    }
    let text = rdf_text(term)?;
    if let Some(identifier) = text.strip_prefix("_:") {
        return BlankNode::new(identifier)
            .map(Into::into)
            .map_err(|error| unreadable("parsing a VoID blank-node object", &error));
    }
    NamedNode::new(text)
        .map(Into::into)
        .map_err(|error| unreadable("parsing a VoID object IRI", &error))
}

fn rdf_text(bytes: &[u8]) -> Result<&str, Problem> {
    std::str::from_utf8(bytes).map_err(|error| unreadable("reading a VoID RDF term", &error))
}

// ---------------------------------------------------------------------------
// The operations
// ---------------------------------------------------------------------------

/// `GET /schema` — one selected partition, one shallow edge, or the persisted
/// flat class-relation projection.
pub fn schema(
    store: &Store,
    target: Target,
    request: &request::Schema,
) -> Result<SchemaAnswer, Problem> {
    let description = store.description().ok_or_else(|| {
        Problem::new(
            ErrorCode::CapabilityNotAvailable,
            "this bundle does not carry the complete tier-1 description artifact set needed by `/schema`",
        )
    })?;
    let view = description
        .view(&request.view)
        .ok_or_else(|| match &request.view {
            StatsView::Component(component) => Problem::new(
                ErrorCode::NotFound,
                format!(
                    "this bundle has no description view for component `{}`",
                    component.as_str()
                ),
            ),
            StatsView::Design | StatsView::Queryable => {
                tracing::error!(?request.view, "a tier-1 description is missing a required view");
                Problem::new(
                    ErrorCode::InternalError,
                    "the bundle's description indexes are missing a required view",
                )
            }
        })?;

    match &request.query {
        SchemaQuery::Node(selection) => {
            let node = view
                .schema_node(selection.store_selector())
                .map_err(|error| unreadable("resolving a schema node", &error))?;
            let mut cache = TermCache::new();
            let node = node
                .map(|node| {
                    materialize_schema_node(
                        &description.dict(),
                        &mut cache,
                        node,
                        selected_node_links(selection, &request.view),
                    )
                })
                .transpose()?;
            Ok(SchemaAnswer::Navigation(SchemaNavigationAnswer {
                dataset: target.id.dataset.clone(),
                version: target.id.version.clone(),
                view: schema_view_name(&request.view),
                selector: selection_resource(selection),
                node,
                collection: None,
                items: None,
                labels: None,
                completeness: Completeness::complete(),
                target,
                byte_budget: request.bytes.0,
                byte_binding: request.binding.clone(),
                byte_continuation: None,
            }))
        }
        SchemaQuery::Children(children) => {
            schema_children(description, view, target, request, children)
        }
        SchemaQuery::ClassRelations(filter) => schema_relations(view, target, request, filter),
        SchemaQuery::ClassProperties(filter) => {
            schema_class_properties(view, target, request, filter)
        }
    }
}

fn schema_children(
    description: &kgf_store::DescriptionStore,
    view: kgf_store::DescriptionView<'_>,
    target: Target,
    request: &request::Schema,
    children: &SchemaChildren,
) -> Result<SchemaAnswer, Problem> {
    let from = request.cursor.as_ref().map_or(0, |cursor| cursor.position);
    let limit = nonzero_schema_limit(request.limit.expect("children carry a page limit"));
    let page = match view.schema_children(children.store_query(), from, limit) {
        Ok(page) => page,
        Err(kgf_store::Error::ResumePositionOutOfRange { .. }) if request.cursor.is_some() => {
            return Err(Problem::from(StaleCursor));
        }
        Err(error) => return Err(unreadable("paging schema children", &error)),
    };
    let page_next = page.next;

    let dictionary = description.dict();
    let mut cache = TermCache::new();
    let parent_links = selected_child_parent_links(children, &request.view);
    let node = page
        .node
        .map(|node| materialize_schema_node(&dictionary, &mut cache, node, parent_links))
        .transpose()?;

    let items = page
        .items
        .into_iter()
        .map(|child| {
            let term = materialize_schema_term(&dictionary, &mut cache, child.node)?;
            let links = child_links(children, &request.view, term.as_deref());
            Ok(schema_resource(
                child.node,
                term,
                links,
                Some(child.position),
            ))
        })
        .collect::<Result<Vec<_>, Problem>>()?;

    let completeness = match page_next {
        Some(position) => {
            Completeness::page_limit(Cursor::at_schema_child(&request.binding, position).encode())
        }
        None => Completeness::complete(),
    };

    Ok(SchemaAnswer::Navigation(SchemaNavigationAnswer {
        dataset: target.id.dataset.clone(),
        version: target.id.version.clone(),
        view: schema_view_name(&request.view),
        selector: child_parent_selection_resource(children),
        node,
        collection: Some(SchemaCollectionResource {
            kind: schema_collection_name(page.collection),
            returned: items.len() as u64,
            order: "published",
        }),
        items: Some(items),
        labels: None,
        completeness,
        target,
        byte_budget: request.bytes.0,
        byte_binding: request.binding.clone(),
        byte_continuation: page_next,
    }))
}

#[derive(Debug, Clone, Copy)]
enum ProjectionStop {
    Complete,
    RowLimit,
    ScanLimit,
}

impl From<ClassRelationStop> for ProjectionStop {
    fn from(stop: ClassRelationStop) -> Self {
        match stop {
            ClassRelationStop::Complete => Self::Complete,
            ClassRelationStop::RowLimit => Self::RowLimit,
            ClassRelationStop::ScanLimit => Self::ScanLimit,
        }
    }
}

impl From<ClassPropertyStop> for ProjectionStop {
    fn from(stop: ClassPropertyStop) -> Self {
        match stop {
            ClassPropertyStop::Complete => Self::Complete,
            ClassPropertyStop::RowLimit => Self::RowLimit,
            ClassPropertyStop::ScanLimit => Self::ScanLimit,
        }
    }
}

fn projection_completeness(
    binding: &CursorBinding,
    page_next: Option<u64>,
    stop: ProjectionStop,
    cursor: fn(&CursorBinding, u64) -> Cursor,
) -> Completeness {
    match stop {
        ProjectionStop::Complete => Completeness::complete(),
        ProjectionStop::RowLimit => Completeness::page_limit(
            cursor(
                binding,
                page_next.expect("a row-limited projection page has a continuation"),
            )
            .encode(),
        ),
        ProjectionStop::ScanLimit => Completeness::budget_exhausted(
            BudgetReason::Candidate,
            cursor(
                binding,
                page_next.expect("a scan-limited projection page has a continuation"),
            )
            .encode(),
        ),
    }
}

fn schema_relations(
    view: kgf_store::DescriptionView<'_>,
    target: Target,
    request: &request::Schema,
    filter: &request::SchemaRelationFilter,
) -> Result<SchemaAnswer, Problem> {
    let from = match request.cursor.as_ref() {
        None => None,
        Some(cursor) => Some(
            view.class_relation_position(cursor.position)
                .ok_or_else(|| Problem::from(StaleCursor))?,
        ),
    };
    let limit = nonzero_schema_limit(request.limit.expect("relations carry a page limit"));
    let scan_limit = NonZeroUsize::new(request.candidates.ceiling())
        .expect("validated configuration has a nonzero candidate budget");
    let page = view
        .class_relations(filter.store_filter(), from, limit, scan_limit)
        .map_err(|error| unreadable("paging schema class relations", &error))?;
    let page_next = page.next.map(|position| position.byte_offset());
    let stop = ProjectionStop::from(page.stop);
    let items = page
        .items
        .into_iter()
        .map(|item| {
            let relation = item.relation;
            ClassRelationResource::new(
                relation.subject_class,
                relation.predicate,
                relation.object_class,
                relation.triples,
                item.position.byte_offset(),
            )
        })
        .collect();
    let completeness =
        projection_completeness(&request.binding, page_next, stop, Cursor::at_class_relation);

    Ok(SchemaAnswer::Relations(SchemaRelationsAnswer {
        dataset: target.id.dataset.clone(),
        version: target.id.version.clone(),
        view: schema_view_name(&request.view),
        projection: "class-relations",
        filters: projection_filters(&filter.class, &filter.predicate),
        order: CLASS_RELATION_ORDER,
        items,
        labels: None,
        completeness,
        target,
        byte_budget: request.bytes.0,
        byte_binding: request.binding.clone(),
        byte_continuation: page_next,
    }))
}

fn schema_class_properties(
    view: kgf_store::DescriptionView<'_>,
    target: Target,
    request: &request::Schema,
    filter: &request::SchemaClassPropertyFilter,
) -> Result<SchemaAnswer, Problem> {
    let from = match request.cursor.as_ref() {
        None => None,
        Some(cursor) => Some(
            view.class_property_position(cursor.position)
                .ok_or_else(|| Problem::from(StaleCursor))?,
        ),
    };
    let limit = nonzero_schema_limit(request.limit.expect("projections carry a page limit"));
    let scan_limit = NonZeroUsize::new(request.candidates.ceiling())
        .expect("validated configuration has a nonzero candidate budget");
    let page = view
        .class_properties(filter.store_filter(), from, limit, scan_limit)
        .map_err(|error| unreadable("paging schema class properties", &error))?;
    let page_next = page.next.map(|position| position.byte_offset());
    let stop = ProjectionStop::from(page.stop);
    let items = page
        .items
        .into_iter()
        .map(|item| {
            let property = item.property;
            ClassPropertyResource::new(
                property.class,
                property.predicate,
                property.triples,
                property.distinct_subjects,
                property.distinct_objects,
                item.position.byte_offset(),
            )
        })
        .collect();
    let completeness =
        projection_completeness(&request.binding, page_next, stop, Cursor::at_class_property);

    Ok(SchemaAnswer::ClassProperties(SchemaClassPropertiesAnswer {
        dataset: target.id.dataset.clone(),
        version: target.id.version.clone(),
        view: schema_view_name(&request.view),
        projection: "class-properties",
        filters: projection_filters(&filter.class, &filter.predicate),
        order: CLASS_PROPERTY_ORDER,
        items,
        labels: None,
        completeness,
        target,
        byte_budget: request.bytes.0,
        byte_binding: request.binding.clone(),
        byte_continuation: page_next,
    }))
}

fn nonzero_schema_limit(limit: u32) -> NonZeroUsize {
    NonZeroUsize::new(limit as usize).expect("request parsing refuses a zero schema limit")
}

fn projection_filters(
    class: &Option<BoundTerm>,
    predicate: &Option<BoundTerm>,
) -> SchemaProjectionFilters {
    SchemaProjectionFilters {
        class: class.as_ref().map(selector_term),
        predicate: predicate.as_ref().map(selector_term),
    }
}

fn materialize_schema_node(
    dictionary: &Dictionary<'_>,
    cache: &mut TermCache,
    node: StoreSchemaNode,
    links: BTreeMap<&'static str, String>,
) -> Result<SchemaResource, Problem> {
    let term = materialize_schema_term(dictionary, cache, node)?;
    Ok(schema_resource(node, term, links, None))
}

fn materialize_schema_term(
    dictionary: &Dictionary<'_>,
    cache: &mut TermCache,
    node: StoreSchemaNode,
) -> Result<Option<Rc<str>>, Problem> {
    node.term()
        .map(|term| {
            cache
                .resolve(dictionary, Role::Object, term)
                .map_err(|error| unreadable("materializing a schema term", &error))
        })
        .transpose()
}

fn schema_resource(
    node: StoreSchemaNode,
    term: Option<Rc<str>>,
    links: BTreeMap<&'static str, String>,
    position: Option<u64>,
) -> SchemaResource {
    SchemaResource {
        kind: schema_kind_name(node.kind()),
        term: term.map(SchemaTerm),
        counts: node.counts().into(),
        links,
        position,
    }
}

fn schema_kind_name(kind: SchemaNodeKind) -> &'static str {
    match kind {
        SchemaNodeKind::Dataset => "dataset",
        SchemaNodeKind::Class => "class",
        SchemaNodeKind::Property => "property",
        SchemaNodeKind::ObjectClass => "object-class",
        SchemaNodeKind::Datatype => "datatype",
        SchemaNodeKind::Language => "language",
    }
}

fn schema_collection_name(collection: SchemaCollection) -> &'static str {
    match collection {
        SchemaCollection::Classes => "classes",
        SchemaCollection::Properties => "properties",
        SchemaCollection::ObjectClasses => "object-classes",
        SchemaCollection::Datatypes => "datatypes",
        SchemaCollection::Languages => "languages",
    }
}

fn selection_resource(selection: &SchemaSelection) -> SchemaSelectorResource {
    match selection {
        SchemaSelection::Dataset => SchemaSelectorResource::Dataset,
        SchemaSelection::Class { class } => SchemaSelectorResource::Class {
            class: selector_term(class),
        },
        SchemaSelection::Property { class, predicate } => SchemaSelectorResource::Property {
            class: class.as_ref().map(selector_term),
            predicate: selector_term(predicate),
        },
        SchemaSelection::Datatype {
            class,
            predicate,
            datatype,
        } => SchemaSelectorResource::Datatype {
            class: class.as_ref().map(selector_term),
            predicate: selector_term(predicate),
            datatype: selector_term(datatype),
        },
    }
}

fn child_parent_selection_resource(children: &SchemaChildren) -> SchemaSelectorResource {
    match children {
        SchemaChildren::Classes | SchemaChildren::DatasetProperties => {
            SchemaSelectorResource::Dataset
        }
        SchemaChildren::ClassProperties { class } => SchemaSelectorResource::Class {
            class: selector_term(class),
        },
        SchemaChildren::PropertyObjectClasses { class, predicate }
        | SchemaChildren::PropertyDatatypes { class, predicate } => {
            SchemaSelectorResource::Property {
                class: class.as_ref().map(selector_term),
                predicate: selector_term(predicate),
            }
        }
        SchemaChildren::DatatypeLanguages {
            class,
            predicate,
            datatype,
        } => SchemaSelectorResource::Datatype {
            class: class.as_ref().map(selector_term),
            predicate: selector_term(predicate),
            datatype: selector_term(datatype),
        },
    }
}

fn selector_term(bound: &BoundTerm) -> SchemaTerm {
    SchemaTerm(Rc::from(bound.dictionary()))
}

fn schema_view_name(view: &StatsView) -> String {
    match view {
        StatsView::Design => "design".to_owned(),
        StatsView::Queryable => "queryable".to_owned(),
        StatsView::Component(component) => format!("component:{}", component.as_str()),
    }
}

#[derive(Debug, Clone, Copy)]
enum SchemaLinkKind {
    Dataset,
    Class,
    Property,
    Datatype,
    Leaf,
}

fn view_params(view: &StatsView) -> Params {
    Params::default().with("view", &schema_view_name(view))
}

fn selected_node_links(
    selection: &SchemaSelection,
    view: &StatsView,
) -> BTreeMap<&'static str, String> {
    let (params, kind) = selection_params(selection, view);
    schema_links(&params, kind, false)
}

fn selected_child_parent_links(
    children: &SchemaChildren,
    view: &StatsView,
) -> BTreeMap<&'static str, String> {
    let (params, kind) = child_parent_params(children, view);
    // A child page describes its parent node as well as the collection. Keep a
    // route back to the cheap node-only representation beside the routes that
    // continue deeper.
    schema_links(&params, kind, true)
}

fn selection_params(selection: &SchemaSelection, view: &StatsView) -> (Params, SchemaLinkKind) {
    let params = view_params(view);
    match selection {
        SchemaSelection::Dataset => (params, SchemaLinkKind::Dataset),
        SchemaSelection::Class { class } => (
            params.with("class", class.requested()),
            SchemaLinkKind::Class,
        ),
        SchemaSelection::Property { class, predicate } => (
            with_optional_param(&params, "class", class.as_ref().map(BoundTerm::requested))
                .with("predicate", predicate.requested()),
            SchemaLinkKind::Property,
        ),
        SchemaSelection::Datatype {
            class,
            predicate,
            datatype,
        } => (
            with_optional_param(&params, "class", class.as_ref().map(BoundTerm::requested))
                .with("predicate", predicate.requested())
                .with("datatype", datatype.requested()),
            SchemaLinkKind::Datatype,
        ),
    }
}

fn child_parent_params(children: &SchemaChildren, view: &StatsView) -> (Params, SchemaLinkKind) {
    let params = view_params(view);
    match children {
        SchemaChildren::Classes | SchemaChildren::DatasetProperties => {
            (params, SchemaLinkKind::Dataset)
        }
        SchemaChildren::ClassProperties { class } => (
            params.with("class", class.requested()),
            SchemaLinkKind::Class,
        ),
        SchemaChildren::PropertyObjectClasses { class, predicate }
        | SchemaChildren::PropertyDatatypes { class, predicate } => (
            with_optional_param(&params, "class", class.as_ref().map(BoundTerm::requested))
                .with("predicate", predicate.requested()),
            SchemaLinkKind::Property,
        ),
        SchemaChildren::DatatypeLanguages {
            class,
            predicate,
            datatype,
        } => (
            with_optional_param(&params, "class", class.as_ref().map(BoundTerm::requested))
                .with("predicate", predicate.requested())
                .with("datatype", datatype.requested()),
            SchemaLinkKind::Datatype,
        ),
    }
}

fn child_links(
    children: &SchemaChildren,
    view: &StatsView,
    term: Option<&str>,
) -> BTreeMap<&'static str, String> {
    let Some(term) = term else {
        return BTreeMap::new();
    };
    let requested = Term::from_dictionary(term).to_request();
    let params = view_params(view);
    let (params, kind) = match children {
        SchemaChildren::Classes => (params.with("class", &requested), SchemaLinkKind::Class),
        SchemaChildren::DatasetProperties => (
            params.with("predicate", &requested),
            SchemaLinkKind::Property,
        ),
        SchemaChildren::ClassProperties { class } => (
            params
                .with("class", class.requested())
                .with("predicate", &requested),
            SchemaLinkKind::Property,
        ),
        SchemaChildren::PropertyObjectClasses { .. } | SchemaChildren::DatatypeLanguages { .. } => {
            (params, SchemaLinkKind::Leaf)
        }
        SchemaChildren::PropertyDatatypes { class, predicate } => (
            with_optional_param(&params, "class", class.as_ref().map(BoundTerm::requested))
                .with("predicate", predicate.requested())
                .with("datatype", &requested),
            SchemaLinkKind::Datatype,
        ),
    };
    schema_links(&params, kind, !matches!(kind, SchemaLinkKind::Leaf))
}

fn schema_links(
    params: &Params,
    kind: SchemaLinkKind,
    include_self: bool,
) -> BTreeMap<&'static str, String> {
    let mut links = BTreeMap::new();
    if include_self {
        links.insert("self", relative_schema_link(params));
    }
    match kind {
        SchemaLinkKind::Dataset => {
            links.insert(
                "classes",
                relative_schema_link(&params.with("children", "classes")),
            );
            links.insert(
                "properties",
                relative_schema_link(&params.with("children", "properties")),
            );
            links.insert(
                "class-relations",
                relative_schema_link(&params.with("projection", "class-relations")),
            );
            links.insert(
                "class-properties",
                relative_schema_link(&params.with("projection", "class-properties")),
            );
        }
        SchemaLinkKind::Class => {
            links.insert(
                "properties",
                relative_schema_link(&params.with("children", "properties")),
            );
        }
        SchemaLinkKind::Property => {
            links.insert(
                "object-classes",
                relative_schema_link(&params.with("children", "object-classes")),
            );
            links.insert(
                "datatypes",
                relative_schema_link(&params.with("children", "datatypes")),
            );
        }
        SchemaLinkKind::Datatype => {
            links.insert(
                "languages",
                relative_schema_link(&params.with("children", "languages")),
            );
        }
        SchemaLinkKind::Leaf => {}
    }
    links
}

fn relative_schema_link(params: &Params) -> String {
    format!("?{}", params.to_query())
}

fn with_optional_param(params: &Params, name: &str, value: Option<&str>) -> Params {
    value.map_or_else(|| params.clone(), |value| params.with(name, value))
}

/// `GET /tpf` — enumerate a TPF or bindings-restricted TPF pattern.
pub fn tpf(store: &Store, target: Target, request: &request::Tpf) -> Result<Answer, Problem> {
    if !target.is_tpf() {
        tracing::error!(
            operation = ?target.operation,
            "a typed TPF request was paired with a non-TPF response target"
        );
        return Err(Problem::new(
            ErrorCode::InternalError,
            "the TPF request was routed to the wrong response target",
        ));
    }
    match request {
        request::Tpf::Plain(request) => fragment(store, target, request),
        request::Tpf::Values(request) => binding_fragment(store, target, request),
    }
}

/// Enumerate an ordinary triple pattern.
pub fn fragment(
    store: &Store,
    target: Target,
    request: &request::Fragment,
) -> Result<Answer, Problem> {
    let dictionary = store.dict();
    let blank_nodes = SkolemScope::new(store.hdt_identity_digest(), *dictionary.counts());
    let echo = Echo::Fragment {
        pattern: request.pattern.clone(),
        g: request.graph.requested().map(str::to_owned),
    };
    let vars = Vars::new(request.pattern.vars(), request.graph.is_quad_view());

    let paging = Paging {
        cursor: request.cursor.as_ref(),
        limit: request.limit,
        bytes: request.bytes,
        binding: &request.binding,
    };
    let envelope = Envelope {
        echo,
        vars,
        directed: false,
        bindings: false,
        absent_terms: Vec::new(),
        blank_nodes,
        tagging: GraphTagging::for_scope(&request.graph),
    };

    match (
        resolve(&dictionary, &envelope.blank_nodes, &request.pattern)?,
        request.pattern.text(),
    ) {
        (Resolved::Absent(absent), _) => paged(
            store,
            target,
            Envelope {
                absent_terms: absent,
                ..envelope
            },
            Vec::new(),
            paging,
        ),
        (Resolved::Ids(ids), None) => {
            match scoped(store, &target, &envelope.blank_nodes, ids, &request.graph)? {
                Ok(enumeration) => paged(
                    store,
                    target,
                    envelope,
                    vec![phase(enumeration, None)?],
                    paging,
                ),
                Err(absent) => paged(
                    store,
                    target,
                    Envelope {
                        absent_terms: vec![absent],
                        ..envelope
                    },
                    Vec::new(),
                    paging,
                ),
            }
        }
        (Resolved::Ids(ids), Some(filter)) => {
            let searcher = searcher(store, &target)?;
            let found = ranked(
                store,
                searcher,
                filter,
                ids,
                paging.cursor,
                paging.want(),
                request.candidates,
            )?;
            ranked_page(store, target, envelope, found, paging)
        }
    }
}

/// The bundle's text index, or the 501 that says this one has none.
///
/// Reached only when a request carries `o.text`, and only after the handler has
/// checked the manifest declares `search` — so this is the second half of one
/// condition rather than a duplicate check: the manifest says what the bundle
/// promises, and this is the artifact that keeps the promise. A bundle where
/// they disagree is one that would otherwise panic here.
fn searcher<'a>(store: &'a Store, target: &Target) -> Result<&'a TextSearcher, Problem> {
    store.text().ok_or_else(|| {
        tracing::error!(
            dataset = %target.id.dataset,
            version = %target.id.version,
            "a bundle declaring `search` has no text index",
        );
        Problem::new(
            ErrorCode::CapabilityNotAvailable,
            "this bundle declares `search` but carries no text index",
        )
    })
}

/// `GET /count` — a pattern's cardinality.
pub fn count(
    store: &Store,
    target: Target,
    request: &request::Count,
) -> Result<CountAnswer, Problem> {
    let dictionary = store.dict();
    let blank_nodes = SkolemScope::new(store.hdt_identity_digest(), *dictionary.counts());
    if request.cursor.is_some() && request.pattern.text().is_none() {
        // Parsing enforces this too; keep the operation correct for callers
        // constructing the public request type directly.
        return Err(Problem::from(StaleCursor));
    }
    let (count, completeness, absent_terms) = match (
        resolve(&dictionary, &blank_nodes, &request.pattern)?,
        request.pattern.text(),
    ) {
        // Exact and free of the enumeration: a range width after bounded
        // descent for seven shapes, and for `s ? o` the same bounded
        // predicate-group probe the enumeration would run. Scoped to a
        // graph, two ranks over its layer; in the quad view, the memberships
        // of the range.
        (Resolved::Ids(ids), None) => {
            match scoped(store, &target, &blank_nodes, ids, &request.graph)? {
                Ok(enumeration) => (
                    Cardinality::exact(enumeration.count()?),
                    Completeness::complete(),
                    Vec::new(),
                ),
                Err(absent) => (
                    Cardinality::exact(0),
                    Completeness::complete(),
                    vec![absent],
                ),
            }
        }
        (Resolved::Absent(_), Some(_)) if request.cursor.is_some() => {
            return Err(Problem::from(StaleCursor));
        }
        (Resolved::Absent(absent), _) => (Cardinality::exact(0), Completeness::complete(), absent),
        (Resolved::Ids(ids), Some(filter)) => {
            let (count, completeness) = text_count(
                store,
                &target,
                filter,
                ids,
                request.candidates,
                request.cursor.as_ref(),
                &request.binding,
            )?;
            (count, completeness, Vec::new())
        }
    };
    Ok(CountAnswer {
        dataset: target.id.dataset.clone(),
        version: target.id.version.clone(),
        pattern: request.pattern.clone(),
        g: request.graph.requested().map(str::to_owned),
        count,
        absent_terms,
        completeness,
        target,
    })
}

/// `QUERY|POST /fragment` — enumerate one pattern for each input binding row.
pub fn binding_fragment(
    store: &Store,
    target: Target,
    request: &request::BindingFragment,
) -> Result<Answer, Problem> {
    let dictionary = store.dict();
    let blank_nodes = SkolemScope::new(store.hdt_identity_digest(), *dictionary.counts());
    let mut cache = LookupCache::new(dictionary, blank_nodes.clone());
    let mut restrictions = Vec::new();
    for row in request.rows() {
        let Some(ids) = resolve_binding(&mut cache, row)? else {
            continue;
        };
        restrictions.push((row.index(), ids));
    }
    if request.distinct_rdf() {
        normalize_rdf_restrictions(&mut restrictions);
    }

    let mut phases = Vec::with_capacity(restrictions.len());
    let mut restriction_counts = Vec::with_capacity(restrictions.len());
    let mut absent_graph = None;
    for (row_index, ids) in restrictions.iter().copied() {
        match scoped(store, &target, &blank_nodes, ids, &request.graph)? {
            Ok(enumeration) => {
                let phase = binding_phase(enumeration, row_index)?;
                restriction_counts.push((ids, phase.count));
                phases.push(phase);
            }
            // A graph this bundle does not hold empties every row at once.
            Err(absent) => {
                absent_graph = Some(absent);
                phases.clear();
                restriction_counts.clear();
                break;
            }
        }
    }

    let envelope = Envelope {
        echo: Echo::BindingsFragment {
            pattern: request.pattern.clone(),
            g: request.graph.requested().map(str::to_owned),
        },
        vars: Vars::new(request.pattern.vars(), request.graph.is_quad_view()),
        directed: false,
        bindings: true,
        absent_terms: absent_graph.into_iter().collect(),
        blank_nodes,
        tagging: GraphTagging::for_scope(&request.graph),
    };
    let paging = Paging {
        cursor: request.cursor.as_ref(),
        limit: request.limit,
        bytes: request.bytes,
        binding: &request.binding,
    };
    if !request.distinct_rdf() {
        return paged(store, target, envelope, phases, paging);
    }

    let base_pattern = resolve_binding_pattern(&mut cache, &request.pattern)?;
    let rdf_cardinality = rdf_projection_cardinality(store, base_pattern, &restriction_counts)?;
    let mut answer = paged_distinct_bindings(
        store,
        target,
        envelope,
        phases,
        &restrictions,
        request.candidates,
        paging,
    )?;
    answer.rdf_cardinality = Some(rdf_cardinality);
    Ok(answer)
}

/// `QUERY|POST /count` — one exact count for each input binding row.
pub fn binding_count(
    store: &Store,
    target: Target,
    request: &request::BindingCount,
) -> Result<BindingCountAnswer, Problem> {
    let dictionary = store.dict();
    let blank_nodes = SkolemScope::new(store.hdt_identity_digest(), *dictionary.counts());
    let mut cache = LookupCache::new(dictionary, blank_nodes.clone());
    let mut counts = Vec::new();
    for row in request.rows() {
        let value = match resolve_binding(&mut cache, row)? {
            Some(ids) => match scoped(store, &target, &blank_nodes, ids, &request.graph)? {
                Ok(enumeration) => enumeration.count()?,
                Err(_) => 0,
            },
            None => 0,
        };
        counts.push(PerBindingCount {
            binding: row.index(),
            count: Cardinality::exact(value),
        });
    }
    Ok(BindingCountAnswer {
        dataset: target.id.dataset.clone(),
        version: target.id.version.clone(),
        pattern: request.pattern.clone(),
        g: request.graph.requested().map(str::to_owned),
        counts,
        completeness: Completeness::complete(),
        target,
    })
}

/// `GET /graphs` — every graph with its membership count, paged by graph id.
///
/// One directory read per graph listed: a layer's count is a field of its
/// entry. The unnamed graph is listed first, under its reserved name, and
/// only when it holds a triple; the named graphs follow in the sidecar's
/// dictionary order. The cursor is the next id to list, so a page resumes by
/// arithmetic rather than by search.
pub fn graphs_list(
    store: &Store,
    target: Target,
    request: &request::GraphList,
) -> Result<GraphsAnswer, Problem> {
    let dictionary = store.dict();
    let blank_nodes = SkolemScope::new(store.hdt_identity_digest(), *dictionary.counts());
    let graphs = graphs(store, &target)?;
    let facts = graphs.facts();
    let unnamed_count = graphs
        .count(GraphId::UNNAMED)
        .map_err(|error| unreadable("reading the unnamed graph's count", &error))?;
    let listed = facts.named_graphs + u64::from(unnamed_count > 0);

    // The first id to list: after the cursor, or the unnamed graph — skipped
    // when empty, so an empty unnamed graph never appears under its name.
    let mut next = match &request.cursor {
        None => 0,
        Some(cursor) => {
            if cursor.position == 0 || cursor.position > facts.named_graphs {
                return Err(Problem::from(StaleCursor));
            }
            cursor.position
        }
    };
    if next == 0 && unnamed_count == 0 {
        next = 1;
    }

    let mut names = GraphNames::new(&blank_nodes);
    let mut rows = Vec::with_capacity(request.limit as usize);
    let mut spent = 0u64;
    let mut stop = None;
    while next <= facts.named_graphs {
        if rows.len() >= request.limit as usize {
            stop = Some(Completeness::page_limit(
                Cursor::at_graph(&request.binding, next).encode(),
            ));
            break;
        }
        let id = GraphId(next);
        let count = if id.is_unnamed() {
            unnamed_count
        } else {
            graphs
                .count(id)
                .map_err(|error| unreadable("reading a graph's count", &error))?
        };
        let (published, term) = names.measured(graphs, id)?;
        let row = GraphEntry::new(published, term, count);
        spent = spent.saturating_add(row.serialized);
        // Never on the first row, for the reason every page keeps its first
        // row: a page that carries nothing would resume where it was issued.
        if spent > request.bytes.0 && !rows.is_empty() {
            stop = Some(Completeness::budget_exhausted(
                BudgetReason::ResponseBytes,
                Cursor::at_graph(&request.binding, next).encode(),
            ));
            break;
        }
        rows.push(row);
        next += 1;
    }

    Ok(GraphsAnswer {
        dataset: target.id.dataset.clone(),
        version: target.id.version.clone(),
        triples: facts.triples,
        memberships: facts.memberships,
        cardinality: Cardinality::exact(listed),
        graphs: rows,
        completeness: stop.unwrap_or_else(Completeness::complete),
        target,
        blank_nodes,
    })
}

/// `GET /search` — rank matching literals, resolve their RDF occurrences, and
/// collapse those occurrences to one result per subject.
pub fn search(
    store: &Store,
    target: Target,
    request: &request::Search,
) -> Result<SearchAnswer, Problem> {
    let dictionary = store.dict();
    let blank_nodes = SkolemScope::new(store.hdt_identity_digest(), *dictionary.counts());
    let searcher = searcher(store, &target)?;
    let found = searcher
        .search_up_to(
            &request.query.to_query(),
            request.candidates.ceiling(),
            request.candidates.0,
        )
        .map_err(|error| unreadable("searching the text index", &error))?;

    // An omitted scope means every predicate. An explicit scope whose terms
    // are all absent means no occurrence can match, not that the scope should
    // silently widen to every predicate.
    let scoped = !request.predicates.is_empty();
    let mut predicate_ids = resolve_predicate_ids(&dictionary, &request.predicates)?;
    predicate_ids.sort_unstable();
    predicate_ids.dedup();
    let label_predicates = resolve_predicate_ids(&dictionary, &request.label_predicates)?;

    let mut results = Vec::with_capacity(request.limit as usize);
    let mut seen = HashSet::with_capacity(request.limit as usize);
    let mut cache = TermCache::new();
    let mut published = PublishedTerms::new(blank_nodes.clone());
    let mut resolution_budget = request.candidates.0;
    let mut spent_bytes = 0u64;
    let mut resolution_exhausted = false;
    let mut response_exhausted = false;

    'hits: for hit in &found.hits {
        if results.len() >= request.limit as usize {
            break;
        }
        if scoped && predicate_ids.is_empty() {
            break;
        }

        if !scoped {
            if resolution_budget == 0 {
                resolution_exhausted = true;
                break;
            }
            resolution_budget -= 1; // the OPS selection probe
            let selection = select(
                store,
                IdPattern {
                    subject: None,
                    predicate: None,
                    object: Some(hit.object_id),
                },
            )?;
            let available = selection.count().value;
            let take = available.min(resolution_budget);
            for triple in selection.page(0, take as usize) {
                resolution_budget -= 1;
                if push_search_result(
                    store,
                    &dictionary,
                    &mut cache,
                    &mut published,
                    &mut seen,
                    &mut results,
                    &mut spent_bytes,
                    request,
                    &label_predicates,
                    triple,
                    hit.object_id,
                    Ranking {
                        score: hit.score,
                        kind: match_kind(hit.kind),
                    },
                )? {
                    response_exhausted = true;
                    break 'hits;
                }
                if results.len() >= request.limit as usize {
                    break 'hits;
                }
            }
            if take < available {
                resolution_exhausted = true;
                break;
            }
        } else {
            for predicate in predicate_ids.iter().copied() {
                if resolution_budget == 0 {
                    resolution_exhausted = true;
                    break 'hits;
                }
                resolution_budget -= 1; // the predicate-bound selection probe
                let selection = select(
                    store,
                    IdPattern {
                        subject: None,
                        predicate: Some(predicate),
                        object: Some(hit.object_id),
                    },
                )?;
                let available = selection.count().value;
                let take = available.min(resolution_budget);
                for triple in selection.page(0, take as usize) {
                    resolution_budget -= 1;
                    if push_search_result(
                        store,
                        &dictionary,
                        &mut cache,
                        &mut published,
                        &mut seen,
                        &mut results,
                        &mut spent_bytes,
                        request,
                        &label_predicates,
                        triple,
                        hit.object_id,
                        Ranking {
                            score: hit.score,
                            kind: match_kind(hit.kind),
                        },
                    )? {
                        response_exhausted = true;
                        break 'hits;
                    }
                    if results.len() >= request.limit as usize {
                        break 'hits;
                    }
                }
                if take < available {
                    resolution_exhausted = true;
                    break 'hits;
                }
            }
        }
    }

    let completeness = if response_exhausted {
        Completeness::budget_exhausted_without_resume(BudgetReason::ResponseBytes)
    } else if resolution_exhausted || !found.complete {
        Completeness::budget_exhausted_without_resume(BudgetReason::Candidate)
    } else {
        Completeness::complete()
    };

    Ok(SearchAnswer {
        dataset: target.id.dataset.clone(),
        version: target.id.version.clone(),
        query: request.query.query().to_owned(),
        roles: request.roles.clone(),
        predicates: request
            .predicates
            .iter()
            .map(|predicate| predicate.dictionary().to_owned())
            .collect(),
        labels: request.labels,
        results,
        completeness,
        target,
        blank_nodes,
    })
}

#[allow(clippy::too_many_arguments)]
fn push_search_result(
    store: &Store,
    dictionary: &Dictionary<'_>,
    cache: &mut TermCache,
    published: &mut PublishedTerms,
    seen: &mut HashSet<u64>,
    results: &mut Vec<SearchResult>,
    spent_bytes: &mut u64,
    request: &request::Search,
    label_predicates: &[u64],
    triple: IdTriple,
    literal_id: u64,
    ranking: Ranking,
) -> Result<bool, Problem> {
    if !seen.insert(triple.subject) {
        return Ok(false);
    }

    // Published, not stored: a text hit can land on a blank-node subject, and a
    // result naming it `_:b1` would be a label no client could ask about and
    // one that collides with every other graph's.
    let (subject, subject_serialized) = published
        .measured(cache, dictionary, Role::Subject, TermId(triple.subject))
        .map(|(term, serialized)| (term.published, serialized))
        .map_err(|error| unreadable("materializing a search subject", &error))?;
    let predicate = cache
        .resolve(dictionary, Role::Predicate, TermId(triple.predicate))
        .map_err(|error| unreadable("materializing a search predicate", &error))?;
    let literal = cache
        .resolve(dictionary, Role::Object, TermId(literal_id))
        .map_err(|error| unreadable("materializing a search literal", &error))?;
    if !matches!(Term::from_dictionary(&literal), Term::Literal(_)) {
        return Err(unreadable(
            "resolving a text hit",
            &format_args!("object term {literal_id} is not a literal"),
        ));
    }
    let label = if request.labels {
        Some(preferred_label(
            store,
            dictionary,
            cache,
            triple.subject,
            label_predicates,
        )?)
    } else {
        None
    };
    let result = SearchResult::new(
        subject,
        subject_serialized,
        label,
        predicate,
        literal,
        ranking,
    );
    let next = spent_bytes.saturating_add(result.serialized);
    // As for the ordinary page materializer, always allow one row through: an
    // oversized legal term must not create an empty response that cannot make
    // progress.
    if next > request.bytes.0 && !results.is_empty() {
        seen.remove(&triple.subject);
        return Ok(true);
    }
    *spent_bytes = next;
    results.push(result);
    Ok(false)
}

/// `QUERY|POST /labels` — preserve the submitted IRI order and return one
/// preferred label or an explicit null for each processed member.
pub fn labels(
    store: &Store,
    target: Target,
    request: &request::Labels,
) -> Result<LabelsAnswer, Problem> {
    let dictionary = store.dict();
    let blank_nodes = SkolemScope::new(store.hdt_identity_digest(), *dictionary.counts());
    let label_predicates = resolve_predicate_ids(&dictionary, &request.label_predicates)?;
    let mut cache = TermCache::new();
    let mut resolved_labels: HashMap<String, Option<String>> = HashMap::new();
    let mut labels = Vec::with_capacity(request.iris().len());
    let mut spent = 0u64;
    let mut exhausted = false;

    for requested in request.iris() {
        let label = if let Some(label) = resolved_labels.get(requested.dictionary()) {
            label.clone()
        } else {
            let label = match locate_scoped(&dictionary, &blank_nodes, Role::Subject, requested)? {
                Some(subject) => {
                    preferred_label(store, &dictionary, &mut cache, subject, &label_predicates)?
                }
                None => None,
            };
            resolved_labels.insert(requested.dictionary().to_owned(), label.clone());
            label
        };
        let result = LabelResult::new(requested.dictionary().to_owned(), label);
        let next = spent.saturating_add(result.serialized);
        if next > request.bytes.0 && !labels.is_empty() {
            exhausted = true;
            break;
        }
        spent = next;
        labels.push(result);
    }

    Ok(LabelsAnswer {
        dataset: target.id.dataset.clone(),
        version: target.id.version.clone(),
        labels,
        completeness: if exhausted {
            Completeness::budget_exhausted_without_resume(BudgetReason::ResponseBytes)
        } else {
            Completeness::complete()
        },
        target,
    })
}

fn resolve_predicate_ids(
    dictionary: &Dictionary<'_>,
    predicates: &[BoundTerm],
) -> Result<Vec<u64>, Problem> {
    predicates
        .iter()
        .map(|predicate| locate(dictionary, Role::Predicate, predicate))
        .filter_map(|result| result.transpose())
        .collect()
}

/// Answer a dictionary prefix scan: a page of terms, or how many there are.
///
/// The dictionary is already sorted, so this needs no artifact a bundle does not
/// have to carry — which is why every release answers it. What it costs is two
/// binary searches to bracket the prefix and then the page.
pub fn terms(
    store: &Store,
    target: Target,
    request: &request::Terms,
) -> Result<TermsAnswer, Problem> {
    let dictionary = store.dict();

    if request.count {
        let counts = dictionary
            .term_counts(request.prefix.as_bytes())
            .map_err(|error| unreadable("counting a dictionary prefix", &error))?;
        return Ok(TermsAnswer::Count(TermsCount {
            dataset: target.id.dataset.clone(),
            version: target.id.version.clone(),
            prefix: request.prefix.clone(),
            role: role_name(request.role),
            count: Cardinality::exact(counts.of(request.role)),
            counts: counts.into(),
            completeness: Completeness::complete(),
            target,
        }));
    }

    // Below the count, which brackets its own four sections: building this for a
    // request that returns one number would pay for the whole scan twice.
    let scan = dictionary
        .terms(request.role, request.prefix.as_bytes())
        .map_err(|error| unreadable("bracketing a dictionary prefix", &error))?;
    let cardinality = scan
        .count()
        .map_err(|error| unreadable("counting a dictionary prefix", &error))?;

    let after = match &request.cursor {
        None => None,
        Some(cursor) => {
            if cursor.space != PositionSpace::DictionaryPrefix {
                return Err(Problem::from(StaleCursor));
            }
            // The position must name a term *this* scan enumerates. A token for
            // another prefix or another role decodes and then fails here, which
            // is the same refusal as a token for another bundle.
            Some(
                scan.resume_at(DictPosition::new(cursor.position))
                    .ok_or(StaleCursor)?,
            )
        }
    };

    let blank_nodes = SkolemScope::new(store.hdt_identity_digest(), *dictionary.counts());
    let label_predicates = resolve_predicate_ids(&dictionary, &request.label_predicates)?;
    let mut cache = TermCache::new();
    let mut published = PublishedTerms::new(blank_nodes.clone());
    let mut rows: Vec<TermRow> = Vec::with_capacity(request.limit as usize);
    let mut spent = 0u64;
    let mut spent_budget = false;
    let mut failure = None;

    let stop = scan
        .page(after, request.limit as usize, |term| {
            let row = match scanned_row(
                store,
                &dictionary,
                &mut cache,
                &mut published,
                &term,
                request.labels.then_some(label_predicates.as_slice()),
            ) {
                Ok(row) => row,
                Err(problem) => {
                    failure = Some(problem);
                    return ScanFlow::Reject;
                }
            };
            spent = spent.saturating_add(row.serialized);
            // Never on the first row, for the reason a fragment page keeps its
            // first row: a page that carries nothing would resume exactly where
            // it was issued, and a client paging on it would never move.
            if spent > request.bytes.0 && !rows.is_empty() {
                spent_budget = true;
                return ScanFlow::Reject;
            }
            rows.push(row);
            ScanFlow::Continue
        })
        .map_err(|error| unreadable("paging a dictionary prefix", &error))?;
    if let Some(problem) = failure {
        return Err(problem);
    }

    let completeness = match stop.last.filter(|_| stop.more) {
        None if stop.more => {
            // Unreachable: `limit` is at least one, the byte budget always
            // admits the first row, and a failed row has already returned.
            // Reported rather than asserted, because a page that kept nothing
            // and cannot say where to resume is not a page to serve.
            return Err(unreadable(
                "paging a dictionary prefix",
                &"a page that left terms behind kept none of them",
            ));
        }
        None => Completeness::complete(),
        Some(position) => {
            let token = Cursor::at_dictionary_position(&request.binding, position).encode();
            if spent_budget {
                Completeness::budget_exhausted(BudgetReason::ResponseBytes, token)
            } else {
                Completeness::page_limit(token)
            }
        }
    };

    Ok(TermsAnswer::Page(TermsPage {
        dataset: target.id.dataset.clone(),
        version: target.id.version.clone(),
        prefix: request.prefix.clone(),
        role: role_name(request.role),
        cardinality: Cardinality::exact(cardinality),
        terms: rows,
        completeness,
        target,
        blank_nodes,
    }))
}

/// One scanned term as a response row: its published spelling, the positions it
/// occupies, and its preferred label when one was asked for.
fn scanned_row(
    store: &Store,
    dictionary: &Dictionary<'_>,
    cache: &mut TermCache,
    published: &mut PublishedTerms,
    term: &ScannedTerm<'_>,
    label_predicates: Option<&[u64]>,
) -> Result<TermRow, Problem> {
    // Any role the scan read spells the term the same way, because a published
    // blank node is named by its section and local id rather than by a role. So
    // the first one is as good as any, and there is always one.
    let (role, id) = term
        .sections()
        .roles()
        .find_map(|role| term.id(role).map(|id| (role, id)))
        .ok_or_else(|| {
            unreadable(
                "materializing a scanned term",
                &"a scanned term has no id in any role its scan read",
            )
        })?;
    let (row_term, serialized) = published
        .measured(cache, dictionary, role, id)
        .map_err(|error| unreadable("materializing a scanned term", &error))?;

    let label = match label_predicates {
        None => None,
        Some(predicates) => {
            // A label statement has the term as its subject, so a term the scan
            // did not read in the subject sections still needs looking up there:
            // a predicate carries `rdfs:label` like anything else, and refusing
            // to look would make `role=predicate&labels=true` answer nothing.
            let subject = match term.id(Role::Subject) {
                Some(id) => Some(id.0),
                None => dictionary
                    .locate(Role::Subject, term.bytes())
                    .map_err(|error| unreadable("looking a scanned term up", &error))?
                    .map(|id| id.0),
            };
            Some(match subject {
                None => None,
                Some(subject) => preferred_label(store, dictionary, cache, subject, predicates)?,
            })
        }
    };

    let roles = term.sections().roles().map(term_role_name).collect();
    Ok(TermRow::new(row_term.published, serialized, roles, label))
}

/// First predicate in the frozen cascade with a value, then its lowest object
/// term id. There is intentionally no language axis: this is the release's one
/// deterministic display label, independent of client locale.
fn preferred_label(
    store: &Store,
    dictionary: &Dictionary<'_>,
    cache: &mut TermCache,
    subject: u64,
    predicates: &[u64],
) -> Result<Option<String>, Problem> {
    for predicate in predicates {
        let selection = select(
            store,
            IdPattern {
                subject: Some(subject),
                predicate: Some(*predicate),
                object: None,
            },
        )?;
        let Some(triple) = selection.page(0, 1).next() else {
            continue;
        };
        let text = cache
            .resolve(dictionary, Role::Object, TermId(triple.object))
            .map_err(|error| unreadable("materializing a preferred label", &error))?;
        return match Term::from_dictionary(&text) {
            Term::Literal(literal) => Ok(Some(literal.value().to_owned())),
            _ => {
                tracing::error!(
                    subject,
                    predicate,
                    object = triple.object,
                    "a declared label predicate has a non-literal value"
                );
                Err(Problem::new(
                    ErrorCode::InternalError,
                    "the bundle's label profile points to a non-literal value",
                ))
            }
        };
    }
    Ok(None)
}

/// Count statements matching a text-constrained pattern, in resumable batches.
///
/// hdtc scans matching object IDs without ranking. Each ID is intersected with
/// the remaining subject/predicate constraints through the ordinary store
/// selection, so the accumulated value counts statements rather than global
/// text hits. The cursor carries both the hdtc scan position and that value.
fn text_count(
    store: &Store,
    target: &Target,
    filter: &TextFilter,
    ids: IdPattern,
    budget: Candidates,
    cursor: Option<&Cursor>,
    binding: &CursorBinding,
) -> Result<(Cardinality, Completeness), Problem> {
    let (from, mut accumulated) = match cursor {
        None => (None, 0u64),
        Some(cursor)
            if cursor.space == PositionSpace::TextScan
                && cursor.binding_index.is_none()
                && cursor.scan_position.is_some() =>
        {
            (
                Some(TextScanPosition::decode(cursor.position)),
                cursor.scan_position.expect("checked above"),
            )
        }
        Some(_) => return Err(Problem::from(StaleCursor)),
    };

    let page = searcher(store, target)?
        .scan_matching_objects(&filter.to_query(), from, budget.ceiling())
        .map_err(|error| {
            if cursor.is_some() {
                Problem::from(StaleCursor)
            } else {
                unreadable("scanning text matches", &error)
            }
        })?;

    for object in page.object_ids {
        let selection = select(
            store,
            IdPattern {
                object: Some(object),
                ..ids
            },
        )?;
        accumulated = accumulated
            .checked_add(selection.count().value)
            .ok_or_else(|| unreadable("counting text matches", &"statement count overflow"))?;
    }

    if page.complete {
        return Ok((Cardinality::exact(accumulated), Completeness::complete()));
    }

    let next = page
        .next
        .expect("an incomplete hdtc scan carries a continuation");
    Ok((
        Cardinality::estimated(accumulated).at_least(accumulated),
        Completeness::budget_exhausted(
            BudgetReason::Candidate,
            Cursor::at_text_scan(binding, next.encode(), accumulated).encode(),
        ),
    ))
}

/// `GET /describe` — a resource's neighborhood.
///
/// Two enumerations behind one envelope, out-edges first, and a row says which
/// it came from. The column earns its place on the one triple the two halves
/// share: `<a> p <a>` is genuinely an
/// out-edge *and* an in-edge, so it appears twice, and without the column the
/// second copy reads as a duplicate rather than as the other half of the
/// answer. Deduplicating instead would cost an `s ? o` probe per request and
/// exceed the operation's two-fragment cost bound just to keep `cardinality`
/// equal to the enumerated length.
pub fn describe(
    store: &Store,
    target: Target,
    request: &request::Describe,
) -> Result<Answer, Problem> {
    let dictionary = store.dict();
    let blank_nodes = SkolemScope::new(store.hdt_identity_digest(), *dictionary.counts());
    let echo = Echo::Describe {
        resource: request.resource.requested().to_owned(),
        direction: request.direction,
    };

    let mut phases = Vec::new();
    if request.direction.walks_out()
        && let Some(subject) =
            locate_scoped(&dictionary, &blank_nodes, Role::Subject, &request.resource)?
    {
        let selection = select(
            store,
            IdPattern {
                subject: Some(subject),
                predicate: None,
                object: None,
            },
        )?;
        phases.push(phase(
            Enumeration::Triples(selection),
            Some(Direction::Out),
        )?);
    }
    if request.direction.walks_in()
        && let Some(object) =
            locate_scoped(&dictionary, &blank_nodes, Role::Object, &request.resource)?
    {
        let selection = select(
            store,
            IdPattern {
                subject: None,
                predicate: None,
                object: Some(object),
            },
        )?;
        phases.push(phase(Enumeration::Triples(selection), Some(Direction::In))?);
    }
    // Absent in the sense that matters for *this* request: the bundle holds no
    // term that could match it in any of the roles the direction walks.
    let absent_terms = if phases.is_empty() {
        vec![AbsentTerm::new("iri", &request.resource)]
    } else {
        Vec::new()
    };

    let mut answer = paged(
        store,
        target,
        Envelope {
            echo,
            // Every row carries all three, because for `direction=both` there
            // is no single bound position — and a row shape that changed with
            // `direction` would make the wrapper harder to consume than the
            // `/fragment` it wraps.
            vars: Vars::new(Position::ALL.to_vec(), false),
            directed: true,
            bindings: false,
            absent_terms,
            blank_nodes,
            tagging: GraphTagging::Untagged,
        },
        phases,
        Paging {
            cursor: request.cursor.as_ref(),
            limit: request.limit,
            bytes: request.bytes,
            binding: &request.binding,
        },
    )?;
    answer.described = Some(request.resource.dictionary().to_owned());
    Ok(answer)
}

/// `GET /sample` — pseudo-random members of a pattern's results.
pub fn sample(store: &Store, target: Target, request: &request::Sample) -> Result<Answer, Problem> {
    let dictionary = store.dict();
    let blank_nodes = SkolemScope::new(store.hdt_identity_digest(), *dictionary.counts());
    let echo = Echo::Sample {
        pattern: request.pattern.clone(),
        n: request.n,
        seed: request.seed,
    };
    let vars = Vars::new(request.pattern.vars(), false);

    let (count, triples, absent_terms) = match resolve(&dictionary, &blank_nodes, &request.pattern)?
    {
        Resolved::Absent(absent) => (0, Vec::new(), absent),
        Resolved::Ids(ids) => {
            let (count, drawn) = draw(&select(store, ids)?, u64::from(request.n), request.seed);
            (count, drawn, Vec::new())
        }
    };

    let steps: Vec<Step> = triples
        .into_iter()
        .map(|triple| Step {
            triple,
            graph: None,
            // A sample never pages, so nothing reads these.
            space: PositionSpace::Spo,
            resume: 0,
            scan: None,
            binding_index: None,
            direction: None,
            ranking: None,
        })
        .collect();

    let (rows, spent_at) = materialize(
        &dictionary,
        &blank_nodes,
        None,
        &vars,
        &steps,
        request.bytes,
    )?;
    Ok(Answer {
        dataset: target.id.dataset.clone(),
        version: target.id.version.clone(),
        echo,
        // The size of the set drawn *from*, which is what makes the sample
        // interpretable — 25 of 1 284 211 is a different statement from 25 of
        // 25.
        cardinality: Cardinality::exact(count),
        absent_terms,
        rows,
        row_resumes: Vec::new(),
        row_binding: None,
        rdf_cardinality: None,
        page_limit: request.n,
        byte_budget: request.bytes.0,
        vars,
        // A sample stops for one reason only. It is not paged, so `n` is what
        // it returns unless the bundle's own literals spend the byte budget
        // first — and then it says so, because there is no cursor to offer and
        // returning fewer members while claiming completeness is the silent
        // truncation the protocol prohibits.
        completeness: match spent_at {
            None => Completeness::complete(),
            Some(_) => Completeness::budget_exhausted_without_resume(BudgetReason::ResponseBytes),
        },
        directed: false,
        bindings: false,
        target,
        blank_nodes,
        page_labels: HashMap::new(),
        described: None,
        tagging: GraphTagging::Untagged,
    })
}

// ---------------------------------------------------------------------------
// Resolution and paging
// ---------------------------------------------------------------------------

/// One enumeration a paged operation walks.
struct Phase<'a> {
    enumeration: Enumeration<'a>,
    space: PositionSpace,
    /// The cardinality this phase contributes: triples, or memberships in the
    /// quad view.
    count: u64,
    /// The triples this phase enumerates, which is what a resume offset is
    /// checked against — one triple carries several quad-view rows.
    triples: u64,
    binding_index: Option<u32>,
    direction: Option<Direction>,
}

/// What a phase enumerates: the union, one graph, or every membership.
///
/// All three read the pattern's own permutation and share its cursor
/// positions; the quad view adds one row per graph a triple belongs to and a
/// second number, the memberships of the current triple already delivered,
/// to resume inside that run.
enum Enumeration<'a> {
    Triples(Selection<'a>),
    Scoped(ScopedSelection<'a>),
    Quads(QuadSelection<'a>),
}

/// One row of an enumeration, before its resume position is assigned.
struct EnumeratedRow {
    triple: IdTriple,
    /// The graph, in the quad view.
    graph: Option<GraphId>,
    /// The quad view's index of this row among its triple's memberships.
    delivered: Option<u64>,
}

impl<'a> Enumeration<'a> {
    fn space(&self) -> PositionSpace {
        match self {
            Self::Triples(selection) => PositionSpace::of(selection),
            Self::Scoped(scoped) => PositionSpace::of_parts(
                scoped.permutation(),
                scoped.subject_object_route().is_some(),
            ),
            Self::Quads(quads) => {
                PositionSpace::of_parts(quads.permutation(), quads.subject_object_route().is_some())
            }
        }
    }

    fn count(&self) -> Result<u64, Problem> {
        match self {
            Self::Triples(selection) => Ok(selection.count().value),
            Self::Scoped(scoped) => scoped
                .count()
                .map_err(|error| unreadable("counting a graph's triples", &error)),
            Self::Quads(quads) => quads
                .count()
                .map_err(|error| unreadable("counting memberships", &error)),
        }
    }

    fn triples(&self) -> Result<u64, Problem> {
        match self {
            Self::Quads(quads) => Ok(quads.triples()),
            _ => self.count(),
        }
    }

    fn is_quad_view(&self) -> bool {
        matches!(self, Self::Quads(_))
    }

    /// Rows from `from` — an offset or the last predicate id — skipping the
    /// first `skip` memberships of the first triple in the quad view.
    fn rows(
        &'a self,
        from: u64,
        skip: u64,
    ) -> Box<dyn Iterator<Item = Result<EnumeratedRow, Problem>> + 'a> {
        match self {
            Self::Triples(selection) => Box::new(selection.page(from, usize::MAX).map(|triple| {
                Ok(EnumeratedRow {
                    triple,
                    graph: None,
                    delivered: None,
                })
            })),
            Self::Scoped(scoped) => Box::new(scoped.page(from, usize::MAX).map(|row| {
                row.map(|triple| EnumeratedRow {
                    triple,
                    graph: None,
                    delivered: None,
                })
                .map_err(|error| unreadable("enumerating a graph's triples", &error))
            })),
            Self::Quads(quads) => Box::new(quads.page(from, skip, usize::MAX).map(|row| {
                row.map(|row| EnumeratedRow {
                    triple: row.triple,
                    graph: Some(row.graph),
                    delivered: Some(row.delivered),
                })
                .map_err(|error| unreadable("enumerating memberships", &error))
            })),
        }
    }
}

fn phase(enumeration: Enumeration<'_>, direction: Option<Direction>) -> Result<Phase<'_>, Problem> {
    Ok(Phase {
        space: enumeration.space(),
        count: enumeration.count()?,
        triples: enumeration.triples()?,
        enumeration,
        binding_index: None,
        direction,
    })
}

fn binding_phase(enumeration: Enumeration<'_>, binding_index: u32) -> Result<Phase<'_>, Problem> {
    Ok(Phase {
        binding_index: Some(binding_index),
        ..phase(enumeration, None)?
    })
}

/// The graph memberships, or the 501 that says this bundle has none.
///
/// Reached only for a scope that needs the sidecar, and only after the
/// handler has checked the manifest declares `graphs` — the second half of one
/// condition, as for the text index.
fn graphs<'a>(store: &'a Store, target: &Target) -> Result<&'a Graphs, Problem> {
    store.graphs().ok_or_else(|| {
        tracing::error!(
            dataset = %target.id.dataset,
            version = %target.id.version,
            "a bundle declaring `graphs` has no membership sidecar",
        );
        Problem::new(
            ErrorCode::CapabilityNotAvailable,
            "this bundle declares `graphs` but carries no membership sidecar",
        )
    })
}

/// Resolve a pattern under a graph scope.
///
/// `Err(absent)` is the well-formed request that names a graph this bundle
/// does not hold — an empty answer that says which parameter, exactly as an
/// absent term is reported.
fn scoped<'a>(
    store: &'a Store,
    target: &Target,
    blank_nodes: &SkolemScope,
    ids: IdPattern,
    scope: &GraphScope,
) -> Result<Result<Enumeration<'a>, AbsentTerm>, Problem> {
    let selection = select(store, ids)?;
    Ok(Ok(match scope.selector() {
        GraphSelector::Union => Enumeration::Triples(selection),
        // Without memberships every triple is unnamed, so the unnamed graph is
        // the union — the same rows, under the name the client used.
        GraphSelector::Unnamed => match store.graphs() {
            None => Enumeration::Triples(selection),
            Some(graphs) => in_layer(selection, graphs, GraphId::UNNAMED)?,
        },
        GraphSelector::Named(term) => {
            let graphs = graphs(store, target)?;
            let found = match blank_nodes.graph_id(term.dictionary()) {
                Some(id) => (id.0 <= graphs.facts().named_graphs).then_some(id),
                None if term.denotes_blank_node() => None,
                None => graphs
                    .resolve(term.dictionary().as_bytes())
                    .map_err(|error| unreadable("looking a graph up", &error))?,
            };
            match found {
                Some(graph) => in_layer(selection, graphs, graph)?,
                None => return Ok(Err(AbsentTerm::new("g", term))),
            }
        }
        GraphSelector::All => {
            let graphs = graphs(store, target)?;
            selection
                .memberships(graphs)
                .map(Enumeration::Quads)
                .map_err(|error| unreadable("preparing the quad view", &error))?
        }
    }))
}

fn in_layer<'a>(
    selection: Selection<'a>,
    graphs: &'a Graphs,
    graph: GraphId,
) -> Result<Enumeration<'a>, Problem> {
    selection
        .in_graph(graphs, graph)
        .map(Enumeration::Scoped)
        .map_err(|error| unreadable("opening a graph's layer", &error))
}

/// One row of an enumeration, with the position that resumes *at* it.
///
/// Before rather than after, which is what makes the extra row do both jobs:
/// the row a page drops is the first row of the next page, so its own resume
/// point is the cursor — including when it is the first row of `/describe`'s
/// second enumeration, where the position after the previous row is the end of
/// the first one and would be refused as out of range.
struct Step {
    triple: IdTriple,
    /// The graph this row's membership is in, in the quad view.
    graph: Option<GraphId>,
    space: PositionSpace,
    resume: u64,
    /// The second number of a position that needs two: for
    /// [`PositionSpace::TextRank`], how many of this hit's statements come
    /// before this row; in the quad view, how many of this triple's
    /// memberships come before this row. `None` in every space whose position
    /// is a single number.
    scan: Option<u64>,
    binding_index: Option<u32>,
    direction: Option<Direction>,
    ranking: Option<Ranking>,
}

/// The result-order position needed if RDF byte fitting omits this row.
///
/// It deliberately excludes the request binding: one binding is shared by the
/// page, while these small values vary per row. Keeping positions rather than
/// encoded tokens removes base64 allocation from JSON and HTML responses.
#[derive(Debug, Clone, Copy)]
struct RowResume {
    space: PositionSpace,
    position: u64,
    scan: Option<u64>,
    binding_index: Option<u32>,
}

impl RowResume {
    fn cursor(self, binding: &CursorBinding) -> CursorToken {
        let mut cursor = Cursor::at(binding, self.space, self.position);
        cursor.binding_index = self.binding_index;
        // The trailer means "how far into this position's run" in every space
        // that has runs: a ranked hit's statements, or a triple's memberships
        // in the quad view. A space without runs never sets it.
        cursor.scan_position = self.scan;
        cursor.encode()
    }
}

impl Step {
    fn row_resume(&self) -> RowResume {
        RowResume {
            space: self.space,
            position: self.resume,
            scan: self.scan,
            binding_index: self.binding_index,
        }
    }

    /// The token that resumes a page at this row.
    fn cursor(&self, binding: &CursorBinding) -> CursorToken {
        // On the space rather than on `scan.is_some()`. The two agree today,
        // and `Cursor::at_rank` hardcodes `TextRank` — so dispatching on the
        // trailer would silently mint a ranked token for the first M2 position
        // that wants a second number in some other space, which
        // `Cursor::scan_position` is already reserved for.
        self.row_resume().cursor(binding)
    }
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// How many distinct literals a text query matched, and whether that is the
/// whole number.
///
/// Two fields rather than one, because a count taken under a budget is not the
/// same claim as a count taken to the end: `distinct_objects` is exact, so a
/// figure that stopped at the budget has to be
/// reported as a lower bound instead of quietly standing in for one.
#[derive(Debug, Clone, Copy)]
struct MatchingLiterals {
    counted: u64,
    exact: bool,
}

/// What one text-filtered page found, and why it stopped looking.
struct Ranked {
    steps: Vec<Step>,
    /// Distinct literals matching the text, and whether the count is complete.
    matching_literals: MatchingLiterals,
    /// Whether no subject or predicate constraint narrows those literals.
    unfiltered: bool,
    /// How the candidates ran out, when they ran out before the page filled.
    spent: Option<Spent>,
}

/// What a filtered operation does when it stops for candidates rather than
/// rows.
enum Spent {
    /// As deep into the ranking as this server pages. No cursor: there is
    /// nowhere further to go, and offering one would be a loop.
    Deepest,
    /// Duplicate candidates exhausted a bounded RDF-union walk. This is the
    /// first unexamined candidate and therefore its continuation.
    Candidate(Step),
}

/// Enumerate a pattern whose object is constrained by a text query.
///
/// A text hit is an object dictionary id, so each one becomes
/// `IdPattern { .., object: Some(id) }` and resolves
/// through permutations this store already holds. There is no text-specific
/// enumeration — only a different way of choosing which objects to enumerate,
/// and a different order to do it in.
///
/// # What bounds it
///
/// Filtered operations are budgeted on *candidates examined*, independently of
/// `limit`, because a hit need not contribute a row: `? p ?` with a text
/// constraint discards every matching literal that does not occur with `p`.
/// The index therefore scores at most `candidate_budget` documents and retains
/// at most that many hits. Row cursors page within that deterministic window.
/// If scoring stopped first, the window ends with `candidate_budget` and no
/// cursor beyond it: resuming a global relevance ranking would require
/// rescoring and retaining an ever-growing prefix, which is unbounded work.
fn ranked(
    store: &Store,
    searcher: &TextSearcher,
    filter: &TextFilter,
    ids: IdPattern,
    cursor: Option<&Cursor>,
    want: usize,
    budget: Candidates,
) -> Result<Ranked, Problem> {
    // Before any traversal: neither check needs the index, and a token replayed
    // from another request should not first cost a walk of every posting for
    // its query term.
    let (from_rank, mut skip) = match cursor {
        None => (0u64, 0u64),
        Some(cursor) => {
            if cursor.space != PositionSpace::TextRank
                || cursor.binding_index.is_some()
                || cursor.scan_position.is_none()
            {
                return Err(Problem::from(StaleCursor));
            }
            (
                cursor.position,
                cursor.scan_position.expect("checked above"),
            )
        }
    };

    let query = filter.to_query();
    if from_rank >= budget.0 {
        return Err(Problem::from(StaleCursor));
    }

    // hdtc independently bounds the score work and retained heap. Keeping the
    // complete candidate window lets a selective subject/predicate constraint
    // walk past text hits that contribute no rows without pretending `limit`
    // bounded that work.
    let found = searcher
        .search_up_to(&query, budget.ceiling(), budget.0)
        .map_err(|error| unreadable("searching the text index", &error))?;
    let hits = &found.hits;

    // A rank past the end of the hit list was never issued: a page that reached
    // the end says so rather than handing out a cursor to nothing. The fetch
    // asked for more hits than this rank, so a short list is the end — and only
    // a *resumed* page has a rank to be past it, since a query that matches
    // nothing is an empty answer rather than a bad token.
    if cursor.is_some() && (hits.len() as u64) <= from_rank {
        return Err(Problem::from(StaleCursor));
    }

    let mut steps = Vec::with_capacity(want.min(hits.len()));
    for (rank, hit) in hits.iter().enumerate().skip(from_rank as usize) {
        if steps.len() >= want {
            break;
        }
        let selection = select(
            store,
            IdPattern {
                object: Some(hit.object_id),
                ..ids
            },
        )?;
        // The position *inside* a hit is a position in that hit's own space,
        // not a row count: `s ? ?` with a text constraint resolves to `s ? o`,
        // whose positions are predicate ids. Reusing the same
        // pairing the pattern walk uses is what keeps the two readings from
        // drifting — and `s ? ?` + `o.text` is the only shape where they
        // differ, which is exactly the shape a special case would get wrong.
        let space = PositionSpace::of(&selection);
        // `skip` applies to the hit the cursor stopped inside, and no other.
        let within = std::mem::take(&mut skip);
        if cursor.is_some()
            && rank as u64 == from_rank
            && selection.page(within, 1).next().is_none()
        {
            return Err(Problem::from(StaleCursor));
        }
        let enumeration = Enumeration::Triples(selection);
        for row in positioned(&enumeration, space, within, 0).take(want - steps.len()) {
            let row = row?;
            steps.push(Step {
                triple: row.triple,
                graph: None,
                space: PositionSpace::TextRank,
                resume: rank as u64,
                scan: Some(row.resume),
                binding_index: None,
                direction: None,
                ranking: Some(Ranking {
                    score: hit.score,
                    kind: match_kind(hit.kind),
                }),
            });
        }
    }

    let spent = (steps.len() < want && !found.complete).then_some(Spent::Deepest);

    Ok(Ranked {
        steps,
        matching_literals: MatchingLiterals {
            counted: found.examined,
            exact: found.complete,
        },
        unfiltered: ids.subject.is_none() && ids.predicate.is_none(),
        spent,
    })
}

/// A pattern's ids, or the parameters whose terms the bundle does not hold.
enum Resolved {
    Ids(IdPattern),
    Absent(Vec<AbsentTerm>),
}

fn resolve(
    dictionary: &Dictionary<'_>,
    blank_nodes: &SkolemScope,
    pattern: &Pattern,
) -> Result<Resolved, Problem> {
    let mut ids = IdPattern {
        subject: None,
        predicate: None,
        object: None,
    };
    let mut absent = Vec::new();
    for position in Position::ALL {
        let Some(term) = pattern.bound(position) else {
            continue;
        };
        match locate_scoped(dictionary, blank_nodes, position.role(), term)? {
            Some(id) => match position {
                Position::Subject => ids.subject = Some(id),
                Position::Predicate => ids.predicate = Some(id),
                Position::Object => ids.object = Some(id),
            },
            // Not an error: the term is well formed and simply not in this
            // bundle, so the answer is provably empty rather than unanswerable.
            None => absent.push(AbsentTerm::new(position.as_str(), term)),
        }
    }
    if absent.is_empty() {
        Ok(Resolved::Ids(ids))
    } else {
        Ok(Resolved::Absent(absent))
    }
}

/// Resolve a request term, or `None` when this bundle does not hold it.
///
/// Blank-node syntax never resolves, and is never even probed for. A stored
/// `_:` label belongs to whichever document was loaded, so the same label names
/// unrelated nodes at different bundles; a lookup that succeeded would join
/// across knowledge graphs on a coincidence of spelling. Blank nodes are
/// addressed by the bundle-scoped IRI [`SkolemScope`] mints, whose digest is
/// what makes the identity refuse to travel. The term is still accepted and
/// reported as absent rather than rejected, so a client submitting a mixed
/// batch of IRIs and blank nodes gets one answer instead of a spoiled request.
fn locate(
    dictionary: &Dictionary<'_>,
    role: Role,
    term: &BoundTerm,
) -> Result<Option<u64>, Problem> {
    if term.denotes_blank_node() {
        return Ok(None);
    }
    dictionary
        .locate(role, term.dictionary().as_bytes())
        .map(|found| found.map(|id| id.0))
        .map_err(|error| unreadable("looking a term up", &error))
}

/// Reverse a scoped blank-node IRI to the dictionary id it names.
///
/// The checks are what stop the identity travelling: a URN reverses only
/// against the bundle whose digest it carries, in a role its section is valid
/// for, at a local id in range, and only when the term there really is a blank
/// node. Anything else is an ordinary IRI and resolves — or does not — as one.
///
/// Shared by request resolution and by the page's label cascade, because a term
/// the API names one way has to be recognized the same way wherever it is read.
fn reverse_scoped(
    dictionary: &Dictionary<'_>,
    blank_nodes: &SkolemScope,
    role: Role,
    text: &str,
) -> Result<Option<u64>, Problem> {
    if !matches!(role, Role::Subject | Role::Object) {
        return Ok(None);
    }
    let Some(id) = blank_nodes.role_id(role, text) else {
        return Ok(None);
    };
    let mut buffer = Vec::new();
    let stored = dictionary
        .extract(role, id, &mut buffer)
        .map_err(|error| unreadable("reversing a blank-node IRI", &error))?;
    Ok(stored.starts_with(b"_:").then_some(id.0))
}

/// Look up a request term, reversing this HDT's skolem URNs in RDF term roles.
///
/// The scoped IRI is the only spelling that reaches a blank node; see
/// [`locate`], which this shares its refusal with.
fn locate_scoped(
    dictionary: &Dictionary<'_>,
    blank_nodes: &SkolemScope,
    role: Role,
    term: &BoundTerm,
) -> Result<Option<u64>, Problem> {
    if term.denotes_blank_node() {
        return Ok(None);
    }
    if let Some(id) = reverse_scoped(dictionary, blank_nodes, role, term.dictionary())? {
        return Ok(Some(id));
    }
    dictionary
        .locate(role, term.dictionary().as_bytes())
        .map(|found| found.map(|id| id.0))
        .map_err(|error| unreadable("looking a term up", &error))
}

/// Dictionary probes shared by all rows of one binding table.
struct LookupCache<'a> {
    dictionary: Dictionary<'a>,
    blank_nodes: SkolemScope,
    found: [HashMap<String, Option<u64>>; 3],
}

impl<'a> LookupCache<'a> {
    fn new(dictionary: Dictionary<'a>, blank_nodes: SkolemScope) -> Self {
        Self {
            dictionary,
            blank_nodes,
            found: std::array::from_fn(|_| HashMap::new()),
        }
    }

    fn locate(&mut self, role: Role, term: &BoundTerm) -> Result<Option<u64>, Problem> {
        let by_term = &mut self.found[role_index(role)];
        if let Some(found) = by_term.get(term.dictionary()) {
            return Ok(*found);
        }
        let found = locate_scoped(&self.dictionary, &self.blank_nodes, role, term)?;
        by_term.insert(term.dictionary().to_owned(), found);
        Ok(found)
    }
}

fn role_index(role: Role) -> usize {
    match role {
        Role::Subject => 0,
        Role::Predicate => 1,
        Role::Object => 2,
    }
}

/// Resolve one body row entirely into this bundle's role-scoped id spaces.
fn resolve_binding(
    cache: &mut LookupCache<'_>,
    row: BindingRow<'_>,
) -> Result<Option<IdPattern>, Problem> {
    let mut ids = IdPattern {
        subject: None,
        predicate: None,
        object: None,
    };
    for position in Position::ALL {
        let Some(term) = row.bound(position) else {
            continue;
        };
        let Some(id) = cache.locate(position.role(), term)? else {
            return Ok(None);
        };
        match position {
            Position::Subject => ids.subject = Some(id),
            Position::Predicate => ids.predicate = Some(id),
            Position::Object => ids.object = Some(id),
        }
    }
    Ok(Some(ids))
}

/// Resolve only the terms fixed directly in a bindings pattern. Every input
/// row is a subset of this selection, so its count is a cheap upper bound for
/// the RDF union when restrictions overlap.
fn resolve_binding_pattern(
    cache: &mut LookupCache<'_>,
    pattern: &BindingPattern,
) -> Result<Option<IdPattern>, Problem> {
    let mut ids = IdPattern {
        subject: None,
        predicate: None,
        object: None,
    };
    for position in Position::ALL {
        let Some(term) = pattern.bound(position) else {
            continue;
        };
        let Some(id) = cache.locate(position.role(), term)? else {
            return Ok(None);
        };
        match position {
            Position::Subject => ids.subject = Some(id),
            Position::Predicate => ids.predicate = Some(id),
            Position::Object => ids.object = Some(id),
        }
    }
    Ok(Some(ids))
}

fn select(store: &Store, ids: IdPattern) -> Result<Selection<'_>, Problem> {
    // Every id here came out of this bundle's own dictionary, so the only error
    // `resolve` defines — an id outside its role's space — is unreachable.
    store
        .resolve(ids)
        .map_err(|error| unreadable("resolving a pattern", &error))
}

/// The parts of an answer that the enumeration does not produce.
struct Envelope {
    echo: Echo,
    vars: Vars,
    directed: bool,
    bindings: bool,
    absent_terms: Vec<AbsentTerm>,
    blank_nodes: SkolemScope,
    tagging: GraphTagging,
}

/// Where a page starts, how far it may go, and what a cursor out of it binds to.
///
/// One value rather than four parameters because the four are one decision: a
/// page is cut by whichever of `limit` and `bytes` is reached first, resumed at
/// `cursor`, and continued by a token `binding` addresses.
struct Paging<'a> {
    cursor: Option<&'a Cursor>,
    limit: u32,
    bytes: ResponseBytes,
    binding: &'a CursorBinding,
}

impl Paging<'_> {
    /// One more row than the page may carry. If it arrives there is a next
    /// page, and that row is where it starts.
    fn want(&self) -> usize {
        self.limit as usize + 1
    }
}

/// Build a page of rows out of `phases`, resuming where `paging` says.
fn paged(
    store: &Store,
    target: Target,
    envelope: Envelope,
    phases: Vec<Phase<'_>>,
    paging: Paging<'_>,
) -> Result<Answer, Problem> {
    let dictionary = store.dict();
    let predicates = dictionary.counts().len(Role::Predicate);
    let steps = walk(&phases, paging.cursor, predicates, paging.want())?;
    // Exact, and known before the walk: a pattern's cardinality is a range
    // width after bounded descent, so the enumeration is not what produces it.
    let cardinality = exact_cardinality_sum(phases.iter().map(|phase| phase.count))?;

    finish(store, target, envelope, steps, paging, None, |_, _| {
        cardinality
    })
}

/// Build a brTPF RDF page after filtering the compatibility relation to its
/// distinct triple union. Filtering happens before the page limit, so overlap
/// cannot turn a full native page into an empty Hydra page.
fn paged_distinct_bindings(
    store: &Store,
    target: Target,
    envelope: Envelope,
    phases: Vec<Phase<'_>>,
    restrictions: &[(u32, IdPattern)],
    candidates: Candidates,
    paging: Paging<'_>,
) -> Result<Answer, Problem> {
    let dictionary = store.dict();
    let predicates = dictionary.counts().len(Role::Predicate);
    let (steps, spent) = walk_distinct_bindings(
        &phases,
        restrictions,
        paging.cursor,
        predicates,
        paging.want(),
        candidates,
    )?;
    let cardinality = exact_cardinality_sum(phases.iter().map(|phase| phase.count))?;
    finish(store, target, envelope, steps, paging, spent, |_, _| {
        cardinality
    })
}

/// Sum independently resolved phase cardinalities without letting a valid
/// request wrap its answer or panic the blocking worker.
fn exact_cardinality_sum(counts: impl IntoIterator<Item = u64>) -> Result<Cardinality, Problem> {
    let mut counts = counts.into_iter();
    let value = counts
        .try_fold(0u64, |total, count| total.checked_add(count))
        .ok_or_else(|| {
            tracing::error!("the sum of binding cardinalities exceeded u64");
            Problem::new(
                ErrorCode::InternalError,
                "the result cardinality exceeds this server's numeric range",
            )
        })?;
    Ok(Cardinality::exact(value))
}

/// Cardinality of brTPF's distinct-RDF projection.
///
/// Disjoint restrictions add exactly, and a restriction that subsumes all the
/// others gives the union exactly. Arbitrary partial overlaps would require an
/// unbounded union enumeration, so report a bounded upper estimate: no larger
/// than either the relation sum or the base triple pattern containing every
/// restriction. TPF cardinalities are planning estimates; query correctness
/// continues to come from paging the distinct projection to exhaustion.
fn rdf_projection_cardinality(
    store: &Store,
    base_pattern: Option<IdPattern>,
    restrictions: &[(IdPattern, u64)],
) -> Result<Cardinality, Problem> {
    let total = exact_cardinality_sum(restrictions.iter().map(|(_, count)| *count))?.value();
    let active: Vec<_> = restrictions
        .iter()
        .copied()
        .filter(|(_, count)| *count > 0)
        .collect();
    if active.is_empty() {
        return Ok(Cardinality::exact(0));
    }
    if let Some((_, count)) = active.iter().find(|(general, _)| {
        active
            .iter()
            .all(|(specific, _)| id_pattern_subsumes(*general, *specific))
    }) {
        return Ok(Cardinality::exact(*count));
    }
    let disjoint = active.iter().enumerate().all(|(index, (left, _))| {
        active[index + 1..]
            .iter()
            .all(|(right, _)| !id_patterns_overlap(*left, *right))
    });
    if disjoint {
        return Ok(Cardinality::exact(total));
    }

    let Some(base_pattern) = base_pattern else {
        // A fixed base term absent from the dictionary makes every restriction
        // empty, which the `active` check above would already have returned.
        tracing::error!("non-empty binding restrictions have an absent base pattern");
        return Err(Problem::new(
            ErrorCode::InternalError,
            "the RDF fragment cardinality could not be determined",
        ));
    };
    let base_count = select(store, base_pattern)?.count().value;
    Ok(Cardinality::estimated(total.min(base_count)))
}

/// Finish a text-filtered page.
///
/// The same materializing, byte budget and cursor as a pattern page — only the
/// steps came from a ranking, which adds a third way to stop and makes the
/// cardinality depend on how the page ended rather than being known before it
/// started.
fn ranked_page(
    store: &Store,
    target: Target,
    envelope: Envelope,
    found: Ranked,
    paging: Paging<'_>,
) -> Result<Answer, Problem> {
    let Ranked {
        steps,
        matching_literals,
        unfiltered,
        spent,
    } = found;
    let from_start = paging.cursor.is_none();

    finish(
        store,
        target,
        envelope,
        steps,
        paging,
        spent,
        |completeness, rows| {
            text_cardinality(
                completeness,
                from_start,
                rows.len() as u64,
                matching_literals,
                unfiltered,
            )
        },
    )
}

/// Materialize a page's rows within the byte budget and say how it ended.
///
/// The whole of what the two paged operations share, which is everything after
/// their steps exist. What they do not share is the two parameters: `spent`
/// adds the stop reason only a filtered operation has, and `cardinality` is
/// computed *after* completeness because a ranked count depends on it — a page
/// that ran out from the top has enumerated its own answer, and can say so
/// exactly.
fn finish(
    store: &Store,
    target: Target,
    envelope: Envelope,
    mut steps: Vec<Step>,
    paging: Paging<'_>,
    spent: Option<Spent>,
    cardinality: impl FnOnce(&Completeness, &[Row]) -> Cardinality,
) -> Result<Answer, Problem> {
    let dictionary = store.dict();
    // The row this page cannot carry, kept because it is where the next one
    // begins rather than merely because it exists.
    let dropped = (steps.len() == paging.want())
        .then(|| steps.pop())
        .flatten();

    let Envelope {
        echo,
        vars,
        directed,
        bindings,
        absent_terms,
        blank_nodes,
        tagging,
    } = envelope;

    // Materializing is where the bytes appear, so it is where the byte budget
    // applies — before the response exists rather than after, which also bounds
    // the memory a page can take.
    let (rows, spent_at) = materialize(
        &dictionary,
        &blank_nodes,
        store.graphs(),
        &vars,
        &steps,
        paging.bytes,
    )?;
    let row_resumes = steps[..rows.len()].iter().map(Step::row_resume).collect();

    // Whichever bound was reached first names the reason and the resume point.
    // Bytes first, because a page stopped for bytes never reached its row count
    // and its cursor is the row the bytes ran out on; then the page limit; then
    // the candidates, which is the one that means "there may be more, and
    // finding out costs more than this request is allowed to spend".
    let completeness = match (spent_at.map(|index| &steps[index]), &dropped, spent) {
        (Some(next), _, _) => {
            Completeness::budget_exhausted(BudgetReason::ResponseBytes, next.cursor(paging.binding))
        }
        (None, Some(next), _) => Completeness::page_limit(next.cursor(paging.binding)),
        (None, None, Some(Spent::Deepest)) => {
            Completeness::budget_exhausted_without_resume(BudgetReason::Candidate)
        }
        (None, None, Some(Spent::Candidate(next))) => {
            Completeness::budget_exhausted(BudgetReason::Candidate, next.cursor(paging.binding))
        }
        // The enumeration ran out inside this page, so it is the whole answer.
        (None, None, None) => Completeness::complete(),
    };

    Ok(Answer {
        dataset: target.id.dataset.clone(),
        version: target.id.version.clone(),
        echo,
        cardinality: cardinality(&completeness, &rows),
        absent_terms,
        rows,
        row_resumes,
        row_binding: Some(paging.binding.clone()),
        rdf_cardinality: None,
        page_limit: paging.limit,
        byte_budget: paging.bytes.0,
        vars,
        completeness,
        directed,
        bindings,
        target,
        blank_nodes,
        page_labels: HashMap::new(),
        described: None,
        tagging,
    })
}

/// How many rows a text-filtered pattern matches.
///
/// A page that started at the beginning and ran out is the whole answer, so the
/// rows *are* the count and it is exact. Saying "about 4" over five rows a
/// client can see is worse than useless — it makes every other estimate in the
/// response harder to believe.
///
/// Otherwise the index supplies the number of distinct matching *literals*,
/// which is a different quantity from the rows: one literal occurs on many
/// subjects. It goes out as the estimate, and as `distinct_objects` when the
/// count reached the end, only when `s` and `p` are both unbound. With either
/// bound, a matching literal may contribute no row, so only the rows actually
/// produced are reported as the estimate.
///
/// `value` never falls below the rows in the response, which it otherwise
/// would: one literal on three hundred subjects is a `distinct_objects` of 1
/// under a page of a hundred. Raising it is a plain `max` rather than
/// [`Cardinality::at_least`], because `min` means "a scan reached this" and a
/// page's own row count is not that — filling it in per page would make the
/// advertised lower bound *fall* as a client pages, which reads as the result
/// set shrinking under a version that cannot change.
fn text_cardinality(
    completeness: &Completeness,
    from_start: bool,
    rows: u64,
    matching_literals: MatchingLiterals,
    unfiltered: bool,
) -> Cardinality {
    if completeness.is_complete() && from_start {
        return Cardinality::exact(rows);
    }
    let MatchingLiterals { counted, exact } = matching_literals;
    if !unfiltered {
        return Cardinality::estimated(rows);
    }
    let estimate = Cardinality::estimated(counted.max(rows));
    if exact {
        estimate.over_distinct_objects(counted)
    } else {
        // The count stopped at the budget, so what is known is a floor on the
        // literals — and one that holds for every page of this request rather
        // than moving with the page.
        estimate.at_least(counted)
    }
}

/// Walk `phases` in order from `cursor`, collecting at most `want` rows.
///
/// The cursor's [`PositionSpace`] selects the phase, which is what lets
/// `/describe` resume in either half without a field of its own: `s ? ?` reads
/// SPO and `? ? o` reads OPS, so the two never collide.
fn walk(
    phases: &[Phase<'_>],
    cursor: Option<&Cursor>,
    predicates: u64,
    want: usize,
) -> Result<Vec<Step>, Problem> {
    let (start, mut from, mut skip) = walk_start(phases, cursor, predicates)?;

    let mut steps = Vec::new();
    for phase in &phases[start..] {
        if steps.len() >= want {
            break;
        }
        let remaining = want - steps.len();
        for row in positioned(&phase.enumeration, phase.space, from, skip).take(remaining) {
            let row = row?;
            steps.push(Step {
                triple: row.triple,
                graph: row.graph,
                space: phase.space,
                resume: row.resume,
                scan: row.delivered,
                binding_index: phase.binding_index,
                direction: phase.direction,
                ranking: None,
            });
        }
        from = 0;
        skip = 0;
    }
    Ok(steps)
}

/// Walk binding phases while assigning every RDF triple to its first matching
/// normalized restriction. Both retained and discarded candidates spend the
/// published budget; the continuation is the first candidate not examined.
fn walk_distinct_bindings(
    phases: &[Phase<'_>],
    restrictions: &[(u32, IdPattern)],
    cursor: Option<&Cursor>,
    predicates: u64,
    want: usize,
    candidates: Candidates,
) -> Result<(Vec<Step>, Option<Spent>), Problem> {
    let (start, mut from, mut skip) = walk_start(phases, cursor, predicates)?;
    let mut steps = Vec::new();
    let mut examined = 0u64;
    for phase in &phases[start..] {
        let binding_index = phase
            .binding_index
            .expect("a distinct binding walk contains only binding phases");
        for row in positioned(&phase.enumeration, phase.space, from, skip) {
            let row = row?;
            let candidate = Step {
                triple: row.triple,
                graph: row.graph,
                space: phase.space,
                resume: row.resume,
                scan: row.delivered,
                binding_index: phase.binding_index,
                direction: phase.direction,
                ranking: None,
            };
            if examined >= candidates.0 {
                return Ok((steps, Some(Spent::Candidate(candidate))));
            }
            examined += 1;
            if restrictions.iter().any(|(owner, pattern)| {
                *owner < binding_index && id_pattern_matches(*pattern, row.triple)
            }) {
                continue;
            }
            steps.push(candidate);
            if steps.len() >= want {
                return Ok((steps, None));
            }
        }
        from = 0;
        skip = 0;
    }
    Ok((steps, None))
}

fn walk_start(
    phases: &[Phase<'_>],
    cursor: Option<&Cursor>,
    predicates: u64,
) -> Result<(usize, u64, u64), Problem> {
    match cursor {
        None => Ok((0, 0, 0)),
        Some(cursor) => {
            let index = phases
                .iter()
                .position(|phase| {
                    phase.space == cursor.space && phase.binding_index == cursor.binding_index
                })
                .ok_or_else(|| Problem::from(StaleCursor))?;
            let (from, skip) = resume_position(cursor, &phases[index], predicates)?;
            Ok((index, from, skip))
        }
    }
}

/// A row with the position a page resumes at to return it first.
struct PositionedRow {
    triple: IdTriple,
    graph: Option<GraphId>,
    resume: u64,
    delivered: Option<u64>,
}

/// Pair each row with the position a page resumes at to return it first.
///
/// The running position *before* each triple, in whichever space this phase
/// counts in: an offset for the three permutation spaces, and for `s ? o` the
/// previous triple's predicate id — route-independent and strictly increasing,
/// since one (s, p, o) occurs at most once. In the quad view every row of one
/// triple shares the triple's position; the row's own place in the triple's
/// run travels beside it.
fn positioned<'a>(
    enumeration: &'a Enumeration<'a>,
    space: PositionSpace,
    from: u64,
    skip: u64,
) -> impl Iterator<Item = Result<PositionedRow, Problem>> + 'a {
    let mut resume = from;
    let mut current: Option<IdTriple> = None;
    // The enumeration is lazy, so the caller's `take` is what bounds the work,
    // and a multi-phase walk cannot know its own bound per phase up front.
    enumeration.rows(from, skip).map(move |row| {
        let row = row?;
        // A new triple, unless this row continues the run the last one was in.
        // The first triple of a page resumes at `from` itself, which for the
        // predicate space is the predicate *before* it — what a page that ends
        // inside this triple's run must carry to start this triple again.
        if current != Some(row.triple) {
            if let Some(previous) = current {
                resume = match space {
                    PositionSpace::Predicate => previous.predicate,
                    _ => resume + 1,
                };
            }
            current = Some(row.triple);
        }
        Ok(PositionedRow {
            triple: row.triple,
            graph: row.graph,
            resume,
            delivered: row.delivered,
        })
    })
}

/// Where a cursor resumes this phase — the triple's position, and how many of
/// its memberships to skip — or `stale_cursor`.
fn resume_position(
    cursor: &Cursor,
    phase: &Phase<'_>,
    predicates: u64,
) -> Result<(u64, u64), Problem> {
    let stale = || Problem::from(StaleCursor);
    // A phase's binding trailer must match it exactly. The run trailer belongs
    // to the quad view alone here — the text spaces that also use it are not
    // phases — so a quad-view cursor carries it and no other cursor may.
    if cursor.binding_index != phase.binding_index
        || cursor.scan_position.is_some() != phase.enumeration.is_quad_view()
    {
        return Err(stale());
    }
    // A position past the end would otherwise page to an empty response, which
    // a client reads as the end of results rather than as a bad token.
    let within = match phase.space {
        PositionSpace::Predicate => {
            (1..=predicates).contains(&cursor.position)
                // At a binding-row boundary there is no previous predicate;
                // zero is the sentinel for the first result of the new row.
                // Nor is there one for a quad-view page that ended inside
                // the first triple's run, which resumes that triple from the
                // start of the enumeration and skips the rows it delivered.
                || (cursor.position == 0
                    && (phase.binding_index.is_some() || phase.enumeration.is_quad_view()))
        }
        _ => cursor.position < phase.triples,
    };
    within
        .then_some((cursor.position, cursor.scan_position.unwrap_or(0)))
        .ok_or_else(stale)
}

/// Turn ids into terms once per distinct term, within
/// `max_response_bytes`.
///
/// Returns the rows and, if the byte budget stopped it, the index of the first
/// step *not* included — which is where the next page starts.
///
/// # Why the budget lands here
///
/// A row cap is not a byte cap because one legal literal can be megabytes, and bundles
/// really do hold them, so `limit` alone leaves a response unbounded, which is
/// the one thing this project exists to prevent. Applying it while rows are
/// built rather than after they are serialized also bounds what a page costs in
/// *memory*: the terms are in hand at this point, and a page assembled first
/// and measured second would have to fit before it could be refused.
///
/// The measure is each row's compact JSON, exact for the JSON serialization and
/// conservative for other representations. It is *counted* rather than produced: [`TermCache`] weighs each
/// distinct term once and [`Row::new`] adds the map's fixed punctuation, so a
/// page pays per term rather than per row. Serializing every row to size it
/// cost a third of what rendering the response costs (10 000 rows: 1.0 ms of
/// weighing against 3.0 ms of rendering), which is what that arrangement is
/// worth avoiding.
///
/// RDF has a second, exact complete-document fit because Turtle and JSON-LD
/// size is not row-local: serializers group statements and retain closing
/// syntax until `finish`. This first pass still applies there as the hard bound
/// on materialized term memory. The exact RDF pass may therefore conservatively
/// return fewer rows than its wire budget alone could hold, but the alternative
/// would require assembling up to `max_limit * max_term_bytes` before any byte
/// bound applied. The two passes bound different resources; neither can be
/// removed without replacing it with an equally explicit memory bound.
fn materialize(
    dictionary: &Dictionary<'_>,
    blank_nodes: &SkolemScope,
    graphs: Option<&Graphs>,
    vars: &Vars,
    steps: &[Step],
    bytes: ResponseBytes,
) -> Result<(Vec<Row>, Option<usize>), Problem> {
    let mut cache = TermCache::new();
    let mut published = PublishedTerms::new(blank_nodes.clone());
    let mut graph_names = GraphNames::new(blank_nodes);
    let mut rows: Vec<Row> = Vec::with_capacity(steps.len());
    let mut spent = 0u64;
    for (index, step) in steps.iter().enumerate() {
        let mut cells = Vec::with_capacity(vars.positions().len());
        let mut terms = 0u64;
        for position in vars.positions() {
            let (term, serialized) = published
                .measured(
                    &mut cache,
                    dictionary,
                    position.role(),
                    TermId(position.of(step.triple)),
                )
                .map_err(|error| unreadable("materializing a term", &error))?;
            terms += serialized;
            cells.push((*position, term));
        }
        let graph = match (vars.has_graph(), step.graph, graphs) {
            (true, Some(graph), Some(graphs)) => Some(graph_names.measured(graphs, graph)?),
            (true, _, _) => {
                tracing::error!("a quad-view row has no graph to report");
                return Err(Problem::new(
                    ErrorCode::InternalError,
                    "the quad view could not name a row's graph",
                ));
            }
            (false, _, _) => None,
        };
        let row = Row::new(
            cells,
            terms,
            graph,
            step.binding_index,
            step.direction,
            step.ranking,
        );

        spent = spent.saturating_add(row.serialized);
        // Never on the first row of a page. A single term larger than the whole
        // budget would otherwise produce an empty page whose cursor resumes
        // exactly where it was issued, and a client paging on it would never
        // move — one row over a budget beats an enumeration nothing can walk.
        if spent > bytes.0 && !rows.is_empty() {
            return Ok((rows, Some(index)));
        }
        rows.push(row);
    }
    Ok((rows, None))
}

/// Graph names as this API publishes them, memoized for one page.
///
/// A graph's name is spelled once per distinct graph rather than once per
/// row: the quad view repeats a handful of graphs down a page. A graph named
/// by a blank node is published as this bundle's scoped IRI, as a data blank
/// node is, because a `_:` label means nothing outside the document it came
/// from.
struct GraphNames<'a> {
    blank_nodes: &'a SkolemScope,
    names: HashMap<GraphId, (Rc<str>, u64)>,
    buffer: Vec<u8>,
}

impl<'a> GraphNames<'a> {
    fn new(blank_nodes: &'a SkolemScope) -> Self {
        Self {
            blank_nodes,
            names: HashMap::new(),
            buffer: Vec::new(),
        }
    }

    /// The published spelling, and the bytes its term object takes.
    fn measured(&mut self, graphs: &Graphs, graph: GraphId) -> Result<(Rc<str>, u64), Problem> {
        if let Some(found) = self.names.get(&graph) {
            return Ok(found.clone());
        }
        let stored = graphs
            .name(graph, &mut self.buffer)
            .map_err(|error| unreadable("reading a graph's name", &error))?;
        let stored = std::str::from_utf8(stored).map_err(|error| {
            unreadable("reading a graph's name", &format!("not UTF-8: {error}"))
        })?;
        let published: Rc<str> = match self.blank_nodes.graph_iri(graph, stored) {
            Some(iri) => Rc::from(iri.as_str()),
            None => Rc::from(stored),
        };
        let serialized = serialized_bytes(&Term::Iri(Cow::Borrowed(published.as_ref())));
        self.names
            .insert(graph, (Rc::clone(&published), serialized));
        Ok((published, serialized))
    }
}

/// A bundle this server published and cannot read is the server's problem, not
/// the request's — so the classified cause goes to the log and the client is
/// told only that it failed.
fn unreadable(what: &'static str, error: &dyn std::fmt::Display) -> Problem {
    tracing::error!(%error, what, "a bundle that opened could not answer");
    Problem::new(
        ErrorCode::InternalError,
        "the bundle could not be read while answering this request",
    )
}

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

/// Which members of a result set of `count` a sample of `n` draws.
///
/// Ascending and without repetition. Without repetition because a sample whose
/// job is to show what values look like is worse for containing one twice;
/// ascending because it costs nothing, makes the response read in the
/// operation's own enumeration order, and walks the index forward rather than
/// jumping about in it.
fn sample_positions(count: u64, n: u64, seed: u64) -> Vec<u64> {
    if count == 0 || n == 0 {
        return Vec::new();
    }
    if n >= count {
        return (0..count).collect();
    }

    let mut random = SplitMix64::seeded(seed);
    let mut positions: Vec<u64> = if count <= n.saturating_mul(2) {
        // Dense. `count` is under twice `n`, so under twice the sample cap, and a
        // partial Fisher–Yates over the whole range is bounded work — whereas
        // rejecting collisions is a coupon-collector loop at this density.
        let mut pool: Vec<u64> = (0..count).collect();
        for index in 0..n {
            let pick = index + random.below(count - index);
            pool.swap(index as usize, pick as usize);
        }
        pool.truncate(n as usize);
        pool
    } else {
        // Sparse. Fewer than half the positions are wanted, so a redraw is
        // needed less than half the time and the expected number of draws is
        // under `2n`.
        let mut drawn = HashSet::with_capacity(n as usize);
        while (drawn.len() as u64) < n {
            drawn.insert(random.below(count));
        }
        drawn.into_iter().collect()
    };
    positions.sort_unstable();
    positions
}

/// Draw `n` members, and report how many there were to draw from.
///
/// The cardinality comes back with the sample because for `s ? o` the two are
/// the *same work*: the server runs its bounded smaller-endpoint probe once,
/// holds the resulting predicate-id set in request-local memory, and samples
/// positions from that set. `Selection::count` is that probe. Asking
/// for the count first and the members afterwards runs it twice — which is what
/// this did until the review caught it, despite the operation budgeting exactly
/// one such probe.
///
/// For the seven contiguous shapes there is nothing to hold: the count is a
/// range width and `Selection::at` is a rank descent, so each is paid once.
fn draw(selection: &Selection<'_>, n: u64, seed: u64) -> (u64, Vec<IdTriple>) {
    if selection.subject_object_route().is_some() {
        let members: Vec<IdTriple> = selection.page(0, usize::MAX).collect();
        let count = members.len() as u64;
        let drawn = sample_positions(count, n, seed)
            .into_iter()
            .map(|position| members[position as usize])
            .collect();
        (count, drawn)
    } else {
        let count = selection.count().value;
        let drawn = sample_positions(count, n, seed)
            .into_iter()
            .map(|position| selection.at(position))
            .collect();
        (count, drawn)
    }
}

/// SplitMix64.
///
/// Written out rather than taken from a crate because the draw is deterministic
/// for a given seed and version, hence cacheable. A generator whose stream may change between
/// releases of someone else's crate cannot back a contract like that. Six
/// lines, fixed forever, and the algorithm is named so a client could
/// reproduce it.
struct SplitMix64(u64);

impl SplitMix64 {
    fn seeded(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform value below `bound`, which must be non-zero.
    ///
    /// Rejects the short tail rather than taking a remainder: `% bound` is
    /// biased towards small values whenever `bound` does not divide 2⁶⁴, which
    /// for a sample means the front of the result set is over-represented.
    fn below(&mut self, bound: u64) -> u64 {
        debug_assert!(bound > 0, "a draw needs something to draw from");
        let remainder = (u64::MAX % bound + 1) % bound;
        loop {
            let value = self.next();
            if value >= remainder {
                return value % bound;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

impl Resource for SchemaAnswer {
    fn to_json(&self) -> Bytes {
        json_body(self)
    }

    fn to_html(&self) -> String {
        match self {
            Self::Navigation(answer) => answer.to_html(),
            Self::Relations(answer) => answer.to_html(),
            Self::ClassProperties(answer) => answer.to_html(),
        }
    }
}

impl SchemaNavigationAnswer {
    fn to_html(&self) -> String {
        let item_terms: Vec<_> = self
            .items
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|item| schema_resource_cell(&self.target, item, self.labels.as_ref()))
            .collect();
        let rows: Vec<Vec<Value<'_>>> = self
            .items
            .as_deref()
            .unwrap_or_default()
            .iter()
            .zip(&item_terms)
            .map(|(item, term)| {
                vec![
                    Value::Text(item.kind),
                    term.value(),
                    optional_number(item.counts.entities),
                    optional_number(item.counts.triples),
                    optional_number(item.counts.distinct_subjects),
                    optional_number(item.counts.distinct_objects),
                    optional_number(item.counts.properties),
                ]
            })
            .collect();
        let node_term = self
            .node
            .as_ref()
            .and_then(|node| node.term.as_ref())
            .map(|term| schema_cell(&self.target, term, None, self.labels.as_ref()));
        let selector_class = self
            .selector
            .class()
            .map(|term| schema_cell(&self.target, term, None, self.labels.as_ref()));
        let selector_predicate = self
            .selector
            .predicate()
            .map(|term| schema_cell(&self.target, term, None, self.labels.as_ref()));
        let selector_datatype = self
            .selector
            .datatype()
            .map(|term| schema_cell(&self.target, term, None, self.labels.as_ref()));
        let canonical = self.target.canonical();
        let context = format!(
            "Schema · {} · {} {}",
            self.selector.kind(),
            self.target.id.dataset,
            self.target.id.version
        );
        let returned = self.items.as_ref().map_or(0, |items| items.len()) as u64;
        let focus = node_term
            .as_ref()
            .or(selector_datatype.as_ref())
            .or(selector_predicate.as_ref())
            .or(selector_class.as_ref());
        let title = focus.map_or_else(
            || match &self.collection {
                Some(collection) => schema_collection_title(collection.kind).to_owned(),
                None => "Schema overview".to_owned(),
            },
            |cell| cell.label.clone(),
        );
        let details = self
            .node
            .as_ref()
            .and_then(|node| node.links.get("self"))
            .map(String::as_str);
        let crumbs = self.schema_crumbs(details);
        operation_page(
            &self.target.mount,
            &title,
            &context,
            &crumbs,
            canonical.as_deref(),
            html! {
                @if let Some(full_iri) = focus.and_then(|cell| cell.full_iri.as_deref()) {
                    p."focus-identifier" { code { (full_iri) } }
                }
                div."answer-summary" {
                    (fields(&[
                        ("view", Value::Code(&self.view)),
                        ("selector", Value::Code(self.selector.kind())),
                        ("class scope", selector_class.as_ref().map_or(Value::Absent, Cell::value)),
                        ("predicate", selector_predicate.as_ref().map_or(Value::Absent, Cell::value)),
                        ("datatype", selector_datatype.as_ref().map_or(Value::Absent, Cell::value)),
                        ("collection", self.collection.as_ref().map_or(Value::Absent, |collection| Value::Text(collection.kind))),
                        ("order", self.collection.as_ref().map_or(Value::Absent, |collection| Value::Code(collection.order))),
                        ("returned", self.items.as_ref().map_or(Value::Absent, |_| Value::Number(returned))),
                        ("complete", Value::Text(completeness_text(&self.completeness))),
                    ]))
                }
                section."section-block" {
                    h2 { (schema_node_heading(self.selector.kind())) }
                    @if let Some(node) = &self.node {
                        (fields(&[
                            ("kind", Value::Text(node.kind)),
                            ("term", node_term.as_ref().map_or(Value::Absent, Cell::value)),
                            ("entities", optional_number(node.counts.entities)),
                            ("triples", optional_number(node.counts.triples)),
                            ("distinct subjects", optional_number(node.counts.distinct_subjects)),
                            ("distinct objects", optional_number(node.counts.distinct_objects)),
                            ("properties", optional_number(node.counts.properties)),
                        ]))
                        @if !node.links.is_empty() {
                            nav."schema-actions" aria-label="Schema drill-down" {
                                ul {
                                    @for (label, href) in &node.links {
                                        li {
                                            a href=(href) {
                                                strong { (schema_action_label(label)) }
                                                span { (schema_action_description(label)) }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } @else {
                        (note("The selected schema node is absent from this view."))
                    }
                }
                @if let Some(collection) = &self.collection {
                    section."section-block" {
                        h2 { (schema_collection_title(collection.kind)) }
                        @if rows.is_empty() {
                            (note("No child items."))
                        } @else {
                            (results_table(
                                &["kind", "term", "entities", "triples", "distinct subjects", "distinct objects", "properties"],
                                &rows,
                            ))
                        }
                    }
                }
                @if let Some(token) = self.completeness.next_cursor() {
                    @if let Some(next) = self.target.next(token) {
                        (pager(&next, "Next page →"))
                    }
                }
            },
        )
    }

    fn schema_crumbs<'a>(&'a self, details: Option<&str>) -> Vec<Crumb<'a>> {
        let mut crumbs = vec![
            Crumb::to(
                &self.target.id.dataset,
                self.target.mount.dataset(&self.target.id.dataset),
            ),
            Crumb::to(
                &self.target.id.version,
                self.target.mount.operation(
                    &self.target.id.dataset,
                    &self.target.id.version,
                    "manifest",
                ),
            ),
        ];
        if self.selector.kind() == "dataset" && self.collection.is_none() {
            crumbs.push(Crumb::here("schema"));
            return crumbs;
        }

        crumbs.push(Crumb::to(
            "schema",
            query(
                self.target.mount.operation(
                    &self.target.id.dataset,
                    &self.target.id.version,
                    "schema",
                ),
                &Params::default().with("view", &self.view),
            ),
        ));
        if let Some(collection) = &self.collection {
            if let Some(details) = details {
                crumbs.push(Crumb::to(self.selector.kind(), details.to_owned()));
            }
            crumbs.push(Crumb::here(collection.kind));
        } else {
            crumbs.push(Crumb::here(self.selector.kind()));
        }
        crumbs
    }
}

impl SchemaRelationsAnswer {
    fn to_html(&self) -> String {
        let filter_class = self
            .filters
            .class
            .as_ref()
            .map(|term| schema_cell(&self.target, term, None, self.labels.as_ref()));
        let filter_predicate = self
            .filters
            .predicate
            .as_ref()
            .map(|term| schema_cell(&self.target, term, None, self.labels.as_ref()));
        let terms: Vec<[Cell<'_>; 3]> = self
            .items
            .iter()
            .map(|item| {
                [
                    relation_cell(
                        &self.target,
                        &self.view,
                        "class",
                        &item.subject_class,
                        self.labels.as_ref(),
                    ),
                    relation_cell(
                        &self.target,
                        &self.view,
                        "predicate",
                        &item.predicate,
                        self.labels.as_ref(),
                    ),
                    relation_cell(
                        &self.target,
                        &self.view,
                        "class",
                        &item.object_class,
                        self.labels.as_ref(),
                    ),
                ]
            })
            .collect();
        let rows: Vec<Vec<Value<'_>>> = self
            .items
            .iter()
            .zip(&terms)
            .map(|(item, terms)| {
                vec![
                    terms[0].value(),
                    terms[1].value(),
                    terms[2].value(),
                    Value::Number(item.triples),
                ]
            })
            .collect();
        let canonical = self.target.canonical();
        let context = self.target.context();
        operation_page(
            &self.target.mount,
            "Class relations",
            &context,
            &self.target.crumbs(),
            canonical.as_deref(),
            html! {
                div."answer-summary" {
                    (fields(&[
                        ("view", Value::Code(&self.view)),
                        ("projection", Value::Code(self.projection)),
                        ("class filter", filter_class.as_ref().map_or(Value::Absent, Cell::value)),
                        ("predicate filter", filter_predicate.as_ref().map_or(Value::Absent, Cell::value)),
                        ("order", Value::Text("triples descending")),
                        ("returned", Value::Number(self.items.len() as u64)),
                        ("complete", Value::Text(completeness_text(&self.completeness))),
                    ]))
                }
                section."section-block" {
                    h2 { "Observed class relations" }
                    (note(
                        "These are observed connections whose subject and object both have RDF \
                         types. They are not rdfs:domain or rdfs:range declarations; multi-typed \
                         entities can contribute to more than one row, and untyped targets are \
                         not represented here."
                    ))
                    @if rows.is_empty() {
                        (note("No matching class relations."))
                    } @else {
                        (results_table(
                            &["subject class", "predicate", "object class", "triples"],
                            &rows,
                        ))
                    }
                }
                @if let Some(token) = self.completeness.next_cursor() {
                    @if let Some(next) = self.target.next(token) {
                        (pager(&next, "Next page →"))
                    }
                }
            },
        )
    }
}

impl SchemaClassPropertiesAnswer {
    fn to_html(&self) -> String {
        let filter_class = self
            .filters
            .class
            .as_ref()
            .map(|term| schema_cell(&self.target, term, None, self.labels.as_ref()));
        let filter_predicate = self
            .filters
            .predicate
            .as_ref()
            .map(|term| schema_cell(&self.target, term, None, self.labels.as_ref()));
        let terms: Vec<[Cell<'_>; 2]> = self
            .items
            .iter()
            .map(|item| {
                [
                    relation_cell(
                        &self.target,
                        &self.view,
                        "class",
                        &item.class,
                        self.labels.as_ref(),
                    ),
                    relation_cell(
                        &self.target,
                        &self.view,
                        "predicate",
                        &item.predicate,
                        self.labels.as_ref(),
                    ),
                ]
            })
            .collect();
        let has_distinct = self
            .items
            .iter()
            .any(|item| item.distinct_subjects.is_some() || item.distinct_objects.is_some());
        let rows: Vec<Vec<Value<'_>>> = self
            .items
            .iter()
            .zip(&terms)
            .map(|(item, terms)| {
                let mut row = vec![
                    terms[0].value(),
                    terms[1].value(),
                    Value::Number(item.triples),
                ];
                if has_distinct {
                    row.push(optional_number(item.distinct_subjects));
                    row.push(optional_number(item.distinct_objects));
                }
                row
            })
            .collect();
        let canonical = self.target.canonical();
        operation_page(
            &self.target.mount,
            "Class properties",
            &self.target.context(),
            &self.target.crumbs(),
            canonical.as_deref(),
            html! {
                div."answer-summary" {
                    (fields(&[
                        ("view", Value::Code(&self.view)),
                        ("projection", Value::Code(self.projection)),
                        ("class filter", filter_class.as_ref().map_or(Value::Absent, Cell::value)),
                        ("predicate filter", filter_predicate.as_ref().map_or(Value::Absent, Cell::value)),
                        ("order", Value::Text("triples descending")),
                        ("returned", Value::Number(self.items.len() as u64)),
                        ("complete", Value::Text(completeness_text(&self.completeness))),
                    ]))
                }
                section."section-block" {
                    h2 { "Properties by class" }
                    (note(
                        "Observed predicates used on instances of each class, ranked by triple count."
                    ))
                    @if rows.is_empty() {
                        (note("No matching class properties."))
                    } @else if has_distinct {
                        (results_table(
                            &["class", "property", "triples", "distinct subjects", "distinct objects"],
                            &rows,
                        ))
                    } @else {
                        (results_table(
                            &["class", "property", "triples"],
                            &rows,
                        ))
                    }
                }
                @if let Some(token) = self.completeness.next_cursor() {
                    @if let Some(next) = self.target.next(token) {
                        (pager(&next, "Next page →"))
                    }
                }
            },
        )
    }
}

fn optional_number(number: Option<u64>) -> Value<'static> {
    number.map_or(Value::Absent, Value::Number)
}

fn schema_resource_cell<'a>(
    target: &Target,
    resource: &'a SchemaResource,
    labels: Option<&'a BTreeMap<String, Option<String>>>,
) -> Cell<'a> {
    match &resource.term {
        Some(term) => schema_cell(target, term, preferred_schema_href(&resource.links), labels),
        None => Cell::text("(none)".to_owned()),
    }
}

/// Follow the only continuation directly; otherwise preserve the branch node.
///
/// A class has only `properties`, and a datatype only `languages`, so making a
/// human click their node-only page first adds no choice. A property has two
/// meaningful branches and therefore keeps its own page as the term link.
fn preferred_schema_href(links: &BTreeMap<&'static str, String>) -> Option<String> {
    let mut children = links
        .iter()
        .filter(|(relation, _)| **relation != "self")
        .map(|(_, href)| href);
    match (children.next(), children.next()) {
        (Some(only), None) => Some(only.clone()),
        _ => links.get("self").cloned(),
    }
}

fn schema_collection_title(collection: &str) -> &str {
    match collection {
        "classes" => "Classes",
        "properties" => "Properties",
        "class-relations" => "Class relations",
        "class-properties" => "Class properties",
        "object-classes" => "Object classes",
        "datatypes" => "Datatypes",
        "languages" => "Languages",
        collection => collection,
    }
}

fn schema_node_heading(kind: &str) -> &str {
    match kind {
        "dataset" => "Dataset statistics",
        "class" => "Class details",
        "property" => "Property details",
        "datatype" => "Datatype details",
        _ => "Selected node",
    }
}

fn schema_action_label(relation: &str) -> &str {
    match relation {
        "self" => "Details only",
        "classes" => "Browse classes",
        "properties" => "Browse properties",
        "class-relations" => "Explore class relations",
        "class-properties" => "Compare class properties",
        "object-classes" => "Object classes",
        "datatypes" => "Datatypes",
        "languages" => "Languages",
        relation => relation,
    }
}

fn schema_action_description(relation: &str) -> &str {
    match relation {
        "self" => "Show this node without expanding a collection.",
        "classes" => "The observed RDF types represented in this view.",
        "properties" => "The predicates used at this scope.",
        "class-relations" => "Observed typed connections between subject and object classes.",
        "class-properties" => "Observed predicates used to describe instances of each class.",
        "object-classes" => "The observed RDF types of IRI-valued targets.",
        "datatypes" => "The datatypes used by literal values.",
        "languages" => "The language tags used by these literal values.",
        _ => "Continue through this schema branch.",
    }
}

fn relation_cell<'a>(
    target: &Target,
    view: &str,
    parameter: &str,
    term: &'a SchemaTerm,
    labels: Option<&'a BTreeMap<String, Option<String>>>,
) -> Cell<'a> {
    let requested = Term::from_dictionary(&term.0).to_request();
    let params = Params::default()
        .with(parameter, &requested)
        .with("view", view);
    let params = if parameter == "class" {
        params.with("children", "properties")
    } else {
        params
    };
    let href = relative_schema_link(&params);
    schema_cell(target, term, Some(href), labels)
}

fn schema_cell<'a>(
    target: &Target,
    term: &'a SchemaTerm,
    href: Option<String>,
    labels: Option<&'a BTreeMap<String, Option<String>>>,
) -> Cell<'a> {
    let annotation = match Term::from_dictionary(&term.0) {
        Term::Iri(iri) => labels
            .and_then(|labels| labels.get(iri.as_ref()))
            .and_then(Option::as_deref),
        Term::BlankNode(_) | Term::Literal(_) => None,
    };
    let (label, qualifier, full_iri) = Term::from_dictionary(&term.0)
        .into_display(&target.prefixes)
        .into_structured();
    Cell {
        label,
        qualifier,
        annotation,
        href,
        full_iri,
        structured: true,
    }
}

impl Resource for Answer {
    fn to_json(&self) -> Bytes {
        json_body(self)
    }

    fn to_html(&self) -> String {
        let cells = self.cells();
        let rows: Vec<Vec<Value<'_>>> = cells
            .iter()
            .map(|row| row.iter().map(Cell::value).collect())
            .collect();
        let headers = self.headers();

        let completeness = self.completeness_text();
        let summary = self.summary(&completeness);
        let canonical = self.target.canonical();
        let heading = self.page_heading();
        let context = self.target.context();
        let alternate = if self.target.is_tpf() {
            Representation::NQuads
        } else {
            Representation::Json
        };
        operation_page_with_format(
            &self.target.mount,
            &heading,
            &context,
            &self.target.crumbs(),
            canonical.as_deref(),
            alternate,
            html! {
                @if let Some(identifier) = self.described_identifier() {
                    p."focus-identifier" { code { (identifier) } }
                }
                div."answer-summary" {
                    (fields(&summary))
                }
                @if let Some(form) = self.target.form() {
                    div."query-editor" { (form) }
                }
                @if !self.absent_terms.is_empty() {
                    (note(&format!(
                        "{}. The answer is empty for that reason, not because the pattern has \
                         no matches.",
                        absent_terms_text(&self.absent_terms)
                    )))
                }
                @if !self.completeness.is_complete()
                    && self.completeness.next_cursor().is_none() {
                    (note(
                        "This answer stopped at a budget and has no position to resume from; \
                         what is here is as much as one response allows."
                    ))
                }

                section."section-block" {
                    h2 { "Triples" }
                    @if self.vars.is_empty() && !self.bindings && self.fragment_pattern().is_none() {
                        (note(
                            "Every position is bound, so a row has nothing to report beyond its own \
                             existence; the cardinality above is the answer."
                        ))
                    } @else if rows.is_empty() {
                        (note("No rows."))
                    } @else {
                        (results_table(&headers, &rows))
                    }
                }

                @if let Some(token) = self.completeness.next_cursor() {
                    @if let Some(next) = self.target.next(token) {
                        (pager(&next, "Next page →"))
                    } @else {
                        p."note" {
                            "Put cursor " code { (token) }
                            " in the same JSON request body to fetch the next page."
                        }
                    }
                }
            },
        )
    }
}

impl Answer {
    /// The page's actual focus rather than its route name.
    fn page_heading(&self) -> String {
        match &self.echo {
            Echo::Fragment { .. } => "Triple pattern".to_owned(),
            Echo::BindingsFragment { .. } => "Bound triple pattern".to_owned(),
            Echo::Describe { resource, .. } => {
                self.described_label().unwrap_or(resource).to_owned()
            }
            Echo::Sample { .. } => "Sample".to_owned(),
        }
    }

    fn described_label(&self) -> Option<&str> {
        let Echo::Describe { .. } = &self.echo else {
            return None;
        };
        self.described
            .as_ref()
            .and_then(|text| self.page_labels.get(text))
            .map(String::as_str)
    }

    /// When the label is the heading, keep the request spelling immediately
    /// below it. Without a label the spelling is already the heading.
    fn described_identifier(&self) -> Option<&str> {
        let Echo::Describe { resource, .. } = &self.echo else {
            return None;
        };
        self.described_label().map(|_| resource.as_str())
    }

    fn fragment_pattern(&self) -> Option<&Pattern> {
        match &self.echo {
            Echo::Fragment { pattern, .. } => Some(pattern),
            _ => None,
        }
    }

    /// The fields above the table: what was asked, and how much of it came back.
    fn summary<'a>(&'a self, completeness: &'a str) -> Vec<(&'a str, Value<'a>)> {
        let mut summary = match &self.echo {
            Echo::Fragment { pattern, g } => {
                let mut fields = pattern_fields(pattern, self.target.operation);
                fields.extend(graph_field(g.as_deref()));
                fields
            }
            Echo::BindingsFragment { pattern, g } => {
                let mut fields = binding_pattern_fields(pattern, self.target.operation);
                fields.extend(graph_field(g.as_deref()));
                fields
            }
            Echo::Describe { direction, .. } => {
                vec![("direction", Value::Text(direction.as_str()))]
            }
            Echo::Sample { pattern, .. } => pattern_fields(pattern, self.target.operation),
        };
        summary.push(("cardinality", Value::Number(self.cardinality.value())));
        summary.push(("returned", Value::Number(self.rows.len() as u64)));
        if let Echo::Sample { n, seed, .. } = &self.echo {
            summary.push(("n", Value::Number(u64::from(*n))));
            summary.push(("seed", Value::Number(*seed)));
        }
        summary.push(("complete", Value::Text(completeness)));
        summary
    }

    /// Prefer a concrete number of remaining pages when this first page has
    /// an exact cardinality. A cursor can live in an offset, predicate-id,
    /// binding-row, or ranked space, so later pages deliberately retain the
    /// reason rather than pretending every cursor reveals how many rows came
    /// before it.
    fn completeness_text(&self) -> Cow<'static, str> {
        // A body-addressed bindings cursor lives in the JSON rather than in
        // `target.params`, so only a cursorless GET proves it is page one.
        let first_page = !self.target.body && self.target.params.get("cursor").is_none();
        let page_limit = self.completeness.truncation_reason() == Some(TruncationReason::PageLimit);
        let page_size = self.rows.len() as u64;
        if first_page && page_limit && self.cardinality.is_exact() && page_size > 0 {
            let remaining = self.cardinality.value().saturating_sub(page_size);
            let pages = remaining.div_ceil(page_size);
            let noun = if pages == 1 { "page" } else { "pages" };
            return Cow::Owned(format!("no — {pages} more {noun}"));
        }
        Cow::Borrowed(completeness_text(&self.completeness))
    }

    fn headers(&self) -> Vec<&str> {
        let mut headers: Vec<&str> = if self.fragment_pattern().is_some() {
            Position::ALL
                .iter()
                .map(|position| position.as_str())
                .collect()
        } else {
            self.vars
                .positions()
                .iter()
                .map(|position| position.as_str())
                .collect()
        };
        if self.vars.has_graph() {
            headers.push(GRAPH);
        }
        if self.bindings {
            headers.insert(0, BINDING);
        }
        if self.directed {
            headers.push("direction");
        }
        if self.rows.iter().any(|row| row.ranking.is_some()) {
            headers.extend([SCORE, MATCH_KIND]);
        }
        headers
    }

    /// Every cell of the table, owned, so the [`Value`]s below can borrow it.
    fn cells(&self) -> Vec<Vec<Cell<'_>>> {
        self.rows
            .iter()
            .map(|row| {
                let mut cells = Vec::new();
                if let Some(binding) = row.binding {
                    cells.push(Cell::text(binding.to_string()));
                }
                if let Some(pattern) = self.fragment_pattern() {
                    // JSON rows carry variables only. A browser page is a
                    // table of triples, so merge the request's bound terms
                    // back into their fixed positions for display. A bound term
                    // has one spelling — the request's, which for a blank node
                    // is already the scoped IRI, since nothing else resolves.
                    for position in Position::ALL {
                        let found = pattern
                            .bound(position)
                            .map(|bound| (bound.dictionary(), bound.dictionary()))
                            .or_else(|| {
                                row.cells
                                    .iter()
                                    .find(|(row_position, _)| *row_position == position)
                                    .map(|(_, term)| {
                                        (term.published.as_ref(), term.stored.as_ref())
                                    })
                            });
                        if let Some((published, stored)) = found {
                            cells.push(self.cell(published, stored));
                        }
                    }
                } else {
                    cells.extend(
                        row.cells
                            .iter()
                            .map(|(_, term)| self.cell(&term.published, &term.stored)),
                    );
                }
                if let Some(graph) = &row.graph {
                    cells.push(self.graph_cell(graph));
                }
                if let Some(direction) = row.direction {
                    cells.push(Cell::text(direction.as_str().to_owned()));
                }
                if let Some(ranking) = row.ranking {
                    cells.push(Cell::text(ranking.score.to_string()));
                    cells.push(Cell::text(ranking.kind.to_owned()));
                }
                cells
            })
            .collect()
    }

    /// A graph's name, linking to the same pattern scoped to that graph — the
    /// question a quad-view row invites.
    fn graph_cell<'a>(&'a self, graph: &'a str) -> Cell<'a> {
        let request = Term::from_dictionary(graph).to_request();
        let mut cell = term_cell(&self.target, &self.blank_nodes, graph, None);
        cell.href = Some(query(
            self.target.base(),
            &self
                .target
                .params
                .without("cursor")
                .without("format")
                .with(GRAPH, &request),
        ));
        cell
    }

    /// One term, and the request that asks about it.
    ///
    /// This is what makes the page a way *into* the data rather than a dump of
    /// it: a subject, predicate or object links to its own neighborhood, a
    /// literal to every triple carrying it.
    /// `published` is what the cell shows and links to; `stored` is the
    /// dictionary spelling the page's labels were resolved against.
    fn cell<'a>(&'a self, published: &'a str, stored: &'a str) -> Cell<'a> {
        let mut cell = term_cell(
            &self.target,
            &self.blank_nodes,
            published,
            self.page_labels.get(stored).map(String::as_str),
        );
        if self.described.as_deref() == Some(published) {
            cell.href = None;
        }
        cell
    }
}

/// The page's sentence about parameters that matched nothing.
fn absent_terms_text(absent: &[AbsentTerm]) -> String {
    absent
        .iter()
        .map(AbsentTerm::explanation)
        .collect::<Vec<_>>()
        .join("; ")
}

/// The one line a page says about completeness, honestly: the actual
/// truncation reason rather than a guess at it.
fn completeness_text(completeness: &Completeness) -> &'static str {
    match completeness.truncation_reason() {
        None => "yes",
        Some(TruncationReason::PageLimit) => "no — the page filled",
        Some(TruncationReason::TimeBudget) => "no — the time budget expired",
        Some(TruncationReason::CandidateBudget) => {
            "no — the candidate budget was spent before the scan finished"
        }
        Some(TruncationReason::ResponseBytes) => "no — the response byte budget filled",
        Some(TruncationReason::CellOverflow) => "no — a cell overflowed its cap",
        Some(TruncationReason::PartialFailure) => "no — part of the request failed",
    }
}

impl Resource for GraphsAnswer {
    fn to_json(&self) -> Bytes {
        json_body(self)
    }

    fn to_html(&self) -> String {
        let cells: Vec<(Cell<'_>, u64)> = self
            .graphs
            .iter()
            .map(|entry| {
                let request = Term::from_dictionary(&entry.published).to_request();
                let mut cell = term_cell(&self.target, &self.blank_nodes, &entry.published, None);
                cell.href = Some(self.target.ask("fragment", GRAPH, &request));
                (cell, entry.count)
            })
            .collect();
        let rows: Vec<Vec<Value<'_>>> = cells
            .iter()
            .map(|(cell, count)| vec![cell.value(), Value::Number(*count)])
            .collect();
        let summary = [
            ("triples", Value::Number(self.triples)),
            ("memberships", Value::Number(self.memberships)),
            ("graphs", Value::Number(self.cardinality.value())),
            ("returned", Value::Number(self.graphs.len() as u64)),
            (
                "complete",
                Value::Text(completeness_text(&self.completeness)),
            ),
        ];
        let canonical = self.target.canonical();
        let context = self.target.context();
        operation_page(
            &self.target.mount,
            "Graphs",
            &context,
            &self.target.crumbs(),
            canonical.as_deref(),
            html! {
                div."answer-summary" {
                    (fields(&summary))
                }
                (note(
                    "Every graph the bundle's triples belong to, with the number of triples in \
                     each. The unnamed graph holds the statements that carried no graph; the \
                     union of all graphs is what an unscoped request reads, and a triple in \
                     several graphs counts once there. Each graph links to its triples."
                ))
                section."section-block" {
                    h2 { "Graphs" }
                    @if rows.is_empty() {
                        (note("No graphs."))
                    } @else {
                        (results_table(&[GRAPH, "triples"], &rows))
                    }
                }
                @if let Some(token) = self.completeness.next_cursor() {
                    @if let Some(next) = self.target.next(token) {
                        (pager(&next, "Next page →"))
                    }
                }
            },
        )
    }
}

impl Resource for SearchAnswer {
    fn to_json(&self) -> Bytes {
        json_body(self)
    }

    fn to_html(&self) -> String {
        let subjects: Vec<_> = self
            .results
            .iter()
            .map(|result| {
                term_cell(
                    &self.target,
                    &self.blank_nodes,
                    &result.subject,
                    result.label.as_ref().and_then(Option::as_deref),
                )
            })
            .collect();
        let predicates: Vec<_> = self
            .results
            .iter()
            .map(|result| {
                term_cell(
                    &self.target,
                    &self.blank_nodes,
                    &result.evidence.predicate,
                    None,
                )
            })
            .collect();
        let scores: Vec<String> = self
            .results
            .iter()
            .map(|result| result.ranking.score.to_string())
            .collect();
        let literals: Vec<String> = self
            .results
            .iter()
            .map(
                |result| match Term::from_dictionary(&result.evidence.literal) {
                    Term::Literal(literal) => literal.value().to_owned(),
                    _ => result.evidence.literal.to_string(),
                },
            )
            .collect();
        let rows: Vec<Vec<Value<'_>>> = self
            .results
            .iter()
            .enumerate()
            .map(|(index, result)| {
                vec![
                    subjects[index].value(),
                    predicates[index].value(),
                    Value::Text(&literals[index]),
                    Value::Text(result.ranking.kind),
                    Value::Text(&scores[index]),
                ]
            })
            .collect();
        let headers = ["subject", "predicate", "literal", MATCH_KIND, SCORE];
        let roles = self.roles.join(", ");
        let predicate_scope = self
            .predicates
            .iter()
            .map(|predicate| {
                Term::from_dictionary(predicate)
                    .into_display(&self.target.prefixes)
                    .into_parts()
                    .0
            })
            .collect::<Vec<_>>()
            .join(", ");
        let all_predicates = self.roles.is_empty() && self.predicates.is_empty();
        let returned = self.results.len() as u64;
        let canonical = self.target.canonical();
        let heading = format!("“{}”", self.query);
        let context = self.target.context();
        operation_page(
            &self.target.mount,
            &heading,
            &context,
            &self.target.crumbs(),
            canonical.as_deref(),
            html! {
                div."answer-summary" {
                    (fields(&[
                        ("query", Value::Text(&self.query)),
                        ("scope", if all_predicates { Value::Text("all predicates") } else { Value::Absent }),
                        ("roles", if roles.is_empty() { Value::Absent } else { Value::Text(&roles) }),
                        ("predicates", if predicate_scope.is_empty() { Value::Absent } else { Value::Code(&predicate_scope) }),
                        ("returned", Value::Number(returned)),
                        ("complete", Value::Text(completeness_text(&self.completeness))),
                    ]))
                }
                @if let Some(form) = self.target.form() {
                    div."query-editor" { (form) }
                }
                @if !self.completeness.is_complete() {
                    (note(
                        "Ranked search retains a bounded candidate window and has no cursor; \
                         narrow the query or its scopes to see what this response could not carry."
                    ))
                }
                section."section-block" {
                    h2 { "Entities" }
                    @if rows.is_empty() {
                        (note("No matching entities."))
                    } @else {
                        (results_table(&headers, &rows))
                    }
                }
            },
        )
    }
}

impl TermsPage {
    /// The same scan, counted instead of paged.
    ///
    /// Built from the two parameters that determine the answer rather than from
    /// the request's own, because a count refuses the page-shaped ones.
    fn counted(&self) -> String {
        format!(
            "{}?prefix={}&role={}&count=true",
            self.target
                .mount
                .operation(&self.target.id.dataset, &self.target.id.version, "terms"),
            url::encode_value(&self.prefix),
            self.role,
        )
    }
}

impl Resource for TermsPage {
    fn to_json(&self) -> Bytes {
        json_body(self)
    }

    fn to_html(&self) -> String {
        let cells: Vec<Cell<'_>> = self
            .terms
            .iter()
            .map(|row| {
                // Every page links a term the same way, predicates included: a
                // predicate is usually a subject too, carrying its label and its
                // definition, and `roles` cannot say otherwise — it reports the
                // sections *this scan read*, so a `role=predicate` scan calls
                // every row a predicate whether or not the term is described
                // elsewhere in the graph.
                term_cell(
                    &self.target,
                    &self.blank_nodes,
                    &row.published,
                    row.label.as_ref().and_then(Option::as_deref),
                )
            })
            .collect();
        let roles: Vec<String> = self.terms.iter().map(|row| row.roles.join(", ")).collect();
        let rows: Vec<Vec<Value<'_>>> = cells
            .iter()
            .zip(&roles)
            .map(|(cell, roles)| vec![cell.value(), Value::Text(roles)])
            .collect();

        let returned = self.terms.len() as u64;
        let canonical = self.target.canonical();
        let context = self.target.context();
        let heading = if self.prefix.is_empty() {
            "Every term".to_owned()
        } else {
            format!("“{}…”", self.prefix)
        };
        let counted = self.counted();
        operation_page(
            &self.target.mount,
            &heading,
            &context,
            &self.target.crumbs(),
            canonical.as_deref(),
            html! {
                div."answer-summary" {
                    (fields(&[
                        ("prefix", if self.prefix.is_empty() { Value::Text("(none)") } else { Value::Code(&self.prefix) }),
                        ("role", Value::Text(self.role)),
                        ("matching", Value::Number(self.cardinality.value())),
                        ("returned", Value::Number(returned)),
                        ("complete", Value::Text(completeness_text(&self.completeness))),
                    ]))
                }
                @if let Some(form) = self.target.form() {
                    div."query-editor" { (form) }
                }
                section."section-block" {
                    h2 { "Terms" }
                    @if rows.is_empty() {
                        (note("No term in this role starts with that prefix."))
                    } @else {
                        (results_table(&["term", "roles"], &rows))
                    }
                }
                (pager(&counted, "How many in total? →"))
                @if let Some(token) = self.completeness.next_cursor() {
                    @if let Some(next) = self.target.next(token) {
                        (pager(&next, "Next page →"))
                    }
                }
            },
        )
    }
}

impl Resource for TermsCount {
    fn to_json(&self) -> Bytes {
        json_body(self)
    }

    fn to_html(&self) -> String {
        let canonical = self.target.canonical();
        let context = self.target.context();
        let heading = if self.prefix.is_empty() {
            "Every term".to_owned()
        } else {
            format!("“{}…”", self.prefix)
        };
        let listed = query(
            self.target
                .mount
                .operation(&self.target.id.dataset, &self.target.id.version, "terms"),
            &self.target.params.without("count"),
        );
        operation_page(
            &self.target.mount,
            &heading,
            &context,
            &self.target.crumbs(),
            canonical.as_deref(),
            html! {
                div."answer-summary" {
                    (fields(&[
                        ("prefix", if self.prefix.is_empty() { Value::Text("(none)") } else { Value::Code(&self.prefix) }),
                        ("role", Value::Text(self.role)),
                        ("count", Value::Number(self.count.value())),
                        ("exact", Value::Text("yes")),
                    ]))
                }
                @if let Some(form) = self.target.form() {
                    div."query-editor" { (form) }
                }
                section."section-block" {
                    h2 { "By position" }
                    (results_table(
                        &["position", "terms"],
                        &[
                            vec![Value::Text("subject"), Value::Number(self.counts.subject)],
                            vec![Value::Text("predicate"), Value::Number(self.counts.predicate)],
                            vec![Value::Text("object"), Value::Number(self.counts.object)],
                            vec![Value::Text("any"), Value::Number(self.counts.any)],
                        ],
                    ))
                    (note(
                        "`any` deduplicates rather than adding up: a term stored as both a \
                         subject and an object is one term, and a predicate may repeat either."
                    ))
                }
                (note(
                    "Two binary searches bracket a sorted dictionary section, so these numbers \
                     cost the same whether they are nought or a million — which is what makes \
                     this worth asking across a federation before asking for anything else."
                ))
                (pager(&listed, "The terms themselves →"))
            },
        )
    }
}

impl Resource for LabelsAnswer {
    fn to_json(&self) -> Bytes {
        json_body(self)
    }

    fn to_html(&self) -> String {
        let rows: Vec<Vec<Value<'_>>> = self
            .labels
            .iter()
            .map(|result| {
                vec![
                    Value::Code(&result.iri),
                    result.label.as_deref().map_or(Value::Absent, Value::Text),
                ]
            })
            .collect();
        let returned = self.labels.len() as u64;
        page(
            &self.target.mount,
            &self.target.title(),
            &self.target.crumbs(),
            None,
            html! {
                div."answer-summary" {
                    (fields(&[
                        ("returned", Value::Number(returned)),
                        ("complete", Value::Text(completeness_text(&self.completeness))),
                    ]))
                }
                section."section-block" {
                    h2 { "Labels" }
                    @if rows.is_empty() {
                        (note("No IRIs were submitted."))
                    } @else {
                        (results_table(&["iri", "label"], &rows))
                    }
                }
            },
        )
    }
}

impl Resource for CountAnswer {
    fn to_json(&self) -> Bytes {
        json_body(self)
    }

    fn to_html(&self) -> String {
        let mut summary = pattern_fields(&self.pattern, self.target.operation);
        summary.extend(graph_field(self.g.as_deref()));
        summary.push(("count", Value::Number(self.count.value())));
        summary.push((
            "exact",
            Value::Text(if self.count.is_exact() { "yes" } else { "no" }),
        ));

        let canonical = self.target.canonical();
        let context = self.target.context();
        operation_page(
            &self.target.mount,
            "Pattern count",
            &context,
            &self.target.crumbs(),
            canonical.as_deref(),
            html! {
                div."answer-summary" {
                    (fields(&summary))
                }
                @if let Some(form) = self.target.form() {
                    div."query-editor" { (form) }
                }
                @if !self.absent_terms.is_empty() {
                    (note(&format!(
                        "{}, so nothing can match.",
                        absent_terms_text(&self.absent_terms)
                    )))
                }
                @if self.pattern.text().is_none() {
                    (note(
                        "A plain pattern's count is exact and costs a bounded descent rather than an \
                         enumeration, which is what makes it worth asking before /fragment."
                    ))
                } @else {
                    (note(
                        "A text count scans a bounded window of matching literals. Continue from the \
                         cursor until the count is exact."
                    ))
                }
                (pager(
                    &query(
                        self.target.mount.operation(&self.target.id.dataset, &self.target.id.version, "fragment"),
                        &self.target.params.without("cursor"),
                    ),
                    "The rows themselves →",
                ))
                @if let Some(token) = self.completeness.next_cursor() {
                    @if let Some(next) = self.target.next(token) {
                        (pager(&next, "Continue counting →"))
                    }
                }
            },
        )
    }
}

impl Resource for BindingCountAnswer {
    fn to_json(&self) -> Bytes {
        json_body(self)
    }

    fn to_html(&self) -> String {
        let rows: Vec<Vec<Value<'_>>> = self
            .counts
            .iter()
            .map(|item| {
                vec![
                    Value::Number(u64::from(item.binding)),
                    Value::Number(item.count.value()),
                    Value::Text(if item.count.is_exact() { "yes" } else { "no" }),
                ]
            })
            .collect();
        let canonical = self.target.canonical();
        page(
            &self.target.mount,
            &self.target.title(),
            &self.target.crumbs(),
            canonical.as_deref(),
            html! {
                div."answer-summary" {
                    (fields(&{
                        let mut fields = binding_pattern_fields(&self.pattern, self.target.operation);
                        fields.extend(graph_field(self.g.as_deref()));
                        fields
                    }))
                }
                section."section-block" {
                    h2 { "Counts" }
                    @if rows.is_empty() {
                        (note("No input binding rows."))
                    } @else {
                        (results_table(&[BINDING, "count", "exact"], &rows))
                    }
                }
            },
        )
    }
}

/// The `g` field, when the request scoped its pattern.
fn graph_field(requested: Option<&str>) -> Option<(&str, Value<'_>)> {
    requested.map(|g| (GRAPH, Value::Code(g)))
}

/// The three pattern positions, as page fields.
fn pattern_fields(pattern: &Pattern, operation: AccessOperation) -> Vec<(&str, Value<'_>)> {
    let mut fields: Vec<_> = Position::ALL
        .into_iter()
        .map(|position| {
            (
                pattern_parameter(position, operation),
                pattern
                    .bound(position)
                    .map_or(Value::Text("(any)"), |term| Value::Code(term.requested())),
            )
        })
        .collect();
    if let Some(text) = pattern.text() {
        fields.push(("o.text", Value::Code(text.query())));
    }
    fields
}

fn binding_pattern_fields(
    pattern: &BindingPattern,
    operation: AccessOperation,
) -> Vec<(&str, Value<'_>)> {
    Position::ALL
        .into_iter()
        .map(|position| {
            (
                pattern_parameter(position, operation),
                Value::Code(pattern.requested(position)),
            )
        })
        .collect()
}

fn pattern_parameter(position: Position, operation: AccessOperation) -> &'static str {
    if operation == AccessOperation::Tpf {
        position.tpf_parameter()
    } else {
        position.as_str()
    }
}

/// Render one RDF term with this release's prefix map and the same drill-down
/// link semantics on every operation page.
///
/// A named term — subject, predicate or object alike — links to its own
/// `/describe` neighborhood; a literal links to every triple carrying it. A
/// predicate used to link to `/fragment?p=`, but the page a reader wants from
/// a predicate is what the term *is*, and its usage is one link further.
/// One term, and the request that asks about it.
///
/// A blank node is shown as `_:{section}-{local-id}` rather than as the scoped
/// IRI it actually is. `_:` is the one spelling every RDF reader recognizes
/// without a legend, and the tail is the canonical identity rather than the
/// parser-local label — but it is a *display* form: nothing expands it and no
/// parameter accepts it, so the link and the tooltip carry the full IRI, which
/// is the spelling that works.
fn term_cell<'a>(
    target: &Target,
    blank_nodes: &SkolemScope,
    text: &'a str,
    annotation: Option<&'a str>,
) -> Cell<'a> {
    let term = Term::from_dictionary(text);
    let request = term.to_request();
    let href = match &term {
        Term::Literal(_) => target.ask("fragment", "o", &request),
        _ => target.ask("describe", "iri", &request),
    };
    let (label, qualifier, full_iri) = match blank_nodes.display_label(text) {
        Some(suffix) => (format!("_:{suffix}"), None, Some(Cow::Borrowed(text))),
        None => term.into_display(&target.prefixes).into_structured(),
    };
    Cell {
        label,
        qualifier,
        annotation,
        href: Some(href),
        full_iri,
        structured: true,
    }
}

/// A rendered table cell, held so the borrowed [`Value`] can point at it.
struct Cell<'a> {
    label: String,
    qualifier: Option<String>,
    annotation: Option<&'a str>,
    href: Option<String>,
    full_iri: Option<Cow<'a, str>>,
    structured: bool,
}

impl<'a> Cell<'a> {
    /// A plain unlinked cell: a binding index, a direction, a score.
    fn text(label: String) -> Self {
        Self {
            label,
            qualifier: None,
            annotation: None,
            href: None,
            full_iri: None,
            structured: false,
        }
    }

    fn value(&self) -> Value<'_> {
        match &self.href {
            Some(href) => Value::TermLink {
                href: href.clone(),
                term: TermText {
                    primary: &self.label,
                    qualifier: self.qualifier.as_deref(),
                    annotation: self.annotation,
                    full_iri: self.full_iri.as_deref(),
                },
            },
            None if self.structured => Value::Term {
                term: TermText {
                    primary: &self.label,
                    qualifier: self.qualifier.as_deref(),
                    annotation: self.annotation,
                    full_iri: self.full_iri.as_deref(),
                },
            },
            None => Value::Text(&self.label),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_cardinalities_cannot_overflow_the_wire_integer() {
        assert_eq!(
            exact_cardinality_sum([2, 3]).unwrap().value(),
            5,
            "ordinary phase counts still add exactly"
        );
        let overflow = exact_cardinality_sum([u64::MAX, 1]).unwrap_err();
        assert_eq!(overflow.code(), ErrorCode::InternalError);
    }

    #[test]
    fn a_published_blank_node_is_a_named_node_in_rdf() {
        // The materializer is what substitutes the scoped IRI, so by the time
        // the RDF serializer sees a term there is no blank node left to keep.
        let counts = kgf_store::dict::DictCounts {
            shared: 1,
            subjects: 0,
            objects: 0,
            predicates: 0,
        };
        let blank_nodes = SkolemScope::new([0xab; 32], counts);
        let published = blank_nodes.iri(Role::Subject, TermId(1), "_:b1").unwrap();

        match rdf_subject(published.as_bytes()).unwrap() {
            NamedOrBlankNode::NamedNode(node) => assert_eq!(node.as_str(), published),
            NamedOrBlankNode::BlankNode(_) => panic!("fragment data kept a local blank node"),
        }
        match rdf_object(published.as_bytes()).unwrap() {
            RdfTerm::NamedNode(node) => assert_eq!(node.as_str(), published),
            other => panic!("fragment data became {other:?} rather than a named node"),
        }
        // And the page spells that IRI back as a blank node for a reader.
        assert_eq!(blank_nodes.display_label(&published), Some("sh-1"));
    }

    #[test]
    fn a_row_weighs_exactly_what_it_serializes() {
        // `Row::new` counts what `Serialize` will write instead of writing it,
        // which is the one place in the byte budget where two pieces of code
        // have to agree about the same bytes. This is that agreement, over
        // every shape a row can take: each width, each term kind, and with and
        // without the two extra columns — `/describe`'s side and `o.text`'s
        // score, the latter being the field whose length has to be formatted to
        // be known.
        let terms = [
            "http://example.org/a",
            "_:b1",
            "\"plain\"",
            "\"tagged\"@en-gb",
            "\"42\"^^<http://www.w3.org/2001/XMLSchema#integer>",
            // The escapes, which are where a byte count is most likely to be
            // wrong: a quote and a backslash double, and a control character
            // becomes six.
            "\"a \\\"quoted\\\" \tvalue\"",
            "\"a Ünicode ☃ value\"",
        ];

        let mut shapes = 0;
        for width in 0..=Position::ALL.len() {
            for term in terms {
                for direction in [None, Some(Direction::Out), Some(Direction::In)] {
                    // Scores that format to different lengths, including the
                    // integral one `serde_json` writes as `14.0` and the long
                    // decimal a BM25 score actually is.
                    let rankings = [
                        None,
                        Some(Ranking {
                            score: 0.0,
                            kind: "exact",
                        }),
                        Some(Ranking {
                            score: 14.0,
                            kind: "exact",
                        }),
                        Some(Ranking {
                            score: 1.0 / 3.0,
                            kind: "stemmed",
                        }),
                        Some(Ranking {
                            score: -0.5,
                            kind: "stemmed",
                        }),
                    ];
                    for score in rankings {
                        for binding in [None, Some(0), Some(12_345)] {
                            let cells: Vec<(Position, RowTerm)> = Position::ALL[..width]
                                .iter()
                                .map(|position| {
                                    let text: Rc<str> = Rc::from(term);
                                    (
                                        *position,
                                        RowTerm {
                                            published: Rc::clone(&text),
                                            stored: text,
                                        },
                                    )
                                })
                                .collect();
                            // What the cache would have measured for each cell.
                            let each = serde_json::to_vec(&Term::from_dictionary(term))
                                .expect("a term serializes")
                                .len() as u64;

                            let row = Row::new(
                                cells,
                                each * width as u64,
                                None,
                                binding,
                                direction,
                                score,
                            );
                            assert_eq!(
                                row.serialized,
                                serde_json::to_vec(&row).expect("a row serializes").len() as u64,
                                "width {width}, {term:?}, {binding:?}, {direction:?}, {score:?}"
                            );
                            shapes += 1;
                        }
                    }
                }
            }
        }
        assert!(shapes >= 80, "{shapes} shapes");
    }

    #[test]
    fn search_and_label_rows_weigh_exactly_what_they_serialize() {
        let all_controls: String = (0..=0x1f).filter_map(char::from_u32).collect();
        for value in [
            "",
            "plain ASCII",
            "quote: \"; slash: \\; solidus: /",
            &all_controls,
            "Ünicode 💩 \u{2028}",
        ] {
            assert_eq!(
                serialized_json_string(value),
                serde_json::to_vec(value).unwrap().len() as u64,
                "{value:?}"
            );
        }

        let subject: Rc<str> = Rc::from("http://example.org/Ünicode");
        let subject_serialized = serde_json::to_vec(&Term::from_dictionary(&subject))
            .expect("a subject serializes")
            .len() as u64;
        let literals = [
            "\"plain\"",
            "\"tagged\"@en-gb",
            "\"42\"^^<http://www.w3.org/2001/XMLSchema#integer>",
            "\"a \\\"quoted\\\" \\tvalue\"",
        ];
        let labels = [
            None,
            Some(None),
            Some(Some("a \"quoted\"\nÜnicode label".to_owned())),
        ];

        for literal in literals {
            for label in &labels {
                for score in [0.0, 14.0, 1.0 / 3.0, f32::NAN, f32::INFINITY] {
                    let result = SearchResult::new(
                        Rc::clone(&subject),
                        subject_serialized,
                        label.clone(),
                        Rc::from("http://example.org/predicate,one"),
                        Rc::from(literal),
                        Ranking {
                            score,
                            kind: "normalized",
                        },
                    );
                    assert_eq!(
                        result.serialized,
                        serde_json::to_vec(&result).unwrap().len() as u64,
                        "{literal:?}, {label:?}, {score}"
                    );
                }
            }
        }

        for label in [None, Some("a \"quoted\"\nÜnicode label".to_owned())] {
            let result = LabelResult::new("http://example.org/Ünicode".to_owned(), label);
            assert_eq!(
                result.serialized,
                serde_json::to_vec(&result).unwrap().len() as u64
            );
        }
    }

    #[test]
    fn a_draw_is_uniform_deterministic_and_free_of_repeats() {
        // The sampling contract: the same seed and version draw the same
        // members. Everything else here is what makes that draw worth having.
        let dense = sample_positions(10, 4, 42);
        assert_eq!(dense, sample_positions(10, 4, 42));
        assert_ne!(dense, sample_positions(10, 4, 43));
        assert_eq!(dense.len(), 4);

        for (count, n) in [(10u64, 4u64), (1_000, 25), (1_000_000, 1_000), (3, 2)] {
            let positions = sample_positions(count, n, 7);
            assert_eq!(positions.len() as u64, n, "{count}/{n}");
            assert!(
                positions.windows(2).all(|pair| pair[0] < pair[1]),
                "sorted and distinct"
            );
            assert!(positions.iter().all(|position| *position < count));
        }

        // Degenerate sizes are members of the same rule, not special cases.
        assert_eq!(sample_positions(0, 10, 1), Vec::<u64>::new());
        assert_eq!(sample_positions(3, 10, 1), vec![0, 1, 2]);
        assert_eq!(sample_positions(3, 3, 1), vec![0, 1, 2]);
        assert_eq!(sample_positions(10, 0, 1), Vec::<u64>::new());
    }

    #[test]
    fn the_generator_covers_its_range_without_favouring_the_front() {
        // `% bound` would bias towards small values whenever `bound` does not
        // divide 2⁶⁴ — which for a sample means over-reporting the front of
        // the result set, the exact thing a sample exists to avoid.
        let mut random = SplitMix64::seeded(0);
        let mut buckets = [0u32; 7];
        for _ in 0..70_000 {
            buckets[random.below(7) as usize] += 1;
        }
        for count in buckets {
            assert!(
                (9_000..11_000).contains(&count),
                "uneven draw over 7 buckets: {buckets:?}"
            );
        }

        // A bound of one has one answer, and a power of two rejects nothing.
        assert_eq!(random.below(1), 0);
        assert!(random.below(2) < 2);
    }
}
