// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-process server fixtures for the client integration tests.
//!
//! Three shapes:
//!
//! - [`spawn_plain_server`] — plain HTTP, no auth. Used by the
//!   round-trip / batch / streaming tests.
//! - [`spawn_bearer_server`] — plain HTTP, bearer-auth required.
//!   Used by the auth-success / auth-failure tests.
//! - [`spawn_mtls_server`] — HTTPS + mTLS. Used by the TLS tests.
//!
//! Each spawn fn returns a `Harness` owning the temp dir, the
//! tokens file (if any), and the server task. Drop aborts the
//! task.

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use roastery::{
    AppState, AuthConfig as RoasteryAuthConfig, AuthLayer, BearerAuthConfig, BearerVerifier, FsCas,
    MtlsAuthConfig, ServerConfig, StorageBackend, TlsConfig as RoasteryTlsConfig, UpstreamConfig,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use super::certs::{TestPki, build_pki, ensure_crypto_provider};

pub struct Harness {
    pub addr: SocketAddr,
    pub scheme: &'static str,
    pub pki: Option<TestPki>,
    pub _tmp: TempDir,
    pub _tokens_file: Option<NamedTempFile>,
    pub server_plain: Option<JoinHandle<()>>,
    pub server_tls: Option<JoinHandle<roastery::Result<()>>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(h) = self.server_plain.take() {
            h.abort();
        }
        if let Some(h) = self.server_tls.take() {
            h.abort();
        }
    }
}

impl Harness {
    pub fn base_url(&self) -> String {
        // For TLS we always go through 127.0.0.1 — the test cert SAN
        // covers both `localhost` and 127.0.0.1; numeric host
        // sidesteps DNS in test runners.
        format!("{}://127.0.0.1:{}", self.scheme, self.addr.port())
    }
}

/// Spin a plain-HTTP roastery on an ephemeral port with no auth.
pub async fn spawn_plain_server() -> Harness {
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

    Harness {
        addr,
        scheme: "http",
        pki: None,
        _tmp: tmp,
        _tokens_file: None,
        server_plain: Some(server),
        server_tls: None,
    }
}

/// Spin a plain-HTTP roastery on an ephemeral port with bearer auth
/// required. The tokens file contains a single `ci:s3cret` entry —
/// the password the client should send is `s3cret`.
pub async fn spawn_bearer_server() -> Harness {
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
    let state = AppState {
        cas: Arc::new(cas),
        config: Arc::new(cfg),
        upstream: None,
        bearer: None,
    };

    let bearer_verifier = Arc::new(BearerVerifier::load(tokens.path()).expect("bearer verifier"));
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

    Harness {
        addr,
        scheme: "http",
        pki: None,
        _tmp: tmp,
        _tokens_file: Some(tokens),
        server_plain: Some(server),
        server_tls: None,
    }
}

/// Spin an HTTPS + mTLS roastery on an ephemeral port. The
/// returned `Harness` carries the test PKI so client-side
/// configuration can pull the CA + client cert/key out.
pub async fn spawn_mtls_server() -> Harness {
    ensure_crypto_provider();
    let pki = build_pki("localhost");

    let probe = StdTcpListener::bind("127.0.0.1:0").expect("probe bind");
    let addr = probe.local_addr().expect("addr");
    drop(probe);

    let tmp = TempDir::new().expect("tempdir");
    let storage_dir = tmp.path().to_path_buf();

    let cfg = ServerConfig {
        bind: addr,
        storage: StorageBackend::Filesystem(storage_dir.clone()),
        storage_dir,
        tls: Some(RoasteryTlsConfig {
            cert_path: pki.server_cert_file.clone(),
            key_path: pki.server_key_file.clone(),
        }),
        auth: RoasteryAuthConfig {
            bearer: None,
            mtls: Some(MtlsAuthConfig {
                ca_cert: pki.ca_pem_file.clone(),
            }),
        },
        upstream: UpstreamConfig::default(),
    };

    let server = tokio::spawn(async move { roastery::run(cfg).await });
    wait_for_listener(addr, Duration::from_secs(10)).await;

    Harness {
        addr,
        scheme: "https",
        pki: Some(pki),
        _tmp: tmp,
        _tokens_file: None,
        server_plain: None,
        server_tls: Some(server),
    }
}

/// Spin a plain-HTTP test fixture that includes a `/slow` route
/// whose handler sleeps before responding. The route is mounted in
/// place of the real CAS surface; the client points at
/// `/v1/cas/sha256/<digest>` and gets a slow body back. Used by the
/// timeout test.
pub async fn spawn_slow_server(sleep_for: Duration) -> Harness {
    use axum::extract::Path;
    use axum::routing::get;

    let tmp = TempDir::new().expect("tempdir");

    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let addr = std_listener.local_addr().expect("addr");
    let listener = TcpListener::from_std(std_listener).expect("from std");

    let app = axum::Router::new().route(
        "/v1/cas/sha256/{digest}",
        get(move |_path: Path<String>| async move {
            tokio::time::sleep(sleep_for).await;
            "would have returned a body"
        }),
    );

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    wait_for_listener(addr, Duration::from_secs(4)).await;

    Harness {
        addr,
        scheme: "http",
        pki: None,
        _tmp: tmp,
        _tokens_file: None,
        server_plain: Some(server),
        server_tls: None,
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
