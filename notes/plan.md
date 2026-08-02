# kgf-rs implementation plan

The route to doc 20 §20.8's **M1**: mapped `data.hdt` + `data.hdt.perm`, all eight
patterns at doc 20 §20.2's bounds, exact counts, stable positional cursors, and the
HTTP surface over them. Spec references are to `../kgf/docs`.

Ordered so that every unit is verifiable when it lands rather than at the end. The
first two touch nothing external — no hdtc, no files, no I/O — which is where the
bit-twiddling everything else trusts belongs. Units 10–12 have the same property on the
server side: cursor tokens, term syntax, and the response envelope are all pure, and
all testable against the golden bundle before a socket exists.

**This is the implementation route, not the design and not the project roadmap.** The
design is `../kgf` docs 01–20 and governs; doc 07 is the project roadmap across all of
KGF; doc 20 §20.8 fixes the milestones. What lives here is the order in which this repo
builds them and what each unit had to decide. `notes/state.md` is the point-in-time
handoff — what is built, what was learned. When this file and a design document
disagree, that is a bug in one of them.

Units 1–13 are complete; each carries a **What landed** section written after the fact,
which is where a unit's plan and its outcome are reconciled. Unit 14 has none yet.

**Nothing here is frozen.** Doc 20 §20.7 and §20.8 speak of formats being stable from
the first release; no release has happened, and the service is expected to run for a
long time before one does. Treat those statements as intent rather than as a freeze:
pick the sensible design, write down where it can move and why, and keep going. What
must not drift meanwhile is the enumeration order doc 20 §20.2's table fixes, because
that is what every position means.

## Units

### 1. `map` — mapped regions and packed element access

`Mapping` (the crate's only `unsafe`), `PackedArray`, `BitmapView`.

Entry `i` of a packed array begins at bit `i * width`, LSB-first, `width` in
`0..=64`; `width == 0` means every entry is zero. One reader serves both the HDT and
the sidecar — see the module docs for why the difference between them is framing
rather than element access.

*Verified by* round-tripping against a naive bit-by-bit encoder: every width `0..=64`,
random lengths, every entry, and the final entries specifically (the case where the
backing slice has no slack).

### 2. `rank` — `RankedBitmap`

`rank1`, `select1`, `count` over the persisted two-level directories, with the
superblock and subblock widths read from the header rather than assumed. Read-only:
nothing here constructs a directory, because constructing one is a full pass over
every bitmap byte — the exact cost lazy open exists to avoid.

*Verified by* naive prefix-popcount and naive scan over random bitmaps at adversarial
densities (empty, single bit, all ones, alternating) and lengths landing exactly on
and either side of multiples of 512 and 4096. The `rank1(L)` sentinel case gets its
own test; it is the one the general formula would index one past the end for.

A directory builder lives in `#[cfg(test)]` so the properties can be checked without
a bundle.

### 3. `hdt::HdtLayout` — locate sections in a mapped `data.hdt` ✅

Walk the global control info, the header, the dictionary's four PFC sections, and the
triples' `ArrayY`/`BitmapY`/`ArrayZ`/`BitmapZ`, recording offsets and shapes.

**This is a preamble walk, not a scan.** Every section states its own size, so
locating all of them costs about a dozen small reads — well under a kilobyte, a dozen
pages — and touches no payload.

**Do not call hdtc's materializing readers here.** `LogArrayReader::read_from` and
`PfcSectionHeader::read_from` read their whole payload into a `Vec` and verify CRCs.
That is right for a bounded-memory CLI and wrong for us: on Ubergraph the object
dictionary's block-offset array alone is ~1.3 M entries. If hdtc offers only a
materializing form for something we need, add a preamble-only variant *to hdtc*
rather than reimplementing the parse here (doc 20 §20.4).

**What landed.** The walk itself is now hdtc's, because it had to be: hdtc's `skip_*`
forms report where a *section* starts, and a mapped reader needs where the *payload*
starts, which cannot be derived without restating preamble layouts here. So hdtc's
façade grew `scan_bitmap_section`, `scan_log_array_section`, `scan_pfc_section`, and
the whole-file `scan_hdt_sections`, all preamble-only and all reporting payload
offsets; `HdtLayout::parse` calls the last of these and turns each located region into
a `PackedSpec`/`BitmapSpec`/`BytesSpec` validated against the mapping. Ubergraph
(2.5 GB, 606 M triples) maps in 88 µs and parses in 2.5 ms.

*Verified by* building a fixture with hdtc and asserting the layout agrees with what
hdtc recorded about the same file in `data.hdt.perm` — triple count, the three id-space
sizes, and the bit lengths its SPO directories say they index — plus: every region
projects and reads in range, `BitmapY` has one set bit per subject and `BitmapZ` one
per (subject, predicate) pair, and a non-HDT or truncated file is refused by name.

### 4. `dict` — PFC random access ✅

`locate`, `extract`, `prefix_bounds`, and the role/shared-section arithmetic, over the
`PfcLayout`s unit 3 already located (term count, block size, the block-offset
`PackedSpec`, and the string buffer's `BytesSpec`).

Standard HDT already supports all of this: each section is lexicographically sorted in
blocks of `block_size` (16 by default), preceded by a `LogArray` of block offsets with
a sentinel. `locate` binary-searches block heads, which are stored uncompressed;
`extract` decodes at most one block; `prefix_bounds` falls out of the same search. The
block-offset array is mapped in place, never materialized.

*Verified by* the strongest differential test available in the system: the dictionary
is fully enumerable, so `extract` over every id must reproduce hdtc's independent
sequential PFC reader term for term. If that passes over Ubergraph's 355 MB of strings,
PFC random access is right.

**What landed.** `DictionaryLayout::view` projects the four mapped sections for a
request; block offsets stay packed and are never materialized. Lookup upper-bounds
block heads and decodes one candidate block per searched section, extraction decodes
only the addressed block into the caller's buffer, and prefix bounds use the same
lower-bound search.
Role arithmetic is confined to `dict`: shared ids retain their subject/object ids and
role-only local ids are offset exactly once. Payload corruption is reported explicitly
rather than being treated as a missing term.

The differential test enumerates every section through hdtc's independent sequential
PFC reader and compares every `extract` and `locate`, using enough terms to cross
multiple blocks. Prefix ranges are checked against that enumeration, including empty
and unbounded byte prefixes.

One spec correction surfaced: doc 20 §20.5 sketches one `Range<TermId>` for a role
prefix, but `dictionaryFour` concatenates two *independently sorted* sections for
subjects and objects (shared, then role-only). A prefix can therefore occupy two
disjoint id ranges. `PrefixBounds` preserves both and still gives an exact `O(log D)`
count; the server will merge the two sorted runs when it implements `/terms`. The code
follows the format here; the single-range sketch in doc 20 is the bug.

### 5. `perm` — map the sidecar ✅

`hdtc::format::PermutationIndex::open` for the header, directory, and binding checks;
then map each region from its directory entry. Assemble the POS and OPS
`BitmapTriples`, and bind the host HDT's SPO bitmaps to the component `0x03`
directories that ride in the sidecar.

hdtc's `PermutationIndex::triples` is a seek-based path for its own CLI and is not
used.

**What landed.** `Permutations::open` owns the coupled HDT and sidecar mappings,
parses the HDT layout, and delegates the sidecar header, directory, and cheap source
binding checks to `hdtc::format::PermutationIndex`. Directory entries become
`PackedSpec`/`BitmapSpec`/`RankedSpec`s without reading payloads. POS and OPS project
entirely from the sidecar; SPO projects arrays and bitmaps from the HDT while taking
both rank directories from the sidecar. The three public projections therefore return
the same `BitmapTriples` type and cannot be paired with mappings from another bundle.

The work closed another façade gap in hdtc: permutation components and section kinds
are now typed public identifiers rather than copied numeric constants, and the parsed
header exposes its superblock/subblock widths. hdtc remains the only owner of the wire
ids and validates that directory parameters agree with those header widths. Its cheap
open also now rejects unequal POS/OPS pair counts, an impossible representation of the
same `(predicate, object)` key set.

*Verified by* a golden bundle built by hdtc: every mapped array stays in its declared
id space; bitmap populations close exactly the level-1 and level-2 groups implied by
the dictionary and pair counts; all three projections assemble; a sidecar from another
HDT and a truncated sidecar are refused before a view is returned. There are 39
`kgf-store` tests after this unit.

### 6. `hdt::BitmapTriples` — the shared traversal ✅

`level2_range`, `level3_range`, `find_level2`, `find_level3`, `level1_of`,
`level2_of`, `level2_at`, `level3_at`. One implementation over all three permutations,
which is possible because all three have implicit level 1 and the same two-level shape.

**What landed.** Group ranges are select-derived, owning groups are rank-derived, and
both packed levels use one bounded binary-search implementation. `level2_at` is the
eighth primitive: the original list omitted the `ArrayY` read needed to materialize an
unbound triple after `level2_of` recovers its position. Without it the stated milestone
was impossible; adding the symmetric accessor corrects the plan without changing the
format or enumeration contract.

*Verified by* reconstructing the complete golden fixture independently in SPO, POS,
and OPS order. The test exhausts every range boundary, inverse rank mapping, packed
value, successful search, missing value on both sides, and empty terminal range, then
compares the result with role ids resolved through the dictionary. There are 40
`kgf-store` tests after this unit.

**This is the real milestone.** At this point the store answers patterns over a
606 M-triple bundle in id space, and doc 20 §20.6's cold-start-with-N-bundles
measurement becomes possible. Everything after it is contract work rather than
discovery.

### 7. `pattern` — the §20.2 table ✅

Eight patterns to `Selection`; exact counts by rank difference; `page`; `at` for
`/sample`. `s ? o` gets its dual route, choosing on `min(deg(s), deg(o))` — both
degrees are rank differences, so choosing is cheap — with linear scan inside a route
for ranges spanning a page or two and binary search above that.

The enumeration order fixed here is a contract: cursors are positions in it.

**What landed.** `Selection` is either a contiguous Z range in the table's canonical
permutation or the bounded `s ? o` group plan. Resolution validates role-scoped ids,
enumerates nothing, and fixes exact counts, random access, and paging. Contiguous
cursors are result offsets; `s ? o` cursors carry the last predicate id, so resumption
is route-independent. Its planner compares endpoint degrees, scans all predicate
groups when the chosen endpoint's complete packed range spans at most two pages, and
otherwise binary-searches every group.

`Selection::at` is constant-rank work for contiguous patterns. For `s ? o`, random
access necessarily costs the same bounded predicate-group probe as enumeration and
count because no permutation makes the answer contiguous. Docs 03 and 20 name this
exception explicitly: `/sample` enumerates the bounded predicate result once and
samples that request-local result rather than calling `at` repeatedly.

*Verified by* differential comparison against `hdtc search` for all eight shapes,
plus exhaustive resolution over every valid bound/unbound id combination in the
golden graph. Counts equal enumeration lengths; `at(i)` equals row `i`; page sizes 1,
2, 3, 7, and over-cap reconstruct the same order; and resuming at every cursor
position yields exactly the suffix. Predicate- and object-rooted count sums close to
N, and `? p o` agrees between POS and OPS. There are 45 `kgf-store` tests after this
unit.

### 8. `store` and `catalog` ✅

Required-artifact checks with errors that name the command to run, cheap binding
verification, `Arc<Store>`, lazy open with a singleflight guard, eviction by dropping
the `Arc`.

**What landed.** `Store::open` requires the manifest, HDT, and permutation sidecar,
requires and cheaply binds `data.hdt.graphs.idx` whenever the graphs sidecar is
present, maps through the module's single audited unsafe boundary, and preserves
classified hdtc binding failures. Full checksums remain on the publish/`kgf verify`
path; the stale open-time checksum flag was removed rather than ignored.

`Catalog::scan` records sorted dataset/version directories without opening them.
Each immutable entry moves through `Closed`, `Opening`, `Open(Arc<Store>)`, or a
cached classified failure. First access is singleflight on success and failure;
eviction resets any state to `Closed`, cannot let a superseded opener repopulate the
entry, and drops only the catalog's `Arc`, so in-flight clones remain valid.

*Verified by* opening the local four-artifact `ubergraph2` bundle in 12 ms and reading
its 606,342,307-triple structural count; required-artifact and cross-HDT binding
failures; concurrent first access sharing one `Arc`; eviction/reopen while an old
clone stays live; and 8 threads × 6 bundles × 80 mixed-pattern iterations under
concurrent eviction. There are 53 `kgf-store` tests after this unit.

### 9. `manifest` — bundle identity, and `kgf manifest` ✅

Not in the original eight: the query core does not need a manifest, but every M1
endpoint does — `prefixes` for CURIE syntax in parameters (doc 03 §3.3), `id`/`version`
for the response envelope, `capabilities` for routing, `content_digest` for ETags and
cursor binding, `counts` for `/void` and `/summary`. Deferring `kgf build` means those
have to come from somewhere, and hand-writing them is not it.

**What landed.** `kgf_store::manifest` owns doc 04 §4.3's document: `Manifest` parses
and serializes it, `BundleFacts::read` recovers the structural half from the artifacts,
and `Manifest::verify_against` proves a manifest still describes the bytes beside it.
`kgf manifest` writes one for a bundle assembled with `hdtc create --perm`.

Three decisions worth naming:

- **`Store::open` still does not parse the manifest.** It requires the file to exist —
  a directory without one is not a bundle — and stops there. The query core answers
  from `data.hdt` and `data.hdt.perm` alone, and keeping the parse out is what keeps
  the store testable headless against fixtures carrying a `{}` placeholder.
- **`BundleFacts::read` is the one path that opens artifacts without a manifest**,
  because it is what produces one; requiring a manifest there is circular. It makes
  every check `Store::open` makes apart from that one — both resolve artifacts through
  the same `ArtifactSet` and share its graph-index binding check — so a bundle
  `kgf manifest` describes is a bundle that opens.
- **Checksums stay out of the read layer.** `content_digest_preimage` fixes the Merkle
  recipe — artifacts sorted by name, `{name}  {sha256}\n`, hashed, prefixed `sha256:` —
  and returns the preimage rather than the digest, so `kgf-store` never hashes a file.
  The one place that reads whole artifacts is the binary, which is where doc 20 §20.6
  puts full digests.

Derived: counts, capabilities, sizes, checksums, `content_digest`. Asked for: identity
and description, re-read from any manifest already present, so regenerating after a
rebuild is `kgf manifest <dir>` with no flags. Capabilities are artifact-determined —
`star`, `sample`, `terms`, `export` need only the required artifacts; `graphs` needs
the sidecar pair; `search`, `range`, and `closure` are never guessed at, since a bundle
cannot acquire them without acquiring an artifact.

`created` dates the bundle, not the file, so it carries forward while the digest is
unchanged. Regeneration over unchanged artifacts is therefore byte-identical.

**Two things a review caught, both about the difference between reading and rewriting.**

`--check` compared only counts, and counts are a weak witness for a rebuild: editing
one literal leaves all four identical — and, as it happens, the file length too — while
rewriting every byte, so a stale `content_digest` passed. The store-side
`verify_against` stays counts-only, since full digests are off the open path by design
(doc 20 §20.6); the CLI, which already hashes every artifact to *write* a manifest, now
hashes them to *check* one, and also compares the capability and artifact sets that no
count reflects.

Rewriting through `Manifest` alone deletes every field this build does not model —
`source`, `components`, a capability's configuration body, anything a newer builder
added. `ManifestDocument` keeps the raw JSON beside the parse and merges derived fields
over it, so unmodeled keys survive and only modeled ones are replaced. The parse is
allowed to fail, because `{}` is a bundle's first manifest; what is not allowed is
silently overwriting a document declaring a schema this build cannot read, which is now
refused before anything is written.

*Verified by* an end-to-end integration test over an hdtc-built bundle: artifacts alone
are refused by `Store::open` naming `kgf manifest`; the generated manifest makes the
bundle servable and agrees with the store's own counts; identity is inferred from the
`{dataset}/{version}` layout; regeneration is byte-stable and carries descriptive
fields; and rebuilding the artifacts makes `--check` fail naming `counts.triples` and
the repair command. Separate cases cover a rebuild that preserves every count, a
hand-edited `content_digest`, unmodeled fields surviving a rewrite, and a
newer-schema manifest being refused rather than downgraded. Plus unit tests for the
capability rule, the digest preimage, manifest round-tripping, document reading when
the manifest in it does not parse, the graph-index refusal, and RFC 3339 formatting.
There are 84 tests after this unit.

**Still open, and recorded rather than fixed:** `artifact_names_for` knows only the
four artifacts that have producers, while doc 04 §4.1 reserves nine more. Each new
sidecar must be added there or it falls out of `content_digest`. Rejecting unknown
files is *not* the answer — doc 04 §4.1 and doc 20 §20.8 both make
`data.hdt.index.v1-1` a conforming artifact that no server reads — but whether that
index should nonetheless be checksummed, given doc 04 §4.3 wants the digest usable for
mirror verification, is a question for the design docs.

### 10. `cursor` — the token codec ✅

Encode and decode doc 20 §20.7's token: version byte, content-digest prefix, operation
id, canonical-request hash, permutation, position, and the reserved `binding_index` and
`scan_position`. Plus the canonicalization the request hash is taken over — doc 03 §3.6
makes a canonical form normative so that content-keyed caching hits, and for M1's GET
routes that is the normalized query string.

Pure: no I/O, no store, no HTTP. It lands before anything that needs it because the
store side that makes its central property assertable is already built and tested.

**What the token is actually for**, since the module's doc comments currently overstate
one part. For the six contiguous patterns a position *is* a result offset —
`Selection::page(from, limit)` takes exactly that — so the token is packaging. It earns
its place on three other counts:

- **Positions that are not offsets.** `s ? o` resumes on the last predicate id returned
  (route-independent, doc 20 §20.2.1); bindings QUERY resumes on an (input row, offset)
  pair; a budgeted scan resumes on a scan position plus an accumulated lower bound.
  Only the first is M1, but doc 20 §20.7 fixes the token format from the first release,
  so the room must exist now.
- **`request_hash`.** A versioned URL pins the *data*; nothing pins the *request*. Two
  requests differing only in a bound `s` are the same path with the same offset, and an
  offset would silently page into a different result set.
- **Opacity.** Published offsets become a contract clients index into, and random access
  has a different cost profile from paging — `Selection::at` for `s ? o` walks the
  bounded probe, which is why doc 03 §3.4.7 forbids `/sample` from calling it per
  sample.

`digest_prefix` is the weakest of the four and should be documented as such rather than
as the reason: doc 04 §4.6 makes versioned URLs immutable, so a client paging
`/v/{version}/` cannot drift. What it still catches is a client that rebuilds page-2
URLs from `/latest/`, and a resume against a mirror serving different bytes under the
same label — which doc 04 §4.3 names as a use of `content_digest`, and doc 05 §5.1
assumes away.

*Verified by* doc 20 §20.9's cursor properties, differentially against the store:
resuming at every position of every pattern over the golden bundle yields exactly the
suffix, at adversarial page sizes. Plus round-trip over the field space, and rejection
of a tampered token, a foreign digest, and a foreign request hash — all as
`stale_cursor`, undifferentiated, so a client learns nothing about data it did not
query.

**What landed.** A 29-byte fixed layout — version, operation, position space, flags,
digest prefix, request hash, position, and the two optional trailers — in URL-safe
base64 without padding, so an M1 token is 39 characters. `CanonicalRequest` sorts its
parameters and length-prefixes every key and value, so no two distinct parameter sets
hash alike. `CursorBinding` bundles digest, operation, and request hash into one value
that `decode` checks in full, because three separate arguments is three chances for a
handler to check two of them.

Two corrections to the skeleton, both from writing the resume property down:

- **`CursorPermutation` became `PositionSpace`, with a fourth variant.** The old type
  recorded which permutation a position indexed and said a mismatch was `stale_cursor`
  — but for `s ? o` the position is a *predicate id*, and the planner is free to switch
  routes between pages (doc 20 §20.2.1), so comparing the route would reject a
  legitimate resume. What a token needs to record is the space the number lives in, and
  `Predicate` is a fourth such space rather than a permutation. Wire values 1–3 are
  unchanged.
- **`/describe`'s phase needs no field.** `direction=both` is out-triples (`s ? ?`, SPO)
  then in-triples (`? ? o`, OPS), which land in different spaces, so the token already
  says which half it stopped in.

`Operation` omits `/sample`, which draws `n` members and never pages, and includes
`Count`, which only issues a token for M2's budgeted scanning counts — enumerated now
so nothing else takes the value.

A forged position is safe rather than merely unlikely: `Selection::page` clamps past the
end and yields an empty page. A handler should still reject a position past the end as
`stale_cursor`, since an empty page otherwise reads as the end of results, and that check
needs the store so it belongs to unit 14. The bound is per position space, though, not
one rule: for the three permutation spaces the position is a result offset and
`selection.count()` bounds it, while a `Predicate` position is the last predicate id
returned and is bounded by the predicate id space — a one-row `s ? o` answer resuming at
predicate 37 is correct, and checking it against a count of 1 would reject a live cursor.

The unit also made `kgf_store::testing` available behind a `testing` feature. The golden
bundles were `#[cfg(test)]`-private, and this is the third place to need them — a
second copy of the build recipe is exactly the drift doc 20 §20.9 warns about. `kgf`'s
integration test dropped its duplicate of the hdtc search as a result. Only the parts an
out-of-crate test needs are exported: `Fixture`, the fixture graphs, and `hdtc_binary`.
The safe wrappers over the publication capabilities stay crate-private, because exporting
a safe `&Path`-taking constructor for a `Mapping` or a `PublishedBundle` hands external
safe code a way to map a file it can still truncate — the exact obligation those
constructors are `unsafe` to record. Inside the crate they are covered by `map`'s
soundness argument; outside it they would not be.

There are 89 tests after this unit.

### 11. `term` — syntax in, syntax out ✅

Doc 03 §3.3 parsing (bare and percent-encoded IRIs, CURIEs against the manifest prefix
map, quoted literals with `@lang` and `^^datatype`, the JSON term-object form) and
serialization of the term shapes rows carry.

This is the boundary doc 20 §20.5 names: resolve to ids once on the way in, run over
ids, materialize strings only while serializing. The per-request term cache belongs
here too — a page of results repeats predicates and IRIs constantly, and `Store` is
forbidden a cache precisely so this layer can have one without a lock.

Still no HTTP.

*Verified by* round-tripping every term in the golden bundle through parse → `locate` →
`extract` → serialize; CURIE resolution against a generated manifest's prefix map,
including a prefix that collides with a URI scheme (doc 03 §3.3 says the manifest wins
only for declared prefixes); and malformed input producing `bad_term_syntax` rather
than a plausible term.

**What landed.** `Term` is the pivot between *three* syntaxes, which the unit's heading
elides into two. Request syntax abbreviates through the prefix map and writes a datatype
bare (`^^xsd:integer`); dictionary syntax never abbreviates and always brackets it
(`^^<http://…#integer>`); response syntax is the term object. Conflating the first two
is the bug that answers "no rows" for data that is present — `locate` simply misses —
and no checksum or test of the store catches it, so the conversions are separate methods
and the round trip is asserted against a real dictionary rather than against itself.

That made the dictionary spelling an hdtc question, not ours. The façade already
published the reading direction through the text analyzer's rules but not the writing
one, so **`hdtc::format` gained `encode_literal` and `XSD_STRING`** beside
`parse_literal`, hdtc's own RDF parser now delegates to it instead of formatting terms
itself, and its format contract test checks both directions against the bytes a built
dictionary actually holds. This is the drift `../kgf` docs 17–18 warn about, in its
quietest form: the subtle rule is not the brackets but that `xsd:string` is *dropped*,
so a client asking for `"a"^^xsd:string` must be answered from the term stored as `"a"`.
`Literal::typed` folds it at construction, which is also what lets `==` mean "the same
RDF term".

**An IRI is bracketed and a CURIE is not** — `<http://x/a>` against `ex:a` — and neither
form can be read as the other. This is a deliberate departure from §3.3 as written, made
during the unit and recorded as decision 10 below rather than as a liberty. §3.3 accepts
a bare IRI and resolves the ambiguity by guessing: "a token parses as a CURIE only when
its prefix is declared in the manifest prefix map; otherwise as an IRI". Three things
follow from that guess, and all of them are bad. The same string denotes different terms
at different endpoints, because the answer depends on the manifest it is sent to. A
bundle that declares `http:` — which §3.3 can only advise against — has IRIs no request
can name. And a CURIE whose prefix is simply misspelled becomes an IRI whose scheme is
the typo, so the server answers "no such term" for a request that was malformed. Under
brackets none of those exist: an undeclared prefix is an error naming the two fixes, and
declaring `http:` stops being a hazard at all. It costs `%3C`/`%3E` in a GET URL, on top
of the escaping §3.3 already requires.

Two more parsing decisions §3.3 does not settle, recorded as questions below. Literals
are **unescaped**, so the closing quote is the last one and `"a "b" c"` is one literal —
the same rule hdtc reads the dictionary by, and the only one under which a term
round-trips out through a response and back into a request unchanged. And `_:label` is a
**blank node** before the CURIE rule sees it, which no sentence in §3.3 covers but every
other reading of which makes blank nodes unaskable.

**Terms are canonical on the way out as well as in.** A stored `"x"@EN` is reported as
`@en` and a stored `"a"^^xsd:string` as plain, because a response should carry one
spelling of a term whichever bundle answered: doc 05's federated clients compare terms
across endpoints, and two spellings of one term read as two terms, failing silently
rather than erroring. The cost is an assumption worth stating — **a bundle's dictionary
is expected to hold canonical terms**, which hdtc's parsers guarantee for bundles built
the documented way. One that does not has a term reported under a name it cannot then be
fetched by. Detecting that is an offline `O(dictionary)` scan, so it belongs to
`kgf manifest --check` rather than to `Store::open` (doc 20 §20.6).

`kgf manifest` now seeds `rdf`, `rdfs`, `owl` and `xsd` into the prefix map, overridable
and idempotent. Requiring brackets made an undeclared prefix an error, and the default
map was empty, so a freshly described bundle accepted no CURIE at all — including doc
03 §3.4's own `p=rdfs:label` and `o.ge="100.0"^^xsd:double`. The four are fixed by the
specs that define RDF, so declaring them asserts nothing about the dataset; a longer
curated list would be this tool guessing at subject matter, and the manifest is the
contract a client reads to know what it may send.

A term object is **closed**: an unrecognized key is refused rather than ignored. Ignoring
`xml:lang` turns a SPARQL Results JSON literal into a plain one — a different term that
resolves and answers — and §3.4.1's claim of SRJ compatibility guarantees clients send
it, so `xml:lang` and `uri` get messages naming the spelling to use instead.

`TermCache` keys on (role, id), not id: the shared section gives a subject and an object
the same id for the same string, but the role-only sections do not, and a cache that
forgot the role would answer confidently from the wrong section. It hands out `Rc<str>`
rather than a borrow so the three terms of a row can be alive at once while the cache
stays usable for the next row, and it validates UTF-8 once per distinct term, which is
what lets `Term::from_dictionary` be infallible.

Terms serialize through `serde::Serialize` straight into the writer rather than by
building a `serde_json::Value`: a page is `limit` rows of up to three terms, and a map
allocated per term is a map allocated per term.

There are 105 tests after this unit.

### 12. `envelope` — completeness and errors ✅

Doc 03 §3.6's uniform vocabulary: `complete`, `truncation_reason`, `next`, `cardinality`
with its `exact` flag, and the `KGF-Complete` / `KGF-Truncation-Reason` /
`KGF-Next-Cursor` headers that carry the same metadata for formats whose bodies cannot.
Errors are RFC 9457 `application/problem+json` with a machine-readable `code`.

Its own unit rather than a detail of the routes, for two reasons. Doc 20 §20.8 requires
envelopes to follow doc 03 **from day one** — there is no phase where responses are
shaped approximately. And the body/header duplication is a correctness obligation, not
formatting: silent truncation is prohibited, so a CSV response that loses `complete`
is a protocol violation rather than a cosmetic gap.

M1 emits `page_limit` truncation only. The budget reasons (`time_budget`,
`candidate_budget`, `response_bytes`) are M2 machinery, but they are part of the closed
vocabulary now so the type cannot grow a stringly-typed escape hatch later.

*Verified by* a property over every M1 operation: a response is either `complete: true`
with no `next`, or `complete: false` with a `truncation_reason` and a resumable `next`
— never any other combination — and the headers agree with the body in every format.

**What landed.** The property above is not asserted on the way out, it is the only thing
the type can express. `Completeness` is opaque, and its constructors are the whole API:
`complete()` takes no cursor, `page_limit(next)` requires one, `cell_overflow()` and
`partial_failure()` refuse one. "Incomplete with no reason" and "complete with a next
page" are unconstructible rather than untested, which is the right shape for a rule doc
03 §3.6 states as a prohibition.

Writing it that way turned up a distinction the plan's sentence glosses: **not every
truncation resumes.** The four interruption reasons stop an enumeration, which has a
position to continue from; `cell_overflow` and `partial_failure` describe a result that
is already as complete as it will get. A client that paged on those would ask for the
same cell, get the same overflow, and loop. So `TruncationReason::resumes()` derives
that from the reason rather than tracking it alongside, and the constructors' arity
enforces it — `budget_exhausted` is the one that takes a reason as a value, and it
asserts, because it is the only hole through which a non-resuming reason could acquire
a cursor.

The headers are checked as *agreement with the body* rather than against expected
strings, so neither rendering can drift alone. That is the test the CSV/Parquet
obligation actually needs: those formats carry `complete` only in the header, and a
mismatch there is a protocol violation no body assertion would catch.

`Cardinality` is `Exact(n)` or `Estimated { value, min }` rather than a value beside an
`exact` flag, because §3.6's `min` lower bound is meaningless on an exact count — an
exact count is its own bound.

A resume cursor is a `CursorToken`, which only `Cursor::encode` mints, so a truncated
response cannot carry an empty continuation or one containing CR/LF — `KGF-Next-Cursor`
puts it in a header, where that is injection rather than a typo.

`Cardinality` is opaque for the same reason as `Completeness`, and for a sharper one
than it first looked: §3.4.4's lower bound and the estimate it bounds come from
*different computations*, so nothing but a constructor stops a scan that reached 50
from reporting "at least 50, about 10". `at_least` raises the estimate instead —
a counted number disproves a guessed one. And §3.4.1's `distinct_objects` is a third
quantity, exact on a response whose `value` is not, so the type carries it rather than
being reopened when M2's ranges arrive.

Errors are RFC 9457, and the code table went into doc 03 as **§3.6.1** rather than being
invented here; see question 15. One code, one status, which is why failing content
negotiation is three codes rather than one code carrying a status, and why
`ErrorCode`'s token, status and reason phrase come from a single match over the enum —
deriving the phrase from the status would check exhaustiveness over `u16` and panic
while rendering the error response a new code was added for. `TermSyntaxError` and
`StaleCursor` both convert straight through, their messages becoming `detail`, which is
why unit 11's messages name the token and the remedy.

There are 116 tests after this unit.

### 13. The HTTP skeleton ✅

The stack decision, the catalog wired to routes, doc 03 §3.2's URL structure, the
`latest` 307/308 (method-preserving — a 302 could rewrite a body-carrying QUERY to
GET), `Cache-Control` and representation-specific `ETag` with `Vary: Accept`, the
blocking-pool boundary, and `/manifest` as the one endpoint that proves the path end to
end.

**The decision this unit makes is the stack.** Nothing is chosen: no `tokio`, `axum`, or
`hyper` in the workspace. Doc 03 §3.1 makes HTTP QUERY (RFC 10008) canonical with POST
a permanent first-class fallback, and extension-method routing is where general-purpose
routers get awkward. M1 has no body-carrying route — bindings QUERY is M2 — so this
could be deferred, and that is exactly the trap: choosing a stack that cannot express
QUERY and discovering it when M2 arrives. Settle it with a spike here.

The blocking boundary is not incidental. A page fault stalls a thread, so a cold mmap
read on the async reactor converts one slow request into a stalled server. Handlers
hold an `Arc<Store>` and call synchronous store methods on a blocking pool
(doc 20 §20.4).

*Verified by* serving the golden bundles over a real listener: version resolution,
`latest` redirect preserving method, immutable caching headers on versioned GETs,
`ETag` varying by `Accept`, an unknown dataset and an unknown version answering
distinctly, and a bundle whose open fails answering as a problem document rather than a
panic.

**What landed.**

**The stack is axum 0.8 on hyper and tokio, and QUERY was settled by spike before
anything was built on it.** hyper's parser accepts the extension method, axum's router
carries it, and a `MethodRouter` fallback receives it with the name intact and an
`Allow` header supplied. So every route is mounted with that fallback, answering
`method_not_allowed` — which is *also* where M2's QUERY handler goes, rather than a
different kind of route. The test sends `QUERY /… HTTP/1.1` onto a `TcpStream` by hand,
because every HTTP client is itself a stack that might normalize the method away.

That closed the trap this unit existed for: M1 has no body-carrying route, so a stack
that could not express RFC 10008 would have looked fine until M2.

**Doc 03 §3.6.1 gained `method_not_allowed` (405)**, for the reason `rate_limited` and
`internal_error` were added in unit 12 — the table claims *every* error response
carries a code, and a 405 is one any server must produce. It earns its row twice over
here: a client's first question of an unfamiliar deployment is whether a resource takes
QUERY, and an empty 405 does not answer it.

**Every route answers JSON *and* HTML at one URL** — the LDF/QPF affordance, on
`Accept` alone with no user-agent sniffing and no second URL space. It falls out of RFC
9110 §12.5.1 plus one decision: JSON is offered first and ties go to the first offer,
so a browser's exact `text/html` beats its own trailing `*/*;q=0.8` while `curl`, a
library and an agent all tie at `*/*` and get data. The machine-readable form is the
default rather than the exception, which is the right way round for doc 01's argument.

Three consequences worth recording. Negotiation had to be *implemented*: nothing in
this stack parses `Accept` — not axum, not `tower-http`, and the `headers` crate stops
short of a type for it — so §12.5.1's rule that the most specific range decides
*before* its `q` applies is written out and checked against `headers-accept`, an
independent reading of the same section, as a dev-dependency oracle. That comparison
found `mime` silently dropping any media range written with the optional whitespace
§5.6.3 allows around `;`, which made `Accept: text/html ; q=0.5, application/json;q=0.4`
serve JSON; `mediatype` handles it. Five differences from the oracle remain and each is
asserted, so none can change by accident. `ETag` being
representation-specific stopped being a formality: without the `.json`/`.html` suffix a
shared cache would answer an agent from the page, and `Vary: Accept` alone would not
stop it, because the tags would be equal. And errors negotiate too, through a
`Problem` that converts into a response carrying *itself* and one middleware that
renders it — so a handler's error, an extractor's rejection, the router's 404 and a
method fallback's 405 are one rendering, and none of them is the one that forgets
`Vary` or answers a browser with raw JSON.

Pages are `maud`, whose `html!` escapes every interpolation and needs `PreEscaped` to
opt out, so nothing in the crate concatenates markup. The data on these pages is a
published bundle's own manifest and dictionary, so a dataset whose title contains a
`<script>` tag is an ordinary case. This started as a hand-written builder and its
escaping test caught a real bug on the first run — an `&` written raw into an `href`
while the URL around it was escaped, exactly the slip the macro makes unavailable. The
builder is gone; the test survived the port unchanged, minus one assertion that turned
out to be testing the builder's belt-and-braces `'` escaping rather than the property
(`maud` always double-quotes attributes, so an apostrophe cannot end one).

**`current` is derived, because there is nothing to read it from.** Doc 04 §4.3 puts it
in the dataset descriptor, calls that document mutable and host-independent, and
nothing in the toolchain writes one; a deployment is a directory of bundles. So `/` and
`/{dataset}` are derived at startup from the bundle manifests, which carry every field
they need. `current` is the greatest release under **one** total order — by `created`,
then by version label — not by label alone, because doc 03 §3.2 permits "a content hash
prefix" and hash labels have no order, and a `latest` redirecting to an arbitrary
version is worse than one that does not exist. The comparison is over parsed instants,
not the strings: RFC 3339 spells one instant several ways and two of them sort wrongly
(a `+01:00` offset, and a fractional second, where `…:00.5Z` sorts *before* `…:00Z`).
Question 16 asks whether the host should supply the descriptor instead.

**`/manifest` opens the bundle.** The bytes are already in memory, so this is not how
they are fetched — it is so that a version which cannot be served is never described as
though it can. A client reads `capabilities` here and issues what it finds; advertising
`sample` for a bundle missing `data.hdt.perm` moves the failure to the query. Opening
is singleflighted and cached, so it costs one open per version for the life of the
process, and it is what gives this unit a real blocking boundary to test. An open
failure is `internal_error` 500 — the request is well formed and the shortfall is the
deployment's — with the classified store error and its `hdtc` remedy going to the log,
not to a public client whose response would otherwise name paths on the server's disk.

**Startup is strict.** A manifest that does not parse, carries a digest that is not a
digest, or declares a version other than its own directory stops the server naming the
path. Dropping that version and serving the rest is the degraded mode doc 20 §20.8
refuses: a silently missing version answers 404 for data that is on disk.

**The workspace acquired its second `unsafe`, and it maps nothing.** `Config` takes a
`PublishedRoot` rather than a path, because `kgf-server` is a library that can be
embedded and a safe `&Path` entry point there would make the mmap immutability promise
on an unknown caller's behalf — the exact unsoundness unit 10's review closed in
`testing`. But someone must make it, and `PublishedBundle::new`/`PublishedRoot::new`
are `pub unsafe` precisely so that someone is outside `kgf-store`. It is
`kgf::serve::published_root`, which canonicalizes the path and cites doc 04 §4.6. That
required `kgf` to grow a lib target beside its bin — which also lets the end-to-end
test drive a real listener without a subprocess — and it is a change to CLAUDE.md's
rule, recorded there rather than made quietly.

**What is *not* written here.** An audit for reinvention, prompted mid-unit, moved
three things onto libraries that were already in axum's tree or close to it:
`percent-encoding` and `mediatype` for RFC 3986 and RFC 9110's grammars, `headers` —
hyperium's — for `ETag`, `If-None-Match`'s §13.1.2 weak comparison and `Cache-Control`,
`jiff` for RFC 3339, and `maud` for the pages. That deleted a hand-transcribed calendar
algorithm, a hand-rolled entity-tag comparison and an HTML escaper. What stayed ours is
what no library in this stack has (`Accept` selection) or what they get deliberately
wrong for this use (below). `kgf-store`'s rank/select and packed arrays look like the
same question and are not: `sucds` and `vers` build their own structures in memory,
while doc 20 §20.1 needs hdtc's *persisted* directories read in place.

Two smaller things the layers took on because unit 14 needs them and they are policy
rather than plumbing. A repeated query parameter is `malformed_request`, since
`?limit=10&limit=99999` has no defensible resolution and server, proxy and client URL
builder each pick a different one. And percent-decoding is strict where the ecosystem
is lossy: `form_urlencoded` and everything on it, including axum's own `Query`, decode
with `decode_utf8_lossy` and pass a malformed escape through, and a term parameter that
lost a byte to U+FFFD is a *different term* that resolves, misses, and is answered "no
rows". The decoding itself is `percent-encoding`'s and the media-range grammar is
`mime`'s; both were already in axum's tree.

Not built, deliberately: `/{dataset}/v/{version}` as a landing page. It would be a
useful thing to browse to, and doc 03 §3.2 does not define it, so inventing URL space
is not this unit's call — the dataset page links straight to `/manifest`, which is
specified and does the job.

There are 161 tests after this unit.

### 14. The four query operations

`/fragment` (GET), `/count`, `/describe`, `/sample`, over units 10–13.

Each is thin by then: parse terms to ids, resolve a `Selection`, page it, serialize.
`/describe` is two enumerations (`out` and `in`) behind one envelope. `/sample` must
enumerate `s ? o`'s bounded predicate result once and sample that request-local set
rather than calling `at` per sample — doc 03 §3.4.7 states the exception and doc 20
§20.2.1 is why.

*Verified by* differential comparison against the store's own answers for every pattern
shape (the store is already differential against `hdtc search`, so this checks the HTTP
layer rather than re-checking the query core), exhaustive paging at adversarial page
sizes through real cursors, and `cardinality` matching enumerated length.

### What M1 is not

**M1 is a strict subset of doc 03 §3.1's mandatory core profile.** That profile is
`fragment` *including QUERY-with-bindings*, `count`, `describe`, and the description
surface `manifest` + `void` + `summary`. Doc 20 §20.8's M1 omits bindings QUERY,
`/void`, and `/summary`, and adds `/sample`, which is an optional capability. So a
deployment at the end of unit 14 answers useful traffic but **cannot claim core-profile
conformance** — that arrives with M2. Worth stating because "the mandatory operations"
and "M1" read like the same set and are not.

## Testing spine

Set up at unit 1 rather than bolted on afterwards. Per doc 20 §20.9 the tests that
matter are differential and property-based:

- **Golden bundles** — tiny fixture RDF checked in, built by hdtc in CI.
- **Differential** — every pattern shape against `hdtc search`; the dictionary against
  `hdtc dump`.
- **Permutation consistency** — `Σₚ count(? p ?) = N = Σₒ count(? ? o)`;
  `count(? p o)` agreeing between POS and OPS; every triple from `? ? ?` found by all
  applicable bound patterns.
- **Paging** — counts equal enumeration lengths under exhaustive paging at
  adversarial page sizes (1, 2, a prime, the cap).
- **Cursors** — resume at every position of every pattern yields exactly the suffix;
  stale digests and foreign-request tokens rejected.

## Decisions recorded here

**Specs are validated at open; views are projected per query.** A `Store` owns
its `Mapping`s, so it cannot also hold views of them — that is a self-referential
struct. It holds `PackedSpec`/`BitmapSpec`/`RankedSpec` instead: validated
offsets and shapes, plain `Copy` data. Projecting a spec onto a mapping is
infallible and costs a bounds compare and a slice.

The reason is not speed. Without the split, every query would re-validate and
`.expect()`, so a malformed bundle would panic on some later request instead of
being refused by `Store::open` with a path and a remedy — which contradicts doc
20 §20.8 and this crate's "shapes validated once" rule. Building a spec reads no
payload, so that validation is bounded independently of bundle size, and the ~30
checks per bundle replace 8–16 per query forever. A projection running from the
region's offset to end-of-file
also hands each view its trailing slack for free, keeping the widened read path
live without a caller ever naming a slack constant.

`Selection<'a>` borrows the `Store` it was resolved against, which makes
"resolved against a different bundle" unrepresentable. Query execution is
synchronous within one blocking task holding an `Arc<Store>` (doc 20 §20.4), so
nothing needs to outlive the borrow; resumption goes through an encoded cursor
token rather than a live `Selection`.

Not done, and deliberately: caching the rank directories' sentinel *value* at
bind time. It would make `count()` free, but it faults a directory page during
open, and rapid startup is worth more than one load.


**The walk over `data.hdt` lives in hdtc, not here.** Composing it from hdtc's section
primitives looked like the doc 20 §20.4 reading, but a mapped reader needs each
*payload's* offset and the `skip_*` forms report only the *section's*, so composing
here would have meant restating "one type byte, a VByte, a CRC8" in this crate — the
drift risk docs 17–18 are about. `hdtc::format::scan_hdt_sections` is the fix §20.4
already prescribes for a missing preamble-only form, and it collapsed hdtc's own second
copy of the walk (in its permutation builder) onto the same code.

**Counts come from the structures, not the header.** `triples()` is `ArrayZ`'s entry
count and `DictCounts` comes from the four PFC preambles. The header agrees in a
well-formed file, and hdtc's builders check that it does — but the header is the one
part of an HDT a rewrite may change, which is why identity digests start past it, and
reading it would mean an N-Triples parser on the open path.

**`BytesSpec` joined the spec family** for the PFC string buffers: bytes whose
interpretation belongs to `dict`, but whose extent should still be validated at open
with the path in hand. Unlike the other projections it ends where the region does,
because a buffer's last block is delimited by the buffer's end.

**`hdt::ForeignBitmap` is gone.** It expressed "this bitmap's directory is in another
file", which `RankedSpec::view(bitmap, directory)` already says precisely.

**Fixtures are built by running the `hdtc` binary**, from `$KGF_HDTC` or the sibling
checkout's `target/`, into a temp dir — doc 20 §20.9's golden bundle, so the bytes under
test are a producer's output rather than this crate's guess at the format. A missing
binary panics with the command to run rather than skipping, since a silently skipped
fixture leaves every differential test passing vacuously.

**Element reads go through `u128` uniformly** rather than splitting a `u64` path for
widths ≤ 57. One code path covers `0..=64` and is correct by construction; the split
would be measurably cheaper on the widths Ubergraph actually uses (25, 16, 11) and is
the obvious first optimization, but it is an optimization, and the house rule is that
those follow a profile. This is the innermost read in the system, so it will get one.

**No `madvise` tuning yet.** Random versus sequential access hints trade against each
other — bindings joins want `RANDOM`, full-page enumeration wants readahead — and
picking without measurement is guessing. Revisit with the §20.6 cold-start numbers.

## Questions for `../kgf`

Things implementing or planning against the design surfaced that the design documents
own. Collected here because they are outbound — each is resolved by an edit in another
repo, not by code here — and because scattered through prose they get lost. CLAUDE.md's
rule applies: when code and spec disagree, say which is wrong rather than silently
following the code.

1. **Doc 20 §20.4's io-primitives bullet is out of date.** It still describes hdtc's
   `skip_*` forms as what locates sections. It sanctioned the change that replaced them
   ("Where hdtc offers only a materializing form, the fix is a preamble-only variant in
   hdtc"), but the bullet now understates the façade; worth a sentence naming the scan
   forms and `scan_hdt_sections`.
2. **Doc 20 §20.5's `Range<TermId>` cannot represent every role prefix.** `dictionaryFour`
   concatenates two independently sorted sections, so a subject or object prefix may
   occupy two disjoint id ranges. The code follows the format (`PrefixBounds` returns up
   to two); the single-range sketch is the bug. Found in unit 4.
3. **Does `data.hdt.index.v1-1` belong in `content_digest`?** Doc 04 §4.1 lists it as a
   conforming optional artifact and doc 20 §20.8 says no server reads it, so it must not
   be refused — but doc 04 §4.3 wants the digest usable for mirror verification, which
   argues every byte a bundle ships should be covered. Today `kgf manifest` excludes it.
   Found in unit 9's review.
4. **Doc 20 §20.7 overstates what a cursor's digest prefix does.** "No-loss/no-duplication
   follows from positional resume against immutable data" is right, but doc 04 §4.6
   already makes versioned URLs immutable, so a client paging `/v/{version}/` cannot
   drift and the digest is not what saves it. The load-bearing parts are `request_hash`
   and the positions that are not scalar offsets. Worth rebalancing the paragraph so the
   next implementer does not infer the wrong priority. See unit 10.
5. **Is M1 being a strict subset of the mandatory core profile intended?** Doc 20 §20.8's
   M1 omits bindings QUERY, `/void`, and `/summary`, all mandatory in doc 03 §3.1, and
   includes `/sample`, which is optional. Almost certainly deliberate — a milestone is
   not a conformance claim — but the two lists read as though they should match.
6. **`hdtc create` does not emit `.perm` unless passed `--perm`,** while doc 04 §4.1
   makes it required. Either `kgf build` always passes the flag or hdtc's default
   changes. Unresolved since unit 5; `testing::Fixture` and every hand-assembly recipe
   pass it explicitly.
7. **What is in the "canonical request" a cursor binds to?** Doc 03 §3.6 says
   "(dataset content digest, operation, canonical request)" and never enumerates the
   third. The pattern parameters must be in it — that binding is the whole reason a
   token beats a bare offset. `limit` must *not* be: a client should be able to change
   page size mid-paging, and a position does not depend on it. Nor should `format`,
   which selects a serialization of the same result set.

   Left to each implementation this is harmless, because a token is opaque and
   server-local. It stops being harmless if a client is meant to resume against a
   **mirror** serving the same bundle — which §3.6's digest binding and doc 04 §4.3's
   "mirror verification" both gesture at — since two independent servers must then
   canonicalize and hash identically. Worth deciding whether cursor portability across
   mirrors is a goal, and if so specifying the canonicalization rather than the
   properties. Surfaced planning unit 10.
8. **Should doc 03 specify `Link: rel="next"`?** §3.6 fixes `next` in the JSON envelope
   and `KGF-Next-Cursor` as a header; RFC 8288 appears nowhere in the docs. On GET
   routes a complete next-page URL is constructible, and `Link: rel="next"` is the
   conventional affordance that generic HTTP clients, crawlers, and libraries follow
   without knowing anything about KGF — a real win for the agent-friendliness doc 01
   argues for.

   It cannot be the *only* mechanism: a QUERY follow-up is a body, not a URL, so the
   bare token stays canonical and the two would coexist. The question is whether doc 03
   specifies it, so clients may rely on it, or leaves it a per-server extra. Surfaced
   planning units 10 and 13.
9. **Doc 03 §3.4.1 does not describe SPARQL Results JSON, though it says it does.** The
   example rows use `{"type": "iri"}` and `"lang"`; SRJ spells those `"uri"` and
   `"xml:lang"`. Both cannot be true, and the format list settles which is meant —
   `srj` is offered *alongside* `json`, so `json` is KGF's own envelope and the
   sentence "term encoding matches SPARQL Results JSON" is the loose one. Worth
   softening it to "mirrors the shape of", since a client that takes it literally will
   write a parser that never matches. The code follows the examples; `format=srj` will
   be a second serialization in unit 14, not a rename of this one. Found in unit 11.
10. **§3.3 requires angle brackets on IRIs. Resolved — `../kgf` 47d1574.** §3.3 used to
    accept a bare IRI alongside a CURIE and disambiguate by guessing: "a token parses as
    a CURIE only when its prefix is declared in the manifest prefix map; otherwise as an
    IRI". A syntax that admits both forms without a delimiter has to guess, and no guess
    is right:

    - The same string denotes different terms at different endpoints, since the reading
      depends on the manifest of the bundle it was sent to. `dc:title` is a CURIE at one
      KG and the IRI `dc:title` at the next — and federated clients (doc 05) send the
      same pattern to many.
    - A bundle declaring a prefix that collides with a scheme has IRIs no request can
      name. §3.3 can only advise against declaring one; the advice does not help a client
      facing a bundle that already did.
    - A misspelled prefix silently becomes a URI scheme. `foaf:name` with `foaf`
      undeclared is answered as the IRI `foaf:name` — "no such term", for a request whose
      real problem was a typo.

    The fix is Turtle's and SPARQL's, and it is what §3.3 now says and `kgf-server`
    implements: `<http://…>` is an IRI, `p:local` is a CURIE whose prefix must be
    declared, and an undeclared prefix is `bad_term_syntax` naming both remedies. It
    applies to `^^datatype`, the other slot taking either form. Cost: `%3C`/`%3E` in a
    GET URL, on top of the escaping §3.3 already requires — cheap for removing every case
    above. §3.4's examples were almost all CURIEs and were unaffected; the collision
    advice is gone, since brackets remove the hazard rather than warn about it, and
    `/terms?prefix=` is called out as a byte prefix rather than a term so it stays bare.
    Decided and landed in unit 11.
11. **Are literals escaped in request syntax?** §3.3 gives `"Diabetes mellitus"@en` and
    never says what a quote inside a value looks like. HDT stores values raw and finds
    the closing quote from the end (`hdtc/docs/text-index-format.md` §3.1), and matching
    that is the only rule under which a term can be copied out of a response and pasted
    back into a request. But it means `\"` is two literal characters, which will surprise
    anyone arriving from N-Triples, so it should be stated rather than inferred. Found in
    unit 11.
12. **Blank nodes are absent from §3.3.** The dictionary stores them, `/fragment` returns
    them, and doc 09 §9 discusses them as reifiers — but the term syntax section lists
    IRIs, literals and variables only, so `_:b1` would parse as an IRI under the letter
    of the rules. Treating a leading `_:` as a blank node is the only reading that lets a
    client ask about a term the server just returned. Worth one line in §3.3. Found in
    unit 11.
13. **Does the JSON term-object form expand CURIEs?** §3.3 offers it as "the canonical
    form for terms that are awkward to escape", and it is described as the form "as in
    response rows", where IRIs are always full — so this implementation treats a term
    object's `value` as a full IRI and does not consult the prefix map. The opposite
    reading would put the ambiguity back into the form that exists to escape it, but
    §3.3 does not say. Found in unit 11.
14. **Is `truncation_reason` present-and-null on a complete response?** §3.4.1's example
    shows `"complete": true, "next": null` — `next` present as null — and omits
    `truncation_reason` entirely, while §3.6 lists both as things "JSON envelopes carry".
    This implementation follows the example: `next` always present, `truncation_reason`
    only when there is one. That asymmetry is awkward for a typed client, which now has
    one nullable field and one optional one for the same condition; either rule is fine,
    but §3.6 should state it rather than leave it to be read off an example. Two more
    §3.6 fields, `scanned` and `returned`, are called "optional" without saying optional
    *when* — presumably present on budgeted scans and absent otherwise, which is M2's
    problem but the same sentence. Found in unit 12.
15. **The error-code set is closed. Resolved — `../kgf` §3.6.1.** §3.6 used to give
    `capability_not_available`, `cap_exceeded`, `bad_term_syntax` and an ellipsis, which
    defeats the point: an agent can only self-correct against a vocabulary it knows in
    advance. §3.6.1 is now a normative table of nine codes with a status and a client
    remedy each, and `kgf-server` has a test that transcribes it.

    Three decisions inside it. **One code, one status** — a condition needing a different
    status is a different code, which is why failing content negotiation is three codes
    (`unsupported_format` 400 for `format=`, `not_acceptable` 406 for `Accept`,
    `unsupported_media_type` 415 for a request body) rather than one carrying a status
    beside it. **`capability_not_available` is 501**, not 4xx: the request is well formed
    and the identical request against a bundle publishing the capability succeeds, so the
    shortfall is the server's. And **`type` is `about:blank` with the status reason phrase
    as `title`**, which RFC 9457 §4.2.1 requires of that pairing — a KGF-specific title
    would be a conformance bug, and minting `https://…/problems/{code}` URIs would claim
    a namespace nothing serves.

    The table's first draft said 429 and 5xx carry no code, which left the server unable
    to answer a rate limit or its own failure as `problem+json` at all — §3.6 requires
    RFC 9457 for errors, and a type deriving its status from a code cannot express a
    codeless one. `rate_limited` and `internal_error` close that, and earn their codes:
    they tell a client the one thing the status leaves ambiguous, whether retrying the
    identical request is worth anything. Decided and landed in unit 12.
16. **Where does `current` come from, if not the bundle manifests?** Doc 04 §4.3 puts
    `current` and the release history in the *dataset descriptor*, and calls it mutable
    and host-independent — but nothing in the toolchain writes one, and a deployment is
    a directory of bundles. This server therefore derives `/{dataset}` from the
    manifests and takes `current` as the greatest `created`, which works and is
    testable, but it is the server guessing at the operator's intent: an operator who
    wants to publish a new version *without* making it current cannot say so, and one
    who re-releases an old version cannot promote it.

    The derivation also cannot reach the fields that are not in a bundle manifest at
    all — `preservation`, `authoritative_namespaces`, and doc 19 §19.1's predicate role
    declarations, which §4.3 calls "the federation's cheapest machine-actionable schema
    documentation". Those are absent from what this server publishes today.

    So: should a host supply a dataset descriptor file (and `kgf` grow a command to
    write one), with derivation as the unconfigured default? That is two sources for
    one document, which the one-implementation rule is suspicious of — but the two
    answer different questions, and only one of them can carry an operator's intent.
    Surfaced in unit 13.
17. **Should `/{dataset}/v/{version}` be a resource?** §3.2 defines the prefix but not
    the URL, so a browser that trims a path lands on a 404 and an agent has no
    per-version index. It would be the natural page to list the operations a version
    supports, which is exactly what its manifest's `capabilities` says. Not invented
    here; worth one line in §3.2 either way. Surfaced in unit 13.
18. **Is a `latest` redirect's `Location` allowed to be relative?** §3.2 says 307/308
    and nothing about the target's form. This server sends an absolute-path reference
    (`/{dataset}/v/{version}/…`), which RFC 9110 permits and which is the only form a
    server behind an unknown proxy can produce correctly — it is not told its own
    public origin. Same question for the `url` field of a derived dataset descriptor,
    where doc 04 §4.3's example shows an absolute URL. Worth stating, since a client
    resolving one against the request URI gets the right answer and a client expecting
    an absolute URL does not. Surfaced in unit 13.

## Not in this plan

Composed operations (`/search`, `/labels`, ranges, star, key resolution), graph
scoping, bindings QUERY, and everything gated on a sidecar beyond `.perm`. Those are
doc 20 §20.8's M2 and M3, and they compose through the `Store` and the envelope,
cursor, and term layers this plan builds.

`kgf build` is also absent, deliberately. Bundles are assembled with
`hdtc create --perm` and described with `kgf manifest` (unit 9) until the server work
is far enough along to say what the build pipeline owes it.
