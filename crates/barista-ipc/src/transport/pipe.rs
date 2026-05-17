//! Windows named-pipe transport.
//!
//! Mirrors [`super::uds`] on the Windows side. The CLI dials
//! `\\.\pipe\barback-<id>` via [`NamedPipeTransport::connect`]; the
//! daemon (M4.2) wraps an accepted `tokio::net::windows::named_pipe::
//! NamedPipeServer` via [`NamedPipeTransport::from_server`].
//!
//! # Cross-compile gating
//!
//! `tokio::net::windows::named_pipe::*` is `#[cfg(windows)]`-gated by
//! tokio itself. We re-export the same `cfg` here so non-Windows hosts
//! exclude this file entirely from compilation. The CI matrix in
//! `.github/workflows/ci.yml` (added in M0.1 T13) runs the Windows
//! target on a `windows-latest` runner; on macOS / Linux dev hosts this
//! file is a no-op.
//!
//! Cross-compiling from a Unix host requires either the
//! `x86_64-pc-windows-msvc` target (rustup-managed) or the
//! `x86_64-pc-windows-gnu` MinGW toolchain. Neither is part of the
//! Barista `rust-toolchain.toml` pin, by design: the canonical Windows
//! build is the dedicated CI runner. Developers who need a local
//! cross-build can `rustup target add x86_64-pc-windows-gnu` and
//! `cargo check --target x86_64-pc-windows-gnu -p barista-ipc`.
//!
//! # Security
//!
//! As with the UDS transport, T4 does **not** install the DACL that
//! restricts the pipe to the current-user SID + `NT AUTHORITY\SYSTEM`.
//! That's M4.1 T5's job. T4 accepts whatever `NamedPipeServer` /
//! `NamedPipeClient` it's handed.
//!
//! The `#[cfg(windows)]` gate that controls whether this module is
//! compiled at all lives in `transport/mod.rs` on the `mod pipe`
//! declaration; we don't need an inner `#![cfg(windows)]` here.

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use super::{
    Result, Transport, TransportError, decode_envelope, encode_envelope, framed_codec,
    map_codec_io_err,
};
use crate::Envelope;

/// One side of a named-pipe connection, framed with the shared codec.
///
/// We parameterize over the underlying pipe type (`NamedPipeClient` vs
/// `NamedPipeServer`) because tokio uses distinct types for the two
/// roles (client-instance vs accepted-server-instance) — they aren't
/// interchangeable the way a Unix `UnixStream` is. Both types
/// implement `AsyncRead + AsyncWrite + Unpin + Send`, which is all
/// `Framed` needs.
///
/// `Debug` is derived for parity with `UdsTransport`; see that type's
/// doc-comment for the rationale.
#[derive(Debug)]
pub struct NamedPipeTransport<S: AsyncRead + AsyncWrite + Unpin + Send> {
    framed: Framed<S, LengthDelimitedCodec>,
}

impl NamedPipeTransport<NamedPipeClient> {
    /// Open a client connection to the named pipe `name`.
    ///
    /// `name` is the full pipe path, conventionally
    /// `\\.\pipe\barback-<id>`. Returns [`TransportError::Io`] on
    /// `ERROR_FILE_NOT_FOUND` (no listener), `ERROR_PIPE_BUSY` (all
    /// instances in use; the daemon should size its pool to make this
    /// rare), and `ERROR_ACCESS_DENIED` (DACL mismatch — once T5
    /// lands, a different user attempting to open the pipe).
    ///
    /// Callers handling `ERROR_PIPE_BUSY` should retry with backoff;
    /// the canonical retry strategy lives in `barista-cli`'s spawn-
    /// or-attach logic, not here.
    pub async fn connect(name: &str) -> Result<Self> {
        // `ClientOptions::open` is synchronous (the Win32 call is
        // `CreateFileW`, which doesn't have an async variant); tokio
        // wraps it without an `async` keyword. We keep this function
        // `async` for API symmetry with the UDS transport.
        let client = ClientOptions::new().open(name)?;
        Ok(Self::from_client(client))
    }

    /// Wrap an already-opened `NamedPipeClient`.
    ///
    /// Used by tests and by callers that want to configure the
    /// client (e.g. with `ClientOptions::pipe_mode`) before handing
    /// it to the transport.
    #[must_use]
    pub fn from_client(client: NamedPipeClient) -> Self {
        Self {
            framed: Framed::new(client, framed_codec()),
        }
    }
}

impl NamedPipeTransport<NamedPipeServer> {
    /// Wrap an accepted `NamedPipeServer` (daemon side).
    ///
    /// The daemon creates a `NamedPipeServer` with
    /// `ServerOptions::create()` and calls `connect()` to wait for a
    /// client. Once that future resolves, the server instance is
    /// "occupied" and ready for framed IO — pass it in here.
    #[must_use]
    pub fn from_server(server: NamedPipeServer) -> Self {
        Self {
            framed: Framed::new(server, framed_codec()),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> NamedPipeTransport<S> {
    /// Borrow the underlying pipe handle immutably. See
    /// [`super::uds::UdsTransport::inner`] for the rationale.
    #[must_use]
    pub fn inner(&self) -> &S {
        self.framed.get_ref()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Transport for NamedPipeTransport<S> {
    async fn send(&mut self, env: Envelope) -> Result<()> {
        let payload = encode_envelope(&env)?;
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
// Unit tests live in `tests/transport_pipe.rs`. They are `#[cfg(windows)]`-
// gated and run on the Windows CI runner. On Unix dev hosts this file is
// excluded from compilation entirely.
// ---------------------------------------------------------------------------
