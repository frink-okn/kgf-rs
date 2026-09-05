#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyyaml"]
# ///
"""Render `kgf build` configs from the OKN registry.

Two jobs. Locally it produces a config for a knowledge graph so a real bundle
can be built from a real registry entry without hand-writing YAML. For kace, it
is the reference: `render()` below is the whole mapping, and porting it is
copying one function rather than rediscovering which registry field becomes
which config key.

Keeping this rendering in the build workflow avoids duplicating generated
configuration in committed files.

    ./tools/okn-build-config.py dreamkg                     # render one config
    ./tools/okn-build-config.py --all --check               # what registry CI runs
    ./tools/okn-build-config.py --all --out-dir /tmp/configs

and, to actually build bundles from knowledge graphs you already have locally:

    ./tools/okn-build-config.py dreamkg --build \
        --hdt-root demo --out-root /tmp/bundles
    ./tools/okn-build-config.py --all --build \
        --hdt-root demo --out-root /tmp/bundles --prefix-table demo/prefixes.json

`--hdt-root` expects the layout `{root}/{shortname}/{version}/data.hdt`, which is
both the bundle layout and what `demo/` already holds; the version directory's
name becomes the bundle's version label. A knowledge graph the registry lists
but the root does not hold is skipped, so `--all` builds what is there.

To build from the knowledge graphs' published releases rather than local files,
add `--lakefs`: each KG's `hdt/graph.hdt` is downloaded from its lakeFS repo at
the latest semver tag (or `--tag`) into that same `--hdt-root` layout, the tag
becomes the version label, and the manifest records the input as
`lakefs://{repo}/{commit}/hdt/graph.hdt`, pinned to the commit so the bundle
says exactly what it was built from. Pass `--builder-image` so it records that
too. `lakectl` must be on PATH and configured with credentials; only the
endpoint is set here (`--lakefs-endpoint`).

    ./tools/okn-build-config.py dreamkg --build --lakefs \
        --hdt-root /var/kgf/hdt --out-root /var/kgf/bundles \
        --registry-prefixes --builder-image ghcr.io/frink-okn/kgf:v0.1.0

With no --registry it reads the same URL kace does.
"""

from __future__ import annotations

import argparse
import copy
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path
from typing import Any

import yaml

# The URL kace resolves the registry from (`config.py`'s `kg_config_url`).
REGISTRY_URL = (
    "https://raw.githubusercontent.com/frink-okn/okn-registry"
    "/refs/heads/main/docs/registry/kgs.yaml"
)

# kace already mints this for the VoID step, so a bundle's VoID and the okn-void
# graph describe one resource. It is not ours to invent a different one.
DATASET_IRI = "https://purl.org/okn/frink/kg/{shortname}"

# The shared OKN prefix table, the base layer under every KG's own prefixes.
REGISTRY_PREFIXES_URL = (
    "https://raw.githubusercontent.com/frink-okn/okn-registry"
    "/refs/heads/main/docs/registry/prefixes.yaml"
)

# The lakeFS server the OKN repositories live on. `lakectl` supplies its own
# credentials from its config file; only the endpoint is pinned here, because
# the repositories moved hosts and a stale `~/.lakectl.yaml` would otherwise
# point at the old one.
LAKEFS_ENDPOINT = "https://repository.okn.us"

# Where kace's conversion leaves the HDT in every repository.
LAKEFS_HDT_PATH = "hdt/graph.hdt"

# The config schema version in `crates/kgf/src/build/config.rs`.
SCHEMA = 1

_SEMVER = re.compile(r"^v?(\d+)\.(\d+)\.(\d+)$")


def semver_key(label: str) -> tuple[int, int, int] | None:
    """`v0.2.10` -> (0, 2, 10); None for anything that is not a semver label."""
    match = _SEMVER.match(label)
    return tuple(int(part) for part in match.groups()) if match else None


def render(kg: dict[str, Any]) -> dict[str, Any]:
    """One registry entry to one `kgf build` config.

    The whole mapping, and the piece kace ports. Two halves:

    * `dataset:` is *derived* from fields the registry already has for other
      purposes, with no invention beyond the dataset IRI kace already mints.
    * `semantics:` and `contents:` are `frink-options.kgf` **verbatim**. That is
      deliberate: the schema is owned by kgf-rs, so a caller copies the subtree
      rather than translating it field by field, and a new config key needs no
      change here or in kace. `kgf build --check-config` is the validator.
    """
    shortname = kg.get("shortname")
    if not shortname:
        raise ValueError(f"registry entry has no shortname: {kg.get('title') or kg}")

    dataset: dict[str, Any] = {
        "id": shortname,
        "iri": DATASET_IRI.format(shortname=shortname),
    }
    for key in ("title", "description", "license", "homepage"):
        # Blank is an omission, not a fact: `kgf build` refuses a whitespace
        # title rather than publishing one.
        value = (kg.get(key) or "").strip()
        if value:
            dataset[key] = value

    publisher = _publisher(kg)
    if publisher:
        dataset["publisher"] = publisher

    config: dict[str, Any] = {"schema": SCHEMA, "dataset": dataset}

    options = kg.get("frink-options") or {}
    for section, block in (options.get("kgf") or {}).items():
        config[section] = copy.deepcopy(block)
    return config


def _publisher(kg: dict[str, Any]) -> dict[str, Any] | None:
    """The first named contact, handling both registry spellings.

    Entries carry either `contacts:` (a list) or the deprecated `contact:` (one
    object); kace's model migrates the second into the first, and so does this.
    """
    contacts = kg.get("contacts")
    if contacts is None and kg.get("contact") is not None:
        contacts = [kg["contact"]]
    for contact in contacts or []:
        name = (contact.get("label") or "").strip()
        if not name:
            continue
        publisher: dict[str, Any] = {"name": name}
        email = contact.get("email")
        if isinstance(email, str):
            email = [part.strip() for part in email.split(",") if part.strip()]
        if email:
            publisher["contact"] = f"mailto:{email[0]}"
        return publisher
    return None


def load_registry(source: str) -> list[dict[str, Any]]:
    if source.startswith(("http://", "https://")):
        with urllib.request.urlopen(source) as response:  # noqa: S310 — fixed scheme
            text = response.read().decode("utf-8")
    else:
        text = Path(source).read_text(encoding="utf-8")
    document = yaml.safe_load(text) or {}
    return document.get("kgs") or []


def find_local_hdt(root: Path, shortname: str) -> tuple[Path, str] | None:
    """The newest `{root}/{shortname}/{version}/data.hdt`, and its version.

    The version label comes from the directory rather than a flag because that
    is where a real bundle's does: `kgf build` reads it from `--out`'s last
    component, and taking it from the source keeps a rebuilt bundle named after
    the release it was built from.
    """
    # Semver order where the label is one, so `v0.2.10` outranks `v0.2.9`;
    # plain name order otherwise.
    versions = sorted(
        (child for child in (root / shortname).glob("*/data.hdt")),
        key=lambda hdt: (semver_key(hdt.parent.name) or (), hdt.parent.name),
    )
    if not versions:
        return None
    newest = versions[-1]
    return newest, newest.parent.name


def lakefs_repo(kg: dict[str, Any]) -> str | None:
    """The lakeFS repository kace converts this KG in, from the registry."""
    return (kg.get("frink-options") or {}).get("lakefs-repo") or None


def lakectl(args: list[str], binary: str, endpoint: str) -> str:
    env = dict(os.environ, LAKECTL_SERVER_ENDPOINT_URL=endpoint)
    result = subprocess.run(
        [binary, *args], env=env, capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise RuntimeError(f"lakectl {' '.join(args)} failed: {detail}")
    return result.stdout


def lakefs_tags(repo: str, binary: str, endpoint: str) -> dict[str, str]:
    """Semver tag -> commit id for `repo`, from `lakectl tag list`."""
    out = lakectl(["tag", "list", f"lakefs://{repo}", "--amount", "1000"], binary, endpoint)
    tags: dict[str, str] = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2 and semver_key(parts[0]) is not None:
            tags[parts[0]] = parts[1]
    return tags


def lakefs_release(
    repo: str, tag: str | None, binary: str, endpoint: str
) -> tuple[str, str]:
    """(tag, commit) for the requested tag, or the highest semver tag."""
    tags = lakefs_tags(repo, binary, endpoint)
    if not tags:
        raise RuntimeError(f"{repo}: no semver tags")
    if tag is not None:
        if tag not in tags:
            raise RuntimeError(f"{repo}: no tag {tag!r} (have {', '.join(sorted(tags))})")
        return tag, tags[tag]
    latest = max(tags, key=lambda label: semver_key(label) or (0, 0, 0))
    return latest, tags[latest]


def lakefs_fetch_hdt(repo: str, tag: str, dest: Path, binary: str, endpoint: str) -> bool:
    """Download `hdt/graph.hdt` at `tag` to `dest` unless it is already there.

    Written to a `.part` sibling and renamed, so an interrupted download is
    never mistaken for the input on the next run. Returns whether a download
    happened.
    """
    if dest.is_file():
        return False
    dest.parent.mkdir(parents=True, exist_ok=True)
    part = dest.with_name(dest.name + ".part")
    lakectl(
        ["fs", "download", f"lakefs://{repo}/{tag}/{LAKEFS_HDT_PATH}", str(part)],
        binary,
        endpoint,
    )
    part.replace(dest)
    return True


def resolve_prefix_table(spec: str) -> Path:
    """A local path as given; a URL fetched once into a temporary file."""
    if not spec.startswith(("http://", "https://")):
        return Path(spec)
    with urllib.request.urlopen(spec) as response:  # noqa: S310 — fixed scheme
        data = response.read()
    handle = tempfile.NamedTemporaryFile(prefix="prefixes-", suffix=".yaml", delete=False)
    with handle:
        handle.write(data)
    return Path(handle.name)


def build(
    config: dict[str, Any],
    hdt: Path,
    out: Path,
    kgf: str,
    hdtc: str | None,
    source_url: str | None = None,
    builder_image: str | None = None,
) -> tuple[bool, str]:
    """Build one bundle, feeding the config on stdin.

    `--config -` rather than a temporary file, for the same reason kace will
    pass it through a configmap: the config is rendered, not stored, so there is
    nothing to leave behind or let go stale.

    `source_url` is what the manifest records as the input's origin. It defaults
    to the local file, which is honest for a local build and useless for
    re-deriving one; the lakeFS mode passes the commit-pinned `lakefs://` URL
    instead. The builder verifies the input's digest itself either way.
    """
    command = [
        kgf, "build",
        "--config", "-",
        "--out", str(out),
        "--hdt", str(hdt),
        "--source-url", source_url or hdt.resolve().as_uri(),
    ]
    if builder_image:
        command += ["--builder-image", builder_image]
    if hdtc:
        command += ["--hdtc", hdtc]
    result = subprocess.run(
        command,
        input=yaml.safe_dump(config, sort_keys=False),
        capture_output=True,
        text=True,
    )
    return result.returncode == 0, (result.stdout + result.stderr).strip()


def check(config: dict[str, Any], kgf: str) -> tuple[bool, str]:
    """Validate one rendered config with the real validator.

    Not a reimplementation of the schema in Python: `--check-config` needs no output
    directory and no input, which is what lets this run over the whole registry.
    """
    result = subprocess.run(
        [kgf, "build", "--config", "-", "--check-config"],
        input=yaml.safe_dump(config, sort_keys=False),
        capture_output=True,
        text=True,
    )
    return result.returncode == 0, (result.stderr or result.stdout).strip()


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("shortname", nargs="?", help="the KG to render")
    parser.add_argument("--all", action="store_true", help="render every entry")
    parser.add_argument(
        "--registry",
        default=REGISTRY_URL,
        help="registry path or URL (default: the one kace uses)",
    )
    parser.add_argument("--out-dir", type=Path, help="write {shortname}.yaml here")
    parser.add_argument(
        "--build", action="store_true", help="build the bundle, not just the config"
    )
    parser.add_argument("--hdt", type=Path, help="the HDT to build from")
    parser.add_argument(
        "--hdt-root",
        type=Path,
        help="find HDTs as {root}/{shortname}/{version}/data.hdt",
    )
    parser.add_argument("--out-root", type=Path, help="publish into {root}/{id}/{version}")
    parser.add_argument("--hdtc", help="the hdtc binary kgf build should use")
    parser.add_argument(
        "--prefix-table",
        help="set contents.stats.prefix_tables to this path or URL; its prefixes are "
        "declared in the manifest and used for CURIEs and display, under the KG's own",
    )
    parser.add_argument(
        "--registry-prefixes",
        action="store_true",
        help=f"use the registry's shared prefix table ({REGISTRY_PREFIXES_URL}) as that base",
    )
    # Machine-local resource settings. These describe the builder, not the
    # bundle, so they are flags here rather than registry data: the same KG is
    # built on a laptop one day and a cluster job the next.
    parser.add_argument(
        "--memory-limit",
        help="soft memory limit passed to every hdtc step, as hdtc spells it (e.g. 32G)",
    )
    parser.add_argument(
        "--temp-dir",
        type=Path,
        help="parent directory for the build's per-step scratch space "
        "(default: beside the output); put it on the fastest disk with room for "
        "several times the HDT",
    )
    parser.add_argument(
        "--threads",
        type=int,
        help="indexing threads for the text index build (default: hdtc decides)",
    )
    parser.add_argument(
        "--lakefs",
        action="store_true",
        help="with --build: fetch each KG's tagged HDT from lakeFS into --hdt-root "
        "and record the commit-pinned lakefs:// URL as the source",
    )
    parser.add_argument("--tag", help="with --lakefs: build this tag instead of the latest")
    parser.add_argument("--lakefs-endpoint", default=LAKEFS_ENDPOINT, help="lakeFS server URL")
    parser.add_argument("--lakectl", default="lakectl", help="the lakectl binary")
    parser.add_argument(
        "--source-url",
        help="with --build for one KG: record this as the input's origin "
        "(default: the local file; --lakefs sets it)",
    )
    parser.add_argument(
        "--builder-image",
        help="image reference to record in the manifest, e.g. ghcr.io/frink-okn/kgf:v0.1.0",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="validate each config with `kgf build --check-config`",
    )
    parser.add_argument("--kgf", default="kgf", help="the kgf binary --check uses")
    parser.add_argument("--json", action="store_true", help="emit JSON, not YAML")
    args = parser.parse_args()

    if bool(args.shortname) == bool(args.all):
        parser.error("give a shortname or --all, not both or neither")
    if args.build:
        if not args.out_root:
            parser.error("--build needs --out-root")
        if not (args.hdt or args.hdt_root):
            parser.error("--build needs --hdt or --hdt-root")
        if args.hdt and args.all:
            parser.error("--hdt names one file; use --hdt-root with --all")
    if args.lakefs:
        if not args.build or not args.hdt_root:
            parser.error("--lakefs needs --build and --hdt-root")
        if args.source_url:
            parser.error("--lakefs sets the source URL; do not pass --source-url")
    elif args.tag:
        parser.error("--tag only means something with --lakefs")
    if args.source_url and args.all:
        parser.error("--source-url names one input; it cannot go with --all")
    if args.prefix_table and args.registry_prefixes:
        parser.error("give --prefix-table or --registry-prefixes, not both")
    prefix_table = None
    if args.prefix_table or args.registry_prefixes:
        prefix_table = resolve_prefix_table(args.prefix_table or REGISTRY_PREFIXES_URL)

    entries = load_registry(args.registry)
    if args.shortname:
        entries = [kg for kg in entries if kg.get("shortname") == args.shortname]
        if not entries:
            print(f"no registry entry named {args.shortname!r}", file=sys.stderr)
            return 1

    failures = 0
    for kg in entries:
        try:
            config = render(kg)
        except ValueError as error:
            print(f"skipped: {error}", file=sys.stderr)
            failures += 1
            continue

        if prefix_table is not None:
            contents = config.setdefault("contents", {})
            stats = contents.setdefault("stats", {})
            stats["prefix_tables"] = [str(prefix_table)]

        resources = {
            "memory_limit": args.memory_limit,
            "temp_dir": str(args.temp_dir) if args.temp_dir else None,
            "threads": args.threads,
        }
        resources = {key: value for key, value in resources.items() if value is not None}
        if resources:
            config.setdefault("resources", {}).update(resources)

        shortname = config["dataset"]["id"]

        if args.build:
            source_url = args.source_url
            if args.lakefs:
                repo = lakefs_repo(kg)
                if not repo:
                    print(f"{shortname:<28} skipped (no lakefs-repo in the registry)")
                    continue
                try:
                    tag, commit = lakefs_release(repo, args.tag, args.lakectl, args.lakefs_endpoint)
                    hdt = args.hdt_root / shortname / tag / "data.hdt"
                    if lakefs_fetch_hdt(repo, tag, hdt, args.lakectl, args.lakefs_endpoint):
                        print(f"{shortname:<28} fetched {repo}@{tag} -> {hdt}")
                except RuntimeError as error:
                    print(f"{shortname:<28} FAILED")
                    print(f"  {error}", file=sys.stderr)
                    failures += 1
                    continue
                version = tag
                source_url = f"lakefs://{repo}/{commit}/{LAKEFS_HDT_PATH}"
            elif args.hdt:
                hdt, version = args.hdt, args.hdt.parent.name
            else:
                found = find_local_hdt(args.hdt_root, shortname)
                if not found:
                    print(f"{shortname:<28} skipped (no local HDT)")
                    continue
                hdt, version = found
            out = args.out_root / shortname / version
            if out.exists():
                # `kgf build` would refuse too; saying so here keeps a re-run
                # over --all from counting every already-built KG as a failure.
                print(f"{shortname:<28} exists   {out}")
                continue
            ok, detail = build(
                config, hdt, out, args.kgf, args.hdtc,
                source_url=source_url, builder_image=args.builder_image,
            )
            print(f"{shortname:<28} {'built ' + str(out) if ok else 'FAILED'}")
            if not ok:
                print(f"  {detail}", file=sys.stderr)
                failures += 1
            continue

        if args.check:
            ok, detail = check(config, args.kgf)
            status = "ok" if ok else "FAILED"
            print(f"{shortname:<28} {status}")
            if not ok:
                print(f"  {detail}", file=sys.stderr)
                failures += 1
            continue

        text = (
            json.dumps(config, indent=2) + "\n"
            if args.json
            else yaml.safe_dump(config, sort_keys=False)
        )
        if args.out_dir:
            args.out_dir.mkdir(parents=True, exist_ok=True)
            suffix = "json" if args.json else "yaml"
            target = args.out_dir / f"{config['dataset']['id']}.{suffix}"
            target.write_text(text, encoding="utf-8")
            print(target)
        else:
            sys.stdout.write(text)

    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
