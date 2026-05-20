// SPDX-License-Identifier: MIT OR Apache-2.0

//! Checksum verification.
//!
//! Maven repositories publish `<artifact>.sha1` and (on Central
//! since 2018) `<artifact>.sha256` sidecar files. Both contain a
//! lowercase hex digest as their first whitespace-separated token,
//! optionally followed by the artifact filename.
//!
//! Policy:
//! - **SHA-256 is authoritative.** If the artifact has a `.sha256`
//!   sidecar and it doesn't match the computed hash of the
//!   downloaded bytes, abort with [`ChecksumError::Mismatch`].
//! - **SHA-1 is advisory.** If the artifact has only a `.sha1`
//!   sidecar (no `.sha256`), SHA-1 is used; a mismatch still
//!   aborts. When both sidecars are present, SHA-256 is the
//!   authoritative choice, but a SHA-1 mismatch alongside a
//!   SHA-256 match is still surfaced as an error — conflicting
//!   sidecars are never silently accepted.
//! - **Missing sidecar is acceptable.** Some artifacts (rare on
//!   Central, common on internal repos) have neither sidecar. The
//!   bytes are accepted but [`Verification::Unverified`] is
//!   returned so the cache layer can warn or reject as policy
//!   dictates.
//!
//! The verify API is sync; async wrappers in the HTTP fetcher are
//! expected to call this after streaming the bytes into memory or
//! a tmp file.

use std::fmt;

use sha1::Sha1;
use sha2::{Digest, Sha256};

/// Hash algorithm advertised by a Maven sidecar file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Algorithm {
    Sha1,
    Sha256,
}

impl Algorithm {
    /// The sidecar filename extension (without leading dot).
    pub fn extension(&self) -> &'static str {
        match self {
            Algorithm::Sha1 => "sha1",
            Algorithm::Sha256 => "sha256",
        }
    }

    /// Number of hex characters in a digest of this algorithm.
    fn hex_len(&self) -> usize {
        match self {
            Algorithm::Sha1 => 40,
            Algorithm::Sha256 => 64,
        }
    }
}

/// A parsed expected digest, normalized to lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumExpected {
    pub algorithm: Algorithm,
    /// Lowercase hex, no whitespace or filename suffix.
    pub hex: String,
}

impl ChecksumExpected {
    /// Parse a Maven-format sidecar file's contents.
    ///
    /// Accepts:
    /// - just the hex (`"abc..."`)
    /// - hex + whitespace + filename (`"abc...  foo-1.0.jar"`)
    /// - leading/trailing whitespace and a trailing newline
    ///
    /// Lines starting with `#` are skipped defensively; Central
    /// never emits them but some mirrors do.
    pub fn parse(algorithm: Algorithm, raw: &str) -> Result<Self, ChecksumError> {
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // First whitespace-separated token is the hex digest.
            // `split_whitespace` on a non-empty trimmed line always
            // yields at least one token.
            let hex = line
                .split_whitespace()
                .next()
                .expect("non-empty line has a token");
            let expected_len = algorithm.hex_len();
            if hex.len() != expected_len {
                return Err(ChecksumError::Format {
                    detail: format!(
                        "{:?} sidecar expected {} hex chars, got {}",
                        algorithm,
                        expected_len,
                        hex.len()
                    ),
                });
            }
            if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(ChecksumError::Format {
                    detail: "sidecar contains non-hex characters".into(),
                });
            }
            return Ok(Self {
                algorithm,
                hex: hex.to_ascii_lowercase(),
            });
        }
        Err(ChecksumError::Format {
            detail: "sidecar file contained no non-empty lines".into(),
        })
    }
}

impl fmt::Display for ChecksumExpected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}={}", self.algorithm, self.hex)
    }
}

/// Errors produced by sidecar parsing and verification.
#[derive(Debug, thiserror::Error)]
pub enum ChecksumError {
    #[error("malformed checksum sidecar: {detail}")]
    Format { detail: String },
    #[error("checksum mismatch: expected {expected}, computed {computed}")]
    Mismatch {
        expected: ChecksumExpected,
        computed: String,
    },
}

/// Outcome of verifying an artifact's bytes against the available
/// sidecars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// SHA-256 sidecar was present and matched. The authoritative
    /// success case.
    Sha256Verified { hex: String },
    /// No SHA-256 sidecar was available, but the SHA-1 sidecar
    /// matched. Acceptable but not authoritative.
    Sha1Verified { hex: String },
    /// No sidecars were available. The cache may choose to accept
    /// (with a warning) or reject.
    Unverified,
}

/// Verify a byte slice against zero, one, or both sidecar
/// contents. Pass `None` for a sidecar that wasn't fetched.
///
/// Policy:
/// - SHA-256 mismatch is an immediate error.
/// - SHA-1 mismatch is an immediate error (whether or not SHA-256
///   was also supplied) — conflicting sidecars are not silently
///   accepted.
/// - Both-present-both-match → [`Verification::Sha256Verified`]
///   (the authoritative result).
/// - Only-SHA-256-present-and-matches → [`Verification::Sha256Verified`].
/// - Only-SHA-1-present-and-matches → [`Verification::Sha1Verified`].
/// - Neither → [`Verification::Unverified`].
pub fn verify(
    bytes: &[u8],
    sha256_sidecar: Option<&str>,
    sha1_sidecar: Option<&str>,
) -> Result<Verification, ChecksumError> {
    let computed_sha256 = hex_sha256(bytes);
    let computed_sha1 = hex_sha1(bytes);

    let sha256_ok = match sha256_sidecar {
        Some(raw) => {
            let expected = ChecksumExpected::parse(Algorithm::Sha256, raw)?;
            if expected.hex != computed_sha256 {
                return Err(ChecksumError::Mismatch {
                    expected,
                    computed: computed_sha256,
                });
            }
            true
        }
        None => false,
    };

    let sha1_ok = match sha1_sidecar {
        Some(raw) => {
            let expected = ChecksumExpected::parse(Algorithm::Sha1, raw)?;
            if expected.hex != computed_sha1 {
                return Err(ChecksumError::Mismatch {
                    expected,
                    computed: computed_sha1,
                });
            }
            true
        }
        None => false,
    };

    Ok(match (sha256_ok, sha1_ok) {
        (true, _) => Verification::Sha256Verified {
            hex: computed_sha256,
        },
        (false, true) => Verification::Sha1Verified { hex: computed_sha1 },
        (false, false) => Verification::Unverified,
    })
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex_encode(&digest)
}

fn hex_sha1(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Well-known digests for `b"hello"`.
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    const HELLO_SHA1: &str = "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d";

    // SHA-256 of the empty byte string (well-known constant).
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const EMPTY_SHA1: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

    // ---- ChecksumExpected::parse ----

    #[test]
    fn parse_sha256_just_hex() {
        let parsed = ChecksumExpected::parse(Algorithm::Sha256, HELLO_SHA256).unwrap();
        assert_eq!(parsed.algorithm, Algorithm::Sha256);
        assert_eq!(parsed.hex, HELLO_SHA256);
    }

    #[test]
    fn parse_sha1_just_hex() {
        let parsed = ChecksumExpected::parse(Algorithm::Sha1, HELLO_SHA1).unwrap();
        assert_eq!(parsed.algorithm, Algorithm::Sha1);
        assert_eq!(parsed.hex, HELLO_SHA1);
    }

    #[test]
    fn parse_sha256_lowercases_uppercase_input() {
        let uppercase = HELLO_SHA256.to_ascii_uppercase();
        let parsed = ChecksumExpected::parse(Algorithm::Sha256, &uppercase).unwrap();
        assert_eq!(parsed.hex, HELLO_SHA256);
    }

    #[test]
    fn parse_sha256_strips_trailing_filename() {
        let raw = format!("{HELLO_SHA256}  foo-1.0.jar");
        let parsed = ChecksumExpected::parse(Algorithm::Sha256, &raw).unwrap();
        assert_eq!(parsed.hex, HELLO_SHA256);
    }

    #[test]
    fn parse_sha256_strips_trailing_newline_and_filename() {
        let raw = format!("{HELLO_SHA256}  foo-1.0.jar\n");
        let parsed = ChecksumExpected::parse(Algorithm::Sha256, &raw).unwrap();
        assert_eq!(parsed.hex, HELLO_SHA256);
    }

    #[test]
    fn parse_sha256_strips_leading_whitespace() {
        let raw = format!("   {HELLO_SHA256}\n");
        let parsed = ChecksumExpected::parse(Algorithm::Sha256, &raw).unwrap();
        assert_eq!(parsed.hex, HELLO_SHA256);
    }

    #[test]
    fn parse_empty_input_errors() {
        let err = ChecksumExpected::parse(Algorithm::Sha256, "").unwrap_err();
        match err {
            ChecksumError::Format { detail } => assert!(detail.contains("no non-empty lines")),
            other => panic!("expected Format, got {other:?}"),
        }
    }

    #[test]
    fn parse_whitespace_only_input_errors() {
        let err = ChecksumExpected::parse(Algorithm::Sha256, "   \n\n  \t  \n").unwrap_err();
        assert!(matches!(err, ChecksumError::Format { .. }));
    }

    #[test]
    fn parse_wrong_length_errors() {
        let err = ChecksumExpected::parse(Algorithm::Sha256, "wronglen").unwrap_err();
        match err {
            ChecksumError::Format { detail } => {
                assert!(detail.contains("expected 64"), "detail was: {detail}");
            }
            other => panic!("expected Format, got {other:?}"),
        }
    }

    #[test]
    fn parse_non_hex_chars_errors() {
        // 64 chars long but contains an underscore (non-hex).
        // 64 chars long; contains an underscore (non-hex).
        let bogus = "garbage_with_underscore_64_chars_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        assert_eq!(bogus.len(), 64);
        let err = ChecksumExpected::parse(Algorithm::Sha256, bogus).unwrap_err();
        match err {
            ChecksumError::Format { detail } => assert!(detail.contains("non-hex")),
            other => panic!("expected Format, got {other:?}"),
        }
    }

    #[test]
    fn parse_sha1_rejects_sha256_length() {
        // SHA-256-shaped input given as SHA-1 should fail length check.
        let err = ChecksumExpected::parse(Algorithm::Sha1, HELLO_SHA256).unwrap_err();
        match err {
            ChecksumError::Format { detail } => {
                assert!(detail.contains("expected 40"), "detail was: {detail}");
            }
            other => panic!("expected Format, got {other:?}"),
        }
    }

    #[test]
    fn parse_sha256_rejects_sha1_length() {
        let err = ChecksumExpected::parse(Algorithm::Sha256, HELLO_SHA1).unwrap_err();
        assert!(matches!(err, ChecksumError::Format { .. }));
    }

    #[test]
    fn parse_skips_comment_lines() {
        let raw = format!("# generated by example\n# at 2025-01-01\n{HELLO_SHA256}\n");
        let parsed = ChecksumExpected::parse(Algorithm::Sha256, &raw).unwrap();
        assert_eq!(parsed.hex, HELLO_SHA256);
    }

    #[test]
    fn parse_blank_lines_before_hash_ok() {
        let raw = format!("\n\n   \n{HELLO_SHA256}\n");
        let parsed = ChecksumExpected::parse(Algorithm::Sha256, &raw).unwrap();
        assert_eq!(parsed.hex, HELLO_SHA256);
    }

    // ---- verify ----

    #[test]
    fn verify_no_sidecars_is_unverified() {
        let v = verify(b"hello", None, None).unwrap();
        assert_eq!(v, Verification::Unverified);
    }

    #[test]
    fn verify_matching_sha256_only() {
        let v = verify(b"hello", Some(HELLO_SHA256), None).unwrap();
        assert_eq!(
            v,
            Verification::Sha256Verified {
                hex: HELLO_SHA256.to_string()
            }
        );
    }

    #[test]
    fn verify_matching_both_prefers_sha256() {
        let v = verify(b"hello", Some(HELLO_SHA256), Some(HELLO_SHA1)).unwrap();
        assert_eq!(
            v,
            Verification::Sha256Verified {
                hex: HELLO_SHA256.to_string()
            }
        );
    }

    #[test]
    fn verify_matching_sha1_only() {
        let v = verify(b"hello", None, Some(HELLO_SHA1)).unwrap();
        assert_eq!(
            v,
            Verification::Sha1Verified {
                hex: HELLO_SHA1.to_string()
            }
        );
    }

    #[test]
    fn verify_wrong_sha256_errors() {
        let wrong = "0".repeat(64);
        let err = verify(b"hello", Some(&wrong), None).unwrap_err();
        match err {
            ChecksumError::Mismatch { expected, computed } => {
                assert_eq!(expected.algorithm, Algorithm::Sha256);
                assert_eq!(computed, HELLO_SHA256);
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_wrong_sha1_errors() {
        let wrong = "0".repeat(40);
        let err = verify(b"hello", None, Some(&wrong)).unwrap_err();
        match err {
            ChecksumError::Mismatch { expected, computed } => {
                assert_eq!(expected.algorithm, Algorithm::Sha1);
                assert_eq!(computed, HELLO_SHA1);
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_conflicting_sidecars_errors_on_sha1() {
        // SHA-256 matches, SHA-1 doesn't. Per policy this is a
        // mismatch, not a silent SHA-256-wins.
        let wrong_sha1 = "1".repeat(40);
        let err = verify(b"hello", Some(HELLO_SHA256), Some(&wrong_sha1)).unwrap_err();
        match err {
            ChecksumError::Mismatch { expected, .. } => {
                assert_eq!(expected.algorithm, Algorithm::Sha1);
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_reports_computed_hex_on_success() {
        let v = verify(b"hello", Some(HELLO_SHA256), None).unwrap();
        if let Verification::Sha256Verified { hex } = v {
            assert_eq!(hex, HELLO_SHA256);
        } else {
            panic!("expected Sha256Verified");
        }
    }

    #[test]
    fn verify_malformed_sidecar_propagates_format_error() {
        let err = verify(b"hello", Some("not-a-hash"), None).unwrap_err();
        assert!(matches!(err, ChecksumError::Format { .. }));
    }

    #[test]
    fn verify_empty_bytes_matches_well_known_sha256() {
        let v = verify(b"", Some(EMPTY_SHA256), None).unwrap();
        assert_eq!(
            v,
            Verification::Sha256Verified {
                hex: EMPTY_SHA256.to_string()
            }
        );
    }

    #[test]
    fn verify_empty_bytes_matches_well_known_sha1() {
        let v = verify(b"", None, Some(EMPTY_SHA1)).unwrap();
        assert_eq!(
            v,
            Verification::Sha1Verified {
                hex: EMPTY_SHA1.to_string()
            }
        );
    }

    #[test]
    fn verify_accepts_sidecar_with_filename_suffix() {
        let raw = format!("{HELLO_SHA256}  hello.txt\n");
        let v = verify(b"hello", Some(&raw), None).unwrap();
        assert!(matches!(v, Verification::Sha256Verified { .. }));
    }

    #[test]
    fn verify_accepts_uppercase_sidecar_hex() {
        let raw = HELLO_SHA256.to_ascii_uppercase();
        let v = verify(b"hello", Some(&raw), None).unwrap();
        assert!(matches!(v, Verification::Sha256Verified { .. }));
    }

    // ---- Display / misc ----

    #[test]
    fn display_includes_algorithm_and_hex() {
        let c = ChecksumExpected::parse(Algorithm::Sha256, HELLO_SHA256).unwrap();
        let s = format!("{c}");
        assert!(s.contains("Sha256"), "got: {s}");
        assert!(s.contains(HELLO_SHA256), "got: {s}");
    }

    #[test]
    fn algorithm_extension_matches_maven_layout() {
        assert_eq!(Algorithm::Sha1.extension(), "sha1");
        assert_eq!(Algorithm::Sha256.extension(), "sha256");
    }

    #[test]
    fn checksum_error_display_format() {
        let c = ChecksumExpected {
            algorithm: Algorithm::Sha256,
            hex: HELLO_SHA256.into(),
        };
        let err = ChecksumError::Mismatch {
            expected: c,
            computed: EMPTY_SHA256.into(),
        };
        let s = err.to_string();
        assert!(s.contains("mismatch"));
        assert!(s.contains(HELLO_SHA256));
        assert!(s.contains(EMPTY_SHA256));
    }

    #[test]
    fn verification_variants_are_distinct() {
        let a = Verification::Sha256Verified { hex: "x".into() };
        let b = Verification::Sha1Verified { hex: "x".into() };
        let c = Verification::Unverified;
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }
}
