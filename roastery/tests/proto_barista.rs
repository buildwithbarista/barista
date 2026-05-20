//! Integration tests for the barista-protocol HTTP/2 surface.
//!
//! Each test spawns a real `roastery::run` instance on an ephemeral
//! port over plain TCP (HTTP/1.1 — the codepath is the same one
//! HTTP/2 will hit once TLS + ALPN land in a later task) and drives
//! it with `reqwest`. A fresh `TempDir`-backed `FsCas` is used per
//! test so the cases stay independent and parallel-safe.
//!
//! The tests cover the full `[T]` proof set for the barista-protocol
//! task: round-trip GET/HEAD/PUT, 404 + 400 error paths, batch
//! presence semantics, batch cap enforcement, health + capabilities
//! shape.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use roastery::{AppState, FsCas, ServerConfig};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::sleep;

// -------------------------------------------------------------------
// Test harness
// -------------------------------------------------------------------

/// Live server fixture: owns the storage temp dir, the server task,
/// and the bound address.
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

/// Spin up a roastery server with a `TempDir`-backed `FsCas` on an
/// OS-assigned ephemeral port. Returns once the listener is accepting
/// connections.
async fn spawn_server() -> Harness {
    let tmp = TempDir::new().unwrap();
    let storage_dir: PathBuf = tmp.path().to_path_buf();

    // Bind to `:0` synchronously to discover an ephemeral port. We
    // hand the listener to `axum::serve` via the same path the binary
    // takes — going through `roastery::run` would re-bind, which on
    // some platforms briefly races with the probe handoff. Building
    // the router by hand here keeps the test deterministic.
    let std_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    let listener = TcpListener::from_std(std_listener).unwrap();

    let cas = FsCas::new(storage_dir.clone()).unwrap();
    let config = ServerConfig::with_bind(addr);
    let state = AppState {
        cas: Arc::new(cas),
        config: Arc::new(config),
        upstream: None,
        bearer: None,
    };

    // Build the public router via the library — same code production
    // serves — but skip `roastery::run`'s graceful-shutdown loop so
    // the test can `abort()` the task on Drop.
    let app =
        axum::Router::new().merge(roastery::proto::barista::router()).with_state(state);

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

/// Wait up to `timeout` for `addr` to accept a TCP connection.
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

/// Compute the canonical lowercase-hex SHA-256 of a byte slice.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    hex::encode(out)
}

/// Build a `reqwest::Client` with a tight timeout — every test runs
/// against a local server, so a long timeout would only hide a
/// regression.
fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

// -------------------------------------------------------------------
// [T] proof set
// -------------------------------------------------------------------

#[tokio::test]
async fn cas_put_then_get_round_trips_bytes() {
    let h = spawn_server().await;
    let c = client();

    // 1 KiB of pseudo-random-but-deterministic bytes.
    let blob: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
    let hex = sha256_hex(&blob);
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), hex);

    let resp = c.put(&url).body(blob.clone()).send().await.unwrap();
    assert_eq!(resp.status(), 201, "PUT status");
    assert_eq!(
        resp.headers().get("x-barista-digest").unwrap(),
        &format!("sha256:{hex}")
    );

    let resp = c.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200, "GET status");
    assert_eq!(
        resp.headers().get("content-length").unwrap(),
        &blob.len().to_string()
    );
    assert_eq!(
        resp.headers().get("x-barista-digest").unwrap(),
        &format!("sha256:{hex}")
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), blob.as_slice(), "GET body byte-equal to PUT");
}

#[tokio::test]
async fn cas_head_returns_metadata_without_body() {
    let h = spawn_server().await;
    let c = client();

    let blob = b"head test payload".to_vec();
    let hex = sha256_hex(&blob);
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), hex);

    let resp = c.put(&url).body(blob.clone()).send().await.unwrap();
    assert_eq!(resp.status(), 201);

    let resp = c.head(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-length").unwrap(),
        &blob.len().to_string()
    );
    assert_eq!(
        resp.headers().get("x-barista-digest").unwrap(),
        &format!("sha256:{hex}")
    );
    let body = resp.bytes().await.unwrap();
    assert!(body.is_empty(), "HEAD response must have empty body");
}

#[tokio::test]
async fn cas_get_returns_404_for_absent() {
    let h = spawn_server().await;
    let c = client();
    let hex = sha256_hex(b"never written get");
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), hex);
    let resp = c.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn cas_head_returns_404_for_absent() {
    let h = spawn_server().await;
    let c = client();
    let hex = sha256_hex(b"never written head");
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), hex);
    let resp = c.head(&url).send().await.unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn cas_get_returns_400_for_malformed_digest() {
    let h = spawn_server().await;
    let c = client();
    let url = format!("{}/v1/cas/sha256/not-a-hex-digest", h.base_url());
    let resp = c.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 400);
    // Body should be the structured `BAR-CAS-002` error.
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BAR-CAS-002");
}

#[tokio::test]
async fn cas_put_rejects_digest_mismatch() {
    let h = spawn_server().await;
    let c = client();

    let real_bytes = b"the real bytes".to_vec();
    let bogus_hex = sha256_hex(b"some other bytes");
    let actual_hex = sha256_hex(&real_bytes);
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), bogus_hex);

    let resp = c.put(&url).body(real_bytes).send().await.unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BAR-CAS-001");
    assert_eq!(body["expected"], bogus_hex);
    assert_eq!(body["actual"], actual_hex);
}

#[tokio::test]
async fn cas_put_is_idempotent_for_existing_blob() {
    let h = spawn_server().await;
    let c = client();

    let blob = b"idempotent payload".to_vec();
    let hex = sha256_hex(&blob);
    let url = format!("{}/v1/cas/sha256/{}", h.base_url(), hex);

    let r1 = c.put(&url).body(blob.clone()).send().await.unwrap();
    assert_eq!(r1.status(), 201);
    let r2 = c.put(&url).body(blob.clone()).send().await.unwrap();
    assert_eq!(r2.status(), 201);

    // Single final blob: HEAD returns the right Content-Length once.
    let r3 = c.head(&url).send().await.unwrap();
    assert_eq!(r3.status(), 200);
    assert_eq!(
        r3.headers().get("content-length").unwrap(),
        &blob.len().to_string()
    );
}

#[tokio::test]
async fn cas_missing_returns_only_absent_digests() {
    let h = spawn_server().await;
    let c = client();

    // PUT two blobs.
    let a = b"alpha".to_vec();
    let b = b"beta".to_vec();
    let absent = b"never written missing".to_vec();
    let a_hex = sha256_hex(&a);
    let b_hex = sha256_hex(&b);
    let absent_hex = sha256_hex(&absent);

    for (hex, bytes) in [(&a_hex, &a), (&b_hex, &b)] {
        let url = format!("{}/v1/cas/sha256/{}", h.base_url(), hex);
        let resp = c.put(&url).body(bytes.clone()).send().await.unwrap();
        assert_eq!(resp.status(), 201);
    }

    let req = serde_json::json!({
        "digests": [
            format!("sha256:{a_hex}"),
            format!("sha256:{b_hex}"),
            format!("sha256:{absent_hex}"),
        ]
    });
    let resp = c
        .post(format!("{}/v1/cas/missing", h.base_url()))
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let missing = body["missing"].as_array().unwrap();
    assert_eq!(missing.len(), 1, "exactly one missing entry");
    assert_eq!(missing[0], format!("sha256:{absent_hex}"));
}

#[tokio::test]
async fn cas_missing_accepts_bare_hex_and_prefixed() {
    let h = spawn_server().await;
    let c = client();

    let absent_hex = sha256_hex(b"absent both formats");
    let other_absent_hex = sha256_hex(b"other absent both formats");

    let req = serde_json::json!({
        "digests": [
            absent_hex.clone(),                       // bare hex
            format!("sha256:{other_absent_hex}"),     // prefixed
        ]
    });
    let resp = c
        .post(format!("{}/v1/cas/missing", h.base_url()))
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let missing: Vec<String> = body["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(missing.len(), 2);
    // Response always normalises to the `sha256:` prefix.
    assert!(missing.iter().all(|s| s.starts_with("sha256:")));
    assert!(missing.contains(&format!("sha256:{absent_hex}")));
    assert!(missing.contains(&format!("sha256:{other_absent_hex}")));
}

#[tokio::test]
async fn cas_missing_rejects_malformed_entries() {
    let h = spawn_server().await;
    let c = client();
    let good = sha256_hex(b"ok");
    let req = serde_json::json!({
        "digests": [good, "garbage-not-a-digest"]
    });
    let resp = c
        .post(format!("{}/v1/cas/missing", h.base_url()))
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BAR-CAS-002");
}

#[tokio::test]
async fn cas_missing_enforces_batch_cap() {
    let h = spawn_server().await;
    let c = client();
    let digest_hex = sha256_hex(b"cap test");

    // 1001 entries — one past the documented cap of 1000.
    let digests: Vec<String> = (0..1001).map(|_| digest_hex.clone()).collect();
    let req = serde_json::json!({ "digests": digests });

    let resp = c
        .post(format!("{}/v1/cas/missing", h.base_url()))
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BAR-CAS-004");
}

#[tokio::test]
async fn health_endpoint_responds() {
    let h = spawn_server().await;
    let c = client();
    let resp = c
        .get(format!("{}/v1/health", h.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["protocol"], "barista");
    assert_eq!(body["version"], "v1");
}

#[tokio::test]
async fn capabilities_endpoint_reflects_config() {
    let h = spawn_server().await;
    let c = client();
    let resp = c
        .get(format!("{}/v1/capabilities", h.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["protocol"], "barista");
    assert_eq!(body["version"], "v1");
    // The default test fixture uses the filesystem backend.
    assert_eq!(body["storage"]["backend"], "filesystem");
    // SHA-256 is the only digest function v0.1 commits to.
    let hashes = body["cas"]["hashes"].as_array().unwrap();
    assert_eq!(hashes.len(), 1);
    assert_eq!(hashes[0], "sha256");
    // Cap is the documented per-call limit.
    assert_eq!(body["cas"]["max_batch_missing"], 1000);
}
