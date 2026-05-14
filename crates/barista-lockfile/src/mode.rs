//! Lockfile validation modes.
//!
//! On every resolve, barista recomputes the **project signature**
//! (a SHA-256 digest of the inputs that influence dependency
//! resolution — reactor modules, declared dependencies, exclusions,
//! repository configuration). That signature is stamped into the
//! lockfile when it's written, so a later run can detect "the
//! source tree changed; the lockfile is stale" without re-doing
//! resolution.
//!
//! The three validation modes:
//!
//! - [`ValidationMode::Default`]: on resolve, recompute the project
//!   signature. If it matches the on-disk lockfile, treat the
//!   lockfile as authoritative ([`ValidationOutcome::Authoritative`]).
//!   Otherwise the caller should log a warning, re-resolve, and
//!   overwrite the lockfile ([`ValidationOutcome::Stale`]).
//! - [`ValidationMode::Frozen`] (aka `--frozen` / `--locked`): on
//!   mismatch, return [`ValidationOutcome::Stale`] for the caller
//!   to surface as an error. Used in CI to gate on "lockfile in
//!   source matches the resolved state."
//! - [`ValidationMode::Update`]: ignore the on-disk lockfile and
//!   re-resolve from scratch ([`ValidationOutcome::Missing`]).
//!
//! [`validate`] returns the raw outcome; [`validate_strict`] is a
//! convenience wrapper that maps `Frozen + Stale` to an `Err` so
//! CLI callers can use a single `?` path.

use crate::schema::Lockfile;

/// How strictly the resolver should treat the on-disk lockfile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationMode {
    /// Use the lockfile when it matches the current source tree;
    /// silently refresh it when it doesn't.
    Default,
    /// Require the lockfile to match the current source tree; error
    /// on mismatch. Used by CI (`--frozen` / `--locked`).
    Frozen,
    /// Ignore the on-disk lockfile and resolve from scratch
    /// (`--update`).
    Update,
}

/// What the caller should do given the mode + on-disk lockfile +
/// freshly-computed project signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationOutcome {
    /// On-disk lockfile is present and its signature matches the
    /// current source tree. Caller should use the lockfile entries
    /// as the authoritative resolution.
    Authoritative,
    /// On-disk lockfile is present but its signature does not match.
    /// Caller behavior depends on the mode:
    ///
    /// - [`ValidationMode::Default`]: log a warning, re-resolve,
    ///   overwrite the lockfile.
    /// - [`ValidationMode::Frozen`]: error out (see
    ///   [`validate_strict`]).
    Stale {
        on_disk_signature: String,
        computed_signature: String,
    },
    /// No on-disk lockfile (or [`ValidationMode::Update`] forces
    /// re-resolution). Caller must resolve from scratch and write a
    /// fresh lockfile.
    Missing,
}

/// Decide what the caller should do based on the mode, the on-disk
/// lockfile (if any), and the freshly-computed project signature.
///
/// This function is pure: it does no I/O and never fails. To map
/// `Frozen + Stale` to an `Err` for a CLI `?` flow, use
/// [`validate_strict`].
pub fn validate(
    mode: ValidationMode,
    on_disk: Option<&Lockfile>,
    computed_signature: &str,
) -> ValidationOutcome {
    match (mode, on_disk) {
        // --update always re-resolves, regardless of what's on disk.
        (ValidationMode::Update, _) => ValidationOutcome::Missing,
        // No lockfile on disk → must resolve from scratch.
        (_, None) => ValidationOutcome::Missing,
        // Signature matches → lockfile is authoritative.
        (_, Some(lf)) if lf.meta.project_signature == computed_signature => {
            ValidationOutcome::Authoritative
        }
        // Signature mismatches → stale; caller decides what to do.
        (_, Some(lf)) => ValidationOutcome::Stale {
            on_disk_signature: lf.meta.project_signature.clone(),
            computed_signature: computed_signature.to_string(),
        },
    }
}

/// Error returned by [`validate_strict`] when `Frozen` mode meets a
/// stale lockfile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error(
        "lockfile is stale (signature mismatch): on-disk {on_disk_signature}, \
         computed {computed_signature}. Re-run resolution with --update to \
         refresh, or unset --frozen to auto-update."
    )]
    Stale {
        on_disk_signature: String,
        computed_signature: String,
    },
}

/// Convenience wrapper that maps [`ValidationOutcome`] to a
/// `Result` suitable for CLI use. `Frozen + Stale` becomes
/// `Err(ValidationError::Stale)`; every other combination is
/// `Ok(outcome)`.
pub fn validate_strict(
    mode: ValidationMode,
    on_disk: Option<&Lockfile>,
    computed_signature: &str,
) -> Result<ValidationOutcome, ValidationError> {
    let outcome = validate(mode, on_disk, computed_signature);
    match (mode, &outcome) {
        (
            ValidationMode::Frozen,
            ValidationOutcome::Stale {
                on_disk_signature,
                computed_signature,
            },
        ) => Err(ValidationError::Stale {
            on_disk_signature: on_disk_signature.clone(),
            computed_signature: computed_signature.clone(),
        }),
        _ => Ok(outcome),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Lockfile;

    fn lf(sig: &str) -> Lockfile {
        Lockfile::new(sig.to_string(), "settings-fp".to_string())
    }

    // --- validate: Default mode --------------------------------------

    #[test]
    fn validate_default_missing_when_no_lockfile() {
        let outcome = validate(ValidationMode::Default, None, "sig-a");
        assert_eq!(outcome, ValidationOutcome::Missing);
    }

    #[test]
    fn validate_default_authoritative_when_signature_matches() {
        let on_disk = lf("sig-a");
        let outcome = validate(ValidationMode::Default, Some(&on_disk), "sig-a");
        assert_eq!(outcome, ValidationOutcome::Authoritative);
    }

    #[test]
    fn validate_default_stale_when_signature_mismatches() {
        let on_disk = lf("sig-a");
        let outcome = validate(ValidationMode::Default, Some(&on_disk), "sig-b");
        assert_eq!(
            outcome,
            ValidationOutcome::Stale {
                on_disk_signature: "sig-a".into(),
                computed_signature: "sig-b".into(),
            }
        );
    }

    // --- validate: Frozen mode ---------------------------------------

    #[test]
    fn validate_frozen_missing_when_no_lockfile() {
        let outcome = validate(ValidationMode::Frozen, None, "sig-a");
        assert_eq!(outcome, ValidationOutcome::Missing);
    }

    #[test]
    fn validate_frozen_authoritative_when_signature_matches() {
        let on_disk = lf("sig-a");
        let outcome = validate(ValidationMode::Frozen, Some(&on_disk), "sig-a");
        assert_eq!(outcome, ValidationOutcome::Authoritative);
    }

    #[test]
    fn validate_frozen_stale_when_signature_mismatches() {
        let on_disk = lf("sig-a");
        let outcome = validate(ValidationMode::Frozen, Some(&on_disk), "sig-b");
        assert_eq!(
            outcome,
            ValidationOutcome::Stale {
                on_disk_signature: "sig-a".into(),
                computed_signature: "sig-b".into(),
            }
        );
    }

    // --- validate: Update mode ---------------------------------------

    #[test]
    fn validate_update_missing_when_no_lockfile() {
        let outcome = validate(ValidationMode::Update, None, "sig-a");
        assert_eq!(outcome, ValidationOutcome::Missing);
    }

    #[test]
    fn validate_update_ignores_matching_lockfile() {
        let on_disk = lf("sig-a");
        let outcome = validate(ValidationMode::Update, Some(&on_disk), "sig-a");
        assert_eq!(outcome, ValidationOutcome::Missing);
    }

    #[test]
    fn validate_update_ignores_mismatched_lockfile() {
        let on_disk = lf("sig-a");
        let outcome = validate(ValidationMode::Update, Some(&on_disk), "sig-b");
        assert_eq!(outcome, ValidationOutcome::Missing);
    }

    // --- validate_strict ---------------------------------------------

    #[test]
    fn validate_strict_frozen_stale_is_err() {
        let on_disk = lf("sig-a");
        let result = validate_strict(ValidationMode::Frozen, Some(&on_disk), "sig-b");
        match result {
            Err(ValidationError::Stale {
                on_disk_signature,
                computed_signature,
            }) => {
                assert_eq!(on_disk_signature, "sig-a");
                assert_eq!(computed_signature, "sig-b");
            }
            other => panic!("expected Err(Stale), got {other:?}"),
        }
    }

    #[test]
    fn validate_strict_default_stale_is_ok() {
        let on_disk = lf("sig-a");
        let result = validate_strict(ValidationMode::Default, Some(&on_disk), "sig-b");
        assert_eq!(
            result,
            Ok(ValidationOutcome::Stale {
                on_disk_signature: "sig-a".into(),
                computed_signature: "sig-b".into(),
            })
        );
    }

    #[test]
    fn validate_strict_frozen_match_is_ok_authoritative() {
        let on_disk = lf("sig-a");
        let result = validate_strict(ValidationMode::Frozen, Some(&on_disk), "sig-a");
        assert_eq!(result, Ok(ValidationOutcome::Authoritative));
    }

    #[test]
    fn validate_strict_update_is_always_ok_missing() {
        let on_disk = lf("sig-a");
        // Matching signature → still Missing under Update.
        let result = validate_strict(ValidationMode::Update, Some(&on_disk), "sig-a");
        assert_eq!(result, Ok(ValidationOutcome::Missing));
        // Mismatched signature → still Missing under Update.
        let result = validate_strict(ValidationMode::Update, Some(&on_disk), "sig-b");
        assert_eq!(result, Ok(ValidationOutcome::Missing));
    }

    #[test]
    fn validate_strict_default_match_is_ok_authoritative() {
        let on_disk = lf("sig-a");
        let result = validate_strict(ValidationMode::Default, Some(&on_disk), "sig-a");
        assert_eq!(result, Ok(ValidationOutcome::Authoritative));
    }

    // --- error formatting --------------------------------------------

    #[test]
    fn stale_error_message_contains_both_signatures_and_hints() {
        let err = ValidationError::Stale {
            on_disk_signature: "abc123".into(),
            computed_signature: "def456".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("abc123"), "missing on-disk signature: {msg}");
        assert!(msg.contains("def456"), "missing computed signature: {msg}");
        assert!(msg.contains("--update"), "missing --update hint: {msg}");
        assert!(msg.contains("--frozen"), "missing --frozen hint: {msg}");
    }
}
