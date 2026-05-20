// SPDX-License-Identifier: MIT OR Apache-2.0

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
use barista_ipc::{Credential, CredentialsEnvelope, credential};

use crate::action_graph::{ActionGraph, PlannedAction, build_action_request};
use crate::cli::{GlobalFlags, MavenVocabArgs, OutputFormat, PourArgs};
use crate::cmd::MavenPhase;
use crate::cmd::ci_repro::{ReproducibilitySeed, apply_to_request, build_seed};
use crate::cmd::pour::{PourError, PourReport, run_inner as pour_run};
use crate::cmd::reactor::{ModuleNode, Reactor, ReactorError};
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
///
/// Thin wrapper around [`run_phase`] pinning the phase to
/// [`MavenPhase::Verify`]. Retained as the cmd/verify entry point so
/// the CLI dispatch in [`crate::cli::dispatch`] doesn't have to know
/// that verify is a generic lifecycle command under the hood.
pub fn run(global: &GlobalFlags, args: &MavenVocabArgs) -> i32 {
    run_phase(global, MavenPhase::Verify, args)
}

/// Run an arbitrary Maven lifecycle phase end-to-end (`clean`,
/// `compile`, `test`, `package`, `verify`, `install`, `deploy`,
/// `site`). M4.3 T2 entry point.
///
/// Routes through the daemon by default; `--no-daemon` short-circuits
/// to a forked upstream `mvn <phase>` invocation per M4.2 T8.
pub fn run_phase(global: &GlobalFlags, phase: MavenPhase, args: &MavenVocabArgs) -> i32 {
    // `--no-daemon` fork: delegate to forked upstream `mvn`.
    if global.no_daemon {
        return crate::cmd::no_daemon::dispatch(global, phase, args);
    }

    let mut renderer = make_runtime_renderer(global);
    let exit = match dispatch_lifecycle(global, phase, args) {
        Ok(report) => {
            if !global.quiet
                && let Err(e) = renderer.render_verify(&report)
            {
                eprintln!("error: rendering {} report failed: {e}", phase.as_str());
                return 1;
            }
            if report.failed_actions > 0 {
                // Deploy auth failures get a distinct exit code so CI
                // pipelines can branch on "fix your creds" vs "fix
                // your code". Daemon-side dispatcher classifies the
                // failure (BAR-DEPLOY-AUTH-INVALID | -MISSING |
                // -ENCRYPTED) and the code propagates through the
                // ActionResult.error → MojoInvocation.error_code path.
                if report
                    .invocations
                    .iter()
                    .any(|i| i.error_code.starts_with("BAR-DEPLOY-AUTH-"))
                {
                    3
                } else {
                    1
                }
            } else {
                0
            }
        }
        Err(e) => {
            let code = e.exit_code();
            if matches!(global.output, OutputFormat::Human) {
                eprintln!("error: barista {} failed: {e}", phase.as_str());
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

/// Back-compat alias for the M4.3 T1 entry point. Calls
/// [`dispatch_lifecycle`] with [`MavenPhase::Verify`].
///
/// Kept so external callers (and the existing integration test
/// imports) compile without churn. Prefer [`dispatch_lifecycle`] in
/// new code so the phase is explicit.
pub fn run_inner(global: &GlobalFlags, args: &MavenVocabArgs) -> Result<VerifyReport, VerifyError> {
    dispatch_lifecycle(global, MavenPhase::Verify, args)
}

/// Library-friendly entry point. Returns a structured report on
/// success (including the failed-build case — `report.failed_actions
/// > 0` signals a Maven-side failure that ran to completion).
///
/// Hard errors (missing project, daemon spawn failure, IPC poison)
/// surface as [`VerifyError`].
///
/// The implementation walks the lifecycle phase prefix for `phase`
/// (see [`crate::action_graph::phase_prefix`]) and dispatches each
/// action through the daemon. For `Deploy`, the parsed `settings.xml`
/// (from the M1.3 T2 loader output) is converted into a
/// [`CredentialsEnvelope`] and attached to the action request so the
/// daemon-side dispatcher can write an ephemeral settings.xml for
/// the embedded Maven invocation.
pub fn dispatch_lifecycle(
    global: &GlobalFlags,
    phase: MavenPhase,
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
    // budget. `clean` skips this step because it doesn't need
    // dependencies; every other phase needs the compile classpath.
    //
    // Note: `pour` requires a `barista.lock` to exist. The CLI tells
    // the user to run `barista pull` first when missing — same
    // expectation the lifecycle commands inherit.
    if !matches!(phase, MavenPhase::Clean) {
        let pour_args = PourArgs {
            target: None,
            scope: crate::cli::ScopeArg::Compile,
            dry_run: false,
        };
        let _pour_report: PourReport =
            pour_run(global, &pour_args).map_err(VerifyError::from_pour)?;
    }

    // -- 4. Action graph --------------------------------------------------
    //
    // Build a Reactor (M4.3 T4). Single-module projects produce a
    // 1-module / 1-level reactor and dispatch the existing serial
    // loop unchanged. Multi-module projects activate the per-level
    // `tokio::join_all` dispatcher gated by the worker budget.
    let reactor = Reactor::from_project_root(&root.root, phase, /* include_clean: */ false)
        .map_err(VerifyError::from_reactor)?;
    // Back-compat: single-module path keeps using the local `graph`
    // binding so the existing dispatch loop below is untouched.
    let graph = reactor.modules[0].action_graph.clone();

    // -- 4.5 `--ci` reproducibility seed (M4.3 T6). --------------------
    // Computed once per command invocation so every action in the
    // graph sees identical determinism inputs (matters for the T4
    // reactor's per-level parallelism — every parallel action must
    // observe the same SOURCE_DATE_EPOCH, otherwise the property
    // becomes a function of scheduler ordering). `None` when `--ci`
    // is not active; the per-action loop below short-circuits the
    // apply step in that case.
    let ci_seed: Option<ReproducibilitySeed> = if global.ci {
        Some(build_seed(&root.root, |k| std::env::var(k).ok()))
    } else {
        None
    };

    // -- 5. Credentials envelope for deploy ------------------------------
    // Only `deploy` ships credentials. Other phases (including
    // `install`, which only writes to the local repo) MUST NOT — the
    // CredentialsEnvelope contract is "populated only for actions
    // that demonstrably need it".
    let deploy_credentials: Option<CredentialsEnvelope> = if matches!(phase, MavenPhase::Deploy) {
        match build_deploy_credentials(&config) {
            Ok(env) => Some(env),
            Err(e) => return Err(e),
        }
    } else {
        None
    };

    // -- 6. Daemon configuration -----------------------------------------
    let workers = resolve_workers_from_config(&config)?;
    let socket_dir = resolve_socket_dir(&config);
    let idle_shutdown_secs = config.daemon.idle_shutdown_secs;

    let plan = LaunchPlan::new(socket_dir, workers, idle_shutdown_secs);

    // -- 7. Discover / spawn daemon --------------------------------------
    let cwd = std::env::current_dir().unwrap_or_else(|_| root.root.clone());
    let jvm_entry = discover_jvm_entry(&cwd)?;
    let initial_handle = discover_or_spawn(&plan, || Ok(jvm_entry.clone()))?;

    // -- 8. Dispatch + collect --------------------------------------------
    //
    // Single-module fast path: walk the action graph sequentially,
    // reusing the existing serial dispatcher. The single-module case
    // is the M4.3 T1 happy path and the reactor wrapper is a no-op
    // overhead for it; bypassing the async `reactor::run` keeps the
    // pre-T4 byte-equality invariants intact.
    //
    // Multi-module path: dispatch through `reactor::run` with per-
    // level `tokio::join_all` parallelism. Per-module action streams
    // remain sequential (Maven lifecycle ordering inside a module is
    // load-bearing); the parallelism is at the module level.
    let total_planned: usize = reactor
        .modules
        .iter()
        .map(|m| m.action_graph.actions.len())
        .sum();
    let (invocations, executed, failed_actions, total_respawns, final_handle) =
        if reactor.is_single_module() {
            dispatch_single_module(
                &plan,
                &jvm_entry,
                initial_handle,
                &graph,
                &root.root,
                deploy_credentials.as_ref(),
                ci_seed.as_ref(),
            )?
        } else {
            dispatch_reactor_modules(
                &plan,
                &jvm_entry,
                initial_handle,
                &reactor,
                &root.root,
                deploy_credentials.as_ref(),
                workers,
                ci_seed.as_ref(),
            )?
        };
    let mut handle = final_handle;

    // Best-effort: if we spawned the child, leave it running for
    // subsequent invocations (the daemon's idle-shutdown timer will
    // reap it). We do NOT join — joining would block until the
    // daemon's idle-shutdown fires (30 min by default).
    if let Some(child) = handle.child.as_mut() {
        // Detach: we want the child to outlive this process.
        let _ = child;
    }

    let total_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(VerifyReport {
        project_root: root.root.clone(),
        phase: phase.as_str().to_string(),
        planned_actions: total_planned,
        executed_actions: executed,
        failed_actions,
        daemon_respawns: total_respawns,
        invocations,
        duration_ms: total_ms,
    })
}

/// Serial single-module dispatcher (pre-T4 behaviour).
///
/// Walks the action graph one action at a time; on Maven failure,
/// aborts the lifecycle (matching `mvn`'s own "stop on first error"
/// default).
#[allow(clippy::too_many_arguments)]
fn dispatch_single_module(
    plan: &LaunchPlan,
    jvm_entry: &crate::daemon::launcher::JvmEntry,
    initial_handle: crate::daemon::launcher::DaemonHandle,
    graph: &ActionGraph,
    project_root: &std::path::Path,
    deploy_credentials: Option<&CredentialsEnvelope>,
    ci_seed: Option<&ReproducibilitySeed>,
) -> Result<
    (
        Vec<MojoInvocation>,
        usize,
        usize,
        u32,
        crate::daemon::launcher::DaemonHandle,
    ),
    VerifyError,
> {
    let mut handle = initial_handle;
    let mut invocations = Vec::with_capacity(graph.actions.len());
    let mut failed_actions = 0usize;
    let mut total_respawns: u32 = 0;
    let mut executed = 0usize;
    for action in &graph.actions {
        if failed_actions > 0 {
            break;
        }
        let mut request = build_action_request(graph, action, project_root);
        if action.phase == "deploy"
            && let Some(env) = deploy_credentials
        {
            request.credentials = Some(env.clone());
        }
        // M4.3 T6: thread the `--ci` reproducibility seed onto every
        // action so all-phase byte-equality holds. No-op when --ci
        // wasn't set. Merge is non-clobbering: explicit per-action
        // overrides (none in v0.1 outside `deploy`'s `extra_mvn_args`
        // path) survive.
        if let Some(seed) = ci_seed {
            apply_to_request(&mut request, seed);
        }
        let phase_name = action.phase.to_string();
        let module = graph.module_root.clone();

        let action_started = Instant::now();
        let (outcome, next_handle) = submit_with_respawn(plan, handle, jvm_entry, request)
            .map_err(|e| classify_respawn_error(&phase_name, e))?;
        handle = next_handle;
        let duration_ms = u64::try_from(action_started.elapsed().as_millis()).unwrap_or(u64::MAX);

        executed += 1;
        total_respawns = total_respawns.saturating_add(outcome.respawns);

        let exit_code = outcome.result.exit_code;
        let status = action_status_str(outcome.result.status);
        if exit_code != 0 {
            failed_actions += 1;
        }
        let error_code = outcome
            .result
            .error
            .as_ref()
            .map(|e| e.code.clone())
            .unwrap_or_default();
        invocations.push(MojoInvocation {
            phase: phase_name,
            mojo: outcome.result.action_id.clone(),
            module,
            exit_code,
            status,
            failure_message: outcome.result.failure_message.clone(),
            error_code,
            duration_ms,
        });
    }
    Ok((
        invocations,
        executed,
        failed_actions,
        total_respawns,
        handle,
    ))
}

/// Multi-module reactor dispatcher (M4.3 T4).
///
/// Walks the reactor level-by-level. Within a level, modules dispatch
/// in parallel through `tokio::join_all`, capped at `workers_budget`
/// concurrent modules by a `tokio::sync::Semaphore`. Within a module,
/// the action stream stays sequential.
///
/// On the first module failure the reactor short-circuits subsequent
/// levels (Maven `--fail-fast` default) but lets every other module
/// in the same level run to completion — cancelling a module mid-
/// action would corrupt its `target/` state.
///
/// The daemon handle threading model is intentionally different from
/// the single-module path: each module's per-action loop runs against
/// a *fresh* `submit_with_respawn` chain rooted at the initial
/// handle's socket. Concurrent modules connect to the same daemon
/// socket through independent `Multiplexer` sessions (which the M4.2
/// T2 daemon's `WorkerPool` is designed to serve), so the handle is
/// not threaded across module boundaries. The reactor only needs the
/// final handle for the caller's "detach the spawned child" step.
#[allow(clippy::too_many_arguments)]
fn dispatch_reactor_modules(
    plan: &LaunchPlan,
    jvm_entry: &crate::daemon::launcher::JvmEntry,
    initial_handle: crate::daemon::launcher::DaemonHandle,
    reactor: &Reactor,
    project_root: &std::path::Path,
    deploy_credentials: Option<&CredentialsEnvelope>,
    workers_budget: usize,
    ci_seed: Option<&ReproducibilitySeed>,
) -> Result<
    (
        Vec<MojoInvocation>,
        usize,
        usize,
        u32,
        crate::daemon::launcher::DaemonHandle,
    ),
    VerifyError,
> {
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    // Build a multi-thread tokio runtime sized to the worker budget.
    // We need `rt-multi-thread` for true parallel module dispatch —
    // the current-thread runtime in `submit_with_respawn` is per-
    // module-action, not per-module.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers_budget.max(1))
        .enable_io()
        .enable_time()
        .build()
        .map_err(|e| VerifyError::Workers {
            detail: format!("tokio multi-thread runtime build: {e}"),
        })?;

    let budget = workers_budget.max(1);
    let semaphore = Arc::new(Semaphore::new(budget));
    let plan_arc = Arc::new(plan.clone());
    let jvm_arc = Arc::new(jvm_entry.clone());
    let project_root_arc: Arc<std::path::PathBuf> = Arc::new(project_root.to_path_buf());
    let creds_arc: Arc<Option<CredentialsEnvelope>> = Arc::new(deploy_credentials.cloned());
    let ci_seed_arc: Arc<Option<ReproducibilitySeed>> = Arc::new(ci_seed.cloned());

    let outcomes: Vec<Result<ModuleOutcome, VerifyError>> = runtime.block_on(async move {
        let mut all: Vec<Result<ModuleOutcome, VerifyError>> =
            Vec::with_capacity(reactor.modules.len());
        let mut module_results: std::collections::HashMap<
            usize,
            Result<ModuleOutcome, VerifyError>,
        > = std::collections::HashMap::new();
        let mut aborted = false;
        for (level_idx, level) in reactor.topo_levels.iter().enumerate() {
            if aborted {
                break;
            }
            let mut tasks: Vec<
                tokio::task::JoinHandle<(usize, Result<ModuleOutcome, VerifyError>)>,
            > = Vec::with_capacity(level.len());
            for &module_idx in level {
                let module = reactor.modules[module_idx].clone();
                let plan_arc = Arc::clone(&plan_arc);
                let jvm_arc = Arc::clone(&jvm_arc);
                let project_root_arc = Arc::clone(&project_root_arc);
                let creds_arc = Arc::clone(&creds_arc);
                let ci_seed_arc = Arc::clone(&ci_seed_arc);
                let semaphore = Arc::clone(&semaphore);
                tasks.push(tokio::spawn(async move {
                    let _permit = semaphore.acquire_owned().await;
                    tracing::info!(
                        target: "barista::reactor",
                        level = level_idx,
                        module = %module.id.to_ga(),
                        "dispatching module"
                    );
                    let module_root = module.root.clone();
                    let outcome = tokio::task::spawn_blocking(move || {
                        dispatch_one_module_blocking(
                            &plan_arc,
                            &jvm_arc,
                            &module,
                            &project_root_arc,
                            creds_arc.as_ref().as_ref(),
                            ci_seed_arc.as_ref().as_ref(),
                        )
                    })
                    .await
                    .map_err(|e| VerifyError::Ipc {
                        phase: "reactor".into(),
                        detail: format!("module task panicked at {module_root:?}: {e}"),
                    });
                    let outcome = match outcome {
                        Ok(r) => r,
                        Err(e) => Err(e),
                    };
                    (module_idx, outcome)
                }));
            }
            for handle in tasks {
                match handle.await {
                    Ok((idx, Ok(out))) => {
                        let failed = out.failed_actions > 0;
                        module_results.insert(idx, Ok(out));
                        if failed {
                            aborted = true;
                        }
                    }
                    Ok((idx, Err(e))) => {
                        module_results.insert(idx, Err(e));
                        aborted = true;
                    }
                    Err(_join_err) => {
                        aborted = true;
                    }
                }
            }
        }
        for idx in 0..reactor.modules.len() {
            if let Some(r) = module_results.remove(&idx) {
                all.push(r);
            }
        }
        all
    });

    // Drop the runtime — every spawned task is done.
    drop(runtime);

    // Aggregate. Module index order = `reactor.modules` order =
    // depth-first discovery order; this is the same order Maven's
    // own reactor reporter uses for its "Reactor Summary".
    let mut invocations: Vec<MojoInvocation> = Vec::new();
    let mut executed = 0usize;
    let mut failed_actions = 0usize;
    let mut total_respawns: u32 = 0;
    for o in outcomes {
        match o {
            Ok(out) => {
                executed += out.executed;
                failed_actions += out.failed_actions;
                total_respawns = total_respawns.saturating_add(out.respawns);
                invocations.extend(out.invocations);
            }
            Err(e) => return Err(e),
        }
    }
    Ok((
        invocations,
        executed,
        failed_actions,
        total_respawns,
        initial_handle,
    ))
}

/// Per-module outcome from the multi-module dispatcher.
#[derive(Debug)]
struct ModuleOutcome {
    invocations: Vec<MojoInvocation>,
    executed: usize,
    failed_actions: usize,
    respawns: u32,
}

/// Synchronously walk one module's action stream against the daemon.
///
/// Identical loop to the single-module path; carved out so the
/// reactor's `spawn_blocking` task can call it without duplicating
/// the action-stream + failure semantics.
fn dispatch_one_module_blocking(
    plan: &LaunchPlan,
    jvm_entry: &crate::daemon::launcher::JvmEntry,
    module: &ModuleNode,
    project_root: &std::path::Path,
    deploy_credentials: Option<&CredentialsEnvelope>,
    ci_seed: Option<&ReproducibilitySeed>,
) -> Result<ModuleOutcome, VerifyError> {
    // Each module connects fresh to the daemon socket. We don't
    // thread a `DaemonHandle` across modules because concurrent
    // modules each open their own `Multiplexer` session against the
    // same daemon — the daemon's worker pool muxes them safely.
    let initial_handle = crate::daemon::launcher::DaemonHandle {
        socket_path: plan.socket_path.clone(),
        child: None,
    };
    let mut handle = initial_handle;
    let graph = &module.action_graph;
    let mut invocations = Vec::with_capacity(graph.actions.len());
    let mut failed_actions = 0usize;
    let mut total_respawns: u32 = 0;
    let mut executed = 0usize;
    for action in &graph.actions {
        if failed_actions > 0 {
            break;
        }
        let mut request = build_action_request(graph, action, project_root);
        // Per-module working directory + pom path. The reactor sets
        // these on the action graph at construction time, so
        // `build_action_request` already uses the correct module
        // root — but we also override `project_root` here to keep
        // the daemon's resolver scoped per-module (the daemon-side
        // dispatcher resolves the effective POM relative to the
        // request's `pom_path`, not `project_root`).
        request.project_root = module.root.display().to_string();
        if action.phase == "deploy"
            && let Some(env) = deploy_credentials
        {
            request.credentials = Some(env.clone());
        }
        // M4.3 T6: thread the `--ci` reproducibility seed onto every
        // action (per-module reactor path; mirrors the
        // dispatch_single_module site).
        if let Some(seed) = ci_seed {
            apply_to_request(&mut request, seed);
        }
        let phase_name = action.phase.to_string();
        let action_started = Instant::now();
        let (outcome, next_handle) = submit_with_respawn(plan, handle, jvm_entry, request)
            .map_err(|e| classify_respawn_error(&phase_name, e))?;
        handle = next_handle;
        let duration_ms = u64::try_from(action_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        executed += 1;
        total_respawns = total_respawns.saturating_add(outcome.respawns);
        let exit_code = outcome.result.exit_code;
        let status = action_status_str(outcome.result.status);
        if exit_code != 0 {
            failed_actions += 1;
        }
        let error_code = outcome
            .result
            .error
            .as_ref()
            .map(|e| e.code.clone())
            .unwrap_or_default();
        invocations.push(MojoInvocation {
            phase: phase_name,
            mojo: outcome.result.action_id.clone(),
            module: module.root.clone(),
            exit_code,
            status,
            failure_message: outcome.result.failure_message.clone(),
            error_code,
            duration_ms,
        });
    }
    Ok(ModuleOutcome {
        invocations,
        executed,
        failed_actions,
        respawns: total_respawns,
    })
}

/// Convert the parsed `settings.xml` `<servers>` block (M1.3 T2
/// output) into a [`CredentialsEnvelope`] suitable for attaching to a
/// `deploy` action request.
///
/// Entries whose password is `{...}`-wrapped surface as
/// [`VerifyError::DeployAuthEncrypted`] — master-password decryption
/// is a documented follow-up (see `barista-config::decrypt_password`)
/// and we refuse to send ciphertext across the wire.
///
/// An empty `<servers>` block is NOT an error here: maven-deploy-plugin
/// can still succeed against an unauthenticated repository (e.g. a
/// local `file://` URL in `<distributionManagement>`). The daemon-side
/// dispatcher surfaces `BAR-DEPLOY-AUTH-MISSING` when the remote
/// actually rejects the un-credentialled request.
pub(crate) fn build_deploy_credentials(
    config: &Config,
) -> Result<CredentialsEnvelope, VerifyError> {
    let mut entries = Vec::with_capacity(config.maven_settings.servers.len());
    for server in &config.maven_settings.servers {
        if server.id.is_empty() {
            // Skip malformed entries silently — Maven itself ignores
            // <server> elements without an <id>.
            continue;
        }
        let mut cred = Credential {
            server_id: server.id.clone(),
            username: server.username.clone().unwrap_or_default(),
            secret: None,
        };
        if let Some(pw) = &server.password {
            let decrypted =
                barista_config::settings_xml::decrypt_password(pw, None).map_err(|_| {
                    VerifyError::DeployAuthEncrypted {
                        server_id: server.id.clone(),
                    }
                })?;
            cred.secret = Some(credential::Secret::Password(decrypted));
        }
        entries.push(cred);
    }
    Ok(CredentialsEnvelope { entries })
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

    /// Deploy attempted with a `{...}`-wrapped (master-password-
    /// encrypted) credential in settings.xml. The CLI refuses to send
    /// ciphertext across the wire (per the CredentialsEnvelope
    /// contract); decryption is a documented follow-up.
    #[error(
        "deploy: server '{server_id}' has a master-password-encrypted credential in settings.xml; \n  \
           code: BAR-DEPLOY-AUTH-ENCRYPTED\n  \
           hint: master-password decryption is a documented follow-up; \
                 use a plaintext credential or configure auth via environment \
                 variables until barista grows the decryption pipeline"
    )]
    DeployAuthEncrypted { server_id: String },

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

    /// Reactor construction failed (POM read/parse error, cycle, etc.).
    /// M4.3 T4: surfaced when the multi-module reactor can't be built
    /// before any action dispatches.
    #[error("reactor: {0}")]
    Reactor(#[from] ReactorError),
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

    /// Reactor errors (POM read/parse, cycle detection) come in via
    /// `Reactor::from_project_root`; wrapped here so the verify-side
    /// caller can `?` them through the same error shape.
    fn from_reactor(r: ReactorError) -> Self {
        VerifyError::Reactor(r)
    }

    /// Process exit code for the error. Mirrors the pour / pull
    /// convention: precondition / user-fixable errors → 2; internal /
    /// unexpected errors → 1; deploy-auth errors → 3 (a distinct
    /// "your credentials are wrong" sentinel CI pipelines can branch
    /// on without parsing the rendered message).
    fn exit_code(&self) -> i32 {
        match self {
            VerifyError::Project(_)
            | VerifyError::Config(_)
            | VerifyError::Workers { .. }
            | VerifyError::Pour { .. }
            | VerifyError::Reactor(_) => 2,
            VerifyError::DeployAuthEncrypted { .. } => 3,
            VerifyError::DaemonError { code, .. } if code.starts_with("BAR-DEPLOY-AUTH-") => 3,
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

    #[test]
    fn deploy_auth_errors_use_exit_code_3() {
        let e = VerifyError::DeployAuthEncrypted {
            server_id: "central".into(),
        };
        assert_eq!(e.exit_code(), 3, "encrypted-credential errors → exit 3");

        let e = VerifyError::DaemonError {
            phase: "deploy".into(),
            code: "BAR-DEPLOY-AUTH-INVALID".into(),
            message: "401".into(),
        };
        assert_eq!(
            e.exit_code(),
            3,
            "daemon-reported auth-invalid errors → exit 3"
        );

        let e = VerifyError::DaemonError {
            phase: "deploy".into(),
            code: "BAR-DEPLOY-AUTH-MISSING".into(),
            message: "no creds".into(),
        };
        assert_eq!(
            e.exit_code(),
            3,
            "daemon-reported missing-creds errors → exit 3"
        );

        // A non-auth daemon error stays at the generic exit 1.
        let e = VerifyError::DaemonError {
            phase: "compile".into(),
            code: "BAR-MAVEN-CORE".into(),
            message: "boom".into(),
        };
        assert_eq!(e.exit_code(), 1);
    }

    #[test]
    fn build_deploy_credentials_skips_blank_server_ids() {
        let mut cfg = Config::default();
        cfg.maven_settings.servers.push(barista_config::Server {
            id: String::new(),
            username: Some("anon".into()),
            password: Some("p".into()),
            ..Default::default()
        });
        cfg.maven_settings.servers.push(barista_config::Server {
            id: "real-repo".into(),
            username: Some("u".into()),
            password: Some("p".into()),
            ..Default::default()
        });
        let env = build_deploy_credentials(&cfg).unwrap();
        assert_eq!(env.entries.len(), 1, "blank-id server entries are skipped");
        assert_eq!(env.entries[0].server_id, "real-repo");
    }

    #[test]
    fn build_deploy_credentials_surfaces_encrypted_passwords() {
        let mut cfg = Config::default();
        cfg.maven_settings.servers.push(barista_config::Server {
            id: "encrypted-repo".into(),
            username: Some("u".into()),
            // `{...}`-wrapped → recognised as encrypted by
            // barista-config::decrypt_password.
            password: Some("{COQLCE53YjsoAtFt3PNZuyP+sb9D9Mr7Hp0/mAtNNNk=}".into()),
            ..Default::default()
        });
        let err = build_deploy_credentials(&cfg).unwrap_err();
        match err {
            VerifyError::DeployAuthEncrypted { server_id } => {
                assert_eq!(server_id, "encrypted-repo");
            }
            other => panic!("expected DeployAuthEncrypted, got {other:?}"),
        }
    }

    /// M4.3 T6 — direct unit test of the `--ci` apply-to-request seam
    /// the dispatcher uses. We don't drive `dispatch_lifecycle` (it
    /// needs a daemon); instead we build the same seed + apply combo
    /// the dispatcher does, and assert the per-action `ActionRequest`
    /// surface matches the wire-contract documented in `ci_repro`.
    ///
    /// Note: avoids mutating the process-wide `BARISTA_SOURCE_DATE_EPOCH`
    /// env var (other tests in this crate also set it, and `cargo test`
    /// runs library tests on a shared thread pool). Instead we pass a
    /// hand-rolled lookup closure into `build_seed`.
    #[test]
    fn ci_macro_populates_reproducibility_env_on_action_request() {
        use crate::action_graph::{build_action_request, lifecycle_graph};
        use crate::cmd::ci_repro::{apply_to_request, build_seed};
        let root = PathBuf::from("/tmp/proj");
        let graph = lifecycle_graph(MavenPhase::Verify, root.clone(), false);
        // Hermetic env lookup: pin BARISTA_SOURCE_DATE_EPOCH to a
        // value inside Maven's outputTimestamp validity range.
        let seed = build_seed(&root, |k| {
            if k == "BARISTA_SOURCE_DATE_EPOCH" {
                Some("1700000000".to_string())
            } else {
                None
            }
        });
        let mut req = build_action_request(&graph, &graph.actions[0], &root);
        apply_to_request(&mut req, &seed);

        // SOURCE_DATE_EPOCH + TZ + LC_ALL must land on environment.
        assert_eq!(
            req.environment.get("SOURCE_DATE_EPOCH").map(String::as_str),
            Some("1700000000"),
        );
        assert_eq!(req.environment.get("TZ").map(String::as_str), Some("UTC"));
        assert_eq!(req.environment.get("LC_ALL").map(String::as_str), Some("C"));
        // project.build.outputTimestamp must land on system_properties.
        assert_eq!(
            req.system_properties
                .get("project.build.outputTimestamp")
                .map(String::as_str),
            Some("2023-11-14T22:13:20Z"),
        );
        // maven_compat pinned to "4" via build_action_request default;
        // apply_to_request preserves it.
        assert_eq!(req.maven_compat, "4");
    }

    #[test]
    fn build_deploy_credentials_passes_plaintext_through() {
        let mut cfg = Config::default();
        cfg.maven_settings.servers.push(barista_config::Server {
            id: "plain".into(),
            username: Some("u".into()),
            password: Some("hunter2".into()),
            ..Default::default()
        });
        let env = build_deploy_credentials(&cfg).unwrap();
        assert_eq!(env.entries.len(), 1);
        match &env.entries[0].secret {
            Some(credential::Secret::Password(p)) => assert_eq!(p, "hunter2"),
            other => panic!("expected Password secret, got {other:?}"),
        }
    }
}
