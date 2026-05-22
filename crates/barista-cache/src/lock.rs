// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-coordinate locking.
//!
//! Coordinates concurrent fetches of the same Maven coord. Two layers:
//!
//! 1. **In-process** ([`CoordLockMap`]): a sharded map of Tokio mutexes
//!    keyed on `(group, artifact, version)`. The resolver acquires the
//!    lock before starting a fetch; only the first concurrent fetcher
//!    does the work, others await the result.
//!
//! 2. **Cross-process** ([`FilesystemLock`]): a per-coord advisory file
//!    lock (`flock(2)` on Unix, `LockFileEx` on Windows) at
//!    `<lock_root>/<aa>/<sha1-of-coords>.lock`. Held for the duration of
//!    a fetch; concurrent processes on the same machine block here.
//!
//! Typical use: acquire the in-process lock first (fast), then the
//! filesystem lock (slow). Drop both when the fetch + index insert is
//! done.
//!
//! ## Runtime
//!
//! This module depends on Tokio at runtime (not just dev). The
//! filesystem-lock acquire path is a **non-blocking poll loop**: each
//! attempt issues a single `try_write()` (non-blocking `flock`/
//! `LockFileEx`) inside `tokio::task::spawn_blocking` and returns
//! immediately, then the loop sleeps a short async backoff before
//! retrying. Crucially, no blocking-pool thread is ever parked inside
//! `flock(2)` waiting for the OS lock — so a timed-out acquirer leaves
//! nothing leaked behind, and a dropped acquire future simply stops
//! polling. (A previous design ran a single blocking `write()` inside
//! `spawn_blocking` and wrapped it in `tokio::time::timeout`; that was
//! unsound, because `timeout` only abandons the *await* of the join
//! handle — it cannot cancel the blocking thread, which stayed parked
//! in `flock()` fighting for the lock long after the "timeout".)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::time::Duration;

use sha1::{Digest, Sha1};
use tokio::sync::Mutex as AsyncMutex;

use barista_coords::Coords;

/// Identity used for locking: a coord plus a version.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CoordVersionKey {
    pub coords: Coords,
    pub version: String,
}

impl CoordVersionKey {
    /// Convenience constructor.
    pub fn new(coords: Coords, version: impl Into<String>) -> Self {
        Self {
            coords,
            version: version.into(),
        }
    }

    /// Canonical string form: `group:artifact:version`.
    pub fn canonical(&self) -> String {
        format!(
            "{}:{}:{}",
            self.coords.group, self.coords.artifact, self.version
        )
    }
}

/// Errors from the filesystem-lock layer.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("I/O error at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("filesystem lock at {path:?} is held by another process; timed out after {seconds}s")]
    Timeout { path: PathBuf, seconds: u64 },
}

const SHARDS: usize = 16;

type ShardMap = HashMap<CoordVersionKey, Arc<AsyncMutex<()>>>;

/// In-process per-coord async mutex map. Cheap to clone.
///
/// Sharded by the first byte of `artifact` (mod 16) to reduce contention
/// on the inner sync mutex that guards the map itself. The sync mutex is
/// only held while looking up / inserting the per-coord async mutex —
/// never across an await.
#[derive(Debug, Clone, Default)]
pub struct CoordLockMap {
    shards: Arc<[SyncMutex<ShardMap>; SHARDS]>,
}

impl CoordLockMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire (or create) the per-coord async mutex. The caller holds
    /// the returned guard for the duration of their critical section.
    pub async fn lock(&self, key: &CoordVersionKey) -> CoordLockGuard {
        let mutex = self.get_or_create(key);
        let guard = mutex.lock_owned().await;
        CoordLockGuard {
            _guard: guard,
            _key: key.clone(),
        }
    }

    /// Try to acquire without waiting. Returns `Some` on success, `None`
    /// if another fetcher is in flight for the same coord.
    pub fn try_lock(&self, key: &CoordVersionKey) -> Option<CoordLockGuard> {
        let mutex = self.get_or_create(key);
        mutex.try_lock_owned().ok().map(|g| CoordLockGuard {
            _guard: g,
            _key: key.clone(),
        })
    }

    fn get_or_create(&self, key: &CoordVersionKey) -> Arc<AsyncMutex<()>> {
        let shard_idx = shard_index(key);
        let mut shard = self.shards[shard_idx].lock().unwrap();
        shard
            .entry(key.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Number of currently-tracked coord locks (across all shards).
    /// Informational for diagnostics. No GC in v0.1 — entries accumulate
    /// for the lifetime of the [`CoordLockMap`]; downstream callers
    /// should keep one map per long-lived process (resolver, daemon).
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().unwrap().len()).sum()
    }

    /// Returns `true` when no coord has ever been locked through this map.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn shard_index(key: &CoordVersionKey) -> usize {
    let bytes = key.coords.artifact.as_bytes();
    let first = if bytes.is_empty() {
        // Fall back to group's first byte if artifact is somehow empty.
        let g = key.coords.group.as_bytes();
        if g.is_empty() { 0 } else { g[0] as usize }
    } else {
        bytes[0] as usize
    };
    first & 0x0f
}

/// RAII guard for an in-process coord lock. Drop to release.
pub struct CoordLockGuard {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    _key: CoordVersionKey,
}

/// Compute the on-disk lock path for a coord. Uses SHA-1 of
/// `group:artifact:version` for a stable, filesystem-safe filename, and
/// fans out via the first two hex characters (`<aa>/<full-hex>.lock`) so
/// a populated lock dir doesn't become a single huge directory.
pub fn lock_path(lock_root: &Path, key: &CoordVersionKey) -> PathBuf {
    let mut hasher = Sha1::new();
    hasher.update(key.canonical().as_bytes());
    let digest = hasher.finalize();
    let hex = hex_encode(&digest);
    let (prefix, _) = hex.split_at(2);
    let mut p = lock_root.to_path_buf();
    p.push(prefix);
    p.push(format!("{hex}.lock"));
    p
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Cross-process filesystem advisory lock for a single coord. Holding
/// this RAII handle keeps the lock; dropping releases it.
///
/// Implemented on top of [`fd_lock::RwLock`]; we always take the
/// exclusive (write) lock since a coord fetch is mutating from the
/// cache's perspective.
pub struct FilesystemLock {
    // Order matters for Drop: `_guard` must drop before `_lock`,
    // which must drop before the file is closed.
    _guard: fd_lock::RwLockWriteGuard<'static, std::fs::File>,
    // `_lock` is the owned `RwLock<File>` the guard borrows from; we
    // stash it in a `Box` so we can hand out a `'static` guard via
    // pointer trickery below.
    _lock: Box<fd_lock::RwLock<std::fs::File>>,
    path: PathBuf,
}

impl std::fmt::Debug for FilesystemLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemLock")
            .field("path", &self.path)
            .finish()
    }
}

/// Initial backoff between non-blocking acquire attempts.
const ACQUIRE_BACKOFF_START: Duration = Duration::from_millis(5);
/// Upper bound on the backoff between attempts (the loop ramps up to
/// this and then polls at this cadence).
const ACQUIRE_BACKOFF_MAX: Duration = Duration::from_millis(100);

impl FilesystemLock {
    /// Acquire the filesystem lock for a coord, blocking indefinitely
    /// until it can be taken. Creates the lock file and any missing
    /// parent directories.
    ///
    /// "Blocking" here means *logically* unbounded — it polls (via a
    /// non-blocking `try_write` + async sleep-backoff) until the lock
    /// frees. It never parks a blocking-pool thread inside `flock(2)`,
    /// so the returned future is cancel-safe: dropping it simply stops
    /// the polling, with nothing leaked behind.
    ///
    /// Callers in long-running or interactive contexts (the CLI, the
    /// daemon) should prefer [`acquire_with_timeout`](Self::acquire_with_timeout)
    /// so a wedged holder can never deadlock them forever.
    pub async fn acquire(lock_root: &Path, key: &CoordVersionKey) -> Result<Self, LockError> {
        let path = lock_path(lock_root, key);
        let mut backoff = ACQUIRE_BACKOFF_START;
        loop {
            if let Some(lock) = Self::try_acquire_once(path.clone()).await? {
                return Ok(lock);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(ACQUIRE_BACKOFF_MAX);
        }
    }

    /// Acquire with a timeout. Returns [`LockError::Timeout`] if the
    /// lock could not be taken within `duration`.
    ///
    /// Unlike a naive `tokio::time::timeout` wrapper around a blocking
    /// `flock`, this is a *truthful* timeout: each attempt is a
    /// non-blocking `try_write` that returns immediately, so when the
    /// deadline elapses there is no parked thread still fighting for
    /// the lock — we simply stop polling.
    pub async fn acquire_with_timeout(
        lock_root: &Path,
        key: &CoordVersionKey,
        duration: Duration,
    ) -> Result<Self, LockError> {
        let path = lock_path(lock_root, key);
        let deadline = tokio::time::Instant::now() + duration;
        let mut backoff = ACQUIRE_BACKOFF_START;
        loop {
            if let Some(lock) = Self::try_acquire_once(path.clone()).await? {
                return Ok(lock);
            }
            // Don't sleep past the deadline; if we'd overrun, bail now.
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(LockError::Timeout {
                    path,
                    seconds: duration.as_secs(),
                });
            }
            let remaining = deadline - now;
            tokio::time::sleep(backoff.min(remaining)).await;
            backoff = (backoff * 2).min(ACQUIRE_BACKOFF_MAX);
        }
    }

    /// One non-blocking acquisition attempt, run on the blocking pool.
    ///
    /// Returns `Ok(Some(lock))` on success, `Ok(None)` if the lock is
    /// currently held by someone else (the attempt returned without
    /// parking), or `Err` on a genuine I/O failure. Because the inner
    /// `try_write` never blocks, the `spawn_blocking` task always
    /// returns promptly — no thread is ever left parked in `flock(2)`.
    async fn try_acquire_once(path: PathBuf) -> Result<Option<Self>, LockError> {
        let path_for_join = path.clone();
        tokio::task::spawn_blocking(move || acquire_blocking_once(path))
            .await
            .map_err(|e| LockError::Io {
                path: path_for_join,
                source: std::io::Error::other(format!("spawn_blocking join: {e}")),
            })?
    }

    /// The on-disk path of this lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Single, non-blocking attempt to take the exclusive lock at `path`.
///
/// Creates the lock file (and parent dirs) as needed, then issues one
/// `try_write()`. On `ErrorKind::WouldBlock` (the `fd-lock` 4.x mapping
/// for a contended `flock`/`LockFileEx` on both Unix and Windows) it
/// returns `Ok(None)` — the caller should back off and retry. Any other
/// I/O error is surfaced as `Err`.
fn acquire_blocking_once(path: PathBuf) -> Result<Option<FilesystemLock>, LockError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| LockError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| LockError::Io {
            path: path.clone(),
            source: e,
        })?;

    let mut boxed: Box<fd_lock::RwLock<std::fs::File>> = Box::new(fd_lock::RwLock::new(file));
    // SAFETY: `boxed` lives on the heap and is stored in the returned
    // struct alongside the guard. The guard's lifetime is tied to the
    // `RwLock` it borrows from; the `RwLock` outlives the guard because
    // `Drop` for `FilesystemLock` runs fields in declaration order
    // (`_guard` first, then `_lock`). We never expose the underlying
    // `RwLock`, so no other reference can outlive the guard.
    let lock_ref: &'static mut fd_lock::RwLock<std::fs::File> = unsafe {
        let ptr: *mut fd_lock::RwLock<std::fs::File> = &mut *boxed;
        &mut *ptr
    };
    match lock_ref.try_write() {
        Ok(guard) => Ok(Some(FilesystemLock {
            _guard: guard,
            _lock: boxed,
            path,
        })),
        // Contended: another holder has the lock. `fd-lock` 4.x maps
        // the non-blocking `flock`/`LockFileEx` "would block" condition
        // to `ErrorKind::WouldBlock` on both Unix and Windows.
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(LockError::Io { path, source: e }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::TempDir;

    fn key(group: &str, artifact: &str, version: &str) -> CoordVersionKey {
        CoordVersionKey {
            coords: Coords::new(group, artifact).unwrap(),
            version: version.to_string(),
        }
    }

    /// Wall-clock watchdog for the **synchronous** lock tests.
    ///
    /// Runs `f` on a spawned thread and waits up to `secs` for it to
    /// finish. If it doesn't, we `panic!` from the test thread instead
    /// of letting a wedged acquire park a worker forever (which would
    /// hang the whole test binary, since the harness main thread blocks
    /// in `recv()` waiting on the worker). The 45 orphaned, forever-
    /// parked `barista_cache-*` test processes that motivated this fix
    /// could never recur with this guard in place.
    ///
    /// On a watchdog trip we deliberately leave the spawned thread
    /// running (we can't safely cancel it) and fail the test fast — the
    /// process exit then reaps it.
    fn with_watchdog<F: FnOnce() + Send + 'static>(secs: u64, f: F) {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            f();
            // Best-effort: the receiver may already be gone if we
            // tripped the watchdog; ignore the send error in that case.
            let _ = tx.send(());
        });
        match rx.recv_timeout(Duration::from_secs(secs)) {
            Ok(()) => {
                handle.join().expect("watched lock test thread panicked");
            }
            Err(_) => {
                panic!("lock test exceeded {secs}s — likely a FilesystemLock acquire regression")
            }
        }
    }

    // 1. Lock on a fresh key succeeds immediately.
    #[tokio::test]
    async fn lock_on_fresh_key_is_immediate() {
        let map = CoordLockMap::new();
        let k = key("org.example", "lib", "1.0");
        let start = std::time::Instant::now();
        let _g = map.lock(&k).await;
        assert!(start.elapsed() < Duration::from_millis(100));
        assert_eq!(map.len(), 1);
    }

    // 2. Two concurrent locks on the same key serialize.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_key_serializes() {
        let map = CoordLockMap::new();
        let k = key("org.example", "lib", "1.0");
        let first_dropped = Arc::new(AtomicBool::new(false));

        let g1 = map.lock(&k).await;
        let map2 = map.clone();
        let k2 = k.clone();
        let flag = first_dropped.clone();
        let h = tokio::spawn(async move {
            let _g2 = map2.lock(&k2).await;
            // Must observe the first guard as already dropped.
            assert!(
                flag.load(Ordering::SeqCst),
                "second locked before first dropped"
            );
        });

        // Give the spawned task a moment to attempt acquisition.
        tokio::time::sleep(Duration::from_millis(50)).await;
        first_dropped.store(true, Ordering::SeqCst);
        drop(g1);
        h.await.unwrap();
    }

    // 3. Two concurrent locks on different keys proceed in parallel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn different_keys_parallel() {
        let map = CoordLockMap::new();
        let k1 = key("org.example", "lib-a", "1.0");
        let k2 = key("org.example", "lib-b", "1.0");

        let _g1 = map.lock(&k1).await;
        // Acquiring k2 while holding k1 must succeed quickly.
        let start = std::time::Instant::now();
        let _g2 = tokio::time::timeout(Duration::from_secs(1), map.lock(&k2))
            .await
            .expect("k2 lock should not block on k1");
        assert!(start.elapsed() < Duration::from_millis(200));
    }

    // 4. try_lock returns None when another guard is held.
    #[tokio::test]
    async fn try_lock_contended_returns_none() {
        let map = CoordLockMap::new();
        let k = key("org.example", "lib", "1.0");
        let _g = map.lock(&k).await;
        assert!(map.try_lock(&k).is_none());
    }

    // 5. try_lock returns Some when nothing's held.
    #[tokio::test]
    async fn try_lock_uncontended_returns_some() {
        let map = CoordLockMap::new();
        let k = key("org.example", "lib", "1.0");
        let g = map.try_lock(&k);
        assert!(g.is_some());
    }

    // 6. len() reflects unique keys ever locked.
    #[tokio::test]
    async fn len_tracks_unique_keys() {
        let map = CoordLockMap::new();
        let k1 = key("g", "a", "1.0");
        let k2 = key("g", "b", "1.0");
        assert!(map.is_empty());
        {
            let _g = map.lock(&k1).await;
        }
        {
            let _g = map.lock(&k2).await;
        }
        // Re-lock k1: should not increase len.
        {
            let _g = map.lock(&k1).await;
        }
        assert_eq!(map.len(), 2);
    }

    // 7. Sharding spreads load (statistical).
    #[test]
    fn shards_spread_load() {
        let mut shards_hit = HashSet::new();
        for i in 0..64u8 {
            // Vary first byte across the alphabet to exercise different shards.
            let artifact = format!("{}artifact", (b'a' + (i % 26)) as char);
            let k = key("g", &artifact, "1.0");
            shards_hit.insert(shard_index(&k));
        }
        // With 26 different first bytes we should easily hit > 6 shards.
        assert!(shards_hit.len() > 6, "only hit {} shards", shards_hit.len());
    }

    // 8. FilesystemLock::acquire creates the file at the documented path.
    #[tokio::test]
    async fn fs_lock_creates_file_at_documented_path() {
        let tmp = TempDir::new().unwrap();
        let k = key("org.example", "lib", "1.0");
        let expected = lock_path(tmp.path(), &k);
        let lock = FilesystemLock::acquire(tmp.path(), &k).await.unwrap();
        assert_eq!(lock.path(), expected);
        assert!(expected.exists(), "lock file should exist on disk");
        // <aa>/<hex>.lock layout.
        let parent = expected.parent().unwrap();
        assert_eq!(parent.file_name().unwrap().to_string_lossy().len(), 2);
    }

    // 9. FilesystemLock drop releases the lock.
    #[tokio::test]
    async fn fs_lock_drop_releases() {
        let tmp = TempDir::new().unwrap();
        let k = key("org.example", "lib", "1.0");
        let lock = FilesystemLock::acquire(tmp.path(), &k).await.unwrap();
        drop(lock);
        // Re-acquisition should succeed quickly.
        let again = tokio::time::timeout(
            Duration::from_secs(2),
            FilesystemLock::acquire(tmp.path(), &k),
        )
        .await
        .expect("re-acquire timed out")
        .unwrap();
        drop(again);
    }

    // 10. Two runtimes racing for the same FilesystemLock.
    //
    // Wall-clock-guarded (see `with_watchdog`): the contended acquirer
    // must time out at ~300ms, never park. A 15s watchdog turns any
    // regression that re-introduces a blocking acquire into a fast,
    // loud failure instead of an indefinite hang.
    #[test]
    fn fs_lock_excludes_concurrent_acquirers() {
        with_watchdog(15, || {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();
            let k = key("org.example", "lib", "1.0");

            let rt1 = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let lock1 = rt1.block_on(FilesystemLock::acquire(&root, &k)).unwrap();

            let root2 = root.clone();
            let k2 = k.clone();
            let handle = std::thread::spawn(move || {
                let rt2 = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt2.block_on(FilesystemLock::acquire_with_timeout(
                    &root2,
                    &k2,
                    Duration::from_millis(300),
                ))
            });

            let result = handle.join().unwrap();
            assert!(
                matches!(result, Err(LockError::Timeout { .. })),
                "expected Timeout, got {result:?}"
            );
            drop(lock1);
        });
    }

    // 11. acquire_with_timeout returns Timeout when contended.
    //
    // Wall-clock-guarded via `tokio::time::timeout`: the 200ms acquire
    // timeout must fire promptly; the 15s outer guard fails fast if the
    // inner acquire ever wedges instead of timing out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fs_lock_timeout_when_contended() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let tmp = TempDir::new().unwrap();
            let k = key("org.example", "lib", "1.0");
            let _held = FilesystemLock::acquire(tmp.path(), &k).await.unwrap();

            let result =
                FilesystemLock::acquire_with_timeout(tmp.path(), &k, Duration::from_millis(200))
                    .await;
            assert!(matches!(result, Err(LockError::Timeout { .. })));
        })
        .await
        .expect("lock test exceeded 15s — likely a FilesystemLock acquire regression");
    }

    // 12. lock_path is stable across runs.
    #[test]
    fn lock_path_is_stable() {
        let root = Path::new("/tmp/locks");
        let k = key("org.example", "lib", "1.0");
        let p1 = lock_path(root, &k);
        let p2 = lock_path(root, &k);
        assert_eq!(p1, p2);
        // And matches a known prefix layout.
        assert!(p1.starts_with(root));
        assert_eq!(p1.extension().and_then(|s| s.to_str()), Some("lock"));
    }

    // 13. lock_path differs for different coords/versions.
    #[test]
    fn lock_path_differs_per_key() {
        let root = Path::new("/tmp/locks");
        let k1 = key("g", "a", "1.0");
        let k2 = key("g", "a", "2.0");
        let k3 = key("g", "b", "1.0");
        let k4 = key("g2", "a", "1.0");
        let paths: HashSet<_> = [&k1, &k2, &k3, &k4]
            .iter()
            .map(|k| lock_path(root, k))
            .collect();
        assert_eq!(paths.len(), 4);
    }

    // 14. lock_path is filesystem-safe.
    #[test]
    fn lock_path_is_filesystem_safe() {
        let root = Path::new("/tmp/locks");
        // Tricky coord components that would be unsafe if interpolated raw.
        let k = key("org/example", "lib..weird", "1.0/../2.0");
        let p = lock_path(root, &k);
        let rel = p.strip_prefix(root).unwrap();
        let rel_str = rel.to_string_lossy();
        // No path traversal or slashes beyond the fanout separator.
        assert!(!rel_str.contains(".."), "rel={rel_str}");
        // Exactly one separator: <aa>/<hex>.lock.
        let components: Vec<_> = rel.components().collect();
        assert_eq!(components.len(), 2);
        for comp in &components {
            let s = comp.as_os_str().to_string_lossy();
            assert!(
                s.chars().all(|c| c.is_ascii_hexdigit()
                    || c == '.'
                    || c == 'l'
                    || c == 'o'
                    || c == 'c'
                    || c == 'k'),
                "unsafe char in component: {s}"
            );
        }
    }
}
