// SPDX-License-Identifier: MIT OR Apache-2.0

// Integration-test target — workspace security lints are allowed for
// the usual reasons (panic-loud-on-misuse, prost enum casts).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]
#![cfg(unix)]

//! Cancellation aborts in-flight action within 100 ms (M4.1 AC #2).
//!
//! Submits an action whose server-side body sleeps for up to 5 seconds
//! but also `select!`s on the per-action `CancelToken`. The test
//! cancels the handle via `ActionHandle::cancel().await` and measures
//! wall-clock time from cancel-send to the server's body exiting.
//! The acceptance bound is 100 ms.
//!
//! The drop-as-cancel ergonomic is exercised in
//! `tests/mux_drop_cancels.rs`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use barista_ipc::{
    ActionRequest, ActionResult, Multiplexer, action_result, transport::uds::UdsTransport,
};
use tempfile::TempDir;
use tokio::net::{UnixListener, UnixStream};

fn temp_socket_path() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("s");
    (dir, path)
}

async fn paired() -> (
    TempDir,
    (Multiplexer, barista_ipc::MuxClient),
    (Multiplexer, barista_ipc::MuxServer),
) {
    let (tmp, path) = temp_socket_path();
    let path_for_server = path.clone();
    let listener = UnixListener::bind(&path_for_server).expect("bind");
    let accept = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept");
        stream
    });
    let client_stream = UnixStream::connect(&path).await.expect("client connect");
    let server_stream = accept.await.expect("accept join");
    let (cmux, cmc, _cms) = Multiplexer::spawn(UdsTransport::from_stream(client_stream));
    let (smux, _smc, sms) = Multiplexer::spawn(UdsTransport::from_stream(server_stream));
    (tmp, (cmux, cmc), (smux, sms))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_aborts_within_100ms() {
    let (_tmp, (_cmux, client), (_smux, server)) = paired().await;

    // Server: accept one action, run a 5s "work" body that races
    // against the cancel token. Returns the wall-clock moment when
    // the body exits so the test can derive cancel-send → body-exit.
    let server_handle = tokio::spawn(async move {
        let incoming = server
            .next_action()
            .await
            .expect("next_action")
            .expect("got incoming");
        let (_req, response, cancel) = incoming.split();
        let exited_at = tokio::select! {
            () = cancel.cancelled() => Instant::now(),
            () = tokio::time::sleep(Duration::from_secs(5)) => {
                panic!("server body finished before cancel arrived (5s elapsed)");
            }
        };
        // Send the terminal Cancelled result so the dispatcher
        // cleans up the per-action state on both sides.
        let result = ActionResult {
            action_id: String::new(),
            status: action_result::Status::Cancelled.into(),
            exit_code: 130,
            duration_micros: 0,
            artifacts: Vec::new(),
            failure_message: String::new(),
            failure_stack: String::new(),
            attributes: Default::default(),
            error: None,
        };
        let _ = response.send_result(result).await;
        exited_at
    });

    let req = ActionRequest {
        action_id: String::new(),
        mojo_coords: "test:cancel:1:goal".to_string(),
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
    let handle = client.submit_action(req).await.expect("submit");

    // Give the round-trip a moment to wire up the server-side body —
    // we want to measure cancel→exit, not submit→accept.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let cancel_sent_at = Instant::now();
    handle.cancel().await.expect("cancel");

    let exited_at = server_handle.await.expect("server task");
    let elapsed = exited_at.saturating_duration_since(cancel_sent_at);

    assert!(
        elapsed < Duration::from_millis(100),
        "cancel-to-exit latency {elapsed:?} exceeded 100ms budget"
    );
}
