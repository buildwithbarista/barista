// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Per-analyzer fixture-based smoke tests.
//!
//! Each analyzer has a positive-case fixture (a hand-crafted HAR
//! engineered to trip the rule) and a negative-case fixture (one
//! engineered *not* to). The positive case must emit ≥1 finding;
//! the negative case must emit 0.
//!
//! A final "multi-issue" fixture exercises the full default
//! registry against a HAR that contains several known issues, and
//! asserts the expected analyzer IDs all fire — a property-style
//! check that the registry orchestration in `pipeline::analyze`
//! does not silently drop analyzers.

use std::path::PathBuf;

use barista_netanalyze::{
    Analyzer, ConnectionChurnAnalyzer, DuplicateRequestAnalyzer, Finding,
    MetadataOverFetchAnalyzer, SlowRedirectAnalyzer, UncompressedTransferAnalyzer, analyze,
    analyze_with, load_har, write_findings,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn run_single(analyzer: Box<dyn Analyzer>, fixture_name: &str) -> Vec<Finding> {
    let har = load_har(&fixture(fixture_name)).expect("load fixture HAR");
    analyze_with(&har, &[analyzer])
}

#[test]
fn duplicate_request_positive_emits_finding() {
    let findings = run_single(
        Box::new(DuplicateRequestAnalyzer::with_defaults()),
        "duplicate_request_positive.har",
    );
    assert_eq!(findings.len(), 1);
    let f = &findings[0];
    assert_eq!(f.discovered_by, "DuplicateRequestAnalyzer");
    assert_eq!(f.id, Finding::PENDING_ID);
    assert_eq!(f.evidence.len(), 3);
    assert!(f.impact.requests_saved_per_build >= 2);
}

#[test]
fn duplicate_request_negative_is_silent() {
    let findings = run_single(
        Box::new(DuplicateRequestAnalyzer::with_defaults()),
        "duplicate_request_negative.har",
    );
    assert!(
        findings.is_empty(),
        "expected no findings, got {findings:?}"
    );
}

#[test]
fn uncompressed_transfer_positive_emits_finding() {
    let findings = run_single(
        Box::new(UncompressedTransferAnalyzer::with_defaults()),
        "uncompressed_transfer_positive.har",
    );
    assert_eq!(findings.len(), 1);
    let f = &findings[0];
    assert_eq!(f.discovered_by, "UncompressedTransferAnalyzer");
    assert!(f.impact.bytes_saved_per_build > 0);
}

#[test]
fn uncompressed_transfer_negative_is_silent() {
    let findings = run_single(
        Box::new(UncompressedTransferAnalyzer::with_defaults()),
        "uncompressed_transfer_negative.har",
    );
    assert!(
        findings.is_empty(),
        "expected no findings, got {findings:?}"
    );
}

#[test]
fn connection_churn_positive_emits_finding() {
    let findings = run_single(
        Box::new(ConnectionChurnAnalyzer::with_defaults()),
        "connection_churn_positive.har",
    );
    assert_eq!(findings.len(), 1);
    let f = &findings[0];
    assert_eq!(f.discovered_by, "ConnectionChurnAnalyzer");
    assert!(f.impact.connections_saved_per_build >= 3);
}

#[test]
fn connection_churn_negative_is_silent() {
    let findings = run_single(
        Box::new(ConnectionChurnAnalyzer::with_defaults()),
        "connection_churn_negative.har",
    );
    assert!(
        findings.is_empty(),
        "expected no findings, got {findings:?}"
    );
}

#[test]
fn slow_redirect_positive_emits_finding() {
    let findings = run_single(
        Box::new(SlowRedirectAnalyzer::with_defaults()),
        "slow_redirect_positive.har",
    );
    assert_eq!(findings.len(), 1);
    let f = &findings[0];
    assert_eq!(f.discovered_by, "SlowRedirectAnalyzer");
    // Two 30x responses cross the per-hop budget in the fixture.
    assert!(f.evidence.len() >= 2);
}

#[test]
fn slow_redirect_negative_is_silent() {
    let findings = run_single(
        Box::new(SlowRedirectAnalyzer::with_defaults()),
        "slow_redirect_negative.har",
    );
    assert!(
        findings.is_empty(),
        "expected no findings, got {findings:?}"
    );
}

#[test]
fn metadata_overfetch_positive_emits_finding() {
    let findings = run_single(
        Box::new(MetadataOverFetchAnalyzer::with_defaults()),
        "metadata_overfetch_positive.har",
    );
    assert_eq!(findings.len(), 1);
    let f = &findings[0];
    assert_eq!(f.discovered_by, "MetadataOverFetchAnalyzer");
    assert_eq!(f.evidence.len(), 3);
    assert!(f.impact.requests_saved_per_build >= 2);
}

#[test]
fn metadata_overfetch_negative_is_silent() {
    let findings = run_single(
        Box::new(MetadataOverFetchAnalyzer::with_defaults()),
        "metadata_overfetch_negative.har",
    );
    assert!(
        findings.is_empty(),
        "expected no findings, got {findings:?}"
    );
}

#[test]
fn empty_session_emits_zero_findings_via_full_pipeline() {
    // Mirrors the netcap T1 smoke fixture (`session_smoke.rs`):
    // an empty `log.entries` array is a valid HAR with nothing to
    // flag.
    let har = load_har(&fixture("empty_session.har")).expect("load empty");
    let findings = analyze(&har);
    assert!(
        findings.is_empty(),
        "empty session should produce no findings, got {findings:?}"
    );
}

#[test]
fn multi_issue_session_trips_expected_analyzers() {
    // The kitchen-sink fixture deliberately exercises:
    //   - 3× identical metadata fetches → MetadataOverFetchAnalyzer
    //     + DuplicateRequestAnalyzer
    //   - All 4 entries with Connection: close + fresh handshakes
    //     → ConnectionChurnAnalyzer (across two hosts)
    //   - 60 KB application/xml without Content-Encoding →
    //     UncompressedTransferAnalyzer
    //   - 250 ms 301 redirect → SlowRedirectAnalyzer
    let har = load_har(&fixture("multi_issue_session.har")).expect("load multi-issue");
    let findings = analyze(&har);
    let ids: std::collections::BTreeSet<_> =
        findings.iter().map(|f| f.discovered_by.clone()).collect();
    let expected = [
        "DuplicateRequestAnalyzer",
        "UncompressedTransferAnalyzer",
        "ConnectionChurnAnalyzer",
        "SlowRedirectAnalyzer",
        "MetadataOverFetchAnalyzer",
    ];
    for name in expected {
        assert!(
            ids.contains(name),
            "expected {name} to fire on multi-issue fixture; saw {ids:?}"
        );
    }
}

#[test]
fn write_findings_round_trips_markdown_files() {
    // [T] AC: Finding::to_markdown produces a parseable,
    // deterministic markdown file.
    let har = load_har(&fixture("metadata_overfetch_positive.har")).expect("load");
    let findings = analyze(&har);
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = write_findings(&findings, dir.path()).expect("write");
    assert!(!paths.is_empty());
    for path in &paths {
        let body = std::fs::read_to_string(path).expect("read");
        assert!(body.starts_with("---\n"));
        assert!(body.contains("id: EFF-2026-PENDING\n"));
        assert!(body.contains("## Evidence"));
        assert!(body.contains("## Impact estimate"));
        assert!(body.contains("## Proposed mitigation"));
        assert!(body.contains("## References"));
    }
    // Re-running against the same HAR + tempdir overwrites and
    // yields byte-identical content (determinism check).
    let first = std::fs::read_to_string(&paths[0]).expect("read 1st time");
    let _again = write_findings(&findings, dir.path()).expect("write again");
    let second = std::fs::read_to_string(&paths[0]).expect("read 2nd time");
    assert_eq!(first, second, "to_markdown must be deterministic");
}
