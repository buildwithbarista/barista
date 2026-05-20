// Integration-test target — workspace security lints are allowed
// here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`) is the
// documented contract for failing a test loudly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Health-probe transport tests.
//!
//! `[T]` `status` against a live roastery returns healthy; against a
//! dead one returns a clear unhealthy reason within the timeout.
//!
//! These use an in-process `wiremock` server for the "healthy" cases
//! and an OS-assigned-but-immediately-closed port for the "dead"
//! case, so the probe never reaches a real network.

use std::time::{Duration, Instant};

use barista_tap::{Tap, TapKind, TapHealth, probe};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A short timeout so the "dead endpoint" cases finish fast.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Bind an ephemeral TCP port, then drop the listener so the port is
/// (almost certainly) closed — a connect there is refused promptly.
async fn dead_endpoint_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

#[tokio::test]
async fn roastery_healthz_200_is_healthy() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok\n"))
        .mount(&server)
        .await;

    let tap = Tap::new("live", server.uri(), TapKind::Roastery).unwrap();
    let health = probe(&tap, PROBE_TIMEOUT).await;
    assert!(health.is_healthy(), "expected healthy, got {health:?}");
    match health {
        TapHealth::Healthy { detail } => assert!(detail.contains("200"), "detail: {detail}"),
        other => panic!("expected Healthy, got {other:?}"),
    }
}

#[tokio::test]
async fn roastery_503_is_unhealthy_with_status_reason() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let tap = Tap::new("sick", server.uri(), TapKind::Roastery).unwrap();
    let health = probe(&tap, PROBE_TIMEOUT).await;
    match health {
        TapHealth::Unhealthy { reason } => assert!(reason.contains("503"), "reason: {reason}"),
        other => panic!("expected Unhealthy, got {other:?}"),
    }
}

#[tokio::test]
async fn roastery_dead_endpoint_is_unhealthy_within_timeout() {
    let url = dead_endpoint_url().await;
    let tap = Tap::new("dead", &url, TapKind::Roastery).unwrap();

    let start = Instant::now();
    let health = probe(&tap, PROBE_TIMEOUT).await;
    let elapsed = start.elapsed();

    match health {
        TapHealth::Unhealthy { reason } => {
            assert!(!reason.is_empty(), "reason should be non-empty");
        }
        other => panic!("expected Unhealthy, got {other:?}"),
    }
    // A refused connection resolves well within the timeout (never
    // hangs). Generous bound to stay robust on a loaded CI box.
    assert!(
        elapsed < PROBE_TIMEOUT + Duration::from_secs(1),
        "probe took {elapsed:?}, expected under the timeout"
    );
}

#[tokio::test]
async fn worker_head_reachable_is_healthy() {
    let server = MockServer::start().await;
    // A worker probe is a plain HEAD against the base URL; any
    // response proves reachability. Answer the HEAD with a 200.
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let tap = Tap::new("worker", server.uri(), TapKind::Worker).unwrap();
    let health = probe(&tap, PROBE_TIMEOUT).await;
    assert!(health.is_healthy(), "expected healthy, got {health:?}");
}

#[tokio::test]
async fn worker_reachable_even_on_4xx() {
    let server = MockServer::start().await;
    // Even a 404 to a HEAD proves the host is reachable — the worker
    // probe is a reachability signal, not a status check.
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let tap = Tap::new("worker", server.uri(), TapKind::Worker).unwrap();
    let health = probe(&tap, PROBE_TIMEOUT).await;
    assert!(health.is_healthy(), "expected healthy (reachable), got {health:?}");
}

#[tokio::test]
async fn worker_dead_endpoint_is_unhealthy() {
    let url = dead_endpoint_url().await;
    let tap = Tap::new("dead-worker", &url, TapKind::Worker).unwrap();
    let health = probe(&tap, PROBE_TIMEOUT).await;
    assert!(!health.is_healthy(), "expected unhealthy, got {health:?}");
}
