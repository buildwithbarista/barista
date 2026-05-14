//! PubGrub-based strict resolver.
//!
//! When the user passes `--strict`, the resolver runs PubGrub
//! instead of the default BFS+Skipper. PubGrub:
//!
//! - Honors hard version ranges (e.g. `[1.0,2.0)`).
//! - Records nearest-wins choices as SOFT preferences (priority
//!   hints, not constraints), so on a clean graph the strict
//!   resolver produces the same output as the BFS walker.
//! - On conflict, produces a structured [`StrictDerivation`]
//!   adapted from PubGrub's `DerivationTree` that names every
//!   dep edge contributing to the conflict — the error formatter
//!   (M2.2 Task 2) turns this into the user-facing "why" output.
//!
//! # Runtime model
//!
//! PubGrub's `DependencyProvider` is a synchronous trait, but our
//! [`MetadataSource`] is async. We bridge the two by pre-fetching
//! every `(coords, version)` reachable from the root POM into an
//! in-memory cache *before* invoking PubGrub. The solver then runs
//! purely synchronously on the cached data inside a
//! [`tokio::task::spawn_blocking`] call.
//!
//! This sidesteps the need to call `Handle::block_on` from inside
//! the solver thread and keeps the I/O surface inside the async
//! caller's runtime. The trade-off is that the pre-fetch may
//! over-fetch versions PubGrub would not have considered. For the
//! typical strict-mode case (small graphs, hard ranges as a
//! conflict-finding aid), this is acceptable.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;

use barista_coords::Coords;
use barista_pom::{RawDependency, ResolvedPom};
use barista_version::Version;
use pubgrub::{
    Dependencies, DependencyProvider, DerivationTree, External, PackageResolutionStatistics,
    PubGrubError, Ranges, resolve,
};

use crate::source::{MetadataError, MetadataSource};
use crate::version_spec::{Bound, Interval, VersionSpec};

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// The result of a strict resolution.
#[derive(Debug, Clone)]
pub enum StrictOutcome {
    /// PubGrub found a satisfying assignment for every reachable
    /// package. The map is keyed by [`Coords`] (the resolution key)
    /// and contains the version PubGrub picked.
    Resolved(BTreeMap<Coords, ResolvedStrictDep>),
    /// PubGrub could not find a satisfying assignment. The
    /// derivation carries enough structure for the error formatter
    /// (M2.2 Task 2) to render a user-facing diagnostic.
    Conflict(StrictDerivation),
}

/// A single resolved dependency in [`StrictOutcome::Resolved`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedStrictDep {
    pub coords: Coords,
    pub version: String,
    pub scope: String,
}

/// A structured explanation of why resolution failed. Built from
/// PubGrub's [`DerivationTree`]. Format-stable so the error
/// formatter (M2.2 T2) can render it without re-walking PubGrub
/// types.
#[derive(Debug, Clone)]
pub struct StrictDerivation {
    /// Human-readable summary of the root cause.
    pub root_cause: String,
    /// Each edge contributing to the conflict.
    pub contributing_edges: Vec<DepEdge>,
}

/// One dep edge mentioned by the derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepEdge {
    pub from_coords: Coords,
    pub from_version: String,
    pub to_coords: Coords,
    /// Canonical-form range required by `from` on `to`.
    pub required_range: String,
    /// The versions of `to_coords` that were considered by the
    /// solver (i.e. published versions discovered during
    /// pre-fetch).
    pub available_versions: Vec<String>,
}

/// Errors that can prevent the strict resolver from even getting
/// to a PubGrub run.
#[derive(Debug, thiserror::Error)]
pub enum StrictError {
    #[error("metadata source error: {0}")]
    Metadata(#[from] MetadataError),
    #[error("invalid version spec {spec:?} on {coords}: {detail}")]
    InvalidSpec {
        coords: String,
        spec: String,
        detail: String,
    },
    #[error("transitive dependency {coords} has no version (depMgt did not provide one)")]
    MissingVersion { coords: String },
    #[error("PubGrub adapter error: {detail}")]
    Adapter { detail: String },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run strict resolution on a root POM.
///
/// `root` must be a fully-resolved POM (parent-merged, interpolated,
/// depMgt-applied) — the same precondition as
/// [`crate::walker::walk`].
pub async fn resolve_strict<S>(root: &ResolvedPom, source: &S) -> Result<StrictOutcome, StrictError>
where
    S: MetadataSource + ?Sized,
{
    // Phase 1 (async): walk the dep graph, fetching every reachable
    // (coords, version) POM into a `Snapshot`. Hard-range deps
    // contribute every published version that satisfies the range;
    // soft deps contribute just the declared version (or the
    // resolved meta-version).
    let snapshot = build_snapshot(root, source).await?;

    // Phase 2 (sync, blocking): run PubGrub on the snapshot.
    let snapshot = Arc::new(snapshot);
    let solver_snap = Arc::clone(&snapshot);

    let join = tokio::task::spawn_blocking(move || run_pubgrub(&solver_snap)).await;

    match join {
        Ok(o) => o,
        Err(e) => Err(StrictError::Adapter {
            detail: format!("solver thread panicked: {e}"),
        }),
    }
}

// ---------------------------------------------------------------------------
// Phase 1: async pre-fetch into a Snapshot
// ---------------------------------------------------------------------------

/// One outgoing edge from a `(Coords, Version)` node: a target
/// package plus the version range it's pinned to.
type SnapshotEdge = (StrictPackage, Ranges<Version>);

/// All the dependency-graph data PubGrub needs, in a synchronously
/// queryable form.
#[derive(Debug, Default)]
struct Snapshot {
    /// `Coords -> sorted-asc list of versions` — what
    /// `fetch_metadata` told us.
    versions_by_coords: HashMap<Coords, Vec<Version>>,
    /// `(Coords, Version) -> list of deps to other packages, each
    /// already turned into a (coords, Ranges) pair`.
    dependencies: HashMap<(Coords, Version), Vec<SnapshotEdge>>,
    /// Versions to bias `choose_version` toward, in preference
    /// order. Matches BFS+Skipper's nearest-wins behaviour on
    /// clean graphs.
    soft_preferences: HashMap<Coords, Vec<Version>>,
    /// Original (as-typed) version strings, keyed by canonical
    /// `Version` so we can reproduce them in the final output.
    version_strings: HashMap<(Coords, Version), String>,
    /// The root package's deps.
    root_deps: Vec<SnapshotEdge>,
    /// Scope, per resolved coord. Carried through from the
    /// dependency declaration that introduced the coord; "compile"
    /// for root deps with no scope.
    scopes: HashMap<Coords, String>,
}

async fn build_snapshot<S>(root: &ResolvedPom, source: &S) -> Result<Snapshot, StrictError>
where
    S: MetadataSource + ?Sized,
{
    let mut snap = Snapshot::default();

    // Enqueue the root POM's directly-declared deps. Track which
    // (coords, version) pairs we've already explored so we don't
    // re-walk on cycles.
    let mut visited: HashSet<(Coords, Version)> = HashSet::new();
    let mut queue: VecDeque<(Coords, Version)> = VecDeque::new();

    // Root deps go in directly.
    for raw in &root.pom.dependencies {
        if is_import_scope(raw) {
            // `import` scope only matters during depMgt expansion,
            // which the POM resolver has already done by the time
            // we get here.
            continue;
        }
        if is_optional(raw) {
            // Strict mode is about hard-range conflicts. Optional
            // direct deps still participate, matching walker's
            // default (`strip_optional` only affects transitives).
        }
        let (pkg, range, declared_versions) = compile_dep_for_root(raw, source, &mut snap).await?;
        snap.root_deps.push((pkg.clone(), range.clone()));
        let coords = match &pkg {
            StrictPackage::Real(c) => c.clone(),
            StrictPackage::Root => continue,
        };
        snap.scopes
            .entry(coords.clone())
            .or_insert_with(|| scope_or_default(raw));
        // Seed the BFS with every published version of the
        // declared coord that satisfies this declaration's range —
        // PubGrub may need any of them.
        for v in declared_versions {
            if range.contains(&v) {
                queue.push_back((coords.clone(), v));
            }
        }
    }

    // BFS over transitive deps. We fetch the POM, then for each
    // transitive declaration we compute its range, intersect with
    // the published versions, and enqueue.
    while let Some((coords, version)) = queue.pop_front() {
        if !visited.insert((coords.clone(), version.clone())) {
            continue;
        }
        let pom = fetch_pom_versioned(source, &coords, &version, &mut snap).await?;
        let mut out_edges: Vec<SnapshotEdge> = Vec::new();
        for raw in &pom.dependencies {
            if is_import_scope(raw) {
                continue;
            }
            if is_optional(raw) {
                // Per Maven semantics + walker default, transitive
                // optionals are dropped. We follow that.
                continue;
            }
            let dep_scope = scope_or_default(raw);
            if !propagates_transitively(&dep_scope) {
                continue;
            }
            match compile_dep_transitive(raw, source, &mut snap).await? {
                None => continue,
                Some((dep_coords, range, declared_versions)) => {
                    out_edges.push((StrictPackage::Real(dep_coords.clone()), range.clone()));
                    snap.scopes
                        .entry(dep_coords.clone())
                        .or_insert(dep_scope.clone());
                    for v in declared_versions {
                        if range.contains(&v) {
                            queue.push_back((dep_coords.clone(), v));
                        }
                    }
                }
            }
        }
        snap.dependencies
            .insert((coords.clone(), version.clone()), out_edges);
    }

    Ok(snap)
}

/// Fetch a POM at a specific version and record both the
/// version_strings entry and the metadata listing.
async fn fetch_pom_versioned<S>(
    source: &S,
    coords: &Coords,
    version: &Version,
    snap: &mut Snapshot,
) -> Result<barista_pom::RawPom, StrictError>
where
    S: MetadataSource + ?Sized,
{
    // Find the original string for this version, if we already
    // know it. Otherwise we fall back to the canonical form, which
    // is round-trip stable for Maven version syntax.
    let raw_version = snap
        .version_strings
        .get(&(coords.clone(), version.clone()))
        .cloned()
        .unwrap_or_else(|| version.to_string());

    let (pom, _origin) = source.fetch_pom(coords, &raw_version).await?;
    snap.version_strings
        .entry((coords.clone(), version.clone()))
        .or_insert(raw_version);
    Ok(pom)
}

/// Compile a root-level `<dependency>` into a `(package, range,
/// declared_versions_to_explore)` triple. Also seeds
/// `version_strings` and `versions_by_coords`.
async fn compile_dep_for_root<S>(
    raw: &RawDependency,
    source: &S,
    snap: &mut Snapshot,
) -> Result<(StrictPackage, Ranges<Version>, Vec<Version>), StrictError>
where
    S: MetadataSource + ?Sized,
{
    let coords =
        Coords::new(&raw.group_id, &raw.artifact_id).map_err(|e| StrictError::Adapter {
            detail: format!(
                "invalid coords {}: {}: {}",
                raw.group_id, raw.artifact_id, e
            ),
        })?;

    let version_text = raw
        .version
        .as_deref()
        .ok_or_else(|| StrictError::MissingVersion {
            coords: coords.to_string(),
        })?;

    let spec = VersionSpec::parse(version_text).map_err(|e| StrictError::InvalidSpec {
        coords: coords.to_string(),
        spec: version_text.to_string(),
        detail: e.to_string(),
    })?;

    // Pull metadata to discover the universe of available versions
    // for this coord.
    let published = list_versions(source, &coords, snap).await?;

    let range = spec_to_range(&spec, &published);

    // Build the soft-preference seed: for a soft spec, that's the
    // declared version (highest priority) followed by anything
    // else in the range; for a hard spec, just the in-range
    // versions in descending order. The solver will use the front
    // of this list first.
    let prefs = soft_preferences_for(&spec, &published);
    snap.soft_preferences
        .entry(coords.clone())
        .or_default()
        .extend(prefs.into_iter());

    // The set of declared versions we want PubGrub to be aware of
    // — every published version that lies in `range`.
    let declared: Vec<Version> = published
        .into_iter()
        .filter(|v| range.contains(v))
        .collect();

    Ok((StrictPackage::Real(coords), range, declared))
}

/// Compile a transitive `<dependency>` to a `(coords, range,
/// declared)` triple. Returns `None` if the dependency declaration
/// is malformed *and* not a hard error (e.g., a missing version
/// for a transitive — we surface this as an error rather than
/// silently dropping).
async fn compile_dep_transitive<S>(
    raw: &RawDependency,
    source: &S,
    snap: &mut Snapshot,
) -> Result<Option<(Coords, Ranges<Version>, Vec<Version>)>, StrictError>
where
    S: MetadataSource + ?Sized,
{
    let coords =
        Coords::new(&raw.group_id, &raw.artifact_id).map_err(|e| StrictError::Adapter {
            detail: format!(
                "invalid coords {}: {}: {}",
                raw.group_id, raw.artifact_id, e
            ),
        })?;

    let version_text = raw
        .version
        .as_deref()
        .ok_or_else(|| StrictError::MissingVersion {
            coords: coords.to_string(),
        })?;

    let spec = VersionSpec::parse(version_text).map_err(|e| StrictError::InvalidSpec {
        coords: coords.to_string(),
        spec: version_text.to_string(),
        detail: e.to_string(),
    })?;

    let published = list_versions(source, &coords, snap).await?;
    let range = spec_to_range(&spec, &published);

    let prefs = soft_preferences_for(&spec, &published);
    snap.soft_preferences
        .entry(coords.clone())
        .or_default()
        .extend(prefs.into_iter());

    let declared: Vec<Version> = published
        .into_iter()
        .filter(|v| range.contains(v))
        .collect();

    Ok(Some((coords, range, declared)))
}

/// Look up published versions for `coords`. Caches the result in
/// `snap.versions_by_coords`. Returns versions sorted ascending
/// (so callers picking the "highest" can take `.last()`).
async fn list_versions<S>(
    source: &S,
    coords: &Coords,
    snap: &mut Snapshot,
) -> Result<Vec<Version>, StrictError>
where
    S: MetadataSource + ?Sized,
{
    if let Some(cached) = snap.versions_by_coords.get(coords) {
        return Ok(cached.clone());
    }
    let (meta, _origin) = source.fetch_metadata(coords).await?;
    let mut versions: Vec<Version> = Vec::with_capacity(meta.versions.len());
    for v in &meta.versions {
        let parsed = Version::parse(v);
        snap.version_strings
            .entry((coords.clone(), parsed.clone()))
            .or_insert(v.clone());
        versions.push(parsed);
    }
    versions.sort();
    snap.versions_by_coords
        .insert(coords.clone(), versions.clone());
    Ok(versions)
}

/// Convert a [`VersionSpec`] to a PubGrub [`Ranges<Version>`].
///
/// Soft specs become "any published version" so PubGrub is free
/// to pick under our `choose_version` preference order. Hard
/// specs become the union of the declared intervals.
fn spec_to_range(spec: &VersionSpec, published: &[Version]) -> Ranges<Version> {
    match spec {
        VersionSpec::Soft(_) | VersionSpec::Latest | VersionSpec::Release => Ranges::full(),
        VersionSpec::Hard(intervals) => {
            // For each Interval, build the PubGrub-side range. If
            // both bounds are `Included` and equal, that's a
            // singleton. We use `published` to do a final
            // filter-into-singletons membership test to keep the
            // representation tight.
            let _ = published; // currently unused; kept for future range tightening
            let mut acc: Ranges<Version> = Ranges::empty();
            for iv in intervals {
                acc = acc.union(&interval_to_range(iv));
            }
            acc
        }
    }
}

fn interval_to_range(iv: &Interval) -> Ranges<Version> {
    // Special case: `[X]` (exact match).
    if let (Bound::Included(lo), Bound::Included(hi)) = (&iv.lower, &iv.upper) {
        if lo == hi {
            return Ranges::singleton(lo.clone());
        }
    }
    let lo: Ranges<Version> = match &iv.lower {
        Bound::Unbounded => Ranges::full(),
        Bound::Included(v) => Ranges::higher_than(v.clone()),
        Bound::Excluded(v) => Ranges::strictly_higher_than(v.clone()),
    };
    let hi: Ranges<Version> = match &iv.upper {
        Bound::Unbounded => Ranges::full(),
        Bound::Included(v) => Ranges::lower_than(v.clone()),
        Bound::Excluded(v) => Ranges::strictly_lower_than(v.clone()),
    };
    lo.intersection(&hi)
}

fn soft_preferences_for(spec: &VersionSpec, published: &[Version]) -> Vec<Version> {
    let mut prefs: Vec<Version> = Vec::new();
    if let VersionSpec::Soft(v) = spec {
        // The declared soft version (if it's actually published)
        // is the highest priority.
        let declared = Version::parse(v);
        if published.iter().any(|p| p == &declared) {
            prefs.push(declared);
        }
    }
    prefs
}

fn scope_or_default(raw: &RawDependency) -> String {
    raw.scope
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("compile")
        .to_owned()
}

fn is_optional(raw: &RawDependency) -> bool {
    raw.optional
        .as_deref()
        .map(str::trim)
        .map(|s| s.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn is_import_scope(raw: &RawDependency) -> bool {
    raw.scope
        .as_deref()
        .map(str::trim)
        .map(|s| s.eq_ignore_ascii_case("import"))
        .unwrap_or(false)
}

fn propagates_transitively(scope: &str) -> bool {
    // Mirror the walker's behaviour: compile/runtime propagate;
    // test/provided/system/import do not.
    matches!(scope, "compile" | "runtime")
}

// ---------------------------------------------------------------------------
// Phase 2: synchronous PubGrub run on the snapshot
// ---------------------------------------------------------------------------

/// PubGrub's "package" key. We use a synthetic `Root` package so
/// PubGrub has a single entry-point to drive resolution from; all
/// other packages map 1:1 to `Coords`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum StrictPackage {
    Root,
    Real(Coords),
}

impl fmt::Display for StrictPackage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StrictPackage::Root => f.write_str("<root>"),
            StrictPackage::Real(c) => write!(f, "{c}"),
        }
    }
}

/// The synthetic root package's version. Singleton — there's only
/// ever one root.
fn root_version() -> Version {
    Version::parse("0")
}

struct SnapshotProvider<'a> {
    snap: &'a Snapshot,
}

impl<'a> DependencyProvider for SnapshotProvider<'a> {
    type P = StrictPackage;
    type V = Version;
    type VS = Ranges<Version>;
    type M = String;
    type Priority = Reverse<u32>;
    type Err = Infallible;

    fn prioritize(
        &self,
        _package: &Self::P,
        _range: &Self::VS,
        _stats: &PackageResolutionStatistics,
    ) -> Self::Priority {
        // Uniform priority — PubGrub's breadth-first tiebreak
        // handles ordering for us.
        Reverse(0)
    }

    fn choose_version(
        &self,
        package: &Self::P,
        range: &Self::VS,
    ) -> Result<Option<Self::V>, Self::Err> {
        match package {
            StrictPackage::Root => {
                let v = root_version();
                if range.contains(&v) {
                    Ok(Some(v))
                } else {
                    Ok(None)
                }
            }
            StrictPackage::Real(coords) => {
                // Preference order:
                //   1. Soft-preference versions, in the order they
                //      were recorded (mirrors BFS nearest-wins).
                //   2. The highest published version that's in range.
                if let Some(prefs) = self.snap.soft_preferences.get(coords) {
                    for v in prefs {
                        if range.contains(v) {
                            return Ok(Some(v.clone()));
                        }
                    }
                }
                let published = match self.snap.versions_by_coords.get(coords) {
                    Some(p) => p,
                    None => return Ok(None),
                };
                // Highest-in-range.
                for v in published.iter().rev() {
                    if range.contains(v) {
                        return Ok(Some(v.clone()));
                    }
                }
                Ok(None)
            }
        }
    }

    fn get_dependencies(
        &self,
        package: &Self::P,
        version: &Self::V,
    ) -> Result<Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        match package {
            StrictPackage::Root => {
                if version != &root_version() {
                    return Ok(Dependencies::Unavailable(
                        "internal: non-singleton root version".to_owned(),
                    ));
                }
                let deps: pubgrub::DependencyConstraints<Self::P, Self::VS> =
                    self.snap.root_deps.iter().cloned().collect();
                Ok(Dependencies::Available(deps))
            }
            StrictPackage::Real(coords) => {
                let key = (coords.clone(), version.clone());
                match self.snap.dependencies.get(&key) {
                    Some(edges) => {
                        let deps: pubgrub::DependencyConstraints<Self::P, Self::VS> =
                            edges.iter().cloned().collect();
                        Ok(Dependencies::Available(deps))
                    }
                    None => Ok(Dependencies::Unavailable(format!(
                        "no POM in snapshot for {coords}:{version}"
                    ))),
                }
            }
        }
    }
}

fn run_pubgrub(snap: &Snapshot) -> Result<StrictOutcome, StrictError> {
    let provider = SnapshotProvider { snap };
    let outcome = resolve(&provider, StrictPackage::Root, root_version());
    match outcome {
        Ok(selected) => {
            let mut resolved: BTreeMap<Coords, ResolvedStrictDep> = BTreeMap::new();
            for (pkg, version) in selected {
                let coords = match pkg {
                    StrictPackage::Root => continue,
                    StrictPackage::Real(c) => c,
                };
                let raw_version = snap
                    .version_strings
                    .get(&(coords.clone(), version.clone()))
                    .cloned()
                    .unwrap_or_else(|| version.to_string());
                let scope = snap
                    .scopes
                    .get(&coords)
                    .cloned()
                    .unwrap_or_else(|| "compile".to_owned());
                resolved.insert(
                    coords.clone(),
                    ResolvedStrictDep {
                        coords,
                        version: raw_version,
                        scope,
                    },
                );
            }
            Ok(StrictOutcome::Resolved(resolved))
        }
        Err(PubGrubError::NoSolution(tree)) => {
            let derivation = build_derivation(snap, &tree);
            Ok(StrictOutcome::Conflict(derivation))
        }
        Err(PubGrubError::ErrorRetrievingDependencies {
            package, version, ..
        }) => Err(StrictError::Adapter {
            detail: format!("ErrorRetrievingDependencies for {package}:{version}"),
        }),
        Err(PubGrubError::ErrorChoosingVersion { package, .. }) => Err(StrictError::Adapter {
            detail: format!("ErrorChoosingVersion for {package}"),
        }),
        Err(PubGrubError::ErrorInShouldCancel(_)) => Err(StrictError::Adapter {
            detail: "ErrorInShouldCancel".to_owned(),
        }),
    }
}

// ---------------------------------------------------------------------------
// DerivationTree → StrictDerivation adapter
// ---------------------------------------------------------------------------

fn build_derivation(
    snap: &Snapshot,
    tree: &DerivationTree<StrictPackage, Ranges<Version>, String>,
) -> StrictDerivation {
    let mut edges: Vec<DepEdge> = Vec::new();
    walk_tree(snap, tree, &mut edges);
    // De-dup edges while preserving order.
    let mut seen: HashSet<(Coords, String, Coords, String)> = HashSet::new();
    edges.retain(|e| {
        seen.insert((
            e.from_coords.clone(),
            e.from_version.clone(),
            e.to_coords.clone(),
            e.required_range.clone(),
        ))
    });

    let root_cause = summarize(tree);
    StrictDerivation {
        root_cause,
        contributing_edges: edges,
    }
}

fn walk_tree(
    snap: &Snapshot,
    tree: &DerivationTree<StrictPackage, Ranges<Version>, String>,
    out: &mut Vec<DepEdge>,
) {
    match tree {
        DerivationTree::External(ext) => collect_external(snap, ext, out),
        DerivationTree::Derived(derived) => {
            walk_tree(snap, &derived.cause1, out);
            walk_tree(snap, &derived.cause2, out);
        }
    }
}

fn collect_external(
    snap: &Snapshot,
    ext: &External<StrictPackage, Ranges<Version>, String>,
    out: &mut Vec<DepEdge>,
) {
    if let External::FromDependencyOf(from_pkg, from_range, to_pkg, to_range) = ext {
        let from_coords = match from_pkg {
            StrictPackage::Root => Coords {
                group: "<root>".to_owned(),
                artifact: "<root>".to_owned(),
            },
            StrictPackage::Real(c) => c.clone(),
        };
        let to_coords = match to_pkg {
            StrictPackage::Root => return,
            StrictPackage::Real(c) => c.clone(),
        };
        // PubGrub gives us a range on `from_pkg` (the set of
        // versions that have this dependency) and on `to_pkg`
        // (the set required). For the canonical edge we pick the
        // first concrete version of `from` in the range — most
        // useful for diagnostic output.
        let from_version =
            pick_witness_version(snap, from_pkg, from_range).unwrap_or_else(|| "*".to_owned());
        let required_range = render_range(to_range);
        let available_versions = snap
            .versions_by_coords
            .get(&to_coords)
            .map(|vs| vs.iter().map(|v| v.to_string()).collect())
            .unwrap_or_default();
        out.push(DepEdge {
            from_coords,
            from_version,
            to_coords,
            required_range,
            available_versions,
        });
    }
}

fn pick_witness_version(
    snap: &Snapshot,
    pkg: &StrictPackage,
    range: &Ranges<Version>,
) -> Option<String> {
    match pkg {
        StrictPackage::Root => Some(root_version().to_string()),
        StrictPackage::Real(c) => {
            let versions = snap.versions_by_coords.get(c)?;
            for v in versions.iter().rev() {
                if range.contains(v) {
                    return snap
                        .version_strings
                        .get(&(c.clone(), v.clone()))
                        .cloned()
                        .or_else(|| Some(v.to_string()));
                }
            }
            None
        }
    }
}

fn render_range(r: &Ranges<Version>) -> String {
    // PubGrub's `Display` for `Ranges` is human-readable and
    // stable; we adopt it directly. The error formatter (T2) may
    // post-process this.
    format!("{r}")
}

fn summarize(tree: &DerivationTree<StrictPackage, Ranges<Version>, String>) -> String {
    match tree {
        DerivationTree::External(External::NoVersions(pkg, range)) => {
            format!("no versions of {pkg} satisfy {range}")
        }
        DerivationTree::External(External::FromDependencyOf(_, _, to_pkg, to_range)) => {
            format!("conflict on dependency {to_pkg} required as {to_range}")
        }
        DerivationTree::External(External::Custom(pkg, _, msg)) => {
            format!("{pkg}: {msg}")
        }
        DerivationTree::External(External::NotRoot(pkg, _)) => {
            format!("could not pick root package {pkg}")
        }
        DerivationTree::Derived(_) => "conflicting version constraints".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walker::{FixtureSource, WalkOptions, walk};
    use barista_pom::{EffectivePom, Properties, RawDependency, RawPom};

    // ---- Test helpers ------------------------------------------------------

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

    fn resolved_versions(out: &StrictOutcome) -> BTreeMap<Coords, String> {
        match out {
            StrictOutcome::Resolved(m) => m
                .iter()
                .map(|(c, d)| (c.clone(), d.version.clone()))
                .collect(),
            StrictOutcome::Conflict(_) => panic!("expected Resolved, got Conflict"),
        }
    }

    fn walker_versions(graph: &crate::walker::ResolvedGraph) -> BTreeMap<Coords, String> {
        graph
            .winners
            .iter()
            .map(|(c, d)| (c.clone(), d.version.clone()))
            .collect()
    }

    // ---- 1. Clean graph: matches BFS walker output -------------------------

    #[tokio::test]
    async fn clean_graph_matches_walker() {
        // root -> A 1.0 -> B 1.0
        let mut src = FixtureSource::new();
        src.add_pom(
            co("ex", "A"),
            "1.0",
            pom("ex", "A", "1.0", vec![dep("ex", "B", "1.0")]),
        );
        src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));

        let root = pom("ex", "root", "1.0", vec![dep("ex", "A", "1.0")]);

        let walker_graph = walk(&resolved(root.clone()), &src, &WalkOptions::default())
            .await
            .expect("walker ok");
        let strict_out = resolve_strict(&resolved(root), &src)
            .await
            .expect("strict ok");

        assert_eq!(
            resolved_versions(&strict_out),
            walker_versions(&walker_graph),
            "strict should match BFS+Skipper on clean graphs"
        );
    }

    // ---- 2. Hard-range satisfied by exactly one version --------------------

    #[tokio::test]
    async fn hard_range_single_match() {
        // root requires A in [1.0,2.0). Available: 1.0, 2.0, 3.0.
        // Strict must pick 1.0.
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));
        src.add_pom(co("ex", "A"), "2.0", pom("ex", "A", "2.0", vec![]));
        src.add_pom(co("ex", "A"), "3.0", pom("ex", "A", "3.0", vec![]));

        let root = pom("ex", "root", "1.0", vec![dep("ex", "A", "[1.0,2.0)")]);
        let out = resolve_strict(&resolved(root), &src)
            .await
            .expect("strict ok");
        let map = resolved_versions(&out);
        assert_eq!(map.get(&co("ex", "A")).map(String::as_str), Some("1.0"));
        assert_eq!(map.len(), 1);
    }

    // ---- 3. Hard-range with multiple — pick highest -----------------------

    #[tokio::test]
    async fn hard_range_multi_picks_highest() {
        // root requires A in [1.0,) with available 1.0, 1.5, 2.0.
        // PubGrub default + our choose_version → 2.0.
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));
        src.add_pom(co("ex", "A"), "1.5", pom("ex", "A", "1.5", vec![]));
        src.add_pom(co("ex", "A"), "2.0", pom("ex", "A", "2.0", vec![]));

        let root = pom("ex", "root", "1.0", vec![dep("ex", "A", "[1.0,)")]);
        let out = resolve_strict(&resolved(root), &src)
            .await
            .expect("strict ok");
        let map = resolved_versions(&out);
        assert_eq!(map.get(&co("ex", "A")).map(String::as_str), Some("2.0"));
    }

    // ---- 4. Hard-range with no satisfying version → Conflict --------------

    #[tokio::test]
    async fn hard_range_no_match_is_conflict() {
        // root requires A in [9.0,) but only 1.0 is published.
        let mut src = FixtureSource::new();
        src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));

        let root = pom("ex", "root", "1.0", vec![dep("ex", "A", "[9.0,)")]);
        let out = resolve_strict(&resolved(root), &src)
            .await
            .expect("strict ok");
        match out {
            StrictOutcome::Conflict(d) => {
                assert!(
                    !d.root_cause.is_empty(),
                    "expected non-empty root cause: {d:?}"
                );
                // The conflict should mention the unsatisfiable
                // package somewhere (either in the summary or in
                // at least one edge).
                let mentions_a = d.root_cause.contains("ex:A")
                    || d.contributing_edges
                        .iter()
                        .any(|e| e.to_coords == co("ex", "A"));
                assert!(mentions_a, "derivation should mention ex:A: {d:?}");
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    // ---- 5. Diamond, soft preferences pick same as nearest-wins ----------

    #[tokio::test]
    async fn diamond_soft_matches_nearest_wins() {
        // root -> B 1.0 -> C 1.0
        // root -> D 1.0 -> C 2.0
        // Walker's nearest-wins: B is declared first, both C's at
        // depth 2, declaration-order tiebreak picks C 1.0.
        //
        // Strict with soft-only specs honours the soft preference
        // that was inserted first (C 1.0), so it should also pick
        // 1.0.
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

        let strict_out = resolve_strict(&resolved(root.clone()), &src)
            .await
            .expect("strict ok");
        let walker_graph = walk(&resolved(root), &src, &WalkOptions::default())
            .await
            .expect("walker ok");

        assert_eq!(
            resolved_versions(&strict_out).get(&co("ex", "C")),
            walker_versions(&walker_graph).get(&co("ex", "C")),
        );
        assert_eq!(
            resolved_versions(&strict_out)
                .get(&co("ex", "C"))
                .map(String::as_str),
            Some("1.0")
        );
    }

    // ---- 6. Diamond hard-conflict → Conflict with both edges named -------

    #[tokio::test]
    async fn diamond_hard_conflict_names_both_edges() {
        // root -> B 1.0 -> C [1.0]
        // root -> D 1.0 -> C [2.0]
        // No version satisfies both [1.0] and [2.0] → Conflict.
        let mut src = FixtureSource::new();
        src.add_pom(
            co("ex", "B"),
            "1.0",
            pom("ex", "B", "1.0", vec![dep("ex", "C", "[1.0]")]),
        );
        src.add_pom(
            co("ex", "D"),
            "1.0",
            pom("ex", "D", "1.0", vec![dep("ex", "C", "[2.0]")]),
        );
        src.add_pom(co("ex", "C"), "1.0", pom("ex", "C", "1.0", vec![]));
        src.add_pom(co("ex", "C"), "2.0", pom("ex", "C", "2.0", vec![]));

        let root = pom(
            "ex",
            "root",
            "1.0",
            vec![dep("ex", "B", "1.0"), dep("ex", "D", "1.0")],
        );

        let out = resolve_strict(&resolved(root), &src)
            .await
            .expect("strict ok");
        match out {
            StrictOutcome::Conflict(d) => {
                // The derivation should name both edges pointing at C.
                let edges_to_c: Vec<&DepEdge> = d
                    .contributing_edges
                    .iter()
                    .filter(|e| e.to_coords == co("ex", "C"))
                    .collect();
                assert!(
                    edges_to_c.len() >= 2,
                    "expected at least 2 edges to ex:C, got {}: {:#?}",
                    edges_to_c.len(),
                    d.contributing_edges
                );
                // The two required ranges must differ — one is
                // [1.0], the other [2.0].
                let ranges: HashSet<_> = edges_to_c
                    .iter()
                    .map(|e| e.required_range.clone())
                    .collect();
                assert!(
                    ranges.len() >= 2,
                    "expected distinct required_ranges, got {ranges:?}"
                );
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    // ---- 7. Cycle in dep graph terminates ----------------------------------

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
        let out = resolve_strict(&resolved(root), &src)
            .await
            .expect("strict ok");
        let map = resolved_versions(&out);
        assert_eq!(map.get(&co("ex", "A")).map(String::as_str), Some("1.0"));
        assert_eq!(map.get(&co("ex", "B")).map(String::as_str), Some("1.0"));
    }

    // ---- 8. Empty root deps -----------------------------------------------

    #[tokio::test]
    async fn empty_root_deps() {
        let src = FixtureSource::new();
        let root = pom("ex", "root", "1.0", vec![]);
        let out = resolve_strict(&resolved(root), &src)
            .await
            .expect("strict ok");
        match out {
            StrictOutcome::Resolved(m) => assert!(m.is_empty(), "expected empty, got {m:?}"),
            other => panic!("expected empty Resolved, got {other:?}"),
        }
    }

    // ---- Bonus oracle test: deeper graph against walker -------------------

    #[tokio::test]
    async fn deeper_chain_matches_walker() {
        // root -> A 1.0 -> B 1.0 -> C 1.0 -> D 1.0
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
        let walker_graph = walk(&resolved(root.clone()), &src, &WalkOptions::default())
            .await
            .expect("walker ok");
        let strict_out = resolve_strict(&resolved(root), &src)
            .await
            .expect("strict ok");

        assert_eq!(
            resolved_versions(&strict_out),
            walker_versions(&walker_graph),
        );
    }
}
