# Comunica over the public KGF deployment: are KGF-specific customizations worth it?

**Date:** 2026-09-06
**Deployment:** `https://apps.okn.us/kgf/` — 41 datasets, `kgf` 0.1.3, protocol 1
**Client:** kgf-sparql (`../kgf-sparql`), Comunica 5.3.0, Node 25.9.0
**Corpus:** the 60 single-KG `auto` tasks of the mcp-okn benchmark, plus the registry's seven multi-KG queries

## Executive summary

Stock Comunica already speaks to KGF through its shipped TPF (`qpf`) and brTPF (`brtpf`)
source actors. This run asks whether any KGF-informed customization is a *real*
improvement over those built-in setups on the public deployment, with every candidate
lever run as its own arm, back to back with the baselines, on the same queries against
the same immutable dataset versions. Eleven arms, 60 tasks, a full repeat of two arms,
and seven federation queries later, the answer is **yes, but not where the effort was
heading.**

1. **Never use the plain TPF setup against KGF.** Built-in brTPF completes 39 of 60 tasks;
   built-in TPF completes 35, loses five that brTPF finishes, and issues 1.5× the requests
   at the median (up to 36× on a federation query). Bindings pushdown is the single most
   valuable thing the server offers, and stock Comunica already uses it.
2. **The VoID selectivity actor is a tail fix, exactly as it was on the local server.**
   Median request ratio 1.00× against brTPF, four tasks better, one worse, one rescue
   (the chemical-names query: timeout → 849 requests in 3.8 s). It is neutral across
   sources in the federation set. It earns its place for the rescue, not for throughput.
3. **The levers that move the median are the boring ones: page size and bind block
   size, sized from KGF's published caps.** Neither is a planner change. Individually they
   are modest (ramped pages 0.87×, block 256 alone 1.00×); *together with VoID* they
   reach **0.43× requests at the median, 27 of 39 tasks better, 3 rescued, none lost, and
   wall clock on the common set cut from 112 s to 61 s.** The pairing matters: a bind
   block of 256 cannot fill from 100-row pages.
4. **The single largest wall-clock lever is a one-line HTTP header.** Comunica's default
   `Accept` ranks JSON-LD above Turtle and KGF offers neither N-Quads nor TriG, so **every
   page Comunica has ever fetched from KGF was JSON-LD** — parsed by a streaming JSON-LD
   parser that is 17–26× slower than the Turtle parser on a 10,000-row page. Asking for
   Turtle changes no plan and no request count, cuts per-task wall clock by 19% at the
   median, and sidesteps a Comunica streaming-parse failure that kills one query outright.
   Its one cost is that faster parsing raises Comunica's already unbounded request
   concurrency (up to 831 in flight measured), so it needs the concurrency cap in item 5.
   Better still is KGF serving TriG or N-Quads, which Comunica ranks above JSON-LD by
   default and which also lets it separate controls from data: with today's single-graph
   formats, hydra control triples leak into results (§4).
5. **The public deployment exposes robustness gaps that the local server hid.** Any 4xx
   from a fragment request becomes an unhandled promise rejection that terminates the
   process (two Comunica-side triggers: bare datatype IRIs in `o=`, and the `??o` variable
   its property-path expansion emits). Comunica issues hundreds of concurrent requests
   (up to 831 in flight measured) and does not retry network-level failures, which
   produced a dozen spurious "fetch failed" results under mild load. dreamkg cannot be
   paged in RDF at all (server bug, §1.1).
6. **Fifteen tasks fail in every arm.** Those are the SOCKG star joins, blocking
   aggregates over broad relations, and unbounded property paths already diagnosed in
   August. No client lever touches them; they need query rewriting or bounded server
   operations, not a better Comunica.

**Recommendation.** Proceed with KGF-specific customizations, but frame them as a thin
*KGF source profile* rather than planner research: prefer Turtle, ramp the page size from
the caps, size bind blocks from the caps (bounded by the GET URL ceiling, or moved to the
`QUERY` body to reach 1,000), fetch VoID from `/void` at first contact, retry
network-level failures and cap concurrency, and fix the two serializations that produce
400s. Measured together (`all+turtle`), that profile completes 42 of 60 tasks instead of
39, at 0.43× the requests and roughly half the wall clock, with identical answers.
Everything in it is a source/HTTP-layer change; the only planner component is the
selectivity actor that already exists.

## 1. What the public deployment exposes to a Comunica client

Everything below was verified against the live server on 2026-09-06.

| Fact | Value | Consequence for Comunica |
|---|---|---|
| Hydra search form | `…/fragment{?s,p,o}` mapped to `rdf:subject/predicate/object` | stock `qpf` and `brtpf` actors discover it unmodified |
| `hydra:totalItems` | present, exact, on every page | pattern cardinalities reach the planner verbatim |
| `hydra:itemsPerPage` | absent | Comunica models request cost per request, not per item |
| Page size | `default_limit` 100, `max_limit` 10,000; `limit=` is preserved on `hydra:next` links, and a cursor minted at one limit is accepted at another | a client can enlarge pages at any point in a scan |
| brTPF bindings | `values=` is honored **only when the pattern's variables are declared** (`s=?s&…&values=(?s){…}`), which is what Comunica sends; an undeclared table is silently ignored and the unrestricted page is returned | works for Comunica; a hand-built client can be silently wrong |
| Term grammar | two grammars on `/fragment`, chosen by the negotiated representation: RDF representations accept the TPF spelling (bare IRIs) as a fallback to the native bracket/CURIE grammar; native JSON accepts only the native grammar | Comunica's bare IRIs work; its bare datatype IRIs inside typed literals do not (§6) |
| Binding cap | `max_bindings` 1,000, `max_request_bytes` 1 MiB (body); over GET the front end rejects URLs above roughly 55 KB (600 × 87-byte IRIs pass, 700 return 502) | bind blocks of a few hundred are safe over GET; 1,000 needs the `QUERY`/`POST` body |
| Representations | Turtle, JSON-LD, HTML, native JSON; **no N-Quads or TriG** (406) | Comunica's default `Accept` lands on JSON-LD, and with no quad format the hydra controls cannot be separated from data (§4) |
| Statistics | `/void` publishes `void:distinctSubjects` / `void:distinctObjects` per property partition, plus class-partitioned property partitions; `/schema?children=properties` serves the same counts as JSON | the VoID selectivity actor's tier-1 inputs exist for all 41 datasets |
| Latency | 60–70 ms per request from this client, against ~3 ms on the local server used in August | per-task wall clock is 1.8× the local run at the median for the same plans (§5) |

### 1.1 A server defect found on the way: dreamkg cannot be paged in RDF

`dreamkg` v0.0.4 contains nine subject IRIs with a literal space
(`dreamkg:/service/channel/AB-…%28Delaware%20Valley Community%20Health…`). The Turtle
and JSON-LD serializers fail on them with HTTP 500 and the message "the bundle could not
be read while answering this request"; the native JSON and HTML representations return
them unchanged. Consequences:

- a full scan of `dreamkg` at the default page size fails on page 91 (rows 9,000–9,100);
- any page of 9,500 rows or more fails, as does any page that spans those rows;
- `?s rdf:type ?o` with `limit=10000` fails as well.

Any TPF client enumerating dreamkg will hit this. It is a bundle data-quality issue
(the IRIs should have been percent-encoded at build time) compounded by a serving issue
(a serializer failure surfaces as a misleading 500 instead of a per-term diagnostic).
Two corpus tasks target dreamkg; they complete only because they never page that far,
and the naive 10,000-row arm fails both of them on its very first request.

## 2. The arms

All arms use the same engine binary (`kgf-sparql`, whose default config is stock
`@comunica/query-sparql` plus the VoID selectivity actor, which declines when no
statistics are supplied). The runner is [`test/run-arm.mjs`](../../kgf-sparql-bench/test/run-arm.mjs); the
driver is [`test/corpus-arms.py`](../../kgf-sparql-bench/test/corpus-arms.py); the arms are defined once in
[`test/corpus_arms_common.py`](../../kgf-sparql-bench/test/corpus_arms_common.py).

| Arm | Source type | Statistics | Page size | Bind block | Accept | What it tests |
|---|---|---|---|---|---|---|
| `qpf` | TPF | none | server default (100) | n/a | default | the built-in TPF setup |
| `brtpf` | brTPF | none | 100 | 64 | default | the built-in brTPF setup — the baseline |
| `void` | brTPF | KGF VoID | 100 | 64 | default | the existing customization: data-driven join selectivity |
| `block256` | brTPF | none | 100 | 256 | default | bind blocks sized for the GET ceiling |
| `page10k` | brTPF | none | 10,000 on every request | 64 | default | the naive "ask for `max_limit`" lever |
| `ramp` | brTPF | none | 100 on a pattern's first page, 10,000 on continuation and bindings pages | 64 | default | the same lever without over-fetching first pages |
| `void+ramp`, `void+block256` | brTPF | KGF VoID | as named | as named | default | pairwise attribution |
| `all` | brTPF | KGF VoID | ramp | 256 | default | the combined KGF-informed profile |
| `turtle` | brTPF | none | 100 | 64 | `text/turtle` | the representation lever alone |
| `all+turtle` | brTPF | KGF VoID | ramp | 256 | `text/turtle` | the full profile |

The page-size and `Accept` levers are implemented in the runner's `fetch` wrapper —
adding `limit=` to outgoing fragment URLs, or replacing the `Accept` header — which is
byte-for-byte what a KGF-aware source or HTTP actor would send from the published caps.
The bind block size is a Components.js config override
([`test/cfg-block256.json`](../../kgf-sparql-bench/test/cfg-block256.json)). Statistics come from
[`test/void-stats-public.json`](../../kgf-sparql-bench/test/void-stats-public.json), built from the public
`/schema` by [`test/build-void.mjs`](../../kgf-sparql-bench/test/build-void.mjs).

Per task, every arm records completion status, row count, an order-independent result
hash, HTTP requests, requests carrying bindings, HTTP status counts, wall clock (engine
construction excluded), and peak RSS. Limits: 60 s and a 2 GB Node heap per arm. Two to
three tasks ran concurrently, so wall clock is under mild contention; request counts are
unaffected. A second full run of `brtpf` and `all` measured run-to-run variance (§2.1).
Cells that failed with a network-level `fetch failed` were re-run; §6 explains why they
occurred.

### 2.1 Reproducibility

<!-- TABLE:variance -->
- `brtpf`: 38 tasks completed in both runs; requests identical in 34, within 10% in 38, p10–p90 of run2/run1 = 1.00–1.00; wall-clock median ratio 0.86
- `all`: 39 tasks completed in both runs; requests identical in 37, within 10% in 37, p10–p90 of run2/run1 = 1.00–1.00; wall-clock median ratio 0.97
  - `biobricks-ice/names-of-chemical-entities`: 1,300 → 906 requests

Request counts are a property of the plan, and the plan is deterministic except where
`LIMIT` without `ORDER BY` lets timing decide which rows arrive first. Wall clock varies
with contention and should be read to about ±20%.

## 3. Corpus results

<!-- TABLE:arms -->
| Arm | Completed | Timeout | OOM | Crash | Requests (common set) | Wall clock (common set) | Bindings requests | Median request ratio vs brTPF | Better / worse (>10%) | Rescued / lost vs brTPF |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| built-in TPF (`qpf`) | **35** | 17 | 5 | 3 | 25,957 | 160 s | 0 | 1.53× | 3 / 20 | 1 / 5 |
| built-in brTPF (`brtpf`) | **39** | 14 | 4 | 3 | 15,163 | 112 s | 668 | 1.00× | 0 / 0 | 0 / 0 |
| + VoID selectivity | **40** | 12 | 5 | 3 | 12,658 | 92 s | 588 | 1.00× | 4 / 1 | 1 / 0 |
| + bind block 256 | **39** | 15 | 3 | 3 | 14,993 | 100 s | 496 | 1.00× | 15 / 1 | 0 / 0 |
| + 10k pages everywhere | **39** | 5 | 11 | 5 | 13,552 | 136 s | 610 | 0.89× | 19 / 1 | 2 / 2 |
| + ramped pages | **41** | 4 | 11 | 4 | 13,539 | 76 s | 570 | 0.87× | 21 / 1 | 2 / 0 |
| + VoID + ramped pages | **41** | 6 | 9 | 4 | 11,190 | 65 s | 541 | 0.83× | 24 / 1 | 2 / 0 |
| + VoID + block 256 | **40** | 12 | 5 | 3 | 12,626 | 91 s | 435 | 1.00× | 18 / 2 | 1 / 0 |
| + VoID + ramp + block 256 | **42** | 5 | 9 | 4 | 10,873 | 61 s | 223 | 0.43× | 27 / 1 | 3 / 0 |
| brTPF + Accept Turtle | **39** | 13 | 5 | 3 | 15,163 | 89 s | 662 | 1.00× | 0 / 0 | 0 / 0 |
| all + Accept Turtle | **42** | 5 | 9 | 4 | 10,843 | 54 s | 194 | 0.43× | 27 / 1 | 3 / 0 |

"Common set" is the 32 tasks completed by every arm; "median request ratio" and
"better/worse" are per task against `brtpf` over the tasks both arms completed;
"rescued/lost" count tasks that only one of the two completed.

<!-- TABLE:rescued -->
- `qpf`: rescued `sockg/water_sample` (4,627 req, 10.8 s); lost `nde/dataset-count-by-agent` (timeout), `nde/nde-influenza-studies` (timeout), `prokn/protein_kinases` (timeout), `ruralkg/rural_counties_rucc_7_9` (timeout), `ruralkg/telehealth_treatment_providers` (timeout)
- `void`: rescued `biobricks-ice/names-of-chemical-entities` (849 req, 3.8 s); lost none
- `page10k`: rescued `ncipidkg/list-all-labeled-interactions` (79 req, 50.3 s), `nde/nde-resources-by-count` (4,034 req, 29.5 s); lost `dreamkg/services-available-weekend` (error), `dreamkg/services-in-more-than-one-language` (error)
- `ramp`: rescued `ncipidkg/list-all-labeled-interactions` (81 req, 49.2 s), `nde/nde-resources-by-count` (4,035 req, 27.3 s); lost none
- `void+ramp`: rescued `biobricks-ice/names-of-chemical-entities` (1,414 req, 12.1 s), `ncipidkg/list-all-labeled-interactions` (81 req, 51.9 s); lost none
- `void+block256`: rescued `biobricks-ice/names-of-chemical-entities` (765 req, 3.5 s); lost none
- `all`: rescued `biobricks-ice/names-of-chemical-entities` (1,300 req, 12.0 s), `ncipidkg/list-all-labeled-interactions` (81 req, 50.8 s), `nde/nde-resources-by-count` (1,031 req, 22.1 s); lost none
- `all+turtle`: rescued `biobricks-ice/names-of-chemical-entities` (1,067 req, 10.1 s), `nasa-gesdisc-kg/frequent-sciencekeywords` (132 req, 13.0 s), `ncipidkg/list-all-labeled-interactions` (81 req, 33.1 s); lost none

### 3.1 Reading the table

**Built-in TPF versus built-in brTPF.** Without bindings pushdown Comunica falls back to
one request per binding (`inner-bind`): 895 requests instead of 22 on the hydrology
query, 3,482 instead of 73 on the NSDUH query, and five tasks that brTPF finishes in
2–8 s time out. The one task TPF "rescues" (`sockg/water_sample`) is a plan-shape
accident — its brTPF plan materialises and times out, its TPF plan happens to stream.

**VoID selectivity.** The same result as the August local run, now on the public server:
the median task is untouched and the pathological one is rescued. Four tasks improve by
2–20× (`pubs-by-year-title` 246 → 12, `abdominal-cell-types` 2,593 → 321,
`ffmpeg-vulnerabilities` 14 → 6, `ffmpeg-dependencies` 33 → 12); one gets modestly worse
(`datasets-science-keywords`, 118 → 148, the known `selectivityModifier` double-count on
an aggregate). The actor makes no HTTP requests of its own, so it is free where it
declines.

**Naive 10,000-row pages.** Requests drop (0.89×) but wall clock *rises* (136 s versus
112 s on the common set), seven timeouts turn into out-of-memory failures because the
materialising plans fill the heap faster, and dreamkg fails on page one (§1.1). Under
`LIMIT` it over-fetches: the assays query issues 23 requests instead of 8 because the
first 10,000-row page feeds the bind-join far more candidates than the limit needs.
**Do not ship this form.**

**Ramped pages.** Keeping a pattern's first page at 100 rows and enlarging only
continuation and bindings pages keeps the request saving (0.87×, 21 tasks better) and
turns the wall-clock loss into a gain (76 s). It still cannot save the plans that
materialise a million triples: they run out of memory instead of timing out.

**Bind block 256 alone is a null result at the median** (1.00×) because a 100-row page
never fills a 256-binding block; the pushdown operator sends whatever it has. **Paired
with ramped pages it is the largest request lever in the study:** `all` versus
`void+ramp` is 0.43× versus 0.83× at the median, with bindings requests on the common set
falling from 541 to 223. `ruralkg/list-providers` goes 590 → 154 → 48; `prokn/protein_kinases`
1,031 → 853 → 220; `nde/dataset-count-by-agent` 1,255 → 918 → 238.

### 3.2 Selected tasks

<!-- TABLE:selected -->
| Task | `qpf` | `brtpf` | `void` | `ramp` | `all` | `all+turtle` |
|---|---:|---:|---:|---:|---:|---:|
| `biobricks-ice/names-of-chemical-entities` | timeout | timeout | 849 / 3.8 s | oom | 1,300 / 12.0 s | 1,067 / 10.1 s |
| `nasa-gesdisc-kg/frequent-sciencekeywords` | timeout | timeout | timeout | timeout | error | 132 / 13.0 s |
| `ncipidkg/list-all-labeled-interactions` | timeout | timeout | timeout | 81 / 49.2 s | 81 / 50.8 s | 81 / 33.1 s |
| `nde/nde-resources-by-count` | timeout | timeout | timeout | 4,035 / 27.3 s | 1,031 / 22.1 s | error |
| `nde/dataset-count-by-agent` | timeout | 1,255 / 16.3 s | 1,254 / 13.7 s | 918 / 6.3 s | 238 / 5.0 s | 238 / 3.4 s |
| `ubergraph/abdominal-cell-types` | 3,162 / 14.7 s | 2,593 / 11.7 s | 321 / 2.8 s | 2,578 / 9.7 s | 279 / 2.5 s | 279 / 1.6 s |
| `nasa-gesdisc-kg/pubs-by-year-title` | 614 / 7.3 s | 246 / 2.9 s | 12 / 0.4 s | 109 / 2.1 s | 48 / 1.4 s | 18 / 0.9 s |
| `nasa-gesdisc-kg/count-publications-use-each-dataset` | 555 / 20.1 s | 555 / 24.1 s | 555 / 18.1 s | 8 / 2.6 s | 8 / 3.1 s | 8 / 1.1 s |
| `ruralkg/list-providers` | 883 / 19.8 s | 590 / 5.5 s | 590 / 5.6 s | 154 / 3.5 s | 48 / 3.1 s | 48 / 2.1 s |
| `prokn/protein_kinases` | timeout | 1,031 / 8.3 s | 1,031 / 8.1 s | 853 / 4.4 s | 220 / 3.7 s | 220 / 2.0 s |
| `hydrologykg/sawgraph-hydrology-02` | 895 / 3.3 s | 22 / 0.8 s | 22 / 0.8 s | 21 / 0.7 s | 12 / 0.6 s | 12 / 0.5 s |
| `oard-kg/diseases-associated-with-phenotype` | 6,600 / 25.2 s | 6,600 / 24.4 s | 6,600 / 24.1 s | 6,591 / 31.1 s | 6,591 / 24.1 s | 6,591 / 23.3 s |
| `biobricks-ice/assays-from-invitrodb` | 6 / 0.7 s | 8 / 0.5 s | 8 / 0.7 s | 36 / 0.7 s | 12 / 0.5 s | 12 / 1.3 s |

`oard-kg/diseases-associated-with-phenotype` is the shape no lever touches: 6,600
requests in every arm, because its plan is 6,500 one-binding probes of a
`LIMIT`-bounded chain. That is a plan-shape problem, not a page-size or block-size one.

## 4. The representation lever: Comunica has been reading JSON-LD all along

Comunica's dereference actor sends
`Accept: application/n-quads, application/trig;q=0.95, application/ld+json;q=0.9, application/n-triples;q=0.8, text/turtle;q=0.6, …`.
KGF serves Turtle, JSON-LD, HTML, and its native JSON, and answers N-Triples with 406. So
the negotiated representation is JSON-LD for every fragment page, and has been in every
measurement kgf-sparql had taken.

Measured on one 10,000-row `rdfs:label` page from biobricks-ice (1.6 MB Turtle,
1.8 MB JSON-LD, both gzip on the wire):

| Parser | Quads | Parse time (3 runs) |
|---|---:|---:|
| N3 `StreamParser` (Turtle) | 10,014 | 11–18 ms |
| `jsonld-streaming-parser` (JSON-LD) | 10,014 | 288–362 ms |

Forcing `Accept: text/turtle` on fragment requests changes no plan and no request count:

<!-- TABLE:turtle -->
- `turtle` vs `brtpf`: 39 tasks both completed; requests median ratio 1.00×; wall clock 153 s → 129 s (median per-task ratio 0.81×)
- `all+turtle` vs `all`: 41 tasks both completed; requests median ratio 1.00×; wall clock 141 s → 111 s (median per-task ratio 0.83×)

It also **rescues a query none of the planning levers could.**
`nasa-gesdisc-kg/frequent-sciencekeywords` (a `GROUP BY` / `COUNT` over three broad
patterns) times out in every JSON-LD arm; in `all` it dies after 164 requests with
`Invalid UTF-8 character at position 0 in state STRING1` from the JSON parser. That is
a parser defect, not bad server output, and it reproduces outside Comunica: all 105
10,000-row JSON-LD pages of the three predicates the query touches are valid UTF-8 and
valid JSON when downloaded whole, yet piping the same responses straight from `fetch`
into `jsonld-streaming-parser` (`Readable.fromWeb(res.body).pipe(new JsonLdParser())`)
fails on 6 of the 96 `rdfs:label` pages with exactly that message — the JSON tokenizer
mishandles a multi-byte character split across stream chunks. Turtle pages parse
cleanly, and with Turtle the query completes in 13 s and 132 requests.

Two ways to apply this, and the server-side one is better. A KGF-aware HTTP actor (or a
context option on the dereference actor) can prefer Turtle today in a few lines. But
Comunica's own preference order is `application/n-quads` (1.0), `application/trig`
(0.95), then JSON-LD (0.9): **if KGF serves TriG or N-Quads, every Comunica client gets
the fast parser with no client change.** N3 parses all three text formats at the same
speed (8–15 ms for the 10,000-row page as Turtle, N-Quads, or TriG; N-Quads is 40%
larger uncompressed, TriG the same size as Turtle).

The quad formats fix a second, worse problem. The TPF specification requires that in a
multi-graph syntax "metadata triples MUST be serialized to a non-default graph" and
"control triples MUST be serialized to a non-default graph", and Comunica's default
configuration separates metadata from data **only** for quad streams (the
`primary-topic` actor); for triple streams it falls back to the `all` actor, which
copies every triple into both streams, and the QPF source then keeps whatever matches
the pattern. So with the single-graph formats KGF serves today, **hydra controls leak
into query results**: `SELECT ?s ?p ?o` against `phaseskg` (2,750 triples) returns
3,141 rows, the extra 391 being `hydra:search`, `hydra:mapping`, `hydra:totalItems`,
`hydra:next`, and `<fragment> a void:Dataset` from each of the 28 pages — identically in
Turtle and JSON-LD. Any pattern that can match a control triple (variable predicate,
`?s a ?type`, a variable object with the fragment as subject) is affected. To make
Comunica split the graphs, the metadata graph must carry the two links the reference LDF
server emits: `<G> foaf:primaryTopic <F>` and `<F> void:subset <U>`, where `U` is the
exact URL of the requested page (cursor and `values` parameters included) and `F` is
the fragment it belongs to; Comunica compares `U` against the URL it fetched. Without
that link Comunica treats every quad as data again.

## 5. Federation

The registry's multi-KG queries, minus those with a hard-coded `SERVICE` or a Wikidata
dependency, run with one brTPF source per tagged dataset:

<!-- TABLE:federation -->
| Query | Sources | `qpf` | `brtpf` | `void` | `ramp` | `all` |
|---|---|---:|---:|---:|---:|---:|
| `federation/fio-spatial-facilities-in-state` | fiokg, spatialkg | oom | crash | crash | crash | crash |
| `federation/nde-diseases-mondo-parents` | nde, ubergraph | 1,866 / 9.3 s | 921 / 4.3 s | 911 / 4.5 s | 889 / 9.3 s | 536 / 8.4 s |
| `federation/nde-study-mondo-xrefs` | ubergraph, nde | 22 / 0.4 s | 22 / 0.3 s | 22 / 0.4 s | 22 / 0.3 s | 22 / 0.4 s |
| `federation/sawgraph-hydrology-spatial-04` | hydrologykg, spatialkg | 17,386 / 27.9 s | 17,386 / 29.6 s | 17,386 / 32.6 s | 17,386 / 30.2 s | 17,386 / 34.9 s |
| `federation/sawgraph-hydrology-spatial_03` | hydrologykg, spatialkg | oom | oom | oom | oom | oom |
| `federation/sawgraph-spatial-hydrology-01` | hydrologykg, spatialkg | oom | crash | crash | crash | crash |
| `pankgraph/non-acinar-cell-adhesion-regulation` | pankgraph, ubergraph | 29,591 / 48.4 s | 818 / 2.7 s | 808 / 5.8 s | 763 / 2.8 s | 582 / 4.3 s |

Three observations. **VoID selectivity is neutral across sources** (911 versus 921
requests), as the August guidance predicted: the containment estimate has no information
about cross-bundle overlap. **Bigger blocks still help** the bind joins that cross the
boundary (`all` 536 versus 921). **The plan-shape failures dominate:** the 17,386-request
enrichment plan is identical in every arm, one query exhausts the heap everywhere, and
two crash in every brTPF arm on the same `??o` property-path request (§6).

Compared with the August local run on the same tasks, the public server executes the
same plans (median request ratio 1.00×) at 1.8× the wall clock; four heavy tasks that
finished locally in 17–50 s now exceed the 60 s limit in the baseline arms.

## 6. Robustness on the public deployment

These surfaced only because the server is remote and shared. All are reproducible.

| Symptom | Cause | Where to fix |
|---|---|---|
| Process terminates with an unhandled rejection, `Request failed: …fragment?…&o=%221998-04-20%22%5E%5Ehttp%3A%2F%2Fwww.w3.org…` (three sockg tasks, every arm) | The TPF specification's search-form encoding is bare: an IRI is "the text value of the IRI", a typed literal is the quoted value followed by `^^` and "the text value of the IRI", e.g. `"42"^^http://www.w3.org/2001/XMLSchema#integer`. Comunica (via `rdf-string`) sends exactly that, for IRIs and literals alike. KGF's `/fragment` runs two grammars: for a native JSON representation the documented one (brackets or CURIE, §3.3 of the API doc), and for an RDF representation a TPF fallback (`BoundTerm::parse_fragment` in `kgf-server/src/request.rs`) that tries the native grammar first and then accepts the parameter if the **whole** value is an absolute IRI. That is why every bare `p=http://…` and `s=urn:…` Comunica sends is accepted. The fallback does not look inside a literal, so `"v"^^http://…` fails the native parse on the datatype token, fails the whole-value IRI test, and returns the native 400 `bad_term_syntax`. The 400 then escapes Comunica as an unhandled promise rejection whether or not the retry actor is configured | **KGF**, by route rather than by patch: keep `/fragment` strictly native in every representation and move TPF interoperability to the `/tpf` route already sketched in API doc §3.8, whose grammar is Hydra `ExplicitRepresentation` verbatim (bare IRIs, `"lex"`, `"lex"@lang`, `"lex"^^IRI`, nothing else). The RDF representations of `/fragment` keep a Hydra form whose template points at `/tpf`; Comunica uses the form's template for every request after the first, so existing `brtpf@…/fragment` configurations migrate on their own. The form must carry exactly the three `s`/`p`/`o` mappings or Comunica ignores it, so `limit`/`cursor` stay out of the template. Retire the `parse_fragment` fallback after a deprecation window, and reject manifest prefixes that are URI schemes so native CURIEs can never collide with IRIs. Comunica separately: surface a 4xx as a query error instead of a process exit |
| Same crash on `…&o=%3F%3Fo` (two federation queries, every brTPF arm) | transitive property-path expansion creates a variable literally named `?o`, serialised as `??o` | Comunica |
| `fetch failed` (`ECONNRESET`) on 1–3% of heavy cells under load; the same cell usually passes in isolation | Comunica issues hundreds of concurrent fragment requests (376–396 in flight on `nde/dataset-count-by-agent` with JSON-LD; **609–831 with Turtle**, because faster parsing feeds the bind join faster) and does not retry network-level errors, only HTTP status codes. `nde/nde-resources-by-count` under `all+turtle` fails one run in two this way and completes in 9 s (versus 24 s with JSON-LD) the other | a concurrency cap and network-error retry in the HTTP actor — a prerequisite for the Turtle lever; KGF-side, a documented connection limit |
| HTTP 429 on 7 of 660 cells | the server's concurrent-work limit under three parallel clients; Comunica honours `Retry-After` and recovers | none needed; expect it in multi-client deployments |
| HTTP 500 on dreamkg pages (§1.1) | IRIs with spaces break the RDF serializers | KGF bundle build and serializer error handling |
| `values=` silently ignored when variables are undeclared | server leniency | KGF should reject a `values` table whose variables are not bound to a position |

## 7. What to build, in order

Each item below is a source-layer or HTTP-layer change; none is planner research.

1. **Get off JSON-LD.** Server side, serve TriG (and/or N-Quads) with metadata and
   controls in a named graph linked by `foaf:primaryTopic`/`void:subset` — Comunica picks
   it by default, parses it 20× faster, and stops leaking controls into results. Client
   side, until then, prefer Turtle on fragment requests: one header, 19% wall clock at
   the median, one query rescued, one Comunica bug avoided.
2. **Ramp the page size from the caps** — default on a pattern's first page, `max_limit`
   on continuation and bindings pages. Never `max_limit` on first pages.
3. **Size bind blocks from `max_bindings`, bounded by the GET URL ceiling** (about 256
   for typical IRIs), and move bindings to the `QUERY /fragment` body to reach 1,000.
   Together with (2) this is the 0.43× result.
4. **Keep the VoID selectivity actor**, and have the source fetch `/void` at first
   contact (immutable, version-pinned, cache forever) instead of taking statistics from
   the caller. It is the tail insurance, and it costs nothing where it declines.
5. **Harden the HTTP path:** retry network-level failures, cap concurrency, and turn 4xx
   into query errors instead of process exits (Comunica). The `??o` path variable is fixed
   in the next Comunica release; the datatype form is resolved on the KGF side by giving
   TPF its own route with the spec's grammar and keeping `/fragment` strictly native (§6).
6. **Report the server defects:** dreamkg space IRIs, silent `values` acceptance, and the
   misleading 500.

What this does *not* buy: the 15 tasks that fail in every arm. They are the SOCKG
class-anchored stars, `DISTINCT`/`GROUP BY` over relations of a million rows, and
unbounded property paths — the failure classes of the August report, unchanged. Those
need the query-authoring guidance, the lint pass, and the bounded server operations
(facets, class-scoped stars, column export) described there, not further work on
Comunica's planner.

## 8. Why client memory is not bounded by a better plan

A plan decides what is fetched and in which order. Memory is decided by which operators
hold state and by what the query semantically requires to be held, and Comunica holds all
of it in the Node heap with no spilling. Those are different axes: pushdown plans usually
fetch less and therefore hold less, but nothing in the planner keeps the heap under a
limit, and a faster plan reaches the same limit sooner. Four measurements from this run:

| Observation | Numbers |
|---|---|
| Heap held per page fetched, stock brTPF, tasks that timed out | 101–472 KB of RSS per 100-row page across 14 tasks (median about 250 KB), i.e. 1–5 KB of heap per triple retained, against roughly 160 bytes per triple on the wire |
| What bigger pages did to plans that were timing out | six tasks went timeout → out-of-memory between `brtpf` and `ramp` (`fio-facilities-by-NAICS-Subsector`, `sawgraph-hydrology-04`, `list_diseases`, `scales-ontology-event-labels`, `harvest_fraction`, `names-of-chemical-entities`); the plan and the total to be held were unchanged, only the rate of arrival |
| Peak RSS of tasks that *completed* under `all` | `nde/nde-resources-by-count`: 2,083 MB for 8 result rows; `ncipidkg/list-all-labeled-interactions`: 1,833 MB for 75,322 rows from 81 requests; `names-of-chemical-entities`: 1,609 MB for 200 rows |
| The August control | removing `LIMIT 10` from the chemical-names query exhausts the heap with the *rescued* plan (guidance doc §5.2.6) |

Where the bytes go, mechanically:

- **Materialising joins.** `join-inner(symmetric-hash)` persists both inputs
  (`persistedItems = |A| + |B|`), `join-optional(hash)` persists its whole left side, and
  both are chosen whenever the planner cannot see a selective side. A 597,129-triple
  `?s ?p ?o` scan held for a hash join is the 1.8 GB above.
- **Blocking operators.** `ORDER BY`, `GROUP BY`, and `DISTINCT` hold their complete input
  before emitting anything; the 2 GB-for-8-rows case is a `GROUP BY` over every dataset
  in the graph. `LIMIT` cannot cut under them.
- **Pipeline buffers.** Every iterator stage buffers, and the bind joins run hundreds of
  branches concurrently (376–831 requests in flight, §6). Under `all+turtle` the same
  query completes at 9 s or resets a connection, depending on how many branches were
  live.
- **Per-binding overhead.** A solution mapping is an immutable map of term objects, not a
  row; that is the 10–30× amplification from wire bytes to heap.

Why the planner does not protect against it: `MediatorJoinCoefficientsFixed` prices
`persistedItems` at weight 1 against 10 for iterations and 10 for I/O, so a hash join
that will hold a million bindings is cheap on paper, and the choice is by total cost
with no comparison against the heap limit. Better selectivity moves plans toward
streaming bind joins — that is the whole VoID effect — but only where the query allows
a selective side; a `GROUP BY` over everything has none.

What actually bounds memory, in order of how much of the corpus it covers:

1. Bindings pushdown wherever a selective anchor exists (the arms above), plus the
   concurrency cap, which bounds in-flight buffers.
2. Doing the blocking work where it is bounded: server-side facets or grouped counts for
   single-pattern `DISTINCT`/`GROUP BY` (§9), or a spillable local engine over exported
   columns for the rest (the August materialising-client design).
3. Failing fast: a `persistedItems` ceiling, or a much higher `memoryWeight`, so that a
   plan expected to hold more than the budget is rejected or re-planned rather than
   exhausting the heap after a minute. Neither is tested here; both are config-level.

## 9. Server-side information that would change planning

The split that matters is between information KGF **already serves that stock Comunica
cannot call**, and endpoints that do not exist yet. The physical plan behind the query no
arm improved settles which is worth more: `oard-kg/diseases-associated-with-phenotype`
runs as `join-inner(bind)`, Comunica re-evaluating a five-pattern star around
`?results_log_odds` once per outer row — 6,600 requests, none carrying bindings — because
the brTPF source accepts one pattern at a time. No statistic changes that; only a source
operation that takes a subject star plus bindings does.

### 9.1 Already served, not consumable by stock Comunica

| Information | Where it lives | Failure it addresses | How Comunica would use it |
|---|---|---|---|
| Bindings-restricted exact counts | `QUERY /count` with a bindings body (verified live; `POST` also works) | join-size guessing, including the cross-source joins where VoID measured neutral | a *plan probe*: sample the smaller side (`GET /sample` exists), count the other pattern for those keys, derive the join cardinality and fan-out exactly. One or two requests per join decision, cached per pattern pair; no containment or independence assumption. Hook: a join actor, or an n-ary selectivity actor allowed to make requests |
| Class-partitioned property counts | `/void` carries `void:entities` per class and `void:triples` per class-property (SoilBiologicalSample: 18,273 entities, 7 partitions) but **no distinct counts at that level** | the SOCKG stars, which global per-predicate counts do not describe | add `distinctSubjects`/`distinctObjects` to the class-level partitions (the same `hdtc void` flag one level deeper) and replace the pairwise estimator with an n-ary one that sees the `rdf:type` pattern in the same BGP. Comunica's selectivity bus already passes every entry of a multi-join; the current actor declines them |
| Predicate inventory | `/schema?children=properties` | variable-predicate namespace filters (`ncipidkg/list-all-labeled-interactions`: timeout → 701 requests when rewritten by hand in August) | an optimize-query-operation actor that rewrites the filter into `VALUES` over the matching predicates |
| Text candidates | `o.text` on `/fragment` and `/count` | label-substring filters that scan every label | a KGF-native source accepting pattern-plus-text-filter shapes and keeping the original `FILTER` as a residual check |
| `hydra:itemsPerPage` | absent from pages today | mixed federations only, where Comunica prices KGF per request rather than per item | trivial to emit; changes relative source cost, not single-source plans |

### 9.2 Endpoints that do not exist yet, ranked by measured need

1. **Subject-star hydration with bindings.** `/star` is advertised in every manifest and
   returns 404. It is the direct answer to the per-row bind join: the oard star over
   about 1,300 result IRIs becomes two requests at 1,000 subjects instead of 6,600, and
   the same shape drives the 17,386-request federation enrichment (§5) and the SOCKG
   stars. Hook: a source declaring `joinBindings` for subject-star BGPs, which routes
   through the bind-source join actor that the TPF source can never use today. The
   caveat from August stands: `/star` arrays are not solution mappings, so the source
   must expand multi-valued properties into the cross product.
2. **Pattern-scoped distinct values or facets.** SCALES needs 75 distinct labels out of
   1.68 million triples; a count says 75, and only an endpoint returning the 75 terms
   helps. In HDT this is an index walk, so it is bounded. Hook: an optimize actor that
   recognises `DISTINCT` or `GROUP BY` over a single pattern. This is also the item that
   bounds memory for that query class (§8).
3. **Characteristic sets.** Per-subject predicate combinations with counts, one SPO pass
   at build time, usually a few thousand entries. They give star cardinalities without
   the independence assumption that class-conditioned coverage still makes, and answer
   "which properties co-occur" for free. Serve in `/void` or `/schema`; consume in the
   same n-ary estimator as the class-partition counts.
4. **Cross-source overlap sketches.** The August tier-2 design. Ranked below the dynamic
   count probe, which answers the same question at query time in one or two exact
   requests; sketches remain useful for source selection or where probe latency matters.

### 9.3 What no server information fixes

Blocking aggregates over million-row relations need bulk transport, not statistics.
Client memory is not bounded by a better plan (§8). And the largest single remaining
cost, the per-row bind join over multi-pattern groups, is an operator-choice limitation
that only the star operation or upstream Comunica work removes.

## Reproducing

From `../kgf-sparql-bench`, the private harness that runs a `../kgf-sparql` checkout:

```sh
KGF_ROOT=https://apps.okn.us/kgf VOID_OUT=test/void-stats-public.json \
  node test/build-void.mjs biobricks-ice dreamkg fiokg hydrologykg nasa-gesdisc-kg \
  ncipidkg nde oard-kg prokn ruralkg scales securechainkg sockg spatialkg ubergraph
TASK_TIMEOUT=60 TASK_WORKERS=3 python3 test/corpus-arms.py test/corpus-arms-public.json \
  qpf brtpf void block256 page10k ramp void+ramp void+block256 all
TASK_WORKERS=2 python3 test/corpus-arms.py test/corpus-arms-public-turtle.json turtle all+turtle
TASK_TIMEOUT=20 python3 test/corpus-arms.py test/corpus-arms-public-rerun.json brtpf all
python3 test/federation-arms.py test/federation-arms-public.json qpf brtpf void ramp all
python3 test/summarize-arms.py test/corpus-arms-public.json
python3 test/report-tables.py
```

Machine-readable results: [`test/corpus-arms-public.json`](../../kgf-sparql-bench/test/corpus-arms-public.json),
[`test/corpus-arms-public-turtle.json`](../../kgf-sparql-bench/test/corpus-arms-public-turtle.json),
[`test/corpus-arms-public-rerun.json`](../../kgf-sparql-bench/test/corpus-arms-public-rerun.json),
[`test/federation-arms-public.json`](../../kgf-sparql-bench/test/federation-arms-public.json).
