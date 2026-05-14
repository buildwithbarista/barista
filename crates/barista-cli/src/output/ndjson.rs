//! NDJSON renderer — newline-delimited JSON, one event per line.
//!
//! M3.2 T1 establishes the writer plumbing: every `render_*` call
//! produces exactly one compact `{"event":"result", …}` line followed
//! by `\n`, and `render_error` produces one `{"event":"error", …}`
//! line. Streaming progress events (`{"event":"fetching", …}`, etc.)
//! land in T3 — this renderer is the substrate they'll plug into.
//!
//! Every line carries an RFC 3339 millisecond-precision `timestamp`
//! and a `payload` object, matching `schema/output/v1/progress-event.json`.
//!
//! Why a separate format from JSON? NDJSON consumers stream-parse:
//! they read until a `\n`, parse one event, repeat. They don't want
//! a single pretty-printed document. The two formats share the
//! same report types but emit different envelopes.

use std::io::Write;
use std::time::SystemTime;

use serde::Serialize;

use super::report::{GrindTreeReport, PourReport, PullReport};
use super::{RenderResult, Renderer};

/// Renderer for `OutputFormat::Ndjson`.
pub struct NdjsonRenderer {
    out: Box<dyn Write + Send>,
    /// Override the timestamp source for deterministic tests. When
    /// `Some`, every emitted line uses this exact string; when
    /// `None`, the current wall clock is formatted to RFC 3339 ms.
    clock: Option<String>,
}

impl NdjsonRenderer {
    /// Build a renderer over `out`. Always compact — NDJSON consumers
    /// stream-parse line-by-line.
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Self { out, clock: None }
    }

    /// **Test hook.** Pin every emitted line to a fixed timestamp so
    /// snapshot tests (and other byte-deterministic tests) can assert
    /// exact output. Not used by the production CLI path.
    pub fn with_fixed_timestamp(out: Box<dyn Write + Send>, ts: impl Into<String>) -> Self {
        Self {
            out,
            clock: Some(ts.into()),
        }
    }

    fn now(&self) -> String {
        self.clock
            .clone()
            .unwrap_or_else(|| rfc3339_millis(SystemTime::now()))
    }

    fn write_line<T: Serialize>(&mut self, value: &T) -> RenderResult<()> {
        serde_json::to_writer(&mut self.out, value)?;
        self.out.write_all(b"\n")?;
        Ok(())
    }
}

impl Renderer for NdjsonRenderer {
    fn render_pull(&mut self, report: &PullReport) -> RenderResult<()> {
        let ts = self.now();
        self.write_line(&Envelope {
            event: "result",
            timestamp: &ts,
            payload: report,
        })
    }

    fn render_grind_tree(&mut self, report: &GrindTreeReport) -> RenderResult<()> {
        let ts = self.now();
        self.write_line(&Envelope {
            event: "result",
            timestamp: &ts,
            payload: report,
        })
    }

    fn render_pour(&mut self, report: &PourReport) -> RenderResult<()> {
        let ts = self.now();
        self.write_line(&Envelope {
            event: "result",
            timestamp: &ts,
            payload: report,
        })
    }

    fn render_error(&mut self, err: &(dyn std::error::Error + 'static)) -> RenderResult<()> {
        let ts = self.now();
        self.write_line(&ErrorEvent {
            event: "error",
            timestamp: &ts,
            payload: ErrorPayload {
                message: err.to_string(),
            },
        })
    }

    fn finish(mut self: Box<Self>) -> RenderResult<()> {
        self.out.flush()?;
        Ok(())
    }
}

/// One NDJSON line for a successful result. The `payload` carries
/// the report (which itself includes a `"command"` discriminator), so
/// a consumer can route on `event == "result"` first and then
/// `payload.command`. Matches `schema/output/v1/progress-event.json`.
#[derive(Debug, Serialize)]
struct Envelope<'a, T: Serialize> {
    event: &'static str,
    timestamp: &'a str,
    payload: &'a T,
}

/// One NDJSON line for an error event. The `payload` carries the
/// diagnostic message. Matches `schema/output/v1/progress-event.json`.
#[derive(Debug, Serialize)]
struct ErrorEvent<'a> {
    event: &'static str,
    timestamp: &'a str,
    payload: ErrorPayload,
}

#[derive(Debug, Serialize)]
struct ErrorPayload {
    message: String,
}

// ---------------------------------------------------------------------
// Tiny RFC 3339 formatter.
//
// Output shape: `YYYY-MM-DDTHH:MM:SS.mmmZ`. Always UTC. Always
// millisecond precision (zero-padded). Matches the regex in
// `schema/output/v1/progress-event.json`.
//
// Implementing this inline avoids pulling chrono/time/jiff into the
// workspace just to stamp a few hundred bytes of NDJSON. The
// algorithm is the standard Howard Hinnant `civil_from_days`.
// ---------------------------------------------------------------------

fn rfc3339_millis(t: SystemTime) -> String {
    let dur = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let total_secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();

    // Break into civil date + time-of-day.
    let days = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    )
}

/// Convert days-since-1970-01-01 to (year, month, day).
///
/// Algorithm from Howard Hinnant's date-algorithms paper
/// <http://howardhinnant.github.io/date_algorithms.html>, public
/// domain. Correct for all dates 0000-03-01 onward.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let year = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (year, m, d)
}

#[cfg(test)]
mod rfc3339_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn unix_epoch_is_formatted_correctly() {
        let t = SystemTime::UNIX_EPOCH;
        assert_eq!(rfc3339_millis(t), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn known_timestamp_matches() {
        // 2026-05-14T12:34:56.789Z
        let secs = 1_778_762_096_u64;
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(789);
        assert_eq!(rfc3339_millis(t), "2026-05-14T12:34:56.789Z");
    }

    #[test]
    fn leap_year_feb_29_renders() {
        // 2024-02-29T00:00:00.000Z — Unix ts 1709164800.
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(rfc3339_millis(t), "2024-02-29T00:00:00.000Z");
    }
}
