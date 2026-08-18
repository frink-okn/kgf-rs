# Comunica conformance

This harness pins stock `@comunica/query-sparql` 5.3.0. The ignored Rust
integration test starts a real fixture listener with one-row fragment pages,
plus a second independent KGF endpoint, then passes both versioned `/fragment`
URLs to `test.mjs`. The script verifies ordinary TPF discovery/paging, a bind
join that uses Comunica's brTPF `values=` transport, and federation across the
two endpoints.

Run it with:

```sh
npm ci --prefix interop/comunica
cargo test -p kgf --test serve stock_comunica_5_3_queries_the_fragment_endpoint -- --ignored --nocapture
```
