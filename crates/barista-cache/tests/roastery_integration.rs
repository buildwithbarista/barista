// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for the roastery-first → upstream-fallback
//! write path wired into `barista-cache` (M5.2 T2).
//!
//! Each test spins:
//!
//! - an in-process **roastery** server (plain HTTP, no/auth) via the
//!   `roastery` crate, mirroring the M5.2 T1 harness pattern; and/or
//! - an in-process **upstream** Maven mock via `wiremock`.
//!
//! A [`barista_cache::CacheSource`] is pointed at both and driven
//! through `fetch_pom`. The tests assert which tier served the bytes
//! (via the recorded [`barista_cache::OriginTier`]), that the bytes
//! are correct, and — where relevant — that the documented
//! `roastery_outcome` tracing field was emitted.
//!
//! `[T]` markers map to the proof set in the T2 spec.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use barista_cache::{
    Cas, CacheSource, FetchConfig, Fetcher, Index, IndexEntry, IndexKey, Origin, OriginTier,
    RoasteryOutcome, RoasteryOutcomeObserver,
};
use barista_config::UpdatePolicy;
use barista_coords::Coords;
use barista_resolver::source::{FetchOrigin, MetadataSource};
use barista_roastery_client::{
    AuthConfig, ClientConfig, Digest, RoasteryClient, TlsConfig,
};
use tempfile::TempDir;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::roastery_harness::{
    free_port_addr, spawn_bearer_roastery, spawn_digest_mismatch_mock, spawn_plain_roastery,
};

const SAMPLE_POM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>lib</artifactId>
  <version>1.0</version>
  <packaging>jar</packaging>
</project>"#;

fn coords(group: &str, artifact: &str) -> Coords {
    Coords::new(group, artifact).expect("valid coords")
}

/// Lowercase-hex SHA-256 of the sample POM, used as the sidecar body.
fn sample_pom_sha256() -> String {
    Digest::of_bytes(SAMPLE_POM.as_bytes()).to_hex()
}

/// Build a CacheSource over a fresh temp dir, pointed at the given
/// upstream URI. Returns the source + temp dir (kept alive) + the
/// underlying cache root.
fn make_source(upstream_uri: &str) -> (CacheSource, TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tmp");
    let cache_root = tmp.path().to_path_buf();
    let cas = Cas::open(&cache_root).expect("cas");
    let index = Index::open(&cache_root).expect("index");
    let cfg = FetchConfig {
        max_concurrent_connections: 4,
        request_timeout: Duration::from_secs(5),
        http2_enabled: false,
        user_agent: "barista-test/0.0".into(),
        default_upstream: upstream_uri.to_string(),
    };
    let fetcher = Fetcher::new(cfg).expect("fetcher");
    let source = CacheSource::new(
        cas,
        index,
        fetcher,
        cache_root.clone(),
        UpdatePolicy::Daily,
        UpdatePolicy::Never,
    );
    (source, tmp, cache_root)
}

/// Build a plain-HTTP roastery client pointed at `base`.
fn roastery_client(base: &str) -> RoasteryClient {
    let url: Url = base.parse().expect("parse base");
    let cfg = ClientConfig::builder(url)
        .tls(TlsConfig::PlainHttp)
        .timeout(Duration::from_secs(5))
        .build();
    RoasteryClient::new(cfg).expect("client")
}

/// Build a bearer-auth roastery client.
fn roastery_client_bearer(base: &str, token: &str) -> RoasteryClient {
    let url: Url = base.parse().expect("parse base");
    let cfg = ClientConfig::builder(url)
        .tls(TlsConfig::PlainHttp)
        .auth(AuthConfig::Bearer {
            token: token.to_string(),
        })
        .timeout(Duration::from_secs(5))
        .build();
    RoasteryClient::new(cfg).expect("client")
}

/// Mount a POM + its sidecars on the upstream mock. `pom_status`
/// controls whether the binary POM is served (200) or not (e.g.
/// 404). The sidecars are always served (200) when `with_sidecar`,
/// else 404.
async fn mount_pom_and_sidecars(
    server: &MockServer,
    with_pom: bool,
    with_sidecar: bool,
) {
    let pom_path = "/org/example/lib/1.0/lib-1.0.pom";
    let sha256_path = "/org/example/lib/1.0/lib-1.0.pom.sha256";
    let sha1_path = "/org/example/lib/1.0/lib-1.0.pom.sha1";

    if with_pom {
        Mock::given(method("GET"))
            .and(path(pom_path))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_POM))
            .mount(server)
            .await;
    } else {
        Mock::given(method("GET"))
            .and(path(pom_path))
            .respond_with(ResponseTemplate::new(404))
            .mount(server)
            .await;
    }

    if with_sidecar {
        Mock::given(method("GET"))
            .and(path(sha256_path))
            .respond_with(ResponseTemplate::new(200).set_body_string(sample_pom_sha256()))
            .mount(server)
            .await;
    } else {
        Mock::given(method("GET"))
            .and(path(sha256_path))
            .respond_with(ResponseTemplate::new(404))
            .mount(server)
            .await;
    }
    // No sha1 sidecar in any of these tests.
    Mock::given(method("GET"))
        .and(path(sha1_path))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

/// Seed the roastery with the sample POM bytes via the client's
/// `put_blob` path so a later `get_blob` is a CAS hit.
async fn seed_roastery(client: &RoasteryClient, bytes: &[u8]) {
    let digest = Digest::of_bytes(bytes);
    let size = bytes.len() as u64;
    let reader = Cursor::new(bytes.to_vec());
    client.put_blob(digest, reader, size).await.expect("seed put");
}

/// Read the recorded OriginTier for the sample POM from a freshly
/// re-opened index over `cache_root`. Returns None if not present.
fn recorded_tier(cache_root: &Path) -> Option<OriginTier> {
    let index = Index::open(cache_root).expect("reopen index");
    let key = IndexKey::new(coords("org.example", "lib"), "1.0", "pom", None);
    index.get(&key).map(|e| e.origin.tier)
}

// -------------------------------------------------------------------
// [T] #1 — roastery serves on local miss; upstream NOT contacted for
// the binary.
// -------------------------------------------------------------------
#[tokio::test]
async fn cache_with_roastery_configured_serves_from_roastery_on_local_miss() {
    let roastery = spawn_plain_roastery().await;
    let upstream = MockServer::start().await;
    // Upstream serves the sidecar (so the cache learns the digest)
    // but NOT the binary POM — proving the binary came from roastery.
    mount_pom_and_sidecars(&upstream, /* with_pom */ false, /* with_sidecar */ true)
        .await;

    let client = roastery_client(&roastery.base_url());
    seed_roastery(&client, SAMPLE_POM.as_bytes()).await;

    let (source, _tmp, cache_root) = make_source(&upstream.uri());
    let observer = RoasteryOutcomeObserver::new();
    let source = source
        .with_roastery(Arc::new(client), roastery.base_url())
        .with_roastery_observer(observer.clone());

    let (pom, origin) = source
        .fetch_pom(&coords("org.example", "lib"), "1.0")
        .await
        .expect("fetch via roastery");
    assert_eq!(pom.artifact_id, "lib");
    assert_eq!(origin, FetchOrigin::Remote);
    assert_eq!(
        recorded_tier(&cache_root),
        Some(OriginTier::Roastery),
        "bytes should be attributed to the roastery tier"
    );
    assert_eq!(
        observer.last(),
        Some(RoasteryOutcome::Hit),
        "the roastery attempt should be classified as Hit"
    );
}

// -------------------------------------------------------------------
// [T] #2 — no roastery configured → upstream only.
// -------------------------------------------------------------------
#[tokio::test]
async fn cache_without_roastery_falls_back_to_upstream_only() {
    let upstream = MockServer::start().await;
    mount_pom_and_sidecars(&upstream, true, true).await;

    let (source, _tmp, cache_root) = make_source(&upstream.uri());

    let (pom, origin) = source
        .fetch_pom(&coords("org.example", "lib"), "1.0")
        .await
        .expect("fetch via upstream");
    assert_eq!(pom.artifact_id, "lib");
    assert_eq!(origin, FetchOrigin::Remote);
    assert_eq!(recorded_tier(&cache_root), Some(OriginTier::Upstream));
}

// -------------------------------------------------------------------
// [T] #3 — roastery unreachable → falls through to upstream;
// observed `RoasteryOutcome::Unreachable`.
//
// The per-outcome classification is asserted via the deterministic
// `RoasteryOutcomeObserver` hook rather than by scraping logs: the
// `roastery_outcome` tracing field is emitted for production
// observability, but a thread-local log capture can miss events that
// fire on a tokio worker thread mid-fetch. The observer is an
// in-process, thread-safe recorder, so it's the robust test surface.
// -------------------------------------------------------------------
#[tokio::test]
async fn cache_with_roastery_unreachable_falls_through_to_upstream() {
    let upstream = MockServer::start().await;
    mount_pom_and_sidecars(&upstream, true, true).await;

    // Point the roastery client at a port nothing listens on.
    let dead = free_port_addr();
    let client = roastery_client(&format!("http://{dead}"));

    let (source, _tmp, cache_root) = make_source(&upstream.uri());
    let observer = RoasteryOutcomeObserver::new();
    let source = source
        .with_roastery(Arc::new(client), format!("http://{dead}"))
        .with_roastery_observer(observer.clone());

    let (pom, origin) = source
        .fetch_pom(&coords("org.example", "lib"), "1.0")
        .await
        .expect("fetch falls through to upstream");
    assert_eq!(pom.artifact_id, "lib");
    assert_eq!(origin, FetchOrigin::Remote);
    assert_eq!(recorded_tier(&cache_root), Some(OriginTier::Upstream));
    assert_eq!(
        observer.last(),
        Some(RoasteryOutcome::Unreachable),
        "expected the roastery attempt to be classified as Unreachable"
    );
}

// -------------------------------------------------------------------
// [T] #4 — roastery 404 (real miss) → falls through;
// observed `RoasteryOutcome::Miss`.
// -------------------------------------------------------------------
#[tokio::test]
async fn cache_with_roastery_404_falls_through_to_upstream() {
    let roastery = spawn_plain_roastery().await; // empty CAS → 404
    let upstream = MockServer::start().await;
    mount_pom_and_sidecars(&upstream, true, true).await;

    let client = roastery_client(&roastery.base_url());
    // Deliberately do NOT seed the roastery.

    let (source, _tmp, cache_root) = make_source(&upstream.uri());
    let observer = RoasteryOutcomeObserver::new();
    let source = source
        .with_roastery(Arc::new(client), roastery.base_url())
        .with_roastery_observer(observer.clone());

    let (pom, origin) = source
        .fetch_pom(&coords("org.example", "lib"), "1.0")
        .await
        .expect("fetch falls through to upstream");
    assert_eq!(pom.artifact_id, "lib");
    assert_eq!(origin, FetchOrigin::Remote);
    assert_eq!(recorded_tier(&cache_root), Some(OriginTier::Upstream));
    assert_eq!(
        observer.last(),
        Some(RoasteryOutcome::Miss),
        "expected the roastery 404 to be classified as Miss"
    );
}

// -------------------------------------------------------------------
// [T] #5 — roastery auth failure → falls through;
// observed `RoasteryOutcome::AuthFailed`.
// -------------------------------------------------------------------
#[tokio::test]
async fn cache_with_roastery_auth_failure_falls_through_to_upstream() {
    let roastery = spawn_bearer_roastery().await;
    let upstream = MockServer::start().await;
    mount_pom_and_sidecars(&upstream, true, true).await;

    // Client sends the WRONG token → 401 on protected routes.
    let client = roastery_client_bearer(&roastery.base_url(), "wrong-token");

    let (source, _tmp, cache_root) = make_source(&upstream.uri());
    let observer = RoasteryOutcomeObserver::new();
    let source = source
        .with_roastery(Arc::new(client), roastery.base_url())
        .with_roastery_observer(observer.clone());

    let (pom, origin) = source
        .fetch_pom(&coords("org.example", "lib"), "1.0")
        .await
        .expect("fetch falls through to upstream");
    assert_eq!(pom.artifact_id, "lib");
    assert_eq!(origin, FetchOrigin::Remote);
    assert_eq!(recorded_tier(&cache_root), Some(OriginTier::Upstream));
    assert_eq!(
        observer.last(),
        Some(RoasteryOutcome::AuthFailed),
        "expected the 401 to be classified as AuthFailed (and logged at WARN)"
    );
}

// -------------------------------------------------------------------
// [T] #6 — roastery digest mismatch → falls through;
// observed `RoasteryOutcome::DigestMismatch`.
// -------------------------------------------------------------------
#[tokio::test]
async fn cache_with_roastery_digest_mismatch_falls_through_to_upstream() {
    let roastery = spawn_digest_mismatch_mock().await;
    let upstream = MockServer::start().await;
    mount_pom_and_sidecars(&upstream, true, true).await;

    let client = roastery_client(&roastery.base_url());

    let (source, _tmp, cache_root) = make_source(&upstream.uri());
    let observer = RoasteryOutcomeObserver::new();
    let source = source
        .with_roastery(Arc::new(client), roastery.base_url())
        .with_roastery_observer(observer.clone());

    let (pom, origin) = source
        .fetch_pom(&coords("org.example", "lib"), "1.0")
        .await
        .expect("fetch falls through to upstream");
    assert_eq!(pom.artifact_id, "lib");
    assert_eq!(origin, FetchOrigin::Remote);
    assert_eq!(recorded_tier(&cache_root), Some(OriginTier::Upstream));
    assert_eq!(
        observer.last(),
        Some(RoasteryOutcome::DigestMismatch),
        "expected the BAR-CAS-001 response to be classified as DigestMismatch (and logged at ERROR)"
    );
}

// -------------------------------------------------------------------
// [T] #7 — after a roastery hit, the bytes are in the local CAS; the
// next fetch is served from disk without touching roastery/upstream.
// -------------------------------------------------------------------
#[tokio::test]
async fn cache_persists_after_roastery_hit_so_next_request_is_disk() {
    let roastery = spawn_plain_roastery().await;
    let upstream = MockServer::start().await;
    // Sidecar only on first fetch; the second fetch must not need
    // upstream at all.
    mount_pom_and_sidecars(&upstream, false, true).await;

    let client = roastery_client(&roastery.base_url());
    seed_roastery(&client, SAMPLE_POM.as_bytes()).await;

    let (source, _tmp, _cache_root) = make_source(&upstream.uri());
    let source = source.with_roastery(Arc::new(client), roastery.base_url());

    let c = coords("org.example", "lib");
    let (_, origin1) = source.fetch_pom(&c, "1.0").await.expect("first (roastery)");
    assert_eq!(origin1, FetchOrigin::Remote);

    // Tear down the roastery + upstream entirely; a disk hit must not
    // need either.
    drop(roastery);
    drop(upstream);

    let (pom, origin2) = source.fetch_pom(&c, "1.0").await.expect("second (disk)");
    assert_eq!(origin2, FetchOrigin::Disk);
    assert_eq!(pom.artifact_id, "lib");
}

// -------------------------------------------------------------------
// [T] #9 — Origin::Roastery round-trips through the index bincode
// codec.
// -------------------------------------------------------------------
#[test]
fn cache_origin_roastery_round_trips_through_index_serde() {
    use barista_cache::ContentHash;

    let entry = IndexEntry {
        hash: ContentHash::from_hex(&"ab".repeat(32)).unwrap(),
        size_bytes: 123,
        sha1_hex: None,
        origin: Origin {
            repository_url: "https://roastery.example.com:8443".to_string(),
            etag: None,
            last_modified: None,
            upstream_last_updated: None,
            tier: OriginTier::Roastery,
        },
        atime_unix: 1,
        created_unix: 1,
    };

    let cfg = bincode::config::standard();
    let bytes = bincode::serde::encode_to_vec(&entry, cfg).unwrap();
    let (decoded, _): (IndexEntry, _) =
        bincode::serde::decode_from_slice(&bytes, cfg).unwrap();
    assert_eq!(decoded, entry);
    assert_eq!(decoded.origin.tier, OriginTier::Roastery);
}

// -------------------------------------------------------------------
// [T] #10 — an old-shape Origin (no `tier` field) deserializes as
// Upstream — backward compat.
// -------------------------------------------------------------------
#[test]
fn cache_origin_upstream_entries_still_load_for_backward_compat() {
    // The migration policy is: a `tier` field missing on disk
    // deserializes to OriginTier::Upstream. We simulate an "old"
    // Origin by deserializing JSON that omits `tier` (serde's
    // `#[serde(default)]` applies regardless of format).
    let json = r#"{
        "repository_url": "https://repo.maven.apache.org/maven2",
        "etag": null,
        "last_modified": null,
        "upstream_last_updated": null
    }"#;
    let origin: Origin = serde_json::from_str(json).expect("legacy origin parses");
    assert_eq!(
        origin.tier,
        OriginTier::Upstream,
        "a missing tier field must default to Upstream"
    );
}
