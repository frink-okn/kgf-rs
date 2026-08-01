# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repository is

The Rust implementation of **Knowledge Graph Fragments (KGF)** — a low-cost,
agent-friendly query interface for federated RDF knowledge graphs, built for the NSF
Open Knowledge Network (~40 KGs).

**The design is not here.** It lives in the sibling repo `../kgf` as documents 01–20,
and those documents govern: doc 03 is normative for the HTTP API and its cost table,
doc 04 for bundles and the build pipeline, doc 20 for the read layer this code
implements. When code and spec disagree, that is a bug in one of them — say which,
rather than silently following the code. The split is deliberate: a bundle is servable
by anything honouring doc 03, so this repo is *an* implementation, not the definition
(`../kgf` doc 07 §7.5 item 25).

Three sibling checkouts are expected:

```
Source/
  kgf/       # the design documents
  hdtc/      # the build/storage toolchain — a path dependency
  kgf-rs/    # this repo
```

| crate | role |
|---|---|
| `kgf-store` | The memory-mapped read layer (doc 20). No HTTP, no async, no locks on the read path. |
| `kgf-server` | The HTTP API (doc 03) over `kgf-store`: caps, budgets, cursors, formats. |
| `kgf` | The binary. `kgf manifest` now, `kgf serve` next; `kgf build` when `kgf-build` lands. |

**Status: the query core is built; the HTTP surface is not.** `kgf-store` implements
doc 20's read layer in full — mapped bundles, dictionary, all eight patterns with exact
counts and positional paging, the lazy catalog, and the bundle manifest — and
`kgf manifest` describes a hand-assembled bundle so it is servable. `kgf-server` is
still a skeleton: real signatures and doc comments over `todo!()` bodies.

`todo!()` is a convention, not laziness — an unimplemented path panics rather than
returning a plausible wrong answer. Do not replace one with a stub that returns a
default.

**`notes/plan.md` is the implementation route** — units 1–14 through doc 20 §20.8's M1,
the decisions each one had to make, and the **Questions for `../kgf`** that
implementation surfaced. It is kept current; read it before planning work.
`notes/state.md` is a point-in-time handoff, written at a moment and not maintained
afterwards, so where the two disagree about what exists, `plan.md` and the code win.

## Project Preferences

- Prefer strongly typed Rust code that uses the type system to maintain invariants.
- Follow "parse, don't validate": convert inputs into precise domain types early, and
  make incorrect states unrepresentable where practical.
- Handle errors explicitly and preserve useful context for callers and users.
- Do not introduce unsafe escape hatches. Keep `unsafe` out of the codebase unless
  there is a documented, reviewed need.
- Prefer small domain-specific types over loosely structured strings, integers, or
  boolean flag combinations when those types encode real constraints.
- Do not optimize only for the smallest patch. Weigh narrow changes against the clean
  long-term design, and prefer APIs that make the intended architecture explicit.

### The one reviewed exception to `unsafe`

`kgf-store::map` maps files and hands out `&[u8]`. It is the *only* module permitted
to write `unsafe`; every other crate and module carries `#![deny(unsafe_code)]`, and
`map` carries the single `#[allow]`. This is doc 20 §20.9's obligation — the mapping
surface stays small and audited, and everything above it is safe code over slices.

The soundness argument is written down in that module and must stay true: mapping a
file is unsound in general, because another process can truncate it under a live
slice. KGF relies on **published bundle versions being immutable** (doc 04 §4.6) plus
the binding checks `Store::open` runs before any query view is exposed. The unsafe
constructors for `PublishedBundle` and `PublishedRoot` are where callers acknowledge
that external immutability guarantee; safe store and catalog APIs require those
capabilities rather than accepting arbitrary paths. Anything that maps a file outside
that guarantee does not belong in this crate.

## Rules that are design decisions, not style

Violating one of these is a change to the design, to be surfaced rather than made
quietly.

1. **One implementation per operation** (doc 20 §20.8). No fallback for a missing or
   superseded index, no second encoding, no degraded mode. A bundle either carries
   what an operation needs or that operation is absent from its manifest; a bundle
   missing a required artifact is refused at open with a message naming what to build.
   Concretely: `data.hdt.perm` is required and never derived at open, the HDT-FoQ
   index is never read, and `data.hdt.graphs` and `data.hdt.graphs.idx` must occur
   together. If a fallback looks tempting, the answer is to build the artifact.
   *Not* covered by this rule: one algorithm choosing between equivalent routes on
   cost — `s ? o` probing whichever endpoint is smaller emits in the same order from
   either route and resumes from the same cursor.
2. **Bounded cost.** Every operation has a documented worst-case cost as a function of
   published caps (doc 03 §3.5 is normative). An operation whose server cost is
   unbounded is rejected by design; that is the project's entire thesis. New work on a
   hot path should be expressible in that table.
3. **Id-space in, id-space through, strings at the edges** (doc 20 §20.5). Resolve
   terms to ids once at the boundary, run entirely over ids, materialize strings only
   while serializing.
4. **`Store` is immutable after `open`.** No interior mutability and no locks on the
   read path. Thread safety is by construction. Caches — including the obvious
   per-request term cache — belong to `kgf-server`, because putting one here means a
   lock on the hot path.
5. **The crate boundary is the point.** HTTP semantics (caps, budgets, the truncation
   vocabulary, serialization formats, cursor tokens) must not leak into `kgf-store`,
   and the store must stay testable headless against fixture bundles.
6. **Enumeration order is a contract.** Cursors are positions in the order doc 20
   §20.2's table fixes, and tokens are stable from the first release. Changing which
   permutation serves a pattern breaks every outstanding cursor.
7. **Never fork `data.hdt`.** The core triple store stays standard, interoperable HDT.
   New semantics live in sidecars beside it.

## The hdtc dependency

`hdtc` (`../hdtc`, github.com/frink-okn/hdtc) owns every byte format a bundle
contains. We depend on **`hdtc::format`** — a curated façade, not its module tree.

The division is *format knowledge from hdtc, access code here*. hdtc's readers seek a
`File` with bounded memory because it builds at 10⁸–10¹¹ triples on a small machine;
a server maps files read-only and addresses fixed-width structures in place. Same
bytes, opposite memory model, so almost nothing is shared below the format layer.

- **Do not reimplement parsing.** Section framing, control info, VByte, PFC preambles,
  identity digests, and the `.hdt.perm` section directory come from `hdtc::format`.
  Duplicating them is the documented drift risk (`../kgf` docs 17–18).
- **Do not restate byte formats in comments or docs.** hdtc's `docs/*-format.md` are
  normative; cite them (`permutation-index-format.md` §7.2) instead of paraphrasing.
- **If something needed is missing, add it to hdtc's façade** rather than widening a
  module's visibility or copying code. `hdtc/tests/format_api_test.rs` is the contract
  test for that surface.
- Known gaps, tracked in `../kgf` doc 07 §7.5 items 18–19: hdtc has no sketch probe
  API and no key-set intersect. Those are hdtc work when the operations arrive.

## Required Checks

Before every commit, run:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

## Testing

Per doc 20 §20.9, the tests that matter are differential and property-based rather
than example-based:

- **Golden bundles**: tiny fixture RDF, built by hdtc in CI; answers compared against
  `hdtc search` and hdt-rs for every pattern shape.
- **Permutation consistency**: `Σₚ count(? p ?) = N = Σₒ count(? ? o)`; `count(? p o)`
  agrees between POS and OPS; every triple from `? ? ?` is found by all applicable
  bound patterns; counts equal enumeration lengths under exhaustive paging at
  adversarial page sizes (1, 2, prime, cap).
- **Cursors**: resume at every position of every pattern yields exactly the suffix;
  stale digests and foreign-request tokens are rejected.
- **Concurrency**: N threads × M bundles × mixed patterns under eviction.
