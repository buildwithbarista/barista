# roastery

`roastery` is the remote artifact-cache server for the Barista build
tool. It speaks the **barista-protocol** (a small REST/JSON surface
tailored to Barista clients) alongside the **Remote Execution API**
(REAPI / `bazel-remote-style` gRPC), backed by a local content-
addressed object store with an optional upstream registry for
miss-fill.

Conceptually, the roastery is the team-shared layer above each
developer's local Barista cache: a CI worker, a build-farm node, or
a contributor on a fast link can resolve a coordinate once, store
the resulting jar + POM + metadata in the roastery, and every
subsequent client in the same pool gets a hot read instead of
re-fetching from Maven Central.

This crate ships a single binary, `roastery`, plus a small library
the integration tests and downstream embedders consume.

## Status

The current code carries the v0.1 surface for storage, the
barista-protocol HTTP handler, the Bazel REAPI gRPC handler (CAS +
ByteStream + Capabilities), upstream-on-miss, the ops endpoints
(`/healthz`, `/metrics`, `/version`), and the authentication surface
(bearer tokens + mTLS).

## Running locally

```bash
cargo run -p roastery
```

By default the server binds `127.0.0.1:7878` and creates
`./.roastery-data/` as its storage root. Override either with env
vars (see next section).

A quick check from another terminal:

```bash
curl -i http://127.0.0.1:7878/v1/health
# HTTP/1.1 200 OK
# content-type: application/json
# {"status":"ok","protocol":"barista","version":"v1"}
```

To shut the server down, send `SIGINT` (Ctrl-C) or — on Unix —
`SIGTERM`. The graceful-shutdown path stops accepting new
connections; in-flight requests complete on their own.

## HTTP API

The server speaks the **barista-protocol** — a small, fixed REST/JSON
surface every Barista client release knows how to talk to. All routes
live under `/v1/`; the SHA-256 of a blob is the only identifier used
in URLs and on the wire.

| Method | Path                              | Purpose                                                            |
|--------|-----------------------------------|--------------------------------------------------------------------|
| `GET`  | `/v1/cas/sha256/{digest}`         | Fetch a blob. Streams the body. 404 if absent, 400 if malformed.   |
| `HEAD` | `/v1/cas/sha256/{digest}`         | Existence check. Same headers as `GET`, empty body.                |
| `PUT`  | `/v1/cas/sha256/{digest}`         | Upload + verify. 201 on success (incl. re-upload), 400 on mismatch.|
| `POST` | `/v1/cas/missing`                 | Batch presence check (≤ 1000 entries per call).                    |
| `GET`  | `/v1/health`                      | Barista-protocol liveness.                                         |
| `GET`  | `/v1/capabilities`                | Server feature flags + the configured storage backend.             |

### Response headers (CAS endpoints)

Every successful `GET`, `HEAD`, and `PUT` against `/v1/cas/sha256/…`
sets:

- `Content-Type: application/octet-stream` (GET only)
- `Content-Length: <size>` — the blob length in bytes.
- `X-Barista-Digest: sha256:<hex>` — echo of the canonical digest.

### `PUT` semantics

- The path digest is authoritative — bytes are hashed as they stream
  in and the put fails with `400 BAR-CAS-001` if the result doesn't
  match.
- Optional `Content-SHA256: sha256:<hex>` (or bare `<hex>`) request
  header asserts the same digest at the header layer; a mismatch
  between the header and the URL is a `400 BAR-CAS-001`.
- Re-PUTting an existing blob is idempotent and returns `201` — under
  SHA-256, "same digest" is by definition "same bytes."

### `cas/missing` shape

Request:

```json
{ "digests": ["sha256:<hex>", "<hex>", ...] }
```

Bare hex and `sha256:`-prefixed entries are both accepted. Responses
always emit the canonical `sha256:` prefix:

```json
{ "missing": ["sha256:<hex>", ...] }
```

`missing` contains only the subset of supplied digests that are NOT
present in the store. Submitting more than 1000 entries in a single
request returns `413 BAR-CAS-004` — clients should batch.

### Error body shape

Every non-2xx response carries a JSON error body with a stable code:

```json
{
  "code": "BAR-CAS-001",
  "message": "digest mismatch",
  "expected": "...",
  "actual": "..."
}
```

`expected` and `actual` are only set on the digest-mismatch code; the
generic shape is `{ "code", "message" }`.

| Code          | HTTP | Meaning                                                   |
|---------------|------|-----------------------------------------------------------|
| `BAR-CAS-001` | 400  | Digest in URL/header disagreed with the body's hash.      |
| `BAR-CAS-002` | 400  | Digest string was not a 64-char lowercase hex SHA-256.    |
| `BAR-CAS-003` | 501  | Storage backend is not yet implemented (S3/GCS stubs).    |
| `BAR-CAS-004` | 413  | Batch request exceeded the per-call cap of 1000 entries.  |
| `BAR-CAS-005` | 400  | Request body did not match the documented JSON schema.    |
| `BAR-CAS-099` | 500  | Unclassified internal/storage I/O failure.                |
| `BAR-CAS-404` | 404  | Blob not present in the store.                            |

Auth failures share the same JSON shape under a separate code
namespace:

| Code           | HTTP | Meaning                                                  |
|----------------|------|----------------------------------------------------------|
| `BAR-AUTH-001` | 401  | Request lacked valid bearer/mTLS credentials.            |
| `BAR-AUTH-002` | 403  | Credentials valid but the principal isn't authorised.    |

### Curl recipes

```bash
# Upload a blob (digest is authoritative).
BLOB=$(echo -n "hello, roastery" | sha256sum | cut -d' ' -f1)
echo -n "hello, roastery" | curl -i --data-binary @- \
    -X PUT "http://127.0.0.1:7878/v1/cas/sha256/$BLOB"

# Fetch it back.
curl -i "http://127.0.0.1:7878/v1/cas/sha256/$BLOB"

# Existence check.
curl -I "http://127.0.0.1:7878/v1/cas/sha256/$BLOB"

# Batch presence.
curl -i -X POST "http://127.0.0.1:7878/v1/cas/missing" \
    -H 'content-type: application/json' \
    -d "{\"digests\":[\"sha256:$BLOB\",\"sha256:000…0\"]}"
```

## REAPI (Bazel CAS gRPC)

Alongside the barista-native HTTP surface, the server speaks the Bazel
**Remote Execution API** (REAPI) over gRPC, so Bazel-ecosystem clients
can use roastery as a remote cache. **Both protocols front the same
content-addressed store** — a blob uploaded over HTTP is immediately
readable over gRPC and vice versa.

The gRPC services mount on the same listener as the HTTP routes (gRPC
is HTTP/2 with a distinct `application/grpc` content type and
service-qualified paths, so the two never collide). No separate port is
needed.

### Services implemented (v0.1)

| Service / RPC | Behaviour |
|---|---|
| `ContentAddressableStorage.FindMissingBlobs` | Returns the supplied digests not present in the store (the gRPC analogue of `POST /v1/cas/missing`). |
| `ContentAddressableStorage.BatchUpdateBlobs` | Uploads small blobs; each is verified against its claimed digest with a per-blob status (`OK` / `INVALID_ARGUMENT`). |
| `ContentAddressableStorage.BatchReadBlobs` | Reads small blobs by digest; per-blob status (`OK` + data / `NOT_FOUND`). |
| `ContentAddressableStorage.GetTree` | **`UNIMPLEMENTED`** — roastery is a flat CAS, not a Merkle store (see below). |
| `ContentAddressableStorage.SplitBlob` / `SpliceBlob` | **`UNIMPLEMENTED`** — no chunking function is advertised in v0.1. |
| `google.bytestream.ByteStream.Read` | Streams a blob out by resource name. |
| `google.bytestream.ByteStream.Write` | Streams a blob in; the bytes are verified against the resource-name digest. |
| `google.bytestream.ByteStream.QueryWriteStatus` | **`UNIMPLEMENTED`** — v0.1 requires a single contiguous write (no resumable uploads). |
| `Capabilities.GetCapabilities` | Advertises `digest_functions: [SHA256]`, the batch-size cap, the symlink strategy, and API version v2. |

The REAPI **Execution** and **ActionCache** services are intentionally
**not** implemented: roastery v0.1 is a content cache, not a remote
executor. The capabilities response advertises no execution
capabilities and disables Action Cache updates.

### Batch vs ByteStream

`BatchUpdateBlobs` / `BatchReadBlobs` are for *small* blobs and are
bounded by `max_batch_total_size_bytes` (4 MiB, advertised via
`Capabilities`). Larger blobs go through the `ByteStream` service, which
streams chunk-by-chunk and never buffers a whole blob in memory.

### Resource-name grammar

`ByteStream` overlays a path-like grammar on its opaque
`resource_name`. roastery serves the uncompressed (`IDENTITY`) forms:

- **Read:** `{instance_name}/blobs/{hash}/{size}`
- **Write:** `{instance_name}/uploads/{uuid}/blobs/{hash}/{size}`

`{instance_name}` may be empty and may itself contain `/`. The
`compressed-blobs/{compressor}/…` variant is rejected with
`UNIMPLEMENTED` (the store keeps raw bytes and does not transcode). On a
`Write`, the streamed bytes are hashed and checked against `{hash}`; a
mismatch is `INVALID_ARGUMENT`. v0.1 requires a single contiguous write
from offset 0 — resumable offsets are a follow-up.

### Why `GetTree` is `UNIMPLEMENTED`

REAPI's `GetTree` walks a Merkle tree of `Directory` nodes. roastery is
a *flat* content-addressed store: it has no directory tree to descend.
Returning `UNIMPLEMENTED` is the honest answer for a flat CAS at v0.1 —
a fabricated single-level response would misrepresent the store. A
future Merkle-aware backend could implement it without a wire-contract
change.

### Auth

The CAS data services (`ContentAddressableStorage` + `ByteStream`) sit
behind the same auth posture as the barista-protocol CAS routes: when
bearer auth is configured, a valid `authorization: Bearer <token>`
metadata entry is required (verified against the same tokens the HTTP
layer loaded), and an unauthenticated call is rejected with gRPC
`UNAUTHENTICATED`. When mTLS is configured, the TLS handshake already
required a CA-chained client certificate, so a connection that reaches a
handler is transport-authenticated. The `Capabilities` service is left
unauthenticated — it is the version-negotiation surface, exactly like
the public HTTP `/v1/capabilities`. The data services are never more
open than their HTTP counterparts.

> v0.2 follow-up: the gRPC interceptor gates access but does not yet
> capture a per-call mTLS *subject* the way the HTTP path does; RBAC in
> v0.2 will attach a `Principal` over gRPC.

### Proto vendoring + pin policy

The REAPI bindings are generated locally at build time by
`tonic-prost-build` from `.proto` files **vendored** under `proto/`. The
exact upstream commits (Bazel `remote-apis` and `googleapis`) are pinned
and recorded — with source URLs and license attribution — in
[`proto/REVISIONS.txt`](proto/REVISIONS.txt). Bumping a pin is a
one-line change in that file plus a re-fetch and rebuild; the generated
code is never checked in. The build needs **no system `protoc`** — a
vendored `protoc` (via the `protoc-bin-vendored` build dependency)
supplies both the compiler and the `google/protobuf/*` well-known-type
includes, so CI and contributors don't have to install the protobuf
toolchain.

> Conformance: the v0.1 wire contract + cross-protocol storage sharing
> are proven by Rust `tonic`-client round-trip tests
> (`tests/reapi.rs`). Running the full Go-based `bazel-remote`
> conformance harness against roastery is a deferred follow-up.

## Upstream-on-miss

When a `GET /v1/cas/sha256/{digest}` lands on a digest the local store
doesn't have, the server can transparently fetch the artifact from an
upstream Maven repository (Maven Central, an internal Nexus, …),
verify its SHA-256 in flight, and persist it to the local cache
before streaming the response. Subsequent requests for the same
digest are local hits.

### Trigger

The fallback path runs **only** when **all three** of these hold:

1. The request method is `GET`. `HEAD` and `PUT` never trigger an
   upstream fetch.
2. The configured upstream is enabled (`fetch_missing = true` plus a
   non-empty repo list — see "Configuration" below).
3. The request includes an `X-Barista-Coords` header whose value
   parses as Maven coordinates:

   - `g:a:v` — 3 components; packaging defaults to `jar`.
   - `g:a:t:v` — 4 components; explicit packaging type.
   - `g:a:t:c:v` — 5 components; explicit type + classifier.

   Each segment must match the Maven character class
   `[A-Za-z0-9._-]+`. The hint is required because there is no
   reverse mapping from a SHA-256 to a Maven layout path; the digest
   alone is not enough to know what to fetch.

A request that satisfies (1) + (2) but not (3) is a plain 404. A
malformed coords header surfaces `400 BAR-CACHE-008`.

### Repository fallthrough

Repositories are tried sequentially in the order given. The first
repository that returns a 2xx with bytes hashing to the requested
digest wins. Failure modes that fall through to the next repository:

- non-2xx response (404 is by far the most common case),
- network / TLS / timeout error,
- digest mismatch — the bytes hashed to a different value than the
  requested digest. The discarded bytes never reach the local store
  (the in-flight verifier in `Cas::put` discards them).

If every configured repository fails, the response is a plain 404.

### Wire-level integrity

Bytes from the upstream stream straight through
`tokio_util::io::StreamReader` into `Cas::put`, which hashes them as
they go. There is no intermediate buffer in memory or on disk that
ever contains unverified bytes — a digest mismatch is detected before
the put commits, and the staging file is dropped. An upstream serving
poisoned content can never poison the local cache.

### Configuration

| Variable                          | Default | Notes                                                                                  |
|-----------------------------------|---------|----------------------------------------------------------------------------------------|
| `ROASTERY_UPSTREAM_FETCH_MISSING` | `false` | Master switch. Accepts `true`/`false`/`1`/`0`/`yes`/`no`/`on`/`off` (case-insensitive). |
| `ROASTERY_UPSTREAM_REPOS`         | unset   | Comma-separated base URLs. Order is preserved; first hit wins.                          |
| `ROASTERY_UPSTREAM_TIMEOUT_SECS`  | `30`    | Connect + overall request timeout per upstream attempt.                                 |

`fetch_missing = true` with an empty repo list is a startup error
(`BAR-CACHE-007`) — enabling the feature without configuring an
upstream is almost certainly a misconfiguration.

Example:

```bash
ROASTERY_UPSTREAM_FETCH_MISSING=true \
ROASTERY_UPSTREAM_REPOS="https://repo.maven.apache.org/maven2/,https://nexus.example.com/repository/public/" \
ROASTERY_UPSTREAM_TIMEOUT_SECS=45 \
    cargo run -p roastery
```

```bash
curl -i \
    -H 'X-Barista-Coords: org.slf4j:slf4j-api:2.0.13' \
    "http://127.0.0.1:7878/v1/cas/sha256/$(echo -n 'slf4j-api-2.0.13.jar' | sha256sum | cut -d' ' -f1)"
```

### Metrics

The upstream-on-miss path emits two Prometheus series alongside the
existing CAS metrics:

- `roastery_upstream_fetch_total{repo, result}` — counter. `repo` is
  the bare host of the upstream URL (e.g.
  `repo.maven.apache.org`); cardinality is bounded by the operator-
  configured repo list. `result` ∈ {`hit`, `miss`, `error`,
  `digest_mismatch`}. The `digest_mismatch` label is the canary for
  an upstream serving stale or compromised content — alert on a
  non-zero rate.
- `roastery_upstream_fetch_duration_seconds_bucket{repo, le=…}`
  plus the standard `_sum` / `_count` — histogram of per-attempt
  latency, labelled by upstream host. Buckets cover the warm-miss
  range (~100 ms) through worst-case slow upstream (~60 s).

### Concurrency note

Concurrent requests for the same missing digest deduplicate **via the
local store**, not via in-process coordination: the first request to
finish its `Cas::put` makes the blob present, and any later request
hits the local fast path on its next `cas.stat` call. This is
deliberately simple — the cost is at most one duplicate upstream
fetch per concurrent miss, which is cheap relative to the wins from
not needing a per-digest lock table in the server.

## Operations endpoints

Alongside the `/v1/…` protocol surface, the server exposes a small set
of operational endpoints at the root path. These follow the
Kubernetes/SRE conventions a load-balancer health check or a
`Prometheus` scrape config expects — they are **not** part of the
barista protocol and are versioned independently:

| Method | Path        | Body                                  | Content-Type                                  |
|--------|-------------|---------------------------------------|-----------------------------------------------|
| `GET`  | `/healthz`  | `ok\n` (plain text)                   | `text/plain; charset=utf-8`                   |
| `GET`  | `/metrics`  | Prometheus text exposition (v0.0.4)   | `text/plain; version=0.0.4; charset=utf-8`    |
| `GET`  | `/version`  | JSON build identity (see below)       | `application/json`                            |

`/healthz` is intentionally distinct from the barista-protocol
`/v1/health` endpoint documented in **HTTP API** above: `/healthz`
answers "is this pod alive?" for kubelet and returns a plain-text 200
unless the process is so broken it can't respond, while `/v1/health`
answers "does the barista protocol stack work?" for clients and
returns JSON describing the protocol version. Both coexist.

### `/version` JSON shape

```json
{
  "name": "roastery",
  "version": "0.1.0-alpha.0",
  "git_sha": "abc123def456",
  "build_date": "2026-05-19T12:34:56Z",
  "rustc": "rustc 1.84.0 (9fc6b4312 2024-12-30)"
}
```

`git_sha`, `build_date`, and `rustc` may each be `null` if the build
machine couldn't determine the value at compile time (e.g. a clean
tarball install with no git on `PATH`). The `name` and `version`
fields are always non-null.

### `/metrics` inventory

The v0.1 metric set is intentionally small. Each entry includes the
Prometheus type and the labels it carries:

- `roastery_build_info{version, rustc}` — info-style gauge, value
  always `1`; the build identity is in the labels.
- `roastery_uptime_seconds` — gauge; seconds since the registry was
  initialised (≈ process start).
- `roastery_cas_requests_total{method, result}` — counter per CAS
  handler outcome. `method` ∈ `{get, head, put}`,
  `result` ∈ `{hit, miss, error}`.
- `roastery_cas_request_duration_seconds_bucket{method, le=…}`
  plus `_sum` / `_count` — histogram of CAS handler latency
  (default buckets `0.001 … 5.0 s`).
- `roastery_storage_bytes_total{backend}` — gauge; total bytes
  resident in the configured CAS backend (`filesystem`, `s3`,
  `gcs`). For the filesystem backend the value is computed by
  walking `<root>/cas/` and is cached for ~5 s.

### Example Prometheus scrape config

Drop into a `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: roastery
    scrape_interval: 5s
    metrics_path: /metrics
    static_configs:
      - targets: ['roastery.svc.cluster.local:7878']
```

A 5 s scrape interval is comfortable: the storage-bytes gauge is
cached at the same TTL, so a tight loop won't make `/metrics` walk
the CAS tree on every poll.

## Authentication

The server supports two authentication mechanisms — bearer tokens
and mTLS — and either, both, or neither can be configured. When
**both** are configured, **either** mechanism suffices on a
per-request basis (bearer is checked first; mTLS is the
fall-through).

### Public vs protected routes

The auth layer wraps only the CAS endpoints. The following routes
are **always public**, regardless of which auth mechanisms are
configured:

- `/healthz` — k8s liveness probe.
- `/metrics` — Prometheus scrape target. Operators that need to
  restrict access should do it at the network layer (NetworkPolicy,
  firewall, sidecar).
- `/version` — build identity. Used by deploy tooling and human
  operators; no sensitive data.
- `/v1/health` — barista-protocol liveness. Clients use this before
  authenticating to confirm the protocol stack is up.
- `/v1/capabilities` — version negotiation. Clients consult this
  before they know what credentials the server expects.

These five routes accept anonymous requests on every deployment.
The CAS endpoints (`/v1/cas/sha256/{digest}` for GET / HEAD / PUT
and `/v1/cas/missing`) require valid credentials when any auth
mechanism is configured.

### Bearer tokens

Set `ROASTERY_BEARER_TOKENS_FILE=/etc/roastery/tokens.txt`. The
file format is UTF-8, one entry per line:

```text
# comments start with '#'
ci-runner-1:s3cret-token-value
ci-runner-2:another-secret
```

Each entry is `<label>:<secret>`. The label is a short non-secret
identifier (it shows up in logs); the secret is what clients send
in the `Authorization` header:

```bash
curl -i https://roastery.example.com/v1/cas/sha256/$DIGEST \
    -H 'Authorization: Bearer s3cret-token-value'
```

Lines without a `:` are also accepted: the entire line is the
secret, and a short SHA-256 prefix stands in for the label.

Tokens are loaded once at startup, hashed with SHA-256, and
compared in constant time. Plaintext bytes never leave the loader;
the in-memory state stores only the hashes. v0.2 will add a
`SIGHUP`-driven reload.

### mTLS

Set `ROASTERY_MTLS_CA_CERT=/etc/roastery/ca.pem` to a PEM bundle of
one or more trust anchors. The server then requires every client
to present a certificate chained to one of those CAs during the
TLS handshake. Clients that don't present a cert, or present one
signed by a different CA, are rejected at the TLS layer — the HTTP
request never starts.

mTLS requires server-side TLS — set `ROASTERY_TLS_CERT` and
`ROASTERY_TLS_KEY` alongside `ROASTERY_MTLS_CA_CERT`. The server
refuses to start if mTLS is configured without TLS termination.

Successful handshakes attach a `Principal::Mtls { subject }` to
the request. The subject is the leaf cert's first URI SAN
(preferred — SPIFFE IDs land here) or the Subject Common Name, in
that order. v0.1 handlers don't read the subject; v0.2 RBAC will
key per-route ACLs off it.

### Fail-closed default

A roastery bound to a **non-loopback** address (anything other
than `127.0.0.1` / `::1` / `localhost`) with **neither** bearer
**nor** mTLS configured refuses to start. The error is
`BAR-AUTH-005`:

```text
roastery: invalid server configuration: BAR-AUTH-005:
  non-loopback bind 0.0.0.0:7878 requires auth configuration
  (set ROASTERY_BEARER_TOKENS_FILE and/or ROASTERY_MTLS_CA_CERT)
```

Loopback binds without auth are explicitly allowed so the
`cargo run -p roastery` dev workflow stays one-command.

## Container image

Roastery ships as an OCI container image published to GitHub
Container Registry under the canonical Barista organisation:

```
ghcr.io/buildwithbarista/roastery:<tag>
```

### Tags

| Tag             | Source                                  | Stability                                                                  |
|-----------------|-----------------------------------------|----------------------------------------------------------------------------|
| `:edge`         | every commit pushed to `main`           | tracks `HEAD`; rebuilt on every merge                                      |
| `:0.1.0`, `:0.2.0`, … | tagged semver releases            | immutable; once published the digest never changes                         |
| `:0.1`, `:0.2`, …     | major-minor pin                   | rolls forward with each patch release in the series                        |
| `:latest`       | most recent stable release              | **only stable releases** (no `-alpha`, `-beta`, `-rc` suffix) get `:latest`|

### Running

```bash
docker run --rm -p 7878:7878 \
    -v $(pwd)/roastery-data:/var/lib/roastery \
    -e ROASTERY_BIND=0.0.0.0:7878 \
    -e ROASTERY_STORAGE_DIR=/var/lib/roastery \
    -e ROASTERY_BEARER_TOKENS_FILE=/etc/roastery/tokens \
    -v $(pwd)/tokens:/etc/roastery/tokens:ro \
    ghcr.io/buildwithbarista/roastery:edge
```

The container `WORKDIR` is `/var/lib/roastery` (the default value of
`ROASTERY_STORAGE_DIR` in the config table below); the image
`EXPOSE`s port `7878`. Any non-loopback bind requires at least one
authentication mechanism — see the **Authentication** section above
for token-file and mTLS configuration.

### Image properties

- **Base:** `gcr.io/distroless/cc-debian12:nonroot`. Distroless ships
  glibc, ca-certificates, and the dynamic linker — nothing else. No
  shell, no package manager, no `curl`, no `cat`. The image runs as
  uid `65532` (the `nonroot` user); operator-side `securityContext`
  declarations should mirror that.
- **Size:** ~35 MB compressed for the runtime stage.
- **Platforms:** `linux/amd64` and `linux/arm64`.
- **Entrypoint:** `/usr/local/bin/roastery`. The binary accepts
  `--version` and `--help` as diagnostic flags; all server
  configuration is environment-variable driven (see the
  **Configuration** table below).

### What NOT to add

The image is intentionally minimal. If you need a shell to debug a
running container, base your local override on
`gcr.io/distroless/cc-debian12:debug-nonroot` — that variant bundles
busybox. Do not deploy the `debug-nonroot` variant to production:
the extra surface is exactly the surface distroless exists to remove.

Equally: do not add additional system packages, do not add a
`HEALTHCHECK` directive (Kubernetes probes the HTTP `/healthz`
endpoint directly via `livenessProbe`/`readinessProbe`; the
distroless image has no shell to satisfy a `CMD`-form
`HEALTHCHECK`), and do not switch the runtime user back to `root`.

### Building locally

```bash
roastery/scripts/build-image.sh
```

Overrides via env: `TAG` (default `roastery:dev`), `PLATFORM`
(default `linux/amd64`). The script wraps a single
`docker buildx build --load …` invocation so the resulting image is
available to the local Docker daemon immediately.

For a full container smoke-test (build + boot + `/healthz` +
`/version` probe + graceful shutdown), run
`scripts/test-dockerfile.sh` from the workspace root.

## Deployment

A Helm chart for Kubernetes lives at `deploy/helm/roastery/`. It
wraps the container image with all of the on-cluster pieces a
production deploy needs (Deployment / Service / PVC / Secrets /
optional Ingress / optional NetworkPolicy / optional Prometheus
Operator `ServiceMonitor`) and surfaces the full env-var
configuration table above as typed `values.yaml` keys.

### Quickstart

The chart is in-tree only at v0.1 — it is not yet published to a Helm
repository. Install it directly from the source tree:

```bash
# Single-replica, filesystem-backed deploy with a 100 GiB PVC.
# Bind to loopback so the BAR-AUTH-005 fail-closed check doesn't
# fire when no auth is configured; flip to 0.0.0.0:7878 once you've
# enabled bearer-token or mTLS auth.
helm install roastery deploy/helm/roastery \
    --namespace roastery --create-namespace \
    --set server.bind=127.0.0.1:7878
```

The bundled Helm test hook runs a short-lived pod that probes
`/healthz`, `/version`, and `/metrics` against the in-cluster
Service:

```bash
helm test roastery --namespace roastery
```

### Configuration

Every roastery env-var documented in the **Configuration** table
above maps to a typed value in
`deploy/helm/roastery/values.yaml`. The file is heavily commented;
read it top-to-bottom for the full surface and the rationale behind
each default.

Common knobs:

- `replicaCount` — pod count. With `storage.backend=filesystem`
  the chart enforces `replicaCount: 1` (the PVC is
  `ReadWriteOnce`); for multi-replica deploys use
  `storage.backend=s3` (or `gcs`) and set
  `persistence.enabled=false`.
- `image.tag` — overrides the chart-default of `:edge`. Pin to a
  released semver tag (e.g. `0.1.0`) when one ships.
- `storage.backend` — `filesystem` (default, PVC), `s3`, or `gcs`.
  Bucket / region / project keys live under `storage.s3` and
  `storage.gcs`.
- `tls.enabled` + `tls.create=true` / `tls.existingSecret` — server-
  side TLS. Use `--set-file tls.certPem=cert.pem
  --set-file tls.keyPem=key.pem` to load real PEM material without
  embedding it in `values.yaml`.
- `auth.bearer.enabled` and `auth.mtls.enabled` — independent;
  enable either, both, or neither. mTLS requires `tls.enabled=true`.
- `upstream.fetchMissing` + `upstream.repos` — the upstream-on-miss
  feature described above.
- `metrics.serviceMonitor.enabled` — opt-in Prometheus Operator
  `ServiceMonitor` CR. `/metrics` is always exposed by the binary
  regardless.

### Validation

The chart ships a JSON Schema (`values.schema.json`) Helm consults
on every `helm install` / `helm upgrade`; bad backend names, bad
types, or missing required keys fail before a render is attempted.
Cross-field consistency (mTLS-without-TLS, multi-replica + PVC,
`upstream.fetchMissing` without `upstream.repos`) is enforced by a
chart-level `fail` validator in `_helpers.tpl` and surfaces with an
actionable error string at `helm template` time.

### CI

`.github/workflows/helm.yml` runs `helm lint`, schema validation,
and a golden-fixture diff against the rendered output. The
golden files under `deploy/helm/fixtures/*.golden.yaml` are the
chart's regression net — any template change that perturbs the
rendered manifest set surfaces as a reviewable diff. Regenerate
intentional changes with:

```bash
bash deploy/helm/scripts/helm-render-fixtures.sh update
```

### End-to-end testing (kind)

The static gates above prove the chart renders correctly. The
`tests/e2e/kind.sh` harness proves the **whole** stack — Dockerfile,
chart, binary — works against a real Kubernetes cluster.

```bash
bash roastery/tests/e2e/kind.sh
```

Pre-requisites on PATH: `docker`, `kind`, `helm`, `kubectl`, `curl`,
`shasum`. The script never invokes `sudo` and never touches a remote
cluster.

What it exercises, end-to-end:

- builds `roastery/Dockerfile` for `linux/amd64`,
- creates a single-node kind cluster (image digest pinned in
  `tests/e2e/kind-config.yaml`),
- loads the locally-built image and `helm install`s the chart with
  bearer-token auth enabled,
- runs the chart's own `helm test` hook (in-cluster probes against
  `/healthz`, `/version`, `/metrics`),
- port-forwards to the rendered Service, PUTs a known blob, GETs it
  back, asserts byte-equal,
- asserts the five always-public endpoints (`/healthz`, `/metrics`,
  `/version`, `/v1/health`, `/v1/capabilities`) all return `200`.

Env-var knobs:

- `CLUSTER_NAME` (default `roastery-e2e`) — reuse an existing kind
  cluster instead of creating a new one.
- `IMAGE_TAG` (default `e2e-<short-sha>`) — tag for the locally-built
  image.
- `RELEASE_NAME`, `NAMESPACE` — Helm release + namespace overrides.
- `TIMEOUT` (default `5m`) — applied to `kind --wait` and `helm
  --timeout`.
- `KEEP_CLUSTER=true` — skip the teardown on exit. Handy when
  iterating locally; `kubectl` against `kind-<cluster>` keeps working
  until you `kind delete cluster --name <cluster>`.
- `SKIP_BUILD=true` — assume `roastery:$IMAGE_TAG` already exists
  (rebuild iteration shortcut).

The same script runs on every PR that touches `roastery/**` via
`.github/workflows/e2e-kind.yml`, plus a Monday 06:00 UTC canary that
catches kindest-node and runner-image drift before it lands on a PR.

## Configuration

All configuration is environment-driven. Defaults are documented in
the table; `ServerConfig::from_env` applies them when the variable is
unset.

| Variable                       | Default            | Notes                                                                      |
|--------------------------------|--------------------|----------------------------------------------------------------------------|
| `ROASTERY_BIND`                | `127.0.0.1:7878`   | `host:port` for the TCP listener.                                          |
| `ROASTERY_STORAGE_DIR`         | `./.roastery-data` | Filesystem CAS root; created on startup if missing.                        |
| `ROASTERY_STORAGE_BACKEND`     | `fs`               | `fs` (default), `s3`, or `gcs`. See **Storage backend** below.             |
| `ROASTERY_STORAGE_BUCKET`      | _(unset)_          | Required when backend is `s3` or `gcs`.                                    |
| `ROASTERY_STORAGE_REGION`      | _(unset)_          | Required when backend is `s3`.                                             |
| `ROASTERY_STORAGE_PROJECT`     | _(unset)_          | Required when backend is `gcs`.                                            |
| `ROASTERY_TLS_CERT`            | _(unset)_          | PEM cert chain. Must be set together with `ROASTERY_TLS_KEY`.              |
| `ROASTERY_TLS_KEY`             | _(unset)_          | PEM private key.                                                           |
| `ROASTERY_BEARER_TOKENS_FILE`  | _(unset)_          | Bearer token file (`<label>:<secret>` per line). See **Authentication**.   |
| `ROASTERY_MTLS_CA_CERT`        | _(unset)_          | PEM CA bundle for mTLS client-cert verification. Requires TLS to be on.    |
| `ROASTERY_UPSTREAM`            | _(unset)_          | Upstream registry consulted on cache miss; reserved for a later milestone. |
| `RUST_LOG`                     | `info`             | Standard `tracing_subscriber::EnvFilter` syntax.                           |

The upstream-on-miss field is accepted today but **not exercised**
— it exists so a subsequent task can plug in without churning the
public config surface.

## Module layout

```
roastery/
├── Cargo.toml
├── README.md             ← you are here
├── src/
│   ├── main.rs           binary entrypoint; tracing init + runtime
│   ├── lib.rs            re-exports the public API
│   ├── config.rs         ServerConfig + env-var loader
│   ├── server.rs         Router assembly, AppState, shutdown loop
│   ├── error.rs          RoasteryError + StorageError + ErrorBody
│   ├── proto/            wire-protocol handlers
│   │   ├── mod.rs        module root
│   │   ├── barista.rs    barista-protocol REST/JSON (/v1/…)
│   │   ├── reapi.rs      Bazel REAPI gRPC handler (CAS + ByteStream + Capabilities)
│   │   └── reapi/        REAPI submodules
│   │       ├── auth.rs       gRPC bearer-auth interceptor for the CAS data services
│   │       └── resource.rs   ByteStream resource-name grammar parser
│   ├── ops/              operational endpoints (/healthz, /metrics, /version)
│   │   ├── mod.rs        module root + sub-router
│   │   ├── health.rs     /healthz handler
│   │   ├── metrics.rs    /metrics handler + Prometheus registry + CAS instrumentation
│   │   └── version.rs    /version handler (reads build.rs constants)
│   ├── auth/             bearer + mTLS authentication
│   │   ├── mod.rs        module root; Principal enum
│   │   ├── bearer.rs     BearerVerifier — loads tokens file, hashes, constant-time compare
│   │   ├── mtls.rs       MtlsVerifier — wraps WebPkiClientVerifier, subject extraction
│   │   └── layer.rs      AuthLayer — tower middleware that enforces auth on the protected sub-router
│   └── storage/          content-addressed storage
│       ├── mod.rs        Digest, Stat, Cas trait
│       ├── fs.rs         filesystem-backed Cas (production default)
│       ├── s3.rs         S3 stub (v0.2)
│       └── gcs.rs        GCS stub (v0.2)
├── build.rs              build-time identity probe + REAPI proto codegen (tonic-prost-build)
├── proto/                vendored REAPI + googleapis .proto files (see proto/REVISIONS.txt for pins)
└── tests/
    ├── smoke.rs          scaffold smoke tests
    ├── proto_barista.rs  barista-protocol HTTP integration tests
    ├── reapi.rs          REAPI gRPC integration tests (tonic-client round-trip + cross-protocol sharing)
    ├── ops.rs            /healthz, /metrics, /version integration tests
    ├── auth.rs           bearer + mTLS integration tests
    └── common/           shared test helpers (e.g. ephemeral cert generation)
```

## Storage backend

Every blob in the roastery is identified by the SHA-256 digest of its
bytes, rendered as 64 lowercase hex characters. The digest is the
cache key — there is no separate metadata index. This matches the
content-addressing model the REAPI gRPC handler negotiates by default
(SHA-256) and the URL scheme the barista-protocol handler commits to.

The `Cas` trait (`roastery::storage::Cas`) is the storage surface every
protocol handler talks to:

- `stat(digest)` — size + digest, or `None` if absent.
- `get(digest)` — streaming reader, or `None` if absent.
- `put(expected_digest, source)` — streams `source` into the store,
  verifies its hash matches `expected_digest`, returns the resulting
  `Stat`. Atomic: a concurrent `get` either sees the complete blob or
  `None`.
- `delete(digest)` — idempotent; returns `true` if the blob existed.
- `list(prefix)` — iterates known digests, optionally filtered by hex
  prefix. Intended for tests + admin tooling, capped at 10 000
  entries per call in v0.1 (pagination is scheduled for v0.2).

### Filesystem layout

The default `fs` backend lays blobs out under
`<ROASTERY_STORAGE_DIR>/cas/<2-hex>/<62-hex>`, with in-flight writes
staged in `<ROASTERY_STORAGE_DIR>/tmp/<random>.tmp` and atomically
renamed into place. The 2-character prefix directory keeps any single
directory under ~65 000 entries even for a fully populated 16-bit
fanout — comfortable territory for ext4, APFS, NTFS, and ZFS dirent
listings, and the same convention git's loose-object store and
bazel-remote use.

### v0.1 limitations

- **S3 and GCS are stubs.** The types exist so config files can name
  them and the trait surface can be exercised in tests; every method
  returns `StorageError::NotImplemented`. Real backends arrive in
  v0.2.
- **`list` is capped at 10 000 entries per call.** v0.2 will replace
  this with a paginated cursor API once GC and admin endpoints need
  it.
- **No GC or eviction yet.** The store grows monotonically. Operators
  who need eviction in v0.1 should run a cron job that calls `delete`
  from outside the server.

### HTTP/1.1 + HTTP/2

The connection acceptor uses `hyper-util`'s `server::conn::auto`
builder, which negotiates HTTP/1.1 or HTTP/2 per connection. Over
plain TCP this stays HTTP/1.1 in practice; HTTP/2 negotiation kicks
in once TLS + ALPN are added (clients won't speak `h2c` by default).
The codepath is reserved here so adding TLS is a layering change,
not a rewrite.

## Extending the scaffold

The server reserves slots for the work that follows. Search the
source for `// T<N>:` comments to find the exact extension points:

- **T2 — storage**: the content-addressed object store described in
  the **Storage backend** section above. The `Cas` backend is
  instantiated from `ServerConfig::storage` and carried on
  `server::AppState`; storage HTTP routes (`/cas/:hash`, `/ac/:hash`,
  …) are not mounted yet — that belongs to the protocol handlers.
- **T3 — barista-protocol**: the small REST/JSON handler Barista
  clients speak. Mounted under `/v1/` — see the **HTTP API** section
  above for the full endpoint and error-code reference.
- **T4 — REAPI gRPC**: the `bazel-remote`-compatible gRPC surface,
  served via `tonic` and merged into the axum router with
  `Router::merge` (both stacks share `hyper` + `tower`).
- **T5 — auth**: a `tower::Layer` wrapping the protected sub-
  router, plus switching the connection builder to `rustls` when
  `ServerConfig::tls` is `Some`. See **Authentication** above for
  the full surface.
- **T6 — upstream-on-miss**: a fallback `Layer` that consults
  `ServerConfig::upstream` when storage returns 404.

## Testing

```bash
cargo test -p roastery
```

The integration tests in `tests/smoke.rs` spawn the server on an
ephemeral port and issue a real `reqwest` call against `GET /`. The
tests in `tests/proto_barista.rs` exercise the full barista-protocol
surface end-to-end against a live server instance backed by a
`TempDir` filesystem CAS. The tests in `tests/ops.rs` cover the
`/healthz`, `/metrics`, and `/version` ops endpoints — including a
PUT + GET round-trip that asserts the Prometheus counter actually
increments. `tests/auth.rs` exercises bearer + mTLS end-to-end:
the bearer side drives a plain-HTTP listener with a tokens file;
the mTLS side mints an ephemeral CA + server + client cert with
`rcgen` at test time and drives a real TLS handshake. None of
these sets require any environment setup.

## License

Dual-licensed under MIT OR Apache-2.0, same as the rest of the
Barista project.
