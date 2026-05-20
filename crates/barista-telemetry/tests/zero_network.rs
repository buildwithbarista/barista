// SPDX-License-Identifier: MIT OR Apache-2.0

// Integration-test / example / benchmark target — workspace security
// lints are allowed here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`)
// is the documented contract for failing a test loudly. This allow block
// keeps the crate root's `#![allow(...)]` from being silently dropped by
// the separate compilation unit each test file forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Acceptance-criterion tests for the opt-in telemetry flag.
//!
//! The central correctness property is: **with telemetry
//! disabled (the default), zero network calls are observed.**
//! These tests pin that property at every layer:
//!
//! 1. The default `TelemetrySettings` has `enabled = false`.
//! 2. A handle built from disabled settings reports
//!    `is_active() == false`.
//! 3. Emit methods on a disabled handle never reach the sink
//!    (asserted with `PanicOnAccessSink`, which panics on
//!    access).
//! 4. The same property holds when the env-var override
//!    `BARISTA_TELEMETRY__ENABLED=0` is folded through
//!    `barista-config`'s layered loader.
//! 5. With `enabled = true` and a `NullSink` (the "no transport
//!    configured" case) emission is a no-op — events are
//!    consumed without any I/O.
//! 6. Round-tripping through `barista-config`'s TOML schema
//!    preserves the three-field surface.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use barista_config::sources::EnvGetter;
use barista_config::{Config, LoaderInputs, PartialConfig, load_effective_config};
use barista_telemetry::{
    PanicOnAccessSink, Telemetry, TelemetryEvent, TelemetrySettings, TelemetrySink,
};

// ============================================================
// Helpers
// ============================================================

/// Project `barista_config::TelemetryConfig` → `TelemetrySettings`.
/// Kept local rather than added to the public API so the
/// dependency edge stays one-way (telemetry → no config crate at
/// runtime; the consumer translates).
fn settings_from_config(cfg: &Config) -> TelemetrySettings {
    TelemetrySettings {
        enabled: cfg.telemetry.enabled,
        endpoint: cfg.telemetry.endpoint.clone(),
        client_id: cfg.telemetry.client_id.clone(),
        transport_enabled: cfg.telemetry.transport_enabled,
    }
}

fn loader_inputs_with_env<'a>(env: &'a HashMap<String, String>) -> LoaderInputs<'a> {
    let getter: Box<EnvGetter<'a>> = Box::new(move |k| env.get(k).cloned());
    // Leak so the lifetime matches the &'a EnvGetter the loader wants.
    let leaked: &'a EnvGetter<'a> = Box::leak(getter);
    LoaderInputs {
        env_get: Some(leaked),
        home_override: Some(std::env::temp_dir().join("barista-telemetry-test-home")),
        cwd_override: Some(std::env::temp_dir().join("barista-telemetry-test-cwd")),
        ..Default::default()
    }
}

/// Counting sink — records submit calls without doing I/O. Used
/// to assert that the active path actually calls through.
#[derive(Debug, Default)]
struct CountingSink {
    count: Arc<AtomicUsize>,
}

impl CountingSink {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        (
            Self {
                count: Arc::clone(&count),
            },
            count,
        )
    }
}

impl TelemetrySink for CountingSink {
    fn submit(&self, _event: TelemetryEvent) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }
}

// ============================================================
// Tests
// ============================================================

/// [T] linkage for M3.3 acceptance criterion
/// "With telemetry disabled (default), zero network calls
/// observed in test."
///
/// Builds a default `Config` through `barista-config`, projects
/// the telemetry slice, constructs a `Telemetry` handle backed
/// by a `PanicOnAccessSink`, then calls every public emit
/// method. The sink panics if reached; the test passes iff it
/// is never reached.
#[test]
fn disabled_default_panic_sink_never_fires() {
    let env: HashMap<String, String> = HashMap::new();
    let inputs = loader_inputs_with_env(&env);
    let (cfg, _audit) = load_effective_config(inputs).expect("default load");
    assert!(!cfg.telemetry.enabled, "default telemetry must be off");

    let settings = settings_from_config(&cfg);
    let t = Telemetry::with_sink(&settings, PanicOnAccessSink);

    assert!(!t.is_active());

    // Every public emit method. If any reaches the sink, the
    // test panics.
    t.emit(TelemetryEvent::CommandInvoked { name: "pour" });
    t.record_command_invoked("pull");
    t.record_command_invoked("grind");

    // And a high-volume call to make the no-op property
    // visible: 10_000 emits, none observable.
    for _ in 0..10_000 {
        t.record_command_invoked("burst");
    }
}

/// Env-var override path: `BARISTA_TELEMETRY__ENABLED=0`
/// explicitly off, same panic-sink guarantee.
#[test]
fn disabled_with_env_override_off_panic_sink_never_fires() {
    let mut env = HashMap::new();
    env.insert("BARISTA_TELEMETRY__ENABLED".into(), "0".into());

    let inputs = loader_inputs_with_env(&env);
    let (cfg, _audit) = load_effective_config(inputs).expect("load with env");
    assert!(!cfg.telemetry.enabled);

    let settings = settings_from_config(&cfg);
    let t = Telemetry::with_sink(&settings, PanicOnAccessSink);
    assert!(!t.is_active());

    t.emit(TelemetryEvent::CommandInvoked { name: "pour" });
    t.record_command_invoked("pour");
}

/// Env-var override flips telemetry on. Sink is a `NullSink`
/// (the "no transport configured" case): emits succeed, do no
/// I/O, and consume the event.
#[test]
fn enabled_with_null_sink_is_inert() {
    let mut env = HashMap::new();
    env.insert("BARISTA_TELEMETRY__ENABLED".into(), "1".into());

    let inputs = loader_inputs_with_env(&env);
    let (cfg, _audit) = load_effective_config(inputs).expect("load with env");
    assert!(cfg.telemetry.enabled, "env override should enable");
    assert!(
        cfg.telemetry.endpoint.is_none(),
        "endpoint not set ⇒ NullSink path"
    );

    let settings = settings_from_config(&cfg);
    let t = Telemetry::from_settings(&settings);
    assert!(t.is_active());

    // Drive the API. NullSink drops these silently; we cannot
    // observe network traffic in unit tests but the crate has
    // no HTTP dependency, so by-construction there is none.
    for _ in 0..1_000 {
        t.record_command_invoked("pour");
    }
}

/// Enabled handle wired to a counting sink: every emit reaches
/// the sink, proving the active path actually calls through.
/// This is the contrapositive of the disabled-path test.
#[test]
fn enabled_path_reaches_sink() {
    let settings = TelemetrySettings {
        enabled: true,
        endpoint: Some("https://example.test/ingest".into()),
        client_id: None,
        transport_enabled: false,
    };
    let (sink, count) = CountingSink::new();
    let t = Telemetry::with_sink(&settings, sink);

    assert!(t.is_active());
    t.record_command_invoked("pour");
    t.emit(TelemetryEvent::CommandInvoked { name: "pull" });
    t.record_command_invoked("grind");

    assert_eq!(count.load(Ordering::SeqCst), 3);
}

/// Config round-trip: a project `barista.toml` with a
/// `[telemetry]` block flows through the loader and projects
/// faithfully into `TelemetrySettings`. Covers all three
/// fields (`enabled`, `endpoint`, `client-id`).
#[test]
fn config_roundtrip_through_barista_config() {
    // First: TOML directly into PartialConfig, to pin the
    // wire-shape (kebab-case `client-id`).
    let toml_src = r#"
[telemetry]
enabled = true
endpoint = "https://telemetry.example/v1"
client-id = "install-abc-123"
"#;
    let partial: PartialConfig = toml::from_str(toml_src).expect("parse telemetry section");
    let mut cfg = Config::default();
    let touched = partial.apply_to(&mut cfg);
    assert!(touched.iter().any(|t| t == "telemetry.enabled"));
    assert!(touched.iter().any(|t| t == "telemetry.endpoint"));
    assert!(touched.iter().any(|t| t == "telemetry.client-id"));

    let s = settings_from_config(&cfg);
    assert_eq!(
        s,
        TelemetrySettings {
            enabled: true,
            endpoint: Some("https://telemetry.example/v1".into()),
            client_id: Some("install-abc-123".into()),
            transport_enabled: false,
        }
    );

    // And: an unknown field under [telemetry] is rejected,
    // proving the deny-unknown-fields contract on the partial
    // type still holds for our new field.
    let bad = r#"[telemetry]
enabled = true
made-up-knob = "nope"
"#;
    let err = toml::from_str::<PartialConfig>(bad).unwrap_err();
    assert!(
        err.to_string().contains("made-up-knob") || err.to_string().contains("unknown field"),
        "expected unknown-field rejection; got: {err}"
    );
}

/// `BARISTA_TELEMETRY__CLIENT_ID` env override sets the
/// per-install ID without flipping `enabled`. The handle is
/// still inactive — env can populate `client_id`, but the
/// opt-in flag is a separate, explicit switch.
#[test]
fn env_var_client_id_does_not_flip_enabled() {
    let mut env = HashMap::new();
    env.insert(
        "BARISTA_TELEMETRY__CLIENT_ID".into(),
        "install-from-env".into(),
    );

    let inputs = loader_inputs_with_env(&env);
    let (cfg, _audit) = load_effective_config(inputs).expect("load with env");

    assert!(
        !cfg.telemetry.enabled,
        "setting client_id alone must NOT enable telemetry"
    );
    assert_eq!(cfg.telemetry.client_id.as_deref(), Some("install-from-env"));

    // And the handle is still inert — wire a panic sink to
    // prove it.
    let settings = settings_from_config(&cfg);
    let t = Telemetry::with_sink(&settings, PanicOnAccessSink);
    assert!(!t.is_active());
    t.record_command_invoked("pour");
}

/// Hot-path allocation/observability check: 100_000 emits on a
/// disabled handle complete in well under a second and reach no
/// sink. There's no portable in-process allocator counter we
/// can lean on without adding a dep, so this is a synthetic
/// "no observable side effect" check — its real value is
/// combined with the panic-sink test above: the panic test
/// proves no dispatch, this one proves there's no hidden cost
/// blowing up at scale.
#[test]
fn disabled_emit_hot_loop_has_no_observable_side_effect() {
    let t = Telemetry::with_sink(&TelemetrySettings::disabled(), PanicOnAccessSink);
    for _ in 0..100_000 {
        t.emit(TelemetryEvent::CommandInvoked { name: "burst" });
        t.record_command_invoked("burst");
    }
    assert!(!t.is_active());
}

/// Permanently-disabled constructor is equivalent to building
/// from default settings.
#[test]
fn telemetry_disabled_constructor_matches_default_settings() {
    let a = Telemetry::disabled();
    let b = Telemetry::from_settings(&TelemetrySettings::default());
    assert_eq!(a.is_active(), b.is_active());
    assert_eq!(a.settings(), b.settings());
}
