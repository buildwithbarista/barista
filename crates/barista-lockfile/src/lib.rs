//! barista.lock parser, serializer, and diff renderer.
//!
//! The lockfile schema and TOML (de)serializer land in a subsequent
//! milestone. The [`diff`] module is a spike prototype that operates
//! on a minimal [`diff::LockEntry`] subset of the eventual schema,
//! to validate the readability of the semantic diff renderer ahead
//! of the full implementation.

pub mod diff;
