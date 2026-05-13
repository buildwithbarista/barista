//! BFS+Skipper resolver spike — not production code.
//!
//! Demonstrates the BFS-with-skipper dependency-resolution algorithm
//! on three small hand-built graphs. Verifies:
//!
//! - Diamond conflict picks the winning version (nearest-wins).
//! - Cyclic graph terminates.
//! - Skipper prunes already-resolved subtrees.
//!
//! Run: cargo run --example spike-bfs
//!
//! This is intentionally deletable research scratch code. It uses only
//! `std` — no Maven Central fetch, no POM parsing. The "repository" is a
//! hardcoded HashMap.

use std::collections::{HashMap, VecDeque};

// --- Data model (a stripped-down Maven coordinate) ----------------------

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Coords {
    group: String,
    artifact: String,
}

impl Coords {
    fn new(group: &str, artifact: &str) -> Self {
        Self {
            group: group.into(),
            artifact: artifact.into(),
        }
    }
}

impl std::fmt::Display for Coords {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.group, self.artifact)
    }
}

#[derive(Clone, Debug)]
struct Dep {
    coords: Coords,
    version: String,
}

impl Dep {
    fn new(group: &str, artifact: &str, version: &str) -> Self {
        Self {
            coords: Coords::new(group, artifact),
            version: version.into(),
        }
    }
}

impl std::fmt::Display for Dep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.coords, self.version)
    }
}

#[derive(Clone, Debug)]
struct PomEntry {
    deps: Vec<Dep>,
}

type Repo = HashMap<(Coords, String), PomEntry>;

fn pom(deps: Vec<Dep>) -> PomEntry {
    PomEntry { deps }
}

// --- Hardcoded test corpora ---------------------------------------------

/// Diamond:
///   A 1.0 --> B 1.0 --> C 1.0
///   A 1.0 --> D 1.0 --> C 2.0
///
/// Nearest-wins: B and D are both at depth 1; their C children are both
/// at depth 2. Tie broken by declaration order — B is declared first in
/// A's deps, so B's C 1.0 wins.
fn build_test_corpus_diamond() -> Repo {
    let mut r: Repo = HashMap::new();
    r.insert(
        (Coords::new("ex", "A"), "1.0".into()),
        pom(vec![Dep::new("ex", "B", "1.0"), Dep::new("ex", "D", "1.0")]),
    );
    r.insert(
        (Coords::new("ex", "B"), "1.0".into()),
        pom(vec![Dep::new("ex", "C", "1.0")]),
    );
    r.insert(
        (Coords::new("ex", "D"), "1.0".into()),
        pom(vec![Dep::new("ex", "C", "2.0")]),
    );
    r.insert((Coords::new("ex", "C"), "1.0".into()), pom(vec![]));
    r.insert((Coords::new("ex", "C"), "2.0".into()), pom(vec![]));
    r
}

/// Cycle:
///   A 1.0 --> B 1.0 --> C 1.0 --> A 1.0
///
/// Resolver must terminate; the second visit to A 1.0 should be pruned
/// by the skipper because A is already a winner at depth 0.
fn build_test_corpus_cycle() -> Repo {
    let mut r: Repo = HashMap::new();
    r.insert(
        (Coords::new("ex", "A"), "1.0".into()),
        pom(vec![Dep::new("ex", "B", "1.0")]),
    );
    r.insert(
        (Coords::new("ex", "B"), "1.0".into()),
        pom(vec![Dep::new("ex", "C", "1.0")]),
    );
    r.insert(
        (Coords::new("ex", "C"), "1.0".into()),
        pom(vec![Dep::new("ex", "A", "1.0")]),
    );
    r
}

/// Skipper pruning:
///   A 1.0 --> X 1.0 --> M 1.0 --> N 1.0 --> Y 1.0
///   A 1.0 --> Y 1.0
///
/// Y is found at depth 1 directly. When we later reach the deep chain
/// under X and the resolver would re-visit Y at depth 4, the skipper
/// prunes — Y was already won at a shallower depth. More importantly,
/// when popping X's subtree, if a node has already been superseded, the
/// whole subtree is skipped via the depth check.
fn build_test_corpus_skipper() -> Repo {
    let mut r: Repo = HashMap::new();
    r.insert(
        (Coords::new("ex", "A"), "1.0".into()),
        pom(vec![Dep::new("ex", "X", "1.0"), Dep::new("ex", "Y", "1.0")]),
    );
    r.insert(
        (Coords::new("ex", "X"), "1.0".into()),
        pom(vec![Dep::new("ex", "M", "1.0")]),
    );
    r.insert(
        (Coords::new("ex", "M"), "1.0".into()),
        pom(vec![Dep::new("ex", "N", "1.0")]),
    );
    r.insert(
        (Coords::new("ex", "N"), "1.0".into()),
        pom(vec![Dep::new("ex", "Y", "1.0")]),
    );
    r.insert((Coords::new("ex", "Y"), "1.0".into()), pom(vec![]));
    r
}

// --- BFS+Skipper resolver -----------------------------------------------

/// Trace event for visibility into what the resolver did.
#[derive(Debug)]
enum Trace {
    Emit { dep: Dep, depth: usize },
    SkipAlreadyWon { coords: Coords, depth: usize, won_depth: usize },
    SkipSupersededSubtree { dep: Dep, depth: usize, won_depth: usize },
    Missing { dep: Dep },
}

struct ResolveResult {
    order: Vec<Dep>,
    trace: Vec<Trace>,
}

/// Conflict-resolution policy: **nearest-wins** (Maven's default).
///
/// - `winners` maps a coordinate to the (version, depth) currently
///   selected. A coordinate at strictly shallower depth always wins.
///   On equal depth, the first seen wins (BFS + declaration order).
/// - `order` is the list of selected `Dep`s, in the order they were
///   first emitted. When a deeper-depth winner is later replaced by a
///   shallower one, we DO NOT re-emit — but in nearest-wins BFS, a
///   coord's *first* sighting is always at the minimum depth, so this
///   never happens. The skipper handles it.
/// - `queue` holds `(Dep, depth)` pairs to expand.
///
/// Skipper logic: when we pop `(dep, depth)`, if `winners[dep.coords]`
/// shows a strictly shallower depth than `depth`, the subtree rooted at
/// this pop was superseded after we enqueued it. Skip the whole pop —
/// don't expand its transitive deps. This prunes work that a naive BFS
/// would do.
fn resolve(root: Dep, repo: &Repo) -> ResolveResult {
    let mut winners: HashMap<Coords, (String, usize)> = HashMap::new();
    let mut order: Vec<Dep> = Vec::new();
    let mut queue: VecDeque<(Dep, usize)> = VecDeque::new();
    let mut trace: Vec<Trace> = Vec::new();

    winners.insert(root.coords.clone(), (root.version.clone(), 0));
    order.push(root.clone());
    trace.push(Trace::Emit { dep: root.clone(), depth: 0 });
    queue.push_back((root, 0));

    while let Some((dep, depth)) = queue.pop_front() {
        // Skipper: did this dep get superseded after enqueue?
        if let Some((won_ver, won_depth)) = winners.get(&dep.coords) {
            if *won_depth < depth || (*won_depth == depth && won_ver != &dep.version) {
                trace.push(Trace::SkipSupersededSubtree {
                    dep: dep.clone(),
                    depth,
                    won_depth: *won_depth,
                });
                continue;
            }
        }

        let key = (dep.coords.clone(), dep.version.clone());
        let Some(entry) = repo.get(&key) else {
            trace.push(Trace::Missing { dep: dep.clone() });
            continue;
        };

        let child_depth = depth + 1;
        for child in &entry.deps {
            match winners.get(&child.coords) {
                Some((_, won_depth)) if *won_depth <= child_depth => {
                    // Already have a winner at shallower-or-equal depth.
                    // Nearest-wins + declaration-order tiebreak: keep the
                    // existing winner; skip emit and don't enqueue (its
                    // subtree will be expanded from the original enqueue).
                    trace.push(Trace::SkipAlreadyWon {
                        coords: child.coords.clone(),
                        depth: child_depth,
                        won_depth: *won_depth,
                    });
                }
                _ => {
                    // First sighting OR deeper-then-shallower replacement.
                    // In strict BFS the latter can't happen — but we
                    // handle it defensively. Record winner, emit, enqueue.
                    winners.insert(
                        child.coords.clone(),
                        (child.version.clone(), child_depth),
                    );
                    order.push(child.clone());
                    trace.push(Trace::Emit {
                        dep: child.clone(),
                        depth: child_depth,
                    });
                    queue.push_back((child.clone(), child_depth));
                }
            }
        }
    }

    ResolveResult { order, trace }
}

// --- Test harness -------------------------------------------------------

fn print_corpus(name: &str, repo: &Repo) {
    println!("Corpus ({name}):");
    // Sort for deterministic display.
    let mut keys: Vec<_> = repo.keys().collect();
    keys.sort_by(|a, b| {
        a.0.group
            .cmp(&b.0.group)
            .then(a.0.artifact.cmp(&b.0.artifact))
            .then(a.1.cmp(&b.1))
    });
    for k in keys {
        let entry = &repo[k];
        let parent = format!("{}:{}", k.0, k.1);
        if entry.deps.is_empty() {
            println!("  {parent}  (leaf)");
        } else {
            let kids: Vec<String> = entry.deps.iter().map(|d| d.to_string()).collect();
            println!("  {parent}  -->  [{}]", kids.join(", "));
        }
    }
}

fn print_resolution(result: &ResolveResult) {
    println!("Resolved order:");
    for (i, dep) in result.order.iter().enumerate() {
        println!("  {i}. {dep}");
    }
    println!("Trace:");
    for ev in &result.trace {
        match ev {
            Trace::Emit { dep, depth } => println!("  EMIT      {dep} @ depth {depth}"),
            Trace::SkipAlreadyWon { coords, depth, won_depth } => {
                println!(
                    "  SKIP      {coords} @ depth {depth}  (already won at depth {won_depth})"
                );
            }
            Trace::SkipSupersededSubtree { dep, depth, won_depth } => {
                println!(
                    "  PRUNE     {dep} @ depth {depth}  (subtree superseded; winner at depth {won_depth})"
                );
            }
            Trace::Missing { dep } => println!("  MISSING   {dep} (not in corpus)"),
        }
    }
}

fn run_diamond_test(repo: &Repo) {
    print_corpus("diamond", repo);
    let result = resolve(Dep::new("ex", "A", "1.0"), repo);
    print_resolution(&result);

    let by_coords: HashMap<Coords, String> = result
        .order
        .iter()
        .map(|d| (d.coords.clone(), d.version.clone()))
        .collect();
    let c_version = by_coords.get(&Coords::new("ex", "C"));
    let expected = "1.0";
    if c_version.map(|s| s.as_str()) == Some(expected) && result.order.len() == 4 {
        println!(
            "PASS: diamond resolved with C {expected} winning (4 nodes; nearest-wins + declaration-order tiebreak)"
        );
    } else {
        println!(
            "FAIL: diamond expected C {expected} and 4 nodes, got C {:?} and {} nodes",
            c_version,
            result.order.len()
        );
    }
}

fn run_cycle_test(repo: &Repo) {
    print_corpus("cycle", repo);
    // If resolve doesn't terminate, the process hangs — that itself is
    // the failure mode. If it returns, we verified termination.
    let result = resolve(Dep::new("ex", "A", "1.0"), repo);
    print_resolution(&result);

    let coords: Vec<_> = result.order.iter().map(|d| d.coords.to_string()).collect();
    let unique: std::collections::HashSet<_> = coords.iter().collect();
    if result.order.len() == 3 && unique.len() == 3 {
        println!("PASS: cycle terminated; each of A, B, C emitted exactly once");
    } else {
        println!(
            "FAIL: cycle expected 3 unique nodes, got {} total / {} unique",
            result.order.len(),
            unique.len()
        );
    }
}

fn run_skipper_test(repo: &Repo) {
    print_corpus("skipper", repo);
    let result = resolve(Dep::new("ex", "A", "1.0"), repo);
    print_resolution(&result);

    // Expected behavior: A, X, Y emit at depths 0, 1, 1. M emits at 2,
    // N at 3. When N's expansion considers Y, Y is already won at
    // depth 1 — we get a SkipAlreadyWon. No re-emit of Y.
    let y_count = result
        .order
        .iter()
        .filter(|d| d.coords == Coords::new("ex", "Y"))
        .count();
    let skipper_pruned_y = result.trace.iter().any(|t| matches!(
        t,
        Trace::SkipAlreadyWon { coords, .. } if *coords == Coords::new("ex", "Y")
    ));
    if y_count == 1 && skipper_pruned_y {
        println!("PASS: skipper pruned Y's re-entry from N's subtree (Y emitted once at depth 1)");
    } else {
        println!(
            "FAIL: skipper expected exactly 1 Y emission AND a skip event for Y; got y_count={y_count} skipper_pruned_y={skipper_pruned_y}"
        );
    }
}

fn main() {
    println!("=== Diamond conflict test ===");
    run_diamond_test(&build_test_corpus_diamond());
    println!();
    println!("=== Cycle termination test ===");
    run_cycle_test(&build_test_corpus_cycle());
    println!();
    println!("=== Skipper pruning test ===");
    run_skipper_test(&build_test_corpus_skipper());
}
