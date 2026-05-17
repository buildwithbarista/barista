//! `DuplicateRequestAnalyzer` — same URL fetched 2+ times within a
//! single session.
//!
//! PRD anchor: §18.3 (O-REQ-01, O-REQ-04, O-REQ-05). Maven's
//! resolver re-issues GETs for the same artifact in surprising
//! places (reactor children, plugin resolution, parent-POM walks).
//! Each duplicate is wasted upstream load and wall time.

use std::collections::HashMap;

use crate::analyzer::Analyzer;
use crate::finding::{
    Category, EvidenceEntry, Finding, ImpactEstimate, Severity, Status,
};
use crate::har::Har;

/// Tunable thresholds for [`DuplicateRequestAnalyzer`].
#[derive(Debug, Clone)]
pub struct DuplicateRequestConfig {
    /// Minimum identical-fetch count required to emit a finding.
    /// Default `2` — i.e. one extra fetch beyond the first is the
    /// smallest waste worth flagging.
    pub min_repeat_count: usize,
}

impl Default for DuplicateRequestConfig {
    fn default() -> Self {
        Self {
            min_repeat_count: 2,
        }
    }
}

/// Detects URL-level duplicates within one session.
#[derive(Debug, Clone, Default)]
pub struct DuplicateRequestAnalyzer {
    config: DuplicateRequestConfig,
}

impl DuplicateRequestAnalyzer {
    /// Construct with default thresholds.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self {
            config: DuplicateRequestConfig::default(),
        }
    }

    /// Construct with custom thresholds.
    #[must_use]
    pub fn new(config: DuplicateRequestConfig) -> Self {
        Self { config }
    }
}

impl Analyzer for DuplicateRequestAnalyzer {
    fn id(&self) -> &'static str {
        "DuplicateRequestAnalyzer"
    }

    fn analyze(&self, har: &Har) -> Vec<Finding> {
        // Key: (method, full URL). HAR records the absolute URL on
        // every entry, so a string key uniquely identifies "the same
        // request" for the purposes of dedup-finding.
        let mut buckets: HashMap<(String, String), Vec<(usize, &str)>> = HashMap::new();
        for (idx, entry) in har.log.entries.iter().enumerate() {
            // Only GET/HEAD are candidates — POSTs may legitimately
            // be retried with the same URL but different bodies.
            let method = entry.request.method.to_ascii_uppercase();
            if method != "GET" && method != "HEAD" {
                continue;
            }
            buckets
                .entry((method, entry.request.url.clone()))
                .or_default()
                .push((idx, entry.request.url.as_str()));
        }

        // Deterministic ordering: sort by URL so reruns produce the
        // same finding sequence.
        let mut keys: Vec<_> = buckets.keys().cloned().collect();
        keys.sort();

        let mut findings = Vec::new();
        for key in keys {
            let occurrences = buckets.get(&key).cloned().unwrap_or_default();
            if occurrences.len() < self.config.min_repeat_count {
                continue;
            }
            let (method, url) = key;
            let evidence: Vec<EvidenceEntry> = occurrences
                .iter()
                .enumerate()
                .map(|(seq, (idx, u))| EvidenceEntry {
                    entry_index: *idx,
                    url: (*u).to_string(),
                    note: format!("occurrence #{}", seq + 1),
                })
                .collect();
            let bytes_per_dup = har
                .log
                .entries
                .get(occurrences[0].0)
                .map(|e| u64::try_from(e.response.body_size.max(0)).unwrap_or(0))
                .unwrap_or(0);
            let extra_count = occurrences.len().saturating_sub(1);
            let extra_count_u64 = u64::try_from(extra_count).unwrap_or(u64::MAX);
            let bytes_saved = bytes_per_dup.saturating_mul(extra_count_u64);

            findings.push(Finding {
                id: Finding::PENDING_ID.to_string(),
                title: format!(
                    "Duplicate {method} {url} — {n} identical fetches",
                    n = occurrences.len()
                ),
                severity: severity_for(occurrences.len()),
                category: Category::WastefulRequest,
                status: Status::Open,
                evidence,
                impact: ImpactEstimate {
                    bytes_saved_per_build: bytes_saved,
                    requests_saved_per_build: extra_count_u64,
                    connections_saved_per_build: 0,
                },
                proposal: format!(
                    "Cache the response in-session and serve the {extra_count} subsequent \
                     requests from memory. See PRD §18.3 O-REQ-01/04/05."
                ),
                references: vec![
                    "PRD §18.3 — O-REQ-01, O-REQ-04, O-REQ-05".to_string(),
                ],
                discovered_by: self.id().to_string(),
            });
        }
        findings
    }
}

fn severity_for(repeat_count: usize) -> Severity {
    match repeat_count {
        0..=2 => Severity::Low,
        3..=5 => Severity::Medium,
        6..=15 => Severity::High,
        _ => Severity::Critical,
    }
}
