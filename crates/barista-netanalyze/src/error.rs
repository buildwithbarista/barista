//! Public error type for `barista-netanalyze`.
//!
//! Mirrors the shape of `barista-netcap::NetcapError`: a single enum
//! with one variant per failure mode the CLI / integration tests want
//! to pattern-match against. The two crates intentionally do *not*
//! share an error type — netcap and netanalyze are loosely coupled
//! through the on-disk HAR contract, not through Rust types.

use std::io;
use std::path::PathBuf;

/// Failure modes for HAR parsing and analyzer execution.
#[derive(Debug, thiserror::Error)]
pub enum AnalyzeError {
    /// The HAR file at the given path could not be read from disk.
    /// Carries the path so the CLI can render a precise diagnostic.
    #[error("could not read HAR file at {path}: {source}")]
    HarRead {
        /// Path we attempted to read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// The HAR file did not parse as JSON, or did not match the HAR
    /// 1.2 envelope we expect (top-level `log` object with an
    /// `entries` array). The `reason` field describes which check
    /// failed.
    #[error("HAR file at {path} is structurally invalid: {reason}")]
    HarInvalid {
        /// Path that failed to parse.
        path: PathBuf,
        /// Human-readable explanation.
        reason: String,
    },

    /// An I/O failure occurred while writing a finding markdown file
    /// to the output directory. Carries the path so the caller can
    /// drill into permissions / ENOSPC cases.
    #[error("could not write finding markdown at {path}: {source}")]
    FindingWrite {
        /// Path the writer tried to create.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// Catch-all for I/O failures during pipeline bookkeeping
    /// (creating the output directory, listing fixtures, etc.).
    #[error("netanalyze I/O error: {0}")]
    Io(#[from] io::Error),
}
