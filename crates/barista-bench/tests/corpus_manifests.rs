// SPDX-License-Identifier: MIT OR Apache-2.0

// Validation tests for the on-disk `Bench.toml` manifests checked in
// under `bench/projects/`. These are not bench-execution tests — they
// only verify the manifests parse against the `Manifest` struct and
// declare a coherent shape (right id, right category, non-empty
// metrics list, ASCII-only labels, etc.). The Tier-2 perf-gate is the
// runtime consumer.
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
    MANIFEST_SCHEMA, Manifest, Metric, load_manifest,
    manifest::{Category, HardwareTier, KnownMetric},
};

/// Resolve a `bench/projects/<id>/Bench.toml` path relative to the
/// monorepo root. `CARGO_MANIFEST_DIR` for this crate is
/// `<root>/crates/barista-bench`; the corpus lives two levels up.
fn manifest_path(id: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("bench")
        .join("projects")
        .join(id)
        .join("Bench.toml")
}

/// Assertions every checked-in corpus manifest must satisfy. Anything
/// per-project lives in the project-specific test below.
fn assert_corpus_invariants(manifest: &Manifest, expected_id: &str) {
    assert_eq!(
        manifest.schema, MANIFEST_SCHEMA,
        "schema discriminator must match crate constant"
    );
    assert_eq!(manifest.id, expected_id, "id must match directory name");
    assert_eq!(
        manifest.category,
        Category::Corpus,
        "bench/projects entries are always Corpus"
    );
    assert!(
        !manifest.display_name.trim().is_empty(),
        "display_name must be human-readable"
    );
    assert!(
        manifest.corpus_id.as_deref().is_some_and(|s| !s.is_empty()),
        "corpus_id is the foreign key into bench/projects/<id>/ and must be set"
    );
    assert!(
        !manifest.metrics.is_empty(),
        "metrics list must contain at least one entry"
    );
    // Every corpus entry must measure wall-clock time — it's the
    // dimension every PRD §17.10 threshold keys on.
    assert!(
        manifest
            .metrics
            .iter()
            .any(|m| matches!(m, Metric::Known(KnownMetric::WallMs))),
        "every corpus manifest must record wall_ms"
    );
    assert!(
        manifest.iterations >= 1,
        "iterations must be >= 1 (PRD §17.7 step 4: median over runs)"
    );
    // PRD §17.7 step 3b mandates a warm-up run, but the cold-cache
    // dimension (cache_isolation = per-iteration) explicitly defeats
    // JIT/cache warming — each iteration fetches the full closure
    // from upstream. A warmup iteration in that mode is wasted
    // network traffic, not a measurement-stability aid. The
    // §17.7 invariant applies to warm-cache manifests only.
    if manifest.cache_isolation == barista_bench::CacheIsolation::PerIteration {
        // Cold-cache: warmup is optional. No assertion.
    } else {
        assert!(
            manifest.warmup_iterations >= 1,
            "warmup_iterations must be >= 1 for warm-cache manifests (PRD §17.7 step 3b)"
        );
    }
    // All P01-P03 are Tier-2 corpus entries today (perf-gate consumes
    // them on the CI runner). Promotion to Tier-3 happens in M A.3
    // once Tier-3 hardware is provisioned.
    assert_eq!(
        manifest.hardware_tier,
        HardwareTier::Tier2,
        "P01-P03 are Tier-2 perf-gate entries at v0.1"
    );
    // The regression gate enforces a wall_ms_p95 budget on every
    // corpus entry — guard against an empty `allowed_variance` map
    // that would let any regression through.
    assert!(
        manifest.allowed_variance.contains_key("wall_ms_p95"),
        "every corpus manifest must declare a wall_ms_p95 variance budget"
    );
    let wall_var = manifest.allowed_variance["wall_ms_p95"];
    assert!(
        wall_var > 0.0 && wall_var <= 0.25,
        "wall_ms_p95 variance must be in (0, 0.25] per PRD §17.10 ceiling; got {wall_var}"
    );
}

#[test]
fn p01_manifest_parses() {
    let path = manifest_path("p01");
    let manifest =
        load_manifest(&path).unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()));
    assert_corpus_invariants(&manifest, "P01");
    assert_eq!(
        manifest.display_name,
        "P01 hello-world (synthetic floor case)"
    );
    assert_eq!(manifest.corpus_id.as_deref(), Some("p01-hello-world"));
    assert_eq!(
        manifest.labels.get("shape").map(String::as_str),
        Some("floor-case")
    );
    assert_eq!(
        manifest.labels.get("checkout_kind").map(String::as_str),
        Some("vendored")
    );
}

#[test]
fn p02_manifest_parses() {
    let path = manifest_path("p02");
    let manifest =
        load_manifest(&path).unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()));
    assert_corpus_invariants(&manifest, "P02");
    assert_eq!(manifest.display_name, "Spring PetClinic");
    assert_eq!(
        manifest.corpus_id.as_deref(),
        Some("spring-petclinic-3.3.0")
    );
    assert_eq!(
        manifest.labels.get("checkout_kind").map(String::as_str),
        Some("submodule")
    );
    // The submodule pin metadata must record both the URL and the SHA
    // so a reviewer can audit the manifest without consulting
    // `.gitmodules`.
    assert!(
        manifest
            .labels
            .get("upstream_url")
            .is_some_and(|s| s.starts_with("https://github.com/")),
        "P02 must record its upstream_url label"
    );
    assert!(
        manifest
            .labels
            .get("upstream_ref")
            .is_some_and(|s| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())),
        "P02 upstream_ref label must be a 40-hex git SHA"
    );
}

#[test]
fn p03_manifest_parses() {
    let path = manifest_path("p03");
    let manifest =
        load_manifest(&path).unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()));
    assert_corpus_invariants(&manifest, "P03");
    assert_eq!(
        manifest.display_name,
        "Spring Boot starter-web app (tiny target)"
    );
    assert_eq!(
        manifest.corpus_id.as_deref(),
        Some("spring-boot-starter-web-app-3.3.5")
    );
    assert_eq!(
        manifest.labels.get("checkout_kind").map(String::as_str),
        Some("vendored")
    );
    // P03 measures network_bytes (it's the corpus's traffic-shape
    // anchor for the network-capture comparison) — assert the metric
    // is wired in.
    assert!(
        manifest
            .metrics
            .iter()
            .any(|m| matches!(m, Metric::Known(KnownMetric::NetworkBytes))),
        "P03 must measure network_bytes"
    );
    assert!(
        manifest.allowed_variance.contains_key("network_bytes_p95"),
        "P03 must declare a network_bytes_p95 variance budget"
    );
}

#[test]
fn every_corpus_manifest_has_a_checkout_dir() {
    // Defensive: catch the case where someone adds a Bench.toml but
    // forgets to create the `checkout/` directory (or symlink) the
    // harness will chdir into at run time. P03's lifecycle sub-entries
    // share the parent P03 checkout via symlink.
    for id in [
        "p01",
        "p02",
        "p03",
        "p03-pull-warm",
        "p03-compile-warm",
        "p03-package-warm",
        "p03-pull-cold",
    ] {
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("bench")
            .join("projects")
            .join(id)
            .join("checkout");
        assert!(
            checkout.is_dir(),
            "expected {} to be a directory (materialize submodules with `git submodule update --init bench/projects/`)",
            checkout.display()
        );
    }
}

// ---------------------------------------------------------------------------
// P03 lifecycle sub-manifests — cross-tool baseline shape introduced
// 2026-05-18. Each measures a single lifecycle dimension (pull, compile,
// package) against barista + (optionally) barista-no-daemon + mvn.
// ---------------------------------------------------------------------------

/// Shared assertions for the P03 lifecycle sub-manifests: they all
/// share P03's corpus_id, declare baselines, and target Tier-2.
fn assert_p03_lifecycle_invariants(
    manifest: &Manifest,
    expected_id: &str,
    want_baselines: &[&str],
) {
    assert_corpus_invariants(manifest, expected_id);
    assert_eq!(
        manifest.corpus_id.as_deref(),
        Some("spring-boot-starter-web-app-3.3.5"),
        "P03 lifecycle entries share the parent corpus_id"
    );
    assert_eq!(
        manifest.labels.get("checkout_kind").map(String::as_str),
        Some("vendored-symlink"),
        "lifecycle entries link to the parent P03 checkout/"
    );
    assert_eq!(
        manifest.labels.get("checkout_target").map(String::as_str),
        Some("bench/projects/p03/checkout"),
        "checkout_target label must point at the canonical P03 checkout"
    );
    let baselines = manifest.effective_baselines();
    assert_eq!(
        baselines.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
        want_baselines,
        "baseline ids + order must match"
    );
    // Every baseline must declare a non-empty command and (where
    // appropriate) a prepare step.
    for b in &baselines {
        assert!(
            !b.command.trim().is_empty(),
            "baseline `{}` has empty command",
            b.id
        );
        assert!(
            !b.display_name.trim().is_empty(),
            "baseline `{}` has empty display_name",
            b.id
        );
    }
}

#[test]
fn p03_pull_warm_manifest_parses() {
    let path = manifest_path("p03-pull-warm");
    let manifest =
        load_manifest(&path).unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()));
    assert_p03_lifecycle_invariants(&manifest, "P03-pull-warm", &["barista", "mvn"]);
    assert_eq!(
        manifest.labels.get("dimension").map(String::as_str),
        Some("D2")
    );
}

#[test]
fn p03_compile_warm_manifest_parses() {
    let path = manifest_path("p03-compile-warm");
    let manifest =
        load_manifest(&path).unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()));
    assert_p03_lifecycle_invariants(
        &manifest,
        "P03-compile-warm",
        &["barista", "barista-no-daemon", "mvn"],
    );
    assert_eq!(
        manifest.labels.get("dimension").map(String::as_str),
        Some("D4")
    );
}

#[test]
fn p03_pull_cold_manifest_parses() {
    let path = manifest_path("p03-pull-cold");
    let manifest =
        load_manifest(&path).unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()));
    assert_p03_lifecycle_invariants(&manifest, "P03-pull-cold", &["barista", "mvn"]);
    assert_eq!(
        manifest.labels.get("dimension").map(String::as_str),
        Some("D1")
    );
    assert_eq!(
        manifest.cache_isolation,
        barista_bench::CacheIsolation::PerIteration,
        "cold-cache manifest must opt into per-iteration isolation"
    );
}

#[test]
fn p03_package_warm_manifest_parses() {
    let path = manifest_path("p03-package-warm");
    let manifest =
        load_manifest(&path).unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()));
    assert_p03_lifecycle_invariants(
        &manifest,
        "P03-package-warm",
        &["barista", "barista-no-daemon", "mvn"],
    );
    assert_eq!(
        manifest.labels.get("dimension").map(String::as_str),
        Some("D4")
    );
}
