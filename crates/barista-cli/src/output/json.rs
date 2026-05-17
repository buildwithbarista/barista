//! JSON renderer — one document per CLI invocation.
//!
//! `pretty` controls indentation; the factory turns it on when stdout
//! is a tty (and off when it's piped) so machine consumers get
//! compact output by default while interactive use stays readable.
//!
//! The first call to a `render_*` method writes the document; any
//! subsequent call is a programming error. The renderer guards
//! against that by tracking a "spoken" flag and surfacing
//! `RenderError::Io` (`InvalidInput`) on the second write — the CLI
//! invocation model is one report per run, period.
//!
//! Errors are emitted as a small `{"command":"error", …}` document
//! so JSON-consuming tooling can detect a failure without parsing
//! stderr.

use std::io::Write;

use serde::Serialize;

use super::report::{GrindTreeReport, PourReport, PullReport, VerifyReport};
use super::{RenderError, RenderResult, Renderer};

/// Renderer for `OutputFormat::Json`.
pub struct JsonRenderer {
    out: Box<dyn Write + Send>,
    pretty: bool,
    /// `true` once a document has been written. JSON output is
    /// single-document; emitting twice would produce invalid JSON.
    spoken: bool,
}

impl JsonRenderer {
    /// Build a renderer over `out`. `pretty` selects
    /// `to_writer_pretty` (typically on a tty) over the compact
    /// writer (typically on a pipe / in CI).
    pub fn new(out: Box<dyn Write + Send>, pretty: bool) -> Self {
        Self {
            out,
            pretty,
            spoken: false,
        }
    }

    fn write_doc<T: Serialize>(&mut self, value: &T) -> RenderResult<()> {
        if self.spoken {
            return Err(RenderError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "JsonRenderer already emitted a document; \
                 only one document is permitted per invocation",
            )));
        }
        if self.pretty {
            serde_json::to_writer_pretty(&mut self.out, value)?;
        } else {
            serde_json::to_writer(&mut self.out, value)?;
        }
        self.out.write_all(b"\n")?;
        self.spoken = true;
        Ok(())
    }
}

impl Renderer for JsonRenderer {
    fn render_pull(&mut self, report: &PullReport) -> RenderResult<()> {
        self.write_doc(report)
    }

    fn render_grind_tree(&mut self, report: &GrindTreeReport) -> RenderResult<()> {
        // The report struct already carries `tag = "command"`, so we
        // serialize it directly to get a single document.
        self.write_doc(report)
    }

    fn render_pour(&mut self, report: &PourReport) -> RenderResult<()> {
        self.write_doc(report)
    }

    fn render_verify(&mut self, report: &VerifyReport) -> RenderResult<()> {
        self.write_doc(report)
    }

    fn render_error(&mut self, err: &(dyn std::error::Error + 'static)) -> RenderResult<()> {
        let doc = ErrorDoc {
            command: "error",
            message: err.to_string(),
        };
        self.write_doc(&doc)
    }

    fn finish(mut self: Box<Self>) -> RenderResult<()> {
        self.out.flush()?;
        Ok(())
    }
}

/// Shape emitted by [`JsonRenderer::render_error`].
#[derive(Debug, Serialize)]
struct ErrorDoc {
    command: &'static str,
    message: String,
}
