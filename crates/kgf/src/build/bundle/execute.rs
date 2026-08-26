//! Running a resolved plan: hdtc invocations, staging, and publication.
//!
//! Every step writes into a staging directory beside the output, and the last
//! thing that happens is a `rename`. Nothing is ever written into a directory a
//! running server may have mapped (doc 04 §4.6), and a build that fails at any
//! step leaves the bundle root exactly as it found it.
//!
//! The manifest is written last, inside staging, so a directory that appears at
//! the output path is a complete bundle by construction — there is no window in
//! which a scan can see artifacts without the document that describes them.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use kgf_store::manifest::{Generator, Manifest, Source, SourceInput};
use kgf_store::store::artifact;
use sha2::{Digest, Sha256};

use super::Build;
use super::plan::{BundlePlan, Input, KEYSET_ROLES, SKETCH_ROLES};
use crate::build::stats;
use crate::manifest::Requested;

/// Build and publish one bundle.
pub(super) fn execute(build: &Build) -> Result<Manifest> {
    let plan = &build.plan;
    let parent = plan
        .output
        .parent()
        .context("--out has no parent directory to stage beside")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating dataset directory {}", parent.display()))?;

    // Staging is a sibling of the output so that publication is a rename on one
    // filesystem, and dot-prefixed so `Catalog::scan` does not mistake it for a
    // release while it is being written.
    let staging = tempfile::Builder::new()
        .prefix(&format!(".kgf-build-{}-", plan.version))
        .tempdir_in(parent)
        .with_context(|| format!("creating staging directory under {}", parent.display()))?;

    // Scratch lives outside staging: anything inside it would be published by
    // the rename. hdtc's own temp directories hang off this, one per
    // invocation — never shared, per doc 18 §18.4.
    let work = match &plan.config.resources.temp_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating scratch directory {}", dir.display()))?;
            tempfile::Builder::new()
                .prefix(".kgf-work-")
                .tempdir_in(dir)
        }
        None => tempfile::Builder::new()
            .prefix(".kgf-work-")
            .tempdir_in(parent),
    }
    .context("creating build scratch directory")?;

    let runner = Runner {
        hdtc: &build.hdtc,
        work: work.path(),
        memory_limit: plan
            .config
            .resources
            .memory_limit
            .map(|size| size.to_hdtc_arg()),
    };

    let layout = Layout {
        staging: staging.path().to_path_buf(),
        data: staging.path().join(artifact::HDT),
    };
    let inputs = materialize(plan, &runner, &layout)?;
    verify_asserted_digest(plan, &inputs)?;

    for step in sidecar_steps(plan, &layout) {
        runner.run(&step)?;
    }

    let staged_stats = staging.path().join("stats");
    std::fs::create_dir(&staged_stats)
        .with_context(|| format!("creating {}", staged_stats.display()))?;
    let card = stats::DatasetCard {
        id: plan.config.dataset.id.as_str(),
        version: plan.version.as_str(),
        title: plan.config.dataset.title.as_deref(),
        description: plan.config.dataset.description.as_deref(),
        license: plan.config.dataset.license.as_deref(),
        homepage: plan.config.dataset.homepage.as_deref(),
    };
    let outcome = stats::produce(
        stats::Inputs {
            hdtc: &build.hdtc,
            data: &layout.data,
            dataset_iri: plan.config.dataset.iri.as_str(),
            prefix_tables: &plan.config.contents.stats.prefix_tables,
            extra_prefixes: &plan.config.semantics.prefixes,
            card,
            work: work.path(),
        },
        &staged_stats,
    )?;

    let requested = requested_manifest(plan, inputs, &build.hdtc)?;
    let manifest =
        crate::manifest::write_description_manifest(staging.path(), &requested, &outcome.metadata)?;

    if let Some(cap) = plan.config.resources.max_bundle_bytes {
        let total: u64 = manifest.artifacts.values().map(|entry| entry.bytes).sum();
        ensure!(
            total <= cap.bytes(),
            "the built bundle is {total} bytes, over the configured \
             resources.max_bundle_bytes of {cap}; nothing was published"
        );
    }

    // `into_path` because the rename must move the directory, not drop it.
    let staged = staging.keep();
    std::fs::rename(&staged, &plan.output).with_context(|| {
        format!(
            "publishing {} to {}",
            staged.display(),
            plan.output.display()
        )
    })?;
    Ok(manifest)
}

/// Where a build's artifacts go. Purely paths, so that the commands a plan
/// implies can be worked out without creating any of them.
struct Layout {
    staging: PathBuf,
    data: PathBuf,
}

/// One hdtc invocation.
struct Step {
    /// What this step is called in logs and error messages.
    name: &'static str,
    /// The full argument list, less the settings [`Runner`] adds to every step.
    args: Vec<OsString>,
    /// A private temporary directory, for the steps that sort to disk.
    temp: Option<&'static str>,
}

/// The invocation that produces `data.hdt.perm`, and `data.hdt` with it when
/// the input is RDF.
///
/// `hdtc create --perm` builds both in one pass, which is why adopting an
/// existing HDT is the more expensive route rather than the cheaper one: it
/// pays a separate `hdtc perm` over bytes the create step would have permuted
/// while it already had them.
fn core_step(plan: &BundlePlan, layout: &Layout) -> Step {
    let position_maps = plan
        .config
        .contents
        .perm
        .position_maps
        .iter()
        .map(|map| map.as_str())
        .collect::<Vec<_>>()
        .join(",");

    match &plan.input {
        Input::Hdt { .. } => {
            let mut args = vec![OsString::from("perm"), layout.data.as_os_str().to_owned()];
            if !position_maps.is_empty() {
                args.push(OsString::from("--position-maps"));
                args.push(OsString::from(&position_maps));
            }
            Step {
                name: "permutation index",
                args,
                temp: Some("perm"),
            }
        }
        Input::Rdf { paths } => {
            let mut args = vec![
                OsString::from("create"),
                OsString::from("--output"),
                layout.data.as_os_str().to_owned(),
                OsString::from("--perm"),
                OsString::from("--dataset-uri"),
                OsString::from(plan.config.dataset.iri.as_str()),
            ];
            if !position_maps.is_empty() {
                args.push(OsString::from("--perm-position-maps"));
                args.push(OsString::from(&position_maps));
            }
            args.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
            Step {
                name: "HDT and permutation build",
                args,
                temp: Some("create"),
            }
        }
    }
}

/// The sidecar steps, in the order they run.
///
/// The `--roles` lists are stated rather than left to hdtc's defaults. Doc 17
/// §17.3 makes each sketch family all-or-nothing and doc 18 §18.4 fixes the
/// key-set trio, so these are the KGF profile and not hdtc's preference: a
/// future hdtc changing a default must not quietly change what a bundle
/// publishes.
fn sidecar_steps(plan: &BundlePlan, layout: &Layout) -> Vec<Step> {
    let mut steps = Vec::new();

    if let Some(text) = &plan.config.contents.text {
        let mut args = vec![
            OsString::from("text"),
            layout.data.as_os_str().to_owned(),
            OsString::from("--output"),
            layout.staging.join(artifact::TEXT).into_os_string(),
            OsString::from("--max-literal-bytes"),
            OsString::from(text.max_literal_bytes.to_string()),
            OsString::from("--untagged-language"),
            OsString::from(text.untagged_language.as_str()),
        ];
        if text.index_all_datatypes {
            args.push(OsString::from("--index-all-datatypes"));
        }
        for datatype in &text.exclude_datatypes {
            args.push(OsString::from("--exclude-datatype"));
            args.push(OsString::from(datatype));
        }
        if let Some(threads) = plan.config.resources.threads {
            args.push(OsString::from("--threads"));
            args.push(OsString::from(threads.to_string()));
        }
        steps.push(Step {
            name: "text index",
            args,
            temp: None,
        });
    }

    let filters = &plan.config.contents.filters;
    steps.push(Step {
        name: "membership filters and overlap sketches",
        args: vec![
            OsString::from("sketch"),
            layout.data.as_os_str().to_owned(),
            OsString::from("--output-dir"),
            layout.staging.join("filters").into_os_string(),
            OsString::from("--k"),
            OsString::from(filters.k.to_string()),
            OsString::from("--filter-bits"),
            OsString::from(u8::from(filters.filter_bits).to_string()),
            OsString::from("--roles"),
            OsString::from(SKETCH_ROLES),
        ],
        temp: Some("sketch"),
    });
    steps.push(Step {
        name: "exact key sets",
        args: vec![
            OsString::from("keyset"),
            layout.data.as_os_str().to_owned(),
            OsString::from("--output-dir"),
            layout.staging.join("keysets").into_os_string(),
            OsString::from("--encoding"),
            OsString::from(plan.config.contents.keysets.encoding.as_str()),
            OsString::from("--roles"),
            OsString::from(KEYSET_ROLES),
        ],
        temp: Some("keyset"),
    });

    steps
}

/// Every command this plan would run, in order, against a named staging path.
///
/// Nothing is created. The staging path is the one a real build would generate
/// bar its random suffix, so the output is a script a person can follow rather
/// than a sketch of one — which is the point, since the place this is read is a
/// build job's logs.
pub(super) fn rehearse(build: &Build) -> String {
    use std::fmt::Write;

    let plan = &build.plan;
    let parent = plan.output.parent().unwrap_or(Path::new("."));
    let staging = parent.join(format!(".kgf-build-{}-XXXXXX", plan.version));
    let layout = Layout {
        data: staging.join(artifact::HDT),
        staging,
    };
    let work = parent.join(".kgf-work-XXXXXX");
    let runner = Runner {
        hdtc: &build.hdtc,
        work: &work,
        memory_limit: plan
            .config
            .resources
            .memory_limit
            .map(|size| size.to_hdtc_arg()),
    };

    let mut out = String::new();
    if let Input::Hdt { path, adopt } = &plan.input {
        let verb = if *adopt { "move" } else { "copy" };
        let _ = writeln!(
            out,
            "# {verb} {} to {}, hashing it",
            path.display(),
            layout.data.display()
        );
    }
    for step in std::iter::once(core_step(plan, &layout)).chain(sidecar_steps(plan, &layout)) {
        let _ = writeln!(out, "# {}", step.name);
        let _ = writeln!(out, "{}", render(&runner.hdtc_argv(&step)));
    }
    let _ = writeln!(
        out,
        "# tier-1 description set into {}",
        layout.staging.join("stats").display()
    );
    let _ = writeln!(
        out,
        "# manifest, written last, into {}",
        layout.staging.join(artifact::MANIFEST).display()
    );
    let _ = writeln!(
        out,
        "# publish: mv {} {}",
        layout.staging.display(),
        plan.output.display()
    );
    out
}

/// Put `data.hdt` and its permutation sidecar in place, and hash what was read.
fn materialize(
    plan: &BundlePlan,
    runner: &Runner<'_>,
    layout: &Layout,
) -> Result<Vec<SourceInput>> {
    let data = &layout.data;
    match &plan.input {
        Input::Hdt { path, adopt } => {
            // kace's conversion also writes an HDT-FoQ index, which KGF never
            // reads (doc 20 §20.8). Only the HDT itself is taken.
            let digest = place(path, data, *adopt)?;
            runner.run(&core_step(plan, layout))?;
            Ok(vec![SourceInput {
                url: plan.provenance.source_url.clone(),
                format: "hdt".to_owned(),
                sha256: digest,
            }])
        }
        Input::Rdf { paths } => {
            runner.run(&core_step(plan, layout))?;

            // One `url` for several inputs cannot be apportioned, so it names
            // the first and the rest carry their digests alone. A caller with
            // several sources should be recording several URLs, which is a
            // config-level change rather than a guess made here.
            paths
                .iter()
                .enumerate()
                .map(|(index, path)| {
                    Ok(SourceInput {
                        url: (index == 0)
                            .then(|| plan.provenance.source_url.clone())
                            .flatten(),
                        format: "rdf".to_owned(),
                        sha256: hash_file(path)?,
                    })
                })
                .collect()
        }
    }
}

/// Check an asserted input digest against the bytes actually read.
///
/// This is what makes `--source-sha256` an integrity check on the download
/// rather than a value copied into the manifest: the digest compared here was
/// computed from the file this build consumed.
fn verify_asserted_digest(plan: &BundlePlan, inputs: &[SourceInput]) -> Result<()> {
    let Some(expected) = &plan.provenance.expected_sha256 else {
        return Ok(());
    };
    let [only] = inputs else {
        bail!(
            "--source-sha256 names one digest but this build read {} inputs; \
             assert per-input digests in the config instead",
            inputs.len()
        );
    };
    ensure!(
        &only.sha256 == expected,
        "the input hashes to {}, not the asserted {expected}; nothing was published",
        only.sha256
    );
    Ok(())
}

fn requested_manifest(
    plan: &BundlePlan,
    inputs: Vec<SourceInput>,
    hdtc: &Path,
) -> Result<Requested> {
    Ok(Requested {
        id: Some(plan.config.dataset.id.to_string()),
        version: Some(plan.version.to_string()),
        dataset_iri: Some(plan.config.dataset.iri.as_str().to_owned()),
        title: plan.config.dataset.title.clone(),
        description: plan.config.dataset.description.clone(),
        license: plan.config.dataset.license.clone(),
        homepage: plan.config.dataset.homepage.clone(),
        publisher: plan
            .config
            .dataset
            .publisher
            .as_ref()
            .map(|publisher| publisher.name.clone()),
        publisher_contact: plan
            .config
            .dataset
            .publisher
            .as_ref()
            .and_then(|publisher| publisher.contact.clone()),
        previous_version: plan.previous_version.as_ref().map(ToString::to_string),
        prefixes: plan.config.semantics.prefixes.clone(),
        roles: plan.config.semantics.roles.clone(),
        source: Some(Source {
            inputs,
            generator: Some(Generator {
                kgf: Some(env!("CARGO_PKG_VERSION").to_owned()),
                hdtc: hdtc_version(hdtc),
                image: plan.provenance.builder_image.clone(),
            }),
        }),
    })
}

/// The version of the hdtc that ran, read from the binary rather than declared.
///
/// A declared version would be a claim about the toolchain; this is an
/// observation of it, and the difference is the whole value of recording it.
/// Unreadable is recorded as absent rather than guessed.
fn hdtc_version(hdtc: &Path) -> Option<String> {
    let output = Command::new(hdtc).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.trim().to_owned()).filter(|text| !text.is_empty())
}

/// Put an existing file at `dest`, and return its SHA-256.
///
/// Copying hashes in the same pass, so a large HDT is read once rather than
/// twice. Moving cannot, and pays a second read — which is still the cheaper
/// option overall, since it skips writing a second copy of the file.
fn place(source: &Path, dest: &Path, adopt: bool) -> Result<String> {
    if adopt {
        match std::fs::rename(source, dest) {
            Ok(()) => return hash_file(dest),
            // A rename across filesystems is the expected failure and a copy is
            // the only way through, but every other rename failure is worth one
            // attempt at a copy too: the fallback is strictly more capable, so
            // telling the causes apart would only turn recoverable cases into
            // errors. If the copy also fails, its own message says why.
            Err(error) => tracing::debug!(
                %error,
                source = %source.display(),
                "moving the input failed; copying instead"
            ),
        }
    }
    copy_hashing(source, dest)
}

fn copy_hashing(source: &Path, dest: &Path) -> Result<String> {
    use std::io::{Read, Write};

    let mut reader =
        std::fs::File::open(source).with_context(|| format!("opening {}", source.display()))?;
    let mut writer =
        std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("reading {}", source.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        writer
            .write_all(&buffer[..read])
            .with_context(|| format!("writing {}", dest.display()))?;
    }
    writer
        .flush()
        .with_context(|| format!("flushing {}", dest.display()))?;
    Ok(hex(&hasher.finalize()))
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).with_context(|| format!("reading {}", path.display()))?;
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Invokes hdtc with the settings every step shares.
struct Runner<'a> {
    hdtc: &'a Path,
    work: &'a Path,
    memory_limit: Option<String>,
}

impl Runner<'_> {
    /// Run one hdtc step.
    ///
    /// `temp` names a private temporary directory for steps that sort to disk.
    /// It is per invocation and never shared: doc 18 §18.4 records a build in
    /// which concurrent `hdtc` processes sharing one temp directory produced key
    /// sets that were structurally perfect — correct checksums, correct source
    /// digest, ascending keys — and held another graph's keys.
    fn run(&self, step: &Step) -> Result<()> {
        if let Some(temp) = step.temp {
            let dir = self.work.join(temp);
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let argv = self.hdtc_argv(step);
        let command = render(&argv);

        tracing::info!(step = step.name, %command, "running hdtc");
        let started = Instant::now();
        let status = Command::new(self.hdtc)
            .args(&argv[1..])
            .status()
            .with_context(|| format!("running hdtc for {}: {command}", step.name))?;
        ensure!(
            status.success(),
            "hdtc {} failed with {status}: {command}",
            step.name
        );
        tracing::info!(step = step.name, elapsed = ?started.elapsed(), "hdtc step complete");
        Ok(())
    }

    /// The full argument vector: the program, the step, and the settings every
    /// step shares.
    ///
    /// Shared with the rehearsal so `--dry-run` prints the command that would
    /// actually run rather than a second construction of it that can drift.
    fn hdtc_argv(&self, step: &Step) -> Vec<OsString> {
        let mut argv = vec![self.hdtc.as_os_str().to_owned()];
        argv.extend(step.args.iter().cloned());
        if let Some(temp) = step.temp {
            argv.push(OsString::from("--temp-dir"));
            argv.push(self.work.join(temp).into_os_string());
        }
        if let Some(limit) = &self.memory_limit {
            argv.push(OsString::from("--memory-limit"));
            argv.push(OsString::from(limit));
        }
        argv.push(OsString::from("--quiet"));
        argv
    }
}

/// One invocation, spelled the way a person would retype it.
fn render(argv: &[OsString]) -> String {
    argv.iter()
        .map(|argument| quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote(argument: &OsStr) -> String {
    let text = argument.to_string_lossy();
    if text.is_empty() || text.contains(|c: char| c.is_whitespace() || "'\"\\$`".contains(c)) {
        format!("'{}'", text.replace('\'', r"'\''"))
    } else {
        text.into_owned()
    }
}

/// The per-artifact sizes doc 04 §4.4 asks a build to report.
pub(super) fn report(manifest: &Manifest, output: &Path) -> String {
    use std::fmt::Write;

    let mut lines = String::new();
    let sizes: BTreeMap<&str, u64> = manifest
        .artifacts
        .iter()
        .map(|(name, entry)| (name.as_str(), entry.bytes))
        .collect();
    let width = sizes.keys().map(|name| name.len()).max().unwrap_or(0);
    let total: u64 = sizes.values().sum();
    let _ = writeln!(
        lines,
        "{}: published {} triples as {}",
        output.display(),
        manifest.counts.triples,
        manifest.content_digest
    );
    for (name, bytes) in &sizes {
        let _ = writeln!(lines, "  {name:<width$}  {}", human(*bytes));
        // `width` is used by the format string above.
        let _ = width;
    }
    let _ = writeln!(lines, "  {:<width$}  {}", "total", human(total));
    lines
}

fn human(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
        ("B", 1),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            return if scale == 1 {
                format!("{bytes} {unit}")
            } else {
                format!("{:.1} {unit}", bytes as f64 / scale as f64)
            };
        }
    }
    "0 B".to_owned()
}
