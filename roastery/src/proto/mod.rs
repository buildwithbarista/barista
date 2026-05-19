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
//! - [`reapi`] — placeholder for the Remote Execution API gRPC
//!   handler. Empty today; populated by a follow-up task.
//!
//! The split lets each protocol evolve independently and keeps the
//! `server` module free of per-protocol details.

pub mod barista;
pub mod reapi;
