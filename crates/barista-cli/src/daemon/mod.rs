//! Daemon-lifecycle plumbing for the CLI.
//!
//! `barista verify` (M4.3 T1) and every other Maven-vocabulary
//! lifecycle command that goes through the warm-JVM `barback` daemon
//! shares the same dispatch contract:
//!
//! 1. Resolve daemon configuration (worker count, idle window, socket
//!    path) from `barista-config`.
//! 2. **Discover** an existing daemon by trying to connect to its
//!    socket; **spawn** a fresh one when none is reachable. See
//!    [`launcher`].
//! 3. Dispatch actions through `barista-ipc`'s `Multiplexer`. If an
//!    action terminates with the `BAR-DAEMON-CRASHED` wire error from
//!    M4.2 T6, kill any leftover PID, respawn the daemon, and retry
//!    the action **exactly once**. See [`respawn`].
//!
//! The auto-respawn retry budget is intentionally bounded to one: a
//! second consecutive crash is treated as a persistent failure mode
//! the user has to investigate (out-of-memory, plugin bug,
//! filesystem corruption). Surfacing the second crash to the user
//! preserves the contract that `barista verify` either makes
//! progress or fails clearly — it never loops.
//!
//! # Module structure
//!
//! * [`launcher`] — discover / spawn / wait-for-ready.
//! * [`respawn`] — the action-level "submit → on crash, respawn +
//!   retry once" wrapper that M4.3 T1 wires around every
//!   `MuxClient::submit_action` call.
//! * [`workers`] — `barback.default_workers` expression resolver
//!   (`"1C"` / `"0.75C"` / literal int). Lives here because the
//!   launcher's `--workers` argument resolution is the only consumer.
//!
//! # Platform support
//!
//! v0.1 is Unix-only (the daemon's named-pipe bind on Windows is
//! deferred — see `Server.java`'s class javadoc). The `#[cfg(unix)]`
//! gates below keep the launcher and respawn-driver out of Windows
//! builds; the `workers` module is cross-platform because the
//! expression resolver has no OS coupling.

pub mod workers;

/// Maven-home resolution for the barback spawn (bundled / env / override).
/// Cross-platform: the bundled-distribution probe and precedence logic have
/// no OS coupling, even though the launcher that consumes them is Unix-only
/// at v0.1.
pub mod maven_home;

#[cfg(unix)]
pub mod launcher;

#[cfg(unix)]
pub mod respawn;

#[cfg(unix)]
pub use launcher::{
    DaemonHandle, ERR_CODE_JAR_NOT_FOUND, ERR_CODE_SPAWN_TIMEOUT, JvmEntry, LaunchPlan,
    LauncherError, discover_jvm_entry, discover_or_spawn,
};

#[cfg(unix)]
pub use respawn::{RespawnError, RespawnOutcome, submit_with_respawn};

pub use maven_home::{
    MavenHomeSource, ResolvedMavenHome, bundled_maven_home, resolve_maven_home,
    resolve_maven_home_from_env,
};

pub use workers::{WorkersError, available_parallelism_or_one, resolve_workers};
