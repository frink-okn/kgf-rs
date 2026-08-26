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

/// The execution engine is the next unit. Until it lands the command must
/// refuse rather than publish something partial, and say what to do instead.
#[test]
fn a_resolvable_build_reports_that_execution_is_not_implemented() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("dreamkg/2026-06-01");
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
    assert!(stderr.contains("dreamkg/2026-06-01"), "{stderr}");
    assert!(stderr.contains("--check-config"), "{stderr}");
    assert!(!out.exists(), "a refused build must leave nothing behind");
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
