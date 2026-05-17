// Integration-test target — workspace security lints are allowed.
// Panic-on-misuse is the documented contract for failing a test
// loudly, and `as` casts are the canonical form for prost-generated
// enum tag values (`Status::Success as i32`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]
#![cfg(unix)]

//! Cross-language Rust↔Java conformance tests for the worker IPC wire
//! protocol.
//!
//! These tests pair the Rust [`barista_ipc::transport::uds::UdsTransport`]
//! against the Java echo server in
//! `barback/src/test/java/com/bluminal/barista/barback/conformance/EchoServer.java`,
//! exercise every `Envelope.body` variant, the 32-in-flight concurrency
//! contract, the oversized-frame guardrail, and the credentials-envelope
//! round-trip path that ties [`barista_ipc::Credential`]'s `ZeroizeOnDrop`
//! contract to the Java side's `RedactedCredential` adapter.
//!
//! # Why this exists
//!
//! `proto/barista/v1/worker.proto` is the contract between the CLI
//! (Rust, `prost`) and the daemon (Java, `protobuf-java`). The two
//! generators read the *same* schema, but ABI skew is real:
//!
//!   * a missing `reserved` block on one side could let it accept a
//!     wire-format the other rejects;
//!   * a forgotten `oneof` variant could decode without error but lose
//!     the discriminator;
//!   * an int / int64 mismatch could overflow silently.
//!
//! The conformance harness is the canary for all three. It catches
//! schema drift before it reaches the integration test in M5.x where
//! the CLI actually drives the daemon.
//!
//! # Wire-format contract (PRD §12.1)
//!
//!   ```text
//!   ┌────────────────────────┬──────────────────────────┐
//!   │ length: u32 big-endian │ payload: protobuf bytes  │
//!   │       (4 bytes)        │      (length bytes)      │
//!   └────────────────────────┴──────────────────────────┘
//!   ```
//!
//! Both sides agree on:
//!
//!   * 4-byte big-endian length prefix (NOT varint — protobuf-java's
//!     `writeDelimitedTo` uses varint and is unsuitable here);
//!   * length covers payload only, not the prefix itself;
//!   * `MAX_FRAME_BYTES = 16 MiB` cap, symmetric on both directions.
//!
//! Encoded-bytes equality (`prost::Message::encode_to_vec` ↔
//! `Envelope.toByteArray`) is the canonical equivalence on the wire:
//! the Java side decodes via `parseFrom` then re-encodes via
//! `toByteArray`, and we assert byte-equality on the result. Comparing
//! the decoded `Envelope` struct directly may fail on field-order or
//! unknown-field handling — encoded bytes are the contract.
//!
//! # Why `#[ignore]`?
//!
//! These tests require Maven + a JDK installed on the host. CI has
//! both (see `.github/workflows/ci.yml`'s `barback` job); local dev
//! may not. Gating the tests behind `#[ignore]` keeps
//! `cargo test -p barista-ipc` green on developer machines that
//! don't have `mvn`.
//!
//! Run manually with:
//!
//! ```bash
//!   cargo test -p barista-ipc --test conformance -- --ignored
//! ```
//!
//! Or run a single variant with:
//!
//! ```bash
//!   cargo test -p barista-ipc --test conformance \
//!       -- --ignored roundtrip_body_ping
//! ```
//!
//! CI runs the full suite via the `barback` job in the workflow.

// Helpers live in `tests/conformance_helpers/mod.rs`. The submodule is
// named distinctly from this file so `cargo test` doesn't see the
// `tests/conformance/` directory as a sibling integration-test target
// (Cargo treats each top-level file/directory under `tests/` as its
// own binary).
mod conformance_helpers;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use barista_ipc::transport::uds::UdsTransport;
use barista_ipc::{
    ActionRequest, ActionResult, ActionStream, CancelRequest, Credential, CredentialsEnvelope,
    Envelope, Error, MAX_FRAME_BYTES, Mojo, Ping, Pong, ProducedArtifact, ProgressEvent, Shutdown,
    SshKey, StatusRequest, StatusResponse, Transport, action_result, credential, envelope,
    progress_event,
};
use prost::Message;
use tempfile::TempDir;

use crate::conformance_helpers::{JavaEchoServer, raw_send_frame, raw_uds_connect};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a temp-dir + a short socket path inside it. macOS limits
/// `sun_path` to 104 chars; the one-character leaf name keeps us
/// well below that bound even when `$TMPDIR` is the long
/// `/var/folders/.../T` form.
fn temp_socket() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir creation should succeed");
    let path = dir.path().join("s.sock");
    (dir, path)
}

/// Spawn a Java echo server bound at a fresh tempdir socket. Returns
/// the JVM handle plus the tempdir guard (which must be held for the
/// life of the test — when it drops, the socket inode is reaped).
fn spawn_echo() -> (JavaEchoServer, TempDir) {
    let (dir, path) = temp_socket();
    let server = JavaEchoServer::spawn(path);
    (server, dir)
}

fn sample_mojo() -> Mojo {
    Mojo {
        group_id: "org.apache.maven.plugins".to_string(),
        artifact_id: "maven-compiler-plugin".to_string(),
        version: "3.11.0".to_string(),
        goal: "compile".to_string(),
        execution_id: "default-compile".to_string(),
    }
}

fn sample_action_request() -> ActionRequest {
    let mut system_properties = HashMap::new();
    system_properties.insert("maven.compiler.source".to_string(), "21".to_string());
    let mut environment = HashMap::new();
    environment.insert("LANG".to_string(), "en_US.UTF-8".to_string());

    ActionRequest {
        action_id: "act-cnf-1".to_string(),
        mojo_coords: "org.apache.maven.plugins:maven-compiler-plugin:3.11.0:compile".to_string(),
        project_root: "/work/proj".to_string(),
        pom_path: "/work/proj/pom.xml".to_string(),
        effective_pom_blob: vec![0xa1, 0x62, 0x69, 0x64, 0x01],
        classpath: vec!["/cas/a.jar".to_string()],
        plugin_classpath: vec!["/cas/p.jar".to_string()],
        system_properties,
        environment,
        working_directory: "/work/proj".to_string(),
        stdout_stream_id: 1,
        stderr_stream_id: 2,
        quiet: false,
        maven_compat: "3".to_string(),
        jvm_args: vec!["-Xmx512m".to_string()],
        credentials: None,
        extra_mvn_args: vec![],
    }
}

/// All 11 `Envelope.body` variants in the canonical schema order,
/// each carrying enough non-default state to exercise its
/// discriminator on the wire. Mirrors the helper in
/// `tests/transport_uds.rs` so the two suites pin the same
/// representative shapes.
fn all_body_variants() -> Vec<envelope::Body> {
    vec![
        envelope::Body::Ping(Ping {
            client: "barista 0.1.0".to_string(),
            sent_at_unix_micros: 1,
        }),
        envelope::Body::Pong(Pong {
            daemon: "barback 0.1.0".to_string(),
            jdk_id: "temurin-21".to_string(),
            jdk_version: "21.0.4".to_string(),
            server_unix_micros: 2,
            client_unix_micros: 1,
        }),
        envelope::Body::Action(sample_action_request()),
        envelope::Body::Stream(ActionStream {
            stream_id: 1,
            payload: b"[INFO] Building...\n".to_vec(),
            end: false,
            action_id: "act-stream".to_string(),
        }),
        envelope::Body::Result(ActionResult {
            action_id: "act-result".to_string(),
            status: action_result::Status::Success as i32,
            exit_code: 0,
            duration_micros: 1000,
            artifacts: vec![ProducedArtifact {
                path: "/work/proj/target/foo.jar".to_string(),
                size_bytes: 2048,
                sha256: "abc123".to_string(),
            }],
            failure_message: String::new(),
            failure_stack: String::new(),
            attributes: HashMap::new(),
            error: None,
        }),
        envelope::Body::Progress(ProgressEvent {
            kind: progress_event::Kind::Fetching as i32,
            action_id: "act-progress".to_string(),
            timestamp: "2026-05-14T12:34:56.789Z".to_string(),
            coord: "org.junit.jupiter:junit-jupiter-api:5.10.0".to_string(),
            phase: "fetch".to_string(),
            progress: 42.5,
            mojo: Some(sample_mojo()),
            details: HashMap::new(),
        }),
        envelope::Body::Cancel(CancelRequest {
            action_id: "act-cancel".to_string(),
            grace_period_ms: 5000,
        }),
        envelope::Body::Shutdown(Shutdown { drain_seconds: 5 }),
        envelope::Body::StatusRequest(StatusRequest {}),
        envelope::Body::Status(StatusResponse {
            uptime_seconds: 60,
            workers_total: 4,
            workers_busy: 0,
            actions_executed: 0,
            actions_failed: 0,
            cached_classloaders: 0,
            heap_used_bytes: 0,
            heap_max_bytes: 0,
            jit_state: "cold".to_string(),
        }),
        envelope::Body::Error(Error {
            code: "BAR-PROTO-001".to_string(),
            message: "version mismatch".to_string(),
            details: HashMap::new(),
            action_id: String::new(),
        }),
    ]
}

/// Round-trip an envelope through the Java echo server, asserting
/// encoded-bytes equality on the reply. This is the canonical
/// equivalence: the Java side ran `parseFrom` + `toByteArray`, so
/// matching bytes prove the schemas agree on every field tag and
/// wire-type used in the payload.
async fn roundtrip_assert_bytes_equal(client: &mut UdsTransport, env: Envelope) {
    let sent_bytes = env.encode_to_vec();
    client.send(env).await.expect("client send");
    let echoed = client.recv().await.expect("client recv");
    let echoed_bytes = echoed.encode_to_vec();
    assert_eq!(
        sent_bytes, echoed_bytes,
        "encoded-bytes equality should hold after Java round-trip",
    );
}

// ---------------------------------------------------------------------------
// 1. Per-variant round-trip — one test per Envelope.body variant.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_body_ping() {
    roundtrip_body_index(0).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_body_pong() {
    roundtrip_body_index(1).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_body_action() {
    roundtrip_body_index(2).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_body_stream() {
    roundtrip_body_index(3).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_body_result() {
    roundtrip_body_index(4).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_body_progress() {
    roundtrip_body_index(5).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_body_cancel() {
    roundtrip_body_index(6).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_body_shutdown() {
    roundtrip_body_index(7).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_body_status_request() {
    roundtrip_body_index(8).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_body_status() {
    roundtrip_body_index(9).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_body_error() {
    roundtrip_body_index(10).await;
}

async fn roundtrip_body_index(index: usize) {
    let (server, _dir) = spawn_echo();
    let mut client = UdsTransport::connect(server.socket_path())
        .await
        .expect("UdsTransport::connect");
    let body = all_body_variants()
        .into_iter()
        .nth(index)
        .expect("index in range");
    let env = Envelope {
        version: 1,
        request_id: 0x100 + index as u64,
        body: Some(body),
    };
    roundtrip_assert_bytes_equal(&mut client, env).await;
    drop(client);
    let status = server.shutdown();
    assert!(
        status.success(),
        "Java echo server should exit cleanly after client close; got {status:?}",
    );
}

// ---------------------------------------------------------------------------
// 2. Empty Envelope (body = None) — the smallest legal frame.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_empty_envelope() {
    let (server, _dir) = spawn_echo();
    let mut client = UdsTransport::connect(server.socket_path())
        .await
        .expect("connect");
    let env = Envelope {
        version: 1,
        request_id: 1,
        body: None,
    };
    roundtrip_assert_bytes_equal(&mut client, env).await;
}

// ---------------------------------------------------------------------------
// 3. Small + near-cap ActionStream.payload exercise.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_actionstream_small_payload() {
    let (server, _dir) = spawn_echo();
    let mut client = UdsTransport::connect(server.socket_path())
        .await
        .expect("connect");
    let env = Envelope {
        version: 1,
        request_id: 9001,
        body: Some(envelope::Body::Stream(ActionStream {
            stream_id: 1,
            payload: b"hello cross-lang\n".to_vec(),
            end: true,
            action_id: "act-stream-small".to_string(),
        })),
    };
    roundtrip_assert_bytes_equal(&mut client, env).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_actionstream_near_cap_payload() {
    let (server, _dir) = spawn_echo();
    let mut client = UdsTransport::connect(server.socket_path())
        .await
        .expect("connect");

    // 16 MiB - 1 KiB of payload bytes — under the cap, but large
    // enough to exercise multi-iteration reads on both sides and the
    // codec's frame-assembly path. Leaving 1 KiB headroom for the
    // protobuf overhead (tag bytes, length varints, action_id string)
    // so the encoded Envelope stays under MAX_FRAME_BYTES.
    let big = vec![0xABu8; MAX_FRAME_BYTES - 1024];
    let env = Envelope {
        version: 1,
        request_id: 9002,
        body: Some(envelope::Body::Stream(ActionStream {
            stream_id: 2,
            payload: big,
            end: true,
            action_id: "act-stream-big".to_string(),
        })),
    };
    roundtrip_assert_bytes_equal(&mut client, env).await;
}

// ---------------------------------------------------------------------------
// 4. Concurrency / ordering — 32 envelopes in flight.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn concurrent_32_inflight_preserves_order() {
    // Send 32 distinct envelopes back-to-back without waiting for
    // responses; the Java echo server is single-threaded, so the
    // responses come back in the order they were sent. Asserting on
    // both order AND encoded-bytes equality doubles as a check that
    // the Rust codec's split sender/receiver halves don't interleave
    // bytes mid-frame under burst load. (T6's `mux_concurrent` test
    // already proves the mux layer's correctness on the Rust-only
    // side; this is the cross-language complement.)
    let (server, _dir) = spawn_echo();
    let mut client = UdsTransport::connect(server.socket_path())
        .await
        .expect("connect");

    let mut sent_bytes = Vec::with_capacity(32);
    for i in 0..32u64 {
        let env = Envelope {
            version: 1,
            request_id: 1_000_000 + i,
            body: Some(envelope::Body::Ping(Ping {
                client: format!("barista 0.1.0 #{i}"),
                sent_at_unix_micros: i as i64,
            })),
        };
        sent_bytes.push(env.encode_to_vec());
        client.send(env).await.expect("send");
    }

    for (i, expected) in sent_bytes.iter().enumerate() {
        let echoed = client.recv().await.expect("recv");
        let echoed_bytes = echoed.encode_to_vec();
        assert_eq!(
            *expected, echoed_bytes,
            "envelope #{i} should round-trip in order",
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Frame-too-large rejection (Java side).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn frame_too_large_is_rejected_by_java() {
    // Bypass `UdsTransport::send` (which enforces the cap on the
    // sender side) and write a raw 17 MiB length prefix. The Java
    // EchoServer's read path enforces MAX_FRAME_BYTES = 16 MiB on
    // inbound and closes the connection without reading the body.
    //
    // We use a sync std::os::unix::net::UnixStream because we
    // explicitly *don't* want tokio's codec — the test is about the
    // raw wire shape, not the framed transport.
    let (server, _dir) = spawn_echo();
    let mut raw = raw_uds_connect(server.socket_path());

    // Announce 17 MiB but don't actually send the body — we just want
    // to see the Java side close the connection on read of the
    // oversized length. Sending a single byte after the prefix lets
    // us trigger the cap check on the Java side without spending
    // 17 MiB of memory or wire time.
    let announced = 17u32 * 1024 * 1024;
    let result = raw_send_frame(&mut raw, announced, &[0u8]);
    // The write may succeed (the Java side may not have read the
    // header yet) — what we care about is the subsequent read.
    drop(result);

    // The Java side closes the connection on oversized-frame detection.
    // From the Rust side we observe this as EOF on the next read.
    use std::io::Read;
    let mut buf = [0u8; 64];
    // Set a sensible read timeout so a buggy Java side doesn't hang
    // the test indefinitely.
    raw.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set_read_timeout");
    let n = raw.read(&mut buf).expect("read after oversized prefix");
    assert_eq!(
        n,
        0,
        "Java echo server should close the connection on oversized frame; \
         got {n} bytes back: {:?}",
        &buf[..n],
    );
}

// ---------------------------------------------------------------------------
// 6. Credentials envelope round-trip.
// ---------------------------------------------------------------------------
//
// The `CredentialsEnvelope` contract in `proto/barista/v1/worker.proto`
// is *schema-level*: the Java side decodes it via the generated
// `CredentialsEnvelope.parseFrom` and re-emits via `toByteArray`. The
// Rust side's `ZeroizeOnDrop` derives on `Credential` /
// `CredentialsEnvelope` / `SshKey` are NOT exercised here — that's the
// job of `tests/auth_zeroize.rs`. This test just proves the wire shape
// for `CredentialsEnvelope { entries: [Credential { secret: Password }]}`
// survives a Rust↔Java↔Rust round-trip with identical bytes.

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_credentials_password() {
    let (server, _dir) = spawn_echo();
    let mut client = UdsTransport::connect(server.socket_path())
        .await
        .expect("connect");

    let env = Envelope {
        version: 1,
        request_id: 7,
        body: Some(envelope::Body::Action(ActionRequest {
            credentials: Some(CredentialsEnvelope {
                entries: vec![Credential {
                    server_id: "central".to_string(),
                    username: "alice".to_string(),
                    secret: Some(credential::Secret::Password(
                        "hunter2-decrypted-on-rust-side".to_string(),
                    )),
                }],
            }),
            ..sample_action_request()
        })),
    };
    roundtrip_assert_bytes_equal(&mut client, env).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_credentials_token() {
    let (server, _dir) = spawn_echo();
    let mut client = UdsTransport::connect(server.socket_path())
        .await
        .expect("connect");

    let env = Envelope {
        version: 1,
        request_id: 8,
        body: Some(envelope::Body::Action(ActionRequest {
            credentials: Some(CredentialsEnvelope {
                entries: vec![Credential {
                    server_id: "ghcr".to_string(),
                    username: String::new(),
                    secret: Some(credential::Secret::Token(
                        "ghp_AAAAAAAAAAAAAAAAAAAA-bbb".to_string(),
                    )),
                }],
            }),
            ..sample_action_request()
        })),
    };
    roundtrip_assert_bytes_equal(&mut client, env).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn roundtrip_credentials_ssh_key() {
    let (server, _dir) = spawn_echo();
    let mut client = UdsTransport::connect(server.socket_path())
        .await
        .expect("connect");

    let env = Envelope {
        version: 1,
        request_id: 9,
        body: Some(envelope::Body::Action(ActionRequest {
            credentials: Some(CredentialsEnvelope {
                entries: vec![Credential {
                    server_id: "scm.example".to_string(),
                    username: "git".to_string(),
                    secret: Some(credential::Secret::SshKey(SshKey {
                        private_key_pem: b"-----BEGIN OPENSSH PRIVATE KEY-----\nfake\n-----END OPENSSH PRIVATE KEY-----\n".to_vec(),
                        passphrase: "passphrase-also-decrypted".to_string(),
                    })),
                }],
            }),
            ..sample_action_request()
        })),
    };
    roundtrip_assert_bytes_equal(&mut client, env).await;
}

// ---------------------------------------------------------------------------
// 7. Secure (0600 UDS) round-trip — proves the auth ceremony stays
//    compatible with the Java echo server.
// ---------------------------------------------------------------------------
//
// The Java side doesn't enforce the 0600 mode; it just binds whatever
// path the harness gives it. What this test proves is that the Rust
// `UdsTransport::connect_secure` ceremony — pre-connect stat(2),
// connect, post-connect SO_PEERCRED — works against a non-Rust peer
// that's running under the same UID. That matters because the
// barback daemon's listener is the same Java code (in M4.2), and the
// CLI's secure connect is the production path.

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn secure_connect_works_against_java_owner_uid_peer() {
    use barista_ipc::auth::SocketPath;
    use std::os::unix::fs::PermissionsExt;

    // Build a SocketPath under a tempdir so we don't interfere with
    // the user's real ~/.barista/run/. The Java side binds at the
    // path SocketPath::as_path() resolves to, and we then chmod the
    // socket inode to 0600 by hand (Java's
    // ServerSocketChannel.bind doesn't honor the parent dir's mode
    // policy the way UdsTransport::bind_secure does).
    let dir = TempDir::new().expect("tempdir");
    let socket_path = SocketPath::new_in(dir.path(), "secure").expect("SocketPath::new_in");
    let server = JavaEchoServer::spawn(socket_path.as_path().to_path_buf());

    // Tighten the inode perms to 0600 to satisfy the policy check.
    // The Java side doesn't do this; in production, the daemon's
    // listener would (or, equivalently, the Rust bind_secure path
    // would). We're proving the *client* half of the ceremony works
    // against any conforming peer.
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(socket_path.as_path(), perms).expect("chmod 0600 on socket inode");

    let mut client = UdsTransport::connect_secure(&socket_path)
        .await
        .expect("connect_secure should succeed against an owner-UID peer");

    let env = Envelope {
        version: 1,
        request_id: 0xABCD,
        body: Some(envelope::Body::Ping(Ping {
            client: "barista-secure-conformance".to_string(),
            sent_at_unix_micros: 42,
        })),
    };
    roundtrip_assert_bytes_equal(&mut client, env).await;
    drop(client);
    let _ = server.shutdown();
}

// ---------------------------------------------------------------------------
// 8. Ensure clean peer-close path matches expectation.
// ---------------------------------------------------------------------------
//
// When the client drops, the Java side's blocking read on the length
// header returns -1 (EOF at a frame boundary), the serve() loop
// exits, and the JVM terminates cleanly. This is the same "clean
// EOF" path covered in the Rust-only `recv_returns_closed_on_clean_peer_drop`
// test, asserted here against the Java implementation.

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn client_close_terminates_java_server_cleanly() {
    let (server, _dir) = spawn_echo();
    let mut client = UdsTransport::connect(server.socket_path())
        .await
        .expect("connect");
    // Send + recv one frame so the loop has consumed at least one
    // iteration before we drop.
    let env = Envelope {
        version: 1,
        request_id: 1,
        body: Some(envelope::Body::Ping(Ping {
            client: "barista".to_string(),
            sent_at_unix_micros: 0,
        })),
    };
    roundtrip_assert_bytes_equal(&mut client, env).await;
    drop(client);

    // After the client drops, the Java server's read on the next
    // length header sees EOF and serve() returns. We don't enforce a
    // timeout here — shutdown_inner waits up to 5s.
    let status = server.shutdown();
    assert!(
        status.success(),
        "Java echo server should exit 0 after clean client close; got {status:?}",
    );
}

// ---------------------------------------------------------------------------
// 9. Mux ↔ Java echo smoke test (optional but recommended per T7 spec).
// ---------------------------------------------------------------------------
//
// Wire the Rust mux layer to the Java echo server. The Java side only
// echoes Envelopes verbatim — it doesn't implement the full
// multiplexer semantics — but a *single* ActionRequest sent via
// `MuxClient::submit_action` should round-trip as an Envelope
// containing the same body. The mux layer's reader task then
// dispatches the echoed envelope back through the per-action
// inbound channel iff the action_id matches.
//
// Why this isn't redundant with the per-variant action test above: it
// exercises the mux layer's UUIDv7 action-id allocation, the
// outbound-channel routing, and the reader-task dispatch logic *in
// situ* against a non-Rust peer. The mux's own integration tests
// (tests/mux_*.rs) cover the same code paths but against a Rust
// echo server.

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK; run with `cargo test --test conformance -- --ignored`"]
async fn mux_submit_action_roundtrips_through_java_echo() {
    use barista_ipc::Multiplexer;

    let (server, _dir) = spawn_echo();
    let transport = UdsTransport::connect(server.socket_path())
        .await
        .expect("connect");
    let (_mux, mux_client, _mux_server) = Multiplexer::spawn(transport);

    // The mux client rewrites action_id with a fresh UUIDv7; we send
    // an action and observe what comes back via the per-action
    // inbound channel. The Java side echoes the Envelope verbatim,
    // so when the reader task dispatches by action_id, the matching
    // entry routes the echo back to our handle.
    //
    // The echoed envelope is an ActionRequest (not a Stream / Result /
    // Progress / Error), so the mux's reader_loop won't currently
    // route it to a per-action channel — `dispatch_inbound` treats
    // inbound `Action` as a server-side accept. That's fine for the
    // smoke test: we just need to prove the writer task's outbound
    // path serialised the Envelope correctly and the Java side
    // produced a parseable echo. The latter is observable via the
    // mux server's `next_action` channel.
    let req = sample_action_request();
    let _handle = mux_client.submit_action(req).await.expect("submit_action");

    // The Java echo sends the same ActionRequest envelope back. The
    // mux's reader_loop calls handle_inbound_action which pushes to
    // the IncomingAction channel.
    let incoming = tokio::time::timeout(Duration::from_secs(5), _mux_server.next_action())
        .await
        .expect("incoming action within timeout")
        .expect("MuxServer poll")
        .expect("incoming action present");

    // The echo'd ActionRequest carries the same UUIDv7 action_id the
    // mux client minted. We don't assert on a specific value (it's
    // freshly minted per call) — what we assert is that the round-
    // trip preserved the body shape, which is the cross-language
    // contract under test.
    let echoed_id = incoming.request().action_id.clone();
    assert!(
        !echoed_id.is_empty(),
        "echoed action_id should be the UUIDv7 the mux client minted",
    );
}
