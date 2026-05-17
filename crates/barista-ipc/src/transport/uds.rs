//! Unix-domain-socket transport (Linux + macOS).
//!
//! The CLI dials `~/.barista/run/barback-<id>.sock` via
//! [`UdsTransport::connect`]; the daemon (M4.2) wraps an accepted
//! `tokio::net::UnixStream` via [`UdsTransport::from_stream`]. Either
//! side then uses [`Transport::send`] / [`Transport::recv`] (see
//! [`crate::transport`]) for framed IO.
//!
//! # Security
//!
//! The plain `connect` / `from_stream` constructors accept whatever
//! `UnixStream` they're handed; they do not enforce socket perms.
//! Use the secure variants for production:
//!
//! * [`UdsTransport::bind_secure`] — server side; binds at a vetted
//!   [`crate::auth::SocketPath`] under a `0700` parent dir, chmods
//!   the socket inode to `0600`, returns a `UnixListener`.
//! * [`UdsTransport::connect_secure`] — client side; pre-connect
//!   `stat(2)` to verify `0600` + owner UID, then `connect(2)`, then
//!   `getsockopt(SO_PEERCRED)` to confirm peer UID.
//!
//! Together they implement the M4.1 T5 acceptance criterion "socket
//! permission check rejects non-owner connection on Linux/macOS".
//!
//! # Concurrency
//!
//! A [`UdsTransport`] is a single connection. Concurrent send + recv
//! requires splitting the underlying `Framed` into its `Sink` / `Stream`
//! halves; that wrapper is M4.1 T6's job. Until T6 ships, callers must
//! interleave `send` and `recv` on the same `&mut self`.
//!
//! The `#[cfg(unix)]` gate that controls whether this module is
//! compiled at all lives in `transport/mod.rs` on the `mod uds`
//! declaration; we don't need an inner `#![cfg(unix)]` here.

use std::path::Path;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use super::{
    Result, Transport, TransportError, decode_envelope, encode_envelope, framed_codec,
    map_codec_io_err,
};
use crate::auth::{BufferZeroizer, SocketPath, verify_peer_uid};
use crate::Envelope;

/// Transport that carries framed `Envelope`s over a Unix domain socket.
///
/// Construct via:
///
/// * [`UdsTransport::connect`] — client side; dials an existing
///   listener socket on disk.
/// * [`UdsTransport::from_stream`] — server side; wraps an already-
///   accepted `UnixStream` from a `UnixListener`.
///
/// `Debug` is derived through to the underlying `Framed`; it prints
/// the framing state and the OS-level socket descriptor — useful for
/// `assert!(matches!(...))` panic messages in tests, never leaks
/// payload bytes (which haven't been read yet by `Framed`'s buffer).
#[derive(Debug)]
pub struct UdsTransport {
    framed: Framed<UnixStream, LengthDelimitedCodec>,
}

impl UdsTransport {
    /// Dial the UDS at `path` and return a ready-to-use transport.
    ///
    /// Returns [`TransportError::Io`] if the path doesn't exist, isn't
    /// a socket, or the kernel refuses the connect. The error wraps
    /// the underlying `std::io::Error` so callers can inspect
    /// `ErrorKind` for actionable diagnostics (`NotFound`,
    /// `PermissionDenied`, `ConnectionRefused`).
    ///
    /// **Security note:** as documented in the module header, this
    /// constructor does NOT verify that the socket is owner-only (mode
    /// 0600). That check is M4.1 T5's job and will be wired in via a
    /// pre-connect hook on the client side.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self> {
        let stream = UnixStream::connect(path.as_ref()).await?;
        Ok(Self::from_stream(stream))
    }

    /// Wrap an already-accepted `UnixStream` (server side).
    ///
    /// Used by the daemon after `UnixListener::accept()`. The caller
    /// owns whatever permission check ran on the listener; this
    /// constructor just attaches the framing codec.
    #[must_use]
    pub fn from_stream(stream: UnixStream) -> Self {
        Self {
            framed: Framed::new(stream, framed_codec()),
        }
    }

    /// Dial the UDS at a vetted [`SocketPath`] with the full T5
    /// security check.
    ///
    /// Three-step ceremony:
    ///
    /// 1. Pre-connect: call [`SocketPath::verify`] to confirm the
    ///    socket inode exists, is a `S_IFSOCK`, is owned by us,
    ///    and has mode bits exactly `0600`. Any mismatch surfaces
    ///    as a typed [`crate::auth::AuthError`] mapped to
    ///    [`TransportError::Io`] (the `AuthError` is wrapped in
    ///    `io::Error::other` to preserve the Display string).
    /// 2. Connect: `UnixStream::connect`.
    /// 3. Post-connect: [`verify_peer_uid`] — verify the kernel's
    ///    `SO_PEERCRED` UID matches our `geteuid()`. If a TOCTOU
    ///    swapped the inode under us between the `stat(2)` and
    ///    the `connect(2)`, this check catches it.
    ///
    /// All three steps run before the function returns
    /// `Ok(UdsTransport)`. The caller may then `send` / `recv`
    /// knowing the policy holds.
    pub async fn connect_secure(socket_path: &SocketPath) -> Result<Self> {
        socket_path.verify().map_err(auth_to_transport_err)?;
        let stream = UnixStream::connect(socket_path.as_path()).await?;
        verify_peer_uid(&stream).map_err(auth_to_transport_err)?;
        Ok(Self::from_stream(stream))
    }

    /// Server-side: bind a `UnixListener` at the vetted
    /// [`SocketPath`] with the full T5 security ceremony.
    ///
    /// Four-step ceremony:
    ///
    /// 1. Best-effort `unlink_if_exists` to clear any stale socket
    ///    inode from a previous crash.
    /// 2. `UnixListener::bind` at the socket path. The parent
    ///    directory was created `0700` at `SocketPath::new` time,
    ///    so the inode is already inaccessible to non-owners
    ///    *during* the bind window — closing the TOCTOU
    ///    between bind and the follow-up chmod.
    /// 3. `chmod(2)` to `0600`. Defense-in-depth in case the
    ///    parent dir's perms are ever loosened later.
    /// 4. Return the listener for the daemon to `accept()` on.
    ///
    /// The caller is responsible for the `accept(2)` loop and for
    /// running [`verify_peer_uid`] on each accepted `UnixStream`
    /// before wrapping it via [`Self::from_stream`].
    pub fn bind_secure(socket_path: &SocketPath) -> Result<UnixListener> {
        socket_path.unlink_if_exists().map_err(TransportError::Io)?;
        let listener = UnixListener::bind(socket_path.as_path())?;
        socket_path.chmod_to_policy().map_err(auth_to_transport_err)?;
        Ok(listener)
    }

    /// Borrow the underlying `tokio::net::UnixStream` immutably.
    ///
    /// Useful for diagnostics — `peer_cred()`, `local_addr()`, etc. —
    /// without giving up framing state. Mutable access is deliberately
    /// not exposed: writing to the raw stream while a `Framed` is
    /// buffered would corrupt the wire.
    #[must_use]
    pub fn inner(&self) -> &UnixStream {
        self.framed.get_ref()
    }
}

/// Coerce an `AuthError` into the transport's typed error model.
/// We wrap as `Io(io::Error::other(auth_err))` so the Display string
/// propagates verbatim while preserving the `TransportError::Io`
/// variant callers already branch on. `is_terminal()` returns true
/// for `Io`, so the connection is correctly considered poisoned.
fn auth_to_transport_err(auth_err: crate::auth::AuthError) -> TransportError {
    TransportError::Io(std::io::Error::other(auth_err))
}

impl Transport for UdsTransport {
    async fn send(&mut self, env: Envelope) -> Result<()> {
        let payload = encode_envelope(&env)?;
        // `Framed`'s `Sink::send` returns `io::Error`; route through the
        // typed error pathway (this also re-classifies the codec's
        // `InvalidData` oversize-frame error). `encode_envelope` already
        // rejected pre-cap, so the codec's outbound oversize path is
        // effectively dead code here — we route it consistently anyway
        // so a future change to `encode_envelope` doesn't silently swap
        // error variants.
        self.framed.send(payload).await.map_err(map_codec_io_err)?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Envelope> {
        match self.framed.next().await {
            Some(Ok(mut buf)) => {
                // Decode first — prost reads through the buffer; it
                // does NOT take ownership of the underlying allocation.
                // Once decode returns, the `Envelope` holds its own
                // heap copies of every variable-length field, so we
                // can safely scrub the wire buffer.
                let result = decode_envelope(&buf);
                // Scrub the wire bytes BEFORE the `BytesMut` is
                // dropped. The codec's allocator pool may re-issue
                // this allocation to a subsequent frame; zeroing
                // here prevents a credential-bearing buffer from
                // being silently re-served as fresh "uninitialized"
                // memory.
                //
                // This runs on EVERY recv, not just credential-
                // carrying ones, because the recv path doesn't peek
                // at the frame contents before passing them to
                // prost — branching on "is this an ActionRequest
                // with credentials?" would add latency to every
                // non-credential frame and risk a bypass when the
                // peek logic gets the discrimination wrong.
                buf.zeroize_buffer();
                result
            }
            Some(Err(e)) => Err(map_codec_io_err(e)),
            None => Err(TransportError::Closed),
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests live in `tests/transport_uds.rs` (integration tests need a
// Tokio runtime + tempdir + Unix-only socket plumbing).
// ---------------------------------------------------------------------------
