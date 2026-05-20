// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for `cargo xtask findings`. These exercise
//! the promotion ceremony and the catalog-listing renderer against
//! a tempdir fixture so they don't touch the real
//! `docs/efficiency/findings/` tree.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use xtask::findings::{self, CatalogRow, FindingsError, collect_rows, list_catalog, promote_draft};

/// Minimal valid draft body — frontmatter plus the four required
/// body sections. Used as the fixture for promotion tests.
const PENDING_DRAFT: &str = "---\n\
id: EFF-2026-PENDING\n\
title: Auto draft for promotion test\n\
severity: low\n\
category: wasteful_request\n\
status: open\n\
discovered_by: DuplicateRequestAnalyzer\n\
impact:\n\
  bytes_saved_per_build: 100\n\
  requests_saved_per_build: 1\n\
  connections_saved_per_build: 0\n\
---\n\
\n\
## Evidence\n\
\n\
- some entry\n\
\n\
## Impact estimate\n\
\n\
- some impact\n\
\n\
## Proposed mitigation\n\
\n\
do the thing\n\
\n\
## References\n\
\n\
- PRD §18.9\n";

/// Existing catalog entry; used to seed the catalog so the allocator
/// has to pick `NNN+1` rather than `001`.
fn catalog_entry(id: &str, title: &str, severity: &str, status: &str) -> String {
    format!(
        "---\n\
id: {id}\n\
title: {title}\n\
severity: {severity}\n\
category: redundant_metadata_fetch\n\
status: {status}\n\
discovered_by: human-authored\n\
---\n\
\n\
## Evidence\n\
- a\n\
\n\
## Impact estimate\n\
- b\n\
\n\
## Proposed mitigation\n\
c\n\
\n\
## References\n\
- d\n"
    )
}

/// Build a catalog skeleton: `<tmp>/findings/` (catalog dir) plus
/// `<tmp>/findings/auto-generated/` (drafts dir).
fn skeleton(tmp: &TempDir) -> (PathBuf, PathBuf) {
    let catalog = tmp.path().join("findings");
    let auto = catalog.join("auto-generated");
    fs::create_dir_all(&auto).unwrap();
    (catalog, auto)
}

#[test]
fn promote_smoke_allocates_next_id_and_moves_file() {
    let tmp = TempDir::new().unwrap();
    let (catalog, auto) = skeleton(&tmp);

    // Seed catalog with EFF-2026-001 and EFF-2026-002 so the allocator
    // must pick 003.
    fs::write(
        catalog.join("EFF-2026-001.md"),
        catalog_entry("EFF-2026-001", "First", "high", "open"),
    )
    .unwrap();
    fs::write(
        catalog.join("EFF-2026-002.md"),
        catalog_entry("EFF-2026-002", "Second", "medium", "open"),
    )
    .unwrap();

    // Drop a draft into auto-generated/.
    let draft = auto.join("wasteful_request--example.md");
    fs::write(&draft, PENDING_DRAFT).unwrap();

    let result = promote_draft(&draft, &catalog).expect("promote ok");
    assert_eq!(result.allocated_id, "EFF-2026-003");
    // `promote_draft` returns the canonicalized destination — on macOS
    // that means `/var/...` becomes `/private/var/...`. Canonicalize
    // the expected path to match.
    let expected = catalog.join("EFF-2026-003.md").canonicalize().unwrap();
    assert_eq!(result.dest, expected);

    // Draft is gone, destination exists with id rewritten in
    // frontmatter.
    assert!(!draft.exists(), "draft should be moved");
    let promoted = fs::read_to_string(&result.dest).unwrap();
    assert!(
        promoted.contains("id: EFF-2026-003"),
        "id should be rewritten"
    );
    assert!(
        !promoted.contains("EFF-2026-PENDING"),
        "placeholder id should be gone"
    );
    // Body sections preserved verbatim.
    assert!(promoted.contains("## Evidence"));
    assert!(promoted.contains("## Impact estimate"));
    assert!(promoted.contains("## Proposed mitigation"));
    assert!(promoted.contains("## References"));
}

#[test]
fn promote_allocates_001_in_empty_catalog() {
    let tmp = TempDir::new().unwrap();
    let (catalog, auto) = skeleton(&tmp);
    let draft = auto.join("draft.md");
    fs::write(&draft, PENDING_DRAFT).unwrap();

    let result = promote_draft(&draft, &catalog).expect("promote ok");
    assert_eq!(result.allocated_id, "EFF-2026-001");
}

#[test]
fn promote_handles_gap_in_id_sequence() {
    let tmp = TempDir::new().unwrap();
    let (catalog, auto) = skeleton(&tmp);

    // Catalog has 001 and 007 but no 002..006. Allocator picks 008.
    fs::write(
        catalog.join("EFF-2026-001.md"),
        catalog_entry("EFF-2026-001", "First", "high", "open"),
    )
    .unwrap();
    fs::write(
        catalog.join("EFF-2026-007.md"),
        catalog_entry("EFF-2026-007", "Seventh", "medium", "open"),
    )
    .unwrap();

    let draft = auto.join("draft.md");
    fs::write(&draft, PENDING_DRAFT).unwrap();
    let result = promote_draft(&draft, &catalog).expect("promote ok");
    assert_eq!(result.allocated_id, "EFF-2026-008");
}

#[test]
fn promote_refuses_draft_outside_auto_generated() {
    let tmp = TempDir::new().unwrap();
    let (catalog, _auto) = skeleton(&tmp);

    // Draft sits next to the catalog, not under auto-generated/.
    let stray = catalog.join("stray.md");
    fs::write(&stray, PENDING_DRAFT).unwrap();

    let err = promote_draft(&stray, &catalog).expect_err("should refuse");
    match err {
        FindingsError::DraftNotUnderAutoGenerated { .. } => {}
        other => panic!("expected DraftNotUnderAutoGenerated, got {other:?}"),
    }
}

#[test]
fn promote_refuses_draft_missing_required_field() {
    let tmp = TempDir::new().unwrap();
    let (catalog, auto) = skeleton(&tmp);

    let bad = PENDING_DRAFT.replace("discovered_by: DuplicateRequestAnalyzer\n", "");
    let draft = auto.join("bad.md");
    fs::write(&draft, bad).unwrap();

    let err = promote_draft(&draft, &catalog).expect_err("should refuse");
    match err {
        FindingsError::InvalidDraft { reason, .. } => {
            assert!(
                reason.contains("discovered_by"),
                "reason should mention missing field: {reason}"
            );
        }
        other => panic!("expected InvalidDraft, got {other:?}"),
    }
    // Draft must still be on disk — invalid drafts are not destroyed.
    assert!(draft.exists());
}

#[test]
fn promote_refuses_draft_missing_required_body_section() {
    let tmp = TempDir::new().unwrap();
    let (catalog, auto) = skeleton(&tmp);

    let bad = PENDING_DRAFT.replace("## References", "## Misc");
    let draft = auto.join("bad-body.md");
    fs::write(&draft, bad).unwrap();

    let err = promote_draft(&draft, &catalog).expect_err("should refuse");
    match err {
        FindingsError::InvalidDraft { reason, .. } => {
            assert!(
                reason.contains("References"),
                "reason should mention missing section: {reason}"
            );
        }
        other => panic!("expected InvalidDraft, got {other:?}"),
    }
    assert!(draft.exists());
}

#[test]
fn list_renders_table_with_seed_rows() {
    let tmp = TempDir::new().unwrap();
    let (catalog, _auto) = skeleton(&tmp);

    fs::write(
        catalog.join("EFF-2026-001.md"),
        catalog_entry("EFF-2026-001", "First", "high", "open"),
    )
    .unwrap();
    fs::write(
        catalog.join("EFF-2026-002.md"),
        catalog_entry("EFF-2026-002", "Second", "medium", "accepted"),
    )
    .unwrap();
    fs::write(
        catalog.join("EFF-2026-003.md"),
        catalog_entry("EFF-2026-003", "Third", "low", "open"),
    )
    .unwrap();

    let rows = collect_rows(&catalog).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].id, "EFF-2026-001");
    assert_eq!(rows[1].id, "EFF-2026-002");
    assert_eq!(rows[2].id, "EFF-2026-003");

    let table = list_catalog(&catalog).unwrap();
    assert!(table.contains("ID"));
    assert!(table.contains("Title"));
    assert!(table.contains("Severity"));
    assert!(table.contains("Category"));
    assert!(table.contains("Status"));
    assert!(table.contains("Discovered-by"));
    assert!(table.contains("EFF-2026-001"));
    assert!(table.contains("EFF-2026-003"));
    // Ordering is numeric on the trailing NNN.
    let pos1 = table.find("EFF-2026-001").unwrap();
    let pos10 = table.find("EFF-2026-003").unwrap();
    assert!(pos1 < pos10);
}

#[test]
fn list_ignores_non_catalog_files() {
    let tmp = TempDir::new().unwrap();
    let (catalog, _auto) = skeleton(&tmp);

    fs::write(catalog.join("README.md"), "# Catalog\n").unwrap();
    fs::write(
        catalog.join("EFF-2026-001.md"),
        catalog_entry("EFF-2026-001", "Only", "high", "open"),
    )
    .unwrap();
    // Also write a sibling that looks plausible but isn't a catalog
    // file — wrong year.
    fs::write(
        catalog.join("EFF-2025-099.md"),
        catalog_entry("EFF-2025-099", "Old", "low", "open"),
    )
    .unwrap();

    let rows = collect_rows(&catalog).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "EFF-2026-001");
}

#[test]
fn list_against_real_catalog_finds_seed_cohort() {
    // Belt-and-suspenders test: read the actual seed catalog
    // shipped at `docs/efficiency/findings/` and verify the three
    // seed entries are detected. Located via `CARGO_MANIFEST_DIR`
    // (the `xtask/` package), then `../docs/efficiency/findings/`.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let catalog = manifest.join("../docs/efficiency/findings");
    if !catalog.exists() {
        // If the test is invoked from a non-monorepo checkout (e.g.
        // someone vendored just the xtask crate), skip rather than
        // fail.
        return;
    }
    let rows = collect_rows(&catalog).expect("collect seed rows");
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"EFF-2026-001"), "ids: {ids:?}");
    assert!(ids.contains(&"EFF-2026-002"), "ids: {ids:?}");
    assert!(ids.contains(&"EFF-2026-003"), "ids: {ids:?}");
    // Sanity check: every seed row uses `human-authored` per the
    // convention documented in the catalog README.
    for row in rows
        .iter()
        .filter(|r| r.id == "EFF-2026-001" || r.id == "EFF-2026-002" || r.id == "EFF-2026-003")
    {
        assert_eq!(
            row.discovered_by, "human-authored",
            "seed cohort uses human-authored provenance"
        );
    }
}

#[test]
fn promote_then_list_picks_up_promoted_finding() {
    let tmp = TempDir::new().unwrap();
    let (catalog, auto) = skeleton(&tmp);

    let draft = auto.join("draft.md");
    fs::write(&draft, PENDING_DRAFT).unwrap();
    promote_draft(&draft, &catalog).expect("promote ok");

    let rows = collect_rows(&catalog).expect("collect rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "EFF-2026-001");
    assert_eq!(
        rows[0].discovered_by, "DuplicateRequestAnalyzer",
        "promotion preserves analyzer-id provenance"
    );
    // Body of the promoted file is still the canonical shape.
    let body = fs::read_to_string(catalog.join("EFF-2026-001.md")).unwrap();
    assert!(body.starts_with("---\n"));
    assert!(body.contains("id: EFF-2026-001"));
}

#[test]
fn args_type_is_exposed_via_lib() {
    // Compile-only smoke: confirm the `findings::Args` re-export is
    // reachable from a downstream caller (this is the shape `main.rs`
    // uses).
    let _: Option<findings::Args> = None;
}

#[test]
fn catalog_row_type_round_trips_through_collect() {
    let tmp = TempDir::new().unwrap();
    let (catalog, _auto) = skeleton(&tmp);
    fs::write(
        catalog.join("EFF-2026-042.md"),
        catalog_entry("EFF-2026-042", "Forty-two", "critical", "proven"),
    )
    .unwrap();
    let rows = collect_rows(&catalog).unwrap();
    assert_eq!(
        rows[0],
        CatalogRow {
            id: "EFF-2026-042".to_string(),
            title: "Forty-two".to_string(),
            severity: "critical".to_string(),
            category: "redundant_metadata_fetch".to_string(),
            status: "proven".to_string(),
            discovered_by: "human-authored".to_string(),
        }
    );
}
