# kgf-rs

The Rust implementation of **Knowledge Graph Fragments** — a low-cost,
agent-friendly query interface for federated RDF knowledge graphs, built for the
NSF Open Knowledge Network.

## Crates

| crate | role |
|---|---|
| `kgf-store` | The memory-mapped read layer (doc 20). No HTTP, no async, no locks on the read path. |
| `kgf-server` | The HTTP API (doc 03) over `kgf-store`: caps, budgets, cursors, formats. |
| `kgf` | The binary: `kgf build`, `kgf serve`, `kgf manifest`. |

## Building

`kgf-store` depends on [hdtc](https://github.com/frink-okn/hdtc) for the on-disk formats.

```
Source/
  kgf/       # the design documents
  hdtc/      # the build/storage toolchain
  kgf-rs/    # this repo
```

```bash
cargo build --workspace
```

## Status

The mapped read layer and HTTP server are operational. `kgf serve` answers all eight
triple-pattern shapes, exact counts, describe, sample, `o.text`, and bindings-restricted
QUERY/POST fragment and count requests with bounded paging and stable cursors. `/void`
and `/summary` remain before this implementation can claim doc 03's core profile.
