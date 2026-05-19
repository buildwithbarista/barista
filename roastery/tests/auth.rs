//! Integration tests for the auth surface: bearer-token validation,
//! mTLS client-cert validation, the fail-closed startup check, and
//! the public-route exceptions for ops + protocol-level
//! capabilities.
//!
//! The tests fall into three buckets:
//!
//! 1. **Bearer tests** — drive a plain-HTTP server with a tokens
//!    file. Exercise the 401 path, the happy path, and the public-
//!    route bypass.
//! 2. **mTLS tests** — drive an HTTPS server with an ephemeral CA;
//!    present a chained client cert, no cert, or a cert signed by
//!    a different CA, and assert the handshake outcome.
//! 3. **Config validation tests** — exercise `ServerConfig::validate`
//!    directly, without spinning up a listener.
//!
//! Bucket 2 has the most moving parts: a one-shot rcgen CA + cert
//! mint, a reqwest client with a custom rustls config, and the
//! roastery's own rustls-via-axum-server listener.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use roastery::{
    AppState, AuthConfig, BearerAuthConfig, FsCas, MtlsAuthConfig, ServerConfig, TlsConfig,
    UpstreamConfig,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tempfile::{NamedTempFile, TempDir};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use common::certs::{build_pki, ensure_crypto_provider, TestPki};

// ---------------------------------------------------------------------
// Bearer-only harness (plain HTTP)
// ---------------------------------------------------------------------

/// Live server fixture for the bearer tests.
struct BearerHarness {
    addr: SocketAddr,
    _tmp: TempDir,
    _tokens_file: NamedTempFile,
    server: Option<JoinHandle<()>>,
}

impl Drop for BearerHarness {
    fn drop(&mut self) {
        if let Some(h) = self.server.take() {
            h.abort();
        }
    }
}

impl BearerHarness {
    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// Spin up a roastery instance over plain HTTP with a bearer-tokens
/// file containing a single `ci:s3cret` entry. The server is built
/// against `roastery::server::build_router` indirectly through the
/// `proto::barista::{public_router,protected_router}` constructors,
/// wrapped with an [`AuthLayer`] just like production assembly
/// does.
async fn spawn_bearer_server() -> BearerHarness {
    let tmp = TempDir::new().unwrap();
    let storage_dir: PathBuf = tmp.path().to_path_buf();

    // Tokens file with one entry.
    let mut tokens = NamedTempFile::new().unwrap();
    use std::io::Write;
    writeln!(tokens, "ci:s3cret").unwrap();

    // Bind synchronously to discover an ephemeral port.
    let std_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    let listener = TcpListener::from_std(std_listener).unwrap();

    let cas = FsCas::new(storage_dir.clone()).unwrap();
    let cfg = ServerConfig {
        bind: addr,
        storage: roastery::StorageBackend::Filesystem(storage_dir.clone()),
        storage_dir,
        tls: None,
        auth: AuthConfig {
            bearer: Some(BearerAuthConfig {
                tokens_file: tokens.path().to_path_buf(),
            }),
            mtls: None,
        },
        upstream: UpstreamConfig::default(),
    };
    let state = AppState {
        cas: Arc::new(cas),
        config: Arc::new(cfg.clone()),
        upstream: None,
    };

    // Build the same router topology production builds, by going
    // through the public + protected sub-router constructors.
    let bearer_verifier =
        Arc::new(roastery::BearerVerifier::load(tokens.path()).unwrap());
    let auth_layer = roastery::AuthLayer::new(Some(bearer_verifier), None);

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

    BearerHarness {
        addr,
        _tmp: tmp,
        _tokens_file: tokens,
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

fn http_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

/// A digest that's syntactically valid but unlikely to exist in
/// store. All bearer tests use this to probe `/v1/cas/sha256/{digest}`
/// — auth is what's under test, not the storage round-trip.
fn arbitrary_digest_hex() -> String {
    "0".repeat(64)
}

// ---------------------------------------------------------------------
// Bearer [T] tests
// ---------------------------------------------------------------------

/// `[T]` #1 — `unauthenticated_request_rejected_when_bearer_required`.
#[tokio::test]
async fn unauthenticated_request_rejected_when_bearer_required() {
    let h = spawn_bearer_server().await;
    let c = http_client();
    let url = format!(
        "{}/v1/cas/sha256/{}",
        h.base_url(),
        arbitrary_digest_hex()
    );
    let resp = c.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BAR-AUTH-001");
    assert_eq!(body["message"], "unauthorized");
}

/// `[T]` #2 — `valid_bearer_token_allowed`.
#[tokio::test]
async fn valid_bearer_token_allowed() {
    let h = spawn_bearer_server().await;
    let c = http_client();
    let url = format!(
        "{}/v1/cas/sha256/{}",
        h.base_url(),
        arbitrary_digest_hex()
    );
    let resp = c
        .get(&url)
        .header("Authorization", "Bearer s3cret")
        .send()
        .await
        .unwrap();
    // 404 is the success indicator for the auth surface: the
    // request reached the handler (storage said "absent"). 200 is
    // also acceptable if a fixture pre-populated the blob.
    assert!(
        resp.status() == 200 || resp.status() == 404,
        "expected 200 or 404 (auth-passed), got {}",
        resp.status()
    );
}

/// `[T]` #3 — `wrong_bearer_token_rejected`.
#[tokio::test]
async fn wrong_bearer_token_rejected() {
    let h = spawn_bearer_server().await;
    let c = http_client();
    let url = format!(
        "{}/v1/cas/sha256/{}",
        h.base_url(),
        arbitrary_digest_hex()
    );
    let resp = c
        .get(&url)
        .header("Authorization", "Bearer wrong-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BAR-AUTH-001");
}

/// `[T]` #4 — `malformed_authorization_header_rejected`.
#[tokio::test]
async fn malformed_authorization_header_rejected() {
    let h = spawn_bearer_server().await;
    let c = http_client();
    let url = format!(
        "{}/v1/cas/sha256/{}",
        h.base_url(),
        arbitrary_digest_hex()
    );
    let resp = c
        .get(&url)
        .header("Authorization", "NotBearer foo")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

/// `[T]` #5 — `ops_endpoints_remain_public_even_with_bearer_required`.
#[tokio::test]
async fn ops_endpoints_remain_public_even_with_bearer_required() {
    let h = spawn_bearer_server().await;
    let c = http_client();
    for path in ["/healthz", "/metrics", "/version"] {
        let resp = c
            .get(format!("{}{}", h.base_url(), path))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "ops endpoint {path} should be public, got {}",
            resp.status()
        );
    }
}

/// `[T]` #6 — `protocol_health_and_capabilities_remain_public`.
#[tokio::test]
async fn protocol_health_and_capabilities_remain_public() {
    let h = spawn_bearer_server().await;
    let c = http_client();
    for path in ["/v1/health", "/v1/capabilities"] {
        let resp = c
            .get(format!("{}{}", h.base_url(), path))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "protocol endpoint {path} should be public, got {}",
            resp.status()
        );
    }
}

// ---------------------------------------------------------------------
// `[T]` #10 — `ServerConfig::validate` bind/auth interaction.
// ---------------------------------------------------------------------

#[test]
fn server_refuses_to_start_with_non_loopback_bind_and_no_auth() {
    let tmp = TempDir::new().unwrap();
    let storage = tmp.path().to_path_buf();
    let cfg = ServerConfig {
        bind: "0.0.0.0:8443".parse().unwrap(),
        storage: roastery::StorageBackend::Filesystem(storage.clone()),
        storage_dir: storage,
        tls: None,
        auth: AuthConfig::default(),
        upstream: UpstreamConfig::default(),
    };
    let err = cfg.validate().expect_err("validate should reject non-loopback + no-auth");
    let msg = format!("{err}");
    assert!(
        msg.contains("BAR-AUTH-005"),
        "expected BAR-AUTH-005 in error: {msg}"
    );

    // Loopback bind with no auth must validate.
    let tmp = TempDir::new().unwrap();
    let storage = tmp.path().to_path_buf();
    let cfg_ok = ServerConfig {
        bind: "127.0.0.1:8443".parse().unwrap(),
        storage: roastery::StorageBackend::Filesystem(storage.clone()),
        storage_dir: storage,
        tls: None,
        auth: AuthConfig::default(),
        upstream: UpstreamConfig::default(),
    };
    cfg_ok.validate().expect("loopback + no-auth should validate");
}

// ---------------------------------------------------------------------
// mTLS harness (HTTPS)
// ---------------------------------------------------------------------

struct MtlsHarness {
    addr: SocketAddr,
    pki: TestPki,
    bearer_token: Option<String>,
    _tmp: TempDir,
    _tokens_file: Option<NamedTempFile>,
    server: Option<JoinHandle<roastery::Result<()>>>,
}

impl Drop for MtlsHarness {
    fn drop(&mut self) {
        if let Some(h) = self.server.take() {
            h.abort();
        }
    }
}

impl MtlsHarness {
    fn base_url(&self) -> String {
        // SAN includes both `localhost` and 127.0.0.1; using the
        // numeric form sidesteps DNS in the test runner.
        format!("https://127.0.0.1:{}", self.addr.port())
    }
}

/// Spin up an HTTPS roastery with TLS + mTLS configured. If
/// `with_bearer` is true, also configure a bearer-tokens file so
/// the test can verify "either mechanism suffices".
///
/// Bind goes through `0.0.0.0:0` so the OS picks an ephemeral
/// port; we look it up via `local_addr` afterwards.
async fn spawn_mtls_server(with_bearer: bool) -> MtlsHarness {
    ensure_crypto_provider();
    let pki = build_pki("localhost");

    // Bind on an ephemeral port through std (so we can read
    // `local_addr` synchronously) and then hand the address to
    // `roastery::run`, which re-binds. We hold onto the std
    // listener only long enough to discover the port — drop it
    // before `run` claims the address.
    let probe = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);

    let tmp = TempDir::new().unwrap();
    let storage_dir = tmp.path().to_path_buf();

    let (tokens_file, bearer_cfg, bearer_token) = if with_bearer {
        let mut f = NamedTempFile::new().unwrap();
        use std::io::Write;
        writeln!(f, "ci:s3cret").unwrap();
        let bearer = BearerAuthConfig {
            tokens_file: f.path().to_path_buf(),
        };
        (Some(f), Some(bearer), Some("s3cret".to_string()))
    } else {
        (None, None, None)
    };

    let cfg = ServerConfig {
        bind: addr,
        storage: roastery::StorageBackend::Filesystem(storage_dir.clone()),
        storage_dir,
        tls: Some(TlsConfig {
            cert_path: pki.server_cert_file.clone(),
            key_path: pki.server_key_file.clone(),
        }),
        auth: AuthConfig {
            bearer: bearer_cfg,
            mtls: Some(MtlsAuthConfig {
                ca_cert: pki.ca_pem_file.clone(),
            }),
        },
        upstream: UpstreamConfig::default(),
    };

    let server = tokio::spawn(async move { roastery::run(cfg).await });
    wait_for_listener(addr, Duration::from_secs(10)).await;

    MtlsHarness {
        addr,
        pki,
        bearer_token,
        _tmp: tmp,
        _tokens_file: tokens_file,
        server: Some(server),
    }
}

/// Build a reqwest client that trusts the test CA, optionally
/// presenting a client identity. `client_pem` is the PEM-encoded
/// client cert (cert chain); `client_key_pem` is the matching key.
fn https_client(
    ca_pem: &str,
    client_identity: Option<(&str, &str)>,
) -> Client {
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut std::io::Cursor::new(ca_pem.as_bytes())) {
        let cert: CertificateDer<'_> = cert.unwrap();
        root_store.add(cert.into_owned()).unwrap();
    }

    let builder = rustls::ClientConfig::builder().with_root_certificates(root_store);

    let cfg = match client_identity {
        Some((cert_pem, key_pem)) => {
            let certs: Vec<CertificateDer<'static>> =
                rustls_pemfile::certs(&mut std::io::Cursor::new(cert_pem.as_bytes()))
                    .map(|c| c.unwrap().into_owned())
                    .collect();
            let key: PrivateKeyDer<'static> =
                rustls_pemfile::private_key(&mut std::io::Cursor::new(key_pem.as_bytes()))
                    .unwrap()
                    .unwrap();
            builder.with_client_auth_cert(certs, key).unwrap()
        }
        None => builder.with_no_client_auth(),
    };

    Client::builder()
        .use_preconfigured_tls(cfg)
        .timeout(Duration::from_secs(10))
        // The test cert covers `localhost` + 127.0.0.1; reqwest's
        // default hostname verification is exactly what we want.
        .resolve("localhost", SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .build()
        .unwrap()
}

fn arbitrary_url(h: &MtlsHarness) -> String {
    format!(
        "{}/v1/cas/sha256/{}",
        h.base_url(),
        arbitrary_digest_hex()
    )
}

// ---------------------------------------------------------------------
// mTLS [T] tests
// ---------------------------------------------------------------------

/// `[T]` #7 — `mtls_client_with_valid_cert_allowed`.
#[tokio::test]
async fn mtls_client_with_valid_cert_allowed() {
    let h = spawn_mtls_server(false).await;
    let c = https_client(
        &h.pki.ca_pem,
        Some((&h.pki.client_cert_pem, &h.pki.client_key_pem)),
    );
    let url = arbitrary_url(&h);
    let resp = c.get(&url).send().await.unwrap();
    assert!(
        resp.status() == 200 || resp.status() == 404,
        "valid mTLS client should auth, got {}",
        resp.status()
    );
}

/// `[T]` #8 — `mtls_client_without_cert_rejected`.
///
/// When the server requires a client cert, a client offering none
/// should fail the TLS handshake. `reqwest` surfaces that as an
/// error result, not as an HTTP status.
#[tokio::test]
async fn mtls_client_without_cert_rejected() {
    let h = spawn_mtls_server(false).await;
    let c = https_client(&h.pki.ca_pem, None);
    let url = arbitrary_url(&h);
    let result = c.get(&url).send().await;
    assert!(
        result.is_err(),
        "client without cert should fail the handshake; got {:?}",
        result.ok().map(|r| r.status())
    );
}

/// `[T]` #9 — `mtls_client_with_unrelated_ca_cert_rejected`.
#[tokio::test]
async fn mtls_client_with_unrelated_ca_cert_rejected() {
    let h = spawn_mtls_server(false).await;
    let c = https_client(
        &h.pki.ca_pem,
        Some((
            &h.pki.unrelated_client_cert_pem,
            &h.pki.unrelated_client_key_pem,
        )),
    );
    let url = arbitrary_url(&h);
    let result = c.get(&url).send().await;
    assert!(
        result.is_err(),
        "client cert from a different CA should fail the handshake; got {:?}",
        result.ok().map(|r| r.status())
    );
}

/// `[T]` #11 — `bearer_and_mtls_both_configured_either_suffices`.
///
/// Drives a single HTTPS+mTLS+bearer server and probes it two ways:
///
/// 1. A client with a valid client cert but no bearer header.
/// 2. A client without a client cert but with a valid bearer token.
///
/// Wait — point (2) can't work as written. When mTLS is configured
/// on the server, the TLS handshake itself demands a client cert.
/// To exercise "bearer alone suffices" we'd need a separate non-
/// mTLS TLS endpoint, which would complicate the server topology
/// significantly.
///
/// In practice the way "either mechanism suffices" gets exercised
/// in mixed deployments is: bearer over a TLS listener with
/// `with_no_client_auth`, and mTLS over a separate TLS listener
/// that requires client certs. The roastery v0.1 surface runs one
/// listener so we test the part the layer enforces: with both
/// configured + a client presenting a valid client cert, the
/// request succeeds regardless of whether a bearer is also sent.
#[tokio::test]
async fn bearer_and_mtls_both_configured_either_suffices() {
    let h = spawn_mtls_server(true).await;

    // Path A: valid client cert, no bearer header.  Auth layer
    // sees mTLS extension, accepts.
    let c = https_client(
        &h.pki.ca_pem,
        Some((&h.pki.client_cert_pem, &h.pki.client_key_pem)),
    );
    let url = arbitrary_url(&h);
    let resp = c.get(&url).send().await.unwrap();
    assert!(
        resp.status() == 200 || resp.status() == 404,
        "mTLS-only auth path should succeed; got {}",
        resp.status()
    );

    // Path B: valid client cert AND a valid bearer header. Both
    // mechanisms accept; the layer's decision order picks bearer
    // first (a fall-through to mTLS would only trigger on
    // bearer-mismatch). Either way, accept → 200/404.
    let token = h.bearer_token.clone().unwrap();
    let resp = c
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == 200 || resp.status() == 404,
        "bearer + mTLS together should succeed; got {}",
        resp.status()
    );

    // Path C: valid client cert + WRONG bearer header. Bearer
    // mismatch is the first signal; the layer must fall through to
    // mTLS rather than emitting 401. The mTLS extension is present,
    // so the request still succeeds.
    let resp = c
        .get(&url)
        .header("Authorization", "Bearer wrong-token")
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == 200 || resp.status() == 404,
        "bearer mismatch + valid mTLS should fall through to mTLS; got {}",
        resp.status()
    );
}
