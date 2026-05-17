// Integration-test target — workspace security lints are allowed.
// Panic-on-misuse is the documented contract for failing a test
// loudly; `as` casts are the canonical form for libc constant
// arithmetic.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]
#![cfg(unix)]

//! End-to-end auth tests for the UDS transport.
//!
//! These tests prove the T5 acceptance criteria:
//!
//!   * `server_socket_bound_with_0600_perms` — after `bind_secure`,
//!     `stat(2)` reports mode bits exactly `0o600` and owner UID
//!     equals `geteuid()`.
//!   * `server_run_dir_is_0700` — the parent run directory is
//!     `0o700` (no group / world access).
//!   * `client_rejects_non_0600_socket` — if the socket's mode bits
//!     are loosened to `0o644` after `bind_secure`,
//!     `connect_secure` rejects with `SocketPermsWrong` before
//!     attempting `connect(2)`.
//!   * `client_rejects_non_socket_at_path` — if a regular file is
//!     placed where the socket should be, `connect_secure` rejects
//!     with `NotASocket`.
//!   * `client_rejects_missing_socket` — connect-secure surfaces a
//!     clean `Io(NotFound)` when the path doesn't exist.
//!   * `connect_secure_round_trip_succeeds_on_same_user` — the
//!     full ceremony (perms check + connect + peer-cred check +
//!     send + recv) completes for a same-process pair.

use std::os::unix::fs::PermissionsExt;

use barista_ipc::auth::SocketPath;
use barista_ipc::transport::uds::UdsTransport;
use barista_ipc::{Envelope, Ping, Transport, TransportError, envelope};
use tempfile::TempDir;
use tokio::net::UnixListener;

/// Build a `SocketPath` under a per-test tempdir to keep concurrent
/// tests from colliding on socket paths. The base name is held to
/// one character to stay clear of macOS' 104-char `sun_path` limit
/// even when `$TMPDIR` is long.
fn temp_socket_path() -> (TempDir, SocketPath) {
    let tmp = TempDir::new().unwrap();
    // SocketPath::new_in builds `<base>/<name>.sock` and tightens
    // perms on `base` to `0700`.
    let sp = SocketPath::new_in(tmp.path(), "s").expect("socket path");
    (tmp, sp)
}

#[tokio::test]
async fn server_run_dir_is_0700() {
    let (tmp, sp) = temp_socket_path();
    // `bind_secure` is what runs the mkdir + chmod ceremony, but
    // the run-dir tightening already fired inside `SocketPath::new_in`.
    let _listener = UdsTransport::bind_secure(&sp).expect("bind_secure");

    let run_dir_mode = std::fs::metadata(tmp.path()).unwrap().permissions().mode() & 0o7777;
    assert_eq!(
        run_dir_mode, 0o700,
        "run dir mode should be 0700, got {run_dir_mode:#o}"
    );
}

#[tokio::test]
async fn server_socket_bound_with_0600_perms() {
    let (_tmp, sp) = temp_socket_path();
    let _listener = UdsTransport::bind_secure(&sp).expect("bind_secure");

    let meta = std::fs::metadata(sp.as_path()).unwrap();
    let mode = meta.permissions().mode() & 0o7777;
    assert_eq!(
        mode, 0o600,
        "socket mode should be 0600 after bind_secure, got {mode:#o}"
    );

    // Owner should be the current effective UID.
    use std::os::unix::fs::MetadataExt;
    let our_uid = barista_ipc::auth::our_uid();
    assert_eq!(meta.uid(), our_uid, "socket owner should be our uid");
}

#[tokio::test]
async fn client_rejects_non_0600_socket() {
    let (_tmp, sp) = temp_socket_path();
    let listener = UdsTransport::bind_secure(&sp).expect("bind_secure");

    // Sanity: pre-mutation, verify() passes.
    sp.verify()
        .expect("verify should pass on freshly-bound 0600 socket");

    // Loosen mode bits to 0644 — simulates a misconfigured server
    // that forgot the chmod step (or an attacker with the same UID
    // who relaxed perms to enable a downgrade attack).
    std::fs::set_permissions(sp.as_path(), std::fs::Permissions::from_mode(0o644)).unwrap();

    // Now connect_secure should reject.
    let err = UdsTransport::connect_secure(&sp).await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("0o644") && msg.contains("0o600"),
        "error should name actual + expected mode, got: {msg}"
    );

    // Server task wasn't started; drop the listener so the temp dir
    // can clean up the socket inode.
    drop(listener);
}

#[tokio::test]
async fn client_rejects_non_socket_at_path() {
    let (_tmp, sp) = temp_socket_path();
    // Place a regular file (mode 0600) at the socket path.
    std::fs::write(sp.as_path(), b"not a socket").unwrap();
    std::fs::set_permissions(sp.as_path(), std::fs::Permissions::from_mode(0o600)).unwrap();

    let err = UdsTransport::connect_secure(&sp).await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("not a Unix-domain socket"),
        "error should call out non-socket path, got: {msg}"
    );
}

#[tokio::test]
async fn client_rejects_missing_socket() {
    let (_tmp, sp) = temp_socket_path();
    // No bind happened; socket file doesn't exist.
    let err = UdsTransport::connect_secure(&sp).await.unwrap_err();
    // verify() surfaces `Io(NotFound)` via the metadata call.
    match err {
        TransportError::Io(e) => {
            assert!(
                e.to_string().contains("No such file") || e.to_string().contains("not found"),
                "expected NotFound-style error, got: {e}"
            );
        }
        other => panic!("expected TransportError::Io, got: {other:?}"),
    }
}

#[tokio::test]
async fn connect_secure_round_trip_succeeds_on_same_user() {
    let (_tmp, sp) = temp_socket_path();
    let listener = UdsTransport::bind_secure(&sp).expect("bind_secure");

    // Spawn a server task that accepts one connection and echoes
    // back the first frame.
    let server = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept");
        // Server-side peer-cred check: we should be talking to
        // ourselves.
        barista_ipc::auth::verify_peer_uid(&stream)
            .expect("server-side peer-cred check should pass on same-user pair");
        let mut server = UdsTransport::from_stream(stream);
        let env = server.recv().await.expect("server recv");
        server.send(env).await.expect("server send");
    });

    // Client side runs the full T5 ceremony.
    let mut client = UdsTransport::connect_secure(&sp)
        .await
        .expect("connect_secure should succeed on same-user 0600 socket");

    let sent = Envelope {
        version: 1,
        request_id: 7,
        body: Some(envelope::Body::Ping(Ping {
            client: "auth-test".to_string(),
            sent_at_unix_micros: 42,
        })),
    };
    client.send(sent.clone()).await.expect("client send");
    let echoed = client.recv().await.expect("client recv");
    assert_eq!(sent, echoed, "round-trip should preserve envelope bytes");

    drop(client);
    server.await.expect("server task");
}

#[tokio::test]
async fn bind_secure_idempotent_against_stale_socket() {
    // Simulate a prior crash that left a stale socket inode behind.
    // `bind_secure` should `unlink_if_exists` cleanly and re-bind.
    let (_tmp, sp) = temp_socket_path();
    let listener_v1 = UdsTransport::bind_secure(&sp).expect("first bind");
    drop(listener_v1);
    // Drop closes the listener but does NOT unlink the inode on Linux/macOS.
    assert!(
        sp.as_path().exists(),
        "socket file should persist after listener drop"
    );

    // Second bind should unlink-then-rebind.
    let _listener_v2 = UdsTransport::bind_secure(&sp).expect("second bind should succeed");
    let mode = std::fs::metadata(sp.as_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(mode, 0o600, "second bind should re-tighten perms");
}

#[tokio::test]
async fn peer_uid_mismatch_test_seam_returns_typed_error() {
    // We can't easily fake a cross-user UDS pair without running as
    // multiple users, but the test seam
    // `verify_peer_uid_with_expected` lets us pin the typed-error
    // mapping deterministically.
    let (_tmp, sp) = temp_socket_path();
    let listener = UnixListener::bind(sp.as_path()).expect("bind");

    let accept = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        stream
    });
    let client = tokio::net::UnixStream::connect(sp.as_path())
        .await
        .expect("connect");
    let _server_stream = accept.await.expect("accept task");

    // Force a UID mismatch via the test seam.
    let err = barista_ipc::auth::verify_peer_uid_with_expected(&client, u32::MAX).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("peer UID mismatch"),
        "error should call out mismatch: {msg}"
    );
    assert!(
        msg.contains("4294967295"),
        "Display should show the bogus expected UID: {msg}"
    );
}
