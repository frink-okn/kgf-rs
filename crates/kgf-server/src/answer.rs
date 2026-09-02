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
use oxrdf::{BlankNode, Literal, NamedNode, NamedOrBlankNode, Term as RdfTerm, Triple};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

use hdtc::format::{TextScanPosition, TextSearcher, parse_literal};
use kgf_store::catalog::BundleId;
use kgf_store::dict::Dictionary;
use kgf_store::pattern::{IdPattern, Selection};
use kgf_store::{
    ClassPropertyStop, ClassRelationStop, IdTriple, Role, SchemaCollection,
    SchemaCounts as StoreSchemaCounts, SchemaNode as StoreSchemaNode, SchemaNodeKind, StatsView,
    Store, TermId,
};

use crate::cursor::{Cursor, CursorBinding, CursorToken, PositionSpace, StaleCursor};
use crate::envelope::{
    BudgetReason, Cardinality, Completeness, ErrorCode, Problem, TruncationReason,
};
use crate::forms;
use crate::html::{
    Crumb, Resource, TermText, Value, fields, group_digits, json_body, note, operation_page,
    operation_page_with_format, page, pager, results_table, stats, table,
};
use crate::rdf::{GraphFormat, serialize_graph};
use crate::representation::Representation;
use crate::request::{
    self, BindingPattern, BindingRow, BoundTerm, Candidates, Direction, Pattern, Position,
    ResponseBytes, SchemaChildren, SchemaQuery, SchemaSelection, TextFilter,
};
use crate::skolem::SkolemScope;
use crate::term::{LiteralKind, PrefixMap, Term, TermCache};
use crate::url::{self, Params};

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
    operation: &'static str,
    params: Params,
    prefixes: PrefixMap,
    body: bool,
    has_search: bool,
    /// The exact absolute GET URL received over HTTP. Hydra metadata keys its
    /// page controls by this IRI, so a merely equivalent canonical URL is not
    /// enough for an LDF client looking up controls for its request URL.
    request_url: Option<String>,
}

impl Target {
    /// The version and operation a request addressed, with its parameters and
    /// the version's immutable prefix map for human-facing result labels.
    pub fn new(id: BundleId, operation: &'static str, params: Params, prefixes: PrefixMap) -> Self {
        Self::get(id, operation, params, prefixes, false, None)
    }

    /// A GET target with the release capabilities its page may expose.
    pub(crate) fn get(
        id: BundleId,
        operation: &'static str,
        params: Params,
        prefixes: PrefixMap,
        has_search: bool,
        request_url: Option<String>,
    ) -> Self {
        Self {
            id,
            operation,
            params,
            prefixes,
            body: false,
            has_search,
            request_url,
        }
    }

    /// A body-addressed operation, whose request cannot be reconstructed as a link.
    pub fn body(
        id: BundleId,
        operation: &'static str,
        params: Params,
        prefixes: PrefixMap,
    ) -> Self {
        Self {
            id,
            operation,
            params,
            prefixes,
            body: true,
            has_search: false,
            request_url: None,
        }
    }

    /// The bundle version to open.
    pub fn id(&self) -> &BundleId {
        &self.id
    }

    fn base(&self) -> String {
        url::operation(&self.id.dataset, &self.id.version, self.operation)
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

    /// This response's URL, with the representation selector removed.
    ///
    /// [`page`] appends `format=json` to build the footer link, and a URL that
    /// already carried `format=html` would come back with the parameter twice —
    /// which this server's own parser refuses. Dropping it is also the more
    /// honest reading of "canonical": one resource, several representations.
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
            url::operation(&self.id.dataset, &self.id.version, operation),
            url::encode_value(value)
        )
    }

    fn crumbs(&self) -> Vec<Crumb<'_>> {
        vec![
            Crumb::to(&self.id.dataset, url::dataset(&self.id.dataset)),
            // There is no landing page for a version, so the version step goes
            // to its manifest, the version's canonical descriptive document.
            Crumb::to(
                &self.id.version,
                url::operation(&self.id.dataset, &self.id.version, "manifest"),
            ),
            Crumb::here(self.operation),
        ]
    }

    fn title(&self) -> String {
        format!(
            "{} — {} {}",
            self.operation, self.id.dataset, self.id.version
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
            "fragment" => "Fragment",
            "count" => "Count",
            "describe" => "Describe",
            "sample" => "Sample",
            "search" => "Search",
            "schema" => "Schema",
            "labels" => "Labels",
            operation => operation,
        }
    }

    /// The GET editor for this answer, absent for a body-addressed request.
    fn form(&self) -> Option<maud::Markup> {
        if self.body {
            None
        } else {
            forms::operation_form(
                &self.id.dataset,
                &self.id.version,
                self.operation,
                &self.params,
                self.has_search,
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

/// One result row: a term per variable, and for `/describe` which side of the
/// neighborhood it came from.
#[derive(Debug, Clone)]
pub struct Row {
    cells: Vec<(Position, Rc<str>)>,
    triple: IdTriple,
    binding: Option<u32>,
    direction: Option<Direction>,
    ranking: Option<Ranking>,
    serialized: u64,
}

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
        for (position, text) in &self.cells {
            map.serialize_entry(position.as_str(), &Term::from_dictionary(text))?;
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
        cells: Vec<(Position, Rc<str>)>,
        terms: u64,
        triple: IdTriple,
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
            triple,
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
    },
    BindingsFragment {
        pattern: BindingPattern,
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
    absent_terms: Vec<&'static str>,
    vars: Vec<Position>,
    rows: Vec<Row>,
    #[serde(skip)]
    row_resumes: Vec<RowResume>,
    #[serde(skip)]
    row_binding: Option<CursorBinding>,
    /// Cardinality of the distinct RDF projection. The native binding
    /// relation keeps its independently exact row count in `cardinality`.
    #[serde(skip)]
    rdf_cardinality: Option<Cardinality>,
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
}

impl Renders for Answer {
    fn render(mut self, representation: Representation) -> Result<Rendered, Problem> {
        let body = match representation {
            Representation::Turtle => self.fit_fragment_rdf(GraphFormat::Turtle)?,
            Representation::JsonLd => self.fit_fragment_rdf(GraphFormat::JsonLd)?,
            _ => standard_body(&self, representation),
        };
        Ok(Rendered {
            body,
            completeness: self.completeness,
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
        if let Echo::Fragment { pattern } | Echo::Sample { pattern, .. } = &self.echo {
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
            for (_, text) in &row.cells {
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
            let Some(subject) = dictionary
                .locate(Role::Subject, text.as_bytes())
                .map_err(|error| unreadable("looking a term up", &error))?
            else {
                continue;
            };
            if let Some(label) =
                preferred_label(store, &dictionary, &mut cache, subject.0, &predicates)?
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
const VOID_DATASET: &str = "http://rdfs.org/ns/void#Dataset";
const HYDRA_SEARCH: &str = "http://www.w3.org/ns/hydra/core#search";
const HYDRA_TEMPLATE: &str = "http://www.w3.org/ns/hydra/core#template";
const HYDRA_MAPPING: &str = "http://www.w3.org/ns/hydra/core#mapping";
const HYDRA_VARIABLE: &str = "http://www.w3.org/ns/hydra/core#variable";
const HYDRA_PROPERTY: &str = "http://www.w3.org/ns/hydra/core#property";
const HYDRA_TOTAL_ITEMS: &str = "http://www.w3.org/ns/hydra/core#totalItems";
const HYDRA_NEXT: &str = "http://www.w3.org/ns/hydra/core#next";

impl Answer {
    /// Fit a finished RDF document, Hydra metadata included, to the response
    /// budget. One indivisible row may exceed the budget so a client can
    /// always advance past it.
    ///
    /// Worst case is `Z·(1 + log limit)` serialized bytes: one complete
    /// candidate document plus bounded complete-prefix probes. RDF fragments
    /// are therefore admitted as heavy work.
    fn fit_fragment_rdf(&mut self, format: GraphFormat) -> Result<Bytes, Problem> {
        let body = self.fragment_rdf(format)?;
        if body.len() as u64 <= self.byte_budget || self.rows.len() <= 1 {
            return Ok(body);
        }

        let total = self.rows.len();
        let encode_prefix = |keep: usize| {
            let next = self.rdf_row_cursor(keep)?;
            self.fragment_rdf_prefix(format, keep, Some(next.as_str()))
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

    /// Project an ordinary fragment page into the TPF RDF graph understood by
    /// stock LDF clients. Native binding answers are body-addressed and never
    /// negotiate this representation; brTPF's GET transport joins this path
    /// once its `values=` request parser has normalized the restrictions.
    fn fragment_rdf(&self, format: GraphFormat) -> Result<Bytes, Problem> {
        self.fragment_rdf_prefix(format, self.rows.len(), self.completeness.next_cursor())
    }

    fn fragment_rdf_prefix(
        &self,
        format: GraphFormat,
        keep: usize,
        next_cursor: Option<&str>,
    ) -> Result<Bytes, Problem> {
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
        let mut triples = Vec::with_capacity(keep.saturating_add(18));
        let mut data = HashSet::with_capacity(keep);
        for row in self.rows.iter().take(keep) {
            let cell = |position| {
                let bound = match &self.echo {
                    Echo::Fragment { pattern } => pattern.bound(position),
                    Echo::BindingsFragment { pattern } => pattern.bound(position),
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
            let triple = Triple::new(
                rdf_fragment_subject(
                    subject.as_bytes(),
                    TermId(row.triple.subject),
                    &self.blank_nodes,
                )?,
                NamedNode::new(predicate)
                    .map_err(|error| unreadable("parsing an RDF predicate IRI", &error))?,
                rdf_fragment_object(
                    object.as_bytes(),
                    TermId(row.triple.object),
                    &self.blank_nodes,
                )?,
            );
            if data.insert(triple.clone()) {
                triples.push(triple);
            }
        }

        let page = metadata_iri(
            self.target.request_url.as_deref().ok_or_else(|| {
                Problem::new(
                    ErrorCode::InternalError,
                    "an RDF fragment response needs the absolute request URL",
                )
            })?,
            "the fragment page URL",
        )?;
        let dataset = metadata_iri(
            &self.target.absolute_base().ok_or_else(|| {
                Problem::new(
                    ErrorCode::InternalError,
                    "an RDF fragment response needs the request origin",
                )
            })?,
            "the fragment dataset URL",
        )?;

        // Metadata blank-node labels are deterministic because strong ETags
        // require stable bytes. Data blank nodes have already become stable
        // HDT-scoped IRIs and therefore cannot merge with these local nodes.
        let mut used = HashSet::new();
        let search = metadata_blank_node("kgf-hydra-search", &mut used);
        let mappings = [
            (
                "s",
                RDF_SUBJECT,
                metadata_blank_node("kgf-hydra-s", &mut used),
            ),
            (
                "p",
                RDF_PREDICATE,
                metadata_blank_node("kgf-hydra-p", &mut used),
            ),
            (
                "o",
                RDF_OBJECT,
                metadata_blank_node("kgf-hydra-o", &mut used),
            ),
        ];

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
            Literal::new_simple_literal(format!("{}{{?s,p,o}}", dataset.as_str())),
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
        if let Some(cursor) = next_cursor {
            triples.push(Triple::new(
                page,
                metadata_iri(HYDRA_NEXT, "hydra:next")?,
                metadata_iri(
                    &self.target.absolute_next(cursor).ok_or_else(|| {
                        Problem::new(
                            ErrorCode::InternalError,
                            "an RDF fragment continuation needs the request origin",
                        )
                    })?,
                    "the fragment continuation URL",
                )?,
            ));
        }

        serialize_graph(
            format,
            &triples,
            &[("kgfbn", self.blank_nodes.iri_prefix())],
        )
        .map(Bytes::from)
        .map_err(|error| unreadable("serializing the fragment RDF graph", &error))
    }
}

fn rdf_fragment_cell<'a>(
    bound: Option<&'a BoundTerm>,
    row: &'a Row,
    position: Position,
) -> Option<&'a str> {
    bound.map(BoundTerm::dictionary).or_else(|| {
        row.cells
            .iter()
            .find_map(|(found, value)| (*found == position).then_some(value.as_ref()))
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
    count: Cardinality,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    absent_terms: Vec<&'static str>,
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
}

impl Renders for SearchAnswer {
    fn render(self, representation: Representation) -> Result<Rendered, Problem> {
        let body = standard_body(&self, representation);
        Ok(Rendered {
            body,
            completeness: self.completeness,
        })
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
        Ok(Rendered {
            body,
            completeness: self.completeness,
        })
    }
}

impl Renders for BindingCountAnswer {
    fn render(self, representation: Representation) -> Result<Rendered, Problem> {
        let body = standard_body(&self, representation);
        Ok(Rendered {
            body,
            completeness: self.completeness,
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
        let body = standard_body(&self, representation);
        Ok(Rendered { body, completeness })
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
        Representation::Turtle | Representation::JsonLd | Representation::Markdown => {
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
            });
        }
        Representation::Json | Representation::Markdown => {
            unreachable!("/void negotiation does not offer this representation")
        }
    };

    Ok(Rendered {
        body,
        completeness: void_completeness(complete && emitted == total),
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
        Representation::Turtle | Representation::JsonLd => {
            unreachable!("/summary negotiation does not offer RDF representations")
        }
    };
    Ok(Rendered {
        body,
        completeness: Completeness::complete(),
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

fn rdf_fragment_subject(
    term: &[u8],
    id: TermId,
    blank_nodes: &SkolemScope,
) -> Result<NamedOrBlankNode, Problem> {
    let text = rdf_text(term)?;
    if let Some(iri) = blank_nodes.iri(Role::Subject, id, text) {
        return NamedNode::new(iri)
            .map(Into::into)
            .map_err(|error| unreadable("skolemizing an RDF blank-node subject", &error));
    }
    rdf_subject(term)
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

fn rdf_fragment_object(
    term: &[u8],
    id: TermId,
    blank_nodes: &SkolemScope,
) -> Result<RdfTerm, Problem> {
    let text = rdf_text(term)?;
    if let Some(iri) = blank_nodes.iri(Role::Object, id, text) {
        return NamedNode::new(iri)
            .map(Into::into)
            .map_err(|error| unreadable("skolemizing an RDF blank-node object", &error));
    }
    rdf_object(term)
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

/// `GET /fragment` — enumerate a triple pattern.
pub fn get_fragment(
    store: &Store,
    target: Target,
    request: &request::GetFragment,
) -> Result<Answer, Problem> {
    match request {
        request::GetFragment::Plain(request) => fragment(store, target, request),
        request::GetFragment::Bindings(request) => binding_fragment(store, target, request),
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
    };
    let vars = request.pattern.vars();

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
    };

    match (
        resolve(&dictionary, &envelope.blank_nodes, &request.pattern)?,
        request.pattern.text(),
    ) {
        (Resolved::Absent(absent), _) => paged(
            &dictionary,
            target,
            Envelope {
                absent_terms: absent,
                ..envelope
            },
            Vec::new(),
            paging,
        ),
        (Resolved::Ids(ids), None) => paged(
            &dictionary,
            target,
            envelope,
            vec![phase(select(store, ids)?, None)],
            paging,
        ),
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
            ranked_page(&dictionary, target, envelope, found, paging)
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
        // predicate-group probe the enumeration would run.
        (Resolved::Ids(ids), None) => (
            Cardinality::exact(select(store, ids)?.count().value),
            Completeness::complete(),
            Vec::new(),
        ),
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
    for (row_index, ids) in restrictions.iter().copied() {
        let selection = select(store, ids)?;
        restriction_counts.push((ids, selection.count().value));
        phases.push(binding_phase(selection, row_index));
    }

    let envelope = Envelope {
        echo: Echo::BindingsFragment {
            pattern: request.pattern.clone(),
        },
        vars: request.pattern.vars(),
        directed: false,
        bindings: true,
        absent_terms: Vec::new(),
        blank_nodes,
    };
    let paging = Paging {
        cursor: request.cursor.as_ref(),
        limit: request.limit,
        bytes: request.bytes,
        binding: &request.binding,
    };
    if !request.distinct_rdf() {
        return paged(&dictionary, target, envelope, phases, paging);
    }

    let base_pattern = resolve_binding_pattern(&mut cache, &request.pattern)?;
    let rdf_cardinality = rdf_projection_cardinality(store, base_pattern, &restriction_counts)?;
    let mut answer = paged_distinct_bindings(
        &dictionary,
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
    let mut cache = LookupCache::new(dictionary, blank_nodes);
    let mut counts = Vec::new();
    for row in request.rows() {
        let value = match resolve_binding(&mut cache, row)? {
            Some(ids) => select(store, ids)?.count().value,
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
        counts,
        completeness: Completeness::complete(),
        target,
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
    })
}

#[allow(clippy::too_many_arguments)]
fn push_search_result(
    store: &Store,
    dictionary: &Dictionary<'_>,
    cache: &mut TermCache,
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

    let (subject, subject_serialized) = cache
        .measured(dictionary, Role::Subject, TermId(triple.subject))
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
        phases.push(phase(selection, Some(Direction::Out)));
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
        phases.push(phase(selection, Some(Direction::In)));
    }
    // Absent in the sense that matters for *this* request: the bundle holds no
    // term that could match it in any of the roles the direction walks.
    let absent_terms = if phases.is_empty() {
        vec!["iri"]
    } else {
        Vec::new()
    };

    let mut answer = paged(
        &dictionary,
        target,
        Envelope {
            echo,
            // Every row carries all three, because for `direction=both` there
            // is no single bound position — and a row shape that changed with
            // `direction` would make the wrapper harder to consume than the
            // `/fragment` it wraps.
            vars: Position::ALL.to_vec(),
            directed: true,
            bindings: false,
            absent_terms,
            blank_nodes,
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
    let vars = request.pattern.vars();

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
            // A sample never pages, so nothing reads these.
            space: PositionSpace::Spo,
            resume: 0,
            scan: None,
            binding_index: None,
            direction: None,
            ranking: None,
        })
        .collect();

    let (rows, spent_at) = materialize(&dictionary, &vars, &steps, request.bytes)?;
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
    })
}

// ---------------------------------------------------------------------------
// Resolution and paging
// ---------------------------------------------------------------------------

/// One enumeration a paged operation walks.
struct Phase<'a> {
    selection: Selection<'a>,
    space: PositionSpace,
    count: u64,
    binding_index: Option<u32>,
    direction: Option<Direction>,
}

fn phase(selection: Selection<'_>, direction: Option<Direction>) -> Phase<'_> {
    Phase {
        space: PositionSpace::of(&selection),
        count: selection.count().value,
        selection,
        binding_index: None,
        direction,
    }
}

fn binding_phase(selection: Selection<'_>, binding_index: u32) -> Phase<'_> {
    Phase {
        space: PositionSpace::of(&selection),
        count: selection.count().value,
        selection,
        binding_index: Some(binding_index),
        direction: None,
    }
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
    space: PositionSpace,
    resume: u64,
    /// The second half of a [`PositionSpace::TextRank`] position: how many of
    /// this hit's statements come before this row. `None` in every space whose
    /// position is a single number.
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
        match self.space {
            PositionSpace::TextRank => {
                Cursor::at_rank(binding, self.position, self.scan.unwrap_or(0))
            }
            space if self.binding_index.is_some() => Cursor::at_binding(
                binding,
                self.binding_index.expect("checked above"),
                space,
                self.position,
            ),
            space => Cursor::at(binding, space, self.position),
        }
        .encode()
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
        steps.extend(
            positioned(&selection, space, within)
                .take(want - steps.len())
                .map(|(triple, at)| Step {
                    triple,
                    space: PositionSpace::TextRank,
                    resume: rank as u64,
                    scan: Some(at),
                    binding_index: None,
                    direction: None,
                    ranking: Some(Ranking {
                        score: hit.score,
                        kind: match_kind(hit.kind),
                    }),
                }),
        );
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
    Absent(Vec<&'static str>),
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
            None => absent.push(position.as_str()),
        }
    }
    if absent.is_empty() {
        Ok(Resolved::Ids(ids))
    } else {
        Ok(Resolved::Absent(absent))
    }
}

fn locate(
    dictionary: &Dictionary<'_>,
    role: Role,
    term: &BoundTerm,
) -> Result<Option<u64>, Problem> {
    dictionary
        .locate(role, term.dictionary().as_bytes())
        .map(|found| found.map(|id| id.0))
        .map_err(|error| unreadable("looking a term up", &error))
}

/// Look up a request term, reversing this HDT's skolem URNs in RDF term roles.
fn locate_scoped(
    dictionary: &Dictionary<'_>,
    blank_nodes: &SkolemScope,
    role: Role,
    term: &BoundTerm,
) -> Result<Option<u64>, Problem> {
    if matches!(role, Role::Subject | Role::Object)
        && let Some(id) = blank_nodes.role_id(role, term.dictionary())
    {
        let mut buffer = Vec::new();
        let stored = dictionary
            .extract(role, id, &mut buffer)
            .map_err(|error| unreadable("reversing a blank-node IRI", &error))?;
        if stored.starts_with(b"_:") {
            return Ok(Some(id.0));
        }
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
    vars: Vec<Position>,
    directed: bool,
    bindings: bool,
    absent_terms: Vec<&'static str>,
    blank_nodes: SkolemScope,
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
    dictionary: &Dictionary<'_>,
    target: Target,
    envelope: Envelope,
    phases: Vec<Phase<'_>>,
    paging: Paging<'_>,
) -> Result<Answer, Problem> {
    let predicates = dictionary.counts().len(Role::Predicate);
    let steps = walk(&phases, paging.cursor, predicates, paging.want())?;
    // Exact, and known before the walk: a pattern's cardinality is a range
    // width after bounded descent, so the enumeration is not what produces it.
    let cardinality = exact_cardinality_sum(phases.iter().map(|phase| phase.count))?;

    finish(dictionary, target, envelope, steps, paging, None, |_, _| {
        cardinality
    })
}

/// Build a brTPF RDF page after filtering the compatibility relation to its
/// distinct triple union. Filtering happens before the page limit, so overlap
/// cannot turn a full native page into an empty Hydra page.
fn paged_distinct_bindings(
    dictionary: &Dictionary<'_>,
    target: Target,
    envelope: Envelope,
    phases: Vec<Phase<'_>>,
    restrictions: &[(u32, IdPattern)],
    candidates: Candidates,
    paging: Paging<'_>,
) -> Result<Answer, Problem> {
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
    finish(
        dictionary,
        target,
        envelope,
        steps,
        paging,
        spent,
        |_, _| cardinality,
    )
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
    dictionary: &Dictionary<'_>,
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
        dictionary,
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
    dictionary: &Dictionary<'_>,
    target: Target,
    envelope: Envelope,
    mut steps: Vec<Step>,
    paging: Paging<'_>,
    spent: Option<Spent>,
    cardinality: impl FnOnce(&Completeness, &[Row]) -> Cardinality,
) -> Result<Answer, Problem> {
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
    } = envelope;

    // Materializing is where the bytes appear, so it is where the byte budget
    // applies — before the response exists rather than after, which also bounds
    // the memory a page can take.
    let (rows, spent_at) = materialize(dictionary, &vars, &steps, paging.bytes)?;
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
        byte_budget: paging.bytes.0,
        vars,
        completeness,
        directed,
        bindings,
        target,
        blank_nodes,
        page_labels: HashMap::new(),
        described: None,
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
    let (start, mut from) = walk_start(phases, cursor, predicates)?;

    let mut steps = Vec::new();
    for phase in &phases[start..] {
        if steps.len() >= want {
            break;
        }
        let remaining = want - steps.len();
        steps.extend(
            positioned(&phase.selection, phase.space, from)
                .take(remaining)
                .map(|(triple, resume)| Step {
                    triple,
                    space: phase.space,
                    resume,
                    scan: None,
                    binding_index: phase.binding_index,
                    direction: phase.direction,
                    ranking: None,
                }),
        );
        from = 0;
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
    let (start, mut from) = walk_start(phases, cursor, predicates)?;
    let mut steps = Vec::new();
    let mut examined = 0u64;
    for phase in &phases[start..] {
        let binding_index = phase
            .binding_index
            .expect("a distinct binding walk contains only binding phases");
        for (triple, resume) in positioned(&phase.selection, phase.space, from) {
            let candidate = Step {
                triple,
                space: phase.space,
                resume,
                scan: None,
                binding_index: phase.binding_index,
                direction: phase.direction,
                ranking: None,
            };
            if examined >= candidates.0 {
                return Ok((steps, Some(Spent::Candidate(candidate))));
            }
            examined += 1;
            if restrictions.iter().any(|(owner, pattern)| {
                *owner < binding_index && id_pattern_matches(*pattern, triple)
            }) {
                continue;
            }
            steps.push(candidate);
            if steps.len() >= want {
                return Ok((steps, None));
            }
        }
        from = 0;
    }
    Ok((steps, None))
}

fn walk_start(
    phases: &[Phase<'_>],
    cursor: Option<&Cursor>,
    predicates: u64,
) -> Result<(usize, u64), Problem> {
    match cursor {
        None => Ok((0, 0)),
        Some(cursor) => {
            let index = phases
                .iter()
                .position(|phase| {
                    phase.space == cursor.space && phase.binding_index == cursor.binding_index
                })
                .ok_or_else(|| Problem::from(StaleCursor))?;
            Ok((index, resume_position(cursor, &phases[index], predicates)?))
        }
    }
}

/// Pair each triple with the position a page resumes at to return it first.
///
/// The running position *before* each row, in whichever space this phase counts
/// in: an offset for the three permutation spaces, and for `s ? o` the previous
/// row's predicate id — route-independent and strictly
/// increasing, since one (s, p, o) occurs at most once.
fn positioned<'a>(
    selection: &'a Selection<'a>,
    space: PositionSpace,
    from: u64,
) -> impl Iterator<Item = (IdTriple, u64)> + 'a {
    let mut resume = from;
    // `usize::MAX` rather than a page size: `Selection::page` is lazy, so the
    // caller's `take` is what bounds the work, and a multi-phase walk cannot
    // know its own bound per phase up front.
    selection.page(from, usize::MAX).map(move |triple| {
        let at = resume;
        resume = match space {
            PositionSpace::Predicate => triple.predicate,
            _ => resume + 1,
        };
        (triple, at)
    })
}

/// Where a cursor resumes this phase, or `stale_cursor`.
fn resume_position(cursor: &Cursor, phase: &Phase<'_>, predicates: u64) -> Result<u64, Problem> {
    let stale = || Problem::from(StaleCursor);
    // A phase's binding trailer must match it exactly; `scan_position` belongs
    // to text spaces, which are not phases. Any other shape was not issued by
    // the request it arrived on.
    if cursor.binding_index != phase.binding_index || cursor.scan_position.is_some() {
        return Err(stale());
    }
    // A position past the end would otherwise page to an empty response, which
    // a client reads as the end of results rather than as a bad token.
    let within = match phase.space {
        PositionSpace::Predicate => {
            (1..=predicates).contains(&cursor.position)
                // At a binding-row boundary there is no previous predicate;
                // zero is the sentinel for the first result of the new row.
                || (cursor.position == 0 && phase.binding_index.is_some())
        }
        _ => cursor.position < phase.count,
    };
    within.then_some(cursor.position).ok_or_else(stale)
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
    vars: &[Position],
    steps: &[Step],
    bytes: ResponseBytes,
) -> Result<(Vec<Row>, Option<usize>), Problem> {
    let mut cache = TermCache::new();
    let mut rows: Vec<Row> = Vec::with_capacity(steps.len());
    let mut spent = 0u64;
    for (index, step) in steps.iter().enumerate() {
        let mut cells = Vec::with_capacity(vars.len());
        let mut terms = 0u64;
        for position in vars {
            let (text, serialized) = cache
                .measured(
                    dictionary,
                    position.role(),
                    TermId(position.of(step.triple)),
                )
                .map_err(|error| unreadable("materializing a term", &error))?;
            terms += serialized;
            cells.push((*position, text));
        }
        let row = Row::new(
            cells,
            terms,
            step.triple,
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
                url::dataset(&self.target.id.dataset),
            ),
            Crumb::to(
                &self.target.id.version,
                url::operation(&self.target.id.dataset, &self.target.id.version, "manifest"),
            ),
        ];
        if self.selector.kind() == "dataset" && self.collection.is_none() {
            crumbs.push(Crumb::here("schema"));
            return crumbs;
        }

        crumbs.push(Crumb::to(
            "schema",
            query(
                url::operation(&self.target.id.dataset, &self.target.id.version, "schema"),
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
        operation_page(
            &heading,
            &context,
            &self.target.crumbs(),
            canonical.as_deref(),
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
                        "This bundle's dictionary holds no term for {}. The answer is empty for \
                         that reason, not because the pattern has no matches.",
                        self.absent_terms.join(", ")
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
            Echo::Fragment { pattern } => Some(pattern),
            _ => None,
        }
    }

    /// The fields above the table: what was asked, and how much of it came back.
    fn summary<'a>(&'a self, completeness: &'a str) -> Vec<(&'a str, Value<'a>)> {
        let mut summary = match &self.echo {
            Echo::Fragment { pattern } => pattern_fields(pattern),
            Echo::BindingsFragment { pattern } => binding_pattern_fields(pattern),
            Echo::Describe { direction, .. } => {
                vec![("direction", Value::Text(direction.as_str()))]
            }
            Echo::Sample { pattern, .. } => pattern_fields(pattern),
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
            self.vars.iter().map(|position| position.as_str()).collect()
        };
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
                    // back into their fixed positions for display.
                    for position in Position::ALL {
                        let text =
                            pattern
                                .bound(position)
                                .map(BoundTerm::dictionary)
                                .or_else(|| {
                                    row.cells
                                        .iter()
                                        .find(|(row_position, _)| *row_position == position)
                                        .map(|(_, text)| text.as_ref())
                                });
                        if let Some(text) = text {
                            cells.push(self.cell(text));
                        }
                    }
                } else {
                    cells.extend(row.cells.iter().map(|(_, text)| self.cell(text)));
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

    /// One term, and the request that asks about it.
    ///
    /// This is what makes the page a way *into* the data rather than a dump of
    /// it: a subject, predicate or object links to its own neighborhood, a
    /// literal to every triple carrying it.
    fn cell<'a>(&'a self, text: &'a str) -> Cell<'a> {
        let mut cell = term_cell(
            &self.target,
            text,
            self.page_labels.get(text).map(String::as_str),
        );
        if self.described.as_deref() == Some(text) {
            cell.href = None;
        }
        cell
    }
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
                    &result.subject,
                    result.label.as_ref().and_then(Option::as_deref),
                )
            })
            .collect();
        let predicates: Vec<_> = self
            .results
            .iter()
            .map(|result| term_cell(&self.target, &result.evidence.predicate, None))
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
        let mut summary = pattern_fields(&self.pattern);
        summary.push(("count", Value::Number(self.count.value())));
        summary.push((
            "exact",
            Value::Text(if self.count.is_exact() { "yes" } else { "no" }),
        ));

        let canonical = self.target.canonical();
        let context = self.target.context();
        operation_page(
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
                        "This bundle's dictionary holds no term for {}, so nothing can match.",
                        self.absent_terms.join(", ")
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
                        url::operation(&self.target.id.dataset, &self.target.id.version, "fragment"),
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
            &self.target.title(),
            &self.target.crumbs(),
            canonical.as_deref(),
            html! {
                div."answer-summary" {
                    (fields(&binding_pattern_fields(&self.pattern)))
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

/// The three pattern positions, as page fields.
fn pattern_fields(pattern: &Pattern) -> Vec<(&str, Value<'_>)> {
    let mut fields: Vec<_> = Position::ALL
        .into_iter()
        .map(|position| {
            (
                position.as_str(),
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

fn binding_pattern_fields(pattern: &BindingPattern) -> Vec<(&str, Value<'_>)> {
    Position::ALL
        .into_iter()
        .map(|position| (position.as_str(), Value::Code(pattern.requested(position))))
        .collect()
}

/// Render one RDF term with this release's prefix map and the same drill-down
/// link semantics on every operation page.
///
/// A named term — subject, predicate or object alike — links to its own
/// `/describe` neighborhood; a literal links to every triple carrying it. A
/// predicate used to link to `/fragment?p=`, but the page a reader wants from
/// a predicate is what the term *is*, and its usage is one link further.
fn term_cell<'a>(target: &Target, text: &'a str, annotation: Option<&'a str>) -> Cell<'a> {
    let term = Term::from_dictionary(text);
    let request = term.to_request();
    let href = match &term {
        Term::Literal(_) => target.ask("fragment", "o", &request),
        _ => target.ask("describe", "iri", &request),
    };
    let (label, qualifier, full_iri) = term.into_display(&target.prefixes).into_structured();
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
    fn fragment_rdf_publishes_data_blank_nodes_as_named_urns() {
        let counts = kgf_store::dict::DictCounts {
            shared: 1,
            subjects: 0,
            objects: 0,
            predicates: 0,
        };
        let blank_nodes = SkolemScope::new([0xab; 32], counts);
        let subject = rdf_fragment_subject(b"_:b1", TermId(1), &blank_nodes).unwrap();
        let object = rdf_fragment_object(b"_:b1", TermId(1), &blank_nodes).unwrap();
        let expected = blank_nodes.iri(Role::Subject, TermId(1), "_:b1").unwrap();

        match subject {
            NamedOrBlankNode::NamedNode(node) => assert_eq!(node.as_str(), expected),
            NamedOrBlankNode::BlankNode(_) => panic!("fragment data kept a local blank node"),
        }
        match object {
            RdfTerm::NamedNode(node) => assert_eq!(node.as_str(), expected),
            other => panic!("fragment data became {other:?} rather than a named node"),
        }
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
                            let cells: Vec<(Position, Rc<str>)> = Position::ALL[..width]
                                .iter()
                                .map(|position| (*position, Rc::from(term)))
                                .collect();
                            // What the cache would have measured for each cell.
                            let each = serde_json::to_vec(&Term::from_dictionary(term))
                                .expect("a term serializes")
                                .len() as u64;

                            let row = Row::new(
                                cells,
                                each * width as u64,
                                IdTriple {
                                    subject: 1,
                                    predicate: 1,
                                    object: 1,
                                },
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
