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

use crate::auth::{AuthLayer, BearerVerifier, MtlsVerifier};
use crate::config::{ServerConfig, StorageBackend};
use crate::error::{Result, RoasteryError};
use crate::storage::{Cas, FsCas, GcsCas, S3Cas};
use crate::upstream::UpstreamFetcher;

/// Crate version exposed in the placeholder root response.
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

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
    /// Optional upstream-on-miss fetcher. `Some` when
    /// [`crate::config::UpstreamConfig::fetch_missing`] is true and
    /// at least one upstream repository is configured; `None`
    /// otherwise. Wrapped in `Arc` because the fetcher carries a
    /// `reqwest::Client` we don't want to clone per request.
    pub upstream: Option<Arc<UpstreamFetcher>>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn Cas` is not `Debug`; print a stable placeholder so
        // tracing macros that include `AppState` in their payload
        // don't fail to compile.
        f.debug_struct("AppState")
            .field("cas", &"<dyn Cas>")
            .field("config", &self.config)
            .field(
                "upstream",
                &self
                    .upstream
                    .as_ref()
                    .map(|_| "<UpstreamFetcher>")
                    .unwrap_or("none"),
            )
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

/// Build the top-level axum router with an auth layer on the
/// protected routes.
///
/// Topology:
///
/// ```text
/// Router::new()
///     ├── /                          (public — scaffold root)
///     ├── ops::router()              (/healthz, /metrics, /version — always public)
///     ├── proto::barista::public_router()      (/v1/health, /v1/capabilities)
///     └── proto::barista::protected_router()   (/v1/cas/...) ← AuthLayer
/// ```
///
/// The auth layer wraps only the protected sub-router. Ops + the
/// protocol-level public surface bypass it so k8s probes,
/// Prometheus scrapes, and version-negotiation clients can talk to
/// the server without credentials.
///
/// `AppState` is applied once to each sub-router with
/// `with_state` and the result is merged into the top-level
/// router. axum's router-merge composition preserves the layers
/// applied to each branch.
pub(crate) fn build_router(state: AppState, auth_layer: AuthLayer) -> Router {
    let protected = crate::proto::barista::protected_router()
        .with_state(state.clone())
        .layer(auth_layer);

    Router::new()
        .route("/", get(placeholder_root))
        .merge(crate::proto::barista::public_router().with_state(state.clone()))
        .merge(crate::ops::router().with_state(state))
        .merge(protected)
        .layer(TraceLayer::new_for_http())
    // T6: upstream-on-miss layer.
    //
    // Implemented inline in the `cas_get` handler (see
    // `proto/barista.rs`) rather than as a tower `Layer`, because the
    // fetcher needs access to the parsed digest path parameter and
    // the request's `X-Barista-Coords` header to know what to fetch
    // — both of which the handler already has in scope. A `Layer`
    // would have to re-parse the path or thread state through an
    // extension, with no extra benefit. The fetcher itself lives in
    // `crate::upstream` and is stashed on `AppState::upstream`.
}

/// Construct the upstream-on-miss fetcher from the resolved server
/// configuration.
///
/// Returns `Ok(None)` when the operator has not opted into the
/// feature (`fetch_missing = false` or empty `repos`). Returns
/// `Ok(Some(_))` when the fetcher is enabled and the `reqwest::Client`
/// builds cleanly. Surfaces a [`RoasteryError::Config`] only on a
/// truly malformed configuration — the `validate()` precheck has
/// already caught the obvious "fetch on, repos empty" case.
fn build_upstream(
    config: &ServerConfig,
    cas: Arc<dyn Cas>,
) -> Result<Option<Arc<UpstreamFetcher>>> {
    if !config.upstream.fetch_missing || config.upstream.repos.is_empty() {
        return Ok(None);
    }
    let timeout = std::time::Duration::from_secs(u64::from(config.upstream.timeout_secs));
    let fetcher = UpstreamFetcher::new(config.upstream.repos.clone(), timeout, cas).map_err(
        |e| RoasteryError::Config(format!("failed to build upstream HTTP client: {e}")),
    )?;
    info!(
        repos = config.upstream.repos.len(),
        timeout_secs = config.upstream.timeout_secs,
        "upstream-on-miss enabled"
    );
    Ok(Some(Arc::new(fetcher)))
}

/// Resolve the configured auth verifiers from disk + assemble the
/// [`AuthLayer`].
///
/// Returns the layer plus owned `Arc` handles to the verifiers (so
/// the caller can stash them on `AppState` if a future task wants
/// runtime introspection — today they live only inside the layer).
/// Surfaces a `RoasteryError::Config` on a file-read or parse
/// failure, attributed to the operator-supplied path.
fn build_auth(config: &ServerConfig) -> Result<AuthLayer> {
    let bearer = match &config.auth.bearer {
        Some(b) => {
            let v = BearerVerifier::load(&b.tokens_file)?;
            info!(
                source = %b.tokens_file.display(),
                count = v.entry_count(),
                "auth: loaded bearer tokens"
            );
            Some(Arc::new(v))
        }
        None => None,
    };
    let mtls = match &config.auth.mtls {
        Some(m) => {
            let v = MtlsVerifier::load_ca(&m.ca_cert)?;
            info!(
                source = %m.ca_cert.display(),
                roots = v.root_count(),
                "auth: loaded mTLS CA"
            );
            Some(Arc::new(v))
        }
        None => None,
    };
    let layer = AuthLayer::new(bearer, mtls);
    if layer.allows_anonymous() {
        // Loopback-bound dev workflow; the validate() check above
        // already verified this state is only possible with a
        // loopback bind.
        info!("auth: no mechanism configured — accepting anonymous requests (loopback only)");
    }
    Ok(layer)
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
///
/// Picks between plain TCP and TLS based on `config.tls`:
/// `None` → `axum::serve` over a `TcpListener`; `Some` →
/// [`crate::server::tls::run_tls`] (a `rustls`-terminated listener
/// that captures the client cert chain for the mTLS auth path).
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

    // T6: upstream-on-miss layer. Build the fetcher (if enabled) at
    // startup so a bad upstream URL or a broken rustls config is a
    // startup error, not a per-request surprise. The fetcher shares
    // the same `Arc<dyn Cas>` as the rest of the server: a successful
    // upstream fetch persists into the same store the handlers read
    // from.
    let upstream = build_upstream(&config, cas.clone())?;
    let state = AppState {
        cas,
        config: Arc::new(config.clone()),
        upstream,
    };
    let auth_layer = build_auth(&config)?;

    if config.tls.is_some() {
        tls::run_tls(config, state, auth_layer).await
    } else {
        run_plain(config, state, auth_layer).await
    }
}

/// Plain-TCP (no TLS) listener loop. Used when `config.tls` is
/// `None`. mTLS is rejected at `validate()` time when TLS is off, so
/// this path never sees a client cert.
async fn run_plain(config: ServerConfig, state: AppState, auth_layer: AuthLayer) -> Result<()> {
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
        tls = false,
        "roastery listening (HTTP/1.1 + HTTP/2 via hyper-util auto)"
    );

    let app = build_router(state, auth_layer);

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

/// TLS listener path: terminates HTTPS with `rustls` and (when
/// mTLS is configured) captures the client certificate chain for
/// the auth layer.
///
/// Lives in a submodule to keep the rustls + axum-server surface
/// out of the plain-TCP path's import set. The plain path doesn't
/// need any of these types.
pub mod tls {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use axum::http::Request;
    use axum_server::accept::Accept;
    use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
    use rustls::pki_types::CertificateDer;
    use tokio::io::{AsyncRead, AsyncWrite};
    use tower::Service;
    use tracing::{debug, info};

    use crate::auth::{AuthLayer, ClientCertChain};
    use crate::config::ServerConfig;
    use crate::error::{Result, RoasteryError};
    use crate::server::{AppState, VERSION, build_router};

    /// Build + run a TLS-terminated server.
    ///
    /// Installs the `ring` rustls crypto provider as the process
    /// default before constructing the server config. The install
    /// is idempotent across calls — `set_default` on an already-
    /// set provider returns `Err`, which we swallow so the test
    /// suite (which may call `run_tls` more than once across
    /// processes-with-shared-state edge cases) doesn't trip on it.
    pub async fn run_tls(
        config: ServerConfig,
        state: AppState,
        auth_layer: AuthLayer,
    ) -> Result<()> {
        // Install the crypto provider once. The `axum-server`
        // feature `tls-rustls-no-provider` deliberately leaves this
        // up to us so we can pick the implementation.
        let _ = rustls::crypto::ring::default_provider().install_default();

        // We expect `config.tls` to be `Some` because the caller
        // checked. Defensive `ok_or` so a future refactor that
        // bypasses the check surfaces a clean error.
        let _tls = config.tls.as_ref().ok_or_else(|| {
            RoasteryError::Config("run_tls called without a TLS config".to_string())
        })?;

        // Build a `rustls::ServerConfig` by hand so we can plug in
        // the optional mTLS client verifier when configured.
        // `axum_server::tls_rustls::RustlsConfig::from_pem_file`
        // would do the cert/key part but doesn't expose the
        // client-verifier slot.
        let server_config = build_rustls_server_config(&config, auth_layer.clone()).await?;
        let rustls_cfg = RustlsConfig::from_config(Arc::new(server_config));

        let app = build_router(state, auth_layer);

        // Custom acceptor: wraps the `RustlsAcceptor` so we can
        // pluck peer certs off the completed handshake and inject
        // them into every request through that connection.
        let acceptor = CertCapturingAcceptor::new(RustlsAcceptor::new(rustls_cfg));

        info!(
            addr = %config.bind,
            version = VERSION,
            tls = true,
            "roastery listening (HTTPS via rustls)"
        );

        axum_server::bind(config.bind)
            .acceptor(acceptor)
            .serve(app.into_make_service())
            .await
            .map_err(|source| RoasteryError::Io { source })?;

        info!("roastery shutdown complete");
        Ok(())
    }

    /// Build a `rustls::ServerConfig` for `axum-server`.
    ///
    /// Reads the PEM cert chain + private key supplied by the
    /// operator. If [`crate::auth::layer::AuthLayer`] carries an
    /// mTLS verifier (we re-read it from the layer for
    /// configurational coherence), the rustls config requires a
    /// client cert chained to that CA on every handshake.
    async fn build_rustls_server_config(
        config: &ServerConfig,
        auth_layer: AuthLayer,
    ) -> Result<rustls::ServerConfig> {
        let tls = config
            .tls
            .as_ref()
            .ok_or_else(|| RoasteryError::Config("missing TLS config".to_string()))?;

        // Load cert chain.
        let cert_pem = tokio::fs::read(&tls.cert_path).await.map_err(|e| {
            RoasteryError::Config(format!(
                "cannot read TLS cert {}: {e}",
                tls.cert_path.display()
            ))
        })?;
        let key_pem = tokio::fs::read(&tls.key_path).await.map_err(|e| {
            RoasteryError::Config(format!(
                "cannot read TLS key {}: {e}",
                tls.key_path.display()
            ))
        })?;

        let cert_chain: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut std::io::Cursor::new(&cert_pem))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| {
                    RoasteryError::Config(format!(
                        "failed to parse TLS cert {}: {e}",
                        tls.cert_path.display()
                    ))
                })?;
        if cert_chain.is_empty() {
            return Err(RoasteryError::Config(format!(
                "TLS cert file {} contained no certificates",
                tls.cert_path.display()
            )));
        }
        let key = rustls_pemfile::private_key(&mut std::io::Cursor::new(&key_pem))
            .map_err(|e| {
                RoasteryError::Config(format!(
                    "failed to parse TLS key {}: {e}",
                    tls.key_path.display()
                ))
            })?
            .ok_or_else(|| {
                RoasteryError::Config(format!(
                    "TLS key file {} contained no usable private key",
                    tls.key_path.display()
                ))
            })?;

        // Pick between vanilla TLS and mTLS by inspecting the
        // mTLS portion of the resolved config (the auth layer
        // already loaded it).
        let builder = rustls::ServerConfig::builder();
        let mut server_config = if let Some(m) = &config.auth.mtls {
            // Re-load the CA — we want the rustls
            // `WebPkiClientVerifier` here, not the layer's wrapper
            // around it.  The path was already validated by
            // ServerConfig::validate.
            let mtls_verifier = crate::auth::MtlsVerifier::load_ca(&m.ca_cert)?;
            builder
                .with_client_cert_verifier(mtls_verifier.verifier())
                .with_single_cert(cert_chain, key)
                .map_err(|e| {
                    RoasteryError::Config(format!("failed to build TLS server config: {e}"))
                })?
        } else {
            builder
                .with_no_client_auth()
                .with_single_cert(cert_chain, key)
                .map_err(|e| {
                    RoasteryError::Config(format!("failed to build TLS server config: {e}"))
                })?
        };

        // ALPN: advertise h2 + http/1.1 so HTTP/2 negotiation works
        // over TLS.
        server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        // `auth_layer` is captured here only so the function
        // signature documents the dependency; rustls itself doesn't
        // consume it.  Avoid an unused-variable warning by binding
        // explicitly.
        let _ = auth_layer;

        Ok(server_config)
    }

    // -----------------------------------------------------------------
    // Cert-capturing Accept wrapper
    // -----------------------------------------------------------------

    /// `axum-server` `Accept` impl that wraps `RustlsAcceptor` and
    /// snapshots the peer cert chain from the completed TLS
    /// handshake into a [`ClientCertChain`] request extension.
    ///
    /// The chain is captured once per connection (TLS handshakes
    /// don't re-key the peer cert) and injected into every request
    /// served through that connection via [`InjectCertChain`].
    #[derive(Clone)]
    pub struct CertCapturingAcceptor {
        inner: RustlsAcceptor,
    }

    impl CertCapturingAcceptor {
        /// Wrap a `RustlsAcceptor`.
        pub fn new(inner: RustlsAcceptor) -> Self {
            Self { inner }
        }
    }

    impl<I, S> Accept<I, S> for CertCapturingAcceptor
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        S: Send + 'static,
    {
        type Stream = tokio_rustls::server::TlsStream<I>;
        type Service = InjectCertChain<S>;
        type Future = Pin<
            Box<
                dyn Future<Output = std::io::Result<(Self::Stream, Self::Service)>>
                    + Send
                    + 'static,
            >,
        >;

        fn accept(&self, stream: I, service: S) -> Self::Future {
            let fut = self.inner.accept(stream, service);
            Box::pin(async move {
                let (tls_stream, service) = fut.await?;
                // Pull the peer-cert chain off the completed
                // handshake.  When the rustls server config was
                // built without `with_client_cert_verifier` (vanilla
                // TLS, no mTLS) this is `None` and we attach an
                // empty chain — the auth layer treats that as
                // "mTLS not satisfied" but bearer can still
                // succeed.
                let chain: Vec<CertificateDer<'static>> = tls_stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .map(|certs| certs.iter().map(|c| c.clone().into_owned()).collect())
                    .unwrap_or_default();
                debug!(certs = chain.len(), "tls: captured peer cert chain");
                let chain = ClientCertChain(Arc::new(chain));
                Ok((tls_stream, InjectCertChain { inner: service, chain }))
            })
        }
    }

    /// Service wrapper that attaches a [`ClientCertChain`] extension
    /// to every request passing through. Created once per
    /// connection by [`CertCapturingAcceptor::accept`].
    #[derive(Clone)]
    pub struct InjectCertChain<S> {
        inner: S,
        chain: ClientCertChain,
    }

    impl<S, B> Service<Request<B>> for InjectCertChain<S>
    where
        S: Service<Request<B>> + Clone + Send + 'static,
        S::Future: Send + 'static,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), S::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, mut req: Request<B>) -> Self::Future {
            req.extensions_mut().insert(self.chain.clone());
            self.inner.call(req)
        }
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
            upstream: None,
        };
        (tmp, state)
    }

    #[test]
    fn build_router_compiles() {
        let (_tmp, state) = fixture_state();
        let _: Router = build_router(state, AuthLayer::new(None, None));
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
