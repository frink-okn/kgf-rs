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

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use kgf_store::manifest::{
    ArtifactDigest, ArtifactEntry, BundleFacts, Formats, Manifest, Publisher,
    content_digest_preimage,
};
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
}

/// Run `kgf manifest`.
pub fn run(args: Args) -> Result<()> {
    let dir = args
        .bundle
        .canonicalize()
        .with_context(|| format!("resolving bundle directory {}", args.bundle.display()))?;

    let facts = read_facts(&dir)?;

    if args.check {
        let manifest = Manifest::read(&dir)?;
        manifest.verify_against(&facts, &dir)?;
        println!(
            "{}: manifest agrees with its artifacts ({} triples)",
            dir.display(),
            facts.triples()
        );
        return Ok(());
    }

    // Any manifest already here supplies defaults, so regenerating after a
    // rebuild needs no flags. A placeholder that does not parse is not an error:
    // supplying one is how a bundle gets its first real manifest.
    let previous = Manifest::read(&dir).ok();
    let manifest = build(&args, &dir, &facts, previous.as_ref())?;

    let path = dir.join("manifest.json");
    std::fs::write(&path, manifest.to_json_bytes()?)
        .with_context(|| format!("writing {}", path.display()))?;

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

/// Read the artifacts' structural facts.
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
fn read_facts(dir: &Path) -> Result<BundleFacts> {
    let bundle = unsafe { kgf_store::PublishedBundle::new(dir) };
    Ok(BundleFacts::read(&bundle)?)
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

    let artifacts = checksum_artifacts(dir, facts)?;
    let content_digest = content_digest(&artifacts);

    // `created` dates the bundle, not the manifest file, so regenerating over
    // unchanged artifacts must not restate it. That also keeps regeneration
    // byte-stable, which is what lets a manifest be diffed across versions and
    // keeps the digest it carries meaningful as an identity.
    let created = match previous {
        Some(previous) if previous.content_digest == content_digest => previous.created.clone(),
        _ => None,
    }
    .unwrap_or_else(now_rfc3339);

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
        prefixes: prefixes(args, previous)?,
        artifacts: artifacts.into_iter().collect(),
        previous_version: args
            .previous_version
            .clone()
            .or_else(|| previous.and_then(|m| m.previous_version.clone())),
    })
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

fn prefixes(args: &Args, previous: Option<&Manifest>) -> Result<BTreeMap<String, String>> {
    if args.prefixes.is_empty() {
        return Ok(previous.map(|m| m.prefixes.clone()).unwrap_or_default());
    }

    let mut prefixes = BTreeMap::new();
    for binding in &args.prefixes {
        let (prefix, expansion) = binding
            .split_once('=')
            .with_context(|| format!("--prefix {binding} is not of the form prefix=IRI"))?;
        if prefix.is_empty() || expansion.is_empty() {
            bail!("--prefix {binding} has an empty prefix or expansion");
        }
        if let Some(existing) = prefixes.insert(prefix.to_owned(), expansion.to_owned()) {
            bail!("--prefix {prefix} given twice ({existing} and {expansion})");
        }
    }
    Ok(prefixes)
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
            let (bytes, sha256) =
                sha256_file(&path).with_context(|| format!("checksumming {}", path.display()))?;
            Ok((name.to_owned(), ArtifactEntry { bytes, sha256 }))
        })
        .collect()
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

/// The Merkle root over the artifact checksums (doc 04 §4.3).
fn content_digest(artifacts: &[(String, ArtifactEntry)]) -> String {
    let digests: Vec<ArtifactDigest> = artifacts
        .iter()
        .map(|(name, entry)| ArtifactDigest {
            name: name.clone(),
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

    #[test]
    fn prefix_bindings_parse_or_say_why_not() {
        let args = |bindings: &[&str]| Args {
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
        };

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
    fn no_prefix_flags_keeps_the_current_map() {
        let mut previous = Manifest {
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
            prefixes: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            previous_version: None,
        };
        previous
            .prefixes
            .insert("ex".to_owned(), "http://example.org/".to_owned());

        let args = Args {
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
            prefixes: Vec::new(),
        };
        assert_eq!(prefixes(&args, Some(&previous)).unwrap(), previous.prefixes);
    }
}
