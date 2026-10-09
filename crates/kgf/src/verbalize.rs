//! `kgf verbalize`: write the texts a bundle's roots would be embedded from.
//!
//! The command form of `kgf-verbalize`: a bundle directory and a config in,
//! two JSON-lines streams out — every root with the digest of its text, and
//! every distinct text under that digest — ready for the embedding stage. It
//! exists for two callers. The build pipeline will run it as a stage; until
//! then, and for anyone with a bundle on disk, it is the way to run a config
//! end to end and read what it produces.
//!
//! Both streams are written as the walk goes, so a run holds the set of
//! digests seen and nothing else that grows with the output.
//!
//! The bundle is opened the way `kgf manifest` opens one: this crate asserts
//! the publication invariant for a directory the operator named, holds the
//! mappings for the run, and writes nothing into it — an output inside the
//! bundle is refused before anything is mapped.

use std::collections::BTreeMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use kgf_store::manifest::default_predicate_roles;
use kgf_store::{Manifest, OpenOptions, Store};
use kgf_verbalize::{Bound, Config, Rendered, RootRecord, Seen, TextRecord, Verbalizer};

/// `kgf verbalize` arguments.
#[derive(Debug, Parser)]
pub struct Args {
    /// A published bundle version directory.
    pub bundle: PathBuf,

    /// The verbalization config, YAML or JSON. `-` reads standard input.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,

    /// Where to write the roots: one JSON object per root, with the digest
    /// of its text.
    #[arg(long, value_name = "FILE")]
    pub roots: PathBuf,

    /// Where to write the texts: one JSON object per distinct text, under
    /// its digest. What the embedding stage reads.
    #[arg(long, value_name = "FILE")]
    pub texts: PathBuf,

    /// Only these targets, by their config names. Repeatable; default all.
    #[arg(long, value_name = "NAME")]
    pub target: Vec<String>,

    /// Stop after this many roots per target.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Verbalize only these roots, under the one `--target` given.
    #[arg(long, value_name = "IRI", requires = "target")]
    pub iri: Vec<String>,

    /// Write the texts as a readable dump instead of JSON lines.
    #[arg(long)]
    pub text: bool,
}

/// Run the command.
pub fn run(args: Args) -> Result<()> {
    let config = read_config(&args.config)?;
    let resolved = config
        .resolve()
        .context("resolving the verbalization config")?;

    refuse_output_inside(&args.bundle, &args.roots)?;
    refuse_output_inside(&args.bundle, &args.texts)?;
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
    let mut out = Streams::create(&args.roots, &args.texts, args.text)?;
    let started = Instant::now();

    if args.iri.is_empty() {
        for (index, name) in &targets {
            let roots = verbalizer.roots(*index)?;
            let total = args.limit.map_or(roots.len(), |limit| {
                u64::try_from(limit).unwrap_or(u64::MAX).min(roots.len())
            });
            let target_started = Instant::now();
            // Roots with a predicate sampled down to the limit: the signal
            // for tuning the config, reported here rather than carried on
            // the records, which no later stage would read it from.
            let mut limited = 0u64;
            for root in roots.take(usize::try_from(total).unwrap_or(usize::MAX)) {
                if let Some(rendered) = verbalizer.verbalize(*index, root)? {
                    limited += u64::from(rendered.limited > 0);
                    out.write(&rendered, name)?;
                }
            }
            eprintln!(
                "{name}: {total} roots in {:.1?} ({limited} with a predicate sampled down; \
                 {} distinct texts so far)",
                target_started.elapsed(),
                out.seen.len()
            );
        }
    } else {
        let [(index, name)] = targets.as_slice() else {
            bail!("--iri needs exactly one --target");
        };
        for iri in &args.iri {
            match verbalizer.verbalize_iri(*index, iri)? {
                Some(rendered) => out.write(&rendered, name)?,
                None => eprintln!("warning: {iri} is not a subject in this bundle ({name})"),
            }
        }
    }

    let (roots, texts) = out.finish()?;
    eprintln!(
        "wrote {roots} roots to {} and {texts} texts to {} in {:.1?}",
        args.roots.display(),
        args.texts.display(),
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
    let mut selected: Vec<(usize, String)> = Vec::with_capacity(names.len());
    for name in names {
        if selected.iter().any(|(_, chosen)| chosen == name) {
            bail!("--target {name:?} is given twice");
        }
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

/// The two output streams, written as roots are rendered.
struct Streams {
    roots: BufWriter<std::fs::File>,
    texts: BufWriter<std::fs::File>,
    roots_path: PathBuf,
    texts_path: PathBuf,
    /// Texts as a readable dump rather than JSON lines.
    readable: bool,
    seen: Seen,
    written: u64,
}

impl Streams {
    fn create(roots: &Path, texts: &Path, readable: bool) -> Result<Self> {
        let open = |path: &Path| {
            std::fs::File::create(path)
                .map(BufWriter::new)
                .with_context(|| format!("creating the output {}", path.display()))
        };
        Ok(Self {
            roots: open(roots)?,
            texts: open(texts)?,
            roots_path: roots.to_owned(),
            texts_path: texts.to_owned(),
            readable,
            seen: Seen::new(),
            written: 0,
        })
    }

    /// One root's line, and its text's line if the text is new.
    fn write(&mut self, rendered: &Rendered, target: &str) -> Result<()> {
        serde_json::to_writer(&mut self.roots, &RootRecord::new(rendered, target))?;
        self.roots.write_all(b"\n")?;
        self.written += 1;
        if self.seen.first(rendered.digest) {
            if self.readable {
                writeln!(
                    self.texts,
                    "digest: {}",
                    kgf_verbalize::text::hex(&rendered.digest)
                )?;
                writeln!(self.texts)?;
                writeln!(self.texts, "{}", rendered.text)?;
                writeln!(self.texts, "\n---\n")?;
            } else {
                serde_json::to_writer(&mut self.texts, &TextRecord::new(rendered))?;
                self.texts.write_all(b"\n")?;
            }
        }
        Ok(())
    }

    /// Flush both; how many roots and how many texts were written.
    fn finish(mut self) -> Result<(u64, usize)> {
        self.roots
            .flush()
            .with_context(|| format!("writing the output {}", self.roots_path.display()))?;
        self.texts
            .flush()
            .with_context(|| format!("writing the output {}", self.texts_path.display()))?;
        Ok((self.written, self.seen.len()))
    }
}

/// A bundle opened for the run.
struct Opened {
    store: Store,
}

/// Refuse an output under the bundle directory.
///
/// The bundle is about to be mapped on the promise that nothing writes into
/// it; the files this command writes must therefore lie elsewhere. Both
/// paths are canonicalized so that a symlink or a `..` cannot slip the output
/// inside. The output's directory must already exist for its path to have a
/// canonical form, which `File::create` would require anyway.
fn refuse_output_inside(bundle: &Path, output: &Path) -> Result<()> {
    let bundle = bundle
        .canonicalize()
        .with_context(|| format!("resolving the bundle {}", bundle.display()))?;
    let parent = match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let parent = parent
        .canonicalize()
        .with_context(|| format!("resolving the output directory {}", parent.display()))?;
    let Some(name) = output.file_name() else {
        bail!("{} names no file", output.display());
    };
    ensure!(
        !parent.join(name).starts_with(&bundle),
        "{} is inside the bundle {}, whose files are published and never written",
        output.display(),
        bundle.display()
    );
    Ok(())
}

/// Open the bundle the operator named.
///
/// # Safety obligation
///
/// [`PublishedBundle::new`](kgf_store::PublishedBundle::new) requires that the
/// artifacts not be modified or truncated while mapped. This command holds the
/// mappings for one run over a directory the operator named as published, and
/// the only files it writes are `--roots` and `--texts`, which
/// [`refuse_output_inside`] has already placed outside the bundle. What has to hold, as for `kgf manifest`,
/// is that nothing *else* rewrites a published version while it runs.
#[allow(unsafe_code)]
fn open_bundle(dir: &Path) -> Result<Opened> {
    ensure!(dir.is_dir(), "{} is not a bundle directory", dir.display());
    let bundle = unsafe { kgf_store::PublishedBundle::new(dir) };
    let store = Store::open(&bundle, OpenOptions::default())
        .with_context(|| format!("opening the bundle {}", dir.display()))?;
    Ok(Opened { store })
}
