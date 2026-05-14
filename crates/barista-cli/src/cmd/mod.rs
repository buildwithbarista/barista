//! Subcommand implementations.
//!
//! Each module corresponds to a top-level (or nested) command from
//! [`crate::cli`]. The router in `cli::dispatch` forwards parsed args
//! here; each module's `run` returns the process exit code.

pub mod dial_in;
pub mod grind;
pub mod maven_vocab;
pub mod pour;
pub mod pull;
pub mod wrapper;

pub use maven_vocab::MavenPhase;
