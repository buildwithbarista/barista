//! `barista verify` — the headline end-to-end command (M4.3 T1).
//!
//! Wires the four major v0.1 subsystems together:
//!
//! 1. **Resolve** — walk up to the project root, load layered config
//!    (M1.3). Lockfile presence is required (the resolver itself runs
//!    via `barista pull`; verify assumes lockfile + CAS are ready).
//! 2. **Pour** — materialize the locked artifacts into the configured
//!    Maven local repository (typically `~/.m2/repository`) so the
//!    daemon's embedded Maven core can find them on its classpath.
//!    Idempotent; re-running is cheap.
//! 3. **Action-graph** — build the `verify` lifecycle phase prefix
//!    (see [`crate::action_graph`]) as a sequential list of one
//!    `ActionRequest` per phase.
//! 4. **Dispatch + collect** — for each action, spawn-or-discover the
//!    daemon and run the action through the auto-respawn driver. The
//!    M4.2 T6 `BAR-DAEMON-CRASHED` retryable-error contract is
//!    honoured: a single crash mid-action triggers respawn + retry
//!    once; a second crash surfaces as a persistent failure.
//!
//! The render goes through the existing M3.2 [`crate::output`]
//! renderer chain via a new `VerifyReport`.
//!
//! # Scope (T1 only)
//!
//! * Single-module projects only. Reactor topo-sort + per-level
//!   parallelism is M4.3 T4.
//! * Phase = `verify` only. Other lifecycle phases (`clean`,
//!   `compile`, `test`, `package`, `install`, `deploy`, `site`) are
//!   M4.3 T2 — they reuse this command's plumbing through a
//!   parameterised entry point.
//! * No `barista shot` warm-path optimisation. That's M4.3 T3.
//! * No `--ci` reproducibility plumbing beyond what M3.2 T4 already
//!   wired (the `dispatch` shim flips `--frozen --output json
//!   --quiet`). M4.3 T6 owns the rest of the reproducibility story.
//!
//! # `--no-daemon` fork
//!
//! `barista verify --no-daemon` short-circuits to
//! [`crate::cmd::no_daemon::dispatch`] which forks an upstream `mvn
//! verify` against the same project. This is the explicit R2
//! mitigation per M4.2 T8. The fork delegates the entire build to
//! upstream Maven; the byte-equality acceptance criterion is
//! satisfied by the existing
//! `cmd_no_daemon::byte_equal_compile_against_real_mvn` test pattern,
//! generalised to `verify` in the integration test.

use std::path::PathBuf;
use std::time::Instant;

use barista_config::{Config, LoadAudit, LoaderError, LoaderInputs, load_effective_config};

use crate::action_graph::{ActionGraph, PlannedAction, build_action_request, verify_graph};
use crate::cli::{GlobalFlags, MavenVocabArgs, OutputFormat, PourArgs};
use crate::cmd::MavenPhase;
use crate::cmd::pour::{PourError, PourReport, run_inner as pour_run};
use crate::daemon::launcher::{LaunchPlan, LauncherError};
use crate::daemon::respawn::{RespawnError, submit_with_respawn};
use crate::daemon::{
    available_parallelism_or_one, discover_jvm_entry, discover_or_spawn, resolve_workers,
};
use crate::output::{MojoInvocation, VerifyReport, make_runtime_renderer};
use crate::project::{ResolveError, ResolveInputs, resolve_project_root};

/// `--workers` expression used when `barback.default_workers` isn't
/// pinned in config. PRD §11.2.2 — "one per core" is the default.
const DEFAULT_WORKERS_EXPR: &str = "1C";

/// Daemon socket directory leaf under `$HOME`. Mirrors barback's
/// `~/.barista/run/barback.sock` default (see `Server.java`'s
/// `defaultPath`). Kept consistent with the daemon side so a CLI
/// built against this code talks to a daemon built against
/// `Server.java`'s default without configuration.
const DEFAULT_RUN_DIR_LEAF: &str = ".barista/run";

/// Run `barista verify`. Returns the process exit code.
pub fn run(global: &GlobalFlags, args: &MavenVocabArgs) -> i32 {
    // `--no-daemon` fork: delegate to forked upstream `mvn verify`.
    if global.no_daemon {
        return crate::cmd::no_daemon::dispatch(global, MavenPhase::Verify, args);
    }

    let mut renderer = make_runtime_renderer(global);
    let exit = match run_inner(global, args) {
        Ok(report) => {
            if !global.quiet
                && let Err(e) = renderer.render_verify(&report)
            {
                eprintln!("error: rendering verify report failed: {e}");
                return 1;
            }
            if report.failed_actions > 0 { 1 } else { 0 }
        }
        Err(e) => {
            let code = e.exit_code();
            if matches!(global.output, OutputFormat::Human) {
                eprintln!("error: barista verify failed: {e}");
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

/// Library-friendly entry point. Returns a structured report on
/// success (including the failed-build case — `report.failed_actions
/// > 0` signals a Maven-side failure that ran to completion).
///
/// Hard errors (missing project, daemon spawn failure, IPC poison)
/// surface as [`VerifyError`].
pub fn run_inner(
    global: &GlobalFlags,
    _args: &MavenVocabArgs,
) -> Result<VerifyReport, VerifyError> {
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

    // -- 3. Pour ----------------------------------------------------------
    // `pour` is idempotent and cheap on a no-op run — re-materialising
    // the same locked artifacts into ~/.m2 takes a small wall-clock
    // budget. If the user already ran `barista pull` + `barista pour`
    // separately, this step is a no-op modulo the directory stat.
    //
    // Note: `pour` requires a `barista.lock` to exist. The CLI tells
    // the user to run `barista pull` first when missing — same
    // expectation `barista verify` inherits today.
    let pour_args = PourArgs {
        target: None,
        scope: crate::cli::ScopeArg::Compile,
        dry_run: false,
    };
    // If `pour` fails because the project never ran `barista pull`,
    // we surface a verify-specific hint that names both prerequisites.
    let _pour_report: PourReport = pour_run(global, &pour_args).map_err(VerifyError::from_pour)?;

    // -- 4. Action graph --------------------------------------------------
    let graph = verify_graph(root.root.clone(), /* include_clean: */ false);

    // -- 5. Daemon configuration -----------------------------------------
    let workers = resolve_workers_from_config(&config)?;
    let socket_dir = resolve_socket_dir(&config);
    let idle_shutdown_secs = config.daemon.idle_shutdown_secs;

    let plan = LaunchPlan::new(socket_dir, workers, idle_shutdown_secs);

    // -- 6. Discover / spawn daemon --------------------------------------
    let cwd = std::env::current_dir().unwrap_or_else(|_| root.root.clone());
    let jvm_entry = discover_jvm_entry(&cwd)?;
    let initial_handle = discover_or_spawn(&plan, || Ok(jvm_entry.clone()))?;

    // -- 7. Dispatch + collect --------------------------------------------
    let mut handle = initial_handle;
    let mut invocations = Vec::with_capacity(graph.actions.len());
    let mut failed_actions = 0usize;
    let mut total_respawns: u32 = 0;
    let mut executed = 0usize;
    for action in &graph.actions {
        if failed_actions > 0 {
            // Maven aborts the lifecycle on the first failure; we
            // honour the same semantics. The unexecuted invocations
            // remain absent from the report so JSON consumers can see
            // "we stopped here".
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
            duration_ms,
        });
    }

    // Best-effort: if we spawned the child, leave it running for
    // subsequent invocations (the daemon's idle-shutdown timer will
    // reap it). We do NOT join — joining would block until the
    // daemon's idle-shutdown fires (30 min by default).
    if let Some(child) = handle.child.as_mut() {
        // Detach: we want the child to outlive this process. The
        // `Child` destructor on Drop does not `kill` or `wait` —
        // letting it drop here is the right behaviour.
        let _ = child;
    }

    let total_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(VerifyReport {
        project_root: root.root.clone(),
        phase: "verify".to_string(),
        planned_actions: graph.actions.len(),
        executed_actions: executed,
        failed_actions,
        daemon_respawns: total_respawns,
        invocations,
        duration_ms: total_ms,
    })
}

/// Resolve the per-call worker count.
///
/// The `barback.default_workers` expression isn't part of the
/// `DaemonConfig` schema yet (it'll land alongside the M4.3 batch's
/// schema extension); for v0.1 we honour an environment-variable
/// override (`BARISTA_DAEMON_WORKERS`) for testability, then fall
/// back to `"1C"`.
fn resolve_workers_from_config(_cfg: &Config) -> Result<usize, VerifyError> {
    let expr = std::env::var("BARISTA_DAEMON_WORKERS")
        .ok()
        .unwrap_or_else(|| DEFAULT_WORKERS_EXPR.to_string());
    let cores = available_parallelism_or_one();
    resolve_workers(&expr, cores).map_err(|e| VerifyError::Workers {
        detail: e.to_string(),
    })
}

/// Resolve the socket directory the daemon will use.
fn resolve_socket_dir(cfg: &Config) -> PathBuf {
    // The schema default is `"~/.barista/run"` (literally — the
    // leading `~` is a stringly-typed placeholder in the schema
    // default), so expand if we see it.
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

fn classify_respawn_error(phase: &str, e: RespawnError) -> VerifyError {
    match e {
        RespawnError::Launcher(le) => VerifyError::Launcher(le),
        RespawnError::PersistentCrash => VerifyError::PersistentCrash {
            phase: phase.to_string(),
        },
        RespawnError::Connect { socket, source } => VerifyError::DaemonConnect {
            phase: phase.to_string(),
            socket,
            detail: source.to_string(),
        },
        RespawnError::Ipc { detail } => VerifyError::Ipc {
            phase: phase.to_string(),
            detail,
        },
        RespawnError::DaemonProtocolError { code, message } => VerifyError::DaemonError {
            phase: phase.to_string(),
            code,
            message,
        },
        RespawnError::PrematureClose => VerifyError::PrematureClose {
            phase: phase.to_string(),
        },
    }
}

/// Errors surfaced from `barista verify`.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// Project resolution failed.
    #[error("project setup: {0}")]
    Project(#[from] ResolveError),

    /// Config load failed.
    #[error("config load: {0}")]
    Config(#[from] LoaderError),

    /// Worker-count expression couldn't be resolved.
    #[error("workers config: {detail}")]
    Workers { detail: String },

    /// Daemon launcher failure (jar-not-found, java-not-found,
    /// spawn-timeout, etc.).
    #[error(transparent)]
    Launcher(#[from] LauncherError),

    /// Pour pre-step failed. The hint message names both
    /// prerequisites (`barista pull` then `barista pour`) so the
    /// user can recover even if they don't know which step failed.
    #[error(
        "pour step (required before verify): {detail}\n  hint: run `barista pull` first to resolve dependencies, then re-run `barista verify`"
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
         inspect daemon logs via `BARISTA_BARBACK_VERBOSE=1 barista verify`.",
        BAR_DAEMON_CRASHED = barista_ipc::mux::DAEMON_CRASHED_CODE,
    )]
    PersistentCrash { phase: String },

    /// Per-action channel closed without a terminal event.
    #[error("phase {phase}: daemon disconnected before action terminated")]
    PrematureClose { phase: String },
}

impl VerifyError {
    /// Pour errors come in via `?` on the inner `pour::run_inner`;
    /// this wraps them with a `Pour { detail }` so the message stays
    /// verify-shaped.
    fn from_pour(p: PourError) -> Self {
        VerifyError::Pour {
            detail: p.to_string(),
        }
    }

    /// Process exit code for the error. Mirrors the pour / pull
    /// convention: precondition / user-fixable errors → 2; internal /
    /// unexpected errors → 1.
    fn exit_code(&self) -> i32 {
        match self {
            VerifyError::Project(_)
            | VerifyError::Config(_)
            | VerifyError::Workers { .. }
            | VerifyError::Pour { .. } => 2,
            VerifyError::Launcher(_)
            | VerifyError::DaemonConnect { .. }
            | VerifyError::Ipc { .. }
            | VerifyError::DaemonError { .. }
            | VerifyError::PersistentCrash { .. }
            | VerifyError::PrematureClose { .. } => 1,
        }
    }
}

// Suppress unused warnings on platforms where the `verify.rs` module
// isn't compiled. (The `#[cfg(unix)]` gate at the `cmd/mod.rs` level
// already keeps us out of Windows builds, but the import-set above
// references types that are themselves cfg-gated.)
#[allow(dead_code)]
fn _planned_action_unused(_a: PlannedAction, _g: ActionGraph) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_status_str_known_variants() {
        assert_eq!(action_status_str(0), "unknown");
        assert_eq!(action_status_str(1), "success");
        assert_eq!(action_status_str(2), "failure");
        assert_eq!(action_status_str(3), "timeout");
        assert_eq!(action_status_str(4), "crashed");
        assert_eq!(action_status_str(5), "cancelled");
    }

    #[test]
    fn action_status_str_unknown_defaults_to_unknown() {
        assert_eq!(action_status_str(99), "unknown");
        assert_eq!(action_status_str(-1), "unknown");
    }

    // Test mutates process-wide env vars; per-fn allow keeps the
    // workspace-wide `unsafe_code = warn` lint clean while letting
    // the test exercise the Rust 2024 `set_var` unsafety contract.
    #[allow(unsafe_code)]
    #[test]
    fn resolve_socket_dir_expands_tilde() {
        let prev = std::env::var_os("HOME");
        // SAFETY: per-test env mutation. Restored after the assertion.
        unsafe {
            std::env::set_var("HOME", "/var/test-home");
        }
        let cfg = Config::default();
        let p = resolve_socket_dir(&cfg);
        assert_eq!(p, PathBuf::from("/var/test-home/.barista/run"));
        // SAFETY: restore.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn verify_error_exit_codes_match_pour_convention() {
        let e = VerifyError::Pour { detail: "x".into() };
        assert_eq!(e.exit_code(), 2);
        let e = VerifyError::Workers { detail: "x".into() };
        assert_eq!(e.exit_code(), 2);
        let e = VerifyError::Ipc {
            phase: "compile".into(),
            detail: "x".into(),
        };
        assert_eq!(e.exit_code(), 1);
        let e = VerifyError::PersistentCrash {
            phase: "compile".into(),
        };
        assert_eq!(e.exit_code(), 1);
    }
}
