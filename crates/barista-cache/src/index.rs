// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-memory cache index.
//!
//! Maps fully-qualified Maven coordinates → on-disk CAS blob. The
//! resolver and `barback` consult this to answer "do I already have
//! `g:a:v` cached, and what SHA-256 does it hash to?" without
//! walking the CAS filesystem.
//!
//! # Persistence model
//!
//! The index lives in two files next to the CAS root:
//!
//! - `<cache_root>/index/journal.log` — append-only log of every
//!   mutation, written via [`crate::journal::Journal`].
//! - `<cache_root>/index/snapshot.bin` — periodic compaction of the
//!   live index state.
//!
//! On startup the snapshot is loaded (if present), then the journal
//! tail is replayed on top. Mutating calls (`put`, `remove`,
//! `touch`) append to the journal first, then update the in-memory
//! map, then optionally trigger compaction if the
//! "entries-since-last-compact" counter has crossed
//! [`DEFAULT_COMPACT_THRESHOLD`].
//!
//! # Concurrency
//!
//! The struct is `Clone` and cheap to share: internally it holds an
//! `Arc<RwLock<…>>` for the in-memory state and an `Arc<Journal>`
//! whose own `Mutex` serializes appends. Readers (`get`, `entries`,
//! `len`) take a read lock; writers (`put`, `remove`, `touch`,
//! `compact`) take a write lock.
//!
//! # Performance note
//!
//! v0.1 uses plain `serde + bincode` for both the journal records and
//! the snapshot. rkyv zero-copy is a possible future optimization —
//! at expected cache sizes (low millions of
//! entries) the perf difference is invisible, and bincode keeps the
//! encoder simple. Swapping codecs is a localized change behind this
//! module's API.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use barista_coords::Coords;

use crate::cas::ContentHash;
use crate::journal::{
    FILE_MAGIC, HEADER_LEN, Journal, JournalEntry, JournalError, SNAPSHOT_VERSION, validate_header,
};

/// Default number of journal mutations after which the index will
/// auto-compact on the next write.
///
/// Picked so a fully warm cache (~tens of thousands of artifacts)
/// snapshots roughly once per "interesting" run, but a `cargo test`
/// loop that re-touches a handful of entries doesn't churn the
/// snapshot. Operators can tune this with
/// [`Index::set_compact_threshold`].
pub const DEFAULT_COMPACT_THRESHOLD: u64 = 10_000;

/// A fully-qualified artifact identity, suitable for use as an index key.
///
/// Mirrors [`barista_coords::GATCV`] but stores `version` as a plain
/// `String` so the cache doesn't have to honour the resolver's
/// stricter version-parse invariants — the cache records whatever
/// the fetcher pulled, even if some other layer would later reject
/// it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IndexKey {
    /// Maven `(group, artifact)`.
    pub coords: Coords,
    /// Version string (`1.2.3`, `1.0-SNAPSHOT`, …).
    pub version: String,
    /// Packaging / type (`jar`, `pom`, `war`, …). Named `type_` because
    /// `type` is a Rust keyword.
    pub type_: String,
    /// Optional Maven classifier (`sources`, `javadoc`, `tests`, …).
    pub classifier: Option<String>,
}

impl IndexKey {
    /// Convenience constructor accepting anything that converts into
    /// `String` for the version + type fields.
    pub fn new(
        coords: Coords,
        version: impl Into<String>,
        type_: impl Into<String>,
        classifier: Option<String>,
    ) -> Self {
        Self {
            coords,
            version: version.into(),
            type_: type_.into(),
            classifier,
        }
    }
}

/// The metadata the cache tracks for one artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// SHA-256 digest of the on-disk artifact bytes.
    pub hash: ContentHash,
    /// Size of the on-disk artifact bytes, in bytes.
    pub size_bytes: u64,
    /// Maven sidecar SHA-1 in lowercase hex, if the fetcher verified
    /// it. Informational — `get()` does not re-verify; GC + repair
    /// (T8) may.
    pub sha1_hex: Option<String>,
    /// Where the artifact came from.
    pub origin: Origin,
    /// Barista-managed access time (UNIX seconds). Updated on every
    /// cache hit; consulted by GC (T8). Filesystem atime is
    /// unreliable on `noatime` / `relatime` mounts so we track our
    /// own.
    pub atime_unix: u64,
    /// When this entry was first written (UNIX seconds).
    pub created_unix: u64,
}

/// Provenance for a cached artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// Repository base URL the artifact was fetched from. For
    /// [`OriginTier::Roastery`], this is the roastery's base URL.
    pub repository_url: String,
    /// Server `ETag` at fetch time, for conditional refetch.
    pub etag: Option<String>,
    /// Server `Last-Modified` at fetch time, for conditional refetch.
    pub last_modified: Option<String>,
    /// Maven's `<lastUpdated>` from `maven-metadata.xml`, if known.
    pub upstream_last_updated: Option<String>,
    /// Which fetch tier produced the bytes — direct from a Maven
    /// upstream, or via a remote roastery cache.
    ///
    /// Defaulted on deserialization to [`OriginTier::Upstream`] so
    /// index entries persisted before the roastery tier landed
    /// continue to load cleanly. This is the migration policy:
    /// no rewrite on load, the missing discriminator implicitly
    /// means "fetched directly from upstream" (the only path the
    /// older code took).
    #[serde(default)]
    pub tier: OriginTier,
}

/// Which fetch tier produced the bytes for a cached artifact.
///
/// Serde-compat: the variant payloads are unit because the
/// `repository_url` already lives in [`Origin`]. A future change
/// could split the URL by tier, but in v0.1 keeping the URL field
/// shared minimises the on-disk impact.
///
/// The default is [`Self::Upstream`] so a missing `tier` field on
/// an older index entry deserialises to the pre-roastery semantics.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum OriginTier {
    /// Fetched directly from a Maven repository (e.g. Maven Central,
    /// a corporate proxy, or an internal mirror). The default for
    /// backward-compat with pre-roastery index entries.
    #[default]
    Upstream,
    /// Fetched via a remote roastery cache. The roastery may itself
    /// have served from its local CAS or relayed from one of its
    /// configured upstreams; both surface here as a single tier
    /// because the cache crate can't (and shouldn't) tell the
    /// difference.
    Roastery,
}

/// Errors surfaced by the index layer.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// Wrapped journal-level failure.
    #[error("journal error: {0}")]
    Journal(#[from] JournalError),
    /// Filesystem error attributable to a specific path.
    #[error("I/O error at {path:?}: {source}")]
    Io {
        /// Path that produced the error.
        path: PathBuf,
        /// Originating `std::io::Error`.
        source: std::io::Error,
    },
    /// Snapshot bytes failed to decode.
    #[error("snapshot at {path:?} is malformed: {detail}")]
    SnapshotMalformed {
        /// Path of the affected snapshot.
        path: PathBuf,
        /// Decoder diagnostic.
        detail: String,
    },
}

/// Thread-safe cache index. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Index {
    inner: Arc<RwLock<IndexState>>,
    journal: Arc<Journal>,
    snapshot_path: PathBuf,
    compact_threshold: Arc<RwLock<u64>>,
}

#[derive(Debug, Default)]
struct IndexState {
    entries: BTreeMap<IndexKey, IndexEntry>,
    journal_entries_since_compact: u64,
}

/// Outcome of [`Index::open_with_recovery`].
#[derive(Debug, Default, Clone)]
pub struct OpenReport {
    /// True iff the journal had to be truncated to recover from a
    /// corrupted or partially-written tail record.
    pub journal_truncated: bool,
    /// Byte offset at which the journal was truncated, if applicable.
    pub journal_truncated_at: Option<u64>,
}

impl Index {
    /// Open (creating if needed) the index rooted at
    /// `<cache_root>/index/`. Loads the snapshot then replays the
    /// journal tail.
    ///
    /// Errors on any journal corruption. For production startup,
    /// prefer [`Self::open_with_recovery`], which silently truncates
    /// recoverable tail damage and reports it via [`OpenReport`].
    pub fn open(cache_root: &Path) -> Result<Self, IndexError> {
        let (index, _) = Self::open_inner(cache_root, false)?;
        Ok(index)
    }

    /// Open with recovery information. Returns the [`Index`] and an
    /// [`OpenReport`] describing any journal truncation that
    /// occurred.
    ///
    /// If the journal has a corrupted or partially-written tail
    /// record (e.g. a SIGKILL between write and fsync), this method
    /// truncates the journal at the last known-good record and
    /// continues with the partial state. Fatal errors (bad magic,
    /// unsupported version, lower-level I/O) still surface as
    /// [`IndexError`].
    ///
    /// Callers should typically follow up with
    /// [`crate::recovery::scan_and_recover`] to identify orphan CAS
    /// blobs and dangling index entries.
    pub fn open_with_recovery(cache_root: &Path) -> Result<(Self, OpenReport), IndexError> {
        Self::open_inner(cache_root, true)
    }

    fn open_inner(cache_root: &Path, recover: bool) -> Result<(Self, OpenReport), IndexError> {
        let index_dir = cache_root.join("index");
        std::fs::create_dir_all(&index_dir).map_err(|e| IndexError::Io {
            path: index_dir.clone(),
            source: e,
        })?;

        let snapshot_path = index_dir.join("snapshot.bin");
        let journal_path = index_dir.join("journal.log");

        let mut state = if snapshot_path.exists() {
            load_snapshot(&snapshot_path)?
        } else {
            IndexState::default()
        };

        let journal = Journal::open(&journal_path)?;
        let mut report = OpenReport::default();

        let mut iter = journal.iter_entries_with_positions()?;
        let mut bad_tail: Option<JournalError> = None;
        for record in iter.by_ref() {
            match record {
                Ok(JournalEntry::Put { key, entry }) => {
                    state.entries.insert(key, entry);
                    state.journal_entries_since_compact += 1;
                }
                Ok(JournalEntry::Remove { key }) => {
                    state.entries.remove(&key);
                    state.journal_entries_since_compact += 1;
                }
                Ok(JournalEntry::Touch { key, atime_unix }) => {
                    if let Some(e) = state.entries.get_mut(&key) {
                        e.atime_unix = atime_unix;
                    }
                    state.journal_entries_since_compact += 1;
                }
                Err(e) => {
                    bad_tail = Some(e);
                    break;
                }
            }
        }

        if let Some(err) = bad_tail {
            if recover && crate::recovery::is_recoverable(&err) {
                let cut_at = iter.last_good_offset();
                journal.truncate_to(cut_at)?;
                report.journal_truncated = true;
                report.journal_truncated_at = Some(cut_at);
            } else {
                return Err(err.into());
            }
        }

        Ok((
            Self {
                inner: Arc::new(RwLock::new(state)),
                journal: Arc::new(journal),
                snapshot_path,
                compact_threshold: Arc::new(RwLock::new(DEFAULT_COMPACT_THRESHOLD)),
            },
            report,
        ))
    }

    /// Insert or replace an entry. The mutation is journaled before
    /// the in-memory map is updated, so a crash between the two leaves
    /// a recoverable on-disk state.
    pub fn put(&self, key: IndexKey, entry: IndexEntry) -> Result<(), IndexError> {
        self.journal.append(&JournalEntry::Put {
            key: key.clone(),
            entry: entry.clone(),
        })?;
        {
            let mut guard = self.inner.write().expect("index lock poisoned");
            guard.entries.insert(key, entry);
            guard.journal_entries_since_compact += 1;
        }
        self.maybe_compact()?;
        Ok(())
    }

    /// Remove an entry. Returns `true` if the key was present.
    /// The journal record is appended unconditionally so replay sees
    /// the intent even if the in-memory map disagrees.
    pub fn remove(&self, key: &IndexKey) -> Result<bool, IndexError> {
        self.journal
            .append(&JournalEntry::Remove { key: key.clone() })?;
        let removed = {
            let mut guard = self.inner.write().expect("index lock poisoned");
            guard.journal_entries_since_compact += 1;
            guard.entries.remove(key).is_some()
        };
        self.maybe_compact()?;
        Ok(removed)
    }

    /// Bump the access time of an existing entry. Returns `true` if
    /// the entry existed. No-op (and still appends a journal record)
    /// if absent — replay tolerates a `Touch` against a missing key.
    pub fn touch(&self, key: &IndexKey, atime_unix: u64) -> Result<bool, IndexError> {
        self.journal.append(&JournalEntry::Touch {
            key: key.clone(),
            atime_unix,
        })?;
        let touched = {
            let mut guard = self.inner.write().expect("index lock poisoned");
            guard.journal_entries_since_compact += 1;
            if let Some(e) = guard.entries.get_mut(key) {
                e.atime_unix = atime_unix;
                true
            } else {
                false
            }
        };
        self.maybe_compact()?;
        Ok(touched)
    }

    /// Look up an entry. Returns `None` if not present.
    pub fn get(&self, key: &IndexKey) -> Option<IndexEntry> {
        let guard = self.inner.read().expect("index lock poisoned");
        guard.entries.get(key).cloned()
    }

    /// Current entry count.
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .expect("index lock poisoned")
            .entries
            .len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot all entries into a `Vec` sorted by key — handy for
    /// GC scans and tests.
    pub fn entries(&self) -> Vec<(IndexKey, IndexEntry)> {
        let guard = self.inner.read().expect("index lock poisoned");
        guard
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Force a snapshot rewrite + journal truncation.
    pub fn compact(&self) -> Result<(), IndexError> {
        let mut guard = self.inner.write().expect("index lock poisoned");
        write_snapshot(&guard, &self.snapshot_path)?;
        self.journal.truncate()?;
        guard.journal_entries_since_compact = 0;
        Ok(())
    }

    /// Set the journal-entries-since-compact threshold that triggers
    /// automatic compaction. The default is
    /// [`DEFAULT_COMPACT_THRESHOLD`].
    pub fn set_compact_threshold(&self, n: u64) {
        *self
            .compact_threshold
            .write()
            .expect("threshold lock poisoned") = n;
    }

    /// Current compaction threshold.
    pub fn compact_threshold(&self) -> u64 {
        *self
            .compact_threshold
            .read()
            .expect("threshold lock poisoned")
    }

    fn maybe_compact(&self) -> Result<(), IndexError> {
        let threshold = self.compact_threshold();
        let should = {
            let guard = self.inner.read().expect("index lock poisoned");
            guard.journal_entries_since_compact >= threshold
        };
        if should {
            self.compact()?;
        }
        Ok(())
    }
}

/// Decode a snapshot file into an `IndexState`. The snapshot is a
/// 10-byte header (magic + version + reserved) followed by a single
/// bincode-encoded `Vec<(IndexKey, IndexEntry)>`.
fn load_snapshot(path: &Path) -> Result<IndexState, IndexError> {
    let mut file = File::open(path).map_err(|e| IndexError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    validate_header(&mut file, path, SNAPSHOT_VERSION)?;
    file.seek(SeekFrom::Start(HEADER_LEN))
        .map_err(|e| IndexError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    let mut buf = Vec::new();
    BufReader::new(file)
        .read_to_end(&mut buf)
        .map_err(|e| IndexError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    let cfg = bincode::config::standard();
    let (entries, _): (Vec<(IndexKey, IndexEntry)>, _) =
        bincode::serde::decode_from_slice(&buf, cfg).map_err(|e| {
            IndexError::SnapshotMalformed {
                path: path.to_path_buf(),
                detail: e.to_string(),
            }
        })?;
    let mut map = BTreeMap::new();
    for (k, v) in entries {
        map.insert(k, v);
    }
    Ok(IndexState {
        entries: map,
        journal_entries_since_compact: 0,
    })
}

/// Atomically rewrite the snapshot via a sibling tmp file + rename.
fn write_snapshot(state: &IndexState, path: &Path) -> Result<(), IndexError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| IndexError::Io {
        path: parent.to_path_buf(),
        source: e,
    })?;
    let tmp_path = path.with_extension("bin.tmp");

    let snapshot: Vec<(IndexKey, IndexEntry)> = state
        .entries
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let cfg = bincode::config::standard();
    let body = bincode::serde::encode_to_vec(&snapshot, cfg).map_err(|e| {
        IndexError::SnapshotMalformed {
            path: path.to_path_buf(),
            detail: e.to_string(),
        }
    })?;

    {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|e| IndexError::Io {
                path: tmp_path.clone(),
                source: e,
            })?;
        let mut writer = BufWriter::new(file);
        writer.write_all(FILE_MAGIC).map_err(|e| IndexError::Io {
            path: tmp_path.clone(),
            source: e,
        })?;
        writer
            .write_all(&SNAPSHOT_VERSION.to_le_bytes())
            .map_err(|e| IndexError::Io {
                path: tmp_path.clone(),
                source: e,
            })?;
        writer.write_all(&[0u8, 0u8]).map_err(|e| IndexError::Io {
            path: tmp_path.clone(),
            source: e,
        })?;
        writer.write_all(&body).map_err(|e| IndexError::Io {
            path: tmp_path.clone(),
            source: e,
        })?;
        writer.flush().map_err(|e| IndexError::Io {
            path: tmp_path.clone(),
            source: e,
        })?;
        writer.get_ref().sync_all().map_err(|e| IndexError::Io {
            path: tmp_path.clone(),
            source: e,
        })?;
    }
    std::fs::rename(&tmp_path, path).map_err(|e| IndexError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use barista_coords::Coords;
    use tempfile::tempdir;

    fn hex_repeat(b: u8) -> String {
        let mut s = String::with_capacity(64);
        for _ in 0..32 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    fn key(artifact: &str, version: &str) -> IndexKey {
        IndexKey::new(
            Coords::new("org.example", artifact).unwrap(),
            version,
            "jar",
            None,
        )
    }

    fn entry(byte: u8) -> IndexEntry {
        IndexEntry {
            hash: ContentHash::from_hex(&hex_repeat(byte)).unwrap(),
            size_bytes: 4096,
            sha1_hex: Some("0".repeat(40)),
            origin: Origin {
                repository_url: "https://repo.example/maven2".to_string(),
                etag: Some("\"abc\"".to_string()),
                last_modified: None,
                upstream_last_updated: None,
                tier: Default::default(),
            },
            atime_unix: 1_700_000_000,
            created_unix: 1_700_000_000,
        }
    }

    #[test]
    fn open_on_fresh_dir_creates_index_subdir_and_is_empty() {
        let dir = tempdir().unwrap();
        let idx = Index::open(dir.path()).unwrap();
        assert!(dir.path().join("index").is_dir());
        assert!(dir.path().join("index").join("journal.log").exists());
        assert!(!dir.path().join("index").join("snapshot.bin").exists());
        assert_eq!(idx.len(), 0);
        assert!(idx.is_empty());
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = tempdir().unwrap();
        let idx = Index::open(dir.path()).unwrap();
        let k = key("a", "1.0.0");
        let e = entry(0x11);
        idx.put(k.clone(), e.clone()).unwrap();
        assert_eq!(idx.get(&k), Some(e));
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn put_then_remove_empties_the_entry() {
        let dir = tempdir().unwrap();
        let idx = Index::open(dir.path()).unwrap();
        let k = key("a", "1.0.0");
        idx.put(k.clone(), entry(1)).unwrap();
        let removed = idx.remove(&k).unwrap();
        assert!(removed);
        assert_eq!(idx.get(&k), None);
        assert_eq!(idx.len(), 0);

        // Removing the same key again returns false.
        let removed = idx.remove(&k).unwrap();
        assert!(!removed);
    }

    #[test]
    fn touch_updates_atime_without_affecting_other_fields() {
        let dir = tempdir().unwrap();
        let idx = Index::open(dir.path()).unwrap();
        let k = key("a", "1.0.0");
        let e = entry(0x42);
        idx.put(k.clone(), e.clone()).unwrap();
        let new_atime = 2_000_000_000u64;
        let touched = idx.touch(&k, new_atime).unwrap();
        assert!(touched);
        let got = idx.get(&k).unwrap();
        assert_eq!(got.atime_unix, new_atime);
        assert_eq!(got.hash, e.hash);
        assert_eq!(got.size_bytes, e.size_bytes);
        assert_eq!(got.created_unix, e.created_unix);
        assert_eq!(got.origin, e.origin);

        // Touching a missing key is a no-op (and returns false).
        let touched = idx.touch(&key("missing", "1.0.0"), 999).unwrap();
        assert!(!touched);
    }

    #[test]
    fn entries_returns_all_entries_sorted_by_key() {
        let dir = tempdir().unwrap();
        let idx = Index::open(dir.path()).unwrap();
        idx.put(key("b", "1.0.0"), entry(2)).unwrap();
        idx.put(key("a", "2.0.0"), entry(3)).unwrap();
        idx.put(key("a", "1.0.0"), entry(1)).unwrap();
        let entries = idx.entries();
        let names: Vec<_> = entries
            .iter()
            .map(|(k, _)| (k.coords.artifact.clone(), k.version.clone()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("a".to_string(), "1.0.0".to_string()),
                ("a".to_string(), "2.0.0".to_string()),
                ("b".to_string(), "1.0.0".to_string()),
            ]
        );
    }

    #[test]
    fn index_persists_across_reopen() {
        let dir = tempdir().unwrap();
        let k = key("a", "1.0.0");
        let e = entry(0xAB);
        {
            let idx = Index::open(dir.path()).unwrap();
            idx.put(k.clone(), e.clone()).unwrap();
        }
        let idx = Index::open(dir.path()).unwrap();
        assert_eq!(idx.get(&k), Some(e));
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn journal_only_state_replays_correctly_on_reopen() {
        let dir = tempdir().unwrap();
        {
            let idx = Index::open(dir.path()).unwrap();
            // Make sure auto-compaction can't fire mid-test.
            idx.set_compact_threshold(u64::MAX);
            idx.put(key("a", "1.0.0"), entry(1)).unwrap();
            idx.put(key("b", "1.0.0"), entry(2)).unwrap();
            idx.touch(&key("a", "1.0.0"), 5_000).unwrap();
            idx.remove(&key("b", "1.0.0")).unwrap();
        }
        // Snapshot must not exist — we only wrote to the journal.
        assert!(!dir.path().join("index").join("snapshot.bin").exists());

        let idx = Index::open(dir.path()).unwrap();
        assert_eq!(idx.len(), 1);
        let got = idx.get(&key("a", "1.0.0")).unwrap();
        assert_eq!(got.atime_unix, 5_000);
        assert_eq!(idx.get(&key("b", "1.0.0")), None);
    }

    #[test]
    fn compact_writes_snapshot_and_truncates_journal() {
        let dir = tempdir().unwrap();
        let idx = Index::open(dir.path()).unwrap();
        idx.set_compact_threshold(u64::MAX);
        for i in 0..16u8 {
            idx.put(key(&format!("a{i}"), "1.0.0"), entry(i)).unwrap();
        }
        let journal_path = dir.path().join("index").join("journal.log");
        let snapshot_path = dir.path().join("index").join("snapshot.bin");
        let pre_len = std::fs::metadata(&journal_path).unwrap().len();
        assert!(pre_len > HEADER_LEN);
        assert!(!snapshot_path.exists());

        idx.compact().unwrap();

        assert!(snapshot_path.exists());
        assert_eq!(std::fs::metadata(&journal_path).unwrap().len(), HEADER_LEN);

        // Reopening reconstructs the same state from the snapshot.
        let idx2 = Index::open(dir.path()).unwrap();
        assert_eq!(idx2.len(), 16);
        for i in 0..16u8 {
            assert!(idx2.get(&key(&format!("a{i}"), "1.0.0")).is_some());
        }
    }

    #[test]
    fn compact_then_new_puts_then_reopen_replays_tail() {
        let dir = tempdir().unwrap();
        {
            let idx = Index::open(dir.path()).unwrap();
            idx.set_compact_threshold(u64::MAX);
            for i in 0..4u8 {
                idx.put(key(&format!("a{i}"), "1.0.0"), entry(i)).unwrap();
            }
            idx.compact().unwrap();
            // Add a few more after compaction.
            idx.put(key("post1", "1.0.0"), entry(0xF0)).unwrap();
            idx.touch(&key("a1", "1.0.0"), 9_999).unwrap();
            idx.remove(&key("a2", "1.0.0")).unwrap();
        }
        let idx = Index::open(dir.path()).unwrap();
        assert_eq!(idx.len(), 4); // 4 - 1 (removed a2) + 1 (post1)
        assert!(idx.get(&key("a0", "1.0.0")).is_some());
        assert_eq!(idx.get(&key("a1", "1.0.0")).unwrap().atime_unix, 9_999);
        assert!(idx.get(&key("a2", "1.0.0")).is_none());
        assert!(idx.get(&key("a3", "1.0.0")).is_some());
        assert!(idx.get(&key("post1", "1.0.0")).is_some());
    }

    #[test]
    fn compact_threshold_triggers_automatic_compaction() {
        let dir = tempdir().unwrap();
        let idx = Index::open(dir.path()).unwrap();
        idx.set_compact_threshold(3);
        let journal_path = dir.path().join("index").join("journal.log");
        let snapshot_path = dir.path().join("index").join("snapshot.bin");

        idx.put(key("a", "1.0.0"), entry(1)).unwrap();
        idx.put(key("b", "1.0.0"), entry(2)).unwrap();
        assert!(!snapshot_path.exists());

        // Third put crosses the threshold and triggers compaction.
        idx.put(key("c", "1.0.0"), entry(3)).unwrap();
        assert!(snapshot_path.exists());
        assert_eq!(std::fs::metadata(&journal_path).unwrap().len(), HEADER_LEN);
    }

    #[test]
    fn bincode_round_trip_of_index_entry() {
        let e = entry(0xAB);
        let cfg = bincode::config::standard();
        let bytes = bincode::serde::encode_to_vec(&e, cfg).unwrap();
        let (decoded, _): (IndexEntry, _) = bincode::serde::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(decoded, e);
    }

    #[test]
    fn classifier_distinguishes_index_keys() {
        let dir = tempdir().unwrap();
        let idx = Index::open(dir.path()).unwrap();
        let k_main = IndexKey::new(
            Coords::new("org.example", "a").unwrap(),
            "1.0.0",
            "jar",
            None,
        );
        let k_sources = IndexKey::new(
            Coords::new("org.example", "a").unwrap(),
            "1.0.0",
            "jar",
            Some("sources".to_string()),
        );
        idx.put(k_main.clone(), entry(1)).unwrap();
        idx.put(k_sources.clone(), entry(2)).unwrap();
        assert_eq!(idx.len(), 2);
        assert_ne!(
            idx.get(&k_main).unwrap().hash,
            idx.get(&k_sources).unwrap().hash
        );
    }

    #[test]
    fn put_overwrites_existing_entry() {
        let dir = tempdir().unwrap();
        let idx = Index::open(dir.path()).unwrap();
        let k = key("a", "1.0.0");
        idx.put(k.clone(), entry(1)).unwrap();
        idx.put(k.clone(), entry(2)).unwrap();
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.get(&k).unwrap().hash, entry(2).hash);
    }
}
