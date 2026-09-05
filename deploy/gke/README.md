# KGF on GKE, by hand

The serving half of KGF, applied with kubectl to the `frink` namespace on
`frink-cluster-0`. No kace involvement: bundles are built wherever there is an
HDT and a CPU, published to a bucket, and this Deployment mirrors the bucket
onto its own disk at start. When kace later automates the pipeline these
manifests become its templates and `sync.sh` becomes its sync Job, under the
same names, so it takes ownership of what is already running.

## Pieces

| File | Object | Role |
|---|---|---|
| `pvc.yaml` | PVC `frink-kgf-bundles` | The bundle root. pd-ssd, ReadWriteOnce, a cache of the bucket. |
| `sync-configmap.yaml` | ConfigMap `frink-kgf-sync` | `sync.sh`, run by the init container. |
| `deployment.yaml` | Deployment `frink-kgf-server` | Init container syncs the bucket, then `kgf serve`. |
| `service.yaml` | Service `frink-kgf-service` | Port 80 to the pod's 8080. |
| `httproute.yaml` | HTTPRoute `frink-kgf-route` | `apps.okn.us/kgf` with the prefix stripped. |
| `healthcheckpolicy.yaml` | HealthCheckPolicy `frink-kgf-health-check` | The gateway probes `/` on 8080. |

Outside this directory and created once by hand: the bucket
`gs://frink-kgf-bundles` (us-east4, uniform access, no lifecycle rule), the
service account `kgf-bundles-reader` with `roles/storage.objectViewer` on it,
and the Secret `kgf-bundles-reader` in `frink` holding that account's HMAC key
as `access_key` and `secret_key`.

## Bucket layout

Identical to the bundle root the server reads:

```
{dataset}/{version}/...        the bundle, exactly as `kgf build` wrote it
{dataset}/{version}.complete   written last, after every object above it
```

The marker is the publication. `sync.sh` only fetches versions that have one,
so an upload in progress is never served, and it copies each version into a
dot-prefixed staging directory and renames it into place, so a server started
mid-copy never lists a partial bundle. A version already on disk is never
touched: published versions are immutable, so there is nothing to refresh.

The version label is the lakeFS tag, verbatim (`v0.0.4`), and the dataset id
is the registry shortname. The server resolves `latest` by the manifest's
`created` time and then the label, not by directory name.

## Apply

```sh
kubectl apply -f deploy/gke/
```

Order does not matter; the PVC binds when the pod schedules. First start
against an empty bucket is expected and fine: `sync.sh` reports zero fetched,
the server starts with no datasets, and `https://apps.okn.us/kgf` answers the
service descriptor. That proves routing, storage, credentials and the health
check before any bundle exists.

## Verify

```sh
kubectl -n frink rollout status deploy/frink-kgf-server
kubectl -n frink logs deploy/frink-kgf-server -c sync
kubectl -n frink get httproute frink-kgf-route -o jsonpath='{.status.parents[0].conditions}'
curl -si -H 'Accept: application/json' https://apps.okn.us/kgf
curl -si https://apps.okn.us/kgf/ | head -20
```

Things to look for from outside the cluster: every link in a descriptor and
every IRI in a fragment starts with `/kgf/` or `https://apps.okn.us/kgf/`;
version resources answer under both `/kgf/{dataset}/v/{version}/` and
`/kgf/{dataset}/latest/` (there is no resource at the version root itself);
and a request sent with `-X QUERY` is not rejected by the gateway before it
reaches the pod, since the load balancer may filter unknown methods.

The bundle volume is mounted read-write on purpose, even though the server
only reads: hdtc's text index reader takes a lock file on open, and a read-only
mount makes every bundle that carries a text index fail with "could not be
opened". See the comment in `deployment.yaml`.

## Build a bundle

Bundles are built from each knowledge graph's published lakeFS release by the
tool in `tools/`, which renders the `kgf build` config from the registry entry,
fetches the tagged `hdt/graph.hdt`, and records the commit-pinned `lakefs://`
URL and the builder image in the manifest. That provenance is what lets kace
adopt a hand-built bundle later instead of rebuilding it.

```sh
./tools/okn-build-config.py dreamkg --build --lakefs \
    --hdt-root /Volumes/ssd/kgf/hdt --out-root /Volumes/ssd/kgf/bundles \
    --registry-prefixes --builder-image ghcr.io/frink-okn/kgf:v0.1.0 \
    --kgf target/release/kgf --hdtc ../hdtc/target/release/hdtc
```

`--all` in place of a shortname walks the whole registry; a KG already built at
its current tag is reported as existing and skipped, so the same command is a
safe re-run. The `--builder-image` should name the image the server runs, so
the recorded toolchain matches what serves the bundle.

## Publish a bundle

`publish.sh` uploads a version directory, writes the marker after the upload
has succeeded, and refuses to re-upload a version whose marker already exists.
With `--restart` it bounces the server so the new versions are fetched and
listed.

```sh
deploy/gke/publish.sh --restart /Volumes/ssd/kgf/bundles/dreamkg/v0.0.4
```

The restart is a Recreate: the old pod stops, the new one syncs only what is
missing and starts, a gap of a minute or two. Nothing already on the disk is
re-fetched. Several directories can be passed at once to publish a batch under
one restart.

## Knobs

- Disk: `pvc.yaml` `storage`. Grows in place; pd-ssd IOPS grow with it.
- Server size: `deployment.yaml` resources. Memory is page cache for the
  mapped bundles; keep it at or under 6.5 GiB per vCPU or Autopilot raises
  the CPU to match.
- Image: pin a new `ghcr.io/frink-okn/kgf` tag in `deployment.yaml`. The
  server only reads bundles, so a server upgrade needs no rebuild.
