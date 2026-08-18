//! Completeness, cardinality and errors: doc 03 §3.6's uniform vocabulary.
//!
//! Every response says whether it is the whole answer. That is the promise the
//! project rests on — §3.6 puts it as "silent truncation is prohibited", which
//! is what makes "trust the result" a client decision rather than a guess — and
//! it is a promise about a *combination of fields*, so it is kept here by types
//! rather than by remembering to set them.
//!
//! # The shape a response can have
//!
//! [`Completeness`] has exactly two: complete, or truncated with a reason. It
//! cannot be truncated without saying why, and it cannot be complete and still
//! offer a next page. Those are not validations run on the way out; there is no
//! constructor that builds them.
//!
//! Whether a truncation *may* resume is a property of the reason: a page that
//! stopped at the limit has somewhere to continue from, a star cell that
//! overflowed does not, and [`TruncationReason::resumes`] is derived from that
//! rather than tracked beside it. [`BudgetReason`] exists so that the
//! constructors taking a reason as a value can only be given a resuming one —
//! the compiler decides it, not an assertion.
//!
//! *May*, not *must*, and the difference is one operation. `/sample` draws `n`
//! members and never pages (§3.4.7), so a byte budget can stop it while there
//! is no position for a cursor to name. Its constructor is separate and says so
//! ([`Completeness::budget_exhausted_without_resume`]), which keeps forgetting
//! a cursor impossible for the operations that have one.
//!
//! The cursor is a [`CursorToken`], which only [`Cursor::encode`] mints. An
//! arbitrary string would let a truncated response carry an empty continuation,
//! or one containing CR/LF, which `KGF-Next-Cursor` would turn into header
//! injection.
//!
//! [`Cursor::encode`]: crate::cursor::Cursor::encode
//!
//! # Both channels, always
//!
//! §3.6 requires the same metadata in the body *and* on the response headers,
//! because CSV, Parquet, Arrow and the RDF serializations have nowhere in the
//! body to put it. That duplication is a correctness obligation rather than a
//! convenience: a CSV response that loses `complete` is a protocol violation,
//! not a cosmetic gap. [`Completeness::headers`] and the [`serde::Serialize`]
//! impl are therefore two renderings of one value, and a test asserts they say
//! the same thing for every shape.
//!
//! # What M1 emits
//!
//! [`TruncationReason::PageLimit`] and [`TruncationReason::ResponseBytes`]. The
//! second was expected to wait for M2's interruptible scans and does not,
//! because it is the one composite budget no cap can bound: §3.5 pairs
//! `max_response_bytes` with the observation that "one legal literal can be
//! megabytes", so a page of `limit` rows is bounded in rows and unbounded in
//! bytes, and a plain `/fragment` reaches it on real data.
//!
//! `candidate_budget` is also emitted by `o.text` scans. `time_budget`,
//! `cell_overflow`, and `partial_failure` belong to operations M1 does not have
//! — but the vocabulary is closed *now*, so the type cannot later grow a
//! stringly-typed escape hatch for a reason someone forgot to add.

use crate::cursor::CursorToken;
use serde::ser::{Serialize, SerializeMap, Serializer};

// ---------------------------------------------------------------------------
// Completeness
// ---------------------------------------------------------------------------

/// Why a response carries less than the whole answer (doc 03 §3.6).
///
/// Closed. A truncation this does not name is a truncation a client cannot
/// reason about, which is the thing §3.6 forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TruncationReason {
    /// The page filled: `limit` rows were returned and more match.
    PageLimit,
    /// A time budget expired mid-scan (M2).
    TimeBudget,
    /// A candidate budget was spent before the scan finished.
    CandidateBudget,
    /// The response reached its byte budget.
    ResponseBytes,
    /// One star cell had more values than its cap allows (M2, `/star`).
    CellOverflow,
    /// A member of a batch or fan-out failed (M2).
    PartialFailure,
}

impl TruncationReason {
    /// The token §3.6 puts on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PageLimit => "page_limit",
            Self::TimeBudget => "time_budget",
            Self::CandidateBudget => "candidate_budget",
            Self::ResponseBytes => "response_bytes",
            Self::CellOverflow => "cell_overflow",
            Self::PartialFailure => "partial_failure",
        }
    }

    /// Whether a response stopped for this reason *may* carry a cursor.
    ///
    /// The four interruption reasons stop an enumeration, which normally has a
    /// position to resume from. The other two describe a result that is already
    /// as complete as it will get: an overflowing star cell has no more values
    /// the caps allow, and a failed fan-out member is not retried by paging. A
    /// client that pages on those would loop, so they must never carry one.
    ///
    /// May, not must. An operation without positions can still exhaust a
    /// budget — `/sample` draws `n` members and never pages (§3.4.7) — and then
    /// the reason is honest while the cursor does not exist. So this bounds
    /// which truncations a cursor is *permitted* on; whether a given response
    /// has one is [`Completeness`]'s business, and its constructors decide it.
    pub fn resumes(self) -> bool {
        match self {
            Self::PageLimit | Self::TimeBudget | Self::CandidateBudget | Self::ResponseBytes => {
                true
            }
            Self::CellOverflow | Self::PartialFailure => false,
        }
    }
}

/// A budget whose exhaustion stopped an operation.
///
/// The subset of [`TruncationReason`] that a budgeted operation can report, so
/// that "this reason resumes" is checked by the compiler at the call site
/// instead of asserted inside the constructor. Without it,
/// [`Completeness::budget_exhausted`] is the one place a non-resuming reason
/// could acquire a cursor, and a client paging on `cell_overflow` never stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BudgetReason {
    /// A time budget expired mid-scan.
    Time,
    /// A candidate budget was spent before the scan finished.
    Candidate,
    /// The response reached its byte budget.
    ResponseBytes,
}

impl From<BudgetReason> for TruncationReason {
    fn from(reason: BudgetReason) -> Self {
        match reason {
            BudgetReason::Time => Self::TimeBudget,
            BudgetReason::Candidate => Self::CandidateBudget,
            BudgetReason::ResponseBytes => Self::ResponseBytes,
        }
    }
}

/// Whether a response is the whole answer, and if not, why not.
///
/// Opaque on purpose: the constructors are the only way to build one, and a
/// cursor comes from the one the operation's own paging picked. There is no way
/// to express "incomplete, no reason", "complete, but here is a next page", or
/// a cursor on a reason that cannot be continued from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completeness(State);

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Complete,
    Truncated {
        reason: TruncationReason,
        next: Option<CursorToken>,
    },
}

impl Completeness {
    /// The whole answer.
    pub fn complete() -> Self {
        Self(State::Complete)
    }

    /// The page filled at `limit`; `next` returns the row it could not carry.
    pub fn page_limit(next: CursorToken) -> Self {
        Self(State::Truncated {
            reason: TruncationReason::PageLimit,
            next: Some(next),
        })
    }

    /// A scan stopped on a budget, resumable from `next`.
    pub fn budget_exhausted(reason: BudgetReason, next: CursorToken) -> Self {
        Self(State::Truncated {
            reason: reason.into(),
            next: Some(next),
        })
    }

    /// A budget stopped an operation that has nowhere to resume from.
    ///
    /// Two operations are in this position and neither is an oversight.
    /// `/sample` draws `n` members and never pages (§3.4.7), so a response-byte
    /// stop has no position to name. A partial relevance window likewise has no
    /// cursor beyond the candidates it retained: recreating a global rank at
    /// arbitrary depth would require unbounded scoring and memory. Reporting
    /// either stop is the alternative to calling a partial result complete,
    /// which is the silent truncation §3.6 prohibits.
    ///
    /// The wire shape — incomplete, a reason, no `next` — is one §3.6 already
    /// defines and clients already handle, since `cell_overflow` and
    /// `partial_failure` produce it too.
    ///
    /// Separate from [`budget_exhausted`](Self::budget_exhausted) so that
    /// forgetting a cursor stays impossible for the operations that have one:
    /// the paging constructors still take a token by value.
    pub fn budget_exhausted_without_resume(reason: BudgetReason) -> Self {
        Self(State::Truncated {
            reason: reason.into(),
            next: None,
        })
    }

    /// A star cell exceeded its values cap (M2). Nothing to resume.
    pub fn cell_overflow() -> Self {
        Self(State::Truncated {
            reason: TruncationReason::CellOverflow,
            next: None,
        })
    }

    /// A batch or fan-out member failed (M2). Nothing to resume.
    pub fn partial_failure() -> Self {
        Self(State::Truncated {
            reason: TruncationReason::PartialFailure,
            next: None,
        })
    }

    // No shared `interrupted(reason, next)` helper: it would take an
    // unrestricted `TruncationReason`, which is the signature this unit's
    // review removed from the public constructor. Two four-line bodies are
    // cheaper than a private path that can rebuild the state the type exists
    // to forbid.

    /// Whether this is the whole answer.
    pub fn is_complete(&self) -> bool {
        matches!(self.0, State::Complete)
    }

    /// Why the response stopped short, if it did.
    pub fn truncation_reason(&self) -> Option<TruncationReason> {
        match self.0 {
            State::Complete => None,
            State::Truncated { reason, .. } => Some(reason),
        }
    }

    /// The cursor that continues this response, if one exists.
    pub fn next_cursor(&self) -> Option<&str> {
        match &self.0 {
            State::Complete => None,
            State::Truncated { next, .. } => next.as_ref().map(CursorToken::as_str),
        }
    }

    /// The `KGF-*` headers carrying this to formats whose bodies cannot (§3.6).
    ///
    /// Same information as the JSON, in the channel a Parquet or CSV response
    /// has. Emitted for every format, including JSON: a client should not have
    /// to parse a body to learn whether it is complete, and an intermediary
    /// cannot.
    /// Borrowed and allocation-free: every value is either a literal or already
    /// owned by this `Completeness`, and this runs once per response.
    pub fn headers(&self) -> impl Iterator<Item = (&'static str, &str)> + '_ {
        [
            Some((
                "KGF-Complete",
                if self.is_complete() { "true" } else { "false" },
            )),
            self.truncation_reason()
                .map(|reason| ("KGF-Truncation-Reason", reason.as_str())),
            self.next_cursor().map(|next| ("KGF-Next-Cursor", next)),
        ]
        .into_iter()
        .flatten()
    }
}

impl Serialize for Completeness {
    /// The three envelope fields, for `#[serde(flatten)]` into a response.
    ///
    /// `next` is always present, null when there is none, following §3.4.1's
    /// example; `truncation_reason` appears only when there is one, which the
    /// same example implies by omitting it. See `notes/plan.md`, question 14.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("complete", &self.is_complete())?;
        if let Some(reason) = self.truncation_reason() {
            map.serialize_entry("truncation_reason", reason.as_str())?;
        }
        map.serialize_entry("next", &self.next_cursor())?;
        map.end()
    }
}

// ---------------------------------------------------------------------------
// Cardinality
// ---------------------------------------------------------------------------

/// How many rows match, and how well the server knows (doc 03 §3.4.1).
///
/// Opaque for [`Completeness`]'s reason: the pieces §3.4.1 and §3.4.4 attach to
/// a count constrain each other, and a struct of public fields cannot say so.
/// An exact count takes no lower bound, because it *is* its own bound; a lower
/// bound cannot exceed the estimate it bounds; and `distinct_objects` is exact
/// even on a response whose `value` is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cardinality(Count);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Count {
    Exact(u64),
    Estimated {
        value: u64,
        min: Option<u64>,
        distinct_objects: Option<u64>,
    },
}

impl Cardinality {
    /// A count known exactly — O(log N) from HDT for a plain pattern, which is
    /// every count M1 reports.
    pub fn exact(value: u64) -> Self {
        Self(Count::Exact(value))
    }

    /// An estimate, as text and range constraints give (§3.4.1).
    pub fn estimated(value: u64) -> Self {
        Self(Count::Estimated {
            value,
            min: None,
            distinct_objects: None,
        })
    }

    /// Record a count actually reached before a scan was interrupted (§3.4.4).
    ///
    /// Raises `value` to `min` when the scan overshot the estimate. The two
    /// numbers come from different computations — one counted, one guessed — so
    /// a scan that got to 50 under an estimate of 10 is not a contradiction to
    /// report but an estimate that has been proven too low. Emitting the pair
    /// as given would tell a client "at least 50, about 10", which is not a
    /// statement about anything.
    ///
    /// # Panics
    ///
    /// Panics on an exact count, which has no scan to interrupt and whose value
    /// is already its own lower bound.
    pub fn at_least(self, min: u64) -> Self {
        match self.0 {
            Count::Exact(_) => panic!("an exact count is already its own lower bound"),
            Count::Estimated {
                value,
                distinct_objects,
                ..
            } => Self(Count::Estimated {
                value: value.max(min),
                min: Some(min),
                distinct_objects,
            }),
        }
    }

    /// Record the exact number of distinct matching objects (§3.4.1).
    ///
    /// A range reports two quantities at once: `value` stays an estimate,
    /// because summing occurrences over the slice is O(slice), while the count
    /// of distinct objects is exact from two binary searches or a popcount.
    ///
    /// # Panics
    ///
    /// Panics on an exact count, where the distinction does not arise.
    pub fn over_distinct_objects(self, distinct_objects: u64) -> Self {
        match self.0 {
            Count::Exact(_) => panic!("an exact count does not carry a separate object count"),
            Count::Estimated { value, min, .. } => Self(Count::Estimated {
                value,
                min,
                distinct_objects: Some(distinct_objects),
            }),
        }
    }

    /// The reported count.
    pub fn value(self) -> u64 {
        match self.0 {
            Count::Exact(value) | Count::Estimated { value, .. } => value,
        }
    }

    /// Whether the count is exact.
    pub fn is_exact(self) -> bool {
        matches!(self.0, Count::Exact(_))
    }

    /// The lower bound a scan established, if it was interrupted.
    pub fn min(self) -> Option<u64> {
        match self.0 {
            Count::Exact(_) => None,
            Count::Estimated { min, .. } => min,
        }
    }

    /// The exact count of distinct matching objects, if this is a range.
    pub fn distinct_objects(self) -> Option<u64> {
        match self.0 {
            Count::Exact(_) => None,
            Count::Estimated {
                distinct_objects, ..
            } => distinct_objects,
        }
    }
}

impl Serialize for Cardinality {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("value", &self.value())?;
        map.serialize_entry("exact", &self.is_exact())?;
        if let Some(min) = self.min() {
            map.serialize_entry("min", &min)?;
        }
        if let Some(distinct_objects) = self.distinct_objects() {
            map.serialize_entry("distinct_objects", &distinct_objects)?;
        }
        map.end()
    }
}

// ---------------------------------------------------------------------------
// Errors (RFC 9457)
// ---------------------------------------------------------------------------

/// The media type an error response carries (RFC 9457 §3).
///
/// Fixed by the RFC rather than chosen, and kept here rather than deferred to
/// its first caller in unit 13 so that the `Content-Type` of an error and the
/// document [`Problem`] serializes are decided in one place.
pub const PROBLEM_MEDIA_TYPE: &str = "application/problem+json";

/// A machine-readable error code (doc 03 §3.6.1).
///
/// Enumerated, and one code is one status: a condition that needs a different
/// status is a different code, not the same code carrying a status beside it.
/// That is what lets a client branch on `code` alone and lets §3.6.1 be a
/// table — `unsupported_format`, `not_acceptable` and `unsupported_media_type`
/// are three ways to fail content negotiation with three remedies, and merging
/// them would leave an agent unable to tell which applies. Doc 03 permits a
/// server to add codes for conditions its normative table does not cover;
/// [`PreconditionFailed`](Self::PreconditionFailed) is such an extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// A term parameter is not a term (§3.3).
    BadTermSyntax,
    /// A parameter is missing, repeated, or unparseable.
    MalformedRequest,
    /// A parameter exceeds a published cap (§3.5).
    CapExceeded,
    /// A cursor is for a different version, operation or request (§3.6).
    StaleCursor,
    /// `format=` names a serialization this operation does not offer (§3.4.1).
    UnsupportedFormat,
    /// No such dataset or version.
    NotFound,
    /// No representation satisfies `Accept`.
    NotAcceptable,
    /// `If-None-Match` was false for a method other than GET or HEAD.
    PreconditionFailed,
    /// The method is not one this resource takes (§3.6.1).
    ///
    /// Unavoidable rather than chosen: any HTTP server must answer a POST to a
    /// read-only resource, and §3.6.1 requires every error response to carry a
    /// code. What makes it worth having is the other direction — a client
    /// probing whether this server speaks RFC 10008 QUERY gets a coded answer
    /// with an `Allow` header rather than an empty 405.
    MethodNotAllowed,
    /// A request body's media type is not supported (`Accept-Query`, §3.6).
    UnsupportedMediaType,
    /// Serialized request input exceeds `max_request_bytes` (§3.5).
    PayloadTooLarge,
    /// The server's current rate or concurrent-work capacity is exhausted (§3.6).
    RateLimited,
    /// The bundle does not offer what the request needs (§3.4, §3.7).
    CapabilityNotAvailable,
    /// The server failed. Not the client's mistake, and still coded, so a
    /// client never has to fall back to reading statuses (§3.6.1).
    InternalError,
}

impl ErrorCode {
    /// Every code, in §3.6.1's order.
    ///
    /// Exists so the test that transcribes that table can prove it transcribed
    /// *all* of it. Without it the test passes by covering the codes someone
    /// remembered, which is how `rate_limited` and `internal_error` sat
    /// unchecked between the commit that added them and the one that noticed.
    pub const ALL: &'static [ErrorCode] = &[
        Self::BadTermSyntax,
        Self::MalformedRequest,
        Self::CapExceeded,
        Self::StaleCursor,
        Self::UnsupportedFormat,
        Self::NotFound,
        Self::MethodNotAllowed,
        Self::NotAcceptable,
        Self::PreconditionFailed,
        Self::PayloadTooLarge,
        Self::UnsupportedMediaType,
        Self::RateLimited,
        Self::InternalError,
        Self::CapabilityNotAvailable,
    ];

    /// This code's row of §3.6.1: wire token, status, and the status's reason
    /// phrase.
    ///
    /// One match over the enum rather than three, and in particular the phrase
    /// is *not* derived by matching on the status: that would check
    /// exhaustiveness over `u16` instead of over the variants, so a new code
    /// with a new status would compile and then panic while serializing the
    /// error response it was added for.
    const fn row(self) -> (&'static str, u16, &'static str) {
        match self {
            Self::BadTermSyntax => ("bad_term_syntax", 400, "Bad Request"),
            Self::MalformedRequest => ("malformed_request", 400, "Bad Request"),
            Self::CapExceeded => ("cap_exceeded", 400, "Bad Request"),
            Self::StaleCursor => ("stale_cursor", 400, "Bad Request"),
            Self::UnsupportedFormat => ("unsupported_format", 400, "Bad Request"),
            Self::NotFound => ("not_found", 404, "Not Found"),
            Self::MethodNotAllowed => ("method_not_allowed", 405, "Method Not Allowed"),
            Self::NotAcceptable => ("not_acceptable", 406, "Not Acceptable"),
            Self::PreconditionFailed => ("precondition_failed", 412, "Precondition Failed"),
            Self::UnsupportedMediaType => ("unsupported_media_type", 415, "Unsupported Media Type"),
            Self::PayloadTooLarge => ("payload_too_large", 413, "Content Too Large"),
            Self::RateLimited => ("rate_limited", 429, "Too Many Requests"),
            Self::InternalError => ("internal_error", 500, "Internal Server Error"),
            Self::CapabilityNotAvailable => ("capability_not_available", 501, "Not Implemented"),
        }
    }

    /// The token that goes in the problem document's `code`.
    pub fn as_str(self) -> &'static str {
        self.row().0
    }

    /// The HTTP status this code is reported with (§3.6.1).
    ///
    /// Every client mistake is 400 except the ones that are not mistakes of
    /// that kind: a bundle that does not publish a capability answers **501**,
    /// because the request is well formed and the same request against a bundle
    /// that does publish it succeeds — the shortfall is the server's.
    pub fn status(self) -> u16 {
        self.row().1
    }

    /// RFC 9457's `title`: the status's reason phrase.
    ///
    /// §4.2.1 requires this when `type` is `about:blank`, which it is here (see
    /// [`Problem`]). A KGF-specific phrase would be more informative and would
    /// also be a conformance bug, and it is not needed: `code` is what §3.6
    /// tells agents to read, and `detail` is what tells a human what happened.
    pub fn title(self) -> &'static str {
        self.row().2
    }
}

impl ErrorCode {
    /// The code for an error response some other layer produced.
    ///
    /// A backstop, not a lookup table: §3.6.1 says *every* error response
    /// carries a code, and this crate does not raise every error response. A
    /// `tower` layer can answer before any handler runs — the body limit does —
    /// and future ones will. Rather than teach the renderer about each, an
    /// error that arrives with no [`Problem`] attached is given one from its
    /// status here.
    ///
    /// Only statuses this table maps 1:1 are recognised. 400 deliberately is
    /// not: five codes share it and guessing between them would tell a client
    /// the wrong thing to fix.
    pub fn for_unattributed_status(status: u16) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|code| code.status() == status && status != 400)
    }
}

/// Cap a client-supplied string before it is quoted back in a `detail`.
///
/// Every problem message names the value that caused it, which is what makes
/// them actionable — and what makes an unauthenticated caller able to choose
/// how large the response is. An 8 KB dataset name would be echoed into
/// `detail` and again into `instance`, so the error is larger than the request
/// that bought it, from routes that do no other work.
///
/// Lives here rather than in the router because the messages do: a `detail` is
/// built wherever the failure is detected, and truncating only at the edge
/// would mean the one built in `service` or `url` still went out whole.
pub fn reflected(text: &str) -> String {
    const LIMIT: usize = 200;
    match text.char_indices().nth(LIMIT) {
        None => text.to_owned(),
        Some((cut, _)) => format!("{}…", &text[..cut]),
    }
}

/// An RFC 9457 problem document.
///
/// `type` is `about:blank`, RFC 9457 §4.2.1's value for a problem with no
/// dereferenceable type URI, and `title` is therefore the status reason phrase
/// that section requires alongside it. Minting `https://…/problems/{code}`
/// URIs would claim a namespace nothing currently serves, and §4.2.1 asks a
/// type URI to document itself when dereferenced. Doc 03 §3.6.1 records both
/// the choice and the way out of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    code: ErrorCode,
    detail: String,
    instance: Option<String>,
    retry_after_seconds: Option<u64>,
}

impl Problem {
    /// A problem of `code`, with `detail` saying what specifically went wrong.
    ///
    /// `detail` is the part an agent acts on, so it should name the offending
    /// value and the fix — §3.6's "error messages are agent UX". The
    /// [`TermSyntaxError`](crate::term::TermSyntaxError) messages are written to
    /// that standard and convert directly.
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            instance: None,
            retry_after_seconds: None,
        }
    }

    /// Tell a client when an otherwise retryable problem may be tried again.
    ///
    /// This is HTTP response metadata rather than a problem-document field, so
    /// serialization deliberately omits it; [`crate::routes`] emits it as
    /// `Retry-After`. Kept on the problem so every negotiated rendering of the
    /// same failure carries the same retry advice.
    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds);
        self
    }

    /// Attach the request URI this problem is about (RFC 9457 `instance`).
    pub fn about(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Attach `instance` only if nothing more specific was attached already.
    ///
    /// The renderer fills this in from the request path for every problem, and
    /// must not overwrite a handler that pointed at something narrower — a
    /// bundle version, a cursor — which is what an unconditional
    /// [`about`](Self::about) there would do silently.
    pub fn about_unless_set(mut self, instance: impl Into<String>) -> Self {
        if self.instance.is_none() {
            self.instance = Some(instance.into());
        }
        self
    }

    /// The code, which is what a client branches on.
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// The HTTP status to send this with.
    pub fn status(&self) -> u16 {
        self.code.status()
    }

    /// Seconds to emit in `Retry-After`, when this failure is retryable.
    pub fn retry_after_seconds(&self) -> Option<u64> {
        self.retry_after_seconds
    }
}

impl Serialize for Problem {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("type", "about:blank")?;
        map.serialize_entry("title", self.code.title())?;
        map.serialize_entry("status", &self.code.status())?;
        map.serialize_entry("detail", &self.detail)?;
        if let Some(instance) = &self.instance {
            map.serialize_entry("instance", instance)?;
        }
        map.serialize_entry("code", self.code.as_str())?;
        map.end()
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.detail)
    }
}

/// A `Problem` is the crate's error currency — the [`From`] impls below let a
/// handler write `?` — so it carries the traits an error is expected to have,
/// or it cannot be logged through `tracing`'s `Display` bindings or wrapped by
/// `anyhow`. There is no `source`: `detail` has already absorbed the message of
/// whatever it was converted from, and keeping the original would put the same
/// text in a response body and an error chain that can drift apart.
impl std::error::Error for Problem {}

impl From<crate::term::TermSyntaxError> for Problem {
    /// Every way a term can be malformed is one code on the wire (§3.6), with
    /// the variant's message as `detail` — which is why those messages name the
    /// offending token and the remedy rather than restating the code.
    fn from(error: crate::term::TermSyntaxError) -> Self {
        Self::new(ErrorCode::BadTermSyntax, error.to_string())
    }
}

impl From<crate::cursor::StaleCursor> for Problem {
    /// Every way a token can be rejected is this one code, deliberately, and
    /// the `detail` stays as uninformative as the error itself: distinguishing
    /// "wrong version" from "wrong request" would tell a client something about
    /// data it did not query.
    fn from(_: crate::cursor::StaleCursor) -> Self {
        Self::new(
            ErrorCode::StaleCursor,
            "this cursor does not belong to this version, operation and request; \
             start the enumeration again from its first page",
        )
    }
}

impl crate::html::Resource for Problem {
    /// The RFC 9457 document.
    fn to_json(&self) -> bytes::Bytes {
        crate::html::json_body(self)
    }

    /// The same failure, for someone who arrived in a browser.
    ///
    /// Worth having rather than letting a person read raw JSON: doc 03 §3.6
    /// calls error messages agent UX, and the human case is the same argument.
    /// `detail` already names the offending value and the remedy, so the page
    /// is mostly a frame around it.
    fn to_html(&self) -> String {
        use crate::html::{Value, fields, note, page};

        // No crumbs: the masthead's brand already links back to `/`, which is
        // the one place every error page can honestly point.
        page(
            self.code.title(),
            &[],
            None,
            maud::html! {
                section."panel" {
                    p { (self.detail) }
                    (fields(&[
                        ("code", Value::Code(self.code.as_str())),
                        ("status", Value::Number(u64::from(self.code.status()))),
                        (
                            "instance",
                            self.instance.as_deref().map_or(Value::Absent, Value::Code),
                        ),
                    ]))
                    (note(
                        "This is an RFC 9457 problem document. The machine-readable form, which \
                         carries the same code, is what a client should branch on."
                    ))
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::{
        BundleBinding, CanonicalRequest, Cursor, CursorBinding, Operation, PositionSpace,
    };

    /// A real encoded cursor, not a stand-in.
    ///
    /// Going through `Cursor::encode` is the point: it is the only thing that
    /// makes a [`CursorToken`], so this also checks the two units compose, and
    /// the header assertions below run against a token of the shape clients
    /// actually receive rather than a hand-written word.
    fn token() -> CursorToken {
        let bundle = BundleBinding::from_content_digest(
            "sha256:1f0e3dad99908345f7439f8ffabdffc4de5f7439f8ffabdffc41f0e3dad99908",
        )
        .expect("a well-formed digest");
        let request = CanonicalRequest::new(Operation::Fragment).with("p", "rdfs:label");
        let binding = CursorBinding::new(&bundle, &request);
        Cursor::at(&binding, PositionSpace::Spo, 100).encode()
    }

    /// Every shape a response's completeness can take.
    fn every_shape() -> Vec<Completeness> {
        vec![
            Completeness::complete(),
            Completeness::page_limit(token()),
            Completeness::budget_exhausted(BudgetReason::Time, token()),
            Completeness::budget_exhausted(BudgetReason::Candidate, token()),
            Completeness::budget_exhausted(BudgetReason::ResponseBytes, token()),
            Completeness::budget_exhausted_without_resume(BudgetReason::ResponseBytes),
            Completeness::cell_overflow(),
            Completeness::partial_failure(),
        ]
    }

    #[test]
    fn a_response_is_complete_or_says_why_not() {
        // §3.6's prohibition on silent truncation, as a property: there is no
        // constructor for the combinations in between, so this checks that the
        // ones that exist stay on the two legal shapes.
        for completeness in every_shape() {
            match completeness.truncation_reason() {
                None => {
                    assert!(completeness.is_complete());
                    assert_eq!(
                        completeness.next_cursor(),
                        None,
                        "a complete response has nowhere to continue"
                    );
                }
                Some(reason) => {
                    assert!(!completeness.is_complete());
                    // One direction, not both. A reason that cannot resume must
                    // never carry a cursor — that is the loop `cell_overflow`
                    // would put a client in. A reason that can may still lack
                    // one, because the operation may have no position to name:
                    // a `/sample` that spends its byte budget is truncated for
                    // a resumable reason and has nowhere to resume.
                    assert!(
                        completeness.next_cursor().is_none() || reason.resumes(),
                        "{} must not carry a cursor",
                        reason.as_str()
                    );
                }
            }
        }
    }

    #[test]
    fn the_headers_say_what_the_body_says() {
        // The obligation that makes CSV and Parquet responses honest. Checked
        // as agreement between the two renderings rather than as two expected
        // outputs, so neither can drift on its own.
        for completeness in every_shape() {
            let emitted: Vec<_> = completeness.headers().collect();
            let headers: std::collections::HashMap<_, _> = emitted.iter().copied().collect();
            // Collecting into a map would hide a repeated name, and a response
            // carrying one header twice with different values is resolved
            // arbitrarily by whatever reads it.
            assert_eq!(
                emitted.len(),
                headers.len(),
                "no header name may be emitted twice: {emitted:?}"
            );

            let body = serde_json::to_value(&completeness).expect("serialize");

            assert_eq!(
                headers["KGF-Complete"],
                body["complete"]
                    .as_bool()
                    .expect("complete is a bool")
                    .to_string(),
                "KGF-Complete must agree with `complete`"
            );
            assert_eq!(
                headers.get("KGF-Truncation-Reason").copied(),
                body.get("truncation_reason")
                    .and_then(|value| value.as_str()),
                "the reason must appear in both channels or neither"
            );
            assert_eq!(
                headers.get("KGF-Next-Cursor").copied(),
                body["next"].as_str(),
                "the cursor must appear in both channels or neither"
            );

            // Every value must survive being put in a header: `KGF-Next-Cursor`
            // is why `CursorToken` exists, and this is the assertion that keeps
            // that guarantee honest for the other two as well.
            for (name, value) in &emitted {
                assert!(
                    value
                        .bytes()
                        .all(|byte| (0x20..0x7f).contains(&byte) || byte == b'\t'),
                    "{name} carries a value no HTTP header may hold: {value:?}"
                );
            }
        }
    }

    #[test]
    fn completeness_flattens_into_an_envelope() {
        // The `Serialize` impl documents itself as being for `#[serde(flatten)]`,
        // which routes through a different serializer than a standalone
        // `to_value`. Unit 14 flattens this into every operation's envelope, so
        // the promise is pinned where it is made rather than discovered there.
        #[derive(serde::Serialize)]
        struct Envelope {
            rows: Vec<u8>,
            #[serde(flatten)]
            completeness: Completeness,
        }

        let flattened = serde_json::to_value(Envelope {
            rows: vec![],
            completeness: Completeness::page_limit(token()),
        })
        .expect("flatten");

        assert_eq!(flattened["complete"], serde_json::json!(false));
        assert_eq!(
            flattened["truncation_reason"],
            serde_json::json!("page_limit")
        );
        assert_eq!(flattened["next"], serde_json::json!(token().as_str()));
        assert!(
            flattened.get("completeness").is_none(),
            "the fields must merge into the envelope, not nest under it"
        );
    }

    #[test]
    fn the_wire_form_is_the_one_doc_03_shows() {
        assert_eq!(
            serde_json::to_string(&Completeness::complete()).unwrap(),
            r#"{"complete":true,"next":null}"#
        );
        let token = token();
        assert_eq!(
            serde_json::to_string(&Completeness::page_limit(token.clone())).unwrap(),
            format!(r#"{{"complete":false,"truncation_reason":"page_limit","next":"{token}"}}"#)
        );
        assert_eq!(
            serde_json::to_string(&Completeness::cell_overflow()).unwrap(),
            r#"{"complete":false,"truncation_reason":"cell_overflow","next":null}"#
        );
    }

    #[test]
    fn a_reason_that_cannot_resume_cannot_be_given_a_cursor() {
        // The loop this prevents: a client that pages on `cell_overflow` asks
        // for the same cell again, gets the same overflow, and never stops.
        // `budget_exhausted` takes a `BudgetReason`, so `cell_overflow` cannot
        // reach it at all — there is nothing to assert at runtime. What is
        // worth pinning is that the subset stays a subset: every reason a
        // budget can produce must be one that resumes, or the constructor
        // would start handing out cursors that loop.
        for reason in [
            BudgetReason::Time,
            BudgetReason::Candidate,
            BudgetReason::ResponseBytes,
        ] {
            let reason = TruncationReason::from(reason);
            assert!(reason.resumes(), "{} must resume", reason.as_str());
            assert_ne!(
                reason,
                TruncationReason::PageLimit,
                "a page limit is not a budget"
            );
        }

        for reason in [
            TruncationReason::CellOverflow,
            TruncationReason::PartialFailure,
        ] {
            assert!(!reason.resumes(), "{} must not resume", reason.as_str());
        }

        // The other constructor for a budget: same reason on the wire, no
        // cursor, for an operation that has no position to resume from. It is
        // the alternative to a `/sample` that returns fewer members than were
        // asked for and calls itself complete.
        let short = Completeness::budget_exhausted_without_resume(BudgetReason::ResponseBytes);
        assert!(!short.is_complete());
        assert_eq!(
            short.truncation_reason(),
            Some(TruncationReason::ResponseBytes)
        );
        assert_eq!(short.next_cursor(), None);
    }

    #[test]
    fn a_cardinality_reports_how_well_it_is_known() {
        let json = |cardinality: Cardinality| serde_json::to_string(&cardinality).unwrap();

        assert_eq!(json(Cardinality::exact(17)), r#"{"value":17,"exact":true}"#);
        assert_eq!(
            json(Cardinality::estimated(17)),
            r#"{"value":17,"exact":false}"#
        );
        // §3.4.4's interrupted count: the estimate, plus what was actually
        // reached before the budget ran out.
        assert_eq!(
            json(Cardinality::estimated(40).at_least(31)),
            r#"{"value":40,"exact":false,"min":31}"#
        );
        // §3.4.1's range: `value` stays an estimate because summing occurrences
        // over the slice is O(slice), while the distinct-object count is exact
        // from two binary searches. One response, two qualities of knowledge.
        assert_eq!(
            json(Cardinality::estimated(900).over_distinct_objects(120)),
            r#"{"value":900,"exact":false,"distinct_objects":120}"#
        );
    }

    #[test]
    fn a_lower_bound_cannot_sit_above_the_estimate_it_bounds() {
        // The two numbers come from different computations — one counted, one
        // guessed — so a scan that reached 50 under an estimate of 10 has
        // disproved the estimate. Reporting the pair as given would say "at
        // least 50, about 10", which states nothing.
        let overshot = Cardinality::estimated(10).at_least(50);
        assert_eq!(overshot.value(), 50);
        assert_eq!(overshot.min(), Some(50));
        assert_eq!(
            serde_json::to_string(&overshot).unwrap(),
            r#"{"value":50,"exact":false,"min":50}"#
        );

        // An estimate the scan did not reach is left alone.
        let undershot = Cardinality::estimated(80).at_least(31);
        assert_eq!(undershot.value(), 80);
        assert_eq!(undershot.min(), Some(31));

        // An exact count has no scan to interrupt and is already its own bound.
        assert!(std::panic::catch_unwind(|| Cardinality::exact(17).at_least(5)).is_err());
    }

    #[test]
    fn a_problem_document_is_rfc_9457() {
        let problem = Problem::new(
            ErrorCode::CapExceeded,
            "limit 50000 exceeds the cap of 10000",
        )
        .about("/v/2026-06-01/fragment?limit=50000");
        assert_eq!(problem.status(), 400);
        assert_eq!(
            serde_json::to_value(&problem).unwrap(),
            serde_json::json!({
                // RFC 9457 §4.2.1: with `about:blank`, the title is the status
                // reason phrase. What went wrong lives in `code` and `detail`.
                "type": "about:blank",
                "title": "Bad Request",
                "status": 400,
                "detail": "limit 50000 exceeds the cap of 10000",
                "instance": "/v/2026-06-01/fragment?limit=50000",
                "code": "cap_exceeded",
            })
        );
    }

    #[test]
    fn every_code_has_its_normative_or_extended_wire_mapping() {
        // §3.6.1 is normative for the codes it names and permits additions for
        // uncovered conditions. This is its table plus this server's RFC 9110
        // precondition extension. An implementation that drifts from either
        // makes agents' self-correction wrong in a way no other test notices.
        let table = [
            (ErrorCode::BadTermSyntax, "bad_term_syntax", 400u16),
            (ErrorCode::MalformedRequest, "malformed_request", 400),
            (ErrorCode::CapExceeded, "cap_exceeded", 400),
            (ErrorCode::StaleCursor, "stale_cursor", 400),
            (ErrorCode::UnsupportedFormat, "unsupported_format", 400),
            (ErrorCode::NotFound, "not_found", 404),
            (ErrorCode::MethodNotAllowed, "method_not_allowed", 405),
            (ErrorCode::NotAcceptable, "not_acceptable", 406),
            (ErrorCode::PreconditionFailed, "precondition_failed", 412),
            (ErrorCode::PayloadTooLarge, "payload_too_large", 413),
            (
                ErrorCode::UnsupportedMediaType,
                "unsupported_media_type",
                415,
            ),
            (ErrorCode::RateLimited, "rate_limited", 429),
            (ErrorCode::InternalError, "internal_error", 500),
            (
                ErrorCode::CapabilityNotAvailable,
                "capability_not_available",
                501,
            ),
        ];

        // The transcription must cover the whole enum, or a code added without
        // a row here is a code no test compares against the spec.
        assert_eq!(
            table.map(|(code, _, _)| code).as_slice(),
            ErrorCode::ALL,
            "every variant must appear, with extensions in status order"
        );

        let mut seen = std::collections::HashSet::new();
        for (code, name, status) in table {
            assert_eq!(code.as_str(), name);
            assert_eq!(code.status(), status, "{name}");
            assert!(seen.insert(name), "{name} is duplicated");
            // RFC 9457 §4.2.1's rule for `about:blank`: the title is the
            // status's reason phrase, never the code or a KGF phrase.
            assert!(
                !code.title().is_empty() && code.title() != name,
                "{name}'s title must be the status reason phrase"
            );
        }
    }

    #[test]
    fn a_status_no_handler_raised_still_finds_a_code() {
        // The backstop for an error a `tower` layer answered before any of this
        // crate's code ran — the request-body limit's 413 today.
        assert_eq!(
            ErrorCode::for_unattributed_status(413),
            Some(ErrorCode::PayloadTooLarge)
        );
        assert_eq!(
            ErrorCode::for_unattributed_status(500),
            Some(ErrorCode::InternalError)
        );
        // 400 is shared by five codes, and picking one would tell a client the
        // wrong thing to fix, so it stays unattributed and is logged instead.
        assert_eq!(ErrorCode::for_unattributed_status(400), None);
        assert_eq!(ErrorCode::for_unattributed_status(200), None);
        assert_eq!(ErrorCode::for_unattributed_status(418), None);

        // Every other status in the table resolves to exactly its own code, or
        // the backstop would answer with a code meaning something else.
        for code in ErrorCode::ALL {
            let status = code.status();
            if status != 400 {
                assert_eq!(
                    ErrorCode::for_unattributed_status(status),
                    Some(*code),
                    "{}",
                    code.as_str()
                );
            }
        }
    }

    #[test]
    fn an_instance_a_handler_set_survives_the_renderer() {
        // The renderer fills `instance` from the request path for every
        // problem, and must not overwrite something narrower.
        let handler_set = Problem::new(ErrorCode::NotFound, "x")
            .about("/tox/v/2026-06-01/manifest")
            .about_unless_set("/tox");
        let json = serde_json::to_value(&handler_set).unwrap();
        assert_eq!(json["instance"], "/tox/v/2026-06-01/manifest");

        let filled_in = Problem::new(ErrorCode::NotFound, "x").about_unless_set("/tox");
        let json = serde_json::to_value(&filled_in).unwrap();
        assert_eq!(json["instance"], "/tox");
    }

    #[test]
    fn negotiation_failures_stay_three_separate_codes() {
        // One status per code only works if conditions needing different
        // statuses are different codes. These three read as one situation and
        // are three different client fixes: change `format=`, relax `Accept`,
        // or send another request media type.
        assert_eq!(ErrorCode::UnsupportedFormat.status(), 400);
        assert_eq!(ErrorCode::NotAcceptable.status(), 406);
        assert_eq!(ErrorCode::UnsupportedMediaType.status(), 415);
        assert_eq!(ErrorCode::NotAcceptable.title(), "Not Acceptable");
    }

    #[test]
    fn a_malformed_term_becomes_a_bad_term_syntax_problem() {
        // The conversion carries the term layer's message through as `detail`,
        // which is the whole reason those messages name the token and the fix.
        let error = crate::term::Term::parse("rdfs:label", &crate::term::PrefixMap::default())
            .expect_err("no prefix is declared");
        let problem = Problem::from(error);

        assert_eq!(problem.code(), ErrorCode::BadTermSyntax);
        assert_eq!(problem.status(), 400);
        let json = serde_json::to_value(&problem).unwrap();
        let detail = json["detail"].as_str().unwrap();
        assert!(
            detail.contains("rdfs:label") && detail.contains("<rdfs:label>"),
            "the detail must survive the conversion intact, got: {detail}"
        );
    }
}
