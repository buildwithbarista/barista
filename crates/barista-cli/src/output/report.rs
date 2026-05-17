//! Structured "report" values produced by each command and consumed
//! by the per-format renderers.
//!
//! Each command-runner builds one of these and hands it to a
//! [`crate::output::Renderer`]. This indirection is the seam between
//! command logic (resolve, fetch, materialize) and presentation
//! (human / json / ndjson). It also lets schema work (M3.2 T2) bind
//! against a single set of types rather than the ad-hoc strings the
//! commands used to emit.
//!
//! # Conventions
//!
//! - All `serde::Serialize` fields use `#[serde(rename_all = "kebab-case")]`
//!   so the JSON shapes are consistent with the rest of the
//!   product (CLI flags, config keys, lockfile keys are all
//!   kebab-case).
//! - `Option`s use `#[serde(skip_serializing_if = "Option::is_none")]`
//!   so absent fields don't litter the JSON.
//! - Each report carries a `"command": "<verb>"` discriminator so a
//!   single JSON document can be routed by its shape, and so the
//!   schema work in T2 has a stable tag to key off.
//!
//! The shapes here intentionally cover only the v0.1 needs — T2
//! produces JSON schemas from these structs, and T5 tightens them as
//! more fields arrive.

use std::path::PathBuf;

use serde::Serialize;

/// Result reported by `barista pull`.
///
/// The "no-fetch" path is the only one fully wired in v0.1; the
/// full-fetch path returns an error today. The `fetched`, `strict`,
/// and `no-fetch` fields are populated regardless so downstream
/// tooling (and the JSON schema) has a stable shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "command")]
#[serde(rename = "pull")]
pub struct PullReport {
    /// Absolute path of the resolved project root.
    pub project_root: PathBuf,
    /// What happened to the lockfile this run.
    pub lockfile_status: LockfileStatus,
    /// Total entry count in the (possibly pre-existing) lockfile.
    pub entries: usize,
    /// How many artifacts were fetched on this run. `0` under
    /// `--no-fetch` and on the v0.1 full-fetch stub path.
    pub fetched: usize,
    /// Truncated project signature, when known. Helps users tie a
    /// run to a lockfile generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_signature: Option<String>,
    /// Human-readable coordinate of the project being pulled.
    /// Echoed by the human renderer; absent under `--quiet`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coords: Option<String>,
    /// `--no-fetch` flag echo.
    pub no_fetch: bool,
    /// `--strict` flag echo.
    pub strict: bool,
}

/// What `pull` did with the lockfile this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LockfileStatus {
    /// No lockfile existed; under `--no-fetch` this is informational.
    Absent,
    /// Lockfile existed; nothing was written.
    Unchanged,
    /// Lockfile was written (or rewritten).
    #[allow(dead_code)] // wired by the v0.2 full-fetch path
    Written,
    /// Would have been written if not for `--dry-run` / a stub path.
    #[allow(dead_code)]
    WouldWrite,
}

/// Result reported by `barista grind tree`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "command")]
#[serde(rename = "grind-tree")]
pub struct GrindTreeReport {
    /// Stable shape version for downstream consumers.
    pub schema_version: u32,
    /// Reactor modules, in lockfile order.
    pub reactor: Vec<ReactorModule>,
    /// Resolved entries, in lockfile order.
    pub nodes: Vec<TreeNode>,
}

/// JSON representation of one reactor module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct ReactorModule {
    pub coords: String,
    pub version: String,
    pub relative_path: String,
}

/// JSON representation of one resolved entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct TreeNode {
    pub coords: String,
    pub version: String,
    pub scope: String,
    pub depth: u32,
    pub from_path: Vec<String>,
}

/// Result reported by `barista pour`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "command")]
#[serde(rename = "pour")]
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
    /// Destination paths. For real runs, paths actually written;
    /// for dry-runs, paths that *would* have been written. Same
    /// length as [`Self::planned`].
    pub planned_paths: Vec<PathBuf>,
}

impl PourReport {
    /// Render a single human-readable summary line. Preserved here
    /// (matching the pre-renderer `cmd::pour::PourReport::summary`
    /// output) so existing snapshot tests keep working after the
    /// type moved to the shared output module.
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

impl PullReport {
    /// Render a single human-readable summary line, mirroring the
    /// pre-renderer message body (`--no-fetch: <coords>: …`).
    pub fn summary(&self) -> String {
        let coords = self.coords.as_deref().unwrap_or("<unknown>");
        match self.lockfile_status {
            LockfileStatus::Absent => {
                format!(
                    "--no-fetch: {coords}: no existing barista.lock (would resolve and write one)"
                )
            }
            LockfileStatus::Unchanged => match &self.project_signature {
                Some(sig) => format!(
                    "--no-fetch: {coords}: existing barista.lock has {} entries (signature {sig})",
                    self.entries,
                ),
                None => format!(
                    "--no-fetch: {coords}: existing barista.lock has {} entries",
                    self.entries,
                ),
            },
            LockfileStatus::Written => {
                format!("{coords}: wrote barista.lock with {} entries", self.entries)
            }
            LockfileStatus::WouldWrite => {
                format!(
                    "{coords}: would write barista.lock with {} entries",
                    self.entries
                )
            }
        }
    }
}

/// Result reported by `barista verify` (and, eventually, every other
/// Maven-vocabulary lifecycle command — `clean`, `compile`, `test`, …).
///
/// The shape covers the v0.1 single-module happy path: a sequence of
/// mojo invocations, each carrying its lifecycle phase, the daemon-
/// side `ActionResult` exit code, and the wall-clock duration. Multi-
/// module reactor output (M4.3 Task 4) extends this with per-module
/// grouping; the v0.1 shape leaves room (`module` field on each
/// invocation) so the schema grows additively rather than via a
/// breaking change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "command")]
#[serde(rename = "verify")]
pub struct VerifyReport {
    /// Absolute path of the resolved project root.
    pub project_root: PathBuf,
    /// The terminal lifecycle phase the user asked for. Today always
    /// `"verify"`; M4.3 Task 2 reuses the same `VerifyReport` shape
    /// for the other Maven-vocabulary commands by setting `phase` to
    /// the requested phase name.
    pub phase: String,
    /// Number of mojo invocations the action-graph contained.
    pub planned_actions: usize,
    /// Number of mojo invocations that completed (success or failure).
    pub executed_actions: usize,
    /// Number of mojo invocations that returned a non-zero exit code.
    /// `0` on the happy path; any non-zero value means the build
    /// failed (and `executed_actions` may be less than
    /// `planned_actions` because execution stops at the first failure).
    pub failed_actions: usize,
    /// Number of times the daemon was auto-respawned because of a
    /// `BAR-DAEMON-CRASHED` mid-action. `0` on the happy path; `>0`
    /// surfaces the M4.2 T6 wire contract worked as intended.
    pub daemon_respawns: u32,
    /// Per-mojo invocation outcomes, in execution order.
    pub invocations: Vec<MojoInvocation>,
    /// Total wall-clock duration, milliseconds. Includes daemon
    /// startup + dispatch + collection.
    pub duration_ms: u64,
}

/// One mojo invocation result on the verify action graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct MojoInvocation {
    /// Maven lifecycle phase this mojo binds to, e.g. `"compile"`.
    pub phase: String,
    /// Canonical Maven plugin coordinate, e.g.
    /// `"org.apache.maven.plugins:maven-compiler-plugin:3.13.0:compile"`.
    pub mojo: String,
    /// Reactor module path (absolute), or the single project root in
    /// the single-module case.
    pub module: PathBuf,
    /// Mojo exit code; 0 on success.
    pub exit_code: i32,
    /// Daemon-side `ActionResult.status` rendered as a stable string
    /// (`"success" | "failure" | "timeout" | "crashed" | "cancelled"
    /// | "unknown"`).
    pub status: String,
    /// Human-readable failure message; empty on success.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub failure_message: String,
    /// Mojo wall-clock duration, milliseconds.
    pub duration_ms: u64,
}

impl VerifyReport {
    /// Human-readable summary line.
    pub fn summary(&self) -> String {
        let respawn_note = if self.daemon_respawns > 0 {
            format!(" (auto-respawned {}×)", self.daemon_respawns)
        } else {
            String::new()
        };
        if self.failed_actions == 0 {
            format!(
                "{phase}: {n}/{planned} mojo(s) in {ms} ms{respawn_note}",
                phase = self.phase,
                n = self.executed_actions,
                planned = self.planned_actions,
                ms = self.duration_ms,
            )
        } else {
            format!(
                "{phase}: {failed} mojo(s) failed (executed {n}/{planned}) in {ms} ms{respawn_note}",
                phase = self.phase,
                failed = self.failed_actions,
                n = self.executed_actions,
                planned = self.planned_actions,
                ms = self.duration_ms,
            )
        }
    }
}
