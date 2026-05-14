// Workspace security lints (clippy::unwrap_used, clippy::expect_used,
// clippy::panic, clippy::as_conversions) are warned on workspace-wide via
// the root `Cargo.toml`. Pre-existing uses in this crate's CLI dispatch
// glue and tests are allowed here while the codebase incrementally
// ratchets them down; new code in this crate should prefer `?` propagation
// or typed errors over `unwrap()`/`expect()`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Barista CLI library — used by the `barista` binary.
//!
//! The CLI surface is defined declaratively with `clap` derive
//! macros in [`cli`]. The binary entry point in `main.rs` is a
//! thin wrapper around [`cli::Cli::parse`] + [`cli::dispatch`];
//! exposing the parser as a library lets integration tests
//! construct a `Cli` from an argv slice without re-invoking the
//! process.

pub mod cli;
pub mod cmd;
pub mod output;
pub mod project;

pub use project::{
    ProjectRoot, ResolveError, ResolveInputs, RootSource, record_sticky, resolve_project_root,
};
