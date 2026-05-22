// SPDX-License-Identifier: MIT OR Apache-2.0

//! SHA-256 content digest — client-side mirror of the type the
//! roastery server uses internally.
//!
//! This newtype is intentionally a sibling of the server crate's
//! `Digest` rather than a re-export: the client library never
//! depends on the server crate at runtime, so the type is defined
//! here with the same semantics (32 raw bytes; lowercase-hex text
//! form; uppercase rejected) and validates the same canonical wire
//! format the server emits.
//!
//! The canonical text form is the 64-character lowercase hex string
//! that appears in the `sha256:<hex>` identifier on every
//! `X-Barista-Digest` header and on every entry in a
//! `/v1/cas/missing` response. The `from_hex` parser accepts only
//! that form; clients that need to handle the prefixed `sha256:`
//! shape should strip the prefix first (the public API surface does
//! this internally when it parses server responses).

use std::fmt;

use sha2::{Digest as _, Sha256};

use crate::error::ClientError;

/// SHA-256 content digest.
///
/// Wraps the raw 32-byte hash. Display / [`Self::to_hex`] emit the
/// canonical 64-character lowercase hex form; [`Self::from_hex`]
/// parses it. The type is `Copy` so it travels by value through the
/// public API surface.
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

    /// Parse the canonical 64-character lowercase hex form.
    ///
    /// Rejects:
    ///
    /// - any string whose length is not exactly 64 characters;
    /// - any character outside `[0-9a-f]` (uppercase hex is rejected
    ///   on purpose — the canonical form is lowercase, and accepting
    ///   the uppercase form would let the same logical blob serialise
    ///   two different ways on the wire).
    pub fn from_hex(s: &str) -> Result<Self, ClientError> {
        if s.len() != Self::HEX_LEN {
            return Err(ClientError::InvalidDigest {
                reason: format!("expected {} hex chars, got {}", Self::HEX_LEN, s.len()),
            });
        }
        if !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(ClientError::InvalidDigest {
                reason: "digest must be lowercase hex [0-9a-f]".to_string(),
            });
        }
        let mut out = [0u8; 32];
        hex::decode_to_slice(s, &mut out).map_err(|e| ClientError::InvalidDigest {
            reason: format!("hex decode failed: {e}"),
        })?;
        Ok(Self(out))
    }

    /// Render the canonical lowercase hex form.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Compute the SHA-256 digest of an in-memory byte slice.
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
        write!(f, "Digest({})", self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const SAMPLE_HEX: &str = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    #[test]
    fn from_hex_round_trip() {
        let d = Digest::from_hex(SAMPLE_HEX).unwrap();
        assert_eq!(d.to_hex(), SAMPLE_HEX);
        assert_eq!(d.to_string(), SAMPLE_HEX);
    }

    #[test]
    fn from_hex_rejects_wrong_length() {
        // 63 chars
        let short = &SAMPLE_HEX[..63];
        let err = Digest::from_hex(short).unwrap_err();
        assert!(matches!(err, ClientError::InvalidDigest { .. }));
        // 65 chars
        let long = format!("{SAMPLE_HEX}a");
        let err = Digest::from_hex(&long).unwrap_err();
        assert!(matches!(err, ClientError::InvalidDigest { .. }));
    }

    #[test]
    fn from_hex_rejects_uppercase() {
        let upper = SAMPLE_HEX.to_uppercase();
        let err = Digest::from_hex(&upper).unwrap_err();
        assert!(matches!(err, ClientError::InvalidDigest { .. }));
    }

    #[test]
    fn from_hex_rejects_non_hex() {
        let bad = "z".repeat(64);
        let err = Digest::from_hex(&bad).unwrap_err();
        assert!(matches!(err, ClientError::InvalidDigest { .. }));
    }

    #[test]
    fn of_bytes_matches_known_vector() {
        // SHA-256("hello") — well-known test vector.
        let d = Digest::of_bytes(b"hello");
        assert_eq!(
            d.to_hex(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn debug_format_includes_hex() {
        let d = Digest::from_hex(SAMPLE_HEX).unwrap();
        let s = format!("{d:?}");
        assert!(s.contains(SAMPLE_HEX));
        assert!(s.starts_with("Digest("));
    }
}
