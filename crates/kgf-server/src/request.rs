//! What the client asked for, parsed from parameters into domain types.
//!
//! Pure — no store, no HTTP. Everything a request can be refused for that does
//! not need the bundle is refused here, which is what lets a handler negotiate,
//! parse and evaluate a precondition before it pays for a cold mmap.
//!
//! # A parameter is never ignored
//!
//! Three kinds of parameter reach these operations, and each has its own
//! answer:
//!
//! - one the operation takes — parsed, and refused if it will not parse;
//! - one the protocol defines but this deployment cannot answer — refused
//!   `capability_not_available` (501), naming the capability;
//! - anything else — refused `malformed_request`, listing what the operation
//!   does take.
//!
//! None of them is dropped. That rule is the same one [`crate::term`] applies
//! to an unknown key in a term object and [`crate::url`] applies to a repeated
//! parameter, and it exists because the failure mode is shared: a filter that
//! is ignored produces a *larger* answer that looks like a correct one, and
//! nothing downstream can tell. `g=` is the sharpest case — a request scoped to
//! one named graph, answered from the whole dataset, is wrong in a way a client
//! has no way to detect.
//!
//! # Terms are canonical before they are hashed
//!
//! A cursor binds to the request that issued it ([`crate::cursor`]), and the
//! binding is computed here — from each term's *dictionary* spelling rather
//! than from the text the client sent. `ex:alice` and
//! `<http://example.org/alice>` are one term, so they are one canonical
//! request, and a client that changes how it writes a term between pages keeps
//! its cursor. It also means the binding needs no dictionary, so a cursor is
//! validated before the bundle opens.

use std::collections::{BTreeMap, BTreeSet};

use oxrdf::VariableRef as SparqlVariableRef;
use serde::Deserialize;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, Serializer};
use spargebra::algebra::GraphPattern;
use spargebra::term::GroundTerm;
use spargebra::{Query, SparqlParser};

use hdtc::format::TextQuery;
use kgf_store::{
    Capability, ClassPropertyFilter as StoreClassPropertyFilter,
    ClassRelationFilter as StoreClassRelationFilter, Role,
    SchemaChildQuery as StoreSchemaChildQuery, SchemaSelector as StoreSchemaSelector, StatsView,
};

use crate::Limits;
use crate::access::{RequestShape, Transport};
use crate::admission::WorkClass;
use crate::cursor::{
    BundleBinding, CanonicalRequest, Cursor, CursorBinding, Operation, StaleCursor,
};
use crate::envelope::{ErrorCode, Problem, reflected};
use crate::service::PredicateRoles;
use crate::term::{Literal as KgfLiteral, PrefixMap, Term};
use crate::url::Params;

// ---------------------------------------------------------------------------
// Positions and terms
// ---------------------------------------------------------------------------

/// A triple position, which is also the parameter that binds it and the key a
/// row reports it under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Position {
    /// `s`.
    Subject,
    /// `p`.
    Predicate,
    /// `o`.
    Object,
}

impl Position {
    /// The three, in the order `vars` lists them.
    pub const ALL: [Position; 3] = [Position::Subject, Position::Predicate, Position::Object];

    /// The parameter name and response key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subject => "s",
            Self::Predicate => "p",
            Self::Object => "o",
        }
    }

    /// Conventional spelling used only when echoing an omitted brTPF GET
    /// position. The request model keeps it anonymous, so this display name
    /// can never join it to an explicitly named variable.
    fn default_variable(self) -> &'static str {
        match self {
            Self::Subject => "?s",
            Self::Predicate => "?p",
            Self::Object => "?o",
        }
    }

    /// The id space a term in this position resolves in.
    pub fn role(self) -> Role {
        match self {
            Self::Subject => Role::Subject,
            Self::Predicate => Role::Predicate,
            Self::Object => Role::Object,
        }
    }

    /// This position's id in a triple.
    pub fn of(self, triple: kgf_store::IdTriple) -> u64 {
        match self {
            Self::Subject => triple.subject,
            Self::Predicate => triple.predicate,
            Self::Object => triple.object,
        }
    }
}

impl Serialize for Position {
    /// The key a response's `vars` and rows use.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// A term a request bound a position to: as the client wrote it, and as the
/// dictionary spells it.
///
/// Both, because they answer different questions. The request spelling is what
/// the response echoes back, so a client sees its own pattern rather than a
/// normalization of it. The dictionary spelling is what [`locate`] is called
/// with and what the cursor binding hashes, and it is *owned* — a request
/// crosses onto the blocking pool, so it cannot borrow the query string it was
/// read from.
///
/// [`locate`]: kgf_store::dict::Dictionary::locate
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundTerm {
    requested: String,
    dictionary: String,
    kind: BoundKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundKind {
    Iri,
    Literal,
    BlankNode,
}

impl BoundKind {
    fn of(term: &Term<'_>) -> Self {
        match term {
            Term::Iri(_) => Self::Iri,
            Term::Literal(_) => Self::Literal,
            Term::BlankNode(_) => Self::BlankNode,
        }
    }

    const fn shape(self) -> char {
        match self {
            Self::Iri => 'i',
            Self::Literal => 'l',
            Self::BlankNode => 'b',
        }
    }
}

impl BoundTerm {
    /// Parse request-term syntax from the parameter named `parameter`.
    fn parse(
        parameter: &str,
        text: &str,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
    ) -> Result<Self, Problem> {
        // `max_term_bytes`, applied where a term enters. Published, so
        // enforced: a cap a server advertises and does not apply is worse than
        // no cap, because a client sizes its requests by it.
        let max = limits.budgets.max_term_bytes;
        if text.len() as u64 > max {
            return Err(Problem::new(
                ErrorCode::CapExceeded,
                format!(
                    "the term in `{parameter}` is {} bytes, over this server's \
                     max_term_bytes of {max}",
                    text.len()
                ),
            ));
        }
        let term = Term::parse(text, prefixes).map_err(|error| {
            Problem::new(
                ErrorCode::BadTermSyntax,
                format!("parameter `{parameter}`: {error}"),
            )
        })?;
        let kind = BoundKind::of(&term);
        Ok(Self {
            requested: text.to_owned(),
            dictionary: term.to_dictionary().into_owned(),
            kind,
        })
    }

    /// Parse either compact request syntax or the JSON term-object form.
    fn parse_body(
        parameter: &str,
        value: WireTerm,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
    ) -> Result<Self, Problem> {
        let value = match value {
            WireTerm::Text(text) => return Self::parse(parameter, &text, limits, prefixes),
            WireTerm::Object(fields) => {
                let mut object = serde_json::Map::new();
                for (key, value) in fields {
                    if object.contains_key(&key) {
                        return Err(Problem::new(
                            ErrorCode::BadTermSyntax,
                            format!("{parameter}: term object is malformed: duplicate key `{key}`"),
                        ));
                    }
                    object.insert(key, value);
                }
                serde_json::Value::Object(object)
            }
            WireTerm::Other(value) => value,
        };
        let term = Term::from_json(&value).map_err(|error| {
            Problem::new(ErrorCode::BadTermSyntax, format!("{parameter}: {error}"))
        })?;
        let kind = BoundKind::of(&term);
        let dictionary = term.to_dictionary().into_owned();
        let max = limits.budgets.max_term_bytes;
        if dictionary.len() as u64 > max {
            return Err(Problem::new(
                ErrorCode::CapExceeded,
                format!(
                    "the term in `{parameter}` is {} bytes in canonical form, over this \
                     server's max_term_bytes of {max}",
                    dictionary.len()
                ),
            ));
        }
        Ok(Self {
            requested: term.to_request(),
            dictionary,
            kind,
        })
    }

    /// Convert a term already parsed by SPARQL without formatting and parsing
    /// it a second time. KGF's HDT dictionaries deliberately store literal
    /// lexical forms unescaped, whereas `oxrdf`'s display form uses SPARQL /
    /// N-Triples escapes; crossing those two syntaxes would change the term.
    fn from_ground_term(
        parameter: &str,
        term: GroundTerm,
        limits: Limits<'_>,
    ) -> Result<Self, Problem> {
        let term = match term {
            GroundTerm::NamedNode(node) => Term::Iri(node.into_string().into()),
            GroundTerm::Literal(literal) => {
                let value = literal.value().to_owned();
                let literal = match literal.language() {
                    Some(language) => KgfLiteral::tagged(value, language.to_owned()),
                    None => KgfLiteral::typed(value, literal.datatype().as_str().to_owned()),
                };
                Term::Literal(literal)
            }
        };
        let kind = BoundKind::of(&term);
        let dictionary = term.to_dictionary().into_owned();
        let max = limits.budgets.max_term_bytes;
        if dictionary.len() as u64 > max {
            return Err(Problem::new(
                ErrorCode::CapExceeded,
                format!(
                    "the term in `{parameter}` is {} bytes in canonical form, over this \
                     server's max_term_bytes of {max}",
                    dictionary.len()
                ),
            ));
        }
        Ok(Self {
            requested: term.to_request(),
            dictionary,
            kind,
        })
    }

    /// Parse a fragment-protocol URL term. TPF clients write a named node as
    /// its bare absolute IRI, while KGF's native spelling brackets it. Keep
    /// the native/CURIE interpretation first (so a declared `ex:a` stays a
    /// CURIE), then accept the TPF spelling when that interpretation fails.
    fn parse_fragment(
        parameter: &str,
        text: &str,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
    ) -> Result<Self, Problem> {
        match Self::parse(parameter, text, limits, prefixes) {
            Ok(term) => Ok(term),
            Err(native_error) => {
                if spargebra::term::NamedNode::new(text).is_err() {
                    return Err(native_error);
                }
                let max = limits.budgets.max_term_bytes;
                if text.len() as u64 > max {
                    return Err(Problem::new(
                        ErrorCode::CapExceeded,
                        format!(
                            "the term in `{parameter}` is {} bytes in canonical form, over this \
                             server's max_term_bytes of {max}",
                            text.len()
                        ),
                    ));
                }
                Ok(Self {
                    requested: text.to_owned(),
                    dictionary: text.to_owned(),
                    kind: BoundKind::Iri,
                })
            }
        }
    }

    /// The term as the request wrote it.
    pub fn requested(&self) -> &str {
        &self.requested
    }

    /// The term as the dictionary spells it.
    pub fn dictionary(&self) -> &str {
        &self.dictionary
    }

    fn shape(&self) -> char {
        self.kind.shape()
    }

    fn require_iri(self, parameter: &str) -> Result<Self, Problem> {
        if self.kind == BoundKind::Iri {
            Ok(self)
        } else {
            Err(Problem::new(
                ErrorCode::BadTermSyntax,
                format!(
                    "`{parameter}` takes an IRI, not {}",
                    reflected(&self.requested)
                ),
            ))
        }
    }

    fn from_profile_iri(iri: &str) -> Self {
        Self {
            requested: format!("<{iri}>"),
            dictionary: iri.to_owned(),
            kind: BoundKind::Iri,
        }
    }
}

/// A triple pattern, as far as it can be read without a bundle.
///
/// An omitted or empty parameter is a variable. Empty is accepted because it is
/// what an ordinary HTML form sends for an untouched optional control, and no
/// RDF term has an empty request spelling (`""` is the empty literal). Required
/// term parameters such as `/describe`'s `iri` remain non-empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pattern {
    subject: Option<BoundTerm>,
    predicate: Option<BoundTerm>,
    object: Option<BoundTerm>,
    text: Option<TextFilter>,
}

impl Pattern {
    fn parse(params: &Params, limits: Limits<'_>, prefixes: &PrefixMap) -> Result<Self, Problem> {
        Self::parse_with(params, limits, prefixes, false)
    }

    fn parse_with(
        params: &Params,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
        tpf_terms: bool,
    ) -> Result<Self, Problem> {
        let mut pattern = Self::default();
        for position in Position::ALL {
            if let Some(text) = params
                .get(position.as_str())
                .filter(|text| !text.is_empty())
            {
                *pattern.slot(position) = Some(if tpf_terms {
                    BoundTerm::parse_fragment(position.as_str(), text, limits, prefixes)?
                } else {
                    BoundTerm::parse(position.as_str(), text, limits, prefixes)?
                });
            }
        }
        // Part of the pattern rather than beside it: the constraint sits in
        // the object position it constrains. An
        // operation that does not offer `o.text` has already refused it in
        // `accept_only`, so this is unreachable for those.
        pattern.text = TextFilter::parse(params, &pattern, limits)?;
        Ok(pattern)
    }

    /// The ranked text constraint on the object, if the request carried one.
    pub fn text(&self) -> Option<&TextFilter> {
        self.text.as_ref()
    }

    fn slot(&mut self, position: Position) -> &mut Option<BoundTerm> {
        match position {
            Position::Subject => &mut self.subject,
            Position::Predicate => &mut self.predicate,
            Position::Object => &mut self.object,
        }
    }

    /// The term bound at `position`, or `None` if it is a variable.
    pub fn bound(&self, position: Position) -> Option<&BoundTerm> {
        match position {
            Position::Subject => self.subject.as_ref(),
            Position::Predicate => self.predicate.as_ref(),
            Position::Object => self.object.as_ref(),
        }
    }

    /// The positions a row carries: the unbound ones.
    ///
    /// `o.text` leaves the object *unbound* — it ranks candidates rather than
    /// naming one — so a text-filtered row still reports its object, which is
    /// the whole point of asking.
    pub fn vars(&self) -> Vec<Position> {
        Position::ALL
            .into_iter()
            .filter(|position| self.bound(*position).is_none())
            .collect()
    }

    fn shape(&self) -> String {
        Position::ALL
            .into_iter()
            .map(|position| self.bound(position).map_or('?', BoundTerm::shape))
            .collect()
    }

    /// Add the pattern to a cursor's canonical request.
    fn canonicalize(&self, mut request: CanonicalRequest) -> CanonicalRequest {
        for position in Position::ALL {
            request = request.with_opt(
                position.as_str(),
                self.bound(position).map(BoundTerm::dictionary),
            );
        }
        request.with_opt("o.text", self.text.as_ref().map(TextFilter::query))
    }
}

impl Serialize for Pattern {
    /// The request echo: all three positions, `null` where unbound, and each bound
    /// one spelled the way the client sent it.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(Position::ALL.len()))?;
        for position in Position::ALL {
            // A constrained object echoes as `{"text": "…"}` in the
            // position it constrains, where a bound one echoes as its term.
            match (position, &self.text) {
                (Position::Object, Some(text)) => map.serialize_entry("o", text)?,
                _ => map.serialize_entry(
                    position.as_str(),
                    &self.bound(position).map(BoundTerm::requested),
                )?,
            }
        }
        map.end()
    }
}

/// A ranked text constraint on the object position.
///
/// Ranked rather than filtering: the index orders the *literals* that match,
/// and the pattern around it decides which of their statements come back. That
/// is why it is not a fourth position — `o` binds one term and `o.text` names
/// many, so a request carrying both is asking two incompatible questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFilter(String);

impl TextFilter {
    /// Read `o.text`, if the request carried one.
    fn parse(
        params: &Params,
        pattern: &Pattern,
        limits: Limits<'_>,
    ) -> Result<Option<Self>, Problem> {
        let Some(text) = params.get("o.text") else {
            return Ok(None);
        };
        if let Some(bound) = pattern.bound(Position::Object) {
            return Err(Problem::new(
                ErrorCode::MalformedRequest,
                format!(
                    "`o={}` and `o.text` both constrain the object; bind the term or                      search for it, not both",
                    reflected(bound.requested())
                ),
            ));
        }
        if text.is_empty() {
            return Err(Problem::new(
                ErrorCode::MalformedRequest,
                "`o.text` is empty; omit it to leave the object unconstrained",
            ));
        }
        // The same budget a term parameter is held to: this is text a client
        // sends, and the term-byte budget caps what one may weigh.
        let max = limits.budgets.max_term_bytes;
        if text.len() as u64 > max {
            return Err(Problem::new(
                ErrorCode::CapExceeded,
                format!(
                    "`o.text` is {} bytes, over this server's max_term_bytes of {max}",
                    text.len()
                ),
            ));
        }
        Ok(Some(Self(text.to_owned())))
    }

    fn required(params: &Params, name: &str, limits: Limits<'_>) -> Result<Self, Problem> {
        let Some(text) = params.get(name) else {
            return Err(Problem::new(
                ErrorCode::MalformedRequest,
                format!("search needs a non-empty `{name}` text query"),
            ));
        };
        if text.is_empty() {
            return Err(Problem::new(
                ErrorCode::MalformedRequest,
                format!("`{name}` is empty; supply text to search for"),
            ));
        }
        let max = limits.budgets.max_term_bytes;
        if text.len() as u64 > max {
            return Err(Problem::new(
                ErrorCode::CapExceeded,
                format!(
                    "`{name}` is {} bytes, over this server's max_term_bytes of {max}",
                    text.len()
                ),
            ));
        }
        Ok(Self(text.to_owned()))
    }

    /// The query text, as the client sent it.
    pub fn query(&self) -> &str {
        &self.0
    }

    /// The index query this constraint asks for.
    ///
    /// One place rather than one per caller: `/fragment` and `/count` must ask
    /// the *same* question, and [`TextQuery`] carries knobs — match mode,
    /// fuzziness, prefix, and language ranges — that may be exposed later.
    /// Wiring one of those into the enumeration and
    /// forgetting the count would report the unfiltered figure for a filtered
    /// page, which is a wrong number that looks right.
    pub fn to_query(&self) -> TextQuery {
        TextQuery {
            text: self.0.clone(),
            ..TextQuery::default()
        }
    }
}

/// `GET /search` — ranked entity resolution over matching literals.
#[derive(Debug)]
pub struct Search {
    /// Text sent to the exhaustive literal index.
    pub query: TextFilter,
    /// Role names the request selected, for the response echo.
    pub roles: Vec<String>,
    /// Explicit and role-expanded predicate IRIs, deduplicated.
    pub predicates: Vec<BoundTerm>,
    /// Ordered predicates used to hydrate the preferred label.
    pub label_predicates: Vec<BoundTerm>,
    /// Whether each entity receives its preferred display label.
    pub labels: bool,
    /// Entity hits retained from the bounded ranking.
    pub limit: u32,
    /// Bytes the result rows may occupy.
    pub bytes: ResponseBytes,
    /// Text documents, occurrence probes, and RDF occurrences this request may examine.
    pub candidates: Candidates,
}

impl Search {
    const PARAMETERS: &'static [&'static str] =
        &["q", "role", "predicate", "labels", "limit", "format"];

    /// Parse one search request against the release's frozen role profile.
    pub fn parse(
        params: &Params,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
        profile: &PredicateRoles,
    ) -> Result<Self, Problem> {
        accept_only(params, SEARCH, Self::PARAMETERS)?;
        let query = TextFilter::required(params, "q", limits)?;

        let mut roles = Vec::new();
        let mut predicates = BTreeMap::<String, BoundTerm>::new();
        if let Some(list) = params.get("role") {
            for role in comma_list("role", list)? {
                let members = profile.get(role).ok_or_else(|| {
                    let available = profile
                        .iter()
                        .map(|(name, _)| name)
                        .collect::<Vec<_>>()
                        .join(", ");
                    Problem::new(
                        ErrorCode::MalformedRequest,
                        format!(
                            "role={} is not declared by this release; use one of [{}] or an explicit `predicate`",
                            reflected(role), available
                        ),
                    )
                })?;
                if !roles.iter().any(|seen| seen == role) {
                    roles.push(role.to_owned());
                }
                for iri in members {
                    predicates
                        .entry(iri.clone())
                        .or_insert_with(|| BoundTerm::from_profile_iri(iri));
                }
            }
        }
        if let Some(list) = params.get("predicate") {
            for text in term_list("predicate", list)? {
                let predicate = BoundTerm::parse("predicate", text, limits, prefixes)?
                    .require_iri("predicate")?;
                predicates
                    .entry(predicate.dictionary.clone())
                    .or_insert(predicate);
            }
        }
        if predicates.len() > limits.caps.max_search_predicates as usize {
            return Err(Problem::new(
                ErrorCode::CapExceeded,
                format!(
                    "the selected roles and predicates expand to {} predicate IRIs, over this server's max_search_predicates of {}",
                    predicates.len(),
                    limits.caps.max_search_predicates
                ),
            ));
        }

        Ok(Self {
            query,
            roles,
            predicates: predicates.into_values().collect(),
            labels: boolean(params, "labels", true)?,
            label_predicates: profile_terms(profile, "label"),
            limit: page_size(
                params,
                "limit",
                limits.caps.default_limit,
                limits.caps.max_search_results,
                "omit search when no hits are wanted",
            )?,
            bytes: ResponseBytes(limits.budgets.max_response_bytes),
            candidates: Candidates(limits.budgets.candidate_budget),
        })
    }
}

/// `QUERY /labels` — one preferred label for each submitted IRI.
#[derive(Debug)]
pub struct Labels {
    iris: Vec<BoundTerm>,
    /// Ordered predicates in the release's label cascade.
    pub label_predicates: Vec<BoundTerm>,
    /// Bytes the result rows may occupy.
    pub bytes: ResponseBytes,
}

impl Labels {
    const PARAMETERS: &'static [&'static str] = &["format"];

    /// Parse the strict JSON body and its compact or term-object IRIs.
    pub fn parse(
        params: &Params,
        body: &[u8],
        limits: Limits<'_>,
        prefixes: &PrefixMap,
        profile: &PredicateRoles,
    ) -> Result<Self, Problem> {
        accept_only(params, LABELS, Self::PARAMETERS)?;
        let wire: WireLabels = parse_body(body)?;
        if wire.iris.len() > limits.caps.max_label_iris as usize {
            return Err(Problem::new(
                ErrorCode::CapExceeded,
                format!(
                    "labels received {} IRIs, over this server's max_label_iris of {}",
                    wire.iris.len(),
                    limits.caps.max_label_iris
                ),
            ));
        }
        let mut iris = Vec::with_capacity(wire.iris.len());
        for (index, iri) in wire.iris.into_iter().enumerate() {
            iris.push(
                BoundTerm::parse_body(&format!("iris[{index}]"), iri, limits, prefixes)?
                    .require_iri(&format!("iris[{index}]"))?,
            );
        }
        Ok(Self {
            iris,
            label_predicates: profile_terms(profile, "label"),
            bytes: ResponseBytes(limits.budgets.max_response_bytes),
        })
    }

    /// Submitted IRIs, in input order.
    pub fn iris(&self) -> &[BoundTerm] {
        &self.iris
    }
}

fn profile_terms(profile: &PredicateRoles, role: &str) -> Vec<BoundTerm> {
    profile
        .get(role)
        .unwrap_or_default()
        .iter()
        .map(|iri| BoundTerm::from_profile_iri(iri))
        .collect()
}

impl Serialize for TextFilter {
    /// The request echo puts the constraint inside the pattern's object position, as
    /// `{"text": "atrazine"}`.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("text", &self.0)?;
        map.end()
    }
}

/// Which side of a resource's neighborhood `/describe` walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Triples with the resource as subject.
    Out,
    /// Triples with the resource as object.
    In,
    /// Both, out-edges first.
    Both,
}

impl Direction {
    /// The token the parameter and the response use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Out => "out",
            Self::In => "in",
            Self::Both => "both",
        }
    }

    /// Whether this direction walks the out-edges.
    pub fn walks_out(self) -> bool {
        matches!(self, Self::Out | Self::Both)
    }

    /// Whether this direction walks the in-edges.
    pub fn walks_in(self) -> bool {
        matches!(self, Self::In | Self::Both)
    }

    fn parse(params: &Params) -> Result<Self, Problem> {
        // `both` by default: the operation returns a resource neighborhood, and
        // half a neighborhood is a surprising default for a
        // client that did not choose.
        let Some(text) = params.get("direction") else {
            return Ok(Self::Both);
        };
        [Self::Out, Self::In, Self::Both]
            .into_iter()
            .find(|direction| direction.as_str() == text)
            .ok_or_else(|| {
                Problem::new(
                    ErrorCode::MalformedRequest,
                    format!(
                        "direction={} is not a direction; use out, in or both",
                        reflected(text)
                    ),
                )
            })
    }
}

impl Serialize for Direction {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// The four requests
// ---------------------------------------------------------------------------

/// The one composite budget a response has to carry with it.
///
/// The caps bound what a client may *ask for*; the budgets bound what a
/// response may *cost*. A row cap is not a byte cap because one legal literal
/// can be megabytes.
/// For M1's operations that difference reduces to exactly one number:
/// [`Limits::validate`] refuses at startup any
/// deployment whose caps could outrun `max_output_rows` or `max_output_terms`,
/// so those two are bounded by construction and need no check per request,
/// while `max_response_bytes` is bounded by nothing a cap can express and is
/// applied while rows are built ([`crate::answer`]).
///
/// Carried on the request because that is what reaches the blocking pool: an
/// operation running there cannot see the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseBytes(pub u64);

/// The `candidate_budget`: rows or postings a filtered operation may
/// *examine*, independently of how many it returns.
///
/// A separate budget from [`ResponseBytes`] because it bounds a different
/// thing. A text-filtered pattern examines one ranked literal per candidate and
/// may keep none of them — `? p ?` discards every match that does not occur
/// with `p` — so the work has no relation to the page size, and `limit` bounds
/// nothing. Exhausting it is not an error: the response comes back short and
/// marked `candidate_budget`, with a cursor when its scan order has a
/// resumable position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidates(pub u64);

impl Candidates {
    /// The budget as a count of items, saturating rather than wrapping on a
    /// target whose `usize` is narrower than the published figure.
    pub fn ceiling(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }
}

// ---------------------------------------------------------------------------
// Bindings bodies
// ---------------------------------------------------------------------------

/// A variable named in a body-carried triple pattern.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Variable(String);

impl Variable {
    fn parse(text: &str, where_: &str) -> Result<Self, Problem> {
        let Some(name) = text.strip_prefix('?') else {
            return Err(Problem::new(
                ErrorCode::MalformedRequest,
                format!(
                    "{where_}={} is not a variable; variables begin with `?`",
                    reflected(text)
                ),
            ));
        };
        if SparqlVariableRef::new(name).is_err() {
            return Err(Problem::new(
                ErrorCode::MalformedRequest,
                format!("{where_}={} is not a SPARQL variable name", reflected(text)),
            ));
        }
        Ok(Self(text.to_owned()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

/// One position of a body-carried pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BindingCell {
    /// An explicitly named SPARQL variable that a bindings column may bind.
    Variable(Variable),
    /// A GET position the client omitted. It is unbound but deliberately has
    /// no variable identity: synthesizing `?s`, `?p`, or `?o` here can collide
    /// with a name explicitly used at another position and silently turn two
    /// independent positions into a repeated-variable join.
    Unbound(Position),
    Term(BoundTerm),
}

/// The explicit pattern in a bindings request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingPattern {
    cells: [BindingCell; 3],
}

impl BindingPattern {
    fn parse(wire: WirePattern, limits: Limits<'_>, prefixes: &PrefixMap) -> Result<Self, Problem> {
        let mut cells = Vec::with_capacity(3);
        for (position, value) in Position::ALL.into_iter().zip([wire.s, wire.p, wire.o]) {
            let parameter = format!("pattern.{}", position.as_str());
            let cell = match value {
                WireTerm::Text(text) if text.starts_with('?') => {
                    BindingCell::Variable(Variable::parse(&text, &parameter)?)
                }
                term => {
                    BindingCell::Term(BoundTerm::parse_body(&parameter, term, limits, prefixes)?)
                }
            };
            cells.push(cell);
        }
        let cells: [BindingCell; 3] = cells
            .try_into()
            .expect("the three positions produce three cells");

        Ok(Self { cells })
    }

    /// Read the variable-preserving pattern a brTPF client puts in `s/p/o`.
    /// Comunica includes variable names on a bindings-restricted request so
    /// the `values=` table can be joined to positions; ordinary TPF omits
    /// unbound positions and never takes this path.
    fn parse_get(
        params: &Params,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
    ) -> Result<Self, Problem> {
        let mut cells = Vec::with_capacity(3);
        for position in Position::ALL {
            let parameter = position.as_str();
            let cell = match params.get(parameter).filter(|value| !value.is_empty()) {
                Some(value) if value.starts_with('?') => {
                    BindingCell::Variable(Variable::parse(value, parameter)?)
                }
                Some(value) => BindingCell::Term(BoundTerm::parse_fragment(
                    parameter, value, limits, prefixes,
                )?),
                None => BindingCell::Unbound(position),
            };
            cells.push(cell);
        }
        Ok(Self {
            cells: cells
                .try_into()
                .expect("the three positions produce three cells"),
        })
    }

    fn cell(&self, position: Position) -> &BindingCell {
        &self.cells[position_index(position)]
    }

    /// The spelling the body used at `position`, for response echoes.
    pub fn requested(&self, position: Position) -> &str {
        match self.cell(position) {
            BindingCell::Variable(variable) => variable.as_str(),
            BindingCell::Unbound(position) => position.default_variable(),
            BindingCell::Term(term) => term.requested(),
        }
    }

    /// A term fixed directly in the pattern, before any input row applies.
    pub fn bound(&self, position: Position) -> Option<&BoundTerm> {
        match self.cell(position) {
            BindingCell::Term(term) => Some(term),
            BindingCell::Variable(_) | BindingCell::Unbound(_) => None,
        }
    }

    /// Positions reported in each result row.
    pub fn vars(&self) -> Vec<Position> {
        Position::ALL
            .into_iter()
            .filter(|position| {
                matches!(
                    self.cell(*position),
                    BindingCell::Variable(_) | BindingCell::Unbound(_)
                )
            })
            .collect()
    }

    fn shape(&self) -> String {
        Position::ALL
            .into_iter()
            .map(|position| self.bound(position).map_or('?', BoundTerm::shape))
            .collect()
    }

    fn variables(&self) -> impl Iterator<Item = &Variable> {
        self.cells.iter().filter_map(|cell| match cell {
            BindingCell::Variable(variable) => Some(variable),
            BindingCell::Unbound(_) | BindingCell::Term(_) => None,
        })
    }

    fn repeated_variables(&self) -> BTreeSet<&Variable> {
        let mut seen = BTreeSet::new();
        self.variables()
            .filter(|variable| !seen.insert(*variable))
            .collect()
    }

    fn canonicalize(&self, mut output: String) -> String {
        for (position, cell) in Position::ALL.into_iter().zip(&self.cells) {
            push_canonical(&mut output, position.as_str());
            match cell {
                BindingCell::Variable(variable) => {
                    output.push('v');
                    push_canonical(&mut output, variable.as_str());
                }
                BindingCell::Unbound(_) => output.push('u'),
                BindingCell::Term(term) => {
                    output.push('t');
                    push_canonical(&mut output, term.dictionary());
                }
            }
        }
        output
    }
}

impl Serialize for BindingPattern {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        for position in Position::ALL {
            match self.cell(position) {
                BindingCell::Variable(variable) => {
                    map.serialize_entry(position.as_str(), variable.as_str())?;
                }
                BindingCell::Unbound(position) => {
                    map.serialize_entry(position.as_str(), position.default_variable())?;
                }
                BindingCell::Term(term) => {
                    map.serialize_entry(position.as_str(), term.requested())?;
                }
            }
        }
        map.end()
    }
}

/// One input row paired with the pattern it restricts.
#[derive(Debug, Clone, Copy)]
pub struct BindingRow<'a> {
    index: u32,
    pattern: &'a BindingPattern,
    columns: &'a BTreeMap<Variable, usize>,
    values: &'a [BindingValue],
}

impl<'a> BindingRow<'a> {
    /// Zero-based input-row index reported beside every result.
    pub fn index(self) -> u32 {
        self.index
    }

    /// The term this input row fixes at `position`, or `None` when it remains a variable.
    pub fn bound(self, position: Position) -> Option<&'a BoundTerm> {
        match self.pattern.cell(position) {
            BindingCell::Term(term) => Some(term),
            BindingCell::Variable(variable) => self
                .columns
                .get(variable)
                .and_then(|column| self.values[*column].bound()),
            BindingCell::Unbound(_) => None,
        }
    }
}

/// One cell of a generalized binding table.
#[derive(Debug)]
enum BindingValue {
    Bound(BoundTerm),
    Undef,
}

impl BindingValue {
    fn bound(&self) -> Option<&BoundTerm> {
        match self {
            Self::Bound(term) => Some(term),
            Self::Undef => None,
        }
    }
}

/// A parsed binding table, with column names already resolved to positions.
#[derive(Debug)]
struct Bindings {
    columns: BTreeMap<Variable, usize>,
    variables: Vec<Variable>,
    rows: Vec<Vec<BindingValue>>,
}

impl Bindings {
    fn parse(
        wire: WireBindings,
        pattern: &BindingPattern,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
    ) -> Result<Self, Problem> {
        if wire.rows.len() > limits.caps.max_bindings as usize {
            return Err(Problem::new(
                ErrorCode::CapExceeded,
                format!(
                    "bindings.rows has {} rows, over this server's max_bindings of {}",
                    wire.rows.len(),
                    limits.caps.max_bindings
                ),
            ));
        }

        let mut columns = BTreeMap::new();
        let mut variables = Vec::with_capacity(wire.vars.len());
        for (column, text) in wire.vars.iter().enumerate() {
            let variable = Variable::parse(text, &format!("bindings.vars[{column}]"))?;
            if columns.insert(variable.clone(), column).is_some() {
                return Err(Problem::new(
                    ErrorCode::MalformedRequest,
                    format!(
                        "bindings.vars contains {} more than once",
                        reflected(variable.as_str())
                    ),
                ));
            }
            variables.push(variable);
        }

        let mut rows = Vec::with_capacity(wire.rows.len());
        for (row_index, wire_row) in wire.rows.into_iter().enumerate() {
            if wire_row.len() != variables.len() {
                return Err(Problem::new(
                    ErrorCode::MalformedRequest,
                    format!(
                        "bindings.rows[{row_index}] has {} values for {} variables",
                        wire_row.len(),
                        variables.len()
                    ),
                ));
            }
            let mut row = Vec::with_capacity(wire_row.len());
            for (column, value) in wire_row.into_iter().enumerate() {
                let parameter = format!("bindings.rows[{row_index}][{column}]");
                row.push(match value {
                    WireTerm::Other(serde_json::Value::Null) => BindingValue::Undef,
                    value => BindingValue::Bound(BoundTerm::parse_body(
                        &parameter, value, limits, prefixes,
                    )?),
                });
            }
            rows.push(row);
        }

        Self::require_bounded_repeated_variables(pattern, &columns, &rows)?;

        Ok(Self {
            columns,
            variables,
            rows,
        })
    }

    fn parse_values(
        text: &str,
        pattern: &BindingPattern,
        limits: Limits<'_>,
    ) -> Result<Self, Problem> {
        let max = limits.budgets.max_request_bytes;
        if text.len() as u64 > max {
            return Err(Problem::new(
                ErrorCode::PayloadTooLarge,
                format!(
                    "values= is {} decoded bytes, over this server's max_request_bytes of {max}",
                    text.len()
                ),
            ));
        }
        let query = SparqlParser::new()
            .parse_query(&format!("SELECT * WHERE {{ VALUES {text} }}"))
            .map_err(|error| {
                Problem::new(
                    ErrorCode::MalformedRequest,
                    format!("values= is not a SPARQL VALUES table: {error}"),
                )
            })?;
        let Query::Select {
            pattern: parsed, ..
        } = query
        else {
            unreachable!("the wrapper is a SELECT query")
        };
        let parsed = match parsed {
            GraphPattern::Project { inner, .. } => *inner,
            pattern => pattern,
        };
        let GraphPattern::Values {
            variables: parsed_variables,
            bindings,
        } = parsed
        else {
            return Err(Problem::new(
                ErrorCode::MalformedRequest,
                "values= must contain exactly one SPARQL VALUES table",
            ));
        };
        if bindings.len() > limits.caps.max_bindings as usize {
            return Err(Problem::new(
                ErrorCode::CapExceeded,
                format!(
                    "values= has {} rows, over this server's max_bindings of {}",
                    bindings.len(),
                    limits.caps.max_bindings
                ),
            ));
        }

        let mut columns = BTreeMap::new();
        let mut variables = Vec::with_capacity(parsed_variables.len());
        for (column, parsed) in parsed_variables.into_iter().enumerate() {
            let variable = Variable(format!("?{}", parsed.as_str()));
            if columns.insert(variable.clone(), column).is_some() {
                return Err(Problem::new(
                    ErrorCode::MalformedRequest,
                    format!(
                        "values= names {} more than once",
                        reflected(variable.as_str())
                    ),
                ));
            }
            variables.push(variable);
        }

        let rows = bindings
            .into_iter()
            .enumerate()
            .map(|(row_index, row)| {
                row.into_iter()
                    .enumerate()
                    .map(|(column, value)| match value {
                        None => Ok(BindingValue::Undef),
                        Some(term) => BoundTerm::from_ground_term(
                            &format!("values row {row_index}, column {column}"),
                            term,
                            limits,
                        )
                        .map(BindingValue::Bound),
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::require_bounded_repeated_variables(pattern, &columns, &rows)?;
        Ok(Self {
            columns,
            variables,
            rows,
        })
    }

    fn require_bounded_repeated_variables(
        pattern: &BindingPattern,
        columns: &BTreeMap<Variable, usize>,
        rows: &[Vec<BindingValue>],
    ) -> Result<(), Problem> {
        // The empty relation has an empty result without probing the store,
        // regardless of whether a repeated variable would be bounded in a row
        // that does not exist.
        if rows.is_empty() {
            return Ok(());
        }
        // `?x p ?x` is bounded only when every row fixes `?x`. An absent or
        // UNDEF cell leaves an equality filter over a non-contiguous
        // enumeration, whose rejected candidates are not bounded by limit.
        for variable in pattern.repeated_variables() {
            let Some(column) = columns.get(variable) else {
                return Err(unbounded_repeated_variable(variable));
            };
            if rows.iter().any(|row| row[*column].bound().is_none()) {
                return Err(unbounded_repeated_variable(variable));
            }
        }
        Ok(())
    }

    fn rows<'a>(&'a self, pattern: &'a BindingPattern) -> impl Iterator<Item = BindingRow<'a>> {
        self.rows
            .iter()
            .enumerate()
            .map(move |(index, values)| BindingRow {
                index: u32::try_from(index).expect("max_bindings is a u32"),
                pattern,
                columns: &self.columns,
                values,
            })
    }

    fn row_count(&self) -> u64 {
        self.rows.len() as u64
    }

    fn column_count(&self) -> u64 {
        self.variables.len() as u64
    }

    fn canonicalize(&self, mut output: String) -> String {
        push_canonical(&mut output, &self.variables.len().to_string());
        for variable in &self.variables {
            push_canonical(&mut output, variable.as_str());
        }
        push_canonical(&mut output, &self.rows.len().to_string());
        for row in &self.rows {
            for value in row {
                match value {
                    BindingValue::Bound(term) => {
                        output.push('t');
                        push_canonical(&mut output, term.dictionary());
                    }
                    BindingValue::Undef => output.push('u'),
                }
            }
        }
        output
    }
}

fn unbounded_repeated_variable(variable: &Variable) -> Problem {
    Problem::new(
        ErrorCode::MalformedRequest,
        format!(
            "repeated pattern variable {} must be bound in every input row; leaving it UNDEF \
             would require an unbudgeted equality scan",
            reflected(variable.as_str())
        ),
    )
}

/// `QUERY|POST /fragment` — a pattern restricted by an input binding table.
#[derive(Debug)]
pub struct BindingFragment {
    /// The explicit body pattern.
    pub pattern: BindingPattern,
    bindings: Bindings,
    /// Global result-row limit across the whole input table.
    pub limit: u32,
    /// Bytes its rows may occupy.
    pub bytes: ResponseBytes,
    /// Candidates an RDF projection may examine while removing overlaps.
    pub candidates: Candidates,
    /// Where to resume, if the body carried a cursor.
    pub cursor: Option<Cursor>,
    /// What a cursor this request issues must match.
    pub binding: CursorBinding,
    /// GET RDF is the distinct brTPF projection; native bodies preserve the
    /// complete compatibility relation and its binding indices.
    distinct_rdf: bool,
}

impl BindingFragment {
    /// Parse one strict JSON request body.
    pub fn parse(
        params: &Params,
        body: &[u8],
        limits: Limits<'_>,
        prefixes: &PrefixMap,
        bundle: &BundleBinding,
    ) -> Result<Self, Problem> {
        accept_only(params, FRAGMENT, &["format"])?;
        let wire: WireBindingFragment = parse_body(body)?;
        let pattern = BindingPattern::parse(wire.pattern, limits, prefixes)?;
        let bindings = Bindings::parse(wire.bindings, &pattern, limits, prefixes)?;
        let limit = body_page_size(wire.limit, limits)?;
        let canonical = bindings.canonicalize(pattern.canonicalize(String::new()));
        let binding = CursorBinding::new(
            bundle,
            &CanonicalRequest::new(Operation::Fragment).with("bindings", &canonical),
        );
        let cursor = wire
            .cursor
            .as_deref()
            .map(|token| Cursor::decode(token, &binding).map_err(Problem::from))
            .transpose()?;
        Ok(Self {
            pattern,
            bindings,
            limit,
            bytes: ResponseBytes(limits.budgets.max_response_bytes),
            candidates: Candidates(limits.budgets.candidate_budget),
            cursor,
            binding,
            distinct_rdf: false,
        })
    }

    /// Parse Comunica's brTPF GET transport: a variable-preserving `s/p/o`
    /// pattern plus SPARQL VALUES syntax without the `VALUES` keyword.
    fn parse_values(
        params: &Params,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
        bundle: &BundleBinding,
        distinct_rdf: bool,
    ) -> Result<Self, Problem> {
        accept_only(
            params,
            FRAGMENT,
            &["s", "p", "o", "values", "limit", "cursor", "format"],
        )?;
        let pattern = BindingPattern::parse_get(params, limits, prefixes)?;
        let values = params.get("values").expect("the caller selected values=");
        let bindings = Bindings::parse_values(values, &pattern, limits)?;
        let limit = page_size(
            params,
            "limit",
            limits.caps.default_limit,
            limits.caps.max_limit,
            "use /count for a cardinality on its own",
        )?;
        let canonical = bindings.canonicalize(pattern.canonicalize(String::new()));
        let binding = CursorBinding::new(
            bundle,
            &CanonicalRequest::new(Operation::Fragment).with("bindings", &canonical),
        );
        Ok(Self {
            pattern,
            bindings,
            limit,
            bytes: ResponseBytes(limits.budgets.max_response_bytes),
            candidates: Candidates(limits.budgets.candidate_budget),
            cursor: resume(params, &binding)?,
            binding,
            distinct_rdf,
        })
    }

    /// Parse the variable-preserving URL emitted by a source typed `brtpf`
    /// before a bind-join block is available. Comunica includes `?name`
    /// positions on every request to such a source, even when it has no
    /// `values=` restriction yet.
    fn parse_variable_get(
        params: &Params,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
        bundle: &BundleBinding,
        distinct_rdf: bool,
    ) -> Result<Self, Problem> {
        accept_only(
            params,
            FRAGMENT,
            &["s", "p", "o", "limit", "cursor", "format"],
        )?;
        let pattern = BindingPattern::parse_get(params, limits, prefixes)?;
        let bindings = Bindings::parse(
            WireBindings {
                vars: Vec::new(),
                rows: vec![Vec::new()],
            },
            &pattern,
            limits,
            prefixes,
        )?;
        let limit = page_size(
            params,
            "limit",
            limits.caps.default_limit,
            limits.caps.max_limit,
            "use /count for a cardinality on its own",
        )?;
        let canonical = bindings.canonicalize(pattern.canonicalize(String::new()));
        let binding = CursorBinding::new(
            bundle,
            &CanonicalRequest::new(Operation::Fragment).with("bindings", &canonical),
        );
        Ok(Self {
            pattern,
            bindings,
            limit,
            bytes: ResponseBytes(limits.budgets.max_response_bytes),
            candidates: Candidates(limits.budgets.candidate_budget),
            cursor: resume(params, &binding)?,
            binding,
            distinct_rdf,
        })
    }

    /// Input rows in their contractual enumeration order.
    pub fn rows(&self) -> impl Iterator<Item = BindingRow<'_>> {
        self.bindings.rows(&self.pattern)
    }

    /// Whether this transport returns brTPF's distinct RDF projection rather
    /// than KGF's native compatibility relation.
    pub(crate) fn distinct_rdf(&self) -> bool {
        self.distinct_rdf
    }
}

/// `QUERY|POST /count` — one exact cardinality per input binding row.
#[derive(Debug)]
pub struct BindingCount {
    /// The explicit body pattern.
    pub pattern: BindingPattern,
    bindings: Bindings,
}

impl BindingCount {
    /// Parse one strict JSON request body.
    pub fn parse(
        params: &Params,
        body: &[u8],
        limits: Limits<'_>,
        prefixes: &PrefixMap,
    ) -> Result<Self, Problem> {
        accept_only(params, COUNT, &["format"])?;
        let wire: WireBindingCount = parse_body(body)?;
        let pattern = BindingPattern::parse(wire.pattern, limits, prefixes)?;
        let bindings = Bindings::parse(wire.bindings, &pattern, limits, prefixes)?;
        Ok(Self { pattern, bindings })
    }

    /// Input rows in their contractual enumeration order.
    pub fn rows(&self) -> impl Iterator<Item = BindingRow<'_>> {
        self.bindings.rows(&self.pattern)
    }
}

fn position_index(position: Position) -> usize {
    match position {
        Position::Subject => 0,
        Position::Predicate => 1,
        Position::Object => 2,
    }
}

fn push_canonical(output: &mut String, value: &str) {
    use std::fmt::Write as _;
    write!(output, "{}:", value.len()).expect("writing to a String cannot fail");
    output.push_str(value);
}

fn body_page_size(value: Option<u32>, limits: Limits<'_>) -> Result<u32, Problem> {
    let value = value.unwrap_or(limits.caps.default_limit.min(limits.caps.max_limit));
    if value == 0 {
        return Err(Problem::new(
            ErrorCode::MalformedRequest,
            "limit=0 asks for nothing back; use QUERY /count for cardinalities",
        ));
    }
    if value > limits.caps.max_limit {
        return Err(Problem::new(
            ErrorCode::CapExceeded,
            format!(
                "limit={value} is over this server's max_limit of {}",
                limits.caps.max_limit
            ),
        ));
    }
    Ok(value)
}

fn parse_body<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, Problem> {
    serde_json::from_slice(body).map_err(|error| {
        Problem::new(
            ErrorCode::MalformedRequest,
            format!("the JSON request body is malformed: {error}"),
        )
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBindingFragment {
    pattern: WirePattern,
    bindings: WireBindings,
    limit: Option<u32>,
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBindingCount {
    pattern: WirePattern,
    bindings: WireBindings,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLabels {
    iris: Vec<WireTerm>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePattern {
    s: WireTerm,
    p: WireTerm,
    o: WireTerm,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBindings {
    vars: Vec<String>,
    rows: Vec<Vec<WireTerm>>,
}

/// One term cell, kept as JSON until its position is known.
///
/// Deserializing directly into the valid term-object shape would reject a
/// malformed object before [`BoundTerm::parse_body`] can attach `pattern.p` or
/// `bindings.rows[7][0]` and the specific remedy from [`Term::from_json`]. The
/// outer body shape is still strict; only a term cell delays interpretation to
/// the parser that has the context needed for an actionable error.
#[derive(Debug)]
enum WireTerm {
    Text(String),
    /// Entries rather than a map so duplicate keys survive until the
    /// contextual parser can reject them by name.
    Object(Vec<(String, serde_json::Value)>),
    Other(serde_json::Value),
}

impl<'de> Deserialize<'de> for WireTerm {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(WireTermVisitor)
    }
}

struct WireTermVisitor;

impl<'de> Visitor<'de> for WireTermVisitor {
    type Value = WireTerm;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a compact term string or JSON term object")
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(WireTerm::Text(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(WireTerm::Text(value.to_owned()))
    }

    fn visit_map<A>(self, mut entries: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut fields = Vec::with_capacity(entries.size_hint().unwrap_or(0));
        while let Some(field) = entries.next_entry()? {
            fields.push(field);
        }
        Ok(WireTerm::Object(fields))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
        while let Some(value) = sequence.next_element()? {
            values.push(value);
        }
        Ok(WireTerm::Other(serde_json::Value::Array(values)))
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(WireTerm::Other(serde_json::Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(WireTerm::Other(serde_json::Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(WireTerm::Other(serde_json::Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let number = serde_json::Number::from_f64(value)
            .ok_or_else(|| E::custom("a JSON number must be finite"))?;
        Ok(WireTerm::Other(serde_json::Value::Number(number)))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(WireTerm::Other(serde_json::Value::Null))
    }
}

/// `GET /fragment` — a paged triple pattern.
#[derive(Debug)]
pub struct Fragment {
    /// The pattern to enumerate, text constraint included.
    pub pattern: Pattern,
    /// Rows this page may carry.
    pub limit: u32,
    /// Bytes its rows may occupy.
    pub bytes: ResponseBytes,
    /// Candidates a text constraint may examine.
    pub candidates: Candidates,
    /// Where to resume, if the request carried a cursor.
    pub cursor: Option<Cursor>,
    /// What a cursor this request issues must match.
    pub binding: CursorBinding,
    /// RDF fitting performs bounded repeated complete-document serialization.
    rdf_serialization: bool,
}

/// The two GET grammars of the one fragment operation.
#[derive(Debug)]
pub enum GetFragment {
    /// Ordinary TPF/KGF query parameters.
    Plain(Fragment),
    /// A brTPF variable pattern restricted by `values=`.
    Values(BindingFragment),
    /// A brTPF variable pattern with no `values=` restriction yet.
    Variables(BindingFragment),
}

impl GetFragment {
    /// Select the grammar by the presence of `values=` and normalize both to
    /// the existing typed operation requests.
    pub fn parse(
        params: &Params,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
        bundle: &BundleBinding,
        rdf_representation: bool,
    ) -> Result<Self, Problem> {
        if params.get("values").is_some() {
            BindingFragment::parse_values(params, limits, prefixes, bundle, rdf_representation)
                .map(Self::Values)
        } else if Position::ALL.into_iter().any(|position| {
            params
                .get(position.as_str())
                .is_some_and(|value| value.starts_with('?'))
        }) {
            BindingFragment::parse_variable_get(
                params,
                limits,
                prefixes,
                bundle,
                rdf_representation,
            )
            .map(Self::Variables)
        } else {
            Fragment::parse_with(params, limits, prefixes, bundle, rdf_representation)
                .map(Self::Plain)
        }
    }

    /// A text filter exists only on the ordinary KGF grammar.
    pub fn text(&self) -> Option<&TextFilter> {
        match self {
            Self::Plain(request) => request.pattern.text(),
            Self::Values(_) | Self::Variables(_) => None,
        }
    }
}

impl Fragment {
    const PARAMETERS: &'static [&'static str] =
        &["s", "p", "o", "o.text", "limit", "cursor", "format"];

    /// Read the parameters of a `/fragment` request.
    pub fn parse(
        params: &Params,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
        bundle: &BundleBinding,
    ) -> Result<Self, Problem> {
        Self::parse_with(params, limits, prefixes, bundle, false)
    }

    fn parse_with(
        params: &Params,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
        bundle: &BundleBinding,
        rdf_representation: bool,
    ) -> Result<Self, Problem> {
        accept_only(params, FRAGMENT, Self::PARAMETERS)?;
        let pattern = Pattern::parse_with(params, limits, prefixes, rdf_representation)?;
        let limit = page_size(
            params,
            "limit",
            limits.caps.default_limit,
            limits.caps.max_limit,
            "use /count for a cardinality on its own",
        )?;
        let binding = CursorBinding::new(
            bundle,
            &pattern.canonicalize(CanonicalRequest::new(Operation::Fragment)),
        );
        Ok(Self {
            pattern,
            limit,
            bytes: ResponseBytes(limits.budgets.max_response_bytes),
            candidates: Candidates(limits.budgets.candidate_budget),
            cursor: resume(params, &binding)?,
            binding,
            rdf_serialization: rdf_representation,
        })
    }
}

/// `GET /count` — a cardinality and nothing else.
#[derive(Debug)]
pub struct Count {
    /// The pattern to count, text constraint included.
    pub pattern: Pattern,
    /// Candidates a text constraint may examine.
    pub candidates: Candidates,
    /// Where to resume a budgeted text scan.
    pub cursor: Option<Cursor>,
    /// What a cursor this count issues must match.
    pub binding: CursorBinding,
}

impl Count {
    /// No `limit`: each request spends at most the published candidate budget.
    const PARAMETERS: &'static [&'static str] = &["s", "p", "o", "o.text", "cursor", "format"];

    /// Read the parameters of a `/count` request.
    pub fn parse(
        params: &Params,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
        bundle: &BundleBinding,
    ) -> Result<Self, Problem> {
        accept_only(params, COUNT, Self::PARAMETERS)?;
        let pattern = Pattern::parse(params, limits, prefixes)?;
        let binding = CursorBinding::new(
            bundle,
            &pattern.canonicalize(CanonicalRequest::new(Operation::Count)),
        );
        let cursor = resume(params, &binding)?;
        if cursor.is_some() && pattern.text().is_none() {
            // Ordinary counts finish in one bounded descent and never issue a
            // continuation. Accepting one would mean silently ignoring it.
            return Err(Problem::from(StaleCursor));
        }
        Ok(Self {
            pattern,
            candidates: Candidates(limits.budgets.candidate_budget),
            cursor,
            binding,
        })
    }
}

/// `GET /describe` — a resource's neighborhood, paged.
#[derive(Debug)]
pub struct Describe {
    /// The resource whose edges are wanted.
    pub resource: BoundTerm,
    /// Which edges.
    pub direction: Direction,
    /// Rows this page may carry.
    pub limit: u32,
    /// Bytes its rows may occupy.
    pub bytes: ResponseBytes,
    /// Where to resume, if the request carried a cursor.
    pub cursor: Option<Cursor>,
    /// What a cursor this request issues must match.
    pub binding: CursorBinding,
}

impl Describe {
    const PARAMETERS: &'static [&'static str] = &["iri", "direction", "limit", "cursor", "format"];

    /// Read the parameters of a `/describe` request.
    pub fn parse(
        params: &Params,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
        bundle: &BundleBinding,
    ) -> Result<Self, Problem> {
        accept_only(params, DESCRIBE, Self::PARAMETERS)?;
        let Some(text) = params.get("iri") else {
            return Err(Problem::new(
                ErrorCode::MalformedRequest,
                "describe needs an `iri` naming the resource to describe, \
                 as `iri=<http://example.org/a>` or `iri=ex:a`",
            ));
        };
        // Named `iri` for compatibility, but any term is accepted: a blank node has a
        // neighborhood, and so does a literal, which has incoming edges like
        // any other object. Refusing them would make part of a bundle
        // unreachable to answer a question nobody asked.
        let resource = BoundTerm::parse("iri", text, limits, prefixes)?;
        let direction = Direction::parse(params)?;
        let limit = page_size(
            params,
            "limit",
            limits.caps.default_limit,
            limits.caps.max_limit,
            "use /count for a cardinality on its own",
        )?;
        let binding = CursorBinding::new(
            bundle,
            &CanonicalRequest::new(Operation::Describe)
                .with("iri", resource.dictionary())
                .with("direction", direction.as_str()),
        );
        Ok(Self {
            resource,
            direction,
            limit,
            bytes: ResponseBytes(limits.budgets.max_response_bytes),
            cursor: resume(params, &binding)?,
            binding,
        })
    }
}

/// One valid node selector for `/schema`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaSelection {
    /// The selected view's dataset root.
    Dataset,
    /// One class partition.
    Class {
        /// The selected class IRI.
        class: BoundTerm,
    },
    /// One dataset- or class-scoped property partition.
    Property {
        /// The optional subject class scope.
        class: Option<BoundTerm>,
        /// The selected predicate IRI.
        predicate: BoundTerm,
    },
    /// One dataset- or class-scoped datatype partition.
    Datatype {
        /// The optional subject class scope.
        class: Option<BoundTerm>,
        /// The selected predicate IRI.
        predicate: BoundTerm,
        /// The selected datatype IRI.
        datatype: BoundTerm,
    },
}

impl SchemaSelection {
    /// Borrow this request selection in the store layer's id-resolution shape.
    pub fn store_selector(&self) -> StoreSchemaSelector<'_> {
        match self {
            Self::Dataset => StoreSchemaSelector::Dataset,
            Self::Class { class } => StoreSchemaSelector::Class {
                class: class.dictionary(),
            },
            Self::Property { class, predicate } => StoreSchemaSelector::Property {
                class: class.as_ref().map(BoundTerm::dictionary),
                predicate: predicate.dictionary(),
            },
            Self::Datatype {
                class,
                predicate,
                datatype,
            } => StoreSchemaSelector::Datatype {
                class: class.as_ref().map(BoundTerm::dictionary),
                predicate: predicate.dictionary(),
                datatype: datatype.dictionary(),
            },
        }
    }

    fn canonicalize(&self, request: CanonicalRequest) -> CanonicalRequest {
        match self {
            Self::Dataset => request,
            Self::Class { class } => request.with("class", class.dictionary()),
            Self::Property { class, predicate } => request
                .with_opt("class", class.as_ref().map(BoundTerm::dictionary))
                .with("predicate", predicate.dictionary()),
            Self::Datatype {
                class,
                predicate,
                datatype,
            } => request
                .with_opt("class", class.as_ref().map(BoundTerm::dictionary))
                .with("predicate", predicate.dictionary())
                .with("datatype", datatype.dictionary()),
        }
    }
}

/// One valid immediate-child collection request.
///
/// The variants pair a collection with the only selector shapes under which
/// the API permits, so parsing cannot produce (for example) languages below
/// a property or classes below a class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaChildren {
    /// Classes below the selected view's dataset root.
    Classes,
    /// Properties below the selected view's dataset root.
    DatasetProperties,
    /// Properties below one class partition.
    ClassProperties {
        /// The selected class IRI.
        class: BoundTerm,
    },
    /// Object classes below one property partition.
    PropertyObjectClasses {
        /// The optional subject class scope.
        class: Option<BoundTerm>,
        /// The selected predicate IRI.
        predicate: BoundTerm,
    },
    /// Datatypes below one property partition.
    PropertyDatatypes {
        /// The optional subject class scope.
        class: Option<BoundTerm>,
        /// The selected predicate IRI.
        predicate: BoundTerm,
    },
    /// Languages below one datatype partition.
    DatatypeLanguages {
        /// The optional subject class scope.
        class: Option<BoundTerm>,
        /// The selected predicate IRI.
        predicate: BoundTerm,
        /// The selected datatype IRI.
        datatype: BoundTerm,
    },
}

impl SchemaChildren {
    /// Borrow this request in the store layer's typed child-query shape.
    pub fn store_query(&self) -> StoreSchemaChildQuery<'_> {
        match self {
            Self::Classes => StoreSchemaChildQuery::Classes,
            Self::DatasetProperties => StoreSchemaChildQuery::DatasetProperties,
            Self::ClassProperties { class } => StoreSchemaChildQuery::ClassProperties {
                class: class.dictionary(),
            },
            Self::PropertyObjectClasses { class, predicate } => {
                StoreSchemaChildQuery::PropertyObjectClasses {
                    class: class.as_ref().map(BoundTerm::dictionary),
                    predicate: predicate.dictionary(),
                }
            }
            Self::PropertyDatatypes { class, predicate } => {
                StoreSchemaChildQuery::PropertyDatatypes {
                    class: class.as_ref().map(BoundTerm::dictionary),
                    predicate: predicate.dictionary(),
                }
            }
            Self::DatatypeLanguages {
                class,
                predicate,
                datatype,
            } => StoreSchemaChildQuery::DatatypeLanguages {
                class: class.as_ref().map(BoundTerm::dictionary),
                predicate: predicate.dictionary(),
                datatype: datatype.dictionary(),
            },
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Classes => "classes",
            Self::DatasetProperties | Self::ClassProperties { .. } => "properties",
            Self::PropertyObjectClasses { .. } => "object-classes",
            Self::PropertyDatatypes { .. } => "datatypes",
            Self::DatatypeLanguages { .. } => "languages",
        }
    }

    fn canonicalize(&self, request: CanonicalRequest) -> CanonicalRequest {
        let request = match self {
            Self::Classes | Self::DatasetProperties => request,
            Self::ClassProperties { class } => request.with("class", class.dictionary()),
            Self::PropertyObjectClasses { class, predicate }
            | Self::PropertyDatatypes { class, predicate } => request
                .with_opt("class", class.as_ref().map(BoundTerm::dictionary))
                .with("predicate", predicate.dictionary()),
            Self::DatatypeLanguages {
                class,
                predicate,
                datatype,
            } => request
                .with_opt("class", class.as_ref().map(BoundTerm::dictionary))
                .with("predicate", predicate.dictionary())
                .with("datatype", datatype.dictionary()),
        };
        request.with("children", self.name())
    }
}

/// Optional filters for `/schema?projection=class-relations`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaRelationFilter {
    /// Subject-class IRI to retain.
    pub class: Option<BoundTerm>,
    /// Predicate IRI to retain.
    pub predicate: Option<BoundTerm>,
}

impl SchemaRelationFilter {
    /// Borrow the filters in the store layer's persisted-projection shape.
    pub fn store_filter(&self) -> StoreClassRelationFilter<'_> {
        StoreClassRelationFilter {
            class: self.class.as_ref().map(BoundTerm::dictionary),
            predicate: self.predicate.as_ref().map(BoundTerm::dictionary),
        }
    }

    fn canonicalize(&self, request: CanonicalRequest) -> CanonicalRequest {
        request
            .with("projection", "class-relations")
            .with_opt("class", self.class.as_ref().map(BoundTerm::dictionary))
            .with_opt(
                "predicate",
                self.predicate.as_ref().map(BoundTerm::dictionary),
            )
    }
}

/// Optional filters for `/schema?projection=class-properties`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaClassPropertyFilter {
    /// Subject-class IRI to retain.
    pub class: Option<BoundTerm>,
    /// Predicate IRI to retain.
    pub predicate: Option<BoundTerm>,
}

impl SchemaClassPropertyFilter {
    /// Borrow the filters in the store layer's persisted-projection shape.
    pub fn store_filter(&self) -> StoreClassPropertyFilter<'_> {
        StoreClassPropertyFilter {
            class: self.class.as_ref().map(BoundTerm::dictionary),
            predicate: self.predicate.as_ref().map(BoundTerm::dictionary),
        }
    }

    fn canonicalize(&self, request: CanonicalRequest) -> CanonicalRequest {
        request
            .with("projection", "class-properties")
            .with_opt("class", self.class.as_ref().map(BoundTerm::dictionary))
            .with_opt(
                "predicate",
                self.predicate.as_ref().map(BoundTerm::dictionary),
            )
    }
}

/// The mutually exclusive result shapes of one `/schema` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaQuery {
    /// Describe one selected node without paging a child collection.
    Node(SchemaSelection),
    /// Describe one node and page one valid immediate-child collection.
    Children(SchemaChildren),
    /// Page the persisted flat class-relation projection.
    ClassRelations(SchemaRelationFilter),
    /// Page the persisted count-ranked class-property inventory.
    ClassProperties(SchemaClassPropertyFilter),
}

impl SchemaQuery {
    fn canonicalize(&self, request: CanonicalRequest) -> CanonicalRequest {
        match self {
            Self::Node(selection) => selection.canonicalize(request),
            Self::Children(children) => children.canonicalize(request),
            Self::ClassRelations(filter) => filter.canonicalize(request),
            Self::ClassProperties(filter) => filter.canonicalize(request),
        }
    }

    fn position_space(&self) -> Option<crate::cursor::PositionSpace> {
        match self {
            Self::Node(_) => None,
            Self::Children(_) => Some(crate::cursor::PositionSpace::SchemaChild),
            Self::ClassRelations(_) => Some(crate::cursor::PositionSpace::ClassRelation),
            Self::ClassProperties(_) => Some(crate::cursor::PositionSpace::ClassProperty),
        }
    }
}

/// `GET /schema` — one bounded description drill-down.
#[derive(Debug)]
pub struct Schema {
    /// The description layer whose numbers are selected.
    pub view: StatsView,
    /// Node-only, one typed child collection, or flat class relations.
    pub query: SchemaQuery,
    /// Child or class-relation rows this page may carry; absent for node-only requests.
    pub limit: Option<u32>,
    /// Bytes its rows may occupy.
    pub bytes: ResponseBytes,
    /// Rows a filtered class-relation page may examine.
    pub candidates: Candidates,
    /// Whether the JSON response should carry the preferred label (or an
    /// explicit null) for each distinct schema IRI on the page.
    pub labels: bool,
    /// Where to resume, if the request carried a cursor.
    pub cursor: Option<Cursor>,
    /// What a cursor this request issues must match.
    pub binding: CursorBinding,
}

/// `GET /void` — the complete VoID graph in one RDF representation.
#[derive(Debug, Clone, Copy)]
pub struct Void {
    /// Maximum serialized bytes before the graph is returned as a valid,
    /// incomplete RDF document.
    pub bytes: ResponseBytes,
}

impl Void {
    /// Parse the representation-only request before opening a bundle.
    pub fn parse(params: &Params, limits: Limits<'_>) -> Result<Self, Problem> {
        accept_only(params, VOID, &["format"])?;
        Ok(Self {
            bytes: ResponseBytes(limits.budgets.max_response_bytes),
        })
    }
}

/// `GET /summary` — one persisted summary card.
#[derive(Debug, Clone, Copy)]
pub struct Summary;

impl Summary {
    /// Refuse every query control except representation selection.
    pub fn parse(params: &Params) -> Result<Self, Problem> {
        accept_only(params, SUMMARY, &["format"])?;
        Ok(Self)
    }
}

impl Schema {
    const PARAMETERS: &'static [&'static str] = &[
        "class",
        "predicate",
        "datatype",
        "children",
        "projection",
        "view",
        "limit",
        "cursor",
        "labels",
        "format",
    ];

    /// Parse and type one `/schema` query before a bundle is opened.
    pub fn parse(
        params: &Params,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
        bundle: &BundleBinding,
    ) -> Result<Self, Problem> {
        accept_only(params, SCHEMA, Self::PARAMETERS)?;
        let view = schema_view(params)?;
        let query = parse_schema_query(params, limits, prefixes)?;
        let labels = boolean(params, "labels", false)?;
        let limit = match &query {
            SchemaQuery::Node(_) => {
                if params.get("limit").is_some() {
                    return Err(Problem::new(
                        ErrorCode::MalformedRequest,
                        "`limit` applies only when `children` or `projection=class-relations` pages schema items",
                    ));
                }
                None
            }
            SchemaQuery::Children(_)
            | SchemaQuery::ClassRelations(_)
            | SchemaQuery::ClassProperties(_) => Some(page_size(
                params,
                "limit",
                limits.caps.default_limit,
                limits.caps.max_schema_items,
                "ask for at least one schema item",
            )?),
        };
        let canonical = query.canonicalize(canonicalize_schema_view(
            &view,
            CanonicalRequest::new(Operation::Schema),
        ));
        let binding = CursorBinding::new(bundle, &canonical);
        let cursor = resume(params, &binding)?;
        if cursor.as_ref().is_some_and(|cursor| {
            Some(cursor.space) != query.position_space()
                || cursor.binding_index.is_some()
                || cursor.scan_position.is_some()
        }) {
            return Err(Problem::from(StaleCursor));
        }
        Ok(Self {
            view,
            query,
            limit,
            bytes: ResponseBytes(limits.budgets.max_response_bytes),
            candidates: Candidates(limits.budgets.candidate_budget),
            labels,
            cursor,
            binding,
        })
    }
}

fn schema_view(params: &Params) -> Result<StatsView, Problem> {
    match params.get("view") {
        None | Some("design") => Ok(StatsView::Design),
        Some("queryable") => Ok(StatsView::Queryable),
        Some(value) => value
            .strip_prefix("component:")
            .and_then(StatsView::component)
            .ok_or_else(|| {
                Problem::new(
                    ErrorCode::MalformedRequest,
                    format!(
                        "view={} is not a schema view; use `design`, `queryable`, or `component:<id>`",
                        reflected(value)
                    ),
                )
            }),
    }
}

fn canonicalize_schema_view(view: &StatsView, request: CanonicalRequest) -> CanonicalRequest {
    match view {
        StatsView::Design => request.with("view", "design"),
        StatsView::Queryable => request.with("view", "queryable"),
        StatsView::Component(component) => {
            request.with("view", &format!("component:{}", component.as_str()))
        }
    }
}

fn parse_schema_query(
    params: &Params,
    limits: Limits<'_>,
    prefixes: &PrefixMap,
) -> Result<SchemaQuery, Problem> {
    if let Some(projection) = params.get("projection") {
        if !matches!(projection, "class-relations" | "class-properties") {
            return Err(Problem::new(
                ErrorCode::MalformedRequest,
                format!(
                    "projection={} is not a schema projection; use `class-relations`, `class-properties`, or omit it",
                    reflected(projection)
                ),
            ));
        }
        if params.get("children").is_some() || params.get("datatype").is_some() {
            return Err(Problem::new(
                ErrorCode::MalformedRequest,
                "schema projections accept `class` and `predicate` filters, but not `children` or `datatype`",
            ));
        }
        let class = schema_iri(params, "class", limits, prefixes)?;
        let predicate = schema_iri(params, "predicate", limits, prefixes)?;
        return Ok(match projection {
            "class-relations" => {
                SchemaQuery::ClassRelations(SchemaRelationFilter { class, predicate })
            }
            "class-properties" => {
                SchemaQuery::ClassProperties(SchemaClassPropertyFilter { class, predicate })
            }
            _ => unreachable!("projection spelling checked above"),
        });
    }

    let selection = match (
        schema_iri(params, "class", limits, prefixes)?,
        schema_iri(params, "predicate", limits, prefixes)?,
        schema_iri(params, "datatype", limits, prefixes)?,
    ) {
        (None, None, None) => SchemaSelection::Dataset,
        (Some(class), None, None) => SchemaSelection::Class { class },
        (class, Some(predicate), None) => SchemaSelection::Property { class, predicate },
        (class, Some(predicate), Some(datatype)) => SchemaSelection::Datatype {
            class,
            predicate,
            datatype,
        },
        (_, None, Some(_)) => {
            return Err(Problem::new(
                ErrorCode::MalformedRequest,
                "`datatype` selects a partition beneath a predicate; add `predicate` or omit `datatype`",
            ));
        }
    };

    let Some(children) = params.get("children") else {
        return Ok(SchemaQuery::Node(selection));
    };
    let collection = RequestedSchemaCollection::parse(children)?;
    Ok(SchemaQuery::Children(collection.beneath(selection)?))
}

fn schema_iri(
    params: &Params,
    name: &str,
    limits: Limits<'_>,
    prefixes: &PrefixMap,
) -> Result<Option<BoundTerm>, Problem> {
    params
        .get(name)
        .map(|value| BoundTerm::parse(name, value, limits, prefixes)?.require_iri(name))
        .transpose()
}

#[derive(Debug, Clone, Copy)]
enum RequestedSchemaCollection {
    Classes,
    Properties,
    ObjectClasses,
    Datatypes,
    Languages,
}

impl RequestedSchemaCollection {
    fn parse(value: &str) -> Result<Self, Problem> {
        match value {
            "classes" => Ok(Self::Classes),
            "properties" => Ok(Self::Properties),
            "object-classes" => Ok(Self::ObjectClasses),
            "datatypes" => Ok(Self::Datatypes),
            "languages" => Ok(Self::Languages),
            _ => Err(Problem::new(
                ErrorCode::MalformedRequest,
                format!(
                    "children={} is not a schema collection; use `classes`, `properties`, `object-classes`, `datatypes`, or `languages`",
                    reflected(value)
                ),
            )),
        }
    }

    fn beneath(self, selection: SchemaSelection) -> Result<SchemaChildren, Problem> {
        match (selection, self) {
            (SchemaSelection::Dataset, Self::Classes) => Ok(SchemaChildren::Classes),
            (SchemaSelection::Dataset, Self::Properties) => Ok(SchemaChildren::DatasetProperties),
            (SchemaSelection::Class { class }, Self::Properties) => {
                Ok(SchemaChildren::ClassProperties { class })
            }
            (SchemaSelection::Property { class, predicate }, Self::ObjectClasses) => {
                Ok(SchemaChildren::PropertyObjectClasses { class, predicate })
            }
            (SchemaSelection::Property { class, predicate }, Self::Datatypes) => {
                Ok(SchemaChildren::PropertyDatatypes { class, predicate })
            }
            (
                SchemaSelection::Datatype {
                    class,
                    predicate,
                    datatype,
                },
                Self::Languages,
            ) => Ok(SchemaChildren::DatatypeLanguages {
                class,
                predicate,
                datatype,
            }),
            (selection, collection) => Err(Problem::new(
                ErrorCode::MalformedRequest,
                format!(
                    "children={} is not available beneath a {} selector",
                    collection.as_str(),
                    selection.kind()
                ),
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Classes => "classes",
            Self::Properties => "properties",
            Self::ObjectClasses => "object-classes",
            Self::Datatypes => "datatypes",
            Self::Languages => "languages",
        }
    }
}

impl SchemaSelection {
    fn kind(&self) -> &'static str {
        match self {
            Self::Dataset => "dataset",
            Self::Class { .. } => "class",
            Self::Property { .. } => "property",
            Self::Datatype { .. } => "datatype",
        }
    }
}

/// `GET /sample` — pseudo-random members of a pattern's result set.
#[derive(Debug)]
pub struct Sample {
    /// The pattern to sample.
    pub pattern: Pattern,
    /// How many members to draw.
    pub n: u32,
    /// The draw's seed.
    pub seed: u64,
    /// Bytes the drawn members may occupy.
    pub bytes: ResponseBytes,
}

impl Sample {
    /// No `cursor`: a sample is drawn whole and has no position to resume from,
    /// which is why [`Operation`] has no variant for it.
    const PARAMETERS: &'static [&'static str] = &["s", "p", "o", "n", "seed", "format"];

    /// Read the parameters of a `/sample` request.
    pub fn parse(
        params: &Params,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
    ) -> Result<Self, Problem> {
        accept_only(params, SAMPLE, Self::PARAMETERS)?;
        let pattern = Pattern::parse(params, limits, prefixes)?;
        let n = page_size(
            params,
            "n",
            limits.caps.default_limit,
            limits.caps.max_sample,
            "ask for at least one member",
        )?;
        Ok(Self {
            pattern,
            n,
            seed: seed(params)?,
            bytes: ResponseBytes(limits.budgets.max_response_bytes),
        })
    }
}

/// Content-free metadata shared by GET and body-carried request types.
pub(crate) trait ObservedRequest {
    /// Parsed structure and magnitudes, excluding client-supplied values.
    fn shape(&self) -> RequestShape;

    /// Whether this request resumes a previous page.
    fn resumed(&self) -> bool {
        false
    }

    /// Canonical request hash used to bind cursors, when this operation has one.
    fn request_hash(&self) -> Option<[u8; 8]> {
        None
    }

    /// Search content for the explicitly enabled raw tier.
    fn raw_query(&self) -> Option<&str> {
        None
    }
}

/// Normalize the successful controls of one native GET form.
///
/// HTML submits untouched named controls as empty strings. Each request type
/// owns the decision about which of those controls are genuinely optional, so
/// the router can apply ordinary form semantics without making empty required,
/// unknown, cursor, representation, or selection parameters disappear.
pub(crate) trait GetRequest: ObservedRequest {
    /// Remove empty controls whose omission selects this request's default.
    fn normalize_params(params: &Params) -> Params;

    /// Whether this request explicitly asks for preferred labels in its
    /// machine representation.
    fn labels_requested(&self) -> bool {
        false
    }

    /// Host admission class for the work this parsed request will perform.
    fn work_class(&self) -> WorkClass {
        WorkClass::Ordinary
    }

    /// How the request arrived, when the grammar its parser selected says
    /// more than "GET".
    fn transport(&self) -> Transport {
        Transport::Get
    }
}

impl ObservedRequest for Fragment {
    fn shape(&self) -> RequestShape {
        RequestShape::Pattern {
            pattern: self.pattern.shape(),
            text: self.pattern.text().is_some(),
            limit: Some(self.limit),
        }
    }

    fn resumed(&self) -> bool {
        self.cursor.is_some()
    }

    fn request_hash(&self) -> Option<[u8; 8]> {
        Some(self.binding.request_hash())
    }
}

impl ObservedRequest for GetFragment {
    fn shape(&self) -> RequestShape {
        match self {
            Self::Plain(request) => request.shape(),
            Self::Values(request) | Self::Variables(request) => request.shape(),
        }
    }

    fn resumed(&self) -> bool {
        match self {
            Self::Plain(request) => request.resumed(),
            Self::Values(request) | Self::Variables(request) => request.resumed(),
        }
    }

    fn request_hash(&self) -> Option<[u8; 8]> {
        match self {
            Self::Plain(request) => request.request_hash(),
            Self::Values(request) | Self::Variables(request) => request.request_hash(),
        }
    }
}

impl ObservedRequest for Count {
    fn shape(&self) -> RequestShape {
        RequestShape::Pattern {
            pattern: self.pattern.shape(),
            text: self.pattern.text().is_some(),
            limit: None,
        }
    }

    fn resumed(&self) -> bool {
        self.cursor.is_some()
    }

    fn request_hash(&self) -> Option<[u8; 8]> {
        Some(self.binding.request_hash())
    }
}

impl ObservedRequest for Describe {
    fn shape(&self) -> RequestShape {
        RequestShape::Describe {
            term: self.resource.shape(),
            direction: self.direction.as_str(),
            limit: self.limit,
        }
    }

    fn resumed(&self) -> bool {
        self.cursor.is_some()
    }

    fn request_hash(&self) -> Option<[u8; 8]> {
        Some(self.binding.request_hash())
    }
}

impl ObservedRequest for Schema {
    fn shape(&self) -> RequestShape {
        let (selection, children, projection) = match &self.query {
            SchemaQuery::Node(selection) => (
                match selection {
                    SchemaSelection::Dataset => "root",
                    SchemaSelection::Class { .. } => "class",
                    SchemaSelection::Property { .. } => "predicate",
                    SchemaSelection::Datatype { .. } => "datatype",
                },
                None,
                None,
            ),
            SchemaQuery::Children(children) => {
                let selection = match children {
                    SchemaChildren::Classes | SchemaChildren::DatasetProperties => "root",
                    SchemaChildren::ClassProperties { .. } => "class",
                    SchemaChildren::PropertyObjectClasses { .. }
                    | SchemaChildren::PropertyDatatypes { .. } => "predicate",
                    SchemaChildren::DatatypeLanguages { .. } => "datatype",
                };
                (selection, Some(children.name()), None)
            }
            SchemaQuery::ClassRelations(_) => ("root", None, Some("class-relations")),
            SchemaQuery::ClassProperties(_) => ("root", None, Some("class-properties")),
        };
        let view = match &self.view {
            StatsView::Design => "design",
            StatsView::Queryable => "queryable",
            StatsView::Component(_) => "component",
        };
        RequestShape::Schema {
            selection,
            children,
            projection,
            view,
            limit: self.limit,
        }
    }

    fn resumed(&self) -> bool {
        self.cursor.is_some()
    }

    fn request_hash(&self) -> Option<[u8; 8]> {
        Some(self.binding.request_hash())
    }
}

impl ObservedRequest for Void {
    fn shape(&self) -> RequestShape {
        RequestShape::Empty {}
    }
}

impl ObservedRequest for Summary {
    fn shape(&self) -> RequestShape {
        RequestShape::Empty {}
    }
}

impl ObservedRequest for Sample {
    fn shape(&self) -> RequestShape {
        RequestShape::Sample {
            pattern: self.pattern.shape(),
            n: self.n,
        }
    }
}

impl ObservedRequest for Search {
    fn shape(&self) -> RequestShape {
        RequestShape::Search {
            q_len: self.query.query().len() as u64,
            roles: self.roles.clone(),
            predicates: self.predicates.len() as u64,
            limit: self.limit,
            labels: self.labels,
        }
    }

    fn raw_query(&self) -> Option<&str> {
        Some(self.query.query())
    }
}

impl ObservedRequest for BindingFragment {
    fn shape(&self) -> RequestShape {
        RequestShape::Bindings {
            pattern: self.pattern.shape(),
            text: false,
            limit: Some(self.limit),
            k: self.bindings.row_count(),
            columns: self.bindings.column_count(),
        }
    }

    fn resumed(&self) -> bool {
        self.cursor.is_some()
    }

    fn request_hash(&self) -> Option<[u8; 8]> {
        Some(self.binding.request_hash())
    }
}

impl ObservedRequest for BindingCount {
    fn shape(&self) -> RequestShape {
        RequestShape::Bindings {
            pattern: self.pattern.shape(),
            text: false,
            limit: None,
            k: self.bindings.row_count(),
            columns: self.bindings.column_count(),
        }
    }
}

impl ObservedRequest for Labels {
    fn shape(&self) -> RequestShape {
        RequestShape::Labels {
            k: self.iris.len() as u64,
        }
    }
}

/// Empty triple positions are unbound; `additional` names the operation's
/// other optional text or number controls.
fn normalize_pattern_params(params: &Params, additional: &[&str]) -> Params {
    let mut optional = Position::ALL
        .into_iter()
        .map(Position::as_str)
        .collect::<Vec<_>>();
    optional.extend_from_slice(additional);
    params.without_empty(&optional)
}

impl GetRequest for Fragment {
    fn normalize_params(params: &Params) -> Params {
        normalize_pattern_params(params, &["o.text", "limit"])
    }

    fn work_class(&self) -> WorkClass {
        if self.pattern.text().is_some() || self.rdf_serialization {
            WorkClass::Heavy
        } else {
            WorkClass::Ordinary
        }
    }
}

impl GetRequest for GetFragment {
    fn normalize_params(params: &Params) -> Params {
        normalize_pattern_params(params, &["o.text", "limit"])
    }

    fn work_class(&self) -> WorkClass {
        match self {
            Self::Plain(request) => request.work_class(),
            Self::Values(_) | Self::Variables(_) => WorkClass::Heavy,
        }
    }

    fn transport(&self) -> Transport {
        match self {
            Self::Values(_) => Transport::GetValues,
            Self::Plain(_) | Self::Variables(_) => Transport::Get,
        }
    }
}

impl GetRequest for Count {
    fn normalize_params(params: &Params) -> Params {
        normalize_pattern_params(params, &["o.text"])
    }

    fn work_class(&self) -> WorkClass {
        if self.pattern.text().is_some() {
            WorkClass::Heavy
        } else {
            WorkClass::Ordinary
        }
    }
}

impl GetRequest for Describe {
    fn normalize_params(params: &Params) -> Params {
        params.without_empty(&["limit"])
    }
}

impl GetRequest for Schema {
    fn normalize_params(params: &Params) -> Params {
        params.without_empty(&[
            "class",
            "predicate",
            "datatype",
            "children",
            "projection",
            "view",
            "limit",
            "labels",
        ])
    }

    fn labels_requested(&self) -> bool {
        self.labels
    }

    fn work_class(&self) -> WorkClass {
        let filtered_projection = match &self.query {
            SchemaQuery::ClassRelations(filter) => {
                filter.class.is_some() || filter.predicate.is_some()
            }
            SchemaQuery::ClassProperties(filter) => {
                filter.class.is_some() || filter.predicate.is_some()
            }
            SchemaQuery::Node(_) | SchemaQuery::Children(_) => false,
        };
        if filtered_projection {
            WorkClass::Heavy
        } else {
            WorkClass::Ordinary
        }
    }
}

impl GetRequest for Void {
    fn normalize_params(params: &Params) -> Params {
        params.clone()
    }

    fn work_class(&self) -> WorkClass {
        WorkClass::Heavy
    }
}

impl GetRequest for Summary {
    fn normalize_params(params: &Params) -> Params {
        params.clone()
    }
}

impl GetRequest for Sample {
    fn normalize_params(params: &Params) -> Params {
        normalize_pattern_params(params, &["n", "seed"])
    }

    fn work_class(&self) -> WorkClass {
        WorkClass::Heavy
    }
}

impl GetRequest for Search {
    fn normalize_params(params: &Params) -> Params {
        params.without_empty(&["role", "predicate", "limit"])
    }

    fn work_class(&self) -> WorkClass {
        WorkClass::Heavy
    }
}

// ---------------------------------------------------------------------------
// Shared parameter reading
// ---------------------------------------------------------------------------

/// Protocol parameters for these operations that this deployment does not
/// answer, and the capability each one needs.
///
/// Refused, never ignored, and `g` is why the rule is absolute rather than
/// pragmatic: a request scoped to one named graph and answered from the whole
/// dataset is a wrong answer that carries no sign of being wrong. These are
/// coded `capability_not_available` with **501**: the request is well
/// formed, and the shortfall is the server's.
/// Which operations define each one *for* is the third column, and it
/// is load-bearing rather than documentation: `g=` on a `/sample` is not a
/// graph-scoped sample this deployment cannot run; it is not a sample
/// parameter. Answering that 501 would send an agent to look for a bundle
/// declaring `graphs`, where the identical request would fail again.
///
/// The table gives `/fragment` and `/count` the same filters but no `labels` on
/// a count, since it has no rows to label. `labels=true` applies to operations
/// that return rows, while graph scope applies only to fragment and count.
const NOT_OFFERED: &[(&str, Option<Capability>, &[&str])] = &[
    ("o.lang", None, &[FRAGMENT, COUNT]),
    ("o.dt", None, &[FRAGMENT, COUNT]),
    ("o.ge", Some(Capability::Range), &[FRAGMENT, COUNT]),
    ("o.gt", Some(Capability::Range), &[FRAGMENT, COUNT]),
    ("o.le", Some(Capability::Range), &[FRAGMENT, COUNT]),
    ("o.lt", Some(Capability::Range), &[FRAGMENT, COUNT]),
    (
        "labels",
        Some(Capability::Labels),
        &[FRAGMENT, DESCRIBE, SAMPLE],
    ),
    ("g", Some(Capability::Graphs), &[FRAGMENT, COUNT]),
];

const FRAGMENT: &str = "fragment";
const COUNT: &str = "count";
const DESCRIBE: &str = "describe";
const SAMPLE: &str = "sample";
const SCHEMA: &str = "schema";
const VOID: &str = "void";
const SUMMARY: &str = "summary";
const SEARCH: &str = "search";
const LABELS: &str = "labels";

/// Refuse anything `operation` does not take.
fn accept_only(params: &Params, operation: &str, accepted: &[&str]) -> Result<(), Problem> {
    for name in params.names() {
        if accepted.contains(&name) {
            continue;
        }
        // The more specific answer first: a parameter defined *for this
        // operation* and this deployment cannot honour is not the same mistake
        // as a typo, and the two have different remedies.
        if let Some((_, capability, _)) = NOT_OFFERED
            .iter()
            .find(|(known, _, operations)| *known == name && operations.contains(&operation))
        {
            return Err(Problem::new(
                ErrorCode::CapabilityNotAvailable,
                match capability {
                    Some(capability) => format!(
                        "`{name}` needs the `{}` capability, which this deployment does not \
                         implement; the bundle's manifest lists the capabilities it declares",
                        capability.as_str()
                    ),
                    None => format!(
                        "`{name}` is a filter this deployment does not implement; \
                         narrow the pattern instead"
                    ),
                },
            ));
        }
        return Err(Problem::new(
            ErrorCode::MalformedRequest,
            format!(
                "`{}` is not a parameter of {operation}; it takes {}",
                reflected(name),
                accepted.join(", ")
            ),
        ));
    }
    Ok(())
}

/// Read a row count — `limit` or `n` — against its default and its cap.
fn page_size(
    params: &Params,
    name: &str,
    default: u32,
    cap: u32,
    instead: &str,
) -> Result<u32, Problem> {
    let Some(text) = params.get(name) else {
        return Ok(default.min(cap));
    };
    let value: u32 = text.parse().map_err(|_| {
        Problem::new(
            ErrorCode::MalformedRequest,
            format!("{name}={} is not a whole number of rows", reflected(text)),
        )
    })?;
    if value == 0 {
        // Refused rather than answered, because the answer is a paradox: no
        // rows, not complete, and a cursor that resumes exactly where it was
        // issued. A client that paged on it would never move.
        return Err(Problem::new(
            ErrorCode::MalformedRequest,
            format!("{name}=0 asks for nothing back; {instead}"),
        ));
    }
    if value > cap {
        return Err(Problem::new(
            ErrorCode::CapExceeded,
            format!(
                "{name}={value} is over this server's cap of {cap}; \
                 GET / publishes the caps, and `next` pages past them"
            ),
        ));
    }
    Ok(value)
}

fn boolean(params: &Params, name: &str, default: bool) -> Result<bool, Problem> {
    match params.get(name) {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(value) => Err(Problem::new(
            ErrorCode::MalformedRequest,
            format!(
                "{name}={} is not a boolean; use `{name}=true` or `{name}=false`",
                reflected(value)
            ),
        )),
    }
}

fn comma_list<'a>(name: &str, value: &'a str) -> Result<Vec<&'a str>, Problem> {
    let values: Vec<_> = value.split(',').map(str::trim).collect();
    if values.is_empty() || values.iter().any(|value| value.is_empty()) {
        return Err(Problem::new(
            ErrorCode::MalformedRequest,
            format!("`{name}` is an empty or malformed comma-separated list"),
        ));
    }
    Ok(values)
}

fn term_list<'a>(name: &str, value: &'a str) -> Result<Vec<&'a str>, Problem> {
    // Accept the same comma ergonomics as `role=`, while retaining whitespace
    // as the natural separator between RDF terms. A comma inside `<…>` belongs
    // to the IRI and is not a list delimiter.
    let mut values = Vec::new();
    let mut in_iri = false;
    let mut segment_start = 0;
    for (index, character) in value.char_indices() {
        match character {
            '<' => in_iri = true,
            '>' => in_iri = false,
            ',' if !in_iri => {
                let segment = value[segment_start..index].trim();
                if segment.is_empty() {
                    return Err(malformed_term_list(name));
                }
                values.extend(segment.split_ascii_whitespace());
                segment_start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    let final_segment = value[segment_start..].trim();
    if final_segment.is_empty() {
        return Err(malformed_term_list(name));
    }
    values.extend(final_segment.split_ascii_whitespace());
    if values.is_empty() {
        return Err(malformed_term_list(name));
    }
    Ok(values)
}

fn malformed_term_list(name: &str) -> Problem {
    Problem::new(
        ErrorCode::MalformedRequest,
        format!("`{name}` needs one or more IRIs separated by commas or whitespace"),
    )
}

/// Read `/sample`'s seed.
///
/// **Zero by default, not random.** A sample is deterministic for a given seed
/// and version, and a versioned GET is immutable, so varying per request would make both properties
/// false, and would put a validator on bytes that change. A client that wants a
/// different draw asks for one.
fn seed(params: &Params) -> Result<u64, Problem> {
    let Some(text) = params.get("seed") else {
        return Ok(0);
    };
    text.parse().map_err(|_| {
        Problem::new(
            ErrorCode::MalformedRequest,
            format!(
                "seed={} is not a whole number; a seed is any u64, and the same \
                 seed draws the same members from the same version",
                reflected(text)
            ),
        )
    })
}

/// Decode a `cursor` parameter against the request that must have issued it.
fn resume(params: &Params, binding: &CursorBinding) -> Result<Option<Cursor>, Problem> {
    match params.get("cursor") {
        None => Ok(None),
        Some(token) => Cursor::decode(token, binding)
            .map(Some)
            .map_err(Problem::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::PositionSpace;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    const CAPS: crate::Caps = crate::Caps::new();
    const BUDGETS: crate::Budgets = crate::Budgets::new();

    #[test]
    fn predicate_lists_accept_commas_and_whitespace_outside_bracketed_iris() {
        assert_eq!(
            term_list(
                "predicate",
                "ex:first, <http://example.org/with,comma> ex:third"
            )
            .unwrap(),
            ["ex:first", "<http://example.org/with,comma>", "ex:third"]
        );
        assert!(term_list("predicate", "ex:first,,ex:third").is_err());
        assert!(term_list("predicate", " ").is_err());
    }

    /// The published defaults, which is what a client reads at `/`.
    fn limits() -> Limits<'static> {
        Limits {
            caps: &CAPS,
            budgets: &BUDGETS,
        }
    }

    fn prefixes() -> PrefixMap {
        [("ex".to_owned(), "http://example.org/".to_owned())]
            .into_iter()
            .collect()
    }

    fn bundle() -> BundleBinding {
        BundleBinding::from_content_digest(DIGEST).expect("a well-formed digest")
    }

    fn params(query: &str) -> Params {
        Params::parse(Some(query)).expect("a well-formed query string")
    }

    fn fragment(query: &str) -> Result<Fragment, Problem> {
        Fragment::parse(&params(query), limits(), &prefixes(), &bundle())
    }

    fn schema(query: &str) -> Result<Schema, Problem> {
        let params = Schema::normalize_params(&params(query));
        Schema::parse(&params, limits(), &prefixes(), &bundle())
    }

    fn binding_fragment(body: &[u8]) -> Result<BindingFragment, Problem> {
        BindingFragment::parse(&params(""), body, limits(), &prefixes(), &bundle())
    }

    #[test]
    fn candidate_and_random_access_requests_are_admitted_as_heavy_work() {
        assert_eq!(fragment("").unwrap().work_class(), WorkClass::Ordinary);
        assert_eq!(
            GetFragment::parse(&params(""), limits(), &prefixes(), &bundle(), true)
                .unwrap()
                .work_class(),
            WorkClass::Heavy
        );
        assert_eq!(
            fragment("o.text=atrazine").unwrap().work_class(),
            WorkClass::Heavy
        );
        assert_eq!(
            Sample::parse(&params("n=1000"), limits(), &prefixes())
                .unwrap()
                .work_class(),
            WorkClass::Heavy
        );
        assert_eq!(
            schema("projection=class-relations").unwrap().work_class(),
            WorkClass::Ordinary
        );
        assert_eq!(
            schema("projection=class-relations&class=ex%3AClass")
                .unwrap()
                .work_class(),
            WorkClass::Heavy
        );
    }

    #[test]
    fn a_pattern_reads_both_spellings_and_echoes_the_one_it_was_sent() {
        let request = fragment("s=ex:alice&p=%3Chttp%3A%2F%2Fexample.org%2Fknows%3E").unwrap();

        let subject = request.pattern.bound(Position::Subject).unwrap();
        assert_eq!(subject.requested(), "ex:alice");
        assert_eq!(subject.dictionary(), "http://example.org/alice");
        let predicate = request.pattern.bound(Position::Predicate).unwrap();
        assert_eq!(predicate.requested(), "<http://example.org/knows>");
        assert_eq!(predicate.dictionary(), "http://example.org/knows");

        assert_eq!(request.pattern.bound(Position::Object), None);
        assert_eq!(request.pattern.vars(), vec![Position::Object]);
        assert_eq!(
            serde_json::to_value(&request.pattern).unwrap(),
            serde_json::json!({
                "s": "ex:alice",
                "p": "<http://example.org/knows>",
                "o": null,
            })
        );
    }

    #[test]
    fn two_spellings_of_one_term_are_one_canonical_request() {
        // A cursor binds to the terms, not to how they were typed, so a client
        // that switches between a CURIE and a bracketed IRI mid-paging keeps
        // its place. It also means the binding needs no dictionary, which is
        // what lets a cursor be rejected before the bundle opens.
        let curie = fragment("s=ex:alice&limit=10").unwrap();
        let iri = fragment("s=%3Chttp%3A%2F%2Fexample.org%2Falice%3E&limit=99").unwrap();

        let token = Cursor::at(&curie.binding, PositionSpace::Spo, 7).encode();
        assert!(Cursor::decode(token.as_str(), &iri.binding).is_ok());

        // And a different pattern is a different request.
        let other = fragment("s=ex:bob").unwrap();
        assert!(Cursor::decode(token.as_str(), &other.binding).is_err());
    }

    #[test]
    fn schema_parses_only_valid_typed_navigation_shapes() {
        let root = schema("").unwrap();
        assert_eq!(root.view, StatsView::Design);
        assert_eq!(root.limit, None);
        assert!(matches!(
            root.query,
            SchemaQuery::Node(SchemaSelection::Dataset)
        ));

        for (query, expected) in [
            ("children=classes", "classes"),
            ("children=properties", "dataset properties"),
            ("class=ex%3AClass&children=properties", "class properties"),
            ("predicate=ex%3Ap&children=object-classes", "object classes"),
            (
                "class=ex%3AClass&predicate=ex%3Ap&children=datatypes",
                "datatypes",
            ),
            (
                "predicate=ex%3Ap&datatype=ex%3AString&children=languages",
                "languages",
            ),
        ] {
            let parsed = schema(query).unwrap_or_else(|error| panic!("{query}: {error}"));
            assert_eq!(parsed.limit, Some(CAPS.default_limit));
            let SchemaQuery::Children(children) = parsed.query else {
                panic!("{query} did not parse as children");
            };
            match (expected, &children) {
                ("classes", SchemaChildren::Classes)
                | ("dataset properties", SchemaChildren::DatasetProperties)
                | ("class properties", SchemaChildren::ClassProperties { .. })
                | ("object classes", SchemaChildren::PropertyObjectClasses { .. })
                | ("datatypes", SchemaChildren::PropertyDatatypes { .. })
                | ("languages", SchemaChildren::DatatypeLanguages { .. }) => {}
                (_, actual) => panic!("{query} produced {actual:?}, expected {expected}"),
            }
            let _store_query = children.store_query();
        }

        let selected = schema("class=ex%3AClass&predicate=ex%3Ap").unwrap();
        let SchemaQuery::Node(SchemaSelection::Property { class, predicate }) = selected.query
        else {
            panic!("class + predicate must select a property node");
        };
        assert_eq!(class.unwrap().dictionary(), "http://example.org/Class");
        assert_eq!(predicate.dictionary(), "http://example.org/p");

        for limit in ["5", "2000"] {
            assert_eq!(
                schema(&format!("class=ex%3AClass&limit={limit}"))
                    .unwrap_err()
                    .code(),
                ErrorCode::MalformedRequest,
                "a node-only request must not silently ignore limit={limit}"
            );
        }
    }

    #[test]
    fn schema_rejects_invalid_selector_collection_combinations() {
        for query in [
            "datatype=ex%3AString",
            "children=languages",
            "class=ex%3AClass&children=classes",
            "predicate=ex%3Ap&children=properties",
            "predicate=ex%3Ap&datatype=ex%3AString&children=datatypes",
            "projection=class-relations&children=classes",
            "projection=class-relations&datatype=ex%3AString",
            "projection=recursive",
            "view=component%3A",
        ] {
            assert_eq!(
                schema(query).unwrap_err().code(),
                ErrorCode::MalformedRequest,
                "{query}"
            );
        }

        assert_eq!(
            schema("class=%22not-an-iri%22").unwrap_err().code(),
            ErrorCode::BadTermSyntax
        );
    }

    #[test]
    fn schema_node_selectors_cross_into_store_with_canonical_iris() {
        let selection = |query: &str| match schema(query).unwrap().query {
            SchemaQuery::Node(selection) => selection,
            other => panic!("{query} produced {other:?}, expected a node selector"),
        };

        let root = selection("");
        assert_eq!(root.store_selector(), StoreSchemaSelector::Dataset);

        let class = selection("class=ex%3AClass");
        assert_eq!(
            class.store_selector(),
            StoreSchemaSelector::Class {
                class: "http://example.org/Class"
            }
        );

        let property = selection("class=ex%3AClass&predicate=ex%3Ap");
        assert_eq!(
            property.store_selector(),
            StoreSchemaSelector::Property {
                class: Some("http://example.org/Class"),
                predicate: "http://example.org/p",
            }
        );

        let datatype = selection("predicate=ex%3Ap&datatype=ex%3AString");
        assert_eq!(
            datatype.store_selector(),
            StoreSchemaSelector::Datatype {
                class: None,
                predicate: "http://example.org/p",
                datatype: "http://example.org/String",
            }
        );

        let scoped_datatype = selection("class=ex%3AClass&predicate=ex%3Ap&datatype=ex%3AString");
        assert_eq!(
            scoped_datatype.store_selector(),
            StoreSchemaSelector::Datatype {
                class: Some("http://example.org/Class"),
                predicate: "http://example.org/p",
                datatype: "http://example.org/String",
            }
        );
    }

    #[test]
    fn schema_types_the_flat_projection_and_applies_its_own_cap() {
        let parsed = schema(
            "projection=class-relations&class=ex%3AClass&predicate=ex%3Ap&view=component%3Acanonical&limit=1000",
        )
        .unwrap();
        assert_eq!(parsed.limit, Some(CAPS.max_schema_items));
        assert_eq!(parsed.candidates, Candidates(BUDGETS.candidate_budget));
        assert_eq!(parsed.view, StatsView::component("canonical").unwrap());
        let SchemaQuery::ClassRelations(filter) = parsed.query else {
            panic!("projection did not type as class relations");
        };
        let filter = filter.store_filter();
        assert_eq!(filter.class, Some("http://example.org/Class"));
        assert_eq!(filter.predicate, Some("http://example.org/p"));

        assert_eq!(
            schema("projection=class-relations&limit=1001")
                .unwrap_err()
                .code(),
            ErrorCode::CapExceeded
        );

        let properties =
            schema("projection=class-properties&class=ex%3AClass&predicate=ex%3Ap&limit=10")
                .unwrap();
        let SchemaQuery::ClassProperties(filter) = properties.query else {
            panic!("projection did not type as class properties");
        };
        let filter = filter.store_filter();
        assert_eq!(filter.class, Some("http://example.org/Class"));
        assert_eq!(filter.predicate, Some("http://example.org/p"));
    }

    #[test]
    fn schema_cursors_bind_the_view_selector_and_collection_but_not_limit() {
        let issued = schema("class=ex%3AClass&children=properties&limit=7").unwrap();
        let token = Cursor::at_schema_child(&issued.binding, 9).encode();
        let resumed = schema(&format!(
            "class=%3Chttp%3A%2F%2Fexample.org%2FClass%3E&children=properties&view=design&limit=1&cursor={token}"
        ))
        .unwrap();
        assert_eq!(resumed.cursor.unwrap().position, 9);

        for query in [
            format!("class=ex%3AOther&children=properties&cursor={token}"),
            format!("class=ex%3AClass&cursor={token}"),
            format!("class=ex%3AClass&children=properties&view=queryable&cursor={token}"),
        ] {
            assert_eq!(
                schema(&query).unwrap_err().code(),
                ErrorCode::StaleCursor,
                "{query}"
            );
        }

        let projection = schema("projection=class-relations").unwrap();
        let projection_token = Cursor::at_class_relation(&projection.binding, 4_096).encode();
        assert_eq!(
            schema(&format!(
                "projection=class-relations&limit=2&cursor={projection_token}"
            ))
            .unwrap()
            .cursor
            .unwrap()
            .position,
            4_096
        );
        let wrong_space = Cursor::at_schema_child(&projection.binding, 4_096).encode();
        assert_eq!(
            schema(&format!("projection=class-relations&cursor={wrong_space}"))
                .unwrap_err()
                .code(),
            ErrorCode::StaleCursor
        );

        let properties = schema("projection=class-properties").unwrap();
        let property_token = Cursor::at_class_property(&properties.binding, 8_192).encode();
        assert_eq!(
            schema(&format!(
                "projection=class-properties&limit=2&cursor={property_token}"
            ))
            .unwrap()
            .cursor
            .unwrap()
            .position,
            8_192
        );
    }

    #[test]
    fn a_parameter_this_deployment_cannot_honour_is_refused_and_not_dropped() {
        // The sharpest case: `g=` scopes a request to one named graph, so
        // answering it from the whole dataset is wrong in a way the client
        // cannot see. It gets 501: the request is fine, the server
        // is not.
        for (query, expected) in [
            ("p=ex:a&g=%3Chttp%3A%2F%2Fexample.org%2Fg%3E", "graphs"),
            ("o.ge=%2242%22", "range"),
            ("labels=true", "labels"),
        ] {
            let refused = fragment(query).unwrap_err();
            assert_eq!(refused.code(), ErrorCode::CapabilityNotAvailable, "{query}");
            assert_eq!(refused.status(), 501);
            let detail = serde_json::to_value(&refused).unwrap();
            assert!(
                detail["detail"].as_str().unwrap().contains(expected),
                "{query} must name the capability it needs: {detail}"
            );
        }

        // A filter with no capability behind it is still refused, and says so
        // without inventing one.
        let refused = fragment("o.lang=en").unwrap_err();
        assert_eq!(refused.code(), ErrorCode::CapabilityNotAvailable);

        // A parameter that is nobody's is the other error, with the list.
        let typo = fragment("limt=10").unwrap_err();
        assert_eq!(typo.code(), ErrorCode::MalformedRequest);
        let detail = serde_json::to_value(&typo).unwrap();
        assert!(
            detail["detail"].as_str().unwrap().contains("limit"),
            "{detail}"
        );
    }

    #[test]
    fn a_parameter_is_classified_against_the_operation_it_was_sent_to() {
        // 501 says "another bundle could answer this", so it is only the right
        // answer where the parameter is defined for the operation. `g=` belongs
        // to fragment and count only, so a graph-scoped `/sample` is not a
        // capability this deployment lacks; it is not a sample parameter, and sending an agent to look
        // for a bundle declaring `graphs` would waste its next request.
        let scoped = "g=%3Chttp%3A%2F%2Fexample.org%2Fg%3E";
        assert_eq!(
            fragment(scoped).unwrap_err().code(),
            ErrorCode::CapabilityNotAvailable
        );
        assert_eq!(
            Count::parse(&params(scoped), limits(), &prefixes(), &bundle())
                .unwrap_err()
                .code(),
            ErrorCode::CapabilityNotAvailable
        );
        assert_eq!(
            Sample::parse(&params(scoped), limits(), &prefixes())
                .unwrap_err()
                .code(),
            ErrorCode::MalformedRequest,
            "sample defines no graph scoping, so `g` is simply not its parameter"
        );

        // `labels` runs the other way: it applies to responses
        // that carry rows, so a count does not take one.
        assert_eq!(
            fragment("labels=true").unwrap_err().code(),
            ErrorCode::CapabilityNotAvailable
        );
        assert_eq!(
            Sample::parse(&params("labels=true"), limits(), &prefixes())
                .unwrap_err()
                .code(),
            ErrorCode::CapabilityNotAvailable
        );
        assert_eq!(
            Count::parse(&params("labels=true"), limits(), &prefixes(), &bundle())
                .unwrap_err()
                .code(),
            ErrorCode::MalformedRequest,
            "a count has no rows to label"
        );

        // And an object filter belongs to the two operations that take a
        // pattern *and* report on objects. `o.text` is answered rather than
        // refused: `/count` supports text constraints, so it
        // accepts the filter even though other operations do not.
        assert!(Count::parse(&params("o.text=atrazine"), limits(), &prefixes(), &bundle()).is_ok());
        assert_eq!(
            Describe::parse(
                &params("iri=ex:a&o.text=atrazine"),
                limits(),
                &prefixes(),
                &bundle()
            )
            .unwrap_err()
            .code(),
            ErrorCode::MalformedRequest,
            "describe takes a resource, not a pattern"
        );
    }

    #[test]
    fn a_page_size_is_bounded_below_as_well_as_above() {
        assert_eq!(fragment("").unwrap().limit, 100);
        assert_eq!(fragment("limit=1").unwrap().limit, 1);
        assert_eq!(fragment("limit=10000").unwrap().limit, 10_000);
        // And the one composite budget a page has to carry with it, since
        // nothing a cap can express bounds it.
        assert_eq!(
            fragment("").unwrap().bytes,
            ResponseBytes(BUDGETS.max_response_bytes)
        );

        let over = fragment("limit=10001").unwrap_err();
        assert_eq!(over.code(), ErrorCode::CapExceeded);
        assert_eq!(over.status(), 400);

        // Zero has no coherent answer: no rows, not complete, and a cursor
        // resuming where it was issued.
        let zero = fragment("limit=0").unwrap_err();
        assert_eq!(zero.code(), ErrorCode::MalformedRequest);
        assert!(
            serde_json::to_value(&zero).unwrap()["detail"]
                .as_str()
                .unwrap()
                .contains("/count")
        );

        for query in ["limit=-1", "limit=x", "limit=1.5", "limit="] {
            assert_eq!(
                fragment(query).unwrap_err().code(),
                ErrorCode::MalformedRequest,
                "{query}"
            );
        }
    }

    #[test]
    fn a_binding_body_is_strict_typed_and_capped() {
        let body = serde_json::json!({
            "pattern": {"s": "?person", "p": "ex:knows", "o": "?known"},
            "bindings": {
                "vars": ["?person"],
                "rows": [[{"type": "iri", "value": "http://example.org/alice"}]]
            },
            "limit": 7
        });
        let parsed = binding_fragment(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(parsed.limit, 7);
        let row = parsed.rows().next().unwrap();
        assert_eq!(row.index(), 0);
        assert_eq!(
            row.bound(Position::Subject).unwrap().dictionary(),
            "http://example.org/alice"
        );
        assert_eq!(row.bound(Position::Object), None);

        let unknown = br#"{
            "pattern":{"s":"?s","p":"ex:p","o":"?o"},
            "bindings":{"vars":[],"rows":[]},
            "surprise":true
        }"#;
        assert_eq!(
            binding_fragment(unknown).unwrap_err().code(),
            ErrorCode::MalformedRequest
        );

        let duplicate = br#"{
            "pattern":{"s":"?s","p":"ex:p","o":"?o"},
            "bindings":{"vars":[],"vars":[],"rows":[]}
        }"#;
        assert_eq!(
            binding_fragment(duplicate).unwrap_err().code(),
            ErrorCode::MalformedRequest
        );

        let rows = vec![Vec::<String>::new(); CAPS.max_bindings as usize + 1];
        let over = serde_json::json!({
            "pattern": {"s": "?s", "p": "ex:p", "o": "?o"},
            "bindings": {"vars": [], "rows": rows}
        });
        assert_eq!(
            binding_fragment(&serde_json::to_vec(&over).unwrap())
                .unwrap_err()
                .code(),
            ErrorCode::CapExceeded
        );
    }

    #[test]
    fn malformed_binding_term_objects_keep_their_cell_and_remedy() {
        for (cell, remedy) in [
            (serde_json::json!({"type": "iri"}), "no `value`"),
            (
                serde_json::json!({
                    "type": "literal",
                    "value": "Alice",
                    "xml:lang": "en"
                }),
                "spells it `lang`",
            ),
        ] {
            let body = serde_json::json!({
                "pattern": {"s": "?s", "p": "ex:p", "o": "?o"},
                "bindings": {"vars": ["?o"], "rows": [[cell]]}
            });
            let error = binding_fragment(&serde_json::to_vec(&body).unwrap()).unwrap_err();
            assert_eq!(error.code(), ErrorCode::BadTermSyntax);
            let detail = serde_json::to_value(&error).unwrap()["detail"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(detail.contains("bindings.rows[0][0]"), "{detail}");
            assert!(detail.contains(remedy), "{detail}");
            assert!(!detail.contains("untagged enum"), "{detail}");
        }

        let duplicate = br#"{
            "pattern":{"s":"?s","p":"ex:p","o":"?o"},
            "bindings":{
                "vars":["?o"],
                "rows":[[{"type":"iri","type":"literal","value":"Alice"}]]
            }
        }"#;
        let error = binding_fragment(duplicate).unwrap_err();
        assert_eq!(error.code(), ErrorCode::BadTermSyntax);
        let detail = serde_json::to_value(&error).unwrap()["detail"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(detail.contains("bindings.rows[0][0]"), "{detail}");
        assert!(detail.contains("duplicate key `type`"), "{detail}");
    }

    #[test]
    fn generalized_bindings_keep_foreign_columns_and_undef_but_bound_repeated_variables() {
        let foreign = serde_json::json!({
            "pattern": {"s": "?s", "p": "ex:p", "o": "?o"},
            "bindings": {"vars": ["?missing", "?s"], "rows": [["ex:a", null]]}
        });
        let parsed = binding_fragment(&serde_json::to_vec(&foreign).unwrap()).unwrap();
        let row = parsed.rows().next().unwrap();
        assert_eq!(row.bound(Position::Subject), None);
        assert_eq!(
            row.bound(Position::Predicate).unwrap().dictionary(),
            "http://example.org/p"
        );

        for body in [
            serde_json::json!({
                "pattern": {"s": "?same", "p": "ex:p", "o": "?same"},
                "bindings": {"vars": [], "rows": [[]]}
            }),
            serde_json::json!({
                "pattern": {"s": "?same", "p": "ex:p", "o": "?same"},
                "bindings": {"vars": ["?same"], "rows": [[null]]}
            }),
        ] {
            assert_eq!(
                binding_fragment(&serde_json::to_vec(&body).unwrap())
                    .unwrap_err()
                    .code(),
                ErrorCode::MalformedRequest
            );
        }

        let bounded = serde_json::json!({
            "pattern": {"s": "?same", "p": "ex:p", "o": "?same"},
            "bindings": {"vars": ["?same"], "rows": [["ex:a"]]}
        });
        assert!(binding_fragment(&serde_json::to_vec(&bounded).unwrap()).is_ok());

        let empty = serde_json::json!({
            "pattern": {"s": "?same", "p": "ex:p", "o": "?same"},
            "bindings": {"vars": [], "rows": []}
        });
        assert!(binding_fragment(&serde_json::to_vec(&empty).unwrap()).is_ok());

        let empty_values = "(?foreign) {}";
        let query = format!(
            "s=%3Fsame&p=ex%3Ap&o=%3Fsame&values={}",
            crate::url::encode_value(empty_values)
        );
        assert!(
            GetFragment::parse(&params(&query), limits(), &prefixes(), &bundle(), true).is_ok()
        );
    }

    #[test]
    fn brtpf_values_are_byte_bounded_before_sparql_parsing() {
        let budgets = crate::Budgets {
            max_request_bytes: 8,
            ..BUDGETS
        };
        let limits = Limits {
            caps: &CAPS,
            budgets: &budgets,
        };
        let values = "(?s) { (<http://example.org/alice>) }";
        let query = format!(
            "s=%3Fs&p=%3Fp&o=%3Fo&values={}",
            crate::url::encode_value(values)
        );
        let error =
            GetFragment::parse(&params(&query), limits, &prefixes(), &bundle(), true).unwrap_err();
        assert_eq!(error.code(), ErrorCode::PayloadTooLarge);
        assert_eq!(error.status(), 413);
    }

    #[test]
    fn binding_variables_follow_the_sparql_varname_grammar() {
        for invalid in ["?", "??person", "?has-hyphen", "?$person"] {
            assert_eq!(
                Variable::parse(invalid, "pattern.s").unwrap_err().code(),
                ErrorCode::MalformedRequest,
                "{invalid}"
            );
        }
        for valid in ["?person", "?person_1", "?1person", "?éclair"] {
            assert_eq!(Variable::parse(valid, "pattern.s").unwrap().as_str(), valid);
        }

        let query = "s=%3F%3Fperson&p=ex%3Aknows&o=%3Fknown&values=%28%3Fperson%29%20%7B%20%28ex%3Aalice%29%20%7D";
        assert_eq!(
            GetFragment::parse(&params(query), limits(), &prefixes(), &bundle(), true)
                .unwrap_err()
                .code(),
            ErrorCode::MalformedRequest
        );
    }

    #[test]
    fn omitted_brtpf_positions_are_anonymous_not_synthetic_variables() {
        let values = "(?p ?known) { (<http://example.org/alice> <http://example.org/bob>) }";
        let query = format!(
            "s=%3Fp&o=%3Fknown&values={}",
            crate::url::encode_value(values)
        );
        let parsed =
            GetFragment::parse(&params(&query), limits(), &prefixes(), &bundle(), true).unwrap();
        let GetFragment::Values(parsed) = parsed else {
            panic!("values= must select the bindings grammar")
        };
        let row = parsed.rows().next().unwrap();
        assert_eq!(
            row.bound(Position::Subject).unwrap().dictionary(),
            "http://example.org/alice"
        );
        assert_eq!(row.bound(Position::Predicate), None);
        assert_eq!(
            row.bound(Position::Object).unwrap().dictionary(),
            "http://example.org/bob"
        );
    }

    #[test]
    fn brtpf_values_are_parsed_by_sparql_and_keep_variable_names() {
        let values = "(?person ?foreign) { (<http://example.org/alice> UNDEF) (UNDEF \"x\"@en) }";
        let query = format!(
            "s=%3Fperson&p={}&o=%3Fknown&values={}",
            crate::url::encode_value("http://example.org/knows"),
            crate::url::encode_value(values)
        );
        let parsed =
            GetFragment::parse(&params(&query), limits(), &prefixes(), &bundle(), false).unwrap();
        let GetFragment::Values(parsed) = parsed else {
            panic!("values= must select the bindings grammar")
        };
        let mut rows = parsed.rows();
        let first = rows.next().unwrap();
        assert_eq!(
            first.bound(Position::Subject).unwrap().dictionary(),
            "http://example.org/alice"
        );
        assert_eq!(first.bound(Position::Object), None);
        let second = rows.next().unwrap();
        assert_eq!(second.bound(Position::Subject), None);
        assert_eq!(second.bound(Position::Object), None);
        assert!(rows.next().is_none());
    }

    #[test]
    fn brtpf_values_preserve_special_characters_in_literal_lexical_forms() {
        let values = r#"(?value) { ("a\"b\nc\\d\t\r"@EN) }"#;
        let query = format!(
            "s=%3Fs&p=%3Fp&o=%3Fvalue&values={}",
            crate::url::encode_value(values)
        );
        let parsed =
            GetFragment::parse(&params(&query), limits(), &prefixes(), &bundle(), false).unwrap();
        let GetFragment::Values(parsed) = parsed else {
            panic!("values= must select the bindings grammar")
        };
        let literal = parsed
            .rows()
            .next()
            .unwrap()
            .bound(Position::Object)
            .unwrap();
        assert_eq!(literal.dictionary(), "\"a\"b\nc\\d\t\r\"@en");
        assert_eq!(literal.requested(), "\"a\"b\nc\\d\t\r\"@en");
    }

    #[test]
    fn an_empty_optional_pattern_position_is_a_variable() {
        let empty = fragment("s=&p=ex:a&o=").unwrap();
        assert_eq!(empty.pattern.bound(Position::Subject), None);
        assert_eq!(
            empty
                .pattern
                .bound(Position::Predicate)
                .map(BoundTerm::dictionary),
            Some("http://example.org/a")
        );
        assert_eq!(empty.pattern.bound(Position::Object), None);
    }

    #[test]
    fn each_get_request_owns_its_blank_optional_controls() {
        let fragment = Fragment::normalize_params(&params(
            "s=&p=ex:a&o=&o.text=&limit=&cursor=&format=&unknown=",
        ));
        for omitted in ["s", "o", "o.text", "limit"] {
            assert_eq!(fragment.get(omitted), None, "{omitted}");
        }
        assert_eq!(fragment.get("p"), Some("ex:a"));
        for strict in ["cursor", "format", "unknown"] {
            assert_eq!(fragment.get(strict), Some(""), "{strict}");
        }

        let count = Count::normalize_params(&params("s=&p=&o=&o.text=&cursor="));
        for omitted in ["s", "p", "o", "o.text"] {
            assert_eq!(count.get(omitted), None, "{omitted}");
        }
        assert_eq!(count.get("cursor"), Some(""));

        let describe = Describe::normalize_params(&params("iri=&direction=&limit=&cursor="));
        assert_eq!(describe.get("limit"), None);
        for strict in ["iri", "direction", "cursor"] {
            assert_eq!(describe.get(strict), Some(""), "{strict}");
        }

        let sample = Sample::normalize_params(&params("s=&p=&o=&n=&seed="));
        for omitted in ["s", "p", "o", "n", "seed"] {
            assert_eq!(sample.get(omitted), None, "{omitted}");
        }

        let search = Search::normalize_params(&params("q=&role=&predicate=&labels=&limit="));
        for omitted in ["role", "predicate", "limit"] {
            assert_eq!(search.get(omitted), None, "{omitted}");
        }
        for strict in ["q", "labels"] {
            assert_eq!(search.get(strict), Some(""), "{strict}");
        }

        let schema = Schema::normalize_params(&params(
            "class=&predicate=&datatype=&children=&projection=&view=&limit=&cursor=&format=",
        ));
        for omitted in [
            "class",
            "predicate",
            "datatype",
            "children",
            "projection",
            "view",
            "limit",
        ] {
            assert_eq!(schema.get(omitted), None, "{omitted}");
        }
        for strict in ["cursor", "format"] {
            assert_eq!(schema.get(strict), Some(""), "{strict}");
        }
    }

    #[test]
    fn a_term_over_the_published_budget_is_refused() {
        let huge = format!("o=%22{}%22", "x".repeat(70_000));
        let refused =
            Fragment::parse(&params(&huge), limits(), &prefixes(), &bundle()).unwrap_err();
        assert_eq!(refused.code(), ErrorCode::CapExceeded);
        assert!(
            serde_json::to_value(&refused).unwrap()["detail"]
                .as_str()
                .unwrap()
                .contains("max_term_bytes")
        );
    }

    #[test]
    fn describe_needs_a_resource_and_takes_three_directions() {
        let parse = |query: &str| Describe::parse(&params(query), limits(), &prefixes(), &bundle());

        assert_eq!(parse("iri=ex:a").unwrap().direction, Direction::Both);
        assert_eq!(
            parse("iri=ex:a&direction=in").unwrap().direction,
            Direction::In
        );
        assert_eq!(
            parse("iri=ex:a&direction=out")
                .unwrap()
                .resource
                .dictionary(),
            "http://example.org/a"
        );
        // A blank node and a literal are resources too.
        assert!(parse("iri=_:b1").is_ok());
        assert!(parse("iri=%22Alice%22%40en").is_ok());

        assert_eq!(
            parse("").unwrap_err().code(),
            ErrorCode::MalformedRequest,
            "a describe with no resource has nothing to describe"
        );
        assert_eq!(
            parse("iri=ex:a&direction=sideways").unwrap_err().code(),
            ErrorCode::MalformedRequest
        );
        // The pattern parameters belong to `/fragment`, not here.
        assert_eq!(
            parse("iri=ex:a&p=ex:knows").unwrap_err().code(),
            ErrorCode::MalformedRequest
        );
    }

    #[test]
    fn a_sample_is_deterministic_by_default() {
        let parse = |query: &str| Sample::parse(&params(query), limits(), &prefixes());

        let bare = parse("").unwrap();
        assert_eq!(bare.seed, 0, "an omitted seed is fixed, not random");
        assert_eq!(bare.n, 100);
        assert_eq!(parse("n=25&seed=42").unwrap().seed, 42);
        assert_eq!(parse("n=1000").unwrap().n, 1_000);

        assert_eq!(
            parse("n=1001").unwrap_err().code(),
            ErrorCode::CapExceeded,
            "a sample is capped at 1000 members"
        );
        assert_eq!(
            parse("seed=x").unwrap_err().code(),
            ErrorCode::MalformedRequest
        );
        // A sample has no position to resume from, so it takes no cursor.
        assert_eq!(
            parse("cursor=abc").unwrap_err().code(),
            ErrorCode::MalformedRequest
        );
    }

    #[test]
    fn a_count_takes_a_pattern_and_only_a_scan_cursor() {
        let parse = |query: &str| Count::parse(&params(query), limits(), &prefixes(), &bundle());

        assert_eq!(
            parse("p=ex:knows")
                .unwrap()
                .pattern
                .bound(Position::Predicate),
            Some(&BoundTerm {
                requested: "p=ex:knows".split_once('=').unwrap().1.to_owned(),
                dictionary: "http://example.org/knows".to_owned(),
                kind: BoundKind::Iri,
            })
        );
        assert_eq!(
            parse("limit=10").unwrap_err().code(),
            ErrorCode::MalformedRequest
        );
        assert_eq!(
            parse("cursor=abc").unwrap_err().code(),
            ErrorCode::StaleCursor
        );

        let plain = parse("p=ex:knows").unwrap();
        let token = Cursor::at_text_scan(&plain.binding, 1, 0).encode();
        assert_eq!(
            parse(&format!("p=ex:knows&cursor={token}"))
                .unwrap_err()
                .code(),
            ErrorCode::StaleCursor,
            "an ordinary count must not silently ignore a well-shaped cursor"
        );
    }

    #[test]
    fn a_cursor_for_another_request_is_stale_before_anything_opens() {
        let issued = fragment("p=ex:knows").unwrap();
        let token = Cursor::at(&issued.binding, PositionSpace::Pos, 3).encode();

        let resumed = fragment(&format!("p=ex:knows&cursor={token}")).unwrap();
        assert_eq!(resumed.cursor.unwrap().position, 3);

        // Changing the page size does not invalidate a position, and changing
        // the pattern does.
        assert!(fragment(&format!("p=ex:knows&limit=5&cursor={token}")).is_ok());
        let stale = fragment(&format!("p=ex:a&cursor={token}")).unwrap_err();
        assert_eq!(stale.code(), ErrorCode::StaleCursor);

        // And a token that is not one of ours at all.
        assert_eq!(
            fragment("cursor=not-a-token").unwrap_err().code(),
            ErrorCode::StaleCursor
        );
    }
}
