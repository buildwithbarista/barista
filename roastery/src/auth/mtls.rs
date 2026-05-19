//! mTLS verification — load the operator-supplied CA bundle and
//! configure rustls to demand-and-validate a client certificate on
//! every TLS handshake.
//!
//! The flow when mTLS is configured looks like:
//!
//! 1. **Startup** — [`MtlsVerifier::load_ca`] reads + parses the
//!    CA PEM bundle into a `rustls::RootCertStore`, then wraps it in
//!    a `WebPkiClientVerifier`. A client that fails to present a
//!    cert during the handshake, or whose cert can't be chained to
//!    one of these roots, gets rejected at the TLS layer — the
//!    request never reaches axum / hyper / the auth middleware.
//!
//! 2. **Per-connection** — once the handshake succeeds the
//!    `tokio_rustls::server::TlsStream` exposes the peer's cert
//!    chain via `get_ref().1.peer_certificates()`. The connection
//!    acceptor (see `crate::server`) snapshots that chain and stuffs
//!    it into a per-connection [`crate::auth::layer::ClientCertChain`]
//!    request extension so the auth layer can read it without
//!    re-traversing hyper internals.
//!
//! 3. **Per-request** — the auth layer pulls the chain extension,
//!    parses the leaf cert's Subject CN or first URI SAN with
//!    [`subject_from_cert`], and emits
//!    [`crate::auth::Principal::Mtls`].
//!
//! ## What this module is *not*
//!
//! It is not an X.509 implementation. The chain-validation work
//! (signature checks, basic-constraints, expiry, etc.) is rustls'
//! `WebPkiClientVerifier`. We only re-parse the already-validated
//! cert here to extract the subject string — that piece needs the
//! full DER decoder `x509-parser` provides and is not part of
//! rustls's exposed API.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::RootCertStore;
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::ClientCertVerifier;
use rustls::pki_types::CertificateDer;

use crate::error::RoasteryError;

/// mTLS verifier holding the parsed CA root store + the built
/// `ClientCertVerifier` rustls will plug into its server config.
///
/// Cheap to clone — the underlying `ClientCertVerifier` is already
/// `Arc<dyn …>`. `Clone` lets `ServerConfig` propagate the verifier
/// without re-reading the PEM bundle.
#[derive(Clone)]
pub struct MtlsVerifier {
    verifier: Arc<dyn ClientCertVerifier>,
    /// Path the CA was loaded from, kept for diagnostics + the
    /// future reload codepath.
    source: PathBuf,
    /// Number of distinct trust anchors loaded from `source`.
    /// Surfaced for tests + structured-log output; not load-bearing
    /// for verification.
    root_count: usize,
}

impl std::fmt::Debug for MtlsVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MtlsVerifier")
            .field("source", &self.source)
            .field("root_count", &self.root_count)
            .finish()
    }
}

impl MtlsVerifier {
    /// Load a CA bundle from a PEM file and build the rustls client
    /// verifier.
    ///
    /// The bundle may contain one or more certificates concatenated
    /// (the standard `cat ca-a.pem ca-b.pem > bundle.pem` shape). An
    /// empty file, an unreadable file, or a file with zero parseable
    /// certificates surfaces as [`RoasteryError::Config`] so the
    /// server fails fast at startup.
    pub fn load_ca<P: AsRef<Path>>(path: P) -> Result<Self, RoasteryError> {
        let path = path.as_ref();
        let pem = fs::read(path).map_err(|e| {
            RoasteryError::Config(format!(
                "cannot read mTLS CA file {}: {e}",
                path.display()
            ))
        })?;
        let certs = parse_pem_certs(&pem, path)?;
        if certs.is_empty() {
            return Err(RoasteryError::Config(format!(
                "mTLS CA file {} contained no certificates",
                path.display()
            )));
        }
        let mut roots = RootCertStore::empty();
        let root_count = certs.len();
        for cert in certs {
            roots.add(cert).map_err(|e| {
                RoasteryError::Config(format!(
                    "mTLS CA file {} contained an unparseable trust anchor: {e}",
                    path.display()
                ))
            })?;
        }
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|e| {
                RoasteryError::Config(format!(
                    "failed to build mTLS client verifier from {}: {e}",
                    path.display()
                ))
            })?;
        Ok(Self {
            verifier,
            source: path.to_path_buf(),
            root_count,
        })
    }

    /// The rustls client-cert verifier. Plug this into a
    /// `rustls::ServerConfig::builder` as the client-auth source.
    pub fn verifier(&self) -> Arc<dyn ClientCertVerifier> {
        self.verifier.clone()
    }

    /// Path the CA bundle was loaded from. Diagnostic only.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// How many distinct trust anchors `source` parsed into.
    pub fn root_count(&self) -> usize {
        self.root_count
    }
}

/// Parse a PEM blob into the list of `CertificateDer` entries it
/// contained. Surfaces a `RoasteryError::Config` on a parse failure
/// so the caller can attribute the error to the operator-supplied
/// path.
fn parse_pem_certs<'a>(
    pem: &'a [u8],
    path: &Path,
) -> Result<Vec<CertificateDer<'a>>, RoasteryError> {
    rustls_pemfile::certs(&mut std::io::Cursor::new(pem))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            RoasteryError::Config(format!(
                "failed to parse PEM certificates in {}: {e}",
                path.display()
            ))
        })
}

/// Extract a non-secret subject identifier from a client cert.
///
/// Strategy:
///
/// 1. First URI SAN — preferred because RFC 6125 / RFC 5280 §4.2.1.6
///    treat URI SANs as the modern identifier form (and SPIFFE IDs
///    are URI SANs).
/// 2. Subject Common Name — fallback for cert profiles that still
///    encode identity in the DN.
///
/// Returns the literal string `unknown` when both lookups miss; we
/// log that case so the operator can spot a misconfigured cert
/// chain, but the request still succeeds — the CA verifier already
/// accepted the chain, so an empty subject just means the cert
/// didn't carry a recognisable identifier field.
pub fn subject_from_cert(cert: &CertificateDer<'_>) -> String {
    match x509_parser::parse_x509_certificate(cert.as_ref()) {
        Ok((_, parsed)) => extract_subject(&parsed),
        Err(_) => "unknown".to_string(),
    }
}

fn extract_subject(parsed: &x509_parser::certificate::X509Certificate<'_>) -> String {
    // 1) Walk SANs for a URI entry.
    if let Ok(Some(san_ext)) = parsed.subject_alternative_name() {
        for name in &san_ext.value.general_names {
            if let x509_parser::extensions::GeneralName::URI(uri) = name {
                return (*uri).to_string();
            }
        }
    }

    // 2) Subject Common Name.
    if let Some(cn) = parsed
        .subject()
        .iter_common_name()
        .next()
        .and_then(|attr| attr.as_str().ok())
    {
        return cn.to_string();
    }

    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    use rcgen::string::Ia5String;
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};

    fn make_self_signed(common_name: &str, sans: Vec<SanType>) -> CertificateDer<'static> {
        let mut params = CertificateParams::new(Vec::new()).unwrap();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, common_name);
        params.distinguished_name = dn;
        params.subject_alt_names = sans;
        let kp = KeyPair::generate().unwrap();
        let cert = params.self_signed(&kp).unwrap();
        cert.der().clone()
    }

    #[test]
    fn subject_prefers_uri_san_over_common_name() {
        let uri = Ia5String::try_from("spiffe://example.org/svc/ci").unwrap();
        let cert = make_self_signed("leaf-cn", vec![SanType::URI(uri)]);
        let subj = subject_from_cert(&cert);
        assert_eq!(subj, "spiffe://example.org/svc/ci");
    }

    #[test]
    fn subject_falls_back_to_common_name() {
        let cert = make_self_signed("leaf-cn", Vec::new());
        let subj = subject_from_cert(&cert);
        assert_eq!(subj, "leaf-cn");
    }

    #[test]
    fn subject_unknown_on_unparseable() {
        let cert = CertificateDer::from(vec![0u8; 4]);
        let subj = subject_from_cert(&cert);
        assert_eq!(subj, "unknown");
    }

    #[test]
    fn load_ca_returns_error_for_missing_file() {
        let err = MtlsVerifier::load_ca("/no/such/file/ca.pem").unwrap_err();
        assert!(matches!(err, RoasteryError::Config(_)));
    }

    #[test]
    fn load_ca_returns_error_for_empty_file() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write;
        writeln!(f, "# not a cert").unwrap();
        let err = MtlsVerifier::load_ca(f.path()).unwrap_err();
        assert!(matches!(err, RoasteryError::Config(_)));
    }
}
