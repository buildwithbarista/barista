//! Concurrent-fetch stress + connection-ceiling integration test.
//!
//! Covers two PRD requirements via [`CacheSource`]:
//!
//! * **SM-4.4 / §18.5 — connection ceiling.** With
//!   `max_concurrent_connections = 3` and 100 in-flight fetches against
//!   distinct coords, the peak number of simultaneously-in-flight HTTP
//!   requests observed by the mock server must be `<= 3`.
//! * **No duplicate downloads of the same coord.** With 100 concurrent
//!   fetches of the *same* coord, exactly one upstream `GET` is issued
//!   (the per-coord lock + cache short-circuit the other 99).
//!
//! Plus a stress test: 100 parallel fetches across 10 overlapping
//! coords all succeed.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use barista_config::UpdatePolicy;
use barista_coords::Coords;
use barista_resolver::source::MetadataSource;

use barista_cache::cas::Cas;
use barista_cache::fetch::{FetchConfig, Fetcher};
use barista_cache::index::Index;
use barista_cache::source::CacheSource;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn pom_xml(group: &str, artifact: &str, version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>{group}</groupId>
  <artifactId>{artifact}</artifactId>
  <version>{version}</version>
  <packaging>jar</packaging>
</project>"#
    )
}

/// Build a `CacheSource` configured against the given mock server with
/// the supplied connection ceiling.
fn make_source(server_uri: &str, max_conn: u32) -> (tempfile::TempDir, CacheSource) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cas = Cas::open(tmp.path()).expect("cas");
    let index = Index::open(tmp.path()).expect("index");
    let cfg = FetchConfig {
        max_concurrent_connections: max_conn,
        request_timeout: Duration::from_secs(30),
        http2_enabled: false, // wiremock speaks HTTP/1.1
        user_agent: "barista-test/0.1".into(),
        default_upstream: server_uri.to_string(),
    };
    let fetcher = Fetcher::new(cfg).expect("fetcher");
    let cache_root = tmp.path().to_path_buf();
    let source = CacheSource::new(
        cas,
        index,
        fetcher,
        cache_root,
        UpdatePolicy::Daily,
        UpdatePolicy::Never,
    );
    (tmp, source)
}

/// Mount a `.sha256` and `.sha1` sidecar that 404s, so the source
/// falls through to the `Unverified` checksum branch and the fetch
/// succeeds without a hash.
async fn mount_404_sidecars(server: &MockServer, pom_path: &str) {
    Mock::given(method("GET"))
        .and(path(format!("{pom_path}.sha256")))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pom_path}.sha1")))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// Test 1: stress — 100 parallel fetches across 10 overlapping coords.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_100_parallel_overlapping() {
    let server = MockServer::start().await;

    // 10 stubbed coords. Each has a 100ms delay to ensure overlap.
    for i in 0..10 {
        let pom_path = format!("/com/example/widget-{i}/1.0.0/widget-{i}-1.0.0.pom");
        let body = pom_xml("com.example", &format!("widget-{i}"), "1.0.0");
        Mock::given(method("GET"))
            .and(path(&pom_path))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/xml")
                    .set_body_string(body)
                    .set_delay(Duration::from_millis(100)),
            )
            .mount(&server)
            .await;
        mount_404_sidecars(&server, &pom_path).await;
    }

    let (_tmp, source) = make_source(&server.uri(), 8);
    let source = Arc::new(source);

    let mut handles = Vec::with_capacity(100);
    for i in 0..100u32 {
        let s = source.clone();
        let idx = i % 10;
        handles.push(tokio::spawn(async move {
            let coords = Coords::new("com.example", format!("widget-{idx}")).expect("valid coords");
            s.fetch_pom(&coords, "1.0.0").await
        }));
    }

    let mut ok = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for h in handles {
        match h.await {
            Ok(Ok(_)) => ok += 1,
            Ok(Err(e)) => errors.push(format!("{e:?}")),
            Err(e) => errors.push(format!("join error: {e}")),
        }
    }
    assert_eq!(
        ok,
        100,
        "expected all 100 fetches to succeed; {} failed: {:?}",
        errors.len(),
        errors.iter().take(3).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Test 2: same-coord concurrency — exactly one upstream GET.
// ---------------------------------------------------------------------------

/// `Respond` impl that counts every invocation in an [`AtomicU64`].
struct CountingResponder {
    counter: Arc<AtomicU64>,
    template: ResponseTemplate,
}

impl Respond for CountingResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.counter.fetch_add(1, Ordering::SeqCst);
        self.template.clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_duplicate_downloads_for_same_coord() {
    let server = MockServer::start().await;

    let pom_path = "/com/example/widget-counted/1.0.0/widget-counted-1.0.0.pom";
    let body = pom_xml("com.example", "widget-counted", "1.0.0");
    let counter = Arc::new(AtomicU64::new(0));

    Mock::given(method("GET"))
        .and(path(pom_path))
        .respond_with(CountingResponder {
            counter: counter.clone(),
            template: ResponseTemplate::new(200)
                .set_body_string(body)
                .set_delay(Duration::from_millis(50)),
        })
        .mount(&server)
        .await;
    mount_404_sidecars(&server, pom_path).await;

    let (_tmp, source) = make_source(&server.uri(), 8);
    let source = Arc::new(source);
    let coords = Coords::new("com.example", "widget-counted").expect("coords");

    let mut handles = Vec::with_capacity(100);
    for _ in 0..100 {
        let s = source.clone();
        let c = coords.clone();
        handles.push(tokio::spawn(async move { s.fetch_pom(&c, "1.0.0").await }));
    }
    let mut ok = 0usize;
    for h in handles {
        if let Ok(Ok(_)) = h.await {
            ok += 1;
        }
    }
    assert_eq!(ok, 100, "all 100 fetches should succeed");

    let hits = counter.load(Ordering::SeqCst);
    assert_eq!(
        hits, 1,
        "expected exactly 1 HTTP GET for the same coord under per-coord locking + cache, got {hits}"
    );
}

// ---------------------------------------------------------------------------
// Test 3: connection ceiling — 100 in-flight, ceiling=3, peak <= 3.
// ---------------------------------------------------------------------------

/// `Respond` impl that increments an in-flight gauge, records the peak,
/// and spawns a background task to decrement after the same delay the
/// `ResponseTemplate` uses. Wiremock applies the response delay between
/// `respond()` returning and the body being sent to the client, so the
/// server-side "in flight" lifetime matches the client-observed
/// request duration.
struct InFlightResponder {
    in_flight: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    delay: Duration,
    template: ResponseTemplate,
}

impl Respond for InFlightResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        // peak = max(peak, cur).
        let mut prev = self.peak.load(Ordering::SeqCst);
        while cur > prev {
            match self
                .peak
                .compare_exchange_weak(prev, cur, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(p) => prev = p,
            }
        }
        let in_flight = self.in_flight.clone();
        let delay = self.delay;
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
        });
        self.template.clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_ceiling_enforced() {
    let server = MockServer::start().await;
    let in_flight = Arc::new(AtomicU32::new(0));
    let peak = Arc::new(AtomicU32::new(0));
    let resp_delay = Duration::from_millis(50);

    // 100 distinct coords so every fetch is a real HTTP request — no
    // cache hits or coord-lock coalescing.
    for i in 0..100 {
        let pom_path = format!("/com/example/ceiling-{i}/1.0.0/ceiling-{i}-1.0.0.pom");
        let body = pom_xml("com.example", &format!("ceiling-{i}"), "1.0.0");
        Mock::given(method("GET"))
            .and(path(&pom_path))
            .respond_with(InFlightResponder {
                in_flight: in_flight.clone(),
                peak: peak.clone(),
                delay: resp_delay,
                template: ResponseTemplate::new(200)
                    .set_body_string(body)
                    .set_delay(resp_delay),
            })
            .mount(&server)
            .await;
        mount_404_sidecars(&server, &pom_path).await;
    }

    let (_tmp, source) = make_source(&server.uri(), 3);
    let source = Arc::new(source);

    let mut handles = Vec::with_capacity(100);
    for i in 0..100u32 {
        let s = source.clone();
        handles.push(tokio::spawn(async move {
            let coords = Coords::new("com.example", format!("ceiling-{i}")).expect("valid coords");
            s.fetch_pom(&coords, "1.0.0").await
        }));
    }
    let mut ok = 0usize;
    for h in handles {
        if let Ok(Ok(_)) = h.await {
            ok += 1;
        }
    }
    assert_eq!(ok, 100, "all 100 fetches should succeed under ceiling=3");

    let peak_observed = peak.load(Ordering::SeqCst);
    eprintln!("connection_ceiling_enforced: peak in-flight observed = {peak_observed}");
    assert!(
        peak_observed >= 1,
        "expected peak > 0 (sanity), got {peak_observed}"
    );
    assert!(
        peak_observed <= 3,
        "expected peak in-flight <= 3 (ceiling), got {peak_observed}"
    );
}
