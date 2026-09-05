#!/bin/sh
# Publish built bundles to the KGF bucket.
#
#   deploy/gke/publish.sh [--restart] [--dry-run] <bundle-dir>...
#
# A bundle dir is {root}/{dataset}/{version}, exactly as `kgf build --out` wrote
# it. Dataset and version come from the path and are cross-checked against the
# manifest, so a mislabeled directory cannot publish under the wrong name.
#
# Each version is uploaded in full and then its marker is written,
# {dataset}/{version}.complete, which is what makes sync.sh treat it as
# published. A version whose marker already exists is skipped: published
# versions are immutable, so a rebuild is a new version, never a re-upload.
#
# --restart bounces the server afterwards so it fetches and lists the new
# versions. Uses your own gcloud and kubectl credentials.
set -eu

BUCKET=${KGF_BUCKET:-gs://frink-kgf-bundles}
NAMESPACE=${KGF_NAMESPACE:-frink}
DEPLOYMENT=${KGF_DEPLOYMENT:-frink-kgf-server}

restart=0
dry=0
while [ $# -gt 0 ]; do
  case $1 in
    --restart) restart=1; shift ;;
    --dry-run) dry=1; shift ;;
    --) shift; break ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) break ;;
  esac
done
[ $# -gt 0 ] || { echo "usage: $0 [--restart] [--dry-run] <bundle-dir>..." >&2; exit 2; }

run() {
  if [ "$dry" = 1 ]; then echo "+ $*"; else "$@"; fi
}

published=0
for dir in "$@"; do
  dir=${dir%/}
  if [ ! -f "$dir/manifest.json" ]; then
    echo "$dir: no manifest.json here; not a bundle" >&2
    exit 1
  fi
  version=$(basename "$dir")
  dataset=$(basename "$(dirname "$dir")")
  case $version in
    .*) echo "$dir: a dot-prefixed version is an in-progress build, not a release" >&2; exit 1 ;;
  esac
  declared=$(python3 -c 'import json, sys; m = json.load(open(sys.argv[1])); print(m["id"], m["version"])' "$dir/manifest.json")
  if [ "$declared" != "$dataset $version" ]; then
    echo "$dir: path says '$dataset $version' but the manifest says '$declared'" >&2
    exit 1
  fi

  marker="$BUCKET/$dataset/$version.complete"
  if gcloud storage objects describe "$marker" >/dev/null 2>&1; then
    echo "$dataset/$version: already published, skipping"
    continue
  fi

  echo "$dataset/$version: uploading"
  run gcloud storage rsync --recursive "$dir" "$BUCKET/$dataset/$version"
  # The marker goes last, and only after rsync returned success.
  if [ "$dry" = 1 ]; then
    echo "+ echo $version | gcloud storage cp - $marker"
  else
    echo "$version" | gcloud storage cp - "$marker"
  fi
  published=$((published + 1))
done

echo "published $published version(s) to $BUCKET"
if [ "$restart" = 1 ] && [ "$published" -gt 0 ]; then
  run kubectl -n "$NAMESPACE" rollout restart "deploy/$DEPLOYMENT"
  run kubectl -n "$NAMESPACE" rollout status "deploy/$DEPLOYMENT"
fi
