//! The KGF HTTP API.
//!
//! This crate implements **KGF doc 03** over [`kgf_store`]. The split is the
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
//! design ships two paths, because the web does. M1 has no body-carrying route,
//! but the router dispatches on the method rather than filtering to a fixed set,
//! so a QUERY reaches a handler today: routes that do not take one answer
//! `method_not_allowed` with an `Allow` header, which is the proof that the
//! stack can express the method at all.
//!
//! # One URL, two readers
//!
//! Every route answers JSON *and* HTML, chosen by `Accept` alone — a page in a
//! browser, data from `curl`, at the same URL. See [`representation`] for why
//! the tie-break falls the way it does, and [`html`] for the rendering.
//!
//! # Status
//!
//! Units 10–14 of `notes/plan.md` are implemented: [`cursor`], [`term`],
//! [`envelope`], the URL space with `latest`, caching and content negotiation,
//! and doc 03 §3.4's four read operations — `/fragment`, `/count`, `/describe`
//! and `/sample` — in [`request`] and [`answer`]. Bindings QUERY, `/void` and
//! `/summary` are M2, so a deployment answers useful traffic but does not yet
//! claim §3.1's core profile.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod answer;
pub mod cursor;
pub mod descriptor;
pub mod envelope;
pub mod html;
pub mod representation;
pub mod request;
pub mod routes;
pub mod service;
pub mod term;
pub mod url;

use std::sync::Arc;

use kgf_store::map::PublishedRoot;
use serde::Serialize;

use crate::service::Service;

/// Server configuration.
///
/// Also the document published at `/`: doc 03 §3.1 tells clients to read the
/// caps and budgets rather than assume them, which is only true if the values
/// they read are the values applied.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory of bundles, laid out as `{root}/{dataset}/{version}`.
    ///
    /// A [`PublishedRoot`] rather than a path: mapping a file that another
    /// process can truncate is undefined behaviour, and this capability is
    /// where a caller records that the tree is published and immutable (doc 04
    /// §4.6). Taking a `&Path` here would make that promise on the operator's
    /// behalf, from a library that can be embedded anywhere.
    pub bundle_root: PublishedRoot,
    /// Address to bind.
    pub bind: std::net::SocketAddr,
    /// The largest values a request may ask for (doc 03 §3.5).
    pub caps: Caps,
    /// The work one response may cost (doc 03 §3.5).
    pub budgets: Budgets,
}

impl Config {
    /// A configuration serving `bundle_root` on `bind`, with doc 03 §3.5's
    /// default caps and budgets.
    pub fn new(bundle_root: PublishedRoot, bind: std::net::SocketAddr) -> Self {
        Self {
            bundle_root,
            bind,
            caps: Caps::default(),
            budgets: Budgets::default(),
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

/// The largest values a request may ask for (doc 03 §3.5).
///
/// A request above a cap is refused with `cap_exceeded` rather than quietly
/// reduced: a client that asked for 50 000 rows and got 10 000 without being
/// told has been handed a truncated answer that claims to be complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Caps {
    /// Rows per page.
    pub max_limit: u32,
    /// Rows per page when a request does not ask for a number.
    ///
    /// Not one of doc 03 §3.5's caps, and published beside them because a
    /// client cannot otherwise know how large `GET /fragment` is: §3.5 fixes
    /// the ceiling and leaves the default to the server, so the default is
    /// something a server has to say. Small on purpose — an agent's first
    /// request to an unfamiliar endpoint should be cheap, and `next` says
    /// there is more. See `notes/plan.md`, "Questions for `../kgf`".
    pub default_limit: u32,
    /// Members drawn by one `/sample` (doc 03 §3.5's `n ≤ 1000`).
    pub max_sample: u32,
    /// Input rows in a bindings QUERY (M2).
    pub max_bindings: u32,
    /// Subjects in a star request (M2).
    pub max_star_subjects: u32,
    /// Predicates per star subject (M2).
    pub max_star_width: u32,
}

impl Caps {
    /// Doc 03 §3.5's defaults.
    ///
    /// `const` so that a caller — or a test of the parsing these bound — can
    /// name them without building a [`Config`], which needs a capability over a
    /// real directory.
    pub const fn new() -> Self {
        Self {
            max_limit: 10_000,
            default_limit: 100,
            max_sample: 1_000,
            max_bindings: 200,
            max_star_subjects: 1_000,
            max_star_width: 32,
        }
    }
}

impl Default for Caps {
    fn default() -> Self {
        Self::new()
    }
}

/// The published numbers a request is read against (doc 03 §3.5).
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

/// The work one response may cost (doc 03 §3.5's composite budgets).
///
/// Separate from [`Caps`] because cap *products* can still be operationally
/// large, and a row cap is not a byte cap — one legal literal can be megabytes.
/// Exhausting a budget is never an error: the response is marked incomplete and
/// carries a cursor (§3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Budgets {
    /// Rows per response, any operation.
    pub max_output_rows: u64,
    /// RDF terms per response.
    pub max_output_terms: u64,
    /// Serialized response size, pre-compression.
    pub max_response_bytes: u64,
    /// Request body size, enforced on the wire by the router.
    pub max_request_bytes: u64,
    /// Any single term or literal, in requests and bindings.
    pub max_term_bytes: u64,
    /// Rows or postings *examined* by filtered operations (M2).
    pub candidate_budget: u64,
    /// Soft per-request wall clock, in milliseconds (M2).
    pub time_budget_ms: u64,
}

impl Budgets {
    /// Doc 03 §3.5's suggested defaults.
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
    axum::serve(listener, routes::router(service))
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
