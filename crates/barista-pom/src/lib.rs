// Workspace security lints (clippy::unwrap_used, clippy::expect_used,
// clippy::panic, clippy::as_conversions, unsafe_code) are warned on
// workspace-wide via the root `Cargo.toml`. `unsafe_code` is allowed here
// because the effective-POM resolver (`src/effective.rs`) uses tightly
// scoped `unsafe` blocks for set-environment side effects in a controlled
// test-helper path; the invariants are documented inline. The other lints
// are allowed pending an incremental ratchet of the existing parser code.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions,
    unsafe_code
)]

//! POM (Project Object Model) parsing for Maven projects.
//!
//! Provides the raw parser ([`raw`] module) which deserializes
//! `pom.xml` into a typed struct without interpreting any semantics.
//! Subsequent modules will layer on parent-chain merge, property
//! interpolation, dependency-management application, and profile
//! activation to produce the *effective* POM.
//!
//! ## Quick start
//!
//! ```no_run
//! use barista_pom::parse_pom;
//!
//! let xml = std::fs::read_to_string("pom.xml").unwrap();
//! let pom = parse_pom(&xml).unwrap();
//! println!("{}:{}", pom.group_id.as_deref().unwrap_or("?"), pom.artifact_id);
//! ```

#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod blob;
pub mod effective;
pub mod profile;
pub mod raw;

pub use blob::{BLOB_MAGIC, BLOB_SCHEMA_VERSION, BlobError, PomBlob, read_blob, write_blob};
pub use effective::{
    EffectiveError, EffectivePom, Interpolation, InterpolationLocation, MAX_CHAIN_DEPTH,
    MAX_INTERPOLATION_DEPTH, ParentResolver, build_effective,
};
pub use profile::{
    ActivationContext, MAX_BOM_IMPORT_DEPTH, ResolveError, ResolvedPom, resolve_pom,
};
pub use raw::{
    DependencyManagement, ParseError, Properties, RawActivation, RawActivationFile,
    RawActivationProperty, RawBuild, RawDependency, RawExclusion, RawParent, RawPlugin,
    RawPluginExecution, RawPluginManagement, RawPom, RawProfile, RawRepository,
    RawRepositoryPolicy, RawResource, XmlValue, parse_pom, parse_pom_reader,
};
