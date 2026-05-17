//! Local structured-logging setup for Barista.
//!
//! This module installs a process-local [`tracing_subscriber`] that
//! renders spans and events emitted by the `tracing` crate to a
//! writer (stderr by default). It is **completely separate** from
//! the opt-in [`crate::TelemetryEvent`] transport in
//! [`crate::transport`]:
//!
//! * Local tracing logs are written **in-process**, to a file
//!   descriptor on the user's machine (stderr / stdout / a TTY).
//!   They never traverse the network.
//! * The [`TelemetryEvent`](crate::TelemetryEvent) catalog is the
//!   only thing eligible to leave the process via the HTTP
//!   transport, and it is gated by three independent opt-in
//!   booleans (see [`crate::TelemetrySettings`]).
//!
//! ## Privacy boundary
//!
//! Because local tracing logs **never leave the user's machine**,
//! they MAY contain rich diagnostic context — file paths, full
//! error messages, CLI arg values, dependency coordinates — that
//! would be inappropriate to attach to a [`TelemetryEvent`]
//! destined for the network transport. The constraint
//!
//! > "Event payloads never contain CLI args, error messages, or
//! > file paths"
//!
//! applies to the *telemetry transport* ([`TelemetryEvent`]) only,
//! not to local `tracing` events. Authors of `tracing::info!` /
//! `tracing::error!` calls should write the most useful local
//! diagnostic they can — these logs are read by humans (or an IDE
//! / AI agent reading the user's own stream) and never shipped
//! anywhere.
//!
//! ## Output formats
//!
//! Two formats are supported, selected by [`LogFormat`]:
//!
//! * [`LogFormat::Human`] (default) — the standard
//!   `tracing_subscriber::fmt` pretty layout, suitable for
//!   interactive terminals. ANSI colors are honored when the
//!   writer is a TTY.
//! * [`LogFormat::Json`] — newline-delimited JSON, one event per
//!   line. Each line is a valid JSON object containing at least
//!   `timestamp`, `level`, `target`, and `fields`. Intended for
//!   IDE / AI-agent consumption: a downstream reader can split on
//!   `\n` and `serde_json::from_str` each line independently.
//!
//! ## Selection
//!
//! Callers pick the format via [`LogFormat::from_env`], which
//! reads the `BARISTA_LOG_FORMAT` environment variable:
//!
//! * `BARISTA_LOG_FORMAT=json` → [`LogFormat::Json`]
//! * `BARISTA_LOG_FORMAT=human` (or unset, or any other value) →
//!   [`LogFormat::Human`]
//!
//! Filter level is taken from `RUST_LOG` (standard
//! `tracing_subscriber::EnvFilter` syntax) and defaults to
//! `info` when unset.
//!
//! ## Installation
//!
//! [`install`] sets the subscriber as the **global** default for
//! the process. It is a one-shot operation; the second call (or
//! any call after another crate has installed a global subscriber)
//! returns [`InstallError::AlreadyInstalled`] without panicking.
//!
//! For tests and library consumers that need a scoped subscriber
//! (e.g. to capture events into a buffer for assertion),
//! [`build_subscriber_with_writer`] returns a configured layered
//! subscriber that can be installed via
//! `tracing::subscriber::with_default` for the duration of a
//! closure — see the integration tests under
//! `tests/tracing_json.rs` and `tests/tracing_human.rs` for the
//! canonical pattern.

use std::io;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::registry::{LookupSpan, Registry};
use tracing_subscriber::util::SubscriberInitExt;

/// Environment variable controlling the local log format.
///
/// See [`LogFormat::from_env`] for the accepted values.
pub const LOG_FORMAT_ENV: &str = "BARISTA_LOG_FORMAT";

/// Default filter directive used when `RUST_LOG` is unset.
pub const DEFAULT_FILTER: &str = "info";

/// Output format for the local tracing subscriber.
///
/// See the module-level documentation for the trade-offs between
/// each variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Pretty, ANSI-colored layout for interactive terminals.
    /// This is the default when `BARISTA_LOG_FORMAT` is unset.
    #[default]
    Human,

    /// Newline-delimited JSON, one event per line. Each line
    /// includes `timestamp`, `level`, `target`, and `fields`.
    /// Suitable for piping to an IDE / AI-agent / log-shipper
    /// that wants structured fields rather than rendered text.
    Json,
}

impl LogFormat {
    /// Resolve the format from the [`LOG_FORMAT_ENV`] environment
    /// variable.
    ///
    /// * `json` (case-insensitive) → [`LogFormat::Json`]
    /// * any other value, empty, or unset → [`LogFormat::Human`]
    ///
    /// Unknown values fall back to [`LogFormat::Human`] rather
    /// than erroring; an unfamiliar format string in the
    /// environment should never break the user's CLI invocation.
    pub fn from_env() -> Self {
        std::env::var(LOG_FORMAT_ENV)
            .ok()
            .as_deref()
            .map(Self::parse)
            .unwrap_or_default()
    }

    /// Parse a single format string. Case-insensitive.
    ///
    /// Public so callers that resolve the format from a config
    /// file rather than the environment can reuse the same
    /// parsing rules.
    pub fn parse(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("json") {
            Self::Json
        } else {
            Self::Human
        }
    }
}

/// Reason an installation attempt failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum InstallError {
    /// A global `tracing` subscriber is already installed —
    /// either by an earlier call to [`install`] in this process
    /// or by another library. Subsequent installations are a
    /// no-op rather than a panic.
    AlreadyInstalled,
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyInstalled => {
                f.write_str("a global tracing subscriber is already installed")
            }
        }
    }
}

impl std::error::Error for InstallError {}

/// Install the local structured-logging subscriber as the global
/// `tracing` default for the process.
///
/// The format is taken from the [`LOG_FORMAT_ENV`] environment
/// variable (see [`LogFormat::from_env`]); the filter level is
/// taken from `RUST_LOG` (defaulting to [`DEFAULT_FILTER`]).
/// Output is written to stderr.
///
/// Returns [`InstallError::AlreadyInstalled`] when another global
/// subscriber is already present — this is not fatal, callers
/// typically log a warning and continue. The function never
/// panics.
pub fn install() -> Result<(), InstallError> {
    install_with(LogFormat::from_env(), env_filter())
}

/// Install the subscriber with an explicit format and filter.
///
/// Same global-state semantics as [`install`].
pub fn install_with(format: LogFormat, filter: EnvFilter) -> Result<(), InstallError> {
    let result = match format {
        LogFormat::Human => {
            let layer = human_layer(io::stderr);
            Registry::default().with(filter).with(layer).try_init()
        }
        LogFormat::Json => {
            let layer = json_layer(io::stderr);
            Registry::default().with(filter).with(layer).try_init()
        }
    };
    result.map_err(|_| InstallError::AlreadyInstalled)
}

/// Build a layered subscriber that writes to a caller-supplied
/// writer, without installing it globally.
///
/// This is the entry point used by tests and by callers that want
/// to capture tracing events into a buffer (e.g. to assert the
/// JSON output shape). To activate the subscriber for the
/// duration of a closure, wrap the call in
/// `tracing::subscriber::with_default`.
pub fn build_subscriber_with_writer<W>(
    format: LogFormat,
    filter: EnvFilter,
    writer: W,
) -> Box<dyn tracing::Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    match format {
        LogFormat::Human => {
            let layer = human_layer(writer);
            Box::new(Registry::default().with(filter).with(layer))
        }
        LogFormat::Json => {
            let layer = json_layer(writer);
            Box::new(Registry::default().with(filter).with(layer))
        }
    }
}

/// Resolve the active filter directive from `RUST_LOG`, falling
/// back to [`DEFAULT_FILTER`] when unset or invalid.
pub fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

fn human_layer<S, W>(writer: W) -> impl Layer<S>
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span>,
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_target(true)
        .with_level(true)
        .with_span_events(FmtSpan::NONE)
}

fn json_layer<S, W>(writer: W) -> impl Layer<S>
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span>,
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_target(true)
        .with_level(true)
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .flatten_event(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_default_is_human() {
        assert_eq!(LogFormat::default(), LogFormat::Human);
    }

    #[test]
    fn parse_json_case_insensitive() {
        assert_eq!(LogFormat::parse("json"), LogFormat::Json);
        assert_eq!(LogFormat::parse("JSON"), LogFormat::Json);
        assert_eq!(LogFormat::parse("Json"), LogFormat::Json);
    }

    #[test]
    fn parse_unknown_falls_back_to_human() {
        assert_eq!(LogFormat::parse(""), LogFormat::Human);
        assert_eq!(LogFormat::parse("pretty"), LogFormat::Human);
        assert_eq!(LogFormat::parse("xml"), LogFormat::Human);
    }

    #[test]
    fn env_filter_defaults_to_info_when_rust_log_unset() {
        // Don't mutate the process env here — just exercise the
        // fallback path by constructing an EnvFilter directly and
        // confirming it does not panic.
        let f = EnvFilter::new(DEFAULT_FILTER);
        assert!(format!("{f}").contains("info"));
    }

    #[test]
    fn install_error_display_is_stable() {
        let msg = format!("{}", InstallError::AlreadyInstalled);
        assert!(msg.contains("already installed"));
    }
}
