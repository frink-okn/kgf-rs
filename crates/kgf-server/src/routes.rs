//! Doc 03 §3.2's URL space, mounted.
//!
//! The thin adapter: every rule it applies is decided in a module that does not
//! know what HTTP is — [`crate::service`] resolves a URL to a release,
//! [`crate::representation`] chooses a serialization and a cache policy,
//! [`crate::html`] and [`crate::descriptor`] render. What lives here is the
//! wiring, and the four things that are genuinely about the protocol: method
//! dispatch, the redirect, conditional requests, and the blocking boundary.
//!
//! # Method dispatch, and why QUERY is settled now
//!
//! M1 has no body-carrying route — bindings QUERY is M2 — so the temptation is
//! to route with a filter over the standard methods and deal with RFC 10008
//! later. That is the trap doc 03 §3.1 sets up: a stack that cannot express an
//! extension method is a stack that has to be replaced when M2 arrives.
//!
//! So it is proven now, on the routes that exist. Each is mounted with a
//! [`MethodRouter`] whose fallback answers `method_not_allowed`, and a `QUERY`
//! reaches that fallback — through hyper's parser, through axum's router, with
//! the method intact and an `Allow` header naming what the route does take. The
//! same fallback is where a QUERY handler is added, rather than being a
//! different kind of route.
//!
//! # Every error is rendered in one place
//!
//! A [`Problem`] converts into a response that carries *itself* rather than a
//! body, and one middleware turns it into JSON or a page according to the
//! request. That indirection buys uniformity: an error raised in a handler, one
//! raised by an extractor before a handler runs, the 404 from the router
//! fallback and the 405 from a method fallback are all rendered by the same
//! code, so none of them can be the one that forgets `Vary` or answers a
//! browser with raw JSON.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequestParts, OriginalUri, Path, Request, State};
use axum::http::header::{ACCEPT, CONTENT_TYPE, LOCATION, VARY};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, any, get};
use axum::{Router, middleware};
use headers::{ETag, HeaderMapExt, IfNoneMatch};
use kgf_store::catalog::BundleId;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::descriptor::{BundleManifest, DatasetDescriptor, ServiceDescriptor};
use crate::envelope::{ErrorCode, PROBLEM_MEDIA_TYPE, Problem};
use crate::html::Resource;
use crate::representation::{CachePolicy, Representation, etag, negotiate};
use crate::service::Service;
use crate::url::{self, Params};

/// The KGF routes over a built service.
pub fn router(service: Arc<Service>) -> Router {
    let max_request_bytes = service.config().budgets.max_request_bytes;

    Router::new()
        .route("/", read(get(service_descriptor)))
        .route("/{dataset}", read(get(dataset_descriptor)))
        // Method-preserving by construction: `any` hands every method to the
        // same handler, which answers 307 (§3.2). A router that matched only
        // GET here would answer a QUERY with 405 and hide the redirect.
        .route("/{dataset}/latest/{*rest}", any(latest_redirect))
        .route(
            "/{dataset}/v/{version}/manifest",
            read(get(bundle_manifest)),
        )
        .fallback(no_such_route)
        .layer(middleware::from_fn(render_problems))
        .layer(TraceLayer::new_for_http())
        // §3.6: permissive, because the data is public and browser and WASM
        // clients are a target. `QUERY` is listed explicitly — a preflight that
        // omitted it would leave the canonical method unusable from a browser,
        // which is the failure this whole unit exists to not discover in M2.
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_headers(Any)
                .allow_methods([Method::GET, Method::HEAD, Method::POST, query_method()])
                .expose_headers(Any),
        )
        .layer(DefaultBodyLimit::max(
            usize::try_from(max_request_bytes).unwrap_or(usize::MAX),
        ))
        .with_state(service)
}

/// RFC 10008's method. Not a constant in `http`, which is the whole reason this
/// unit checks that one survives the stack.
fn query_method() -> Method {
    Method::from_bytes(b"QUERY").expect("QUERY is a valid method token")
}

/// Mount a read-only route: the given methods, and a coded 405 for the rest.
///
/// axum supplies the `Allow` header for a method fallback, so what is added
/// here is the problem document — §3.6.1 says *every* error response carries a
/// code, and an empty 405 body is the one an off-the-shelf router gives away.
fn read(method_router: MethodRouter<Arc<Service>>) -> MethodRouter<Arc<Service>> {
    method_router.fallback(method_not_allowed)
}

async fn method_not_allowed(method: Method) -> Problem {
    Problem::new(
        ErrorCode::MethodNotAllowed,
        format!("{method} is not a method this resource takes; see the Allow header"),
    )
}

async fn no_such_route(OriginalUri(uri): OriginalUri) -> Problem {
    Problem::new(
        ErrorCode::NotFound,
        format!(
            "no resource at {}; this server serves / (service descriptor), \
             /{{dataset}}, /{{dataset}}/v/{{version}}/manifest, and the same under \
             /{{dataset}}/latest/",
            uri.path()
        ),
    )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn service_descriptor(
    State(service): State<Arc<Service>>,
    wants: Wants,
) -> Result<Response, Problem> {
    // No ETag: the descriptor changes when the deployment does, and there is no
    // digest over "what this host currently serves" to key one on.
    respond(
        &ServiceDescriptor::of(&service),
        &wants,
        CachePolicy::Mutable,
        None,
    )
}

async fn dataset_descriptor(
    State(service): State<Arc<Service>>,
    Path(dataset): Path<String>,
    wants: Wants,
) -> Result<Response, Problem> {
    let found = service.datasets().get(&dataset)?;
    respond(
        &DatasetDescriptor::of(&dataset, found),
        &wants,
        CachePolicy::Mutable,
        None,
    )
}

async fn bundle_manifest(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    wants: Wants,
) -> Result<Response, Problem> {
    let release = service.datasets().release(&dataset, &version)?;
    let resource = BundleManifest::new(
        &dataset,
        &version,
        release.digest().clone(),
        release.manifest().bytes(),
        release.manifest().parsed(),
    );
    let digest = release.digest().clone();

    // The manifest itself is already in memory, but a bundle that cannot be
    // opened must not be described as though it can: a client reads
    // `capabilities` here and issues the operations it finds. Opening is
    // singleflighted and cached by the catalog, so this costs one open per
    // version for the life of the process — and it is why this handler has a
    // blocking boundary at all.
    let id = BundleId { dataset, version };
    let opened = Arc::clone(&service);
    blocking(move || opened.open(&id).map(|_| ())).await?;

    respond(
        &resource,
        &wants,
        CachePolicy::Immutable,
        Some(etag(&digest, wants.representation()?)),
    )
}

async fn latest_redirect(
    State(service): State<Arc<Service>>,
    Path((dataset, _rest)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response, Problem> {
    let current = service.datasets().get(&dataset)?.current();

    // Rewritten on the *raw* path rather than on the decoded captures: whatever
    // encoding the client used for the rest of the path is what the redirect
    // must preserve, and re-encoding a decoded segment is not the identity.
    let path = uri.path();
    let (raw_dataset, remainder) = path
        .strip_prefix('/')
        .and_then(|path| path.split_once('/'))
        .ok_or_else(|| unreachable_route(path))?;
    let rest = remainder
        .strip_prefix("latest")
        .ok_or_else(|| unreachable_route(path))?;
    let location = match uri.query() {
        Some(query) => format!(
            "/{raw_dataset}/v/{}{rest}?{query}",
            url::encode_segment(current)
        ),
        None => format!("/{raw_dataset}/v/{}{rest}", url::encode_segment(current)),
    };

    // 307, not 308: `latest` moves, and a permanent redirect invites a client
    // to remember it. §3.2 requires one of the two that preserve the method —
    // a 302 may be rewritten to GET by intermediaries, which would silently
    // turn a body-carrying QUERY into something else.
    let mut response = Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .body(Body::empty())
        .expect("a redirect is a valid response");
    let headers = response.headers_mut();
    headers.insert(LOCATION, header(&location)?);
    headers.typed_insert(CachePolicy::Mutable.header());
    Ok(response)
}

/// The router matched `/{dataset}/latest/{rest}`, so the path has that shape.
fn unreachable_route(path: &str) -> Problem {
    tracing::error!(path, "a matched route did not have the shape it matched on");
    Problem::new(ErrorCode::InternalError, "the request could not be routed")
}

// ---------------------------------------------------------------------------
// What the request asked for
// ---------------------------------------------------------------------------

/// The negotiation and caching inputs of a request.
///
/// One extractor rather than three, because a handler that took `Accept`
/// without `format=`, or an `ETag` without the representation it belongs to,
/// would be answering a slightly different question than the client asked.
#[derive(Debug, Clone, Default)]
pub struct Wants {
    format: Option<String>,
    accept: Option<String>,
    if_none_match: Option<IfNoneMatch>,
}

impl Wants {
    /// The representation to answer with, or the negotiation failure.
    pub fn representation(&self) -> Result<Representation, Problem> {
        negotiate(
            self.format.as_deref(),
            self.accept.as_deref(),
            Representation::ALL,
        )
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Wants {
    type Rejection = Problem;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let params = Params::parse(parts.uri.query())?;
        let accept = parts
            .headers
            .get(ACCEPT)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Ok(Self {
            format: params.get("format").map(str::to_owned),
            accept,
            // A malformed `If-None-Match` is treated as absent, which RFC 9110
            // §13.1 requires of a precondition a server cannot evaluate: the
            // response is the full one, never a wrong 304.
            if_none_match: parts.headers.typed_get(),
        })
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Serve a resource in the representation the request asked for.
fn respond(
    resource: &impl Resource,
    wants: &Wants,
    cache: CachePolicy,
    etag: Option<ETag>,
) -> Result<Response, Problem> {
    let representation = wants.representation()?;

    // Before the body is built, not after: the point of a conditional request
    // is that the server does not spend the work. `precondition_passes` is
    // RFC 9110 §13.1.2's weak comparison, so `*`, a comma list and a `W/`
    // prefix are handled by `headers` rather than here.
    if let (Some(etag), Some(if_none_match)) = (&etag, &wants.if_none_match)
        && !if_none_match.precondition_passes(etag)
    {
        return finish(
            StatusCode::NOT_MODIFIED,
            representation,
            Body::empty(),
            cache,
            etag.clone().into(),
        );
    }

    let body = match representation {
        Representation::Json => resource.to_json(),
        Representation::Html => resource.to_html().into_bytes(),
    };
    finish(
        StatusCode::OK,
        representation,
        Body::from(body),
        cache,
        etag,
    )
}

fn finish(
    status: StatusCode,
    representation: Representation,
    body: Body,
    cache: CachePolicy,
    etag: Option<ETag>,
) -> Result<Response, Problem> {
    let mut response = Response::builder()
        .status(status)
        .body(body)
        .expect("a status and a body are a valid response");
    let headers = response.headers_mut();
    if status != StatusCode::NOT_MODIFIED {
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static(representation.content_type()),
        );
    }
    headers.typed_insert(cache.header());
    // §3.6: one URL serves many formats, so a cache that ignored `Accept` would
    // hand a page to an agent. Always, including on responses with no `ETag`.
    headers.insert(VARY, HeaderValue::from_static("Accept"));
    if let Some(etag) = etag {
        headers.typed_insert(etag);
    }
    Ok(response)
}

/// A header value, or the internal error that says this server built one it
/// cannot send.
///
/// Every value reaching here has been through a type that constrains it — an
/// [`ETag`] over a parsed digest, a location over percent-encoded segments — so
/// a failure is this server's bug, not the request's.
fn header(value: &str) -> Result<HeaderValue, Problem> {
    HeaderValue::from_str(value).map_err(|error| {
        tracing::error!(value, %error, "built a header value that cannot be sent");
        Problem::new(ErrorCode::InternalError, "the response could not be built")
    })
}

impl IntoResponse for Problem {
    /// Carry the problem, not its body.
    ///
    /// The rendering needs the *request* — a browser gets a page — which an
    /// `IntoResponse` does not have. The problem-rendering middleware has both.
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = Response::new(Body::empty());
        *response.status_mut() = status;
        response.extensions_mut().insert(self);
        response
    }
}

/// Render every [`Problem`] a request produced, in the client's representation.
async fn render_problems(request: Request, next: Next) -> Response {
    let format = Params::parse(request.uri().query())
        .ok()
        .and_then(|params| params.get("format").map(str::to_owned));
    let accept = request
        .headers()
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let instance = request.uri().path().to_owned();

    let mut response = next.run(request).await;
    let Some(problem) = response.extensions_mut().remove::<Problem>() else {
        return response;
    };

    let problem = problem.about(instance);
    let representation = Representation::for_problem(format.as_deref(), accept.as_deref());
    let (content_type, body) = match representation {
        // RFC 9457 §3 names the media type; it is not `application/json`.
        Representation::Json => (PROBLEM_MEDIA_TYPE, problem.to_json()),
        Representation::Html => (
            Representation::Html.content_type(),
            problem.to_html().into_bytes(),
        ),
    };

    *response.body_mut() = Body::from(body);
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.typed_insert(CachePolicy::Uncacheable.header());
    headers.insert(VARY, HeaderValue::from_static("Accept"));
    response
}

// ---------------------------------------------------------------------------
// The blocking boundary
// ---------------------------------------------------------------------------

/// Run store work on the blocking pool (doc 20 §20.4).
///
/// A cold read of a mapped bundle faults pages, and a page fault stalls the
/// thread it happens on. On the async reactor that is not one slow request but
/// a stalled server, so nothing that touches a `Store` runs there.
///
/// A panic in the pool becomes a coded 500 rather than a dropped connection.
/// That matters more here than in most servers: `todo!()` is this project's
/// convention for a path that is not built, so an unimplemented operation
/// reached by a client should answer `internal_error` and log, not look like a
/// network failure.
async fn blocking<T, F>(work: F) -> Result<T, Problem>
where
    F: FnOnce() -> Result<T, Problem> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(%error, "a request panicked on the blocking pool");
            Err(Problem::new(
                ErrorCode::InternalError,
                "the request failed while reading the bundle",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_method_is_a_method() {
        // If this ever stops holding, RFC 10008 is not expressible on this
        // stack and the choice of stack has to be revisited (doc 03 §3.1).
        assert_eq!(query_method().as_str(), "QUERY");
        assert!(!query_method().is_safe() || query_method().is_idempotent());
    }

    #[test]
    fn a_problem_carries_itself_until_something_can_render_it() {
        let response = Problem::new(ErrorCode::NotFound, "nope").into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            response.extensions().get::<Problem>().is_some(),
            "the middleware has nothing to render without it"
        );
    }
}
