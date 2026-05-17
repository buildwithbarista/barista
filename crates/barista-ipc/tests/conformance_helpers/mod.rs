// Test-support module hub for the cross-language Rust↔Java conformance
// harness. Loaded via `mod conformance_helpers;` from
// `tests/conformance.rs` (UDS variant) and `tests/conformance_pipe.rs`
// (named-pipe variant).
//
// This file is intentionally not `#[cfg(test)]`: integration tests
// under `tests/` are already compiled in test context, so the extra
// gate is redundant and would just hide the helpers from
// rust-analyzer.
//
// # Layout
//
//   * [`jvm`] — cross-platform JVM ceremony (Maven test-compile,
//     classpath resolution, `java` binary lookup). No `cfg` gating.
//   * [`uds`] — `#[cfg(unix)]` UDS spawn for `EchoServerCli` (Java =
//     server). Used by `tests/conformance.rs`.
//   * [`pipe`] — `#[cfg(windows)]` named-pipe spawn for
//     `EchoPipeClientCli` (Java = client). Used by
//     `tests/conformance_pipe.rs`.
//
// The role inversion between the two transports (Java binds on UDS,
// Java connects on pipes) is documented in
// [`pipe`]'s module-level doc-comment.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions,
    dead_code
)]

pub mod jvm;

#[cfg(unix)]
pub mod uds;

#[cfg(unix)]
pub use uds::{JavaEchoServer, raw_send_frame, raw_uds_connect};

#[cfg(windows)]
pub mod pipe;

#[cfg(windows)]
pub use pipe::{JavaEchoPipeClient, unique_test_pipe_name};
