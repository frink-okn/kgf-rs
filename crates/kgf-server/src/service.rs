//! What this deployment serves: datasets, their versions, and how a URL
//! resolves to one.
//!
//! Doc 03 §3.2's URL space has two halves. `/{dataset}/v/{version}/…` addresses
//! an immutable bundle, and the catalog ([`kgf_store::catalog`]) already knows
//! those. `/`, `/{dataset}` and `/{dataset}/latest/…` address things that
//! *move*, and nothing below this layer knows about them — doc 04 §4.3 splits
//! the three descriptors by mutability precisely so that a host can change its
//! mind without a new data version.
//!
//! # Where `current` comes from
//!
//! Doc 04 §4.3 puts `current` in the dataset descriptor, which it calls mutable
//! and host-independent. This server has no such document to read: a deployment
//! is a directory of bundles, and nothing in the toolchain writes one. So the
//! descriptor is **derived** from the bundle manifests, which carry every field
//! it needs — `dataset_iri`, `title`, `publisher`, `version`, `content_digest`,
//! `created`.
//!
//! `current` is then the greatest release under one total order: by `created`,
//! then by version label. Not by label alone, which doc 03 §3.2 allows to be "a
//! content hash prefix" — hash labels have no order, and a `latest` that
//! redirects to an arbitrary version is worse than one that does not exist. The
//! comparison is over parsed instants rather than the strings, because RFC 3339
//! lets the same instant be written several ways and two of them sort wrongly:
//! a `+01:00` offset, and a fractional second (`…:00.5Z` sorts before `…:00Z`).
//!
//! The fields a host cannot derive — preservation policy and authoritative
//! namespaces — are simply absent. Predicate roles are different: versioned
//! operations need a frozen interpretation profile, so this implementation
//! publishes the current release's manifest snapshot in the derived dataset
//! descriptor and uses that same snapshot for its immutable routes.
//!
//! # Startup is strict
//!
//! A manifest that does not parse, carries a digest that is not a digest, or
//! names a version other than its own directory stops the server with the path
//! in the message. The alternative — dropping that version from the catalog and
//! serving the rest — is the degraded mode doc 20 §20.8 refuses: a silently
//! missing version answers 404 for data that is on disk, and an operator who
//! deployed it has no way to see that.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use serde::Serialize;
use sha2::{Digest, Sha256};

use kgf_store::Capability;
use kgf_store::catalog::{BundleId, Catalog};
use kgf_store::manifest::{
    Manifest, Publisher, default_predicate_roles, validate_predicate_role_iri,
};
use kgf_store::store::{OpenOptions, Store, artifact};

use crate::Config;
use crate::admission::AdmissionController;
use crate::cursor::BundleBinding;
use crate::envelope::{ErrorCode, Problem, reflected};
use crate::representation::ContentDigest;
use crate::term::PrefixMap;

/// What stops the server from starting.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The caps and budgets this deployment publishes contradict each other.
    #[error("configuration: {detail}")]
    Configuration {
        /// What is inconsistent, and which two numbers disagree.
        detail: String,
    },

    /// A bundle's `manifest.json` is missing, unreadable, or not what it must
    /// be for the version to be addressable.
    #[error("bundle {dataset}/{version}: {detail}")]
    Manifest {
        /// The dataset directory.
        dataset: String,
        /// The version directory.
        version: String,
        /// What was wrong, and what to do about it.
        detail: String,
    },

    /// The catalog could not scan the bundle root.
    #[error(transparent)]
    Catalog(#[from] kgf_store::Error),
}

/// The bundles this process serves, and everything derived from them.
#[derive(Debug)]
pub struct Service {
    config: Config,
    catalog: Catalog,
    datasets: Datasets,
    descriptors: ContentDigest,
    admission: AdmissionController,
}

impl Service {
    /// Scan the configured root, read every manifest, and derive the
    /// descriptors.
    ///
    /// Reading N small JSON files is not the bundle opening that doc 20 §20.6
    /// keeps off the startup path: no artifact is mapped and no payload is
    /// touched. It is the minimum needed to answer `/` and `/{dataset}` and to
    /// resolve `latest`, all of which must work before any bundle is opened.
    pub fn build(config: Config) -> Result<Self, ServiceError> {
        // Before the scan: a deployment whose published numbers contradict each
        // other cannot answer correctly whatever is on disk, and the operator
        // should hear about it without waiting for a bundle to open.
        config
            .limits()
            .validate()
            .map_err(|detail| ServiceError::Configuration { detail })?;
        config
            .admission
            .validate()
            .map_err(|detail| ServiceError::Configuration { detail })?;
        let catalog = Catalog::scan(config.bundle_root.clone(), OpenOptions::default())?;
        let mut manifests = Vec::new();
        for id in catalog.ids() {
            let bundle = catalog.bundle_dir(&id)?;
            let manifest =
                PublishedManifest::read(bundle).map_err(|detail| ServiceError::Manifest {
                    dataset: id.dataset.clone(),
                    version: id.version.clone(),
                    detail,
                })?;
            manifests.push((id, manifest));
        }
        let datasets = Datasets::derive(manifests)?;
        datasets.validate_profile_caps(config.caps.max_search_predicates)?;
        let descriptors = descriptor_digest(&config, &datasets);
        let admission = AdmissionController::new(config.admission);
        Ok(Self {
            config,
            catalog,
            datasets,
            descriptors,
            admission,
        })
    }

    /// A validator for the mutable descriptors at `/` and `/{dataset}`.
    ///
    /// They have no `content_digest` of their own — they are derived, not
    /// published — but they are *fixed for the life of the process*: the
    /// catalog is scanned once and the caps come from an immutable [`Config`].
    /// So one digest over everything they are derived from is an honest strong
    /// validator, and it changes exactly when a restart picks up new bundles or
    /// new caps. Without one, a conditional request on a descriptor cannot be
    /// answered 304 at all, including RFC 9110 §13.1.2's `If-None-Match: *`.
    pub fn descriptor_digest(&self) -> &ContentDigest {
        &self.descriptors
    }

    /// This deployment's configuration, as published at `/`.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The datasets and their releases.
    pub fn datasets(&self) -> &Datasets {
        &self.datasets
    }

    /// Deployment-wide active-work and waiting-room gate.
    pub(crate) fn admission(&self) -> &AdmissionController {
        &self.admission
    }

    /// Open a bundle, or say why this request cannot be answered.
    ///
    /// **Blocking**: the first call for a version maps its artifacts and faults
    /// their preambles, so this belongs on a blocking pool and never on the
    /// async reactor (doc 20 §20.4).
    ///
    /// An open failure is [`ErrorCode::InternalError`] rather than a 4xx: the
    /// request is well formed, and what went wrong is that this deployment
    /// published a bundle it cannot serve. The classified store error goes to
    /// the log, where an operator will see the artifact and the command that
    /// builds it; the response says only which version failed, because the
    /// remedy names paths on the server's own disk and the client cannot act on
    /// it.
    pub fn open(&self, id: &BundleId) -> Result<Arc<Store>, Problem> {
        self.catalog.get(id).map_err(|error| {
            tracing::error!(
                dataset = %id.dataset,
                version = %id.version,
                error = %error,
                "bundle failed to open",
            );
            Problem::new(
                ErrorCode::InternalError,
                format!(
                    "bundle {}/{} is published but could not be opened",
                    id.dataset, id.version
                ),
            )
        })
    }
}

/// Digest the inputs every derived descriptor is built from.
///
/// Not the rendered documents: those differ per dataset and per
/// representation, and the representation is mixed into the `ETag` separately.
/// What is hashed is the deployment's identity — its caps, its budgets, and
/// every `(dataset, version, publication_digest)` it serves. The latter covers
/// the immutable request profile as well as the artifact checksum.
fn descriptor_digest(config: &Config, datasets: &Datasets) -> ContentDigest {
    let mut hasher = Sha256::new();
    // Length-prefixed, so no two different deployments hash alike by having
    // one field's end look like the next field's start.
    let mut field = |bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field(
        serde_json::to_vec(&config.caps)
            .expect("caps serialize")
            .as_slice(),
    );
    field(
        serde_json::to_vec(&config.budgets)
            .expect("budgets serialize")
            .as_slice(),
    );
    field(
        config
            .public_origin
            .as_ref()
            .map_or(&[], |origin| origin.as_str().as_bytes()),
    );
    field(env!("CARGO_PKG_VERSION").as_bytes());
    for name in datasets.names() {
        field(name.as_bytes());
        let dataset = datasets
            .get(name)
            .expect("a name this map just yielded is in it");
        field(dataset.current().as_bytes());
        for (version, release) in dataset.releases() {
            field(version.as_bytes());
            field(release.digest().as_str().as_bytes());
        }
    }
    ContentDigest::parse(&format!("sha256:{:x}", hasher.finalize()))
        .expect("a sha256 hex digest is a content digest")
}

/// A bundle's manifest as it was read: the bytes, and the parse of them.
///
/// Both, because they answer different questions. The bytes are what
/// `/manifest` serves, so that a document written by a newer builder keeps the
/// fields this build does not model (doc 04 §4.3). The parse is what the page,
/// the release ordering and the `ETag` are built from.
#[derive(Debug, Clone)]
pub struct PublishedManifest {
    bytes: Bytes,
    parsed: Arc<Manifest>,
    digest: ContentDigest,
}

impl PublishedManifest {
    /// Read `manifest.json` from a bundle directory.
    ///
    /// The file is read twice — once for the bytes, once through
    /// [`Manifest::read`], which is what applies doc 04 §4.3's
    /// `formats.manifest` check. Two reads of a few kilobytes, once per bundle
    /// at startup, is worth not duplicating that check here.
    fn read(bundle_dir: &Path) -> Result<Self, String> {
        let path = bundle_dir.join(artifact::MANIFEST);
        let bytes = std::fs::read(&path).map_err(|error| {
            format!(
                "{} could not be read ({error}); write one with `kgf manifest {}`",
                path.display(),
                bundle_dir.display()
            )
        })?;
        let parsed = Manifest::read(bundle_dir).map_err(|error| {
            format!(
                "{error}; regenerate it with `kgf manifest {}`",
                bundle_dir.display()
            )
        })?;
        let digest = bytes_digest(&bytes);
        Ok(Self {
            bytes: bytes.into(),
            parsed: Arc::new(parsed),
            digest,
        })
    }

    /// Pair a manifest with its canonical serialization.
    ///
    /// `#[cfg(test)]`: every real path reaches a `PublishedManifest` through
    /// [`read`](Self::read), and this one substitutes a re-serialization for
    /// the bytes on disk — which is the one thing the type's own contract says
    /// must not happen, since a manifest written by a newer builder would lose
    /// the fields this build cannot model.
    #[cfg(test)]
    pub(crate) fn of(parsed: Manifest) -> Result<Self, String> {
        let bytes = parsed.to_json_bytes().map_err(|error| error.to_string())?;
        let digest = bytes_digest(&bytes);
        Ok(Self {
            bytes: bytes.into(),
            parsed: Arc::new(parsed),
            digest,
        })
    }

    /// The document as published.
    pub fn bytes(&self) -> Bytes {
        self.bytes.clone()
    }

    /// The parse.
    pub fn parsed(&self) -> Arc<Manifest> {
        Arc::clone(&self.parsed)
    }

    /// Identity of the complete immutable publication profile.
    ///
    /// The artifact `content_digest` alone does not cover prefixes or
    /// predicate roles, both of which change the meaning of versioned
    /// requests. ETags and cursors therefore bind to the manifest bytes while
    /// release-history metadata continues to publish the artifact digest.
    pub fn digest(&self) -> &ContentDigest {
        &self.digest
    }
}

fn bytes_digest(bytes: &[u8]) -> ContentDigest {
    ContentDigest::parse(&format!("sha256:{:x}", Sha256::digest(bytes)))
        .expect("a SHA-256 digest is a content digest")
}

/// Every dataset this deployment hosts.
#[derive(Debug, Default)]
pub struct Datasets(BTreeMap<String, Dataset>);

impl Datasets {
    /// Build the descriptors from the manifests of every scanned bundle.
    ///
    /// Pure, so the derivation rules — which release is `current`, which
    /// identity fields a dataset takes, which manifests are refused — are
    /// testable without a filesystem.
    pub fn derive(
        manifests: impl IntoIterator<Item = (BundleId, PublishedManifest)>,
    ) -> Result<Self, ServiceError> {
        let mut datasets: BTreeMap<String, BTreeMap<String, Release>> = BTreeMap::new();

        for (id, published) in manifests {
            let manifest = published.parsed();
            let refuse = |detail: String| ServiceError::Manifest {
                dataset: id.dataset.clone(),
                version: id.version.clone(),
                detail,
            };

            // The directory names are what every URL, cursor and ETag is keyed
            // on. A manifest naming a different version would be describing
            // some other release under this one's URL.
            if manifest.version != id.version {
                return Err(refuse(format!(
                    "manifest.json declares version {:?} but sits in the directory for {:?}; \
                     regenerate it with `kgf manifest`",
                    manifest.version, id.version
                )));
            }
            let digest = ContentDigest::parse(&manifest.content_digest).ok_or_else(|| {
                refuse(format!(
                    "content_digest {:?} is not `{{algorithm}}:{{lowercase hex}}`; \
                     regenerate it with `kgf manifest`",
                    manifest.content_digest
                ))
            })?;
            let version_digest = published.digest().clone();
            let predicate_roles = PredicateRoles::from_manifest(&manifest).map_err(&refuse)?;
            let created = match manifest.created.as_deref() {
                None => None,
                Some(text) => Some(parse_rfc3339(text).ok_or_else(|| {
                    refuse(format!("created {text:?} is not an RFC 3339 timestamp"))
                })?),
            };

            datasets.entry(id.dataset).or_default().insert(
                id.version,
                Release {
                    content_digest: digest,
                    version_digest,
                    created,
                    // Copied once per bundle rather than once per request: a
                    // manifest cannot change while the version exists, and an
                    // OKN graph declaring fifty prefixes would otherwise
                    // allocate a hundred strings to read something fixed.
                    prefixes: PrefixMap::from_manifest(&manifest),
                    predicate_roles,
                    manifest: published,
                },
            );
        }

        Ok(Self(
            datasets
                .into_iter()
                .map(|(name, releases)| (name, Dataset::assemble(releases)))
                .collect(),
        ))
    }

    /// Every dataset name, in the order `/` lists them.
    pub fn names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Every dataset with its name, in the same order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &Dataset)> {
        self.0
            .iter()
            .map(|(name, dataset)| (name.as_str(), dataset))
    }

    /// Look up a dataset, or the 404 that says it is not hosted here.
    pub fn get(&self, dataset: &str) -> Result<&Dataset, Problem> {
        self.0.get(dataset).ok_or_else(|| {
            Problem::new(
                ErrorCode::NotFound,
                format!(
                    "this server hosts no dataset named {:?}; GET / lists the {} it does host",
                    reflected(dataset),
                    self.0.len()
                ),
            )
        })
    }

    /// Look up one release of one dataset.
    ///
    /// The two failures are distinct on purpose: "no such dataset" and "no such
    /// version of a dataset that exists" have different remedies, and a client
    /// that cannot tell them apart cannot tell a typo from a version that has
    /// been retired.
    pub fn release(&self, dataset: &str, version: &str) -> Result<&Release, Problem> {
        let found = self.get(dataset)?;
        found.releases.get(version).ok_or_else(|| {
            Problem::new(
                ErrorCode::NotFound,
                format!(
                    "dataset {:?} has no version {:?}; GET /{} lists its releases",
                    reflected(dataset),
                    reflected(version),
                    reflected(dataset),
                ),
            )
        })
    }

    /// Refuse an immutable label cascade this deployment cannot execute inside
    /// its published predicate cap. Search can reject a client-selected union,
    /// but `/labels` always uses the release profile, so letting an oversized
    /// one start would make every valid labels request fail for server policy.
    fn validate_profile_caps(&self, max_predicates: u32) -> Result<(), ServiceError> {
        for (dataset, found) in &self.0 {
            for (version, release) in &found.releases {
                let labels = release.predicate_roles.get("label").unwrap_or_default();
                if labels.len() > max_predicates as usize {
                    return Err(ServiceError::Manifest {
                        dataset: dataset.clone(),
                        version: version.clone(),
                        detail: format!(
                            "the label role has {} predicates, over this deployment's max_search_predicates of {max_predicates}",
                            labels.len()
                        ),
                    });
                }
            }
        }
        Ok(())
    }
}

/// One dataset: its identity, its releases, and which of them is current.
#[derive(Debug)]
pub struct Dataset {
    current: String,
    releases: BTreeMap<String, Release>,
}

impl Dataset {
    fn assemble(releases: BTreeMap<String, Release>) -> Self {
        // One total order: `created`, then the version label. A release whose
        // manifest omits `created` sorts below every dated one, which is a
        // consequence of `None < Some` rather than a rule of its own.
        let current = releases
            .iter()
            .max_by(|(left_version, left), (right_version, right)| {
                (left.created, *left_version).cmp(&(right.created, *right_version))
            })
            .map(|(version, _)| version.clone())
            .expect("a dataset exists because a version of it was scanned");
        Self { current, releases }
    }

    /// The version `latest` resolves to.
    pub fn current(&self) -> &str {
        &self.current
    }

    /// The release `latest` resolves to.
    pub fn current_release(&self) -> &Release {
        &self.releases[&self.current]
    }

    /// The releases this deployment holds, by label.
    pub fn releases(&self) -> impl ExactSizeIterator<Item = (&str, &Release)> {
        self.releases
            .iter()
            .map(|(version, release)| (version.as_str(), release))
    }

    /// The dataset's globally stable identity (doc 04 §4.3), if it declares one.
    ///
    /// Taken from the current release: identity is a property of the logical
    /// dataset, and the newest release is this deployment's best statement of
    /// what that is.
    pub fn dataset_iri(&self) -> Option<&str> {
        self.current_release()
            .manifest
            .parsed
            .dataset_iri
            .as_deref()
    }

    /// A short human title, from the current release.
    pub fn title(&self) -> Option<&str> {
        self.current_release().manifest.parsed.title.as_deref()
    }

    /// A longer human description, from the current release.
    pub fn description(&self) -> Option<&str> {
        self.current_release()
            .manifest
            .parsed
            .description
            .as_deref()
    }

    /// Who published it, from the current release.
    pub fn publisher(&self) -> Option<&Publisher> {
        self.current_release().manifest.parsed.publisher.as_ref()
    }

    /// The current release's frozen predicate-role profile.
    pub fn predicate_roles(&self) -> &PredicateRoles {
        self.current_release().predicate_roles()
    }

    /// The current release's triple count, for the catalog.
    pub fn triples(&self) -> u64 {
        self.current_release().manifest.parsed.counts.triples
    }

    /// The capability names the current release declares, in manifest order.
    pub fn capabilities(&self) -> impl ExactSizeIterator<Item = &str> {
        self.current_release()
            .manifest
            .parsed
            .capabilities
            .keys()
            .map(String::as_str)
    }
}

/// A release's named predicate groups, expanded to full IRIs.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(transparent)]
pub struct PredicateRoles(BTreeMap<String, Vec<String>>);

impl PredicateRoles {
    fn from_manifest(manifest: &Manifest) -> Result<Self, String> {
        let roles = if manifest.predicate_roles.is_empty() {
            default_predicate_roles()
        } else {
            manifest.predicate_roles.clone()
        };
        for (role, predicates) in &roles {
            if role.is_empty()
                || !role
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                return Err(format!(
                    "predicate role {role:?} is not an ASCII name made of letters, digits, `_` or `-`"
                ));
            }
            if predicates.is_empty() {
                return Err(format!("predicate role {role:?} has no predicate IRIs"));
            }
            let mut seen = std::collections::BTreeSet::new();
            for iri in predicates {
                validate_predicate_role_iri(iri, &manifest.prefixes).map_err(|detail| {
                    format!(
                        "predicate role {role:?} contains {iri:?}, which is not a full predicate IRI: {detail}"
                    )
                })?;
                if !seen.insert(iri) {
                    return Err(format!(
                        "predicate role {role:?} repeats predicate IRI {iri:?}"
                    ));
                }
            }
        }
        Ok(Self(roles))
    }

    /// Predicates in `role`, strongest first.
    pub fn get(&self, role: &str) -> Option<&[String]> {
        self.0.get(role).map(Vec::as_slice)
    }

    /// Every declared role and its ordered predicates.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &[String])> {
        self.0
            .iter()
            .map(|(role, predicates)| (role.as_str(), predicates.as_slice()))
    }
}

/// One published version of a dataset.
#[derive(Debug)]
pub struct Release {
    content_digest: ContentDigest,
    version_digest: ContentDigest,
    created: Option<Instant>,
    prefixes: PrefixMap,
    predicate_roles: PredicateRoles,
    manifest: PublishedManifest,
}

impl Release {
    /// The version's canonical identity (doc 04 §4.3), and the ETag's data half.
    pub fn digest(&self) -> &ContentDigest {
        &self.version_digest
    }

    /// Digest of the artifact bytes, as published in release history.
    pub fn content_digest(&self) -> &ContentDigest {
        &self.content_digest
    }

    /// The manifest as published, and its parse.
    pub fn manifest(&self) -> &PublishedManifest {
        &self.manifest
    }

    /// The CURIE prefixes this version's parameters accept (doc 03 §3.3).
    pub fn prefixes(&self) -> &PrefixMap {
        &self.prefixes
    }

    /// Named predicate groups used by this immutable release.
    pub fn predicate_roles(&self) -> &PredicateRoles {
        &self.predicate_roles
    }

    /// What a cursor issued against this version must match.
    ///
    /// Derived here rather than per request, and infallible: a digest that
    /// would not parse stopped the server at startup, and one that parses as a
    /// [`ContentDigest`] has at least eight bytes of hex.
    pub fn binding(&self) -> BundleBinding {
        BundleBinding::from_content_digest(self.version_digest.as_str())
            .expect("a parsed content digest is a cursor binding")
    }

    /// Whether this bundle declares `capability` (doc 04 §4.3).
    pub fn declares(&self, capability: Capability) -> bool {
        self.manifest.parsed.declares(capability)
    }

    /// Whether this release carries the complete Tier-1 description set.
    pub fn carries_description(&self) -> bool {
        self.manifest.parsed.carries_description_artifacts()
    }
}

// ---------------------------------------------------------------------------
// Instants
// ---------------------------------------------------------------------------

/// Parse RFC 3339's `date-time` into an instant that orders correctly.
///
/// `jiff` rather than the strings, because two spellings of one instant must
/// compare equal and two instants must compare in the right order, and the text
/// does neither: `2026-06-01T09:00:00-05:00` sorts before `2026-06-01T13:00:00Z`
/// although it is an hour later, and `…:00.5Z` sorts before `…:00Z` although it
/// is half a second later.
///
/// `jiff` parses ISO 8601, which is a superset, so the offset is gated first.
/// That is not pedantry: `+25:00` is not an offset, and jiff reads it as one
/// and lands a day earlier — a manifest with a typo in its timestamp would sort
/// as a release from the previous day and could take `current` from the version
/// that should have it. Startup is meant to stop on a manifest like that
/// (§20.8), so it does. The rest of RFC 3339 — the calendar, the leap day, the
/// fractional second — stays jiff's, because that is the part worth ninety
/// lines to get wrong.
fn parse_rfc3339(text: &str) -> Option<Instant> {
    offset_is_rfc_3339(text).then(|| text.parse().ok())?
}

/// Whether `text` ends in RFC 3339's `time-offset`: `Z`, or `±HH:MM` in range.
fn offset_is_rfc_3339(text: &str) -> bool {
    let two_digits = |text: &str, limit: u8| {
        text.len() == 2
            && text.bytes().all(|byte| byte.is_ascii_digit())
            && text.parse::<u8>().is_ok_and(|value| value <= limit)
    };
    match text.as_bytes() {
        [.., b'Z' | b'z'] => true,
        // `time-numoffset = ("+" / "-") time-hour ":" time-minute`, and the
        // two components carry `time-hour`'s and `time-minute`'s own ranges.
        [.., sign, _, _, b':', _, _] if *sign == b'+' || *sign == b'-' => {
            let offset = &text[text.len() - 6..];
            two_digits(&offset[1..3], 23) && two_digits(&offset[4..6], 59)
        }
        _ => false,
    }
}

use jiff::Timestamp as Instant;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::DatasetDescriptor;
    use crate::html::Resource;
    use kgf_store::manifest::{Counts, Formats};

    fn published(version: &str, digest_byte: &str, created: Option<&str>) -> PublishedManifest {
        PublishedManifest::of(Manifest {
            id: "tox".to_owned(),
            dataset_iri: Some("https://okn.example/id/tox".to_owned()),
            version: version.to_owned(),
            content_digest: format!("sha256:{}", digest_byte.repeat(16)),
            created: created.map(str::to_owned),
            formats: Formats::default(),
            title: Some(format!("Tox as of {version}")),
            description: None,
            license: None,
            homepage: None,
            publisher: None,
            counts: Counts {
                triples: 8,
                subjects: 3,
                predicates: 4,
                objects: 6,
            },
            capabilities: BTreeMap::new(),
            prefixes: BTreeMap::new(),
            predicate_roles: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            previous_version: None,
        })
        .expect("a manifest serializes")
    }

    fn id(dataset: &str, version: &str) -> BundleId {
        BundleId {
            dataset: dataset.to_owned(),
            version: version.to_owned(),
        }
    }

    #[test]
    fn current_is_the_most_recently_created_release() {
        // Labels that sort the wrong way round, so this can only pass by
        // reading `created` — doc 03 §3.2 allows "a content hash prefix" as a
        // version label, and those have no order at all.
        let datasets = Datasets::derive([
            (
                id("tox", "aa11"),
                published("aa11", "1", Some("2026-06-01T14:03:22Z")),
            ),
            (
                id("tox", "bb22"),
                published("bb22", "2", Some("2026-01-09T09:00:00Z")),
            ),
        ])
        .expect("well-formed manifests");

        assert_eq!(datasets.get("tox").unwrap().current(), "aa11");
    }

    #[test]
    fn releases_are_ordered_by_instant_and_not_by_text() {
        // The two spellings RFC 3339 allows that sort wrongly as strings: an
        // offset, and a fractional second. As text, "2026-06-01T09:00:00-05:00"
        // sorts before "2026-06-01T13:00:00Z" although it is an hour later, and
        // "…:00.5Z" sorts before "…:00Z" although it is half a second later.
        let datasets = Datasets::derive([
            (
                id("offset", "later"),
                published("later", "1", Some("2026-06-01T09:00:00-05:00")),
            ),
            (
                id("offset", "earlier"),
                published("earlier", "2", Some("2026-06-01T13:00:00Z")),
            ),
            (
                id("fraction", "later"),
                published("later", "3", Some("2026-06-01T13:00:00.5Z")),
            ),
            (
                id("fraction", "earlier"),
                published("earlier", "4", Some("2026-06-01T13:00:00Z")),
            ),
        ])
        .expect("well-formed manifests");

        assert_eq!(datasets.get("offset").unwrap().current(), "later");
        assert_eq!(datasets.get("fraction").unwrap().current(), "later");
    }

    #[test]
    fn an_undated_release_loses_to_a_dated_one_and_ties_break_on_the_label() {
        let datasets = Datasets::derive([
            (id("tox", "a"), published("a", "1", None)),
            (id("tox", "b"), published("b", "2", None)),
            (
                id("tox", "c"),
                published("c", "3", Some("2020-01-01T00:00:00Z")),
            ),
        ])
        .expect("well-formed manifests");
        assert_eq!(datasets.get("tox").unwrap().current(), "c");

        let undated = Datasets::derive([
            (id("tox", "a"), published("a", "1", None)),
            (id("tox", "b"), published("b", "2", None)),
        ])
        .expect("well-formed manifests");
        assert_eq!(undated.get("tox").unwrap().current(), "b");
    }

    #[test]
    fn a_dataset_takes_its_identity_from_its_current_release() {
        let datasets = Datasets::derive([
            (
                id("tox", "old"),
                published("old", "1", Some("2020-01-01T00:00:00Z")),
            ),
            (
                id("tox", "new"),
                published("new", "2", Some("2026-01-01T00:00:00Z")),
            ),
        ])
        .expect("well-formed manifests");

        let dataset = datasets.get("tox").unwrap();
        let descriptor = DatasetDescriptor::of("tox", dataset);
        let json: serde_json::Value = serde_json::from_slice(&descriptor.to_json()).unwrap();
        assert_eq!(json["current"], "new");
        assert!(
            json.get("latest").is_none(),
            "doc 04 names the machine-readable field `current`"
        );
        assert_eq!(json["title"], "Tox as of new");
        assert_eq!(json["dataset_iri"], "https://okn.example/id/tox");
        // Every release is listed, whether or not it is current — a client
        // paging an older version needs to find it.
        assert_eq!(json["releases"].as_array().unwrap().len(), 2);
        assert_eq!(json["releases"][0]["version"], "new");
        assert_eq!(json["releases"][0]["url"], "/tox/v/new/");
        assert_eq!(
            json["releases"][0]["links"]["manifest"],
            "/tox/v/new/manifest"
        );
        assert!(json["releases"][0]["links"].get("summary").is_none());
        assert!(
            json["releases"][0]["content_digest"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );

        // The human page calls the movable selection `latest`, matching its
        // `/{dataset}/latest/…` URL, while the JSON above keeps doc 04's field.
        let page = descriptor.to_html();
        assert!(page.contains("href=\"/tox/v/new/manifest\""));
        assert!(page.contains("href=\"/tox/v/old/manifest\""));
        assert_eq!(page.matches(">latest<").count(), 2);
        assert!(!page.contains(">current<"));
    }

    #[test]
    fn an_unknown_dataset_and_an_unknown_version_are_different_answers() {
        let datasets =
            Datasets::derive([(id("tox", "v1"), published("v1", "1", None))]).expect("well-formed");

        let no_dataset = datasets.release("nope", "v1").unwrap_err();
        let no_version = datasets.release("tox", "nope").unwrap_err();
        assert_eq!(no_dataset.code(), ErrorCode::NotFound);
        assert_eq!(no_version.code(), ErrorCode::NotFound);

        let detail = |problem: &Problem| {
            serde_json::to_value(problem).unwrap()["detail"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        // A client that cannot tell these apart cannot tell a typo in the
        // dataset name from a version that has been retired.
        assert!(detail(&no_dataset).contains("GET /"));
        assert!(detail(&no_version).contains("GET /tox"));
        assert_ne!(detail(&no_dataset), detail(&no_version));
    }

    #[test]
    fn a_manifest_that_describes_another_version_is_refused() {
        // It would otherwise be served under this directory's URL, with this
        // directory's label in every cursor and ETag, while saying it is a
        // different release.
        let error =
            Datasets::derive([(id("tox", "2026-06-01"), published("2026-03-01", "1", None))])
                .expect_err("the label and the directory disagree");
        let message = error.to_string();
        assert!(
            message.contains("2026-06-01") && message.contains("2026-03-01"),
            "{message}"
        );
        assert!(message.contains("kgf manifest"), "{message}");
    }

    #[test]
    fn a_digest_or_timestamp_that_cannot_be_parsed_stops_the_server() {
        let mut bad_digest = (*published("v1", "1", None).parsed()).clone();
        bad_digest.content_digest = "not-a-digest".to_owned();
        let error = Datasets::derive([(
            id("tox", "v1"),
            PublishedManifest::of(bad_digest).expect("serializes"),
        )])
        .expect_err("bad digest");
        assert!(error.to_string().contains("content_digest"), "{error}");

        let error = Datasets::derive([(id("tox", "v1"), published("v1", "1", Some("June 2026")))])
            .expect_err("bad timestamp");
        assert!(error.to_string().contains("RFC 3339"), "{error}");
    }

    #[test]
    fn rfc_3339_parses_the_forms_a_manifest_may_carry() {
        let at = |text: &str| parse_rfc3339(text).unwrap_or_else(|| panic!("{text}"));

        assert_eq!(at("1970-01-01T00:00:00Z").as_second(), 0);
        assert_eq!(at("2026-06-01T14:03:22Z").as_second(), 1_780_322_602);
        // The same instant three ways.
        assert_eq!(at("2026-06-01T14:03:22Z"), at("2026-06-01T15:03:22+01:00"));
        assert_eq!(at("2026-06-01T14:03:22Z"), at("2026-06-01T09:03:22-05:00"));
        assert_eq!(at("2026-06-01t14:03:22z"), at("2026-06-01T14:03:22Z"));
        // Fractional seconds order within a second.
        assert!(at("2026-06-01T14:03:22.5Z") > at("2026-06-01T14:03:22Z"));
        assert_eq!(
            at("2026-06-01T14:03:22.5Z").subsec_nanosecond(),
            500_000_000
        );
        // Dates before the epoch, and a leap day.
        assert!(at("1969-12-31T23:59:59Z").as_second() < 0);
        assert!(parse_rfc3339("2024-02-29T00:00:00Z").is_some());
    }

    #[test]
    fn rfc_3339_refuses_what_it_cannot_order() {
        for text in [
            "",
            "2026-06-01",
            "2026-06-01T14:03:22",       // no offset: the instant is unknown
            "2026-13-01T00:00:00Z",      // month
            "2026-02-30T00:00:00Z",      // day, in a month that is short
            "2023-02-29T00:00:00Z",      // day, in a year that is not leap
            "2026-06-01T24:00:00Z",      // hour
            "2026-06-01T14:61:22Z",      // minute
            "2026-06-01T14:03:61Z",      // second, one past the leap second
            "2026-06-01T14:03:22+0x:00", // offset digits
            "2026-06-01T14:03:22.Z",     // an empty fraction
            "20x6-06-01T14:03:22Z",
            "June 2026",
        ] {
            assert!(parse_rfc3339(text).is_none(), "{text:?} must not parse");
        }
    }

    #[test]
    fn an_offset_that_is_not_an_offset_is_refused() {
        // The gate in front of jiff, which parses ISO 8601 and would take all
        // of these. `+25:00` is the one that matters: jiff reads it as a real
        // offset and lands a day earlier, so a typo in a timestamp could take
        // `current` from the release that should have it.
        for text in [
            "2026-06-01T14:03:22+25:00", // an offset hour past 23
            "2026-06-01T14:03:22+01:70", // an offset minute past 59
            "2026-06-01T14:03:22+01",    // an offset without its minutes
            "2026-06-01T14:03:22+0100",  // an offset without its colon
            "2026-06-01T14:03:22",       // no offset: the instant is unknown
        ] {
            assert!(parse_rfc3339(text).is_none(), "{text:?}");
        }

        // Still accepted, and still looser than §5.6's grammar: a space in
        // place of `T`, which that section's own note allows.
        assert!(parse_rfc3339("2026-06-01 14:03:22Z").is_some());
        assert_eq!(
            parse_rfc3339("2026-06-01T14:03:22+23:59"),
            parse_rfc3339("2026-05-31T14:04:22Z"),
            "an offset at the edge of the range is still an offset"
        );
    }
}
