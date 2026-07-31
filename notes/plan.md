# kgf-store implementation plan

The route from the current skeleton to a working query core: mapped `data.hdt` +
`data.hdt.perm`, all eight patterns at doc 20 §20.2's bounds, exact counts, stable
positional cursors. Spec references are to `../kgf/docs`.

Ordered so that every unit is verifiable when it lands rather than at the end. The
first two touch nothing external — no hdtc, no files, no I/O — which is where the
bit-twiddling everything else trusts belongs.

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

### 6. `hdt::BitmapTriples` — the shared traversal

`level2_range`, `level3_range`, `find_level2`, `find_level3`, `level1_of`,
`level2_of`, `level3_at`. One implementation over all three permutations, which is
possible because all three have implicit level 1 and the same two-level shape.

**This is the real milestone.** At this point the store answers patterns over a
606 M-triple bundle in id space, and doc 20 §20.6's cold-start-with-N-bundles
measurement becomes possible. Everything after it is contract work rather than
discovery.

### 7. `pattern` — the §20.2 table

Eight patterns to `Selection`; exact counts by rank difference; `page`; `at` for
`/sample`. `s ? o` gets its dual route, choosing on `min(deg(s), deg(o))` — both
degrees are rank differences, so choosing is cheap — with linear scan inside a route
for ranges spanning a page or two and binary search above that.

The enumeration order fixed here is a contract: cursors are positions in it.

*Verified by* differential comparison against `hdtc search` for all eight shapes, plus
the §20.9 consistency properties.

### 8. `store` and `catalog`

Required-artifact checks with errors that name the command to run, cheap binding
verification, `Arc<Store>`, lazy open with a singleflight guard, eviction by dropping
the `Arc`.

*Verified by* opening `ubergraph2` and by an N-threads × M-bundles stress under
eviction.

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
payload, so open stays header-only, and the ~30 checks per bundle replace 8–16
per query forever. A projection running from the region's offset to end-of-file
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

## Not in this plan

Composed operations (`/search`, `/labels`, ranges, star, key resolution), graph
scoping, and everything gated on a sidecar beyond `.perm`. Those are doc 20 §20.8's
M2 and M3, and they compose through the `Store` this plan builds.
