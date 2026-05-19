//! The `tower::Layer` that enforces auth on the protected routes.
//!
//! `AuthLayer` is parameterised over the inner service. It captures
//! the configured verifiers — either or both of bearer + mTLS — and
//! emits an `AuthService` that runs on every protected-route request:
//!
//! 1. If a bearer verifier is configured, look for an
//!    `Authorization: Bearer <token>` header and try to match it.
//!    A successful match attaches `Principal::Bearer { token_id }`.
//! 2. Else (or if bearer matching failed and mTLS is also
//!    configured), look for a [`ClientCertChain`] request extension.
//!    Its presence proves the TLS layer already accepted the
//!    chain — the layer here only needs to extract a subject string
//!    via [`crate::auth::subject_from_cert`].
//! 3. Otherwise, return `401 BAR-AUTH-001`.
//!
//! The order matters in one small way: if both mechanisms are
//! configured and the request carries a valid bearer token, we
//! accept on bearer without consulting the cert chain. This lets a
//! client transit a load balancer or sidecar that re-terminates TLS
//! (so the original client cert is gone) provided the operator gave
//! it a bearer token. Operators who want stricter behaviour can
//! configure only one mechanism.
//!
//! The layer never reads the `Principal` after attaching it. v0.2
//! RBAC will read it from `axum::Extension<Principal>` to apply
//! per-route ACLs; for v0.1 the contract is purely "the layer
//! accepted the request" — handlers stay identity-blind.

use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::Json;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use axum::response::IntoResponse;
use futures_util::future::BoxFuture;
use rustls::pki_types::CertificateDer;
use tower::{Layer, Service};
use tracing::{debug, warn};

use crate::auth::bearer::{BearerVerifier, BearerVerifyError};
use crate::auth::mtls::{MtlsVerifier, subject_from_cert};
use crate::auth::Principal;
use crate::error::ErrorBody;

/// Per-connection client certificate chain attached as a request
/// extension by the TLS-acceptor wrapper in `crate::server`.
///
/// Wrapped in a newtype so handler/middleware code can extract it
/// unambiguously via `req.extensions().get::<ClientCertChain>()`
/// even if some other layer ever started attaching raw
/// `Vec<CertificateDer<'static>>` extensions.
///
/// Empty inner vec means "TLS was active but the peer didn't
/// present any cert" — an impossible state when mTLS is on (the TLS
/// layer would have rejected the handshake) but we still model it
/// explicitly so the auth layer's match logic is exhaustive.
#[derive(Clone, Debug)]
pub struct ClientCertChain(pub Arc<Vec<CertificateDer<'static>>>);

impl ClientCertChain {
    /// The leaf cert — the one offered by the client. None when the
    /// chain is empty (see the docstring above).
    pub fn leaf(&self) -> Option<&CertificateDer<'static>> {
        self.0.first()
    }
}

/// Auth `tower::Layer` for protected routes.
///
/// Build with [`AuthLayer::new`] from the resolved server config.
/// Cheap to clone — both verifiers are already `Arc`-shaped
/// internally, and the layer itself only re-bumps refcounts.
#[derive(Clone, Debug)]
pub struct AuthLayer {
    bearer: Option<Arc<BearerVerifier>>,
    mtls: Option<Arc<MtlsVerifier>>,
}

impl AuthLayer {
    /// Build a new layer.
    ///
    /// At least one of `bearer` / `mtls` should be `Some` in
    /// production; an `AuthLayer` with both `None` accepts every
    /// request as `Principal::Anonymous` and is only used when the
    /// server is bound to loopback with no auth configured (the
    /// dev-loop convenience path documented on
    /// [`crate::config::ServerConfig::validate`]).
    pub fn new(bearer: Option<Arc<BearerVerifier>>, mtls: Option<Arc<MtlsVerifier>>) -> Self {
        Self { bearer, mtls }
    }

    /// Whether this layer accepts unauthenticated requests as
    /// `Principal::Anonymous`. True iff neither bearer nor mTLS is
    /// configured.
    pub fn allows_anonymous(&self) -> bool {
        self.bearer.is_none() && self.mtls.is_none()
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthService {
            inner,
            bearer: self.bearer.clone(),
            mtls: self.mtls.clone(),
        }
    }
}

/// Tower service produced by [`AuthLayer`]. Owns the inner service +
/// the verifier handles.
#[derive(Clone, Debug)]
pub struct AuthService<S> {
    inner: S,
    bearer: Option<Arc<BearerVerifier>>,
    mtls: Option<Arc<MtlsVerifier>>,
}

impl<S> Service<Request<Body>> for AuthService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response<Body>, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        // Clone-then-swap is the standard tower-middleware idiom: the
        // `&mut self` `inner` may be in a not-ready state when the
        // future runs; we want the version that just returned `Ready`
        // from `poll_ready`.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let bearer = self.bearer.clone();
        let mtls = self.mtls.clone();

        Box::pin(async move {
            let outcome = decide(&req, bearer.as_deref(), mtls.as_deref());
            match outcome {
                Outcome::Allow(principal) => {
                    // Attach the principal for any downstream
                    // middleware that wants it (v0.2 RBAC). Handlers
                    // in v0.1 ignore it.
                    req.extensions_mut().insert(principal);
                    inner.call(req).await
                }
                Outcome::Deny { log_reason } => {
                    // `warn!` is the right level here — a 401 means
                    // either a misconfigured client or a probe; both
                    // are operator-relevant signals.
                    warn!(reason = log_reason, "auth: rejecting request with 401");
                    Ok(unauthorized_response())
                }
            }
        })
    }
}

/// Internal decision enum. Keeps the `call` body readable.
enum Outcome {
    Allow(Principal),
    Deny { log_reason: &'static str },
}

/// Decide whether to accept the request based on the configured
/// verifiers.
fn decide(
    req: &Request<Body>,
    bearer: Option<&BearerVerifier>,
    mtls: Option<&MtlsVerifier>,
) -> Outcome {
    // No verifier configured at all → anonymous accept. The
    // ServerConfig::validate() startup check guarantees this only
    // happens on a loopback bind.
    if bearer.is_none() && mtls.is_none() {
        return Outcome::Allow(Principal::Anonymous);
    }

    // Try bearer first if configured.
    if let Some(b) = bearer {
        match req.headers().get(header::AUTHORIZATION) {
            Some(hv) => match hv.to_str() {
                Ok(s) => match b.verify(s) {
                    Ok(Some(token_id)) => {
                        debug!(token_id = %token_id, "auth: bearer token accepted");
                        return Outcome::Allow(Principal::Bearer { token_id });
                    }
                    Ok(None) => {
                        // Valid bearer-shaped header, wrong secret.
                        // Fall through to mTLS only if mTLS is also
                        // configured — otherwise this is a hard deny.
                        if mtls.is_none() {
                            return Outcome::Deny {
                                log_reason: "bearer: unknown token",
                            };
                        }
                    }
                    Err(BearerVerifyError::Malformed) => {
                        // Same fall-through behaviour as "wrong
                        // secret" when mTLS is also configured.
                        if mtls.is_none() {
                            return Outcome::Deny {
                                log_reason: "bearer: malformed Authorization header",
                            };
                        }
                    }
                },
                Err(_) => {
                    if mtls.is_none() {
                        return Outcome::Deny {
                            log_reason: "bearer: non-ASCII Authorization header",
                        };
                    }
                }
            },
            None => {
                // No Authorization header. If bearer is the only
                // mechanism, that's a hard deny; otherwise fall
                // through to mTLS.
                if mtls.is_none() {
                    return Outcome::Deny {
                        log_reason: "bearer: no Authorization header",
                    };
                }
            }
        }
    }

    // Try mTLS if configured. The TLS layer has already validated
    // the chain — our job here is just to read the leaf subject.
    if mtls.is_some() {
        match req.extensions().get::<ClientCertChain>() {
            Some(chain) => match chain.leaf() {
                Some(leaf) => {
                    let subject = subject_from_cert(leaf);
                    debug!(subject = %subject, "auth: mTLS client cert accepted");
                    return Outcome::Allow(Principal::Mtls { subject });
                }
                None => {
                    return Outcome::Deny {
                        log_reason: "mtls: client cert chain extension was empty",
                    };
                }
            },
            None => {
                return Outcome::Deny {
                    log_reason: "mtls: no client cert chain on request",
                };
            }
        }
    }

    Outcome::Deny {
        log_reason: "no auth mechanism accepted the request",
    }
}

/// Build the canonical `401 BAR-AUTH-001` response. Centralised so
/// every deny path returns byte-identical bodies — the response body
/// MUST NOT distinguish "no token" from "bad token" (timing already
/// covered by the constant-time hash compare; the body covers the
/// observable shape).
fn unauthorized_response() -> Response<Body> {
    let body = ErrorBody::new("BAR-AUTH-001", "unauthorized");
    (StatusCode::UNAUTHORIZED, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    use axum::Router;
    use axum::body::to_bytes;
    use axum::routing::get;
    use tower::ServiceExt;

    async fn ok_handler() -> &'static str {
        "ok"
    }

    fn protected_router() -> Router {
        Router::new().route("/protected", get(ok_handler))
    }

    fn bearer(label: &str, secret: &str) -> Arc<BearerVerifier> {
        Arc::new(BearerVerifier::from_pairs(&[(label, secret)], "/test"))
    }

    #[tokio::test]
    async fn anonymous_accepted_when_no_verifier_configured() {
        let layer = AuthLayer::new(None, None);
        assert!(layer.allows_anonymous());
        let app = protected_router().layer(layer);
        let req = Request::builder()
            .uri("/protected")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bearer_accepts_valid_token() {
        let layer = AuthLayer::new(Some(bearer("ci", "s3cret")), None);
        let app = protected_router().layer(layer);
        let req = Request::builder()
            .uri("/protected")
            .header(header::AUTHORIZATION, "Bearer s3cret")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bearer_rejects_missing_header_with_bar_auth_001() {
        let layer = AuthLayer::new(Some(bearer("ci", "s3cret")), None);
        let app = protected_router().layer(layer);
        let req = Request::builder()
            .uri("/protected")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], "BAR-AUTH-001");
        // Body must NOT distinguish "no header" from "bad token".
        assert_eq!(json["message"], "unauthorized");
    }

    #[tokio::test]
    async fn bearer_rejects_wrong_token() {
        let layer = AuthLayer::new(Some(bearer("ci", "s3cret")), None);
        let app = protected_router().layer(layer);
        let req = Request::builder()
            .uri("/protected")
            .header(header::AUTHORIZATION, "Bearer wrong")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bearer_rejects_malformed_scheme() {
        let layer = AuthLayer::new(Some(bearer("ci", "s3cret")), None);
        let app = protected_router().layer(layer);
        let req = Request::builder()
            .uri("/protected")
            .header(header::AUTHORIZATION, "Basic s3cret")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
