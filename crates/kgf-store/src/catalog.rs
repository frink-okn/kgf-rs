//! Lazy multi-tenant bundle catalog.
//!
//! A server points at a directory of bundles and serves all of them. The
//! catalog scans at startup and **opens nothing**; a version opens on its first
//! request, and opening is cheap enough that this is invisible (doc 20 §20.6).
//!
//! # Why this is only a map
//!
//! Reads take an `Arc<Store>` and never a lock. The catalog map is the only
//! synchronized structure in the crate, and it is touched once per request to
//! clone an `Arc` — never during evaluation. Eviction is dropping the `Arc`:
//! in-flight requests finish on their clone and the maps unmap when the last
//! one goes.
//!
//! Because an idle open bundle costs address space rather than resident memory,
//! eviction policy is about file descriptors and hygiene, not memory pressure.
//! Historical versions are ordinary entries; nothing distinguishes them from
//! current ones.
//!
//! # Budgets to watch
//!
//! Roughly one file descriptor per artifact per open bundle (raise `ulimit -n`
//! in deployment), and Linux's `vm.max_map_count` at hundreds of bundles. Both
//! are deployment notes, not design constraints.

use std::path::Path;
use std::sync::Arc;

use crate::error::Result;
use crate::store::{OpenOptions, Store};

/// A dataset and version, the catalog's key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BundleId {
    /// Dataset identifier.
    pub dataset: String,
    /// Version identifier.
    pub version: String,
}

/// The set of known bundles, opened on demand.
#[derive(Debug)]
pub struct Catalog {
    _root: std::path::PathBuf,
}

impl Catalog {
    /// Scan `root` for `{dataset}/{version}/` directories without opening any.
    pub fn scan(_root: &Path, _opts: OpenOptions) -> Result<Self> {
        todo!("enumerate dataset/version directories; open nothing")
    }

    /// Every known bundle, whether open or not.
    pub fn ids(&self) -> Vec<BundleId> {
        todo!("list the scanned entries")
    }

    /// Get a bundle, opening it if this is its first request.
    ///
    /// A singleflight guard means concurrent first requests for the same
    /// version open it once rather than N times.
    pub fn get(&self, _id: &BundleId) -> Result<Arc<Store>> {
        todo!("fast path on the map; singleflight the open")
    }

    /// Drop the catalog's reference to a bundle.
    ///
    /// In-flight requests finish on their own clone; the mapping is released
    /// when the last `Arc` goes.
    pub fn evict(&self, _id: &BundleId) {
        todo!("remove the entry from the map")
    }
}
