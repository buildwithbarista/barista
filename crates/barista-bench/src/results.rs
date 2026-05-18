//! `results.json` — per-run benchmark results document.
//!
//! Emitted by the harness at the end of every benchmark run; consumed
//! by the regression gate (Tier 2) and the dashboard backend (Tier 3).
//! The on-disk shape is locked by the JSON-Schema at
//! `schema/results.schema.json` — this module is the Rust mirror.
//!
//! # Forward compatibility
//!
//! The top-level document, [`RunHardware`], [`IterationMeasurement`],
//! and [`Summary`] are **closed** (`additionalProperties: false` on the
//! JSON side, `#[serde(deny_unknown_fields)]` on the Rust side).
//! Producers that want to attach extra context use the `metadata`
//! map — it is intentionally permissive so the on-disk format does not
//! need a schema bump every time a new label is added.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::manifest::HardwareTier;

/// Top-level `results.json` document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultsDocument {
    /// Schema discriminator. Always [`crate::RESULTS_SCHEMA`]
    /// (`"barista.bench.results/v1"`).
    pub schema: String,

    /// Identifier of the manifest that produced this run. Equal to
    /// [`crate::Manifest::id`].
    pub manifest_id: String,

    /// Optional identifier of the baseline within the manifest's
    /// `[[baselines]]` array (e.g. `"barista"`, `"mvn"`). Omitted on
    /// legacy single-baseline runs so older results documents
    /// continue to validate against the v1 schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_id: Option<String>,

    /// Optional record of the actual command line invoked under
    /// measurement. Lets reviewers audit what was measured without
    /// back-referencing the manifest's `[[baselines]]` array.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_command: Option<String>,

    /// Stable identifier for this run. Convention: `<rfc3339>-<git_sha_short>`,
    /// e.g. `2026-05-10T18:30:00Z-abcd1234`.
    pub run_id: String,

    /// RFC 3339 timestamp at which the run started. Consumers may
    /// parse with `chrono::DateTime::parse_from_rfc3339`.
    pub timestamp: String,

    /// Git SHA of the `barista` source tree producing the binary
    /// under test. Full 40-char SHA; abbreviated SHAs are rejected by
    /// the JSON-Schema.
    pub git_sha: String,

    /// Semver version of the `barista` binary under test.
    pub barista_version: String,

    /// Hardware tier this run was executed on. Matches the manifest's
    /// declared tier; the harness asserts equality at run start.
    pub hardware_tier: HardwareTier,

    /// Stable identifier for the runner that produced this document
    /// (e.g. `R-Bench-1`). Used for result signing (PRD §17.12) and
    /// dashboard provenance.
    pub runner_id: String,

    /// Captured hardware fingerprint. Mirrors PRD §17.8 `manifest.json`.
    pub hardware: RunHardware,

    /// One entry per measured iteration, in execution order. Excludes
    /// warmup iterations.
    pub iterations: Vec<IterationMeasurement>,

    /// Aggregate statistics across [`Self::iterations`]. Required so
    /// downstream consumers do not have to recompute them.
    pub summary: Summary,

    /// Free-form labels attached to the document. Unlike the rest of
    /// the schema this map allows arbitrary string keys / values so
    /// producers can attach forward-compatible context.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// Hardware fingerprint of the runner that produced a results document.
///
/// Loosely modeled on the `manifest.json` block in PRD §17.8; trimmed
/// to the fields a results consumer actually needs (the full provision
/// record lives in the upstream `manifest.json` blob alongside this
/// document).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunHardware {
    /// Hardware ID, e.g. `R-Bench-1`.
    pub id: String,
    /// CPU model string, e.g. `AMD Ryzen 9 7950X`.
    pub cpu: String,
    /// Physical core count.
    pub cores_physical: u32,
    /// Logical (SMT) core count.
    pub cores_logical: u32,
    /// Installed RAM in gibibytes.
    pub memory_gb: u32,
    /// Operating-system identifier, e.g. `Ubuntu 24.04`.
    pub os: String,
}

/// One measured iteration.
///
/// The harness fills in whichever metrics the manifest requested; the
/// rest are `None`. `wall_ms` is always populated because every
/// supported manifest is required to capture it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IterationMeasurement {
    /// 0-indexed iteration number among measured (non-warmup) runs.
    pub iteration: u32,

    /// Wall-clock duration, milliseconds. Always present.
    pub wall_ms: u64,

    /// User-mode CPU time, milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_user_ms: Option<u64>,

    /// System-mode CPU time, milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_sys_ms: Option<u64>,

    /// Peak resident-set size, kibibytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_kb: Option<u64>,

    /// Network bytes read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_bytes: Option<u64>,

    /// Disk bytes read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_read_bytes: Option<u64>,

    /// Disk bytes written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_write_bytes: Option<u64>,

    /// Subprocess exit code reported by the runner. `0` on success.
    pub exit_code: i32,
}

/// Aggregate summary across [`ResultsDocument::iterations`].
///
/// All fields are computed on the primary metric for the manifest,
/// which is wall-clock milliseconds in v0.1. Future revisions may
/// add per-metric summary blocks; the current shape is intentionally
/// flat to keep dashboard glue simple.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    /// Arithmetic mean across iterations.
    pub avg_wall_ms: f64,
    /// Median across iterations.
    pub median_wall_ms: f64,
    /// 95th-percentile (nearest-rank) across iterations.
    pub p95_wall_ms: f64,
    /// Sample standard deviation across iterations.
    pub stddev_wall_ms: f64,
}
