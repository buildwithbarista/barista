// SPDX-License-Identifier: MIT OR Apache-2.0

//! Public response types returned by [`crate::RoasteryClient`].
//!
//! Kept in their own module so the `client` module's request flow
//! stays focused on wire handling and the public API surface is
//! discoverable from one place.

use tokio::io::AsyncRead;

use crate::digest::Digest;

/// Metadata for a blob present on the roastery, as returned by
/// `HEAD /v1/cas/sha256/{digest}` and emitted alongside the body
/// stream on a successful `GET`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobStat {
    /// Size of the blob in bytes, as reported by the server's
    /// `Content-Length` response header.
    pub size: u64,
    /// SHA-256 digest of the blob — always the same value the
    /// caller passed in, but echoed back from the server's
    /// `X-Barista-Digest` header for an extra integrity check.
    pub digest: Digest,
}

/// A successful `GET /v1/cas/sha256/{digest}` response, split into
/// the parsed metadata and the streaming body.
///
/// The `body` is a boxed `AsyncRead` over the response stream; the
/// caller drives it with `tokio::io::AsyncReadExt` (or hands it to
/// `tokio::io::copy` / `tokio_util::io::ReaderStream::new` /
/// `Cas::put`). The stream lives as long as the underlying HTTP
/// response — drop the [`BlobStream`] to release the connection
/// early.
pub struct BlobStream {
    /// Parsed metadata (size + digest) from the response headers.
    pub stat: BlobStat,
    /// The blob bytes as a streaming `AsyncRead`. Reads return EOF
    /// when the server finishes streaming the body.
    pub body: Box<dyn AsyncRead + Send + Unpin>,
}

impl std::fmt::Debug for BlobStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobStream")
            .field("stat", &self.stat)
            .field("body", &"<AsyncRead>")
            .finish()
    }
}

/// Parsed body of `GET /v1/health`.
///
/// The server emits a fixed JSON document declaring the
/// barista-protocol surface is up. Clients can probe it before
/// authenticating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthResponse {
    /// Always `"ok"` on a healthy server.
    pub status: String,
    /// Always `"barista"` for the barista-protocol surface.
    pub protocol: String,
    /// Protocol version string (e.g. `"v1"`). Distinct from the
    /// server's crate version — bumped only when the wire contract
    /// changes incompatibly.
    pub version: String,
}

/// Parsed body of `GET /v1/capabilities`.
///
/// Used for client/server feature negotiation before authenticated
/// traffic begins. The `cas.max_batch_missing` field tells the
/// client the largest batch the server will accept; the client
/// should never send a larger one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitiesResponse {
    /// Always `"barista"` for the barista-protocol surface.
    pub protocol: String,
    /// Protocol version string (e.g. `"v1"`).
    pub version: String,
    /// CAS capabilities sub-document.
    pub cas: CapabilitiesCas,
    /// Storage backend sub-document.
    pub storage: CapabilitiesStorage,
}

/// CAS-specific capabilities advertised by the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitiesCas {
    /// Digest functions the server understands. v0.1 always
    /// returns `["sha256"]`.
    pub hashes: Vec<String>,
    /// Maximum number of digest entries accepted by a single
    /// `POST /v1/cas/missing` call. v0.1 servers cap at 1000.
    pub max_batch_missing: usize,
}

/// Storage backend the server is configured against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitiesStorage {
    /// Backend discriminant: `"filesystem"`, `"s3"`, or `"gcs"`.
    /// Informational only — the wire protocol is identical
    /// regardless of backend.
    pub backend: String,
}
