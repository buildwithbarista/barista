// SPDX-License-Identifier: MIT OR Apache-2.0

// Integration-test target — workspace security lints are allowed
// here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`) is the
// documented contract for failing a test loudly. This allow block
// keeps the crate root's `#![allow(...)]` from being silently dropped
// by the separate compilation unit each test file forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Gap-closing snapshot matrix: pin **every** CLI output shape.
//!
//! The renderer matrix in `tests/output_snapshots.rs` already pins
//! `pull` / `grind tree` / `pour` across human / json / ndjson, and
//! the per-command suites (`cmd_tap`, `cmd_dial_in`, `cmd_wrapper`,
//! `cmd_grind`, `cmd_maven_vocab`, `cli_parser`) pin the shapes they
//! own. This file closes the remaining gaps so the contract "every
//! CLI output shape is snapshot-tested" holds end-to-end:
//!
//! 1. **Structured error envelope** — the `{"command":"error", …}`
//!    JSON document (pretty + compact), the `{"event":"error", …}`
//!    NDJSON line, and the `error: …` human stderr line. This is the
//!    shape downstream tooling parses to detect a failure, and it
//!    carries the `BAR-*` error code in its `message`; previously it
//!    was only exercised behaviourally, never pinned to bytes.
//!
//! 2. **`verify` report** — the [`VerifyReport`] shape rendered by
//!    `barista verify` / `barista shot <phase>` / the eight Maven-
//!    vocabulary lifecycle commands when they execute. Pinned across
//!    human (happy + failure paths), json (pretty + compact), and
//!    ndjson. Previously schema-validated but never byte-snapshotted.
//!
//! 3. **All eight Maven-vocabulary "not yet executable" texts** — the
//!    Phase-3 stub shape for `clean | compile | test | package |
//!    verify | install | deploy | site`. `cmd_maven_vocab` snapshots
//!    `compile` + `test`; this file pins the remaining six so every
//!    phase has a frozen text shape.
//!
//! # Honesty notes
//!
//! - The `verify` snapshots pin the **real** [`VerifyReport`]
//!   serialization (`phase`, `invocations`, `planned-actions`, …).
//!   That is intentionally *not* validated against
//!   `schema/output/v1/verify.json`, which is a reserved stub
//!   (`status` / `details`) describing a different, not-yet-shipped
//!   shape. Pinning the real bytes is the point of T2; reconciling the
//!   stub schema with the shipped struct is out of scope here.
//! - The eight Maven-vocab commands return a structured "not yet
//!   executable" stub in this phase. We snapshot **that** current
//!   shape as-is rather than fabricating executed-build output.
//! - Non-deterministic fields are redacted with `insta` filters:
//!   tempdir project paths (`project:` line), and `duration-ms` /
//!   `duration_ms` (wall-clock) in the verify shapes.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use barista_cli::cli::{Cli, GlobalFlags, MavenVocabArgs};
use barista_cli::cmd::{MavenPhase, maven_vocab};
use barista_cli::output::{
    HumanRenderer, JsonRenderer, MojoInvocation, NdjsonRenderer, Renderer, VerifyReport,
};
use clap::Parser;
use serde_json::Value;
use tempfile::tempdir;

// =====================================================================
// Shared in-memory `Write` buffer (same pattern as the sibling
// snapshot suites — integration tests can't share a `mod` without a
// `tests/common/` declaration, so this is copied inline).
// =====================================================================

#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
    fn writer(&self) -> Box<dyn Write + Send> {
        Box::new(BufWriter(self.0.clone()))
    }
    fn as_string(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).expect("renderer wrote non-UTF8")
    }
}

struct BufWriter(Arc<Mutex<Vec<u8>>>);
impl Write for BufWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Fixed NDJSON timestamp shared across the snapshot suites so the
/// pinned bytes are deterministic.
const FIXED_TS: &str = "2026-05-14T12:34:56.789Z";

// =====================================================================
// 1. Structured error envelope.
//
// Every command's error arm calls `Renderer::render_error`. The JSON
// document is the contract downstream tooling parses; pin all three
// presentations. A representative `BAR-*`-coded message is used so the
// snapshot also documents how a structured daemon code surfaces in the
// `message` field.
// =====================================================================

/// A stand-in for a command error whose `Display` carries a `BAR-*`
/// code, mirroring the way `cmd::verify` / `cmd::no_daemon` surface
/// daemon-side codes through `Renderer::render_error`.
#[derive(Debug, thiserror::Error)]
#[error("barista verify failed: BAR-DEPLOY-AUTH-MISSING: no credentials for server `central`")]
struct CodedError;

#[test]
fn error_envelope_json_pretty() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ true);
    r.render_error(&CodedError).unwrap();
    let body = stdout.as_string();
    let doc: Value = serde_json::from_str(&body).expect("error doc is valid JSON");
    assert_eq!(doc["command"], "error");
    insta::assert_snapshot!("error_envelope_json_pretty", body);
}

#[test]
fn error_envelope_json_compact() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ false);
    r.render_error(&CodedError).unwrap();
    let body = stdout.as_string();
    // Compact JSON has exactly one newline (the trailing one).
    assert_eq!(body.matches('\n').count(), 1, "got:\n{body}");
    insta::assert_snapshot!("error_envelope_json_compact", body);
}

#[test]
fn error_envelope_ndjson() {
    let stdout = SharedBuf::new();
    let mut r = NdjsonRenderer::with_fixed_timestamp(stdout.writer(), FIXED_TS);
    r.render_error(&CodedError).unwrap();
    let body = stdout.as_string();
    let env: Value = serde_json::from_str(body.trim_end()).expect("ndjson line is JSON");
    assert_eq!(env["event"], "error");
    insta::assert_snapshot!("error_envelope_ndjson", body);
}

#[test]
fn error_envelope_human() {
    // The human renderer writes errors to stderr and leaves stdout
    // untouched. Pin the stderr line and assert stdout stays empty.
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut r = HumanRenderer::new(stdout.writer(), stderr.writer(), /* ansi */ false);
    r.render_error(&CodedError).unwrap();
    assert!(
        stdout.as_string().is_empty(),
        "human error must not touch stdout"
    );
    insta::assert_snapshot!("error_envelope_human", stderr.as_string());
}

// =====================================================================
// 2. `verify` report — the execution shape shared by `verify`,
//    `shot <phase>`, and the eight Maven-vocabulary commands once they
//    route through the daemon. Two canonical samples: a happy
//    single-module run and a build with a failed mojo carrying a
//    `BAR-*` code.
// =====================================================================

/// A happy single-module `verify` run: two mojos, both succeed.
fn sample_verify_ok() -> VerifyReport {
    VerifyReport {
        project_root: PathBuf::from("/proj"),
        phase: "verify".to_string(),
        planned_actions: 2,
        executed_actions: 2,
        failed_actions: 0,
        daemon_respawns: 0,
        invocations: vec![
            MojoInvocation {
                phase: "compile".to_string(),
                mojo: "org.apache.maven.plugins:maven-compiler-plugin:3.13.0:compile".to_string(),
                module: PathBuf::from("/proj"),
                exit_code: 0,
                status: "success".to_string(),
                failure_message: String::new(),
                error_code: String::new(),
                duration_ms: 412,
            },
            MojoInvocation {
                phase: "test".to_string(),
                mojo: "org.apache.maven.plugins:maven-surefire-plugin:3.2.5:test".to_string(),
                module: PathBuf::from("/proj"),
                exit_code: 0,
                status: "success".to_string(),
                failure_message: String::new(),
                error_code: String::new(),
                duration_ms: 1893,
            },
        ],
        duration_ms: 2305,
    }
}

/// A failing run: the second mojo fails with a structured daemon code.
/// Exercises the `failure_message` + `error_code` fields (both skipped
/// when empty on the happy path) and the failure summary line.
fn sample_verify_failed() -> VerifyReport {
    VerifyReport {
        project_root: PathBuf::from("/proj"),
        phase: "deploy".to_string(),
        planned_actions: 2,
        executed_actions: 2,
        failed_actions: 1,
        daemon_respawns: 0,
        invocations: vec![
            MojoInvocation {
                phase: "package".to_string(),
                mojo: "org.apache.maven.plugins:maven-jar-plugin:3.4.1:jar".to_string(),
                module: PathBuf::from("/proj"),
                exit_code: 0,
                status: "success".to_string(),
                failure_message: String::new(),
                error_code: String::new(),
                duration_ms: 88,
            },
            MojoInvocation {
                phase: "deploy".to_string(),
                mojo: "org.apache.maven.plugins:maven-deploy-plugin:3.1.2:deploy".to_string(),
                module: PathBuf::from("/proj"),
                exit_code: 1,
                status: "failure".to_string(),
                failure_message: "no credentials for server `central` in settings.xml".to_string(),
                error_code: "BAR-DEPLOY-AUTH-MISSING".to_string(),
                duration_ms: 51,
            },
        ],
        duration_ms: 162,
    }
}

/// Redact the wall-clock `duration-ms` (kebab, JSON) and `duration_ms`
/// (the human summary uses ` ms` after the number) so the verify
/// snapshots stay byte-stable regardless of the sample's literal
/// durations drifting.
fn verify_settings() -> insta::Settings {
    let mut s = insta::Settings::clone_current();
    // JSON: `"duration-ms": 2305` (pretty) and `"duration-ms":2305`
    // (compact / ndjson) -> redact the integer in both forms.
    s.add_filter(r#""duration-ms": ?\d+"#, r#""duration-ms":"[MS]""#);
    // Human summary: `… in 2305 ms` -> redact the integer.
    s.add_filter(r"in \d+ ms", "in [MS] ms");
    s
}

#[test]
fn verify_ok_human() {
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut r = HumanRenderer::new(stdout.writer(), stderr.writer(), /* ansi */ false);
    r.render_verify(&sample_verify_ok()).unwrap();
    assert!(
        stdout.as_string().is_empty(),
        "human verify must not touch stdout"
    );
    verify_settings().bind(|| {
        insta::assert_snapshot!("verify_ok_human", stderr.as_string());
    });
}

#[test]
fn verify_failed_human() {
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut r = HumanRenderer::new(stdout.writer(), stderr.writer(), /* ansi */ false);
    r.render_verify(&sample_verify_failed()).unwrap();
    verify_settings().bind(|| {
        insta::assert_snapshot!("verify_failed_human", stderr.as_string());
    });
}

#[test]
fn verify_ok_json_pretty() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ true);
    r.render_verify(&sample_verify_ok()).unwrap();
    let body = stdout.as_string();
    let doc: Value = serde_json::from_str(&body).expect("verify is valid JSON");
    assert_eq!(doc["command"], "verify");
    verify_settings().bind(|| {
        insta::assert_snapshot!("verify_ok_json_pretty", body);
    });
}

#[test]
fn verify_ok_json_compact() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ false);
    r.render_verify(&sample_verify_ok()).unwrap();
    let body = stdout.as_string();
    assert_eq!(body.matches('\n').count(), 1, "got:\n{body}");
    verify_settings().bind(|| {
        insta::assert_snapshot!("verify_ok_json_compact", body);
    });
}

#[test]
fn verify_failed_json_pretty() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ true);
    r.render_verify(&sample_verify_failed()).unwrap();
    let body = stdout.as_string();
    let doc: Value = serde_json::from_str(&body).expect("verify is valid JSON");
    // The failing path carries the structured daemon code downstream
    // tooling branches on.
    assert_eq!(doc["invocations"][1]["error-code"], "BAR-DEPLOY-AUTH-MISSING");
    verify_settings().bind(|| {
        insta::assert_snapshot!("verify_failed_json_pretty", body);
    });
}

#[test]
fn verify_ok_ndjson() {
    let stdout = SharedBuf::new();
    let mut r = NdjsonRenderer::with_fixed_timestamp(stdout.writer(), FIXED_TS);
    r.render_verify(&sample_verify_ok()).unwrap();
    let body = stdout.as_string();
    let env: Value = serde_json::from_str(body.trim_end()).expect("ndjson line is JSON");
    assert_eq!(env["event"], "result");
    assert_eq!(env["payload"]["command"], "verify");
    verify_settings().bind(|| {
        insta::assert_snapshot!("verify_ok_ndjson", body);
    });
}

// =====================================================================
// 3. Maven-vocabulary "not yet executable" stub text — the six phases
//    `cmd_maven_vocab` does not already snapshot. Driven through the
//    same `maven_vocab::render` seam the existing suite uses; the
//    tempdir-derived `project:` line is redacted for stability.
// =====================================================================

fn no_args() -> MavenVocabArgs {
    MavenVocabArgs { args: vec![] }
}

/// Build a `GlobalFlags` whose `--root` points at `dir` (so the
/// resolver succeeds and the message names the project root, which we
/// then redact).
fn globals_with_root(dir: &std::path::Path) -> GlobalFlags {
    let cli = Cli::try_parse_from(["barista", "--root", dir.to_str().unwrap(), "clean"])
        .expect("parse clean with root");
    cli.global
}

/// Redact the tempdir-derived `project:` line so the stub-text
/// snapshots are host-independent (mirrors `cmd_maven_vocab`).
fn redact_project_line(s: &str) -> String {
    s.lines()
        .map(|l| {
            if l.starts_with("project: ") {
                "project: <REDACTED>".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn snapshot_phase_stub(phase: MavenPhase, snap_name: &str) {
    let td = tempdir().unwrap();
    std::fs::write(td.path().join("pom.xml"), b"<project/>").unwrap();
    let g = globals_with_root(td.path());
    let out = maven_vocab::render(&g, phase, &no_args());
    insta::assert_snapshot!(snap_name, redact_project_line(&out));
}

#[test]
fn maven_vocab_clean_stub_text() {
    snapshot_phase_stub(MavenPhase::Clean, "maven_vocab_clean_stub");
}

#[test]
fn maven_vocab_package_stub_text() {
    snapshot_phase_stub(MavenPhase::Package, "maven_vocab_package_stub");
}

#[test]
fn maven_vocab_verify_stub_text() {
    snapshot_phase_stub(MavenPhase::Verify, "maven_vocab_verify_stub");
}

#[test]
fn maven_vocab_install_stub_text() {
    snapshot_phase_stub(MavenPhase::Install, "maven_vocab_install_stub");
}

#[test]
fn maven_vocab_deploy_stub_text() {
    snapshot_phase_stub(MavenPhase::Deploy, "maven_vocab_deploy_stub");
}

#[test]
fn maven_vocab_site_stub_text() {
    snapshot_phase_stub(MavenPhase::Site, "maven_vocab_site_stub");
}
