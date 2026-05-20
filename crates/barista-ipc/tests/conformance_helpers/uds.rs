// SPDX-License-Identifier: MIT OR Apache-2.0

// Test-support submodule: UDS-flavoured Rust↔Java conformance helpers.
// Loaded via `mod conformance_helpers;` from `tests/conformance.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions,
    dead_code
)]
// Note: the `#[cfg(unix)]` gate is applied on `mod uds;` in
// `conformance_helpers/mod.rs`, not here, to keep clippy's
// `duplicated_attributes` lint happy.

//! Spawn / drive / tear-down for the Java echo *server*
//! (`EchoServerCli`) over Unix domain sockets.
//!
//! In the UDS topology the Java side binds and the Rust side connects:
//! the Rust harness picks a tempdir socket path, hands it to the JVM
//! via `--socket`, waits for the `READY <path>` line on stdout, then
//! dials the socket and drives the test. This matches `EchoServer.java`'s
//! contract.
//!
//! The pipe variant inverts the roles (see `pipe.rs`): Rust binds, Java
//! connects. The Maven-compile / classpath / `java` lookup ceremony in
//! [`super::jvm`] is reused unchanged.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::jvm::{ensure_test_classes_compiled, java_binary, maven_classpath};

/// Maximum time we wait for the Java side to print `READY <path>` on
/// stdout. 30 s accommodates a cold JVM start on a busy CI runner; in
/// practice the warm path is well under 1 s.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

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
