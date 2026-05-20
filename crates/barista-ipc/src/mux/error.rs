// SPDX-License-Identifier: MIT OR Apache-2.0

//! Typed errors for the multiplex layer.
//!
//! [`MuxError`] is distinct from [`crate::TransportError`] so callers can
//! distinguish wire-level failures (socket closed, decode error) from
//! multiplex-level failures (action submitted to a shut-down multiplexer,
//! per-action channel buffer overflow). The two error families compose:
//! `MuxError::Transport` wraps a `TransportError` for the underlying
//! socket failures, and the rest are conditions that only exist in the
//! multiplex layer.
//!
//! Every variant is terminal at the per-action granularity — once an
//! action produces a `MuxError`, the handle is done and the caller
//! should drop it. The connection-level multiplexer may still be usable
//! (e.g. a single action overrun does not shut the socket down); the
//! caller can resubmit on a fresh `ActionHandle` if appropriate.

use crate::TransportError;

/// Errors returned by the multiplex layer.
///
/// Variants are carefully scoped:
///
/// * [`Transport`](Self::Transport) — the underlying socket / codec
///   raised a typed [`TransportError`]. Routed transparently; the
///   multiplexer does not retry. The connection itself is poisoned.
/// * [`MultiplexerShutDown`](Self::MultiplexerShutDown) — the
///   multiplexer's background tasks have exited. New action submissions
///   fail with this; in-flight actions get a `Closed` on their per-
///   stream channel. Triggered by a socket close, a fatal codec error,
///   or an explicit `Multiplexer::shutdown` (future).
/// * [`UnknownAction`](Self::UnknownAction) — an inbound envelope
///   carried a correlation id the multiplexer doesn't know about (no
///   per-stream channel registered). On the client this means the
///   daemon sent a response for an action we never submitted (or one
///   we cancelled past); on the server it means a `CancelRequest`
///   arrived for an action that has already finished. Logged but not
///   fatal; the envelope is dropped.
/// * [`SendBufferFull`](Self::SendBufferFull) — the outbound mpsc
///   buffer is full and the writer task can't keep up. The caller is
///   producing envelopes faster than the network can drain them. This
///   is the "natural backpressure" signal — if `ActionHandle::send_*`
///   sees this in non-blocking mode (`try_send`), it's a bug; the
///   public API uses `send().await` which blocks on the buffer being
///   drained.
/// * [`DaemonCrashed`](Self::DaemonCrashed) — the M4.2 T6 failure-
///   model signal: the daemon process died mid-action (typically a
///   `kill -9` or an internal `Runtime.halt`), and at least one
///   in-flight action never received its terminal `Result`/`Error`.
///   This is the *retryable* path: a CLI driver may respawn the
///   daemon and resubmit idempotent actions; non-idempotent actions
///   must surface the error to the user. Maps 1:1 to the
///   `BAR-DAEMON-CRASHED` wire code (PRD §A `BAR-DAEMON-001`). See
///   the `kind` field for the originating `io::ErrorKind` that
///   triggered the classification.
///
/// The error is `Clone` for the test fixtures that need to inspect the
/// same error across multiple `select!` arms; in production code,
/// callers typically just match on the variant.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    /// Wire-level failure from the underlying [`crate::Transport`].
    /// Connection is poisoned.
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),

    /// The multiplexer's background tasks have exited. Subsequent
    /// `submit_action` / `next_action` calls will return this without
    /// touching the wire.
    #[error("multiplexer has shut down")]
    MultiplexerShutDown,

    /// An inbound envelope's correlation id matched no registered
    /// per-stream channel. Logged at the dispatch site; the envelope
    /// is dropped silently. This variant exists for diagnostics /
    /// metrics; callers do not normally see it.
    #[error("unknown action id: {action_id}")]
    UnknownAction {
        /// The unknown correlation id, as it appeared on the wire.
        action_id: String,
    },

    /// The per-action outbound channel could not accept a new envelope
    /// because its buffer is full. In the synchronous (`try_send`)
    /// path this is the explicit backpressure error; in the async
    /// (`send().await`) path it's effectively unreachable because the
    /// future awaits free buffer space.
    #[error("outbound send buffer full")]
    SendBufferFull,

    /// The daemon crashed (or was `kill -9`'d) while this action was
    /// in flight. Wire code: `BAR-DAEMON-CRASHED` (PRD §A
    /// `BAR-DAEMON-001`). **Retryable:** the caller may respawn the
    /// daemon and resubmit if the action is idempotent. Carries the
    /// originating `io::ErrorKind` so callers can distinguish the
    /// canonical crash flavours (`BrokenPipe`, `ConnectionReset`,
    /// `UnexpectedEof`) — useful for telemetry but not for routing.
    ///
    /// The multiplex layer produces this error in two distinct
    /// paths, both of which feed the connection-shutdown classifier
    /// in `mux::mod::shutdown_state`:
    ///
    ///   * the underlying [`crate::TransportError::DaemonCrashed`]
    ///     surfaces from `recv` (the OS told us a crash kind);
    ///   * the underlying [`crate::TransportError::Closed`] surfaces
    ///     with at least one in-flight client still registered — a
    ///     graceful EOF that landed *during* an in-flight action is
    ///     functionally indistinguishable from an abrupt crash and
    ///     receives the same retryable classification.
    #[error("daemon crashed mid-action ({kind:?}); BAR-DAEMON-CRASHED is retryable")]
    DaemonCrashed {
        /// The originating `io::ErrorKind`. For the "clean EOF
        /// while in flight" path this is synthesized as
        /// `UnexpectedEof` so callers don't have to special-case the
        /// Closed origin.
        kind: std::io::ErrorKind,
    },
}

/// Convenience alias used throughout the multiplex layer.
pub type Result<T> = std::result::Result<T, MuxError>;
