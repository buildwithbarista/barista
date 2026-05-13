//! Shared test fixtures and corpus indexing helpers.
//!
//! Currently exposes one corpus: the version-comparison cases at
//! `data/version-cases.toml`, used to validate the `barista-version`
//! crate's ordering implementation against Apache Maven's
//! `ComparableVersion` semantics.
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
}
