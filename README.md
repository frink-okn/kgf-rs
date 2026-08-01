# kgf-rs

The Rust implementation of **Knowledge Graph Fragments** — a low-cost,
agent-friendly query interface for federated RDF knowledge graphs, built for the
NSF Open Knowledge Network.

The design lives in a separate repository, [`kgf`](../kgf), as documents 01–20.
This one is the code. That split is deliberate: a bundle is servable by anything
that honours doc 03, so `kgf-rs` is *an* implementation, not the definition, and
the spec versions independently of it.

## Crates

| crate | role |
|---|---|
| `kgf-store` | The memory-mapped read layer (doc 20). No HTTP, no async, no locks on the read path. |
| `kgf-server` | The HTTP API (doc 03) over `kgf-store`: caps, budgets, cursors, formats. |
| `kgf` | The binary. `kgf serve` now; `kgf build` when `kgf-build` lands. |

## Building

`kgf-store` depends on [hdtc](../hdtc) for the on-disk formats — every byte a
bundle contains is hdtc's, and `hdtc::format` is the curated surface we link
against (doc 20 §20.4). The dependency is a path to the sibling checkout, so
both repos need to be cloned next to each other:

```
Source/
  kgf/       # the design documents
  hdtc/      # the build/storage toolchain
  kgf-rs/    # this repo
```

```bash
cargo build --workspace
```

## What is deliberately absent

`kgf serve` implements each operation **exactly once** (doc 20 §20.8). There is
no alternate path for a missing index, no second encoding, no degraded mode. A
bundle either carries what an operation needs or that operation is absent from
its manifest; a bundle missing a required artifact is refused at open with a
message naming what to build.

Concretely: `data.hdt.perm` is required and never derived at open; the HDT-FoQ
index is never read; `data.hdt.graphs` and `data.hdt.graphs.idx` must either both
be present or both be absent.
Every fallback doubles the surface that has to be tested and measured for a
bounded-cost guarantee, and it is the fallback that gets neither.

## Status

Skeleton. Module boundaries and public shapes are settled; the bodies are not
written, and every unimplemented entry point says so rather than returning a
plausible wrong answer.
