// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bazel Remote Execution API (REAPI) gRPC handler.
//!
//! Roastery speaks two wire protocols against one shared
//! content-addressed store: the barista-native REST/JSON surface
//! (`crate::proto::barista`, mounted under `/v1/…`) and the Bazel
//! REAPI gRPC surface implemented here. Both front the same
//! `Arc<dyn Cas>` carried on [`AppState`]; a blob uploaded over one
//! protocol is immediately readable over the other.
//!
//! ## Services implemented (v0.1)
//!
//! - **`ContentAddressableStorage`** —
//!   [`FindMissingBlobs`](CasService::find_missing_blobs),
//!   [`BatchUpdateBlobs`](CasService::batch_update_blobs),
//!   [`BatchReadBlobs`](CasService::batch_read_blobs), and
//!   [`GetTree`](CasService::get_tree). Roastery is a *flat* CAS, not a
//!   Merkle store, so `GetTree` returns `UNIMPLEMENTED` (an honest
//!   answer for a server that has no Directory tree to walk — see the
//!   method docs).
//! - **`google.bytestream.ByteStream`** —
//!   [`Read`](ByteStreamService::read) and
//!   [`Write`](ByteStreamService::write) for large blobs, using the
//!   REAPI resource-name grammar. `QueryWriteStatus` returns
//!   `UNIMPLEMENTED` (v0.1 requires a single contiguous write; resumable
//!   offsets are a v0.2 follow-up).
//! - **`Capabilities`** —
//!   [`GetCapabilities`](CapabilitiesService::get_capabilities)
//!   advertises SHA-256, the batch-size cap, and API version v2.
//!
//! Execution and Action Cache are **not** implemented (v0.1 is a cache,
//! not a remote executor). Those services are present in the compiled
//! proto but have no server-side handler here.
//!
//! ## Mounting alongside the axum HTTP server
//!
//! tonic 0.14 service servers are tower/hyper services, the same shape
//! axum routes are. [`routes`] assembles the three gRPC servers into a
//! [`tonic::service::Routes`] and converts it into an `axum::Router`
//! via `into_axum_router()`; the top-level assembly in `crate::server`
//! `merge`s that into the main router. The gRPC paths
//! (`/build.bazel.remote.execution.v2.*`, `/google.bytestream.ByteStream/*`)
//! never collide with the barista `/v1/…` + ops `/healthz` etc. routes,
//! and content-type negotiation (`application/grpc`) is handled by the
//! generated services.
//!
//! ## Auth
//!
//! The CAS data services (CAS + ByteStream) sit behind the same auth
//! posture as the barista-protocol CAS routes via a gRPC interceptor
//! ([`auth::ReapiAuth`]) that mirrors `crate::auth`'s bearer logic. The
//! `Capabilities` service stays unauthenticated — it is the negotiation
//! surface, exactly like the public HTTP `/v1/capabilities`. mTLS is
//! enforced at the TLS-handshake layer (the rustls server config
//! requires a client cert when mTLS is configured), so the interceptor
//! only needs to enforce the bearer requirement; a connection that
//! completed an mTLS handshake is already authenticated at the
//! transport. See [`auth`] for the precise contract + the documented
//! v0.2 follow-up (per-call mTLS subject extraction over gRPC).

use std::pin::Pin;

use futures_util::Stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::server::AppState;
use crate::storage::Digest;

/// The generated bindings for every vendored proto package, laid out
/// as the real package-module tree (`build::bazel::…`, `google::…`).
///
/// Emitted by `tonic-prost-build` (see `build.rs`) into `$OUT_DIR` as a
/// single `reapi_generated.rs` and `include!`d here. A single nested
/// tree is required because the generated cross-package references use
/// relative `super::` paths (e.g. a CAS response referencing
/// `google.rpc.Status`) that only resolve when the package modules
/// share their real ancestors.
// Generated code is not ours to lint. The workspace promotes a few
// `restriction`-group clippy lints (`as_conversions`, …) and rustc
// lints to first-class gates; prost/tonic codegen legitimately uses
// patterns those flag (`enum as i32`, `pub` re-exports, …). Suppress
// the full set on this one module so the gate stays meaningful for our
// handwritten handlers without forcing edits to machine-generated code.
#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::restriction,
    clippy::as_conversions,
    missing_docs,
    unreachable_pub,
    unused_qualifications
)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/reapi_generated.rs"));
}

/// The REAPI v2 message types + service traits
/// (`build.bazel.remote.execution.v2`).
pub use generated::build::bazel::remote::execution::v2 as reapi_v2;
/// The `build.bazel.semver` types (`SemVer`) the `ServerCapabilities`
/// message references for the API version fields.
pub use generated::build::bazel::semver;
/// The `google.bytestream.ByteStream` bindings (large-blob streaming).
pub use generated::google::bytestream;
/// The `google.rpc` types (`Status`) used in the per-blob batch
/// responses.
pub use generated::google::rpc as google_rpc;

use bytestream::byte_stream_server::{ByteStream, ByteStreamServer};
use bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use reapi_v2::capabilities_server::{Capabilities, CapabilitiesServer};
use reapi_v2::content_addressable_storage_server::{
    ContentAddressableStorage, ContentAddressableStorageServer,
};
use reapi_v2::{
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, CacheCapabilities, Digest as ReapiDigest, FindMissingBlobsRequest,
    FindMissingBlobsResponse, GetCapabilitiesRequest, GetTreeRequest, GetTreeResponse,
    ServerCapabilities, batch_read_blobs_response, batch_update_blobs_response, digest_function,
    symlink_absolute_path_strategy,
};

pub mod auth;
pub mod resource;

pub use auth::ReapiAuth;

/// Maximum total size, in bytes, of blobs roastery accepts or returns
/// through the REAPI batch methods (`BatchUpdateBlobs` /
/// `BatchReadBlobs`). Advertised to clients as
/// `CacheCapabilities.max_batch_total_size_bytes`.
///
/// 4 MiB is the conventional REAPI batch ceiling: it sits comfortably
/// under the default gRPC 4 MiB message limit while staying large
/// enough for the small blobs (POMs, metadata, small classes) the batch
/// path is meant for. Blobs above this go through the ByteStream
/// service instead. The cap is the REAPI analogue of the barista
/// protocol's `cas.max_batch_missing` knob.
pub const MAX_BATCH_TOTAL_SIZE_BYTES: i64 = 4 * 1024 * 1024;

/// Chunk size for the ByteStream `Read` server-stream. Each
/// `ReadResponse` carries at most this many bytes. 64 KiB matches the
/// de-facto REAPI client expectation and keeps individual gRPC frames
/// well under the message-size limit.
const BYTESTREAM_CHUNK_SIZE: usize = 64 * 1024;

/// Assemble the three implemented REAPI gRPC services into an
/// `axum::Router` ready to be merged into the top-level server router.
///
/// The CAS + ByteStream services are wrapped with the bearer-auth
/// interceptor derived from `state`; `Capabilities` is left
/// unauthenticated (the negotiation surface). The returned router is
/// stateless from axum's perspective — each gRPC service owns its own
/// `AppState` clone, so the result needs no `.with_state(...)`.
pub fn routes(state: AppState) -> axum::Router {
    let interceptor = ReapiAuth::from_state(&state);

    let cas = ContentAddressableStorageServer::with_interceptor(
        CasService {
            state: state.clone(),
        },
        interceptor.clone(),
    );
    let bs = ByteStreamServer::with_interceptor(
        ByteStreamService {
            state: state.clone(),
        },
        interceptor,
    );
    // Capabilities is the negotiation surface — unauthenticated, like
    // the public HTTP `/v1/capabilities`.
    let caps = CapabilitiesServer::new(CapabilitiesService { state });

    tonic::service::Routes::builder()
        .routes()
        .add_service(cas)
        .add_service(bs)
        .add_service(caps)
        .into_axum_router()
}

// -------------------------------------------------------------------
// ContentAddressableStorage
// -------------------------------------------------------------------

/// REAPI `ContentAddressableStorage` server backed by the shared
/// [`crate::storage::Cas`].
struct CasService {
    state: AppState,
}

/// Parse a REAPI `Digest{hash, size_bytes}` into the crate's
/// [`Digest`] newtype, enforcing the SHA-256 hex contract. Returns a
/// gRPC `INVALID_ARGUMENT` status on a malformed hash so the client
/// learns the digest is unusable rather than seeing a generic error.
fn parse_reapi_digest(d: &ReapiDigest) -> Result<Digest, Status> {
    Digest::from_hex(&d.hash)
        .map_err(|e| Status::invalid_argument(format!("invalid digest hash {:?}: {e}", d.hash)))
}

#[tonic::async_trait]
impl ContentAddressableStorage for CasService {
    /// `FindMissingBlobs` — return the subset of supplied digests NOT
    /// present in the store. Mirrors the barista-protocol
    /// `POST /v1/cas/missing` logic: a sequential `cas.stat` per digest
    /// (fine for the handful-to-dozens batch a build issues during
    /// resolution), honoring the same batch cap.
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        let req = request.into_inner();

        if req.blob_digests.len() > crate::proto::barista::MAX_BATCH_MISSING {
            return Err(Status::invalid_argument(format!(
                "batch size {} exceeds the per-call cap of {}",
                req.blob_digests.len(),
                crate::proto::barista::MAX_BATCH_MISSING
            )));
        }

        let mut missing = Vec::new();
        for rd in &req.blob_digests {
            let digest = parse_reapi_digest(rd)?;
            if self
                .state
                .cas
                .stat(digest)
                .await
                .map_err(storage_to_status)?
                .is_none()
            {
                // Echo the client's original Digest (hash + size) so it
                // can correlate by identity.
                missing.push(rd.clone());
            }
        }

        Ok(Response::new(FindMissingBlobsResponse {
            missing_blob_digests: missing,
        }))
    }

    /// `BatchUpdateBlobs` — upload a batch of small blobs. Each entry is
    /// verified independently: the bytes are hashed through `cas.put`
    /// against the claimed `Digest.hash`, and the per-blob `status`
    /// records `OK` or `INVALID_ARGUMENT` (digest mismatch / bad hash /
    /// size mismatch). A single bad blob does NOT fail the whole batch —
    /// the RPC succeeds and the offending entry carries its own error
    /// status, per the REAPI contract.
    async fn batch_update_blobs(
        &self,
        request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        let req = request.into_inner();

        // Enforce the advertised batch-total-size cap up front. Sum in
        // `u64` (lengths are `usize`) and compare against the cap; the
        // cap is small and positive so the `u64::try_from` of it never
        // fails.
        let total: u64 = req.requests.iter().map(|r| len_u64(r.data.len())).sum();
        let cap = u64::try_from(MAX_BATCH_TOTAL_SIZE_BYTES).unwrap_or(u64::MAX);
        if total > cap {
            return Err(Status::invalid_argument(format!(
                "batch total size {total} exceeds max_batch_total_size_bytes {MAX_BATCH_TOTAL_SIZE_BYTES}"
            )));
        }

        let mut responses = Vec::with_capacity(req.requests.len());
        for entry in req.requests {
            let claimed = entry.digest.clone().unwrap_or_default();
            let status = self.put_one(&claimed, entry.data).await;
            responses.push(batch_update_blobs_response::Response {
                digest: Some(claimed),
                status: Some(status),
            });
        }

        Ok(Response::new(BatchUpdateBlobsResponse { responses }))
    }

    /// `BatchReadBlobs` — read a batch of small blobs by digest. Each
    /// entry carries either the data with an `OK` status or empty data
    /// with `NOT_FOUND`. Like the update path, one missing blob does not
    /// fail the batch.
    async fn batch_read_blobs(
        &self,
        request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        let req = request.into_inner();

        let mut responses = Vec::with_capacity(req.digests.len());
        for rd in req.digests {
            let response = self.read_one(rd).await;
            responses.push(response);
        }

        Ok(Response::new(BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream =
        Pin<Box<dyn Stream<Item = Result<GetTreeResponse, Status>> + Send + 'static>>;

    /// `GetTree` — REAPI Merkle-tree walk. Roastery v0.1 is a *flat*
    /// content-addressed store: it has no notion of `Directory` nodes
    /// or a tree to descend, so there is nothing to walk. Returning
    /// `UNIMPLEMENTED` is the honest answer — a client that needs tree
    /// semantics is talking to the wrong server, and a fabricated
    /// single-level response would be a lie. (A future Merkle-aware
    /// store could implement this without a wire-contract change.)
    async fn get_tree(
        &self,
        _request: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        Err(Status::unimplemented(
            "GetTree is not supported: roastery is a flat content-addressed store with no Merkle directory tree (v0.1)",
        ))
    }

    /// `SplitBlob` — content-defined-chunking blob split. Not supported
    /// in v0.1 (roastery stores whole blobs and advertises no chunking
    /// function in its capabilities), so this is `UNIMPLEMENTED`.
    async fn split_blob(
        &self,
        _request: Request<reapi_v2::SplitBlobRequest>,
    ) -> Result<Response<reapi_v2::SplitBlobResponse>, Status> {
        Err(Status::unimplemented(
            "SplitBlob is not supported: roastery advertises no chunking function (v0.1)",
        ))
    }

    /// `SpliceBlob` — reassemble a blob from chunks. The dual of
    /// `SplitBlob`; equally unsupported in v0.1.
    async fn splice_blob(
        &self,
        _request: Request<reapi_v2::SpliceBlobRequest>,
    ) -> Result<Response<reapi_v2::SpliceBlobResponse>, Status> {
        Err(Status::unimplemented(
            "SpliceBlob is not supported: roastery advertises no chunking function (v0.1)",
        ))
    }
}

impl CasService {
    /// Store one batch-update entry, returning the `google.rpc.Status`
    /// for that blob. `OK` on success; `INVALID_ARGUMENT` on a bad hash
    /// or a digest/size mismatch (the bytes don't hash to the claimed
    /// digest). Storage I/O failures map to `INTERNAL`.
    async fn put_one(&self, claimed: &ReapiDigest, data: Vec<u8>) -> google_rpc::Status {
        // Verify the claimed size matches the supplied bytes before we
        // touch storage — a size lie is an INVALID_ARGUMENT, not an I/O
        // error. Compare in `i64` (the proto type); a negative claimed
        // size can never match a real length.
        let actual_len = match i64::try_from(data.len()) {
            Ok(n) => n,
            Err(_) => {
                return status_invalid_argument(format!(
                    "blob length {} does not fit in i64",
                    data.len()
                ));
            }
        };
        if claimed.size_bytes != actual_len {
            return status_invalid_argument(format!(
                "size_bytes {} does not match data length {}",
                claimed.size_bytes, actual_len
            ));
        }

        let digest = match Digest::from_hex(&claimed.hash) {
            Ok(d) => d,
            Err(e) => {
                return status_invalid_argument(format!(
                    "invalid digest hash {:?}: {e}",
                    claimed.hash
                ));
            }
        };

        let reader: crate::storage::CasReader = Box::new(std::io::Cursor::new(data));
        match self.state.cas.put(digest, reader).await {
            Ok(_) => status_ok(),
            Err(crate::error::StorageError::DigestMismatch { .. }) => status_invalid_argument(
                "blob content does not hash to the claimed digest".to_string(),
            ),
            Err(crate::error::StorageError::InvalidDigest { reason }) => {
                status_invalid_argument(format!("invalid digest: {reason}"))
            }
            Err(other) => status_internal(format!("storage error: {other}")),
        }
    }

    /// Read one batch-read entry by digest, returning the per-blob
    /// response (data + OK, or empty + NOT_FOUND, or empty + INTERNAL on
    /// an I/O failure / a digest the store can't even parse).
    async fn read_one(&self, rd: ReapiDigest) -> batch_read_blobs_response::Response {
        let digest = match Digest::from_hex(&rd.hash) {
            Ok(d) => d,
            Err(e) => {
                return batch_read_blobs_response::Response {
                    digest: Some(rd),
                    data: Vec::new(),
                    compressor: 0,
                    status: Some(status_invalid_argument(format!("invalid digest hash: {e}"))),
                };
            }
        };

        match self.state.cas.get(digest).await {
            Ok(Some(mut reader)) => {
                let mut buf = Vec::new();
                match reader.read_to_end(&mut buf).await {
                    Ok(_) => batch_read_blobs_response::Response {
                        digest: Some(rd),
                        data: buf,
                        compressor: 0,
                        status: Some(status_ok()),
                    },
                    Err(e) => batch_read_blobs_response::Response {
                        digest: Some(rd),
                        data: Vec::new(),
                        compressor: 0,
                        status: Some(status_internal(format!("read error: {e}"))),
                    },
                }
            }
            Ok(None) => batch_read_blobs_response::Response {
                digest: Some(rd),
                data: Vec::new(),
                compressor: 0,
                status: Some(status_not_found("blob not found".to_string())),
            },
            Err(e) => batch_read_blobs_response::Response {
                digest: Some(rd),
                data: Vec::new(),
                compressor: 0,
                status: Some(status_internal(format!("storage error: {e}"))),
            },
        }
    }
}

// -------------------------------------------------------------------
// ByteStream
// -------------------------------------------------------------------

/// `google.bytestream.ByteStream` server for streaming large blobs to
/// and from the shared CAS using the REAPI resource-name grammar.
struct ByteStreamService {
    state: AppState,
}

#[tonic::async_trait]
impl ByteStream for ByteStreamService {
    type ReadStream = Pin<Box<dyn Stream<Item = Result<ReadResponse, Status>> + Send + 'static>>;

    /// `Read` — stream a blob out by its resource name. The resource
    /// name follows the REAPI read grammar
    /// `{instance}/blobs/{hash}/{size}` (`{instance}` may be empty).
    /// The blob is read from the CAS and emitted in
    /// [`BYTESTREAM_CHUNK_SIZE`]-byte chunks. An absent blob yields
    /// `NOT_FOUND`; v0.1 does not support a non-zero `read_offset`
    /// (it returns `OUT_OF_RANGE`/`UNIMPLEMENTED` accordingly).
    async fn read(
        &self,
        request: Request<ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        let req = request.into_inner();
        let parsed = resource::parse_read_resource(&req.resource_name)?;

        // v0.1: only a full read from offset 0 is supported.
        if req.read_offset != 0 {
            return Err(Status::unimplemented(
                "non-zero read_offset is not supported (v0.1 serves whole blobs)",
            ));
        }

        let reader = self
            .state
            .cas
            .get(parsed.digest)
            .await
            .map_err(storage_to_status)?;
        let Some(mut reader) = reader else {
            return Err(Status::not_found(format!(
                "no blob in store for {}",
                parsed.digest
            )));
        };

        // Stream chunks through a bounded channel so we never buffer the
        // whole blob in memory. The reader task fills the channel; the
        // gRPC layer drains it as the client reads.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ReadResponse, Status>>(4);
        tokio::spawn(async move {
            let mut buf = vec![0u8; BYTESTREAM_CHUNK_SIZE];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = ReadResponse {
                            data: buf[..n].to_vec(),
                        };
                        if tx.send(Ok(chunk)).await.is_err() {
                            // Client hung up; stop reading.
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Err(Status::internal(format!("read error: {e}"))))
                            .await;
                        return;
                    }
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }

    /// `Write` — stream a blob in. The resource name follows the REAPI
    /// write grammar `{instance}/uploads/{uuid}/blobs/{hash}/{size}`.
    /// All `WriteRequest` chunks are concatenated and verified through
    /// `cas.put` against the resource-name digest; a content/digest
    /// mismatch surfaces as `INVALID_ARGUMENT`.
    ///
    /// v0.1 requires a single contiguous write: the first chunk must
    /// carry the resource name and `write_offset == 0`, and offsets must
    /// advance contiguously. Resumable writes (re-attaching at a
    /// `committed_size` from `QueryWriteStatus`) are a v0.2 follow-up.
    async fn write(
        &self,
        request: Request<tonic::Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let mut stream = request.into_inner();

        // The expected blob identity, learned from the first chunk's
        // resource name.
        let mut expected: Option<resource::WriteResource> = None;
        // A pipe that feeds the collected bytes into `cas.put` as they
        // arrive, so we never buffer the whole blob in memory. The read
        // half is moved into the put task once we know the digest (first
        // chunk); `Option::take` guarantees that single move even though
        // the move site is inside the receive loop.
        let (writer, reader) = tokio::io::duplex(BYTESTREAM_CHUNK_SIZE);
        let mut writer = writer;
        let mut reader = Some(reader);
        let cas = self.state.cas.clone();

        // Spawn the put against the CAS; it consumes the read end of the
        // duplex pipe. We learn the digest from the first chunk, so the
        // put task is started lazily once we have it.
        let mut put_handle: Option<
            tokio::task::JoinHandle<crate::storage::Result<crate::storage::Stat>>,
        > = None;

        let mut next_offset: i64 = 0;
        let mut saw_finish = false;
        let mut committed: i64 = 0;

        while let Some(chunk) = stream.message().await? {
            // First chunk establishes the resource + starts the put.
            if expected.is_none() {
                let parsed = resource::parse_write_resource(&chunk.resource_name)?;
                if chunk.write_offset != 0 {
                    return Err(Status::invalid_argument(
                        "first WriteRequest must have write_offset == 0 (v0.1 requires a single contiguous write)",
                    ));
                }
                let digest = parsed.digest;
                // `take` the read half + clone the CAS handle for the
                // task; both happen exactly once (guarded by
                // `expected.is_none()`). The `None` arm is unreachable
                // given that guard, but we surface it as an INTERNAL
                // status rather than panicking, per the no-panic policy.
                let Some(read_half) = reader.take() else {
                    return Err(Status::internal("write pipe reader already consumed"));
                };
                let cas = cas.clone();
                put_handle = Some(tokio::spawn(async move {
                    let r: crate::storage::CasReader = Box::new(read_half);
                    cas.put(digest, r).await
                }));
                expected = Some(parsed);
            } else if chunk.write_offset != next_offset {
                return Err(Status::invalid_argument(format!(
                    "non-contiguous write_offset {} (expected {next_offset}); resumable writes are not supported in v0.1",
                    chunk.write_offset
                )));
            }

            if saw_finish && !chunk.data.is_empty() {
                return Err(Status::invalid_argument(
                    "received data after finish_write was set",
                ));
            }

            if !chunk.data.is_empty() {
                writer
                    .write_all(&chunk.data)
                    .await
                    .map_err(|e| Status::internal(format!("write pipe error: {e}")))?;
                let n = i64::try_from(chunk.data.len()).map_err(|_| {
                    Status::invalid_argument("write chunk length does not fit in i64")
                })?;
                next_offset += n;
                committed += n;
            }

            if chunk.finish_write {
                saw_finish = true;
            }
        }

        if expected.is_none() {
            return Err(Status::invalid_argument(
                "empty Write stream: no WriteRequest carried a resource name",
            ));
        }

        // Close the write end so the put task's reader sees EOF, then
        // await the put result.
        writer
            .shutdown()
            .await
            .map_err(|e| Status::internal(format!("write shutdown error: {e}")))?;
        drop(writer);

        if let Some(handle) = put_handle {
            match handle.await {
                Ok(Ok(_stat)) => {}
                Ok(Err(crate::error::StorageError::DigestMismatch { .. })) => {
                    return Err(Status::invalid_argument(
                        "blob content does not hash to the resource-name digest",
                    ));
                }
                Ok(Err(e)) => return Err(storage_to_status(e)),
                Err(join) => {
                    return Err(Status::internal(format!("write task panicked: {join}")));
                }
            }
        }

        Ok(Response::new(WriteResponse {
            committed_size: committed,
        }))
    }

    /// `QueryWriteStatus` — resumable-write status. v0.1 requires a
    /// single contiguous `Write`, so there is no partial-write state to
    /// query; `UNIMPLEMENTED` is the honest answer. (v0.2 resumable
    /// writes would implement this against per-upload tracking.)
    async fn query_write_status(
        &self,
        _request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        Err(Status::unimplemented(
            "QueryWriteStatus is not supported: v0.1 requires a single contiguous Write (no resumable uploads)",
        ))
    }
}

// -------------------------------------------------------------------
// Capabilities
// -------------------------------------------------------------------

/// REAPI `Capabilities` server. Advertises the static v0.1 cache
/// capabilities — there is no per-deployment variation worth surfacing
/// beyond what's compiled in.
struct CapabilitiesService {
    state: AppState,
}

#[tonic::async_trait]
impl Capabilities for CapabilitiesService {
    /// `GetCapabilities` — advertise the CAS capabilities:
    /// `digest_functions: [SHA256]`, the batch-total-size cap, the
    /// symlink strategy, and API version low/high = v2.0. Action Cache
    /// update is disabled (no Action Cache in v0.1) and no execution
    /// capabilities are advertised (roastery is a cache, not an
    /// executor).
    async fn get_capabilities(
        &self,
        _request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<ServerCapabilities>, Status> {
        let _ = &self.state;
        let v2 = semver::SemVer {
            major: 2,
            minor: 0,
            patch: 0,
            prerelease: String::new(),
        };

        let cache = CacheCapabilities {
            // prost enums implement `From<Value> for i32`; use that
            // rather than an `as` cast to satisfy the workspace
            // `as_conversions` lint.
            digest_functions: vec![i32::from(digest_function::Value::Sha256)],
            // No Action Cache in v0.1 — updates are disabled.
            action_cache_update_capabilities: Some(reapi_v2::ActionCacheUpdateCapabilities {
                update_enabled: false,
            }),
            max_batch_total_size_bytes: MAX_BATCH_TOTAL_SIZE_BYTES,
            symlink_absolute_path_strategy: i32::from(
                symlink_absolute_path_strategy::Value::Disallowed,
            ),
            // The remaining fields (priority ranges, compressor lists,
            // chunking params, max blob size, …) are all "unset / none"
            // for the v0.1 CAS — `Default` gives the right zero values
            // and keeps this resilient to additive REAPI fields on a
            // future proto bump.
            ..Default::default()
        };

        Ok(Response::new(ServerCapabilities {
            cache_capabilities: Some(cache),
            // No remote-execution surface in v0.1.
            execution_capabilities: None,
            deprecated_api_version: None,
            low_api_version: Some(v2.clone()),
            high_api_version: Some(v2),
        }))
    }
}

// -------------------------------------------------------------------
// Shared helpers
// -------------------------------------------------------------------

/// Convert a slice/`Vec` length (`usize`) to `u64` without an `as`
/// cast. `usize` is at most 64 bits on every target roastery builds
/// for, so `try_from` cannot actually fail; the `unwrap_or(u64::MAX)`
/// is a defensive saturate that keeps the function total and the
/// `as_conversions` lint satisfied.
fn len_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Map a [`crate::error::StorageError`] to a gRPC [`Status`]. Bad
/// digests are the client's fault (`INVALID_ARGUMENT`); everything else
/// is `INTERNAL`.
fn storage_to_status(err: crate::error::StorageError) -> Status {
    match err {
        crate::error::StorageError::InvalidDigest { reason } => {
            Status::invalid_argument(format!("invalid digest: {reason}"))
        }
        crate::error::StorageError::DigestMismatch { .. } => {
            Status::invalid_argument("blob content does not hash to the claimed digest")
        }
        other => Status::internal(format!("storage error: {other}")),
    }
}

/// Build a `google.rpc.Status` carrying a canonical gRPC code +
/// message. The numeric `code` matches the `google.rpc.Code` /
/// `tonic::Code` mapping (OK=0, INVALID_ARGUMENT=3, NOT_FOUND=5,
/// INTERNAL=13). `i32::from(tonic::Code)` is used over an `as` cast to
/// satisfy the workspace `as_conversions` lint.
fn rpc_status(code: tonic::Code, message: String) -> google_rpc::Status {
    google_rpc::Status {
        code: i32::from(code),
        message,
        details: Vec::new(),
    }
}

/// Build a `google.rpc.Status` with code OK (0).
fn status_ok() -> google_rpc::Status {
    rpc_status(tonic::Code::Ok, String::new())
}

/// Build a `google.rpc.Status` with code INVALID_ARGUMENT (3).
fn status_invalid_argument(message: String) -> google_rpc::Status {
    rpc_status(tonic::Code::InvalidArgument, message)
}

/// Build a `google.rpc.Status` with code NOT_FOUND (5).
fn status_not_found(message: String) -> google_rpc::Status {
    rpc_status(tonic::Code::NotFound, message)
}

/// Build a `google.rpc.Status` with code INTERNAL (13).
fn status_internal(message: String) -> google_rpc::Status {
    rpc_status(tonic::Code::Internal, message)
}
