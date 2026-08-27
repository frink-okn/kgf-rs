//! `kgf build`'s command surface: what a caller can and cannot ask for.
//!
//! The resolution rules themselves are unit-tested beside them in
//! `build::plan`. What is tested here is the seam kace meets — a config
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
    let stdout = kgf(&["build", "--config", "-", "--check-config"], MINIMAL).ok();
    let plan: serde_json::Value = serde_json::from_str(&stdout).expect("a resolved plan is JSON");
    assert_eq!(plan["dataset"]["id"], "dreamkg");
    assert_eq!(plan["contents"]["filters"]["k"], 65536);
    assert_eq!(plan["contents"]["keysets"]["encoding"], "elias-fano");
}

#[test]
fn a_build_without_an_output_is_refused_by_the_parser() {
    let stderr = kgf(&["build", "--config", "-"], MINIMAL).err();
    assert!(stderr.contains("--out"), "{stderr}");
}

#[test]
fn a_build_without_an_input_says_which_flags_supply_one() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("dreamkg/2026-06-01");
    let stderr = kgf(&["build", "--config", "-", "--out", path(&out)], MINIMAL).err();
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

/// Doc 17 §17.3 and doc 18 §18.4 require a manifest entry per `filters/` and
/// `keysets/` file. Without them those bytes sit in the bundle uncovered by
/// `content_digest`, unverifiable by any mirror — and describing them later
/// changes that digest, which for an immutable version means a rebuild.
#[test]
fn key_artifacts_are_described_per_file_and_cross_checked() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("tiny.nt");
    std::fs::write(&source, SOURCE).unwrap();
    let out = dir.path().join("root/tinykg/2026-06-01");

    kgf(
        &[
            "build",
            "--config",
            "-",
            "--out",
            path(&out),
            "--input",
            path(&source),
            "--hdtc",
            &hdtc(),
        ],
        CONFIG,
    )
    .ok();

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
    let capabilities = manifest["capabilities"].as_object().unwrap();
    assert!(capabilities.contains_key("filters"), "{capabilities:?}");
    assert!(capabilities.contains_key("keysets"), "{capabilities:?}");

    let artifacts = manifest["artifacts"].as_object().unwrap();
    for name in [
        "filters/subjects.filter",
        "filters/subjects.minhash",
        "filters/objects.filter",
        "filters/objects.minhash",
        "keysets/shared.keys",
        "keysets/subjects-only.keys",
        "keysets/objects-only.keys",
    ] {
        let entry = artifacts
            .get(name)
            .unwrap_or_else(|| panic!("{name} is not described"));
        // The comparability pair, which doc 18 §18.4 says a registry must
        // verify on ingest, plus the count the identity below rests on.
        let keys = &entry["keys"];
        assert_eq!(keys["convention_id"], 1, "{name}");
        assert_eq!(keys["hash_id"], 1, "{name}");
        assert!(keys["key_count"].is_u64(), "{name}");
        assert!(entry["sha256"].is_string(), "{name}");
    }
    assert_eq!(
        artifacts["keysets/shared.keys"]["keys"]["encoding"],
        "elias-fano"
    );
    // Sketches carry no encoding; only key sets do.
    assert!(artifacts["filters/subjects.filter"]["keys"]["encoding"].is_null());

    let count = |name: &str| artifacts[name]["keys"]["key_count"].as_u64().unwrap();
    for (whole, only) in [("subjects", "subjects-only"), ("objects", "objects-only")] {
        assert_eq!(
            count("keysets/shared.keys") + count(&format!("keysets/{only}.keys")),
            count(&format!("filters/{whole}.filter")),
            "doc 18 §18.4 identity must hold for {whole}"
        );
    }
}

/// The case the doc 18 §18.4 count identity cannot catch: a key set from
/// another bundle whose totals happen to agree. Both fixtures here have two
/// shared keys, so the identity holds and only the `source_digest` binding
/// separates them.
#[test]
fn a_key_set_from_another_bundle_is_refused_even_when_the_counts_agree() {
    let dir = tempfile::tempdir().unwrap();
    let mine = dir.path().join("mine.nt");
    let theirs = dir.path().join("theirs.nt");
    std::fs::write(&mine, SOURCE).unwrap();
    // A structural mirror of SOURCE — same triple shape, same role
    // cardinalities, different IRIs — so every count agrees and nothing but the
    // binding can tell the two bundles' key sets apart.
    std::fs::write(
        &theirs,
        concat!(
            "<http://example.org/carol> <http://www.w3.org/2000/01/rdf-schema#label> \"Carol\" .\n",
            "<http://example.org/carol> <http://example.org/knows> <http://example.org/dave> .\n",
            "<http://example.org/dave> <http://www.w3.org/2000/01/rdf-schema#label> \"Dave\" .\n",
        ),
    )
    .unwrap();

    let ours = dir.path().join("root/tinykg/2026-06-01");
    let other = dir.path().join("root/tinykg/2026-07-01");
    for (out, input) in [(&ours, &mine), (&other, &theirs)] {
        kgf(
            &[
                "build",
                "--config",
                "-",
                "--out",
                path(out),
                "--input",
                path(input),
                "--hdtc",
                &hdtc(),
            ],
            CONFIG,
        )
        .ok();
    }

    let key_count = |bundle: &std::path::Path| -> u64 {
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
        manifest["artifacts"]["keysets/shared.keys"]["keys"]["key_count"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(
        key_count(&ours),
        key_count(&other),
        "the fixtures must agree on this count, or the test proves nothing"
    );

    std::fs::copy(
        other.join("keysets/shared.keys"),
        ours.join("keysets/shared.keys"),
    )
    .unwrap();
    let stderr = kgf(&["manifest", path(&ours)], "").err();
    assert!(stderr.contains("different HDT"), "{stderr}");
    assert!(stderr.contains("another bundle"), "{stderr}");
}

/// Doc 17 §17.4: "the manifest mirrors the header, it never overrides it." A
/// checksum cannot enforce that — the file stays intact while the manifest
/// misreports what is in it — and §17.4 makes these the values a registry
/// verifies on ingest, so `--check` has to compare them.
#[test]
fn a_manifest_that_misreports_key_metadata_fails_check() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("tiny.nt");
    std::fs::write(&source, SOURCE).unwrap();
    let out = dir.path().join("root/tinykg/2026-06-01");
    kgf(
        &[
            "build",
            "--config",
            "-",
            "--out",
            path(&out),
            "--input",
            path(&source),
            "--hdtc",
            &hdtc(),
        ],
        CONFIG,
    )
    .ok();
    kgf(&["manifest", path(&out), "--check"], "").ok();

    // Every artifact byte is untouched; only the manifest lies.
    let manifest_path = out.join("manifest.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    document["artifacts"]["keysets/shared.keys"]["keys"]["key_count"] = serde_json::json!(9_999);
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&document).unwrap(),
    )
    .unwrap();

    let stderr = kgf(&["manifest", path(&out), "--check"], "").err();
    assert!(stderr.contains("misdescribes"), "{stderr}");
    assert!(stderr.contains("§17.4"), "{stderr}");
}

/// The half the count identity cannot cover: a file whose header names a
/// different role than its name does is misplaced, whatever the totals say.
#[test]
fn a_key_set_stored_under_another_role_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("tiny.nt");
    std::fs::write(&source, SOURCE).unwrap();
    let out = dir.path().join("root/tinykg/2026-06-01");
    kgf(
        &[
            "build",
            "--config",
            "-",
            "--out",
            path(&out),
            "--input",
            path(&source),
            "--hdtc",
            &hdtc(),
        ],
        CONFIG,
    )
    .ok();

    std::fs::copy(
        out.join("keysets/objects-only.keys"),
        out.join("keysets/shared.keys"),
    )
    .unwrap();
    let stderr = kgf(&["manifest", path(&out)], "").err();
    assert!(stderr.contains("declares role"), "{stderr}");
    assert!(stderr.contains("wrong place"), "{stderr}");
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

/// A prefix table that does not exist fails the build immediately, not at the
/// namespace inventory — the last hdtc invocation of the whole pipeline, after
/// the text index, sketches, key sets and VoID have all been built.
///
/// It is checked on the build path and not in `--check-config`, because a
/// rendered config names paths inside the build container while registry CI
/// validates it on the host.
#[test]
fn a_missing_prefix_table_fails_before_anything_is_built() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("tiny.nt");
    std::fs::write(&source, SOURCE).unwrap();
    let out = dir.path().join("root/tinykg/2026-06-01");
    let config =
        format!("{CONFIG}contents: {{stats: {{prefix_tables: ['/nope/missing.json']}}}}\n");

    let stderr = kgf(
        &[
            "build",
            "--config",
            "-",
            "--out",
            path(&out),
            "--input",
            path(&source),
            "--hdtc",
            &hdtc(),
        ],
        &config,
    )
    .err();
    assert!(stderr.contains("/nope/missing.json"), "{stderr}");
    assert!(!out.exists(), "nothing may be published");
    assert!(
        !out.parent().unwrap().exists()
            || std::fs::read_dir(out.parent().unwrap()).unwrap().count() == 0,
        "the build ran far enough to stage something"
    );

    // The same config still passes `--check-config`, which cannot know the
    // paths of the machine that will run the build.
    kgf(&["build", "--config", "-", "--check-config"], &config).ok();
}

/// `--adopt` must not cost the caller their input when the build fails.
///
/// Staging is a temporary directory deleted on any error, so moving the source
/// into it put the only copy somewhere a later failure would destroy. The
/// source is linked instead and released only after the publishing rename.
#[test]
fn a_failed_adopt_build_leaves_the_input_alone() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("tiny.nt");
    std::fs::write(&source, SOURCE).unwrap();
    let staging = dir.path().join("staging/tinykg/v0");
    kgf(
        &[
            "build",
            "--config",
            "-",
            "--out",
            path(&staging),
            "--input",
            path(&source),
            "--hdtc",
            &hdtc(),
        ],
        CONFIG,
    )
    .ok();
    let hdt = staging.join("data.hdt");
    let before = std::fs::read(&hdt).unwrap();

    // Fails after the input is in place: the digest is checked once the bytes
    // have been read, which is downstream of `--adopt`'s move.
    let out = dir.path().join("root/tinykg/2026-06-01");
    kgf(
        &[
            "build",
            "--config",
            "-",
            "--out",
            path(&out),
            "--hdt",
            path(&hdt),
            "--adopt",
            "--hdtc",
            &hdtc(),
            "--source-sha256",
            &"0".repeat(64),
        ],
        CONFIG,
    )
    .err();

    assert!(
        hdt.exists(),
        "a failed --adopt build destroyed its own input"
    );
    assert_eq!(
        std::fs::read(&hdt).unwrap(),
        before,
        "the input survived but was modified"
    );
}

/// And on success it does go away, which is what `--adopt` promises.
#[test]
fn a_successful_adopt_build_releases_the_input() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("tiny.nt");
    std::fs::write(&source, SOURCE).unwrap();
    let staging = dir.path().join("staging/tinykg/v0");
    kgf(
        &[
            "build",
            "--config",
            "-",
            "--out",
            path(&staging),
            "--input",
            path(&source),
            "--hdtc",
            &hdtc(),
        ],
        CONFIG,
    )
    .ok();
    let hdt = staging.join("data.hdt");

    let out = dir.path().join("root/tinykg/2026-06-01");
    kgf(
        &[
            "build",
            "--config",
            "-",
            "--out",
            path(&out),
            "--hdt",
            path(&hdt),
            "--adopt",
            "--hdtc",
            &hdtc(),
        ],
        CONFIG,
    )
    .ok();

    assert!(!hdt.exists(), "--adopt left the input behind");
    assert!(out.join("data.hdt").exists());
}

/// The config schema documented in `notes/build-bundle.md` §3 must be one the
/// parser accepts.
///
/// Written after two keys in that sample had already gone stale — a `stats:
/// enabled:` that was never modelled, and a `source:` block that became flags —
/// neither of which anything would have caught. A schema documented wrongly is
/// worse than one documented not at all: it is what a reader copies.
#[test]
fn the_documented_config_sample_parses() {
    let note = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../notes/build-bundle.md"),
    )
    .expect("notes/build-bundle.md is beside the crate it documents");
    let sample = note
        .split("```yaml")
        .map(|block| block.split("```").next().unwrap_or_default())
        .find(|block| block.trim_start().starts_with("schema: 1"))
        .expect("§3 documents a complete config, opening with `schema: 1`");

    let stdout = kgf(&["build", "--config", "-", "--check-config"], sample).ok();
    let plan: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        plan["contents"]["filters"]["k"], 65536,
        "k is not a config knob"
    );
}

/// Doc 04 §4.4's component DAG is not built here, and a config that declares
/// one is refused with that reason rather than with "unknown field". The
/// failure mode this prevents is quiet: a bundle whose config named components
/// and whose artifacts contain none would be described as an ordinary bundle,
/// with every per-component statistic and graph identity silently absent.
#[test]
fn a_config_declaring_components_is_refused_with_a_reason() {
    for field in ["components", "publish"] {
        let stderr = kgf(
            &["build", "--config", "-", "--check-config"],
            &format!("{MINIMAL}{field}: []\n"),
        )
        .err();
        assert!(stderr.contains("doc 04 §4.4"), "{field}: {stderr}");
        assert!(stderr.contains("--input"), "{field}: {stderr}");
    }
}

#[test]
fn an_unreadable_config_names_the_file() {
    let stderr = kgf(
        &[
            "build",
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
