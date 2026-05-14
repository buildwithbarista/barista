// Integration-test / example / benchmark target — workspace security
// lints are allowed here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`)
// is the documented contract for failing a test loudly. This allow block
// keeps the crate root's `#![allow(...)]` from being silently dropped by
// the separate compilation unit each test file forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Integration tests for `barista dial-in` — the onboarding wizard.
//!
//! The wizard's most important property is round-trip parseability:
//! whatever it writes must load cleanly through
//! `barista_config::load_effective_config`. These tests drive the
//! library entry point [`barista_cli::cmd::dial_in::dial_in`]
//! directly with a [`ScriptedPrompter`] so the assertion can be
//! deterministic (no real stdin involved).
//!
//! Each test isolates its filesystem via a `tempdir`. The `HOME`
//! env var is never read — the wizard takes an explicit
//! `output_path` and the loader takes an explicit `home_override`.

use std::fs;

use barista_cli::cmd::dial_in::{
    DialInError, DialInOpts, DialInReport, MAX_CONCURRENCY, ScriptedPrompter, dial_in,
};
use barista_config::{LoaderInputs, load_effective_config};
use tempfile::TempDir;

// ---- helpers ------------------------------------------------------

/// Build a `DialInOpts` writing into a fresh tempdir. Returns the
/// tempdir so the caller can keep it alive for the test's duration
/// and inspect the written file.
fn opts_in_tmp(force: bool) -> (TempDir, DialInOpts) {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("config.toml");
    let opts = DialInOpts {
        output_path: path,
        force,
    };
    (td, opts)
}

/// Run the loader against the freshly-written file with no other
/// layers. Returns `Ok` iff `barista-config` accepts it.
fn round_trip_load(written: &std::path::Path, home: &std::path::Path) {
    let (_cfg, _audit) = load_effective_config(LoaderInputs {
        user_config_path: Some(written.to_path_buf()),
        // Point the loader at the tempdir for HOME so it doesn't go
        // looking at the real `~/.m2/settings.xml` and inherit junk
        // from the dev machine.
        home_override: Some(home.to_path_buf()),
        // Also pin settings.xml to a nonexistent path inside the
        // tempdir to be extra explicit.
        settings_xml_path: Some(home.join(".m2").join("settings.xml")),
        // No project config either.
        project_config_path: Some(home.join("__no_project_barista_toml__")),
        ..Default::default()
    })
    .expect("round-trip load must succeed");
}

// ---- tests --------------------------------------------------------

/// Happy path: accept every default. The written file must exist,
/// must contain a `[network]` section, and must be round-trip
/// loadable by `barista-config`.
#[test]
fn happy_path_defaults_roundtrip() {
    let (td, opts) = opts_in_tmp(false);
    let output = opts.output_path.clone();
    let mut prompter = ScriptedPrompter::all_defaults();

    let report: DialInReport = dial_in(opts, &mut prompter).expect("wizard");

    assert_eq!(report.output_path, output);
    assert!(!report.use_roastery, "default for roastery is no");
    assert!(report.roastery_url.is_none());
    assert!(!report.strict, "default for strict is no");
    assert!(
        (1..=MAX_CONCURRENCY).contains(&report.concurrency),
        "concurrency {} must be in [1, {}]",
        report.concurrency,
        MAX_CONCURRENCY,
    );

    let body = fs::read_to_string(&output).unwrap();
    assert!(body.contains("[network]"), "expected [network]:\n{body}");
    assert!(
        body.contains("max-concurrent-connections"),
        "expected max-concurrent-connections:\n{body}"
    );

    round_trip_load(&output, td.path());
}

/// `--output <PATH>` is honored: the file appears exactly where the
/// caller asked for it, including a nested subdirectory the wizard
/// is responsible for creating.
#[test]
fn output_path_is_honored_and_creates_parent() {
    let td = TempDir::new().unwrap();
    let nested = td.path().join("nested").join("subdir").join("custom.toml");
    let opts = DialInOpts {
        output_path: nested.clone(),
        force: false,
    };
    let mut prompter = ScriptedPrompter::all_defaults();

    let report = dial_in(opts, &mut prompter).expect("wizard");

    assert_eq!(report.output_path, nested);
    assert!(nested.exists(), "wizard must create the requested path");

    round_trip_load(&nested, td.path());
}

/// Without `--force`, an existing file at the destination must not
/// be clobbered. The error must be the structured
/// `WouldOverwrite` variant so the CLI layer can render a useful
/// message.
#[test]
fn refuses_to_overwrite_without_force() {
    let (_td, opts) = opts_in_tmp(false);
    fs::write(&opts.output_path, "# pre-existing\n").unwrap();
    let pre_contents = fs::read_to_string(&opts.output_path).unwrap();

    let mut prompter = ScriptedPrompter::all_defaults();
    let err = dial_in(opts.clone(), &mut prompter).expect_err("should refuse");

    match err {
        DialInError::WouldOverwrite { path } => assert_eq!(path, opts.output_path),
        other => panic!("expected WouldOverwrite, got {other:?}"),
    }

    // The pre-existing file must be untouched.
    let after = fs::read_to_string(&opts.output_path).unwrap();
    assert_eq!(after, pre_contents);
}

/// With `--force`, an existing file at the destination is replaced
/// and the new file is round-trip-loadable.
#[test]
fn force_overwrites_existing() {
    let (td, opts) = opts_in_tmp(true);
    fs::write(
        &opts.output_path,
        "garbage = \"not valid for our schema\"\n",
    )
    .unwrap();

    let mut prompter = ScriptedPrompter::all_defaults();
    let report = dial_in(opts, &mut prompter).expect("force should succeed");

    let new_body = fs::read_to_string(&report.output_path).unwrap();
    assert!(!new_body.contains("garbage"));
    assert!(new_body.contains("[network]"));

    round_trip_load(&report.output_path, td.path());
}

/// A non-default roastery answer is captured in the report and
/// recorded in the file's comment header. The file still
/// round-trips because the URL is held in comments, not a TOML key.
#[test]
fn custom_roastery_url_roundtrips() {
    let (td, opts) = opts_in_tmp(false);

    // Answers (in prompt order):
    //   1. mirror URL  — default
    //   2. use-roastery — yes
    //   3. roastery URL — custom
    //   4. concurrency  — default
    //   5. strict       — default
    let answers = vec![
        "".to_string(),
        "yes".to_string(),
        "https://roastery.example.internal:9000".to_string(),
        "".to_string(),
        "".to_string(),
    ];
    let mut prompter = ScriptedPrompter::new(answers);

    let report = dial_in(opts, &mut prompter).expect("wizard");
    assert!(report.use_roastery);
    assert_eq!(
        report.roastery_url.as_deref(),
        Some("https://roastery.example.internal:9000"),
    );

    let body = fs::read_to_string(&report.output_path).unwrap();
    assert!(
        body.contains("roastery.example.internal:9000"),
        "expected roastery url in body:\n{body}"
    );

    round_trip_load(&report.output_path, td.path());
}

/// An over-large concurrency answer is clamped to `MAX_CONCURRENCY`.
/// This is a soft guardrail — the resolver and HTTP pool don't get
/// faster past 32, and a user who types 999 almost certainly meant
/// "as many as you can".
#[test]
fn concurrency_is_clamped() {
    let (td, opts) = opts_in_tmp(false);

    let answers = vec![
        "".to_string(),   // mirror — default
        "no".to_string(), // no roastery
        "64".to_string(), // concurrency way above MAX_CONCURRENCY
        "".to_string(),   // strict — default
    ];
    let mut prompter = ScriptedPrompter::new(answers);

    let report = dial_in(opts, &mut prompter).expect("wizard");
    assert_eq!(report.concurrency, MAX_CONCURRENCY);

    let body = fs::read_to_string(&report.output_path).unwrap();
    assert!(body.contains(&format!("max-concurrent-connections = {MAX_CONCURRENCY}")));

    round_trip_load(&report.output_path, td.path());
}

/// A garbage concurrency value is surfaced as a structured error
/// rather than silently swallowed.
#[test]
fn invalid_concurrency_errors_cleanly() {
    let (_td, opts) = opts_in_tmp(false);

    let answers = vec![
        "".to_string(),
        "no".to_string(),
        "lots".to_string(),
        "".to_string(),
    ];
    let mut prompter = ScriptedPrompter::new(answers);

    let err = dial_in(opts, &mut prompter).expect_err("bad number should fail");
    assert!(
        matches!(err, DialInError::InvalidNumber { .. }),
        "got {err:?}"
    );
}

/// Snapshot the TOML produced by a fully-defaulted run. The
/// snapshot fixes the human-readable surface so tweaks to the
/// header/comment block are caught in review rather than slipping
/// into a release. Concurrency varies by host, so we redact it
/// before snapshotting.
#[test]
fn default_run_toml_snapshot() {
    let (_td, opts) = opts_in_tmp(false);
    let mut prompter = ScriptedPrompter::all_defaults();
    let report = dial_in(opts, &mut prompter).expect("wizard");

    let body = fs::read_to_string(&report.output_path).unwrap();
    // Redact the concurrency value — it depends on the host CPU
    // count and would make the snapshot flaky.
    let redacted = body.replace(
        &format!("max-concurrent-connections = {}", report.concurrency),
        "max-concurrent-connections = <REDACTED>",
    );

    insta::assert_snapshot!("dial_in_default_run", redacted);
}
