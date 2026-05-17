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
//! The plain `connect` / `from_server` constructors accept whatever
//! handle they're handed; they do not install the DACL. Use the
//! secure variants for production:
//!
//! * [`NamedPipeTransport::bind_secure`] — server side; creates the
//!   pipe via `CreateNamedPipeW` with a DACL granting access to the
//!   current process token's user SID + `NT AUTHORITY\SYSTEM` only.
//!   See [`crate::auth::dacl`] for the security-descriptor builder.
//! * [`NamedPipeTransport::connect_secure`] — client side; opens
//!   the pipe and maps `ERROR_ACCESS_DENIED` (Win32 error 5) to a
//!   typed [`crate::auth::AuthError::PipeAccessDenied`].
//!
//! Together they implement the M4.1 T5 acceptance criterion "non-
//! owner process cannot connect to the named pipe (access denied);
//! verified on Windows CI runner".
//!
//! The `#[cfg(windows)]` gate that controls whether this module is
//! compiled at all lives in `transport/mod.rs` on the `mod pipe`
//! declaration; we don't need an inner `#![cfg(windows)]` here.

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use super::{
    Result, SplitTransport, Transport, TransportError, TransportReceiver, TransportSender,
    decode_envelope, encode_envelope, framed_codec, map_codec_io_err,
};
use crate::Envelope;
use crate::auth::dacl::PipeDacl;
use crate::auth::{BufferZeroizer, PipeName};

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

    /// Open a client connection to the pipe at the vetted
    /// [`PipeName`], mapping `ERROR_ACCESS_DENIED` to a typed
    /// auth error.
    ///
    /// The DACL installed at server-create time enforces access at
    /// open time; the kernel rejects non-owner non-SYSTEM
    /// `CreateFileW` attempts with `ERROR_ACCESS_DENIED` (Win32
    /// error 5). We surface that as [`crate::auth::AuthError::
    /// PipeAccessDenied`], wrapped as a `TransportError::Io` so
    /// callers keep their existing error-branching code.
    pub async fn connect_secure(name: &PipeName) -> Result<Self> {
        match ClientOptions::new().open(name.as_str()) {
            Ok(client) => Ok(Self::from_client(client)),
            Err(e) if e.raw_os_error() == Some(5) => {
                // ERROR_ACCESS_DENIED. Map to typed AuthError, then
                // wrap as TransportError::Io for the existing error
                // contract.
                Err(TransportError::Io(std::io::Error::other(
                    crate::auth::AuthError::PipeAccessDenied,
                )))
            }
            Err(e) => Err(TransportError::Io(e)),
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

    /// Server-side: bind a `NamedPipeServer` at the vetted
    /// [`PipeName`] with the per-user DACL installed.
    ///
    /// The DACL grants `FILE_ALL_ACCESS` to:
    ///
    /// * the current process token's user SID, and
    /// * `NT AUTHORITY\SYSTEM` (S-1-5-18)
    ///
    /// All other principals (other interactive users, even
    /// administrators on the same host) get `ERROR_ACCESS_DENIED`
    /// from `CreateFileW`. See [`crate::auth::dacl`] for the
    /// security-descriptor builder.
    ///
    /// Returns the raw `NamedPipeServer`; the caller is responsible
    /// for `connect().await` and for wrapping the connected
    /// instance via [`Self::from_server`].
    ///
    /// # Safety
    ///
    /// This calls `create_with_security_attributes_raw`, which is
    /// `unsafe` because the caller must ensure the pointer remains
    /// valid for the duration of the call. We construct the DACL
    /// fresh on every invocation and let it drop after the call
    /// returns — tokio copies the SD into the kernel by then.
    pub fn bind_secure(name: &PipeName) -> Result<NamedPipeServer> {
        let dacl = PipeDacl::new().map_err(|e| TransportError::Io(std::io::Error::other(e)))?;

        // `create_with_security_attributes_raw` is `unsafe` because
        // we're handing it a raw pointer. We satisfy the contract by:
        // * Building `dacl` immediately above and keeping it alive
        //   until this fn returns (it goes out of scope at the
        //   bottom).
        // * Setting `nLength` correctly in `PipeDacl::new`.
        // * Setting `bInheritHandle = 0` so child processes don't
        //   inherit the pipe handle.
        // SAFETY: see above; the pointer is valid until `dacl`
        // drops, which happens after this fn returns its `Ok`.
        let server = unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .create_with_security_attributes_raw(
                    name.as_str(),
                    dacl.raw_attrs().cast::<std::ffi::c_void>(),
                )
        }?;

        // `dacl` drops here, after `create_with_security_attributes_raw`
        // has copied the SD into the kernel. The kernel keeps its
        // own SD reference for the pipe's lifetime.
        drop(dacl);

        Ok(server)
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
            Some(Ok(mut buf)) => {
                // Decode first, then scrub the wire bytes before
                // the `BytesMut` is released to the codec's pool.
                // See `uds.rs::recv` for the full rationale.
                let result = decode_envelope(&buf);
                buf.zeroize_buffer();
                result
            }
            Some(Err(e)) => Err(map_codec_io_err(e)),
            None => Err(TransportError::Closed),
        }
    }
}

/// Send-only half of a [`NamedPipeTransport`]. Counterpart to
/// [`PipeReceiver`]; used by the multiplex layer's writer task.
pub struct PipeSender<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> {
    sink: SplitSink<Framed<S, LengthDelimitedCodec>, bytes::Bytes>,
}

/// Recv-only half of a [`NamedPipeTransport`]. Counterpart to
/// [`PipeSender`]; used by the multiplex layer's reader task.
pub struct PipeReceiver<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> {
    stream: SplitStream<Framed<S, LengthDelimitedCodec>>,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> TransportSender for PipeSender<S> {
    async fn send(&mut self, env: Envelope) -> Result<()> {
        let payload = encode_envelope(&env)?;
        self.sink.send(payload).await.map_err(map_codec_io_err)?;
        Ok(())
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> TransportReceiver for PipeReceiver<S> {
    async fn recv(&mut self) -> Result<Envelope> {
        match self.stream.next().await {
            Some(Ok(buf)) => decode_envelope(&buf),
            Some(Err(e)) => Err(map_codec_io_err(e)),
            None => Err(TransportError::Closed),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> SplitTransport for NamedPipeTransport<S> {
    type Sender = PipeSender<S>;
    type Receiver = PipeReceiver<S>;

    fn split(self) -> (Self::Sender, Self::Receiver) {
        let (sink, stream) = self.framed.split();
        (PipeSender { sink }, PipeReceiver { stream })
    }
}

// ---------------------------------------------------------------------------
// Unit tests live in `tests/transport_pipe.rs`. They are `#[cfg(windows)]`-
// gated and run on the Windows CI runner. On Unix dev hosts this file is
// excluded from compilation entirely.
// ---------------------------------------------------------------------------
