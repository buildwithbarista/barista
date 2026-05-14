//! Cache crash recovery.
//!
//! On startup, the Index loads its snapshot + replays the journal
//! tail. If the journal is truncated mid-record (e.g. a SIGKILL
//! between write and fsync) or a record's CRC32 doesn't match,
//! we have two options:
//!
//! 1. **Accept-partial:** trust the entries that loaded cleanly,
//!    discard the trailing bad records, and continue. Risk: a
//!    recent successful put may have been lost.
//!
//! 2. **Rebuild-from-CAS:** scan the on-disk CAS for blobs that
//!    aren't reflected in the index, and... they can't actually
//!    be re-indexed because the CAS doesn't know Maven coords.
//!    But we can at least flag orphan blobs for GC.
//!
//! Strategy: combine the two. On truncation/checksum failure:
//!   a. Recover what we can from the journal (partial is OK).
//!   b. Scan the CAS and emit a report of orphan blobs (blobs
//!      not referenced by any IndexEntry).
//!   c. Surface a recovery report to the caller; the cache
//!      operator decides whether to delete orphans or leave them.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::cas::{Cas, ContentHash};
use crate::index::Index;
use crate::journal::JournalError;

/// Summary of what `scan_and_recover` did and found.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    /// True if the journal had to be truncated to recover.
    pub journal_truncated: bool,
    /// Offset at which the journal was truncated, if applicable.
    pub journal_truncated_at: Option<u64>,
    /// Blobs in CAS not referenced by any IndexEntry. The cache
    /// operator can choose to keep these (in case index was wrong)
    /// or evict them (to free space).
    pub orphan_blobs: Vec<(ContentHash, PathBuf, u64)>,
    /// Total bytes of orphan blobs.
    pub orphan_bytes: u64,
    /// Number of index entries whose CAS blob is missing on disk.
    /// These are removed from the index during recovery.
    pub dangling_entries: u64,
}

/// Errors produced by the recovery layer.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// Wrapped CAS-level failure encountered while walking blobs.
    #[error("CAS error: {0}")]
    Cas(#[from] crate::cas::CasError),
    /// Wrapped index-level failure encountered while pruning.
    #[error("index error: {0}")]
    Index(#[from] crate::index::IndexError),
}

/// Scan the CAS + index and produce a recovery report. Removes
/// dangling index entries (whose CAS blob is gone). Identifies
/// orphan blobs (in CAS but not in index). Caller decides what to
/// do with orphans.
pub fn scan_and_recover(cas: &Cas, index: &Index) -> Result<RecoveryReport, RecoveryError> {
    let mut report = RecoveryReport::default();

    // 1. Walk index entries; verify each entry's CAS blob exists.
    let entries = index.entries();
    let mut indexed_hashes: HashSet<ContentHash> = HashSet::new();
    for (key, entry) in entries {
        if cas.contains(&entry.hash) {
            indexed_hashes.insert(entry.hash);
        } else {
            // Dangling index entry — CAS blob is gone.
            let _ = index.remove(&key);
            report.dangling_entries += 1;
        }
    }

    // 2. Walk CAS; identify orphans.
    for result in cas.entries() {
        let (hash, path) = result?;
        if !indexed_hashes.contains(&hash) {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            report.orphan_bytes += size;
            report.orphan_blobs.push((hash, path, size));
        }
    }

    Ok(report)
}

/// Helper: inspect a [`JournalError`] and decide whether it's
/// recoverable (truncation / bad-tail-checksum) or fatal (bad
/// magic / unsupported version / lower-level I/O).
pub fn is_recoverable(err: &JournalError) -> bool {
    matches!(
        err,
        JournalError::Truncated { .. } | JournalError::BadChecksum { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::Cas;
    use crate::index::{Index, IndexEntry, IndexKey, Origin};
    use crate::journal::HEADER_LEN;
    use barista_coords::Coords;
    use std::fs::OpenOptions;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn key(artifact: &str, version: &str) -> IndexKey {
        IndexKey::new(
            Coords::new("org.example", artifact).unwrap(),
            version,
            "jar",
            None,
        )
    }

    fn entry_for(hash: ContentHash) -> IndexEntry {
        IndexEntry {
            hash,
            size_bytes: 0,
            sha1_hex: None,
            origin: Origin {
                repository_url: "https://repo.example/maven2".to_string(),
                etag: None,
                last_modified: None,
                upstream_last_updated: None,
            },
            atime_unix: 1_700_000_000,
            created_unix: 1_700_000_000,
        }
    }

    /// Tiny helper: 32-byte hash from a repeated byte.
    fn fake_hash(b: u8) -> ContentHash {
        ContentHash::from_bytes([b; 32])
    }

    #[test]
    fn empty_cas_and_index_yields_empty_report() {
        let dir = tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let idx = Index::open(dir.path()).unwrap();
        let report = scan_and_recover(&cas, &idx).unwrap();
        assert!(!report.journal_truncated);
        assert_eq!(report.journal_truncated_at, None);
        assert!(report.orphan_blobs.is_empty());
        assert_eq!(report.orphan_bytes, 0);
        assert_eq!(report.dangling_entries, 0);
    }

    #[test]
    fn matched_cas_and_index_has_no_orphans_or_dangling() {
        let dir = tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let idx = Index::open(dir.path()).unwrap();

        let payload = b"hello world";
        let (hash, _) = cas.put(payload).unwrap();
        idx.put(key("a", "1.0.0"), entry_for(hash)).unwrap();

        let report = scan_and_recover(&cas, &idx).unwrap();
        assert!(report.orphan_blobs.is_empty());
        assert_eq!(report.orphan_bytes, 0);
        assert_eq!(report.dangling_entries, 0);
    }

    #[test]
    fn orphan_blob_is_reported() {
        let dir = tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let idx = Index::open(dir.path()).unwrap();

        // Write to CAS but never index.
        let (hash, _) = cas.put(b"orphan-payload").unwrap();

        let report = scan_and_recover(&cas, &idx).unwrap();
        assert_eq!(report.orphan_blobs.len(), 1);
        assert_eq!(report.orphan_blobs[0].0, hash);
        assert_eq!(report.dangling_entries, 0);
    }

    #[test]
    fn dangling_index_entry_is_pruned() {
        let dir = tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let idx = Index::open(dir.path()).unwrap();

        // Index claims a hash that isn't on disk.
        let phantom = fake_hash(0xCD);
        idx.put(key("ghost", "1.0.0"), entry_for(phantom)).unwrap();

        let report = scan_and_recover(&cas, &idx).unwrap();
        assert_eq!(report.dangling_entries, 1);
        assert_eq!(idx.len(), 0, "dangling entry should be pruned");
        assert!(report.orphan_blobs.is_empty());
    }

    #[test]
    fn orphan_bytes_total_matches_blob_size() {
        let dir = tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let idx = Index::open(dir.path()).unwrap();

        // Two orphan blobs of known sizes.
        let p1 = vec![0u8; 100];
        let p2 = vec![0u8; 250];
        cas.put(&p1).unwrap();
        cas.put(&p2).unwrap();

        // One indexed blob (not an orphan).
        let p3 = vec![1u8; 500];
        let (h3, _) = cas.put(&p3).unwrap();
        idx.put(key("kept", "1.0.0"), entry_for(h3)).unwrap();

        let report = scan_and_recover(&cas, &idx).unwrap();
        assert_eq!(report.orphan_blobs.len(), 2);
        assert_eq!(report.orphan_bytes, 100 + 250);
    }

    #[test]
    fn is_recoverable_accepts_truncated() {
        let err = JournalError::Truncated {
            path: PathBuf::from("/tmp/x"),
        };
        assert!(is_recoverable(&err));
    }

    #[test]
    fn is_recoverable_accepts_bad_checksum() {
        let err = JournalError::BadChecksum { offset: 42 };
        assert!(is_recoverable(&err));
    }

    #[test]
    fn is_recoverable_rejects_bad_magic() {
        let err = JournalError::BadMagic {
            path: PathBuf::from("/tmp/x"),
            expected: *b"BCAS",
            got: *b"XXXX",
        };
        assert!(!is_recoverable(&err));
    }

    #[test]
    fn is_recoverable_rejects_unsupported_version() {
        let err = JournalError::UnsupportedVersion {
            path: PathBuf::from("/tmp/x"),
            version: 999,
            expected: 1,
        };
        assert!(!is_recoverable(&err));
    }

    #[test]
    fn open_with_recovery_handles_truncated_tail() {
        let dir = tempdir().unwrap();
        let cache_root = dir.path().to_path_buf();

        // Step 1: populate the index with three entries.
        {
            let idx = Index::open(&cache_root).unwrap();
            idx.set_compact_threshold(u64::MAX);
            idx.put(key("a", "1.0.0"), entry_for(fake_hash(1))).unwrap();
            idx.put(key("b", "1.0.0"), entry_for(fake_hash(2))).unwrap();
            idx.put(key("c", "1.0.0"), entry_for(fake_hash(3))).unwrap();
        }

        // Step 2: simulate a SIGKILL mid-record by chopping the
        // last 5 bytes off the journal (lands in the tail CRC of
        // the final record).
        let journal_path = cache_root.join("index").join("journal.log");
        let full_len = std::fs::metadata(&journal_path).unwrap().len();
        assert!(full_len > HEADER_LEN + 5);
        let truncated_to = full_len - 5;
        let file = OpenOptions::new().write(true).open(&journal_path).unwrap();
        file.set_len(truncated_to).unwrap();
        drop(file);

        // Step 3: plain open() must surface the recoverable error.
        let plain = Index::open(&cache_root);
        assert!(plain.is_err(), "expected truncation error on plain open");

        // Step 4: open_with_recovery succeeds, reports the
        // truncation, and exposes the partial state.
        let (idx, report) = Index::open_with_recovery(&cache_root).unwrap();
        assert!(report.journal_truncated);
        assert!(report.journal_truncated_at.is_some());
        // At least one entry survived; at most all three.
        let n = idx.len();
        assert!((1..=3).contains(&n), "expected partial entries, got {n}");

        // Step 5: the cache is usable post-recovery (new puts work).
        idx.put(key("post", "1.0.0"), entry_for(fake_hash(0xFF)))
            .unwrap();
        assert!(idx.get(&key("post", "1.0.0")).is_some());
    }
}
