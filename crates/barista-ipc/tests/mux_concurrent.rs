// Integration-test target — workspace security lints are allowed for
// the usual reasons (panic-loud-on-misuse, prost enum casts).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]
#![cfg(unix)]

//! 32 concurrent actions on a single multiplexed connection (M4.1 AC #2).
//!
//! Each action gets a stream of progress events tagged with the action
//! sequence number; the test asserts that every event arrived on the
//! correct client-side handle (no cross-routing) and that the count
//! per action matches what the server emitted.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use barista_ipc::{
    transport::uds::UdsTransport, ActionRequest, ActionResult, Multiplexer, ProgressEvent,
    StreamEvent, action_result, progress_event,
};
use tempfile::TempDir;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

const N_ACTIONS: usize = 32;
const EVENTS_PER_ACTION: usize = 8;

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
async fn thirty_two_concurrent_actions_round_trip_independently() {
    let (_tmp, (_cmux, client), (_smux, server)) = paired().await;

    // Server: accept N_ACTIONS, dispatch each into its own task that
    // sends EVENTS_PER_ACTION progress events tagged with the action
    // sequence number embedded in `phase`. The test re-derives the
    // sequence from the action's `mojo_coords` string.
    //
    // Per-server-action handlers are spawned so they all interleave on
    // the same outbound channel — this is the "no corruption" test.
    let server_done = Arc::new(Mutex::new(Vec::<tokio::task::JoinHandle<()>>::new()));
    let server_done_clone = Arc::clone(&server_done);
    let accept_handle = tokio::spawn(async move {
        for _ in 0..N_ACTIONS {
            let incoming = server
                .next_action()
                .await
                .expect("next_action")
                .expect("got incoming");
            let task = tokio::spawn(async move {
                let (req, response, _cancel) = incoming.split();
                // Re-derive the sequence number from mojo_coords =
                // "test:action:N:goal".
                let seq: usize = req
                    .mojo_coords
                    .split(':')
                    .nth(2)
                    .and_then(|s| s.parse().ok())
                    .expect("parse seq");
                for i in 0..EVENTS_PER_ACTION {
                    let ev = ProgressEvent {
                        kind: progress_event::Kind::Started.into(),
                        action_id: String::new(),
                        timestamp: format!("ts-{seq}-{i}"),
                        coord: String::new(),
                        // Encode (seq, i) so the test can verify both
                        // routing (right action id) and ordering
                        // (events arrive in order).
                        phase: format!("seq={seq};idx={i}"),
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
            server_done_clone.lock().await.push(task);
        }
    });

    // Client: submit N_ACTIONS in flight, each its own task that
    // drains progress + result, returns the count of events seen and
    // the order they were observed.
    let mut handles = Vec::new();
    for seq in 0..N_ACTIONS {
        let req = ActionRequest {
            action_id: String::new(),
            mojo_coords: format!("test:action:{seq}:goal"),
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
        let action_handle = client.submit_action(req).await.expect("submit");
        handles.push((seq, action_handle));
    }

    // Drain each handle and collect (seq, idx) tuples seen.
    let mut drains = Vec::new();
    for (seq, mut h) in handles {
        let task = tokio::spawn(async move {
            let action_id = h.action_id().to_string();
            let mut events = Vec::<(usize, usize)>::new();
            let mut got_result = false;
            while let Some(ev) = h.next_event().await.expect("next_event") {
                match ev {
                    StreamEvent::Progress(p) => {
                        assert_eq!(p.action_id, action_id, "no cross-routing");
                        // Parse `phase = "seq=X;idx=Y"`.
                        let parts: HashMap<&str, &str> = p
                            .phase
                            .split(';')
                            .filter_map(|kv| {
                                let mut it = kv.splitn(2, '=');
                                Some((it.next()?, it.next()?))
                            })
                            .collect();
                        let s: usize = parts["seq"].parse().expect("seq parse");
                        let i: usize = parts["idx"].parse().expect("idx parse");
                        events.push((s, i));
                    }
                    StreamEvent::Result(r) => {
                        assert_eq!(r.action_id, action_id);
                        got_result = true;
                    }
                    StreamEvent::Error(e) => panic!("unexpected error: {e:?}"),
                    StreamEvent::Stream(_) => panic!("unexpected stream chunk"),
                }
                if got_result {
                    break;
                }
            }
            // Drop the handle here — the channel will close cleanly
            // because cleanup_client removed the entry on Result.
            (seq, events)
        });
        drains.push(task);
    }

    accept_handle.await.expect("accept loop");
    for t in server_done.lock().await.drain(..) {
        t.await.expect("server task");
    }

    let mut results: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
    for d in drains {
        let (seq, events) = d.await.expect("client drain");
        results.insert(seq, events);
    }

    // Per-action assertions:
    assert_eq!(results.len(), N_ACTIONS, "every action returned");
    for (seq, events) in &results {
        assert_eq!(
            events.len(),
            EVENTS_PER_ACTION,
            "action {seq} received {} events; expected {EVENTS_PER_ACTION}",
            events.len()
        );
        // All events are correctly tagged with this action's seq (the
        // server only emits its own seq; cross-tagging would mean
        // the routing layer corrupted the dispatch).
        for (i, (es, ei)) in events.iter().enumerate() {
            assert_eq!(*es, *seq, "event seq tag matches action seq");
            assert_eq!(*ei, i, "events arrived in order for action {seq}");
        }
    }
}
