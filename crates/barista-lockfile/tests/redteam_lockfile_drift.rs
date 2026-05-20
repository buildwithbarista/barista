//! Lockfile-drift red-team suite.
//!
//! These are adversarial tests: each crafts a *tampered* committed
//! lockfile — a mutated pinned digest/version, a corrupted/forged
//! "signature", a missing signature — and asserts that `--frozen`
//! mode (`ValidationMode::Frozen`) rejects it so the resolver does NOT
//! proceed with the tampered graph. The negative control (case 5)
//! proves a legitimately-generated lockfile passes frozen verification
//! so the defense doesn't false-positive on valid input.
//!
//! ## What `--frozen` actually enforces (discovered, stated honestly)
//!
//! The lockfile's integrity gate is the **project signature**
//! (`meta.project_signature`) — a SHA-256 over the canonicalized
//! effective POMs of the reactor (`signature::compute_signature`),
//! i.e. the resolution *inputs*, NOT a cryptographic signature over
//! the lockfile *entries*. `validate_strict(Frozen, on_disk, computed)`
//! recomputes that signature from the current source tree and compares
//! it to the one stamped in the lockfile; a mismatch is
//! `ValidationError::Stale` → the build fails loudly. So:
//!
//! - Tampering with the stamped `project_signature` itself (forged /
//!   corrupted / empty) makes it disagree with the recomputed
//!   signature → frozen REJECTS. (cases 2, 3.)
//! - Tampering with the source tree (a POM) without regenerating the
//!   lockfile moves the recomputed signature → frozen REJECTS. (the
//!   canonical "stale lockfile" drift — case 1, variant A.)
//! - Tampering with an *entry* (a pinned `sha256`/`version`) WITHOUT
//!   touching the source tree does NOT move `project_signature`, so
//!   the frozen *signature* check alone still says Authoritative.
//!   That entry-level tamper is caught not by the signature but by the
//!   downstream content-addressed re-verify when the artifact is
//!   fetched (the cache-poisoning suite proves that property). This is
//!   the documented v0.1 boundary — the lockfile is "not itself
//!   signed" (threat-model finding #3 residual). Case 1, variant B
//!   asserts that boundary HONESTLY rather than pretending the
//!   signature covers entry bytes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use barista_lockfile::{
    Lockfile, LockfileEntry, ValidationError, ValidationMode, ValidationOutcome, compute_signature,
    validate, validate_strict,
};
use barista_lockfile::signature::ReactorModule;
use barista_pom::raw::{Properties, RawPom};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Build a minimal reactor module mirroring the `signature` module's
/// own test fixtures so the computed signature is realistic.
fn module(group: &str, artifact: &str, version: &str) -> ReactorModule {
    let mut pom = RawPom {
        model_version: "4.0.0".to_string(),
        group_id: Some(group.to_string()),
        artifact_id: artifact.to_string(),
        version: Some(version.to_string()),
        packaging: "jar".to_string(),
        ..RawPom::default()
    };
    pom.properties = Properties::default();
    ReactorModule {
        group_id: group.to_string(),
        artifact_id: artifact.to_string(),
        pom,
    }
}

/// The reactor that "the source tree" represents in these tests.
fn reactor() -> Vec<ReactorModule> {
    vec![
        module("com.example", "app", "1.0.0"),
        module("com.example", "lib", "1.0.0"),
    ]
}

fn sample_entry(coords: &str, version: &str, sha256: &str) -> LockfileEntry {
    LockfileEntry {
        coords: coords.to_string(),
        version: version.to_string(),
        scope: "compile".to_string(),
        optional: false,
        sha256: sha256.to_string(),
        sha1: None,
        size_bytes: 1024,
        source_url: format!(
            "https://repo.maven.apache.org/maven2/{}/{}-{}.jar",
            coords.replace([':', '.'], "/"),
            coords.split(':').next_back().unwrap_or(""),
            version
        ),
        etag: None,
        last_modified: None,
        classifier: None,
        type_: "jar".to_string(),
        from_path: Vec::new(),
        depth: 0,
        snapshot_resolution: None,
        exclusions: Vec::new(),
    }
}

/// A legitimately-generated, internally-consistent lockfile: its
/// stamped `project_signature` is the real signature of `reactor()`,
/// and it pins a couple of resolved entries. This is the "golden"
/// committed lockfile every adversarial case mutates.
fn golden_lockfile() -> (Lockfile, String) {
    let computed = compute_signature(&reactor()).expect("compute signature");
    let mut lf = Lockfile::new(computed.clone(), "settings-fingerprint".to_string());
    lf.entries.push(sample_entry(
        "org.slf4j:slf4j-api",
        "2.0.16",
        &"ab".repeat(32),
    ));
    lf.entries.push(sample_entry(
        "com.google.guava:guava",
        "33.0.0-jre",
        &"cd".repeat(32),
    ));
    (lf, computed)
}

// ===========================================================================
// Case 1 — tampered entry under --frozen.
//
// Two honest variants, because the v0.1 mechanism distinguishes them:
//
//   A) The source tree (a POM) changed but the committed lockfile was
//      not regenerated. The recomputed project signature no longer
//      matches the stamped one → frozen REJECTS (the canonical drift).
//
//   B) An attacker mutates a pinned digest in an entry but leaves the
//      source tree untouched. project_signature is computed over the
//      POMs, not the entries, so the frozen *signature* check still
//      says Authoritative. This is the documented residual (finding
//      #3): the lockfile is not itself signed; entry tampering is
//      caught downstream by the content-addressed re-verify, not by
//      --frozen's signature gate. Asserted honestly here so the
//      boundary is explicit and CANNOT silently regress.
// ===========================================================================

#[test]
fn frozen_rejects_when_source_tree_drifted_from_lockfile() {
    let (on_disk, _committed_sig) = golden_lockfile();

    // Round-trip through TOML to simulate reading the committed file.
    let on_disk = Lockfile::from_toml(&on_disk.to_toml().unwrap()).unwrap();

    // The source tree changed: a dependency version bumped in a POM.
    // Recompute the signature from the NEW reactor.
    let mut drifted = reactor();
    drifted[0] = module("com.example", "app", "2.0.0"); // version bump
    let computed_now = compute_signature(&drifted).expect("recompute");

    let result = validate_strict(ValidationMode::Frozen, Some(&on_disk), &computed_now);
    match result {
        Err(ValidationError::Stale {
            on_disk_signature,
            computed_signature,
        }) => {
            assert_eq!(on_disk_signature, on_disk.meta.project_signature);
            assert_eq!(computed_signature, computed_now);
            assert_ne!(
                on_disk_signature, computed_signature,
                "the drift must actually have moved the signature"
            );
        }
        other => panic!("expected frozen to reject drift as Stale, got {other:?}"),
    }
}

#[test]
fn frozen_entry_tamper_without_source_change_is_documented_residual() {
    let (mut on_disk, committed_sig) = golden_lockfile();

    // ATTACK: rewrite a pinned digest to point at attacker bytes, and
    // bump a pinned version — but DO NOT touch the source tree.
    on_disk.entries[0].sha256 = "ff".repeat(32);
    on_disk.entries[1].version = "99.0.0-evil".to_string();

    // Round-trip through TOML (the attacker edits the committed file).
    let tampered = Lockfile::from_toml(&on_disk.to_toml().unwrap()).unwrap();
    assert_eq!(tampered.entries[0].sha256, "ff".repeat(32));

    // The resolver recomputes the project signature from the UNCHANGED
    // source tree — it matches the stamped one (entries don't feed the
    // signature).
    let computed_now = compute_signature(&reactor()).expect("recompute");
    assert_eq!(
        computed_now, committed_sig,
        "entry tamper must NOT change the project signature (it's over POMs)"
    );

    // DOCUMENTED v0.1 BEHAVIOR: --frozen's signature gate alone does
    // NOT catch the entry tamper — it reports Authoritative. The
    // tampered digest is instead rejected downstream when the artifact
    // is fetched and content-addressed re-verification fails (proven
    // in the cache-poisoning suite). If a future lockfile-content
    // signature lands and this flips to Stale/rejected, finding #3 and
    // this assertion must be revisited.
    let outcome = validate_strict(ValidationMode::Frozen, Some(&tampered), &computed_now)
        .expect("signature still matches, so no Stale error");
    assert_eq!(
        outcome,
        ValidationOutcome::Authoritative,
        "entry tamper is the documented residual: not caught by the signature gate"
    );
}

// ===========================================================================
// Case 2 — forged / corrupted signature under --frozen.
//
// The attacker overwrites the stamped project_signature with garbage
// (or a forged value). It can no longer agree with the recomputed
// signature → frozen REJECTS.
// ===========================================================================
#[test]
fn frozen_rejects_forged_signature() {
    let (mut on_disk, _committed) = golden_lockfile();

    // Forge the signature: a syntactically-valid-looking but wrong hex.
    on_disk.meta.project_signature = "de".repeat(32);
    let forged = Lockfile::from_toml(&on_disk.to_toml().unwrap()).unwrap();

    let computed_now = compute_signature(&reactor()).expect("recompute");
    let result = validate_strict(ValidationMode::Frozen, Some(&forged), &computed_now);
    assert!(
        matches!(result, Err(ValidationError::Stale { .. })),
        "a forged signature must be rejected under --frozen, got {result:?}"
    );
}

#[test]
fn frozen_rejects_corrupted_signature_bytes() {
    let (mut on_disk, committed) = golden_lockfile();

    // Corrupt a single nibble of the real signature — the subtle
    // tamper a hex-blob reviewer would never spot.
    let mut corrupted: Vec<char> = committed.chars().collect();
    corrupted[0] = if corrupted[0] == 'a' { 'b' } else { 'a' };
    on_disk.meta.project_signature = corrupted.into_iter().collect();
    let corrupted_lf = Lockfile::from_toml(&on_disk.to_toml().unwrap()).unwrap();

    let computed_now = compute_signature(&reactor()).expect("recompute");
    let result = validate_strict(ValidationMode::Frozen, Some(&corrupted_lf), &computed_now);
    assert!(
        matches!(result, Err(ValidationError::Stale { .. })),
        "a single-nibble corruption must be rejected under --frozen, got {result:?}"
    );
}

// ===========================================================================
// Case 3 — missing signature.
//
// Two shapes, because `meta.project_signature` is a required field:
//
//   A) The field is literally absent from the TOML → the lockfile
//      fails to PARSE at all (defense before validation even runs).
//   B) The field is present but EMPTY (an attacker blanking it to try
//      to slip past) → it parses, but an empty string can't match the
//      real recomputed signature → frozen REJECTS.
// ===========================================================================
#[test]
fn missing_signature_field_fails_to_parse() {
    // Hand-build a lockfile TOML with NO project_signature key.
    let toml = r#"
[meta]
schema_version = 1
generated_by = "barista 0.1.0-alpha.0"
generated_at = "2026-05-13T00:00:00Z"
settings_fingerprint = "0"

[[entries]]
coords = "org.slf4j:slf4j-api"
version = "2.0.16"
scope = "compile"
sha256 = "abababababababababababababababababababababababababababababababab"
size_bytes = 10
source_url = "https://example.com/a.jar"
"#;
    let err = Lockfile::from_toml(toml)
        .expect_err("a lockfile missing project_signature must not parse");
    // It's a TOML/serde error about the missing required field — the
    // exact variant is TomlParse; we just assert it did not silently
    // succeed with some default.
    let msg = format!("{err}");
    assert!(
        msg.contains("project_signature") || msg.contains("missing"),
        "expected a missing-field parse error, got: {msg}"
    );
}

#[test]
fn frozen_rejects_empty_signature() {
    let (mut on_disk, _committed) = golden_lockfile();
    on_disk.meta.project_signature = String::new();
    let blanked = Lockfile::from_toml(&on_disk.to_toml().unwrap()).unwrap();
    assert_eq!(blanked.meta.project_signature, "");

    let computed_now = compute_signature(&reactor()).expect("recompute");
    let result = validate_strict(ValidationMode::Frozen, Some(&blanked), &computed_now);
    assert!(
        matches!(result, Err(ValidationError::Stale { .. })),
        "a blanked signature must be rejected under --frozen, got {result:?}"
    );
}

// ===========================================================================
// Case 4 — drift WITHOUT --frozen is permissive by design.
//
// The SAME tampered/drifted lockfile, validated in Default mode,
// returns Stale as an OK outcome (the caller re-resolves and
// regenerates) rather than a hard error. This makes the contrast
// explicit: --frozen is the gate; the default is permissive-by-design
// for local iteration. Update mode ignores the lockfile entirely.
// ===========================================================================
#[test]
fn default_mode_treats_drift_as_refreshable_not_fatal() {
    let (on_disk, _committed) = golden_lockfile();
    let on_disk = Lockfile::from_toml(&on_disk.to_toml().unwrap()).unwrap();

    // Source tree drifted (same as case 1A).
    let mut drifted = reactor();
    drifted[1] = module("com.example", "lib", "1.5.0");
    let computed_now = compute_signature(&drifted).expect("recompute");

    // Default mode: Stale is returned as a NON-error outcome; the
    // caller logs a warning and regenerates. NOT a hard failure.
    let strict = validate_strict(ValidationMode::Default, Some(&on_disk), &computed_now)
        .expect("Default mode never errors on drift");
    assert!(
        matches!(strict, ValidationOutcome::Stale { .. }),
        "Default mode should report Stale (refreshable), got {strict:?}"
    );

    // And the raw `validate` agrees it's Stale (not Authoritative).
    let raw = validate(ValidationMode::Default, Some(&on_disk), &computed_now);
    assert!(matches!(raw, ValidationOutcome::Stale { .. }));

    // Update mode ignores the on-disk lockfile entirely → resolve from
    // scratch (Missing), regardless of drift.
    let upd = validate(ValidationMode::Update, Some(&on_disk), &computed_now);
    assert_eq!(upd, ValidationOutcome::Missing);
}

// ===========================================================================
// Case 5 — round-trip integrity (negative control).
//
// A legitimately-generated + signed lockfile, written to TOML and read
// back, passes frozen verification when validated against the signature
// recomputed from the SAME (unchanged) source tree. The defenses must
// not false-positive on valid input.
// ===========================================================================
#[test]
fn frozen_accepts_untampered_round_tripped_lockfile() {
    let (golden, committed) = golden_lockfile();

    // Write + read back exactly as a committed file would be.
    let toml = golden.to_toml().expect("serialize");
    let read_back = Lockfile::from_toml(&toml).expect("parse");
    assert_eq!(read_back, golden, "round-trip must preserve every field");

    // Recompute from the unchanged source tree → matches.
    let computed_now = compute_signature(&reactor()).expect("recompute");
    assert_eq!(computed_now, committed);

    let outcome = validate_strict(ValidationMode::Frozen, Some(&read_back), &computed_now)
        .expect("a valid lockfile must pass frozen verification");
    assert_eq!(
        outcome,
        ValidationOutcome::Authoritative,
        "untampered lockfile must validate as Authoritative under --frozen"
    );

    // Entries survived the round-trip intact (the pinned digests are
    // exactly what was generated).
    assert_eq!(read_back.entries.len(), 2);
    assert_eq!(read_back.entries[0].sha256, "ab".repeat(32));
    assert_eq!(read_back.entries[1].version, "33.0.0-jre");
}
