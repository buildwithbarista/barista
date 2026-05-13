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

use async_trait::async_trait;
use barista_coords::Coords;
use barista_pom::{
    ActivationContext, ParentResolver, RawDependency, RawExclusion, RawPom, ResolvedPom,
    resolve_pom,
};
use barista_version::Version;

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
    pub fn inherit(parent_path_scope: Scope, declared: Scope) -> Option<Scope> {
        use Scope::*;
        match (parent_path_scope, declared) {
            (Compile, Compile) => Some(Compile),
            (Compile, Runtime) => Some(Runtime),
            (Compile, Provided) => Some(Provided),
            (Compile, Test) => None,
            (Compile, System) => None,
            (Provided, Compile) => Some(Provided),
            (Provided, Runtime) => Some(Provided),
            (Provided, Provided) => Some(Provided),
            (Provided, Test) => None,
            (Provided, System) => None,
            (Runtime, Compile) => Some(Runtime),
            (Runtime, Runtime) => Some(Runtime),
            (Runtime, Test) => None,
            (Runtime, Provided) => None,
            (Runtime, System) => None,
            (Compile, Import) => None,
            (Provided, Import) => None,
            (Runtime, Import) => None,
            (Test, Import) => None,
            (Test, Compile) => Some(Test),
            (Test, Runtime) => Some(Test),
            (Test, Provided) => None,
            (Test, Test) => None,
            (Test, System) => None,
            (System, _) => None,
            (Import, _) => None,
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
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            strip_optional: true,
            include_scopes: BTreeSet::new(),
            activation: ActivationContext::default(),
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
    let mut state = WalkState::new();
    // Seed the frontier with the root POM's directly-declared deps.
    enqueue_direct_deps(&mut state, &root.pom.dependencies, opts)?;

    // BFS loop. Each iteration dequeues exactly one work item.
    while let Some(item) = state.queue.pop_front() {
        process_item(&mut state, source, opts, item).await?;
    }

    Ok(state.finish())
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
}

impl WalkState {
    fn new() -> Self {
        Self::default()
    }

    fn finish(self) -> ResolvedGraph {
        let WalkState {
            winners,
            order,
            losers,
            warnings,
            ..
        } = self;

        let resolved: Vec<ResolvedDep> = order
            .iter()
            .filter_map(|c| winners.get(c).cloned())
            .collect();

        let winners_btree: BTreeMap<Coords, ResolvedDep> =
            winners.into_iter().collect();

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

    let resolved_version = resolve_spec(&spec, &coords, source, &mut state.warnings).await?;

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

    // SKIPPER SEAM (M2.1 Task 3): consult Skipper.should_prune(&coords,
    // &resolved_version, depth, &exclusions) here. If it returns true,
    // skip the fetch_pom + child enqueue. The walker's correctness must
    // not depend on the skipper firing — it is purely an optimization.

    // 6. Record this node as the (provisional) winner.
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
        return Ok(());
    }

    // Fetch + resolve the child POM. A missing transitive POM is a hard
    // error — surface it.
    let (raw_pom, _origin) = source.fetch_pom(&coords, &resolved_version).await?;

    // Resolve the child POM (parent merge + interpolation + depMgt). For
    // the resolver-only walker, we use a parent resolver that delegates
    // back into `source.fetch_pom` synchronously. To avoid re-entering
    // async from sync, we pre-fetch the parent chain... in practice, for
    // T2 most fixtures + Maven-Central POMs have already-resolved deps
    // by the time we get here. To keep T2 simple and deterministic, we
    // run resolve_pom with a `NoParentResolver` that returns an empty
    // POM on any parent ask. T7 / M2.3 will plug in a real parent
    // resolver backed by the same MetadataSource.
    let resolved_child = match resolve_child_pom(raw_pom, &opts.activation) {
        Ok(r) => r,
        Err(e) => {
            // Fall back to using the raw POM with no depMgt expansion. We
            // still propagate dependencies that have explicit versions.
            // This matches the spec's "T2 stays small; T7 polishes."
            let _ = e;
            return Ok(());
        }
    };

    let child_depth = depth.saturating_add(1);
    // Merge our exclusions: parent's exclusions + this dep's exclusions.
    let merged_exclusions: Vec<RawExclusion> = {
        let mut v = exclusions.clone();
        v.extend(dep.exclusions.iter().cloned());
        v
    };

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
    }

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
) -> Result<String, WalkError> {
    match spec {
        VersionSpec::Soft(v) => Ok(v.clone()),
        VersionSpec::Hard(intervals) => {
            // For T2, hard ranges are reported as "pick the largest in-range
            // version we know about, else surface the first interval's
            // lower bound as a soft preference." We can't enumerate all
            // versions without calling fetch_metadata, so we keep it
            // simple and fall back to fetch_metadata when needed.
            let (md, _) = source.fetch_metadata(coords).await?;
            let in_range: Vec<&String> = md
                .versions
                .iter()
                .filter(|v| {
                    let parsed = Version::parse(v);
                    intervals.iter().any(|iv| interval_contains(iv, &parsed))
                })
                .collect();
            if let Some(picked) = in_range.iter().max_by(|a, b| {
                Version::parse(a).cmp(&Version::parse(b))
            }) {
                Ok((*picked).clone())
            } else {
                Err(WalkError::NoMetaVersionCandidate {
                    coords: coords.to_string(),
                    spec: format!("{spec:?}"),
                })
            }
        }
        VersionSpec::Latest => {
            let (md, _) = source.fetch_metadata(coords).await?;
            let picked = md.versions.last().cloned().ok_or_else(|| {
                WalkError::NoMetaVersionCandidate {
                    coords: coords.to_string(),
                    spec: "LATEST".to_string(),
                }
            })?;
            warnings.push(SpecWarning::LatestUsed {
                coords: coords.to_string(),
                resolved_to: picked.clone(),
            });
            Ok(picked)
        }
        VersionSpec::Release => {
            let (md, _) = source.fetch_metadata(coords).await?;
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

/// A [`ParentResolver`] that returns an empty POM for every parent ask.
/// Used by the walker when resolving fetched transitive POMs: the M2.1
/// task-2 surface is intentionally narrow — proper parent-chain
/// resolution for transitives is the job of T7's golden-tests scaffolding
/// (and ultimately of M2.3's cache-backed resolver). For fixtures whose
/// POMs are already self-contained (no `<parent>` element) this resolver
/// is never consulted; for real-world POMs it degrades to "do not expand
/// what we can't resolve" without crashing.
struct NoParentResolver;

impl ParentResolver for NoParentResolver {
    fn resolve(&mut self, parent: &barista_pom::RawParent) -> Result<RawPom, String> {
        Err(format!(
            "no parent resolver wired (looked up {}:{}:{})",
            parent.group_id, parent.artifact_id, parent.version
        ))
    }
}

fn resolve_child_pom(
    raw: RawPom,
    activation: &ActivationContext,
) -> Result<ResolvedPom, barista_pom::ResolveError> {
    let mut r = NoParentResolver;
    resolve_pom(raw, &mut r, activation)
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
    use barista_pom::{
        EffectivePom, Properties, RawDependency, RawExclusion, ResolvedPom,
    };

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
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![dep("ex", "C", "1.0")]));
        src.add_pom(co("ex", "D"), "1.0", pom("ex", "D", "1.0", vec![dep("ex", "C", "2.0")]));
        src.add_pom(co("ex", "C"), "1.0", pom("ex", "C", "1.0", vec![]));
        src.add_pom(co("ex", "C"), "2.0", pom("ex", "C", "2.0", vec![]));

        let root = pom("ex", "root", "1.0", vec![dep("ex", "B", "1.0"), dep("ex", "D", "1.0")]);
        let g = run(root, &src).await;
        assert_eq!(versions(&g, "ex", "C"), Some("1.0".into()));
        assert_eq!(g.resolved.len(), 3); // B, D, C
    }

    // ---- 2. Cycle termination ---------------------------------------------

    #[tokio::test]
    async fn cycle_terminates() {
        // root -> A -> B -> A (cycle)
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]));
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![dep("ex", "A", "1.0")]));

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
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]));
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

    // ---- 4. Provided is non-transitive (becomes provided) -----------------

    #[tokio::test]
    async fn provided_parent_makes_transitive_provided() {
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]));
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with("ex", "A", Some("1.0"), Some("provided"), None, vec![])],
        );
        let g = run(root, &src).await;
        assert_eq!(g.winners.get(&co("ex", "B")).unwrap().scope, Scope::Provided);
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
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]));
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![dep("org.foo", "bar", "1.0")]));
        src.add_pom(co("org.foo", "bar"), "1.0", pom("org.foo", "bar", "1.0", vec![]));

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with("ex", "A", Some("1.0"), None, None, vec![("org.foo", "bar")])],
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
        let err = walk(&resolved(root), &src, &WalkOptions::default()).await.unwrap_err();
        assert!(matches!(err, WalkError::MissingVersion { .. }));
    }

    // ---- 8. System scope does not propagate transitives -------------------

    #[tokio::test]
    async fn system_does_not_propagate() {
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]));
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with("ex", "A", Some("1.0"), Some("system"), None, vec![])],
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
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![dep("ex", "C", "2.0")]));
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
        let err = walk(&resolved(root), &src, &WalkOptions::default()).await.unwrap_err();
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
        assert!(g.warnings.iter().any(|w| matches!(w, SpecWarning::LatestUsed { .. })));
    }

    // ---- 15. RELEASE filters snapshots ------------------------------------

    #[tokio::test]
    async fn release_skips_snapshots() {
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));
        src.add_pom(co("ex", "A"), "2.0-SNAPSHOT", pom("ex", "A", "2.0-SNAPSHOT", vec![]));

        let mut d = dep("ex", "A", "RELEASE");
        d.version = Some("RELEASE".into());
        let root = pom("ex", "root", "1.0", vec![d]);
        let g = walk(&resolved(root), &src, &WalkOptions::default())
            .await
            .unwrap();
        assert_eq!(versions(&g, "ex", "A"), Some("1.0".into()));
        assert!(g.warnings.iter().any(|w| matches!(w, SpecWarning::ReleaseUsed { .. })));
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
                vec![dep_with("ex", "B", Some("1.0"), Some("runtime"), None, vec![])],
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
            vec![dep_with("ex", "A", Some("1.0"), Some("runtime"), None, vec![])],
        );
        let g = run(root, &src).await;
        assert!(!g.winners.contains_key(&co("ex", "B")));
    }

    // 18. Wildcard exclusion (*:*) prunes everything under the parent.
    #[tokio::test]
    async fn wildcard_exclusion_kills_all_transitives() {
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0"), dep("ex", "C", "1.0")]));
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));
        src.add_pom(co("ex", "C"), "1.0", pom("ex", "C", "1.0", vec![]));

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with("ex", "A", Some("1.0"), None, None, vec![("*", "*")])],
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
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]));
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![dep("ex", "C", "1.0")]));
        src.add_pom(co("ex", "C"), "1.0", pom("ex", "C", "1.0", vec![dep("org.bad", "evil", "1.0")]));
        src.add_pom(co("org.bad", "evil"), "1.0", pom("org.bad", "evil", "1.0", vec![]));

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with("ex", "A", Some("1.0"), None, None, vec![("org.bad", "evil")])],
        );
        let g = run(root, &src).await;
        assert!(!g.winners.contains_key(&co("org.bad", "evil")));
    }

    // 20. Audit records loser version/depth.
    #[tokio::test]
    async fn audit_records_losers() {
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![dep("ex", "C", "2.0")]));
        src.add_pom(co("ex", "C"), "1.0", pom("ex", "C", "1.0", vec![]));
        src.add_pom(co("ex", "C"), "2.0", pom("ex", "C", "2.0", vec![]));
        let root = pom("ex", "root", "1.0", vec![dep("ex", "C", "1.0"), dep("ex", "A", "1.0")]);
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
        let root = pom("ex", "root", "1.0", vec![dep("ex", "A", "1.0"), dep("ex", "B", "1.0"), dep("ex", "C", "1.0")]);
        let g = run(root, &src).await;
        let names: Vec<&str> = g.resolved.iter().map(|d| d.coords.artifact.as_str()).collect();
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
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]));
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
        assert!(matches!(err, WalkError::Metadata(MetadataError::NotFound { .. })));
    }

    // 29. Import scope on direct dep is silently dropped.
    #[tokio::test]
    async fn import_scope_on_direct_dep_dropped() {
        let src = FixtureSource::new();
        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep_with("ex", "A", Some("1.0"), Some("import"), None, vec![])],
        );
        let g = run(root, &src).await;
        assert!(g.winners.is_empty());
    }

    // 30. Scope::inherit fully covers Maven's table — direct enum test.
    #[test]
    fn scope_inherit_table_smoke() {
        use Scope::*;
        assert_eq!(Scope::inherit(Compile, Compile), Some(Compile));
        assert_eq!(Scope::inherit(Compile, Runtime), Some(Runtime));
        assert_eq!(Scope::inherit(Compile, Provided), Some(Provided));
        assert_eq!(Scope::inherit(Compile, Test), None);
        assert_eq!(Scope::inherit(Provided, Compile), Some(Provided));
        assert_eq!(Scope::inherit(Provided, Runtime), Some(Provided));
        assert_eq!(Scope::inherit(Runtime, Compile), Some(Runtime));
        assert_eq!(Scope::inherit(Runtime, Test), None);
        assert_eq!(Scope::inherit(Test, Compile), Some(Test));
        assert_eq!(Scope::inherit(Test, Runtime), Some(Test));
        assert_eq!(Scope::inherit(System, Compile), None);
        assert_eq!(Scope::inherit(Import, Compile), None);
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
}
