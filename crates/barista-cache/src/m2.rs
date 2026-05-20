// SPDX-License-Identifier: MIT OR Apache-2.0

//! Maven-compatible mirror via hardlinks.
//!
//! After the cache puts an artifact's bytes into CAS at
//! `<cache_root>/objects/<aa>/<full-hex>`, this module additionally
//! creates a hardlink at the conventional Maven path
//! `<m2_root>/<group/slashed>/<artifact>/<version>/<artifact>-<version>[-classifier].<ext>`.
//! The hardlink shares the same on-disk inode as the CAS blob, so
//! the bytes aren't duplicated. Two consequences:
//!
//! 1. mvn and other tools that walk `~/.m2/repository` find the
//!    artifact at the conventional path.
//! 2. The CAS blob's `nlink` becomes >= 2, which GC treats as a
//!    signal to keep the entry alive.
//!
//! Hardlinks require source + destination on the same filesystem.
//! If the user's `~/.m2` is on a separate volume, we silently fall
//! back to a byte copy when `link(2)` returns `EXDEV`.
//!
//! # Platform notes
//!
//! - On Unix, `std::fs::hard_link` calls `link(2)` and the
//!   `nlink`-based GC retention check works directly.
//! - On Windows, `std::fs::hard_link` calls `CreateHardLinkW`, which
//!   NTFS supports for files on the same volume. The GC's nlink
//!   check is unix-gated, so retention on Windows currently relies
//!   on other signals (the entry remains valid as long as the CAS
//!   path exists).

use std::path::{Path, PathBuf};

use barista_coords::Coords;

use crate::cas::{Cas, ContentHash};

/// Errors produced while mirroring CAS blobs into the Maven layout.
#[derive(Debug, thiserror::Error)]
pub enum MirrorError {
    /// An underlying filesystem operation failed.
    #[error("I/O error at {path:?}: {source}")]
    Io {
        /// Path the operation was acting on.
        path: PathBuf,
        /// Originating I/O error.
        source: std::io::Error,
    },
    /// The CAS blob referenced by `hash` does not exist on disk.
    #[error("CAS blob for hash {hash} not found")]
    CasMiss {
        /// Hex-encoded content hash.
        hash: String,
    },
}

/// Compose the Maven-conventional path for an artifact under
/// `m2_root`.
///
/// The group id's dots are converted to path separators; the
/// filename is `<artifact>-<version>[-<classifier>].<extension>`.
///
/// # Example
///
/// ```
/// use barista_cache::m2::m2_path;
/// use barista_coords::Coords;
/// use std::path::Path;
///
/// let coords = Coords::new("org.apache.commons", "commons-lang3").unwrap();
/// let p = m2_path(Path::new("/repo"), &coords, "3.14.0", None, "jar");
/// assert_eq!(
///     p,
///     Path::new("/repo/org/apache/commons/commons-lang3/3.14.0/commons-lang3-3.14.0.jar"),
/// );
/// ```
pub fn m2_path(
    m2_root: &Path,
    coords: &Coords,
    version: &str,
    classifier: Option<&str>,
    extension: &str,
) -> PathBuf {
    let group_slashed = coords.group.replace('.', "/");
    let filename = match classifier {
        Some(c) => format!("{}-{}-{}.{}", coords.artifact, version, c, extension),
        None => format!("{}-{}.{}", coords.artifact, version, extension),
    };
    m2_root
        .join(group_slashed)
        .join(&coords.artifact)
        .join(version)
        .join(filename)
}

/// Materialize a CAS blob at the Maven-conventional path under
/// `m2_root` as a hardlink to the CAS file.
///
/// Behavior:
///
/// - If the destination already exists and shares the same inode as
///   the CAS path, this is a no-op (idempotent).
/// - If the destination exists pointing elsewhere, it is removed and
///   recreated.
/// - Falls back to a byte copy if `link(2)` returns `EXDEV`
///   (source and destination on different filesystems).
///
/// Returns the destination path on success.
pub fn materialize(
    cas: &Cas,
    hash: &ContentHash,
    m2_root: &Path,
    coords: &Coords,
    version: &str,
    classifier: Option<&str>,
    extension: &str,
) -> Result<PathBuf, MirrorError> {
    let cas_path = cas.path_for(hash);
    if !cas_path.exists() {
        return Err(MirrorError::CasMiss {
            hash: hash.to_hex(),
        });
    }
    let dest = m2_path(m2_root, coords, version, classifier, extension);

    // Ensure parent dir exists.
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| MirrorError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    // Idempotency: if dest exists and shares the same inode, done.
    if let Ok(md) = std::fs::metadata(&dest) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(src_md) = std::fs::metadata(&cas_path) {
                if md.ino() == src_md.ino() && md.dev() == src_md.dev() {
                    return Ok(dest);
                }
            }
        }
        // Either non-unix or different inode → remove and recreate.
        let _ = std::fs::remove_file(&dest);
        // Drop the metadata handle explicitly to avoid the
        // `unused_variables` warning on non-unix platforms.
        let _ = md;
    }

    // Try hard link first.
    match std::fs::hard_link(&cas_path, &dest) {
        Ok(()) => Ok(dest),
        Err(e) => {
            if cross_device_error(&e) {
                std::fs::copy(&cas_path, &dest).map_err(|e| MirrorError::Io {
                    path: dest.clone(),
                    source: e,
                })?;
                Ok(dest)
            } else {
                Err(MirrorError::Io {
                    path: dest,
                    source: e,
                })
            }
        }
    }
}

/// `true` if the error indicates the operation crossed a filesystem
/// boundary (`EXDEV`).
fn cross_device_error(e: &std::io::Error) -> bool {
    // EXDEV = 18 on both Linux and macOS.
    e.raw_os_error() == Some(18)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::Cas;
    use tempfile::tempdir;

    fn sample_coords() -> Coords {
        Coords::new("org.apache.commons", "commons-lang3").unwrap()
    }

    fn put_blob(cas: &Cas, bytes: &[u8]) -> ContentHash {
        let (hash, _) = cas.put(bytes).expect("put");
        hash
    }

    #[test]
    fn m2_path_composes_standard_jar_layout() {
        let p = m2_path(Path::new("/repo"), &sample_coords(), "3.14.0", None, "jar");
        assert_eq!(
            p,
            Path::new("/repo/org/apache/commons/commons-lang3/3.14.0/commons-lang3-3.14.0.jar"),
        );
    }

    #[test]
    fn m2_path_composes_with_classifier() {
        let coords = Coords::new("g", "a").unwrap();
        let p = m2_path(Path::new("/r"), &coords, "v", Some("sources"), "jar");
        assert_eq!(p, Path::new("/r/g/a/v/a-v-sources.jar"));
    }

    #[test]
    fn materialize_creates_hardlink_and_parent_dirs() {
        let cas_root = tempdir().unwrap();
        let m2_root = tempdir().unwrap();
        let cas = Cas::open(cas_root.path()).unwrap();
        let hash = put_blob(&cas, b"hello-world");

        let dest = materialize(
            &cas,
            &hash,
            m2_root.path(),
            &sample_coords(),
            "3.14.0",
            None,
            "jar",
        )
        .unwrap();

        assert!(dest.is_file(), "destination file should exist");
        assert!(
            dest.parent().unwrap().is_dir(),
            "parent dir should be created"
        );
        let bytes = std::fs::read(&dest).unwrap();
        assert_eq!(bytes, b"hello-world");
    }

    #[test]
    fn materialize_is_idempotent() {
        let cas_root = tempdir().unwrap();
        let m2_root = tempdir().unwrap();
        let cas = Cas::open(cas_root.path()).unwrap();
        let hash = put_blob(&cas, b"abc");

        let dest1 = materialize(
            &cas,
            &hash,
            m2_root.path(),
            &sample_coords(),
            "1.0.0",
            None,
            "jar",
        )
        .unwrap();
        let dest2 = materialize(
            &cas,
            &hash,
            m2_root.path(),
            &sample_coords(),
            "1.0.0",
            None,
            "jar",
        )
        .unwrap();
        assert_eq!(dest1, dest2);
        assert!(dest2.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn materialized_inode_matches_cas() {
        use std::os::unix::fs::MetadataExt;

        let cas_root = tempdir().unwrap();
        let m2_root = tempdir().unwrap();
        let cas = Cas::open(cas_root.path()).unwrap();
        let hash = put_blob(&cas, b"inode-check");

        let dest = materialize(
            &cas,
            &hash,
            m2_root.path(),
            &sample_coords(),
            "2.0",
            None,
            "jar",
        )
        .unwrap();

        let src_md = std::fs::metadata(cas.path_for(&hash)).unwrap();
        let dst_md = std::fs::metadata(&dest).unwrap();
        assert_eq!(src_md.ino(), dst_md.ino());
        assert_eq!(src_md.dev(), dst_md.dev());
    }

    #[test]
    fn materialize_missing_cas_blob_returns_cas_miss() {
        let cas_root = tempdir().unwrap();
        let m2_root = tempdir().unwrap();
        let cas = Cas::open(cas_root.path()).unwrap();
        // Hash that points at no real blob.
        let bogus = ContentHash::from_hex(&"a".repeat(64)).unwrap();

        let err = materialize(
            &cas,
            &bogus,
            m2_root.path(),
            &sample_coords(),
            "1.0",
            None,
            "jar",
        )
        .unwrap_err();
        match err {
            MirrorError::CasMiss { hash } => {
                assert_eq!(hash, "a".repeat(64));
            }
            other => panic!("expected CasMiss, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn cas_blob_nlink_increases_after_materialize() {
        use std::os::unix::fs::MetadataExt;

        let cas_root = tempdir().unwrap();
        let m2_root = tempdir().unwrap();
        let cas = Cas::open(cas_root.path()).unwrap();
        let hash = put_blob(&cas, b"nlink-bytes");

        let before = std::fs::metadata(cas.path_for(&hash)).unwrap().nlink();
        let _ = materialize(
            &cas,
            &hash,
            m2_root.path(),
            &sample_coords(),
            "3.0",
            None,
            "jar",
        )
        .unwrap();
        let after = std::fs::metadata(cas.path_for(&hash)).unwrap().nlink();
        assert!(after >= 2, "nlink should be at least 2, got {after}");
        assert!(
            after > before,
            "nlink should have grown (before={before}, after={after})"
        );
    }

    #[test]
    fn removing_m2_link_does_not_destroy_cas_blob() {
        let cas_root = tempdir().unwrap();
        let m2_root = tempdir().unwrap();
        let cas = Cas::open(cas_root.path()).unwrap();
        let hash = put_blob(&cas, b"survive-removal");
        let cas_path = cas.path_for(&hash);

        let dest = materialize(
            &cas,
            &hash,
            m2_root.path(),
            &sample_coords(),
            "1.2.3",
            None,
            "jar",
        )
        .unwrap();
        std::fs::remove_file(&dest).unwrap();

        assert!(cas_path.exists(), "CAS blob should outlive the m2 link");
        let bytes = std::fs::read(&cas_path).unwrap();
        assert_eq!(bytes, b"survive-removal");
    }

    #[test]
    fn rematerialize_after_m2_delete_recreates_link() {
        let cas_root = tempdir().unwrap();
        let m2_root = tempdir().unwrap();
        let cas = Cas::open(cas_root.path()).unwrap();
        let hash = put_blob(&cas, b"recreate-me");

        let dest = materialize(
            &cas,
            &hash,
            m2_root.path(),
            &sample_coords(),
            "0.0.1",
            None,
            "jar",
        )
        .unwrap();
        std::fs::remove_file(&dest).unwrap();
        assert!(!dest.exists());

        let dest2 = materialize(
            &cas,
            &hash,
            m2_root.path(),
            &sample_coords(),
            "0.0.1",
            None,
            "jar",
        )
        .unwrap();
        assert_eq!(dest, dest2);
        assert!(dest2.is_file());
        let bytes = std::fs::read(&dest2).unwrap();
        assert_eq!(bytes, b"recreate-me");
    }

    #[test]
    fn distinct_coords_share_cas_blob_at_different_m2_paths() {
        let cas_root = tempdir().unwrap();
        let m2_root = tempdir().unwrap();
        let cas = Cas::open(cas_root.path()).unwrap();
        // Same bytes → same hash → single CAS blob.
        let hash = put_blob(&cas, b"shared-bytes");

        let coords_a = Coords::new("com.example", "lib-a").unwrap();
        let coords_b = Coords::new("org.other", "lib-b").unwrap();

        let dest_a =
            materialize(&cas, &hash, m2_root.path(), &coords_a, "1.0", None, "jar").unwrap();
        let dest_b =
            materialize(&cas, &hash, m2_root.path(), &coords_b, "2.0", None, "jar").unwrap();

        assert_ne!(dest_a, dest_b, "m2 paths should differ per coord");
        assert!(dest_a.is_file());
        assert!(dest_b.is_file());

        // Both files share content with the CAS blob.
        let cas_bytes = std::fs::read(cas.path_for(&hash)).unwrap();
        assert_eq!(std::fs::read(&dest_a).unwrap(), cas_bytes);
        assert_eq!(std::fs::read(&dest_b).unwrap(), cas_bytes);

        // On unix, both should be hardlinks to the same inode.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let cas_md = std::fs::metadata(cas.path_for(&hash)).unwrap();
            let md_a = std::fs::metadata(&dest_a).unwrap();
            let md_b = std::fs::metadata(&dest_b).unwrap();
            assert_eq!(md_a.ino(), cas_md.ino());
            assert_eq!(md_b.ino(), cas_md.ino());
            assert!(cas_md.nlink() >= 3);
        }
    }

    #[test]
    fn m2_path_with_pom_extension() {
        let p = m2_path(Path::new("/repo"), &sample_coords(), "3.14.0", None, "pom");
        assert_eq!(
            p,
            Path::new("/repo/org/apache/commons/commons-lang3/3.14.0/commons-lang3-3.14.0.pom"),
        );
    }
}
