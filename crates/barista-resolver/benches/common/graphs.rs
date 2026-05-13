//! Shared synthetic graph builders for resolver microbenchmarks and
//! the skip-rate report example.
//!
//! Each builder returns:
//!
//! - The root [`RawPom`] (a project whose `<dependencies>` are the
//!   direct deps of the graph).
//! - A [`GraphSource`] that can answer `fetch_pom` /
//!   `fetch_metadata` for every transitive coord in the graph.
//! - The naive visit count — i.e. the number of `(coord, depth)`
//!   pairs a non-pruning walk would visit (the unfolded BFS tree).
//!   The walker, with nearest-wins + skipper enabled, will issue
//!   strictly fewer `fetch_pom` calls than this; the ratio is the
//!   "combined prune rate" used to validate PRD §5's ≥60% target.
//!
//! Each graph is small enough to construct in microseconds; bench
//! iterations build the source once outside `iter` and only time the
//! walk itself.

// This module is shared between the `walker` bench and the
// `skip_rate_report` example. Each consumer uses a different subset
// of the API surface, so blanket-allow dead_code rather than
// chasing per-consumer feature flags.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use barista_coords::Coords;
use barista_pom::{EffectivePom, RawDependency, RawPom, ResolvedPom, raw::Properties};
use barista_resolver::source::{
    FetchOrigin, GaMetadata, MetadataError, MetadataSource, ResolveKey,
};

// ---------------------------------------------------------------------------
// GraphSource
// ---------------------------------------------------------------------------

/// In-memory [`MetadataSource`] backed by a hand-built `(coords,
/// version) -> RawPom` map. Counts every `fetch_pom` call so the
/// skip-rate harness can compute the combined-prune rate.
pub struct GraphSource {
    poms: HashMap<(Coords, String), RawPom>,
    metadata: HashMap<Coords, Vec<String>>,
    pom_fetches: AtomicU64,
}

impl GraphSource {
    pub fn new() -> Self {
        Self {
            poms: HashMap::new(),
            metadata: HashMap::new(),
            pom_fetches: AtomicU64::new(0),
        }
    }

    pub fn add(&mut self, coords: Coords, version: &str, pom: RawPom) {
        self.metadata
            .entry(coords.clone())
            .or_default()
            .push(version.to_string());
        self.poms.insert((coords, version.to_string()), pom);
    }

    pub fn pom_fetches(&self) -> u64 {
        self.pom_fetches.load(Ordering::Relaxed)
    }

    pub fn reset_counters(&self) {
        self.pom_fetches.store(0, Ordering::Relaxed);
    }
}

#[async_trait]
impl MetadataSource for GraphSource {
    async fn fetch_pom(
        &self,
        coords: &ResolveKey,
        version: &str,
    ) -> Result<(RawPom, FetchOrigin), MetadataError> {
        self.pom_fetches.fetch_add(1, Ordering::Relaxed);
        match self.poms.get(&(coords.clone(), version.to_string())) {
            Some(p) => Ok((p.clone(), FetchOrigin::Fixture)),
            None => Err(MetadataError::NotFound {
                coords: format!("{}:{}", coords.group, coords.artifact),
                version: version.to_string(),
            }),
        }
    }

    async fn fetch_metadata(
        &self,
        coords: &ResolveKey,
    ) -> Result<(GaMetadata, FetchOrigin), MetadataError> {
        match self.metadata.get(coords) {
            Some(v) => Ok((
                GaMetadata {
                    coords: coords.clone(),
                    versions: v.clone(),
                    latest_snapshot_timestamp: None,
                    last_updated: None,
                },
                FetchOrigin::Fixture,
            )),
            None => Err(MetadataError::MetadataNotFound {
                coords: format!("{}:{}", coords.group, coords.artifact),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn co(group: &str, artifact: &str) -> Coords {
    Coords::new(group, artifact).expect("valid coords")
}

fn dep(group: &str, artifact: &str, version: &str) -> RawDependency {
    RawDependency {
        group_id: group.into(),
        artifact_id: artifact.into(),
        version: Some(version.into()),
        ..RawDependency::default()
    }
}

fn pom(group: &str, artifact: &str, version: &str, deps: Vec<RawDependency>) -> RawPom {
    RawPom {
        model_version: "4.0.0".into(),
        group_id: Some(group.into()),
        artifact_id: artifact.into(),
        version: Some(version.into()),
        packaging: "jar".into(),
        dependencies: deps,
        properties: Properties::default(),
        ..RawPom::default()
    }
}

/// Wrap a [`RawPom`] in a [`ResolvedPom`] with no parent chain and no
/// interpolations. The bench graphs never use parent inheritance, so
/// `EffectivePom { pom, .. }` is identical to the input.
pub fn as_resolved(p: RawPom) -> ResolvedPom {
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

/// A built graph + bookkeeping for the skip-rate harness.
pub struct Graph {
    pub name: &'static str,
    pub root: RawPom,
    pub source: GraphSource,
    /// Number of `(parent -> child)` traversal events a non-pruning
    /// walk would visit (the unfolded BFS tree, minus the root).
    /// `fetch_pom` calls with skipper + nearest-wins enabled should
    /// be a small fraction of this.
    pub naive_visits: u64,
}

// ---------------------------------------------------------------------------
// Graph 1: diamond
// ---------------------------------------------------------------------------

/// `root -> A -> C` and `root -> B -> C`. Classic nearest-wins conflict.
/// Naive visits: 4 (A, B, C-via-A, C-via-B). With pruning: A, B, C.
pub fn diamond() -> Graph {
    let mut src = GraphSource::new();
    src.add(
        co("ex", "A"),
        "1.0",
        pom("ex", "A", "1.0", vec![dep("ex", "C", "1.0")]),
    );
    src.add(
        co("ex", "B"),
        "1.0",
        pom("ex", "B", "1.0", vec![dep("ex", "C", "1.0")]),
    );
    src.add(co("ex", "C"), "1.0", pom("ex", "C", "1.0", vec![]));

    let root = pom(
        "ex",
        "root",
        "1.0",
        vec![dep("ex", "A", "1.0"), dep("ex", "B", "1.0")],
    );

    Graph {
        name: "diamond",
        root,
        source: src,
        naive_visits: 4,
    }
}

// ---------------------------------------------------------------------------
// Graph 2: fan-out N x M with heavy sharing
// ---------------------------------------------------------------------------

/// Build a fan-out graph: N "module" deps `M_1..M_N` at the root,
/// each declaring the SAME M shared transitive deps `L_1..L_M`. The
/// shared-leaf pattern is the BFS+Skipper target case — every
/// `M_i -> L_j` edge after the first is prunable.
///
/// Naive visits: `N + N*M` (every parent walks every leaf).
/// With pruning: `N + M` (each unique coord seen once).
pub fn fan_out_shared(n: u32, m: u32) -> Graph {
    let mut src = GraphSource::new();

    let leaf_deps: Vec<RawDependency> =
        (0..m).map(|j| dep("ex", &format!("L{j}"), "1.0")).collect();

    for i in 0..n {
        let module = format!("M{i}");
        src.add(
            co("ex", &module),
            "1.0",
            pom("ex", &module, "1.0", leaf_deps.clone()),
        );
    }

    for j in 0..m {
        let leaf = format!("L{j}");
        src.add(co("ex", &leaf), "1.0", pom("ex", &leaf, "1.0", vec![]));
    }

    let root_deps: Vec<RawDependency> =
        (0..n).map(|i| dep("ex", &format!("M{i}"), "1.0")).collect();
    let root = pom("ex", "root", "1.0", root_deps);

    Graph {
        name: "fan_out_shared",
        root,
        source: src,
        naive_visits: u64::from(n) + u64::from(n) * u64::from(m),
    }
}

// ---------------------------------------------------------------------------
// Graph 3: deep chain
// ---------------------------------------------------------------------------

/// Linear chain `A_0 -> A_1 -> ... -> A_{depth-1}`. No cross-edges,
/// so skipper has nothing to prune. Useful as a baseline.
///
/// Naive visits == real visits == depth.
pub fn deep_chain(depth: u32) -> Graph {
    let mut src = GraphSource::new();

    for i in 0..depth {
        let here = format!("A{i}");
        let deps = if i + 1 < depth {
            vec![dep("ex", &format!("A{}", i + 1), "1.0")]
        } else {
            Vec::new()
        };
        src.add(co("ex", &here), "1.0", pom("ex", &here, "1.0", deps));
    }

    let root = pom("ex", "root", "1.0", vec![dep("ex", "A0", "1.0")]);

    Graph {
        name: "deep_chain",
        root,
        source: src,
        naive_visits: u64::from(depth),
    }
}

// ---------------------------------------------------------------------------
// Graph 4: full synthetic — two-level fan-out with sharing
// ---------------------------------------------------------------------------

/// Two-level fan-out: root pulls 6 "framework" deps `F_1..F_6`.
/// Each framework dep pulls the same set of 8 "utility" deps
/// `U_1..U_8`. Each utility dep pulls the same set of 4 "core" deps
/// `C_1..C_4`. The result is a wide-and-shallow graph with massive
/// overlap — representative of real-world Maven projects (think
/// `spring-*` + `commons-*` + `slf4j-*`).
///
/// Naive visits: 6 + 6*8 + 6*8*4 = 6 + 48 + 192 = 246.
/// With pruning: 6 + 8 + 4 = 18 unique coords.
pub fn full_synthetic() -> Graph {
    const F: u32 = 6;
    const U: u32 = 8;
    const C: u32 = 4;

    let mut src = GraphSource::new();

    let core_deps: Vec<RawDependency> =
        (0..C).map(|k| dep("ex", &format!("C{k}"), "1.0")).collect();
    let util_deps: Vec<RawDependency> =
        (0..U).map(|j| dep("ex", &format!("U{j}"), "1.0")).collect();

    for k in 0..C {
        let name = format!("C{k}");
        src.add(co("ex", &name), "1.0", pom("ex", &name, "1.0", vec![]));
    }
    for j in 0..U {
        let name = format!("U{j}");
        src.add(
            co("ex", &name),
            "1.0",
            pom("ex", &name, "1.0", core_deps.clone()),
        );
    }
    for i in 0..F {
        let name = format!("F{i}");
        src.add(
            co("ex", &name),
            "1.0",
            pom("ex", &name, "1.0", util_deps.clone()),
        );
    }

    let root_deps: Vec<RawDependency> =
        (0..F).map(|i| dep("ex", &format!("F{i}"), "1.0")).collect();
    let root = pom("ex", "root", "1.0", root_deps);

    Graph {
        name: "full_synthetic",
        root,
        source: src,
        naive_visits: u64::from(F)
            + u64::from(F) * u64::from(U)
            + u64::from(F) * u64::from(U) * u64::from(C),
    }
}
