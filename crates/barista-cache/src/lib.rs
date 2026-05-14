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
pub mod recovery;
pub mod source;

pub use cas::{Cas, CasError, ContentHash};
pub use checksum::{Algorithm, ChecksumError, ChecksumExpected, Verification, verify};
pub use fetch::{ConditionalHeaders, FetchConfig, FetchError, FetchOutcome, Fetcher};
pub use gc::{GcConfig, GcError, GcStats, run_gc};
pub use index::{
    DEFAULT_COMPACT_THRESHOLD, Index, IndexEntry, IndexError, IndexKey, OpenReport, Origin,
};
pub use journal::{Journal, JournalEntry, JournalError};
pub use lock::{CoordLockGuard, CoordLockMap, CoordVersionKey, FilesystemLock, LockError, lock_path};
pub use recovery::{RecoveryError, RecoveryReport, is_recoverable, scan_and_recover};
pub use source::CacheSource;
