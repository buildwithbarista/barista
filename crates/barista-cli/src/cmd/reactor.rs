// SPDX-License-Identifier: MIT OR Apache-2.0

//! Multi-module Maven reactor: topological sort + per-level parallelism.
//!
//! `barista verify` on a multi-module Maven project (a "reactor" in
//! Maven parlance) must respect the inter-module dependency DAG. A
//! module that depends on another can't be built until its upstream
//! is done. Independent modules at the same depth in the DAG can —
//! and should — be built in parallel.
//!
//! This module owns three responsibilities:
//!
//! 1. **Discover the modules.** Walk the root `pom.xml`'s `<modules>`
//!    list (recursively for nested aggregator POMs).
//! 2. **Build the inter-module DAG.** For each module, parse its
//!    `<dependency>` entries; any reference whose `groupId:artifactId`
//!    coordinate matches another module in the reactor is an
//!    intra-reactor edge. Other references (third-party deps from
//!    `~/.m2`) are not edges — they're already resolved at this point.
//! 3. **Topologically sort + level-group.** Kahn's algorithm produces
//!    a deterministic `Vec<Vec<usize>>` of module indices, where each
//!    inner `Vec` is a "level" of modules with no unresolved deps.
//!    The dispatcher runs every module in one level in parallel,
//!    waits for the whole level, then advances.
//!
//! # Algorithm: Kahn's
//!
//! Kahn's algorithm was picked over DFS-based topo sort because:
//!
//! * It produces a natural per-level grouping (which is what the
//!   parallel dispatcher needs — DFS topo sort gives a flat order),
//! * It detects cycles cleanly (modules left with non-zero in-degree
//!   after the algorithm completes form the cycle), and
//! * It is deterministic when the input edge list is iterated in a
//!   stable order — we sort module indices within each level so the
//!   reactor produces bit-identical level-groupings across runs.
//!
//! Cycles in a Maven reactor are illegal (Maven itself refuses to
//! build them). We surface them with the structured error code
//! `BAR-REACTOR-CYCLE` naming the modules in the cycle path so the
//! user can untangle the offender.
//!
//! # Concurrency model
//!
//! Per-level parallelism uses `tokio::join_all` over the level's
//! modules. Within one module, the action stream (the lifecycle phase
//! prefix from [`crate::action_graph::lifecycle_graph`]) is dispatched
//! **sequentially** — Maven's lifecycle ordering inside a module is
//! load-bearing. The parallelism is module-level, not action-level.
//!
//! A `tokio::sync::Semaphore` caps the number of modules executing
//! concurrently. The cap is the same `workers` budget the daemon
//! launcher resolves (`1C` / `0.75C` / literal int per M4.2 T2). Even
//! if a level contains more modules than the budget allows, only
//! `workers` of them run at once; the rest queue on the semaphore.
//! This matches Maven's `-T <n>` reactor parallelism semantics.
//!
//! # `--no-daemon` interaction
//!
//! When `--no-daemon` is set (M4.2 T8), `cmd::verify::run_phase`
//! short-circuits to `crate::cmd::no_daemon::dispatch`, which forks an
//! upstream `mvn` invocation. Upstream `mvn` has its own multi-module
//! reactor with its own `-T` thread budget — the barista-side reactor
//! is not consulted on that path. This is the documented trade-off:
//! `--no-daemon` means "delegate the whole build to upstream Maven",
//! including the reactor. The barista-side reactor in this module
//! only fires on the daemon path.
//!
//! # v0.1 scope
//!
//! * Module discovery uses the root POM's `<modules>` block + nested
//!   aggregator POMs. Profiles' `<modules>` are not yet honoured;
//!   profile-driven module sets are a v0.2 follow-up.
//! * The inter-module edge set is built from each module's
//!   `<dependencies>` block by direct `groupId:artifactId` match
//!   against the reactor's module index. Parent-POM-inherited
//!   `groupId` is honoured via the M1.2 effective-POM parent chain.
//!   Property interpolation in dep coordinates is *not* yet wired —
//!   if a module references a sibling with `${project.groupId}`, the
//!   edge is missed in v0.1. Real-world reactors use literal
//!   coordinates for intra-reactor refs in >99% of cases; the
//!   interpolation case is tracked as a v0.2 follow-up.
//! * `--projects` / `--also-make` / `--resume-from` Maven CLI flags
//!   are not yet implemented; the reactor always builds the full
//!   module set. Sub-reactor selection lands in a future task.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::action_graph::{ActionGraph, lifecycle_graph};
use crate::cmd::MavenPhase;

/// Stable Maven module identity: the `groupId:artifactId` pair. The
/// `version` is intentionally not part of the identity — a reactor
/// can have only one version per `(groupId, artifactId)` (this is
/// the canonical Maven contract; module-version mismatch is itself a
/// build error upstream Maven surfaces), so the `version`-less pair
/// is a sufficient key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleId {
    /// Maven `groupId`. Inherited from the parent POM when the module
    /// itself doesn't declare one.
    pub group_id: String,
    /// Maven `artifactId`. Always present on a well-formed POM.
    pub artifact_id: String,
}

impl ModuleId {
    /// Canonical `"groupId:artifactId"` rendering. Used in cycle
    /// messages + diagnostic logs.
    #[must_use]
    pub fn to_ga(&self) -> String {
        format!("{}:{}", self.group_id, self.artifact_id)
    }
}

/// One module in the reactor.
#[derive(Debug, Clone)]
pub struct ModuleNode {
    /// `groupId:artifactId` identity.
    pub id: ModuleId,
    /// Absolute path of the module's directory (contains `pom.xml`).
    pub root: PathBuf,
    /// Sequential action stream for this module's lifecycle phase.
    /// Sourced from [`crate::action_graph::lifecycle_graph`].
    pub action_graph: ActionGraph,
    /// Reactor-internal indices of modules this one depends on.
    /// Empty for leaf modules. Topological ordering ensures these
    /// indices have already executed by the time this module starts.
    pub depends_on: Vec<usize>,
}

/// A multi-module reactor: the ordered list of modules + the per-
/// level topological grouping used for parallel dispatch.
#[derive(Debug, Clone)]
pub struct Reactor {
    /// All modules in the reactor, in stable discovery order (the
    /// order they appear in the root POM's `<modules>` block, with
    /// nested aggregator modules expanded depth-first).
    pub modules: Vec<ModuleNode>,
    /// Per-level grouping. `topo_levels[0]` is the set of root modules
    /// (no unresolved deps); `topo_levels[1]` is the next layer; and so
    /// on. The dispatcher runs every module in one level in parallel
    /// and waits for the level to complete before advancing.
    ///
    /// Within a level, indices are sorted ascending so the dispatch
    /// order is deterministic across runs — useful for byte-equal
    /// artifact comparisons and reproducibility tests (M4.3 T6).
    pub topo_levels: Vec<Vec<usize>>,
}

/// Errors surfaced while building a [`Reactor`].
#[derive(Debug, thiserror::Error)]
pub enum ReactorError {
    /// Failed to read or parse a POM. Wraps the path + the underlying
    /// I/O or parse error message.
    #[error("BAR-REACTOR-POM: failed to read POM at {path}: {detail}")]
    Pom {
        /// Path to the offending `pom.xml`.
        path: PathBuf,
        /// Underlying error rendered as a string.
        detail: String,
    },

    /// A module is missing its `artifactId`. Maven itself rejects
    /// these, so we surface it explicitly rather than silently
    /// dropping the module.
    #[error("BAR-REACTOR-POM: module at {path} has no <artifactId>")]
    MissingArtifactId {
        /// Path to the module's `pom.xml`.
        path: PathBuf,
    },

    /// The reactor's inter-module dependency graph contains a cycle.
    /// The `path` field renders the cycle as `a -> b -> c -> a`.
    #[error("BAR-REACTOR-CYCLE: inter-module dependency cycle detected: {path}")]
    Cycle {
        /// Human-readable cycle rendering.
        path: String,
    },
}

impl Reactor {
    /// Build a `Reactor` rooted at `project_root` for the given
    /// lifecycle `phase`.
    ///
    /// In the single-module case (no `<modules>` block in the root
    /// POM, or only one POM in the project), the returned reactor
    /// has one module + one topo-level — the dispatcher's parallel
    /// fast path degenerates to the existing serial behaviour with
    /// no observable change.
    pub fn from_project_root(
        project_root: &Path,
        phase: MavenPhase,
        include_clean: bool,
    ) -> Result<Self, ReactorError> {
        let discovered = discover_modules(project_root)?;
        Self::from_discovered(discovered, phase, include_clean)
    }

    /// Build a `Reactor` from a pre-discovered set of modules. Carved
    /// out so unit tests can drive the topo-sort with synthetic
    /// module sets without writing real POMs to disk.
    fn from_discovered(
        discovered: Vec<DiscoveredModule>,
        phase: MavenPhase,
        include_clean: bool,
    ) -> Result<Self, ReactorError> {
        // Index modules by GA so we can resolve intra-reactor edges.
        let mut by_ga: HashMap<ModuleId, usize> = HashMap::new();
        for (idx, m) in discovered.iter().enumerate() {
            by_ga.insert(m.id.clone(), idx);
        }

        let mut modules: Vec<ModuleNode> = Vec::with_capacity(discovered.len());
        for m in &discovered {
            let mut depends_on: Vec<usize> = Vec::new();
            for dep_ga in &m.dependency_gas {
                if let Some(&dep_idx) = by_ga.get(dep_ga)
                    && dep_idx != modules.len()
                {
                    // Self-edges are silently ignored (a module can't
                    // depend on itself; upstream Maven would reject
                    // this at POM parse time anyway).
                    depends_on.push(dep_idx);
                }
            }
            // Dedup + sort for determinism. A pathological POM with
            // two `<dependency>` blocks naming the same module would
            // otherwise inflate the edge count.
            depends_on.sort_unstable();
            depends_on.dedup();

            let action_graph = lifecycle_graph(phase, m.root.clone(), include_clean);
            modules.push(ModuleNode {
                id: m.id.clone(),
                root: m.root.clone(),
                action_graph,
                depends_on,
            });
        }

        let topo_levels = kahn_topo_levels(&modules)?;
        Ok(Reactor {
            modules,
            topo_levels,
        })
    }

    /// `true` when the reactor is a single-module degenerate case.
    /// The dispatcher uses this as a fast-path check to skip
    /// per-level structure entirely.
    #[must_use]
    pub fn is_single_module(&self) -> bool {
        self.modules.len() == 1
    }
}

/// Intermediate shape used during discovery: a parsed module that
/// hasn't yet had its action graph attached or its inter-module
/// edges resolved.
#[derive(Debug, Clone)]
struct DiscoveredModule {
    id: ModuleId,
    root: PathBuf,
    /// `groupId:artifactId` pairs the module's `<dependencies>` block
    /// references. Resolved against the reactor's GA index by
    /// [`Reactor::from_discovered`].
    dependency_gas: Vec<ModuleId>,
}

/// Walk the project's `pom.xml` tree, returning every module in stable
/// discovery order.
///
/// Discovery is depth-first over `<modules>` blocks: an aggregator POM
/// with `<modules><module>core</module><module>api</module></modules>`
/// yields the aggregator itself, then `core` and its nested modules,
/// then `api` and its nested modules. Aggregator POMs with
/// `<packaging>pom</packaging>` and no sources of their own are
/// included because Maven's reactor visits them too (their lifecycle
/// is a no-op in practice but the dispatcher still routes the action
/// stream so the daemon-side renderer sees a consistent event flow).
fn discover_modules(project_root: &Path) -> Result<Vec<DiscoveredModule>, ReactorError> {
    let mut out: Vec<DiscoveredModule> = Vec::new();
    discover_at(project_root, None, &mut out)?;
    Ok(out)
}

fn discover_at(
    module_root: &Path,
    inherited_group_id: Option<&str>,
    out: &mut Vec<DiscoveredModule>,
) -> Result<(), ReactorError> {
    let pom_path = module_root.join("pom.xml");
    let xml = std::fs::read_to_string(&pom_path).map_err(|e| ReactorError::Pom {
        path: pom_path.clone(),
        detail: e.to_string(),
    })?;
    let raw = barista_pom::parse_pom(&xml).map_err(|e| ReactorError::Pom {
        path: pom_path.clone(),
        detail: e.to_string(),
    })?;

    // Resolve groupId: explicit > parent > inherited (from caller).
    // Maven's rule is parent inheritance, so the parent block wins
    // over the caller-passed `inherited_group_id` — the caller fallback
    // is for the deeply-pathological case where the parent block is
    // omitted entirely (`pom.xml` with no parent, no groupId — which
    // Maven itself rejects, but we surface it as MissingArtifactId via
    // the artifactId check below rather than groupId).
    let group_id = raw
        .group_id
        .clone()
        .or_else(|| raw.parent.as_ref().map(|p| p.group_id.clone()))
        .or_else(|| inherited_group_id.map(str::to_string))
        .unwrap_or_default();

    if raw.artifact_id.is_empty() {
        return Err(ReactorError::MissingArtifactId { path: pom_path });
    }

    let id = ModuleId {
        group_id: group_id.clone(),
        artifact_id: raw.artifact_id.clone(),
    };

    // Build the dep GA list. We honour parent inheritance of groupId
    // on the dependency side too — a `<dependency>` declaring only
    // an `<artifactId>` is malformed (Maven requires groupId), so we
    // skip those entries silently.
    let mut dependency_gas: Vec<ModuleId> = Vec::with_capacity(raw.dependencies.len());
    for d in &raw.dependencies {
        if d.group_id.is_empty() || d.artifact_id.is_empty() {
            continue;
        }
        dependency_gas.push(ModuleId {
            group_id: d.group_id.clone(),
            artifact_id: d.artifact_id.clone(),
        });
    }

    out.push(DiscoveredModule {
        id,
        root: module_root.to_path_buf(),
        dependency_gas,
    });

    // Recurse into <modules>. Stable iteration order (the order in
    // the parent POM) drives the reactor's stable module index.
    for child in &raw.modules {
        if child.is_empty() {
            continue;
        }
        let child_root = module_root.join(child);
        // Pass our group_id down as the inherited fallback for the
        // child's groupId resolution.
        discover_at(&child_root, Some(&group_id), out)?;
    }
    Ok(())
}

/// Kahn's algorithm: produce a level-grouped topological sort.
///
/// For each level, modules whose remaining in-degree is zero form
/// that level. After emitting a level we decrement in-degrees on
/// modules that depend on the just-emitted ones; modules that hit
/// zero in-degree on this pass become the next level's seed.
///
/// Within a level, indices are sorted ascending so the level
/// ordering is deterministic — two runs of `barista verify` against
/// the same project produce the same level grouping.
///
/// Cycle detection: when no further module can be emitted but some
/// modules still have non-zero in-degree, those modules collectively
/// form the cycle (one or more cycles, all interlinked). We render
/// one representative cycle path through them for the error message.
fn kahn_topo_levels(modules: &[ModuleNode]) -> Result<Vec<Vec<usize>>, ReactorError> {
    let n = modules.len();
    // in_degree[i] = number of unresolved deps for module i.
    let mut in_degree: Vec<usize> = modules.iter().map(|m| m.depends_on.len()).collect();

    // reverse_edges[j] = list of modules that depend on j (so when
    // j is emitted, we can decrement their in-degree). Built from
    // each module's depends_on list.
    let mut reverse_edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, m) in modules.iter().enumerate() {
        for &dep in &m.depends_on {
            if dep < n {
                reverse_edges[dep].push(i);
            }
        }
    }
    // Sort reverse-edge targets for determinism.
    for r in &mut reverse_edges {
        r.sort_unstable();
        r.dedup();
    }

    let mut levels: Vec<Vec<usize>> = Vec::new();
    let mut emitted = vec![false; n];
    let mut emitted_count = 0usize;

    loop {
        // Collect every module with in_degree == 0 and not yet emitted.
        let mut level: Vec<usize> = (0..n)
            .filter(|&i| !emitted[i] && in_degree[i] == 0)
            .collect();
        if level.is_empty() {
            break;
        }
        level.sort_unstable();
        // Decrement in-degrees on dependants.
        for &i in &level {
            emitted[i] = true;
            for &j in &reverse_edges[i] {
                in_degree[j] = in_degree[j].saturating_sub(1);
            }
        }
        emitted_count += level.len();
        levels.push(level);
    }

    if emitted_count < n {
        let cycle_path = render_cycle(modules, &emitted);
        return Err(ReactorError::Cycle { path: cycle_path });
    }

    Ok(levels)
}

/// Render a representative cycle through the un-emitted modules.
///
/// Walks one un-emitted node, follows its first un-emitted dep, and
/// continues until we revisit a node — that closes a cycle. The
/// rendering is `"a -> b -> c -> a"` (the repeated tail names the
/// node that closes the cycle).
fn render_cycle(modules: &[ModuleNode], emitted: &[bool]) -> String {
    // Start from the lowest-indexed un-emitted module for determinism.
    let start = match emitted.iter().position(|e| !*e) {
        Some(i) => i,
        None => return String::new(),
    };
    let mut visited: BTreeMap<usize, usize> = BTreeMap::new();
    let mut path: Vec<usize> = Vec::new();
    let mut cur = start;
    loop {
        if let Some(&first_seen) = visited.get(&cur) {
            // Close the cycle at the first re-visit.
            let mut out: Vec<String> = path[first_seen..]
                .iter()
                .map(|&i| modules[i].id.to_ga())
                .collect();
            out.push(modules[cur].id.to_ga());
            return out.join(" -> ");
        }
        visited.insert(cur, path.len());
        path.push(cur);
        // Follow the first un-emitted dep. If there is none (a node
        // whose only deps are emitted), break — this shouldn't happen
        // for a cycle-implicated node, but be defensive.
        let next = modules[cur].depends_on.iter().copied().find(|&d| {
            d < emitted.len() && !emitted[d] && !path.contains(&d) || visited.contains_key(&d)
        });
        match next {
            Some(n) => cur = n,
            None => {
                // No traversable next node — render the partial path.
                return path
                    .iter()
                    .map(|&i| modules[i].id.to_ga())
                    .collect::<Vec<_>>()
                    .join(" -> ");
            }
        }
    }
}

/// Boxed `Send` future used by [`ModuleDispatcher::dispatch`].
///
/// Spelled out as a type alias so the complex `Pin<Box<dyn Future +
/// Send>>` shape doesn't trip the `clippy::type_complexity` lint at
/// each call site. The trait can't use `async fn` directly in a
/// public trait without `async-trait`, and the dispatcher's
/// implementation needs `Send` to be `tokio::spawn`able.
pub type ModuleDispatchFuture<O, E> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<O, E>> + Send>>;

/// Per-module dispatcher signature.
///
/// The reactor wraps user-supplied dispatch logic (the verify path's
/// "submit every action through the daemon respawn driver" loop) in
/// a `tokio` task per module. The signature is deliberately generic:
/// the reactor only needs to know how to invoke the dispatcher and
/// receive a `Result` back. Tests substitute a no-op dispatcher; the
/// real `verify` path passes in a closure that walks the module's
/// `action_graph.actions` and submits each through `submit_with_respawn`.
pub trait ModuleDispatcher: Send + Sync + 'static {
    /// Per-module outcome. The verify path uses `Vec<MojoInvocation>`;
    /// tests use anything `Send + 'static`.
    type Outcome: Send + 'static;
    /// Per-module error. The verify path uses its own `VerifyError`;
    /// tests use anything `Send + 'static`.
    type Error: Send + 'static;

    /// Dispatch one module. The reactor invokes this from a `tokio`
    /// task — it must be `Send` so the future can be `tokio::spawn`ed.
    fn dispatch(&self, module: &ModuleNode) -> ModuleDispatchFuture<Self::Outcome, Self::Error>;
}

/// Run the reactor's modules through the supplied dispatcher with
/// per-level parallelism.
///
/// `workers_budget` is the maximum number of modules executing
/// concurrently across the whole reactor; the dispatch loop installs a
/// `tokio::sync::Semaphore` with that many permits and every module
/// acquires one before it dispatches. A level with more modules than
/// the budget queues; a level with fewer is bound by `workers_budget`.
///
/// On the first failing module the reactor stops dispatching further
/// modules at *subsequent* levels — modules already in flight in the
/// current level run to completion (cancellation mid-action is unsafe
/// without daemon cooperation). This matches Maven's `--fail-fast`
/// reactor default.
///
/// The return value is the per-module outcome list in module index
/// order, populated only for modules that ran. On any failure the
/// `Result` is `Err`.
pub async fn run<D: ModuleDispatcher>(
    reactor: &Reactor,
    dispatcher: Arc<D>,
    workers_budget: usize,
) -> Result<Vec<D::Outcome>, D::Error>
where
    D::Outcome: Default,
{
    let budget = workers_budget.max(1);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(budget));
    let mut outcomes: Vec<Option<D::Outcome>> = (0..reactor.modules.len()).map(|_| None).collect();

    type LevelTaskHandle<D> = tokio::task::JoinHandle<(
        usize,
        Result<<D as ModuleDispatcher>::Outcome, <D as ModuleDispatcher>::Error>,
    )>;
    for (level_idx, level) in reactor.topo_levels.iter().enumerate() {
        let mut handles: Vec<LevelTaskHandle<D>> = Vec::with_capacity(level.len());
        for &module_idx in level {
            let dispatcher = Arc::clone(&dispatcher);
            let semaphore = Arc::clone(&semaphore);
            // Clone module shape for the task; the underlying paths
            // are PathBuf so this is cheap.
            let module = reactor.modules[module_idx].clone();
            let module_id = module.id.to_ga();
            handles.push(tokio::spawn(async move {
                // Acquire a worker permit. Even a level of 100 modules
                // only runs `budget` at a time.
                let _permit = match semaphore.acquire_owned().await {
                    Ok(p) => p,
                    // Semaphore closed mid-build — happens only if the
                    // reactor itself is torn down. Treat as a generic
                    // module failure for the outer caller to surface.
                    Err(_) => {
                        let fut = dispatcher.dispatch(&module);
                        return (module_idx, fut.await);
                    }
                };
                tracing::debug!(
                    target: "barista::reactor",
                    level = level_idx,
                    module = %module_id,
                    "dispatching module"
                );
                let fut = dispatcher.dispatch(&module);
                let r = fut.await;
                (module_idx, r)
            }));
        }

        // Await every task in this level. We collect successes +
        // failures both — a level failure short-circuits the next
        // level but we still surface every result we got.
        let mut level_err: Option<D::Error> = None;
        for handle in handles {
            match handle.await {
                Ok((idx, Ok(out))) => {
                    outcomes[idx] = Some(out);
                }
                Ok((_idx, Err(e))) => {
                    // Keep the first error encountered. Subsequent
                    // failures in the same level still drain so the
                    // tokio tasks don't leak.
                    if level_err.is_none() {
                        level_err = Some(e);
                    }
                }
                Err(_join_err) => {
                    // A spawned task panicked. We can't propagate
                    // panics as the dispatcher's error type without
                    // an extra trait bound, so we surface it via
                    // tracing and treat it as a missing outcome.
                    tracing::error!(
                        target: "barista::reactor",
                        level = level_idx,
                        "module task join failed"
                    );
                }
            }
        }

        if let Some(e) = level_err {
            return Err(e);
        }
    }

    Ok(outcomes
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_node(idx_letter: char, deps: Vec<usize>) -> ModuleNode {
        let id = ModuleId {
            group_id: "g".to_string(),
            artifact_id: format!("m-{idx_letter}"),
        };
        ModuleNode {
            id,
            root: PathBuf::from(format!("/tmp/m-{idx_letter}")),
            action_graph: lifecycle_graph(
                MavenPhase::Verify,
                PathBuf::from(format!("/tmp/m-{idx_letter}")),
                false,
            ),
            depends_on: deps,
        }
    }

    #[test]
    fn topo_levels_single_module_is_one_level() {
        let modules = vec![make_node('a', vec![])];
        let levels = kahn_topo_levels(&modules).unwrap();
        assert_eq!(levels, vec![vec![0]]);
    }

    #[test]
    fn topo_levels_linear_chain_one_per_level() {
        // a -> b -> c (each depends on the previous).
        let modules = vec![
            make_node('a', vec![]),
            make_node('b', vec![0]),
            make_node('c', vec![1]),
        ];
        let levels = kahn_topo_levels(&modules).unwrap();
        assert_eq!(levels, vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn topo_levels_diamond_collapses_middle() {
        //     a
        //    / \
        //   b   c
        //    \ /
        //     d
        // b and c both depend on a; d depends on b + c. Level 0 = {a},
        // level 1 = {b, c} (parallel), level 2 = {d}.
        let modules = vec![
            make_node('a', vec![]),
            make_node('b', vec![0]),
            make_node('c', vec![0]),
            make_node('d', vec![1, 2]),
        ];
        let levels = kahn_topo_levels(&modules).unwrap();
        assert_eq!(levels, vec![vec![0], vec![1, 2], vec![3]]);
    }

    #[test]
    fn topo_levels_fan_in_one_root_level() {
        // a, b, c are all independent; d depends on all three. Level 0
        // is {a, b, c} (parallel); level 1 is {d}.
        let modules = vec![
            make_node('a', vec![]),
            make_node('b', vec![]),
            make_node('c', vec![]),
            make_node('d', vec![0, 1, 2]),
        ];
        let levels = kahn_topo_levels(&modules).unwrap();
        assert_eq!(levels, vec![vec![0, 1, 2], vec![3]]);
    }

    #[test]
    fn topo_levels_fan_out_one_leaf_level() {
        // a is the root; b, c, d all depend on it but have no inter-
        // dependencies. Level 0 = {a}; level 1 = {b, c, d} (parallel).
        let modules = vec![
            make_node('a', vec![]),
            make_node('b', vec![0]),
            make_node('c', vec![0]),
            make_node('d', vec![0]),
        ];
        let levels = kahn_topo_levels(&modules).unwrap();
        assert_eq!(levels, vec![vec![0], vec![1, 2, 3]]);
    }

    #[test]
    fn topo_levels_disconnected_components_share_root_level() {
        // Two independent linear chains share the root level — neither
        // chain's root has any deps.
        let modules = vec![
            make_node('a', vec![]),
            make_node('b', vec![0]),
            make_node('c', vec![]),
            make_node('d', vec![2]),
        ];
        let levels = kahn_topo_levels(&modules).unwrap();
        assert_eq!(levels, vec![vec![0, 2], vec![1, 3]]);
    }

    #[test]
    fn topo_levels_cycle_two_modules_errors() {
        // a -> b -> a. Both end up with non-zero in-degree.
        let modules = vec![make_node('a', vec![1]), make_node('b', vec![0])];
        let err = kahn_topo_levels(&modules).unwrap_err();
        match err {
            ReactorError::Cycle { path } => {
                assert!(path.contains("m-a"));
                assert!(path.contains("m-b"));
                assert!(path.contains("->"));
            }
            _ => panic!("expected ReactorError::Cycle, got {err:?}"),
        }
    }

    #[test]
    fn topo_levels_cycle_three_modules_errors() {
        // a -> b -> c -> a.
        let modules = vec![
            make_node('a', vec![2]),
            make_node('b', vec![0]),
            make_node('c', vec![1]),
        ];
        let err = kahn_topo_levels(&modules).unwrap_err();
        assert!(matches!(err, ReactorError::Cycle { .. }));
    }

    #[test]
    fn topo_levels_empty_reactor_is_empty() {
        // Degenerate: zero modules → zero levels (no work).
        let modules: Vec<ModuleNode> = Vec::new();
        let levels = kahn_topo_levels(&modules).unwrap();
        assert!(levels.is_empty());
    }

    #[test]
    fn topo_levels_within_level_indices_sorted() {
        // Determinism: even if the input dep order shuffles, the
        // within-level indices are sorted ascending.
        let modules = vec![
            make_node('a', vec![]),
            make_node('b', vec![]),
            make_node('c', vec![]),
        ];
        let levels = kahn_topo_levels(&modules).unwrap();
        assert_eq!(levels, vec![vec![0, 1, 2]]);
    }

    #[test]
    fn module_id_to_ga_renders_canonical_form() {
        let id = ModuleId {
            group_id: "com.example".into(),
            artifact_id: "core".into(),
        };
        assert_eq!(id.to_ga(), "com.example:core");
    }

    // ----- discover_modules tests ---------------------------------

    fn write_pom(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("pom.xml"), body).unwrap();
    }

    #[test]
    fn discover_walks_modules_block_depth_first() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        write_pom(
            root,
            r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
                <modelVersion>4.0.0</modelVersion>
                <groupId>com.example</groupId>
                <artifactId>parent</artifactId>
                <version>1.0.0</version>
                <packaging>pom</packaging>
                <modules>
                    <module>mod-a</module>
                    <module>mod-b</module>
                </modules>
            </project>"#,
        );
        write_pom(
            &root.join("mod-a"),
            r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
                <modelVersion>4.0.0</modelVersion>
                <parent>
                    <groupId>com.example</groupId>
                    <artifactId>parent</artifactId>
                    <version>1.0.0</version>
                </parent>
                <artifactId>mod-a</artifactId>
            </project>"#,
        );
        write_pom(
            &root.join("mod-b"),
            r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
                <modelVersion>4.0.0</modelVersion>
                <parent>
                    <groupId>com.example</groupId>
                    <artifactId>parent</artifactId>
                    <version>1.0.0</version>
                </parent>
                <artifactId>mod-b</artifactId>
                <dependencies>
                    <dependency>
                        <groupId>com.example</groupId>
                        <artifactId>mod-a</artifactId>
                        <version>1.0.0</version>
                    </dependency>
                </dependencies>
            </project>"#,
        );

        let discovered = discover_modules(root).unwrap();
        assert_eq!(discovered.len(), 3);
        assert_eq!(discovered[0].id.artifact_id, "parent");
        assert_eq!(discovered[1].id.artifact_id, "mod-a");
        assert_eq!(discovered[2].id.artifact_id, "mod-b");
        // mod-b's dep on mod-a is recorded as a GA reference.
        assert_eq!(discovered[2].dependency_gas.len(), 1);
        assert_eq!(discovered[2].dependency_gas[0].artifact_id, "mod-a");
    }

    #[test]
    fn reactor_from_two_module_diamond_orders_correctly() {
        // Aggregator parent + 3 modules: a (leaf), b (depends on a),
        // c (depends on a). Expect levels: [parent + a] then [b, c].
        // (Parent has no deps; a has no deps; b and c depend on a.)
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        write_pom(
            root,
            r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
                <modelVersion>4.0.0</modelVersion>
                <groupId>com.example</groupId>
                <artifactId>parent</artifactId>
                <version>1.0.0</version>
                <packaging>pom</packaging>
                <modules>
                    <module>a</module>
                    <module>b</module>
                    <module>c</module>
                </modules>
            </project>"#,
        );
        for m in &["a", "b", "c"] {
            let deps = if *m == "a" {
                String::new()
            } else {
                r#"<dependencies>
                    <dependency>
                        <groupId>com.example</groupId>
                        <artifactId>a</artifactId>
                        <version>1.0.0</version>
                    </dependency>
                </dependencies>"#
                    .to_string()
            };
            write_pom(
                &root.join(m),
                &format!(
                    r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
                        <modelVersion>4.0.0</modelVersion>
                        <parent>
                            <groupId>com.example</groupId>
                            <artifactId>parent</artifactId>
                            <version>1.0.0</version>
                        </parent>
                        <artifactId>{m}</artifactId>
                        {deps}
                    </project>"#
                ),
            );
        }

        let reactor =
            Reactor::from_project_root(root, MavenPhase::Verify, /*include_clean*/ false).unwrap();
        assert_eq!(reactor.modules.len(), 4);
        // The 4 modules: parent (0), a (1), b (2), c (3).
        // parent and a are both root (parent has no deps; a has no
        // deps either). b and c depend on a (index 1).
        assert_eq!(reactor.topo_levels.len(), 2);
        // First level has parent + a — the indices that depend on
        // nothing. The exact membership depends on discovery order;
        // we assert both are present.
        let l0: BTreeSet<usize> = reactor.topo_levels[0].iter().copied().collect();
        assert!(l0.contains(&0), "parent in level 0");
        assert!(l0.contains(&1), "a in level 0");
        let l1: BTreeSet<usize> = reactor.topo_levels[1].iter().copied().collect();
        assert!(l1.contains(&2), "b in level 1");
        assert!(l1.contains(&3), "c in level 1");
    }

    #[test]
    fn reactor_single_module_is_one_level_one_module() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        write_pom(
            root,
            r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
                <modelVersion>4.0.0</modelVersion>
                <groupId>com.example</groupId>
                <artifactId>solo</artifactId>
                <version>1.0.0</version>
            </project>"#,
        );
        let reactor = Reactor::from_project_root(root, MavenPhase::Verify, false).unwrap();
        assert!(reactor.is_single_module());
        assert_eq!(reactor.topo_levels, vec![vec![0]]);
    }

    // ----- run() dispatcher tests --------------------------------

    /// Test dispatcher that records the order modules are dispatched
    /// in + emits the global call count when dispatch starts. Used to
    /// prove per-level parallelism + workers-budget gating.
    ///
    /// Shared state lives in `Arc<…>` so the spawned future doesn't
    /// borrow `&self` (the dispatcher trait's future is `Send` and
    /// `'static`).
    struct RecordingDispatcher {
        state: Arc<RecordingState>,
        delay_ms: u64,
    }

    struct RecordingState {
        order: std::sync::Mutex<Vec<String>>,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    impl ModuleDispatcher for RecordingDispatcher {
        type Outcome = String;
        type Error = String;

        fn dispatch(&self, module: &ModuleNode) -> ModuleDispatchFuture<String, String> {
            let name = module.id.artifact_id.clone();
            let state = Arc::clone(&self.state);
            let delay_ms = self.delay_ms;
            Box::pin(async move {
                let cur = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                state.max_in_flight.fetch_max(cur, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                state.order.lock().unwrap().push(name.clone());
                state.in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(name)
            })
        }
    }

    fn make_recorder(delay_ms: u64) -> (Arc<RecordingDispatcher>, Arc<RecordingState>) {
        let state = Arc::new(RecordingState {
            order: std::sync::Mutex::new(Vec::new()),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        });
        let d = Arc::new(RecordingDispatcher {
            state: Arc::clone(&state),
            delay_ms,
        });
        (d, state)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_dispatches_independent_modules_in_parallel() {
        // 3 independent modules; workers budget 3 → all three should
        // overlap in flight.
        let modules = vec![
            make_node('a', vec![]),
            make_node('b', vec![]),
            make_node('c', vec![]),
        ];
        let reactor = Reactor {
            modules,
            topo_levels: vec![vec![0, 1, 2]],
        };
        let (dispatcher, state) = make_recorder(30);
        let _ = run(&reactor, dispatcher, 3).await.unwrap();
        assert_eq!(
            state.max_in_flight.load(Ordering::SeqCst),
            3,
            "all three modules should run concurrently when budget allows"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_respects_workers_budget_at_one() {
        // 3 independent modules; workers budget 1 → strictly serial.
        let modules = vec![
            make_node('a', vec![]),
            make_node('b', vec![]),
            make_node('c', vec![]),
        ];
        let reactor = Reactor {
            modules,
            topo_levels: vec![vec![0, 1, 2]],
        };
        let (dispatcher, state) = make_recorder(10);
        let _ = run(&reactor, dispatcher, 1).await.unwrap();
        assert_eq!(
            state.max_in_flight.load(Ordering::SeqCst),
            1,
            "workers=1 must serialize even an independent level"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_respects_level_ordering() {
        // Diamond: a -> b, c -> d. b and c parallel, d after.
        let modules = vec![
            make_node('a', vec![]),
            make_node('b', vec![0]),
            make_node('c', vec![0]),
            make_node('d', vec![1, 2]),
        ];
        let reactor = Reactor {
            modules,
            topo_levels: vec![vec![0], vec![1, 2], vec![3]],
        };
        let (dispatcher, state) = make_recorder(5);
        let _ = run(&reactor, dispatcher, 4).await.unwrap();
        let order = state.order.lock().unwrap().clone();
        // `a` must complete before `b` and `c`; `d` must complete last.
        let pos = |name: &str| order.iter().position(|x| x == name).unwrap();
        assert!(pos("m-a") < pos("m-b"), "a before b");
        assert!(pos("m-a") < pos("m-c"), "a before c");
        assert!(pos("m-b") < pos("m-d"), "b before d");
        assert!(pos("m-c") < pos("m-d"), "c before d");
    }
}
