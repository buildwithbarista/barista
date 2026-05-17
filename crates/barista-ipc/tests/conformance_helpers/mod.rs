// Test-support submodule for the cross-language Rust↔Java conformance
// harness. Loaded via `mod conformance;` from `tests/conformance.rs`.
//
// This file is intentionally not a `#[cfg(test)]` module: integration
// tests under `tests/` are already compiled in test context, so the
// extra gate is redundant and would just hide the helpers from
// rust-analyzer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions,
    dead_code
)]
#![cfg(unix)]

//! Helpers for spawning the Java echo server as a subprocess.
//!
//! The conformance suite invokes one `EchoServerCli` JVM per test, dials
//! it over UDS, drives the round-trip, and tears down. This module owns
//! the ceremony of:
//!
//!   1. compiling `barback/src/test/java/com/bluminal/barista/barback/conformance/EchoServer*.java`
//!      once (via `mvn -f barback/pom.xml test-compile`),
//!   2. resolving the Maven test-classpath once (via
//!      `mvn -f barback/pom.xml dependency:build-classpath`),
//!   3. spawning a fresh JVM per call with a caller-supplied socket
//!      path,
//!   4. waiting for the `READY <path>` line on stdout, and
//!   5. exposing a `Drop` impl that closes stdin (triggering the
//!      child's stdin-watchdog → `Runtime.halt(0)`) and waits with a
//!      short timeout so a test panic doesn't leak the JVM.
//!
//! The mvn invocations are gated behind a `OnceLock` so subsequent
//! tests in the same `cargo test` invocation pay the Maven startup cost
//! exactly once.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

/// Path to the workspace's `barback/` directory relative to the
/// `barista-ipc` crate. `tests/` runs with `CARGO_MANIFEST_DIR` set to
/// `crates/barista-ipc`, so `../../barback` resolves to the worktree's
/// `barback/`.
const BARBACK_REL: &str = "../../barback";

/// Maximum time we wait for the Java side to print `READY <path>` on
/// stdout. 30 s accommodates a cold JVM start on a busy CI runner; in
/// practice the warm path is well under 1 s.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Look up the absolute path of `barback/`. Done lazily once per
/// process because `CARGO_MANIFEST_DIR` may not be a stable canonical
/// path on macOS where `/private/var` ↔ `/var` symlinks differ between
/// `cargo test` and `mvn`.
fn barback_dir() -> &'static Path {
    static BARBACK_DIR: OnceLock<PathBuf> = OnceLock::new();
    BARBACK_DIR.get_or_init(|| {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let p = Path::new(manifest).join(BARBACK_REL);
        p.canonicalize().unwrap_or(p)
    })
}

/// Run `mvn test-compile` once per process. Subsequent calls are a
/// no-op. Panics on Maven failure: a busted Java build is a hard error
/// for the conformance suite, not a per-test flake.
pub fn ensure_test_classes_compiled() -> &'static () {
    static COMPILED: OnceLock<()> = OnceLock::new();
    COMPILED.get_or_init(|| {
        // We deliberately don't pass `-q` so the build's stderr is
        // visible in `cargo test -- --nocapture` invocations during
        // development; the conformance suite is `#[ignore]` by default,
        // so the verbose output isn't paid by routine `cargo test`
        // runs.
        let status = Command::new("mvn")
            .arg("-f")
            .arg(barback_dir().join("pom.xml"))
            .arg("-q")
            .arg("test-compile")
            .status()
            .expect("`mvn test-compile` should spawn — is Maven on PATH?");
        assert!(
            status.success(),
            "`mvn test-compile` failed (status: {status:?}). Java echo server cannot start.",
        );
    })
}

/// Resolve the Maven classpath for the echo server JVM. Cached once per
/// process; the resolution is the slowest single step of the harness on
/// a warm cache (~2 s on a laptop), so amortising it matters.
fn maven_classpath() -> &'static str {
    static CP: OnceLock<String> = OnceLock::new();
    CP.get_or_init(|| {
        let out = Command::new("mvn")
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
        // classpath as a single line followed by a newline; there may
        // be leading whitespace from the `[INFO]` line suppression.
        let cp = String::from_utf8(out.stdout)
            .expect("classpath must be UTF-8")
            .trim()
            .to_string();
        // Prepend the compiled test-classes + main classes so the
        // JVM resolves `EchoServerCli` and the generated proto types.
        let tc = barback_dir().join("target").join("test-classes");
        let mc = barback_dir().join("target").join("classes");
        format!("{}:{}:{}", tc.display(), mc.display(), cp)
    })
}

/// Locate a `java` binary. Prefer `JAVA_HOME/bin/java` when set (asdf,
/// CI's setup-java action, IntelliJ's "JDK for tests" all set this);
/// fall back to plain `java` on PATH.
fn java_binary() -> String {
    if let Ok(home) = std::env::var("JAVA_HOME") {
        let p = Path::new(&home).join("bin").join("java");
        if p.exists() {
            return p.display().to_string();
        }
    }
    "java".to_string()
}

/// A running `EchoServerCli` subprocess + the path it bound at.
///
/// On `Drop`:
///   1. Closes the child's stdin (the watchdog thread inside the JVM
///      observes EOF and calls `Runtime.halt(0)`).
///   2. Waits up to 5 s for the process to exit; if it doesn't, kills
///      it.
///   3. Drains stderr to a side buffer the harness can fetch via
///      `stderr_log()` for debugging.
pub struct JavaEchoServer {
    socket_path: PathBuf,
    child: Option<Child>,
    stderr_drainer: Option<thread::JoinHandle<Vec<u8>>>,
}

impl JavaEchoServer {
    /// Spawn `EchoServerCli --socket <path>` and wait for its
    /// `READY <path>` line. Panics on any failure — the conformance
    /// tests are not expected to recover from a busted JVM.
    pub fn spawn(socket_path: PathBuf) -> Self {
        ensure_test_classes_compiled();
        let cp = maven_classpath();

        let mut child = Command::new(java_binary())
            .arg("-cp")
            .arg(cp)
            .arg("com.bluminal.barista.barback.conformance.EchoServerCli")
            .arg("--socket")
            .arg(&socket_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("`java EchoServerCli` should spawn");

        let stdout = child.stdout.take().expect("child stdout is piped");
        let stderr = child.stderr.take().expect("child stderr is piped");

        // Drain stderr on a background thread into a Vec<u8> we can
        // dump if a test fails. Without this, the JVM blocks on a
        // full stderr pipe when it logs a stack trace.
        let stderr_drainer = thread::spawn(move || drain_to_vec(stderr));

        // Read stdout line-by-line on the foreground thread until we
        // see READY (or the timeout elapses).
        let socket_path_clone = socket_path.clone();
        wait_for_ready(stdout, &socket_path_clone);

        Self {
            socket_path,
            child: Some(child),
            stderr_drainer: Some(stderr_drainer),
        }
    }

    /// The socket path the Java server is listening on.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Drain the child's accumulated stderr buffer. Useful for debug
    /// output when a test panics — call this from a `tokio::test`'s
    /// failure path to see the JVM's perspective on what went wrong.
    pub fn stderr_log(&mut self) -> Vec<u8> {
        if let Some(handle) = self.stderr_drainer.take() {
            handle.join().unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Cleanly shut down the JVM. Equivalent to `drop(self)` but
    /// exposes the exit status to the caller, which lets a test
    /// assert on it (e.g. the oversized-frame test wants to see the
    /// JVM exit cleanly even after rejecting a frame).
    pub fn shutdown(mut self) -> std::process::ExitStatus {
        self.shutdown_inner()
            .expect("subprocess shutdown should complete")
    }

    fn shutdown_inner(&mut self) -> Option<std::process::ExitStatus> {
        let mut child = self.child.take()?;
        // Drop child stdin → JVM watchdog sees EOF → Runtime.halt(0).
        drop(child.stdin.take());
        // Wait with a short timeout.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait().expect("try_wait should not fail") {
                Some(status) => return Some(status),
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    return child.wait().ok();
                }
                None => thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

impl Drop for JavaEchoServer {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

fn drain_to_vec(stderr: ChildStderr) -> Vec<u8> {
    use std::io::Read;
    let mut buf = Vec::new();
    let mut reader = stderr;
    let _ = reader.read_to_end(&mut buf);
    buf
}

fn wait_for_ready(stdout: ChildStdout, expected_path: &Path) {
    // We spawn a thread to read with a join-with-timeout, since
    // BufReader::read_line is blocking. The thread reads exactly one
    // line then returns.
    let handle = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap_or(0);
        if n == 0 {
            return Err("child closed stdout before READY".to_string());
        }
        Ok(line.trim().to_string())
    });

    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if handle.is_finished() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for `READY <path>` from EchoServerCli");
        }
        thread::sleep(Duration::from_millis(25));
    }
    let line = handle
        .join()
        .expect("ready-reader thread should join")
        .expect("READY line should be readable");
    assert!(
        line.starts_with("READY "),
        "expected `READY <path>`, got: {line:?}"
    );
    // We don't strict-match the path because Java may canonicalize it
    // through a symlink (macOS /var ↔ /private/var). The
    // round-trip is the real proof anyway.
    let _ = expected_path; // suppress unused warning under cfg
}

/// Helper used by tests that need to bypass `UdsTransport::send` and
/// write raw bytes onto the wire (e.g. the oversized-frame variant).
/// Returns a connected `std::os::unix::net::UnixStream` set to
/// blocking mode.
pub fn raw_uds_connect(path: &Path) -> std::os::unix::net::UnixStream {
    let s = std::os::unix::net::UnixStream::connect(path).expect("raw UDS connect should succeed");
    s.set_nonblocking(false).expect("set blocking mode");
    s
}

/// Write a raw frame (4-byte BE length + payload) to a sync socket.
/// Used by the oversized-frame test to exercise the Java server's
/// read-path cap independently of the Rust send-path cap.
pub fn raw_send_frame(
    sock: &mut std::os::unix::net::UnixStream,
    announced_length: u32,
    payload: &[u8],
) -> std::io::Result<()> {
    let hdr = announced_length.to_be_bytes();
    sock.write_all(&hdr)?;
    sock.write_all(payload)?;
    sock.flush()?;
    Ok(())
}
