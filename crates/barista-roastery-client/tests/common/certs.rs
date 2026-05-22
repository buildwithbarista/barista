// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ephemeral PKI for mTLS integration tests.
//!
//! Per-run `rcgen` CA + server cert + client cert + an unrelated CA
//! for the rejected-handshake test. The server cert/key get
//! written to temp files because `roastery::run` reads them from
//! disk; the client cert/key stay in-memory as PEM strings.

use std::path::PathBuf;
use std::sync::OnceLock;

use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};
use tempfile::NamedTempFile;

pub struct TestPki {
    pub ca_pem: String,
    pub ca_pem_file: PathBuf,
    pub server_cert_file: PathBuf,
    pub server_key_file: PathBuf,
    pub client_cert_pem: String,
    pub client_key_pem: String,
    pub unrelated_client_cert_pem: String,
    pub unrelated_client_key_pem: String,
    pub _keepalive: Vec<NamedTempFile>,
}

pub fn build_pki(server_dns: &str) -> TestPki {
    // ---- CA ----
    let ca_kp = KeyPair::generate().expect("ca keypair");
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("ca params");
    let mut ca_dn = DistinguishedName::new();
    ca_dn.push(DnType::CommonName, "roastery-client-test-ca");
    ca_params.distinguished_name = ca_dn;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_cert = ca_params.self_signed(&ca_kp).expect("ca self-signed");
    let ca_issuer = rcgen::Issuer::new(ca_params, ca_kp);
    let ca_pem = ca_cert.pem();

    // ---- server cert ----
    let server_kp = KeyPair::generate().expect("server keypair");
    let mut server_params =
        CertificateParams::new(vec![server_dns.to_string()]).expect("server params");
    let mut server_dn = DistinguishedName::new();
    server_dn.push(DnType::CommonName, server_dns);
    server_params.distinguished_name = server_dn;
    server_params.subject_alt_names = vec![
        SanType::DnsName(Ia5String::try_from(server_dns).expect("dns san")),
        SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
    ];
    server_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    server_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server_params
        .signed_by(&server_kp, &ca_issuer)
        .expect("server cert");
    let server_cert_pem = server_cert.pem();
    let server_key_pem = server_kp.serialize_pem();

    // ---- client cert (same CA) ----
    let client_kp = KeyPair::generate().expect("client keypair");
    let mut client_params = CertificateParams::new(Vec::<String>::new()).expect("client params");
    let mut client_dn = DistinguishedName::new();
    client_dn.push(DnType::CommonName, "roastery-client-test-client");
    client_params.distinguished_name = client_dn;
    client_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let client_cert = client_params
        .signed_by(&client_kp, &ca_issuer)
        .expect("client cert");
    let client_cert_pem = client_cert.pem();
    let client_key_pem = client_kp.serialize_pem();

    // ---- unrelated CA + client cert ----
    let unrelated_ca_kp = KeyPair::generate().expect("u-ca keypair");
    let mut unrelated_ca_params =
        CertificateParams::new(Vec::<String>::new()).expect("u-ca params");
    let mut u_dn = DistinguishedName::new();
    u_dn.push(DnType::CommonName, "unrelated-ca");
    unrelated_ca_params.distinguished_name = u_dn;
    unrelated_ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    unrelated_ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let _unrelated_ca_cert = unrelated_ca_params
        .self_signed(&unrelated_ca_kp)
        .expect("u-ca cert");
    let unrelated_issuer = rcgen::Issuer::new(unrelated_ca_params, unrelated_ca_kp);
    let unrelated_client_kp = KeyPair::generate().expect("u-client keypair");
    let mut unrelated_client_params =
        CertificateParams::new(Vec::<String>::new()).expect("u-client params");
    let mut uc_dn = DistinguishedName::new();
    uc_dn.push(DnType::CommonName, "unrelated-client");
    unrelated_client_params.distinguished_name = uc_dn;
    unrelated_client_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let unrelated_client_cert = unrelated_client_params
        .signed_by(&unrelated_client_kp, &unrelated_issuer)
        .expect("u-client cert");
    let unrelated_client_cert_pem = unrelated_client_cert.pem();
    let unrelated_client_key_pem = unrelated_client_kp.serialize_pem();

    // ---- write to temp files ----
    let mut ca_file = NamedTempFile::new().expect("ca temp");
    let mut cert_file = NamedTempFile::new().expect("cert temp");
    let mut key_file = NamedTempFile::new().expect("key temp");
    use std::io::Write;
    ca_file.write_all(ca_pem.as_bytes()).expect("write ca");
    cert_file
        .write_all(server_cert_pem.as_bytes())
        .expect("write cert");
    key_file
        .write_all(server_key_pem.as_bytes())
        .expect("write key");

    let ca_path = ca_file.path().to_path_buf();
    let cert_path = cert_file.path().to_path_buf();
    let key_path = key_file.path().to_path_buf();

    TestPki {
        ca_pem,
        ca_pem_file: ca_path,
        server_cert_file: cert_path,
        server_key_file: key_path,
        client_cert_pem,
        client_key_pem,
        unrelated_client_cert_pem,
        unrelated_client_key_pem,
        _keepalive: vec![ca_file, cert_file, key_file],
    }
}

/// Install the rustls process-default crypto provider exactly once
/// — needed for any test that touches a rustls `ClientConfig`.
pub fn ensure_crypto_provider() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
