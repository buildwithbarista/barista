//! [`RoasteryClient`] — the public client surface.
//!
//! Wraps a long-lived `reqwest::Client` (so connection pooling
//! kicks in across calls) plus the bits of [`ClientConfig`] we need
//! to consult on every request (the base URL, the bearer token if
//! any, the batch cap).
//!
//! Each public method maps 1-to-1 to an endpoint on the
//! roastery's barista-protocol surface:
//!
//! - [`get_blob`](RoasteryClient::get_blob) → `GET  /v1/cas/sha256/{digest}`
//! - [`stat_blob`](RoasteryClient::stat_blob) → `HEAD /v1/cas/sha256/{digest}`
//! - [`put_blob`](RoasteryClient::put_blob)  → `PUT  /v1/cas/sha256/{digest}`
//! - [`missing`](RoasteryClient::missing)   → `POST /v1/cas/missing`
//! - [`health`](RoasteryClient::health)    → `GET  /v1/health`
//! - [`capabilities`](RoasteryClient::capabilities) → `GET  /v1/capabilities`

use std::io;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::TryStreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::{Body, Client, Method, Response, StatusCode};
use serde::Deserialize;
use tokio::io::AsyncRead;
use tokio_util::io::{ReaderStream, StreamReader};
use url::Url;

use crate::config::{AuthConfig, ClientConfig, TlsConfig};
use crate::digest::Digest;
use crate::error::ClientError;
use crate::tls::build_client_config;
use crate::types::{
    BlobStat, BlobStream, CapabilitiesCas, CapabilitiesResponse, CapabilitiesStorage,
    HealthResponse,
};

/// HTTP header echoed by the roastery on every CAS response and
/// accepted on uploads — `sha256:<hex>`-formatted identifier of the
/// blob involved.
const HDR_BARISTA_DIGEST: &str = "x-barista-digest";

/// Async client for the roastery cache server's barista-protocol
/// surface.
///
/// Construct one per server with [`RoasteryClient::new`] and reuse
/// it across requests — the underlying `reqwest::Client` pools
/// connections, so a long-lived instance is the cheap path. The
/// client is `Clone` (cheap — internally `Arc`-shared) so it can be
/// handed to multiple async tasks.
#[derive(Clone, Debug)]
pub struct RoasteryClient {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    http: Client,
    base_url: Url,
    auth: AuthConfig,
    max_batch_missing: usize,
}

impl RoasteryClient {
    /// Construct a client for the given configuration.
    ///
    /// Returns a [`ClientError::Config`] if the configuration is
    /// internally inconsistent — most notably, if
    /// [`TlsConfig::PlainHttp`] is paired with an `https://` base
    /// URL.
    pub fn new(config: ClientConfig) -> Result<Self, ClientError> {
        validate(&config)?;
        let http = build_http_client(&config)?;
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                base_url: config.base_url,
                auth: config.auth,
                max_batch_missing: config.max_batch_missing,
            }),
        })
    }

    /// `GET /v1/cas/sha256/{digest}` — fetch a blob.
    ///
    /// Returns a [`BlobStream`] carrying the response headers
    /// (parsed into a [`BlobStat`]) and an `AsyncRead` over the
    /// body. The caller is responsible for consuming the body; the
    /// stream is dropped (and the connection released) when the
    /// returned [`BlobStream`] goes out of scope.
    ///
    /// A 404 response surfaces as [`ClientError::NotFound`]; auth
    /// failures as [`ClientError::Auth`]; anything else with a
    /// structured error body as [`ClientError::ServerRejected`].
    pub async fn get_blob(&self, digest: Digest) -> Result<BlobStream, ClientError> {
        let url = self.cas_url(digest)?;
        tracing::debug!(%url, "GET blob");

        let resp = self.send(self.request(Method::GET, url, true)?).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(ClientError::NotFound);
        }
        let resp = check_status(resp).await?;

        let stat = parse_blob_stat(resp.headers(), digest)?;
        let stream = resp.bytes_stream().map_err(io::Error::other);
        let body: Box<dyn AsyncRead + Send + Unpin> = Box::new(StreamReader::new(stream));

        tracing::info!(size = stat.size, "GET blob ok");
        Ok(BlobStream { stat, body })
    }

    /// `HEAD /v1/cas/sha256/{digest}` — existence check.
    ///
    /// Returns `Ok(Some(stat))` if the blob is present, `Ok(None)`
    /// if it's absent. Auth and other errors surface the same way
    /// as [`get_blob`](Self::get_blob).
    pub async fn stat_blob(&self, digest: Digest) -> Result<Option<BlobStat>, ClientError> {
        let url = self.cas_url(digest)?;
        tracing::debug!(%url, "HEAD blob");

        let resp = self.send(self.request(Method::HEAD, url, true)?).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = check_status(resp).await?;
        let stat = parse_blob_stat(resp.headers(), digest)?;
        Ok(Some(stat))
    }

    /// `PUT /v1/cas/sha256/{digest}` — upload + verify a blob.
    ///
    /// `body` is streamed to the server; `size` advertises the
    /// total length up front via `Content-Length` so the server
    /// can budget I/O. The server hashes-and-verifies as bytes flow
    /// in; a digest mismatch surfaces as
    /// `ClientError::ServerRejected { code: "BAR-CAS-001", .. }`.
    pub async fn put_blob<R>(
        &self,
        digest: Digest,
        body: R,
        size: u64,
    ) -> Result<(), ClientError>
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        let url = self.cas_url(digest)?;
        tracing::debug!(%url, size, "PUT blob");

        // Wrap the AsyncRead into a `Stream<io::Result<Bytes>>` that
        // `reqwest::Body::wrap_stream` accepts. This is what keeps
        // the upload path streaming — the whole blob never sits in
        // memory.
        let stream = ReaderStream::new(body);
        let req_body = Body::wrap_stream(stream.map_ok(Bytes::from));

        let mut req = self.request(Method::PUT, url, true)?;
        req = req
            .header(CONTENT_LENGTH, size)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(req_body);

        let resp = self.send(req).await?;
        let resp = check_status(resp).await?;
        if resp.status() != StatusCode::CREATED && !resp.status().is_success() {
            return Err(ClientError::BadResponse {
                reason: format!(
                    "expected 201 Created on PUT, got {}",
                    resp.status().as_u16()
                ),
            });
        }
        tracing::info!(size, "PUT blob ok");
        Ok(())
    }

    /// `POST /v1/cas/missing` — batch presence check.
    ///
    /// Returns the subset of `digests` the server reports as
    /// absent. The request is split into batches of at most
    /// [`ClientConfig::max_batch_missing`] entries; results from
    /// each batch are concatenated (input order is not preserved
    /// across batch boundaries, but within a batch the server
    /// preserves order).
    pub async fn missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, ClientError> {
        if digests.is_empty() {
            return Ok(Vec::new());
        }
        let cap = self.inner.max_batch_missing.max(1);

        let mut out: Vec<Digest> = Vec::new();
        for chunk in digests.chunks(cap) {
            let body = serde_json::json!({
                "digests": chunk
                    .iter()
                    .map(|d| format!("sha256:{}", d.to_hex()))
                    .collect::<Vec<_>>(),
            });
            let url = self.endpoint("v1/cas/missing")?;
            tracing::debug!(%url, batch = chunk.len(), "POST cas/missing");

            let req = self
                .request(Method::POST, url, true)?
                .json(&body);
            let resp = self.send(req).await?;
            let resp = check_status(resp).await?;

            #[derive(Deserialize)]
            struct MissingBody {
                missing: Vec<String>,
            }
            let parsed: MissingBody = resp.json().await.map_err(|e| ClientError::BadResponse {
                reason: format!("missing response did not parse as JSON: {e}"),
            })?;
            for entry in parsed.missing {
                let trimmed = entry.trim();
                let hex = trimmed.strip_prefix("sha256:").unwrap_or(trimmed);
                let d = Digest::from_hex(hex).map_err(|e| ClientError::BadResponse {
                    reason: format!("missing entry {entry:?} is not a valid digest: {e}"),
                })?;
                out.push(d);
            }
        }
        Ok(out)
    }

    /// `GET /v1/health` — protocol-level liveness probe.
    ///
    /// Always anonymous (the server's auth layer exempts this
    /// route). Useful for confirming the protocol surface is up
    /// before authenticating.
    pub async fn health(&self) -> Result<HealthResponse, ClientError> {
        let url = self.endpoint("v1/health")?;
        tracing::debug!(%url, "GET health");
        let resp = self.send(self.request(Method::GET, url, false)?).await?;
        let resp = check_status(resp).await?;

        #[derive(Deserialize)]
        struct Body {
            status: String,
            protocol: String,
            version: String,
        }
        let body: Body = resp.json().await.map_err(|e| ClientError::BadResponse {
            reason: format!("health response did not parse as JSON: {e}"),
        })?;
        Ok(HealthResponse {
            status: body.status,
            protocol: body.protocol,
            version: body.version,
        })
    }

    /// `GET /v1/capabilities` — server feature negotiation.
    ///
    /// Always anonymous. Clients should consult this before sending
    /// large `/v1/cas/missing` batches: the
    /// `cas.max_batch_missing` field is the server's hard cap.
    pub async fn capabilities(&self) -> Result<CapabilitiesResponse, ClientError> {
        let url = self.endpoint("v1/capabilities")?;
        tracing::debug!(%url, "GET capabilities");
        let resp = self.send(self.request(Method::GET, url, false)?).await?;
        let resp = check_status(resp).await?;

        #[derive(Deserialize)]
        struct Body {
            protocol: String,
            version: String,
            cas: CasBody,
            storage: StorageBody,
        }
        #[derive(Deserialize)]
        struct CasBody {
            hashes: Vec<String>,
            max_batch_missing: usize,
        }
        #[derive(Deserialize)]
        struct StorageBody {
            backend: String,
        }
        let body: Body = resp.json().await.map_err(|e| ClientError::BadResponse {
            reason: format!("capabilities response did not parse as JSON: {e}"),
        })?;
        Ok(CapabilitiesResponse {
            protocol: body.protocol,
            version: body.version,
            cas: CapabilitiesCas {
                hashes: body.cas.hashes,
                max_batch_missing: body.cas.max_batch_missing,
            },
            storage: CapabilitiesStorage {
                backend: body.storage.backend,
            },
        })
    }

    /// Build the URL for `/v1/cas/sha256/{digest}` against the
    /// configured base.
    fn cas_url(&self, digest: Digest) -> Result<Url, ClientError> {
        self.endpoint(&format!("v1/cas/sha256/{}", digest.to_hex()))
    }

    /// Build an endpoint URL by joining `path` onto the configured
    /// base URL.
    fn endpoint(&self, path: &str) -> Result<Url, ClientError> {
        // `Url::join` requires the base to end in `/` to treat the
        // last segment as a directory; we normalise here so callers
        // can pass a base with or without the trailing slash.
        let mut base = self.inner.base_url.clone();
        if !base.path().ends_with('/') {
            // SAFETY: appending '/' to an existing valid path is
            // always valid; the no-panic lint is satisfied because
            // we use `set_path` which doesn't panic.
            let p = format!("{}/", base.path());
            base.set_path(&p);
        }
        base.join(path).map_err(|e| ClientError::Config {
            reason: format!("could not build URL for {path:?}: {e}"),
        })
    }

    /// Build a `reqwest::RequestBuilder` for the given method/URL,
    /// optionally applying the bearer-auth header.
    ///
    /// `with_auth = false` is used for the always-public
    /// `/v1/health` and `/v1/capabilities` endpoints. Even when the
    /// client is configured with a bearer token, we don't send it
    /// to those routes — the server doesn't enforce auth on them
    /// and not sending the token over the wire is a small
    /// defense-in-depth win.
    fn request(
        &self,
        method: Method,
        url: Url,
        with_auth: bool,
    ) -> Result<reqwest::RequestBuilder, ClientError> {
        let mut req = self.inner.http.request(method, url);
        if with_auth {
            if let AuthConfig::Bearer { token } = &self.inner.auth {
                let value = format!("Bearer {token}");
                let header_value =
                    HeaderValue::from_str(&value).map_err(|e| ClientError::Config {
                        reason: format!("bearer token is not a valid header value: {e}"),
                    })?;
                req = req.header(AUTHORIZATION, header_value);
            }
        }
        Ok(req)
    }

    /// Send a built request, mapping the reqwest error into
    /// `ClientError` (separating `Timeout` from generic `Network`).
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Response, ClientError> {
        req.send().await.map_err(ClientError::from_reqwest)
    }
}

/// Validate a [`ClientConfig`] before building the HTTP client.
///
/// The single invariant we check up-front is the one the public
/// docs promise: `TlsConfig::PlainHttp` against an `https://` base
/// URL is a programming error and must surface as
/// [`ClientError::Config`] rather than failing at first request.
fn validate(cfg: &ClientConfig) -> Result<(), ClientError> {
    let scheme = cfg.base_url.scheme();
    match (scheme, &cfg.tls) {
        ("https", TlsConfig::PlainHttp) => Err(ClientError::Config {
            reason: "TlsConfig::PlainHttp cannot be used against an https:// base URL"
                .to_string(),
        }),
        ("http", TlsConfig::SystemRoots) | ("http", TlsConfig::CustomCa { .. }) => {
            // Permitted: a TLS-flavoured config against an http://
            // base is wasteful but not incorrect (the rustls config
            // simply won't be exercised). Surface as a debug log so
            // it's visible in tests.
            tracing::debug!(
                scheme = "http",
                "TLS config present but base URL is http — TLS will not be used",
            );
            Ok(())
        }
        ("http", TlsConfig::PlainHttp) => Ok(()),
        ("https", _) => Ok(()),
        (other, _) => Err(ClientError::Config {
            reason: format!("unsupported URL scheme {other:?}; expected http or https"),
        }),
    }
}

/// Build the `reqwest::Client` for a given configuration.
///
/// - Plain HTTP: build with no TLS config; rustls isn't touched.
/// - HTTPS: build a rustls `ClientConfig` (system roots / custom
///   CA, plus optional mTLS client identity) and hand it to
///   `reqwest::ClientBuilder::use_preconfigured_tls`.
fn build_http_client(cfg: &ClientConfig) -> Result<Client, ClientError> {
    let user_agent = cfg.user_agent.clone();
    let timeout = cfg.timeout;
    let builder = Client::builder()
        .user_agent(user_agent)
        .timeout(timeout)
        // The connect timeout defaults to "none" in reqwest; bound
        // it by the request timeout so a syn-blackhole doesn't
        // hang for longer than the caller asked.
        .connect_timeout(min_duration(timeout, Duration::from_secs(30)));

    let builder = if let TlsConfig::PlainHttp = cfg.tls {
        builder
    } else {
        let rustls_cfg = build_client_config(&cfg.tls, &cfg.auth)?;
        builder.use_preconfigured_tls(rustls_cfg)
    };

    builder.build().map_err(ClientError::from_reqwest)
}

/// Return the smaller of two durations.
fn min_duration(a: Duration, b: Duration) -> Duration {
    if a < b { a } else { b }
}

/// Parse the `Content-Length` + `X-Barista-Digest` headers from a
/// CAS response into a [`BlobStat`], cross-checking the digest
/// against the one the caller asked about.
fn parse_blob_stat(headers: &HeaderMap, expected: Digest) -> Result<BlobStat, ClientError> {
    let size = headers
        .get(CONTENT_LENGTH)
        .ok_or_else(|| ClientError::BadResponse {
            reason: "response is missing Content-Length".to_string(),
        })?
        .to_str()
        .map_err(|_| ClientError::BadResponse {
            reason: "Content-Length is not valid ASCII".to_string(),
        })?
        .parse::<u64>()
        .map_err(|e| ClientError::BadResponse {
            reason: format!("Content-Length is not a u64: {e}"),
        })?;

    let raw_digest = headers
        .get(HDR_BARISTA_DIGEST)
        .ok_or_else(|| ClientError::BadResponse {
            reason: "response is missing X-Barista-Digest".to_string(),
        })?
        .to_str()
        .map_err(|_| ClientError::BadResponse {
            reason: "X-Barista-Digest is not valid ASCII".to_string(),
        })?
        .trim();
    let hex = raw_digest.strip_prefix("sha256:").unwrap_or(raw_digest);
    let server_digest = Digest::from_hex(hex).map_err(|e| ClientError::BadResponse {
        reason: format!("X-Barista-Digest is not a valid SHA-256: {e}"),
    })?;
    if server_digest != expected {
        return Err(ClientError::BadResponse {
            reason: format!(
                "X-Barista-Digest mismatch: requested {} but server echoed {}",
                expected.to_hex(),
                server_digest.to_hex()
            ),
        });
    }
    Ok(BlobStat {
        size,
        digest: expected,
    })
}

/// Examine a response status. On 2xx, return the response
/// unchanged. On 401, map to [`ClientError::Auth`]. On any other
/// 4xx/5xx, try to parse the server's structured error body into
/// [`ClientError::ServerRejected`] (falling back to a synthetic
/// body when the response isn't JSON).
async fn check_status(resp: Response) -> Result<Response, ClientError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }

    let status_code = status.as_u16();

    // Drain the body once; the JSON parse is best-effort.
    let body_bytes = resp
        .bytes()
        .await
        .map_err(ClientError::from_reqwest)?;

    // Try the structured `ErrorBody` shape first. Match the server's
    // serialisation: `code`, `message`, optional `expected` /
    // `actual` (digest hex without the `sha256:` prefix).
    #[derive(Deserialize)]
    struct ErrorBody {
        code: String,
        message: String,
        #[serde(default)]
        expected: Option<String>,
        #[serde(default)]
        actual: Option<String>,
    }

    let parsed: Option<ErrorBody> = serde_json::from_slice(&body_bytes).ok();

    if status == StatusCode::UNAUTHORIZED {
        let (code, message) = parsed
            .map(|b| (b.code, b.message))
            .unwrap_or_else(|| ("BAR-AUTH-001".to_string(), "unauthorized".to_string()));
        return Err(ClientError::Auth { code, message });
    }

    let (code, message, expected, actual) = match parsed {
        Some(b) => {
            let expected_digest = b.expected.as_deref().and_then(|s| {
                let hex = s.strip_prefix("sha256:").unwrap_or(s);
                Digest::from_hex(hex).ok()
            });
            let actual_digest = b.actual.as_deref().and_then(|s| {
                let hex = s.strip_prefix("sha256:").unwrap_or(s);
                Digest::from_hex(hex).ok()
            });
            (b.code, b.message, expected_digest, actual_digest)
        }
        None => {
            let text = String::from_utf8_lossy(&body_bytes).into_owned();
            (
                "BAR-UNKNOWN".to_string(),
                if text.is_empty() {
                    format!("server returned HTTP {status_code} with empty body")
                } else {
                    text
                },
                None,
                None,
            )
        }
    };

    Err(ClientError::ServerRejected {
        status: status_code,
        code,
        message,
        expected,
        actual,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn validate_rejects_plain_http_against_https() {
        let url: Url = "https://example.com".parse().unwrap();
        let cfg = ClientConfig::builder(url)
            .tls(TlsConfig::PlainHttp)
            .build();
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, ClientError::Config { .. }));
    }

    #[test]
    fn validate_accepts_plain_http_against_http() {
        let url: Url = "http://127.0.0.1:8080".parse().unwrap();
        let cfg = ClientConfig::builder(url)
            .tls(TlsConfig::PlainHttp)
            .build();
        validate(&cfg).expect("plain http + http URL should validate");
    }

    #[test]
    fn validate_accepts_system_roots_against_https() {
        let url: Url = "https://example.com".parse().unwrap();
        let cfg = ClientConfig::builder(url)
            .tls(TlsConfig::SystemRoots)
            .build();
        validate(&cfg).expect("system roots + https should validate");
    }

    #[test]
    fn validate_rejects_unsupported_scheme() {
        let url: Url = "ftp://example.com".parse().unwrap();
        let cfg = ClientConfig::builder(url)
            .tls(TlsConfig::PlainHttp)
            .build();
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, ClientError::Config { .. }));
    }

    #[tokio::test]
    async fn new_refuses_plain_http_against_https() {
        let url: Url = "https://example.com".parse().unwrap();
        let cfg = ClientConfig::builder(url)
            .tls(TlsConfig::PlainHttp)
            .build();
        let err = RoasteryClient::new(cfg).unwrap_err();
        assert!(matches!(err, ClientError::Config { .. }));
    }

    #[tokio::test]
    async fn new_succeeds_for_plain_http_base() {
        let url: Url = "http://127.0.0.1:8080".parse().unwrap();
        let cfg = ClientConfig::builder(url)
            .tls(TlsConfig::PlainHttp)
            .build();
        let _client = RoasteryClient::new(cfg).expect("plain http construction");
    }
}
