// SPDX-License-Identifier: MIT OR Apache-2.0

//! Benchmark harness and schemas for Barista.
//!
//! This crate is the source of truth for the two on-disk contracts that
//! drive Barista's performance program:
//!
//! 1. **`Bench.toml`** — a per-project manifest declaring how a benchmark
//!    target should be invoked, which metrics to capture, and what tier
//!    of hardware it targets. Authored by humans and checked in alongside
//!    each reference project (Tier 2/3) or per-crate microbench fixture
//!    (Tier 1).
//! 2. **`results.json`** — a per-run measurement blob emitted by the
//!    harness. One file is written per benchmark run; the dashboard
//!    backend uploads these to `bench.barista.build/data/`.
//!
//! Both schemas are mirrored as JSON-Schema Draft 2020-12 files under
//! [`schema/`](https://github.com/buildwithbarista/barista/tree/main/crates/barista-bench/schema)
//! so external tools (CI gates, the dashboard, third-party reviewers)
//! can validate documents without instantiating the Rust types.
//!
//! # Three benchmark tiers
//!
//! - **Tier 1** — sub-second to ~60-second microbenchmarks driven by
//!   [`criterion`](https://docs.rs/criterion) inside each crate's
//!   `benches/` directory. Tier-1 results are produced locally by
//!   developers and surfaced in the regression gate.
//! - **Tier 2** — CI/CD performance gate. A subset of the competitive
//!   corpus runs on a self-hosted runner on every PR touching the
//!   resolver, cache, IPC, or `barback`.
//! - **Tier 3** — public competitive corpus. Runs on every release tag
//!   and nightly. Published to `bench.barista.build`.
//!
//! All three tiers emit the same `results.json` shape — the `hardware_tier`
//! field on each results document tags which tier produced it.
//!
//! # Library surface (v0.1)
//!
//! The crate intentionally exposes a small surface: parse a manifest,
//! write a results document, surface a typed error. Higher-level glue
//! (CLI, corpus runner, dashboard uploader) lives in downstream crates
//! and CI workflows; they consume the types defined here.
//!
//! ```no_run
//! use barista_bench::{load_manifest, write_results, ResultsDocument};
//!
//! let manifest = load_manifest("Bench.toml").expect("parse manifest");
//! // ... run the benchmark, build `results` ...
//! # let results: ResultsDocument = unimplemented!();
//! write_results("bench-results/results.json", &results).expect("write results");
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod manifest;
pub mod results;

use std::fs;
use std::path::Path;

pub use error::Error;
pub use manifest::{Baseline, CacheIsolation, HardwareTier, Manifest, Metric};
pub use results::{IterationMeasurement, ResultsDocument, RunHardware, Summary};

/// Schema identifier emitted in `Bench.toml`'s top-level `schema` field.
pub const MANIFEST_SCHEMA: &str = "barista.bench.manifest/v1";

/// Schema identifier emitted in `results.json`'s top-level `schema` field.
pub const RESULTS_SCHEMA: &str = "barista.bench.results/v1";

/// Load and parse a `Bench.toml` manifest from disk.
///
/// Returns an [`Error::Io`] if the file cannot be read, or
/// [`Error::ManifestParse`] if the contents are not a valid manifest.
pub fn load_manifest<P: AsRef<Path>>(path: P) -> Result<Manifest, Error> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Manifest::from_toml_str(&raw)
}

/// Serialize a [`ResultsDocument`] to JSON and write it to disk.
///
/// The document is written with two-space indentation and a trailing
/// newline so diffs against checked-in fixtures stay readable. Returns
/// [`Error::Io`] on filesystem errors or [`Error::ResultsSerialize`]
/// if serialization fails.
pub fn write_results<P: AsRef<Path>>(path: P, document: &ResultsDocument) -> Result<(), Error> {
    let path = path.as_ref();
    let mut json = serde_json::to_string_pretty(document).map_err(Error::ResultsSerialize)?;
    json.push('\n');
    fs::write(path, json).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}
