# `/tpf` — a spec-exact Triple Pattern Fragments route beside the native `/fragment`

Status: implemented locally, 2026-09-08; the external corpus rerun in step 7 remains a
deployment gate. Supersedes the decision recorded in
[`comunica-brtpf.md`](comunica-brtpf.md) and plan unit 20 that TPF and brTPF are
representations of the one `/fragment` resource with "no `/tpf` alias". They become a
route of their own. The deployment is experimental, so this plan has no deprecation
window and no URL-compatibility shim: `/fragment` changes in place.

## Why

The public deployment was measured with stock Comunica 5.3.0 over the 60-task mcp-okn
corpus (`../kgf-sparql/docs/public-deployment-comunica-evaluation.md`). Two findings are
about the term grammar rather than about performance:

1. **The native grammar and the TPF grammar contradict each other, and today one route
   serves both by accident.** Doc 03 §3.3 brackets IRIs and reserves bare tokens for
   CURIEs, with a written argument for why the delimiter must be required. The TPF
   specification and Hydra's `ExplicitRepresentation` require the opposite: an IRI is
   "the text value of the IRI", a typed literal is `"lex"^^` followed by "the text value
   of the IRI", and Hydra says explicitly that the form "omits angular brackets around
   IRIs". `/fragment` reconciles them with `BoundTerm::parse_fragment`
   (`crates/kgf-server/src/request.rs`), which runs the native grammar first and, when
   the negotiated representation is RDF, accepts the parameter if the whole value is an
   absolute IRI. That is why Comunica's bare `p=http://…` and `s=urn:…` work.
2. **The fallback only covers whole-value IRIs.** `o="1998-04-20"^^http://www.w3.org/2001/XMLSchema#date`,
   the exact form the TPF spec prescribes and Comunica emits, fails the native parse on
   the datatype token and fails the whole-value IRI test, so the server answers 400
   `bad_term_syntax`. Comunica turns that into an unhandled rejection and the process
   exits. Three corpus tasks die this way in every configuration.

Two smaller defects share the cause: a `values=` table with no variable declared in
`s`/`p`/`o` is silently ignored and the unrestricted page is returned; and because
the native grammar runs first, a bare IRI whose scheme coincided with a declared
manifest prefix would be read as a CURIE.

The resolution is to select the grammar **by route, not by `Accept`**. One URL, one
grammar. `/fragment` stays strictly native in every representation. TPF and brTPF get a
route that implements their specification verbatim.

## What Comunica requires of the route (verified against 5.3.0)

These are the facts the design below is shaped by; each was read in the installed
source or measured against the live server.

- **Discovery is by Hydra form, not by URL.** `QuerySourceQpf.getSearchForm` accepts a
  form only if it has *exactly* three mappings onto `rdf:subject/predicate/object` (or
  four with `rdf:graph`), and then uses that form's `hydra:template` for every request
  after the first. A form with a `values`, `limit`, or `cursor` mapping is ignored.
- **brTPF is appended, not templated.** `getBindingsRestrictedLink` hardcodes
  `&values=` onto the templated URL; the payload is SPARQL `VALUES` syntax without the
  keyword, terms in Turtle spelling, `UNDEF` allowed. Comunica also declares the
  pattern's variables in the URL (`s=?assay&…`).
- **Terms in the template are `ExplicitRepresentation`.** `rdf-string`'s
  `termToString`: IRIs bare, literals `"lex"`, `"lex"@lang`, `"lex"^^http://…`. No
  brackets anywhere. Comunica ignores `hydra:variableRepresentation`; it always sends
  this form.
- **Its `Accept` prefers quad formats.** `application/n-quads` (1.0),
  `application/trig` (0.95), `application/ld+json` (0.9), `application/n-triples` (0.8),
  `text/turtle` (0.6). Serving only Turtle and JSON-LD means Comunica has been parsing
  JSON-LD for every page, 17–26× slower than N3 on a 10,000-row page, through a streaming
  parser with a known chunk-split bug.
- **Controls are separated from data only in graph-preserving formats.** Comunica's default
  configuration splits metadata from data with `ActorRdfMetadataPrimaryTopic`, which
  handles quad streams only and needs two links inside the metadata graph:
  `<G> foaf:primaryTopic <F>` and `<F> void:subset <U>`, where `U` is the exact URL it
  dereferenced (cursor and `values` included) and `F` is the fragment the page belongs
  to. For triple streams it falls back to `ActorRdfMetadataAll`, which copies every
  triple into both streams. The old implementation flattened JSON-LD as though it were
  a triple syntax, so its Hydra controls appeared as query results too:
  `SELECT ?s ?p ?o` over `phaseskg` returned 3,141 rows for 2,750 triples. JSON-LD is a
  dataset syntax and can carry named graphs; the TPF specification requires metadata
  and controls in a non-default graph wherever the syntax has graphs.
- **It never sends `page`** and never reads `hydra:previous`; it follows `hydra:next`.

## The contract

### Route

```
GET /{dataset}/v/{version}/tpf{?subject,predicate,object}
GET /{dataset}/v/{version}/tpf{?subject,predicate,object}&values=…     (brTPF)
```

GET only. Body-carrying bindings (`QUERY`/`POST`) stay native on `/fragment`; the TPF
family has no body transport and this route does not invent one. Every core-profile
release answers it; there is no capability flag, as unit 20 already decided.

### Parameters

| Parameter | Meaning |
|---|---|
| `subject`, `predicate`, `object` | the pattern, in `ExplicitRepresentation` (below); omitted or `?name` is a variable |
| `values` | brTPF binding table, SPARQL `VALUES` syntax without the keyword; parsed by `spargebra` exactly as today |
| `cursor` | KGF's opaque continuation, only ever obtained from `hydra:next` |
| `limit` | page size, a KGF extension a client may add out of band (the `../kgf-sparql` harness ramps it); never part of the template |
| `format` | `nq`, `trig`, `ttl`, `jsonld`, `html`, for browsers and debugging |

Anything else is a 400 through the existing `accept_only` machinery, including `s`,
`p`, `o`, `o.text`, and `page`. The conventional TPF names are chosen so that a
hand-written TPF client and the reference LDF server's URLs line up; Comunica reads the
names from the mappings and does not care.

### Term grammar: Hydra `ExplicitRepresentation`, nothing else

Applied to `subject`, `predicate`, `object`. Percent-decoding happens once, in the URL
layer, as it does today.

| Value | Term |
|---|---|
| empty, or `?` followed by word characters | variable (the TPF spec's two spellings) |
| `"lex"` | plain literal |
| `"lex"@tag` | language-tagged literal; the tag is validated as today |
| `"lex"^^IRI` | typed literal, datatype **bare**; `^^http://www.w3.org/2001/XMLSchema#string` folds to plain as `KgfLiteral::typed` already does |
| anything else | an IRI, and it must be one: `oxrdf::NamedNode::new` accepts it or the value is a 400 |

Rules that follow from "nothing else":

- No CURIEs. The prefix map is not consulted on this route, so `urn:…`, `doi:…`,
  `mailto:…`, and KGF's own `urn:fdc:…` skolem IRIs parse as the IRIs they are.
- No brackets. An IRI cannot contain `<`, so `<http://…>` fails `NamedNode::new` and the
  400 says so: "this is the TPF route; angle brackets and CURIEs belong to `/fragment`".
- The lexical form is everything between the opening quote and the closing quote that
  precedes `@`, `^^`, or the end of the value. Hydra says the form "requires no
  escaping"; a value that opens a quote and never closes it is a 400.
- `_:x` is parsed and never matched, the rule doc 03 §3.3 already applies.
- `max_term_bytes` applies to the canonical form, as in `BoundTerm::parse`.

### `values=`

Unchanged in syntax and semantics (`BindingFragment::parse_values`). Two tightenings:
a table with no column declared in `subject`/`predicate`/`object` is a 400 rather than
being ignored, and the brTPF distinct-RDF projection is the only projection this route
has (`distinct_rdf` is always true here; native JSON's binding relation is a
`/fragment` body-transport concern). Extra columns are retained because Comunica sends
upstream join variables alongside the column used by the current pattern; they cannot
affect the distinct-RDF result.

### Representations

Offered, in the server's own preference order for `*/*`:
`application/n-quads`, `application/trig`, `text/turtle`, `application/ld+json`,
`text/html`. Comunica's own ranking lands on N-Quads. All three dataset-capable formats
are serialized by `oxrdfio` (`RdfFormat::NQuads`, `RdfFormat::TriG`, and JSON-LD);
`rdf.rs` grows a quad-aware
`serialize_dataset` beside `serialize_graph`, and `Representation` grows `NQuads` and
`TriG` with tokens `nq` and `trig`. The byte-fitting in `fit_fragment_rdf` works on
serialized bytes and needs no change beyond taking a `DatasetFormat`.

### The document

Data triples in the default graph. Metadata and controls in one named graph,
`<U#metadata>`, where `U` is the exact request URL:

```
<U#metadata> {
  <U#metadata>  foaf:primaryTopic  <F> .            # F = U without cursor and limit; on a first page F = U
  <F>           void:subset        <U> .
  <D>           void:subset        <F> .            # D = the tpf resource of this release
  <D>           a void:Dataset ;
                hydra:search [
                  hydra:template "…/tpf{?subject,predicate,object}" ;
                  hydra:variableRepresentation hydra:ExplicitRepresentation ;
                  hydra:mapping [ hydra:variable "subject" ;   hydra:property rdf:subject ] ,
                                [ hydra:variable "predicate" ; hydra:property rdf:predicate ] ,
                                [ hydra:variable "object" ;    hydra:property rdf:object ]
                ] .
  <U>           hydra:totalItems  N ;                 # exact, as today; the brTPF estimate rule unchanged
                hydra:itemsPerPage limit ;
                hydra:next        <U'> .              # present only when the page is incomplete
  <U>           void:inDataset    <dataset_iri> .     # the manifest's dataset IRI, which /void describes
  <dataset_iri> rdfs:seeAlso      <…/void> .
}
```

Every IRI here is absolute and carries `--public-base`, exactly as the current Hydra
emission does. N-Quads, TriG, and JSON-LD preserve the named metadata graph; Turtle is
the one offered single-graph syntax and necessarily flattens controls and data into its
graph. This is why a graph-preserving format comes first in the server's order. The
`void:inDataset` link is one triple whose consumer is the VoID selectivity actor in
`../kgf-sparql`, which will fetch `/void` at first contact instead of taking statistics
from the caller; the other predicates are what Comunica's `primary-topic` actor and
`hydra-*` extractors read.

The `hydra:itemsPerPage` triple is new. Comunica divides its measured request time by it
to price per-item I/O; without it a KGF source is priced per request.

### `/fragment` afterwards

- `Pattern::parse_with` loses `tpf_terms`; `BoundTerm::parse_fragment` is deleted; every
  representation of `/fragment` parses doc 03 §3.3 and nothing else.
- GET `/fragment` no longer accepts `?name` variables or `values=`
  (`BindingFragment::parse_variable_get` and the GET `values` branch move to `/tpf`).
  Native bindings are body-carried; that is now the whole story.
- The RDF representations of `/fragment` are **data only**: the selected triples,
  serialized in the same four formats, with no Hydra vocabulary. Completeness and the
  continuation are in the `kgf-*` headers, as for JSON. Hypermedia is the TPF contract
  and lives on the TPF route.
- `Representation::FRAGMENT` keeps JSON first.

Recorded alternative, not taken: keep a Hydra form on `/fragment` whose template points
at `/tpf`, so that a source configured as `brtpf@…/fragment` migrates by itself
(Comunica follows the form's template). Ten lines, and worth remembering if the
`/fragment` URLs are ever handed to TPF clients again; today nobody depends on them.

### Everything else the route touches

- **Descriptor and links.** `release_links` gains `tpf`; the service and dataset
  descriptors list it with parameters `subject, predicate, object, values, limit,
  cursor`; the HTML workbench links to it beside `fragment`.
- **Cursors.** A new `Operation::Tpf` in the canonical request, so a `/tpf` cursor is
  refused on `/fragment` and vice versa even though both lower to the same selection.
- **Admission and access log.** `AccessOperation::Tpf`, admitted as heavy work exactly
  as RDF fragments are today; the observation records the route so the census can tell
  TPF traffic from native traffic.
- **Caching.** ETags already carry the representation token; the two new tokens are
  header-safe. `Vary` is unchanged.
- **Manifest.** Nothing. No capability, no prefix-map involvement.
- **Manifest validation (native side, independent of the route).** Refuse a prefix whose
  name is a registered URI scheme in use in RDF data (`http`, `https`, `urn`, `mailto`,
  `doi`, `tag`, `data`, `file`, `ftp`), so a native CURIE can never be mistaken for an
  IRI even in a body from a confused client.

## Sequence

Each step is a mergeable unit with its own tests; none needs a fixture change.

1. **The `ExplicitRepresentation` parser** — `request.rs` (or a `tpf` module beside
   `term.rs`): `TpfTerm::parse(text) -> Result<BoundTerm, Problem>`. Pure. Tests: the
   spec's own examples (`http://example.org/bar`, `"my text"`, `"my text"@en-gb`,
   `"42"^^http://www.w3.org/2001/XMLSchema#integer`); `?s` and empty as variables;
   `urn:uuid:…`, `doi:10.1000/x`, and a `urn:fdc:…` skolem IRI as IRIs; `<http://…>`,
   `rdfs:label`, `"a"^^xsd:date`, and an unclosed quote as 400s with the route-specific
   hint; `"a"^^http://www.w3.org/2001/XMLSchema#string` folding to plain; the term cap.
2. **Quad formats** — `rdf.rs` gains `DatasetFormat::{NQuads, TriG, JsonLd}` and
   `serialize_dataset`; `representation.rs` gains the two variants; the round-trip test
   parses every format back through `oxrdfio::RdfParser` including the named graph.
3. **The document** — a `tpf_dataset()` in `answer.rs` that builds the two graphs from
   the same rows and echo the current `fragment_rdf_prefix` uses, with the byte-fitting
   generalized to datasets. Tests in `crates/kgf/tests/serve.rs`: every format parses
   with an independent parser; the metadata graph carries `foaf:primaryTopic`,
   `void:subset` with the *exact* request URL including `cursor` and `values`,
   exactly three mappings, `ExplicitRepresentation`, `hydra:itemsPerPage`; the data
   graph contains no hydra or void triple; `--public-base` drives every IRI.
4. **The route** — `routes.rs` `tpf` handler over a `request::Tpf` type
   (`Plain | Values`), `Operation::Tpf`, `AccessOperation::Tpf`, descriptor links.
   Tests: parameters accepted and refused (`s`, `page`, `o.text` are 400s naming the
   route); a `values` table with only undeclared variables is a 400; cursor rejection
   across routes; `format=` tokens.
5. **Strip `/fragment`** — delete `parse_fragment` and the `tpf_terms`/
   `rdf_representation` threading; GET refuses `?name` and `values=`; RDF becomes data
   only. Tests: a bare IRI is a 400 under every `Accept`; RDF from `/fragment` carries
   no hydra triple; the existing brTPF integration tests move to `/tpf` unchanged in
   substance.
6. **Conformance** — `interop/comunica/test.mjs` targets `…/tpf` and adds: a bound
   typed-literal object (the sockg shape, `?s <p> "1998-04-20"^^xsd:date`); a bound
   `urn:fdc:…` skolem subject reached through a bind join; `SELECT (COUNT(*) AS ?n)
   WHERE { ?s ?p ?o }` equal to the fixture's triple count under Comunica's default
   `Accept` (no control leak, which also proves the primary-topic split); and the
   negotiated content type observed to be N-Quads. The ignored Rust test that drives
   it passes both `/tpf` URLs.
7. **External gate** — rerun the `../kgf-sparql` corpus harness with the endpoints
   swapped to `/tpf` (`TASK_WORKERS=3 python3 test/corpus-arms.py … brtpf all`), and
   compare with the recorded public-deployment numbers: 42 of 60 tasks under the
   combined profile, 0.43× requests at the median, the three sockg crashes gone, and
   `phaseskg`'s `?s ?p ?o` returning 2,750 rows. This is the acceptance test for the
   whole unit.
8. **Documents** — unit 24 in `plan.md` with a What-landed section; the header of
   `comunica-brtpf.md` marked superseded; the outbound spec edits below filed under
   Questions for `../kgf`; `state.md`'s handoff updated.

`cargo fmt --check`, clippy with denied warnings, the workspace tests, and the Comunica
suite gate every step, as for unit 20.

## Spec edits this needs in `../kgf` (filed under Questions)

- **Doc 03 §3.2** adds `/{dataset}/v/{version}/tpf` to the URL tree.
- **Doc 03 §3.4.1** loses the sentences that make `/fragment`'s RDF the TPF surface
  (the data rule paragraph moves with it); a new **§3.4.x `GET /tpf`** is normative for
  the grammar, the document, and the brTPF `values=` transport; **§3.8**'s TPF entry
  becomes a pointer to it.
- **Doc 03 §3.3** stays as written and gains one sentence: bare IRIs are the TPF route's
  grammar, and that route is where a TPF client belongs.
- **Doc 03 §3.5** cost table: `/tpf` has `/fragment`'s cost with the RDF-fitting note.
- **Doc 06 §6.4** (the Comunica actor): sources are typed `brtpf` on `…/tpf`; the
  quad-format and `void:inDataset` discovery story is written there, not in the source
  actor.

## Not in this plan

- The four-position QPF form (graphs) — waits for doc 03 §3.7, on this route when it
  comes.
- `page=`, `hydra:previous`, `hydra:first` — Comunica does not use them and the TPF spec
  does not require them.
- `BasicRepresentation` — a client that sends unquoted literals gets a 400; declaring
  `ExplicitRepresentation` on the form is the answer.
- Bindings over a body on `/tpf`, or TPF hypermedia on `/fragment`.
- Comunica-side work that this route exposes but does not fix: 4xx surfacing as
  unhandled rejections, no retry on network-level failures, unbounded request
  concurrency. Those are tracked in the `../kgf-sparql` report.
