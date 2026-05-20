// SPDX-License-Identifier: MIT OR Apache-2.0

//! Framed bidirectional transport for the worker IPC protocol.
//!
//! This module implements the byte-channel layer that sits between the
//! generated `prost` message types in [`crate::proto`] and the OS-level
//! socket primitives. It hands callers a pair of `async fn send(Envelope)`
//! / `async fn recv() -> Envelope` methods and hides every detail of how
//! bytes get on and off the wire.
//!
//! # Wire format
//!
//! Per PRD §12.1, each `Envelope` is framed as:
//!
//! ```text
//!   ┌────────────────────────┬──────────────────────────┐
//!   │ length: u32 big-endian │ payload: protobuf bytes  │
//!   │       (4 bytes)        │      (length bytes)      │
//!   └────────────────────────┴──────────────────────────┘
//! ```
//!
//! The framing is provided by [`tokio_util::codec::LengthDelimitedCodec`],
//! configured below in [`framed_codec`]. We do not roll our own framer:
//! `LengthDelimitedCodec` already handles partial frames, torn writes,
//! buffered reads, and back-pressure correctly, and is used in production
//! by `tonic`, `tarpc`, and `kafka-protocol` (among others).
//!
//! # Frame-size guardrail
//!
//! [`MAX_FRAME_BYTES`] caps each frame at 16 MiB. The cap is not a PRD
//! requirement — PRD §12 doesn't specify a hard limit — but is a
//! defense-in-depth bound:
//!
//! * The largest realistic payload is an `ActionRequest` carrying an
//!   `effective_pom_blob` (CBOR-encoded). Multi-module reactors with
//!   hundreds of dependencies produce blobs in the tens of KiB; 16 MiB
//!   leaves four orders of magnitude of headroom.
//! * A peer announcing a 4 GiB length would otherwise force us to
//!   `read_exact` into a 4 GiB buffer before noticing the protocol
//!   violation. Capping at 16 MiB bounds that worst case to a single
//!   page of allocation per malformed frame.
//! * The cap is the same on both directions and on both transports
//!   (UDS + named pipe). When a peer sends an oversized frame we
//!   surface [`TransportError::FrameTooLarge`] and close — there's no
//!   recovery path inside protocol v1.
//!
//! # Encoding / decoding
//!
//! `Envelope::send` calls `prost::Message::encode_to_vec()`, hands the
//! bytes to the codec, and returns. `recv` waits for one framed buffer,
//! runs `Envelope::decode`, and returns. Errors from either half map to
//! the variants of [`TransportError`].
//!
//! # Security policy
//!
//! Filesystem-permission auth (`0600` UDS mode + per-user-SID DACL'd
//! named pipe) and credential-buffer zeroization live in
//! [`crate::auth`]. The secure constructors on each concrete
//! transport — `UdsTransport::connect_secure` /
//! `UdsTransport::bind_secure` on Unix, and
//! `NamedPipeTransport::connect_secure` / `bind_secure` on Windows —
//! run the full permission ceremony before returning. The plain
//! `connect` / `from_stream` constructors stay available for tests
//! and for callers that own the listener-side perms ceremony
//! themselves.
//!
//! `Transport::recv` calls
//! [`crate::auth::BufferZeroizer::zeroize_buffer`] on the wire
//! `BytesMut` after decoding each frame, so credential-carrying
//! bytes don't linger in the codec's allocator pool.
//!
//! # What this module does NOT do
//!
//! * **No streaming / multiplexing.** Stream IDs, per-stream channels,
//!   cancellation, and head-of-line-blocking avoidance are M4.1 T6's
//!   job. T4/T5 ship only `send` / `recv` primitives; T6 wraps them.
//! * **No reconnection.** A `Transport` is a single connection. On
//!   `recv` returning [`TransportError::Closed`], the caller decides
//!   whether to dial a fresh transport.
//!
//! # Cross-platform layout
//!
//! Concrete transports live in dedicated submodules:
//!
//! * `uds` — Unix-domain-socket transport, gated `#[cfg(unix)]`.
//! * `pipe` — Windows named-pipe transport, gated `#[cfg(windows)]`.
//!
//! (The submodules are not intra-doc-linkable from this comment
//! because at most one of them is present on any given host; the
//! cfg-gating means rustdoc on the other platform would report a
//! broken link. The module declarations appear at the bottom of this
//! file.)
//!
//! Both produce a `Framed<Stream, LengthDelimitedCodec>` and share the
//! `Envelope` encode/decode helpers in this file. The [`Transport`]
//! trait is the common interface.

use bytes::Bytes;
use prost::Message;
use tokio_util::codec::LengthDelimitedCodec;

use crate::Envelope;

#[cfg(unix)]
pub mod uds;

#[cfg(windows)]
pub mod pipe;

// ---------------------------------------------------------------------------
// Public constants
// ---------------------------------------------------------------------------

/// Maximum size, in bytes, of a single framed `Envelope` payload.
///
/// Frames larger than this — in either direction, on either transport —
/// produce a [`TransportError::FrameTooLarge`] and the connection is
/// considered poisoned. See the module-level docs for the rationale
/// behind the 16 MiB value.
///
/// This bound is also enforced when configuring the
/// `LengthDelimitedCodec`'s `max_frame_length` so an oversized inbound
/// frame is rejected *before* its bytes are read into a buffer.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Width of the length prefix, in bytes. Matches PRD §12.1.
pub const LENGTH_FIELD_BYTES: usize = 4;

// ---------------------------------------------------------------------------
// Error model
// ---------------------------------------------------------------------------

/// Errors returned by the transport layer.
///
/// All variants are terminal: once a transport produces an error, the
/// underlying socket is considered unusable and the caller should drop
/// the [`Transport`] handle. Partial-read recovery is the codec's job,
/// not the caller's — by the time a `TransportError` surfaces, the
/// codec has already determined that recovery is impossible.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The peer closed the socket cleanly (read returned EOF before a
    /// complete frame). Distinct from [`Self::Io`] so callers can
    /// distinguish "peer hung up" from "kernel error during read".
    ///
    /// This is the "clean disconnect" path — the peer called `close(2)`
    /// at a frame boundary and we observed the resulting EOF before
    /// any partial frame was buffered. Compare with
    /// [`Self::DaemonCrashed`], which is the "peer died mid-frame"
    /// path the M4.2 T6 failure-model wiring detects.
    #[error("peer closed the transport")]
    Closed,

    /// The peer process disappeared mid-action: a `BrokenPipe` /
    /// `ConnectionReset` / `UnexpectedEof` surfaced from the read or
    /// write half of the socket, characteristic of an external
    /// `kill -9` (or equivalent abrupt JVM exit via
    /// `Runtime.halt`) while a frame was in flight.
    ///
    /// Distinct from [`Self::Io`] so the multiplex layer can map this
    /// to the canonical `BAR-DAEMON-CRASHED` (PRD §A
    /// {@code BAR-DAEMON-001}) retryable error without re-inspecting
    /// the underlying `io::Error` kind. The caller decides per-action
    /// whether to respawn-and-retry (idempotent actions) or surface
    /// the error to the user (non-idempotent or already-side-effecting
    /// actions). See `crate::mux::error::MuxError::DaemonCrashed` for
    /// the action-scoped wrapper.
    ///
    /// The `kind` field carries the originating `io::ErrorKind` so
    /// telemetry can distinguish the three crash flavours
    /// (`BrokenPipe` on writes after the peer's TCP/UDS close;
    /// `ConnectionReset` on resets from the kernel; `UnexpectedEof`
    /// on truncated reads). Promotion to a stable string in the wire
    /// error's `details` map is the multiplex layer's job — at the
    /// transport layer we keep the raw kind for diagnostics.
    #[error("daemon crashed mid-action ({kind:?})")]
    DaemonCrashed {
        /// The originating `io::ErrorKind` that triggered the crash
        /// classification. Never `Other`; the mapping helper only
        /// reclassifies the three canonical crash kinds.
        kind: std::io::ErrorKind,
    },

    /// I/O error from the OS — socket reset, broken pipe, permission
    /// denied on connect, etc. The wrapped [`std::io::Error`] retains
    /// the OS error code for diagnostics.
    #[error("transport I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// `prost::Message::encode_to_vec()` failed. In practice prost
    /// only emits an error here when a length field would overflow
    /// `usize`; for our schema this is effectively unreachable, but
    /// keeping it typed is cheaper than panicking.
    #[error("envelope encode failed: {0}")]
    Encode(#[from] prost::EncodeError),

    /// `Envelope::decode` failed on inbound bytes — malformed
    /// protobuf, unknown wire type, tag-number conflict, etc. Per
    /// PRD §12.8 the daemon treats this as a protocol violation and
    /// closes the connection.
    #[error("envelope decode failed: {0}")]
    Decode(#[from] prost::DecodeError),

    /// The peer advertised a frame larger than [`MAX_FRAME_BYTES`].
    /// Carries the offending length so logs can record what was
    /// rejected.
    ///
    /// Note: `LengthDelimitedCodec` enforces the cap during read by
    /// returning `io::Error` with kind `InvalidData`; we surface that
    /// as this typed variant via the (crate-private)
    /// `map_codec_io_err` helper in this module.
    #[error(
        "frame size {announced} exceeds max-frame cap of {} bytes",
        MAX_FRAME_BYTES
    )]
    FrameTooLarge {
        /// The length the peer announced, as decoded from the 4-byte
        /// big-endian prefix. May be the inbound `u32` or an outbound
        /// payload length our encoder refused to send.
        announced: u64,
    },
}

impl TransportError {
    /// `true` if this error indicates the connection should be torn
    /// down with no further `send` / `recv` attempts. Every variant
    /// currently returns `true`; the predicate exists so future
    /// recoverable variants can be added without churning callers.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Closed
            | Self::DaemonCrashed { .. }
            | Self::Io(_)
            | Self::Encode(_)
            | Self::Decode(_)
            | Self::FrameTooLarge { .. } => true,
        }
    }

    /// `true` if this error is the failure-model "daemon died mid-
    /// action" signal: an in-flight action should surface a
    /// {@code BAR-DAEMON-CRASHED} retryable error rather than a hard
    /// transport failure. Wraps the pattern-match so callers don't
    /// have to import [`std::io::ErrorKind`] just to branch.
    #[must_use]
    pub fn is_daemon_crash(&self) -> bool {
        matches!(self, Self::DaemonCrashed { .. })
    }
}

/// Convenience alias used throughout the transport layer.
pub type Result<T> = std::result::Result<T, TransportError>;

// ---------------------------------------------------------------------------
// Transport trait
// ---------------------------------------------------------------------------

/// A bidirectional, framed channel that carries `Envelope` messages.
///
/// Both methods take `&mut self` because the underlying `Framed` wraps a
/// single `AsyncRead + AsyncWrite` stream and we expose it as a single
/// owned handle. Concurrent `send` + `recv` on the same transport are
/// the streaming-layer's responsibility (M4.1 T6) and require splitting
/// the `Framed` into its `Sink` / `Stream` halves; that split lives in
/// T6's wrapper, not here.
///
/// # Cancel safety
///
/// `send` and `recv` are `async fn` futures over the underlying
/// `tokio` socket and `LengthDelimitedCodec`. Both are cancel-safe at
/// the granularity of a complete frame: cancelling a future before
/// completion leaves the codec's internal buffer in a consistent state
/// (partial reads stay buffered; partial writes never flush a partial
/// frame). T6 relies on this when implementing per-stream cancellation.
pub trait Transport {
    /// Send a single `Envelope`. Returns once the frame has been
    /// queued into the underlying `tokio` writer's buffer (not once
    /// the peer has acknowledged — there is no application-level ack
    /// at this layer; that's the protocol's job).
    fn send(&mut self, env: Envelope) -> impl Future<Output = Result<()>> + Send;

    /// Wait for one inbound `Envelope`. Returns
    /// [`TransportError::Closed`] when the peer hangs up cleanly mid-
    /// or pre-frame; [`TransportError::Io`] for hard failures; and
    /// [`TransportError::FrameTooLarge`] when the codec rejects an
    /// oversized header before reading the body.
    fn recv(&mut self) -> impl Future<Output = Result<Envelope>> + Send;
}

// ---------------------------------------------------------------------------
// Split transport (sender + receiver halves)
// ---------------------------------------------------------------------------

/// The send-only half of a [`Transport`].
///
/// Exposes only `send`. Carved out so the multiplex layer (M4.1 T6) can
/// own the writer half in a dedicated background task while the
/// receiver half lives in the reader task — without any locking on the
/// underlying socket. See `mux::Multiplexer` for the consumer.
pub trait TransportSender: Send + 'static {
    /// Send a single `Envelope`. See [`Transport::send`] for semantics.
    fn send(&mut self, env: Envelope) -> impl Future<Output = Result<()>> + Send;
}

/// The recv-only half of a [`Transport`]. Counterpart to
/// [`TransportSender`]; see that trait's docs.
pub trait TransportReceiver: Send + 'static {
    /// Wait for one inbound `Envelope`. See [`Transport::recv`] for
    /// semantics.
    fn recv(&mut self) -> impl Future<Output = Result<Envelope>> + Send;
}

/// A [`Transport`] that can be split into independently-owned
/// sender + receiver halves.
///
/// Implementors must split the underlying socket in a way that lets
/// the two halves operate concurrently without locking — typically via
/// `tokio::io::split` on an `AsyncRead + AsyncWrite` stream, or via
/// `futures_util::StreamExt::split` on a `Framed`. The multiplex layer
/// (M4.1 T6) requires this property: its reader and writer tasks run
/// in parallel, and routing a `CancelRequest` from the writer while
/// the reader is blocked on `recv` would deadlock if the underlying
/// transport serialised both halves on a single mutex.
pub trait SplitTransport: Transport {
    /// The sender half produced by [`split`](Self::split).
    type Sender: TransportSender;
    /// The receiver half produced by [`split`](Self::split).
    type Receiver: TransportReceiver;

    /// Consume the transport and return its independently-owned
    /// sender / receiver halves. The two halves must be safe to use
    /// concurrently — see the trait-level doc for the constraint.
    fn split(self) -> (Self::Sender, Self::Receiver);
}

// ---------------------------------------------------------------------------
// Shared codec configuration
// ---------------------------------------------------------------------------

/// Build the `LengthDelimitedCodec` used by every concrete transport.
///
/// Configured for:
///
/// * `length_field_offset = 0` — the length sits at the very start of
///   each frame (PRD §12.1).
/// * `length_field_length = 4` — a 4-byte length, per PRD §12.1.
/// * big-endian (the codec's default) — also per PRD §12.1.
/// * `length_adjustment = 0` — the announced length is the payload
///   length only; the 4-byte header is not included in the count.
/// * `max_frame_length = MAX_FRAME_BYTES` — see module-level docs.
///
/// Both the read half and the write half of every `Framed` use this
/// same configuration, so the codec rejects oversized frames in both
/// directions symmetrically.
#[must_use]
pub fn framed_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_offset(0)
        .length_field_length(LENGTH_FIELD_BYTES)
        .length_adjustment(0)
        .big_endian()
        .max_frame_length(MAX_FRAME_BYTES)
        .new_codec()
}

// ---------------------------------------------------------------------------
// Encode / decode helpers
// ---------------------------------------------------------------------------

/// Encode an `Envelope` to the framed-codec wire payload (the protobuf
/// bytes; the codec prepends the 4-byte length when it writes).
///
/// We also enforce [`MAX_FRAME_BYTES`] on the *outbound* side: if our
/// own serializer would emit a frame larger than the cap, fail with
/// [`TransportError::FrameTooLarge`] rather than relying on the peer
/// to reject the frame. This catches application bugs (e.g. an
/// `ActionRequest` constructed with a multi-gigabyte
/// `effective_pom_blob`) at the sender, where the diagnostic is most
/// useful.
pub(crate) fn encode_envelope(env: &Envelope) -> Result<Bytes> {
    let payload = env.encode_to_vec();
    if payload.len() > MAX_FRAME_BYTES {
        // `u64::try_from(usize)` is infallible on every Rust target we
        // care about (`usize` is 32 or 64 bits; both fit in `u64`), but
        // we use the fallible form to satisfy the workspace
        // `clippy::as_conversions = warn` lint without sprinkling
        // per-call `#[allow]` annotations. The `unwrap_or(u64::MAX)`
        // tail is unreachable on any sane target — it's there to keep
        // the function infallible at the lint level.
        let announced = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        return Err(TransportError::FrameTooLarge { announced });
    }
    Ok(Bytes::from(payload))
}

/// Decode an inbound framed payload back into an `Envelope`.
///
/// The codec has already validated the length prefix and produced a
/// `BytesMut` of exactly `payload-length` bytes. Decode is the only
/// step that can still fail at the protocol level, and that's what
/// [`TransportError::Decode`] covers.
pub(crate) fn decode_envelope(buf: &[u8]) -> Result<Envelope> {
    Envelope::decode(buf).map_err(TransportError::from)
}

/// Convert the `io::Error` that `LengthDelimitedCodec` returns when a
/// peer-announced length exceeds `max_frame_length` into a typed
/// [`TransportError::FrameTooLarge`]. The codec uses
/// `io::ErrorKind::InvalidData` with a string-shaped message; we match
/// on the kind only (the message is upstream-defined and not stable).
///
/// The `announced` value is best-effort: the codec discards the raw
/// length once it rejects the frame, so we report `0` to indicate
/// "the length exceeded the cap but the exact value isn't recoverable
/// from the codec error". Callers who need the exact size can enable
/// trace-level logging on the codec itself.
pub(crate) fn map_codec_io_err(e: std::io::Error) -> TransportError {
    if e.kind() == std::io::ErrorKind::InvalidData {
        // Heuristic: `LengthDelimitedCodec`'s oversized-frame error
        // is the only `InvalidData` it currently emits, but we keep
        // the `Io` fallback so future codec changes don't silently
        // mis-route a hard I/O error.
        let msg = e.to_string();
        if msg.contains("frame size too big") || msg.contains("max frame length") {
            return TransportError::FrameTooLarge { announced: 0 };
        }
    }
    // M4.2 T6 failure-model: classify the three canonical
    // "peer died mid-frame" kinds as `DaemonCrashed`. The codec
    // surfaces these unchanged from the underlying socket (a
    // `read(2)` that returns EBADF / ECONNRESET / EPIPE, or a
    // partial-frame `read(2) == 0` after the codec already buffered
    // some bytes of the length prefix or payload).
    //
    // Why match on `ErrorKind` and not the error message: kinds are
    // a stable contract; messages are upstream-defined and host-
    // locale-dependent (libc strerror text differs across glibc /
    // musl / macOS). The three kinds below are exactly the set the
    // kernel emits on a kill-9'd peer; anything else passes through
    // as `Io` so callers don't conflate a real bug (e.g. permission
    // change mid-stream) with the crash path.
    if matches!(
        e.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::UnexpectedEof
    ) {
        return TransportError::DaemonCrashed { kind: e.kind() };
    }
    // `tokio_util::codec::FramedRead` surfaces a partial-frame EOF
    // (the peer closed the connection while bytes were still
    // buffered for an incomplete length prefix or payload) as an
    // `io::Error{ kind: Other, message: "bytes remaining on stream" }`.
    // This is the exact shape `kill -9` produces when the daemon
    // had queued part of a response onto its socket buffer before
    // halting: the kernel flushes the partial bytes to us, then
    // delivers EOF. Reclassify as `DaemonCrashed { UnexpectedEof }`
    // so the multiplex layer can route it through the
    // `BAR-DAEMON-CRASHED` synthesised reply. The string match is
    // exact (upstream-stable: `LengthDelimitedCodec` constructs the
    // literal "bytes remaining on stream" in its decode path; see
    // tokio-util 0.7's `length_delimited.rs::FramedImpl`).
    if e.kind() == std::io::ErrorKind::Other && e.to_string().contains("bytes remaining on stream")
    {
        return TransportError::DaemonCrashed {
            kind: std::io::ErrorKind::UnexpectedEof,
        };
    }
    TransportError::Io(e)
}

// ---------------------------------------------------------------------------
// Unit tests for the shared helpers.
// ---------------------------------------------------------------------------
//
// Tests that actually move bytes over a socket live under `tests/` so
// they can spin up their own Tokio runtime; here we only exercise the
// pure helpers (`encode_envelope`, `decode_envelope`, the codec config,
// and the error model).

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::as_conversions
    )]

    use super::*;
    use crate::{Ping, envelope};

    #[test]
    fn max_frame_bytes_is_16_mib() {
        assert_eq!(MAX_FRAME_BYTES, 16 * 1024 * 1024);
    }

    #[test]
    fn length_field_is_4_bytes_per_prd() {
        assert_eq!(LENGTH_FIELD_BYTES, 4);
    }

    #[test]
    fn codec_uses_4_byte_big_endian_prefix() {
        // We can't directly inspect the codec's private fields, but we
        // *can* assert it's the default configuration we expect by
        // round-tripping a buffer through Encoder/Decoder and checking
        // the byte shape.
        use bytes::BytesMut;
        use tokio_util::codec::{Decoder, Encoder};

        let mut codec = framed_codec();
        let mut buf = BytesMut::new();
        codec
            .encode(Bytes::from_static(&[0x00, 0x01, 0x02, 0x03]), &mut buf)
            .unwrap();

        // First 4 bytes must be big-endian length = 4, then payload.
        assert_eq!(
            &buf[..4],
            &[0, 0, 0, 4],
            "length prefix is 4-byte big-endian"
        );
        assert_eq!(
            &buf[4..],
            &[0x00, 0x01, 0x02, 0x03],
            "payload follows verbatim"
        );

        // Round-trip through the decoder.
        let decoded = codec
            .decode(&mut buf)
            .unwrap()
            .expect("frame should decode");
        assert_eq!(&decoded[..], &[0x00, 0x01, 0x02, 0x03]);
    }

    #[test]
    fn encode_envelope_succeeds_on_small_payload() {
        let env = Envelope {
            version: 1,
            request_id: 7,
            body: Some(envelope::Body::Ping(Ping {
                client: "barista 0.1.0".to_string(),
                sent_at_unix_micros: 42,
            })),
        };
        let bytes = encode_envelope(&env).expect("encode should succeed");
        // Sanity-check by round-tripping through decode_envelope.
        let round = decode_envelope(&bytes).expect("decode should succeed");
        assert_eq!(env, round);
    }

    #[test]
    fn encode_envelope_rejects_oversized_payload() {
        // Build an Envelope whose body is so large that prost's
        // serialized output exceeds MAX_FRAME_BYTES. We use
        // `ActionStream.payload` (a `bytes` field) — the on-wire
        // shape is `tag + length-varint + bytes`, so a 16 MiB + 1024
        // byte payload guarantees an over-cap frame.
        use crate::ActionStream;

        let big = vec![0u8; MAX_FRAME_BYTES + 1024];
        let env = Envelope {
            version: 1,
            request_id: 1,
            body: Some(envelope::Body::Stream(ActionStream {
                stream_id: 1,
                payload: big,
                end: false,
                action_id: "act-too-big".to_string(),
            })),
        };

        match encode_envelope(&env) {
            Err(TransportError::FrameTooLarge { announced }) => {
                assert!(
                    announced as usize > MAX_FRAME_BYTES,
                    "FrameTooLarge.announced should exceed cap; got {announced}"
                );
            }
            Err(other) => panic!("expected FrameTooLarge, got: {other:?}"),
            Ok(_) => panic!("expected FrameTooLarge, got Ok"),
        }
    }

    #[test]
    fn decode_envelope_rejects_garbage() {
        // Random bytes that don't form a valid protobuf header.
        // Tag 0 is invalid in proto3, so a leading 0x00 byte trips
        // prost's "invalid tag value" check.
        let garbage = vec![0xFFu8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        match decode_envelope(&garbage) {
            Err(TransportError::Decode(_)) => {}
            other => panic!("expected Decode error, got: {other:?}"),
        }
    }

    #[test]
    fn empty_payload_decodes_to_empty_envelope() {
        // An empty buffer is a valid (empty) protobuf message — every
        // field is optional in proto3 — so it decodes to a default
        // `Envelope` with `version: 0`, `request_id: 0`, `body: None`.
        let env = decode_envelope(&[]).expect("empty buf is a valid empty proto message");
        assert_eq!(env.version, 0);
        assert_eq!(env.request_id, 0);
        assert!(env.body.is_none());
    }

    #[test]
    fn transport_error_is_terminal_on_every_variant() {
        // We assert every variant is terminal because callers branch on
        // `is_terminal()` and we want the predicate's behavior pinned.
        // If a future variant becomes recoverable, this test fails and
        // forces an update of the predicate and the doc comment.
        assert!(TransportError::Closed.is_terminal());
        assert!(
            TransportError::DaemonCrashed {
                kind: std::io::ErrorKind::BrokenPipe,
            }
            .is_terminal()
        );
        assert!(TransportError::FrameTooLarge { announced: 1 }.is_terminal());
        // `Io` carries an `io::Error` with a kind *other* than the three
        // crash-kinds, since those are now reclassified upstream by
        // `map_codec_io_err`. Pick `Other` to make the test pin both
        // the terminal-ness and the kind-disjointness.
        let io = std::io::Error::other("synthetic");
        assert!(TransportError::Io(io).is_terminal());
    }

    #[test]
    fn is_daemon_crash_predicate_is_kind_independent() {
        // The three canonical crash kinds all classify as DaemonCrashed.
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            assert!(TransportError::DaemonCrashed { kind }.is_daemon_crash());
        }
        // Other variants do not.
        assert!(!TransportError::Closed.is_daemon_crash());
        assert!(!TransportError::FrameTooLarge { announced: 1 }.is_daemon_crash());
        let io = std::io::Error::other("not a crash");
        assert!(!TransportError::Io(io).is_daemon_crash());
    }

    #[test]
    fn map_codec_io_err_routes_invalid_data_oversize_to_frame_too_large() {
        // Mirror the exact `io::Error` shape `LengthDelimitedCodec`
        // emits when the announced length exceeds `max_frame_length`.
        let e = std::io::Error::new(std::io::ErrorKind::InvalidData, "frame size too big");
        match map_codec_io_err(e) {
            TransportError::FrameTooLarge { .. } => {}
            other => panic!("expected FrameTooLarge, got: {other:?}"),
        }
    }

    #[test]
    fn map_codec_io_err_routes_crash_kinds_to_daemon_crashed() {
        // The three canonical "peer died mid-frame" `io::ErrorKind`s
        // (M4.2 T6 failure model) must surface as `DaemonCrashed`
        // rather than `Io`, so the multiplex layer can synthesise a
        // `BAR-DAEMON-CRASHED` reply per in-flight action without
        // re-inspecting the wrapped `io::Error`.
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            let e = std::io::Error::new(kind, "peer hung up mid-frame");
            match map_codec_io_err(e) {
                TransportError::DaemonCrashed { kind: reported } => {
                    assert_eq!(reported, kind, "DaemonCrashed.kind preserves origin");
                }
                other => panic!("expected DaemonCrashed for {kind:?}, got: {other:?}"),
            }
        }
    }

    #[test]
    fn map_codec_io_err_routes_partial_frame_eof_to_daemon_crashed() {
        // `LengthDelimitedCodec` emits an `io::Error{ Other, "bytes
        // remaining on stream" }` when the peer closes mid-frame.
        // This is the dominant shape on macOS + Linux when the
        // daemon is `kill -9`'d while a reply is partially queued;
        // M4.2 T6 requires it routes to `DaemonCrashed` not `Io`.
        let e = std::io::Error::other("bytes remaining on stream");
        match map_codec_io_err(e) {
            TransportError::DaemonCrashed { kind } => {
                assert_eq!(
                    kind,
                    std::io::ErrorKind::UnexpectedEof,
                    "partial-frame EOF surfaces as synthesised UnexpectedEof"
                );
            }
            other => panic!("expected DaemonCrashed (partial-frame EOF), got: {other:?}"),
        }
    }

    #[test]
    fn map_codec_io_err_passes_through_non_crash_io_errors() {
        // A "real" non-crash IO error (PermissionDenied, NotFound, etc.)
        // must NOT be misclassified as DaemonCrashed — those are bugs
        // or misconfiguration, not the failure-model path.
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "EACCES");
        match map_codec_io_err(e) {
            TransportError::Io(inner) => {
                assert_eq!(inner.kind(), std::io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected Io, got: {other:?}"),
        }
    }
}
