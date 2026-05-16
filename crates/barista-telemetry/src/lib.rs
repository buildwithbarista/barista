// Workspace security lints (clippy::unwrap_used, clippy::expect_used,
// clippy::panic, clippy::as_conversions) are warned on workspace-wide via
// the root `Cargo.toml`. Pre-existing transport-test scaffolding is
// allowed here pending an incremental ratchet.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Opt-in telemetry transport for Barista.
//!
//! # Default-off
//!
//! Telemetry is opt-in. The default-constructed [`TelemetrySettings`]
//! has `enabled = false`, and **no API on this crate can flip that
//! bit implicitly**. The user has to either set
//! `[telemetry] enabled = true` in `~/.barista/config.toml` or set
//! `BARISTA_TELEMETRY__ENABLED=1` in the environment. Both paths
//! flow through `barista-config`'s layered loader before this
//! crate sees the resulting [`TelemetrySettings`].
//!
//! # Hard guarantee: the disabled path is a tail-call no-op
//!
//! When [`Telemetry::is_active`] returns `false`, every emit method
//! returns immediately at the top of the function — before any
//! allocation, any [`TelemetryEvent`] construction, any sink
//! dispatch, any I/O. This is the central correctness property of
//! the crate and is regression-tested in
//! [`tests/zero_network.rs`](../../tests/zero_network.rs):
//!
//! * `disabled_default_panic_sink_never_fires` — wires a
//!   [`PanicOnAccessSink`] under a disabled handle and emits a
//!   `CommandInvoked` event. The test passes iff the sink is
//!   never reached.
//! * `disabled_with_env_override_off_panic_sink_never_fires` —
//!   same property after `BARISTA_TELEMETRY__ENABLED=0` is folded
//!   through the config loader.
//!
//! The disabled path also never:
//!
//! * Opens a socket (there is no socket type instantiated; the
//!   crate has no HTTP dependency).
//! * Touches the filesystem (no `std::fs` calls anywhere in the
//!   emit chain).
//! * Allocates (`is_active` is an inline field read; the early
//!   return precedes any `String`/`Box`/`Vec` construction).
//!
//! # What ships in this revision
//!
//! This is the gating infrastructure only:
//!
//! * [`TelemetrySettings`] — the three-field config slice that
//!   mirrors `barista-config`'s `[telemetry]` section. Owned by
//!   this crate so downstream consumers can depend on
//!   `barista-telemetry` without pulling in the whole config
//!   crate.
//! * [`Telemetry`] — the handle the rest of the codebase will
//!   hold. Constructed via [`Telemetry::from_settings`] (or
//!   [`Telemetry::with_sink`] for non-default transports).
//! * [`TelemetrySink`] — the trait every real transport will
//!   implement. The only sink shipped in this crate is
//!   [`NullSink`], which drops events on the floor — the
//!   "telemetry is enabled but no endpoint is configured" case.
//! * [`PanicOnAccessSink`] — test-only sink that panics if
//!   reached. Used to assert the disabled-path no-op guarantee.
//! * [`TelemetryEvent`] — a placeholder enum with a single
//!   `CommandInvoked` variant so the trait compiles. The full
//!   event catalog (build, pour, pull, daemon-lifecycle, etc.)
//!   lands in a subsequent task.
//!
//! Notably absent — and intentional:
//!
//! * **No HTTP client.** There is no `reqwest` / `ureq` /
//!   `hyper` dependency. The actual transport sink lands in a
//!   subsequent task.
//! * **No background task / thread.** Emit is synchronous; the
//!   sink decides whether to buffer.
//! * **No identifier generation.** If `client_id` is `None`,
//!   none is invented or persisted.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use serde::{Deserialize, Serialize};

pub mod tracing;
pub mod transport;

pub use transport::{
    HttpSinkInitError, HttpTelemetrySink, HttpTransport, MockHttpCall, MockHttpTransport,
    ReqwestTransport, TransportError,
};

// ============================================================
// Settings
// ============================================================

/// Effective telemetry settings as resolved by `barista-config`.
///
/// Mirrors `barista_config::TelemetryConfig` but is owned by this
/// crate so consumers can wire transports without depending on
/// the config crate directly. The default is
/// `{ enabled: false, endpoint: None, client_id: None,
/// transport_enabled: false }` — i.e. fully off and unconfigured.
///
/// # Three independent guards
///
/// The HTTP transport is gated behind three separate booleans —
/// **all** of which must be true before a request is sent:
///
/// 1. [`enabled`](Self::enabled) — the user has opted in to
///    telemetry at all.
/// 2. [`endpoint`](Self::endpoint) is `Some(_)` — a destination
///    URL exists.
/// 3. [`transport_enabled`](Self::transport_enabled) — the
///    transport is allowed to fire. Held off until the privacy
///    posture has been reviewed and signed off; flipped on in a
///    later release once the privacy document lands.
///
/// All three default to "off" so the network path is unreachable
/// out of the box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct TelemetrySettings {
    /// Whether telemetry is enabled. Default `false`.
    ///
    /// **This is the user-facing opt-in.** The crate has no other
    /// way to enable emission. There is no environment variable
    /// read inside this crate; the env-var override happens
    /// upstream in `barista-config` before settings are
    /// constructed.
    pub enabled: bool,

    /// Endpoint URL the transport posts to. `None` means no
    /// transport is configured — even with `enabled = true` the
    /// default handle uses [`NullSink`] and events are dropped.
    pub endpoint: Option<String>,

    /// Stable opaque per-install identifier. `None` means no
    /// per-install ID is attached to outgoing events. This crate
    /// does **not** invent one if absent.
    pub client_id: Option<String>,

    /// Master switch for the HTTP transport. Default `false`.
    ///
    /// **This is the post-privacy-review go-live lever.** Even
    /// when the user has set `enabled = true` and configured an
    /// `endpoint`, no HTTP request leaves the process until this
    /// is `true`. The rationale: the wire shape of events, the
    /// destination, and the privacy contract need to ship and be
    /// reviewed before the transport is allowed to fire. Once the
    /// privacy doc lands (M3.3 T5) and the v0.2 release approves
    /// going live, this defaults to `true` (or the field is
    /// dropped entirely). Until then, the transport stub is
    /// implemented but unreachable by default.
    pub transport_enabled: bool,
}

impl TelemetrySettings {
    /// Construct a disabled settings block. Equivalent to
    /// [`TelemetrySettings::default`] but spelled out for callers
    /// who want the intent to be obvious at the use site.
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            client_id: None,
            transport_enabled: false,
        }
    }
}

// ============================================================
// Events
// ============================================================

/// A telemetry event payload.
///
/// Placeholder catalog — only one variant ships in this revision.
/// The full event shapes (build start/finish, pour, pull,
/// daemon-lifecycle, error categories) land in a subsequent
/// task.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TelemetryEvent {
    /// A top-level CLI subcommand was invoked. `name` is the
    /// static command name (`"pour"`, `"pull"`, etc.); never
    /// user-provided text, never a path.
    CommandInvoked {
        /// Static subcommand name.
        name: &'static str,
    },
}

// ============================================================
// Sink trait
// ============================================================

/// Transport-side handler for telemetry events.
///
/// Implementations decide what to do with the event — typically
/// buffer + POST to the endpoint. Implementations **must not**
/// panic on submission of well-formed events; the disabled path
/// is the only place where panicking on access is intended (see
/// [`PanicOnAccessSink`]).
pub trait TelemetrySink: Send + Sync {
    /// Hand off a single event for transport. Called only when
    /// the parent [`Telemetry`] is active; callers do not need
    /// to re-check the enabled bit.
    fn submit(&self, event: TelemetryEvent);
}

/// No-op sink. Drops every event silently and performs no I/O.
///
/// This is the default sink picked by [`Telemetry::from_settings`]
/// when no specific transport has been wired up — i.e. the
/// "telemetry is enabled but there's nowhere to send to" case.
/// It performs no allocation, opens no socket, and touches no
/// file.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl TelemetrySink for NullSink {
    #[inline]
    fn submit(&self, _event: TelemetryEvent) {
        // Intentionally empty. No I/O, no allocation.
    }
}

/// Test sink that panics if ever reached.
///
/// Used to assert the disabled-path no-op guarantee: wire one of
/// these under a [`Telemetry`] constructed from disabled settings,
/// emit events, and the test passes iff the panic never fires.
///
/// Public (not `#[cfg(test)]`-only) so downstream crates can
/// reuse the same guarantee in their own integration tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct PanicOnAccessSink;

impl TelemetrySink for PanicOnAccessSink {
    fn submit(&self, event: TelemetryEvent) {
        panic!(
            "PanicOnAccessSink::submit called — telemetry was \
             expected to be off; received event: {event:?}"
        );
    }
}

// ============================================================
// Handle
// ============================================================

/// Telemetry handle held by the rest of the codebase.
///
/// Constructed once at startup from [`TelemetrySettings`] and
/// passed by reference to subsystems that emit events. The
/// handle is cheap to clone via `Arc`-wrapping at the call site
/// if needed; the type itself is not `Clone` to keep ownership
/// of the boxed sink explicit.
///
/// # Disabled-path guarantee
///
/// When [`Telemetry::is_active`] returns `false`, every emit
/// method returns at the very top of its body — before any
/// allocation, event construction, or sink access. See the
/// crate-level docs for the test that pins this property.
pub struct Telemetry {
    enabled: bool,
    sink: Box<dyn TelemetrySink>,
    settings: TelemetrySettings,
}

impl std::fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telemetry")
            .field("enabled", &self.enabled)
            .field("settings", &self.settings)
            .field("sink", &"<dyn TelemetrySink>")
            .finish()
    }
}

impl Telemetry {
    /// Construct a [`Telemetry`] handle from resolved settings,
    /// using [`NullSink`] as the default transport.
    ///
    /// When `settings.enabled == false` the resulting handle is
    /// guaranteed inert: every emit method short-circuits before
    /// reaching the sink. The sink itself is still constructed
    /// (it's a zero-sized [`NullSink`]) but never touched.
    pub fn from_settings(settings: &TelemetrySettings) -> Self {
        Self {
            enabled: settings.enabled,
            sink: Box::new(NullSink),
            settings: settings.clone(),
        }
    }

    /// Construct a [`Telemetry`] handle with a caller-supplied
    /// sink. Used by transports outside this crate and by tests
    /// that want to swap in a [`PanicOnAccessSink`] or a
    /// capturing sink.
    pub fn with_sink<S>(settings: &TelemetrySettings, sink: S) -> Self
    where
        S: TelemetrySink + 'static,
    {
        Self {
            enabled: settings.enabled,
            sink: Box::new(sink),
            settings: settings.clone(),
        }
    }

    /// Construct a permanently-disabled handle. Equivalent to
    /// `Telemetry::from_settings(&TelemetrySettings::disabled())`
    /// but spelled out for callers who want the intent obvious.
    pub fn disabled() -> Self {
        Self::from_settings(&TelemetrySettings::disabled())
    }

    /// Returns whether this handle will actually emit events.
    ///
    /// This is the single source of truth for "is telemetry
    /// live?". Callers that do their own conditional work
    /// upstream of an emit (e.g. timing measurements) should
    /// guard that work behind this check so the cost is paid
    /// only when telemetry is on.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.enabled
    }

    /// Returns a reference to the settings this handle was
    /// built from. Useful for logging / debugging.
    pub fn settings(&self) -> &TelemetrySettings {
        &self.settings
    }

    /// Emit a previously-constructed [`TelemetryEvent`].
    ///
    /// Returns immediately (no allocation, no sink dispatch)
    /// when the handle is disabled.
    #[inline]
    pub fn emit(&self, event: TelemetryEvent) {
        if !self.enabled {
            return;
        }
        self.sink.submit(event);
    }

    /// Record a `CommandInvoked` event.
    ///
    /// The full event-shape catalog (build start/finish, pour,
    /// pull, daemon-lifecycle, etc.) lands in a subsequent task;
    /// this one variant is plumbed end-to-end so the trait,
    /// handle, and sink contracts have a concrete user.
    ///
    /// Returns immediately when the handle is disabled — the
    /// event struct is not even constructed.
    #[inline]
    pub fn record_command_invoked(&self, name: &'static str) {
        if !self.enabled {
            return;
        }
        self.sink.submit(TelemetryEvent::CommandInvoked { name });
    }
}

// Conversion convenience so `barista-cli` can plumb settings
// without manually copying fields. Gated on the consumer enabling
// the dependency; we don't depend on barista-config in this
// crate's runtime deps to keep the dependency edge one-way.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_are_disabled() {
        let s = TelemetrySettings::default();
        assert!(!s.enabled);
        assert!(s.endpoint.is_none());
        assert!(s.client_id.is_none());
    }

    #[test]
    fn disabled_const_matches_default() {
        assert_eq!(TelemetrySettings::default(), TelemetrySettings::disabled());
    }

    #[test]
    fn handle_from_disabled_settings_is_inactive() {
        let t = Telemetry::from_settings(&TelemetrySettings::default());
        assert!(!t.is_active());
    }

    #[test]
    fn handle_from_enabled_settings_is_active() {
        let s = TelemetrySettings {
            enabled: true,
            endpoint: Some("https://telemetry.example/v1".into()),
            client_id: None,
            transport_enabled: false,
        };
        let t = Telemetry::from_settings(&s);
        assert!(t.is_active());
    }

    #[test]
    fn settings_round_trip_through_toml() {
        let s = TelemetrySettings {
            enabled: true,
            endpoint: Some("https://example.test/ingest".into()),
            client_id: Some("ci-001".into()),
            transport_enabled: true,
        };
        let serialized = toml::to_string(&s).unwrap();
        let back: TelemetrySettings = toml::from_str(&serialized).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn telemetry_event_serializes_with_kind_tag() {
        let e = TelemetryEvent::CommandInvoked { name: "pour" };
        let s = serde_json::to_string(&e).unwrap();
        // External tagging by `kind` is part of the public wire
        // shape; pin it.
        assert!(s.contains("\"kind\""), "missing kind tag: {s}");
        assert!(s.contains("command_invoked"), "wrong variant tag: {s}");
        assert!(s.contains("\"name\":\"pour\""), "missing name: {s}");
    }
}
