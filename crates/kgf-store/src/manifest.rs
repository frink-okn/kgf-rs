//! The bundle manifest (doc 04 §4.3).
//!
//! The manifest is the immutable half of the three-document split: content
//! identity, counts, capabilities, and prefixes, all intrinsic to the artifacts.
//! Runtime caps, rate limits, and mirrors are the *service* descriptor's, and
//! predicate role declarations are the *dataset* descriptor's, precisely so that
//! host policy never forces a new data version.
//!
//! # Why this lives in the read layer
//!
//! A manifest is bundle metadata, not HTTP vocabulary, so it does not violate
//! the crate boundary (CLAUDE.md rule 5): nothing here knows about caps,
//! budgets, formats, or cursors. What the server does with a capability — route
//! an operation or answer `capability_not_available` — stays in `kgf-server`.
//!
//! # Not read by `Store::open`
//!
//! [`Store::open`](crate::store::Store::open) requires `manifest.json` to
//! *exist*, because a directory without one is not a bundle, but never parses
//! it. The query core answers patterns from `data.hdt` and `data.hdt.perm`
//! alone, and keeping the parse out of `open` is what lets the store stay
//! testable headless against fixture bundles that carry a placeholder.
//!
//! # Producing one
//!
//! Most of a manifest is derivable from the artifacts it describes, and the
//! derivable parts — counts, capabilities, checksums, the content digest — are
//! exactly the ones that are unwritable by hand or that rot silently when
//! hand-edited. [`BundleFacts::read`] recovers the structural half at a cost
//! independent of bundle size; `kgf manifest` combines it with the identity a
//! human supplies. [`Manifest::verify_against`] is the other direction: proof
//! that a manifest still describes the bytes beside it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Role;
use crate::dict::DictCounts;
use crate::error::{Error, Result};
use crate::map::{PublishedBundle, open_published};
use crate::perm::Permutations;
use crate::store::{ArtifactSet, artifact};

/// The manifest format version this build reads and writes (doc 04 §4.3).
pub const MANIFEST_FORMAT: &str = "1";

/// The bundle layout version this build reads and writes.
pub const BUNDLE_FORMAT: &str = "1";

/// The HDT format version bundles carry.
pub const HDT_FORMAT: &str = "1.0";

/// An operation family a bundle's artifacts can support (doc 03 §3.4).
///
/// A capability describes the *bundle*, not the deployment: it says the bytes
/// needed to answer are present, while whether a given server routes the
/// operation is the service descriptor's business (doc 04 §4.3). Declaring one
/// commits to that capability's full contract — methods, formats, and response
/// metadata (doc 03 §3.1) — so nothing here is declared speculatively.
///
/// The core profile (`fragment`, `count`, `describe`, and the description
/// surface) is mandatory and therefore absent from this enum: it is not a
/// capability, it is the floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    /// `QUERY /star` — star hydration.
    Star,
    /// `GET /sample` — pseudo-random members of a pattern's results.
    Sample,
    /// `GET /terms` — dictionary prefix access.
    Terms,
    /// `GET /export/...` — bulk artifact download.
    Export,
    /// `g=` scoping and `GET /graphs`; needs the graph sidecar pair.
    Graphs,
    /// `GET /search` and `o.text`; needs the text sidecar.
    Search,
    /// Typed range bounds on objects; needs the range sidecar.
    Range,
    /// `GET /closure` — transitive expansion; needs the closure sidecar.
    Closure,
}

impl Capability {
    /// The name this capability has as a manifest key and in doc 03.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Star => "star",
            Self::Sample => "sample",
            Self::Terms => "terms",
            Self::Export => "export",
            Self::Graphs => "graphs",
            Self::Search => "search",
            Self::Range => "range",
            Self::Closure => "closure",
        }
    }
}

/// What a bundle's artifacts say about themselves, read without a manifest.
///
/// Every field here is recovered from the structures rather than from any
/// header or sidecar summary, for the reason [`DictCounts`] documents: a header
/// is the one part of an HDT that a rewrite may change. Reading them costs a
/// bundle open — bounded and independent of bundle size — and no payload scan.
#[derive(Debug, Clone)]
pub struct BundleFacts {
    triples: u64,
    counts: DictCounts,
    capabilities: BTreeSet<Capability>,
    artifact_names: Vec<&'static str>,
}

impl BundleFacts {
    /// Read the structural facts of a bundle directory that may not yet have a
    /// manifest.
    ///
    /// This is the one entry point that opens a bundle's artifacts without
    /// requiring `manifest.json`, because it is what `kgf manifest` uses to
    /// produce one. Everything else goes through
    /// [`Store::open`](crate::store::Store::open).
    ///
    /// Constructing the [`PublishedBundle`] capability is where the caller
    /// accepts doc 04 §4.6's immutability obligation, exactly as for the store.
    ///
    /// Every check [`Store::open`](crate::store::Store::open) makes is made
    /// here except the manifest's existence, so that a bundle this describes is
    /// a bundle that opens. Describing one that then refuses to open would be
    /// worse than describing nothing.
    pub fn read(bundle: &PublishedBundle) -> Result<Self> {
        let artifacts = ArtifactSet::resolve(bundle.path())?;

        let hdt = open_published(bundle, &artifacts.hdt)?;
        let perm = open_published(bundle, &artifacts.perm)?;
        let perms = Permutations::open(hdt, perm)?;

        artifacts.verify_graph_index()?;

        Ok(Self {
            triples: perms.triples(),
            counts: *perms.dict_counts(),
            capabilities: capabilities_for(&artifacts),
            artifact_names: artifact_names_for(&artifacts),
        })
    }

    /// Total triples, from `ArrayZ`'s entry count.
    pub fn triples(&self) -> u64 {
        self.triples
    }

    /// Per-role dictionary counts, from the four PFC preambles.
    pub fn dict_counts(&self) -> &DictCounts {
        &self.counts
    }

    /// The capabilities these artifacts support.
    pub fn capabilities(&self) -> impl Iterator<Item = Capability> + '_ {
        self.capabilities.iter().copied()
    }

    /// The bundle's artifact file names, in a stable order.
    ///
    /// `manifest.json` is deliberately absent: the manifest records checksums of
    /// the artifacts *it* describes, and including itself would be circular —
    /// its own bytes contain the digest computed over them.
    pub fn artifact_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.artifact_names.iter().copied()
    }

    /// The counts a manifest records for these artifacts.
    pub fn counts(&self) -> Counts {
        Counts {
            triples: self.triples,
            subjects: self.counts.len(Role::Subject),
            predicates: self.counts.len(Role::Predicate),
            objects: self.counts.len(Role::Object),
        }
    }
}

/// Which capabilities an artifact set supports.
///
/// Four of doc 03's optional capabilities need nothing beyond the artifacts
/// every bundle is required to carry: `star` and `sample` are compositions of
/// triple patterns, `terms` reads the dictionary already in `data.hdt`, and
/// `export` serves the files themselves. The rest are gated on sidecars — the
/// graph pair and the text index exist today, and `range` and `closure` are
/// therefore never derived here, since a bundle cannot acquire them without
/// acquiring an artifact.
///
/// `terms` is declared bare rather than with doc 04's `resolve_keys` option:
/// key resolution is a separate contract, and declaring an option the artifacts
/// do not support would be exactly the speculative claim doc 03 §3.1 forbids.
fn capabilities_for(artifacts: &ArtifactSet) -> BTreeSet<Capability> {
    let mut capabilities = BTreeSet::from([
        Capability::Star,
        Capability::Sample,
        Capability::Terms,
        Capability::Export,
    ]);
    if artifacts.graphs.is_some() {
        capabilities.insert(Capability::Graphs);
    }
    if artifacts.text.is_some() {
        capabilities.insert(Capability::Search);
    }
    capabilities
}

/// The artifact file names present, in the order doc 04 §4.1 lists them.
///
/// **This list must grow with every sidecar.** Doc 04 §4.1 reserves `labels/`,
/// `ranges/`, `closures/`, `reif/`, `geo/`, `vectors/`, `filters/`,
/// and `stats/`, none of which has a producer yet; an artifact absent from here
/// is absent from the manifest's checksums and therefore from
/// `content_digest`. Adding a sidecar without adding it here silently narrows a
/// version's identity.
///
/// Files a bundle may carry that are deliberately *not* artifacts of this list
/// are a separate matter: `data.hdt.index.v1-1` is optional and never read
/// (doc 04 §4.1, doc 20 §20.8), so a bundle carrying one is conforming and must
/// not be refused. Whether it should nonetheless be checksummed — doc 04 §4.3
/// wants `content_digest` usable for mirror verification, which argues yes — is
/// an open question for the design docs, not something to settle here.
fn artifact_names_for(artifacts: &ArtifactSet) -> Vec<&'static str> {
    let mut names = vec![artifact::HDT, artifact::PERM];
    if artifacts.graphs.is_some() {
        names.push(artifact::GRAPHS);
    }
    if artifacts.graph_index.is_some() {
        names.push(artifact::GRAPHS_IDX);
    }
    if artifacts.text.is_some() {
        names.push(artifact::TEXT);
    }
    names
}

/// Structural counts over the merged, queryable graph.
///
/// Doc 03 §3.4.10 makes these load-bearing rather than decorative: the VoID
/// document's numbers must equal `/count` results, and these are where it gets
/// them. `subjects` and `objects` are id-space sizes — each counts the shared
/// section once, so they overlap and do not sum to a distinct-term total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// Total triples.
    pub triples: u64,
    /// Size of the subject id space: shared plus subject-only terms.
    pub subjects: u64,
    /// Size of the predicate id space.
    pub predicates: u64,
    /// Size of the object id space: shared plus object-only terms.
    pub objects: u64,
}

/// Format versions the bundle declares (doc 04 §4.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Formats {
    /// Bundle layout version.
    pub bundle: String,
    /// Manifest schema version.
    pub manifest: String,
    /// HDT format version.
    pub hdt: String,
}

impl Default for Formats {
    fn default() -> Self {
        Self {
            bundle: BUNDLE_FORMAT.to_owned(),
            manifest: MANIFEST_FORMAT.to_owned(),
            hdt: HDT_FORMAT.to_owned(),
        }
    }
}

/// Who published the bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publisher {
    /// Display name.
    pub name: String,
    /// Contact address, if given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact: Option<String>,
}

/// One artifact's size and checksum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactEntry {
    /// Size in bytes.
    pub bytes: u64,
    /// Lowercase hex SHA-256 of the file's contents.
    pub sha256: String,
}

/// A bundle manifest.
///
/// Field order here is the serialized order and every collection is a
/// `BTreeMap`, so equal manifests serialize to equal bytes. A writer that also
/// keeps `created` fixed while the artifacts are unchanged — `kgf manifest`
/// does — therefore regenerates byte-identically, which is what lets a manifest
/// be diffed across versions and keeps
/// [`content_digest`](Self::content_digest) usable as the canonical identity
/// doc 04 §4.3 makes it.
///
/// Unknown fields are accepted and dropped rather than refused: the manifest
/// grows over time, `formats.manifest` is what versions it, and a bundle written
/// by a newer builder should stay readable for the fields this build knows. What
/// is *not* tolerated is a manifest that contradicts its artifacts — see
/// [`verify_against`](Self::verify_against).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Host-local dataset slug.
    pub id: String,
    /// Globally stable dataset identity, used by registries and citations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_iri: Option<String>,
    /// Human-facing release label.
    pub version: String,
    /// Merkle root over the artifact checksums; the version's canonical
    /// identity, prefixed with its algorithm (`sha256:…`).
    pub content_digest: String,
    /// RFC 3339 build timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    /// Bundle, manifest, and HDT format versions.
    #[serde(default)]
    pub formats: Formats,
    /// Short human title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Longer human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// License identifier or URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Project homepage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Publisher identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<Publisher>,
    /// Structural counts over the queryable graph.
    pub counts: Counts,
    /// Declared capabilities, keyed by the names in doc 03 §3.4.
    ///
    /// Values are untyped because a capability's configuration is defined by the
    /// sidecar behind it — `search`'s exclusion lists, `range`'s families — and
    /// typing bodies for artifacts this build cannot produce would be inventing
    /// a schema ahead of the thing it describes.
    #[serde(default)]
    pub capabilities: BTreeMap<String, serde_json::Value>,
    /// Prefix map for CURIE syntax in parameters (doc 03 §3.3).
    #[serde(default)]
    pub prefixes: BTreeMap<String, String>,
    /// Per-artifact sizes and checksums.
    #[serde(default)]
    pub artifacts: BTreeMap<String, ArtifactEntry>,
    /// The version this one supersedes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
}

impl Manifest {
    /// Read and parse `manifest.json` from a bundle directory.
    ///
    /// For readers. A *writer* wants [`ManifestDocument::read`], which also
    /// keeps the fields this build does not model.
    pub fn read(bundle_dir: &Path) -> Result<Self> {
        ManifestDocument::read(bundle_dir)?
            .ok_or_else(|| {
                Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "{} not found",
                        bundle_dir.join(artifact::MANIFEST).display()
                    ),
                ))
            })?
            .into_parsed()
    }

    /// Serialize to the canonical on-disk bytes: two-space indent, trailing
    /// newline.
    ///
    /// For a bundle whose manifest is being written for the first time. Once one
    /// exists, [`ManifestDocument::rewrite_with`] is the way to replace it,
    /// because this drops anything the document held that [`Manifest`] does not
    /// model.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>> {
        canonical_bytes(self, Path::new(artifact::MANIFEST))
    }

    /// Whether this bundle declares `capability`.
    pub fn declares(&self, capability: Capability) -> bool {
        self.capabilities.contains_key(capability.as_str())
    }

    /// Check that this manifest still describes the artifacts beside it.
    ///
    /// A manifest is written once and read forever, so the failure that matters
    /// is not a corrupt file but a stale one: artifacts rebuilt without
    /// regenerating the manifest. Left unchecked that surfaces as a `/void`
    /// document quietly disagreeing with `/count` — the one invariant doc 03
    /// §3.4.10 states outright — so it is refused here instead, naming the
    /// command that repairs it.
    ///
    /// Counts are the whole check: they are free to obtain and no rebuild
    /// preserves all four by accident. Checksums are not verified, because that
    /// is a full read of every artifact and belongs to publish and `kgf verify`
    /// (doc 20 §20.6).
    pub fn verify_against(&self, facts: &BundleFacts, bundle_dir: &Path) -> Result<()> {
        let actual = facts.counts();
        for (field, recorded, actual) in [
            ("counts.triples", self.counts.triples, actual.triples),
            ("counts.subjects", self.counts.subjects, actual.subjects),
            (
                "counts.predicates",
                self.counts.predicates,
                actual.predicates,
            ),
            ("counts.objects", self.counts.objects, actual.objects),
        ] {
            if recorded != actual {
                return Err(Error::ManifestDisagreement {
                    path: bundle_dir.join(artifact::MANIFEST),
                    field: field.to_owned(),
                    recorded,
                    actual,
                    remedy: format!("kgf manifest {}", bundle_dir.display()),
                });
            }
        }
        Ok(())
    }
}

/// A `manifest.json` as it exists on disk: the parsed form when there is one,
/// and always the raw JSON object.
///
/// Rewriting a manifest is not the same operation as reading one. A writer that
/// deserializes into [`Manifest`] and serializes back **deletes every field this
/// build does not model** — doc 04 §4.3's `source` and `components`, a
/// capability's configuration body, anything a newer builder added. That is
/// silent data loss in a file whose whole job is to be the record.
///
/// So the raw object is kept alongside the parse, and
/// [`rewrite_with`](Self::rewrite_with) merges derived fields over it. The
/// parse is allowed to fail: a bundle's first manifest is often a `{}`
/// placeholder, and refusing to write over one would defeat the purpose. What
/// is *not* allowed to fail quietly is a document declaring a schema this build
/// does not read — overwriting a newer manifest with an older one loses more
/// than it repairs, so that is refused at [`read`](Self::read).
#[derive(Debug, Clone)]
pub struct ManifestDocument {
    path: PathBuf,
    raw: serde_json::Map<String, serde_json::Value>,
    parsed: std::result::Result<Manifest, String>,
}

impl ManifestDocument {
    /// Read the document, or `Ok(None)` if the bundle has no manifest yet.
    ///
    /// Fails on JSON that does not parse or is not an object, and on a
    /// `formats.manifest` this build does not read. Succeeds with an
    /// unparseable-as-[`Manifest`] document, since `{}` is one.
    pub fn read(bundle_dir: &Path) -> Result<Option<Self>> {
        let path = bundle_dir.join(artifact::MANIFEST);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        let document: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|source| Error::ManifestSyntax {
                path: path.clone(),
                detail: source.to_string(),
            })?;
        let raw = match document {
            serde_json::Value::Object(raw) => raw,
            other => {
                return Err(Error::ManifestSyntax {
                    path,
                    detail: format!("expected a JSON object, found {}", kind_of(&other)),
                });
            }
        };

        // Checked against the raw document rather than a parsed one, so that a
        // future-format manifest is refused even when this build cannot make
        // sense of the rest of it.
        if let Some(found) = declared_format(&raw)
            && found != MANIFEST_FORMAT
        {
            return Err(Error::UnsupportedManifestFormat {
                path,
                found: found.to_owned(),
                supported: MANIFEST_FORMAT.to_owned(),
            });
        }

        let parsed = serde_json::from_value(serde_json::Value::Object(raw.clone()))
            .map_err(|source| source.to_string());
        Ok(Some(Self { path, raw, parsed }))
    }

    /// The manifest file this was read from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The parsed manifest, if the document is a complete one.
    pub fn parsed(&self) -> Option<&Manifest> {
        self.parsed.as_ref().ok()
    }

    /// The parsed manifest, or why it is not one.
    pub fn into_parsed(self) -> Result<Manifest> {
        self.parsed.map_err(|detail| Error::ManifestSyntax {
            path: self.path,
            detail,
        })
    }

    /// The canonical bytes of this document with `manifest`'s fields applied.
    ///
    /// Every key `manifest` serializes replaces the one here; every other key
    /// survives untouched. The consequence worth knowing is that a field cannot
    /// be *removed* by regenerating — `--license` can be changed but not
    /// cleared, which is the right trade for a record that must not silently
    /// lose what it was given.
    pub fn rewrite_with(&self, manifest: &Manifest) -> Result<Vec<u8>> {
        let fresh = serde_json::to_value(manifest).map_err(|source| Error::ManifestSyntax {
            path: self.path.clone(),
            detail: source.to_string(),
        })?;
        let fresh = fresh.as_object().ok_or_else(|| Error::ManifestSyntax {
            path: self.path.clone(),
            detail: "a manifest did not serialize as a JSON object".to_owned(),
        })?;

        let mut merged = self.raw.clone();
        for (key, value) in fresh {
            merged.insert(key.clone(), value.clone());
        }
        canonical_bytes(&serde_json::Value::Object(merged), &self.path)
    }
}

/// The schema version a raw document declares, if it declares one.
fn declared_format(raw: &serde_json::Map<String, serde_json::Value>) -> Option<&str> {
    raw.get("formats")?.get("manifest")?.as_str()
}

/// A JSON value's type, for an error message.
fn kind_of(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// The canonical on-disk form: two-space indent, trailing newline.
///
/// One place decides this so that regeneration is byte-stable and a manifest can
/// be diffed across versions.
fn canonical_bytes<T: Serialize>(value: &T, path: &Path) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| Error::ManifestSyntax {
        path: path.to_path_buf(),
        detail: source.to_string(),
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// The unit `content_digest` is computed over: one artifact's name and checksum.
///
/// Exposed so that the builder computing checksums and this crate agree on the
/// Merkle recipe without either restating it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDigest {
    /// The artifact's file name.
    pub name: String,
    /// Lowercase hex SHA-256 of its contents.
    pub sha256: String,
}

/// The bytes a bundle's `content_digest` is the SHA-256 of.
///
/// The recipe, fixed here so `kgf build` reproduces what `kgf manifest` wrote:
/// sort the artifacts by name, emit `{name}  {sha256}\n` for each, hash the
/// concatenation, and prefix the result with `sha256:`. `manifest.json` is not
/// among them — it carries the digest, so hashing it would be circular.
///
/// Returning the preimage rather than the digest keeps SHA-256 out of the read
/// layer: this crate never hashes a file, by design (doc 20 §20.6).
pub fn content_digest_preimage(artifacts: &[ArtifactDigest]) -> Vec<u8> {
    let mut sorted: Vec<&ArtifactDigest> = artifacts.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    let mut preimage = Vec::new();
    for artifact in sorted {
        preimage.extend_from_slice(artifact.name.as_bytes());
        preimage.extend_from_slice(b"  ");
        preimage.extend_from_slice(artifact.sha256.as_bytes());
        preimage.push(b'\n');
    }
    preimage
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Fixture, TINY_NQ, TINY_NT, published_bundle};

    fn facts(fixture: &Fixture) -> BundleFacts {
        let bundle = published_bundle(fixture.bundle_path());
        BundleFacts::read(&bundle).unwrap()
    }

    #[test]
    fn facts_read_without_a_manifest_being_parsed() {
        let fixture = Fixture::build(TINY_NT);
        let facts = facts(&fixture);

        // The fixture's manifest is the `{}` placeholder, which would not parse
        // as a `Manifest`. Reading facts must not care.
        assert!(Manifest::read(fixture.bundle_path()).is_err());

        assert_eq!(facts.triples(), 8);
        let counts = facts.counts();
        assert_eq!(counts.triples, 8);
        assert_eq!(
            counts.subjects,
            facts.dict_counts().shared + facts.dict_counts().subjects
        );
        assert_eq!(
            counts.objects,
            facts.dict_counts().shared + facts.dict_counts().objects
        );
    }

    #[test]
    fn a_core_bundle_declares_only_artifact_backed_capabilities() {
        let fixture = Fixture::build(TINY_NT);
        let capabilities: Vec<_> = facts(&fixture).capabilities().collect();

        assert_eq!(
            capabilities,
            vec![
                Capability::Star,
                Capability::Sample,
                Capability::Terms,
                Capability::Export
            ]
        );
        // Sidecar-gated capabilities are never guessed at.
        assert!(!capabilities.contains(&Capability::Search));
        assert!(!capabilities.contains(&Capability::Range));
        assert!(!capabilities.contains(&Capability::Closure));
        assert!(!capabilities.contains(&Capability::Graphs));
    }

    #[test]
    fn a_text_index_is_what_declares_search() {
        // The capability and the artifact are the same fact, so `search` is
        // derived from the bytes rather than configured: a bundle cannot
        // advertise an operation whose index it does not carry, and one that
        // carries the index cannot forget to advertise it.
        let fixture = Fixture::build(TINY_NT).with_text();
        let facts = facts(&fixture);

        assert!(facts.capabilities().any(|c| c == Capability::Search));
        // And it joins the checksummed set, so adding an index changes the
        // version's identity rather than slipping in unrecorded.
        let names: Vec<_> = facts.artifact_names().collect();
        assert!(names.contains(&crate::store::artifact::TEXT), "{names:?}");
    }

    #[test]
    fn a_quad_bundle_adds_graphs_and_its_two_artifacts() {
        let fixture = Fixture::build_quads(TINY_NQ);
        let facts = facts(&fixture);

        assert!(facts.capabilities().any(|c| c == Capability::Graphs));
        assert_eq!(
            facts.artifact_names().collect::<Vec<_>>(),
            vec![
                artifact::HDT,
                artifact::PERM,
                artifact::GRAPHS,
                artifact::GRAPHS_IDX
            ]
        );
    }

    #[test]
    fn a_bundle_that_would_not_open_is_not_described() {
        // The invariant: every check `Store::open` makes, `BundleFacts::read`
        // makes too, apart from the manifest's existence. Without it
        // `kgf manifest` writes a manifest declaring `graphs` for a bundle that
        // then refuses to open.
        let first = Fixture::build_quads(TINY_NQ);
        let other = Fixture::build_quads(concat!(
            "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> ",
            "<http://example.org/g1> .\n",
            "<http://example.org/extra> <http://example.org/p> <http://example.org/o> ",
            "<http://example.org/g3> .\n",
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
        match BundleFacts::read(&published)
            .expect_err("a graph index from another HDT must not be described")
        {
            Error::ArtifactBindingMismatch { artifact, hdt, .. } => {
                assert_eq!(artifact, bundle.join(artifact::GRAPHS_IDX));
                assert_eq!(hdt, bundle.join(artifact::HDT));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn artifact_names_never_include_the_manifest() {
        for fixture in [Fixture::build(TINY_NT), Fixture::build_quads(TINY_NQ)] {
            assert!(
                facts(&fixture)
                    .artifact_names()
                    .all(|name| name != artifact::MANIFEST)
            );
        }
    }

    fn sample_manifest(counts: Counts) -> Manifest {
        Manifest {
            id: "tiny".to_owned(),
            dataset_iri: None,
            version: "2026-08-01".to_owned(),
            content_digest: "sha256:0".to_owned(),
            created: None,
            formats: Formats::default(),
            title: None,
            description: None,
            license: None,
            homepage: None,
            publisher: None,
            counts,
            capabilities: BTreeMap::new(),
            prefixes: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            previous_version: None,
        }
    }

    #[test]
    fn a_manifest_round_trips_through_its_canonical_bytes() {
        let mut manifest = sample_manifest(Counts {
            triples: 8,
            subjects: 4,
            predicates: 3,
            objects: 6,
        });
        manifest
            .capabilities
            .insert("sample".to_owned(), serde_json::json!({}));
        manifest
            .prefixes
            .insert("ex".to_owned(), "http://example.org/".to_owned());

        let bytes = manifest.to_json_bytes().unwrap();
        assert!(bytes.ends_with(b"\n"));
        let parsed: Manifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, manifest);
        // Byte-stable: the same manifest serializes identically every time,
        // which is what makes `content_digest` a usable identity.
        assert_eq!(parsed.to_json_bytes().unwrap(), bytes);
        assert!(parsed.declares(Capability::Sample));
        assert!(!parsed.declares(Capability::Search));
    }

    fn write_document(dir: &Path, json: &serde_json::Value) {
        std::fs::write(
            dir.join(artifact::MANIFEST),
            serde_json::to_vec(json).unwrap(),
        )
        .unwrap();
    }

    fn sample_counts() -> Counts {
        Counts {
            triples: 8,
            subjects: 4,
            predicates: 3,
            objects: 6,
        }
    }

    #[test]
    fn unknown_fields_survive_a_newer_writer_but_a_newer_format_does_not() {
        let dir = tempfile::tempdir().unwrap();

        let mut json = serde_json::to_value(sample_manifest(sample_counts())).unwrap();
        json["some_field_from_the_future"] = serde_json::json!({"a": 1});
        write_document(dir.path(), &json);
        assert_eq!(Manifest::read(dir.path()).unwrap().counts.triples, 8);

        json["formats"]["manifest"] = serde_json::json!("2");
        write_document(dir.path(), &json);
        assert!(matches!(
            Manifest::read(dir.path()),
            Err(Error::UnsupportedManifestFormat { found, .. }) if found == "2"
        ));
    }

    #[test]
    fn a_document_is_readable_when_the_manifest_in_it_is_not() {
        let dir = tempfile::tempdir().unwrap();

        // No manifest at all is not an error for a writer; it is the first run.
        assert!(ManifestDocument::read(dir.path()).unwrap().is_none());

        // The placeholder a bundle starts with: valid JSON, not a manifest.
        write_document(dir.path(), &serde_json::json!({}));
        let document = ManifestDocument::read(dir.path()).unwrap().unwrap();
        assert!(document.parsed().is_none());
        assert!(matches!(
            document.into_parsed(),
            Err(Error::ManifestSyntax { .. })
        ));

        // A future schema is refused even though the rest is unreadable, because
        // the alternative is overwriting it with an older document.
        write_document(
            dir.path(),
            &serde_json::json!({"formats": {"manifest": "2"}}),
        );
        assert!(matches!(
            ManifestDocument::read(dir.path()),
            Err(Error::UnsupportedManifestFormat { found, .. }) if found == "2"
        ));

        // Not JSON, and JSON that is not an object, are both refused rather than
        // silently replaced.
        std::fs::write(dir.path().join(artifact::MANIFEST), b"{not json").unwrap();
        assert!(matches!(
            ManifestDocument::read(dir.path()),
            Err(Error::ManifestSyntax { .. })
        ));
        write_document(dir.path(), &serde_json::json!([1, 2, 3]));
        let error = ManifestDocument::read(dir.path()).unwrap_err();
        assert!(error.to_string().contains("found an array"), "{error}");
    }

    #[test]
    fn rewriting_replaces_modeled_fields_and_keeps_everything_else() {
        let dir = tempfile::tempdir().unwrap();

        // A document carrying doc 04 §4.3 fields this build does not model, plus
        // one from a newer writer.
        let mut json = serde_json::to_value(sample_manifest(sample_counts())).unwrap();
        json["source"] = serde_json::json!({"format": "n-triples", "sha256": "abc"});
        json["components"] = serde_json::json!([{"id": "canonical", "role": "source"}]);
        json["something_newer"] = serde_json::json!(["a", "b"]);
        write_document(dir.path(), &json);

        let document = ManifestDocument::read(dir.path()).unwrap().unwrap();
        let mut updated = document.parsed().unwrap().clone();
        updated.counts.triples = 9;
        updated.content_digest = "sha256:new".to_owned();

        let rewritten: serde_json::Value =
            serde_json::from_slice(&document.rewrite_with(&updated).unwrap()).unwrap();

        // Modeled fields come from the new manifest.
        assert_eq!(rewritten["counts"]["triples"], 9);
        assert_eq!(rewritten["content_digest"], "sha256:new");
        // Everything else is exactly as it was.
        assert_eq!(rewritten["source"], json["source"]);
        assert_eq!(rewritten["components"], json["components"]);
        assert_eq!(rewritten["something_newer"], json["something_newer"]);

        // And the result is still a manifest, in canonical form.
        let bytes = document.rewrite_with(&updated).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert_eq!(
            serde_json::from_slice::<Manifest>(&bytes).unwrap().counts,
            updated.counts
        );
    }

    #[test]
    fn rewriting_a_placeholder_produces_a_whole_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_document(dir.path(), &serde_json::json!({}));

        let document = ManifestDocument::read(dir.path()).unwrap().unwrap();
        let bytes = document
            .rewrite_with(&sample_manifest(sample_counts()))
            .unwrap();

        let parsed: Manifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.counts, sample_counts());
        assert_eq!(parsed.id, "tiny");
    }

    #[test]
    fn a_manifest_that_disagrees_with_its_artifacts_is_refused_by_name() {
        let fixture = Fixture::build(TINY_NT);
        let facts = facts(&fixture);

        let honest = sample_manifest(facts.counts());
        honest
            .verify_against(&facts, fixture.bundle_path())
            .unwrap();

        // The failure this exists for: artifacts rebuilt, manifest not.
        let mut stale = honest.clone();
        stale.counts.triples += 1;
        let error = stale
            .verify_against(&facts, fixture.bundle_path())
            .unwrap_err();
        assert!(matches!(
            &error,
            Error::ManifestDisagreement { field, recorded, actual, .. }
                if field == "counts.triples" && *recorded == 9 && *actual == 8
        ));
        assert!(error.to_string().contains("kgf manifest"));

        let mut wrong_objects = honest;
        wrong_objects.counts.objects += 1;
        assert!(matches!(
            wrong_objects.verify_against(&facts, fixture.bundle_path()),
            Err(Error::ManifestDisagreement { field, .. }) if field == "counts.objects"
        ));
    }

    #[test]
    fn the_digest_preimage_is_order_independent_and_excludes_nothing_given() {
        let a = ArtifactDigest {
            name: "data.hdt".to_owned(),
            sha256: "aa".to_owned(),
        };
        let b = ArtifactDigest {
            name: "data.hdt.perm".to_owned(),
            sha256: "bb".to_owned(),
        };

        let forwards = content_digest_preimage(&[a.clone(), b.clone()]);
        let backwards = content_digest_preimage(&[b, a]);
        assert_eq!(forwards, backwards);
        assert_eq!(forwards, b"data.hdt  aa\ndata.hdt.perm  bb\n");

        // A changed checksum changes the preimage, which is the whole point.
        let changed = content_digest_preimage(&[
            ArtifactDigest {
                name: "data.hdt".to_owned(),
                sha256: "ac".to_owned(),
            },
            ArtifactDigest {
                name: "data.hdt.perm".to_owned(),
                sha256: "bb".to_owned(),
            },
        ]);
        assert_ne!(forwards, changed);
    }
}
