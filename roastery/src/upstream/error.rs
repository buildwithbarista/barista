//! Errors surfaced by the upstream-on-miss fetch path.
//!
//! `UpstreamError` is the dedicated error type for the
//! [`super::UpstreamFetcher`] codepath. It is deliberately separate
//! from [`crate::error::StorageError`] so the GET handler can pattern-
//! match on "upstream said no" vs "local storage said no" vs "the
//! coords the client gave us are malformed".
//!
//! The handler maps the variants to HTTP responses:
//!
//! - [`UpstreamError::InvalidCoords`] → `400 BAR-CACHE-008`. The
//!   client passed a `X-Barista-Coords` header that didn't parse; this
//!   is a client bug, not a server- or upstream-side failure.
//! - [`UpstreamError::NotConfigured`] → never travels over the wire;
//!   it's an internal sentinel the handler turns into "no upstream
//!   fetch attempted, return 404".
//! - [`UpstreamError::AllReposFailed`] → never returned to a caller as
//!   an `Err`; the fetcher folds it into `Ok(None)` so the handler
//!   surfaces a plain 404.
//! - [`UpstreamError::Io`] → wrapped at the boundary; the fetcher
//!   logs it and moves on to the next upstream rather than aborting
//!   the whole request, so this variant never reaches the handler.
//! - [`UpstreamError::DigestMismatch`] → logged + metric incremented
//!   inside the fetcher; the request continues against the next repo
//!   in the list, so this also doesn't reach the handler.

use thiserror::Error;

use crate::storage::Digest;

/// Errors raised by the upstream-on-miss fetch path.
#[derive(Debug, Error)]
pub enum UpstreamError {
    /// The `X-Barista-Coords` header was malformed (wrong number of
    /// components, empty segment, illegal characters).
    #[error("invalid X-Barista-Coords: {reason}")]
    InvalidCoords {
        /// Human-readable explanation suitable for surfacing in the
        /// error body.
        reason: String,
    },

    /// The fetcher was asked to run but no upstream is configured.
    /// Internal sentinel — never reaches the HTTP surface. The handler
    /// only invokes the fetcher when an [`UpstreamFetcher`] is present
    /// on `AppState`, so this is defensive only.
    ///
    /// [`UpstreamFetcher`]: super::UpstreamFetcher
    #[error("no upstream repositories configured")]
    NotConfigured,

    /// Every configured upstream repository was tried and none served
    /// the artifact. The fetcher converts this into `Ok(None)` before
    /// returning, so this variant is internal only.
    #[error("all configured upstream repositories failed to serve the artifact")]
    AllReposFailed,

    /// An underlying I/O / HTTP-client error wrapped from `reqwest`.
    /// The fetcher logs + moves on; doesn't bubble to the handler.
    #[error("upstream I/O error: {source}")]
    Io {
        /// The wrapped `reqwest` error.
        #[from]
        source: reqwest::Error,
    },

    /// A local CAS write failed while the fetcher was attempting to
    /// stage an upstream blob. Logged + the fetcher continues with
    /// the next repository, on the assumption a transient local I/O
    /// blip might clear by the time the next attempt runs.
    #[error("local CAS write failed during upstream fetch: {0}")]
    Storage(#[from] crate::error::StorageError),

    /// An upstream served bytes whose SHA-256 didn't match the
    /// requested digest. Logged + metric incremented; the fetcher
    /// continues to the next repository in the list.
    #[error("upstream {repo} served bytes with digest {actual} (expected {expected})")]
    DigestMismatch {
        /// The bare host of the upstream URL that misbehaved.
        repo: String,
        /// The digest the client asked for.
        expected: Digest,
        /// The digest the bytes actually hashed to.
        actual: Digest,
    },
}
