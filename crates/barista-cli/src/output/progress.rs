// SPDX-License-Identifier: MIT OR Apache-2.0

//! Progress-event sink — the seam between command runners and the
//! NDJSON streaming API.
//!
//! Commands that want to surface per-step progress (`barista pull`
//! per-coord, future `pour` per-artifact) take a
//! `&mut dyn ProgressSink` and call its variant-specific methods at
//! the natural points in their pipeline. The sink is selected by
//! `--output`:
//!
//! - `--output ndjson` → [`NdjsonSink`] — every call produces one
//!   line on the same writer the terminal `result` line lands on.
//! - `--output json` → [`NullSink`] — JSON is a single terminal
//!   document, not a stream; intermediate events would corrupt it,
//!   so they're dropped.
//! - `--output human` → [`HumanSink`] — no-op at v0.1. The human
//!   surfaces already have terse stderr summaries (`pull: …`); the
//!   M3.x interactive progress UI (spinner / per-coord ticker) is
//!   tracked separately and will replace this no-op then. Choosing
//!   no-op for v0.1 keeps the human snapshot suite stable.
//!
//! # Hot-loop discipline
//!
//! The trait takes `&str` (not `String`) for `coord` so per-iteration
//! callers don't have to allocate. The [`NdjsonSink`] writes one
//! compact line per call without flushing — flushing every line on a
//! 500-dep run would dominate runtime. Callers that need streaming
//! UX (a separate `tee` consumer) can request a flush via
//! [`ProgressSink::flush`] at coarse boundaries; the renderer's
//! `finish` flushes once at the end regardless.
//!
//! # Error swallowing
//!
//! A progress emit that fails (closed pipe, full disk) must NOT
//! crash the command — progress is decorative. The Ndjson impl
//! discards the [`crate::output::RenderError`] and continues; the
//! one place where we DO want failure is the terminal `result` line,
//! which still goes through [`crate::output::Renderer::render_pull`]
//! and surfaces normally.
//!
//! We'd route the swallowed error to `tracing::warn!` if the CLI
//! crate already depended on `tracing`; it doesn't, and the task's
//! "no new top-level deps" rule trumps the hint. A future logging
//! pass can fold the trace in without touching call sites.

use std::io::{self, Write};

use crate::cli::OutputFormat;

use super::NdjsonRenderer;

/// Streaming progress sink. Implemented by NDJSON (`NdjsonSink`),
/// human-format (`HumanSink`, currently a no-op), and the
/// non-streaming JSON path (`NullSink`).
///
/// Methods take `&mut self` so an impl can hold a writer reference
/// without interior mutability, and `&str` rather than `String` so
/// the hot loop doesn't allocate. Failures inside a sink are
/// swallowed: a busted pipe must not crash the command.
pub trait ProgressSink {
    /// Run boundary — emitted once at the start of a command run.
    fn started(&mut self, phase: &str);

    /// Resolver progress. `coord` may be absent (pre-walk); progress
    /// is an optional percentage in `[0, 100]`.
    fn resolving(&mut self, coord: Option<&str>, progress: Option<f64>);

    /// One artifact's fetch is starting.
    fn fetching(&mut self, coord: &str, progress: Option<f64>);

    /// One artifact's fetch finished.
    fn fetched(&mut self, coord: &str);

    /// One artifact was served from cache.
    fn cached(&mut self, coord: &str);

    /// The lockfile is being written. Reserved for the v0.2
    /// full-fetch path.
    fn writing_lockfile(&mut self);

    /// Run boundary — emitted once at the end of a command run.
    fn completed(&mut self, phase: &str);

    /// Optional flush hint. The default no-ops; the NDJSON impl
    /// pushes its buffer through. Callers may use this between
    /// coarse-grained batches; don't call it per-coord.
    fn flush(&mut self) {}
}

/// Sink that drops every event. Used for `--output json` (a
/// single-document format that can't carry an event stream) and as a
/// safe default in tests / future commands that aren't yet
/// progress-aware.
#[derive(Debug, Default)]
pub struct NullSink;

impl ProgressSink for NullSink {
    fn started(&mut self, _phase: &str) {}
    fn resolving(&mut self, _coord: Option<&str>, _progress: Option<f64>) {}
    fn fetching(&mut self, _coord: &str, _progress: Option<f64>) {}
    fn fetched(&mut self, _coord: &str) {}
    fn cached(&mut self, _coord: &str) {}
    fn writing_lockfile(&mut self) {}
    fn completed(&mut self, _phase: &str) {}
}

/// Sink for `--output human`. A no-op at v0.1: the human surfaces
/// already print a terse one-line summary on `render_*`, and the
/// interactive progress UI (spinner / per-coord ticker) is tracked
/// separately.
///
/// We instantiate it with a borrow of the human-format stderr so a
/// future implementation can `writeln!` without re-wiring callers.
/// At v0.1 the writer is held but unused.
#[derive(Default)]
pub struct HumanSink {
    // Reserved for the future ticker. Kept boxed so the type is
    // identical to the production stderr handle in `make_progress_sink`.
    #[allow(dead_code)]
    err: Option<Box<dyn Write + Send>>,
}

impl HumanSink {
    /// Build a HumanSink that holds (but does not yet use) the given
    /// stderr writer. The interactive ticker that will use it lands
    /// in a later milestone.
    pub fn with_stderr(err: Box<dyn Write + Send>) -> Self {
        Self { err: Some(err) }
    }
}

impl ProgressSink for HumanSink {
    fn started(&mut self, _phase: &str) {}
    fn resolving(&mut self, _coord: Option<&str>, _progress: Option<f64>) {}
    fn fetching(&mut self, _coord: &str, _progress: Option<f64>) {}
    fn fetched(&mut self, _coord: &str) {}
    fn cached(&mut self, _coord: &str) {}
    fn writing_lockfile(&mut self) {}
    fn completed(&mut self, _phase: &str) {}
}

/// Sink that forwards events to an [`NdjsonRenderer`] as
/// `progress-event.json`-valid lines.
///
/// Holds a writer that is **shared** with the renderer that will
/// emit the final `result` line: both go through this same handle,
/// so the stream is naturally ordered. In production both sides come
/// from `io::stdout()`; in tests we wrap a `SharedBuf`.
///
/// Each method discards its [`crate::output::RenderError`] — see the
/// module docstring on error swallowing.
pub struct NdjsonSink {
    renderer: NdjsonRenderer,
}

impl NdjsonSink {
    /// Build a sink that writes to `out`. Use [`with_fixed_timestamp`]
    /// in tests where byte-deterministic output is required.
    ///
    /// [`with_fixed_timestamp`]: NdjsonSink::with_fixed_timestamp
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Self {
            renderer: NdjsonRenderer::new(out),
        }
    }

    /// Test hook — every emitted event uses `ts` for its
    /// `timestamp` field.
    pub fn with_fixed_timestamp(out: Box<dyn Write + Send>, ts: impl Into<String>) -> Self {
        Self {
            renderer: NdjsonRenderer::with_fixed_timestamp(out, ts),
        }
    }

    /// Consume the sink and return its inner [`NdjsonRenderer`], so
    /// the same writer can emit the terminal `result` line.
    pub fn into_renderer(self) -> NdjsonRenderer {
        self.renderer
    }

    /// Helper: discard a render error. Centralized so a future
    /// logging pass can fold in `tracing::warn!` in exactly one place.
    fn swallow<T>(_r: super::RenderResult<T>) {
        // Intentionally a no-op. See module docstring; once the CLI
        // crate gains a `tracing` dep this becomes
        // `if let Err(e) = _r { tracing::warn!(error = ?e, ...) }`.
    }
}

impl ProgressSink for NdjsonSink {
    fn started(&mut self, phase: &str) {
        Self::swallow(self.renderer.emit_started(phase));
    }
    fn resolving(&mut self, coord: Option<&str>, progress: Option<f64>) {
        Self::swallow(self.renderer.emit_resolving(coord, progress));
    }
    fn fetching(&mut self, coord: &str, progress: Option<f64>) {
        Self::swallow(self.renderer.emit_fetching(coord, progress));
    }
    fn fetched(&mut self, coord: &str) {
        Self::swallow(self.renderer.emit_fetched(coord));
    }
    fn cached(&mut self, coord: &str) {
        Self::swallow(self.renderer.emit_cached(coord));
    }
    fn writing_lockfile(&mut self) {
        Self::swallow(self.renderer.emit_writing_lockfile());
    }
    fn completed(&mut self, phase: &str) {
        Self::swallow(self.renderer.emit_completed(phase));
    }
    fn flush(&mut self) {
        Self::swallow(self.renderer.flush());
    }
}

/// Pick the progress sink for a given output format.
///
/// `stdout`/`stderr` are the writers the sink should target.
/// `format` selects the variant:
///
/// - [`OutputFormat::Ndjson`] → [`NdjsonSink`] over `stdout`.
/// - [`OutputFormat::Json`] → [`NullSink`] (single-document format).
/// - [`OutputFormat::Human`] → [`HumanSink`] (no-op at v0.1).
///
/// The factory is intentionally separate from [`crate::output::make_renderer`]
/// because the sink and the renderer can in principle target
/// different writers; in production the NDJSON case uses the same
/// stdout for both, but tests routinely thread a shared buffer.
pub fn make_progress_sink(
    format: OutputFormat,
    stdout: Box<dyn Write + Send>,
    stderr: Box<dyn Write + Send>,
) -> Box<dyn ProgressSink> {
    match format {
        OutputFormat::Ndjson => Box::new(NdjsonSink::new(stdout)),
        OutputFormat::Json => Box::new(NullSink),
        OutputFormat::Human => Box::new(HumanSink::with_stderr(stderr)),
    }
}

/// Runtime convenience: build a progress sink wired to the process'
/// stdout / stderr, picking the variant via the global flags.
pub fn make_runtime_progress_sink(global: &crate::cli::GlobalFlags) -> Box<dyn ProgressSink> {
    let stdout: Box<dyn Write + Send> = Box::new(io::stdout());
    let stderr: Box<dyn Write + Send> = Box::new(io::stderr());
    make_progress_sink(global.output, stdout, stderr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Buf {
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
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn null_sink_writes_nothing() {
        let mut s = NullSink;
        s.started("pull");
        s.fetching("a:b:c", Some(50.0));
        s.fetched("a:b:c");
        s.completed("pull");
        // No assertion beyond "doesn't panic, doesn't allocate
        // anywhere observable" — NullSink has no writer.
    }

    #[test]
    fn human_sink_writes_nothing_at_v01() {
        let buf = Buf::new();
        let mut s = HumanSink::with_stderr(buf.writer());
        s.started("pull");
        s.cached("a:b:c");
        s.completed("pull");
        assert_eq!(buf.text(), "", "HumanSink is a no-op at v0.1");
    }

    #[test]
    fn ndjson_sink_emits_one_line_per_event() {
        let buf = Buf::new();
        let mut s = NdjsonSink::with_fixed_timestamp(buf.writer(), "2026-05-14T12:34:56.789Z");
        s.started("pull");
        s.cached("com.example:a:1.0");
        s.cached("com.example:b:2.0");
        s.completed("pull");
        let text = buf.text();
        let lines: Vec<&str> = text.lines().collect();
        // started + 2 cached + completed = 4 lines.
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("\"event\":\"started\""));
        assert!(lines[1].contains("\"coord\":\"com.example:a:1.0\""));
        assert!(lines[2].contains("\"coord\":\"com.example:b:2.0\""));
        assert!(lines[3].contains("\"event\":\"completed\""));
    }

    #[test]
    fn ndjson_sink_omits_optional_keys() {
        let buf = Buf::new();
        let mut s = NdjsonSink::with_fixed_timestamp(buf.writer(), "2026-05-14T12:34:56.789Z");
        s.started("pull");
        let line = buf.text();
        // `started` has no coord, no progress, no payload. Verify
        // the JSON shape contains none of them.
        assert!(!line.contains("\"coord\""));
        assert!(!line.contains("\"progress\""));
        assert!(!line.contains("\"payload\""));
    }

    #[test]
    fn make_progress_sink_dispatches_by_format() {
        // Each variant just needs to construct without panicking;
        // the per-impl behaviour is covered above.
        let stdout: Box<dyn Write + Send> = Box::new(Vec::<u8>::new());
        let stderr: Box<dyn Write + Send> = Box::new(Vec::<u8>::new());
        let _h = make_progress_sink(OutputFormat::Human, stdout, stderr);
        let stdout: Box<dyn Write + Send> = Box::new(Vec::<u8>::new());
        let stderr: Box<dyn Write + Send> = Box::new(Vec::<u8>::new());
        let _j = make_progress_sink(OutputFormat::Json, stdout, stderr);
        let stdout: Box<dyn Write + Send> = Box::new(Vec::<u8>::new());
        let stderr: Box<dyn Write + Send> = Box::new(Vec::<u8>::new());
        let _n = make_progress_sink(OutputFormat::Ndjson, stdout, stderr);
    }
}
