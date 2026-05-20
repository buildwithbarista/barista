// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cache garbage collection.
//!
//! GC triggers when the on-disk cache fills past a configured high
//! watermark (default 95% of the configured cap). It evicts entries
//! oldest-atime-first, skips any with hardlinks (those are
//! referenced by `~/.m2/repository` or another consumer), and stops
//! when the cache drops below the low watermark (default 80%).
//!
//! The atime consulted here is the barista-managed `atime_unix`
//! field on `IndexEntry`, not the filesystem atime — `noatime` /
//! `relatime` mounts make the OS atime unreliable.
//!
//! ## Hardlink awareness
//!
//! On Unix, an entry whose CAS blob has `nlink >= 2` is treated as
//! non-evictable when `keep_hardlinked` is on (the default). A
//! second link typically means the `~/.m2/repository` mirror is
//! still pointing at the blob; evicting it would leave a dangling
//! reference on the user's local Maven layout. The mirror itself
//! is owned by the layout writer; GC only consults the link count.
//!
//! Non-Unix platforms skip the `nlink` check for now (v0.1 is
//! Unix-only for hardlink semantics).

use std::path::PathBuf;

use crate::cas::Cas;
use crate::index::Index;

/// Tunables for a GC pass.
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// Hard cap on cache size, in bytes. Default: 50 GiB.
    pub max_size_bytes: u64,
    /// High watermark — GC starts when usage crosses this fraction
    /// of `max_size_bytes`. Default: `0.95`.
    pub high_watermark: f64,
    /// Low watermark — GC stops when usage drops below this fraction
    /// of `max_size_bytes`. Default: `0.80`.
    pub low_watermark: f64,
    /// When true, entries whose CAS blob has more than one hardlink
    /// are treated as non-evictable. Default: `true`.
    pub keep_hardlinked: bool,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            max_size_bytes: 50 * 1024 * 1024 * 1024,
            high_watermark: 0.95,
            low_watermark: 0.80,
            keep_hardlinked: true,
        }
    }
}

/// Outcome of a GC pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcStats {
    /// Entries examined (including those skipped or already missing).
    pub considered: u64,
    /// Entries actually evicted (CAS blob + index entry removed).
    pub evicted: u64,
    /// Entries left alone because they had hardlinks.
    pub skipped_hardlinked: u64,
    /// Bytes freed from the cache (per the index's recorded sizes).
    pub bytes_freed: u64,
    /// `true` iff the pass either started below the high watermark
    /// or successfully drove usage below the low watermark.
    pub reached_target: bool,
}

/// Errors surfaced from a GC pass.
#[derive(Debug, thiserror::Error)]
pub enum GcError {
    /// Filesystem error attributable to a specific path.
    #[error("I/O error at {path:?}: {source}")]
    Io {
        /// Path that produced the error.
        path: PathBuf,
        /// Originating `std::io::Error`.
        source: std::io::Error,
    },
    /// Wrapped index-layer failure.
    #[error("index error: {0}")]
    Index(#[from] crate::index::IndexError),
}

/// Run a single GC pass against `cas` + `index`. Returns stats.
///
/// The pass is a no-op (and reports `reached_target = true`) when
/// the current cache size — summed from `IndexEntry.size_bytes` —
/// is already below the high watermark. Otherwise entries are
/// considered in ascending `atime_unix` order, skipping hardlinked
/// blobs when `config.keep_hardlinked` is set, until either the
/// low watermark is reached or the candidate list is exhausted.
pub fn run_gc(cas: &Cas, index: &Index, config: &GcConfig) -> Result<GcStats, GcError> {
    let max_bytes = config.max_size_bytes;
    let high_bytes = (max_bytes as f64 * config.high_watermark) as u64;
    let low_bytes = (max_bytes as f64 * config.low_watermark) as u64;

    let entries = index.entries();
    let mut total: u64 = entries.iter().map(|(_, e)| e.size_bytes).sum();
    let mut stats = GcStats::default();

    if total < high_bytes {
        stats.reached_target = true;
        return Ok(stats);
    }

    // Oldest atime first; ties are broken by the index's natural
    // BTreeMap key order, which `entries()` already preserves.
    let mut sorted = entries;
    sorted.sort_by_key(|(_, e)| e.atime_unix);

    for (key, entry) in sorted {
        stats.considered += 1;
        if total < low_bytes {
            stats.reached_target = true;
            break;
        }

        let cas_path = cas.path_for(&entry.hash);

        if config.keep_hardlinked {
            match std::fs::metadata(&cas_path) {
                Ok(_md) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        if _md.nlink() >= 2 {
                            stats.skipped_hardlinked += 1;
                            continue;
                        }
                    }
                    // Non-unix: fall through and evict.
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Orphaned index entry — blob already gone. Drop
                    // the index row and shrink our running total
                    // (the row was contributing to it, even though
                    // the disk space was already reclaimed out of
                    // band). Don't count it as evicted bytes.
                    index.remove(&key)?;
                    total = total.saturating_sub(entry.size_bytes);
                    continue;
                }
                Err(e) => {
                    return Err(GcError::Io {
                        path: cas_path,
                        source: e,
                    });
                }
            }
        }

        // Evict: drop CAS blob then drop index entry. Order matters
        // for crash safety — a missing blob with a present index
        // row is the recoverable failure mode (next GC pass cleans
        // the orphan); the reverse leaves a live blob nobody knows
        // about.
        match std::fs::remove_file(&cas_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Blob already gone; still remove the index row.
            }
            Err(e) => {
                return Err(GcError::Io {
                    path: cas_path,
                    source: e,
                });
            }
        }
        index.remove(&key)?;
        stats.evicted += 1;
        stats.bytes_freed += entry.size_bytes;
        total = total.saturating_sub(entry.size_bytes);
    }

    if total < low_bytes {
        stats.reached_target = true;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::ContentHash;
    use crate::index::{IndexEntry, IndexKey, Origin};
    use barista_coords::Coords;
    use tempfile::tempdir;

    /// Build a cache fixture: a fresh `Cas` and a fresh `Index`,
    /// both rooted under a single tempdir.
    struct Fixture {
        _dir: tempfile::TempDir,
        cas: Cas,
        index: Index,
    }

    fn fixture() -> Fixture {
        let dir = tempdir().unwrap();
        let cas = Cas::open(dir.path().join("cas")).unwrap();
        let index = Index::open(dir.path()).unwrap();
        Fixture {
            _dir: dir,
            cas,
            index,
        }
    }

    fn key(artifact: &str) -> IndexKey {
        IndexKey::new(
            Coords::new("org.example", artifact).unwrap(),
            "1.0.0",
            "jar",
            None,
        )
    }

    /// Write `bytes` to the CAS, record a sized index entry with the
    /// given atime, and return the (key, hash, size) triple.
    fn seed(
        fx: &Fixture,
        artifact: &str,
        bytes: &[u8],
        atime_unix: u64,
        size_bytes: u64,
    ) -> (IndexKey, ContentHash) {
        let (hash, _path) = fx.cas.put(bytes).unwrap();
        let k = key(artifact);
        let entry = IndexEntry {
            hash,
            size_bytes,
            sha1_hex: None,
            origin: Origin {
                repository_url: "https://repo.example/maven2".to_string(),
                etag: None,
                last_modified: None,
                upstream_last_updated: None,
                tier: Default::default(),
            },
            atime_unix,
            created_unix: atime_unix,
        };
        fx.index.put(k.clone(), entry).unwrap();
        (k, hash)
    }

    fn small_cap(cap: u64) -> GcConfig {
        GcConfig {
            max_size_bytes: cap,
            high_watermark: 0.95,
            low_watermark: 0.80,
            keep_hardlinked: true,
        }
    }

    #[test]
    fn empty_cache_is_a_noop() {
        let fx = fixture();
        let stats = run_gc(&fx.cas, &fx.index, &small_cap(1_000)).unwrap();
        assert!(stats.reached_target);
        assert_eq!(stats.considered, 0);
        assert_eq!(stats.evicted, 0);
        assert_eq!(stats.bytes_freed, 0);
        assert_eq!(stats.skipped_hardlinked, 0);
    }

    #[test]
    fn below_high_watermark_no_eviction() {
        let fx = fixture();
        // 100 bytes recorded against a 1000-byte cap → 10% full,
        // far below the 95% trigger.
        seed(&fx, "a", b"hello-a", 100, 100);
        let stats = run_gc(&fx.cas, &fx.index, &small_cap(1_000)).unwrap();
        assert!(stats.reached_target);
        assert_eq!(stats.evicted, 0);
        assert_eq!(stats.considered, 0);
        assert_eq!(fx.index.len(), 1);
    }

    #[test]
    fn evicts_oldest_first_above_high_watermark() {
        let fx = fixture();
        // Cap 1000, high=950, low=800. Total 1000 → over high.
        // Oldest (atime=100) should be the eviction target.
        let (k_old, _) = seed(&fx, "old", b"old-bytes", 100, 500);
        let (k_new, _) = seed(&fx, "new", b"new-bytes", 9_000, 500);

        let stats = run_gc(&fx.cas, &fx.index, &small_cap(1_000)).unwrap();
        assert!(stats.reached_target);
        assert_eq!(stats.evicted, 1);
        assert_eq!(stats.bytes_freed, 500);
        assert!(fx.index.get(&k_old).is_none());
        assert!(fx.index.get(&k_new).is_some());
    }

    #[test]
    fn stops_at_low_watermark() {
        let fx = fixture();
        // Cap 1000 → high=950, low=800. Five entries, 200 bytes each,
        // total=1000. We must drive total *strictly below* 800, so
        // two evictions get us to 600 and the loop terminates.
        // (After one eviction total=800; `800 < 800` is false, so
        // we keep going.)
        for (i, atime) in [100u64, 200, 300, 400, 500].iter().enumerate() {
            seed(
                &fx,
                &format!("a{i}"),
                format!("p{i}").as_bytes(),
                *atime,
                200,
            );
        }
        let stats = run_gc(&fx.cas, &fx.index, &small_cap(1_000)).unwrap();
        assert!(stats.reached_target);
        assert_eq!(stats.evicted, 2);
        assert_eq!(stats.bytes_freed, 400);
        assert_eq!(fx.index.len(), 3);
    }

    #[test]
    fn hardlinked_entries_are_skipped() {
        let fx = fixture();
        let (k_pinned, hash_pinned) = seed(&fx, "pinned", b"pinned-bytes", 50, 600);
        let (k_loose, _) = seed(&fx, "loose", b"loose-bytes", 100, 400);

        // Simulate the ~/.m2 mirror by hardlinking the pinned blob.
        let link_target = fx._dir.path().join("m2-mirror-link");
        std::fs::hard_link(fx.cas.path_for(&hash_pinned), &link_target).unwrap();

        let stats = run_gc(&fx.cas, &fx.index, &small_cap(1_000)).unwrap();
        // Loose got evicted; pinned survived even though it's older.
        assert_eq!(stats.evicted, 1);
        assert_eq!(stats.skipped_hardlinked, 1);
        assert!(fx.index.get(&k_pinned).is_some());
        assert!(fx.index.get(&k_loose).is_none());
    }

    #[test]
    fn hardlinked_entries_evict_when_keep_hardlinked_is_off() {
        let fx = fixture();
        let (k_pinned, hash_pinned) = seed(&fx, "pinned", b"pinned-bytes", 50, 600);
        let _ = seed(&fx, "other", b"other-bytes", 9_000, 400);

        let link_target = fx._dir.path().join("m2-link");
        std::fs::hard_link(fx.cas.path_for(&hash_pinned), &link_target).unwrap();

        let cfg = GcConfig {
            keep_hardlinked: false,
            ..small_cap(1_000)
        };
        let stats = run_gc(&fx.cas, &fx.index, &cfg).unwrap();
        assert_eq!(stats.skipped_hardlinked, 0);
        assert!(stats.evicted >= 1);
        // The originally hardlinked index row is gone even though
        // `link_target` still points at the (now removed) blob.
        assert!(fx.index.get(&k_pinned).is_none());
    }

    #[test]
    fn eviction_removes_both_cas_blob_and_index_entry() {
        let fx = fixture();
        let (k, hash) = seed(&fx, "victim", b"victim-bytes", 100, 600);
        let _ = seed(&fx, "survivor", b"survivor-bytes", 9_000, 400);

        let blob_path = fx.cas.path_for(&hash);
        assert!(blob_path.exists());

        run_gc(&fx.cas, &fx.index, &small_cap(1_000)).unwrap();

        assert!(!blob_path.exists(), "CAS blob must be removed");
        assert!(fx.index.get(&k).is_none(), "index row must be removed");
    }

    #[test]
    fn handles_orphaned_index_entry_when_blob_is_missing() {
        let fx = fixture();
        let (k_orphan, hash_orphan) = seed(&fx, "orphan", b"orphan-bytes", 50, 600);
        let (k_keep, _) = seed(&fx, "keep", b"keep-bytes", 9_000, 400);

        // Out-of-band corruption: blob vanished but index still
        // remembers it.
        std::fs::remove_file(fx.cas.path_for(&hash_orphan)).unwrap();

        let stats = run_gc(&fx.cas, &fx.index, &small_cap(1_000)).unwrap();
        // Orphan removal doesn't count as "evicted bytes" — the
        // disk space was already reclaimed out-of-band — but the
        // index row should be gone.
        assert!(fx.index.get(&k_orphan).is_none());
        assert!(fx.index.get(&k_keep).is_some());
        assert!(stats.considered >= 1);
    }

    #[test]
    fn atime_ordering_drives_eviction_order() {
        let fx = fixture();
        // Three entries; all the same size. Eviction order should
        // strictly follow atime ascending.
        let (k_a, _) = seed(&fx, "a", b"aaaa", 100, 400);
        let (k_b, _) = seed(&fx, "b", b"bbbb", 200, 400);
        let (k_c, _) = seed(&fx, "c", b"cccc", 300, 400);

        // Cap 1000 → 1200 used → must drop below low=800. Two
        // evictions get us to 400. After one we're at 800 which
        // is not strictly below low_bytes (800 < 800 is false), so
        // we evict a second.
        let stats = run_gc(&fx.cas, &fx.index, &small_cap(1_000)).unwrap();
        assert!(stats.reached_target);
        assert_eq!(stats.evicted, 2);
        // The two oldest go first; the newest (c) survives.
        assert!(fx.index.get(&k_a).is_none());
        assert!(fx.index.get(&k_b).is_none());
        assert!(fx.index.get(&k_c).is_some());
    }

    #[test]
    fn gc_stats_report_all_counters_correctly() {
        let fx = fixture();
        // Mix: one pinned (hardlinked, old) + two evictable.
        let (_k_pin, hash_pin) = seed(&fx, "pin", b"pin-bytes", 10, 400);
        let (_k_old, _) = seed(&fx, "old", b"old-bytes", 50, 400);
        let (_k_mid, _) = seed(&fx, "mid", b"mid-bytes", 100, 400);

        let link = fx._dir.path().join("pin-link");
        std::fs::hard_link(fx.cas.path_for(&hash_pin), &link).unwrap();

        // Cap 1000 → 1200 in use. Pin is skipped (oldest but
        // hardlinked); old gets evicted (now 800 used — still not
        // strictly below low=800); mid gets evicted (now 400).
        let stats = run_gc(&fx.cas, &fx.index, &small_cap(1_000)).unwrap();
        assert!(stats.reached_target);
        assert_eq!(stats.skipped_hardlinked, 1);
        assert_eq!(stats.evicted, 2);
        assert_eq!(stats.bytes_freed, 800);
        assert!(stats.considered >= 3);
    }
}
