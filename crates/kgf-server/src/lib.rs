//! The KGF HTTP API.
//!
//! This crate implements the KGF HTTP API over [`kgf_store`]. The split is the
//! point: HTTP semantics — caps, budgets, the truncation vocabulary,
//! serialization formats, cursor tokens — must not leak into storage code, and
//! the store must stay testable headless against fixture bundles.
//!
//! # Shape
//!
//! Handlers hold an `Arc<Store>` for the request and call synchronous methods
//! on a blocking pool. That is not incidental: a page fault stalls a thread, and
//! stalling the async reactor on a cold mmap read would convert one slow request
//! into a stalled server.
//!
//! HTTP QUERY (RFC 10008) is canonical for body-carrying reads; POST is a
//! permanent first-class fallback with identical semantics — the one place this
//! design ships two paths, because the web does. `/fragment` and `/count`
//! dispatch QUERY through the router's custom-method fallback; routes that do
//! not take it answer `method_not_allowed` with an `Allow` header.
//!
//! # One URL, two readers
//!
//! Every route answers a machine representation *and* HTML, chosen by `Accept`
//! alone — a page in a browser, data from `curl`, at the same URL. Most use
//! JSON; RDF `/void` uses Turtle/JSON-LD and static `/summary` uses
//! Markdown/JSON. See [`representation`] for why the tie-break falls the way it
//! does, and [`html`] for the rendering.
//!
//! # Status
//!
//! Units 10–23 in `notes/plan.md` are implemented: [`cursor`], [`term`],
//! [`envelope`], the URL space with `latest`, caching and content negotiation,
//! and the read operations `/fragment`, `/count`, `/describe`, `/sample`
//! and `/schema`, the `/void` and `/summary` description resources, plus bindings
//! QUERY/POST for fragment and count in [`request`] and [`answer`]. The service
//! emits typed, content-free access records through [`access`] when configured.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod access;
mod admission;
pub mod answer;
pub mod cursor;
pub mod descriptor;
pub mod envelope;
mod forms;
pub mod html;
mod rdf;
pub mod representation;
pub mod request;
pub mod routes;
pub mod service;
mod skolem;
pub mod term;
pub mod url;

use std::sync::Arc;

use kgf_store::map::PublishedRoot;
use serde::Serialize;

use crate::service::Service;

pub use access::{AccessLog, AccessRecord, StdoutAccessLog};
pub use admission::Admission;

/// Server configuration.
///
/// Its caps and budgets are also published at `/`, allowing clients to discover
/// them instead of assuming values. That is only useful if the
/// values they read are the values applied. Admission is host policy rather
/// than a per-request cost promise; clients encounter it through the standard
/// `rate_limited` problem and `Retry-After`.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory of bundles, laid out as `{root}/{dataset}/{version}`.
    ///
    /// A [`PublishedRoot`] rather than a path: mapping a file that another
    /// process can truncate is undefined behaviour, and this capability is
    /// where a caller records that the tree is published and immutable. Taking
    /// a `&Path` here would make that promise on the operator's
    /// behalf, from a library that can be embedded anywhere.
    pub bundle_root: PublishedRoot,
    /// Address to bind.
    pub bind: std::net::SocketAddr,
    /// The trusted external base this deployment is reachable at, when TLS
    /// termination, host rewriting, or path-prefix stripping happens in a
    /// reverse proxy.
    ///
    /// Its origin replaces the request's `Host` in absolute Hydra IRIs, and
    /// its path — the prefix the proxy removed — is put back on every link the
    /// server emits. Forwarding headers are deliberately not trusted
    /// implicitly: an untrusted client could otherwise choose the dataset and
    /// continuation IRIs emitted in a response. Leave this unset for direct
    /// plain-HTTP service at a hostname root, where the request's `Host` is
    /// authoritative and no link needs a prefix.
    pub public_base: Option<PublicBase>,
    /// The largest values a request may ask for.
    pub caps: Caps,
    /// The work one response may cost.
    pub budgets: Budgets,
    /// Deployment-wide admission limits for active and waiting bundle work.
    pub admission: Admission,
    /// Destination for one structured access record per response.
    ///
    /// `None` disables record emission. Server-minted request identifiers are
    /// still returned so an embedder can correlate its own instrumentation.
    pub access_log: Option<Arc<dyn AccessLog>>,
    /// Whether records include the raw request target, typed search string,
    /// User-Agent, and inbound request identifier.
    ///
    /// Off by default because these fields contain client-supplied content;
    /// the ordinary shape tier contains only parsed structure and magnitudes.
    pub log_raw: bool,
    /// Reverse proxies between the listener and its clients that append to
    /// `X-Forwarded-For`.
    ///
    /// Zero, the default, ignores the header: the peer is the client, and a
    /// caller cannot pick its own pseudonymous identity by sending one. Behind
    /// one gateway, set one; a record's `forwarded_hash` then names the
    /// address that gateway received the request from.
    pub trusted_proxies: u8,
}

impl Config {
    /// A configuration serving `bundle_root` on `bind`, with the default caps
    /// and budgets.
    pub fn new(bundle_root: PublishedRoot, bind: std::net::SocketAddr) -> Self {
        Self {
            bundle_root,
            bind,
            public_base: None,
            caps: Caps::default(),
            budgets: Budgets::default(),
            admission: Admission::default(),
            access_log: None,
            log_raw: false,
            trusted_proxies: 0,
        }
    }

    /// The published numbers this deployment reads requests against.
    pub fn limits(&self) -> Limits<'_> {
        Limits {
            caps: &self.caps,
            budgets: &self.budgets,
        }
    }
}

/// A deployment's trusted externally visible base: `scheme://authority`, plus
/// the path prefix a gateway strips before requests reach this server.
///
/// The path is *what the gateway removed*, nothing more. The router stays
/// mounted at `/` and never accepts the prefixed spelling; the base only
/// changes what the server emits — every root-relative link gains the prefix,
/// and every absolute IRI starts with the whole base. A base with a query,
/// fragment, or userinfo is refused: none of those can be part of a resource's
/// identity.
///
/// Normalized: `http` or `https`, a non-empty host, no trailing slash, and the
/// path percent-encoded exactly as typed, because the gateway matched those
/// bytes and the emitted links must repeat them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicBase {
    /// `scheme://authority`.
    origin: String,
    /// `""` or `/segment(/segment)*`.
    path_prefix: String,
}

impl PublicBase {
    /// The whole base, `scheme://authority[/prefix]`, without a trailing slash.
    ///
    /// An absolute IRI is this followed by the server-seen path and query.
    pub fn as_str(&self) -> String {
        format!("{}{}", self.origin, self.path_prefix)
    }

    /// The `scheme://authority` part alone.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The path the gateway removes: `""` at a hostname root, else `/kgf`.
    ///
    /// Never a bare `/`, so `format!("{prefix}/{dataset}")` is right in both
    /// cases.
    pub fn path_prefix(&self) -> &str {
        &self.path_prefix
    }

    /// The mount every emitted link is built against.
    pub fn mount(&self) -> url::Mount {
        url::Mount::with_prefix(&self.path_prefix)
    }
}

impl std::str::FromStr for PublicBase {
    type Err = PublicBaseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.contains('#') {
            return Err(PublicBaseError);
        }
        let uri: axum::http::Uri = value.parse().map_err(|_| PublicBaseError)?;
        let authority = uri.authority().ok_or(PublicBaseError)?;
        if !matches!(uri.scheme_str(), Some("http" | "https"))
            || authority.host().is_empty()
            || authority.as_str().contains('@')
            || uri.query().is_some()
        {
            return Err(PublicBaseError);
        }
        // `""` and `/` are the root; `/kgf/` and `/kgf` are the same prefix.
        // Every other segment must be a plain one. An empty segment (`/kgf//x`)
        // would emit links with `//`, and a dot segment (`/a/../kgf`, or its
        // percent-encoded spelling) would emit links that a browser normalizes
        // to a different path from the one the RDF identities carry — so the
        // same page would be one resource to Hydra and another to a browser.
        // Both are refused rather than normalized here: the prefix must be
        // spelled exactly as the gateway matches it.
        let path_prefix = uri.path().trim_end_matches('/');
        if path_prefix
            .split('/')
            .skip(1)
            .any(|segment| segment.is_empty() || is_dot_segment(segment))
        {
            return Err(PublicBaseError);
        }
        Ok(Self {
            origin: format!(
                "{}://{}",
                uri.scheme_str().expect("checked above"),
                authority
            ),
            path_prefix: path_prefix.to_owned(),
        })
    }
}

/// Whether a path segment is `.` or `..` in any percent-encoded spelling
/// (RFC 3986 §3.3; `%2E` is the same octet as `.`).
fn is_dot_segment(segment: &str) -> bool {
    let mut dots = 0usize;
    let mut rest = segment;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            rest = after;
        } else if let Some(after) = rest
            .strip_prefix("%2E")
            .or_else(|| rest.strip_prefix("%2e"))
        {
            rest = after;
        } else {
            return false;
        }
        dots += 1;
    }
    matches!(dots, 1 | 2)
}

/// A configured public base is not an HTTP(S) URL a deployment can be mounted at.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error(
    "expected an absolute http:// or https:// base with a host, an optional path prefix, and no userinfo, query, or fragment"
)]
pub struct PublicBaseError;

/// The largest values a request may ask for.
///
/// A request above a cap is refused with `cap_exceeded` rather than quietly
/// reduced: a client that asked for 50 000 rows and got 10 000 without being
/// told has been handed a truncated answer that claims to be complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Caps {
    /// Rows per ordinary result page; `/schema` has `max_schema_items`.
    pub max_limit: u32,
    /// Rows per page when a request does not ask for a number.
    ///
    /// Published beside the hard caps because a client cannot otherwise know
    /// how large `GET /fragment` is when `limit` is omitted. Small on purpose —
    /// an agent's first
    /// request to an unfamiliar endpoint should be cheap, and `next` says
    /// there is more.
    pub default_limit: u32,
    /// Members drawn by one `/sample`; `n` may not exceed 1,000.
    pub max_sample: u32,
    /// Input rows in a bindings QUERY.
    pub max_bindings: u32,
    /// Subjects in a star request (M2).
    pub max_star_subjects: u32,
    /// Predicates per star subject (M2).
    pub max_star_width: u32,
    /// Predicate IRIs one search request may select after role expansion.
    pub max_search_predicates: u32,
    /// Entity hits one `/search` request may retain.
    pub max_search_results: u32,
    /// IRIs one `/labels` request may resolve.
    pub max_label_iris: u32,
    /// Child or class-relation rows in one `/schema` page.
    pub max_schema_items: u32,
}

impl Caps {
    /// The default request caps.
    ///
    /// `const` so that a caller — or a test of the parsing these bound — can
    /// name them without building a [`Config`], which needs a capability over a
    /// real directory.
    pub const fn new() -> Self {
        Self {
            max_limit: 10_000,
            default_limit: 100,
            max_sample: 1_000,
            max_bindings: 1_000,
            max_star_subjects: 1_000,
            max_star_width: 32,
            max_search_predicates: 128,
            max_search_results: 1_000,
            max_label_iris: 10_000,
            max_schema_items: 1_000,
        }
    }
}

impl Default for Caps {
    fn default() -> Self {
        Self::new()
    }
}

/// The published numbers a request is read against.
///
/// Borrowed together because they are read together: a term parameter is
/// checked against a budget and a page size against a cap, and an operation
/// that saw one without the other would enforce half of what `/` advertises.
#[derive(Debug, Clone, Copy)]
pub struct Limits<'a> {
    /// The largest values a request may ask for.
    pub caps: &'a Caps,
    /// The work one response may cost.
    pub budgets: &'a Budgets,
}

impl Limits<'_> {
    /// Refuse a deployment whose caps let a request outrun its own budgets.
    ///
    /// Caps bound what a client may ask for and budgets bound what a response
    /// may cost, so the operator must keep the two consistent. A `max_limit` above
    /// `max_output_rows` is a server that publishes a page size it will not
    /// honour. Refusing to start avoids either silent or routine truncation and
    /// buys something concrete:
    /// [`crate::answer`] does not check the row and term budgets per request,
    /// because a configuration that passes here cannot reach them.
    ///
    /// `max_response_bytes` is deliberately not in this list. No cap bounds it
    /// because one legal literal can be megabytes, so it is
    /// the one composite budget that has to be applied while a response is
    /// built.
    pub fn validate(&self) -> Result<(), String> {
        // Describe rows and flat schema class relations are the widest this
        // milestone serves: each carries three RDF terms.
        const WIDEST_ROW: u64 = 3;

        let rows = |what: &str, cap: u32| {
            let cap = u64::from(cap);
            if cap > self.budgets.max_output_rows {
                return Err(format!(
                    "caps.{what} is {cap}, over budgets.max_output_rows of {}; \
                     a request at the cap would exceed a budget this server publishes",
                    self.budgets.max_output_rows
                ));
            }
            if cap * WIDEST_ROW > self.budgets.max_output_terms {
                return Err(format!(
                    "caps.{what} is {cap}, and {cap} rows of {WIDEST_ROW} terms is over \
                     budgets.max_output_terms of {}",
                    self.budgets.max_output_terms
                ));
            }
            Ok(())
        };
        rows("max_limit", self.caps.max_limit)?;
        rows("max_sample", self.caps.max_sample)?;
        rows("max_search_results", self.caps.max_search_results)?;
        rows("max_label_iris", self.caps.max_label_iris)?;
        rows("max_schema_items", self.caps.max_schema_items)?;

        if self.caps.default_limit > self.caps.max_limit {
            return Err(format!(
                "caps.default_limit is {}, over caps.max_limit of {}; \
                 a request that named no limit would be refused for exceeding one",
                self.caps.default_limit, self.caps.max_limit
            ));
        }
        if self.caps.default_limit > self.caps.max_schema_items {
            return Err(format!(
                "caps.default_limit is {}, over caps.max_schema_items of {}; \
                 a schema request that named no limit would exceed its cap",
                self.caps.default_limit, self.caps.max_schema_items
            ));
        }
        if self.caps.max_limit == 0
            || self.caps.max_sample == 0
            || self.caps.max_bindings == 0
            || self.caps.default_limit == 0
            || self.caps.max_search_predicates == 0
            || self.caps.max_search_results == 0
            || self.caps.max_label_iris == 0
            || self.caps.max_schema_items == 0
        {
            return Err(
                "caps.max_limit, caps.default_limit, caps.max_sample, caps.max_bindings, \
                 caps.max_search_predicates, caps.max_search_results, caps.max_label_iris and \
                 caps.max_schema_items must be at least 1; \
                 a zero-width operation is not usable"
                    .to_owned(),
            );
        }
        if self.budgets.candidate_budget == 0 {
            return Err(
                "budgets.candidate_budget must be at least 1; a zero-width scan cannot advance"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

/// The work one response may cost.
///
/// Separate from [`Caps`] because cap *products* can still be operationally
/// large, and a row cap is not a byte cap — one legal literal can be megabytes.
/// Exhausting a budget is never an error: the response is marked incomplete and
/// carries a cursor where the operation has a resumable position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Budgets {
    /// Rows per response, any operation.
    pub max_output_rows: u64,
    /// RDF terms per response.
    pub max_output_terms: u64,
    /// Serialized response size, pre-compression.
    pub max_response_bytes: u64,
    /// Serialized request input size: enforced on the wire for bodies and
    /// before SPARQL parsing for brTPF's `values=` query parameter.
    pub max_request_bytes: u64,
    /// Any single term or literal, in requests and bindings.
    pub max_term_bytes: u64,
    /// Candidates *examined* by filtered operations.
    pub candidate_budget: u64,
    /// Soft per-request wall clock, in milliseconds (M2).
    pub time_budget_ms: u64,
}

impl Budgets {
    /// The suggested default budgets.
    pub const fn new() -> Self {
        Self {
            max_output_rows: 100_000,
            max_output_terms: 1_000_000,
            max_response_bytes: 64 * 1024 * 1024,
            max_request_bytes: 1024 * 1024,
            max_term_bytes: 64 * 1024,
            candidate_budget: 1_000_000,
            time_budget_ms: 2_000,
        }
    }
}

impl Default for Budgets {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the service and serve it until shutdown.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let bind = config.bind;
    let service = Arc::new(Service::build(config)?);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(
        address = %listener.local_addr()?,
        datasets = service.datasets().names().len(),
        max_concurrent_work = service.config().admission.max_concurrent_work,
        heavy_request_weight = service.config().admission.heavy_request_weight,
        max_queued_requests = service.config().admission.max_queued_requests,
        queue_timeout_ms = service.config().admission.queue_timeout_ms,
        "serving",
    );
    serve_on(listener, service, shutdown_signal()).await
}

/// Serve an already-bound listener until `shutdown` resolves.
///
/// Split from [`serve`] so a caller — a test, or a supervisor handing down a
/// socket — can bind port 0 and learn the address before traffic starts.
///
/// The shutdown trigger is the caller's, and deliberately: `serve` passes
/// [`shutdown_signal`], which installs process-global Ctrl-C and `SIGTERM`
/// handlers, and a library entry point must not do that on an embedder's
/// behalf. It would compete with whatever the host process already has, and it
/// would leave a caller with no way to stop the server at all.
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    service: Arc<Service>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    axum::serve(
        listener,
        routes::router(service).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    Ok(())
}

/// Resolve on Ctrl-C or `SIGTERM`.
///
/// Graceful rather than abrupt because a request in flight holds an
/// `Arc<Store>` over mapped files; letting it finish is both correct and free.
///
/// Public so that an embedder can opt *in* to the same behaviour by passing it
/// to [`serve_on`], rather than having it installed for them.
pub async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            // Nothing to do but keep serving: a server that exited because it
            // could not install a handler would be worse than one that stops
            // only on Ctrl-C.
            Err(error) => {
                tracing::warn!(%error, "no SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published numbers, which is all `validate` reads — a `Config` would
    /// additionally need a capability over a real directory.
    fn config(caps: &Caps, budgets: &Budgets) -> Limits<'static> {
        // Leaked so the borrow outlives the call; a test binary's lifetime.
        Limits {
            caps: Box::leak(Box::new(*caps)),
            budgets: Box::leak(Box::new(*budgets)),
        }
    }

    #[test]
    fn the_published_defaults_are_consistent_with_each_other() {
        assert_eq!(config(&Caps::new(), &Budgets::new()).validate(), Ok(()));
    }

    #[test]
    fn public_bases_are_typed_and_normalized() {
        let root = "https://data.example:8443/".parse::<PublicBase>().unwrap();
        assert_eq!(root.as_str(), "https://data.example:8443");
        assert_eq!(root.origin(), "https://data.example:8443");
        assert_eq!(root.path_prefix(), "");
        assert_eq!(root.mount(), url::Mount::default());

        // A path is the prefix a gateway strips; with or without the trailing
        // slash it is the same deployment.
        let mounted = "https://apps.okn.us/kgf".parse::<PublicBase>().unwrap();
        assert_eq!(mounted.as_str(), "https://apps.okn.us/kgf");
        assert_eq!(mounted.origin(), "https://apps.okn.us");
        assert_eq!(mounted.path_prefix(), "/kgf");
        assert_eq!(mounted.mount().dataset("tox"), "/kgf/tox");
        assert_eq!(
            "https://apps.okn.us/kgf/".parse::<PublicBase>().unwrap(),
            mounted
        );
        // Spelled as typed: the gateway matched these bytes.
        assert_eq!(
            "https://apps.okn.us/a%20b/c"
                .parse::<PublicBase>()
                .unwrap()
                .path_prefix(),
            "/a%20b/c"
        );
        // A segment that merely contains dots is an ordinary segment.
        assert_eq!(
            "https://apps.okn.us/v1.2/...x"
                .parse::<PublicBase>()
                .unwrap()
                .path_prefix(),
            "/v1.2/...x"
        );

        for invalid in [
            "data.example",
            "ftp://data.example",
            "https://data.example/?tenant=a",
            "https://data.example/kgf?x",
            "https://data.example/#fragment",
            "https://data.example/kgf#f",
            "https://data.example/kgf//x",
            "https://data.example/a/../kgf",
            "https://data.example/./kgf",
            "https://data.example/kgf/..",
            "https://data.example/%2e%2e/kgf",
            "https://data.example/a/.%2E/kgf",
            "https://:443",
            "https://user@example.com",
            "https://user@example.com/kgf",
        ] {
            assert!(invalid.parse::<PublicBase>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn a_cap_that_outruns_a_budget_stops_the_server() {
        // The reason `crate::answer` does not check the row and term budgets
        // per request: a configuration that reaches them cannot start. The
        // independent tables must be consistent, and that is an operator error.
        let over_rows = Caps {
            max_limit: 200_000,
            ..Caps::new()
        };
        let refused = config(&over_rows, &Budgets::new()).validate().unwrap_err();
        assert!(refused.contains("max_output_rows"), "{refused}");

        // The term budget bites where the row budget would not: a page of
        // 10 000 describe rows is 30 000 terms, over a 20 000 term budget that
        // leaves `max_output_rows` untouched.
        let tight_terms = Budgets {
            max_output_terms: 20_000,
            ..Budgets::new()
        };
        let refused = config(&Caps::new(), &tight_terms).validate().unwrap_err();
        assert!(refused.contains("max_output_terms"), "{refused}");

        // A sample is capped separately and checked the same way.
        let over_sample = Caps {
            max_sample: 200_000,
            ..Caps::new()
        };
        assert!(config(&over_sample, &Budgets::new()).validate().is_err());

        let over_schema = Caps {
            max_schema_items: 200_000,
            ..Caps::new()
        };
        assert!(config(&over_schema, &Budgets::new()).validate().is_err());

        // A default above its own cap would refuse every request that named no
        // limit, for exceeding a limit it did not name.
        let bad_default = Caps {
            default_limit: 20_000,
            ..Caps::new()
        };
        let refused = config(&bad_default, &Budgets::new())
            .validate()
            .unwrap_err();
        assert!(refused.contains("default_limit"), "{refused}");

        let bad_schema_default = Caps {
            default_limit: 2_000,
            ..Caps::new()
        };
        let refused = config(&bad_schema_default, &Budgets::new())
            .validate()
            .unwrap_err();
        assert!(refused.contains("max_schema_items"), "{refused}");

        for zero in [
            Caps {
                max_limit: 0,
                ..Caps::new()
            },
            Caps {
                max_sample: 0,
                ..Caps::new()
            },
            Caps {
                max_bindings: 0,
                ..Caps::new()
            },
            Caps {
                default_limit: 0,
                ..Caps::new()
            },
            Caps {
                max_schema_items: 0,
                ..Caps::new()
            },
        ] {
            assert!(config(&zero, &Budgets::new()).validate().is_err());
        }
    }

    #[test]
    fn max_response_bytes_is_deliberately_not_validated() {
        // It cannot be: a row cap is not a byte cap because one legal literal
        // can be megabytes, so no combination of caps bounds it
        // and it has to be applied while a response is built. Pinned here so
        // that adding it to `validate` — which would look like tidiness — has
        // to argue with this comment first.
        let tiny = Budgets {
            max_response_bytes: 1,
            ..Budgets::new()
        };
        assert_eq!(config(&Caps::new(), &tiny).validate(), Ok(()));
    }
}
