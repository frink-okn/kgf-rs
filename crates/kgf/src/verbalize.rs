//! `kgf verbalize`: write the texts a bundle's roots would be embedded from.
//!
//! The command form of the verbalizer in `kgf-server`: a bundle directory and
//! a config in, one JSON line per distinct text out, ready for the embedding
//! stage. It exists for two callers. The build pipeline will run it as a
//! stage; until then, and for anyone with a bundle on disk, it is the way to
//! run a config end to end and read what it produces.
//!
//! The bundle is opened the way `kgf manifest` opens one: this crate asserts
//! the publication invariant for a directory the operator named, holds the
//! mappings for the run, and writes nothing into it.

use std::collections::BTreeMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use kgf_store::manifest::default_predicate_roles;
use kgf_store::{Manifest, OpenOptions, Store};
use kgf_verbalize::{Bound, Config, Grouper, Record, Verbalizer};

/// `kgf verbalize` arguments.
#[derive(Debug, Parser)]
pub struct Args {
    /// A published bundle version directory.
    pub bundle: PathBuf,

    /// The verbalization config, YAML or JSON. `-` reads standard input.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,

    /// Where to write the records, one JSON object per line.
    #[arg(long, short, value_name = "FILE")]
    pub output: PathBuf,

    /// Only these targets, by their config names. Repeatable; default all.
    #[arg(long, value_name = "NAME")]
    pub target: Vec<String>,

    /// Stop after this many roots per target.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Verbalize only these roots, under the one `--target` given.
    #[arg(long, value_name = "IRI", requires = "target")]
    pub iri: Vec<String>,

    /// Source IRIs kept on a record whose text several roots share.
    #[arg(long, value_name = "N", default_value_t = 10)]
    pub max_iris_per_record: usize,

    /// Write a readable text dump instead of JSON lines.
    #[arg(long)]
    pub text: bool,
}

/// Run the command.
pub fn run(args: Args) -> Result<()> {
    let config = read_config(&args.config)?;
    let resolved = config
        .resolve()
        .context("resolving the verbalization config")?;

    let opened = open_bundle(&args.bundle)?;
    let manifest = Manifest::read(&args.bundle)
        .with_context(|| format!("reading the manifest of {}", args.bundle.display()))?;
    let roles = if manifest.predicate_roles.is_empty() {
        default_predicate_roles()
    } else {
        manifest.predicate_roles.clone()
    };
    let label_role = roles.get("label").cloned().unwrap_or_default();

    let bound = Bound::bind(&opened.store, &resolved, &label_role)?;
    for unknown in bound.unknown() {
        eprintln!(
            "warning: {}: {} is not in this bundle",
            unknown.at, unknown.iri
        );
    }

    let targets = select_targets(&bound, &args.target)?;
    let mut verbalizer = Verbalizer::new(&opened.store, &bound);
    let mut grouper = Grouper::new(args.max_iris_per_record);
    let started = Instant::now();

    if args.iri.is_empty() {
        for (index, name) in &targets {
            let roots = verbalizer.roots(*index)?;
            let total = args
                .limit
                .map_or(roots.len(), |limit| limit.min(roots.len()));
            let target_started = Instant::now();
            for &root in roots.iter().take(total) {
                if let Some(rendered) = verbalizer.verbalize(*index, root)? {
                    grouper.add(rendered);
                }
            }
            eprintln!(
                "{name}: {total} roots in {:.1?} ({} distinct texts so far)",
                target_started.elapsed(),
                grouper.len()
            );
        }
    } else {
        let [(index, name)] = targets.as_slice() else {
            bail!("--iri needs exactly one --target");
        };
        for iri in &args.iri {
            match verbalizer.verbalize_iri(*index, iri)? {
                Some(rendered) => grouper.add(rendered),
                None => eprintln!("warning: {iri} is not a subject in this bundle ({name})"),
            }
        }
    }

    let records = grouper.finish();
    write_records(&args.output, &records, args.text)?;
    eprintln!(
        "wrote {} records to {} in {:.1?}",
        records.len(),
        args.output.display(),
        started.elapsed()
    );
    Ok(())
}

fn read_config(path: &Path) -> Result<Config> {
    let text = if path == Path::new("-") {
        std::io::read_to_string(std::io::stdin()).context("reading the config from stdin")?
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading the config {}", path.display()))?
    };
    serde_norway::from_str(&text).with_context(|| format!("parsing the config {}", path.display()))
}

/// The targets to run: `(index, name)` in config order, or the named subset.
fn select_targets(bound: &Bound, names: &[String]) -> Result<Vec<(usize, String)>> {
    let all: Vec<(usize, String)> = bound
        .targets()
        .enumerate()
        .map(|(index, name)| (index, name.to_owned()))
        .collect();
    if names.is_empty() {
        return Ok(all);
    }
    let by_name: BTreeMap<&str, usize> = all
        .iter()
        .map(|(index, name)| (name.as_str(), *index))
        .collect();
    let mut selected = Vec::with_capacity(names.len());
    for name in names {
        let Some(index) = by_name.get(name.as_str()) else {
            bail!(
                "no target named {name:?}; the config declares {}",
                all.iter()
                    .map(|(_, name)| format!("{name:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        };
        selected.push((*index, name.clone()));
    }
    Ok(selected)
}

fn write_records(path: &Path, records: &[Record], text: bool) -> Result<()> {
    let file = std::fs::File::create(path)
        .with_context(|| format!("creating the output {}", path.display()))?;
    let mut out = BufWriter::new(file);
    for record in records {
        if text {
            writeln!(out, "label: {}", record.label)?;
            writeln!(out, "iris:")?;
            for iri in &record.iris {
                writeln!(out, "- {iri}")?;
            }
            writeln!(out)?;
            writeln!(out, "{}", record.embedding_text)?;
            writeln!(out, "\n---\n")?;
        } else {
            serde_json::to_writer(&mut out, record)?;
            out.write_all(b"\n")?;
        }
    }
    out.flush()
        .with_context(|| format!("writing the output {}", path.display()))
}

/// A bundle opened for the run.
struct Opened {
    store: Store,
}

/// Open the bundle the operator named.
///
/// # Safety obligation
///
/// [`PublishedBundle::new`](kgf_store::PublishedBundle::new) requires that the
/// artifacts not be modified or truncated while mapped. This command holds the
/// mappings for one run over a directory the operator named as published, and
/// writes only to `--output`, which it refuses to place inside the bundle.
#[allow(unsafe_code)]
fn open_bundle(dir: &Path) -> Result<Opened> {
    ensure!(dir.is_dir(), "{} is not a bundle directory", dir.display());
    let bundle = unsafe { kgf_store::PublishedBundle::new(dir) };
    let store = Store::open(&bundle, OpenOptions::default())
        .with_context(|| format!("opening the bundle {}", dir.display()))?;
    Ok(Opened { store })
}
