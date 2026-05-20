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

//! End-to-end tests that feed the actual renderer output through
//! the published JSON schemas in `schema/output/v1/`.
//!
//! `tests/output_schema_validation.rs` validates **hand-written** sample
//! documents against the schemas; this test validates **what the real
//! renderer actually emits**. Together they pin both ends of the
//! contract: the schema is what we publish, the renderer is what we
//! emit, and these tests assert they agree.
//!
//! This is the gate for M3.2 acceptance criterion
//! `[T] Every output JSON validates against its schema`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use barista_cli::output::report::{
    GrindTreeReport, LockfileStatus, PourReport, PullReport, ReactorModule, TreeNode,
};
use barista_cli::output::{JsonRenderer, NdjsonRenderer, Renderer};
use jsonschema::Validator;
use serde_json::Value;

// ---------------------------------------------------------------------
// Shared buffer for capturing renderer output. The renderer wants a
// `Box<dyn Write + Send>`; we wrap an `Arc<Mutex<Vec<u8>>>` so the
// outer test can still read after the renderer is dropped.
// ---------------------------------------------------------------------

#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
    fn writer(&self) -> Box<dyn Write + Send> {
        Box::new(BufWriter(self.0.clone()))
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

// ---------------------------------------------------------------------
// schema loading helpers (mirror tests/output_schema_validation.rs)
// ---------------------------------------------------------------------

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
            "[{label}] expected renderer output to validate, but got:\n  {error}\nDocument:\n{}",
            serde_json::to_string_pretty(doc).unwrap()
        );
    }
}

// ---------------------------------------------------------------------
// Sample reports — built from the public report types so any
// rename/restructure breaks the test at compile time.
// ---------------------------------------------------------------------

fn sample_pull_report() -> PullReport {
    PullReport {
        project_root: PathBuf::from("/tmp/proj"),
        lockfile_status: LockfileStatus::Absent,
        entries: 142,
        fetched: 0,
        project_signature: Some("ab12cd34".into()),
        coords: Some("com.example:demo:1.0.0".into()),
        no_fetch: true,
        strict: false,
    }
}

fn sample_grind_tree_report() -> GrindTreeReport {
    GrindTreeReport {
        schema_version: 1,
        reactor: vec![ReactorModule {
            coords: "com.example:demo".into(),
            version: "1.0.0".into(),
            relative_path: "".into(),
        }],
        nodes: vec![
            TreeNode {
                coords: "org.slf4j:slf4j-api".into(),
                version: "2.0.13".into(),
                scope: "compile".into(),
                depth: 1,
                from_path: vec![],
            },
            TreeNode {
                coords: "ch.qos.logback:logback-classic".into(),
                version: "1.5.6".into(),
                scope: "compile".into(),
                depth: 2,
                from_path: vec!["org.slf4j:slf4j-api".into()],
            },
        ],
    }
}

fn sample_pour_report() -> PourReport {
    PourReport {
        target: PathBuf::from("/tmp/m2"),
        scope: "compile".into(),
        considered: 3,
        planned: 2,
        materialized: 2,
        dry_run: false,
        planned_paths: vec![
            PathBuf::from("/tmp/m2/a/1.0/a-1.0.jar"),
            PathBuf::from("/tmp/m2/b/2.0/b-2.0.jar"),
        ],
    }
}

// ---------------------------------------------------------------------
// Per-command JSON contract: --output json
// ---------------------------------------------------------------------

#[test]
fn pull_json_output_validates_against_pull_schema() {
    let buf = SharedBuf::new();
    {
        let mut r = JsonRenderer::new(buf.writer(), /* pretty */ false);
        r.render_pull(&sample_pull_report()).unwrap();
    }
    let doc: Value = serde_json::from_slice(&buf.bytes()).expect("renderer produced valid JSON");
    assert_valid(&load("pull.json"), &doc, "pull --output json");
}

#[test]
fn grind_tree_json_output_validates_against_grind_tree_schema() {
    let buf = SharedBuf::new();
    {
        let mut r = JsonRenderer::new(buf.writer(), /* pretty */ false);
        r.render_grind_tree(&sample_grind_tree_report()).unwrap();
    }
    let doc: Value = serde_json::from_slice(&buf.bytes()).expect("renderer produced valid JSON");
    assert_valid(&load("grind-tree.json"), &doc, "grind tree --output json");
}

#[test]
fn pour_json_output_validates_against_pour_schema() {
    let buf = SharedBuf::new();
    {
        let mut r = JsonRenderer::new(buf.writer(), /* pretty */ false);
        r.render_pour(&sample_pour_report()).unwrap();
    }
    let doc: Value = serde_json::from_slice(&buf.bytes()).expect("renderer produced valid JSON");
    assert_valid(&load("pour.json"), &doc, "pour --output json");
}

// ---------------------------------------------------------------------
// NDJSON contract: each emitted line validates against the
// progress-event envelope, AND its `payload` validates against the
// per-command schema.
// ---------------------------------------------------------------------

fn ndjson_for<F>(render: F) -> Value
where
    F: FnOnce(&mut NdjsonRenderer),
{
    let buf = SharedBuf::new();
    {
        let mut r = NdjsonRenderer::with_fixed_timestamp(buf.writer(), "2026-05-14T12:34:56.789Z");
        render(&mut r);
    }
    let text = String::from_utf8(buf.bytes()).expect("ndjson is utf-8");
    let mut lines = text.lines();
    let first = lines.next().expect("ndjson emitted no lines");
    assert!(
        lines.next().is_none(),
        "expected exactly one ndjson line, got more"
    );
    serde_json::from_str(first).expect("ndjson line is not valid JSON")
}

#[test]
fn pull_ndjson_line_validates_envelope_and_payload() {
    let env = ndjson_for(|r| {
        r.render_pull(&sample_pull_report()).unwrap();
    });
    assert_valid(&load("progress-event.json"), &env, "pull ndjson envelope");
    assert_valid(&load("pull.json"), &env["payload"], "pull ndjson payload");
}

#[test]
fn grind_tree_ndjson_line_validates_envelope_and_payload() {
    let env = ndjson_for(|r| {
        r.render_grind_tree(&sample_grind_tree_report()).unwrap();
    });
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
}

#[test]
fn pour_ndjson_line_validates_envelope_and_payload() {
    let env = ndjson_for(|r| {
        r.render_pour(&sample_pour_report()).unwrap();
    });
    assert_valid(&load("progress-event.json"), &env, "pour ndjson envelope");
    assert_valid(&load("pour.json"), &env["payload"], "pour ndjson payload");
}

#[test]
fn ndjson_error_event_validates_envelope() {
    #[derive(Debug)]
    struct E(&'static str);
    impl std::fmt::Display for E {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "boom: {}", self.0)
        }
    }
    impl std::error::Error for E {}

    let env = ndjson_for(|r| {
        r.render_error(&E("oops")).unwrap();
    });
    assert_valid(&load("progress-event.json"), &env, "error ndjson envelope");
}
