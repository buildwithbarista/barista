//! Worker-protocol IPC transport between the CLI and the barback daemon.
//!
//! This crate hosts the Rust side of the IPC wire contract defined in
//! `proto/barista/v1/worker.proto`. It ships:
//!
//! * The generated message types under [`proto`], produced by `prost-build`
//!   from the canonical schema at compile time.
//! * Convenience re-exports of every top-level message at the crate root so
//!   downstream callers can write `barista_ipc::Envelope` rather than
//!   `barista_ipc::proto::Envelope`.
//! * Redacted `Debug` impls and `ZeroizeOnDrop` derives on the credential
//!   types (`Credential`, `CredentialsEnvelope`, `SshKey`,
//!   `credential::Secret`) — see [`proto`] for the contract.
//! * A framed bidirectional transport under [`transport`] (UDS on Unix,
//!   named pipe on Windows) implementing the 4-byte big-endian length-
//!   prefix wire format from PRD §12.1. The transport hands callers
//!   `async fn send(Envelope) -> Result<()>` / `async fn recv() ->
//!   Result<Envelope>` and hides codec / framing / buffering details.
//!
//! The socket-permission layer (0600 UDS mode, DACL'd named pipe) and the
//! streaming/multiplexing/cancellation layer ride on top of [`transport`]
//! in subsequent tasks. This crate ships the framing primitive; higher
//! protocol semantics layer on the `Transport` trait.

/// Generated wire types and their redacted-`Debug` overrides.
pub mod proto;

// Filesystem-permission auth + buffer-zeroization (M4.1 T5). The
// module's own `//!` doc-comment in `src/auth/mod.rs` is authoritative
// — it covers the 0600 UDS perms model on Unix, the DACL'd named-pipe
// model on Windows, and the cross-platform `BufferZeroizer` trait that
// the transport's `recv` path uses to scrub credential-carrying wire
// buffers before they re-enter the codec's allocator pool.
pub mod auth;

// The module's own `//!` doc-comment in `src/transport/mod.rs` is
// authoritative — see there for the wire format, the `Transport`
// trait, the typed error model, and the cross-platform submodule
// layout (UDS / named pipe). We deliberately don't repeat that here
// in an outer doc-comment: doing so would create a second copy of the
// same intra-doc links resolved from a different scope, which
// rustdoc reports as broken when the linked items live inside the
// child module.
pub mod transport;

// Re-export the transport surface at the crate root so downstream
// callers can write `barista_ipc::Transport` / `barista_ipc::
// TransportError`. The concrete `UdsTransport` and `NamedPipeTransport`
// stay namespaced under `transport::` to keep the crate root focused on
// the trait + error model + the wire types from `proto`.
pub use transport::{MAX_FRAME_BYTES, Transport, TransportError};

// Re-export the auth surface so downstream callers can write
// `barista_ipc::AuthError`, `barista_ipc::BufferZeroizer`,
// `barista_ipc::zeroize_envelope`. Platform-specific newtypes
// (`SocketPath` on Unix, `PipeName` on Windows) are re-exported
// from inside the `auth` module under matching cfg gates.
pub use auth::{AuthError, BufferZeroizer, zeroize_envelope};

#[cfg(unix)]
pub use auth::SocketPath;

#[cfg(windows)]
pub use auth::PipeName;

// Re-export every top-level message at the crate root.
//
// `Envelope` is the single wire-level type; the rest are body variants or
// nested helpers. We expose them all because tests, transport plumbing,
// and downstream callers (barista-cli, future roastery-client) construct
// them by name.
pub use proto::{
    ActionRequest, ActionResult, ActionStream, CancelRequest, Credential, CredentialsEnvelope,
    Envelope, Error, Mojo, Ping, Pong, ProducedArtifact, ProgressEvent, Shutdown, SshKey,
    StatusRequest, StatusResponse,
};

// The `Envelope.body` oneof and `Credential.secret` oneof live in dedicated
// sub-modules under `proto`. Re-export the modules themselves so callers
// can write `barista_ipc::envelope::Body::Ping(...)` and
// `barista_ipc::credential::Secret::Password(...)`.
pub use proto::{credential, envelope};

// `ProgressEvent::Kind` and `ActionResult::Status` are nested enums on
// their parent messages. prost emits them under sibling modules
// (`progress_event`, `action_result`) — re-export those so external callers
// don't have to qualify the long path.
pub use proto::{action_result, progress_event};
