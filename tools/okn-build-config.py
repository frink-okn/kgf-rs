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

`notes/build-bundle.md` §6 is the design and explains why this rendering belongs
in the build workflow rather than in committed config files.

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

With no --registry it reads the same URL kace does.
"""

from __future__ import annotations

import argparse
import copy
import json
import subprocess
import sys
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

# The config schema version in `crates/kgf/src/build/config.rs`.
SCHEMA = 1


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
    versions = sorted(
        (child for child in (root / shortname).glob("*/data.hdt")),
        key=lambda hdt: hdt.parent.name,
    )
    if not versions:
        return None
    newest = versions[-1]
    return newest, newest.parent.name


def build(
    config: dict[str, Any],
    hdt: Path,
    out: Path,
    kgf: str,
    hdtc: str | None,
) -> tuple[bool, str]:
    """Build one bundle, feeding the config on stdin.

    `--config -` rather than a temporary file, for the same reason kace will
    pass it through a configmap: the config is rendered, not stored, so there is
    nothing to leave behind or let go stale.
    """
    command = [
        kgf, "build",
        "--config", "-",
        "--out", str(out),
        "--hdt", str(hdt),
        "--source-url", hdt.resolve().as_uri(),
    ]
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

    Not a reimplementation of the schema in Python — that is exactly the split
    `notes/build-bundle.md` §6 argues against. `--check-config` needs no output
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
        type=Path,
        help="set contents.stats.prefix_tables to this path, for a local build",
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

        if args.prefix_table:
            contents = config.setdefault("contents", {})
            stats = contents.setdefault("stats", {})
            stats["prefix_tables"] = [str(args.prefix_table)]

        shortname = config["dataset"]["id"]

        if args.build:
            if args.hdt:
                hdt, version = args.hdt, args.hdt.parent.name
            else:
                found = find_local_hdt(args.hdt_root, shortname)
                if not found:
                    print(f"{shortname:<28} skipped (no local HDT)")
                    continue
                hdt, version = found
            out = args.out_root / shortname / version
            ok, detail = build(config, hdt, out, args.kgf, args.hdtc)
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
