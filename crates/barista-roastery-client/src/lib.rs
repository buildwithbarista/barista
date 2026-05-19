//! Async HTTP/2 client for the roastery cache server's
//! barista-protocol surface.
//!
//! The roastery is a remote artifact cache; its barista-protocol
//! surface is a small, fixed REST/JSON contract over HTTP/2 (or
//! HTTP/1.1 when ALPN doesn't negotiate h2). This crate is the
//! client side of that contract: a single [`RoasteryClient`] that
//! exposes one method per endpoint, with bearer / mTLS / anonymous
//! authentication and rustls-backed TLS.
//!
//! # Quick start
//!
//! ```no_run
//! use std::time::Duration;
//! use barista_roastery_client::{
//!     AuthConfig, ClientConfig, Digest, RoasteryClient, TlsConfig,
//! };
//!
//! # async fn _example() -> Result<(), Box<dyn std::error::Error>> {
//! let base = "https://roastery.example.com:8443".parse()?;
//! let config = ClientConfig::builder(base)
//!     .auth(AuthConfig::Bearer { token: "s3cret".into() })
//!     .tls(TlsConfig::SystemRoots)
//!     .timeout(Duration::from_secs(10))
//!     .build();
//! let client = RoasteryClient::new(config)?;
//!
//! let digest = Digest::from_hex(
//!     "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
//! )?;
//! let mut blob = client.get_blob(digest).await?;
//! println!("blob is {} bytes", blob.stat.size);
//!
//! use tokio::io::AsyncReadExt;
//! let mut bytes = Vec::with_capacity(blob.stat.size as usize);
//! blob.body.read_to_end(&mut bytes).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Endpoints
//!
//! | Method on [`RoasteryClient`]                | Endpoint                              |
//! |---------------------------------------------|---------------------------------------|
//! | [`get_blob`](RoasteryClient::get_blob)      | `GET  /v1/cas/sha256/{digest}`        |
//! | [`stat_blob`](RoasteryClient::stat_blob)    | `HEAD /v1/cas/sha256/{digest}`        |
//! | [`put_blob`](RoasteryClient::put_blob)      | `PUT  /v1/cas/sha256/{digest}`        |
//! | [`missing`](RoasteryClient::missing)        | `POST /v1/cas/missing`                |
//! | [`health`](RoasteryClient::health)          | `GET  /v1/health` (always anonymous)  |
//! | [`capabilities`](RoasteryClient::capabilities) | `GET /v1/capabilities` (anonymous) |
//!
//! # Authentication
//!
//! The server's protected routes (every `/v1/cas/...`) require
//! either a bearer token or a valid mTLS client certificate. The
//! always-public `/v1/health` and `/v1/capabilities` routes accept
//! anonymous requests regardless of how the server is configured —
//! the client honours this by never sending the bearer header to
//! those endpoints.
//!
//! Pick the mechanism via [`AuthConfig`]:
//!
//! - `AuthConfig::Anonymous` — send no credentials.
//! - `AuthConfig::Bearer { token }` — send
//!   `Authorization: Bearer <token>` on protected routes.
//! - `AuthConfig::Mtls { client_cert_pem, client_key_pem }` —
//!   present a client certificate during the TLS handshake.
//!
//! # TLS
//!
//! [`TlsConfig`] controls server-cert verification (and threads
//! through the mTLS client identity from `AuthConfig::Mtls` when
//! present):
//!
//! - `TlsConfig::SystemRoots` — load the platform trust store.
//! - `TlsConfig::CustomCa { ca_cert_pem }` — verify against a
//!   caller-supplied CA bundle.
//! - `TlsConfig::PlainHttp` — no TLS. Refused at construction time
//!   if the base URL is `https://`.
//!
//! # Limitations
//!
//! - No per-request retry / backoff. The caller is responsible for
//!   wrapping the client with retry logic when the use case
//!   requires it (e.g. exponential backoff for transient 5xx). A
//!   retry policy is a planned v0.2 enhancement.
//! - The REAPI gRPC surface is not implemented here — this crate
//!   covers only the barista-native HTTP/2 protocol. A separate
//!   client surface handles REAPI.

pub mod client;
pub mod config;
pub mod digest;
pub mod error;
pub mod tls;
pub mod types;

pub use client::RoasteryClient;
pub use config::{AuthConfig, ClientConfig, ClientConfigBuilder, TlsConfig};
pub use digest::Digest;
pub use error::ClientError;
pub use types::{
    BlobStat, BlobStream, CapabilitiesCas, CapabilitiesResponse, CapabilitiesStorage,
    HealthResponse,
};
