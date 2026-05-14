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
//!
//! The [`mode`] module defines the three validation modes
//! (`Default`, `Frozen`, `Update`) and the decision logic that maps
//! mode + on-disk lockfile + computed signature to an outcome the
//! resolver can act on.

pub mod diff;
pub mod mode;
pub mod schema;
pub mod signature;

pub use mode::{ValidationError, ValidationMode, ValidationOutcome, validate, validate_strict};
pub use schema::{
    Exclusion, LOCKFILE_SCHEMA_VERSION, Lockfile, LockfileEntry, LockfileError, Meta, MirrorRef,
    ReactorEntry, RepositoryRef, SettingsSnapshot,
};
pub use signature::{ReactorModule, SignatureError, compute_signature};
