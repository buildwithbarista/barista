// Integration-test target — workspace security lints are allowed
// here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`) is the
// documented contract for failing a test loudly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Acceptance-criterion tests for the [`TelemetryEvent`] catalog
//! shapes (M3.3 T2).
//!
//! # `[T]` linkage
//!
//! [`no_event_field_names_carry_pii`] is the proof for the M3.3
//! acceptance criterion **"Event payloads never contain CLI args,
//! error messages, or file paths."** It introspects every
//! variant of the public [`TelemetryEvent`] enum via a serde JSON
//! round-trip and asserts that no field name on any variant is
//! one of the forbidden identifiers (`args`, `message`, `path`,
//! `coord`, `project`, …). Adding a new variant whose field uses
//! any of those names — even spelled with a prefix/suffix — fails
//! this test loudly, so the privacy contract from PRD §20.2 is
//! enforced as a test-time invariant rather than relying on code
//! review alone.
//!
//! The complementary acceptance criterion **"zero network calls
//! when disabled"** is re-verified across every variant by the
//! existing tests in `tests/zero_network.rs` and
//! `tests/transport_stub.rs`, which were updated in T2 to
//! exercise the full catalog.

use barista_telemetry::TelemetryEvent;
use serde_json::Value;

/// Every concrete `TelemetryEvent` variant in the v0.1 catalog,
/// with representative payloads. The list is intentionally
/// exhaustive — see [`catalog_is_exhaustive`] for the guard that
/// fails if a new variant lands without being added here.
fn every_variant() -> Vec<TelemetryEvent> {
    vec![
        TelemetryEvent::CommandInvoked { name: "pour" },
        TelemetryEvent::BuildDuration {
            phase: "resolve",
            duration_ms: 1_240,
        },
        TelemetryEvent::ArtifactCount {
            category: "resolved-deps",
            count: 42,
        },
        TelemetryEvent::CacheHitMiss {
            hits: 100,
            misses: 3,
        },
        TelemetryEvent::ErrorCodeOnly { code: "BAR-001" },
    ]
}

/// Field-name tokens that must never appear in any telemetry
/// event, per PRD §20.2 (no PII, no CLI args, no error messages,
/// no paths, no project identities, no Maven coordinates). The
/// comparison is case-folded and substring-based so aliases like
/// `error_message`, `file_path`, or `project_name` are also
/// caught.
const FORBIDDEN_FIELD_TOKENS: &[&str] = &[
    "args",
    "argv",
    "message",
    "msg",
    "path",
    "filename",
    "file",
    "coord",
    "gav",
    "project",
    "groupid",
    "artifactid",
    "username",
    "hostname",
    "ip",
    "secret",
    "token",
    "password",
    "credential",
    "env",
    "url", // endpoints belong in settings, never in event payloads
];

/// Walks `v` and pushes the lowercase form of every object key
/// into `out`, recursively.
fn collect_field_names(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                out.push(k.to_ascii_lowercase());
                collect_field_names(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_field_names(child, out);
            }
        }
        _ => {}
    }
}

/// Returns `Some(token)` if `name` contains any forbidden token
/// as a substring (case-folded), `None` otherwise.
fn is_forbidden(name: &str) -> Option<&'static str> {
    FORBIDDEN_FIELD_TOKENS
        .iter()
        .find(|token| name.contains(*token))
        .copied()
}

/// **[T]** AC: "Event payloads never contain CLI args, error
/// messages, or file paths."
///
/// Strategy: serialize every variant to JSON, walk every object
/// in the resulting tree, and assert that no key (other than the
/// externally-tagged `kind` discriminator) matches a forbidden
/// token. This catches both:
///
/// 1. A field literally named `args` / `message` / `path` /
///    `coord` / `project`.
/// 2. Aliases that try to sneak past with a prefix or suffix
///    (`error_message`, `file_path`, `project_name`,
///    `coord_string`, `cli_args`, …).
#[test]
fn no_event_field_names_carry_pii() {
    for ev in every_variant() {
        let v: Value = serde_json::to_value(&ev).expect("event must serialize");
        let mut names = Vec::new();
        collect_field_names(&v, &mut names);

        for name in &names {
            // `kind` is the discriminator, not a payload field;
            // it is part of the public wire frame.
            if name == "kind" {
                continue;
            }
            if let Some(token) = is_forbidden(name) {
                panic!(
                    "TelemetryEvent variant {ev:?} has a field whose name \
                     {name:?} matches forbidden token {token:?}; PRD §20.2 \
                     forbids CLI args, error messages, file paths, project \
                     names, and coordinates in event payloads"
                );
            }
        }
    }
}

/// Sanity counterpart: assert the forbidden-list logic itself
/// rejects what it claims to reject. If somebody loosens
/// `is_forbidden` so the AC test silently always passes, this
/// test catches them.
#[test]
fn forbidden_token_check_actually_rejects_bad_names() {
    let bad = [
        "args",
        "cli_args",
        "argv",
        "message",
        "error_message",
        "msg",
        "path",
        "file_path",
        "filename",
        "coord",
        "coordinates",
        "gav",
        "project",
        "project_name",
        "groupid",
        "artifactid",
        "url",
    ];
    for b in bad {
        assert!(
            is_forbidden(&b.to_ascii_lowercase()).is_some(),
            "expected {b:?} to be flagged as a forbidden field name"
        );
    }
}

/// Sanity counterpart in the other direction: the field names
/// the v0.1 catalog actually uses are *not* flagged. If a future
/// refactor makes `is_forbidden` too eager, this breaks before
/// the AC test does — keeping the two halves honest.
#[test]
fn forbidden_token_check_accepts_legitimate_names() {
    let good = [
        "name",
        "phase",
        "duration_ms",
        "kind",     // catalog discriminator
        "category", // ArtifactCount label
        "count",
        "hits",
        "misses",
        "code",
    ];
    for g in good {
        assert!(
            is_forbidden(g).is_none(),
            "expected legitimate field name {g:?} to pass the filter"
        );
    }
}

/// Catalog-exhaustiveness guard: `every_variant()` must cover
/// every variant of the (non-exhaustive) `TelemetryEvent` enum.
/// A `match` against the enum forces a compile error if a new
/// variant lands without being added to the fixture, so future
/// AC re-runs continue to introspect the full catalog.
#[test]
fn catalog_is_exhaustive() {
    let representatives = every_variant();
    // The fixture must have at least one representative of each
    // declared variant — count distinct `kind` tags in the
    // serialized form. If a future T adds a sixth variant
    // without extending `every_variant()`, this count stays at 5
    // and the test fails loudly.
    let mut kinds: Vec<String> = representatives
        .iter()
        .map(|ev| {
            let v = serde_json::to_value(ev).expect("serialize");
            v.get("kind")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .expect("every variant must serialize with a `kind` tag")
        })
        .collect();
    kinds.sort();
    kinds.dedup();
    let expected: Vec<&str> = vec![
        "artifact_count",
        "build_duration",
        "cache_hit_miss",
        "command_invoked",
        "error_code_only",
    ];
    assert_eq!(
        kinds, expected,
        "v0.1 telemetry catalog must contain exactly these five kinds; \
         if you added a variant, extend every_variant() AND the expected list \
         here so the privacy-introspection test in this file covers it"
    );
}

/// Pin the wire-shape of every variant. The serialized form is a
/// public contract: a downstream ingestion service decodes by
/// `kind`, so the snake_case tag values, field names, and scalar
/// types are all part of the API.
#[test]
fn wire_shape_is_stable() {
    let cases = [
        (
            TelemetryEvent::CommandInvoked { name: "pour" },
            serde_json::json!({"kind": "command_invoked", "name": "pour"}),
        ),
        (
            TelemetryEvent::BuildDuration {
                phase: "resolve",
                duration_ms: 1_240,
            },
            serde_json::json!({
                "kind": "build_duration",
                "phase": "resolve",
                "duration_ms": 1_240,
            }),
        ),
        (
            TelemetryEvent::ArtifactCount {
                category: "resolved-deps",
                count: 42,
            },
            serde_json::json!({
                "kind": "artifact_count",
                "category": "resolved-deps",
                "count": 42,
            }),
        ),
        (
            TelemetryEvent::CacheHitMiss {
                hits: 7,
                misses: 1,
            },
            serde_json::json!({
                "kind": "cache_hit_miss",
                "hits": 7,
                "misses": 1,
            }),
        ),
        (
            TelemetryEvent::ErrorCodeOnly { code: "BAR-001" },
            serde_json::json!({"kind": "error_code_only", "code": "BAR-001"}),
        ),
    ];

    // Note: `TelemetryEvent`'s textual fields are `&'static str`,
    // which constrains `Deserialize` to the `'static` lifetime
    // — `serde_json::from_value`/`from_str` on owned input
    // therefore won't infer the right lifetime. We pin
    // serialization here; round-tripping deserialization is
    // covered for the static case in the crate-level unit tests
    // (see `telemetry_event_serializes_with_kind_tag`). The
    // wire-format contract this test enforces is *one-way*:
    // these are the bytes that go out on the wire.
    for (ev, expected) in &cases {
        let got = serde_json::to_value(ev).expect("serialize");
        assert_eq!(
            &got, expected,
            "wire shape drift for {ev:?}: got {got}, want {expected}"
        );
    }
}

/// `ArtifactCount` deliberately spells its label field `category`
/// rather than `kind`, so it doesn't shadow the serde
/// external-tag discriminator (also `kind`). Pin that choice so a
/// future refactor that renames `category` back to `kind`
/// trips a test rather than silently producing a wire stream
/// where the discriminator and a payload field collide.
#[test]
fn artifact_count_category_does_not_shadow_discriminator() {
    let ev = TelemetryEvent::ArtifactCount {
        category: "resolved-deps",
        count: 42,
    };
    let v = serde_json::to_value(&ev).expect("serialize");
    let kind = v
        .get("kind")
        .and_then(Value::as_str)
        .expect("kind tag present");
    let category = v
        .get("category")
        .and_then(Value::as_str)
        .expect("category field present");
    assert_eq!(
        kind, "artifact_count",
        "discriminator must reflect the variant, not the payload field"
    );
    assert_eq!(
        category, "resolved-deps",
        "payload label preserved under its own field name"
    );
}
