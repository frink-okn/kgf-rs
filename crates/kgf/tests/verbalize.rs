//! The verbalizer against hdtc-built fixture bundles.
//!
//! These live here rather than beside the verbalizer because only this crate
//! may assert a directory is published and open a `Store` over it. Each test
//! builds a small graph chosen to exercise one rule, opens it through the
//! catalog exactly as `kgf serve` would, and reads the text back.

use std::collections::BTreeMap;

use kgf_store::catalog::{BundleId, Catalog};
use kgf_store::manifest::default_predicate_roles;
use kgf_store::testing::Fixture;
use kgf_store::{OpenOptions, Store};
use kgf_verbalize::{Bound, Config, Grouper, Record, Rendered, Verbalizer};

const DATASET: &str = "fx";
const VERSION: &str = "2026-09-04";

/// A fixture bundle published under a catalog root, and the store over it.
struct Published {
    // Held: dropping it removes the bundle the mappings are over.
    _root: tempfile::TempDir,
    store: std::sync::Arc<Store>,
}

impl Published {
    fn build(graph: &str) -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let bundle = root.path().join(DATASET).join(VERSION);
        Fixture::build(graph).copy_bundle_to(&bundle);
        // A manifest, as `kgf build` would write one.
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: kgf::manifest::Args,
        }
        let cli = <Cli as clap::Parser>::parse_from([
            "kgf-manifest",
            bundle.to_str().unwrap(),
            "--id",
            DATASET,
            "--version",
            VERSION,
        ]);
        kgf::manifest::run(cli.args).expect("describe the bundle");
        let catalog = Catalog::scan(
            kgf::serve::published_root(root.path()).expect("a published root"),
            OpenOptions::default(),
        )
        .expect("scan the root");
        let store = catalog
            .get(&BundleId {
                dataset: DATASET.to_owned(),
                version: VERSION.to_owned(),
            })
            .expect("open the bundle");
        Self { _root: root, store }
    }
}

fn parse_config(json: &str) -> Config {
    serde_json::from_str(json).expect("a well-formed config")
}

/// Every root of every target, as records.
fn all_records(store: &Store, config: &Config) -> Vec<Record> {
    let resolved = config.resolve().expect("a resolvable config");
    let label_role = default_predicate_roles().remove("label").unwrap();
    let bound = Bound::bind(store, &resolved, &label_role).expect("bind");
    let mut verbalizer = Verbalizer::new(store, &bound);
    let mut grouper = Grouper::new(10);
    for target in 0..bound.targets().len() {
        for root in verbalizer.roots(target).expect("roots") {
            if let Some(rendered) = verbalizer.verbalize(target, root).expect("verbalize") {
                grouper.add(rendered);
            }
        }
    }
    grouper.finish()
}

/// One named root under the first target.
fn one(store: &Store, config: &Config, iri: &str) -> Rendered {
    let resolved = config.resolve().expect("a resolvable config");
    let label_role = default_predicate_roles().remove("label").unwrap();
    let bound = Bound::bind(store, &resolved, &label_role).expect("bind");
    let mut verbalizer = Verbalizer::new(store, &bound);
    verbalizer
        .verbalize_iri(0, iri)
        .expect("verbalize")
        .expect("a subject the bundle has")
}

fn by_text(records: &[Record]) -> BTreeMap<&str, &Record> {
    records
        .iter()
        .map(|record| (record.embedding_text.as_str(), record))
        .collect()
}

const EX: &str = "http://example.com/";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";

fn nt(s: &str, p: &str, o: &str) -> String {
    let term = |t: &str| {
        if t.starts_with('"') || t.starts_with("_:") {
            t.to_owned()
        } else if t.contains("://") {
            format!("<{t}>")
        } else {
            format!("<{EX}{t}>")
        }
    };
    format!("{} {} {} .\n", term(s), term(p), term(o))
}

fn graph(triples: &[(&str, &str, &str)]) -> String {
    triples.iter().map(|(s, p, o)| nt(s, p, o)).collect()
}

#[test]
fn a_text_is_a_label_line_then_one_line_per_selected_edge() {
    // A root with a label, a literal, an object with its own label, a blank
    // node value, and a predicate that carries a label of its own.
    let published = Published::build(&graph(&[
        ("root", RDF_TYPE, "Thing"),
        ("root", RDFS_LABEL, "\"Root label\""),
        ("root", "hasRelatedThing", "relatedThing"),
        ("root", "has-score", "\"42\""),
        ("root", "blank", "_:b1"),
        ("_:b1", "nestedName", "\"Nested literal\""),
        ("hasRelatedThing", RDFS_LABEL, "\"related predicate\""),
        ("relatedThing", RDFS_LABEL, "\"Related label\""),
    ]));
    let config = parse_config(r#"{"targets": {"thing": {"type": "http://example.com/Thing"}}}"#);
    let rendered = one(&published.store, &config, &format!("{EX}root"));

    assert_eq!(rendered.label, "Root label");
    // Predicates in IRI order, which puts `rdf:type` (…/1999/…) ahead of
    // `rdfs:label` (…/2000/…); the blank node contributes no line; the type
    // line names the class by its humanized fragment because it has no label;
    // and the label predicate is walked like any other, so the label appears
    // twice unless a config excludes it from the walk.
    assert_eq!(
        rendered.text,
        "label: Root label\n\
         has score: 42\n\
         related predicate: Related label\n\
         type: Thing\n\
         label: Root label"
    );
}

#[test]
fn the_config_cascade_wins_over_the_bundle_role() {
    // A generic label carrying boilerplate beside a domain predicate carrying
    // the name: the config lists the domain predicate first and gets it.
    let published = Published::build(&graph(&[
        ("root", RDF_TYPE, "Thing"),
        ("root", "cites", "article"),
        ("article", RDFS_LABEL, "\"Journal article about data\""),
        (
            "article",
            "http://purl.org/dc/terms/title",
            "\"Nitrogen and tillage effects\"",
        ),
    ]));
    let with_title_first = parse_config(
        r#"{"defaults": {"label_predicates": ["http://purl.org/dc/terms/title"]},
            "targets": {"thing": {"type": "http://example.com/Thing"}}}"#,
    );
    let rendered = one(&published.store, &with_title_first, &format!("{EX}root"));
    assert!(
        rendered
            .text
            .contains("cites: Nitrogen and tillage effects"),
        "{}",
        rendered.text
    );

    // With no config cascade the bundle's role decides, and rdfs:label wins.
    let role_only = parse_config(r#"{"targets": {"thing": {"type": "http://example.com/Thing"}}}"#);
    let rendered = one(&published.store, &role_only, &format!("{EX}root"));
    assert!(
        rendered.text.contains("cites: Journal article about data"),
        "{}",
        rendered.text
    );
}

#[test]
fn a_predicate_limit_keeps_the_same_sample_on_every_run() {
    let mut triples = vec![("root", RDF_TYPE, "Thing")];
    let parts: Vec<String> = (0..12).map(|i| format!("\"part {i}\"")).collect();
    for part in &parts {
        triples.push(("root", "hasPart", part.as_str()));
    }
    let published = Published::build(&graph(&triples));
    let config = parse_config(
        r#"{"predicate_limit": 3, "targets": {"thing": {"type": "http://example.com/Thing"}}}"#,
    );
    let first = one(&published.store, &config, &format!("{EX}root"));
    let second = one(&published.store, &config, &format!("{EX}root"));
    assert_eq!(first, second);
    let parts_kept: Vec<&str> = first
        .text
        .lines()
        .filter_map(|line| line.strip_prefix("has part: "))
        .collect();
    assert_eq!(parts_kept.len(), 3, "{}", first.text);
    // A sample, not a prefix: the three kept are not simply the first three.
    assert_ne!(parts_kept, ["part 0", "part 1", "part 2"]);
}

#[test]
fn an_unchanged_node_reads_the_same_when_ids_shift() {
    // Two bundles holding the same root, one with extra terms that shift every
    // id: the text is the same, because nothing in it is keyed on an id.
    let base = vec![
        ("root", RDF_TYPE, "Thing"),
        ("root", RDFS_LABEL, "\"Root\""),
        ("root", "hasPart", "\"part a\""),
        ("root", "hasPart", "\"part b\""),
        ("root", "hasPart", "\"part c\""),
        ("root", "hasPart", "\"part d\""),
        ("root", "hasPart", "\"part e\""),
    ];
    let mut shifted = base.clone();
    shifted.push(("aaa", RDF_TYPE, "Thing"));
    shifted.push(("aaa", "aaaPredicate", "\"aaa\""));
    shifted.push(("aaa", "hasPart", "\"aaa part\""));
    let config = parse_config(
        r#"{"predicate_limit": 2, "targets": {"thing": {"type": "http://example.com/Thing"}}}"#,
    );
    let one_bundle = Published::build(&graph(&base));
    let other_bundle = Published::build(&graph(&shifted));
    let root = format!("{EX}root");
    assert_eq!(
        one(&one_bundle.store, &config, &root),
        one(&other_bundle.store, &config, &root)
    );
}

#[test]
fn an_allow_list_restricts_and_a_deny_list_applies_within_it() {
    let published = Published::build(&graph(&[
        ("root", RDF_TYPE, "Thing"),
        ("root", "hasPart", "\"a part\""),
        ("root", "ignored", "\"skip me\""),
        ("root", "other", "\"other value\""),
    ]));
    let root = format!("{EX}root");
    let lines = |config: &Config| -> Vec<String> {
        one(&published.store, config, &root)
            .text
            .lines()
            .skip(1)
            .map(str::to_owned)
            .collect()
    };

    // Nothing declared: every predicate walks.
    let all = parse_config(r#"{"targets": {"thing": {"type": "http://example.com/Thing"}}}"#);
    assert_eq!(
        lines(&all),
        [
            "has part: a part",
            "ignored: skip me",
            "other: other value",
            "type: Thing"
        ]
    );

    // An allow list is exactly the walk, even naming a predicate a deny list
    // elsewhere would skip.
    let allow = parse_config(
        r#"{"targets": {"thing": {"type": "http://example.com/Thing",
            "include_predicates": ["http://example.com/ignored"]}}}"#,
    );
    assert_eq!(lines(&allow), ["ignored: skip me"]);

    // A deny list still applies inside the allow list.
    let both = parse_config(
        r#"{"targets": {"thing": {"type": "http://example.com/Thing",
            "include_predicates": ["http://example.com/hasPart", "http://example.com/ignored"],
            "ignore_predicates": ["http://example.com/ignored"]}}}"#,
    );
    assert_eq!(lines(&both), ["has part: a part"]);

    // Defaults and the target's own lists merge.
    let merged = parse_config(
        r#"{"defaults": {"ignore_predicates": ["http://example.com/ignored"]},
            "targets": {"thing": {"type": "http://example.com/Thing",
            "ignore_predicates": ["http://example.com/other"]}}}"#,
    );
    assert_eq!(lines(&merged), ["has part: a part", "type: Thing"]);
}

#[test]
fn a_target_template_names_the_root_and_a_profile_names_a_mention() {
    let published = Published::build(&graph(&[
        ("root", RDF_TYPE, "Thing"),
        ("root", RDFS_LABEL, "\"Root label\""),
        ("root", "has-score", "\"42\""),
        ("root", "hasRelatedThing", "related"),
        ("related", RDF_TYPE, "Related"),
        ("related", RDFS_LABEL, "\"Related label\""),
        ("related", "hasRelatedThing", "root"),
    ]));
    let config = parse_config(
        r#"{
          "profiles": {
            "related": {"type": "http://example.com/Related", "template": "profile: {name}",
                        "fields": {"name": "http://www.w3.org/2000/01/rdf-schema#label"}}
          },
          "targets": {
            "thing": {"type": "http://example.com/Thing",
                      "label_template": "{name}: {score}",
                      "label_fields": {"name": "http://www.w3.org/2000/01/rdf-schema#label",
                                       "score": "http://example.com/has-score"}}
          }
        }"#,
    );
    let rendered = one(&published.store, &config, &format!("{EX}root"));
    // The root is named by its target's template ...
    assert_eq!(rendered.label, "Root label: 42");
    // ... and the mention by its class's profile.
    assert!(
        rendered
            .text
            .contains("has related thing: profile: Related label"),
        "{}",
        rendered.text
    );

    // Seen from the other side, the root is a mention: the target template
    // does not apply to it, and it has no profile, so the cascade names it.
    let from_related = parse_config(
        r#"{
          "targets": {
            "related": {"type": "http://example.com/Related"},
            "thing": {"type": "http://example.com/Thing",
                      "label_template": "{name}: {score}",
                      "label_fields": {"name": "http://www.w3.org/2000/01/rdf-schema#label",
                                       "score": "http://example.com/has-score"}}
          }
        }"#,
    );
    let rendered = one(&published.store, &from_related, &format!("{EX}related"));
    assert!(
        rendered.text.contains("has related thing: Root label\n"),
        "{}",
        rendered.text
    );
}

#[test]
fn identical_text_from_several_roots_is_one_record_naming_them_all() {
    let published = Published::build(&graph(&[
        ("a", RDF_TYPE, "Thing"),
        ("a", "value", "\"same\""),
        ("b", RDF_TYPE, "Thing"),
        ("b", "value", "\"same\""),
        ("c", RDF_TYPE, "Thing"),
        ("c", "value", "\"different\""),
        ("_:x", RDF_TYPE, "Thing"),
        ("_:x", "value", "\"same\""),
    ]));
    // The type and label edges are left out of the walk so that the label
    // line is the only thing telling roots apart.
    let config = parse_config(
        r#"{"targets": {"thing": {"type": "http://example.com/Thing",
            "ignore_predicates": ["http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                                  "http://www.w3.org/2000/01/rdf-schema#label"]}}}"#,
    );
    let records = all_records(&published.store, &config);
    let grouped = by_text(&records);
    // Without labels the fallback names each root by its fragment, so a, b
    // and c differ in their label line; the blank-node root is skipped.
    assert_eq!(records.len(), 3);
    assert_eq!(grouped["label: a\nvalue: same"].iris, [format!("{EX}a")]);
    assert_eq!(grouped["label: c\nvalue: different"].label, "c");

    // Give a and b the same label and they collapse into one record.
    let published = Published::build(&graph(&[
        ("a", RDF_TYPE, "Thing"),
        ("a", RDFS_LABEL, "\"twin\""),
        ("a", "value", "\"same\""),
        ("b", RDF_TYPE, "Thing"),
        ("b", RDFS_LABEL, "\"twin\""),
        ("b", "value", "\"same\""),
    ]));
    let records = all_records(&published.store, &config);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].iris, [format!("{EX}a"), format!("{EX}b")]);
    assert_eq!(records[0].iri_count, 2);
    assert_eq!(records[0].embedding_text, "label: twin\nvalue: same");
}

#[test]
fn iris_the_bundle_lacks_are_reported_and_bind_to_nothing() {
    let published = Published::build(&graph(&[("root", RDF_TYPE, "Thing")]));
    let config = parse_config(
        r#"{"targets": {
            "thing": {"type": "http://example.com/Thing",
                      "ignore_predicates": ["http://example.com/nope"]},
            "ghost": {"type": "http://example.com/Ghost"}}}"#,
    );
    let resolved = config.resolve().unwrap();
    let bound = Bound::bind(&published.store, &resolved, &[]).unwrap();
    let unknown: Vec<(&str, &str)> = bound
        .unknown()
        .iter()
        .map(|u| (u.at.as_str(), u.iri.as_str()))
        .collect();
    assert_eq!(
        unknown,
        [
            ("targets.ghost.type", "http://example.com/Ghost"),
            ("targets.thing.ignore_predicates", "http://example.com/nope"),
        ]
    );
    let verbalizer = Verbalizer::new(&published.store, &bound);
    assert!(verbalizer.roots(0).unwrap().is_empty());
    assert_eq!(verbalizer.roots(1).unwrap().len(), 1);
}
