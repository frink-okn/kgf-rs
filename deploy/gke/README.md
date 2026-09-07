# KGF on GKE, by hand

The serving half of KGF, applied with kubectl to the `frink` namespace on
`frink-cluster-0`. No kace involvement: bundles are built wherever there is an
HDT and a CPU, published to a bucket, and this Deployment mirrors the bucket
onto its own disk continuously, beside the running server. When kace later
automates the pipeline these manifests become its templates and `sync.sh`
becomes its sync step, under the same names, so it takes ownership of what is
already running.

## Pieces

| File | Object | Role |
|---|---|---|
| `pvc.yaml` | PVC `frink-kgf-bundles` | The bundle root. pd-ssd, ReadWriteOnce, a cache of the bucket. |
| `sync-configmap.yaml` | ConfigMap `frink-kgf-sync` | `sync.sh`, one mirror pass, and `sync-loop.sh`, which repeats it. |
| `deployment.yaml` | Deployment `frink-kgf-server` | `kgf serve` and a `sync` sidecar that mirrors the bucket alongside it. |
| `service.yaml` | Service `frink-kgf-service` | Port 80 to the pod's 8080. |
| `httproute.yaml` | HTTPRoute `frink-kgf-route` | `apps.okn.us/kgf` with the prefix stripped. |
| `healthcheckpolicy.yaml` | HealthCheckPolicy `frink-kgf-health-check` | The gateway probes `/healthz` on 8080. |

Outside this directory and created once by hand: the bucket
`gs://frink-kgf-bundles` (us-east4, uniform access, no lifecycle rule), the
service account `kgf-bundles-reader` with `roles/storage.objectViewer` on it,
and the Secret `kgf-bundles-reader` in `frink` holding that account's HMAC key
as `access_key` and `secret_key`.

## Why the sync runs beside the server, not in front of it

There used to be an init container that ran the same mirror before the server
was allowed to start. It made every publish an outage as long as the copy —
over twelve minutes for the largest bundle — and made a cold or resized PVC a
full mirror with the API down for all of it.

It bought nothing for that. The server fixes its catalog when it scans the
bundle root at startup and never rescans, so a version that lands under the
root while it is running is invisible until the next restart: there is no
window in which a half-copied bundle can be listed or opened, and `sync.sh`
stages under a dot-prefixed name the catalog skips even then. Adopting a bundle
was always a restart; only the waiting was avoidable. Adding version
directories under a live bundle root is also the one mutation the server's
mapping invariant permits, so this is the anticipated shape rather than a
liberty taken with it.

So the syncer is a sidecar in the server's pod, and the two are independent:

- **Startup no longer depends on the bucket.** The server comes up against
  whatever is on disk, immediately. A partial catalog beats no catalog — a
  missing dataset is one that appears at the next restart, and an empty root
  still answers the service descriptor.
- **Readiness says nothing about sync state.** Gating it would put bucket
  availability back on the serving path, and the server has no notion of
  "complete" to gate on.
- **A failed sync pass is a log line, not a restart.** A rotated credential
  delays new bundles instead of taking down an API that was serving fine
  without them.
- **It shares the pod because the PVC is ReadWriteOnce**, which binds to a node
  rather than a pod. A separate Job would need podAffinity onto the server's
  node and would then have to fit on a node Autopilot sized for the server
  alone; same pod is same node by construction, and no two syncers can race on
  one staging directory.

What that costs: the sidecar is billed for the life of the pod rather than
disappearing into Autopilot's `max(init, sum(containers))`, and its bytes
compete for page cache and disk IOPS with the server mapping bundles beside it.
Both are answered the same way — nothing waits for this sync, so it is pinned
small and unhurried. See the resource comment in `deployment.yaml`.

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
touched: published versions are immutable, so there is nothing to refresh, and
rewriting one under a live mapping is exactly what the server's invariant
forbids.

The version label is the lakeFS tag, verbatim (`v0.0.4`), and the dataset id
is the registry shortname. The server resolves `latest` by the manifest's
`created` time and then the label, not by directory name.

## Apply

```sh
kubectl apply -f deploy/gke/
```

Order does not matter; the PVC binds when the pod schedules. First start
against an empty bucket is expected and fine: the server starts with no
datasets, `https://apps.okn.us/kgf` answers the service descriptor, and the
sync sidecar reports zero fetched. That proves routing, storage, credentials
and the health check before any bundle exists.

## Verify

```sh
kubectl -n frink rollout status deploy/frink-kgf-server
kubectl -n frink logs deploy/frink-kgf-server -c sync
kubectl -n frink get httproute frink-kgf-route -o jsonpath='{.status.parents[0].conditions}'
curl -si -H 'Accept: application/json' https://apps.okn.us/kgf
curl -si https://apps.okn.us/kgf/ | head -20
```

The server's log is one JSON access record per response, and no longer one per
health probe: kubelet and the gateway between them poll `/healthz` several times
a second, and an uneventful probe is not recorded. A probe that failed, or that
took long enough to suggest the process could not schedule trivial work, still
is — so an empty probe log means healthy, not blind.

The sync log is a pass every five minutes: `no new versions` when the bucket
holds nothing the disk does not, and a `fetching`/`fetched` pair plus an
inventory when it does. What the disk holds right now, independent of what the
server has scanned:

```sh
kubectl -n frink exec deploy/frink-kgf-server -c sync -- ls /bundles
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
    --registry-prefixes --builder-image ghcr.io/frink-okn/kgf:v0.1.3 \
    --kgf target/release/kgf --hdtc ../hdtc/target/release/hdtc
```

`--all` in place of a shortname walks the whole registry; a KG already built at
its current tag is reported as existing and skipped, so the same command is a
safe re-run. The `--builder-image` should name the image the server runs, so
the recorded toolchain matches what serves the bundle.

## Publish a bundle

`publish.sh` uploads a version directory, writes the marker after the upload
has succeeded, and refuses to re-upload a version whose marker already exists.

```sh
deploy/gke/publish.sh --restart /Volumes/ssd/kgf/bundles/dreamkg/v0.0.4
```

Publishing and serving are two steps now, and `--restart` does both: it waits
until the sidecar has mirrored exactly the versions this run published, then
bounces the server so it scans them. The wait belongs to the publisher because
nothing else knows which versions to wait for. It also matters that it comes
first — a restart during a fetch discards the sidecar's staging directory and
makes that version start over, and completed versions survive a restart only
because they were already renamed into place.

The restart itself is a Recreate: the old pod stops and the new one starts
against a disk that already holds everything, so the gap is seconds rather than
a mirror. Without `--restart`, the new version is on disk within a sync
interval and is adopted by the next restart for any reason. Several directories
can be passed at once to publish a batch under one restart.

`KGF_WAIT_TIMEOUT` (default 5400s) bounds the wait; exceeding it reports what
is still missing and does not restart.

## Knobs

- Disk: `pvc.yaml` `storage`. Grows in place; pd-ssd IOPS grow with it.
  `sync.sh` refuses a version that would not leave `HEADROOM_MIB` (10 GiB) free
  rather than filling the volume the server is mapping.
- Server size: `deployment.yaml` resources. Memory is page cache for the
  mapped bundles; the 6.5 GiB per vCPU ratio Autopilot enforces is a whole-pod
  rule, so it is both containers summed that has to stay inside it.
- Sync cadence: `SYNC_INTERVAL` on the sidecar, seconds between passes.
- Sync transfer shape: `NUMWORKERS`, `CONCURRENCY`, `PART_SIZE_MIB`. Their
  product is the sidecar's buffer high-water mark, so raise them and its memory
  request together; s5cmd's defaults bound the same product at 64 GiB and will
  OOM any limit worth paying for.
- Image: pin a new `ghcr.io/frink-okn/kgf` tag in `deployment.yaml`. The
  server only reads bundles, so a server upgrade needs no rebuild.
