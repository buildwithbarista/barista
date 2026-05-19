//! Server assembly + graceful-shutdown loop.
//!
//! `run` builds an `axum::Router` with a single placeholder route,
//! installs request tracing, and serves it via `axum::serve`. Under
//! the hood `axum::serve` drives `hyper_util`'s
//! `server::conn::auto::Builder`, which negotiates HTTP/1.1 vs HTTP/2
//! per connection. Over plain TCP the connection stays HTTP/1.1 in
//! practice (clients don't speak `h2c` by default); HTTP/2 negotiation
//! kicks in once M5.1 T5 adds TLS + ALPN. The codepath is reserved
//! here so that's a layering change, not a rewrite.
//!
//! ## Extension points
//!
//! Subsequent M5.1 tasks plug in at the marked locations in
//! [`build_router`] and [`run`]:
//!
//! - **T2 (storage):** mount storage routes (`/cas/:hash`, …).
//! - **T3 (barista-protocol):** mount the barista-protocol handler.
//! - **T4 (REAPI gRPC):** mount a `tonic` `Service` via
//!   `Router::merge` (axum 0.8 + tonic 0.14 share `hyper`/`tower`).
//! - **T5 (auth):** wrap the router with an auth `Layer`; switch
//!   the listener to `rustls` once `config.tls` is `Some`.
//! - **T6 (upstream-on-miss):** add a fallback `Layer` that consults
//!   `config.upstream` when storage returns 404.
//! - **T7 (health + metrics):** mount `/healthz`, `/metrics`,
//!   `/version`.

use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{error, info};

use crate::config::ServerConfig;
use crate::error::{Result, RoasteryError};

/// Crate version exposed in the placeholder root response.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build the top-level axum router.
fn build_router() -> Router {
    Router::new()
        .route("/", get(placeholder_root))
        // T2: storage routes (`/cas/:hash`, `/ac/:hash`, …) mount here.
        // T3: barista-protocol routes mount here.
        // T4: REAPI gRPC services merge in via `Router::merge`.
        // T7: `/healthz`, `/metrics`, `/version` mount here.
        .layer(TraceLayer::new_for_http())
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

    let app = build_router();

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

    #[test]
    fn build_router_compiles() {
        let _: Router = build_router();
    }

    #[tokio::test]
    async fn placeholder_root_mentions_roastery() {
        let body = placeholder_root().await;
        assert!(body.contains("roastery"));
        assert!(body.contains(VERSION));
    }
}
