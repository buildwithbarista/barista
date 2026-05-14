//! NDJSON renderer — newline-delimited JSON, one event per line.
//!
//! M3.2 T1 establishes the writer plumbing: every `render_*` call
//! produces exactly one compact `{"event":"result", …}` line followed
//! by `\n`, and `render_error` produces one `{"event":"error", …}`
//! line. Streaming progress events (`{"event":"step", …}`, etc.)
//! land in T3 — this renderer is the substrate they'll plug into.
//!
//! Why a separate format from JSON? NDJSON consumers stream-parse:
//! they read until a `\n`, parse one event, repeat. They don't want
//! a single pretty-printed document. The two formats share the
//! same report types but emit different envelopes.

use std::io::Write;

use serde::Serialize;

use super::report::{GrindTreeReport, PourReport, PullReport};
use super::{RenderResult, Renderer};

/// Renderer for `OutputFormat::Ndjson`.
pub struct NdjsonRenderer {
    out: Box<dyn Write + Send>,
}

impl NdjsonRenderer {
    /// Build a renderer over `out`. Always compact — NDJSON consumers
    /// stream-parse line-by-line.
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Self { out }
    }

    fn write_line<T: Serialize>(&mut self, value: &T) -> RenderResult<()> {
        serde_json::to_writer(&mut self.out, value)?;
        self.out.write_all(b"\n")?;
        Ok(())
    }
}

impl Renderer for NdjsonRenderer {
    fn render_pull(&mut self, report: &PullReport) -> RenderResult<()> {
        self.write_line(&Envelope {
            event: "result",
            data: report,
        })
    }

    fn render_grind_tree(&mut self, report: &GrindTreeReport) -> RenderResult<()> {
        self.write_line(&Envelope {
            event: "result",
            data: report,
        })
    }

    fn render_pour(&mut self, report: &PourReport) -> RenderResult<()> {
        self.write_line(&Envelope {
            event: "result",
            data: report,
        })
    }

    fn render_error(&mut self, err: &(dyn std::error::Error + 'static)) -> RenderResult<()> {
        self.write_line(&ErrorEvent {
            event: "error",
            message: err.to_string(),
        })
    }

    fn finish(mut self: Box<Self>) -> RenderResult<()> {
        self.out.flush()?;
        Ok(())
    }
}

/// One NDJSON line for a successful result. The `data` field carries
/// the report (which itself includes a `"command"` discriminator), so
/// a consumer can route on `event == "result"` first and then
/// `data.command`.
#[derive(Debug, Serialize)]
struct Envelope<'a, T: Serialize> {
    event: &'static str,
    data: &'a T,
}

/// One NDJSON line for an error event.
#[derive(Debug, Serialize)]
struct ErrorEvent {
    event: &'static str,
    message: String,
}
