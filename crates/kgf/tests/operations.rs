//! Doc 03 §3.4's four operations, against the store's own answers.
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
    assert!(page.contains("<h1>fragment — tox 2026-06-01</h1>"));
    assert!(page.contains("http://example.org/alice"));
    // A subject links to its neighborhood and a predicate to its own fragment,
    // which is what makes the page a way into the data.
    assert!(page.contains("/tox/v/2026-06-01/describe?iri=%3Chttp%3A%2F%2Fexample.org%2Falice%3E"));
    assert!(page.contains("/tox/v/2026-06-01/fragment?p=%3Chttp%3A%2F%2Fexample.org%2Fknows%3E"));
    // A truncated page offers the next one.
    assert!(page.contains("Next page"));
    assert!(page.contains("cursor="));

    // And the JSON link carries `format` exactly once, whatever the request
    // asked for — the page's own footer builds it by appending.
    let pinned = served.render(
        &store,
        "fragment",
        "limit=2&format=html",
        Representation::Html,
    );
    assert_eq!(pinned.matches("format=json").count(), 1);
    assert!(!pinned.contains("format=html&amp;format=json"));

    for (operation, query) in [
        ("count", "p=ex:knows"),
        ("describe", "iri=ex:alice"),
        ("sample", "n=2"),
    ] {
        let page = served.render(&store, operation, query, Representation::Html);
        assert!(
            page.to_ascii_lowercase().starts_with("<!doctype html>"),
            "{operation} must answer a whole document"
        );
        assert!(page.contains(&format!("<h1>{operation} — tox 2026-06-01</h1>")));
    }
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
        let root = tempfile::tempdir().expect("temp dir");
        let bundle = root.path().join(DATASET).join(VERSION);
        Fixture::build(GRAPH).copy_bundle_to(&bundle);

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

    fn target(&self, operation: &'static str, query: &str) -> Target {
        Target::new(self.id(), operation, params(query))
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
        let request = request::Fragment::parse(
            &params(query),
            self.limits(),
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
        let request =
            request::Count::parse(&params(query), self.limits(), self.release().prefixes())
                .unwrap_or_else(|error| panic!("GET /count?{query}: {error}"));
        let answer = answer::count(store, self.target("count", query), &request)
            .unwrap_or_else(|error| panic!("GET /count?{query}: {error}"));
        json(answer, Representation::Json)
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

    fn render(
        &self,
        store: &Store,
        operation: &'static str,
        query: &str,
        representation: Representation,
    ) -> String {
        use kgf_server::answer::Renders;

        let target = Target::new(self.id(), operation, params(query));
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
                let request =
                    request::Count::parse(&params(query), self.limits(), self.release().prefixes())
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
