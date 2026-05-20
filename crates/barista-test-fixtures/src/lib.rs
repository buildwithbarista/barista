// SPDX-License-Identifier: MIT OR Apache-2.0

// This crate is a test-only helper library. The workspace security lints
// (clippy::unwrap_used, clippy::expect_used, clippy::panic) are explicitly
// allowed because panic-on-misuse is the documented contract for fixture
// loaders — a malformed test fixture is a developer error that should
// loudly fail the test run.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Shared test fixtures and corpus indexing helpers.
//!
//! Exposes two corpora:
//!
//! - **Version-comparison cases** at `data/version-cases.toml`,
//!   used to validate the `barista-version` crate's ordering
//!   implementation against Apache Maven's `ComparableVersion`
//!   semantics.
//! - **Corpus pointer index** at `data/corpus-100.toml`, listing
//!   the real-world Maven projects materialized under `test-corpus/`
//!   for use by resolver, POM-parser, and end-to-end tests.
//!
//! # Example
//!
//! ```
//! use barista_test_fixtures::{load_version_cases, Expected};
//!
//! let cases = load_version_cases();
//! assert!(!cases.is_empty());
//! for case in &cases {
//!     match case.expected {
//!         Expected::Lt | Expected::Eq | Expected::Gt => {}
//!     }
//! }
//! ```

use serde::Deserialize;

/// Expected ordering between two version strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Expected {
    /// `left < right`
    Lt,
    /// `left == right` (canonical equality, not raw string equality)
    Eq,
    /// `left > right`
    Gt,
}

/// A single version-comparison case parsed from the fixture corpus.
#[derive(Debug, Clone, Deserialize)]
pub struct VersionCase {
    pub left: String,
    pub right: String,
    pub expected: Expected,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VersionCorpus {
    #[serde(rename = "case")]
    cases: Vec<VersionCase>,
}

/// Raw TOML source of the version-comparison corpus, embedded at
/// compile time so consumers do not need a runtime working directory.
pub const VERSION_CASES_TOML: &str = include_str!("../data/version-cases.toml");

/// Parse and return every case in the version-comparison corpus.
///
/// Panics if the embedded TOML is malformed. The fixture is checked
/// into the crate, so a panic here indicates a corrupted source tree
/// and is not a runtime failure mode worth recovering from.
pub fn load_version_cases() -> Vec<VersionCase> {
    let corpus: VersionCorpus =
        toml::from_str(VERSION_CASES_TOML).expect("version-cases.toml is malformed");
    corpus.cases
}

/// One entry in the corpus pointer index. See `data/corpus-100.toml`.
///
/// `relative_path` and `pom_relative_path` are relative to the
/// **monorepo root** — the directory containing the workspace
/// `Cargo.toml` — not to this crate. Consumers resolve them by
/// canonicalizing against an env var (e.g. `CARGO_MANIFEST_DIR`
/// walked up to the workspace root) or another known project-root
/// anchor.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CorpusEntry {
    /// Stable identifier; matches the directory name under
    /// `test-corpus/`.
    pub id: String,
    /// Human-readable one-line description of the project.
    pub description: String,
    /// Relative path to the materialized checkout directory.
    pub relative_path: String,
    /// Relative path to the root POM (the parent POM for
    /// multi-module projects).
    pub pom_relative_path: String,
}

#[derive(Debug, Deserialize)]
struct CorpusIndexFile {
    #[serde(rename = "entry")]
    entries: Vec<CorpusEntry>,
}

/// Raw TOML source of the corpus pointer index, embedded at compile
/// time so consumers do not need a runtime working directory.
pub const CORPUS_INDEX_TOML: &str = include_str!("../data/corpus-100.toml");

/// Load the corpus pointer index.
///
/// Returns the list of configured corpus projects (currently a small
/// seed set, growing toward ~100). The returned paths are relative
/// to the monorepo root — see [`CorpusEntry`].
///
/// Panics if the embedded TOML is malformed. The fixture is checked
/// into the crate, so a panic indicates a corrupted source tree.
pub fn load_corpus_index() -> Vec<CorpusEntry> {
    let parsed: CorpusIndexFile =
        toml::from_str(CORPUS_INDEX_TOML).expect("corpus-100.toml must be valid TOML");
    parsed.entries
}

// ---------------------------------------------------------------------------
// Strict-resolver conflict fixtures.
//
// Each fixture under `data/strict-conflicts/` is a self-contained TOML
// description of a synthetic dependency graph plus its expected
// resolution outcome. See `data/strict-conflicts/README.md` for the
// format.
// ---------------------------------------------------------------------------

/// Expected outcome of resolving a strict-conflict fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum ExpectedOutcome {
    /// The graph resolves cleanly; `expected_versions` is authoritative.
    Resolved,
    /// The graph cannot resolve; `expected_edges` names the edges the
    /// derivation tree must surface.
    Conflict,
}

/// One edge the resolver's derivation tree must surface when a fixture
/// is expected to fail.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ExpectedEdge {
    /// `groupId:artifactId:version` of the parent node.
    pub from: String,
    /// `groupId:artifactId` of the required dependency.
    pub to: String,
    /// Maven `VersionSpec` string the parent declared.
    pub range: String,
}

/// One dependency declared by a `FixtureNode`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct FixtureDep {
    /// `groupId:artifactId` (optionally extended with `:packaging:classifier`).
    pub coords: String,
    /// Maven `VersionSpec` — soft (`1.0`) or hard (`[1.0]`, `[1.0,2.0)`).
    pub version: String,
    /// Maven scope: `compile`, `runtime`, `test`, `provided`, `system`,
    /// or `import` for BOM imports. Defaults to compile when omitted.
    #[serde(default)]
    pub scope: Option<String>,
    /// True if this dependency is marked optional. Optional transitives
    /// are not inherited by downstream consumers per Maven semantics.
    #[serde(default)]
    pub optional: bool,
}

/// One `(coords, version)` available in the synthetic registry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct FixtureNode {
    /// `groupId:artifactId` (optionally extended with `:packaging:classifier`).
    pub coords: String,
    /// Concrete version string; never a range.
    pub version: String,
    /// The node's declared `<dependencies>` block.
    #[serde(default)]
    pub dependencies: Vec<FixtureDep>,
}

/// A complete strict-conflict fixture parsed from one TOML file under
/// `data/strict-conflicts/`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct StrictConflictFixture {
    /// Stable identifier; equals the source filename without extension.
    pub id: String,
    /// One-sentence human-readable summary.
    pub description: String,
    /// Whether the resolver is expected to succeed or report a conflict.
    pub expected_outcome: ExpectedOutcome,
    /// Edges the derivation tree must surface (only for Conflict outcomes).
    #[serde(default)]
    pub expected_edges: Vec<ExpectedEdge>,
    /// Resolved versions per `g:a` (only for Resolved outcomes).
    #[serde(default)]
    pub expected_versions: std::collections::BTreeMap<String, String>,
    /// The synthetic registry: one entry per `(coords, version)`.
    #[serde(default, rename = "node")]
    pub nodes: Vec<FixtureNode>,
}

/// Compile-time list of every fixture under `data/strict-conflicts/`.
///
/// Each tuple is `(expected_id, raw_toml)`. The expected_id is the
/// fixture's filename stem; we cross-check that the parsed `id` field
/// matches at load time so a file rename without a matching content
/// change does not silently corrupt the corpus.
const STRICT_CONFLICT_FIXTURES: &[(&str, &str)] = &[
    (
        "01-clean-no-conflict",
        include_str!("../data/strict-conflicts/01-clean-no-conflict.toml"),
    ),
    (
        "02-hard-range-satisfied",
        include_str!("../data/strict-conflicts/02-hard-range-satisfied.toml"),
    ),
    (
        "03-hard-range-no-version",
        include_str!("../data/strict-conflicts/03-hard-range-no-version.toml"),
    ),
    (
        "04-diamond-hard-conflict",
        include_str!("../data/strict-conflicts/04-diamond-hard-conflict.toml"),
    ),
    (
        "05-diamond-soft-resolves",
        include_str!("../data/strict-conflicts/05-diamond-soft-resolves.toml"),
    ),
    (
        "06-three-way-conflict",
        include_str!("../data/strict-conflicts/06-three-way-conflict.toml"),
    ),
    (
        "07-cycle",
        include_str!("../data/strict-conflicts/07-cycle.toml"),
    ),
    (
        "08-version-range-narrowing",
        include_str!("../data/strict-conflicts/08-version-range-narrowing.toml"),
    ),
    (
        "09-excluded-version-via-multiple-ranges",
        include_str!("../data/strict-conflicts/09-excluded-version-via-multiple-ranges.toml"),
    ),
    (
        "10-snapshot-vs-release",
        include_str!("../data/strict-conflicts/10-snapshot-vs-release.toml"),
    ),
    (
        "11-deep-transitive-conflict",
        include_str!("../data/strict-conflicts/11-deep-transitive-conflict.toml"),
    ),
    (
        "12-classifier-distinct-no-conflict",
        include_str!("../data/strict-conflicts/12-classifier-distinct-no-conflict.toml"),
    ),
    (
        "13-empty-root",
        include_str!("../data/strict-conflicts/13-empty-root.toml"),
    ),
    (
        "14-bom-import-narrowing",
        include_str!("../data/strict-conflicts/14-bom-import-narrowing.toml"),
    ),
    (
        "15-optional-pulls-conflict",
        include_str!("../data/strict-conflicts/15-optional-pulls-conflict.toml"),
    ),
];

/// Load every strict-conflict fixture, sorted by `id` for stable
/// iteration. Panics on malformed TOML or an `id` mismatch — these
/// fixtures are checked into the crate, so any failure here indicates
/// a corrupted source tree, not a runtime failure mode.
pub fn load_strict_conflict_fixtures() -> Vec<StrictConflictFixture> {
    let mut out: Vec<StrictConflictFixture> = STRICT_CONFLICT_FIXTURES
        .iter()
        .map(|(expected_id, raw)| {
            let fixture: StrictConflictFixture = toml::from_str(raw).unwrap_or_else(|e| {
                panic!("strict-conflict fixture {expected_id}.toml is malformed: {e}")
            });
            assert_eq!(
                &fixture.id, expected_id,
                "fixture id field ({}) must match filename stem ({})",
                fixture.id, expected_id,
            );
            fixture
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

#[cfg(test)]
mod strict_conflict_tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn loads_at_least_fifteen_fixtures() {
        let fixtures = load_strict_conflict_fixtures();
        assert!(
            fixtures.len() >= 15,
            "expected at least 15 strict-conflict fixtures, got {}",
            fixtures.len()
        );
    }

    #[test]
    fn every_fixture_has_unique_id() {
        let fixtures = load_strict_conflict_fixtures();
        let mut seen: HashSet<String> = HashSet::new();
        for f in &fixtures {
            assert!(seen.insert(f.id.clone()), "duplicate fixture id: {}", f.id);
        }
    }

    #[test]
    fn every_fixture_has_valid_outcome() {
        // The enum is closed, so this test mainly pins the contract:
        // every fixture parses into one of the two variants.
        for f in load_strict_conflict_fixtures() {
            match f.expected_outcome {
                ExpectedOutcome::Resolved | ExpectedOutcome::Conflict => {}
            }
        }
    }

    #[test]
    fn conflict_fixtures_have_expected_edges() {
        for f in load_strict_conflict_fixtures() {
            if f.expected_outcome == ExpectedOutcome::Conflict {
                assert!(
                    !f.expected_edges.is_empty(),
                    "conflict fixture {} must declare at least one expected_edge",
                    f.id,
                );
                for edge in &f.expected_edges {
                    assert!(!edge.from.is_empty(), "edge.from empty in {}", f.id);
                    assert!(!edge.to.is_empty(), "edge.to empty in {}", f.id);
                    assert!(!edge.range.is_empty(), "edge.range empty in {}", f.id);
                }
            }
        }
    }

    #[test]
    fn resolved_fixtures_have_expected_versions() {
        for f in load_strict_conflict_fixtures() {
            if f.expected_outcome == ExpectedOutcome::Resolved {
                assert!(
                    !f.expected_versions.is_empty(),
                    "resolved fixture {} must declare at least one expected_version",
                    f.id,
                );
            }
        }
    }

    #[test]
    fn fixtures_are_internally_consistent() {
        // Every fixture must have at least one node, at least one
        // root node (coords ending in `:root`), and every dependency
        // it declares must reference some node present in the
        // fixture's synthetic registry. This catches typos like
        // `org.exmaple:lib`.
        for f in load_strict_conflict_fixtures() {
            assert!(!f.nodes.is_empty(), "fixture {} has no nodes", f.id);

            let known_coords: HashSet<String> = f.nodes.iter().map(|n| n.coords.clone()).collect();

            let has_root = f.nodes.iter().any(|n| n.coords.ends_with(":root"));
            assert!(has_root, "fixture {} has no node ending in :root", f.id,);

            for node in &f.nodes {
                for dep in &node.dependencies {
                    assert!(
                        known_coords.contains(&dep.coords),
                        "fixture {}: node {} depends on {} which is not in the registry",
                        f.id,
                        node.coords,
                        dep.coords,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_parses() {
        let cases = load_version_cases();
        assert!(
            cases.len() >= 90,
            "fixture corpus must contain at least 90 cases, got {}",
            cases.len()
        );
    }

    #[test]
    fn every_case_is_structurally_valid() {
        // Empty strings ARE valid version inputs (Maven accepts them);
        // what we check here is that no case is structurally broken.
        // Serde would refuse to deserialize a missing field — this
        // test just pins that contract.
        let cases = load_version_cases();
        for c in cases {
            let _ = (c.left, c.right, c.expected);
        }
    }

    #[test]
    fn corpus_index_parses() {
        let entries = load_corpus_index();
        assert!(
            entries.len() >= 5,
            "corpus index must contain at least 5 entries, got {}",
            entries.len()
        );
    }

    #[test]
    fn corpus_index_entries_are_structurally_valid() {
        for entry in load_corpus_index() {
            assert!(!entry.id.is_empty(), "corpus entry id must be non-empty");
            assert!(
                !entry.description.is_empty(),
                "corpus entry description must be non-empty (id = {})",
                entry.id,
            );
            assert!(
                entry.pom_relative_path.ends_with("pom.xml"),
                "pom_relative_path must end with pom.xml (id = {}, got {})",
                entry.id,
                entry.pom_relative_path,
            );
            // The relative_path is expected to be of the form
            // `test-corpus/<id>/checkout`, so the second segment
            // must match the entry's id.
            let segments: Vec<&str> = entry.relative_path.split('/').collect();
            assert!(
                segments.len() >= 2,
                "relative_path must have at least two segments (id = {}, got {})",
                entry.id,
                entry.relative_path,
            );
            assert_eq!(
                segments[1], entry.id,
                "second segment of relative_path must match id ({} vs {})",
                segments[1], entry.id,
            );
        }
    }
}
