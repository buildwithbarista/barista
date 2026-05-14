//! Multi-format output renderer.
//!
//! The CLI's `--output <FORMAT>` global flag (see [`crate::cli::OutputFormat`])
//! picks between three presentations of the same data:
//!
//! - **`human`** — ANSI-aware text designed for a developer at a tty.
//! - **`json`** — a single JSON document on stdout. Pretty-printed
//!   when stdout is a tty, compact otherwise.
//! - **`ndjson`** — newline-delimited JSON; one event per line. M3.2
//!   T1 lays the writer plumbing; the streaming progress events
//!   (`progress`, `step`, …) come in T3.
//!
//! # Architecture
//!
//! Each command's [`crate::cmd`] runner builds a structured report
//! ([`report::PullReport`], [`report::GrindTreeReport`],
//! [`report::PourReport`]) and hands it to a [`Renderer`]. The
//! renderer is responsible for **all** byte emission to stdout / the
//! configured writer; commands no longer call `print!` /
//! `println!` / `serde_json::to_writer` directly.
//!
//! ```text
//!   cmd::pull::run ─┐                  ┌─ HumanRenderer
//!   cmd::grind::run ┼─► report::* ──►──┼─ JsonRenderer
//!   cmd::pour::run ─┘                  └─ NdjsonRenderer
//! ```
//!
//! Pick a renderer with [`make_renderer`] (or construct one directly
//! for tests).
//!
//! # What is *not* in scope
//!
//! Some commands have conversational stdout that is intentionally
//! not part of the structured-output story for v0.1:
//!
//! - [`crate::cmd::dial_in`] — prints a config-write summary
//!   (`"Wrote ~/.barista/config.toml."`) directly.
//! - [`crate::cmd::wrapper`] — prints a `"baristaw: wrote 3 files…"`
//!   summary directly.
//! - [`crate::cmd::maven_vocab`] — prints the "not yet executable"
//!   error directly.
//!
//! These remain on direct `println!` / `eprintln!` calls. They are
//! either interactive (dial-in), pre-execution side-effects (wrapper
//! file generation), or stub paths whose machine-readable shape is
//! pinned by a later milestone. Folding them into the renderer
//! before they have stable JSON shapes would be premature.

pub mod human;
pub mod json;
pub mod ndjson;
pub mod report;

use std::io::{self, IsTerminal, Write};

pub use human::HumanRenderer;
pub use json::JsonRenderer;
pub use ndjson::NdjsonRenderer;
pub use report::{
    GrindTreeReport, LockfileStatus, PourReport, PullReport, ReactorModule, TreeNode,
};

use crate::cli::OutputFormat;

/// Convenience result alias for renderer methods.
pub type RenderResult<T> = std::result::Result<T, RenderError>;

/// Errors raised while rendering a report.
///
/// Renderers fail for two reasons: the underlying writer returned an
/// I/O error, or `serde_json` rejected a value during serialization.
/// Both are recoverable in the same sense — the process should report
/// the failure and exit non-zero — so the two variants share a single
/// type.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// The underlying [`Write`] returned an error.
    #[error("output write: {0}")]
    Io(#[from] io::Error),
    /// `serde_json` failed to serialize a report.
    #[error("output serialization: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// A renderer turns a structured report into bytes on its underlying
/// writer(s). One renderer is constructed per CLI invocation and
/// consumes the report (or reports — NDJSON streams) the running
/// command produces.
///
/// Renderers are dropped on `finish`. Implementations that buffer
/// data should flush there.
pub trait Renderer {
    /// Render the result of `barista pull`.
    fn render_pull(&mut self, report: &PullReport) -> RenderResult<()>;

    /// Render the result of `barista grind tree`.
    fn render_grind_tree(&mut self, report: &GrindTreeReport) -> RenderResult<()>;

    /// Render the result of `barista pour`.
    fn render_pour(&mut self, report: &PourReport) -> RenderResult<()>;

    /// Render a terminal error. Commands call this from their error
    /// arm before exiting non-zero. Renderers in machine-readable
    /// formats emit a single error document / event; the human
    /// renderer emits a friendly stderr message.
    fn render_error(&mut self, err: &(dyn std::error::Error + 'static)) -> RenderResult<()>;

    /// Flush and consume the renderer. Called once at the end of an
    /// invocation. Implementations that buffer must flush here.
    fn finish(self: Box<Self>) -> RenderResult<()>;
}

/// Build the renderer the global flags ask for.
///
/// `stdout` is the writer that receives structured (json / ndjson)
/// output and human-format `grind tree` output. The human renderer
/// also writes informational summary lines for `pull` / `pour` to
/// stderr; the factory wires `io::stderr()` for that.
///
/// `ansi` is the colour-and-pretty-printing gate. For `Human`, it
/// selects ANSI styling (today: stored but plain-text; T3+ will
/// colorize). For `Json`, it selects `to_writer_pretty` over the
/// compact writer — the convention is "pretty on a tty, compact on
/// a pipe / in CI". Callers typically derive it from
/// `!global.no_color && stream_is_tty()`.
pub fn make_renderer(
    format: OutputFormat,
    stdout: Box<dyn Write + Send>,
    ansi: bool,
) -> Box<dyn Renderer> {
    match format {
        OutputFormat::Human => {
            Box::new(HumanRenderer::new(stdout, Box::new(io::stderr()), ansi))
        }
        OutputFormat::Json => Box::new(JsonRenderer::new(stdout, /* pretty: */ ansi)),
        OutputFormat::Ndjson => Box::new(NdjsonRenderer::new(stdout)),
    }
}

/// Build the renderer most commands want at runtime: the global
/// flags pick the format, and tty detection on stdout decides
/// pretty-print vs compact.
///
/// The "ansi" axis combines:
///
/// - `!global.no_color` — the user hasn't explicitly opted out of
///   colour / pretty output.
/// - [`io::stdout().is_terminal()`] — stdout is connected to a
///   terminal, not a pipe / file. CI invocations (and
///   `barista … | jq`) hit this and get compact JSON; an interactive
///   `barista … --output json` at a terminal gets pretty output.
///
/// This convenience wrapper exists so command-runner code under
/// `crate::cmd::*` doesn't have to thread `Box<dyn Write>` /
/// tty-detection through every call site.
pub fn make_runtime_renderer(global: &crate::cli::GlobalFlags) -> Box<dyn Renderer> {
    let stdout: Box<dyn Write + Send> = Box::new(io::stdout());
    let ansi = !global.no_color && io::stdout().is_terminal();
    make_renderer(global.output, stdout, ansi)
}
