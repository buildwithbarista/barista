// Workspace security lints (clippy::unwrap_used, clippy::expect_used,
// clippy::panic, clippy::as_conversions, unsafe_code) are warned on
// workspace-wide via the root `Cargo.toml`. `unsafe_code` is allowed here
// because the file-locking implementation (`src/lock.rs`) holds a
// short-lived `&'static mut` borrow whose lifetime invariants are
// documented inline next to the `unsafe` block; the rest of the lints are
// allowed pending an incremental ratchet of the existing call sites.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions,
    unsafe_code
)]

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
pub mod fetch;
pub mod gc;
pub mod index;
pub mod journal;
pub mod lock;
pub mod m2;
pub mod recovery;
pub mod source;

pub use cas::{Cas, CasError, ContentHash};
pub use checksum::{Algorithm, ChecksumError, ChecksumExpected, Verification, verify};
pub use fetch::{ConditionalHeaders, FetchConfig, FetchError, FetchOutcome, Fetcher};
pub use gc::{GcConfig, GcError, GcStats, run_gc};
pub use index::{
    DEFAULT_COMPACT_THRESHOLD, Index, IndexEntry, IndexError, IndexKey, OpenReport, Origin,
    OriginTier,
};
pub use journal::{Journal, JournalEntry, JournalError};
pub use lock::{
    CoordLockGuard, CoordLockMap, CoordVersionKey, FilesystemLock, LockError, lock_path,
};
pub use m2::{MirrorError, m2_path, materialize};
pub use recovery::{RecoveryError, RecoveryReport, is_recoverable, scan_and_recover};
pub use source::{CacheSource, RoasteryOutcome, RoasteryOutcomeObserver};
