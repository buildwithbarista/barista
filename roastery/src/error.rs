//! Typed errors for the roastery server.
//!
//! The crate splits errors in two layers:
//!
//! - [`RoasteryError`] is the top-level error the binary surfaces from
//!   `run`. It composes lower-level error kinds (config, bind, raw
//!   I/O, storage).
//! - [`StorageError`] is the content-addressed storage layer's
//!   dedicated error type. It is exposed on the public `storage::Cas`
//!   trait so callers (the barista-protocol handler in M5.1 T3, the
//!   REAPI gRPC handler in M5.1 T4) can pattern-match on the specific
//!   failure mode — `NotFound` vs `DigestMismatch` vs `NotImplemented`
//!   — without parsing strings.
//!
//! `StorageError` converts into `RoasteryError` via `From` so storage
//! failures can bubble through `Result<T, RoasteryError>` at the top
//! level when the caller doesn't care about the distinction.

use std::io;
use std::net::SocketAddr;
use std::path::Path;

use thiserror::Error;

/// All fatal errors the roastery server can surface from `run`.
#[derive(Debug, Error)]
pub enum RoasteryError {
    /// Server configuration is invalid (bad address, unreadable
    /// storage directory, malformed upstream URL, …).
    #[error("invalid server configuration: {0}")]
    Config(String),

    /// Could not bind the TCP listener to the configured address.
    #[error("failed to bind listener on {addr}: {source}")]
    Bind {
        /// The address the server attempted to bind.
        addr: SocketAddr,
        /// The underlying OS error from `tokio::net::TcpListener`.
        #[source]
        source: io::Error,
    },

    /// Generic I/O failure (storage-dir creation, listener accept
    /// loop, signal-handler installation).
    #[error("I/O error: {source}")]
    Io {
        #[from]
        source: io::Error,
    },

    /// Content-addressed storage backend failure.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

impl RoasteryError {
    /// Construct a [`RoasteryError::Config`] complaining about a
    /// path-shaped value. Used by `ServerConfig` validation.
    pub(crate) fn config_path(reason: &str, path: &Path) -> Self {
        RoasteryError::Config(format!("{reason}: {}", path.display()))
    }
}

/// Crate-wide `Result` alias.
pub type Result<T> = std::result::Result<T, RoasteryError>;

/// Errors surfaced by the content-addressed storage layer
/// (`crate::storage::Cas` implementations).
///
/// Public so callers can match on specific failure modes. Converts
/// into [`RoasteryError::Storage`] for callers that just want to
/// bubble the error up.
#[derive(Debug, Error)]
pub enum StorageError {
    /// An underlying I/O operation failed (open, read, write, rename,
    /// remove, metadata, directory walk).
    #[error("storage I/O error: {0}")]
    Io(#[from] io::Error),

    /// `put` finished writing the bytes but the hash of what was
    /// written did not match the digest the caller claimed. The
    /// partial write has been discarded; no entry was added to the
    /// store.
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch {
        /// The digest the caller passed to `put`.
        expected: crate::storage::Digest,
        /// The digest that was actually computed over the streamed
        /// bytes.
        actual: crate::storage::Digest,
    },

    /// A hex string passed to [`crate::storage::Digest::from_hex`] was
    /// not a 64-character lowercase SHA-256 hex digest.
    #[error("invalid digest: {reason}")]
    InvalidDigest {
        /// Human-readable explanation (wrong length, non-hex char, …).
        reason: String,
    },

    /// The backend exists at the type level but its trait methods are
    /// not yet wired. Returned by the `S3Cas` and `GcsCas` stubs so
    /// the trait surface can be exercised by tests + so config files
    /// referencing those backends parse cleanly today.
    #[error("backend {backend} is not yet implemented")]
    NotImplemented {
        /// Stable backend identifier (`"s3"`, `"gcs"`).
        backend: &'static str,
    },

    /// Catch-all for backend-specific failure modes that don't map to
    /// the variants above (e.g. corrupted directory structure under
    /// `<root>/cas/`).
    #[error("storage error: {context}")]
    Other {
        /// Human-readable context.
        context: String,
    },
}
