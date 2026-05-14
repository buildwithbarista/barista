//! Integration tests for `cmd::maven_vocab`.
//!
//! These exercise the structured error surface for every
//! Maven-vocabulary command. They drive `cmd::maven_vocab::render`
//! directly so we capture the rendered string without spawning a
//! subprocess; that keeps the tests hermetic and lets insta own
//! the snapshots of the formatted output.
//!
//! Project-root behavior is exercised against tempdirs: one with
//! a `pom.xml` (so the resolver succeeds and the message names
//! the project), and one without (so the resolver fails and the
//! message points the user at `--help` instead).

use std::fs;
use std::path::Path;

use barista_cli::cli::{Cli, GlobalFlags, MavenVocabArgs};
use barista_cli::cmd::{MavenPhase, maven_vocab};
use clap::Parser;
use tempfile::tempdir;

/// Build a `GlobalFlags` with everything defaulted to its
/// not-passed value. We rely on clap's defaults rather than
/// hand-writing the struct so a future flag addition doesn't
/// silently break this test file.
fn empty_globals() -> GlobalFlags {
    // Parsing a single `clean` invocation gives us a fully
    // populated `GlobalFlags` with every flag at its default.
    let cli = Cli::try_parse_from(["barista", "clean"]).expect("parse clean");
    cli.global
}

/// Build a `GlobalFlags` whose `--root` points at `dir`.
fn globals_with_root(dir: &Path) -> GlobalFlags {
    let cli = Cli::try_parse_from(["barista", "--root", dir.to_str().unwrap(), "clean"])
        .expect("parse clean with root");
    cli.global
}

/// Empty `MavenVocabArgs` (no pass-through).
fn no_args() -> MavenVocabArgs {
    MavenVocabArgs { args: vec![] }
}

/// `MavenVocabArgs` with the given pass-through args.
fn args(v: &[&str]) -> MavenVocabArgs {
    MavenVocabArgs {
        args: v.iter().map(|s| (*s).to_string()).collect(),
    }
}

/// Write an empty `pom.xml` into `dir`.
fn touch_pom(dir: &Path) {
    fs::write(dir.join("pom.xml"), b"<project/>").unwrap();
}

// --- exit code + "no project" branch ------------------------------

#[test]
fn clean_no_project_returns_exit_2() {
    // CWD here is wherever cargo runs the test from. We force
    // the no-project branch by pointing `--root` at a tempdir
    // with no pom.xml, which makes the resolver error out.
    let td = tempdir().unwrap();
    let g = globals_with_root(td.path());
    let code = maven_vocab::run(&g, MavenPhase::Clean, &no_args());
    assert_eq!(code, 2);

    let out = maven_vocab::render(&g, MavenPhase::Clean, &no_args());
    assert!(
        out.contains("no pom.xml found"),
        "expected 'no pom.xml found' detail, got:\n{out}"
    );
    assert!(out.starts_with("barista: `clean` is not yet executable."));
}

// --- in-project branch: message names the project root -------------

#[test]
fn clean_in_project_dir_names_project_root() {
    let td = tempdir().unwrap();
    touch_pom(td.path());
    let g = globals_with_root(td.path());

    let out = maven_vocab::render(&g, MavenPhase::Clean, &no_args());
    let expected_line = format!("project: {}", td.path().display());
    assert!(
        out.contains(&expected_line),
        "expected '{expected_line}' in output, got:\n{out}"
    );
    // No-args sentinel is shown when nothing was passed.
    assert!(
        out.contains("args:    (none)"),
        "expected '(none)' args line, got:\n{out}"
    );
}

// --- args passthrough --------------------------------------------------

#[test]
fn compile_includes_single_arg_in_message() {
    let td = tempdir().unwrap();
    touch_pom(td.path());
    let g = globals_with_root(td.path());

    let out = maven_vocab::render(&g, MavenPhase::Compile, &args(&["-DskipTests"]));
    assert!(
        out.contains("args:    `-DskipTests`"),
        "expected backtick-wrapped arg, got:\n{out}"
    );
    // Fallback suggestion echoes the args too.
    assert!(
        out.contains("`mvn compile -DskipTests`"),
        "expected fallback w/ arg, got:\n{out}"
    );
}

#[test]
fn test_includes_multi_arg_passthrough() {
    let td = tempdir().unwrap();
    touch_pom(td.path());
    let g = globals_with_root(td.path());

    let passed = ["-Dprop=value", "-DskipTests=false"];
    let out = maven_vocab::render(&g, MavenPhase::Test, &args(&passed));
    assert!(
        out.contains("args:    `-Dprop=value` `-DskipTests=false`"),
        "expected multi-arg passthrough, got:\n{out}"
    );
    assert!(
        out.contains("`mvn test -Dprop=value -DskipTests=false`"),
        "expected fallback w/ multi-args, got:\n{out}"
    );
}

// --- per-phase smoke: each phase mentions its own name and `mvn <phase>` -

fn assert_phase_message_shape(phase: MavenPhase, name: &str) {
    let td = tempdir().unwrap();
    touch_pom(td.path());
    let g = globals_with_root(td.path());

    let out = maven_vocab::render(&g, phase, &no_args());
    assert!(
        out.contains(&format!("`{name}` is not yet executable.")),
        "phase {name} missing headline, got:\n{out}"
    );
    assert!(
        out.contains(&format!("phase:   {name}\n")),
        "phase {name} missing phase line, got:\n{out}"
    );
    assert!(
        out.contains(&format!("`mvn {name}`")),
        "phase {name} missing mvn fallback, got:\n{out}"
    );
}

#[test]
fn package_message_shape() {
    assert_phase_message_shape(MavenPhase::Package, "package");
}

#[test]
fn verify_message_shape() {
    assert_phase_message_shape(MavenPhase::Verify, "verify");
}

#[test]
fn install_message_shape() {
    assert_phase_message_shape(MavenPhase::Install, "install");
}

#[test]
fn deploy_message_shape() {
    assert_phase_message_shape(MavenPhase::Deploy, "deploy");
}

#[test]
fn site_message_shape() {
    assert_phase_message_shape(MavenPhase::Site, "site");
}

// --- daemon language + no internal milestone IDs -----------------------

#[test]
fn message_names_barback_daemon_without_internal_ids() {
    // The error must mention the barback daemon and "subsequent
    // milestone" — but must NOT leak internal milestone IDs
    // ("Phase 4", "M3.1", etc.) into public-facing output.
    let g = empty_globals();
    let out = maven_vocab::render(&g, MavenPhase::Compile, &no_args());

    assert!(
        out.contains("barback daemon"),
        "expected 'barback daemon' in:\n{out}"
    );
    assert!(
        out.contains("subsequent milestone"),
        "expected 'subsequent milestone' in:\n{out}"
    );
    assert!(
        !out.contains("Phase 4"),
        "must not leak 'Phase 4' into:\n{out}"
    );
    assert!(!out.contains("M3.1"), "must not leak 'M3.1' into:\n{out}");
}

// --- snapshots ----------------------------------------------------------
//
// Snapshot the rendered output for two representative cases so
// the orchestrator's tone review has a stable artifact to point
// at. We strip the tempdir path from the output before snapshotting
// because tempdirs aren't stable across runs.

fn redact_project_line(s: &str) -> String {
    s.lines()
        .map(|l| {
            if l.starts_with("project: ") {
                "project: <REDACTED>".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn snapshot_compile_no_args() {
    let td = tempdir().unwrap();
    touch_pom(td.path());
    let g = globals_with_root(td.path());
    let out = maven_vocab::render(&g, MavenPhase::Compile, &no_args());
    insta::assert_snapshot!("compile_no_args", redact_project_line(&out));
}

#[test]
fn snapshot_test_skip_tests() {
    let td = tempdir().unwrap();
    touch_pom(td.path());
    let g = globals_with_root(td.path());
    let out = maven_vocab::render(&g, MavenPhase::Test, &args(&["-DskipTests"]));
    insta::assert_snapshot!("test_skip_tests", redact_project_line(&out));
}
