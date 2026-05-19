//! Smoke tests for the roastery HTTP scaffold.
//!
//! These exercise the binary's library entry point (`roastery::run`)
//! end-to-end: spawn a server on an ephemeral port, issue a real
//! HTTP request via `reqwest`, assert the placeholder body comes
//! back.
//!
//! They are deliberately small — subsequent M5.1 tasks own deeper
//! protocol-level tests in their own integration files.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::time::{Duration, Instant};

use roastery::{ServerConfig, run};
use tokio::net::TcpStream;
use tokio::time::sleep;

/// Reserve an OS-assigned localhost port, then release it. The
/// returned address is racy but good enough for a single-threaded
/// test — the kernel is unlikely to hand the same port to anyone
/// else in the few milliseconds between drop and re-bind.
fn pick_free_port() -> SocketAddr {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    addr
}

/// Wait up to `timeout` for `addr` to accept a TCP connection.
/// Returns once the connection succeeds; panics on timeout.
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

#[tokio::test]
async fn roastery_server_starts_and_serves_placeholder_root() {
    let cfg = ServerConfig::with_bind("127.0.0.1:0".parse().unwrap());

    // Bind synchronously up-front so we know the port before we
    // spawn the server task. We use `with_bind` plus the ephemeral
    // port the OS would have picked anyway — but we drop our probe
    // listener and let `run` re-bind it.
    let probe = StdTcpListener::bind(cfg.bind).expect("bind probe");
    let bound = probe.local_addr().expect("probe local_addr");
    drop(probe);

    let cfg = ServerConfig::with_bind(bound);
    let server = tokio::spawn(async move { run(cfg).await });

    wait_for_listener(bound, Duration::from_secs(4)).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("build client");
    let url = format!("http://{bound}/");
    let resp = client.get(&url).send().await.expect("send GET /");
    assert_eq!(resp.status(), 200, "status: {}", resp.status());
    let body = resp.text().await.expect("read body");
    assert!(
        body.contains("roastery"),
        "body did not mention roastery: {body:?}"
    );

    server.abort();
    // Best-effort: let the abort propagate so the runtime tears down
    // cleanly. We don't await the JoinHandle — `run` is designed to
    // loop until shutdown, so it'd surface as a `JoinError::Cancelled`.
    let _ = server.await;
}

#[tokio::test]
async fn roastery_server_listens_on_configured_port() {
    let addr = pick_free_port();
    let cfg = ServerConfig::with_bind(addr);
    let server = tokio::spawn(async move { run(cfg).await });

    wait_for_listener(addr, Duration::from_secs(4)).await;

    // A second connect attempt also succeeds → the server is
    // accepting, not just bound-and-dropped.
    let _ = TcpStream::connect(addr).await.expect("second connect");

    server.abort();
    let _ = server.await;
}
