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

//! Integration tests for `barista pull`.
//!
//! These drive the CLI library's `dispatch` entry point directly so
//! we exercise the same path the binary does — argv parse, dispatch,
//! exit code. Filesystem fixtures live in `tempdir`s that survive
//! for the lifetime of one test.
//!
//! The v0.1 surface under test is:
//!
//! - The `--no-fetch` branch end-to-end: project-root resolution,
//!   config load, pom parse, lockfile read.
//! - The full-fetch branch's clean "not yet implemented" error path.
//! - Error surfaces (bad root, missing pom).
//!
//! Stdout/stderr are not redirected; we assert on exit codes and on
//! the in-process [`barista_cli::cmd::pull::run_inner`] helpers
//! where richer assertions are useful.

use std::fs;
use std::path::Path;

use barista_cli::cli::{Cli, dispatch};
use barista_lockfile::{Lockfile, LockfileEntry};
use clap::Parser;
use tempfile::TempDir;

// ---- fixture helpers ---------------------------------------------------

/// Minimal valid pom.xml: declares group/artifact/version, no parent,
/// no dependencies. Enough for `parse_pom` + (in the fetch path)
/// `resolve_pom` with a NullParentResolver.
const MINIMAL_POM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
  <packaging>jar</packaging>
</project>
"#;

fn write_minimal_pom(dir: &Path) {
    fs::write(dir.join("pom.xml"), MINIMAL_POM).unwrap();
}

/// Construct a one-entry lockfile suitable for round-trip in the
/// "lockfile exists" test cases. The contents are not asserted —
/// only the count and that the file was read without error.
fn write_sample_lockfile(dir: &Path) {
    let mut lf = Lockfile::new(
        "deadbeef".repeat(8), // 64-char hex
        "cafebabe".repeat(8),
    );
    lf.entries.push(LockfileEntry {
        coords: "org.example:lib".to_string(),
        version: "1.2.3".to_string(),
        scope: "compile".to_string(),
        optional: false,
        sha256: "0".repeat(64),
        sha1: None,
        size_bytes: 2048,
        source_url: "https://repo.maven.apache.org/maven2/org/example/lib/1.2.3/lib-1.2.3.jar"
            .to_string(),
        etag: None,
        last_modified: None,
        classifier: None,
        type_: "jar".to_string(),
        from_path: Vec::new(),
        depth: 0,
        snapshot_resolution: None,
        exclusions: Vec::new(),
    });
    lf.write(&dir.join("barista.lock")).unwrap();
}

fn run_dispatch(argv: &[&str]) -> i32 {
    let cli = Cli::try_parse_from(argv).expect("parse argv");
    dispatch(cli)
}

fn fresh_project(td: &TempDir) -> &Path {
    let root = td.path();
    write_minimal_pom(root);
    root
}

// ---- --no-fetch: lockfile absent ---------------------------------------

#[test]
fn no_fetch_without_lockfile_succeeds() {
    let td = tempfile::tempdir().unwrap();
    let root = fresh_project(&td);
    let code = run_dispatch(&[
        "barista",
        "--root",
        root.to_str().unwrap(),
        "pull",
        "--no-fetch",
    ]);
    assert_eq!(code, 0, "exit 0 on no-fetch with no lockfile");
    assert!(
        !root.join("barista.lock").exists(),
        "--no-fetch must not write a lockfile"
    );
}

// ---- --no-fetch: lockfile present --------------------------------------

#[test]
fn no_fetch_with_lockfile_succeeds() {
    let td = tempfile::tempdir().unwrap();
    let root = fresh_project(&td);
    write_sample_lockfile(root);
    let code = run_dispatch(&[
        "barista",
        "--root",
        root.to_str().unwrap(),
        "pull",
        "--no-fetch",
    ]);
    assert_eq!(code, 0, "exit 0 on no-fetch with existing lockfile");

    // Lockfile must be unchanged: re-read and check the seeded entry
    // is still there with its original coords. (write() is atomic;
    // any rewrite would change the timestamp in [meta], but the
    // entry payload must be preserved exactly.)
    let lf = Lockfile::read(&root.join("barista.lock")).unwrap();
    assert_eq!(lf.entries.len(), 1);
    assert_eq!(lf.entries[0].coords, "org.example:lib");
}

// ---- full-fetch path: returns NotYetImplemented ------------------------

#[test]
fn full_fetch_returns_not_yet_implemented() {
    let td = tempfile::tempdir().unwrap();
    let root = fresh_project(&td);
    let code = run_dispatch(&["barista", "--root", root.to_str().unwrap(), "pull"]);
    assert_eq!(
        code, 2,
        "exit 2 for the full-fetch path until M3.x cache wiring lands"
    );
}

// ---- bad project root --------------------------------------------------

#[test]
fn pull_with_bad_root_errors_cleanly() {
    let td = tempfile::tempdir().unwrap();
    let bad = td.path().join("does-not-exist");
    let code = run_dispatch(&[
        "barista",
        "--root",
        bad.to_str().unwrap(),
        "pull",
        "--no-fetch",
    ]);
    assert_eq!(code, 1, "exit 1 on a non-existent project root");
}

#[test]
fn pull_with_root_missing_pom_errors() {
    let td = tempfile::tempdir().unwrap();
    // Directory exists but has no pom.xml.
    let code = run_dispatch(&[
        "barista",
        "--root",
        td.path().to_str().unwrap(),
        "pull",
        "--no-fetch",
    ]);
    assert_eq!(code, 1, "exit 1 on a root with no pom.xml");
}

// ---- flag acceptance ---------------------------------------------------

#[test]
fn pull_accepts_update_flag() {
    let td = tempfile::tempdir().unwrap();
    let root = fresh_project(&td);
    // With --no-fetch the --update flag is a no-op for now, but it
    // must parse and dispatch cleanly.
    let code = run_dispatch(&[
        "barista",
        "--root",
        root.to_str().unwrap(),
        "pull",
        "--update",
        "--no-fetch",
    ]);
    assert_eq!(code, 0);
}

#[test]
fn pull_accepts_strict_flag() {
    let td = tempfile::tempdir().unwrap();
    let root = fresh_project(&td);
    let code = run_dispatch(&[
        "barista",
        "--strict",
        "--root",
        root.to_str().unwrap(),
        "pull",
        "--no-fetch",
    ]);
    assert_eq!(code, 0);
}

#[test]
fn pull_accepts_scope_flag() {
    let td = tempfile::tempdir().unwrap();
    let root = fresh_project(&td);
    let code = run_dispatch(&[
        "barista",
        "--root",
        root.to_str().unwrap(),
        "pull",
        "--scope",
        "test",
        "--no-fetch",
    ]);
    assert_eq!(code, 0);
}

#[test]
fn pull_accepts_explain_flag() {
    let td = tempfile::tempdir().unwrap();
    let root = fresh_project(&td);
    let code = run_dispatch(&[
        "barista",
        "--root",
        root.to_str().unwrap(),
        "pull",
        "--explain",
        "--no-fetch",
    ]);
    assert_eq!(code, 0);
}

// ---- --file alias for --root -------------------------------------------

#[test]
fn no_fetch_via_file_flag_succeeds() {
    let td = tempfile::tempdir().unwrap();
    let root = fresh_project(&td);
    let pom = root.join("pom.xml");
    let code = run_dispatch(&[
        "barista",
        "--file",
        pom.to_str().unwrap(),
        "pull",
        "--no-fetch",
    ]);
    assert_eq!(code, 0, "exit 0 when project is selected via -f");
}
