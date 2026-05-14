//! HTTP transport for telemetry — implemented but off by default.
//!
//! # Three independent guards
//!
//! Even when this sink is reached, an HTTP request is sent only
//! when **all three** of the following are true:
//!
//! 1. [`TelemetrySettings::enabled`] — the user-facing opt-in.
//! 2. [`TelemetrySettings::endpoint`] is `Some(_)` — a
//!    destination URL exists.
//! 3. [`TelemetrySettings::transport_enabled`] — the
//!    post-privacy-review go-live lever.
//!
//! Any of the three being `false` causes the call to drop
//! silently (with no allocation beyond the event already on the
//! stack). The transport path is *implemented* here so the
//! pipeline can be exercised end-to-end in tests, but it is
//! *unreachable by default* — flipping the switch is a separate
//! step that lands once the privacy doc (M3.3 T5) is signed off.
//!
//! # Wire format
//!
//! - `POST <endpoint>`
//! - `Content-Type: application/json`
//! - `User-Agent: barista/<crate-version>`
//! - Body: the JSON serialization of [`TelemetryEvent`]
//!   (externally tagged via `kind`, see the trait module).
//! - Timeout: 5 seconds. **No retries** in v0.1 — failed sends
//!   are counted and dropped. Retry/batch policy lands in v0.2.
//! - No cookies, no auth headers.
//!
//! # Error handling
//!
//! `HttpTelemetrySink::submit` must not panic on well-formed
//! events. Transport errors (timeout, DNS, non-2xx) are counted
//! into [`HttpTelemetrySink::dropped_count`] and otherwise
//! swallowed. Construction with an unparseable URL returns an
//! `Err` from [`HttpTelemetrySink::new`] rather than panicking.
//!
//! # Async / sync
//!
//! Consumers in v0.1 are sync, so this sink uses
//! `reqwest::blocking`. The blocking client builds its own
//! single-thread Tokio runtime internally; the telemetry crate
//! itself remains free of an async API surface.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::{TelemetryEvent, TelemetrySettings, TelemetrySink};

/// `User-Agent` header value, including the crate version. Const
/// so we don't reallocate on each call.
const USER_AGENT: &str = concat!("barista/", env!("CARGO_PKG_VERSION"));

/// Per-request timeout. **Not** configurable in v0.1 — keep the
/// stub deliberately uninteresting.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Error returned from [`HttpTelemetrySink::new`] when the
/// supplied endpoint is not a syntactically valid URL or when
/// the underlying HTTP client cannot be constructed.
///
/// We never panic on a bad URL — the user can perfectly well
/// type `endpoint = "not a url"` in their config file. The error
/// is returned so the caller can decide whether to fall back to
/// [`NullSink`](crate::NullSink) or refuse to start.
#[derive(Debug)]
pub struct HttpSinkInitError {
    /// The raw endpoint string that failed to parse (or empty if
    /// the failure was in the client builder rather than the URL).
    pub endpoint: String,
    /// Free-form diagnostic message. Kept as a string so we
    /// don't leak the concrete error type from `reqwest` or
    /// `url` through the public API.
    pub message: String,
}

impl std::fmt::Display for HttpSinkInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.endpoint.is_empty() {
            write!(f, "telemetry transport init failed: {}", self.message)
        } else {
            write!(
                f,
                "telemetry endpoint {:?} is not usable: {}",
                self.endpoint, self.message
            )
        }
    }
}

impl std::error::Error for HttpSinkInitError {}

// ============================================================
// Transport abstraction
// ============================================================

/// Internal transport contract — the thing that actually sends
/// (or pretends to send) the request.
///
/// Split out from [`HttpTelemetrySink`] so tests can swap in a
/// [`MockHttpTransport`] that records calls without ever opening
/// a socket. The trait is `pub` so external integration tests
/// can implement it; the production type ([`ReqwestTransport`])
/// is the only implementation that does real network I/O.
pub trait HttpTransport: Send + Sync {
    /// Send the JSON `body` to `url`. Returns `Ok(())` on
    /// success, `Err` on any failure (transport, non-2xx, etc.).
    /// Implementations **must not** panic on a well-formed body.
    fn post_json(&self, url: &str, body: &[u8]) -> Result<(), TransportError>;
}

/// Opaque transport error. We don't surface details upstream —
/// the sink swallows these and increments a counter. The type is
/// public so test transports can return their own errors.
#[derive(Debug)]
pub struct TransportError {
    /// Free-form diagnostic. Not part of the public wire surface;
    /// callers should treat this as opaque.
    pub message: String,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "telemetry transport error: {}", self.message)
    }
}

impl std::error::Error for TransportError {}

impl From<reqwest::Error> for TransportError {
    fn from(e: reqwest::Error) -> Self {
        Self {
            message: e.to_string(),
        }
    }
}

// ============================================================
// Real (reqwest) transport
// ============================================================

/// Production transport: a `reqwest::blocking::Client` configured
/// with the 5-second timeout and barista User-Agent.
///
/// Cheap to clone (the client itself is `Arc`-backed internally),
/// but we don't expose that — the sink holds a single instance.
pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
}

impl std::fmt::Debug for ReqwestTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReqwestTransport").finish_non_exhaustive()
    }
}

impl ReqwestTransport {
    /// Build a transport with the v0.1 defaults: 5 s timeout,
    /// rustls TLS stack, the static barista User-Agent.
    pub fn new() -> Result<Self, reqwest::Error> {
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self { client })
    }
}

impl HttpTransport for ReqwestTransport {
    fn post_json(&self, url: &str, body: &[u8]) -> Result<(), TransportError> {
        let resp = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec())
            .send()?;
        if !resp.status().is_success() {
            return Err(TransportError {
                message: format!("non-success status: {}", resp.status()),
            });
        }
        Ok(())
    }
}

// ============================================================
// Test transport
// ============================================================

/// Captured invocation of [`MockHttpTransport::post_json`].
///
/// Public so integration tests can pattern-match on the body to
/// assert that telemetry events serialize exactly as expected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockHttpCall {
    /// The endpoint URL the sink tried to POST to.
    pub url: String,
    /// The raw JSON body bytes the sink tried to send.
    pub body: Vec<u8>,
}

/// Test-only transport. Records every call instead of opening a
/// socket. Cheap to clone (handles are `Arc`-wrapped).
///
/// # Why public
///
/// Exposed so downstream crates that wire telemetry into their
/// own subsystems can reuse the same recording fixture in their
/// integration tests, rather than each crate growing its own
/// near-identical mock. The mock is intentionally minimal — it
/// records, it can be configured to fail, and that's it.
#[derive(Debug, Clone, Default)]
pub struct MockHttpTransport {
    calls: Arc<Mutex<Vec<MockHttpCall>>>,
    fail_next: Arc<AtomicUsize>,
}

impl MockHttpTransport {
    /// Construct an empty mock. No calls recorded, no scheduled
    /// failures.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of `post_json` calls observed so far. Used by the
    /// zero-network-by-default tests to assert the sink never
    /// reached the transport when any guard was off.
    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("mock transport mutex").len()
    }

    /// Snapshot the recorded calls. Returns a clone so the lock
    /// is held only briefly.
    pub fn calls(&self) -> Vec<MockHttpCall> {
        self.calls.lock().expect("mock transport mutex").clone()
    }

    /// Schedule the next `n` calls to return `Err`. Useful for
    /// asserting that transport failures are swallowed and
    /// counted into [`HttpTelemetrySink::dropped_count`].
    pub fn fail_next(&self, n: usize) {
        self.fail_next.store(n, Ordering::SeqCst);
    }
}

impl HttpTransport for MockHttpTransport {
    fn post_json(&self, url: &str, body: &[u8]) -> Result<(), TransportError> {
        self.calls
            .lock()
            .expect("mock transport mutex")
            .push(MockHttpCall {
                url: url.to_string(),
                body: body.to_vec(),
            });
        let remaining = self.fail_next.load(Ordering::SeqCst);
        if remaining > 0 {
            self.fail_next.store(remaining - 1, Ordering::SeqCst);
            return Err(TransportError {
                message: "mock-scheduled failure".into(),
            });
        }
        Ok(())
    }
}

// ============================================================
// HttpTelemetrySink
// ============================================================

/// Telemetry sink that POSTs events to a configured HTTP
/// endpoint — but only when all three guards (`enabled`,
/// `endpoint.is_some()`, `transport_enabled`) are satisfied.
///
/// Construct with [`HttpTelemetrySink::new`] (uses
/// [`ReqwestTransport`]) or with [`HttpTelemetrySink::with_transport`]
/// (for tests). When any guard is `false`, [`Self::submit`] is a
/// silent no-op: no network I/O, no serialization, no log.
pub struct HttpTelemetrySink {
    /// Snapshot of the resolved settings. Owned because the
    /// caller may drop the original.
    settings: TelemetrySettings,
    /// The actual transport. `Arc<dyn HttpTransport>` so the
    /// sink is `Clone`-friendly if a consumer wants to share it
    /// across threads (the trait already requires `Send + Sync`).
    transport: Arc<dyn HttpTransport>,
    /// Counter of events whose serialization or POST failed.
    /// Read via [`Self::dropped_count`] for diagnostics.
    dropped: Arc<AtomicUsize>,
}

impl std::fmt::Debug for HttpTelemetrySink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTelemetrySink")
            .field("settings", &self.settings)
            .field("dropped", &self.dropped.load(Ordering::SeqCst))
            .field("transport", &"<dyn HttpTransport>")
            .finish()
    }
}

impl HttpTelemetrySink {
    /// Build a sink backed by a real [`ReqwestTransport`].
    ///
    /// Returns `Err(HttpSinkInitError)` if the endpoint is set
    /// but not a syntactically valid URL. **Does not panic on a
    /// bad URL** — callers can choose to fall back to
    /// [`NullSink`](crate::NullSink) and log.
    ///
    /// Note: even on `Ok(_)`, the sink may be fully inert at
    /// `submit` time if `enabled`, `transport_enabled`, or the
    /// endpoint are not all set. The constructor does not
    /// short-circuit on any guard — it just builds the
    /// machinery, and the runtime gating happens at the
    /// `submit` boundary.
    pub fn new(settings: &TelemetrySettings) -> Result<Self, HttpSinkInitError> {
        // Pre-flight: if an endpoint is configured, sanity-check
        // that it parses as a URL. We use reqwest's re-exported
        // `Url::parse` so the shape matches what the real
        // client would accept. **No panic on a bad URL** — the
        // user can type `endpoint = "not a url"` in their
        // config file and we want a clean `Err`.
        if let Some(url) = &settings.endpoint {
            if let Err(e) = reqwest::Url::parse(url) {
                return Err(HttpSinkInitError {
                    endpoint: url.clone(),
                    message: e.to_string(),
                });
            }
        }
        let transport = ReqwestTransport::new().map_err(|source| HttpSinkInitError {
            endpoint: settings.endpoint.clone().unwrap_or_default(),
            message: source.to_string(),
        })?;
        Ok(Self {
            settings: settings.clone(),
            transport: Arc::new(transport),
            dropped: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Build a sink backed by a caller-supplied transport.
    /// Primary use case is wiring a [`MockHttpTransport`] in
    /// tests; downstream production code should prefer
    /// [`Self::new`].
    pub fn with_transport<T>(settings: &TelemetrySettings, transport: T) -> Self
    where
        T: HttpTransport + 'static,
    {
        Self {
            settings: settings.clone(),
            transport: Arc::new(transport),
            dropped: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Number of events that reached the transport but failed
    /// to send (transport error, non-2xx response, or
    /// serialization failure). Exposed for diagnostics; not
    /// reset by anything in this crate.
    pub fn dropped_count(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }
}

impl TelemetrySink for HttpTelemetrySink {
    fn submit(&self, event: TelemetryEvent) {
        // Guard 1: master opt-in. Should not normally fire —
        // the `Telemetry` handle already short-circuits on
        // `enabled == false` before reaching the sink — but
        // re-check defensively so a caller that constructed the
        // sink directly still gets the no-network guarantee.
        if !self.settings.enabled {
            return;
        }
        // Guard 2: privacy-review go-live lever.
        if !self.settings.transport_enabled {
            // T3 lands `tracing`; once it does, this becomes:
            //   tracing::trace!(
            //     event_kind = ?event,
            //     "telemetry transport disabled; would have sent event"
            //   );
            // Until then, deliberately silent — no eprintln, no
            // log dep — so the disabled path remains zero-side-effect.
            return;
        }
        // Guard 3: no destination configured.
        let endpoint = match self.settings.endpoint.as_deref() {
            Some(url) => url,
            None => return,
        };

        // All guards passed — serialize and send. Swallow
        // errors; bump the dropped counter for diagnostics.
        let body = match serde_json::to_vec(&event) {
            Ok(b) => b,
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::SeqCst);
                return;
            }
        };
        if self.transport.post_json(endpoint, &body).is_err() {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}
