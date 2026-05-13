//! `MetadataSource` — the sole resolver↔cache interface.
//!
//! The resolver crate does NOT depend on the cache crate. Instead,
//! both share this trait: the cache provides the production impl,
//! and an offline fixture-backed impl lets resolver tests run
//! without a real cache.
//!
//! The trait is async so production implementations can do HTTP I/O
//! under the hood. The resolver's BFS walker is sync with a
//! deterministic frontier order; it `block_on`'s individual fetches
//! through the trait. Parallelism comes from the cache's connection
//! pool, NOT from out-of-order resolver traversal.

use async_trait::async_trait;
use barista_coords::Coords;
use barista_pom::RawPom;

/// Coordinate identity used by the resolver. Maven's group+artifact
/// pair is the resolution-conflict key; type+classifier participate
/// only in artifact-file identity (GATC).
pub type ResolveKey = Coords;

/// A version string as-typed. The resolver parses it via
/// `barista_version::Version` for ordering.
pub type VersionString = String;

/// Errors a [`MetadataSource`] implementation can return. Production
/// implementations may layer additional context on top via their own
/// error types; the trait surface keeps these tight.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("artifact {coords} version {version} not found")]
    NotFound { coords: String, version: String },

    #[error("metadata for {coords} not found (no versions known)")]
    MetadataNotFound { coords: String },

    #[error("transport error fetching {coords}:{version}: {detail}")]
    Transport {
        coords: String,
        version: String,
        detail: String,
    },

    #[error("parse error in {what} for {coords}:{version}: {detail}")]
    Parse {
        what: &'static str,
        coords: String,
        version: String,
        detail: String,
    },

    #[error("authentication required for {coords}:{version}")]
    AuthRequired { coords: String, version: String },

    #[error("the source is offline and {coords}:{version} is not cached")]
    Offline { coords: String, version: String },
}

/// Maven `<metadata>` payload — the gist of `maven-metadata.xml`.
/// Used to resolve `LATEST`/`RELEASE` and SNAPSHOT versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GaMetadata {
    pub coords: ResolveKey,
    /// Every version published for this coord, in publication order
    /// (oldest first). The resolver picks `LATEST` = last; `RELEASE`
    /// = the last non-SNAPSHOT.
    pub versions: Vec<VersionString>,
    /// "Effective" snapshot timestamp for the latest SNAPSHOT
    /// version, if the metadata file declares one (Maven 3+
    /// snapshot-versioned uniqueness).
    pub latest_snapshot_timestamp: Option<String>,
    /// When the metadata was last updated upstream (RFC3339 string).
    pub last_updated: Option<String>,
}

/// Per-request observation. Source implementations report whether
/// the answer came from a hot in-memory cache, on-disk cache, a
/// remote fetch, or an offline fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOrigin {
    InMemory,
    Disk,
    Remote,
    /// Offline fixture-backed impl (resolver test harness).
    Fixture,
}

/// The resolver↔cache interface.
///
/// Production cache impl lives in `barista-cache`; fixture impl in
/// `barista-resolver` tests. Implementations must be `Send + Sync`
/// so an `Arc<dyn MetadataSource>` can be shared across the BFS
/// walker's task pool.
#[async_trait]
pub trait MetadataSource: Send + Sync {
    /// Fetch the raw POM for a specific coordinate + version.
    /// Returns [`MetadataError::NotFound`] if the coord/version
    /// doesn't exist on the configured upstreams or in cache.
    async fn fetch_pom(
        &self,
        coords: &ResolveKey,
        version: &str,
    ) -> Result<(RawPom, FetchOrigin), MetadataError>;

    /// Fetch the group:artifact `maven-metadata.xml` payload.
    async fn fetch_metadata(
        &self,
        coords: &ResolveKey,
    ) -> Result<(GaMetadata, FetchOrigin), MetadataError>;

    /// Hint that the resolver will probably want `coords:version`
    /// soon. Implementations MAY pre-fetch in the background; the
    /// default does nothing. Used to overlap I/O with BFS traversal.
    async fn warm(&self, _coords: &ResolveKey, _version: &str) -> Result<(), MetadataError> {
        Ok(())
    }
}

/// A no-op [`MetadataSource`] that errors `NotFound` on every call.
///
/// Useful as a placeholder where a real source is required but the
/// test only exercises non-fetching paths (e.g., serializing a
/// pre-built dependency graph).
pub struct NullMetadataSource;

#[async_trait]
impl MetadataSource for NullMetadataSource {
    async fn fetch_pom(
        &self,
        coords: &ResolveKey,
        version: &str,
    ) -> Result<(RawPom, FetchOrigin), MetadataError> {
        Err(MetadataError::NotFound {
            coords: format!("{}:{}", coords.group, coords.artifact),
            version: version.to_string(),
        })
    }

    async fn fetch_metadata(
        &self,
        coords: &ResolveKey,
    ) -> Result<(GaMetadata, FetchOrigin), MetadataError> {
        Err(MetadataError::MetadataNotFound {
            coords: format!("{}:{}", coords.group, coords.artifact),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn coords(group: &str, artifact: &str) -> Coords {
        Coords::new(group, artifact).expect("valid coords")
    }

    #[tokio::test]
    async fn null_source_fetch_pom_returns_not_found() {
        let src = NullMetadataSource;
        let c = coords("org.example", "lib");
        let err = src.fetch_pom(&c, "1.0").await.unwrap_err();
        match err {
            MetadataError::NotFound { coords, version } => {
                assert_eq!(coords, "org.example:lib");
                assert_eq!(version, "1.0");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn null_source_fetch_metadata_returns_metadata_not_found() {
        let src = NullMetadataSource;
        let c = coords("org.example", "lib");
        let err = src.fetch_metadata(&c).await.unwrap_err();
        match err {
            MetadataError::MetadataNotFound { coords } => {
                assert_eq!(coords, "org.example:lib");
            }
            other => panic!("expected MetadataNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn null_source_warm_is_ok_by_default() {
        let src = NullMetadataSource;
        let c = coords("org.example", "lib");
        src.warm(&c, "1.0").await.expect("default warm is Ok");
    }

    #[test]
    fn not_found_display_includes_coords_and_version() {
        let e = MetadataError::NotFound {
            coords: "g:a".to_string(),
            version: "1.2.3".to_string(),
        };
        let s = format!("{e}");
        assert!(s.contains("g:a"), "missing coords in: {s}");
        assert!(s.contains("1.2.3"), "missing version in: {s}");
    }

    #[test]
    fn transport_display_includes_detail() {
        let e = MetadataError::Transport {
            coords: "g:a".into(),
            version: "1.0".into(),
            detail: "connection reset".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("connection reset"), "missing detail in: {s}");
    }

    #[test]
    fn ga_metadata_clones() {
        let m = GaMetadata {
            coords: coords("g", "a"),
            versions: vec!["1.0".into(), "2.0".into()],
            latest_snapshot_timestamp: Some("20260101.000000-1".into()),
            last_updated: Some("2026-01-01T00:00:00Z".into()),
        };
        let m2 = m.clone();
        assert_eq!(m, m2);
    }

    #[test]
    fn fetch_origin_variants_eq() {
        assert_eq!(FetchOrigin::InMemory, FetchOrigin::InMemory);
        assert_ne!(FetchOrigin::InMemory, FetchOrigin::Disk);
        assert_ne!(FetchOrigin::Disk, FetchOrigin::Remote);
        assert_ne!(FetchOrigin::Remote, FetchOrigin::Fixture);
    }

    /// A tiny hand-rolled source returning a hardcoded RawPom for a
    /// single coordinate. Exercises the trait surface end-to-end.
    struct FixedSource {
        coords: Coords,
        version: String,
    }

    #[async_trait]
    impl MetadataSource for FixedSource {
        async fn fetch_pom(
            &self,
            coords: &ResolveKey,
            version: &str,
        ) -> Result<(RawPom, FetchOrigin), MetadataError> {
            if coords == &self.coords && version == self.version {
                let pom = RawPom {
                    model_version: "4.0.0".into(),
                    group_id: Some(coords.group.clone()),
                    artifact_id: coords.artifact.clone(),
                    version: Some(version.to_string()),
                    packaging: "jar".into(),
                    ..RawPom::default()
                };
                Ok((pom, FetchOrigin::Fixture))
            } else {
                Err(MetadataError::NotFound {
                    coords: format!("{}:{}", coords.group, coords.artifact),
                    version: version.to_string(),
                })
            }
        }

        async fn fetch_metadata(
            &self,
            coords: &ResolveKey,
        ) -> Result<(GaMetadata, FetchOrigin), MetadataError> {
            if coords == &self.coords {
                Ok((
                    GaMetadata {
                        coords: coords.clone(),
                        versions: vec![self.version.clone()],
                        latest_snapshot_timestamp: None,
                        last_updated: None,
                    },
                    FetchOrigin::Fixture,
                ))
            } else {
                Err(MetadataError::MetadataNotFound {
                    coords: format!("{}:{}", coords.group, coords.artifact),
                })
            }
        }
    }

    #[tokio::test]
    async fn fixed_source_returns_hardcoded_pom() {
        let src = FixedSource {
            coords: coords("org.example", "lib"),
            version: "1.0".into(),
        };
        let (pom, origin) = src
            .fetch_pom(&coords("org.example", "lib"), "1.0")
            .await
            .expect("fixture hit");
        assert_eq!(pom.artifact_id, "lib");
        assert_eq!(pom.version.as_deref(), Some("1.0"));
        assert_eq!(origin, FetchOrigin::Fixture);

        // Miss path.
        let err = src
            .fetch_pom(&coords("org.example", "other"), "1.0")
            .await
            .unwrap_err();
        assert!(matches!(err, MetadataError::NotFound { .. }));
    }

    #[tokio::test]
    async fn boxed_dyn_metadata_source_is_object_safe() {
        let boxed: Box<dyn MetadataSource> = Box::new(NullMetadataSource);
        let c = coords("g", "a");
        let err = boxed.fetch_pom(&c, "1.0").await.unwrap_err();
        assert!(matches!(err, MetadataError::NotFound { .. }));
    }

    #[tokio::test]
    async fn arc_dyn_metadata_source_is_shareable() {
        let src: Arc<dyn MetadataSource> = Arc::new(NullMetadataSource);
        let s2 = Arc::clone(&src);
        let c = coords("g", "a");
        let err = s2.fetch_metadata(&c).await.unwrap_err();
        assert!(matches!(err, MetadataError::MetadataNotFound { .. }));
    }
}
