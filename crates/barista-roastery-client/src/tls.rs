// SPDX-License-Identifier: MIT OR Apache-2.0

//! TLS [`rustls::ClientConfig`] construction.
//!
//! Centralises the three TLS modes the public API exposes:
//!
//! - [`TlsConfig::SystemRoots`] — load the platform trust store
//!   via `rustls-native-certs`.
//! - [`TlsConfig::CustomCa`] — parse caller-supplied PEM CA blocks
//!   and build a root store from those.
//! - [`TlsConfig::PlainHttp`] — no TLS at all; this module is never
//!   called in that path (the client takes the non-TLS branch in
//!   `RoasteryClient::new`).
//!
//! When [`AuthConfig::Mtls`] is configured the client cert/key
//! material is threaded into the same `ClientConfig` builder so the
//! resulting config carries the client identity automatically.

use std::io::Cursor;
use std::sync::OnceLock;

use rustls::ClientConfig as RustlsConfig;
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::config::{AuthConfig, TlsConfig};
use crate::error::ClientError;

/// Build a rustls `ClientConfig` from the public [`TlsConfig`] and
/// (optionally) the mTLS client identity carried by [`AuthConfig`].
///
/// The caller passes the [`AuthConfig`] in unchanged; this function
/// only consumes its `Mtls` variant if present, because the client
/// identity is part of the TLS handshake, not an HTTP header. Bearer
/// tokens are added as a header by [`crate::client`] and don't go
/// through this codepath.
pub(crate) fn build_client_config(
    tls: &TlsConfig,
    auth: &AuthConfig,
) -> Result<RustlsConfig, ClientError> {
    ensure_crypto_provider();

    let root_store = match tls {
        TlsConfig::SystemRoots => system_root_store()?,
        TlsConfig::CustomCa { ca_cert_pem } => custom_ca_root_store(ca_cert_pem)?,
        TlsConfig::PlainHttp => {
            // The public API never reaches here — `RoasteryClient::new`
            // takes the non-TLS branch for `PlainHttp`. If we ever do
            // reach this, surface a configuration error rather than
            // silently building an empty root store.
            return Err(ClientError::Config {
                reason: "TlsConfig::PlainHttp does not produce a rustls config".to_string(),
            });
        }
    };

    let builder = RustlsConfig::builder().with_root_certificates(root_store);

    let cfg = match auth {
        AuthConfig::Mtls {
            client_cert_pem,
            client_key_pem,
        } => {
            let certs = parse_certs(client_cert_pem)?;
            let key = parse_private_key(client_key_pem)?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| ClientError::Tls {
                    reason: format!("invalid mTLS client identity: {e}"),
                })?
        }
        AuthConfig::Anonymous | AuthConfig::Bearer { .. } => builder.with_no_client_auth(),
    };

    Ok(cfg)
}

/// Install the rustls process-default crypto provider exactly once.
///
/// Matches the pattern the roastery server uses: any process that
/// links rustls needs a default provider installed before the first
/// `ClientConfig::builder()` call. Calling `install_default` more
/// than once errors silently, so the `OnceLock` is just a courtesy
/// to keep the trace logs quiet.
fn ensure_crypto_provider() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // The provider crate exposes a `Result`-shaped API: ignore
        // the error because being "already installed" is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Build a `RootCertStore` from the operating system's native trust
/// store.
fn system_root_store() -> Result<RootCertStore, ClientError> {
    let mut store = RootCertStore::empty();
    let certs = rustls_native_certs::load_native_certs();
    if certs.certs.is_empty() {
        let err_msg = if certs.errors.is_empty() {
            "no native certificates found".to_string()
        } else {
            certs
                .errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        };
        return Err(ClientError::Tls {
            reason: format!("could not load system trust store: {err_msg}"),
        });
    }
    for cert in certs.certs {
        store.add(cert).map_err(|e| ClientError::Tls {
            reason: format!("invalid native certificate: {e}"),
        })?;
    }
    Ok(store)
}

/// Build a `RootCertStore` from a caller-supplied PEM CA bundle.
fn custom_ca_root_store(pem: &[u8]) -> Result<RootCertStore, ClientError> {
    let mut store = RootCertStore::empty();
    let mut count = 0usize;
    for cert in rustls_pemfile::certs(&mut Cursor::new(pem)) {
        let cert: CertificateDer<'static> = cert.map_err(|e| ClientError::Tls {
            reason: format!("PEM parse error: {e}"),
        })?;
        store.add(cert).map_err(|e| ClientError::Tls {
            reason: format!("invalid CA certificate: {e}"),
        })?;
        count += 1;
    }
    if count == 0 {
        return Err(ClientError::Tls {
            reason: "CustomCa PEM bundle contained no certificates".to_string(),
        });
    }
    Ok(store)
}

/// Parse a PEM-encoded client certificate chain into rustls
/// `CertificateDer` values.
fn parse_certs(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, ClientError> {
    let mut out = Vec::new();
    for cert in rustls_pemfile::certs(&mut Cursor::new(pem)) {
        let cert = cert.map_err(|e| ClientError::Tls {
            reason: format!("client cert PEM parse error: {e}"),
        })?;
        out.push(cert);
    }
    if out.is_empty() {
        return Err(ClientError::Tls {
            reason: "mTLS client cert PEM contained no certificates".to_string(),
        });
    }
    Ok(out)
}

/// Parse a PEM-encoded private key into a rustls `PrivateKeyDer`.
///
/// `rustls_pemfile::private_key` handles PKCS#1, PKCS#8, and SEC1
/// formats — whatever a sensibly-encoded key file contains.
fn parse_private_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>, ClientError> {
    rustls_pemfile::private_key(&mut Cursor::new(pem))
        .map_err(|e| ClientError::Tls {
            reason: format!("client key PEM parse error: {e}"),
        })?
        .ok_or_else(|| ClientError::Tls {
            reason: "mTLS client key PEM contained no private key".to_string(),
        })
}
