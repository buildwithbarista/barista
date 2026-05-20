// SPDX-License-Identifier: MIT OR Apache-2.0

//! Subcommand implementations.
//!
//! Each module corresponds to a top-level (or nested) command from
//! [`crate::cli`]. The router in `cli::dispatch` forwards parsed args
//! here; each module's `run` returns the process exit code.

pub mod ci_repro;
pub mod dial_in;
pub mod grind;
pub mod maven_vocab;
pub mod no_daemon;
pub mod pour;
pub mod pull;
#[cfg(unix)]
pub mod reactor;
#[cfg(unix)]
pub mod shot;
pub mod tap;
#[cfg(unix)]
pub mod verify;
pub mod wrapper;

pub use maven_vocab::MavenPhase;
