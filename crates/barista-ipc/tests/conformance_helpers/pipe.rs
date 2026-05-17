// Test-support submodule: named-pipe Rust↔Java conformance helpers.
// Loaded via `mod conformance_helpers;` from
// `tests/conformance_pipe.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions,
    dead_code
)]
// Note: the `#[cfg(windows)]` gate is applied on `mod pipe;` in
// `conformance_helpers/mod.rs`, not here, to keep clippy's
// `duplicated_attributes` lint happy.

//! Spawn / drive / tear-down for the Java echo *client*
//! (`EchoPipeClientCli`) over Windows named pipes.
//!
//! # Role inversion (vs UDS)
//!
//! On UDS, the Java side binds via `ServerSocketChannel.open(UNIX)`
//! and the Rust side connects. On named pipes, the topology is
//! inverted: the Rust side binds (so it owns the DACL installed by
//! [`barista_ipc::transport::pipe::NamedPipeTransport::bind_secure`]),
//! and the Java side connects via `RandomAccessFile`. That's because
//! the Win32 pipe namespace exposes pipes as filesystem-like paths
//! (`\\.\pipe\<name>`), which Java's `RandomAccessFile` can open
//! directly — no JNI'ing into `CreateNamedPipeW` required.
//!
//! Concretely, the spawn ceremony is:
//!
//!   1. Rust binds a `NamedPipeServer` via `bind_secure` (or plain
//!      `ServerOptions::create` for the non-secure variant).
//!   2. Rust spawns `EchoPipeClientCli --pipe <full-pipe-path>`.
//!   3. Java opens `\\.\pipe\<name>` as a `RandomAccessFile("rw")`
//!      and prints `READY <pipe>` on stdout once the open succeeds.
//!   4. Rust calls `NamedPipeServer::connect().await` (which
//!      *resolves* — the kernel matches Java's `CreateFile` to our
//!      pending listen).
//!   5. Rust wraps the connected server via
//!      `NamedPipeTransport::from_server` and drives the test.
//!
//! Step 4 is the subtle bit: on Win32, the server's `ConnectNamedPipe`
//! either returns immediately (because a client is already pending) or
//! blocks until one arrives. tokio's `NamedPipeServer::connect()`
//! wraps that — either way, awaiting it after the Java client opens
//! the pipe is correct.
//!
//! # Why `RandomAccessFile` (not `AsynchronousFileChannel`)
//!
//! `RandomAccessFile` is the simplest portable Java API for the Win32
//! pipe namespace. It backs a synchronous blocking-IO model — perfect
//! for the conformance harness, where each test spawns a fresh JVM and
//! drives exactly one round-trip per envelope. `AsynchronousFileChannel`
//! would let us issue overlapping IO, but the echo loop is
//! single-action-at-a-time by construction (Rust mux multiplex tests on
//! UDS already prove the codec's parallel correctness; see the
//! "Concurrency" note in `tests/conformance_pipe.rs`).
//!
//! # `READY` handshake
//!
//! The Rust side binds the pipe *before* spawning Java, so in theory
//! Java's connect attempt could race the spawn. Two safeguards:
//!
//!   * `ServerOptions::create` (under `bind_secure` or plain) registers
//!     the pipe with the kernel synchronously, so by the time
//!     `Command::spawn` returns, the pipe is listenable.
//!   * Java emits `READY <pipe>` only *after* its `RandomAccessFile`
//!     constructor returns — i.e. after Win32's `CreateFileW` returned
//!     a valid handle. Rust waits for that line before calling
//!     `connect().await`, which then resolves immediately.

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::jvm::{ensure_test_classes_compiled, java_binary, maven_classpath};

/// Maximum time we wait for the Java side to print `READY <pipe>` on
/// stdout. 30 s accommodates a cold JVM start on a busy CI runner; in
/// practice the warm path is well under 1 s.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// A running `EchoPipeClientCli` subprocess + the pipe path it
/// connected to.
///
/// On `Drop`:
///   1. Closes the child's stdin (the watchdog thread inside the JVM
///      observes EOF and calls `Runtime.halt(0)`).
///   2. Waits up to 5 s for the process to exit; if it doesn't, kills
///      it.
///   3. Drains stderr to a side buffer the harness can fetch via
///      `stderr_log()` for debugging.
pub struct JavaEchoPipeClient {
    pipe_name: String,
    child: Option<Child>,
    stderr_drainer: Option<thread::JoinHandle<Vec<u8>>>,
}

impl JavaEchoPipeClient {
    /// Spawn `EchoPipeClientCli --pipe <name>` and wait for its
    /// `READY <pipe>` line. The caller is responsible for having
    /// already bound the pipe (via `ServerOptions::create` or
    /// `NamedPipeTransport::bind_secure`) *before* calling this — see
    /// the module-level docs on the bind-spawn race.
    pub fn spawn(pipe_name: String) -> Self {
        ensure_test_classes_compiled();
        let cp = maven_classpath();

        let mut child = Command::new(java_binary())
            .arg("-cp")
            .arg(cp)
            .arg("com.bluminal.barista.barback.conformance.EchoPipeClientCli")
            .arg("--pipe")
            .arg(&pipe_name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("`java EchoPipeClientCli` should spawn");

        let stdout = child.stdout.take().expect("child stdout is piped");
        let stderr = child.stderr.take().expect("child stderr is piped");

        let stderr_drainer = thread::spawn(move || drain_to_vec(stderr));
        wait_for_ready(stdout);

        Self {
            pipe_name,
            child: Some(child),
            stderr_drainer: Some(stderr_drainer),
        }
    }

    /// The pipe path the Java client connected to.
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    /// Drain the child's accumulated stderr buffer. Useful for debug
    /// output when a test panics.
    pub fn stderr_log(&mut self) -> Vec<u8> {
        if let Some(handle) = self.stderr_drainer.take() {
            handle.join().unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Cleanly shut down the JVM. Equivalent to `drop(self)` but
    /// exposes the exit status to the caller.
    pub fn shutdown(mut self) -> std::process::ExitStatus {
        self.shutdown_inner()
            .expect("subprocess shutdown should complete")
    }

    fn shutdown_inner(&mut self) -> Option<std::process::ExitStatus> {
        let mut child = self.child.take()?;
        drop(child.stdin.take());
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

impl Drop for JavaEchoPipeClient {
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

fn wait_for_ready(stdout: ChildStdout) {
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
            panic!("timed out waiting for `READY <pipe>` from EchoPipeClientCli");
        }
        thread::sleep(Duration::from_millis(25));
    }
    let line = handle
        .join()
        .expect("ready-reader thread should join")
        .expect("READY line should be readable");
    assert!(
        line.starts_with("READY "),
        "expected `READY <pipe>`, got: {line:?}"
    );
}

/// Build a unique pipe name per test to avoid collisions across
/// concurrent `cargo test` workers and across re-runs.
///
/// Pipes live under `\\.\pipe\barista-ipc-test-<test_id>-<pid>-<nanos>`.
/// We deliberately stay off the production `\\.\pipe\barista\` root so a
/// crashed test never collides with a live daemon.
pub fn unique_test_pipe_name(test_id: &str) -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(r"\\.\pipe\barista-ipc-test-{test_id}-{pid}-{nanos}")
}
