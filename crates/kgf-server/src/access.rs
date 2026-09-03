//! Structured access records for the request-shape census.
//!
//! The default record deliberately contains no request terms, search text,
//! path captures, bodies, response bodies, or problem details. Handlers add
//! only metadata derived from typed requests; the outer middleware owns the
//! raw HTTP facts and emits exactly one record per request — when the response
//! has been produced, or when the request was abandoned before that.

use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use axum::body::HttpBody as _;
use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::http::header::USER_AGENT;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use jiff::Timestamp;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::admission::WorkClass;
use crate::answer::Rendered;
use crate::envelope::ErrorCode;
use crate::representation::Representation;
use crate::request::ObservedRequest;
use crate::service::Service;

const REQUEST_ID: HeaderName = HeaderName::from_static("kgf-request-id");
const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const TEXT_LIMIT: usize = 200;
/// Records the writer thread may fall behind by before new ones are dropped.
const QUEUE_CAPACITY: usize = 4096;

/// One structured record emitted for one HTTP request.
///
/// Optional fields serialize as `null`. The raw-tier fields — `user_agent`,
/// `client_request_id`, `target`, and `q` — are omitted from the object rather
/// than written as `null`, and are only ever present when the operator
/// explicitly enabled raw logging, so a shape-tier line never carries a key
/// that could hold client content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccessRecord {
    /// Time the record was emitted, in UTC.
    pub time: String,
    /// How much attention the record deserves, from what the server did.
    pub severity: Severity,
    /// Server-minted request identifier, also returned in `KGF-Request-Id`.
    pub request_id: String,
    /// HTTP method.
    pub method: String,
    /// Axum's matched route template, or `null` when no route matched.
    pub route: Option<String>,
    /// KGF operation inferred from the matched route.
    pub operation: Option<AccessOperation>,
    /// Transport by which the typed operation was submitted.
    pub transport: Option<Transport>,
    /// Resolved dataset identifier; never an unresolved path capture.
    pub dataset: Option<String>,
    /// Resolved immutable release; never an unresolved path capture.
    pub version: Option<String>,
    /// Selected response representation.
    pub representation: Option<String>,
    /// HTTP status, or `null` when the client went away before a response
    /// was produced.
    pub status: Option<u16>,
    /// Machine-readable problem code.
    pub code: Option<&'static str>,
    /// Admission class for bundle work.
    pub work_class: Option<AccessWorkClass>,
    /// Time spent waiting for admission.
    pub queue_ms: Option<u64>,
    /// Time spent on the blocking worker, excluding its scheduling delay.
    pub work_ms: Option<u64>,
    /// Time from the middleware seeing the request to emitting this record.
    pub total_ms: u64,
    /// Requests in the admission waiting room when this one arrived.
    pub waiting: usize,
    /// Request-body bytes a handler read; `null` when no body was consumed.
    /// Never the declared `Content-Length`, which the client chooses.
    pub bytes_in: Option<u64>,
    /// Exact response-body size when the HTTP stack knows it.
    pub bytes_out: Option<u64>,
    /// Whether an operation response is complete.
    pub complete: Option<bool>,
    /// Why an operation response is incomplete.
    pub truncation_reason: Option<&'static str>,
    /// Result items materialized into the response.
    pub rows: Option<u64>,
    /// Reported result cardinality.
    pub cardinality: Option<u64>,
    /// Whether `cardinality` is exact.
    pub exact: Option<bool>,
    /// Whether the request resumed from a cursor.
    pub cursor: bool,
    /// Truncated hash of the canonical request, when that operation has one.
    pub request_hash: Option<String>,
    /// Time spent opening or finding the mapped bundle.
    pub open_ms: Option<u64>,
    /// Whether the bundle appeared unopened immediately before the lookup.
    pub first_open: Option<bool>,
    /// Pseudonymous direct peer identity, rotated on process restart; `null`
    /// when the listener supplies no peer address.
    pub client_hash: Option<String>,
    /// Pseudonymous client identity reported by the configured chain of
    /// trusted proxies; `null` when none are configured or the chain is
    /// shorter than configured.
    pub forwarded_hash: Option<String>,
    /// Coarse user-agent family.
    pub client_class: ClientClass,
    /// Raw-tier User-Agent truncated to 200 bytes at a UTF-8 boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Raw-tier inbound X-Request-Id, truncated rather than trusted as our identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_request_id: Option<String>,
    /// Content-free typed request shape.
    pub shape: Option<RequestShape>,
    /// Raw path and query, present only in the opt-in raw tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Raw search text, present only in the opt-in raw tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
}

/// How much attention a record deserves, derived from what the server did.
///
/// The conventional syslog and OpenTelemetry level names, so a log system that
/// routes on a `severity` field needs no mapping. A client error is `INFO`:
/// the server answered as designed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Severity {
    /// The server answered as designed, including with a client error.
    Info,
    /// The server refused work it would normally do: admission was full.
    Warning,
    /// The server failed to answer: a `5xx` status.
    Error,
}

impl Severity {
    fn for_status(status: Option<u16>) -> Self {
        match status {
            Some(500..=599) => Self::Error,
            Some(429) => Self::Warning,
            _ => Self::Info,
        }
    }
}

/// A destination for access records.
///
/// `record` runs on the request's async task, so an implementation must not
/// block: hand the record to a queue, a channel, or memory, never to a
/// synchronous write that can stall.
pub trait AccessLog: Send + Sync + std::fmt::Debug {
    /// Record one completed or abandoned request.
    fn record(&self, record: &AccessRecord);
}

/// A JSON-lines access log on standard output, written by its own thread.
///
/// `record` serializes the line and hands it to a bounded queue; it performs
/// no I/O on the caller's thread. Standard output can block — a container's
/// log pipe fills when its shipper stalls — and a blocking write on an async
/// worker would stall every request that worker serves. When the queue is
/// full the record is dropped and counted, and the writer reports the count on
/// stderr once it catches up: an access log that loses lines behind a stalled
/// shipper is preferable to a server that stops answering.
#[derive(Debug)]
pub struct StdoutAccessLog {
    lines: QueuedLines,
}

impl StdoutAccessLog {
    /// Start the writer thread over this process's standard output.
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            lines: QueuedLines::spawn(std::io::stdout(), QUEUE_CAPACITY)?,
        })
    }
}

impl AccessLog for StdoutAccessLog {
    fn record(&self, record: &AccessRecord) {
        match serde_json::to_string(record) {
            Ok(line) => self.lines.push(line),
            Err(error) => tracing::error!(%error, "could not serialize an access record"),
        }
    }
}

#[derive(Debug)]
enum Message {
    Line(String),
    Stop,
}

/// Lines handed to a dedicated writer thread through a bounded queue.
#[derive(Debug)]
struct QueuedLines {
    queue: SyncSender<Message>,
    /// Lines dropped since the writer last reported.
    dropped: Arc<AtomicU64>,
    writer: Option<JoinHandle<()>>,
}

impl QueuedLines {
    fn spawn(mut output: impl Write + Send + 'static, capacity: usize) -> std::io::Result<Self> {
        let (queue, lines) = std::sync::mpsc::sync_channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&dropped);
        let writer = std::thread::Builder::new()
            .name("kgf-access-log".to_owned())
            .spawn(move || {
                for message in lines {
                    let Message::Line(line) = message else {
                        return;
                    };
                    let written = output
                        .write_all(line.as_bytes())
                        .and_then(|()| output.write_all(b"\n"))
                        .and_then(|()| output.flush());
                    if let Err(error) = written {
                        tracing::error!(
                            %error,
                            "could not write the access log; further records are dropped"
                        );
                        return;
                    }
                    let dropped = counted.swap(0, Ordering::Relaxed);
                    if dropped > 0 {
                        tracing::warn!(
                            dropped,
                            "access records were dropped while the log queue was full"
                        );
                    }
                }
            })?;
        Ok(Self {
            queue,
            dropped,
            writer: Some(writer),
        })
    }

    /// Queue one line, or count it as dropped when the writer is behind or gone.
    fn push(&self, line: String) {
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            self.queue.try_send(Message::Line(line))
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for QueuedLines {
    fn drop(&mut self) {
        // Blocking is acceptable here: this is shutdown, and every record
        // already queued should reach the output before the process exits.
        // The send fails only if the writer has already gone, and the join then
        // returns at once.
        let _ = self.queue.send(Message::Stop);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

/// The finite operation vocabulary used by the census.
///
/// A record spells the operation as the serialized variant name. The URL
/// spelling is [`path_segment`](Self::path_segment), and the two are
/// deliberately separate methods: the record vocabulary may be revised as the
/// census learns what it needs, and the path segment may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccessOperation {
    /// Triple-pattern enumeration.
    Fragment,
    /// Cardinality lookup.
    Count,
    /// Resource neighborhood.
    Describe,
    /// Deterministic sample.
    Sample,
    /// Entity search.
    Search,
    /// Description graph navigation.
    Schema,
    /// VoID description.
    Void,
    /// Summary card.
    Summary,
    /// Preferred-label batch.
    Labels,
    /// Immutable bundle manifest.
    Manifest,
    /// Service descriptor.
    Service,
    /// Dataset descriptor.
    Dataset,
    /// Moving-version redirect.
    Latest,
}

impl AccessOperation {
    /// The operation's URL path segment.
    ///
    /// This spelling is the wire contract: it names the resource in strong
    /// validators and in the next-page links a response emits, so it is fixed
    /// for the life of every issued cursor. It is not the census spelling.
    pub fn path_segment(self) -> &'static str {
        match self {
            Self::Fragment => "fragment",
            Self::Count => "count",
            Self::Describe => "describe",
            Self::Sample => "sample",
            Self::Search => "search",
            Self::Schema => "schema",
            Self::Void => "void",
            Self::Summary => "summary",
            Self::Labels => "labels",
            Self::Manifest => "manifest",
            Self::Service => "service",
            Self::Dataset => "dataset",
            Self::Latest => "latest",
        }
    }
}

/// How a request reached an operation handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    /// Ordinary GET semantics, including HEAD.
    Get,
    /// brTPF's query-carried `values=` transport.
    GetValues,
    /// RFC 10008 QUERY with a body.
    Query,
    /// POST compatibility transport.
    Post,
}

/// The admission class assigned to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessWorkClass {
    /// Ordinary bounded index work.
    Ordinary,
    /// Candidate-heavy, random-I/O, or bulk work.
    Heavy,
}

impl From<WorkClass> for AccessWorkClass {
    fn from(value: WorkClass) -> Self {
        match value {
            WorkClass::Ordinary => Self::Ordinary,
            WorkClass::Heavy => Self::Heavy,
        }
    }
}

/// Coarse client family inferred from User-Agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientClass {
    /// Comunica.
    Comunica,
    /// A conventional browser User-Agent.
    Browser,
    /// curl.
    Curl,
    /// A common Python HTTP client.
    Python,
    /// A Node.js HTTP client.
    Node,
    /// A KGF client.
    Kgf,
    /// No recognised family.
    Unknown,
}

/// Content-free structure derived from a successfully parsed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum RequestShape {
    /// Triple pattern or count request.
    Pattern {
        /// `?`, `i`, `l`, or `b` for subject, predicate, and object.
        pattern: String,
        /// Whether `o.text` constrains the object.
        text: bool,
        /// Requested page size; `null` for `/count`.
        limit: Option<u32>,
    },
    /// Bindings-restricted fragment or count request.
    Bindings {
        /// Shape of the body pattern.
        pattern: String,
        /// Bindings requests do not carry `o.text`.
        text: bool,
        /// Requested page size; `null` for bindings count.
        limit: Option<u32>,
        /// Submitted binding rows.
        k: u64,
        /// Submitted binding columns.
        columns: u64,
    },
    /// Resource description request.
    Describe {
        /// `i`, `l`, or `b` for the selected term.
        term: char,
        /// Edge direction.
        direction: &'static str,
        /// Requested page size.
        limit: u32,
    },
    /// Sampling request.
    Sample {
        /// Triple-pattern shape.
        pattern: String,
        /// Requested sample size.
        n: u32,
    },
    /// Entity-search request.
    Search {
        /// Search-string size in bytes.
        q_len: u64,
        /// Server-declared role names selected by the request.
        roles: Vec<String>,
        /// Number of selected predicates after role expansion.
        predicates: u64,
        /// Requested result limit.
        limit: u32,
        /// Whether preferred labels were requested.
        labels: bool,
    },
    /// Preferred-label batch.
    Labels {
        /// Submitted IRIs.
        k: u64,
    },
    /// Description-graph request.
    Schema {
        /// Semantic selector kind.
        selection: &'static str,
        /// Requested child collection.
        children: Option<&'static str>,
        /// Requested flat projection.
        projection: Option<&'static str>,
        /// Description view.
        view: &'static str,
        /// Requested item limit, if this shape pages items.
        limit: Option<u32>,
    },
    /// An operation with no request-shape fields.
    Empty {},
}

/// What the access log records.
///
/// `Off` when no sink is configured: handlers then skip the work of describing
/// a request nothing will read. `Raw` is `Shape` plus client-supplied content
/// — the request target, search text, User-Agent, and inbound request id.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Tier {
    #[default]
    Off,
    Shape,
    Raw,
}

/// Handler-owned fields carried outward on a response extension.
///
/// Every setter is a no-op when the tier is [`Tier::Off`], so a handler
/// describes its request unconditionally and pays nothing when no sink is
/// configured. Client content — the raw search string — is captured only in
/// [`Tier::Raw`]; below that tier it is never copied out of the request.
#[derive(Debug, Clone, Default)]
pub(crate) struct Observation {
    tier: Tier,
    pub(crate) operation: Option<AccessOperation>,
    pub(crate) transport: Option<Transport>,
    pub(crate) dataset: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) representation: Option<Representation>,
    pub(crate) work_class: Option<WorkClass>,
    pub(crate) queue_ms: Option<u64>,
    pub(crate) work_ms: Option<u64>,
    pub(crate) bytes_in: Option<u64>,
    pub(crate) complete: Option<bool>,
    pub(crate) truncation_reason: Option<&'static str>,
    pub(crate) rows: Option<u64>,
    pub(crate) cardinality: Option<u64>,
    pub(crate) exact: Option<bool>,
    pub(crate) cursor: bool,
    pub(crate) request_hash: Option<[u8; 8]>,
    pub(crate) open_ms: Option<u64>,
    pub(crate) first_open: Option<bool>,
    pub(crate) shape: Option<RequestShape>,
    pub(crate) q: Option<String>,
}

impl Observation {
    /// An observation of `operation` at the deployment's recording tier.
    pub(crate) fn new(access: &AccessState, operation: AccessOperation) -> Self {
        Self {
            tier: access.tier,
            operation: Some(operation),
            ..Self::default()
        }
    }

    /// An observation of an operation handler with its transport and the
    /// representation it negotiated.
    pub(crate) fn operation(
        access: &AccessState,
        operation: AccessOperation,
        transport: Transport,
        representation: Representation,
    ) -> Self {
        Self {
            transport: Some(transport),
            representation: Some(representation),
            ..Self::new(access, operation)
        }
    }

    /// Whether a sink will read this observation.
    pub(crate) fn recording(&self) -> bool {
        self.tier != Tier::Off
    }

    /// Name the release the service resolved; never an unresolved capture.
    pub(crate) fn resolved(mut self, dataset: &str, version: Option<&str>) -> Self {
        if self.recording() {
            self.dataset = Some(dataset.to_owned());
            self.version = version.map(str::to_owned);
        }
        self
    }

    /// Record that this operation has no request-shape fields.
    pub(crate) fn empty_shape(mut self) -> Self {
        if self.recording() {
            self.shape = Some(RequestShape::Empty {});
        }
        self
    }

    /// Describe a successfully parsed request and the work class it earned.
    pub(crate) fn request(&mut self, request: &impl ObservedRequest, work_class: WorkClass) {
        if !self.recording() {
            return;
        }
        self.work_class = Some(work_class);
        self.shape = Some(request.shape());
        self.cursor = request.resumed();
        self.request_hash = request.request_hash();
        if self.tier == Tier::Raw {
            self.q = request.raw_query().map(str::to_owned);
        }
    }

    pub(crate) fn rendered(&mut self, rendered: &Rendered) {
        if !self.recording() {
            return;
        }
        self.complete = Some(rendered.completeness.is_complete());
        self.truncation_reason = rendered
            .completeness
            .truncation_reason()
            .map(|reason| reason.as_str());
        self.rows = rendered.rows;
        if let Some(cardinality) = rendered.cardinality {
            self.cardinality = Some(cardinality.value());
            self.exact = Some(cardinality.is_exact());
        }
    }
}

/// Admission and worker timing returned even when work fails.
#[derive(Debug)]
pub(crate) struct Timed<T> {
    pub(crate) result: Result<T, crate::envelope::Problem>,
    pub(crate) queue_ms: u64,
    pub(crate) work_ms: Option<u64>,
}

/// Bundle-open timing attached to a successful lookup.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OpenTiming {
    pub(crate) open_ms: u64,
    pub(crate) first_open: bool,
}

/// Per-process identity and destination shared by the middleware.
pub(crate) struct AccessState {
    sink: Option<Arc<dyn AccessLog>>,
    /// `Off` exactly when `sink` is `None`.
    tier: Tier,
    trusted_proxies: u8,
    /// Salt for pseudonymous client identities, random per process.
    salt: [u8; 32],
    /// Prefix of every request id this process mints, random per process and
    /// independent of the salt: it is published on every response.
    nonce: [u8; 8],
    counter: AtomicU64,
}

impl std::fmt::Debug for AccessState {
    // Hand-written so the salt is never printed: a debug dump of the service
    // must not be the thing that unmasks the pseudonyms.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccessState")
            .field("sink", &self.sink)
            .field("tier", &self.tier)
            .field("trusted_proxies", &self.trusted_proxies)
            .finish_non_exhaustive()
    }
}

impl AccessState {
    pub(crate) fn new(
        sink: Option<Arc<dyn AccessLog>>,
        raw: bool,
        trusted_proxies: u8,
    ) -> Result<Self, getrandom::Error> {
        let tier = match (&sink, raw) {
            (None, _) => Tier::Off,
            (Some(_), false) => Tier::Shape,
            (Some(_), true) => Tier::Raw,
        };
        let mut salt = [0u8; 32];
        getrandom::fill(&mut salt)?;
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce)?;
        Ok(Self {
            sink,
            tier,
            trusted_proxies,
            salt,
            nonce,
            counter: AtomicU64::new(0),
        })
    }

    fn request_id(&self) -> String {
        let sequence = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{}-{sequence:08x}", hex(self.nonce))
    }

    /// `SHA-256(salt ‖ value)`, truncated to 64 bits: one-way, stable within
    /// this process, and unlinkable across restarts.
    fn pseudonym(&self, value: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        hasher.update(value.as_bytes());
        let digest = hasher.finalize();
        let prefix: [u8; 8] = digest[..8]
            .try_into()
            .expect("a SHA-256 digest has at least eight bytes");
        hex(prefix)
    }
}

/// Record one request outside every response-producing router layer.
pub(crate) async fn record_request(
    State(service): State<Arc<Service>>,
    request: Request,
    next: Next,
) -> Response {
    let in_flight = InFlight::begin(service.access(), service.admission().waiting(), &request);
    let response = next.run(request).await;
    in_flight.finish(response)
}

/// One request the middleware has seen and not yet recorded.
///
/// Emits exactly once: from [`finish`](Self::finish) with the response, or
/// from `Drop` when the request future is cancelled before that — the client
/// disconnected while the server was still working — so an abandoned request
/// appears in the census with no status rather than not at all. The work it
/// admitted still runs to completion, which is why it must be counted.
struct InFlight {
    request_id: String,
    recording: Option<(Arc<dyn AccessLog>, Pending)>,
}

/// The facts a record needs that are known before the handler runs.
struct Pending {
    started: Instant,
    method: String,
    route: Option<String>,
    target: Option<String>,
    user_agent: Option<String>,
    client_request_id: Option<String>,
    client_class: ClientClass,
    client_hash: Option<String>,
    forwarded_hash: Option<String>,
    waiting: usize,
}

/// What a produced response contributes to its record.
struct Outcome {
    status: u16,
    code: Option<&'static str>,
    representation: Option<Representation>,
    bytes_out: Option<u64>,
    observation: Observation,
}

impl InFlight {
    fn begin(access: &AccessState, waiting: usize, request: &Request) -> Self {
        let request_id = access.request_id();
        let Some(sink) = access.sink.clone() else {
            return Self {
                request_id,
                recording: None,
            };
        };
        let raw = access.tier == Tier::Raw;
        let headers = request.headers();
        let user_agent_header = header_text(headers, &USER_AGENT);
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|connected| connected.0.ip().to_string());
        let pending = Pending {
            started: Instant::now(),
            method: request.method().as_str().to_owned(),
            route: request
                .extensions()
                .get::<MatchedPath>()
                .map(|path| path.as_str().to_owned()),
            target: raw.then(|| {
                request
                    .uri()
                    .path_and_query()
                    .map_or_else(|| request.uri().path().to_owned(), ToString::to_string)
            }),
            user_agent: raw.then(|| user_agent_header.map(truncate)).flatten(),
            client_request_id: raw
                .then(|| header_text(headers, &X_REQUEST_ID).map(truncate))
                .flatten(),
            client_class: classify(user_agent_header),
            client_hash: peer.as_deref().map(|peer| access.pseudonym(peer)),
            forwarded_hash: forwarded_client(headers, access.trusted_proxies)
                .map(|client| access.pseudonym(client)),
            waiting,
        };
        Self {
            request_id,
            recording: Some((sink, pending)),
        }
    }

    fn finish(mut self, mut response: Response) -> Response {
        response.headers_mut().insert(
            REQUEST_ID,
            HeaderValue::from_str(&self.request_id)
                .expect("server request ids are valid header values"),
        );
        if let Some((sink, pending)) = self.recording.take() {
            let outcome = Outcome::of(&mut response);
            let request_id = std::mem::take(&mut self.request_id);
            sink.record(&pending.record(request_id, Some(outcome)));
        }
        response
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some((sink, pending)) = self.recording.take() {
            let request_id = std::mem::take(&mut self.request_id);
            sink.record(&pending.record(request_id, None));
        }
    }
}

impl Outcome {
    fn of(response: &mut Response) -> Self {
        Self {
            status: response.status().as_u16(),
            code: response
                .extensions()
                .get::<ErrorCode>()
                .copied()
                .map(ErrorCode::as_str),
            representation: response.extensions().get::<Representation>().copied(),
            bytes_out: response.body().size_hint().exact(),
            observation: response
                .extensions_mut()
                .remove::<Observation>()
                .unwrap_or_default(),
        }
    }
}

impl Pending {
    /// The record for a response, or for a request abandoned without one.
    fn record(self, request_id: String, outcome: Option<Outcome>) -> AccessRecord {
        let (status, code, problem_representation, bytes_out, observation) = match outcome {
            Some(outcome) => (
                Some(outcome.status),
                outcome.code,
                outcome.representation,
                outcome.bytes_out,
                outcome.observation,
            ),
            None => (None, None, None, None, Observation::default()),
        };
        let operation = observation
            .operation
            .or_else(|| self.route.as_deref().and_then(operation_for_route));
        // A rendered problem chose its own representation, which is the one
        // the client received whatever the handler had negotiated.
        let representation = problem_representation
            .or(observation.representation)
            .map(|value| value.token().to_owned());
        AccessRecord {
            time: Timestamp::now().to_string(),
            severity: Severity::for_status(status),
            request_id,
            method: self.method,
            route: self.route,
            operation,
            transport: observation.transport,
            dataset: observation.dataset,
            version: observation.version,
            representation,
            status,
            code,
            work_class: observation.work_class.map(AccessWorkClass::from),
            queue_ms: observation.queue_ms,
            work_ms: observation.work_ms,
            total_ms: millis(self.started.elapsed()),
            waiting: self.waiting,
            bytes_in: observation.bytes_in,
            bytes_out,
            complete: observation.complete,
            truncation_reason: observation.truncation_reason,
            rows: observation.rows,
            cardinality: observation.cardinality,
            exact: observation.exact,
            cursor: observation.cursor,
            request_hash: observation.request_hash.map(hex),
            open_ms: observation.open_ms,
            first_open: observation.first_open,
            client_hash: self.client_hash,
            forwarded_hash: self.forwarded_hash,
            client_class: self.client_class,
            user_agent: self.user_agent,
            client_request_id: self.client_request_id,
            shape: observation.shape,
            target: self.target,
            q: observation.q,
        }
    }
}

pub(crate) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn header_text<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// The client address a chain of `trusted_proxies` forwarding hops reports.
///
/// Each trusted proxy appends the address it received the request from, so
/// the client is the `trusted_proxies`-th entry from the end of the combined
/// list. Everything before that entry was written by whoever sent the request
/// to the first trusted proxy and is not trusted; with no trusted proxies the
/// peer is the client and the header is ignored entirely, so a caller cannot
/// choose its own identity. A list shorter than the chain means a proxy did
/// not append, and nothing in it is trusted either.
fn forwarded_client(headers: &HeaderMap, trusted_proxies: u8) -> Option<&str> {
    let hops = usize::from(trusted_proxies);
    if hops == 0 {
        return None;
    }
    let entries: Vec<&str> = headers
        .get_all(&X_FORWARDED_FOR)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .collect();
    let index = entries.len().checked_sub(hops)?;
    Some(entries[index]).filter(|entry| !entry.is_empty())
}

fn truncate(value: &str) -> String {
    if value.len() <= TEXT_LIMIT {
        return value.to_owned();
    }
    let mut end = TEXT_LIMIT;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn classify(user_agent: Option<&str>) -> ClientClass {
    let Some(user_agent) = user_agent else {
        return ClientClass::Unknown;
    };
    let lower = user_agent.to_ascii_lowercase();
    // Most specific family first: a KGF client may name the runtime it runs
    // on, and the runtime's name must not claim the request.
    if lower.contains("kgf-client") || lower.contains("kgfq") {
        ClientClass::Kgf
    } else if lower.contains("comunica") {
        ClientClass::Comunica
    } else if lower.starts_with("mozilla/") {
        ClientClass::Browser
    } else if lower.contains("curl") {
        ClientClass::Curl
    } else if ["python-requests", "httpx", "aiohttp", "urllib"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        ClientClass::Python
    } else if lower.contains("node") || lower.contains("undici") {
        ClientClass::Node
    } else {
        ClientClass::Unknown
    }
}

fn operation_for_route(route: &str) -> Option<AccessOperation> {
    match route {
        "/" => Some(AccessOperation::Service),
        "/{dataset}" => Some(AccessOperation::Dataset),
        "/{dataset}/latest/{*rest}" => Some(AccessOperation::Latest),
        "/{dataset}/v/{version}/manifest" => Some(AccessOperation::Manifest),
        "/{dataset}/v/{version}/fragment" => Some(AccessOperation::Fragment),
        "/{dataset}/v/{version}/count" => Some(AccessOperation::Count),
        "/{dataset}/v/{version}/describe" => Some(AccessOperation::Describe),
        "/{dataset}/v/{version}/sample" => Some(AccessOperation::Sample),
        "/{dataset}/v/{version}/search" => Some(AccessOperation::Search),
        "/{dataset}/v/{version}/schema" => Some(AccessOperation::Schema),
        "/{dataset}/v/{version}/void" => Some(AccessOperation::Void),
        "/{dataset}/v/{version}/summary" => Some(AccessOperation::Summary),
        "/{dataset}/v/{version}/labels" => Some(AccessOperation::Labels),
        _ => None,
    }
}

fn hex(bytes: [u8; 8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(16);
    for byte in bytes {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
    }
    text
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc;

    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;

    #[derive(Debug, Default)]
    struct Recording(Mutex<Vec<AccessRecord>>);

    impl AccessLog for Recording {
        fn record(&self, record: &AccessRecord) {
            self.0.lock().unwrap().push(record.clone());
        }
    }

    fn headers(lines: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for line in lines {
            headers.append(X_FORWARDED_FOR, HeaderValue::from_str(line).unwrap());
        }
        headers
    }

    #[test]
    fn user_agents_are_classified_most_specific_first_and_case_insensitively() {
        assert_eq!(classify(Some("Comunica/5.3.0")), ClientClass::Comunica);
        assert_eq!(classify(Some("Mozilla/5.0")), ClientClass::Browser);
        assert_eq!(classify(Some("mozilla/5.0")), ClientClass::Browser);
        assert_eq!(classify(Some("curl/8.0")), ClientClass::Curl);
        assert_eq!(classify(Some("python-requests/2")), ClientClass::Python);
        assert_eq!(classify(Some("undici")), ClientClass::Node);
        assert_eq!(classify(Some("kgf-client/0.1")), ClientClass::Kgf);
        assert_eq!(classify(Some("kgf-client/0.3 (node 22)")), ClientClass::Kgf);
        assert_eq!(classify(None), ClientClass::Unknown);
    }

    #[test]
    fn truncation_preserves_utf8_and_the_byte_bound() {
        let value = format!("{}é", "a".repeat(199));
        let truncated = truncate(&value);
        assert_eq!(truncated.len(), 199);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn a_rendered_rate_limit_maps_to_its_access_code() {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        response.extensions_mut().insert(ErrorCode::RateLimited);

        let outcome = Outcome::of(&mut response);
        assert_eq!(outcome.code, Some("rate_limited"));
    }

    #[test]
    fn severity_follows_what_the_server_did() {
        assert_eq!(Severity::for_status(Some(200)), Severity::Info);
        assert_eq!(Severity::for_status(Some(404)), Severity::Info);
        assert_eq!(Severity::for_status(Some(429)), Severity::Warning);
        assert_eq!(Severity::for_status(Some(500)), Severity::Error);
        assert_eq!(Severity::for_status(Some(503)), Severity::Error);
        assert_eq!(Severity::for_status(None), Severity::Info);
    }

    #[test]
    fn the_forwarded_client_is_counted_from_the_trusted_end_of_the_chain() {
        let spoofed = headers(&["10.0.0.1, 192.0.2.8, 192.0.2.9"]);
        assert_eq!(forwarded_client(&spoofed, 0), None);
        assert_eq!(forwarded_client(&spoofed, 1), Some("192.0.2.9"));
        assert_eq!(forwarded_client(&spoofed, 2), Some("192.0.2.8"));
        assert_eq!(forwarded_client(&spoofed, 3), Some("10.0.0.1"));
        assert_eq!(forwarded_client(&spoofed, 4), None);

        let split = headers(&["10.0.0.1", "192.0.2.8 , 192.0.2.9"]);
        assert_eq!(forwarded_client(&split, 2), Some("192.0.2.8"));

        assert_eq!(forwarded_client(&headers(&[]), 1), None);
        assert_eq!(forwarded_client(&headers(&["192.0.2.8,"]), 1), None);
    }

    #[test]
    fn pseudonyms_are_stable_within_a_process_and_differ_between_salts() {
        let one = AccessState::new(None, false, 0).unwrap();
        let two = AccessState::new(None, false, 0).unwrap();
        assert_eq!(one.pseudonym("192.0.2.8"), one.pseudonym("192.0.2.8"));
        assert_ne!(one.pseudonym("192.0.2.8"), one.pseudonym("192.0.2.9"));
        assert_ne!(one.pseudonym("192.0.2.8"), two.pseudonym("192.0.2.8"));
        assert_eq!(one.pseudonym("192.0.2.8").len(), 16);
        assert_ne!(one.request_id(), two.request_id());
    }

    #[test]
    fn an_abandoned_request_is_recorded_without_a_status() {
        let sink = Arc::new(Recording::default());
        let access = AccessState::new(Some(sink.clone()), false, 0).unwrap();
        let request = Request::builder()
            .method("GET")
            .uri("/tox/v/v1/fragment?s=secret")
            .header(USER_AGENT, "curl/8.0")
            .body(Body::empty())
            .unwrap();

        let in_flight = InFlight::begin(&access, 3, &request);
        drop(in_flight);

        let records = sink.0.lock().unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.status, None);
        assert_eq!(record.severity, Severity::Info);
        assert_eq!(record.method, "GET");
        assert_eq!(record.waiting, 3);
        assert_eq!(record.client_class, ClientClass::Curl);
        assert_eq!(record.client_hash, None);
        assert_eq!(record.target, None);
        assert!(!serde_json::to_string(record).unwrap().contains("secret"));
    }

    #[test]
    fn a_finished_request_is_recorded_once() {
        let sink = Arc::new(Recording::default());
        let access = AccessState::new(Some(sink.clone()), false, 0).unwrap();
        let request = Request::builder().uri("/").body(Body::empty()).unwrap();

        let in_flight = InFlight::begin(&access, 0, &request);
        let response = in_flight.finish(Response::new(Body::from("{}")));

        assert!(response.headers().contains_key(&REQUEST_ID));
        let records = sink.0.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, Some(200));
        assert_eq!(records[0].bytes_out, Some(2));
        assert_eq!(
            response.headers()[&REQUEST_ID].to_str().unwrap(),
            records[0].request_id
        );
    }

    #[test]
    fn without_a_sink_only_the_request_id_is_produced() {
        let access = AccessState::new(None, true, 2).unwrap();
        let request = Request::builder().uri("/").body(Body::empty()).unwrap();
        let in_flight = InFlight::begin(&access, 0, &request);
        assert!(in_flight.recording.is_none());
        let response = in_flight.finish(Response::new(Body::empty()));
        assert!(response.headers().contains_key(&REQUEST_ID));
    }

    /// A writer that blocks on its first write until released, then appends to
    /// a shared buffer.
    struct Gated {
        entered: mpsc::Sender<()>,
        release: Option<mpsc::Receiver<()>>,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for Gated {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Some(release) = self.release.take() {
                self.entered.send(()).unwrap();
                release.recv().unwrap();
            }
            self.written.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_full_queue_drops_records_instead_of_blocking_and_drains_on_drop() {
        let (entered, entered_signal) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let lines = QueuedLines::spawn(
            Gated {
                entered,
                release: Some(released),
                written: Arc::clone(&written),
            },
            2,
        )
        .unwrap();

        lines.push("first".to_owned());
        // The writer is now blocked inside its first write, so the queue is
        // empty and holds exactly two more lines before dropping.
        entered_signal.recv().unwrap();
        lines.push("second".to_owned());
        lines.push("third".to_owned());
        lines.push("fourth".to_owned());
        assert_eq!(lines.dropped.load(Ordering::Relaxed), 1);

        release.send(()).unwrap();
        drop(lines);
        assert_eq!(
            String::from_utf8(written.lock().unwrap().clone()).unwrap(),
            "first\nsecond\nthird\n"
        );
    }
}
