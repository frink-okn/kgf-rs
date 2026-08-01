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
//! design ships two paths, because the web does.
//!
//! # Status
//!
//! [`cursor`] and [`term`] are implemented (`notes/plan.md` units 10–11). The
//! response envelope and the routes are units 12–14; [`serve`] is still
//! `todo!()`, which is the convention here rather than an oversight — an
//! unimplemented path panics rather than returning a plausible wrong answer.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod cursor;
pub mod term;

/// Server configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory of bundles to serve.
    pub bundle_root: std::path::PathBuf,
    /// Address to bind.
    pub bind: std::net::SocketAddr,
}

/// Serve until shutdown.
pub async fn serve(_config: Config) -> anyhow::Result<()> {
    todo!("build the catalog, mount the doc 03 routes, run the listener")
}
