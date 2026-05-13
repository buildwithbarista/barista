//! Offline fixture-backed [`MetadataSource`] for resolver tests.
//!
//! Loads pre-fetched POMs and `maven-metadata.xml` snapshots from
//! `crates/barista-resolver/tests/fixtures/<groupId>/<artifactId>/`:
//!
//! ```text
//! tests/fixtures/
//! ├── commons-codec/commons-codec/
//! │   ├── maven-metadata.xml
//! │   ├── 1.16.0/pom.xml
//! │   └── 1.16.1/pom.xml
//! └── org.apache.commons/commons-lang3/
//!     └── 3.14.0/pom.xml
//! ```
//!
//! `groupId` is laid out flat (dots-as-dots, e.g. `org.apache.commons/`)
//! rather than the on-disk Maven repository layout (slashes,
//! `org/apache/commons/`) to keep the fixtures human-greppable.
//!
//! Everything is loaded once at construction time; subsequent
//! `fetch_pom` / `fetch_metadata` calls return clones from the
//! in-memory map. No I/O during a test run.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use barista_coords::Coords;
use barista_pom::raw::{RawPom, parse_pom};
use barista_resolver::source::{
    FetchOrigin, GaMetadata, MetadataError, MetadataSource, ResolveKey, VersionString,
};

/// In-process cache of POMs + maven-metadata.xml payloads loaded from
/// the test-fixtures tree on disk.
pub struct FixtureMetadataSource {
    poms: HashMap<(Coords, String), RawPom>,
    metadatas: HashMap<Coords, GaMetadata>,
}

impl FixtureMetadataSource {
    /// Load fixtures from the default location:
    /// `<crate>/tests/fixtures/`.
    pub fn load_default() -> Result<Self, LoadError> {
        let root = default_fixtures_root();
        Self::load_from(&root)
    }

    /// Load fixtures from a caller-supplied root. Useful for tests
    /// that want to spin up a synthetic corpus in a temp dir.
    pub fn load_from(root: &Path) -> Result<Self, LoadError> {
        let mut poms: HashMap<(Coords, String), RawPom> = HashMap::new();
        let mut metadatas: HashMap<Coords, GaMetadata> = HashMap::new();

        if !root.exists() {
            return Err(LoadError::BadPath {
                path: root.to_path_buf(),
                detail: "fixtures root does not exist".into(),
            });
        }

        for group_entry in read_dir(root)? {
            let group_path = group_entry.path();
            if !group_path.is_dir() {
                continue;
            }
            // README.md and other non-dir entries are ignored.
            let group_id = match group_path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };

            for artifact_entry in read_dir(&group_path)? {
                let artifact_path = artifact_entry.path();
                if !artifact_path.is_dir() {
                    continue;
                }
                let artifact_id = match artifact_path.file_name().and_then(|s| s.to_str()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };

                let coords = Coords::new(&group_id, &artifact_id).map_err(|e| {
                    LoadError::BadPath {
                        path: artifact_path.clone(),
                        detail: format!("invalid coords: {e}"),
                    }
                })?;

                // Optional maven-metadata.xml at the group:artifact level.
                let metadata_path = artifact_path.join("maven-metadata.xml");
                if metadata_path.is_file() {
                    let bytes = read_file(&metadata_path)?;
                    let text = std::str::from_utf8(&bytes).map_err(|e| {
                        LoadError::MetadataParse {
                            path: metadata_path.clone(),
                            detail: format!("non-UTF8 maven-metadata.xml: {e}"),
                        }
                    })?;
                    let parsed = parse_maven_metadata(text).map_err(|detail| {
                        LoadError::MetadataParse {
                            path: metadata_path.clone(),
                            detail,
                        }
                    })?;
                    metadatas.insert(
                        coords.clone(),
                        GaMetadata {
                            coords: coords.clone(),
                            versions: parsed.versions,
                            latest_snapshot_timestamp: parsed.latest_snapshot_timestamp,
                            last_updated: parsed.last_updated,
                        },
                    );
                }

                // Each subdir at this level is a version.
                for version_entry in read_dir(&artifact_path)? {
                    let version_path = version_entry.path();
                    if !version_path.is_dir() {
                        continue;
                    }
                    let version = match version_path.file_name().and_then(|s| s.to_str()) {
                        Some(s) => s.to_string(),
                        None => continue,
                    };

                    let pom_path = version_path.join("pom.xml");
                    if !pom_path.is_file() {
                        return Err(LoadError::BadPath {
                            path: version_path.clone(),
                            detail: "version directory missing pom.xml".into(),
                        });
                    }

                    let bytes = read_file(&pom_path)?;
                    let text = std::str::from_utf8(&bytes).map_err(|e| LoadError::PomParse {
                        path: pom_path.clone(),
                        detail: format!("non-UTF8 pom.xml: {e}"),
                    })?;
                    let pom = parse_pom(text).map_err(|e| LoadError::PomParse {
                        path: pom_path.clone(),
                        detail: format!("{e}"),
                    })?;

                    poms.insert((coords.clone(), version), pom);
                }
            }
        }

        Ok(Self { poms, metadatas })
    }

    /// Number of POMs loaded across all coord+version pairs.
    pub fn pom_count(&self) -> usize {
        self.poms.len()
    }

    /// Number of group:artifact metadata entries loaded.
    pub fn metadata_count(&self) -> usize {
        self.metadatas.len()
    }

    /// All (coords, version) pairs the source can answer.
    #[allow(dead_code)] // consumed by sibling integration tests (T2/T7) not yet landed
    pub fn pom_keys(&self) -> impl Iterator<Item = (&Coords, &str)> {
        self.poms.iter().map(|((c, v), _)| (c, v.as_str()))
    }
}

#[async_trait]
impl MetadataSource for FixtureMetadataSource {
    async fn fetch_pom(
        &self,
        coords: &ResolveKey,
        version: &str,
    ) -> Result<(RawPom, FetchOrigin), MetadataError> {
        let key = (coords.clone(), version.to_string());
        match self.poms.get(&key) {
            Some(p) => Ok((p.clone(), FetchOrigin::Fixture)),
            None => Err(MetadataError::NotFound {
                coords: format!("{}:{}", coords.group, coords.artifact),
                version: version.to_string(),
            }),
        }
    }

    async fn fetch_metadata(
        &self,
        coords: &ResolveKey,
    ) -> Result<(GaMetadata, FetchOrigin), MetadataError> {
        match self.metadatas.get(coords) {
            Some(m) => Ok((m.clone(), FetchOrigin::Fixture)),
            None => Err(MetadataError::MetadataNotFound {
                coords: format!("{}:{}", coords.group, coords.artifact),
            }),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("I/O error reading fixture at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("malformed fixture path {path:?}: {detail}")]
    BadPath { path: PathBuf, detail: String },
    #[error("POM parse error in {path:?}: {detail}")]
    PomParse { path: PathBuf, detail: String },
    #[error("metadata parse error in {path:?}: {detail}")]
    MetadataParse { path: PathBuf, detail: String },
}

fn default_fixtures_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is the crate root at compile time, which is
    // what we want for a stable, repo-relative fixtures path.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn read_dir(path: &Path) -> Result<Vec<fs::DirEntry>, LoadError> {
    let iter = fs::read_dir(path).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut entries = Vec::new();
    for e in iter {
        let entry = e.map_err(|source| LoadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        entries.push(entry);
    }
    // Deterministic order for reproducible test runs.
    entries.sort_by_key(|e| e.file_name());
    Ok(entries)
}

fn read_file(path: &Path) -> Result<Vec<u8>, LoadError> {
    fs::read(path).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })
}

// ---------------------------------------------------------------------------
// maven-metadata.xml parsing
// ---------------------------------------------------------------------------

/// A minimal parsed view of `maven-metadata.xml`. Only the fields the
/// resolver needs (versions list + timestamps).
#[derive(Debug, Clone, Default)]
struct ParsedMavenMetadata {
    versions: Vec<VersionString>,
    latest_snapshot_timestamp: Option<String>,
    last_updated: Option<String>,
}

/// Parse a `maven-metadata.xml` document. This is deliberately
/// minimal: it extracts `<versioning><versions><version>` entries plus
/// the `<lastUpdated>` and snapshot `<timestamp>` fields. Anything
/// else (plugins, snapshotVersions) is ignored — fixtures only need
/// the core resolver-facing data.
fn parse_maven_metadata(xml: &str) -> Result<ParsedMavenMetadata, String> {
    let mut out = ParsedMavenMetadata::default();
    // Cheap event walker: find each `<tag>...</tag>` pair we care
    // about. The fixture XML is hand-written and small, so a
    // full-blown parser would be overkill. We require well-formed
    // input: any structural surprise yields a clear error.
    let trimmed = xml.trim();
    if !trimmed.starts_with("<?xml") && !trimmed.starts_with("<metadata") {
        return Err("expected XML or <metadata> root".into());
    }

    out.versions = extract_versions(xml);
    out.last_updated = extract_first_text(xml, "lastUpdated");
    out.latest_snapshot_timestamp = extract_first_text(xml, "timestamp");

    Ok(out)
}

/// Extract every `<version>X</version>` occurring inside the first
/// `<versions>...</versions>` block. Order is preserved.
fn extract_versions(xml: &str) -> Vec<String> {
    let block = match find_block(xml, "versions") {
        Some(b) => b,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut cursor = 0usize;
    let open = "<version>";
    let close = "</version>";
    while let Some(rel_start) = block[cursor..].find(open) {
        let abs_start = cursor + rel_start + open.len();
        let rel_end = match block[abs_start..].find(close) {
            Some(e) => e,
            None => break,
        };
        let v = block[abs_start..abs_start + rel_end].trim();
        if !v.is_empty() {
            out.push(v.to_string());
        }
        cursor = abs_start + rel_end + close.len();
    }
    out
}

/// Find the inner text of the first `<TAG>...</TAG>` pair.
fn extract_first_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let rel_end = xml[start..].find(&close)?;
    let s = xml[start..start + rel_end].trim();
    if s.is_empty() { None } else { Some(s.to_string()) }
}

/// Return the body of the first `<TAG> ... </TAG>` block.
fn find_block<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let rel_end = xml[start..].find(&close)?;
    Some(&xml[start..start + rel_end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_metadata() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata>
  <groupId>org.example</groupId>
  <artifactId>lib</artifactId>
  <versioning>
    <latest>2.0</latest>
    <release>2.0</release>
    <versions>
      <version>1.0</version>
      <version>2.0</version>
    </versions>
    <lastUpdated>20260101000000</lastUpdated>
  </versioning>
</metadata>"#;
        let m = parse_maven_metadata(xml).unwrap();
        assert_eq!(m.versions, vec!["1.0", "2.0"]);
        assert_eq!(m.last_updated.as_deref(), Some("20260101000000"));
    }

    #[test]
    fn rejects_non_xml_input() {
        let err = parse_maven_metadata("not xml").unwrap_err();
        assert!(err.contains("expected"));
    }

    #[test]
    fn empty_versions_block_returns_empty_vec() {
        let xml = r#"<metadata><versioning><versions></versions></versioning></metadata>"#;
        let m = parse_maven_metadata(xml).unwrap();
        assert!(m.versions.is_empty());
    }
}
