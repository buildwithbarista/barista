// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Always-run integration test for the `CaptureSession` start/stop
//! lifecycle.
//!
//! These tests cover T1's acceptance criterion that the lifecycle works
//! end-to-end against a *stub* subprocess. The stub is a `/bin/sh -c`
//! one-liner that writes a known-good HAR to the requested path and
//! then sleeps until killed; this mirrors mitmproxy's "flush on
//! shutdown" contract without requiring mitmproxy to be installed.
//!
//! The real-mitmproxy round-trip is gated `#[ignore]` and lives in
//! `tests/session_smoke.rs`.

#![cfg(unix)]

use std::time::Duration;

use barista_netcap::{CaptureConfig, CaptureSession};

/// A minimal, valid HAR document. Kept inline so the stub script
/// doesn't need a separate fixture file.
const STUB_HAR: &str = r#"{
  "log": {
    "version": "1.2",
    "creator": {"name": "barista-netcap-test-stub", "version": "0"},
    "entries": [
      {"request": {"method": "GET", "url": "http://example.test/a"}},
      {"request": {"method": "GET", "url": "http://example.test/b"}}
    ]
  }
}"#;

#[tokio::test(flavor = "current_thread")]
async fn start_stop_lifecycle_with_stub_subprocess() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let har_path = tmp.path().join("session.har");

    // Build a stub command: write the HAR atomically, then sleep until
    // the parent SIGKILLs us. `mv` after `cat` makes the write
    // atomic — if the parent kills us *during* the cat we don't leave a
    // half-written HAR behind that would fail validation.
    let script = format!(
        "cat > '{tmp}/session.har.partial' <<'HAR_EOF'\n{har}\nHAR_EOF\nmv '{tmp}/session.har.partial' '{out}'\nexec sleep 30\n",
        tmp = tmp.path().display(),
        out = har_path.display(),
        har = STUB_HAR,
    );

    let mut cfg = CaptureConfig::for_har(&har_path);
    cfg.program = Some(std::path::PathBuf::from("/bin/sh"));
    cfg.extra_args = vec!["-c".to_string(), script];
    cfg.timeout = Duration::from_secs(5);

    let session = CaptureSession::start(cfg).await.expect("start");
    assert!(
        session.listen_port() > 0,
        "session.listen_port() should be allocated"
    );

    // Give the stub a moment to write the HAR before we signal it.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let summary = session.stop().await.expect("stop");
    assert_eq!(summary.har.entry_count, 2);
    assert_eq!(summary.har.path, har_path);
    assert!(
        summary.exit_status.contains("signal") || summary.exit_status.contains("exit code"),
        "exit_status should be human-readable, got {:?}",
        summary.exit_status
    );
}

#[tokio::test(flavor = "current_thread")]
async fn missing_program_reports_typed_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let har_path = tmp.path().join("session.har");

    let mut cfg = CaptureConfig::for_har(&har_path);
    cfg.program = Some(std::path::PathBuf::from(
        "/nonexistent/path/to/mitmdump-stub",
    ));
    cfg.extra_args = vec![];

    let err = CaptureSession::start(cfg).await.expect_err("should fail");
    match err {
        barista_netcap::NetcapError::SpawnFailed { program, .. } => {
            assert!(program.contains("mitmdump-stub"));
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stop_reports_har_invalid_when_subprocess_wrote_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let har_path = tmp.path().join("session.har");

    // Stub that just sleeps — never writes the HAR.
    let mut cfg = CaptureConfig::for_har(&har_path);
    cfg.program = Some(std::path::PathBuf::from("/bin/sh"));
    cfg.extra_args = vec!["-c".to_string(), "exec sleep 30".to_string()];
    cfg.timeout = Duration::from_secs(5);

    let session = CaptureSession::start(cfg).await.expect("start");
    let err = session.stop().await.expect_err("should fail validation");
    match err {
        barista_netcap::NetcapError::HarInvalid { reason, .. } => {
            assert!(reason.contains("does not exist") || reason.contains("empty"));
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
