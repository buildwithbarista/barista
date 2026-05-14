//! `barista pour` — materialize resolved artifacts into a target
//! environment.
//!
//! Conceptually this is the Maven-equivalent of
//! `mvn dependency:resolve` plus an install step: every artifact
//! locked in `barista.lock` is materialized at a Maven-conventional
//! path under a target root (default: `~/.m2/repository`) as a
//! hardlink to the content-addressed cache. Idempotent and cheap;
//! re-running `pour` is a no-op when nothing has changed.
//!
//! # End-to-end shape
//!
//! 1. Resolve the project root (CWD walk-up, `--root`, or `-f`).
//! 2. Load the layered effective config so we know the cache root
//!    and the default Maven local-repository path.
//! 3. Read `barista.lock`.
//! 4. For each lock entry that matches `--scope`, look up the CAS
//!    blob by its `sha256` and hardlink it into
//!    `<target>/<group/slashed>/<artifact>/<version>/...`.
//! 5. Print a one-line summary.
//!
//! # v0.1 scope
//!
//! Like [`crate::cmd::pull`], the full path that would *fetch*
//! missing artifacts from the network is not yet wired (that work
//! tracks alongside the M3.x cache-pipeline tasks). `pour` therefore
//! operates only on artifacts **already in the local CAS**. If a
//! locked coordinate is missing, [`PourError::NotInCache`] surfaces
//! every missing coord and points the user at `barista pull` to
//! populate the cache.

use std::path::{Path, PathBuf};

use barista_cache::cas::{Cas, ContentHash};
use barista_cache::m2::materialize;
use barista_config::{Config, LoadAudit, LoaderError, LoaderInputs, load_effective_config};
use barista_coords::Coords;
use barista_lockfile::{Lockfile, LockfileEntry, LockfileError};

use crate::cli::{GlobalFlags, PourArgs, ScopeArg};
use crate::project::{ResolveError, ResolveInputs, resolve_project_root};

/// Run `barista pour`.
///
/// Returns the process exit code:
///
/// - `0` on success (including a dry-run that completes cleanly).
/// - `1` on internal errors (I/O, corrupt lockfile, etc.).
/// - `2` on user/precondition errors: no lockfile, artifacts missing
///   from the cache, malformed coords in the lockfile.
pub fn run(global: &GlobalFlags, args: &PourArgs) -> i32 {
    match run_inner(global, args) {
        Ok(report) => {
            if !global.quiet {
                eprintln!("pour: {}", report.summary());
            }
            0
        }
        Err(e) if e.is_precondition() => {
            eprintln!("error: barista pour failed: {e}");
            2
        }
        Err(e) => {
            eprintln!("error: barista pour failed: {e}");
            1
        }
    }
}

/// Library-friendly entry point used by [`run`] and integration
/// tests. Drives the full pipeline and returns a structured report
/// on success.
pub fn run_inner(global: &GlobalFlags, args: &PourArgs) -> Result<PourReport, PourError> {
    // -- 1. Project root --------------------------------------------------
    let root = resolve_project_root(ResolveInputs {
        root: global.root.clone(),
        file: global.file.clone(),
        ..Default::default()
    })?;

    // -- 2. Effective config ---------------------------------------------
    let (config, _audit): (Config, LoadAudit) = load_effective_config(LoaderInputs {
        project_config_path: Some(root.root.join("barista.toml")),
        cwd_override: Some(root.root.clone()),
        ..Default::default()
    })?;

    // -- 3. Lockfile ------------------------------------------------------
    let lock_path = root.root.join("barista.lock");
    if !lock_path.exists() {
        return Err(PourError::NoLockfile {
            expected_at: lock_path,
            hint: "run `barista pull` first to resolve dependencies and create barista.lock"
                .to_string(),
        });
    }
    let lockfile = Lockfile::read(&lock_path)?;

    // -- 4. Decide what to materialize -----------------------------------
    let requested_scope = scope_str(args.scope);
    let selected: Vec<&LockfileEntry> = lockfile
        .entries
        .iter()
        .filter(|e| scope_matches(requested_scope, &e.scope))
        .collect();

    let target = args
        .target
        .clone()
        .unwrap_or_else(|| config.paths.m2_repository.clone());

    // -- 5. Dry-run short-circuits before opening the CAS ----------------
    if args.dry_run {
        let plan = plan_dry_run(&selected, &target)?;
        return Ok(PourReport {
            target,
            scope: requested_scope.to_string(),
            considered: lockfile.entries.len(),
            planned: plan.len(),
            materialized: 0,
            dry_run: true,
            planned_paths: plan,
        });
    }

    // -- 6. Verify the CAS has every selected artifact -------------------
    let cas = Cas::open(&config.paths.cache_dir).map_err(|e| PourError::CasOpen {
        path: config.paths.cache_dir.clone(),
        detail: e.to_string(),
    })?;

    let mut missing: Vec<String> = Vec::new();
    let mut materialize_plan: Vec<(ContentHash, &LockfileEntry)> = Vec::new();
    for entry in &selected {
        let hash = ContentHash::from_hex(&entry.sha256).map_err(|e| PourError::BadHash {
            coord: format_coord(entry),
            detail: e.to_string(),
        })?;
        if cas.contains(&hash) {
            materialize_plan.push((hash, entry));
        } else {
            missing.push(format_coord(entry));
        }
    }
    if !missing.is_empty() {
        return Err(PourError::NotInCache { coords: missing });
    }

    // -- 7. Materialize ---------------------------------------------------
    let mut materialized_paths = Vec::with_capacity(materialize_plan.len());
    for (hash, entry) in &materialize_plan {
        let coords = parse_coords(&entry.coords).map_err(|e| PourError::BadCoords {
            coord: entry.coords.clone(),
            detail: e,
        })?;
        let dest = materialize(
            &cas,
            hash,
            &target,
            &coords,
            &entry.version,
            entry.classifier.as_deref(),
            &entry.type_,
        )
        .map_err(|e| PourError::Mirror {
            coord: format_coord(entry),
            detail: e.to_string(),
        })?;
        materialized_paths.push(dest);
    }

    Ok(PourReport {
        target,
        scope: requested_scope.to_string(),
        considered: lockfile.entries.len(),
        planned: materialize_plan.len(),
        materialized: materialized_paths.len(),
        dry_run: false,
        planned_paths: materialized_paths,
    })
}

/// Build the list of Maven-conventional destination paths a real
/// materialize would write to. Used by `--dry-run` so the user can
/// preview the plan without opening the CAS.
fn plan_dry_run(selected: &[&LockfileEntry], target: &Path) -> Result<Vec<PathBuf>, PourError> {
    let mut out = Vec::with_capacity(selected.len());
    for entry in selected {
        let coords = parse_coords(&entry.coords).map_err(|e| PourError::BadCoords {
            coord: entry.coords.clone(),
            detail: e,
        })?;
        out.push(barista_cache::m2::m2_path(
            target,
            &coords,
            &entry.version,
            entry.classifier.as_deref(),
            &entry.type_,
        ));
    }
    Ok(out)
}

/// Convert a [`ScopeArg`] to its lockfile string form.
fn scope_str(s: ScopeArg) -> &'static str {
    match s {
        ScopeArg::Compile => "compile",
        ScopeArg::Runtime => "runtime",
        ScopeArg::Test => "test",
        ScopeArg::Provided => "provided",
        ScopeArg::System => "system",
    }
}

/// Decide whether a lockfile entry's scope satisfies the requested
/// scope filter.
///
/// Today the policy is exact-match; this is the simplest
/// interpretation of "limit to compile" and matches the acceptance
/// criterion that `--scope compile` excludes `test`-only deps. A
/// future revision can expand `compile` to include `runtime` and
/// `provided` if needed to match `mvn dependency:resolve` more
/// faithfully.
fn scope_matches(requested: &str, entry_scope: &str) -> bool {
    requested.eq_ignore_ascii_case(entry_scope)
}

/// Render the `group:artifact:version` form of an entry for error
/// messages. Includes the classifier when present.
fn format_coord(e: &LockfileEntry) -> String {
    match e.classifier.as_deref() {
        Some(c) => format!("{}:{}:{}:{}", e.coords, e.version, c, e.type_),
        None => format!("{}:{}", e.coords, e.version),
    }
}

/// Parse a `group:artifact` coords string from a lockfile entry into
/// a [`Coords`]. Wraps the error for clean propagation.
fn parse_coords(s: &str) -> Result<Coords, String> {
    s.parse::<Coords>().map_err(|e| e.to_string())
}

/// Result of a successful `barista pour` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PourReport {
    /// The directory artifacts were (or would be) materialized into.
    pub target: PathBuf,
    /// The scope filter that was applied, e.g. `"compile"`.
    pub scope: String,
    /// Total entries in the lockfile (before filtering).
    pub considered: usize,
    /// Entries selected after scope filtering.
    pub planned: usize,
    /// Entries actually materialized. `0` for `--dry-run`.
    pub materialized: usize,
    /// `true` when this was a `--dry-run`.
    pub dry_run: bool,
    /// Destination paths. For real runs, these are the paths
    /// actually written. For dry-runs, the paths that *would* be
    /// written. Same length as [`Self::planned`].
    pub planned_paths: Vec<PathBuf>,
}

impl PourReport {
    /// Render a single human-readable summary line.
    pub fn summary(&self) -> String {
        let mode = if self.dry_run { "dry-run: " } else { "" };
        format!(
            "{mode}{} of {} entries (scope={}) → {}",
            if self.dry_run {
                self.planned
            } else {
                self.materialized
            },
            self.considered,
            self.scope,
            self.target.display(),
        )
    }
}

/// Errors surfaced from `barista pour`.
#[derive(Debug, thiserror::Error)]
pub enum PourError {
    #[error("project setup: {0}")]
    Project(#[from] ResolveError),

    #[error("config load: {0}")]
    Config(#[from] LoaderError),

    #[error("lockfile: {0}")]
    Lockfile(#[from] LockfileError),

    #[error("no barista.lock at {expected_at:?}\n  hint: {hint}")]
    NoLockfile { expected_at: PathBuf, hint: String },

    #[error("opening cache at {path:?}: {detail}")]
    CasOpen { path: PathBuf, detail: String },

    #[error("bad sha256 on entry {coord}: {detail}")]
    BadHash { coord: String, detail: String },

    #[error("invalid coords {coord:?}: {detail}")]
    BadCoords { coord: String, detail: String },

    #[error(
        "{} artifact(s) missing from the local cache; run `barista pull` to fetch them:\n  - {}",
        coords.len(),
        coords.join("\n  - ")
    )]
    NotInCache { coords: Vec<String> },

    #[error("materializing {coord}: {detail}")]
    Mirror { coord: String, detail: String },

    #[error("not yet implemented: {detail}")]
    NotYetImplemented { detail: String },
}

impl PourError {
    /// True for user-fixable / precondition errors that should exit
    /// with code `2` (vs `1` for internal/unexpected failures).
    fn is_precondition(&self) -> bool {
        matches!(
            self,
            PourError::Project(_)
                | PourError::NoLockfile { .. }
                | PourError::NotInCache { .. }
                | PourError::BadCoords { .. }
                | PourError::BadHash { .. }
                | PourError::NotYetImplemented { .. }
        )
    }
}
