// Integration tests for the `Bench.toml` manifest schema.
//
// Workspace security lints disable `unwrap`/`expect`/`panic` in
// production code; tests deliberately use them so a regression fails
// loudly with a useful diagnostic. Re-enable the allows for this
// translation unit.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::path::{Path, PathBuf};

use barista_bench::{
    Error, MANIFEST_SCHEMA, Manifest, Metric, load_manifest,
    manifest::{Category, HardwareTier, KnownMetric},
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn parses_full_corpus_manifest() {
    let manifest = load_manifest(fixture("manifest-valid.toml")).expect("parse");
    assert_eq!(manifest.schema, MANIFEST_SCHEMA);
    assert_eq!(manifest.id, "P02");
    assert_eq!(manifest.display_name, "Spring PetClinic");
    assert_eq!(manifest.category, Category::Corpus);
    assert_eq!(manifest.corpus_id.as_deref(), Some("spring-petclinic-3.3.0"));
    assert_eq!(manifest.hardware_tier, HardwareTier::Tier3);
    assert_eq!(manifest.iterations, 5);
    assert_eq!(manifest.warmup_iterations, 1);
    assert_eq!(manifest.metrics.len(), 3);
    assert!(matches!(
        manifest.metrics[0],
        Metric::Known(KnownMetric::WallMs)
    ));
    assert_eq!(
        manifest.allowed_variance.get("wall_ms_p95").copied(),
        Some(0.10)
    );
    assert_eq!(manifest.labels.get("shape").map(String::as_str), Some("small-spring"));
}

#[test]
fn parses_microbench_manifest_with_defaults() {
    let manifest = load_manifest(fixture("manifest-microbench.toml")).expect("parse");
    assert_eq!(manifest.category, Category::Microbench);
    assert_eq!(manifest.hardware_tier, HardwareTier::Tier1);
    // iterations defaults to 5 even when omitted
    assert_eq!(manifest.iterations, 5);
    assert_eq!(manifest.warmup_iterations, 1);
    assert!(manifest.corpus_id.is_none());
    assert!(manifest.allowed_variance.is_empty());
    assert!(manifest.labels.is_empty());
}

#[test]
fn roundtrips_byte_stable() {
    let manifest = load_manifest(fixture("manifest-valid.toml")).expect("parse");
    let serialized = manifest.to_toml_string().expect("serialize");
    // Re-parse to confirm semantic stability (whitespace / key ordering
    // are not byte-stable through `toml`, but the deserialized struct
    // must be).
    let reparsed = Manifest::from_toml_str(&serialized).expect("reparse");
    assert_eq!(manifest, reparsed);
}

#[test]
fn rejects_missing_id() {
    let err = load_manifest(fixture("manifest-missing-id.toml")).unwrap_err();
    // `serde(deny_unknown_fields)` + missing `id` produces a parse error
    // mentioning the missing field.
    let msg = err.to_string();
    assert!(
        matches!(err, Error::ManifestParse(_)),
        "expected ManifestParse, got: {msg}"
    );
    assert!(msg.contains("id"), "diagnostic should mention `id`: {msg}");
}

#[test]
fn rejects_bad_schema_string() {
    let err = load_manifest(fixture("manifest-bad-schema.toml")).unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(err, Error::ManifestInvalid(_)),
        "expected ManifestInvalid, got: {msg}"
    );
    assert!(msg.contains("schema"), "diagnostic should mention schema: {msg}");
}

#[test]
fn rejects_empty_metrics_list() {
    let err = load_manifest(fixture("manifest-empty-metrics.toml")).unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(err, Error::ManifestInvalid(_)),
        "expected ManifestInvalid, got: {msg}"
    );
    assert!(msg.contains("metrics"), "diagnostic should mention metrics: {msg}");
}

#[test]
fn rejects_unknown_top_level_field() {
    // `deny_unknown_fields` must trip on extra top-level keys to keep
    // the on-disk contract closed.
    let raw = r#"
schema = "barista.bench.manifest/v1"
id = "P02"
display_name = "Spring PetClinic"
category = "corpus"
command = "barista verify"
metrics = ["wall_ms"]
hardware_tier = 3
totally_unknown_field = "boo"
"#;
    let err = Manifest::from_toml_str(raw).unwrap_err();
    assert!(matches!(err, Error::ManifestParse(_)));
}
