// SPDX-License-Identifier: MIT OR Apache-2.0

//! `Analyzer` trait + the default analyzer registry.
//!
//! ## What "AI-assisted" means in v0.1
//!
//! The task brief calls this an "AI-assisted analysis pipeline".
//! That refers to **authoring**, not runtime: the rule-based
//! analyzers below were drafted with Claude Code from the
//! optimization catalogs in PRD §18.3–§18.6 and the pattern roster
//! in §18.9. The pipeline itself runs *no* LLM inference — it
//! evaluates pure functions over a parsed HAR. The continuous-AI
//! analysis step described in PRD §18.9 step 6 is an out-of-band
//! review pass over the auto-generated findings corpus, owned by a
//! separate workstream (not this crate).
//!
//! ## Trait shape
//!
//! Analyzers are pure functions over `&Har`. They never mutate
//! shared state and never perform I/O — the orchestrator (`pipeline`
//! module) collects results, sorts them, and writes the markdown.
//!
//! Each analyzer also carries an [`Analyzer::id`] which is written
//! into the finding's `discovered_by` frontmatter so the catalog can
//! trace findings back to the rule that produced them.

use crate::finding::Finding;
use crate::har::Har;

pub mod connection_churn;
pub mod duplicate_request;
pub mod metadata_overfetch;
pub mod slow_redirect;
pub mod uncompressed_transfer;

pub use connection_churn::{ConnectionChurnAnalyzer, ConnectionChurnConfig};
pub use duplicate_request::{DuplicateRequestAnalyzer, DuplicateRequestConfig};
pub use metadata_overfetch::{MetadataOverFetchAnalyzer, MetadataOverFetchConfig};
pub use slow_redirect::{SlowRedirectAnalyzer, SlowRedirectConfig};
pub use uncompressed_transfer::{UncompressedTransferAnalyzer, UncompressedTransferConfig};

/// Pure-function analyzer trait. Implementors operate on a parsed
/// HAR and emit zero or more findings.
///
/// `Analyzer` is object-safe so the pipeline can hold a heterogeneous
/// `Vec<Box<dyn Analyzer>>` rather than a sum type — adding a new
/// analyzer doesn't touch a central enum.
pub trait Analyzer {
    /// Stable identifier for this analyzer (e.g.
    /// `"DuplicateRequestAnalyzer"`). Written into the finding's
    /// `discovered_by` frontmatter for traceability. Must be unique
    /// across the registry.
    fn id(&self) -> &'static str;

    /// Evaluate the analyzer against `har` and return findings.
    /// Implementations must be deterministic — re-running against
    /// the same input produces the same output.
    fn analyze(&self, har: &Har) -> Vec<Finding>;
}

/// The default registry: every v0.1 analyzer with its default
/// thresholds. The pipeline calls this when no custom registry is
/// supplied.
///
/// ## Roster
///
/// 1. [`DuplicateRequestAnalyzer`] — same URL + body fetched ≥ 2×
///    within one session.
/// 2. [`UncompressedTransferAnalyzer`] — compressible MIME without
///    `Content-Encoding` and above a size threshold.
/// 3. [`ConnectionChurnAnalyzer`] — many fresh TCP/TLS handshakes to
///    a single host where a single H/2 connection would suffice.
/// 4. [`SlowRedirectAnalyzer`] — redirect chains where each hop
///    takes more than a configurable wall-time budget.
/// 5. [`MetadataOverFetchAnalyzer`] — repeated `maven-metadata.xml`
///    fetches on the same `(repo, groupId, artifactId)` triple.
#[must_use]
pub fn default_registry() -> Vec<Box<dyn Analyzer>> {
    vec![
        Box::new(DuplicateRequestAnalyzer::with_defaults()),
        Box::new(UncompressedTransferAnalyzer::with_defaults()),
        Box::new(ConnectionChurnAnalyzer::with_defaults()),
        Box::new(SlowRedirectAnalyzer::with_defaults()),
        Box::new(MetadataOverFetchAnalyzer::with_defaults()),
    ]
}
