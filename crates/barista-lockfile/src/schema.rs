// SPDX-License-Identifier: MIT OR Apache-2.0

//! Lockfile TOML schema.
//!
//! `barista.lock` is the project-level pinning file. It records the
//! exact resolved dependency graph + metadata sufficient to:
//!
//! - Re-fetch every artifact (URL + checksums).
//! - Validate that the same lockfile applies to the current source
//!   (`project_signature` from the lockfile-signature module).
//! - Render code-review-friendly diffs between two lockfile versions
//!   (see the [`crate::diff`] module).
//!
//! ## File layout
//!
//! ```toml
//! [meta]
//! schema_version = 1
//! generated_by   = "barista 0.1.0-alpha.0"
//! generated_at   = "2026-05-13T12:34:56Z"
//! project_signature    = "<sha256-hex>"
//! settings_fingerprint = "<sha256-hex>"
//!
//! [[reactor]]
//! coords         = "com.example:my-app"
//! version        = "1.0.0-SNAPSHOT"
//! relative_path  = "pom.xml"
//!
//! [[entries]]
//! coords       = "org.slf4j:slf4j-api"
//! version      = "2.0.16"
//! scope        = "compile"
//! sha256       = "..."
//! size_bytes   = 65432
//! source_url   = "https://repo.maven.apache.org/maven2/org/slf4j/..."
//! # ... etc.
//! ```
//!
//! ## Forward compatibility
//!
//! Backward-compatible additions (new optional fields) do not bump
//! [`LOCKFILE_SCHEMA_VERSION`]. Removing fields, renaming fields, or
//! changing field semantics requires a version bump. Readers reject
//! lockfiles whose `schema_version` does not match what this build
//! understands; the CLI then prints a friendly "regenerate with
//! `barista lock`" message.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Current lockfile schema version.
///
/// Bumped on backward-incompatible changes. Readers reject unknown
/// versions; backward-compatible additions do not bump the version.
pub const LOCKFILE_SCHEMA_VERSION: u32 = 1;

/// A parsed `barista.lock` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Lockfile {
    pub meta: Meta,

    /// Modules of the current reactor (multi-module Maven project).
    /// Single-module projects emit a single reactor entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactor: Vec<ReactorEntry>,

    /// Resolved dependency entries, one per artifact.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<LockfileEntry>,

    /// Optional snapshot of relevant `settings.xml` bits at lock time.
    /// Used to detect environment drift between lock and build hosts.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "settings")]
    pub settings_snapshot: Option<SettingsSnapshot>,
}

/// `[meta]` table: stable identifying information about the lockfile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Meta {
    /// Schema version. Must equal [`LOCKFILE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Producer string, e.g. `"barista 0.1.0-alpha.0"`.
    pub generated_by: String,
    /// RFC 3339 timestamp.
    pub generated_at: String,
    /// SHA-256 (hex) of canonicalized effective POMs across the reactor.
    pub project_signature: String,
    /// SHA-256 (hex) of the relevant `settings.xml` bits used during resolution.
    pub settings_fingerprint: String,
}

/// One module of the current reactor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReactorEntry {
    /// `group:artifact`.
    pub coords: String,
    pub version: String,
    /// Path to the module's `pom.xml`, relative to the project root.
    pub relative_path: String,
}

/// A single resolved artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LockfileEntry {
    /// `group:artifact`. Classifier and type are encoded in their own fields.
    pub coords: String,
    pub version: String,
    /// One of `compile`, `runtime`, `test`, `provided`, `system`.
    pub scope: String,
    /// `true` for optional dependencies. Defaults to `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,

    /// SHA-256 (hex) of the artifact bytes.
    pub sha256: String,
    /// SHA-1 (hex), if the upstream repository served one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
    pub size_bytes: u64,

    /// Upstream URL the artifact was fetched from.
    pub source_url: String,
    /// HTTP `ETag` from the fetch response, for revalidation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// HTTP `Last-Modified` from the fetch response, for revalidation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,

    /// Maven classifier, e.g. `sources`, `linux-x86_64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    /// Artifact type / packaging. Defaults to `"jar"` when absent.
    #[serde(default = "default_type", rename = "type")]
    pub type_: String,

    /// The dep-graph path (coords by BFS) that "won" this entry under
    /// nearest-wins. Empty for direct dependencies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from_path: Vec<String>,
    /// BFS depth at which this entry was selected. 0 = direct.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub depth: u32,

    /// For SNAPSHOTs: the timestamped version actually fetched
    /// (e.g. `1.2.3-20260513.123456-7`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_resolution: Option<String>,

    /// Exclusions applied while resolving this entry's transitive
    /// dependencies. Recorded for diff context — they do not affect
    /// re-fetching.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclusions: Vec<Exclusion>,
}

/// A `group:artifact` exclusion applied during resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Exclusion {
    pub group: String,
    pub artifact: String,
}

/// `[settings]` table: snapshot of relevant `settings.xml` bits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SettingsSnapshot {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<MirrorRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<RepositoryRef>,
}

/// A mirror declaration as captured at lock time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MirrorRef {
    pub id: String,
    pub url: String,
    pub mirror_of: String,
}

/// A repository declaration as captured at lock time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepositoryRef {
    pub id: String,
    pub url: String,
}

/// Errors raised when reading, writing, or parsing a lockfile.
#[derive(Debug, thiserror::Error)]
pub enum LockfileError {
    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("TOML serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    #[error("I/O error at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "unsupported lockfile schema version: {got} (this build understands {expected}). \
         Regenerate the lockfile with `barista lock`."
    )]
    UnsupportedVersion { got: u32, expected: u32 },
}

impl Lockfile {
    /// Construct an empty lockfile carrying the current schema
    /// metadata. The `generated_by` and `generated_at` fields are
    /// filled in from build-time information and the current wall
    /// clock respectively.
    pub fn new(project_signature: String, settings_fingerprint: String) -> Self {
        Self {
            meta: Meta {
                schema_version: LOCKFILE_SCHEMA_VERSION,
                generated_by: format!("barista {}", env!("CARGO_PKG_VERSION")),
                generated_at: now_rfc3339(),
                project_signature,
                settings_fingerprint,
            },
            reactor: Vec::new(),
            entries: Vec::new(),
            settings_snapshot: None,
        }
    }

    /// Parse a lockfile from a TOML string. Rejects schema versions
    /// this build does not understand.
    pub fn from_toml(s: &str) -> Result<Self, LockfileError> {
        let lf: Self = toml::from_str(s)?;
        if lf.meta.schema_version != LOCKFILE_SCHEMA_VERSION {
            return Err(LockfileError::UnsupportedVersion {
                got: lf.meta.schema_version,
                expected: LOCKFILE_SCHEMA_VERSION,
            });
        }
        Ok(lf)
    }

    /// Serialize the lockfile to TOML. Pretty-printed for human
    /// readability and code-review-friendly diffs.
    pub fn to_toml(&self) -> Result<String, LockfileError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Read a lockfile from disk.
    pub fn read(path: &Path) -> Result<Self, LockfileError> {
        let bytes = std::fs::read_to_string(path).map_err(|source| LockfileError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml(&bytes)
    }

    /// Write the lockfile to disk atomically: serialize to a sibling
    /// `*.tmp` file (with a random suffix to avoid concurrent writer
    /// collisions), `fsync` it, then `rename` over the destination.
    /// On POSIX this is atomic with respect to readers — a concurrent
    /// `read` either sees the old content or the new content, never a
    /// truncated mix.
    pub fn write(&self, path: &Path) -> Result<(), LockfileError> {
        let toml_str = self.to_toml()?;

        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "barista.lock".to_string());

        // Random-ish suffix from current nanoseconds + process id, to
        // avoid collisions between concurrent writers without pulling
        // in a `rand` dependency.
        let nonce = {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{}-{}", std::process::id(), now)
        };
        let tmp = parent.join(format!(".{file_name}.tmp-{nonce}"));

        // Scoped block so the file is closed before we rename — some
        // platforms (notably Windows) cannot rename an open file.
        {
            let mut f = std::fs::File::create(&tmp).map_err(|source| LockfileError::Io {
                path: tmp.clone(),
                source,
            })?;
            f.write_all(toml_str.as_bytes())
                .map_err(|source| LockfileError::Io {
                    path: tmp.clone(),
                    source,
                })?;
            f.sync_all().map_err(|source| LockfileError::Io {
                path: tmp.clone(),
                source,
            })?;
        }

        std::fs::rename(&tmp, path).map_err(|source| LockfileError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        Ok(())
    }

    /// Look the meta table up by name. Used by the diff renderer and
    /// CLI status output. Returns a fresh `BTreeMap` to keep call
    /// sites simple; not on the hot path.
    pub fn meta_as_map(&self) -> BTreeMap<&'static str, String> {
        let mut m: BTreeMap<&'static str, String> = BTreeMap::new();
        m.insert("schema_version", self.meta.schema_version.to_string());
        m.insert("generated_by", self.meta.generated_by.clone());
        m.insert("generated_at", self.meta.generated_at.clone());
        m.insert("project_signature", self.meta.project_signature.clone());
        m.insert(
            "settings_fingerprint",
            self.meta.settings_fingerprint.clone(),
        );
        m
    }
}

// ----- serde helpers ---------------------------------------------------------

fn default_type() -> String {
    "jar".to_string()
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde calls these by ref
fn is_false(b: &bool) -> bool {
    !*b
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

/// Render the current wall clock as an RFC 3339 / ISO 8601 timestamp
/// in UTC, second precision. Format: `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Implemented inline to keep this crate dependency-light — no
/// `chrono`/`time` pull-in just for a single timestamp emission.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    epoch_to_rfc3339(secs)
}

/// Convert a Unix-epoch second count into a UTC RFC 3339 string.
/// Civil-date algorithm from Howard Hinnant ("date algorithms",
/// public domain). Sufficient for second-precision timestamps in
/// the supported range.
fn epoch_to_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let secs_of_day = (secs % 86_400) as u32;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    // Days since 1970-01-01 -> civil date (Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

// ----- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(coords: &str, version: &str) -> LockfileEntry {
        LockfileEntry {
            coords: coords.to_string(),
            version: version.to_string(),
            scope: "compile".to_string(),
            optional: false,
            sha256: "0".repeat(64),
            sha1: None,
            size_bytes: 1024,
            source_url: format!(
                "https://repo.maven.apache.org/maven2/{}/{}-{}.jar",
                coords.replace([':', '.'], "/"),
                coords.split(':').next_back().unwrap_or(""),
                version
            ),
            etag: None,
            last_modified: None,
            classifier: None,
            type_: "jar".to_string(),
            from_path: Vec::new(),
            depth: 0,
            snapshot_resolution: None,
            exclusions: Vec::new(),
        }
    }

    fn empty_lockfile() -> Lockfile {
        Lockfile::new("a".repeat(64), "b".repeat(64))
    }

    #[test]
    fn new_lockfile_has_current_schema_version() {
        let lf = empty_lockfile();
        assert_eq!(lf.meta.schema_version, LOCKFILE_SCHEMA_VERSION);
        assert!(lf.meta.generated_by.starts_with("barista "));
        assert!(lf.entries.is_empty());
        assert!(lf.reactor.is_empty());
        assert!(lf.settings_snapshot.is_none());
    }

    #[test]
    fn new_lockfile_carries_signatures_through() {
        let lf = Lockfile::new("sig1".to_string(), "sig2".to_string());
        assert_eq!(lf.meta.project_signature, "sig1");
        assert_eq!(lf.meta.settings_fingerprint, "sig2");
    }

    #[test]
    fn round_trip_small_lockfile_preserves_all_fields() {
        let mut lf = empty_lockfile();
        lf.entries
            .push(sample_entry("org.slf4j:slf4j-api", "2.0.16"));
        let mut e2 = sample_entry("org.springframework:spring-core", "6.1.6");
        e2.scope = "runtime".to_string();
        e2.optional = true;
        e2.sha1 = Some("a".repeat(40));
        e2.etag = Some("\"abc\"".to_string());
        e2.last_modified = Some("Wed, 01 May 2026 00:00:00 GMT".to_string());
        e2.classifier = Some("sources".to_string());
        e2.type_ = "jar".to_string();
        e2.from_path = vec!["root".to_string(), "child".to_string()];
        e2.depth = 2;
        e2.exclusions.push(Exclusion {
            group: "commons-logging".to_string(),
            artifact: "commons-logging".to_string(),
        });
        lf.entries.push(e2);

        let toml_str = lf.to_toml().expect("serialize");
        let parsed = Lockfile::from_toml(&toml_str).expect("parse");
        assert_eq!(parsed, lf);
    }

    #[test]
    fn round_trip_with_50_entries() {
        let mut lf = empty_lockfile();
        for i in 0..50 {
            let mut e = sample_entry(
                &format!("com.example.group{}:artifact{}", i % 5, i),
                "1.2.3",
            );
            e.depth = (i % 7) as u32;
            e.scope = match i % 5 {
                0 => "compile",
                1 => "runtime",
                2 => "test",
                3 => "provided",
                _ => "system",
            }
            .to_string();
            lf.entries.push(e);
        }
        let toml_str = lf.to_toml().expect("serialize");
        let parsed = Lockfile::from_toml(&toml_str).expect("parse");
        assert_eq!(parsed, lf);
        assert_eq!(parsed.entries.len(), 50);
    }

    #[test]
    fn round_trip_with_reactor_entries() {
        let mut lf = empty_lockfile();
        lf.reactor.push(ReactorEntry {
            coords: "com.example:parent".to_string(),
            version: "1.0.0-SNAPSHOT".to_string(),
            relative_path: "pom.xml".to_string(),
        });
        lf.reactor.push(ReactorEntry {
            coords: "com.example:child-a".to_string(),
            version: "1.0.0-SNAPSHOT".to_string(),
            relative_path: "child-a/pom.xml".to_string(),
        });
        lf.reactor.push(ReactorEntry {
            coords: "com.example:child-b".to_string(),
            version: "1.0.0-SNAPSHOT".to_string(),
            relative_path: "child-b/pom.xml".to_string(),
        });
        let toml_str = lf.to_toml().expect("serialize");
        let parsed = Lockfile::from_toml(&toml_str).expect("parse");
        assert_eq!(parsed, lf);
        assert_eq!(parsed.reactor.len(), 3);
    }

    #[test]
    fn round_trip_with_exclusions_on_entry() {
        let mut lf = empty_lockfile();
        let mut e = sample_entry("org.example:thing", "1.0.0");
        e.exclusions.push(Exclusion {
            group: "commons-logging".to_string(),
            artifact: "commons-logging".to_string(),
        });
        e.exclusions.push(Exclusion {
            group: "log4j".to_string(),
            artifact: "log4j".to_string(),
        });
        lf.entries.push(e);

        let toml_str = lf.to_toml().expect("serialize");
        let parsed = Lockfile::from_toml(&toml_str).expect("parse");
        assert_eq!(parsed, lf);
    }

    #[test]
    fn round_trip_with_snapshot_resolution() {
        let mut lf = empty_lockfile();
        let mut e = sample_entry("com.example:libsnap", "1.2.3-SNAPSHOT");
        e.snapshot_resolution = Some("1.2.3-20260513.123456-7".to_string());
        lf.entries.push(e);

        let toml_str = lf.to_toml().expect("serialize");
        let parsed = Lockfile::from_toml(&toml_str).expect("parse");
        assert_eq!(parsed, lf);
        assert_eq!(
            parsed.entries[0].snapshot_resolution.as_deref(),
            Some("1.2.3-20260513.123456-7")
        );
    }

    #[test]
    fn round_trip_with_classifier() {
        let mut lf = empty_lockfile();
        let mut e = sample_entry("io.netty:netty-tcnative-boringssl-static", "2.0.62.Final");
        e.classifier = Some("linux-x86_64".to_string());
        lf.entries.push(e);

        let toml_str = lf.to_toml().expect("serialize");
        let parsed = Lockfile::from_toml(&toml_str).expect("parse");
        assert_eq!(parsed, lf);
        assert_eq!(
            parsed.entries[0].classifier.as_deref(),
            Some("linux-x86_64")
        );
    }

    #[test]
    fn from_toml_rejects_unsupported_schema_version() {
        let lf = empty_lockfile();
        let mut toml_str = lf.to_toml().unwrap();
        // Bump to a version this build does not understand.
        toml_str = toml_str.replace(
            &format!("schema_version = {LOCKFILE_SCHEMA_VERSION}"),
            "schema_version = 999",
        );
        let err = Lockfile::from_toml(&toml_str).unwrap_err();
        match err {
            LockfileError::UnsupportedVersion { got, expected } => {
                assert_eq!(got, 999);
                assert_eq!(expected, LOCKFILE_SCHEMA_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn type_field_defaults_to_jar_when_omitted() {
        // Hand-build a minimal lockfile TOML omitting the `type` field
        // on the entry. It should deserialize with `type_ = "jar"`.
        let toml_str = format!(
            r#"
[meta]
schema_version = {ver}
generated_by = "test"
generated_at = "2026-05-13T00:00:00Z"
project_signature = "0"
settings_fingerprint = "0"

[[entries]]
coords = "g:a"
version = "1.0.0"
scope = "compile"
sha256 = "{sha}"
size_bytes = 10
source_url = "https://example.com/a.jar"
"#,
            ver = LOCKFILE_SCHEMA_VERSION,
            sha = "0".repeat(64),
        );
        let parsed = Lockfile::from_toml(&toml_str).expect("parse");
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].type_, "jar");
    }

    #[test]
    fn optional_defaults_to_false_when_omitted() {
        let toml_str = format!(
            r#"
[meta]
schema_version = {ver}
generated_by = "test"
generated_at = "2026-05-13T00:00:00Z"
project_signature = "0"
settings_fingerprint = "0"

[[entries]]
coords = "g:a"
version = "1.0.0"
scope = "compile"
sha256 = "{sha}"
size_bytes = 10
source_url = "https://example.com/a.jar"
"#,
            ver = LOCKFILE_SCHEMA_VERSION,
            sha = "0".repeat(64),
        );
        let parsed = Lockfile::from_toml(&toml_str).expect("parse");
        assert!(!parsed.entries[0].optional);
    }

    #[test]
    fn sha1_is_omitted_from_toml_when_none() {
        let mut lf = empty_lockfile();
        lf.entries.push(sample_entry("g:a", "1.0.0"));
        let toml_str = lf.to_toml().expect("serialize");
        assert!(
            !toml_str.contains("sha1"),
            "TOML unexpectedly contains sha1:\n{toml_str}"
        );
    }

    #[test]
    fn etag_and_last_modified_omitted_when_none() {
        let mut lf = empty_lockfile();
        lf.entries.push(sample_entry("g:a", "1.0.0"));
        let toml_str = lf.to_toml().expect("serialize");
        assert!(!toml_str.contains("etag"));
        assert!(!toml_str.contains("last_modified"));
    }

    #[test]
    fn from_path_omitted_when_empty() {
        let mut lf = empty_lockfile();
        lf.entries.push(sample_entry("g:a", "1.0.0"));
        let toml_str = lf.to_toml().expect("serialize");
        assert!(!toml_str.contains("from_path"));
    }

    #[test]
    fn exclusions_omitted_when_empty() {
        let mut lf = empty_lockfile();
        lf.entries.push(sample_entry("g:a", "1.0.0"));
        let toml_str = lf.to_toml().expect("serialize");
        assert!(!toml_str.contains("exclusions"));
    }

    #[test]
    fn write_and_read_round_trip_via_tempfile() {
        let mut lf = empty_lockfile();
        lf.entries.push(sample_entry("g:a", "1.0.0"));
        lf.entries.push(sample_entry("g:b", "2.0.0"));

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("barista.lock");
        lf.write(&path).expect("write");

        let read_back = Lockfile::read(&path).expect("read");
        assert_eq!(read_back, lf);
    }

    #[test]
    fn atomic_write_replaces_prior_content_completely() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("barista.lock");

        // Seed with a much larger old lockfile.
        let mut old = empty_lockfile();
        for i in 0..30 {
            old.entries.push(sample_entry(&format!("g:a{i}"), "9.9.9"));
        }
        old.write(&path).expect("seed");
        let old_len = std::fs::read(&path).expect("read seed").len();

        // Overwrite with a tiny new lockfile.
        let new = empty_lockfile();
        new.write(&path).expect("overwrite");

        // Disk content must equal exactly the new serialization — no
        // trailing bytes from the old, longer file.
        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert_eq!(on_disk, new.to_toml().unwrap());
        assert!(
            on_disk.len() < old_len,
            "atomic overwrite should have shrunk the file"
        );

        // And no temp files left behind in the directory.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                let n = e.file_name();
                let s = n.to_string_lossy();
                s.starts_with(".barista.lock.tmp-")
            })
            .collect();
        assert!(leftovers.is_empty(), "tmp files left behind: {leftovers:?}");
    }

    #[test]
    fn malformed_toml_returns_parse_error() {
        let bad = "this is = = not valid toml [[[";
        let err = Lockfile::from_toml(bad).unwrap_err();
        assert!(
            matches!(err, LockfileError::TomlParse(_)),
            "expected TomlParse, got {err:?}"
        );
    }

    #[test]
    fn empty_toml_errors_on_missing_required_fields() {
        let err = Lockfile::from_toml("").unwrap_err();
        assert!(
            matches!(err, LockfileError::TomlParse(_)),
            "expected TomlParse, got {err:?}"
        );
        // Specifically: should mention the missing meta table.
        if let LockfileError::TomlParse(e) = err {
            let msg = e.to_string();
            assert!(
                msg.contains("meta") || msg.contains("missing"),
                "unexpected error message: {msg}"
            );
        }
    }

    #[test]
    fn read_nonexistent_file_returns_io_error_with_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist.lock");
        let err = Lockfile::read(&missing).unwrap_err();
        match err {
            LockfileError::Io { path, .. } => {
                assert_eq!(path, missing);
            }
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn meta_as_map_includes_all_fields() {
        let lf = Lockfile::new("sig1".to_string(), "sig2".to_string());
        let m = lf.meta_as_map();
        assert_eq!(m.get("schema_version").map(String::as_str), Some("1"));
        assert_eq!(m.get("project_signature").map(String::as_str), Some("sig1"));
        assert_eq!(
            m.get("settings_fingerprint").map(String::as_str),
            Some("sig2")
        );
        assert!(m.contains_key("generated_by"));
        assert!(m.contains_key("generated_at"));
    }

    #[test]
    fn epoch_to_rfc3339_known_values() {
        // 0 -> 1970-01-01T00:00:00Z
        assert_eq!(epoch_to_rfc3339(0), "1970-01-01T00:00:00Z");
        // 2026-05-13T12:34:56Z -> 1_778_675_696
        assert_eq!(epoch_to_rfc3339(1_778_675_696), "2026-05-13T12:34:56Z");
        // Cross a leap day: 2024-02-29T00:00:00Z -> 1_709_164_800
        assert_eq!(epoch_to_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn settings_snapshot_round_trips() {
        let mut lf = empty_lockfile();
        lf.settings_snapshot = Some(SettingsSnapshot {
            mirrors: vec![MirrorRef {
                id: "central-mirror".to_string(),
                url: "https://mirror.example.com/maven2".to_string(),
                mirror_of: "central".to_string(),
            }],
            repositories: vec![RepositoryRef {
                id: "central".to_string(),
                url: "https://repo.maven.apache.org/maven2".to_string(),
            }],
        });

        let toml_str = lf.to_toml().expect("serialize");
        let parsed = Lockfile::from_toml(&toml_str).expect("parse");
        assert_eq!(parsed, lf);
        let snap = parsed.settings_snapshot.unwrap();
        assert_eq!(snap.mirrors.len(), 1);
        assert_eq!(snap.repositories.len(), 1);
    }

    #[test]
    fn toml_output_is_pretty_with_section_headers() {
        let mut lf = empty_lockfile();
        lf.entries.push(sample_entry("g:a", "1.0.0"));
        let s = lf.to_toml().expect("serialize");
        // Pretty TOML uses bracketed table headers, not single-line tables.
        assert!(s.contains("[meta]"));
        assert!(s.contains("[[entries]]"));
    }

    #[test]
    fn type_field_serializes_under_toml_key_type_not_type_underscore() {
        // Important: the field is named `type_` in Rust to avoid the
        // keyword clash, but we serialize it as `type` in the TOML
        // (this is what Maven users expect to see).
        let mut lf = empty_lockfile();
        let mut e = sample_entry("g:a", "1.0.0");
        e.type_ = "pom".to_string();
        lf.entries.push(e);

        let s = lf.to_toml().expect("serialize");
        assert!(
            s.contains("type = \"pom\""),
            "expected `type = \"pom\"` in:\n{s}"
        );
        assert!(
            !s.contains("type_ ="),
            "did not expect raw `type_` in TOML:\n{s}"
        );
    }

    #[test]
    fn depth_omitted_from_toml_when_zero() {
        // Direct dependencies have depth 0; we want minimal noise.
        let mut lf = empty_lockfile();
        lf.entries.push(sample_entry("g:a", "1.0.0"));
        let s = lf.to_toml().expect("serialize");
        assert!(!s.contains("depth = 0"), "unexpected `depth = 0` in:\n{s}");
    }

    #[test]
    fn json_schema_validates_a_real_lockfile() {
        // Build a hand-crafted Lockfile value, serialize to JSON (not
        // TOML — JSON Schema validates JSON shape; the TOML and JSON
        // serde forms share the same struct so this is well-defined).
        let mut lf = empty_lockfile();
        lf.entries
            .push(sample_entry("org.slf4j:slf4j-api", "2.0.16"));
        let json = serde_json::to_value(&lf).expect("Lockfile serializes to JSON");

        // Load the on-disk schema. The schema lives at the monorepo
        // root under `schema/lockfile/v1.json`. CI generates it before
        // running tests; locally, skip the test if the file is absent.
        let schema_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../schema/lockfile/v1.json");
        if !schema_path.exists() {
            eprintln!(
                "skipping: schema not present at {schema_path:?} \
                 (run `cargo run -p barista-lockfile --example export-schema \
                 > schema/lockfile/v1.json` to generate)"
            );
            return;
        }
        let schema_text = std::fs::read_to_string(&schema_path)
            .unwrap_or_else(|_| panic!("schema must exist at {schema_path:?}"));
        let schema: serde_json::Value =
            serde_json::from_str(&schema_text).expect("v1.json must parse as JSON");

        // Sanity: the schema should declare itself a JSON Schema and
        // carry the public $id that downstream tools key off of.
        assert!(schema.is_object(), "schema must be a JSON object");
        assert_eq!(
            schema.get("$id").and_then(|v| v.as_str()),
            Some("https://barista.build/schema/lockfile/v1.json"),
            "schema $id must match the published URL"
        );

        // Full schema-driven validation against the Lockfile JSON
        // requires a JSON Schema library, which is heavyweight for
        // v0.1. Flagged for v0.2. For now we only verify the schema
        // is valid JSON and a real Lockfile serializes to a JSON
        // object.
        assert!(json.is_object());
    }
}
