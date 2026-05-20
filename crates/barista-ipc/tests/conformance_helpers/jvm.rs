// SPDX-License-Identifier: MIT OR Apache-2.0

// Test-support submodule: cross-platform JVM-spawn ceremony for the
// Rust↔Java conformance harness. Loaded via `mod conformance_helpers;`
// from the integration-test targets in `tests/`.
//
// This file is intentionally not `#[cfg(test)]`: integration tests under
// `tests/` are already compiled in test context, so the extra gate is
// redundant and would just hide the helpers from rust-analyzer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions,
    dead_code
)]

//! Maven compile + classpath resolution + `java` binary lookup, shared
//! between the UDS conformance suite (`tests/conformance.rs`, Java =
//! server) and the named-pipe conformance suite (`tests/
//! conformance_pipe.rs`, Java = client).
//!
//! The Maven ceremony is identical on both transports:
//!
//!   1. `mvn -f barback/pom.xml test-compile` once per process, to build
//!      `EchoServerCli` (UDS server) and `EchoPipeClientCli` (pipe
//!      client) under `barback/target/test-classes/`.
//!   2. `mvn -f barback/pom.xml dependency:build-classpath` once per
//!      process, cached via `OnceLock`, so subsequent JVM spawns pay no
//!      Maven startup cost.
//!   3. Resolve `java` from `$JAVA_HOME/bin/java` or fall back to PATH.
//!
//! What differs between transports lives in the sibling sub-modules:
//! `uds.rs` owns the Unix-domain-socket spawn (Java binds, Rust
//! connects); `pipe.rs` owns the named-pipe spawn (Rust binds, Java
//! connects). Both depend on this module.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Path to the workspace's `barback/` directory relative to the
/// `barista-ipc` crate. `tests/` runs with `CARGO_MANIFEST_DIR` set to
/// `crates/barista-ipc`, so `../../barback` resolves to the repo's
/// `barback/`.
const BARBACK_REL: &str = "../../barback";

/// Look up the absolute path of `barback/`. Done lazily once per
/// process because `CARGO_MANIFEST_DIR` may not be a stable canonical
/// path on macOS where `/private/var` ↔ `/var` symlinks differ between
/// `cargo test` and `mvn`.
pub fn barback_dir() -> &'static Path {
    static BARBACK_DIR: OnceLock<PathBuf> = OnceLock::new();
    BARBACK_DIR.get_or_init(|| {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let p = Path::new(manifest).join(BARBACK_REL);
        p.canonicalize().unwrap_or(p)
    })
}

/// Resolve the platform-appropriate Maven binary name.
///
/// On Windows, Maven ships as `mvn.cmd` (a batch wrapper); the bare
/// `mvn` shell script is Unix-only. Calling `Command::new("mvn")`
/// directly on Windows yields `ERROR_FILE_NOT_FOUND` even when Maven
/// is on `PATH`. We probe for `mvn.cmd` first on Windows and fall back
/// to `mvn` (in case some user installed a POSIX-shell wrapper).
fn maven_binary() -> &'static str {
    if cfg!(windows) { "mvn.cmd" } else { "mvn" }
}

/// Run `mvn test-compile` once per process. Subsequent calls are a
/// no-op. Panics on Maven failure: a busted Java build is a hard error
/// for the conformance suite, not a per-test flake.
pub fn ensure_test_classes_compiled() -> &'static () {
    static COMPILED: OnceLock<()> = OnceLock::new();
    COMPILED.get_or_init(|| {
        // We deliberately don't pass `-q` so the build's stderr is
        // visible in `cargo test -- --nocapture` invocations during
        // development; the conformance suites are `#[ignore]` by
        // default, so the verbose output isn't paid by routine
        // `cargo test` runs.
        let status = Command::new(maven_binary())
            .arg("-f")
            .arg(barback_dir().join("pom.xml"))
            .arg("-q")
            .arg("test-compile")
            .status()
            .expect("`mvn test-compile` should spawn — is Maven on PATH?");
        assert!(
            status.success(),
            "`mvn test-compile` failed (status: {status:?}). Java echo server/client cannot start.",
        );
    })
}

/// The classpath separator used by the active JVM. POSIX-shell uses
/// `:`; Windows uses `;`.
pub fn classpath_separator() -> char {
    if cfg!(windows) { ';' } else { ':' }
}

/// Resolve the Maven classpath for the echo JVM. Cached once per
/// process; the resolution is the slowest single step of the harness on
/// a warm cache (~2 s on a laptop), so amortising it matters.
///
/// On Unix we point `-Dmdep.outputFile` at `/dev/stdout`; on Windows
/// there's no `/dev/stdout`, so we write to a tempfile and read back.
pub fn maven_classpath() -> &'static str {
    static CP: OnceLock<String> = OnceLock::new();
    CP.get_or_init(|| {
        let cp = if cfg!(windows) {
            classpath_via_tempfile()
        } else {
            classpath_via_dev_stdout()
        };
        // Prepend the compiled test-classes + main classes so the
        // JVM resolves `EchoServerCli` / `EchoPipeClientCli` and the
        // generated proto types.
        let tc = barback_dir().join("target").join("test-classes");
        let mc = barback_dir().join("target").join("classes");
        let sep = classpath_separator();
        format!("{}{sep}{}{sep}{}", tc.display(), mc.display(), cp)
    })
}

/// Unix path: emit the classpath on stdout, capture and trim.
fn classpath_via_dev_stdout() -> String {
    let out = Command::new(maven_binary())
        .arg("-f")
        .arg(barback_dir().join("pom.xml"))
        .arg("-q")
        .arg("dependency:build-classpath")
        .arg("-Dmdep.outputFile=/dev/stdout")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("`mvn dependency:build-classpath` should spawn");
    assert!(
        out.status.success(),
        "`mvn dependency:build-classpath` failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    // Maven's `dependency:build-classpath` to /dev/stdout emits the
    // classpath as a single line followed by a newline; there may be
    // leading whitespace from the `[INFO]` line suppression.
    String::from_utf8(out.stdout)
        .expect("classpath must be UTF-8")
        .trim()
        .to_string()
}

/// Windows path: `/dev/stdout` doesn't exist, so write the classpath
/// to a tempfile and read it back. Slightly more I/O than the Unix
/// path but only paid once per cargo-test invocation (the `OnceLock`
/// in `maven_classpath` caches the result).
fn classpath_via_tempfile() -> String {
    let tmp = std::env::temp_dir().join(format!(
        "barista-ipc-cp-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    let status = Command::new(maven_binary())
        .arg("-f")
        .arg(barback_dir().join("pom.xml"))
        .arg("-q")
        .arg("dependency:build-classpath")
        .arg(format!("-Dmdep.outputFile={}", tmp.display()))
        .status()
        .expect("`mvn dependency:build-classpath` should spawn");
    assert!(
        status.success(),
        "`mvn dependency:build-classpath` failed (status: {status:?})",
    );
    let cp = std::fs::read_to_string(&tmp)
        .expect("classpath tempfile should be readable")
        .trim()
        .to_string();
    let _ = std::fs::remove_file(&tmp);
    cp
}

/// Locate a `java` binary. Prefer `JAVA_HOME/bin/java` when set (asdf,
/// CI's setup-java action, IntelliJ's "JDK for tests" all set this);
/// fall back to plain `java` on PATH.
pub fn java_binary() -> String {
    if let Ok(home) = std::env::var("JAVA_HOME") {
        // On Windows, the executable is `java.exe`.
        let exe = if cfg!(windows) { "java.exe" } else { "java" };
        let p = Path::new(&home).join("bin").join(exe);
        if p.exists() {
            return p.display().to_string();
        }
    }
    "java".to_string()
}
