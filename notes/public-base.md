# `--public-base` — serving KGF under a path prefix

Status: **landed 2026-09-03** as unit 23 in `notes/plan.md`, which records what was
built and the two places this note's expectations did not match the wire format (JSON
continuations are cursor tokens, not links, so there was nothing to prefix; and the
page shell and problem renderer needed the mount too). §9's external verification and
the QUERY transport check are still to run once the deployment exists. Written
2026-09-03 against kgf-rs `2e2e26c` and kace at `2026-07-20`
(`registry-augmentations` merge). Scopes `../kgf/docs/gcp-deployment-plan.md` §6;
doc 03 §3.2 is the URL space this changes the mounting of, not the shape of.

Estimated size: half a day, including the test. The router, the cursors, the store,
and every bundle artifact are untouched.

## 1. Why

FRINK routes every service through one shared hostname with a path prefix per
service: LDF at `/ldf`, and each knowledge graph's QLever at `/{kg_name}` with a
`URLRewrite` that strips the prefix before the request reaches the pod
(`../kace/src/k8s/templates/qlever/httproute.j2`). The trial deployment will sit at
`https://apps.okn.us/kgf/`, so a fragment is
`https://apps.okn.us/kgf/ubergraph/v/2026-05-31/fragment`.

Today that cannot work, for three independent reasons, each of which fails silently
in the sense that matters — the response is a 200 and the links in it are wrong:

1. **The origin type refuses a path.** `PublicOrigin` (`crates/kgf-server/src/lib.rs:129`)
   rejects `https://apps.okn.us/kgf` at startup. Its doc gives the reason: route
   paths are appended exactly as received, so an origin with a path would produce
   ambiguous identities. That reasoning is right for the case it considered — a
   server that sees the prefixed path — and wrong for the case FRINK actually runs,
   where the gateway has already removed the prefix and the path the server sees is
   exactly the part that belongs *after* it.
2. **Every generated link is root-relative and prefix-blind.** `url::bundle_base`,
   `url::operation`, and `url::dataset` (`crates/kgf-server/src/url.rs:43–59`)
   produce `/ubergraph/v/…`. A browser at `https://apps.okn.us/kgf/ubergraph` that
   follows `/ubergraph/v/2026-05-31/summary` lands on `apps.okn.us/ubergraph/…`,
   which on FRINK is **the Ubergraph QLever route**. So does page two of every JSON
   fragment, every form action, every breadcrumb, and the `latest` redirect.
3. **The Hydra IRIs are built from the stripped path.** `absolute_request_url`
   (`routes.rs:1252`) is `origin + path_and_query`, so with the origin set to the bare
   host the page IRI and `hydra:next` come out as `https://apps.okn.us/ubergraph/…`,
   and a Comunica client follows them straight to the SPARQL server.

The alternative to this unit — a dedicated hostname with the route at `/` — needs no
code and is what the spec quietly assumes. It is not how FRINK is set up, and a
server that can only be mounted at a hostname root is a deployment constraint the
design never meant to impose. The prefix is the ordinary "external URL" setting
every reverse-proxied service has.

## 2. The rule: the gateway strips, the server emits

One sentence fixes every ambiguity below:

> The base's path is **what the gateway removed**. The server strips nothing itself,
> its router stays mounted at `/`, and every URL it *emits* carries the base.

Consequences, each of which should be a test:

- A request arriving *with* the prefix (`/kgf/ubergraph`) is a misconfigured gateway
  and answers the ordinary 404. The server must not accept both spellings: two URLs
  for one resource breaks cache identity, and a second accepted path is a second
  implementation of the same operation (`CLAUDE.md` rule 1).
- With no base configured, nothing changes: the prefix is empty and every link is
  exactly what it is today. The existing tests are the regression suite for that.
- The base is a deployment fact, not a bundle fact. It lives in `Config`, is set by
  `kgf serve`, and never reaches a manifest or a cursor.

## 3. The type

Generalize `PublicOrigin` into `PublicBase`: a normalized
`scheme://authority[/path]` with the trailing slash removed, no query, no fragment,
no userinfo, `http` or `https` only. Two accessors: `origin()` returning
`scheme://authority`, and `path_prefix()` returning `""` or `/kgf` — never a bare
`/`, so that `format!("{prefix}/{dataset}")` is right in both cases.

The parser is the existing `FromStr` (`lib.rs:138`) with the path branch relaxed:
`""` and `/` normalize to an empty prefix, `/kgf/` normalizes to `/kgf`, and the
percent-encoding of the path is passed through as the operator typed it, because it
must match what the gateway matched. `lib.rs:471`'s invalid list currently asserts
that `https://data.example/kgf` is refused; that case flips to accepted-and-
normalized, and `?`, `#`, and userinfo stay refused.

Rename the flag to `--public-base` (`crates/kgf/src/serve.rs:47`, `:69`). Nothing is
released, so no alias; `notes/plan.md` unit 20's `--public-origin` sentence and the
deployment plan follow the rename. The benchmark documents in `../kgf/docs` are
historical records of runs made with the old flag and should stay as written.

## 4. The inventory

Everything in the server that spells a URL, measured rather than recalled. Line
numbers are at `2e2e26c`.

| what | where | change |
|---|---|---|
| the type and its tests | `lib.rs:129–170`, `:471` | §3 |
| `Config.public_origin` | `lib.rs:91` | becomes `public_base: Option<PublicBase>` |
| Hydra page IRI, template, `next` | `absolute_request_url`, `routes.rs:1252` | `base.as_str() + path_and_query` — the same expression with the base in place of the origin |
| the three root-relative builders | `url.rs:43`, `:52`, `:57` | gain the prefix, via `Mount` (§5) |
| their call sites | `answer.rs` ×8, `descriptor.rs` ×13, `forms.rs` ×5 | mechanical; the forms take `(dataset, version, …)` today (`forms.rs:60–201`) and gain a `&Mount` |
| the brand link | `html.rs:187`, `href="/"` | `mount.root()` |
| the service descriptor's canonical URL | `descriptor.rs:153`, `page(SITE, &[], Some("/"), …)` | `mount.root()` |
| the `latest` redirect | `routes.rs:1099–1104`, two `format!("/{raw_dataset}/v/…")` | prefix the `Location` |
| `Target` constructors | `routes.rs` ×3, `tests/operations.rs:1566`, `:1844` | one added field |
| the ETag digest | `descriptor_digest`, `service.rs:233–237` | **nothing** — it already hashes the origin string; hash the base string instead and a base change invalidates every cached link, which is the property wanted |
| `Problem.instance` | `routes.rs:1539`, `reflected(uri.path())` | optional: prefix it so `instance` is the public path (§6) |
| `no_such_route`'s message | `routes.rs:157` | optional: same, cosmetic |

Two places look like they need a change and do not:

- **`Target::absolute_base`** (`answer.rs:173`) is `origin() + base()`, where
  `origin()` parses `request_url` back to `scheme://authority` and `base()` calls
  `url::operation`. Once `url::operation` carries the prefix, this composes correctly
  on its own. **Do not also prepend the base path there**, or the dataset IRI in the
  Hydra graph comes out as `/kgf/kgf/…`.
- **The summary card.** `stats/summary.json` is written at build time
  (`crates/kgf/src/build/stats.rs:833`) with *version-relative* links —
  `"schema?view=design"`, `"fragment"` — and the HTML cells emit them as relative
  hrefs (`answer.rs:2227` `summary_iri_cell`). A browser resolves those against the
  page URL, prefix included. The artifact is prefix-safe by construction and no
  bundle is rebuilt. (`tests/serve.rs:41`'s `SUMMARY_CARD_JSON` fixture uses
  root-relative links; that is hand-written test data, not what `kgf build` writes.)

## 5. Threading the prefix: `Mount`

The three builders are free functions with no access to configuration, and the
house style rules out a process global. Make them methods on a small value:

```rust
/// Where this deployment is mounted: the path the gateway removed, or nothing.
#[derive(Debug, Clone)]
pub struct Mount(Arc<str>);          // "" or "/kgf"

impl Mount {
    pub fn root(&self) -> String;                                  // "/" or "/kgf/"
    pub fn dataset(&self, name: &str) -> String;                   // "{prefix}/{name}"
    pub fn bundle_base(&self, dataset: &str, version: &str) -> String;
    pub fn operation(&self, dataset: &str, version: &str, operation: &str) -> String;
}
```

`Service` derives one from `Config.public_base` at build and exposes `mount()`.
`Target` (`answer.rs:92`) gains a `mount: Mount` field beside `prefixes`, so every
link an answer renders — `base`, `canonical`, `crumbs`, `context`, the pager, the
term links — reads it from the same place it reads the prefix map. `descriptor.rs`
reads `service.mount()`; `forms.rs` takes `&Mount`. `Arc<str>` because `Target` is
cloned per request and the prefix is not.

The free functions can stay as thin wrappers over an empty `Mount` for the tests
that call them directly, or the tests can construct `Mount::default()`. Either is
fine; do not leave two ways to build a production link.

## 6. `Problem.instance` and the error page

`render_problems` (`routes.rs:1539`) sets `instance` to the server-seen path, which
under a prefix is not a URL the client ever requested. RFC 9457 §3.1.5 makes
`instance` a URI reference identifying the occurrence, so the honest value is the
public path. Prefixing it needs the middleware to see the `Mount`, which means
`middleware::from_fn_with_state` instead of `from_fn` — the same change
`notes/request-logging.md` §3.1 wants for the access layer, so do it once. If the
two units land in either order, whichever is second gets it for free.

## 7. The deployment shape on FRINK

Copy the QLever route's pattern, not the LDF route's: the LDF route matches `/ldf`
without a rewrite, which is the shape where the pod sees the prefix and is exactly
the shape §2 rules out.

```yaml
apiVersion: gateway.networking.k8s.io/v1beta1
kind: HTTPRoute
metadata:
  name: frink-kgf-route
spec:
  parentRefs:
  - name: ingress-gateway
    namespace: ingress
  hostnames:
  - "apps.okn.us"
  rules:
  - matches:
    - path:
        type: PathPrefix
        value: /kgf
    backendRefs:
    - name: frink-kgf-service
      port: 8080
    filters:
    - type: URLRewrite
      urlRewrite:
        path:
          type: ReplacePrefixMatch
          replacePrefixMatch: "/"
```

and the pod runs

```
kgf serve --bundle-root /bundles --bind 0.0.0.0:8080 --public-base https://apps.okn.us/kgf
```

Three details:

- `PathPrefix` matching is per path element, so `/kgf` matches `/kgf` and `/kgf/…`
  and not `/kgfoo`; `ReplacePrefixMatch: /` turns `/kgf` and `/kgf/` into `/`. The
  service descriptor is therefore reachable at `https://apps.okn.us/kgf` with or
  without a trailing slash.
- The same host carries a `/{kg_name}` route per knowledge graph. `kgf` is not a
  registry shortname today; the registry's CI should refuse one, or the route table
  becomes ambiguous.
- The health probe: `HealthCheckPolicy` targets the pod, so it probes `/` as the
  Dockerfile says (`Dockerfile:61`); through the gateway that resource is `/kgf`.

## 8. Tests

- **Parsing.** `https://apps.okn.us/kgf` accepted with `path_prefix() == "/kgf"`;
  `https://apps.okn.us/kgf/` normalizes to the same; `https://apps.okn.us` and
  `https://apps.okn.us/` give an empty prefix; `?x`, `#f`, and `user@` refused.
- **`Mount`.** Every builder with an empty prefix equals today's output byte for
  byte; with `/kgf`, every output starts with `/kgf/` and never `/kgf//`.
- **Over the wire**, modeled on `a_trusted_public_origin_drives_hydra_identity_and_continuations`
  (`tests/serve.rs:374`) and its helpers `serve_with_public_origin` (`:1837`) and
  `request_without_host` (`:1936`). Serve with `https://apps.okn.us/kgf` and send
  requests **without** the prefix, as the gateway delivers them:
  - `/` → `datasets[].url == "/kgf/tox"`, release links under `/kgf/tox/v/…`;
  - `/tox/latest/summary` → 307, `Location` starts with `/kgf/tox/v/`;
  - `/tox/v/v1/fragment?p=…&limit=1` as JSON → `next` starts with
    `/kgf/tox/v/v1/fragment?`; as Turtle → the page IRI is
    `https://apps.okn.us/kgf/tox/v/v1/fragment?…`, the template is
    `https://apps.okn.us/kgf/tox/v/v1/fragment{?s,p,o}`, `hydra:next` starts with
    the base;
  - the same as HTML → the brand link is `/kgf/` and a form action is
    `/kgf/tox/v/v1/fragment`;
  - `/kgf/tox` → **404** with the standard message (§2: no double acceptance).
- The 102 root-relative assertions already in `tests/serve.rs` are the no-base
  regression suite and should not need editing.

## 9. Verification once deployed

From outside the cluster, before pointing anything at it:

```sh
# every IRI must start with https://apps.okn.us/kgf/
curl -s -H 'Accept: text/turtle' \
  'https://apps.okn.us/kgf/dreamkg/latest/fragment?limit=1' -L \
  | grep -E 'hydra:(next|template)|^<https'

# the redirect must carry the prefix
curl -sI 'https://apps.okn.us/kgf/dreamkg/latest/summary' | grep -i '^location'
```

Then run stock Comunica with `https://apps.okn.us/kgf/dreamkg/v/<version>/fragment`
as a `brtpf` source on a pattern that needs more than one page; `interop/comunica`
has the harness. If page two arrives, the prefix is right end to end.

**Do the QUERY check in the same session.** The extension method has never been
sent through the GKE Gateway; `../kgf` doc 07 §7.2 lists "QUERY and POST through the
intended proxy stack" as an unrun transport check. Send a bindings request with
`-X QUERY` and expect a 200 with `Accept-Query`; a 400 or 405 *from the gateway*
means the load balancer filters unknown methods. POST is the designed fallback and
clients will use it, but the service descriptor advertises QUERY and would need a
switch to stop — `notes/plan.md` question 29 already notes that the descriptor has
no field for this. That is a separate small item; this note only says to find out.

## 10. Docs to touch when it lands

- `../kgf` doc 03 §3.2: one paragraph — the URL space is relative to a **service
  base** that may carry a path; a deployment is mounted at exactly one base; every
  emitted link (JSON, HTML, Hydra, `Location`) carries it; the server does not accept
  the prefixed spelling itself.
- `../kgf/docs/gcp-deployment-plan.md` §6: the §7 route and flag, replacing the
  implicit assumption of a hostname root.
- `notes/plan.md` unit 20: the `--public-origin` sentence.
- `CLAUDE.md`: nothing — the status paragraph does not mention the flag.

## Questions for `../kgf`

To be moved into `notes/plan.md`'s list when the unit lands:

1. Doc 03 §3.2 assumes the URL space is rooted at an origin. State the service base
   and the one-base rule (§2).
2. `Problem.instance` — is it the public path or the server-seen one? This note says
   public (§6); doc 03 §3.6 should say so too, since RFC 9457 leaves it to the server.
3. JSON links stay origin-relative rather than absolute. That is a decision, not a
   question, but doc 03 §3.4.10's "typed links" wording should not be read as
   requiring absolute IRIs outside the RDF representations.
