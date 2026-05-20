// SPDX-License-Identifier: MIT OR Apache-2.0

// Integration-test / example / benchmark target — workspace security
// lints are allowed here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`)
// is the documented contract for failing a test loudly. This allow block
// keeps the crate root's `#![allow(...)]` from being silently dropped by
// the separate compilation unit each test file forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Golden-tree integration test.
//!
//! For each configured fixture root, run the walker via
//! [`FixtureMetadataSource`] and compare the resulting graph against a
//! checked-in `mvn dependency:tree -DoutputType=tgf` ground-truth file
//! at `tests/fixtures/<g>/<a>/<v>/expected.tgf`.
//!
//! This is the canonical "does the walker match Maven" gate, paralleling
//! the effective-POM golden gate in `barista-pom`. With the current
//! 5-fixture corpus the gate covers two graph shapes:
//!
//! 1. **Leaf roots** (commons-lang3, commons-io, jackson-core, slf4j-api):
//!    the root resolves with no compile-scope transitives. These prove
//!    the walker correctly walks a direct edge without spuriously
//!    surfacing test-scope or unresolvable transitives.
//! 2. **Real transitive depth** (jackson-databind): a two-level graph
//!    where the root pulls jackson-annotations + jackson-core. This is
//!    the meaningful case — the walker must agree with Maven on which
//!    compile-scope nodes appear in the resolved set.
//!
//! ## Ground-truth format
//!
//! TGF (Trivial Graph Format) is the lightweight node + edge format
//! Maven's `dependency:tree -DoutputType=tgf` emits. The .tgf files
//! here are hand-authored to match `mvn dependency:tree -Dscope=compile
//! -DoutputType=tgf` on the upstream artifact — see each file's header
//! for any deliberate trimming. As the corpus grows, a future
//! `regen-golden-tgf.sh` helper can regenerate these from a pinned
//! mvn invocation against the materialized corpus.
//!
//! ## Format we compare on
//!
//! Each node line is `<id> <group>:<artifact>:<version>:<scope>`. The
//! type/packaging field that real `mvn dependency:tree -DoutputType=tgf`
//! emits (e.g. `:jar` between version and scope) is intentionally
//! omitted: the v0.1 gate compares the *resolved coord set*, not jar
//! packaging. Edges (after `#`) are informative but currently unused.
//!
//! The test must work offline — no network, no mvn invocation at test
//! time.

mod common;

use std::collections::BTreeSet;

use barista_coords::Coords;
use barista_pom::{EffectivePom, Properties, RawDependency, RawPom, ResolvedPom};
use barista_resolver::source::MetadataSource;
use barista_resolver::walker::{WalkOptions, walk};

use common::fixture_source::FixtureMetadataSource;

// ---------------------------------------------------------------------------
// TGF parser
// ---------------------------------------------------------------------------

/// A parsed TGF document. Node ids map to their `group:artifact:version:scope`
/// label.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Tgf {
    /// Node labels in declaration order. The label is the canonical
    /// `group:artifact:version:scope` string the walker emits.
    nodes: Vec<String>,
}

/// Parse a TGF document.
///
/// TGF separates nodes from edges with a bare `#` line, but for the
/// goldens here we also want to use `#`-prefixed lines as comments
/// (including bare `#` lines as visual section breaks). To make those
/// two conventions coexist, the parser identifies lines structurally
/// rather than relying on the `#` separator:
///
/// * Lines starting with `#` (including a bare `#`) are comments.
/// * Lines whose first two whitespace-separated tokens are both numeric
///   are edges (e.g. `1 2 compile`) and are ignored.
/// * Lines whose first token is numeric and second token is non-numeric
///   are nodes; the rest of the line (after the id) is the node label.
/// * Everything else is silently skipped.
fn parse_tgf(content: &str) -> Tgf {
    let mut nodes = Vec::new();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (first, rest) = match line.split_once(char::is_whitespace) {
            Some(t) => t,
            None => continue, // single token — malformed for both node + edge
        };
        if first.parse::<u64>().is_err() {
            continue; // first token must be a numeric id
        }
        let rest = rest.trim();
        if rest.is_empty() {
            continue;
        }
        // Edge lines: second token is also numeric.
        let second_token = rest.split_whitespace().next().unwrap_or("");
        if second_token.parse::<u64>().is_ok() {
            continue;
        }
        nodes.push(rest.to_string());
    }
    Tgf { nodes }
}

// ---------------------------------------------------------------------------
// Walker driver
// ---------------------------------------------------------------------------

/// Wrap a raw POM as a `ResolvedPom` without invoking parent-chain
/// resolution. The fixture POMs used as walker roots in this test are
/// hand-trimmed to have no `<parent>` element, so manual wrapping is
/// correctness-equivalent to calling `resolve_pom` with a real parent
/// resolver — and it keeps the test self-contained (no need to seed
/// every parent POM in the corpus).
fn wrap_resolved(p: RawPom) -> ResolvedPom {
    ResolvedPom {
        effective: EffectivePom {
            pom: p.clone(),
            interpolations: Vec::new(),
            parent_chain: Vec::new(),
        },
        pom: p,
        active_profile_ids: Vec::new(),
        imported_boms: Vec::new(),
    }
}

fn dep(g: &str, a: &str, v: &str) -> RawDependency {
    RawDependency {
        group_id: g.into(),
        artifact_id: a.into(),
        version: Some(v.into()),
        ..RawDependency::default()
    }
}

/// Build a synthetic root POM whose single direct dep is the fixture
/// under test. The walker then fetches the fixture's POM and expands
/// from there. This mirrors the `walker_skipper::fixture_corpus_*`
/// pattern and avoids requiring each fixture's `<parent>` chain to be
/// seeded in the corpus.
fn synthetic_root(target: &Coords, version: &str) -> RawPom {
    RawPom {
        model_version: "4.0.0".into(),
        group_id: Some("barista.test".into()),
        artifact_id: "synthetic-root".into(),
        version: Some("0.0.0".into()),
        packaging: "jar".into(),
        dependencies: vec![dep(&target.group, &target.artifact, version)],
        properties: Properties::default(),
        ..RawPom::default()
    }
}

/// Walk the fixture root and return the resolved coord set as a sorted
/// vec of `group:artifact:version:scope` strings — the same canonical
/// form the .tgf goldens use.
async fn walk_fixture(
    target: &Coords,
    version: &str,
    src: &FixtureMetadataSource,
) -> Result<Vec<String>, String> {
    // Sanity check: the fixture must be loadable.
    src.fetch_pom(target, version)
        .await
        .map_err(|e| format!("fixture fetch_pom failed for {target}:{version}: {e}"))?;

    let root = synthetic_root(target, version);
    let opts = WalkOptions::default();
    let graph = walk(&wrap_resolved(root), src, &opts)
        .await
        .map_err(|e| format!("walk failed: {e}"))?;

    let mut out: Vec<String> = graph
        .resolved
        .iter()
        .map(|d| {
            format!(
                "{}:{}:{}:{}",
                d.coords.group,
                d.coords.artifact,
                d.version,
                scope_str(d.scope)
            )
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

fn scope_str(s: barista_resolver::walker::Scope) -> &'static str {
    use barista_resolver::walker::Scope::*;
    match s {
        Compile => "compile",
        Provided => "provided",
        Runtime => "runtime",
        Test => "test",
        System => "system",
        Import => "import",
    }
}

// ---------------------------------------------------------------------------
// The golden test
// ---------------------------------------------------------------------------

/// One configured golden case.
struct GoldenCase {
    name: &'static str,
    group: &'static str,
    artifact: &'static str,
    version: &'static str,
}

const CASES: &[GoldenCase] = &[
    GoldenCase {
        name: "commons-lang3",
        group: "org.apache.commons",
        artifact: "commons-lang3",
        version: "3.14.0",
    },
    GoldenCase {
        name: "commons-io",
        group: "commons-io",
        artifact: "commons-io",
        version: "2.16.1",
    },
    GoldenCase {
        name: "jackson-core",
        group: "com.fasterxml.jackson.core",
        artifact: "jackson-core",
        version: "2.18.0",
    },
    GoldenCase {
        name: "jackson-databind",
        group: "com.fasterxml.jackson.core",
        artifact: "jackson-databind",
        version: "2.18.0",
    },
    GoldenCase {
        name: "slf4j-api",
        group: "org.slf4j",
        artifact: "slf4j-api",
        version: "2.0.16",
    },
];

fn tgf_path(c: &GoldenCase) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(c.group)
        .join(c.artifact)
        .join(c.version)
        .join("expected.tgf")
}

#[tokio::test]
async fn golden_dependency_tree() {
    let src = FixtureMetadataSource::load_default().expect("fixtures must load");

    let mut hard_failures: Vec<String> = Vec::new();
    let mut passed: Vec<&'static str> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for case in CASES {
        let coords =
            Coords::new(case.group, case.artifact).expect("hard-coded fixture coords parse");

        let walked = match walk_fixture(&coords, case.version, &src).await {
            Ok(ws) => ws,
            Err(e) => {
                hard_failures.push(format!("{}: walker error: {e}", case.name));
                continue;
            }
        };

        let path = tgf_path(case);
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => {
                skipped.push(format!(
                    "{}: no ground-truth .tgf at {}",
                    case.name,
                    path.display()
                ));
                continue;
            }
        };
        let expected = parse_tgf(&raw);

        let walked_set: BTreeSet<String> = walked.into_iter().collect();
        let expected_set: BTreeSet<String> = expected.nodes.into_iter().collect();

        if walked_set != expected_set {
            let missing: Vec<&String> = expected_set.difference(&walked_set).collect();
            let extra: Vec<&String> = walked_set.difference(&expected_set).collect();
            hard_failures.push(format!(
                "{}: walker output != mvn dependency:tree ground truth\n  \
                 missing from walker (expected by mvn): {:?}\n  \
                 extra in walker (not in mvn):          {:?}\n  \
                 walker set:   {:?}\n  \
                 expected set: {:?}",
                case.name, missing, extra, walked_set, expected_set,
            ));
        } else {
            passed.push(case.name);
            eprintln!(
                "{}: PASS ({} node{})",
                case.name,
                walked_set.len(),
                if walked_set.len() == 1 { "" } else { "s" }
            );
        }
    }

    for s in &skipped {
        eprintln!("--- SKIP: {s}");
    }

    if !hard_failures.is_empty() {
        for f in &hard_failures {
            eprintln!("--- FAILURE: {f}");
        }
        panic!(
            "{} project(s) failed the golden-tree gate ({} passed, {} skipped)",
            hard_failures.len(),
            passed.len(),
            skipped.len()
        );
    }

    // Acceptance gate: at least three of the configured leaf cases plus
    // the jackson-databind transitive case must pass — otherwise the
    // corpus is too thin to call this a meaningful gate.
    assert!(
        passed.len() >= 3,
        "golden-tree gate is too thin: only {} case(s) passed",
        passed.len()
    );
    assert!(
        passed.contains(&"jackson-databind"),
        "jackson-databind transitive case must pass — it's the one with real graph depth"
    );
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

/// Walker returns an empty resolved set when the synthetic root depends
/// on a coord with no .tgf node listed beyond itself — i.e. a leaf.
#[tokio::test]
async fn walk_fixture_leaf_root_returns_only_self() {
    let src = FixtureMetadataSource::load_default().unwrap();
    let coords = Coords::new("commons-io", "commons-io").unwrap();
    let got = walk_fixture(&coords, "2.16.1", &src).await.unwrap();
    assert_eq!(
        got,
        vec!["commons-io:commons-io:2.16.1:compile".to_string()]
    );
}

/// Walker reports a clear error when the requested fixture isn't in
/// the corpus.
#[tokio::test]
async fn walk_fixture_missing_root_errors() {
    let src = FixtureMetadataSource::load_default().unwrap();
    let coords = Coords::new("org.bogus", "nonexistent").unwrap();
    let err = walk_fixture(&coords, "9.9.9", &src).await.unwrap_err();
    assert!(
        err.contains("fixture fetch_pom failed") || err.contains("walk failed"),
        "expected a clear error mentioning the missing fixture, got: {err}"
    );
}

/// jackson-databind's golden has exactly 3 nodes (root + 2 transitives)
/// — guard against accidental corpus shrink that would silently turn
/// the meaningful case back into a leaf-only smoke test.
#[test]
fn jackson_databind_golden_has_three_nodes() {
    let path = tgf_path(&CASES[3]); // jackson-databind
    assert_eq!(CASES[3].name, "jackson-databind", "CASES order changed");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let parsed = parse_tgf(&raw);
    assert_eq!(
        parsed.nodes.len(),
        3,
        "jackson-databind .tgf should have 3 nodes, got: {:?}",
        parsed.nodes
    );
}

#[cfg(test)]
mod tgf_parser_tests {
    use super::*;

    #[test]
    fn parses_minimal_one_node() {
        let s = "1 a:b:1.0:compile\n#\n";
        let t = parse_tgf(s);
        assert_eq!(t.nodes, vec!["a:b:1.0:compile"]);
    }

    #[test]
    fn parses_multi_node_with_edges() {
        let s = "\
1 a:b:1.0:compile
2 c:d:2.0:compile
3 e:f:3.0:compile
#
1 2 compile
1 3 compile
";
        let t = parse_tgf(s);
        assert_eq!(
            t.nodes,
            vec![
                "a:b:1.0:compile".to_string(),
                "c:d:2.0:compile".to_string(),
                "e:f:3.0:compile".to_string(),
            ]
        );
    }

    #[test]
    fn ignores_comment_lines_before_separator() {
        let s = "\
# this is a comment
# another comment
1 a:b:1.0:compile
#
";
        let t = parse_tgf(s);
        assert_eq!(t.nodes, vec!["a:b:1.0:compile"]);
    }

    #[test]
    fn ignores_blank_lines() {
        let s = "\n1 a:b:1.0:compile\n\n2 c:d:2.0:compile\n\n#\n\n1 2\n\n";
        let t = parse_tgf(s);
        assert_eq!(t.nodes.len(), 2);
    }

    #[test]
    fn ignores_edges_after_separator() {
        let s = "1 a:b:1.0:compile\n#\n1 2 compile\n2 3 compile\n";
        let t = parse_tgf(s);
        // Only the one node line before `#` is captured.
        assert_eq!(t.nodes, vec!["a:b:1.0:compile"]);
    }

    #[test]
    fn empty_input_yields_empty() {
        assert_eq!(parse_tgf(""), Tgf::default());
    }

    #[test]
    fn malformed_node_line_skipped() {
        // No whitespace between id and label → can't split → skipped.
        let s = "1\n2 ok:lib:1.0:compile\n#\n";
        let t = parse_tgf(s);
        assert_eq!(t.nodes, vec!["ok:lib:1.0:compile"]);
    }

    #[test]
    fn comments_after_separator_also_ignored() {
        let s = "\
1 a:b:1.0:compile
#
# edges follow
1 2 compile
";
        let t = parse_tgf(s);
        assert_eq!(t.nodes, vec!["a:b:1.0:compile"]);
    }
}
