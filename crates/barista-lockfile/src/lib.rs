//! barista.lock parser, serializer, and diff renderer.
//!
//! The [`schema`] module defines the on-disk TOML format
//! (`barista.lock`) and provides `serde`-based read/write. The
//! [`diff`] module renders semantic diffs between two lockfile
//! snapshots in a code-review-friendly format. (The diff module
//! currently operates on a minimal [`diff::LockEntry`] subset of
//! the full schema — see its module docs.)
//!
//! The [`signature`] module computes the project signature — the
//! SHA-256 digest the lockfile carries to detect "the source tree
//! changed; the lockfile is stale" in `--frozen` validation mode.

pub mod diff;
pub mod schema;
pub mod signature;

pub use schema::{
    Exclusion, LOCKFILE_SCHEMA_VERSION, Lockfile, LockfileEntry, LockfileError, Meta, MirrorRef,
    ReactorEntry, RepositoryRef, SettingsSnapshot,
};
pub use signature::{ReactorModule, SignatureError, compute_signature};
