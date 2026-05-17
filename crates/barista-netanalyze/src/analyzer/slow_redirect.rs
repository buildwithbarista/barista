//! `SlowRedirectAnalyzer` — chains of 30x responses where each hop
//! adds non-trivial wall time.
//!
//! PRD anchor: §18.3 (O-REQ-07 — mirror substitution avoids the
//! first-hop redirect entirely). Detecting these post-hoc tells us
//! when an upstream is forcing an avoidable round trip.

use crate::analyzer::Analyzer;
use crate::finding::{
    Category, EvidenceEntry, Finding, ImpactEstimate, Severity, Status,
};
use crate::har::Har;

/// Tunable thresholds for [`SlowRedirectAnalyzer`].
#[derive(Debug, Clone)]
pub struct SlowRedirectConfig {
    /// Per-hop wall-time budget in milliseconds. Hops above this
    /// are counted. Default `100ms` — anything below is plausibly
    /// "redirect over already-warm H/2 connection" and not worth
    /// flagging.
    pub per_hop_ms: f64,
    /// Minimum number of slow redirect responses in the HAR before
    /// emitting a finding. Default `1` — even one slow redirect is
    /// signal in the resource-efficiency program.
    pub min_count: usize,
}

impl Default for SlowRedirectConfig {
    fn default() -> Self {
        Self {
            per_hop_ms: 100.0,
            min_count: 1,
        }
    }
}

/// Detects 30x responses with wall time above the per-hop budget.
#[derive(Debug, Clone, Default)]
pub struct SlowRedirectAnalyzer {
    config: SlowRedirectConfig,
}

impl SlowRedirectAnalyzer {
    /// Construct with default thresholds.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self {
            config: SlowRedirectConfig::default(),
        }
    }

    /// Construct with custom thresholds.
    #[must_use]
    pub fn new(config: SlowRedirectConfig) -> Self {
        Self { config }
    }
}

impl Analyzer for SlowRedirectAnalyzer {
    fn id(&self) -> &'static str {
        "SlowRedirectAnalyzer"
    }

    fn analyze(&self, har: &Har) -> Vec<Finding> {
        let mut slow: Vec<(usize, &str, f64, u16)> = Vec::new();
        for (idx, entry) in har.log.entries.iter().enumerate() {
            let status = entry.response.status;
            if !(300..400).contains(&status) {
                continue;
            }
            if entry.time < self.config.per_hop_ms {
                continue;
            }
            slow.push((idx, entry.request.url.as_str(), entry.time, status));
        }
        if slow.len() < self.config.min_count {
            return Vec::new();
        }

        let evidence: Vec<EvidenceEntry> = slow
            .iter()
            .map(|(idx, url, ms, status)| EvidenceEntry {
                entry_index: *idx,
                url: (*url).to_string(),
                note: format!("{status} redirect, {ms:.1}ms total"),
            })
            .collect();
        let extra_requests = u64::try_from(slow.len()).unwrap_or(u64::MAX);
        vec![Finding {
            id: Finding::PENDING_ID.to_string(),
            title: format!(
                "Slow redirect chain — {n} hops above {budget}ms each",
                n = slow.len(),
                budget = self.config.per_hop_ms
            ),
            severity: severity_for_count(slow.len()),
            category: Category::SlowRedirect,
            status: Status::Open,
            evidence,
            impact: ImpactEstimate {
                bytes_saved_per_build: 0,
                requests_saved_per_build: extra_requests,
                connections_saved_per_build: 0,
            },
            proposal: "Apply mirror substitution at resolve time so the original URL is \
                       never requested. See PRD §18.3 O-REQ-07."
                .to_string(),
            references: vec!["PRD §18.3 — O-REQ-07".to_string()],
            discovered_by: self.id().to_string(),
        }]
    }
}

fn severity_for_count(count: usize) -> Severity {
    match count {
        0..=1 => Severity::Low,
        2..=4 => Severity::Medium,
        5..=14 => Severity::High,
        _ => Severity::Critical,
    }
}
