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
use crate::map::{PublishedBundle, open_published};
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

/// Open-time options, reserved for options that preserve bounded,
/// size-independent opening.
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
    bundle: PublishedBundle,
    perms: Permutations,
}

impl Store {
    /// Open a bundle version whose immutability has been established.
    ///
    /// Maps files, parses headers, and performs a bounded number of metadata and
    /// rank-sentinel reads independent of bundle size. It does not scan payload
    /// regions, rebuild indexes, or compute full digests and CRCs.
    ///
    /// Fails if a required artifact is missing, or if exactly one of
    /// `data.hdt.graphs` and `data.hdt.graphs.idx` is present. There is no
    /// degraded mode: the error names the command that produces a missing
    /// required artifact (doc 20 §20.8).
    ///
    /// Constructing [`PublishedBundle`] is where the caller acknowledges doc 04
    /// §4.6's external immutability requirement. This method is safe because it
    /// cannot be called with an unqualified path.
    pub fn open(bundle: &PublishedBundle, _opts: OpenOptions) -> Result<Self> {
        let dir = bundle.path();
        // The manifest is what makes an artifact set a *bundle* (doc 04 §4.1),
        // so it is required here and not by `ArtifactSet::resolve`, which the
        // manifest generator uses on a bundle that does not have one yet.
        require_file(dir, artifact::MANIFEST, "kgf manifest")?;
        let artifacts = ArtifactSet::resolve(dir)?;

        let hdt = open_published(bundle, &artifacts.hdt)?;
        let perm = open_published(bundle, &artifacts.perm)?;
        let perms = Permutations::open(hdt, perm)?;

        artifacts.verify_graph_index()?;

        Ok(Self {
            bundle: bundle.clone(),
            perms,
        })
    }

    /// The immutable bundle-version directory backing this store.
    pub fn bundle_dir(&self) -> &Path {
        self.bundle.path()
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

/// The artifact paths a bundle directory provides, resolved and checked against
/// doc 04 §4.1's required set.
///
/// Deliberately says nothing about `manifest.json`. A bundle needs one to be
/// servable, but [`crate::manifest`] reads these same artifacts in order to
/// *produce* that manifest, and demanding one there would be circular. Resolving
/// the set is therefore separate from the check that a set is a complete bundle,
/// which is [`Store::open`]'s first line.
#[derive(Debug, Clone)]
pub(crate) struct ArtifactSet {
    pub(crate) hdt: PathBuf,
    pub(crate) perm: PathBuf,
    pub(crate) graphs: Option<PathBuf>,
    pub(crate) graph_index: Option<PathBuf>,
}

impl ArtifactSet {
    /// Resolve `dir`'s artifacts, requiring the ones with no fallback.
    ///
    /// The graph sidecar and its index must occur together: an index without
    /// its parent is malformed, and a parent without its index would leave the
    /// three index-side patterns to a per-candidate probe this crate does not
    /// implement (doc 20 §20.7, §20.8).
    pub(crate) fn resolve(dir: &Path) -> Result<Self> {
        let hdt = require_file(dir, artifact::HDT, "kgf build")?;
        let perm = require_file(dir, artifact::PERM, format!("hdtc perm {}", hdt.display()))?;

        let graphs = optional_file(dir, artifact::GRAPHS)?;
        let graph_index = optional_file(dir, artifact::GRAPHS_IDX)?;
        match (&graphs, &graph_index) {
            (Some(_), None) => {
                return Err(Error::MissingRequiredArtifact {
                    bundle: dir.to_path_buf(),
                    artifact: artifact::GRAPHS_IDX.to_owned(),
                    remedy: format!("hdtc graphs-index {}", hdt.display()),
                });
            }
            (None, Some(index)) => {
                return Err(Error::Malformed {
                    artifact: index.clone(),
                    detail: format!(
                        "graph index is present without its required parent {}",
                        dir.join(artifact::GRAPHS).display()
                    ),
                });
            }
            _ => {}
        }

        Ok(Self {
            hdt,
            perm,
            graphs,
            graph_index,
        })
    }

    /// Refuse a graph index that does not belong to this HDT.
    ///
    /// `verify_binding` rather than `open`: graph scoping is a later milestone,
    /// so what open owes a bundle today is a refusal when its index does not
    /// bind (doc 20 §20.8). Opening the index would additionally build its two
    /// per-query layer readers — a file handle each — and then drop them.
    ///
    /// Shared with [`crate::manifest::BundleFacts::read`] so that the two paths
    /// cannot disagree about whether a bundle is sound. A manifest describing a
    /// bundle that then refuses to open would be worse than useless.
    pub(crate) fn verify_graph_index(&self) -> Result<()> {
        let Some(index) = &self.graph_index else {
            return Ok(());
        };
        hdtc::format::GraphIndex::verify_binding(index, &self.hdt).map_err(|error| match error {
            GraphIndexOpenError::Binding { source } => Error::ArtifactBindingMismatch {
                artifact: index.clone(),
                hdt: self.hdt.clone(),
                detail: format!("{source:#}"),
            },
            GraphIndexOpenError::Index { source } => {
                Error::Format(source.context(format!("opening graph index {}", index.display())))
            }
            GraphIndexOpenError::Source { source } => Error::Format(
                source.context(format!("validating source HDT {}", self.hdt.display())),
            ),
            GraphIndexOpenError::Sidecar { source } => {
                let graphs = self
                    .graphs
                    .as_deref()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from(artifact::GRAPHS));
                Error::Format(source.context(format!("opening graph sidecar {}", graphs.display())))
            }
        })
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
    use crate::testing::{Fixture, TINY_NT, published_bundle};

    #[test]
    fn a_complete_bundle_opens_and_answers_through_the_store() {
        let fixture = Fixture::build(TINY_NT);
        let bundle = published_bundle(fixture.bundle_path());
        let store = Store::open(&bundle, OpenOptions::default()).unwrap();

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
            // The manifest names `kgf manifest` rather than `kgf build`: it is
            // the one required artifact that can be produced for a bundle whose
            // data is already built, which is what makes hand-assembly work.
            (artifact::MANIFEST, "kgf manifest"),
            (artifact::HDT, "kgf build"),
            (artifact::PERM, "hdtc perm"),
        ];

        for (missing, command) in cases {
            let bundle = root.path().join(missing.replace('.', "-"));
            fixture.copy_bundle_to(&bundle);
            std::fs::remove_file(bundle.join(missing)).unwrap();
            let published = published_bundle(&bundle);
            match Store::open(&published, OpenOptions::default()).expect_err("must refuse bundle") {
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
        let published = published_bundle(&bundle);
        match Store::open(&published, OpenOptions::default()).expect_err("must require graph index")
        {
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

        let bundle = root.path().join("index-without-graphs");
        fixture.copy_bundle_to(&bundle);
        std::fs::write(bundle.join(artifact::GRAPHS_IDX), b"orphan").unwrap();
        let published = published_bundle(&bundle);
        match Store::open(&published, OpenOptions::default())
            .expect_err("must reject a graph index without its sidecar parent")
        {
            Error::Malformed { artifact, detail } => {
                assert_eq!(artifact, bundle.join(artifact::GRAPHS_IDX));
                assert!(detail.contains(artifact::GRAPHS), "{detail}");
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

        let published = published_bundle(&bundle);
        assert!(matches!(
            Store::open(&published, OpenOptions::default()),
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

        let published = published_bundle(&bundle);
        match Store::open(&published, OpenOptions::default())
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
