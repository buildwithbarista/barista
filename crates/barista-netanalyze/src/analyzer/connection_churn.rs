// SPDX-License-Identifier: MIT OR Apache-2.0

//! `ConnectionChurnAnalyzer` — many fresh TCP/TLS handshakes to a
//! single host where one HTTP/2 connection would suffice.
//!
//! PRD anchor: §18.5 (O-PROTO-01, O-PROTO-02), pattern
//! `PM-CONNECTION-CHURN` from §18.9.
//!
//! Signal sources, in priority order:
//!   1. `timings.connect >= 0` (fresh handshake) per HAR 1.2.
//!   2. `Connection: close` request/response header — explicit
//!      one-shot connection.
//!   3. `connection` field changes across consecutive entries to the
//!      same host (mitmproxy emits a distinct connection id per
//!      flow).

use std::collections::HashMap;

use crate::analyzer::Analyzer;
use crate::finding::{Category, EvidenceEntry, Finding, ImpactEstimate, Severity, Status};
use crate::har::Har;

/// Tunable thresholds for [`ConnectionChurnAnalyzer`].
#[derive(Debug, Clone)]
pub struct ConnectionChurnConfig {
    /// Minimum number of fresh connections to a single host before
    /// emitting a finding. Default `3` — two fresh connections is
    /// inside the noise band of legitimate parallelism.
    pub min_fresh_connections: usize,
}

impl Default for ConnectionChurnConfig {
    fn default() -> Self {
        Self {
            min_fresh_connections: 3,
        }
    }
}

/// Detects per-host connection churn.
#[derive(Debug, Clone, Default)]
pub struct ConnectionChurnAnalyzer {
    config: ConnectionChurnConfig,
}

impl ConnectionChurnAnalyzer {
    /// Construct with default thresholds.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self {
            config: ConnectionChurnConfig::default(),
        }
    }

    /// Construct with custom thresholds.
    #[must_use]
    pub fn new(config: ConnectionChurnConfig) -> Self {
        Self { config }
    }
}

impl Analyzer for ConnectionChurnAnalyzer {
    fn id(&self) -> &'static str {
        "ConnectionChurnAnalyzer"
    }

    fn analyze(&self, har: &Har) -> Vec<Finding> {
        // For each host, collect the entries that opened a fresh
        // connection. A "fresh" connection is one where the
        // `timings.connect` value is non-negative (HAR 1.2 sets it
        // to `-1` when the connection was reused).
        let mut per_host: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, entry) in har.log.entries.iter().enumerate() {
            let Some(host) = entry.host() else { continue };
            let close_signal = entry
                .request_header("connection")
                .map(|v| v.eq_ignore_ascii_case("close"))
                .unwrap_or(false)
                || entry
                    .response_header("connection")
                    .map(|v| v.eq_ignore_ascii_case("close"))
                    .unwrap_or(false);
            if entry.opened_new_connection() || close_signal {
                per_host.entry(host).or_default().push(idx);
            }
        }

        let mut hosts: Vec<_> = per_host.keys().cloned().collect();
        hosts.sort();

        let mut findings = Vec::new();
        for host in hosts {
            let occurrences = per_host.get(&host).cloned().unwrap_or_default();
            if occurrences.len() < self.config.min_fresh_connections {
                continue;
            }
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
                        note: "fresh TCP/TLS handshake".to_string(),
                    }
                })
                .collect();
            let extra_count = occurrences.len().saturating_sub(1);
            findings.push(Finding {
                id: Finding::PENDING_ID.to_string(),
                title: format!(
                    "Connection churn on `{host}` — {n} fresh handshakes",
                    n = occurrences.len()
                ),
                severity: severity_for(occurrences.len()),
                category: Category::ConnectionChurn,
                status: Status::Open,
                evidence,
                impact: ImpactEstimate {
                    bytes_saved_per_build: 0,
                    requests_saved_per_build: 0,
                    connections_saved_per_build: u64::try_from(extra_count).unwrap_or(u64::MAX),
                },
                proposal: format!(
                    "Reuse a single HTTP/2 connection to `{host}` across all requests; \
                     drop any `Connection: close` hint from the client. See PRD §18.5 \
                     O-PROTO-01 (default H/2) and O-PROTO-02 (cert coalescing)."
                ),
                references: vec![
                    "PRD §18.5 — O-PROTO-01, O-PROTO-02".to_string(),
                    "PRD §18.9 — PM-CONNECTION-CHURN".to_string(),
                ],
                discovered_by: self.id().to_string(),
            });
        }
        findings
    }
}

fn severity_for(count: usize) -> Severity {
    match count {
        0..=3 => Severity::Low,
        4..=9 => Severity::Medium,
        10..=49 => Severity::High,
        _ => Severity::Critical,
    }
}
