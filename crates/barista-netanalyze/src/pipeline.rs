// SPDX-License-Identifier: MIT OR Apache-2.0

//! Orchestrator: load a HAR, run the analyzer registry, write
//! per-finding markdown files.
//!
//! This module is a thin glue layer — all the interesting logic
//! lives in [`crate::analyzer`] and [`crate::finding`].

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::analyzer::{Analyzer, default_registry};
use crate::error::AnalyzeError;
use crate::finding::Finding;
use crate::har::{Har, parse_har_bytes};

/// Load a HAR file from `path`. Returns a parsed in-memory model
/// on success.
pub fn load_har(path: &Path) -> Result<Har, AnalyzeError> {
    let bytes = fs::read(path).map_err(|source| AnalyzeError::HarRead {
        path: path.to_path_buf(),
        source,
    })?;
    parse_har_bytes(&bytes).map_err(|reason| AnalyzeError::HarInvalid {
        path: path.to_path_buf(),
        reason,
    })
}

/// Run the default analyzer registry against `har` and return all
/// findings in registry-then-analyzer-internal order. The output
/// order is deterministic so the catalog tool can diff successive
/// runs.
#[must_use]
pub fn analyze(har: &Har) -> Vec<Finding> {
    analyze_with(har, &default_registry())
}

/// Run a caller-supplied analyzer registry against `har`. Used by
/// tests that want to exercise a single analyzer in isolation.
#[must_use]
pub fn analyze_with(har: &Har, registry: &[Box<dyn Analyzer>]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for analyzer in registry {
        findings.extend(analyzer.analyze(har));
    }
    findings
}

/// Write each finding as a separate markdown file under
/// `output_dir`. Returns the list of paths created.
///
/// Filename collisions inside the same run are disambiguated with a
/// numeric suffix (`-1`, `-2`, ...). The pipeline does **not**
/// inspect pre-existing files in the output dir — re-running
/// overwrites prior output.
pub fn write_findings(
    findings: &[Finding],
    output_dir: &Path,
) -> Result<Vec<PathBuf>, AnalyzeError> {
    fs::create_dir_all(output_dir)?;
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut paths = Vec::with_capacity(findings.len());
    for finding in findings {
        let base = finding.suggested_filename();
        let counter = seen.entry(base.clone()).or_insert(0);
        let path = if *counter == 0 {
            output_dir.join(&base)
        } else {
            let stem = base.strip_suffix(".md").unwrap_or(&base);
            output_dir.join(format!("{stem}-{counter}.md"))
        };
        *counter += 1;
        let md = finding.to_markdown();
        fs::write(&path, md).map_err(|source| AnalyzeError::FindingWrite {
            path: path.clone(),
            source,
        })?;
        paths.push(path);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::finding::{Category, EvidenceEntry, ImpactEstimate, Severity, Status};
    use tempfile::tempdir;

    fn finding(slug: &str) -> Finding {
        Finding {
            id: Finding::PENDING_ID.to_string(),
            title: "test".to_string(),
            severity: Severity::Low,
            category: Category::WastefulRequest,
            status: Status::Open,
            evidence: vec![EvidenceEntry {
                entry_index: 0,
                url: format!("https://example.com/{slug}"),
                note: "n".to_string(),
            }],
            impact: ImpactEstimate::default(),
            proposal: "p".to_string(),
            references: vec![],
            discovered_by: "test".to_string(),
        }
    }

    #[test]
    fn write_findings_creates_files_and_disambiguates() {
        let dir = tempdir().expect("tempdir");
        let findings = vec![finding("a"), finding("a"), finding("b")];
        let paths = write_findings(&findings, dir.path()).expect("write");
        assert_eq!(paths.len(), 3);
        // First two share the same evidence URL slug → second is
        // suffixed with `-1`.
        assert!(
            paths[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".md")
        );
        assert_ne!(paths[0], paths[1]);
        for path in &paths {
            assert!(path.exists());
        }
    }

    #[test]
    fn analyze_returns_findings_from_registry() {
        let har_json = r#"{
          "log": {
            "version": "1.2",
            "entries": [
              {
                "startedDateTime": "2026-05-15T00:00:00Z",
                "time": 1.0,
                "request": {"method": "GET", "url": "https://r.example/x", "headers": [], "bodySize": 0},
                "response": {"status": 200, "statusText": "OK", "headers": [], "content": {"size": 1, "mimeType": "text/plain"}, "bodySize": 1},
                "timings": {"connect": -1.0, "send": 0.0, "wait": 1.0, "receive": 0.0}
              },
              {
                "startedDateTime": "2026-05-15T00:00:01Z",
                "time": 1.0,
                "request": {"method": "GET", "url": "https://r.example/x", "headers": [], "bodySize": 0},
                "response": {"status": 200, "statusText": "OK", "headers": [], "content": {"size": 1, "mimeType": "text/plain"}, "bodySize": 1},
                "timings": {"connect": -1.0, "send": 0.0, "wait": 1.0, "receive": 0.0}
              }
            ]
          }
        }"#;
        let har = parse_har_bytes(har_json.as_bytes()).expect("parse");
        let findings = analyze(&har);
        // The two identical GETs trip DuplicateRequestAnalyzer.
        assert!(
            findings
                .iter()
                .any(|f| f.discovered_by == "DuplicateRequestAnalyzer"),
            "expected a DuplicateRequestAnalyzer finding, got: {:?}",
            findings
                .iter()
                .map(|f| &f.discovered_by)
                .collect::<Vec<_>>()
        );
    }
}
