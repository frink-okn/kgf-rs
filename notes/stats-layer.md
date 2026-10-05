# The description surface: VoID, schema, class relations, summary, namespaces

Status: the design record from before the work, revised 2026-08-07 and now
superseded by what landed. Everything planned here is built — the shared
indexed-HDT core, the mapped artifacts, the selector and relation indexes,
/schema, /void, /summary, and the kgf build stats producer — and details moved on
the way: the artifact set is eight files including stats/class-properties.tsv,
the count-ranked predicate inventory per class, which this note does not name,
and the full proof runs from kgf manifest rather than a separate kgf verify.
notes/plan.md unit 19's *What landed* is the current account; where it and this
note disagree, it and the code win. Kept for the reasoning that chose this shape
— why VoID is bounded by schema rather than data size, why nothing is
materialized at open, and why a semantic selector index exists at all. This
tracks ../kgf docs 03 §3.4.10, 04 §4.1–4.3, 07 §7.5 item 27, and 20 §20.3–20.4.
Those documents are authoritative when this note and the protocol disagree.

The read core (fragment, count, describe, sample, search, labels) is live. The
missing core-profile work is the surface that lets a client understand a KG
before it knows which queries to form.

## Contract to implement

A tier-1 bundle carries these required description artifacts:

- stats/void.hdt and stats/void.hdt.perm: the layered VoID graph and the
  ordinary hdtc POS/OPS sidecar over it.
- stats/schema-nodes.tsv: the selector-sorted mapping from semantic schema
  paths to subject IDs in the final VoID HDT.
- stats/class-relations.tsv: the count-ranked observational
  ⟨subject class, predicate, object class, triples⟩ projection.
- stats/namespaces.json: per-role counts against the curated federation prefix
  table.
- stats/summary.json and stats/summary.md: persisted summary cards.

They back /void, /schema, and /summary. There is no stored void.ttl; Turtle and
JSON-LD are derived representations of void.hdt.

The existing hand-built bundles have a flat void.hdt only. It is absent from
manifest.json; there is no VoID .perm, schema-nodes TSV, class-relations TSV,
namespace inventory, summary card, or kgf build. During bring-up, a missing input
returns the existing structured “not built” response. Once the producer lands,
serving the core profile refuses an incomplete description set at open rather
than keeping a permanent fallback.

## Facts that drive the shape

- VoID is bounded by schema size rather than data size. The largest measured OKN
  result is Ubergraph at about 478 KiB of HDT (versus 2.5 GiB of data and about
  7 MiB of Turtle); babel is about 2.7 KiB over 10.6 GiB of data.
- “Small once” is not “free per open version.” Dozens of KGs and retained
  historical versions make eager class/property/adjacency maps the wrong memory
  model. The VoID HDT and .perm remain mapped and lazy.
- .perm is cheap here and gives one read implementation for SPO/POS/OPS, but an
  index alone does not resolve a semantic selector such as class=C&predicate=P
  to its nested partition node without a scan. A small sorted selector index
  records the subject ID in the final VoID HDT. It remains mapped, not
  materialized, and creates no dependency on hdtc's current MD5 names.
- VoID is weak for mapping/annotation graphs. Babel has two predicates and no
  classes, so the namespace inventory is a co-equal content signal rather than
  an optional embellishment.

## Store-side architecture

### 1. Extract the reusable indexed-HDT core

Permutations already owns almost all of the right boundary: one mapped HDT, its
mapped .perm, the parsed dictionary, and SPO/POS/OPS. Refactor/rename that
boundary as an internal IndexedHdt (a thin wrapper is acceptable if a rename
would make the first change noisy):

~~~text
IndexedHdt
  HDT mapping + HdtLayout
  .perm mapping + validated POS/OPS/rank directories
  dictionary locate/extract
  resolve/count/iterate one triple pattern

Store
  data: IndexedHdt
  data-specific sidecars and operations
  description: DescriptionStore

DescriptionStore
  void: IndexedHdt
  schema_nodes: mapped selector → subject-ID TSV
  class_relations: mapped TSV
  view directory: TSV byte ranges from the manifest
  namespaces/summary artifact handles
~~~

The main and description graphs use the same parsing, binding validation,
rank/select, pattern selection, and cursor primitives. DescriptionStore is not
a second bundle and does not inherit data sidecars. No class map, property map,
tree adjacency, or deserialized VoID model is built at open. The small view
directory already present in the manifest is the only resident navigation
metadata.

Keep Store::dict, Store::perms (if still public), Store::resolve, and
Store::triples as delegations so this refactor does not widen the HTTP change.
Add headless description methods only after the core is shared; routes must not
open-code RDF patterns.

### 2. Map and validate the selector index

stats/schema-nodes.tsv has this header:

~~~text
view\tkind\tclass\tpredicate\tdatatype\tsubject_id
~~~

It has one row for every directly selectable node: dataset root, class,
dataset- or class-scoped property, and datatype. Empty semantic columns are part
of the key. subject_id is an unsigned decimal, one-based subject dictionary ID
in the final stats/void.hdt.

Use fixed block order design, queryable, then component:<id> by component-id
bytes. Within a view, sort by the byte tuple
(kind, class, predicate, datatype), reject duplicates, and record offset, bytes,
rows, and maximum row length in typed manifest metadata. The artifact entry also
records parents: ["stats/void.hdt"]. Extend the mapped-file layer with a
row-boundary-aware binary search inside a declared view range. Its work is
O(log rows · max_row_bytes), with both quantities published. Open checks only
ranges against file length; it neither scans the file nor allocates row offsets.

kgf manifest --verify / kgf verify performs the full proof: UTF-8, six fields,
allowed kinds and empty-field rules, ordering, uniqueness, view ranges, row
boundaries, and maximum row length. It then uses the indexed VoID reader to
check that every subject ID exists, states the requested semantic term, and is
joined to the correct parent path. The file is a recoverable index bound to
stats/void.hdt and covered by content_digest.

At request time, construct the semantic tuple, binary-search its view block,
parse and range-check the returned subject ID, and issue the normal indexed
subject-rooted patterns directly. Child collections still follow VoID partition
edges. No generated node name is stored, derived, or interpreted.

Partition terms never appear as selectors. Links contain semantic
class/predicate/datatype parameters. A producer may replace every partition IRI
or switch to blank nodes in a rebuild; kgf build simply traverses the final
VoID again and emits its new subject IDs. This is neither a KGF wire break nor a
kgf-store code change.

### 3. Map and validate class-relations TSV

The file is UTF-8 TSV with one header:

~~~text
view\tsubject_class\tpredicate\tobject_class\ttriples
~~~

Use the same fixed view-block order. Each view is one contiguous block, sorted by
triples descending and then by expanded subject/predicate/object IRI bytes
ascending. Extend the manifest's typed artifact metadata so each block has
offset, bytes, and rows, with a global max_row_bytes and
parents: ["stats/void.hdt"].

kgf manifest --verify / kgf verify performs the full scan: header, UTF-8, five
fields, view membership, unsigned counts, ordering, non-overlapping row-boundary
ranges, and exact coverage. Open performs bounded structural checks against file
length and maps the bytes; it does not rescan rows.

Unfiltered paging starts at the recorded view offset and stops after limit rows.
Filtered paging preserves the global order while checking class and/or
predicate, spending candidate_budget and the time/response budgets. Its cursor
is the next TSV row boundary plus the ordinary content-digest and
canonical-request binding.

## Store API before HTTP

Implement and test a representation-neutral API roughly along these lines:

- description.node(view, selectors) → optional SchemaNode
- description.children(view, selectors, collection, page) → SchemaPage
- description.class_relations(view, filters, page, budgets) → RelationPage
- description.void_patterns() or an iterator suitable for RDF serialization
- handles/readers for the persisted namespace inventory and summary cards

The default node projection selects one node and at most one immediate child
collection:

- root → classes or properties
- class → properties
- property (root or class-scoped) → object-classes or datatypes
- datatype → languages

limit defaults to 100 and is capped by max_schema_items (suggested 1,000). There
is no recursive expansion inside a child item. Add fixture tests for every
selector/children combination, absent terms, every view, cursor resumption, and
the promise that opening N bundle versions does not allocate per-VoID-node
state.

## HTTP work

Mount the three routes under /{dataset}/v/{version}:

- /schema: parse class, predicate, datatype, children, projection, view, limit,
  and cursor; run store calls on the blocking pool; emit the fixed JSON
  envelopes and uniform completeness fields. Implement
  projection=class-relations from the TSV, not by walking/sorting VoID.
- /void: traverse void.hdt and stream an RDF representation. hdtc currently
  supplies HDT traversal and an N-Triples dump; it does not already supply
  KGF's Turtle and JSON-LD serializers. Choose/test those serializers in
  kgf-server (or add a deliberately shared hdtc façade) rather than claiming
  reuse that does not exist. Enforce max_response_bytes; compact bulk access
  remains /export/void.hdt.
- /summary: serve the persisted JSON/Markdown bytes. Runtime rendering may be
  added as a verifier, but persisted cards are the contract used by the
  registry and static browsing.

Publisher prose remains explicitly delimited from templated operational facts.
When class partitions are thin, the summary leads with predicates, namespaces,
and leading class relations where any exist.

## Producer: kgf build stats

Land the smallest useful producer before the full build.yaml DAG:

1. Run hdtc void for the merged queryable HDT and each published component HDT.
2. Link component descriptions from the merged dataset with void:subset. Record
   which source becomes queryable, design (canonical component), and each
   component view.
3. Build stats/void.hdt with hdtc create, then stats/void.hdt.perm with hdtc perm.
4. Open the completed VoID pair through IndexedHdt and traverse each known view
   root once. Emit stats/schema-nodes.tsv with the final subject IDs and
   stats/class-relations.tsv from the typed object-class leaves (untyped targets
   have no object_class column and remain visible in VoID property counts);
   duplicate the
   canonical component's rows for the design view, sort each block as specified,
   and record exact byte ranges.
5. Run hdtc namespaces with registry prefixes first and manifest overrides
   second.
6. Render stats/summary.json and .md.
7. Add every artifact, its SHA-256/size, and typed TSV view directories to
   manifest.json; recompute content_digest.
8. Run the same verification used by registry ingest before publishing.

Do not shell together partial output and call it a successful build. Write into
a staging directory and publish the complete immutable artifact set only after
every command and verification succeeds. Later, this step becomes the stats
node in the full build.yaml DAG.

## hdtc boundary

The companion plans are ../hdtc/notes/namespaces-command.md and
../hdtc/notes/description-artifacts.md. hdtc owns:

- VoID analysis and the generic void, create, perm, and namespace-count commands
  and their formats.

KGF owns multi-component view names, void:subset assembly, traversal of the
final indexed VoID, both final TSVs and their manifest offsets, summaries, HTTP
semantics, caps, and cursors.

## Resolved decisions

1. No void.ttl bundle artifact; serialize void.hdt on demand.
2. Require stats/void.hdt.perm; do not materialize the VoID tree.
3. Reuse one IndexedHdt implementation for data and description graphs.
4. Resolve semantic selectors through a sorted, disk-backed selector index that
   stores final VoID subject IDs; never derive or standardize partition names.
5. Call the flat observational view **class relations**, not “type edges” or
   declared schema edges.
6. Persist class relations as count-descending TSV, not CSV.
7. hdtc counts namespaces; KGF supplies/merges the curated prefix table.
8. Persist summaries; runtime rendering is optional verification.
9. Move all description artifacts under stats/ and checksum them individually.

## Completion checks

- cargo test --workspace
- open-cost test shows no payload scan and no allocation proportional to VoID
  node count
- golden /schema tests for shallow shape, count ordering, filters, budgets, and
  cursor no-loss/no-duplication
- malformed selector nodes, parent paths, ranges, and TSV fixtures are refused
- /void representations parse as RDF and describe the same triple set
- build output verifies byte-for-byte and rebuilds deterministically for fixed
  inputs
