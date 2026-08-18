//! Doc 03 §3.2's URL space, mounted.
//!
//! The thin adapter: every rule it applies is decided in a module that does not
//! know what HTTP is — [`crate::service`] resolves a URL to a release,
//! [`crate::representation`] chooses a serialization and a cache policy,
//! [`crate::html`] and [`crate::descriptor`] render. What lives here is the
//! wiring, and the things that are genuinely about the protocol: method
//! dispatch, the redirect, conditional requests, and the blocking boundary.
//!
//! # Method dispatch, including QUERY
//!
//! axum has no custom-method filter, so `QUERY` reaches the method fallback
//! with its method intact. `/fragment` and `/count` dispatch it there, while
//! their ordinary `POST` fallback is registered directly. Other methods get a
//! coded 405 whose `Allow` includes the extension method.
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
use axum::extract::{DefaultBodyLimit, FromRequestParts, OriginalUri, Request, State};
use axum::http::header::{ACCEPT, ALLOW, CONTENT_TYPE, LOCATION, RETRY_AFTER, VARY};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, any, get};
use axum::{Router, middleware};
use headers::{ETag, HeaderMapExt, Host, IfNoneMatch};
use kgf_store::Capability;
use kgf_store::catalog::BundleId;
use mediatype::{MediaTypeBuf, names};
use tower_http::cors::{Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::Limits;
use crate::admission::WorkClass;
use crate::answer::{self, Rendered, Renders, Target};
use crate::descriptor::{BundleManifest, DatasetDescriptor, ServiceDescriptor};
use crate::envelope::{ErrorCode, PROBLEM_MEDIA_TYPE, Problem, reflected};
use crate::html::Resource;
use crate::representation::{CachePolicy, Representation, etag, etag_for_body, negotiate};
use crate::request;
use crate::service::{Release, Service};
use crate::url::{self, Params};

/// The KGF routes over a built service.
pub fn router(service: Arc<Service>) -> Router {
    let body_limit =
        usize::try_from(service.config().budgets.max_request_bytes).unwrap_or(usize::MAX);

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
        // §3.4's read operations. Bindings use QUERY canonically and POST as a
        // compatibility fallback on `/fragment` and `/count`.
        .route(
            "/{dataset}/v/{version}/fragment",
            get(fragment)
                .post(fragment_bindings_post)
                .fallback(fragment_fallback),
        )
        .route(
            "/{dataset}/v/{version}/count",
            get(count)
                .post(count_bindings_post)
                .fallback(count_fallback),
        )
        .route("/{dataset}/v/{version}/describe", read(get(describe)))
        .route("/{dataset}/v/{version}/sample", read(get(sample)))
        .route("/{dataset}/v/{version}/search", read(get(search)))
        .route("/{dataset}/v/{version}/schema", read(get(schema)))
        .route("/{dataset}/v/{version}/void", read(get(void)))
        .route("/{dataset}/v/{version}/summary", read(get(summary)))
        .route(
            "/{dataset}/v/{version}/labels",
            MethodRouter::new()
                .post(labels_post)
                .fallback(labels_fallback),
        )
        .fallback(no_such_route)
        // Order matters more than usual here, and reads innermost first.
        //
        // `render_problems` must sit *outside* the body limit, or the 413 that
        // limit produces never reaches the code that gives it a `code` — which
        // is the whole reason the backstop exists. It must sit *inside* CORS,
        // because it sets `Vary: Accept` with `insert` and CORS appends its own
        // afterwards; the other way round would wipe them.
        .layer(TraceLayer::new_for_http())
        // Two limits, because they catch different things. `RequestBodyLimitLayer`
        // enforces the published figure on the wire whether or not anything
        // reads the body, which is what makes `max_request_bytes` true today
        // rather than a promise. `DefaultBodyLimit` is what the bindings body
        // extractors consult.
        .layer(RequestBodyLimitLayer::new(body_limit))
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(middleware::from_fn(render_problems))
        // §3.6: permissive, because the data is public and browser and WASM
        // clients are a target. `QUERY` is listed explicitly — a preflight that
        // omitted it would leave the canonical method unusable from a browser.
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_headers(Any)
                .allow_methods([Method::GET, Method::HEAD, Method::POST, query_method()])
                .expose_headers(Any),
        )
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
    method_not_allowed_problem(method)
}

fn method_not_allowed_problem(method: Method) -> Problem {
    Problem::new(
        ErrorCode::MethodNotAllowed,
        format!(
            "{} is not a method this resource takes; see the Allow header",
            reflected(method.as_str())
        ),
    )
}

async fn no_such_route(OriginalUri(uri): OriginalUri) -> Problem {
    Problem::new(
        ErrorCode::NotFound,
        format!(
            "no resource at {}; this server serves / (service descriptor), \
             /{{dataset}}, and /{{dataset}}/v/{{version}}/{{manifest,fragment,count,describe,\
             sample,search,labels,schema,void,summary}}; version resources are also available \
             under /{{dataset}}/latest/",
            reflected(uri.path())
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
    let representation = wants.representation()?;
    respond(
        &ServiceDescriptor::of(&service),
        representation,
        &wants,
        CachePolicy::Mutable,
        Some(etag(
            service.descriptor_digest(),
            service.descriptor_digest(),
            representation,
        )),
    )
}

async fn dataset_descriptor(
    State(service): State<Arc<Service>>,
    Path(dataset): Path<String>,
    wants: Wants,
) -> Result<Response, Problem> {
    let found = service.datasets().get(&dataset)?;
    let representation = wants.representation()?;
    respond(
        &DatasetDescriptor::of(&dataset, found),
        representation,
        &wants,
        CachePolicy::Mutable,
        Some(etag(
            service.descriptor_digest(),
            service.descriptor_digest(),
            representation,
        )),
    )
}

async fn bundle_manifest(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    wants: Wants,
) -> Result<Response, Problem> {
    // Both before any work: a request this server cannot answer is refused for
    // the reason it cannot be answered, and a request that was never going to
    // be served does not open a cold bundle to find that out. Read the other
    // way round, `?format=parquet` against a bundle whose artifacts are
    // incomplete is a 500 about the bundle rather than a 400 about the request.
    let representation = wants.representation()?;
    let release = service.datasets().release(&dataset, &version)?;
    let validator = etag(
        release.digest(),
        service.descriptor_digest(),
        representation,
    );

    // Ahead of the open, not merely ahead of the body. A revalidation names the
    // exact bytes it already holds, and a versioned URL cannot serve different
    // ones, so there is nothing left to check: opening the bundle first would
    // make a client's cheapest possible request pay for a cold mmap.
    if wants.already_has(&validator) {
        return not_modified(CachePolicy::Immutable, validator);
    }

    let resource = BundleManifest::new(
        &dataset,
        &version,
        release.manifest().bytes(),
        release.manifest().parsed(),
    );

    // The manifest itself is already in memory, but a bundle that cannot be
    // opened must not be described as though it can: a client reads
    // `capabilities` here and issues the operations it finds. Opening is
    // singleflighted and cached by the catalog, so this costs one open per
    // version for the life of the process — and it is why this handler has a
    // blocking boundary at all.
    let id = BundleId { dataset, version };
    let opened = Arc::clone(&service);
    blocking(&service, WorkClass::Ordinary, move || {
        opened.open(&id).map(|_| ())
    })
    .await?;

    respond(
        &resource,
        representation,
        &wants,
        CachePolicy::Immutable,
        Some(validator),
    )
}

// ---------------------------------------------------------------------------
// The §3.4 operations
// ---------------------------------------------------------------------------

async fn fragment(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    wants: Wants,
) -> Result<Response, Problem> {
    advertise_query(
        operate_represented(
            service,
            BundleId { dataset, version },
            "fragment",
            wants,
            Representation::FRAGMENT,
            |params, limits, release, representation| {
                let request = request::GetFragment::parse(
                    params,
                    limits,
                    release.prefixes(),
                    &release.binding(),
                    matches!(
                        representation,
                        Representation::Turtle | Representation::JsonLd
                    ),
                )?;
                declares_search(release, request.text().is_some())?;
                Ok(request)
            },
            answer::get_fragment,
        )
        .await,
    )
}

async fn count(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    wants: Wants,
) -> Result<Response, Problem> {
    advertise_query(
        operate(
            service,
            BundleId { dataset, version },
            "count",
            wants,
            |params, limits, release, _representation| {
                let request =
                    request::Count::parse(params, limits, release.prefixes(), &release.binding())?;
                declares_search(release, request.pattern.text().is_some())?;
                Ok(request)
            },
            answer::count,
        )
        .await,
    )
}

async fn fragment_bindings_post(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    wants: Wants,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Result<Response, Problem> {
    binding_fragment(
        service,
        BundleId { dataset, version },
        wants,
        headers,
        body,
        BodyMethod::Post,
    )
    .await
}

async fn fragment_fallback(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    request: Request,
) -> Result<Response, Problem> {
    let method = request.method().clone();
    if method != query_method() {
        return Ok(method_not_allowed_for_bindings(method));
    }
    let (wants, headers, body) = binding_body(request, &service).await?;
    binding_fragment(
        service,
        BundleId { dataset, version },
        wants,
        headers,
        body,
        BodyMethod::Query,
    )
    .await
}

async fn binding_fragment(
    service: Arc<Service>,
    id: BundleId,
    wants: Wants,
    headers: HeaderMap,
    body: bytes::Bytes,
    method: BodyMethod,
) -> Result<Response, Problem> {
    require_json(&headers)?;
    advertise_query(
        operate_body(
            service,
            id,
            "fragment",
            BodyOperation {
                wants,
                body,
                method,
            },
            |params, body, limits, release| {
                request::BindingFragment::parse(
                    params,
                    body,
                    limits,
                    release.prefixes(),
                    &release.binding(),
                )
            },
            answer::binding_fragment,
        )
        .await,
    )
}

async fn count_bindings_post(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    wants: Wants,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Result<Response, Problem> {
    binding_count(
        service,
        BundleId { dataset, version },
        wants,
        headers,
        body,
        BodyMethod::Post,
    )
    .await
}

async fn count_fallback(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    request: Request,
) -> Result<Response, Problem> {
    let method = request.method().clone();
    if method != query_method() {
        return Ok(method_not_allowed_for_bindings(method));
    }
    let (wants, headers, body) = binding_body(request, &service).await?;
    binding_count(
        service,
        BundleId { dataset, version },
        wants,
        headers,
        body,
        BodyMethod::Query,
    )
    .await
}

async fn binding_count(
    service: Arc<Service>,
    id: BundleId,
    wants: Wants,
    headers: HeaderMap,
    body: bytes::Bytes,
    method: BodyMethod,
) -> Result<Response, Problem> {
    require_json(&headers)?;
    advertise_query(
        operate_body(
            service,
            id,
            "count",
            BodyOperation {
                wants,
                body,
                method,
            },
            |params, body, limits, release| {
                request::BindingCount::parse(params, body, limits, release.prefixes())
            },
            answer::binding_count,
        )
        .await,
    )
}

async fn binding_body(
    request: Request,
    service: &Arc<Service>,
) -> Result<(Wants, HeaderMap, bytes::Bytes), Problem> {
    let (mut parts, body) = request.into_parts();
    let wants = Wants::from_request_parts(&mut parts, service).await?;
    let headers = parts.headers;
    let limit = usize::try_from(service.config().budgets.max_request_bytes).unwrap_or(usize::MAX);
    let body = axum::body::to_bytes(body, limit).await.map_err(|error| {
        tracing::debug!(%error, "a QUERY body exceeded its read bound");
        Problem::new(
            ErrorCode::PayloadTooLarge,
            format!(
                "the request body exceeds this server's max_request_bytes of {}",
                service.config().budgets.max_request_bytes
            ),
        )
    })?;
    Ok((wants, headers, body))
}

fn method_not_allowed_for_bindings(method: Method) -> Response {
    let mut response = method_not_allowed_problem(method).into_response();
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static("GET, HEAD, POST, QUERY"));
    add_accept_query(&mut response);
    response
}

fn require_json(headers: &HeaderMap) -> Result<(), Problem> {
    let values: Vec<_> = headers.get_all(CONTENT_TYPE).iter().collect();
    let json = match values.as_slice() {
        [value] => value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<MediaTypeBuf>().ok())
            .is_some_and(|value| value.ty() == names::APPLICATION && value.subty() == names::JSON),
        _ => false,
    };
    if json {
        Ok(())
    } else {
        Err(Problem::new(
            ErrorCode::UnsupportedMediaType,
            "this body operation needs exactly one Content-Type: application/json header; the supported request media type is also published in Accept-Query",
        ))
    }
}

fn advertise_query(result: Result<Response, Problem>) -> Result<Response, Problem> {
    result.map(|mut response| {
        add_accept_query(&mut response);
        response
    })
}

fn add_accept_query(response: &mut Response) {
    response.headers_mut().insert(
        HeaderName::from_static("accept-query"),
        HeaderValue::from_static("application/json"),
    );
}

async fn describe(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    wants: Wants,
) -> Result<Response, Problem> {
    operate(
        service,
        BundleId { dataset, version },
        "describe",
        wants,
        |params, limits, release, _representation| {
            request::Describe::parse(params, limits, release.prefixes(), &release.binding())
        },
        answer::describe,
    )
    .await
}

async fn sample(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    wants: Wants,
) -> Result<Response, Problem> {
    operate(
        service,
        BundleId { dataset, version },
        "sample",
        wants,
        |params, limits, release, _representation| {
            // §3.4.7 is an optional capability, so a bundle that does not
            // declare one is refused rather than served from artifacts it
            // never promised — and refused *here*, before the open, because
            // the manifest is already in memory.
            if !release.declares(Capability::Sample) {
                return Err(Problem::new(
                    ErrorCode::CapabilityNotAvailable,
                    "this bundle does not declare the `sample` capability; \
                     its manifest lists the ones it does",
                ));
            }
            request::Sample::parse(params, limits, release.prefixes())
        },
        answer::sample,
    )
    .await
}

async fn search(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    wants: Wants,
) -> Result<Response, Problem> {
    operate(
        service,
        BundleId { dataset, version },
        "search",
        wants,
        |params, limits, release, _representation| {
            if !release.declares(Capability::Search) {
                return Err(Problem::new(
                    ErrorCode::CapabilityNotAvailable,
                    "this bundle does not declare the `search` capability; its manifest lists the ones it does",
                ));
            }
            request::Search::parse(
                params,
                limits,
                release.prefixes(),
                release.predicate_roles(),
            )
        },
        answer::search,
    )
    .await
}

async fn schema(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    wants: Wants,
) -> Result<Response, Problem> {
    operate(
        service,
        BundleId { dataset, version },
        "schema",
        wants,
        |params, limits, release, _representation| {
            let request =
                request::Schema::parse(params, limits, release.prefixes(), &release.binding())?;
            if request.labels && !release.declares(Capability::Labels) {
                return Err(Problem::new(
                    ErrorCode::CapabilityNotAvailable,
                    "this bundle does not declare the `labels` capability; omit `labels=true` or use a release that does",
                ));
            }
            Ok(request)
        },
        answer::schema,
    )
    .await
}

async fn void(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    wants: Wants,
) -> Result<Response, Problem> {
    operate_special(
        service,
        BundleId { dataset, version },
        "void",
        wants,
        Representation::VOID,
        |params, limits, _release| request::Void::parse(params, limits),
        answer::void,
    )
    .await
}

async fn summary(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    wants: Wants,
) -> Result<Response, Problem> {
    operate_special(
        service,
        BundleId { dataset, version },
        "summary",
        wants,
        Representation::SUMMARY,
        |params, _limits, _release| request::Summary::parse(params),
        answer::summary,
    )
    .await
}

async fn labels_post(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    wants: Wants,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Result<Response, Problem> {
    labels_operation(
        service,
        BundleId { dataset, version },
        wants,
        headers,
        body,
        BodyMethod::Post,
    )
    .await
}

async fn labels_fallback(
    State(service): State<Arc<Service>>,
    Path((dataset, version)): Path<(String, String)>,
    request: Request,
) -> Result<Response, Problem> {
    let method = request.method().clone();
    if method != query_method() {
        let mut response = method_not_allowed_problem(method).into_response();
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("POST, QUERY"));
        add_accept_query(&mut response);
        return Ok(response);
    }
    let (wants, headers, body) = binding_body(request, &service).await?;
    labels_operation(
        service,
        BundleId { dataset, version },
        wants,
        headers,
        body,
        BodyMethod::Query,
    )
    .await
}

async fn labels_operation(
    service: Arc<Service>,
    id: BundleId,
    wants: Wants,
    headers: HeaderMap,
    body: bytes::Bytes,
    method: BodyMethod,
) -> Result<Response, Problem> {
    require_json(&headers)?;
    advertise_query(
        operate_body(
            service,
            id,
            "labels",
            BodyOperation {
                wants,
                body,
                method,
            },
            |params, body, limits, release| {
                if !release.declares(Capability::Labels) {
                    return Err(Problem::new(
                        ErrorCode::CapabilityNotAvailable,
                        "this bundle does not declare the `labels` capability; its manifest lists the ones it does",
                    ));
                }
                request::Labels::parse(
                    params,
                    body,
                    limits,
                    release.prefixes(),
                    release.predicate_roles(),
                )
            },
            answer::labels,
        )
        .await,
    )
}

/// Refuse `o.text` against a bundle that publishes no text index.
///
/// The same gate `/sample` gets, and in the same place: before the open, off
/// the manifest already in memory. §3.6.1 codes it 501 because the request is
/// well formed and the identical one against a bundle declaring `search`
/// succeeds — the shortfall is what this bundle carries.
fn declares_search(release: &Release, wanted: bool) -> Result<(), Problem> {
    if wanted && !release.declares(Capability::Search) {
        return Err(Problem::new(
            ErrorCode::CapabilityNotAvailable,
            "`o.text` needs the `search` capability, which this bundle does not declare;              its manifest lists the ones it does",
        ));
    }
    Ok(())
}

/// The shape every §3.4 operation has.
///
/// Read in order, because the order is the decision: negotiate, resolve the
/// version, read the parameters, evaluate the precondition, and only then open
/// a bundle. Everything before the open is pure and cheap, so a request that
/// cannot be answered — or one whose answer the client already holds — never
/// pays for a cold mmap.
async fn operate<Q, A, P, E>(
    service: Arc<Service>,
    id: BundleId,
    operation: &'static str,
    wants: Wants,
    parse: P,
    execute: E,
) -> Result<Response, Problem>
where
    Q: request::GetRequest + Send + 'static,
    A: Renders,
    P: FnOnce(&Params, Limits<'_>, &Release, Representation) -> Result<Q, Problem>,
    E: FnOnce(&kgf_store::Store, Target, &Q) -> Result<A, Problem> + Send + 'static,
{
    operate_represented(
        service,
        id,
        operation,
        wants,
        Representation::ALL,
        parse,
        execute,
    )
    .await
}

/// Run an ordinary answer operation with its own representation set.
async fn operate_represented<Q, A, P, E>(
    service: Arc<Service>,
    id: BundleId,
    operation: &'static str,
    wants: Wants,
    offered: &'static [Representation],
    parse: P,
    execute: E,
) -> Result<Response, Problem>
where
    Q: request::GetRequest + Send + 'static,
    A: Renders,
    P: FnOnce(&Params, Limits<'_>, &Release, Representation) -> Result<Q, Problem>,
    E: FnOnce(&kgf_store::Store, Target, &Q) -> Result<A, Problem> + Send + 'static,
{
    let representation = wants.representation_from(offered)?;
    if matches!(
        representation,
        Representation::Turtle | Representation::JsonLd
    ) && wants.request_url.is_none()
    {
        return Err(Problem::new(
            ErrorCode::MalformedRequest,
            "an RDF fragment request requires an absolute request target or a valid Host header",
        ));
    }
    let release = service.datasets().release(&id.dataset, &id.version)?;
    let params = Q::normalize_params(wants.params());
    let request = parse(&params, service.config().limits(), release, representation)?;
    let work_class = request.work_class();

    // A versioned operation is a deterministic function of immutable bytes
    // (doc 04 §4.6), so the URL and the representation fix the response
    // exactly — which is what makes a strong validator honest here and not
    // only on `/manifest`.
    let validator = etag(
        release.digest(),
        service.descriptor_digest(),
        representation,
    );
    if wants.already_has(&validator) {
        return not_modified(CachePolicy::Immutable, validator);
    }

    let target = Target::get(
        id,
        operation,
        params,
        release.prefixes().clone(),
        release.declares(Capability::Search),
        wants.request_url.clone(),
    );
    let labels = PageLabelProfile::for_request(
        &service,
        release,
        representation,
        request.labels_requested(),
    );
    let opened = Arc::clone(&service);
    let rendered = blocking(&service, work_class, move || {
        let store = opened.open(target.id())?;
        // Serialized in here, not outside: doc 20 §20.5 materializes strings
        // only while writing them, and the term cache that makes that cheap is
        // deliberately not `Send`.
        let mut answer = execute(&store, target, &request)?;
        labels.hydrate(&store, &mut answer)?;
        answer.render(representation)
    })
    .await?;

    respond_rendered(rendered, representation, CachePolicy::Immutable, validator)
}

/// Run a versioned operation whose representation set is not the ordinary
/// JSON/page pair.
async fn operate_special<Q, P, E>(
    service: Arc<Service>,
    id: BundleId,
    operation: &'static str,
    wants: Wants,
    offered: &'static [Representation],
    parse: P,
    execute: E,
) -> Result<Response, Problem>
where
    Q: request::GetRequest + Send + 'static,
    P: FnOnce(&Params, Limits<'_>, &Release) -> Result<Q, Problem>,
    E: FnOnce(&kgf_store::Store, Target, &Q, Representation) -> Result<Rendered, Problem>
        + Send
        + 'static,
{
    let representation = wants.representation_from(offered)?;
    let release = service.datasets().release(&id.dataset, &id.version)?;
    let params = Q::normalize_params(wants.params());
    let request = parse(&params, service.config().limits(), release)?;
    let work_class = request.work_class();
    let validator = etag(
        release.digest(),
        service.descriptor_digest(),
        representation,
    );
    if wants.already_has(&validator) {
        return not_modified(CachePolicy::Immutable, validator);
    }

    let target = Target::get(
        id,
        operation,
        params,
        release.prefixes().clone(),
        release.declares(Capability::Search),
        wants.request_url.clone(),
    );
    let opened = Arc::clone(&service);
    let rendered = blocking(&service, work_class, move || {
        let store = opened.open(target.id())?;
        execute(&store, target, &request, representation)
    })
    .await?;

    respond_rendered(rendered, representation, CachePolicy::Immutable, validator)
}

/// What an HTML render needs to annotate its IRIs with display labels: the
/// release's frozen `label` cascade and the cap that bounds the work.
///
/// Built only when the negotiated representation is a page. JSON does not pay
/// for labels it does not carry — a JSON client hydrates through `/labels`,
/// with the same cascade and the same cap.
struct PageLabelProfile {
    predicates: Vec<String>,
    cap: usize,
    required: bool,
}

impl PageLabelProfile {
    fn for_request(
        service: &Service,
        release: &Release,
        representation: Representation,
        requested: bool,
    ) -> Option<Self> {
        (representation == Representation::Html || requested).then(|| Self {
            predicates: release
                .predicate_roles()
                .get("label")
                .map(<[String]>::to_vec)
                .unwrap_or_default(),
            cap: usize::try_from(service.config().caps.max_label_iris).unwrap_or(usize::MAX),
            required: requested,
        })
    }
}

/// Hydrate an answer's page labels if this request renders a page.
trait HydrateForPage {
    fn hydrate(&self, store: &kgf_store::Store, answer: &mut impl Renders) -> Result<(), Problem>;
}

impl HydrateForPage for Option<PageLabelProfile> {
    fn hydrate(&self, store: &kgf_store::Store, answer: &mut impl Renders) -> Result<(), Problem> {
        match self {
            None => Ok(()),
            Some(profile) => {
                answer.hydrate_labels(store, &profile.predicates, profile.cap, profile.required)
            }
        }
    }
}

/// The body-addressed form of an operation.
///
/// QUERY is immutable like a versioned GET, but its entity identity includes
/// the body. POST executes the same request as the compatibility method and is
/// deliberately `no-store`. The method is carried explicitly because a failed
/// `If-None-Match` is 304 for QUERY and 412 for POST; cacheability does not
/// decide precondition semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyMethod {
    Query,
    Post,
}

impl BodyMethod {
    const fn cache(self) -> CachePolicy {
        match self {
            Self::Query => CachePolicy::Immutable,
            Self::Post => CachePolicy::Uncacheable,
        }
    }
}

struct BodyOperation {
    wants: Wants,
    body: bytes::Bytes,
    method: BodyMethod,
}

async fn operate_body<Q, A, P, E>(
    service: Arc<Service>,
    id: BundleId,
    operation: &'static str,
    submitted: BodyOperation,
    parse: P,
    execute: E,
) -> Result<Response, Problem>
where
    Q: Send + 'static,
    A: Renders,
    P: FnOnce(&Params, &[u8], Limits<'_>, &Release) -> Result<Q, Problem>,
    E: FnOnce(&kgf_store::Store, Target, &Q) -> Result<A, Problem> + Send + 'static,
{
    let BodyOperation {
        wants,
        body,
        method,
    } = submitted;
    let cache = method.cache();
    let representation = wants.representation()?;
    let release = service.datasets().release(&id.dataset, &id.version)?;
    let request = parse(wants.params(), &body, service.config().limits(), release)?;
    let validator = etag_for_body(
        release.digest(),
        service.descriptor_digest(),
        operation,
        representation,
        &body,
    );
    if wants.already_has(&validator) {
        return match method {
            BodyMethod::Query => not_modified(cache, validator),
            BodyMethod::Post => Err(Problem::new(
                ErrorCode::PreconditionFailed,
                "If-None-Match matches the representation this POST would return; remove the \
                 precondition or submit a request whose representation is not current",
            )),
        };
    }

    let target = Target::body(
        id,
        operation,
        wants.params().clone(),
        release.prefixes().clone(),
    );
    let labels = PageLabelProfile::for_request(&service, release, representation, false);
    let opened = Arc::clone(&service);
    let rendered = blocking(&service, WorkClass::Heavy, move || {
        let store = opened.open(target.id())?;
        let mut answer = execute(&store, target, &request)?;
        labels.hydrate(&store, &mut answer)?;
        answer.render(representation)
    })
    .await?;

    respond_rendered(rendered, representation, cache, validator)
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

/// A path capture, rejected as a [`Problem`] like everything else.
///
/// `axum::extract::Path` rejects with its own plain-text 400, which is
/// reachable from a URL as ordinary as `/%FF`: a segment that is not UTF-8 once
/// decoded. That response has no `code`, no `Vary`, and is never a page, so it
/// is the one hole in §3.6.1's "every error response carries a code" — and one
/// this crate argued into the spec. Wrapping the extractor closes it.
struct Path<T>(T);

impl<S, T> FromRequestParts<S> for Path<T>
where
    T: serde::de::DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = Problem;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        use axum::extract::rejection::PathRejection;

        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(captured)) => Ok(Self(captured)),
            // The client's: a segment that does not decode, or does not fit the
            // shape the route captures.
            Err(PathRejection::FailedToDeserializePathParams(error)) => Err(Problem::new(
                ErrorCode::MalformedRequest,
                format!(
                    "a path segment could not be read: {}",
                    reflected(&error.to_string())
                ),
            )),
            // Ours: the route's captures and the handler's parameters disagree,
            // which is a wiring bug in this file and not a bad request.
            Err(rejection) => {
                tracing::error!(%rejection, "a route's path captures do not match its handler");
                Err(Problem::new(
                    ErrorCode::InternalError,
                    "the request could not be routed",
                ))
            }
        }
    }
}

/// The negotiation and caching inputs of a request.
///
/// One extractor rather than three, because a handler that took `Accept`
/// without `format=`, or an `ETag` without the representation it belongs to,
/// would be answering a slightly different question than the client asked.
#[derive(Debug, Clone, Default)]
pub struct Wants {
    params: Params,
    accept: Option<String>,
    if_none_match: Option<IfNoneMatch>,
    /// Exact absolute URL as the client addressed it, when the request carries
    /// a usable authority. Fragment RDF needs this for Hydra's page subject.
    request_url: Option<String>,
}

impl Wants {
    /// The request's query parameters, parsed once.
    ///
    /// Held rather than re-read per handler: `format=` is a negotiation input
    /// and the rest are the operation's, and parsing the string twice would
    /// also mean two chances to disagree about what it said.
    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Whether the client already holds this exact entity (RFC 9110 §13.1.2).
    ///
    /// `precondition_passes` is `headers`' implementation of that section's
    /// weak comparison, so `*`, a comma list and a `W/` prefix are its problem
    /// rather than this crate's. Called twice per conditional request — once by
    /// a handler wanting to skip work before it starts, once by the responder so
    /// that a route which forgot to ask still cannot serve a body the client
    /// has — and it is a header comparison, so twice is free.
    pub fn already_has(&self, validator: &ETag) -> bool {
        self.if_none_match
            .as_ref()
            .is_some_and(|if_none_match| !if_none_match.precondition_passes(validator))
    }

    /// The representation to answer with, or the negotiation failure.
    pub fn representation(&self) -> Result<Representation, Problem> {
        self.representation_from(Representation::ALL)
    }

    /// Negotiate from the representations one specialized resource offers.
    pub fn representation_from(
        &self,
        offered: &[Representation],
    ) -> Result<Representation, Problem> {
        negotiate(self.params.get("format"), self.accept.as_deref(), offered)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Wants {
    type Rejection = Problem;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self {
            params: Params::parse(parts.uri.query())?,
            accept: accept_header(&parts.headers)?,
            // A malformed `If-None-Match` is treated as absent, which RFC 9110
            // §13.1 requires of a precondition a server cannot evaluate: the
            // response is the full one, never a wrong 304.
            if_none_match: parts.headers.typed_get(),
            request_url: absolute_request_url(parts),
        })
    }
}

/// Reconstruct the absolute request target from HTTP's authority and the
/// untouched path/query. `kgf serve` is a plain-HTTP listener; a TLS reverse
/// proxy must preserve the public Host (public-origin configuration remains a
/// deployment-level design question in `notes/plan.md`).
fn absolute_request_url(parts: &axum::http::request::Parts) -> Option<String> {
    if parts.uri.scheme().is_some() && parts.uri.authority().is_some() {
        return Some(parts.uri.to_string());
    }
    let host: Host = parts.headers.typed_get()?;
    Some(format!("http://{host}{}", parts.uri))
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The request's `Accept`, however many field lines it arrived on.
///
/// RFC 9110 §5.3 lets a sender split a list-valued field across several lines
/// and requires a recipient to treat them as one comma-separated list.
/// `HeaderMap::get` returns the first, so reading it alone turns
/// `Accept: application/xml` + `Accept: text/html` into a 406 for a request
/// that asked for something this server has.
///
/// A line that is not readable as text is an error rather than a line to skip.
/// Dropping one and negotiating from the rest answers a *different* request
/// than the client made — the same substitution `url::decode_component` refuses
/// for a term parameter, and for the same reason: it succeeds, so nobody finds
/// out.
fn accept_header(headers: &axum::http::HeaderMap) -> Result<Option<String>, Problem> {
    let mut lines = Vec::new();
    for value in headers.get_all(ACCEPT) {
        lines.push(value.to_str().map_err(|_| {
            Problem::new(
                ErrorCode::MalformedRequest,
                "an Accept header is not readable as text; \
                 media types and their parameters are ASCII (RFC 9110 §5.6.3)",
            )
        })?);
    }
    let combined = lines.join(", ");
    Ok((!combined.is_empty()).then_some(combined))
}

/// Serve a resource in the representation the request asked for.
fn respond(
    resource: &impl Resource,
    representation: Representation,
    wants: &Wants,
    cache: CachePolicy,
    etag: Option<ETag>,
) -> Result<Response, Problem> {
    // The backstop. A handler that can skip real work by checking earlier does
    // so itself; this is here so that no route can serve a full body to a
    // client that already holds it just because its handler did not ask.
    if let Some(validator) = &etag
        && wants.already_has(validator)
    {
        return not_modified(cache, validator.clone());
    }

    let body = match representation {
        Representation::Json => resource.to_json(),
        Representation::Html => bytes::Bytes::from(resource.to_html()),
        Representation::Turtle | Representation::JsonLd | Representation::Markdown => {
            unreachable!("Resource responses negotiate only JSON and HTML")
        }
    };
    finish(
        StatusCode::OK,
        Some(representation),
        Body::from(body),
        cache,
        etag,
    )
}

/// Serve a body an operation already rendered.
///
/// Separate from [`respond`] because the §3.4 operations serialize inside the
/// blocking task rather than handing back a [`Resource`] — and because they are
/// the only responses that carry §3.6's completeness metadata.
fn respond_rendered(
    rendered: Rendered,
    representation: Representation,
    cache: CachePolicy,
    validator: ETag,
) -> Result<Response, Problem> {
    let mut response = finish(
        StatusCode::OK,
        Some(representation),
        Body::from(rendered.body),
        cache,
        Some(validator),
    )?;
    // §3.6 requires the same metadata on the headers as in the body, so that a
    // serialization whose body has nowhere to put it still carries it, and so
    // an intermediary can read it without parsing a response.
    let headers = response.headers_mut();
    for (name, value) in rendered.completeness.headers() {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).expect("§3.6's field names are field names"),
            header(value)?,
        );
    }
    Ok(response)
}

/// RFC 9110 §15.4.5's 304: no body, and the validator and freshness the
/// response would have carried.
fn not_modified(cache: CachePolicy, validator: ETag) -> Result<Response, Problem> {
    finish(
        StatusCode::NOT_MODIFIED,
        None,
        Body::empty(),
        cache,
        Some(validator),
    )
}

fn finish(
    status: StatusCode,
    representation: Option<Representation>,
    body: Body,
    cache: CachePolicy,
    etag: Option<ETag>,
) -> Result<Response, Problem> {
    let mut response = Response::builder()
        .status(status)
        .body(body)
        .expect("a status and a body are a valid response");
    let headers = response.headers_mut();
    if let Some(representation) = representation {
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
        if let Some(seconds) = self.retry_after_seconds() {
            response.headers_mut().insert(
                RETRY_AFTER,
                HeaderValue::from_str(&seconds.to_string())
                    .expect("an integer number of seconds is a valid Retry-After value"),
            );
        }
        response.extensions_mut().insert(self);
        response
    }
}

/// Render every error response in the client's representation, with a code.
///
/// Two jobs, and the second is why this is a layer rather than a helper. A
/// [`Problem`] a handler or an extractor raised arrives as an empty response
/// carrying itself, and is rendered here. But a `tower` layer can answer
/// *before* any of this crate's code runs — the body limit below does — and
/// §3.6.1 says every error response carries a code, including those. So an
/// error that arrives unattributed is given a problem from its status rather
/// than shipped as whatever the layer produced.
async fn render_problems(request: Request, next: Next) -> Response {
    // Cloned, not parsed: both are refcounted buffers, and the parsing they
    // feed is only needed by the small minority of requests that fail. Doing it
    // up front cost every successful response a `BTreeMap` and two `String`s.
    let uri = request.uri().clone();
    let accept: Vec<_> = request.headers().get_all(ACCEPT).iter().cloned().collect();

    let mut response = next.run(request).await;
    let problem = match response.extensions_mut().remove::<Problem>() {
        Some(problem) => problem,
        None => match unattributed_error(&response) {
            Some(problem) => problem,
            None => return response,
        },
    };

    let format = Params::parse(uri.query())
        .ok()
        .and_then(|params| params.get("format").map(str::to_owned));
    let accept = {
        let mut headers = axum::http::HeaderMap::new();
        for value in accept {
            headers.append(ACCEPT, value);
        }
        accept_header(&headers).ok().flatten()
    };
    let problem = problem.about_unless_set(reflected(uri.path()));
    let representation = Representation::for_problem(format.as_deref(), accept.as_deref());
    let (content_type, body) = match representation {
        // RFC 9457 §3 names the media type; it is not `application/json`.
        Representation::Json => (PROBLEM_MEDIA_TYPE, problem.to_json()),
        Representation::Html => (
            Representation::Html.content_type(),
            bytes::Bytes::from(problem.to_html()),
        ),
        Representation::Turtle | Representation::JsonLd | Representation::Markdown => {
            unreachable!("problem negotiation resolves to JSON or HTML")
        }
    };

    *response.body_mut() = Body::from(body);
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.typed_insert(CachePolicy::Uncacheable.header());
    headers.insert(VARY, HeaderValue::from_static("Accept"));
    if advertises_query(uri.path()) {
        add_accept_query(&mut response);
    }
    response
}

fn advertises_query(path: &str) -> bool {
    path.ends_with("/fragment") || path.ends_with("/count")
}

/// A problem for an error response no handler in this crate produced.
///
/// `None` for a success, and for a status this crate cannot attribute — 400 in
/// particular, which five codes share, so guessing would tell a client the
/// wrong thing to fix. Anything unattributable is logged, because it means a
/// layer is answering in a shape §3.6.1 does not cover.
fn unattributed_error(response: &Response) -> Option<Problem> {
    let status = response.status();
    if !status.is_client_error() && !status.is_server_error() {
        return None;
    }
    match ErrorCode::for_unattributed_status(status.as_u16()) {
        Some(code) => Some(Problem::new(
            code,
            status
                .canonical_reason()
                .unwrap_or("the request could not be completed"),
        )),
        None => {
            tracing::warn!(%status, "an error response was produced with no code");
            None
        }
    }
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
async fn blocking<T, F>(service: &Service, class: WorkClass, work: F) -> Result<T, Problem>
where
    F: FnOnce() -> Result<T, Problem> + Send + 'static,
    T: Send + 'static,
{
    let admitted = service.admission().enter(class).await?;
    match tokio::task::spawn_blocking(move || {
        // A blocking task cannot be cancelled after it starts. Keep its
        // capacity in the task itself, so a disconnected client dropping the
        // awaiting handler does not admit replacement work while this work is
        // still faulting pages or building a response.
        let _admitted = admitted;
        work()
    })
    .await
    {
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

    #[test]
    fn a_rate_limit_tells_the_client_when_to_retry() {
        let response = Problem::new(ErrorCode::RateLimited, "busy")
            .with_retry_after(2)
            .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[RETRY_AFTER], "2");
        assert_eq!(
            response.extensions().get::<Problem>().map(Problem::code),
            Some(ErrorCode::RateLimited)
        );
    }
}
