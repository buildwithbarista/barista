//! Owner-only Unix-domain socket paths with policy enforcement.
//!
//! [`SocketPath`] is a newtype around an absolute path that has been
//! validated to live under a `0700` per-user directory and that, once
//! a socket inode exists at the path, has mode bits exactly `0600`
//! and is owned by the current effective UID.
//!
//! Construction (`SocketPath::new`) creates the parent directory at
//! `0700` if it doesn't exist; it does NOT create the socket inode.
//! `UnixListener::bind()` is the caller's job; `bind_secure()` in
//! the transport module wraps that step plus the follow-up
//! `chmod(2)` to `0600`.
//!
//! See [`crate::auth`] for the full security model.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::{AuthError, Result};

/// Mode bits required on the socket file.
///
/// Owner read+write, no group, no world. The kernel won't let any
/// non-owner UID `connect(2)` to a socket inode without read access,
/// so `0600` is exactly what we want (no `x` bit — sockets don't
/// have an "execute" semantic).
pub const SOCKET_MODE: u32 = 0o600;

/// Mode bits required on the parent run directory.
///
/// Owner traverse+read+write, nothing for group or world. An
/// unprivileged peer cannot even `opendir(3)` the directory, let
/// alone `stat(2)` an inode inside it. This closes the TOCTOU
/// window between `bind(2)` and the follow-up `chmod(2)` on the
/// socket itself.
pub const RUN_DIR_MODE: u32 = 0o700;

/// Default sub-directory under the user's home for transient runtime
/// state. Matches PRD §12 ("UDS at `~/.barista/run/`").
///
/// Kept as `&'static str` rather than baking a full path in: the
/// home directory is resolved at runtime via `dirs::home_dir()` and
/// the two components are joined cross-platform-safely.
const DEFAULT_RUN_DIR: &str = ".barista/run";

/// A vetted, ready-to-bind Unix-domain socket path.
///
/// `Debug` prints the path verbatim — it's not a secret (the file's
/// existence is observable to any user with execute on the parent
/// directory anyway).
#[derive(Debug, Clone)]
pub struct SocketPath {
    path: PathBuf,
}

impl SocketPath {
    /// Construct a socket path under the default `~/.barista/run/`
    /// directory.
    ///
    /// `name` is the leaf component (without extension); the full
    /// path becomes `<home>/.barista/run/<name>.sock`.
    ///
    /// Side-effects:
    ///
    /// * If `~/.barista/run/` doesn't exist, it is created `mkdir -p`
    ///   with mode `0700`.
    /// * If `~/.barista/run/` exists but has different mode bits,
    ///   they are tightened to `0700` (an admin reset to `755`
    ///   shouldn't break the security model).
    ///
    /// Returns:
    ///
    /// * [`AuthError::SocketDirCreateFailed`] if `mkdir`/`chmod`
    ///   fail.
    /// * [`AuthError::Io`] for other I/O during home-dir resolution.
    pub fn new(name: &str) -> Result<Self> {
        // `dirs::home_dir()` returns `None` if the platform has no
        // home concept; on Unix that means `$HOME` is unset AND no
        // entry in `/etc/passwd`. We surface a typed error in that
        // case rather than panicking.
        let home = dirs::home_dir().ok_or_else(|| {
            AuthError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "could not resolve home directory; set HOME or run as a user with a passwd entry",
            ))
        })?;
        let base = home.join(DEFAULT_RUN_DIR);
        Self::new_in(&base, name)
    }

    /// Construct a socket path under an explicit base directory.
    ///
    /// Used by tests (so they can point at a `tempfile::TempDir`)
    /// and by callers who want a non-default location (e.g.
    /// `XDG_RUNTIME_DIR`-style integrations).
    ///
    /// Same side-effects as [`Self::new`]: the base directory is
    /// created `0700` if absent and tightened to `0700` if it
    /// exists with looser perms.
    pub fn new_in(base_dir: &Path, name: &str) -> Result<Self> {
        ensure_run_dir(base_dir)?;
        let mut path = base_dir.to_path_buf();
        // We append `<name>.sock` rather than letting the caller pass
        // an arbitrary suffix — keeps every barista-managed socket
        // discoverable by extension. macOS' 104-char `sun_path` limit
        // is the caller's responsibility (tests use `dir.join("s")`
        // to stay short).
        path.push(format!("{name}.sock"));
        Ok(Self { path })
    }

    /// The fully-resolved path the socket lives at.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// The parent directory the socket lives in.
    ///
    /// Useful for callers who want to verify the directory's perms
    /// themselves (e.g. defense-in-depth before binding).
    ///
    /// Returns `None` only if the construction invariants have been
    /// broken (e.g. someone constructed `SocketPath` from a single
    /// path component) — [`Self::new`] / [`Self::new_in`] always
    /// join a base dir + a non-empty leaf, so a parent always
    /// exists in practice. Callers built on the standard
    /// constructors can `.unwrap_or` against a sensible default.
    #[must_use]
    pub fn parent(&self) -> Option<&Path> {
        // Return `Option` rather than panicking on the broken-invariant
        // path: the workspace's `clippy::expect_used = "warn"` policy
        // wants graceful handling, and the cost to the caller is one
        // `unwrap_or` (only test code calls `parent()` today).
        self.path.parent()
    }

    /// Verify the socket inode at this path meets the policy.
    ///
    /// Runs three checks:
    ///
    /// 1. The path exists and is a `S_IFSOCK` inode. (Symlinks are
    ///    followed by `metadata`; if the target isn't a socket we
    ///    reject with [`AuthError::NotASocket`].)
    /// 2. The owner UID matches the current `geteuid()`. Mismatch
    ///    surfaces as [`AuthError::SocketOwnerWrong`].
    /// 3. The mode bits, masked to `0o7777`, are exactly `0o600`.
    ///    Anything else surfaces as [`AuthError::SocketPermsWrong`].
    ///
    /// Returns `Ok(())` if all three pass; the caller may then
    /// `UnixStream::connect(socket_path.as_path())` knowing the
    /// kernel will accept the connect.
    ///
    /// **Race-conditions:** there's an inherent TOCTOU between this
    /// `stat(2)` and the subsequent `connect(2)`. We mitigate by:
    ///
    /// * Keeping the parent directory `0700` so an unprivileged
    ///   attacker can't even `lstat` the socket, let alone
    ///   replace it.
    /// * Running a follow-up `SO_PEERCRED` check on the connected
    ///   stream (see [`super::peer_cred::verify_peer_uid`]). If
    ///   someone *did* swap the inode under us, the kernel-level
    ///   peer-cred oracle reports a different UID and we close the
    ///   connection.
    pub fn verify(&self) -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        let meta = std::fs::metadata(&self.path)?;
        let file_type = meta.file_type();

        // `FileType::is_socket` is in `std::os::unix::fs::FileTypeExt`;
        // we keep the import local to avoid a top-of-file `use` that
        // would be unused on non-Unix `cargo doc` runs.
        use std::os::unix::fs::FileTypeExt;
        if !file_type.is_socket() {
            return Err(AuthError::NotASocket {
                path: self.path.display().to_string(),
            });
        }

        // `Metadata::uid` returns the owning UID directly; no need to
        // call `stat(2)` ourselves.
        let actual_uid = meta.uid();
        // `geteuid` is `libc`'s safe wrapper (no errno path).
        // SAFETY: `geteuid` is documented to never fail and never
        // touches errno; the cast is workspace-wide allowed only via
        // an `#[allow]` on the call site (clippy::as_conversions).
        #[allow(clippy::as_conversions)]
        let our_uid = unsafe { libc::geteuid() } as u32;
        if actual_uid != our_uid {
            return Err(AuthError::SocketOwnerWrong {
                actual_uid,
                expected_uid: our_uid,
            });
        }

        // Mask to the low 12 bits (suid/sgid/sticky + rwxrwxrwx)
        // for the comparison. POSIX `stat.st_mode` includes the
        // file-type bits in the high nibble; we don't care about
        // those for the policy check.
        let actual_mode = meta.permissions().mode() & 0o7777;
        if actual_mode != SOCKET_MODE {
            return Err(AuthError::SocketPermsWrong {
                actual_mode,
                expected: SOCKET_MODE,
            });
        }

        Ok(())
    }

    /// Tighten the mode bits on the socket inode to `0600`.
    ///
    /// Called immediately after `UnixListener::bind` by the server-
    /// side path in `bind_secure`. The path must already exist.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Io`] if `chmod(2)` fails (e.g. the
    /// caller doesn't own the inode — should be impossible in the
    /// post-bind path but typed for safety).
    pub fn chmod_to_policy(&self) -> Result<()> {
        let perms = std::fs::Permissions::from_mode(SOCKET_MODE);
        std::fs::set_permissions(&self.path, perms)?;
        Ok(())
    }

    /// Best-effort cleanup: unlink the socket inode if it exists.
    ///
    /// Called by tests (so successive runs don't trip "address in
    /// use" on `bind`). The server should also call this before
    /// `bind_secure` to clear any stale socket from a previous
    /// crash. Errors are converted to `io::Error` and propagated;
    /// callers may ignore `NotFound` if a missing socket is
    /// expected.
    pub fn unlink_if_exists(&self) -> io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Idempotently create `dir` with mode `0700`, tightening if it
/// already exists with looser perms.
fn ensure_run_dir(dir: &Path) -> Result<()> {
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {
            // Tighten if necessary. Note we do not relax: if the
            // user has hardened to `0500` they presumably know
            // what they're doing.
            let cur = meta.permissions().mode() & 0o7777;
            // We require the owner-only bits; tolerate `0500`,
            // `0700`. Reject anything that grants group or world
            // access.
            if cur & 0o077 != 0 {
                let perms = std::fs::Permissions::from_mode(RUN_DIR_MODE);
                std::fs::set_permissions(dir, perms)
                    .map_err(AuthError::SocketDirCreateFailed)?;
            }
            Ok(())
        }
        Ok(_) => Err(AuthError::SocketDirCreateFailed(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "run directory path exists but is not a directory",
        ))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // `create_dir_all` sets perms to the process umask
            // first; we have to follow up with an explicit
            // `set_permissions` to force `0700` regardless of
            // umask. Doing the chmod on the leaf only is enough —
            // intermediate components (`~/.barista`) inherit the
            // user's existing perms, which is fine.
            std::fs::create_dir_all(dir).map_err(AuthError::SocketDirCreateFailed)?;
            let perms = std::fs::Permissions::from_mode(RUN_DIR_MODE);
            std::fs::set_permissions(dir, perms).map_err(AuthError::SocketDirCreateFailed)?;
            Ok(())
        }
        Err(e) => Err(AuthError::SocketDirCreateFailed(e)),
    }
}

// Suppress the unused-import warning on platforms where `CString` /
// `OsStrExt` aren't reached by any non-test code path. We keep the
// imports for forward-compat (future callers may need to drop down
// to `libc::stat`) and document the rationale here so a future
// pedantic-lint sweep doesn't strip them.
#[allow(dead_code)]
fn _imports_kept_alive() -> (Option<CString>, Option<&'static [u8]>) {
    (
        CString::new("/").ok(),
        Some(std::ffi::OsStr::new("").as_bytes()),
    )
}

// ---------------------------------------------------------------------------
// Unit tests for the SocketPath newtype.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::as_conversions
    )]

    use super::*;
    use tempfile::TempDir;

    #[test]
    fn new_in_creates_run_dir_with_0700() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("run");
        assert!(!base.exists());

        let _sp = SocketPath::new_in(&base, "s").expect("new_in should succeed");

        assert!(base.is_dir(), "run dir should exist");
        let mode = std::fs::metadata(&base).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o700, "run dir mode should be 0700, got {mode:#o}");
    }

    #[test]
    fn new_in_tightens_loose_run_dir() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("run");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _sp = SocketPath::new_in(&base, "s").expect("new_in should succeed");

        let mode = std::fs::metadata(&base).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            mode, 0o700,
            "new_in should tighten 0755 → 0700; got {mode:#o}"
        );
    }

    #[test]
    fn new_in_accepts_already_0700_dir() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("run");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();

        let sp = SocketPath::new_in(&base, "s").expect("new_in should succeed");

        let mode = std::fs::metadata(&base).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o700);
        assert!(sp.as_path().ends_with("s.sock"));
    }

    #[test]
    fn new_in_appends_sock_extension() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("run");
        let sp = SocketPath::new_in(&base, "barback").unwrap();
        assert_eq!(sp.as_path().file_name().unwrap(), "barback.sock");
    }

    #[test]
    fn verify_rejects_missing_path() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("run");
        let sp = SocketPath::new_in(&base, "missing").unwrap();
        // No bind has happened — the socket inode doesn't exist.
        let err = sp.verify().unwrap_err();
        match err {
            AuthError::Io(e) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
            other => panic!("expected Io(NotFound), got: {other:?}"),
        }
    }

    #[test]
    fn verify_rejects_non_socket_file() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("run");
        let sp = SocketPath::new_in(&base, "regular").unwrap();
        // Place a regular file at the socket path. `0600` so the
        // owner / perms checks would otherwise pass.
        std::fs::write(sp.as_path(), b"not a socket").unwrap();
        std::fs::set_permissions(sp.as_path(), std::fs::Permissions::from_mode(0o600))
            .unwrap();

        let err = sp.verify().unwrap_err();
        match err {
            AuthError::NotASocket { path } => assert_eq!(path, sp.as_path().display().to_string()),
            other => panic!("expected NotASocket, got: {other:?}"),
        }
    }

    #[test]
    fn parent_returns_run_dir() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("run");
        let sp = SocketPath::new_in(&base, "s").unwrap();
        assert_eq!(sp.parent(), Some(base.as_path()));
    }
}
