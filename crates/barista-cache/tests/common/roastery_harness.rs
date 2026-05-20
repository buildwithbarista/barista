//! In-process roastery server fixtures for the cache integration
//! tests, mirroring the M5.2 T1 client-test harness pattern.
//!
//! Two server shapes are provided:
//!
//! - [`spawn_plain_roastery`] — plain HTTP, no auth, upstream-on-miss
//!   disabled. The CAS starts empty; tests seed it via the roastery
//!   client's `put_blob`.
//! - [`spawn_bearer_roastery`] — plain HTTP, bearer-auth required.
//!
//! A separate [`spawn_digest_mismatch_mock`] spins a tiny axum mock
//! that always answers `400 BAR-CAS-001` so the cache's
//! digest-mismatch fall-through can be exercised deterministically.

// This module is shared scaffolding compiled separately into each
// integration-test binary. Any given binary uses only a subset of the
// fixtures, so `dead_code` is expected here and silenced module-wide.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::Path as AxPath;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use roastery::{
    AppState, AuthConfig as RoasteryAuthConfig, AuthLayer, BearerAuthConfig, BearerVerifier, FsCas,
    ServerConfig, StorageBackend, UpstreamConfig,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::sleep;

pub struct RoasteryHarness {
    pub addr: SocketAddr,
    pub _tmp: TempDir,
    pub _tokens_file: Option<NamedTempFile>,
    pub server: Option<JoinHandle<()>>,
}

impl Drop for RoasteryHarness {
    fn drop(&mut self) {
        if let Some(h) = self.server.take() {
            h.abort();
        }
    }
}

impl RoasteryHarness {
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }
}

/// Spin a plain-HTTP roastery on an ephemeral port with no auth and
/// upstream-on-miss disabled. The CAS starts empty.
pub async fn spawn_plain_roastery() -> RoasteryHarness {
    let tmp = TempDir::new().expect("tempdir");
    let storage_dir: PathBuf = tmp.path().to_path_buf();

    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let addr = std_listener.local_addr().expect("addr");
    let listener = TcpListener::from_std(std_listener).expect("from std");

    let cas = FsCas::new(storage_dir.clone()).expect("cas");
    let cfg = ServerConfig {
        bind: addr,
        storage: StorageBackend::Filesystem(storage_dir.clone()),
        storage_dir,
        tls: None,
        auth: RoasteryAuthConfig::default(),
        upstream: UpstreamConfig::default(),
    };
    let state = AppState {
        cas: Arc::new(cas),
        config: Arc::new(cfg),
        upstream: None,
        bearer: None,
    };
    let app = axum::Router::new()
        .merge(roastery::proto::barista::public_router().with_state(state.clone()))
        .merge(roastery::proto::barista::protected_router().with_state(state.clone()))
        .merge(roastery::ops::router().with_state(state));

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    wait_for_listener(addr, Duration::from_secs(4)).await;

    RoasteryHarness {
        addr,
        _tmp: tmp,
        _tokens_file: None,
        server: Some(server),
    }
}

/// Spin a plain-HTTP roastery requiring bearer auth. The tokens file
/// contains a single `ci:s3cret` entry — the password the client
/// should send is `s3cret`.
pub async fn spawn_bearer_roastery() -> RoasteryHarness {
    let tmp = TempDir::new().expect("tempdir");
    let storage_dir: PathBuf = tmp.path().to_path_buf();

    let mut tokens = NamedTempFile::new().expect("tokens temp");
    use std::io::Write;
    writeln!(tokens, "ci:s3cret").expect("write tokens");

    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let addr = std_listener.local_addr().expect("addr");
    let listener = TcpListener::from_std(std_listener).expect("from std");

    let cas = FsCas::new(storage_dir.clone()).expect("cas");
    let cfg = ServerConfig {
        bind: addr,
        storage: StorageBackend::Filesystem(storage_dir.clone()),
        storage_dir,
        tls: None,
        auth: RoasteryAuthConfig {
            bearer: Some(BearerAuthConfig {
                tokens_file: tokens.path().to_path_buf(),
            }),
            mtls: None,
        },
        upstream: UpstreamConfig::default(),
    };
    let bearer_verifier = Arc::new(BearerVerifier::load(tokens.path()).expect("bearer verifier"));

    let state = AppState {
        cas: Arc::new(cas),
        config: Arc::new(cfg),
        upstream: None,
        bearer: Some(bearer_verifier.clone()),
    };

    let auth_layer = AuthLayer::new(Some(bearer_verifier), None);

    let protected = roastery::proto::barista::protected_router()
        .with_state(state.clone())
        .layer(auth_layer);
    let app = axum::Router::new()
        .merge(roastery::proto::barista::public_router().with_state(state.clone()))
        .merge(roastery::ops::router().with_state(state))
        .merge(protected);

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    wait_for_listener(addr, Duration::from_secs(4)).await;

    RoasteryHarness {
        addr,
        _tmp: tmp,
        _tokens_file: Some(tokens),
        server: Some(server),
    }
}

/// Spin a tiny axum mock that always answers any
/// `GET /v1/cas/sha256/{digest}` with `400 BAR-CAS-001`, mimicking
/// a roastery that served bytes the server itself rejected as a
/// digest mismatch. Used to drive the cache's digest-mismatch
/// fall-through.
pub async fn spawn_digest_mismatch_mock() -> RoasteryHarness {
    let tmp = TempDir::new().expect("tempdir");

    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let addr = std_listener.local_addr().expect("addr");
    let listener = TcpListener::from_std(std_listener).expect("from std");

    let app: Router = Router::new().route(
        "/v1/cas/sha256/{digest}",
        get(|_: AxPath<String>| async {
            let body = serde_json::json!({
                "code": "BAR-CAS-001",
                "message": "digest mismatch: bytes did not hash to the requested digest",
                "expected": "0000000000000000000000000000000000000000000000000000000000000000",
                "actual": "1111111111111111111111111111111111111111111111111111111111111111",
            });
            (StatusCode::BAD_REQUEST, axum::Json(body))
        }),
    );

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    wait_for_listener(addr, Duration::from_secs(4)).await;

    RoasteryHarness {
        addr,
        _tmp: tmp,
        _tokens_file: None,
        server: Some(server),
    }
}

/// Spin a tiny axum mock that 404s every blob GET (so the cache's
/// read path falls through to upstream) but answers every blob PUT
/// with `500 Internal Server Error`. Used to exercise the
/// push-after-build failure path: the read succeeds via upstream, the
/// subsequent push fails, and the failure must NOT fail the fetch.
pub async fn spawn_put_failing_roastery() -> RoasteryHarness {
    let tmp = TempDir::new().expect("tempdir");

    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let addr = std_listener.local_addr().expect("addr");
    let listener = TcpListener::from_std(std_listener).expect("from std");

    let app: Router = Router::new().route(
        "/v1/cas/sha256/{digest}",
        get(|_: AxPath<String>| async { StatusCode::NOT_FOUND }).put(
            |_: AxPath<String>| async {
                let body = serde_json::json!({
                    "code": "BAR-INTERNAL",
                    "message": "synthetic PUT failure for push-after-build test",
                });
                (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(body))
            },
        ),
    );

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    wait_for_listener(addr, Duration::from_secs(4)).await;

    RoasteryHarness {
        addr,
        _tmp: tmp,
        _tokens_file: None,
        server: Some(server),
    }
}

/// Spin a tiny axum mock that answers every blob GET with `200 OK`
/// carrying **attacker-controlled bytes** while echoing the
/// *requested* digest in `X-Barista-Digest` (so the client's
/// header cross-check in `parse_blob_stat` is satisfied). This models
/// a malicious — or simply buggy — roastery that bypasses its own
/// server-side PUT verification and serves bytes that do NOT hash to
/// the digest the client asked for.
///
/// It is the harder cousin of [`spawn_digest_mismatch_mock`]: that
/// fixture has the server *honestly reject* with `400 BAR-CAS-001`;
/// this one *lies* and returns a 200 with poison. The cache must
/// still catch it client-side (local sidecar re-verify +
/// `cas_hash == asked` assertion) and refuse to persist the poison.
///
/// `poison` is the body served for every blob GET regardless of the
/// requested digest.
pub async fn spawn_lying_roastery(poison: Vec<u8>) -> RoasteryHarness {
    let tmp = TempDir::new().expect("tempdir");

    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let addr = std_listener.local_addr().expect("addr");
    let listener = TcpListener::from_std(std_listener).expect("from std");

    let poison = Arc::new(poison);
    let app: Router = Router::new().route(
        "/v1/cas/sha256/{digest}",
        get({
            let poison = Arc::clone(&poison);
            move |AxPath(digest): AxPath<String>| {
                let poison = Arc::clone(&poison);
                async move {
                    // Echo back the *requested* digest so the
                    // client-side `X-Barista-Digest` header check
                    // passes; the body is poison that does NOT hash
                    // to it.
                    let hex = digest.strip_prefix("sha256:").unwrap_or(&digest);
                    let mut headers = axum::http::HeaderMap::new();
                    headers.insert(
                        "X-Barista-Digest",
                        axum::http::HeaderValue::from_str(&format!("sha256:{hex}"))
                            .expect("digest header"),
                    );
                    headers.insert(
                        axum::http::header::CONTENT_LENGTH,
                        axum::http::HeaderValue::from_str(&poison.len().to_string())
                            .expect("len header"),
                    );
                    (StatusCode::OK, headers, (*poison).clone())
                }
            }
        }),
    );

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    wait_for_listener(addr, Duration::from_secs(4)).await;

    RoasteryHarness {
        addr,
        _tmp: tmp,
        _tokens_file: None,
        server: Some(server),
    }
}

/// Find a TCP port that is (momentarily) free, then return its
/// address WITHOUT binding anything to it. Used by the
/// "roastery unreachable" test — nothing ever listens here, so the
/// client gets connection-refused.
pub fn free_port_addr() -> SocketAddr {
    let probe = StdTcpListener::bind("127.0.0.1:0").expect("probe bind");
    let addr = probe.local_addr().expect("addr");
    drop(probe);
    addr
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
