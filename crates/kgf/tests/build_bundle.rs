//! `kgf build bundle`'s command surface: what a caller can and cannot ask for.
//!
//! The resolution rules themselves are unit-tested beside them in
//! `build::bundle::plan`. What is tested here is the seam kace meets — a config
//! on stdin, a resolved plan on stdout, and refusals that name the fix — because
//! that surface is the contract `notes/build-bundle.md` §6 asks a build workflow
//! to depend on.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const MINIMAL: &str =
    "schema: 1\ndataset: {id: dreamkg, iri: 'https://purl.org/okn/frink/kg/dreamkg'}\n";

const CONFIG: &str = concat!(
    "schema: 1\n",
    "dataset:\n",
    "  id: tinykg\n",
    "  iri: https://purl.org/okn/frink/kg/tinykg\n",
    "  title: Tiny KG\n",
    "semantics:\n",
    "  prefixes: {ex: 'http://example.org/'}\n",
    "  roles: {label: ['http://www.w3.org/2000/01/rdf-schema#label']}\n",
);

const SOURCE: &str = concat!(
    "<http://example.org/alice> <http://www.w3.org/2000/01/rdf-schema#label> \"Alice\" .\n",
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> .\n",
    "<http://example.org/bob> <http://www.w3.org/2000/01/rdf-schema#label> \"Bob\" .\n",
);

/// The whole point of `--check-config`: a registry can validate an entry
/// without knowing where a bundle would ever be written.
#[test]
fn check_config_needs_neither_an_output_nor_an_input() {
    let stdout = kgf(
        &["build", "bundle", "--config", "-", "--check-config"],
        MINIMAL,
    )
    .ok();
    let plan: serde_json::Value = serde_json::from_str(&stdout).expect("a resolved plan is JSON");
    assert_eq!(plan["dataset"]["id"], "dreamkg");
    assert_eq!(plan["contents"]["filters"]["k"], 65536);
    assert_eq!(plan["contents"]["keysets"]["encoding"], "elias-fano");
}

#[test]
fn a_build_without_an_output_is_refused_by_the_parser() {
    let stderr = kgf(&["build", "bundle", "--config", "-"], MINIMAL).err();
    assert!(stderr.contains("--out"), "{stderr}");
}

#[test]
fn a_build_without_an_input_says_which_flags_supply_one() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("dreamkg/2026-06-01");
    let stderr = kgf(
        &["build", "bundle", "--config", "-", "--out", path(&out)],
        MINIMAL,
    )
    .err();
    assert!(
        stderr.contains("--hdt") && stderr.contains("--input"),
        "{stderr}"
    );
}

/// Doc 04 §4.6: a published version is immutable, so a rebuild is a new
/// directory rather than a rewrite of a live one a server may have mapped.
#[test]
fn an_existing_output_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("dreamkg/2026-06-01");
    std::fs::create_dir_all(&out).unwrap();
    let hdt = dir.path().join("graph.hdt");
    std::fs::write(&hdt, b"not really an hdt, and never read").unwrap();

    let stderr = kgf(
        &[
            "build",
            "bundle",
            "--config",
            "-",
            "--out",
            path(&out),
            "--hdt",
            path(&hdt),
        ],
        MINIMAL,
    )
    .err();
    assert!(stderr.contains("already exists"), "{stderr}");
    assert!(stderr.contains("immutable"), "{stderr}");
}

#[test]
fn an_output_path_disagreeing_with_the_config_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("dream-kg/2026-06-01");
    let hdt = dir.path().join("graph.hdt");
    std::fs::write(&hdt, b"never read").unwrap();

    let stderr = kgf(
        &[
            "build",
            "bundle",
            "--config",
            "-",
            "--out",
            path(&out),
            "--hdt",
            path(&hdt),
        ],
        MINIMAL,
    )
    .err();
    assert!(stderr.contains("dataset.id"), "{stderr}");
}

/// The whole command, end to end: RDF in, a bundle `kgf manifest --check`
/// accepts out. Doc 20 §20.9's golden-bundle rule — hdtc builds the artifacts,
/// so what is checked is a producer's output rather than this crate's guess.
#[test]
fn a_build_produces_a_bundle_its_own_check_accepts() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("tiny.nt");
    std::fs::write(&source, SOURCE).unwrap();
    let out = dir.path().join("root/tinykg/2026-06-01");

    let stdout = kgf(
        &[
            "build",
            "bundle",
            "--config",
            "-",
            "--out",
            path(&out),
            "--input",
            path(&source),
            "--hdtc",
            &hdtc(),
            "--source-url",
            "lakefs://tinykg/abc123/tiny.nt",
        ],
        CONFIG,
    )
    .ok();
    assert!(
        stdout.contains("data.hdt"),
        "the report lists artifacts:\n{stdout}"
    );
    assert!(
        stdout.contains("total"),
        "the report totals them:\n{stdout}"
    );

    // Every family a conforming bundle publishes, including the two nothing
    // reads yet (doc 17 §17.3, doc 18 §18.1).
    for entry in [
        "manifest.json",
        "data.hdt",
        "data.hdt.perm",
        "data.hdt.text",
        "filters",
        "keysets",
        "stats/void.hdt",
        "stats/summary.md",
    ] {
        assert!(out.join(entry).exists(), "missing {entry}");
    }

    // The staging and scratch directories are gone.
    let siblings: Vec<_> = std::fs::read_dir(out.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(siblings.len(), 1, "staging was left behind: {siblings:?}");

    kgf(&["manifest", path(&out), "--check"], "").ok();

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["id"], "tinykg");
    assert_eq!(manifest["version"], "2026-06-01");
    assert_eq!(manifest["title"], "Tiny KG");
    // The config's semantics reached the manifest, which is what makes them
    // this version's frozen profile rather than a serve-time overlay.
    assert_eq!(manifest["prefixes"]["ex"], "http://example.org/");
    assert_eq!(
        manifest["predicate_roles"]["label"][0],
        "http://www.w3.org/2000/01/rdf-schema#label"
    );
    // Provenance: the URL is passed through, the digest is computed here.
    let input = &manifest["source"]["inputs"][0];
    assert_eq!(input["url"], "lakefs://tinykg/abc123/tiny.nt");
    assert_eq!(input["sha256"], sha256(&source));
}

/// A digest the caller asserts is checked against the bytes the build read, so
/// `--source-sha256` is an integrity check on the download rather than a value
/// copied into the manifest.
#[test]
fn a_mismatched_source_digest_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("tiny.nt");
    std::fs::write(&source, SOURCE).unwrap();
    let out = dir.path().join("root/tinykg/2026-06-01");

    let stderr = kgf(
        &[
            "build",
            "bundle",
            "--config",
            "-",
            "--out",
            path(&out),
            "--input",
            path(&source),
            "--hdtc",
            &hdtc(),
            "--source-sha256",
            &"0".repeat(64),
        ],
        CONFIG,
    )
    .err();
    assert!(stderr.contains("nothing was published"), "{stderr}");
    assert!(!out.exists(), "a refused build must publish nothing");
    let siblings: Vec<_> = std::fs::read_dir(out.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(siblings.is_empty(), "staging was left behind: {siblings:?}");
}

/// `--dry-run` names every command in order and runs none of them. This is what
/// a build job's logs are read for, so the commands must be the ones that would
/// actually run rather than a second construction of them.
#[test]
fn a_dry_run_names_every_command_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("tiny.nt");
    std::fs::write(&source, SOURCE).unwrap();
    let out = dir.path().join("root/tinykg/2026-06-01");

    let stdout = kgf(
        &[
            "build",
            "bundle",
            "--config",
            "-",
            "--out",
            path(&out),
            "--input",
            path(&source),
            "--hdtc",
            "/usr/bin/hdtc",
            "--dry-run",
        ],
        CONFIG,
    )
    .ok();

    for fragment in [
        "hdtc create",
        "--perm",
        "hdtc text",
        "hdtc sketch",
        "hdtc keyset",
        // The profile role lists are stated, never left to an hdtc default.
        "--roles subjects,objects",
        "--roles subjects-only,objects-only,shared",
        "manifest, written last",
    ] {
        assert!(
            stdout.contains(fragment),
            "missing {fragment:?} in:\n{stdout}"
        );
    }
    assert!(
        stdout.find("hdtc create").unwrap() < stdout.find("hdtc text").unwrap(),
        "the core step must precede the sidecars:\n{stdout}"
    );
    assert!(!out.exists(), "--dry-run must create nothing");
}

#[test]
fn an_asserted_source_digest_must_look_like_one() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("dreamkg/2026-06-01");
    let hdt = dir.path().join("graph.hdt");
    std::fs::write(&hdt, b"never read").unwrap();

    for bad in ["cafe", &"A".repeat(64), &"z".repeat(64)] {
        let stderr = kgf(
            &[
                "build",
                "bundle",
                "--config",
                "-",
                "--out",
                path(&out),
                "--hdt",
                path(&hdt),
                "--source-sha256",
                bad,
            ],
            MINIMAL,
        )
        .err();
        assert!(stderr.contains("--source-sha256"), "{bad}: {stderr}");
    }
}

#[test]
fn an_unreadable_config_names_the_file() {
    let stderr = kgf(
        &[
            "build",
            "bundle",
            "--config",
            "/nonexistent/build.yaml",
            "--check-config",
        ],
        "",
    )
    .err();
    assert!(stderr.contains("/nonexistent/build.yaml"), "{stderr}");
}

struct Run {
    stdout: String,
    stderr: String,
    status: std::process::ExitStatus,
}

impl Run {
    fn ok(self) -> String {
        assert!(
            self.status.success(),
            "kgf failed unexpectedly:\n{}",
            self.stderr
        );
        self.stdout
    }

    fn err(self) -> String {
        assert!(
            !self.status.success(),
            "kgf succeeded unexpectedly:\n{}",
            self.stdout
        );
        self.stderr
    }
}

fn path(path: &Path) -> &str {
    path.to_str().expect("test paths are UTF-8")
}

fn hdtc() -> String {
    kgf_store::testing::hdtc_binary()
        .to_str()
        .expect("the hdtc path is UTF-8")
        .to_owned()
}

fn sha256(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(std::fs::read(path).expect("read the hashed file"));
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn kgf(args: &[&str], stdin: &str) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kgf"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run kgf");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin.as_bytes())
        .expect("write config to kgf");
    let output = child.wait_with_output().expect("collect kgf output");
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        status: output.status,
    }
}
