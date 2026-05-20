// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bearer-token verification.
//!
//! Loads a tokens file at startup, hashes every entry with SHA-256,
//! and exposes a `verify` API that returns the matching token's
//! non-secret label (used as the `Principal::Bearer.token_id`).
//!
//! ## Tokens file format
//!
//! UTF-8, one entry per line:
//!
//! ```text
//! # comments start with `#`
//! ci-runner-1:s3cret-token-value
//! ci-runner-2:another-secret
//! ```
//!
//! Each entry is `<label>:<secret>`. The label is a short non-secret
//! identifier — it shows up in logs and in the [`Principal::Bearer`]
//! variant — and the secret is the actual bearer token clients send
//! in the `Authorization` header. Lines without a `:` separator are
//! also accepted: the entire line is treated as the secret, and a
//! short SHA-256 prefix of the secret stands in for the label.
//!
//! ## Security posture
//!
//! - Plaintext token bytes never leave [`BearerVerifier::load`]. The
//!   loader hashes them as it parses, then drops the parsed string.
//!   The in-memory state stores SHA-256 digests only.
//! - Header comparison goes through [`subtle::ConstantTimeEq`] so an
//!   attacker can't recover a token byte-by-byte from response
//!   timing.
//! - Failure modes ("no Authorization header", "wrong scheme", "no
//!   match") all surface as the same `Unauthorized` outcome to the
//!   caller; finer-grained reasons are recorded in logs for the
//!   operator, never in the response body. See the `BAR-AUTH-001`
//!   row in [`crate::error`].
//!
//! ## Reload
//!
//! v0.1 loads the tokens file once at startup. A `SIGHUP`-driven
//! reload is a documented v0.2 follow-up — the wire contract doesn't
//! change, and the public API of this module is structured so a
//! reload simply replaces the `BearerVerifier` value.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::RoasteryError;

/// SHA-256 output width in bytes. Hard-coded so the hash compare
/// stays a fixed-length operation.
const HASH_LEN: usize = 32;

/// A single entry loaded from the tokens file.
///
/// The plaintext token is intentionally absent — only its SHA-256
/// digest + the non-secret label survive parsing.
#[derive(Clone)]
struct Entry {
    /// Short non-secret label surfaced as `Principal::Bearer.token_id`.
    label: String,
    /// SHA-256 of the raw token bytes.
    hash: [u8; HASH_LEN],
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Print only the label + the first 8 hex chars of the hash so
        // a stray `dbg!` or tracing call can never leak the full
        // hash, let alone the plaintext.
        f.debug_struct("Entry")
            .field("label", &self.label)
            .field("hash_prefix", &hex_prefix(&self.hash, 8))
            .finish()
    }
}

/// In-memory bearer-token verifier.
///
/// Construct with [`BearerVerifier::load`]; check requests with
/// [`BearerVerifier::verify`]. Stateless once built.
#[derive(Clone, Debug)]
pub struct BearerVerifier {
    entries: Vec<Entry>,
    /// Path the entries were loaded from, kept for diagnostics + the
    /// future reload codepath.
    source: PathBuf,
}

impl BearerVerifier {
    /// Load a tokens file from disk.
    ///
    /// Returns [`RoasteryError::Config`] if the file is missing,
    /// unreadable, or contains no usable entries (so a typo'd path
    /// or an empty file is caught at startup, not at first request).
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, RoasteryError> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).map_err(|e| {
            RoasteryError::Config(format!(
                "cannot read bearer tokens file {}: {e}",
                path.display()
            ))
        })?;
        let entries = parse(&raw);
        if entries.is_empty() {
            return Err(RoasteryError::Config(format!(
                "bearer tokens file {} contained no entries",
                path.display()
            )));
        }
        Ok(Self {
            entries,
            source: path.to_path_buf(),
        })
    }

    /// Build a verifier directly from in-memory entries. Used by
    /// tests; production loads from disk via [`Self::load`].
    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: &[(&str, &str)], source: impl Into<PathBuf>) -> Self {
        let entries = pairs
            .iter()
            .map(|(label, secret)| Entry {
                label: (*label).to_string(),
                hash: sha256_bytes(secret.as_bytes()),
            })
            .collect();
        Self {
            entries,
            source: source.into(),
        }
    }

    /// Path the verifier was loaded from. Diagnostic only.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Number of loaded entries. Tests rely on this for the
    /// hashed-not-plaintext property; not part of the public secrecy
    /// contract beyond "the count is not a secret."
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Verify an `Authorization` header value.
    ///
    /// Accepts only the exact `Bearer <token>` form (case-insensitive
    /// scheme, single space, non-empty token). Returns:
    ///
    /// - `Ok(Some(token_id))` on a successful match — the caller
    ///   wraps `token_id` in [`crate::auth::Principal::Bearer`].
    /// - `Ok(None)` if the header was syntactically a `Bearer ...`
    ///   header but the token didn't match any loaded entry.
    /// - `Err(BearerVerifyError::Malformed)` if the scheme/format
    ///   was unrecognisable.
    ///
    /// The caller is expected to surface every non-`Ok(Some(...))`
    /// outcome as the same `BAR-AUTH-001` 401 — the variant exists
    /// only so the operator-facing log can distinguish "no header /
    /// malformed scheme" from "valid form, wrong secret."
    pub fn verify(&self, header_value: &str) -> Result<Option<String>, BearerVerifyError> {
        let token = parse_bearer_header(header_value)?;
        let candidate_hash = sha256_bytes(token.as_bytes());
        for entry in &self.entries {
            // `ConstantTimeEq` returns a `Choice` that's `1` on match
            // and `0` otherwise; comparing against the explicit `1`
            // gives us a `bool`. Fixed-length (32-byte) input on both
            // sides — the loop iteration over entries is constant
            // per-attempt regardless of which entry hits, because we
            // can't early-exit without leaking which slot matched.
            if entry.hash.ct_eq(&candidate_hash).into() {
                return Ok(Some(entry.label.clone()));
            }
        }
        Ok(None)
    }
}

/// Why a bearer header couldn't be evaluated. Distinct from "no
/// match" so the operator-facing log can tell the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BearerVerifyError {
    /// Header didn't carry the `Bearer ` scheme prefix or was empty
    /// after the space.
    Malformed,
}

/// Parse a single `Authorization` header value into the bearer token
/// portion. Case-insensitive on the scheme, but a single space is
/// required between the scheme and the token (matches RFC 6750).
fn parse_bearer_header(raw: &str) -> Result<&str, BearerVerifyError> {
    let trimmed = raw.trim_start();
    let (scheme, rest) = trimmed
        .split_once(' ')
        .ok_or(BearerVerifyError::Malformed)?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Err(BearerVerifyError::Malformed);
    }
    let token = rest.trim();
    if token.is_empty() {
        return Err(BearerVerifyError::Malformed);
    }
    Ok(token)
}

/// Parse a tokens file body into the in-memory entry list.
///
/// Lines starting with `#` and blank lines are ignored. Each
/// remaining line is split at the first `:`. The portion before the
/// colon is the label; the portion after is the secret. A line
/// without a `:` is taken whole as the secret, with a short SHA-256
/// prefix substituting for the label.
fn parse(raw: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (label, secret) = match line.split_once(':') {
            Some((l, s)) => (l.trim().to_string(), s.trim()),
            None => {
                let s = line;
                let h = sha256_bytes(s.as_bytes());
                let prefix = hex_prefix(&h, 8);
                (prefix, s)
            }
        };
        if secret.is_empty() {
            continue;
        }
        out.push(Entry {
            label,
            hash: sha256_bytes(secret.as_bytes()),
        });
    }
    out
}

fn sha256_bytes(bytes: &[u8]) -> [u8; HASH_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut buf = [0u8; HASH_LEN];
    buf.copy_from_slice(&out);
    buf
}

fn hex_prefix(bytes: &[u8], chars: usize) -> String {
    let full = hex::encode(bytes);
    let take = chars.min(full.len());
    // Indexing is safe by construction: `hex::encode` only emits ASCII
    // (`0-9a-f`), so byte-indexing matches char-indexing here, and
    // `chars.min(...)` guarantees we stay in bounds. The crate's
    // no-panic lint requires we avoid `&full[..take]` without that
    // bound; the `min` above gives it.
    full.chars().take(take).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn parses_labelled_entries() {
        let raw = "ci-runner-1:secret-one\nci-runner-2:secret-two\n";
        let entries = parse(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].label, "ci-runner-1");
        assert_eq!(entries[1].label, "ci-runner-2");
        // Hashes differ on different inputs.
        assert_ne!(entries[0].hash, entries[1].hash);
    }

    #[test]
    fn parses_unlabelled_entries_with_hash_prefix_label() {
        let raw = "just-the-secret\n";
        let entries = parse(raw);
        assert_eq!(entries.len(), 1);
        // Synthesised label is an 8-char hex prefix of the secret's hash.
        assert_eq!(entries[0].label.len(), 8);
        assert!(entries[0].label.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn ignores_blanks_and_comments() {
        let raw = "
# a leading comment
ci-runner-1:secret-one

# another comment
ci-runner-2:secret-two
";
        let entries = parse(raw);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn empty_secret_is_skipped() {
        let raw = "label-only:\n";
        let entries = parse(raw);
        assert!(entries.is_empty());
    }

    #[test]
    fn verify_matches_valid_token() {
        let v = BearerVerifier::from_pairs(&[("ci", "s3cret")], "/test");
        let id = v.verify("Bearer s3cret").unwrap().unwrap();
        assert_eq!(id, "ci");
    }

    #[test]
    fn verify_rejects_wrong_token() {
        let v = BearerVerifier::from_pairs(&[("ci", "s3cret")], "/test");
        let outcome = v.verify("Bearer not-the-token").unwrap();
        assert!(outcome.is_none());
    }

    #[test]
    fn verify_accepts_lowercase_scheme() {
        let v = BearerVerifier::from_pairs(&[("ci", "s3cret")], "/test");
        let id = v.verify("bearer s3cret").unwrap().unwrap();
        assert_eq!(id, "ci");
    }

    #[test]
    fn verify_rejects_wrong_scheme() {
        let v = BearerVerifier::from_pairs(&[("ci", "s3cret")], "/test");
        let err = v.verify("Basic s3cret").unwrap_err();
        assert_eq!(err, BearerVerifyError::Malformed);
    }

    #[test]
    fn verify_rejects_missing_token() {
        let v = BearerVerifier::from_pairs(&[("ci", "s3cret")], "/test");
        let err = v.verify("Bearer ").unwrap_err();
        assert_eq!(err, BearerVerifyError::Malformed);
        let err = v.verify("Bearer").unwrap_err();
        assert_eq!(err, BearerVerifyError::Malformed);
    }

    #[test]
    fn load_returns_error_for_missing_file() {
        let err = BearerVerifier::load("/no/such/file/tokens.txt").unwrap_err();
        assert!(matches!(err, RoasteryError::Config(_)));
    }

    #[test]
    fn load_returns_error_for_empty_file() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "# nothing useful").unwrap();
        let err = BearerVerifier::load(f.path()).unwrap_err();
        assert!(matches!(err, RoasteryError::Config(_)));
    }

    #[test]
    fn load_round_trips() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "ci-1:secret-one").unwrap();
        writeln!(f, "ci-2:secret-two").unwrap();
        let v = BearerVerifier::load(f.path()).unwrap();
        assert_eq!(v.entry_count(), 2);
        assert_eq!(v.verify("Bearer secret-one").unwrap().unwrap(), "ci-1");
        assert_eq!(v.verify("Bearer secret-two").unwrap().unwrap(), "ci-2");
    }

    /// `[T]` linkage: `bearer_tokens_stored_hashed_not_plaintext`.
    ///
    /// Inspect the loaded verifier's in-memory state and assert the
    /// plaintext token bytes do NOT appear anywhere reachable
    /// (including via `Debug` formatting).
    #[test]
    fn tokens_stored_hashed_not_plaintext() {
        let secret = "this-is-the-plaintext-secret-x7y9z";
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "ci:{secret}").unwrap();
        let v = BearerVerifier::load(f.path()).unwrap();

        // 1) `Debug` formatting must not echo the secret.
        let dbg = format!("{v:?}");
        assert!(
            !dbg.contains(secret),
            "Debug output leaked the plaintext token: {dbg}"
        );

        // 2) Stored hashes must equal SHA-256(secret), not the bytes
        //    of the secret itself.
        let expected_hash = sha256_bytes(secret.as_bytes());
        assert_eq!(v.entries.len(), 1);
        assert_eq!(v.entries[0].hash, expected_hash);

        // 3) None of the hash bytes equal any contiguous substring of
        //    the secret. (SHA-256 of a non-trivial input is, with
        //    overwhelming probability, not a substring of that input;
        //    asserting it pins the property in the test.)
        let secret_bytes = secret.as_bytes();
        for window in secret_bytes.windows(v.entries[0].hash.len()) {
            assert_ne!(
                window, &v.entries[0].hash,
                "stored hash bytes happen to equal a window of the plaintext"
            );
        }
    }
}
