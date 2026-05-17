// Integration-test target — workspace security lints are allowed for
// the usual reasons (panic-loud-on-misuse, prost enum casts).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]
#![cfg(unix)]

//! End-to-end basic flow for the multiplex layer.
//!
//! Spins up a paired UDS connection (client + server), submits one
//! action from the client, watches the server's `MuxServer::next_action`
//! yield it, drives progress events + the final `ActionResult` back,
//! and confirms the client sees the events in order with no
//! corruption.

use std::path::PathBuf;
use std::time::Duration;

use barista_ipc::{
    ActionRequest, ActionResult, Multiplexer, ProgressEvent, StreamEvent, action_result,
    progress_event, transport::uds::UdsTransport,
};
use tempfile::TempDir;
use tokio::net::{UnixListener, UnixStream};

fn temp_socket_path() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("s");
    (dir, path)
}

/// Bring up a UDS pair + a multiplexer on each side. Returns the two
/// `(Multiplexer, MuxClient, MuxServer)` triples + the `TempDir` whose
/// lifetime must outlive the connection (Drop would unlink the
/// socket file, which is harmless after `accept` but we keep it pinned
/// for hygiene).
async fn paired() -> (
    TempDir,
    (Multiplexer, barista_ipc::MuxClient),
    (Multiplexer, barista_ipc::MuxServer),
) {
    let (tmp, path) = temp_socket_path();
    let path_for_server = path.clone();

    // Server: bind first, then accept the connection.
    let listener = UnixListener::bind(&path_for_server).expect("bind");
    let accept = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept");
        stream
    });

    // Client: dial.
    let client_stream = UnixStream::connect(&path).await.expect("client connect");
    let server_stream = accept.await.expect("accept join");

    let client_transport = UdsTransport::from_stream(client_stream);
    let server_transport = UdsTransport::from_stream(server_stream);

    let (client_mux, client_mc, _client_ms) = Multiplexer::spawn(client_transport);
    let (server_mux, _server_mc, server_ms) = Multiplexer::spawn(server_transport);

    (tmp, (client_mux, client_mc), (server_mux, server_ms))
}

#[tokio::test]
async fn submit_one_action_round_trips_progress_and_result() {
    let (_tmp, (_cmux, client), (_smux, server)) = paired().await;

    // Server-side handler: wait for one action, send 3 progress events
    // + a SUCCESS result, then stop.
    let server_handle = tokio::spawn(async move {
        let action = server
            .next_action()
            .await
            .expect("next_action")
            .expect("got incoming");
        let (req, response, _cancel) = action.split();
        // Echo the action_id on every progress event — the response
        // channel will overwrite, so any string works.
        for i in 0..3 {
            let ev = ProgressEvent {
                kind: progress_event::Kind::Started.into(),
                action_id: String::new(),
                timestamp: format!("2026-05-16T00:00:0{i}.000Z"),
                coord: String::new(),
                phase: format!("phase-{i}"),
                progress: f64::from(i) * 33.0,
                mojo: None,
                details: Default::default(),
            };
            response.send_progress(ev).await.expect("send_progress");
        }
        let result = ActionResult {
            action_id: String::new(),
            status: action_result::Status::Success.into(),
            exit_code: 0,
            duration_micros: 1_000,
            artifacts: Vec::new(),
            failure_message: String::new(),
            failure_stack: String::new(),
            attributes: Default::default(),
            error: None,
        };
        response.send_result(result).await.expect("send_result");
        // Return the request so the test can introspect what the
        // server saw (action_id is multiplexer-assigned).
        req
    });

    // Client side: submit + drain events.
    let req = ActionRequest {
        action_id: String::new(),
        mojo_coords: "org.example:test-plugin:1.0:goal".to_string(),
        project_root: "/tmp/x".to_string(),
        pom_path: "/tmp/x/pom.xml".to_string(),
        effective_pom_blob: Vec::new(),
        classpath: Vec::new(),
        plugin_classpath: Vec::new(),
        system_properties: Default::default(),
        environment: Default::default(),
        working_directory: "/tmp/x".to_string(),
        stdout_stream_id: 1,
        stderr_stream_id: 2,
        quiet: false,
        maven_compat: "3".to_string(),
        jvm_args: Vec::new(),
        credentials: None,
        extra_mvn_args: Vec::new(),
    };

    let mut handle = client.submit_action(req).await.expect("submit");
    let client_action_id = handle.action_id().to_string();
    assert!(!client_action_id.is_empty(), "action_id is assigned");

    let mut progress_count = 0;
    let mut got_result = false;
    let mut got_terminal = false;
    while let Some(event) = handle.next_event().await.expect("next_event") {
        match event {
            StreamEvent::Progress(p) => {
                assert_eq!(p.action_id, client_action_id, "action_id correlation");
                progress_count += 1;
            }
            StreamEvent::Result(r) => {
                assert_eq!(r.action_id, client_action_id);
                assert_eq!(r.status, action_result::Status::Success as i32);
                got_result = true;
            }
            StreamEvent::Error(e) => panic!("unexpected error: {e:?}"),
            StreamEvent::Stream(_) => panic!("unexpected stream chunk"),
        }
        // After receiving Result, the dispatcher closes the channel
        // and the next iteration will see `None`.
        if got_result {
            got_terminal = true;
        }
    }
    assert_eq!(progress_count, 3);
    assert!(got_terminal);

    // The server saw the same action_id we received progress for.
    let server_req = tokio::time::timeout(Duration::from_secs(1), server_handle)
        .await
        .expect("server task timed out")
        .expect("server task panicked");
    assert_eq!(server_req.action_id, client_action_id);
    assert_eq!(server_req.mojo_coords, "org.example:test-plugin:1.0:goal");
}

/// Cancel-safety smoke: `next_event` must survive being dropped in a
/// `select!` arm without dropping events. We submit one action, drive
/// the server to send 5 progress events, then drain the client side
/// with `tokio::select!` where the competing arm fires only after the
/// drain completes — proving the next_event future was repeatedly
/// dropped + resumed without losing events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn next_event_survives_select_drop() {
    let (_tmp, (_cmux, client), (_smux, server)) = paired().await;

    let server_handle = tokio::spawn(async move {
        let incoming = server
            .next_action()
            .await
            .expect("next_action")
            .expect("got incoming");
        let (_req, response, _cancel) = incoming.split();
        for i in 0..5 {
            let ev = ProgressEvent {
                kind: progress_event::Kind::Started.into(),
                action_id: String::new(),
                timestamp: format!("t{i}"),
                coord: String::new(),
                phase: format!("p{i}"),
                progress: 0.0,
                mojo: None,
                details: Default::default(),
            };
            response.send_progress(ev).await.expect("send_progress");
        }
        let result = ActionResult {
            action_id: String::new(),
            status: action_result::Status::Success.into(),
            exit_code: 0,
            duration_micros: 0,
            artifacts: Vec::new(),
            failure_message: String::new(),
            failure_stack: String::new(),
            attributes: Default::default(),
            error: None,
        };
        response.send_result(result).await.expect("send_result");
    });

    let req = ActionRequest {
        action_id: String::new(),
        mojo_coords: "test:cancelsafe:1:goal".to_string(),
        project_root: "/tmp/x".to_string(),
        pom_path: "/tmp/x/pom.xml".to_string(),
        effective_pom_blob: Vec::new(),
        classpath: Vec::new(),
        plugin_classpath: Vec::new(),
        system_properties: Default::default(),
        environment: Default::default(),
        working_directory: "/tmp/x".to_string(),
        stdout_stream_id: 1,
        stderr_stream_id: 2,
        quiet: false,
        maven_compat: "3".to_string(),
        jvm_args: Vec::new(),
        credentials: None,
        extra_mvn_args: Vec::new(),
    };
    let mut handle = client.submit_action(req).await.expect("submit");

    let mut count = 0;
    let mut got_terminal = false;
    while !got_terminal {
        // Race `next_event` against a far-future timeout that will
        // never fire — exercising the future's drop behavior on every
        // iteration nonetheless because `select!` polls all arms and
        // drops the loser's future on completion. If `next_event`
        // weren't cancel-safe, repeated drop+resume would lose
        // events; the assertion below catches that.
        tokio::select! {
            ev = handle.next_event() => {
                match ev.expect("next_event") {
                    Some(StreamEvent::Progress(_)) => count += 1,
                    Some(StreamEvent::Result(_)) => got_terminal = true,
                    Some(other) => panic!("unexpected event: {other:?}"),
                    None => got_terminal = true,
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(10)) => {
                panic!("timeout waiting for events");
            }
        }
    }
    assert_eq!(
        count, 5,
        "all 5 progress events observed across select! drops"
    );
    server_handle.await.expect("server task");
}
