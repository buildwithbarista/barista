//! Integration tests proving the BFS+Skipper invariant:
//!
//! For any walkable graph, `walk(enable_skipper = true)` produces the
//! same `winners` map as `walk(enable_skipper = false)`. Skipper-related
//! pruning is correctness-safe — it changes performance, not the final
//! resolved graph.
//!
//! These tests also exercise the skipper's stats to demonstrate that
//! pruning actually fires on representative graphs (diamonds, repeated
//! leaves, deep linear chains).

mod common;

use std::collections::BTreeMap;

use barista_coords::Coords;
use barista_pom::{EffectivePom, Properties, RawDependency, RawExclusion, RawPom, ResolvedPom};
use barista_resolver::walker::{FixtureSource, ResolvedDep, ResolvedGraph, WalkOptions, walk};

use common::fixture_source::FixtureMetadataSource;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn co(g: &str, a: &str) -> Coords {
    Coords::new(g, a).unwrap()
}

fn dep(g: &str, a: &str, v: &str) -> RawDependency {
    RawDependency {
        group_id: g.into(),
        artifact_id: a.into(),
        version: Some(v.into()),
        ..RawDependency::default()
    }
}

fn dep_excl(g: &str, a: &str, v: &str, exclusions: &[(&str, &str)]) -> RawDependency {
    RawDependency {
        group_id: g.into(),
        artifact_id: a.into(),
        version: Some(v.into()),
        exclusions: exclusions
            .iter()
            .map(|(g, a)| RawExclusion {
                group_id: (*g).into(),
                artifact_id: (*a).into(),
            })
            .collect(),
        ..RawDependency::default()
    }
}

fn pom(g: &str, a: &str, v: &str, deps: Vec<RawDependency>) -> RawPom {
    RawPom {
        model_version: "4.0.0".into(),
        group_id: Some(g.into()),
        artifact_id: a.into(),
        version: Some(v.into()),
        packaging: "jar".into(),
        dependencies: deps,
        properties: Properties::default(),
        ..RawPom::default()
    }
}

fn resolved(p: RawPom) -> ResolvedPom {
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

/// Run the walker twice — once with the skipper enabled, once disabled
/// — and assert the resulting `winners` maps are identical (the core
/// correctness invariant).
async fn assert_skipper_equivalence(
    root: RawPom,
    src: &FixtureSource,
) -> (ResolvedGraph, ResolvedGraph) {
    let opts_on = WalkOptions {
        enable_skipper: true,
        ..WalkOptions::default()
    };
    let opts_off = WalkOptions {
        enable_skipper: false,
        ..WalkOptions::default()
    };
    let g_on = walk(&resolved(root.clone()), src, &opts_on)
        .await
        .expect("walk(skipper=on)");
    let g_off = walk(&resolved(root), src, &opts_off)
        .await
        .expect("walk(skipper=off)");

    assert_eq!(
        winners_summary(&g_on),
        winners_summary(&g_off),
        "skipper changed the winners map — correctness invariant violated"
    );
    (g_on, g_off)
}

/// Reduce winners to a comparable (coord -> version) map so the
/// equality assertion isn't sensitive to BFS discovery-order quirks.
fn winners_summary(g: &ResolvedGraph) -> BTreeMap<String, String> {
    g.winners
        .iter()
        .map(|(c, d): (&Coords, &ResolvedDep)| {
            (format!("{}:{}", c.group, c.artifact), d.version.clone())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Diamond graph with shared transitives — skipper should fire at least
/// once when the same coord is reached at deeper depth.
#[tokio::test]
async fn diamond_skipper_fires_and_preserves_graph() {
    // root -> A -> X (leaf)
    // root -> B -> X (leaf, reached at same depth → declaration tiebreak,
    //                but the leaf-cache still skips the second walk)
    let mut src = FixtureSource::new();
    src.add_pom(
        co("ex", "A"),
        "1.0",
        pom("ex", "A", "1.0", vec![dep("ex", "X", "1.0")]),
    );
    src.add_pom(
        co("ex", "B"),
        "1.0",
        pom("ex", "B", "1.0", vec![dep("ex", "X", "1.0")]),
    );
    src.add_pom(co("ex", "X"), "1.0", pom("ex", "X", "1.0", vec![]));

    let root = pom(
        "ex",
        "root",
        "1.0",
        vec![dep("ex", "A", "1.0"), dep("ex", "B", "1.0")],
    );
    let (g_on, _g_off) = assert_skipper_equivalence(root, &src).await;
    // X is winning at depth 2; either the leaf-cache or the
    // already-resolved branch should account for some skips.
    assert!(
        g_on.skipper_stats.total_skips() >= 1,
        "expected skipper to fire on diamond; stats = {:?}",
        g_on.skipper_stats
    );
}

/// Known-leaf cache fires when a leaf is visited multiple times via
/// different parents.
#[tokio::test]
async fn known_leaf_cache_fires_on_repeated_leaf() {
    // root -> A -> L (leaf)
    //      -> B -> L (leaf again — leaf cache must engage)
    //      -> C -> L (and again)
    let mut src = FixtureSource::new();
    src.add_pom(co("ex", "L"), "1.0", pom("ex", "L", "1.0", vec![]));
    for n in ["A", "B", "C"] {
        src.add_pom(
            co("ex", n),
            "1.0",
            pom("ex", n, "1.0", vec![dep("ex", "L", "1.0")]),
        );
    }
    let root = pom(
        "ex",
        "root",
        "1.0",
        vec![
            dep("ex", "A", "1.0"),
            dep("ex", "B", "1.0"),
            dep("ex", "C", "1.0"),
        ],
    );
    let (g_on, _g_off) = assert_skipper_equivalence(root, &src).await;
    assert!(
        g_on.skipper_stats.skips_known_leaf >= 1,
        "expected leaf cache to fire; stats = {:?}",
        g_on.skipper_stats
    );
}

/// Linear chain — no shared coords; skipper has nothing to skip but
/// must still produce the correct graph.
#[tokio::test]
async fn linear_chain_equivalence_no_skips_needed() {
    // root -> A -> B -> C -> D
    let mut src = FixtureSource::new();
    src.add_pom(
        co("ex", "A"),
        "1.0",
        pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]),
    );
    src.add_pom(
        co("ex", "B"),
        "1.0",
        pom("ex", "B", "1.0", vec![dep("ex", "C", "1.0")]),
    );
    src.add_pom(
        co("ex", "C"),
        "1.0",
        pom("ex", "C", "1.0", vec![dep("ex", "D", "1.0")]),
    );
    src.add_pom(co("ex", "D"), "1.0", pom("ex", "D", "1.0", vec![]));
    let root = pom("ex", "root", "1.0", vec![dep("ex", "A", "1.0")]);
    let (g_on, _g_off) = assert_skipper_equivalence(root, &src).await;
    assert_eq!(g_on.winners.len(), 4);
}

/// Cycle graph — skipper must terminate and produce the same set of
/// winners as the un-skippered walk.
#[tokio::test]
async fn cyclic_graph_equivalence() {
    // A -> B -> A (cycle)
    let mut src = FixtureSource::new();
    src.add_pom(
        co("ex", "A"),
        "1.0",
        pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]),
    );
    src.add_pom(
        co("ex", "B"),
        "1.0",
        pom("ex", "B", "1.0", vec![dep("ex", "A", "1.0")]),
    );
    let root = pom("ex", "root", "1.0", vec![dep("ex", "A", "1.0")]);
    let (_g_on, _g_off) = assert_skipper_equivalence(root, &src).await;
}

/// Exclusions on the parent path: skipper must NOT skip when the
/// alternate path has fewer exclusions and could surface a different
/// transitive subgraph.
#[tokio::test]
async fn exclusion_asymmetry_does_not_break_equivalence() {
    // root -> A (excl org.foo:bar) -> M -> org.foo:bar
    // root -> B                     -> M -> org.foo:bar
    //
    // Via A, the leaf is excluded. Via B, it's kept. Both walks (with
    // and without skipper) must produce the same final graph.
    let mut src = FixtureSource::new();
    src.add_pom(
        co("ex", "A"),
        "1.0",
        pom("ex", "A", "1.0", vec![dep("ex", "M", "1.0")]),
    );
    src.add_pom(
        co("ex", "B"),
        "1.0",
        pom("ex", "B", "1.0", vec![dep("ex", "M", "1.0")]),
    );
    src.add_pom(
        co("ex", "M"),
        "1.0",
        pom("ex", "M", "1.0", vec![dep("org.foo", "bar", "1.0")]),
    );
    src.add_pom(
        co("org.foo", "bar"),
        "1.0",
        pom("org.foo", "bar", "1.0", vec![]),
    );
    let root = pom(
        "ex",
        "root",
        "1.0",
        vec![
            dep_excl("ex", "A", "1.0", &[("org.foo", "bar")]),
            dep("ex", "B", "1.0"),
        ],
    );
    assert_skipper_equivalence(root, &src).await;
}

/// Wider fan-out diamond — three roots all share a transitive subtree.
/// One root reaches the shared subtree directly (depth 1), the others
/// reach it through an intermediate (depth 2), so the skipper's
/// already-resolved-shallower rule fires when the deeper visits arrive.
#[tokio::test]
async fn fan_out_equivalence_and_skipper_fires() {
    // root -> M directly (depth 1)
    // root -> A -> M (depth 2; should be pruned by skipper as M wins at 1)
    // root -> B -> M (depth 2; same)
    let mut src = FixtureSource::new();
    let shared_deps = vec![
        dep("ex", "X", "1.0"),
        dep("ex", "Y", "1.0"),
        dep("ex", "Z", "1.0"),
    ];
    src.add_pom(co("ex", "M"), "1.0", pom("ex", "M", "1.0", shared_deps));
    for n in ["A", "B"] {
        src.add_pom(
            co("ex", n),
            "1.0",
            pom("ex", n, "1.0", vec![dep("ex", "M", "1.0")]),
        );
    }
    for n in ["X", "Y", "Z"] {
        src.add_pom(co("ex", n), "1.0", pom("ex", n, "1.0", vec![]));
    }
    let root = pom(
        "ex",
        "root",
        "1.0",
        vec![
            dep("ex", "M", "1.0"),
            dep("ex", "A", "1.0"),
            dep("ex", "B", "1.0"),
        ],
    );
    let (g_on, _) = assert_skipper_equivalence(root, &src).await;
    assert!(
        g_on.skipper_stats.total_skips() >= 1,
        "expected skipper to fire on fan-out; stats = {:?}",
        g_on.skipper_stats
    );
}

/// Real-world fixture corpus from `tests/fixtures/`: load whatever
/// fixtures are present and prove skipper equivalence over each
/// fixture's POM tree.
#[tokio::test]
async fn fixture_corpus_skipper_equivalence() {
    let src = match FixtureMetadataSource::load_default() {
        Ok(s) => s,
        Err(e) => panic!("failed to load fixtures: {e}"),
    };
    // The corpus is small; pick every loaded POM, wrap it as a root
    // and prove the walks match. Use a synthetic root that depends on
    // the fixture as its single direct dep (so transitive expansion
    // is what's exercised, not the fixture's own depMgt).
    //
    // The corpus may have POMs whose declared deps point at coords
    // not in the corpus — those should error identically in both
    // modes, but we don't want test failures from that. Filter the
    // attempts: only assert equivalence on roots that walk cleanly
    // with the skipper disabled.
    let opts_off = WalkOptions {
        enable_skipper: false,
        ..WalkOptions::default()
    };
    let opts_on = WalkOptions {
        enable_skipper: true,
        ..WalkOptions::default()
    };

    let keys: Vec<(Coords, String)> = src
        .pom_keys()
        .map(|(c, v)| (c.clone(), v.to_string()))
        .collect();
    assert!(!keys.is_empty(), "fixture corpus is empty");

    let mut compared = 0usize;
    for (coords, version) in keys {
        // Build a synthetic root depending on this fixture coord.
        let root = pom(
            "ex",
            "synthetic-root",
            "0.0.1",
            vec![dep(&coords.group, &coords.artifact, &version)],
        );
        let off_result = walk(&resolved(root.clone()), &src, &opts_off).await;
        let on_result = walk(&resolved(root), &src, &opts_on).await;
        match (off_result, on_result) {
            (Ok(off), Ok(on)) => {
                assert_eq!(winners_summary(&on), winners_summary(&off));
                compared += 1;
            }
            (Err(_), Err(_)) => {
                // Both errored identically — fine, skipper didn't
                // introduce a difference. The corpus has some POMs
                // with missing transitives.
            }
            (off, on) => panic!(
                "skipper changed walk outcome for {}: off={:?} on={:?}",
                coords,
                off.is_ok(),
                on.is_ok()
            ),
        }
    }
    assert!(
        compared >= 1,
        "no fixture-corpus roots walked cleanly; cannot prove equivalence"
    );
}

/// Skipper-disabled mode produces zero skips in stats (sanity check).
#[tokio::test]
async fn disabled_skipper_records_no_skips() {
    let mut src = FixtureSource::new();
    src.add_pom(co("ex", "L"), "1.0", pom("ex", "L", "1.0", vec![]));
    src.add_pom(
        co("ex", "A"),
        "1.0",
        pom("ex", "A", "1.0", vec![dep("ex", "L", "1.0")]),
    );
    src.add_pom(
        co("ex", "B"),
        "1.0",
        pom("ex", "B", "1.0", vec![dep("ex", "L", "1.0")]),
    );
    let root = pom(
        "ex",
        "root",
        "1.0",
        vec![dep("ex", "A", "1.0"), dep("ex", "B", "1.0")],
    );
    let opts = WalkOptions {
        enable_skipper: false,
        ..WalkOptions::default()
    };
    let g = walk(&resolved(root), &src, &opts).await.unwrap();
    assert_eq!(g.skipper_stats.total_skips(), 0);
}

/// Deep diamond: A->B->...->X reachable via two long parallel chains.
/// Skipper should fire on the second arrival at the shared subtree.
#[tokio::test]
async fn deep_parallel_chains_equivalence() {
    let mut src = FixtureSource::new();
    // Left chain: L1 -> L2 -> L3 -> S
    src.add_pom(
        co("ex", "L1"),
        "1.0",
        pom("ex", "L1", "1.0", vec![dep("ex", "L2", "1.0")]),
    );
    src.add_pom(
        co("ex", "L2"),
        "1.0",
        pom("ex", "L2", "1.0", vec![dep("ex", "L3", "1.0")]),
    );
    src.add_pom(
        co("ex", "L3"),
        "1.0",
        pom("ex", "L3", "1.0", vec![dep("ex", "S", "1.0")]),
    );
    // Right chain: R1 -> R2 -> S (shorter, so it wins S)
    src.add_pom(
        co("ex", "R1"),
        "1.0",
        pom("ex", "R1", "1.0", vec![dep("ex", "R2", "1.0")]),
    );
    src.add_pom(
        co("ex", "R2"),
        "1.0",
        pom("ex", "R2", "1.0", vec![dep("ex", "S", "1.0")]),
    );
    src.add_pom(co("ex", "S"), "1.0", pom("ex", "S", "1.0", vec![]));

    let root = pom(
        "ex",
        "root",
        "1.0",
        vec![dep("ex", "L1", "1.0"), dep("ex", "R1", "1.0")],
    );
    let (g_on, _) = assert_skipper_equivalence(root, &src).await;
    assert!(g_on.skipper_stats.total_skips() >= 1);
}

/// Skipper-on graph order may differ from skipper-off graph order (the
/// `resolved` discovery list), but the WINNERS map is identical.
/// This test asserts that explicitly.
#[tokio::test]
async fn winners_identical_even_if_discovery_order_differs() {
    let mut src = FixtureSource::new();
    src.add_pom(
        co("ex", "shared"),
        "1.0",
        pom("ex", "shared", "1.0", vec![]),
    );
    src.add_pom(
        co("ex", "A"),
        "1.0",
        pom("ex", "A", "1.0", vec![dep("ex", "shared", "1.0")]),
    );
    src.add_pom(
        co("ex", "B"),
        "1.0",
        pom("ex", "B", "1.0", vec![dep("ex", "shared", "1.0")]),
    );
    let root = pom(
        "ex",
        "root",
        "1.0",
        vec![dep("ex", "A", "1.0"), dep("ex", "B", "1.0")],
    );
    let (g_on, g_off) = assert_skipper_equivalence(root, &src).await;
    // Same set of winning coords.
    let on_keys: std::collections::BTreeSet<_> = g_on.winners.keys().collect();
    let off_keys: std::collections::BTreeSet<_> = g_off.winners.keys().collect();
    assert_eq!(on_keys, off_keys);
}
