# Where kgf-rs stands — 2026-08-01 (unit 9 landed)

A point-in-time handoff. `CLAUDE.md` has the conventions and the design rules,
`notes/plan.md` has the unit-by-unit route through M1 plus the decisions and the open
questions for the design docs, `../kgf/docs/20-read-layer.md` is the spec. This file is
the part that lives in none of them: what is actually built, what was learned building
it, and what is deliberately left open.

Written at a point in time and not maintained afterwards — where it and `plan.md`
disagree about what exists, `plan.md` and the code win.

## The three repositories

```
Source/
  kgf/       # design documents 01–20. Normative. This repo implements doc 20.
  hdtc/      # the HDT toolchain. A path dependency; owns every byte format.
  kgf-rs/    # this repo
```

All three are Jim's. hdtc is github.com/frink-okn/hdtc; kgf-rs has **no remote yet**.

| repo | branch | status |
|---|---|---|
| `kgf-rs` | `main` | no remote; units 1–8 implemented |
| `kgf` | `main` | 1 commit ahead of `origin/main` |
| `hdtc` | `lib` | unit 3 is `0a31692`; unit 5 is `8cba61c` plus review fix `48f90f3` |

**kgf-rs depends on hdtc through `48f90f3`.** Unit 3's scan forms are in `0a31692`;
unit 5's typed section identifiers and rank geometry are in `8cba61c`, followed by
classified open errors in `48f90f3` that preserve binding-error semantics.

## What is built

All nine units from `notes/plan.md`, complete with tests. 84 tests, ~9 s,
clean under `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
`cargo doc` with no warnings.

| module | state |
|---|---|
| `map.rs` | **done** — `Mapping`, `PackedSpec`/`PackedArray`, `BitmapSpec`/`BitmapView`, `BytesSpec` |
| `rank.rs` | **done** — `RankedSpec`/`RankedBitmap`: `rank1`, `select1`, `count` |
| `hdt.rs` | **done** — `HdtLayout`/`TriplesLayout` plus shared `BitmapTriples` traversal |
| `dict.rs` | **done** — mapped PFC `locate`/`extract`/`prefix_bounds`, role/shared arithmetic |
| `perm.rs` | **done** — mapped POS/OPS and cross-file SPO rank binding; shared `BitmapTriples` projections |
| `pattern.rs` | **done** — eight patterns, exact counts, positional paging/`at`, dual-route `s ? o` |
| `store.rs` | **done** — required-artifact policy, immutable mapped store, cheap sidecar binding |
| `catalog.rs` | **done** — lazy sorted catalog, singleflight opens, cached failures, Arc eviction |
| `testing.rs` | shared test support (`Rng`, `bit`, golden ids/bundle, independent hdtc search) |
| `manifest.rs` | **done** — doc 04 §4.3 parse/serialize, `BundleFacts`, counts cross-check |
| `error.rs` | **done for the query core** — structural, binding, lookup, lazy-open, manifest context |
| `kgf` binary | `kgf manifest` implemented; `kgf serve` is `todo!()` behind a real CLI |
| `kgf-server` | skeleton: real signatures and doc comments, `todo!()` bodies |

`todo!()` is a convention here, not an oversight: an unimplemented path panics rather
than returning a plausible wrong answer. Do not replace one with a default-returning
stub.

### The architectural decision that shaped every module

A `Store` owns its `Mapping`s and therefore **cannot hold views of them** — that would
be a self-referential struct. So the crate splits *validated shape* from *projection*:

- A **spec** (`PackedSpec`, `BitmapSpec`, `BytesSpec`, `RankedSpec`) is validated
  offsets, lengths, widths, and derived arithmetic. Plain `Copy` data, `Send + Sync` by
  construction. Built **once at open**, where the file path is in hand for the error
  message. Building one reads **no payload byte**, so this validation remains bounded
  independently of bundle size.
- A **view** (`PackedArray`, `BitmapView`, a `&[u8]`, `RankedBitmap`) is a spec projected
  onto a mapping. Projection is **infallible** — a bounds compare and a slice.

Unit 3 is the first non-test use: a `*Layout` is a bundle of specs, one per located
region, and `HdtLayout::parse` is where roughly thirty checks happen instead of a dozen
per query forever.

The reason is not speed. Without the split every query re-validates and `.expect()`s,
so a malformed bundle panics on the ten-thousandth request instead of being refused by
`Store::open` with a path and a remedy — contradicting doc 20 §20.8.

Consequences worth knowing before writing units 6–8:

- **`Selection<'a>` borrows the `Store`.** Query execution is synchronous inside one
  blocking task holding an `Arc<Store>` (doc 20 §20.4), so nothing outlives the
  borrow, and resumption is an encoded cursor token rather than a live `Selection`.
- **Projections run to end-of-file**, not to the end of the region. This hands every
  view its trailing slack for free, so `PackedArray`'s widened 16-byte read stays on
  its fast path without any caller naming a slack constant. `BytesSpec` is the one
  exception — a PFC string buffer's last block is delimited by the buffer's end, so that
  projection is exact.
- **`RankedSpec::view` takes two mappings.** The SPO bitmaps live in `data.hdt` while
  their rank directories ride in `data.hdt.perm`. POS and OPS pass the same mapping
  twice. There is a test over two real files for the cross-file case.
- **`Permutations` owns both mappings and the `HdtLayout`.** That keeps the HDT-side
  specs and sidecar-side directories coupled to the exact mappings they were validated
  against. `Store` reaches the dictionary through this owner rather than holding a
  second copy of the HDT mapping.
- `map` is the only module allowed `unsafe`; every crate carries
  `#![deny(unsafe_code)]` and `map` the single `#[allow]`. The soundness argument —
  published bundle versions are immutable, so no writer exists for a mapped file — is
  written in the module and must stay true.

### What unit 3 added to hdtc

The façade had no preamble-only form that reports a *payload's* offset — `skip_*`
reports where a section starts, and deriving the payload from that means restating "type
byte, VByte, CRC8" in this crate. So `hdtc::format` grew, all preamble-only:

| addition | what it gives a mapped reader |
|---|---|
| `scan_bitmap_section` → `BitmapSection` | section start, **payload start**, payload length, bit count, section end |
| `scan_log_array_section` → `LogArraySection` | the same plus entry count and width |
| `scan_pfc_section` → `PfcSection` | term count, block size, the block-offset `LogArraySection`, buffer start and length |
| `scan_hdt_sections` → `HdtSections` | the whole walk: header offsets, four PFC sections, `BitmapY`/`BitmapZ`/`ArrayY`/`ArrayZ` |
| `packed_len` | the bits→bytes rounding rule, no longer duplicated here |
| `DICTIONARY_FOUR_FORMAT`, `TRIPLES_BITMAP_FORMAT`, `TRIPLES_ORDER_SPO` | the format URIs the walk checks |

The scan forms verify every preamble CRC8 (the old `skip_*` forms read that byte and
discarded it) and never touch a payload CRC32C. `skip_*` and `skip_pfc_section` are now
thin wrappers over them, so `skip_pfc_section` no longer materializes a
dictionary-sized offset array for its callers either.

Inside hdtc this collapsed a second copy of the same walk: `permutation/builder.rs`'s
`scan_hdt` had private `read_bitmap`/`read_array`/`skip_pfc_section` helpers, and now
calls `scan_hdt_sections` and adds only what sidecar construction needs. The format URI
constants, previously declared in four modules, moved to `hdt/sections.rs`. hdtc's
`tests/format_api_test.rs` gained the HDT-side contract test; all 293 lib tests plus
every integration test pass.

## Things learned the hard way

**`hdtc create` does not emit `.perm` unless you pass `--perm`.** Doc 04 now makes
`data.hdt.perm` a *required* bundle artifact, so either `kgf build` always passes the
flag or hdtc's default changes. Unresolved; `testing::Fixture` passes the flag.

**Never call hdtc's materializing readers on the open path.** `LogArrayReader::read_from`
and `PfcSectionHeader::read_from` read their entire payload into a `Vec` and verify
CRCs — correct for a bounded-memory CLI, disqualifying here. On Ubergraph the object
dictionary's block-offset array alone is 674 437 entries (and its buffer 276 MB). Use
the **scan forms** above. If hdtc offers only a materializing form for something
needed, **add a preamble-only variant to hdtc** rather than reimplementing the parse
here (doc 20 §20.4 says so explicitly).

**Locating sections in an HDT is a preamble walk, not a scan** — every section declares
its own size, so the whole walk is about a dozen small reads. Measured on Ubergraph
(2 504 078 921 bytes, N = 606 342 307): **map 88 µs, parse 2.5 ms**, which is the dozen
page faults and nothing else. HDT section discovery therefore has fixed metadata I/O;
the complete store open additionally reads a fixed number of rank sentinels and still
does no size-dependent scan.

**The HDT header is not a source of truth.** `triples()` is `ArrayZ`'s entry count and
`DictCounts` comes from the four PFC preambles, because a header rewrite may change the
header (identity digests start past it) and parsing its N-Triples would put an RDF
parser on the open path. Ubergraph's structures report exactly doc 20 §20.3's numbers:
S = 9 480 192 + 8 178 910 = 17 659 102, P = 1 251, O = 9 480 192 + 10 790 972 =
20 271 164, `ArrayZ` 25 bits, `ArrayY` 11 bits, n_sp = 118 393 685.

**Test oracles must be linear.** The first version of the rank tests was quadratic and
took 34 s; rewritten around a single-pass prefix table it is 0.2 s. The oracles still
share no code with the implementation — that is the property that matters, and it does
not require them to be slow.

## The measurement that froze the format

`hdtc perm` over Ubergraph (in `../hdtc/ubergraph2/`), N = 606 342 307 triples:

| artifact | bytes |
|---|---:|
| `ubergraph.hdt` | 2 504 078 921 |
| `ubergraph.hdt.perm` | 4 097 720 192 (**1.64×** the HDT) |
| `ubergraph.hdt.graphs` | 257 619 840 |
| `ubergraph.hdt.graphs.idx` | 237 700 352 |

`ArrayZ` × 2 is 92.5% of the sidecar; rank directories are 0.29%. Every region matches
doc 20 §20.3's closed form to the byte. This closed both open Phase 0 questions — the
FoQ fallback is withdrawn and delta-block encoding declined — so `.perm` v1 is frozen
and this is the real bundle for mapped-reader measurements.

One measurement still outstanding: `hdtc index` on the same file, to pin the FoQ
comparison exactly rather than at the estimated ~1.4–1.6×. About 4 minutes, writes
~2.5 GB.

## Open, deliberately

Recorded in `notes/plan.md` under "Decisions recorded here" unless noted:

- **`u128` vs `u64` element reads.** Uniform `u128` covers widths 0..=64 in one path.
  A `u64` path for widths ≤ 57 would cover every width Ubergraph actually uses (25, 16,
  11) and is the obvious first optimization — deferred pending a profile, and this is
  the innermost read in the system so it will get one.
- **No `madvise` tuning.** Random and sequential hints trade against each other;
  revisit with doc 20 §20.6's cold-start numbers.
- ~~**`packed_bytes` duplicates hdtc's private `packed_len`.**~~ Closed: the scan forms
  needed it too, so `packed_len` is in `hdtc::format` and `map::packed_bytes` delegates,
  keeping only this crate's error vocabulary.
- **`Error::Region` is stringly-typed.** Now defensible: spec constructors have the
  mapping and so the path, which is the wrapping mechanism its doc always described.
  Revisit if the variants multiply.
- **`BitmapView::count_ones_in` clamps while `get` asserts** — two out-of-range
  policies on one type. Clamping is documented and tested; consistency is arguable.
- **Tests shell out to the `hdtc` binary** to build fixtures (`testing::Fixture`), found
  via `$KGF_HDTC` or `../hdtc/target/{release,debug}/hdtc`. The alternatives were both
  worse: committing a golden `.hdt` deviates from doc 20 §20.9 (fixture *RDF* is checked
  in, CI builds the bundles), and a programmatic builder is deliberately absent from
  hdtc's façade. Revisit when there is CI to build them in.
- **hdtc gaps, not on this crate's path:** no sketch probe API, no key-set intersect
  (`../kgf` doc 07 §7.5 items 18–19). hdtc work when the operations that need them
  arrive; do not fill them in here.

## Unit 4: mapped dictionary access

`DictionaryLayout::view` projects the four PFC sections without materializing their
block-offset arrays. `locate` binary-searches block heads then decodes one block per
searched section; `extract` decodes only the addressed block into a caller buffer;
`prefix_bounds` uses the same search. All shared/role-only id arithmetic lives in
`dict`.

The differential test uses hdtc's independent sequential PFC reader as the oracle and
compares every id and term across multiple blocks. There are 35 tests after this unit.

The implementation exposed a bug in doc 20 §20.5's indicative signature:
`Range<TermId>` cannot represent every subject/object prefix. Their ids concatenate
shared and role-only sections, which are individually sorted but not jointly sorted, so
a prefix may have one disjoint range in each. `PrefixBounds` returns up to two ranges
and an exact count. `/terms` will merge those two sorted runs; the cost remains
`O(log D + limit)`.

## Unit 5: mapped permutation sidecar

`Permutations::open` binds one mapped HDT to one mapped `.hdt.perm`. hdtc parses and
validates the header/directory and cheap source metadata; `kgf-store` turns its region
descriptors into specs without using hdtc's seek-based triples reader. POS and OPS use
the sidecar for data and directories. SPO uses `data.hdt` for data and the sidecar for
directories. `pos()`, `ops()`, and `spo()` all project to `BitmapTriples`.

The façade now exports typed `PermutationComponent`/`PermutationSectionKind` values
and the header's rank geometry, so this crate contains no copied section numbers or
assumed block widths. The golden-bundle tests cover region shapes, id ranges, bitmap
populations, all three projections, a foreign sidecar, and truncation. There are 39
tests after this unit.

## Unit 6: shared BitmapTriples traversal

`BitmapTriples` now derives level-2 and level-3 group ranges with select, recovers
their owners with rank, binary-searches either sorted packed level, and reads both
packed values directly. The same implementation serves SPO, POS, and OPS.

The plan's original seven primitives omitted `level2_at`: `level2_of(z_position)`
recovers the owning `ArrayY` position, but materializing an unbound triple also needs
the value at that position. The symmetric accessor is now the eighth primitive. This
is an implementation-plan correction; the format and doc 20 enumeration contract are
unchanged.

The golden-bundle test reconstructs all three projections from dictionary-resolved
fixture ids and checks every group boundary, reverse mapping, value read, binary-search
hit/miss, and final traversal order. There are 40 tests after this unit.

## Unit 7: pattern selection

`resolve` now implements doc 20 §20.2's complete dispatch table. A `Selection` holds
either one contiguous range in SPO/POS/OPS or an `s ? o` group plan, all borrowing the
bundle they were resolved against. Bound ids are checked in their role spaces before
descent. Counts are exact; paging and `at` materialize ids directly from mapped views.

Contiguous cursors are result offsets. `s ? o` instead resumes after the last predicate
id returned, which is stable across its SPO and OPS routes. The route probes the lower
degree endpoint; when that endpoint's complete packed range occupies at most two pages
its groups scan linearly, otherwise every group binary-searches. Count and random
access share that bounded probe because this is the one non-contiguous pattern. Docs
03 and 20 state the exception explicitly and require `/sample` to enumerate this tiny
predicate result once instead of calling `at` repeatedly.

All valid bound/unbound combinations in the golden graph are checked at adversarial
page sizes and every resume position. All eight representative shapes also agree with
hdtc's independent search path. Predicate/object count sums and POS/OPS pair counts
also close independently. There are 45 tests after this unit.

## Unit 8: store and lazy catalog

`Store::open` now enforces doc 04's bundle boundary: `manifest.json`, `data.hdt`, and
`data.hdt.perm` are required, and the graphs sidecar and its graph index must occur
together. Missing artifacts name `kgf build`, `hdtc perm`, or `hdtc graphs-index`;
malformed and cross-HDT sidecars keep their classified context. Full digests and CRCs
remain off the latency-sensitive open path as doc 20 §20.6 requires. `OpenOptions` is a reserved
empty type rather than carrying the earlier, contradictory checksum flag.

The store maps through `map::open_published`, keeping the production unsafe block in
the one audited module. Public `PublishedBundle`/`PublishedRoot` capabilities make the
published-version immutability premise explicit before safe store or catalog APIs can
map anything. It owns the capability, two core mappings, and validated specs; every
read remains an immutable projection with no cache, lock, or interior mutability.

`Catalog::scan` records UTF-8 `{dataset}/{version}` directories in deterministic order
and opens nothing. Entry-local state provides singleflight on both successful and
failed opens; failures retain their classified `Store::open` source because published
versions cannot repair themselves in place. `evict` resets the entry, prevents an
older in-progress open from recaching itself, and drops only the catalog's `Arc`.

Tests cover the required-artifact matrix, cross-HDT refusal, lazy discovery, unknown
ids, shared first-open identity, cached failures, eviction with an in-flight clone,
and 8 threads over 6 bundles for 80 mixed-pattern iterations under concurrent
eviction. The local 6.6 GiB four-artifact Ubergraph bundle opened in about 12 ms and
reported 606,342,307 triples. There are 53 tests after this unit.

## Unit 9: the bundle manifest

`kgf_store::manifest` owns doc 04 §4.3's document; `kgf manifest` writes one for a
bundle assembled by hand. This unblocks server work without `kgf build`: bundles are
now built with `hdtc create --perm` and described with `kgf manifest`.

```sh
hdtc create input.nt -o bundles/demo-kg/2026-08-01/data.hdt --perm
kgf manifest bundles/demo-kg/2026-08-01 --prefix ex=http://example.org/
kgf manifest bundles/demo-kg/2026-08-01 --check
```

`--id` and `--version` default to the `{dataset}/{version}` path components the catalog
already requires, and every descriptive field is re-read from the manifest already
present, so regenerating after a rebuild takes no flags. `--check` is what to run after
touching artifacts; a stale manifest names the field that disagrees and the command
that repairs it.

`Store::open` still does not parse the manifest — it requires the file and stops — so
the store stays testable headless and the `kgf-store` tests keep their `{}`
placeholder. The parse is `Manifest::read`, for the server to call.

**Where verification happens is split, deliberately.** `Manifest::verify_against` is
store-side and compares counts only, because full digests are off the open path by
design (doc 20 §20.6). That is too weak on its own: editing one literal leaves all four
counts identical — the file length too, in the case that found this — and rewrites
every byte. So `kgf manifest --check` additionally recomputes every checksum and the
content digest, and compares the capability and artifact sets. The CLI already hashes
everything to write a manifest; hashing to check one costs nothing new and is not on
any latency path.

**Rewriting is not reading.** `ManifestDocument` holds the raw JSON beside the parse so
a regeneration preserves fields this build does not model — `source`, `components`, a
capability's configuration body, anything newer. An unparseable document is still
writable (`{}` is every bundle's first manifest), but one declaring a schema this build
cannot read is refused before anything is written, since overwriting a newer manifest
with an older one loses more than it repairs.

**`BundleFacts::read` makes every check `Store::open` makes** except the manifest's
existence, sharing `ArtifactSet` and its graph-index binding check. A manifest
describing a bundle that then refuses to open would be worse than describing nothing.

**The one place in KGF that reads whole artifacts is the binary.** `kgf-store` never
hashes a file: `content_digest_preimage` fixes the Merkle recipe and returns the
preimage, and the binary hashes it. `kgf build` must reproduce that recipe.

**One gap left open on purpose.** `artifact_names_for` knows the four artifacts that
have producers; doc 04 §4.1 reserves nine more (`text/`, `labels/`, `ranges/`,
`closures/`, `reif/`, `geo/`, `vectors/`, `filters/`, `stats/`). Each must be added
there when it lands or it falls out of `content_digest`. Refusing unknown files is not
the fix — `data.hdt.index.v1-1` is a conforming artifact no server reads (doc 04 §4.1,
doc 20 §20.8) — but whether it should still be checksummed, since doc 04 §4.3 wants the
digest usable for mirror verification, is a question for `../kgf`.

## Next

`kgf-store` is complete for M1. What remains is M1's HTTP surface, planned as units
10–14 in `notes/plan.md`: the cursor codec, term syntax, the response envelope, the HTTP
skeleton, and the four query operations. That file has the ordering and the reasoning;
this section records only what a fresh session should know before opening it.

**The stack is unchosen** — no `tokio`, `axum`, or `hyper` in the workspace. Unit 13
decides it, and the trap is deferring: M1 has no body-carrying route, so a stack that
cannot express HTTP QUERY looks fine until bindings QUERY arrives in M2.

**Start with unit 10, the cursor codec.** Pure, no I/O, and the store side that makes
doc 20 §20.9's "resume at every position yields exactly the suffix" assertable is
already built and tested. Read that unit before the module's own doc comments, which
overstate what `digest_prefix` does.

**M1 is not core-profile conformance.** Doc 20 §20.8's M1 omits bindings QUERY,
`/void`, and `/summary` — all mandatory in doc 03 §3.1 — and includes the optional
`/sample`. A deployment at the end of unit 14 serves useful traffic and cannot claim
the profile.

Outbound spec questions — from the nine units built and from planning the five ahead —
are collected under **Questions for `../kgf`** in `notes/plan.md`, including the two
long-standing ones (doc 20 §20.4's stale io-primitives bullet, and `hdtc create` not
defaulting to `--perm`). Two of them block nothing but should be settled before unit 10
writes a token format: what the "canonical request" a cursor binds to actually includes,
and whether cursor portability across mirrors is a goal.

One item that is neither: hdtc still has three private copies of the section walk —
`hdt/reader.rs::open_hdt`, `hdt/input_adapter.rs`, and `index/mod.rs`'s own `skip_*`
trio. None are on KGF's path, so unit 3 left them alone; consolidating them onto the
scan forms is hdtc hygiene, not KGF work.
