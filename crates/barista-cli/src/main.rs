// See `lib.rs` for the per-crate clippy security-lint allow rationale.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! The `barista` CLI entry point.
//!
//! Parses argv with `clap` and hands off to [`barista_cli::cli::dispatch`].
//! The actual command implementations live in sibling modules under
//! `barista_cli::cli` and are wired in piecemeal as later milestones land.

use barista_cli::cli::{Cli, dispatch};
use clap::Parser;

fn main() {
    let cli = Cli::parse();
    std::process::exit(dispatch(cli));
}
