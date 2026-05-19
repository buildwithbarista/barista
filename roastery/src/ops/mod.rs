//! Operational endpoints — the `/healthz`, `/metrics`, `/version`
//! surface a Kubernetes/SRE deployment expects.
//!
//! These routes are intentionally **separate** from the
//! barista-protocol surface (`/v1/health`, `/v1/capabilities`, …):
//!
//! - `/healthz` is the k8s liveness convention — a plain-text 200 used
//!   by `livenessProbe` / `readinessProbe`. It answers "is this pod
//!   alive?" and has nothing to say about the protocol stack.
//! - `/v1/health` is the **barista-protocol** liveness endpoint owned
//!   by `proto::barista`. It answers "does the barista protocol speak
//!   here?" and returns a JSON document.
//!
//! Both coexist on purpose. Clients use `/v1/health`; orchestration
//! infrastructure (kubelet, load balancers, smoke tests) uses
//! `/healthz`.
//!
//! ## Endpoints
//!
//! | Method | Path        | Body                                      | Content-Type                                  |
//! |--------|-------------|-------------------------------------------|-----------------------------------------------|
//! | `GET`  | `/healthz`  | `ok\n`                                    | `text/plain; charset=utf-8`                   |
//! | `GET`  | `/metrics`  | Prometheus text exposition (v0.0.4)       | `text/plain; version=0.0.4; charset=utf-8`    |
//! | `GET`  | `/version`  | JSON build-identity (see [`version`])     | `application/json`                            |
//!
//! ## Metric inventory (v0.1)
//!
//! - `roastery_build_info{version, rustc} 1` — info-style gauge; the
//!   labels carry the build identity, the value is the standard "1"
//!   sentinel.
//! - `roastery_uptime_seconds` — gauge; seconds since the metric
//!   registry was initialised (≈ process start).
//! - `roastery_cas_requests_total{method, result}` — counter per CAS
//!   handler outcome. `method` ∈ `{get, head, put}`,
//!   `result` ∈ `{hit, miss, error}`.
//! - `roastery_cas_request_duration_seconds_bucket{method, le=…}`
//!   plus `_sum` / `_count` — histogram of CAS handler latency. The
//!   default buckets (`0.001 … 5.0 s`) cover both warm hits and cold
//!   upstream-on-miss requests once T6 lands.
//! - `roastery_storage_bytes_total{backend}` — gauge; total bytes
//!   resident in the configured CAS backend. For the filesystem
//!   backend the value is computed lazily by walking `<root>/cas/`
//!   and summing file sizes; the result is cached for 5 seconds so a
//!   tight scrape loop can't turn `/metrics` into an `fts_walk`
//!   bottleneck. For the S3/GCS stubs the value is `0`.
//!
//! The choice of the bare `prometheus` crate over the
//! `metrics`/`metrics-exporter-prometheus` pair is documented inline
//! in [`metrics`] — short version: the v0.1 metric set is small
//! enough that an extra recorder thread and a facade crate don't
//! earn their keep.

use axum::Router;
use axum::routing::get;

use crate::server::AppState;

pub mod health;
pub mod metrics;
pub mod version;

/// Build the ops sub-router.
///
/// Mounts `/healthz`, `/metrics`, and `/version` at the root path
/// (no `/v1/` prefix — these are the canonical SRE locations, not
/// part of the barista protocol versioning). The caller merges this
/// into the top-level router via `Router::merge`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/metrics", get(metrics::metrics_handler))
        .route("/version", get(version::version_handler))
}
