//! `kgf serve`, end to end over a real socket.
//!
//! The deployment recipe, run as written: hdtc builds the artifacts, `kgf
//! manifest` describes them, `kgf serve` serves the directory. Nothing is
//! stubbed, and the client below writes request lines onto a `TcpStream` by
//! hand rather than going through a client library.
//!
//! That last part is deliberate. The unit's central question is whether an
//! extension method survives the stack, and every HTTP client is itself a stack
//! that might normalize the request. Writing `QUERY /… HTTP/1.1` onto the
//! socket leaves nothing between the test and hyper's parser.

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use clap::Parser;
use kgf_server::service::Service;
use kgf_server::{AccessLog, AccessRecord};
use kgf_store::testing::{Fixture, TINY_NT, WORKED_EXAMPLE_NQ};
use sha2::{Digest, Sha256};

/// A second fixture graph, so two versions of one dataset differ in content and
/// therefore in `content_digest`.
const GROWN_NT: &str = concat!(
    "<http://example.org/alice> <http://example.org/name> \"Alice\" .\n",
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> .\n",
    "<http://example.org/bob> <http://example.org/name> \"Bob\" .\n",
    "<http://example.org/carol> <http://example.org/name> \"Carol\" .\n",
);

const REMOTE_NT: &str = "<http://example.org/bob> <http://example.org/remoteName> \"Bobby\" .\n";

const PARTIAL_OVERLAP_NT: &str = concat!(
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> .\n",
    "<http://example.org/carol> <http://example.org/knows> <http://example.org/bob> .\n",
);

const SUMMARY_CARD_JSON: &str = r#"{
  "dataset": {"id": "tox", "version": "v1", "title": "Fixture graph"},
  "counts": {"triples": 21, "subjects": 9, "predicates": 7, "objects": 14},
  "links": {
    "classes": "/tox/v/v1/schema?children=classes&view=design",
    "properties": "/tox/v/v1/schema?children=properties&view=design",
    "class_relations": "/tox/v/v1/schema?projection=class-relations&view=design",
    "class_properties": "/tox/v/v1/schema?projection=class-properties&view=design",
    "schema": "/tox/v/v1/schema",
    "fragment": "/tox/v/v1/fragment",
    "void": "/tox/v/v1/void"
  },
  "top_classes": [
    {
      "class": "https://example.org/A",
      "entities": 3,
      "links": {"schema": "/tox/v/v1/schema?class=ex%3AA&view=design"}
    }
  ],
  "top_properties": [
    {
      "predicate": "https://example.org/p",
      "triples": 3,
      "links": {"schema": "/tox/v/v1/schema?predicate=ex%3Ap&view=design"}
    }
  ],
  "leading_class_relations": [
    {
      "subject_class": "https://example.org/A",
      "predicate": "https://example.org/p",
      "object_class": "https://example.org/B",
      "triples": 2
    }
  ]
}
"#;

#[test]
fn a_304_carries_the_encoding_metadata_its_200_would_have_carried() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-01-09T09:00:00Z");
    let server = deployment.serve();
    let target = "/tox/v/v1/fragment?limit=100";

    // Compression is negotiated, so the resource varies on the coding and an
    // encoded body must not claim the identity body's strong validator. The trap
    // is the `304`: it carries no body, so nothing marks it downstream, and
    // RFC 9110 §15.4.5 still requires it to carry the `ETag` and `Vary` a `200`
    // to the same request would have sent. A cache updating stored headers from
    // that `304` (RFC 9111 §4.3.4) would otherwise drop the field keeping its
    // stored encoded bytes from an `Accept-Encoding: identity` request.
    //
    // The spellings matter as much as the statuses. Anything that decided from
    // the request would have to re-derive the compression layer's own
    // negotiation, and these are the cases where a narrower reading diverges: a
    // header split across field lines is one list, `x-gzip` is `gzip`, and a
    // zero quality is a refusal. The rule here is unconditional, so every
    // spelling has to come out the same.
    let spellings: [(&str, &[(&str, &str)]); 6] = [
        ("absent", &[]),
        ("identity", &[("accept-encoding", "identity")]),
        ("gzip", &[("accept-encoding", "gzip")]),
        ("zero quality", &[("accept-encoding", "gzip;q=0")]),
        ("x-gzip", &[("accept-encoding", "x-gzip")]),
        (
            "split field lines",
            &[("accept-encoding", "deflate"), ("accept-encoding", "gzip")],
        ),
    ];

    for (label, headers) in spellings {
        let ok = server.request("GET", target, headers);
        ok.assert_status(200);
        let etag = ok.header("etag").expect("a versioned GET carries an ETag");
        assert!(
            etag.starts_with("W/"),
            "{label}: a negotiable representation carries a weak validator, got {etag}"
        );
        assert_encoding_vary(&ok, label);

        let mut conditional = headers.to_vec();
        conditional.push(("if-none-match", etag.as_str()));
        let revalidated = server.request("GET", target, &conditional);
        revalidated.assert_status(304);
        assert_eq!(
            revalidated.header("etag").as_deref(),
            Some(etag.as_str()),
            "{label}: the 304 must carry the validator its 200 sent"
        );
        assert_encoding_vary(&revalidated, label);
    }
}

#[track_caller]
fn assert_encoding_vary(response: &Response, context: &str) {
    let vary = response.header("vary").unwrap_or_default();
    assert!(
        vary.split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("accept-encoding")),
        "Vary must name accept-encoding for {context}, got {vary:?}"
    );
}

#[test]
fn the_url_space_answers_over_a_real_listener() {
    let deployment = Deployment::new();
    deployment.publish("tox", "2026-01-09", TINY_NT, "2026-01-09T09:00:00Z");
    deployment.publish("tox", "2026-06-01", GROWN_NT, "2026-06-01T14:03:22Z");
    deployment.publish("atlas", "v1", TINY_NT, "2020-01-01T00:00:00Z");
    let server = deployment.serve();

    // `/` — the service descriptor: what is hosted, and the caps a client reads
    // rather than assumes.
    let root = server.get("/");
    root.assert_status(200);
    root.assert_cache_control(&["public", "max-age=300"]);
    root.assert_varies_on_accept();
    assert!(
        root.header("x-robots-tag").is_none(),
        "the finite service catalog remains discoverable"
    );
    let descriptor = root.json();
    // The catalog: a summary per dataset, so choosing one is one round trip.
    let datasets = descriptor["datasets"].as_array().unwrap();
    assert_eq!(datasets.len(), 2);
    assert_eq!(datasets[0]["id"], "atlas");
    assert_eq!(datasets[1]["id"], "tox");
    assert_eq!(datasets[1]["current"], "2026-06-01");
    assert_eq!(datasets[1]["title"], "tox 2026-06-01");
    assert!(datasets[1]["triples"].as_u64().unwrap() > 0);
    assert_eq!(datasets[1]["url"], "/tox");
    assert_eq!(descriptor["caps"]["max_limit"], 10_000);
    assert_eq!(descriptor["caps"]["max_bindings"], 1_000);
    assert_eq!(descriptor["implementation"]["protocol"], "1");

    // `/{dataset}` — the release history, and which release is current.
    let dataset = server.get("/tox");
    dataset.assert_status(200);
    assert!(
        dataset.header("x-robots-tag").is_none(),
        "the finite release catalog remains discoverable"
    );
    let descriptor = dataset.json();
    assert_eq!(descriptor["current"], "2026-06-01");
    assert_eq!(descriptor["releases"].as_array().unwrap().len(), 2);

    // An unknown dataset and an unknown version are both 404 and are not the
    // same 404: one is a typo in the name, the other a version that is gone.
    let no_dataset = server.get("/nope");
    no_dataset.assert_status(404);
    no_dataset.assert_header("content-type", "application/problem+json");
    no_dataset.assert_cache_control(&["no-store"]);
    assert_eq!(no_dataset.json()["code"], "not_found");

    let no_version = server.get("/tox/v/1999-01-01/manifest");
    no_version.assert_status(404);
    assert_eq!(no_version.json()["code"], "not_found");
    assert_ne!(
        no_dataset.json()["detail"],
        no_version.json()["detail"],
        "the two 404s must be distinguishable"
    );

    // RFC 9457's `instance`, filled in from the request rather than by hand at
    // every call site.
    assert_eq!(no_version.json()["instance"], "/tox/v/1999-01-01/manifest");
}

#[test]
fn schema_answers_json_html_latest_and_resumable_pages_over_http() {
    let deployment = Deployment::new();
    deployment.publish_description("tox", "v1", "2026-08-08T12:00:00Z");
    let server = deployment.serve();
    let query = "class=ex%3AA&predicate=ex%3Ap&children=object-classes&limit=1";

    let first = server.get(&format!("/tox/v/v1/schema?{query}"));
    first.assert_status(200);
    first.assert_header("content-type", "application/json");
    first.assert_header("kgf-complete", "false");
    first.assert_header("kgf-truncation-reason", "page_limit");
    first.assert_cache_control(&["public", "max-age=31536000", "immutable"]);
    assert_eq!(
        first.json()["items"][0]["term"]["value"],
        "https://example.org/B"
    );
    let next = first.json()["next"].as_str().unwrap().to_owned();

    let labelled = server.get(&format!("/tox/v/v1/schema?{query}&labels=true"));
    labelled.assert_status(200);
    assert_eq!(
        labelled.json()["labels"]["https://example.org/A"],
        serde_json::Value::Null,
        "requested hydration distinguishes a missing label from an omitted labels map"
    );
    assert!(
        first.json().get("labels").is_none(),
        "labels are opt-in for the machine representation"
    );

    let resumed = server.get(&format!(
        "/tox/v/v1/schema?{query}&cursor={}",
        kgf_server::url::encode_value(&next)
    ));
    resumed.assert_status(200);
    resumed.assert_header("kgf-complete", "true");
    assert_eq!(
        resumed.json()["items"][0]["term"]["value"],
        "https://example.org/C"
    );

    let page = server.request(
        "GET",
        &format!("/tox/v/v1/schema?{query}"),
        &[("Accept", "text/html")],
    );
    page.assert_status(200);
    page.assert_header("content-type", "text/html; charset=utf-8");
    let html = String::from_utf8(page.body.clone()).unwrap();
    assert!(html.contains("Property details"));
    assert!(html.contains(&next));

    server
        .get(&format!("/tox/latest/schema?{query}"))
        .assert_header("location", &format!("/tox/v/v1/schema?{query}"));

    let unknown = server.get("/tox/v/v1/schema?view=component%3Amissing");
    unknown.assert_status(404);
    assert_eq!(unknown.json()["code"], "not_found");

    let root = server.get("/").json();
    assert_eq!(root["datasets"][0]["links"]["summary"], "/tox/v/v1/summary");
    assert_eq!(root["datasets"][0]["links"]["schema"], "/tox/v/v1/schema");
    let dataset = server.get("/tox").json();
    assert_eq!(dataset["releases"][0]["links"]["void"], "/tox/v/v1/void");
}

#[test]
fn schema_omits_requested_labels_when_the_release_has_no_label_cascade() {
    let deployment = Deployment::new();
    deployment.publish_description_without_labels("tox", "v1", "2026-08-08T12:00:00Z");
    let server = deployment.serve();

    let response = server.get("/tox/v/v1/schema?children=classes&labels=true");
    response.assert_status(200);
    assert!(
        response.json().get("labels").is_none(),
        "an absent cascade is distinct from configured predicates that found no labels"
    );
}

#[test]
fn tpf_quad_formats_keep_controls_out_of_the_default_graph() {
    const HYDRA: &str = "http://www.w3.org/ns/hydra/core#";
    const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
    const VOID: &str = "http://rdfs.org/ns/void#";
    const FOAF_PRIMARY_TOPIC: &str = "http://xmlns.com/foaf/0.1/primaryTopic";

    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    deployment.set_dataset_iri("tox", "v1", "http://example.org/dataset/tox");
    let server = deployment.serve();
    let target = "/tox/v/v1/tpf?predicate=http%3A%2F%2Fexample.org%2Fknows&limit=1";
    let page = format!("http://{}{}", server.address, target);
    let fragment = format!(
        "http://{}/tox/v/v1/tpf?predicate=http%3A%2F%2Fexample.org%2Fknows",
        server.address
    );
    let metadata = format!("{page}#metadata");
    let dataset = format!("http://{}/tox/v/v1/tpf", server.address);

    let turtle = server.request("GET", target, &[("Accept", "text/turtle")]);
    turtle.assert_status(200);
    turtle.assert_header("content-type", "text/turtle; charset=utf-8");
    turtle.assert_header("kgf-complete", "false");
    turtle.assert_header("kgf-truncation-reason", "page_limit");
    assert!(
        String::from_utf8_lossy(&turtle.body).contains("@prefix kgfbn:"),
        "every prefix-capable RDF response declares the dataset blank-node namespace"
    );
    let turtle_graph: HashSet<_> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::Turtle)
        .for_slice(&turtle.body)
        .collect::<Result<_, _>>()
        .expect("TPF Turtle parses");
    let predicate_count = |iri: &str| {
        turtle_graph
            .iter()
            .filter(|quad| quad.predicate.as_str() == iri)
            .count()
    };

    assert_eq!(predicate_count("http://example.org/knows"), 1);
    assert_eq!(predicate_count(&format!("{HYDRA}search")), 1);
    assert_eq!(predicate_count(&format!("{HYDRA}mapping")), 3);
    assert_eq!(predicate_count(&format!("{HYDRA}variable")), 3);
    assert_eq!(predicate_count(&format!("{HYDRA}property")), 3);
    assert!(turtle_graph.iter().any(|quad| {
        matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == dataset)
            && quad.predicate.as_str() == format!("{HYDRA}search")
    }));

    let variables: HashSet<_> = turtle_graph
        .iter()
        .filter(|quad| quad.predicate.as_str() == format!("{HYDRA}variable"))
        .filter_map(|quad| match &quad.object {
            oxrdf::Term::Literal(value) => Some(value.value().to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(
        variables,
        HashSet::from([
            "subject".to_owned(),
            "predicate".to_owned(),
            "object".to_owned(),
        ])
    );

    let properties: HashSet<_> = turtle_graph
        .iter()
        .filter(|quad| quad.predicate.as_str() == format!("{HYDRA}property"))
        .filter_map(|quad| match &quad.object {
            oxrdf::Term::NamedNode(value) => Some(value.as_str().to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(
        properties,
        HashSet::from([
            format!("{RDF}subject"),
            format!("{RDF}predicate"),
            format!("{RDF}object"),
        ])
    );

    assert!(turtle_graph.iter().any(|quad| {
        matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == page)
            && quad.predicate.as_str() == format!("{HYDRA}totalItems")
            && matches!(&quad.object, oxrdf::Term::Literal(value) if value.value() == "2")
    }));
    assert!(turtle_graph.iter().any(|quad| {
        matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == page)
            && quad.predicate.as_str() == format!("{HYDRA}next")
            && matches!(&quad.object, oxrdf::Term::NamedNode(node) if node.as_str().starts_with(&dataset))
    }));

    let mut datasets = Vec::new();
    for (accept, format) in [
        ("application/n-quads", oxrdfio::RdfFormat::NQuads),
        ("application/trig", oxrdfio::RdfFormat::TriG),
        (
            "application/ld+json",
            oxrdfio::RdfFormat::JsonLd {
                profile: oxrdfio::JsonLdProfile::Streaming | oxrdfio::JsonLdProfile::Expanded,
            },
        ),
    ] {
        let response = server.request("GET", target, &[("Accept", accept)]);
        response.assert_status(200);
        let quads: HashSet<_> = oxrdfio::RdfParser::from_format(format)
            .for_slice(&response.body)
            .collect::<Result<_, _>>()
            .unwrap_or_else(|error| panic!("{accept} did not parse: {error}"));
        assert!(quads.iter().any(|quad| {
            quad.graph_name == oxrdf::GraphName::DefaultGraph
                && quad.predicate.as_str() == "http://example.org/knows"
        }));
        assert!(
            !quads.iter().any(|quad| {
                quad.graph_name == oxrdf::GraphName::DefaultGraph
                    && (quad.predicate.as_str().starts_with(HYDRA)
                        || quad.predicate.as_str().starts_with(VOID)
                        || quad.predicate.as_str() == FOAF_PRIMARY_TOPIC)
            }),
            "{accept} leaked controls into the default graph"
        );
        assert!(quads.iter().any(|quad| {
            matches!(&quad.graph_name, oxrdf::GraphName::NamedNode(node) if node.as_str() == metadata)
                && matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == metadata)
                && quad.predicate.as_str() == FOAF_PRIMARY_TOPIC
                && matches!(&quad.object, oxrdf::Term::NamedNode(node) if node.as_str() == fragment)
        }));
        assert!(quads.iter().any(|quad| {
            matches!(&quad.graph_name, oxrdf::GraphName::NamedNode(node) if node.as_str() == metadata)
                && matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == fragment)
                && quad.predicate.as_str() == format!("{VOID}subset")
                && matches!(&quad.object, oxrdf::Term::NamedNode(node) if node.as_str() == page)
        }));
        assert!(quads.iter().any(|quad| {
            matches!(&quad.graph_name, oxrdf::GraphName::NamedNode(node) if node.as_str() == metadata)
                && matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == dataset)
                && quad.predicate.as_str() == format!("{VOID}subset")
                && matches!(&quad.object, oxrdf::Term::NamedNode(node) if node.as_str() == fragment)
        }));
        assert!(quads.iter().any(|quad| {
            matches!(&quad.graph_name, oxrdf::GraphName::NamedNode(node) if node.as_str() == metadata)
                && matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == dataset)
                && quad.predicate.as_str() == format!("{HYDRA}search")
        }));
        assert!(quads.iter().any(|quad| {
            matches!(&quad.graph_name, oxrdf::GraphName::NamedNode(node) if node.as_str() == metadata)
                && quad.predicate.as_str() == format!("{HYDRA}variableRepresentation")
                && matches!(&quad.object, oxrdf::Term::NamedNode(node) if node.as_str() == format!("{HYDRA}ExplicitRepresentation"))
        }));
        assert!(quads.iter().any(|quad| {
            matches!(&quad.graph_name, oxrdf::GraphName::NamedNode(node) if node.as_str() == metadata)
                && matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == page)
                && quad.predicate.as_str() == format!("{HYDRA}itemsPerPage")
                && matches!(&quad.object, oxrdf::Term::Literal(value) if value.value() == "1")
        }));
        assert!(quads.iter().any(|quad| {
            matches!(&quad.graph_name, oxrdf::GraphName::NamedNode(node) if node.as_str() == metadata)
                && matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == page)
                && quad.predicate.as_str() == format!("{VOID}inDataset")
                && matches!(&quad.object, oxrdf::Term::NamedNode(node) if node.as_str() == "http://example.org/dataset/tox")
        }));
        assert!(!quads.iter().any(|quad| {
            matches!(&quad.graph_name, oxrdf::GraphName::NamedNode(node) if node.as_str() == metadata)
                && matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == "http://example.org/dataset/tox")
                && quad.predicate.as_str() == "http://www.w3.org/2000/01/rdf-schema#seeAlso"
        }), "{accept} advertised an unavailable VoID description");
        datasets.push(quads);
    }
    assert_eq!(datasets[0], datasets[1]);
    assert_eq!(
        datasets[0], datasets[2],
        "JSON-LD must preserve the named control graph"
    );

    let selected = server.get(&format!("{target}&format=nq"));
    let selected_quads: HashSet<_> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::NQuads)
        .for_slice(&selected.body)
        .collect::<Result<_, _>>()
        .expect("format-selected N-Quads parses");
    assert!(selected_quads.iter().any(|quad| {
        quad.predicate.as_str() == FOAF_PRIMARY_TOPIC
            && matches!(&quad.object, oxrdf::Term::NamedNode(node) if node.as_str() == fragment)
    }));

    let unavailable = server.get("/tox/v/v1/void");
    unavailable.assert_status(501);
}

#[test]
fn tpf_links_a_void_description_only_when_it_is_published() {
    let deployment = Deployment::new();
    deployment.publish_description("tox", "v1", "2026-08-08T12:00:00Z");
    deployment.set_dataset_iri("tox", "v1", "http://example.org/dataset/tox");
    let server = deployment.serve();

    server.get("/tox/v/v1/void").assert_status(200);
    let response = server.request(
        "GET",
        "/tox/v/v1/tpf?limit=1",
        &[("Accept", "application/n-quads")],
    );
    response.assert_status(200);
    let quads: Vec<_> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::NQuads)
        .for_slice(&response.body)
        .collect::<Result<_, _>>()
        .expect("TPF N-Quads parses");
    assert!(quads.iter().any(|quad| {
        matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == "http://example.org/dataset/tox")
            && quad.predicate.as_str() == "http://www.w3.org/2000/01/rdf-schema#seeAlso"
            && matches!(&quad.object, oxrdf::Term::NamedNode(node) if node.as_str() == format!("http://{}/tox/v/v1/void", server.address))
    }));
}

#[test]
fn tpf_escapes_raw_query_punctuation_in_its_page_iri() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();
    let target = "/tox/v/v1/tpf?object=%3Fo&values=(%3Fo)%20{%20(%22[x]%22)%20}";
    let response = server.request("GET", target, &[("Accept", "application/n-quads")]);
    response.assert_status(200);

    let quads: Vec<_> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::NQuads)
        .for_slice(&response.body)
        .collect::<Result<_, _>>()
        .expect("the response with escaped page metadata parses");
    let page = format!(
        "http://{}/tox/v/v1/tpf?object=%3Fo&values=(%3Fo)%20%7B%20(%22%5Bx%5D%22)%20%7D",
        server.address
    );
    assert!(quads.iter().any(|quad| {
        matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == page)
            && quad.predicate.as_str() == "http://www.w3.org/ns/hydra/core#itemsPerPage"
    }));
}

#[test]
fn every_tpf_page_names_one_canonical_fragment() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();
    let target =
        "/tox/v/v1/tpf?subject=%3Fs&predicate=http%3A%2F%2Fexample.org%2Fknows&object=%3Fo&limit=1";

    let first = server.request("GET", target, &[("Accept", "application/n-quads")]);
    first.assert_status(200);
    let first_quads: Vec<_> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::NQuads)
        .for_slice(&first.body)
        .collect::<Result<_, _>>()
        .expect("the first page parses");
    let object_for = |quads: &[oxrdf::Quad], predicate: &str| {
        quads.iter().find_map(|quad| {
            (quad.predicate.as_str() == predicate)
                .then(|| match &quad.object {
                    oxrdf::Term::NamedNode(node) => Some(node.as_str().to_owned()),
                    _ => None,
                })
                .flatten()
        })
    };
    let primary_topic = "http://xmlns.com/foaf/0.1/primaryTopic";
    let next_predicate = "http://www.w3.org/ns/hydra/core#next";
    let fragment = object_for(&first_quads, primary_topic).expect("page names its fragment");
    assert_eq!(
        fragment,
        format!(
            "http://{}/tox/v/v1/tpf?object=%3Fo&predicate=http%3A%2F%2Fexample.org%2Fknows&subject=%3Fs",
            server.address
        )
    );
    let next = object_for(&first_quads, next_predicate).expect("first page has a continuation");
    let origin = format!("http://{}", server.address);
    let second = server.request(
        "GET",
        next.strip_prefix(&origin).unwrap(),
        &[("Accept", "application/n-quads")],
    );
    second.assert_status(200);
    let second_quads: Vec<_> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::NQuads)
        .for_slice(&second.body)
        .collect::<Result<_, _>>()
        .expect("the second page parses");
    assert_eq!(object_for(&second_quads, primary_topic), Some(fragment));
}

#[test]
fn tpf_and_native_fragment_keep_their_protocols_disjoint() {
    const HYDRA: &str = "http://www.w3.org/ns/hydra/core#";

    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();
    let bare = "http%3A%2F%2Fexample.org%2Fknows";

    server
        .get("/tox/v/v1/tpf")
        .assert_header("content-type", "application/n-quads; charset=utf-8");

    for (accept, content_type) in [
        ("application/n-quads", "application/n-quads; charset=utf-8"),
        ("application/trig", "application/trig; charset=utf-8"),
        ("text/turtle", "text/turtle; charset=utf-8"),
        ("application/ld+json", "application/ld+json"),
        ("text/html", "text/html; charset=utf-8"),
    ] {
        let tpf = server.request(
            "GET",
            &format!("/tox/v/v1/tpf?predicate={bare}"),
            &[("Accept", accept)],
        );
        tpf.assert_status(200);
        tpf.assert_header("content-type", content_type);
        if accept == "text/html" {
            let page = tpf.text();
            assert!(page.contains("format=nq"));
            assert!(!page.contains("format=json"));
            assert!(page.contains("<dt>subject</dt>"));
            assert!(page.contains("<dt>predicate</dt>"));
            assert!(page.contains("<dt>object</dt>"));
            assert!(!page.contains("<dt>s</dt>"));
        }

        let native = server.request(
            "GET",
            &format!("/tox/v/v1/fragment?p={bare}"),
            &[("Accept", accept)],
        );
        native.assert_status(400);
        assert!(
            String::from_utf8_lossy(&native.body).contains("bad_term_syntax"),
            "{accept}"
        );
    }

    for format in ["nq", "trig", "ttl", "jsonld"] {
        let native = server.get(&format!(
            "/tox/v/v1/fragment?p=ex%3Aknows&limit=1&format={format}"
        ));
        native.assert_status(200);
        assert!(
            !String::from_utf8_lossy(&native.body).contains(HYDRA),
            "native {format} carried TPF controls"
        );
    }

    for target in [
        "/tox/v/v1/tpf?s=http%3A%2F%2Fexample.org%2Falice",
        "/tox/v/v1/tpf?page=2",
        "/tox/v/v1/tpf?o.text=alice",
        "/tox/v/v1/tpf?subject=%3Fx&object=%3Fx",
    ] {
        let refused = server.get(target);
        refused.assert_status(400);
        assert_eq!(refused.json()["code"], "malformed_request", "{target}");
        assert!(refused.json()["detail"].as_str().unwrap().contains("tpf"));
    }

    let values = kgf_server::url::encode_value("(?undeclared) { (<http://example.org/alice>) }");
    let undeclared = server.get(&format!("/tox/v/v1/tpf?values={values}"));
    undeclared.assert_status(400);
    assert_eq!(undeclared.json()["code"], "malformed_request");

    let native_page = server.get("/tox/v/v1/fragment?limit=1");
    let native_cursor = native_page.header("kgf-next-cursor").unwrap();
    let wrong_tpf_cursor = server.get(&format!(
        "/tox/v/v1/tpf?cursor={}",
        kgf_server::url::encode_value(&native_cursor)
    ));
    wrong_tpf_cursor.assert_status(400);
    assert_eq!(wrong_tpf_cursor.json()["code"], "stale_cursor");

    let tpf_page = server.get("/tox/v/v1/tpf?limit=1&format=nq");
    let tpf_cursor = tpf_page.header("kgf-next-cursor").unwrap();
    let wrong_native_cursor = server.get(&format!(
        "/tox/v/v1/fragment?cursor={}",
        kgf_server::url::encode_value(&tpf_cursor)
    ));
    wrong_native_cursor.assert_status(400);
    assert_eq!(wrong_native_cursor.json()["code"], "stale_cursor");

    let native_values = server.get(&format!("/tox/v/v1/fragment?values={values}"));
    native_values.assert_status(400);
    assert_eq!(native_values.json()["code"], "malformed_request");

    for method in ["QUERY", "POST"] {
        let refused = server.request(method, "/tox/v/v1/tpf", &[]);
        refused.assert_status(405);
        assert_eq!(refused.json()["code"], "method_not_allowed");
    }
}

#[test]
fn every_representation_names_a_blank_node_the_same_way() {
    // One node, one name, whatever a client negotiated. The native and RDF
    // representations must agree exactly, because a client that combines them —
    // or federates over several bundles — has nothing else to join on.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();
    let target = "/tox/v/v1/fragment?p=ex%3Atype";

    let json = server.get(target);
    json.assert_status(200);
    let subject = json.json()["rows"][0]["s"].clone();
    let iri = subject["value"].as_str().expect("a term value").to_owned();
    assert_eq!(subject["type"], serde_json::json!("iri"));
    assert!(
        iri.starts_with("urn:fdc:") && iri.ends_with(":s-1"),
        "native JSON publishes the scoped IRI, not a dictionary label: {iri}"
    );

    let turtle = server.request("GET", target, &[("Accept", "text/turtle")]);
    turtle.assert_status(200);
    let data_subject = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::Turtle)
        .for_slice(&turtle.body)
        .collect::<Result<Vec<_>, _>>()
        .expect("fragment Turtle parses")
        .into_iter()
        .find(|quad| quad.predicate.as_str() == "http://example.org/type")
        .expect("the fragment contains its data triple")
        .subject;
    match data_subject {
        oxrdf::NamedOrBlankNode::NamedNode(node) => assert_eq!(
            node.as_str(),
            iri,
            "the RDF and native representations name one node identically"
        ),
        oxrdf::NamedOrBlankNode::BlankNode(_) => panic!("RDF kept a document-local blank node"),
    }

    // The scoped IRI is what addresses it, in either spelling a client holds.
    let asked = server.get(&format!(
        "/tox/v/v1/fragment?s={}",
        kgf_server::url::encode_value(&format!("<{iri}>"))
    ));
    asked.assert_status(200);
    assert_eq!(asked.json()["cardinality"]["value"], serde_json::json!(1));
}

#[test]
fn blank_node_syntax_addresses_nothing_and_says_so() {
    // A stored `_:` label is local to whatever document was loaded, so the same
    // label names unrelated nodes at different bundles. Resolving one would join
    // across knowledge graphs on a coincidence of spelling — so it never
    // matches, and is reported rather than rejected so a mixed batch survives.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    let asked = server.get("/tox/v/v1/fragment?s=_%3Ab1");
    asked.assert_status(200);
    assert_eq!(asked.json()["rows"], serde_json::json!([]));
    assert_eq!(
        asked.json()["absent_terms"],
        serde_json::json!([{"parameter": "s", "reason": "blank_node"}]),
        "the diagnostic separates \"not addressable here\" from \"not in this bundle\""
    );
    assert_eq!(asked.json()["complete"], serde_json::json!(true));
}

/// A blank node carrying a label, so the page has something to show for it.
const LABELLED_BNODE_NT: &str = concat!(
    "_:b1 <http://example.org/name> \"A blank thing\" .\n",
    "_:b1 <http://example.org/type> <http://example.org/Thing> .\n",
    "<http://example.org/alice> <http://example.org/name> \"Alice\" .\n",
);

#[test]
fn a_blank_node_is_labelled_however_the_page_reached_it() {
    // The label cascade runs on whatever spelling a position put on the page:
    // the dictionary label for a row cell, the scoped IRI for a term the
    // request bound. They are one node, so one label — otherwise a node is
    // labelled when a row happens to carry it and bare when you ask about it,
    // which is the sort of difference nobody can explain from the outside.
    //
    // Both requests fix `ex:type`, which keeps the labelling triple out of the
    // rows: the label text can then only have come from a cell's annotation.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", LABELLED_BNODE_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();
    const ANNOTATED: &str = "<span class=\"t-label\">A blank thing</span>";

    let found = server.get("/tox/v/v1/fragment?p=ex%3Atype");
    found.assert_status(200);
    let iri = found.json()["rows"][0]["s"]["value"]
        .as_str()
        .expect("a scoped IRI")
        .to_owned();

    // Reached as a row cell, where the subject is a variable.
    let by_row = server.request(
        "GET",
        "/tox/v/v1/fragment?p=ex%3Atype",
        &[("Accept", "text/html")],
    );
    by_row.assert_status(200);
    assert!(
        by_row.text().contains(ANNOTATED),
        "a blank node found in a row carries its label"
    );

    // Reached as the bound term, where the page merges the request back in.
    let by_request = server.request(
        "GET",
        &format!(
            "/tox/v/v1/fragment?p=ex%3Atype&s={}",
            kgf_server::url::encode_value(&format!("<{iri}>"))
        ),
        &[("Accept", "text/html")],
    );
    by_request.assert_status(200);
    assert!(
        by_request.text().contains(ANNOTATED),
        "and the same node carries it when the request is what named it"
    );
}

#[test]
fn a_blank_node_label_cannot_be_smuggled_in_under_an_iri() {
    // The digest is the whole safety mechanism, so every spelling that omits it
    // has to miss. A `{"type": "iri"}` term object could otherwise carry a
    // label straight past the blank-node refusal and resolve to the stored
    // node, which is a cross-bundle join on a coincidence of spelling.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", LABELLED_BNODE_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    let body = serde_json::json!({
        "pattern": {"s": {"type": "iri", "value": "_:b1"}, "p": "?p", "o": "?o"},
        "bindings": {"vars": [], "rows": [[]]}
    })
    .to_string();
    let object_form = server.request_with_body(
        "POST",
        "/tox/v/v1/fragment",
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    );
    object_form.assert_status(400);
    assert_eq!(object_form.json()["code"], "bad_term_syntax");

    // The query-carried table cannot reach this at all — SPARQL admits no blank
    // node in `DataBlockValue`, and `<_:b1>`, whose characters do satisfy the
    // IRIREF grammar, is rejected by the parser as not a valid IRI. Pinned so
    // the refusal is known to come from somewhere rather than assumed.
    let values = server.get(&format!(
        "/tox/v/v1/fragment?values={}",
        kgf_server::url::encode_value("(?s) { (<_:b1>) }")
    ));
    values.assert_status(400);
    assert_eq!(values.json()["code"], "malformed_request");

    // And the honest spelling still reaches the node these failed to name.
    let found = server.get("/tox/v/v1/fragment?p=ex%3Atype");
    assert_eq!(found.json()["rows"].as_array().expect("rows").len(), 1);
}

#[test]
fn a_browser_page_spells_a_blank_node_as_one() {
    // `_:` is the one token an RDF reader recognizes without a legend, so the
    // page shows the identity's tail that way — while the link and the tooltip
    // carry the full IRI, which is the spelling any parameter accepts.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    let page = server.request(
        "GET",
        "/tox/v/v1/fragment?p=ex%3Atype",
        &[("Accept", "text/html")],
    );
    page.assert_status(200);
    let html = page.text();
    assert!(
        html.contains("_:s-1"),
        "the page spells the node as a blank node"
    );
    assert!(
        html.contains("urn:fdc:") && html.contains("%3As-1%3E"),
        "and links it by the IRI that actually resolves"
    );
}

#[test]
fn a_tpf_document_without_an_authority_is_a_client_error() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();
    let target = "/tox/v/v1/tpf?predicate=http%3A%2F%2Fexample.org%2Fknows";

    let rdf = server.request_without_host(target, &[("Accept", "text/turtle")]);
    rdf.assert_status(400);
    assert_eq!(rdf.json()["code"], "malformed_request");

    // Native RDF is data-only and therefore needs no absolute control IRIs.
    server
        .request_without_host(
            "/tox/v/v1/fragment?p=ex%3Aknows",
            &[("Accept", "text/turtle")],
        )
        .assert_status(200);
}

#[test]
fn a_trusted_public_origin_drives_hydra_identity_and_continuations() {
    const HYDRA_NEXT: &str = "http://www.w3.org/ns/hydra/core#next";
    const HYDRA_TEMPLATE: &str = "http://www.w3.org/ns/hydra/core#template";

    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve_with_public_base("https://data.example");
    let target = "/tox/v/v1/tpf?predicate=http%3A%2F%2Fexample.org%2Fknows&limit=1";
    let response = server.request_without_host(target, &[("Accept", "text/turtle")]);
    response.assert_status(200);
    let graph: Vec<_> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::Turtle)
        .for_slice(&response.body)
        .collect::<Result<_, _>>()
        .expect("fragment Turtle parses");
    let dataset = "https://data.example/tox/v/v1/tpf";
    let page = format!("https://data.example{target}");

    assert!(graph.iter().any(|quad| {
        matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == page)
            && quad.predicate.as_str() == "http://www.w3.org/ns/hydra/core#totalItems"
    }));
    assert!(graph.iter().any(|quad| {
        quad.predicate.as_str() == HYDRA_NEXT
            && matches!(&quad.object, oxrdf::Term::NamedNode(node) if node.as_str().starts_with(dataset))
    }));
    assert!(graph.iter().any(|quad| {
        quad.predicate.as_str() == HYDRA_TEMPLATE
            && matches!(&quad.object, oxrdf::Term::Literal(value) if value.value() == format!("{dataset}{{?subject,predicate,object}}"))
    }));
}

#[test]
fn a_public_base_with_a_path_prefixes_every_emitted_link() {
    const HYDRA_NEXT: &str = "http://www.w3.org/ns/hydra/core#next";
    const HYDRA_TEMPLATE: &str = "http://www.w3.org/ns/hydra/core#template";

    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    // The gateway matched `/kgf` and removed it before forwarding: every
    // request below arrives without the prefix, as the pod sees it, and every
    // link in the answers must put it back or the client's next request lands
    // on whatever else the shared hostname serves at `/tox`.
    let server = deployment.serve_with_public_base("https://apps.okn.us/kgf");
    let json = [("Accept", "application/json")];

    // The service descriptor's dataset and release links.
    let descriptor = server.request_without_host("/", &json);
    descriptor.assert_status(200);
    let dataset = &descriptor.json()["datasets"][0];
    assert_eq!(dataset["url"], "/kgf/tox");
    for (_, link) in dataset["links"].as_object().unwrap() {
        let link = link.as_str().unwrap();
        assert!(link.starts_with("/kgf/tox/v/v1/"), "{link}");
    }

    // The dataset descriptor's release history.
    let releases = server.request_without_host("/tox", &json);
    releases.assert_status(200);
    assert_eq!(releases.json()["releases"][0]["url"], "/kgf/tox/v/v1/");

    // The `latest` redirect: `Location` is a URL the client will request, and
    // its requests go through the gateway.
    let redirect = server.request_without_host("/tox/latest/summary", &[]);
    redirect.assert_status(307);
    let location = redirect.header("location").expect("a redirect location");
    assert!(location.starts_with("/kgf/tox/v/"), "{location}");

    // A JSON continuation is a cursor token the client appends to the URL it
    // already holds, not a link, so there is nothing to prefix; the check is
    // that it is still a token and not a path the mount would have to touch.
    let target = "/tox/v/v1/fragment?p=ex%3Aknows&limit=1";
    let page_one = server.request_without_host(target, &json);
    page_one.assert_status(200);
    let next = page_one.json()["next"].as_str().unwrap().to_owned();
    assert!(!next.contains('/'), "{next}");

    // The Hydra identities are the whole base followed by the server-seen
    // path: the prefix once, never twice.
    let tpf_target = "/tox/v/v1/tpf?predicate=http%3A%2F%2Fexample.org%2Fknows&limit=1";
    let turtle = server.request_without_host(tpf_target, &[("Accept", "text/turtle")]);
    turtle.assert_status(200);
    let graph: Vec<_> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::Turtle)
        .for_slice(&turtle.body)
        .collect::<Result<_, _>>()
        .expect("fragment Turtle parses");
    let operation = "https://apps.okn.us/kgf/tox/v/v1/tpf";
    let page = format!("https://apps.okn.us/kgf{tpf_target}");
    assert!(graph.iter().any(|quad| {
        matches!(&quad.subject, oxrdf::NamedOrBlankNode::NamedNode(node) if node.as_str() == page)
            && quad.predicate.as_str() == "http://www.w3.org/ns/hydra/core#totalItems"
    }));
    assert!(graph.iter().any(|quad| {
        quad.predicate.as_str() == HYDRA_NEXT
            && matches!(&quad.object, oxrdf::Term::NamedNode(node) if node.as_str().starts_with(operation))
    }));
    assert!(graph.iter().any(|quad| {
        quad.predicate.as_str() == HYDRA_TEMPLATE
            && matches!(&quad.object, oxrdf::Term::Literal(value) if value.value() == format!("{operation}{{?subject,predicate,object}}"))
    }));
    assert!(
        !String::from_utf8_lossy(&turtle.body).contains("/kgf/kgf/"),
        "the prefix was applied twice"
    );

    // The page: the brand link, the form action, and every root-relative link.
    let html = server.request_without_host(target, &[("Accept", "text/html")]);
    html.assert_status(200);
    let text = html.text();
    assert!(text.contains("class=\"brand\" href=\"/kgf/\""));
    assert!(text.contains("action=\"/kgf/tox/v/v1/fragment\""));
    for (href, _) in links(&text) {
        if href.starts_with('/') {
            assert!(
                href.starts_with("/kgf/"),
                "root-relative link escapes the mount: {href}"
            );
        }
    }

    // A problem names the occurrence as the client spelled it, and the
    // descriptor it points at is a URL the client can follow.
    let missing = server.request_without_host("/nope", &json);
    missing.assert_status(404);
    let problem = missing.json();
    assert_eq!(problem["instance"], "/kgf/nope");
    assert!(
        problem["detail"].as_str().unwrap().contains("GET /kgf/ "),
        "{}",
        problem["detail"]
    );
    let no_version = server.request_without_host("/tox/v/v9/manifest", &json);
    no_version.assert_status(404);
    assert!(
        no_version.json()["detail"]
            .as_str()
            .unwrap()
            .contains("GET /kgf/tox "),
        "{}",
        no_version.json()["detail"]
    );

    // The server strips nothing itself: a request that still carries the
    // prefix is a misconfigured gateway, and answering it would give one
    // resource two URLs. It cannot tell that case from a client that doubled
    // the prefix, so it reports the path as received under the mount and says
    // that the prefix was still present.
    let doubled = server.request_without_host("/kgf/tox", &json);
    doubled.assert_status(404);
    let problem = doubled.json();
    assert_eq!(problem["instance"], "/kgf/kgf/tox");
    let detail = problem["detail"].as_str().unwrap();
    assert!(
        detail.contains("already began with the mount prefix"),
        "{detail}"
    );
    assert!(detail.contains("\"/kgf\""), "{detail}");
}

#[test]
fn brtpf_values_accept_undef_and_project_overlapping_rows_to_distinct_rdf() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();
    let values = "(?person ?known) { (<http://example.org/alice> UNDEF) (UNDEF UNDEF) }";
    let mut target = format!(
        "/tox/v/v1/tpf?subject=%3Fperson&predicate=http%3A%2F%2Fexample.org%2Fknows&object=%3Fknown&limit=1&values={}",
        kgf_server::url::encode_value(values)
    );
    let origin = format!("http://{}", server.address);
    let mut knows = HashSet::new();
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages < 10, "the Hydra continuation must terminate");
        let response = server.request("GET", &target, &[("Accept", "text/turtle")]);
        response.assert_status(200);
        let graph: Vec<_> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::Turtle)
            .for_slice(&response.body)
            .collect::<Result<_, _>>()
            .expect("brTPF Turtle parses");
        let total = graph
            .iter()
            .find_map(|quad| {
                (quad.predicate.as_str() == "http://www.w3.org/ns/hydra/core#totalItems")
                    .then(|| match &quad.object {
                        oxrdf::Term::Literal(value) => Some(value.value()),
                        _ => None,
                    })
                    .flatten()
            })
            .expect("the brTPF page carries a cardinality");
        assert_eq!(
            total, "2",
            "a restriction subsumed by another must not inflate the distinct RDF total"
        );
        let page_knows: Vec<_> = graph
            .iter()
            .filter(|quad| quad.predicate.as_str() == "http://example.org/knows")
            .collect();
        assert_eq!(
            page_knows.len(),
            1,
            "overlap filtering must happen before the Hydra page limit"
        );
        for quad in page_knows {
            assert!(
                knows.insert((
                    quad.subject.clone(),
                    quad.predicate.clone(),
                    quad.object.clone(),
                )),
                "an overlapping triple must not reappear on a later RDF page"
            );
        }
        let next = graph.iter().find_map(|quad| {
            (quad.predicate.as_str() == "http://www.w3.org/ns/hydra/core#next")
                .then(|| match &quad.object {
                    oxrdf::Term::NamedNode(node) => Some(node.as_str()),
                    _ => None,
                })
                .flatten()
        });
        let Some(next) = next else { break };
        target = next
            .strip_prefix(&origin)
            .expect("the continuation stays on this endpoint")
            .to_owned();
    }
    assert_eq!(
        knows.len(),
        2,
        "the Alice row overlaps the UNDEF row, but RDF is a set projection"
    );
    assert_eq!(
        pages, 2,
        "a subsumed restriction must not create an empty page"
    );
}

#[test]
fn brtpf_partial_overlap_spends_the_candidate_budget_and_resumes() {
    let deployment = Deployment::new();
    deployment.publish("overlap", "v1", PARTIAL_OVERLAP_NT, "2026-06-01T14:03:22Z");
    let mut budgets = kgf_server::Budgets::new();
    budgets.candidate_budget = 2;
    let server = deployment.serve_with_limits(kgf_server::Caps::new(), budgets);
    let values =
        "(?person ?known) { (<http://example.org/alice> UNDEF) (UNDEF <http://example.org/bob>) }";
    let target = format!(
        "/overlap/v/v1/tpf?subject=%3Fperson&predicate=http%3A%2F%2Fexample.org%2Fknows&object=%3Fknown&limit=2&values={}",
        kgf_server::url::encode_value(values)
    );

    let first = server.request("GET", &target, &[("Accept", "text/turtle")]);
    first.assert_status(200);
    first.assert_header("kgf-complete", "false");
    first.assert_header("kgf-truncation-reason", "candidate_budget");
    let first_graph: Vec<_> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::Turtle)
        .for_slice(&first.body)
        .collect::<Result<_, _>>()
        .expect("the first brTPF page parses");
    assert_eq!(
        first_graph
            .iter()
            .filter(|quad| quad.predicate.as_str() == "http://example.org/knows")
            .count(),
        1
    );
    let next = first_graph
        .iter()
        .find_map(|quad| {
            (quad.predicate.as_str() == "http://www.w3.org/ns/hydra/core#next")
                .then(|| match &quad.object {
                    oxrdf::Term::NamedNode(node) => Some(node.as_str()),
                    _ => None,
                })
                .flatten()
        })
        .expect("candidate exhaustion carries a Hydra continuation");
    let origin = format!("http://{}", server.address);
    let next_target = next
        .strip_prefix(&origin)
        .expect("the continuation stays on this endpoint");

    let second = server.request("GET", next_target, &[("Accept", "text/turtle")]);
    second.assert_status(200);
    second.assert_header("kgf-complete", "true");
    let second_graph: Vec<_> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::Turtle)
        .for_slice(&second.body)
        .collect::<Result<_, _>>()
        .expect("the resumed brTPF page parses");
    assert_eq!(
        second_graph
            .iter()
            .filter(|quad| quad.predicate.as_str() == "http://example.org/knows")
            .count(),
        1
    );
}

#[test]
fn tpf_rdf_byte_fitting_keeps_a_complete_parseable_document_and_cursor() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let unlimited = deployment.serve();
    let target = "/tox/v/v1/tpf?limit=8";
    let full = unlimited.request("GET", target, &[("Accept", "application/n-quads")]);
    full.assert_status(200);
    full.assert_header("kgf-complete", "true");

    let mut budgets = kgf_server::Budgets::new();
    budgets.max_response_bytes = (full.body.len() - 100) as u64;
    let limited = deployment.serve_with_limits(kgf_server::Caps::new(), budgets);
    let response = limited.request("GET", target, &[("Accept", "application/n-quads")]);
    response.assert_status(200);
    response.assert_header("kgf-complete", "false");
    response.assert_header("kgf-truncation-reason", "response_bytes");
    assert!(
        response.body.len() as u64 <= budgets.max_response_bytes,
        "fitted={} budget={} full={}",
        response.body.len(),
        budgets.max_response_bytes,
        full.body.len()
    );
    let graph: Vec<_> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::NQuads)
        .for_slice(&response.body)
        .collect::<Result<_, _>>()
        .expect("the fitted RDF response is still a complete document");
    assert!(
        graph
            .iter()
            .any(|quad| { quad.predicate.as_str() == "http://www.w3.org/ns/hydra/core#next" })
    );
    let data_items = graph
        .iter()
        .filter(|quad| quad.graph_name == oxrdf::GraphName::DefaultGraph)
        .count();
    assert!(data_items < 8, "the byte budget should shorten the page");
    assert!(graph.iter().any(|quad| {
        quad.predicate.as_str() == "http://www.w3.org/ns/hydra/core#itemsPerPage"
            && matches!(&quad.object, oxrdf::Term::Literal(value) if value.value() == "8")
    }));
}

#[test]
#[ignore = "requires npm ci --prefix interop/comunica"]
fn stock_comunica_5_3_queries_the_tpf_endpoint() {
    let deployment = Deployment::new();
    let source = format!(
        "{TINY_NT}<http://example.org/alice> <http://example.org/born> \
         \"1998-04-20\"^^<http://www.w3.org/2001/XMLSchema#date> .\n"
    );
    deployment.publish("tox", "v1", &source, "2026-06-01T14:03:22Z");
    let mut caps = kgf_server::Caps::new();
    caps.default_limit = 1;
    let server = deployment.serve_with(caps);
    let endpoint = format!("http://{}/tox/v/v1/tpf", server.address);
    let remote = Deployment::new();
    remote.publish("remote", "v1", REMOTE_NT, "2026-06-01T14:03:22Z");
    let remote_server = remote.serve_with(caps);
    let remote_endpoint = format!("http://{}/remote/v/v1/tpf", remote_server.address);
    let quads = Deployment::new();
    quads.publish_quads("quads", "v1", WORKED_EXAMPLE_NQ, "2026-06-01T14:03:22Z");
    let quads_server = quads.serve_with(caps);
    let quads_endpoint = format!("http://{}/quads/v/v1/tpf", quads_server.address);
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("interop/comunica/test.mjs");
    let status = Command::new("node")
        .arg(script)
        .arg(endpoint)
        .arg(remote_endpoint)
        .arg(quads_endpoint)
        .status()
        .expect("run Node.js; install it and run npm ci --prefix interop/comunica first");
    assert!(
        status.success(),
        "the stock Comunica conformance script failed"
    );
}

#[test]
fn void_and_summary_serve_the_published_description_in_every_format() {
    let deployment = Deployment::new();
    deployment.publish_description("tox", "v1", "2026-08-08T12:00:00Z");
    let server = deployment.serve();

    let turtle = server.get("/tox/v/v1/void?format=ttl");
    turtle.assert_status(200);
    turtle.assert_header("content-type", "text/turtle; charset=utf-8");
    assert!(
        turtle.header("x-robots-tag").is_none(),
        "a description document narrows nothing and stays discoverable"
    );
    turtle.assert_header("kgf-complete", "true");
    assert!(String::from_utf8_lossy(&turtle.body).contains("@prefix kgfbn:"));
    let turtle_quads = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::Turtle)
        .for_slice(&turtle.body)
        .collect::<Result<Vec<_>, _>>()
        .expect("/void Turtle parses");
    assert_eq!(turtle_quads.len(), 24);

    let jsonld = server.request(
        "GET",
        "/tox/v/v1/void",
        &[("Accept", "application/ld+json")],
    );
    jsonld.assert_status(200);
    jsonld.assert_header("content-type", "application/ld+json");
    let format = oxrdfio::RdfFormat::from_media_type("application/ld+json").unwrap();
    let jsonld_quads = oxrdfio::RdfParser::from_format(format)
        .for_slice(&jsonld.body)
        .collect::<Result<Vec<_>, _>>()
        .expect("/void JSON-LD parses");
    assert_eq!(jsonld_quads.len(), turtle_quads.len());

    let pinned_jsonld = server.get("/tox/v/v1/void?format=jsonld");
    pinned_jsonld.assert_status(200);
    pinned_jsonld.assert_header("content-type", "application/ld+json");

    let page = server.get("/tox/v/v1/void?format=html");
    page.assert_status(200);
    page.assert_header("content-type", "text/html; charset=utf-8");
    let page = String::from_utf8(page.body).unwrap();
    assert!(page.contains("VoID dataset description"));
    assert!(page.contains("View JSON-LD"));
    assert!(page.contains("/tox/v/v1/void?format=jsonld"));
    assert!(!page.contains("/tox/v/v1/void?format=json\""));

    let markdown = server.get("/tox/v/v1/summary");
    markdown.assert_status(200);
    markdown.assert_header("content-type", "text/markdown; charset=utf-8");
    assert!(
        markdown.header("x-robots-tag").is_none(),
        "a description document narrows nothing and stays discoverable"
    );
    assert_eq!(markdown.body, b"# Summary\n");

    let json = server.get("/tox/v/v1/summary?format=json");
    json.assert_status(200);
    json.assert_header("content-type", "application/json");
    assert!(json.json().is_object());

    let summary_page = server.request("GET", "/tox/v/v1/summary", &[("Accept", "text/html")]);
    summary_page.assert_status(200);
    let summary_page = String::from_utf8(summary_page.body).unwrap();
    for expected in [
        "Fixture graph",
        "Browse classes",
        "Top classes",
        "Top properties",
        "Leading typed class relations",
        "ex:A",
        "ex:p",
        "schema?children=properties",
    ] {
        assert!(
            summary_page.contains(expected),
            "rich summary page is missing {expected:?}"
        );
    }

    let manifest = server.get("/tox/v/v1/manifest?format=html");
    manifest.assert_status(200);
    let manifest = String::from_utf8(manifest.body).unwrap();
    for operation in ["schema", "void", "summary"] {
        assert!(manifest.contains(&format!("href=\"/tox/v/v1/{operation}\"")));
    }
}

/// The graph scope over the wire: gated on the capability before any bundle
/// opens, answerable in its reserved forms on every release, and paged
/// through the quad view with the cursor the envelope carries.
#[test]
fn graph_scope_is_gated_on_the_capability_and_pages_over_the_wire() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-01-09T09:00:00Z");
    deployment.publish_quads("quads", "v1", WORKED_EXAMPLE_NQ, "2026-01-09T09:00:00Z");
    let server = deployment.serve();
    let g1 = kgf_server::url::encode_value("<http://example.org/g1>");
    let unnamed = kgf_server::url::encode_value("<urn:x-kgf:unnamed>");

    // No memberships: a named graph and the quad view are 501 before the
    // open; the two reserved names answer.
    for target in [
        format!("/tox/v/v1/fragment?g={g1}"),
        format!("/tox/v/v1/count?g={g1}"),
        "/tox/v/v1/fragment?g=*".to_owned(),
    ] {
        let refused = server.request("GET", &target, &[]);
        refused.assert_status(501);
        assert_eq!(
            refused.json()["code"],
            "capability_not_available",
            "{target}"
        );
    }
    let whole = server.request("GET", "/tox/v/v1/count", &[]).json()["count"]["value"]
        .as_u64()
        .unwrap();
    let unnamed_count = server.request("GET", &format!("/tox/v/v1/count?g={unnamed}"), &[]);
    unnamed_count.assert_status(200);
    assert_eq!(unnamed_count.json()["count"]["value"], whole);

    // The capability is declared for the quad bundle, and every form answers.
    let manifest = server.request("GET", "/quads/v/v1/manifest", &[]).json();
    assert!(
        manifest["capabilities"]
            .as_object()
            .unwrap()
            .contains_key("graphs"),
        "{manifest}"
    );
    let count = server.request("GET", &format!("/quads/v/v1/count?g={g1}"), &[]);
    count.assert_status(200);
    assert_eq!(count.json()["count"]["value"], 2);
    let quads = server.request("GET", "/quads/v/v1/count?g=*", &[]);
    assert_eq!(quads.json()["count"]["value"], 5);

    // Paged at one row: five pages, each resumed from the previous cursor,
    // including the boundaries inside a triple's run.
    let mut target = "/quads/v/v1/fragment?g=*&limit=1".to_owned();
    let mut graphs = Vec::new();
    loop {
        let page = server.request("GET", &target, &[]);
        page.assert_status(200);
        let body = page.json();
        assert_eq!(body["vars"], serde_json::json!(["s", "p", "o", "g"]));
        graphs.push(body["rows"][0]["g"]["value"].as_str().unwrap().to_owned());
        match body["next"].as_str() {
            Some(next) => target = format!("/quads/v/v1/fragment?g=*&limit=1&cursor={next}"),
            None => break,
        }
    }
    assert_eq!(
        graphs,
        [
            "urn:x-kgf:unnamed",
            "http://example.org/g1",
            "urn:x-kgf:unnamed",
            "http://example.org/g1",
            "http://example.org/g2",
        ]
    );

    // The browser form offers the control only where it can be answered.
    let form = server
        .request("GET", "/quads/v/v1/fragment", &[("accept", "text/html")])
        .text();
    assert!(
        form.contains("name=\"g\""),
        "the quad bundle offers a graph control"
    );
    let form = server
        .request("GET", "/tox/v/v1/fragment", &[("accept", "text/html")])
        .text();
    assert!(
        !form.contains("name=\"g\""),
        "a bundle without memberships does not"
    );

    // The listing is routed and linked only where the capability is declared.
    let refused = server.request("GET", "/tox/v/v1/graphs", &[]);
    refused.assert_status(501);
    assert_eq!(refused.json()["code"], "capability_not_available");
    let listing = server.request("GET", "/quads/v/v1/graphs", &[]);
    listing.assert_status(200);
    assert_eq!(listing.json()["graphs"].as_array().unwrap().len(), 3);
    let page = server.request("GET", "/quads/v/v1/graphs", &[("accept", "text/html")]);
    page.assert_status(200);
    assert!(page.text().contains("urn:x-kgf:unnamed"));
    let links = |dataset: &str| {
        server.request("GET", &format!("/{dataset}"), &[]).json()["releases"][0]["links"].clone()
    };
    assert_eq!(links("quads")["graphs"], "/quads/v/v1/graphs");
    assert!(links("tox").get("graphs").is_none());
}

/// The TPF route over a bundle with memberships: the four-position Hydra
/// form with the union declared as the default graph, and the serving rule
/// for every form of `graph` — the quad view with unnamed statements
/// untagged, the union constant untagged like an absent `graph` but one row
/// per distinct triple, and a named graph or the unnamed constant tagging
/// with itself — with `hydra:totalItems` the count of the view requested.
#[test]
fn tpf_serves_the_quad_view_and_scoped_views_by_the_serving_table() {
    const HYDRA: &str = "http://www.w3.org/ns/hydra/core#";
    const SD: &str = "http://www.w3.org/ns/sparql-service-description#";
    const UNION: &str = "urn:x-kgf:union";
    const UNNAMED: &str = "urn:x-kgf:unnamed";

    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    deployment.publish_quads("quads", "v1", WORKED_EXAMPLE_NQ, "2026-06-01T14:03:22Z");
    let server = deployment.serve();
    let nquads = |target: &str| -> Vec<oxrdf::Quad> {
        let response = server.request("GET", target, &[("Accept", "application/n-quads")]);
        response.assert_status(200);
        oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::NQuads)
            .for_slice(&response.body)
            .collect::<Result<_, _>>()
            .unwrap_or_else(|error| panic!("{target} did not parse: {error}"))
    };
    // Data statements: everything outside the page's metadata graph.
    let data = |quads: &[oxrdf::Quad]| -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = quads
            .iter()
            .filter(|quad| {
                !matches!(&quad.graph_name, oxrdf::GraphName::NamedNode(node)
                    if node.as_str().ends_with("#metadata"))
            })
            .map(|quad| {
                let graph = match &quad.graph_name {
                    oxrdf::GraphName::DefaultGraph => String::new(),
                    oxrdf::GraphName::NamedNode(node) => node.as_str().to_owned(),
                    other => panic!("unexpected graph name {other}"),
                };
                (quad.object.to_string(), graph)
            })
            .collect();
        rows.sort();
        rows
    };
    let total_items = |quads: &[oxrdf::Quad]| -> u64 {
        quads
            .iter()
            .find(|quad| quad.predicate.as_str() == format!("{HYDRA}totalItems"))
            .and_then(|quad| match &quad.object {
                oxrdf::Term::Literal(literal) => literal.value().parse().ok(),
                _ => None,
            })
            .expect("hydra:totalItems")
    };
    let mappings = |quads: &[oxrdf::Quad]| -> Vec<String> {
        let mut properties: Vec<String> = quads
            .iter()
            .filter(|quad| quad.predicate.as_str() == format!("{HYDRA}property"))
            .map(|quad| quad.object.to_string())
            .collect();
        properties.sort();
        properties
    };
    let default_graph = |quads: &[oxrdf::Quad]| -> Option<(bool, String)> {
        quads
            .iter()
            .find(|quad| quad.predicate.as_str() == format!("{SD}defaultGraph"))
            .map(|quad| {
                (
                    matches!(quad.subject, oxrdf::NamedOrBlankNode::BlankNode(_)),
                    quad.object.to_string(),
                )
            })
    };

    // The graph-unbound quad view: five memberships, layer 0 untagged.
    let unbound = nquads("/quads/v/v1/tpf?limit=10");
    assert_eq!(total_items(&unbound), 5);
    assert_eq!(
        data(&unbound),
        vec![
            ("<http://example.org/c>".to_owned(), String::new()),
            (
                "<http://example.org/c>".to_owned(),
                "http://example.org/g1".to_owned()
            ),
            ("<http://example.org/d>".to_owned(), String::new()),
            (
                "<http://example.org/z>".to_owned(),
                "http://example.org/g1".to_owned()
            ),
            (
                "<http://example.org/z>".to_owned(),
                "http://example.org/g2".to_owned()
            ),
        ]
    );
    assert_eq!(
        mappings(&unbound),
        vec![
            "<http://www.w3.org/1999/02/22-rdf-syntax-ns#object>",
            "<http://www.w3.org/1999/02/22-rdf-syntax-ns#predicate>",
            "<http://www.w3.org/1999/02/22-rdf-syntax-ns#subject>",
            "<http://www.w3.org/ns/sparql-service-description#graph>",
        ],
        "a bundle with memberships publishes the four-position form"
    );
    assert_eq!(
        default_graph(&unbound),
        Some((true, format!("<{UNION}>"))),
        "the union is declared as the default graph under a blank-node subject"
    );
    assert!(unbound.iter().any(|quad| {
        quad.predicate.as_str() == format!("{HYDRA}template")
            && quad
                .object
                .to_string()
                .contains("{?subject,predicate,object,graph}")
    }));
    // A variable, as a bindings-restricted client sends it, is the same view.
    assert_eq!(
        data(&nquads("/quads/v/v1/tpf?graph=%3Fg&limit=10")),
        data(&unbound)
    );

    // The union constant: each triple once, in the document's default graph
    // like the rows a bare pattern reads, never tagged with the constant.
    let union = nquads(&format!("/quads/v/v1/tpf?graph={UNION}&limit=10"));
    assert_eq!(total_items(&union), 3);
    assert!(
        data(&union).iter().all(|(_, graph)| graph.is_empty()) && data(&union).len() == 3,
        "{:?}",
        data(&union)
    );

    // A named graph and the unnamed graph, tagged with themselves.
    let g1 = nquads("/quads/v/v1/tpf?graph=http%3A%2F%2Fexample.org%2Fg1&limit=10");
    assert_eq!(total_items(&g1), 2);
    assert_eq!(
        data(&g1),
        vec![
            (
                "<http://example.org/c>".to_owned(),
                "http://example.org/g1".to_owned()
            ),
            (
                "<http://example.org/z>".to_owned(),
                "http://example.org/g1".to_owned()
            ),
        ]
    );
    let unnamed = nquads(&format!("/quads/v/v1/tpf?graph={UNNAMED}&limit=10"));
    assert_eq!(total_items(&unnamed), 2);
    assert!(data(&unnamed).iter().all(|(_, graph)| graph == UNNAMED));

    // TriG names graphs too, so the quad view serializes in it unchanged.
    let trig = server.request(
        "GET",
        "/quads/v/v1/tpf?limit=10",
        &[("Accept", "application/trig")],
    );
    trig.assert_status(200);
    let parsed: Vec<oxrdf::Quad> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::TriG)
        .for_slice(&trig.body)
        .collect::<Result<_, _>>()
        .expect("TriG parses");
    assert_eq!(data(&parsed), data(&unbound));

    // Every form pages to the same rows one row at a time, following the
    // `hydra:next` link of each page — which carries the scope it was issued
    // under, and for the quad view a run trailer that resumes inside one
    // triple's memberships.
    let origin = format!("http://{}", server.address);
    let next_link = |quads: &[oxrdf::Quad]| -> Option<String> {
        quads
            .iter()
            .find(|quad| quad.predicate.as_str() == format!("{HYDRA}next"))
            .and_then(|quad| match &quad.object {
                oxrdf::Term::NamedNode(node) => Some(node.as_str().to_owned()),
                _ => None,
            })
    };
    let walk = |target: &str| -> Vec<(String, String)> {
        let mut collected = Vec::new();
        let mut next = Some(target.to_owned());
        let mut pages = 0;
        while let Some(target) = next {
            pages += 1;
            assert!(pages < 20, "{target} did not terminate");
            let page = nquads(&target);
            collected.extend(data(&page));
            next = next_link(&page).map(|link| {
                let path = link
                    .strip_prefix(&origin)
                    .unwrap_or_else(|| panic!("a page link addresses this server: {link}"));
                path.to_owned()
            });
        }
        collected.sort();
        collected
    };
    for scope in [
        String::new(),
        format!("graph={UNION}&"),
        "graph=http%3A%2F%2Fexample.org%2Fg1&".to_owned(),
        format!("graph={UNNAMED}&"),
    ] {
        assert_eq!(
            walk(&format!("/quads/v/v1/tpf?{scope}limit=1")),
            data(&nquads(&format!("/quads/v/v1/tpf?{scope}limit=10"))),
            "paging {scope:?} one row at a time"
        );
    }

    // Turtle can carry one graph: the union and scoped views, not the quad view.
    let refused = server.request(
        "GET",
        "/quads/v/v1/tpf?limit=10",
        &[("Accept", "text/turtle")],
    );
    refused.assert_status(406);
    assert_eq!(refused.json()["code"], "not_acceptable");
    let turtle = server.request(
        "GET",
        &format!("/quads/v/v1/tpf?graph={UNION}&limit=10"),
        &[("Accept", "text/turtle")],
    );
    turtle.assert_status(200);
    let triples: Vec<_> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::Turtle)
        .for_slice(&turtle.body)
        .collect::<Result<_, _>>()
        .expect("Turtle parses");
    assert_eq!(
        triples
            .iter()
            .filter(|quad| quad.predicate.as_str() == "http://example.org/b"
                || quad.predicate.as_str() == "http://example.org/y")
            .count(),
        3
    );

    // A bindings-restricted request keeps the same rule.
    let values = kgf_server::url::encode_value("(?s) { (<http://example.org/x>) }");
    let restricted = nquads(&format!(
        "/quads/v/v1/tpf?subject=%3Fs&graph=%3Fg&values={values}&limit=10"
    ));
    assert_eq!(
        data(&restricted),
        vec![
            (
                "<http://example.org/z>".to_owned(),
                "http://example.org/g1".to_owned()
            ),
            (
                "<http://example.org/z>".to_owned(),
                "http://example.org/g2".to_owned()
            ),
        ]
    );
    let bound_graph = server.request(
        "GET",
        &format!(
            "/quads/v/v1/tpf?subject=%3Fs&graph=%3Fg&values={}&limit=10",
            kgf_server::url::encode_value(
                "(?s ?g) { (<http://example.org/x> <http://example.org/g1>) }"
            )
        ),
        &[("Accept", "application/n-quads")],
    );
    bound_graph.assert_status(400);

    // Without memberships: the three-position form, no default-graph
    // declaration, and every statement untagged.
    let plain = nquads("/tox/v/v1/tpf?limit=10");
    assert_eq!(mappings(&plain).len(), 3);
    assert_eq!(default_graph(&plain), None);
    assert!(data(&plain).iter().all(|(_, graph)| graph.is_empty()));
    let refused = server.request(
        "GET",
        "/tox/v/v1/tpf?graph=http%3A%2F%2Fexample.org%2Fg1&limit=10",
        &[("Accept", "application/n-quads")],
    );
    refused.assert_status(501);
}

#[test]
fn a_versioned_manifest_is_immutable_cacheable_and_conditional() {
    let deployment = Deployment::new();
    deployment.publish("tox", "2026-06-01", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    let manifest = server.get("/tox/v/2026-06-01/manifest");
    manifest.assert_status(200);
    manifest.assert_cache_control(&["public", "max-age=31536000", "immutable"]);
    manifest.assert_varies_on_accept();
    manifest.assert_header("content-type", "application/json");
    assert_eq!(manifest.json()["version"], "2026-06-01");

    // The ETag covers the complete immutable publication profile, including
    // prefixes and predicate roles that affect versioned request semantics,
    // and remains representation-specific.
    let publication_digest = format!("sha256:{:x}", Sha256::digest(&manifest.body));
    let etag = manifest
        .header("etag")
        .expect("a versioned GET carries an ETag");
    assert!(
        etag.contains(&publication_digest),
        "{etag} must identify the publication"
    );
    assert!(etag.contains("json"), "{etag} must identify the format");

    // A conditional GET is answered without the body.
    let unchanged = server.request(
        "GET",
        "/tox/v/2026-06-01/manifest",
        &[("If-None-Match", etag.as_str())],
    );
    unchanged.assert_status(304);
    assert!(unchanged.body.is_empty(), "a 304 carries no body");
    unchanged.assert_header("etag", etag.as_str());

    // A stale validator is not honoured.
    server
        .request(
            "GET",
            "/tox/v/2026-06-01/manifest",
            &[("If-None-Match", "\"something-else\"")],
        )
        .assert_status(200);

    // The same URL under a different representation is a different entity, so
    // the JSON validator must not match the page.
    let page = server.request(
        "GET",
        "/tox/v/2026-06-01/manifest",
        &[("Accept", "text/html"), ("If-None-Match", etag.as_str())],
    );
    page.assert_status(200);
    assert_ne!(page.header("etag"), Some(etag));
}

#[test]
fn latest_redirects_to_the_current_version_and_keeps_the_method() {
    let deployment = Deployment::new();
    deployment.publish("tox", "2026-01-09", TINY_NT, "2026-01-09T09:00:00Z");
    deployment.publish("tox", "2026-06-01", GROWN_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    let redirect = server.get("/tox/latest/manifest");
    // 307, not 302: a 302 may be rewritten to GET by an intermediary, which
    // would silently turn a body-carrying QUERY into something else.
    redirect.assert_status(307);
    redirect.assert_header("location", "/tox/v/2026-06-01/manifest");
    redirect.assert_cache_control(&["public", "max-age=300"]);

    // The query string survives, so a paging URL rebuilt against `latest`
    // arrives at the version with its parameters intact.
    server
        .get("/tox/latest/manifest?format=json&p=rdfs%3Alabel")
        .assert_header(
            "location",
            "/tox/v/2026-06-01/manifest?format=json&p=rdfs%3Alabel",
        );

    // And the redirect is method-preserving for the method M1 does not yet
    // route: a QUERY to `latest` is redirected rather than rejected.
    let query = server.request("QUERY", "/tox/latest/manifest", &[]);
    query.assert_status(307);
    query.assert_header("location", "/tox/v/2026-06-01/manifest");

    // Following it lands on the current release.
    let followed = server.get(&redirect.header("location").unwrap());
    followed.assert_status(200);
    assert_eq!(followed.json()["version"], "2026-06-01");
}

#[test]
fn an_extension_method_reaches_the_router_with_its_name_intact() {
    // A resource that does not accept QUERY still has to receive the method
    // intact in order to name it in its coded 405.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    for method in ["QUERY", "POST", "PUT", "DELETE"] {
        let response = server.request(method, "/tox/v/v1/manifest", &[]);
        response.assert_status(405);
        assert_eq!(response.json()["code"], "method_not_allowed");
        assert!(
            response.json()["detail"].as_str().unwrap().contains(method),
            "the problem must name the method that was refused",
        );
        // Every error carries a code; RFC 9110 §15.5.6 says a 405
        // carries `Allow`. Both, not one.
        let allow = response.header("allow").expect("405 requires Allow");
        assert!(allow.contains("GET"), "{allow}");
    }

    // HEAD is not a fifth case: it is GET without the body, and the router
    // must answer it as such.
    let head = server.request("HEAD", "/tox/v/v1/manifest", &[]);
    head.assert_status(200);
    head.assert_header("content-type", "application/json");
    assert!(head.body.is_empty(), "a HEAD response carries no body");
}

#[test]
fn bindings_query_and_post_answer_over_the_wire() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();
    let path = "/tox/v/v1/fragment";
    let mut body = serde_json::json!({
        "pattern": {"s": "?person", "p": "ex:knows", "o": "?known"},
        "bindings": {"vars": ["?person"], "rows": [["ex:alice"], ["ex:bob"]]},
        "limit": 1
    });
    let encoded = serde_json::to_vec(&body).unwrap();

    let query = server.request_with_body(
        "QUERY",
        path,
        &[("Content-Type", "application/json")],
        &encoded,
    );
    query.assert_status(200);
    query.assert_header("accept-query", "application/json");
    query.assert_cache_control(&["public", "max-age=31536000", "immutable"]);
    assert_eq!(query.json()["rows"][0]["binding"], 0);
    let next = query.json()["next"].as_str().unwrap().to_owned();
    let etag = query
        .header("etag")
        .expect("a QUERY response carries an ETag");
    let unchanged = server.request_with_body(
        "QUERY",
        path,
        &[
            ("Content-Type", "application/json"),
            ("If-None-Match", etag.as_str()),
        ],
        &encoded,
    );
    unchanged.assert_status(304);
    unchanged.assert_header("accept-query", "application/json");

    body["cursor"] = serde_json::json!(next);
    let resumed_body = serde_json::to_vec(&body).unwrap();
    let resumed = server.request_with_body(
        "QUERY",
        path,
        &[("Content-Type", "application/json")],
        &resumed_body,
    );
    resumed.assert_status(200);
    assert_eq!(resumed.json()["rows"][0]["binding"], 1);
    assert_eq!(resumed.json()["complete"], true);

    let post = server.request_with_body(
        "POST",
        path,
        &[("Content-Type", "application/json; charset=utf-8")],
        &encoded,
    );
    post.assert_status(200);
    post.assert_cache_control(&["no-store"]);
    assert_eq!(post.json()["rows"][0], query.json()["rows"][0]);

    // Cache policy does not switch off request preconditions. The same
    // operation has the same representation under QUERY and POST, but RFC
    // 9110 §13.1.2 requires a false If-None-Match on POST to answer 412 rather
    // than QUERY's 304.
    let refused_post = server.request_with_body(
        "POST",
        path,
        &[
            ("Content-Type", "application/json; charset=utf-8"),
            ("If-None-Match", etag.as_str()),
        ],
        &encoded,
    );
    refused_post.assert_status(412);
    refused_post.assert_cache_control(&["no-store"]);
    refused_post.assert_header("accept-query", "application/json");
    assert_eq!(refused_post.json()["code"], "precondition_failed");

    let count_body = serde_json::json!({
        "pattern": {"s": "?person", "p": "ex:knows", "o": "?known"},
        "bindings": {"vars": ["?person"], "rows": [["ex:alice"], ["ex:bob"]]}
    });
    let count_encoded = serde_json::to_vec(&count_body).unwrap();
    let counted = server.request_with_body(
        "QUERY",
        "/tox/v/v1/count",
        &[("Content-Type", "application/json")],
        &count_encoded,
    );
    counted.assert_status(200);
    assert_eq!(
        counted.json()["counts"],
        serde_json::json!([
            {"binding": 0, "count": {"value": 1, "exact": true}},
            {"binding": 1, "count": {"value": 1, "exact": true}}
        ])
    );

    // `/fragment` and `/count` both accept this exact body, but produce
    // different representations. Their strong validators must therefore be
    // different, and a fragment validator must not suppress a count response.
    let same_body_fragment = server.request_with_body(
        "QUERY",
        path,
        &[("Content-Type", "application/json")],
        &count_encoded,
    );
    same_body_fragment.assert_status(200);
    let fragment_etag = same_body_fragment.header("etag").unwrap();
    assert_ne!(counted.header("etag"), Some(fragment_etag.clone()));
    server
        .request_with_body(
            "QUERY",
            "/tox/v/v1/count",
            &[
                ("Content-Type", "application/json"),
                ("If-None-Match", fragment_etag.as_str()),
            ],
            &count_encoded,
        )
        .assert_status(200);

    let wrong_type =
        server.request_with_body("QUERY", path, &[("Content-Type", "text/plain")], &encoded);
    wrong_type.assert_status(415);
    wrong_type.assert_header("accept-query", "application/json");
    assert_eq!(wrong_type.json()["code"], "unsupported_media_type");

    server
        .get(path)
        .assert_header("accept-query", "application/json");
    let refused = server.request("PUT", path, &[]);
    refused.assert_status(405);
    assert!(refused.header("allow").unwrap().contains("QUERY"));
    server
        .request("PUT", &format!("{path}?bad=%"), &[])
        .assert_status(405);
}

#[test]
fn search_and_labels_answer_over_the_wire_without_a_language_parameter() {
    let deployment = Deployment::new();
    deployment.publish_text("tox", "v1", GROWN_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    let searched = server.get("/tox/v/v1/search?q=Alice&predicate=ex%3Aname&labels=true&limit=20");
    searched.assert_status(200);
    assert_eq!(searched.json()["results"].as_array().unwrap().len(), 1);
    assert_eq!(
        searched.json()["results"][0]["subject"]["value"],
        "http://example.org/alice"
    );
    assert_eq!(searched.json()["results"][0]["label"], "Alice");
    assert_eq!(
        searched.json()["results"][0]["match"]["predicate"],
        "http://example.org/name"
    );
    server
        .get("/tox/v/v1/search?q=Alice&role=&predicate=&labels=true&limit=")
        .assert_status(200);
    server
        .get("/tox/v/v1/search?q=&role=&predicate=&labels=true&limit=")
        .assert_status(400);
    server
        .get("/tox/v/v1/search?q=Alice&labels=&limit=")
        .assert_status(400);
    let search_page = server.request(
        "GET",
        "/tox/v/v1/search?q=Alice&predicate=ex%3Aname",
        &[("Accept", "text/html")],
    );
    search_page.assert_status(200);
    search_page.assert_header("content-type", "text/html; charset=utf-8");
    let search_html = search_page.text();
    assert!(search_html.contains("<h1>“Alice”</h1>"), "{search_html}");
    assert!(search_html.contains("Search · tox v1"), "{search_html}");
    assert!(
        search_html.find("Search · tox v1") < search_html.find("<h1>“Alice”</h1>"),
        "{search_html}"
    );
    assert!(
        search_html.contains("<div class=\"field\"><dt>query</dt><dd>Alice</dd></div>"),
        "{search_html}"
    );
    assert!(search_html.contains(">ex:alice<"), "{search_html}");
    // `labels` defaults on, and the page sets the resolved label under the
    // subject term rather than in a column of its own.
    assert!(
        search_html.contains("<span class=\"t-label\">Alice</span>"),
        "{search_html}"
    );
    assert!(search_html.contains(">ex:name</a>"), "{search_html}");
    assert!(search_html.contains("<dt>predicates</dt>"), "{search_html}");
    assert!(!search_html.contains("all predicates"), "{search_html}");

    // Locale is not part of the operation: labels are the release's stable
    // display labels rather than a per-request localization service.
    let with_lang = server.get("/tox/v/v1/search?q=Alice&lang=en");
    with_lang.assert_status(400);
    assert_eq!(with_lang.json()["code"], "malformed_request");

    let body = serde_json::to_vec(&serde_json::json!({
        "iris": ["ex:bob", "ex:missing"]
    }))
    .unwrap();
    let labeled = server.request_with_body(
        "QUERY",
        "/tox/v/v1/labels",
        &[("Content-Type", "application/json")],
        &body,
    );
    labeled.assert_status(200);
    assert_eq!(
        labeled.json()["labels"],
        serde_json::json!([
            {"iri": {"type": "iri", "value": "http://example.org/bob"}, "label": "Bob"},
            {"iri": {"type": "iri", "value": "http://example.org/missing"}, "label": null},
        ])
    );
    assert_eq!(
        labeled.header("accept-query").as_deref(),
        Some("application/json")
    );
    let labels_page = server.request_with_body(
        "QUERY",
        "/tox/v/v1/labels",
        &[
            ("Content-Type", "application/json"),
            ("Accept", "text/html"),
        ],
        &body,
    );
    labels_page.assert_status(200);
    labels_page.assert_header("content-type", "text/html; charset=utf-8");

    let empty_body = serde_json::to_vec(&serde_json::json!({"iris": []})).unwrap();
    let empty_page = server.request_with_body(
        "QUERY",
        "/tox/v/v1/labels",
        &[
            ("Content-Type", "application/json"),
            ("Accept", "text/html"),
        ],
        &empty_body,
    );
    empty_page.assert_status(200);
    let empty_html = empty_page.text();
    assert!(
        empty_html.contains("No IRIs were submitted."),
        "{empty_html}"
    );
    assert!(!empty_html.contains("response budget"), "{empty_html}");
}

#[test]
fn terms_pages_the_dictionary_and_counts_it_over_the_wire() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", GROWN_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    // Discoverable from the service descriptor rather than guessed at, and
    // browsable with no parameters at all: an empty prefix is the first page of
    // the whole dictionary.
    let descriptor = server.get("/").json();
    assert_eq!(
        descriptor["datasets"][0]["links"]["terms"],
        "/tox/v/v1/terms"
    );
    server.get("/tox/v/v1/terms").assert_status(200);

    let page = server.get("/tox/v/v1/terms?prefix=http%3A%2F%2Fexample.org%2F&role=any&limit=2");
    page.assert_status(200);
    page.assert_header("content-type", "application/json");
    page.assert_cache_control(&["public", "max-age=31536000", "immutable"]);
    let body = page.json();
    assert_eq!(
        body["terms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["term"]["value"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["http://example.org/alice", "http://example.org/bob"]
    );
    assert_eq!(body["terms"][0]["roles"], serde_json::json!(["subject"]));
    assert_eq!(
        body["terms"][1]["roles"],
        serde_json::json!(["subject", "object"])
    );
    assert_eq!(body["terms"][0]["label"], "Alice");
    // The page carries the size of the scan it is paging through.
    assert_eq!(
        body["cardinality"],
        serde_json::json!({"value": 5, "exact": true})
    );
    page.assert_header("kgf-complete", "false");
    page.assert_header("kgf-truncation-reason", "page_limit");
    page.assert_header(
        "kgf-next-cursor",
        body["next"].as_str().expect("a cursor in the body"),
    );

    // The count is the whole answer, so it says so in both channels and offers
    // nothing to continue.
    let counted = server.get("/tox/v/v1/terms?prefix=http%3A%2F%2Fexample.org%2F&count=true");
    counted.assert_status(200);
    counted.assert_header("kgf-complete", "true");
    assert!(counted.header("kgf-next-cursor").is_none());
    assert_eq!(
        counted.json()["count"],
        serde_json::json!({"value": 5, "exact": true}),
        "three subjects and two predicates under the prefix"
    );
    // Every position, in one request: the fan-out probe this operation exists
    // for wants to know *how* a namespace is used, not only whether it is.
    // `bob` is the one term under the prefix in both positions, so it is stored
    // once and `any` is five rather than the six the positions add up to.
    assert_eq!(
        counted.json()["counts"],
        serde_json::json!({"subject": 3, "predicate": 2, "object": 1, "any": 5})
    );

    // Empty controls are how a browser submits an untouched form, and each of
    // these selects the same default as its omission.
    server
        .get("/tox/v/v1/terms?prefix=&role=&limit=&labels=")
        .assert_status(200);

    // And the same URL is a page in a browser.
    let html = server.request(
        "GET",
        "/tox/v/v1/terms?prefix=http%3A%2F%2Fexample.org%2F&role=any&limit=2",
        &[("Accept", "text/html")],
    );
    html.assert_status(200);
    html.assert_header("content-type", "text/html; charset=utf-8");
    let text = html.text();
    assert!(text.contains("<h1>“http://example.org/…”</h1>"), "{text}");
    assert!(text.contains("Terms · tox v1"), "{text}");
    // The term is a link into its own neighborhood, and the label the request
    // asked for sits under it rather than in a column of its own.
    assert!(
        text.contains(">ex:alice<span class=\"t-label\">Alice</span></a>"),
        "{text}"
    );
    assert!(text.contains("How many in total?"), "{text}");

    // A manifest that predates the operation does not withdraw it: see
    // `an_operation_needing_no_sidecar_is_not_gated_on_its_declaration`.
}

#[test]
fn one_url_serves_a_page_to_a_browser_and_data_to_everything_else() {
    const BROWSER: &str =
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";

    let deployment = Deployment::new();
    deployment.publish("tox", "2026-06-01", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    for path in ["/", "/tox", "/tox/v/2026-06-01/manifest"] {
        // What `curl` and every library send.
        let data = server.request("GET", path, &[("Accept", "*/*")]);
        data.assert_status(200);
        data.assert_header("content-type", "application/json");
        data.assert_varies_on_accept();
        serde_json::from_slice::<serde_json::Value>(&data.body)
            .unwrap_or_else(|error| panic!("{path} must answer JSON: {error}"));

        // What a browser sends.
        let page = server.request("GET", path, &[("Accept", BROWSER)]);
        page.assert_status(200);
        page.assert_header("content-type", "text/html; charset=utf-8");
        page.assert_varies_on_accept();
        let text = page.text();
        // Case-insensitive: `<!DOCTYPE html>` and `<!doctype html>` are the
        // same declaration, and which one the template engine emits is not a
        // property of this server.
        assert!(
            text.to_ascii_lowercase().starts_with("<!doctype html>"),
            "{path} must answer HTML"
        );
        assert!(text.contains("Knowledge Graph Fragments"), "{path}");
        // Every page links back to its own machine-readable form, which is what
        // makes the browser a way into the API rather than a separate product.
        assert!(text.contains("format=json"), "{path}");

        // And either can be pinned, so a link on the page works.
        server
            .get(&format!("{path}?format=html"))
            .assert_header("content-type", "text/html; charset=utf-8");
        server
            .request(
                "GET",
                &format!("{path}?format=json"),
                &[("Accept", BROWSER)],
            )
            .assert_header("content-type", "application/json");
    }

    // Errors negotiate too: a mistyped URL in a browser is a page, not raw
    // JSON, and the same URL from an agent is a problem document.
    let lost = server.request("GET", "/nope", &[("Accept", BROWSER)]);
    lost.assert_status(404);
    lost.assert_header("content-type", "text/html; charset=utf-8");
    assert!(lost.text().contains("not_found"));
}

#[test]
fn every_error_response_carries_a_code() {
    // Every error carries a code, including the ones an off-the-shelf router
    // answers on its own. `/%FF` is a path segment that is not UTF-8 once
    // decoded, and reaches axum's extractor before any handler runs.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    let undecodable = server.get("/%FF");
    undecodable.assert_status(400);
    undecodable.assert_header("content-type", "application/problem+json");
    undecodable.assert_varies_on_accept();
    assert_eq!(undecodable.json()["code"], "malformed_request");

    // And it negotiates, like every other error.
    let page = server.request("GET", "/%FF", &[("Accept", "text/html")]);
    page.assert_header("content-type", "text/html; charset=utf-8");
}

#[test]
fn a_page_is_admitted_the_same_way_in_every_representation_it_offers() {
    // Admission classes what bounds the work, not how the answer is spelled.
    // A page costs its `limit` rows in JSON, in RDF and in HTML alike, so
    // charging the RDF forms a heavy permit priced the format rather than the
    // work — and priced it backwards, since the HTML page resolves a label per
    // distinct term and the Turtle page resolves none.
    let deployment = Deployment::new();
    deployment.publish_text("tox", "v1", GROWN_NT, "2026-06-01T14:03:22Z");
    let records = RecordingAccessLog::default();
    let server = deployment.serve_with_access(Arc::new(records.clone()), false);

    let representations = [
        "application/json",
        "text/turtle",
        "application/n-quads",
        "application/trig",
        "application/ld+json",
        "text/html",
    ];
    for accept in representations {
        server
            .request("GET", "/tox/v/v1/fragment?limit=2", &[("Accept", accept)])
            .assert_status(200);
    }
    // `/tpf` offers no JSON, so every machine representation of it is RDF.
    for accept in representations
        .iter()
        .filter(|accept| **accept != "application/json")
    {
        server
            .request("GET", "/tox/v/v1/tpf?limit=2", &[("Accept", *accept)])
            .assert_status(200);
    }
    // The one thing on these routes that does leave the page behind.
    server
        .request(
            "GET",
            "/tox/v/v1/fragment?o.text=Alice",
            &[("Accept", "text/turtle")],
        )
        .assert_status(200);

    let records = records.records();
    let (pages, filtered) = records
        .split_last()
        .map(|(filtered, pages)| (pages, filtered))
        .expect("one record per request");
    assert_eq!(pages.len(), representations.len() * 2 - 1);
    for record in pages {
        assert_eq!(
            record.work_class,
            Some(kgf_server::access::AccessWorkClass::Ordinary),
            "{:?} in {:?} is a page, not candidate-sized work",
            record.operation,
            record.representation
        );
    }
    assert_eq!(
        filtered.work_class,
        Some(kgf_server::access::AccessWorkClass::Heavy)
    );
}

#[test]
fn access_logging_emits_one_correlated_record_for_every_response() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    deployment.publish_quads("quads", "v1", WORKED_EXAMPLE_NQ, "2026-06-01T14:03:22Z");
    let records = RecordingAccessLog::default();
    let server = deployment.serve_with_access(Arc::new(records.clone()), false);
    let fragment_path = "/tox/v/v1/fragment?limit=2";

    let page = server.request(
        "GET",
        fragment_path,
        &[
            ("User-Agent", "curl/8.0"),
            ("X-Request-Id", "client-17"),
            ("X-Forwarded-For", "192.0.2.8, 192.0.2.9"),
        ],
    );
    page.assert_status(200);
    let etag = page.header("etag").expect("fragment validator");
    let not_modified = server.request("GET", fragment_path, &[("If-None-Match", etag.as_str())]);
    not_modified.assert_status(304);
    let missing = server.get("/SECRET/nope");
    missing.assert_status(404);
    let wrong_method = server.request("PUT", "/tox/v/v1/fragment", &[]);
    wrong_method.assert_status(405);
    let oversized = server.request_with_body("POST", "/tox", &[], &vec![b'x'; 2 * 1024 * 1024]);
    oversized.assert_status(413);
    let latest = server.get("/tox/latest/fragment?limit=2");
    latest.assert_status(307);
    let malformed_rdf = server.request(
        "GET",
        "/tox/v/v1/fragment?s=%3Cbroken",
        &[("Accept", "text/turtle")],
    );
    malformed_rdf.assert_status(400);
    malformed_rdf.assert_header("content-type", "application/problem+json");
    let latest_query = server.request("QUERY", "/tox/latest/fragment", &[]);
    latest_query.assert_status(307);
    let graphs = server.request("GET", "/quads/v/v1/graphs?limit=1", &[]);
    graphs.assert_status(200);

    let responses = [
        &page,
        &not_modified,
        &missing,
        &wrong_method,
        &oversized,
        &latest,
        &malformed_rdf,
        &latest_query,
        &graphs,
    ];
    let records = records.records();
    assert_eq!(records.len(), responses.len());
    for (record, response) in records.iter().zip(responses) {
        assert_eq!(record.status, Some(response.status));
        assert_eq!(
            response.header("kgf-request-id").as_deref(),
            Some(record.request_id.as_str())
        );
    }
    assert_eq!(
        records
            .iter()
            .map(|record| &record.request_id)
            .collect::<HashSet<_>>()
            .len(),
        records.len()
    );

    let page_record = &records[0];
    assert_eq!(
        page_record.route.as_deref(),
        Some("/{dataset}/v/{version}/fragment")
    );
    assert_eq!(
        page_record.operation,
        Some(kgf_server::access::AccessOperation::Fragment)
    );
    assert_eq!(page_record.dataset.as_deref(), Some("tox"));
    assert_eq!(page_record.version.as_deref(), Some("v1"));
    assert_eq!(page_record.representation.as_deref(), Some("json"));
    assert_eq!(page_record.rows, Some(2));
    assert_eq!(page_record.complete, Some(false));
    assert_eq!(page_record.truncation_reason, Some("page_limit"));
    assert_eq!(page_record.cardinality, Some(8));
    assert_eq!(page_record.exact, Some(true));
    assert_eq!(page_record.first_open, Some(true));
    assert!(page_record.open_ms.is_some());
    assert!(page_record.queue_ms.is_some());
    assert!(page_record.work_ms.is_some());
    assert_eq!(
        page_record.client_class,
        kgf_server::access::ClientClass::Curl
    );
    assert!(page_record.user_agent.is_none());
    assert!(page_record.client_request_id.is_none());
    assert_eq!(page_record.client_hash.as_deref().map(str::len), Some(16));
    assert_eq!(
        page_record.forwarded_hash.as_deref().map(str::len),
        Some(16)
    );
    assert!(page_record.target.is_none());
    assert!(page_record.q.is_none());
    assert_eq!(
        serde_json::to_value(page_record).unwrap()["shape"],
        serde_json::json!({"pattern": "???", "text": false, "limit": 2})
    );

    assert_eq!(records[1].status, Some(304));
    assert!(records[1].work_class.is_none());
    assert!(records[1].work_ms.is_none());
    assert_eq!(records[2].route, None);
    assert_eq!(records[2].operation, None);
    assert_eq!(records[2].code, Some("not_found"));
    assert_eq!(
        records[3].route.as_deref(),
        Some("/{dataset}/v/{version}/fragment")
    );
    assert_eq!(
        records[3].operation,
        Some(kgf_server::access::AccessOperation::Fragment)
    );
    assert_eq!(records[3].code, Some("method_not_allowed"));
    assert_eq!(records[4].code, Some("payload_too_large"));
    assert_eq!(
        records[5].operation,
        Some(kgf_server::access::AccessOperation::Latest)
    );
    assert_eq!(records[5].dataset.as_deref(), Some("tox"));
    assert_eq!(records[5].version.as_deref(), Some("v1"));
    assert_eq!(records[6].code, Some("bad_term_syntax"));
    assert_eq!(records[6].representation.as_deref(), Some("json"));
    assert_eq!(
        records[7].transport,
        Some(kgf_server::access::Transport::Query)
    );

    // A listing records the shape of what it was asked for and how much of the
    // answer it delivered, exactly as a pattern page does.
    let listing = &records[8];
    assert_eq!(
        listing.operation,
        Some(kgf_server::access::AccessOperation::Graphs)
    );
    assert_eq!(listing.dataset.as_deref(), Some("quads"));
    assert_eq!(listing.rows, Some(1));
    assert_eq!(listing.cardinality, Some(3));
    assert_eq!(listing.complete, Some(false));
    assert_eq!(listing.truncation_reason, Some("page_limit"));
    assert_eq!(
        serde_json::to_value(listing).unwrap()["shape"],
        serde_json::json!({"limit": 1})
    );
}

#[test]
fn shape_logging_excludes_content_and_raw_logging_is_explicit() {
    const SECRET: &str = "VERY_DISTINCTIVE_SECRET";
    let deployment = Deployment::new();
    deployment.publish_text("tox", "v1", GROWN_NT, "2026-06-01T14:03:22Z");
    deployment.publish_description("schema", "v1", "2026-08-08T12:00:00Z");
    let records = RecordingAccessLog::default();
    let server = deployment.serve_with_access(Arc::new(records.clone()), false);

    let fragment = format!(
        "/tox/v/v1/fragment?s={}",
        kgf_server::url::encode_value(&format!("<http://example.org/{SECRET}>"))
    );
    let user_agent = format!("curl/{SECRET}");
    server
        .request(
            "GET",
            &fragment,
            &[("User-Agent", &user_agent), ("X-Request-Id", SECRET)],
        )
        .assert_status(200);
    server
        .get(&format!("/tox/v/v1/search?q={SECRET}&predicate=ex%3Aname"))
        .assert_status(200);
    let body = serde_json::to_vec(&serde_json::json!({
        "pattern": {"s": "?person", "p": "ex:knows", "o": "?known"},
        "bindings": {
            "vars": ["?person"],
            "rows": [[format!("<http://example.org/{SECRET}>")]]
        }
    }))
    .unwrap();
    server
        .request_with_body(
            "QUERY",
            "/tox/v/v1/fragment",
            &[("Content-Type", "application/json")],
            &body,
        )
        .assert_status(200);
    server
        .request_with_body(
            "POST",
            "/tox/v/v1/fragment",
            &[("Content-Type", "application/json")],
            &body,
        )
        .assert_status(200);
    let values = format!("(?person) {{ (<http://example.org/{SECRET}>) }}");
    server
        .get(&format!(
            "/tox/v/v1/tpf?subject=%3Fperson&predicate=http%3A%2F%2Fexample.org%2Fknows&object=%3Fknown&values={}",
            kgf_server::url::encode_value(&values)
        ))
        .assert_status(200);
    server.get(&format!("/{SECRET}")).assert_status(404);
    server
        .get(&format!("/schema/v/v1/schema?view=component%3A{SECRET}"))
        .assert_status(404);

    let shape_records = records.records();
    let encoded = serde_json::to_string(&shape_records).unwrap();
    assert!(
        !encoded.contains(SECRET),
        "shape tier leaked request content: {encoded}"
    );
    assert_eq!(
        shape_records[2].transport,
        Some(kgf_server::access::Transport::Query)
    );
    assert_eq!(
        shape_records[3].transport,
        Some(kgf_server::access::Transport::Post)
    );
    assert_eq!(shape_records[0].bytes_in, None);
    assert_eq!(shape_records[2].bytes_in, Some(body.len() as u64));
    assert_eq!(shape_records[3].bytes_in, Some(body.len() as u64));
    assert_eq!(
        shape_records[4].transport,
        Some(kgf_server::access::Transport::GetValues)
    );
    assert_eq!(
        shape_records[4].operation,
        Some(kgf_server::access::AccessOperation::Tpf)
    );
    assert_eq!(
        serde_json::to_value(&shape_records[2]).unwrap()["shape"],
        serde_json::json!({
            "pattern": "?i?", "text": false, "limit": 100,
            "k": 1, "columns": 1
        })
    );

    let raw_records = RecordingAccessLog::default();
    let raw_server = deployment.serve_with_access(Arc::new(raw_records.clone()), true);
    raw_server
        .request(
            "GET",
            &format!("/tox/v/v1/search?q={SECRET}&predicate=ex%3Aname"),
            &[("User-Agent", &user_agent), ("X-Request-Id", SECRET)],
        )
        .assert_status(200);
    let raw = raw_records.records();
    assert_eq!(raw.len(), 1);
    assert!(raw[0].target.as_deref().unwrap().contains(SECRET));
    assert_eq!(raw[0].q.as_deref(), Some(SECRET));
    assert_eq!(raw[0].user_agent.as_deref(), Some(user_agent.as_str()));
    assert_eq!(raw[0].client_request_id.as_deref(), Some(SECRET));
}

#[test]
fn request_ids_are_minted_without_an_access_log() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    let first = server.get("/");
    first.assert_status(200);
    let second = server.get("/tox/v/v1/fragment?limit=1");
    second.assert_status(200);

    let first_id = first.header("kgf-request-id").expect("a minted request id");
    let second_id = second
        .header("kgf-request-id")
        .expect("a minted request id");
    assert_ne!(first_id, second_id);
    assert_eq!(first_id.len(), "0123456789abcdef-00000000".len());
}

#[test]
fn forwarded_identity_comes_from_the_trusted_hop_only() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");

    let records = RecordingAccessLog::default();
    let server = deployment.serve_with_access(Arc::new(records.clone()), false);
    server
        .request("GET", "/", &[("X-Forwarded-For", "192.0.2.9")])
        .assert_status(200);
    server
        .request("GET", "/", &[("X-Forwarded-For", "10.0.0.1, 192.0.2.9")])
        .assert_status(200);
    server
        .request("GET", "/", &[("X-Forwarded-For", "192.0.2.9, 10.0.0.1")])
        .assert_status(200);
    server.get("/").assert_status(200);
    let records = records.records();
    assert_eq!(records[0].forwarded_hash.as_deref().map(str::len), Some(16));
    assert_eq!(
        records[0].forwarded_hash, records[1].forwarded_hash,
        "an entry the caller prepended must not change its identity"
    );
    assert_ne!(records[0].forwarded_hash, records[2].forwarded_hash);
    assert_eq!(records[3].forwarded_hash, None);
    assert!(
        records
            .iter()
            .all(|record| record.client_hash == records[0].client_hash)
    );

    let unproxied = RecordingAccessLog::default();
    let direct = deployment.serve_configured(|config| {
        config.access_log = Some(Arc::new(unproxied.clone()));
    });
    direct
        .request("GET", "/", &[("X-Forwarded-For", "192.0.2.9")])
        .assert_status(200);
    assert_eq!(unproxied.records()[0].forwarded_hash, None);
}

#[test]
fn a_request_is_refused_before_it_costs_anything() {
    // Two things at once: an unanswerable request is refused for the reason it
    // is unanswerable, and it does not open a cold bundle on the way. The
    // bundle here cannot be opened at all, so a 400 proves negotiation ran
    // first — the open would have made it a 500.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    std::fs::remove_file(deployment.bundle("tox", "v1").join("data.hdt.perm")).unwrap();
    let server = deployment.serve();

    let unsupported = server.get("/tox/v/v1/manifest?format=parquet");
    unsupported.assert_status(400);
    assert_eq!(unsupported.json()["code"], "unsupported_format");

    let unacceptable = server.request(
        "GET",
        "/tox/v/v1/manifest",
        &[("Accept", "application/parquet")],
    );
    unacceptable.assert_status(406);
    assert_eq!(unacceptable.json()["code"], "not_acceptable");

    // A request that *is* answerable still reaches the broken bundle.
    server.get("/tox/v/v1/manifest").assert_status(500);
}

#[test]
fn an_accept_split_across_field_lines_is_one_list() {
    // RFC 9110 §5.3: a sender may split a list-valued field, and a recipient
    // treats the lines as one comma-separated value. Reading only the first
    // turns this into a 406 for a request that asked for something we have.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    let split = server.request(
        "GET",
        "/tox",
        &[("Accept", "application/xml"), ("Accept", "text/html")],
    );
    split.assert_status(200);
    split.assert_header("content-type", "text/html; charset=utf-8");

    // The same list on one line, for comparison.
    server
        .request("GET", "/tox", &[("Accept", "application/xml, text/html")])
        .assert_header("content-type", "text/html; charset=utf-8");
}

#[test]
fn negotiation_and_parameter_failures_are_told_apart() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    // Three ways to fail at choosing a representation, three codes, three
    // statuses, three remedies.
    let unsupported = server.get("/tox/v/v1/manifest?format=parquet");
    unsupported.assert_status(400);
    assert_eq!(unsupported.json()["code"], "unsupported_format");

    let unacceptable = server.request(
        "GET",
        "/tox/v/v1/manifest",
        &[("Accept", "application/parquet")],
    );
    unacceptable.assert_status(406);
    assert_eq!(unacceptable.json()["code"], "not_acceptable");

    // A repeated parameter has no defensible resolution, so it is refused.
    let repeated = server.get("/tox/v/v1/manifest?format=json&format=html");
    repeated.assert_status(400);
    assert_eq!(repeated.json()["code"], "malformed_request");
}

#[test]
fn a_revalidation_does_not_open_the_bundle_it_is_revalidating() {
    // The cheapest request a client can make must be the cheapest one the
    // server answers. The bundle here cannot be opened at all, so a 304 is
    // proof the precondition was evaluated before the open — not merely before
    // the body, which is where it used to sit.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    let first = server.get("/tox/v/v1/manifest");
    first.assert_status(200);
    let etag = first
        .header("etag")
        .expect("a versioned GET carries an ETag");

    std::fs::remove_file(deployment.bundle("tox", "v1").join("data.hdt.perm")).unwrap();
    let server = deployment.serve();
    let revalidated = server.request(
        "GET",
        "/tox/v/v1/manifest",
        &[("If-None-Match", etag.as_str())],
    );
    revalidated.assert_status(304);
    assert!(
        revalidated.header("x-robots-tag").is_none(),
        "a manifest narrows nothing, on the 304 as on the 200"
    );
    // And the unconditional request against the same bundle still fails, so the
    // 304 above was not simply a bundle that happens to open.
    server.get("/tox/v/v1/manifest").assert_status(500);
}

#[test]
fn the_descriptors_can_be_revalidated_too() {
    // They are derived rather than published, but they are fixed for the life
    // of the process, so they get a validator — without one a conditional
    // request on them cannot be answered 304 at all.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    for path in ["/", "/tox"] {
        let first = server.get(path);
        first.assert_status(200);
        let etag = first.header("etag").unwrap_or_else(|| panic!("{path}"));

        let revalidated = server.request("GET", path, &[("If-None-Match", etag.as_str())]);
        revalidated.assert_status(304);
        assert!(
            revalidated.header("x-robots-tag").is_none(),
            "{path} is catalog, on the 304 as on the 200"
        );
        // RFC 9110 §13.1.2's wildcard: the resource exists, so it is unchanged.
        server
            .request("GET", path, &[("If-None-Match", "*")])
            .assert_status(304);
        // And the validator is representation-specific here too.
        server
            .request(
                "GET",
                path,
                &[("Accept", "text/html"), ("If-None-Match", etag.as_str())],
            )
            .assert_status(200);
    }
}

#[test]
fn an_error_no_handler_raised_still_carries_a_code() {
    // The request-body limit answers before any of this crate's code runs.
    // Every error response carries a code, including
    // the ones a `tower` layer produces.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    let oversized = server.request_with_body("POST", "/tox", &[], &vec![b'x'; 2 * 1024 * 1024]);
    oversized.assert_status(413);
    oversized.assert_header("content-type", "application/problem+json");
    assert_eq!(oversized.json()["code"], "payload_too_large");
}

#[test]
fn an_accept_header_that_cannot_be_read_is_refused() {
    // Dropping the unreadable line and negotiating from the rest answers a
    // different request than the client made, and succeeds while doing it.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    let refused = server.request("GET", "/tox", &[("Accept", "text/\u{e9}html")]);
    refused.assert_status(400);
    assert_eq!(refused.json()["code"], "malformed_request");
}

#[test]
fn a_long_path_is_not_reflected_whole_into_the_error() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();

    let long = format!("/{}", "a".repeat(4000));
    let lost = server.get(&long);
    lost.assert_status(404);
    assert!(
        lost.body.len() < 2000,
        "an error must not be larger than the request that caused it: {} bytes",
        lost.body.len()
    );
    assert!(lost.json()["instance"].as_str().unwrap().ends_with('…'));
}

#[test]
fn a_bundle_that_cannot_be_opened_answers_rather_than_panics() {
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");

    // Remove the required permutation sidecar *after* the manifest is written,
    // so the version is scanned and described but cannot be served. There is
    // no fallback, so the bundle is refused at open.
    std::fs::remove_file(deployment.bundle("tox", "v1").join("data.hdt.perm")).unwrap();
    let server = deployment.serve();

    // The descriptors still work — they are published bytes, not the store.
    server.get("/tox").assert_status(200);

    let failed = server.get("/tox/v/v1/manifest");
    failed.assert_status(500);
    assert_eq!(failed.json()["code"], "internal_error");
    let detail = failed.json()["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains("tox") && detail.contains("v1"), "{detail}");
    // The remedy names artifacts on the server's disk, so it goes to the log
    // rather than to a public client.
    assert!(!detail.contains("data.hdt.perm"), "{detail}");
    assert!(
        !detail.contains(deployment.root_path().to_str().unwrap()),
        "{detail}"
    );

    // A `/manifest` that opens the bundle is the point: without it this URL
    // would have advertised capabilities for a version no query can answer.
}

#[test]
fn a_manifest_that_disagrees_with_its_directory_stops_startup() {
    // Loud rather than degraded: the alternative is a version
    // that is on disk and 404s, which an operator has no way to notice.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let manifest = deployment.bundle("tox", "v1").join("manifest.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    document["version"] = serde_json::json!("v2");
    std::fs::write(&manifest, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

    let error = Service::build(kgf_server::Config::new(
        kgf::serve::published_root(deployment.root_path()).unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    ))
    .expect_err("a mislabelled version is not servable");
    let message = error.to_string();
    assert!(
        message.contains("v1") && message.contains("v2"),
        "{message}"
    );
}

#[test]
fn a_dataset_directory_named_after_a_route_stops_startup() {
    // `kgf build` refuses to mint this id, so the only way one reaches a bundle
    // root is the way this test makes it: a directory put there by something
    // else. Loud rather than degraded, for the same reason a mislabelled
    // version is — the alternative is a dataset that is on disk, listed in the
    // service descriptor, and answers the health probe at every one of its URLs.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    std::fs::create_dir_all(deployment.root_path().join("healthz/v1"))
        .expect("a hand-assembled bundle directory");

    let error = Service::build(kgf_server::Config::new(
        kgf::serve::published_root(deployment.root_path()).unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    ))
    .expect_err("a shadowed dataset is not servable");
    let message = error.to_string();
    assert!(message.contains("healthz"), "{message}");
    assert!(
        message.contains("rename"),
        "the operator needs the fix: {message}"
    );
}

#[test]
fn the_operations_answer_over_the_wire_with_their_completeness_on_the_headers() {
    // `operations.rs` checks what the read operations *answer*, headless. What
    // only a socket can show is the rest of the response: completeness metadata in
    // both channels, an immutable validator on a versioned GET, and a page for
    // a browser at the same URL.
    let deployment = Deployment::new();
    deployment.publish("tox", "2026-06-01", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();
    let base = "/tox/v/2026-06-01";

    let page = server.get(&format!("{base}/fragment?limit=2"));
    page.assert_status(200);
    page.assert_header("content-type", "application/json");
    page.assert_cache_control(&["public", "max-age=31536000", "immutable"]);
    page.assert_varies_on_accept();
    assert!(
        page.header("x-robots-tag").is_none(),
        "a machine representation is the client surface, never a crawl target"
    );

    // The body says it is truncated, and so do the headers. Both are required
    // both, because a CSV or Parquet body has nowhere to put it.
    let body = page.json();
    assert_eq!(body["complete"], serde_json::json!(false));
    assert_eq!(body["truncation_reason"], "page_limit");
    page.assert_header("kgf-complete", "false");
    page.assert_header("kgf-truncation-reason", "page_limit");
    page.assert_header(
        "kgf-next-cursor",
        body["next"].as_str().expect("a cursor in the body"),
    );

    // A complete response says so in both channels too, and offers nothing to
    // continue.
    let whole = server.get(&format!("{base}/count?p=ex:knows"));
    whole.assert_status(200);
    whole.assert_header("kgf-complete", "true");
    assert!(whole.header("kgf-next-cursor").is_none());
    assert_eq!(
        whole.json()["count"],
        serde_json::json!({"value": 2, "exact": true}),
        "alice and bob know each other"
    );

    // A versioned operation is a deterministic function of immutable bytes, so
    // it revalidates like `/manifest` does.
    let etag = page.header("etag").expect("an operation carries an ETag");
    let not_modified = server.request(
        "GET",
        &format!("{base}/fragment?limit=2"),
        &[("If-None-Match", etag.as_str())],
    );
    not_modified.assert_status(304);
    assert!(
        not_modified.header("x-robots-tag").is_none(),
        "a 304 carries the directives of the 200 it stands in for"
    );

    // And the same URL is a page in a browser.
    let html = server.request(
        "GET",
        &format!("{base}/fragment?limit=2"),
        &[("Accept", "text/html")],
    );
    html.assert_header("content-type", "text/html; charset=utf-8");
    html.assert_header("x-robots-tag", "noindex, nofollow");
    // And the same operation's entry point is not: it is one page per release,
    // linking into the narrowed space a crawler is asked to stop at.
    let entry = server.request(
        "GET",
        &format!("{base}/fragment"),
        &[("Accept", "text/html")],
    );
    entry.assert_status(200);
    assert!(
        entry.header("x-robots-tag").is_none(),
        "an operation's entry point is finite and stays discoverable"
    );
    // Which is why its fan-out is marked per link instead. Every drill-down and
    // continuation from this page leads into the narrowed space, and a narrowed
    // page is served `noindex`, so following one can never reach anything an
    // index would keep. A page-wide directive would have taken the breadcrumbs
    // with it, and those are the links tying an operation to its dataset.
    let entry_links = links(&entry.text());
    assert!(
        entry_links
            .iter()
            .filter(|(href, _)| href.contains("/describe?"))
            .count()
            >= 3,
        "the entry page fans out into term links: {entry_links:?}"
    );
    for (href, nofollow) in &entry_links {
        assert_eq!(
            *nofollow,
            href.contains('?'),
            "{href} leads {} the data space, so nofollow should be {}",
            if href.contains('?') { "into" } else { "out of" },
            href.contains('?')
        );
    }
    assert!(
        entry_links
            .iter()
            .any(|(href, nofollow)| href == "/tox" && !nofollow),
        "the breadcrumb back to the dataset stays followable: {entry_links:?}"
    );
    let page_etag = html.header("etag").expect("a page carries an ETag");
    let page_revalidated = server.request(
        "GET",
        &format!("{base}/fragment?limit=2"),
        &[
            ("Accept", "text/html"),
            ("If-None-Match", page_etag.as_str()),
        ],
    );
    page_revalidated.assert_status(304);
    page_revalidated.assert_header("x-robots-tag", "noindex, nofollow");

    let html_text = html.text();
    assert!(html_text.contains("Next page"));
    assert!(html_text.contains("<summary>Fragment</summary>"));
    assert!(html_text.contains("name=\"limit\" value=\"2\""));
    assert!(!html_text.contains("name=\"cursor\""));
    // The completeness headers ride the page as well, since they are what an
    // intermediary reads without parsing a body it cannot parse.
    html.assert_header("kgf-complete", "false");

    // `latest` reaches the operations with the query intact.
    server
        .get("/tox/latest/fragment?limit=2")
        .assert_header("location", "/tox/v/2026-06-01/fragment?limit=2");

    // A native HTML form submits untouched optional controls as empty. The
    // server applies their defaults even without JavaScript, and the page's
    // own canonical link drops the empty aliases again.
    let blank = server.request(
        "GET",
        &format!("{base}/fragment?s=&p=ex:knows&o=&o.text=&limit="),
        &[("Accept", "text/html")],
    );
    blank.assert_status(200);
    let blank_html = blank.text();
    assert!(blank_html.contains("/fragment?p=ex%3Aknows&amp;format=json"));
    assert!(!blank_html.contains("limit="));

    server
        .get(&format!("{base}/count?s=&p=&o=&o.text="))
        .assert_status(200);
    server
        .get(&format!("{base}/describe?iri=ex:alice&limit="))
        .assert_status(200);
    server
        .get(&format!("{base}/sample?s=&p=&o=&n=&seed="))
        .assert_status(200);

    // Required and unknown controls are not part of that normalization.
    server
        .get(&format!("{base}/describe?iri=&limit="))
        .assert_status(400);
    server
        .get(&format!("{base}/fragment?unknown="))
        .assert_status(400);

    // The manifest is the entry point for required-argument operations such as
    // describe, so its forms must all point at this exact immutable version.
    let manifest = server.request(
        "GET",
        &format!("{base}/manifest"),
        &[("Accept", "text/html")],
    );
    assert!(
        manifest.header("x-robots-tag").is_none(),
        "the finite version manifest remains discoverable"
    );
    let manifest_html = manifest.text();
    for operation in ["fragment", "tpf", "count", "describe", "sample"] {
        assert!(
            manifest_html.contains(&format!("action=\"{base}/{operation}\"")),
            "manifest omitted the {operation} form"
        );
    }
    assert!(!manifest_html.contains(&format!("action=\"{base}/search\"")));
}

#[test]
fn a_validator_moves_when_the_configuration_does() {
    // The bytes at a URL are a function of the data, the configuration *and*
    // the code — not the data alone. `GET /fragment` with no `limit` returns
    // `caps.default_limit` rows, so raising that number changes this response
    // while the bundle it reads has not moved. Under `immutable` and a year of
    // `max-age`, a validator that missed it would answer 304 for a year.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");

    let small = deployment.serve_with(kgf_server::Caps {
        default_limit: 2,
        ..kgf_server::Caps::new()
    });
    let first = small.get("/tox/v/v1/fragment");
    first.assert_status(200);
    let etag = first.header("etag").expect("an operation carries an ETag");
    assert_eq!(first.json()["rows"].as_array().unwrap().len(), 2);
    // Its own tag still revalidates, or the validator would be useless.
    small
        .request(
            "GET",
            "/tox/v/v1/fragment",
            &[("If-None-Match", etag.as_str())],
        )
        .assert_status(304);
    drop(small);

    // Same bundle, same URL, different published default — and now a different
    // answer, so it must be a different entity.
    let larger = deployment.serve_with(kgf_server::Caps {
        default_limit: 5,
        ..kgf_server::Caps::new()
    });
    let second = larger.get("/tox/v/v1/fragment");
    assert_eq!(second.json()["rows"].as_array().unwrap().len(), 5);
    assert_ne!(second.header("etag"), Some(etag.clone()));
    larger
        .request(
            "GET",
            "/tox/v/v1/fragment",
            &[("If-None-Match", etag.as_str())],
        )
        .assert_status(200);

    // The immutable publication half is still in there; the deployment
    // component is an addition, not a replacement.
    let manifest = larger.get("/tox/v/v1/manifest");
    let publication_digest = format!("sha256:{:x}", Sha256::digest(&manifest.body));
    assert!(second.header("etag").unwrap().contains(&publication_digest));
}

#[test]
fn an_operation_whose_artifact_a_bundle_lacks_is_refused_before_it_is_opened() {
    // Search needs the text index, so a bundle without one is answered 501 — the
    // request is well formed and the shortfall is what this bundle carries, which
    // is exactly what `capability_not_available` says.
    let deployment = Deployment::new();
    deployment.publish_text("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();
    server.get("/tox/v/v1/search?q=Alice").assert_status(200);
    server
        .get("/tox/v/v1/fragment?o.text=Alice")
        .assert_status(200);

    // Withdraw it, and only the operations that read those bytes change.
    let manifest = deployment.bundle("tox", "v1").join("manifest.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    document["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove("search");
    std::fs::write(&manifest, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let server = deployment.serve();

    for target in [
        "/tox/v/v1/search?q=Alice",
        "/tox/v/v1/fragment?o.text=Alice",
    ] {
        let refused = server.get(target);
        refused.assert_status(501);
        assert_eq!(
            refused.json()["code"],
            "capability_not_available",
            "{target}"
        );
    }
    server.get("/tox/v/v1/fragment?limit=2").assert_status(200);
}

#[test]
fn an_operation_needing_no_sidecar_is_not_gated_on_its_declaration() {
    // The other half of the rule. `sample`, `labels`, and `terms` compose the
    // artifacts every bundle is required to carry, so nothing published can fail
    // to answer them. A manifest that omits one is therefore not a bundle that
    // cannot serve it, and out-of-date metadata must not be able to withdraw
    // work the bytes support — the failure a gate here would cause and could not
    // prevent. `terms` is not declared at all, which makes the point twice over.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let manifest = deployment.bundle("tox", "v1").join("manifest.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    let capabilities = document["capabilities"].as_object_mut().unwrap();
    assert!(
        !capabilities.contains_key("terms"),
        "a bundle cannot honestly declare a capability half of which it cannot answer"
    );
    for capability in ["sample", "labels"] {
        assert!(
            capabilities.remove(capability).is_some(),
            "a core bundle declares {capability}"
        );
    }
    std::fs::write(&manifest, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let server = deployment.serve();

    server.get("/tox/v/v1/sample?n=2").assert_status(200);
    server.get("/tox/v/v1/terms?limit=2").assert_status(200);
    server
        .get("/tox/v/v1/terms?limit=2&labels=true")
        .assert_status(200);
    let body = serde_json::to_vec(&serde_json::json!({"iris": ["ex:alice"]})).unwrap();
    server
        .request_with_body(
            "QUERY",
            "/tox/v/v1/labels",
            &[("Content-Type", "application/json")],
            &body,
        )
        .assert_status(200);

    // And they stay advertised, because what this deployment routes is the
    // descriptor's statement rather than the bundle's.
    let links = &server.get("/").json()["datasets"][0]["links"];
    for operation in ["sample", "terms", "labels"] {
        assert_eq!(
            links[operation],
            serde_json::json!(format!("/tox/v/v1/{operation}")),
            "{operation}"
        );
    }
}

#[test]
fn an_operations_parameters_are_read_before_the_bundle_is_opened() {
    // The same rule unit 13 established for negotiation, now with parameters:
    // this bundle cannot be opened at all, so anything but a 500 proves the
    // refusal came first.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    std::fs::remove_file(deployment.bundle("tox", "v1").join("data.hdt.perm")).unwrap();
    let server = deployment.serve();

    for (target, code) in [
        ("/tox/v/v1/fragment?limit=99999", "cap_exceeded"),
        ("/tox/v/v1/fragment?limt=1", "malformed_request"),
        ("/tox/v/v1/fragment?s=nope:x", "bad_term_syntax"),
        ("/tox/v/v1/fragment?cursor=nonsense", "stale_cursor"),
        (
            "/tox/v/v1/fragment?g=%3Chttp%3A%2F%2Fx%3E",
            "capability_not_available",
        ),
        ("/tox/v/v1/describe", "malformed_request"),
    ] {
        let refused = server.get(target);
        assert_eq!(refused.json()["code"], code, "{target}");
        assert_ne!(
            refused.status, 500,
            "{target} must not have opened anything"
        );
    }

    // And a request that is fine still reaches the broken bundle.
    server.get("/tox/v/v1/fragment").assert_status(500);
}

// ---------------------------------------------------------------------------
// A deployment, and a client that writes its own requests
// ---------------------------------------------------------------------------

struct Deployment {
    root: tempfile::TempDir,
}

impl Deployment {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("temp dir"),
        }
    }

    fn root_path(&self) -> &Path {
        self.root.path()
    }

    fn bundle(&self, dataset: &str, version: &str) -> std::path::PathBuf {
        self.root.path().join(dataset).join(version)
    }

    /// Build a bundle with hdtc, describe it with `kgf manifest`, and date it.
    fn publish(&self, dataset: &str, version: &str, source: &str, created: &str) {
        self.publish_bundle(dataset, version, source, created, false);
    }

    fn publish_text(&self, dataset: &str, version: &str, source: &str, created: &str) {
        self.publish_bundle(dataset, version, source, created, true);
    }

    /// Build a quad bundle — sidecar and index beside the HDT — and describe it.
    fn publish_quads(&self, dataset: &str, version: &str, source: &str, created: &str) {
        self.publish_fixture(dataset, version, Fixture::build_quads(source), created);
    }

    /// Assemble a quad bundle with `kgf build`, the way a deployment does.
    ///
    /// The whole pipeline rather than a fixture: the description of each graph
    /// is produced by the build and read back by the server, so this is the
    /// only kind of test that can catch the two disagreeing about what a view
    /// is called or where its rows are.
    fn publish_built_quads(&self, dataset: &str, version: &str, source: &str, created: &str) {
        self.publish_built(dataset, version, source, created, "");
    }

    /// The same, with extra build config appended — components, say.
    fn publish_built(
        &self,
        dataset: &str,
        version: &str,
        source: &str,
        created: &str,
        extra: &str,
    ) {
        let workspace = tempfile::tempdir().expect("build scratch");
        let input = workspace.path().join("source.nq");
        std::fs::write(&input, source).expect("write the build's input");
        let config = workspace.path().join("build.yaml");
        std::fs::write(
            &config,
            format!(
                "schema: 1\ndataset: {{id: {dataset}, iri: 'https://example.org/{dataset}'}}\n\
                 semantics: {{prefixes: {{ex: 'http://example.org/'}}}}\n{extra}"
            ),
        )
        .expect("write the build config");

        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: kgf::build::Args,
        }
        let cli = Cli::parse_from([
            "kgf-build",
            "--config",
            config.to_str().unwrap(),
            "--out",
            self.bundle(dataset, version).to_str().unwrap(),
            "--input",
            input.to_str().unwrap(),
            "--hdtc",
            kgf_store::testing::hdtc_binary().to_str().unwrap(),
        ]);
        kgf::build::run(cli.args).expect("build a quad bundle");
        self.set_created(&self.bundle(dataset, version), created);
    }

    fn publish_description(&self, dataset: &str, version: &str, created: &str) {
        self.publish_description_with_labels(dataset, version, created, true);
    }

    fn publish_description_without_labels(&self, dataset: &str, version: &str, created: &str) {
        self.publish_description_with_labels(dataset, version, created, false);
    }

    fn publish_description_with_labels(
        &self,
        dataset: &str,
        version: &str,
        created: &str,
        labels: bool,
    ) {
        let bundle = self.bundle(dataset, version);
        Fixture::description().copy_bundle_to(&bundle);
        std::fs::write(bundle.join("stats/summary.json"), SUMMARY_CARD_JSON)
            .expect("write rich summary fixture");

        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: kgf::manifest::Args,
        }
        let mut arguments = vec![
            "kgf-manifest".to_owned(),
            bundle.to_str().unwrap().to_owned(),
            "--id".to_owned(),
            dataset.to_owned(),
            "--version".to_owned(),
            version.to_owned(),
            "--title".to_owned(),
            format!("{dataset} {version}"),
            "--prefix".to_owned(),
            "ex=https://example.org/".to_owned(),
        ];
        if labels {
            arguments.extend([
                "--role".to_owned(),
                "label=https://example.org/label".to_owned(),
            ]);
        } else {
            // Supplying a non-label role opts out of the federation defaults,
            // whose profile deliberately includes a label cascade.
            arguments.extend([
                "--role".to_owned(),
                "synonym=https://example.org/synonym".to_owned(),
            ]);
        }
        let cli = Cli::parse_from(arguments);
        kgf::manifest::run(cli.args).expect("describe the tier-1 bundle");
        self.set_created(&bundle, created);
    }

    fn publish_bundle(
        &self,
        dataset: &str,
        version: &str,
        source: &str,
        created: &str,
        text: bool,
    ) {
        let fixture = Fixture::build(source);
        let fixture = if text { fixture.with_text() } else { fixture };
        self.publish_fixture(dataset, version, fixture, created);
    }

    fn publish_fixture(&self, dataset: &str, version: &str, fixture: Fixture, created: &str) {
        let bundle = self.bundle(dataset, version);
        fixture.copy_bundle_to(&bundle);

        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: kgf::manifest::Args,
        }
        let cli = Cli::parse_from([
            "kgf-manifest",
            bundle.to_str().unwrap(),
            "--title",
            &format!("{dataset} {version}"),
            "--prefix",
            "ex=http://example.org/",
            "--role",
            "label=http://example.org/name",
        ]);
        kgf::manifest::run(cli.args).expect("describe the bundle");

        // `kgf manifest` stamps `created` with the build time, and two bundles
        // built inside one test share a second. The releases here need a
        // defined order, so the timestamps are written explicitly.
        self.set_created(&bundle, created);
    }

    fn set_created(&self, bundle: &Path, created: &str) {
        let path = bundle.join("manifest.json");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        document["created"] = serde_json::json!(created);
        std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    }

    fn set_dataset_iri(&self, dataset: &str, version: &str, iri: &str) {
        let path = self.bundle(dataset, version).join("manifest.json");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        document["dataset_iri"] = serde_json::json!(iri);
        std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    }

    fn serve(&self) -> Server {
        self.serve_with(kgf_server::Caps::new())
    }

    fn serve_with(&self, caps: kgf_server::Caps) -> Server {
        self.serve_with_limits(caps, kgf_server::Budgets::new())
    }

    fn serve_with_limits(&self, caps: kgf_server::Caps, budgets: kgf_server::Budgets) -> Server {
        self.serve_config(caps, budgets, None)
    }

    fn serve_with_public_base(&self, public_base: &str) -> Server {
        self.serve_config(
            kgf_server::Caps::new(),
            kgf_server::Budgets::new(),
            Some(public_base.parse().expect("a valid public base")),
        )
    }

    fn serve_config(
        &self,
        caps: kgf_server::Caps,
        budgets: kgf_server::Budgets,
        public_base: Option<kgf_server::PublicBase>,
    ) -> Server {
        self.serve_configured(|config| {
            config.caps = caps;
            config.budgets = budgets;
            config.public_base = public_base;
        })
    }

    /// Serve with a recording sink behind one trusted forwarding hop.
    fn serve_with_access(&self, access_log: Arc<dyn AccessLog>, log_raw: bool) -> Server {
        self.serve_configured(|config| {
            config.access_log = Some(access_log);
            config.log_raw = log_raw;
            config.trusted_proxies = 1;
        })
    }

    fn serve_configured(&self, configure: impl FnOnce(&mut kgf_server::Config)) -> Server {
        let mut config = kgf_server::Config::new(
            kgf::serve::published_root(self.root.path()).expect("a published root"),
            "127.0.0.1:0".parse().unwrap(),
        );
        configure(&mut config);
        let service = Arc::new(Service::build(config).expect("a servable deployment"));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("bind");
        let address = listener.local_addr().expect("local address");

        // The server stops when this test's `Server` drops, rather than living
        // until the process does. `serve_on` takes the trigger precisely so a
        // caller that is not `kgf serve` need not adopt its signal handlers.
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        runtime.spawn(kgf_server::serve_on(listener, service, async move {
            let _ = stopped.await;
        }));

        Server {
            address,
            stop: Some(stop),
            runtime: Some(runtime),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct RecordingAccessLog(Arc<Mutex<Vec<AccessRecord>>>);

impl RecordingAccessLog {
    fn records(&self) -> Vec<AccessRecord> {
        self.0.lock().expect("access records").clone()
    }
}

impl AccessLog for RecordingAccessLog {
    fn record(&self, record: &AccessRecord) {
        self.0.lock().expect("access records").push(record.clone());
    }
}

struct Server {
    address: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    /// Held so the reactor outlives the requests made against it, and shut down
    /// with it — a test binary that leaked one runtime per test would carry
    /// every worker and blocking thread it ever started to the end of the run.
    runtime: Option<tokio::runtime::Runtime>,
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(runtime) = self.runtime.take() {
            // Bounded: a request still in flight holds an `Arc<Store>` over
            // mapped files, and the fixture directory is removed right after
            // this returns.
            runtime.shutdown_timeout(std::time::Duration::from_secs(5));
        }
    }
}

impl Server {
    fn get(&self, target: &str) -> Response {
        self.request("GET", target, &[])
    }

    fn request(&self, method: &str, target: &str, headers: &[(&str, &str)]) -> Response {
        self.request_with_body(method, target, headers, &[])
    }

    /// Write one request onto a socket, byte for byte, and read the answer.
    fn request_with_body(
        &self,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Response {
        let mut request = format!(
            "{method} {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.address
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        // Always present, so a server expecting a body on QUERY or POST does
        // not wait for one.
        request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        self.exchange(method, &request, body)
    }

    fn request_without_host(&self, target: &str, headers: &[(&str, &str)]) -> Response {
        let mut request = format!("GET {target} HTTP/1.0\r\nConnection: close\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("Content-Length: 0\r\n\r\n");
        self.exchange("GET", &request, &[])
    }

    fn exchange(&self, method: &str, request: &str, body: &[u8]) -> Response {
        let mut stream = TcpStream::connect(self.address).expect("connect");
        stream.write_all(request.as_bytes()).expect("write");
        // A server that rejects the body mid-stream closes the connection, and
        // its response is still worth reading — so a broken pipe here is not a
        // test failure.
        let _ = stream.write_all(body);

        // A server that rejects a body mid-stream answers and closes, which on
        // some platforms surfaces to the sender as a reset rather than an EOF.
        // What arrived before that is still the response.
        let mut raw = Vec::new();
        match stream.read_to_end(&mut raw) {
            Ok(_) => {}
            Err(error) if !raw.is_empty() => {
                eprintln!("peer closed after answering ({error})");
            }
            Err(error) => panic!("read: {error}"),
        }
        Response::parse(&raw, method)
    }
}

/// Every `<a>` in a rendered page, as its href and whether it is `nofollow`.
///
/// Hand-scanned rather than parsed: the assertion is about two attributes on
/// one element, and a dependency on an HTML parser would be a larger surface
/// than the thing under test.
fn links(html: &str) -> Vec<(String, bool)> {
    let mut found = Vec::new();
    for tag in html.split("<a ").skip(1) {
        let tag = &tag[..tag.find('>').expect("an opening tag is closed")];
        let Some((_, after)) = tag.split_once("href=\"") else {
            continue;
        };
        let href = after.split('"').next().expect("a quoted href");
        found.push((href.to_owned(), tag.contains("rel=\"nofollow\"")));
    }
    found
}

struct Response {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl Response {
    fn parse(raw: &[u8], method: &str) -> Self {
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("a complete header block");
        let head = std::str::from_utf8(&raw[..split]).expect("headers are ASCII");
        let mut lines = head.split("\r\n");

        let status = lines
            .next()
            .expect("a status line")
            .split_whitespace()
            .nth(1)
            .expect("a status code")
            .parse()
            .expect("a numeric status");

        let mut headers = BTreeMap::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                // Repeated headers are joined the way RFC 9110 §5.2 says, so a
                // duplicate cannot hide behind a map insert.
                headers
                    .entry(name.trim().to_ascii_lowercase())
                    .and_modify(|existing: &mut String| {
                        existing.push_str(", ");
                        existing.push_str(value.trim());
                    })
                    .or_insert_with(|| value.trim().to_owned());
            }
        }

        let body = raw[split + 4..].to_vec();
        assert!(
            method != "HEAD" || body.is_empty(),
            "a HEAD response must not carry a body",
        );
        Self {
            status,
            headers,
            body,
        }
    }

    /// Lowercased on the way in as well as on the way out. Several assertions
    /// here are about a header being *absent*, and a case-sensitive lookup
    /// would let one of those pass against a header that is present.
    fn header(&self, name: &str) -> Option<String> {
        self.headers.get(&name.to_ascii_lowercase()).cloned()
    }

    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).expect("a UTF-8 body")
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|error| panic!("expected JSON, got {:?}: {error}", self.text()))
    }

    #[track_caller]
    fn assert_status(&self, expected: u16) {
        assert_eq!(
            self.status,
            expected,
            "unexpected status; body was {:?}",
            String::from_utf8_lossy(&self.body)
        );
    }

    /// `Cache-Control` is a set of directives, and their order is the header
    /// library's business rather than this server's.
    #[track_caller]
    fn assert_cache_control(&self, expected: &[&str]) {
        let value = self.header("cache-control").unwrap_or_default();
        let mut directives: Vec<_> = value.split(',').map(str::trim).collect();
        let mut expected = expected.to_vec();
        directives.sort_unstable();
        expected.sort_unstable();
        assert_eq!(directives, expected, "Cache-Control was {value:?}");
    }

    /// `Vary` is a list, and the CORS layer legitimately adds its own tokens to
    /// it. What matters is that a shared cache keys on `Accept`, since one URL
    /// serves both a page and JSON.
    #[track_caller]
    fn assert_varies_on_accept(&self) {
        let vary = self.header("vary").unwrap_or_default();
        assert!(
            vary.split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("Accept")),
            "Vary must include Accept, got {vary:?}"
        );
    }

    #[track_caller]
    fn assert_header(&self, name: &str, expected: &str) {
        assert_eq!(
            self.header(name).as_deref(),
            Some(expected),
            "header {name}; all headers were {:?}",
            self.headers
        );
    }
}

/// A bundle the build assembled from quads describes every graph it holds, and
/// the server reads those descriptions back under the names `/graphs` lists.
///
/// The whole pipeline in one test, because the two halves are only correct
/// together: the build names a view after the graph, lays the rows out in the
/// order a mapped bundle walks them, and records the ranges; the server parses
/// the name a request sends with the same grammar and reads those ranges.
#[test]
fn a_built_quad_bundle_describes_each_of_its_graphs() {
    const G1: &str = "http://example.org/g1";
    const UNNAMED: &str = "urn:x-kgf:unnamed";

    let deployment = Deployment::new();
    deployment.publish_built_quads("quads", "v1", WORKED_EXAMPLE_NQ, "2026-09-17T09:00:00Z");
    let server = deployment.serve();

    // The graphs the bundle holds, and the counts of the worked example.
    let graphs = server.get("/quads/v/v1/graphs");
    graphs.assert_status(200);
    let listed: Vec<(String, u64)> = graphs.json()["graphs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["g"]["value"].as_str().unwrap().to_owned(),
                entry["count"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        vec![
            (UNNAMED.to_owned(), 2),
            (G1.to_owned(), 2),
            ("http://example.org/g2".to_owned(), 1),
        ]
    );

    // Each graph has a description of its own, under the name it is listed by,
    // and its counts are the graph's rather than the dataset's.
    for (graph, triples) in [(UNNAMED, 2), (G1, 2), ("http://example.org/g2", 1)] {
        let view = kgf_server::url::encode_value(&format!("graph:{graph}"));
        let schema = server.get(&format!("/quads/v/v1/schema?view={view}"));
        schema.assert_status(200);
        let body = schema.json();
        assert_eq!(body["view"], format!("graph:{graph}"), "{graph}");
        assert_eq!(body["node"]["counts"]["triples"], triples, "{graph}");
    }
    // The union is what the dataset's own views describe, and it counts each
    // distinct triple once rather than once per graph.
    let whole = server.get("/quads/v/v1/schema?view=queryable");
    assert_eq!(whole.json()["node"]["counts"]["triples"], 3);

    // The persisted summary names the same graphs, and its links work.
    let summary = server.get("/quads/v/v1/summary?format=json");
    summary.assert_status(200);
    let summary = summary.json();
    let named: Vec<String> = summary["graphs"]
        .as_array()
        .expect("a quad bundle's summary names its graphs")
        .iter()
        .map(|entry| entry["graph"].as_str().unwrap().to_owned())
        .collect();
    // The card ranks by size; `/graphs` above lists by layer id.
    assert_eq!(named, vec![G1, UNNAMED, "http://example.org/g2"]);
    assert_eq!(summary["graphs_total"], 3);
    for entry in summary["graphs"].as_array().unwrap() {
        for link in ["schema", "fragment"] {
            let followed = server.get(&format!(
                "/quads/v/v1/{}",
                entry["links"][link].as_str().unwrap()
            ));
            followed.assert_status(200);
        }
    }

    // The browser page shows them too, with the way into each graph's triples.
    let page = server.request("GET", "/quads/v/v1/summary", &[("Accept", "text/html")]);
    page.assert_status(200);
    let page = String::from_utf8(page.body.to_vec()).unwrap();
    assert!(page.contains("Named graphs"), "{page}");
    assert!(
        page.contains("g=%3Chttp%3A%2F%2Fexample.org%2Fg1%3E"),
        "{page}"
    );

    // A graph this bundle does not hold is a 404 that says where to look, and
    // a view name of no known kind is refused before anything opens.
    let missing = server.get("/quads/v/v1/schema?view=graph%3Ahttp%3A%2F%2Fexample.org%2Fnope");
    missing.assert_status(404);
    assert!(
        missing.json()["detail"]
            .as_str()
            .unwrap()
            .contains("/graphs"),
        "{missing:?}",
        missing = missing.json()
    );
    let malformed = server.get("/quads/v/v1/schema?view=nonsense");
    malformed.assert_status(400);
    assert!(
        malformed.json()["detail"]
            .as_str()
            .unwrap()
            .contains("graph:<IRI>"),
        "{malformed:?}",
        malformed = malformed.json()
    );
}

/// A declared component is a graph with a name of its own: `/graphs` says which
/// graph holds it, `/schema` describes it under that name, and the design view
/// follows the canonical one rather than the merged whole.
#[test]
fn a_bundle_serves_the_components_it_declares() {
    const G1: &str = "http://example.org/g1";

    let deployment = Deployment::new();
    deployment.publish_built(
        "quads",
        "v1",
        WORKED_EXAMPLE_NQ,
        "2026-09-17T09:00:00Z",
        concat!(
            "components:\n",
            "  asserted: {role: source, graph: 'http://example.org/g1'}\n",
            "  closure: {role: entailment, graph: 'http://example.org/g2', ",
            "inputs: [asserted]}\n",
        ),
    );
    let server = deployment.serve();

    // The listing says which graphs are components, and which are not.
    let graphs = server.get("/quads/v/v1/graphs");
    graphs.assert_status(200);
    let claimed: Vec<(String, Option<String>)> = graphs.json()["graphs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["g"]["value"].as_str().unwrap().to_owned(),
                entry["component"].as_str().map(str::to_owned),
            )
        })
        .collect();
    assert_eq!(
        claimed,
        vec![
            ("urn:x-kgf:unnamed".to_owned(), None),
            (G1.to_owned(), Some("asserted".to_owned())),
            (
                "http://example.org/g2".to_owned(),
                Some("closure".to_owned())
            ),
        ]
    );

    // Its description is under the component's id, and asking for the graph's
    // own name says where to look instead.
    let component = server.get("/quads/v/v1/schema?view=component%3Aasserted");
    component.assert_status(200);
    assert_eq!(component.json()["node"]["counts"]["triples"], 2);
    let by_graph = server.get("/quads/v/v1/schema?view=graph%3Ahttp%3A%2F%2Fexample.org%2Fg1");
    by_graph.assert_status(404);
    assert!(
        by_graph.json()["detail"]
            .as_str()
            .unwrap()
            .contains("view=component:asserted"),
        "{:?}",
        by_graph.json()
    );

    // The design view is the canonical component, not the merged graph: two of
    // the three distinct triples.
    let design = server.get("/quads/v/v1/schema?view=design");
    design.assert_status(200);
    assert_eq!(design.json()["node"]["counts"]["triples"], 2);
    let queryable = server.get("/quads/v/v1/schema?view=queryable");
    assert_eq!(queryable.json()["node"]["counts"]["triples"], 3);

    // And the manifest publishes what was declared.
    let manifest = server.get("/quads/v/v1/manifest").json();
    assert_eq!(manifest["components"][0]["id"], "asserted");
    assert_eq!(manifest["components"][1]["role"], "entailment");
    assert_eq!(manifest["components"][1]["inputs"][0], "asserted");
}
