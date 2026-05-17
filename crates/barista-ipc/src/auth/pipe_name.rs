//! Owner-restricted Windows named-pipe paths.
//!
//! [`PipeName`] is a newtype around the canonical
//! `\\.\pipe\barista\<name>` path. Unlike the Unix `SocketPath`,
//! there's no on-disk inode to chmod; the security-bearing call is
//! the explicit DACL passed to `CreateNamedPipeW` at server-create
//! time, built in [`super::dacl::PipeDacl`].
//!
//! Path conventions:
//!
//! * `\\.\pipe\` — required Windows namespace prefix; the kernel
//!   only recognizes named pipes under this root.
//! * `barista\` — sub-namespace so multiple barista-managed pipes
//!   don't collide with unrelated software on the same host.
//! * `<name>` — instance leaf, typically `barback-<pid>` or
//!   `barback-<user>` depending on the daemon's spawn strategy
//!   (the latter for the M4.2 single-daemon-per-user model).
//!
//! Windows treats pipe-name backslashes as separators inside the
//! `\\.\pipe\` namespace but does NOT require a particular depth;
//! the `barista\` prefix is convention, not a kernel-enforced
//! constraint.

#![cfg(windows)]

/// Standard root of all barista-managed named pipes.
///
/// Concatenated with the instance leaf to form the full pipe path
/// passed to `CreateNamedPipeW` (server) and `CreateFileW` (client).
const PIPE_ROOT: &str = r"\\.\pipe\barista\";

/// A vetted named-pipe path ready to be passed to tokio's
/// `ServerOptions::create_with_security_attributes_raw` (server) or
/// `ClientOptions::open` (client).
#[derive(Debug, Clone)]
pub struct PipeName {
    /// Full pipe path including the `\\.\pipe\barista\` prefix.
    /// Owned `String` so the wide-char conversion in
    /// [`Self::as_wide`] has a stable source slice.
    full: String,
}

impl PipeName {
    /// Construct a pipe path under the standard `\\.\pipe\barista\`
    /// root.
    ///
    /// `name` is the instance leaf; it must not contain backslashes
    /// (we don't enforce here — Windows itself rejects malformed
    /// names at `CreateNamedPipeW` / `CreateFileW` time with
    /// `ERROR_INVALID_NAME`, which the transport surfaces via
    /// `AuthError::Io`).
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            full: format!("{PIPE_ROOT}{name}"),
        }
    }

    /// The full pipe path as a UTF-8 string.
    ///
    /// Useful for diagnostics and for tokio's
    /// `ClientOptions::open(path)` which takes `impl AsRef<OsStr>`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.full
    }

    /// The full pipe path as a NUL-terminated UTF-16 buffer, ready
    /// for `CreateNamedPipeW` / `CreateFileW`.
    ///
    /// Used when calling raw Win32 directly; tokio's wrappers do
    /// this conversion internally and accept `&str` / `&OsStr`.
    #[must_use]
    pub fn as_wide(&self) -> Vec<u16> {
        let mut wide: Vec<u16> = self.full.encode_utf16().collect();
        wide.push(0);
        wide
    }
}

// ---------------------------------------------------------------------------
// Unit tests.
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

    #[test]
    fn new_prepends_pipe_root() {
        let p = PipeName::new("barback-42");
        assert_eq!(p.as_str(), r"\\.\pipe\barista\barback-42");
    }

    #[test]
    fn as_wide_is_null_terminated() {
        let p = PipeName::new("x");
        let w = p.as_wide();
        assert_eq!(*w.last().unwrap(), 0, "wide buffer should end in NUL");
        // Length: prefix + "x" + NUL.
        let expected_len = r"\\.\pipe\barista\".len() + 1 + 1;
        assert_eq!(w.len(), expected_len);
    }
}
