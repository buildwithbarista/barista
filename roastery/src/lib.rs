// SPDX-License-Identifier: MIT OR Apache-2.0

//! `roastery` — the remote artifact cache server.
//!
//! This library crate hosts the server's internals so they can be
//! exercised from integration tests under `roastery/tests/` and from
//! the binary entry point in `src/main.rs`. The binary stays slim;
//! everything testable lives here.
//!
//! ## Module layout
//!
//! - [`config`] — `ServerConfig` plus its env-var loader and a
//!   builder helper for tests.
//! - [`storage`] — the content-addressed storage trait
//!   ([`storage::Cas`]) and its filesystem implementation
//!   ([`storage::FsCas`]), plus stub S3 / GCS backends scheduled for
//!   v0.2. Subsequent milestones (the barista-protocol handler in
//!   M5.1 T3, the REAPI gRPC handler in M5.1 T4) call this trait
//!   instead of touching the filesystem directly.
//! - [`server`] — the `axum::Router` assembly, the shared `AppState`
//!   that carries the storage backend, and the graceful-shutdown
//!   loop. Subsequent milestones bolt their routes, layers, and gRPC
//!   services onto the extension points reserved in this module.
//! - [`proto`] — wire-protocol handlers. Each submodule owns one
//!   surface and exposes a `router(state)` constructor the `server`
//!   assembly merges in.
//! - [`ops`] — operational endpoints (`/healthz`, `/metrics`,
//!   `/version`) plus the process-global Prometheus metric registry
//!   the protocol handlers feed counter/histogram updates into.
//! - [`error`] — the crate-local `RoasteryError` enum, the
//!   `StorageError` enum surfaced by the storage layer, and the
//!   crate-wide `Result` alias.
//! - [`auth`] — bearer-token + mTLS authentication. Exposes the
//!   `AuthLayer` (a `tower::Layer` applied to the protected routes),
//!   the loader/verifier helpers (`BearerVerifier`, `MtlsVerifier`),
//!   and the `Principal` enum attached to every authenticated
//!   request.
//! - [`upstream`] — upstream-on-miss fetch. When a `GET` lands on a
//!   digest the local CAS doesn't have and the client supplied an
//!   `X-Barista-Coords` hint, the `UpstreamFetcher` tries each
//!   configured Maven repository in order, verifies the served bytes
//!   against the requested digest, and persists the blob locally
//!   before the response streams to the client.

pub mod auth;
pub mod config;
pub mod error;
pub mod ops;
pub mod proto;
pub mod server;
pub mod storage;
pub mod upstream;

pub use auth::{AuthLayer, BearerVerifier, ClientCertChain, MtlsVerifier, Principal};
pub use config::{
    AuthConfig, BearerAuthConfig, MtlsAuthConfig, ServerConfig, StorageBackend, TlsConfig,
    UpstreamConfig,
};
pub use error::{ErrorBody, Result, RoasteryError, StorageError};
pub use server::{AppState, init_tracing, run};
pub use storage::{Cas, Digest, FsCas, GcsCas, S3Cas, Stat};
pub use upstream::{Coords, UpstreamError, UpstreamFetcher};
