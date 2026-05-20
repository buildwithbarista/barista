// SPDX-License-Identifier: MIT OR Apache-2.0

// Integration-test target — workspace security lints are allowed.
// Panic-on-misuse is the documented contract for failing a test loudly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]
#![cfg(unix)]

//! Cross-language M4.2 T6 failure-model conformance test.
//!
//! Launches the real `com.bluminal.barista.barback.Server` JVM with
//! the hidden `--crash-after <n>` debug flag (added in this task),
//! drives `n` action envelopes through the Rust IPC client, asserts
//! the daemon self-terminates with exit code 137 mid-action, and
//! asserts the in-flight Rust `ActionHandle` surfaces the canonical
//! `BAR-DAEMON-CRASHED` retryable error.
//!
//! This is the cross-language counterpart to `tests/crash_detection.rs`
//! which exercises the same wiring against a synthetic Rust-only
//! daemon. The synthetic test covers the codec / mux logic in
//! milliseconds; this test proves the contract holds against a real
//! barback JVM that goes through `Runtime.halt(137)` mid-write.
//!
//! `#[ignore]`-gated for the same reason as the M4.1 T7 / T8
//! conformance tests: requires Maven + JDK on PATH. CI's `barback`
//! job runs the M4.1 conformance suite already; the M4.2 follow-up
//! to wire this test into the same job is a one-line addition.
//!
//! Run manually with:
//!
//! ```bash
//!   cargo test -p barista-ipc --test crash_recovery_conformance \
//!       -- --ignored --test-threads=1
//! ```

mod conformance_helpers;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use barista_ipc::{
    ActionRequest, Multiplexer, StreamEvent, mux::DAEMON_CRASHED_CODE, transport::uds::UdsTransport,
};
use tempfile::TempDir;
use tokio::net::UnixStream;

use conformance_helpers::jvm::{ensure_test_classes_compiled, java_binary, maven_classpath};

/// Spawn `barback Server.main` with `--socket <path> --crash-after <n>`.
/// Returns the child handle + the bound socket path.
///
/// The JVM's accept loop binds inside `Server.start(...)` and
/// publishes the listening port via the `INFO: barback listening on
/// <path>` log line on stderr. We poll the socket path for existence
/// (the inode appears synchronously with the bind) rather than parse
/// stderr — log-line parsing across JDK locales is a fragile contract
/// for a fast-iterating dev loop.
fn spawn_barback(socket_path: &std::path::Path, crash_after: usize) -> Child {
    ensure_test_classes_compiled();
    let cp = maven_classpath();
    let mut child = Command::new(java_binary())
        .arg("-cp")
        .arg(cp)
        .arg("com.bluminal.barista.barback.Server")
        .arg("--socket")
        .arg(socket_path)
        .arg("--crash-after")
        .arg(crash_after.to_string())
        // 60 s idle window so the test doesn't race the idle-shutdown
        // path; we tear the daemon down explicitly at end of test.
        .arg("--idle-shutdown")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("java Server should spawn");

    // Drain stderr on a background thread so the JVM doesn't block on
    // a full pipe when logging at WARNING / SEVERE (the
    // `--crash-after` arming path logs at WARNING, and the
    // self-immolation path logs at SEVERE).
    let stderr = child.stderr.take().expect("piped stderr");
    thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = std::io::BufReader::new(stderr).read_to_end(&mut buf);
        // Surface in `cargo test -- --nocapture` only; quiet by default.
        if std::env::var("BARISTA_CRASH_IT_VERBOSE").is_ok() {
            eprintln!("[barback stderr]\n{}", String::from_utf8_lossy(&buf));
        }
    });
    let stdout = child.stdout.take().expect("piped stdout");
    thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = std::io::BufReader::new(stdout).read_to_end(&mut buf);
    });

    // Poll for the socket inode. The JVM binds before the
    // `Server.start` call returns; we wait up to 30 s for a busy CI
    // runner cold-start.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !socket_path.exists() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "barback did not bind the socket at {} within 30 s",
                socket_path.display()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    child
}

fn temp_socket() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir creation should succeed");
    // Short leaf name so we stay under macOS's 104-char sun_path cap.
    let path = dir.path().join("s");
    (dir, path)
}

/// Force the once-per-process classpath + test-compile cache in
/// `conformance_helpers::jvm` to warm before the first JVM spawn,
/// so the spawn-side timing budget below doesn't have to include
/// the `mvn dependency:build-classpath` and `mvn test-compile`
/// runs on a cold cargo-test cache.
fn classpath_cache_warm() -> &'static () {
    static WARMED: OnceLock<()> = OnceLock::new();
    WARMED.get_or_init(|| {
        ensure_test_classes_compiled();
        let _ = maven_classpath();
    })
}

/// The M4.2 T6 acceptance-criterion fixture: launch a real barback
/// with `--crash-after 1`, send two action envelopes, and assert:
///
///   1. action #1 is *the trigger* — the daemon halts before its
///      reply hits the wire;
///   2. the in-flight Rust `ActionHandle` for action #1 surfaces a
///      `StreamEvent::Error` carrying `BAR-DAEMON-CRASHED` with
///      `details.retryable = "true"`;
///   3. the JVM exit status is 137 (= the configured
///      `CRASH_EXIT_CODE`, matching `128 + SIGKILL`).
///
/// This closes the `[T]` "CLI auto-respawns daemon after `kill -9`;
/// in-flight actions surface retryable error" acceptance criterion
/// from M4.2 — the synthesized-error half. Auto-respawn itself is
/// the M4.3 dispatcher's concern (the CLI calls `Multiplexer::spawn`
/// against a fresh transport after observing this code); the
/// failure-model *mechanism* is what lands here.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Maven + JDK on PATH; run with --ignored"]
async fn real_barback_crash_after_one_action_surfaces_bar_daemon_crashed() {
    classpath_cache_warm();

    let (_tmp, socket_path) = temp_socket();
    let mut child = spawn_barback(&socket_path, 1);

    let connect_result = {
        // Brief connect-retry loop: the socket inode existing doesn't
        // strictly guarantee `accept()` is armed yet on every libc.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match UnixStream::connect(&socket_path).await {
                Ok(s) => break Ok(s),
                Err(e) if Instant::now() >= deadline => break Err(e),
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    };
    let client_stream = connect_result.expect("connect to barback");
    let transport = UdsTransport::from_stream(client_stream);
    let (_mux, client, _server) = Multiplexer::spawn(transport);

    // Submit two actions back-to-back. The first triggers the crash
    // (daemon dispatches action #1, `--crash-after 1` increments
    // counter to 1, halt(137) fires before reply is written). The
    // second is queued in the outbound mpsc but never reaches the
    // wire — it's not part of the AC, just defends against an
    // off-by-one that would leak past the trigger.
    let handle_a = client
        .submit_action(ActionRequest::default())
        .await
        .expect("submit action #1");
    let _handle_b_result = client.submit_action(ActionRequest::default()).await;
    // We don't assert on _handle_b: depending on tokio scheduling
    // the outbound mpsc may or may not have accepted the second
    // submission before the writer task observes the closed socket.
    // Either MultiplexerShutDown or a successful handle-then-crash
    // are valid; the load-bearing assertion is on action #1.

    let mut handle_a = handle_a;
    let evt = tokio::time::timeout(Duration::from_secs(30), handle_a.next_event())
        .await
        .expect("next_event should not hang past 30 s")
        .expect("next_event should not Err");
    let evt = evt.expect("first event should be present, not None");

    match evt {
        StreamEvent::Error(err) => {
            assert_eq!(
                err.code, DAEMON_CRASHED_CODE,
                "M4.2 T6 wire code (PRD §A BAR-DAEMON-001)"
            );
            assert_eq!(
                err.action_id,
                handle_a.action_id(),
                "error scoped to the in-flight action"
            );
            assert_eq!(
                err.details.get("retryable").map(String::as_str),
                Some("true"),
                "retryable=true in details map"
            );
        }
        other => panic!(
            "expected StreamEvent::Error(BAR-DAEMON-CRASHED) on real-barback crash, got {other:?}"
        ),
    }

    // The JVM should have exited with status 137. We wait a few
    // seconds because `Runtime.halt` is synchronous on the calling
    // thread but the OS reports the exit asynchronously to the
    // parent.
    let deadline = Instant::now() + Duration::from_secs(15);
    let exit_status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("barback did not exit within 15 s of the trigger");
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    };
    // On Unix, exit_status.code() returns Some(137) for
    // Runtime.halt(137); the kernel does NOT convert this to a
    // signal status because the JVM exited cleanly from the kernel's
    // perspective (process called exit_group(137), not killed via
    // signal). The 137 value is just our chosen exit code that
    // happens to equal 128 + SIGKILL for log readability.
    assert_eq!(
        exit_status.code(),
        Some(137),
        "JVM exit status matches Server.CRASH_EXIT_CODE; got {exit_status:?}"
    );
}
