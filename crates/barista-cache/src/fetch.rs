// SPDX-License-Identifier: MIT OR Apache-2.0

//! HTTP/2 multiplexed upstream fetcher.
//!
//! Fetches POMs, artifacts, and `maven-metadata.xml` from configured
//! upstream Maven repositories. Built on `reqwest` with `hyper`'s
//! HTTP/2 backend for multiplexed parallel fetches over a single
//! connection per host. Honors:
//!
//! - **Peak concurrent connections ceiling** via a tokio semaphore.
//!   The default is 6; this is configurable through [`FetchConfig`].
//! - **Conditional requests** (`If-None-Match` / `If-Modified-Since`)
//!   so cached entries can be revalidated without re-downloading. A
//!   304 Not Modified bumps the cache's atime without rewriting the
//!   blob; see [`FetchOutcome::NotModified`].
//! - **Request timeout** propagated from config to the HTTP client.
//!
//! The fetcher is cheap to clone — internally it wraps an `Arc` over
//! the underlying `reqwest::Client` and semaphore, so a single
//! instance can be shared across many concurrent resolution tasks.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use reqwest::header::{
    CONTENT_TYPE, ETAG, HeaderMap, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED,
};
use reqwest::{Client, ClientBuilder};
use tokio::sync::Semaphore;

/// How long an idle pooled connection is kept alive for reuse. Sized to
/// comfortably outlast a single build's resolve burst so the many
/// fetches of a cold build reuse one connection per host instead of
/// churning fresh TCP/TLS handshakes (PRD §18.5 O-PROTO-01).
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Tunable knobs for the fetcher. These map 1:1 onto the network
/// section of the user-facing config; the cache crate keeps its own
/// struct so the fetcher does not depend on `barista-config`.
#[derive(Debug, Clone)]
pub struct FetchConfig {
    /// Ceiling on simultaneously in-flight requests. Enforced via a
    /// semaphore on top of the HTTP/2 connection pool.
    pub max_concurrent_connections: u32,
    /// Per-request timeout applied by the HTTP client.
    pub request_timeout: Duration,
    /// If `true` (the default), prefer HTTP/2: the client negotiates
    /// `h2` via ALPN over TLS and falls back to HTTP/1.1 only when the
    /// host declines it (PRD §18.5 O-PROTO-01). This is ALPN-negotiated,
    /// not prior-knowledge, so it is safe against HTTP/1.1-only hosts.
    /// If `false`, the client is pinned to HTTP/1.1 — tests against
    /// legacy HTTP/1.1 mocks set this.
    pub http2_enabled: bool,
    /// User-Agent header sent with every request.
    pub user_agent: String,
    /// Default upstream root, e.g. `https://repo.maven.apache.org/maven2`.
    /// Per-call overrides are supported on the URL-composition helpers.
    pub default_upstream: String,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            max_concurrent_connections: 6,
            request_timeout: Duration::from_secs(60),
            http2_enabled: true,
            user_agent: concat!("barista/", env!("CARGO_PKG_VERSION")).to_string(),
            default_upstream: "https://repo.maven.apache.org/maven2".to_string(),
        }
    }
}

/// Errors emitted by the fetcher. Transport-level failures keep the
/// originating URL on the variant so logs and surface messages can
/// identify which coord triggered the problem.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("HTTP transport error fetching {url}: {source}")]
    Transport {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("HTTP {status} fetching {url}")]
    Status { url: String, status: u16 },
    #[error("response body too large at {url}: limit {limit_bytes} bytes")]
    BodyTooLarge { url: String, limit_bytes: u64 },
    #[error("timeout after {seconds}s fetching {url}")]
    Timeout { url: String, seconds: u64 },
    #[error("invalid URL: {url}")]
    InvalidUrl { url: String },
}

/// Outcome of a single fetch. `Fresh` carries the downloaded bytes
/// plus any response headers the cache may want to persist (so the
/// next revalidation can issue a conditional request). `NotModified`
/// signals a 304 from upstream — the caller should bump the local
/// entry's atime and reuse the existing blob.
#[derive(Debug)]
pub enum FetchOutcome {
    Fresh {
        bytes: Bytes,
        etag: Option<String>,
        last_modified: Option<String>,
        content_type: Option<String>,
    },
    NotModified,
}

/// Cache-supplied conditional headers for a coord. If both are set,
/// the request carries both — Maven Central and most mirrors honor
/// either, and supplying both is idempotent.
#[derive(Debug, Default, Clone)]
pub struct ConditionalHeaders {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Multiplexed HTTP/2 fetcher. Clone-friendly (Arc internally).
#[derive(Debug, Clone)]
pub struct Fetcher {
    inner: Arc<FetcherInner>,
}

#[derive(Debug)]
struct FetcherInner {
    client: Client,
    semaphore: Semaphore,
    config: FetchConfig,
}

impl Fetcher {
    /// Construct a fetcher with the given config. The underlying
    /// HTTP client is reused across all `.fetch()` calls so HTTP/2
    /// connections stay warm.
    pub fn new(config: FetchConfig) -> Result<Self, FetchError> {
        let mut builder = ClientBuilder::new()
            .timeout(config.request_timeout)
            .user_agent(config.user_agent.clone())
            // O-XFER-02 (PRD §18.4): negotiate content compression so
            // text resources (POMs, maven-metadata.xml, repo-manager
            // JSON) come down compressed. `reqwest`'s `gzip` feature
            // advertises `Accept-Encoding: gzip` on every request that
            // doesn't set the header itself and transparently inflates
            // the response, so callers still see a plain byte stream.
            // gzip is the universally-supported codec and matches the
            // Maven 3.9.x baseline; zstd/brotli negotiation is sequenced
            // after the HTTP/2-default work (see EFF-2026-003).
            .gzip(true)
            // Honor pool size in the same ballpark as the semaphore;
            // the semaphore is the source of truth for the ceiling.
            .pool_max_idle_per_host(config.max_concurrent_connections as usize)
            // Keep idle connections warm across the resolve burst so a
            // cold build reuses connections instead of churning new
            // handshakes (O-PROTO-01 persistent connections).
            .pool_idle_timeout(POOL_IDLE_TIMEOUT);
        if config.http2_enabled {
            // O-PROTO-01 (PRD §18.5): prefer HTTP/2. `reqwest` negotiates
            // `h2` via ALPN over TLS and falls back to HTTP/1.1 when the
            // host declines, so this is safe against HTTP/1.1-only hosts
            // (no prior-knowledge forcing). The adaptive window lets a
            // single multiplexed connection carry the whole fetch fan-out.
            builder = builder.http2_adaptive_window(true);
        } else {
            builder = builder.http1_only();
        }
        let client = builder.build().map_err(|e| FetchError::Transport {
            url: "<client build>".into(),
            source: e,
        })?;
        Ok(Self {
            inner: Arc::new(FetcherInner {
                client,
                semaphore: Semaphore::new(config.max_concurrent_connections as usize),
                config,
            }),
        })
    }

    /// Issue a GET against `url`, optionally with conditional headers
    /// from a prior cache entry. The semaphore permit is held for the
    /// duration of the request — including body draining — so the
    /// ceiling applies to in-flight bytes-in-transit, not just
    /// dispatched headers.
    pub async fn fetch(
        &self,
        url: &str,
        cond: &ConditionalHeaders,
    ) -> Result<FetchOutcome, FetchError> {
        let _permit = self
            .inner
            .semaphore
            .acquire()
            .await
            .expect("fetch semaphore is never closed");

        let mut headers = HeaderMap::new();
        if let Some(etag) = &cond.etag {
            if let Ok(v) = HeaderValue::from_str(etag) {
                headers.insert(IF_NONE_MATCH, v);
            }
        }
        if let Some(lm) = &cond.last_modified {
            if let Ok(v) = HeaderValue::from_str(lm) {
                headers.insert(IF_MODIFIED_SINCE, v);
            }
        }

        let resp = self
            .inner
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    FetchError::Timeout {
                        url: url.into(),
                        seconds: self.inner.config.request_timeout.as_secs(),
                    }
                } else {
                    FetchError::Transport {
                        url: url.into(),
                        source: e,
                    }
                }
            })?;

        let status = resp.status();
        if status.as_u16() == 304 {
            return Ok(FetchOutcome::NotModified);
        }
        if !status.is_success() {
            return Err(FetchError::Status {
                url: url.into(),
                status: status.as_u16(),
            });
        }

        let etag = resp
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok().map(String::from));
        let last_modified = resp
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|v| v.to_str().ok().map(String::from));
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok().map(String::from));

        let bytes = resp.bytes().await.map_err(|e| {
            if e.is_timeout() {
                FetchError::Timeout {
                    url: url.into(),
                    seconds: self.inner.config.request_timeout.as_secs(),
                }
            } else {
                FetchError::Transport {
                    url: url.into(),
                    source: e,
                }
            }
        })?;

        Ok(FetchOutcome::Fresh {
            bytes,
            etag,
            last_modified,
            content_type,
        })
    }

    /// Compose the URL for a Maven artifact:
    /// `<upstream>/<group/slashed>/<artifact>/<version>/<artifact>-<version>[-classifier].<ext>`.
    pub fn url_for_artifact(
        &self,
        upstream: Option<&str>,
        group: &str,
        artifact: &str,
        version: &str,
        classifier: Option<&str>,
        extension: &str,
    ) -> String {
        let base = upstream
            .unwrap_or(&self.inner.config.default_upstream)
            .trim_end_matches('/');
        let group_path = group.replace('.', "/");
        let suffix = match classifier {
            Some(c) => format!("-{c}"),
            None => String::new(),
        };
        format!("{base}/{group_path}/{artifact}/{version}/{artifact}-{version}{suffix}.{extension}")
    }

    /// Compose the URL for an artifact's `maven-metadata.xml`:
    /// `<upstream>/<group/slashed>/<artifact>/maven-metadata.xml`.
    pub fn url_for_metadata(&self, upstream: Option<&str>, group: &str, artifact: &str) -> String {
        let base = upstream
            .unwrap_or(&self.inner.config.default_upstream)
            .trim_end_matches('/');
        let group_path = group.replace('.', "/");
        format!("{base}/{group_path}/{artifact}/maven-metadata.xml")
    }

    /// Compose the URL for a checksum sidecar by appending `.<algorithm>`
    /// to a base artifact URL. `algorithm` is conventionally `sha256`,
    /// `sha1`, or `md5`.
    pub fn url_for_sidecar(&self, artifact_url: &str, algorithm: &str) -> String {
        format!("{artifact_url}.{algorithm}")
    }

    /// Returns the configured ceiling on simultaneous in-flight
    /// requests. Exposed for telemetry and tests.
    pub fn max_concurrent_connections(&self) -> u32 {
        self.inner.config.max_concurrent_connections
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    /// Test config: HTTP/1.1, small timeout, default upstream pointed
    /// at the mock server.
    fn test_config(server: &MockServer, max_conn: u32, timeout_ms: u64) -> FetchConfig {
        FetchConfig {
            max_concurrent_connections: max_conn,
            request_timeout: Duration::from_millis(timeout_ms),
            http2_enabled: false,
            user_agent: "barista-test/0.0".into(),
            default_upstream: server.uri(),
        }
    }

    #[tokio::test]
    async fn new_with_default_config_succeeds() {
        let f = Fetcher::new(FetchConfig::default());
        assert!(f.is_ok());
    }

    #[tokio::test]
    async fn default_config_prefers_http2() {
        // O-PROTO-01: HTTP/2 is the default preference (ALPN-negotiated).
        assert!(FetchConfig::default().http2_enabled);
    }

    #[tokio::test]
    async fn sequential_fetches_reuse_pooled_client() {
        // O-PROTO-01 persistent connections: a burst of fetches through
        // one Fetcher rides the shared, pooled client. wiremock can't
        // surface the OS-level connection count, so this exercises the
        // reuse path by confirming a multi-fetch burst all succeeds; the
        // bound on *simultaneous* connections is proven separately by
        // `concurrent_connection_ceiling_is_enforced`.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/a"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes("x"))
            .mount(&server)
            .await;
        let f = Fetcher::new(test_config(&server, 4, 5_000)).unwrap();
        for _ in 0..3 {
            let out = f
                .fetch(
                    &format!("{}/a", server.uri()),
                    &ConditionalHeaders::default(),
                )
                .await
                .unwrap();
            assert!(matches!(out, FetchOutcome::Fresh { .. }));
        }
    }

    #[tokio::test]
    async fn fetch_200_returns_fresh_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/blob"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes("hello"))
            .mount(&server)
            .await;

        let f = Fetcher::new(test_config(&server, 4, 5_000)).unwrap();
        let out = f
            .fetch(
                &format!("{}/blob", server.uri()),
                &ConditionalHeaders::default(),
            )
            .await
            .unwrap();
        match out {
            FetchOutcome::Fresh { bytes, .. } => assert_eq!(bytes.as_ref(), b"hello"),
            _ => panic!("expected Fresh"),
        }
    }

    // --- O-XFER-02 compression negotiation (PRD §18.4, EFF-2026-003) ---

    #[tokio::test]
    async fn gzip_response_is_transparently_decompressed() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write as _;

        // A repetitive maven-metadata-shaped payload so gzip genuinely
        // shrinks the wire bytes (the whole point of O-XFER-02).
        let plaintext = "<metadata><versioning><versions>".to_string()
            + &"<version>1.0.0</version>".repeat(200)
            + "</versions></versioning></metadata>";
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(plaintext.as_bytes()).unwrap();
        let gzipped = enc.finish().unwrap();
        assert!(
            gzipped.len() < plaintext.len(),
            "fixture should compress: {} gz vs {} raw",
            gzipped.len(),
            plaintext.len()
        );

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/meta"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Encoding", "gzip")
                    .set_body_bytes(gzipped),
            )
            .mount(&server)
            .await;

        let f = Fetcher::new(test_config(&server, 4, 5_000)).unwrap();
        let out = f
            .fetch(
                &format!("{}/meta", server.uri()),
                &ConditionalHeaders::default(),
            )
            .await
            .unwrap();
        // The caller sees the inflated plaintext, with no awareness of
        // the transport encoding.
        match out {
            FetchOutcome::Fresh { bytes, .. } => {
                assert_eq!(bytes.as_ref(), plaintext.as_bytes())
            }
            _ => panic!("expected Fresh"),
        }
    }

    #[tokio::test]
    async fn requests_advertise_gzip_accept_encoding() {
        struct CaptureAccept(StdArc<std::sync::Mutex<Option<String>>>);
        impl Respond for CaptureAccept {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                let ae = req
                    .headers
                    .get("accept-encoding")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                *self.0.lock().unwrap() = ae;
                ResponseTemplate::new(200).set_body_bytes("ok")
            }
        }

        let captured = StdArc::new(std::sync::Mutex::new(None));
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(CaptureAccept(captured.clone()))
            .mount(&server)
            .await;

        let f = Fetcher::new(test_config(&server, 4, 5_000)).unwrap();
        f.fetch(
            &format!("{}/x", server.uri()),
            &ConditionalHeaders::default(),
        )
        .await
        .unwrap();

        let ae = captured
            .lock()
            .unwrap()
            .clone()
            .expect("request must carry an Accept-Encoding header");
        assert!(
            ae.contains("gzip"),
            "Accept-Encoding should advertise gzip, got {ae:?}"
        );
    }

    #[tokio::test]
    async fn etag_captured_in_fresh_outcome() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/e"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", "\"abc123\"")
                    .set_body_bytes("x"),
            )
            .mount(&server)
            .await;

        let f = Fetcher::new(test_config(&server, 4, 5_000)).unwrap();
        let out = f
            .fetch(
                &format!("{}/e", server.uri()),
                &ConditionalHeaders::default(),
            )
            .await
            .unwrap();
        match out {
            FetchOutcome::Fresh { etag, .. } => {
                assert_eq!(etag.as_deref(), Some("\"abc123\""));
            }
            _ => panic!("expected Fresh"),
        }
    }

    #[tokio::test]
    async fn last_modified_captured() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/lm"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Last-Modified", "Wed, 21 Oct 2026 07:28:00 GMT")
                    .set_body_bytes("x"),
            )
            .mount(&server)
            .await;

        let f = Fetcher::new(test_config(&server, 4, 5_000)).unwrap();
        let out = f
            .fetch(
                &format!("{}/lm", server.uri()),
                &ConditionalHeaders::default(),
            )
            .await
            .unwrap();
        match out {
            FetchOutcome::Fresh { last_modified, .. } => {
                assert_eq!(
                    last_modified.as_deref(),
                    Some("Wed, 21 Oct 2026 07:28:00 GMT")
                );
            }
            _ => panic!("expected Fresh"),
        }
    }

    #[tokio::test]
    async fn three_oh_four_yields_not_modified() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cond"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;

        let f = Fetcher::new(test_config(&server, 4, 5_000)).unwrap();
        let cond = ConditionalHeaders {
            etag: Some("\"prev\"".into()),
            last_modified: None,
        };
        let out = f
            .fetch(&format!("{}/cond", server.uri()), &cond)
            .await
            .unwrap();
        assert!(matches!(out, FetchOutcome::NotModified));
    }

    #[tokio::test]
    async fn if_none_match_header_is_sent() {
        let server = MockServer::start().await;
        // Only matches if the header is present — otherwise wiremock
        // returns 404 by default, which fails the assertion below.
        Mock::given(method("GET"))
            .and(path("/inm"))
            .and(header("if-none-match", "\"prev\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;

        let f = Fetcher::new(test_config(&server, 4, 5_000)).unwrap();
        let cond = ConditionalHeaders {
            etag: Some("\"prev\"".into()),
            last_modified: None,
        };
        let out = f
            .fetch(&format!("{}/inm", server.uri()), &cond)
            .await
            .unwrap();
        assert!(matches!(out, FetchOutcome::NotModified));
    }

    #[tokio::test]
    async fn if_modified_since_header_is_sent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ims"))
            .and(header_exists("if-modified-since"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;

        let f = Fetcher::new(test_config(&server, 4, 5_000)).unwrap();
        let cond = ConditionalHeaders {
            etag: None,
            last_modified: Some("Wed, 21 Oct 2026 07:28:00 GMT".into()),
        };
        let out = f
            .fetch(&format!("{}/ims", server.uri()), &cond)
            .await
            .unwrap();
        assert!(matches!(out, FetchOutcome::NotModified));
    }

    #[tokio::test]
    async fn status_404_surfaces_as_status_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let f = Fetcher::new(test_config(&server, 4, 5_000)).unwrap();
        let err = f
            .fetch(
                &format!("{}/missing", server.uri()),
                &ConditionalHeaders::default(),
            )
            .await
            .unwrap_err();
        match err {
            FetchError::Status { status, .. } => assert_eq!(status, 404),
            other => panic!("expected Status(404), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_500_surfaces_as_status_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/boom"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let f = Fetcher::new(test_config(&server, 4, 5_000)).unwrap();
        let err = f
            .fetch(
                &format!("{}/boom", server.uri()),
                &ConditionalHeaders::default(),
            )
            .await
            .unwrap_err();
        match err {
            FetchError::Status { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Status(500), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_when_response_is_slow() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(500))
                    .set_body_bytes("late"),
            )
            .mount(&server)
            .await;

        // 100ms timeout vs 500ms response delay.
        let f = Fetcher::new(test_config(&server, 4, 100)).unwrap();
        let err = f
            .fetch(
                &format!("{}/slow", server.uri()),
                &ConditionalHeaders::default(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                FetchError::Timeout { .. } | FetchError::Transport { .. }
            ),
            "expected Timeout or Transport, got {err:?}"
        );
    }

    /// **The headline acceptance test for PRD §2.4 SM-4.4.**
    ///
    /// Configures a ceiling of 2, dispatches 5 fetches concurrently
    /// against a mock that holds each request open for 200ms while
    /// tracking the peak number of simultaneously in-flight requests.
    /// Asserts the observed peak is `<= 2`.
    #[tokio::test]
    async fn concurrent_connection_ceiling_is_enforced() {
        // Custom responder that increments a counter on enter,
        // decrements on exit, and records the peak.
        struct CountingResponder {
            in_flight: StdArc<AtomicUsize>,
            peak: StdArc<AtomicUsize>,
        }
        impl Respond for CountingResponder {
            fn respond(&self, _: &Request) -> ResponseTemplate {
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                // Update peak monotonically.
                let mut prev = self.peak.load(Ordering::SeqCst);
                while now > prev {
                    match self
                        .peak
                        .compare_exchange(prev, now, Ordering::SeqCst, Ordering::SeqCst)
                    {
                        Ok(_) => break,
                        Err(actual) => prev = actual,
                    }
                }
                // The decrement happens on response *finalization*;
                // wiremock's `Respond` is sync, so we schedule the
                // decrement after the delay via a small body trick:
                // a delay holds the response open, and we decrement
                // synchronously here. Because wiremock builds the
                // ResponseTemplate eagerly but only sends after the
                // configured delay, the "in-flight" window is the
                // delay window. So: increment now, decrement after
                // the delay via a spawned task.
                let in_flight = self.in_flight.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                });
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(200))
                    .set_body_bytes("ok")
            }
        }

        let server = MockServer::start().await;
        let in_flight = StdArc::new(AtomicUsize::new(0));
        let peak = StdArc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/c"))
            .respond_with(CountingResponder {
                in_flight: in_flight.clone(),
                peak: peak.clone(),
            })
            .mount(&server)
            .await;

        let f = Fetcher::new(test_config(&server, 2, 5_000)).unwrap();
        let url = format!("{}/c", server.uri());

        let mut handles = Vec::new();
        for _ in 0..5 {
            let f = f.clone();
            let url = url.clone();
            handles.push(tokio::spawn(async move {
                f.fetch(&url, &ConditionalHeaders::default()).await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }

        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(
            observed_peak <= 2,
            "ceiling=2 but observed peak in-flight = {observed_peak}"
        );
        assert!(
            observed_peak >= 1,
            "test sanity: peak should be at least 1, got {observed_peak}"
        );
    }

    #[test]
    fn url_for_artifact_basic() {
        let f = Fetcher::new(FetchConfig::default()).unwrap();
        let u = f.url_for_artifact(
            Some("https://example.test/m2"),
            "com.example.lib",
            "thing",
            "1.2.3",
            None,
            "jar",
        );
        assert_eq!(
            u,
            "https://example.test/m2/com/example/lib/thing/1.2.3/thing-1.2.3.jar"
        );
    }

    #[test]
    fn url_for_artifact_with_classifier() {
        let f = Fetcher::new(FetchConfig::default()).unwrap();
        let u = f.url_for_artifact(
            Some("https://example.test/m2"),
            "com.example",
            "thing",
            "1.0",
            Some("sources"),
            "jar",
        );
        assert_eq!(
            u,
            "https://example.test/m2/com/example/thing/1.0/thing-1.0-sources.jar"
        );
    }

    #[test]
    fn url_for_artifact_uses_default_upstream() {
        let cfg = FetchConfig {
            default_upstream: "https://repo.example/m2".into(),
            ..FetchConfig::default()
        };
        let f = Fetcher::new(cfg).unwrap();
        let u = f.url_for_artifact(None, "a.b", "c", "1", None, "pom");
        assert_eq!(u, "https://repo.example/m2/a/b/c/1/c-1.pom");
    }

    #[test]
    fn url_for_artifact_trims_trailing_slash() {
        let f = Fetcher::new(FetchConfig::default()).unwrap();
        let u = f.url_for_artifact(Some("https://example.test/m2/"), "a", "b", "1", None, "jar");
        assert_eq!(u, "https://example.test/m2/a/b/1/b-1.jar");
    }

    #[test]
    fn url_for_metadata_basic() {
        let f = Fetcher::new(FetchConfig::default()).unwrap();
        let u = f.url_for_metadata(Some("https://example.test/m2"), "com.example", "thing");
        assert_eq!(
            u,
            "https://example.test/m2/com/example/thing/maven-metadata.xml"
        );
    }

    #[test]
    fn url_for_sidecar_appends_algorithm() {
        let f = Fetcher::new(FetchConfig::default()).unwrap();
        assert_eq!(
            f.url_for_sidecar("https://example.test/m2/a/b-1.jar", "sha256"),
            "https://example.test/m2/a/b-1.jar.sha256"
        );
        assert_eq!(
            f.url_for_sidecar("https://example.test/m2/a/b-1.jar", "sha1"),
            "https://example.test/m2/a/b-1.jar.sha1"
        );
    }
}
