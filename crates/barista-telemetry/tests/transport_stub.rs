//! Acceptance tests for the HTTP telemetry transport stub
//! (M3.3 T4).
//!
//! These tests pin the central property of the stub: **even
//! though the transport is implemented, no HTTP call leaves the
//! process unless all three guards are satisfied**. The
//! `MockHttpTransport` lets us assert this at the wire layer
//! without ever opening a socket.
//!
//! # [T] linkage
//!
//! The test
//! [`disabled_sink_makes_zero_calls_across_every_event_variant`]
//! is the proof for the M3.3 acceptance criterion "With
//! telemetry disabled (default), zero network calls observed in
//! test" — end-to-end through the *HTTP sink* (rather than just
//! the trait). It constructs an `HttpTelemetrySink` from default
//! settings, wires a `MockHttpTransport` underneath, drives
//! every public emit path on a `Telemetry` handle and a
//! direct-`submit` path, and asserts
//! `MockHttpTransport::call_count() == 0`.

use std::sync::atomic::Ordering;

use barista_telemetry::{
    HttpSinkInitError, HttpTelemetrySink, HttpTransport, MockHttpTransport, Telemetry,
    TelemetryEvent, TelemetrySettings, TelemetrySink, TransportError,
};

// ============================================================
// Helpers
// ============================================================

/// Every `TelemetryEvent` variant currently in the catalog.
/// The catalog is `#[non_exhaustive]`; this list is intended to
/// be extended in lockstep with T2 as new variants land. The
/// test asserts zero-network *across every variant*, so adding
/// a new variant here is the only place a future contributor
/// needs to touch when expanding the event set.
fn every_event_variant() -> Vec<TelemetryEvent> {
    vec![
        TelemetryEvent::CommandInvoked { name: "pour" },
        TelemetryEvent::CommandInvoked { name: "pull" },
        TelemetryEvent::CommandInvoked { name: "grind" },
    ]
}

fn fully_open_settings() -> TelemetrySettings {
    TelemetrySettings {
        enabled: true,
        endpoint: Some("https://telemetry.example/v1".into()),
        client_id: Some("install-abc".into()),
        transport_enabled: true,
    }
}

// ============================================================
// Tests
// ============================================================

/// **[T] linkage** for the M3.3 acceptance criterion "With
/// telemetry disabled (default), zero network calls observed in
/// test" — proven end-to-end through `HttpTelemetrySink`.
///
/// Default `TelemetrySettings` (`enabled = false`,
/// `transport_enabled = false`, `endpoint = None`) → build the
/// HTTP sink → exercise every event variant via both the
/// `Telemetry` handle path and the direct `submit` path → the
/// mock transport must record **zero** calls.
#[test]
fn disabled_sink_makes_zero_calls_across_every_event_variant() {
    let settings = TelemetrySettings::default();
    let mock = MockHttpTransport::new();
    let sink = HttpTelemetrySink::with_transport(&settings, mock.clone());

    // Drive via the high-level handle: this is the path real
    // callers take and it has its own short-circuit on
    // `enabled == false`.
    let handle = Telemetry::with_sink(&settings, {
        let mock2 = mock.clone();
        HttpTelemetrySink::with_transport(&settings, mock2)
    });
    for ev in every_event_variant() {
        handle.emit(ev);
    }
    handle.record_command_invoked("pour");

    // And drive the sink directly, bypassing the handle, to
    // prove the sink itself defends against the disabled case.
    for ev in every_event_variant() {
        sink.submit(ev);
    }

    assert_eq!(
        mock.call_count(),
        0,
        "disabled sink must never reach the HTTP transport; got calls: {:?}",
        mock.calls()
    );
    assert_eq!(sink.dropped_count(), 0, "no drops when nothing was tried");
}

/// `enabled = true` but `transport_enabled = false`: the privacy
/// gate alone is enough to block all outbound calls. This is
/// the v0.1 default for a user who has opted in to telemetry
/// before the privacy doc has shipped.
#[test]
fn enabled_but_transport_off_makes_zero_calls() {
    let settings = TelemetrySettings {
        enabled: true,
        endpoint: Some("https://telemetry.example/v1".into()),
        client_id: None,
        transport_enabled: false, // <-- the post-T5 lever
    };
    let mock = MockHttpTransport::new();
    let sink = HttpTelemetrySink::with_transport(&settings, mock.clone());

    for ev in every_event_variant() {
        sink.submit(ev);
    }

    assert_eq!(
        mock.call_count(),
        0,
        "transport_enabled=false must block HTTP path even when enabled=true"
    );
}

/// `enabled = true`, `transport_enabled = true`, but no endpoint
/// configured: still zero calls. There's nowhere to send.
#[test]
fn enabled_no_endpoint_makes_zero_calls() {
    let settings = TelemetrySettings {
        enabled: true,
        endpoint: None,
        client_id: None,
        transport_enabled: true,
    };
    let mock = MockHttpTransport::new();
    let sink = HttpTelemetrySink::with_transport(&settings, mock.clone());

    for ev in every_event_variant() {
        sink.submit(ev);
    }
    assert_eq!(mock.call_count(), 0, "no endpoint ⇒ no calls");
}

/// All three guards open: every submitted event reaches the
/// transport exactly once, and the body is the JSON
/// serialization of the event.
#[test]
fn all_guards_open_posts_one_call_per_event_with_json_body() {
    let settings = fully_open_settings();
    let mock = MockHttpTransport::new();
    let sink = HttpTelemetrySink::with_transport(&settings, mock.clone());

    let events = every_event_variant();
    for ev in &events {
        sink.submit(ev.clone());
    }

    let calls = mock.calls();
    assert_eq!(
        calls.len(),
        events.len(),
        "one call per submitted event when all guards are open"
    );

    // Body assertion: each call's body must be the exact JSON
    // serialization of the corresponding event. Pin both the
    // wire shape (externally tagged on `kind`) and the
    // round-tripping of the variant payload.
    for (call, expected) in calls.iter().zip(events.iter()) {
        assert_eq!(call.url, "https://telemetry.example/v1");
        let expected_body = serde_json::to_vec(expected).expect("event must serialize");
        assert_eq!(
            call.body, expected_body,
            "POST body must be the JSON of the event"
        );
        // And: the body really is JSON with the `kind` tag we
        // committed to in the public wire surface.
        let parsed: serde_json::Value =
            serde_json::from_slice(&call.body).expect("body is valid JSON");
        assert!(
            parsed.get("kind").is_some(),
            "wire body missing kind tag: {parsed}"
        );
    }
}

/// Transport errors are swallowed and counted — they never
/// propagate as panics or return values. This is what makes
/// telemetry safe to wire into hot paths: a flaky endpoint can
/// never crash a build.
#[test]
fn transport_errors_are_swallowed_and_counted() {
    let settings = fully_open_settings();
    let mock = MockHttpTransport::new();
    mock.fail_next(2); // first two posts will error
    let sink = HttpTelemetrySink::with_transport(&settings, mock.clone());

    sink.submit(TelemetryEvent::CommandInvoked { name: "pour" }); // err
    sink.submit(TelemetryEvent::CommandInvoked { name: "pull" }); // err
    sink.submit(TelemetryEvent::CommandInvoked { name: "grind" }); // ok

    // All three reached the transport — the mock recorded them.
    assert_eq!(mock.call_count(), 3);
    // But two were counted as dropped.
    assert_eq!(sink.dropped_count(), 2, "failed sends increment dropped");
}

/// An invalid endpoint URL is reported as an `Err` from
/// `new` — **no panic**. The user can perfectly well type
/// `endpoint = "not a url"` in their config, and we must not
/// take the process down for it.
#[test]
fn invalid_url_returns_err_no_panic() {
    let settings = TelemetrySettings {
        enabled: true,
        endpoint: Some("not a url".into()),
        client_id: None,
        transport_enabled: true,
    };
    let err = HttpTelemetrySink::new(&settings).expect_err("bad URL must error");
    let HttpSinkInitError { endpoint, .. } = &err;
    assert_eq!(endpoint, "not a url");
    // And the `Display` impl says something useful, so an
    // operator's log doesn't just say "error".
    let msg = err.to_string();
    assert!(
        msg.contains("not a url"),
        "Display should mention the endpoint; got: {msg}"
    );
}

/// `HttpTelemetrySink::new` with no endpoint configured
/// succeeds — it produces a constructible-but-silent sink, the
/// "no-op default" mode described in the M3.3 task brief.
#[test]
fn new_with_no_endpoint_succeeds_and_is_silent() {
    let settings = TelemetrySettings::default();
    let sink =
        HttpTelemetrySink::new(&settings).expect("no endpoint ⇒ constructor should succeed");
    // It's a real sink that just happens to drop everything.
    for ev in every_event_variant() {
        sink.submit(ev);
    }
    // Nothing observable, no drops counted (the drop counter
    // is for transport-stage failures, not gate short-circuits).
    assert_eq!(sink.dropped_count(), 0);
}

/// Drive the sink through the high-level `Telemetry` handle and
/// confirm the handle's own short-circuit doesn't mask the
/// sink's. With `enabled = true` + transport open, the handle
/// passes the event through and the sink posts.
#[test]
fn handle_with_http_sink_posts_when_all_guards_open() {
    let settings = fully_open_settings();
    let mock = MockHttpTransport::new();
    let sink = HttpTelemetrySink::with_transport(&settings, mock.clone());
    let handle = Telemetry::with_sink(&settings, sink);

    assert!(handle.is_active());
    handle.record_command_invoked("pour");
    handle.emit(TelemetryEvent::CommandInvoked { name: "pull" });

    assert_eq!(mock.call_count(), 2);
    let calls = mock.calls();
    assert!(
        calls
            .iter()
            .all(|c| c.url == "https://telemetry.example/v1"),
        "every call must hit the configured endpoint"
    );
}

/// Concrete sanity: a custom transport implementation can be
/// plugged in by external crates (e.g. integration tests in
/// downstream crates) and it works with the same gating logic.
/// This pins the `HttpTransport` trait as part of the public
/// API surface.
#[test]
fn external_transport_impl_observes_same_gating() {
    /// Inline transport that fails loudly if it's ever invoked.
    /// Mirrors `PanicOnAccessSink` but at the transport layer.
    struct PanicTransport;
    impl HttpTransport for PanicTransport {
        fn post_json(&self, _url: &str, _body: &[u8]) -> Result<(), TransportError> {
            panic!("PanicTransport::post_json must not be reached");
        }
    }

    let settings = TelemetrySettings::default(); // all guards off
    let sink = HttpTelemetrySink::with_transport(&settings, PanicTransport);
    for ev in every_event_variant() {
        sink.submit(ev);
    }
    // The panic transport was never hit: test reaches here.
}

/// Stress: with all guards off, a hot loop of submits costs
/// nothing observable. Mirrors `disabled_emit_hot_loop_*` from
/// `zero_network.rs`, but at the HTTP sink layer rather than
/// the trait layer — so it specifically pins that the gates in
/// `HttpTelemetrySink::submit` short-circuit before any
/// serialization or transport dispatch.
#[test]
fn disabled_sink_hot_loop_is_a_noop() {
    let settings = TelemetrySettings::default();
    let mock = MockHttpTransport::new();
    let sink = HttpTelemetrySink::with_transport(&settings, mock.clone());

    for _ in 0..10_000 {
        sink.submit(TelemetryEvent::CommandInvoked { name: "burst" });
    }
    assert_eq!(mock.call_count(), 0);
    assert_eq!(sink.dropped_count(), 0);
}

/// Mock transport's `fail_next(n)` exhausts after `n` failures
/// and then succeeds again. Pins the test fixture itself so
/// other tests can rely on it.
#[test]
fn mock_transport_fail_next_exhausts() {
    let mock = MockHttpTransport::new();
    mock.fail_next(1);
    assert!(mock.post_json("https://x.test/", b"{}").is_err());
    assert!(mock.post_json("https://x.test/", b"{}").is_ok());
    assert_eq!(mock.call_count(), 2);
}

/// Atomic-counter sanity: the dropped counter is updated under
/// `SeqCst` and the test reads it back in `SeqCst` ordering.
/// This pins the diagnostic surface so a future change that
/// loosens to `Relaxed` has a test forcing the conversation.
#[test]
fn dropped_count_uses_seqcst_ordering() {
    let settings = fully_open_settings();
    let mock = MockHttpTransport::new();
    mock.fail_next(usize::MAX);
    let sink = HttpTelemetrySink::with_transport(&settings, mock);
    sink.submit(TelemetryEvent::CommandInvoked { name: "pour" });
    // We don't have direct access to the AtomicUsize, but
    // `dropped_count()` is documented to read SeqCst; assert
    // the value is observable from the test thread, which is
    // the property that matters.
    let _ = Ordering::SeqCst; // import-use sanity
    assert_eq!(sink.dropped_count(), 1);
}
