# hdtc feature request: report a version

**Repo:** `hdtc` (github.com/frink-okn/hdtc), at `1.2.0-beta.2` / `087d7a1`.
**Requested by:** kgf-rs, `kgf build bundle` (`crates/kgf/src/build/bundle/execute.rs`).
**Size:** one clap attribute, plus a decision about the output string's stability.

## The symptom

```console
$ hdtc --version
error: unexpected argument '--version' found

  tip: a similar argument exists: '--verbose'
```

`src/cli.rs:155` declares the root command as

```rust
#[command(
    name = "hdtc",
    about = "HDT Creator - converts RDF files to HDT format",
    long_about = "…"
)]
pub struct Cli {
```

with no `version`, so clap generates no `--version` and no `-V`. `Cargo.toml`
already carries `version = "1.2.0-beta.2"`; nothing surfaces it.

## Why it matters downstream

`kgf build bundle` assembles a KGF bundle by orchestrating `hdtc create --perm`,
`hdtc text`, `hdtc sketch`, `hdtc keyset`, and `hdtc void`, then writes a
`manifest.json` describing the result. That manifest now carries a `source`
block whose purpose is re-derivation:

```json
"source": {
  "inputs": [{"url": "lakefs://dreamkg/9f3c…/hdt/graph.hdt",
              "format": "hdt", "sha256": "…"}],
  "generator": {"kgf": "0.1.0", "hdtc": null, "image": "ghcr.io/…@sha256:…"}
}
```

`generator.hdtc` is `null` in every bundle built today, and that is the field
that makes the rest of the block mean anything. The `.hdt.perm`, sketch, text,
and key-set formats are pinned by *convention* rather than by commit, so two
hdtc builds can produce byte-different artifacts that are both valid. Without
recording which one ran, "rebuild this bundle exactly" is not a statement
anybody can act on — and the deployment this feeds (a GCP cluster where the
serving volume is the only copy of ~40 bundles) treats rebuild-from-LakeFS as
its recovery path.

kgf deliberately does **not** use the linked library version. hdtc is a path
dependency of `kgf-store`, so `env!` would compile in the version of the source
tree kgf was built against — but the binary `kgf build bundle` *invokes* may be
a completely different build, which is precisely the case worth catching. It
must be an observation of the process that ran, not a claim about the one that
was linked.

kgf currently warns when the version is unreadable rather than passing over it,
because a `source` block that quietly omits the toolchain looks like provenance
while failing at the only thing provenance is for. That warning fires on every
build.

## Requested change

Add `version` to the root command so clap derives `--version` and `-V` from
`CARGO_PKG_VERSION`:

```rust
#[command(
    name = "hdtc",
    version,
    about = "HDT Creator - converts RDF files to HDT format",
    long_about = "…"
)]
pub struct Cli {
```

### What the output should be

clap's default is `hdtc 1.2.0-beta.2\n` on stdout with exit status 0. That is
fine and is what the consumer expects. Two properties matter more than the
shape:

1. **stdout, not stderr, and exit 0.** kgf runs `hdtc --version`, requires
   success, and takes trimmed stdout. A version printed to stderr or exiting
   non-zero reads as "no version available".
2. **Stable across a release.** kgf records the whole trimmed line **verbatim**
   into `manifest.json`, and a published manifest is immutable (KGF doc 04
   §4.6). It does not parse the string, so extra content is not a breaking
   change for kgf — but anything varying build-to-build within one release
   (a timestamp, a build host, a dirty-tree marker) would make otherwise
   identical bundles differ in a field meant to identify the toolchain.

If a richer string is wanted, a git describe suffix (`1.2.0-beta.2 (087d7a1)`)
is strictly more useful to kgf than the bare version, since the formats are
pinned by convention and the commit is the real identifier. That is a
nice-to-have, not part of this request — but if it is added, derive it at build
time from a committed source, not from the working tree's dirty state.

### Subcommands

Only the root command needs it. clap's `version` on the root does not propagate
to subcommands unless `propagate_version = true` is set, and kgf never asks a
subcommand for its version. Adding propagation is harmless but unnecessary.

## Acceptance criteria

- `hdtc --version` exits 0 and prints a non-empty line to stdout.
- `hdtc -V` does the same.
- The string contains the `Cargo.toml` version.
- No existing subcommand parse changes — in particular, `-v`/`--verbose` still
  means verbosity and is not shadowed by `-V`. (clap distinguishes them by case;
  worth an explicit test because `--verbose` is the tip clap currently offers
  for the mistyped flag, so the two are easy to confuse.)

Suggested test in `tests/` (there is a `Command`-driven pattern already in
`tests/format_api_test.rs`):

```rust
#[test]
fn the_cli_reports_its_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_hdtc"))
        .arg("--version")
        .output()
        .expect("run hdtc --version");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
}
```

## What this unblocks

`source.generator.hdtc` becomes populated in every bundle `kgf build bundle`
writes, and the warning it currently emits on every build goes away. No kgf-rs
change is needed — the code already reads `hdtc --version` and records what it
gets. This is purely an hdtc-side gap.
