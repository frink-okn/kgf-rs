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

use hdtc::format::{GraphIndexOpenError, TextSearcher};

use crate::description::DescriptionStore;
use crate::dict::Dictionary;
use crate::error::{Error, Result};
use crate::indexed::IndexedHdt;
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
    /// Full-text index over the dictionary's literals. Optional; gates `search`.
    ///
    /// **A directory, not a file** — the only artifact that is. Its bytes are
    /// Tantivy's rather than hdtc's (`hdtc/docs/text-index-format.md` §1.1), so
    /// it is a set of segment files whose names the build chooses. Doc 04 §4.1
    /// places it and §4.3 says how one entry checksums the whole directory.
    pub const TEXT: &str = "data.hdt.text";
    /// VoID description graph as standard HDT. Part of the tier-1 description set.
    pub const VOID_HDT: &str = "stats/void.hdt";
    /// Permutations and rank directories for [`VOID_HDT`].
    pub const VOID_PERM: &str = "stats/void.hdt.perm";
    /// Semantic schema selector to final VoID subject-id index.
    pub const SCHEMA_NODES: &str = "stats/schema-nodes.tsv";
    /// Count-ranked class relation projection.
    pub const CLASS_RELATIONS: &str = "stats/class-relations.tsv";
    /// Count-ranked properties used by each observed subject class.
    pub const CLASS_PROPERTIES: &str = "stats/class-properties.tsv";
    /// Per-role namespace inventory.
    pub const NAMESPACES: &str = "stats/namespaces.json";
    /// Structured LLM summary card.
    pub const SUMMARY_JSON: &str = "stats/summary.json";
    /// Rendered Markdown LLM summary card.
    pub const SUMMARY_MD: &str = "stats/summary.md";

    /// Every `filters/` file the sketch convention can produce.
    ///
    /// Enumerated rather than discovered by listing the directory, because a
    /// manifest's artifact set must be a closed vocabulary: a stray file in
    /// `filters/` is not an artifact, and checksumming whatever happens to be
    /// there would let one appear in `content_digest`. Doc 17 §17.3 restricts
    /// the sketch families to the subject and object roles, so this is the
    /// complete space; which of them a bundle carries is discovered.
    pub const FILTERS: [&str; 4] = [
        "filters/objects.filter",
        "filters/objects.minhash",
        "filters/subjects.filter",
        "filters/subjects.minhash",
    ];

    /// Every `keysets/` file the key-set profile can produce.
    ///
    /// Doc 18 §18.4 publishes the disjoint trio by default but keeps the
    /// composite roles available, and says consumers "must not assume exactly
    /// two key-set files per bundle" — so all six roles are listed and presence
    /// decides. hdtc's experimental `terms` role is deliberately absent: doc 18
    /// §18.4 excludes it from the KGF profile.
    pub const KEYSETS: [&str; 6] = [
        "keysets/objects-only.keys",
        "keysets/objects.keys",
        "keysets/predicates.keys",
        "keysets/shared.keys",
        "keysets/subjects-only.keys",
        "keysets/subjects.keys",
    ];

    /// The complete tier-1 description artifact set, in bundle-layout order.
    pub const DESCRIPTION: [&str; 8] = [
        VOID_HDT,
        VOID_PERM,
        SCHEMA_NODES,
        CLASS_RELATIONS,
        CLASS_PROPERTIES,
        NAMESPACES,
        SUMMARY_JSON,
        SUMMARY_MD,
    ];
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
pub struct Store {
    bundle: PublishedBundle,
    data: IndexedHdt,
    description: Option<DescriptionStore>,
    text: Option<TextSearcher>,
}

impl std::fmt::Debug for Store {
    /// Written out rather than derived, because a [`TextSearcher`] is a search
    /// engine and has no `Debug` — and would be noise if it had one. What is
    /// worth seeing in a log line is which bundle this is and what it can
    /// answer.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("bundle", &self.bundle)
            .field("data", &self.data)
            .field("description", &self.description.is_some())
            .field("text", &self.text.is_some())
            .finish()
    }
}

impl Store {
    /// Open a bundle version whose immutability has been established.
    ///
    /// Maps files, parses headers, and performs a bounded number of metadata and
    /// rank-sentinel reads independent of bundle size. It does not scan payload
    /// regions, rebuild indexes, or compute full digests and CRCs.
    ///
    /// Fails if a required artifact is missing, if exactly one of
    /// `data.hdt.graphs` and `data.hdt.graphs.idx` is present, or if a published
    /// description set cannot be bound to its manifest view directory. There
    /// is no degraded mode: the error names the command that produces a missing
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
        let data = IndexedHdt::open(hdt, perm)?;

        artifacts.verify_graph_index()?;
        let document = crate::manifest::ManifestDocument::read(dir)?
            .expect("manifest existence was required immediately above");
        let manifest_description = document.lists_description_artifacts();
        if manifest_description && document.declares_components() {
            return Err(description_set_disagreement(
                dir,
                "this build does not yet support component description views; use a componentless bundle or wait for the full `kgf build` component contract",
            ));
        }
        let description = match (artifacts.description.as_ref(), manifest_description) {
            (Some(_), true) => {
                let manifest = document.into_parsed()?;
                manifest.validate(dir)?;
                let Some(entries) = manifest.description_artifacts() else {
                    return Err(description_set_disagreement(
                        dir,
                        "the manifest does not provide complete typed metadata for the on-disk \
                         description set; rebuild it with `kgf build`",
                    ));
                };
                Some(DescriptionStore::open(bundle, &artifacts, entries)?)
            }
            (None, false) => None,
            (Some(_), false) => {
                return Err(description_set_disagreement(
                    dir,
                    "the description files are present, but the manifest lists none of them; \
                     rebuild the description set with `kgf build`",
                ));
            }
            (None, true) => {
                return Err(description_set_disagreement(
                    dir,
                    "the manifest lists the description artifacts, but the files are absent; \
                     regenerate it with `kgf manifest` or rebuild them with `kgf build`",
                ));
            }
        };

        Ok(Self {
            bundle: bundle.clone(),
            data,
            description,
            text: artifacts.open_text()?,
        })
    }

    /// The immutable bundle-version directory backing this store.
    pub fn bundle_dir(&self) -> &Path {
        self.bundle.path()
    }

    /// The dictionary.
    pub fn dict(&self) -> Dictionary<'_> {
        self.data.dict()
    }

    /// The permutations.
    pub fn perms(&self) -> &Permutations {
        self.data.permutations()
    }

    /// The full-text index over this bundle's literals, if it published one.
    ///
    /// A hit is an **object dictionary id** and nothing else
    /// (`hdtc/docs/text-index-format.md` §2.1), which is why this composes
    /// rather than duplicating: turning hits into statements is
    /// [`resolve`](Store::resolve) with the object bound, over permutations
    /// this store already holds. There is no text-specific query path here.
    ///
    /// `None` when the bundle has no `data.hdt.text`, which is also when its
    /// manifest declares no `search` capability — one condition, read two ways.
    pub fn text(&self) -> Option<&TextSearcher> {
        self.text.as_ref()
    }

    /// The mapped description surface, when this bundle carries the complete
    /// tier-1 artifact set.
    pub fn description(&self) -> Option<&DescriptionStore> {
        self.description.as_ref()
    }

    /// Total triples in the bundle.
    pub fn triples(&self) -> u64 {
        self.data.triples()
    }

    /// SHA-256 identity of `data.hdt`'s dictionary and triples.
    ///
    /// Read from the required, publication-verified permutation sidecar after
    /// hdtc validates its cheap structural binding to this HDT. The digest
    /// excludes the mutable HDT header and is available without scanning
    /// either mapped payload on the request path.
    pub fn hdt_identity_digest(&self) -> [u8; 32] {
        self.data.permutations().hdt_identity_digest()
    }

    /// Resolve a pattern. `O(log N)`; enumerates nothing.
    ///
    /// The returned [`Selection`] borrows this store, which is what makes
    /// "resolved against a different bundle" unrepresentable. Query execution is
    /// synchronous within one blocking task holding an `Arc<Store>` (doc 20
    /// §20.4), so nothing needs to outlive the borrow; resumption goes through
    /// an encoded cursor token, not a live `Selection`.
    pub fn resolve(&self, pattern: IdPattern) -> Result<Selection<'_>> {
        self.data.resolve(pattern)
    }
}

pub(crate) fn description_set_disagreement(dir: &Path, detail: &str) -> Error {
    Error::ManifestSyntax {
        path: dir.join(artifact::MANIFEST),
        detail: detail.to_owned(),
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
    pub(crate) text: Option<PathBuf>,
    pub(crate) description: Option<DescriptionArtifacts>,
    /// The `filters/` files present, a subset of [`artifact::FILTERS`].
    pub(crate) filters: Vec<&'static str>,
    /// The `keysets/` files present, a subset of [`artifact::KEYSETS`].
    pub(crate) keysets: Vec<&'static str>,
}

/// The all-or-none artifact set behind the tier-1 description surface.
#[derive(Debug, Clone)]
pub(crate) struct DescriptionArtifacts {
    pub(crate) void_hdt: PathBuf,
    pub(crate) void_perm: PathBuf,
    pub(crate) schema_nodes: PathBuf,
    pub(crate) class_relations: PathBuf,
    pub(crate) class_properties: PathBuf,
    pub(crate) namespaces: PathBuf,
    pub(crate) summary_json: PathBuf,
    pub(crate) summary_md: PathBuf,
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
            text: optional_dir(dir, artifact::TEXT)?,
            description: DescriptionArtifacts::resolve(dir)?,
            filters: present(dir, &artifact::FILTERS)?,
            keysets: present(dir, &artifact::KEYSETS)?,
        })
    }

    /// Open the description graph when this is a tier-1 artifact set.
    ///
    /// [`DescriptionStore`] owns the returned core on the serving path;
    /// [`crate::manifest::BundleFacts`] opens and drops it as its binding proof.
    /// Sharing this boundary prevents `kgf manifest` from describing, or
    /// [`Store`] from accepting, a foreign or malformed VoID permutation pair.
    pub(crate) fn open_description(&self, bundle: &PublishedBundle) -> Result<Option<IndexedHdt>> {
        let Some(description) = &self.description else {
            return Ok(None);
        };
        let hdt = open_published(bundle, &description.void_hdt)?;
        let perm = open_published(bundle, &description.void_perm)?;
        IndexedHdt::open(hdt, perm).map(Some)
    }

    /// Open the text index, if the bundle published one.
    ///
    /// **The binding to `data.hdt` is not checked here, and cannot be.** Every
    /// other sidecar carries cheap source metadata — `.hdt.perm` has dictionary
    /// counts, a triple count and a suffix length — so a foreign one is refused
    /// for the price of a header read. A text index records only a SHA-256 over
    /// the HDT payload, so verifying it is a pass over the whole file, which
    /// doc 20 §20.3 keeps off the open path. It is checked where the other
    /// whole-file digests are, by `kgf manifest` and `kgf verify`, and a bundle
    /// this server was pointed at is one whose publication it is trusting
    /// already (doc 04 §4.6).
    ///
    /// What *is* established here is that the index opens and its manifest
    /// parses, so a broken one is refused at open rather than on the first
    /// query — the same rule the graph index follows.
    ///
    /// Opened eagerly, because [`Store`] has no interior mutability by design:
    /// a lazily-opened searcher would need one, and that is a lock on the read
    /// path. The cost is file descriptors — Tantivy holds one per segment —
    /// which is the budget the catalog's module docs already flag.
    pub(crate) fn open_text(&self) -> Result<Option<TextSearcher>> {
        let Some(dir) = &self.text else {
            return Ok(None);
        };
        TextSearcher::open(dir)
            .map(Some)
            .map_err(|error| Error::Malformed {
                artifact: dir.clone(),
                detail: format!("text index could not be opened: {error:#}"),
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

impl DescriptionArtifacts {
    /// Resolve the description surface as one publication unit.
    ///
    /// Carrying none of these files is a valid tier-0 bundle. Once any one is
    /// present, every one is required: publishing a partial set would either
    /// make a mandatory route lie about its availability or create a fallback
    /// path, both forbidden by docs 03 and 20.
    fn resolve(dir: &Path) -> Result<Option<Self>> {
        let paths: Vec<Option<PathBuf>> = artifact::DESCRIPTION
            .iter()
            .map(|name| optional_file(dir, name))
            .collect::<Result<_>>()?;
        let present = paths.iter().filter(|path| path.is_some()).count();
        if present == 0 {
            return Ok(None);
        }
        if present != artifact::DESCRIPTION.len() {
            let missing = paths
                .iter()
                .zip(artifact::DESCRIPTION)
                .find_map(|(path, name)| path.is_none().then_some(name))
                .expect("a partial description set has a missing artifact");
            return Err(Error::MissingRequiredArtifact {
                bundle: dir.to_path_buf(),
                artifact: missing.to_owned(),
                remedy: "kgf build".to_owned(),
            });
        }

        let mut paths = paths
            .into_iter()
            .map(|path| path.expect("every description artifact was required immediately above"));
        Ok(Some(Self {
            void_hdt: paths.next().expect("void HDT"),
            void_perm: paths.next().expect("void permutations"),
            schema_nodes: paths.next().expect("schema nodes"),
            class_relations: paths.next().expect("class relations"),
            class_properties: paths.next().expect("class properties"),
            namespaces: paths.next().expect("namespaces"),
            summary_json: paths.next().expect("summary JSON"),
            summary_md: paths.next().expect("summary Markdown"),
        }))
    }

    /// Every description artifact paired with its bundle-relative name.
    pub(crate) fn paths(&self) -> [(&'static str, &Path); 8] {
        [
            (artifact::VOID_HDT, &self.void_hdt),
            (artifact::VOID_PERM, &self.void_perm),
            (artifact::SCHEMA_NODES, &self.schema_nodes),
            (artifact::CLASS_RELATIONS, &self.class_relations),
            (artifact::CLASS_PROPERTIES, &self.class_properties),
            (artifact::NAMESPACES, &self.namespaces),
            (artifact::SUMMARY_JSON, &self.summary_json),
            (artifact::SUMMARY_MD, &self.summary_md),
        ]
    }
}

fn require_file(dir: &Path, name: &str, remedy: impl Into<String>) -> Result<PathBuf> {
    optional_file(dir, name)?.ok_or_else(|| Error::MissingRequiredArtifact {
        bundle: dir.to_path_buf(),
        artifact: name.to_owned(),
        remedy: remedy.into(),
    })
}

/// Which of a closed set of artifact names a directory carries.
///
/// Existence only. These files are validated when a producer reads their
/// headers to describe them, which costs a full pass for the CRC32C the two
/// formats require before any field may be interpreted — far outside what an
/// open may spend (doc 20 §20.6).
fn present(dir: &Path, names: &[&'static str]) -> Result<Vec<&'static str>> {
    let mut found = Vec::new();
    for name in names {
        if dir.join(name).try_exists()? {
            found.push(*name);
        }
    }
    Ok(found)
}

fn optional_file(dir: &Path, name: &str) -> Result<Option<PathBuf>> {
    optional(dir, name, false)
}

/// The one artifact shape that is a directory (doc 04 §4.1): `data.hdt.text`.
fn optional_dir(dir: &Path, name: &str) -> Result<Option<PathBuf>> {
    optional(dir, name, true)
}

fn optional(dir: &Path, name: &str, want_dir: bool) -> Result<Option<PathBuf>> {
    let path = dir.join(name);
    if !path.try_exists()? {
        return Ok(None);
    }
    let metadata = std::fs::metadata(&path)?;
    // Checked rather than assumed, because the two shapes fail differently: a
    // file where a directory belongs is refused here, while an empty directory
    // where a file belongs would otherwise be mapped as a zero-length artifact.
    let shape_matches = if want_dir {
        metadata.is_dir()
    } else {
        metadata.is_file()
    };
    if !shape_matches {
        return Err(Error::Malformed {
            artifact: path,
            detail: if want_dir {
                "artifact is not a directory".to_owned()
            } else {
                "artifact is not a regular file".to_owned()
            },
        });
    }
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::IdPattern;
    use crate::testing::{
        CLASS_PROPERTIES_HEADER, CLASS_RELATIONS_HEADER, Fixture, SCHEMA_NODES_HEADER, TINY_NT,
        published_bundle,
    };

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
    fn description_artifacts_are_absent_or_complete() {
        let fixture = Fixture::build(TINY_NT);
        std::fs::create_dir_all(fixture.bundle_path().join("stats")).unwrap();
        std::fs::copy(
            fixture.hdt_path(),
            fixture.bundle_path().join(artifact::VOID_HDT),
        )
        .unwrap();

        let published = published_bundle(fixture.bundle_path());
        match Store::open(&published, OpenOptions::default())
            .expect_err("one description artifact is not a tier-1 set")
        {
            Error::MissingRequiredArtifact {
                artifact: missing,
                remedy,
                ..
            } => {
                assert_eq!(missing, artifact::VOID_PERM);
                assert_eq!(remedy, "kgf build");
            }
            other => panic!("unexpected error: {other}"),
        }

        fixture.add_description_artifacts(
            SCHEMA_NODES_HEADER,
            CLASS_RELATIONS_HEADER,
            CLASS_PROPERTIES_HEADER,
        );
        let artifacts = ArtifactSet::resolve(fixture.bundle_path()).unwrap();
        let description = artifacts
            .description
            .as_ref()
            .expect("all eight files form the description set");
        assert_eq!(
            description.void_hdt,
            fixture.bundle_path().join(artifact::VOID_HDT)
        );
        assert_eq!(
            description.void_perm,
            fixture.bundle_path().join(artifact::VOID_PERM)
        );
        assert_eq!(
            description.schema_nodes,
            fixture.bundle_path().join(artifact::SCHEMA_NODES)
        );
        assert_eq!(
            description.class_relations,
            fixture.bundle_path().join(artifact::CLASS_RELATIONS)
        );
        assert_eq!(
            description.class_properties,
            fixture.bundle_path().join(artifact::CLASS_PROPERTIES)
        );
        assert_eq!(
            description.namespaces,
            fixture.bundle_path().join(artifact::NAMESPACES)
        );
        assert_eq!(
            description.summary_json,
            fixture.bundle_path().join(artifact::SUMMARY_JSON)
        );
        assert_eq!(
            description.summary_md,
            fixture.bundle_path().join(artifact::SUMMARY_MD)
        );

        artifacts
            .open_description(&published)
            .expect("a complete description pair binds")
            .expect("description core");
    }

    #[test]
    fn disk_and_manifest_description_sets_must_agree_without_panicking() {
        let files_only = Fixture::build(TINY_NT);
        files_only.add_description_artifacts(
            SCHEMA_NODES_HEADER,
            CLASS_RELATIONS_HEADER,
            CLASS_PROPERTIES_HEADER,
        );
        let published = published_bundle(files_only.bundle_path());
        match Store::open(&published, OpenOptions::default())
            .expect_err("unlisted description files must be refused")
        {
            Error::ManifestSyntax { path, detail } => {
                assert_eq!(path, files_only.bundle_path().join(artifact::MANIFEST));
                assert!(detail.contains("files are present"), "{detail}");
            }
            other => panic!("unexpected error: {other}"),
        }

        let manifest_only = Fixture::build(TINY_NT);
        let listed = artifact::DESCRIPTION
            .into_iter()
            .map(|name| (name.to_owned(), serde_json::json!({})))
            .collect::<serde_json::Map<_, _>>();
        std::fs::write(
            manifest_only.bundle_path().join(artifact::MANIFEST),
            serde_json::to_vec(&serde_json::json!({"artifacts": listed})).unwrap(),
        )
        .unwrap();
        let published = published_bundle(manifest_only.bundle_path());
        match Store::open(&published, OpenOptions::default())
            .expect_err("listed but absent description files must be refused")
        {
            Error::ManifestSyntax { path, detail } => {
                assert_eq!(path, manifest_only.bundle_path().join(artifact::MANIFEST));
                assert!(detail.contains("files are absent"), "{detail}");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn a_foreign_void_permutation_is_refused_with_the_description_set() {
        let fixture = Fixture::build(TINY_NT);
        let other = Fixture::build(&format!(
            "{TINY_NT}<http://example.org/extra> <http://example.org/p> <http://example.org/o> .\n"
        ));
        fixture.add_description_artifacts(
            SCHEMA_NODES_HEADER,
            CLASS_RELATIONS_HEADER,
            CLASS_PROPERTIES_HEADER,
        );
        std::fs::copy(
            other.perm_path(),
            fixture.bundle_path().join(artifact::VOID_PERM),
        )
        .unwrap();

        let published = published_bundle(fixture.bundle_path());
        let artifacts = ArtifactSet::resolve(fixture.bundle_path()).unwrap();
        match artifacts
            .open_description(&published)
            .expect_err("the description permutation must bind to its own VoID HDT")
        {
            Error::ArtifactBindingMismatch { artifact, hdt, .. } => {
                assert_eq!(artifact, fixture.bundle_path().join(artifact::VOID_PERM));
                assert_eq!(hdt, fixture.bundle_path().join(artifact::VOID_HDT));
            }
            other => panic!("unexpected error: {other}"),
        }
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
    fn a_text_index_is_opened_with_the_bundle_or_absent_from_it() {
        // The capability and the artifact are one condition read two ways, so
        // the store must not offer a searcher a manifest would not declare.
        let plain = Fixture::build(TINY_NT);
        let bundle = published_bundle(plain.bundle_path());
        let store = Store::open(&bundle, OpenOptions::default()).unwrap();
        assert!(store.text().is_none(), "no artifact, no search");

        let indexed = Fixture::build(TINY_NT).with_text();
        let bundle = published_bundle(indexed.bundle_path());
        let store = Store::open(&bundle, OpenOptions::default()).unwrap();
        let searcher = store.text().expect("the bundle published an index");

        // A hit is an object dictionary id, which is what makes it resolvable
        // through permutations this store already holds — no text-specific
        // query path, and no second dictionary.
        let hits = searcher
            .search(
                &hdtc::format::TextQuery {
                    text: "alice".to_owned(),
                    ..Default::default()
                },
                10,
            )
            .expect("search the fixture");
        assert!(!hits.is_empty(), "the fixture holds \"Alice\"");
        let objects = store.dict().counts().len(crate::Role::Object);
        for hit in &hits {
            assert!(
                (1..=objects).contains(&hit.object_id),
                "a hit must name a term in this bundle's object space"
            );
            let selection = store
                .resolve(IdPattern {
                    subject: None,
                    predicate: None,
                    object: Some(hit.object_id),
                })
                .expect("an id from this dictionary");
            assert!(
                selection.count().value > 0,
                "an indexed literal occurs in at least one triple"
            );
        }
    }

    #[test]
    fn a_text_index_that_will_not_open_is_refused_with_the_bundle() {
        // A present-but-broken optional artifact is refused at open, the same
        // rule the graph index follows: there is no degraded mode in which a
        // bundle serves everything except the operation it advertises.
        let fixture = Fixture::build(TINY_NT).with_text();
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("broken-text");
        fixture.copy_bundle_to(&bundle);
        std::fs::write(bundle.join(artifact::TEXT).join("meta.json"), b"not json").unwrap();

        let published = published_bundle(&bundle);
        match Store::open(&published, OpenOptions::default()).expect_err("must refuse") {
            Error::Malformed { artifact, detail } => {
                assert_eq!(artifact, bundle.join(artifact::TEXT));
                assert!(detail.contains("text index"), "{detail}");
            }
            other => panic!("unexpected error: {other}"),
        }

        // And a file where the directory belongs is refused by shape, before
        // anything tries to read it as an index.
        let bundle = root.path().join("text-is-a-file");
        Fixture::build(TINY_NT).copy_bundle_to(&bundle);
        std::fs::write(bundle.join(artifact::TEXT), b"not a directory").unwrap();
        let published = published_bundle(&bundle);
        assert!(matches!(
            Store::open(&published, OpenOptions::default()),
            Err(Error::Malformed { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_regular_file_is_not_an_artifact() {
        let fixture = Fixture::build(TINY_NT);
        let fifo = fixture.bundle_path().join(artifact::GRAPHS);
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo failed");

        let published = published_bundle(fixture.bundle_path());
        match Store::open(&published, OpenOptions::default())
            .expect_err("a FIFO is not a regular artifact file")
        {
            Error::Malformed { artifact, detail } => {
                assert_eq!(artifact, fifo);
                assert_eq!(detail, "artifact is not a regular file");
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
