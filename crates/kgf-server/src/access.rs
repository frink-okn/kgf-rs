//! Structured access records for the request-shape census.
//!
//! The default record deliberately contains no request terms, search text,
//! path captures, bodies, response bodies, or problem details. Handlers add
//! only metadata derived from typed requests; the outer middleware owns the
//! raw HTTP facts and emits exactly once after the response is complete.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash};
use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::HttpBody as _;
use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::http::header::{CONTENT_LENGTH, USER_AGENT};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use jiff::Timestamp;
use serde::Serialize;

use crate::admission::WorkClass;
use crate::answer::Rendered;
use crate::envelope::ErrorCode;
use crate::representation::Representation;
use crate::service::Service;

const REQUEST_ID: HeaderName = HeaderName::from_static("kgf-request-id");
const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const TEXT_LIMIT: usize = 200;

/// One structured record emitted for one HTTP response.
///
/// Optional fields serialize as `null`, except `target` and `q`: those raw-tier
/// fields are absent unless the operator explicitly enabled raw logging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccessRecord {
    /// Response time in UTC.
    pub time: String,
    /// Cloud Logging-compatible severity.
    pub severity: &'static str,
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
    /// HTTP status.
    pub status: u16,
    /// Machine-readable problem code.
    pub code: Option<&'static str>,
    /// Admission class for bundle work.
    pub work_class: Option<AccessWorkClass>,
    /// Time spent waiting for admission.
    pub queue_ms: Option<u64>,
    /// Time spent on the blocking worker, excluding its scheduling delay.
    pub work_ms: Option<u64>,
    /// End-to-end middleware time.
    pub total_ms: u64,
    /// Waiting-room permits available when the request entered.
    pub queued: usize,
    /// Exact request-body size when the HTTP stack knows it.
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
    /// Pseudonymous direct peer identity, rotated on process restart.
    pub client_hash: String,
    /// Pseudonymous first forwarded hop, when supplied.
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

/// A destination for access records.
pub trait AccessLog: Send + Sync {
    /// Record one completed response.
    fn record(&self, record: &AccessRecord);
}

/// A JSON-lines access log on standard output.
#[derive(Debug, Default)]
pub struct StdoutAccessLog;

impl AccessLog for StdoutAccessLog {
    fn record(&self, record: &AccessRecord) {
        let stdout = std::io::stdout();
        let mut output = stdout.lock();
        if let Err(error) = serde_json::to_writer(&mut output, record)
            .and_then(|()| output.write_all(b"\n").map_err(serde_json::Error::io))
        {
            tracing::error!(%error, "could not write an access record");
        }
    }
}

/// The finite operation vocabulary used by the census.
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
    /// Stable operation token used by response targets and records.
    pub fn as_str(self) -> &'static str {
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

/// Handler-owned fields carried outward on a response extension.
#[derive(Debug, Clone, Default)]
pub(crate) struct Observation {
    pub(crate) operation: Option<AccessOperation>,
    pub(crate) transport: Option<Transport>,
    pub(crate) dataset: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) representation: Option<Representation>,
    pub(crate) work_class: Option<WorkClass>,
    pub(crate) queue_ms: Option<u64>,
    pub(crate) work_ms: Option<u64>,
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
    pub(crate) fn operation(
        operation: AccessOperation,
        transport: Transport,
        representation: Representation,
    ) -> Self {
        Self {
            operation: Some(operation),
            transport: Some(transport),
            representation: Some(representation),
            ..Self::default()
        }
    }

    pub(crate) fn resolved(mut self, dataset: &str, version: Option<&str>) -> Self {
        self.dataset = Some(dataset.to_owned());
        self.version = version.map(str::to_owned);
        self
    }

    pub(crate) fn rendered(&mut self, rendered: &Rendered) {
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
    raw: bool,
    hashes: RandomState,
    nonce: u64,
    counter: AtomicU64,
}

impl std::fmt::Debug for AccessState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccessState")
            .field("sink", &self.sink.as_ref().map(|_| "configured"))
            .field("raw", &self.raw)
            .finish_non_exhaustive()
    }
}

impl AccessState {
    pub(crate) fn new(sink: Option<Arc<dyn AccessLog>>, raw: bool) -> Self {
        let hashes = RandomState::new();
        let nonce = hashes.hash_one("kgf-request-id");
        Self {
            sink,
            raw,
            hashes,
            nonce,
            counter: AtomicU64::new(0),
        }
    }

    fn request_id(&self) -> String {
        let sequence = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{:016x}-{sequence:08x}", self.nonce)
    }

    fn hash(&self, value: impl Hash) -> String {
        format!("{:016x}", self.hashes.hash_one(value))
    }
}

/// Record one request outside every response-producing router layer.
pub(crate) async fn record_request(
    State(service): State<Arc<Service>>,
    mut request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let access = service.access();
    let request_id = access.request_id();
    let method = request.method().as_str().to_owned();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned());
    let target = access.raw.then(|| {
        request
            .uri()
            .path_and_query()
            .map_or_else(|| request.uri().path().to_owned(), ToString::to_string)
    });
    let bytes_in = content_length(request.headers()).or_else(|| request.body().size_hint().exact());
    let user_agent_header = header_text(request.headers(), &USER_AGENT);
    let client_class = classify(user_agent_header);
    let user_agent = access
        .raw
        .then(|| user_agent_header.map(truncate))
        .flatten();
    let client_request_id = access
        .raw
        .then(|| header_text(request.headers(), &X_REQUEST_ID).map(truncate))
        .flatten();
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connected| connected.0.ip().to_string())
        .unwrap_or_default();
    let forwarded = header_text(request.headers(), &X_FORWARDED_FOR)
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let client_hash = access.hash(&peer);
    let forwarded_hash = forwarded.map(|value| access.hash(value));
    let queued = service.admission().queued_available();

    request.extensions_mut().insert(request_id.clone());
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        REQUEST_ID,
        HeaderValue::from_str(&request_id).expect("server request ids are valid header values"),
    );

    let bytes_out = response.body().size_hint().exact();
    let status = response.status().as_u16();
    let code = response_code(&response);
    let problem_representation = response.extensions().get::<Representation>().copied();
    let observation = response
        .extensions_mut()
        .remove::<Observation>()
        .unwrap_or_default();
    let operation = observation
        .operation
        .or_else(|| route.as_deref().and_then(operation_for_route));
    let representation = problem_representation
        .or(observation.representation)
        .map(|value| value.token().to_owned());
    let request_hash = observation.request_hash.map(hex_hash);
    let q = access.raw.then_some(observation.q).flatten();

    let record = AccessRecord {
        time: Timestamp::now().to_string(),
        severity: "INFO",
        request_id,
        method,
        route,
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
        total_ms: millis(started.elapsed()),
        queued,
        bytes_in,
        bytes_out,
        complete: observation.complete,
        truncation_reason: observation.truncation_reason,
        rows: observation.rows,
        cardinality: observation.cardinality,
        exact: observation.exact,
        cursor: observation.cursor,
        request_hash,
        open_ms: observation.open_ms,
        first_open: observation.first_open,
        client_hash,
        forwarded_hash,
        client_class,
        user_agent,
        client_request_id,
        shape: observation.shape,
        target,
        q,
    };
    if let Some(sink) = &access.sink {
        sink.record(&record);
    }
    response
}

pub(crate) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn response_code(response: &Response) -> Option<&'static str> {
    response
        .extensions()
        .get::<ErrorCode>()
        .copied()
        .map(ErrorCode::as_str)
}

fn header_text<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
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
    if lower.contains("comunica") {
        ClientClass::Comunica
    } else if user_agent.starts_with("Mozilla/") {
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
    } else if lower.contains("kgf-client") || lower.contains("kgfq") {
        ClientClass::Kgf
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

fn hex_hash(bytes: [u8; 8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(16);
    for byte in bytes {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;

    #[test]
    fn user_agents_are_classified_without_case_sensitive_accidents() {
        assert_eq!(classify(Some("Comunica/5.3.0")), ClientClass::Comunica);
        assert_eq!(classify(Some("Mozilla/5.0")), ClientClass::Browser);
        assert_eq!(classify(Some("curl/8.0")), ClientClass::Curl);
        assert_eq!(classify(Some("python-requests/2")), ClientClass::Python);
        assert_eq!(classify(Some("undici")), ClientClass::Node);
        assert_eq!(classify(Some("kgf-client/0.1")), ClientClass::Kgf);
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

        assert_eq!(response_code(&response), Some("rate_limited"));
    }
}
