// Integration-test target — workspace security lints are allowed for
// the usual reasons (panic-loud-on-misuse, prost enum casts).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]
#![cfg(unix)]

//! Drop-without-cancel triggers an automatic CancelRequest (M4.1 T6
//! drop-cancel AC).
//!
//! Pattern: client submits an action, server starts a slow body that
//! awaits the per-action `CancelToken`; client drops the `ActionHandle`
//! without calling `.cancel()`; test asserts the server observes a
//! `CancelRequest` (the token fires) within a reasonable bound.
//!
//! This is the safety-net behavior — if user code forgets a cleanup,
//! the daemon doesn't leak the in-flight action body. Documented in
//! `ActionHandle`'s `Drop` impl.

use std::path::PathBuf;
use std::time::Duration;

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
async fn handle_drop_triggers_cancel_request() {
    let (_tmp, (_cmux, client), (_smux, server)) = paired().await;

    // Server-side body: wait for cancel. If it doesn't arrive within
    // 5s, panic — the client should have dropped the handle and
    // the resulting CancelRequest should have hit us long before
    // then.
    let server_handle = tokio::spawn(async move {
        let incoming = server
            .next_action()
            .await
            .expect("next_action")
            .expect("got incoming");
        let (_req, response, cancel) = incoming.split();
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                panic!("server body did not observe Drop-triggered cancel within 5s");
            }
        }
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
    });

    let req = ActionRequest {
        action_id: String::new(),
        mojo_coords: "test:drop:1:goal".to_string(),
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
    let id = handle.action_id().to_string();
    assert!(!id.is_empty());

    // Brief wait so the server-side action is fully registered before
    // we drop. (Without this the Drop could fire before the
    // ActionRequest envelope round-trips, sending a CancelRequest for
    // an unknown id — recoverable but masks the test intent.)
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The critical line: drop without calling `.cancel()`.
    drop(handle);

    // Server must see the cancel within a reasonable bound. Use a
    // 1s timeout — drop-cancel is best-effort but should land in
    // tens of milliseconds on a localhost UDS.
    tokio::time::timeout(Duration::from_secs(1), server_handle)
        .await
        .expect("server task did not exit after handle drop")
        .expect("server task panicked");
}
