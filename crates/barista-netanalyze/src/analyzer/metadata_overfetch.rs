//! `MetadataOverFetchAnalyzer` — repeated `maven-metadata.xml`
//! fetches on the same `(host, groupId, artifactId)` triple within
//! one session.
//!
//! This is the canonical Maven-specific waste: the resolver polls
//! `maven-metadata.xml` per-GAV when several GAVs share a GA, even
//! though one metadata fetch carries all the version info needed.
//!
//! PRD anchor: §18.3 O-REQ-01, pattern `PM-REDUNDANT-METADATA` from
//! §18.9. This analyzer is more specific than
//! [`DuplicateRequestAnalyzer`] in two ways:
//!   1. It groups by `(host, groupId, artifactId)` so requests with
//!      different version-subpaths still count together (Maven URLs
//!      have version after the artifact-id segment for some
//!      endpoints).
//!   2. It calls out the Maven-specific mitigation (O-REQ-01 — the
//!      per-session dedup) by name.

use std::collections::HashMap;

use crate::analyzer::Analyzer;
use crate::finding::{Category, EvidenceEntry, Finding, ImpactEstimate, Severity, Status};
use crate::har::Har;

/// Tunable thresholds for [`MetadataOverFetchAnalyzer`].
#[derive(Debug, Clone)]
pub struct MetadataOverFetchConfig {
    /// Minimum identical-fetch count required to emit a finding.
    /// Default `2`.
    pub min_repeat_count: usize,
}

impl Default for MetadataOverFetchConfig {
    fn default() -> Self {
        Self {
            min_repeat_count: 2,
        }
    }
}

/// Detects `maven-metadata.xml` over-fetching.
#[derive(Debug, Clone, Default)]
pub struct MetadataOverFetchAnalyzer {
    config: MetadataOverFetchConfig,
}

impl MetadataOverFetchAnalyzer {
    /// Construct with default thresholds.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self {
            config: MetadataOverFetchConfig::default(),
        }
    }

    /// Construct with custom thresholds.
    #[must_use]
    pub fn new(config: MetadataOverFetchConfig) -> Self {
        Self { config }
    }
}

impl Analyzer for MetadataOverFetchAnalyzer {
    fn id(&self) -> &'static str {
        "MetadataOverFetchAnalyzer"
    }

    fn analyze(&self, har: &Har) -> Vec<Finding> {
        let mut buckets: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (idx, entry) in har.log.entries.iter().enumerate() {
            let Some(host) = entry.host() else { continue };
            let Some(path) = entry.path_and_query() else {
                continue;
            };
            if !path.ends_with("/maven-metadata.xml") {
                continue;
            }
            let Some(ga) = group_artifact_key(&path) else {
                continue;
            };
            buckets.entry((host, ga)).or_default().push(idx);
        }

        let mut keys: Vec<_> = buckets.keys().cloned().collect();
        keys.sort();

        let mut findings = Vec::new();
        for key in keys {
            let occurrences = buckets.get(&key).cloned().unwrap_or_default();
            if occurrences.len() < self.config.min_repeat_count {
                continue;
            }
            let (host, ga) = key;
            let evidence: Vec<EvidenceEntry> = occurrences
                .iter()
                .map(|idx| {
                    let url = har
                        .log
                        .entries
                        .get(*idx)
                        .map(|e| e.request.url.clone())
                        .unwrap_or_default();
                    EvidenceEntry {
                        entry_index: *idx,
                        url,
                        note: "maven-metadata.xml fetch".to_string(),
                    }
                })
                .collect();
            let bytes_per_dup = occurrences
                .first()
                .and_then(|idx| har.log.entries.get(*idx))
                .map(|e| u64::try_from(e.response.body_size.max(0)).unwrap_or(0))
                .unwrap_or(0);
            let extra_count = occurrences.len().saturating_sub(1);
            let extra_u64 = u64::try_from(extra_count).unwrap_or(u64::MAX);
            findings.push(Finding {
                id: Finding::PENDING_ID.to_string(),
                title: format!(
                    "Redundant maven-metadata.xml fetches for `{ga}` on `{host}` ({n}×)",
                    n = occurrences.len()
                ),
                severity: severity_for(occurrences.len()),
                category: Category::RedundantMetadataFetch,
                status: Status::Open,
                evidence,
                impact: ImpactEstimate {
                    bytes_saved_per_build: bytes_per_dup.saturating_mul(extra_u64),
                    requests_saved_per_build: extra_u64,
                    connections_saved_per_build: 0,
                },
                proposal: format!(
                    "Apply session-scoped per-(repo, groupId, artifactId) dedup of \
                     metadata fetches. Emit at most one GET per `{ga}` against `{host}` \
                     per CLI invocation. See PRD §18.3 O-REQ-01."
                ),
                references: vec![
                    "PRD §18.3 — O-REQ-01".to_string(),
                    "PRD §18.9 — PM-REDUNDANT-METADATA".to_string(),
                ],
                discovered_by: self.id().to_string(),
            });
        }
        findings
    }
}

/// Given a Maven path like
/// `/maven2/org/example/widget/maven-metadata.xml`, returns
/// `Some("org/example/widget")`. Returns `None` if the path does not
/// look like a Maven layout (no `maven-metadata.xml` suffix or no
/// leading `/repo/.../`).
///
/// Public-ish for analyzer testability but not exported from the
/// crate.
fn group_artifact_key(path: &str) -> Option<String> {
    // Strip the trailing `/maven-metadata.xml`.
    let stem = path.strip_suffix("/maven-metadata.xml")?;
    // Strip a leading repo prefix (`/maven2/`, `/repository/`,
    // `/content/groups/public/`, etc.) by skipping the first
    // segment.
    let trimmed = stem.trim_start_matches('/');
    let (_repo, rest) = trimmed.split_once('/')?;
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

fn severity_for(count: usize) -> Severity {
    match count {
        0..=2 => Severity::Low,
        3..=5 => Severity::Medium,
        6..=14 => Severity::High,
        _ => Severity::Critical,
    }
}
