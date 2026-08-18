//! `kgf serve` — run the doc 03 API over a directory of bundles.
//!
//! # Where the mmap promise is made
//!
//! [`kgf_store::map`] confines the `unsafe` that *maps* a file. What it cannot
//! confine is the promise that makes mapping sound: that the files under the
//! bundle root are published and will not be modified or truncated while the
//! server holds them (doc 04 §4.6). No library can establish that — it is a
//! fact about a deployment — so [`PublishedBundle::new`] and
//! [`PublishedRoot::new`] are `unsafe` constructors whose entire purpose is to
//! be called from outside `kgf-store`, by the layer that knows.
//!
//! That layer is this one. `kgf-server` deliberately takes a [`PublishedRoot`]
//! rather than a path, because it is a library that can be embedded, and a safe
//! `&Path`-taking entry point there would make the promise on an unknown
//! caller's behalf. Here there is no ambiguity: the operator typed
//! `--bundle-root`, and running a KGF server over a tree that is rewritten
//! underneath it is already unsupported.
//!
//! So this crate carries the project's second `unsafe`, in
//! [`published_root`] and nowhere else, and `CLAUDE.md` records it beside
//! `map`'s.
//!
//! [`PublishedBundle::new`]: kgf_store::map::PublishedBundle::new
//! [`PublishedRoot::new`]: kgf_store::map::PublishedRoot::new

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use kgf_server::{Admission, Config, PublicOrigin};
use kgf_store::map::PublishedRoot;

/// Arguments for `kgf serve`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Directory of bundles, laid out as {root}/{dataset}/{version}.
    #[arg(long)]
    pub bundle_root: PathBuf,

    /// Address to bind. Port 0 binds an ephemeral port, logged at startup.
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,

    /// Trusted external origin for Hydra IRIs when serving behind a TLS or host-rewriting proxy.
    #[arg(long, value_name = "ORIGIN")]
    pub public_origin: Option<PublicOrigin>,

    /// Concurrent ordinary bundle-work units; heavy requests consume multiple units.
    #[arg(long, default_value_t = 32)]
    pub max_concurrent_work: u32,

    /// Work units consumed by search, sample, and other candidate-heavy requests.
    #[arg(long, default_value_t = 4)]
    pub heavy_request_weight: u32,

    /// Requests allowed to wait after all concurrent-work units are occupied.
    #[arg(long, default_value_t = 128)]
    pub max_queued_requests: u32,

    /// Milliseconds a queued request waits before receiving HTTP 429.
    #[arg(long, default_value_t = 500)]
    pub queue_timeout_ms: u64,
}

/// Serve until Ctrl-C or `SIGTERM`.
pub fn run(args: Args) -> Result<()> {
    let mut config = Config::new(published_root(&args.bundle_root)?, args.bind);
    config.public_origin = args.public_origin;
    config.admission = Admission {
        max_concurrent_work: args.max_concurrent_work,
        heavy_request_weight: args.heavy_request_weight,
        max_queued_requests: args.max_queued_requests,
        queue_timeout_ms: args.queue_timeout_ms,
    };

    // A current-thread runtime would serialize every request behind the one
    // that is faulting a page. The blocking pool that store work runs on is
    // this runtime's (doc 20 §20.4).
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("start the async runtime")?
        .block_on(kgf_server::serve(config))
}

/// Assert that `root` is a published, immutable bundle tree.
///
/// Canonicalized first, so the capability names one resolved directory for the
/// life of the process rather than a path that a replaced symlink could point
/// somewhere else — which is one of the three things the invariant asks for.
///
/// The directory check is not the safety argument; it is only a decent error
/// message. The safety argument is the operator's, recorded in the module docs
/// above.
#[allow(unsafe_code)]
pub fn published_root(root: &Path) -> Result<PublishedRoot> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("bundle root {} cannot be resolved", root.display()))?;
    if !canonical.is_dir() {
        bail!(
            "bundle root {} is not a directory; it should hold {{dataset}}/{{version}}/ bundles",
            canonical.display()
        );
    }

    // SAFETY: the caller of `kgf serve` has published this tree. Doc 04 §4.6
    // makes a published bundle version immutable: its artifacts are written
    // once, before the directory is served, and a new release is a new version
    // directory rather than an edit. Adding version directories beneath the
    // root while running is explicitly permitted by this constructor, and is
    // the only mutation a normal deployment performs.
    Ok(unsafe { PublishedRoot::new(&canonical) })
}
