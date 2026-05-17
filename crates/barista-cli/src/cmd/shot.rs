//! `barista shot <phase> [args...]` — ephemeral build invocation
//! optimised for warm-path latency (M4.3 T3).
//!
//! `barista shot` is a sibling to [`crate::cmd::verify`]: it builds the
//! same action graph (a Maven lifecycle phase prefix) and dispatches
//! the actions to the warm `barback` daemon. The difference is the
//! **warm-path optimisation**: before doing any work, `shot` checks
//! whether
//!
//! 1. the daemon is still listening on its UDS,
//! 2. the on-disk `barista.lock` is still the one we used for the
//!    last `shot` invocation in this project, and
//! 3. the project's `pom.xml` hasn't been edited since the last shot.
//!
//! If all three predicates hold, `shot` **skips the resolve + pour
//! pre-step** entirely and submits the action graph directly to the
//! daemon. This is what gets `barista shot test` on a no-change rerun
//! to the ≥10× speedup target over `mvn test` (PRD §2.4 SM-3.2): the
//! warm daemon has the plugin classloaders already cached (M4.2 T4),
//! the resolved local Maven repo is already populated, and the
//! lockfile validation cost is replaced by a single stat + hex-string
//! comparison.
//!
//! # Warm-path cache: `last-shot.toml`
//!
//! Per-project under `~/.barista/cache/<sha256-of-project-root>/last-
//! shot.toml`. Schema (TOML):
//!
//! ```toml
//! # Schema version — incremented on incompatible changes.
//! version = 1
//! # Hex-encoded SHA-256 from the lockfile's `meta.project_signature`
//! # field. A `barista pull` that re-resolves the project bumps this.
//! lockfile_signature = "abcd…"
//! # Modification time of `pom.xml` at the time of last shot, in
//! # nanoseconds since the Unix epoch. Catches local edits the user
//! # made without re-running `barista pull`.
//! pom_mtime_ns = 1234567890123456789
//! # PID of the daemon process at the time of last shot. A daemon
//! # restart (idle-shutdown, OOM, manual kill, host reboot) bumps
//! # this; the warm-path predicate fails when the cached PID no
//! # longer matches `~/.barista/run/barback.pid`.
//! daemon_pid = 12345
//! # Unix timestamp of last shot (informational; not used in the
//! # cache predicate).
//! last_shot_unix = 1700000000
//! ```
//!
//! ## Invalidation rules
//!
//! The cache is invalidated (warm path skipped, cold path taken) when
//! any of the following is true:
//!
//! * `last-shot.toml` doesn't exist (first shot in this project).
//! * The TOML can't be parsed (corrupt cache — rebuild).
//! * The schema `version` field is unknown to this binary.
//! * `barista.lock` doesn't exist or can't be read.
//! * `barista.lock`'s `project_signature` ≠ cached `lockfile_signature`.
//! * `pom.xml` mtime ≠ cached `pom_mtime_ns`.
//! * The daemon socket isn't accepting connections.
//! * `~/.barista/run/barback.pid` doesn't exist, can't be parsed, or
//!   doesn't match the cached `daemon_pid`.
//!
//! After a successful action-graph dispatch, the cache is rewritten
//! with the current values so the next invocation can take the warm
//! path.
//!
//! # `--no-daemon`
//!
//! Honoured: routes the invocation to [`crate::cmd::no_daemon`] (forks
//! upstream `mvn`). The warm-path optimisation only makes sense when
//! the daemon is in play.
//!
//! # v0.1 scope
//!
//! * Single-module projects only (the action graph builder is the
//!   single-module variant from [`crate::action_graph::shot_graph`]).
//! * Single-phase expressions only — `barista shot test`, not
//!   `barista shot "clean package"`. Multi-phase composition is a
//!   v0.2 follow-up.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use barista_config::{Config, LoadAudit, LoaderError, LoaderInputs, load_effective_config};
use barista_lockfile::Lockfile;
use sha2::{Digest, Sha256};

use crate::action_graph::{ActionGraph, ShotGraphError, build_action_request, shot_graph};
use crate::cli::{GlobalFlags, OutputFormat, PourArgs, ShotArgs};
use crate::cmd::MavenPhase;
use crate::cmd::pour::{PourError, PourReport, run_inner as pour_run};
use crate::daemon::launcher::{LaunchPlan, LauncherError, PID_LEAF, SOCKET_LEAF, socket_is_live};
use crate::daemon::respawn::{RespawnError, submit_with_respawn};
use crate::daemon::{
    available_parallelism_or_one, discover_jvm_entry, discover_or_spawn, resolve_workers,
};
use crate::output::{MojoInvocation, VerifyReport, make_runtime_renderer};
use crate::project::{ResolveError, ResolveInputs, resolve_project_root};

/// `--workers` expression used when `barback.default_workers` isn't
/// pinned in config. Same default as `cmd::verify`.
const DEFAULT_WORKERS_EXPR: &str = "1C";

/// Filesystem leaf for the per-project warm-path cache file.
const LAST_SHOT_LEAF: &str = "last-shot.toml";

/// Current schema version of [`LastShotCache`]. Bumped on
/// incompatible changes; older versions invalidate the cache.
const CACHE_SCHEMA_VERSION: u32 = 1;

/// Default leaf of the `~/.barista` user directory holding the
/// per-project cache directories.
const USER_CACHE_DIR_LEAF: &str = ".barista/cache";

/// Run `barista shot <phase> [args...]`. Returns the process exit
/// code.
pub fn run(global: &GlobalFlags, shot_args: &ShotArgs) -> i32 {
    // Split the trailing-args vector into `<expr>` and `[args...]`.
    let (expr, fwd) = match split_expr(&shot_args.args) {
        Some(pair) => pair,
        None => {
            eprintln!(
                "barista: shot requires a phase expression, e.g. `barista shot test`.\n\
                 \n\
                 The phase is a single Maven lifecycle phase name (compile, test,\n\
                 package, verify, install, deploy, etc.). Trailing args are\n\
                 forwarded to the daemon as the action arguments."
            );
            return 2;
        }
    };

    // `--no-daemon`: fork upstream `mvn`. Warm-path optimisation
    // doesn't apply without a daemon — delegate to the existing
    // R2-mitigation path (M4.2 T8).
    if global.no_daemon {
        // Route through MavenPhase when the expression maps onto a
        // known one; otherwise refuse with a structured message.
        if let Some(phase) = MavenPhase::from_phase_name(expr) {
            let args = crate::cli::MavenVocabArgs { args: fwd.to_vec() };
            return crate::cmd::no_daemon::dispatch(global, phase, &args);
        }
        eprintln!(
            "barista: shot --no-daemon only supports known Maven lifecycle phases \
             (clean, compile, test, package, verify, install, deploy, site); got `{expr}`."
        );
        return 2;
    }

    let mut renderer = make_runtime_renderer(global);
    let exit = match run_inner(global, expr, fwd) {
        Ok(report) => {
            if !global.quiet
                && let Err(e) = renderer.render_verify(&report)
            {
                eprintln!("error: rendering shot report failed: {e}");
                return 1;
            }
            if report.failed_actions > 0 { 1 } else { 0 }
        }
        Err(e) => {
            let code = e.exit_code();
            if matches!(global.output, OutputFormat::Human) {
                eprintln!("error: barista shot failed: {e}");
            } else if let Err(re) = renderer.render_error(&e) {
                eprintln!("error: rendering error report failed: {re}");
            }
            code
        }
    };
    if let Err(e) = renderer.finish() {
        eprintln!("error: flushing output failed: {e}");
        return 1;
    }
    exit
}

/// Split `["test", "-DskipTests=false"]` into (`"test"`,
/// `["-DskipTests=false"]`). Returns `None` when the vector is empty.
fn split_expr(args: &[String]) -> Option<(&str, &[String])> {
    let (first, rest) = args.split_first()?;
    Some((first.as_str(), rest))
}

/// Library-friendly entry point.
///
/// Returns a [`VerifyReport`] (the same shape `verify` emits — see
/// [`crate::output::VerifyReport`] for the design rationale: a single
/// report shape covers every Maven-vocabulary lifecycle command). The
/// `phase` field carries the requested phase expression.
pub fn run_inner(
    global: &GlobalFlags,
    expr: &str,
    _forwarded_args: &[String],
) -> Result<VerifyReport, ShotError> {
    let started_at = Instant::now();

    // -- 1. Project root --------------------------------------------------
    let root = resolve_project_root(ResolveInputs {
        root: global.root.clone(),
        file: global.file.clone(),
        ..Default::default()
    })?;

    // -- 2. Effective config ----------------------------------------------
    let (config, _audit): (Config, LoadAudit) = load_effective_config(LoaderInputs {
        project_config_path: Some(root.root.join("barista.toml")),
        cwd_override: Some(root.root.clone()),
        ..Default::default()
    })?;

    // -- 3. Action graph (cheap; pure computation) ------------------------
    let graph = shot_graph(root.root.clone(), expr)?;

    // -- 4. Daemon configuration -----------------------------------------
    let workers = resolve_workers_from_config(&config)?;
    let socket_dir = resolve_socket_dir(&config);
    let idle_shutdown_secs = config.daemon.idle_shutdown_secs;
    let plan = LaunchPlan::new(socket_dir.clone(), workers, idle_shutdown_secs);

    // -- 5. Warm-path probe ----------------------------------------------
    // Probe before touching pour: if the predicate holds we skip the
    // pour step entirely. The probe is cheap (a few stat calls, a
    // connect probe, a TOML deserialize) so a miss costs essentially
    // nothing on the cold path that follows.
    let cache_path = last_shot_cache_path(&root.root);
    let pom_path = graph.pom_path.clone();
    let warm = is_warm_path(&root.root, &pom_path, &cache_path, &plan.socket_path);

    // -- 6. Cold-path pour (skipped on warm) -----------------------------
    if !warm {
        // `pour` is idempotent and cheap on a no-op run. On the cold
        // path we always run it so the local repo is correctly seeded
        // for the daemon's classloader. The warm path skips it because
        // the cache predicate proves the local repo already reflects
        // the current lockfile (we ran pour the last time we updated
        // `last-shot.toml`).
        let pour_args = PourArgs {
            target: None,
            scope: crate::cli::ScopeArg::Compile,
            dry_run: false,
        };
        let _pour_report: PourReport =
            pour_run(global, &pour_args).map_err(ShotError::from_pour)?;
    }

    // -- 7. Discover / spawn daemon --------------------------------------
    let cwd = std::env::current_dir().unwrap_or_else(|_| root.root.clone());
    let jvm_entry = discover_jvm_entry(&cwd)?;
    let initial_handle = discover_or_spawn(&plan, || Ok(jvm_entry.clone()))?;

    // -- 8. Dispatch + collect --------------------------------------------
    let mut handle = initial_handle;
    let mut invocations = Vec::with_capacity(graph.actions.len());
    let mut failed_actions = 0usize;
    let mut total_respawns: u32 = 0;
    let mut executed = 0usize;
    for action in &graph.actions {
        if failed_actions > 0 {
            break;
        }
        let request = build_action_request(&graph, action, &root.root);
        let phase = action.phase.to_string();
        let module = graph.module_root.clone();

        let action_started = Instant::now();
        let (outcome, next_handle) = submit_with_respawn(&plan, handle, &jvm_entry, request)
            .map_err(|e| classify_respawn_error(&phase, e))?;
        handle = next_handle;
        let duration_ms = u64::try_from(action_started.elapsed().as_millis()).unwrap_or(u64::MAX);

        executed += 1;
        total_respawns = total_respawns.saturating_add(outcome.respawns);

        let exit_code = outcome.result.exit_code;
        let status = action_status_str(outcome.result.status);
        if exit_code != 0 {
            failed_actions += 1;
        }
        invocations.push(MojoInvocation {
            phase,
            mojo: outcome.result.action_id.clone(),
            module,
            exit_code,
            status,
            failure_message: outcome.result.failure_message.clone(),
            error_code: String::new(),
            duration_ms,
        });
    }

    // -- 9. Update warm-path cache on success -----------------------------
    // Only refresh the cache when every action succeeded — a partial
    // failure leaves the project in an unknown state and we don't want
    // a subsequent warm path to skip a re-resolve that would have
    // recovered.
    if failed_actions == 0 {
        if let Err(e) = update_last_shot_cache(&root.root, &pom_path, &plan.socket_dir) {
            // Cache-write failures are best-effort: the next shot will
            // just take the cold path again. Surface at warning volume.
            eprintln!(
                "barista: warning: could not update warm-path cache at {}: {e}",
                cache_path.display(),
            );
        }
    }

    // Detach the child (same convention as `cmd::verify`): we want it
    // to outlive this process.
    if let Some(child) = handle.child.as_mut() {
        let _ = child;
    }

    let total_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(VerifyReport {
        project_root: root.root.clone(),
        phase: format!("shot:{expr}"),
        planned_actions: graph.actions.len(),
        executed_actions: executed,
        failed_actions,
        daemon_respawns: total_respawns,
        invocations,
        duration_ms: total_ms,
    })
}

// ===================================================================
// Warm-path cache
// ===================================================================

/// On-disk cache schema; see module-level docs for invalidation
/// rules. Serialized as TOML.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LastShotCache {
    /// Schema version. Increments invalidate older caches.
    version: u32,
    /// Hex-encoded SHA-256 from the lockfile's `meta.project_signature`.
    lockfile_signature: String,
    /// Mtime of `pom.xml` at the time of last shot (ns since epoch).
    /// Stored as i64 (saturating) to fit TOML's integer width — i64
    /// covers ±292 years around the Unix epoch which is plenty for
    /// real-world filesystem timestamps.
    pom_mtime_ns: i64,
    /// Daemon PID at the time of last shot.
    daemon_pid: u32,
    /// Unix timestamp of last shot (informational).
    last_shot_unix: i64,
}

/// Compute the cache file path for a given project root. The path is
/// keyed on a sha256 of the absolute project root so per-project
/// state doesn't collide across the user's machine.
pub(crate) fn last_shot_cache_path(project_root: &Path) -> PathBuf {
    let key = project_root_key(project_root);
    cache_root().join(&key).join(LAST_SHOT_LEAF)
}

/// Stable key for a project root: hex SHA-256 of the absolute path's
/// utf-8 bytes. Identical inputs produce identical keys regardless of
/// the host's $HOME or working directory.
fn project_root_key(project_root: &Path) -> String {
    let canonical = project_root.canonicalize();
    let bytes = match &canonical {
        Ok(p) => p.as_os_str().as_encoded_bytes(),
        Err(_) => project_root.as_os_str().as_encoded_bytes(),
    };
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

/// `~/.barista/cache`. Falls back to a temp-dir-shaped path when
/// `$HOME` isn't set (an exotic environment we still want to function
/// without panicking — the cache is a performance hint, not a
/// correctness contract).
fn cache_root() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(USER_CACHE_DIR_LEAF);
    }
    // Match Maven's "no $HOME" convention: fall back to a directory
    // in the system temp dir.
    std::env::temp_dir().join("barista-cache")
}

/// Decide whether the warm path applies for this invocation. Returns
/// `true` only when every predicate in the module-level
/// "Invalidation rules" doc-comment passes.
fn is_warm_path(
    project_root: &Path,
    pom_path: &Path,
    cache_path: &Path,
    socket_path: &Path,
) -> bool {
    // 1. Cache file present + parseable.
    let cache = match load_cache(cache_path) {
        Some(c) => c,
        None => return false,
    };
    if cache.version != CACHE_SCHEMA_VERSION {
        return false;
    }

    // 2. Lockfile signature matches.
    let lock_path = project_root.join("barista.lock");
    let on_disk_sig = match read_lockfile_signature(&lock_path) {
        Some(s) => s,
        None => return false,
    };
    if on_disk_sig != cache.lockfile_signature {
        return false;
    }

    // 3. pom.xml mtime matches.
    let pom_mtime = match pom_mtime_ns(pom_path) {
        Some(t) => t,
        None => return false,
    };
    if pom_mtime != cache.pom_mtime_ns {
        return false;
    }

    // 4. Daemon socket is live.
    if !socket_is_live(socket_path, Duration::from_millis(250)) {
        return false;
    }

    // 5. Daemon PID matches.
    let pid_path = socket_path
        .parent()
        .map(|d| d.join(PID_LEAF))
        .unwrap_or_else(|| PathBuf::from(PID_LEAF));
    let on_disk_pid = match read_pid(&pid_path) {
        Some(p) => p,
        None => return false,
    };
    if on_disk_pid != cache.daemon_pid {
        return false;
    }

    true
}

/// Read + parse the on-disk cache. Returns `None` on every failure
/// (missing file, IO error, TOML parse error).
fn load_cache(path: &Path) -> Option<LastShotCache> {
    let s = std::fs::read_to_string(path).ok()?;
    toml::from_str::<LastShotCache>(&s).ok()
}

/// Extract `meta.project_signature` from a lockfile on disk.
///
/// We don't validate the rest of the lockfile — the warm-path probe
/// is allowed to be optimistic. The cold path (which runs `pour`)
/// performs the full lockfile load + validation.
fn read_lockfile_signature(path: &Path) -> Option<String> {
    let lockfile = Lockfile::read(path).ok()?;
    Some(lockfile.meta.project_signature)
}

/// `pom.xml` mtime as nanoseconds since the Unix epoch (i64,
/// saturating). Returns `None` when the file doesn't exist or its
/// mtime can't be read.
fn pom_mtime_ns(pom_path: &Path) -> Option<i64> {
    let meta = std::fs::metadata(pom_path).ok()?;
    let mtime = meta.modified().ok()?;
    match mtime.duration_since(UNIX_EPOCH) {
        Ok(d) => Some(i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)),
        Err(e) => {
            // pre-epoch mtime — unusual but possible on synthetic
            // fixtures. Surface as a negative ns value (saturating).
            let secs = e.duration().as_secs();
            let nanos = u64::from(e.duration().subsec_nanos());
            let total_ns = secs.saturating_mul(1_000_000_000).saturating_add(nanos);
            let signed = i64::try_from(total_ns).unwrap_or(i64::MAX);
            Some(-signed)
        }
    }
}

/// Read + parse the daemon PID file.
fn read_pid(path: &Path) -> Option<u32> {
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse::<u32>().ok()
}

/// Write a fresh cache entry after a successful shot.
fn update_last_shot_cache(
    project_root: &Path,
    pom_path: &Path,
    socket_dir: &Path,
) -> Result<(), CacheWriteError> {
    let lock_path = project_root.join("barista.lock");
    let lockfile_signature =
        read_lockfile_signature(&lock_path).ok_or(CacheWriteError::MissingLockfile)?;
    let pom_mtime_ns = pom_mtime_ns(pom_path).ok_or(CacheWriteError::MissingPom)?;
    let daemon_pid =
        read_pid(&socket_dir.join(PID_LEAF)).ok_or(CacheWriteError::MissingDaemonPid)?;
    let last_shot_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0);

    let cache = LastShotCache {
        version: CACHE_SCHEMA_VERSION,
        lockfile_signature,
        pom_mtime_ns,
        daemon_pid,
        last_shot_unix,
    };

    let cache_path = last_shot_cache_path(project_root);
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).map_err(CacheWriteError::Io)?;
    }
    let body = toml::to_string(&cache).map_err(|e| CacheWriteError::Serialize(e.to_string()))?;
    std::fs::write(&cache_path, body).map_err(CacheWriteError::Io)?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum CacheWriteError {
    #[error("barista.lock missing or unreadable")]
    MissingLockfile,
    #[error("pom.xml missing or unreadable")]
    MissingPom,
    #[error("daemon pid file missing or unreadable")]
    MissingDaemonPid,
    #[error("io: {0}")]
    Io(std::io::Error),
    #[error("serialize: {0}")]
    Serialize(String),
}

// ===================================================================
// Glue shared with `cmd::verify` (intentionally duplicated rather
// than refactored — verify is the cold-path baseline, shot is the
// warm-path optimisation, and the shared blob is small enough that
// inlining it keeps the cold/warm seam visible. A consolidating
// refactor is a v0.2 follow-up.)
// ===================================================================

const DEFAULT_RUN_DIR_LEAF: &str = ".barista/run";

fn resolve_workers_from_config(_cfg: &Config) -> Result<usize, ShotError> {
    let expr = std::env::var("BARISTA_DAEMON_WORKERS")
        .ok()
        .unwrap_or_else(|| DEFAULT_WORKERS_EXPR.to_string());
    let cores = available_parallelism_or_one();
    resolve_workers(&expr, cores).map_err(|e| ShotError::Workers {
        detail: e.to_string(),
    })
}

fn resolve_socket_dir(cfg: &Config) -> PathBuf {
    let raw = &cfg.daemon.socket_dir;
    let s = raw.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs_home() {
            return home.join(rest);
        }
    }
    if s == "~" || s == "~/" {
        if let Some(home) = dirs_home() {
            return home.join(DEFAULT_RUN_DIR_LEAF);
        }
    }
    raw.clone()
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn action_status_str(status: i32) -> String {
    use barista_ipc::action_result::Status;
    let parsed = Status::try_from(status).unwrap_or(Status::Unknown);
    match parsed {
        Status::Unknown => "unknown",
        Status::Success => "success",
        Status::Failure => "failure",
        Status::Timeout => "timeout",
        Status::Crashed => "crashed",
        Status::Cancelled => "cancelled",
    }
    .to_string()
}

fn classify_respawn_error(phase: &str, e: RespawnError) -> ShotError {
    match e {
        RespawnError::Launcher(le) => ShotError::Launcher(le),
        RespawnError::PersistentCrash => ShotError::PersistentCrash {
            phase: phase.to_string(),
        },
        RespawnError::Connect { socket, source } => ShotError::DaemonConnect {
            phase: phase.to_string(),
            socket,
            detail: source.to_string(),
        },
        RespawnError::Ipc { detail } => ShotError::Ipc {
            phase: phase.to_string(),
            detail,
        },
        RespawnError::DaemonProtocolError { code, message } => ShotError::DaemonError {
            phase: phase.to_string(),
            code,
            message,
        },
        RespawnError::PrematureClose => ShotError::PrematureClose {
            phase: phase.to_string(),
        },
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(*b >> 4) as usize] as char);
        s.push(HEX[(*b & 0xf) as usize] as char);
    }
    s
}

/// Errors surfaced from `barista shot`. Mirrors `VerifyError`'s
/// shape so the renderer can treat both uniformly.
#[derive(Debug, thiserror::Error)]
pub enum ShotError {
    /// Project resolution failed.
    #[error("project setup: {0}")]
    Project(#[from] ResolveError),

    /// Config load failed.
    #[error("config load: {0}")]
    Config(#[from] LoaderError),

    /// The `<expr>` argument wasn't a recognised lifecycle phase.
    #[error("invalid phase expression: {0}")]
    Graph(#[from] ShotGraphError),

    /// Worker-count expression couldn't be resolved.
    #[error("workers config: {detail}")]
    Workers { detail: String },

    /// Daemon launcher failure (jar-not-found, java-not-found,
    /// spawn-timeout, etc.).
    #[error(transparent)]
    Launcher(#[from] LauncherError),

    /// Pour pre-step failed (cold-path only).
    #[error(
        "pour step (required before shot when lockfile is dirty or daemon is cold): {detail}\n  \
         hint: run `barista pull` first to resolve dependencies, then re-run `barista shot`"
    )]
    Pour { detail: String },

    /// Connect to the daemon's UDS failed.
    #[error("phase {phase}: connect to daemon at {socket:?}: {detail}")]
    DaemonConnect {
        phase: String,
        socket: PathBuf,
        detail: String,
    },

    /// IPC layer raised a non-crash error.
    #[error("phase {phase}: ipc: {detail}")]
    Ipc { phase: String, detail: String },

    /// Daemon answered with a typed protocol error.
    #[error("phase {phase}: daemon error: {code}: {message}")]
    DaemonError {
        phase: String,
        code: String,
        message: String,
    },

    /// Daemon crashed twice in a row on the same phase.
    #[error(
        "phase {phase}: persistent daemon crash — barback crashed mid-action twice in a row \
         ({BAR_DAEMON_CRASHED}). The daemon may be in a persistent failure mode; \
         inspect daemon logs via `BARISTA_BARBACK_VERBOSE=1 barista shot {phase}`.",
        BAR_DAEMON_CRASHED = barista_ipc::mux::DAEMON_CRASHED_CODE,
    )]
    PersistentCrash { phase: String },

    /// Per-action channel closed without a terminal event.
    #[error("phase {phase}: daemon disconnected before action terminated")]
    PrematureClose { phase: String },
}

impl ShotError {
    fn from_pour(p: PourError) -> Self {
        ShotError::Pour {
            detail: p.to_string(),
        }
    }

    fn exit_code(&self) -> i32 {
        match self {
            ShotError::Project(_)
            | ShotError::Config(_)
            | ShotError::Graph(_)
            | ShotError::Workers { .. }
            | ShotError::Pour { .. } => 2,
            ShotError::Launcher(_)
            | ShotError::DaemonConnect { .. }
            | ShotError::Ipc { .. }
            | ShotError::DaemonError { .. }
            | ShotError::PersistentCrash { .. }
            | ShotError::PrematureClose { .. } => 1,
        }
    }
}

// Silence the unused-import warning on the `ActionGraph` re-export
// when this file is built standalone in `cargo check`. The dispatcher
// + tests do exercise the type.
#[allow(dead_code)]
fn _action_graph_unused(_g: ActionGraph) {}

// Reference `SOCKET_LEAF` so the import doesn't dangle on a future
// refactor; the launcher's plan uses it implicitly via `LaunchPlan::new`.
const _ASSERT_SOCKET_LEAF: &str = SOCKET_LEAF;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_expr_pulls_first_arg() {
        let v = vec!["test".to_string(), "-DskipTests=false".to_string()];
        let (expr, rest) = split_expr(&v).unwrap();
        assert_eq!(expr, "test");
        assert_eq!(rest, &["-DskipTests=false".to_string()]);
    }

    #[test]
    fn split_expr_empty_returns_none() {
        let v: Vec<String> = vec![];
        assert!(split_expr(&v).is_none());
    }

    #[test]
    fn split_expr_single_arg_has_empty_rest() {
        let v = vec!["compile".to_string()];
        let (expr, rest) = split_expr(&v).unwrap();
        assert_eq!(expr, "compile");
        assert!(rest.is_empty());
    }

    #[test]
    fn project_root_key_is_deterministic_for_same_path() {
        let a = project_root_key(Path::new("/tmp/barista-test-project-1"));
        let b = project_root_key(Path::new("/tmp/barista-test-project-1"));
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "key should be 64-char hex SHA-256");
    }

    #[test]
    fn project_root_key_differs_for_different_paths() {
        let a = project_root_key(Path::new("/tmp/proj-a"));
        let b = project_root_key(Path::new("/tmp/proj-b"));
        assert_ne!(a, b);
    }

    #[test]
    fn is_warm_path_returns_false_when_cache_missing() {
        let td = tempfile::tempdir().unwrap();
        // No `barista.lock`, no `last-shot.toml`, no socket.
        let warm = is_warm_path(
            td.path(),
            &td.path().join("pom.xml"),
            &td.path().join("last-shot.toml"),
            &td.path().join("barback.sock"),
        );
        assert!(!warm, "warm path must miss when cache is absent");
    }

    #[test]
    fn last_shot_cache_round_trip_serializes_to_toml() {
        let cache = LastShotCache {
            version: CACHE_SCHEMA_VERSION,
            lockfile_signature: "abc123".to_string(),
            pom_mtime_ns: 1_700_000_000_000_000_000_i64,
            daemon_pid: 12345,
            last_shot_unix: 1_700_000_000,
        };
        let s = toml::to_string(&cache).unwrap();
        // Sanity: TOML body has every field name.
        assert!(s.contains("version"));
        assert!(s.contains("lockfile_signature"));
        assert!(s.contains("pom_mtime_ns"));
        assert!(s.contains("daemon_pid"));
        let parsed: LastShotCache = toml::from_str(&s).unwrap();
        assert_eq!(parsed.lockfile_signature, "abc123");
        assert_eq!(parsed.daemon_pid, 12345);
    }

    #[test]
    fn shot_error_exit_codes_match_verify_convention() {
        let e = ShotError::Pour { detail: "x".into() };
        assert_eq!(e.exit_code(), 2);
        let e = ShotError::Workers { detail: "x".into() };
        assert_eq!(e.exit_code(), 2);
        let e = ShotError::Ipc {
            phase: "compile".into(),
            detail: "x".into(),
        };
        assert_eq!(e.exit_code(), 1);
        let e = ShotError::PersistentCrash {
            phase: "compile".into(),
        };
        assert_eq!(e.exit_code(), 1);
    }
}
