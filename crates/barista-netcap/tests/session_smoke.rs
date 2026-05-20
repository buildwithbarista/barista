// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `#[ignore]`-gated smoke test against a real mitmproxy installation.
//!
//! This test is **off by default** because mitmproxy isn't on the
//! default CI image; run it manually on a host with mitmproxy installed
//! to verify the full HTTPS round-trip:
//!
//! ```text
//! cargo test -p barista-netcap -- --ignored
//! ```
//!
//! The test does not exercise HTTPS interception (that requires the CA
//! to be trusted by the HTTP client we'd use). Instead it confirms that
//! a real mitmdump process spawned by [`CaptureSession`]:
//!   1. starts and binds the assigned port,
//!   2. writes a parseable HAR on shutdown.
//!
//! Sending traffic through the proxy is left as a follow-up for B.1 T3
//! once we wire a real Maven build into the harness; the unit-level
//! lifecycle path is covered by `session_lifecycle.rs`.

#![cfg(unix)]

use std::time::Duration;

use barista_netcap::{CaptureConfig, CaptureSession};

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires mitmproxy on PATH; run with `cargo test -- --ignored`"]
async fn real_mitmdump_starts_and_flushes_har() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let har_path = tmp.path().join("session.har");

    let mut cfg = CaptureConfig::for_har(&har_path);
    cfg.timeout = Duration::from_secs(30);

    let session = CaptureSession::start(cfg).await.expect("mitmdump start");
    let port = session.listen_port();
    assert!(port > 0);

    // Give mitmdump a beat to actually bind the listen socket and
    // register its HAR add-on before we tear it down.
    tokio::time::sleep(Duration::from_millis(750)).await;

    let summary = session.stop().await.expect("mitmdump stop + HAR parse");
    // A clean shutdown with no requests through the proxy emits an
    // empty `log.entries` array — that's still a valid HAR.
    assert_eq!(summary.listen_port, port);
}
