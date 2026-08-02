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

use serde::ser::{Serialize, SerializeMap, Serializer};

use kgf_store::{Capability, Role};

use crate::Limits;
use crate::cursor::{BundleBinding, CanonicalRequest, Cursor, CursorBinding, Operation};
use crate::envelope::{ErrorCode, Problem, reflected};
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

    /// The term as the request wrote it.
    pub fn requested(&self) -> &str {
        &self.requested
    }

    /// The term as the dictionary spells it.
    pub fn dictionary(&self) -> &str {
        &self.dictionary
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
        Ok(pattern)
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
        request
    }
}

impl Serialize for Pattern {
    /// §3.4.1's echo: the three positions, `null` where unbound, and each bound
    /// one spelled the way the client sent it.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(Position::ALL.len()))?;
        for position in Position::ALL {
            map.serialize_entry(
                position.as_str(),
                &self.bound(position).map(BoundTerm::requested),
            )?;
        }
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

/// `GET /fragment` — a triple pattern, paged (§3.4.1).
#[derive(Debug)]
pub struct Fragment {
    /// The pattern to enumerate.
    pub pattern: Pattern,
    /// Rows this page may carry.
    pub limit: u32,
    /// Bytes its rows may occupy.
    pub bytes: ResponseBytes,
    /// Where to resume, if the request carried a cursor.
    pub cursor: Option<Cursor>,
    /// What a cursor this request issues must match.
    pub binding: CursorBinding,
}

impl Fragment {
    const PARAMETERS: &'static [&'static str] = &["s", "p", "o", "limit", "cursor", "format"];

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
            cursor: resume(params, &binding)?,
            binding,
        })
    }
}

/// `GET /count` — a cardinality and nothing else (§3.4.4).
#[derive(Debug)]
pub struct Count {
    /// The pattern to count.
    pub pattern: Pattern,
}

impl Count {
    /// No `limit` and no `cursor`: an M1 count is exact and arrives whole.
    /// §3.4.4's budgeted scanning counts, which do resume, are M2 — and
    /// [`Operation::Count`] already reserves the token value they will need.
    const PARAMETERS: &'static [&'static str] = &["s", "p", "o", "format"];

    /// Read the parameters of a `/count` request.
    pub fn parse(
        params: &Params,
        limits: Limits<'_>,
        prefixes: &PrefixMap,
    ) -> Result<Self, Problem> {
        accept_only(params, COUNT, Self::PARAMETERS)?;
        Ok(Self {
            pattern: Pattern::parse(params, limits, prefixes)?,
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
/// "counts for text/range constraints are estimates", which gives `/count` the
/// same filters and no `labels` (a count has no rows to label); §3.5's
/// `labels=true` modifier row for the operations that return rows; and §3.5's
/// `fragment +g scope` and `count +g scope` rows for `g`.
const NOT_OFFERED: &[(&str, Option<Capability>, &[&str])] = &[
    ("o.text", Some(Capability::Search), &[FRAGMENT, COUNT]),
    ("o.lang", None, &[FRAGMENT, COUNT]),
    ("o.dt", None, &[FRAGMENT, COUNT]),
    ("o.ge", Some(Capability::Range), &[FRAGMENT, COUNT]),
    ("o.gt", Some(Capability::Range), &[FRAGMENT, COUNT]),
    ("o.le", Some(Capability::Range), &[FRAGMENT, COUNT]),
    ("o.lt", Some(Capability::Range), &[FRAGMENT, COUNT]),
    (
        "labels",
        Some(Capability::Search),
        &[FRAGMENT, DESCRIBE, SAMPLE],
    ),
    ("g", Some(Capability::Graphs), &[FRAGMENT, COUNT]),
];

const FRAGMENT: &str = "fragment";
const COUNT: &str = "count";
const DESCRIBE: &str = "describe";
const SAMPLE: &str = "sample";

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
            ("o.text=atrazine", "search"),
            ("o.ge=%2242%22", "range"),
            ("labels=true", "search"),
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
            Count::parse(&params(scoped), limits(), &prefixes())
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
            Count::parse(&params("labels=true"), limits(), &prefixes())
                .unwrap_err()
                .code(),
            ErrorCode::MalformedRequest,
            "a count has no rows to label"
        );

        // And an object filter belongs to the two operations that take a
        // pattern *and* report on objects.
        assert_eq!(
            Count::parse(&params("o.text=atrazine"), limits(), &prefixes())
                .unwrap_err()
                .code(),
            ErrorCode::CapabilityNotAvailable,
            "§3.4.4 says counts for text constraints are estimates, so it takes one"
        );
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
    fn a_count_takes_a_pattern_and_nothing_that_pages() {
        let parse = |query: &str| Count::parse(&params(query), limits(), &prefixes());

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
        // M1's counts are exact and arrive whole; §3.4.4's resumable scanning
        // counts are M2, and this server never issues a token for one.
        for query in ["limit=10", "cursor=abc"] {
            assert_eq!(
                parse(query).unwrap_err().code(),
                ErrorCode::MalformedRequest,
                "{query}"
            );
        }
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
