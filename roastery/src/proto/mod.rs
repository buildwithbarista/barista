// SPDX-License-Identifier: MIT OR Apache-2.0

//! Wire-protocol handlers for the roastery server.
//!
//! Each submodule owns one protocol surface and exposes a single
//! `router(state)` constructor that mounts its routes on a fresh
//! `axum::Router` keyed off the shared [`crate::AppState`]. The
//! top-level [`crate::server`] assembly merges them together.
//!
//! - [`barista`] — the **barista-protocol** REST/JSON surface Barista
//!   clients speak. CAS GET/HEAD/PUT, batch presence, health,
//!   capabilities. Mounted under `/v1/…`.
//! - [`reapi`] — the Bazel **Remote Execution API** gRPC handler:
//!   `ContentAddressableStorage`, `google.bytestream.ByteStream`, and
//!   `Capabilities`, fronting the same `Cas` as the barista surface.
//!   Exposed as an `axum::Router` via [`reapi::routes`] and merged into
//!   the top-level router (tonic 0.14 services are tower/hyper
//!   services, the same shape axum routes are).
//!
//! The split lets each protocol evolve independently and keeps the
//! `server` module free of per-protocol details.

pub mod barista;
pub mod reapi;
