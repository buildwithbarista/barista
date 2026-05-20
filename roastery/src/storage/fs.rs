// SPDX-License-Identifier: MIT OR Apache-2.0

//! Filesystem-backed content-addressed storage.
//!
//! ## On-disk layout
//!
//! ```text
//! <root>/
//! ├── cas/
//! │   ├── ab/
//! │   │   └── cdef0123…   ← 62 hex chars; full digest = "ab" + filename
//! │   └── …
//! └── tmp/
//!     └── <random>.tmp    ← in-flight `put` staging files
//! ```
//!
//! Splitting the digest into a 2-char prefix directory keeps any one
//! directory under ~256² ≈ 65 000 entries even for a fully populated
//! 16-bit fanout — well within the comfort zone for ext4, APFS, NTFS,
//! and ZFS dirent listings. The convention matches what git's loose
//! object store, bazel-remote, and buildbarn do; rolling our own
//! would be gratuitously different for no win.
//!
//! ## Atomic write protocol
//!
//! `put` writes to `<root>/tmp/<random>.tmp` and hashes bytes with a
//! running `sha2::Sha256` as they flow through. On a successful
//! digest match the staging file is `persist`-ed (POSIX `rename`)
//! into `<root>/cas/<2>/<62>`. Same-filesystem renames are atomic on
//! Linux, macOS, and Windows (within a single volume), so any
//! concurrent `get` either sees no file or a fully complete one —
//! never a half-written blob.
//!
//! If the caller's claimed digest disagrees with the bytes' actual
//! digest, the [`tempfile::NamedTempFile`]'s `Drop` deletes the
//! staging file; the store never gains a poisoned entry.
//!
//! If the target path already exists (digest collision = same blob
//! by definition under SHA-256), the existing entry is kept and the
//! staging file is dropped. The put returns success.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::StorageError;
use crate::storage::{Cas, CasReader, Digest, Result, Stat};

/// Hard cap on the number of digests `list` returns in a single call.
///
/// v0.1 simplicity choice: `list` is for tests + admin tooling, not
/// the hot serving path. v0.2 will replace this with a paginated
/// cursor API once GC and admin endpoints actually need it.
pub const LIST_CAP: usize = 10_000;

/// Buffer size used when streaming bytes from `source` into the
/// staging file. 64 KiB is a good fit for ext4 / APFS block sizes
/// while staying small enough to keep stack pressure negligible.
const PUT_BUF_LEN: usize = 64 * 1024;

/// Filesystem-backed [`Cas`] implementation.
#[derive(Debug, Clone)]
pub struct FsCas {
    /// Root directory. The CAS proper lives under `<root>/cas/`,
    /// staging files under `<root>/tmp/`.
    root: PathBuf,
}

impl FsCas {
    /// Open or initialise a filesystem CAS rooted at `root`.
    ///
    /// Creates `<root>/cas/` and `<root>/tmp/` if they don't already
    /// exist. If `<root>/tmp/` already contains files from a crashed
    /// previous run, they are left alone — `tempfile`'s `Drop` would
    /// have removed them in the normal exit path, so anything still
    /// present is an artefact of a hard crash and is harmless to
    /// leave for an admin to inspect.
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(root.join("cas"))?;
        std::fs::create_dir_all(root.join("tmp"))?;
        Ok(Self { root })
    }

    /// Return the root directory this backend is anchored at.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Build the on-disk path for a digest: `<root>/cas/<2>/<62>`.
    fn path_for(&self, digest: Digest) -> PathBuf {
        let hex = digest.to_hex();
        // `to_hex` always returns 64 lowercase chars, so slicing at
        // byte index 2 is safe (ASCII-only string).
        let (prefix, rest) = hex.split_at(2);
        self.root.join("cas").join(prefix).join(rest)
    }

    /// Tmp staging directory.
    fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }
}

#[async_trait]
impl Cas for FsCas {
    async fn stat(&self, digest: Digest) -> Result<Option<Stat>> {
        let path = self.path_for(digest);
        match fs::metadata(&path).await {
            Ok(meta) => Ok(Some(Stat {
                size: meta.len(),
                digest,
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    async fn get(&self, digest: Digest) -> Result<Option<CasReader>> {
        let path = self.path_for(digest);
        match fs::File::open(&path).await {
            Ok(f) => {
                // Coerce `Box<File>` to `Box<dyn AsyncRead + Send + Unpin>`
                // via the `let _: CasReader = …` binding rather than an
                // `as` cast — the workspace lint policy forbids the
                // latter.
                let reader: CasReader = Box::new(f);
                Ok(Some(reader))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    async fn put(
        &self,
        expected_digest: Digest,
        mut source: CasReader,
    ) -> Result<Stat> {
        // Fast-path: if the digest is already present, skip the work.
        // We still read the source to EOF to honour the streaming
        // contract (callers may be tee'ing data through us), but we
        // do it after a cheap existence check so the common
        // "already-cached" case is a one-syscall stat plus a drain.
        let target = self.path_for(expected_digest);
        if fs::metadata(&target).await.is_ok() {
            // Drain the source to give the caller a uniform "I read
            // your whole stream" contract. Errors here propagate.
            let mut sink = tokio::io::sink();
            tokio::io::copy(&mut source, &mut sink)
                .await
                .map_err(StorageError::Io)?;
            let meta = fs::metadata(&target).await.map_err(StorageError::Io)?;
            return Ok(Stat {
                size: meta.len(),
                digest: expected_digest,
            });
        }

        // Ensure the 2-char prefix dir exists before we attempt the
        // rename — the parent of the final path must be present.
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await.map_err(StorageError::Io)?;
        }

        // Stage to a tempfile in `<root>/tmp/`. Same filesystem as
        // the final destination so the rename is atomic.
        let tmp_dir = self.tmp_dir();
        // `NamedTempFile::new_in` is blocking I/O; run it on a blocking
        // worker so we don't stall the runtime.
        let staging = tokio::task::spawn_blocking({
            let tmp_dir = tmp_dir.clone();
            move || NamedTempFile::new_in(&tmp_dir)
        })
        .await
        .map_err(|e| StorageError::Other {
            context: format!("spawn_blocking for tempfile creation failed: {e}"),
        })?
        .map_err(StorageError::Io)?;

        // Wrap the std::fs::File the tempfile owns in a tokio handle
        // for async writes. We use `try_clone` so the tempfile keeps
        // its own handle for `persist`.
        let staging_path = staging.path().to_path_buf();
        let std_file = staging.as_file().try_clone().map_err(StorageError::Io)?;
        let mut writer = fs::File::from_std(std_file);

        // Stream bytes: source → hasher + writer.
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; PUT_BUF_LEN];
        let mut total: u64 = 0;
        loop {
            let n = source.read(&mut buf).await.map_err(StorageError::Io)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            writer.write_all(&buf[..n]).await.map_err(StorageError::Io)?;
            // Track total bytes. `usize → u64` is widening on every
            // target Barista supports (32-bit and 64-bit), so the
            // conversion can't fail; we still go through `try_from`
            // to satisfy the workspace's `as_conversions` lint.
            let n_u64 = u64::try_from(n).map_err(|_| StorageError::Other {
                context: "read returned a byte count that doesn't fit in u64".to_string(),
            })?;
            total = total.saturating_add(n_u64);
        }
        writer.flush().await.map_err(StorageError::Io)?;
        // Drop the async writer so the underlying fd is closed before
        // we attempt the rename. The `NamedTempFile` keeps its own
        // handle alive via the clone.
        drop(writer);

        let digest_bytes = hasher.finalize();
        let mut actual = [0u8; 32];
        actual.copy_from_slice(&digest_bytes);
        let actual = Digest::from_bytes(actual);

        if actual != expected_digest {
            // `staging` drops here → tempfile is deleted. No partial
            // blob is left behind under <root>/cas/ or <root>/tmp/.
            return Err(StorageError::DigestMismatch {
                expected: expected_digest,
                actual,
            });
        }

        // Persist (rename) the staging file into its final home.
        // `tempfile::NamedTempFile::persist` consumes the tempfile so
        // its Drop won't fire on the now-renamed inode.
        let target_for_blocking = target.clone();
        let persist_result = tokio::task::spawn_blocking(move || {
            staging.persist(&target_for_blocking)
        })
        .await
        .map_err(|e| StorageError::Other {
            context: format!("spawn_blocking for tempfile persist failed: {e}"),
        })?;

        match persist_result {
            Ok(_) => Ok(Stat {
                size: total,
                digest: expected_digest,
            }),
            Err(persist_err) => {
                // Two cases:
                //
                // 1. Another writer beat us to it — the target now
                //    exists. Under SHA-256 the bytes are identical by
                //    definition, so this is a successful no-op.
                // 2. A real I/O error. The tempfile is dropped by
                //    `persist_err` going out of scope → cleaned up.
                if persist_err.error.kind() == std::io::ErrorKind::AlreadyExists
                    || fs::metadata(&target).await.is_ok()
                {
                    return Ok(Stat {
                        size: total,
                        digest: expected_digest,
                    });
                }
                // Make sure the staging path is gone before we return.
                // `persist` may have left the file at its original
                // location on some error kinds; the tempfile's Drop
                // handles the common case but a belt-and-braces sweep
                // is cheap.
                let _ = fs::remove_file(&staging_path).await;
                Err(StorageError::Io(persist_err.error))
            }
        }
    }

    async fn delete(&self, digest: Digest) -> Result<bool> {
        let path = self.path_for(digest);
        match fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<Digest>> {
        // Validate the prefix up front so we can fail fast on garbage.
        if let Some(p) = prefix {
            if p.len() > Digest::HEX_LEN {
                return Err(StorageError::InvalidDigest {
                    reason: format!(
                        "list prefix is longer than a digest ({} > {})",
                        p.len(),
                        Digest::HEX_LEN
                    ),
                });
            }
            if !p.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return Err(StorageError::InvalidDigest {
                    reason: "list prefix must be lowercase hex [0-9a-f]".to_string(),
                });
            }
        }

        let cas_root = self.root.join("cas");
        let mut out = Vec::new();

        // Walk the 2-char prefix directories. We could open them all
        // and stream entries, but two sequential reads keep the code
        // simple and the I/O pattern is fine for the v0.1 use case
        // (admin tooling + tests).
        let mut outer = match fs::read_dir(&cas_root).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(StorageError::Io(e)),
        };

        while let Some(prefix_entry) = outer.next_entry().await.map_err(StorageError::Io)? {
            let prefix_name = match prefix_entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue, // non-UTF8 dirent: not ours, skip.
            };
            if prefix_name.len() != 2
                || !prefix_name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            {
                continue; // not a CAS prefix dir.
            }

            // Apply the caller's prefix filter at the dir level when
            // we can — if the filter is >=2 chars, it must match the
            // prefix dir name; otherwise skip this whole subtree.
            if let Some(p) = prefix {
                if p.len() >= 2 && !prefix_name.starts_with(&p[..2]) {
                    continue;
                }
                if p.len() == 1 && !prefix_name.starts_with(p) {
                    continue;
                }
            }

            let mut inner = match fs::read_dir(prefix_entry.path()).await {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(StorageError::Io(e)),
            };

            while let Some(file_entry) = inner.next_entry().await.map_err(StorageError::Io)? {
                let rest = match file_entry.file_name().into_string() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if rest.len() != Digest::HEX_LEN - 2 {
                    continue;
                }
                let hex = format!("{prefix_name}{rest}");
                if let Some(p) = prefix {
                    if !hex.starts_with(p) {
                        continue;
                    }
                }
                let digest = Digest::from_hex(&hex)?;
                out.push(digest);
                if out.len() >= LIST_CAP {
                    return Ok(out);
                }
            }
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::as_conversions
    )]

    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;
    use tokio::io::{AsyncRead, AsyncReadExt};

    fn fixture() -> (TempDir, FsCas) {
        let tmp = TempDir::new().unwrap();
        let cas = FsCas::new(tmp.path().to_path_buf()).unwrap();
        (tmp, cas)
    }

    fn cursor(bytes: &'static [u8]) -> CasReader {
        Box::new(Cursor::new(bytes)) as CasReader
    }

    #[tokio::test]
    async fn put_get_stat_round_trip() {
        let (_tmp, cas) = fixture();
        let blob: &'static [u8] = b"hello, roastery";
        let digest = Digest::of_bytes(blob);

        let stat = cas.put(digest, cursor(blob)).await.unwrap();
        assert_eq!(stat.size, blob.len() as u64);
        assert_eq!(stat.digest, digest);

        let stat2 = cas.stat(digest).await.unwrap().expect("present");
        assert_eq!(stat2, stat);

        let mut reader = cas.get(digest).await.unwrap().expect("present");
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, blob);
    }

    #[tokio::test]
    async fn stat_and_get_return_none_for_missing() {
        let (_tmp, cas) = fixture();
        let digest = Digest::of_bytes(b"never written");
        assert!(cas.stat(digest).await.unwrap().is_none());
        assert!(cas.get(digest).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_rejects_digest_mismatch() {
        let (tmp, cas) = fixture();
        let blob: &'static [u8] = b"the real bytes";
        // Claim a digest that does NOT match `blob`.
        let bogus = Digest::of_bytes(b"some other bytes");

        let err = cas.put(bogus, cursor(blob)).await.unwrap_err();
        match err {
            StorageError::DigestMismatch { expected, actual } => {
                assert_eq!(expected, bogus);
                assert_eq!(actual, Digest::of_bytes(blob));
            }
            other => panic!("expected DigestMismatch, got {other:?}"),
        }

        // No entry under the claimed digest's path…
        let bogus_path = cas.path_for(bogus);
        assert!(!bogus_path.exists());
        // …and no leftover staging file under <root>/tmp/.
        let mut tmp_entries = std::fs::read_dir(tmp.path().join("tmp")).unwrap();
        assert!(
            tmp_entries.next().is_none(),
            "tmp dir not empty after mismatch"
        );
    }

    #[tokio::test]
    async fn put_is_atomic_under_concurrency() {
        let (_tmp, cas) = fixture();
        let blob: &'static [u8] = b"contended blob";
        let digest = Digest::of_bytes(blob);

        let mut handles = Vec::new();
        for _ in 0..16 {
            let cas = cas.clone();
            handles.push(tokio::spawn(async move {
                cas.put(digest, cursor(blob)).await
            }));
        }
        for h in handles {
            let stat = h.await.unwrap().unwrap();
            assert_eq!(stat.digest, digest);
            assert_eq!(stat.size, blob.len() as u64);
        }

        // Exactly one final file on disk.
        let listed = cas.list(None).await.unwrap();
        assert_eq!(listed, vec![digest]);

        // And the bytes are intact.
        let mut reader = cas.get(digest).await.unwrap().expect("present");
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, blob);
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let (_tmp, cas) = fixture();
        let blob: &'static [u8] = b"delete me";
        let digest = Digest::of_bytes(blob);
        cas.put(digest, cursor(blob)).await.unwrap();

        assert!(cas.delete(digest).await.unwrap());
        assert!(!cas.delete(digest).await.unwrap());
        assert!(cas.stat(digest).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_returns_all_present_digests_and_filters_by_prefix() {
        let (_tmp, cas) = fixture();

        // Put 10 unique blobs. We hold the digests in a Vec so we can
        // compare against `list(None)`.
        let mut expected = Vec::new();
        for i in 0..10u32 {
            let blob = format!("blob-{i}").into_bytes();
            let digest = Digest::of_bytes(&blob);
            cas.put(digest, Box::new(Cursor::new(blob)) as CasReader)
                .await
                .unwrap();
            expected.push(digest);
        }

        let mut all = cas.list(None).await.unwrap();
        all.sort_by_key(|d| d.to_hex());
        expected.sort_by_key(|d| d.to_hex());
        assert_eq!(all, expected);

        // Filter by the 2-char prefix of the first digest.
        let target = expected[0];
        let hex = target.to_hex();
        let two = &hex[..2];
        let filtered = cas.list(Some(two)).await.unwrap();
        assert!(filtered.contains(&target));
        for d in &filtered {
            assert!(d.to_hex().starts_with(two));
        }

        // A single-char prefix also filters correctly.
        let one = &hex[..1];
        let filtered_one = cas.list(Some(one)).await.unwrap();
        assert!(filtered_one.contains(&target));
        for d in &filtered_one {
            assert!(d.to_hex().starts_with(one));
        }

        // Garbage prefix is rejected.
        let err = cas.list(Some("ZZ")).await.unwrap_err();
        assert!(matches!(err, StorageError::InvalidDigest { .. }));
    }

    #[tokio::test]
    async fn handles_large_blob_without_oom() {
        let (_tmp, cas) = fixture();

        // 16 MiB streamed source. We deliberately don't buffer the
        // whole thing in the test — we generate it on the fly via a
        // `RepeatReader` so the streaming codepath is what's
        // exercised.
        const SIZE: usize = 16 * 1024 * 1024;
        const BYTE: u8 = 0xA5;

        // Pre-hash the synthetic stream so we can pass the right
        // expected digest. Computing the hash takes one pass; the
        // `put` will do another over the streaming source.
        let mut hasher = Sha256::new();
        // Update in 64 KiB chunks to avoid allocating a 16 MiB slice
        // for the hashing pass.
        let chunk = vec![BYTE; 64 * 1024];
        let chunks = SIZE / chunk.len();
        for _ in 0..chunks {
            hasher.update(&chunk);
        }
        let mut digest_bytes = [0u8; 32];
        digest_bytes.copy_from_slice(&hasher.finalize());
        let digest = Digest::from_bytes(digest_bytes);

        let source: CasReader = Box::new(RepeatReader::new(BYTE, SIZE));
        let stat = cas.put(digest, source).await.unwrap();
        assert_eq!(stat.size, SIZE as u64);
        assert_eq!(stat.digest, digest);

        // Read it back and verify byte-by-byte (in 64 KiB chunks to
        // keep the verifier itself streaming).
        let mut reader = cas.get(digest).await.unwrap().expect("present");
        let mut total = 0usize;
        let mut verify = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut verify).await.unwrap();
            if n == 0 {
                break;
            }
            assert!(verify[..n].iter().all(|&b| b == BYTE));
            total += n;
        }
        assert_eq!(total, SIZE);
    }

    /// Streaming reader that emits `byte` for exactly `len` bytes
    /// without buffering the whole stream up front. Used by the
    /// large-blob test to keep memory pressure proportional to the
    /// per-read buffer, not the blob size.
    struct RepeatReader {
        byte: u8,
        remaining: usize,
    }

    impl RepeatReader {
        fn new(byte: u8, len: usize) -> Self {
            Self { byte, remaining: len }
        }
    }

    impl AsyncRead for RepeatReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if self.remaining == 0 {
                return std::task::Poll::Ready(Ok(()));
            }
            let n = self.remaining.min(buf.remaining());
            // Fill `n` bytes with `self.byte`.
            let pos_before = buf.filled().len();
            buf.initialize_unfilled_to(n);
            let dst = &mut buf.initialized_mut()[pos_before..pos_before + n];
            dst.fill(self.byte);
            buf.advance(n);
            self.remaining -= n;
            std::task::Poll::Ready(Ok(()))
        }
    }
}
