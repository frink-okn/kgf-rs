//! Doc 03 §3.4's read operations, against the store's own answers.
//!
//! Headless on purpose. `kgf-store` is already differential against `hdtc
//! search`, so what is worth checking here is the layer unit 14 added — term
//! resolution, paging, cursors, cardinality — and checking it through a socket
//! would mean ten thousand connections to test three hundred patterns. The HTTP
//! surface of the same operations is in `serve.rs`, over a real listener.
//!
//! This is also the only place outside `kgf-store` that can open a bundle at
//! all: the safe constructors for a mapped bundle are crate-private there, and
//! `kgf::serve::published_root` is the one sanctioned way in (see CLAUDE.md's
//! note on the workspace's second `unsafe`).

use std::sync::Arc;

use clap::Parser;
use kgf_server::answer::{self, Target};
use kgf_server::representation::Representation;
use kgf_server::request;
use kgf_server::service::{Release, Service};
use kgf_server::url::Params;
use kgf_server::{Budgets, Caps, Limits};
use kgf_store::catalog::BundleId;
use kgf_store::pattern::IdPattern;
use kgf_store::testing::Fixture;
use kgf_store::{IdTriple, Role, Store, TermId};

/// Small, and every term shape in it.
///
/// `alice` is a subject *and* an object, which is what makes `describe`'s two
/// halves both non-empty; `alice ex:self alice` is the self-loop that appears
/// in each of them; and `_:b1` keeps a blank node in the dictionary.
const GRAPH: &str = concat!(
    "<http://example.org/alice> <http://example.org/name> \"Alice\" .\n",
    "<http://example.org/alice> <http://example.org/label> \"Alice\"@en .\n",
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> .\n",
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/carol> .\n",
    "<http://example.org/alice> <http://example.org/self> <http://example.org/alice> .\n",
    "<http://example.org/bob> <http://example.org/name> \"Bob\" .\n",
    "<http://example.org/bob> <http://example.org/knows> <http://example.org/alice> .\n",
    "<http://example.org/carol> <http://example.org/name> \"Carol\" .\n",
    "<http://example.org/carol> <http://example.org/age> ",
    "\"31\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n",
    "_:b1 <http://example.org/note> \"a blank subject\" .\n",
);

#[test]
fn every_pattern_shape_answers_what_the_store_answers() {
    // The property the plan asks for: for every one of doc 20 §20.2's eight
    // shapes, over every term in the bundle, `/fragment` returns the store's
    // rows and `/count` returns their number. Every term is written back into
    // the request in §3.3 syntax and parsed again, so the trip through the
    // dictionary and back is part of what is under test.
    let served = Served::new();
    let store = served.store();
    let terms = served.terms(&store);

    let mut shapes = 0;
    let mut nonempty = 0;
    for subject in options(&terms, Role::Subject) {
        for predicate in options(&terms, Role::Predicate) {
            for object in options(&terms, Role::Object) {
                let query = pattern_query(&terms, subject, predicate, object);
                let expected = served.expect(&store, &terms, subject, predicate, object);
                shapes += 1;
                nonempty += usize::from(!expected.is_empty());

                let answer = served.fragment(&store, &format!("{query}&limit=10000"));
                assert_eq!(rows(&answer), expected, "GET /fragment?{query}");
                assert_eq!(
                    answer["cardinality"],
                    serde_json::json!({"value": expected.len(), "exact": true}),
                    "GET /fragment?{query}"
                );
                assert_eq!(answer["complete"], serde_json::json!(true), "{query}");

                // The same number, from the operation that promises it without
                // enumerating anything.
                let counted = served.count(&store, &query);
                assert_eq!(
                    counted["count"]["value"].as_u64().unwrap() as usize,
                    expected.len(),
                    "GET /count?{query}"
                );
                assert_eq!(counted["count"]["exact"], serde_json::json!(true));

                // And `vars` names exactly the positions a row carries.
                let vars: Vec<&str> = answer["vars"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|var| var.as_str().unwrap())
                    .collect();
                for row in answer["rows"].as_array().unwrap() {
                    let keys: Vec<&str> = row
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(String::as_str)
                        .collect();
                    assert_eq!(keys, vars, "a row must carry its vars and nothing else");
                }
            }
        }
    }
    assert!(
        shapes >= 300 && nonempty > 20,
        "{shapes} shapes, {nonempty} non-empty"
    );
}

#[test]
fn exhaustive_paging_at_adversarial_sizes_yields_each_row_once() {
    // Doc 20 §20.9's paging property, driven through real cursor tokens rather
    // than through positions: 1 and 2 are the sizes an off-by-one survives, 3
    // is prime, and 10 000 is the cap — a page that ends exactly at the last
    // row must say `complete`, not hand out a cursor to nothing.
    let served = Served::new();
    let store = served.store();
    let terms = served.terms(&store);

    for subject in options(&terms, Role::Subject) {
        for predicate in options(&terms, Role::Predicate) {
            for object in options(&terms, Role::Object) {
                let query = pattern_query(&terms, subject, predicate, object);
                let expected = served.expect(&store, &terms, subject, predicate, object);
                for size in [1usize, 2, 3, 10_000] {
                    let paged = served.page_through(&store, &query, size);
                    assert_eq!(paged, expected, "?{query} at limit={size}");
                }
            }
        }
    }
}

#[test]
fn bindings_enumerate_in_input_order_page_globally_and_count_per_row() {
    let served = Served::new();
    let store = served.store();
    let mut body = serde_json::json!({
        "pattern": {"s": "?person", "p": "ex:knows", "o": "?known"},
        "bindings": {
            "vars": ["?person"],
            "rows": [["ex:alice"], ["ex:bob"], ["ex:missing"]]
        },
        "limit": 1
    });

    let first = served.binding_fragment(&store, &body);
    let first_cursor = first["next"].as_str().expect("the first page continues");
    let html = served.binding_fragment_html(&store, &body);
    assert!(
        html.contains(&format!("<code>{first_cursor}</code>")),
        "a body-addressed page must show the cursor its instructions refer to: {html}"
    );

    let mut found = Vec::new();
    loop {
        let page = served.binding_fragment(&store, &body);
        assert_eq!(
            page["cardinality"],
            serde_json::json!({"value": 3, "exact": true})
        );
        for row in page["rows"].as_array().unwrap() {
            found.push((
                row["binding"].as_u64().unwrap(),
                row["o"]["value"].as_str().unwrap().to_owned(),
            ));
        }
        match page["next"].as_str() {
            Some(cursor) => body["cursor"] = serde_json::json!(cursor),
            None => break,
        }
    }
    assert_eq!(
        found,
        vec![
            (0, "http://example.org/bob".to_owned()),
            (0, "http://example.org/carol".to_owned()),
            (1, "http://example.org/alice".to_owned()),
        ]
    );

    let counted = served.binding_count(&store, &body);
    assert_eq!(
        counted["counts"],
        serde_json::json!([
            {"binding": 0, "count": {"value": 2, "exact": true}},
            {"binding": 1, "count": {"value": 1, "exact": true}},
            {"binding": 2, "count": {"value": 0, "exact": true}},
        ])
    );
}

#[test]
fn a_binding_cursor_can_start_the_first_predicate_of_the_next_row() {
    let served = Served::new();
    let store = served.store();
    let mut body = serde_json::json!({
        "pattern": {"s": "?s", "p": "?p", "o": "?o"},
        "bindings": {
            "vars": ["?s", "?o"],
            "rows": [["ex:alice", "ex:alice"], ["ex:bob", "ex:alice"]]
        },
        "limit": 1
    });
    let first = served.binding_fragment(&store, &body);
    assert_eq!(first["rows"][0]["binding"], 0);
    body["cursor"] = first["next"].clone();
    let second = served.binding_fragment(&store, &body);
    assert_eq!(second["rows"][0]["binding"], 1);
    assert_eq!(second["rows"][0]["p"]["value"], "http://example.org/knows");
    assert_eq!(second["complete"], true);
}

#[test]
fn a_cursor_past_the_end_is_stale_rather_than_an_empty_last_page() {
    // Unit 10 left this to unit 14 because it needs the store, and it is two
    // rules rather than one: a permutation position is an offset bounded by the
    // cardinality, while an `s ? o` position is the last predicate id returned
    // and is bounded by the predicate id space. Checking the second against a
    // cardinality would reject a live cursor.
    use kgf_server::cursor::{Cursor, PositionSpace};

    let served = Served::new();
    let store = served.store();

    // `? ? ?` has ten rows in SPO order.
    let request = served.parse_fragment("limit=1");
    let inside = Cursor::at(&request.binding, PositionSpace::Spo, 9).encode();
    let at_the_end = Cursor::at(&request.binding, PositionSpace::Spo, 10).encode();
    let beyond = Cursor::at(&request.binding, PositionSpace::Spo, 5_000).encode();

    assert!(
        served
            .try_fragment(&store, &format!("limit=1&cursor={inside}"))
            .is_ok()
    );
    for token in [at_the_end, beyond] {
        let refused = served
            .try_fragment(&store, &format!("limit=1&cursor={token}"))
            .expect_err("a position at or past the end is not a position");
        assert_eq!(refused.code(), kgf_server::envelope::ErrorCode::StaleCursor);
    }

    // A position from the wrong space, for a pattern that reads another one.
    let wrong_space = Cursor::at(&request.binding, PositionSpace::Ops, 0).encode();
    assert!(
        served
            .try_fragment(&store, &format!("cursor={wrong_space}"))
            .is_err()
    );

    // And an `s ? o` cursor, whose position is a predicate id: the fixture has
    // six predicates, so 6 resumes and 7 does not — even though the answer has
    // exactly one row.
    let query = "s=%3Chttp%3A%2F%2Fexample.org%2Falice%3E&o=%3Chttp%3A%2F%2Fexample.org%2Falice%3E";
    let request = served.parse_fragment(query);
    let answered = served.fragment(&store, query);
    assert_eq!(answered["cardinality"]["value"], serde_json::json!(1));
    for (position, expected) in [(1u64, true), (6, true), (7, false), (0, false)] {
        let token = Cursor::at(&request.binding, PositionSpace::Predicate, position).encode();
        assert_eq!(
            served
                .try_fragment(&store, &format!("{query}&cursor={token}"))
                .is_ok(),
            expected,
            "predicate position {position}"
        );
    }
}

#[test]
fn describe_is_two_enumerations_that_page_as_one() {
    let served = Served::new();
    let store = served.store();
    let alice = "iri=%3Chttp%3A%2F%2Fexample.org%2Falice%3E";

    let whole = served.describe(&store, alice);
    let rows: Vec<(String, String, String, String)> = describe_rows(&whole);
    assert_eq!(whole["cardinality"]["value"], serde_json::json!(7));
    assert_eq!(rows.len(), 7);
    assert_eq!(whole["complete"], serde_json::json!(true));

    // Out-edges first, then in-edges, and every row says which.
    let directions: Vec<&str> = rows.iter().map(|row| row.0.as_str()).collect();
    assert_eq!(directions, ["out", "out", "out", "out", "out", "in", "in"]);

    // The self-loop is genuinely in both halves, and the column is what makes
    // the second copy legible rather than a duplicate.
    let loops: Vec<_> = rows
        .iter()
        .filter(|row| row.2.ends_with("/self"))
        .map(|row| row.0.as_str())
        .collect();
    assert_eq!(loops, ["out", "in"]);

    // Paging crosses the boundary between the halves without losing or
    // repeating a row, at every size that can land on it.
    for size in [1, 2, 3, 5, 6, 7, 8] {
        let paged = served.page_through_describe(&store, alice, size);
        assert_eq!(paged, rows, "describe at limit={size}");
    }

    // One direction at a time is the same rows, split.
    let out = describe_rows(&served.describe(&store, &format!("{alice}&direction=out")));
    let inward = describe_rows(&served.describe(&store, &format!("{alice}&direction=in")));
    assert_eq!(out.len(), 5);
    assert_eq!(inward.len(), 2);
    assert_eq!([out, inward].concat(), rows);

    // A resource the bundle does not hold is an empty answer that says why.
    let unknown = served.describe(&store, "iri=%3Chttp%3A%2F%2Fexample.org%2Fnobody%3E");
    assert_eq!(unknown["cardinality"]["value"], serde_json::json!(0));
    assert_eq!(unknown["absent_terms"], serde_json::json!(["iri"]));

    // A literal has incoming edges like any other object, and a bundle that
    // holds one must be able to answer for it.
    let literal = served.describe(&store, "iri=%22Alice%22%40en");
    assert_eq!(literal["cardinality"]["value"], serde_json::json!(1));
    assert!(literal.get("absent_terms").is_none());
}

#[test]
fn a_sample_draws_real_members_and_draws_them_the_same_way_twice() {
    let served = Served::new();
    let store = served.store();
    let store = &store;

    let everything: Vec<_> = rows(&served.fragment(store, "limit=10000"));

    for n in [1usize, 3, 9, 10, 25] {
        let drawn = rows(&served.sample(store, &format!("n={n}&seed=42")));
        assert_eq!(drawn.len(), n.min(everything.len()), "n={n}");
        // Members of the result set, each at most once.
        let unique: std::collections::HashSet<_> = drawn.iter().collect();
        assert_eq!(
            unique.len(),
            drawn.len(),
            "a sample must not repeat a member"
        );
        assert!(drawn.iter().all(|row| everything.contains(row)), "n={n}");
        // §3.4.7: deterministic for a given seed and version, which is what
        // lets the response carry an immutable validator at all.
        assert_eq!(
            drawn,
            rows(&served.sample(store, &format!("n={n}&seed=42")))
        );
    }

    // A different seed is a different draw, once there is room for one.
    assert_ne!(
        rows(&served.sample(store, "n=3&seed=1")),
        rows(&served.sample(store, "n=3&seed=999"))
    );

    // Asking for more than there is returns everything, and says so.
    let all = served.sample(store, "n=1000");
    assert_eq!(rows(&all).len(), everything.len());
    assert_eq!(all["complete"], serde_json::json!(true));
    assert_eq!(all["next"], serde_json::json!(null));

    // `s ? o` samples from the bounded probe run once (§3.4.7's exception).
    let loop_query = "s=%3Chttp%3A%2F%2Fexample.org%2Falice%3E\
                      &o=%3Chttp%3A%2F%2Fexample.org%2Falice%3E&n=5";
    let looped = served.sample(store, loop_query);
    assert_eq!(rows(&looped).len(), 1);
    assert_eq!(looped["cardinality"]["value"], serde_json::json!(1));
}

#[test]
fn a_response_stops_at_the_byte_budget_and_resumes_from_where_it_stopped() {
    // §3.5's `max_response_bytes`, and the reason it cannot be a cap: "a row
    // cap is not a byte cap (one legal literal can be megabytes)". `limit`
    // bounds rows and nothing bounds what a row weighs, so a bundle of long
    // literals answers a legal request with an illegal response — which is the
    // bounded-cost thesis failing, not a rounding error.
    let served = Served::new();
    let store = served.store();

    // Small enough that two rows of this fixture will not fit.
    let budgets = Budgets {
        max_response_bytes: 200,
        ..Budgets::new()
    };

    let mut collected = Vec::new();
    let mut cursor: Option<String> = None;
    let mut reasons = Vec::new();
    for _ in 0..100 {
        let query = match &cursor {
            Some(token) => format!("limit=10000&cursor={token}"),
            None => "limit=10000".to_owned(),
        };
        let answer = served.fragment_within(&store, &query, &budgets);
        let page = rows(&answer);
        assert!(!page.is_empty(), "a page must always carry a row");
        collected.extend(page);
        if answer["complete"].as_bool().unwrap() {
            break;
        }
        reasons.push(answer["truncation_reason"].as_str().unwrap().to_owned());
        cursor = Some(answer["next"].as_str().expect("a cursor").to_owned());
    }

    // The budget did the truncating, not the limit — the request asked for
    // every row and the cap allowed it.
    assert!(!reasons.is_empty(), "the budget must have bitten");
    assert!(
        reasons.iter().all(|reason| reason == "response_bytes"),
        "{reasons:?}"
    );
    // And paging on that cursor loses and repeats nothing, exactly as a page
    // limit does.
    assert_eq!(collected, rows(&served.fragment(&store, "limit=10000")));

    // A budget smaller than a single row still yields that row. The
    // alternative is an empty page whose cursor resumes where it was issued,
    // which a client would follow forever.
    let impossible = Budgets {
        max_response_bytes: 1,
        ..Budgets::new()
    };
    let answer = served.fragment_within(&store, "limit=10000", &impossible);
    assert_eq!(rows(&answer).len(), 1);
    assert_eq!(
        answer["truncation_reason"],
        serde_json::json!("response_bytes")
    );
    assert!(answer["next"].is_string());
}

#[test]
fn a_sample_that_spends_the_byte_budget_says_so_and_offers_no_cursor() {
    // §3.4.7 draws `n` members and never pages, so there is no position for a
    // cursor to name — and the budget can still bite, because a bundle may hold
    // a literal of any size. Returning fewer members and calling it complete
    // would be the silent truncation §3.6 prohibits, so it reports the reason
    // with no `next`, the shape `cell_overflow` already uses.
    let served = Served::new();
    let store = served.store();
    let budgets = Budgets {
        max_response_bytes: 200,
        ..Budgets::new()
    };

    let whole = served.sample(&store, "n=10&seed=1");
    assert_eq!(whole["complete"], serde_json::json!(true));

    let short = served.sample_within(&store, "n=10&seed=1", &budgets);
    assert!(rows(&short).len() < rows(&whole).len());
    assert!(!rows(&short).is_empty());
    assert_eq!(short["complete"], serde_json::json!(false));
    assert_eq!(
        short["truncation_reason"],
        serde_json::json!("response_bytes")
    );
    assert_eq!(short["next"], serde_json::json!(null));
    // The cardinality still describes the set drawn *from*, which is what makes
    // a short sample interpretable rather than merely small.
    assert_eq!(short["cardinality"], whole["cardinality"]);
}

#[test]
fn an_absent_term_is_an_empty_answer_that_says_which_position() {
    // Unit 11 chose not to reject unusual IRIs at the edge, on the grounds that
    // a diagnostic is worth more than a syntax error. This is the diagnostic.
    let served = Served::new();
    let store = served.store();

    let answer = served.fragment(&store, "s=%3Chttp%3A%2F%2Fexample.org%2Fnobody%3E");
    assert_eq!(answer["cardinality"]["value"], serde_json::json!(0));
    assert_eq!(answer["rows"], serde_json::json!([]));
    assert_eq!(answer["absent_terms"], serde_json::json!(["s"]));
    assert_eq!(answer["complete"], serde_json::json!(true));

    // Per role, because a term can be present as one thing and not another:
    // `ex:name` is a predicate and never an object.
    let as_object = served.fragment(&store, "o=%3Chttp%3A%2F%2Fexample.org%2Fname%3E");
    assert_eq!(as_object["absent_terms"], serde_json::json!(["o"]));
    let as_predicate = served.fragment(&store, "p=%3Chttp%3A%2F%2Fexample.org%2Fname%3E");
    assert!(as_predicate.get("absent_terms").is_none());

    // A pattern that is present says nothing at all.
    assert!(served.fragment(&store, "").get("absent_terms").is_none());
}

#[test]
fn every_operation_renders_a_page_as_well_as_json() {
    // The crate's rule: a route implements `Resource` or it does not compile.
    // What that cannot check is that the page is *usable*, which for these
    // means the rows are there and every term links onward.
    let served = Served::new();
    let store = served.store();

    let page = served.render(&store, "fragment", "limit=2", Representation::Html);
    assert!(page.contains("<h1>Triple pattern</h1>"));
    assert!(page.contains("Fragment · tox 2026-06-01"));
    assert!(page.find("Fragment · tox 2026-06-01") < page.find("<h1>Triple pattern</h1>"));
    assert!(page.contains(">ex:alice</a>"));
    assert!(page.contains("title=\"http://example.org/alice\""));
    // A named term — subject and predicate alike — links to its own
    // neighborhood, which is what makes the page a way into the data. The
    // visible CURIE does not leak into the target: links retain the portable
    // full IRI spelling.
    assert!(page.contains("/tox/v/2026-06-01/describe?iri=%3Chttp%3A%2F%2Fexample.org%2Falice%3E"));
    assert!(page.contains("/tox/v/2026-06-01/describe?iri=%3Chttp%3A%2F%2Fexample.org%2Fknows%3E"));
    // A truncated page offers the next one.
    assert!(page.contains("Next page"));
    assert!(page.contains("cursor="));

    // A fragment page is a table of triples even though the JSON row carries
    // variables only. Bound request terms are restored in position, remain
    // drill-down links, and s/p/o form one aligned row in the request summary.
    let bound = served.render(
        &store,
        "fragment",
        "s=ex:alice&p=ex:knows&limit=2",
        Representation::Html,
    );
    assert!(bound.contains("<th>s</th><th>p</th><th>o</th>"));
    assert!(bound.contains(">ex:alice</a>"));
    assert!(bound.contains(">ex:knows</a>"));
    assert!(bound.contains(">ex:bob</a>"));

    // Even a fully bound, one-row fragment shows the triple instead of
    // replacing it with prose about a row that has no variable cells.
    let fully_bound = served.render(
        &store,
        "fragment",
        "s=ex:alice&p=ex:self&o=ex:alice",
        Representation::Html,
    );
    assert!(fully_bound.contains("<th>s</th><th>p</th><th>o</th>"));
    assert_eq!(fully_bound.matches(">ex:alice</a>").count(), 2);

    // Both JSON affordances carry one clean selector, whatever the request
    // asked for — neither appends it to the existing HTML selector.
    let pinned = served.render(
        &store,
        "fragment",
        "limit=2&format=html",
        Representation::Html,
    );
    assert_eq!(pinned.matches("format=json").count(), 2);
    assert!(!pinned.contains("format=html&amp;format=json"));

    for (operation, query, heading, label) in [
        ("count", "p=ex:knows", "Pattern count", "Count"),
        ("describe", "iri=ex:alice", "ex:alice", "Describe"),
        ("sample", "n=2", "Sample", "Sample"),
    ] {
        let page = served.render(&store, operation, query, Representation::Html);
        assert!(
            page.to_ascii_lowercase().starts_with("<!doctype html>"),
            "{operation} must answer a whole document"
        );
        assert!(page.contains(&format!("<h1>{heading}</h1>")));
        assert!(page.contains(&format!("{label} · tox 2026-06-01")));
        assert!(
            page.find(&format!("{label} · tox 2026-06-01"))
                < page.find(&format!("<h1>{heading}</h1>"))
        );
    }

    // The described term is the page focus, not a link back to the page the
    // reader is already on. Neighbors remain links, which makes the two ends
    // of each in/out row visually distinct.
    let described = served.render(&store, "describe", "iri=ex:alice", Representation::Html);
    assert!(described.contains("<span class=\"term term-static\""));
    assert!(!described.contains(">ex:alice</a>"));
    assert!(described.contains(">ex:bob</a>"));

    // A datatype IRI is the other IRI-shaped part of an RDF term. It receives
    // the same display treatment while the literal link remains full syntax.
    let typed = served.render(
        &store,
        "fragment",
        "s=ex:carol&p=ex:age",
        Representation::Html,
    );
    // The lexical form and its datatype are one term set in two weights: the
    // qualifier is dim beside the value rather than fused into one string.
    assert!(typed.contains("&quot;31&quot;<span class=\"t-qual\">^^xsd:integer</span>"));
    assert!(typed.contains("title=\"http://www.w3.org/2001/XMLSchema#integer\""));
    assert!(
        typed
            .contains("o=%2231%22%5E%5E%3Chttp%3A%2F%2Fwww.w3.org%2F2001%2FXMLSchema%23integer%3E")
    );

    // The summary is an echo, not a term rendering: it preserves exactly the
    // spelling the caller supplied even when the manifest offers a CURIE.
    let echoed = served.render(
        &store,
        "count",
        "p=%3Chttp%3A%2F%2Fexample.org%2Fknows%3E",
        Representation::Html,
    );
    assert!(echoed.contains("<code>&lt;http://example.org/knows&gt;</code>"));
}

#[test]
fn a_text_constraint_ranks_literals_and_resolves_them_through_the_permutations() {
    // Doc 19 §19.2.2's composition: the index returns object dictionary ids,
    // and the statements come from the permutations the store already has. So
    // what this checks is that the two halves line up — every row's object is a
    // literal the query matched, and every statement of a matched literal is
    // there.
    let served = Served::with_text();
    let store = served.store();

    let answer = served.fragment(&store, "o.text=Alice&limit=1000");
    let matched = rows(&answer);
    assert!(!matched.is_empty(), "the fixture holds \"Alice\"");
    assert_eq!(answer["vars"], serde_json::json!(["s", "p", "o"]));

    // §3.4.1 echoes the constraint in the position it constrains.
    assert_eq!(
        answer["pattern"],
        serde_json::json!({"s": null, "p": null, "o": {"text": "Alice"}})
    );

    // Every row carries a score, and a matching literal's every statement is
    // present — `"Alice"` is the object of one triple and `"Alice"@en` of
    // another, so a hit on either brings its own rows and no others.
    for row in answer["rows"].as_array().unwrap() {
        assert!(row["score"].is_number(), "a ranked row carries its score");
        let object = row["o"]["value"].as_str().unwrap();
        assert!(
            object.to_ascii_lowercase().contains("alice"),
            "matched {object}"
        );
    }

    // A page that started at the beginning and ran out is the whole answer, so
    // the count is exact rather than an estimate of what the client can see.
    assert_eq!(answer["complete"], serde_json::json!(true));
    assert_eq!(
        answer["cardinality"],
        serde_json::json!({"value": matched.len(), "exact": true})
    );

    // And the rows are exactly the statements of the matched literals, which is
    // the store's own answer for each of them.
    let expected: usize = ["\"Alice\"", "\"Alice\"@en"]
        .iter()
        .map(|literal| {
            let query = format!("o={}", kgf_server::url::encode_value(literal));
            rows(&served.fragment(&store, &query)).len()
        })
        .sum();
    assert_eq!(matched.len(), expected);
}

#[test]
fn a_ranked_page_resumes_where_it_stopped_at_every_size() {
    // The property doc 20 §20.9 asks of every enumeration, over the one whose
    // position is not an enumeration order at all. Two things make it hard: a
    // hit fans out, so a page can stop inside one; and `s ? ?` with a text
    // constraint resolves per hit to `s ? o`, whose positions are predicate
    // ids rather than offsets. Both shapes are here.
    let served = Served::with_text();
    let store = served.store();

    for query in [
        "o.text=Alice",
        "o.text=Alice&p=%3Chttp%3A%2F%2Fexample.org%2Flabel%3E",
        "o.text=Alice&s=%3Chttp%3A%2F%2Fexample.org%2Falice%3E",
        "o.text=a",
        "o.text=nosuchword",
    ] {
        let whole = rows(&served.fragment(&store, &format!("{query}&limit=1000")));
        for size in [1usize, 2, 3] {
            let paged = served.page_through(&store, query, size);
            assert_eq!(paged, whole, "?{query} at limit={size}");
        }
    }
}

#[test]
fn a_ranked_cursor_must_name_a_real_hit_and_statement_offset() {
    let served = Served::with_text();
    let store = served.store();
    let request = served.parse_fragment("o.text=Alice&limit=1");

    for token in [
        kgf_server::cursor::Cursor::at_rank(&request.binding, 0, u64::MAX).encode(),
        kgf_server::cursor::Cursor::at_rank(&request.binding, u64::MAX, 0).encode(),
    ] {
        let error = served
            .try_fragment(&store, &format!("o.text=Alice&limit=1&cursor={token}"))
            .expect_err("an invented ranked position must be stale");
        assert_eq!(error.code(), kgf_server::envelope::ErrorCode::StaleCursor);
    }
}

#[test]
fn a_text_page_shows_its_constraint_and_ranking_in_html() {
    let served = Served::with_text();
    let store = served.store();
    let page = served.render(
        &store,
        "fragment",
        "o.text=Alice&limit=10",
        Representation::Html,
    );

    for visible in ["o.text", "Alice", "score", "match_kind", "exact"] {
        assert!(page.contains(visible), "HTML omitted {visible}: {page}");
    }
}

#[test]
fn a_text_count_is_exact_after_pattern_intersection() {
    let served = Served::with_text();
    let store = served.store();

    let counted = served.count(&store, "o.text=Alice");
    let count = &counted["count"];
    assert_eq!(count["exact"], serde_json::json!(true));
    assert_eq!(
        count["value"].as_u64().unwrap() as usize,
        rows(&served.fragment(&store, "o.text=Alice&limit=1000")).len()
    );

    // The text index returns global matching object IDs, but count intersects
    // each one with the remaining pattern. A predicate on which those literals
    // never occur must therefore be exactly zero, not a positive global hit
    // count presented as an estimate or lower bound.
    let filtered = served.count(
        &store,
        "o.text=Alice&p=%3Chttp%3A%2F%2Fexample.org%2Fknows%3E",
    );
    assert_eq!(
        filtered["count"],
        serde_json::json!({"value": 0, "exact": true})
    );

    let budgets = Budgets {
        candidate_budget: 1,
        ..Budgets::new()
    };
    let partial = served.fragment_within(
        &store,
        "o.text=Alice&p=%3Chttp%3A%2F%2Fexample.org%2Fknows%3E&limit=10",
        &budgets,
    );
    assert_eq!(partial["complete"], serde_json::json!(false));
    assert_eq!(partial["cardinality"]["value"], serde_json::json!(0));
    assert!(partial["cardinality"].get("min").is_none());
    assert!(partial["cardinality"].get("distinct_objects").is_none());
}

#[test]
fn o_text_needs_an_index_and_will_not_share_the_object_position() {
    let served = Served::with_text();
    let store = served.store();

    // `o` names one term and `o.text` ranks many, so a request carrying both is
    // asking two incompatible questions rather than narrowing.
    let refused = served
        .try_fragment(&store, "o.text=Alice&o=%22Alice%22")
        .expect_err("o and o.text both constrain the object");
    assert_eq!(
        refused.code(),
        kgf_server::envelope::ErrorCode::MalformedRequest
    );

    // A bundle with no index declares no `search`, and the store offers no
    // searcher — one condition, and the manifest is the half read first.
    let plain = Served::new();
    assert!(plain.store().text().is_none());
    assert!(!plain.release().declares(kgf_store::Capability::Search));
    assert!(served.release().declares(kgf_store::Capability::Search));
}

#[test]
fn a_tight_candidate_budget_pages_to_its_bound_and_then_stops() {
    // Two promises, and the first is the one that broke. §3.5's budget bounds
    // what one *request* examines, not how far a client may page. Read the
    // other way — as a ceiling on the rank a request may reach — paging stops
    // dead the moment a client arrives at the budget: the search returns at
    // most `budget` hits, skipping to the resume rank yields nothing, and the
    // cursor points at a rank already passed, so the client asks forever.
    //
    // The second promise is what bounds it instead. A top-k index has no
    // cursor, so a page holds a hit list as long as the rank it reached; the
    // budget is therefore also how deep the ranking is pageable, and reaching
    // that depth ends the enumeration *without* a cursor rather than with one
    // that goes nowhere.
    //
    // Neither shows up at the published default of 1 000 000 over a ten-triple
    // fixture, which is why this lowers it.
    let served = Served::with_text();
    let store = served.store();

    let whole = rows(&served.fragment(&store, "o.text=Alice&limit=1000"));
    assert!(
        whole.len() > 1,
        "the fixture must need more than one page at limit=1"
    );

    for candidate_budget in [1u64, 2, 3, 1_000] {
        let budgets = Budgets {
            candidate_budget,
            ..Budgets::new()
        };
        let limits = Limits {
            caps: &CAPS,
            budgets: &budgets,
        };

        let mut collected = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        let stopped_short = loop {
            let query = match &cursor {
                Some(token) => format!("o.text=Alice&limit=1&cursor={token}"),
                None => "o.text=Alice&limit=1".to_owned(),
            };
            let answer = served
                .try_fragment_within(&store, &query, limits)
                .unwrap_or_else(|error| panic!("budget {candidate_budget}: {error}"));
            collected.extend(rows(&answer));
            pages += 1;
            assert!(
                pages < 50,
                "budget {candidate_budget} never terminated; saw {} of {} rows",
                collected.len(),
                whole.len()
            );
            if answer["complete"].as_bool().unwrap() {
                break false;
            }
            match answer["next"].as_str() {
                Some(token) => cursor = Some(token.to_owned()),
                // The depth bound: it says why it stopped and offers nothing to
                // continue with, so a client stops too.
                None => {
                    assert_eq!(answer["truncation_reason"], "candidate_budget");
                    break true;
                }
            }
        };

        // An interrupted score scan ranks only the candidates it examined; an
        // unexamined literal may enter ahead of them when a larger budget later
        // establishes the full ranking. The partial window must still contain
        // only real answer rows and never duplicate one while it pages.
        assert!(
            collected.iter().all(|row| whole.contains(row)),
            "budget {candidate_budget} returned a row outside the full answer"
        );
        let unique: std::collections::BTreeSet<_> = collected.iter().collect();
        assert_eq!(
            unique.len(),
            collected.len(),
            "budget {candidate_budget} duplicated a row"
        );
        assert_eq!(
            stopped_short,
            collected.len() < whole.len(),
            "budget {candidate_budget} must say it stopped exactly when it did"
        );
        // And a budget with room for the whole ranking reaches all of it.
        if candidate_budget as usize >= whole.len() {
            assert_eq!(collected, whole, "budget {candidate_budget}");
        }
    }
}

#[test]
fn a_count_that_spends_the_candidate_budget_resumes_to_the_exact_total() {
    let served = Served::with_text();
    let store = served.store();

    let whole = served.count(&store, "o.text=Alice");
    let total = whole["count"]["value"].as_u64().unwrap();
    assert!(total > 1, "the fixture must match more than one statement");
    assert_eq!(whole["complete"], serde_json::json!(true));
    assert_eq!(whole["count"]["exact"], serde_json::json!(true));

    let budgets = Budgets {
        candidate_budget: 1,
        ..Budgets::new()
    };
    let limits = Limits {
        caps: &CAPS,
        budgets: &budgets,
    };
    let initial = request::Count::parse(
        &params("o.text=Alice"),
        limits,
        served.release().prefixes(),
        &served.release().binding(),
    )
    .expect("a well-formed request");
    let invented = kgf_server::cursor::Cursor::at_text_scan(&initial.binding, u64::MAX, 0).encode();
    let query = format!("o.text=Alice&cursor={invented}");
    let request = request::Count::parse(
        &params(&query),
        limits,
        served.release().prefixes(),
        &served.release().binding(),
    )
    .expect("the token has the right request binding");
    let error = answer::count(&store, served.target("count", &query), &request)
        .expect_err("a scan position outside the index must be stale");
    assert_eq!(error.code(), kgf_server::envelope::ErrorCode::StaleCursor);

    let mut cursor = None;
    let mut lower_bound = 0;
    for _ in 0..10 {
        let query = cursor.as_ref().map_or_else(
            || "o.text=Alice".to_owned(),
            |token| format!("o.text=Alice&cursor={token}"),
        );
        let request = request::Count::parse(
            &params(&query),
            limits,
            served.release().prefixes(),
            &served.release().binding(),
        )
        .expect("a well-formed request");
        let answer =
            answer::count(&store, served.target("count", &query), &request).expect("an answer");
        let counted = json(answer, Representation::Json);
        let value = counted["count"]["value"].as_u64().unwrap();
        assert!(
            value >= lower_bound,
            "the accumulated lower bound cannot fall"
        );
        lower_bound = value;

        if counted["complete"].as_bool().unwrap() {
            assert_eq!(counted["count"]["exact"], serde_json::json!(true));
            assert_eq!(value, total);
            return;
        }
        assert_eq!(counted["count"]["exact"], serde_json::json!(false));
        assert_eq!(counted["count"]["min"], counted["count"]["value"]);
        assert_eq!(counted["truncation_reason"], "candidate_budget");
        cursor = Some(
            counted["next"]
                .as_str()
                .expect("an interrupted count is resumable")
                .to_owned(),
        );
    }
    panic!("the text count did not finish");
}

#[test]
fn a_ranked_row_says_which_class_its_score_belongs_to() {
    // hdtc ranks exact matches as a class ahead of stemmed ones and its scores
    // are comparable only within a class, so a stemmed row can carry a higher
    // number than the exact row above it. Without the class, a client sorting
    // the page by `score` — which doc 06 §6.2.1 tells federated clients to do —
    // undoes the ranking the server computed.
    let served = Served::with_text();
    let store = served.store();

    let answer = served.fragment(&store, "o.text=Alice&limit=1000");
    let rows = answer["rows"].as_array().unwrap();
    assert!(!rows.is_empty());

    let mut classes = std::collections::BTreeSet::new();
    for row in rows {
        let kind = row["match_kind"]
            .as_str()
            .expect("a ranked row names its class");
        assert!(
            matches!(kind, "exact" | "stemmed"),
            "unexpected class {kind}"
        );
        assert!(row["score"].is_number());
        classes.insert(kind.to_owned());
    }

    // Exact matches come first as a class, whatever the raw scores say.
    let ordered: Vec<&str> = rows
        .iter()
        .map(|row| row["match_kind"].as_str().unwrap())
        .collect();
    let first_stemmed = ordered.iter().position(|kind| *kind != "exact");
    if let Some(boundary) = first_stemmed {
        assert!(
            ordered[boundary..].iter().all(|kind| *kind != "exact"),
            "a class boundary is crossed once: {ordered:?}"
        );
    }

    // And a row with no text constraint carries neither field — there is no
    // ranking to report.
    let plain = served.fragment(&store, "limit=1");
    let row = &plain["rows"].as_array().unwrap()[0];
    assert!(row.get("score").is_none());
    assert!(row.get("match_kind").is_none());
}

#[test]
fn labels_preserve_input_order_and_search_returns_one_entity_with_evidence() {
    let served = Served::with_text();
    let store = served.store();

    let labels = served.labels(
        &store,
        &serde_json::json!({
            "iris": ["ex:bob", "ex:alice", "ex:missing", "ex:alice"]
        }),
    );
    assert_eq!(
        labels["labels"],
        serde_json::json!([
            {"iri": {"type": "iri", "value": "http://example.org/bob"}, "label": "Bob"},
            {"iri": {"type": "iri", "value": "http://example.org/alice"}, "label": "Alice"},
            {"iri": {"type": "iri", "value": "http://example.org/missing"}, "label": null},
            {"iri": {"type": "iri", "value": "http://example.org/alice"}, "label": "Alice"},
        ])
    );
    assert_eq!(labels["complete"], true);

    let explicit = served.search(
        &store,
        "q=Alice&predicate=%3Chttp%3A%2F%2Fexample.org%2Fname%3E&labels=false&limit=20",
    );
    assert_eq!(explicit["results"].as_array().unwrap().len(), 1);
    let alice = &explicit["results"][0];
    assert_eq!(alice["subject"]["value"], "http://example.org/alice");
    assert!(alice.get("label").is_none());
    assert_eq!(alice["match"]["predicate"], "http://example.org/name");
    assert_eq!(alice["match"]["literal"], "Alice");

    // A role is query-time sugar for its profile predicates. Label hydration
    // uses that same release profile but remains independently switchable.
    let role = served.search(&store, "q=Bob&role=label&limit=20");
    assert_eq!(role["roles"], serde_json::json!(["label"]));
    assert_eq!(
        role["results"][0]["subject"]["value"],
        "http://example.org/bob"
    );
    assert_eq!(role["results"][0]["label"], "Bob");
    assert_eq!(
        role["results"][0]["match"]["predicate"],
        "http://example.org/name"
    );
}

// ---------------------------------------------------------------------------
// A served bundle, and the operations over it
// ---------------------------------------------------------------------------

const DATASET: &str = "tox";
const VERSION: &str = "2026-06-01";
const CAPS: Caps = Caps::new();
const BUDGETS: Budgets = Budgets::new();

struct Served {
    // Held: dropping it removes the bundle the mappings are over.
    _root: tempfile::TempDir,
    service: Arc<Service>,
}

impl Served {
    fn new() -> Self {
        Self::build(false)
    }

    /// The same bundle with a full-text index over its literals, so `o.text`
    /// has something to rank.
    fn with_text() -> Self {
        Self::build(true)
    }

    fn build(text: bool) -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let bundle = root.path().join(DATASET).join(VERSION);
        let fixture = Fixture::build(GRAPH);
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
            "--prefix",
            "ex=http://example.org/",
            "--role",
            "label=http://example.org/label",
            "--role",
            "label=http://example.org/name",
        ]);
        kgf::manifest::run(cli.args).expect("describe the bundle");

        let config = kgf_server::Config::new(
            kgf::serve::published_root(root.path()).expect("a published root"),
            "127.0.0.1:0".parse().unwrap(),
        );
        Self {
            service: Arc::new(Service::build(config).expect("a servable deployment")),
            _root: root,
        }
    }

    fn id(&self) -> BundleId {
        BundleId {
            dataset: DATASET.to_owned(),
            version: VERSION.to_owned(),
        }
    }

    fn release(&self) -> &Release {
        self.service
            .datasets()
            .release(DATASET, VERSION)
            .expect("the fixture version")
    }

    fn store(&self) -> Arc<Store> {
        self.service.open(&self.id()).expect("the bundle opens")
    }

    fn limits(&self) -> Limits<'static> {
        Limits {
            caps: &CAPS,
            budgets: &BUDGETS,
        }
    }

    /// The same, under budgets a test chose — for the one composite budget that
    /// no cap bounds and that therefore has to bite at run time.
    fn within<'a>(&self, budgets: &'a Budgets) -> Limits<'a> {
        Limits {
            caps: &CAPS,
            budgets,
        }
    }

    fn fragment_within(&self, store: &Store, query: &str, budgets: &Budgets) -> serde_json::Value {
        let request = request::Fragment::parse(
            &params(query),
            self.within(budgets),
            self.release().prefixes(),
            &self.release().binding(),
        )
        .expect("a well-formed request");
        json(
            answer::fragment(store, self.target("fragment", query), &request).expect("an answer"),
            Representation::Json,
        )
    }

    fn sample_within(&self, store: &Store, query: &str, budgets: &Budgets) -> serde_json::Value {
        let request = request::Sample::parse(
            &params(query),
            self.within(budgets),
            self.release().prefixes(),
        )
        .expect("a well-formed request");
        json(
            answer::sample(store, self.target("sample", query), &request).expect("an answer"),
            Representation::Json,
        )
    }

    fn target(&self, operation: &'static str, query: &str) -> Target {
        Target::new(
            self.id(),
            operation,
            params(query),
            self.release().prefixes().clone(),
        )
    }

    fn parse_fragment(&self, query: &str) -> request::Fragment {
        request::Fragment::parse(
            &params(query),
            self.limits(),
            self.release().prefixes(),
            &self.release().binding(),
        )
        .expect("a well-formed request")
    }

    fn try_fragment(
        &self,
        store: &Store,
        query: &str,
    ) -> Result<serde_json::Value, kgf_server::envelope::Problem> {
        self.try_fragment_within(store, query, self.limits())
    }

    /// A `/fragment` read against budgets the caller chose.
    ///
    /// The published defaults are far larger than any fixture, so a budget only
    /// ever fires in a test that lowers it — which is exactly how the
    /// candidate budget went unexercised while `top_k` capped the absolute
    /// rank and paging stalled at the ceiling.
    fn try_fragment_within(
        &self,
        store: &Store,
        query: &str,
        limits: Limits<'_>,
    ) -> Result<serde_json::Value, kgf_server::envelope::Problem> {
        let request = request::Fragment::parse(
            &params(query),
            limits,
            self.release().prefixes(),
            &self.release().binding(),
        )?;
        let answer = answer::fragment(store, self.target("fragment", query), &request)?;
        Ok(json(answer, Representation::Json))
    }

    fn fragment(&self, store: &Store, query: &str) -> serde_json::Value {
        self.try_fragment(store, query)
            .unwrap_or_else(|error| panic!("GET /fragment?{query}: {error}"))
    }

    fn count(&self, store: &Store, query: &str) -> serde_json::Value {
        let request = request::Count::parse(
            &params(query),
            self.limits(),
            self.release().prefixes(),
            &self.release().binding(),
        )
        .unwrap_or_else(|error| panic!("GET /count?{query}: {error}"));
        let answer = answer::count(store, self.target("count", query), &request)
            .unwrap_or_else(|error| panic!("GET /count?{query}: {error}"));
        json(answer, Representation::Json)
    }

    fn binding_fragment(&self, store: &Store, body: &serde_json::Value) -> serde_json::Value {
        let rendered = self.render_binding_fragment(store, body, Representation::Json);
        serde_json::from_slice(&rendered.body).expect("a bindings fragment serializes as JSON")
    }

    fn binding_fragment_html(&self, store: &Store, body: &serde_json::Value) -> String {
        let rendered = self.render_binding_fragment(store, body, Representation::Html);
        String::from_utf8(rendered.body.to_vec()).expect("a bindings fragment page is UTF-8")
    }

    fn render_binding_fragment(
        &self,
        store: &Store,
        body: &serde_json::Value,
        representation: Representation,
    ) -> kgf_server::answer::Rendered {
        use kgf_server::answer::Renders;

        let encoded = serde_json::to_vec(body).expect("a JSON body");
        let request = request::BindingFragment::parse(
            &params(""),
            &encoded,
            self.limits(),
            self.release().prefixes(),
            &self.release().binding(),
        )
        .expect("a bindings fragment request");
        answer::binding_fragment(
            store,
            Target::body(
                self.id(),
                "fragment",
                params(""),
                self.release().prefixes().clone(),
            ),
            &request,
        )
        .expect("a bindings fragment answer")
        .render(representation)
    }

    fn binding_count(&self, store: &Store, body: &serde_json::Value) -> serde_json::Value {
        let mut body = body.clone();
        body.as_object_mut().unwrap().remove("limit");
        body.as_object_mut().unwrap().remove("cursor");
        let encoded = serde_json::to_vec(&body).expect("a JSON body");
        let request = request::BindingCount::parse(
            &params(""),
            &encoded,
            self.limits(),
            self.release().prefixes(),
        )
        .expect("a bindings count request");
        json(
            answer::binding_count(
                store,
                Target::body(
                    self.id(),
                    "count",
                    params(""),
                    self.release().prefixes().clone(),
                ),
                &request,
            )
            .expect("a bindings count answer"),
            Representation::Json,
        )
    }

    fn describe(&self, store: &Store, query: &str) -> serde_json::Value {
        let request = request::Describe::parse(
            &params(query),
            self.limits(),
            self.release().prefixes(),
            &self.release().binding(),
        )
        .unwrap_or_else(|error| panic!("GET /describe?{query}: {error}"));
        let answer = answer::describe(store, self.target("describe", query), &request)
            .unwrap_or_else(|error| panic!("GET /describe?{query}: {error}"));
        json(answer, Representation::Json)
    }

    fn sample(&self, store: &Store, query: &str) -> serde_json::Value {
        let request =
            request::Sample::parse(&params(query), self.limits(), self.release().prefixes())
                .unwrap_or_else(|error| panic!("GET /sample?{query}: {error}"));
        let answer = answer::sample(store, self.target("sample", query), &request)
            .unwrap_or_else(|error| panic!("GET /sample?{query}: {error}"));
        json(answer, Representation::Json)
    }

    fn search(&self, store: &Store, query: &str) -> serde_json::Value {
        let request = request::Search::parse(
            &params(query),
            self.limits(),
            self.release().prefixes(),
            self.release().predicate_roles(),
        )
        .unwrap_or_else(|error| panic!("GET /search?{query}: {error}"));
        let answer = answer::search(store, self.target("search", query), &request)
            .unwrap_or_else(|error| panic!("GET /search?{query}: {error}"));
        json(answer, Representation::Json)
    }

    fn labels(&self, store: &Store, body: &serde_json::Value) -> serde_json::Value {
        let encoded = serde_json::to_vec(body).expect("a JSON body");
        let request = request::Labels::parse(
            &params(""),
            &encoded,
            self.limits(),
            self.release().prefixes(),
            self.release().predicate_roles(),
        )
        .expect("a labels request");
        json(
            answer::labels(
                store,
                Target::body(
                    self.id(),
                    "labels",
                    params(""),
                    self.release().prefixes().clone(),
                ),
                &request,
            )
            .expect("a labels answer"),
            Representation::Json,
        )
    }

    fn render(
        &self,
        store: &Store,
        operation: &'static str,
        query: &str,
        representation: Representation,
    ) -> String {
        use kgf_server::answer::Renders;

        let target = Target::new(
            self.id(),
            operation,
            params(query),
            self.release().prefixes().clone(),
        );
        let rendered = match operation {
            "fragment" => {
                let request = request::Fragment::parse(
                    &params(query),
                    self.limits(),
                    self.release().prefixes(),
                    &self.release().binding(),
                )
                .expect("a well-formed request");
                answer::fragment(store, target, &request)
                    .expect("an answer")
                    .render(representation)
            }
            "describe" => {
                let request = request::Describe::parse(
                    &params(query),
                    self.limits(),
                    self.release().prefixes(),
                    &self.release().binding(),
                )
                .expect("a well-formed request");
                answer::describe(store, target, &request)
                    .expect("an answer")
                    .render(representation)
            }
            "sample" => {
                let request = request::Sample::parse(
                    &params(query),
                    self.limits(),
                    self.release().prefixes(),
                )
                .expect("a well-formed request");
                answer::sample(store, target, &request)
                    .expect("an answer")
                    .render(representation)
            }
            "count" => {
                let request = request::Count::parse(
                    &params(query),
                    self.limits(),
                    self.release().prefixes(),
                    &self.release().binding(),
                )
                .expect("a well-formed request");
                answer::count(store, target, &request)
                    .expect("an answer")
                    .render(representation)
            }
            other => panic!("no such operation: {other}"),
        };
        String::from_utf8(rendered.body.to_vec()).expect("a UTF-8 body")
    }

    /// Every term of the bundle, per role, in §3.3 request syntax.
    fn terms(&self, store: &Store) -> Terms {
        let dictionary = store.dict();
        let mut scratch = Vec::new();
        let mut roles = Vec::new();
        for role in [Role::Subject, Role::Predicate, Role::Object] {
            let mut terms = Vec::new();
            for id in 1..=dictionary.counts().len(role) {
                let stored = dictionary
                    .extract(role, TermId(id), &mut scratch)
                    .expect("a term the dictionary counted");
                let text = std::str::from_utf8(stored).expect("a UTF-8 term");
                terms.push((
                    id,
                    kgf_server::term::Term::from_dictionary(text).to_request(),
                ));
            }
            roles.push(terms);
        }
        Terms(roles)
    }

    /// The rows the store gives for this pattern, in its own enumeration order.
    fn expect(
        &self,
        store: &Store,
        terms: &Terms,
        subject: Option<usize>,
        predicate: Option<usize>,
        object: Option<usize>,
    ) -> Vec<Vec<String>> {
        let ids = IdPattern {
            subject: subject.map(|index| terms.id(Role::Subject, index)),
            predicate: predicate.map(|index| terms.id(Role::Predicate, index)),
            object: object.map(|index| terms.id(Role::Object, index)),
        };
        let selection = store.resolve(ids).expect("ids from this dictionary");
        let dictionary = store.dict();
        let mut scratch = Vec::new();
        let mut extract = |role: Role, id: u64| -> String {
            let bytes = dictionary
                .extract(role, TermId(id), &mut scratch)
                .expect("a term");
            std::str::from_utf8(bytes).expect("UTF-8").to_owned()
        };
        selection
            .page(0, usize::MAX)
            .map(|triple: IdTriple| {
                let mut row = Vec::new();
                if subject.is_none() {
                    row.push(extract(Role::Subject, triple.subject));
                }
                if predicate.is_none() {
                    row.push(extract(Role::Predicate, triple.predicate));
                }
                if object.is_none() {
                    row.push(extract(Role::Object, triple.object));
                }
                row
            })
            .collect()
    }

    /// Page a fragment request to exhaustion through its own cursors.
    fn page_through(&self, store: &Store, query: &str, limit: usize) -> Vec<Vec<String>> {
        let mut collected = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..1_000 {
            let paged = match &cursor {
                Some(token) => format!("{query}&limit={limit}&cursor={token}"),
                None => format!("{query}&limit={limit}"),
            };
            let answer = self.fragment(store, &paged);
            let page = rows(&answer);
            assert!(page.len() <= limit, "a page must not exceed its limit");
            collected.extend(page);
            if answer["complete"].as_bool().expect("complete is a bool") {
                assert_eq!(answer["next"], serde_json::json!(null));
                return collected;
            }
            cursor = Some(
                answer["next"]
                    .as_str()
                    .expect("an incomplete page carries a cursor")
                    .to_owned(),
            );
        }
        panic!("paging ?{query} at limit={limit} did not terminate");
    }

    fn page_through_describe(
        &self,
        store: &Store,
        query: &str,
        limit: usize,
    ) -> Vec<(String, String, String, String)> {
        let mut collected = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..1_000 {
            let paged = match &cursor {
                Some(token) => format!("{query}&limit={limit}&cursor={token}"),
                None => format!("{query}&limit={limit}"),
            };
            let answer = self.describe(store, &paged);
            collected.extend(describe_rows(&answer));
            if answer["complete"].as_bool().expect("complete is a bool") {
                return collected;
            }
            cursor = Some(answer["next"].as_str().expect("a cursor").to_owned());
        }
        panic!("paging describe?{query} at limit={limit} did not terminate");
    }
}

/// The bundle's terms, per role: the id, and how a request writes it.
struct Terms(Vec<Vec<(u64, String)>>);

impl Terms {
    fn of(&self, role: Role) -> &[(u64, String)] {
        &self.0[match role {
            Role::Subject => 0,
            Role::Predicate => 1,
            Role::Object => 2,
        }]
    }

    fn id(&self, role: Role, index: usize) -> u64 {
        self.of(role)[index].0
    }

    fn request(&self, role: Role, index: usize) -> &str {
        &self.of(role)[index].1
    }
}

/// Every term of a role, plus the variable.
fn options(terms: &Terms, role: Role) -> Vec<Option<usize>> {
    std::iter::once(None)
        .chain((0..terms.of(role).len()).map(Some))
        .collect()
}

/// The query string a client would write for this pattern.
///
/// Each bound position goes out in §3.3 request syntax and percent-encoded, so
/// the round trip through the term parser and the dictionary is inside what is
/// being compared rather than beside it.
fn pattern_query(
    terms: &Terms,
    subject: Option<usize>,
    predicate: Option<usize>,
    object: Option<usize>,
) -> String {
    [
        ("s", Role::Subject, subject),
        ("p", Role::Predicate, predicate),
        ("o", Role::Object, object),
    ]
    .into_iter()
    .filter_map(|(name, role, index)| {
        index.map(|index| {
            format!(
                "{name}={}",
                kgf_server::url::encode_value(terms.request(role, index))
            )
        })
    })
    .collect::<Vec<_>>()
    .join("&")
}

fn params(query: &str) -> Params {
    Params::parse(Some(query)).unwrap_or_else(|error| panic!("query {query:?}: {error}"))
}

fn json(
    answer: impl kgf_server::answer::Renders,
    representation: Representation,
) -> serde_json::Value {
    let rendered = answer.render(representation);
    serde_json::from_slice(&rendered.body).expect("an answer serializes as JSON")
}

/// A response's rows, as the term strings the store would have produced.
fn rows(answer: &serde_json::Value) -> Vec<Vec<String>> {
    answer["rows"]
        .as_array()
        .expect("rows is an array")
        .iter()
        .map(|row| {
            answer["vars"]
                .as_array()
                .expect("vars is an array")
                .iter()
                .map(|var| dictionary_spelling(&row[var.as_str().unwrap()]))
                .collect()
        })
        .collect()
}

fn describe_rows(answer: &serde_json::Value) -> Vec<(String, String, String, String)> {
    answer["rows"]
        .as_array()
        .expect("rows is an array")
        .iter()
        .map(|row| {
            (
                row["direction"].as_str().expect("a direction").to_owned(),
                dictionary_spelling(&row["s"]),
                dictionary_spelling(&row["p"]),
                dictionary_spelling(&row["o"]),
            )
        })
        .collect()
}

/// A term object, written the way the dictionary holds it.
///
/// The oracle side of the comparison, so this is a second reading of doc 03
/// §3.4.1 and hdtc's dictionary encoding rather than a call into the code under
/// test.
fn dictionary_spelling(term: &serde_json::Value) -> String {
    let value = term["value"].as_str().expect("a term value");
    match term["type"].as_str().expect("a term type") {
        "iri" => value.to_owned(),
        "bnode" => format!("_:{value}"),
        "literal" => match (term.get("lang"), term.get("datatype")) {
            (Some(lang), _) => format!("\"{value}\"@{}", lang.as_str().unwrap()),
            (None, Some(datatype)) => {
                format!("\"{value}\"^^<{}>", datatype.as_str().unwrap())
            }
            (None, None) => format!("\"{value}\""),
        },
        other => panic!("unknown term type {other}"),
    }
}
