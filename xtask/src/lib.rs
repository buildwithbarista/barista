// SPDX-License-Identifier: MIT OR Apache-2.0

//! `xtask` — workspace task runner, exposed both as a `[[bin]]` (so
//! `cargo xtask <sub>` works) and as a `[lib]` (so integration tests
//! under `tests/` can call the subcommand entry points without
//! shelling out).
//!
//! Subcommand modules:
//!
//! - [`security`] — locally-runnable security suite.
//! - [`findings`] — efficiency-findings catalog: list + promote drafts.
//!
//! Test code in the subcommand modules (`#[cfg(test)] mod tests`) is
//! allowed to use `unwrap`/`expect`/`panic` — the workspace lint
//! policy warns on those in production code but tests use them
//! liberally to keep failure messages compact. The same allow lives
//! in `main.rs` for the binary build; we mirror it here for the
//! library build.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod findings;
pub mod security;
