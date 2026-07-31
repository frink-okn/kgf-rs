//! One open, immutable bundle version.
//!
//! `Store` is `Send + Sync`, every method takes `&self`, and there is **no
//! interior mutability and no lock anywhere on the read path**. Thread safety
//! is by construction rather than by discipline: after `open` returns, nothing
//! about a `Store` changes until it is dropped.
//!
//! Caching belongs to the server. A page of results repeats predicates and IRIs
//! constantly and is worth a per-request term cache — but that cache is
//! request-scoped state, and putting it here would mean a lock on the hot path
//! for a benefit the server can have for free.

use std::path::{Path, PathBuf};

use crate::dict::Dictionary;
use crate::error::Result;
use crate::pattern::{IdPattern, Selection};
use crate::perm::Permutations;

/// Names of the artifacts the store knows about.
pub mod artifact {
    /// The triples and dictionary. Required.
    pub const HDT: &str = "data.hdt";
    /// POS + OPS permutations and all six rank directories. Required.
    pub const PERM: &str = "data.hdt.perm";
    /// Graph dictionary and membership layers. Optional; gates `graphs`.
    pub const GRAPHS: &str = "data.hdt.graphs";
    /// POS/OPS-keyed membership layers. Required whenever [`GRAPHS`] is present.
    pub const GRAPHS_IDX: &str = "data.hdt.graphs.idx";
}

/// Open-time options.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Verify every region's CRC32C at open.
    ///
    /// Off by default and expected to stay off in production: full verification
    /// is a publish-time and `kgf verify` concern, and doing it at open would
    /// read every byte of every artifact — the one thing lazy open exists to
    /// avoid (doc 20 §20.6). Cheap binding checks always run.
    pub verify_checksums: bool,
}

/// An open bundle version.
#[derive(Debug)]
pub struct Store {
    _dir: PathBuf,
    _dict: Dictionary,
    _perms: Permutations,
}

impl Store {
    /// Open the bundle version rooted at `dir`.
    ///
    /// Maps files and parses headers — **no data pages are touched**, because
    /// rank directories are persisted rather than derived. Cheap binding checks
    /// (suffix lengths, triple counts, dictionary counts) run for every sidecar;
    /// digests and CRCs do not unless [`OpenOptions::verify_checksums`] asks.
    ///
    /// Fails if a required artifact is missing, or if `data.hdt.graphs` is
    /// present without `data.hdt.graphs.idx`. There is no degraded mode: the
    /// error names the command that produces what is missing (doc 20 §20.8).
    pub fn open(_dir: &Path, _opts: OpenOptions) -> Result<Self> {
        todo!("check required artifacts, map data.hdt and data.hdt.perm, parse headers")
    }

    /// The dictionary.
    pub fn dict(&self) -> &Dictionary {
        &self._dict
    }

    /// The permutations.
    pub fn perms(&self) -> &Permutations {
        &self._perms
    }

    /// Total triples in the bundle.
    pub fn triples(&self) -> u64 {
        todo!("read from the HDT header parsed at open")
    }

    /// Resolve a pattern. `O(log N)`; enumerates nothing.
    pub fn resolve(&self, _pattern: IdPattern) -> Result<Selection> {
        todo!("delegate to pattern::resolve with this store's permutations")
    }
}
