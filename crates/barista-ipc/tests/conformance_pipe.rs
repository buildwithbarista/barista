// SPDX-License-Identifier: MIT OR Apache-2.0

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
#![cfg(windows)]

//! Cross-language Rust↔Java conformance tests for the worker IPC wire
//! protocol over **Windows named pipes**.
//!
//! Companion to `tests/conformance.rs` (UDS, `#[cfg(unix)]`). The two
//! suites pin the same `Envelope.body` variants and the same wire
//! contract (4-byte BE length prefix + protobuf payload, 16 MiB max
//! frame); the difference is purely the underlying transport.
//!
//! # Role inversion (vs UDS)
//!
//! On UDS, the Java side binds and the Rust side connects. On named
//! pipes, the topology is **inverted**: the Rust side binds (so it
//! owns the DACL installed by
//! [`barista_ipc::transport::pipe::NamedPipeTransport::bind_secure`]),
//! and the Java side opens the pipe via
//! `RandomAccessFile(\\.\pipe\<name>, "rw")`. Inversion is
//! intentional — JNI'ing `CreateNamedPipeW` with the same
//! `SECURITY_ATTRIBUTES` dance as the Rust `bind_secure` would be
//! ugly, brittle, and would duplicate the security model. Keeping
//! the DACL on one side (Rust) keeps the auth story unambiguous.
//!
//! # Why `RandomAccessFile` for Java?
//!
//! The Win32 pipe namespace surfaces as a filesystem path
//! (`\\.\pipe\<name>`); opening such a path with
//! `new RandomAccessFile(path, "rw")` is a synchronous
//! `CreateFileW(GENERIC_READ | GENERIC_WRITE)` under the hood. That
//! matches what the Rust side's `tokio::net::windows::named_pipe::
//! NamedPipeClient` does (minus the async wrapper). The alternative
//! — `AsynchronousFileChannel` — would allow overlapping IO against
//! the pipe, but each test here drives exactly one round-trip per
//! envelope, so a blocking-IO model is the simpler fit. See
//! `barback/src/test/java/.../conformance/EchoPipeClient.java` for
//! the Java side's full rationale.
//!
//! # What's missing vs `conformance.rs` (UDS) and why
//!
//! The UDS suite covers a **32-concurrent in-flight** ordering test
//! (`concurrent_32_inflight_preserves_order`). That test relies on a
//! single connection handling multiple in-flight envelopes — UDS
//! supports this trivially since it's stream-of-bytes on both halves.
//! Windows named pipes, by contrast, are *channel-oriented*: each
//! pipe instance is a 1-to-1 conversation. Multi-client concurrency
//! on the daemon would use multi-instance pipes (a server pool of
//! `NamedPipeServer`s under the same name), not a single instance
//! shared across in-flight messages. That's a daemon-architecture
//! topic (M4.2), not a wire-format topic — so the
//! 32-concurrent test is deferred to M4.2's daemon test plan, not
//! mirrored here. The 22-test UDS coverage minus that one test gives
//! us 21 conformance tests on the pipe side; we also drop the
//! `mux_submit_action_roundtrips_through_java_echo` smoke (which
//! exercises the Rust mux layer end-to-end via the Java echo) for
//! the same reason — the Rust mux is transport-agnostic and the UDS
//! variant already covers it; the named-pipe daemon variant lands
//! with M4.2.
//!
//! Final tally: **20 named-pipe conformance tests**, plus the
//! deferral marker (`pipe_concurrency_is_m42_topic`).
//!
//! # Why `#[ignore]`?
//!
//! These tests require Maven + a JDK installed on the host. CI has
//! both (see `.github/workflows/ci.yml`'s `rust-windows` job); local
//! dev may not, and the suite is gated on `#[cfg(windows)]` so it
//! only ever runs on the Windows CI runner. Manual invocation:
//!
//! ```bash
//!   cargo test -p barista-ipc --test conformance_pipe -- --ignored
//! ```

// Helpers live in `tests/conformance_helpers/mod.rs`. The submodule is
// named distinctly from this file so `cargo test` doesn't see the
// `tests/conformance_pipe/` directory as a sibling integration-test
// target (Cargo treats each top-level file/directory under `tests/`
// as its own binary).
mod conformance_helpers;

use std::collections::HashMap;
use std::time::Duration;

use barista_ipc::transport::pipe::NamedPipeTransport;
use barista_ipc::{
    ActionRequest, ActionResult, ActionStream, CancelRequest, Credential, CredentialsEnvelope,
    Envelope, Error, MAX_FRAME_BYTES, Mojo, Ping, Pong, ProducedArtifact, ProgressEvent, Shutdown,
    SshKey, StatusRequest, StatusResponse, Transport, action_result, credential, envelope,
    progress_event,
};
use prost::Message;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use crate::conformance_helpers::{JavaEchoPipeClient, unique_test_pipe_name};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Bind a fresh `NamedPipeServer` at a unique path and spawn the Java
/// echo client against it. Returns the connected server-side transport
/// plus the JVM handle the test owns until it drops.
///
/// The bind happens first so the Java side's `RandomAccessFile` open
/// finds a listening pipe immediately (otherwise it would race the
/// spawn-launch latency and fail with `ERROR_FILE_NOT_FOUND`).
async fn spawn_echo(test_id: &str) -> (NamedPipeTransport<NamedPipeServer>, JavaEchoPipeClient) {
    let pipe_name = unique_test_pipe_name(test_id);
    // `first_pipe_instance(true)` rejects any subsequent server using
    // the same name — defence against test-collision flakes.
    let server_pipe = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_name)
        .expect("ServerOptions::create");
    // Spawn Java now; it will open the pipe and emit `READY <pipe>`.
    let client = JavaEchoPipeClient::spawn(pipe_name.clone());
    // Java has already opened the pipe by the time it printed READY,
    // so `connect()` resolves immediately (either the kernel has
    // queued the open, or `ConnectNamedPipe` returns
    // ERROR_PIPE_CONNECTED which tokio treats as success).
    server_pipe.connect().await.expect("server pipe connect");
    let transport = NamedPipeTransport::from_server(server_pipe);
    (transport, client)
}

/// Variant of [`spawn_echo`] that uses
/// [`NamedPipeTransport::bind_secure`] (DACL installed) instead of the
/// plain `ServerOptions::create`. Used by the secure-connect
/// conformance test.
async fn spawn_echo_secure(
    test_id: &str,
) -> (NamedPipeTransport<NamedPipeServer>, JavaEchoPipeClient) {
    use barista_ipc::auth::PipeName;
    // PipeName builds `\\.\pipe\barista\<name>`. We use a unique
    // sub-leaf so concurrent test runs don't collide.
    let leaf = format!(
        "test-secure-{test_id}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    let pn = PipeName::new(&leaf);
    let server_pipe = NamedPipeTransport::<NamedPipeServer>::bind_secure(&pn)
        .expect("bind_secure should succeed on same-user process");
    let client = JavaEchoPipeClient::spawn(pn.as_str().to_string());
    server_pipe.connect().await.expect("server pipe connect");
    let transport = NamedPipeTransport::from_server(server_pipe);
    (transport, client)
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
        // Windows-flavoured paths so the test isn't trivially passing
        // on cross-platform string handling.
        project_root: "C:\\work\\proj".to_string(),
        pom_path: "C:\\work\\proj\\pom.xml".to_string(),
        effective_pom_blob: vec![0xa1, 0x62, 0x69, 0x64, 0x01],
        classpath: vec!["C:\\cas\\a.jar".to_string()],
        plugin_classpath: vec!["C:\\cas\\p.jar".to_string()],
        system_properties,
        environment,
        working_directory: "C:\\work\\proj".to_string(),
        stdout_stream_id: 1,
        stderr_stream_id: 2,
        quiet: false,
        maven_compat: "3".to_string(),
        jvm_args: vec!["-Xmx512m".to_string()],
        credentials: None,
    }
}

/// All 11 `Envelope.body` variants in the canonical schema order,
/// each carrying enough non-default state to exercise its
/// discriminator on the wire. Mirrors the helper in
/// `tests/conformance.rs` so the two suites pin the same
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
            payload: b"[INFO] Building...\r\n".to_vec(),
            end: false,
            action_id: "act-stream".to_string(),
        }),
        envelope::Body::Result(ActionResult {
            action_id: "act-result".to_string(),
            status: action_result::Status::Success as i32,
            exit_code: 0,
            duration_micros: 1000,
            artifacts: vec![ProducedArtifact {
                path: "C:\\work\\proj\\target\\foo.jar".to_string(),
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

/// Round-trip an envelope through the Java echo client, asserting
/// encoded-bytes equality on the reply. This is the canonical
/// equivalence: the Java side ran `parseFrom` + `toByteArray`, so
/// matching bytes prove the schemas agree on every field tag and
/// wire-type used in the payload.
async fn roundtrip_assert_bytes_equal(
    transport: &mut NamedPipeTransport<NamedPipeServer>,
    env: Envelope,
) {
    let sent_bytes = env.encode_to_vec();
    transport.send(env).await.expect("server send");
    let echoed = transport.recv().await.expect("server recv");
    let echoed_bytes = echoed.encode_to_vec();
    assert_eq!(
        sent_bytes, echoed_bytes,
        "encoded-bytes equality should hold after Java round-trip on named pipe",
    );
}

// ---------------------------------------------------------------------------
// 1. Per-variant round-trip — one test per Envelope.body variant.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_body_ping() {
    roundtrip_body_index(0, "ping").await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_body_pong() {
    roundtrip_body_index(1, "pong").await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_body_action() {
    roundtrip_body_index(2, "action").await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_body_stream() {
    roundtrip_body_index(3, "stream").await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_body_result() {
    roundtrip_body_index(4, "result").await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_body_progress() {
    roundtrip_body_index(5, "progress").await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_body_cancel() {
    roundtrip_body_index(6, "cancel").await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_body_shutdown() {
    roundtrip_body_index(7, "shutdown").await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_body_status_request() {
    roundtrip_body_index(8, "statusrequest").await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_body_status() {
    roundtrip_body_index(9, "status").await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_body_error() {
    roundtrip_body_index(10, "error").await;
}

async fn roundtrip_body_index(index: usize, test_id: &str) {
    let (mut transport, client) = spawn_echo(test_id).await;
    let body = all_body_variants()
        .into_iter()
        .nth(index)
        .expect("index in range");
    let env = Envelope {
        version: 1,
        request_id: 0x100 + index as u64,
        body: Some(body),
    };
    roundtrip_assert_bytes_equal(&mut transport, env).await;
    drop(transport);
    let status = client.shutdown();
    assert!(
        status.success(),
        "Java echo client should exit cleanly after server close; got {status:?}",
    );
}

// ---------------------------------------------------------------------------
// 2. Empty Envelope (body = None) — the smallest legal frame.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_empty_envelope() {
    let (mut transport, _client) = spawn_echo("empty").await;
    let env = Envelope {
        version: 1,
        request_id: 1,
        body: None,
    };
    roundtrip_assert_bytes_equal(&mut transport, env).await;
}

// ---------------------------------------------------------------------------
// 3. Small + near-cap ActionStream.payload exercise.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_actionstream_small_payload() {
    let (mut transport, _client) = spawn_echo("stream-small").await;
    let env = Envelope {
        version: 1,
        request_id: 9001,
        body: Some(envelope::Body::Stream(ActionStream {
            stream_id: 1,
            payload: b"hello cross-lang via named pipe\r\n".to_vec(),
            end: true,
            action_id: "act-stream-small".to_string(),
        })),
    };
    roundtrip_assert_bytes_equal(&mut transport, env).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_actionstream_near_cap_payload() {
    let (mut transport, _client) = spawn_echo("stream-big").await;

    // 16 MiB - 1 KiB of payload bytes — under the cap, but large
    // enough to exercise multi-iteration reads on both sides and the
    // codec's frame-assembly path. Leaving 1 KiB headroom for the
    // protobuf overhead so the encoded Envelope stays under
    // MAX_FRAME_BYTES.
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
    roundtrip_assert_bytes_equal(&mut transport, env).await;
}

// ---------------------------------------------------------------------------
// 4. Frame-too-large rejection (Java side).
// ---------------------------------------------------------------------------
//
// The Java EchoPipeClient enforces MAX_FRAME_BYTES = 16 MiB on its
// read path. To exercise that without a 17 MiB allocation, we bypass
// `NamedPipeTransport::send` (which enforces the cap on the sender
// side) and write a raw 4-byte BE length prefix announcing 17 MiB,
// followed by a single placeholder byte. The Java side reads the
// header, observes the oversized announcement, and closes the pipe.
// From the server's perspective that surfaces as EOF on the next
// recv.

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn frame_too_large_is_rejected_by_java() {
    use barista_ipc::TransportError;
    use tokio::io::AsyncWriteExt;

    let pipe_name = unique_test_pipe_name("toobig");
    let server_pipe = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_name)
        .expect("ServerOptions::create");
    let _client = JavaEchoPipeClient::spawn(pipe_name.clone());
    server_pipe.connect().await.expect("server pipe connect");

    // Bypass the framed codec: write the 4-byte BE length prefix +
    // one byte payload directly on the raw NamedPipeServer (which
    // implements AsyncWrite).
    let mut raw_server = server_pipe;
    let announced: u32 = 17 * 1024 * 1024;
    let hdr = announced.to_be_bytes();
    raw_server
        .write_all(&hdr)
        .await
        .expect("write oversized header");
    // Send a single byte after the prefix so the Java side has
    // something on the wire to trigger the cap check on the read of
    // the header (Java reads the int, then checks the cap before
    // allocating the body buffer).
    raw_server
        .write_all(&[0u8])
        .await
        .expect("write 1-byte stub");
    raw_server.flush().await.expect("flush oversized frame");

    // Now re-wrap in a transport and observe the close on recv. The
    // Java side closes the pipe on oversized-frame detection;
    // tokio's framed codec surfaces that as a Closed error on the
    // next recv attempt.
    let mut transport = NamedPipeTransport::from_server(raw_server);
    let result = tokio::time::timeout(Duration::from_secs(10), transport.recv()).await;
    let recv = result.expect("recv within timeout");
    match recv {
        Err(TransportError::Closed) | Err(TransportError::Io(_)) => {
            // Acceptable: clean EOF (Closed) or BrokenPipe (Io). Both
            // signal that the Java side hung up on the oversized
            // frame as designed.
        }
        Ok(env) => panic!(
            "expected Closed / Io after oversized prefix; \
             Java side instead echoed back: {env:?}",
        ),
        Err(other) => panic!("expected Closed / Io after oversized prefix; got: {other:?}",),
    }
}

// ---------------------------------------------------------------------------
// 5. Credentials envelope round-trip.
// ---------------------------------------------------------------------------
//
// The `CredentialsEnvelope` contract is schema-level: the Java side
// decodes it via the generated `CredentialsEnvelope.parseFrom` and
// re-emits via `toByteArray`. The Rust side's `ZeroizeOnDrop` derives
// on `Credential` / `CredentialsEnvelope` / `SshKey` are NOT exercised
// here — that's the job of `tests/auth_zeroize.rs`. This test just
// proves the wire shape for `CredentialsEnvelope { entries: [...] }`
// survives a Rust→Java→Rust round-trip with identical bytes, on the
// named-pipe transport.

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_credentials_password() {
    let (mut transport, _client) = spawn_echo("creds-pw").await;
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
    roundtrip_assert_bytes_equal(&mut transport, env).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_credentials_token() {
    let (mut transport, _client) = spawn_echo("creds-token").await;
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
    roundtrip_assert_bytes_equal(&mut transport, env).await;
}

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn roundtrip_credentials_ssh_key() {
    let (mut transport, _client) = spawn_echo("creds-ssh").await;
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
    roundtrip_assert_bytes_equal(&mut transport, env).await;
}

// ---------------------------------------------------------------------------
// 6. Secure (DACL'd) round-trip — proves the auth ceremony stays
//    compatible with the Java echo client.
// ---------------------------------------------------------------------------
//
// The Rust side binds the pipe with the per-user DACL installed
// (`bind_secure` → `CreateNamedPipeW(SECURITY_ATTRIBUTES)`); the
// Java client opens it via `CreateFileW`. Same-user processes are
// allowed by the DACL; everyone else gets ERROR_ACCESS_DENIED. The
// CI runner runs the harness as the runner's user, so the open
// succeeds and the round-trip completes.

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn secure_bind_works_against_java_same_user_client() {
    let (mut transport, client) = spawn_echo_secure("secure").await;

    let env = Envelope {
        version: 1,
        request_id: 0xABCD,
        body: Some(envelope::Body::Ping(Ping {
            client: "barista-secure-conformance-pipe".to_string(),
            sent_at_unix_micros: 42,
        })),
    };
    roundtrip_assert_bytes_equal(&mut transport, env).await;
    drop(transport);
    let _ = client.shutdown();
}

// ---------------------------------------------------------------------------
// 7. Clean peer-close termination.
// ---------------------------------------------------------------------------
//
// When the server (Rust) drops, the Java client's next `readInt()`
// raises EOFException, which the echo loop catches and returns from
// cleanly. `EchoPipeClientCli`'s try-with-resources then closes the
// `RandomAccessFile` and exits 0 (the stdin watchdog hasn't fired —
// we're closing from the bottom of `main`).

#[tokio::test]
#[ignore = "conformance: requires Maven + JDK + Windows; run with `cargo test --test conformance_pipe -- --ignored`"]
async fn server_close_terminates_java_client_cleanly() {
    let (mut transport, client) = spawn_echo("clean-close").await;
    // Send + recv one frame so the Java loop has consumed at least
    // one iteration before we drop.
    let env = Envelope {
        version: 1,
        request_id: 1,
        body: Some(envelope::Body::Ping(Ping {
            client: "barista".to_string(),
            sent_at_unix_micros: 0,
        })),
    };
    roundtrip_assert_bytes_equal(&mut transport, env).await;
    drop(transport);

    // After the server drops, the Java client's read on the next
    // length header sees EOF and `serve()` returns. The JVM exits
    // 0; we don't enforce a timeout here — `shutdown_inner` waits
    // up to 5 s.
    let status = client.shutdown();
    assert!(
        status.success(),
        "Java echo client should exit 0 after clean server close; got {status:?}",
    );
}

// ---------------------------------------------------------------------------
// 8. Deferral marker — surfaces the 32-concurrent / mux-smoke test
//    omission in test output so future readers know where to look.
// ---------------------------------------------------------------------------

#[test]
fn pipe_concurrency_is_m42_topic() {
    // This test exists only to surface the deferral in test output.
    // It always passes. The UDS variant runs a 32-in-flight ordering
    // test on a single connection because UDS is byte-stream-
    // oriented and one connection can carry overlapping messages
    // trivially. Windows named pipes are channel-oriented: each
    // pipe instance is a 1-to-1 conversation. Multi-client
    // concurrency on the daemon is a multi-instance pipe pool
    // (server-pool sharing a name), which is M4.2 daemon
    // architecture, not M4.1 wire-format scope.
    //
    // The Rust mux layer itself is transport-agnostic; its
    // correctness against an echo peer is already pinned by the
    // UDS variant `mux_submit_action_roundtrips_through_java_echo`.
    let _ = "multi-instance pipe pool is M4.2 daemon scope; see tests/conformance.rs for the UDS variant's 32-concurrent test";
}
