// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end integration tests for the roastery client.
//!
//! Each test spins a live `roastery` server in-process on an
//! ephemeral port, points a real [`RoasteryClient`] at it, and
//! exercises one endpoint or auth/TLS path. The `[T]` markers
//! correspond to the proof set documented in the M5.2 T1 spec.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::io::Cursor;
use std::time::Duration;

use barista_roastery_client::{
    AuthConfig, ClientConfig, ClientError, Digest, RoasteryClient, TlsConfig,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

mod common;

use common::harness::{
    spawn_bearer_server, spawn_mtls_server, spawn_plain_server, spawn_slow_server,
};

/// Build a no-auth, plain-HTTP client against a fixture's base URL.
fn plain_client(base: &str) -> RoasteryClient {
    let url: Url = base.parse().expect("parse base url");
    let cfg = ClientConfig::builder(url)
        .tls(TlsConfig::PlainHttp)
        .timeout(Duration::from_secs(10))
        .build();
    RoasteryClient::new(cfg).expect("plain client")
}

/// Build a bearer-auth, plain-HTTP client.
fn bearer_client(base: &str, token: &str) -> RoasteryClient {
    let url: Url = base.parse().expect("parse base url");
    let cfg = ClientConfig::builder(url)
        .tls(TlsConfig::PlainHttp)
        .auth(AuthConfig::Bearer {
            token: token.to_string(),
        })
        .timeout(Duration::from_secs(10))
        .build();
    RoasteryClient::new(cfg).expect("bearer client")
}

/// Build a mTLS, HTTPS client using a test PKI bundle.
fn mtls_client(
    base: &str,
    ca_pem: &str,
    client_cert_pem: &str,
    client_key_pem: &str,
) -> RoasteryClient {
    let url: Url = base.parse().expect("parse base url");
    let cfg = ClientConfig::builder(url)
        .tls(TlsConfig::CustomCa {
            ca_cert_pem: ca_pem.as_bytes().to_vec(),
        })
        .auth(AuthConfig::Mtls {
            client_cert_pem: client_cert_pem.as_bytes().to_vec(),
            client_key_pem: client_key_pem.as_bytes().to_vec(),
        })
        .timeout(Duration::from_secs(15))
        .build();
    RoasteryClient::new(cfg).expect("mtls client")
}

/// Drain a `BlobStream` into a `Vec<u8>`.
async fn drain(mut blob: barista_roastery_client::BlobStream) -> Vec<u8> {
    let mut buf = Vec::with_capacity(blob.stat.size as usize);
    blob.body.read_to_end(&mut buf).await.expect("read_to_end");
    buf
}

/// Helper: PUT a blob; return its digest.
async fn put_blob(client: &RoasteryClient, bytes: &[u8]) -> Digest {
    let digest = Digest::of_bytes(bytes);
    let size = bytes.len() as u64;
    let reader = Cursor::new(bytes.to_vec());
    client
        .put_blob(digest, reader, size)
        .await
        .expect("put_blob");
    digest
}

// -------------------------------------------------------------------
// [T] #1
// -------------------------------------------------------------------
#[tokio::test]
async fn client_get_blob_round_trips_bytes() {
    let h = spawn_plain_server().await;
    let c = plain_client(&h.base_url());

    let payload: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
    let digest = put_blob(&c, &payload).await;

    let blob = c.get_blob(digest).await.expect("get_blob");
    assert_eq!(blob.stat.digest, digest);
    assert_eq!(blob.stat.size, payload.len() as u64);
    let bytes = drain(blob).await;
    assert_eq!(bytes, payload);
}

// -------------------------------------------------------------------
// [T] #2
// -------------------------------------------------------------------
#[tokio::test]
async fn client_head_returns_stat_when_present() {
    let h = spawn_plain_server().await;
    let c = plain_client(&h.base_url());

    let payload = b"head returns stat payload".to_vec();
    let digest = put_blob(&c, &payload).await;

    let stat = c.stat_blob(digest).await.expect("stat_blob");
    let stat = stat.expect("present");
    assert_eq!(stat.digest, digest);
    assert_eq!(stat.size, payload.len() as u64);
}

// -------------------------------------------------------------------
// [T] #3
// -------------------------------------------------------------------
#[tokio::test]
async fn client_head_returns_none_when_absent() {
    let h = spawn_plain_server().await;
    let c = plain_client(&h.base_url());

    let absent = Digest::of_bytes(b"never written");
    let stat = c.stat_blob(absent).await.expect("stat_blob");
    assert!(stat.is_none());
}

// -------------------------------------------------------------------
// [T] #4
// -------------------------------------------------------------------
#[tokio::test]
async fn client_put_with_wrong_digest_surfaces_server_error() {
    let h = spawn_plain_server().await;
    let c = plain_client(&h.base_url());

    let real_bytes = b"the real bytes".to_vec();
    let actual_digest = Digest::of_bytes(&real_bytes);
    let bogus_digest = Digest::of_bytes(b"different bytes entirely");

    let size = real_bytes.len() as u64;
    let reader = Cursor::new(real_bytes);
    let err = c
        .put_blob(bogus_digest, reader, size)
        .await
        .expect_err("expected digest-mismatch error");

    match err {
        ClientError::ServerRejected {
            code,
            expected,
            actual,
            ..
        } => {
            assert_eq!(code, "BAR-CAS-001");
            assert_eq!(expected, Some(bogus_digest));
            assert_eq!(actual, Some(actual_digest));
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

// -------------------------------------------------------------------
// [T] #5
// -------------------------------------------------------------------
#[tokio::test]
async fn client_missing_returns_only_absent_digests() {
    let h = spawn_plain_server().await;
    let c = plain_client(&h.base_url());

    let a = b"alpha-missing".to_vec();
    let b = b"beta-missing".to_vec();
    let absent = Digest::of_bytes(b"never-written-missing");
    let a_digest = put_blob(&c, &a).await;
    let b_digest = put_blob(&c, &b).await;

    let result = c
        .missing(&[a_digest, b_digest, absent])
        .await
        .expect("missing");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], absent);
}

// -------------------------------------------------------------------
// [T] #6
// -------------------------------------------------------------------
#[tokio::test]
async fn client_missing_normalizes_prefixed_and_bare_hex() {
    // The client always serialises outgoing digests with the
    // `sha256:` prefix. The server responds with the same prefix.
    // This test pins that the response-parser correctly strips the
    // prefix back into a `Digest`.
    let h = spawn_plain_server().await;
    let c = plain_client(&h.base_url());

    let first = Digest::of_bytes(b"first-never-written");
    let second = Digest::of_bytes(b"second-never-written");
    let result = c.missing(&[first, second]).await.expect("missing");
    assert_eq!(result.len(), 2);
    assert!(result.contains(&first));
    assert!(result.contains(&second));
}

// -------------------------------------------------------------------
// [T] #7
// -------------------------------------------------------------------
#[tokio::test]
async fn client_health_endpoint_works_unauthenticated() {
    let h = spawn_bearer_server().await;
    // Anonymous config — the health endpoint must succeed anyway.
    let c = plain_client(&h.base_url());

    let health = c.health().await.expect("health");
    assert_eq!(health.status, "ok");
    assert_eq!(health.protocol, "barista");
    assert_eq!(health.version, "v1");
}

// -------------------------------------------------------------------
// [T] #8
// -------------------------------------------------------------------
#[tokio::test]
async fn client_capabilities_endpoint_works_unauthenticated() {
    let h = spawn_bearer_server().await;
    let c = plain_client(&h.base_url());

    let caps = c.capabilities().await.expect("capabilities");
    assert_eq!(caps.protocol, "barista");
    assert_eq!(caps.version, "v1");
    assert_eq!(caps.cas.max_batch_missing, 1000);
    assert_eq!(caps.cas.hashes, vec!["sha256".to_string()]);
    assert_eq!(caps.storage.backend, "filesystem");
}

// -------------------------------------------------------------------
// [T] #9
// -------------------------------------------------------------------
#[tokio::test]
async fn client_bearer_auth_works_against_protected_route() {
    let h = spawn_bearer_server().await;
    let c = bearer_client(&h.base_url(), "s3cret");

    // PUT then GET — both go through the protected route.
    let payload = b"bearer-protected payload".to_vec();
    let digest = put_blob(&c, &payload).await;
    let blob = c.get_blob(digest).await.expect("get_blob");
    assert_eq!(drain(blob).await, payload);
}

// -------------------------------------------------------------------
// [T] #10
// -------------------------------------------------------------------
#[tokio::test]
async fn client_bearer_auth_failure_surfaces_401() {
    let h = spawn_bearer_server().await;
    let c = bearer_client(&h.base_url(), "wrong-token");

    let absent = Digest::of_bytes(b"will not reach storage");
    let err = c
        .get_blob(absent)
        .await
        .expect_err("expected 401 auth error");
    match err {
        ClientError::Auth { code, .. } => {
            assert_eq!(code, "BAR-AUTH-001");
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

// -------------------------------------------------------------------
// [T] #11
// -------------------------------------------------------------------
#[tokio::test]
async fn client_anonymous_against_protected_route_fails_401() {
    let h = spawn_bearer_server().await;
    let c = plain_client(&h.base_url()); // anonymous

    let absent = Digest::of_bytes(b"anonymous protected probe");
    let err = c
        .get_blob(absent)
        .await
        .expect_err("expected 401 auth error");
    match err {
        ClientError::Auth { code, .. } => {
            assert_eq!(code, "BAR-AUTH-001");
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

// -------------------------------------------------------------------
// [T] #12
// -------------------------------------------------------------------
#[tokio::test]
async fn client_mtls_with_valid_cert_succeeds() {
    let h = spawn_mtls_server().await;
    let pki = h.pki.as_ref().expect("pki present");
    let c = mtls_client(
        &h.base_url(),
        &pki.ca_pem,
        &pki.client_cert_pem,
        &pki.client_key_pem,
    );

    let payload = b"mtls round-trip payload".to_vec();
    let digest = put_blob(&c, &payload).await;
    let blob = c.get_blob(digest).await.expect("get_blob");
    assert_eq!(drain(blob).await, payload);
}

// -------------------------------------------------------------------
// [T] #13
// -------------------------------------------------------------------
//
// An "unrelated CA" client cert fails the TLS handshake. rustls
// (via reqwest) surfaces this as a transport-level error. The
// client's `ClientError::from_reqwest` mapping classifies it as
// either `Tls` or `Network` depending on whether the underlying
// error string includes a TLS keyword. Both are acceptable
// outcomes — this test just pins that it surfaces as a hard error,
// not a successful response.
#[tokio::test]
async fn client_mtls_with_unrelated_cert_fails() {
    let h = spawn_mtls_server().await;
    let pki = h.pki.as_ref().expect("pki present");
    let c = mtls_client(
        &h.base_url(),
        &pki.ca_pem,
        &pki.unrelated_client_cert_pem,
        &pki.unrelated_client_key_pem,
    );

    let absent = Digest::of_bytes(b"mtls unrelated probe");
    let err = c
        .get_blob(absent)
        .await
        .expect_err("expected TLS/Network error");
    assert!(
        matches!(err, ClientError::Tls { .. } | ClientError::Network { .. }),
        "expected Tls or Network variant, got {err:?}"
    );
}

// -------------------------------------------------------------------
// [T] #14
// -------------------------------------------------------------------
#[tokio::test]
async fn client_plain_http_against_https_url_refuses_to_construct() {
    let url: Url = "https://example.com:8443".parse().unwrap();
    let cfg = ClientConfig::builder(url)
        .tls(TlsConfig::PlainHttp)
        .build();
    let err = RoasteryClient::new(cfg).expect_err("expected Config error");
    assert!(matches!(err, ClientError::Config { .. }));
}

// -------------------------------------------------------------------
// [T] #15
// -------------------------------------------------------------------
//
// Streaming proof: produce a 16 MiB blob by writing to an
// `tokio::io::duplex` writer in a spawned task and feeding the
// reader half to `put_blob`. Neither side buffers the whole blob
// in memory.
#[tokio::test]
async fn client_streams_large_blob_without_oom() {
    let h = spawn_plain_server().await;
    let c = plain_client(&h.base_url());

    const SIZE: usize = 16 * 1024 * 1024; // 16 MiB
    const CHUNK: usize = 64 * 1024;

    // Build the blob deterministically so we can verify it round-trips.
    let pattern: Vec<u8> = (0..CHUNK).map(|i| (i % 251) as u8).collect();
    let blob: Vec<u8> = pattern
        .iter()
        .cycle()
        .take(SIZE)
        .copied()
        .collect();
    let digest = Digest::of_bytes(&blob);

    let (reader, mut writer) = tokio::io::duplex(64 * 1024);
    let blob_clone = blob.clone();
    let writer_task = tokio::spawn(async move {
        for chunk in blob_clone.chunks(CHUNK) {
            writer.write_all(chunk).await.expect("duplex write");
        }
        writer.shutdown().await.expect("duplex shutdown");
    });

    c.put_blob(digest, reader, SIZE as u64)
        .await
        .expect("streamed put");
    writer_task.await.expect("writer task");

    let got = c.get_blob(digest).await.expect("get_blob");
    assert_eq!(got.stat.size, SIZE as u64);
    let bytes = drain(got).await;
    assert_eq!(bytes.len(), SIZE);
    assert_eq!(Digest::of_bytes(&bytes), digest);
}

// -------------------------------------------------------------------
// [T] #16
// -------------------------------------------------------------------
//
// The fixture's handler sleeps 2s before responding; the client is
// configured with a 250ms timeout. We pin that the failure
// surfaces as `ClientError::Timeout`, not a generic `Network` with
// a buried "timed out" string.
#[tokio::test]
async fn client_request_timeout_surfaces_as_timeout_error() {
    let h = spawn_slow_server(Duration::from_secs(2)).await;
    let url: Url = h.base_url().parse().unwrap();
    let cfg = ClientConfig::builder(url)
        .tls(TlsConfig::PlainHttp)
        .timeout(Duration::from_millis(250))
        .build();
    let c = RoasteryClient::new(cfg).expect("client");

    // Any well-formed digest will do — the fixture echoes the same
    // slow handler regardless of the path.
    let probe = Digest::of_bytes(b"timeout probe");
    let err = c.get_blob(probe).await.expect_err("expected timeout");
    assert!(
        matches!(err, ClientError::Timeout),
        "expected ClientError::Timeout, got {err:?}"
    );
}
