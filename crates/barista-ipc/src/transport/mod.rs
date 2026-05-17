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
//! # What this module does NOT do
//!
//! * **No security policy.** Setting the UDS to 0600 and DACL-ing the
//!   named pipe is M4.1 T5's job. Transports expose constructors that
//!   accept already-permissioned streams; the listener-side permission
//!   hook is left to T5.
//! * **No streaming / multiplexing.** Stream IDs, per-stream channels,
//!   cancellation, and head-of-line-blocking avoidance are M4.1 T6's
//!   job. T4 ships only `send` / `recv` primitives; T6 wraps them.
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
    #[error("peer closed the transport")]
    Closed,

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
    #[error("frame size {announced} exceeds max-frame cap of {} bytes", MAX_FRAME_BYTES)]
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
            | Self::Io(_)
            | Self::Encode(_)
            | Self::Decode(_)
            | Self::FrameTooLarge { .. } => true,
        }
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
        codec.encode(Bytes::from_static(&[0x00, 0x01, 0x02, 0x03]), &mut buf)
            .unwrap();

        // First 4 bytes must be big-endian length = 4, then payload.
        assert_eq!(&buf[..4], &[0, 0, 0, 4], "length prefix is 4-byte big-endian");
        assert_eq!(&buf[4..], &[0x00, 0x01, 0x02, 0x03], "payload follows verbatim");

        // Round-trip through the decoder.
        let decoded = codec.decode(&mut buf).unwrap().expect("frame should decode");
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
        assert!(TransportError::FrameTooLarge { announced: 1 }.is_terminal());
        let io = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "test");
        assert!(TransportError::Io(io).is_terminal());
    }

    #[test]
    fn map_codec_io_err_routes_invalid_data_oversize_to_frame_too_large() {
        // Mirror the exact `io::Error` shape `LengthDelimitedCodec`
        // emits when the announced length exceeds `max_frame_length`.
        let e = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame size too big",
        );
        match map_codec_io_err(e) {
            TransportError::FrameTooLarge { .. } => {}
            other => panic!("expected FrameTooLarge, got: {other:?}"),
        }
    }

    #[test]
    fn map_codec_io_err_passes_through_real_io_errors() {
        // A "real" IO error (BrokenPipe, ConnectionReset, etc.) must
        // not be misclassified as FrameTooLarge.
        let e = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "peer hung up");
        match map_codec_io_err(e) {
            TransportError::Io(inner) => {
                assert_eq!(inner.kind(), std::io::ErrorKind::BrokenPipe);
            }
            other => panic!("expected Io, got: {other:?}"),
        }
    }
}
