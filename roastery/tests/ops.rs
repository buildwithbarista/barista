//! Integration tests for the ops endpoints (`/healthz`, `/metrics`,
//! `/version`).
//!
//! Each test spawns a live `roastery` server on an ephemeral port,
//! issues a real HTTP request via `reqwest`, and asserts on the
//! status / headers / body. The CAS-traffic tests also drive
//! `PUT` + `GET` against the `/v1/cas/sha256/{digest}` endpoint so
//! the request-counter increment is exercised against real handlers
//! rather than via direct `record_cas_request` calls.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use roastery::config::StorageBackend;
use roastery::{ServerConfig, UpstreamConfig, run};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::time::sleep;

/// Reserve an OS-assigned localhost port and release it. Racy, but
/// fine for a single-threaded test — the kernel is unlikely to hand
/// the same port to anyone else in the few milliseconds between drop
/// and re-bind. The existing smoke tests use the same trick.
fn pick_free_port() -> SocketAddr {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    addr
}

/// Wait up to `timeout` for `addr` to accept a TCP connection. Panics
/// on timeout — a server we just spawned but can't connect to is a
/// test bug.
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

/// Fixture: build a `ServerConfig` pointed at a fresh `TempDir`
/// filesystem CAS, bound to the given socket. Returns the `TempDir`
/// so the caller can hold it for the duration of the test (drop ⇒
/// directory removed).
fn fixture_config(addr: SocketAddr) -> (TempDir, ServerConfig) {
    let tmp = TempDir::new().expect("tempdir");
    let storage_dir: PathBuf = tmp.path().to_path_buf();
    let cfg = ServerConfig {
        bind: addr,
        storage: StorageBackend::Filesystem(storage_dir.clone()),
        storage_dir,
        tls: None,
        auth: roastery::AuthConfig::default(),
        upstream: UpstreamConfig::default(),
    };
    (tmp, cfg)
}

/// Spawn a server with the supplied config on the supplied address.
/// Returns the `JoinHandle` so the caller can abort it on teardown.
async fn spawn_server(addr: SocketAddr, cfg: ServerConfig) -> tokio::task::JoinHandle<roastery::Result<()>> {
    let handle = tokio::spawn(async move { run(cfg).await });
    wait_for_listener(addr, Duration::from_secs(4)).await;
    handle
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("build client")
}

// ----------------------------------------------------------------------
// /healthz
// ----------------------------------------------------------------------

#[tokio::test]
async fn healthz_returns_200_ok() {
    let addr = pick_free_port();
    let (_tmp, cfg) = fixture_config(addr);
    let server = spawn_server(addr, cfg).await;

    let client = http_client();
    let url = format!("http://{addr}/healthz");
    let resp = client.get(&url).send().await.expect("GET /healthz");
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.starts_with("text/plain"),
        "expected text/plain, got {ct:?}"
    );
    let body = resp.text().await.expect("body");
    assert_eq!(body, "ok\n");

    server.abort();
    let _ = server.await;
}

// ----------------------------------------------------------------------
// /metrics
// ----------------------------------------------------------------------

#[tokio::test]
async fn metrics_returns_200_with_prometheus_content_type() {
    let addr = pick_free_port();
    let (_tmp, cfg) = fixture_config(addr);
    let server = spawn_server(addr, cfg).await;

    let client = http_client();
    let url = format!("http://{addr}/metrics");
    let resp = client.get(&url).send().await.expect("GET /metrics");
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.starts_with("text/plain; version=0.0.4"),
        "expected Prometheus exposition content-type, got {ct:?}"
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn metrics_includes_build_info_gauge() {
    let addr = pick_free_port();
    let (_tmp, cfg) = fixture_config(addr);
    let server = spawn_server(addr, cfg).await;

    let client = http_client();
    let url = format!("http://{addr}/metrics");
    let body = client
        .get(&url)
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("text");

    assert!(
        body.lines().any(|line| {
            line.starts_with("roastery_build_info{")
                && line.contains("version=")
                && line.contains("rustc=")
                && line.trim_end().ends_with(" 1")
        }),
        "metrics body did not contain a roastery_build_info{{…}} 1 line. Body:\n{body}"
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn metrics_includes_uptime_seconds_gauge() {
    let addr = pick_free_port();
    let (_tmp, cfg) = fixture_config(addr);
    let server = spawn_server(addr, cfg).await;

    let client = http_client();
    let url = format!("http://{addr}/metrics");
    let body = client
        .get(&url)
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("text");

    // Find the value line (skip the `# HELP` / `# TYPE` comments).
    let value_line = body
        .lines()
        .find(|line| {
            line.starts_with("roastery_uptime_seconds")
                && !line.starts_with("# ")
                && !line.starts_with("roastery_uptime_seconds_")
        })
        .unwrap_or_else(|| panic!("no roastery_uptime_seconds value line in:\n{body}"));

    // Line shape: `roastery_uptime_seconds <float>`.
    let parts: Vec<&str> = value_line.split_whitespace().collect();
    assert_eq!(parts.len(), 2, "unexpected line shape: {value_line:?}");
    let value: f64 = parts[1].parse().expect("uptime not a float");
    assert!(value >= 0.0, "uptime was negative: {value}");

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn metrics_reflects_cas_traffic() {
    let addr = pick_free_port();
    let (_tmp, cfg) = fixture_config(addr);
    let server = spawn_server(addr, cfg).await;

    let client = http_client();

    // PUT a small blob.
    let blob = b"hello, ops endpoints";
    let digest = {
        let mut hasher = Sha256::new();
        hasher.update(blob);
        hex::encode(hasher.finalize())
    };
    let put_url = format!("http://{addr}/v1/cas/sha256/{digest}");
    let resp = client
        .put(&put_url)
        .body(blob.to_vec())
        .send()
        .await
        .expect("PUT");
    assert_eq!(resp.status(), 201, "PUT failed: {}", resp.status());

    // GET it back.
    let get_url = format!("http://{addr}/v1/cas/sha256/{digest}");
    let resp = client.get(&get_url).send().await.expect("GET");
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("drain body");

    // Now scrape /metrics and look for the counter lines.
    let metrics_url = format!("http://{addr}/metrics");
    let body = client
        .get(&metrics_url)
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("text");

    let put_count = scrape_counter(
        &body,
        "roastery_cas_requests_total",
        &[("method", "put"), ("result", "hit")],
    );
    assert!(
        put_count >= 1,
        "expected at least one put/hit, got {put_count}. Body:\n{body}"
    );

    let get_count = scrape_counter(
        &body,
        "roastery_cas_requests_total",
        &[("method", "get"), ("result", "hit")],
    );
    assert!(
        get_count >= 1,
        "expected at least one get/hit, got {get_count}. Body:\n{body}"
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn metrics_storage_bytes_reflects_filesystem_backend() {
    let addr = pick_free_port();
    let (_tmp, cfg) = fixture_config(addr);
    let server = spawn_server(addr, cfg).await;

    let client = http_client();

    // PUT a known-size blob.
    let blob: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    let blob_len = blob.len();
    let digest = {
        let mut hasher = Sha256::new();
        hasher.update(&blob);
        hex::encode(hasher.finalize())
    };
    let put_url = format!("http://{addr}/v1/cas/sha256/{digest}");
    let resp = client
        .put(&put_url)
        .body(blob)
        .send()
        .await
        .expect("PUT");
    assert_eq!(resp.status(), 201);

    // Bypass the 5-second TTL via the test hook so we don't have to
    // sleep through it.
    roastery::ops::metrics::reset_storage_bytes_cache_for_tests();

    let metrics_url = format!("http://{addr}/metrics");
    let body = client
        .get(&metrics_url)
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("text");

    let bytes = scrape_gauge(
        &body,
        "roastery_storage_bytes_total",
        &[("backend", "filesystem")],
    );
    let blob_len_f64 = blob_len as f64;
    assert!(
        bytes >= blob_len_f64,
        "expected storage_bytes >= {blob_len_f64}, got {bytes}. Body:\n{body}"
    );

    server.abort();
    let _ = server.await;
}

// ----------------------------------------------------------------------
// /version
// ----------------------------------------------------------------------

#[tokio::test]
async fn version_returns_expected_fields() {
    let addr = pick_free_port();
    let (_tmp, cfg) = fixture_config(addr);
    let server = spawn_server(addr, cfg).await;

    let client = http_client();
    let url = format!("http://{addr}/version");
    let resp = client.get(&url).send().await.expect("GET /version");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("parse json");

    assert_eq!(body["name"], "roastery");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));

    // `rustc` is either null or a non-empty string. We accept null
    // (the build-script degraded case); we don't accept the literal
    // string "unknown" (the handler maps that to null).
    let rustc = &body["rustc"];
    if !rustc.is_null() {
        let s = rustc.as_str().expect("rustc should be a string");
        assert!(!s.is_empty(), "rustc was an empty string");
        assert_ne!(s, "unknown", "rustc should never be the literal 'unknown'");
    }

    // Same shape for git_sha + build_date.
    for key in ["git_sha", "build_date"] {
        let v = &body[key];
        if !v.is_null() {
            let s = v.as_str().unwrap_or_else(|| panic!("{key} not string"));
            assert!(!s.is_empty(), "{key} empty");
            assert_ne!(s, "unknown", "{key} should never be literal 'unknown'");
        }
    }

    server.abort();
    let _ = server.await;
}

// ----------------------------------------------------------------------
// Helpers — light text-format parsers good enough for the assertions
// above. We deliberately don't depend on a Prometheus parser crate;
// the contract under test IS the text format.
// ----------------------------------------------------------------------

/// Scrape an integer counter line from a Prometheus text body.
///
/// Looks for `<name>{<labels in any order>} <value>` (or `<name> <value>`
/// if `labels` is empty) and returns the parsed `i64`. Returns 0 if
/// no matching line is found — callers assert `>= n` on the result so
/// "not present" reads as "zero," which is what Prometheus dashboards
/// see too.
fn scrape_counter(body: &str, name: &str, labels: &[(&str, &str)]) -> i64 {
    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        if !line.starts_with(name) {
            continue;
        }
        if !labels_match(line, labels) {
            continue;
        }
        let Some(value) = line.split_whitespace().last() else {
            continue;
        };
        if let Ok(v) = value.parse::<i64>() {
            return v;
        }
        if let Ok(f) = value.parse::<f64>() {
            return f as i64;
        }
    }
    0
}

/// Same idea as [`scrape_counter`] but returns a float (for gauges).
fn scrape_gauge(body: &str, name: &str, labels: &[(&str, &str)]) -> f64 {
    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        if !line.starts_with(name) {
            continue;
        }
        if !labels_match(line, labels) {
            continue;
        }
        let Some(value) = line.split_whitespace().last() else {
            continue;
        };
        if let Ok(f) = value.parse::<f64>() {
            return f;
        }
    }
    0.0
}

/// Return true iff every `(key, value)` in `wanted` appears verbatim
/// as `key="value"` somewhere in `line`. Order-independent.
fn labels_match(line: &str, wanted: &[(&str, &str)]) -> bool {
    for (k, v) in wanted {
        let needle = format!("{k}=\"{v}\"");
        if !line.contains(&needle) {
            return false;
        }
    }
    true
}
