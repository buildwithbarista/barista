// SPDX-License-Identifier: MIT OR Apache-2.0

//! Barista-protocol HTTP/2 handler — the REST/JSON surface Barista
//! clients use to talk to the roastery.
//!
//! All routes live under `/v1/` and share a single `axum::Router` built
//! by [`router`]. The surface intentionally keeps to a small, fixed
//! contract — the goal is a transport every Barista release can speak
//! without negotiation, not a generic key-value API.
//!
//! ## Endpoints
//!
//! | Method | Path                              | Purpose                                  |
//! |--------|-----------------------------------|------------------------------------------|
//! | `GET`  | `/v1/cas/sha256/{digest}`         | Fetch a blob by SHA-256.                 |
//! | `HEAD` | `/v1/cas/sha256/{digest}`         | Existence check (same headers as `GET`). |
//! | `PUT`  | `/v1/cas/sha256/{digest}`         | Upload + verify a blob.                  |
//! | `POST` | `/v1/cas/missing`                 | Batch presence check.                    |
//! | `GET`  | `/v1/health`                      | Barista-protocol liveness.               |
//! | `GET`  | `/v1/capabilities`                | Server feature flags + storage backend.  |
//!
//! `/v1/health` is the **protocol-level** health endpoint and is
//! intentionally distinct from the ops/SRE `/healthz` liveness probe a
//! follow-up task introduces. Both will coexist: `/v1/health` answers
//! "the barista protocol is up"; `/healthz` answers "the process is
//! alive enough for kubelet to leave it running."
//!
//! ## Errors
//!
//! Every non-2xx response carries a JSON [`crate::error::ErrorBody`]
//! with a stable `BAR-CAS-NNN` code. See the table on `ErrorBody` for
//! the full code reference.
//!
//! ## Streaming
//!
//! `GET` returns a streaming body built from the `AsyncRead` the CAS
//! backend hands back, so blobs are never fully buffered in memory.
//! `PUT` reads its body as a stream too (axum/hyper enforces the
//! advertised `Content-Length`), and the underlying `Cas::put`
//! hashes-and-verifies as bytes flow through.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::Json;
use axum::Router;
use axum::body::{Body, BodyDataStream};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use futures_util::{Stream, TryStream};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::io::{ReaderStream, StreamReader};

use crate::config::StorageBackend;
use crate::error::{ErrorBody, StorageError};
use crate::ops::metrics::{CasMethod, CasResult, record_cas_request};
use crate::server::AppState;
use crate::storage::{CasReader, Digest};
use crate::upstream::{Coords, UpstreamError};

/// Header carrying the canonical `sha256:<hex>` identifier of the blob
/// involved in the request — set on every CAS response (200 OK, 201
/// Created, HEAD).
const HDR_BARISTA_DIGEST: HeaderName = HeaderName::from_static("x-barista-digest");

/// Request header carrying the Maven coordinates of the artifact the
/// client expects this digest to identify. Format: `g:a[:t[:c]]:v`.
/// Used only by `GET /v1/cas/sha256/{digest}` on a local cache miss
/// when the upstream-on-miss path is enabled.
const HDR_BARISTA_COORDS: HeaderName = HeaderName::from_static("x-barista-coords");

/// Maximum number of digests accepted in a single `POST /v1/cas/missing`
/// request. Documented in the `capabilities` payload as
/// `cas.max_batch_missing`. Requests above this cap get a 413.
///
/// v0.2 follow-up: replace the sequential `stat` fan-out with a
/// parallel one + raise (or drop) this cap; see `cas_missing` for the
/// `TODO`.
pub const MAX_BATCH_MISSING: usize = 1000;

/// Barista-protocol version string surfaced in `/v1/health` and
/// `/v1/capabilities`. Bumped when the wire contract changes in a
/// non-backward-compatible way (separate from the crate version).
const PROTOCOL_VERSION: &str = "v1";

/// Build the barista-protocol sub-router (public + protected,
/// merged).
///
/// Equivalent to `public_router().merge(protected_router())`. Used
/// by callers that don't care about per-route auth — primarily the
/// integration tests that exercise the wire surface end-to-end.
/// Production assembly wires the two sub-routers separately and
/// applies the auth layer to only the protected one.
pub fn router() -> Router<AppState> {
    public_router().merge(protected_router())
}

/// Public sub-router: routes that MUST remain accessible without
/// credentials.
///
/// - `/v1/health` — protocol-level liveness; clients hit it before
///   they authenticate to confirm the protocol stack is up.
/// - `/v1/capabilities` — version negotiation; clients consult it
///   before they know what auth mechanism the server expects.
///
/// Mounting them on a separate sub-router (rather than punching
/// per-path exceptions inside the auth layer) keeps the auth layer
/// simple and the public-vs-protected split unambiguous from the
/// router topology alone.
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/capabilities", get(capabilities))
}

/// Protected sub-router: routes that require authentication when
/// any auth mechanism is configured.
///
/// All CAS endpoints live here. The caller wraps this router with
/// the configured `AuthLayer` before merging into the top-level
/// router; on a loopback dev server with no auth configured the
/// layer accepts anonymous requests so the wrapping is transparent.
pub fn protected_router() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/cas/sha256/{digest}",
            get(cas_get).head(cas_head).put(cas_put),
        )
        .route("/v1/cas/missing", post(cas_missing))
}

// -------------------------------------------------------------------
// GET / HEAD /v1/cas/sha256/{digest}
// -------------------------------------------------------------------

/// `GET /v1/cas/sha256/{digest}` — stream a blob back to the caller.
///
/// Thin wrapper around [`cas_get_inner`] that classifies the outcome
/// (`hit` / `miss` / `error`) and records it against the
/// `roastery_cas_requests_total` counter + the latency histogram.
/// Keeping the wrapper this slim means the inner function reads
/// exactly like a plain handler — no metric scaffolding obscures the
/// request flow.
async fn cas_get(
    state: State<AppState>,
    path: Path<String>,
    headers: HeaderMap,
) -> Result<Response, StorageError> {
    let started = std::time::Instant::now();
    let result = cas_get_inner(state, path, headers).await;
    let outcome = classify_get_or_head(&result);
    record_cas_request(CasMethod::Get, outcome, started.elapsed());
    result
}

/// Inner GET handler — see [`cas_get`] for the metrics wrapper.
///
/// On a local cache miss this handler consults the upstream-on-miss
/// fetcher when:
///
/// 1. an `UpstreamFetcher` is configured (`AppState::upstream` is
///    `Some`), and
/// 2. the request carries an `X-Barista-Coords` header.
///
/// If the fetcher succeeds, the blob is now in the local CAS and the
/// handler re-issues the standard `stat`+`get` path to stream it.
/// Otherwise the handler returns 404.
async fn cas_get_inner(
    State(state): State<AppState>,
    Path(digest_hex): Path<String>,
    headers: HeaderMap,
) -> Result<Response, StorageError> {
    let digest = parse_digest(&digest_hex)?;

    // Fast path: blob is local.
    if let Some(stat) = state.cas.stat(digest).await? {
        return serve_from_local(state.cas.clone(), digest, stat).await;
    }

    // Slow path: try the upstream-on-miss fetcher when it's
    // configured AND the caller supplied a coords hint.
    if let Some(fetcher) = &state.upstream
        && let Some(coords_header) = headers.get(&HDR_BARISTA_COORDS)
    {
        let coords_str = match coords_header.to_str() {
            Ok(s) => s.trim(),
            Err(_) => {
                return Ok(invalid_coords_response(
                    "X-Barista-Coords header is not valid ASCII",
                ));
            }
        };
        let coords = match Coords::parse(coords_str) {
            Ok(c) => c,
            Err(UpstreamError::InvalidCoords { reason }) => {
                return Ok(invalid_coords_response(&reason));
            }
            Err(other) => {
                // Other variants from Coords::parse aren't reachable
                // (it only produces InvalidCoords), but cover for the
                // future with a generic 400.
                return Ok(invalid_coords_response(&format!(
                    "coords parse failed: {other}"
                )));
            }
        };

        // try_fetch consumes per-attempt errors internally; the
        // outer Result here only carries the "not configured" /
        // genuine surprise paths, which we treat as a miss.
        match fetcher.try_fetch(digest, &coords).await {
            Ok(Some(_stat)) => {
                // Re-issue the local fast path. We deliberately
                // don't reuse the AsyncRead from inside the fetcher
                // — going back through `cas.stat`+`cas.get` keeps
                // the streaming codepath identical between "hot
                // local" and "freshly populated" hits, and the
                // second `stat` is a single syscall.
                if let Some(stat) = state.cas.stat(digest).await? {
                    return serve_from_local(state.cas.clone(), digest, stat).await;
                }
                // Vanishingly unlikely: the blob was evicted between
                // the put and the re-stat. Fall through to 404.
            }
            Ok(None) => {
                // All upstreams missed — fall through to 404.
            }
            Err(_) => {
                // Programming-level error (NotConfigured); fall
                // through to 404 rather than 500ing the client.
            }
        }
    }

    Ok(not_found(digest))
}

/// Serve a blob from the local CAS, building the standard streaming
/// response. Shared by the fast path and the post-upstream-fetch path
/// so the response shape is identical in both cases.
async fn serve_from_local(
    cas: std::sync::Arc<dyn crate::storage::Cas>,
    digest: Digest,
    stat: crate::storage::Stat,
) -> Result<Response, StorageError> {
    let Some(reader) = cas.get(digest).await? else {
        // Race: the blob existed at `stat` time but was removed before
        // `get` could open it. Surface as 404 — by the time the
        // response reaches the client, the blob really is gone.
        return Ok(not_found(digest));
    };
    Ok(streaming_response(stat.size, digest, reader))
}

/// Build a 400 response with `BAR-CACHE-008` for a malformed
/// `X-Barista-Coords` header. The body shape matches every other
/// error response on this surface.
fn invalid_coords_response(reason: &str) -> Response {
    let body = ErrorBody::new(
        "BAR-CACHE-008",
        format!("invalid X-Barista-Coords header: {reason}"),
    );
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

/// `HEAD /v1/cas/sha256/{digest}` — existence check.
///
/// Returns the same headers a successful `GET` would, but no body.
/// Axum + hyper take care of suppressing the body for `HEAD` even if
/// we returned one; we omit it explicitly anyway so the codepath is
/// obvious.
async fn cas_head(state: State<AppState>, path: Path<String>) -> Result<Response, StorageError> {
    let started = std::time::Instant::now();
    let result = cas_head_inner(state, path).await;
    let outcome = classify_get_or_head(&result);
    record_cas_request(CasMethod::Head, outcome, started.elapsed());
    result
}

/// Inner HEAD handler — see [`cas_head`] for the metrics wrapper.
async fn cas_head_inner(
    State(state): State<AppState>,
    Path(digest_hex): Path<String>,
) -> Result<Response, StorageError> {
    let digest = parse_digest(&digest_hex)?;
    let Some(stat) = state.cas.stat(digest).await? else {
        return Ok(not_found(digest));
    };
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::OK;
    apply_cas_headers(resp.headers_mut(), stat.size, digest);
    Ok(resp)
}

/// Build a 200 OK streaming response for a CAS blob.
///
/// `Body::from_stream` expects a `TryStream<Item = Result<Bytes, _>>`;
/// `ReaderStream` adapts an `AsyncRead` to exactly that. We carry the
/// reader by `Box<dyn AsyncRead + Send + Unpin>` (the `CasReader`
/// alias) so heterogenous backends compose without monomorphisation
/// per backend.
fn streaming_response(size: u64, digest: Digest, reader: CasReader) -> Response {
    let stream = ReaderStream::new(reader);
    let body = Body::from_stream(stream);
    let mut resp = Response::new(body);
    apply_cas_headers(resp.headers_mut(), size, digest);
    resp
}

/// Apply the CAS response headers used by both `GET` and `HEAD`:
/// `Content-Length`, `Content-Type: application/octet-stream`, and the
/// `X-Barista-Digest: sha256:<hex>` echo of the digest.
fn apply_cas_headers(headers: &mut HeaderMap, size: u64, digest: Digest) {
    // `HeaderValue::from_str` only fails on non-ASCII; `size.to_string`
    // and the digest's lowercase hex are ASCII by construction. We
    // still go through the fallible API and fall back to an empty
    // value on the impossible-but-typeable error path rather than
    // unwrap, to respect the workspace's no-panic lint.
    if let Ok(v) = HeaderValue::from_str(&size.to_string()) {
        headers.insert(header::CONTENT_LENGTH, v);
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("sha256:{}", digest.to_hex())) {
        headers.insert(HDR_BARISTA_DIGEST, v);
    }
}

/// Build a 404 Not Found response with a stable JSON error body
/// identifying the absent digest.
fn not_found(digest: Digest) -> Response {
    let body = ErrorBody::new(
        "BAR-CAS-404",
        format!("no blob in store for sha256:{}", digest.to_hex()),
    );
    (StatusCode::NOT_FOUND, Json(body)).into_response()
}

// -------------------------------------------------------------------
// PUT /v1/cas/sha256/{digest}
// -------------------------------------------------------------------

/// `PUT /v1/cas/sha256/{digest}` — upload + verify a blob.
///
/// Headers:
///
/// - `Content-Length` (required) — axum/hyper enforces this for
///   buffered bodies; clients streaming a chunked body must still
///   advertise the total length so the server can budget I/O.
/// - `Content-SHA256` (optional) — if present, MUST equal the path
///   digest. Allows clients that want a header-level digest assertion
///   (matching the REAPI convention) without having to round-trip a
///   second URL.
///
/// On a digest-mismatch the underlying `Cas::put` discards the partial
/// write and the response is `400 BAR-CAS-001` with `expected` /
/// `actual` populated. Re-PUTting an existing blob is idempotent —
/// the backend treats a digest collision as success (SHA-256 makes
/// "same digest" definitionally "same bytes").
async fn cas_put(
    state: State<AppState>,
    path: Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, StorageError> {
    let started = std::time::Instant::now();
    let result = cas_put_inner(state, path, headers, body).await;
    let outcome = classify_put(&result);
    record_cas_request(CasMethod::Put, outcome, started.elapsed());
    result
}

/// Inner PUT handler — see [`cas_put`] for the metrics wrapper.
async fn cas_put_inner(
    State(state): State<AppState>,
    Path(digest_hex): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, StorageError> {
    let digest = parse_digest(&digest_hex)?;

    // Optional header-level digest assertion. If the client supplied
    // both a path digest and a `Content-SHA256` header, they must
    // agree before we even read the body — a mismatch here is a
    // client bug, not a content-integrity failure.
    if let Some(hv) = headers.get("content-sha256") {
        let hv_str = hv
            .to_str()
            .map_err(|_| StorageError::InvalidDigest {
                reason: "Content-SHA256 header is not valid ASCII".to_string(),
            })?
            .trim();
        // Accept either `sha256:<hex>` or bare hex.
        let normalised = hv_str.strip_prefix("sha256:").unwrap_or(hv_str);
        let header_digest = Digest::from_hex(normalised)?;
        if header_digest != digest {
            let body = ErrorBody::digest_mismatch(&digest.to_hex(), &header_digest.to_hex());
            return Ok((StatusCode::BAD_REQUEST, Json(body)).into_response());
        }
    }

    // Turn the axum `Body` into an `AsyncRead` so we can hand it to
    // `Cas::put` as a `CasReader`. `BodyAsyncRead` wraps the body's
    // chunk stream and exposes the `AsyncRead` contract; `Cas::put`
    // hashes-and-verifies as bytes flow through.
    let reader: CasReader = Box::new(BodyAsyncRead::new(body));

    let stat = state.cas.put(digest, reader).await?;

    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::CREATED;
    // Echo the verified digest. `Content-Length: 0` is set explicitly
    // — clients that pipeline a follow-up GET shouldn't have to wait
    // for the connection to drain.
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    if let Ok(v) = HeaderValue::from_str(&format!("sha256:{}", stat.digest.to_hex())) {
        headers.insert(HDR_BARISTA_DIGEST, v);
    }
    Ok(resp)
}

/// Adapter that exposes an axum `Body` as a `tokio::io::AsyncRead`,
/// so we can hand it to the trait `Cas::put` (which takes a
/// `Box<dyn AsyncRead + Send + Unpin>`).
///
/// Internally we lift the body's chunk stream to a `StreamReader`,
/// which is the standard `Stream<Bytes>` → `AsyncRead` bridge from
/// `tokio_util::io`. The wrapper is a thin field-struct so we can name
/// the type for the `Box<dyn …>` we hand to the storage layer.
struct BodyAsyncRead {
    inner: StreamReader<BodyAsErrIoStream, Bytes>,
}

impl BodyAsyncRead {
    fn new(body: Body) -> Self {
        let stream = BodyAsErrIoStream {
            inner: body.into_data_stream(),
        };
        Self {
            inner: StreamReader::new(stream),
        }
    }
}

impl AsyncRead for BodyAsyncRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

/// Wrapper that converts the axum body data stream's `axum::Error`
/// items into `io::Error`, the item type `StreamReader` expects.
struct BodyAsErrIoStream {
    inner: BodyDataStream,
}

impl Stream for BodyAsErrIoStream {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match <BodyDataStream as TryStream>::try_poll_next(Pin::new(&mut self.inner), cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Ok(chunk))) => Poll::Ready(Some(Ok(chunk))),
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(io::Error::other(format!(
                "request body read error: {e}"
            ))))),
        }
    }
}

// -------------------------------------------------------------------
// POST /v1/cas/missing
// -------------------------------------------------------------------

/// Request body for [`cas_missing`]. Accepts either bare 64-char
/// lowercase hex digests or `sha256:<hex>`-prefixed entries; the
/// handler normalises before looking each up.
#[derive(Debug, Deserialize)]
struct MissingRequest {
    digests: Vec<String>,
}

/// Response body for [`cas_missing`]. Entries are always emitted with
/// the canonical `sha256:` prefix regardless of how the client wrote
/// them in the request.
#[derive(Debug, Serialize)]
struct MissingResponse {
    missing: Vec<String>,
}

/// `POST /v1/cas/missing` — return the subset of supplied digests
/// that are NOT present in the store.
///
/// The v0.1 implementation does a sequential `Cas::stat` per entry.
/// Sequential is fine for the typical request size (a handful to a few
/// dozen digests during dependency resolution); larger batches are
/// capped at [`MAX_BATCH_MISSING`] with a 413.
///
/// v0.2 follow-up: parallel fan-out via `futures::stream::iter` +
/// `buffer_unordered`, and/or a streaming pagination cursor for very
/// large batches. Both are layered changes on this handler; the wire
/// contract stays put.
async fn cas_missing(
    State(state): State<AppState>,
    Json(req): Json<MissingRequest>,
) -> Result<Response, StorageError> {
    if req.digests.len() > MAX_BATCH_MISSING {
        let body = ErrorBody::new(
            "BAR-CAS-004",
            format!(
                "batch size {} exceeds the per-call cap of {}",
                req.digests.len(),
                MAX_BATCH_MISSING
            ),
        );
        return Ok((StatusCode::PAYLOAD_TOO_LARGE, Json(body)).into_response());
    }

    // Parse and de-dupe up front so we can give back a clean 400 on a
    // single bad entry before doing any I/O. We preserve input order
    // for the response so clients can correlate by position if they
    // want to — the JSON shape doesn't require it, but it's the
    // less-surprising behaviour.
    let mut parsed: Vec<Digest> = Vec::with_capacity(req.digests.len());
    for raw in &req.digests {
        parsed.push(parse_digest_loose(raw)?);
    }

    let mut missing: Vec<String> = Vec::new();
    for digest in parsed {
        if state.cas.stat(digest).await?.is_none() {
            missing.push(format!("sha256:{}", digest.to_hex()));
        }
    }

    Ok((StatusCode::OK, Json(MissingResponse { missing })).into_response())
}

// -------------------------------------------------------------------
// GET /v1/health
// -------------------------------------------------------------------

/// JSON payload returned by [`health`].
#[derive(Debug, Serialize)]
struct HealthBody {
    status: &'static str,
    protocol: &'static str,
    version: &'static str,
}

/// `GET /v1/health` — barista-protocol liveness.
///
/// Returns a fixed JSON document declaring "the barista protocol
/// surface is mounted and responding." Distinct from the operational
/// `/healthz` endpoint a follow-up task introduces; both coexist.
async fn health() -> Response {
    Json(HealthBody {
        status: "ok",
        protocol: "barista",
        version: PROTOCOL_VERSION,
    })
    .into_response()
}

// -------------------------------------------------------------------
// GET /v1/capabilities
// -------------------------------------------------------------------

/// JSON payload returned by [`capabilities`].
#[derive(Debug, Serialize)]
struct CapabilitiesBody {
    protocol: &'static str,
    version: &'static str,
    cas: CapabilitiesCas,
    storage: CapabilitiesStorage,
}

#[derive(Debug, Serialize)]
struct CapabilitiesCas {
    hashes: Vec<&'static str>,
    max_batch_missing: usize,
}

#[derive(Debug, Serialize)]
struct CapabilitiesStorage {
    backend: &'static str,
}

/// `GET /v1/capabilities` — server feature flags + storage backend
/// discriminant.
///
/// The `storage.backend` field reflects what the running server is
/// configured against (`filesystem`, `s3`, or `gcs`), not what the
/// build supports. Clients should treat this as informational: the
/// wire protocol is the same regardless of backend.
async fn capabilities(State(state): State<AppState>) -> Response {
    let backend = backend_name(&state.config.storage);
    Json(CapabilitiesBody {
        protocol: "barista",
        version: PROTOCOL_VERSION,
        cas: CapabilitiesCas {
            hashes: vec!["sha256"],
            max_batch_missing: MAX_BATCH_MISSING,
        },
        storage: CapabilitiesStorage { backend },
    })
    .into_response()
}

/// Stable wire name for a storage backend, as emitted in the
/// `capabilities.storage.backend` field.
fn backend_name(backend: &StorageBackend) -> &'static str {
    match backend {
        StorageBackend::Filesystem(_) => "filesystem",
        StorageBackend::S3 { .. } => "s3",
        StorageBackend::Gcs { .. } => "gcs",
    }
}

// -------------------------------------------------------------------
// Shared helpers
// -------------------------------------------------------------------

/// Parse a strict 64-char lowercase hex digest from a URL segment.
///
/// The path captures the whole segment as a `String`; `Digest::from_hex`
/// then enforces the canonical form. A malformed value surfaces as
/// `StorageError::InvalidDigest`, which the `IntoResponse` impl maps
/// to a 400 `BAR-CAS-002`.
fn parse_digest(hex: &str) -> Result<Digest, StorageError> {
    Digest::from_hex(hex)
}

/// Parse a digest from a `cas/missing` request entry. Accepts either
/// `sha256:<hex>` or bare `<hex>`. Anything else surfaces as a 400
/// via the standard `InvalidDigest` mapping.
fn parse_digest_loose(raw: &str) -> Result<Digest, StorageError> {
    let trimmed = raw.trim();
    let hex = trimmed.strip_prefix("sha256:").unwrap_or(trimmed);
    Digest::from_hex(hex)
}

/// Classify a `GET` / `HEAD` handler result for the
/// `roastery_cas_requests_total` counter.
///
/// - `Err(_)` → `error` (bad digest, I/O failure, …).
/// - `Ok(resp)` where status is 404 → `miss`.
/// - any other `Ok(resp)` → `hit`.
fn classify_get_or_head(result: &Result<Response, StorageError>) -> CasResult {
    match result {
        Err(_) => CasResult::Error,
        Ok(resp) => {
            if resp.status() == StatusCode::NOT_FOUND {
                CasResult::Miss
            } else {
                CasResult::Hit
            }
        }
    }
}

/// Classify a `PUT` handler result for the `roastery_cas_requests_total`
/// counter.
///
/// - `Err(_)` → `error`.
/// - `Ok(resp)` whose status is 2xx → `hit` (a successful store is the
///   PUT analogue of a hit; the blob is in the cache afterwards).
/// - any other `Ok(resp)` (4xx digest-mismatch responses we built
///   in-band) → `error`.
fn classify_put(result: &Result<Response, StorageError>) -> CasResult {
    match result {
        Err(_) => CasResult::Error,
        Ok(resp) => {
            if resp.status().is_success() {
                CasResult::Hit
            } else {
                CasResult::Error
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn parse_digest_loose_accepts_bare_and_prefixed() {
        let hex = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        let bare = parse_digest_loose(hex).unwrap();
        let prefixed = parse_digest_loose(&format!("sha256:{hex}")).unwrap();
        assert_eq!(bare, prefixed);
        assert_eq!(bare.to_hex(), hex);
    }

    #[test]
    fn parse_digest_loose_rejects_garbage() {
        let err = parse_digest_loose("not-a-digest").unwrap_err();
        assert!(matches!(err, StorageError::InvalidDigest { .. }));
        // Uppercase is rejected on purpose (canonical form is lowercase).
        let err =
            parse_digest_loose("B94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9")
                .unwrap_err();
        assert!(matches!(err, StorageError::InvalidDigest { .. }));
    }

    #[test]
    fn backend_name_covers_all_variants() {
        let fs = StorageBackend::Filesystem(std::path::PathBuf::from("/tmp"));
        assert_eq!(backend_name(&fs), "filesystem");
        let s3 = StorageBackend::S3 {
            bucket: "b".into(),
            region: "r".into(),
        };
        assert_eq!(backend_name(&s3), "s3");
        let gcs = StorageBackend::Gcs {
            bucket: "b".into(),
            project: "p".into(),
        };
        assert_eq!(backend_name(&gcs), "gcs");
    }
}
