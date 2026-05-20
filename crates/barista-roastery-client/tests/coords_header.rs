//! Test that `RoasteryClient::get_blob_with_coords` actually puts an
//! `X-Barista-Coords` header on the wire.
//!
//! The roastery server's upstream-on-miss path inspects this header
//! when serving a `GET /v1/cas/sha256/{digest}` for an artifact it
//! doesn't have locally. The cache crate uses the
//! `get_blob_with_coords` method so the roastery can resolve a
//! digest-keyed request back to a coordinate-keyed upstream URL.
//!
//! Rather than spinning a full roastery with upstream-on-miss
//! configured, we point the client at a small axum mock that
//! responds 200 with a deterministic body — but only if the
//! `X-Barista-Coords` header matches the expected value. A
//! mismatched or missing header surfaces as a 400, which the test
//! catches.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use barista_roastery_client::{ClientConfig, Digest, RoasteryClient, TlsConfig};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use url::Url;

const EXPECTED_COORDS: &str = "org.example:lib:jar:1.0.0";

#[derive(Clone, Default)]
struct CapturedState {
    captured: Arc<Mutex<Option<String>>>,
}

async fn handler(
    State(state): State<CapturedState>,
    AxPath(digest_hex): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    let coords = headers
        .get("x-barista-coords")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    {
        let mut guard = state.captured.lock().await;
        *guard = coords.clone();
    }
    if coords.as_deref() != Some(EXPECTED_COORDS) {
        return (
            StatusCode::BAD_REQUEST,
            format!("missing or mismatched coords header: got {coords:?}"),
        )
            .into_response();
    }
    let body = b"hello-coords";
    let body_digest = Digest::of_bytes(body);
    // Only succeed when the caller asked about the body's actual
    // digest — otherwise the client's parse_blob_stat will reject the
    // mismatched X-Barista-Digest header.
    if digest_hex != body_digest.to_hex() {
        return (
            StatusCode::BAD_REQUEST,
            "test only serves the canonical digest".to_string(),
        )
            .into_response();
    }
    let mut resp = Response::new(axum::body::Body::from(body.to_vec()));
    resp.headers_mut()
        .insert("content-length", body.len().to_string().parse().unwrap());
    resp.headers_mut().insert(
        "x-barista-digest",
        format!("sha256:{}", body_digest.to_hex()).parse().unwrap(),
    );
    resp
}

#[tokio::test]
async fn get_blob_with_coords_sets_x_barista_coords_header() {
    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let addr = std_listener.local_addr().expect("addr");
    let listener = TcpListener::from_std(std_listener).expect("from std");

    let state = CapturedState::default();
    let app: Router = Router::new()
        .route("/v1/cas/sha256/{digest}", get(handler))
        .with_state(state.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Brief retry loop for listener readiness.
    let base = format!("http://127.0.0.1:{}", addr.port());
    let url: Url = base.parse().unwrap();
    let cfg = ClientConfig::builder(url)
        .tls(TlsConfig::PlainHttp)
        .timeout(Duration::from_secs(5))
        .build();
    let client = RoasteryClient::new(cfg).expect("client");

    let body = b"hello-coords";
    let digest = Digest::of_bytes(body);

    let mut blob = client
        .get_blob_with_coords(digest, EXPECTED_COORDS)
        .await
        .expect("get_blob_with_coords succeeds");

    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    blob.body.read_to_end(&mut buf).await.expect("drain");
    assert_eq!(buf, body);

    let captured = state.captured.lock().await.clone();
    assert_eq!(
        captured.as_deref(),
        Some(EXPECTED_COORDS),
        "server should have observed the X-Barista-Coords header"
    );

    server.abort();
}

#[tokio::test]
async fn get_blob_without_coords_omits_header() {
    // Same fixture, but the plain get_blob path should NOT send the
    // coords header — the mock returns 400 in that case, which we
    // observe via ClientError::ServerRejected.
    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let addr = std_listener.local_addr().expect("addr");
    let listener = TcpListener::from_std(std_listener).expect("from std");

    let state = CapturedState::default();
    let app: Router = Router::new()
        .route("/v1/cas/sha256/{digest}", get(handler))
        .with_state(state.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let base = format!("http://127.0.0.1:{}", addr.port());
    let url: Url = base.parse().unwrap();
    let cfg = ClientConfig::builder(url)
        .tls(TlsConfig::PlainHttp)
        .timeout(Duration::from_secs(5))
        .build();
    let client = RoasteryClient::new(cfg).expect("client");
    let digest = Digest::of_bytes(b"hello-coords");

    let err = client
        .get_blob(digest)
        .await
        .expect_err("expected server rejection");
    // The plain get_blob path doesn't include the header → mock
    // returns 400 → client surfaces as ServerRejected.
    use barista_roastery_client::ClientError;
    match err {
        ClientError::ServerRejected { status, .. } => assert_eq!(status, 400),
        other => panic!("expected ServerRejected(400), got {other:?}"),
    }

    let captured = state.captured.lock().await.clone();
    assert_eq!(
        captured, None,
        "server should NOT have observed an X-Barista-Coords header"
    );

    server.abort();
}
