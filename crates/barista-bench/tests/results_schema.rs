// Integration tests for the `results.json` schema. Validates fixtures
// against the JSON-Schema using the `jsonschema` crate; also exercises
// the Rust `ResultsDocument` type's serde round-trip so the two halves
// of the contract stay in lock-step.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::path::{Path, PathBuf};

use barista_bench::{
    IterationMeasurement, RESULTS_SCHEMA, ResultsDocument, RunHardware, Summary,
    manifest::HardwareTier, write_results,
};
use jsonschema::Validator;
use serde_json::Value;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn schema_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("schema")
        .join("results.schema.json")
}

fn load_validator() -> Validator {
    let raw = std::fs::read_to_string(schema_path()).expect("read results.schema.json");
    let schema: Value = serde_json::from_str(&raw).expect("parse results.schema.json");
    jsonschema::draft202012::new(&schema).expect("compile results.schema.json")
}

fn load_json(name: &str) -> Value {
    let raw = std::fs::read_to_string(fixture(name)).unwrap_or_else(|e| panic!("read {name}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

#[test]
fn valid_results_passes_schema() {
    let validator = load_validator();
    let doc = load_json("results-valid.json");
    if let Err(error) = validator.validate(&doc) {
        panic!("expected valid document; got: {error}");
    }
}

#[test]
fn valid_results_deserializes_into_rust_type() {
    let raw = std::fs::read_to_string(fixture("results-valid.json")).expect("read");
    let doc: ResultsDocument = serde_json::from_str(&raw).expect("deserialize");
    assert_eq!(doc.schema, RESULTS_SCHEMA);
    assert_eq!(doc.manifest_id, "P02");
    assert_eq!(doc.hardware_tier, HardwareTier::Tier3);
    assert_eq!(doc.iterations.len(), 5);
    assert_eq!(doc.iterations[0].wall_ms, 8420);
    assert_eq!(doc.iterations[0].cpu_user_ms, Some(31200));
    assert!((doc.summary.median_wall_ms - 8463.0).abs() < f64::EPSILON);
    assert_eq!(doc.metadata.get("jdk").map(String::as_str), Some("21"));
}

#[test]
fn rust_emitted_results_validate_against_schema() {
    // Construct a `ResultsDocument` from typed values, serialize via
    // `serde_json`, then validate against the JSON-Schema. This is the
    // tightest version of "wire format and Rust type agree".
    let doc = ResultsDocument {
        schema: RESULTS_SCHEMA.to_string(),
        manifest_id: "P01".to_string(),
        baseline_id: None,
        resolved_command: None,
        run_id: "2026-05-10T18:00:00Z-deadbeef".to_string(),
        timestamp: "2026-05-10T18:00:00Z".to_string(),
        git_sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
        barista_version: "0.1.0".to_string(),
        hardware_tier: HardwareTier::Tier1,
        runner_id: "local-dev".to_string(),
        hardware: RunHardware {
            id: "local-dev".to_string(),
            cpu: "Apple M2 Pro".to_string(),
            cores_physical: 10,
            cores_logical: 10,
            memory_gb: 32,
            os: "macOS 14.5".to_string(),
        },
        iterations: vec![IterationMeasurement {
            iteration: 0,
            wall_ms: 42,
            cpu_user_ms: None,
            cpu_sys_ms: None,
            peak_rss_kb: None,
            network_bytes: None,
            disk_read_bytes: None,
            disk_write_bytes: None,
            exit_code: 0,
        }],
        summary: Summary {
            avg_wall_ms: 42.0,
            median_wall_ms: 42.0,
            p95_wall_ms: 42.0,
            stddev_wall_ms: 0.0,
        },
        metadata: Default::default(),
    };
    let value = serde_json::to_value(&doc).expect("serialize");
    let validator = load_validator();
    if let Err(error) = validator.validate(&value) {
        panic!("Rust-emitted document failed schema validation: {error}\nvalue: {value:#}");
    }
}

#[test]
fn rejects_malformed_timestamp() {
    let validator = load_validator();
    let doc = load_json("results-bad-timestamp.json");
    let err = validator
        .validate(&doc)
        .expect_err("schema must reject non-RFC3339 timestamp");
    let msg = err.to_string();
    // The validator reports the regex / format mismatch on the bad value.
    // Look for the pattern literal or the offending value substring.
    assert!(
        msg.contains("yesterday")
            || msg.contains("match")
            || msg.contains("pattern")
            || msg.contains("format"),
        "diagnostic should mention the timestamp constraint: {msg}"
    );
}

#[test]
fn rejects_missing_required_field() {
    let validator = load_validator();
    let doc = load_json("results-missing-summary.json");
    let err = validator
        .validate(&doc)
        .expect_err("schema must reject document without `summary`");
    let msg = err.to_string();
    assert!(
        msg.contains("summary") || msg.contains("required"),
        "diagnostic should mention the missing field: {msg}"
    );
}

#[test]
fn rejects_unknown_hardware_tier() {
    let validator = load_validator();
    let doc = load_json("results-bad-tier.json");
    let err = validator
        .validate(&doc)
        .expect_err("schema must reject hardware_tier outside 1..=3");
    let msg = err.to_string();
    // jsonschema reports `7 is not one of 1, 2 or 3` on the offending
    // value; assert on the enumerated values rather than the field name.
    assert!(
        msg.contains("hardware_tier")
            || msg.contains("1, 2")
            || msg.contains("not one of")
            || msg.contains("enum"),
        "diagnostic should mention the hardware_tier enum constraint: {msg}"
    );
}

#[test]
fn rejects_unknown_top_level_field() {
    // additionalProperties: false on the top-level object must trip
    // unknown keys so the contract stays closed.
    let validator = load_validator();
    let mut doc = load_json("results-valid.json");
    doc.as_object_mut()
        .unwrap()
        .insert("totally_unknown".to_string(), Value::String("x".into()));
    let err = validator.validate(&doc).expect_err("unknown key must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("totally_unknown") || msg.contains("additional"),
        "diagnostic should mention the unknown field: {msg}"
    );
}

#[test]
fn write_results_round_trips_through_disk() {
    use tempfile::tempdir;

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("results.json");

    let doc = ResultsDocument {
        schema: RESULTS_SCHEMA.to_string(),
        manifest_id: "P01".to_string(),
        baseline_id: None,
        resolved_command: None,
        run_id: "2026-05-10T18:00:00Z-deadbeef".to_string(),
        timestamp: "2026-05-10T18:00:00Z".to_string(),
        git_sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
        barista_version: "0.1.0".to_string(),
        hardware_tier: HardwareTier::Tier1,
        runner_id: "local-dev".to_string(),
        hardware: RunHardware {
            id: "local-dev".to_string(),
            cpu: "Apple M2 Pro".to_string(),
            cores_physical: 10,
            cores_logical: 10,
            memory_gb: 32,
            os: "macOS 14.5".to_string(),
        },
        iterations: vec![IterationMeasurement {
            iteration: 0,
            wall_ms: 42,
            cpu_user_ms: None,
            cpu_sys_ms: None,
            peak_rss_kb: None,
            network_bytes: None,
            disk_read_bytes: None,
            disk_write_bytes: None,
            exit_code: 0,
        }],
        summary: Summary {
            avg_wall_ms: 42.0,
            median_wall_ms: 42.0,
            p95_wall_ms: 42.0,
            stddev_wall_ms: 0.0,
        },
        metadata: Default::default(),
    };

    write_results(&path, &doc).expect("write");

    let raw = std::fs::read_to_string(&path).expect("read back");
    assert!(raw.ends_with('\n'), "written file must end with a newline");

    let value: Value = serde_json::from_str(&raw).expect("parse back");
    let validator = load_validator();
    if let Err(error) = validator.validate(&value) {
        panic!("written file failed schema validation: {error}");
    }

    let reparsed: ResultsDocument = serde_json::from_str(&raw).expect("deserialize back");
    assert_eq!(reparsed, doc);
}
