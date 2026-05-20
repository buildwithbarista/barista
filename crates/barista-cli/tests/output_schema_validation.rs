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

//! Validation tests for the published JSON output schemas in
//! `schema/output/v1/`.
//!
//! These tests load each schema from disk, then validate a
//! hand-authored representative document against it. They serve
//! two purposes:
//!
//! 1. Catch accidental drift between the schema files and what
//!    the renderer is documented to emit.
//! 2. Exercise the `jsonschema` crate against every schema so
//!    that publishing-time consumers (CI bots, downstream tools)
//!    can rely on the schemas parsing under a real Draft 2020-12
//!    validator.
//!
//! Schema files live at the monorepo root under `schema/output/v1/`;
//! we locate them via `CARGO_MANIFEST_DIR` which points at
//! `crates/barista-cli/`.

use std::path::{Path, PathBuf};

use jsonschema::Validator;
use serde_json::{Value, json};

/// Resolve a schema file relative to the workspace root.
fn schema_path(name: &str) -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .join("..")
        .join("..")
        .join("schema")
        .join("output")
        .join("v1")
        .join(name)
}

/// Load and compile a schema by file name.
fn load(name: &str) -> Validator {
    let path = schema_path(name);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()));
    let schema: Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("parse {} as JSON failed: {e}", path.display()));
    jsonschema::draft202012::new(&schema)
        .unwrap_or_else(|e| panic!("compile {} as draft2020-12 failed: {e}", path.display()))
}

/// Assert that `doc` validates against `validator`; on failure, dump
/// every error so the test output is actionable.
#[track_caller]
fn assert_valid(validator: &Validator, doc: &Value) {
    if let Err(error) = validator.validate(doc) {
        panic!("expected document to be valid, but got error: {error}");
    }
}

/// Assert that `doc` is rejected by `validator`.
#[track_caller]
fn assert_invalid(validator: &Validator, doc: &Value) {
    if validator.is_valid(doc) {
        panic!(
            "expected document to be rejected, but it validated: {}",
            serde_json::to_string(doc).unwrap()
        );
    }
}

#[test]
fn pull_happy_path_validates() {
    let validator = load("pull.json");
    let doc = json!({
        "command": "pull",
        "project-root": "/Users/dev/projects/example",
        "lockfile-status": "written",
        "entries": 142,
        "fetched": 17,
        "no-fetch": false,
        "strict": false,
        "warnings": [
            "snapshot artifact com.example:foo:1.0-SNAPSHOT refetched"
        ]
    });
    assert_valid(&validator, &doc);

    // The minimal form (no optional `warnings`) also validates.
    let minimal = json!({
        "command": "pull",
        "project-root": "/tmp/proj",
        "lockfile-status": "unchanged",
        "entries": 0,
        "fetched": 0,
        "no-fetch": true,
        "strict": true,
    });
    assert_valid(&validator, &minimal);
}

#[test]
fn grind_tree_happy_path_validates() {
    let validator = load("grind-tree.json");
    let doc = json!({
        "command": "grind-tree",
        "schema-version": 1,
        "reactor": [
            {
                "coords": "com.example:app",
                "version": "1.0.0",
                "relative-path": ""
            }
        ],
        "nodes": [
            {
                "coords": "org.slf4j:slf4j-api",
                "version": "2.0.13",
                "scope": "compile",
                "depth": 1,
                "from-path": []
            },
            {
                "coords": "ch.qos.logback:logback-classic",
                "version": "1.5.6",
                "scope": "compile",
                "depth": 2,
                "from-path": ["org.slf4j:slf4j-api"]
            }
        ]
    });
    assert_valid(&validator, &doc);
}

#[test]
fn verify_stub_validates() {
    let validator = load("verify.json");

    let stubbed = json!({
        "command": "verify",
        "status": "not-yet-implemented",
        "details": [],
    });
    assert_valid(&validator, &stubbed);

    // Once implemented, an `ok` document with details still validates.
    let future_ok = json!({
        "command": "verify",
        "status": "ok",
        "details": [
            {"check": "checksum", "passed": true},
            "informational text is also allowed in v1 stub"
        ],
    });
    assert_valid(&validator, &future_ok);
}

#[test]
fn progress_event_happy_path_validates_every_variant() {
    let validator = load("progress-event.json");

    let events = [
        json!({"event": "started", "timestamp": "2026-05-14T12:00:00.000Z"}),
        json!({
            "event": "resolving",
            "timestamp": "2026-05-14T12:00:00.123Z",
            "phase": "resolve",
            "progress": 42
        }),
        json!({
            "event": "fetching",
            "timestamp": "2026-05-14T12:00:01.456Z",
            "phase": "fetch",
            "coord": "org.slf4j:slf4j-api:2.0.13",
            "progress": 0
        }),
        json!({
            "event": "fetched",
            "timestamp": "2026-05-14T12:00:01.789Z",
            "phase": "fetch",
            "coord": "org.slf4j:slf4j-api:2.0.13",
            "progress": 100
        }),
        json!({
            "event": "cached",
            "timestamp": "2026-05-14T12:00:01.790Z",
            "phase": "fetch",
            "coord": "com.example:already-cached:1.0.0"
        }),
        json!({
            "event": "writing-lockfile",
            "timestamp": "2026-05-14T12:00:02.000Z",
            "phase": "lock-write"
        }),
        json!({"event": "completed", "timestamp": "2026-05-14T12:00:02.100Z"}),
        json!({
            "event": "error",
            "timestamp": "2026-05-14T12:00:02.100Z",
            "payload": {"message": "fetch failed for org.example:nope:1.0.0"}
        }),
        json!({
            "event": "result",
            "timestamp": "2026-05-14T12:00:02.101Z",
            "payload": {
                "command": "pull",
                "project-root": "/tmp/proj",
                "lockfile-status": "unchanged",
                "entries": 0,
                "fetched": 0,
                "no-fetch": false,
                "strict": false
            }
        }),
        // Timezone-offset timestamps are also valid RFC 3339.
        json!({
            "event": "started",
            "timestamp": "2026-05-14T08:00:00.000-04:00"
        }),
    ];

    for ev in &events {
        assert_valid(&validator, ev);
    }
}

#[test]
fn pull_rejects_unknown_keys_and_wrong_enum() {
    let validator = load("pull.json");

    // `additionalProperties: false` should reject unknown keys.
    let unknown_key = json!({
        "command": "pull",
        "project-root": "/tmp/proj",
        "lockfile-status": "unchanged",
        "entries": 0,
        "fetched": 0,
        "no-fetch": true,
        "strict": false,
        "surprise": "boo"
    });
    assert_invalid(&validator, &unknown_key);

    // Wrong `command` discriminator.
    let wrong_command = json!({
        "command": "verify",
        "project-root": "/tmp/proj",
        "lockfile-status": "unchanged",
        "entries": 0,
        "fetched": 0,
        "no-fetch": true,
        "strict": false,
    });
    assert_invalid(&validator, &wrong_command);

    // Lockfile-status outside the enum.
    let bad_enum = json!({
        "command": "pull",
        "project-root": "/tmp/proj",
        "lockfile-status": "borked",
        "entries": 0,
        "fetched": 0,
        "no-fetch": true,
        "strict": false,
    });
    assert_invalid(&validator, &bad_enum);

    // Missing required field.
    let missing = json!({
        "command": "pull",
        "project-root": "/tmp/proj",
        "lockfile-status": "unchanged",
        // entries omitted
        "fetched": 0,
        "no-fetch": true,
        "strict": false,
    });
    assert_invalid(&validator, &missing);
}

#[test]
fn progress_event_rejects_missing_required_fields_per_variant() {
    let validator = load("progress-event.json");

    // `fetching` without `coord` should be rejected.
    let fetching_no_coord = json!({
        "event": "fetching",
        "timestamp": "2026-05-14T12:00:01.456Z",
        "phase": "fetch"
    });
    assert_invalid(&validator, &fetching_no_coord);

    // `result` without `payload` should be rejected.
    let result_no_payload = json!({
        "event": "result",
        "timestamp": "2026-05-14T12:00:02.101Z"
    });
    assert_invalid(&validator, &result_no_payload);

    // Unknown event name should be rejected.
    let unknown_event = json!({
        "event": "exploded",
        "timestamp": "2026-05-14T12:00:02.101Z"
    });
    assert_invalid(&validator, &unknown_event);

    // Progress out of range should be rejected.
    let bad_progress = json!({
        "event": "fetching",
        "timestamp": "2026-05-14T12:00:01.456Z",
        "coord": "com.example:foo:1.0",
        "progress": 250
    });
    assert_invalid(&validator, &bad_progress);

    // Missing the millisecond fragment should be rejected (pattern enforces it).
    let coarse_timestamp = json!({
        "event": "started",
        "timestamp": "2026-05-14T12:00:00Z"
    });
    assert_invalid(&validator, &coarse_timestamp);
}
