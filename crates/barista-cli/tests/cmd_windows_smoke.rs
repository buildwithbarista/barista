// Integration-test target — workspace security lints are allowed
// here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`) is the
// documented contract for failing a test loudly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Cross-platform sanity check for the Windows `barista verify`
//! smoke-build CI job (D19 / M4.3 T7).
//!
//! The `[T]` AC for T7 — "Windows `barista verify` smoke-build
//! green on every PR" — is gated by the `Smoke build — barista
//! verify --no-daemon` step in the `rust-windows` job of
//! `.github/workflows/ci.yml`. This test mirrors that step on
//! whichever host runs `cargo test` (typically Linux or macOS),
//! so fixture / CLI regressions surface during local dev instead
//! of waiting for a PR push to exercise the Windows runner.
//!
//! ## What this test does
//!
//! 1. Locates the on-disk fixture at
//!    `crates/barista-cli/tests/fixtures/windows-smoke/`.
//! 2. Copies it into a tempdir (so the build dirties no source
//!    tree state).
//! 3. Pins the asdf toolchain by mirroring the workspace's
//!    `.tool-versions` (same trick as `cmd_verify.rs`).
//! 4. Runs `barista verify --no-daemon --root <copy>` against the
//!    `CARGO_BIN_EXE_barista` binary.
//! 5. Asserts exit 0 and the presence of the produced `.class` +
//!    `.jar`.
//!
//! ## Skipped on hosts without `mvn`
//!
//! `--no-daemon` forks upstream `mvn`. If the host has no `mvn`
//! on `$PATH` (and no `MAVEN_HOME`), the test prints a `skipped:`
//! message and returns — same convention as
//! `cmd_no_daemon::byte_equal_compile_against_real_mvn`. The
//! Windows runner installs `mvn.cmd` via `actions/setup-java`,
//! so the CI job is never skipped there.
//!
//! ## What this test does NOT cover
//!
//! - Windows-specific path / shell quoting behaviour. Those only
//!   manifest on a real `windows-latest` runner; the CI step is
//!   what gates the AC.
//! - The barback daemon happy-path on Windows. The daemon is
//!   `#[cfg(unix)]`-gated for v0.1; Windows builds fall through
//!   to the `--no-daemon` forked-`mvn` path.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Path to the `barista` test-built binary (cargo sets
/// `CARGO_BIN_EXE_<name>` for integration tests of bin-crates).
fn barista_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_barista"))
}

/// Path to the on-disk fixture, resolved relative to this crate's
/// manifest dir (`crates/barista-cli/`).
fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("windows-smoke")
}

/// Whether the host has a usable `mvn` (or `mvn.cmd`) on `$PATH`.
fn host_has_mvn() -> bool {
    let name = if cfg!(windows) { "mvn.cmd" } else { "mvn" };
    which::which(name).is_ok()
}

/// Recursively copy `src` into `dst`. The fixture is small (one
/// pom, one Java source, one README) so a plain recursive copy is
/// fine; no need to pull in `fs_extra` just for this.
fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

/// Mirror the workspace `.tool-versions` (if present) into `dir`.
/// This is the same dance `cmd_verify.rs` performs so asdf-shim
/// users can run the test without a host-level toolchain pin.
fn pin_toolchain(dir: &Path) {
    let mut search = Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let mut content: Option<String> = None;
    while let Some(d) = search {
        let candidate = d.join(".tool-versions");
        if candidate.is_file()
            && let Ok(c) = fs::read_to_string(&candidate)
        {
            content = Some(c);
            break;
        }
        search = d.parent().map(Path::to_path_buf);
    }
    let pinned =
        content.unwrap_or_else(|| "java temurin-21.0.4+7.0.LTS\nmaven 3.9.9\n".to_string());
    fs::write(dir.join(".tool-versions"), pinned).unwrap();
}

/// Cross-platform sanity check: drive the Windows-smoke fixture
/// through `barista verify --no-daemon` on whichever host is
/// running `cargo test`. Skipped if no `mvn` is on `$PATH`.
#[test]
fn windows_smoke_fixture_builds_via_barista_verify_no_daemon() {
    if !host_has_mvn() {
        // Same convention as the other --no-daemon real-mvn tests:
        // skip cleanly rather than block contributors without a
        // Java toolchain.
        eprintln!("skipped: no `mvn` on $PATH");
        return;
    }

    let td = tempfile::tempdir().unwrap();
    let project = td.path().join("windows-smoke");
    copy_dir_recursive(&fixture_dir(), &project);
    pin_toolchain(&project);

    // `barista verify --no-daemon` short-circuits to forked `mvn
    // verify`. We pass `-q` to keep the captured stdout under the
    // 64 KiB cargo-test buffer ceiling; failures still surface
    // via the exit code + final-error log.
    // nosemgrep: barista-rust-unchecked-command-new
    // `barista_bin()` is the cargo-managed `CARGO_BIN_EXE_barista`
    // path — a trusted toolchain entry, identical pattern to the
    // existing tests under `crates/barista-cli/tests/`.
    let out = Command::new(barista_bin())
        .arg("--no-daemon")
        .arg("--root")
        .arg(&project)
        .arg("verify")
        .arg("-q")
        .output()
        .expect("spawn barista");

    assert!(
        out.status.success(),
        "barista verify --no-daemon should succeed against the windows-smoke fixture; \
         exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Sanity: target/classes/example/Hello.class exists.
    let class = project
        .join("target")
        .join("classes")
        .join("example")
        .join("Hello.class");
    assert!(
        class.is_file(),
        "expected Hello.class at {} after verify",
        class.display(),
    );

    // And the packaged JAR.
    let jar = fs::read_dir(project.join("target"))
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().map(|x| x == "jar").unwrap_or(false));
    assert!(jar.is_some(), "verify must produce a .jar in target/");
}

/// Pure-Rust sanity check that does not require `mvn` on the
/// host: the fixture's pom + source exist with the expected
/// shape. Catches accidental deletion / rename of the fixture
/// before the heavier `mvn` test would surface it.
#[test]
fn windows_smoke_fixture_layout_is_well_formed() {
    let dir = fixture_dir();
    assert!(dir.is_dir(), "fixture dir must exist at {}", dir.display(),);
    assert!(
        dir.join("pom.xml").is_file(),
        "fixture must have a pom.xml at the root",
    );
    let java = dir.join("src/main/java/example/Hello.java");
    assert!(
        java.is_file(),
        "fixture must have Hello.java at {}",
        java.display(),
    );

    let pom = fs::read_to_string(dir.join("pom.xml")).unwrap();
    assert!(
        pom.contains("<artifactId>hello</artifactId>"),
        "pom.xml lost its artifactId — fixture has drifted",
    );
    assert!(
        pom.contains("maven-compiler-plugin"),
        "pom.xml must pin maven-compiler-plugin for deterministic bytecode",
    );
}
