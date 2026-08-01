//! End-to-end `kgf manifest`, over a bundle assembled the way a person would.
//!
//! Doc 20 §20.9's golden-bundle rule applies here too: hdtc builds the
//! artifacts, so what is described is a producer's output rather than this
//! crate's guess. The point of the test is the seam — that a directory holding
//! nothing but `hdtc create --perm`'s output becomes a bundle
//! [`Store::open`](kgf_store::Store::open) accepts, and that the manifest stops
//! agreeing the moment the artifacts move underneath it.

use std::path::{Path, PathBuf};
use std::process::Command;

use kgf_store::manifest::Manifest;
use kgf_store::{OpenOptions, Store};

const SOURCE: &str = concat!(
    "<http://example.org/alice> <http://example.org/name> \"Alice\" .\n",
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> .\n",
    "<http://example.org/bob> <http://example.org/name> \"Bob\" .\n",
    "<http://example.org/bob> <http://example.org/knows> <http://example.org/alice> .\n",
);

/// One more triple, and one more subject and object, than [`SOURCE`].
const GROWN_SOURCE: &str = concat!(
    "<http://example.org/alice> <http://example.org/name> \"Alice\" .\n",
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> .\n",
    "<http://example.org/bob> <http://example.org/name> \"Bob\" .\n",
    "<http://example.org/bob> <http://example.org/knows> <http://example.org/alice> .\n",
    "<http://example.org/carol> <http://example.org/name> \"Carol\" .\n",
);

#[test]
fn a_hand_assembled_bundle_becomes_servable_and_stays_honest() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join("demo-kg").join("2026-08-01");
    std::fs::create_dir_all(&bundle).unwrap();
    build_artifacts(&bundle, SOURCE);

    // Before: artifacts only. `Store::open` refuses a directory that is not a
    // bundle, naming the command that completes it.
    let error = open(&bundle).expect_err("a bundle without a manifest is not servable");
    assert!(error.to_string().contains("kgf manifest"), "{error}");

    kgf(&[
        "manifest",
        path(&bundle),
        "--prefix",
        "ex=http://example.org/",
    ])
    .success();

    // After: servable, and the manifest describes what the store reads.
    let store = open(&bundle).expect("bundle opens once it has a manifest");
    let manifest = Manifest::read(&bundle).unwrap();
    assert_eq!(manifest.counts.triples, store.triples());
    assert_eq!(manifest.counts.triples, 4);

    // Identity is inferred from the catalog layout, so hand-assembly needs no
    // flags for it.
    assert_eq!(manifest.id, "demo-kg");
    assert_eq!(manifest.version, "2026-08-01");
    assert!(manifest.content_digest.starts_with("sha256:"));
    assert_eq!(manifest.prefixes["ex"], "http://example.org/");

    // Both required artifacts are checksummed; the manifest never lists itself.
    assert_eq!(manifest.artifacts.len(), 2);
    assert!(manifest.artifacts.contains_key("data.hdt"));
    assert!(manifest.artifacts.contains_key("data.hdt.perm"));
    assert!(!manifest.artifacts.contains_key("manifest.json"));

    // A core bundle declares only what its artifacts back.
    let mut capabilities: Vec<&str> = manifest.capabilities.keys().map(String::as_str).collect();
    capabilities.sort_unstable();
    assert_eq!(capabilities, ["export", "sample", "star", "terms"]);

    kgf(&["manifest", path(&bundle), "--check"]).success();

    // Regeneration carries the identity forward and reproduces the same bytes,
    // so a manifest can be diffed across versions.
    let before = std::fs::read(bundle.join("manifest.json")).unwrap();
    kgf(&["manifest", path(&bundle)]).success();
    let after = std::fs::read(bundle.join("manifest.json")).unwrap();
    assert_eq!(before, after, "regeneration is not byte-stable");

    // The failure this all exists for: artifacts rebuilt, manifest not.
    std::fs::remove_file(bundle.join("data.hdt")).unwrap();
    std::fs::remove_file(bundle.join("data.hdt.perm")).unwrap();
    build_artifacts(&bundle, GROWN_SOURCE);

    let stale = kgf(&["manifest", path(&bundle), "--check"]).failure();
    assert!(stale.contains("counts.triples"), "{stale}");
    assert!(stale.contains("kgf manifest"), "{stale}");

    kgf(&["manifest", path(&bundle)]).success();
    kgf(&["manifest", path(&bundle), "--check"]).success();

    let regenerated = Manifest::read(&bundle).unwrap();
    assert_eq!(regenerated.counts.triples, 5);
    // Descriptive fields survive a regeneration; the digest tracks the bytes.
    assert_eq!(regenerated.prefixes["ex"], "http://example.org/");
    assert_ne!(regenerated.content_digest, manifest.content_digest);
}

/// Open the bundle through the read layer.
///
/// # Safety
///
/// The artifacts are written by this test into a temporary directory and are
/// not modified while the returned store lives — each rebuild below happens
/// after the previous store has been dropped.
#[allow(unsafe_code)]
fn open(bundle: &Path) -> kgf_store::Result<Store> {
    let published = unsafe { kgf_store::PublishedBundle::new(bundle) };
    Store::open(&published, OpenOptions::default())
}

fn path(path: &Path) -> &str {
    path.to_str().expect("temp paths are UTF-8")
}

/// Run `hdtc create --perm` into `bundle`, as a person assembling one would.
fn build_artifacts(bundle: &Path, source: &str) {
    let input = bundle.join("input.nt");
    std::fs::write(&input, source).unwrap();

    let output = Command::new(hdtc_binary())
        .args([
            "create",
            path(&input),
            "-o",
            path(&bundle.join("data.hdt")),
            "--temp-dir",
            path(&bundle.join("work")),
            "--memory-limit",
            "64M",
            "--perm",
        ])
        .output()
        .expect("run hdtc");
    assert!(
        output.status.success(),
        "hdtc create failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Leave only what a bundle carries, so the artifact list under test is the
    // real one.
    std::fs::remove_file(&input).unwrap();
    let _ = std::fs::remove_dir_all(bundle.join("work"));
}

/// The outcome of one `kgf` invocation.
struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

impl Run {
    fn success(self) -> String {
        assert!(
            self.status.success(),
            "kgf failed unexpectedly:\n{}",
            self.stderr
        );
        self.stdout
    }

    fn failure(self) -> String {
        assert!(
            !self.status.success(),
            "kgf succeeded unexpectedly:\n{}",
            self.stdout
        );
        self.stderr
    }
}

fn kgf(args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_kgf"))
        .args(args)
        .output()
        .expect("run kgf");
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        status: output.status,
    }
}

/// Locate the `hdtc` binary the same way `kgf-store`'s fixtures do: `$KGF_HDTC`
/// if set, else the sibling checkout's build.
fn hdtc_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("KGF_HDTC") {
        return PathBuf::from(path);
    }
    let hdtc = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../hdtc")
        .canonicalize()
        .expect("../hdtc sibling checkout");
    for profile in ["release", "debug"] {
        let candidate = hdtc.join("target").join(profile).join("hdtc");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "no hdtc binary under {}; build it with \
         `cargo build --release --manifest-path {}/Cargo.toml`, \
         or set KGF_HDTC",
        hdtc.join("target").display(),
        hdtc.display()
    );
}
