//! Local content-addressed cache for Barista artifacts.
//!
//! The cache stores artifact bytes by SHA-256 digest under a
//! 256-way fan-out tree (`objects/<aa>/<full-hex>`) and writes
//! every entry atomically via tmp-file + `rename(2)`. See the
//! [`cas`] module for the on-disk substrate, and [`checksum`]
//! for sidecar-driven verification of downloaded artifacts.
//!
//! Higher-level pieces (index/journal, fetcher, GC) layer on top
//! of `cas` and will land in subsequent modules.

pub mod cas;
pub mod checksum;

pub use cas::{Cas, CasError, ContentHash};
pub use checksum::{Algorithm, ChecksumError, ChecksumExpected, Verification, verify};
