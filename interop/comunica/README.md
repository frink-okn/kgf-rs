# Comunica conformance

This harness pins stock `@comunica/query-sparql` 5.3.0. The ignored Rust
integration test starts a real fixture listener with one-row TPF pages,
plus a second independent KGF endpoint and a third serving a bundle with
named-graph memberships, then passes the three versioned `/tpf` URLs to
`test.mjs`. The script verifies ordinary TPF discovery/paging, a bind join
that uses Comunica's brTPF `values=` transport, typed-literal ingress, stable
skolem identity, control/data graph separation, N-Quads negotiation,
federation across the two endpoints, and — against the third — that stock
Comunica reads the union for a bare pattern from the page's `sd:defaultGraph`
declaration alone and pages it to the end, reaches a named graph and the
unnamed graph through `GRAPH`, and sees only named graphs through `GRAPH ?g`.

Run it with:

```sh
npm ci --prefix interop/comunica
cargo test -p kgf --test serve stock_comunica_5_3_queries_the_tpf_endpoint -- --ignored --nocapture
```
