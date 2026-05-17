// Tests legitimately use `expect`/`unwrap`/`panic!` to keep failure
// messages compact; scope the exemption to `#[cfg(test)]` so
// production code in `src/` still has to justify each panic-path.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! Network-capture analysis pipeline for Barista's resource-efficiency
//! program.
//!
//! `barista-netanalyze` consumes the `.har` files emitted by
//! `barista-netcap` and runs a battery of rule-based analyzers
//! against them, producing draft *findings* — structured records of
//! observed inefficiencies, each pointing back at the HAR entries
//! that triggered it.
//!
//! ## Three-line usage
//!
//! ```no_run
//! # fn demo() -> Result<(), barista_netanalyze::AnalyzeError> {
//! use barista_netanalyze::{analyze, load_har, write_findings};
//!
//! let har = load_har("/tmp/session.har".as_ref())?;
//! let findings = analyze(&har);
//! write_findings(&findings, "auto-generated/".as_ref())?;
//! # Ok(()) }
//! ```
//!
//! ## What "AI-assisted" means here
//!
//! The crate-level brief calls the pipeline "AI-assisted". That
//! refers to **authoring** — the rule set in [`analyzer`] was
//! drafted with Claude Code from the optimization catalogs in PRD
//! §18.3–§18.6 — not to runtime. The pipeline performs **no** LLM
//! inference; it is a deterministic, pure-function evaluation over
//! a parsed HAR. The continuous-AI review pass over the
//! auto-generated findings corpus (PRD §18.9 step 6) is an
//! out-of-band workflow, not embedded in this crate.
//!
//! ## ID-assignment policy
//!
//! Findings are emitted with the placeholder ID
//! [`Finding::PENDING_ID`] (`EFF-2026-PENDING`). The pipeline does
//! **not** allocate `EFF-2026-NNN` identifiers — a human reviewer
//! picks the next free NNN from the catalog
//! (`docs/efficiency/findings/`) when promoting the draft. This
//! keeps the pipeline stateless and makes the catalog the single
//! source of truth for live IDs.
//!
//! See [`Finding`] for the full output shape and
//! [`Finding::to_markdown`] for the canonical on-disk format.
//!
//! [`Finding`]: crate::finding::Finding
//! [`Finding::PENDING_ID`]: crate::finding::Finding::PENDING_ID
//! [`Finding::to_markdown`]: crate::finding::Finding::to_markdown

pub mod analyzer;
pub mod error;
pub mod finding;
pub mod har;
pub mod pipeline;

pub use analyzer::{
    Analyzer, ConnectionChurnAnalyzer, ConnectionChurnConfig, DuplicateRequestAnalyzer,
    DuplicateRequestConfig, MetadataOverFetchAnalyzer, MetadataOverFetchConfig,
    SlowRedirectAnalyzer, SlowRedirectConfig, UncompressedTransferAnalyzer,
    UncompressedTransferConfig, default_registry,
};
pub use error::AnalyzeError;
pub use finding::{Category, EvidenceEntry, Finding, ImpactEstimate, Severity, Status};
pub use har::{Har, HarContent, HarEntry, HarHeader, HarLog, HarRequest, HarResponse, HarTimings};
pub use pipeline::{analyze, analyze_with, load_har, write_findings};
