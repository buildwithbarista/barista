// SPDX-License-Identifier: MIT OR Apache-2.0

// Integration-test target — workspace security lints are allowed
// here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`) is the
// documented contract for failing a test loudly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! End-to-end snapshot + behaviour tests for `barista tap`.
//!
//! These run the real `barista` binary as a subprocess
//! (`CARGO_BIN_EXE_barista`) against a hermetic temp `barista.toml`
//! (`--config <tmp>/barista.toml`), so the captured stdout is exactly
//! what a user sees. Snapshots are committed via `insta`.
//!
//! `[T]` **Add → list → status → remove cycle is idempotent**:
//! [`add_list_status_remove_cycle_is_idempotent`] drives the full
//! cycle twice and asserts the registry returns to empty and a
//! double-remove is a clean success.
//!
//! `[T]` **`status` healthy vs unhealthy**:
//! [`status_healthy_then_unhealthy`] points one tap at an in-process
//! mock returning 200 (`/healthz`) and another at a closed port, and
//! asserts the healthy/unhealthy split + the non-zero exit when any
//! tap is unhealthy.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Path to the compiled `barista` binary under test.
fn barista_bin() -> &'static str {
    env!("CARGO_BIN_EXE_barista")
}

/// Run `barista --config <cfg> tap <args...>` and return the output.
fn run_tap(cfg: &Path, args: &[&str]) -> Output {
    Command::new(barista_bin())
        .arg("--config")
        .arg(cfg)
        .arg("tap")
        .args(args)
        .output()
        .expect("spawn barista")
}

/// Stdout of a tap invocation as a `String`, asserting the expected
/// exit code.
fn stdout_with_code(cfg: &Path, args: &[&str], expect_code: i32) -> String {
    let out = run_tap(cfg, args);
    let code = out.status.code().unwrap_or(-1);
    assert_eq!(
        code,
        expect_code,
        "args {args:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

// ============================================================
// add / list / remove snapshots
// ============================================================

#[test]
fn add_confirmation_text() {
    let dir = TempDir::new().unwrap();
    let cfg = dir.path().join("barista.toml");
    let out = stdout_with_code(&cfg, &["add", "acme", "https://roastery.acme.com"], 0);
    insta::assert_snapshot!("tap_add_text", out);
}

#[test]
fn list_empty_text() {
    let dir = TempDir::new().unwrap();
    let cfg = dir.path().join("barista.toml");
    let out = stdout_with_code(&cfg, &["list"], 0);
    insta::assert_snapshot!("tap_list_empty_text", out);
}

#[test]
fn list_empty_json() {
    let dir = TempDir::new().unwrap();
    let cfg = dir.path().join("barista.toml");
    // `--output json` is a global flag; place it before the
    // subcommand path so it's unambiguous.
    let out = Command::new(barista_bin())
        .args(["--config"])
        .arg(&cfg)
        .args(["--output", "json", "tap", "list"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    insta::assert_snapshot!(
        "tap_list_empty_json",
        String::from_utf8(out.stdout).unwrap()
    );
}

#[test]
fn list_one_text() {
    let dir = TempDir::new().unwrap();
    let cfg = dir.path().join("barista.toml");
    stdout_with_code(&cfg, &["add", "acme", "https://roastery.acme.com"], 0);
    let out = stdout_with_code(&cfg, &["list"], 0);
    insta::assert_snapshot!("tap_list_one_text", out);
}

#[test]
fn list_many_text_and_json() {
    let dir = TempDir::new().unwrap();
    let cfg = dir.path().join("barista.toml");
    stdout_with_code(&cfg, &["add", "alpha", "https://alpha.example.com"], 0);
    stdout_with_code(
        &cfg,
        &[
            "add",
            "beta",
            "http://beta.example.com:9000",
            "--kind",
            "worker",
        ],
        0,
    );
    stdout_with_code(&cfg, &["add", "gamma", "https://gamma.example.com"], 0);

    let text = stdout_with_code(&cfg, &["list"], 0);
    insta::assert_snapshot!("tap_list_many_text", text);

    let json = Command::new(barista_bin())
        .args(["--config"])
        .arg(&cfg)
        .args(["--output", "json", "tap", "list"])
        .output()
        .unwrap();
    assert_eq!(json.status.code(), Some(0));
    insta::assert_snapshot!(
        "tap_list_many_json",
        String::from_utf8(json.stdout).unwrap()
    );
}

#[test]
fn remove_present_text() {
    let dir = TempDir::new().unwrap();
    let cfg = dir.path().join("barista.toml");
    stdout_with_code(&cfg, &["add", "acme", "https://roastery.acme.com"], 0);
    let out = stdout_with_code(&cfg, &["remove", "acme"], 0);
    insta::assert_snapshot!("tap_remove_present_text", out);
}

#[test]
fn remove_absent_text_is_clean_success() {
    let dir = TempDir::new().unwrap();
    let cfg = dir.path().join("barista.toml");
    // Removing from an empty/fresh config is a clean success (exit 0).
    let out = stdout_with_code(&cfg, &["remove", "ghost"], 0);
    insta::assert_snapshot!("tap_remove_absent_text", out);
}

// ============================================================
// [T] add -> list -> status -> remove cycle is idempotent
// ============================================================

#[tokio::test]
async fn add_list_status_remove_cycle_is_idempotent() {
    // A live mock so `status` is deterministic during the cycle.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    let cfg = dir.path().join("barista.toml");
    let uri = server.uri();

    for round in 0..2 {
        // add
        stdout_with_code(&cfg, &["add", "live", &uri], 0);
        // list shows exactly one tap
        let listing = stdout_with_code(&cfg, &["list"], 0);
        assert!(listing.contains("live"), "round {round}: {listing}");
        // status: the single live tap is healthy -> exit 0
        let status = stdout_with_code(&cfg, &["status"], 0);
        assert!(status.contains("HEALTHY"), "round {round}: {status}");
        // remove: present -> removed
        let removed = stdout_with_code(&cfg, &["remove", "live"], 0);
        assert!(removed.contains("removed"), "round {round}: {removed}");
        // remove again: absent -> clean no-op success (exit 0)
        stdout_with_code(&cfg, &["remove", "live"], 0);
        // registry is empty again
        let empty = stdout_with_code(&cfg, &["list"], 0);
        assert!(
            empty.contains("no taps registered"),
            "round {round}: {empty}"
        );
    }

    // After the full cycle the on-disk config has no taps section.
    let raw = std::fs::read_to_string(&cfg).unwrap_or_default();
    assert!(
        !raw.contains("[[taps]]"),
        "config should be tap-less:\n{raw}"
    );
}

// ============================================================
// [T] status: healthy vs unhealthy + snapshot
// ============================================================

/// Bind then drop an ephemeral port so a connect there is refused.
async fn dead_uri() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    format!("http://{addr}")
}

#[tokio::test]
async fn status_healthy_then_unhealthy() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let dead = dead_uri().await;

    let dir = TempDir::new().unwrap();
    let cfg = dir.path().join("barista.toml");
    stdout_with_code(&cfg, &["add", "live", &server.uri()], 0);
    stdout_with_code(&cfg, &["add", "dead", &dead], 0);

    // Probing a specific live tap is healthy -> exit 0.
    let live = run_tap(&cfg, &["status", "live"]);
    assert_eq!(live.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&live.stdout).contains("HEALTHY"));

    // Probing the dead tap is unhealthy -> exit 1, with a clear reason.
    let dead_out = run_tap(&cfg, &["status", "dead"]);
    assert_eq!(dead_out.status.code(), Some(1));
    let dead_text = String::from_utf8_lossy(&dead_out.stdout);
    assert!(dead_text.contains("UNHEALTHY"), "stdout: {dead_text}");

    // Probing all: one healthy + one unhealthy -> overall exit 1.
    let all = run_tap(&cfg, &["status"]);
    assert_eq!(all.status.code(), Some(1));

    // JSON status of the dead tap is deterministic enough to snapshot
    // (the reason string is a fixed classification).
    let json = Command::new(barista_bin())
        .args(["--config"])
        .arg(&cfg)
        .args(["--output", "json", "tap", "status", "dead"])
        .output()
        .unwrap();
    assert_eq!(json.status.code(), Some(1));
    // The dead tap's URL carries a random ephemeral port; redact it
    // to a stable placeholder so the snapshot is deterministic.
    let mut settings = insta::Settings::clone_current();
    settings.add_filter(r"127\.0\.0\.1:\d+", "127.0.0.1:[PORT]");
    settings.bind(|| {
        insta::assert_snapshot!(
            "tap_status_unhealthy_json",
            String::from_utf8(json.stdout).unwrap()
        );
    });
}
