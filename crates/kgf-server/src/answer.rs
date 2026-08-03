//! What the server sends back: doc 03 §3.4's four operations, executed and
//! rendered.
//!
//! Each is thin, because units 10–13 did the work: parse terms to ids, resolve
//! a [`Selection`], page it, serialize. What is left here is the part that
//! genuinely belongs to the operations — how a page decides it is complete,
//! where a cursor's position is bounded, and how `/describe` walks two
//! enumerations behind one envelope.
//!
//! # Strings are materialized while serializing, and nowhere else
//!
//! Doc 20 §20.5's rule has a consequence for the blocking boundary: the whole
//! of an operation, *including writing the response body*, runs inside the task
//! that holds the [`Store`]. [`Answer`] holds `Rc<str>` handed out by the
//! request's [`TermCache`], so it is deliberately not `Send`; what crosses back
//! is [`Rendered`] — bytes, and the §3.6 metadata the headers repeat.
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

use std::collections::HashSet;
use std::rc::Rc;

use bytes::Bytes;
use maud::html;
use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};

use hdtc::format::{TextScanPosition, TextSearcher};
use kgf_store::catalog::BundleId;
use kgf_store::dict::Dictionary;
use kgf_store::pattern::{IdPattern, Selection};
use kgf_store::{IdTriple, Role, Store, TermId};

use crate::cursor::{Cursor, CursorBinding, PositionSpace, StaleCursor};
use crate::envelope::{BudgetReason, Cardinality, Completeness, ErrorCode, Problem};
use crate::html::{Crumb, Resource, Value, fields, json_body, note, page, table};
use crate::representation::Representation;
use crate::request::{
    self, BoundTerm, Candidates, Direction, Pattern, Position, ResponseBytes, TextFilter,
};
use crate::term::{Term, TermCache};
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
}

impl Target {
    /// The version and operation a request addressed, with its parameters.
    pub fn new(id: BundleId, operation: &'static str, params: Params) -> Self {
        Self {
            id,
            operation,
            params,
        }
    }

    /// The bundle version to open.
    pub fn id(&self) -> &BundleId {
        &self.id
    }

    fn base(&self) -> String {
        url::operation(&self.id.dataset, &self.id.version, self.operation)
    }

    /// This response's URL, with the representation selector removed.
    ///
    /// [`page`] appends `format=json` to build the footer link, and a URL that
    /// already carried `format=html` would come back with the parameter twice —
    /// which this server's own parser refuses. Dropping it is also the more
    /// honest reading of "canonical": one resource, several representations.
    fn canonical(&self) -> String {
        query(self.base(), &self.params.without("format"))
    }

    /// The same request, resumed at `token`.
    fn next(&self, token: &str) -> String {
        query(self.base(), &self.params.with("cursor", token))
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
            Crumb::to("kgf", "/".to_owned()),
            Crumb::to(&self.id.dataset, url::dataset(&self.id.dataset)),
            // There is no landing page for a version, so the version step goes
            // to the one document doc 03 §3.2 does define for it.
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
}

fn query(base: String, params: &Params) -> String {
    if params.is_empty() {
        base
    } else {
        format!("{base}?{}", params.to_query())
    }
}

/// A serialized response, and the metadata §3.6 requires on its headers.
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
/// have a single shape for all four operations — and so that adding a
/// serialization is a change the compiler routes through every answer, the same
/// reason [`Resource`] exists.
pub trait Renders {
    /// Serialize into `representation`, with the metadata its headers need.
    fn render(self, representation: Representation) -> Rendered;
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// The key `/describe` reports an edge's side under.
const DIRECTION: &str = "direction";

/// The key a text-ranked row reports its relevance under (doc 03 §3.4.1).
const SCORE: &str = "score";

/// The key a text-ranked row reports *how* it matched under (doc 03 §3.4.5).
const MATCH_KIND: &str = "match_kind";

/// How a text hit matched, in §3.4.5's vocabulary.
///
/// Emitted beside `score` because without it the score is misleading. hdtc
/// ranks exact matches as a class ahead of stemmed ones and its BM25 figures
/// are comparable only *within* a class, so a stemmed row can carry a higher
/// number than the exact row above it — and a client that sorts a page by
/// `score`, which doc 06 §6.2.1 tells it to do when merging endpoints, would
/// undo the ranking the server computed. With the class present, "by class,
/// then rank" reproduces the order this server sent.
///
/// The two vocabularies do not line up, and this reports §3.4.5's because that
/// is the one clients branch on: it names `exact | normalized | prefix |
/// fuzzy`, while hdtc classifies a hit as exact or stemmed and treats prefix
/// and fuzzy as query *modes* rather than per-hit outcomes. Stemming is a
/// normalization, so `normalized` is the honest member of the published set —
/// but it is wider than what is being said. See `notes/plan.md`, "Questions for
/// `../kgf`".
fn match_kind(kind: hdtc::format::MatchKind) -> &'static str {
    match kind {
        hdtc::format::MatchKind::Exact => "exact",
        hdtc::format::MatchKind::Stemmed => "normalized",
    }
}

/// One result row: a term per variable, and for `/describe` which side of the
/// neighborhood it came from.
#[derive(Debug)]
pub struct Row {
    cells: Vec<(Position, Rc<str>)>,
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
    /// §3.4.1's row: one key per variable, each a term object.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
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
        direction: Option<Direction>,
        ranking: Option<Ranking>,
    ) -> Self {
        let mut entries = cells.len() as u64;
        let mut serialized = 2 + terms;
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
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Echo {
    Fragment {
        pattern: Pattern,
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

/// A page of rows: doc 03 §3.4.1's envelope, shared by `/fragment`,
/// `/describe` and `/sample`.
#[derive(Debug, Serialize)]
pub struct Answer {
    dataset: String,
    version: String,
    #[serde(flatten)]
    echo: Echo,
    cardinality: Cardinality,
    /// Which bound parameters name terms this bundle's dictionary does not
    /// hold.
    ///
    /// Not in doc 03, and worth having: an empty answer because a term is
    /// absent and an empty answer because the pattern has no matches are the
    /// same response with very different remedies, and only the server can tell
    /// them apart. Unit 11 promised this diagnostic when it decided *not* to
    /// reject unusual IRIs at the edge. See `notes/plan.md`, "Questions for
    /// `../kgf`".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    absent_terms: Vec<&'static str>,
    vars: Vec<Position>,
    rows: Vec<Row>,
    #[serde(flatten)]
    completeness: Completeness,
    #[serde(skip)]
    directed: bool,
    #[serde(skip)]
    target: Target,
}

impl Renders for Answer {
    fn render(self, representation: Representation) -> Rendered {
        let body = match representation {
            Representation::Json => self.to_json(),
            Representation::Html => Bytes::from(self.to_html()),
        };
        Rendered {
            body,
            completeness: self.completeness,
        }
    }
}

/// `GET /count`'s envelope (§3.4.4).
///
/// `count` is an object rather than §3.4.4's first example's bare integer, so
/// that it is the same shape as §3.4.1's `cardinality` and the shape an
/// interrupted text count needs: `{"value": n, "exact": false, "min": n}`.
/// One field with two shapes would be a client-breaking change waiting to
/// happen. See `notes/plan.md`, "Questions for `../kgf`".
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
    fn render(self, representation: Representation) -> Rendered {
        let body = match representation {
            Representation::Json => self.to_json(),
            Representation::Html => Bytes::from(self.to_html()),
        };
        Rendered {
            body,
            completeness: self.completeness,
        }
    }
}

// ---------------------------------------------------------------------------
// The operations
// ---------------------------------------------------------------------------

/// `GET /fragment` — enumerate a triple pattern (§3.4.1).
pub fn fragment(
    store: &Store,
    target: Target,
    request: &request::Fragment,
) -> Result<Answer, Problem> {
    let dictionary = store.dict();
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
        absent_terms: Vec::new(),
    };

    match (
        resolve(&dictionary, &request.pattern)?,
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

/// `GET /count` — a pattern's cardinality (§3.4.4).
pub fn count(
    store: &Store,
    target: Target,
    request: &request::Count,
) -> Result<CountAnswer, Problem> {
    let dictionary = store.dict();
    if request.cursor.is_some() && request.pattern.text().is_none() {
        // Parsing enforces this too; keep the operation correct for callers
        // constructing the public request type directly.
        return Err(Problem::from(StaleCursor));
    }
    let (count, completeness, absent_terms) = match (
        resolve(&dictionary, &request.pattern)?,
        request.pattern.text(),
    ) {
        // Exact and free of the enumeration: a range width after bounded
        // descent for seven shapes, and for `s ? o` the same bounded
        // predicate-group probe the enumeration would run (doc 20 §20.2.1).
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

/// `GET /describe` — a resource's neighborhood (§3.4.6).
///
/// Two enumerations behind one envelope, out-edges first, and a row says which
/// it came from. That column is this crate's, not §3.4.6's, and it earns its
/// place on the one triple the two halves share: `<a> p <a>` is genuinely an
/// out-edge *and* an in-edge, so it appears twice, and without the column the
/// second copy reads as a duplicate rather than as the other half of the
/// answer. Deduplicating instead would cost an `s ? o` probe per request to
/// keep `cardinality` equal to the enumerated length, which is a cost §3.5's
/// "describe | 2 × fragment" does not budget for.
pub fn describe(
    store: &Store,
    target: Target,
    request: &request::Describe,
) -> Result<Answer, Problem> {
    let dictionary = store.dict();
    let echo = Echo::Describe {
        resource: request.resource.requested().to_owned(),
        direction: request.direction,
    };

    let mut phases = Vec::new();
    if request.direction.walks_out()
        && let Some(subject) = locate(&dictionary, Role::Subject, &request.resource)?
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
        && let Some(object) = locate(&dictionary, Role::Object, &request.resource)?
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

    paged(
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
            absent_terms,
        },
        phases,
        Paging {
            cursor: request.cursor.as_ref(),
            limit: request.limit,
            bytes: request.bytes,
            binding: &request.binding,
        },
    )
}

/// `GET /sample` — pseudo-random members of a pattern's results (§3.4.7).
pub fn sample(store: &Store, target: Target, request: &request::Sample) -> Result<Answer, Problem> {
    let dictionary = store.dict();
    let echo = Echo::Sample {
        pattern: request.pattern.clone(),
        n: request.n,
        seed: request.seed,
    };
    let vars = request.pattern.vars();

    let (count, triples, absent_terms) = match resolve(&dictionary, &request.pattern)? {
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
        vars,
        // A sample stops for one reason only. It is not paged, so `n` is what
        // it returns unless the bundle's own literals spend the byte budget
        // first — and then it says so, because there is no cursor to offer and
        // returning fewer members while claiming completeness is the silent
        // truncation §3.6 prohibits.
        completeness: match spent_at {
            None => Completeness::complete(),
            Some(_) => Completeness::budget_exhausted_without_resume(BudgetReason::ResponseBytes),
        },
        directed: false,
        target,
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
    direction: Option<Direction>,
}

fn phase(selection: Selection<'_>, direction: Option<Direction>) -> Phase<'_> {
    Phase {
        space: PositionSpace::of(&selection),
        count: selection.count().value,
        selection,
        direction,
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
    direction: Option<Direction>,
    ranking: Option<Ranking>,
}

impl Step {
    /// The token that resumes a page at this row.
    fn cursor(&self, binding: &CursorBinding) -> crate::cursor::CursorToken {
        // On the space rather than on `scan.is_some()`. The two agree today,
        // and `Cursor::at_rank` hardcodes `TextRank` — so dispatching on the
        // trailer would silently mint a ranked token for the first M2 position
        // that wants a second number in some other space, which
        // `Cursor::scan_position` is already reserved for.
        match self.space {
            PositionSpace::TextRank => {
                Cursor::at_rank(binding, self.resume, self.scan.unwrap_or(0))
            }
            space => Cursor::at(binding, space, self.resume),
        }
        .encode()
    }
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// How many distinct literals a text query matched, and whether that is the
/// whole number.
///
/// Two fields rather than one, because a count taken under a budget is not the
/// same claim as a count taken to the end: §3.4.1's `distinct_objects` is
/// documented as *exact*, so a figure that stopped at the budget has to be
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
#[derive(Debug, Clone, Copy)]
enum Spent {
    /// As deep into the ranking as this server pages. No cursor: there is
    /// nowhere further to go, and offering one would be a loop.
    Deepest,
}

/// Enumerate a pattern whose object is constrained by a text query.
///
/// The composition doc 19 §19.2.2 is built for: a hit is an object dictionary
/// id, so each one becomes `IdPattern { .., object: Some(id) }` and resolves
/// through permutations this store already holds. There is no text-specific
/// enumeration — only a different way of choosing which objects to enumerate,
/// and a different order to do it in.
///
/// # What bounds it
///
/// §3.5 budgets filtered operations on *candidates examined*, independently of
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
        // whose positions are predicate ids (doc 20 §20.2.1). Reusing the same
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

fn resolve(dictionary: &Dictionary<'_>, pattern: &Pattern) -> Result<Resolved, Problem> {
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
        match locate(dictionary, position.role(), term)? {
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
    absent_terms: Vec<&'static str>,
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
    let cardinality = Cardinality::exact(phases.iter().map(|phase| phase.count).sum());

    finish(dictionary, target, envelope, steps, paging, None, |_, _| {
        cardinality
    })
}

/// Finish a text-filtered page (§3.4.1).
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
        absent_terms,
    } = envelope;

    // Materializing is where the bytes appear, so it is where the byte budget
    // applies — before the response exists rather than after, which also bounds
    // the memory a page can take.
    let (rows, spent_at) = materialize(dictionary, &vars, &steps, paging.bytes)?;

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
        vars,
        completeness,
        directed,
        target,
    })
}

/// How many rows a text-filtered pattern matches (§3.4.1).
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
    let (start, mut from) = match cursor {
        None => (0, 0),
        Some(cursor) => {
            let index = phases
                .iter()
                .position(|phase| phase.space == cursor.space)
                .ok_or_else(|| Problem::from(StaleCursor))?;
            (index, resume_position(cursor, &phases[index], predicates)?)
        }
    };

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
                    direction: phase.direction,
                    ranking: None,
                }),
        );
        from = 0;
    }
    Ok(steps)
}

/// Pair each triple with the position a page resumes at to return it first.
///
/// The running position *before* each row, in whichever space this phase counts
/// in: an offset for the three permutation spaces, and for `s ? o` the previous
/// row's predicate id — route-independent (doc 20 §20.2.1) and strictly
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
    // No M1 operation issues a token carrying either trailer *in this space* —
    // `scan_position` belongs to `TextRank`, which is not a phase — so one that
    // does was not issued by the request it arrived on.
    if cursor.binding_index.is_some() || cursor.scan_position.is_some() {
        return Err(stale());
    }
    // A position past the end would otherwise page to an empty response, which
    // a client reads as the end of results rather than as a bad token.
    let within = match phase.space {
        PositionSpace::Predicate => (1..=predicates).contains(&cursor.position),
        _ => cursor.position < phase.count,
    };
    within.then_some(cursor.position).ok_or_else(stale)
}

/// Turn ids into terms, once per distinct term (doc 20 §20.5), within
/// `max_response_bytes`.
///
/// Returns the rows and, if the byte budget stopped it, the index of the first
/// step *not* included — which is where the next page starts.
///
/// # Why the budget lands here
///
/// §3.5 publishes `max_response_bytes` and says in the same breath that a row
/// cap is not a byte cap, "one legal literal can be megabytes" — and bundles
/// really do hold them, so `limit` alone leaves a response unbounded, which is
/// the one thing this project exists to prevent. Applying it while rows are
/// built rather than after they are serialized also bounds what a page costs in
/// *memory*: the terms are in hand at this point, and a page assembled first
/// and measured second would have to fit before it could be refused.
///
/// The measure is each row's compact JSON — exact for the serialization §3.4.1
/// defines, and conservative for the page, which is not one of §3.4.1's formats
/// at all. It is *counted* rather than produced: [`TermCache`] weighs each
/// distinct term once and [`Row::new`] adds the map's fixed punctuation, so a
/// page pays per term rather than per row. Serializing every row to size it
/// cost a third of what rendering the response costs (10 000 rows: 1.0 ms of
/// weighing against 3.0 ms of rendering), which is what that arrangement is
/// worth avoiding.
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
        let row = Row::new(cells, terms, step.direction, step.ranking);

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
        // Dense. `count` is under twice `n`, so under twice §3.5's cap, and a
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
/// the *same work*: §3.4.7 has the server "run its bounded smaller-endpoint
/// probe once, hold the resulting predicate-id set in request-local memory, and
/// sample positions from that set", and `Selection::count` is that probe. Asking
/// for the count first and the members afterwards runs it twice — which is what
/// this did until the review caught it, and what doc 20 §20.2.1 budgets exactly
/// one of.
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
/// Written out rather than taken from a crate, because §3.4.7 makes the draw
/// part of the response's contract — "deterministic for a given seed +
/// version, hence cacheable" — and a generator whose stream may change between
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
        let mut headers: Vec<&str> = self.vars.iter().map(|var| var.as_str()).collect();
        if self.directed {
            headers.push("direction");
        }
        if self.rows.iter().any(|row| row.ranking.is_some()) {
            headers.extend([SCORE, MATCH_KIND]);
        }

        page(
            &self.target.title(),
            &self.target.crumbs(),
            Some(&self.target.canonical()),
            html! {
                (fields(&self.summary()))
                @if !self.absent_terms.is_empty() {
                    (note(&format!(
                        "This bundle's dictionary holds no term for {}. The answer is empty for \
                         that reason, not because the pattern has no matches.",
                        self.absent_terms.join(", ")
                    )))
                }

                h2 { "Rows" }
                @if self.vars.is_empty() {
                    (note(
                        "Every position is bound, so a row has nothing to report beyond its own \
                         existence; the cardinality above is the answer."
                    ))
                } @else if rows.is_empty() {
                    (note("No rows."))
                } @else {
                    (table(&headers, &rows))
                }

                @if let Some(token) = self.completeness.next_cursor() {
                    p { a href=(self.target.next(token)) { "Next page →" } }
                }
            },
        )
    }
}

impl Answer {
    /// The fields above the table: what was asked, and how much of it came back.
    fn summary(&self) -> Vec<(&str, Value<'_>)> {
        let mut summary = match &self.echo {
            Echo::Fragment { pattern } => pattern_fields(pattern),
            Echo::Describe {
                resource,
                direction,
            } => vec![
                ("iri", Value::Code(resource)),
                ("direction", Value::Text(direction.as_str())),
            ],
            Echo::Sample { pattern, .. } => pattern_fields(pattern),
        };
        summary.push(("cardinality", Value::Number(self.cardinality.value())));
        summary.push(("returned", Value::Number(self.rows.len() as u64)));
        if let Echo::Sample { n, seed, .. } = &self.echo {
            summary.push(("n", Value::Number(u64::from(*n))));
            summary.push(("seed", Value::Number(*seed)));
        }
        summary.push((
            "complete",
            Value::Text(if self.completeness.is_complete() {
                "yes"
            } else {
                "no — the page filled"
            }),
        ));
        summary
    }

    /// Every cell of the table, owned, so the [`Value`]s below can borrow it.
    fn cells(&self) -> Vec<Vec<Cell>> {
        self.rows
            .iter()
            .map(|row| {
                let mut cells: Vec<Cell> = row
                    .cells
                    .iter()
                    .map(|(position, text)| self.cell(*position, text))
                    .collect();
                if let Some(direction) = row.direction {
                    cells.push(Cell {
                        label: direction.as_str().to_owned(),
                        href: None,
                    });
                }
                if let Some(ranking) = row.ranking {
                    cells.push(Cell {
                        label: ranking.score.to_string(),
                        href: None,
                    });
                    cells.push(Cell {
                        label: ranking.kind.to_owned(),
                        href: None,
                    });
                }
                cells
            })
            .collect()
    }

    /// One term, and the request that asks about it.
    ///
    /// This is what makes the page a way *into* the data rather than a dump of
    /// it: a subject or object links to its own neighborhood, a predicate to
    /// every triple using it, a literal to every triple carrying it.
    fn cell(&self, position: Position, text: &str) -> Cell {
        let term = Term::from_dictionary(text);
        let label = term.to_request();
        let href = match (&term, position) {
            (Term::Literal(_), _) => self.target.ask("fragment", "o", &label),
            (_, Position::Predicate) => self.target.ask("fragment", "p", &label),
            _ => self.target.ask("describe", "iri", &label),
        };
        Cell {
            label,
            href: Some(href),
        }
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

        page(
            &self.target.title(),
            &self.target.crumbs(),
            Some(&self.target.canonical()),
            html! {
                (fields(&summary))
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
                p {
                    a href=(query(
                        url::operation(&self.target.id.dataset, &self.target.id.version, "fragment"),
                        &self.target.params.without("cursor"),
                    )) { "The rows themselves →" }
                }
                @if let Some(token) = self.completeness.next_cursor() {
                    p { a href=(self.target.next(token)) { "Continue counting →" } }
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

/// A rendered table cell, held so the borrowed [`Value`] can point at it.
struct Cell {
    label: String,
    href: Option<String>,
}

impl Cell {
    fn value(&self) -> Value<'_> {
        match &self.href {
            Some(href) => Value::Link {
                href: href.clone(),
                label: &self.label,
            },
            None => Value::Text(&self.label),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                        let cells: Vec<(Position, Rc<str>)> = Position::ALL[..width]
                            .iter()
                            .map(|position| (*position, Rc::from(term)))
                            .collect();
                        // What the cache would have measured for each cell.
                        let each = serde_json::to_vec(&Term::from_dictionary(term))
                            .expect("a term serializes")
                            .len() as u64;

                        let row = Row::new(cells, each * width as u64, direction, score);
                        assert_eq!(
                            row.serialized,
                            serde_json::to_vec(&row).expect("a row serializes").len() as u64,
                            "width {width}, {term:?}, {direction:?}, {score:?}"
                        );
                        shapes += 1;
                    }
                }
            }
        }
        assert!(shapes >= 80, "{shapes} shapes");
    }

    #[test]
    fn a_draw_is_uniform_deterministic_and_free_of_repeats() {
        // The contract §3.4.7 states: the same seed and version draw the same
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
