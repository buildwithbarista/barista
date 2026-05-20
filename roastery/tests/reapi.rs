// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for the Bazel REAPI gRPC surface.
//!
//! Each test spins an in-process roastery server (the barista-native
//! HTTP router merged with the REAPI gRPC services, exactly the
//! topology `roastery::run` builds) on an ephemeral TCP port and drives
//! the gRPC surface with generated `tonic` clients. A fresh
//! `TempDir`-backed `FsCas` per test keeps the cases independent and
//! parallel-safe.
//!
//! These Rust round-trip tests prove the REAPI wire contract
//! (Capabilities negotiation, FindMissingBlobs, BatchUpdate/BatchRead,
//! ByteStream Read/Write, GetTree's honest UNIMPLEMENTED) AND that the
//! gRPC surface fronts the SAME content-addressed store as the
//! barista-native HTTP surface (the headline cross-protocol test). The
//! full Go-based `bazel-remote` conformance harness is a deferred
//! follow-up; it would exercise the same wire contract from the
//! reference client, but is out of scope for this crate's Rust test
//! suite.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use roastery::{AppState, BearerVerifier, FsCas, ServerConfig};
use roastery::proto::reapi;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tonic::Request;
use tonic::transport::{Channel, Endpoint};

use reapi::bytestream::byte_stream_client::ByteStreamClient;
use reapi::bytestream::{ReadRequest, WriteRequest};
use reapi::google_rpc;
use reapi::reapi_v2::capabilities_client::CapabilitiesClient;
use reapi::reapi_v2::content_addressable_storage_client::ContentAddressableStorageClient;
use reapi::reapi_v2::{
    BatchReadBlobsRequest, BatchUpdateBlobsRequest, Digest as ReapiDigest,
    FindMissingBlobsRequest, GetCapabilitiesRequest, GetTreeRequest, batch_update_blobs_request,
    digest_function,
};

// -------------------------------------------------------------------
// Test harness
// -------------------------------------------------------------------

/// Live server fixture: owns the storage temp dir, the server task,
/// and the bound address.
struct Harness {
    addr: SocketAddr,
    _tmp: TempDir,
    server: Option<JoinHandle<()>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(h) = self.server.take() {
            h.abort();
        }
    }
}

impl Harness {
    /// gRPC endpoint URL.
    fn grpc_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Base URL for the barista-native HTTP surface.
    fn http_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Dial a gRPC `Channel` to this server.
    async fn channel(&self) -> Channel {
        Endpoint::from_shared(self.grpc_url())
            .unwrap()
            .connect()
            .await
            .unwrap()
    }
}

/// Spin up a roastery server with the merged HTTP + REAPI gRPC router
/// on an OS-assigned ephemeral port. `bearer` configures bearer auth
/// (the tokens-file path is written to a temp file and pointed at by
/// the config) when `Some`.
async fn spawn_server() -> Harness {
    spawn_server_inner(None).await
}

/// Spawn a server whose CAS data services require the given bearer
/// token. Loads the verifier through the public `BearerVerifier::load`
/// path (writing a tokens file to a temp file), exactly as production
/// does — `from_pairs` is a crate-internal test helper not visible to
/// this external test crate.
async fn spawn_server_with_bearer(label: &str, secret: &str) -> Harness {
    let tokens = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(
        &mut std::fs::File::create(tokens.path()).unwrap(),
        format!("{label}:{secret}\n").as_bytes(),
    )
    .unwrap();
    let verifier = BearerVerifier::load(tokens.path()).unwrap();
    // Keep the tokens file alive for the server's lifetime by leaking it
    // into the harness via a closure capture is overkill; the verifier
    // already hashed + dropped the file contents at load time, so the
    // temp file can be dropped here safely.
    drop(tokens);
    spawn_server_inner(Some(Arc::new(verifier))).await
}

async fn spawn_server_inner(bearer: Option<Arc<BearerVerifier>>) -> Harness {
    let tmp = TempDir::new().unwrap();
    let storage_dir: PathBuf = tmp.path().to_path_buf();

    let std_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    let listener = TcpListener::from_std(std_listener).unwrap();

    let cas = FsCas::new(storage_dir.clone()).unwrap();
    let config = ServerConfig::with_bind(addr);
    let state = AppState {
        cas: Arc::new(cas),
        config: Arc::new(config),
        upstream: None,
        bearer,
    };

    // Merge the barista-native HTTP router with the REAPI gRPC router —
    // the same coexistence the production assembly builds. Both front
    // `state.cas`.
    let app = axum::Router::new()
        .merge(roastery::proto::barista::router().with_state(state.clone()))
        .merge(reapi::routes(state));

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    wait_for_listener(addr, Duration::from_secs(5)).await;

    Harness {
        addr,
        _tmp: tmp,
        server: Some(server),
    }
}

/// Wait up to `timeout` for `addr` to accept a TCP connection.
async fn wait_for_listener(addr: SocketAddr, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last_err = None;
    while Instant::now() < deadline {
        match TcpStream::connect(addr).await {
            Ok(_) => return,
            Err(e) => {
                last_err = Some(e);
                sleep(Duration::from_millis(20)).await;
            }
        }
    }
    panic!("server at {addr} did not accept connections within {timeout:?}: {last_err:?}");
}

/// Canonical lowercase-hex SHA-256 of a byte slice.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Build a REAPI `Digest` for a byte slice.
fn digest_of(bytes: &[u8]) -> ReapiDigest {
    ReapiDigest {
        hash: sha256_hex(bytes),
        size_bytes: bytes.len() as i64,
    }
}

// -------------------------------------------------------------------
// 1. Capabilities
// -------------------------------------------------------------------

#[tokio::test]
async fn reapi_capabilities_advertises_sha256_and_v2() {
    let h = spawn_server().await;
    let mut client = CapabilitiesClient::new(h.channel().await);

    let resp = client
        .get_capabilities(Request::new(GetCapabilitiesRequest {
            instance_name: String::new(),
        }))
        .await
        .unwrap()
        .into_inner();

    let cache = resp.cache_capabilities.expect("cache_capabilities present");
    assert!(
        cache
            .digest_functions
            .contains(&i32::from(digest_function::Value::Sha256)),
        "SHA256 must be advertised, got {:?}",
        cache.digest_functions
    );
    assert!(
        cache.max_batch_total_size_bytes > 0,
        "max_batch_total_size_bytes must be non-zero"
    );

    let low = resp.low_api_version.expect("low_api_version present");
    let high = resp.high_api_version.expect("high_api_version present");
    assert_eq!(low.major, 2, "low API version major must be 2");
    assert_eq!(high.major, 2, "high API version major must be 2");

    // No Action Cache updates in v0.1.
    assert!(
        !cache
            .action_cache_update_capabilities
            .map(|a| a.update_enabled)
            .unwrap_or(false),
        "action cache update must be disabled in v0.1"
    );
}

// -------------------------------------------------------------------
// 2. FindMissingBlobs
// -------------------------------------------------------------------

#[tokio::test]
async fn reapi_find_missing_blobs_reports_absent() {
    let h = spawn_server().await;
    let mut cas = ContentAddressableStorageClient::new(h.channel().await);

    let a = b"alpha blob".to_vec();
    let b = b"beta blob".to_vec();
    let c = b"gamma blob (never uploaded)".to_vec();

    // Put a + b via BatchUpdate so they are present.
    cas.batch_update_blobs(Request::new(BatchUpdateBlobsRequest {
        instance_name: String::new(),
        requests: vec![
            batch_update_blobs_request::Request {
                digest: Some(digest_of(&a)),
                data: a.clone(),
                compressor: 0,
            },
            batch_update_blobs_request::Request {
                digest: Some(digest_of(&b)),
                data: b.clone(),
                compressor: 0,
            },
        ],
        digest_function: i32::from(digest_function::Value::Sha256),
    }))
    .await
    .unwrap();

    // FindMissingBlobs([a, b, c]) → [c].
    let resp = cas
        .find_missing_blobs(Request::new(FindMissingBlobsRequest {
            instance_name: String::new(),
            blob_digests: vec![digest_of(&a), digest_of(&b), digest_of(&c)],
            digest_function: i32::from(digest_function::Value::Sha256),
        }))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(resp.missing_blob_digests.len(), 1, "exactly c is missing");
    assert_eq!(
        resp.missing_blob_digests[0].hash,
        sha256_hex(&c),
        "the missing digest is c"
    );
}

// -------------------------------------------------------------------
// 3. BatchUpdate → BatchRead round-trip
// -------------------------------------------------------------------

#[tokio::test]
async fn reapi_batch_update_then_batch_read_round_trips() {
    let h = spawn_server().await;
    let mut cas = ContentAddressableStorageClient::new(h.channel().await);

    let blob = b"a small batch blob".to_vec();
    let digest = digest_of(&blob);

    let upd = cas
        .batch_update_blobs(Request::new(BatchUpdateBlobsRequest {
            instance_name: String::new(),
            requests: vec![batch_update_blobs_request::Request {
                digest: Some(digest.clone()),
                data: blob.clone(),
                compressor: 0,
            }],
            digest_function: i32::from(digest_function::Value::Sha256),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(upd.responses.len(), 1);
    assert_ok(upd.responses[0].status.as_ref());

    let read = cas
        .batch_read_blobs(Request::new(BatchReadBlobsRequest {
            instance_name: String::new(),
            digests: vec![digest.clone()],
            acceptable_compressors: Vec::new(),
            digest_function: i32::from(digest_function::Value::Sha256),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(read.responses.len(), 1);
    assert_ok(read.responses[0].status.as_ref());
    assert_eq!(read.responses[0].data, blob, "read bytes equal written bytes");
}

// -------------------------------------------------------------------
// 4. BatchUpdate rejects a per-blob digest mismatch
// -------------------------------------------------------------------

#[tokio::test]
async fn reapi_batch_update_rejects_digest_mismatch() {
    let h = spawn_server().await;
    let mut cas = ContentAddressableStorageClient::new(h.channel().await);

    let real = b"the actual bytes".to_vec();
    // Claim the digest of some OTHER content while sending `real`.
    let lie = digest_of(b"completely different content");
    // Keep the claimed size consistent with the bytes so the failure is
    // a *hash* mismatch, not a trivially-detected size mismatch.
    let claimed = ReapiDigest {
        hash: lie.hash.clone(),
        size_bytes: real.len() as i64,
    };

    let resp = cas
        .batch_update_blobs(Request::new(BatchUpdateBlobsRequest {
            instance_name: String::new(),
            requests: vec![batch_update_blobs_request::Request {
                digest: Some(claimed),
                data: real,
                compressor: 0,
            }],
            digest_function: i32::from(digest_function::Value::Sha256),
        }))
        .await
        .unwrap()
        .into_inner();

    // The whole RPC succeeds; the offending blob carries its own
    // INVALID_ARGUMENT status.
    assert_eq!(resp.responses.len(), 1);
    let status = resp.responses[0].status.as_ref().expect("per-blob status");
    assert_eq!(
        status.code,
        tonic::Code::InvalidArgument as i32,
        "digest mismatch must be a per-blob INVALID_ARGUMENT, got code {}",
        status.code
    );
}

// -------------------------------------------------------------------
// 5. ByteStream Write → Read round-trip
// -------------------------------------------------------------------

#[tokio::test]
async fn reapi_bytestream_write_then_read_round_trips() {
    let h = spawn_server().await;
    let mut bs = ByteStreamClient::new(h.channel().await);

    // A blob larger than one ByteStream chunk to exercise multi-chunk
    // read streaming (64 KiB chunk size).
    let blob: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let hash = sha256_hex(&blob);
    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    let write_name = format!("uploads/{uuid}/blobs/{hash}/{}", blob.len());

    // Write the blob in two chunks via a client-streaming request.
    let chunk = 128 * 1024;
    let first = blob[..chunk].to_vec();
    let second = blob[chunk..].to_vec();
    let name_for_stream = write_name.clone();
    let first_len = first.len() as i64;
    let reqs = futures_stream(vec![
        WriteRequest {
            resource_name: name_for_stream,
            write_offset: 0,
            finish_write: false,
            data: first,
        },
        WriteRequest {
            resource_name: String::new(),
            write_offset: first_len,
            finish_write: true,
            data: second,
        },
    ]);
    let wresp = bs.write(Request::new(reqs)).await.unwrap().into_inner();
    assert_eq!(
        wresp.committed_size,
        blob.len() as i64,
        "committed_size equals blob length"
    );

    // Read it back via the read grammar and concatenate the chunks.
    let read_name = format!("blobs/{hash}/{}", blob.len());
    let mut stream = bs
        .read(Request::new(ReadRequest {
            resource_name: read_name,
            read_offset: 0,
            read_limit: 0,
        }))
        .await
        .unwrap()
        .into_inner();

    let mut got = Vec::new();
    while let Some(resp) = stream.message().await.unwrap() {
        got.extend_from_slice(&resp.data);
    }
    assert_eq!(got, blob, "read bytes equal written bytes");
}

// -------------------------------------------------------------------
// 6. ByteStream Read of an absent blob → NOT_FOUND
// -------------------------------------------------------------------

#[tokio::test]
async fn reapi_bytestream_read_missing_returns_not_found() {
    let h = spawn_server().await;
    let mut bs = ByteStreamClient::new(h.channel().await);

    let absent = sha256_hex(b"this blob was never uploaded");
    let name = format!("blobs/{absent}/27");

    let result = bs
        .read(Request::new(ReadRequest {
            resource_name: name,
            read_offset: 0,
            read_limit: 0,
        }))
        .await;

    match result {
        // The status may surface on the initial call or on the first
        // stream poll depending on tonic buffering; handle both.
        Err(status) => assert_eq!(status.code(), tonic::Code::NotFound),
        Ok(resp) => {
            let mut stream = resp.into_inner();
            let err = stream
                .message()
                .await
                .expect_err("reading an absent blob must error");
            assert_eq!(err.code(), tonic::Code::NotFound);
        }
    }
}

// -------------------------------------------------------------------
// 7. GetTree → UNIMPLEMENTED
// -------------------------------------------------------------------

#[tokio::test]
async fn reapi_get_tree_unimplemented() {
    let h = spawn_server().await;
    let mut cas = ContentAddressableStorageClient::new(h.channel().await);

    let result = cas
        .get_tree(Request::new(GetTreeRequest {
            instance_name: String::new(),
            root_digest: Some(digest_of(b"any root")),
            page_size: 0,
            page_token: String::new(),
            digest_function: i32::from(digest_function::Value::Sha256),
        }))
        .await;

    match result {
        Err(status) => assert_eq!(status.code(), tonic::Code::Unimplemented),
        Ok(resp) => {
            let mut stream = resp.into_inner();
            let err = stream
                .message()
                .await
                .expect_err("GetTree must be unimplemented on a flat CAS");
            assert_eq!(err.code(), tonic::Code::Unimplemented);
        }
    }
}

// -------------------------------------------------------------------
// 8. Cross-protocol storage sharing (headline)
// -------------------------------------------------------------------

#[tokio::test]
async fn reapi_and_barista_protocol_share_storage() {
    let h = spawn_server().await;

    let blob = b"shared across both protocols".to_vec();
    let hash = sha256_hex(&blob);

    // PUT via the barista-native HTTP surface.
    let http = reqwest::Client::new();
    let put = http
        .put(format!("{}/v1/cas/sha256/{hash}", h.http_url()))
        .body(blob.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), reqwest::StatusCode::CREATED, "HTTP PUT created");

    // Read it back via REAPI BatchReadBlobs.
    let mut cas = ContentAddressableStorageClient::new(h.channel().await);
    let read = cas
        .batch_read_blobs(Request::new(BatchReadBlobsRequest {
            instance_name: String::new(),
            digests: vec![digest_of(&blob)],
            acceptable_compressors: Vec::new(),
            digest_function: i32::from(digest_function::Value::Sha256),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(read.responses.len(), 1);
    assert_ok(read.responses[0].status.as_ref());
    assert_eq!(
        read.responses[0].data, blob,
        "REAPI BatchRead sees the HTTP-PUT blob byte-for-byte"
    );

    // And via ByteStream.Read.
    let mut bs = ByteStreamClient::new(h.channel().await);
    let mut stream = bs
        .read(Request::new(ReadRequest {
            resource_name: format!("blobs/{hash}/{}", blob.len()),
            read_offset: 0,
            read_limit: 0,
        }))
        .await
        .unwrap()
        .into_inner();
    let mut got = Vec::new();
    while let Some(resp) = stream.message().await.unwrap() {
        got.extend_from_slice(&resp.data);
    }
    assert_eq!(got, blob, "REAPI ByteStream.Read sees the HTTP-PUT blob");
}

// -------------------------------------------------------------------
// 9. Auth: CAS data services require a bearer token when configured
// -------------------------------------------------------------------

#[tokio::test]
async fn reapi_cas_requires_auth_when_configured() {
    let h = spawn_server_with_bearer("ci", "s3cret-token").await;

    // No credential → BatchReadBlobs is rejected with UNAUTHENTICATED.
    let mut anon = ContentAddressableStorageClient::new(h.channel().await);
    let err = anon
        .batch_read_blobs(Request::new(BatchReadBlobsRequest {
            instance_name: String::new(),
            digests: vec![digest_of(b"anything")],
            acceptable_compressors: Vec::new(),
            digest_function: i32::from(digest_function::Value::Sha256),
        }))
        .await
        .expect_err("unauthenticated BatchReadBlobs must be rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // With a valid bearer token in the `authorization` metadata → OK.
    let channel = h.channel().await;
    let token: tonic::metadata::MetadataValue<_> = "Bearer s3cret-token".parse().unwrap();
    let mut authed = ContentAddressableStorageClient::with_interceptor(
        channel,
        move |mut req: Request<()>| {
            req.metadata_mut().insert("authorization", token.clone());
            Ok(req)
        },
    );
    let resp = authed
        .batch_read_blobs(Request::new(BatchReadBlobsRequest {
            instance_name: String::new(),
            digests: vec![digest_of(b"anything")],
            acceptable_compressors: Vec::new(),
            digest_function: i32::from(digest_function::Value::Sha256),
        }))
        .await
        .expect("authenticated BatchReadBlobs must be accepted")
        .into_inner();
    // The blob isn't present, but the call was authorized: a single
    // NOT_FOUND per-blob status, not a transport-level rejection.
    assert_eq!(resp.responses.len(), 1);
    assert_eq!(
        resp.responses[0].status.as_ref().unwrap().code,
        tonic::Code::NotFound as i32
    );

    // Capabilities stays public even with bearer configured.
    let mut caps = CapabilitiesClient::new(h.channel().await);
    caps.get_capabilities(Request::new(GetCapabilitiesRequest {
        instance_name: String::new(),
    }))
    .await
    .expect("Capabilities is the public negotiation surface");
}

// -------------------------------------------------------------------
// helpers
// -------------------------------------------------------------------

/// Assert a `google.rpc.Status` is OK (code 0).
fn assert_ok(status: Option<&google_rpc::Status>) {
    let s = status.expect("per-blob status present");
    assert_eq!(s.code, tonic::Code::Ok as i32, "expected OK, got {s:?}");
}

/// Build a `Stream` of items for a client-streaming request.
fn futures_stream<T: Send + 'static>(
    items: Vec<T>,
) -> impl futures_util::Stream<Item = T> + Send + 'static {
    futures_util::stream::iter(items)
}
