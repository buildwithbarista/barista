//! `UncompressedTransferAnalyzer` — compressible payload sent
//! without `Content-Encoding`.
//!
//! PRD anchor: §18.4 (O-XFER-01, O-XFER-02). XML and JSON payloads
//! over a few KiB compress 70–85% with zstd/brotli; sending them
//! plaintext is one of the simplest fixes in the ecosystem.
//!
//! Pattern PRD §18.9: `PM-MISSED-COMPRESSION`.

use crate::analyzer::Analyzer;
use crate::finding::{Category, EvidenceEntry, Finding, ImpactEstimate, Severity, Status};
use crate::har::Har;

/// Tunable thresholds for [`UncompressedTransferAnalyzer`].
#[derive(Debug, Clone)]
pub struct UncompressedTransferConfig {
    /// Minimum `response.content.size` (decompressed bytes) for an
    /// entry to be considered worth compressing. Default 10 KiB —
    /// below this, header overhead overwhelms the saving.
    pub min_size_bytes: i64,
    /// Estimated compression ratio used for the impact estimate
    /// (0.0..=1.0 — fraction of bytes saved). Default 0.75 (zstd on
    /// XML/JSON Maven payloads, observed empirically).
    pub estimated_ratio: f64,
}

impl Default for UncompressedTransferConfig {
    fn default() -> Self {
        Self {
            min_size_bytes: 10 * 1024,
            estimated_ratio: 0.75,
        }
    }
}

/// Detects compressible responses sent without a recognised
/// `Content-Encoding`.
#[derive(Debug, Clone, Default)]
pub struct UncompressedTransferAnalyzer {
    config: UncompressedTransferConfig,
}

impl UncompressedTransferAnalyzer {
    /// Construct with default thresholds.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self {
            config: UncompressedTransferConfig::default(),
        }
    }

    /// Construct with custom thresholds.
    #[must_use]
    pub fn new(config: UncompressedTransferConfig) -> Self {
        Self { config }
    }
}

impl Analyzer for UncompressedTransferAnalyzer {
    fn id(&self) -> &'static str {
        "UncompressedTransferAnalyzer"
    }

    fn analyze(&self, har: &Har) -> Vec<Finding> {
        let mut findings = Vec::new();
        for (idx, entry) in har.log.entries.iter().enumerate() {
            if entry.response.status < 200 || entry.response.status >= 300 {
                continue;
            }
            let mime = entry
                .response_header("content-type")
                .unwrap_or(entry.response.content.mime_type.as_str());
            if !is_compressible_mime(mime) {
                continue;
            }
            if entry.response.content.size < self.config.min_size_bytes {
                continue;
            }
            let encoding = entry.response_header("content-encoding").unwrap_or("");
            if is_recognised_compression(encoding) {
                continue;
            }
            let size_u64 = u64::try_from(entry.response.content.size.max(0)).unwrap_or(0);
            let size_f64 = i64_to_f64(entry.response.content.size.max(0));
            // Multiplying then truncating is intentional — sub-byte
            // fractions of a saving are meaningless at finding-level
            // aggregation.
            let saved_f64 = size_f64 * self.config.estimated_ratio;
            let saved_u64 = f64_to_u64(saved_f64);

            findings.push(Finding {
                id: Finding::PENDING_ID.to_string(),
                title: format!("Compressible {mime} response sent uncompressed ({size_u64} bytes)"),
                severity: severity_for_size(size_u64),
                category: Category::CompressionAbsent,
                status: Status::Open,
                evidence: vec![EvidenceEntry {
                    entry_index: idx,
                    url: entry.request.url.clone(),
                    note: format!(
                        "Content-Type `{mime}`, Content-Encoding `{enc}`",
                        enc = if encoding.is_empty() {
                            "<none>"
                        } else {
                            encoding
                        }
                    ),
                }],
                impact: ImpactEstimate {
                    bytes_saved_per_build: saved_u64,
                    requests_saved_per_build: 0,
                    connections_saved_per_build: 0,
                },
                proposal: "Negotiate `Accept-Encoding: zstd, br, gzip` and require the \
                           upstream repo manager support at least gzip. See PRD §18.4 \
                           O-XFER-01/02 and §18.13 EFF-PROPOSED-04 for the \
                           server-side rollout."
                    .to_string(),
                references: vec![
                    "PRD §18.4 — O-XFER-01, O-XFER-02".to_string(),
                    "PRD §18.13 — EFF-PROPOSED-04 (zstd rollout)".to_string(),
                ],
                discovered_by: self.id().to_string(),
            });
        }
        findings
    }
}

fn is_compressible_mime(mime: &str) -> bool {
    // Strip parameters: `application/json; charset=utf-8` → base.
    let base = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "application/json" | "application/xml" | "application/javascript" | "application/xhtml+xml"
    ) || base.starts_with("text/")
}

fn is_recognised_compression(encoding: &str) -> bool {
    let lc = encoding.to_ascii_lowercase();
    // HAR values can be comma-separated; any recognised token wins.
    lc.split(',')
        .map(str::trim)
        .any(|t| matches!(t, "gzip" | "br" | "zstd" | "deflate"))
}

fn severity_for_size(bytes: u64) -> Severity {
    match bytes {
        0..=49_999 => Severity::Low,
        50_000..=499_999 => Severity::Medium,
        500_000..=4_999_999 => Severity::High,
        _ => Severity::Critical,
    }
}

// The workspace lint policy warns on `as` conversions; these two
// helpers funnel the warning through a single justification.
#[allow(clippy::as_conversions)]
fn i64_to_f64(v: i64) -> f64 {
    // f64 holds every i64 exactly up to 2^53; response sizes beyond
    // that are impossible in practice (8 PB single HTTP body).
    v as f64
}

#[allow(clippy::as_conversions)]
fn f64_to_u64(v: f64) -> u64 {
    if v.is_finite() && (0.0..=9.0e18).contains(&v) {
        // Truncation to u64 is the intended rounding; we already
        // bounds-checked the float.
        v as u64
    } else {
        0
    }
}
