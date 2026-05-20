// SPDX-License-Identifier: MIT OR Apache-2.0

//! `Bench.toml` — per-project benchmark manifest schema.
//!
//! Every benchmark target (a Tier-1 microbench fixture inside a crate's
//! `benches/` directory, or a Tier-2/3 reference project checked in under
//! `bench/projects/`) ships with a `Bench.toml` declaring **how to run
//! it**, **what to measure**, and **what tier of hardware it targets**.
//! The manifest is the only on-disk source of truth for those facts —
//! the harness reads it, the regression gate reads it, the dashboard
//! reads it.
//!
//! # Example
//!
//! ```toml
//! schema = "barista.bench.manifest/v1"
//! id = "P02"
//! display_name = "Spring PetClinic"
//! category = "corpus"
//! corpus_id = "spring-petclinic-3.3.0"
//! command = "barista verify"
//! hardware_tier = 3
//! iterations = 5
//! warmup_iterations = 1
//! metrics = ["wall_ms", "cpu_user_ms", "peak_rss_kb"]
//!
//! [allowed_variance]
//! wall_ms_p95 = 0.10
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{MANIFEST_SCHEMA, error::Error};

/// Top-level `Bench.toml` document.
///
/// Field naming and structure track PRD §17.8. The harness in
/// `barista-bench` (CLI) and the Tier-2 regression gate both
/// deserialize this struct directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Schema discriminator. Must equal [`crate::MANIFEST_SCHEMA`]
    /// (`"barista.bench.manifest/v1"`).
    ///
    /// Future revisions will bump the suffix; consumers should reject
    /// unknown values rather than guess.
    pub schema: String,

    /// Stable identifier for this benchmark target. For Tier-3 corpus
    /// projects this matches the `P01..P12` IDs from PRD §17.5; for
    /// Tier-1 microbenches it is a `kebab-case` slug unique to the
    /// crate.
    pub id: String,

    /// Human-readable name for dashboard rows and report headings.
    pub display_name: String,

    /// What kind of benchmark target this manifest describes.
    pub category: Category,

    /// Optional foreign key into a checked-in project corpus
    /// (`bench/projects/<corpus_id>/`) or microbench fixture
    /// (`crates/<crate>/benches/fixtures/<corpus_id>/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus_id: Option<String>,

    /// Default shell command line to invoke under measurement.
    ///
    /// Backwards-compatible with the v0.1 single-baseline manifest
    /// shape: when [`Self::baselines`] is empty, the harness derives
    /// a single implicit baseline with `id = "barista"` whose
    /// `command` equals this field. When [`Self::baselines`] is
    /// non-empty, every measurement runs out of an explicit
    /// `[[baselines]]` entry; this field is retained on the document
    /// for tools that still consult it (e.g. the perf-gate workflow's
    /// placeholder), but the harness ignores it at run time.
    pub command: String,

    /// Optional cross-tool baselines: one entry per tool variant to
    /// measure (e.g. `barista`, `barista-no-daemon`, `mvn`, `mvnd`).
    /// Each `(manifest, baseline)` pair produces one `results.json`
    /// document. Empty (the default) preserves the legacy
    /// single-baseline shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub baselines: Vec<Baseline>,

    /// Which counters the harness should capture for every iteration.
    /// The set must be non-empty.
    pub metrics: Vec<Metric>,

    /// Which hardware tier this manifest targets.
    pub hardware_tier: HardwareTier,

    /// Number of measured iterations. Default 5; the harness reports
    /// median + p95 across these.
    #[serde(default = "default_iterations")]
    pub iterations: u32,

    /// Number of un-measured warmup iterations. Default 1.
    #[serde(default = "default_warmup_iterations")]
    pub warmup_iterations: u32,

    /// Seconds to sleep between successive iterations (warmup AND
    /// measured), NOT before the first or after the last. Default 0.
    ///
    /// Cold-cache manifests opt into a non-zero value to space
    /// real-network iterations under Maven Central's rate-limit
    /// threshold. Three back-to-back ~438-request cold pulls have
    /// been observed to trigger HTTP 429 from Maven Central; a
    /// `iteration_spacing_seconds = 60` gap keeps the cell under
    /// the threshold in practice.
    ///
    /// Warm-cache manifests should leave this at the default 0 —
    /// there's no upstream traffic to throttle.
    #[serde(default)]
    pub iteration_spacing_seconds: u32,

    /// Optional per-metric variance budget. Map key is a metric name
    /// (e.g. `"wall_ms_p95"`); value is the fractional drift tolerated
    /// by the regression gate (e.g. `0.10` for 10%).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub allowed_variance: BTreeMap<String, f64>,

    /// Free-form labels attached to dashboard rows (e.g. `"shape": "library"`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,

    /// Cache-isolation policy for the harness. Default `none` means
    /// every iteration shares the caller's caches (warm-cache
    /// scenario — `~/.barista/cache` + `~/.m2` populated by prior
    /// runs). `per-iteration` tells the harness to allocate a fresh
    /// tempdir per measured iteration and route both barista (via
    /// `BARISTA_PATHS__CACHE_DIR`) and mvn/mvnd (via `MAVEN_OPTS=-Dmaven.repo.local=...`)
    /// to it — every iteration genuinely re-fetches from upstream,
    /// which is the only honest way to measure "calls to Maven
    /// Central" on a project.
    #[serde(default)]
    pub cache_isolation: CacheIsolation,
}

/// How the harness manages per-iteration cache state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CacheIsolation {
    /// Share the caller's caches across iterations (warm scenario).
    #[default]
    None,
    /// Allocate a fresh tempdir per iteration so every iteration is
    /// a cold-cache run. Surface for the per-PRD-§17.10 D1 (cold
    /// dependency resolution) and D4 (cold build) dimensions.
    PerIteration,
}

/// Benchmark category — drives which harness consumes the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    /// Tier-1 in-process microbenchmark driven by `criterion`.
    Microbench,
    /// Tier-2 / Tier-3 task-oriented reference project.
    Corpus,
}

/// One cross-tool baseline inside [`Manifest::baselines`].
///
/// Each entry names a single tool variant (e.g. `mvn`, `mvnd`,
/// `barista`, `barista-no-daemon`) and the exact command line that
/// invokes it. The harness produces one `results.json` document per
/// baseline so the dashboard can render each variant as its own row /
/// trend line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    /// Stable identifier for this baseline, lowercase kebab-case
    /// (e.g. `"barista"`, `"barista-no-daemon"`, `"mvn"`, `"mvnd"`).
    /// Echoed into `results.json::baseline_id`; the dashboard uses
    /// it as the chart-series key.
    pub id: String,

    /// Human-readable name for the dashboard (e.g.
    /// `"barista (warm daemon)"`).
    pub display_name: String,

    /// Argv-style command line — the harness splits on whitespace and
    /// runs `argv[0]` with `argv[1..]` as arguments. Shell features
    /// (`&&`, `|`, `$VAR`) are NOT supported; for multi-step setup
    /// use [`Self::prepare`].
    pub command: String,

    /// Optional command run once before each measured iteration (e.g.
    /// `rm -rf target` to force a clean compile). Run synchronously;
    /// its wall-clock time is NOT included in the measurement.
    /// Argv-style (same splitting rules as [`Self::command`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepare: Option<String>,
}

/// Hardware tier the manifest is calibrated for. See PRD §17.6.
///
/// The discriminant values match the `1..=3` integers used in the
/// JSON-Schema for results documents so the wire format and the Rust
/// type stay in lock-step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum HardwareTier {
    /// Tier 1 — local developer machine. Variance acceptable; used
    /// for `cargo bench` style microbenches.
    Tier1,
    /// Tier 2 — self-hosted CI runner (e.g. `R-Bench-1`). Concurrency
    /// pinned to 1; controlled variance.
    Tier2,
    /// Tier 3 — public reference hardware (e.g. `R-Bench-3` on AWS).
    /// Publishable results.
    Tier3,
}

impl From<HardwareTier> for u8 {
    fn from(t: HardwareTier) -> u8 {
        match t {
            HardwareTier::Tier1 => 1,
            HardwareTier::Tier2 => 2,
            HardwareTier::Tier3 => 3,
        }
    }
}

impl TryFrom<u8> for HardwareTier {
    type Error = String;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            1 => Ok(HardwareTier::Tier1),
            2 => Ok(HardwareTier::Tier2),
            3 => Ok(HardwareTier::Tier3),
            other => Err(format!("hardware_tier must be 1, 2, or 3 (got {other})")),
        }
    }
}

/// Named metric counter the harness can capture per iteration.
///
/// `Other` carries an arbitrary string so a manifest can request a
/// metric that the harness emits but this enum does not yet enumerate
/// (e.g. `cache_hit_rate`). Unknown metric names round-trip cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", untagged)]
pub enum Metric {
    /// Well-known metric (closed enum).
    Known(KnownMetric),
    /// Free-form metric name, e.g. `"cache_hit_rate"`.
    Other(String),
}

/// Closed enumeration of metrics the harness knows how to capture
/// natively. New entries are additive (forward-compatible).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnownMetric {
    /// Wall-clock time, milliseconds.
    WallMs,
    /// User-mode CPU time, milliseconds.
    CpuUserMs,
    /// System-mode CPU time, milliseconds.
    CpuSysMs,
    /// Peak resident-set size, kibibytes.
    PeakRssKb,
    /// Number of distinct HTTP requests the build emitted to upstream
    /// repositories during the iteration. Captured via the
    /// `--capture` harness mode by parsing a mitmproxy HAR; absent on
    /// timing-pass runs.
    NetworkCalls,
    /// Bytes read from the network during the run. Counted from HAR
    /// response payloads under `--capture`; absent on timing-pass
    /// runs.
    NetworkBytes,
    /// Bytes read from disk during the run.
    DiskReadBytes,
    /// Bytes written to disk during the run.
    DiskWriteBytes,
}

fn default_iterations() -> u32 {
    5
}

fn default_warmup_iterations() -> u32 {
    1
}

impl Manifest {
    /// Parse a manifest from a TOML string.
    ///
    /// Performs structural validation beyond what `serde` can express:
    /// `schema` must match [`crate::MANIFEST_SCHEMA`], `id` /
    /// `display_name` / `command` must be non-empty, and `metrics`
    /// must contain at least one entry.
    pub fn from_toml_str(input: &str) -> Result<Self, Error> {
        let manifest: Manifest = toml::from_str(input)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Serialize the manifest back to TOML.
    pub fn to_toml_string(&self) -> Result<String, Error> {
        // toml::to_string never fails for our owned types, but we
        // surface the error path defensively in case of future fields.
        toml::to_string(self).map_err(|e| Error::ManifestInvalid(e.to_string()))
    }

    fn validate(&self) -> Result<(), Error> {
        if self.schema != MANIFEST_SCHEMA {
            return Err(Error::ManifestInvalid(format!(
                "schema must be `{MANIFEST_SCHEMA}` (got `{}`)",
                self.schema
            )));
        }
        if self.id.trim().is_empty() {
            return Err(Error::ManifestInvalid("id must not be empty".into()));
        }
        if self.display_name.trim().is_empty() {
            return Err(Error::ManifestInvalid(
                "display_name must not be empty".into(),
            ));
        }
        if self.command.trim().is_empty() {
            return Err(Error::ManifestInvalid("command must not be empty".into()));
        }
        if self.metrics.is_empty() {
            return Err(Error::ManifestInvalid(
                "metrics must contain at least one entry".into(),
            ));
        }
        if self.iterations == 0 {
            return Err(Error::ManifestInvalid("iterations must be >= 1".into()));
        }
        // Baseline-section validation: ids must be unique + non-empty,
        // display_name + command must be non-empty per entry.
        let mut seen_ids = std::collections::HashSet::new();
        for (i, b) in self.baselines.iter().enumerate() {
            if b.id.trim().is_empty() {
                return Err(Error::ManifestInvalid(format!(
                    "baselines[{i}].id must not be empty"
                )));
            }
            if !seen_ids.insert(b.id.clone()) {
                return Err(Error::ManifestInvalid(format!(
                    "duplicate baseline id `{}` in baselines[{i}]",
                    b.id
                )));
            }
            if b.display_name.trim().is_empty() {
                return Err(Error::ManifestInvalid(format!(
                    "baselines[{i}] (id `{}`).display_name must not be empty",
                    b.id
                )));
            }
            if b.command.trim().is_empty() {
                return Err(Error::ManifestInvalid(format!(
                    "baselines[{i}] (id `{}`).command must not be empty",
                    b.id
                )));
            }
        }
        Ok(())
    }

    /// Return the effective list of baselines for this manifest.
    ///
    /// When [`Self::baselines`] is non-empty, returns clones of those
    /// entries. When empty, derives a single implicit baseline with
    /// `id = "barista"`, `display_name = "barista"`, `command =
    /// Self::command` — preserving the v0.1 single-baseline shape.
    pub fn effective_baselines(&self) -> Vec<Baseline> {
        if self.baselines.is_empty() {
            vec![Baseline {
                id: "barista".to_string(),
                display_name: "barista".to_string(),
                command: self.command.clone(),
                prepare: None,
            }]
        } else {
            self.baselines.clone()
        }
    }
}
