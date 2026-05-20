//! Roastery-speedup **mechanism** demonstration under simulated WAN
//! latency.
//!
//! # What this proves
//!
//! The whole point of standing a roastery in front of a build fleet is
//! latency asymmetry: the roastery sits on the LAN (single-digit-ms
//! RTT), while the upstream Maven repository (Maven Central, an
//! internal Nexus over a WAN link, …) is tens-to-hundreds of ms away.
//! When a developer's local cache is cold, *every* artifact has to come
//! from somewhere — and "somewhere fast and near" beats "somewhere slow
//! and far" by exactly the ratio of the two round-trip times, multiplied
//! across every artifact in the dependency closure.
//!
//! This test demonstrates that mechanism end-to-end with a real client,
//! a real (in-process) roastery, and a latency-injected mock standing in
//! for a far-away "Central". It drives a batch of synthetic blobs down
//! two paths and measures wall-clock time:
//!
//! - **Path A — cold cache → direct upstream.** Each artifact is fetched
//!   from a mock "Central" that sleeps [`UPSTREAM_LATENCY`] before
//!   responding, modelling a WAN round trip.
//! - **Path B — cold cache → warm roastery.** Each artifact is fetched
//!   from a local in-process roastery that has already been populated
//!   (the "warm" precondition), modelling the LAN round trip.
//!
//! The assertion is that Path B is at least [`MIN_SPEEDUP`]× faster than
//! Path A on this workload. The measured ratio is printed so it is
//! visible in `--nocapture` runs and CI logs.
//!
//! # What this does NOT prove
//!
//! This is a **mechanism demonstration under a controlled, simulated
//! upstream latency** — it is *not* the milestone-level measurement
//! "cold cache + warm roastery beats cold cache + Central direct by ≥5×
//! on the 100-project corpus median". That measurement is a property of
//! a specific corpus on specific reference hardware against the real
//! network, and it is owned by the benchmark workstream (it needs the
//! full project corpus materialised and the reference-hardware harness
//! provisioned). Nothing here should be read as a claim that the
//! corpus-median target has been met. What it *does* establish is that
//! the client + protocol deliver the speedup the milestone targets
//! whenever the latency asymmetry the milestone assumes is present.
//!
//! # Why the chosen numbers are robust, not knife-edge
//!
//! The in-process roastery answers a GET in well under a millisecond, so
//! Path B's per-artifact cost is dominated by client + loopback
//! overhead, not by any injected delay. We inject [`UPSTREAM_LATENCY`]
//! (150 ms) per request into Path A and drive [`ARTIFACT_COUNT`] (20)
//! artifacts *sequentially* down each path — the same artifact, in the
//! same order, with a freshly-constructed (cold-pool) client per path so
//! neither path gets a connection-reuse head start the other doesn't.
//! 150 ms × 20 = 3 s of pure injected latency on Path A versus a few
//! tens of ms of real work on Path B puts the ratio comfortably in the
//! tens-of-× range, so a 5× floor clears with a wide margin even on a
//! loaded CI runner. The test asserts ≥5× but logs the actual ratio so
//! drift toward the floor is visible long before it would flake.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::io::Cursor;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::Path as AxumPath;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use barista_roastery_client::{ClientConfig, Digest, RoasteryClient, TlsConfig};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use url::Url;

mod common;

use common::harness::spawn_plain_server;

/// Per-request latency injected into the mock "Central" to model a WAN
/// round trip. Picked large enough that the ≥5× floor clears with a wide
/// margin against the sub-millisecond local roastery (see the module
/// doc-comment for the budget).
const UPSTREAM_LATENCY: Duration = Duration::from_millis(150);

/// Number of synthetic artifacts driven sequentially down each path.
/// Stands in for a small dependency closure; 20 × 150 ms = 3 s of pure
/// injected latency on the slow path.
const ARTIFACT_COUNT: usize = 20;

/// Minimum speedup the mechanism must deliver on this workload. This is
/// the milestone's ≥5× target *as a mechanism property under simulated
/// latency* — NOT the corpus-median measurement (see module docs).
const MIN_SPEEDUP: f64 = 5.0;

/// Build the synthetic artifact corpus: `ARTIFACT_COUNT` distinct blobs
/// of varying (small) sizes, each paired with its digest.
fn synthetic_artifacts() -> Vec<(Digest, Vec<u8>)> {
    (0..ARTIFACT_COUNT)
        .map(|i| {
            // Distinct, deterministic content per artifact so digests
            // differ and the byte-equality checks are meaningful.
            let size = 256 + i * 64;
            let bytes: Vec<u8> = (0..size).map(|j| ((i * 31 + j) % 251) as u8).collect();
            let digest = Digest::of_bytes(&bytes);
            (digest, bytes)
        })
        .collect()
}

/// A latency-injected mock "Central": an axum server that, on
/// `GET /v1/cas/sha256/{digest}`, sleeps [`UPSTREAM_LATENCY`] and then
/// returns the matching blob from an in-memory map. Models a far-away
/// upstream over a slow link. Returned handle aborts the server on drop.
struct MockCentral {
    addr: SocketAddr,
    task: Option<JoinHandle<()>>,
}

impl Drop for MockCentral {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

impl MockCentral {
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }
}

/// Shared state for the mock: digest-hex → bytes.
type CentralStore = Arc<std::collections::HashMap<String, Vec<u8>>>;

async fn central_get(
    axum::extract::State(store): axum::extract::State<CentralStore>,
    AxumPath(digest_hex): AxumPath<String>,
) -> impl IntoResponse {
    // The WAN round-trip cost. A real `tokio::time::sleep` (real
    // wall-clock time), so the measurement below reflects actual elapsed
    // time, not virtual/paused time.
    tokio::time::sleep(UPSTREAM_LATENCY).await;
    match store.get(&digest_hex) {
        Some(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_LENGTH, bytes.len().to_string()),
                (
                    header::HeaderName::from_static("x-barista-digest"),
                    format!("sha256:{digest_hex}"),
                ),
            ],
            bytes.clone(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Spawn the latency-injected mock "Central", pre-populated with every
/// artifact so a GET always hits (after paying the latency tax).
async fn spawn_mock_central(artifacts: &[(Digest, Vec<u8>)]) -> MockCentral {
    let mut map = std::collections::HashMap::new();
    for (digest, bytes) in artifacts {
        map.insert(digest.to_hex(), bytes.clone());
    }
    let store: CentralStore = Arc::new(map);

    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind central");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let addr = std_listener.local_addr().expect("central addr");
    let listener = TcpListener::from_std(std_listener).expect("from std");

    let app = axum::Router::new()
        .route("/v1/cas/sha256/{digest}", get(central_get))
        .with_state(store);

    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Wait for the listener to accept before returning.
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    MockCentral {
        addr,
        task: Some(task),
    }
}

/// Build a cold (fresh connection pool) plain-HTTP client at `base`.
fn cold_client(base: &str) -> RoasteryClient {
    let url: Url = base.parse().expect("parse base url");
    let cfg = ClientConfig::builder(url)
        .tls(TlsConfig::PlainHttp)
        .timeout(Duration::from_secs(30))
        .build();
    RoasteryClient::new(cfg).expect("client")
}

/// Drain a `BlobStream` into a `Vec<u8>`.
async fn drain(mut blob: barista_roastery_client::BlobStream) -> Vec<u8> {
    let mut buf = Vec::with_capacity(blob.stat.size as usize);
    blob.body.read_to_end(&mut buf).await.expect("read_to_end");
    buf
}

// -------------------------------------------------------------------
// Mechanism demonstration: warm-roastery vs latency-injected upstream.
// -------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warm_roastery_beats_latency_injected_upstream_by_at_least_5x() {
    let artifacts = synthetic_artifacts();

    // --- Set up the "warm roastery": a local in-process server,
    // pre-populated so every GET on Path B is a hit (the "warm"
    // precondition the milestone assumes).
    let roastery = spawn_plain_server().await;
    {
        // Use a throwaway client to seed the roastery; Path B below gets
        // its own cold client so the measurement isn't biased by a warm
        // connection pool.
        let seeder = cold_client(&roastery.base_url());
        for (digest, bytes) in &artifacts {
            let reader = Cursor::new(bytes.clone());
            seeder
                .put_blob(*digest, reader, bytes.len() as u64)
                .await
                .expect("seed roastery");
        }
    }

    // --- Set up the latency-injected mock "Central" for Path A.
    let central = spawn_mock_central(&artifacts).await;

    // ---------------------------------------------------------------
    // Path A: cold cache → direct from the far-away upstream. Every GET
    // pays UPSTREAM_LATENCY.
    // ---------------------------------------------------------------
    let central_client = cold_client(&central.base_url());
    let path_a_start = Instant::now();
    for (digest, expected) in &artifacts {
        let blob = central_client
            .get_blob(*digest)
            .await
            .expect("path A get_blob (upstream)");
        let bytes = drain(blob).await;
        assert_eq!(&bytes, expected, "path A byte-equality");
    }
    let path_a_elapsed = path_a_start.elapsed();

    // ---------------------------------------------------------------
    // Path B: cold cache → warm (local) roastery. Each GET is a local
    // hit; no injected latency.
    // ---------------------------------------------------------------
    let roastery_client = cold_client(&roastery.base_url());
    let path_b_start = Instant::now();
    for (digest, expected) in &artifacts {
        let blob = roastery_client
            .get_blob(*digest)
            .await
            .expect("path B get_blob (roastery)");
        let bytes = drain(blob).await;
        assert_eq!(&bytes, expected, "path B byte-equality");
    }
    let path_b_elapsed = path_b_start.elapsed();

    // ---------------------------------------------------------------
    // Report + assert.
    // ---------------------------------------------------------------
    let ratio = path_a_elapsed.as_secs_f64() / path_b_elapsed.as_secs_f64();

    println!("=== roastery-speedup mechanism demonstration ===");
    println!("  artifacts driven per path : {ARTIFACT_COUNT}");
    println!("  injected upstream latency : {UPSTREAM_LATENCY:?} per request (simulated WAN RTT)");
    println!("  Path A (cold → upstream)  : {path_a_elapsed:?}");
    println!("  Path B (cold → roastery)  : {path_b_elapsed:?}");
    println!("  measured speedup          : {ratio:.1}x  (floor: {MIN_SPEEDUP:.0}x)");
    println!(
        "  NOTE: mechanism demo under simulated {UPSTREAM_LATENCY:?} upstream latency — \
         NOT the 100-project corpus-median measurement (deferred to the benchmark workstream)."
    );

    assert!(
        ratio >= MIN_SPEEDUP,
        "warm roastery should beat the latency-injected upstream by at least {MIN_SPEEDUP:.0}x \
         under {UPSTREAM_LATENCY:?} simulated upstream latency, but measured {ratio:.1}x \
         (Path A {path_a_elapsed:?} vs Path B {path_b_elapsed:?}). This is a mechanism \
         demonstration, NOT the corpus-median measurement."
    );
}
