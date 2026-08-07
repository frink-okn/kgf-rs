//! `kgf manifest` — write a bundle's `manifest.json` from its artifacts.
//!
//! A stand-in for the manifest step of `kgf build`, so that bundles assembled by
//! hand with `hdtc create --perm` are complete bundles rather than directories
//! with a placeholder. It builds nothing: the artifacts must already exist, and
//! this only describes them.
//!
//! # What is derived and what is asked for
//!
//! Everything structural comes from the bundle: counts from
//! [`kgf_store::BundleFacts`], capabilities from which sidecars are
//! present, sizes and checksums from the files, and `content_digest` from those
//! checksums. Those are the fields that cannot be written by hand correctly, and
//! the ones that rot silently when artifacts are rebuilt — doc 03 §3.4.10 makes
//! the counts load-bearing, since the VoID document's numbers must equal
//! `/count` results.
//!
//! What is asked for is identity and description: `--id`, `--version`, prefixes,
//! title, license. Those are re-read from any manifest already present, so
//! regenerating after a rebuild is `kgf manifest <dir>` with no flags.

#![allow(clippy::too_many_arguments)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
#[cfg(test)]
use kgf_store::manifest::ArtifactView;
use kgf_store::manifest::{
    ArtifactDigest, ArtifactEntry, BundleFacts, Capability, Formats, Manifest, ManifestDocument,
    Publisher, content_digest_preimage, default_predicate_roles, validate_predicate_role_iri,
};
use kgf_store::store::artifact;
use kgf_store::{PublishedBundle, verify_description_indexes};
use sha2::{Digest, Sha256};

/// Arguments for `kgf manifest`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The bundle version directory to describe.
    pub bundle: PathBuf,

    /// Verify the existing manifest against the artifacts instead of writing one.
    #[arg(long)]
    pub check: bool,

    /// Dataset slug. Defaults to the bundle directory's parent name.
    #[arg(long)]
    pub id: Option<String>,

    /// Version label. Defaults to the bundle directory's name.
    #[arg(long)]
    pub version: Option<String>,

    /// Globally stable dataset IRI.
    #[arg(long)]
    pub dataset_iri: Option<String>,

    /// Short human title.
    #[arg(long)]
    pub title: Option<String>,

    /// Longer description.
    #[arg(long)]
    pub description: Option<String>,

    /// License identifier or URL.
    #[arg(long)]
    pub license: Option<String>,

    /// Project homepage.
    #[arg(long)]
    pub homepage: Option<String>,

    /// Publisher display name.
    #[arg(long)]
    pub publisher: Option<String>,

    /// Publisher contact address.
    #[arg(long)]
    pub publisher_contact: Option<String>,

    /// The version this one supersedes.
    #[arg(long)]
    pub previous_version: Option<String>,

    /// A prefix binding, as `prefix=expansion`. Repeatable.
    ///
    /// Replaces the prefix map rather than merging into it, so that removing a
    /// prefix is possible; pass none to keep whatever the current manifest has.
    #[arg(long = "prefix", value_name = "PREFIX=IRI")]
    pub prefixes: Vec<String>,

    /// A predicate-role member, as `role=IRI`. Repeatable and ordered.
    ///
    /// Passing any role replaces the complete role map, so removing a stale
    /// declaration is possible; pass none to retain the current manifest's
    /// frozen profile. IRIs are written in full because this is the immutable
    /// snapshot request parsing will rely on, not another layer of aliases.
    #[arg(long = "role", value_name = "ROLE=IRI")]
    pub roles: Vec<String>,
}

/// Run `kgf manifest`.
pub fn run(args: Args) -> Result<()> {
    let dir = args
        .bundle
        .canonicalize()
        .with_context(|| format!("resolving bundle directory {}", args.bundle.display()))?;

    let BundleInspection { bundle, facts } = inspect_bundle(&dir)?;
    verify_text_binding(&dir)?;

    if args.check {
        let manifest = Manifest::read(&dir)?;
        // Counts first: they are free, and they are what catches a rebuild that
        // changed the data. Then the bytes, which catch a rebuild that did not.
        manifest.verify_against(&facts, &dir)?;
        verify_described_artifacts(&manifest, &dir, &facts)?;
        verify_description_indexes(&bundle, &manifest)?;
        println!(
            "{}: manifest agrees with its artifacts ({} triples, {})",
            dir.display(),
            facts.triples(),
            manifest.content_digest,
        );
        return Ok(());
    }

    // Any manifest already here supplies defaults, so regenerating after a
    // rebuild needs no flags, and its unmodeled fields survive the rewrite. A
    // placeholder that does not parse is not an error: supplying one is how a
    // bundle gets its first real manifest.
    let document = ManifestDocument::read(&dir)?;
    let manifest = build(
        &args,
        &dir,
        &facts,
        document.as_ref().and_then(ManifestDocument::parsed),
    )?;
    manifest.validate(&dir)?;
    verify_description_indexes(&bundle, &manifest)?;

    let bytes = match &document {
        Some(document) => document.rewrite_with(&manifest)?,
        None => manifest.to_json_bytes()?,
    };
    let path = dir.join("manifest.json");
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;

    println!(
        "{}: wrote manifest for {}/{} ({} triples, {})",
        path.display(),
        manifest.id,
        manifest.version,
        manifest.counts.triples,
        manifest.content_digest,
    );
    Ok(())
}

/// The published capability and bounded facts used throughout one invocation.
struct BundleInspection {
    bundle: PublishedBundle,
    facts: BundleFacts,
}

/// Read the bundle's structural facts without scanning description indexes.
///
/// # Safety obligation
///
/// [`PublishedBundle::new`](kgf_store::PublishedBundle::new) requires that the
/// artifacts not be modified or truncated while mapped. This is a one-shot
/// command over a directory the operator named, holding the mappings only for
/// the duration of this call and writing nothing but `manifest.json`, which is
/// not among the mapped artifacts. Establishing that obligation explicitly is
/// what the capability exists for (doc 20 §20.9); the rest of this crate keeps
/// `unsafe` denied.
#[allow(unsafe_code)]
fn inspect_bundle(dir: &Path) -> Result<BundleInspection> {
    let bundle = unsafe { kgf_store::PublishedBundle::new(dir) };
    let facts = BundleFacts::read(&bundle)?;
    Ok(BundleInspection { bundle, facts })
}

/// Check that the manifest describes the artifacts *byte for byte*, not merely
/// in cardinality.
///
/// `Manifest::verify_against` compares counts, which is all the read layer can
/// afford — full digests are off the open path by design (doc 20 §20.6). But
/// counts are a weak witness for a rebuild: editing one literal leaves all four
/// unchanged and rewrites every artifact, so a counts-only check passes a
/// manifest whose `content_digest` is stale. That digest is a version's
/// canonical identity — ETags and cursor binding are derived from it — so a
/// build-side command that already hashes every artifact to *write* one should
/// hash them to *check* one.
///
/// Two comparisons cover it. The capability and artifact sets catch a sidecar
/// added or removed since the manifest was written, which no count reflects.
/// The per-artifact checksums catch changed bytes, and the recomputed digest
/// catches the remaining case of a hand-edited `content_digest`.
fn verify_described_artifacts(manifest: &Manifest, dir: &Path, facts: &BundleFacts) -> Result<()> {
    let remedy = format!("kgf manifest {}", dir.display());

    let declared: BTreeSet<&str> = manifest.capabilities.keys().map(String::as_str).collect();
    let supported: BTreeSet<&str> = facts.capabilities().map(Capability::as_str).collect();
    if declared != supported {
        bail!(
            "manifest {} declares capabilities [{}], but the artifacts support [{}]; \
             regenerate it with `{remedy}`",
            manifest_path(dir).display(),
            join(&declared),
            join(&supported),
        );
    }

    let computed = checksum_artifacts(dir, facts)?;
    let listed: BTreeSet<&str> = manifest.artifacts.keys().map(String::as_str).collect();
    let present: BTreeSet<&str> = computed.iter().map(|(name, _)| name.as_str()).collect();
    if listed != present {
        bail!(
            "manifest {} lists artifacts [{}], but the bundle contains [{}]; \
             regenerate it with `{remedy}`",
            manifest_path(dir).display(),
            join(&listed),
            join(&present),
        );
    }

    for (name, actual) in &computed {
        let recorded = manifest
            .artifacts
            .get(name)
            .expect("artifact sets were just compared");
        if recorded.bytes != actual.bytes || recorded.sha256 != actual.sha256 {
            bail!(
                "manifest {} records {name} as {} bytes, sha256 {}, but it is {} bytes, \
                 sha256 {}; regenerate it with `{remedy}`",
                manifest_path(dir).display(),
                recorded.bytes,
                recorded.sha256,
                actual.bytes,
                actual.sha256,
            );
        }
    }

    let digest = content_digest(computed.iter().map(|(name, entry)| (name.as_str(), entry)));
    if manifest.content_digest != digest {
        bail!(
            "manifest {} records content_digest {} but its artifacts hash to {}; \
             regenerate it with `{remedy}`",
            manifest_path(dir).display(),
            manifest.content_digest,
            digest,
        );
    }
    Ok(())
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest.json")
}

fn join(names: &BTreeSet<&str>) -> String {
    names.iter().copied().collect::<Vec<_>>().join(", ")
}

/// Assemble the manifest from derived facts, supplied identity, and whatever the
/// current manifest already said.
fn build(
    args: &Args,
    dir: &Path,
    facts: &BundleFacts,
    previous: Option<&Manifest>,
) -> Result<Manifest> {
    let id = pick(&args.id, previous.map(|m| m.id.clone()), || {
        dir.parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .map(str::to_owned)
    })
    .context(
        "cannot infer a dataset id from the bundle path; pass --id \
         (the catalog layout is {root}/{dataset}/{version})",
    )?;

    let version = pick(&args.version, previous.map(|m| m.version.clone()), || {
        dir.file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
    })
    .context("cannot infer a version from the bundle path; pass --version")?;

    let mut artifacts = checksum_artifacts(dir, facts)?;
    carry_artifact_metadata(&mut artifacts, previous)?;
    let content_digest =
        content_digest(artifacts.iter().map(|(name, entry)| (name.as_str(), entry)));

    // `created` dates the bundle, not the manifest file, so regenerating over
    // unchanged artifacts must not restate it. That also keeps regeneration
    // byte-stable, which is what lets a manifest be diffed across versions and
    // keeps the digest it carries meaningful as an identity.
    let created = match previous {
        Some(previous) if previous.content_digest == content_digest => previous.created.clone(),
        _ => None,
    }
    .unwrap_or_else(now_rfc3339);
    let prefixes = prefixes(args, previous)?;
    let predicate_roles = predicate_roles(args, previous, &prefixes)?;

    Ok(Manifest {
        id,
        dataset_iri: args
            .dataset_iri
            .clone()
            .or_else(|| previous.and_then(|m| m.dataset_iri.clone())),
        version,
        content_digest,
        created: Some(created),
        formats: Formats::default(),
        title: args
            .title
            .clone()
            .or_else(|| previous.and_then(|m| m.title.clone())),
        description: args
            .description
            .clone()
            .or_else(|| previous.and_then(|m| m.description.clone())),
        license: args
            .license
            .clone()
            .or_else(|| previous.and_then(|m| m.license.clone())),
        homepage: args
            .homepage
            .clone()
            .or_else(|| previous.and_then(|m| m.homepage.clone())),
        publisher: publisher(args, previous),
        counts: facts.counts(),
        capabilities: facts
            .capabilities()
            .map(|capability| (capability.as_str().to_owned(), serde_json::json!({})))
            .collect(),
        prefixes,
        predicate_roles,
        artifacts: artifacts.into_iter().collect(),
        previous_version: args
            .previous_version
            .clone()
            .or_else(|| previous.and_then(|m| m.previous_version.clone())),
    })
}

fn predicate_roles(
    args: &Args,
    previous: Option<&Manifest>,
    prefixes: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Vec<String>>> {
    if args.roles.is_empty() {
        let roles = previous
            .map(|manifest| manifest.predicate_roles.clone())
            .filter(|roles| !roles.is_empty())
            .unwrap_or_else(default_predicate_roles);
        for (role, members) in &roles {
            for iri in members {
                validate_predicate_role_iri(iri, prefixes).map_err(|detail| {
                    anyhow::anyhow!(
                        "predicate role {role} does not contain a full predicate IRI {iri:?}: {detail}"
                    )
                })?;
            }
        }
        return Ok(roles);
    }

    let mut roles: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for declaration in &args.roles {
        let (role, iri) = declaration
            .split_once('=')
            .with_context(|| format!("--role {declaration} is not of the form role=IRI"))?;
        if role.is_empty() || iri.is_empty() {
            bail!("--role {declaration} has an empty role or IRI");
        }
        if !role
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            bail!(
                "--role {declaration} has an invalid role name; use ASCII letters, digits, `_` or `-`"
            );
        }
        validate_predicate_role_iri(iri, prefixes).map_err(|detail| {
            anyhow::anyhow!("--role {declaration} does not contain a full predicate IRI: {detail}")
        })?;
        let members = roles.entry(role.to_owned()).or_default();
        if members.iter().any(|member| member == iri) {
            bail!("--role {declaration} repeats the same predicate IRI");
        }
        members.push(iri.to_owned());
    }
    Ok(roles)
}

/// A flag, else what the current manifest said, else a value inferred from the
/// path.
fn pick(
    flag: &Option<String>,
    carried: Option<String>,
    infer: impl FnOnce() -> Option<String>,
) -> Option<String> {
    flag.clone().or(carried).or_else(infer)
}

fn publisher(args: &Args, previous: Option<&Manifest>) -> Option<Publisher> {
    let carried = previous.and_then(|m| m.publisher.clone());
    let name = args
        .publisher
        .clone()
        .or_else(|| carried.as_ref().map(|p| p.name.clone()))?;
    Some(Publisher {
        name,
        contact: args
            .publisher_contact
            .clone()
            .or_else(|| carried.and_then(|p| p.contact)),
    })
}

/// Prefixes every bundle declares unless it says otherwise.
///
/// Doc 03 §3.3 makes a CURIE resolvable only against a declared prefix, and an
/// undeclared one an error — so a bundle declaring nothing accepts no CURIE at
/// all, and even §3.4's own examples (`p=rdfs:label`, `o.ge="100.0"^^xsd:double`)
/// fail against it. These four are not a guess about the data: they are fixed by
/// the W3C specs that define RDF itself, so declaring them asserts nothing about
/// the dataset that is not already true everywhere.
///
/// Deliberately short. A curated vocabulary list (`skos`, `dcterms`, `foaf`, …)
/// would be this tool guessing at the dataset's subject matter, and getting a
/// prefix wrong is worse than omitting it: the manifest is the contract a client
/// reads to know what it may send.
const WELL_KNOWN_PREFIXES: [(&str, &str); 4] = [
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
];

fn prefixes(args: &Args, previous: Option<&Manifest>) -> Result<BTreeMap<String, String>> {
    // Seeded first so an explicit binding, or one carried forward from a
    // manifest that chose differently, overrides rather than collides.
    let mut prefixes: BTreeMap<String, String> = WELL_KNOWN_PREFIXES
        .iter()
        .map(|(prefix, namespace)| ((*prefix).to_owned(), (*namespace).to_owned()))
        .collect();

    if args.prefixes.is_empty() {
        if let Some(previous) = previous {
            prefixes.extend(previous.prefixes.clone());
        }
        return Ok(prefixes);
    }

    let mut declared = BTreeMap::new();
    for binding in &args.prefixes {
        let (prefix, expansion) = binding
            .split_once('=')
            .with_context(|| format!("--prefix {binding} is not of the form prefix=IRI"))?;
        if prefix.is_empty() || expansion.is_empty() {
            bail!("--prefix {binding} has an empty prefix or expansion");
        }
        if let Some(existing) = declared.insert(prefix.to_owned(), expansion.to_owned()) {
            bail!("--prefix {prefix} given twice ({existing} and {expansion})");
        }
    }
    prefixes.extend(declared);
    Ok(prefixes)
}

/// Refuse a text index that was not built from this bundle's `data.hdt`.
///
/// **This is the only place the check can happen**, and it is why it happens
/// here rather than at open. Every other sidecar carries cheap source metadata,
/// so `Store::open` rejects a foreign one for the price of a header read; a
/// text index records only a SHA-256 over the HDT payload, and verifying that
/// is a pass over the whole file — exactly the work doc 20 §20.3 keeps off the
/// open path. So the server trusts a described bundle, and this is where a
/// bundle becomes described.
///
/// The failure it prevents is the quiet kind: a hit is an object dictionary id,
/// so an index built from a *different* HDT returns ids that resolve to real
/// terms in this one, and every row would look well formed.
fn verify_text_binding(dir: &Path) -> Result<()> {
    let text = dir.join(artifact::TEXT);
    if !text.is_dir() {
        return Ok(());
    }
    hdtc::format::verify_text_index_binding(&text, &dir.join(artifact::HDT)).with_context(|| {
        format!(
            "{} does not belong to this bundle; rebuild it with `hdtc text {}`",
            text.display(),
            dir.join(artifact::HDT).display()
        )
    })
}

/// Size and SHA-256 every artifact the bundle declares.
///
/// The one place in KGF that reads whole artifacts. That is why it is here and
/// not in `kgf-store`: full digests belong to publish and `kgf verify`, never to
/// the latency-sensitive open path (doc 20 §20.6).
fn checksum_artifacts(dir: &Path, facts: &BundleFacts) -> Result<Vec<(String, ArtifactEntry)>> {
    facts
        .artifact_names()
        .map(|name| {
            let path = dir.join(name);
            Ok((name.to_owned(), checksum_artifact(&path)?))
        })
        .collect()
}

/// Compute one manifest artifact entry from the bytes at `path`.
///
/// Files are streamed through SHA-256. A directory is identified by the same
/// sorted relative-path/checksum construction used for `data.hdt.text`.
pub fn checksum_artifact(path: &Path) -> Result<ArtifactEntry> {
    let (bytes, sha256) = if path.is_dir() {
        sha256_dir(path)
    } else {
        sha256_file(path)
    }
    .with_context(|| format!("checksumming {}", path.display()))?;
    Ok(ArtifactEntry::checksum(bytes, sha256))
}

/// Carry build-produced artifact metadata only while its artifact is unchanged.
///
/// `kgf manifest` can recompute checksums, but it cannot derive the semantic
/// view blocks in the two stats TSVs without doing the producer's indexed-VoID
/// traversal. Retaining those ranges across identical bytes is exact. Inventing
/// them for a new file, or carrying them across changed bytes, would publish
/// offsets the server has no reason to trust, so both cases name `kgf build`.
fn carry_artifact_metadata(
    artifacts: &mut [(String, ArtifactEntry)],
    previous: Option<&Manifest>,
) -> Result<()> {
    let current_digests: BTreeMap<String, (u64, String)> = artifacts
        .iter()
        .map(|(name, entry)| (name.clone(), (entry.bytes, entry.sha256.clone())))
        .collect();

    for (name, current) in artifacts {
        let prior = previous.and_then(|manifest| manifest.artifacts.get(name));
        let same_content = prior
            .is_some_and(|prior| prior.bytes == current.bytes && prior.sha256 == current.sha256);

        if matches!(
            name.as_str(),
            artifact::SCHEMA_NODES | artifact::CLASS_RELATIONS
        ) {
            let prior = prior.with_context(|| {
                format!(
                    "artifact {name} needs build-produced parents, max_row_bytes and view ranges; \
                     produce the description set with `kgf build`"
                )
            })?;
            if !same_content {
                bail!(
                    "artifact {name} changed, so its recorded view ranges may be stale; \
                     rebuild the description set with `kgf build`"
                );
            }
            if prior.parents.len() != 1
                || prior.parents[0] != artifact::VOID_HDT
                || prior.max_row_bytes.is_none()
                || !prior.views.contains_key("design")
                || !prior.views.contains_key("queryable")
            {
                bail!(
                    "artifact {name} has incomplete view metadata; rebuild the description set \
                     with `kgf build`"
                );
            }
        }

        if same_content {
            let prior = prior.expect("same_content is true only for a previous entry");
            for parent in &prior.parents {
                let prior_parent = previous
                    .and_then(|manifest| manifest.artifacts.get(parent))
                    .with_context(|| {
                        format!(
                            "artifact {name} declares parent {parent}, but the previous manifest \
                             has no entry for it; rebuild the derived artifact"
                        )
                    })?;
                let current_parent = current_digests.get(parent).with_context(|| {
                    format!(
                        "artifact {name} declares parent {parent}, but the current bundle does \
                         not contain it; rebuild the derived artifact"
                    )
                })?;
                if current_parent.0 != prior_parent.bytes || current_parent.1 != prior_parent.sha256
                {
                    bail!(
                        "artifact {name} is unchanged but its parent {parent} changed, so the \
                         derived index may be stale; rebuild the description set with `kgf build`"
                    );
                }
            }
            current.parents.clone_from(&prior.parents);
            current.max_row_bytes = prior.max_row_bytes;
            current.views.clone_from(&prior.views);
        }
    }
    Ok(())
}

/// Size and digest a directory artifact as one entry (doc 04 §4.3).
///
/// `data.hdt.text` is the only one, and it is a directory because its bytes are
/// Tantivy's. The digest is the same preimage `content_digest` is built from —
/// `{relative path}  {sha256}\n` per file, sorted by path — applied one level
/// down, so a directory and the bundle around it are identified by one
/// construction rather than two.
///
/// One entry rather than one per segment file: those names are chosen per
/// build, so enumerating them would put a key set that changes on every rebuild
/// into the manifest without adding a fact a mirror can check.
fn sha256_dir(dir: &Path) -> Result<(u64, String)> {
    let mut files = Vec::new();
    let mut bytes = 0u64;
    for entry in walk(dir)? {
        let relative = entry
            .strip_prefix(dir)
            .expect("a walked path is under the directory it was walked from");
        let (size, sha256) = sha256_file(&entry)?;
        bytes += size;
        files.push(ArtifactDigest {
            // Slash-separated whatever the platform is, so a bundle built on
            // one and verified on another agrees with itself.
            name: relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
            sha256,
        });
    }
    Ok((bytes, hex(&Sha256::digest(content_digest_preimage(&files)))))
}

/// Every file under `dir`, depth first.
///
/// A text index is flat today; walking anyway costs nothing and means a
/// Tantivy release that nests something does not silently drop it from the
/// digest — which would be a change to a version's identity that nothing
/// reports.
fn walk(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in
            std::fs::read_dir(&current).with_context(|| format!("reading {}", current.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Stream a file through SHA-256, returning its length and lowercase hex digest.
///
/// Streamed in fixed blocks because a bundle artifact runs to gigabytes — the
/// Ubergraph sidecar is 4.1 GB — and this is a build step on the same small
/// machines the toolchain targets.
fn sha256_file(path: &Path) -> Result<(u64, String)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut bytes = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes += read as u64;
    }
    Ok((bytes, hex(&hasher.finalize())))
}

/// Compute the bundle content digest over its artifact entries (doc 04 §4.3).
pub fn content_digest<'a>(
    artifacts: impl IntoIterator<Item = (&'a str, &'a ArtifactEntry)>,
) -> String {
    let digests: Vec<ArtifactDigest> = artifacts
        .into_iter()
        .map(|(name, entry)| ArtifactDigest {
            name: name.to_owned(),
            sha256: entry.sha256.clone(),
        })
        .collect();
    let root = Sha256::digest(content_digest_preimage(&digests));
    format!("sha256:{}", hex(&root))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The current time as an RFC 3339 UTC timestamp.
///
/// Hand-rolled rather than pulling in a date library for one field. The
/// days-to-civil conversion is Howard Hinnant's `civil_from_days`, which is
/// exact for the whole proleptic Gregorian range.
fn now_rfc3339() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    rfc3339(seconds)
}

fn rfc3339(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let time = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60,
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1), "1970-01-01T00:00:01Z");
        // A leap day, which the month shift is the part that gets wrong.
        assert_eq!(rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_709_251_199), "2024-02-29T23:59:59Z");
        assert_eq!(rfc3339(1_754_006_400), "2025-08-01T00:00:00Z");
        // 2000 is a leap year and 1900 is not; the era arithmetic is what
        // separates them, and 1900 is also the pre-epoch sign case.
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(-2_203_891_200), "1900-03-01T00:00:00Z");
    }

    #[test]
    fn hex_pads_every_byte() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }

    /// `kgf manifest` invoked with only `--prefix` bindings.
    fn args(bindings: &[&str]) -> Args {
        Args {
            bundle: PathBuf::new(),
            check: false,
            id: None,
            version: None,
            dataset_iri: None,
            title: None,
            description: None,
            license: None,
            homepage: None,
            publisher: None,
            publisher_contact: None,
            previous_version: None,
            prefixes: bindings.iter().map(|s| (*s).to_owned()).collect(),
            roles: Vec::new(),
        }
    }

    /// A manifest carrying nothing but a prefix map.
    fn manifest_with(prefixes: BTreeMap<String, String>) -> Manifest {
        Manifest {
            id: "d".to_owned(),
            dataset_iri: None,
            version: "v".to_owned(),
            content_digest: "sha256:0".to_owned(),
            created: None,
            formats: Formats::default(),
            title: None,
            description: None,
            license: None,
            homepage: None,
            publisher: None,
            counts: kgf_store::manifest::Counts {
                triples: 0,
                subjects: 0,
                predicates: 0,
                objects: 0,
            },
            capabilities: BTreeMap::new(),
            prefixes,
            predicate_roles: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            previous_version: None,
        }
    }

    #[test]
    fn prefix_bindings_parse_or_say_why_not() {
        let parsed = prefixes(&args(&["ex=http://example.org/"]), None).unwrap();
        assert_eq!(parsed["ex"], "http://example.org/");

        // An expansion containing '=' splits at the first one only.
        let parsed = prefixes(&args(&["q=http://example.org/?a=b"]), None).unwrap();
        assert_eq!(parsed["q"], "http://example.org/?a=b");

        assert!(prefixes(&args(&["nope"]), None).is_err());
        assert!(prefixes(&args(&["=http://example.org/"]), None).is_err());
        assert!(prefixes(&args(&["ex="]), None).is_err());
        assert!(prefixes(&args(&["ex=a", "ex=b"]), None).is_err());
    }

    #[test]
    fn role_declarations_keep_predicate_order_and_reject_duplicates() {
        let mut supplied = args(&[]);
        supplied.roles = vec![
            "label=http://example.org/preferred".to_owned(),
            "label=http://example.org/fallback".to_owned(),
            "synonym=http://example.org/alias".to_owned(),
        ];
        let roles = predicate_roles(&supplied, None, &BTreeMap::new()).unwrap();
        assert_eq!(
            roles["label"],
            [
                "http://example.org/preferred",
                "http://example.org/fallback"
            ]
        );
        assert_eq!(roles["synonym"], ["http://example.org/alias"]);

        supplied
            .roles
            .push("label=http://example.org/preferred".to_owned());
        assert!(predicate_roles(&supplied, None, &BTreeMap::new()).is_err());

        let defaults = predicate_roles(&args(&[]), None, &BTreeMap::new()).unwrap();
        assert!(defaults.contains_key("label"));

        let mut curie = args(&[]);
        curie.roles = vec!["label=ex:name".to_owned()];
        let prefixes = BTreeMap::from([("ex".to_owned(), "http://example.org/".to_owned())]);
        let error = predicate_roles(&curie, None, &prefixes).unwrap_err();
        assert!(error.to_string().contains("http://example.org/name"));
    }

    #[test]
    fn no_prefix_flags_keeps_the_current_map() {
        let previous = manifest_with(BTreeMap::from([(
            "ex".to_owned(),
            "http://example.org/".to_owned(),
        )]));

        let carried = prefixes(&args(&[]), Some(&previous)).unwrap();
        for (prefix, namespace) in &previous.prefixes {
            assert_eq!(carried.get(prefix), Some(namespace), "carried {prefix}");
        }
        for (prefix, namespace) in WELL_KNOWN_PREFIXES {
            assert_eq!(carried.get(prefix).map(String::as_str), Some(namespace));
        }
    }

    #[test]
    fn the_well_known_prefixes_are_declared_and_overridable() {
        // Without these a freshly described bundle accepts no CURIE at all,
        // since doc 03 §3.3 resolves one only against a declared prefix.
        let fresh = prefixes(&args(&[]), None).unwrap();
        assert_eq!(
            fresh.get("xsd").map(String::as_str),
            Some("http://www.w3.org/2001/XMLSchema#")
        );

        // Seeded, not imposed: a dataset that means something else by `xsd`
        // says so and is believed.
        let overridden = prefixes(&args(&["xsd=http://example.org/xsd#"]), None).unwrap();
        assert_eq!(
            overridden.get("xsd").map(String::as_str),
            Some("http://example.org/xsd#")
        );
        assert!(overridden.contains_key("rdfs"), "the rest still stand");

        // Idempotent, so regenerating a manifest does not keep changing it.
        let carried = manifest_with(fresh.clone());
        assert_eq!(prefixes(&args(&[]), Some(&carried)).unwrap(), fresh);
    }

    #[test]
    fn manifest_carries_view_metadata_only_for_unchanged_tsv_bytes() {
        let mut previous = manifest_with(BTreeMap::new());
        previous.artifacts.insert(
            artifact::VOID_HDT.to_owned(),
            ArtifactEntry::checksum(90, "void-abc"),
        );
        let mut entry = ArtifactEntry::checksum(100, "abc");
        entry.parents = vec![artifact::VOID_HDT.to_owned()];
        entry.max_row_bytes = Some(80);
        entry.views = BTreeMap::from([
            (
                "design".to_owned(),
                ArtifactView {
                    offset: 50,
                    bytes: 20,
                    rows: 1,
                },
            ),
            (
                "queryable".to_owned(),
                ArtifactView {
                    offset: 70,
                    bytes: 20,
                    rows: 1,
                },
            ),
        ]);
        previous
            .artifacts
            .insert(artifact::SCHEMA_NODES.to_owned(), entry.clone());

        let mut unchanged = vec![
            (
                artifact::VOID_HDT.to_owned(),
                ArtifactEntry::checksum(90, "void-abc"),
            ),
            (
                artifact::SCHEMA_NODES.to_owned(),
                ArtifactEntry::checksum(100, "abc"),
            ),
        ];
        carry_artifact_metadata(&mut unchanged, Some(&previous)).unwrap();
        assert_eq!(unchanged[1].1, entry);

        let mut changed = vec![
            (
                artifact::VOID_HDT.to_owned(),
                ArtifactEntry::checksum(90, "void-abc"),
            ),
            (
                artifact::SCHEMA_NODES.to_owned(),
                ArtifactEntry::checksum(101, "def"),
            ),
        ];
        let error = carry_artifact_metadata(&mut changed, Some(&previous)).unwrap_err();
        assert!(error.to_string().contains("view ranges may be stale"));

        let error = carry_artifact_metadata(&mut unchanged, None).unwrap_err();
        assert!(error.to_string().contains("kgf build"));

        let mut stale_parent = vec![
            (
                artifact::VOID_HDT.to_owned(),
                ArtifactEntry::checksum(91, "void-def"),
            ),
            (
                artifact::SCHEMA_NODES.to_owned(),
                ArtifactEntry::checksum(100, "abc"),
            ),
        ];
        let error = carry_artifact_metadata(&mut stale_parent, Some(&previous)).unwrap_err();
        assert!(error.to_string().contains("parent stats/void.hdt changed"));
    }
}
