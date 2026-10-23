//! `kgf vectors`: assemble a bundle's `vectors/` sidecar from the embedding
//! stage's output.
//!
//! The embedding stage (`okn-indexing embed-texts`, outside this workspace)
//! takes the texts `kgf verbalize --texts` wrote and returns one Parquet row
//! per text, in the same order: the text's digest and its vector, with the
//! model's identity in the footer. This command joins that file to the roots
//! `kgf verbalize --roots` wrote — each root's IRI, target and text digest —
//! through the bundle's dictionary, and writes the five files a server and a
//! client need: the ANN index with row `i` under key `i`, the id map in
//! subject ids, the Parquet itself (linked where the file system allows),
//! `roots.parquet`, entity IRI → digest for a client holding the vectors
//! without the bundle, and `verbalize.json`, the config the texts were
//! rendered under, in canonical JSON — the recipe, published and
//! digest-covered beside what it produced, so a server can render an
//! entity's text live and a client can embed a query the same way.
//!
//! Everything is written into a scratch directory beside `vectors/` and
//! renamed into place at the end, so the sidecar exists whole or not at all.
//! The Parquet is hard-linked rather than copied where the file system
//! allows, so it inherits the promise `kgf build --adopt` already relies on:
//! the operator does not rewrite an input after handing it to a bundle.
//! An existing `vectors/` is never replaced: a sidecar is part of a version's
//! identity, and a different one is a different version. The manifest is not
//! rewritten here — `kgf manifest` derives the `vectors` capability from the
//! files, as it does every other, and the command says so when it is done.
//!
//! Memory: every digest with its row (40 bytes a record) and every root's id
//! grouped by record (8 bytes an entity, plus a vector per record) are held
//! until the id map's counts are known. The index is built in memory and
//! saved; it is the one structure here proportional to the vectors' bytes.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail, ensure};
use arrow::array::{Array, AsArray, FixedSizeBinaryArray, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Float32Type, Schema};
use clap::Parser;
use kgf_store::vectors::{IdMapIdentity, IdMapRow, write_id_map};
use kgf_store::{OpenOptions, Role, Store, TermId};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;
use sha2::{Digest, Sha256};

use kgf_store::store::artifact;

/// The base name of one of the sidecar's artifacts, which the store's
/// vocabulary spells with its `vectors/` directory.
fn file_name(artifact: &'static str) -> &'static str {
    Path::new(artifact)
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a sidecar artifact has a file name")
}

/// The sidecar directory under a bundle, from the same vocabulary.
fn sidecar_dir(bundle: &Path) -> PathBuf {
    bundle.join(
        Path::new(artifact::VECTORS_INDEX)
            .parent()
            .expect("a sidecar artifact is in a directory"),
    )
}

/// The footer keys the embedding stage writes, with this prefix.
const FOOTER_PREFIX: &str = "okn-embeddings.";

/// The one layout of the embedding stage's file this command reads: one row
/// per distinct text, `text_digest` and `vector`, in the texts stream's order.
const TEXTS_FORMAT: &str = "2";

/// Rows per batch when reading the vectors and writing the roots table.
const BATCH_ROWS: usize = 8192;

/// `kgf vectors`.
#[derive(Debug, Parser)]
pub struct Args {
    /// The bundle directory: an unpublished version being assembled, holding
    /// `data.hdt`, `data.hdt.perm` and a manifest, and no `vectors/` yet.
    pub bundle: PathBuf,

    /// The embedding stage's Parquet: one row per text, `text_digest` and
    /// `vector`, in the order `kgf verbalize --texts` wrote the texts.
    #[arg(long)]
    pub embeddings: PathBuf,

    /// The roots `kgf verbalize --roots` wrote: one JSON object per root with
    /// its IRI, target and text digest.
    #[arg(long)]
    pub roots: PathBuf,

    /// The verbalization config the roots and texts were written under,
    /// YAML or JSON. Published in the sidecar in canonical JSON.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,
}

/// What was assembled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assembled {
    /// Records: distinct texts, index keys.
    pub records: u64,
    /// Entities: roots joined to a record.
    pub entities: u64,
    /// The most entities one record names.
    pub max_entities_per_record: u64,
    /// Vector width.
    pub dimensions: u32,
}

/// Run the command.
pub fn run(args: Args) -> Result<()> {
    let assembled = assemble(&args.bundle, &args.embeddings, &args.roots, &args.config)?;
    println!(
        "wrote {} with {} records over {} entities ({} dimensions, at most {} entities a record)",
        sidecar_dir(&args.bundle).display(),
        assembled.records,
        assembled.entities,
        assembled.dimensions,
        assembled.max_entities_per_record
    );
    println!("declare it with `kgf manifest {}`", args.bundle.display());
    Ok(())
}

/// Assemble `bundle/vectors/` from `embeddings`, `roots` and `config`.
pub fn assemble(
    bundle: &Path,
    embeddings: &Path,
    roots: &Path,
    config: &Path,
) -> Result<Assembled> {
    let target = sidecar_dir(bundle);
    let verbalize = read_config(config)?;
    let resolved = verbalize
        .resolve()
        .with_context(|| format!("{} is not a usable verbalization config", config.display()))?;
    let target_names: Vec<&str> = resolved
        .targets
        .iter()
        .map(|target| target.name.as_str())
        .collect();
    ensure!(
        !target.exists(),
        "{} already exists; a version's sidecar is never replaced, so assemble a new version",
        target.display()
    );
    let store = open_bundle(bundle)?;

    let scratch = tempfile::Builder::new()
        .prefix(".vectors-")
        .tempdir_in(bundle)
        .with_context(|| format!("creating a scratch directory under {}", bundle.display()))?;

    let read = read_embeddings(
        embeddings,
        &scratch.path().join(file_name(artifact::VECTORS_INDEX)),
    )?;
    let joined = join_roots(
        &store,
        roots,
        &read,
        &target_names,
        &scratch.path().join(file_name(artifact::VECTORS_ROOTS)),
    )?;

    // Canonical: keys sorted, as the config type's maps are, and the field
    // order fixed by the type, so the same config always has the same bytes
    // and the artifact's checksum is the recipe's hash.
    let mut canonical = serde_json::to_vec_pretty(&verbalize).context("serializing the config")?;
    canonical.push(b'\n');
    let config_path = scratch.path().join(file_name(artifact::VECTORS_CONFIG));
    std::fs::write(&config_path, canonical)
        .with_context(|| format!("writing {}", config_path.display()))?;

    let identity = IdMapIdentity {
        dimensions: read.dimensions,
        hdt_identity: store.hdt_identity_digest(),
        convention_id: read.convention_id,
        parent_digest: file_digest(embeddings)?,
    };
    let id_map = scratch.path().join(file_name(artifact::VECTORS_ENTITIES));
    let mut out = BufWriter::new(
        File::create(&id_map).with_context(|| format!("creating {}", id_map.display()))?,
    );
    let summary = write_id_map(&mut out, identity, joined)
        .with_context(|| format!("writing {}", id_map.display()))?;
    out.flush()
        .with_context(|| format!("writing {}", id_map.display()))?;

    link_or_copy(
        embeddings,
        &scratch.path().join(file_name(artifact::VECTORS_EMBEDDINGS)),
    )?;

    drop(store);
    let scratch = scratch.keep();
    std::fs::rename(&scratch, &target)
        .with_context(|| format!("publishing {} as {}", scratch.display(), target.display()))?;

    Ok(Assembled {
        records: summary.records,
        entities: summary.entities,
        max_entities_per_record: summary.max_entities_per_record,
        dimensions: read.dimensions,
    })
}

/// Open the bundle the operator named.
///
/// # Safety obligation
///
/// [`PublishedBundle::new`](kgf_store::PublishedBundle::new) requires that the
/// artifacts not be modified or truncated while mapped. This command holds
/// the mappings for one run over a directory the operator named, and writes
/// only new files under a scratch directory of its own beside them, renamed
/// to `vectors/` at the end — a name [`assemble`] has just found absent.
/// Nothing it maps is written. What has to hold, as for `kgf manifest`, is
/// that nothing *else* rewrites the directory while it runs.
#[allow(unsafe_code)]
fn open_bundle(dir: &Path) -> Result<Store> {
    ensure!(dir.is_dir(), "{} is not a bundle directory", dir.display());
    let bundle = unsafe { kgf_store::PublishedBundle::new(dir) };
    Store::open(&bundle, OpenOptions::default().with_search_contexts(1))
        .with_context(|| format!("opening the bundle {}", dir.display()))
}

/// The embedding stage's file, read: its vectors in the index, its digests
/// by row, and what its footer says about the space.
struct ReadEmbeddings {
    /// Every digest with its row, sorted by digest.
    by_digest: Vec<([u8; 32], u64)>,
    /// Row → its position in `by_digest`.
    position_of_row: Vec<u64>,
    records: u64,
    dimensions: u32,
    convention_id: [u8; 32],
}

impl ReadEmbeddings {
    fn row_of(&self, digest: &[u8; 32]) -> Option<u64> {
        self.by_digest
            .binary_search_by(|(d, _)| d.cmp(digest))
            .ok()
            .map(|i| self.by_digest[i].1)
    }

    fn digest_of(&self, row: u64) -> [u8; 32] {
        self.by_digest[self.position_of_row[row as usize] as usize].0
    }
}

/// Read `path`, build the index at `index_path`, and keep the digests.
fn read_embeddings(path: &Path, index_path: &Path) -> Result<ReadEmbeddings> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .with_context(|| format!("reading {} as Parquet", path.display()))?;

    let footer = Footer::of(path, builder.metadata().file_metadata());
    let footer_field = |name: &str| footer.field(name);
    let format = footer_field("format")?;
    ensure!(
        format == TEXTS_FORMAT,
        "{} is layout format {format}; this build reads the embed-texts layout {TEXTS_FORMAT}",
        path.display()
    );
    let metric = footer_field("metric")?;
    ensure!(
        metric == "cosine",
        "{} holds vectors under the {metric} metric; the index is built for cosine",
        path.display()
    );
    let convention_id = parse_digest(footer_field("convention_id")?)
        .with_context(|| format!("{}'s convention_id", path.display()))?;
    let dimensions: u32 = footer_field("dim")?
        .parse()
        .with_context(|| format!("{}'s dim", path.display()))?;
    let records: u64 = footer_field("record_count")?
        .parse()
        .with_context(|| format!("{}'s record_count", path.display()))?;
    ensure!(
        builder.metadata().file_metadata().num_rows() as u64 == records,
        "{} has {} rows but its footer says {records}",
        path.display(),
        builder.metadata().file_metadata().num_rows()
    );

    let mask = parquet::arrow::ProjectionMask::columns(
        builder.parquet_schema(),
        ["text_digest", "vector"],
    );
    let reader = builder
        .with_projection(mask)
        .with_batch_size(BATCH_ROWS)
        .build()
        .with_context(|| format!("reading {}", path.display()))?;

    let index = usearch::Index::new(&usearch::IndexOptions {
        dimensions: dimensions as usize,
        metric: usearch::MetricKind::Cos,
        quantization: usearch::ScalarKind::F16,
        ..Default::default()
    })
    .map_err(|e| anyhow!("creating the index: {e}"))?;
    index
        .reserve(records as usize)
        .map_err(|e| anyhow!("reserving {records} vectors: {e}"))?;

    let mut by_digest: Vec<([u8; 32], u64)> = Vec::with_capacity(records as usize);
    let mut row = 0u64;
    for batch in reader {
        let batch = batch.with_context(|| format!("reading {}", path.display()))?;
        let digests = batch
            .column_by_name("text_digest")
            .and_then(|c| c.as_any().downcast_ref::<FixedSizeBinaryArray>())
            .filter(|c| c.value_length() == 32)
            .ok_or_else(|| {
                anyhow!(
                    "{}'s text_digest column is not 32 fixed bytes",
                    path.display()
                )
            })?;
        let vectors = batch
            .column_by_name("vector")
            .and_then(|c| {
                c.as_any()
                    .downcast_ref::<arrow::array::FixedSizeListArray>()
            })
            .filter(|c| c.value_length() == dimensions as i32)
            .ok_or_else(|| {
                anyhow!(
                    "{}'s vector column is not a fixed list of {dimensions} floats",
                    path.display()
                )
            })?;
        for i in 0..batch.num_rows() {
            let digest: [u8; 32] = digests.value(i).try_into().expect("32 bytes");
            let vector = vectors.value(i);
            let floats = vector.as_primitive::<Float32Type>();
            ensure!(
                floats.null_count() == 0 && floats.len() == dimensions as usize,
                "{} row {row}: vector is not {dimensions} finite floats",
                path.display()
            );
            index
                .add(row, floats.values())
                .map_err(|e| anyhow!("adding row {row} to the index: {e}"))?;
            by_digest.push((digest, row));
            row += 1;
        }
    }
    ensure!(
        row == records,
        "{} yielded {row} rows of the {records} its footer records",
        path.display()
    );
    index
        .save(
            index_path
                .to_str()
                .ok_or_else(|| anyhow!("index path is not UTF-8"))?,
        )
        .map_err(|e| anyhow!("saving the index to {}: {e}", index_path.display()))?;

    by_digest.sort_unstable();
    if let Some(pair) = by_digest.windows(2).find(|w| w[0].0 == w[1].0) {
        bail!(
            "{} holds digest {} twice, at rows {} and {}; every text is embedded once",
            path.display(),
            hex(&pair[0].0),
            pair[0].1,
            pair[1].1
        );
    }

    let mut position_of_row = vec![0u64; records as usize];
    for (position, (_, row)) in by_digest.iter().enumerate() {
        position_of_row[*row as usize] = position as u64;
    }

    Ok(ReadEmbeddings {
        by_digest,
        position_of_row,
        records,
        dimensions,
        convention_id,
    })
}

/// One line of the roots stream.
#[derive(Debug, serde::Deserialize)]
struct RootLine {
    iri: String,
    target: String,
    digest: String,
}

/// Resolve every root to a subject id and a record, write `roots.parquet`,
/// and return the id-map rows in record order. A root under a target the
/// config does not declare was verbalized under another config.
fn join_roots(
    store: &Store,
    path: &Path,
    read: &ReadEmbeddings,
    targets: &[&str],
    roots_parquet: &Path,
) -> Result<Vec<IdMapRow>> {
    let reader =
        BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    let dictionary = store.dict();

    let schema = Arc::new(Schema::new(vec![
        Field::new("iri", DataType::Utf8, false),
        Field::new("target", DataType::Utf8, false),
        Field::new("text_digest", DataType::FixedSizeBinary(32), false),
    ]));
    let properties = WriterProperties::builder()
        .set_key_value_metadata(Some(vec![
            KeyValue::new("kgf.roots_format".to_owned(), "1".to_owned()),
            KeyValue::new("kgf.convention_id".to_owned(), hex(&read.convention_id)),
        ]))
        .build();
    let out = File::create(roots_parquet)
        .with_context(|| format!("creating {}", roots_parquet.display()))?;
    let mut writer = ArrowWriter::try_new(out, schema.clone(), Some(properties))
        .with_context(|| format!("writing {}", roots_parquet.display()))?;

    let mut per_record: Vec<Vec<TermId>> = vec![Vec::new(); read.records as usize];
    let mut batch = RootsBatch::default();
    for (number, line) in reader.lines().enumerate() {
        let number = number + 1;
        let line = line.with_context(|| format!("reading {}:{number}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let root: RootLine = serde_json::from_str(&line)
            .with_context(|| format!("{}:{number} is not a roots line", path.display()))?;
        if !targets.contains(&root.target.as_str()) {
            bail!(
                "{}:{number}: root {} is under target {:?}, which the config does not declare ({}); the roots were verbalized under a different config",
                path.display(),
                root.iri,
                root.target,
                targets.join(", ")
            );
        }
        let digest = parse_digest(&root.digest)
            .with_context(|| format!("{}:{number}'s digest", path.display()))?;
        let row = read.row_of(&digest).ok_or_else(|| {
            anyhow!(
                "{}:{number}: root {} has text {}, which the embeddings do not hold; \
                 the roots and the embeddings come from different verbalizer runs",
                path.display(),
                root.iri,
                root.digest
            )
        })?;
        let id = dictionary
            .locate(Role::Subject, root.iri.as_bytes())
            .with_context(|| format!("looking up {}", root.iri))?
            .ok_or_else(|| {
                anyhow!(
                    "{}:{number}: root {} is not a subject of this bundle; \
                     the roots were verbalized from a different HDT",
                    path.display(),
                    root.iri
                )
            })?;
        per_record[row as usize].push(id);
        batch.push(&root, digest);
        if batch.len() == BATCH_ROWS {
            batch.write(&mut writer, &schema, roots_parquet)?;
        }
    }
    batch.write(&mut writer, &schema, roots_parquet)?;
    writer
        .close()
        .with_context(|| format!("finishing {}", roots_parquet.display()))?;

    let mut rows = Vec::with_capacity(per_record.len());
    for (record, entities) in per_record.into_iter().enumerate() {
        let digest = read.digest_of(record as u64);
        if entities.is_empty() {
            bail!(
                "record {record} (text {}) is named by no root in {}; \
                 the roots and the embeddings come from different verbalizer runs",
                hex(&digest),
                path.display()
            );
        }
        rows.push(IdMapRow { digest, entities });
    }
    Ok(rows)
}

/// Roots not yet written to the table.
#[derive(Default)]
struct RootsBatch {
    iris: Vec<String>,
    targets: Vec<String>,
    digests: Vec<[u8; 32]>,
}

impl RootsBatch {
    fn push(&mut self, root: &RootLine, digest: [u8; 32]) {
        self.iris.push(root.iri.clone());
        self.targets.push(root.target.clone());
        self.digests.push(digest);
    }

    fn len(&self) -> usize {
        self.iris.len()
    }

    fn write(
        &mut self,
        writer: &mut ArrowWriter<File>,
        schema: &Arc<Schema>,
        path: &Path,
    ) -> Result<()> {
        if self.iris.is_empty() {
            return Ok(());
        }
        let digests = FixedSizeBinaryArray::try_from_iter(self.digests.iter())
            .context("building the digest column")?;
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(std::mem::take(&mut self.iris))),
                Arc::new(StringArray::from(std::mem::take(&mut self.targets))),
                Arc::new(digests),
            ],
        )
        .context("building a roots batch")?;
        self.digests.clear();
        writer
            .write(&batch)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// The embedding stage's footer keys, without their prefix.
struct Footer<'a> {
    path: &'a Path,
    fields: BTreeMap<String, String>,
}

impl<'a> Footer<'a> {
    fn of(path: &'a Path, metadata: &parquet::file::metadata::FileMetaData) -> Self {
        let fields = metadata
            .key_value_metadata()
            .into_iter()
            .flatten()
            .filter_map(|KeyValue { key, value }| {
                key.strip_prefix(FOOTER_PREFIX)
                    .map(|key| (key.to_owned(), value.clone().unwrap_or_default()))
            })
            .collect();
        Self { path, fields }
    }

    /// Read only the footer of the Parquet at `path`.
    fn read(path: &'a Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)
            .with_context(|| format!("reading {} as Parquet", path.display()))?;
        Ok(Self::of(path, builder.metadata().file_metadata()))
    }

    fn field(&self, name: &str) -> Result<&String> {
        self.fields.get(name).ok_or_else(|| {
            anyhow!(
                "{} carries no `{FOOTER_PREFIX}{name}` in its footer; \
                 it was not written by the embedding stage",
                self.path.display()
            )
        })
    }

    fn parsed<T: std::str::FromStr>(&self, name: &str) -> Result<T>
    where
        T::Err: std::error::Error + Send + Sync + 'static,
    {
        self.field(name)?
            .parse()
            .with_context(|| format!("{}'s {name}", self.path.display()))
    }
}

/// What a bundle's `vectors` capability declares, for `kgf manifest` to write
/// into the manifest: the sidecar's own facts, the space and the model from
/// the embedding stage's footer (read here once, so a server never parses the
/// Parquet), and the targets from the published config. `None` for a bundle
/// without the sidecar, and for one whose sidecar does not publish the
/// Parquet: the model is known only from its footer, and a sidecar is servable
/// without it. Without the config, the targets are empty.
///
/// The index's library version is the one this binary links. The sidecar is
/// written by `kgf vectors` and described by `kgf manifest`, which `--locked`
/// builds from one lockfile, so it is also the version that wrote the index.
pub fn describe(
    bundle: &kgf_store::PublishedBundle,
) -> Result<Option<kgf_store::VectorsDescription>> {
    let Some(facts) = kgf_store::vectors_sidecar_facts(bundle)
        .with_context(|| format!("opening the vector sidecar in {}", bundle.path().display()))?
    else {
        return Ok(None);
    };
    let parquet = bundle.path().join(artifact::VECTORS_EMBEDDINGS);
    if !parquet.is_file() {
        return Ok(None);
    }
    let footer = Footer::read(&parquet)?;
    let convention_id = footer.field("convention_id")?;
    ensure!(
        parse_digest(convention_id)? == facts.convention_id,
        "{} names a different vector space than the id map beside it; \
         assemble the sidecar again with `kgf vectors`",
        parquet.display()
    );
    let dimensions: u32 = footer.parsed("dim")?;
    ensure!(
        dimensions == facts.dimensions,
        "{} holds {dimensions}-dimensional vectors but the id map says {}",
        parquet.display(),
        facts.dimensions
    );
    let config_path = bundle.path().join(artifact::VECTORS_CONFIG);
    let targets = if config_path.is_file() {
        read_config(&config_path)?
            .targets
            .into_iter()
            .map(|(name, target)| (name, target.class))
            .collect()
    } else {
        BTreeMap::new()
    };
    Ok(Some(kgf_store::VectorsDescription {
        model: footer.field("model")?.clone(),
        convention_id: convention_id.clone(),
        dimensions,
        metric: footer.field("metric")?.clone(),
        normalized: footer.parsed("normalized")?,
        max_seq_length: footer.parsed("max_seq_length")?,
        entities: facts.entities,
        records: facts.records,
        truncated_records: footer.parsed("truncated_records")?,
        targets,
        index: kgf_store::VectorsIndexDescription {
            library: "usearch".to_owned(),
            library_version: usearch::version().to_owned(),
            kind: "hnsw".to_owned(),
            quantization: "f16".to_owned(),
            connectivity: facts.connectivity,
            expansion_search: facts.expansion_search,
        },
    }))
}

/// The config as given, YAML or JSON.
fn read_config(path: &Path) -> Result<kgf_verbalize::Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the config {}", path.display()))?;
    serde_norway::from_str(&text).with_context(|| format!("parsing the config {}", path.display()))
}

/// Hard-link `from` at `to`, or copy it where linking is refused.
fn link_or_copy(from: &Path, to: &Path) -> Result<()> {
    if std::fs::hard_link(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)
        .map(|_| ())
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))
}

/// The SHA-256 of a whole file.
fn file_digest(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).with_context(|| format!("hashing {}", path.display()))?;
    Ok(hasher.finalize().into())
}

/// Sixty-four hex characters as thirty-two bytes.
fn parse_digest(hex: &str) -> Result<[u8; 32]> {
    ensure!(
        hex.is_ascii() && hex.len() == 64,
        "{hex:?} is not 64 hex digits"
    );
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .with_context(|| format!("{hex:?} is not hex"))?;
    }
    Ok(out)
}

fn hex(digest: &[u8; 32]) -> String {
    kgf_verbalize::text::hex(digest)
}
