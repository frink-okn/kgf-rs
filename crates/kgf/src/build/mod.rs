//! `kgf build` — assemble and publish a complete bundle.
//!
//! One command and one config, so that a caller — kace's build job, a
//! contributor with a `.nt.gz` — learns the artifact ordering once, from here,
//! rather than reproducing it.
//!
//! Three ways in, sharing one resolution: `--check-config` validates and prints
//! the resolved plan, `--dry-run` prints the commands the plan implies, and a
//! plain run carries them out. The first needs neither an output nor an input,
//! and none of the three can disagree about what the config means, because all
//! three read the same [`plan::ConfigPlan`].

mod execute;
mod hdtc;
mod prefixes;

pub mod config;
pub mod plan;
pub mod stats;

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use plan::{BundlePlan, ConfigPlan, Input, Provenance, VersionLabel};

/// Arguments for `kgf build`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Build configuration, YAML or JSON. `-` reads standard input.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,

    /// The version directory to publish: `{root}/{dataset}/{version}`.
    #[arg(long, value_name = "DIR", required_unless_present = "check_config")]
    pub out: Option<PathBuf>,

    /// An existing HDT to use as this bundle's `data.hdt`.
    #[arg(long, value_name = "FILE", conflicts_with = "input")]
    pub hdt: Option<PathBuf>,

    /// Move the HDT named by `--hdt` instead of copying it.
    #[arg(long, requires = "hdt")]
    pub adopt: bool,

    /// RDF input to build from. Repeatable.
    #[arg(long = "input", value_name = "FILE")]
    pub input: Vec<PathBuf>,

    /// Version label. Defaults to the final component of `--out`.
    #[arg(long, value_name = "LABEL")]
    pub version: Option<String>,

    /// The version this one supersedes.
    #[arg(long, value_name = "LABEL")]
    pub previous_version: Option<String>,

    /// Where the input came from. Recorded in the manifest, never fetched.
    #[arg(long, value_name = "URL")]
    pub source_url: Option<String>,

    /// Expected SHA-256 of the input, verified against the bytes read.
    #[arg(long, value_name = "HEX")]
    pub source_sha256: Option<String>,

    /// Builder image reference. Recorded in the manifest.
    #[arg(long, value_name = "REF")]
    pub builder_image: Option<String>,

    /// hdtc executable used for every external build step.
    #[arg(long, default_value = "hdtc")]
    pub hdtc: PathBuf,

    /// Validate the config, print the resolved plan as JSON, and build nothing.
    ///
    /// Needs no `--out` and no input, so a registry can check an entry without
    /// knowing where a bundle would go.
    #[arg(long)]
    pub check_config: bool,

    /// Print every command the build would run, in order, and run none of them.
    #[arg(long, conflicts_with = "check_config")]
    pub dry_run: bool,
}

/// Run `kgf build`.
pub fn run(args: Args) -> Result<()> {
    let config = read_config(&args.config)?;
    let config = ConfigPlan::resolve(config)?;

    if args.check_config {
        println!("{}", serde_json::to_string_pretty(&config)?);
        return Ok(());
    }

    let build = resolve_build(args, config)?;

    if build.dry_run {
        print!("{}", execute::rehearse(&build));
        return Ok(());
    }

    let built = execute::execute(&build)?;
    print!("{}", execute::report(&built, &build.plan.output));
    Ok(())
}

/// A resolved plan and the process-level settings that carry it out.
///
/// `hdtc` and `dry_run` are not in [`BundlePlan`] because neither describes the
/// bundle: which binary runs and whether it runs at all are facts about this
/// invocation, and a plan that carried them would serialize them into the
/// document `--check-config` prints.
struct Build {
    plan: BundlePlan,
    hdtc: PathBuf,
    dry_run: bool,
}

fn resolve_build(args: Args, config: ConfigPlan) -> Result<Build> {
    let out = args
        .out
        .as_deref()
        .context("--out is required unless --check-config is given")?;
    let version = args
        .version
        .as_deref()
        .map(str::parse::<VersionLabel>)
        .transpose()
        .context("--version is not a usable version label")?;
    let (output, version) = plan::resolve_output(out, &config.dataset.id, version.as_ref())?;

    ensure!(
        !output.exists(),
        "{} already exists; a published version is immutable, so \
         a rebuild is a new version directory rather than a rewrite",
        output.display()
    );

    let previous_version = args
        .previous_version
        .as_deref()
        .map(str::parse::<VersionLabel>)
        .transpose()
        .context("--previous-version is not a usable version label")?;
    ensure!(
        previous_version.as_ref() != Some(&version),
        "--previous-version names this same version, {version}"
    );

    let input = resolve_input(&args)?;
    let provenance = resolve_provenance(&args)?;

    // Checked here rather than in `ConfigPlan::resolve`, and long before the
    // last hdtc invocation. Not in resolution, because a rendered config names
    // paths inside the *build container* and registry CI runs `--check-config`
    // on the host, where `/kgf/prefixes.yaml` legitimately does not exist —
    // existence is a fact about this machine, not about the config. But not at
    // the namespace inventory either, which is where it used to surface: a
    // one-character typo otherwise costs the HDT, permutation, text index,
    // sketches, key sets and VoID before anyone hears about it.
    for table in &config.contents.stats.prefix_tables {
        ensure!(
            table.is_file(),
            "contents.stats.prefix_tables names {}, which is not a file on this machine",
            table.display()
        );
    }

    Ok(Build {
        plan: BundlePlan {
            config,
            version,
            previous_version,
            input,
            output,
            provenance,
        },
        hdtc: args.hdtc,
        dry_run: args.dry_run,
    })
}

fn resolve_input(args: &Args) -> Result<Input> {
    match (&args.hdt, args.input.as_slice()) {
        (Some(hdt), []) => {
            ensure!(hdt.is_file(), "--hdt {} is not a file", hdt.display());
            Ok(Input::Hdt {
                path: hdt.clone(),
                adopt: args.adopt,
            })
        }
        (None, []) => bail!("no input; pass --hdt for an existing HDT or --input for RDF"),
        (None, paths) => {
            for path in paths {
                ensure!(path.exists(), "--input {} does not exist", path.display());
            }
            Ok(Input::Rdf {
                paths: paths.to_vec(),
            })
        }
        // clap's `conflicts_with` rejects this before we see it; the arm keeps
        // the match total without a panic that would outlive the attribute.
        (Some(_), _) => bail!("--hdt and --input are alternatives, not a pair"),
    }
}

fn resolve_provenance(args: &Args) -> Result<Provenance> {
    if let Some(digest) = &args.source_sha256 {
        ensure!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "--source-sha256 must be 64 hex digits, not {digest:?}"
        );
        ensure!(
            digest.chars().all(|c| !c.is_ascii_uppercase()),
            "--source-sha256 must be lowercase hex, to match how a manifest \
             spells every other digest"
        );
    }
    Ok(Provenance {
        source_url: args.source_url.clone(),
        expected_sha256: args.source_sha256.clone(),
        builder_image: args.builder_image.clone(),
    })
}

/// Read a config from a path, or from standard input for `-`.
///
/// YAML is a superset of JSON and `serde_norway` parses both, so one reader
/// covers a hand-written file and a machine-rendered one. It is also the parser
/// hdtc uses for the prefix tables this config points at, which keeps one YAML
/// implementation across the toolchain rather than two that disagree at the
/// edges.
fn read_config(path: &Path) -> Result<config::Config> {
    let text = if path == Path::new("-") {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("reading build config from standard input")?;
        text
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading build config {}", path.display()))?
    };
    let source = if path == Path::new("-") {
        "standard input".to_owned()
    } else {
        path.display().to_string()
    };
    serde_norway::from_str(&text).with_context(|| format!("parsing build config {source}"))
}
