//! What the client asked for: doc 03 §3.4's parameters, parsed into types.
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
//! - one doc 03 defines but this deployment cannot answer — refused
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

use serde::Deserialize;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, Serializer};

use hdtc::format::TextQuery;
use kgf_store::{Capability, Role};

use crate::Limits;
use crate::cursor::{
    BundleBinding, CanonicalRequest, Cursor, CursorBinding, Operation, StaleCursor,
};
use crate::envelope::{ErrorCode, Problem, reflected};
use crate::service::PredicateRoles;
use crate::term::{PrefixMap, Term};
use crate::url::Params;

// ---------------------------------------------------------------------------
// Positions and terms
// ---------------------------------------------------------------------------

/// A triple position, which is also the parameter that binds it and the key a
/// row reports it under (doc 03 §3.4.1).
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
}

impl BoundTerm {
    /// Parse §3.3 request syntax from the parameter named `parameter`.
    fn parse(
        parameter: &str,
        text: &str,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
    ) -> Result<Self, Problem> {
        // §3.5's `max_term_bytes`, applied where a term enters. Published, so
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
        Ok(Self {
            requested: text.to_owned(),
            dictionary: term.to_dictionary().into_owned(),
        })
    }

    /// Parse either §3.3's compact string or its JSON term-object form.
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
        })
    }

    /// The term as the request wrote it.
    pub fn requested(&self) -> &str {
        &self.requested
    }

    /// The term as the dictionary spells it.
    pub fn dictionary(&self) -> &str {
        &self.dictionary
    }

    fn require_iri(self, parameter: &str) -> Result<Self, Problem> {
        if matches!(Term::from_dictionary(&self.dictionary), Term::Iri(_)) {
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
        }
    }
}

/// A triple pattern, as far as it can be read without a bundle (§3.4.1).
///
/// An omitted parameter is a variable. An *empty* one is not: §3.3 makes
/// omission the way to leave a position unbound, and a value that arrived empty
/// is far more likely to be a client that interpolated a variable it never set
/// than a client asking for everything. See `notes/plan.md`, "Questions for
/// `../kgf`" — §3.4.4's own example sends `s=&o=`, which this refuses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pattern {
    subject: Option<BoundTerm>,
    predicate: Option<BoundTerm>,
    object: Option<BoundTerm>,
    text: Option<TextFilter>,
}

impl Pattern {
    fn parse(params: &Params, limits: Limits<'_>, prefixes: &PrefixMap) -> Result<Self, Problem> {
        let mut pattern = Self::default();
        for position in Position::ALL {
            if let Some(text) = params.get(position.as_str()) {
                *pattern.slot(position) =
                    Some(BoundTerm::parse(position.as_str(), text, limits, prefixes)?);
            }
        }
        // Part of the pattern rather than beside it, which is how §3.4.1 echoes
        // it — the constraint sits in the object position it constrains. An
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

    /// The positions a row carries: the unbound ones (§3.4.1).
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
    /// §3.4.1's echo: the three positions, `null` where unbound, and each bound
    /// one spelled the way the client sent it.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(Position::ALL.len()))?;
        for position in Position::ALL {
            // §3.4.1: a constrained object echoes as `{"text": "…"}` in the
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

/// A text constraint on the object position (§3.4.1, doc 19 §19.3).
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
        // sends, and §3.5 caps what one of those may weigh.
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
    /// fuzziness, prefix, language ranges — that doc 03 §3.4.5 and doc 19 §19.3
    /// will expose later. Wiring one of those into the enumeration and
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
    /// §3.4.1 echoes the constraint inside the pattern's object position, as
    /// `{"text": "atrazine"}`.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("text", &self.0)?;
        map.end()
    }
}

/// Which side of a resource's neighborhood `/describe` walks (§3.4.6).
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
        // `both` by default: §3.4.6 calls the operation a resource
        // *neighborhood*, and half a neighborhood is a surprising default for a
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

/// The one composite budget a response has to carry with it (§3.5).
///
/// The caps bound what a client may *ask for*; the budgets bound what a
/// response may *cost*, and §3.5 is explicit that the two are not the same
/// thing — "a row cap is not a byte cap (one legal literal can be megabytes)".
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

/// §3.5's `candidate_budget`: rows or postings a filtered operation may
/// *examine*, independently of how many it returns.
///
/// A separate budget from [`ResponseBytes`] because it bounds a different
/// thing. A text-filtered pattern examines one ranked literal per candidate and
/// may keep none of them — `? p ?` discards every match that does not occur
/// with `p` — so the work has no relation to the page size, and `limit` bounds
/// nothing. Exhausting it is not an error: §3.5 says the response comes back
/// short and marked `candidate_budget`, with a cursor when its scan order has a
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
        if name.is_empty() || name.chars().any(char::is_whitespace) {
            return Err(Problem::new(
                ErrorCode::MalformedRequest,
                format!(
                    "{where_}={} is not a variable name; put a non-empty name after `?` and \
                     do not include whitespace",
                    reflected(text)
                ),
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
    Variable(Variable),
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

    fn cell(&self, position: Position) -> &BindingCell {
        &self.cells[position_index(position)]
    }

    /// The spelling the body used at `position`, for response echoes.
    pub fn requested(&self, position: Position) -> &str {
        match self.cell(position) {
            BindingCell::Variable(variable) => variable.as_str(),
            BindingCell::Term(term) => term.requested(),
        }
    }

    /// Positions reported in each result row.
    pub fn vars(&self) -> Vec<Position> {
        Position::ALL
            .into_iter()
            .filter(|position| matches!(self.cell(*position), BindingCell::Variable(_)))
            .collect()
    }

    fn variables(&self) -> impl Iterator<Item = &Variable> {
        self.cells.iter().filter_map(|cell| match cell {
            BindingCell::Variable(variable) => Some(variable),
            BindingCell::Term(_) => None,
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
    values: &'a [BoundTerm],
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
                .map(|column| &self.values[*column]),
        }
    }
}

/// A parsed binding table, with column names already resolved to positions.
#[derive(Debug)]
struct Bindings {
    columns: BTreeMap<Variable, usize>,
    variables: Vec<Variable>,
    rows: Vec<Vec<BoundTerm>>,
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

        let pattern_variables: BTreeSet<_> = pattern.variables().map(Variable::as_str).collect();
        let mut columns = BTreeMap::new();
        let mut variables = Vec::with_capacity(wire.vars.len());
        for (column, text) in wire.vars.iter().enumerate() {
            let variable = Variable::parse(text, &format!("bindings.vars[{column}]"))?;
            if !pattern_variables.contains(variable.as_str()) {
                return Err(Problem::new(
                    ErrorCode::MalformedRequest,
                    format!(
                        "bindings.vars[{column}] names {}, which is not a variable in the pattern",
                        reflected(variable.as_str())
                    ),
                ));
            }
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

        // `?x p ?x` is bounded when every input row fixes `?x`: it becomes a
        // ground or singly-variable lookup. Left unbound it is an equality
        // filter over a non-contiguous enumeration, whose rejected candidates
        // are not bounded by the result limit.
        for variable in pattern.repeated_variables() {
            if !columns.contains_key(variable) {
                return Err(Problem::new(
                    ErrorCode::MalformedRequest,
                    format!(
                        "repeated pattern variable {} must be present in bindings.vars; leaving \
                         it unbound would require an unbudgeted equality scan",
                        reflected(variable.as_str())
                    ),
                ));
            }
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
                row.push(BoundTerm::parse_body(
                    &format!("bindings.rows[{row_index}][{column}]"),
                    value,
                    limits,
                    prefixes,
                )?);
            }
            rows.push(row);
        }

        Ok(Self {
            columns,
            variables,
            rows,
        })
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

    fn canonicalize(&self, mut output: String) -> String {
        push_canonical(&mut output, &self.variables.len().to_string());
        for variable in &self.variables {
            push_canonical(&mut output, variable.as_str());
        }
        push_canonical(&mut output, &self.rows.len().to_string());
        for row in &self.rows {
            for term in row {
                push_canonical(&mut output, term.dictionary());
            }
        }
        output
    }
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
    /// Where to resume, if the body carried a cursor.
    pub cursor: Option<Cursor>,
    /// What a cursor this request issues must match.
    pub binding: CursorBinding,
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
            cursor,
            binding,
        })
    }

    /// Input rows in their contractual enumeration order.
    pub fn rows(&self) -> impl Iterator<Item = BindingRow<'_>> {
        self.bindings.rows(&self.pattern)
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

/// `GET /fragment` — a triple pattern, paged (§3.4.1).
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
        accept_only(params, FRAGMENT, Self::PARAMETERS)?;
        let pattern = Pattern::parse(params, limits, prefixes)?;
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
        })
    }
}

/// `GET /count` — a cardinality and nothing else (§3.4.4).
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

/// `GET /describe` — a resource's neighborhood, paged (§3.4.6).
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
        // Named `iri` by §3.4.6, and any term is accepted: a blank node has a
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

/// `GET /sample` — pseudo-random members of a pattern's result set (§3.4.7).
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

// ---------------------------------------------------------------------------
// Shared parameter reading
// ---------------------------------------------------------------------------

/// Parameters doc 03 defines for these operations that this deployment does not
/// answer, and the capability each one needs.
///
/// Refused, never ignored, and `g` is why the rule is absolute rather than
/// pragmatic: a request scoped to one named graph and answered from the whole
/// dataset is a wrong answer that carries no sign of being wrong. §3.6.1 codes
/// these `capability_not_available` and gives it **501** — the request is well
/// formed, and the shortfall is the server's.
/// Which operations doc 03 defines each one *for* is the third column, and it
/// is load-bearing rather than documentation: `g=` on a `/sample` is not a
/// graph-scoped sample this deployment cannot run, it is a parameter §3.4.7
/// does not have. Answering that 501 would send an agent to look for a bundle
/// declaring `graphs`, where the identical request would fail again — and §3.6
/// makes the remedy the whole point of a code.
///
/// The column transcribes: §3.4.1's parameter table for `/fragment`; §3.4.4's
/// filtered counts, which give `/count` the same filters and no `labels` (a
/// count has no rows to label); §3.5's
/// `labels=true` modifier row for the operations that return rows; and §3.5's
/// `fragment +g scope` and `count +g scope` rows for `g`.
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
const SEARCH: &str = "search";
const LABELS: &str = "labels";

/// Refuse anything `operation` does not take.
fn accept_only(params: &Params, operation: &str, accepted: &[&str]) -> Result<(), Problem> {
    for name in params.names() {
        if accepted.contains(&name) {
            continue;
        }
        // The more specific answer first: a parameter doc 03 defines *for this
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
    let values: Vec<_> = value.split_ascii_whitespace().collect();
    if values.is_empty() {
        return Err(Problem::new(
            ErrorCode::MalformedRequest,
            format!("`{name}` needs at least one IRI"),
        ));
    }
    Ok(values)
}

/// Read `/sample`'s seed.
///
/// **Zero by default, not random.** §3.4.7 makes a sample "deterministic for a
/// given seed + version, hence cacheable", and a versioned GET is immutable
/// (§3.6) — so a response that varied per request would make both statements
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

    fn binding_fragment(body: &[u8]) -> Result<BindingFragment, Problem> {
        BindingFragment::parse(&params(""), body, limits(), &prefixes(), &bundle())
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
    fn a_parameter_this_deployment_cannot_honour_is_refused_and_not_dropped() {
        // The sharpest case: `g=` scopes a request to one named graph, so
        // answering it from the whole dataset is wrong in a way the client
        // cannot see. §3.6.1 gives it 501 — the request is fine, the server
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
        // answer where doc 03 defines the parameter for the operation. `g=` is
        // §3.5's `fragment +g scope` and `count +g scope` and nothing else, so
        // a graph-scoped `/sample` is not a capability this deployment lacks —
        // it is a parameter §3.4.7 does not have, and sending an agent to look
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
            "§3.4.7 defines no graph scoping, so `g` is simply not its parameter"
        );

        // `labels` runs the other way: §3.5's modifier row is about responses
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
        // refused — §3.4.4 defines counts for text constraints, so `/count`
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
            "§3.4.6 takes a resource, not a pattern"
        );
    }

    #[test]
    fn a_page_size_is_bounded_below_as_well_as_above() {
        assert_eq!(fragment("").unwrap().limit, 100);
        assert_eq!(fragment("limit=1").unwrap().limit, 1);
        assert_eq!(fragment("limit=10000").unwrap().limit, 10_000);
        // And the one composite budget a page has to carry with it, since
        // nothing a cap can express bounds it (§3.5).
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
    fn binding_variables_must_describe_a_bounded_pattern() {
        for body in [
            serde_json::json!({
                "pattern": {"s": "?s", "p": "ex:p", "o": "?o"},
                "bindings": {"vars": ["?missing"], "rows": [["ex:a"]]}
            }),
            serde_json::json!({
                "pattern": {"s": "?same", "p": "ex:p", "o": "?same"},
                "bindings": {"vars": [], "rows": [[]]}
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
    }

    #[test]
    fn an_empty_term_is_not_a_variable() {
        // §3.3 makes omission the way to leave a position unbound, and §3.4.4's
        // own example contradicts it with `?s=&p=ex:affects&o=`. The example is
        // the bug: an empty value is far more likely to be an unset variable in
        // a client's URL template than a deliberate wildcard, and reading it as
        // a wildcard answers with the whole dataset.
        let empty = fragment("s=&p=ex:a").unwrap_err();
        assert_eq!(empty.code(), ErrorCode::BadTermSyntax);
        let detail = serde_json::to_value(&empty).unwrap();
        assert!(
            detail["detail"].as_str().unwrap().contains("omit"),
            "the message must name the fix: {detail}"
        );
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
            "§3.5 caps a sample at 1000 members"
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
