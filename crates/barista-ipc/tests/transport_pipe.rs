// Integration-test target — workspace security lints are allowed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]
#![cfg(windows)]

//! Named-pipe round-trip tests, mirroring `transport_uds.rs`.
//!
//! These tests are `#[cfg(windows)]`-gated and only run on the
//! `windows-latest` CI runner from M0.1 T13. On macOS / Linux dev
//! hosts the entire file is excluded from compilation. The test set
//! is intentionally narrower than the UDS file — once the cross-host
//! parity is established (every `Envelope` variant round-trips on
//! both transports), the broader edge-case coverage in
//! `transport_framing.rs` carries the burden, and it runs on UDS
//! because the framing logic is byte-identical.

use std::collections::HashMap;

use barista_ipc::{
    ActionRequest, ActionResult, ActionStream, CancelRequest, Envelope, Error, Mojo, Ping, Pong,
    ProducedArtifact, ProgressEvent, Shutdown, StatusRequest, StatusResponse, Transport,
    TransportError, action_result, envelope, progress_event,
    transport::pipe::NamedPipeTransport,
};
use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

/// Build a unique pipe name per test to avoid collisions when tests
/// run in parallel under `cargo test`.
fn unique_pipe_name(test_id: &str) -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(r"\\.\pipe\barista-ipc-test-{test_id}-{pid}-{nanos}")
}

fn sample_mojo() -> Mojo {
    Mojo {
        group_id: "g".to_string(),
        artifact_id: "a".to_string(),
        version: "1".to_string(),
        goal: "compile".to_string(),
        execution_id: "default".to_string(),
    }
}

fn sample_action_request() -> ActionRequest {
    ActionRequest {
        action_id: "act-1234".to_string(),
        mojo_coords: "g:a:1:compile".to_string(),
        project_root: "C:\\work\\proj".to_string(),
        pom_path: "C:\\work\\proj\\pom.xml".to_string(),
        effective_pom_blob: vec![0xa1, 0x62, 0x69, 0x64, 0x01],
        classpath: vec![],
        plugin_classpath: vec![],
        system_properties: HashMap::new(),
        environment: HashMap::new(),
        working_directory: "C:\\work\\proj".to_string(),
        stdout_stream_id: 1,
        stderr_stream_id: 2,
        quiet: false,
        maven_compat: "3".to_string(),
        jvm_args: vec![],
        credentials: None,
    }
}

fn all_body_variants() -> Vec<envelope::Body> {
    vec![
        envelope::Body::Ping(Ping {
            client: "barista".to_string(),
            sent_at_unix_micros: 1,
        }),
        envelope::Body::Pong(Pong {
            daemon: "barback".to_string(),
            jdk_id: "temurin-21".to_string(),
            jdk_version: "21.0.4".to_string(),
            server_unix_micros: 2,
            client_unix_micros: 1,
        }),
        envelope::Body::Action(sample_action_request()),
        envelope::Body::Stream(ActionStream {
            stream_id: 1,
            payload: b"out".to_vec(),
            end: false,
            action_id: "a1".to_string(),
        }),
        envelope::Body::Result(ActionResult {
            action_id: "a1".to_string(),
            status: action_result::Status::Success as i32,
            exit_code: 0,
            duration_micros: 1000,
            artifacts: vec![ProducedArtifact {
                path: "C:\\out\\foo.jar".to_string(),
                size_bytes: 1,
                sha256: "x".to_string(),
            }],
            failure_message: String::new(),
            failure_stack: String::new(),
            attributes: HashMap::new(),
            error: None,
        }),
        envelope::Body::Progress(ProgressEvent {
            kind: progress_event::Kind::Started as i32,
            action_id: "a1".to_string(),
            timestamp: "t".to_string(),
            coord: String::new(),
            phase: "p".to_string(),
            progress: 0.0,
            mojo: Some(sample_mojo()),
            details: HashMap::new(),
        }),
        envelope::Body::Cancel(CancelRequest {
            action_id: "a1".to_string(),
            grace_period_ms: 1000,
        }),
        envelope::Body::Shutdown(Shutdown { drain_seconds: 1 }),
        envelope::Body::StatusRequest(StatusRequest {}),
        envelope::Body::Status(StatusResponse {
            uptime_seconds: 1,
            workers_total: 1,
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
            message: "m".to_string(),
            details: HashMap::new(),
            action_id: String::new(),
        }),
    ]
}

// ---------------------------------------------------------------------------
// Round-trip every variant on a single named-pipe connection.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn roundtrip_all_variants_one_pipe() {
    let pipe_name = unique_pipe_name("roundtrip");
    let pipe_name_for_server = pipe_name.clone();

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let server_pipe = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name_for_server)
            .expect("create named pipe");
        ready_tx.send(()).expect("ready signal");
        server_pipe.connect().await.expect("server connect");
        let mut t = NamedPipeTransport::from_server(server_pipe);
        loop {
            match t.recv().await {
                Ok(env) => t.send(env).await.expect("server-side send"),
                Err(TransportError::Closed) => return,
                Err(e) => panic!("server recv: {e:?}"),
            }
        }
    });
    ready_rx.await.expect("server ready");

    let mut client = NamedPipeTransport::<tokio::net::windows::named_pipe::NamedPipeClient>::connect(&pipe_name)
        .await
        .expect("client connect");
    for (i, body) in all_body_variants().into_iter().enumerate() {
        let env = Envelope {
            version: 1,
            request_id: i as u64,
            body: Some(body),
        };
        client.send(env.clone()).await.expect("client send");
        let echoed = client.recv().await.expect("client recv");
        assert_eq!(env, echoed, "variant {i} round-trip on named pipe");
    }

    drop(client);
    server.await.expect("server join");
}

// ---------------------------------------------------------------------------
// Connect to a nonexistent pipe surfaces Io.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connect_to_missing_pipe_errors() {
    let result = NamedPipeTransport::<tokio::net::windows::named_pipe::NamedPipeClient>::connect(
        r"\\.\pipe\barista-ipc-test-nonexistent-pipe-9876543210",
    )
    .await;
    match result {
        Err(TransportError::Io(_)) => {}
        other => panic!("expected Io on missing pipe, got: {other:?}"),
    }
}
