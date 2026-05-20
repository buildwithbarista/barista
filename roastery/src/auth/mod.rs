// SPDX-License-Identifier: MIT OR Apache-2.0

//! Authentication for the roastery server.
//!
//! The auth surface supports two mutually-acceptable mechanisms:
//!
//! - **Bearer token** — operator publishes a tokens file containing
//!   `<label>:<secret>` entries; clients send `Authorization: Bearer
//!   <secret>` on every request to a protected route. Tokens are
//!   loaded once at startup, hashed with SHA-256, and compared in
//!   constant time so a network-adjacent attacker can't recover a
//!   token byte-by-byte from timing.
//! - **mTLS** — operator publishes a CA certificate; the TLS
//!   acceptor requires every client to present a certificate chained
//!   to that CA. A successful handshake yields a `Principal` keyed
//!   on the cert's Subject Common Name or first URI SAN.
//!
//! Both can be configured simultaneously; **either** mechanism
//! suffices on a per-request basis. Routes that the operator wants
//! locked down get wrapped in an [`AuthLayer`]; routes that need to
//! stay public (k8s probes, Prometheus scrapes, the protocol-level
//! capability negotiation surface) are mounted on a separate
//! sub-router that the layer never sees.
//!
//! ## Fail-closed default
//!
//! A roastery bound to a non-loopback address with **no** auth
//! configured refuses to start (see [`crate::config::ServerConfig::
//! validate`]). Loopback binds (`127.0.0.1` / `::1`) without auth are
//! allowed so the `cargo run -p roastery` dev workflow stays
//! one-command. Operators who want to expose a roastery on a routable
//! address MUST configure at least one of bearer or mTLS.
//!
//! ## Forward-looking shape
//!
//! Successful auth attaches a [`Principal`] to the request as an
//! `axum::Extension`. v0.1 handlers ignore the principal — they just
//! check that the layer accepted the request. v0.2 RBAC will read
//! the principal to apply per-route ACLs; the wire shape is reserved
//! today so adding RBAC is a layering change, not an API break.
//!
//! Logged identifiers (the bearer `token_id` label, the mTLS
//! subject) are non-secret by construction: the token's plaintext
//! never leaves the loader, and the cert's Subject CN / URI SAN is
//! public material the client offered to the server. Raw secrets are
//! never logged at any level.

pub mod bearer;
pub mod layer;
pub mod mtls;

pub use bearer::BearerVerifier;
pub use layer::{AuthLayer, ClientCertChain};
pub use mtls::{MtlsVerifier, subject_from_cert};

/// The authenticated identity attached to a request after the auth
/// layer accepts it.
///
/// Attached via `axum::Extension<Principal>` so downstream handlers
/// (or future RBAC middleware) can extract it without parsing
/// headers a second time. v0.1 handlers do not consult the principal
/// — its presence on the request is the contract; the value itself
/// is forward-looking plumbing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// No auth was configured on the server and the request arrived
    /// on a loopback bind. Only ever produced when neither
    /// [`BearerVerifier`] nor [`MtlsVerifier`] is active.
    Anonymous,
    /// Bearer token matched a configured tokens-file entry.
    /// `token_id` is the non-secret label from the tokens file (or a
    /// short SHA-256 prefix when no label was provided) — safe to
    /// log; never the raw token.
    Bearer {
        /// Non-secret identifier for the matching token.
        token_id: String,
    },
    /// Client presented an X.509 certificate that chained to the
    /// configured CA. `subject` is the cert's Subject Common Name or
    /// the first URI SAN, in that order of preference.
    Mtls {
        /// Public subject extracted from the client cert.
        subject: String,
    },
}

impl Principal {
    /// Short, log-safe identifier for this principal. Combines the
    /// auth mechanism + the non-secret identifier so structured logs
    /// can distinguish bearer vs mTLS without exposing secrets.
    pub fn log_id(&self) -> String {
        match self {
            Principal::Anonymous => "anonymous".to_string(),
            Principal::Bearer { token_id } => format!("bearer:{token_id}"),
            Principal::Mtls { subject } => format!("mtls:{subject}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_id_covers_each_variant() {
        assert_eq!(Principal::Anonymous.log_id(), "anonymous");
        assert_eq!(
            Principal::Bearer {
                token_id: "ci-runner".to_string()
            }
            .log_id(),
            "bearer:ci-runner"
        );
        assert_eq!(
            Principal::Mtls {
                subject: "CN=ci".to_string()
            }
            .log_id(),
            "mtls:CN=ci"
        );
    }
}
