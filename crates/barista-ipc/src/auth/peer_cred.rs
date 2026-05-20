// SPDX-License-Identifier: MIT OR Apache-2.0

//! `getsockopt(SO_PEERCRED)` peer-UID verification for Unix sockets.
//!
//! After a successful `connect(2)` or `accept(2)`, we ask the kernel
//! who's on the other end of the connection and verify the UID
//! matches what we expect. This is belt-and-braces protection
//! against any TOCTOU between our `stat(2)` on the socket path and
//! the kernel's `connect(2)` resolution.
//!
//! # Platform mechanics
//!
//! * **Linux:** `SO_PEERCRED` returns a `struct ucred { pid, uid, gid }`.
//!   The kernel populates these from the *peer's* connect-time
//!   credentials, snapshot at connect time. Even if the peer
//!   subsequently calls `setuid(2)` (and isn't root), the ucred we
//!   see was captured at connect time.
//! * **macOS:** `SO_PEERCRED` is not implemented. The closest analog
//!   is `LOCAL_PEERCRED` (Darwin-specific) or `getpeereid(3)`.
//!   We use [`libc::getpeereid`] on `target_os = "macos"`.
//! * **BSD family:** `getpeereid(3)` works on FreeBSD / OpenBSD /
//!   NetBSD; we route everything non-Linux through that path.
//!
//! # `ucred` struct layout
//!
//! On Linux, `struct ucred` is:
//! ```c
//!     pid_t pid;   // i32 — peer process ID
//!     uid_t uid;   // u32 — peer effective UID
//!     gid_t gid;   // u32 — peer effective GID
//! ```
//! Total size on glibc/musl x86_64: 12 bytes. We pass that as
//! `optlen` to `getsockopt(2)`.
//!
//! # Gotchas
//!
//! * `getsockopt` on a closed socket returns `ENOTCONN`. The caller
//!   is responsible for calling this on a fresh, connected stream.
//! * `optlen` is an in/out parameter — we initialize it to the size
//!   of `ucred` and the kernel writes back the actual returned size.
//!   On Linux it's always exactly `sizeof(ucred)`; we sanity-check
//!   that anyway.
//! * `geteuid` returns the *effective* UID, not the real UID. We
//!   use effective because that's what the kernel checks for
//!   permission-bearing operations.

use std::io;
use std::os::fd::AsRawFd;

use tokio::net::UnixStream;

use super::{AuthError, Result};

/// Verify the peer of `stream` has the same UID as us.
///
/// This is the production entry point. Calls [`peer_uid`] and
/// compares against [`our_uid`]; returns [`AuthError::PeerUidMismatch`]
/// on mismatch or [`AuthError::Io`] on syscall failure.
///
/// Should be called immediately after `UnixStream::connect` (client
/// side) or `UnixListener::accept` (server side), before any
/// `Envelope` is read or written. This window is narrow enough that
/// a TOCTOU between our `stat(2)` of the socket path and the
/// kernel's `connect(2)` resolution is essentially impossible to
/// exploit.
pub fn verify_peer_uid(stream: &UnixStream) -> Result<()> {
    let our = our_uid();
    verify_peer_uid_with_expected(stream, our)
}

/// Verify the peer of `stream` matches `expected_uid`.
///
/// Test seam — production callers should use [`verify_peer_uid`] (which
/// pulls `expected` from `geteuid()`). Tests can pass an arbitrary
/// `expected_uid` to force the mismatch branch without needing to
/// run as multiple users.
pub fn verify_peer_uid_with_expected(stream: &UnixStream, expected_uid: u32) -> Result<()> {
    let peer = peer_uid(stream)?;
    if peer != expected_uid {
        return Err(AuthError::PeerUidMismatch {
            peer_uid: peer,
            our_uid: expected_uid,
        });
    }
    Ok(())
}

/// Read the peer UID from a connected `UnixStream`.
///
/// Routes to the platform-appropriate kernel mechanism:
///
/// * Linux: `getsockopt(SOL_SOCKET, SO_PEERCRED)`
/// * macOS / BSD: `getpeereid(3)`
///
/// # Errors
///
/// [`AuthError::Io`] on any syscall failure. Most commonly:
///
/// * `ENOTCONN` if the stream has been closed.
/// * `EBADF` if the FD was leaked (should be impossible from safe
///   Rust holding a `UnixStream` handle).
pub fn peer_uid(stream: &UnixStream) -> Result<u32> {
    #[cfg(target_os = "linux")]
    {
        peer_uid_linux(stream)
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        peer_uid_getpeereid(stream)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    {
        // Other Unixes (Illumos, Solaris) have their own mechanism
        // (`ucred_get`). Until we have a CI runner to validate it,
        // fail closed rather than fail open: the kernel-level UDS
        // perms (0600 + 0700 parent) are still in effect; we just
        // can't add the SO_PEERCRED belt-and-braces.
        let _ = stream;
        Err(AuthError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "SO_PEERCRED-equivalent peer-UID lookup not implemented for this target_os",
        )))
    }
}

/// Our effective UID, via `libc::geteuid()`.
///
/// `geteuid` is documented to never fail (it just reads a process
/// control block field), so this is infallible.
#[must_use]
pub fn our_uid() -> u32 {
    // SAFETY: `geteuid` is async-signal-safe and never touches errno.
    // The cast from `libc::uid_t` (typedef'd to `u32` on every target
    // we support) to `u32` is a no-op.
    #[allow(clippy::as_conversions)]
    unsafe {
        libc::geteuid() as u32
    }
}

#[cfg(target_os = "linux")]
fn peer_uid_linux(stream: &UnixStream) -> Result<u32> {
    use std::mem::MaybeUninit;

    // `struct ucred { pid: i32, uid: u32, gid: u32 }` — total 12
    // bytes on every Linux target (glibc + musl, x86_64 + aarch64).
    // We use libc's `ucred` rather than redefining the struct so
    // future ABI tweaks (unlikely; ucred has been stable for 20+
    // years) flow through with a libc bump.
    let mut cred: MaybeUninit<libc::ucred> = MaybeUninit::uninit();

    // `optlen` is `socklen_t` (u32) — initialized to the buffer size,
    // overwritten by the kernel with the returned size.
    #[allow(clippy::as_conversions)]
    let mut optlen: libc::socklen_t = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

    let fd = stream.as_raw_fd();

    // SAFETY:
    // * `fd` is a live socket FD held by the `UnixStream`.
    // * `cred` is a `MaybeUninit<ucred>` — the kernel writes the
    //   struct contents iff `getsockopt` returns 0.
    // * `optlen` points to a stack-local `socklen_t` we own.
    // * `SOL_SOCKET` + `SO_PEERCRED` is a well-defined Linux pair.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            cred.as_mut_ptr().cast::<libc::c_void>(),
            &raw mut optlen,
        )
    };
    if rc != 0 {
        return Err(AuthError::Io(io::Error::last_os_error()));
    }

    // Sanity-check the returned size; the kernel should always
    // write back exactly `sizeof(ucred)`. If a future kernel
    // changes the ABI we want to fail loud rather than silently
    // reading uninitialized bytes.
    #[allow(clippy::as_conversions)]
    let expected = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if optlen != expected {
        return Err(AuthError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "getsockopt(SO_PEERCRED) returned unexpected optlen {optlen}; expected {expected}"
            ),
        )));
    }

    // SAFETY: `getsockopt` returned 0 above, so the kernel
    // populated all bytes of the `ucred` struct.
    let cred = unsafe { cred.assume_init() };
    #[allow(clippy::as_conversions)]
    let uid = cred.uid as u32;
    Ok(uid)
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn peer_uid_getpeereid(stream: &UnixStream) -> Result<u32> {
    // `getpeereid(3)` exists on every BSD-derived OS (including
    // Darwin). It's the cleanest way to get the peer's effective
    // UID without relying on the Darwin-private LOCAL_PEERCRED.
    let fd = stream.as_raw_fd();
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;

    // SAFETY: `fd` is owned by the live `UnixStream`; the out-ptrs
    // are stack-local `uid_t`/`gid_t` we own. `getpeereid` writes
    // both iff it returns 0.
    let rc = unsafe { libc::getpeereid(fd, &raw mut uid, &raw mut gid) };
    if rc != 0 {
        return Err(AuthError::Io(io::Error::last_os_error()));
    }

    #[allow(clippy::as_conversions)]
    let uid = uid as u32;
    Ok(uid)
}

// ---------------------------------------------------------------------------
// Unit tests.
//
// Same-process round-trips are easy: we open a UDS pair and call
// `verify_peer_uid` from both ends. The peer is us, so the check
// must succeed. The mismatch branch is exercised via the test seam
// `verify_peer_uid_with_expected(..., bogus_uid)`.
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
    use tokio::net::UnixStream;

    #[tokio::test]
    async fn our_uid_matches_geteuid() {
        // Sanity: our wrapper agrees with libc directly.
        let direct = unsafe { libc::geteuid() } as u32;
        assert_eq!(our_uid(), direct);
    }

    #[tokio::test]
    async fn verify_peer_uid_succeeds_on_same_process_pair() {
        let (a, b) = UnixStream::pair().expect("pair");
        verify_peer_uid(&a).expect("a sees itself as the peer");
        verify_peer_uid(&b).expect("b sees itself as the peer");
    }

    #[tokio::test]
    async fn verify_peer_uid_with_expected_rejects_mismatch() {
        let (a, _b) = UnixStream::pair().expect("pair");
        // Force a UID that can't possibly match (u32::MAX is never a
        // real UID — POSIX reserves -1 / u32::MAX for "no user").
        let bogus = u32::MAX;
        match verify_peer_uid_with_expected(&a, bogus) {
            Err(AuthError::PeerUidMismatch { peer_uid, our_uid }) => {
                assert_eq!(
                    our_uid, bogus,
                    "expected_uid should round-trip into our_uid field"
                );
                assert_ne!(peer_uid, bogus);
            }
            other => panic!("expected PeerUidMismatch, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn peer_uid_returns_our_uid_on_pair() {
        let (a, _b) = UnixStream::pair().expect("pair");
        let peer = peer_uid(&a).expect("peer_uid should succeed");
        assert_eq!(peer, our_uid());
    }
}
