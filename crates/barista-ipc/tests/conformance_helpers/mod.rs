// SPDX-License-Identifier: MIT OR Apache-2.0

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
    dead_code,
    // The pub re-exports below (`JavaEchoServer`, `raw_*`,
    // `JavaEchoPipeClient`, `unique_test_pipe_name`) are consumed by
    // a subset of the integration-test binaries that mount this
    // module via `mod conformance_helpers;`. Test binaries that only
    // need the `jvm` ceremony (e.g. `crash_recovery_conformance`)
    // trigger `unused_imports` on the re-exports they don't consume.
    // Suppressing here keeps the shared module re-exports stable.
    unused_imports
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
