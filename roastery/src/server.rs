//! Server assembly + graceful-shutdown loop.
//!
//! `run` instantiates the configured CAS backend, wraps it in
//! [`AppState`], builds an `axum::Router` carrying that state, installs
//! request tracing, and serves it via `axum::serve`. Under the hood
//! `axum::serve` drives `hyper_util`'s `server::conn::auto::Builder`,
//! which negotiates HTTP/1.1 vs HTTP/2 per connection. Over plain TCP
//! the connection stays HTTP/1.1 in practice (clients don't speak
//! `h2c` by default); HTTP/2 negotiation kicks in once M5.1 T5 adds
//! TLS + ALPN. The codepath is reserved here so that's a layering
//! change, not a rewrite.
//!
//! ## Extension points
//!
//! Subsequent M5.1 tasks plug in at the marked locations in
//! [`build_router`] and [`run`]:
//!
//! - **T2 (storage):** the CAS backend is instantiated in [`run`] and
//!   carried on [`AppState`]. T3 and T4 read it from the router state.
//! - **T3 (barista-protocol):** mount the barista-protocol handler.
//!   It will take `State<AppState>` and call `state.cas` directly.
//! - **T4 (REAPI gRPC):** mount a `tonic` `Service` via
//!   `Router::merge` (axum 0.8 + tonic 0.14 share `hyper`/`tower`).
//!   Same `AppState` story.
//! - **T5 (auth):** wrap the router with an auth `Layer`; switch
//!   the listener to `rustls` once `config.tls` is `Some`.
//! - **T6 (upstream-on-miss):** add a fallback `Layer` that consults
//!   `config.upstream` when storage returns 404.
//! - **T7 (health + metrics):** mount `/healthz`, `/metrics`,
//!   `/version`.

use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{error, info};

use crate::config::{ServerConfig, StorageBackend};
use crate::error::{Result, RoasteryError};
use crate::storage::{Cas, FsCas, GcsCas, S3Cas};

/// Crate version exposed in the placeholder root response.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared application state passed through the axum router.
///
/// Contains the boxed CAS backend (so the trait stays object-safe and
/// the same router code drives `FsCas` in production and a mock or
/// in-memory backend in tests) plus the resolved [`ServerConfig`] so
/// handlers can introspect the deployment shape (e.g. the
/// barista-protocol `capabilities` endpoint reports the storage
/// backend discriminant). Cheap to clone (`Arc` bumps); axum clones
/// state per request handler when needed.
#[derive(Clone)]
pub struct AppState {
    /// Content-addressed storage backend. Constructed from
    /// [`ServerConfig::storage`] in [`run`].
    pub cas: Arc<dyn Cas>,
    /// Resolved server configuration. Shared by `Arc` so handlers can
    /// read fields without copying the whole struct on every request.
    pub config: Arc<ServerConfig>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn Cas` is not `Debug`; print a stable placeholder so
        // tracing macros that include `AppState` in their payload
        // don't fail to compile.
        f.debug_struct("AppState")
            .field("cas", &"<dyn Cas>")
            .field("config", &self.config)
            .finish()
    }
}

/// Instantiate the CAS backend described by `backend`.
///
/// This is the single place the server picks between filesystem, S3
/// (stub), and GCS (stub). T3 and T4 read from `AppState::cas`
/// without caring which backend is underneath.
fn build_cas(backend: &StorageBackend) -> Result<Arc<dyn Cas>> {
    // The `let _: Arc<dyn Cas> = Arc::new(concrete)` pattern triggers
    // Rust's unsized-coercion rule for `Arc<T>`-to-`Arc<dyn Trait>`
    // without resorting to an `as` cast, which the workspace lint
    // policy forbids.
    match backend {
        StorageBackend::Filesystem(root) => {
            let fs = FsCas::new(root.clone())?;
            let cas: Arc<dyn Cas> = Arc::new(fs);
            Ok(cas)
        }
        StorageBackend::S3 { bucket, region } => {
            let s3 = S3Cas::new(bucket.clone(), region.clone())?;
            let cas: Arc<dyn Cas> = Arc::new(s3);
            Ok(cas)
        }
        StorageBackend::Gcs { bucket, project } => {
            let gcs = GcsCas::new(bucket.clone(), project.clone())?;
            let cas: Arc<dyn Cas> = Arc::new(gcs);
            Ok(cas)
        }
    }
}

/// Build the top-level axum router.
///
/// Takes the shared [`AppState`] so subsequent tasks can mount
/// handlers that consume it (`axum::extract::State<AppState>`). The
/// scaffold itself still serves only the placeholder root.
fn build_router(state: AppState) -> Router {
    // The barista-protocol surface (mounted under `/v1/…`) lives in
    // its own sub-router so the wire-protocol concerns stay isolated
    // from this assembly. `Router::merge` composes both sub-routers
    // into one — they share the listener, the `TraceLayer`, and the
    // graceful-shutdown path. The merged router applies the shared
    // `AppState` exactly once at the bottom of the chain.
    Router::new()
        .route("/", get(placeholder_root))
        // T2: the CAS backend lives on `state.cas`, ready for T3 and
        // T4 to mount handlers that call it.
        // T3: barista-protocol routes (`/v1/…`).
        // T4: REAPI gRPC services merge in via `Router::merge`.
        // T7: `/healthz`, `/metrics`, `/version` — the ops surface
        // distinct from the protocol-level `/v1/health`.
        .merge(crate::proto::barista::router())
        .merge(crate::ops::router())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
    // T5: wrap with auth `Layer` once auth lands.
    // T6: wrap with upstream-on-miss `Layer` once storage lands.
}

/// Placeholder handler for `GET /`.
///
/// Returns a short text body so an operator running the scaffold can
/// confirm the server is up. Replaced/augmented by T7's health
/// endpoints in a later task.
async fn placeholder_root() -> String {
    format!("roastery {VERSION} scaffold\n")
}

/// Run the roastery server until a shutdown signal is received.
///
/// On Unix the server shuts down gracefully on either `SIGINT`
/// (Ctrl-C) or `SIGTERM`. On other platforms only Ctrl-C is wired;
/// SIGTERM is a Unix-only concept.
pub async fn run(config: ServerConfig) -> Result<()> {
    config.validate()?;

    // Register the ops/metrics collectors against the Prometheus
    // default registry before we accept a single connection. `init`
    // is idempotent — calling it from a test that also drove `run`
    // earlier is fine — so we don't need to gate the call on a
    // "first time" flag here. Doing this up-front avoids the race
    // where the first `/metrics` scrape hits an empty registry.
    crate::ops::metrics::init();

    // Build the CAS backend up front so a misconfigured storage layer
    // is a startup error, not a first-request error.
    let cas = build_cas(&config.storage)?;
    let state = AppState {
        cas,
        config: Arc::new(config.clone()),
    };

    let listener = TcpListener::bind(config.bind)
        .await
        .map_err(|source| RoasteryError::Bind {
            addr: config.bind,
            source,
        })?;
    let local_addr = listener
        .local_addr()
        .map_err(|source| RoasteryError::Io { source })?;

    info!(
        addr = %local_addr,
        version = VERSION,
        "roastery listening (scaffold; HTTP/1.1 + HTTP/2 via hyper-util auto)"
    );

    let app = build_router(state);

    // `axum::serve` wraps `hyper_util::server::conn::auto::Builder`
    // under the hood; passing the `Router` directly preserves the
    // dual HTTP/1.1 + HTTP/2 codepath without us having to wire the
    // service-fn / executor plumbing by hand.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|source| RoasteryError::Io { source })?;

    info!("roastery shutdown complete");
    Ok(())
}

/// Future that resolves when the process should shut down.
///
/// Resolves on `SIGINT` (Ctrl-C) on every platform and on `SIGTERM`
/// on Unix. If a handler fails to install we log and fall back to
/// the remaining signal.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            error!(error = %err, "failed to install Ctrl-C handler");
            // Pend forever rather than spinning in `select!`.
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                error!(error = %err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => info!("Ctrl-C received; initiating graceful shutdown"),
        () = terminate => info!("SIGTERM received; initiating graceful shutdown"),
    }
}

/// Initialise the global `tracing` subscriber.
///
/// Honours `RUST_LOG` via `tracing_subscriber::EnvFilter`; defaults
/// to `info` for the roastery crate when unset. Safe to call more
/// than once — `try_init` returns an error on a second call and we
/// deliberately swallow it.
pub fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,roastery=info"));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .try_init();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use tempfile::TempDir;

    fn fixture_state() -> (TempDir, AppState) {
        let tmp = TempDir::new().unwrap();
        let cas = FsCas::new(tmp.path().to_path_buf()).unwrap();
        let cas: Arc<dyn Cas> = Arc::new(cas);
        let config = ServerConfig::with_bind("127.0.0.1:0".parse().unwrap());
        let state = AppState {
            cas,
            config: Arc::new(config),
        };
        (tmp, state)
    }

    #[test]
    fn build_router_compiles() {
        let (_tmp, state) = fixture_state();
        let _: Router = build_router(state);
    }

    #[tokio::test]
    async fn placeholder_root_mentions_roastery() {
        let body = placeholder_root().await;
        assert!(body.contains("roastery"));
        assert!(body.contains(VERSION));
    }

    #[test]
    fn build_cas_picks_filesystem_for_filesystem_backend() {
        let tmp = TempDir::new().unwrap();
        let backend = StorageBackend::Filesystem(tmp.path().to_path_buf());
        let cas = build_cas(&backend).unwrap();
        // We can't downcast through `dyn Cas` without `Any`, but the
        // construction succeeding (and the CAS root existing) is the
        // observable contract.
        assert!(tmp.path().join("cas").is_dir());
        assert!(tmp.path().join("tmp").is_dir());
        // Drop the Arc so the temp dir can be cleaned up.
        drop(cas);
    }

    #[test]
    fn build_cas_constructs_s3_stub() {
        let backend = StorageBackend::S3 {
            bucket: "b".to_string(),
            region: "r".to_string(),
        };
        let _cas = build_cas(&backend).unwrap();
    }

    #[test]
    fn build_cas_constructs_gcs_stub() {
        let backend = StorageBackend::Gcs {
            bucket: "b".to_string(),
            project: "p".to_string(),
        };
        let _cas = build_cas(&backend).unwrap();
    }
}
