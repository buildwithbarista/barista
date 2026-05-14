//! Integration tests for the multi-format output renderer
//! (M3.2 T1).
//!
//! These exercise each renderer directly with in-memory buffers
//! rather than spawning the binary. The factory + each renderer are
//! tested end-to-end on the report types they consume; the
//! command-level wiring is covered by the existing
//! `cmd_{pull,grind,pour}` test files.
//!
//! [T] linkage: M3.2 T1 contributes the renderer plumbing referenced
//! by the acceptance criterion "every output JSON validates against
//! its schema" (covered fully in T5). This file pins the byte-level
//! shape of the JSON / NDJSON / human surfaces so T2 (schema
//! generator) and T5 (cross-format snapshot tests) have a stable
//! target.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use barista_cli::cli::OutputFormat;
use barista_cli::output::{
    GrindTreeReport, HumanRenderer, JsonRenderer, LockfileStatus, NdjsonRenderer, PourReport,
    PullReport, ReactorModule, Renderer, TreeNode, make_renderer,
};

// ============================================================
// shared in-memory writer
// ============================================================

/// Thread-safe byte buffer wired into a renderer via a `Write` impl,
/// while still allowing the test to peek at what was written.
///
/// The renderer takes `Box<dyn Write + Send>`, so we hand it a
/// [`BufWriter`] (which holds the same [`Arc<Mutex<Vec<u8>>>`]
/// the test inspects).
#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    fn writer(&self) -> Box<dyn Write + Send> {
        Box::new(BufWriter(self.0.clone()))
    }

    /// Snapshot the accumulated bytes as a `String`.
    fn as_string(&self) -> String {
        let guard = self.0.lock().unwrap();
        String::from_utf8(guard.clone()).expect("renderer wrote non-UTF8")
    }
}

struct BufWriter(Arc<Mutex<Vec<u8>>>);

impl Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut g = self.0.lock().unwrap();
        g.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ============================================================
// fixture builders
// ============================================================

fn sample_pull_report() -> PullReport {
    PullReport {
        project_root: PathBuf::from("/proj"),
        lockfile_status: LockfileStatus::Unchanged,
        entries: 142,
        fetched: 0,
        project_signature: Some("deadbeefdead".to_string()),
        coords: Some("com.example:demo:1.0.0".to_string()),
        no_fetch: true,
        strict: false,
    }
}

fn sample_grind_tree_report() -> GrindTreeReport {
    GrindTreeReport {
        schema_version: 1,
        reactor: vec![ReactorModule {
            coords: "com.example:app".to_string(),
            version: "1.0.0".to_string(),
            relative_path: "pom.xml".to_string(),
        }],
        nodes: vec![
            TreeNode {
                coords: "org.apache.commons:commons-lang3".to_string(),
                version: "3.14.0".to_string(),
                scope: "compile".to_string(),
                depth: 0,
                from_path: Vec::new(),
            },
            TreeNode {
                coords: "org.slf4j:slf4j-api".to_string(),
                version: "2.0.16".to_string(),
                scope: "compile".to_string(),
                depth: 0,
                from_path: Vec::new(),
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
            PathBuf::from("/m2/g/a/1.0/a-1.0.jar"),
            PathBuf::from("/m2/g/b/1.0/b-1.0.jar"),
        ],
    }
}

#[derive(Debug, thiserror::Error)]
#[error("boom: {0}")]
struct SampleError(&'static str);

// ============================================================
// 1. Human renderer — ANSI on / ANSI off
// ============================================================

#[test]
fn human_renderer_grind_tree_writes_to_stdout() {
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut r = HumanRenderer::new(stdout.writer(), stderr.writer(), /* ansi */ false);
    r.render_grind_tree(&sample_grind_tree_report()).unwrap();
    let out = stdout.as_string();
    let err = stderr.as_string();
    assert!(out.contains("com.example:app:1.0.0"), "got: {out}");
    assert!(out.contains("org.apache.commons:commons-lang3"));
    assert!(out.contains("org.slf4j:slf4j-api"));
    assert!(out.ends_with('\n'));
    assert!(err.is_empty(), "stderr should be empty for tree text");
}

#[test]
fn human_renderer_ansi_flag_is_observable() {
    // The v0.1 human output is plain text — `ansi` is stored but
    // not styled — so this test just pins the constructor wiring.
    // Without it, later ANSI work could silently drop the flag.
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let r_on = HumanRenderer::new(stdout.writer(), stderr.writer(), true);
    let r_off = HumanRenderer::new(stdout.writer(), stderr.writer(), false);
    assert!(r_on.ansi());
    assert!(!r_off.ansi());
}

#[test]
fn human_renderer_pull_writes_to_stderr() {
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut r = HumanRenderer::new(stdout.writer(), stderr.writer(), false);
    r.render_pull(&sample_pull_report()).unwrap();
    let err = stderr.as_string();
    let out = stdout.as_string();
    assert!(err.starts_with("pull: "), "got stderr: {err}");
    assert!(err.contains("com.example:demo:1.0.0"));
    assert!(err.ends_with('\n'));
    assert!(out.is_empty(), "stdout should be empty for human pull");
}

#[test]
fn human_renderer_pour_writes_to_stderr() {
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut r = HumanRenderer::new(stdout.writer(), stderr.writer(), false);
    r.render_pour(&sample_pour_report()).unwrap();
    let err = stderr.as_string();
    let out = stdout.as_string();
    assert!(err.starts_with("pour: "), "got stderr: {err}");
    assert!(err.contains("2 of 3 entries (scope=compile)"));
    assert!(out.is_empty());
}

#[test]
fn human_renderer_error_writes_to_stderr() {
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut r = HumanRenderer::new(stdout.writer(), stderr.writer(), false);
    r.render_error(&SampleError("kaboom")).unwrap();
    let err = stderr.as_string();
    assert_eq!(err, "error: boom: kaboom\n");
}

// ============================================================
// 2. JSON renderer — pretty vs compact
// ============================================================

#[test]
fn json_renderer_pretty_emits_indented_document() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ true);
    r.render_pull(&sample_pull_report()).unwrap();
    let body = stdout.as_string();
    // Pretty-printed JSON contains a newline between fields.
    assert!(body.contains("\n  \""), "expected indentation; got:\n{body}");
    // serde_json::to_writer_pretty does not append a newline; the
    // renderer does. Confirm exactly one trailing newline.
    assert!(body.ends_with('\n'));
    assert!(!body.ends_with("\n\n"), "expected exactly one trailing newline");
    // Round-trip.
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["command"], "pull");
    assert_eq!(v["entries"], 142);
    assert_eq!(v["no-fetch"], true);
    assert_eq!(v["strict"], false);
    assert_eq!(v["lockfile-status"], "unchanged");
    assert_eq!(v["project-signature"], "deadbeefdead");
}

#[test]
fn json_renderer_compact_emits_single_line() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ false);
    r.render_pull(&sample_pull_report()).unwrap();
    let body = stdout.as_string();
    // Compact = exactly one newline (the trailing one).
    assert_eq!(
        body.matches('\n').count(),
        1,
        "expected single-line compact JSON, got:\n{body}"
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["command"], "pull");
}

#[test]
fn json_renderer_emits_pour_report() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), false);
    r.render_pour(&sample_pour_report()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout.as_string()).unwrap();
    assert_eq!(v["command"], "pour");
    assert_eq!(v["scope"], "compile");
    assert_eq!(v["considered"], 3);
    assert_eq!(v["planned"], 2);
    assert_eq!(v["materialized"], 2);
    assert_eq!(v["dry-run"], false);
}

#[test]
fn json_renderer_emits_grind_tree_report() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), false);
    r.render_grind_tree(&sample_grind_tree_report()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout.as_string()).unwrap();
    assert_eq!(v["command"], "grind-tree");
    assert_eq!(v["schema-version"], 1);
    let nodes = v["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 2);
    for n in nodes {
        for k in ["coords", "version", "scope", "depth", "from-path"] {
            assert!(n.get(k).is_some(), "missing key `{k}` in node: {n}");
        }
    }
}

#[test]
fn json_renderer_emits_single_error_document() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), false);
    r.render_error(&SampleError("oh-no")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout.as_string()).unwrap();
    assert_eq!(v["command"], "error");
    assert_eq!(v["message"], "boom: oh-no");
}

#[test]
fn json_renderer_rejects_double_render() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), false);
    r.render_pull(&sample_pull_report()).unwrap();
    let err = r.render_pull(&sample_pull_report()).expect_err("must reject");
    assert!(
        format!("{err}").contains("already emitted"),
        "expected double-emit error, got: {err}"
    );
}

// ============================================================
// 3. NDJSON renderer — one line per render_* call
// ============================================================

#[test]
fn ndjson_renderer_emits_one_line_per_call() {
    let stdout = SharedBuf::new();
    let mut r = NdjsonRenderer::new(stdout.writer());
    r.render_pull(&sample_pull_report()).unwrap();
    r.render_grind_tree(&sample_grind_tree_report()).unwrap();
    r.render_pour(&sample_pour_report()).unwrap();
    r.render_error(&SampleError("ndjson-err")).unwrap();
    let body = stdout.as_string();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 4, "expected 4 events, got: {body}");

    // Each line is a valid JSON document.
    for (i, line) in lines.iter().enumerate() {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {i} parse failed: {e}\nline: {line}"));
        assert!(v.is_object(), "line {i} is not an object: {line}");
    }

    // Every emitted line carries an RFC 3339 millis timestamp and
    // a `payload` (matches schema/output/v1/progress-event.json).
    let v0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v0["event"], "result");
    assert_eq!(v0["payload"]["command"], "pull");
    assert!(v0["timestamp"].is_string(), "timestamp missing: {lines:?}");

    let v1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(v1["event"], "result");
    assert_eq!(v1["payload"]["command"], "grind-tree");
    assert!(v1["timestamp"].is_string());

    let v2: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(v2["event"], "result");
    assert_eq!(v2["payload"]["command"], "pour");
    assert!(v2["timestamp"].is_string());

    let v3: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
    assert_eq!(v3["event"], "error");
    assert_eq!(v3["payload"]["message"], "boom: ndjson-err");
    assert!(v3["timestamp"].is_string());
}

#[test]
fn ndjson_renderer_is_compact() {
    let stdout = SharedBuf::new();
    let mut r = NdjsonRenderer::new(stdout.writer());
    r.render_pull(&sample_pull_report()).unwrap();
    let body = stdout.as_string();
    // Exactly one newline (line terminator); no internal indentation.
    assert_eq!(body.matches('\n').count(), 1);
    assert!(!body.contains("  \""), "ndjson must not be pretty-printed");
}

// ============================================================
// 4. Round-trips through serde
// ============================================================

#[test]
fn pull_report_roundtrips_through_serde() {
    let r = sample_pull_report();
    let s = serde_json::to_string(&r).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["command"], "pull");
    assert_eq!(v["entries"], 142);
    assert_eq!(v["project-root"], "/proj");
}

#[test]
fn pour_report_roundtrips_through_serde() {
    let r = sample_pour_report();
    let s = serde_json::to_string(&r).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["command"], "pour");
    assert_eq!(v["planned-paths"].as_array().unwrap().len(), 2);
}

#[test]
fn grind_tree_report_roundtrips_through_serde() {
    let r = sample_grind_tree_report();
    let s = serde_json::to_string(&r).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["command"], "grind-tree");
    assert_eq!(v["nodes"].as_array().unwrap().len(), 2);
}

// ============================================================
// 5. Factory dispatch
// ============================================================

#[test]
fn factory_dispatches_to_correct_renderer() {
    // Human: pull goes to stderr (not the writer we hand the
    // factory), so the factory's `stdout` argument should stay
    // empty after `render_pull`.
    {
        let stdout = SharedBuf::new();
        let mut r = make_renderer(OutputFormat::Human, stdout.writer(), false);
        r.render_pull(&sample_pull_report()).unwrap();
        assert!(
            stdout.as_string().is_empty(),
            "Human::render_pull must not touch stdout; got: {}",
            stdout.as_string()
        );
    }

    // JSON: pull writes a single JSON document to stdout.
    {
        let stdout = SharedBuf::new();
        let mut r = make_renderer(OutputFormat::Json, stdout.writer(), false);
        r.render_pull(&sample_pull_report()).unwrap();
        let body = stdout.as_string();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["command"], "pull");
    }

    // NDJSON: pull writes a single NDJSON line with event=result.
    {
        let stdout = SharedBuf::new();
        let mut r = make_renderer(OutputFormat::Ndjson, stdout.writer(), false);
        r.render_pull(&sample_pull_report()).unwrap();
        let body = stdout.as_string();
        assert_eq!(body.matches('\n').count(), 1);
        let v: serde_json::Value = serde_json::from_str(body.trim_end()).unwrap();
        assert_eq!(v["event"], "result");
    }
}

#[test]
fn factory_finish_flushes_and_consumes_renderer() {
    let stdout = SharedBuf::new();
    let mut r = make_renderer(OutputFormat::Json, stdout.writer(), false);
    r.render_pull(&sample_pull_report()).unwrap();
    r.finish().expect("flush ok");
}

// ============================================================
// 6. insta snapshots — pin the sample shape
// ============================================================

#[test]
fn snapshot_pull_json_pretty() {
    let stdout = SharedBuf::new();
    let mut r = JsonRenderer::new(stdout.writer(), /* pretty */ true);
    r.render_pull(&sample_pull_report()).unwrap();
    insta::assert_snapshot!("pull_json_pretty", stdout.as_string());
}

#[test]
fn snapshot_pour_ndjson_result_line() {
    let stdout = SharedBuf::new();
    // Pin the clock so the snapshot bytes are deterministic.
    let mut r = NdjsonRenderer::with_fixed_timestamp(
        stdout.writer(),
        "2026-05-14T12:34:56.789Z",
    );
    r.render_pour(&sample_pour_report()).unwrap();
    insta::assert_snapshot!("pour_ndjson_result_line", stdout.as_string());
}

#[test]
fn snapshot_grind_tree_human_text() {
    // Pin the human-renderer's tree text against a sample report.
    // This is the source of truth for the new pipeline; the legacy
    // `cmd::grind::tree::render_text` snapshot still tracks the
    // byte-for-byte stability of the M3.1 contract.
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut r = HumanRenderer::new(stdout.writer(), stderr.writer(), false);
    r.render_grind_tree(&sample_grind_tree_report()).unwrap();
    insta::assert_snapshot!("grind_tree_human_text", stdout.as_string());
}
