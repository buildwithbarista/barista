// SPDX-License-Identifier: MIT OR Apache-2.0

//! `GET /healthz` — Kubernetes-canonical liveness probe.
//!
//! This is intentionally **not** the same endpoint as `/v1/health`
//! (the barista-protocol liveness check, owned by `proto::barista`).
//! The two answer different questions:
//!
//! - `/healthz` answers "is this pod alive?" for `kubelet`. It returns
//!   a plain-text `ok\n` and never returns anything else, because the
//!   k8s convention readers (a curl in a `livenessProbe` shell-exec,
//!   a load-balancer health-check) expect either a 200-with-body or a
//!   timeout/connection error.
//! - `/v1/health` answers "does the barista protocol stack work?" for
//!   clients. It returns JSON describing the protocol version.
//!
//! ## What this endpoint does *not* check
//!
//! v0.1 keeps `/healthz` shallow on purpose: it's a process-liveness
//! signal, not a deep dependency check. We do **not**:
//!
//! - Probe the storage backend (a transient `stat` failure shouldn't
//!   make kubelet restart the pod).
//! - Probe the upstream-on-miss endpoint (an upstream outage isn't a
//!   reason to kill the cache server — its hits still work).
//! - Sniff connection counts or queue depth.
//!
//! If a future operator wants a deeper readiness signal, the right
//! shape is a separate `/readyz` endpoint that reports on those
//! checks; `/healthz` stays a single line of `ok\n`. (See v0.2
//! follow-up notes.)

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// `GET /healthz` — return 200 + body `ok\n`.
///
/// Plain-text body, not JSON, matching the k8s/SRE convention. The
/// trailing newline is part of the canonical body and is what every
/// `curl -fsS http://…/healthz` script greps for.
pub async fn healthz() -> Response {
    let mut resp = (StatusCode::OK, "ok\n").into_response();
    // Pin the Content-Type explicitly. axum infers `text/plain;
    // charset=utf-8` for `&str` bodies today, but spelling it out
    // here means a future axum bump or middleware injection can't
    // silently turn this into `application/octet-stream` (which would
    // break the canonical k8s reader contract).
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn healthz_returns_200_ok_body() {
        let resp = healthz().await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            ct.starts_with("text/plain"),
            "unexpected content-type: {ct:?}"
        );
        let body = to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"ok\n");
    }
}
