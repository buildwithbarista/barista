// SPDX-License-Identifier: MIT OR Apache-2.0

//! Public error type for the roastery client.
//!
//! Every public method returns `Result<T, ClientError>`. The variants
//! are stable and pattern-matchable so callers can distinguish (for
//! example) "the server rejected my upload because the digest didn't
//! match" from "the network dropped halfway through" without parsing
//! free-form messages.

use crate::digest::Digest;

/// Failure modes the client can surface to its caller.
///
/// The variants split along caller-meaningful lines:
///
/// - [`Self::Config`] — the client was constructed with an invalid
///   configuration (e.g. `TlsConfig::PlainHttp` against an
///   `https://` base URL).
/// - [`Self::Network`] — an `reqwest` error that isn't more
///   specifically classified below (connection refused, DNS failure,
///   etc.).
/// - [`Self::Timeout`] — the per-request timeout elapsed before the
///   server produced a complete response.
/// - [`Self::Tls`] — the TLS handshake or certificate validation
///   failed. mTLS handshake failures surface here when the rustls
///   error mapping makes that obvious; some rustls errors bubble up
///   as `Network` instead — both are non-recoverable and should be
///   logged with the same urgency.
/// - [`Self::Auth`] — the server returned `401 BAR-AUTH-001`
///   (missing or invalid bearer / mTLS credentials).
/// - [`Self::ServerRejected`] — the server returned a non-2xx
///   response with a structured error body (status, `BAR-CAS-NNN`
///   code, message, optional expected/actual digests for
///   `BAR-CAS-001`).
/// - [`Self::NotFound`] — convenience variant for 404 on a GET. HEAD
///   maps 404 to `Ok(None)`; only GET surfaces 404 as an error
///   because the caller asked specifically for the bytes.
/// - [`Self::BadResponse`] — the server responded but the body
///   didn't match the documented wire shape (e.g. missing
///   `X-Barista-Digest` header on a successful GET, JSON that didn't
///   parse into the expected struct).
/// - [`Self::InvalidDigest`] — a [`Digest`] construction failed.
///   Surfaced here so callers don't have to introduce a separate
///   error type for digest parsing.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Caller supplied an invalid configuration. Surfaced from
    /// [`crate::RoasteryClient::new`] before any network I/O.
    #[error("invalid client configuration: {reason}")]
    Config {
        /// Human-readable description of what went wrong.
        reason: String,
    },

    /// Lower-level transport failure (connection refused, DNS error,
    /// connection reset, HTTP/2 stream error, etc.). Wraps the
    /// underlying `reqwest::Error` so callers can inspect it.
    #[error("network error: {source}")]
    Network {
        /// The underlying `reqwest` error.
        #[from]
        source: reqwest::Error,
    },

    /// The per-request timeout elapsed before the server produced a
    /// complete response. Surfaced as a distinct variant (rather
    /// than buried inside [`Self::Network`]) so callers can apply
    /// retry/backoff policy on timeouts specifically.
    #[error("request timed out")]
    Timeout,

    /// TLS handshake or certificate validation failed.
    #[error("TLS error: {reason}")]
    Tls {
        /// Description of the failure, sourced from the underlying
        /// rustls / reqwest error chain.
        reason: String,
    },

    /// The server returned 401 with `BAR-AUTH-001`. Either no
    /// credentials were sent and the route required them, or the
    /// supplied credentials were rejected.
    #[error("authentication failed ({code}): {message}")]
    Auth {
        /// Stable `BAR-AUTH-NNN` identifier (always `BAR-AUTH-001`
        /// for v0.1 of the server, but kept as a string for forward
        /// compatibility).
        code: String,
        /// Human-readable message from the server response body.
        message: String,
    },

    /// The server returned a non-2xx response with a structured
    /// error body. Carries the HTTP status, the stable `BAR-CAS-NNN`
    /// (or other prefix) code, the message, and the optional
    /// `expected` / `actual` digest fields the server populates on
    /// `BAR-CAS-001` digest mismatches.
    #[error("server rejected request (HTTP {status} / {code}): {message}")]
    ServerRejected {
        /// HTTP status code.
        status: u16,
        /// Stable error identifier (`BAR-CAS-001`, `BAR-CAS-002`,
        /// etc.).
        code: String,
        /// Human-readable message from the server.
        message: String,
        /// On `BAR-CAS-001`: the digest the caller claimed.
        expected: Option<Digest>,
        /// On `BAR-CAS-001`: the digest the bytes actually hashed
        /// to.
        actual: Option<Digest>,
    },

    /// Convenience variant: the GET target wasn't present in the
    /// server's CAS.
    ///
    /// `HEAD` (`stat_blob`) maps a 404 to `Ok(None)`; `GET`
    /// (`get_blob`) surfaces it as this error so the caller has to
    /// acknowledge the absence rather than silently dropping the
    /// "bytes weren't there" signal.
    #[error("blob not found in roastery")]
    NotFound,

    /// The server responded with a 2xx status but the body didn't
    /// match the documented wire shape. Indicates a server bug or a
    /// protocol-version skew the client doesn't know how to handle.
    #[error("malformed server response: {reason}")]
    BadResponse {
        /// What was wrong about the response.
        reason: String,
    },

    /// A [`Digest`] couldn't be parsed from text. Re-exposes the
    /// internal validator error so callers don't have to introduce
    /// a separate error type for digest construction.
    #[error("invalid digest: {reason}")]
    InvalidDigest {
        /// Why the digest was rejected.
        reason: String,
    },
}

impl ClientError {
    /// Map a `reqwest::Error` to either [`Self::Timeout`] or
    /// [`Self::Network`], depending on whether it represents a
    /// timeout.
    ///
    /// Kept as a free function (rather than relying solely on the
    /// `#[from]` `From<reqwest::Error>` impl) so the request-driving
    /// codepaths can disambiguate timeouts at the call site. The
    /// `From` impl exists too, but it always maps to `Network`,
    /// matching its caller-of-last-resort role.
    pub(crate) fn from_reqwest(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            Self::Timeout
        } else if e.is_connect() {
            // Connection refused / DNS / etc. — usually surfaces as
            // a `Network` error, but if the underlying issue is TLS
            // (which `is_connect` covers on rustls), promote it.
            let s = e.to_string().to_lowercase();
            if s.contains("certificate")
                || s.contains("tls")
                || s.contains("handshake")
                || s.contains("invalidcertificate")
                || s.contains("invalid certificate")
            {
                Self::Tls { reason: e.to_string() }
            } else {
                Self::Network { source: e }
            }
        } else {
            Self::Network { source: e }
        }
    }
}
