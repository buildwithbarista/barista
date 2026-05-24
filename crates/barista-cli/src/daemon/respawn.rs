// SPDX-License-Identifier: MIT OR Apache-2.0

//! Auto-respawn driver on top of [`barista_ipc::Multiplexer`].
//!
//! Wraps a single `submit_action` + collect-result round-trip with the
//! M4.2 T6 failure-model contract:
//!
//!   1. Submit the action through a fresh `Multiplexer` connected to
//!      the daemon at `socket_path`.
//!   2. Drain the action handle's `StreamEvent`s, collecting any
//!      `Progress` / `Stream` events the caller wants and waiting for
//!      the terminal `Result` or `Error`.
//!   3. If the terminal event is a `StreamEvent::Error` whose code is
//!      `BAR-DAEMON-CRASHED` (the wire contract M4.2 T6 minted), and
//!      the action has not yet been retried, **respawn** the daemon
//!      (kill leftover PID, spawn afresh, wait-for-ready) and resubmit
//!      the same `ActionRequest`. Retry budget = 1.
//!   4. After the retry budget is exhausted, surface the most-recent
//!      outcome — either a real `ActionResult` (success or non-crash
//!      failure) or the persistent crash error.
//!
//! The retry budget is deliberately small. PRD §11.9 frames daemon
//! crashes as recoverable transients (OOM under load, JIT pathology,
//! plugin bug); a second crash within one user invocation indicates a
//! persistent failure mode the user has to fix, so we surface it
//! rather than loop. Tests in `tests/cmd_verify.rs` pin both halves of
//! the contract: one-shot crash → success on retry, persistent crash →
//! `BAR-DAEMON-CRASHED` surfaces to the caller.
//!
//! # Idempotency assumption
//!
//! Auto-respawn assumes the action is idempotent. The v0.1 lifecycle
//! mojos (`compile`, `test-compile`, `test`, `package`, `verify`) all
//! either (a) produce deterministic outputs from the same inputs (so
//! re-running them just overwrites the same files) or (b) are
//! side-effect-free read-only checks. The retry is therefore safe.
//!
//! `install`/`deploy` mojos are NOT idempotent in the same way (deploy
//! has remote side-effects); M4.3 T2 wires those through a
//! `retry_policy: NoRetry` knob on the action graph. M4.3 T1 only
//! handles `verify` and downstream phases that are idempotent.

use std::path::PathBuf;
use std::time::Duration;

use barista_ipc::{
    ActionRequest, ActionResult, Multiplexer, ProgressEvent, StreamEvent, mux::DAEMON_CRASHED_CODE,
};

use super::launcher::{DaemonHandle, JvmEntry, LaunchPlan, LauncherError, wait_for_ready};

/// Errors surfaced from [`submit_with_respawn`].
#[derive(Debug, thiserror::Error)]
pub enum RespawnError {
    /// The daemon couldn't be discovered or spawned. Wraps a
    /// [`LauncherError`] so the user-facing message names the
    /// specific failure (jar-not-found, java-not-found, spawn timeout).
    #[error(transparent)]
    Launcher(#[from] LauncherError),

    /// The IPC transport / multiplex layer raised a typed error that
    /// isn't the `BAR-DAEMON-CRASHED` retryable kind. Connection is
    /// poisoned; the action is not retried.
    #[error("ipc error: {detail}")]
    Ipc {
        /// Stringified `MuxError` / `TransportError`; we don't expose
        /// the typed error since the underlying types don't derive
        /// `Clone` (we may need the value across retries).
        detail: String,
    },

    /// Connecting to the daemon's UDS failed.
    #[error("connect to daemon at {socket:?}: {source}")]
    Connect {
        /// Path we tried to connect to.
        socket: PathBuf,
        /// Underlying `io::Error`.
        #[source]
        source: std::io::Error,
    },

    /// The daemon crashed mid-action, the retry budget was exhausted
    /// (auto-respawn attempted once), and the next action also
    /// crashed. Surfaces the canonical `BAR-DAEMON-CRASHED` wire code
    /// so callers can branch on it.
    #[error(
        "{DAEMON_CRASHED_CODE}: barback crashed mid-action and the auto-respawn retry \
         also crashed. The daemon may be in a persistent failure mode \
         (OOM, plugin bug, filesystem issue); inspect daemon logs via \
         `BARISTA_BARBACK_VERBOSE=1 barista verify`."
    )]
    PersistentCrash,

    /// The daemon's terminal `StreamEvent` arrived but was an
    /// `Error` with a non-`BAR-DAEMON-CRASHED` code. Not retried;
    /// surfaces the error code + message to the caller.
    #[error("daemon error: {code}: {message}")]
    DaemonProtocolError {
        /// Wire error code from the daemon.
        code: String,
        /// Human-readable summary.
        message: String,
    },

    /// The per-action channel closed without a terminal event. The
    /// connection has been torn down between submission and result;
    /// reported as a one-time failure (not retried — the partial
    /// state is opaque to us).
    #[error("daemon connection closed before action terminated")]
    PrematureClose,
}

/// Outcome of a successful [`submit_with_respawn`] call.
#[derive(Debug, Clone)]
pub struct RespawnOutcome {
    /// The terminal `ActionResult` (success or non-crash failure).
    pub result: ActionResult,
    /// Number of times the daemon was respawned during this call.
    /// `0` on the happy path; `1` after a successful auto-respawn.
    pub respawns: u32,
    /// Progress events observed during the (final) successful submit,
    /// for callers that want to surface mojo-level NDJSON output.
    pub progress: Vec<ProgressEvent>,
}

/// Submit `request` to the daemon, transparently respawning + retrying
/// once on `BAR-DAEMON-CRASHED`.
///
/// `handle` is the initial daemon handle (from [`discover_or_spawn`]).
/// On retry, the launcher's plan + jvm-entry are reused to spawn a
/// fresh daemon at the same socket.
///
/// The function drives the IPC interaction inside a small tokio
/// current-thread runtime, so the caller stays synchronous. The
/// runtime is dropped at the end of the call — the next action
/// builds its own.
pub fn submit_with_respawn(
    plan: &LaunchPlan,
    handle: DaemonHandle,
    jvm_entry: &JvmEntry,
    request: ActionRequest,
) -> Result<(RespawnOutcome, DaemonHandle), RespawnError> {
    submit_with_respawn_inner(plan, handle, jvm_entry, request, /* attempt */ 0)
}

fn submit_with_respawn_inner(
    plan: &LaunchPlan,
    mut handle: DaemonHandle,
    jvm_entry: &JvmEntry,
    request: ActionRequest,
    attempt: u32,
) -> Result<(RespawnOutcome, DaemonHandle), RespawnError> {
    // Build a fresh tokio runtime per attempt. Multiplexer's reader +
    // writer tasks are spawned on this runtime; tearing it down on
    // crash detection is the simplest way to drop the poisoned mux +
    // its background tasks without leaking state.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|e| RespawnError::Ipc {
            detail: format!("tokio runtime build: {e}"),
        })?;

    let attempt_result =
        runtime.block_on(async { run_one_attempt(&handle.socket_path, request.clone()).await });

    match attempt_result {
        Ok(outcome) => Ok((
            RespawnOutcome {
                result: outcome.result,
                respawns: attempt,
                progress: outcome.progress,
            },
            handle,
        )),
        Err(AttemptError::Crashed) if attempt == 0 => {
            // Budget allows one auto-respawn. Drop the poisoned runtime
            // first (in the `match` scope's drop point) so the child
            // process resources are released before we reap the
            // previous daemon.
            drop(runtime);

            // Reap the existing child (if any) — it crashed via
            // `Runtime.halt`, but we still need to wait() it to avoid
            // a zombie. The launcher's spawn writes a fresh PID file
            // for the new daemon; the old PID is recorded there so
            // the launcher will kill it on the next discover, but we
            // do it here too for belt-and-suspenders.
            if let Some(mut child) = handle.child.take() {
                let _ = child.wait();
            }

            // Spawn a fresh daemon at the same plan. We don't go
            // through `discover_or_spawn` here because the previous
            // socket inode is the *crashed daemon's* inode — calling
            // `socket_is_live` against it would return true if the
            // kernel hasn't reaped the listener yet, masking the
            // crash. Instead, unlink the inode and spawn directly.
            let _ = std::fs::remove_file(&plan.socket_path);
            let (mut child, stderr_tail) = super::launcher::spawn_daemon(plan, jvm_entry)?;
            wait_for_ready(
                &plan.socket_path,
                plan.spawn_timeout,
                &mut child,
                &stderr_tail,
            )?;
            let new_handle = DaemonHandle {
                socket_path: plan.socket_path.clone(),
                child: Some(child),
            };

            // Recursion-via-helper: exactly one retry, then surface.
            submit_with_respawn_inner(plan, new_handle, jvm_entry, request, attempt + 1)
        }
        Err(AttemptError::Crashed) => {
            // Budget exhausted; persistent crash.
            Err(RespawnError::PersistentCrash)
        }
        Err(AttemptError::Connect { source }) => Err(RespawnError::Connect {
            socket: handle.socket_path.clone(),
            source,
        }),
        Err(AttemptError::Ipc { detail }) => Err(RespawnError::Ipc { detail }),
        Err(AttemptError::DaemonProtocolError { code, message }) => {
            Err(RespawnError::DaemonProtocolError { code, message })
        }
        Err(AttemptError::PrematureClose) => Err(RespawnError::PrematureClose),
    }
}

#[derive(Debug)]
enum AttemptError {
    /// `BAR-DAEMON-CRASHED` surfaced — the retry path applies.
    Crashed,
    /// `UnixStream::connect` failed.
    Connect { source: std::io::Error },
    /// Other IPC failure (transport poisoned, mux shutdown, etc.).
    Ipc { detail: String },
    /// Daemon-side typed protocol error.
    DaemonProtocolError { code: String, message: String },
    /// Per-action channel closed without a terminal event.
    PrematureClose,
}

#[derive(Debug)]
struct AttemptOutcome {
    result: ActionResult,
    progress: Vec<ProgressEvent>,
}

/// One submit/collect attempt. Returns `Err(AttemptError::Crashed)`
/// when the daemon disappears mid-action; other variants are terminal
/// for the call.
async fn run_one_attempt(
    socket_path: &std::path::Path,
    request: ActionRequest,
) -> Result<AttemptOutcome, AttemptError> {
    use tokio::net::UnixStream;

    let stream = match UnixStream::connect(socket_path).await {
        Ok(s) => s,
        Err(e) => return Err(AttemptError::Connect { source: e }),
    };

    let transport = barista_ipc::transport::uds::UdsTransport::from_stream(stream);
    let (_mux, client, _server) = Multiplexer::spawn(transport);

    let mut handle = match client.submit_action(request).await {
        Ok(h) => h,
        Err(e) => {
            return Err(AttemptError::Ipc {
                detail: format!("submit_action: {e}"),
            });
        }
    };

    let mut progress = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);

    loop {
        let evt = tokio::select! {
            biased;
            r = handle.next_event() => r,
            _ = tokio::time::sleep_until(deadline) => {
                return Err(AttemptError::Ipc {
                    detail: "action exceeded 600s deadline".to_string(),
                });
            }
        };
        let evt = match evt {
            Ok(Some(e)) => e,
            Ok(None) => return Err(AttemptError::PrematureClose),
            Err(e) => {
                return Err(AttemptError::Ipc {
                    detail: format!("next_event: {e}"),
                });
            }
        };
        match evt {
            StreamEvent::Progress(p) => progress.push(p),
            StreamEvent::Stream(_) => {
                // v0.1 verify doesn't yet forward chunked stdout/
                // stderr to the user-facing renderer. The IPC layer
                // discards the chunks; M3.2 T3 / T4 wires the
                // forwarding in a subsequent batch.
            }
            StreamEvent::Result(result) => {
                return Ok(AttemptOutcome { result, progress });
            }
            StreamEvent::Error(err) => {
                if err.code == DAEMON_CRASHED_CODE {
                    return Err(AttemptError::Crashed);
                }
                return Err(AttemptError::DaemonProtocolError {
                    code: err.code,
                    message: err.message,
                });
            }
        }
    }
}
