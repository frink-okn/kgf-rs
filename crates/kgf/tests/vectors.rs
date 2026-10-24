//! `kgf vectors` against an hdtc-built fixture: the verbalizer's streams in,
//! a Parquet written the way the embedding stage writes one, the sidecar
//! assembled, declared by `kgf manifest`, and opened through the catalog as
//! `kgf serve` would open it.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{FixedSizeBinaryArray, FixedSizeListArray, Float32Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use kgf_store::catalog::{BundleId, Catalog};
use kgf_store::manifest::Capability;
use kgf_store::testing::Fixture;
use kgf_store::vectors::Row;
use kgf_store::{OpenOptions, Role, Store};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

const DATASET: &str = "fx";
const VERSION: &str = "2026-10-22";
const DIM: usize = 8;
const CONVENTION: &str = "1111111111111111111111111111111111111111111111111111111111111111";

const T: &str = "http://example.org/T";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";

/// Four members of `T`: `b` and `c` read identically, so they share a record.
fn graph() -> String {
    let mut out = String::new();
    for (node, label) in [("a", "Alpha"), ("b", "Same"), ("c", "Same"), ("d", "Delta")] {
        out += &format!("<http://example.org/{node}> <{RDF_TYPE}> <{T}> .\n");
        out += &format!("<http://example.org/{node}> <{RDFS_LABEL}> \"{label}\" .\n");
    }
    out += &format!("<{T}> <{RDFS_LABEL}> \"a class\" .\n");
    out
}

/// A fixture bundle under a catalog root.
struct Published {
    root: tempfile::TempDir,
}

impl Published {
    fn build(graph: &str) -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let bundle = root.path().join(DATASET).join(VERSION);
        Fixture::build(graph).copy_bundle_to(&bundle);
        let published = Self { root };
        published.describe();
        published
    }

    fn bundle(&self) -> PathBuf {
        self.root.path().join(DATASET).join(VERSION)
    }

    fn beside(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    /// `kgf manifest`, as `kgf build` would write one.
    fn describe(&self) {
        self.try_describe().expect("describe the bundle");
    }

    /// `kgf manifest`, with its refusal.
    fn try_describe(&self) -> anyhow::Result<()> {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: kgf::manifest::Args,
        }
        let cli = <Cli as clap::Parser>::parse_from([
            "kgf-manifest",
            self.bundle().to_str().unwrap(),
            "--id",
            DATASET,
            "--version",
            VERSION,
        ]);
        kgf::manifest::run(cli.args)
    }

    /// `kgf manifest --check`.
    fn check(&self) -> anyhow::Result<()> {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: kgf::manifest::Args,
        }
        let cli = <Cli as clap::Parser>::parse_from([
            "kgf-manifest",
            self.bundle().to_str().unwrap(),
            "--check",
        ]);
        kgf::manifest::run(cli.args)
    }

    /// `kgf verbalize` over every member of `T`, to `roots` and `texts`.
    fn verbalize(&self, roots: &Path, texts: &Path) {
        self.verbalize_with(
            &format!(r#"{{"targets": {{"things": {{"type": "{T}"}}}}}}"#),
            roots,
            texts,
        );
    }

    /// `kgf verbalize` under `config` (JSON), to `roots` and `texts`.
    fn verbalize_with(&self, config_json: &str, roots: &Path, texts: &Path) {
        let config = self.beside("config.json");
        std::fs::write(&config, config_json).unwrap();
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: kgf::verbalize::Args,
        }
        let cli = <Cli as clap::Parser>::parse_from([
            "kgf-verbalize",
            self.bundle().to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--roots",
            roots.to_str().unwrap(),
            "--texts",
            texts.to_str().unwrap(),
        ]);
        kgf::verbalize::run(cli.args).expect("verbalize");
    }

    fn open(&self) -> Arc<Store> {
        let catalog = Catalog::scan(
            kgf::serve::published_root(self.root.path()).expect("a published root"),
            OpenOptions::default(),
        )
        .expect("scan the root");
        catalog
            .get(&BundleId {
                dataset: DATASET.to_owned(),
                version: VERSION.to_owned(),
            })
            .expect("open the bundle")
    }
}

/// The vector the stand-in embedding stage gives a text: from its digest, so
/// a test can recompute it.
fn vector_for(digest: &[u8; 32]) -> Vec<f32> {
    digest[..DIM]
        .iter()
        .map(|&b| f32::from(b) / 255.0 + 0.01)
        .collect()
}

fn hex_to_digest(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

/// Write `texts` (a `--texts` stream) as the embedding stage would: one row
/// per text, `text_digest` and `vector`, with the format-2 footer.
fn embed(texts: &Path, out: &Path, format: &str) -> Vec<[u8; 32]> {
    let digests: Vec<[u8; 32]> = BufReader::new(File::open(texts).unwrap())
        .lines()
        .map(|line| {
            let line: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
            hex_to_digest(line["digest"].as_str().unwrap())
        })
        .collect();
    write_parquet(out, &digests, format);
    digests
}

fn write_parquet(out: &Path, digests: &[[u8; 32]], format: &str) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("text_digest", DataType::FixedSizeBinary(32), false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, false)),
                DIM as i32,
            ),
            false,
        ),
    ]));
    let flat: Vec<f32> = digests.iter().flat_map(vector_for).collect();
    let vectors = FixedSizeListArray::new(
        Arc::new(Field::new("item", DataType::Float32, false)),
        DIM as i32,
        Arc::new(Float32Array::from(flat)),
        None,
    );
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(FixedSizeBinaryArray::try_from_iter(digests.iter()).unwrap()),
            Arc::new(vectors),
        ],
    )
    .unwrap();
    let footer = [
        ("format", format.to_owned()),
        ("graph", "fx".to_owned()),
        ("model", "test-model".to_owned()),
        ("dim", DIM.to_string()),
        ("metric", "cosine".to_owned()),
        ("normalized", "false".to_owned()),
        ("record_count", digests.len().to_string()),
        ("convention_id", CONVENTION.to_owned()),
        ("max_seq_length", "256".to_owned()),
        ("truncated_records", "1".to_owned()),
    ]
    .into_iter()
    .map(|(key, value)| KeyValue::new(format!("okn-embeddings.{key}"), value))
    .collect();
    let properties = WriterProperties::builder()
        .set_key_value_metadata(Some(footer))
        .build();
    let mut writer =
        ArrowWriter::try_new(File::create(out).unwrap(), schema, Some(properties)).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// (iri, digest) per roots line.
fn read_roots(path: &Path) -> Vec<(String, [u8; 32])> {
    BufReader::new(File::open(path).unwrap())
        .lines()
        .map(|line| {
            let line: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
            (
                line["iri"].as_str().unwrap().to_owned(),
                hex_to_digest(line["digest"].as_str().unwrap()),
            )
        })
        .collect()
}

#[test]
fn the_verbalizers_streams_become_a_sidecar_the_server_opens() {
    let published = Published::build(&graph());
    let (roots, texts) = (
        published.beside("roots.jsonl"),
        published.beside("texts.jsonl"),
    );
    published.verbalize(&roots, &texts);
    let embeddings = published.beside("embeddings.parquet");
    let digests = embed(&texts, &embeddings, "2");
    assert_eq!(digests.len(), 3, "four roots, two reading the same");

    let assembled = kgf::vectors::assemble(
        &published.bundle(),
        &embeddings,
        &roots,
        &published.beside("config.json"),
    )
    .unwrap();
    assert_eq!(
        (
            assembled.records,
            assembled.entities,
            assembled.max_entities_per_record
        ),
        (3, 4, 2)
    );
    for name in [
        "index.usearch",
        "entities.idmap",
        "embeddings.parquet",
        "roots.parquet",
        "verbalize.json",
    ] {
        assert!(
            published.bundle().join("vectors").join(name).is_file(),
            "{name}"
        );
    }
    // The config is published canonically: the same config, the same bytes.
    let stored: serde_json::Value = serde_json::from_slice(
        &std::fs::read(published.bundle().join("vectors/verbalize.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stored["targets"]["things"]["type"], T);

    published.describe();
    published.check().expect("the described bundle checks");
    let store = published.open();
    let vectors = store.vectors().expect("the sidecar opened");
    // Declared where a client reads it: the manifest kgf manifest wrote.
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(published.bundle().join("manifest.json")).unwrap())
            .unwrap();
    let declared = manifest["capabilities"].as_object().unwrap();
    assert!(
        declared.contains_key(Capability::Vectors.as_str()),
        "{declared:?}"
    );
    assert!(
        manifest["artifacts"]
            .get("vectors/verbalize.json")
            .is_some()
    );
    // The capability describes the space, the coverage and the index, from
    // the footer and the sidecar, so a reader needs neither.
    let description = &declared[Capability::Vectors.as_str()];
    assert_eq!(description["model"], "test-model", "{description}");
    assert_eq!(description["convention_id"], CONVENTION);
    assert_eq!(description["dimensions"], DIM);
    assert_eq!(description["max_seq_length"], 256);
    assert_eq!(description["truncated_records"], 1);
    assert_eq!(description["records"], vectors.records());
    assert_eq!(description["entities"], vectors.entities());
    assert_eq!(description["targets"]["things"], T);
    assert_eq!(description["index"]["kind"], "hnsw");
    assert_eq!(
        description["index"]["connectivity"],
        vectors.connectivity() as u64
    );

    // A live render under the stored config is the text that was embedded:
    // its digest is the one the id map holds for the root's record.
    let config: kgf_verbalize::Config = serde_json::from_value(stored).unwrap();
    let resolved = config.resolve().unwrap();
    let bound = kgf_verbalize::Bound::bind(&store, &resolved, &[RDFS_LABEL.to_owned()]).unwrap();
    let mut verbalizer = kgf_verbalize::Verbalizer::new(&store, &bound);
    let rendered = verbalizer
        .verbalize_iri(0, "http://example.org/a")
        .unwrap()
        .expect("a is a subject");
    let a = store
        .dict()
        .locate(Role::Subject, b"http://example.org/a")
        .unwrap()
        .unwrap();
    let row = vectors.row_of(a).unwrap().unwrap();
    assert_eq!(vectors.digest_of(row).unwrap(), rendered.digest);
    assert_eq!(bound.target_of(&store, a.0).unwrap(), Some(0));

    for (iri, digest) in read_roots(&roots) {
        let id = store
            .dict()
            .locate(Role::Subject, iri.as_bytes())
            .unwrap()
            .unwrap();
        let row = vectors.row_of(id).unwrap().expect("every root is embedded");
        assert_eq!(vectors.digest_of(row).unwrap(), digest, "{iri}");
        let stored = vectors.vector(row).unwrap();
        for (got, want) in stored.iter().zip(vector_for(&digest)) {
            assert!((got - want).abs() < 1e-3, "{iri}: {got} vs {want}");
        }
        let hits = vectors.similar(&stored, 1).unwrap();
        assert_eq!(hits[0].row, row, "{iri}");
        assert!(vectors.entities_of(row).unwrap().any(|e| e == id));
    }
    let shared = read_roots(&roots)
        .into_iter()
        .find(|(iri, _)| iri.ends_with("/b"))
        .map(|(_, d)| d)
        .unwrap();
    let row = (0..3)
        .map(Row)
        .find(|&r| vectors.digest_of(r).unwrap() == shared)
        .unwrap();
    assert_eq!(
        vectors.entities_of(row).unwrap().count(),
        2,
        "b and c share a record"
    );

    // roots.parquet: one row per root, joinable to the embeddings by digest.
    let table = ParquetRecordBatchReaderBuilder::try_new(
        File::open(published.bundle().join("vectors/roots.parquet")).unwrap(),
    )
    .unwrap()
    .build()
    .unwrap()
    .map(|b| b.unwrap())
    .collect::<Vec<_>>();
    let rows: usize = table.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 4);
    assert_eq!(
        table[0]
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().as_str())
            .collect::<Vec<_>>(),
        ["iri", "target", "text_digest"]
    );
}

/// A node that is a member of two targets' classes is embedded once, under
/// the first target in config order — the one `/describe` renders it by —
/// even when the two targets would render it differently.
#[test]
fn a_node_under_two_targets_is_embedded_once_under_the_first() {
    const U: &str = "http://example.org/U";
    const NOTE: &str = "http://example.org/note";
    let mut graph = graph();
    graph += &format!("<http://example.org/a> <{RDF_TYPE}> <{U}> .\n");
    graph += &format!("<http://example.org/a> <{NOTE}> \"only things say this\" .\n");
    graph += &format!("<http://example.org/e> <{RDF_TYPE}> <{U}> .\n");
    graph += &format!("<http://example.org/e> <{RDFS_LABEL}> \"Echo\" .\n");
    let published = Published::build(&graph);
    let (roots, texts) = (
        published.beside("roots.jsonl"),
        published.beside("texts.jsonl"),
    );
    // `units` ignores the note, so `a` reads differently under each target.
    published.verbalize_with(
        &format!(
            r#"{{"targets": {{
                "things": {{"type": "{T}"}},
                "units": {{"type": "{U}", "ignore_predicates": ["{NOTE}"]}}
            }}}}"#
        ),
        &roots,
        &texts,
    );
    let listed = read_roots(&roots);
    assert_eq!(
        listed
            .iter()
            .filter(|(iri, _)| iri == "http://example.org/a")
            .count(),
        1,
        "a is one root: {listed:?}"
    );
    let embeddings = published.beside("embeddings.parquet");
    let digests = embed(&texts, &embeddings, "2");

    // Roots from a verbalizer that rooted `a` under both targets, with a
    // second text, are refused naming the node and where it appears.
    let stale = published.beside("stale.jsonl");
    std::fs::copy(&roots, &stale).unwrap();
    let other = digests
        .iter()
        .find(|d| {
            !listed
                .iter()
                .any(|(iri, digest)| iri.ends_with("/a") && digest == *d)
        })
        .unwrap();
    writeln!(
        File::options().append(true).open(&stale).unwrap(),
        r#"{{"iri": "http://example.org/a", "target": "units", "digest": "{}"}}"#,
        kgf_verbalize::text::hex(other)
    )
    .unwrap();
    let error = kgf::vectors::assemble(
        &published.bundle(),
        &embeddings,
        &stale,
        &published.beside("config.json"),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("http://example.org/a is a root with two texts")
            && error.contains(r#"line 1 under "things""#)
            && error.contains(r#"under "units""#),
        "{error}"
    );
    assert!(!published.bundle().join("vectors").exists());

    let assembled = kgf::vectors::assemble(
        &published.bundle(),
        &embeddings,
        &roots,
        &published.beside("config.json"),
    )
    .expect("a node under two targets assembles");
    assert_eq!(assembled.entities, 5, "a, b, c, d and e");

    published.describe();
    let store = published.open();
    let vectors = store.vectors().expect("the sidecar opened");
    let stored: kgf_verbalize::Config = serde_json::from_slice(
        &std::fs::read(published.bundle().join("vectors/verbalize.json")).unwrap(),
    )
    .unwrap();
    let resolved = stored.resolve().unwrap();
    let bound = kgf_verbalize::Bound::bind(&store, &resolved, &[RDFS_LABEL.to_owned()]).unwrap();
    let a = store
        .dict()
        .locate(Role::Subject, b"http://example.org/a")
        .unwrap()
        .unwrap();
    let first = bound.target_of(&store, a.0).unwrap().expect("a is claimed");
    assert_eq!(bound.targets().nth(first), Some("things"));
    let rendered = kgf_verbalize::Verbalizer::new(&store, &bound)
        .verbalize(first, a.0)
        .unwrap()
        .expect("a is not blank");
    assert!(
        rendered.text.contains("only things say this"),
        "{}",
        rendered.text
    );
    let row = vectors.row_of(a).unwrap().unwrap();
    assert_eq!(vectors.digest_of(row).unwrap(), rendered.digest);
}

#[test]
fn kgf_manifest_refuses_a_sidecar_that_does_not_describe_its_bundle() {
    let published = Published::build(&graph());
    let (roots, texts) = (
        published.beside("roots.jsonl"),
        published.beside("texts.jsonl"),
    );
    published.verbalize(&roots, &texts);
    let embeddings = published.beside("embeddings.parquet");
    embed(&texts, &embeddings, "2");
    kgf::vectors::assemble(
        &published.bundle(),
        &embeddings,
        &roots,
        &published.beside("config.json"),
    )
    .unwrap();
    published.describe();
    let id_map = published.bundle().join("vectors/entities.idmap");
    let good = std::fs::read(&id_map).unwrap();

    // The id map records a different Parquet than the one beside it.
    let mut bytes = good.clone();
    bytes[104..136].fill(0xAB);
    std::fs::write(&id_map, &bytes).unwrap();
    let error = format!("{:#}", published.try_describe().unwrap_err());
    assert!(error.contains("different embeddings.parquet"), "{error}");

    // A region open does not read: the first two member ids swapped.
    let mut bytes = good.clone();
    let members = 152 + 32 + 32 + 32;
    let (x, y) = (
        bytes[members..members + 8].to_vec(),
        bytes[members + 8..members + 16].to_vec(),
    );
    bytes[members..members + 8].copy_from_slice(&y);
    bytes[members + 8..members + 16].copy_from_slice(&x);
    std::fs::write(&id_map, &bytes).unwrap();
    let error = format!("{:#}", published.try_describe().unwrap_err());
    assert!(error.contains("entity table puts in"), "{error}");

    std::fs::write(&id_map, &good).unwrap();
    published.describe();
    published.check().expect("restored, it checks again");

    // A manifest whose description no longer matches the sidecar is caught.
    let manifest_path = published.bundle().join("manifest.json");
    let good_manifest = std::fs::read(&manifest_path).unwrap();
    let mut manifest: serde_json::Value = serde_json::from_slice(&good_manifest).unwrap();
    manifest["capabilities"]["vectors"]["model"] = "another-model".into();
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let error = format!("{:#}", published.check().unwrap_err());
    assert!(error.contains("disagrees with the sidecar"), "{error}");
    std::fs::write(&manifest_path, &good_manifest).unwrap();
}

#[test]
fn an_existing_sidecar_is_never_replaced() {
    let published = Published::build(&graph());
    let (roots, texts) = (
        published.beside("roots.jsonl"),
        published.beside("texts.jsonl"),
    );
    published.verbalize(&roots, &texts);
    let embeddings = published.beside("embeddings.parquet");
    embed(&texts, &embeddings, "2");
    kgf::vectors::assemble(
        &published.bundle(),
        &embeddings,
        &roots,
        &published.beside("config.json"),
    )
    .unwrap();

    let error = kgf::vectors::assemble(
        &published.bundle(),
        &embeddings,
        &roots,
        &published.beside("config.json"),
    )
    .unwrap_err();

    assert!(error.to_string().contains("never replaced"), "{error:#}");
}

#[test]
fn roots_and_embeddings_from_different_runs_are_refused_naming_the_root() {
    let published = Published::build(&graph());
    let (roots, texts) = (
        published.beside("roots.jsonl"),
        published.beside("texts.jsonl"),
    );
    published.verbalize(&roots, &texts);
    let embeddings = published.beside("embeddings.parquet");
    let mut digests = embed(&texts, &embeddings, "2");

    // A root whose text the embeddings do not hold.
    let foreign = published.beside("foreign.jsonl");
    std::fs::copy(&roots, &foreign).unwrap();
    writeln!(
        File::options().append(true).open(&foreign).unwrap(),
        r#"{{"iri": "http://example.org/a", "target": "things", "digest": "{}"}}"#,
        "ab".repeat(32)
    )
    .unwrap();
    let error = kgf::vectors::assemble(
        &published.bundle(),
        &embeddings,
        &foreign,
        &published.beside("config.json"),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("which the embeddings do not hold"),
        "{error:#}"
    );
    assert!(
        !published.bundle().join("vectors").exists(),
        "nothing published on failure"
    );

    // A root that is not a subject of this bundle.
    let stranger = published.beside("stranger.jsonl");
    std::fs::copy(&roots, &stranger).unwrap();
    writeln!(
        File::options().append(true).open(&stranger).unwrap(),
        r#"{{"iri": "http://example.org/nobody", "target": "things", "digest": "{}"}}"#,
        kgf_verbalize::text::hex(&digests[0])
    )
    .unwrap();
    let error = kgf::vectors::assemble(
        &published.bundle(),
        &embeddings,
        &stranger,
        &published.beside("config.json"),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("is not a subject of this bundle"),
        "{error:#}"
    );

    // A digest that is not hex, including one whose bytes are not characters
    // at the offsets a hex digit would start — refused, never a panic.
    for bad in ["zz".repeat(32), "é".repeat(32), "ab".repeat(31)] {
        let odd = published.beside("odd.jsonl");
        std::fs::write(
            &odd,
            format!(r#"{{"iri": "http://example.org/a", "target": "things", "digest": "{bad}"}}"#),
        )
        .unwrap();
        let error = kgf::vectors::assemble(
            &published.bundle(),
            &embeddings,
            &odd,
            &published.beside("config.json"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("digest"), "{bad:?}: {error:#}");
    }

    // Roots under a target the config does not declare.
    let other = published.beside("other.jsonl");
    std::fs::write(
        &other,
        format!(
            r#"{{"iri": "http://example.org/a", "target": "elsewhere", "digest": "{}"}}"#,
            kgf_verbalize::text::hex(&digests[0])
        ),
    )
    .unwrap();
    let error = kgf::vectors::assemble(
        &published.bundle(),
        &embeddings,
        &other,
        &published.beside("config.json"),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("which the config does not declare"),
        "{error:#}"
    );

    // A record no root names.
    digests.push([0xEE; 32]);
    let wider = published.beside("wider.parquet");
    write_parquet(&wider, &digests, "2");
    let error = kgf::vectors::assemble(
        &published.bundle(),
        &wider,
        &roots,
        &published.beside("config.json"),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("is named by no root"),
        "{error:#}"
    );

    // The record layout, not the texts layout.
    let old = published.beside("old.parquet");
    write_parquet(&old, &digests[..3], "1");
    let error = kgf::vectors::assemble(
        &published.bundle(),
        &old,
        &roots,
        &published.beside("config.json"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("layout format 1"), "{error:#}");
}
