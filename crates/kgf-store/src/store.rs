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

use hdtc::format::GraphIndexOpenError;

use crate::dict::Dictionary;
use crate::error::{Error, Result};
use crate::map::open_published;
use crate::pattern::{IdPattern, Selection};
use crate::perm::Permutations;

/// Names of the artifacts the store knows about.
pub mod artifact {
    /// Bundle identity, components, capabilities, and checksums. Required.
    pub const MANIFEST: &str = "manifest.json";
    /// The triples and dictionary. Required.
    pub const HDT: &str = "data.hdt";
    /// POS + OPS permutations and all six rank directories. Required.
    pub const PERM: &str = "data.hdt.perm";
    /// Graph dictionary and membership layers. Optional; gates `graphs`.
    pub const GRAPHS: &str = "data.hdt.graphs";
    /// POS/OPS-keyed membership layers. Required whenever [`GRAPHS`] is present.
    pub const GRAPHS_IDX: &str = "data.hdt.graphs.idx";
}

/// Open-time options, reserved for options that preserve header-only opening.
///
/// Full digest and checksum verification deliberately does not belong here: it
/// runs at publish/registry ingest and through `kgf verify` (doc 20 §20.6).
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct OpenOptions {}

/// An open bundle version.
///
/// Holds the mappings and the **specs** validated against them at open
/// ([`crate::map`]), never views: a view borrows from a mapping this struct
/// owns, so holding both would be self-referential. Callers project what they
/// need for the duration of a query, which costs a bounds compare and a slice
/// and needs no synchronisation.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    perms: Permutations,
}

impl Store {
    /// Open the bundle version rooted at `dir`.
    ///
    /// Maps files and parses headers — **no payload pages are scanned**, because
    /// rank directories are persisted rather than derived. Cheap binding checks
    /// (suffix lengths, triple counts, dictionary counts) run for every sidecar;
    /// full digests and CRCs stay on the publish/`kgf verify` path.
    ///
    /// Fails if a required artifact is missing, or if `data.hdt.graphs` is
    /// present without `data.hdt.graphs.idx`. There is no degraded mode: the
    /// error names the command that produces what is missing (doc 20 §20.8).
    ///
    /// Published bundle versions are immutable (doc 04 §4.6). Callers must not
    /// modify or truncate this directory while the returned store is alive; that
    /// environmental guarantee is the soundness condition for its read-only maps.
    pub fn open(dir: &Path, _opts: OpenOptions) -> Result<Self> {
        require_file(dir, artifact::MANIFEST, "kgf build")?;
        let hdt_path = require_file(dir, artifact::HDT, "kgf build")?;
        let perm_path = require_file(
            dir,
            artifact::PERM,
            format!("hdtc perm {}", hdt_path.display()),
        )?;

        let graph_index_path = if optional_file(dir, artifact::GRAPHS)?.is_some() {
            Some(require_file(
                dir,
                artifact::GRAPHS_IDX,
                format!("hdtc graphs-index {}", hdt_path.display()),
            )?)
        } else {
            None
        };

        let hdt = open_published(&hdt_path)?;
        let perm = open_published(&perm_path)?;
        let perms = Permutations::open(hdt, perm)?;

        if let Some(graph_index_path) = graph_index_path {
            hdtc::format::GraphIndex::open(&graph_index_path, &hdt_path).map_err(|error| {
                match error {
                    GraphIndexOpenError::Binding { source } => Error::ArtifactBindingMismatch {
                        artifact: graph_index_path.clone(),
                        hdt: hdt_path.clone(),
                        detail: format!("{source:#}"),
                    },
                    GraphIndexOpenError::Index { source } => Error::Format(source.context(
                        format!("opening graph index {}", graph_index_path.display()),
                    )),
                    GraphIndexOpenError::Source { source } => Error::Format(
                        source.context(format!("validating source HDT {}", hdt_path.display())),
                    ),
                    GraphIndexOpenError::Sidecar { source } => {
                        Error::Format(source.context(format!(
                            "opening graph sidecar {}",
                            dir.join(artifact::GRAPHS).display()
                        )))
                    }
                }
            })?;
        }

        Ok(Self {
            dir: dir.to_path_buf(),
            perms,
        })
    }

    /// The immutable bundle-version directory backing this store.
    pub fn bundle_dir(&self) -> &Path {
        &self.dir
    }

    /// The dictionary.
    pub fn dict(&self) -> Dictionary<'_> {
        self.perms
            .hdt_layout()
            .dictionary()
            .view(self.perms.hdt_mapping())
    }

    /// The permutations.
    pub fn perms(&self) -> &Permutations {
        &self.perms
    }

    /// Total triples in the bundle.
    pub fn triples(&self) -> u64 {
        self.perms.triples()
    }

    /// Resolve a pattern. `O(log N)`; enumerates nothing.
    ///
    /// The returned [`Selection`] borrows this store, which is what makes
    /// "resolved against a different bundle" unrepresentable. Query execution is
    /// synchronous within one blocking task holding an `Arc<Store>` (doc 20
    /// §20.4), so nothing needs to outlive the borrow; resumption goes through
    /// an encoded cursor token, not a live `Selection`.
    pub fn resolve(&self, pattern: IdPattern) -> Result<Selection<'_>> {
        crate::pattern::resolve(&self.perms, pattern)
    }
}

fn require_file(dir: &Path, name: &str, remedy: impl Into<String>) -> Result<PathBuf> {
    optional_file(dir, name)?.ok_or_else(|| Error::MissingRequiredArtifact {
        bundle: dir.to_path_buf(),
        artifact: name.to_owned(),
        remedy: remedy.into(),
    })
}

fn optional_file(dir: &Path, name: &str) -> Result<Option<PathBuf>> {
    let path = dir.join(name);
    if !path.try_exists()? {
        return Ok(None);
    }
    if !std::fs::metadata(&path)?.is_file() {
        return Err(Error::Malformed {
            artifact: path,
            detail: "artifact is not a regular file".to_owned(),
        });
    }
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::IdPattern;
    use crate::testing::{Fixture, TINY_NT};

    #[test]
    fn a_complete_bundle_opens_and_answers_through_the_store() {
        let fixture = Fixture::build(TINY_NT);
        let store = Store::open(fixture.bundle_path(), OpenOptions::default()).unwrap();

        assert_eq!(store.bundle_dir(), fixture.bundle_path());
        assert_eq!(store.triples(), 8);
        let selection = store
            .resolve(IdPattern {
                subject: None,
                predicate: None,
                object: None,
            })
            .unwrap();
        assert_eq!(selection.count().value, 8);
        assert_eq!(selection.page(0, usize::MAX).count(), 8);
    }

    #[test]
    fn required_artifact_errors_name_what_to_build() {
        let fixture = Fixture::build(TINY_NT);
        let root = tempfile::tempdir().unwrap();
        let cases = [
            (artifact::MANIFEST, "kgf build"),
            (artifact::HDT, "kgf build"),
            (artifact::PERM, "hdtc perm"),
        ];

        for (missing, command) in cases {
            let bundle = root.path().join(missing.replace('.', "-"));
            fixture.copy_bundle_to(&bundle);
            std::fs::remove_file(bundle.join(missing)).unwrap();
            match Store::open(&bundle, OpenOptions::default()).expect_err("must refuse bundle") {
                Error::MissingRequiredArtifact {
                    bundle: actual_bundle,
                    artifact,
                    remedy,
                } => {
                    assert_eq!(actual_bundle, bundle);
                    assert_eq!(artifact, missing);
                    assert!(remedy.starts_with(command), "{remedy}");
                }
                other => panic!("unexpected error: {other}"),
            }
        }

        let bundle = root.path().join("graphs-without-index");
        fixture.copy_bundle_to(&bundle);
        std::fs::write(bundle.join(artifact::GRAPHS), b"present").unwrap();
        match Store::open(&bundle, OpenOptions::default()).expect_err("must require graph index") {
            Error::MissingRequiredArtifact {
                artifact: actual_artifact,
                remedy,
                ..
            } => {
                assert_eq!(actual_artifact, artifact::GRAPHS_IDX);
                assert!(remedy.starts_with("hdtc graphs-index"), "{remedy}");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn store_open_preserves_permutation_binding_errors() {
        let fixture = Fixture::build(TINY_NT);
        let other = Fixture::build(&format!(
            "{TINY_NT}<http://example.org/extra> <http://example.org/p> <http://example.org/o> .\n"
        ));
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("mismatched");
        fixture.copy_bundle_to(&bundle);
        std::fs::copy(other.perm_path(), bundle.join(artifact::PERM)).unwrap();

        assert!(matches!(
            Store::open(&bundle, OpenOptions::default()),
            Err(Error::ArtifactBindingMismatch { .. })
        ));
    }

    #[test]
    fn store_open_preserves_graph_index_binding_errors() {
        let first = Fixture::build_quads(concat!(
            "<http://example.org/s> <http://example.org/p> <http://example.org/o> ",
            "<http://example.org/g> .\n",
        ));
        let other = Fixture::build_quads(concat!(
            "<http://example.org/s> <http://example.org/p> <http://example.org/o> ",
            "<http://example.org/g> .\n",
            "<http://example.org/extra> <http://example.org/p> <http://example.org/o> ",
            "<http://example.org/other-graph> .\n",
        ));
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("mismatched-graph-index");
        first.copy_bundle_to(&bundle);
        std::fs::copy(
            other.bundle_path().join(artifact::GRAPHS_IDX),
            bundle.join(artifact::GRAPHS_IDX),
        )
        .unwrap();

        match Store::open(&bundle, OpenOptions::default())
            .expect_err("a graph index from another HDT must not bind")
        {
            Error::ArtifactBindingMismatch { artifact, hdt, .. } => {
                assert_eq!(artifact, bundle.join(artifact::GRAPHS_IDX));
                assert_eq!(hdt, bundle.join(artifact::HDT));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn store_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Store>();
    }
}
