# Request logging — the access record `kgf serve` owes the census

Status: implemented as unit 22. Written 2026-09-03 against kgf-rs `2e2e26c`, hdtc
`v1.2.0-beta.3`, `../kgf` docs at `970d442`. Scopes `../kgf/docs/gcp-deployment-plan.md`
§3.5; doc 12 §12.1–§12.2 is the authority on what to collect and on the hygiene
rules, and this note does not relax either. The implementation outcome is recorded as
unit 22 in `notes/plan.md`; its cross-repository follow-ups are recorded there under
**Questions for `../kgf`**.

Estimated size: about two days. Nothing here touches `kgf-store` except one optional
read-only probe (§8).

## 1. Why this is next, and what exists

Doc 12's census is what gates the roadmap, and the GCP trial is the first place real
traffic will exist. The deployment plan's §3.5 says it plainly: the logging has to
exist *before* the first bundle is served, or the first months of traffic are
unmeasured. The benchmark already paid for its absence once — it had to put a
counting reverse proxy in front of the server to learn how many requests a client
made (`../kgf/docs/benchmark-kgf-results.md` §11), and that proxy truncated paths at
220 characters and invalidated one measurement. The server should count itself.

What the server emits today, all through `tracing`:

| where | what | level |
|---|---|---|
| `crates/kgf/src/main.rs:54` `install_logging` | `fmt` subscriber, human text, **stderr**, `RUST_LOG` default `info` | — |
| `crates/kgf-server/src/lib.rs` `serve` | one `serving` line at startup with the admission numbers; one at shutdown | info |
| `crates/kgf-server/src/service.rs:188` `Service::open` | `bundle failed to open`, with the classified store error | error |
| `answer.rs`, `routes.rs`, `admission.rs` | invariant violations — a header that cannot be sent, a panic on the blocking pool, a closed semaphore | error |
| `crates/kgf-server/src/routes.rs:106` | `TraceLayer::new_for_http()` | debug |

So at the default level there is **no per-request record at all**, and the one layer
that would produce one is a hazard rather than a help: tower-http's default span
records the full request URI, query string included, at `debug`. Turning it on in
production would log every `q=` and every bound term — the exact opposite of doc 12
§12.1's shape-by-default rule. §7 says what to do with it.

Also absent: any metrics endpoint. Doc 05 §5.7 mentions per-server Prometheus counters
and that is a reasonable later layer, but it is *derived* from what this note
specifies rather than a substitute for it. The census wants per-request shapes joined
offline in DuckDB (doc 12 §12.1 "load into DuckDB for analysis"); counters cannot give
that, and the record can give the counters.

## 2. The contract: one record per response

Every HTTP response `kgf serve` sends produces exactly one **access record**, as one
JSON object on one line. Two tiers, following doc 12 §12.1 exactly:

- **Shape tier (default).** Operation, structure, magnitudes, timings, outcome,
  pseudonymous client. Nothing the client typed.
- **Raw tier (`--log-raw`, off by default).** Adds the raw request target and the
  search string. Doc 12 asks that this be opt-in per deployment, access-controlled,
  and retained briefly (~30 days); retention is the deployment's job (a Cloud Logging
  bucket policy), the flag is ours.

**Never logged in either tier:** request bodies, response bodies, `Problem`'s
`detail` (it reflects up to 200 characters of whatever the client sent —
`envelope.rs:595` `reflected`), and any `Authorization` header, of which there are
none yet.

### 2.1 Fields

Field order is stable — `serde_json`'s `preserve_order` is on workspace-wide — so
the line is also readable by eye.

| field | type | source | tier |
|---|---|---|---|
| `time` | RFC 3339, UTC | `jiff::Timestamp::now()` (already a dependency) | shape |
| `severity` | `"INFO"` | constant; see §6 for why it is there | shape |
| `request_id` | string | minted by the layer, §4 | shape |
| `method` | `GET` / `POST` / `QUERY` / `HEAD` / `OPTIONS` / other | request | shape |
| `route` | the matched route template, e.g. `/{dataset}/v/{version}/fragment`, or `null` for a 404 | axum `MatchedPath`, §3 | shape |
| `operation` | `fragment` `count` `describe` `sample` `search` `schema` `void` `summary` `labels` `manifest` `service` `dataset` `latest`, or `null` | derived from `route` | shape |
| `transport` | `get` / `get-values` (brTPF `values=`) / `query` / `post` | handler | shape |
| `dataset`, `version` | strings, **only once the service has resolved the release** — never a client string the catalog does not know | handler, after `Datasets::release` (`service.rs:447`) | shape |
| `representation` | `Representation::token()` | handler / problem renderer | shape |
| `status` | u16 | response | shape |
| `code` | `ErrorCode::as_str()` or `null` | §3.2 | shape |
| `work_class` | `ordinary` / `heavy` / `null` (no store work) | `GetRequest::work_class` | shape |
| `queue_ms`, `work_ms`, `total_ms` | integers | §3.3 | shape |
| `queued` | admission waiting-room slots available at entry | §9 | shape |
| `bytes_in`, `bytes_out` | integers or `null` | body size hints | shape |
| `complete`, `truncation_reason` | bool, token or `null` | `Rendered.completeness` | shape |
| `rows` | integer or `null` | `Rendered.rows` (new, §3.4) | shape |
| `cardinality`, `exact` | integer or `null`, bool | the answer's `Cardinality` | shape |
| `cursor` | bool — the request resumed one | typed request | shape |
| `request_hash` | 16 hex chars or `null` | `CanonicalRequest::hash` where a binding exists, §5 | shape |
| `open_ms`, `first_open` | integer or `null`, bool | §8 | shape |
| `client_hash`, `forwarded_hash` | 16 hex chars | §4 | shape |
| `client_class` | `comunica` / `browser` / `curl` / `python` / `node` / `kgf` / `unknown` | §4 | shape |
| `user_agent` | truncated to 200 bytes | header | shape |
| `client_request_id` | truncated inbound `X-Request-Id`, if any | header | shape |
| `shape` | object, per operation — §2.2 | typed request | shape |
| `target` | raw path and query | request | **raw** |
| `q` | the search string | typed request | **raw** |

### 2.2 The `shape` object, per operation

This is doc 12's "parsed shape" for KGF. It is computed from the *typed* request
after parsing, never from the raw parameters, so a malformed request has no shape
and a well-formed one has exactly the shape the server acted on.

| operation | fields |
|---|---|
| `fragment`, `count` (GET) | `pattern`: three characters, one per position, `?` unbound or the bound term's kind — `i` IRI, `l` literal, `b` blank node (e.g. `"?il"`); `text`: bool (`o.text` present); `limit` |
| `fragment`, `count` (bindings) | as above for the body pattern, plus `k` rows submitted and `columns` |
| `describe` | `term`: `i`/`b`; `direction`; `limit` |
| `sample` | `pattern`; `n` |
| `search` | `q_len` (bytes); `roles` (names — these are declared vocabulary, not client text); `predicates` (count); `limit`; `labels`: bool |
| `labels` | `k` IRIs submitted |
| `schema` | `selection`: `root` / `class` / `predicate` / `datatype` / …; `children`; `projection`; `view`; `limit` |
| `void`, `summary`, `manifest`, `service`, `dataset`, `latest` | none |

Term *kinds* are shape; term *values* are content. Role names are shape because the
vocabulary is the server's (`label`, `synonym`, `description`), and the request is
refused if it names another.

## 3. Where the record is assembled

Three places know different parts, and the design is to let each write only what
it knows.

### 3.1 The outermost layer owns the clock, the id, and the emit

One `middleware::from_fn_with_state` added as the **last** `.layer` in
`routes::router` — axum layers wrap outward, so last is outermost, outside CORS, and
the layer sees every response including CORS preflights and the body-limit 413.

On the way in: start the clock, mint `request_id` (§4), read method, `User-Agent`,
peer address, inbound `X-Request-Id`, `Content-Length`. On the way out: read
`status`, `code` (§3.2), `bytes_out` from `body.size_hint().exact()` — every body
this server builds is `Body::from(Bytes)` or `Body::empty()`, so the hint is exact
and nothing is buffered; a streamed body, should one ever exist, logs `null`. Then
take the handler's `Observation` out of the response extensions, merge, and hand the
record to the sink (§6). Set `KGF-Request-Id` on the response before returning it.

`route` and `operation` come from axum's `MatchedPath` extension, which needs the
`matched-path` feature on the workspace's `axum` dependency (currently `http1`,
`tokio`, `original-uri`). This is what lets a 405 or a refused extractor still carry
an operation without any handler having run. A request that matched no route logs
`route: null`.

### 3.2 `render_problems` leaves the code behind

`routes.rs:1521` currently `remove`s the `Problem` from the response extensions and
renders it. The outer layer runs after that and would see nothing. The fix is one
line: after rendering, insert `problem.code()` (an `ErrorCode`, `Copy`) back into the
extensions. The access layer reads that. For the errors `render_problems` itself
attributes from a bare status — the body-limit 413 — it already computes an
`ErrorCode` via `for_unattributed_status` (`envelope.rs:576`), so the same insert
covers them.

### 3.3 The handlers add what only they know

The four `operate_*` functions (`operate`, `operate_represented`, `operate_special`,
`operate_body`) and the three descriptor handlers each build an `Observation` and
insert it into the response extensions. The `Observation` is a plain struct:
`dataset`, `version`, `operation`, `transport`, `representation`, `work_class`,
`shape`, `cursor`, `request_hash`, and — filled in after the blocking call —
timings, `rows`, `cardinality`, completeness, `open_ms`.

Two plumbing changes make that possible:

- **`GetRequest::shape(&self) -> Shape`** (`request.rs:2499`), alongside
  `work_class` and `labels_requested`, so every typed GET request describes itself;
  the body requests (`BindingFragment`, `BindingCount`, `Labels`) get an inherent
  `shape()`. `Pattern::bound` and `BoundTerm` already expose what §2.2 needs, except
  the term *kind*, which `BoundTerm` does not currently store — add it at parse
  (`term::Term` knows), do not re-parse the string to find out.
- **`blocking` returns timings** (`routes.rs:1608`). Measure `admission.enter` as
  `queue_ms` and time the closure body as `work_ms`; `total_ms` is the layer's.
  Pool scheduling delay is whatever is left, and it is worth seeing on the
  shared-storage gate. The three callers destructure a pair; that is the whole
  ripple.

The 304 path (`not_modified`, `routes.rs:1372`) and the redirect (`latest_redirect`)
insert an `Observation` too — without timings or rows, but with operation, dataset
and version. A 304 is the cache-hit signal doc 12's cacheability row wants, and it
must not be the one response that goes unrecorded.

### 3.4 `Rendered` gains `rows` and `cardinality`

`answer.rs:282` `Rendered` carries `body` and `completeness` because the body is
produced inside the blocking task and the headers are set outside it. Row count and
cardinality are in exactly the same position: known inside, wanted outside. Add
`rows: Option<u64>` and `cardinality: Option<Cardinality>`, and let every `Renders`
impl fill them — `Answer` its `rows.len()` and `cardinality`, `CountAnswer` its
`count`, `SearchAnswer` `results.len()`, `LabelsAnswer` `labels.len()`,
`BindingCountAnswer` and the `SchemaAnswer` variants their item counts. The compiler
routes the change through every answer, which is the reason `Renders` is a trait.

## 4. Client identity and the request id

**Peer address.** `serve_on` (`lib.rs:404`) calls `axum::serve(listener, router)`;
the peer is only available if that becomes
`router.into_make_service_with_connect_info::<SocketAddr>()`, after which the layer
reads `ConnectInfo<SocketAddr>` from the request extensions. Behind the FRINK
gateway the peer is the gateway, and the client is the first hop of
`X-Forwarded-For`. The `public_origin` design deliberately does not trust forwarding
headers for *IRIs*; for a log the only risk is spoofed attribution, so record both:
`client_hash` over the peer, `forwarded_hash` over the first forwarded hop when the
header is present. The analyst picks the meaningful one per deployment.

**Hashing.** `hex(SHA-256(salt ‖ address))[..16]`, with `salt` random per process.
`std::hash::RandomState::new()` is seeded randomly per process and hashing a
constant through it yields 64 bits with no new dependency. Identities are therefore
stable within a process lifetime and unlinkable across restarts. That is doc 12's
"hash and rotate" at the coarsest useful granularity: the census rows that need
identity (sessions, co-access) span hours, not deploys. A daily-rotating salt is a
later refinement if that ever proves false; do not build it first.

**`client_class`** from `User-Agent`: `comunica` (contains `Comunica`), `browser`
(starts `Mozilla/`), `curl`, `python` (`python-requests`, `httpx`, `aiohttp`,
`urllib`), `node` (`node`, `undici`), `kgf` (the client library and `kgfq`, once
they exist — **they should send `User-Agent: kgf-client/<version>`**, and this note
is where that requirement is recorded until doc 06 says it), else `unknown`. Keep
the truncated raw UA beside the class; the classifier will need refining once real
clients appear, and doc 12 §12.5.2 shows how much a UA fingerprint can identify.

**`request_id`.** `{process_nonce:016x}-{counter:08x}`: the same per-process nonce
and an `AtomicU64`. Unique across restarts by the nonce, ordered within one by the
counter, no `uuid` dependency. Echoed on **every** response — 200, 304, 4xx, 5xx,
redirects — as `KGF-Request-Id`, named to sit beside the `KGF-Complete` family.
This header is the join key the census has been missing: doc 12 §12.5.4 measured
MCP transcripts under-reporting endpoint traffic by 1.8× and concluded transcripts
must be joined to endpoint logs; doc 06 §6.2.1's receipts already carry "canonical
request + request hash". A receipt that also carries the server's `request_id`s
makes that join exact rather than heuristic. An inbound `X-Request-Id` from a client
is logged as `client_request_id`, truncated, and never adopted as ours.

## 5. Hygiene: the two judgment calls

Everything above follows doc 12 §12.1 directly. Two things it does not settle, and
this note's recommendation on each — record whichever way they land in doc 12 when
the unit ships:

1. **`request_hash` in the shape tier.** `CanonicalRequest::hash` (`cursor.rs:273`)
   is an 8-byte SHA-256 truncation over the operation and the result-determining
   parameters — bound terms included. It is one-way and reversible only by guessing a
   candidate request, and it is exactly what doc 12 §12.2's *repetition rate* row
   needs ("normalized-identical queries, within/across sessions") without storing
   content. **Recommendation: shape tier.** Reuse the binding's hash where the
   request built one — fragment, count, describe, schema, the operations that page
   and therefore the ones repetition is about — and log `null` for the rest in v1.
   Do **not** extend `cursor::Operation` (`cursor.rs:87`) to cover search or labels
   for logging's sake: its discriminants are wire values in every cursor token, and
   adding variants there is a cursor-format decision, not a logging one. If search
   repetition matters later, hash `q` separately and decide then whether a hash of
   a search string is shape or content.

2. **Client-chosen identifiers.** `dataset` and `version` are public identifiers
   *when the catalog knows them*, and arbitrary client text when it does not. The
   rule: the shape tier logs `dataset`/`version` only after `Datasets::release`
   succeeded; a 404 logs `route` and `operation` and nothing the client typed. The
   raw path lives in the raw tier's `target`. The same rule is why `roles` are shape
   and `q` is not.

## 6. Output: a typed record, a sink, and stdout

**Mechanism.** A `#[derive(Serialize)] struct AccessRecord` and a sink trait —
`fn record(&self, record: &AccessRecord)` — held as `Option<Arc<dyn AccessLog +
Send + Sync>>` on `kgf_server::Config` (`lib.rs:73`). The default sink writes one
`serde_json` line to `std::io::stdout().lock()`; `None` disables. Reasons for a
struct over `tracing::info!` field macros: the record's field set is a contract the
DuckDB analysis reads against, so it should be one type the compiler checks rather
than a set of macro call sites that can drift; tests inject an in-memory sink and
assert on typed records (§10); no new dependency. The `tracing` alternative — an
event on a dedicated target plus a JSON `fmt` layer with `flatten_event(true)`
filtered to it — works, but needs the `json` feature, nests under tracing's own
`level`/`target`/`fields` conventions, and is awkward to assert on. Pick the struct.

**Streams.** Access records to **stdout**, diagnostics (everything `tracing` does
today) to **stderr**, as now. One Cloud Logging detail decides two things: GKE's
logging agent assigns severity *from the stream* when a JSON line carries no
`severity` field — stdout lines become INFO, stderr lines become ERROR. So the
record carries `"severity":"INFO"` (free, harmless elsewhere), and the existing
startup `info!` lines on stderr will show as ERROR in Cloud Logging until
`install_logging` (`main.rs:54`) emits JSON with a `severity` field.
`tracing-subscriber`'s JSON formatter writes `level`, not `severity`, so that needs a
small custom `FormatEvent` or a stream change; it is cosmetic and **not** part of
this unit. Verify both behaviours against the cluster's actual agent before relying
on them — this is from documentation, not from a test on FRINK.

**Flags on `kgf serve`** (`crates/kgf/src/serve.rs`): `--access-log stdout|off`
(default `stdout`) and `--log-raw` (default off). A file path can come later if a
deployment wants one; the container runtime is the log shipper today.

**Throughput.** The benchmark saturated at ~5,000 req/s. A ~600-byte line per request
is ~3 MB/s through a locked stdout, written synchronously on the request's async
task. That is fine. If it ever shows in a profile, the fix is a bounded channel and
a writer thread (the `tracing-appender` non-blocking shape). Do not build it first,
and if it is built, drop records rather than block the reactor — a full log channel
must never become back-pressure on bundle reads.

## 7. What to do with `TraceLayer`

Remove it (`routes.rs:106`) once the access record exists, and drop `trace` from the
`tower-http` features. It duplicates the record with worse hygiene, it is silent at
the production log level so it protects nothing, and leaving it means a future
`RUST_LOG=debug` on a live host logs raw URIs. If someone wants request-level
`tracing` spans for local debugging later, `TraceLayer` takes a custom
`make_span_with` that can omit the URI; that is the form to reintroduce it in, never
the default.

## 8. Bundle opens

The benchmark measured cold opens by hand — p50 9.9 ms, max 143 ms, `babel` at 1.5 B
triples in 18 ms — and the shared-storage gate in the deployment plan's §6 is
exactly that number under real conditions. It should land in the same table as
everything else. `Service::open` (`service.rs:188`) can time `catalog.get` and record
it as `open_ms`, but it cannot today tell a first open from a cached one:
`Catalog::get` (`catalog.rs:127`) returns an `Arc<Store>` either way. The cheapest
honest signal is a `Catalog::is_open(&BundleId) -> bool` on `kgf-store` — a lock and
a match on the entry state, read-only, no interior mutability added — probed before
the call, giving `first_open`. Racy under a concurrent first open, and that is fine
for a log field. The alternative, a `tracing::info!` inside `get_with`'s `Closed` arm
(`catalog.rs:150`), would be the store's first event ever (it depends on `tracing`
and emits nothing today) and would land on stderr instead of in the record. Prefer
the probe.

## 9. Admission

A 429 is a `Problem` with `RateLimited`, so the record gets `code: rate_limited`
through §3.2, and `queue_ms` says how long the request waited before refusal. Add
`queued` — the waiting room's available permits at entry — on every record:
`AdmissionController::queued_available` (`admission.rs:165`) is `cfg(test)` today
and is a single `Semaphore::available_permits`; make it `pub(crate)`. Together with
`time` this is the *peak concurrency* row of doc 12 §12.2, and it is what tells the
operator whether the defaults `Admission::new` chose from the local load pass (32
active, 128 queued, 500 ms) fit the cluster.

## 10. Tests

- **Shape tier leaks nothing.** Build records from requests carrying a distinctive
  token in every content position — `q=SECRET`, `s=<http://x/SECRET>`, a bindings
  body with `"SECRET"`, a 404 path `/SECRET` — serialize, and assert the byte string
  `SECRET` does not appear. Then the same with `--log-raw` and assert where it does.
  This is the test that makes doc 12 §12.1 a property rather than a review item.
- **One record per response**, over the real listener (`tests/serve.rs`).
  `Deployment::serve_config` (`serve.rs:1845`) builds `Config` directly, so it can
  install an `Arc<Mutex<Vec<AccessRecord>>>` sink. Drive a fixed sequence — a 200
  page, its 304 revalidation, a 404, a 405, a 413 (the model is
  `a_request_is_refused_before_it_costs_anything`, `serve.rs:1153`), a `latest` 307 —
  and assert one record each, with `status`, `code`, `operation`, and for the 200
  `rows`/`complete`/`truncation_reason` equal to the `KGF-*` headers the existing
  completeness test reads (`serve.rs:1397`).
- **The id round-trips.** Every response carries `KGF-Request-Id`, distinct per
  response, equal to its record's `request_id`.
- **Bindings and brTPF transports** are told apart: QUERY, POST, and GET `values=`
  produce `transport` `query`/`post`/`get-values` with `shape.k`.
- **429 mapping** is a unit test on the layer's `Problem` → record path with a
  synthetic `RateLimited`; forcing a real 429 over the wire is racy and
  `admission.rs`'s own tests already cover the gate.

## 11. What the record answers, and how

Doc 12 §12.1's collection points list QLever logs, MCP events, TPF logs, and human
UIs. It does not yet list `kgf serve` — add it when this lands. Against §12.2's
census rows:

| §12.2 row | from the record |
|---|---|
| Query shape distribution | `operation`, `shape.pattern`, `shape.text` |
| `VALUES` usage, list sizes | bindings records: `shape.k`, `shape.columns` |
| Result sizes; paging depth | `rows`; pages per `request_hash`; `cursor` |
| FILTER kinds | `shape.text` (text); range filters have no server-side signal until `/ranges` exists |
| Entity resolution frequency | `search` records, `shape.q_len`, `shape.roles`; `labels` records, `shape.k` |
| Repetition rate, cacheability | `request_hash`; `status = 304` |
| Failure telemetry | `status`, `code`, `truncation_reason` |
| Traffic distribution, peak concurrency | `time`, `dataset`, `queued`, `queue_ms` |
| Cross-KG sessions | `client_hash` × `dataset` within a time window — approximate; exact sessions need the receipt join (§4) |
| Client mix | `client_class`, `user_agent` |

What it cannot answer, so nobody looks for it here: task-level value weighting (doc
12 §12.3 — needs client receipts), tokens (client-side), and anything in a body.

Analysis is `read_json_auto` over the shipped lines:

```sql
SELECT operation, shape.pattern, count(*) AS requests,
       quantile_cont(total_ms, 0.95) AS p95_ms
FROM read_json_auto('access-*.jsonl')
GROUP BY ALL ORDER BY requests DESC;
```

## 12. Sequence

1. `AccessRecord`, the sink trait, the stdout default, the two flags. Half a day.
2. The outermost layer, `render_problems` leaving `ErrorCode` behind,
   `KGF-Request-Id`, `MatchedPath`, `ConnectInfo`. Half a day.
3. `GetRequest::shape`, term kinds on `BoundTerm`, `Rendered.rows`/`cardinality`,
   `blocking` timings, `Observation` through the four `operate_*` and the three
   descriptor handlers, the 304 and redirect paths. The bulk of it — up to a day.
4. Client identity and admission fields. A quarter day.
5. Tests per §10. Half a day.
6. Remove `TraceLayer`. Update `CLAUDE.md`'s status paragraph and `notes/plan.md`
   (unit 22). In `../kgf`: doc 12 §12.1 gains `kgf serve` as a collection point and
   the two §5 decisions; doc 03 §3.6 lists `KGF-Request-Id`; doc 06 §6.2.1's receipt
   carries server request ids; the deployment plan's §3.5 flips to done.

## 13. Not in this

A Prometheus `/metrics` endpoint (doc 05 §5.7) — derive from the record first.
Sampling — log everything; the rate is fine. Log rotation and shipping — the
container runtime's. Auth and API keys (doc 03 §3.6 leaves them optional and
undecided). Body capture in any tier. Per-response work-unit accounting, which doc
07 §7.5 item 15 deliberately deferred.

## Questions for `../kgf`

To be moved into `notes/plan.md`'s list when the unit lands:

1. Doc 12 §12.1 lists four collection points and not `kgf serve`; the server's own
   access record is now the primary one for the tier the design targets.
2. `KGF-Request-Id` — a response header on every response, named with the `KGF-*`
   completeness family. Doc 03 §3.6 should list it.
3. Receipts (doc 06 §6.2.1) should carry the server request ids of the operations
   that built a table, so transcript-to-log joins are exact (doc 12 §12.5.4).
4. Whether a 64-bit hash of the canonical request belongs in the shape tier (§5.1).
5. The client library and `kgfq` should send a distinctive `User-Agent`; doc 06 has
   no such requirement.
