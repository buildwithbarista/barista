//! Upstream fetcher: streams a missing artifact in from one of the
//! configured Maven repositories, verifies its SHA-256 against the
//! requested digest, and parks it in the local CAS so subsequent
//! requests are local hits.
//!
//! ## Algorithm
//!
//! For each configured upstream URL, in order:
//!
//! 1. Build the upstream URL by joining the Maven layout path onto
//!    the repository base URL.
//! 2. Issue `GET <url>` with a connect / first-byte timeout from
//!    [`super::UpstreamConfig::timeout_secs`]. Once the response
//!    starts streaming there is no further timeout — Maven artifacts
//!    can be tens of MiB and we don't want a slow upstream to fail
//!    halfway through.
//! 3. If the response status is not 2xx, log + continue to the next
//!    repository. 404 is the common case; a 5xx is rarer but treated
//!    the same way (best-effort fallthrough).
//! 4. Stream the response body into [`crate::storage::Cas::put`],
//!    which hashes-and-verifies in-flight. A digest mismatch means
//!    this upstream is serving the wrong bytes for this digest —
//!    log + bump the `digest_mismatch` metric + continue to the next
//!    repository. Subsequent upstreams may serve the correct bytes.
//! 5. On the first repository that round-trips a hash-matching blob,
//!    return `Ok(Some(stat))`. The HTTP handler then re-issues the
//!    `stat`+`get` codepath against the local CAS to stream the
//!    response — there's no fast-path that bypasses the local store,
//!    because we want the side effect of the put to deduplicate
//!    concurrent fetches.
//! 6. If every repository was tried without success, return
//!    `Ok(None)`. The handler turns that into a 404.
//!
//! ## Metrics
//!
//! Every attempt records a counter increment + a histogram
//! observation through [`crate::ops::metrics::record_upstream_fetch`].
//! The label set is intentionally narrow:
//!
//! - `repo` — the bare host of the upstream URL (e.g.
//!   `repo.maven.apache.org`). Bounded by the operator's configured
//!   repo list.
//! - `result` ∈ {`hit`, `miss`, `error`, `digest_mismatch`}.
//!
//! The histogram tracks per-repo latency (no `result` label) so
//! Prometheus queries don't have to multiply out the cross product.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::TryStreamExt;
use tokio_util::io::StreamReader;
use tracing::{debug, info, warn};
use url::Url;

use super::coords::Coords;
use super::error::UpstreamError;
use crate::ops::metrics::{UpstreamResult, record_upstream_fetch};
use crate::storage::{Cas, CasReader, Digest, Stat};

/// Outcome of a single per-repository attempt. Kept as a private
/// enum so the outer `try_fetch` loop can decide the metric label +
/// whether to continue without juggling the public `UpstreamError`
/// variants for control flow.
enum AttemptOutcome {
    /// Repository served a hash-matching blob; persisted to local CAS.
    Hit(Stat),
    /// Repository returned a non-2xx; treated as "this repo doesn't
    /// have the artifact, try the next one".
    Miss,
    /// Repository served bytes whose hash didn't match the requested
    /// digest. The bytes were discarded by `Cas::put`'s verifier.
    DigestMismatch,
    /// Some other failure (network error, local CAS write blip, …).
    Error(UpstreamError),
}

/// Streaming Maven-Central-style upstream fetcher.
///
/// Built once at server startup from
/// [`super::UpstreamConfig`] + a shared CAS handle, then stashed on
/// `AppState` behind an `Arc`. Cheap to clone (`Arc` bumps); the
/// `reqwest::Client` it carries pools connections internally.
pub struct UpstreamFetcher {
    repos: Vec<Url>,
    client: reqwest::Client,
    cas: Arc<dyn Cas>,
}

impl UpstreamFetcher {
    /// Build a fetcher from the resolved upstream configuration.
    ///
    /// Returns an error if `reqwest::Client::builder` can't construct
    /// the client (typically a TLS / rustls setup problem — the
    /// `rustls-tls-manual-roots` feature requires the default crypto
    /// provider to be installed, which `server::run` does before
    /// constructing the fetcher).
    pub fn new(
        repos: Vec<Url>,
        timeout: Duration,
        cas: Arc<dyn Cas>,
    ) -> Result<Self, UpstreamError> {
        // Build a reqwest client with:
        //
        // - A connect + request timeout (`timeout`). Once the
        //   response starts streaming, reqwest's `timeout` applies to
        //   the whole request, but we accept that as the same "long
        //   enough for a typical Maven artifact" bound; operators
        //   bump `ROASTERY_UPSTREAM_TIMEOUT_SECS` for larger blobs.
        // - Connection pooling defaults are fine; the client is
        //   shared across all upstream attempts for the process.
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()?;
        Ok(Self { repos, client, cas })
    }

    /// Construct from an externally-built `reqwest::Client`. Used by
    /// tests that want to drive a deterministic client (e.g. a tiny
    /// http2-only setup) without going through the env-driven
    /// configuration path.
    #[doc(hidden)]
    pub fn with_client(repos: Vec<Url>, client: reqwest::Client, cas: Arc<dyn Cas>) -> Self {
        Self { repos, client, cas }
    }

    /// Attempt to fetch the artifact identified by `coords` from each
    /// configured upstream in order. On the first repository that
    /// round-trips a hash-matching blob, persists it to the local CAS
    /// and returns its [`Stat`].
    ///
    /// Returns `Ok(None)` when every repository was tried without
    /// success. Errors that interrupt a single attempt (timeout,
    /// non-2xx, digest mismatch) are folded into the next attempt;
    /// only a programming error (e.g.
    /// [`UpstreamError::NotConfigured`]) propagates.
    pub async fn try_fetch(
        &self,
        digest: Digest,
        coords: &Coords,
    ) -> Result<Option<Stat>, UpstreamError> {
        if self.repos.is_empty() {
            return Err(UpstreamError::NotConfigured);
        }

        let path = coords.to_maven_path();
        for repo in &self.repos {
            let repo_label = repo.host_str().unwrap_or("unknown").to_string();
            let started = Instant::now();
            let outcome = self.try_one(repo, &path, digest).await;
            let elapsed = started.elapsed();
            match outcome {
                AttemptOutcome::Hit(stat) => {
                    record_upstream_fetch(&repo_label, UpstreamResult::Hit, elapsed);
                    info!(
                        repo = %repo,
                        digest = %digest,
                        size = stat.size,
                        coords = %format!(
                            "{}:{}:{}",
                            coords.group, coords.artifact, coords.version
                        ),
                        "upstream: served + cached blob"
                    );
                    return Ok(Some(stat));
                }
                AttemptOutcome::DigestMismatch => {
                    record_upstream_fetch(
                        &repo_label,
                        UpstreamResult::DigestMismatch,
                        elapsed,
                    );
                    warn!(
                        repo = %repo,
                        digest = %digest,
                        "upstream: digest mismatch — falling through to next repo"
                    );
                    continue;
                }
                AttemptOutcome::Miss => {
                    record_upstream_fetch(&repo_label, UpstreamResult::Miss, elapsed);
                    continue;
                }
                AttemptOutcome::Error(err) => {
                    record_upstream_fetch(&repo_label, UpstreamResult::Error, elapsed);
                    warn!(
                        repo = %repo,
                        error = %err,
                        "upstream: fetch attempt failed"
                    );
                    continue;
                }
            }
        }

        debug!(digest = %digest, "upstream: all repos exhausted, returning miss");
        Ok(None)
    }

    /// Try a single upstream repository. Classified return value
    /// drives both the metric label and the outer loop's continue /
    /// short-circuit decision.
    async fn try_one(&self, repo: &Url, path: &str, digest: Digest) -> AttemptOutcome {
        let url = match repo.join(path) {
            Ok(u) => u,
            Err(e) => {
                return AttemptOutcome::Error(UpstreamError::InvalidCoords {
                    reason: format!("cannot join Maven path {path:?} onto {repo}: {e}"),
                });
            }
        };

        debug!(url = %url, "upstream: GET");
        let response = match self.client.get(url.clone()).send().await {
            Ok(r) => r,
            Err(e) => return AttemptOutcome::Error(UpstreamError::Io { source: e }),
        };
        let status = response.status();
        if !status.is_success() {
            debug!(
                url = %url,
                status = %status,
                "upstream: non-2xx — treating as miss for this repo"
            );
            return AttemptOutcome::Miss;
        }

        // Adapt the chunked response body into a
        // `tokio::io::AsyncRead` we can hand to `Cas::put`. Each chunk
        // is a `Bytes`; reqwest's stream item type is
        // `Result<Bytes, reqwest::Error>`, which we map to
        // `io::Error` so `StreamReader` accepts it.
        let stream = response
            .bytes_stream()
            .map_err(|e| std::io::Error::other(format!("upstream read error: {e}")));
        let reader = StreamReader::<_, Bytes>::new(stream);
        let cas_reader: CasReader = Box::new(reader);

        // `Cas::put` hashes-and-verifies in-flight; on a digest
        // mismatch it discards the staging file and returns
        // `StorageError::DigestMismatch` with both digests populated.
        match self.cas.put(digest, cas_reader).await {
            Ok(stat) => AttemptOutcome::Hit(stat),
            Err(crate::error::StorageError::DigestMismatch { .. }) => {
                AttemptOutcome::DigestMismatch
            }
            Err(other) => AttemptOutcome::Error(UpstreamError::Storage(other)),
        }
    }
}
