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
    assert_eq!(
        manifest.corpus_id.as_deref(),
        Some("spring-petclinic-3.3.0")
    );
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
    assert_eq!(
        manifest.labels.get("shape").map(String::as_str),
        Some("small-spring")
    );
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
    assert!(
        msg.contains("schema"),
        "diagnostic should mention schema: {msg}"
    );
}

#[test]
fn rejects_empty_metrics_list() {
    let err = load_manifest(fixture("manifest-empty-metrics.toml")).unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(err, Error::ManifestInvalid(_)),
        "expected ManifestInvalid, got: {msg}"
    );
    assert!(
        msg.contains("metrics"),
        "diagnostic should mention metrics: {msg}"
    );
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

// ---------------------------------------------------------------------------
// [[baselines]] — cross-tool baseline section (added 2026-05-18)
// ---------------------------------------------------------------------------

#[test]
fn legacy_no_baselines_section_derives_implicit_barista() {
    // A manifest without a [[baselines]] section continues to parse
    // (backward compat with the v0.1 single-command shape) and the
    // harness sees a single implicit baseline named `barista`.
    let raw = r#"
schema = "barista.bench.manifest/v1"
id = "P03"
display_name = "Spring Boot starter-web"
category = "corpus"
command = "barista verify"
metrics = ["wall_ms"]
hardware_tier = 2
"#;
    let m = Manifest::from_toml_str(raw).expect("legacy shape parses");
    assert!(m.baselines.is_empty());
    let eff = m.effective_baselines();
    assert_eq!(eff.len(), 1);
    assert_eq!(eff[0].id, "barista");
    assert_eq!(eff[0].command, "barista verify");
}

#[test]
fn baselines_section_parses_with_multiple_entries() {
    let raw = r#"
schema = "barista.bench.manifest/v1"
id = "P03-package-warm"
display_name = "Spring Boot starter-web — warm package"
category = "corpus"
command = "barista package -DskipTests"
metrics = ["wall_ms"]
hardware_tier = 2

[[baselines]]
id = "barista"
display_name = "barista (warm daemon)"
command = "barista package -DskipTests"
prepare = "rm -rf target"

[[baselines]]
id = "barista-no-daemon"
display_name = "barista (--no-daemon, forked mvn)"
command = "barista --no-daemon package -DskipTests"
prepare = "rm -rf target"

[[baselines]]
id = "mvn"
display_name = "Apache Maven 3.9.9"
command = "mvn -B -q package -DskipTests"
prepare = "rm -rf target"
"#;
    let m = Manifest::from_toml_str(raw).expect("parses");
    assert_eq!(m.baselines.len(), 3);
    let eff = m.effective_baselines();
    assert_eq!(eff.len(), 3);
    assert_eq!(eff[0].id, "barista");
    assert_eq!(eff[1].id, "barista-no-daemon");
    assert_eq!(eff[2].id, "mvn");
    assert_eq!(eff[0].prepare.as_deref(), Some("rm -rf target"));
}

#[test]
fn rejects_duplicate_baseline_ids() {
    let raw = r#"
schema = "barista.bench.manifest/v1"
id = "P03"
display_name = "P03"
category = "corpus"
command = "barista verify"
metrics = ["wall_ms"]
hardware_tier = 2

[[baselines]]
id = "barista"
display_name = "barista"
command = "barista verify"

[[baselines]]
id = "barista"
display_name = "second barista"
command = "barista compile"
"#;
    let err = Manifest::from_toml_str(raw).unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, Error::ManifestInvalid(_)), "got: {msg}");
    assert!(
        msg.contains("duplicate baseline id"),
        "diagnostic should mention duplicate baseline id: {msg}"
    );
}

#[test]
fn rejects_empty_baseline_id() {
    let raw = r#"
schema = "barista.bench.manifest/v1"
id = "P03"
display_name = "P03"
category = "corpus"
command = "barista verify"
metrics = ["wall_ms"]
hardware_tier = 2

[[baselines]]
id = ""
display_name = "barista"
command = "barista verify"
"#;
    let err = Manifest::from_toml_str(raw).unwrap_err();
    assert!(matches!(err, Error::ManifestInvalid(_)));
}
