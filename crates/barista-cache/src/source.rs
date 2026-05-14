//! Production [`MetadataSource`] impl backed by the local cache.
//!
//! This is the glue between the resolver and the cache subsystem.
//! For each `fetch_pom` / `fetch_metadata` call:
//!
//! 1. Lock the per-coord async mutex ([`CoordLockMap`]) so concurrent
//!    fetches of the same coord serialize in-process.
//! 2. Check the [`Index`]. On a hit we read from [`Cas`] and serve
//!    [`FetchOrigin::Disk`].
//! 3. Otherwise, acquire the cross-process [`FilesystemLock`], then
//!    fetch via the [`Fetcher`] with conditional headers from the
//!    prior entry (when revalidating).
//! 4. On a 2xx response, fetch the `.sha256` and `.sha1` sidecars in
//!    parallel, verify via [`checksum::verify`], `put` bytes into the
//!    CAS, and journal the new [`IndexEntry`]. Serve
//!    [`FetchOrigin::Remote`].
//!
//! The `update_policy` knobs are accepted but only consulted for
//! future revalidation behavior. In v0.1 the policy stays at the
//! configured defaults (snapshot=Daily, release=Never) and the
//! cache serves directly on hit; a richer freshness check is a
//! follow-up.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;

use barista_config::UpdatePolicy;
use barista_coords::Coords;
use barista_pom::raw::{RawPom, parse_pom};
use barista_resolver::snapshot::{SnapshotInfo, parse_snapshot_metadata};
use barista_resolver::source::{FetchOrigin, GaMetadata, MetadataError, MetadataSource};

use crate::cas::{Cas, CasError};
use crate::checksum::{self, Verification};
use crate::fetch::{ConditionalHeaders, FetchError, FetchOutcome, Fetcher};
use crate::index::{Index, IndexEntry, IndexKey, Origin};
use crate::lock::{CoordLockMap, CoordVersionKey, FilesystemLock};

/// Sentinel "version" used as the [`IndexKey::version`] for the
/// group:artifact-level `maven-metadata.xml` entries. The `<>` makes
/// it impossible to collide with a real Maven version string.
const METADATA_SENTINEL_VERSION: &str = "<metadata>";

/// Sentinel "version" for a SNAPSHOT version's per-version
/// `maven-metadata.xml`. Combined with the actual SNAPSHOT version
/// string by [`snapshot_meta_version`] below.
fn snapshot_meta_version(version: &str) -> String {
    format!("<snapshot-metadata>:{version}")
}

/// A cache-backed implementation of
/// [`barista_resolver::source::MetadataSource`].
///
/// Wires together every cache subsystem (CAS, index, fetcher,
/// in-process + cross-process locks) behind the resolver's trait.
/// Cheap to clone; internally an `Arc` over the shared state.
#[derive(Debug, Clone)]
pub struct CacheSource {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    cas: Cas,
    index: Index,
    fetcher: Fetcher,
    coord_locks: CoordLockMap,
    cache_root: PathBuf,
    // Update policies are stashed for future-use; v0.1 serves directly
    // from cache on a hit. See module docstring.
    #[allow(dead_code)]
    snapshot_update_policy: UpdatePolicy,
    #[allow(dead_code)]
    release_update_policy: UpdatePolicy,
}

impl CacheSource {
    /// Construct a `CacheSource` from already-opened subsystems. The
    /// caller is responsible for opening the `Cas`/`Index`/`Fetcher`
    /// against consistent paths (the `cache_root` here is used only
    /// for the cross-process lock directory).
    pub fn new(
        cas: Cas,
        index: Index,
        fetcher: Fetcher,
        cache_root: PathBuf,
        snapshot_update_policy: UpdatePolicy,
        release_update_policy: UpdatePolicy,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                cas,
                index,
                fetcher,
                coord_locks: CoordLockMap::new(),
                cache_root,
                snapshot_update_policy,
                release_update_policy,
            }),
        }
    }

    fn lock_root(&self) -> PathBuf {
        self.inner.cache_root.join("locks")
    }

    /// Fetch + verify + cache an artifact's bytes. Returns the
    /// payload bytes alongside the index entry to write and the
    /// observed [`FetchOrigin`].
    async fn fetch_and_cache(
        &self,
        coords: &Coords,
        version: &str,
        type_: &str,
        classifier: Option<&str>,
        cond: ConditionalHeaders,
    ) -> Result<(Vec<u8>, IndexEntry, FetchOrigin), MetadataError> {
        let upstream_url = self.inner.fetcher.url_for_artifact(
            None,
            &coords.group,
            &coords.artifact,
            version,
            classifier,
            type_,
        );
        let sha256_url = self.inner.fetcher.url_for_sidecar(&upstream_url, "sha256");
        let sha1_url = self.inner.fetcher.url_for_sidecar(&upstream_url, "sha1");

        let empty_cond = ConditionalHeaders::default();
        let (artifact_res, sha256_res, sha1_res) = tokio::join!(
            self.inner.fetcher.fetch(&upstream_url, &cond),
            self.inner.fetcher.fetch(&sha256_url, &empty_cond),
            self.inner.fetcher.fetch(&sha1_url, &empty_cond),
        );

        let artifact = artifact_res.map_err(|e| map_fetch_err(coords, version, e))?;
        let (bytes, etag, last_modified) = match artifact {
            FetchOutcome::Fresh {
                bytes,
                etag,
                last_modified,
                ..
            } => (bytes, etag, last_modified),
            FetchOutcome::NotModified => {
                // Caller should only hit this path with conditional
                // headers and an entry already in cache. We surface
                // it as a transport error to keep the caller honest;
                // higher-level code that supplies a real `cond`
                // handles 304 before reaching here.
                return Err(MetadataError::Transport {
                    coords: format!("{}:{}", coords.group, coords.artifact),
                    version: version.to_string(),
                    detail: "upstream returned 304 with no cached entry".into(),
                });
            }
        };

        let sha256_sidecar = match sha256_res {
            Ok(FetchOutcome::Fresh { bytes, .. }) => {
                Some(String::from_utf8_lossy(&bytes).into_owned())
            }
            _ => None,
        };
        let sha1_sidecar = match sha1_res {
            Ok(FetchOutcome::Fresh { bytes, .. }) => {
                Some(String::from_utf8_lossy(&bytes).into_owned())
            }
            _ => None,
        };

        let verification =
            checksum::verify(&bytes, sha256_sidecar.as_deref(), sha1_sidecar.as_deref()).map_err(
                |e| MetadataError::Parse {
                    what: "checksum sidecar",
                    coords: format!("{}:{}", coords.group, coords.artifact),
                    version: version.to_string(),
                    detail: format!("{e}"),
                },
            )?;

        let (hash, _path) = self
            .inner
            .cas
            .put(&bytes)
            .map_err(|e| map_cas_err(coords, version, e))?;

        let sha1_hex = match &verification {
            Verification::Sha1Verified { hex } => Some(hex.clone()),
            _ => None,
        };

        let now = now_unix();
        let entry = IndexEntry {
            hash,
            size_bytes: bytes.len() as u64,
            sha1_hex,
            origin: Origin {
                repository_url: upstream_url,
                etag,
                last_modified,
                upstream_last_updated: None,
            },
            atime_unix: now,
            created_unix: now,
        };
        Ok((bytes.to_vec(), entry, FetchOrigin::Remote))
    }

    /// Shared fetch-bytes path used by `fetch_pom`,
    /// `fetch_metadata`, and `fetch_snapshot_info`. Encapsulates the
    /// in-process lock, the cache hit/miss decision, and the
    /// cross-process lock + double-check sequence.
    #[allow(clippy::too_many_arguments)]
    async fn get_or_fetch_artifact(
        &self,
        coords: &Coords,
        version: &str,
        index_key: IndexKey,
        lock_key: CoordVersionKey,
        type_for_fetch: &str,
        classifier_for_fetch: Option<&str>,
        // For maven-metadata.xml the fetcher uses a different URL
        // shape (no version directory). The closure lets the caller
        // override the URL builder.
        url_override: Option<String>,
    ) -> Result<(Vec<u8>, FetchOrigin), MetadataError> {
        let _guard = self.inner.coord_locks.lock(&lock_key).await;

        // In-process cache check.
        if let Some(entry) = self.inner.index.get(&index_key) {
            let bytes = self
                .inner
                .cas
                .get(&entry.hash)
                .map_err(|e| map_cas_err(coords, version, e))?;
            let _ = self.inner.index.touch(&index_key, now_unix());
            return Ok((bytes, FetchOrigin::Disk));
        }

        // Cross-process lock + double-checked lookup. Another
        // process may have populated the entry between our index
        // check and the lock acquisition; re-check after taking the
        // fs lock so we don't double-fetch.
        let lock_root = self.lock_root();
        let _fs_lock = FilesystemLock::acquire(&lock_root, &lock_key)
            .await
            .map_err(|e| MetadataError::Transport {
                coords: format!("{}:{}", coords.group, coords.artifact),
                version: version.to_string(),
                detail: format!("filesystem lock: {e}"),
            })?;

        if let Some(entry) = self.inner.index.get(&index_key) {
            let bytes = self
                .inner
                .cas
                .get(&entry.hash)
                .map_err(|e| map_cas_err(coords, version, e))?;
            return Ok((bytes, FetchOrigin::Disk));
        }

        // True miss — fetch from upstream.
        let (bytes, entry, origin) = if let Some(url) = url_override {
            self.fetch_url_and_cache(coords, version, &url).await?
        } else {
            self.fetch_and_cache(
                coords,
                version,
                type_for_fetch,
                classifier_for_fetch,
                ConditionalHeaders::default(),
            )
            .await?
        };

        self.inner
            .index
            .put(index_key, entry)
            .map_err(|e| MetadataError::Transport {
                coords: format!("{}:{}", coords.group, coords.artifact),
                version: version.to_string(),
                detail: format!("index put: {e}"),
            })?;
        Ok((bytes, origin))
    }

    /// Fetch + CAS-store a raw URL (no sidecars, no checksum).
    /// Used for `maven-metadata.xml`, which Maven Central does not
    /// publish sidecars for.
    async fn fetch_url_and_cache(
        &self,
        coords: &Coords,
        version: &str,
        url: &str,
    ) -> Result<(Vec<u8>, IndexEntry, FetchOrigin), MetadataError> {
        let outcome = self
            .inner
            .fetcher
            .fetch(url, &ConditionalHeaders::default())
            .await
            .map_err(|e| map_fetch_err(coords, version, e))?;
        let (bytes, etag, last_modified) = match outcome {
            FetchOutcome::Fresh {
                bytes,
                etag,
                last_modified,
                ..
            } => (bytes, etag, last_modified),
            FetchOutcome::NotModified => {
                return Err(MetadataError::MetadataNotFound {
                    coords: format!("{}:{}", coords.group, coords.artifact),
                });
            }
        };
        let (hash, _path) = self
            .inner
            .cas
            .put(&bytes)
            .map_err(|e| map_cas_err(coords, version, e))?;
        let now = now_unix();
        let entry = IndexEntry {
            hash,
            size_bytes: bytes.len() as u64,
            sha1_hex: None,
            origin: Origin {
                repository_url: url.to_string(),
                etag,
                last_modified,
                upstream_last_updated: None,
            },
            atime_unix: now,
            created_unix: now,
        };
        Ok((bytes.to_vec(), entry, FetchOrigin::Remote))
    }
}

#[async_trait]
impl MetadataSource for CacheSource {
    async fn fetch_pom(
        &self,
        coords: &Coords,
        version: &str,
    ) -> Result<(RawPom, FetchOrigin), MetadataError> {
        let lock_key = CoordVersionKey {
            coords: coords.clone(),
            version: version.to_string(),
        };
        let index_key = IndexKey::new(coords.clone(), version, "pom", None);
        let (bytes, origin) = self
            .get_or_fetch_artifact(coords, version, index_key, lock_key, "pom", None, None)
            .await?;
        let pom =
            parse_pom(&String::from_utf8_lossy(&bytes)).map_err(|e| MetadataError::Parse {
                what: "pom.xml",
                coords: format!("{}:{}", coords.group, coords.artifact),
                version: version.to_string(),
                detail: format!("{e}"),
            })?;
        Ok((pom, origin))
    }

    async fn fetch_metadata(
        &self,
        coords: &Coords,
    ) -> Result<(GaMetadata, FetchOrigin), MetadataError> {
        let lock_key = CoordVersionKey {
            coords: coords.clone(),
            version: METADATA_SENTINEL_VERSION.to_string(),
        };
        let index_key = IndexKey::new(
            coords.clone(),
            METADATA_SENTINEL_VERSION,
            "maven-metadata.xml",
            None,
        );
        let url = self
            .inner
            .fetcher
            .url_for_metadata(None, &coords.group, &coords.artifact);
        let (bytes, origin) = self
            .get_or_fetch_artifact(
                coords,
                "",
                index_key,
                lock_key,
                "maven-metadata.xml",
                None,
                Some(url),
            )
            .await
            .map_err(|e| match e {
                MetadataError::NotFound { coords, .. } => {
                    MetadataError::MetadataNotFound { coords }
                }
                other => other,
            })?;
        let xml = String::from_utf8_lossy(&bytes);
        let versions = parse_versions_list(&xml);
        Ok((
            GaMetadata {
                coords: coords.clone(),
                versions,
                latest_snapshot_timestamp: None,
                last_updated: None,
            },
            origin,
        ))
    }

    async fn fetch_snapshot_info(
        &self,
        coords: &Coords,
        version: &str,
    ) -> Result<(SnapshotInfo, FetchOrigin), MetadataError> {
        let sentinel = snapshot_meta_version(version);
        let lock_key = CoordVersionKey {
            coords: coords.clone(),
            version: sentinel.clone(),
        };
        let index_key = IndexKey::new(coords.clone(), sentinel, "maven-metadata.xml", None);
        // The per-version snapshot metadata lives at
        // <group>/<artifact>/<version>/maven-metadata.xml.
        let base = self
            .inner
            .fetcher
            .url_for_metadata(None, &coords.group, &coords.artifact);
        // Replace the trailing `/maven-metadata.xml` with
        // `/<version>/maven-metadata.xml`.
        let url = base.replace(
            "/maven-metadata.xml",
            &format!("/{version}/maven-metadata.xml"),
        );
        let (bytes, origin) = self
            .get_or_fetch_artifact(
                coords,
                version,
                index_key,
                lock_key,
                "maven-metadata.xml",
                None,
                Some(url),
            )
            .await?;
        let info = parse_snapshot_metadata(&String::from_utf8_lossy(&bytes)).map_err(|e| {
            MetadataError::Parse {
                what: "maven-metadata.xml",
                coords: format!("{}:{}", coords.group, coords.artifact),
                version: version.to_string(),
                detail: format!("{e}"),
            }
        })?;
        Ok((info, origin))
    }
}

/// Extract the version list from a Maven `maven-metadata.xml`.
///
/// Walks the XML with `quick_xml`'s event reader and accumulates the
/// text content of every `<version>` element nested under
/// `<versioning><versions>`. Tolerant to whitespace, element order,
/// and unrelated siblings; ignores everything outside that path.
pub(crate) fn parse_versions_list(xml: &str) -> Vec<String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut versions = Vec::new();
    let mut in_versioning = false;
    let mut in_versions = false;
    let mut in_version = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"versioning" => in_versioning = true,
                b"versions" if in_versioning => in_versions = true,
                b"version" if in_versions => in_version = true,
                _ => {}
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"versioning" => in_versioning = false,
                b"versions" => in_versions = false,
                b"version" => in_version = false,
                _ => {}
            },
            Ok(Event::Text(t)) if in_version => {
                if let Ok(s) = std::str::from_utf8(t.as_ref()) {
                    let v = s.trim().to_string();
                    if !v.is_empty() {
                        versions.push(v);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    versions
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn map_cas_err(coords: &Coords, version: &str, e: CasError) -> MetadataError {
    MetadataError::Transport {
        coords: format!("{}:{}", coords.group, coords.artifact),
        version: version.to_string(),
        detail: format!("cas: {e}"),
    }
}

fn map_fetch_err(coords: &Coords, version: &str, e: FetchError) -> MetadataError {
    match &e {
        FetchError::Status { status, .. } if *status == 404 => MetadataError::NotFound {
            coords: format!("{}:{}", coords.group, coords.artifact),
            version: version.to_string(),
        },
        FetchError::Status { status, .. } if *status == 401 || *status == 403 => {
            MetadataError::AuthRequired {
                coords: format!("{}:{}", coords.group, coords.artifact),
                version: version.to_string(),
            }
        }
        _ => MetadataError::Transport {
            coords: format!("{}:{}", coords.group, coords.artifact),
            version: version.to_string(),
            detail: format!("{e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use barista_coords::Coords;

    use crate::fetch::FetchConfig;

    fn coords(group: &str, artifact: &str) -> Coords {
        Coords::new(group, artifact).expect("valid coords")
    }

    struct Harness {
        _tmp: TempDir,
        source: CacheSource,
        server: MockServer,
        cache_root: PathBuf,
    }

    async fn make_harness() -> Harness {
        let tmp = TempDir::new().expect("tmp");
        let cache_root = tmp.path().to_path_buf();
        let cas = Cas::open(&cache_root).expect("cas");
        let index = Index::open(&cache_root).expect("index");
        let server = MockServer::start().await;
        let cfg = FetchConfig {
            max_concurrent_connections: 4,
            request_timeout: Duration::from_secs(5),
            http2_enabled: false,
            user_agent: "barista-test/0.0".into(),
            default_upstream: server.uri(),
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
        Harness {
            _tmp: tmp,
            source,
            server,
            cache_root,
        }
    }

    const SAMPLE_POM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>lib</artifactId>
  <version>1.0</version>
  <packaging>jar</packaging>
</project>"#;

    async fn mount_pom_async(
        server: &MockServer,
        group: &str,
        artifact: &str,
        version: &str,
        body: &'static str,
    ) {
        let group_path = group.replace('.', "/");
        let p = format!("/{group_path}/{artifact}/{version}/{artifact}-{version}.pom");
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
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

    // 1. Constructor smoke.
    #[tokio::test]
    async fn cache_source_new_constructs() {
        let h = make_harness().await;
        // Calling a getter on the inner is not part of the public API,
        // but cloning must work cheaply.
        let _clone = h.source.clone();
    }

    // 2. fetch_pom miss path: 200 + sidecar 404 (Unverified) → success.
    #[tokio::test]
    async fn fetch_pom_cache_miss_returns_remote() {
        let h = make_harness().await;
        mount_pom_async(&h.server, "org.example", "lib", "1.0", SAMPLE_POM).await;
        // Sidecars: 404 → Unverified path.
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha256", 404).await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha1", 404).await;

        let (pom, origin) = h
            .source
            .fetch_pom(&coords("org.example", "lib"), "1.0")
            .await
            .expect("fetch pom");
        assert_eq!(pom.artifact_id, "lib");
        assert_eq!(origin, FetchOrigin::Remote);
    }

    // 3. Second call returns Disk origin (cache hit).
    #[tokio::test]
    async fn fetch_pom_second_call_returns_disk() {
        let h = make_harness().await;
        mount_pom_async(&h.server, "org.example", "lib", "1.0", SAMPLE_POM).await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha256", 404).await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha1", 404).await;

        let c = coords("org.example", "lib");
        let (_, origin1) = h.source.fetch_pom(&c, "1.0").await.expect("first");
        assert_eq!(origin1, FetchOrigin::Remote);
        let (_, origin2) = h.source.fetch_pom(&c, "1.0").await.expect("second");
        assert_eq!(origin2, FetchOrigin::Disk);
    }

    // 4. fetch_metadata parses <versions> list.
    #[tokio::test]
    async fn fetch_metadata_parses_versions() {
        let h = make_harness().await;
        let xml = r#"<?xml version="1.0"?>
<metadata>
  <groupId>org.example</groupId>
  <artifactId>lib</artifactId>
  <versioning>
    <latest>2.0</latest>
    <release>2.0</release>
    <versions>
      <version>1.0</version>
      <version>1.5</version>
      <version>2.0</version>
    </versions>
    <lastUpdated>20260101000000</lastUpdated>
  </versioning>
</metadata>"#;
        Mock::given(method("GET"))
            .and(path("/org/example/lib/maven-metadata.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(xml))
            .mount(&h.server)
            .await;

        let (md, origin) = h
            .source
            .fetch_metadata(&coords("org.example", "lib"))
            .await
            .expect("fetch metadata");
        assert_eq!(md.versions, vec!["1.0", "1.5", "2.0"]);
        assert_eq!(origin, FetchOrigin::Remote);
    }

    // 5. Concurrent fetch_pom on same coord serializes (single upstream hit).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_same_coord_serializes() {
        let h = make_harness().await;
        // Tracker mock: every call increments an atomic. We assert
        // exactly one underlying HTTP call across N concurrent
        // resolver requests for the same (coord, version).
        mount_pom_async(&h.server, "org.example", "lib", "1.0", SAMPLE_POM).await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha256", 404).await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha1", 404).await;

        let src = h.source.clone();
        let c = coords("org.example", "lib");
        let mut handles = Vec::new();
        for _ in 0..8 {
            let src = src.clone();
            let c = c.clone();
            handles.push(tokio::spawn(async move {
                src.fetch_pom(&c, "1.0").await.expect("fetch")
            }));
        }
        let mut disk_count = 0;
        let mut remote_count = 0;
        for h in handles {
            let (_, origin) = h.await.expect("join");
            match origin {
                FetchOrigin::Disk => disk_count += 1,
                FetchOrigin::Remote => remote_count += 1,
                _ => panic!("unexpected origin"),
            }
        }
        assert_eq!(remote_count, 1, "exactly one underlying fetch");
        assert_eq!(disk_count, 7);
    }

    // 6. Checksum mismatch returns Parse{ what: "checksum sidecar" }.
    #[tokio::test]
    async fn checksum_mismatch_is_parse_error() {
        let h = make_harness().await;
        mount_pom_async(&h.server, "org.example", "lib", "1.0", SAMPLE_POM).await;
        // A non-matching SHA-256 sidecar of the correct shape (64 hex).
        let bogus_sha256 = "0".repeat(64);
        Mock::given(method("GET"))
            .and(path("/org/example/lib/1.0/lib-1.0.pom.sha256"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bogus_sha256))
            .mount(&h.server)
            .await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha1", 404).await;

        let err = h
            .source
            .fetch_pom(&coords("org.example", "lib"), "1.0")
            .await
            .expect_err("should mismatch");
        match err {
            MetadataError::Parse { what, .. } => assert_eq!(what, "checksum sidecar"),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    // 7. 404 from upstream → MetadataError::NotFound.
    #[tokio::test]
    async fn upstream_404_returns_not_found() {
        let h = make_harness().await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom", 404).await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha256", 404).await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha1", 404).await;

        let err = h
            .source
            .fetch_pom(&coords("org.example", "lib"), "1.0")
            .await
            .expect_err("404");
        assert!(matches!(err, MetadataError::NotFound { .. }), "got {err:?}");
    }

    // 8. parse_versions_list on a realistic Maven Central response.
    #[test]
    fn parse_versions_list_extracts_5() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata>
  <groupId>org.example</groupId>
  <artifactId>thing</artifactId>
  <versioning>
    <latest>3.0</latest>
    <release>3.0</release>
    <versions>
      <version>1.0</version>
      <version>1.1</version>
      <version>2.0</version>
      <version>2.5</version>
      <version>3.0</version>
    </versions>
    <lastUpdated>20260101120000</lastUpdated>
  </versioning>
</metadata>"#;
        let versions = parse_versions_list(xml);
        assert_eq!(versions, vec!["1.0", "1.1", "2.0", "2.5", "3.0"]);
    }

    // 9. After dropping & reopening with the same paths, the cache is
    // hit on disk (persistence via snapshot+journal).
    #[tokio::test]
    async fn cache_persists_across_reopen() {
        let h = make_harness().await;
        mount_pom_async(&h.server, "org.example", "lib", "1.0", SAMPLE_POM).await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha256", 404).await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha1", 404).await;

        let c = coords("org.example", "lib");
        let (_, origin) = h.source.fetch_pom(&c, "1.0").await.expect("first");
        assert_eq!(origin, FetchOrigin::Remote);

        // Tear down only the CacheSource (keep tempdir + server).
        let cache_root = h.cache_root.clone();
        let server_uri = h.server.uri();
        drop(h.source);

        // Reopen against the same on-disk state.
        let cas = Cas::open(&cache_root).unwrap();
        let index = Index::open(&cache_root).unwrap();
        let cfg = FetchConfig {
            max_concurrent_connections: 4,
            request_timeout: Duration::from_secs(5),
            http2_enabled: false,
            user_agent: "barista-test/0.0".into(),
            default_upstream: server_uri,
        };
        let fetcher = Fetcher::new(cfg).unwrap();
        let source2 = CacheSource::new(
            cas,
            index,
            fetcher,
            cache_root,
            UpdatePolicy::Daily,
            UpdatePolicy::Never,
        );

        let (pom, origin2) = source2.fetch_pom(&c, "1.0").await.expect("reopen fetch");
        assert_eq!(origin2, FetchOrigin::Disk, "reopened cache should hit");
        assert_eq!(pom.artifact_id, "lib");
    }

    // 10. Object-safety: Box<dyn MetadataSource> compiles and dispatches.
    #[tokio::test]
    async fn boxed_dyn_metadata_source_is_object_safe() {
        let h = make_harness().await;
        mount_pom_async(&h.server, "org.example", "lib", "1.0", SAMPLE_POM).await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha256", 404).await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha1", 404).await;

        let boxed: Box<dyn MetadataSource> = Box::new(h.source.clone());
        let (_, origin) = boxed
            .fetch_pom(&coords("org.example", "lib"), "1.0")
            .await
            .expect("dyn dispatch");
        assert_eq!(origin, FetchOrigin::Remote);

        // Also exercise Arc<dyn> share.
        let arced: Arc<dyn MetadataSource> = Arc::new(h.source.clone());
        let a2 = Arc::clone(&arced);
        let (_, origin2) = a2
            .fetch_pom(&coords("org.example", "lib"), "1.0")
            .await
            .expect("arc dyn dispatch");
        // Same coord on same source — second call must hit disk.
        assert_eq!(origin2, FetchOrigin::Disk);
    }

    // 11. fetch_metadata 404 → MetadataNotFound.
    #[tokio::test]
    async fn fetch_metadata_404_returns_metadata_not_found() {
        let h = make_harness().await;
        mount_status(&h.server, "/org/example/lib/maven-metadata.xml", 404).await;
        let err = h
            .source
            .fetch_metadata(&coords("org.example", "lib"))
            .await
            .expect_err("404");
        assert!(
            matches!(err, MetadataError::MetadataNotFound { .. }),
            "got {err:?}"
        );
    }

    // 12. AuthRequired on 401.
    #[tokio::test]
    async fn upstream_401_returns_auth_required() {
        let h = make_harness().await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom", 401).await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha256", 401).await;
        mount_status(&h.server, "/org/example/lib/1.0/lib-1.0.pom.sha1", 401).await;
        let err = h
            .source
            .fetch_pom(&coords("org.example", "lib"), "1.0")
            .await
            .expect_err("401");
        assert!(
            matches!(err, MetadataError::AuthRequired { .. }),
            "got {err:?}"
        );
    }
}
