// SPDX-License-Identifier: MIT OR Apache-2.0

//! BFS frontier + nearest-wins dependency walker.
//!
//! Given a root [`ResolvedPom`] (parent-merged, interpolated, BOM-imported,
//! profile-applied, depMgt-applied — i.e. the output of [`barista_pom::resolve_pom`])
//! and a [`MetadataSource`], the walker emits the resolved dependency
//! graph by:
//!
//! 1. Enqueuing each direct dep at depth 1 in declaration order.
//! 2. For each dequeued node:
//!    a. Apply scope inheritance from the parent path.
//!    b. Apply accumulated exclusions from the parent path.
//!    c. If the coord is already-winning at a shallower-or-equal depth,
//!    skip (nearest-wins + declaration-order tiebreak).
//!    d. Otherwise: fetch the dep's POM via `MetadataSource`, resolve its
//!    own depMgt via [`barista_pom::resolve_pom`], and enqueue each of
//!    its transitive deps at depth+1.
//! 3. Cycle skipping falls out of the same nearest-wins check: a coord
//!    already in `winners` skips on second visit regardless of depth.
//!
//! Frontier ordering is deterministic: depth-then-declaration-order within
//! each parent. The walker is async — `MetadataSource::fetch_pom` calls
//! are awaited sequentially so the BFS order is stable. Parallel pre-fetch
//! is achieved (transparently) by the underlying cache's connection pool
//! and the [`MetadataSource::warm`] hint, *not* by out-of-order traversal.
//!
//! # Skipper seam (M2.1 Task 3)
//!
//! This implementation does NOT yet integrate the exclusion-aware skipper
//! responsible for pruning entire subtrees whose coords are guaranteed to
//! lose to a shallower winner. The seam lives at exactly one site —
//! immediately before [`fetch_pom`](MetadataSource::fetch_pom) inside the
//! main BFS loop (see `// SKIPPER SEAM` marker). Task 3 will:
//!
//! 1. Maintain a `Skipper` data structure that tracks "this subtree
//!    cannot produce any new winner" predicates.
//! 2. Consult the skipper at the seam: if it returns "prune",
//!    skip the `fetch_pom` + child-enqueue entirely.
//! 3. Feed every successful winner back into the skipper so the
//!    predicate set grows monotonically.
//!
//! Task 2 implements correct nearest-wins + scope + exclusion behaviour;
//! Task 3 makes it fast by avoiding pointless fetches.
//!
//! # SNAPSHOT handling (M2.1 Task 5)
//!
//! SNAPSHOT versions are currently treated as opaque version strings —
//! the same coord-with-`-SNAPSHOT` is one coord, and version equality is
//! string equality after [`barista_version::Version`] canonicalisation.
//! Task 5 will plug in snapshot-timestamp resolution via
//! [`MetadataSource::fetch_metadata`].

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use barista_coords::Coords;
use barista_pom::{
    ActivationContext, ParentResolver, RawDependency, RawExclusion, RawPom, ResolvedPom,
    resolve_pom,
};
use barista_version::Version;

use crate::oreq::{MetadataKey, OreqSession, OreqStats};
use crate::skipper::{ExclusionSet, SkipDecision, SkipperState, SkipperStats};
use crate::source::{MetadataError, MetadataSource, ResolveKey};
use crate::version_spec::{ParseError as SpecParseError, SpecWarning, VersionSpec};

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// The resolved dependency graph: a list of edges + the winning version
/// per coord.
#[derive(Debug, Clone, Default)]
pub struct ResolvedGraph {
    /// Final flat list of resolved deps, in BFS discovery order.
    pub resolved: Vec<ResolvedDep>,
    /// Per-coord winning version, indexed for lookup.
    pub winners: BTreeMap<Coords, ResolvedDep>,
    /// Warnings emitted during resolution.
    pub warnings: Vec<SpecWarning>,
    /// Per-coord audit: which path won and what alternatives were seen.
    pub audit: Vec<AuditEntry>,
    /// Skipper telemetry. Zeroed when the skipper is disabled via
    /// [`WalkOptions::enable_skipper`] = `false`.
    pub skipper_stats: SkipperStats,
    /// O-REQ-01..05 counters (PRD §18.3). Zeroed unless the caller
    /// provided an [`OreqSession`] via [`WalkOptions::oreq`].
    pub oreq_stats: OreqStats,
}

/// A resolved dependency edge in the final graph.
#[derive(Debug, Clone)]
pub struct ResolvedDep {
    pub coords: Coords,
    pub version: String,
    pub scope: Scope,
    pub classifier: Option<String>,
    pub type_: String,
    pub optional: bool,
    /// Distance in BFS from the root project. Root deps are depth 1.
    pub depth: u32,
    /// Path through the graph that resolved this coord. For a coord
    /// reached via `root -> spring-boot -> spring-core`, this is
    /// `[spring-boot-coords, spring-core-coords]` (the root is implicit).
    pub winning_path: Vec<Coords>,
}

/// Maven dependency scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    Compile,
    Provided,
    Runtime,
    Test,
    System,
    /// depMgt-only; never appears in the resolved list.
    Import,
}

impl Scope {
    /// Parse Maven's `<scope>` text. Empty / unknown / missing → `Compile`
    /// (Maven's default).
    pub fn parse(raw: Option<&str>) -> Scope {
        match raw.map(str::trim).unwrap_or("") {
            "compile" | "" => Scope::Compile,
            "provided" => Scope::Provided,
            "runtime" => Scope::Runtime,
            "test" => Scope::Test,
            "system" => Scope::System,
            "import" => Scope::Import,
            _ => Scope::Compile,
        }
    }

    /// Maven's scope-inheritance table. When a transitive dep is reached
    /// via a parent path of effective-scope `parent_path_scope` and the
    /// dep itself declares `declared`, this returns the resulting
    /// transitive scope, or `None` if the transitive should be omitted.
    ///
    /// Maven's rule, paraphrased:
    ///
    /// - A transitive whose **declared** scope is `provided`, `test`,
    ///   `system`, or `import` is **dropped**. The Maven docs phrase
    ///   this as "provided / test / system dependencies are not
    ///   transitive"; in practice `mvn dependency:tree` omits these,
    ///   and a build's compile classpath does not see them. Concrete
    ///   example: `log4j-to-slf4j`'s POM declares `org.osgi.core` at
    ///   `<scope>provided</scope>` — Maven omits `org.osgi.core` from
    ///   the closure of any project that pulls in `log4j-to-slf4j`.
    /// - A parent in the path whose effective scope is `provided`,
    ///   `system`, or `import` is non-transitive: the entire subtree
    ///   below it is dropped. (For `provided`, this is the same rule
    ///   as above, applied recursively: a `provided` direct-dep does
    ///   not propagate its closure to consumers.)
    /// - Otherwise, the transitive's effective scope is computed by
    ///   the standard table for the remaining `(compile, runtime)`
    ///   parent-row × `(compile, runtime)` declared-column pairs, with
    ///   `test` parent-row preserving `test` on its transitives.
    pub fn inherit(parent_path_scope: Scope, declared: Scope) -> Option<Scope> {
        use Scope::*;

        // Declared scope drops first: provided / test / system / import
        // transitives are never propagated, regardless of parent path.
        match declared {
            Provided | Test | System | Import => return None,
            _ => {}
        }

        // Parent-path scope drops second: provided / system / import
        // parents have non-transitive closures.
        match parent_path_scope {
            Provided | System | Import => return None,
            _ => {}
        }

        // Remaining cases are the Maven mediation table restricted to
        // the (compile, runtime) declared × (compile, runtime, test)
        // parent grid.
        match (parent_path_scope, declared) {
            (Compile, Compile) => Some(Compile),
            (Compile, Runtime) => Some(Runtime),
            (Runtime, Compile) => Some(Runtime),
            (Runtime, Runtime) => Some(Runtime),
            (Test, Compile) => Some(Test),
            (Test, Runtime) => Some(Test),
            // Every other (parent, declared) combination has already
            // been filtered above.
            _ => None,
        }
    }
}

/// Per-coord conflict audit.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub coords: Coords,
    pub winning_version: String,
    pub winning_depth: u32,
    /// `(version, depth)` for every loser seen during traversal.
    pub also_seen_at: Vec<(String, u32)>,
}

/// Knobs for [`walk`].
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// If true (default), omit `<optional>true</optional>` transitive deps.
    /// `<optional>true</optional>` on a *direct* dep is always kept.
    pub strip_optional: bool,
    /// Scopes to include in the output. Empty = all except `Import`.
    /// `System` is included by default but never propagates transitives.
    pub include_scopes: BTreeSet<Scope>,
    /// Activation context for resolving transitive POMs. Defaults to an
    /// empty context (only `activeByDefault` profiles fire).
    pub activation: ActivationContext,
    /// Enable BFS+Skipper subtree pruning. Default: `true`. Set to
    /// `false` to produce the un-skippered graph for differential
    /// testing — correctness requires the two modes produce identical
    /// `winners` maps.
    pub enable_skipper: bool,
    /// Optional [`OreqSession`] (PRD §18.3, O-REQ-01..05) that the
    /// walker consults to dedup `maven-metadata.xml` lookups,
    /// short-circuit metadata under frozen-lockfile mode, and dedup
    /// parent / effective POMs in-session. When `None` the walker
    /// behaves exactly as it did pre-B.2-T1 — no dedup, no
    /// counters. When `Some`, the underlying [`MetadataSource`] is
    /// still consulted on cache misses; the session only avoids the
    /// redundant calls. The session is shared via `Arc` so the
    /// caller can hold a clone and read counters after the walk.
    pub oreq: Option<Arc<OreqSession>>,
    /// Repository identifier used as the O-REQ-01 dedup key. The
    /// resolver itself doesn't know which upstream is configured —
    /// the cache layer does. For v0.1 single-repo resolves this is
    /// fine to leave at the default `"default"` literal; multi-repo
    /// resolves should set it to the configured upstream URL so
    /// per-repo dedup is correct.
    pub repo_id: String,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            strip_optional: true,
            include_scopes: BTreeSet::new(),
            activation: ActivationContext::default(),
            enable_skipper: true,
            oreq: None,
            repo_id: "default".to_string(),
        }
    }
}

/// Errors produced by [`walk`].
#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    /// Underlying [`MetadataSource`] error.
    #[error("metadata source error: {0}")]
    Metadata(#[from] MetadataError),
    /// A `<version>` element couldn't be parsed as a [`VersionSpec`].
    #[error("invalid version spec {spec:?} for {coords}: {detail}")]
    InvalidSpec {
        coords: String,
        spec: String,
        detail: String,
    },
    /// A transitive dep had no concrete version after the depMgt pass.
    #[error("dependencyManagement could not provide a version for {coords}")]
    MissingVersion { coords: String },
    /// `LATEST` / `RELEASE` was used but `fetch_metadata` returned no versions.
    #[error("meta-version {spec} for {coords} has no candidate versions")]
    NoMetaVersionCandidate { coords: String, spec: String },
    /// A `<dependency>` referenced a coord we couldn't even construct.
    #[error("invalid dependency coordinate {detail}")]
    InvalidCoords { detail: String },
    /// A SNAPSHOT version was encountered but no timestamped publish
    /// could be resolved from the upstream `maven-metadata.xml`.
    #[error("could not resolve snapshot version for {coords}:{version}: {detail}")]
    SnapshotResolution {
        coords: String,
        version: String,
        detail: String,
    },
    /// A transitive POM declared `<parent>` but the parent chain
    /// couldn't be fully fetched from the metadata source. This was a
    /// silent skip before — every POM with a `<parent>` had its
    /// transitive subtree dropped, which under-counted Maven Central
    /// closures by ~15% on Spring-Boot-shaped projects.
    #[error("could not resolve parent chain for {coords}:{version}: {detail}")]
    ParentChainResolution {
        coords: String,
        version: String,
        detail: String,
    },
    /// Building the effective POM (parent merge + interpolation +
    /// depMgt + profiles + BOM imports) failed for a transitive dep.
    #[error("could not build effective POM for {coords}:{version}: {detail}")]
    EffectivePomResolution {
        coords: String,
        version: String,
        detail: String,
    },
}

// ---------------------------------------------------------------------------
// Walk
// ---------------------------------------------------------------------------

/// Walk the dependency graph rooted at `root` using `source` for transitive
/// POM fetches. `root` must already be the output of
/// [`barista_pom::resolve_pom`] (parent-merged, interpolated, depMgt-applied).
pub async fn walk<S: MetadataSource + ?Sized>(
    root: &ResolvedPom,
    source: &S,
    opts: &WalkOptions,
) -> Result<ResolvedGraph, WalkError> {
    let mut state = WalkState::new(opts.enable_skipper);
    // Seed the frontier with the root POM's directly-declared deps.
    enqueue_direct_deps(&mut state, &root.pom.dependencies, opts)?;

    // BFS loop. Each iteration dequeues exactly one work item.
    while let Some(item) = state.queue.pop_front() {
        process_item(&mut state, source, opts, item).await?;
    }

    let mut graph = state.finish();
    if let Some(s) = opts.oreq.as_ref() {
        graph.oreq_stats = s.stats();
    }
    Ok(graph)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// One frontier work item. `parent_path` is the coords path through the
/// graph that led here (root is implicit; root-level deps have an empty
/// path). `parent_scope` is the effective scope of that path so we can
/// apply Maven's scope-inheritance table to this node's declared scope.
#[derive(Debug, Clone)]
struct WorkItem {
    dep: RawDependency,
    depth: u32,
    parent_path: Vec<Coords>,
    parent_scope: Scope,
    /// Exclusions accumulated along the parent path. We compare each
    /// transitive coord against this set before processing.
    exclusions: Vec<RawExclusion>,
    /// `true` iff this is a directly-declared root dep. Direct deps keep
    /// `<optional>true</optional>`; transitives are stripped per
    /// [`WalkOptions::strip_optional`].
    is_direct: bool,
}

#[derive(Default)]
struct WalkState {
    queue: VecDeque<WorkItem>,
    /// All winners, keyed by resolution identity.
    winners: HashMap<Coords, ResolvedDep>,
    /// Discovery-order list of winners (parallel to `winners` map).
    order: Vec<Coords>,
    /// Conflict audit: coord -> losing (version, depth) pairs seen
    /// during traversal.
    losers: HashMap<Coords, Vec<(String, u32)>>,
    warnings: Vec<SpecWarning>,
    /// Subtree-pruning state. Disabled when
    /// [`WalkOptions::enable_skipper`] = `false`.
    skipper: SkipperState,
}

impl WalkState {
    fn new(enable_skipper: bool) -> Self {
        Self {
            skipper: if enable_skipper {
                SkipperState::new()
            } else {
                SkipperState::disabled()
            },
            ..Self::default()
        }
    }

    fn finish(self) -> ResolvedGraph {
        let WalkState {
            winners,
            order,
            losers,
            warnings,
            skipper,
            ..
        } = self;

        let resolved: Vec<ResolvedDep> = order
            .iter()
            .filter_map(|c| winners.get(c).cloned())
            .collect();

        let winners_btree: BTreeMap<Coords, ResolvedDep> = winners.into_iter().collect();

        let mut audit: Vec<AuditEntry> = winners_btree
            .values()
            .map(|w| AuditEntry {
                coords: w.coords.clone(),
                winning_version: w.version.clone(),
                winning_depth: w.depth,
                also_seen_at: losers.get(&w.coords).cloned().unwrap_or_default(),
            })
            .collect();
        audit.sort_by(|a, b| a.coords.cmp(&b.coords));

        ResolvedGraph {
            resolved,
            winners: winners_btree,
            warnings,
            audit,
            skipper_stats: skipper.into_stats(),
            oreq_stats: OreqStats::default(),
        }
    }
}

/// Enqueue every direct (root-level) dep at depth 1, in declaration order.
fn enqueue_direct_deps(
    state: &mut WalkState,
    deps: &[RawDependency],
    _opts: &WalkOptions,
) -> Result<(), WalkError> {
    for d in deps {
        let scope = Scope::parse(d.scope.as_deref());
        // `<scope>import</scope>` only makes sense in depMgt; if it
        // appears on a plain `<dependency>`, skip it (Maven does the same).
        if scope == Scope::Import {
            continue;
        }
        state.queue.push_back(WorkItem {
            dep: d.clone(),
            depth: 1,
            parent_path: Vec::new(),
            // Direct deps inherit "from" Compile (the implicit root scope).
            parent_scope: Scope::Compile,
            exclusions: Vec::new(),
            is_direct: true,
        });
    }
    Ok(())
}

async fn process_item<S: MetadataSource + ?Sized>(
    state: &mut WalkState,
    source: &S,
    opts: &WalkOptions,
    item: WorkItem,
) -> Result<(), WalkError> {
    let WorkItem {
        dep,
        depth,
        parent_path,
        parent_scope,
        exclusions,
        is_direct,
    } = item;

    // 0. Build the coord identity for this dep.
    let coords = match Coords::new(&dep.group_id, &dep.artifact_id) {
        Ok(c) => c,
        Err(e) => {
            return Err(WalkError::InvalidCoords {
                detail: format!("{}:{} ({e})", dep.group_id, dep.artifact_id),
            });
        }
    };

    // 1. Exclusion check (path-accumulated).
    if matches_any_exclusion(&coords, &exclusions) {
        return Ok(());
    }

    // SKIPPER SEAM (M2.1 Task 3): consult the path-pruning skipper.
    //
    // The skipper runs BEFORE nearest-wins so it can short-circuit
    // version resolution and POM fetches that nearest-wins would
    // otherwise also prune — and so the leaf cache (MRESOLVER-256)
    // can fire on repeated leaf visits, saving a `fetch_pom` call
    // that nearest-wins doesn't currently avoid in every case.
    //
    // The skipper is correctness-safe: when it returns `Skip`, the
    // walker simply returns Ok(()) — equivalent to the nearest-wins
    // skip for already-resolved cases, or to "we know there are no
    // transitives" for the known-leaf case. The walk's `winners`
    // map is therefore identical to a walk with the skipper disabled.
    let path_exclusions = ExclusionSet::from_raw(&exclusions);
    let skip_decision = state
        .skipper
        .decide(&coords, depth, &path_exclusions, &state.winners);
    if let SkipDecision::Skip { .. } = skip_decision {
        // Preserve audit semantics: when the skipper short-circuits a
        // candidate that would otherwise be recorded as a "loser" by
        // the nearest-wins check below, append a loser entry now. We
        // use the raw declared version (pre-spec-resolution) because
        // resolving here would defeat the whole point of the skip.
        if state.winners.contains_key(&coords) {
            let raw_version = dep
                .version
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("<unresolved>")
                .to_string();
            state
                .losers
                .entry(coords.clone())
                .or_default()
                .push((raw_version, depth));
        }
        return Ok(());
    }

    // 2. Effective scope.
    let declared_scope = Scope::parse(dep.scope.as_deref());
    let effective_scope = if is_direct {
        // Root-level: declared scope is the effective scope, except Import
        // which we already filtered.
        if declared_scope == Scope::Import {
            return Ok(());
        }
        declared_scope
    } else {
        match Scope::inherit(parent_scope, declared_scope) {
            Some(s) => s,
            None => return Ok(()),
        }
    };

    // Scope filter (output-side).
    if !scope_included(effective_scope, &opts.include_scopes) {
        // Even when filtered from output, we still record the win and
        // expand transitives — Maven's filtering happens at consumption,
        // not at traversal. But we DO expand transitives only if scope
        // inheritance allowed propagation. For `system` scope, inherit
        // already returns None on transitives, so we never recurse.
    }

    // 3. Optional check.
    let optional = parse_bool(dep.optional.as_deref());
    if optional && !is_direct && opts.strip_optional {
        return Ok(());
    }

    // 4. Version resolution.
    let raw_version = match dep.version.as_deref() {
        Some(v) if !v.trim().is_empty() => v.to_string(),
        _ => {
            return Err(WalkError::MissingVersion {
                coords: coords.to_string(),
            });
        }
    };
    let spec = VersionSpec::parse(&raw_version).map_err(|e| WalkError::InvalidSpec {
        coords: coords.to_string(),
        spec: raw_version.clone(),
        detail: spec_parse_error_detail(&e),
    })?;

    let resolved_version = resolve_spec(&spec, &coords, source, &mut state.warnings, opts).await?;

    // SNAPSHOT timestamp resolution. If the resolved version is a
    // `-SNAPSHOT`, ask the source for its timestamped publish so we
    // fetch the actual POM. The walker still keys conflict resolution
    // on the `-SNAPSHOT` form (that's the resolution identity); only
    // the upstream fetch needs the timestamped value.
    let fetch_version =
        resolve_snapshot_fetch_version(&resolved_version, &coords, &dep, source).await?;

    // 5. Nearest-wins check. If a winner exists at shallower-or-equal
    // depth, this node loses; record in audit and skip recursion.
    if let Some(existing) = state.winners.get(&coords) {
        if existing.depth <= depth {
            state
                .losers
                .entry(coords.clone())
                .or_default()
                .push((resolved_version, depth));
            return Ok(());
        }
        // existing.depth > depth: shouldn't happen under strict BFS, but
        // be defensive — we'll overwrite below.
        // Record the displaced previous winner as a loser.
        state
            .losers
            .entry(coords.clone())
            .or_default()
            .push((existing.version.clone(), existing.depth));
    }

    // Note: the SKIPPER SEAM is now earlier in this function (right
    // after the exclusion check). By the time we reach this point the
    // skipper has already said Walk — we're committed to recording
    // this node as a winner and (if it's not a system-scope leaf)
    // walking its transitives.
    let winning_path = {
        let mut p = parent_path.clone();
        p.push(coords.clone());
        p
    };
    let dep_record = ResolvedDep {
        coords: coords.clone(),
        version: resolved_version.clone(),
        scope: effective_scope,
        classifier: dep.classifier.clone(),
        type_: dep.r#type.clone().unwrap_or_else(|| "jar".to_string()),
        optional,
        depth,
        winning_path: winning_path.clone(),
    };
    if !state.winners.contains_key(&coords) {
        state.order.push(coords.clone());
    }
    state.winners.insert(coords.clone(), dep_record);

    // 7. Expand transitives. Only if effective_scope allows propagation
    // (Scope::inherit covers this); system scope is non-transitive, and
    // we don't fetch for that.
    if effective_scope == Scope::System {
        // System scope is a terminal node — feed the skipper so future
        // visits skip the (no-op) re-walk.
        state
            .skipper
            .record_visit(coords.clone(), path_exclusions.clone(), true);
        return Ok(());
    }

    // Fetch + resolve the child POM. A missing transitive POM is a hard
    // error — surface it. For SNAPSHOTs, fetch_version may be a
    // timestamped publish like `1.0.0-20240101.123456-7`.
    //
    // O-REQ-05 fast path: if we already resolved an effective POM for
    // this (coord, version) in-session, return it without re-fetching
    // the raw POM and without re-running resolve_pom. The effective-POM
    // hit also avoids a raw-POM fetch — credit O-REQ-04 too (the two
    // optimizations are semantically nested: an effective-POM hit is a
    // strict superset of a raw-POM hit). EFF-LINK:
    // docs/efficiency/findings/EFF-2026-007.md
    let resolved_child = if let Some(session) = opts.oreq.as_ref() {
        if let Some(r) = session.lookup_effective_pom(&coords, &fetch_version) {
            // Treat the effective-POM hit as also avoiding the underlying
            // raw POM fetch — bump the O-REQ-04 counter by inserting an
            // explicit lookup attempt that hits the deposited raw-POM
            // cache (the walker always deposits both caches together on
            // first resolve, so this lookup is guaranteed to hit when the
            // effective-POM lookup hit).
            // EFF-LINK: docs/efficiency/findings/EFF-2026-006.md
            let _ = session.lookup_parent_pom(&coords, &fetch_version);
            r
        } else {
            // O-REQ-04 fast path inside the miss branch: the raw POM
            // may have been fetched earlier this session (e.g. a
            // shared parent). EFF-LINK: docs/efficiency/findings/EFF-2026-006.md
            let (raw_pom, _origin) =
                fetch_pom_via_session(source, &coords, &fetch_version, opts).await?;
            let r = resolve_child_pom(source, &coords, &fetch_version, raw_pom, opts).await?;
            session.deposit_effective_pom(&coords, &fetch_version, r.clone());
            r
        }
    } else {
        let (raw_pom, _origin) = source.fetch_pom(&coords, &fetch_version).await?;
        // Async-prefetch the parent chain into a map, then run the
        // sync `resolve_pom` against a `MapParentResolver`. The old
        // path used a `NoParentResolver` stub and silently dropped any
        // transitive subtree rooted at a POM declaring `<parent>`,
        // which under-counted Maven Central closures (e.g. the entire
        // jackson-databind / logback-classic subtrees of Spring Boot).
        resolve_child_pom(source, &coords, &fetch_version, raw_pom, opts).await?
    };

    let child_depth = depth.saturating_add(1);
    // Merge our exclusions: parent's exclusions + this dep's exclusions.
    let merged_exclusions: Vec<RawExclusion> = {
        let mut v = exclusions.clone();
        v.extend(dep.exclusions.iter().cloned());
        v
    };

    let mut enqueued_any = false;
    for child in &resolved_child.pom.dependencies {
        let child_scope = Scope::parse(child.scope.as_deref());
        // Skip depMgt-only scope on transitives.
        if child_scope == Scope::Import {
            continue;
        }
        state.queue.push_back(WorkItem {
            dep: child.clone(),
            depth: child_depth,
            parent_path: winning_path.clone(),
            parent_scope: effective_scope,
            exclusions: merged_exclusions.clone(),
            is_direct: false,
        });
        enqueued_any = true;
    }

    // Feed the skipper: was this a leaf (no transitives enqueued)?
    let was_leaf = !enqueued_any;
    state
        .skipper
        .record_visit(coords.clone(), path_exclusions.clone(), was_leaf);

    Ok(())
}

/// `<exclusion>` matching uses Maven's wildcard rules: `*` matches any
/// group / artifact.
fn matches_any_exclusion(coords: &Coords, exclusions: &[RawExclusion]) -> bool {
    exclusions.iter().any(|x| {
        (x.group_id == "*" || x.group_id == coords.group)
            && (x.artifact_id == "*" || x.artifact_id == coords.artifact)
    })
}

fn scope_included(scope: Scope, included: &BTreeSet<Scope>) -> bool {
    if included.is_empty() {
        scope != Scope::Import
    } else {
        included.contains(&scope)
    }
}

fn parse_bool(s: Option<&str>) -> bool {
    matches!(s.map(str::trim), Some("true"))
}

fn spec_parse_error_detail(e: &SpecParseError) -> String {
    e.to_string()
}

/// Resolve a [`VersionSpec`] to a concrete version string, emitting
/// warnings as appropriate.
async fn resolve_spec<S: MetadataSource + ?Sized>(
    spec: &VersionSpec,
    coords: &ResolveKey,
    source: &S,
    warnings: &mut Vec<SpecWarning>,
    opts: &WalkOptions,
) -> Result<String, WalkError> {
    match spec {
        VersionSpec::Soft(v) => {
            // O-REQ-03: in frozen-lockfile mode, the pin (if any)
            // is authoritative regardless of what's declared inline.
            // EFF-LINK: docs/efficiency/findings/EFF-2026-005.md
            if let Some(session) = opts.oreq.as_ref() {
                if let Some(pin) = session.frozen_pin(coords) {
                    session.record_frozen_skip();
                    return Ok(pin);
                }
            }
            Ok(v.clone())
        }
        VersionSpec::Hard(intervals) => {
            // O-REQ-03: under frozen-lockfile mode, if the pin lies in
            // one of the requested intervals, skip the metadata fetch
            // entirely. EFF-LINK: docs/efficiency/findings/EFF-2026-005.md
            if let Some(session) = opts.oreq.as_ref() {
                if let Some(pin) = session.frozen_pin(coords) {
                    let parsed = Version::parse(&pin);
                    if intervals.iter().any(|iv| interval_contains(iv, &parsed)) {
                        session.record_frozen_skip();
                        return Ok(pin);
                    }
                }
            }
            // For T2, hard ranges are reported as "pick the largest in-range
            // version we know about, else surface the first interval's
            // lower bound as a soft preference." We can't enumerate all
            // versions without calling fetch_metadata, so we keep it
            // simple and fall back to fetch_metadata when needed.
            let md = fetch_metadata_via_session(source, coords, opts).await?;
            let in_range: Vec<&String> = md
                .versions
                .iter()
                .filter(|v| {
                    let parsed = Version::parse(v);
                    intervals.iter().any(|iv| interval_contains(iv, &parsed))
                })
                .collect();
            if let Some(picked) = in_range
                .iter()
                .max_by(|a, b| Version::parse(a).cmp(&Version::parse(b)))
            {
                Ok((*picked).clone())
            } else {
                Err(WalkError::NoMetaVersionCandidate {
                    coords: coords.to_string(),
                    spec: format!("{spec:?}"),
                })
            }
        }
        VersionSpec::Latest => {
            // O-REQ-03: frozen pin wins over LATEST.
            if let Some(session) = opts.oreq.as_ref() {
                if let Some(pin) = session.frozen_pin(coords) {
                    session.record_frozen_skip();
                    warnings.push(SpecWarning::LatestUsed {
                        coords: coords.to_string(),
                        resolved_to: pin.clone(),
                    });
                    return Ok(pin);
                }
            }
            let md = fetch_metadata_via_session(source, coords, opts).await?;
            let picked =
                md.versions
                    .last()
                    .cloned()
                    .ok_or_else(|| WalkError::NoMetaVersionCandidate {
                        coords: coords.to_string(),
                        spec: "LATEST".to_string(),
                    })?;
            warnings.push(SpecWarning::LatestUsed {
                coords: coords.to_string(),
                resolved_to: picked.clone(),
            });
            Ok(picked)
        }
        VersionSpec::Release => {
            // O-REQ-03: frozen pin wins over RELEASE.
            if let Some(session) = opts.oreq.as_ref() {
                if let Some(pin) = session.frozen_pin(coords) {
                    session.record_frozen_skip();
                    warnings.push(SpecWarning::ReleaseUsed {
                        coords: coords.to_string(),
                        resolved_to: pin.clone(),
                    });
                    return Ok(pin);
                }
            }
            let md = fetch_metadata_via_session(source, coords, opts).await?;
            let picked = md
                .versions
                .iter()
                .rev()
                .find(|v| !v.ends_with("-SNAPSHOT"))
                .cloned()
                .ok_or_else(|| WalkError::NoMetaVersionCandidate {
                    coords: coords.to_string(),
                    spec: "RELEASE".to_string(),
                })?;
            warnings.push(SpecWarning::ReleaseUsed {
                coords: coords.to_string(),
                resolved_to: picked.clone(),
            });
            Ok(picked)
        }
    }
}

/// Fetch `maven-metadata.xml` for `coords`, consulting the
/// [`OreqSession`] in-session cache first when one is configured.
///
/// O-REQ-01: a hit in the session cache returns the cached
/// [`crate::source::GaMetadata`] without touching `source`, and
/// bumps the O-REQ-01 counter.
/// EFF-LINK: docs/efficiency/findings/EFF-2026-001.md
///
/// O-REQ-02: on a miss + successful fetch, the [`FetchOrigin`]
/// reported by `source` bumps the O-REQ-02 counter when it's a
/// local-cache origin (`Disk` / `InMemory`) — i.e. the cache layer
/// served the byte payload via a conditional revalidation rather
/// than a fresh upstream body transfer.
/// EFF-LINK: docs/efficiency/findings/EFF-2026-004.md
async fn fetch_metadata_via_session<S: MetadataSource + ?Sized>(
    source: &S,
    coords: &ResolveKey,
    opts: &WalkOptions,
) -> Result<crate::source::GaMetadata, WalkError> {
    if let Some(session) = opts.oreq.as_ref() {
        let key = MetadataKey {
            repo: opts.repo_id.clone(),
            coords: coords.clone(),
        };
        if let Some(md) = session.lookup_metadata(&key) {
            return Ok(md);
        }
        let (md, origin) = source.fetch_metadata(coords).await?;
        session.deposit_metadata(key, md.clone(), origin);
        Ok(md)
    } else {
        let (md, _) = source.fetch_metadata(coords).await?;
        Ok(md)
    }
}

/// Fetch a POM via the configured [`MetadataSource`], consulting
/// the [`OreqSession`] in-session raw-POM cache first when one is
/// configured.
///
/// O-REQ-04: a hit returns the cached [`RawPom`] without going to
/// the source, and bumps the O-REQ-04 counter. This catches the
/// "shared parent POM across sibling modules" pattern at the heart
/// of the §18.3 O-REQ-04 description.
/// EFF-LINK: docs/efficiency/findings/EFF-2026-006.md
async fn fetch_pom_via_session<S: MetadataSource + ?Sized>(
    source: &S,
    coords: &ResolveKey,
    version: &str,
    opts: &WalkOptions,
) -> Result<(RawPom, crate::source::FetchOrigin), WalkError> {
    if let Some(session) = opts.oreq.as_ref() {
        if let Some(pom) = session.lookup_parent_pom(coords, version) {
            return Ok((pom, crate::source::FetchOrigin::InMemory));
        }
        let (pom, origin) = source.fetch_pom(coords, version).await?;
        session.deposit_parent_pom(coords, version, pom.clone());
        Ok((pom, origin))
    } else {
        Ok(source.fetch_pom(coords, version).await?)
    }
}

/// If `version` is a SNAPSHOT, consult [`MetadataSource::fetch_snapshot_info`]
/// to find the timestamped publish to actually fetch. For non-SNAPSHOT
/// versions, returns `version` unchanged.
///
/// Failures to fetch snapshot info are reported as
/// [`WalkError::SnapshotResolution`] so the caller can distinguish
/// "no timestamped publish available" from a generic transport
/// failure. If the source returns `MetadataNotFound` (the trait's
/// default impl), we fall back to using the `-SNAPSHOT` version
/// verbatim — this preserves the pre-snapshot-resolution behaviour
/// for sources that don't override the trait method.
async fn resolve_snapshot_fetch_version<S: MetadataSource + ?Sized>(
    version: &str,
    coords: &ResolveKey,
    dep: &RawDependency,
    source: &S,
) -> Result<String, WalkError> {
    let parsed = Version::parse(version);
    if !parsed.is_snapshot() {
        return Ok(version.to_string());
    }

    match source.fetch_snapshot_info(coords, version).await {
        Ok((info, _origin)) => {
            let extension = dep.r#type.as_deref().unwrap_or("jar");
            let classifier = dep.classifier.as_deref();
            match info.pick_version(extension, classifier) {
                Some(ts) => Ok(ts.to_string()),
                None => Err(WalkError::SnapshotResolution {
                    coords: coords.to_string(),
                    version: version.to_string(),
                    detail: format!(
                        "no snapshotVersion entry for extension={extension:?} classifier={classifier:?}"
                    ),
                }),
            }
        }
        // Default impl + sources that don't carry snapshot info: fall
        // back to the SNAPSHOT-form version. This is the pre-T5
        // behaviour and keeps the existing fixture-source tests
        // working.
        Err(MetadataError::MetadataNotFound { .. }) => Ok(version.to_string()),
        Err(e) => Err(WalkError::SnapshotResolution {
            coords: coords.to_string(),
            version: version.to_string(),
            detail: e.to_string(),
        }),
    }
}

fn interval_contains(iv: &crate::version_spec::Interval, v: &Version) -> bool {
    use crate::version_spec::Bound;
    let lower_ok = match &iv.lower {
        Bound::Unbounded => true,
        Bound::Included(b) => v >= b,
        Bound::Excluded(b) => v > b,
    };
    let upper_ok = match &iv.upper {
        Bound::Unbounded => true,
        Bound::Included(b) => v <= b,
        Bound::Excluded(b) => v < b,
    };
    lower_ok && upper_ok
}

// ---------------------------------------------------------------------------
// Child POM resolution
// ---------------------------------------------------------------------------

/// A sync [`ParentResolver`] backed by a pre-built map of the parent
/// chain. The walker async-fetches every ancestor before invoking
/// [`resolve_pom`], so by the time this resolver is consulted every
/// lookup is an in-memory `HashMap::get`. This bridges
/// [`resolve_pom`]'s sync `ParentResolver` trait to the async
/// [`MetadataSource`] API without re-entering the tokio runtime
/// recursively.
struct MapParentResolver {
    chain: HashMap<(String, String, String), RawPom>,
}

impl ParentResolver for MapParentResolver {
    fn resolve(&mut self, parent: &barista_pom::RawParent) -> Result<RawPom, String> {
        let key = (
            parent.group_id.clone(),
            parent.artifact_id.clone(),
            parent.version.clone(),
        );
        self.chain.get(&key).cloned().ok_or_else(|| {
            format!(
                "parent {}:{}:{} not in prefetched chain",
                parent.group_id, parent.artifact_id, parent.version
            )
        })
    }
}

/// Walk the parent chain rooted at `raw_pom`, async-fetching each
/// ancestor through `source` into a `(group, artifact, version) ->
/// RawPom` map keyed for [`MapParentResolver`].
///
/// Capped at [`barista_pom::MAX_CHAIN_DEPTH`]; cycle short-circuits.
/// In both cases we stop accumulating but return the partial map and
/// let [`barista_pom::build_effective`] surface a structured
/// `ChainTooDeep` / `CircularParent` diagnostic when it re-walks the
/// chain. A fetch failure mid-chain surfaces as
/// [`WalkError::ParentChainResolution`].
async fn prefetch_parent_chain<S: MetadataSource + ?Sized>(
    source: &S,
    raw_pom: &RawPom,
    opts: &WalkOptions,
) -> Result<HashMap<(String, String, String), RawPom>, WalkError> {
    let mut chain: HashMap<(String, String, String), RawPom> = HashMap::new();
    let mut current = raw_pom.parent.clone();
    let mut depth = 0usize;
    while let Some(p) = current {
        if depth >= barista_pom::MAX_CHAIN_DEPTH {
            break;
        }
        let key = (p.group_id.clone(), p.artifact_id.clone(), p.version.clone());
        if chain.contains_key(&key) {
            break;
        }
        let coords = Coords::new(&p.group_id, &p.artifact_id).map_err(|e| {
            WalkError::ParentChainResolution {
                coords: format!("{}:{}", p.group_id, p.artifact_id),
                version: p.version.clone(),
                detail: e.to_string(),
            }
        })?;
        let (parent_pom, _origin) = fetch_pom_via_session(source, &coords, &p.version, opts)
            .await
            .map_err(|e| WalkError::ParentChainResolution {
                coords: coords.to_string(),
                version: p.version.clone(),
                detail: e.to_string(),
            })?;
        current = parent_pom.parent.clone();
        chain.insert(key, parent_pom);
        depth += 1;
    }
    Ok(chain)
}

/// Maximum number of iterative-prefetch rounds when
/// [`resolve_child_pom`] discovers an unseen parent / BOM. Each round
/// strictly grows the prefetched-chain map, so the loop is bounded by
/// the size of the (g, a, v) universe; this cap is a defence-in-depth
/// guard against pathological feedback loops.
const MAX_PARENT_PREFETCH_ROUNDS: usize = 64;

/// Parse a `"group:artifact:version"` triple, as produced by
/// [`barista_pom::ResolveError`] / [`barista_pom::EffectiveError`]
/// when they surface an unresolvable coordinate.
fn parse_gav(s: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    Some((parts[0].into(), parts[1].into(), parts[2].into()))
}

/// Resolve a transitive POM's effective form, awaiting any parent or
/// BOM-import fetches via `source`.
///
/// Earlier iterations of the walker stubbed parent resolution out
/// entirely and silently truncated transitive subtrees rooted at any
/// POM declaring `<parent>` — which in Maven Central includes
/// `jackson-databind`, `logback-classic`, `log4j-to-slf4j`, and
/// effectively every artifact published from a multi-module project.
/// That under-counted Spring Boot's transitive closure by ~15% and
/// produced lockfiles that looked valid but were wrong.
///
/// Strategy: pre-fetch the parent chain into a map, then iterate.
/// [`resolve_pom`] consults [`MapParentResolver`] synchronously; if a
/// lookup misses (a previously-unseen parent in a deeper chain, or a
/// BOM import the resolver tries to splice), the resolver surfaces a
/// structured error carrying the missing `(group, artifact, version)`.
/// We async-fetch the missing artifact + its own parent chain, fold
/// the result into the map, and retry. The loop is bounded by
/// [`MAX_PARENT_PREFETCH_ROUNDS`] but typically converges in 1–4
/// rounds for real-world POMs.
async fn resolve_child_pom<S: MetadataSource + ?Sized>(
    source: &S,
    coords: &Coords,
    version: &str,
    raw: RawPom,
    opts: &WalkOptions,
) -> Result<ResolvedPom, WalkError> {
    let mut chain = prefetch_parent_chain(source, &raw, opts).await?;

    for _ in 0..MAX_PARENT_PREFETCH_ROUNDS {
        let mut r = MapParentResolver {
            chain: chain.clone(),
        };
        match resolve_pom(raw.clone(), &mut r, &opts.activation) {
            Ok(resolved) => return Ok(resolved),
            Err(barista_pom::ResolveError::Effective(
                barista_pom::EffectiveError::ParentResolution {
                    coords: missing, ..
                },
            ))
            | Err(barista_pom::ResolveError::BomImportResolution {
                coords: missing, ..
            }) => {
                let (mg, ma, mv) =
                    parse_gav(&missing).ok_or_else(|| WalkError::EffectivePomResolution {
                        coords: coords.to_string(),
                        version: version.to_string(),
                        detail: format!("unparseable missing-coord from resolver: {missing}"),
                    })?;
                let mc = Coords::new(&mg, &ma).map_err(|e| WalkError::ParentChainResolution {
                    coords: format!("{mg}:{ma}"),
                    version: mv.clone(),
                    detail: e.to_string(),
                })?;
                let (missing_pom, _origin) = fetch_pom_via_session(source, &mc, &mv, opts)
                    .await
                    .map_err(|e| WalkError::ParentChainResolution {
                        coords: mc.to_string(),
                        version: mv.clone(),
                        detail: e.to_string(),
                    })?;
                // Also pre-fetch the missing artifact's own parent
                // chain so the next round doesn't have to recurse for
                // every ancestor individually.
                let extra = prefetch_parent_chain(source, &missing_pom, opts).await?;
                chain.insert((mg, ma, mv), missing_pom);
                chain.extend(extra);
            }
            Err(e) => {
                return Err(WalkError::EffectivePomResolution {
                    coords: coords.to_string(),
                    version: version.to_string(),
                    detail: e.to_string(),
                });
            }
        }
    }
    Err(WalkError::EffectivePomResolution {
        coords: coords.to_string(),
        version: version.to_string(),
        detail: format!(
            "parent-prefetch fixed point did not converge after {MAX_PARENT_PREFETCH_ROUNDS} rounds"
        ),
    })
}

// ---------------------------------------------------------------------------
// Convenience: walk with a NullMetadataSource shim that returns a fixture.
// ---------------------------------------------------------------------------

/// Hand-rolled fixture source: a `HashMap` of `(coords, version) -> RawPom`
/// pretending to be a metadata source. Used by the unit tests below and
/// available to downstream crates for integration-style tests.
#[derive(Debug, Default)]
pub struct FixtureSource {
    poms: HashMap<(Coords, String), RawPom>,
    metadata: HashMap<Coords, Vec<String>>,
}

impl FixtureSource {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_pom(&mut self, coords: Coords, version: impl Into<String>, pom: RawPom) {
        let v = version.into();
        self.metadata
            .entry(coords.clone())
            .or_default()
            .push(v.clone());
        self.poms.insert((coords, v), pom);
    }
}

#[async_trait]
impl MetadataSource for FixtureSource {
    async fn fetch_pom(
        &self,
        coords: &ResolveKey,
        version: &str,
    ) -> Result<(RawPom, crate::source::FetchOrigin), MetadataError> {
        match self.poms.get(&(coords.clone(), version.to_string())) {
            Some(p) => Ok((p.clone(), crate::source::FetchOrigin::Fixture)),
            None => Err(MetadataError::NotFound {
                coords: coords.to_string(),
                version: version.to_string(),
            }),
        }
    }

    async fn fetch_metadata(
        &self,
        coords: &ResolveKey,
    ) -> Result<(crate::source::GaMetadata, crate::source::FetchOrigin), MetadataError> {
        match self.metadata.get(coords) {
            Some(v) => Ok((
                crate::source::GaMetadata {
                    coords: coords.clone(),
                    versions: v.clone(),
                    latest_snapshot_timestamp: None,
                    last_updated: None,
                },
                crate::source::FetchOrigin::Fixture,
            )),
            None => Err(MetadataError::MetadataNotFound {
                coords: coords.to_string(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use barista_pom::{EffectivePom, Properties, RawDependency, RawExclusion, ResolvedPom};

    // ---- helpers ----------------------------------------------------------

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

    fn dep_with(
        g: &str,
        a: &str,
        v: Option<&str>,
        scope: Option<&str>,
        optional: Option<&str>,
        exclusions: Vec<(&str, &str)>,
    ) -> RawDependency {
        RawDependency {
            group_id: g.into(),
            artifact_id: a.into(),
            version: v.map(String::from),
            scope: scope.map(String::from),
            optional: optional.map(String::from),
            exclusions: exclusions
                .into_iter()
                .map(|(g, a)| RawExclusion {
                    group_id: g.into(),
                    artifact_id: a.into(),
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

    async fn run(root: RawPom, src: &FixtureSource) -> ResolvedGraph {
        let opts = WalkOptions::default();
        walk(&resolved(root), src, &opts).await.expect("walk ok")
    }

    fn versions(g: &ResolvedGraph, group: &str, artifact: &str) -> Option<String> {
        g.winners
            .get(&co(group, artifact))
            .map(|d| d.version.clone())
    }

    // ---- 1. Diamond, nearest wins -----------------------------------------

    #[tokio::test]
    async fn diamond_nearest_wins() {
        // root -> B 1.0 -> C 1.0
        // root -> D 1.0 -> C 2.0
        // Both C's at depth 2; declaration order ties → B's C 1.0 wins.
        let mut src = FixtureSource::new();
        src.add_pom(
            co("ex", "B"),
            "1.0",
            pom("ex", "B", "1.0", vec![dep("ex", "C", "1.0")]),
        );
        src.add_pom(
            co("ex", "D"),
            "1.0",
            pom("ex", "D", "1.0", vec![dep("ex", "C", "2.0")]),
        );
        src.add_pom(co("ex", "C"), "1.0", pom("ex", "C", "1.0", vec![]));
        src.add_pom(co("ex", "C"), "2.0", pom("ex", "C", "2.0", vec![]));

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep("ex", "B", "1.0"), dep("ex", "D", "1.0")],
        );
        let g = run(root, &src).await;
        assert_eq!(versions(&g, "ex", "C"), Some("1.0".into()));
        assert_eq!(g.resolved.len(), 3); // B, D, C
    }

    // ---- 2. Cycle termination ---------------------------------------------

    #[tokio::test]
    async fn cycle_terminates() {
        // root -> A -> B -> A (cycle)
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
        let g = run(root, &src).await;
        assert_eq!(g.winners.len(), 2);
        assert!(g.winners.contains_key(&co("ex", "A")));
        assert!(g.winners.contains_key(&co("ex", "B")));
    }

    // ---- 3. Scope narrowing (parent test) ---------------------------------

    #[tokio::test]
    async fn scope_narrowing_test_parent() {
        // root -test-> A -compile-> B  =>  B effective scope = test
        let mut src = FixtureSource::new();
        src.add_pom(
            co("ex", "A"),
            "1.0",
            pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]),
        );
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with("ex", "A", Some("1.0"), Some("test"), None, vec![])],
        );
        let g = run(root, &src).await;
        let b = g.winners.get(&co("ex", "B")).unwrap();
        assert_eq!(b.scope, Scope::Test);
    }

    // ---- 4. Provided direct dep is non-transitive: its closure is dropped --

    #[tokio::test]
    async fn provided_direct_does_not_transit() {
        // Maven rule: a `provided`-scoped direct dep is kept (compile +
        // test classpath need it), but its transitive closure is NOT
        // propagated to consumers. `mvn dependency:tree` shows only the
        // direct provided node, not its descendants.
        let mut src = FixtureSource::new();
        src.add_pom(
            co("ex", "A"),
            "1.0",
            pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]),
        );
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with(
                "ex",
                "A",
                Some("1.0"),
                Some("provided"),
                None,
                vec![],
            )],
        );
        let g = run(root, &src).await;
        assert_eq!(
            g.winners.get(&co("ex", "A")).map(|d| d.scope),
            Some(Scope::Provided),
            "A is kept as a direct provided dep"
        );
        assert!(
            g.winners.get(&co("ex", "B")).is_none(),
            "B is the closure of a `provided` direct dep — must not be \
             pulled into the consumer's graph; closure = {:?}",
            g.winners.keys().collect::<Vec<_>>()
        );
    }

    // ---- 5. Optional transitive stripped ----------------------------------

    #[tokio::test]
    async fn optional_transitive_stripped() {
        let mut src = FixtureSource::new();
        // A's POM declares B as optional → B should NOT appear in output.
        src.add_pom(
            co("ex", "A"),
            "1.0",
            pom(
                "ex",
                "A",
                "1.0",
                vec![dep_with("ex", "B", Some("1.0"), None, Some("true"), vec![])],
            ),
        );
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));

        let root = pom("ex", "root", "1.0", vec![dep("ex", "A", "1.0")]);
        let g = run(root, &src).await;
        assert!(!g.winners.contains_key(&co("ex", "B")));
        assert!(g.winners.contains_key(&co("ex", "A")));
    }

    // ---- 6. Exclusion accumulates and fires -------------------------------

    #[tokio::test]
    async fn exclusion_fires_on_transitive() {
        let mut src = FixtureSource::new();
        // root -> A (excl org.foo:bar) -> B -> org.foo:bar
        src.add_pom(
            co("ex", "A"),
            "1.0",
            pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]),
        );
        src.add_pom(
            co("ex", "B"),
            "1.0",
            pom("ex", "B", "1.0", vec![dep("org.foo", "bar", "1.0")]),
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
            vec![dep_with(
                "ex",
                "A",
                Some("1.0"),
                None,
                None,
                vec![("org.foo", "bar")],
            )],
        );
        let g = run(root, &src).await;
        assert!(!g.winners.contains_key(&co("org.foo", "bar")));
        assert!(g.winners.contains_key(&co("ex", "B")));
    }

    // ---- 7. depMgt-supplied version (we test the input case: root
    //        already has versions because resolve_pom would've set them) --

    #[tokio::test]
    async fn missing_version_errors() {
        let src = FixtureSource::new();
        let mut d = dep("ex", "A", "1.0");
        d.version = None; // simulate missing version with no depMgt
        let root = pom("ex", "root", "1.0", vec![d]);
        let err = walk(&resolved(root), &src, &WalkOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, WalkError::MissingVersion { .. }));
    }

    // ---- 8. System scope does not propagate transitives -------------------

    #[tokio::test]
    async fn system_does_not_propagate() {
        let mut src = FixtureSource::new();
        src.add_pom(
            co("ex", "A"),
            "1.0",
            pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]),
        );
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with(
                "ex",
                "A",
                Some("1.0"),
                Some("system"),
                None,
                vec![],
            )],
        );
        let g = run(root, &src).await;
        // A is recorded (direct dep), but B (transitive via system) is not.
        assert!(g.winners.contains_key(&co("ex", "A")));
        assert!(!g.winners.contains_key(&co("ex", "B")));
    }

    // ---- 9. Direct wins over transitive (depth 1 vs depth 2) -------------

    #[tokio::test]
    async fn direct_wins_over_transitive() {
        let mut src = FixtureSource::new();
        // root declares C 1.0 directly; via A it'd get C 2.0.
        src.add_pom(
            co("ex", "A"),
            "1.0",
            pom("ex", "A", "1.0", vec![dep("ex", "C", "2.0")]),
        );
        src.add_pom(co("ex", "C"), "1.0", pom("ex", "C", "1.0", vec![]));
        src.add_pom(co("ex", "C"), "2.0", pom("ex", "C", "2.0", vec![]));

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep("ex", "A", "1.0"), dep("ex", "C", "1.0")],
        );
        let g = run(root, &src).await;
        assert_eq!(versions(&g, "ex", "C"), Some("1.0".into()));
    }

    // ---- 10. Same depth, declaration order tie-break ---------------------

    #[tokio::test]
    async fn same_depth_declaration_order_wins() {
        let mut src = FixtureSource::new();
        // Both roots declare C at the same depth (1, direct).
        src.add_pom(co("ex", "C"), "1.0", pom("ex", "C", "1.0", vec![]));
        src.add_pom(co("ex", "C"), "2.0", pom("ex", "C", "2.0", vec![]));

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep("ex", "C", "1.0"), dep("ex", "C", "2.0")],
        );
        let g = run(root, &src).await;
        assert_eq!(versions(&g, "ex", "C"), Some("1.0".into()));
        let audit = g
            .audit
            .iter()
            .find(|a| a.coords == co("ex", "C"))
            .expect("C audit");
        assert!(!audit.also_seen_at.is_empty());
    }

    // ---- 11. Optional DIRECT dep is kept ----------------------------------

    #[tokio::test]
    async fn optional_direct_is_kept() {
        let src = FixtureSource::new();
        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with("ex", "A", Some("1.0"), None, Some("true"), vec![])],
        );
        // No need to fetch A's POM because we error if we try; force a
        // version where A's POM is absent. Walker should still record A
        // as direct, then attempt expansion and... need a POM. Add one.
        let mut src = src;
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));
        let g = run(root, &src).await;
        assert!(g.winners.contains_key(&co("ex", "A")));
        let a = g.winners.get(&co("ex", "A")).unwrap();
        assert!(a.optional);
    }

    // ---- 12. Invalid version spec surfaces InvalidSpec -------------------

    #[tokio::test]
    async fn invalid_range_errors() {
        let src = FixtureSource::new();
        let mut d = dep("ex", "A", "[1.0,");
        d.version = Some("[1.0,".into()); // unmatched bracket
        let root = pom("ex", "root", "1.0", vec![d]);
        let err = walk(&resolved(root), &src, &WalkOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, WalkError::InvalidSpec { .. }));
    }

    // ---- 13. Missing version (with no depMgt) ----------------------------
    // already covered in test #7 — keep numbering aligned.

    // ---- 14. LATEST resolves and emits warning ---------------------------

    #[tokio::test]
    async fn latest_meta_version_resolved() {
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));
        src.add_pom(co("ex", "A"), "2.0", pom("ex", "A", "2.0", vec![]));

        let mut d = dep("ex", "A", "LATEST");
        d.version = Some("LATEST".into());
        let root = pom("ex", "root", "1.0", vec![d]);
        let g = walk(&resolved(root), &src, &WalkOptions::default())
            .await
            .unwrap();
        assert_eq!(versions(&g, "ex", "A"), Some("2.0".into()));
        assert!(
            g.warnings
                .iter()
                .any(|w| matches!(w, SpecWarning::LatestUsed { .. }))
        );
    }

    // ---- 15. RELEASE filters snapshots ------------------------------------

    #[tokio::test]
    async fn release_skips_snapshots() {
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));
        src.add_pom(
            co("ex", "A"),
            "2.0-SNAPSHOT",
            pom("ex", "A", "2.0-SNAPSHOT", vec![]),
        );

        let mut d = dep("ex", "A", "RELEASE");
        d.version = Some("RELEASE".into());
        let root = pom("ex", "root", "1.0", vec![d]);
        let g = walk(&resolved(root), &src, &WalkOptions::default())
            .await
            .unwrap();
        assert_eq!(versions(&g, "ex", "A"), Some("1.0".into()));
        assert!(
            g.warnings
                .iter()
                .any(|w| matches!(w, SpecWarning::ReleaseUsed { .. }))
        );
    }

    // ---- Extras ----------------------------------------------------------

    // 16. Scope inheritance table — compile -> runtime stays runtime.
    #[tokio::test]
    async fn compile_parent_runtime_transitive_stays_runtime() {
        let mut src = FixtureSource::new();
        src.add_pom(
            co("ex", "A"),
            "1.0",
            pom(
                "ex",
                "A",
                "1.0",
                vec![dep_with(
                    "ex",
                    "B",
                    Some("1.0"),
                    Some("runtime"),
                    None,
                    vec![],
                )],
            ),
        );
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));
        let root = pom("ex", "root", "1.0", vec![dep("ex", "A", "1.0")]);
        let g = run(root, &src).await;
        assert_eq!(g.winners.get(&co("ex", "B")).unwrap().scope, Scope::Runtime);
    }

    // 17. Scope inheritance: runtime parent + test transitive => omitted.
    #[tokio::test]
    async fn runtime_parent_test_transitive_omitted() {
        let mut src = FixtureSource::new();
        src.add_pom(
            co("ex", "A"),
            "1.0",
            pom(
                "ex",
                "A",
                "1.0",
                vec![dep_with("ex", "B", Some("1.0"), Some("test"), None, vec![])],
            ),
        );
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));
        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with(
                "ex",
                "A",
                Some("1.0"),
                Some("runtime"),
                None,
                vec![],
            )],
        );
        let g = run(root, &src).await;
        assert!(!g.winners.contains_key(&co("ex", "B")));
    }

    // 18. Wildcard exclusion (*:*) prunes everything under the parent.
    #[tokio::test]
    async fn wildcard_exclusion_kills_all_transitives() {
        let mut src = FixtureSource::new();
        src.add_pom(
            co("ex", "A"),
            "1.0",
            pom(
                "ex",
                "A",
                "1.0",
                vec![dep("ex", "B", "1.0"), dep("ex", "C", "1.0")],
            ),
        );
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));
        src.add_pom(co("ex", "C"), "1.0", pom("ex", "C", "1.0", vec![]));

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with(
                "ex",
                "A",
                Some("1.0"),
                None,
                None,
                vec![("*", "*")],
            )],
        );
        let g = run(root, &src).await;
        assert!(g.winners.contains_key(&co("ex", "A")));
        assert!(!g.winners.contains_key(&co("ex", "B")));
        assert!(!g.winners.contains_key(&co("ex", "C")));
    }

    // 19. Exclusion is inherited deeper down the tree.
    #[tokio::test]
    async fn exclusion_propagates_two_levels_down() {
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
            pom("ex", "C", "1.0", vec![dep("org.bad", "evil", "1.0")]),
        );
        src.add_pom(
            co("org.bad", "evil"),
            "1.0",
            pom("org.bad", "evil", "1.0", vec![]),
        );

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with(
                "ex",
                "A",
                Some("1.0"),
                None,
                None,
                vec![("org.bad", "evil")],
            )],
        );
        let g = run(root, &src).await;
        assert!(!g.winners.contains_key(&co("org.bad", "evil")));
    }

    // 20. Audit records loser version/depth.
    #[tokio::test]
    async fn audit_records_losers() {
        let mut src = FixtureSource::new();
        src.add_pom(
            co("ex", "A"),
            "1.0",
            pom("ex", "A", "1.0", vec![dep("ex", "C", "2.0")]),
        );
        src.add_pom(co("ex", "C"), "1.0", pom("ex", "C", "1.0", vec![]));
        src.add_pom(co("ex", "C"), "2.0", pom("ex", "C", "2.0", vec![]));
        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep("ex", "C", "1.0"), dep("ex", "A", "1.0")],
        );
        let g = run(root, &src).await;
        let a = g.audit.iter().find(|a| a.coords == co("ex", "C")).unwrap();
        assert_eq!(a.winning_version, "1.0");
        assert!(a.also_seen_at.iter().any(|(v, _)| v == "2.0"));
    }

    // 21. Empty graph: root with no deps.
    #[tokio::test]
    async fn empty_root_yields_empty_graph() {
        let src = FixtureSource::new();
        let root = pom("ex", "root", "1.0", vec![]);
        let g = run(root, &src).await;
        assert!(g.resolved.is_empty());
        assert!(g.winners.is_empty());
    }

    // 22. Multiple direct deps preserve declaration order.
    #[tokio::test]
    async fn direct_deps_preserve_declaration_order() {
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));
        src.add_pom(co("ex", "C"), "1.0", pom("ex", "C", "1.0", vec![]));
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
        let g = run(root, &src).await;
        let names: Vec<&str> = g
            .resolved
            .iter()
            .map(|d| d.coords.artifact.as_str())
            .collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    // 23. Hard range picks largest in-range version.
    #[tokio::test]
    async fn hard_range_picks_largest_in_range() {
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));
        src.add_pom(co("ex", "A"), "1.5", pom("ex", "A", "1.5", vec![]));
        src.add_pom(co("ex", "A"), "2.0", pom("ex", "A", "2.0", vec![]));

        let mut d = dep("ex", "A", "[1.0,2.0)");
        d.version = Some("[1.0,2.0)".into());
        let root = pom("ex", "root", "1.0", vec![d]);
        let g = run(root, &src).await;
        assert_eq!(versions(&g, "ex", "A"), Some("1.5".into()));
    }

    // 24. Classifier preserved in ResolvedDep.
    #[tokio::test]
    async fn classifier_preserved() {
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));
        let mut d = dep("ex", "A", "1.0");
        d.classifier = Some("sources".into());
        let root = pom("ex", "root", "1.0", vec![d]);
        let g = run(root, &src).await;
        assert_eq!(
            g.winners.get(&co("ex", "A")).unwrap().classifier.as_deref(),
            Some("sources")
        );
    }

    // 25. Type defaults to "jar" when unspecified.
    #[tokio::test]
    async fn type_defaults_to_jar() {
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));
        let root = pom("ex", "root", "1.0", vec![dep("ex", "A", "1.0")]);
        let g = run(root, &src).await;
        assert_eq!(g.winners.get(&co("ex", "A")).unwrap().type_, "jar");
    }

    // 26. Type passes through when set.
    #[tokio::test]
    async fn type_passes_through() {
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));
        let mut d = dep("ex", "A", "1.0");
        d.r#type = Some("pom".into());
        let root = pom("ex", "root", "1.0", vec![d]);
        let g = run(root, &src).await;
        assert_eq!(g.winners.get(&co("ex", "A")).unwrap().type_, "pom");
    }

    // 27. winning_path records the path the winner took.
    #[tokio::test]
    async fn winning_path_records_traversal() {
        let mut src = FixtureSource::new();
        src.add_pom(
            co("ex", "A"),
            "1.0",
            pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]),
        );
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));
        let root = pom("ex", "root", "1.0", vec![dep("ex", "A", "1.0")]);
        let g = run(root, &src).await;
        let b = g.winners.get(&co("ex", "B")).unwrap();
        assert_eq!(b.winning_path, vec![co("ex", "A"), co("ex", "B")]);
        assert_eq!(b.depth, 2);
    }

    // 28. Missing transitive POM is a hard error.
    #[tokio::test]
    async fn missing_transitive_pom_errors() {
        let mut src = FixtureSource::new();
        // Root declares A, but we don't add A's POM to the source.
        src.add_pom(co("ex", "root"), "1.0", pom("ex", "root", "1.0", vec![]));
        // intentionally omit A
        let root = pom("ex", "root", "1.0", vec![dep("ex", "A", "1.0")]);
        let err = walk(&resolved(root), &src, &WalkOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            WalkError::Metadata(MetadataError::NotFound { .. })
        ));
    }

    // 29. Import scope on direct dep is silently dropped.
    #[tokio::test]
    async fn import_scope_on_direct_dep_dropped() {
        let src = FixtureSource::new();
        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with(
                "ex",
                "A",
                Some("1.0"),
                Some("import"),
                None,
                vec![],
            )],
        );
        let g = run(root, &src).await;
        assert!(g.winners.is_empty());
    }

    // 30. Scope::inherit fully covers Maven's table — direct enum test.
    //
    // Maven's transitive-scope rule: any declared `provided` / `test`
    // / `system` / `import` is dropped on a transitive (rows below
    // marked `None` in the declared-side columns), and any parent at
    // `provided` / `system` / `import` is non-transitive (rows below
    // marked `None` on the parent-side rows). Surviving cases are the
    // (compile|runtime|test parent) × (compile|runtime declared) grid
    // with the usual mediation: runtime widens, test stickies, etc.
    #[test]
    fn scope_inherit_table_smoke() {
        use Scope::*;
        // Surviving (parent, declared) pairs.
        assert_eq!(Scope::inherit(Compile, Compile), Some(Compile));
        assert_eq!(Scope::inherit(Compile, Runtime), Some(Runtime));
        assert_eq!(Scope::inherit(Runtime, Compile), Some(Runtime));
        assert_eq!(Scope::inherit(Runtime, Runtime), Some(Runtime));
        assert_eq!(Scope::inherit(Test, Compile), Some(Test));
        assert_eq!(Scope::inherit(Test, Runtime), Some(Test));
        // Declared = provided / test / system / import → drop, always.
        // This is the row whose old behavior produced the spurious
        // `org.osgi.core`, `error_prone_annotations`, `jsr305` etc.
        // transitives in real-world Spring Boot lockfiles.
        assert_eq!(Scope::inherit(Compile, Provided), None);
        assert_eq!(Scope::inherit(Compile, Test), None);
        assert_eq!(Scope::inherit(Compile, System), None);
        assert_eq!(Scope::inherit(Compile, Import), None);
        assert_eq!(Scope::inherit(Runtime, Provided), None);
        assert_eq!(Scope::inherit(Runtime, Test), None);
        assert_eq!(Scope::inherit(Test, Provided), None);
        assert_eq!(Scope::inherit(Test, Test), None);
        // Parent = provided / system / import → drop entire subtree.
        assert_eq!(Scope::inherit(Provided, Compile), None);
        assert_eq!(Scope::inherit(Provided, Runtime), None);
        assert_eq!(Scope::inherit(Provided, Provided), None);
        assert_eq!(Scope::inherit(System, Compile), None);
        assert_eq!(Scope::inherit(Import, Compile), None);
    }

    // 32 (sibling of #30). Walker-integration regression: a transitive
    // POM that declares a `provided`-scope dep — the
    // `log4j-to-slf4j → org.osgi.core` shape — must not pull that dep
    // into the closure. Pre-fix, `Scope::inherit(Compile, Provided)`
    // returned `Some(Provided)` and the walker recorded a winner.
    #[tokio::test]
    async fn provided_scope_transitive_is_dropped() {
        let mut src = FixtureSource::new();

        // lib-a is at compile scope in the root; its POM declares a
        // single transitive at provided scope.
        let lib_a_pom = {
            let mut p = pom("ex", "lib-a", "1.0", vec![]);
            p.dependencies.push(RawDependency {
                group_id: "ex".into(),
                artifact_id: "system-api".into(),
                version: Some("1.0".into()),
                scope: Some("provided".into()),
                ..RawDependency::default()
            });
            p
        };
        src.add_pom(co("ex", "lib-a"), "1.0", lib_a_pom);
        src.add_pom(
            co("ex", "system-api"),
            "1.0",
            pom("ex", "system-api", "1.0", vec![]),
        );

        let root = pom("ex", "root", "1.0", vec![dep("ex", "lib-a", "1.0")]);
        let g = run(root, &src).await;

        assert_eq!(versions(&g, "ex", "lib-a"), Some("1.0".into()));
        assert!(
            versions(&g, "ex", "system-api").is_none(),
            "provided-scope transitive must be dropped; closure = {:?}",
            g.winners.keys().collect::<Vec<_>>()
        );
    }

    // 31. Scope::parse defaults to Compile on unknown.
    #[test]
    fn scope_parse_defaults() {
        assert_eq!(Scope::parse(None), Scope::Compile);
        assert_eq!(Scope::parse(Some("")), Scope::Compile);
        assert_eq!(Scope::parse(Some("compile")), Scope::Compile);
        assert_eq!(Scope::parse(Some("provided")), Scope::Provided);
        assert_eq!(Scope::parse(Some("runtime")), Scope::Runtime);
        assert_eq!(Scope::parse(Some("test")), Scope::Test);
        assert_eq!(Scope::parse(Some("system")), Scope::System);
        assert_eq!(Scope::parse(Some("import")), Scope::Import);
        assert_eq!(Scope::parse(Some("garbage")), Scope::Compile);
    }

    // 32. Regression: a transitive POM declaring `<parent>` must have
    //     its own `<dependencies>` walked, not silently dropped.
    //     This mirrors the jackson-databind / logback-classic shape
    //     that under-counted Spring Boot closures by ~15% before the
    //     walker grew `MapParentResolver` + `prefetch_parent_chain`.
    #[tokio::test]
    async fn parent_bearing_transitive_recurses_into_its_deps() {
        let mut src = FixtureSource::new();

        // Parent POM that contributes no deps to its child — exists
        // only so lib-a's `<parent>` declaration resolves.
        src.add_pom(
            co("ex", "lib-parent"),
            "1.0",
            pom("ex", "lib-parent", "1.0", vec![]),
        );

        // lib-a: declares `<parent>lib-parent</parent>` *and* a
        // compile-scope dep on lib-b. The pre-fix walker silently
        // returned Ok(()) here because `NoParentResolver` errored on
        // the parent ask, so lib-b was never enqueued.
        let mut lib_a = pom("ex", "lib-a", "1.0", vec![dep("ex", "lib-b", "1.0")]);
        lib_a.parent = Some(barista_pom::RawParent {
            group_id: "ex".into(),
            artifact_id: "lib-parent".into(),
            version: "1.0".into(),
            relative_path: None,
        });
        src.add_pom(co("ex", "lib-a"), "1.0", lib_a);

        // lib-b: leaf, no transitives.
        src.add_pom(co("ex", "lib-b"), "1.0", pom("ex", "lib-b", "1.0", vec![]));

        let root = pom("ex", "root", "1.0", vec![dep("ex", "lib-a", "1.0")]);
        let g = run(root, &src).await;

        assert_eq!(versions(&g, "ex", "lib-a"), Some("1.0".into()));
        assert_eq!(
            versions(&g, "ex", "lib-b"),
            Some("1.0".into()),
            "lib-b should be in winners — the walker must recurse into \
             a transitive POM's <dependencies> even when that POM \
             declares a <parent>"
        );
    }

    // 33. Companion to #32: when the parent itself cannot be fetched,
    //     surface the failure loudly instead of silently dropping the
    //     subtree. The pre-fix walker would have produced a wrong but
    //     "successful" lockfile here.
    #[tokio::test]
    async fn unresolvable_parent_surfaces_walk_error() {
        let mut src = FixtureSource::new();

        // lib-a declares a parent that is NOT in the fixture.
        let mut lib_a = pom("ex", "lib-a", "1.0", vec![]);
        lib_a.parent = Some(barista_pom::RawParent {
            group_id: "ex".into(),
            artifact_id: "missing-parent".into(),
            version: "1.0".into(),
            relative_path: None,
        });
        src.add_pom(co("ex", "lib-a"), "1.0", lib_a);

        let root = pom("ex", "root", "1.0", vec![dep("ex", "lib-a", "1.0")]);
        let opts = WalkOptions::default();
        let err = walk(&resolved(root), &src, &opts)
            .await
            .expect_err("walk must surface the parent fetch failure");
        assert!(
            matches!(err, WalkError::ParentChainResolution { .. }),
            "expected ParentChainResolution, got {err:?}"
        );
    }
}
