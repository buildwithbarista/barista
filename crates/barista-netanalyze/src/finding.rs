// SPDX-License-Identifier: MIT OR Apache-2.0

//! `Finding` — the analysis pipeline's output record.
//!
//! Each analyzer emits zero or more findings. Findings are typed so
//! downstream tooling (the catalog at `docs/efficiency/findings/`,
//! the bench harness, the dashboard) can render or aggregate them
//! without re-parsing free-form markdown — but the canonical
//! on-disk shape is markdown with YAML frontmatter (per PRD §18.10),
//! because the catalog lives in `docs/` and is human-edited after
//! the pipeline drafts it.
//!
//! ## EFF-2026 ID-assignment policy (v0.1)
//!
//! Auto-emitted findings carry the placeholder ID
//! [`Finding::PENDING_ID`] (`EFF-2026-PENDING`). A human reviewer
//! picks the next free `EFF-2026-NNN` from the catalog when they
//! move the draft from `docs/efficiency/findings/auto-generated/`
//! to `docs/efficiency/findings/`. This keeps the pipeline
//! stateless (no central counter to coordinate against in parallel
//! runs) and makes the catalog the single source of truth for live
//! IDs. The lifecycle (open → accepted → resolved → proven) is
//! advanced by the human reviewer in lockstep.
//!
//! See `crates/barista-netanalyze/README.md` (workspace-internal)
//! and PRD §18.10 for the public contract.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Catalog-finding severity. Maps to the four-level severity scale
/// used across the resource-efficiency program (PRD §18.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Low: small per-build savings, no architectural implications.
    Low,
    /// Medium: meaningful per-build savings or moderate ecosystem
    /// impact.
    Medium,
    /// High: large per-build savings or significant ecosystem impact.
    High,
    /// Critical: blocking-level inefficiency or load-shedding-class
    /// regression.
    Critical,
}

impl Severity {
    /// Lower-case string suitable for the YAML frontmatter `severity`
    /// field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

/// Lifecycle status (PRD §18.10).
///
/// Auto-emitted findings always start at [`Status::Open`]; reviewers
/// move them along the lifecycle as the catalog is groomed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Discovered, not yet triaged.
    Open,
    /// Confirmed real, prioritized for work.
    Accepted,
    /// Fixed in code, awaiting bench verification.
    Resolved,
    /// Fix is live and bench harness confirms savings.
    Proven,
    /// Accepted as inherent or out-of-scope.
    Wontfix,
    /// Requires upstream cooperation (PRD §18.13).
    ProposedEcosystem,
}

impl Status {
    /// Lower-case string suitable for the YAML frontmatter `status`
    /// field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Accepted => "accepted",
            Self::Resolved => "resolved",
            Self::Proven => "proven",
            Self::Wontfix => "wontfix",
            Self::ProposedEcosystem => "proposed-ecosystem",
        }
    }
}

/// Coarse finding category. Used for catalog filtering and is a
/// superset of (but compatible with) the PM-* pattern identifiers
/// in PRD §18.9.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Duplicate/redundant request issued within one session.
    WastefulRequest,
    /// Compressible payload transferred without `Content-Encoding`.
    CompressionAbsent,
    /// Many short-lived TCP/TLS connections to one host.
    ConnectionChurn,
    /// Long redirect chain inflating wall-clock time.
    SlowRedirect,
    /// Repeated metadata fetches (Maven's poll-on-update behavior).
    RedundantMetadataFetch,
}

impl Category {
    /// Lower-snake-case string for the YAML frontmatter `category`
    /// field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WastefulRequest => "wasteful_request",
            Self::CompressionAbsent => "compression_absent",
            Self::ConnectionChurn => "connection_churn",
            Self::SlowRedirect => "slow_redirect",
            Self::RedundantMetadataFetch => "redundant_metadata_fetch",
        }
    }
}

/// Per-evidence record. Links a finding back to the specific HAR
/// entries that triggered it. Reviewers use this to re-open the
/// underlying capture and confirm the analyzer's call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceEntry {
    /// Zero-based index of the entry in `har.log.entries`.
    pub entry_index: usize,
    /// Request URL — copied verbatim from `request.url` for
    /// readability.
    pub url: String,
    /// Free-form note explaining why this entry is evidence.
    pub note: String,
}

/// Estimated savings from fixing the finding. v0.1 keeps these
/// simple integer fields; a richer model with confidence intervals
/// is deferred to the bench-integration milestone (PRD §18.11,
/// §18.12).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImpactEstimate {
    /// Wire bytes that would be eliminated per build.
    pub bytes_saved_per_build: u64,
    /// Requests that would be eliminated per build.
    pub requests_saved_per_build: u64,
    /// Connections that would be eliminated per build.
    pub connections_saved_per_build: u64,
}

/// A single analyzer finding.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Catalog ID. Auto-emitted findings carry
    /// [`Finding::PENDING_ID`]; reviewers assign a real
    /// `EFF-2026-NNN` when promoting to the catalog.
    pub id: String,
    /// Short, one-line title.
    pub title: String,
    /// Severity (PRD §18.10).
    pub severity: Severity,
    /// Category — analyzer family.
    pub category: Category,
    /// Lifecycle status — auto-emitted findings always start at
    /// [`Status::Open`].
    pub status: Status,
    /// Linked HAR entries that triggered the finding.
    pub evidence: Vec<EvidenceEntry>,
    /// Estimated savings if the finding is resolved.
    pub impact: ImpactEstimate,
    /// Free-form proposed mitigation. Multi-line markdown is
    /// permitted.
    pub proposal: String,
    /// Reference URLs (upstream issues, RFCs, PRD section anchors).
    pub references: Vec<String>,
    /// Originating analyzer ID — written into the frontmatter as
    /// `discovered_by` so the catalog can trace back to the rule.
    ///
    /// **Convention** (locked in M B.1 T4 — see
    /// `docs/efficiency/findings/README.md`): auto-emitted drafts
    /// use the **analyzer's stable id** (e.g.
    /// `"MetadataOverFetchAnalyzer"`, `"ConnectionChurnAnalyzer"`)
    /// rather than the generic `"claude-analysis"` label the PRD
    /// §18.10 example shows. The catalog refines that single label
    /// into three traceable provenance classes:
    ///
    /// - `<AnalyzerName>` — a `barista-netanalyze` analyzer emitted
    ///   the draft (this field).
    /// - `human-authored` — a human wrote the finding directly
    ///   (used by the seed cohort).
    /// - `claude-analysis` — an out-of-band Claude Code session
    ///   produced the finding without going through this pipeline.
    pub discovered_by: String,
}

impl Finding {
    /// Placeholder ID emitted by the pipeline. Replaced with a real
    /// `EFF-2026-NNN` by a human reviewer at promotion time. See the
    /// module-level ID-assignment policy.
    pub const PENDING_ID: &'static str = "EFF-2026-PENDING";

    /// Render the finding as a markdown document with YAML
    /// frontmatter, matching the catalog format documented in the
    /// crate root.
    ///
    /// The output is **deterministic**: identical inputs produce
    /// byte-identical output (no timestamps, no random IDs, evidence
    /// preserved in caller-supplied order). The B.1 T4 catalog tools
    /// rely on this so re-running the pipeline against the same HAR
    /// is idempotent.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("id: {}\n", self.id));
        out.push_str(&format!("title: {}\n", yaml_scalar(&self.title)));
        out.push_str(&format!("severity: {}\n", self.severity.as_str()));
        out.push_str(&format!("category: {}\n", self.category.as_str()));
        out.push_str(&format!("status: {}\n", self.status.as_str()));
        out.push_str(&format!("discovered_by: {}\n", self.discovered_by));
        out.push_str("impact:\n");
        out.push_str(&format!(
            "  bytes_saved_per_build: {}\n",
            self.impact.bytes_saved_per_build
        ));
        out.push_str(&format!(
            "  requests_saved_per_build: {}\n",
            self.impact.requests_saved_per_build
        ));
        out.push_str(&format!(
            "  connections_saved_per_build: {}\n",
            self.impact.connections_saved_per_build
        ));
        out.push_str("---\n\n");

        out.push_str("## Evidence\n\n");
        if self.evidence.is_empty() {
            out.push_str("_No specific HAR entries linked._\n\n");
        } else {
            for ev in &self.evidence {
                // Bullet-list one line per evidence entry; the URL
                // goes in backticks so reviewers can copy/paste.
                let _ = writeln!(
                    out,
                    "- Entry #{idx}: `{url}` — {note}",
                    idx = ev.entry_index,
                    url = ev.url,
                    note = ev.note
                );
            }
            out.push('\n');
        }

        out.push_str("## Impact estimate\n\n");
        out.push_str(&format!(
            "- Bytes saved per build: **{}**\n",
            self.impact.bytes_saved_per_build
        ));
        out.push_str(&format!(
            "- Requests saved per build: **{}**\n",
            self.impact.requests_saved_per_build
        ));
        out.push_str(&format!(
            "- Connections saved per build: **{}**\n\n",
            self.impact.connections_saved_per_build
        ));

        out.push_str("## Proposed mitigation\n\n");
        if self.proposal.trim().is_empty() {
            out.push_str("_No proposal recorded yet._\n\n");
        } else {
            out.push_str(self.proposal.trim_end());
            out.push_str("\n\n");
        }

        out.push_str("## References\n\n");
        if self.references.is_empty() {
            out.push_str("_None recorded._\n");
        } else {
            for r in &self.references {
                out.push_str(&format!("- {r}\n"));
            }
        }

        out
    }

    /// Suggested filename for this finding under
    /// `docs/efficiency/findings/auto-generated/`. The pipeline
    /// disambiguates collisions by suffixing `-{n}` when more than
    /// one finding lands in the same analyzer-category bucket within
    /// a single run; see [`crate::pipeline::write_findings`].
    #[must_use]
    pub fn suggested_filename(&self) -> String {
        // Stable slug: `{category}--{first-evidence-url-slug or
        // sequence}`. We keep the disambiguating numeric suffix to
        // the writer so this method stays a pure function of the
        // finding contents.
        let slug = self
            .evidence
            .first()
            .map(|e| slugify(&e.url))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "no-evidence".to_string());
        format!("{cat}--{slug}.md", cat = self.category.as_str())
    }
}

/// Conservative YAML scalar quoting: we only need to handle the
/// `title` field, which is a short single-line string. Double-quote
/// when the value contains a colon, leading whitespace, or starts
/// with a YAML-reserved indicator. Otherwise emit bare.
fn yaml_scalar(s: &str) -> String {
    let needs_quote = s.contains(':')
        || s.contains('#')
        || s.starts_with(' ')
        || s.starts_with('-')
        || s.starts_with('?')
        || s.starts_with('!')
        || s.starts_with('&')
        || s.starts_with('*')
        || s.starts_with('[')
        || s.starts_with('{')
        || s.starts_with('|')
        || s.starts_with('>')
        || s.starts_with('"')
        || s.starts_with('\'');
    if needs_quote {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

/// Strip a URL down to a filesystem-safe slug. Lower-case, ASCII
/// alphanumerics and dashes only; collapses runs of non-allowed
/// chars to a single `-`. Cap at 64 chars so filenames don't blow
/// out ext4 limits.
fn slugify(url: &str) -> String {
    let mut out = String::with_capacity(url.len().min(64));
    let mut prev_dash = false;
    for ch in url.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
        if out.len() >= 64 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

/// Helper for tests: count findings grouped by category. Not used
/// in production code paths but small enough to live alongside the
/// Finding type.
#[must_use]
pub fn count_by_category(findings: &[Finding]) -> BTreeMap<&'static str, usize> {
    let mut out = BTreeMap::new();
    for f in findings {
        *out.entry(f.category.as_str()).or_insert(0) += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn sample_finding() -> Finding {
        Finding {
            id: Finding::PENDING_ID.to_string(),
            title: "Duplicate metadata fetch within session".to_string(),
            severity: Severity::Medium,
            category: Category::RedundantMetadataFetch,
            status: Status::Open,
            evidence: vec![EvidenceEntry {
                entry_index: 3,
                url: "https://repo.example.com/g/a/maven-metadata.xml".to_string(),
                note: "first of 4 identical fetches in this session".to_string(),
            }],
            impact: ImpactEstimate {
                bytes_saved_per_build: 30_000,
                requests_saved_per_build: 3,
                connections_saved_per_build: 0,
            },
            proposal: "Dedupe per `(repo, groupId, artifactId)` within one CLI invocation."
                .to_string(),
            references: vec!["PRD §18.3 — O-REQ-01".to_string()],
            discovered_by: "MetadataOverFetchAnalyzer".to_string(),
        }
    }

    #[test]
    fn renders_deterministic_markdown() {
        let md = sample_finding().to_markdown();
        let again = sample_finding().to_markdown();
        assert_eq!(md, again, "to_markdown must be deterministic");
        assert!(md.starts_with("---\n"));
        assert!(md.contains("id: EFF-2026-PENDING\n"));
        assert!(md.contains("severity: medium\n"));
        assert!(md.contains("category: redundant_metadata_fetch\n"));
        assert!(md.contains("status: open\n"));
        assert!(md.contains("## Evidence"));
        assert!(md.contains("## Impact estimate"));
        assert!(md.contains("## Proposed mitigation"));
        assert!(md.contains("## References"));
        assert!(md.contains("PRD §18.3 — O-REQ-01"));
    }

    #[test]
    fn title_with_colon_is_quoted() {
        let mut f = sample_finding();
        f.title = "Bad: title with colon".to_string();
        let md = f.to_markdown();
        assert!(md.contains(r#"title: "Bad: title with colon""#));
    }

    #[test]
    fn suggested_filename_contains_category_and_slug() {
        let name = sample_finding().suggested_filename();
        assert!(name.starts_with("redundant_metadata_fetch--"));
        assert!(name.ends_with(".md"));
        assert!(!name.contains(' '));
    }

    #[test]
    fn empty_proposal_renders_placeholder() {
        let mut f = sample_finding();
        f.proposal = String::new();
        let md = f.to_markdown();
        assert!(md.contains("_No proposal recorded yet._"));
    }
}
