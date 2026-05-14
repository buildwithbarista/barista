// Integration-test / example / benchmark target — workspace security
// lints are allowed here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`)
// is the documented contract for failing a test loudly. This allow block
// keeps the crate root's `#![allow(...)]` from being silently dropped by
// the separate compilation unit each test file forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Tests for the `--ci` macro flag (M3.2 T4).
//!
//! `--ci` expands to `--frozen --output json --quiet` (plus
//! `--no-color`) so a single switch produces deterministic,
//! machine-consumable output. The acceptance criterion is:
//!
//! `[T]` `--ci` produces deterministic byte-equal output across runs
//!
//! We test the macro at two levels:
//!
//! 1. **Parser-level:** assert that the post-parse `GlobalFlags`
//!    reflect the expansion. This is fast and runs without any
//!    filesystem state.
//! 2. **End-to-end:** spawn-equivalent invocations of `dispatch()`
//!    over the same project two times in a row, capture stdout,
//!    and assert byte-equality.

use std::fs;
use std::path::PathBuf;

use barista_cli::cli::{Cli, GlobalFlags, OutputFormat};
use clap::Parser;

/// Run the parser over `argv`, mutate as `dispatch` would (apply the
/// `--ci` macro), and return the resulting `GlobalFlags`.
fn parse_and_apply(argv: &[&str]) -> GlobalFlags {
    let cli = Cli::try_parse_from(argv).expect("clap parse");
    let mut g = cli.global;
    if g.ci {
        g.frozen = true;
        g.quiet = true;
        g.output = OutputFormat::Json;
        g.no_color = true;
    }
    g
}

#[test]
fn ci_macro_expands_to_frozen_json_quiet_nocolor() {
    let g = parse_and_apply(&["barista", "--ci", "pull"]);
    assert!(g.ci, "--ci should be set");
    assert!(g.frozen, "--ci should imply --frozen");
    assert!(g.quiet, "--ci should imply --quiet");
    assert!(g.no_color, "--ci should imply --no-color");
    assert_eq!(
        g.output,
        OutputFormat::Json,
        "--ci should imply --output json"
    );
}

#[test]
fn ci_macro_does_not_set_unrelated_flags() {
    let g = parse_and_apply(&["barista", "--ci", "pull"]);
    assert!(!g.strict, "--ci should NOT imply --strict");
    assert!(!g.no_daemon, "--ci should NOT imply --no-daemon");
    assert_eq!(g.verbose, 0, "--ci should leave --verbose at 0");
}

#[test]
fn ci_macro_is_idempotent_when_user_also_passes_components() {
    // The user double-spells the macro by passing `--ci` AND the
    // individual flags. The result must be the same.
    let g = parse_and_apply(&[
        "barista",
        "--ci",
        "--frozen",
        "--quiet",
        "--no-color",
        "--output",
        "json",
        "pull",
    ]);
    assert!(g.frozen && g.quiet && g.no_color);
    assert_eq!(g.output, OutputFormat::Json);
}

#[test]
fn frozen_flag_is_independently_settable_without_ci() {
    let g = parse_and_apply(&["barista", "--frozen", "pull"]);
    assert!(g.frozen);
    assert!(!g.ci);
    assert_eq!(
        g.output,
        OutputFormat::Human,
        "no --ci → human stays default"
    );
}

#[test]
fn ci_macro_off_by_default() {
    let g = parse_and_apply(&["barista", "pull"]);
    assert!(!g.ci);
    assert!(!g.frozen);
    assert!(!g.quiet);
    assert!(!g.no_color);
    assert_eq!(g.output, OutputFormat::Human);
}

// ---------------------------------------------------------------------
// End-to-end determinism: two `barista --ci pull --no-fetch` runs
// over the same project produce byte-equal stdout.
// ---------------------------------------------------------------------

/// Build a minimal Maven project under `dir`. Returns the project root.
fn write_minimal_project(dir: &std::path::Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let pom = r#"<?xml version="1.0"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
</project>
"#;
    fs::write(dir.join("pom.xml"), pom).unwrap();
    dir.to_path_buf()
}

/// Capture stdout of `barista --ci pull --no-fetch --root <dir>`.
fn run_ci_pull(dir: &std::path::Path) -> Vec<u8> {
    // Invoke via the built binary so we exercise the real
    // `dispatch()` path under a separate process — that's the same
    // surface a CI environment hits. Path is resolved relative to
    // the workspace's target/debug.
    let exe = barista_test_bin();
    let out = std::process::Command::new(&exe)
        .args([
            "--ci",
            "--root",
            dir.to_str().unwrap(),
            "pull",
            "--no-fetch",
        ])
        .output()
        .unwrap_or_else(|e| panic!("spawn {exe:?}: {e}"));
    assert!(
        out.status.success(),
        "barista --ci pull failed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Locate the freshly-built `barista` binary. Cargo sets
/// `CARGO_BIN_EXE_barista` for integration tests.
fn barista_test_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_barista"))
}

#[test]
fn ci_pull_produces_byte_equal_output_across_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = write_minimal_project(&tmp.path().join("proj"));

    let a = run_ci_pull(&proj);
    let b = run_ci_pull(&proj);

    assert_eq!(
        a, b,
        "two consecutive `barista --ci pull --no-fetch` runs should produce byte-equal stdout"
    );

    // Sanity-check: the output is JSON and discriminated as `pull`.
    let v: serde_json::Value = serde_json::from_slice(&a)
        .unwrap_or_else(|e| panic!("--ci output should be JSON: {e}\nbytes: {a:?}"));
    assert_eq!(v["command"], "pull");
}

#[test]
fn ci_pull_output_omits_trailing_garbage() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = write_minimal_project(&tmp.path().join("proj"));
    let out = run_ci_pull(&proj);

    // Trim a single trailing newline (json renderer terminates the
    // document with one).
    let trimmed: &[u8] = out.strip_suffix(b"\n").unwrap_or(&out);
    let parsed: serde_json::Value = serde_json::from_slice(trimmed).unwrap();
    assert!(parsed.is_object());
}
