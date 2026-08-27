//! The Tier-1 description set: `stats/`'s eight artifacts.
//!
//! hdtc owns the HDT, permutation, VoID, and namespace byte formats. This
//! module composes those builders and derives KGF's semantic TSV projections
//! and persisted summaries on top of them.
//!
//! It writes files and nothing else — no manifest is read, none is written,
//! nothing is published. [`super::execute`] owns where the output directory
//! came from and what happens to it, which is what lets one producer serve a
//! build without knowing anything about staging or publication.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use kgf_server::url::Params;
use kgf_store::manifest::ArtifactView;
use oxrdf::{NamedOrBlankNode, Term, Triple};
use oxrdfio::{RdfFormat, RdfParser};
use serde::Serialize;
use serde_json::{Value, json};

use crate::manifest::{DescriptionArtifactMetadata, RowArtifactMetadata};

const VOID_CLASS_PARTITION: &str = "http://rdfs.org/ns/void#classPartition";
const VOID_PROPERTY_PARTITION: &str = "http://rdfs.org/ns/void#propertyPartition";
const VOID_CLASS: &str = "http://rdfs.org/ns/void#class";
const VOID_PROPERTY: &str = "http://rdfs.org/ns/void#property";
const VOID_TRIPLES: &str = "http://rdfs.org/ns/void#triples";
const VOID_ENTITIES: &str = "http://rdfs.org/ns/void#entities";
const VOID_DISTINCT_SUBJECTS: &str = "http://rdfs.org/ns/void#distinctSubjects";
const VOID_DISTINCT_OBJECTS: &str = "http://rdfs.org/ns/void#distinctObjects";
const VOID_PROPERTIES: &str = "http://rdfs.org/ns/void#properties";
const VOID_EXT_OBJECT_CLASS_PARTITION: &str = "http://ldf.fi/void-ext#objectClassPartition";
const VOID_EXT_DATATYPE_PARTITION: &str = "http://ldf.fi/void-ext#datatypePartition";
const VOID_EXT_DATATYPE: &str = "http://ldf.fi/void-ext#datatype";

const SCHEMA_HEADER: &str = "view\tkind\tclass\tpredicate\tdatatype\tsubject_id\n";
const RELATIONS_HEADER: &str = "view\tsubject_class\tpredicate\tobject_class\ttriples\n";
const CLASS_PROPERTIES_HEADER: &str =
    "view\tclass\tpredicate\ttriples\tdistinct_subjects\tdistinct_objects\n";

/// What a summary card says about the dataset it describes.
///
/// The summary needs six descriptive fields and nothing structural. Taking them
/// as their own value rather than as a `Manifest` is what lets the producer run
/// before a manifest exists at all — which it must, because `kgf build` writes
/// the manifest last, after this has already produced the artifacts it will
/// describe.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DatasetCard<'a> {
    pub(crate) id: &'a str,
    pub(crate) version: &'a str,
    pub(crate) title: Option<&'a str>,
    pub(crate) description: Option<&'a str>,
    pub(crate) license: Option<&'a str>,
    pub(crate) homepage: Option<&'a str>,
}

/// Everything the description producer needs, and nothing about where the
/// bundle it describes gets its identity from.
pub(crate) struct Inputs<'a> {
    /// How to invoke hdtc, carrying the build's configured memory limit and its
    /// per-invocation temporary directories.
    pub(crate) runner: &'a super::hdtc::Runner<'a>,
    /// The bundle's `data.hdt`.
    pub(crate) data: &'a Path,
    /// Stable dataset IRI, already validated as an absolute IRI.
    pub(crate) dataset_iri: &'a str,
    /// Prefix tables, layered with later files winning.
    pub(crate) prefix_tables: &'a [PathBuf],
    /// Prefixes layered last of all, above every table.
    pub(crate) extra_prefixes: &'a BTreeMap<String, String>,
    /// What the summary card says about the dataset.
    pub(crate) card: DatasetCard<'a>,
    /// Scratch directory for intermediates that are not published.
    pub(crate) work: &'a Path,
}

/// What a description build produced, beyond the files themselves.
pub(crate) struct Outcome {
    /// Row bounds and view ranges only the producer can measure.
    pub(crate) metadata: DescriptionArtifactMetadata,
    /// Schema selectors written, across both views.
    pub(crate) schema_rows: usize,
    /// Typed class relations written, across both views.
    pub(crate) relation_rows: usize,
    /// Class properties written, across both views.
    pub(crate) class_property_rows: usize,
}

/// Produce the complete eight-artifact description set into `into`.
///
/// Writes files and nothing else: no manifest is read, none is written, and
/// nothing is published. The caller owns where `into` came from and what
/// happens to it, which is the difference between upgrading a live bundle in
/// place and assembling a new one.
pub(crate) fn produce(inputs: Inputs<'_>, into: &Path) -> Result<Outcome> {
    let Inputs {
        runner,
        data,
        dataset_iri,
        prefix_tables,
        extra_prefixes,
        card,
        work,
    } = inputs;

    let void_nt = work.join("void.nt");
    runner.run(&super::hdtc::Step {
        name: "queryable VoID analysis",
        temp: None,
        args: vec![
            OsString::from("void"),
            data.as_os_str().to_owned(),
            OsString::from("--dataset-uri"),
            OsString::from(dataset_iri),
            OsString::from("--output"),
            void_nt.as_os_str().to_owned(),
            // Dataset-level property partitions carry exact distinct subject and
            // object counts; the object side reads `data.hdt.perm`, which every
            // bundle publishes.
            OsString::from("--partition-distinct-counts"),
            OsString::from("dataset-properties"),
        ],
    })?;

    let void_hdt = into.join("void.hdt");
    runner.run(&super::hdtc::Step {
        name: "VoID HDT and permutation build",
        temp: Some("void-create"),
        args: vec![
            OsString::from("create"),
            OsString::from("--output"),
            void_hdt.as_os_str().to_owned(),
            OsString::from("--perm"),
            OsString::from("--dataset-uri"),
            OsString::from(dataset_iri),
            void_nt.as_os_str().to_owned(),
        ],
    })?;
    ensure!(
        into.join("void.hdt.perm").is_file(),
        "hdtc did not produce {}",
        into.join("void.hdt.perm").display()
    );

    let triples = read_ntriples(&void_nt)?;
    let graph = VoidGraph::new(&triples);
    let root = NamedOrBlankNode::NamedNode(oxrdf::NamedNode::new(dataset_iri)?);
    graph.require_dataset_root(&root)?;
    let subject_ids = subject_ids(&void_hdt)?;
    let queryable = graph.project(&root, &subject_ids)?;
    // A componentless bundle has one real graph. `design` and `queryable`
    // are API aliases for that same root, not distinct RDF datasets.
    let design = graph.project(&root, &subject_ids)?;
    let schema_views = vec![
        ("design", design.schema.as_slice()),
        ("queryable", queryable.schema.as_slice()),
    ];
    let relation_views = vec![
        ("design", design.relations.as_slice()),
        ("queryable", queryable.relations.as_slice()),
    ];
    let class_property_views = vec![
        ("design", design.class_properties.as_slice()),
        ("queryable", queryable.class_properties.as_slice()),
    ];
    let schema_rows = schema_views
        .iter()
        .map(|(_, rows)| rows.len())
        .sum::<usize>();
    let relation_rows = relation_views
        .iter()
        .map(|(_, rows)| rows.len())
        .sum::<usize>();
    let class_property_rows = class_property_views
        .iter()
        .map(|(_, rows)| rows.len())
        .sum::<usize>();
    let schema = render_schema(&schema_views)?;
    let relations = render_relations(&relation_views)?;
    let class_properties = render_class_properties(&class_property_views)?;
    write(&into.join("schema-nodes.tsv"), &schema.bytes)?;
    write(&into.join("class-relations.tsv"), &relations.bytes)?;
    write(&into.join("class-properties.tsv"), &class_properties.bytes)?;

    let mut namespace_tables = prefix_tables.to_vec();
    if !extra_prefixes.is_empty() {
        // Layered last, above every table: the shared OKN table is the base and
        // a per-dataset binding wins over it.
        let extra = work.join("dataset-prefixes.json");
        write(&extra, &serde_json::to_vec_pretty(extra_prefixes)?)?;
        namespace_tables.push(extra);
    }
    let namespaces_path = into.join("namespaces.json");
    let mut namespace_args = vec![OsString::from("namespaces")];
    for table in &namespace_tables {
        namespace_args.push(OsString::from("--prefixes"));
        namespace_args.push(table.as_os_str().to_owned());
    }
    namespace_args.extend([
        OsString::from("--output"),
        namespaces_path.as_os_str().to_owned(),
        OsString::from("--format"),
        OsString::from("json"),
        data.as_os_str().to_owned(),
    ]);
    runner.run(&super::hdtc::Step {
        name: "namespace inventory",
        args: namespace_args,
        temp: None,
    })?;
    let mut namespaces: Value = serde_json::from_slice(
        &std::fs::read(&namespaces_path)
            .with_context(|| format!("reading {}", namespaces_path.display()))?,
    )
    .with_context(|| format!("parsing {}", namespaces_path.display()))?;
    let prefix_table = namespaces
        .get_mut("prefix_table")
        .and_then(Value::as_object_mut)
        .context("hdtc namespace inventory has no prefix_table object")?;
    // hdtc records its input paths for build diagnostics. Those paths are not
    // content and would make otherwise identical bundles machine-dependent.
    // The merged map's version digest is the published prefix-table identity.
    prefix_table.remove("source");
    write(&namespaces_path, &serde_json::to_vec_pretty(&namespaces)?)?;

    let summary = Summary::new(&card, dataset_iri, &graph, &root, &design, namespaces);
    write(
        &into.join("summary.json"),
        &serde_json::to_vec_pretty(&summary.json)?,
    )?;
    write(&into.join("summary.md"), summary.markdown.as_bytes())?;

    Ok(Outcome {
        metadata: DescriptionArtifactMetadata {
            schema_nodes: schema.metadata,
            class_relations: relations.metadata,
            class_properties: class_properties.metadata,
        },
        schema_rows,
        relation_rows,
        class_property_rows,
    })
}

fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn read_ntriples(path: &Path) -> Result<Vec<Triple>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    RdfParser::from_format(RdfFormat::NTriples)
        .for_reader(file)
        .map(|quad| quad.map(Triple::from).map_err(anyhow::Error::from))
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("parsing hdtc VoID output {}", path.display()))
}

#[derive(Debug, Clone)]
struct SchemaRow {
    kind: &'static str,
    class: String,
    predicate: String,
    datatype: String,
    subject_id: u64,
}

#[derive(Debug, Clone, Serialize)]
struct RelationRow {
    subject_class: String,
    predicate: String,
    object_class: String,
    triples: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ClassPropertyRow {
    class: String,
    predicate: String,
    triples: u64,
    distinct_subjects: Option<u64>,
    distinct_objects: Option<u64>,
}

struct Projections {
    schema: Vec<SchemaRow>,
    relations: Vec<RelationRow>,
    class_properties: Vec<ClassPropertyRow>,
    classes: Vec<(String, u64)>,
    properties: Vec<PropertyRow>,
}

/// A dataset-level property partition. The distinct counts are `None` when the
/// source VoID partition does not state them, exactly as in `ClassPropertyRow`.
#[derive(Debug, Clone)]
struct PropertyRow {
    predicate: String,
    triples: u64,
    distinct_subjects: Option<u64>,
    distinct_objects: Option<u64>,
}

struct ProjectionRows<'a> {
    schema: &'a mut Vec<SchemaRow>,
    relations: &'a mut Vec<RelationRow>,
    class_properties: &'a mut Vec<ClassPropertyRow>,
}

struct RenderedTsv {
    bytes: Vec<u8>,
    metadata: RowArtifactMetadata,
}

fn render_schema(views: &[(&str, &[SchemaRow])]) -> Result<RenderedTsv> {
    let mut blocks = Vec::new();
    for &(view, rows) in views {
        let mut sorted = rows.to_vec();
        sorted.sort_by(|a, b| {
            (&a.kind, &a.class, &a.predicate, &a.datatype).cmp(&(
                &b.kind,
                &b.class,
                &b.predicate,
                &b.datatype,
            ))
        });
        for pair in sorted.windows(2) {
            ensure!(
                (
                    pair[0].kind,
                    &pair[0].class,
                    &pair[0].predicate,
                    &pair[0].datatype
                ) != (
                    pair[1].kind,
                    &pair[1].class,
                    &pair[1].predicate,
                    &pair[1].datatype
                ),
                "VoID traversal produced duplicate schema selector"
            );
        }
        blocks.push((view, sorted));
    }
    render_views(SCHEMA_HEADER, blocks, |view, row, bytes, max| {
        let line = format!(
            "{view}\t{}\t{}\t{}\t{}\t{}\n",
            row.kind, row.class, row.predicate, row.datatype, row.subject_id
        );
        *max = (*max).max(line.len() as u64);
        bytes.extend_from_slice(line.as_bytes());
    })
}

fn render_relations(views: &[(&str, &[RelationRow])]) -> Result<RenderedTsv> {
    let mut blocks = Vec::new();
    for &(view, rows) in views {
        let mut sorted = rows.to_vec();
        sorted.sort_by(|a, b| {
            b.triples.cmp(&a.triples).then_with(|| {
                (&a.subject_class, &a.predicate, &a.object_class).cmp(&(
                    &b.subject_class,
                    &b.predicate,
                    &b.object_class,
                ))
            })
        });
        blocks.push((view, sorted));
    }
    render_views(RELATIONS_HEADER, blocks, |view, row, bytes, max| {
        let line = format!(
            "{view}\t{}\t{}\t{}\t{}\n",
            row.subject_class, row.predicate, row.object_class, row.triples
        );
        *max = (*max).max(line.len() as u64);
        bytes.extend_from_slice(line.as_bytes());
    })
}

fn render_class_properties(views: &[(&str, &[ClassPropertyRow])]) -> Result<RenderedTsv> {
    let mut blocks = Vec::new();
    for &(view, rows) in views {
        let mut sorted = rows.to_vec();
        sorted.sort_by(|a, b| {
            b.triples
                .cmp(&a.triples)
                .then_with(|| (&a.class, &a.predicate).cmp(&(&b.class, &b.predicate)))
        });
        blocks.push((view, sorted));
    }
    render_views(CLASS_PROPERTIES_HEADER, blocks, |view, row, bytes, max| {
        let line = format!(
            "{view}\t{}\t{}\t{}\t{}\t{}\n",
            row.class,
            row.predicate,
            row.triples,
            optional_decimal(row.distinct_subjects),
            optional_decimal(row.distinct_objects),
        );
        *max = (*max).max(line.len() as u64);
        bytes.extend_from_slice(line.as_bytes());
    })
}

fn render_views<T>(
    header: &str,
    views: Vec<(&str, Vec<T>)>,
    mut append: impl FnMut(&str, &T, &mut Vec<u8>, &mut u64),
) -> Result<RenderedTsv> {
    let mut bytes = header.as_bytes().to_vec();
    let mut max_row_bytes = header.len() as u64;
    let mut directory = BTreeMap::new();
    for (view, rows) in views {
        let offset = bytes.len() as u64;
        let row_count = rows.len() as u64;
        for row in &rows {
            append(view, row, &mut bytes, &mut max_row_bytes);
        }
        directory.insert(
            view.to_owned(),
            ArtifactView {
                offset,
                bytes: bytes.len() as u64 - offset,
                rows: row_count,
            },
        );
    }
    Ok(RenderedTsv {
        bytes,
        metadata: RowArtifactMetadata {
            max_row_bytes,
            views: directory,
        },
    })
}

struct VoidGraph {
    outgoing: HashMap<NamedOrBlankNode, HashMap<String, Vec<Term>>>,
}

impl VoidGraph {
    fn new(triples: &[Triple]) -> Self {
        let mut outgoing: HashMap<NamedOrBlankNode, HashMap<String, Vec<Term>>> = HashMap::new();
        for triple in triples {
            outgoing
                .entry(triple.subject.clone())
                .or_default()
                .entry(triple.predicate.as_str().to_owned())
                .or_default()
                .push(triple.object.clone());
        }
        Self { outgoing }
    }

    fn require_dataset_root(&self, root: &NamedOrBlankNode) -> Result<()> {
        ensure!(
            self.outgoing.contains_key(root),
            "hdtc VoID output does not describe dataset root {root}"
        );
        Ok(())
    }

    fn objects(&self, subject: &NamedOrBlankNode, predicate: &str) -> &[Term] {
        self.outgoing
            .get(subject)
            .and_then(|predicates| predicates.get(predicate))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn children(
        &self,
        subject: &NamedOrBlankNode,
        predicate: &str,
    ) -> Result<Vec<NamedOrBlankNode>> {
        self.objects(subject, predicate)
            .iter()
            .map(|term| match term {
                Term::NamedNode(node) => Ok(NamedOrBlankNode::NamedNode(node.clone())),
                Term::BlankNode(node) => Ok(NamedOrBlankNode::BlankNode(node.clone())),
                Term::Literal(_) => bail!("VoID partition edge {predicate} has literal target"),
            })
            .collect()
    }

    fn unique_iri(&self, subject: &NamedOrBlankNode, predicate: &str) -> Result<String> {
        let values = self.objects(subject, predicate);
        ensure!(
            values.len() == 1,
            "VoID node {subject} must have exactly one {predicate}, found {}",
            values.len()
        );
        match &values[0] {
            Term::NamedNode(node) => Ok(node.as_str().to_owned()),
            other => bail!("VoID node {subject} has non-IRI {predicate} value {other}"),
        }
    }

    fn count(&self, subject: &NamedOrBlankNode, predicate: &str) -> Result<u64> {
        let values = self.objects(subject, predicate);
        ensure!(
            values.len() == 1,
            "VoID node {subject} must have exactly one {predicate}, found {}",
            values.len()
        );
        match &values[0] {
            Term::Literal(literal) => literal
                .value()
                .parse::<u64>()
                .with_context(|| format!("VoID {predicate} is not an unsigned count: {literal}")),
            other => bail!("VoID node {subject} has non-literal {predicate} value {other}"),
        }
    }

    fn optional_count(&self, subject: &NamedOrBlankNode, predicate: &str) -> u64 {
        self.count(subject, predicate).unwrap_or(0)
    }

    /// A count the partition may or may not state.
    ///
    /// Absence and malformedness are different answers and must not collapse
    /// into one. `Ok(None)` means the partition does not state the count, which
    /// the summary renders by omitting the key; anything present but unusable —
    /// a non-numeric literal, two conflicting values for one partition — is an
    /// error, because publishing "never computed" for a value hdtc did compute
    /// and got wrong is exactly the misreport the summary must not make.
    fn maybe_count(&self, subject: &NamedOrBlankNode, predicate: &str) -> Result<Option<u64>> {
        if self.objects(subject, predicate).is_empty() {
            return Ok(None);
        }
        self.count(subject, predicate).map(Some)
    }

    fn project(
        &self,
        root: &NamedOrBlankNode,
        subject_ids: &HashMap<String, u64>,
    ) -> Result<Projections> {
        let mut schema = vec![SchemaRow {
            kind: "dataset",
            class: String::new(),
            predicate: String::new(),
            datatype: String::new(),
            subject_id: id_for(root, subject_ids)?,
        }];
        let mut relations = Vec::new();
        let mut class_properties = Vec::new();
        let mut classes = Vec::new();
        let mut properties = Vec::new();

        for property_node in self.children(root, VOID_PROPERTY_PARTITION)? {
            let predicate = self.unique_iri(&property_node, VOID_PROPERTY)?;
            properties.push(PropertyRow {
                predicate: predicate.clone(),
                triples: self.count(&property_node, VOID_TRIPLES)?,
                distinct_subjects: self.maybe_count(&property_node, VOID_DISTINCT_SUBJECTS)?,
                distinct_objects: self.maybe_count(&property_node, VOID_DISTINCT_OBJECTS)?,
            });
            self.project_property(
                &property_node,
                "",
                &predicate,
                subject_ids,
                &mut ProjectionRows {
                    schema: &mut schema,
                    relations: &mut relations,
                    class_properties: &mut class_properties,
                },
            )?;
        }
        for class_node in self.children(root, VOID_CLASS_PARTITION)? {
            let class = self.unique_iri(&class_node, VOID_CLASS)?;
            classes.push((class.clone(), self.count(&class_node, VOID_ENTITIES)?));
            schema.push(SchemaRow {
                kind: "class",
                class: class.clone(),
                predicate: String::new(),
                datatype: String::new(),
                subject_id: id_for(&class_node, subject_ids)?,
            });
            for property_node in self.children(&class_node, VOID_PROPERTY_PARTITION)? {
                let predicate = self.unique_iri(&property_node, VOID_PROPERTY)?;
                self.project_property(
                    &property_node,
                    &class,
                    &predicate,
                    subject_ids,
                    &mut ProjectionRows {
                        schema: &mut schema,
                        relations: &mut relations,
                        class_properties: &mut class_properties,
                    },
                )?;
            }
        }
        classes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        properties.sort_by(|a, b| {
            b.triples
                .cmp(&a.triples)
                .then_with(|| a.predicate.cmp(&b.predicate))
        });
        relations.sort_by(|a, b| {
            b.triples.cmp(&a.triples).then_with(|| {
                (&a.subject_class, &a.predicate, &a.object_class).cmp(&(
                    &b.subject_class,
                    &b.predicate,
                    &b.object_class,
                ))
            })
        });
        Ok(Projections {
            schema,
            relations,
            class_properties,
            classes,
            properties,
        })
    }

    fn project_property(
        &self,
        property_node: &NamedOrBlankNode,
        class: &str,
        predicate: &str,
        subject_ids: &HashMap<String, u64>,
        rows: &mut ProjectionRows<'_>,
    ) -> Result<()> {
        rows.schema.push(SchemaRow {
            kind: "property",
            class: class.to_owned(),
            predicate: predicate.to_owned(),
            datatype: String::new(),
            subject_id: id_for(property_node, subject_ids)?,
        });
        if !class.is_empty() {
            rows.class_properties.push(ClassPropertyRow {
                class: class.to_owned(),
                predicate: predicate.to_owned(),
                triples: self.count(property_node, VOID_TRIPLES)?,
                distinct_subjects: self.maybe_count(property_node, VOID_DISTINCT_SUBJECTS)?,
                distinct_objects: self.maybe_count(property_node, VOID_DISTINCT_OBJECTS)?,
            });
        }
        for datatype_node in self.children(property_node, VOID_EXT_DATATYPE_PARTITION)? {
            rows.schema.push(SchemaRow {
                kind: "datatype",
                class: class.to_owned(),
                predicate: predicate.to_owned(),
                datatype: self.unique_iri(&datatype_node, VOID_EXT_DATATYPE)?,
                subject_id: id_for(&datatype_node, subject_ids)?,
            });
        }
        if !class.is_empty() {
            for target in self.children(property_node, VOID_EXT_OBJECT_CLASS_PARTITION)? {
                let values = self.objects(&target, VOID_CLASS);
                if values.is_empty() {
                    continue;
                }
                ensure!(
                    values.len() == 1,
                    "object-class partition {target} has multiple class terms"
                );
                let object_class = match &values[0] {
                    Term::NamedNode(node) => node.as_str().to_owned(),
                    other => bail!("object-class partition {target} has non-IRI class {other}"),
                };
                rows.relations.push(RelationRow {
                    subject_class: class.to_owned(),
                    predicate: predicate.to_owned(),
                    object_class,
                    triples: self.count(&target, VOID_TRIPLES)?,
                });
            }
        }
        Ok(())
    }
}

fn optional_decimal(value: Option<u64>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
}

fn id_for(subject: &NamedOrBlankNode, ids: &HashMap<String, u64>) -> Result<u64> {
    let key = match subject {
        NamedOrBlankNode::NamedNode(node) => node.as_str().to_owned(),
        NamedOrBlankNode::BlankNode(node) => format!("_:{}", node.as_str()),
    };
    ids.get(&key)
        .copied()
        .with_context(|| format!("VoID subject {subject} is absent from the final HDT dictionary"))
}

fn subject_ids(path: &Path) -> Result<HashMap<String, u64>> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let sections = hdtc::format::scan_hdt_sections(&mut file)
        .with_context(|| format!("scanning {}", path.display()))?;
    let mut ids = HashMap::new();
    read_subject_section(&mut file, sections.shared, 0, "shared", &mut ids)?;
    read_subject_section(
        &mut file,
        sections.subjects,
        sections.shared.string_count,
        "subjects",
        &mut ids,
    )?;
    Ok(ids)
}

fn read_subject_section(
    file: &mut File,
    section: hdtc::format::PfcSection,
    offset: u64,
    name: &str,
    ids: &mut HashMap<String, u64>,
) -> Result<()> {
    file.seek(SeekFrom::Start(section.section_start))?;
    let header = hdtc::format::PfcSectionHeader::read_from(file, name)?;
    for (index, term) in hdtc::format::PfcSectionIterator::new(file, header, name).enumerate() {
        let term = String::from_utf8(term?)
            .with_context(|| format!("non-UTF-8 term in {name} dictionary section"))?;
        ids.insert(term, offset + index as u64 + 1);
    }
    Ok(())
}

struct Summary {
    json: Value,
    markdown: String,
}

impl Summary {
    fn new(
        card: &DatasetCard<'_>,
        dataset_iri: &str,
        graph: &VoidGraph,
        root: &NamedOrBlankNode,
        projections: &Projections,
        namespaces: Value,
    ) -> Self {
        let top_classes = projections
            .classes
            .iter()
            .take(10)
            .map(|(class, entities)| {
                json!({
                    "class": class,
                    "entities": entities,
                    "links": {
                        "schema": summary_schema_link(
                            Params::default()
                                .with("class", &iri_request(class))
                                .with("children", "properties")
                                .with("view", "design")
                        )
                    }
                })
            })
            .collect::<Vec<_>>();
        let top_properties = projections
            .properties
            .iter()
            .take(10)
            .map(|row| {
                let mut entry = serde_json::Map::new();
                entry.insert("predicate".into(), json!(row.predicate));
                entry.insert("triples".into(), json!(row.triples));
                // Omitted rather than nulled when the partition does not state
                // them, matching the class-properties projection (doc 03 §3.6).
                for (key, value) in [
                    ("distinct_subjects", row.distinct_subjects),
                    ("distinct_objects", row.distinct_objects),
                ] {
                    if let Some(value) = value {
                        entry.insert(key.into(), json!(value));
                    }
                }
                entry.insert(
                    "links".into(),
                    json!({
                        "schema": summary_schema_link(
                            Params::default()
                                .with("predicate", &iri_request(&row.predicate))
                                .with("view", "design")
                        )
                    }),
                );
                Value::Object(entry)
            })
            .collect::<Vec<_>>();
        let leading_relations = projections
            .relations
            .iter()
            .take(10)
            .map(|relation| {
                json!({
                    "subject_class": relation.subject_class,
                    "predicate": relation.predicate,
                    "object_class": relation.object_class,
                    "triples": relation.triples,
                    "links": {
                        "schema": summary_schema_link(
                            Params::default()
                                .with("class", &iri_request(&relation.subject_class))
                                .with("predicate", &iri_request(&relation.predicate))
                                .with("projection", "class-relations")
                                .with("view", "design")
                        )
                    }
                })
            })
            .collect::<Vec<_>>();
        let counts = json!({
            "triples": graph.optional_count(root, VOID_TRIPLES),
            "subjects": graph.optional_count(root, VOID_DISTINCT_SUBJECTS),
            "predicates": graph.optional_count(root, VOID_PROPERTIES),
            "objects": graph.optional_count(root, VOID_DISTINCT_OBJECTS),
        });
        let json = json!({
            "dataset": {
                "id": card.id,
                "version": card.version,
                "iri": dataset_iri,
                "title": card.title,
                "description": card.description,
                "license": card.license,
                "homepage": card.homepage,
            },
            "view": "design",
            "links": {
                "manifest": "manifest",
                "fragment": "fragment",
                "schema": "schema?view=design",
                "classes": "schema?children=classes&view=design",
                "properties": "schema?children=properties&view=design",
                "class_relations": "schema?projection=class-relations&view=design",
                "class_properties": "schema?projection=class-properties&view=design",
                "void": "void",
            },
            "counts": counts,

            "top_classes": top_classes,
            "top_properties": top_properties,
            "leading_class_relations": leading_relations,
            "namespaces": namespaces,
        });
        let markdown = render_summary_markdown(card, &json);
        Self { json, markdown }
    }
}

fn iri_request(iri: &str) -> String {
    format!("<{iri}>")
}

fn summary_schema_link(params: Params) -> String {
    format!("schema?{}", params.to_query())
}

fn render_summary_markdown(card: &DatasetCard<'_>, summary: &Value) -> String {
    let title = card.title.unwrap_or(card.id);
    let counts = &summary["counts"];
    let mut out = format!(
        "# {title}\n\n## Dataset facts\n\n- Dataset: `{}` version `{}`\n- Triples: {}; distinct subjects: {}; predicates: {}; distinct objects: {}\n",
        card.id,
        card.version,
        counts["triples"],
        counts["subjects"],
        counts["predicates"],
        counts["objects"],
    );
    if let Some(license) = card.license {
        out.push_str(&format!("- License: {license}\n"));
    }
    out.push_str("\n## Description\n\n");
    match card.description {
        Some(description) => {
            out.push_str(description);
            out.push('\n');
        }
        None => out.push_str("No description was supplied.\n"),
    }
    append_ranked(
        &mut out,
        "Top classes",
        &summary["top_classes"],
        "class",
        "entities",
    );
    append_ranked(
        &mut out,
        "Top predicates",
        &summary["top_properties"],
        "predicate",
        "triples",
    );
    out.push_str("\n## Leading typed class relations\n\n");
    if let Some(relations) = summary["leading_class_relations"].as_array() {
        if relations.is_empty() {
            out.push_str("No typed object-class relations were observed.\n");
        } else {
            for relation in relations {
                out.push_str(&format!(
                    "- `{}` → `{}` → `{}` ({} triples)\n",
                    relation["subject_class"].as_str().unwrap_or(""),
                    relation["predicate"].as_str().unwrap_or(""),
                    relation["object_class"].as_str().unwrap_or(""),
                    relation["triples"],
                ));
            }
        }
    }
    out
}

fn append_ranked(out: &mut String, heading: &str, values: &Value, term: &str, count: &str) {
    out.push_str(&format!("\n## {heading}\n\n"));
    if let Some(values) = values.as_array() {
        if values.is_empty() {
            out.push_str("None observed.\n");
        } else {
            for value in values {
                out.push_str(&format!(
                    "- `{}` ({})\n",
                    value[term].as_str().unwrap_or(""),
                    value[count]
                ));
            }
        }
    }
}
