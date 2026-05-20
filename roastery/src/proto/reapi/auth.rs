//! gRPC auth interceptor for the REAPI CAS data services.
//!
//! The barista-protocol CAS routes sit behind the axum/tower
//! [`crate::auth::AuthLayer`]. The gRPC CAS data services
//! (`ContentAddressableStorage` + `ByteStream`) get the equivalent
//! posture through this tonic [`Interceptor`], which mirrors the bearer
//! half of the layer's [`decide`](crate::auth) logic:
//!
//! - **No auth configured** → accept (the loopback dev-loop posture;
//!   `ServerConfig::validate` guarantees this only happens on a
//!   loopback bind).
//! - **Bearer configured** → require a valid `authorization: Bearer
//!   <token>` metadata entry, verified against the same
//!   [`BearerVerifier`] the HTTP layer loaded. A miss is `UNAUTHENTICATED`.
//! - **mTLS configured** → the rustls server config already required a
//!   client cert chained to the configured CA during the handshake, so
//!   a connection that reached this interceptor is transport-level
//!   authenticated. When mTLS is the *only* mechanism, the interceptor
//!   accepts (the handshake did the gating). When bearer is also
//!   configured, a valid bearer token short-circuits, matching the HTTP
//!   layer's "either mechanism suffices" contract.
//!
//! The data services must never be MORE open than their HTTP
//! counterparts: whenever the HTTP CAS routes require a credential,
//! these gRPC services require one too. `Capabilities` is intentionally
//! left unauthenticated (the negotiation surface), exactly like the
//! public HTTP `/v1/capabilities`.
//!
//! ## v0.2 follow-up
//!
//! This interceptor does not extract a per-call mTLS *subject* the way
//! the HTTP path does (tonic surfaces peer certs through a connection
//! extension, not request metadata, and threading that into a plain
//! `Interceptor` is more plumbing than the gating contract needs in
//! v0.1). When v0.2 adds RBAC, the interceptor should grow to attach a
//! `Principal` extension mirroring `crate::auth::Principal`; the gating
//! behaviour here is already correct, only the identity capture is
//! deferred.

use std::sync::Arc;

use tonic::service::Interceptor;
use tonic::{Request, Status};

use crate::auth::BearerVerifier;
use crate::server::AppState;

/// Cloneable tonic interceptor that enforces the bearer requirement on
/// the REAPI CAS data services. Built from [`AppState`] so it shares the
/// exact verifier the HTTP auth layer loaded at startup.
#[derive(Clone)]
pub struct ReapiAuth {
    /// `Some` when bearer auth is configured; the same `Arc` the HTTP
    /// `AuthLayer` holds.
    bearer: Option<Arc<BearerVerifier>>,
    /// Whether mTLS is configured. When `true`, a connection that
    /// reached the handler already presented a CA-chained client cert
    /// (the rustls server config required it), so transport-level auth
    /// is satisfied.
    mtls_configured: bool,
}

impl ReapiAuth {
    /// Build the interceptor from the resolved server state. Reads the
    /// loaded bearer verifier (if any) and whether mTLS is configured.
    pub fn from_state(state: &AppState) -> Self {
        Self {
            bearer: state.bearer.clone(),
            mtls_configured: state.config.auth.mtls.is_some(),
        }
    }

    /// Whether this interceptor accepts unauthenticated calls. True iff
    /// neither bearer nor mTLS is configured — the loopback dev posture.
    fn allows_anonymous(&self) -> bool {
        self.bearer.is_none() && !self.mtls_configured
    }
}

impl Interceptor for ReapiAuth {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        // No mechanism configured → anonymous accept (loopback only,
        // enforced by ServerConfig::validate at startup).
        if self.allows_anonymous() {
            return Ok(request);
        }

        // Try bearer first if configured.
        if let Some(verifier) = &self.bearer {
            if let Some(value) = request.metadata().get("authorization")
                && let Ok(header) = value.to_str()
                // `Ok(Some(token_id))` is a successful match; `Ok(None)`
                // (unknown token) and any `Err(_)` (malformed header)
                // fall through to mTLS only if it is configured.
                && let Ok(Some(_token_id)) = verifier.verify(header)
            {
                return Ok(request);
            }
            // Bearer is the only mechanism and it didn't accept → deny.
            if !self.mtls_configured {
                return Err(Status::unauthenticated("missing or invalid bearer token"));
            }
        }

        // mTLS: a connection that reached here completed the CA-chained
        // handshake (rustls enforced it), so transport-level auth is
        // satisfied. Accept.
        if self.mtls_configured {
            return Ok(request);
        }

        Err(Status::unauthenticated("unauthorized"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::sync::Arc;

    use crate::config::ServerConfig;
    use crate::server::AppState;
    use crate::storage::{Cas, FsCas};

    fn state_with_bearer(bearer: Option<Arc<BearerVerifier>>) -> AppState {
        let tmp = std::env::temp_dir().join(format!("reapi-auth-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let cas = FsCas::new(tmp).unwrap();
        let cas: Arc<dyn Cas> = Arc::new(cas);
        let config = ServerConfig::with_bind("127.0.0.1:0".parse().unwrap());
        AppState {
            cas,
            config: Arc::new(config),
            upstream: None,
            bearer,
        }
    }

    fn req_with_auth(value: Option<&str>) -> Request<()> {
        let mut req = Request::new(());
        if let Some(v) = value {
            req.metadata_mut()
                .insert("authorization", v.parse().unwrap());
        }
        req
    }

    #[test]
    fn anonymous_accepts_when_no_auth_configured() {
        let mut auth = ReapiAuth::from_state(&state_with_bearer(None));
        assert!(auth.call(req_with_auth(None)).is_ok());
    }

    #[test]
    fn bearer_accepts_valid_token() {
        let verifier = Arc::new(BearerVerifier::from_pairs(&[("ci", "s3cret")], "/test"));
        let mut auth = ReapiAuth::from_state(&state_with_bearer(Some(verifier)));
        assert!(auth.call(req_with_auth(Some("Bearer s3cret"))).is_ok());
    }

    #[test]
    fn bearer_rejects_missing_token() {
        let verifier = Arc::new(BearerVerifier::from_pairs(&[("ci", "s3cret")], "/test"));
        let mut auth = ReapiAuth::from_state(&state_with_bearer(Some(verifier)));
        let err = auth.call(req_with_auth(None)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn bearer_rejects_wrong_token() {
        let verifier = Arc::new(BearerVerifier::from_pairs(&[("ci", "s3cret")], "/test"));
        let mut auth = ReapiAuth::from_state(&state_with_bearer(Some(verifier)));
        let err = auth.call(req_with_auth(Some("Bearer wrong"))).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }
}
