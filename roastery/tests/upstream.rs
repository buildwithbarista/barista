//! Integration tests for the upstream-on-miss fetch path.
//!
//! The harness spawns a roastery server with a `TempDir`-backed
//! `FsCas` on one ephemeral port and one or more **mock upstream**
//! axum servers on other ephemeral ports. The mocks serve a small,
//! known blob at a known Maven path so the tests can assert
//! end-to-end:
//!
//! - `GET /v1/cas/sha256/<digest>` with `X-Barista-Coords` triggers
//!   an upstream fetch.
//! - On a hit, the blob is byte-equal to what the mock served.
//! - On a hit, the blob is **persisted** to the local CAS (proved by
//!   a second GET that omits the coords header and still returns the
//!   bytes — even after the mock is torn down).
//! - Multiple repos are tried in order; first hit wins.
//! - A digest mismatch from one repo falls through to the next.
//! - Metric labels for `hit` / `miss` / `digest_mismatch` are
//!   surfaced through `/metrics`.
//!
//! No test in this file talks to Maven Central or any other public
//! repository. The mock upstreams are entirely in-process.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State as AxumState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use reqwest::Client;
use roastery::{
    AppState, AuthConfig, FsCas, ServerConfig, StorageBackend, UpstreamConfig, UpstreamFetcher,
};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use url::Url;

// -------------------------------------------------------------------
// Mock upstream
// -------------------------------------------------------------------

/// Per-test mock upstream: a tiny axum server that serves a static
/// map of Maven-layout paths → response bodies. Anything not in the
/// map responds 404.
///
/// `bytes_override` lets a test serve **different** bytes at a path
/// than the canonical content — used by the digest-mismatch case to
/// simulate an upstream serving the wrong file for the digest.
#[derive(Clone, Default)]
struct MockUpstreamState {
    entries: HashMap<String, Vec<u8>>,
}

impl MockUpstreamState {
    fn with(mut self, path: &str, body: Vec<u8>) -> Self {
        self.entries.insert(path.to_string(), body);
        self
    }
}

async fn mock_upstream_handler(
    AxumState(state): AxumState<Arc<MockUpstreamState>>,
    AxumPath(path): AxumPath<String>,
) -> Response {
    match state.entries.get(&path) {
        Some(body) => (StatusCode::OK, body.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Body::from(format!("mock upstream: no entry for {path}\n")),
        )
            .into_response(),
    }
}

struct MockUpstream {
    addr: SocketAddr,
    server: Option<JoinHandle<()>>,
}

impl MockUpstream {
    fn base_url(&self) -> Url {
        // Trailing slash matters: `Url::join` discards the last
        // segment when the base lacks it.
        Url::parse(&format!("http://{}/", self.addr)).unwrap()
    }
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        if let Some(h) = self.server.take() {
            h.abort();
        }
    }
}

async fn spawn_mock_upstream(state: MockUpstreamState) -> MockUpstream {
    let std_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    let listener = TcpListener::from_std(std_listener).unwrap();

    let app = Router::new()
        .route("/{*path}", get(mock_upstream_handler))
        .with_state(Arc::new(state));

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    wait_for_listener(addr, Duration::from_secs(4)).await;

    MockUpstream {
        addr,
        server: Some(server),
    }
}

// -------------------------------------------------------------------
// Roastery harness
// -------------------------------------------------------------------

struct Harness {
    addr: SocketAddr,
    _tmp: TempDir,
    server: Option<JoinHandle<()>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(h) = self.server.take() {
            h.abort();
        }
    }
}

impl Harness {
    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// Spin up a roastery on an ephemeral port with the given upstream
/// configuration. Uses a fresh `TempDir`-backed `FsCas`.
async fn spawn_roastery_with_upstream(upstream_cfg: UpstreamConfig) -> Harness {
    let tmp = TempDir::new().unwrap();
    let storage_dir: PathBuf = tmp.path().to_path_buf();

    let std_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    let listener = TcpListener::from_std(std_listener).unwrap();

    let cas = FsCas::new(storage_dir.clone()).unwrap();
    let cas_arc: Arc<dyn roastery::Cas> = Arc::new(cas);

    // Build the upstream fetcher (when configured) directly so the
    // test doesn't depend on env-var plumbing.
    let upstream = if upstream_cfg.fetch_missing && !upstream_cfg.repos.is_empty() {
        let timeout = Duration::from_secs(u64::from(upstream_cfg.timeout_secs));
        let fetcher = UpstreamFetcher::new(upstream_cfg.repos.clone(), timeout, cas_arc.clone())
            .unwrap();
        Some(Arc::new(fetcher))
    } else {
        None
    };

    // Build the metrics registry so the `/metrics` assertions exercise
    // the same code production runs.
    roastery::ops::metrics::init();

    let cfg = ServerConfig {
        bind: addr,
        storage: StorageBackend::Filesystem(storage_dir.clone()),
        storage_dir,
        tls: None,
        auth: AuthConfig::default(),
        upstream: upstream_cfg,
    };
    let state = AppState {
        cas: cas_arc,
        config: Arc::new(cfg),
        upstream,
    };

    let app = axum::Router::new()
        .merge(roastery::proto::barista::router())
        .merge(roastery::ops::router())
        .with_state(state);

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    wait_for_listener(addr, Duration::from_secs(4)).await;

    Harness {
        addr,
        _tmp: tmp,
        server: Some(server),
    }
}

async fn wait_for_listener(addr: SocketAddr, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last_err = None;
    while Instant::now() < deadline {
        match TcpStream::connect(addr).await {
            Ok(_) => return,
            Err(e) => {
                last_err = Some(e);
                sleep(Duration::from_millis(20)).await;
            }
        }
    }
    panic!("server at {addr} did not accept connections within {timeout:?}: {last_err:?}");
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

/// Standard test artifact: 256 bytes of deterministic-but-non-trivial
/// content. Stable across runs so the digest is reproducible.
fn test_blob() -> Vec<u8> {
    (0..256u32).map(|i| (i ^ 0x5a) as u8).collect()
}

/// The Maven path for the standard `org.example:foo:1.0` coords (jar
/// packaging, no classifier).
fn test_blob_path() -> &'static str {
    "org/example/foo/1.0/foo-1.0.jar"
}

fn test_blob_coords() -> &'static str {
    "org.example:foo:1.0"
}

// -------------------------------------------------------------------
// [T] proof set
// -------------------------------------------------------------------

#[tokio::test]
async fn upstream_disabled_returns_404() {
    // `fetch_missing = false`: the GET must 404 without ever
    // touching an upstream.
    let h = spawn_roastery_with_upstream(UpstreamConfig {
        fetch_missing: false,
        repos: Vec::new(),
        timeout_secs: 5,
    })
    .await;

    let c = client();
    let blob = test_blob();
    let digest_hex = sha256_hex(&blob);
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), digest_hex);

    let resp = c
        .get(&url)
        .header("X-Barista-Coords", test_blob_coords())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn upstream_enabled_no_coords_header_returns_404() {
    // Upstream is configured AND has the blob, but the GET omits
    // the X-Barista-Coords header. The handler must NOT consult the
    // upstream — the coords hint is a hard requirement.
    let blob = test_blob();
    let digest_hex = sha256_hex(&blob);

    let upstream_state =
        MockUpstreamState::default().with(test_blob_path(), blob.clone());
    let upstream = spawn_mock_upstream(upstream_state).await;

    let h = spawn_roastery_with_upstream(UpstreamConfig {
        fetch_missing: true,
        repos: vec![upstream.base_url()],
        timeout_secs: 5,
    })
    .await;

    let c = client();
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), digest_hex);
    let resp = c.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn upstream_fetch_hits_first_repo_and_serves_blob() {
    let blob = test_blob();
    let digest_hex = sha256_hex(&blob);

    let upstream_state =
        MockUpstreamState::default().with(test_blob_path(), blob.clone());
    let upstream = spawn_mock_upstream(upstream_state).await;

    let h = spawn_roastery_with_upstream(UpstreamConfig {
        fetch_missing: true,
        repos: vec![upstream.base_url()],
        timeout_secs: 5,
    })
    .await;

    let c = client();
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), digest_hex);
    let resp = c
        .get(&url)
        .header("X-Barista-Coords", test_blob_coords())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "expected upstream-served 200");
    assert_eq!(
        resp.headers().get("content-length").unwrap(),
        &blob.len().to_string()
    );
    assert_eq!(
        resp.headers().get("x-barista-digest").unwrap(),
        &format!("sha256:{digest_hex}")
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), blob.as_slice(), "body byte-equal to upstream");
}

#[tokio::test]
async fn upstream_fetch_persists_to_local_cas() {
    let blob = test_blob();
    let digest_hex = sha256_hex(&blob);

    let upstream_state =
        MockUpstreamState::default().with(test_blob_path(), blob.clone());
    let upstream = spawn_mock_upstream(upstream_state).await;

    let h = spawn_roastery_with_upstream(UpstreamConfig {
        fetch_missing: true,
        repos: vec![upstream.base_url()],
        timeout_secs: 5,
    })
    .await;

    let c = client();
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), digest_hex);

    // Initial fetch — populates the local CAS via the upstream.
    let r1 = c
        .get(&url)
        .header("X-Barista-Coords", test_blob_coords())
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);
    drop(r1.bytes().await.unwrap());

    // Tear down the upstream. The local store MUST be enough on its
    // own from this point — the second GET tests that.
    drop(upstream);

    // Second GET, no coords header — must be served entirely from
    // the local CAS.
    let r2 = c.get(&url).send().await.unwrap();
    assert_eq!(r2.status(), 200, "second GET must be a local hit");
    let body = r2.bytes().await.unwrap();
    assert_eq!(body.as_ref(), blob.as_slice());
}

#[tokio::test]
async fn upstream_fetch_falls_through_to_second_repo() {
    let blob = test_blob();
    let digest_hex = sha256_hex(&blob);

    // First upstream: serves nothing (404).
    let empty_upstream = spawn_mock_upstream(MockUpstreamState::default()).await;
    // Second upstream: serves the blob.
    let real_upstream =
        spawn_mock_upstream(MockUpstreamState::default().with(test_blob_path(), blob.clone()))
            .await;

    let h = spawn_roastery_with_upstream(UpstreamConfig {
        fetch_missing: true,
        repos: vec![empty_upstream.base_url(), real_upstream.base_url()],
        timeout_secs: 5,
    })
    .await;

    let c = client();
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), digest_hex);
    let resp = c
        .get(&url)
        .header("X-Barista-Coords", test_blob_coords())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), blob.as_slice());
}

#[tokio::test]
async fn upstream_all_repos_404_returns_404() {
    let blob = test_blob();
    let digest_hex = sha256_hex(&blob);

    // Two upstreams, neither carries the blob.
    let u1 = spawn_mock_upstream(MockUpstreamState::default()).await;
    let u2 = spawn_mock_upstream(MockUpstreamState::default()).await;

    let h = spawn_roastery_with_upstream(UpstreamConfig {
        fetch_missing: true,
        repos: vec![u1.base_url(), u2.base_url()],
        timeout_secs: 5,
    })
    .await;

    let c = client();
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), digest_hex);
    let resp = c
        .get(&url)
        .header("X-Barista-Coords", test_blob_coords())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn upstream_digest_mismatch_falls_through() {
    let blob = test_blob();
    let digest_hex = sha256_hex(&blob);

    // First upstream: serves DIFFERENT bytes at the right path.
    let wrong_bytes = b"these bytes are not the requested digest's preimage".to_vec();
    let bad_upstream = spawn_mock_upstream(
        MockUpstreamState::default().with(test_blob_path(), wrong_bytes),
    )
    .await;
    // Second upstream: serves the correct bytes.
    let good_upstream = spawn_mock_upstream(
        MockUpstreamState::default().with(test_blob_path(), blob.clone()),
    )
    .await;

    let h = spawn_roastery_with_upstream(UpstreamConfig {
        fetch_missing: true,
        repos: vec![bad_upstream.base_url(), good_upstream.base_url()],
        timeout_secs: 5,
    })
    .await;

    let c = client();
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), digest_hex);
    let resp = c
        .get(&url)
        .header("X-Barista-Coords", test_blob_coords())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "second repo's bytes win");
    let body = resp.bytes().await.unwrap();
    assert_eq!(
        body.as_ref(),
        blob.as_slice(),
        "bytes match the SECOND repo's content, not the first"
    );
}

#[tokio::test]
async fn upstream_digest_mismatch_metric_recorded() {
    let blob = test_blob();
    let digest_hex = sha256_hex(&blob);

    let wrong_bytes = b"wrong bytes for metric test".to_vec();
    let bad_upstream = spawn_mock_upstream(
        MockUpstreamState::default().with(test_blob_path(), wrong_bytes),
    )
    .await;
    let good_upstream = spawn_mock_upstream(
        MockUpstreamState::default().with(test_blob_path(), blob.clone()),
    )
    .await;

    let h = spawn_roastery_with_upstream(UpstreamConfig {
        fetch_missing: true,
        repos: vec![bad_upstream.base_url(), good_upstream.base_url()],
        timeout_secs: 5,
    })
    .await;

    let c = client();
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), digest_hex);
    let resp = c
        .get(&url)
        .header("X-Barista-Coords", test_blob_coords())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    drop(resp.bytes().await.unwrap());

    // Scrape /metrics and look for the digest_mismatch counter.
    let metrics = c
        .get(format!("{}/metrics", h.base_url()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("roastery_upstream_fetch_total"),
        "metrics must include upstream counters: {metrics}"
    );
    assert!(
        metrics
            .lines()
            .any(|l| l.contains("roastery_upstream_fetch_total")
                && l.contains("digest_mismatch")
                && !l.starts_with('#')),
        "expected a digest_mismatch line in /metrics output, got:\n{metrics}"
    );
}

#[tokio::test]
async fn upstream_hit_metric_recorded() {
    let blob = test_blob();
    let digest_hex = sha256_hex(&blob);

    let upstream = spawn_mock_upstream(
        MockUpstreamState::default().with(test_blob_path(), blob.clone()),
    )
    .await;

    let h = spawn_roastery_with_upstream(UpstreamConfig {
        fetch_missing: true,
        repos: vec![upstream.base_url()],
        timeout_secs: 5,
    })
    .await;

    let c = client();
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), digest_hex);
    let resp = c
        .get(&url)
        .header("X-Barista-Coords", test_blob_coords())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    drop(resp.bytes().await.unwrap());

    let metrics = c
        .get(format!("{}/metrics", h.base_url()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics
            .lines()
            .any(|l| l.contains("roastery_upstream_fetch_total")
                && l.contains("result=\"hit\"")
                && !l.starts_with('#')),
        "expected a hit line in /metrics output, got:\n{metrics}"
    );
}

#[tokio::test]
async fn upstream_repos_empty_with_fetch_missing_enabled_fails_validate() {
    // `validate()` lives on `ServerConfig`; the integration-level
    // proof is that constructing such a config + calling validate
    // surfaces BAR-CACHE-007.
    let tmp = TempDir::new().unwrap();
    let cfg = ServerConfig {
        bind: "127.0.0.1:8443".parse().unwrap(),
        storage: StorageBackend::Filesystem(tmp.path().to_path_buf()),
        storage_dir: tmp.path().to_path_buf(),
        tls: None,
        auth: AuthConfig::default(),
        upstream: UpstreamConfig {
            fetch_missing: true,
            repos: Vec::new(),
            timeout_secs: 30,
        },
    };
    let err = cfg.validate().unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("BAR-CACHE-007"),
        "expected BAR-CACHE-007 in error, got: {msg}"
    );
}

#[tokio::test]
async fn upstream_coords_invalid_returns_400() {
    let blob = test_blob();
    let digest_hex = sha256_hex(&blob);

    // Upstream configured but the request coords are unparseable. The
    // handler must reject the request with a 400 + structured error
    // body, BEFORE touching the upstream.
    let upstream =
        spawn_mock_upstream(MockUpstreamState::default().with(test_blob_path(), blob)).await;
    let h = spawn_roastery_with_upstream(UpstreamConfig {
        fetch_missing: true,
        repos: vec![upstream.base_url()],
        timeout_secs: 5,
    })
    .await;

    let c = client();
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), digest_hex);
    let resp = c
        .get(&url)
        .header(
            "X-Barista-Coords",
            "not:valid:coords:with:way:too:many:colons",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BAR-CACHE-008");
}

#[tokio::test]
async fn upstream_only_fires_on_get_not_head_or_put() {
    let blob = test_blob();
    let digest_hex = sha256_hex(&blob);

    let upstream = spawn_mock_upstream(
        MockUpstreamState::default().with(test_blob_path(), blob.clone()),
    )
    .await;
    let h = spawn_roastery_with_upstream(UpstreamConfig {
        fetch_missing: true,
        repos: vec![upstream.base_url()],
        timeout_secs: 5,
    })
    .await;

    let c = client();
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), digest_hex);

    // HEAD with the coords header MUST still return 404 — only GET
    // triggers the upstream fetch.
    let head_resp = c
        .head(&url)
        .header("X-Barista-Coords", test_blob_coords())
        .send()
        .await
        .unwrap();
    assert_eq!(head_resp.status(), 404, "HEAD must not fetch upstream");

    // PUT semantics are unchanged — clients deliver the bytes
    // themselves, no upstream involvement. A valid PUT succeeds.
    let put_resp = c.put(&url).body(blob.clone()).send().await.unwrap();
    assert_eq!(put_resp.status(), 201, "PUT semantics unchanged by T6");

    // After the PUT, the blob IS in the local CAS. A subsequent
    // GET-without-coords is a local hit — proving HEAD's prior 404
    // wasn't because the blob had been silently fetched.
    let local_get = c.get(&url).send().await.unwrap();
    assert_eq!(local_get.status(), 200);
}
