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
//! mappings for the run, and writes nothing into it — an output that is, by
//! any name, a file of the bundle is refused before anything is mapped.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use kgf_store::manifest::default_predicate_roles;
use kgf_store::{Manifest, OpenOptions, Store};
use kgf_verbalize::{Bound, Config, Rendered, RootRecord, Seen, TextRecord, Verbalizer};
use same_file::Handle;

/// `kgf verbalize` arguments.
#[derive(Debug, Parser)]
pub struct Args {
    /// A published bundle version directory.
    pub bundle: PathBuf,

    /// The verbalization config, YAML or JSON. `-` reads standard input.
    /// Omitted, the bundle's own (`vectors/verbalize.json`) is used, as
    /// `/verbalize` uses it.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

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

    /// Stop after this many roots per target. A member an earlier target
    /// claims is not one of this target's roots and does not count.
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
    let config = match &args.config {
        Some(path) => read_config(path)?,
        None => {
            let published = args.bundle.join(kgf_store::store::artifact::VECTORS_CONFIG);
            ensure!(
                published.is_file(),
                "no --config, and {} publishes no verbalization config to fall back on",
                args.bundle.display()
            );
            read_config(&published)?
        }
    };
    let resolved = config
        .resolve()
        .context("resolving the verbalization config")?;

    let outputs = Outputs::open(&args.bundle, &args.roots, &args.texts)?;
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
    let iri_target = if args.iri.is_empty() {
        None
    } else {
        let [(index, name)] = targets.as_slice() else {
            bail!("--iri needs exactly one --target");
        };
        Some((*index, name.as_str()))
    };
    let mut verbalizer = Verbalizer::new(&opened.store, &bound);
    // Every check has passed; only now are the outputs emptied, so a run
    // refused for a misspelled target or an unreadable bundle leaves the last
    // run's files as they were.
    let mut out = Streams::start(outputs, args.text)?;
    let started = Instant::now();

    if let Some((index, name)) = iri_target {
        for iri in &args.iri {
            match verbalizer.verbalize_iri(index, iri)? {
                Some(rendered) => out.write(&rendered, name)?,
                None => eprintln!("warning: {iri} is not a subject in this bundle ({name})"),
            }
        }
    } else {
        for (index, name) in &targets {
            let mut roots = verbalizer.roots(*index)?;
            let limit = args.limit.unwrap_or(usize::MAX);
            let target_started = Instant::now();
            // Roots with a predicate sampled down to the limit: the signal
            // for tuning the config, reported here rather than carried on
            // the records, which no later stage would read it from.
            let mut limited = 0u64;
            let mut total = 0u64;
            for root in roots.by_ref().take(limit) {
                total += 1;
                if let Some(rendered) = verbalizer.verbalize(*index, root?)? {
                    limited += u64::from(rendered.limited > 0);
                    out.write(&rendered, name)?;
                }
            }
            eprintln!(
                "{name}: {total} roots in {:.1?} ({limited} with a predicate sampled down; \
                 {} members left to an earlier target; {} distinct texts so far)",
                target_started.elapsed(),
                roots.claimed_before(),
                out.seen.len()
            );
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
    /// Empty both outputs and start writing them: the only way to write to
    /// one, and the first thing that changes it.
    fn start(outputs: Outputs, readable: bool) -> Result<Self> {
        for (file, path) in [
            (&outputs.roots, &outputs.roots_path),
            (&outputs.texts, &outputs.texts_path),
        ] {
            file.set_len(0)
                .with_context(|| format!("emptying the output {}", path.display()))?;
        }
        Ok(Self {
            roots: BufWriter::new(outputs.roots),
            texts: BufWriter::new(outputs.texts),
            roots_path: outputs.roots_path,
            texts_path: outputs.texts_path,
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

/// The two output files, open and checked against the bundle, and not yet
/// changed: [`Streams::start`] empties them once the run's other checks pass.
struct Outputs {
    roots: File,
    texts: File,
    roots_path: PathBuf,
    texts_path: PathBuf,
}

impl Outputs {
    /// Open both outputs, refusing either one that is a file of the bundle by
    /// any name. Neither is changed: an output that does not exist is created
    /// empty, and one that does keeps its contents until the run starts.
    ///
    /// The bundle is about to be mapped on the promise that nothing writes
    /// into it, and truncating a mapped artifact is not an error but a crash,
    /// in this process and in any server mapping the same version. A path
    /// check cannot keep that promise on its own: a symlink or a hard link
    /// outside the bundle can name an artifact inside it. So each output's
    /// path is resolved with its links followed and refused under the bundle,
    /// and the file it opens — opened without truncating — is compared by
    /// identity with every file the bundle holds before anything is written
    /// to it. The two outputs must also be two files, or each would write
    /// over the other.
    fn open(bundle: &Path, roots: &Path, texts: &Path) -> Result<Self> {
        let bundle_dir = bundle
            .canonicalize()
            .with_context(|| format!("resolving the bundle {}", bundle.display()))?;
        let published = published_files(&bundle_dir)?;
        let roots_file = open_output(&bundle_dir, &published, roots)?;
        let texts_file = open_output(&bundle_dir, &published, texts)?;
        ensure!(
            identity(&roots_file, roots)? != identity(&texts_file, texts)?,
            "--roots {} and --texts {} are the same file, which each would write over",
            roots.display(),
            texts.display()
        );
        Ok(Self {
            roots: roots_file,
            texts: texts_file,
            roots_path: roots.to_owned(),
            texts_path: texts.to_owned(),
        })
    }
}

/// Open one output for writing, without truncating it, unless it is or would
/// be a file of the bundle.
fn open_output(bundle: &Path, published: &[(PathBuf, Handle)], output: &Path) -> Result<File> {
    ensure!(
        !resolved_output(output)?.starts_with(bundle),
        "{} is inside the bundle {}, whose files are published and never written",
        output.display(),
        bundle.display()
    );
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(output)
        .with_context(|| format!("creating the output {}", output.display()))?;
    let handle = identity(&file, output)?;
    if let Some((artifact, _)) = published.iter().find(|(_, file)| *file == handle) {
        bail!(
            "{} is the bundle's {} under another name, and the bundle's files are published \
             and never written",
            output.display(),
            artifact.display()
        );
    }
    Ok(file)
}

/// Where an output path lands once its links are followed.
///
/// A path that exists — a link included, since creating a file through a link
/// writes where the link points — is resolved whole; a link that resolves
/// nowhere is refused, for the same reason. A new path is its directory's
/// canonical form plus its name, the directory having to exist for the file to
/// be created in it.
fn resolved_output(output: &Path) -> Result<PathBuf> {
    if output.symlink_metadata().is_ok() {
        return output
            .canonicalize()
            .with_context(|| format!("resolving the output {}", output.display()));
    }
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
    Ok(parent.join(name))
}

/// Every file under the bundle directory, links to files followed, with the
/// handle an output is compared against.
fn published_files(dir: &Path) -> Result<Vec<(PathBuf, Handle)>> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("listing {}", dir.display()))?;
        for entry in entries {
            let path = entry
                .with_context(|| format!("listing {}", dir.display()))?
                .path();
            let kind = path
                .symlink_metadata()
                .with_context(|| format!("reading {}", path.display()))?
                .file_type();
            if kind.is_dir() {
                pending.push(path);
            } else if path.is_file() {
                let handle = Handle::from_path(&path)
                    .with_context(|| format!("opening {}", path.display()))?;
                files.push((path, handle));
            }
        }
    }
    Ok(files)
}

/// The identity of an open file, to compare with another.
fn identity(file: &File, path: &Path) -> Result<Handle> {
    file.try_clone()
        .and_then(Handle::from_file)
        .with_context(|| format!("identifying the output {}", path.display()))
}

/// Open the bundle the operator named.
///
/// # Safety obligation
///
/// [`PublishedBundle::new`](kgf_store::PublishedBundle::new) requires that the
/// artifacts not be modified or truncated while mapped. This command holds the
/// mappings for one run over a directory the operator named as published, and
/// the only files it writes are `--roots` and `--texts`, which
/// [`Outputs::open`] has already opened and found to be no file of the
/// bundle's by any name. What has to hold, as for `kgf manifest`, is that
/// nothing *else* rewrites a published version while it runs.
#[allow(unsafe_code)]
fn open_bundle(dir: &Path) -> Result<Opened> {
    ensure!(dir.is_dir(), "{} is not a bundle directory", dir.display());
    let bundle = unsafe { kgf_store::PublishedBundle::new(dir) };
    let store = Store::open(&bundle, OpenOptions::default())
        .with_context(|| format!("opening the bundle {}", dir.display()))?;
    Ok(Opened { store })
}
