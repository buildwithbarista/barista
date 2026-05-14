//! Barista CLI library — used by the `barista` binary.
//!
//! The CLI surface is defined declaratively with `clap` derive
//! macros in [`cli`]. The binary entry point in `main.rs` is a
//! thin wrapper around [`cli::Cli::parse`] + [`cli::dispatch`];
//! exposing the parser as a library lets integration tests
//! construct a `Cli` from an argv slice without re-invoking the
//! process.

pub mod cli;
