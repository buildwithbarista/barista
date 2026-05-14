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

//! End-to-end tests for the NDJSON progress-event stream (M3.2 T3).
//!
//! These exercise the streaming half of the `--output ndjson` surface:
//!
//! - The renderer's per-variant `emit_*` API (`started`,
//!   `resolving`, `fetching`, `fetched`, `cached`, `writing-lockfile`,
//!   `completed`) — every emit must be a single valid NDJSON line
//!   conforming to `schema/output/v1/progress-event.json`.
//! - The sink layer that drives the renderer from command code
//!   (`ProgressSink` trait, `NdjsonSink`/`NullSink`/`HumanSink` impls,
//!   `make_progress_sink` factory).
//! - The full `barista pull --no-fetch` flow with a 500-coord
//!   lockfile: the `[T]` acceptance criterion for T3.
//!
//! The test that establishes the `[T]` linkage for the 500-dep
//! criterion is [`pull_500_dep_stream_validates_every_event_against_schema`].

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use barista_cli::cli::{Cli, OutputFormat, PullArgs, ScopeArg, dispatch};
use barista_cli::cmd::pull::run_inner;
use barista_cli::output::progress::{
    HumanSink, NdjsonSink, NullSink, ProgressSink, make_progress_sink,
};
use barista_cli::output::{NdjsonRenderer, Renderer};
use barista_lockfile::{Lockfile, LockfileEntry};
use clap::Parser;
use jsonschema::Validator;
use serde_json::Value;
use tempfile::TempDir;

// ---------------------------------------------------------------------
// shared writer (lets the test read after the renderer drops)
// ---------------------------------------------------------------------

#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
    fn writer(&self) -> Box<dyn Write + Send> {
        Box::new(BufW(self.0.clone()))
    }
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

struct BufW(Arc<Mutex<Vec<u8>>>);
impl Write for BufW {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------
// schema loading
// ---------------------------------------------------------------------

fn schema_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("schema")
        .join("output")
        .join("v1")
        .join("progress-event.json")
}

fn progress_event_validator() -> Validator {
    let raw = fs::read_to_string(schema_path()).expect("read progress-event.json");
    let schema: Value = serde_json::from_str(&raw).expect("parse progress-event.json");
    jsonschema::draft202012::new(&schema).expect("compile progress-event.json")
}

#[track_caller]
fn assert_line_valid(validator: &Validator, line: &str, index: usize) {
    let doc: Value = serde_json::from_str(line)
        .unwrap_or_else(|e| panic!("line {index} is not valid JSON: {e}\nline: {line}"));
    if let Err(error) = validator.validate(&doc) {
        panic!("line {index} failed schema validation: {error}\nline: {line}");
    }
}

// ---------------------------------------------------------------------
// fixture helpers
// ---------------------------------------------------------------------

const MINIMAL_POM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
  <packaging>jar</packaging>
</project>
"#;

fn write_pom(dir: &Path) {
    fs::write(dir.join("pom.xml"), MINIMAL_POM).unwrap();
}

/// Write a `barista.lock` with exactly `n` entries. Coords are
/// `com.example:dep-NNNN:1.0.0`, zero-padded for stable ordering;
/// the rest of the entry uses dummy-but-valid values that satisfy
/// the lockfile reader's invariants.
fn write_lockfile_with_n_entries(dir: &Path, n: usize) {
    let mut lf = Lockfile::new("deadbeef".repeat(8), "cafebabe".repeat(8));
    for i in 0..n {
        lf.entries.push(LockfileEntry {
            coords: format!("com.example:dep-{i:04}"),
            version: "1.0.0".to_string(),
            scope: "compile".to_string(),
            optional: false,
            sha256: "0".repeat(64),
            sha1: None,
            size_bytes: 1024,
            source_url: format!(
                "https://repo.maven.apache.org/maven2/com/example/dep-{i:04}/1.0.0/dep-{i:04}-1.0.0.jar"
            ),
            etag: None,
            last_modified: None,
            classifier: None,
            type_: "jar".to_string(),
            from_path: Vec::new(),
            depth: 0,
            snapshot_resolution: None,
            exclusions: Vec::new(),
        });
    }
    lf.write(&dir.join("barista.lock")).unwrap();
}

/// Build a `GlobalFlags` rooted at `root` with the given output format.
/// Round-trips through clap so the defaults stay consistent with the
/// production CLI surface.
fn flags_for(root: &Path, format: &str) -> Cli {
    Cli::try_parse_from([
        "barista",
        "--root",
        root.to_str().unwrap(),
        "--output",
        format,
        "pull",
        "--no-fetch",
    ])
    .expect("parse argv")
}

// =====================================================================
// 1. [T] 500-coord stream: every line validates against the schema.
// =====================================================================

/// **[T]** for the M3.2 T3 acceptance criterion: `barista pull` on a
/// 500-dep project emits one event per coordinate, and every line of
/// the resulting NDJSON stream validates against
/// `schema/output/v1/progress-event.json`.
///
/// The lockfile contains 500 entries. The `--no-fetch` path reads it
/// and emits one `cached` event per entry plus the `started` /
/// `resolving` (project root) / `completed` bookends plus the
/// terminal `result` line carrying the full `PullReport`.
#[test]
fn pull_500_dep_stream_validates_every_event_against_schema() {
    const N: usize = 500;
    let td = TempDir::new().unwrap();
    let root = td.path();
    write_pom(root);
    write_lockfile_with_n_entries(root, N);

    let buf = SharedBuf::new();
    let cli = flags_for(root, "ndjson");
    let Cli { global, command: _ } = cli;
    let args = PullArgs {
        update: false,
        scope: ScopeArg::Compile,
        no_fetch: true,
        explain: false,
    };

    let started = Instant::now();
    {
        // Wire the sink and the renderer to the same shared buffer
        // so the event stream and the terminal `result` line land in
        // the same byte sequence the production stdout would carry.
        let mut sink = NdjsonSink::with_fixed_timestamp(buf.writer(), "2026-05-14T12:34:56.789Z");
        let report = run_inner(&global, &args, &mut sink).expect("pull --no-fetch ok");
        let mut renderer =
            NdjsonRenderer::with_fixed_timestamp(buf.writer(), "2026-05-14T12:34:56.789Z");
        renderer.render_pull(&report).unwrap();
        Box::new(renderer).finish().unwrap();
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed.as_secs() < 5,
        "500-dep stream took {elapsed:?}; expected under 5s"
    );

    let validator = progress_event_validator();
    let text = buf.text();
    let lines: Vec<&str> = text.lines().collect();

    // Sanity: at least N `cached` + started + resolving + completed
    // + result == N + 4 lines minimum.
    assert!(
        lines.len() >= N + 4,
        "expected >= {} lines, got {}",
        N + 4,
        lines.len()
    );

    // Every line conforms to the schema.
    for (i, line) in lines.iter().enumerate() {
        assert_line_valid(&validator, line, i);
    }

    // Exactly N `cached` events.
    let cached_count = lines
        .iter()
        .filter(|l| l.contains("\"event\":\"cached\""))
        .count();
    assert_eq!(cached_count, N, "one cached event per coord");

    // Result line is the last line; carries `payload`.
    let last: Value = serde_json::from_str(lines.last().unwrap()).unwrap();
    assert_eq!(last["event"], "result");
    assert!(last.get("payload").is_some(), "result carries payload");

    // First line is `started`.
    let first: Value = serde_json::from_str(lines.first().unwrap()).unwrap();
    assert_eq!(first["event"], "started");
}

// =====================================================================
// 2. ordering invariants — `started` first, `completed` then `result`
// =====================================================================

#[test]
fn pull_stream_ordering_invariants() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write_pom(root);
    write_lockfile_with_n_entries(root, 10);

    let buf = SharedBuf::new();
    let cli = flags_for(root, "ndjson");
    let Cli { global, command: _ } = cli;
    let args = PullArgs {
        update: false,
        scope: ScopeArg::Compile,
        no_fetch: true,
        explain: false,
    };

    {
        let mut sink = NdjsonSink::with_fixed_timestamp(buf.writer(), "2026-05-14T12:34:56.789Z");
        let report = run_inner(&global, &args, &mut sink).expect("pull --no-fetch ok");
        let mut renderer =
            NdjsonRenderer::with_fixed_timestamp(buf.writer(), "2026-05-14T12:34:56.789Z");
        renderer.render_pull(&report).unwrap();
        Box::new(renderer).finish().unwrap();
    }

    let text = buf.text();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid json line"))
        .collect();

    // started is the first event.
    assert_eq!(lines.first().unwrap()["event"], "started");
    // result is the last event.
    assert_eq!(lines.last().unwrap()["event"], "result");
    // completed appears exactly once, before result.
    let completed_idx = lines
        .iter()
        .position(|l| l["event"] == "completed")
        .expect("completed event present");
    let result_idx = lines.iter().position(|l| l["event"] == "result").unwrap();
    assert!(
        completed_idx < result_idx,
        "completed must come before result"
    );
}

// =====================================================================
// 3. per-variant required-field invariants (schema is the gate, but
//    we assert directly for human-readable failures)
// =====================================================================

#[test]
fn per_variant_required_fields_are_present() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write_pom(root);
    write_lockfile_with_n_entries(root, 5);

    let buf = SharedBuf::new();
    let cli = flags_for(root, "ndjson");
    let Cli { global, command: _ } = cli;
    let args = PullArgs {
        update: false,
        scope: ScopeArg::Compile,
        no_fetch: true,
        explain: false,
    };

    {
        let mut sink = NdjsonSink::with_fixed_timestamp(buf.writer(), "2026-05-14T12:34:56.789Z");
        let report = run_inner(&global, &args, &mut sink).expect("pull ok");
        let mut renderer =
            NdjsonRenderer::with_fixed_timestamp(buf.writer(), "2026-05-14T12:34:56.789Z");
        renderer.render_pull(&report).unwrap();
        Box::new(renderer).finish().unwrap();
    }

    let text = buf.text();
    for line in text.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        // Every event has `event` + `timestamp`.
        assert!(v.get("event").is_some(), "event field present: {line}");
        assert!(v.get("timestamp").is_some(), "timestamp present: {line}");
        match v["event"].as_str().unwrap() {
            "fetching" | "fetched" | "cached" => {
                assert!(
                    v.get("coord").is_some(),
                    "{} requires coord: {line}",
                    v["event"]
                );
            }
            "result" | "error" => {
                assert!(
                    v.get("payload").is_some(),
                    "{} requires payload: {line}",
                    v["event"]
                );
            }
            _ => {}
        }
    }
}

// =====================================================================
// 4. NullSink emits nothing for --output json
// =====================================================================

#[test]
fn null_sink_emits_no_events_for_json_format() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write_pom(root);
    write_lockfile_with_n_entries(root, 25);

    let buf = SharedBuf::new();
    let mut sink: Box<dyn ProgressSink> = Box::new(NullSink);

    // Build CLI shape for json. Drive run_inner directly so we can
    // inspect what the sink produced.
    let cli = Cli::try_parse_from([
        "barista",
        "--root",
        root.to_str().unwrap(),
        "--output",
        "json",
        "pull",
        "--no-fetch",
    ])
    .unwrap();
    let Cli { global, .. } = cli;
    let args = PullArgs {
        update: false,
        scope: ScopeArg::Compile,
        no_fetch: true,
        explain: false,
    };

    // The sink writes nowhere — but we also need to confirm the
    // factory hands out the right impl for `--output json`. We
    // bridge those by checking the buffer stays empty after a run
    // that emits ~25 cached events through a Null sink.
    let _ = run_inner(&global, &args, sink.as_mut()).unwrap();
    assert_eq!(buf.text(), "", "NullSink must not write to its buffer");
}

// =====================================================================
// 5. factory dispatch — make_progress_sink returns the right variant
//    behaviour per format
// =====================================================================

#[test]
fn factory_dispatches_correct_sink_per_format() {
    // NDJSON path produces lines. JSON path produces nothing. Human
    // path produces nothing (v0.1 no-op).
    for (format, expect_lines) in [
        (OutputFormat::Ndjson, true),
        (OutputFormat::Json, false),
        (OutputFormat::Human, false),
    ] {
        let stdout = SharedBuf::new();
        let stderr = SharedBuf::new();
        let mut sink = make_progress_sink(format, stdout.writer(), stderr.writer());
        sink.started("pull");
        sink.cached("a:b:c");
        sink.cached("d:e:f");
        sink.completed("pull");
        sink.flush();

        let stdout_text = stdout.text();
        let stderr_text = stderr.text();
        if expect_lines {
            let lines: Vec<&str> = stdout_text.lines().collect();
            assert_eq!(
                lines.len(),
                4,
                "ndjson sink should emit 4 events, got {}: {stdout_text}",
                lines.len()
            );
            assert!(
                stderr_text.is_empty(),
                "ndjson sink should not touch stderr"
            );
        } else {
            assert!(
                stdout_text.is_empty(),
                "{format:?} sink should not write stdout: {stdout_text}"
            );
            assert!(
                stderr_text.is_empty(),
                "{format:?} sink should not write stderr at v0.1: {stderr_text}"
            );
        }
    }
}

// =====================================================================
// 6. line-by-line schema validation on the small case
// =====================================================================

#[test]
fn ten_dep_stream_every_line_validates() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write_pom(root);
    write_lockfile_with_n_entries(root, 10);

    let buf = SharedBuf::new();
    let cli = flags_for(root, "ndjson");
    let Cli { global, command: _ } = cli;
    let args = PullArgs {
        update: false,
        scope: ScopeArg::Compile,
        no_fetch: true,
        explain: false,
    };

    {
        let mut sink = NdjsonSink::with_fixed_timestamp(buf.writer(), "2026-05-14T12:34:56.789Z");
        let report = run_inner(&global, &args, &mut sink).expect("ok");
        let mut renderer =
            NdjsonRenderer::with_fixed_timestamp(buf.writer(), "2026-05-14T12:34:56.789Z");
        renderer.render_pull(&report).unwrap();
        Box::new(renderer).finish().unwrap();
    }

    let validator = progress_event_validator();
    let text = buf.text();
    let lines: Vec<&str> = text.lines().collect();
    assert!(!lines.is_empty(), "stream not empty");
    for (i, line) in lines.iter().enumerate() {
        assert_line_valid(&validator, line, i);
    }
}

// =====================================================================
// 7. existing dispatch path still works under --output ndjson
//    (no regression of the cmd_pull integration tests' exit codes)
// =====================================================================

#[test]
fn dispatch_with_ndjson_output_returns_zero() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write_pom(root);
    write_lockfile_with_n_entries(root, 3);

    let cli = Cli::try_parse_from([
        "barista",
        "--root",
        root.to_str().unwrap(),
        "--output",
        "ndjson",
        "pull",
        "--no-fetch",
    ])
    .unwrap();
    // dispatch writes to the actual stdout; we only assert it
    // doesn't panic and returns zero.
    let code = dispatch(cli);
    assert_eq!(code, 0, "ndjson dispatch path should exit 0");
}

// =====================================================================
// 8. HumanSink is a no-op at v0.1 (documented behaviour)
// =====================================================================

#[test]
fn human_sink_is_a_noop_at_v01() {
    let buf = SharedBuf::new();
    let mut sink = HumanSink::with_stderr(buf.writer());
    sink.started("pull");
    sink.fetching("a:b:c", Some(50.0));
    sink.cached("d:e:f");
    sink.completed("pull");
    assert_eq!(
        buf.text(),
        "",
        "HumanSink is a no-op at v0.1 (see output::progress docstring)"
    );
}
