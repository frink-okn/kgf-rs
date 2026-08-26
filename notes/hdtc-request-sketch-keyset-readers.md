# hdtc feature request: header readers for `filters/` and `keysets/`

**Repo:** `hdtc` (github.com/frink-okn/hdtc), at `1.2.0-beta.2` / `087d7a1`.
**Requested by:** kgf-rs, `kgf build bundle` and `kgf manifest`.
**Normative:** `docs/sketch-format.md` §4–§6, `docs/keyset-format.md` §4;
KGF `docs/17-sketch-convention.md` §17.3, `docs/18-exact-membership-and-overlap.md`
§18.4.
**Size:** one reader per family exposing the header fields, plus façade
re-exports and contract-test coverage. No payload *decoding*, no probe, no
intersect.

## The gap

`hdtc sketch` and `hdtc keyset` write `filters/{subjects,objects}.{filter,minhash}`
and `keysets/{subjects-only,objects-only,shared}.keys`. Both modules are
**write-only**:

| file | public surface today |
|---|---|
| `src/hdt/sketch.rs` | `SketchConfig` (:49), `SketchSummary` (:61), `Role` (:69), `create_sketches` (:193) |
| `src/hdt/keyset.rs` | `KeysetConfig` (:50), `KeyRole` (:61), `KeysetEncoding` (:122), `KeysetRoleSummary` (:147), `KeysetSummary` (:170), `create_keysets` (:177) |

Both are `pub(crate) mod` in `src/hdt/mod.rs` (lines 7 and 13) and neither
appears in `src/format.rs`. So there is **no supported way to read back a byte
of a file hdtc just wrote** — not the `convention_id`, not the `key_count`, not
the `role`. The builder returns summaries, but a summary is in memory in the
process that built the file; it is gone by the time anything reads the bundle.

The formats themselves are fully specified and self-describing —
`docs/sketch-format.md` §4's 56-byte envelope and `docs/keyset-format.md` §4.1's
96-byte header carry everything needed — which is what makes this a
missing-accessor request rather than a format change.

## Why kgf-rs needs it

### 1. The manifest entries the KGF profile requires

KGF doc 18 §18.4, defining the `kgf-keyset/1` profile:

> …and **a manifest entry per file** carrying `convention_id`, `format_version`,
> `role`, `encoding`, `key_count`, byte size and checksum, **which a registry
> MUST verify on ingest**.

Doc 17 §17.3 says the equivalent for `filters/`. kgf-rs can produce the byte size
and the checksum itself. It cannot produce the other five, because they live in
headers only hdtc knows how to parse.

The consequence today is concrete and bad: `filters/` and `keysets/` are built
into every bundle and appear in **no** `artifacts` entry, so they are not covered
by the bundle's `content_digest` (KGF doc 04 §4.3's Merkle root over artifact
checksums). They are bytes on disk that no manifest mentions and no mirror can
verify. Note the demo bundles in kgf-rs already carry both directories in exactly
this undescribed state.

This is also why the request is urgent rather than deferrable. The read side
(source selection, overlap estimation, exact intersect) is later work — KGF doc
07 §7.5 items 18–19 — but bundles are about to be built for ~40 OKN knowledge
graphs. Describing these files now costs one build; describing them later costs a
corpus-wide rebuild, because adding artifacts to a manifest changes
`content_digest`, and a published version is immutable (doc 04 §4.6).

### 2. The doc 18 §18.4 cross-check

The same section specifies a validation that catches a failure nothing else does:

> **Verify the decomposition on ingest**, with the identity that makes it
> checkable: `shared + subjects-only` must equal the `subjects` filter's
> `key_count`, and `shared + objects-only` the `objects` one. Both commands
> derive those counts independently from the same dictionary, so disagreement
> means one artifact is wrong. This is not theoretical: a build on 2026-07-30
> that shared one temp directory across concurrent `hdtc` processes produced key
> sets that were structurally perfect — correct CRC32C, correct `source_digest`,
> strictly ascending keys — and held **another graph's keys**. Every
> format-level check passed. Only the cross-command identity caught it.

`kgf build bundle` runs `hdtc sketch` and `hdtc keyset` back to back and is the
natural place to run this before publishing. (It already gives every hdtc
invocation its own `--temp-dir` so it cannot reproduce that specific bug, but the
check is cheap and catches the class, not the instance.) Running it needs
`key_count` out of both a `.keys` and a `.filter` — three header reads and two
comparisons, and impossible from outside hdtc today.

## Requested API

Readers that return the header fields, in the shape the façade already uses for
the other sidecars (`PermutationHeader`, `GraphIndex`, `TextManifest`).
Suggested placement:
`src/hdt/sketch.rs` and `src/hdt/keyset.rs` beside the builders, re-exported from
`src/format.rs` under new section comments.

```rust
// ---------------------------------------------------------------------------
// Sketch artifacts (filters/)
// ---------------------------------------------------------------------------

/// The common 56-byte envelope of `docs/sketch-format.md` §4, plus the
/// type-specific body header (§5.1 for `.filter`, §6.2 for `.minhash`).
pub struct SketchHeader {
    pub kind: SketchKind,          // Filter | MinHash, from `magic`
    pub format_version: u16,
    pub convention_id: u16,
    pub hash_id: u8,
    pub role: KeyRole,             // constrained to Subjects | Objects
    pub key_count: u64,
    pub source_digest: [u8; 32],
    pub body: SketchBody,
}

pub enum SketchBody {
    /// §5.1
    Filter { variant: u8, seed: u64, segment_length: u32, fingerprint_len: u64 },
    /// §6.2
    MinHash { k: u32, stored_count: u32, saturated: bool },
}

/// Read and validate a `.filter` or `.minhash`, returning its header fields.
/// Verifies the CRC32C first, per `docs/sketch-format.md` §8 reader rule 1.
pub fn read_sketch_header(path: &Path) -> Result<SketchHeader, SketchOpenError>;

/// `filters/{role}.filter` / `filters/{role}.minhash` beside a bundle.
pub fn sketch_path(dir: &Path, kind: SketchKind, role: KeyRole) -> PathBuf;

// ---------------------------------------------------------------------------
// Key sets (keysets/)
// ---------------------------------------------------------------------------

/// `docs/keyset-format.md` §4.1's 96-byte header.
pub struct KeysetHeader {
    pub format_version: u16,
    pub convention_id: u16,
    pub hash_id: u8,
    pub role: KeyRole,
    pub encoding: KeysetEncoding,
    pub low_width: u8,
    pub key_count: u64,
    pub min_key: u64,
    pub max_key: u64,
    pub payload_len: u64,
    pub source_digest: [u8; 32],
}

/// Read and validate a `.keys`, returning its header fields. Verifies the
/// CRC32C first, per `docs/keyset-format.md` §4.4 rule 8.
pub fn read_keyset_header(path: &Path) -> Result<KeysetHeader, KeysetOpenError>;
pub fn keyset_path(dir: &Path, role: KeyRole) -> PathBuf;
```

Exact naming is hdtc's call; what kgf-rs needs is every field named in doc 18
§18.4's manifest-entry list, reachable from a validated file.

### Design notes

**One `KeyRole`, not two.** `docs/sketch-format.md` §4 permits `role ∈ {0,1}`;
`docs/keyset-format.md` §4.1 permits `0..=5` with `0` and `1` meaning the same
things. The key-set space is a superset of the sketch space, deliberately —
§4.1 says the two families are comparable because it is one convention. Model it
as one enum and have `read_sketch_header` reject `role ∉ {0,1}` per §4's
"a reader MUST reject… whose `role` is not `0` or `1`". Two enums would invite a
lossy conversion at exactly the boundary the format says is shared.

`KeyRole` already exists at `src/hdt/keyset.rs:61` with the right six variants
and a `file_stem()` at :88, and `KeysetEncoding` at :122 with `label()` at :137.
Reuse them rather than adding parallel types — but note they are currently
builder-facing, so exporting them makes their variant names part of the façade's
semver surface.

**It must be a conforming reader, which means verifying the CRC32C.** An
earlier draft of this request asked for a header-only read that skipped the
trailer. That is wrong: `sketch-format.md` §8 reader rule 1 and
`keyset-format.md` §4.4 rule 8 both say *verify the CRC before interpreting any
other field*, so a reader that skips it is not conforming, and these files
"cross trust boundaries" by design.

The cost objection turns out not to exist. kgf-rs must SHA-256 each of these
files in full anyway to write the `bytes`/`sha256` half of its manifest entry,
so the payload is already being read on exactly the path that wants the header.
Read the whole file and validate it properly.

**Which validations to apply.** All of them — the reader MUSTs are enumerated
in `sketch-format.md` §8 and `keyset-format.md` §4.4, and both sections are
short and complete. The ones worth calling out because they shape the API:

- *sketch* §4 / §8: reject unknown `magic`, unimplemented `format_version`,
  `convention_id`, or `hash_id`, `role ∉ {0,1}`, and any non-zero `reserved`.
  §5.1: `variant ∈ {8,16}`, `segment_length` a power of two in `[4, 262144]`,
  `segment_length_mask == segment_length - 1`. §6.2: `k ≥ 2`,
  `stored_count ≤ k`, `stored_count ≤ key_count`,
  `saturated == (stored_count == k)`.
- *keyset* §4.4: rules 1–10, including rule 9's exact `payload_len`/`low_width`
  agreement — the rule that "makes decoding memory-safe" by pinning both arrays'
  extent, and which §4.4 warns fails silently if evaluated in a width that wraps.
- Both: `§2`'s exact-arithmetic requirement applies to every size computation.

**`source_digest` is advisory.** `sketch-format.md` §8 reader rule 4 says so
explicitly. The reader should surface it and must not reject on a mismatch —
that is the consumer's policy decision, and for KGF it is the manifest's job,
not this reader's.

**A missing role file is absent information, never an empty role**
(`sketch-format.md` §8 reader rule 6). An empty role is a file with
`key_count = 0`. Whatever the API returns for a missing file must preserve that
distinction — an `Option`, not a default-constructed header.

Relatedly: §8 says emitting a *subset* of roles is conforming at the format
level, and that "a packaging layer that needs it enforced should state which
roles it requires". KGF is that packaging layer — doc 17 §17.3 makes each family
all-or-nothing and doc 18 §18.4 fixes the key-set trio — which is why
`kgf build bundle` passes `--roles subjects,objects` and
`--roles subjects-only,objects-only,shared` explicitly rather than relying on
hdtc's defaults. No hdtc change is wanted there; it is noted so the two layers'
different obligations are not confused.

Both docs carry frozen conformance vectors (`sketch-format.md` §9,
`keyset-format.md` §8) the reader can be tested against directly.

**Typed errors.** `thiserror`, matching `PermutationIndexOpenError` and
`GraphIndexOpenError`. Distinguishing "not this format" from "this format, and
wrong" matters to a caller deciding between *skip* and *refuse to publish*.

## Acceptance criteria

1. `hdtc::format` exports both header readers, the shared `KeyRole`,
   `KeysetEncoding`, `SketchKind`, and the two path helpers.
2. Each reader is conforming: the CRC32C is verified before any other field is
   interpreted, and a file with a corrupted trailer is rejected.
3. Each reader-MUST in `sketch-format.md` §8 and `keyset-format.md` §4.4 is
   exercised by a corrupted-header test, including keyset rule 9's
   `payload_len`/`low_width` agreement.
4. The frozen conformance vectors in `sketch-format.md` §9 and
   `keyset-format.md` §8 parse to the stated field values.
5. `tests/format_api_test.rs` — the contract test kgf-rs's CLAUDE.md names as
   the guarantee for this surface — builds a fixture bundle with
   `hdtc sketch` and `hdtc keyset` and reads back every header, asserting the
   doc 18 §18.4 identity holds on a good build:
   `shared.key_count + subjects-only.key_count == subjects.filter.key_count`,
   and the same for objects.

## Explicitly out of scope

- Filter probe and MinHash estimation (`sketch-format.md` §5.2, §6.3).
- Key-set payload decoding and intersection (`keyset-format.md` §4.2–4.3, §5).
- Any CLI change. This is a library surface; `hdtc sketch` and `hdtc keyset`
  keep their current behaviour and output.

Those are KGF doc 07 §7.5 items 18–19 and a larger conversation. This request is
only what is needed to *describe* the files hdtc already writes, so that bundles
built in the next few weeks do not have to be rebuilt when the read side lands.

## What this unblocks in kgf-rs

`kgf build bundle` gains per-file `artifacts` entries for `filters/` and
`keysets/` — bringing them under `content_digest` and making them mirror-
verifiable — plus the §18.4 cross-check before publication, and the
`filters`/`keysets` capability declarations KGF doc 04 §4.3 shows but
`kgf_store::manifest::Capability` does not yet carry. Tracked in kgf-rs at
`notes/build-bundle.md` §7.
