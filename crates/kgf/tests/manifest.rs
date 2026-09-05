//! End-to-end `kgf manifest`, over a bundle assembled the way a person would.
//!
//! hdtc builds the golden-bundle artifacts, so what is described is a
//! producer's output rather than this
//! crate's guess. The point of the test is the seam — that a directory holding
//! nothing but `hdtc create --perm`'s output becomes a bundle
//! [`Store::open`](kgf_store::Store::open) accepts, and that the manifest stops
//! agreeing the moment the artifacts move underneath it.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use kgf_store::manifest::{ArtifactView, Manifest};
use kgf_store::store::artifact;
use kgf_store::testing::{NAMESPACES_JSON, hdtc_binary};
use kgf_store::{OpenOptions, Role, SchemaSelector, StatsView, Store};

const SOURCE: &str = concat!(
    "<http://example.org/alice> <http://example.org/name> \"Alice\" .\n",
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> .\n",
    "<http://example.org/bob> <http://example.org/name> \"Bob\" .\n",
    "<http://example.org/bob> <http://example.org/knows> <http://example.org/alice> .\n",
);

/// One more triple, and one more subject and object, than [`SOURCE`].
const GROWN_SOURCE: &str = concat!(
    "<http://example.org/alice> <http://example.org/name> \"Alice\" .\n",
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> .\n",
    "<http://example.org/bob> <http://example.org/name> \"Bob\" .\n",
    "<http://example.org/bob> <http://example.org/knows> <http://example.org/alice> .\n",
    "<http://example.org/carol> <http://example.org/name> \"Carol\" .\n",
);

/// [`SOURCE`] with one literal edited: every count is identical and every
/// artifact byte is not. Counts are a weak witness for a rebuild, which is why
/// `--check` also compares checksums.
const RETITLED_SOURCE: &str = concat!(
    "<http://example.org/alice> <http://example.org/name> \"Alicia\" .\n",
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> .\n",
    "<http://example.org/bob> <http://example.org/name> \"Bob\" .\n",
    "<http://example.org/bob> <http://example.org/knows> <http://example.org/alice> .\n",
);

const TYPED_SOURCE: &str = concat!(
    "<http://example.org/alice> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/Person> .\n",
    "<http://example.org/bob> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/Person> .\n",
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> .\n",
);

const VOID_SOURCE: &str = concat!(
    "<https://example.org/design> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://rdfs.org/ns/void#Dataset> .\n",
    "<https://example.org/queryable> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://rdfs.org/ns/void#Dataset> .\n",
    "<https://example.org/queryable> <http://rdfs.org/ns/void#triples> \"4\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n",
    "<https://example.org/queryable> <http://rdfs.org/ns/void#distinctSubjects> \"2\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n",
    "<https://example.org/queryable> <http://rdfs.org/ns/void#properties> \"2\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n",
    "<https://example.org/queryable> <http://rdfs.org/ns/void#distinctObjects> \"4\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n",
    "<https://example.org/queryable> <http://rdfs.org/ns/void#subset> <https://example.org/design> .\n",
);

#[test]
fn a_hand_assembled_bundle_becomes_servable_and_stays_honest() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join("demo-kg").join("2026-08-01");
    std::fs::create_dir_all(&bundle).unwrap();
    build_artifacts(&bundle, SOURCE);

    // Before: artifacts only. `Store::open` refuses a directory that is not a
    // bundle, naming the command that completes it.
    let error = open(&bundle).expect_err("a bundle without a manifest is not servable");
    assert!(error.to_string().contains("kgf manifest"), "{error}");

    kgf(&[
        "manifest",
        path(&bundle),
        "--prefix",
        "ex=http://example.org/",
    ])
    .success();

    // After: servable, and the manifest describes what the store reads.
    let store = open(&bundle).expect("bundle opens once it has a manifest");
    let manifest = Manifest::read(&bundle).unwrap();
    assert_eq!(manifest.counts.triples, store.triples());
    assert_eq!(manifest.counts.triples, 4);

    // Identity is inferred from the catalog layout, so hand-assembly needs no
    // flags for it.
    assert_eq!(manifest.id, "demo-kg");
    assert_eq!(manifest.version, "2026-08-01");
    assert!(manifest.content_digest.starts_with("sha256:"));
    assert_eq!(manifest.prefixes["ex"], "http://example.org/");

    // Both required artifacts are checksummed; the manifest never lists itself.
    assert_eq!(manifest.artifacts.len(), 2);
    assert!(manifest.artifacts.contains_key("data.hdt"));
    assert!(manifest.artifacts.contains_key("data.hdt.perm"));
    assert!(!manifest.artifacts.contains_key("manifest.json"));

    // A core bundle declares only the optional operations this build serves.
    let mut capabilities: Vec<&str> = manifest.capabilities.keys().map(String::as_str).collect();
    capabilities.sort_unstable();
    assert_eq!(capabilities, ["labels", "sample"]);
    assert!(manifest.predicate_roles.contains_key("label"));

    kgf(&["manifest", path(&bundle), "--check"]).success();

    // Regeneration carries the identity forward and reproduces the same bytes,
    // so a manifest can be diffed across versions.
    let before = std::fs::read(bundle.join("manifest.json")).unwrap();
    kgf(&["manifest", path(&bundle)]).success();
    let after = std::fs::read(bundle.join("manifest.json")).unwrap();
    assert_eq!(before, after, "regeneration is not byte-stable");

    // The failure this all exists for: artifacts rebuilt, manifest not.
    std::fs::remove_file(bundle.join("data.hdt")).unwrap();
    std::fs::remove_file(bundle.join("data.hdt.perm")).unwrap();
    build_artifacts(&bundle, GROWN_SOURCE);

    let stale = kgf(&["manifest", path(&bundle), "--check"]).failure();
    assert!(stale.contains("counts.triples"), "{stale}");
    assert!(stale.contains("kgf manifest"), "{stale}");

    kgf(&["manifest", path(&bundle)]).success();
    kgf(&["manifest", path(&bundle), "--check"]).success();

    let regenerated = Manifest::read(&bundle).unwrap();
    assert_eq!(regenerated.counts.triples, 5);
    // Descriptive fields survive a regeneration; the digest tracks the bytes.
    assert_eq!(regenerated.prefixes["ex"], "http://example.org/");
    assert_ne!(regenerated.content_digest, manifest.content_digest);
}

/// `kgf build` produces and publishes one verified description set.
///
/// The assertions below were written against the retired `kgf build stats`,
/// which upgraded a hand-assembled bundle in place. They move here unchanged
/// because the producer did not change — only the command that drives it — so
/// what they pin is the description set itself rather than an entry point.
#[test]
fn a_build_publishes_one_verified_description_set() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("typed.nt");
    std::fs::write(&source, TYPED_SOURCE).unwrap();
    let prefixes = root.path().join("prefixes.json");
    std::fs::write(
        &prefixes,
        r#"{"ex":"http://example.org/","rdf":"http://www.w3.org/1999/02/22-rdf-syntax-ns#"}"#,
    )
    .unwrap();
    let config_text = |table: &std::path::Path| {
        format!(
            concat!(
                "schema: 1\n",
                "dataset:\n",
                "  id: typed-kg\n",
                "  iri: https://example.org/typed-kg\n",
                "  title: Typed KG\n",
                "  description: Publisher supplied text.\n",
                "semantics:\n",
                "  prefix_tables: ['{}']\n",
            ),
            path(table)
        )
    };
    let config = root.path().join("build.yaml");
    std::fs::write(&config, config_text(&prefixes)).unwrap();

    let bundle = root.path().join("bundles").join("typed-kg").join("v1");
    let hdtc = hdtc_binary();
    kgf(&[
        "build",
        "--config",
        path(&config),
        "--out",
        path(&bundle),
        "--input",
        path(&source),
        "--hdtc",
        path(&hdtc),
    ])
    .success();

    let manifest = Manifest::read(&bundle).unwrap();
    for name in artifact::DESCRIPTION {
        assert!(manifest.artifacts.contains_key(name), "missing {name}");
    }
    assert_eq!(
        manifest.artifacts[artifact::VOID_HDT].parents,
        [artifact::HDT]
    );
    assert_eq!(
        manifest.artifacts[artifact::VOID_PERM].parents,
        [artifact::VOID_HDT]
    );
    for name in [
        artifact::SCHEMA_NODES,
        artifact::CLASS_RELATIONS,
        artifact::CLASS_PROPERTIES,
    ] {
        let entry = &manifest.artifacts[name];
        assert_eq!(entry.parents, [artifact::VOID_HDT]);
        assert!(entry.max_row_bytes.is_some());
        assert!(entry.views.contains_key("design"));
        assert!(entry.views.contains_key("queryable"));
    }
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join(artifact::SUMMARY_JSON)).unwrap())
            .unwrap();
    assert_eq!(summary["dataset"]["id"], "typed-kg");
    assert_eq!(summary["counts"]["triples"], 3);
    assert_eq!(summary["links"]["schema"], "schema?view=design");
    assert_eq!(
        summary["links"]["class_properties"],
        "schema?projection=class-properties&view=design"
    );
    assert_eq!(
        summary["top_classes"][0]["links"]["schema"],
        "schema?children=properties&class=%3Chttp%3A%2F%2Fexample.org%2FPerson%3E&view=design"
    );
    assert!(
        summary["namespaces"]["prefix_table"]
            .get("source")
            .is_none()
    );
    let namespaces: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join(artifact::NAMESPACES)).unwrap()).unwrap();
    assert!(namespaces["prefix_table"].get("source").is_none());
    assert!(
        std::fs::read_to_string(bundle.join(artifact::SUMMARY_MD))
            .unwrap()
            .contains("## Description")
    );
    let class_properties =
        std::fs::read_to_string(bundle.join(artifact::CLASS_PROPERTIES)).unwrap();
    assert!(
        class_properties
            .contains("design\thttp://example.org/Person\thttp://example.org/knows\t1\t\t"),
        "{class_properties}"
    );

    let store = open(&bundle).expect("stats producer publishes a store-verifiable bundle");
    let description = store.description().expect("description set opens");
    let dictionary = description.dict();
    assert_eq!(
        dictionary
            .locate(Role::Subject, b"https://example.org/typed-kg#kgf-design")
            .unwrap(),
        None,
        "a componentless build must not mint a synthetic design dataset"
    );
    assert_eq!(
        dictionary
            .locate(Role::Predicate, b"http://rdfs.org/ns/void#subset")
            .unwrap(),
        None,
        "a componentless build has no component subset to assert"
    );
    let design_root = description
        .view(&StatsView::Design)
        .unwrap()
        .schema_node(SchemaSelector::Dataset)
        .unwrap()
        .unwrap()
        .subject();
    let queryable_root = description
        .view(&StatsView::Queryable)
        .unwrap()
        .schema_node(SchemaSelector::Dataset)
        .unwrap()
        .unwrap()
        .subject();
    assert_eq!(
        design_root, queryable_root,
        "the two API views alias the sole published dataset root"
    );
    drop(store);
    // hdtc records its input paths for build diagnostics. Those paths are not
    // content, so where the prefix table happened to live must not change what
    // a bundle publishes. The retired `kgf build stats` proved this by
    // regenerating in place; a build proves it by building twice.
    let relocated_prefixes = root.path().join("relocated-prefixes.json");
    std::fs::rename(&prefixes, &relocated_prefixes).unwrap();
    let relocated_config = root.path().join("relocated.yaml");
    std::fs::write(&relocated_config, config_text(&relocated_prefixes)).unwrap();
    let again = root.path().join("bundles").join("typed-kg").join("v2");
    kgf(&[
        "build",
        "--config",
        path(&relocated_config),
        "--out",
        path(&again),
        "--input",
        path(&source),
        "--hdtc",
        path(&hdtc),
    ])
    .success();
    assert_eq!(
        std::fs::read(bundle.join(artifact::NAMESPACES)).unwrap(),
        std::fs::read(again.join(artifact::NAMESPACES)).unwrap(),
        "prefix-table filesystem location changed the published namespace inventory"
    );

    let after = std::fs::read(bundle.join(artifact::MANIFEST)).unwrap();
    kgf(&["manifest", path(&bundle), "--check"]).success();

    let manifest_path = bundle.join(artifact::MANIFEST);
    let mut document: serde_json::Value = serde_json::from_slice(&after).unwrap();
    document["components"] = serde_json::json!([{"id": "canonical", "role": "source"}]);
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&document).unwrap(),
    )
    .unwrap();
    let open_error = open(&bundle).expect_err("component views are not silently misclassified");
    assert!(
        open_error
            .to_string()
            .contains("does not yet support component description views"),
        "{open_error}"
    );
    let error = kgf(&["manifest", path(&bundle), "--check"]).failure();
    assert!(
        error.contains("does not yet verify component description views"),
        "{error}"
    );
}

#[test]
fn a_text_index_is_described_as_one_artifact_and_must_belong_to_its_bundle() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join("tox").join("2026-06-01");
    std::fs::create_dir_all(&bundle).unwrap();
    build_artifacts(&bundle, SOURCE);
    build_text_index(&bundle);

    kgf(&["manifest", path(&bundle)]).success();
    let manifest = Manifest::read(&bundle).unwrap();

    // One entry for the directory, not one per segment file — those names are
    // chosen per build.
    let text = manifest
        .artifacts
        .get("data.hdt.text")
        .expect("the index is checksummed");
    assert!(text.bytes > 0);
    assert!(!manifest.artifacts.keys().any(|name| name.contains('/')));

    // And carrying the index is what declares the capability.
    assert!(manifest.capabilities.contains_key("search"));
    kgf(&["manifest", path(&bundle), "--check"]).success();

    // An index built from a *different* HDT is the dangerous case: its hits
    // are object ids that resolve to real terms in the wrong dictionary, so
    // every row would look well formed. It is refused where the whole-file
    // digests are checked, which is here.
    let other = root.path().join("other").join("v1");
    std::fs::create_dir_all(&other).unwrap();
    build_artifacts(&other, GROWN_SOURCE);
    build_text_index(&other);
    std::fs::remove_dir_all(bundle.join("data.hdt.text")).unwrap();
    copy_dir(&other.join("data.hdt.text"), &bundle.join("data.hdt.text"));

    let error = kgf(&["manifest", path(&bundle), "--check"]).failure();
    assert!(error.contains("binding mismatch"), "{error}");
    assert!(
        error.contains("hdtc text"),
        "the remedy must be named: {error}"
    );
}

#[test]
fn a_manifest_is_not_written_for_a_text_index_tantivy_cannot_open() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join("demo-kg").join("2026-08-01");
    std::fs::create_dir_all(&bundle).unwrap();
    build_artifacts(&bundle, SOURCE);
    build_text_index(&bundle);

    // The hdtc manifest and source binding remain valid; only Tantivy's own
    // index metadata is broken. Checking merely hdtc-text.meta would therefore
    // publish a bundle that Store::open immediately refuses.
    std::fs::write(bundle.join("data.hdt.text").join("meta.json"), b"not json").unwrap();
    let error = kgf(&["manifest", path(&bundle)]).failure();
    assert!(error.contains("text index could not be opened"), "{error}");
    assert!(!bundle.join("manifest.json").exists());
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

#[test]
fn a_rebuild_that_preserves_every_count_still_fails_the_check() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join("demo-kg").join("2026-08-01");
    std::fs::create_dir_all(&bundle).unwrap();
    build_artifacts(&bundle, SOURCE);
    kgf(&["manifest", path(&bundle)]).success();

    let before = Manifest::read(&bundle).unwrap();

    // Edit one literal: same triples, same id-space sizes, different bytes.
    std::fs::remove_file(bundle.join("data.hdt")).unwrap();
    std::fs::remove_file(bundle.join("data.hdt.perm")).unwrap();
    build_artifacts(&bundle, RETITLED_SOURCE);

    let facts_are_unchanged = kgf(&["manifest", path(&bundle), "--check"]);
    let error = facts_are_unchanged.failure();
    // Counts cannot catch this; the checksums must.
    assert!(error.contains("sha256"), "{error}");
    assert!(error.contains("kgf manifest"), "{error}");

    kgf(&["manifest", path(&bundle)]).success();
    let after = Manifest::read(&bundle).unwrap();
    assert_eq!(after.counts, before.counts, "the counts really are equal");
    assert_ne!(after.content_digest, before.content_digest);
}

#[test]
fn a_hand_edited_content_digest_is_caught() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join("demo-kg").join("2026-08-01");
    std::fs::create_dir_all(&bundle).unwrap();
    build_artifacts(&bundle, SOURCE);
    kgf(&["manifest", path(&bundle)]).success();

    // The one case per-artifact checksums miss, since nothing on disk moved.
    let manifest_path = bundle.join("manifest.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    document["content_digest"] = serde_json::json!("sha256:0000");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&document).unwrap(),
    )
    .unwrap();

    let error = kgf(&["manifest", path(&bundle), "--check"]).failure();
    assert!(error.contains("content_digest"), "{error}");
}

#[test]
fn regeneration_preserves_fields_this_build_does_not_model() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join("demo-kg").join("2026-08-01");
    std::fs::create_dir_all(&bundle).unwrap();
    build_artifacts(&bundle, SOURCE);
    kgf(&["manifest", path(&bundle)]).success();

    // A field with no producer yet, added by hand as someone would.
    // `source` is deliberately not the example any more: `kgf build bundle`
    // models and writes it, so it is carried forward as a typed field rather
    // than as an opaque one.
    let manifest_path = bundle.join("manifest.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    document["components"] = serde_json::json!([{"id": "canonical", "role": "source"}]);
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&document).unwrap(),
    )
    .unwrap();

    std::fs::remove_file(bundle.join("data.hdt")).unwrap();
    std::fs::remove_file(bundle.join("data.hdt.perm")).unwrap();
    build_artifacts(&bundle, GROWN_SOURCE);
    kgf(&["manifest", path(&bundle)]).success();

    let rewritten: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(rewritten["components"], document["components"]);
    assert_eq!(rewritten["counts"]["triples"], 5);
}

#[test]
fn a_manifest_from_a_newer_build_is_refused_rather_than_downgraded() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join("demo-kg").join("2026-08-01");
    std::fs::create_dir_all(&bundle).unwrap();
    build_artifacts(&bundle, SOURCE);
    kgf(&["manifest", path(&bundle)]).success();

    let manifest_path = bundle.join("manifest.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    document["formats"]["manifest"] = serde_json::json!("2");
    let newer = serde_json::to_vec_pretty(&document).unwrap();
    std::fs::write(&manifest_path, &newer).unwrap();

    let error = kgf(&["manifest", path(&bundle)]).failure();
    assert!(error.contains("format 2"), "{error}");
    // Refusing to read it and then rewriting it anyway would be incoherent.
    assert_eq!(
        std::fs::read(&manifest_path).unwrap(),
        newer,
        "a manifest this build cannot read must not be overwritten"
    );
}

#[test]
fn manifest_check_runs_the_offline_description_index_proof() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join("demo-kg").join("2026-08-01");
    std::fs::create_dir_all(&bundle).unwrap();
    build_artifacts(&bundle, SOURCE);
    kgf(&["manifest", path(&bundle)]).success();

    let schema = b"view\tkind\tclass\tpredicate\tdatatype\tsubject_id\n";
    let relations = b"view\tsubject_class\tpredicate\tobject_class\ttriples\n";
    let empty_views = views(schema.len() as u64, 0, 0, 0, 0);
    let empty_relations = views(relations.len() as u64, 0, 0, 0, 0);
    publish_description_manifest(&bundle, schema, empty_views, relations, empty_relations);

    let error = kgf(&["manifest", path(&bundle), "--check"]).failure();
    assert!(
        error.contains("queryable view has no dataset selector"),
        "{error}"
    );
}

#[test]
fn a_valid_description_proof_passes_check_and_regeneration() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join("demo-kg").join("2026-08-01");
    std::fs::create_dir_all(&bundle).unwrap();
    build_artifacts(&bundle, SOURCE);
    kgf(&["manifest", path(&bundle)]).success();

    let header = b"view\tkind\tclass\tpredicate\tdatatype\tsubject_id\n";
    let design = b"design\tdataset\t\t\t\t1\n";
    let queryable = b"queryable\tdataset\t\t\t\t2\n";
    let schema = [header.as_slice(), design.as_slice(), queryable.as_slice()].concat();
    let schema_views = views(
        header.len() as u64,
        design.len() as u64,
        1,
        queryable.len() as u64,
        1,
    );
    let relations = b"view\tsubject_class\tpredicate\tobject_class\ttriples\n";
    let relation_views = views(relations.len() as u64, 0, 0, 0, 0);
    publish_description_manifest(&bundle, &schema, schema_views, relations, relation_views);

    kgf(&["manifest", path(&bundle), "--check"]).success();
    kgf(&["manifest", path(&bundle)]).success();
    kgf(&["manifest", path(&bundle), "--check"]).success();
}

fn publish_description_manifest(
    bundle: &Path,
    schema: &[u8],
    schema_views: BTreeMap<String, ArtifactView>,
    relations: &[u8],
    relation_views: BTreeMap<String, ArtifactView>,
) {
    build_void_artifacts(bundle);
    let class_properties =
        b"view\tclass\tpredicate\ttriples\tdistinct_subjects\tdistinct_objects\n";
    let class_property_views = views(class_properties.len() as u64, 0, 0, 0, 0);
    for (name, bytes) in [
        (artifact::SCHEMA_NODES, schema),
        (artifact::CLASS_RELATIONS, relations),
        (artifact::CLASS_PROPERTIES, class_properties.as_slice()),
        (artifact::NAMESPACES, NAMESPACES_JSON.as_bytes()),
        (artifact::SUMMARY_JSON, b"{}\n".as_slice()),
        (artifact::SUMMARY_MD, b"# Summary\n".as_slice()),
    ] {
        std::fs::write(bundle.join(name), bytes).unwrap();
    }

    let mut manifest = Manifest::read(bundle).unwrap();
    for name in artifact::DESCRIPTION {
        let path = bundle.join(name);
        let mut entry = kgf::manifest::checksum_artifact(&path).unwrap();
        if matches!(
            name,
            artifact::SCHEMA_NODES | artifact::CLASS_RELATIONS | artifact::CLASS_PROPERTIES
        ) {
            entry.parents = vec![artifact::VOID_HDT.to_owned()];
            let bytes = std::fs::read(&path).unwrap();
            entry.max_row_bytes = Some(max_row_bytes(&bytes));
            entry.views = match name {
                artifact::SCHEMA_NODES => schema_views.clone(),
                artifact::CLASS_RELATIONS => relation_views.clone(),
                artifact::CLASS_PROPERTIES => class_property_views.clone(),
                _ => unreachable!("only TSV artifacts reach this branch"),
            };
        } else if name == artifact::VOID_HDT {
            entry.parents = vec![artifact::HDT.to_owned()];
        } else if name == artifact::VOID_PERM {
            entry.parents = vec![artifact::VOID_HDT.to_owned()];
        }
        manifest.artifacts.insert(name.to_owned(), entry);
    }
    manifest.content_digest = kgf::manifest::content_digest(
        manifest
            .artifacts
            .iter()
            .map(|(name, entry)| (name.as_str(), entry)),
    );
    std::fs::write(
        bundle.join(artifact::MANIFEST),
        manifest.to_json_bytes().unwrap(),
    )
    .unwrap();
}

fn views(
    header: u64,
    design_bytes: u64,
    design_rows: u64,
    queryable_bytes: u64,
    queryable_rows: u64,
) -> BTreeMap<String, ArtifactView> {
    BTreeMap::from([
        (
            "design".to_owned(),
            ArtifactView {
                offset: header,
                bytes: design_bytes,
                rows: design_rows,
            },
        ),
        (
            "queryable".to_owned(),
            ArtifactView {
                offset: header + design_bytes,
                bytes: queryable_bytes,
                rows: queryable_rows,
            },
        ),
    ])
}

fn max_row_bytes(bytes: &[u8]) -> u64 {
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::len)
        .max()
        .unwrap_or(0) as u64
}

fn build_void_artifacts(bundle: &Path) {
    let stats = bundle.join("stats");
    std::fs::create_dir_all(&stats).unwrap();
    let input = stats.join("void.nt");
    std::fs::write(&input, VOID_SOURCE).unwrap();
    let output = Command::new(hdtc_binary())
        .args([
            "create",
            path(&input),
            "-o",
            path(&bundle.join(artifact::VOID_HDT)),
            "--temp-dir",
            path(&stats.join("work")),
            "--memory-limit",
            "64M",
            "--perm",
        ])
        .output()
        .expect("run hdtc for VoID graph");
    assert!(
        output.status.success(),
        "hdtc create for VoID graph failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_file(input).unwrap();
    let _ = std::fs::remove_dir_all(stats.join("work"));
}

/// Open the bundle through the read layer.
///
/// # Safety
///
/// The artifacts are written by this test into a temporary directory and are
/// not modified while the returned store lives — each rebuild below happens
/// after the previous store has been dropped.
#[allow(unsafe_code)]
fn open(bundle: &Path) -> kgf_store::Result<Store> {
    let published = unsafe { kgf_store::PublishedBundle::new(bundle) };
    Store::open(&published, OpenOptions::default())
}

fn path(path: &Path) -> &str {
    path.to_str().expect("temp paths are UTF-8")
}

/// Run `hdtc create --perm` into `bundle`, as a person assembling one would.
/// Build a text index over a bundle's HDT, the way `hdtc text` does.
fn build_text_index(bundle: &Path) {
    let output = Command::new(hdtc_binary())
        .args(["text", path(&bundle.join("data.hdt"))])
        .output()
        .expect("run hdtc text");
    assert!(
        output.status.success(),
        "hdtc text failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn build_artifacts(bundle: &Path, source: &str) {
    let input = bundle.join("input.nt");
    std::fs::write(&input, source).unwrap();

    let output = Command::new(hdtc_binary())
        .args([
            "create",
            path(&input),
            "-o",
            path(&bundle.join("data.hdt")),
            "--temp-dir",
            path(&bundle.join("work")),
            "--memory-limit",
            "64M",
            "--perm",
        ])
        .output()
        .expect("run hdtc");
    assert!(
        output.status.success(),
        "hdtc create failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Leave only what a bundle carries, so the artifact list under test is the
    // real one.
    std::fs::remove_file(&input).unwrap();
    let _ = std::fs::remove_dir_all(bundle.join("work"));
}

/// The outcome of one `kgf` invocation.
struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

impl Run {
    fn success(self) -> String {
        assert!(
            self.status.success(),
            "kgf failed unexpectedly:\n{}",
            self.stderr
        );
        self.stdout
    }

    fn failure(self) -> String {
        assert!(
            !self.status.success(),
            "kgf succeeded unexpectedly:\n{}",
            self.stdout
        );
        self.stderr
    }
}

fn kgf(args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_kgf"))
        .args(args)
        .output()
        .expect("run kgf");
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        status: output.status,
    }
}
