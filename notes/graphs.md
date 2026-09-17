# Named graphs: the read contract for the `graphs` capability

Status: decided 2026-09-15, implemented 2026-09-17. `g` answers its four forms on
`/fragment` and `/count`, `/graphs` lists them, `/tpf` serves the table below,
`kgf build` assembles a bundle that carries memberships, and each graph has a
description view of its own. This note is the contract that implementation meets;
where the two disagree, that is a bug in one of them. The rationale is a comparison
with what union-default triplestores do; the short version is that KGF does what
QLever, RDF4J/GraphDB, and Blazegraph do, and nothing else.

## The model

- `data.hdt` is the deduplicated union of every graph. The sidecar records memberships:
  `(graph id, SPO position)` pairs, set semantics, and a triple may be in several graphs.
- hdtc's layer 0 holds the statements that carried no graph in the source. KGF calls
  this the **unnamed graph**. It is never called the default graph.
- **The default graph is the union.** A request with no `g` reads it. This is a
  semantic choice, stated as KGF's own; it matches union-default stores and not a
  spec-strict SPARQL store.

## Two reserved IRIs, fixed federation-wide

| Constant | Names |
|---|---|
| `urn:x-kgf:union` | the union; `g=<urn:x-kgf:union>` is identical to omitting `g` |
| `urn:x-kgf:unnamed` | the unnamed graph, layer 0 |

`kgf build` refuses a source quad, or a component graph, whose IRI is either
constant. It refuses it by *running the runtime check*: the sidecar is built, opened
the way a server opens it, and a dictionary holding either constant fails there —
before the text index, the sketches, the key sets and the description set, but after
the HDT. Refusing at parse time would be cheaper and needs hdtc to know these names,
which is a question for `../hdtc` rather than a thing to duplicate here. In the s, p,
and o positions the two are ordinary IRIs that match nothing. Both are accepted on
every release, sidecar or not: on a bundle with no memberships each selects every
triple, since every triple of such a bundle is unnamed.

## The four forms of `g` on `/fragment` and `/count`

| `g` | Selection | Count |
|---|---|---|
| absent | union, one row per distinct triple | N |
| `<G>` | triples in G, set semantics | rank difference in G's layer |
| `<urn:x-kgf:unnamed>` | layer 0 | rank difference in layer 0 |
| `*` | quad view: one row per membership, with a `g` column | Σ over layers |

`g=<G>` and `g=*` need the capability and stay 501 without it. In the quad view every
row has a `g`; layer-0 rows carry `urn:x-kgf:unnamed`, and `urn:x-kgf:union` never
appears as a value. `GET /graphs` lists every named graph with its count, and the
unnamed graph under its constant when layer 0 is non-empty. Cursors carry the scope.

Five decisions the implementation had to make, recorded here because each is a
promise to a client rather than an internal choice:

- **`g` beside `o.text` is refused**, 400 rather than 501. A ranked text page is
  assembled from one selection per matching literal and this build scopes none of
  them, so the request is well-formed and unanswerable rather than unimplemented.
  Every other ignored filter is refused the same way.
- **`/graphs` enumerates by layer id**: the unnamed graph first when it holds a
  triple, then the named graphs in the sidecar's dictionary order, and the cursor is
  the next id to list. That order is a contract, as every enumeration order here is:
  it is what an outstanding token indexes.
- **A graph whose stored name is a blank node** is published under the same
  bundle-scoped IRI a blank node in the data gets, in a section of its own keyed by
  layer id. A request may send that IRI back, and the layer id in it is checked
  against the sidecar rather than trusted: an id out of range, or one whose layer
  carries an ordinary IRI, names no graph. `_:label` itself addresses nothing, here
  as in every other position. Such a graph has no *description* view: see below.
- **The RDF representations of `/fragment` follow the serving table below**, not just
  `/tpf`'s: a scope tags every statement with the graph it named, the quad view tags
  per row, and a single-graph syntax refuses the quad view with 406.
- **A TPF `graph` variable may not repeat a pattern variable.** `graph=?s` beside
  `subject=?s` asks for each statement's graph to equal one of its own terms, which is
  a join this build does not do; answering the unjoined quad view would be a superset,
  and silently wrong for a client that does not filter it again. Refused with 400,
  like the repeated variables `/tpf`'s plain form already refuses.

Worked example. Source, five statements:

```
:a :b :c .          :a :b :c :g1        :a :b :d .          :x :y :z :g1        :x :y :z :g2
```

Three distinct triples. Memberships: `:a :b :c` in {unnamed, g1}; `:a :b :d` in
{unnamed}; `:x :y :z` in {g1, g2}. Counts: absent 3; `<g1>` 2; `<g2>` 1;
`<urn:x-kgf:unnamed>` 2; `*` 5.

## What SPARQL clients derive from this

Normative for the Comunica source in `../kgf-sparql` and for any restricted SPARQL
profile:

| Pattern graph | Request | Result |
|---|---|---|
| none | no `g` | the union |
| `GRAPH <G>` | `g=<G>`, constants included | G |
| `GRAPH ?g` | `g=*`, then drop rows whose `g` is `urn:x-kgf:unnamed` | named-graph memberships only |

`GRAPH ?g` never lists the unnamed graph. That is what QLever, RDF4J, and Blazegraph do
by default; each reaches its bucket through its own reserved IRI. On the example,
`GRAPH ?g { ?s ?p ?o }` has 3 solutions.

## The TPF route

A bundle with the capability publishes the **four-mapping** Hydra form, adding
`hydra:property sd:graph` with variable `graph`, and declares in the page metadata:

```
<D> sd:defaultDataset [ sd:defaultGraph <urn:x-kgf:union> ] .
```

The blank-node subject is load-bearing: Comunica's metadata extractor reads
`sd:defaultGraph` only from the page URL it fetched, from a resource declared earlier
in the stream as that page's `void:subset` superset, or from a blank node. `<D>` is
none of those (its subset is `<F>`, not the page), so `<D> sd:defaultGraph …` is
silently ignored and Comunica treats the default graph as empty, sending no request
at all for bare patterns. The blank-node shape works in any quad order. That
declaration is what makes stock Comunica request the union for a bare pattern (its
QPF source sends `graph=<the declared IRI>` for a default-graph pattern). Serving
rule, in N-Quads/TriG/JSON-LD:

| `graph=` | Data quads | `hydra:totalItems` |
|---|---|---|
| absent | quad view; layer-0 memberships **untagged** (document default graph), others tagged with their graph | memberships |
| `<urn:x-kgf:union>` | each triple once, **untagged** | distinct triples |
| `<G>` | G's triples, tagged G | count in G |
| `<urn:x-kgf:unnamed>` | layer 0, tagged `urn:x-kgf:unnamed` | count in layer 0 |
| `?g` (brTPF row variable) | as absent | as absent |

The union constant never appears as a tag in any response. An earlier draft of this
table tagged union rows with the constant so that the identical
`graph=urn:x-kgf:union` request Comunica sends for a bare pattern and for an explicit
`GRAPH <urn:x-kgf:union>` would satisfy both (measured on the s2-qpf implementation,
whose fixture fit one page). Implementation (2026-09-17) showed that only the first
page of a fragment goes through Comunica 5.3.0's `QuerySourceQpf`, whose filter
accepts a default-graph pattern against rows tagged with the declared
`sd:defaultGraph` *or* left in the document's default graph; every later page is
identified as a plain RDF document and matched by `QuerySourceRdfJs` against the
pattern's literal graph term, which knows nothing of the declaration. Tagged union
rows therefore vanished from the second page on, and a bare `?s ?p ?o` returned one
row of three. Untagged union rows page to the end, and the bare pattern — the query
every client sends — is the one that must work. The cost is that stock Comunica reads
nothing through `GRAPH <urn:x-kgf:union>`, on the first page or any other; that idiom
belongs to KGF's own API and to a KGF-aware SPARQL source, where the union is the
default graph. Traced through the same code and verified against a one-row-page
listener: a bare pattern gets every union row; `GRAPH ?g` requests with `graph` absent
(`qpf`) or `graph=?g` (`brtpf`) and Comunica's own filter discards the untagged
quads, so it sees named graphs only; `GRAPH <G>` and `GRAPH <urn:x-kgf:unnamed>` pass
straight through. No context flag is involved. A bundle without the capability keeps
the three-mapping form. Turtle, being single-graph, can only serve the union and
scoped views; the quad view is refused in it.

## Each graph's own description

A bundle with memberships is described one graph at a time as well as whole. The
analysis describes the dataset as the union plus one `void:subset` per graph, and
the build projects each subset into a description view named `graph:<IRI>` — the
unnamed graph under `urn:x-kgf:unnamed`, the same name `g=` and `/graphs` use, so a
client holding a graph's name holds its description without a second vocabulary to
map between. `GET /schema?view=graph:<IRI>` reads one, `stats/summary.json` lists
every graph with its own counts and the links into both, and a graph the bundle does
not describe is a 404 naming `/graphs`.

A graph is a view rather than a second kind of description because it is on the axis
components are on: both name a subset of the published triples, and the analysis
already expresses both as `void:subset`. The counts are each graph's own, so they sum
to more than the dataset's whenever a triple is in two graphs — which is the point of
publishing them separately.

**Not every graph gets one.** A graph whose stored name is a blank node has no IRI to
name a view after, and the analysis gives it a bare subset with no service description
saying which graph it is — which makes it indistinguishable from the unnamed graph's
own subset. Where such a graph exists, neither of those two is described, rather than
one being described under the other's name; the graphs with IRIs for names are
unaffected. So `/graphs` can list more graphs than the summary describes, and
`/schema?view=graph:<IRI>` answers 404 for one of them. Every graph is still complete
in the memberships and reachable by `g=`. Linking a blank-named graph in the dataset
view would close this, and that is a question for `../hdtc`.

## Store operations this needs

- `graph_id(term)` and `graph(id)` over the sidecar dictionary; the two constants are
  resolved before the dictionary is consulted.
- Scoped enumeration for all eight patterns in the pattern's native position space
  (SPO from the sidecar, POS and OPS from the index), `next_member`/`select` driven,
  O(1) per row; scoped counts as two ranks.
- `graphs_of(position)` for the `g` column, from the transpose when built and from
  probing each layer otherwise.
- Quad-view counts as the sum of per-layer rank differences.
- Build: refuse the two constants as graph names; assign nothing to layer 0 that the
  source did not leave bare.

## The tests this contract is held to

Written, and kept: the worked example as a fixture with every count in both tables
above, on every representation; `GET /graphs` listing g1, g2 and the unnamed graph
with counts 2, 1, 2; the TPF route's tagging per row of the serving table, with
`sd:defaultGraph` present in the four-mapping form and absent in the three-mapping
one; every form of `graph` paging to the same rows one row at a time through
`hydra:next`, quad view included, which is the case the run trailer exists for; a
forged run trailer refused as a stale cursor rather than skipping the rest of a
triple — including on the last triple of an enumeration, where the rows run out
before anything is checked; a blank-node graph fixture whose minted IRI round-trips
while neither an out-of-range id nor the id of an IRI-named layer resolves through
it; the build refusing a quad in graph `urn:x-kgf:union`; and, in
`interop/comunica/test.mjs`, stock Comunica as both a `qpf` and a `brtpf` source with
no `unionDefaultGraph` context, against one-row pages, reading 3 rows for a bare
`SELECT * { ?s ?p ?o }`, 3 for `GRAPH ?g`, 2 for `GRAPH <urn:x-kgf:unnamed>`, 2 for
`GRAPH <g1>`, and completing a bind join through the bindings-restricted path.

Still to write: the metadata graph parses to a `sd:defaultGraph` triple whose subject
is a blank node, and Comunica's own extractor, run over the page, reports
`defaultGraph` — the conformance script exercises the consequence rather than the
declaration.
