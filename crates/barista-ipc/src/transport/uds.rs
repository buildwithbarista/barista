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
//! T4 deliberately does **not** set the socket's filesystem mode. The
//! 0600 owner-only mode required by PRD §12.1 is M4.1 T5's job — T5
//! lands a `SocketPermissions` helper that the daemon calls *after*
//! binding the listener and *before* accepting connections, plus a
//! pre-connect check on the client side that rejects worldly-readable
//! sockets. T4 only owns the wire; it accepts already-permissioned
//! `UnixStream`s and dials whatever path it's handed.
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
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use super::{
    Result, Transport, TransportError, decode_envelope, encode_envelope, framed_codec,
    map_codec_io_err,
};
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
            Some(Ok(buf)) => decode_envelope(&buf),
            Some(Err(e)) => Err(map_codec_io_err(e)),
            None => Err(TransportError::Closed),
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests live in `tests/transport_uds.rs` (integration tests need a
// Tokio runtime + tempdir + Unix-only socket plumbing).
// ---------------------------------------------------------------------------
