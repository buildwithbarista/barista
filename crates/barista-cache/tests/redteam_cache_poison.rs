//! Cache-poisoning red-team suite.
//!
//! These are adversarial tests: each one crafts *malicious* input —
//! bytes that don't match an expected checksum, a coordinated
//! artifact+sidecar swap, a roastery that lies about what it serves,
//! a truncated mid-stream response — and asserts that the cache
//! **aborts the fetch, surfaces a clear error, and leaves NO
//! partial/poisoned artifact** behind. The mismatch error types
//! already exist (`ChecksumError::Mismatch`, `RoasteryOutcome::
//! DigestMismatch`, the roastery's `BAR-CAS-001`); the job here is to
//! prove the integrity defenses actually reject the attacker.
//!
//! The defended property under test is *no-poison-persisted*: after a
//! rejected fetch, the local content-addressed store must contain no
//! object for either the digest the attacker claimed or the digest the
//! poison bytes actually hash to, the index must hold no entry for the
//! coordinate, and no orphan tmp file may be left in `tmp/`.
//!
//! One case (#2, the coordinated upstream artifact+sidecar swap) is
//! NOT a defended case in v0.1: checksum verification is
//! trust-on-first-use against the *upstream-published* sidecar, so an
//! upstream that swaps *both* the artifact and its `.sha256` in
//! lockstep passes the checksum. That test asserts the **current,
//! honest** behavior (the swap is accepted) and documents it as the
//! accepted residual risk recorded in the threat model (finding #2 —
//! the publisher-signature gap). The real defense against a
//! coordinated swap is the committed lockfile pin, which is exercised
//! by the lockfile-drift red-team suite, not here.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use barista_cache::{
    Cas, CacheSource, ContentHash, FetchConfig, Fetcher, Index, IndexKey, OriginTier,
    RoasteryOutcome, RoasteryOutcomeObserver,
};
use barista_config::UpdatePolicy;
use barista_coords::Coords;
use barista_resolver::source::{FetchOrigin, MetadataError, MetadataSource};
use barista_roastery_client::{ClientConfig, RoasteryClient, TlsConfig};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::roastery_harness::{spawn_digest_mismatch_mock, spawn_lying_roastery};

const SAMPLE_POM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>lib</artifactId>
  <version>1.0</version>
  <packaging>jar</packaging>
</project>"#;

/// Bytes an attacker would love to slip into the build instead.
const POISON: &[u8] = b"PK\x03\x04 trojaned-jar-payload (definitely not the real artifact)";

const GROUP: &str = "org.example";
const ARTIFACT: &str = "lib";
const VERSION: &str = "1.0";

const POM_PATH: &str = "/org/example/lib/1.0/lib-1.0.pom";
const SHA256_PATH: &str = "/org/example/lib/1.0/lib-1.0.pom.sha256";
const SHA1_PATH: &str = "/org/example/lib/1.0/lib-1.0.pom.sha1";

fn coords() -> Coords {
    Coords::new(GROUP, ARTIFACT).expect("valid coords")
}

/// Lowercase-hex SHA-256 of a byte slice.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

struct Harness {
    _tmp: TempDir,
    source: CacheSource,
    server: MockServer,
    cache_root: PathBuf,
}

fn make_source(upstream_uri: &str) -> (CacheSource, TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tmp");
    let cache_root = tmp.path().to_path_buf();
    let cas = Cas::open(&cache_root).expect("cas");
    let index = Index::open(&cache_root).expect("index");
    let cfg = FetchConfig {
        max_concurrent_connections: 4,
        request_timeout: Duration::from_secs(5),
        http2_enabled: false,
        user_agent: "barista-redteam/0.0".into(),
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

async fn make_harness() -> Harness {
    let server = MockServer::start().await;
    let (source, tmp, cache_root) = make_source(&server.uri());
    Harness {
        _tmp: tmp,
        source,
        server,
        cache_root,
    }
}

fn roastery_client(base: &str) -> RoasteryClient {
    let url: Url = base.parse().expect("parse base");
    let cfg = ClientConfig::builder(url)
        .tls(TlsConfig::PlainHttp)
        .timeout(Duration::from_secs(5))
        .build();
    RoasteryClient::new(cfg).expect("client")
}

// --- post-state assertions -------------------------------------------------

/// Count the objects currently in the CAS, surfacing any walk error.
fn cas_object_count(cache_root: &Path) -> usize {
    let cas = Cas::open(cache_root).expect("reopen cas");
    let mut count = 0usize;
    for entry in cas.entries() {
        entry.expect("CAS walk error");
        count += 1;
    }
    count
}

/// True iff the CAS holds an object whose hash equals the SHA-256 of
/// `bytes` — i.e. those exact bytes were persisted.
fn cas_contains_bytes(cache_root: &Path, bytes: &[u8]) -> bool {
    let cas = Cas::open(cache_root).expect("reopen cas");
    let hash = ContentHash::from_hex(&sha256_hex(bytes)).expect("hash");
    cas.contains(&hash)
}

/// True iff the index has an entry for the sample POM coordinate.
fn index_has_pom_entry(cache_root: &Path) -> bool {
    let index = Index::open(cache_root).expect("reopen index");
    let key = IndexKey::new(coords(), VERSION, "pom", None);
    index.get(&key).is_some()
}

/// Number of stray files left in the CAS `tmp/` directory. The atomic
/// tmp+rename contract requires this to be zero after any
/// fetch — successful or rejected.
fn tmp_orphan_count(cache_root: &Path) -> usize {
    let tmp_dir = cache_root.join("tmp");
    match std::fs::read_dir(&tmp_dir) {
        Ok(rd) => rd.filter_map(Result::ok).count(),
        Err(_) => 0,
    }
}

/// Assert the full no-poison-persisted property: the CAS holds no
/// object at all, neither the claimed nor the actual poison bytes are
/// present, the index has no entry for the coord, and no tmp orphan
/// was left behind.
fn assert_clean_state(cache_root: &Path, poison: &[u8], claimed: &[u8]) {
    assert!(
        !cas_contains_bytes(cache_root, poison),
        "poison bytes were persisted into the CAS"
    );
    assert!(
        !cas_contains_bytes(cache_root, claimed),
        "the claimed-digest object leaked into the CAS"
    );
    assert_eq!(
        cas_object_count(cache_root),
        0,
        "CAS should be empty after a rejected poisoned fetch"
    );
    assert!(
        !index_has_pom_entry(cache_root),
        "index gained an entry for a coord whose fetch was rejected"
    );
    assert_eq!(
        tmp_orphan_count(cache_root),
        0,
        "an orphan tmp file was left behind (atomic tmp+rename contract violated)"
    );
}

async fn mount(server: &MockServer, url_path: &str, status: u16, body: impl Into<Vec<u8>>) {
    Mock::given(method("GET"))
        .and(path(url_path.to_string()))
        .respond_with(ResponseTemplate::new(status).set_body_bytes(body.into()))
        .mount(server)
        .await;
}

async fn mount_status(server: &MockServer, url_path: &str, status: u16) {
    Mock::given(method("GET"))
        .and(path(url_path.to_string()))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

// ===========================================================================
// Case 1 — upstream serves wrong bytes for a coordinate.
//
// The artifact URL returns POISON, but the `.sha256` sidecar carries
// the digest of the *real* artifact. The bytes don't match → the fetch
// must abort with a verification error and persist nothing.
// ===========================================================================
#[tokio::test]
async fn upstream_wrong_bytes_rejected_and_nothing_persisted() {
    let h = make_harness().await;
    // Artifact: poison. Sidecar: digest of the *honest* POM.
    mount(&h.server, POM_PATH, 200, POISON.to_vec()).await;
    mount(&h.server, SHA256_PATH, 200, sample_real_sha256()).await;
    mount_status(&h.server, SHA1_PATH, 404).await;

    let err = h
        .source
        .fetch_pom(&coords(), VERSION)
        .await
        .expect_err("poisoned bytes must be rejected");

    // The verify() failure is surfaced as a checksum-sidecar parse
    // error (the cache maps ChecksumError into MetadataError::Parse).
    match err {
        MetadataError::Parse { what, .. } => assert_eq!(what, "checksum sidecar"),
        other => panic!("expected a verification error, got {other:?}"),
    }

    assert_clean_state(&h.cache_root, POISON, SAMPLE_POM.as_bytes());
}

/// Sidecar body = SHA-256 of the *real* sample POM.
fn sample_real_sha256() -> String {
    sha256_hex(SAMPLE_POM.as_bytes())
}

// ===========================================================================
// Case 2 — coordinated upstream swap (artifact + sidecar both attacker).
//
// HARDER CASE. The attacker controls the upstream and swaps BOTH the
// artifact bytes AND the `.sha256` sidecar to a self-consistent
// attacker blob. Checksum-only verification is trust-on-first-use:
// "do the bytes match the sidecar I was handed?" — and here they do.
//
// This documents the v0.1 boundary HONESTLY: with no lockfile pin in
// play, the coordinated swap is ACCEPTED. The test asserts that
// current behavior rather than pretending a defense exists. This is
// the accepted residual risk recorded as threat-model finding #2; the
// real defense is the lockfile pin (see the lockfile-drift suite).
//
// We use a POM-SHAPED poison so the bytes parse as XML and the swap is
// observable end-to-end *through* `fetch_pom` — i.e. the acceptance is
// genuinely at the checksum layer, not masked by a downstream XML
// parse failure. The poison is a different artifact than the real one
// (a transitive dep the reviewer never saw), self-consistent with its
// own sidecar.
// ===========================================================================
const POISON_POM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>lib</artifactId>
  <version>1.0</version>
  <packaging>jar</packaging>
  <!-- attacker-substituted: a transitive dep the reviewer never saw -->
</project>"#;

#[tokio::test]
async fn coordinated_upstream_swap_is_accepted_documented_tofu_residual() {
    let h = make_harness().await;
    // Both the artifact and its sidecar are the attacker's, and they
    // agree with each other: sidecar = sha256(POISON_POM).
    mount(&h.server, POM_PATH, 200, POISON_POM.as_bytes().to_vec()).await;
    mount(&h.server, SHA256_PATH, 200, sha256_hex(POISON_POM.as_bytes())).await;
    mount_status(&h.server, SHA1_PATH, 404).await;

    let result = h.source.fetch_pom(&coords(), VERSION).await;

    // CURRENT v0.1 BEHAVIOR: checksum-only TOFU accepts the
    // self-consistent swap. If this assertion ever flips (i.e. a
    // publisher-signature check lands and rejects the swap), this test
    // and threat-model finding #2 must both be revisited.
    let (pom, origin) = result.expect(
        "v0.1 checksum-only TOFU is expected to accept a coordinated \
         artifact+sidecar swap. If a publisher-signature defense now \
         rejects it, update finding #2 and this assertion.",
    );
    assert_eq!(origin, FetchOrigin::Remote);
    assert_eq!(pom.artifact_id, ARTIFACT);

    // The attacker's bytes ARE now in the CAS — that is the residual
    // risk, stated plainly. The lockfile pin (other suite) is what
    // would have caught a swap whose digest differs from the committed
    // expectation.
    assert!(
        cas_contains_bytes(&h.cache_root, POISON_POM.as_bytes()),
        "documented residual: the accepted swap persists the attacker bytes"
    );
}

// ===========================================================================
// Case 3a — roastery honestly rejects (400 BAR-CAS-001).
//
// The roastery returns its own digest-mismatch error. The cache must
// classify it as RoasteryOutcome::DigestMismatch, fall through to
// upstream (404 here so the whole fetch fails cleanly), and persist
// nothing.
// ===========================================================================
#[tokio::test]
async fn roastery_bar_cas_001_rejected_no_poison_persisted() {
    let roastery = spawn_digest_mismatch_mock().await;
    let upstream = MockServer::start().await;
    // Upstream serves the sidecar (so the cache learns the digest to
    // ask the roastery for) but NOT the binary, and the binary 404s so
    // the whole fetch fails after the roastery fall-through.
    mount(&upstream, SHA256_PATH, 200, sample_real_sha256()).await;
    mount_status(&upstream, SHA1_PATH, 404).await;
    mount_status(&upstream, POM_PATH, 404).await;

    let (source, _tmp, cache_root) = make_source(&upstream.uri());
    let observer = RoasteryOutcomeObserver::new();
    let client = roastery_client(&roastery.base_url());
    let source = source
        .with_roastery(Arc::new(client), roastery.base_url())
        .with_roastery_observer(observer.clone());

    let err = source
        .fetch_pom(&coords(), VERSION)
        .await
        .expect_err("roastery rejected + upstream 404 → fetch fails");
    assert!(matches!(err, MetadataError::NotFound { .. }), "got {err:?}");

    assert_eq!(
        observer.last(),
        Some(RoasteryOutcome::DigestMismatch),
        "the 400 BAR-CAS-001 must be classified as DigestMismatch"
    );
    assert_clean_state(&cache_root, POISON, SAMPLE_POM.as_bytes());
}

// ===========================================================================
// Case 3b — roastery LIES: 200 OK with poison + echoed requested digest.
//
// The harder roastery case. A malicious/buggy roastery bypasses its
// own PUT verification and serves attacker bytes, echoing the
// *requested* digest header so the client's header cross-check passes.
// The cache's defense-in-depth (local sidecar re-verify +
// `cas_hash == asked`) must still catch it, fall through to upstream,
// and persist NONE of the poison.
// ===========================================================================
#[tokio::test]
async fn roastery_lies_with_poison_caught_client_side_no_poison_persisted() {
    let roastery = spawn_lying_roastery(POISON.to_vec()).await;
    let upstream = MockServer::start().await;
    // Sidecar present (cache learns the real digest, asks roastery for
    // it); binary 404 so the fetch fails after fall-through and we can
    // assert the clean state with no upstream success masking it.
    mount(&upstream, SHA256_PATH, 200, sample_real_sha256()).await;
    mount_status(&upstream, SHA1_PATH, 404).await;
    mount_status(&upstream, POM_PATH, 404).await;

    let (source, _tmp, cache_root) = make_source(&upstream.uri());
    let observer = RoasteryOutcomeObserver::new();
    let client = roastery_client(&roastery.base_url());
    let source = source
        .with_roastery(Arc::new(client), roastery.base_url())
        .with_roastery_observer(observer.clone());

    let err = source
        .fetch_pom(&coords(), VERSION)
        .await
        .expect_err("lying roastery poison rejected + upstream 404 → fetch fails");
    assert!(matches!(err, MetadataError::NotFound { .. }), "got {err:?}");

    // The lying roastery's bytes are rejected by the local
    // sidecar re-verify before they can be journaled.
    assert_eq!(
        observer.last(),
        Some(RoasteryOutcome::DigestMismatch),
        "client-side re-verify must classify the lying roastery as DigestMismatch"
    );
    assert_clean_state(&cache_root, POISON, SAMPLE_POM.as_bytes());
}

// ===========================================================================
// Case 4 — partial-write / interrupted fetch cleanup.
//
// A truncated mid-stream response: the artifact body is a PREFIX of
// the real bytes (as if the connection dropped), but the sidecar
// carries the full artifact's digest. The truncated bytes hash
// differently → verification fails → the atomic tmp+rename contract
// must leave no partial object in the CAS and no tmp orphan.
// ===========================================================================
#[tokio::test]
async fn truncated_body_leaves_no_partial_object() {
    let h = make_harness().await;
    // Serve only the first 20 bytes of the real POM, but advertise the
    // full POM's sha256 in the sidecar.
    let truncated = SAMPLE_POM.as_bytes()[..20].to_vec();
    mount(&h.server, POM_PATH, 200, truncated.clone()).await;
    mount(&h.server, SHA256_PATH, 200, sample_real_sha256()).await;
    mount_status(&h.server, SHA1_PATH, 404).await;

    let err = h
        .source
        .fetch_pom(&coords(), VERSION)
        .await
        .expect_err("truncated body must fail verification");
    match err {
        MetadataError::Parse { what, .. } => assert_eq!(what, "checksum sidecar"),
        other => panic!("expected a verification error, got {other:?}"),
    }

    // No partial object (neither the truncated prefix nor the full
    // artifact) and no tmp orphan.
    assert!(
        !cas_contains_bytes(&h.cache_root, &truncated),
        "truncated prefix was persisted as a CAS object"
    );
    assert_clean_state(&h.cache_root, &truncated, SAMPLE_POM.as_bytes());
}

// ===========================================================================
// Case 5 — idempotent re-fetch after a rejected poisoning attempt.
//
// First fetch is poisoned (wrong bytes vs sidecar) and rejected. The
// rejection must not wedge the coord lock or leave stale state, so a
// subsequent LEGITIMATE fetch of the same coord succeeds and lands the
// real bytes in the CAS.
// ===========================================================================
#[tokio::test]
async fn legit_refetch_succeeds_after_rejected_poison() {
    // First attempt: poisoned upstream.
    let poisoned = MockServer::start().await;
    mount(&poisoned, POM_PATH, 200, POISON.to_vec()).await;
    mount(&poisoned, SHA256_PATH, 200, sample_real_sha256()).await;
    mount_status(&poisoned, SHA1_PATH, 404).await;

    let (source, _tmp, cache_root) = make_source(&poisoned.uri());

    let err = source
        .fetch_pom(&coords(), VERSION)
        .await
        .expect_err("first (poisoned) fetch must be rejected");
    assert!(matches!(err, MetadataError::Parse { .. }), "got {err:?}");
    assert_clean_state(&cache_root, POISON, SAMPLE_POM.as_bytes());

    // Second attempt against an HONEST upstream over the SAME cache +
    // source. The failed attempt must not have wedged the per-coord
    // lock or left stale state. We point the SAME source at a fresh
    // honest server by rebuilding the source over the same cache root
    // (the coord-lock map lives on the source; rebuilding it proves
    // the on-disk state, not just the in-memory lock, is clean — and
    // mirrors a second `barista` invocation).
    drop(source);
    let honest = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(POM_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_POM))
        .mount(&honest)
        .await;
    mount(&honest, SHA256_PATH, 200, sample_real_sha256()).await;
    mount_status(&honest, SHA1_PATH, 404).await;

    let cas = Cas::open(&cache_root).expect("reopen cas");
    let index = Index::open(&cache_root).expect("reopen index");
    let cfg = FetchConfig {
        max_concurrent_connections: 4,
        request_timeout: Duration::from_secs(5),
        http2_enabled: false,
        user_agent: "barista-redteam/0.0".into(),
        default_upstream: honest.uri(),
    };
    let fetcher = Fetcher::new(cfg).expect("fetcher");
    let source2 = CacheSource::new(
        cas,
        index,
        fetcher,
        cache_root.clone(),
        UpdatePolicy::Daily,
        UpdatePolicy::Never,
    );

    let (pom, origin) = source2
        .fetch_pom(&coords(), VERSION)
        .await
        .expect("legit re-fetch must succeed after a rejected poison");
    assert_eq!(origin, FetchOrigin::Remote);
    assert_eq!(pom.artifact_id, ARTIFACT);

    // The HONEST bytes are now cached, attributed to upstream, and the
    // poison is still absent.
    assert!(
        cas_contains_bytes(&cache_root, SAMPLE_POM.as_bytes()),
        "the legitimate bytes should now be in the CAS"
    );
    assert!(
        !cas_contains_bytes(&cache_root, POISON),
        "the earlier poison must never have been persisted"
    );
    let index = Index::open(&cache_root).expect("reopen index");
    let key = IndexKey::new(coords(), VERSION, "pom", None);
    let entry = index.get(&key).expect("index entry present after legit fetch");
    assert_eq!(entry.origin.tier, OriginTier::Upstream);

    // A second read of the now-cached coord is a clean disk hit.
    let (_, origin2) = source2
        .fetch_pom(&coords(), VERSION)
        .await
        .expect("disk hit");
    assert_eq!(origin2, FetchOrigin::Disk);
}
