// Integration-test / example / benchmark target — workspace security
// lints are allowed here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`)
// is the documented contract for failing a test loudly. This allow block
// keeps the crate root's `#![allow(...)]` from being silently dropped by
// the separate compilation unit each test file forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Smoke tests for `FixtureMetadataSource`.
//!
//! Verifies the loader, the trait impl, and the seed fixtures all
//! agree. T2 (walker integration tests) and T7 (golden-tree tests)
//! consume the same fixtures via the shared `common` module.

mod common;

use std::sync::Arc;

use barista_coords::Coords;
use barista_resolver::source::{FetchOrigin, MetadataError, MetadataSource};

use common::fixture_source::FixtureMetadataSource;

fn coords(group: &str, artifact: &str) -> Coords {
    Coords::new(group, artifact).expect("valid coords")
}

#[tokio::test]
async fn loads_default_fixtures() {
    let src = FixtureMetadataSource::load_default().expect("default fixtures must load cleanly");
    assert!(
        src.pom_count() >= 3,
        "expected ≥3 POMs loaded, got {}",
        src.pom_count()
    );
    assert!(
        src.metadata_count() >= 3,
        "expected ≥3 maven-metadata entries, got {}",
        src.metadata_count()
    );
}

#[tokio::test]
async fn fetch_pom_returns_fixture_origin() {
    let src = FixtureMetadataSource::load_default().unwrap();
    let c = coords("org.apache.commons", "commons-lang3");
    let (pom, origin) = src.fetch_pom(&c, "3.14.0").await.expect("fixture present");
    assert_eq!(pom.artifact_id, "commons-lang3");
    assert_eq!(pom.version.as_deref(), Some("3.14.0"));
    assert_eq!(origin, FetchOrigin::Fixture);
}

#[tokio::test]
async fn fetch_metadata_returns_versions_in_order() {
    let src = FixtureMetadataSource::load_default().unwrap();
    let c = coords("org.apache.commons", "commons-lang3");
    let (md, origin) = src.fetch_metadata(&c).await.expect("metadata present");
    assert_eq!(origin, FetchOrigin::Fixture);
    assert_eq!(md.coords, c);
    assert!(md.versions.contains(&"3.14.0".to_string()));
    assert!(md.versions.contains(&"3.15.0".to_string()));
    // Order preserved from the XML.
    let i14 = md.versions.iter().position(|v| v == "3.14.0").unwrap();
    let i15 = md.versions.iter().position(|v| v == "3.15.0").unwrap();
    assert!(i14 < i15, "versions must be ordered as written in XML");
    assert!(md.last_updated.is_some());
}

#[tokio::test]
async fn multiple_versions_of_same_coord_load() {
    let src = FixtureMetadataSource::load_default().unwrap();
    let c = coords("org.apache.commons", "commons-lang3");
    let (p14, _) = src.fetch_pom(&c, "3.14.0").await.unwrap();
    let (p15, _) = src.fetch_pom(&c, "3.15.0").await.unwrap();
    assert_eq!(p14.version.as_deref(), Some("3.14.0"));
    assert_eq!(p15.version.as_deref(), Some("3.15.0"));
    // Different parent versions confirm they're distinct POMs.
    assert_ne!(
        p14.parent.as_ref().map(|p| &p.version),
        p15.parent.as_ref().map(|p| &p.version),
    );
}

#[tokio::test]
async fn missing_coord_returns_not_found() {
    let src = FixtureMetadataSource::load_default().unwrap();
    let c = coords("made.up", "doesnt-exist");
    let err = src.fetch_pom(&c, "1.0").await.unwrap_err();
    assert!(
        matches!(err, MetadataError::NotFound { .. }),
        "expected NotFound, got {err:?}"
    );
}

#[tokio::test]
async fn missing_version_returns_not_found() {
    let src = FixtureMetadataSource::load_default().unwrap();
    let c = coords("org.apache.commons", "commons-lang3");
    // A version that exists in metadata but has no pom.xml fixture.
    let err = src.fetch_pom(&c, "3.13.0").await.unwrap_err();
    assert!(
        matches!(err, MetadataError::NotFound { .. }),
        "expected NotFound for unseeded version, got {err:?}"
    );
}

#[tokio::test]
async fn missing_metadata_returns_metadata_not_found() {
    let src = FixtureMetadataSource::load_default().unwrap();
    let c = coords("made.up", "doesnt-exist");
    let err = src.fetch_metadata(&c).await.unwrap_err();
    assert!(
        matches!(err, MetadataError::MetadataNotFound { .. }),
        "expected MetadataNotFound, got {err:?}"
    );
}

#[tokio::test]
async fn shareable_via_arc_dyn() {
    // The walker holds an `Arc<dyn MetadataSource>`. Verify the
    // fixture source plugs in transparently.
    let src: Arc<dyn MetadataSource> = Arc::new(FixtureMetadataSource::load_default().unwrap());
    let s2 = Arc::clone(&src);
    let c = coords("commons-io", "commons-io");
    let (pom, _) = s2.fetch_pom(&c, "2.16.1").await.expect("hit");
    assert_eq!(pom.artifact_id, "commons-io");
}

#[tokio::test]
async fn empty_root_dir_loads_with_zero_entries() {
    let tmp = tempdir();
    let src = FixtureMetadataSource::load_from(&tmp.path).unwrap();
    assert_eq!(src.pom_count(), 0);
    assert_eq!(src.metadata_count(), 0);
}

#[tokio::test]
async fn nonexistent_root_dir_is_an_error() {
    let bogus = std::path::PathBuf::from("/tmp/definitely-not-a-real-fixtures-dir-xyz123");
    assert!(FixtureMetadataSource::load_from(&bogus).is_err());
}

// ---------------------------------------------------------------------------
// Tiny in-test tempdir helper. We don't want to pull in `tempfile` as a
// dev-dep just for two tests, so we use a manual scoped dir.
// ---------------------------------------------------------------------------

struct TempDir {
    path: std::path::PathBuf,
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn tempdir() -> TempDir {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("barista-resolver-fixture-test-{pid}-{n}"));
    std::fs::create_dir_all(&path).expect("create tempdir");
    TempDir { path }
}
