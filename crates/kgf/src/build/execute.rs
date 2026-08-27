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
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use kgf_store::manifest::{Generator, Manifest, Source, SourceInput};
use kgf_store::store::artifact;
use sha2::{Digest, Sha256};

use super::Build;
use super::hdtc::{Runner, Step, render};
use super::plan::{BundlePlan, Input, KEYSET_ROLES, SKETCH_ROLES};
use crate::build::stats;
use crate::manifest::Requested;

/// What a completed build produced.
pub(super) struct Built {
    /// The manifest written last, describing everything beside it.
    pub(super) manifest: Manifest,
    /// Row counts from the description set, so an empty one is visible.
    pub(super) description: stats::Outcome,
}

/// Build and publish one bundle.
pub(super) fn execute(build: &Build) -> Result<Built> {
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
            runner: &runner,
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

    // Renamed while the temporary directory still owns it, and disarmed only
    // once the move succeeded. Disarming first left a complete bundle's worth
    // of bytes in `.kgf-build-*` whenever the rename failed — invisible to
    // `Catalog::scan`, which skips dot-prefixed names, and with nothing left to
    // remove it. Retries would have accumulated them on the serving volume.
    std::fs::rename(staging.path(), &plan.output).with_context(|| {
        format!(
            "publishing {} to {}",
            staging.path().display(),
            plan.output.display()
        )
    })?;
    let _ = staging.keep();
    release_adopted_input(plan);
    Ok(Built {
        manifest,
        description: outcome,
    })
}

/// Where a build's artifacts go. Purely paths, so that the commands a plan
/// implies can be worked out without creating any of them.
struct Layout {
    staging: PathBuf,
    data: PathBuf,
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
            let inputs = vec![SourceInput {
                url: plan.provenance.source_url.clone(),
                format: "hdt".to_owned(),
                sha256: digest,
            }];
            // Before the permutation build, not after. The digest is known as
            // soon as the bytes are read, and `hdtc perm` over a large HDT is
            // hours — a corrupt download should not buy them.
            verify_asserted_digest(plan, &inputs)?;
            runner.run(&core_step(plan, layout))?;
            Ok(inputs)
        }
        Input::Rdf { paths } => {
            // One `url` for several inputs cannot be apportioned, so it names
            // the first and the rest carry their digests alone. A caller with
            // several sources should be recording several URLs, which is a
            // config-level change rather than a guess made here.
            let inputs = paths
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
                .collect::<Result<Vec<_>>>()?;
            // Hashing the inputs is cheap beside `hdtc create --perm`, so the
            // assertion is settled first.
            verify_asserted_digest(plan, &inputs)?;
            runner.run(&core_step(plan, layout))?;
            Ok(inputs)
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
            "--source-sha256 asserts one digest, but this build reads {} inputs and \
             there is no way to say which it describes. Build from a single --input \
             or --hdt to assert a digest, or drop the flag",
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
        // A hard link, never a rename. Staging is a temporary directory that is
        // deleted on *any* later failure — a bad digest, a failed sidecar, a
        // full disk — and a rename would put the caller's only copy of the
        // input inside it. Linking leaves the source in place until the build
        // has actually published, so a failed `--adopt` costs nothing.
        match std::fs::hard_link(source, dest) {
            Ok(()) => return hash_file(dest),
            // Across filesystems there is no link to make, and every other
            // failure is worth one attempt at a copy too: the fallback is
            // strictly more capable, so telling the causes apart would only
            // turn recoverable cases into errors.
            Err(error) => tracing::debug!(
                %error,
                source = %source.display(),
                "linking the input failed; copying instead"
            ),
        }
    }
    copy_hashing(source, dest)
}

/// Drop the adopted input, once the bundle that replaced it is published.
///
/// `--adopt` promises the caller's copy goes away; it does not promise when.
/// Doing it after the publishing rename is what makes a failed build harmless,
/// and by then the bundle holds the bytes — under the same inode when the link
/// succeeded, under its own when the copy fallback ran.
///
/// A failure here is reported and not fatal. The bundle is already published
/// and correct; a leftover input is untidy, not wrong, and unpublishing a good
/// bundle over it would be the worse trade.
fn release_adopted_input(plan: &BundlePlan) {
    let Input::Hdt { path, adopt: true } = &plan.input else {
        return;
    };
    if let Err(error) = std::fs::remove_file(path) {
        tracing::warn!(
            %error,
            source = %path.display(),
            "the bundle is published, but the adopted input could not be removed"
        );
    }
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
    Ok(crate::manifest::hex(&hasher.finalize()))
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).with_context(|| format!("reading {}", path.display()))?;
    Ok(crate::manifest::hex(&hasher.finalize()))
}

/// The per-artifact sizes doc 04 §4.4 asks a build to report.
pub(super) fn report(built: &Built, output: &Path) -> String {
    use std::fmt::Write;

    let manifest = &built.manifest;
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
    }
    let _ = writeln!(lines, "  {:<width$}  {}", "total", human(total));
    let _ = writeln!(
        lines,
        "  described: {} schema selectors, {} typed class relations, {} class properties",
        built.description.schema_rows,
        built.description.relation_rows,
        built.description.class_property_rows,
    );
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
