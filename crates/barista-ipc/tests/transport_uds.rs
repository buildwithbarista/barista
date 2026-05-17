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

//! UDS round-trip tests for the framed transport.
//!
//! Each test spawns a server task that listens on a unique
//! per-test socket path under a `tempfile::TempDir`, accepts one
//! client, and either echoes a frame or pipes a sequence of
//! `Envelope` messages back to the test thread. The test thread
//! drives the client side of `UdsTransport` and asserts the
//! decoded `Envelope` is byte-equal to the one sent.
//!
//! The coverage matrix is intentionally exhaustive:
//!
//!  1. **Empty Envelope** — bare `Envelope { version, request_id,
//!     body: None }` to exercise the smallest valid frame.
//!  2. **Each of the 11 `oneof body` variants** — proves the wire
//!     discriminator is preserved on every variant.
//!  3. **Bidirectional traffic** — both sides send + receive in the
//!     same connection.
//!  4. **Multiple frames on the same connection** — proves the
//!     codec maintains framing state correctly across consecutive
//!     reads / writes.
//!  5. **Clean peer-close** — the receiver gets `TransportError::
//!     Closed` (not `Io`) when the sender drops the transport.
//!  6. **Large but legal frame** — a `~1 MiB` `ActionStream.payload`
//!     to exercise multi-iteration reads under the codec.

use std::collections::HashMap;
use std::path::PathBuf;

use barista_ipc::{
    ActionRequest, ActionResult, ActionStream, CancelRequest, Credential, CredentialsEnvelope,
    Envelope, Error, Mojo, Ping, Pong, ProducedArtifact, ProgressEvent, Shutdown, SshKey,
    StatusRequest, StatusResponse, Transport, TransportError, action_result, credential,
    envelope, progress_event,
    transport::uds::UdsTransport,
};
use tempfile::TempDir;
use tokio::net::UnixListener;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a temp-dir + a short socket path inside it. macOS limits
/// `sun_path` to 104 chars; we keep the leaf name to one character to
/// stay clear of that bound when the tempdir lives in `$TMPDIR`.
fn temp_socket_path() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir creation should succeed");
    let path = dir.path().join("s");
    (dir, path)
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
        action_id: "act-1234".to_string(),
        mojo_coords: "org.apache.maven.plugins:maven-compiler-plugin:3.11.0:compile".to_string(),
        project_root: "/work/proj".to_string(),
        pom_path: "/work/proj/pom.xml".to_string(),
        effective_pom_blob: vec![0xa1, 0x62, 0x69, 0x64, 0x01],
        classpath: vec!["/cas/a.jar".to_string()],
        plugin_classpath: vec![],
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

/// All 11 `oneof body` variants, in the same order as the proto schema.
/// Each entry carries enough non-default state to exercise its
/// discriminator on the wire.
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

/// Spawn a server task that:
///
/// 1. Binds a `UnixListener` at `path`.
/// 2. Signals readiness by sending `()` on the ready channel.
/// 3. Accepts exactly one client.
/// 4. Wraps the accepted stream in a `UdsTransport`.
/// 5. Echoes back every `Envelope` it receives until the client
///    drops, at which point it returns.
///
/// Returns the JoinHandle so the test can await termination and
/// surface any panic from the server side.
async fn spawn_echo_server(path: PathBuf) -> tokio::task::JoinHandle<()> {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let listener = UnixListener::bind(&path).expect("listener bind");
        ready_tx.send(()).expect("ready signal");
        let (stream, _addr) = listener.accept().await.expect("accept");
        let mut server = UdsTransport::from_stream(stream);
        loop {
            match server.recv().await {
                Ok(env) => server.send(env).await.expect("server send"),
                Err(TransportError::Closed) => return,
                Err(e) => panic!("server recv error: {e:?}"),
            }
        }
    });
    ready_rx.await.expect("server ready");
    handle
}

// ---------------------------------------------------------------------------
// 1. Round-trip every Envelope variant on its own connection.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn roundtrip_empty_envelope() {
    let (_tmp, path) = temp_socket_path();
    let server = spawn_echo_server(path.clone()).await;

    let mut client = UdsTransport::connect(&path).await.expect("connect");
    let env = Envelope {
        version: 1,
        request_id: 42,
        body: None,
    };
    client.send(env.clone()).await.expect("client send");
    let echoed = client.recv().await.expect("client recv");
    assert_eq!(env, echoed);

    drop(client);
    server.await.expect("server join");
}

#[tokio::test]
async fn roundtrip_body_ping() {
    roundtrip_one_variant(0).await;
}

#[tokio::test]
async fn roundtrip_body_pong() {
    roundtrip_one_variant(1).await;
}

#[tokio::test]
async fn roundtrip_body_action() {
    roundtrip_one_variant(2).await;
}

#[tokio::test]
async fn roundtrip_body_stream() {
    roundtrip_one_variant(3).await;
}

#[tokio::test]
async fn roundtrip_body_result() {
    roundtrip_one_variant(4).await;
}

#[tokio::test]
async fn roundtrip_body_progress() {
    roundtrip_one_variant(5).await;
}

#[tokio::test]
async fn roundtrip_body_cancel() {
    roundtrip_one_variant(6).await;
}

#[tokio::test]
async fn roundtrip_body_shutdown() {
    roundtrip_one_variant(7).await;
}

#[tokio::test]
async fn roundtrip_body_status_request() {
    roundtrip_one_variant(8).await;
}

#[tokio::test]
async fn roundtrip_body_status() {
    roundtrip_one_variant(9).await;
}

#[tokio::test]
async fn roundtrip_body_error() {
    roundtrip_one_variant(10).await;
}

/// Drive a single variant through a fresh echo server.
async fn roundtrip_one_variant(index: usize) {
    let bodies = all_body_variants();
    let body = bodies.into_iter().nth(index).expect("variant index in range");

    let (_tmp, path) = temp_socket_path();
    let server = spawn_echo_server(path.clone()).await;

    let mut client = UdsTransport::connect(&path).await.expect("connect");
    let env = Envelope {
        version: 1,
        request_id: index as u64 + 100,
        body: Some(body),
    };
    client.send(env.clone()).await.expect("client send");
    let echoed = client.recv().await.expect("client recv");
    assert_eq!(env, echoed, "variant {index} should round-trip");

    drop(client);
    server.await.expect("server join");
}

// ---------------------------------------------------------------------------
// 2. All 11 variants on a single connection.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn roundtrip_all_variants_one_connection() {
    let (_tmp, path) = temp_socket_path();
    let server = spawn_echo_server(path.clone()).await;

    let mut client = UdsTransport::connect(&path).await.expect("connect");
    for (i, body) in all_body_variants().into_iter().enumerate() {
        let env = Envelope {
            version: 1,
            request_id: i as u64,
            body: Some(body),
        };
        client.send(env.clone()).await.expect("send");
        let echoed = client.recv().await.expect("recv");
        assert_eq!(env, echoed, "variant {i} on shared connection");
    }

    drop(client);
    server.await.expect("server join");
}

// ---------------------------------------------------------------------------
// 3. Many sequential frames on a single connection.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn roundtrip_many_pings() {
    let (_tmp, path) = temp_socket_path();
    let server = spawn_echo_server(path.clone()).await;

    let mut client = UdsTransport::connect(&path).await.expect("connect");
    for i in 0..256u64 {
        let env = Envelope {
            version: 1,
            request_id: i,
            body: Some(envelope::Body::Ping(Ping {
                client: format!("barista 0.1.0 #{i}"),
                sent_at_unix_micros: i64::try_from(i).unwrap(),
            })),
        };
        client.send(env.clone()).await.expect("send");
        let echoed = client.recv().await.expect("recv");
        assert_eq!(env, echoed, "ping {i}");
    }

    drop(client);
    server.await.expect("server join");
}

// ---------------------------------------------------------------------------
// 4. Clean peer-close surfaces TransportError::Closed.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recv_returns_closed_on_clean_peer_drop() {
    let (_tmp, path) = temp_socket_path();
    let path_for_server = path.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let listener = UnixListener::bind(&path_for_server).expect("listener bind");
        ready_tx.send(()).expect("ready signal");
        let (stream, _addr) = listener.accept().await.expect("accept");
        let server = UdsTransport::from_stream(stream);
        drop(server);
    });
    ready_rx.await.expect("server ready");

    let mut client = UdsTransport::connect(&path).await.expect("connect");
    let result = client.recv().await;
    assert!(
        matches!(result, Err(TransportError::Closed)),
        "expected Closed on clean peer drop, got: {result:?}"
    );
    server.await.expect("server join");
}

// ---------------------------------------------------------------------------
// 5. Bidirectional pingpong with distinct request_ids.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bidirectional_ping_pong() {
    let (_tmp, path) = temp_socket_path();
    let path_for_server = path.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let listener = UnixListener::bind(&path_for_server).expect("listener bind");
        ready_tx.send(()).expect("ready signal");
        let (stream, _addr) = listener.accept().await.expect("accept");
        let mut server = UdsTransport::from_stream(stream);
        // Server expects a Ping, responds with a Pong, then closes.
        let ping = server.recv().await.expect("server recv");
        let body = ping.body.expect("ping body");
        match body {
            envelope::Body::Ping(p) => {
                let pong = Envelope {
                    version: 1,
                    request_id: ping.request_id,
                    body: Some(envelope::Body::Pong(Pong {
                        daemon: "barback 0.1.0".to_string(),
                        jdk_id: "temurin-21".to_string(),
                        jdk_version: "21.0.4".to_string(),
                        server_unix_micros: 99,
                        client_unix_micros: p.sent_at_unix_micros,
                    })),
                };
                server.send(pong).await.expect("server send pong");
            }
            other => panic!("expected Ping, got: {other:?}"),
        }
    });
    ready_rx.await.expect("server ready");

    let mut client = UdsTransport::connect(&path).await.expect("connect");
    let ping = Envelope {
        version: 1,
        request_id: 7,
        body: Some(envelope::Body::Ping(Ping {
            client: "barista 0.1.0".to_string(),
            sent_at_unix_micros: 1_700_000_000_000_000,
        })),
    };
    client.send(ping).await.expect("client send ping");
    let pong = client.recv().await.expect("client recv pong");
    assert_eq!(pong.request_id, 7, "pong echoes ping's request_id");
    match pong.body.expect("pong body") {
        envelope::Body::Pong(p) => {
            assert_eq!(p.daemon, "barback 0.1.0");
            assert_eq!(p.client_unix_micros, 1_700_000_000_000_000);
        }
        other => panic!("expected Pong, got: {other:?}"),
    }
    server.await.expect("server join");
}

// ---------------------------------------------------------------------------
// 6. A reasonably large but legal frame (~1 MiB) round-trips.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn large_legal_frame_roundtrips() {
    let (_tmp, path) = temp_socket_path();
    let server = spawn_echo_server(path.clone()).await;
    let mut client = UdsTransport::connect(&path).await.expect("connect");

    // 1 MiB of payload bytes — well under the 16 MiB cap but large
    // enough to exercise multi-poll reads under the codec.
    let big = vec![0xABu8; 1024 * 1024];
    let env = Envelope {
        version: 1,
        request_id: 1,
        body: Some(envelope::Body::Stream(ActionStream {
            stream_id: 9,
            payload: big.clone(),
            end: true,
            action_id: "act-big".to_string(),
        })),
    };
    client.send(env.clone()).await.expect("send");
    let echoed = client.recv().await.expect("recv");
    assert_eq!(env, echoed);
    match echoed.body.expect("body") {
        envelope::Body::Stream(s) => assert_eq!(s.payload.len(), big.len()),
        other => panic!("expected Stream, got: {other:?}"),
    }

    drop(client);
    server.await.expect("server join");
}

// ---------------------------------------------------------------------------
// 7. Credentials envelope round-trip (zeroize doesn't break wire shape).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn credentials_envelope_roundtrips() {
    let (_tmp, path) = temp_socket_path();
    let server = spawn_echo_server(path.clone()).await;
    let mut client = UdsTransport::connect(&path).await.expect("connect");

    // ActionRequest with attached credentials — the realistic payload
    // shape from M3.x. We're proving that ZeroizeOnDrop derives on
    // the cred types don't interfere with prost's encode/decode path.
    let mut req = sample_action_request();
    req.credentials = Some(CredentialsEnvelope {
        entries: vec![Credential {
            server_id: "central".to_string(),
            username: "deploy-bot".to_string(),
            secret: Some(credential::Secret::SshKey(SshKey {
                private_key_pem: b"-----BEGIN OPENSSH PRIVATE KEY-----...".to_vec(),
                passphrase: "phrase".to_string(),
            })),
        }],
    });
    let env = Envelope {
        version: 1,
        request_id: 5,
        body: Some(envelope::Body::Action(req)),
    };
    client.send(env.clone()).await.expect("send");
    let echoed = client.recv().await.expect("recv");
    assert_eq!(env, echoed);

    drop(client);
    server.await.expect("server join");
}

// ---------------------------------------------------------------------------
// 8. Multiple concurrent connections to the same listener.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multiple_concurrent_clients() {
    let (_tmp, path) = temp_socket_path();
    let path_for_server = path.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let listener = UnixListener::bind(&path_for_server).expect("listener bind");
        ready_tx.send(()).expect("ready signal");
        for _ in 0..4 {
            let (stream, _addr) = listener.accept().await.expect("accept");
            tokio::spawn(async move {
                let mut t = UdsTransport::from_stream(stream);
                loop {
                    match t.recv().await {
                        Ok(env) => t.send(env).await.expect("server-side send"),
                        Err(TransportError::Closed) => return,
                        Err(e) => panic!("server recv error: {e:?}"),
                    }
                }
            });
        }
    });
    ready_rx.await.expect("server ready");

    let mut handles = vec![];
    for i in 0..4u64 {
        let p = path.clone();
        handles.push(tokio::spawn(async move {
            let mut client = UdsTransport::connect(&p).await.expect("connect");
            let env = Envelope {
                version: 1,
                request_id: i,
                body: Some(envelope::Body::Ping(Ping {
                    client: format!("client-{i}"),
                    sent_at_unix_micros: i64::try_from(i).unwrap(),
                })),
            };
            client.send(env.clone()).await.expect("send");
            let echoed = client.recv().await.expect("recv");
            assert_eq!(env, echoed);
            drop(client);
        }));
    }
    for h in handles {
        h.await.expect("client task");
    }
    server.await.expect("server join");
}

// ---------------------------------------------------------------------------
// 9. Connect to a nonexistent socket returns Io.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connect_to_missing_socket_errors() {
    let (_tmp, path) = temp_socket_path();
    // Path exists (the tempdir does) but no socket has been bound.
    let result = UdsTransport::connect(&path).await;
    match result {
        Err(TransportError::Io(e)) => {
            // ENOENT on Linux, "no such file or directory" on macOS.
            assert!(
                matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ),
                "expected NotFound/ConnectionRefused, got: {:?}",
                e.kind()
            );
        }
        other => panic!("expected Io error, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 10. inner() exposes the underlying UnixStream.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inner_exposes_peer_cred() {
    let (_tmp, path) = temp_socket_path();
    let server = spawn_echo_server(path.clone()).await;

    let client = UdsTransport::connect(&path).await.expect("connect");
    // The diagnostic surface we promise in `inner()`'s doc-comment is
    // `peer_cred` and friends. We don't assert on the credential
    // contents (they're platform-specific) — only that the call
    // doesn't panic and returns something.
    let cred = client.inner().peer_cred();
    assert!(cred.is_ok(), "peer_cred should succeed on a live UDS");

    drop(client);
    server.await.expect("server join");
}
