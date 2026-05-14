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

//! Integration tests for `barista grind` subcommands.
//!
//! These drive the CLI library's `dispatch` entry point directly, so
//! they don't shell out to the binary. Filesystem fixtures (project
//! roots, `pom.xml`, `barista.lock`) live in `tempdir`s that survive
//! for the lifetime of one test.
//!
//! Stdout/stderr from the dispatcher are not redirected here — `grind
//! tree` builds its rendered output into a `String` via the public
//! helpers in `barista_cli::cmd::grind::tree`, so we test those
//! renderers directly for content + snapshot the result. The exit
//! codes are asserted against the dispatcher.

use std::fs;
use std::path::Path;

use barista_cli::cli::{Cli, dispatch};
use barista_cli::cmd::grind::tree::{render_json, render_text};
use barista_lockfile::{Lockfile, LockfileEntry, ReactorEntry};
use clap::Parser;
use tempfile::TempDir;

// ---- fixture helpers ---------------------------------------------------

/// Create an empty `pom.xml` at `dir`.
fn touch_pom(dir: &Path) {
    fs::write(dir.join("pom.xml"), b"<project/>").unwrap();
}

fn sample_entry(coords: &str, version: &str, scope: &str) -> LockfileEntry {
    LockfileEntry {
        coords: coords.to_string(),
        version: version.to_string(),
        scope: scope.to_string(),
        optional: false,
        sha256: "0".repeat(64),
        sha1: None,
        size_bytes: 1024,
        source_url: format!("https://example.com/{}.jar", coords.replace(':', "/")),
        etag: None,
        last_modified: None,
        classifier: None,
        type_: "jar".to_string(),
        from_path: Vec::new(),
        depth: 0,
        snapshot_resolution: None,
        exclusions: Vec::new(),
    }
}

/// Build a small lockfile:
///   reactor:    com.example:app:1.0.0
///   direct:     org.apache.commons:commons-lang3:3.14.0   [compile]
///               com.fasterxml.jackson.core:jackson-core:2.18.0 [compile]
///               org.slf4j:slf4j-api:2.0.16 [compile]
///   transitive: org.slf4j:slf4j-jdk14:2.0.16 [runtime]
///               (from_path = ["org.slf4j:slf4j-api"])
fn three_entry_lockfile() -> Lockfile {
    let mut lf = Lockfile::new("a".repeat(64), "b".repeat(64));
    lf.reactor.push(ReactorEntry {
        coords: "com.example:app".to_string(),
        version: "1.0.0".to_string(),
        relative_path: "pom.xml".to_string(),
    });
    lf.entries.push(sample_entry(
        "org.apache.commons:commons-lang3",
        "3.14.0",
        "compile",
    ));
    lf.entries.push(sample_entry(
        "com.fasterxml.jackson.core:jackson-core",
        "2.18.0",
        "compile",
    ));
    lf.entries
        .push(sample_entry("org.slf4j:slf4j-api", "2.0.16", "compile"));
    let mut transitive = sample_entry("org.slf4j:slf4j-jdk14", "2.0.16", "runtime");
    transitive.from_path = vec!["org.slf4j:slf4j-api".to_string()];
    transitive.depth = 1;
    lf.entries.push(transitive);
    // Force the timestamps to a fixed value so snapshots don't drift.
    lf.meta.generated_at = "2026-05-13T00:00:00Z".to_string();
    lf.meta.generated_by = "barista 0.0.0-test".to_string();
    lf
}

/// Set up a project root with a pom + lockfile, return the tempdir.
fn project_with_lockfile(lf: &Lockfile) -> TempDir {
    let td = tempfile::tempdir().unwrap();
    touch_pom(td.path());
    lf.write(&td.path().join("barista.lock")).unwrap();
    td
}

// ---- direct renderer tests --------------------------------------------

#[test]
fn text_render_groups_direct_and_transitive_entries() {
    let lf = three_entry_lockfile();
    let text = render_text(&lf);
    insta::assert_snapshot!("tree_text_small", text);
}

#[test]
fn json_render_emits_documented_shape() {
    let lf = three_entry_lockfile();
    let json = render_json(&lf).expect("json render");

    // Snapshot
    insta::assert_snapshot!("tree_json_small", &json);

    // Parse + assert the documented shape.
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert!(v["nodes"].is_array());
    assert!(v["reactor"].is_array());
    let nodes = v["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 4);
    for node in nodes {
        for k in ["coords", "version", "scope", "depth", "from_path"] {
            assert!(node.get(k).is_some(), "node missing key `{k}`: {node}");
        }
    }
}

#[test]
fn text_render_without_reactor_uses_placeholder_root() {
    // No reactor entries; render should still produce something
    // sensible and include the direct + transitive deps.
    let mut lf = three_entry_lockfile();
    lf.reactor.clear();
    let text = render_text(&lf);
    assert!(text.contains("(no reactor)"), "got:\n{text}");
    assert!(text.contains("org.apache.commons:commons-lang3"));
    assert!(text.contains("org.slf4j:slf4j-jdk14"));
}

#[test]
fn text_render_surfaces_orphan_transitives() {
    // A transitive entry whose `from_path` does not match any
    // entry in the lockfile must still appear in the output —
    // we render it under "(orphan transitives)".
    let mut lf = three_entry_lockfile();
    let mut orphan = sample_entry("org.example:orphan", "9.9.9", "compile");
    orphan.from_path = vec!["does.not:exist".to_string()];
    orphan.depth = 1;
    lf.entries.push(orphan);
    let text = render_text(&lf);
    assert!(
        text.contains("(orphan transitives)"),
        "expected orphan heading; got:\n{text}",
    );
    assert!(text.contains("org.example:orphan"));
}

// ---- dispatcher exit-code tests ---------------------------------------

/// Drive the dispatcher with a synthetic argv, scoped to a project
/// root we've prepared. Returns the exit code.
fn run_dispatch(argv: &[&str]) -> i32 {
    let cli = Cli::try_parse_from(argv).expect("parse argv");
    dispatch(cli)
}

#[test]
fn grind_tree_without_lockfile_returns_1() {
    let td = tempfile::tempdir().unwrap();
    touch_pom(td.path());
    let root = td.path().to_string_lossy().into_owned();
    let code = run_dispatch(&["barista", "--root", &root, "grind", "tree"]);
    assert_eq!(code, 1);
}

#[test]
fn grind_tree_with_lockfile_returns_0() {
    let lf = three_entry_lockfile();
    let td = project_with_lockfile(&lf);
    let root = td.path().to_string_lossy().into_owned();
    let code = run_dispatch(&["barista", "--root", &root, "grind", "tree"]);
    assert_eq!(code, 0);
}

#[test]
fn grind_tree_json_format_returns_0() {
    let lf = three_entry_lockfile();
    let td = project_with_lockfile(&lf);
    let root = td.path().to_string_lossy().into_owned();
    let code = run_dispatch(&[
        "barista", "--root", &root, "grind", "tree", "--format", "json",
    ]);
    assert_eq!(code, 0);
}

#[test]
fn grind_diff_is_not_yet_implemented() {
    let code = run_dispatch(&["barista", "grind", "diff"]);
    assert_eq!(code, 2);
}

#[test]
fn grind_audit_is_not_yet_implemented() {
    let code = run_dispatch(&["barista", "grind", "audit"]);
    assert_eq!(code, 2);
}

#[test]
fn grind_why_is_not_yet_implemented() {
    let code = run_dispatch(&["barista", "grind", "why", "org.example:foo"]);
    assert_eq!(code, 2);
}
