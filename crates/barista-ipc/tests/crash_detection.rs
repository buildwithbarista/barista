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

//! Failure-model unit tests for the M4.2 T6 daemon-crash detection
//! path.
//!
//! Each test in this file exercises the Rust-only side of the
//! crash detection: a synthetic "daemon" UDS peer that connects to a
//! real `UdsTransport` and then either drops the socket abruptly
//! (the kernel surfaces the connection close as `ConnectionReset` /
//! `BrokenPipe`) or half-writes a frame and `_exit`s. We assert that
//! the transport layer surfaces a typed `TransportError::DaemonCrashed`
//! and that the multiplex layer turns a closed-mid-action transport
//! into a synthetic `StreamEvent::Error` carrying the
//! `BAR-DAEMON-CRASHED` code (with `details.retryable = "true"`).
//!
//! The cross-language version of these tests (driving the real
//! `barback` JVM with the `--crash-after` debug flag) lives in
//! `tests/crash_recovery_conformance.rs`.

use std::time::Duration;

use barista_ipc::{
    ActionRequest, Multiplexer, MuxError, StreamEvent, Transport, TransportError,
    mux::DAEMON_CRASHED_CODE, transport::uds::UdsTransport,
};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};

fn temp_socket() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("tempdir creation should succeed");
    let path = dir.path().join("s");
    (dir, path)
}

/// Transport-layer test: when the synthetic peer drops the socket
/// without ever writing a complete frame *and* the Rust side was
/// mid-read on a length prefix, the codec surfaces an
/// `UnexpectedEof` / `ConnectionReset` which `map_codec_io_err`
/// reclassifies as `TransportError::DaemonCrashed`.
#[tokio::test]
async fn transport_recv_after_peer_writes_partial_frame_then_exits_returns_daemon_crashed() {
    let (_dir, path) = temp_socket();
    let listener = UnixListener::bind(&path).expect("listener bind");

    let path_clone = path.clone();
    let peer = tokio::spawn(async move {
        // The "daemon" connects, writes 2 of the 4 length-prefix
        // bytes, then drops the stream. Dropping closes the UnixStream
        // mid-frame; on the client side the codec is blocked in
        // `read_exact(4)` for the length prefix and observes an
        // `UnexpectedEof` once buffered bytes run out — exactly the
        // shape `kill -9` produces against a barback that's already
        // queued a partial response onto the kernel socket buffer.
        let mut s = UnixStream::connect(&path_clone)
            .await
            .expect("peer connect");
        s.write_all(&[0x00, 0x00]).await.expect("partial header");
        // Flush before dropping so the bytes are actually on the wire.
        s.shutdown().await.expect("peer shutdown");
        drop(s);
    });

    let (stream, _addr) = listener.accept().await.expect("accept");
    let mut transport = UdsTransport::from_stream(stream);

    // Bound the wait so a regression that never surfaces an error
    // (e.g. a future codec that silently re-tries on partial reads)
    // fails the test loudly rather than hanging CI.
    let err = tokio::time::timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("recv should not hang")
        .expect_err("recv should fail; peer wrote partial frame then exited");

    assert!(
        matches!(err, TransportError::DaemonCrashed { .. }),
        "expected DaemonCrashed, got {err:?}"
    );
    assert!(err.is_daemon_crash(), "is_daemon_crash predicate");
    assert!(err.is_terminal(), "DaemonCrashed is terminal");

    peer.await.expect("peer task");
}

/// Transport-layer test: a peer that does a *clean* close (no
/// bytes written, polite EOF at a frame boundary) yields the
/// existing `TransportError::Closed` variant — not `DaemonCrashed`.
/// Pins that we do not over-classify a graceful daemon shutdown
/// as a crash.
#[tokio::test]
async fn transport_recv_after_peer_closes_cleanly_returns_closed_not_crashed() {
    let (_dir, path) = temp_socket();
    let listener = UnixListener::bind(&path).expect("listener bind");

    let path_clone = path.clone();
    let peer = tokio::spawn(async move {
        // Connect and immediately drop — clean EOF at a frame
        // boundary (zero bytes pending).
        let s = UnixStream::connect(&path_clone)
            .await
            .expect("peer connect");
        drop(s);
    });

    let (stream, _addr) = listener.accept().await.expect("accept");
    let mut transport = UdsTransport::from_stream(stream);

    let err = tokio::time::timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("recv should not hang")
        .expect_err("recv should fail; peer closed");

    assert!(
        matches!(err, TransportError::Closed),
        "expected Closed (frame-boundary EOF), got {err:?}"
    );

    peer.await.expect("peer task");
}

/// Multiplex-layer test: the daemon "crashes" (drops the connection
/// mid-frame) while a client action is in flight. The client's
/// `ActionHandle::next_event` must yield a synthesized
/// `StreamEvent::Error` carrying the canonical `BAR-DAEMON-CRASHED`
/// code and `details.retryable = "true"`, not a silent `None`.
#[tokio::test]
async fn mux_in_flight_action_surfaces_bar_daemon_crashed_on_peer_drop() {
    let (_dir, path) = temp_socket();
    let listener = UnixListener::bind(&path).expect("listener bind");

    let path_clone = path.clone();
    // The synthetic "daemon": accept the connection, wait until the
    // CLI's submit_action envelope has been read, then drop the
    // stream mid-frame so the CLI observes a crash. We read up to
    // 64 KiB before dropping so the entire ActionRequest envelope is
    // off the kernel socket buffer — this guarantees the writer task
    // on the CLI side completes its `send().await` (no BrokenPipe
    // surfaces from the write half) and the *reader* side is the one
    // that surfaces the crash classification. That matches the
    // realistic shape: a kill -9'd barback that had successfully
    // acknowledged the action's TCP write before halting.
    let daemon = tokio::spawn(async move {
        let (mut stream, _addr) = listener.accept().await.expect("accept");
        let mut scratch = [0u8; 65_536];
        // One read is sufficient — the action envelope is well under
        // 1 KiB and tokio's `read_buf` returns as soon as any bytes
        // are available.
        let n = tokio::io::AsyncReadExt::read(&mut stream, &mut scratch)
            .await
            .expect("read submit");
        assert!(n > 0, "expected the action envelope on the wire");
        // Half-write the length prefix of a reply — this is the
        // "the daemon was constructing a response when it crashed"
        // path. Drop the stream without sending the rest.
        stream
            .write_all(&[0x00, 0x00, 0x00, 0x10])
            .await
            .expect("partial reply prefix");
        stream.shutdown().await.expect("daemon shutdown");
        drop(stream);
        let _ = path_clone; // suppress unused warning
    });

    // CLI side: connect via UdsTransport + spawn the multiplexer.
    let client_stream = UnixStream::connect(&path).await.expect("client connect");
    let transport = UdsTransport::from_stream(client_stream);
    let (_mux, client, _server) = Multiplexer::spawn(transport);

    let handle = client
        .submit_action(ActionRequest {
            action_id: String::new(),
            ..Default::default()
        })
        .await
        .expect("submit_action");

    // The handle's first event must be the synthesised crash error.
    let mut handle = handle;
    let evt = tokio::time::timeout(Duration::from_secs(5), handle.next_event())
        .await
        .expect("next_event should not hang")
        .expect("next_event Result");
    let evt = evt.expect("first event present");
    match evt {
        StreamEvent::Error(err) => {
            assert_eq!(err.code, DAEMON_CRASHED_CODE, "wire error code");
            assert_eq!(
                err.action_id,
                handle.action_id(),
                "error scoped to the in-flight action"
            );
            assert_eq!(
                err.details.get("retryable").map(String::as_str),
                Some("true"),
                "retryable=true in details map"
            );
            assert!(!err.message.is_empty(), "human-readable message populated");
        }
        other => panic!("expected StreamEvent::Error, got {other:?}"),
    }

    // Subsequent events: the per-action channel is closed after the
    // synthesised terminal error, so the next recv returns None.
    let drained = tokio::time::timeout(Duration::from_secs(2), handle.next_event())
        .await
        .expect("next_event should not hang")
        .expect("next_event Result");
    assert!(drained.is_none(), "channel closed after terminal error");

    daemon.await.expect("daemon task");
}

/// Multiplex-layer test: a clean peer close with *no* in-flight
/// actions yields no synthesised crash error — clients that arrived
/// after the close see `MultiplexerShutDown` on submit, and any
/// drained channel sees `None`. Pins the "graceful daemon shutdown"
/// path against the crash path.
#[tokio::test]
async fn mux_clean_close_with_no_in_flight_does_not_synthesise_crash() {
    let (_dir, path) = temp_socket();
    let listener = UnixListener::bind(&path).expect("listener bind");

    let path_clone = path.clone();
    let daemon = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept");
        // Immediately drop without reading anything — clean EOF.
        drop(stream);
        let _ = path_clone;
    });

    let client_stream = UnixStream::connect(&path).await.expect("client connect");
    let transport = UdsTransport::from_stream(client_stream);
    let (_mux, client, _server) = Multiplexer::spawn(transport);

    // Give the reader task time to observe EOF + flag shutdown.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Submitting against a torn-down connection surfaces
    // `MultiplexerShutDown`, not `DaemonCrashed`. The no-in-flight
    // path means there is no per-action client to receive the
    // synthesised crash — submission post-close is the closest
    // observable signal.
    let result = client
        .submit_action(ActionRequest {
            action_id: String::new(),
            ..Default::default()
        })
        .await;
    match result {
        Err(MuxError::MultiplexerShutDown) => {}
        Err(MuxError::Transport(_)) => {
            // Acceptable too: depending on scheduling the outbound
            // send may fail before the state flag is observed. The
            // discriminator is "not a DaemonCrashed".
        }
        Err(other) => {
            assert!(
                !matches!(other, MuxError::DaemonCrashed { .. }),
                "must not synthesise DaemonCrashed on clean-close path; got {other:?}"
            );
        }
        Ok(_) => panic!("submit_action should fail post-close"),
    }

    daemon.await.expect("daemon task");
}
