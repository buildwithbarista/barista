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
//! - [`server`] — the `axum::Router` assembly and the
//!   graceful-shutdown loop. Subsequent milestones bolt their routes,
//!   layers, and gRPC services onto the extension points reserved in
//!   this module.
//! - [`error`] — the crate-local `RoasteryError` enum and `Result`
//!   alias.

pub mod config;
pub mod error;
pub mod server;

pub use config::{ServerConfig, TlsConfig};
pub use error::{Result, RoasteryError};
pub use server::{init_tracing, run};
