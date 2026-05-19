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
barista-protocol HTTP handler, and the ops endpoints (`/healthz`,
`/metrics`, `/version`). Auth, the REAPI gRPC handler, and
upstream-on-miss are wired in by subsequent milestones — see
**Extending the scaffold** below.

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

## Configuration

All configuration is environment-driven. Defaults are documented in
the table; `ServerConfig::from_env` applies them when the variable is
unset.

| Variable                   | Default            | Notes                                                                      |
|----------------------------|--------------------|----------------------------------------------------------------------------|
| `ROASTERY_BIND`            | `127.0.0.1:7878`   | `host:port` for the TCP listener.                                          |
| `ROASTERY_STORAGE_DIR`     | `./.roastery-data` | Filesystem CAS root; created on startup if missing.                        |
| `ROASTERY_STORAGE_BACKEND` | `fs`               | `fs` (default), `s3`, or `gcs`. See **Storage backend** below.             |
| `ROASTERY_STORAGE_BUCKET`  | _(unset)_          | Required when backend is `s3` or `gcs`.                                    |
| `ROASTERY_STORAGE_REGION`  | _(unset)_          | Required when backend is `s3`.                                             |
| `ROASTERY_STORAGE_PROJECT` | _(unset)_          | Required when backend is `gcs`.                                            |
| `ROASTERY_TLS_CERT`        | _(unset)_          | PEM cert chain. Must be set together with `ROASTERY_TLS_KEY`.              |
| `ROASTERY_TLS_KEY`         | _(unset)_          | PEM private key.                                                           |
| `ROASTERY_UPSTREAM`        | _(unset)_          | Upstream registry consulted on cache miss; reserved for a later milestone. |
| `RUST_LOG`                 | `info`             | Standard `tracing_subscriber::EnvFilter` syntax.                           |

The TLS and upstream-on-miss fields are accepted today but **not
exercised** — they exist so subsequent tasks can plug in without
churning the public config surface.

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
│   │   └── reapi.rs      REAPI gRPC placeholder (filled in later)
│   ├── ops/              operational endpoints (/healthz, /metrics, /version)
│   │   ├── mod.rs        module root + sub-router
│   │   ├── health.rs     /healthz handler
│   │   ├── metrics.rs    /metrics handler + Prometheus registry + CAS instrumentation
│   │   └── version.rs    /version handler (reads build.rs constants)
│   └── storage/          content-addressed storage
│       ├── mod.rs        Digest, Stat, Cas trait
│       ├── fs.rs         filesystem-backed Cas (production default)
│       ├── s3.rs         S3 stub (v0.2)
│       └── gcs.rs        GCS stub (v0.2)
├── build.rs              build-time identity probe (git sha, rustc, build date)
└── tests/
    ├── smoke.rs          scaffold smoke tests
    ├── proto_barista.rs  barista-protocol HTTP integration tests
    └── ops.rs            /healthz, /metrics, /version integration tests
```

## Storage backend

Every blob in the roastery is identified by the SHA-256 digest of its
bytes, rendered as 64 lowercase hex characters. The digest is the
cache key — there is no separate metadata index. This matches the
content-addressing model the REAPI gRPC handler (a later milestone)
negotiates by default and the URL scheme the barista-protocol handler
commits to.

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
- **T5 — auth**: a `tower::Layer` wrapping the router, plus
  switching the connection builder to `rustls` when
  `ServerConfig::tls` is `Some`.
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
increments. None of these sets require any environment setup.

## License

Dual-licensed under MIT OR Apache-2.0, same as the rest of the
Barista project.
