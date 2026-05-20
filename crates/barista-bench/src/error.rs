// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error types for the benchmark harness.

use std::path::PathBuf;

use thiserror::Error;

/// Errors surfaced by the public API of `barista-bench`.
#[derive(Debug, Error)]
pub enum Error {
    /// I/O error reading or writing a benchmark file on disk.
    #[error("i/o error at {path}: {source}")]
    Io {
        /// The file path that was being accessed when the error occurred.
        path: PathBuf,
        /// The underlying [`std::io::Error`].
        #[source]
        source: std::io::Error,
    },

    /// The `Bench.toml` manifest could not be deserialized.
    ///
    /// The wrapped [`toml::de::Error`] preserves line / column
    /// information so authors of `Bench.toml` files get actionable
    /// diagnostics.
    #[error("manifest parse error: {0}")]
    ManifestParse(#[from] toml::de::Error),

    /// The manifest deserialized but violated a structural invariant
    /// the type system cannot express (e.g. an empty `id`).
    #[error("invalid manifest: {0}")]
    ManifestInvalid(String),

    /// Serializing a [`crate::ResultsDocument`] to JSON failed.
    ///
    /// In practice this only fires for unsupported map key types or
    /// custom `Serialize` impls that bubble up errors — the default
    /// derived impl on [`crate::ResultsDocument`] does not.
    #[error("results serialization error: {0}")]
    ResultsSerialize(#[source] serde_json::Error),
}
