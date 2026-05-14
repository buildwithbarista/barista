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

//! Integration tests for `barista wrapper`.
//!
//! These drive the CLI library's `dispatch` (and the lower-level
//! [`generate`] helper) directly so we exercise the same path the
//! binary does — argv parse → dispatch → exit code → on-disk file
//! tree.
//!
//! The acceptance-criterion boot test stubs the launcher's download
//! by pre-populating `~/.barista/wrapper/<version>/barista` with the
//! freshly-built `barista` binary, then runs `baristaw --version`
//! and asserts the exit code + output. The stubbed path makes the
//! test fully hermetic — no network, no GitHub, no curl.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use barista_cli::cli::{Cli, dispatch};
use barista_cli::cmd::wrapper::{
    DEFAULT_DISTRIBUTION_URL, GeneratePlan, WrapperError, current_barista_version, generate,
    render_properties,
};
use clap::Parser;
use tempfile::TempDir;

fn run_dispatch(argv: &[&str]) -> i32 {
    let cli = Cli::try_parse_from(argv).expect("parse argv");
    dispatch(cli)
}

/// Build a default-shaped plan rooted at `dir`.
fn plan(dir: &Path) -> GeneratePlan {
    GeneratePlan {
        target_dir: dir.to_path_buf(),
        version: "0.1.0-alpha.0".to_string(),
        distribution_url: DEFAULT_DISTRIBUTION_URL.to_string(),
        checksum_sha256: None,
        force: false,
    }
}

// ---- file-tree shape ---------------------------------------------------

#[test]
fn generates_expected_file_tree() {
    let td = tempfile::tempdir().unwrap();
    let outcome = generate(&plan(td.path())).expect("generate");

    let sh = td.path().join("baristaw");
    let cmd = td.path().join("baristaw.cmd");
    let props = td.path().join(".barista").join("wrapper.properties");

    assert!(sh.exists(), "baristaw should exist");
    assert!(cmd.exists(), "baristaw.cmd should exist");
    assert!(props.exists(), "wrapper.properties should exist");

    assert_eq!(
        outcome.written,
        vec![sh.clone(), cmd.clone(), props.clone()],
        "written list captures every file emitted",
    );

    // Sanity-check the launcher header so nobody ships an empty file.
    let sh_text = fs::read_to_string(&sh).unwrap();
    assert!(sh_text.starts_with("#!/usr/bin/env bash"));
    assert!(sh_text.contains("set -euo pipefail"));
}

// ---- Unix executable bit -----------------------------------------------

#[cfg(unix)]
#[test]
fn unix_launcher_is_executable_0755() {
    use std::os::unix::fs::PermissionsExt;
    let td = tempfile::tempdir().unwrap();
    generate(&plan(td.path())).expect("generate");
    let perms = fs::metadata(td.path().join("baristaw"))
        .unwrap()
        .permissions();
    assert_eq!(
        perms.mode() & 0o777,
        0o755,
        "baristaw must be world-executable (0o755)",
    );
}

// ---- TOML round-trip ----------------------------------------------------

#[test]
fn wrapper_properties_round_trips_through_toml() {
    let td = tempfile::tempdir().unwrap();
    let mut p = plan(td.path());
    p.checksum_sha256 = Some("d".repeat(64));
    generate(&p).expect("generate");
    let text = fs::read_to_string(td.path().join(".barista").join("wrapper.properties")).unwrap();
    let parsed: toml::Value = toml::from_str(&text).expect("valid TOML");
    assert_eq!(parsed["version"].as_str(), Some("0.1.0-alpha.0"));
    assert_eq!(
        parsed["distribution_url"].as_str(),
        Some(DEFAULT_DISTRIBUTION_URL)
    );
    assert_eq!(parsed["checksum_sha256"].as_str(), Some(&*"d".repeat(64)));
}

// ---- --force overwrites -------------------------------------------------

#[test]
fn force_overwrites_existing_wrapper() {
    let td = tempfile::tempdir().unwrap();
    // Pre-populate the launcher with bogus content.
    fs::write(td.path().join("baristaw"), b"old garbage").unwrap();
    fs::create_dir_all(td.path().join(".barista")).unwrap();
    fs::write(
        td.path().join(".barista").join("wrapper.properties"),
        b"version = \"0.0.0\"\n",
    )
    .unwrap();

    let mut p = plan(td.path());
    p.force = true;
    generate(&p).expect("generate --force");

    let sh = fs::read_to_string(td.path().join("baristaw")).unwrap();
    assert!(sh.contains("set -euo pipefail"), "force replaced baristaw");
    let props = fs::read_to_string(td.path().join(".barista").join("wrapper.properties")).unwrap();
    assert!(
        props.contains("version = \"0.1.0-alpha.0\""),
        "force replaced wrapper.properties (got: {props})",
    );
}

// ---- existing wrapper without --force ----------------------------------

#[test]
fn existing_wrapper_without_force_is_structured_error() {
    let td = tempfile::tempdir().unwrap();
    fs::write(td.path().join("baristaw"), b"do not stomp on me").unwrap();
    let err = generate(&plan(td.path())).expect_err("should refuse to overwrite");
    match err {
        WrapperError::AlreadyExists { path } => {
            assert_eq!(path, td.path().join("baristaw"));
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    // Bogus original content survives untouched.
    let body = fs::read_to_string(td.path().join("baristaw")).unwrap();
    assert_eq!(body, "do not stomp on me");
}

// ---- --version propagation ---------------------------------------------

#[test]
fn version_flag_propagates_into_wrapper_properties() {
    let td = tempfile::tempdir().unwrap();
    let mut p = plan(td.path());
    p.version = "9.9.9-custom".to_string();
    generate(&p).expect("generate");
    let text = fs::read_to_string(td.path().join(".barista").join("wrapper.properties")).unwrap();
    assert!(
        text.contains("version = \"9.9.9-custom\""),
        "wrapper.properties did not pick up --version (got: {text})",
    );
}

// ---- snapshot of wrapper.properties ------------------------------------

#[test]
fn wrapper_properties_snapshot() {
    let p = GeneratePlan {
        target_dir: PathBuf::from("/tmp/snap"),
        version: "0.1.0-alpha.0".to_string(),
        distribution_url: DEFAULT_DISTRIBUTION_URL.to_string(),
        checksum_sha256: None,
        force: false,
    };
    insta::assert_snapshot!("wrapper_properties_default", render_properties(&p));
}

// ---- dispatch round-trip -----------------------------------------------

#[test]
fn dispatch_runs_wrapper_with_root_override() {
    let td = tempfile::tempdir().unwrap();
    let code = run_dispatch(&["barista", "--root", td.path().to_str().unwrap(), "wrapper"]);
    assert_eq!(code, 0, "barista wrapper --root <td> should succeed");
    assert!(td.path().join("baristaw").exists());
    assert!(td.path().join("baristaw.cmd").exists());
    assert!(
        td.path()
            .join(".barista")
            .join("wrapper.properties")
            .exists()
    );
}

#[test]
fn dispatch_picks_up_version_and_checksum_flags() {
    let td = tempfile::tempdir().unwrap();
    let code = run_dispatch(&[
        "barista",
        "--root",
        td.path().to_str().unwrap(),
        "wrapper",
        "--version",
        "2.0.0",
        "--checksum",
        "deadbeef",
    ]);
    assert_eq!(code, 0);
    let text = fs::read_to_string(td.path().join(".barista").join("wrapper.properties")).unwrap();
    assert!(text.contains("version = \"2.0.0\""));
    assert!(text.contains("checksum_sha256 = \"deadbeef\""));
}

// ---- acceptance-criterion: generated baristaw boots and reports version
//
// [T] — boot test for M3.1 Task 8.
//
// We can't actually fetch a release tarball in a hermetic test, so we
// stub the cache: pre-populate `<BARISTA_USER_HOME>/wrapper/<version>/barista`
// with the freshly-built debug binary. The launcher sees an executable
// at the expected path and skips straight to `exec`. Then we invoke
// `./baristaw --version` and confirm the exit code is 0 and stdout
// contains the version string the binary itself reports.

#[cfg(unix)]
#[test]
fn generated_baristaw_version_boot_test() {
    // Locate the debug `barista` binary that cargo just built for
    // this test run. `CARGO_BIN_EXE_<name>` is set automatically for
    // integration tests when the crate has a [[bin]] target.
    let barista_bin = env!("CARGO_BIN_EXE_barista");
    assert!(
        Path::new(barista_bin).exists(),
        "cargo did not provide a barista binary at {barista_bin}",
    );

    let project = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    // Generate the wrapper into the project dir.
    let plan = GeneratePlan {
        target_dir: project.path().to_path_buf(),
        version: current_barista_version().to_string(),
        distribution_url: DEFAULT_DISTRIBUTION_URL.to_string(),
        checksum_sha256: None,
        force: false,
    };
    generate(&plan).expect("generate wrapper");

    // Pre-populate the cache so the launcher never hits the network.
    let cache_dir = home.path().join("wrapper").join(current_barista_version());
    fs::create_dir_all(&cache_dir).unwrap();
    let cached_bin = cache_dir.join("barista");
    fs::copy(barista_bin, &cached_bin).expect("stub cached barista");
    // The wrapper checks `[ -x ... ]` before exec; make sure the copy
    // preserved the exec bit. (fs::copy preserves permissions on Unix,
    // but be defensive.)
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&cached_bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&cached_bin, perms).unwrap();
    }

    // Invoke the generated wrapper.
    let output = Command::new(project.path().join("baristaw"))
        .arg("--version")
        .env("BARISTA_USER_HOME", home.path())
        // Drop PATH-sensitive vars where possible so a stray system
        // `barista` can't sneak in. The launcher only needs uname,
        // mkdir, awk, etc., which live in standard system paths.
        .env_remove("BARISTA_LOG")
        .output()
        .expect("exec baristaw");

    assert!(
        output.status.success(),
        "baristaw --version exited with {}: stdout={:?} stderr={:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(current_barista_version()),
        "expected `{}` in stdout, got: {stdout:?}",
        current_barista_version(),
    );
}
