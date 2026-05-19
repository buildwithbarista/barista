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

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
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

/// JSON-serialisable error body the protocol handlers return on every
/// non-2xx response.
///
/// The shape is stable: callers (the Barista CLI, third-party clients)
/// can match on `code` to dispatch on the failure mode without parsing
/// `message`. Optional `expected` / `actual` fields are populated for
/// digest-mismatch errors so a client can surface "you said X, the
/// bytes hashed to Y" diagnostics without a second round-trip.
///
/// ## `BAR-CAS-NNN` code reference
///
/// | Code         | HTTP | Meaning                                                   |
/// |--------------|------|-----------------------------------------------------------|
/// | `BAR-CAS-001`| 400  | Digest in URL/header disagreed with the body's hash.      |
/// | `BAR-CAS-002`| 400  | Digest string was not a 64-char lowercase hex SHA-256.    |
/// | `BAR-CAS-003`| 501  | Storage backend is not yet implemented (S3/GCS stubs).    |
/// | `BAR-CAS-004`| 413  | Batch request exceeded the documented per-call cap.       |
/// | `BAR-CAS-005`| 400  | Request body did not match the documented JSON schema.    |
/// | `BAR-CAS-099`| 500  | Unclassified internal/storage I/O failure.                |
///
/// ## `BAR-AUTH-NNN` code reference
///
/// Auth-related codes share the same `ErrorBody` shape (no
/// `expected` / `actual` fields). They never appear together with a
/// `BAR-CAS-NNN` code on the same response.
///
/// | Code           | HTTP | Meaning                                                                  |
/// |----------------|------|--------------------------------------------------------------------------|
/// | `BAR-AUTH-001` | 401  | Request lacked valid bearer / mTLS credentials.                          |
/// | `BAR-AUTH-002` | 403  | Credentials were valid but the principal isn't authorised (v0.2 RBAC).   |
/// | `BAR-AUTH-005` | —    | Startup error: non-loopback bind requires `bearer` or `mtls` configured. |
/// | `BAR-AUTH-099` | 500  | Internal auth-layer failure (verifier panic, extension extraction bug).  |
///
/// `BAR-AUTH-005` never travels over the wire — it's the startup
/// error a misconfigured server surfaces from
/// [`crate::config::ServerConfig::validate`]. Codes 003/004 are
/// reserved for future use (`004` is the planned "token expired"
/// once we add expiry).
///
/// Codes are append-only: new failure modes get a fresh number, never
/// reuse a retired one.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    /// Stable `BAR-CAS-NNN` identifier. See the table above.
    pub code: &'static str,
    /// Human-readable message. Safe to surface in CLI output; does not
    /// embed secrets or filesystem paths.
    pub message: String,
    /// Digest the caller claimed (for `BAR-CAS-001`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// Digest the bytes actually hashed to (for `BAR-CAS-001`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
}

impl ErrorBody {
    /// Construct an `ErrorBody` with just a code + message; no
    /// `expected` / `actual` fields. Most call sites use this.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            expected: None,
            actual: None,
        }
    }

    /// Construct an `ErrorBody` for a digest-mismatch (`BAR-CAS-001`),
    /// populated with the expected + actual hex digests.
    pub fn digest_mismatch(expected: &str, actual: &str) -> Self {
        Self {
            code: "BAR-CAS-001",
            message: "digest mismatch".to_string(),
            expected: Some(expected.to_string()),
            actual: Some(actual.to_string()),
        }
    }
}

impl StorageError {
    /// Map a storage failure to the `(status, body)` pair the HTTP
    /// handlers return. Kept as an inherent method so callers that
    /// want to attach extra headers (e.g. echoing `X-Barista-Digest`
    /// on a successful put) can compose without going through
    /// `IntoResponse`.
    pub fn to_http(&self) -> (StatusCode, ErrorBody) {
        match self {
            StorageError::DigestMismatch { expected, actual } => (
                StatusCode::BAD_REQUEST,
                ErrorBody::digest_mismatch(&expected.to_hex(), &actual.to_hex()),
            ),
            StorageError::InvalidDigest { reason } => (
                StatusCode::BAD_REQUEST,
                ErrorBody::new("BAR-CAS-002", format!("invalid digest: {reason}")),
            ),
            StorageError::NotImplemented { backend } => (
                StatusCode::NOT_IMPLEMENTED,
                ErrorBody::new(
                    "BAR-CAS-003",
                    format!("storage backend {backend} is not yet implemented"),
                ),
            ),
            StorageError::Io(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorBody::new("BAR-CAS-099", format!("storage I/O error: {e}")),
            ),
            StorageError::Other { context } => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorBody::new("BAR-CAS-099", format!("storage error: {context}")),
            ),
        }
    }
}

impl IntoResponse for StorageError {
    fn into_response(self) -> Response {
        let (status, body) = self.to_http();
        (status, Json(body)).into_response()
    }
}

impl IntoResponse for RoasteryError {
    fn into_response(self) -> Response {
        match self {
            RoasteryError::Storage(s) => s.into_response(),
            other => {
                let body =
                    ErrorBody::new("BAR-CAS-099", format!("internal server error: {other}"));
                (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
            }
        }
    }
}
