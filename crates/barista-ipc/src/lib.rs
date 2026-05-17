//! Worker-protocol IPC transport between the CLI and the barback daemon.
//!
//! This crate hosts the Rust side of the IPC wire contract defined in
//! `proto/barista/v1/worker.proto`. At this milestone it ships:
//!
//! * The generated message types under [`proto`], produced by `prost-build`
//!   from the canonical schema at compile time.
//! * Convenience re-exports of every top-level message at the crate root so
//!   downstream callers can write `barista_ipc::Envelope` rather than
//!   `barista_ipc::proto::Envelope`.
//! * Redacted `Debug` impls and `ZeroizeOnDrop` derives on the credential
//!   types (`Credential`, `CredentialsEnvelope`, `SshKey`,
//!   `credential::Secret`) — see [`proto`] for the contract.
//!
//! The framed-transport layer (4-byte big-endian length prefix over a
//! 0600 UDS or DACL'd named pipe) is intentionally not in this milestone —
//! it lands in a subsequent task. This crate is byte-shape-only until then.

/// Generated wire types and their redacted-`Debug` overrides.
pub mod proto;

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
