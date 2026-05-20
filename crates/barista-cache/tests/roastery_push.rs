// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for push-after-build (M5.2 T3).
//!
//! When `cache.roastery.push = true` and a roastery is configured,
//! after the cache fetches an artifact from **upstream** (the bytes
//! did NOT come from the roastery) and persists it locally, it also
//! uploads the blob to the roastery so the next client on the team
//! gets a roastery hit. The push is strictly best-effort: a push
//! failure is logged but never fails the fetch.
//!
//! Each test spins an in-process **roastery** server (plain HTTP) via
//! the `roastery` crate plus an in-process **upstream** Maven mock via
//! `wiremock`, then drives a [`barista_cache::CacheSource`] through
//! `fetch_pom`. Push behavior is asserted via the deterministic
//! [`barista_cache::RoasteryPushObserver`] hook (never by scraping
//! logs — T2 learned that thread-local log capture misses events that
//! fire on tokio worker threads), and by directly statting the
//! roastery's CAS with a second client.
//!
//! `[T]` markers map to the proof set in the T3 spec.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use barista_cache::{
    Cas, CacheSource, FetchConfig, Fetcher, Index, OriginTier, PushOutcome, RoasteryOutcome,
    RoasteryOutcomeObserver, RoasteryPushObserver,
};
use barista_config::UpdatePolicy;
use barista_coords::Coords;
use barista_resolver::source::{FetchOrigin, MetadataSource};
use barista_roastery_client::{ClientConfig, Digest, RoasteryClient, TlsConfig};
use tempfile::TempDir;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::roastery_harness::{spawn_plain_roastery, spawn_put_failing_roastery, RoasteryHarness};

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

/// Mount the sample POM + its sidecars on the upstream mock.
/// `with_pom` controls whether the binary POM is served (200) or 404s;
/// the sha256 sidecar is always served (so the cache can learn the
/// digest), and there is never a sha1 sidecar.
async fn mount_pom_and_sidecars(server: &MockServer, with_pom: bool) {
    let pom_path = "/org/example/lib/1.0/lib-1.0.pom";
    let sha256_path = "/org/example/lib/1.0/lib-1.0.pom.sha256";
    let sha1_path = "/org/example/lib/1.0/lib-1.0.pom.sha1";

    let pom_status = if with_pom { 200 } else { 404 };
    let pom_resp = if with_pom {
        ResponseTemplate::new(pom_status).set_body_string(SAMPLE_POM)
    } else {
        ResponseTemplate::new(pom_status)
    };
    Mock::given(method("GET"))
        .and(path(pom_path))
        .respond_with(pom_resp)
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(sha256_path))
        .respond_with(ResponseTemplate::new(200).set_body_string(sample_pom_sha256()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(sha1_path))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

/// Mount only the `maven-metadata.xml` for the GA coordinate. Used by
/// the metadata-skip test.
async fn mount_metadata(server: &MockServer) {
    let xml = r#"<?xml version="1.0"?>
<metadata>
  <groupId>org.example</groupId>
  <artifactId>lib</artifactId>
  <versioning>
    <latest>1.0</latest>
    <release>1.0</release>
    <versions>
      <version>1.0</version>
    </versions>
    <lastUpdated>20260101000000</lastUpdated>
  </versioning>
</metadata>"#;
    Mock::given(method("GET"))
        .and(path("/org/example/lib/maven-metadata.xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(xml))
        .mount(server)
        .await;
}

/// True iff the roastery at `harness` currently holds the sample-POM
/// blob (checked via a direct `stat_blob` from an independent client).
async fn roastery_has_sample_pom(harness: &RoasteryHarness) -> bool {
    let probe = roastery_client(&harness.base_url());
    let digest = Digest::of_bytes(SAMPLE_POM.as_bytes());
    probe.stat_blob(digest).await.expect("stat_blob").is_some()
}

// -------------------------------------------------------------------
// [T] #1 — push enabled uploads an upstream-fetched blob to the
// roastery. Proves: fetch succeeds from upstream; the blob is now
// present in the (initially empty) roastery; the push observer
// recorded `Pushed`.
// -------------------------------------------------------------------
#[tokio::test]
async fn push_enabled_uploads_upstream_fetched_blob_to_roastery() {
    let roastery = spawn_plain_roastery().await; // empty CAS → GET 404
    let upstream = MockServer::start().await;
    mount_pom_and_sidecars(&upstream, /* with_pom */ true).await;

    let client = roastery_client(&roastery.base_url());
    // Deliberately do NOT seed the roastery — it starts empty.
    assert!(
        !roastery_has_sample_pom(&roastery).await,
        "precondition: roastery must start empty"
    );

    let (source, _tmp, cache_root) = make_source(&upstream.uri());
    let outcome = RoasteryOutcomeObserver::new();
    let push = RoasteryPushObserver::new();
    let source = source
        .with_roastery(Arc::new(client), roastery.base_url())
        .with_roastery_observer(outcome.clone())
        .with_roastery_push(true)
        .with_roastery_push_observer(push.clone());

    let (pom, origin) = source
        .fetch_pom(&coords("org.example", "lib"), "1.0")
        .await
        .expect("fetch via upstream");

    // (a) Served from upstream.
    assert_eq!(pom.artifact_id, "lib");
    assert_eq!(origin, FetchOrigin::Remote);
    let index = Index::open(&cache_root).expect("reopen index");
    let key = barista_cache::IndexKey::new(coords("org.example", "lib"), "1.0", "pom", None);
    assert_eq!(
        index.get(&key).map(|e| e.origin.tier),
        Some(OriginTier::Upstream),
        "bytes should be attributed to the upstream tier"
    );
    assert_eq!(outcome.last(), Some(RoasteryOutcome::Miss));

    // (b) The blob is now PRESENT in the roastery.
    assert!(
        roastery_has_sample_pom(&roastery).await,
        "push should have populated the roastery CAS"
    );
    // (c) The push observer recorded a single Pushed.
    assert_eq!(push.outcomes(), vec![PushOutcome::Pushed]);
}

// -------------------------------------------------------------------
// [T] #2 — push disabled does NOT upload. Same setup but push=false:
// the blob is fetched from upstream and served, but the roastery still
// does not have it afterward and the push observer is empty.
// -------------------------------------------------------------------
#[tokio::test]
async fn push_disabled_does_not_upload() {
    let roastery = spawn_plain_roastery().await;
    let upstream = MockServer::start().await;
    mount_pom_and_sidecars(&upstream, true).await;

    let client = roastery_client(&roastery.base_url());

    let (source, _tmp, _cache_root) = make_source(&upstream.uri());
    let push = RoasteryPushObserver::new();
    let source = source
        .with_roastery(Arc::new(client), roastery.base_url())
        // push left at its default of false; assert the builder isn't
        // required by simply not calling with_roastery_push.
        .with_roastery_push_observer(push.clone());

    let (pom, origin) = source
        .fetch_pom(&coords("org.example", "lib"), "1.0")
        .await
        .expect("fetch via upstream");
    assert_eq!(pom.artifact_id, "lib");
    assert_eq!(origin, FetchOrigin::Remote);

    assert!(
        !roastery_has_sample_pom(&roastery).await,
        "push disabled: roastery must remain empty"
    );
    assert!(
        push.outcomes().is_empty(),
        "push disabled: nothing should be recorded, got {:?}",
        push.outcomes()
    );
}

// -------------------------------------------------------------------
// [T] #3 — push skips a roastery-sourced blob. The roastery already
// has the artifact (warm); the fetch hits the roastery. We must NOT
// re-push what we just pulled — the push observer records
// `SkippedRoasterySource` and the upload path is never taken.
// -------------------------------------------------------------------
#[tokio::test]
async fn push_skips_roastery_sourced_blob() {
    let roastery = spawn_plain_roastery().await;
    let upstream = MockServer::start().await;
    // Upstream serves the sidecar (so the cache learns the digest) but
    // NOT the binary — proving the bytes came from the roastery.
    mount_pom_and_sidecars(&upstream, /* with_pom */ false).await;

    let client = roastery_client(&roastery.base_url());
    // Warm the roastery with the blob via an independent client.
    let seed = roastery_client(&roastery.base_url());
    let digest = Digest::of_bytes(SAMPLE_POM.as_bytes());
    seed.put_blob(
        digest,
        Cursor::new(SAMPLE_POM.as_bytes().to_vec()),
        SAMPLE_POM.len() as u64,
    )
    .await
    .expect("seed put");

    let (source, _tmp, _cache_root) = make_source(&upstream.uri());
    let outcome = RoasteryOutcomeObserver::new();
    let push = RoasteryPushObserver::new();
    let source = source
        .with_roastery(Arc::new(client), roastery.base_url())
        .with_roastery_observer(outcome.clone())
        .with_roastery_push(true)
        .with_roastery_push_observer(push.clone());

    let (pom, origin) = source
        .fetch_pom(&coords("org.example", "lib"), "1.0")
        .await
        .expect("fetch via roastery");
    assert_eq!(pom.artifact_id, "lib");
    assert_eq!(origin, FetchOrigin::Remote);
    assert_eq!(
        outcome.last(),
        Some(RoasteryOutcome::Hit),
        "bytes should have come from the roastery"
    );
    assert_eq!(
        push.outcomes(),
        vec![PushOutcome::SkippedRoasterySource],
        "a roastery-sourced blob must not be re-pushed"
    );
}

// -------------------------------------------------------------------
// [T] #4 — a push FAILURE does not fail the fetch. The roastery 404s
// the read (so the cache fetches from upstream) but 500s the PUT. The
// fetch still succeeds (served from local CAS), the push observer
// records `Failed`, and nothing panics / no error propagates.
// -------------------------------------------------------------------
#[tokio::test]
async fn push_failure_does_not_fail_the_fetch() {
    let roastery = spawn_put_failing_roastery().await; // GET 404, PUT 500
    let upstream = MockServer::start().await;
    mount_pom_and_sidecars(&upstream, true).await;

    let client = roastery_client(&roastery.base_url());

    let (source, _tmp, cache_root) = make_source(&upstream.uri());
    let push = RoasteryPushObserver::new();
    let source = source
        .with_roastery(Arc::new(client), roastery.base_url())
        .with_roastery_push(true)
        .with_roastery_push_observer(push.clone());

    // (a) Fetch STILL succeeds despite the failing PUT.
    let (pom, origin) = source
        .fetch_pom(&coords("org.example", "lib"), "1.0")
        .await
        .expect("fetch must succeed even when the push fails");
    assert_eq!(pom.artifact_id, "lib");
    assert_eq!(origin, FetchOrigin::Remote);

    // The artifact is durably in the local CAS (a disk hit next time).
    let index = Index::open(&cache_root).expect("reopen index");
    let key = barista_cache::IndexKey::new(coords("org.example", "lib"), "1.0", "pom", None);
    assert_eq!(
        index.get(&key).map(|e| e.origin.tier),
        Some(OriginTier::Upstream)
    );

    // (b) The push observer recorded Failed; (c) no panic / no error.
    assert_eq!(push.outcomes(), vec![PushOutcome::Failed]);
}

// -------------------------------------------------------------------
// [T] #5 — push skips maven-metadata.xml. Metadata XML is not a
// content-addressed blob (no stable sha256 identity), so even with
// push=true no push must be attempted.
// -------------------------------------------------------------------
#[tokio::test]
async fn push_skips_metadata_xml() {
    let roastery = spawn_plain_roastery().await;
    let upstream = MockServer::start().await;
    mount_metadata(&upstream).await;

    let client = roastery_client(&roastery.base_url());

    let (source, _tmp, _cache_root) = make_source(&upstream.uri());
    let push = RoasteryPushObserver::new();
    let source = source
        .with_roastery(Arc::new(client), roastery.base_url())
        .with_roastery_push(true)
        .with_roastery_push_observer(push.clone());

    let (md, origin) = source
        .fetch_metadata(&coords("org.example", "lib"))
        .await
        .expect("fetch metadata");
    assert_eq!(md.versions, vec!["1.0"]);
    assert_eq!(origin, FetchOrigin::Remote);

    assert!(
        push.outcomes().is_empty(),
        "maven-metadata.xml is not content-addressed and must never be pushed, got {:?}",
        push.outcomes()
    );
}

// -------------------------------------------------------------------
// [T] #6 (HEADLINE) — push then a second client gets a roastery hit.
// Client A (push=true, empty local cache) fetches from upstream and
// pushes. A SECOND CacheSource (separate empty local cache, SAME
// roastery) then fetches the same coord and gets RoasteryOutcome::Hit
// — proving the push populated the shared cache for the team.
// -------------------------------------------------------------------
#[tokio::test]
async fn push_uploads_then_second_client_gets_roastery_hit() {
    let roastery = spawn_plain_roastery().await; // shared, starts empty
    let upstream = MockServer::start().await;
    mount_pom_and_sidecars(&upstream, /* with_pom */ true).await;

    // ---- Client A: fetch from upstream + push to roastery. ----
    let client_a = roastery_client(&roastery.base_url());
    let (source_a, _tmp_a, _root_a) = make_source(&upstream.uri());
    let outcome_a = RoasteryOutcomeObserver::new();
    let push_a = RoasteryPushObserver::new();
    let source_a = source_a
        .with_roastery(Arc::new(client_a), roastery.base_url())
        .with_roastery_observer(outcome_a.clone())
        .with_roastery_push(true)
        .with_roastery_push_observer(push_a.clone());

    let (_, origin_a) = source_a
        .fetch_pom(&coords("org.example", "lib"), "1.0")
        .await
        .expect("client A fetch");
    assert_eq!(origin_a, FetchOrigin::Remote);
    assert_eq!(outcome_a.last(), Some(RoasteryOutcome::Miss));
    assert_eq!(
        push_a.outcomes(),
        vec![PushOutcome::Pushed],
        "client A should have pushed the upstream-fetched blob"
    );

    // ---- Client B: separate empty local cache, SAME roastery. ----
    let client_b = roastery_client(&roastery.base_url());
    let (source_b, _tmp_b, root_b) = make_source(&upstream.uri());
    let outcome_b = RoasteryOutcomeObserver::new();
    let source_b = source_b
        .with_roastery(Arc::new(client_b), roastery.base_url())
        .with_roastery_observer(outcome_b.clone());

    let (pom_b, origin_b) = source_b
        .fetch_pom(&coords("org.example", "lib"), "1.0")
        .await
        .expect("client B fetch");
    assert_eq!(pom_b.artifact_id, "lib");
    assert_eq!(origin_b, FetchOrigin::Remote);

    // The headline assertion: client B's bytes came from the roastery,
    // populated by client A's push.
    assert_eq!(
        outcome_b.last(),
        Some(RoasteryOutcome::Hit),
        "client B must get a roastery HIT — proving A's push populated the shared cache"
    );
    let index_b = Index::open(&root_b).expect("reopen index B");
    let key = barista_cache::IndexKey::new(coords("org.example", "lib"), "1.0", "pom", None);
    assert_eq!(
        index_b.get(&key).map(|e| e.origin.tier),
        Some(OriginTier::Roastery),
        "client B's bytes should be attributed to the roastery tier"
    );
}
