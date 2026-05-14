//! Fixture-driven integration test for `Version` ordering semantics.
//!
//! Loads the version-comparison corpus from `barista-test-fixtures`
//! (193+ cases ported from Apache Maven's `ComparableVersionTest` plus
//! additional edge cases) and asserts every case against the
//! `Version` port's `Ord` implementation.
//!
//! Cases whose `notes` contain the string
//! `"tentative — verify against runtime mvn behavior"` are still
//! evaluated, but their mismatches are reported as warnings rather
//! than test failures — they exist to be reconciled once a real Maven
//! runtime can be queried for ground truth.

use std::cmp::Ordering;

use barista_test_fixtures::{Expected, load_version_cases};
use barista_version::Version;

const TENTATIVE_MARKER: &str = "tentative — verify against runtime mvn behavior";

fn ordering_for(e: Expected) -> Ordering {
    match e {
        Expected::Lt => Ordering::Less,
        Expected::Eq => Ordering::Equal,
        Expected::Gt => Ordering::Greater,
    }
}

#[test]
fn version_cases_corpus() {
    let cases = load_version_cases();
    assert!(
        cases.len() >= 100,
        "corpus shrank — expected at least 100 cases, got {}",
        cases.len(),
    );

    let mut hard_failures: Vec<String> = Vec::new();
    let mut tentative_failures: Vec<String> = Vec::new();

    for (i, case) in cases.iter().enumerate() {
        let left: Version = match case.left.parse() {
            Ok(v) => v,
            Err(e) => {
                let msg = format!(
                    "case {} ({:?} vs {:?}): failed to parse left: {:?}",
                    i, case.left, case.right, e,
                );
                hard_failures.push(msg);
                continue;
            }
        };
        let right: Version = match case.right.parse() {
            Ok(v) => v,
            Err(e) => {
                let msg = format!(
                    "case {} ({:?} vs {:?}): failed to parse right: {:?}",
                    i, case.left, case.right, e,
                );
                hard_failures.push(msg);
                continue;
            }
        };

        let got = left.cmp(&right);
        let want = ordering_for(case.expected);

        if got != want {
            let notes_suffix = case
                .notes
                .as_ref()
                .map(|n| format!(" — notes: {n:?}"))
                .unwrap_or_default();
            let msg = format!(
                "case {} ({:?} vs {:?}): want {:?}, got {:?}{}",
                i, case.left, case.right, want, got, notes_suffix,
            );
            let is_tentative = case
                .notes
                .as_deref()
                .unwrap_or("")
                .contains(TENTATIVE_MARKER);
            if is_tentative {
                tentative_failures.push(msg);
            } else {
                hard_failures.push(msg);
            }
        }
    }

    if !tentative_failures.is_empty() {
        eprintln!(
            "--- {} tentative case mismatch(es) (warning, not fatal):",
            tentative_failures.len(),
        );
        for m in &tentative_failures {
            eprintln!("  {m}");
        }
    }

    if !hard_failures.is_empty() {
        eprintln!("--- {} hard failure(s):", hard_failures.len());
        for m in &hard_failures {
            eprintln!("  {m}");
        }
        panic!(
            "{} non-tentative case(s) failed; see eprintln output above",
            hard_failures.len(),
        );
    }
}
