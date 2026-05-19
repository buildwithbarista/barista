//! Content-addressed storage backend abstraction.
//!
//! Every artifact in the roastery is identified by the SHA-256 digest
//! of its bytes. There is no separate metadata index: the digest IS
//! the cache key. This module defines:
//!
//! - [`Digest`] — a 32-byte SHA-256 newtype with a strict lowercase
//!   hex parser. Cheap (`Copy`), hashable, and the unit of identity
//!   for every storage call.
//! - [`Stat`] — size + digest of a stored blob.
//! - [`Cas`] — the async trait every backend implements. Methods are
//!   streaming (`tokio::io::AsyncRead`) so a fat JAR doesn't have to
//!   be loaded into memory just to be stored or served.
//!
//! ## Backends
//!
//! - [`FsCas`] — filesystem-backed, the production v0.1 default. Lays
//!   blobs out under `<root>/cas/<first-2-hex>/<remaining-62-hex>`
//!   (loose-object style, the same convention git and bazel-remote
//!   use), with atomic writes via `<root>/tmp/<random>.tmp` →
//!   `rename`.
//! - [`S3Cas`] — type-only stub. Trait methods return
//!   [`StorageError::NotImplemented`]. Scheduled for v0.2.
//! - [`GcsCas`] — type-only stub. Same shape as `S3Cas`. Scheduled
//!   for v0.2.
//!
//! ## Atomicity guarantee
//!
//! `put` is atomic: a concurrent `get` either sees the complete blob
//! or [`StorageError::Io`] / `Ok(None)` (= not found). Partial writes
//! are never visible. The filesystem backend achieves this via a
//! same-filesystem `rename` from `<root>/tmp/` into `<root>/cas/`.
//!
//! ## Verification
//!
//! `put` hashes bytes as they stream into the staging file. If the
//! computed digest disagrees with the digest the caller claimed, the
//! staged file is dropped and [`StorageError::DigestMismatch`] is
//! returned. The store never accepts a poisoned blob.

use std::fmt;

use async_trait::async_trait;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncRead;

use crate::error::StorageError;

pub mod fs;
pub mod gcs;
pub mod s3;

pub use fs::FsCas;
pub use gcs::GcsCas;
pub use s3::S3Cas;

/// `Result` alias for storage operations.
pub type Result<T> = std::result::Result<T, StorageError>;

/// SHA-256 content digest.
///
/// Represented as the raw 32-byte hash. Display + `to_hex` emit the
/// 64-character lowercase hex form used in URLs and on-disk paths.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Number of bytes in a SHA-256 digest.
    pub const SIZE: usize = 32;

    /// Number of hex characters in the canonical text form.
    pub const HEX_LEN: usize = 64;

    /// Wrap an existing 32-byte SHA-256 hash.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw 32-byte hash.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Parse the canonical lowercase hex form.
    ///
    /// Rejects:
    /// - any string whose length is not exactly 64 characters;
    /// - any character outside `[0-9a-f]` (uppercase hex is rejected
    ///   on purpose — the canonical form is lowercase, accepting
    ///   uppercase would let the same logical blob have two distinct
    ///   on-disk paths).
    pub fn from_hex(s: &str) -> Result<Self> {
        if s.len() != Self::HEX_LEN {
            return Err(StorageError::InvalidDigest {
                reason: format!(
                    "expected {} hex chars, got {}",
                    Self::HEX_LEN,
                    s.len()
                ),
            });
        }
        if !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(StorageError::InvalidDigest {
                reason: "digest must be lowercase hex [0-9a-f]".to_string(),
            });
        }
        let mut out = [0u8; 32];
        hex::decode_to_slice(s, &mut out).map_err(|e| StorageError::InvalidDigest {
            reason: format!("hex decode failed: {e}"),
        })?;
        Ok(Self(out))
    }

    /// Render the canonical lowercase hex form.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Compute the SHA-256 digest of an in-memory byte slice. Useful
    /// for tests that know the bytes up front.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let out = hasher.finalize();
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&out);
        Self(buf)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Render digests as `Digest(<hex>)` so logs stay readable.
        write!(f, "Digest({})", self.to_hex())
    }
}

/// Metadata for a stored blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// Size of the blob in bytes.
    pub size: u64,
    /// SHA-256 digest of the blob (also its cache key).
    pub digest: Digest,
}

/// Boxed streaming reader returned by [`Cas::get`]. Kept as a type
/// alias so trait signatures stay readable.
pub type CasReader = Box<dyn AsyncRead + Send + Unpin>;

/// Content-addressed storage backend.
///
/// All methods are async. The trait is object-safe (via
/// `#[async_trait]`) so `Arc<dyn Cas>` can be embedded in shared
/// application state and the same router/handler code can drive
/// different backends in tests vs production.
#[async_trait]
pub trait Cas: Send + Sync + 'static {
    /// Return the size + digest of the blob identified by `digest`,
    /// or `Ok(None)` if the blob is not in the store.
    async fn stat(&self, digest: Digest) -> Result<Option<Stat>>;

    /// Stream the blob identified by `digest` back to the caller, or
    /// return `Ok(None)` if the blob is not in the store.
    ///
    /// The returned reader is a boxed `AsyncRead`; callers drive it
    /// with `tokio::io::AsyncReadExt`.
    async fn get(&self, digest: Digest) -> Result<Option<CasReader>>;

    /// Stream `source` into the store, verifying its hash matches
    /// `expected_digest`. On success, returns the [`Stat`] for the
    /// newly stored (or already-present) blob.
    ///
    /// Atomicity: a concurrent `get` either sees the complete blob or
    /// `Ok(None)`. On a digest mismatch the partial write is
    /// discarded and [`StorageError::DigestMismatch`] is returned.
    /// If the digest already exists in the store the existing entry
    /// is kept; the put is treated as a no-op success.
    async fn put(
        &self,
        expected_digest: Digest,
        source: CasReader,
    ) -> Result<Stat>;

    /// Remove the blob identified by `digest`. Returns `true` if a
    /// blob was present and removed, `false` if no blob was present
    /// (idempotent — re-deleting a missing blob is not an error).
    async fn delete(&self, digest: Digest) -> Result<bool>;

    /// List the digests currently in the store, optionally filtered
    /// to those whose lowercase hex representation starts with
    /// `prefix`.
    ///
    /// Intended for tests, GC, and admin tooling — not for hot-path
    /// serving. Implementations may impose an upper bound on the
    /// number of digests returned per call; the filesystem backend
    /// caps at 10_000 entries with a `TODO` to add pagination in
    /// v0.2.
    async fn list(&self, prefix: Option<&str>) -> Result<Vec<Digest>>;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn digest_from_hex_round_trips() {
        // Pre-computed SHA-256 of "hello world".
        let hex = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        let d = Digest::from_hex(hex).unwrap();
        assert_eq!(d.to_hex(), hex);

        // Bytes round trip too.
        let bytes = *d.as_bytes();
        let d2 = Digest::from_bytes(bytes);
        assert_eq!(d, d2);
        assert_eq!(d2.to_hex(), hex);
    }

    #[test]
    fn digest_from_hex_rejects_invalid() {
        // Too short.
        let err = Digest::from_hex("abcd").unwrap_err();
        assert!(matches!(err, StorageError::InvalidDigest { .. }));

        // Too long.
        let too_long: String = std::iter::repeat_n('a', 65).collect();
        let err = Digest::from_hex(&too_long).unwrap_err();
        assert!(matches!(err, StorageError::InvalidDigest { .. }));

        // Non-hex character (G).
        let bad: String = std::iter::repeat_n('a', 63).chain(std::iter::once('G')).collect();
        let err = Digest::from_hex(&bad).unwrap_err();
        assert!(matches!(err, StorageError::InvalidDigest { .. }));

        // Uppercase hex is rejected on purpose (canonical form is
        // lowercase to keep the on-disk path unambiguous).
        let upper: String = std::iter::repeat_n('A', 64).collect();
        let err = Digest::from_hex(&upper).unwrap_err();
        assert!(matches!(err, StorageError::InvalidDigest { .. }));
    }

    #[test]
    fn digest_of_bytes_matches_known_sha256() {
        // SHA-256("hello world") == b94d27b9...
        let d = Digest::of_bytes(b"hello world");
        assert_eq!(
            d.to_hex(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn digest_display_and_debug_use_hex() {
        let d = Digest::of_bytes(b"hello world");
        let hex = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert_eq!(format!("{d}"), hex);
        assert_eq!(format!("{d:?}"), format!("Digest({hex})"));
    }
}
