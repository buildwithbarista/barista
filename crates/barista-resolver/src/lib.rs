// Workspace security lints (clippy::unwrap_used, clippy::expect_used,
// clippy::panic, clippy::as_conversions) are warned on workspace-wide via
// the root `Cargo.toml`. Pre-existing resolver internals are allowed here
// pending an incremental ratchet.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Parallel dependency resolver (BFS+Skipper) for Maven artifacts.
//!
//! The resolver walks dependency graphs by querying a
//! [`MetadataSource`] for POMs and `maven-metadata.xml` payloads. The
//! resolver crate has no knowledge of where those bytes come from —
//! a remote HTTP repository, an on-disk cache, or an in-process test
//! fixture all look the same through this trait.

pub mod oreq;
pub mod skipper;
pub mod snapshot;
pub mod source;
pub mod strict;
pub mod strict_format;
pub mod version_spec;
pub mod walker;

pub use oreq::{MetadataKey, OreqCounters, OreqSession, OreqStats};
pub use skipper::{
    ExclusionPattern, ExclusionSet, SkipDecision, SkipReason, SkipperState, SkipperStats,
};
pub use snapshot::{
    SnapshotError, SnapshotInfo, SnapshotMetadata, SnapshotVersionEntry, UpdatePolicy,
    UpdatePolicyExt, parse_snapshot_metadata,
};
pub use source::{
    FetchOrigin, GaMetadata, MetadataError, MetadataSource, NullMetadataSource, ResolveKey,
    VersionString,
};
pub use strict::{
    DepEdge, ResolvedStrictDep, StrictDerivation, StrictError, StrictOutcome, resolve_strict,
};
pub use strict_format::{format_derivation, pubgrub_range_to_maven};
pub use version_spec::{Bound, Interval, ParseError, SpecWarning, VersionSpec};
pub use walker::{
    AuditEntry, FixtureSource, ResolvedDep, ResolvedGraph, Scope, WalkError, WalkOptions, walk,
};
