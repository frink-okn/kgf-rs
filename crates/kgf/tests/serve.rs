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

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use kgf_server::service::Service;
use kgf_store::testing::{Fixture, TINY_NT};
use sha2::{Digest, Sha256};

/// A second fixture graph, so two versions of one dataset differ in content and
/// therefore in `content_digest`.
const GROWN_NT: &str = concat!(
    "<http://example.org/alice> <http://example.org/name> \"Alice\" .\n",
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> .\n",
    "<http://example.org/bob> <http://example.org/name> \"Bob\" .\n",
    "<http://example.org/carol> <http://example.org/name> \"Carol\" .\n",
);

#[test]
fn the_url_space_answers_over_a_real_listener() {
    let deployment = Deployment::new();
    deployment.publish("tox", "2026-01-09", TINY_NT, "2026-01-09T09:00:00Z");
    deployment.publish("tox", "2026-06-01", GROWN_NT, "2026-06-01T14:03:22Z");
    deployment.publish("atlas", "v1", TINY_NT, "2020-01-01T00:00:00Z");
    let server = deployment.serve();

    // `/` — the service descriptor: what is hosted, and the caps a client is
    // told to read rather than assume (doc 03 §3.1).
    let root = server.get("/");
    root.assert_status(200);
    root.assert_cache_control(&["public", "max-age=300"]);
    root.assert_varies_on_accept();
    let descriptor = root.json();
    assert_eq!(descriptor["datasets"], serde_json::json!(["atlas", "tox"]));
    assert_eq!(descriptor["caps"]["max_limit"], 10_000);
    assert_eq!(descriptor["caps"]["max_bindings"], 1_000);
    assert_eq!(descriptor["implementation"]["protocol"], "1");

    // `/{dataset}` — the release history, and which release is current.
    let dataset = server.get("/tox");
    dataset.assert_status(200);
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
    // would silently turn a body-carrying QUERY into something else (§3.2).
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
        // §3.6.1 says every error carries a code; RFC 9110 §15.5.6 says a 405
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
    let search_page = server.request(
        "GET",
        "/tox/v/v1/search?q=Alice&predicate=ex%3Aname",
        &[("Accept", "text/html")],
    );
    search_page.assert_status(200);
    search_page.assert_header("content-type", "text/html; charset=utf-8");
    let search_html = search_page.text();
    assert!(search_html.contains(">ex:alice</a>"), "{search_html}");
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
    // §3.6.1 says every one, which includes the ones an off-the-shelf router
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
    // statuses, three remedies (§3.6.1).
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
    server
        .request(
            "GET",
            "/tox/v/v1/manifest",
            &[("If-None-Match", etag.as_str())],
        )
        .assert_status(304);
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

        server
            .request("GET", path, &[("If-None-Match", etag.as_str())])
            .assert_status(304);
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
    // §3.6.1 says every error response carries a code, which has to include
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
    // so the version is scanned and described but cannot be served. Doc 20
    // §20.8: there is no fallback, so the bundle is refused at open.
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
    // Loud rather than degraded (doc 20 §20.8): the alternative is a version
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
fn the_operations_answer_over_the_wire_with_their_completeness_on_the_headers() {
    // `operations.rs` checks what the read operations *answer*, headless. What
    // only a socket can show is the rest of the response: §3.6's metadata in
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

    // The body says it is truncated, and so do the headers — §3.6 requires
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
    server
        .request(
            "GET",
            &format!("{base}/fragment?limit=2"),
            &[("If-None-Match", etag.as_str())],
        )
        .assert_status(304);

    // And the same URL is a page in a browser.
    let html = server.request(
        "GET",
        &format!("{base}/fragment?limit=2"),
        &[("Accept", "text/html")],
    );
    html.assert_header("content-type", "text/html; charset=utf-8");
    assert!(html.text().contains("Next page"));
    // The completeness headers ride the page as well, since they are what an
    // intermediary reads without parsing a body it cannot parse.
    html.assert_header("kgf-complete", "false");

    // `latest` reaches the operations with the query intact.
    server
        .get("/tox/latest/fragment?limit=2")
        .assert_header("location", "/tox/v/2026-06-01/fragment?limit=2");
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
fn an_operation_a_bundle_does_not_declare_is_refused_before_it_is_opened() {
    // §3.4.7 is an optional capability, so a bundle that does not declare it is
    // answered 501 — the request is well formed and the shortfall is what this
    // bundle offers, which is exactly what `capability_not_available` says.
    let deployment = Deployment::new();
    deployment.publish("tox", "v1", TINY_NT, "2026-06-01T14:03:22Z");
    let server = deployment.serve();
    server.get("/tox/v/v1/sample?n=2").assert_status(200);

    // Withdraw it, and only `/sample` changes.
    let manifest = deployment.bundle("tox", "v1").join("manifest.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    document["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove("sample");
    std::fs::write(&manifest, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let server = deployment.serve();

    let refused = server.get("/tox/v/v1/sample?n=2");
    refused.assert_status(501);
    assert_eq!(refused.json()["code"], "capability_not_available");
    server.get("/tox/v/v1/fragment?limit=2").assert_status(200);
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

    fn publish_bundle(
        &self,
        dataset: &str,
        version: &str,
        source: &str,
        created: &str,
        text: bool,
    ) {
        let bundle = self.bundle(dataset, version);
        let fixture = Fixture::build(source);
        let fixture = if text { fixture.with_text() } else { fixture };
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
        let path = bundle.join("manifest.json");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        document["created"] = serde_json::json!(created);
        std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    }

    fn serve(&self) -> Server {
        self.serve_with(kgf_server::Caps::new())
    }

    fn serve_with(&self, caps: kgf_server::Caps) -> Server {
        let mut config = kgf_server::Config::new(
            kgf::serve::published_root(self.root.path()).expect("a published root"),
            "127.0.0.1:0".parse().unwrap(),
        );
        config.caps = caps;
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
        let mut stream = TcpStream::connect(self.address).expect("connect");
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

    fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).cloned()
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
    /// serves both a page and JSON (doc 03 §3.6).
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
