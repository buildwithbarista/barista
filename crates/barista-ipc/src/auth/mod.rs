//! Filesystem-permission authentication + transport-buffer zeroization.
//!
//! This module owns the *who's allowed to connect* half of the IPC
//! security contract. The transport layer (`crate::transport`) hands
//! callers a framed byte channel; this module decides which channels
//! the daemon will accept and which sockets the CLI will dial.
//!
//! # Threat model
//!
//! IPC v1 is a single-host, single-user channel. The CLI and the
//! barback daemon both run as the same OS user — there is no
//! cross-user trust boundary inside the protocol itself. The threat
//! we're defending against is therefore "another local user on the
//! same host attempting to read or inject IPC traffic". On a
//! multi-tenant Unix host (think `t2.micro` shared dev box, classroom
//! workstations, CI runners with multiple jobs interleaved), that's a
//! realistic adversary. The defenses below are all targeted at
//! denying access to *other* local users; we make no claim about the
//! root user, who can always read any FD by definition.
//!
//! # Unix — 0600 mode + 0700 directory + `SO_PEERCRED`
//!
//! The Unix-domain socket lives at `~/.barista/run/<name>.sock` and is
//! created `0600` (owner read+write, no group, no world). The parent
//! directory is created `0700`. Three layers:
//!
//! 1. **Mode bits on the directory:** `~/.barista/run/` is `0700`. An
//!    unprivileged peer cannot even traverse the directory to `stat()`
//!    the socket inode, let alone `connect(2)` to it. This closes the
//!    TOCTOU window between `bind(2)` and the follow-up `chmod(2)`.
//! 2. **Mode bits on the socket:** the socket itself is `0600`. Belt
//!    and braces — if the parent directory's perms are ever loosened
//!    (e.g. an admin running `chmod -R 755 ~/.barista` "to fix a
//!    permission problem"), the socket itself still refuses non-owner
//!    `connect(2)` attempts at the kernel layer.
//! 3. **`getsockopt(SO_PEERCRED)` on the connected stream:** after
//!    `connect(2)` succeeds, we ask the kernel for the peer's UID via
//!    the connected socket's credentials oracle. If the peer UID
//!    doesn't match our `geteuid()`, we close the connection and
//!    return [`AuthError::PeerUidMismatch`]. This catches the case
//!    where the file-system checks are spoofed (e.g. a sysadmin moved
//!    the socket inode under a directory the attacker controls).
//!
//! The client-side pre-connect check `stat(2)`s the socket and
//! verifies (a) it's a `S_IFSOCK`, (b) owner UID matches `geteuid()`,
//! (c) mode bits are exactly `0600`. Any mismatch is reported as a
//! typed [`AuthError`] before `connect(2)` is even attempted, so
//! diagnostic output identifies *which* check failed.
//!
//! # Windows — DACL'd named pipe
//!
//! There is no `chmod`-equivalent on a named pipe. Instead, the pipe
//! is created with an explicit security descriptor whose DACL grants
//! `GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE` to exactly two SIDs:
//!
//! * the current process token's user SID (read from the process's
//!   primary token via `OpenProcessToken` + `GetTokenInformation`)
//! * `NT AUTHORITY\SYSTEM` (built via `ConvertStringSidToSidW` from
//!   the SDDL-style string `"S-1-5-18"`)
//!
//! No `Everyone`, no `Authenticated Users`, no `BUILTIN\Users`. A
//! second user on the same machine attempting `CreateFileW` against
//! the pipe gets `ERROR_ACCESS_DENIED` at the kernel ACL-check layer,
//! which we map to [`AuthError::PipeAccessDenied`] on the client side.
//!
//! Cross-user testing is non-trivial on a developer host (creating a
//! second local user requires admin privileges and is hard to
//! script); the DACL is exercised end-to-end by the Windows CI runner
//! added in M0.1 T13. The unit tests here verify same-user open
//! succeeds and that the DACL has the expected shape.
//!
//! # Buffer zeroization
//!
//! The credential-bearing wire types (`Credential`, `CredentialsEnvelope`,
//! `SshKey`, and the `credential::Secret` oneof) all derive
//! `zeroize::ZeroizeOnDrop` — when a decoded `Envelope` is dropped, the
//! plaintext secret bytes are scrubbed from the prost-owned heap
//! allocation. That covers the *message* lifetime, but not the
//! transport's *wire buffer* — the `BytesMut` the codec hands us
//! contains a copy of the secret bytes (prost decodes by reading
//! through the buffer, not by stealing it).
//!
//! [`BufferZeroizer`] is the cross-platform trait the transport's
//! `recv` path uses to scrub that wire buffer before it's released
//! back to the codec's allocator pool. The contract is:
//!
//! 1. The codec yields a `BytesMut` of exactly one frame's payload.
//! 2. `Transport::recv` decodes the frame into an `Envelope`.
//! 3. **Before** the buffer is dropped (i.e. before `BytesMut`'s drop
//!    glue returns the bytes to the pool), the recv path calls
//!    `BufferZeroizer::zeroize_buffer` to overwrite the bytes with
//!    zeros. The `BytesMut` is then dropped normally — the underlying
//!    allocation may be reused, but the secret bytes are gone.
//!
//! `Transport::send` doesn't need an analogous hook because the
//! encoded-and-handed-to-the-sink `Bytes` is immutable and tracked
//! by reference count; once the sink flushes, the runtime drops it,
//! and prost's `encode_to_vec()` doesn't share a backing allocation
//! with the source `Envelope`. The source `Envelope` itself is
//! `ZeroizeOnDrop`, so the secret bytes are scrubbed when the caller
//! drops their handle.
//!
//! # `unsafe` budget
//!
//! The platform-specific submodules (`peer_cred`, `socket_path`,
//! `dacl`) call FFI into `libc` (Unix) and `windows-sys` (Windows).
//! All `unsafe` blocks carry a per-block SAFETY comment describing
//! the invariants the call relies on; the workspace `unsafe_code =
//! "warn"` policy is allowed at the module level rather than the
//! crate level to keep the unsafe blast radius confined to the auth
//! module. Code outside `auth::` remains `unsafe`-free.

#![allow(
    unsafe_code,
    reason = "FFI into libc + windows-sys for SO_PEERCRED + DACL builder; each unsafe block has a SAFETY comment"
)]

use std::io;

/// Typed errors from the auth layer.
///
/// Every variant is terminal at the connection level: an attempt to
/// `connect_secure` / `bind_secure` that returns one of these has
/// failed authentication and cannot be retried with the same socket
/// path. Callers that need to recover should construct a fresh
/// `SocketPath` / `PipeName` (e.g. after the operator fixes
/// permissions on disk).
///
/// `From<io::Error>` is implemented so platform-level `bind` /
/// `connect` / `stat` failures bubble up as [`AuthError::Io`]; the
/// more specific variants are reserved for *policy* failures the
/// auth layer detects deterministically.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The socket file's mode bits don't match the required `0600`.
    /// `actual_mode` is the value `stat(2)` returned (masked to the
    /// low 12 permission bits); `expected` is the constant `0o600`
    /// from the policy.
    #[error("socket permissions wrong: got mode {actual_mode:#o}, expected {expected:#o}")]
    SocketPermsWrong {
        /// Mode bits read from the socket inode, masked to `0o7777`.
        actual_mode: u32,
        /// The required mode (`0o600`).
        expected: u32,
    },

    /// The socket file is owned by a different UID than the current
    /// effective UID. Surfaced before `connect(2)`.
    #[error("socket owner wrong: socket owned by uid {actual_uid}, we are uid {expected_uid}")]
    SocketOwnerWrong {
        /// UID returned by `stat(2)` on the socket inode.
        actual_uid: u32,
        /// `geteuid()` at check time.
        expected_uid: u32,
    },

    /// The kernel reported a peer-UID different from our own via
    /// `getsockopt(SO_PEERCRED)`. Belt-and-braces check after a
    /// successful `connect(2)`; this catches the case where someone
    /// replaced the socket inode between our `stat(2)` and our
    /// `connect(2)` calls.
    #[error("peer UID mismatch: peer reported uid {peer_uid}, we are uid {our_uid}")]
    PeerUidMismatch {
        /// UID reported by the kernel for the peer.
        peer_uid: u32,
        /// Our `geteuid()`.
        our_uid: u32,
    },

    /// The path we expected to be a socket inode wasn't a socket —
    /// either a regular file, a symlink, a directory, or a FIFO. We
    /// refuse to `connect(2)` to it because the policy guarantees no
    /// longer apply.
    #[error("path is not a Unix-domain socket: {path}")]
    NotASocket {
        /// The offending path, lossily UTF-8 rendered for diagnostics.
        path: String,
    },

    /// The Windows kernel returned `ERROR_ACCESS_DENIED` when the
    /// client tried to open the named pipe. With the DACL we install
    /// in [`bind_secure`](#) this happens when a *different* user
    /// (not the daemon owner, not `NT AUTHORITY\SYSTEM`) attempts to
    /// open the pipe. Same-user processes get through.
    #[error(
        "named pipe access denied — the DACL only grants the daemon owner and NT AUTHORITY\\SYSTEM"
    )]
    PipeAccessDenied,

    /// Creating the per-user run directory (`~/.barista/run/`)
    /// failed. Wrapped error carries the OS-level cause (no
    /// `$HOME`, parent dir not writable, etc.).
    #[error("could not create barista run directory: {0}")]
    SocketDirCreateFailed(#[source] io::Error),

    /// Generic platform I/O during a `bind` / `connect` / `stat` /
    /// Win32 call. Wraps the underlying [`io::Error`] so callers can
    /// inspect `ErrorKind` for `NotFound`, `PermissionDenied`, etc.
    #[error("auth I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Convenience alias used throughout the auth layer.
pub type Result<T> = std::result::Result<T, AuthError>;

// ---------------------------------------------------------------------------
// Cross-platform sub-modules
// ---------------------------------------------------------------------------

pub mod zeroize;
pub use zeroize::{BufferZeroizer, zeroize_envelope};

#[cfg(unix)]
pub mod socket_path;
#[cfg(unix)]
pub use socket_path::SocketPath;

#[cfg(unix)]
pub mod peer_cred;
#[cfg(unix)]
pub use peer_cred::{our_uid, verify_peer_uid, verify_peer_uid_with_expected};

#[cfg(windows)]
pub mod pipe_name;
#[cfg(windows)]
pub use pipe_name::PipeName;

#[cfg(windows)]
pub(crate) mod dacl;

// ---------------------------------------------------------------------------
// Unit tests for the typed error model.
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
    fn auth_error_display_includes_actionable_detail() {
        // We rely on the Display strings in operator-facing
        // diagnostics, so pin their shape — a regression here changes
        // what shows up in `barista status` / barback logs.
        let e = AuthError::SocketPermsWrong {
            actual_mode: 0o644,
            expected: 0o600,
        };
        let s = format!("{e}");
        assert!(
            s.contains("0o644"),
            "Display should show actual_mode in octal: {s}"
        );
        assert!(
            s.contains("0o600"),
            "Display should show expected in octal: {s}"
        );

        let e = AuthError::PeerUidMismatch {
            peer_uid: 1000,
            our_uid: 501,
        };
        let s = format!("{e}");
        assert!(s.contains("1000"), "Display should show peer_uid: {s}");
        assert!(s.contains("501"), "Display should show our_uid: {s}");
    }

    #[test]
    fn auth_error_from_io_error_round_trips() {
        // `From<io::Error>` is what makes `?` chain cleanly from
        // platform syscalls; pin it.
        let io_e = io::Error::new(io::ErrorKind::PermissionDenied, "test");
        let auth_e: AuthError = io_e.into();
        match auth_e {
            AuthError::Io(inner) => assert_eq!(inner.kind(), io::ErrorKind::PermissionDenied),
            other => panic!("expected AuthError::Io, got: {other:?}"),
        }
    }

    #[test]
    fn pipe_access_denied_message_names_dacl() {
        // Operators reading the error need to know *why* it failed,
        // not just "access denied". Pin the Display string.
        let s = format!("{}", AuthError::PipeAccessDenied);
        assert!(
            s.contains("DACL"),
            "PipeAccessDenied should reference the DACL: {s}"
        );
    }
}
