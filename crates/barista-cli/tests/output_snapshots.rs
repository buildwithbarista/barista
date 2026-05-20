// SPDX-License-Identifier: MIT OR Apache-2.0

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

//! Cross-product output snapshot matrix (M3.2 T5).
//!
//! [T] linkage: M3.2 T5 — pin every (command × output-format)
//! combination to a byte-deterministic snapshot. Each cell is double-
//! pinned: an `insta` snapshot catches any unintended byte-level
//! change, and the same output is validated against the published
//! schema in `schema/output/v1/`. T5 closes the loop opened by T1
//! (renderer plumbing), T2 (schemas), and T3 (NDJSON streaming).
//!
//! # Matrix
//!
//! |               | human | json (pretty) | json (compact) | ndjson           |
//! |---------------|-------|---------------|----------------|------------------|
//! | `pull`        |   ✓   |       ✓       |        ✓       | result + stream  |
//! | `grind tree`  |   ✓   |       ✓       |        ✓       | result line      |
//! | `pour`        |   ✓   |       ✓       |        ✓       | result line      |
//!
//! That's 12 cells, plus one extra snapshot for the full `pull` NDJSON
//! progress-event stream (started → resolving → cached × 5 → completed
//! → result) — 13 snapshot tests minimum.
//!
//! # Conventions
//!
//! - Sample reports use stable, fake paths (`/proj`, `/m2`) so no
//!   per-host detail (tempdir names, PIDs, real cwds) sneaks into the
//!   pinned bytes.
//! - NDJSON renderers use [`NdjsonRenderer::with_fixed_timestamp`] so
//!   timestamps are deterministic.
//! - Snapshots live under `tests/snapshots/output_snapshots__*.snap`;
//!   the snapshot names match the test function names.
//! - Existing snapshots owned by `output_renderer.rs`,
//!   `renderer_schema_e2e.rs`, `cmd_*` tests are intentionally
//!   untouched — T5 adds a new matrix, it does not consolidate the
//!   per-renderer or per-command suites.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use barista_cli::output::{
    GrindTreeReport, HumanRenderer, JsonRenderer, LockfileStatus, NdjsonRenderer, PourReport,
    PullReport, ReactorModule, Renderer, TreeNode,
};
use jsonschema::Validator;
use serde_json::Value;

// =====================================================================
// Shared `Write` buffer (same pattern as tests/output_renderer.rs).
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
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
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

// =====================================================================
// Schema loader. Mirrors the helper in renderer_schema_e2e.rs; copied
// inline so test files stay independent (Rust integration tests can't
// share code without a `tests/common/` module + `mod` declaration).
// =====================================================================

fn schema_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("schema")
        .join("output")
        .join("v1")
        .join(name)
}

fn load(name: &str) -> Validator {
    let path = schema_path(name);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()));
    let schema: Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("parse {} as JSON failed: {e}", path.display()));
    jsonschema::draft202012::new(&schema)
        .unwrap_or_else(|e| panic!("compile {}: {e}", path.display()))
}

#[track_caller]
fn assert_valid(validator: &Validator, doc: &Value, label: &str) {
    if let Err(error) = validator.validate(doc) {
        panic!(
            "[{label}] expected output to validate, but got:\n  {error}\nDocument:\n{}",
            serde_json::to_string_pretty(doc).unwrap()
        );
    }
}

// =====================================================================
// Sample reports.
//
// One canonical builder per command. Every matrix cell renders the
// **same** sample, so the snapshot deltas between cells reflect only
// format differences. Values are stable, fake, and chosen to exercise
// the interesting schema-level fields (optional `project-signature`,
// non-empty `from-path`, multi-element `planned-paths`).
// =====================================================================

const FIXED_TS: &str = "2026-05-14T12:34:56.789Z";

fn sample_pull_report() -> PullReport {
    PullReport {
        project_root: PathBuf::from("/proj"),
        lockfile_status: LockfileStatus::Unchanged,
        entries: 5,
        fetched: 0,
        project_signature: Some("deadbeefcafe".to_string()),
        coords: Some("com.example:demo:1.0.0".to_string()),
        no_fetch: true,
        strict: false,
    }
}

fn sample_grind_tree_report() -> GrindTreeReport {
    GrindTreeReport {
        schema_version: 1,
        reactor: vec![ReactorModule {
            coords: "com.example:demo".to_string(),
            version: "1.0.0".to_string(),
            relative_path: "pom.xml".to_string(),
        }],
        nodes: vec![
            TreeNode {
                coords: "org.slf4j:slf4j-api".to_string(),
                version: "2.0.13".to_string(),
                scope: "compile".to_string(),
                depth: 1,
                from_path: vec![],
            },
            TreeNode {
                coords: "ch.qos.logback:logback-classic".to_string(),
                version: "1.5.6".to_string(),
                scope: "compile".to_string(),
                depth: 2,
                from_path: vec!["org.slf4j:slf4j-api".to_string()],
            },
        ],
    }
}

fn sample_pour_report() -> PourReport {
    PourReport {
        target: PathBuf::from("/m2"),
        scope: "compile".to_string(),
        considered: 3,
        planned: 2,
        materialized: 2,
        dry_run: false,
        planned_paths: vec![
            PathBuf::from("/m2/com/example/a/1.0/a-1.0.jar"),
            PathBuf::from("/m2/com/example/b/2.0/b-2.0.jar"),
        ],
    }
}

/// The five coordinates used by the streaming-NDJSON `pull` snapshot.
/// Kept short (5) so the snapshot stays readable while still hitting
/// the hot-loop path (`emit_cached` once per coord).
const STREAM_COORDS: &[&str] = &[
    "org.slf4j:slf4j-api:2.0.13",
    "ch.qos.logback:logback-core:1.5.6",
    "ch.qos.logback:logback-classic:1.5.6",
    "com.fasterxml.jackson.core:jackson-databind:2.17.1",
    "org.apache.commons:commons-lang3:3.14.0",
];

// =====================================================================
// 1. `pull` row.
// =====================================================================

#[test]
fn pull_human_snapshot() {
    // The human renderer writes pull to stderr (informational). We
    // snapshot the stderr stream and assert stdout stays empty.
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut r = HumanRenderer::new(stdout.writer(), stderr.writer(), /* ansi */ false);
    r.render_pull(&sample_pull_report()).unwrap();
    assert!(
        stdout.as_string().is_empty(),
        "human pull must not touch stdout"
    );
    insta::assert_snapshot!("pull_human", stderr.as_string());
}

#[test]
fn pull_json_pretty_snapshot() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ true);
    r.render_pull(&sample_pull_report()).unwrap();
    let body = stdout.as_string();
    let doc: Value = serde_json::from_str(&body).expect("pretty pull is valid JSON");
    assert_valid(&load("pull.json"), &doc, "pull json (pretty)");
    insta::assert_snapshot!("pull_json_pretty", body);
}

#[test]
fn pull_json_compact_snapshot() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ false);
    r.render_pull(&sample_pull_report()).unwrap();
    let body = stdout.as_string();
    // Compact JSON has exactly one newline (the trailing one).
    assert_eq!(body.matches('\n').count(), 1);
    let doc: Value = serde_json::from_str(&body).expect("compact pull is valid JSON");
    assert_valid(&load("pull.json"), &doc, "pull json (compact)");
    insta::assert_snapshot!("pull_json_compact", body);
}

#[test]
fn pull_ndjson_result_snapshot() {
    // Single-line NDJSON result envelope. Distinct from the
    // streaming snapshot below (which interleaves progress events).
    let stdout = SharedBuf::new();
    let mut r = NdjsonRenderer::with_fixed_timestamp(stdout.writer(), FIXED_TS);
    r.render_pull(&sample_pull_report()).unwrap();
    let body = stdout.as_string();
    let env: Value = serde_json::from_str(body.trim_end()).expect("ndjson line is JSON");
    assert_valid(&load("progress-event.json"), &env, "pull ndjson envelope");
    assert_valid(&load("pull.json"), &env["payload"], "pull ndjson payload");
    insta::assert_snapshot!("pull_ndjson_result", body);
}

#[test]
fn pull_ndjson_stream_snapshot() {
    // Full streaming-progress sequence for a `pull --no-fetch` run
    // over 5 cached coordinates. Mirrors the production wiring in
    // `cmd::pull::run` for the `--no-fetch` path:
    //
    //   emit_started("pull")
    //   emit_resolving(None, None)
    //   emit_cached(coord)  // once per lockfile entry
    //   emit_completed("pull")
    //   render_pull(report)
    //
    // Every emitted line is validated against the progress-event
    // schema; the final `result` line's payload also validates
    // against `pull.json`.
    let stdout = SharedBuf::new();
    let mut r = NdjsonRenderer::with_fixed_timestamp(stdout.writer(), FIXED_TS);
    r.emit_started("pull").unwrap();
    r.emit_resolving(None, None).unwrap();
    for c in STREAM_COORDS {
        r.emit_cached(c).unwrap();
    }
    r.emit_completed("pull").unwrap();
    r.render_pull(&sample_pull_report()).unwrap();

    let body = stdout.as_string();
    let envelope = load("progress-event.json");
    let pull_schema = load("pull.json");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(
        lines.len(),
        // started + resolving + 5×cached + completed + result
        1 + 1 + STREAM_COORDS.len() + 1 + 1,
        "unexpected line count:\n{body}"
    );
    for (i, line) in lines.iter().enumerate() {
        let env: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {i} parse failed: {e}\nline: {line}"));
        assert_valid(&envelope, &env, &format!("stream line {i}"));
    }
    // Sanity-check the result line's payload against pull.json.
    let last: Value = serde_json::from_str(lines.last().unwrap()).unwrap();
    assert_eq!(last["event"], "result");
    assert_valid(&pull_schema, &last["payload"], "stream result payload");

    insta::assert_snapshot!("pull_ndjson_stream", body);
}

// =====================================================================
// 2. `grind tree` row.
// =====================================================================

#[test]
fn grind_tree_human_snapshot() {
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut r = HumanRenderer::new(stdout.writer(), stderr.writer(), /* ansi */ false);
    r.render_grind_tree(&sample_grind_tree_report()).unwrap();
    assert!(
        stderr.as_string().is_empty(),
        "human grind tree must not touch stderr"
    );
    insta::assert_snapshot!("grind_tree_human", stdout.as_string());
}

#[test]
fn grind_tree_json_pretty_snapshot() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ true);
    r.render_grind_tree(&sample_grind_tree_report()).unwrap();
    let body = stdout.as_string();
    let doc: Value = serde_json::from_str(&body).expect("pretty grind-tree is valid JSON");
    assert_valid(&load("grind-tree.json"), &doc, "grind-tree json (pretty)");
    insta::assert_snapshot!("grind_tree_json_pretty", body);
}

#[test]
fn grind_tree_json_compact_snapshot() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ false);
    r.render_grind_tree(&sample_grind_tree_report()).unwrap();
    let body = stdout.as_string();
    assert_eq!(body.matches('\n').count(), 1);
    let doc: Value = serde_json::from_str(&body).expect("compact grind-tree is valid JSON");
    assert_valid(&load("grind-tree.json"), &doc, "grind-tree json (compact)");
    insta::assert_snapshot!("grind_tree_json_compact", body);
}

#[test]
fn grind_tree_ndjson_result_snapshot() {
    let stdout = SharedBuf::new();
    let mut r = NdjsonRenderer::with_fixed_timestamp(stdout.writer(), FIXED_TS);
    r.render_grind_tree(&sample_grind_tree_report()).unwrap();
    let body = stdout.as_string();
    let env: Value = serde_json::from_str(body.trim_end()).expect("ndjson line is JSON");
    assert_valid(
        &load("progress-event.json"),
        &env,
        "grind-tree ndjson envelope",
    );
    assert_valid(
        &load("grind-tree.json"),
        &env["payload"],
        "grind-tree ndjson payload",
    );
    insta::assert_snapshot!("grind_tree_ndjson_result", body);
}

// =====================================================================
// 3. `pour` row.
// =====================================================================

#[test]
fn pour_human_snapshot() {
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut r = HumanRenderer::new(stdout.writer(), stderr.writer(), /* ansi */ false);
    r.render_pour(&sample_pour_report()).unwrap();
    assert!(
        stdout.as_string().is_empty(),
        "human pour must not touch stdout"
    );
    insta::assert_snapshot!("pour_human", stderr.as_string());
}

#[test]
fn pour_json_pretty_snapshot() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ true);
    r.render_pour(&sample_pour_report()).unwrap();
    let body = stdout.as_string();
    let doc: Value = serde_json::from_str(&body).expect("pretty pour is valid JSON");
    assert_valid(&load("pour.json"), &doc, "pour json (pretty)");
    insta::assert_snapshot!("pour_json_pretty", body);
}

#[test]
fn pour_json_compact_snapshot() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ false);
    r.render_pour(&sample_pour_report()).unwrap();
    let body = stdout.as_string();
    assert_eq!(body.matches('\n').count(), 1);
    let doc: Value = serde_json::from_str(&body).expect("compact pour is valid JSON");
    assert_valid(&load("pour.json"), &doc, "pour json (compact)");
    insta::assert_snapshot!("pour_json_compact", body);
}

#[test]
fn pour_ndjson_result_snapshot() {
    let stdout = SharedBuf::new();
    let mut r = NdjsonRenderer::with_fixed_timestamp(stdout.writer(), FIXED_TS);
    r.render_pour(&sample_pour_report()).unwrap();
    let body = stdout.as_string();
    let env: Value = serde_json::from_str(body.trim_end()).expect("ndjson line is JSON");
    assert_valid(&load("progress-event.json"), &env, "pour ndjson envelope");
    assert_valid(&load("pour.json"), &env["payload"], "pour ndjson payload");
    insta::assert_snapshot!("pour_ndjson_result", body);
}

// =====================================================================
// 4. Determinism guards.
//
// The byte-determinism story (no PIDs / cwds / real timestamps in
// snapshot output) is only as strong as the discipline that pins it.
// These two tests catch the most likely regressions: a wall-clock
// timestamp leaking into NDJSON, or a stray newline / suffix in the
// matrix output. Both run the same renderer twice with identical
// inputs and assert byte-identical outputs.
// =====================================================================

#[test]
fn matrix_outputs_are_byte_stable_across_runs() {
    // Run every cell twice, byte-compare. If anything pulls in
    // wall-clock state or a HashMap iteration order, we'll see a
    // diff here long before the snapshot files start flaking.
    fn json_compact(report: impl FnOnce(&mut JsonRenderer)) -> Vec<u8> {
        let buf = SharedBuf::new();
        let mut r = JsonRenderer::new(buf.writer(), false);
        report(&mut r);
        buf.bytes()
    }
    let a = json_compact(|r| r.render_pull(&sample_pull_report()).unwrap());
    let b = json_compact(|r| r.render_pull(&sample_pull_report()).unwrap());
    assert_eq!(a, b, "pull json compact is not byte-stable");

    let a = json_compact(|r| r.render_grind_tree(&sample_grind_tree_report()).unwrap());
    let b = json_compact(|r| r.render_grind_tree(&sample_grind_tree_report()).unwrap());
    assert_eq!(a, b, "grind-tree json compact is not byte-stable");

    let a = json_compact(|r| r.render_pour(&sample_pour_report()).unwrap());
    let b = json_compact(|r| r.render_pour(&sample_pour_report()).unwrap());
    assert_eq!(a, b, "pour json compact is not byte-stable");
}

#[test]
fn ndjson_fixed_timestamp_is_byte_stable() {
    fn render() -> Vec<u8> {
        let buf = SharedBuf::new();
        let mut r = NdjsonRenderer::with_fixed_timestamp(buf.writer(), FIXED_TS);
        r.emit_started("pull").unwrap();
        for c in STREAM_COORDS {
            r.emit_cached(c).unwrap();
        }
        r.emit_completed("pull").unwrap();
        r.render_pull(&sample_pull_report()).unwrap();
        buf.bytes()
    }
    let a = render();
    let b = render();
    assert_eq!(a, b, "ndjson stream is not byte-stable");
    // And every line must carry exactly the fixed timestamp.
    let body = String::from_utf8(a).unwrap();
    for line in body.lines() {
        assert!(
            line.contains(FIXED_TS),
            "line missing fixed timestamp: {line}"
        );
    }
}
