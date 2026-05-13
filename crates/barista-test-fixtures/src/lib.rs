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
pub const VERSION_CASES_TOML: &str =
    include_str!("../data/version-cases.toml");

/// Parse and return every case in the version-comparison corpus.
///
/// Panics if the embedded TOML is malformed. The fixture is checked
/// into the crate, so a panic here indicates a corrupted source tree
/// and is not a runtime failure mode worth recovering from.
pub fn load_version_cases() -> Vec<VersionCase> {
    let corpus: VersionCorpus = toml::from_str(VERSION_CASES_TOML)
        .expect("version-cases.toml is malformed");
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
pub const CORPUS_INDEX_TOML: &str =
    include_str!("../data/corpus-100.toml");

/// Load the corpus pointer index.
///
/// Returns the list of configured corpus projects (currently a small
/// seed set, growing toward ~100). The returned paths are relative
/// to the monorepo root — see [`CorpusEntry`].
///
/// Panics if the embedded TOML is malformed. The fixture is checked
/// into the crate, so a panic indicates a corrupted source tree.
pub fn load_corpus_index() -> Vec<CorpusEntry> {
    let parsed: CorpusIndexFile = toml::from_str(CORPUS_INDEX_TOML)
        .expect("corpus-100.toml must be valid TOML");
    parsed.entries
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
            assert!(
                !entry.id.is_empty(),
                "corpus entry id must be non-empty"
            );
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
            let segments: Vec<&str> =
                entry.relative_path.split('/').collect();
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
