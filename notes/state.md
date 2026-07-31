# Where kgf-rs stands — 2026-07-31 (unit 5 landed)

A point-in-time handoff. `CLAUDE.md` has the conventions and the design rules,
`notes/plan.md` has the route and the recorded decisions, `../kgf/docs/20-read-layer.md`
is the spec. This file is the part that lives in neither: what is actually built, what
was learned building it, and what is deliberately left open.

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
| `kgf-rs` | `main` | no remote; units 1–5 implemented |
| `kgf` | `main` | 2 commits ahead of `origin/main` |
| `hdtc` | `lib` | unit 3 is `0a31692`; unit 5 is `8cba61c` plus review fix `48f90f3` |

**kgf-rs depends on hdtc through `48f90f3`.** Unit 3's scan forms are in `0a31692`;
unit 5's typed section identifiers and rank geometry are in `8cba61c`, followed by
classified open errors in `48f90f3` that preserve binding-error semantics.

## What is built

Five of eight units from `notes/plan.md`, all complete with tests. 39 tests, ~2 s,
clean under `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
`cargo doc` with no warnings.

| module | state |
|---|---|
| `map.rs` | **done** — `Mapping`, `PackedSpec`/`PackedArray`, `BitmapSpec`/`BitmapView`, `BytesSpec` |
| `rank.rs` | **done** — `RankedSpec`/`RankedBitmap`: `rank1`, `select1`, `count` |
| `hdt.rs` | `HdtLayout::parse` + `TriplesLayout` **done**; `BitmapTriples`' traversal is unit 6 |
| `dict.rs` | **done** — mapped PFC `locate`/`extract`/`prefix_bounds`, role/shared arithmetic |
| `perm.rs` | **done** — mapped POS/OPS and cross-file SPO rank binding; shared `BitmapTriples` projections |
| `testing.rs` | shared test support (`Rng`, `bit`, `TINY_NT`, `Fixture`) |
| `error.rs` | complete enough to build on |
| everything else | skeleton: real signatures and doc comments, `todo!()` bodies |

`todo!()` is a convention here, not an oversight: an unimplemented path panics rather
than returning a plausible wrong answer. Do not replace one with a default-returning
stub.

### The architectural decision that shaped every module

A `Store` owns its `Mapping`s and therefore **cannot hold views of them** — that would
be a self-referential struct. So the crate splits *validated shape* from *projection*:

- A **spec** (`PackedSpec`, `BitmapSpec`, `BytesSpec`, `RankedSpec`) is validated
  offsets, lengths, widths, and derived arithmetic. Plain `Copy` data, `Send + Sync` by
  construction. Built **once at open**, where the file path is in hand for the error
  message. Building one reads **no payload byte**, so open stays header-only.
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
page faults and nothing else. "Open is free" holds for `data.hdt` as well as the
sidecar.

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

## Next

Unit 6, `hdt::BitmapTriples`: implement the seven shared traversal primitives over
the SPO/POS/OPS projections. This is the first point where the mapped structures
answer triple patterns in id space and enables the Ubergraph cold-start measurement.

Two smaller unit-3 follow-ups remain:

- `../kgf` doc 20 §20.4's io-primitives bullet still describes hdtc's `skip_*` forms as
  what locates sections. It sanctioned the change that replaced them ("Where hdtc offers
  only a materializing form, the fix is a preamble-only variant in hdtc"), but the bullet
  is now an understatement of the façade; worth a sentence naming the scan forms and
  `scan_hdt_sections`.
- hdtc still has three further private copies of the section walk —
  `hdt/reader.rs::open_hdt`, `hdt/input_adapter.rs`, and `index/mod.rs`'s own `skip_*`
  trio. None are on KGF's path, so unit 3 left them alone; consolidating them onto the
  scan forms is hdtc hygiene, not KGF work.
