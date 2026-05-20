// SPDX-License-Identifier: MIT OR Apache-2.0

//! NDJSON renderer — newline-delimited JSON, one event per line.
//!
//! M3.2 T1 established the writer plumbing: every `render_*` call
//! produces exactly one compact `{"event":"result", …}` line followed
//! by `\n`, and `render_error` produces one `{"event":"error", …}`
//! line. M3.2 T3 adds the streaming progress-event API
//! (`emit_started`, `emit_fetching`, `emit_cached`, …) — one method
//! per `event` variant in `schema/output/v1/progress-event.json`.
//! Each emit produces exactly one valid NDJSON line and conforms to
//! the schema; per-variant required fields (`coord` on
//! `fetching`/`fetched`/`cached`) are enforced by the method signatures.
//!
//! Every line carries an RFC 3339 millisecond-precision `timestamp`
//! and (for `result`/`error`) a `payload` object, matching
//! `schema/output/v1/progress-event.json`.
//!
//! Why a separate format from JSON? NDJSON consumers stream-parse:
//! they read until a `\n`, parse one event, repeat. They don't want
//! a single pretty-printed document. The two formats share the
//! same report types but emit different envelopes.
//!
//! # Hot-loop discipline
//!
//! On a 500-dep `barista pull` we emit one `cached` (or
//! `fetched`/`fetching`) event per coordinate. The trait that drives
//! emission ([`crate::output::progress::ProgressSink`]) takes
//! `&str` rather than `String` so callers don't have to allocate at
//! the call site, and the writer is flushed on `finish` (or
//! coarse-grained pulses) — not after every line — so we don't pay a
//! syscall per coord.

use std::io::Write;
use std::time::SystemTime;

use serde::Serialize;

use super::report::{GrindTreeReport, PourReport, PullReport, VerifyReport};
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

    // ----- progress-event API (M3.2 T3) ------------------------------------
    //
    // Every method below produces exactly one NDJSON line conforming
    // to `schema/output/v1/progress-event.json`. The single private
    // [`Self::emit`] helper stamps the timestamp and serializes the
    // event; the public methods are thin wrappers that build a
    // [`ProgressEvent`] value with the right variant-specific fields.
    //
    // Required-field invariants from the schema are encoded directly
    // in the method signatures: `emit_fetching`/`emit_fetched`/
    // `emit_cached` take a non-`Option` `coord: &str`, so it's a
    // compile-time error to omit the coordinate from a variant that
    // requires it.

    /// Emit one `started` event. `phase` is a free-form label for the
    /// run (e.g. `"pull"`); it is forwarded verbatim into the
    /// envelope's optional `phase` field via [`ProgressEvent::phase`].
    pub fn emit_started(&mut self, phase: &str) -> RenderResult<()> {
        let ts = self.now();
        self.write_line(&ProgressEvent {
            event: "started",
            timestamp: &ts,
            phase: schema_phase_for(phase),
            ..Default::default()
        })
    }

    /// Emit one `resolving` event. `coord` and `progress` are
    /// optional — pre-resolution we typically have neither; mid-walk
    /// we may have a coord; with a known node count we can populate
    /// a percentage.
    pub fn emit_resolving(
        &mut self,
        coord: Option<&str>,
        progress: Option<f64>,
    ) -> RenderResult<()> {
        let ts = self.now();
        self.write_line(&ProgressEvent {
            event: "resolving",
            timestamp: &ts,
            coord,
            progress,
            phase: Some("resolve"),
        })
    }

    /// Emit one `fetching` event. `coord` is required by the schema
    /// (`required: ["coord"]` in the `fetching` branch); `progress`
    /// is an optional byte-percentage in `[0, 100]`.
    pub fn emit_fetching(&mut self, coord: &str, progress: Option<f64>) -> RenderResult<()> {
        let ts = self.now();
        self.write_line(&ProgressEvent {
            event: "fetching",
            timestamp: &ts,
            coord: Some(coord),
            progress,
            phase: Some("fetch"),
        })
    }

    /// Emit one `fetched` event. `coord` is required by the schema.
    pub fn emit_fetched(&mut self, coord: &str) -> RenderResult<()> {
        let ts = self.now();
        self.write_line(&ProgressEvent {
            event: "fetched",
            timestamp: &ts,
            coord: Some(coord),
            phase: Some("fetch"),
            ..Default::default()
        })
    }

    /// Emit one `cached` event. `coord` is required by the schema.
    ///
    /// In v0.1 the `--no-fetch` path uses this to surface each
    /// pre-existing lockfile entry so streaming consumers see
    /// per-coord progress at the same cadence the full-fetch path
    /// will emit `fetched`.
    pub fn emit_cached(&mut self, coord: &str) -> RenderResult<()> {
        let ts = self.now();
        self.write_line(&ProgressEvent {
            event: "cached",
            timestamp: &ts,
            coord: Some(coord),
            phase: Some("fetch"),
            ..Default::default()
        })
    }

    /// Emit one `writing-lockfile` event. Reserved for the v0.2
    /// full-fetch path; included now so the trait surface is closed.
    pub fn emit_writing_lockfile(&mut self) -> RenderResult<()> {
        let ts = self.now();
        self.write_line(&ProgressEvent {
            event: "writing-lockfile",
            timestamp: &ts,
            phase: Some("lock-write"),
            ..Default::default()
        })
    }

    /// Emit one `completed` event. `phase` matches the `started` value.
    pub fn emit_completed(&mut self, phase: &str) -> RenderResult<()> {
        let ts = self.now();
        self.write_line(&ProgressEvent {
            event: "completed",
            timestamp: &ts,
            phase: schema_phase_for(phase),
            ..Default::default()
        })
    }

    /// Flush the underlying writer without consuming the renderer.
    ///
    /// Useful between coarse-grained batches in a long stream so
    /// downstream consumers don't sit on a buffered pipe. Hot-loop
    /// callers (per-coord) should NOT flush every iteration; flush
    /// every N events or at a phase boundary instead.
    pub fn flush(&mut self) -> RenderResult<()> {
        self.out.flush()?;
        Ok(())
    }
}

/// Translate the free-form `phase` strings the commands hand us
/// (`"pull"`) into the closed enum the schema's optional `phase`
/// field accepts (`"resolve" | "fetch" | "lock-write" | "pour"`).
///
/// Run-level phases like `"pull"` don't map cleanly to the schema's
/// fine-grained `phase` enum, so we drop the field rather than
/// invent a value the schema would reject. The verb is preserved on
/// the `event` discriminator; consumers that need to group by command
/// can look at adjacent `started`/`completed` pairs.
fn schema_phase_for(phase: &str) -> Option<&'static str> {
    match phase {
        "resolve" => Some("resolve"),
        "fetch" => Some("fetch"),
        "lock-write" => Some("lock-write"),
        "pour" => Some("pour"),
        _ => None,
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

    fn render_verify(&mut self, report: &VerifyReport) -> RenderResult<()> {
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

/// In-flight progress event. Borrows everything it can (the event
/// discriminator is `'static`, the coord and timestamp are borrowed
/// from the caller) so the per-coord hot path doesn't allocate.
///
/// `Option` fields are `skip_serializing_if = Option::is_none` so an
/// event only carries the keys the schema permits for its variant
/// (e.g. a `started` event has no `coord`, no `progress`, no
/// `payload`).
#[derive(Debug, Default, Serialize)]
struct ProgressEvent<'a> {
    event: &'static str,
    timestamp: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    coord: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<f64>,
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

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
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
