// SPDX-License-Identifier: MIT OR Apache-2.0

//! Append-only journal for the cache index.
//!
//! Every mutation of the in-memory cache index (put / remove / touch)
//! appends a single length-prefixed, CRC32-checksummed record to the
//! journal. A periodic [`Index::compact`](crate::index::Index::compact)
//! rewrites the snapshot file and truncates the journal back to its
//! header.
//!
//! # On-disk format
//!
//! ```text
//! File header (10 bytes):
//!   4 bytes  magic = "BCAS"
//!   4 bytes  version (u32 LE)
//!   2 bytes  reserved (zero; future flags)
//!
//! Per record:
//!   4 bytes  payload_length (u32 LE)
//!   N bytes  bincode payload (serde-encoded `JournalEntry`)
//!   4 bytes  CRC32 of payload (u32 LE)
//! ```
//!
//! The journal magic + the snapshot magic intentionally share the same
//! four-byte prefix (`b"BCAS"`); readers dispatch on the post-magic
//! version word so the two file types can evolve independently.
//!
//! Tail-truncation tolerance: an `iter_entries` walk that runs off the
//! end of the file mid-record (e.g. a power loss between `write` and
//! the next `fsync`) yields a [`JournalError::Truncated`] at the point
//! of failure and stops. Higher layers (T10 crash recovery) decide
//! whether to accept the partial state or rebuild from the CAS.
//!
//! # Performance note
//!
//! v0.1 uses plain `serde + bincode`. The plan mentions rkyv zero-copy
//! as a future optimization; at expected cache sizes (low millions of
//! entries) the difference is invisible, and bincode keeps the encoder
//! simple. Swapping codecs is a localized change behind this module's
//! API.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::index::{IndexEntry, IndexKey};

/// Magic bytes at the start of journal AND snapshot files.
pub const FILE_MAGIC: &[u8; 4] = b"BCAS";

/// Current journal-file version. Bumped on any breaking format change.
pub const JOURNAL_VERSION: u32 = 1;

/// Current snapshot-file version. Bumped on any breaking format change.
pub const SNAPSHOT_VERSION: u32 = 1;

/// Length of the fixed file header: 4 magic + 4 version + 2 reserved.
pub(crate) const HEADER_LEN: u64 = 10;

/// Errors that can arise reading or writing a journal file.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// Underlying filesystem error.
    #[error("journal I/O at {path:?}: {source}")]
    Io {
        /// Path that produced the error.
        path: PathBuf,
        /// Originating `std::io::Error`.
        source: std::io::Error,
    },
    /// The file did not start with the expected four-byte magic.
    #[error("journal at {path:?} has bad magic (expected {expected:?}, got {got:?})")]
    BadMagic {
        /// Path of the offending file.
        path: PathBuf,
        /// Magic bytes this build was looking for.
        expected: [u8; 4],
        /// Magic bytes actually read from disk.
        got: [u8; 4],
    },
    /// Magic matched but the version word is outside the range this
    /// build understands.
    #[error(
        "journal at {path:?} has unsupported version: {version} (this build understands {expected})"
    )]
    UnsupportedVersion {
        /// Path of the offending file.
        path: PathBuf,
        /// Version word read from the file.
        version: u32,
        /// Highest version this build supports.
        expected: u32,
    },
    /// A record's length prefix promised more bytes than the file
    /// contained — typically a power loss between write and fsync.
    #[error("journal at {path:?} ends mid-record (truncation detected)")]
    Truncated {
        /// Path of the offending file.
        path: PathBuf,
    },
    /// A record's payload bytes did not match its trailing CRC32.
    #[error("journal record at offset {offset} fails CRC32 checksum")]
    BadChecksum {
        /// Byte offset of the record's length prefix within the file.
        offset: u64,
    },
    /// Bincode failed to encode or decode a record's payload.
    #[error("bincode encode/decode error at offset {offset}: {detail}")]
    Bincode {
        /// Byte offset of the affected record's payload.
        offset: u64,
        /// Decoder/encoder diagnostic message.
        detail: String,
    },
    /// The cross-process journal lock could not be acquired within the
    /// timeout — another process is holding it for longer than expected.
    #[error("timed out after {seconds}s acquiring journal lock at {path:?}")]
    LockTimeout {
        /// Path of the `.lock` file that is contended.
        path: PathBuf,
        /// Number of seconds waited before giving up.
        seconds: u64,
    },
}

/// A single mutation of the cache index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalEntry {
    /// Insert or replace an entry.
    Put {
        /// Key being inserted.
        key: IndexKey,
        /// Value being written.
        entry: IndexEntry,
    },
    /// Remove an entry.
    Remove {
        /// Key being removed.
        key: IndexKey,
    },
    /// Update the access time of an existing entry.
    Touch {
        /// Key whose atime is being bumped.
        key: IndexKey,
        /// New UNIX-seconds atime value.
        atime_unix: u64,
    },
}

/// Default time to wait for the cross-process journal lock before
/// giving up with [`JournalError::LockTimeout`]. Generous enough to sit
/// behind another process finishing a full `barista pull`, short enough
/// that a genuinely wedged (not crashed) holder can't hang us forever.
/// A *crashed* holder never reaches this: `flock` releases automatically
/// when the owning process dies.
const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// Cross-process exclusive lock guarding a single journal write or
/// truncation. Acquired transiently at the start of each mutating
/// operation and released when this RAII guard drops at the end of it.
///
/// `barista pull` is a one-shot process and the cache journal has no
/// other cross-process serialization. Two unsynchronized writers against
/// the same cache otherwise race on the file offset and interleave each
/// other's `len|payload|crc` triples, leaving a tail that doesn't parse.
/// Taking this lock around every append + truncate serializes the actual
/// file mutations across processes so that can't happen. The lock lives
/// on a sibling `.lock` file (never the journal itself) so reader fds
/// opened for iteration are never blocked.
///
/// Locking per-operation (rather than for the journal's whole lifetime)
/// is deliberate: it mirrors the per-fetch locking in [`crate::lock`] and
/// lets a single process legitimately hold more than one read-mostly
/// [`Index`](crate::index::Index) handle on the same cache without
/// self-deadlocking — only the brief mutation windows contend.
///
/// Implemented on top of [`fd_lock::RwLock`], mirroring the
/// self-referential held-lock pattern in [`crate::lock`].
struct JournalLock {
    // Drop order matters: `_guard` must drop (release the flock) before
    // `_lock` frees the `RwLock` it borrows from and the file closes.
    _guard: fd_lock::RwLockWriteGuard<'static, File>,
    _lock: Box<fd_lock::RwLock<File>>,
}

impl std::fmt::Debug for JournalLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalLock").finish_non_exhaustive()
    }
}

impl JournalLock {
    /// Acquire the exclusive lock at `lock_path`, polling a non-blocking
    /// `try_write` with sleep-backoff until it frees or `timeout` elapses.
    ///
    /// Polling (rather than a blocking `flock`) keeps the timeout
    /// truthful: when the deadline passes there is no thread parked in
    /// `flock(2)` still fighting for the lock.
    fn acquire(lock_path: &Path, timeout: Duration) -> Result<Self, JournalError> {
        let deadline = Instant::now() + timeout;
        let mut backoff = Duration::from_millis(5);
        loop {
            if let Some(lock) = Self::try_acquire_once(lock_path)? {
                return Ok(lock);
            }
            if Instant::now() >= deadline {
                return Err(JournalError::LockTimeout {
                    path: lock_path.to_path_buf(),
                    seconds: timeout.as_secs(),
                });
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_millis(100));
        }
    }

    /// A single non-blocking acquisition attempt. Returns `Ok(Some)` on
    /// success, `Ok(None)` if another holder currently has the lock, or
    /// `Err` on a genuine I/O failure.
    ///
    /// Each attempt opens a fresh lock-file handle (matching the
    /// single-attempt design in [`crate::lock`]) so the retry loop in
    /// [`Self::acquire`] never re-borrows a live `RwLock` — that would
    /// not satisfy the borrow checker with the `'static` guard below.
    fn try_acquire_once(lock_path: &Path) -> Result<Option<Self>, JournalError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|e| JournalError::Io {
                path: lock_path.to_path_buf(),
                source: e,
            })?;

        let mut boxed: Box<fd_lock::RwLock<File>> = Box::new(fd_lock::RwLock::new(file));
        // SAFETY: `boxed` is heap-allocated and stored in the returned
        // struct alongside the guard. The guard borrows the `RwLock`,
        // which outlives it because `Drop` runs fields in declaration
        // order (`_guard` before `_lock`). The `RwLock` is never exposed,
        // so no other reference can outlive the guard.
        let lock_ref: &'static mut fd_lock::RwLock<File> = unsafe {
            let ptr: *mut fd_lock::RwLock<File> = &mut *boxed;
            &mut *ptr
        };
        match lock_ref.try_write() {
            Ok(guard) => Ok(Some(Self {
                _guard: guard,
                _lock: boxed,
            })),
            // `fd-lock` 4.x maps a contended non-blocking
            // `flock`/`LockFileEx` to `WouldBlock` on Unix + Windows.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(JournalError::Io {
                path: lock_path.to_path_buf(),
                source: e,
            }),
        }
    }
}

/// The append-only journal file.
///
/// Multiple [`Index`](crate::index::Index) handles can share one
/// `Journal` through an `Arc`; concurrent appenders serialize through
/// the internal `Mutex`. Across *processes*, each mutation takes a
/// transient [`JournalLock`] on the sibling `.lock` file so writers
/// cannot corrupt the tail.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    /// Sibling `.lock` file the cross-process [`JournalLock`] is taken on.
    lock_path: PathBuf,
    write_handle: Mutex<BufWriter<File>>,
}

impl Journal {
    /// Open (creating if necessary) the journal at `path`.
    ///
    /// If the file is empty, the 10-byte header is written and
    /// `fsync`ed before the call returns. Otherwise the magic and
    /// version word are validated; mismatches surface as
    /// [`JournalError::BadMagic`] / [`JournalError::UnsupportedVersion`].
    ///
    /// The fresh-file header initialization is serialized across
    /// processes via a transient [`JournalLock`] on a sibling `.lock`
    /// file, so two processes racing to create the same cache can't both
    /// write a header. Subsequent mutations re-take that lock per
    /// operation (see [`Self::append`]).
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        let lock_path = lock_path_for(path);

        // Open in append mode: O_APPEND makes every write land at the
        // current EOF, so even if the advisory lock is ever unenforced
        // (e.g. NFS), records are never written over a stale cached
        // offset. Combined with the single-`write_all` record assembly in
        // `append`, each record reaches the file as one write at EOF.
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| JournalError::Io {
                path: path.to_path_buf(),
                source: e,
            })?;

        // Hold the cross-process lock across the empty-file check + header
        // write so the initialization is atomic between racing creators.
        let _init_lock = JournalLock::acquire(&lock_path, LOCK_ACQUIRE_TIMEOUT)?;
        let len = file
            .metadata()
            .map_err(|e| JournalError::Io {
                path: path.to_path_buf(),
                source: e,
            })?
            .len();

        if len == 0 {
            write_header(&mut file, JOURNAL_VERSION).map_err(|e| JournalError::Io {
                path: path.to_path_buf(),
                source: e,
            })?;
            file.sync_all().map_err(|e| JournalError::Io {
                path: path.to_path_buf(),
                source: e,
            })?;
        } else {
            validate_header(&mut file, path, JOURNAL_VERSION)?;
        }

        file.seek(SeekFrom::End(0)).map_err(|e| JournalError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        drop(_init_lock);

        Ok(Self {
            path: path.to_path_buf(),
            lock_path,
            write_handle: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Append one entry. Returns the file offset at which the record's
    /// length prefix landed.
    ///
    /// Takes the cross-process [`JournalLock`] for the duration of the
    /// write, then re-seeks to the true end of file under that lock so
    /// the recorded offset and the written bytes agree even if another
    /// process appended since this handle last wrote. The append is
    /// flushed and `fsync`ed before return so an in-progress write cannot
    /// be observed as committed by a later reader.
    pub fn append(&self, entry: &JournalEntry) -> Result<u64, JournalError> {
        let payload = encode_payload(entry, 0)?;
        let crc = crc32fast::hash(&payload);
        let len = u32::try_from(payload.len()).map_err(|_| JournalError::Bincode {
            offset: 0,
            detail: "payload exceeds u32::MAX bytes".to_string(),
        })?;

        // Acquire the cross-process lock first, then the in-process mutex.
        // This ordering is consistent across every mutating method so the
        // two locks never deadlock against each other.
        let _xlock = JournalLock::acquire(&self.lock_path, LOCK_ACQUIRE_TIMEOUT)?;
        let mut guard = self.write_handle.lock().expect("journal mutex poisoned");
        // Flush any buffered writes, then re-seek to the real EOF. Under
        // O_APPEND the bytes always land at EOF regardless, but another
        // process may have grown the file since our cached offset was
        // last updated — re-seeking makes the offset we *return* truthful.
        guard.flush().map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        let offset = guard
            .get_mut()
            .seek(SeekFrom::End(0))
            .map_err(|e| JournalError::Io {
                path: self.path.clone(),
                source: e,
            })?;

        // Assemble the whole record into one buffer and emit it with a
        // single `write_all`, so a record reaches the file as one
        // contiguous append rather than three separate writes that a
        // concurrent (or crash-interrupted) writer could interleave.
        let mut record = Vec::with_capacity(4 + payload.len() + 4);
        record.extend_from_slice(&len.to_le_bytes());
        record.extend_from_slice(&payload);
        record.extend_from_slice(&crc.to_le_bytes());
        guard
            .write_all(&record)
            .and_then(|()| guard.flush())
            .map_err(|e| JournalError::Io {
                path: self.path.clone(),
                source: e,
            })?;
        guard.get_ref().sync_data().map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;

        Ok(offset)
    }

    /// Stream every well-formed entry. Stops on the first truncated or
    /// CRC-failed tail record (yielding the error and then ending the
    /// iterator).
    pub fn iter_entries(
        &self,
    ) -> Result<impl Iterator<Item = Result<JournalEntry, JournalError>>, JournalError> {
        // Open a fresh read handle so concurrent appends don't fight
        // for the write-side seek position.
        let mut file = File::open(&self.path).map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        validate_header(&mut file, &self.path, JOURNAL_VERSION)?;
        Ok(JournalIter {
            path: self.path.clone(),
            reader: BufReader::new(file),
            position: HEADER_LEN,
            done: false,
        })
    }

    /// Like [`Self::iter_entries`] but also tracks the file byte
    /// offset just past the most recently decoded record. Useful for
    /// crash-recovery code that wants to truncate the journal at the
    /// last known-good record boundary.
    ///
    /// The returned iterator is the same as `iter_entries` but
    /// callers can read `last_good_offset()` after iteration ends to
    /// learn where to cut.
    pub fn iter_entries_with_positions(&self) -> Result<JournalIter, JournalError> {
        let mut file = File::open(&self.path).map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        validate_header(&mut file, &self.path, JOURNAL_VERSION)?;
        Ok(JournalIter {
            path: self.path.clone(),
            reader: BufReader::new(file),
            position: HEADER_LEN,
            done: false,
        })
    }

    /// Truncate the journal to `offset` bytes — chops off any
    /// corrupted / truncated tail records past that point. The
    /// caller is responsible for choosing an offset that lands on a
    /// record boundary (typically the value returned by
    /// [`JournalIter::last_good_offset`]).
    ///
    /// `offset` must be at least [`HEADER_LEN`]; smaller values are
    /// clamped up so the file header is never destroyed.
    pub fn truncate_to(&self, offset: u64) -> Result<(), JournalError> {
        let target = offset.max(HEADER_LEN);
        let _xlock = JournalLock::acquire(&self.lock_path, LOCK_ACQUIRE_TIMEOUT)?;
        let mut guard = self.write_handle.lock().expect("journal mutex poisoned");
        guard.flush().map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        let file = guard.get_mut();
        file.set_len(target).map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        file.seek(SeekFrom::End(0)).map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        file.sync_all().map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        Ok(())
    }

    /// Truncate the file back to a bare header. Called after a
    /// successful snapshot rewrite.
    pub fn truncate(&self) -> Result<(), JournalError> {
        let _xlock = JournalLock::acquire(&self.lock_path, LOCK_ACQUIRE_TIMEOUT)?;
        let mut guard = self.write_handle.lock().expect("journal mutex poisoned");
        guard.flush().map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        let file = guard.get_mut();
        file.set_len(0).map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| JournalError::Io {
                path: self.path.clone(),
                source: e,
            })?;
        write_header(file, JOURNAL_VERSION).map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        file.sync_all().map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        // BufWriter's internal position now matches the file's end.
        Ok(())
    }

    /// Path this journal was opened at.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Sibling lock-file path for a journal at `path`: `<dir>/.lock`. The
/// lock is deliberately a separate file from the journal so iteration's
/// read fds are never blocked by the writer's advisory lock.
fn lock_path_for(path: &Path) -> PathBuf {
    match path.parent() {
        Some(dir) => dir.join(".lock"),
        None => PathBuf::from(".lock"),
    }
}

fn write_header<W: Write>(w: &mut W, version: u32) -> std::io::Result<()> {
    w.write_all(FILE_MAGIC)?;
    w.write_all(&version.to_le_bytes())?;
    w.write_all(&[0u8, 0u8])?;
    Ok(())
}

/// Validate (magic + version) the 10-byte header on `file`. Leaves
/// the file's read cursor pointing just past the header on success.
pub(crate) fn validate_header(
    file: &mut File,
    path: &Path,
    expected_version: u32,
) -> Result<(), JournalError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|e| JournalError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    let mut header = [0u8; 10];
    file.read_exact(&mut header).map_err(|e| JournalError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    let magic: [u8; 4] = header[0..4].try_into().expect("4 bytes");
    if &magic != FILE_MAGIC {
        return Err(JournalError::BadMagic {
            path: path.to_path_buf(),
            expected: *FILE_MAGIC,
            got: magic,
        });
    }
    let version = u32::from_le_bytes(header[4..8].try_into().expect("4 bytes"));
    if version != expected_version {
        return Err(JournalError::UnsupportedVersion {
            path: path.to_path_buf(),
            version,
            expected: expected_version,
        });
    }
    Ok(())
}

fn encode_payload(entry: &JournalEntry, offset: u64) -> Result<Vec<u8>, JournalError> {
    let cfg = bincode::config::standard();
    bincode::serde::encode_to_vec(entry, cfg).map_err(|e| JournalError::Bincode {
        offset,
        detail: e.to_string(),
    })
}

fn decode_payload(bytes: &[u8], offset: u64) -> Result<JournalEntry, JournalError> {
    let cfg = bincode::config::standard();
    let (entry, _) =
        bincode::serde::decode_from_slice::<JournalEntry, _>(bytes, cfg).map_err(|e| {
            JournalError::Bincode {
                offset,
                detail: e.to_string(),
            }
        })?;
    Ok(entry)
}

/// Iterator over journal records.
///
/// Exposed (rather than hidden behind `impl Iterator`) so crash-
/// recovery code can call [`JournalIter::last_good_offset`] after
/// iteration to learn where the corrupted tail begins.
pub struct JournalIter {
    path: PathBuf,
    reader: BufReader<File>,
    position: u64,
    done: bool,
}

impl JournalIter {
    /// Byte offset just past the last successfully decoded record.
    /// Equals [`HEADER_LEN`] if no records were read.
    pub fn last_good_offset(&self) -> u64 {
        self.position
    }
}

impl Iterator for JournalIter {
    type Item = Result<JournalEntry, JournalError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        let mut len_buf = [0u8; 4];
        match self.reader.read(&mut len_buf) {
            Ok(0) => {
                self.done = true;
                return None;
            }
            Ok(n) if n < 4 => {
                self.done = true;
                return Some(Err(JournalError::Truncated {
                    path: self.path.clone(),
                }));
            }
            Ok(_) => {}
            Err(e) => {
                self.done = true;
                return Some(Err(JournalError::Io {
                    path: self.path.clone(),
                    source: e,
                }));
            }
        }
        let payload_len = u32::from_le_bytes(len_buf) as usize;
        let record_start = self.position;

        let mut payload = vec![0u8; payload_len];
        if let Err(e) = self.reader.read_exact(&mut payload) {
            self.done = true;
            return Some(Err(match e.kind() {
                std::io::ErrorKind::UnexpectedEof => JournalError::Truncated {
                    path: self.path.clone(),
                },
                _ => JournalError::Io {
                    path: self.path.clone(),
                    source: e,
                },
            }));
        }

        let mut crc_buf = [0u8; 4];
        if let Err(e) = self.reader.read_exact(&mut crc_buf) {
            self.done = true;
            return Some(Err(match e.kind() {
                std::io::ErrorKind::UnexpectedEof => JournalError::Truncated {
                    path: self.path.clone(),
                },
                _ => JournalError::Io {
                    path: self.path.clone(),
                    source: e,
                },
            }));
        }
        let expected_crc = u32::from_le_bytes(crc_buf);
        let actual_crc = crc32fast::hash(&payload);
        if expected_crc != actual_crc {
            self.done = true;
            return Some(Err(JournalError::BadChecksum {
                offset: record_start,
            }));
        }

        self.position += 4 + payload_len as u64 + 4;
        Some(decode_payload(&payload, record_start + 4))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::ContentHash;
    use crate::index::{IndexEntry, IndexKey, Origin};
    use barista_coords::Coords;
    use tempfile::tempdir;

    fn sample_key(artifact: &str) -> IndexKey {
        IndexKey::new(
            Coords::new("org.example", artifact).unwrap(),
            "1.0.0",
            "jar",
            None,
        )
    }

    fn sample_entry(byte: u8) -> IndexEntry {
        IndexEntry {
            hash: ContentHash::from_hex(&hex_repeat(byte)).unwrap(),
            size_bytes: 1024,
            sha1_hex: None,
            origin: Origin {
                repository_url: "https://repo.example/maven2".to_string(),
                etag: None,
                last_modified: None,
                upstream_last_updated: None,
                tier: Default::default(),
            },
            atime_unix: 1_700_000_000,
            created_unix: 1_700_000_000,
        }
    }

    fn hex_repeat(b: u8) -> String {
        let mut s = String::with_capacity(64);
        for _ in 0..32 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    #[test]
    fn fresh_journal_has_correct_header() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let _j = Journal::open(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], FILE_MAGIC);
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            JOURNAL_VERSION
        );
        assert_eq!(&bytes[8..10], &[0u8, 0u8]);
        assert_eq!(bytes.len() as u64, HEADER_LEN);
    }

    #[test]
    fn append_then_iter_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let j = Journal::open(&path).unwrap();

        let k1 = sample_key("a");
        let k2 = sample_key("b");
        let e1 = sample_entry(0x11);
        let e2 = sample_entry(0x22);

        j.append(&JournalEntry::Put {
            key: k1.clone(),
            entry: e1.clone(),
        })
        .unwrap();
        j.append(&JournalEntry::Touch {
            key: k1.clone(),
            atime_unix: 42,
        })
        .unwrap();
        j.append(&JournalEntry::Put {
            key: k2.clone(),
            entry: e2.clone(),
        })
        .unwrap();
        j.append(&JournalEntry::Remove { key: k2.clone() }).unwrap();

        let entries: Vec<_> = j
            .iter_entries()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 4);
        assert!(matches!(entries[0], JournalEntry::Put { .. }));
        assert!(matches!(
            entries[1],
            JournalEntry::Touch { atime_unix: 42, .. }
        ));
        assert!(matches!(entries[2], JournalEntry::Put { .. }));
        assert!(matches!(entries[3], JournalEntry::Remove { .. }));
    }

    #[test]
    fn bad_magic_returns_bad_magic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        std::fs::write(&path, b"XXXX\x01\x00\x00\x00\x00\x00").unwrap();
        let err = Journal::open(&path).unwrap_err();
        match err {
            JournalError::BadMagic { got, .. } => {
                assert_eq!(&got, b"XXXX");
            }
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_version_returns_unsupported_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(FILE_MAGIC);
        bytes.extend_from_slice(&999u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8, 0u8]);
        std::fs::write(&path, &bytes).unwrap();
        let err = Journal::open(&path).unwrap_err();
        match err {
            JournalError::UnsupportedVersion {
                version, expected, ..
            } => {
                assert_eq!(version, 999);
                assert_eq!(expected, JOURNAL_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn truncated_tail_record_returns_truncated() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        {
            let j = Journal::open(&path).unwrap();
            j.append(&JournalEntry::Put {
                key: sample_key("a"),
                entry: sample_entry(0xAA),
            })
            .unwrap();
        }
        let full_len = std::fs::metadata(&path).unwrap().len();
        // Drop the final 4 bytes (the trailing CRC) plus one payload
        // byte so we hit mid-record EOF.
        let truncated = full_len - 5;
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(truncated).unwrap();
        drop(file);

        let j = Journal::open(&path).unwrap();
        let mut it = j.iter_entries().unwrap();
        let first = it.next().expect("at least one yield");
        assert!(matches!(first, Err(JournalError::Truncated { .. })));
        assert!(it.next().is_none(), "iterator must stop after truncation");
    }

    #[test]
    fn corrupted_crc_returns_bad_checksum_at_offset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        {
            let j = Journal::open(&path).unwrap();
            j.append(&JournalEntry::Put {
                key: sample_key("a"),
                entry: sample_entry(0xAA),
            })
            .unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        // Flip a bit in the trailing CRC.
        bytes[len - 1] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let j = Journal::open(&path).unwrap();
        let mut it = j.iter_entries().unwrap();
        let first = it.next().expect("at least one yield");
        match first {
            Err(JournalError::BadChecksum { offset }) => {
                assert_eq!(offset, HEADER_LEN);
            }
            other => panic!("expected BadChecksum, got {other:?}"),
        }
    }

    #[test]
    fn truncate_rewinds_to_header_only() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let j = Journal::open(&path).unwrap();
        for i in 0..5u8 {
            j.append(&JournalEntry::Put {
                key: sample_key(&format!("a{i}")),
                entry: sample_entry(i),
            })
            .unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() > HEADER_LEN);
        j.truncate().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), HEADER_LEN);

        // Subsequent appends still work and round-trip.
        j.append(&JournalEntry::Put {
            key: sample_key("post"),
            entry: sample_entry(0x77),
        })
        .unwrap();
        let entries: Vec<_> = j
            .iter_entries()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn empty_journal_iterates_zero_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let j = Journal::open(&path).unwrap();
        let entries: Vec<_> = j.iter_entries().unwrap().collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn append_after_reopen_preserves_prior_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        {
            let j = Journal::open(&path).unwrap();
            j.append(&JournalEntry::Put {
                key: sample_key("a"),
                entry: sample_entry(1),
            })
            .unwrap();
        }
        let j = Journal::open(&path).unwrap();
        j.append(&JournalEntry::Put {
            key: sample_key("b"),
            entry: sample_entry(2),
        })
        .unwrap();
        let entries: Vec<_> = j
            .iter_entries()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 2);
    }

    // --- Cross-process / multi-handle write safety (T12) -------------
    //
    // The reported bug: two unsynchronized `barista pull` processes
    // against the same cache raced on the journal's cached write offset
    // and overwrote each other's records, leaving a tail that doesn't
    // parse. These tests reproduce the same hazard *in-process* via two
    // independent `Journal` handles on one path (each handle has its own
    // fd + cached offset, exactly like two processes) and assert the
    // cross-process lock + seek-to-EOF + single-`write_all` fix keeps
    // every record intact. Without the fix they fail: the second
    // appender writes over the first at a stale offset.

    #[test]
    fn second_handle_on_same_path_opens_without_deadlock() {
        // Per-operation (not lifetime) locking must let one process hold
        // two handles on the same journal at once — `Index` reopen paths
        // and verification helpers rely on this.
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let _a = Journal::open(&path).unwrap();
        let _b = Journal::open(&path).unwrap();
    }

    #[test]
    fn two_handles_alternating_appends_do_not_corrupt_tail() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let a = Journal::open(&path).unwrap();
        let b = Journal::open(&path).unwrap();

        // Alternate appends between the two handles. Each handle's cached
        // offset goes stale the instant the *other* one appends; the fix
        // re-seeks to true EOF under the lock so neither clobbers the
        // other.
        for i in 0..20u8 {
            a.append(&JournalEntry::Put {
                key: sample_key(&format!("a{i}")),
                entry: sample_entry(i),
            })
            .unwrap();
            b.append(&JournalEntry::Put {
                key: sample_key(&format!("b{i}")),
                entry: sample_entry(i),
            })
            .unwrap();
        }
        drop(a);
        drop(b);

        // Reopen and confirm all 40 records survived and parse cleanly —
        // no `Truncated` / `BadChecksum` tail.
        let j = Journal::open(&path).unwrap();
        let entries: Vec<_> = j
            .iter_entries()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .expect("no torn or checksum-failed records");
        assert_eq!(entries.len(), 40, "every alternating append must survive");
    }

    #[test]
    fn concurrent_writers_from_separate_handles_keep_journal_parseable() {
        use std::thread;

        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        // Initialize the header up front so neither thread sees an empty
        // file (header init is itself locked, but this keeps the test
        // focused on the append race).
        drop(Journal::open(&path).unwrap());

        const THREADS: u8 = 4;
        const PER_THREAD: u8 = 25;
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let path = path.clone();
            handles.push(thread::spawn(move || {
                let j = Journal::open(&path).unwrap();
                for i in 0..PER_THREAD {
                    j.append(&JournalEntry::Put {
                        key: sample_key(&format!("t{t}-{i}")),
                        entry: sample_entry(i),
                    })
                    .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let j = Journal::open(&path).unwrap();
        let entries: Vec<_> = j
            .iter_entries()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .expect("concurrent appends must not tear the journal");
        assert_eq!(
            entries.len(),
            (THREADS as usize) * (PER_THREAD as usize),
            "every concurrent append must be present exactly once"
        );
    }
}
