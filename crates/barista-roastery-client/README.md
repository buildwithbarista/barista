# barista-roastery-client

Async Rust client for the **roastery** cache server's
barista-protocol HTTP/2 surface.

The roastery is Barista's remote artifact cache: a content-addressed
store accessed over a small, fixed REST/JSON contract. This crate is
the client side of that contract. It exposes one cohesive
[`RoasteryClient`] with one method per endpoint, plus the
configuration types needed to wire up authentication and TLS.

The authoritative wire-protocol specification lives in the
[`roastery`](../../roastery) crate (the server's
`proto::barista` module + its README). The client speaks exactly
what that surface advertises.

## Status

Part of Barista v0.1. Provides the barista-native HTTP/2 client only —
the REAPI gRPC surface (when it lands) ships as a separate client.

## Endpoints

| Method on `RoasteryClient`   | Endpoint                              | Auth required when configured? |
|------------------------------|---------------------------------------|--------------------------------|
| `get_blob(digest)`           | `GET  /v1/cas/sha256/{digest}`        | Yes                            |
| `stat_blob(digest)`          | `HEAD /v1/cas/sha256/{digest}`        | Yes                            |
| `put_blob(digest, body, n)`  | `PUT  /v1/cas/sha256/{digest}`        | Yes                            |
| `missing(&[Digest])`         | `POST /v1/cas/missing`                | Yes                            |
| `health()`                   | `GET  /v1/health`                     | No (always anonymous)          |
| `capabilities()`             | `GET  /v1/capabilities`               | No (always anonymous)          |

`stat_blob` returns `Ok(None)` for an absent blob; `get_blob` surfaces
the same absence as `ClientError::NotFound`. `missing` batches its
input to honour the server's `cas.max_batch_missing` cap (default
1000).

## Authentication

Pick the mechanism via `AuthConfig`:

- `AuthConfig::Anonymous` — send no credentials. Works against an
  unsecured server and the always-public health / capabilities
  endpoints; fails 401 against protected CAS routes when the server
  requires auth.
- `AuthConfig::Bearer { token }` — send
  `Authorization: Bearer <token>` on every protected request. The
  server compares the token against its SHA-256-hashed token list.
- `AuthConfig::Mtls { client_cert_pem, client_key_pem }` — present a
  client certificate during the TLS handshake. The server validates
  the chain against its configured CA bundle.

The client never sends the bearer header to the always-public
`/v1/health` and `/v1/capabilities` endpoints, even when one is
configured. This avoids leaking a token to a route that doesn't need
it.

## TLS

`TlsConfig` controls server-cert verification (and threads the mTLS
client identity from `AuthConfig::Mtls` through the same rustls
`ClientConfig`):

- `TlsConfig::SystemRoots` — load the platform's native trust store
  via `rustls-native-certs`. Production default for CA-issued
  certificates.
- `TlsConfig::CustomCa { ca_cert_pem }` — verify against a
  caller-supplied PEM CA bundle. Useful for self-signed or private-CA
  roastery deployments.
- `TlsConfig::PlainHttp` — no TLS. **Refused at construction time** if
  the base URL is `https://`. Intended for development and integration
  tests against a loopback server.

The client uses rustls 0.23 with the `ring` provider, matching the
server's crypto stack. HTTP/2 is negotiated via ALPN over TLS; over
plain HTTP the connection stays HTTP/1.1, which the protocol supports
identically.

## Examples

### Minimal: anonymous, plain HTTP

```rust
use barista_roastery_client::{ClientConfig, Digest, RoasteryClient, TlsConfig};

# async fn _ex() -> Result<(), Box<dyn std::error::Error>> {
let base = "http://127.0.0.1:8080".parse()?;
let cfg = ClientConfig::builder(base)
    .tls(TlsConfig::PlainHttp)
    .build();
let client = RoasteryClient::new(cfg)?;

let health = client.health().await?;
assert_eq!(health.status, "ok");
# Ok(())
# }
```

### GET a blob with bearer auth over TLS

```rust
use std::time::Duration;
use barista_roastery_client::{
    AuthConfig, ClientConfig, Digest, RoasteryClient, TlsConfig,
};
use tokio::io::AsyncReadExt;

# async fn _ex() -> Result<(), Box<dyn std::error::Error>> {
let base = "https://roastery.example.com:8443".parse()?;
let cfg = ClientConfig::builder(base)
    .auth(AuthConfig::Bearer { token: std::env::var("ROASTERY_TOKEN")? })
    .tls(TlsConfig::SystemRoots)
    .timeout(Duration::from_secs(10))
    .build();
let client = RoasteryClient::new(cfg)?;

let digest = Digest::from_hex(
    "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
)?;
let mut blob = client.get_blob(digest).await?;
let mut bytes = Vec::with_capacity(blob.stat.size as usize);
blob.body.read_to_end(&mut bytes).await?;
# Ok(())
# }
```

### Stream a PUT with mTLS

```rust
use barista_roastery_client::{
    AuthConfig, ClientConfig, Digest, RoasteryClient, TlsConfig,
};
use tokio::fs::File;

# async fn _ex() -> Result<(), Box<dyn std::error::Error>> {
let base = "https://roastery.example.com:8443".parse()?;
let cfg = ClientConfig::builder(base)
    .tls(TlsConfig::CustomCa {
        ca_cert_pem: std::fs::read("ca.pem")?,
    })
    .auth(AuthConfig::Mtls {
        client_cert_pem: std::fs::read("client.crt")?,
        client_key_pem: std::fs::read("client.key")?,
    })
    .build();
let client = RoasteryClient::new(cfg)?;

let file = File::open("artifact.jar").await?;
let metadata = file.metadata().await?;
let bytes = tokio::fs::read("artifact.jar").await?;
let digest = Digest::of_bytes(&bytes);

client.put_blob(digest, file, metadata.len()).await?;
# Ok(())
# }
```

## Streaming

`get_blob` returns a `BlobStream` carrying a boxed `AsyncRead` over
the response body. Bytes flow as the server emits them — the whole
blob is never buffered in memory client-side.

`put_blob` takes an `AsyncRead + 'static` source and a known total
size. The body is streamed via a `tokio_util::io::ReaderStream`
into the HTTP request, so uploads of large artifacts don't OOM the
client.

## Errors

`ClientError` partitions failures into pattern-matchable variants:

- `Config` — invalid construction (e.g. plain HTTP against
  `https://`). Surfaced from `RoasteryClient::new`, before I/O.
- `Network` / `Timeout` — transport failures. `Timeout` is split out
  so callers can apply retry/backoff policy on timeouts specifically.
- `Tls` — TLS handshake / certificate validation failed.
- `Auth` — server returned 401 with `BAR-AUTH-001`.
- `ServerRejected` — server returned a non-2xx with a structured
  `BAR-CAS-NNN` body (digest mismatch, malformed digest, batch cap
  exceeded, etc.); carries the status, code, message, and the
  optional `expected` / `actual` digests for `BAR-CAS-001`.
- `NotFound` — convenience variant for 404 on GET (HEAD maps 404 to
  `Ok(None)`).
- `BadResponse` — server responded but the body didn't match the
  documented wire shape.
- `InvalidDigest` — `Digest` parsing failed.

## Limitations

- **No retry / backoff.** Each method makes exactly one request. Wrap
  the client with retry logic where the use case requires it (e.g.
  exponential backoff on transient 5xx). Built-in retry is a v0.2
  enhancement.
- **No REAPI gRPC surface.** This client covers only the barista
  HTTP/2 protocol. A separate REAPI client ships when that wire
  protocol is wired up.
- **One connection pool per client.** A `RoasteryClient` reuses
  `reqwest::Client`'s pool across all calls. Clone the client (cheap
  — internally `Arc`-shared) to hand it to multiple tasks; construct
  a fresh client per *server* to avoid mixing trust configurations.

## Testing

The crate ships three integration suites under `tests/`:

- **`round_trip.rs`** — the core suite. Each test spins a roastery
  server **in-process** on an ephemeral port and drives one endpoint or
  auth/TLS path. Runs by default under `cargo test`.
- **`coords_header.rs`** — pins that `get_blob_with_coords` puts the
  `X-Barista-Coords` header on the wire. Runs by default.
- **`container_roundtrip.rs`** — exercises the client against a **real
  ephemeral roastery Docker container** built from `roastery/Dockerfile`,
  over a real published TCP socket. This is `#[ignore]`d by default
  **and** gated on the `BARISTA_ROASTERY_CONTAINER_TEST` env var, so a
  plain `cargo test` / `cargo test --workspace` stays green on a host
  without Docker. To run it where Docker is available:

  ```sh
  BARISTA_ROASTERY_CONTAINER_TEST=1 \
    cargo test -p barista-roastery-client \
    --test container_roundtrip -- --ignored --nocapture
  ```

  It builds the image itself (via `roastery/scripts/build-image.sh`,
  tag `roastery:test`) unless `SKIP_BUILD=1` is set, in which case it
  assumes a prebuilt image (`ROASTERY_TEST_IMAGE`, default
  `roastery:test`) already exists. The container is torn down with
  `docker rm -f` from a `Drop` guard, so a panic still cleans up.

- **`roastery_speedup.rs`** — a **mechanism demonstration** of the
  cold-cache speedup a roastery delivers. It drives a batch of synthetic
  blobs down two paths and measures wall-clock time: cold cache → a
  latency-injected mock "Central" (a `tokio::time::sleep`-delayed mock
  upstream, modelling WAN RTT) versus cold cache → a warm local
  in-process roastery. It asserts the roastery path is ≥5× faster and
  logs the measured ratio. Runs by default (no Docker needed — all
  in-process + mock).

### What the speedup test proves — and what it does not

`roastery_speedup.rs` proves the **mechanism**: under a controlled,
simulated upstream latency (150 ms per request), a warm local roastery
beats a far-away upstream by well over 5× — because the speedup is a
latency-asymmetry effect (LAN/local roastery RTT vs WAN upstream RTT),
multiplied across every artifact in the closure.

It is **not** the milestone-level measurement *"cold cache + warm
roastery beats cold cache + Central direct by ≥5× on the 100-project
corpus median"*. That measurement is a property of a specific corpus on
specific reference hardware against the real network; it requires the
full project corpus to be materialised and the reference-hardware
benchmark harness to be provisioned, and it is owned by the benchmark
workstream. Nothing in this crate should be read as a claim that the
corpus-median target has been met — only that the client and protocol
deliver the targeted speedup whenever the latency asymmetry the
milestone assumes is present.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
