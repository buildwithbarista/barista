//! SNAPSHOT resolution + update policies.
//!
//! Maven snapshots are moving targets: the version
//! `1.0.0-SNAPSHOT` resolves to a uniquely-timestamped version like
//! `1.0.0-20240101.123456-7` via `maven-metadata.xml`. This module
//! handles:
//!
//! - Parsing the `<snapshot>` and `<snapshotVersions>` blocks of a
//!   `maven-metadata.xml`.
//! - Picking the latest snapshot publication for a `(extension,
//!   classifier)` pair.
//! - Update-policy helpers ([`should_refetch`](UpdatePolicyExt::should_refetch))
//!   that govern when the resolver re-fetches metadata vs. uses the
//!   cached copy.
//!
//! The [`UpdatePolicy`] type itself lives in `barista-config` so the
//! dependency edge stays one-way (resolver → config). This module
//! provides the parse/refetch helpers as an extension trait on the
//! re-exported enum.

use std::time::{Duration, SystemTime};

use quick_xml::escape::unescape;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde::{Deserialize, Serialize};

pub use barista_config::UpdatePolicy;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// The contents of a `<snapshot>` element from `maven-metadata.xml`.
///
/// This is the older (Maven 2-style) representation: a single
/// timestamp + buildNumber pair, with the timestamped version
/// reconstructed as `<base>-<timestamp>-<buildNumber>` where `<base>`
/// is the version stripped of `-SNAPSHOT`. Maven 3 publishes the
/// fully-built version strings in `<snapshotVersions>` instead, but
/// the `<snapshot>` block is still present as a fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    /// The fully-formed timestamped version, e.g.
    /// `"1.0.0-20240101.123456-7"`. Reconstructed from the surrounding
    /// `<version>` element + `<timestamp>` + `<buildNumber>` if a
    /// `<snapshotVersions>` block isn't available.
    pub timestamped_version: String,
    /// Maven's `<timestamp>` field, e.g. `"20240101.123456"`.
    pub timestamp: String,
    /// Maven's `<buildNumber>` field.
    pub build_number: u32,
    /// When the metadata file was last updated upstream (Maven's
    /// compact form, `"yyyymmddhhmmss"`).
    pub last_updated: Option<String>,
}

/// One entry in the `<snapshotVersions>` list — Maven 3's per-
/// `(extension, classifier)` mapping from a SNAPSHOT version to its
/// concrete timestamped publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotVersionEntry {
    /// `<extension>`, e.g. `"jar"`, `"pom"`, `"module"`.
    pub extension: String,
    /// `<classifier>`, e.g. `Some("sources")`. Absent for the default
    /// classifier.
    pub classifier: Option<String>,
    /// `<value>` — the timestamped version, e.g.
    /// `"1.0.0-20240101.123456-7"`.
    pub value: String,
    /// `<updated>` — Maven compact timestamp for when this entry was
    /// published.
    pub updated: String,
}

/// Parsed snapshot section of a `maven-metadata.xml` file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotInfo {
    /// The top-level `<snapshot>` block, if present.
    pub snapshot: Option<SnapshotMetadata>,
    /// Per-`(extension, classifier)` entries from `<snapshotVersions>`.
    /// Empty for older Maven 2-style metadata.
    pub snapshot_versions: Vec<SnapshotVersionEntry>,
}

impl SnapshotInfo {
    /// Pick the concrete timestamped version for a given `(extension,
    /// classifier)`.
    ///
    /// Returns the `<value>` from the first matching
    /// `<snapshotVersion>` entry. If no entry matches but a top-level
    /// `<snapshot>` block is present, falls back to that block's
    /// reconstructed `timestamped_version` — this matches the older
    /// Maven format where there is only one timestamped publish per
    /// SNAPSHOT version.
    pub fn pick_version(&self, extension: &str, classifier: Option<&str>) -> Option<&str> {
        for entry in &self.snapshot_versions {
            if entry.extension == extension && entry.classifier.as_deref() == classifier {
                return Some(&entry.value);
            }
        }
        self.snapshot
            .as_ref()
            .map(|s| s.timestamped_version.as_str())
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors raised by snapshot resolution and update-policy parsing.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The update-policy string was malformed.
    #[error("invalid update policy {raw:?}: {detail}")]
    InvalidUpdatePolicy { raw: String, detail: String },

    /// No snapshot version was available for the requested
    /// `(coords, version, extension)` triple.
    #[error("no snapshot version available for {coords}:{version} extension {extension:?}")]
    NoSnapshotVersion {
        coords: String,
        version: String,
        extension: String,
    },

    /// The `maven-metadata.xml` payload failed to parse.
    #[error("XML parse error in maven-metadata.xml for {coords}: {detail}")]
    MetadataParse { coords: String, detail: String },
}

// ---------------------------------------------------------------------------
// UpdatePolicy helpers
// ---------------------------------------------------------------------------

/// Resolver-side helpers on [`UpdatePolicy`]. The enum itself lives
/// in `barista-config`; this trait adds the parse + `should_refetch`
/// methods that only make sense inside the resolver / cache.
pub trait UpdatePolicyExt: Sized {
    /// Parse a policy string from `settings.xml` or the CLI.
    /// Accepts: `"always"`, `"daily"`, `"interval:N"` (with `N` a
    /// non-negative integer minute count), `"never"`. Case-insensitive.
    fn parse(raw: &str) -> Result<Self, SnapshotError>;

    /// Decide whether to re-fetch metadata, given when the local
    /// cached copy was last updated.
    ///
    /// Semantics:
    ///
    /// * [`UpdatePolicy::Always`] — always refetch.
    /// * [`UpdatePolicy::Never`] — never refetch (the `--update` CLI
    ///   flag is honored by the caller, not by this method).
    /// * [`UpdatePolicy::Daily`] — refetch if `now - last >= 24h`.
    /// * [`UpdatePolicy::Interval { minutes }`](UpdatePolicy::Interval) —
    ///   refetch if `now - last >= minutes`.
    ///
    /// `now < last_local_update` (clock skew) is treated as "no time
    /// elapsed" — i.e. don't refetch on the basis of a negative
    /// elapsed window.
    fn should_refetch(&self, last_local_update: SystemTime, now: SystemTime) -> bool;
}

impl UpdatePolicyExt for UpdatePolicy {
    fn parse(raw: &str) -> Result<Self, SnapshotError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(SnapshotError::InvalidUpdatePolicy {
                raw: raw.to_owned(),
                detail: "empty policy string".to_owned(),
            });
        }
        let lower = trimmed.to_ascii_lowercase();
        match lower.as_str() {
            "always" => Ok(UpdatePolicy::Always),
            "daily" => Ok(UpdatePolicy::Daily),
            "never" => Ok(UpdatePolicy::Never),
            other if other.starts_with("interval:") => {
                let rest = &other["interval:".len()..];
                if rest.is_empty() {
                    return Err(SnapshotError::InvalidUpdatePolicy {
                        raw: raw.to_owned(),
                        detail: "missing minutes after `interval:`".to_owned(),
                    });
                }
                let minutes: u32 = rest.parse().map_err(|e: std::num::ParseIntError| {
                    SnapshotError::InvalidUpdatePolicy {
                        raw: raw.to_owned(),
                        detail: format!("invalid minute count: {e}"),
                    }
                })?;
                Ok(UpdatePolicy::Interval { minutes })
            }
            _ => Err(SnapshotError::InvalidUpdatePolicy {
                raw: raw.to_owned(),
                detail: format!(
                    "expected one of `always`, `daily`, `interval:N`, `never`; got `{trimmed}`"
                ),
            }),
        }
    }

    fn should_refetch(&self, last_local_update: SystemTime, now: SystemTime) -> bool {
        match self {
            UpdatePolicy::Always => true,
            UpdatePolicy::Never => false,
            UpdatePolicy::Daily => {
                let elapsed = now
                    .duration_since(last_local_update)
                    .unwrap_or(Duration::ZERO);
                elapsed >= Duration::from_secs(24 * 3600)
            }
            UpdatePolicy::Interval { minutes } => {
                let elapsed = now
                    .duration_since(last_local_update)
                    .unwrap_or(Duration::ZERO);
                elapsed >= Duration::from_secs(u64::from(*minutes) * 60)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// XML parsing
// ---------------------------------------------------------------------------

/// Parse the snapshot section of a `maven-metadata.xml` payload.
///
/// The parser is deliberately lenient: it ignores elements it doesn't
/// recognise and never errors on unknown structure. It returns an
/// [`SnapshotError::MetadataParse`] only when the underlying XML is
/// malformed.
///
/// The expected shape is:
///
/// ```xml
/// <metadata>
///   <groupId>com.example</groupId>
///   <artifactId>foo</artifactId>
///   <version>1.0.0-SNAPSHOT</version>
///   <versioning>
///     <snapshot>
///       <timestamp>20240101.123456</timestamp>
///       <buildNumber>7</buildNumber>
///     </snapshot>
///     <lastUpdated>20240101123456</lastUpdated>
///     <snapshotVersions>
///       <snapshotVersion>
///         <extension>jar</extension>
///         <value>1.0.0-20240101.123456-7</value>
///         <updated>20240101123456</updated>
///       </snapshotVersion>
///     </snapshotVersions>
///   </versioning>
/// </metadata>
/// ```
pub fn parse_snapshot_metadata(xml: &str) -> Result<SnapshotInfo, SnapshotError> {
    let mut reader = Reader::from_str(xml);
    {
        let cfg = reader.config_mut();
        cfg.trim_text(true);
        cfg.check_end_names = true;
    }

    let mut buf = Vec::new();
    let mut path: Vec<String> = Vec::new();

    let mut info = SnapshotInfo::default();
    let mut current_text = String::new();
    let mut version: Option<String> = None;
    let mut snap_ts: Option<String> = None;
    let mut snap_build: Option<u32> = None;
    let mut last_updated: Option<String> = None;

    // Per-entry accumulators for <snapshotVersion>.
    let mut sv_extension: Option<String> = None;
    let mut sv_classifier: Option<String> = None;
    let mut sv_value: Option<String> = None;
    let mut sv_updated: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = std::str::from_utf8(e.name().as_ref())
                    .map_err(|err| SnapshotError::MetadataParse {
                        coords: String::new(),
                        detail: format!("non-UTF8 element name: {err}"),
                    })?
                    .to_owned();
                path.push(name);
                current_text.clear();
            }
            Ok(Event::End(_)) => {
                // Decide what to do with the just-closed element based
                // on its full path.
                let p: Vec<&str> = path.iter().map(String::as_str).collect();
                let text = current_text.trim().to_owned();
                match p.as_slice() {
                    ["metadata", "version"] => version = Some(text),
                    ["metadata", "versioning", "lastUpdated"] => last_updated = Some(text),
                    ["metadata", "versioning", "snapshot", "timestamp"] => snap_ts = Some(text),
                    ["metadata", "versioning", "snapshot", "buildNumber"] => {
                        snap_build = text.parse().ok();
                    }
                    [
                        "metadata",
                        "versioning",
                        "snapshotVersions",
                        "snapshotVersion",
                        inner,
                    ] => match *inner {
                        "extension" => sv_extension = Some(text),
                        "classifier" => {
                            if !text.is_empty() {
                                sv_classifier = Some(text);
                            }
                        }
                        "value" => sv_value = Some(text),
                        "updated" => sv_updated = Some(text),
                        _ => {}
                    },
                    [
                        "metadata",
                        "versioning",
                        "snapshotVersions",
                        "snapshotVersion",
                    ] => {
                        // End of one <snapshotVersion> entry.
                        if let (Some(ext), Some(val)) = (sv_extension.take(), sv_value.take()) {
                            info.snapshot_versions.push(SnapshotVersionEntry {
                                extension: ext,
                                classifier: sv_classifier.take(),
                                value: val,
                                updated: sv_updated.take().unwrap_or_default(),
                            });
                        }
                        sv_extension = None;
                        sv_classifier = None;
                        sv_value = None;
                        sv_updated = None;
                    }
                    _ => {}
                }
                current_text.clear();
                path.pop();
            }
            Ok(Event::Text(t)) => {
                let raw = t.decode().map_err(|err| SnapshotError::MetadataParse {
                    coords: String::new(),
                    detail: err.to_string(),
                })?;
                let s = unescape(&raw).map_err(|err| SnapshotError::MetadataParse {
                    coords: String::new(),
                    detail: err.to_string(),
                })?;
                current_text.push_str(&s);
            }
            Ok(Event::CData(t)) => {
                let s = std::str::from_utf8(t.as_ref()).map_err(|err| {
                    SnapshotError::MetadataParse {
                        coords: String::new(),
                        detail: err.to_string(),
                    }
                })?;
                current_text.push_str(s);
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(err) => {
                return Err(SnapshotError::MetadataParse {
                    coords: String::new(),
                    detail: err.to_string(),
                });
            }
        }
        buf.clear();
    }

    // Reconstruct the top-level <snapshot> if we saw a timestamp +
    // buildNumber. The timestamped version is `<base>-<ts>-<bn>` where
    // `<base>` is the `<version>` minus the `-SNAPSHOT` suffix.
    if let (Some(ts), Some(bn)) = (snap_ts.as_ref(), snap_build) {
        let base = version
            .as_deref()
            .map(strip_snapshot_suffix)
            .unwrap_or("")
            .to_owned();
        let timestamped_version = if base.is_empty() {
            format!("{ts}-{bn}")
        } else {
            format!("{base}-{ts}-{bn}")
        };
        info.snapshot = Some(SnapshotMetadata {
            timestamped_version,
            timestamp: ts.clone(),
            build_number: bn,
            last_updated: last_updated.clone(),
        });
    }

    Ok(info)
}

/// Strip a trailing `-SNAPSHOT` (case-insensitive) from `v`. Returns
/// the input unchanged if no such suffix is present.
fn strip_snapshot_suffix(v: &str) -> &str {
    const SUFFIX: &str = "-SNAPSHOT";
    if v.len() >= SUFFIX.len() {
        let tail = &v[v.len() - SUFFIX.len()..];
        if tail.eq_ignore_ascii_case(SUFFIX) {
            return &v[..v.len() - SUFFIX.len()];
        }
    }
    v
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    // ---- UpdatePolicy::parse --------------------------------------------

    #[test]
    fn parse_always() {
        assert_eq!(UpdatePolicy::parse("always").unwrap(), UpdatePolicy::Always);
    }

    #[test]
    fn parse_daily() {
        assert_eq!(UpdatePolicy::parse("daily").unwrap(), UpdatePolicy::Daily);
    }

    #[test]
    fn parse_never() {
        assert_eq!(UpdatePolicy::parse("never").unwrap(), UpdatePolicy::Never);
    }

    #[test]
    fn parse_interval_60() {
        assert_eq!(
            UpdatePolicy::parse("interval:60").unwrap(),
            UpdatePolicy::Interval { minutes: 60 }
        );
    }

    #[test]
    fn parse_case_insensitive() {
        assert_eq!(UpdatePolicy::parse("ALWAYS").unwrap(), UpdatePolicy::Always);
        assert_eq!(
            UpdatePolicy::parse("Interval:5").unwrap(),
            UpdatePolicy::Interval { minutes: 5 }
        );
    }

    #[test]
    fn parse_interval_missing_minutes() {
        let err = UpdatePolicy::parse("interval:").unwrap_err();
        assert!(matches!(err, SnapshotError::InvalidUpdatePolicy { .. }));
    }

    #[test]
    fn parse_interval_nonnumeric() {
        let err = UpdatePolicy::parse("interval:soon").unwrap_err();
        assert!(matches!(err, SnapshotError::InvalidUpdatePolicy { .. }));
    }

    #[test]
    fn parse_unknown() {
        let err = UpdatePolicy::parse("nonsense").unwrap_err();
        assert!(matches!(err, SnapshotError::InvalidUpdatePolicy { .. }));
    }

    #[test]
    fn parse_empty() {
        let err = UpdatePolicy::parse("").unwrap_err();
        assert!(matches!(err, SnapshotError::InvalidUpdatePolicy { .. }));
    }

    // ---- UpdatePolicy::should_refetch -----------------------------------

    #[test]
    fn always_refetches_immediately() {
        let now = t(1_000_000);
        assert!(UpdatePolicy::Always.should_refetch(now, now));
    }

    #[test]
    fn never_never_refetches() {
        let now = t(1_000_000);
        assert!(!UpdatePolicy::Never.should_refetch(t(0), now));
        assert!(!UpdatePolicy::Never.should_refetch(now, now));
    }

    #[test]
    fn daily_under_24h_no_refetch() {
        let now = t(24 * 3600);
        let last = t(2 * 3600); // 22h ago
        assert!(!UpdatePolicy::Daily.should_refetch(last, now));
    }

    #[test]
    fn daily_at_24h_refetches() {
        let now = t(24 * 3600);
        let last = t(0); // exactly 24h ago
        assert!(UpdatePolicy::Daily.should_refetch(last, now));
    }

    #[test]
    fn daily_over_24h_refetches() {
        let now = t(48 * 3600);
        let last = t(0); // 48h ago
        assert!(UpdatePolicy::Daily.should_refetch(last, now));
    }

    #[test]
    fn interval_under_window_no_refetch() {
        let now = t(60 * 60); // 1h
        let last = t(2 * 60); // 58 min ago, window=60min
        assert!(!UpdatePolicy::Interval { minutes: 60 }.should_refetch(last, now));
    }

    #[test]
    fn interval_over_window_refetches() {
        let now = t(70 * 60);
        let last = t(0); // 70 min ago, window=60min
        assert!(UpdatePolicy::Interval { minutes: 60 }.should_refetch(last, now));
    }

    #[test]
    fn clock_skew_does_not_force_refetch_for_daily() {
        // last > now (clock went backwards). Treat elapsed as zero.
        let last = t(1_000_000);
        let now = t(500_000);
        assert!(!UpdatePolicy::Daily.should_refetch(last, now));
    }

    // ---- XML parsing ----------------------------------------------------

    const FULL_METADATA: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata>
  <groupId>com.example</groupId>
  <artifactId>foo</artifactId>
  <version>1.0.0-SNAPSHOT</version>
  <versioning>
    <snapshot>
      <timestamp>20240101.123456</timestamp>
      <buildNumber>7</buildNumber>
    </snapshot>
    <lastUpdated>20240101123456</lastUpdated>
    <snapshotVersions>
      <snapshotVersion>
        <extension>jar</extension>
        <value>1.0.0-20240101.123456-7</value>
        <updated>20240101123456</updated>
      </snapshotVersion>
      <snapshotVersion>
        <classifier>sources</classifier>
        <extension>jar</extension>
        <value>1.0.0-20240101.123456-7</value>
        <updated>20240101123456</updated>
      </snapshotVersion>
      <snapshotVersion>
        <extension>pom</extension>
        <value>1.0.0-20240101.123456-7</value>
        <updated>20240101123456</updated>
      </snapshotVersion>
    </snapshotVersions>
  </versioning>
</metadata>
"#;

    #[test]
    fn parse_full_metadata() {
        let info = parse_snapshot_metadata(FULL_METADATA).expect("parse");
        let snap = info.snapshot.as_ref().expect("snapshot block");
        assert_eq!(snap.timestamp, "20240101.123456");
        assert_eq!(snap.build_number, 7);
        assert_eq!(snap.timestamped_version, "1.0.0-20240101.123456-7");
        assert_eq!(snap.last_updated.as_deref(), Some("20240101123456"));
        assert_eq!(info.snapshot_versions.len(), 3);
    }

    #[test]
    fn pick_version_default_classifier() {
        let info = parse_snapshot_metadata(FULL_METADATA).unwrap();
        assert_eq!(
            info.pick_version("jar", None),
            Some("1.0.0-20240101.123456-7")
        );
    }

    #[test]
    fn pick_version_sources_classifier() {
        let info = parse_snapshot_metadata(FULL_METADATA).unwrap();
        assert_eq!(
            info.pick_version("jar", Some("sources")),
            Some("1.0.0-20240101.123456-7")
        );
    }

    #[test]
    fn pick_version_pom_extension() {
        let info = parse_snapshot_metadata(FULL_METADATA).unwrap();
        assert_eq!(
            info.pick_version("pom", None),
            Some("1.0.0-20240101.123456-7")
        );
    }

    #[test]
    fn pick_version_unknown_classifier_falls_back_to_top_level() {
        let info = parse_snapshot_metadata(FULL_METADATA).unwrap();
        // No `(jar, javadoc)` entry — falls back to the top-level
        // <snapshot> timestamped_version.
        assert_eq!(
            info.pick_version("jar", Some("javadoc")),
            Some("1.0.0-20240101.123456-7")
        );
    }

    #[test]
    fn malformed_xml_errors() {
        // Mismatched end tag — quick-xml's default config rejects this.
        let bad = "<metadata><versioning></bogus></metadata>";
        let err = parse_snapshot_metadata(bad).unwrap_err();
        assert!(
            matches!(err, SnapshotError::MetadataParse { .. }),
            "expected MetadataParse, got {err:?}"
        );
    }

    #[test]
    fn maven2_style_no_snapshot_versions() {
        // Older Maven format — only the <snapshot> block, no
        // <snapshotVersions>.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata>
  <groupId>org.legacy</groupId>
  <artifactId>old</artifactId>
  <version>2.5-SNAPSHOT</version>
  <versioning>
    <snapshot>
      <timestamp>20100101.000000</timestamp>
      <buildNumber>3</buildNumber>
    </snapshot>
    <lastUpdated>20100101000000</lastUpdated>
  </versioning>
</metadata>
"#;
        let info = parse_snapshot_metadata(xml).unwrap();
        assert!(info.snapshot_versions.is_empty());
        let snap = info.snapshot.as_ref().unwrap();
        assert_eq!(snap.timestamped_version, "2.5-20100101.000000-3");
        // pick_version with anything falls back to top-level.
        assert_eq!(
            info.pick_version("jar", None),
            Some("2.5-20100101.000000-3")
        );
    }

    #[test]
    fn empty_metadata_yields_empty_info() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata>
  <groupId>g</groupId>
  <artifactId>a</artifactId>
  <versioning/>
</metadata>"#;
        let info = parse_snapshot_metadata(xml).unwrap();
        assert!(info.snapshot.is_none());
        assert!(info.snapshot_versions.is_empty());
        assert_eq!(info.pick_version("jar", None), None);
    }

    // ---- strip_snapshot_suffix ------------------------------------------

    #[test]
    fn strip_snapshot_suffix_uppercase() {
        assert_eq!(strip_snapshot_suffix("1.0.0-SNAPSHOT"), "1.0.0");
    }

    #[test]
    fn strip_snapshot_suffix_lowercase() {
        assert_eq!(strip_snapshot_suffix("1.0.0-snapshot"), "1.0.0");
    }

    #[test]
    fn strip_snapshot_suffix_no_suffix() {
        assert_eq!(strip_snapshot_suffix("1.0.0"), "1.0.0");
    }

    #[test]
    fn strip_snapshot_suffix_short_input() {
        assert_eq!(strip_snapshot_suffix(""), "");
        assert_eq!(strip_snapshot_suffix("S"), "S");
    }
}
