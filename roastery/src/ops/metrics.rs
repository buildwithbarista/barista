// SPDX-License-Identifier: MIT OR Apache-2.0

//! `GET /metrics` — Prometheus text exposition + metric registry.
//!
//! ## Choice of client library
//!
//! We use the `prometheus` crate directly (not `metrics` +
//! `metrics-exporter-prometheus`). The v0.1 metric set is small — one
//! info gauge, one uptime gauge, one counter vec, one histogram vec,
//! one storage-bytes gauge — and the `prometheus` crate's
//! `default_registry()` + `TextEncoder` get us there in ~50 lines with
//! no recorder thread, no facade, no extra layer of indirection. If
//! we later need per-request scoped metrics or pluggable exporters
//! (StatsD, OTLP) the migration to `metrics` is mechanical; until
//! then the lean dep is the right call.
//!
//! ## Registration discipline
//!
//! The collectors are registered into `prometheus::default_registry()`
//! exactly once per process via [`init`]. `init` is idempotent: a
//! second call from a test, a re-entry on a `tokio::spawn` race, or a
//! call after the registry has been populated is a no-op. We use
//! `std::sync::OnceLock` rather than `lazy_static!` so we don't pull a
//! macro crate in just for `static` initialisation — stdlib gets the
//! job done.
//!
//! ## Storage-bytes computation
//!
//! Filesystem-backend storage bytes are computed by walking
//! `<root>/cas/` and summing file sizes — the only way to get an
//! accurate number out of a content-addressed tree without keeping a
//! separate index. To stop a tight Prometheus scrape interval (5 s,
//! 15 s) from turning `/metrics` into an `fts_walk` bottleneck we
//! cache the value for [`STORAGE_BYTES_TTL`] seconds. The cache is a
//! `Mutex<Option<(Instant, u64)>>`; concurrent scrapes during a
//! refresh either wait or read a stale value, whichever the mutex
//! gives them — both are acceptable for a gauge.
//!
//! For the S3 / GCS stubs the reported value is `0`. The real
//! backends in v0.2 will publish bucket-level metrics themselves and
//! this gauge can either delegate or stay at 0; the wire contract
//! doesn't change.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use prometheus::{
    Encoder, Gauge, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, TextEncoder,
    default_registry,
};
use tracing::warn;

use crate::config::StorageBackend;
use crate::server::AppState;

/// How long the filesystem-backend `storage_bytes` value stays cached
/// before the next scrape recomputes it. Five seconds is a comfortable
/// trade-off: long enough to absorb a typical 5 s / 15 s Prometheus
/// scrape interval without re-walking, short enough that a fresh PUT
/// becomes visible to operators well inside a coffee break.
const STORAGE_BYTES_TTL: Duration = Duration::from_secs(5);

/// Default histogram buckets for CAS request latency, in seconds.
///
/// Covers everything from a single-syscall warm hit (`stat` + open at
/// the bottom of the lowest bucket) through a slow upstream-fill on a
/// cold network (upper bucket). Tuned by inspection rather than
/// production data — revisit once we have real scrape numbers from a
/// deployment.
const CAS_LATENCY_BUCKETS: &[f64] = &[0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0];

/// Default histogram buckets for upstream-fetch duration, in seconds.
///
/// Upstreams are slow compared to the local CAS — a Maven Central
/// round-trip plus a several-MiB jar download takes hundreds of
/// milliseconds at minimum. Buckets cover the warm cache-miss range
/// (a few hundred ms) through the worst-case "Central is being
/// unhelpful today" timeout (~60 s).
const UPSTREAM_LATENCY_BUCKETS: &[f64] = &[0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0];

/// Initialised by [`init`]. Holds the typed metric handles + the
/// storage-bytes cache + the process-start instant for uptime.
struct Metrics {
    /// `roastery_build_info{version, rustc} 1`. Held here so the
    /// registered collector lives for the lifetime of the process —
    /// we set the label-stamped value once at `init` and never touch
    /// it again, hence the `dead_code` allow. Dropping the field
    /// would (in theory) drop the last `Arc` to the collector and
    /// orphan it from the registry.
    #[allow(dead_code)]
    build_info: IntGaugeVec,
    /// `roastery_uptime_seconds`.
    uptime: Gauge,
    /// `roastery_cas_requests_total{method, result}`.
    cas_requests: IntCounterVec,
    /// `roastery_cas_request_duration_seconds`.
    cas_latency: HistogramVec,
    /// `roastery_storage_bytes_total{backend}`.
    storage_bytes: IntGaugeVec,
    /// `roastery_upstream_fetch_total{repo, result}`.
    upstream_fetches: IntCounterVec,
    /// `roastery_upstream_fetch_duration_seconds{repo}`.
    upstream_latency: HistogramVec,
    /// Process start instant (for uptime). Captured once at [`init`].
    started: Instant,
    /// Cached value of the most recent filesystem-walk, with the time
    /// it was captured. `None` until the first scrape; `Some(_)` after
    /// any successful walk.
    storage_bytes_cache: Mutex<Option<(Instant, u64)>>,
}

/// Process-global metric handles. Set exactly once by [`init`].
static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Register the metric collectors against the Prometheus default
/// registry and stamp the build-info gauge with the current crate
/// version + rustc string.
///
/// Idempotent: a second call is a no-op, so the smoke tests and the
/// integration tests can both call this without coordinating.
/// Concurrent first-callers are arbitrated by [`OnceLock`].
///
/// Called from [`crate::server::run`] *before* the listener starts
/// accepting connections so the first scrape doesn't race the
/// registration.
pub fn init() {
    METRICS.get_or_init(|| {
        let build_info = IntGaugeVec::new(
            Opts::new(
                "roastery_build_info",
                "Build identity for the running roastery binary. Always 1; the labels carry the info.",
            ),
            &["version", "rustc"],
        )
        .unwrap_or_else(|err| {
            // `prometheus::IntGaugeVec::new` only fails on malformed
            // metric/label names, both of which are static here. If
            // we hit this path the build is broken; surface a warning
            // and limp on with an unregistered placeholder so the
            // server still serves real traffic.
            warn!(error = %err, "failed to construct build_info gauge");
            placeholder_int_gauge_vec("roastery_build_info_placeholder", &["version", "rustc"])
        });

        let uptime = Gauge::new(
            "roastery_uptime_seconds",
            "Seconds since the metric registry was initialised (≈ process start).",
        )
        .unwrap_or_else(|err| {
            warn!(error = %err, "failed to construct uptime gauge");
            placeholder_gauge("roastery_uptime_seconds_placeholder")
        });

        let cas_requests = IntCounterVec::new(
            Opts::new(
                "roastery_cas_requests_total",
                "Total CAS handler invocations, partitioned by HTTP method and outcome.",
            ),
            &["method", "result"],
        )
        .unwrap_or_else(|err| {
            warn!(error = %err, "failed to construct cas_requests counter");
            placeholder_int_counter_vec(
                "roastery_cas_requests_total_placeholder",
                &["method", "result"],
            )
        });

        let cas_latency = HistogramVec::new(
            HistogramOpts::new(
                "roastery_cas_request_duration_seconds",
                "CAS handler latency in seconds.",
            )
            .buckets(CAS_LATENCY_BUCKETS.to_vec()),
            &["method"],
        )
        .unwrap_or_else(|err| {
            warn!(error = %err, "failed to construct cas_latency histogram");
            placeholder_histogram_vec(
                "roastery_cas_request_duration_seconds_placeholder",
                &["method"],
            )
        });

        let storage_bytes = IntGaugeVec::new(
            Opts::new(
                "roastery_storage_bytes_total",
                "Bytes resident in the configured CAS backend. Cached for ~5s.",
            ),
            &["backend"],
        )
        .unwrap_or_else(|err| {
            warn!(error = %err, "failed to construct storage_bytes gauge");
            placeholder_int_gauge_vec(
                "roastery_storage_bytes_total_placeholder",
                &["backend"],
            )
        });

        let upstream_fetches = IntCounterVec::new(
            Opts::new(
                "roastery_upstream_fetch_total",
                "Upstream-on-miss fetch attempts, partitioned by upstream host and outcome.",
            ),
            &["repo", "result"],
        )
        .unwrap_or_else(|err| {
            warn!(error = %err, "failed to construct upstream_fetches counter");
            placeholder_int_counter_vec(
                "roastery_upstream_fetch_total_placeholder",
                &["repo", "result"],
            )
        });

        let upstream_latency = HistogramVec::new(
            HistogramOpts::new(
                "roastery_upstream_fetch_duration_seconds",
                "Upstream-on-miss fetch duration in seconds, labelled by upstream host.",
            )
            .buckets(UPSTREAM_LATENCY_BUCKETS.to_vec()),
            &["repo"],
        )
        .unwrap_or_else(|err| {
            warn!(error = %err, "failed to construct upstream_latency histogram");
            placeholder_histogram_vec(
                "roastery_upstream_fetch_duration_seconds_placeholder",
                &["repo"],
            )
        });

        // Best-effort registration. If a metric is already registered
        // (e.g. a test that called `init` from a parallel binary)
        // `register` returns `AlreadyReg` — we ignore it.
        let _ = default_registry().register(Box::new(build_info.clone()));
        let _ = default_registry().register(Box::new(uptime.clone()));
        let _ = default_registry().register(Box::new(cas_requests.clone()));
        let _ = default_registry().register(Box::new(cas_latency.clone()));
        let _ = default_registry().register(Box::new(storage_bytes.clone()));
        let _ = default_registry().register(Box::new(upstream_fetches.clone()));
        let _ = default_registry().register(Box::new(upstream_latency.clone()));

        // Stamp the info gauge with the build identity. The labels
        // are read by Prometheus consumers via the
        // `roastery_build_info{version="…",rustc="…"}` pattern.
        let version = env!("CARGO_PKG_VERSION");
        let rustc = env!("ROASTERY_BUILD_RUSTC");
        build_info.with_label_values(&[version, rustc]).set(1);

        Metrics {
            build_info,
            uptime,
            cas_requests,
            cas_latency,
            storage_bytes,
            upstream_fetches,
            upstream_latency,
            started: Instant::now(),
            storage_bytes_cache: Mutex::new(None),
        }
    });
}

// ---------------------------------------------------------------------
// Placeholder collectors used when registration fails. These exist so
// the panic-free invariant holds (workspace lints flag `unwrap`); they
// should never be reached in practice because the metric names + label
// sets are all static.
// ---------------------------------------------------------------------

fn placeholder_gauge(name: &str) -> Gauge {
    Gauge::new(name, "placeholder").unwrap_or_else(|_| {
        // Truly impossible — `gauge_placeholder` is a valid name. If
        // we somehow get here, returning *some* `Gauge` keeps the
        // server limping; we already logged the original error.
        Gauge::new("gauge_placeholder", "placeholder").unwrap_or_else(|_| {
            // Final fallback: a deliberately leaked placeholder gauge
            // whose name we know is valid. We cannot panic here per
            // the workspace lint policy; a zero-valued gauge that
            // never gets registered is a survivable degradation.
            #[allow(clippy::unwrap_used)]
            Gauge::new("placeholder", "p").unwrap()
        })
    })
}

fn placeholder_int_gauge_vec(name: &str, labels: &[&str]) -> IntGaugeVec {
    IntGaugeVec::new(Opts::new(name, "placeholder"), labels).unwrap_or_else(|_| {
        #[allow(clippy::unwrap_used)]
        IntGaugeVec::new(Opts::new("placeholder_g", "p"), labels).unwrap()
    })
}

fn placeholder_int_counter_vec(name: &str, labels: &[&str]) -> IntCounterVec {
    IntCounterVec::new(Opts::new(name, "placeholder"), labels).unwrap_or_else(|_| {
        #[allow(clippy::unwrap_used)]
        IntCounterVec::new(Opts::new("placeholder_c", "p"), labels).unwrap()
    })
}

fn placeholder_histogram_vec(name: &str, labels: &[&str]) -> HistogramVec {
    HistogramVec::new(HistogramOpts::new(name, "placeholder"), labels).unwrap_or_else(|_| {
        #[allow(clippy::unwrap_used)]
        HistogramVec::new(HistogramOpts::new("placeholder_h", "p"), labels).unwrap()
    })
}

// ---------------------------------------------------------------------
// Public instrumentation API used by the protocol handlers.
// ---------------------------------------------------------------------

/// Stable wire identifier for a CAS HTTP method, as used in the
/// `method` label of `roastery_cas_requests_total` and
/// `roastery_cas_request_duration_seconds`.
#[derive(Debug, Clone, Copy)]
pub enum CasMethod {
    Get,
    Head,
    Put,
}

impl CasMethod {
    fn as_label(self) -> &'static str {
        match self {
            CasMethod::Get => "get",
            CasMethod::Head => "head",
            CasMethod::Put => "put",
        }
    }
}

/// Stable wire identifier for the outcome of a CAS request.
///
/// - `Hit`: the requested blob was present (GET/HEAD/PUT-success).
/// - `Miss`: the blob was absent (GET/HEAD → 404).
/// - `Error`: the handler failed (bad digest, I/O error, …).
#[derive(Debug, Clone, Copy)]
pub enum CasResult {
    Hit,
    Miss,
    Error,
}

impl CasResult {
    fn as_label(self) -> &'static str {
        match self {
            CasResult::Hit => "hit",
            CasResult::Miss => "miss",
            CasResult::Error => "error",
        }
    }
}

/// Record one CAS handler invocation: bumps the request counter and
/// observes its latency in the histogram.
///
/// Cheap: both `with_label_values` calls hit a small concurrent hash
/// map and the histogram `observe` is a bucket-array bump. Safe to
/// call from anywhere on the request path. No-op if [`init`] hasn't
/// run (which only happens before the server's metric registration
/// kicks in — i.e. in tests that drive a handler in isolation).
pub fn record_cas_request(method: CasMethod, result: CasResult, duration: Duration) {
    let Some(m) = METRICS.get() else {
        return;
    };
    let method_label = method.as_label();
    let result_label = result.as_label();
    m.cas_requests
        .with_label_values(&[method_label, result_label])
        .inc();
    m.cas_latency
        .with_label_values(&[method_label])
        .observe(duration.as_secs_f64());
}

/// Stable wire identifier for the outcome of one upstream-on-miss
/// fetch attempt against a single repository.
///
/// - `Hit`: upstream returned 2xx + the bytes hashed to the requested
///   digest. The blob is now in the local CAS.
/// - `Miss`: upstream returned a non-2xx (typically 404). Caller
///   moves on to the next repository in the list.
/// - `DigestMismatch`: upstream returned 2xx but the bytes hashed to
///   a different digest than the caller asked for. The bytes were
///   discarded by `Cas::put`'s verifier. Counted separately from
///   plain errors because it's the canary for an upstream serving
///   stale or compromised content.
/// - `Error`: network failure, timeout, or local CAS write blip.
///   Caller moves on.
#[derive(Debug, Clone, Copy)]
pub enum UpstreamResult {
    Hit,
    Miss,
    DigestMismatch,
    Error,
}

impl UpstreamResult {
    fn as_label(self) -> &'static str {
        match self {
            UpstreamResult::Hit => "hit",
            UpstreamResult::Miss => "miss",
            UpstreamResult::DigestMismatch => "digest_mismatch",
            UpstreamResult::Error => "error",
        }
    }
}

/// Record one upstream-on-miss fetch attempt: bumps the per-`(repo,
/// result)` counter and observes the per-`repo` latency histogram.
///
/// `repo` should be the bare host of the upstream URL (e.g.
/// `repo.maven.apache.org`) — the cardinality is bounded by the
/// operator's configured repo list, which is small.
///
/// No-op if [`init`] hasn't run.
pub fn record_upstream_fetch(repo: &str, result: UpstreamResult, duration: Duration) {
    let Some(m) = METRICS.get() else {
        return;
    };
    m.upstream_fetches
        .with_label_values(&[repo, result.as_label()])
        .inc();
    m.upstream_latency
        .with_label_values(&[repo])
        .observe(duration.as_secs_f64());
}

/// RAII helper: construct it at the top of a CAS handler, set the
/// outcome via [`CasTimer::finish`] before returning. If the handler
/// panics or early-returns without calling `finish`, `Drop` records
/// the request as an error. This keeps the handler bodies readable
/// — `let timer = CasTimer::start(CasMethod::Get); … timer.finish(…)`
/// — while still guaranteeing a counter bump on every codepath.
pub struct CasTimer {
    method: CasMethod,
    start: Instant,
    result: Option<CasResult>,
}

impl CasTimer {
    /// Start timing a CAS request of the given HTTP method.
    pub fn start(method: CasMethod) -> Self {
        Self {
            method,
            start: Instant::now(),
            result: None,
        }
    }

    /// Stamp the outcome. Must be called exactly once per timer; a
    /// subsequent `Drop` is a no-op (the recording already happened).
    pub fn finish(mut self, result: CasResult) {
        self.result = Some(result);
        // Drop runs here, doing the actual record_cas_request call.
    }
}

impl Drop for CasTimer {
    fn drop(&mut self) {
        // If `finish` was never called, surface the request as an
        // error — that's the conservative classification (a panicked
        // handler is closer to an error than a miss).
        let outcome = self.result.unwrap_or(CasResult::Error);
        record_cas_request(self.method, outcome, self.start.elapsed());
    }
}

/// Reset the storage-bytes cache. Intended for tests that need to
/// observe a freshly written blob without waiting for the 5-second
/// TTL to expire. No-op if [`init`] hasn't run.
#[doc(hidden)]
pub fn reset_storage_bytes_cache_for_tests() {
    let Some(m) = METRICS.get() else {
        return;
    };
    let Ok(mut guard) = m.storage_bytes_cache.lock() else {
        return;
    };
    *guard = None;
}

// ---------------------------------------------------------------------
// /metrics handler
// ---------------------------------------------------------------------

/// `GET /metrics` — render the registry into the Prometheus text
/// exposition format.
///
/// Refreshes the uptime gauge + the storage-bytes gauge for the
/// configured backend, then runs `TextEncoder` over the registry. The
/// Content-Type carries the exposition version
/// (`text/plain; version=0.0.4`) so Prometheus parsers downstream
/// know which dialect they got.
pub async fn metrics_handler(State(state): State<AppState>) -> Response {
    if let Some(m) = METRICS.get() {
        m.uptime.set(m.started.elapsed().as_secs_f64());
        refresh_storage_bytes(m, &state.config.storage).await;
    }

    let metric_families = default_registry().gather();
    let encoder = TextEncoder::new();
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    if let Err(err) = encoder.encode(&metric_families, &mut buf) {
        warn!(error = %err, "metrics encode failed");
        let body = format!("# encode error: {err}\n");
        let mut resp = (StatusCode::INTERNAL_SERVER_ERROR, body).into_response();
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        return resp;
    }

    // The exposition-format media type the Prometheus server expects.
    // `version=0.0.4` is the current stable text format; `0.0.5` is
    // OpenMetrics, which we don't emit.
    let mut resp = (StatusCode::OK, buf).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    resp
}

/// Update the `roastery_storage_bytes_total{backend=…}` gauge,
/// honouring the [`STORAGE_BYTES_TTL`] cache.
async fn refresh_storage_bytes(metrics: &Metrics, backend: &StorageBackend) {
    let backend_label = backend_label(backend);
    let bytes = match backend {
        StorageBackend::Filesystem(root) => fs_bytes_cached(metrics, root.clone()).await,
        // The S3/GCS stubs don't expose a byte count yet; report 0 so
        // the gauge is still emitted (Prometheus dashboards prefer a
        // present-but-zero series over a missing one).
        StorageBackend::S3 { .. } | StorageBackend::Gcs { .. } => 0,
    };
    let value = i64::try_from(bytes).unwrap_or(i64::MAX);
    metrics
        .storage_bytes
        .with_label_values(&[backend_label])
        .set(value);
}

/// Stable wire identifier for a storage backend, as used in the
/// `backend` label of `roastery_storage_bytes_total`.
fn backend_label(backend: &StorageBackend) -> &'static str {
    match backend {
        StorageBackend::Filesystem(_) => "filesystem",
        StorageBackend::S3 { .. } => "s3",
        StorageBackend::Gcs { .. } => "gcs",
    }
}

/// Return the filesystem-backend's resident byte count, hitting the
/// in-memory cache when fresh and walking the tree otherwise.
///
/// The walk is spawned on `tokio::task::spawn_blocking` so it doesn't
/// stall the runtime when a large CAS is being measured (`fs::read_dir`
/// is a blocking syscall).
async fn fs_bytes_cached(metrics: &Metrics, root: PathBuf) -> u64 {
    {
        let Ok(guard) = metrics.storage_bytes_cache.lock() else {
            return 0;
        };
        if let Some((captured_at, value)) = *guard
            && captured_at.elapsed() < STORAGE_BYTES_TTL
        {
            return value;
        }
    }

    let bytes = tokio::task::spawn_blocking(move || walk_cas_dir_size(&root))
        .await
        .unwrap_or_else(|err| {
            warn!(error = %err, "storage-bytes walk task panicked");
            0
        });

    if let Ok(mut guard) = metrics.storage_bytes_cache.lock() {
        *guard = Some((Instant::now(), bytes));
    }
    bytes
}

/// Walk `<root>/cas/` and sum every regular file's size. Silently
/// skips entries we can't `stat` (transient I/O errors during a walk
/// shouldn't crash the scrape). Returns 0 if the directory doesn't
/// exist yet — the gauge will start emitting non-zero on the first
/// successful PUT.
fn walk_cas_dir_size(root: &Path) -> u64 {
    let cas_root = root.join("cas");
    if !cas_root.is_dir() {
        return 0;
    }
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![cas_root];
    while let Some(dir) = stack.pop() {
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                if let Ok(meta) = entry.metadata() {
                    total = total.saturating_add(meta.len());
                }
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn backend_label_covers_all_variants() {
        assert_eq!(
            backend_label(&StorageBackend::Filesystem(PathBuf::from("/x"))),
            "filesystem"
        );
        assert_eq!(
            backend_label(&StorageBackend::S3 {
                bucket: "b".into(),
                region: "r".into(),
            }),
            "s3"
        );
        assert_eq!(
            backend_label(&StorageBackend::Gcs {
                bucket: "b".into(),
                project: "p".into(),
            }),
            "gcs"
        );
    }

    #[test]
    fn cas_method_and_result_labels_are_stable() {
        assert_eq!(CasMethod::Get.as_label(), "get");
        assert_eq!(CasMethod::Head.as_label(), "head");
        assert_eq!(CasMethod::Put.as_label(), "put");
        assert_eq!(CasResult::Hit.as_label(), "hit");
        assert_eq!(CasResult::Miss.as_label(), "miss");
        assert_eq!(CasResult::Error.as_label(), "error");
    }

    #[test]
    fn walk_cas_dir_size_returns_zero_for_missing_dir() {
        let tmp = TempDir::new().unwrap();
        // No `cas/` subdirectory.
        assert_eq!(walk_cas_dir_size(tmp.path()), 0);
    }

    #[test]
    fn walk_cas_dir_size_sums_files() {
        let tmp = TempDir::new().unwrap();
        let cas = tmp.path().join("cas");
        let sub = cas.join("ab");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("blob-1"), b"hello").unwrap(); // 5 bytes
        fs::write(sub.join("blob-2"), b"world!").unwrap(); // 6 bytes
        // Add an unrelated directory level to make sure recursion
        // covers it.
        let sub2 = cas.join("cd");
        fs::create_dir_all(&sub2).unwrap();
        fs::write(sub2.join("blob-3"), b"!!!").unwrap(); // 3 bytes
        assert_eq!(walk_cas_dir_size(tmp.path()), 5 + 6 + 3);
    }

    #[tokio::test]
    async fn fs_bytes_cached_returns_cached_value_within_ttl() {
        init();
        let m = METRICS.get().unwrap();

        let tmp = TempDir::new().unwrap();
        let cas = tmp.path().join("cas").join("ab");
        fs::create_dir_all(&cas).unwrap();
        fs::write(cas.join("blob-1"), b"hello").unwrap();

        // Clear the cache so we start clean — the OnceLock may have
        // been initialised by another test in the same binary.
        reset_storage_bytes_cache_for_tests();

        let first = fs_bytes_cached(m, tmp.path().to_path_buf()).await;
        assert_eq!(first, 5);

        // Add another file — without a reset, the cached value should
        // hold for the TTL window.
        fs::write(tmp.path().join("cas").join("ab").join("blob-2"), b"world!").unwrap();
        let second = fs_bytes_cached(m, tmp.path().to_path_buf()).await;
        assert_eq!(second, 5, "expected cached value within TTL, got {second}");

        // After an explicit reset the next call should recompute.
        reset_storage_bytes_cache_for_tests();
        let third = fs_bytes_cached(m, tmp.path().to_path_buf()).await;
        assert_eq!(third, 11);
    }

    #[test]
    fn init_is_idempotent() {
        // Two calls in a row must not panic or double-register. The
        // contract is "exactly once"; repeated calls are silently
        // dropped by `OnceLock::get_or_init`.
        init();
        init();
        assert!(METRICS.get().is_some());
    }
}
