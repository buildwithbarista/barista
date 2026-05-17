// Integration-test target — workspace security lints are allowed for
// the usual reasons (panic-loud-on-misuse, prost enum casts).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]
#![cfg(unix)]

//! Wire-framing tests for the transport layer.
//!
//! These tests target the `LengthDelimitedCodec` configuration end-to-
//! end through a UDS pair (using UDS rather than `tokio::io::duplex`
//! because the goal is to exercise the same code paths the production
//! transports walk). They cover the framing edge cases that PRD §12
//! and Task 4's acceptance criteria call out:
//!
//!  1. **Exact wire shape** — a known `Envelope` produces a wire frame
//!     whose first 4 bytes equal the big-endian length of the payload.
//!  2. **Torn writes** — writing a 100-byte frame as two `write_all`
//!     calls (each carrying part of the prefix/payload) still
//!     reassembles into one frame on the read side.
//!  3. **Partial frame** — closing the socket mid-frame surfaces a
//!     terminal error (not a silent truncation).
//!  4. **Oversized announcement** — sending a frame whose announced
//!     length exceeds `MAX_FRAME_BYTES` is rejected with
//!     `TransportError::FrameTooLarge`.
//!  5. **Sender-side oversize guard** — `encode_envelope` (exercised
//!     indirectly via `Transport::send`) refuses to put an oversized
//!     payload on the wire.
//!  6. **Interleaved send + recv** — frames sent in alternation are
//!     received in the same order (FIFO ordering on a stream socket).

use std::path::PathBuf;

use barista_ipc::{
    Envelope, Ping, Transport, TransportError, envelope, transport::MAX_FRAME_BYTES,
    transport::uds::UdsTransport,
};
use prost::Message;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

fn temp_socket_path() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir creation");
    let path = dir.path().join("s");
    (dir, path)
}

/// Spawn a server that accepts one connection and returns the raw
/// `UnixStream` to the caller (via a oneshot channel). The test then
/// either drives a `UdsTransport` or pokes the raw bytes — depending on
/// which framing edge it's after.
async fn raw_accept_one(path: PathBuf) -> tokio::sync::oneshot::Receiver<UnixStream> {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (stream_tx, stream_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let listener = UnixListener::bind(&path).expect("listener bind");
        ready_tx.send(()).expect("ready signal");
        let (stream, _addr) = listener.accept().await.expect("accept");
        stream_tx.send(stream).expect("stream forward");
    });
    ready_rx.await.expect("server ready");
    stream_rx
}

// ---------------------------------------------------------------------------
// 1. Exact wire shape: 4-byte big-endian length prefix + protobuf payload.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn frame_starts_with_4_byte_big_endian_length() {
    let (_tmp, path) = temp_socket_path();
    let stream_rx = raw_accept_one(path.clone()).await;
    let mut client = UdsTransport::connect(&path).await.expect("connect");

    let env = Envelope {
        version: 1,
        request_id: 99,
        body: Some(envelope::Body::Ping(Ping {
            client: "barista 0.1.0".to_string(),
            sent_at_unix_micros: 123,
        })),
    };
    client.send(env.clone()).await.expect("send");

    let mut server_stream = stream_rx.await.expect("server stream");
    let mut header = [0u8; 4];
    server_stream
        .read_exact(&mut header)
        .await
        .expect("read length prefix");
    let announced_len = u32::from_be_bytes(header);
    // Cross-check: the payload bytes the codec wrote should equal what
    // prost would emit standalone.
    let expected_payload = env.encode_to_vec();
    assert_eq!(
        announced_len as usize,
        expected_payload.len(),
        "announced length must equal protobuf payload length"
    );

    let mut payload = vec![0u8; announced_len as usize];
    server_stream
        .read_exact(&mut payload)
        .await
        .expect("read payload");
    assert_eq!(
        payload, expected_payload,
        "payload bytes must match prost output"
    );

    drop(client);
}

// ---------------------------------------------------------------------------
// 2. Torn writes: split a frame across two write_all calls, server reads
//    a single intact frame back.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn torn_writes_reassemble_into_one_frame() {
    let (_tmp, path) = temp_socket_path();
    let stream_rx = raw_accept_one(path.clone()).await;

    // Connect via the raw UnixStream so we can drive bytes manually.
    let mut raw_client = UnixStream::connect(&path).await.expect("raw connect");
    let server_stream = stream_rx.await.expect("server stream");
    let mut server = UdsTransport::from_stream(server_stream);

    // Build the expected wire bytes for an Envelope by encoding by
    // hand: 4-byte big-endian length, then prost payload.
    let env = Envelope {
        version: 1,
        request_id: 7,
        body: Some(envelope::Body::Ping(Ping {
            client: "barista 0.1.0".to_string(),
            sent_at_unix_micros: 42,
        })),
    };
    let payload = env.encode_to_vec();
    let mut wire = Vec::with_capacity(4 + payload.len());
    wire.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
    wire.extend_from_slice(&payload);

    // Split into three writes at non-aligned offsets to exercise the
    // codec's resumption-after-pause logic. The split points are:
    //   - byte 2 (mid-length-prefix)
    //   - byte 7 (just after the prefix, mid-payload)
    //   - end
    let (a, rest) = wire.split_at(2);
    let (b, c) = rest.split_at(5.min(rest.len()));
    raw_client.write_all(a).await.expect("write a");
    raw_client.flush().await.expect("flush a");
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    raw_client.write_all(b).await.expect("write b");
    raw_client.flush().await.expect("flush b");
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    raw_client.write_all(c).await.expect("write c");
    raw_client.flush().await.expect("flush c");

    let echoed = server.recv().await.expect("recv");
    assert_eq!(env, echoed, "torn writes should reassemble cleanly");
}

// ---------------------------------------------------------------------------
// 3. Partial frame followed by close surfaces a terminal error.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn partial_frame_then_close_errors() {
    let (_tmp, path) = temp_socket_path();
    let stream_rx = raw_accept_one(path.clone()).await;

    let mut raw_client = UnixStream::connect(&path).await.expect("raw connect");
    let server_stream = stream_rx.await.expect("server stream");
    let mut server = UdsTransport::from_stream(server_stream);

    // Write a 4-byte prefix announcing 100 bytes, then write only 10
    // bytes of payload before closing — the codec should surface a
    // terminal error when the connection ends mid-frame. Since
    // M4.2 T6 landed, a partial-frame EOF is reclassified as
    // `TransportError::DaemonCrashed { UnexpectedEof }` (a peer
    // that disappeared mid-frame is, by definition, the failure-
    // model shape the daemon-crash path needs to detect). `Closed`
    // is also accepted to defend against future codec changes that
    // route the half-state through a different read path; the
    // contract is "terminal + daemon-crash-flavoured", not the
    // exact variant.
    raw_client
        .write_all(&100u32.to_be_bytes())
        .await
        .expect("write prefix");
    raw_client
        .write_all(&[0u8; 10])
        .await
        .expect("write partial");
    raw_client.flush().await.expect("flush");
    drop(raw_client);

    let result = server.recv().await;
    match result {
        Err(TransportError::DaemonCrashed { .. }) | Err(TransportError::Closed) => {}
        other => panic!(
            "expected DaemonCrashed/Closed on partial frame (M4.2 T6 failure model), got: {other:?}"
        ),
    }
}

// ---------------------------------------------------------------------------
// 4. Oversized announcement is rejected as FrameTooLarge.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oversized_announcement_rejected_as_frame_too_large() {
    let (_tmp, path) = temp_socket_path();
    let stream_rx = raw_accept_one(path.clone()).await;

    let mut raw_client = UnixStream::connect(&path).await.expect("raw connect");
    let server_stream = stream_rx.await.expect("server stream");
    let mut server = UdsTransport::from_stream(server_stream);

    // Announce a frame that's MAX_FRAME_BYTES + 1 — the codec should
    // reject the length prefix *before* attempting to allocate the
    // payload buffer. We never need to send the payload bytes.
    let announce = u32::try_from(MAX_FRAME_BYTES + 1).unwrap();
    raw_client
        .write_all(&announce.to_be_bytes())
        .await
        .expect("write oversized prefix");
    raw_client.flush().await.expect("flush");

    let result = server.recv().await;
    match result {
        Err(TransportError::FrameTooLarge { .. }) => {}
        other => panic!("expected FrameTooLarge, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 5. Sender-side oversize guard.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sender_refuses_oversized_payload() {
    // No socket needed — `encode_envelope` is the gatekeeper, and
    // `Transport::send` runs it before touching the wire. We exercise
    // it indirectly by trying to send through a transport whose peer
    // never reads. The error must surface before any bytes hit the
    // socket.
    use barista_ipc::ActionStream;

    let (_tmp, path) = temp_socket_path();
    let stream_rx = raw_accept_one(path.clone()).await;
    let mut client = UdsTransport::connect(&path).await.expect("connect");
    // Drop the server side immediately — we want to prove `send`
    // fails on size before it even tries to write.
    let _server_stream = stream_rx.await.expect("server stream");

    let big = vec![0u8; MAX_FRAME_BYTES + 4096];
    let env = Envelope {
        version: 1,
        request_id: 1,
        body: Some(envelope::Body::Stream(ActionStream {
            stream_id: 1,
            payload: big,
            end: false,
            action_id: "too-big".to_string(),
        })),
    };
    match client.send(env).await {
        Err(TransportError::FrameTooLarge { announced }) => {
            assert!(
                usize::try_from(announced).unwrap() > MAX_FRAME_BYTES,
                "announced should exceed cap; got {announced}"
            );
        }
        other => panic!("expected sender-side FrameTooLarge, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 6. Interleaved send + recv preserves FIFO order.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn interleaved_traffic_preserves_order() {
    let (_tmp, path) = temp_socket_path();
    let path_for_server = path.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let listener = UnixListener::bind(&path_for_server).expect("listener bind");
        ready_tx.send(()).expect("ready signal");
        let (stream, _addr) = listener.accept().await.expect("accept");
        let mut server = UdsTransport::from_stream(stream);
        // Receive 10 frames, send 10 frames back in the same order
        // (each tagged with the request_id from the corresponding
        // inbound frame).
        let mut received = Vec::with_capacity(10);
        for _ in 0..10 {
            received.push(server.recv().await.expect("server recv"));
        }
        for env in received {
            server.send(env).await.expect("server send");
        }
    });
    ready_rx.await.expect("server ready");

    let mut client = UdsTransport::connect(&path).await.expect("connect");
    // Send 10 frames first, then read 10 back. The codec must hand
    // them back in the same order.
    for i in 0..10u64 {
        let env = Envelope {
            version: 1,
            request_id: i * 17,
            body: Some(envelope::Body::Ping(Ping {
                client: format!("ping-{i}"),
                sent_at_unix_micros: i64::try_from(i).unwrap(),
            })),
        };
        client.send(env).await.expect("send");
    }
    for i in 0..10u64 {
        let env = client.recv().await.expect("recv");
        assert_eq!(
            env.request_id,
            i * 17,
            "frame {i} arrived out of order: got request_id {}",
            env.request_id
        );
    }

    drop(client);
    server.await.expect("server join");
}

// ---------------------------------------------------------------------------
// 7. Exactly-at-cap frame is accepted (boundary).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn at_cap_frame_is_accepted() {
    // We pick `MAX_FRAME_BYTES - 256` as the payload size to leave a
    // little headroom for protobuf wire overhead (tag varints, the
    // length-delimited string for action_id, etc.) so the final
    // serialized Envelope size lands just under the cap. A payload
    // exactly at the cap would overflow once prost adds the field-tag
    // bytes; this test proves the boundary works for a "max realistic"
    // frame, not a literal `len == MAX` frame.
    use barista_ipc::ActionStream;

    let (_tmp, path) = temp_socket_path();
    let path_for_server = path.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server_h = tokio::spawn(async move {
        let listener = UnixListener::bind(&path_for_server).expect("listener bind");
        ready_tx.send(()).expect("ready signal");
        let (stream, _addr) = listener.accept().await.expect("accept");
        let mut server = UdsTransport::from_stream(stream);
        let env = server.recv().await.expect("server recv");
        server.send(env).await.expect("server send");
    });
    ready_rx.await.expect("server ready");

    let mut client = UdsTransport::connect(&path).await.expect("connect");
    let big = vec![0u8; MAX_FRAME_BYTES - 256];
    let env = Envelope {
        version: 1,
        request_id: 1,
        body: Some(envelope::Body::Stream(ActionStream {
            stream_id: 1,
            payload: big.clone(),
            end: true,
            action_id: "a".to_string(),
        })),
    };
    client.send(env.clone()).await.expect("send near-cap frame");
    let echoed = client.recv().await.expect("recv near-cap frame");
    assert_eq!(env, echoed);

    drop(client);
    server_h.await.expect("server join");
}
