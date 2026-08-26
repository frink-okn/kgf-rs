# `kgf build` — one command, one config

Status: built and working end to end — config, plan, `--check-config`,
`--dry-run`, the hdtc pipeline, provenance, and atomic publication. Two things
remain, both blocked on hdtc (§7): describing `filters/` and `keysets/` in the
manifest, and the doc 18 §18.4 cross-check between them. Written 2026-08-26
against kgf-rs `7b238d0`, hdtc `1.2.0-beta.2` (`087d7a1`), okn-registry
`docs/registry/kgs.yaml`, kace `d6414eb`.

Scopes `../kgf/docs/gcp-deployment-plan.md` §3.2. Doc 04 §4.4 is the authority on
what the build pipeline is; this note is about the subset that is buildable today
and the config schema kace will render. Where the two disagree, §7 says so.

## 1. What is ad hoc today

A bundle is assembled by hand: `hdtc create --perm`, then `hdtc text`, then
optionally `hdtc sketch` and `hdtc keyset`, then `kgf manifest` to describe it,
then `kgf build stats` to produce and publish the eight-artifact description set.
Both stop-gaps are now retired: `kgf build` is the whole pipeline, and the
producer they shared lives on as `build::stats::produce`.
The ordering is load-bearing and written down nowhere executable. The risk the
deployment plan names is precise: if kace learns that ordering, it lives in two
repos and drifts, the way `augmentations.py` did.

So the goal is not a new capability. It is moving knowledge that already works
out of a person's head and into one binary with one config schema.

## 2. Command shape

```
kgf build --config build.yaml --out <root>/<dataset>/<version>/
```

`--out` is the final version directory, exactly as doc 04 §4.4 spells it. The
command stages into a sibling temp directory on the same filesystem and
`rename`s into place with the manifest written last — the same publish discipline
`kgf build stats` already uses (`build/stats.rs:publish`), and the same atomicity
the deployment plan's §5 step 5 asks kace to arrange by hand. It should not have
to: staging is the builder's job, and doing it here means one implementation
rather than one per caller.

`id` and `version` default from the last two path components of `--out`, matching
`kgf manifest`'s existing behaviour, and are an error if they contradict the
config.

Input, one of:

- `--hdt <path>` — adopt an existing HDT. This is the OKN path: kace's
  `HDTConversionWorkflow` already leaves `hdt/graph.hdt` in LakeFS, and that file
  is exactly the input. Add `--adopt` to move rather than copy, because the
  downloaded copy is scratch and a copy costs a full HDT-sized read and write on
  a KG where that is the dominant term.
- `--input <rdf>...` — build from RDF via `hdtc create --perm`, which produces
  the permutation sidecar in the same pass rather than a second one. This is the
  local contributor's path and doc 04 §4.4's headline invocation.

Per-build values are flags, not config: `--version`, `--previous-version`,
`--source-url`, `--source-sha256`. They change every run, and rendering a YAML
template to carry a value the caller already holds is a template for no reason.

Two subcommands of the same config, so that a caller learns one schema:

- `--check-config` parses, defaults, and validates, prints the resolved plan as
  JSON, and builds nothing. This is what lets the registry be validated across
  the whole corpus in CI by the real validator rather than by a second one
  written in pydantic.
- `--dry-run` additionally prints every `hdtc` argv it would execute, in order.
  Worth more than it sounds for debugging a K8s Job.

`--config -` reads stdin, so kace can pipe a rendered config instead of
materialising a file into a ConfigMap.

## 3. The config schema

Three sections, split by what they govern, and every field optional except the
dataset's identity. Doc 04 §4.4's requirement is that a bare build produce a
useful bundle, so the minimal config is four lines.

```yaml
schema: 1

dataset:                       # identity and description
  id: dreamkg
  iri: https://purl.org/okn/frink/kg/dreamkg
  title: DREAM-KG
  description: Explainable AI for homelessness services.
  license: https://creativecommons.org/licenses/by/4.0/
  homepage: https://github.com/dream-kg
  publisher: {name: Temple University, contact: mailto:…}

semantics:                     # an interpretation of the data, frozen per version
  prefixes: {dream: https://dreamkg.org/}
  roles:
    label: [http://www.w3.org/2004/02/skos/core#prefLabel,
            http://www.w3.org/2000/01/rdf-schema#label]
  authoritative_namespaces: [dream]

contents:                      # what changes bytes
  perm:
    position_maps: []          # [] | [pos] | [ops] | [pos, ops]
  text:
    enabled: true
    max_literal_bytes: 4096
    exclude_datatypes: []
    index_all_datatypes: false
    untagged_language: en
  filters:                     # hdtc sketch — always built, doc 17 §17.3
    k: 65536
    filter_bits: 16
  keysets:                     # hdtc keyset — always built, doc 18 §18.4
    encoding: elias-fano
  stats:
    enabled: true
    prefix_tables: [/etc/kgf/prefixes.yaml]   # layered, later wins

resources:
  memory_limit: 4G
  temp_dir: /scratch
  threads: null
  max_bundle_bytes: null       # refuse rather than fill a PVC

source:                        # recorded, never acted on
  url: lakefs://dreamkg/<commit>/hdt/graph.hdt
  format: hdt
  sha256: …
```

Two naming decisions worth stating, because both will look arbitrary later.

**`contents:` keys are bundle directory entries, not hdtc subcommands.**
`filters` and `keysets` are what doc 04 §4.1 calls the directories and what
§4.3's capability map calls them; `hdtc sketch` and `hdtc keyset` are the tools
that happen to produce them today. The config describes the bundle, so renaming
an hdtc subcommand must not be a config break.

**`contents.perm`, `contents.filters`, and `contents.keysets` have no
`enabled`.** `data.hdt.perm` is required by rule 1: no fallback for a missing
index, never derived at open. Filters and key sets are required by conformance:
doc 18 opens by saying a conforming bundle publishes key sets "unconditionally —
there is no size threshold", and doc 17 §17.3 makes each sketch family
all-or-nothing. So none of the three carries an enable flag, and none exposes a
free `roles` list — doc 17 §17.3 forbids publishing one role of a family, and
doc 18 §18.4 excludes hdtc's experimental `terms` role from the KGF profile
outright, because predicate IRIs make every pair of KGs "overlap" through
`rdfs:label`. Both are made unrepresentable rather than validated. `text` keeps
its flag: `search` is an optional capability and the text index is the expensive
step.

`semantics.prefixes` layers *last* over `contents.stats.prefix_tables`, which is
what `kgf build stats` already does with the manifest's prefix map
(`build/stats.rs:220`). The shared OKN table is the base; the per-KG block wins.

## 4. Step order

1. Config + flags → a typed, fully-defaulted `BundlePlan`. Validate here: the
   dataset IRI parses as an absolute IRI, role IRIs pass
   `validate_predicate_role_iri` against the resolved prefix map, `id` is a legal
   URL path component. Nothing after this point re-checks a string.
2. Stage: `<out>/../.kgf-build-<version>-<rand>/`.
3. `data.hdt` + `data.hdt.perm`: `hdtc create --perm` from RDF, or adopt the HDT
   and run `hdtc perm` over our own copy. kace's conversion builds HDT-FoQ with
   `--index`; KGF never reads it, and the conversion job stays untouched.
4. `hdtc text` → `data.hdt.text/`, if enabled. The expensive step: every literal.
5. `hdtc sketch` → `filters/`, `hdtc keyset` → `keysets/`, each in its **own**
   temp directory (doc 18 §18.4), then the cross-command identity check between
   them before anything is published.
6. Provisional manifest, then the description set, then the final manifest —
   see §5.
7. `rename(staging, out)`.
8. Report per-artifact bytes and per-step elapsed. Doc 04 §4.4 asks for size
   estimates before expensive steps; a text index is not estimable to a useful
   precision, so report actuals as they land and let `resources.max_bundle_bytes`
   refuse, rather than print a number that will be wrong.

## 5. One producer, one command

`kgf build stats` was the stop-gap that produced the description set, and it
required a bundle that already had a `manifest.json` carrying typed dataset
fields — it upgraded a hand-assembled bundle in place. `kgf build` has that
identity from the config and has no manifest yet, so the two could not compose
until the producer was separated from the command.

`build::stats::produce` now writes the eight artifacts into a directory and does
nothing else: no manifest read, none written, nothing published. `kgf build`
owns where that directory came from and what happens to it. What had forced the
coupling was the summary card taking a `&Manifest` for six descriptive fields
and nothing structural; `DatasetCard` carries those six from a config instead.

With the producer separated, `kgf build stats` had no unique job left except
regenerating a description set *in place* on a published bundle — which fights
doc 04 §4.6's immutability and the `PublishedBundle` obligation that rests on
it. It is retired, and with it `kgf build bundle` collapses to `kgf build`,
which is how doc 04 §4.4 spells it.

**What retiring it costs, and how that comes back.** When the description-set
format revs, regenerating it across the corpus used to be cheap: stats are
small, and the text index — the expensive step, covering every literal — could
be left alone. A full rebuild pays for the text index again. The immutability-
respecting version of that shortcut is a mode of `kgf build`: derive a new
version from an existing one by hardlinking the expensive artifacts and
rebuilding only stats and the manifest. That is a *new version*, not an edit,
so it keeps §4.6.

Worth building rather than a nicety, because it is the same primitive a
predicate-roles correction needs (§7): a directory of hardlinks plus a new
`manifest.json`, seconds rather than a rebuild. Two independent needs landing on
one operation. It is additive — a flag on a command whose shape is already
fixed — so the pipeline can depend on `kgf build` today without waiting for it.

## 6. How kace renders the config

The answer to "assemble it out of band from the registry" is yes, rendered in
the workflow rather than by a separate script, and the shape of the yes is what
keeps it from drifting.

**No committed config files.** They would be a second copy of registry data, and
the failure mode is the one this plan already names with `augmentations.py`: two
places to edit, one forgotten. The registry is the source of truth, kace already
loads it through `KGConfig.from_git()`, and rendering is a few dozen lines.

### The mechanism already exists

`JobMan.run_job` takes `configmap_overrides: Dict[str, str]`, documented as
"{configmap_path: content} — replace the content of a configmap file declared in
the job template ... Lets callers supply config-driven file contents without
baking them into the static template." That is what the QLever settings JSON
already uses. A build config is the same problem, so:

```python
configmap_overrides={
    "/kgf/build.yaml": render_build_config(kg),
    "/kgf/prefixes.yaml": shared_prefix_table,
}
```

with the job template's args carrying `--config /kgf/build.yaml`. No new
machinery, and it follows the house pattern rather than adding a second one.
`--config -` reads stdin and also works, but there is no reason to reach for it.

### What is rendered, and what stays a flag

`dataset:` comes from fields kace already has — `shortname` -> `id`, plus
`title`, `description`, `license`, `homepage`, and `contacts` -> `publisher`.
`iri` is the string kace already mints for the VoID step,
`https://purl.org/okn/frink/kg/{shortname}`, so the bundle's VoID and the
okn-void graph describe one resource.

`semantics:` and `contents:` are a **verbatim subtree**, not a field-by-field
translation:

```yaml
frink-options:
  lakefs-repo: scales
  kgf:
    semantics:
      roles:
        label: [https://scales.okn.us/property/hasName,
                http://www.w3.org/2000/01/rdf-schema#label]
      prefixes:
        scales: https://scales.okn.us/
    contents:
      text: {max_literal_bytes: 8192}
```

`frink-options.kgf` *is* those two sections. kace copies the subtree and merges
the `dataset:` block it derived; its pydantic model needs
`Optional[Dict[str, Any]]` and nothing more, so the schema stays owned by this
repo. Same pattern as `augmentations:`, minus the part where the schema ends up
in two languages.

Per-build values stay flags, never config, because they change every run:
`--out` (the LakeFS tag), `--source-url` (`lakefs://` pinned to the **commit**,
not the tag), `--source-sha256`, `--previous-version`.

### The prefix table has to reach the container

`contents.stats.prefix_tables` names a path, and the base table is the
registry's `docs/registry/prefixes.yaml`. Baking it into the kgf image would pin
it at image-build time, so a registry prefix addition would not reach a bundle
until the image was rebuilt. Render it as a second `configmap_overrides` entry
instead, so the table tracks the registry the way everything else does — and so
it falls inside the config hash below automatically. `hdtc namespaces` records
the table's sha256 in its output, which keeps a bundle auditable against a
registry revision.

### Change detection needs the config

Deployment plan §5 step 2 compares `(commit_id, builder_image)` and says a
registry-only edit does not retrigger a build. That is not quite right any more:
a `frink-options.kgf` edit **can** change bytes — `text.max_literal_bytes`,
`filters.k`, a role cascade — and must retrigger. The tuple wants to be
`(commit_id, builder_image, config_hash)`.

The hash should be over `kgf build --check-config` output, not over the rendered
YAML. The resolved plan is canonical and round-trips, so it is invariant to key
order, formatting, and omitted defaults: two configs that *mean* the same thing
hash the same, and only a real change trips a rebuild. That property is the
reason `--check-config` prints a document that parses as a config again rather
than a report.

### Validate in registry CI

Run `kgf build --check-config` over every entry's rendered config in the
registry's own CI. A malformed `kgf:` block then fails in the pull request
rather than at 3am in a K8s Job, and the validator is the real one instead of a
pydantic reimplementation of it.

### The descriptor closes the loop

Deployment plan §5 step 6 has kace refreshing
`{root}/{dataset}/descriptor.json` from the registry, which means kace learning
a second JSON schema. A `kgf descriptor --config build.yaml --out
descriptor.json` renders it from the config kace already holds, and kace learns
one schema total.

## 6a. What goes in `source`

`content_digest` is a Merkle root over *published bytes*, and doc 04 §4.3 is
emphatic that this is not a digest of build inputs: two builds from one source
may legitimately produce different text indexes. So `source` is **provenance,
not identity** — it never participates in the digest, and it answers "what would
I run to get a bundle like this one again", not "is this bundle intact".

Deployment plan §7 makes that load-bearing. The PVC is the only copy of a bundle,
and what makes losing it survivable is that the durable input is in LakeFS and
that the recipe was recorded. §5 step 2 records the recipe in a kace ConfigMap.
Recording it in the manifest too costs nothing and makes the bundle
self-describing, so a bundle found on disk with no cluster around it still says
where it came from.

Two fields kgf can *prove*, and the rest it can only *repeat*:

```json
"source": {
  "inputs": [
    {"url": "lakefs://dreamkg/9f3c…/hdt/graph.hdt",
     "format": "hdt",
     "sha256": "…"}
  ],
  "generator": {"kgf": "0.1.0", "hdtc": "1.2.0-beta.2",
                "image": "ghcr.io/frink-okn/kgf@sha256:…"}
}
```

- `sha256` is **computed by the builder**, not taken on trust. The input is read
  in full anyway to build the permutation, so the hash is nearly free, and a
  `--source-sha256` flag then becomes an *assertion to verify* rather than a
  value to copy — which turns it into a real integrity check on the LakeFS
  download rather than a restatement of it. Mismatch fails the build.
- `generator.hdtc` is read from the binary actually invoked (`hdtc --version`),
  not from the config. "Re-derive exactly" is false without it: `.perm`, sketch,
  and text formats are pinned by convention rather than by commit, so the
  producing version is the only thing that makes a rebuild comparable.
- `url` and `image` are caller-supplied and unverifiable here. That is fine, and
  it should be said plainly rather than implied — they are labels the builder
  passes through.

Three shape changes against doc 04 §4.3, all of them things this repo should
propose rather than quietly adopt (`notes/plan.md`'s **Questions for `../kgf`**):

1. `inputs` is a list. `--input a.nt b.nt` is ordinary, and doc 04 §4.4 step 1
   already talks about per-input blank-node disambiguation, so one object cannot
   describe the general case.
2. `generator` has no home in a componentless bundle. Doc 04 §4.3 hangs
   `generator` off each *component*, which is right for a derived component and
   leaves a plain one-source bundle with nowhere to record which hdtc built it.
3. `format: "hdt"` is a legitimate source format. Doc 04 §4.4 step 1 assumes RDF
   in, but the OKN path's input is an HDT that another pipeline already built,
   and normalization has already happened upstream.

## 7. Where this disagrees with the plan and the docs

**Predicate roles cannot be a pure serve-time overlay.** Deployment plan §3.4 and
§5 both assume a roles correction becomes a descriptor edit plus a restart rather
than a rebuild, on doc 04 §4.3's reasoning that a role is an interpretation and
correcting one should be an edit. The implementation freezes roles into the
manifest deliberately, and the reason is written at `manifest.rs`'s
`predicate_roles`: a versioned URL is cacheable forever, so letting `role=label`
resolve through mutable state would let a cache-forever answer change meaning.
Both arguments are correct about different URLs. The resolution the field comment
already implies:

- the descriptor carries the publisher's *current* profile, and `/{dataset}`
  shows it;
- each bundle freezes the snapshot that `/{dataset}/v/{version}/search` and
  `/labels` resolve against.

So roles belong in `build.yaml`, a roles correction *does* require a rebuild to
change versioned answers, and the overlay's real scope is title, description,
publisher, homepage, license, and authoritative namespaces — none of which change
a query result. The deployment plan's §5 claim that the sweep can fix roles
cheaply needs revising, and doc 04 §4.3's "correcting one must be an edit, not a
rebuild" needs the versioned-URL caveat.

**`Catalog::scan` cannot tell a staging directory from a published one.** It
walks every `{root}/{dataset}/{version}` directory it finds and records it,
lazily, as a release. A build staging into a sibling of its output therefore
appears in `catalog.ids()` and in the dataset descriptor's release list if a
server starts mid-build. This is not hypothetical and not new: `kgf build stats`
already stages as `.kgf-stats-*` inside the dataset directory
(`build/stats.rs:120`), so a `kgf build stats` interrupted against a live root
leaves exactly that.

The fix is one rule in `Catalog::scan` — a name beginning with `.` is not a
published version — and it is worth making a rule rather than a convention,
because it is what lets a build stage on the same filesystem as its output and
still publish by `rename`. `DatasetId` and `VersionLabel` refuse leading dots
from the other side, so the two halves cannot drift into disagreeing about which
directories are real.

**~~`filters/` and `keysets/` are undescribed bytes~~ — fixed.** hdtc
v1.2.0-beta.3 added the header readers, so both families now carry a manifest
entry per file with `convention_id`, `format_version`, `hash_id`, `role`,
`key_count` and (for key sets) `encoding`, they are covered by
`content_digest`, and the bundle declares the `filters` and `keysets`
capabilities. Doc 18 §18.4's cross-family identity runs before any manifest is
written. What follows is the original finding, kept because the reasoning about
*why* it had to happen before the corpus build still applies.

**Three checks, because the first one alone is not enough.** The §18.4 identity
compares **counts, not content**: a key set swapped for another bundle's passes
whenever the totals happen to agree, which is not hypothetical — two fixtures
here had the same `shared.keys` count and the planted file went undetected until
a count differed. So the build also verifies that a file's declared role matches
the name it is stored under, and — the real guard — that its `source_digest`
binds to this bundle's own HDT.

That last one needed no hdtc change, which is worth recording because the first
conclusion was that it did. hdtc's `hdt_data_digest` is `pub(crate)`, but it is
only `scan_hdt_sections` to find `data_offset`, a seek, and `sha256_to_end` —
and all three of those *are* exported. Composing them here gives the identical
digest. Check what the façade already permits before asking for an addition to
it.

The formats call `source_digest` advisory, and for a consumer it is: doc 18 §4.1
forbids letting it gate comparability, since a rebuild changes the digest
without changing what the keys mean. A producer describing its own bundle is the
opposite case — a mismatch there says this file was built from different bytes
than the ones beside it, exactly the staleness the digest exists to detect, and
the same rule `verify_text_binding` already applies to the text index.

The original finding:

 Neither appears in `store::artifact`, `Capability` has no variant for
either, and — the sharp part — neither is in the `artifacts` map, so neither is
covered by `content_digest`. The demo bundles carry both directories today as
bytes no mirror can verify and no manifest mentions.

They are still built unconditionally. Doc 18 opens by requiring exactly that, and
the read side is coming (doc 07 §7.5 items 18–19: hdtc has no sketch probe API
and no key-set intersect yet). Building them now and describing them now is what
keeps the read side from costing a corpus-wide rebuild later, which is the whole
point.

Describing them is more than a checksum. Doc 17 §17.3 and doc 18 §18.4 both
require a manifest entry **per file**, carrying `convention_id`,
`format_version`, `role`, `encoding`, `key_count`, bytes and checksum — and doc
18 says a registry MUST verify those on ingest. Per file, not one directory
entry: unlike `data.hdt.text/`, whose Tantivy segment names are build-generated,
these have stable role-derived names (`subjects.filter`, `objects.minhash`,
`shared.keys`) and, per doc 04 §4.3, different dependency sets and lifecycles.
A missing role file means "not built", never "empty role".

**~~hdtc answers no `--version`~~ — fixed** in v1.2.0-beta.3; `source.generator.hdtc`
now records the binary that ran. Original finding:

**hdtc answers no `--version`.** Its clap command sets no `version`, so
`hdtc --version` exits 2. `source.generator.hdtc` is therefore empty in every
bundle built today, and "re-derive exactly" is not true without it: the
permutation, sketch, and text formats are pinned by convention rather than by
commit, so the producing version is what makes a rebuild comparable. The build
warns rather than passing over it, because a `source` block that quietly omits
the toolchain looks like provenance while failing at the one thing provenance is
for. One attribute in hdtc fixes it.

**~~hdtc's façade exposes no sketch or key-set readers~~ — fixed** in
v1.2.0-beta.3 (`read_sketch_header`, `read_keyset_header`, `KeyRole`,
`KeysetEncoding`, the path helpers). Original finding:

**hdtc's façade exposes no sketch or key-set readers.** `hdtc::format` re-exports
section framing, PFC, permutation, graph-index, and text items and nothing for
`filters/` or `keysets/`, so today there is no supported way to read a
`convention_id` or a `key_count` out of a published file. Those entries cannot be
written without it, and per the crate rule the fix is hdtc's façade, not a parser
here. The same readers are what a registry ingest check would use, so they want
to exist once.

**The doc 18 §18.4 cross-check belongs in this command.** `shared + subjects-only`
must equal the `subjects` filter's `key_count`, and `shared + objects-only` the
`objects` one; `hdtc sketch` and `hdtc keyset` derive those counts independently
from the same dictionary, so disagreement means one artifact is wrong. This is
not hypothetical — a build on 2026-07-30 that shared one temp directory across
concurrent `hdtc` processes produced key sets that were structurally perfect and
held another graph's keys, and only this identity caught it. Two consequences for
the builder: give every `hdtc` invocation its **own** temp directory under
`resources.temp_dir`, never one shared, and run the cross-check before the
manifest is written, refusing to publish on mismatch. A build that can detect its
own corruption should not leave it for a registry to find.

**The manifest has no `source` block.** Doc 04 §4.3 specifies one
(`{format, sha256, url}`); `Manifest` does not model it. See §6a for what should
go in it.

**Labels are resolved live, so hdtc never needs to know a label role.** Doc 04
§4.4 step 3 has hdtc emitting "the label array for the declared default cascade"
at build time. It does not, and it should not: `hdtc text` has no cascade flag,
and the server resolves the cascade per request from the core permutations
(`routes.rs:598`, `:741`, `:944`) by probing `(s, p, ?)` for each role predicate
in the manifest's frozen order until one hits. That is a bounded number of
pattern lookups against an index the bundle already has.

The division that falls out is clean, and worth stating because it is the reason
the config splits the way it does: **hdtc indexes literals exhaustively and
role-agnostically; KGF applies the roles at read time.** The text index is a
property of the data, the cascade is an interpretation of it, and nothing in the
expensive artifact depends on the cheap declaration.

The consequence is operational. Because `content_digest` is a Merkle root over
*artifact* checksums (`manifest.rs:content_digest_preimage`), and
`predicate_roles` is a manifest field rather than an artifact, correcting a
cascade changes no artifact and no digest. It still must be a new published
version — doc 04 §4.6's immutability is what `PublishedBundle`'s unsafe
constructor rests on, and rewriting a live manifest in place would break it — but
that version is a directory of hardlinks plus a new `manifest.json`, seconds of
work rather than a rebuild. That is the honest version of the deployment plan's
"a roles edit no longer needs a build": not an overlay, but a relink.

Doc 04 §4.4 step 3 and doc 19 §19.4.5 should follow the implementation here
rather than the build growing an artifact nothing reads.

## 8. Explicitly not in this

The component DAG (doc 04 §4.4's `components:` and `publish:`), external tools,
content-addressed memoisation, and per-component VoID. The config names
`components:` and `publish:` and refuses them with that reason rather than as
unknown fields, so a config written against doc 04's DAG fails with an
explanation. Claiming the keys stays additive: when the DAG lands the refusal
becomes an implementation, and no config that works today breaks. That is the whole reason for the `schema: 1` line.

Named graphs are off for this deployment (plan §9): `hdtc create` drops the graph
component of quads, so there is no `.graphs` sidecar and no `graphs` capability
to declare.
