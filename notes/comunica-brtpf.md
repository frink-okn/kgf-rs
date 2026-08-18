# Serving Comunica as a bindings-restricted TPF/QPF source

Status: implemented and externally verified, revised 2026-08-18. This records what Comunica
5.3.0 requires of a brTPF source and the resulting design decision: **TPF and
brTPF are first-class representations of the one `/fragment` operation**, not a
compatibility route beside it. KGF is a bounded, extended fragment protocol;
stock Comunica supplies complete SPARQL evaluation and federation over one or
more KGF endpoints. QPF remains the graph-aware extension of the same route,
but its four-position form waits for the graph read semantics reserved by doc
03 §3.7.

This touches ../kgf doc 03 §3.1 (core profile and formats), §3.2 (URL space),
§3.4.1–2 (`/fragment`), §3.5 (the cost table), and doc 20 §20.2/§20.8
(enumeration order; one implementation per operation). The design documents
must be revised with the implementation; until that sibling change lands, the
disagreements are recorded under `notes/plan.md`'s Questions for `../kgf`.

## The question

Comunica is the modular JavaScript SPARQL engine used across the Linked Data
Fragments world. If it can consume our `/fragment` as a bindings-restricted
Triple Pattern Fragments (brTPF) source, a stock Comunica engine federates over
a KGF bundle with bind joins, and we get a real, external client for the
bindings-restricted operation we already implement — at the price of one output
representation, not any new query machinery.

The answer is yes, with one important refinement: the ordinary selection
machinery is already ours, but brTPF's generalized binding table and RDF-set
projection are a real protocol layer rather than merely another serializer.
They still lower to the same store selections; they do not justify another
endpoint or another read implementation.

## What Comunica 5.3.0 actually does

brTPF is a first-class, default-wired capability in 5.3.0 (verified against tag
`v5.3.0`), not a legacy branch. Two pieces:

- **Source** — `@comunica/actor-query-source-identify-hypermedia-qpf`
  (`QuerySourceQpf`). Its README states it handles "Triple Pattern Fragments,
  Quad Pattern Fragments, and bindings-restricted Triple Pattern Fragments
  interfaces." When the source is brTPF, its selector shape advertises
  `filterBindings: true` — "you may push a bag of bindings into me."
- **Join** — `@comunica/actor-rdf-join-inner-multi-smallest-filter-bindings`
  (`ActorRdfJoinMultiSmallestFilterBindings`), present in the default
  `config-query-sparql` join config. It picks the smallest entry and, if the
  other side's shape allows `filterBindings`, streams the intermediate bindings
  into it — the bind join.

### The wire contract Comunica emits

Per request, against a source typed brTPF, Comunica:

1. Resolves the pattern through the **ordinary** TPF/QPF Hydra search form.
   `getSearchForm` requires the form to have *exactly* three mappings (s/p/o) or
   four (s/p/o/g) — it rejects any other arity. So the form must be the plain
   TPF form; we must **not** add a `values` mapping to it, or detection breaks.
2. Appends `&values=<url-encoded>` to that URL. The payload is SPARQL `VALUES`
   syntax **without** the `VALUES` keyword:

   ```
   (?v1 ?v2) { (<t1> <t2>) (<t3> UNDEF) ... }
   ```

   Terms in Turtle spelling; `UNDEF` permitted. An empty bag is sent as a single
   poison binding `(<ex:comunica:unknown>)` because the reference brTPF server
   errors on zero bindings. The append is **hardcoded** in `QuerySourceQpf`
   (`getBindingsRestrictedLink`) — the source comment notes brTPF exposes no URL
   template for the bindings, so Comunica does not read one.
3. Dereferences the result as **RDF** (Turtle / TriG / JSON-LD) and runs Hydra
   metadata extraction. The page must therefore carry: the matched triples/quads,
   a cardinality (`hydra:totalItems` / `void:triples`) used for join planning,
   paging controls (`hydra:next` / `hydra:previous`), and the `hydra:search`
   form from (1).

### brTPF is not negotiated — it is declared

The bindings-restricted flag comes **only** from the caller typing the source:
`ActorQuerySourceIdentifyHypermediaQpf` sets it from
`action.forceSourceType === 'brtpf'`. There is no hypermedia or metadata channel
by which a server announces "I am brTPF." So interop is out-of-band: the operator
configures Comunica with `sources: [{ type: 'brtpf', value: '…' }]`. Our manifest
can *document* the capability but Comunica will not discover it.

## How many bindings per request — and who decides

The batch size is `blockSize` on the join actor: "The maximum amount of bindings
to send to the source per block," **default 64**. Comunica slices the driving
stream into `blockSize` chunks (`ChunkedIterator(..., this.blockSize)`) and fires
one brTPF request per chunk.

This is a **client-side** knob. There is no channel for the server to advertise a
per-request bindings cap; consistent with the declared-not-negotiated point
above, the `&values=` append is hardcoded and the batch size is pure client
config. We cannot push the number to Comunica — only whoever runs Comunica can,
by overriding `blockSize` in their engine config.

Design consequences, and they cut *toward* the bounded-cost thesis (doc 03 §3.5):

- Because the client will not honour a server-declared limit, our brTPF
  `/fragment` **must enforce its own maximum** on the `values` bag and reject
  (4xx) anything over it. A bindings-restricted request is bounded only if *we*
  bound the bindings count.
- **Our default cap is 1000**, comfortably above Comunica's default 64. So a
  stock, unconfigured Comunica engine federates against us without touching
  `blockSize` — every default 64-binding block is well under our limit. (The bad
  failure mode we avoid: a cap below 64 would reject every unconfigured client
  until they hand-edit their config, with no negotiation to warn them.)
- Publish the cap (manifest + doc 03) as documentation even though Comunica
  cannot read it: it is what tells an operator what `blockSize` may safely be
  raised to.

## What this means for KGF

The fundamental operation is the compatibility relation between an input
binding row and a matching triple. KGF's JSON representation exposes that
relation directly with its `binding` column. brTPF projects it to the distinct
RDF triples compatible with at least one row, because RDF graph output cannot
carry an input-row index. Both projections use the same dictionary resolution,
`Selection`s, enumeration order, and positional cursor machinery, but their
visible cardinalities can differ when binding rows overlap.

The RDF cardinality follows that projection without introducing an unbounded counting
scan. It is exact when restrictions are pairwise disjoint or one restriction subsumes
all others. For arbitrary partial overlap it is a planning estimate bounded above by
both the per-binding relation sum and the count of the base triple pattern containing
every restriction. Comunica already consumes TPF cardinality as planning metadata;
result correctness comes from following Hydra pages to exhaustion.

The original native body was narrower than Comunica's input: it required every
cell to be a term and every binding variable to occur in the pattern. The shared
domain model now admits `UNDEF`, retains foreign columns for input-row identity
while ignoring them during pattern restriction, and normalizes rows onto the
pattern positions before execution.

| | original `kgf-server` | implemented shared model |
|---|---|---|
| Bindings ingress | SPARQL `QUERY` method / POST body | `&values=` query param, VALUES-minus-keyword text |
| Binding cells | terms only; columns must occur in pattern | terms or `UNDEF`; unrelated columns permitted |
| Response body | KGF JSON envelope (+ HTML) | Hydra-annotated RDF page (distinct triples + estimate/count + paging + search form) |
| Capability discovery | manifest | none — client is still told `brtpf` out of band |

The wire spellings normalize into one typed restricted-pattern request. RDF is
built as `oxrdf` triples/quads and serialized by `oxrdfio`; KGF owns the graph it
publishes, while the library owns escaping and syntax for Turtle, N-Triples,
JSON-LD, and eventually TriG/N-Quads.

### Target TPF/brTPF now; QPF on the same route later

Serve the three-mapping (`s/p/o`) form over today's triple read layer. A
four-mapping QPF form is not cost-free in Comunica: absent an `sd:defaultGraph`
declaration (or the caller's `unionDefaultGraph` option), Comunica deliberately
treats a QPF endpoint's default graph as empty. KGF additionally distinguishes
its unscoped union triple view from the stored default graph (doc 03 §3.7).

When graph reads land, the same `/fragment` page can advertise a four-mapping
form and an explicit default-graph mapping consistent with those semantics.
Immutable releases may advertise the form they can answer; the operation URL
and its native KGF representations do not change.

### Endpoint shape: one `/fragment` resource

`GET /fragment` is already the natural TPF discovery root: with no bound
positions it is the first page of the all-triples fragment, and its RDF
representation can carry the `void:Dataset` and three-mapping Hydra search form.
The form maps RDF subject/predicate/object properties to KGF's existing `s/p/o`
parameters; TPF does not prescribe their query-string names.

`values=` selects the brTPF request grammar. `Accept` continues to do only its
HTTP job: choose JSON, RDF, or HTML for the selected fragment. A Hydra `next`
link is the RDF spelling of the existing opaque cursor and points back to the
same versioned `/fragment` URL. No `/tpf` or `/qpf` alias is introduced, and TPF
support is a server/core-profile commitment rather than a bundle artifact
capability.

## Implemented sequence

1. Added a shared `oxrdfio` serializer in `kgf-server`; migrated `/void` and proved
   every emitted format by parsing it back to the expected RDF dataset.
2. Added Hydra-annotated Turtle/JSON-LD representations to plain GET
   `/fragment`, including the dataset, search form, cardinality, and next link.
3. Pinned Comunica 5.3.0 in a separate conformance harness and made a single
   pattern query pass against a real fixture listener.
4. Parsed `values=` with a real SPARQL parser into generalized binding cells,
   normalize both GET and QUERY/POST spellings into one restricted-pattern
   domain model, and preserve the existing store selection implementation.
5. Implemented the distinct-triple RDF projection with bounded overlap handling,
   truthful cardinality metadata, and positional resumption.
6. Extended the gates through bind joins, `UNDEF`, overlapping rows, paging,
   term edge cases, and federation over two KGF endpoints. The stock-client harness
   covers paging, bind join, and federation; focused Rust HTTP tests cover the input
   and projection edge cases that Comunica does not deterministically generate.

## Questions for `../kgf`

1. **TPF/brTPF belongs to the core profile, not §3.8 compatibility.** Remove the
   separate `/tpf` route and specify the Hydra RDF representation and `values=`
   spelling on `/fragment` itself. The one-implementation rule governs the
   typed operation and store selections, not the number of accepted transports.
2. **Publishing the bindings cap.** doc 03 §3.5 should name the per-request
   bindings maximum (our default 1000) as a published cap, with a note that no
   brTPF client discovers it — it is a documentation/operator obligation, and it
   should stay ≥ 64 so stock Comunica works unconfigured.
3. **Generalized binding semantics.** Define `UNDEF`, unrelated binding
   variables, the RDF-set projection, overlap/cardinality behavior, and how
   budget interruption resumes without silently losing compatible triples.
4. **Quad enumeration order and cursors (doc 20 §20.2).** Serving QPF later commits us
   to eventually generalize the eight-pattern order and cursor tokens to the graph
   position when the quad read layer arrives. The single-default-graph case must
   remain a stable specialization of whatever multi-graph order §20.2 grows, so
   the four-position wire shipped now never has to change under it.

## References

- Comunica QPF source: `packages/actor-query-source-identify-hypermedia-qpf/lib/QuerySourceQpf.ts`
  and its README (`getSearchForm` arity rule; `getBindingsRestrictedLink` VALUES
  append). https://github.com/comunica/comunica/tree/master/packages/actor-query-source-identify-hypermedia-qpf
- Comunica brTPF bind join: `packages/actor-rdf-join-inner-multi-smallest-filter-bindings`
  (README: `blockSize`, default 64). https://github.com/comunica/comunica/tree/master/packages/actor-rdf-join-inner-multi-smallest-filter-bindings
- brTPF, Hartig & Buil-Aranda 2016 (ODBASE): https://arxiv.org/abs/1608.08148
