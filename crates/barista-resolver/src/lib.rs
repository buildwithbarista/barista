//! Parallel dependency resolver (BFS+Skipper) for Maven artifacts.
//!
//! The resolver walks dependency graphs by querying a
//! [`MetadataSource`] for POMs and `maven-metadata.xml` payloads. The
//! resolver crate has no knowledge of where those bytes come from —
//! a remote HTTP repository, an on-disk cache, or an in-process test
//! fixture all look the same through this trait.

pub mod source;
pub mod version_spec;
pub mod walker;

pub use source::{
    FetchOrigin, GaMetadata, MetadataError, MetadataSource, NullMetadataSource, ResolveKey,
    VersionString,
};
pub use version_spec::{Bound, Interval, ParseError, SpecWarning, VersionSpec};
pub use walker::{
    AuditEntry, FixtureSource, ResolvedDep, ResolvedGraph, Scope, WalkError, WalkOptions, walk,
};
