//! Configuration types for [`crate::RoasteryClient`].
//!
//! [`ClientConfig`] is the single struct passed to
//! [`crate::RoasteryClient::new`]. It carries the base URL, the auth
//! mechanism, the TLS trust configuration, and a few request-shaping
//! knobs (timeout, user-agent string, batch cap).
//!
//! Build it directly (every field is public) or use
//! [`ClientConfig::builder`] for the more ergonomic builder flow.

use std::time::Duration;

use url::Url;

/// Bundle of every option a [`RoasteryClient`](crate::RoasteryClient)
/// reads at construction time.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Base URL the client points at, e.g. `https://roastery.example.com:8443`.
    /// All endpoint paths (`/v1/cas/...`, `/v1/health`,
    /// `/v1/capabilities`) are appended to this base.
    pub base_url: Url,
    /// Which authentication mechanism (if any) the client presents.
    pub auth: AuthConfig,
    /// How the client validates the server's TLS certificate (and,
    /// for mTLS, what client identity the client presents).
    pub tls: TlsConfig,
    /// Per-request total timeout — covers connect + TLS handshake +
    /// headers + body. Default 30 seconds.
    pub timeout: Duration,
    /// `User-Agent` header value. Default
    /// `"barista-roastery-client/<crate-version>"`.
    pub user_agent: String,
    /// Maximum number of digest entries the client will send in a
    /// single `/v1/cas/missing` request. The server caps at 1000;
    /// going above that risks a 413. Default 1000 to match.
    pub max_batch_missing: usize,
}

impl ClientConfig {
    /// Start building a `ClientConfig` for the given base URL.
    ///
    /// All other fields start at their documented defaults
    /// (anonymous auth, system roots for TLS, 30-second timeout,
    /// crate-version user agent, 1000-entry batch cap).
    pub fn builder(base_url: Url) -> ClientConfigBuilder {
        ClientConfigBuilder::new(base_url)
    }
}

/// Authentication mechanism the client presents to the server.
///
/// The server's auth layer accepts either bearer or mTLS (or
/// both); the client picks one. `Anonymous` is fine when the
/// server hasn't been configured with any auth — the always-public
/// `/v1/health` and `/v1/capabilities` endpoints accept anonymous
/// clients regardless.
#[derive(Debug, Clone)]
pub enum AuthConfig {
    /// Don't send any credentials. Works against unsecured servers
    /// and the always-public health/capabilities endpoints; fails
    /// 401 against protected CAS routes when the server requires
    /// auth.
    Anonymous,
    /// Send `Authorization: Bearer <token>` on every protected
    /// request. The token is compared against the server's
    /// SHA-256-hashed token list.
    Bearer {
        /// The bearer token. The client never logs the token; it
        /// flows directly into the `Authorization` header.
        token: String,
    },
    /// Authenticate via mutual TLS — present the supplied client
    /// certificate and private key during the TLS handshake. The
    /// server validates the chain against its configured CA bundle.
    Mtls {
        /// PEM-encoded client certificate chain. The leaf cert
        /// comes first; intermediates (if any) follow.
        client_cert_pem: Vec<u8>,
        /// PEM-encoded private key matching the leaf certificate.
        client_key_pem: Vec<u8>,
    },
}

/// How the client validates the server's TLS certificate.
#[derive(Debug, Clone)]
pub enum TlsConfig {
    /// Use the operating-system's native trust store. Suitable for
    /// production deployments behind a CA-issued certificate.
    SystemRoots,
    /// Use a caller-supplied CA bundle. Suitable for self-signed
    /// or private-CA roastery deployments.
    CustomCa {
        /// PEM-encoded CA certificate(s). May contain multiple
        /// concatenated PEM blocks.
        ca_cert_pem: Vec<u8>,
    },
    /// Plain HTTP — no TLS. Refused against `https://` base URLs
    /// at construction time. Intended for development and
    /// integration tests against a loopback server.
    PlainHttp,
}

/// Builder for [`ClientConfig`].
///
/// Constructed via [`ClientConfig::builder`]. Every setter takes
/// `mut self` and returns `self`, so calls chain.
#[derive(Debug, Clone)]
pub struct ClientConfigBuilder {
    base_url: Url,
    auth: AuthConfig,
    tls: TlsConfig,
    timeout: Duration,
    user_agent: String,
    max_batch_missing: usize,
}

impl ClientConfigBuilder {
    /// Start a builder for the given base URL.
    pub fn new(base_url: Url) -> Self {
        Self {
            base_url,
            auth: AuthConfig::Anonymous,
            tls: TlsConfig::SystemRoots,
            timeout: Duration::from_secs(30),
            user_agent: default_user_agent(),
            max_batch_missing: 1000,
        }
    }

    /// Set the authentication mechanism.
    pub fn auth(mut self, auth: AuthConfig) -> Self {
        self.auth = auth;
        self
    }

    /// Set the TLS configuration.
    pub fn tls(mut self, tls: TlsConfig) -> Self {
        self.tls = tls;
        self
    }

    /// Set the per-request timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the User-Agent header value.
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    /// Set the maximum number of digest entries the client will
    /// pack into a single `/v1/cas/missing` request.
    pub fn max_batch_missing(mut self, cap: usize) -> Self {
        self.max_batch_missing = cap;
        self
    }

    /// Finalise the builder into a [`ClientConfig`].
    pub fn build(self) -> ClientConfig {
        ClientConfig {
            base_url: self.base_url,
            auth: self.auth,
            tls: self.tls,
            timeout: self.timeout,
            user_agent: self.user_agent,
            max_batch_missing: self.max_batch_missing,
        }
    }
}

/// Default User-Agent string, embedding the crate version.
fn default_user_agent() -> String {
    format!("barista-roastery-client/{}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn builder_sets_documented_defaults() {
        let url: Url = "https://roastery.example.com:8443".parse().unwrap();
        let cfg = ClientConfig::builder(url.clone()).build();
        assert_eq!(cfg.base_url, url);
        assert!(matches!(cfg.auth, AuthConfig::Anonymous));
        assert!(matches!(cfg.tls, TlsConfig::SystemRoots));
        assert_eq!(cfg.timeout, Duration::from_secs(30));
        assert_eq!(cfg.max_batch_missing, 1000);
        assert!(cfg.user_agent.starts_with("barista-roastery-client/"));
    }

    #[test]
    fn builder_chains_setters() {
        let url: Url = "http://127.0.0.1:8080".parse().unwrap();
        let cfg = ClientConfig::builder(url)
            .auth(AuthConfig::Bearer {
                token: "secret".into(),
            })
            .tls(TlsConfig::PlainHttp)
            .timeout(Duration::from_millis(500))
            .user_agent("custom-agent/1.0")
            .max_batch_missing(200)
            .build();
        assert!(matches!(cfg.auth, AuthConfig::Bearer { .. }));
        assert!(matches!(cfg.tls, TlsConfig::PlainHttp));
        assert_eq!(cfg.timeout, Duration::from_millis(500));
        assert_eq!(cfg.user_agent, "custom-agent/1.0");
        assert_eq!(cfg.max_batch_missing, 200);
    }
}
