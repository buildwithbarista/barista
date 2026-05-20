// SPDX-License-Identifier: MIT OR Apache-2.0

//! `barista tap` — register and inspect taps.
//!
//! A **tap** is a named remote endpoint: a roastery shared-cache
//! server or a (placeholder) worker. v0.1 ships **registration and
//! inspection only**:
//!
//! - `tap add <name> <url> [--kind roastery|worker]` — register +
//!   persist.
//! - `tap list` — list registered taps (text table or `--output
//!   json`).
//! - `tap remove <name>` — remove + persist; idempotent (removing an
//!   absent tap succeeds quietly).
//! - `tap status [<name>]` — health-probe one tap, or every tap.
//!
//! Routing build actions to a tap is **out of scope** for v0.1.
//!
//! ## Persistence
//!
//! Each mutating command loads the `[[taps]]` section from
//! `barista.toml`, mutates the in-memory [`TapRegistry`], and writes
//! it back atomically. The config path is resolved from the global
//! `--config` flag, then `--root`/`-f`, then a walk-up search, and
//! finally defaults to `./barista.toml` (created on first `add`).
//!
//! ## Exit codes
//!
//! - `0` — success (including `list` of an empty registry and an
//!   idempotent no-op `remove`).
//! - `1` — a user/IO error (bad name/URL, unreadable config, etc.).
//! - For `status`: `0` only if **every** probed tap is healthy;
//!   `1` if any probed tap is unhealthy (so scripts can gate on it).
//!   Probing zero taps (empty registry) is `0`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use barista_tap::{DEFAULT_PROBE_TIMEOUT, Tap, TapHealth, TapRegistry, probe};
use serde::Serialize;

use crate::cli::{
    GlobalFlags, OutputFormat, TapAddArgs, TapCommand, TapRemoveArgs, TapStatusArgs,
};

/// Dispatch a `barista tap <sub>` invocation. Returns the exit code.
pub fn run(global: &GlobalFlags, cmd: &TapCommand) -> i32 {
    let result = match cmd {
        TapCommand::Add(args) => run_add(global, args),
        TapCommand::List(_) => run_list(global),
        TapCommand::Remove(args) => run_remove(global, args),
        TapCommand::Status(args) => run_status(global, args),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            emit_error(global, &e);
            1
        }
    }
}

/// Whether structured (JSON) output was requested.
fn json_output(global: &GlobalFlags) -> bool {
    matches!(global.output, OutputFormat::Json | OutputFormat::Ndjson)
}

fn emit_error(global: &GlobalFlags, e: &TapCmdError) {
    if json_output(global) {
        let body = serde_json::json!({ "command": "tap", "error": e.to_string() });
        // Pretty so the error is readable on a terminal; scripts parse
        // it the same either way.
        match serde_json::to_string_pretty(&body) {
            Ok(s) => println!("{s}"),
            Err(_) => eprintln!("error: {e}"),
        }
    } else {
        eprintln!("error: {e}");
    }
}

// ============================================================
// add / list / remove
// ============================================================

fn run_add(global: &GlobalFlags, args: &TapAddArgs) -> Result<i32, TapCmdError> {
    let path = resolve_config_path(global);
    let mut registry = load_registry(&path)?;

    let tap = Tap::new(&args.name, &args.url, args.kind.into())?;
    registry.add(tap)?;
    save_registry(&path, &registry)?;

    if json_output(global) {
        let added = registry.get(&args.name).map(TapView::from);
        print_json(&serde_json::json!({
            "command": "tap-add",
            "added": added,
            "config": path.display().to_string(),
        }))?;
    } else {
        println!(
            "added tap '{}' -> {} ({})",
            args.name,
            args.url,
            barista_tap::TapKind::from(args.kind)
        );
    }
    Ok(0)
}

fn run_list(global: &GlobalFlags) -> Result<i32, TapCmdError> {
    let path = resolve_config_path(global);
    let registry = load_registry(&path)?;

    if json_output(global) {
        let views: Vec<TapView> = registry.list().iter().map(TapView::from).collect();
        print_json(&serde_json::json!({ "command": "tap-list", "taps": views }))?;
    } else if registry.is_empty() {
        println!("no taps registered");
    } else {
        print_tap_table(registry.list());
    }
    Ok(0)
}

fn run_remove(global: &GlobalFlags, args: &TapRemoveArgs) -> Result<i32, TapCmdError> {
    let path = resolve_config_path(global);
    let mut registry = load_registry(&path)?;

    let removed = registry.remove(&args.name)?;
    if removed {
        save_registry(&path, &registry)?;
    }

    if json_output(global) {
        print_json(&serde_json::json!({
            "command": "tap-remove",
            "name": args.name,
            "removed": removed,
        }))?;
    } else if removed {
        println!("removed tap '{}'", args.name);
    } else {
        // Idempotent no-op: a clean success, not an error.
        println!("no tap named '{}' (nothing to remove)", args.name);
    }
    Ok(0)
}

// ============================================================
// status
// ============================================================

fn run_status(global: &GlobalFlags, args: &TapStatusArgs) -> Result<i32, TapCmdError> {
    let path = resolve_config_path(global);
    let registry = load_registry(&path)?;

    // Select the taps to probe.
    let targets: Vec<&Tap> = match &args.name {
        Some(name) => match registry.get(name) {
            Some(t) => vec![t],
            None => {
                return Err(TapCmdError::NotFound { name: name.clone() });
            }
        },
        None => registry.list().iter().collect(),
    };

    // Probe them on a small async runtime. A bounded timeout per
    // probe means a dead endpoint never hangs the command.
    let results = probe_all(&targets, DEFAULT_PROBE_TIMEOUT)?;

    let all_healthy = results.iter().all(|(_, h)| h.is_healthy());

    if json_output(global) {
        let views: Vec<StatusView> = results
            .iter()
            .map(|(t, h)| StatusView::new(t, h))
            .collect();
        print_json(&serde_json::json!({ "command": "tap-status", "taps": views }))?;
    } else if results.is_empty() {
        println!("no taps registered");
    } else {
        print_status_table(&results);
    }

    // Exit non-zero if any probed tap was unhealthy, so scripts can
    // gate on `barista tap status`. An empty registry is healthy by
    // vacuity (exit 0).
    Ok(if all_healthy { 0 } else { 1 })
}

/// Probe every target, preserving order. Builds a current-thread
/// tokio runtime with IO + time enabled (the probe needs both).
fn probe_all(
    targets: &[&Tap],
    timeout: Duration,
) -> Result<Vec<(Tap, TapHealth)>, TapCmdError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|e| TapCmdError::Runtime(e.to_string()))?;

    let results = runtime.block_on(async {
        let mut out = Vec::with_capacity(targets.len());
        for tap in targets {
            let health = probe(tap, timeout).await;
            out.push(((*tap).clone(), health));
        }
        out
    });
    Ok(results)
}

// ============================================================
// Config-path resolution + registry load/save
// ============================================================

/// Resolve the `barista.toml` path the tap commands operate on.
///
/// Precedence: `--config` > `--root <dir>/barista.toml` >
/// `-f <file>` (its parent dir) > a walk-up search for an existing
/// `barista.toml` > `./barista.toml`.
fn resolve_config_path(global: &GlobalFlags) -> PathBuf {
    if let Some(c) = &global.config {
        return c.clone();
    }
    if let Some(root) = &global.root {
        return root.join("barista.toml");
    }
    if let Some(file) = &global.file {
        // `-f` may name a pom file or a directory; in both cases the
        // sibling `barista.toml` is what we want.
        let dir = if file.is_dir() {
            file.clone()
        } else {
            file.parent().map(Path::to_path_buf).unwrap_or_default()
        };
        return dir.join("barista.toml");
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    walk_up_for_config(&cwd).unwrap_or_else(|| cwd.join("barista.toml"))
}

/// Walk up from `start` looking for an existing `barista.toml`,
/// bounded by a `.git` directory (the project boundary).
fn walk_up_for_config(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        let candidate = dir.join("barista.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if dir.join(".git").exists() {
            return None;
        }
        cur = dir.parent();
    }
    None
}

fn load_registry(path: &Path) -> Result<TapRegistry, TapCmdError> {
    let decls = barista_config::load_taps(path)?;
    Ok(TapRegistry::from_decls(decls)?)
}

fn save_registry(path: &Path, registry: &TapRegistry) -> Result<(), TapCmdError> {
    barista_config::save_taps(path, &registry.to_decls())?;
    Ok(())
}

// ============================================================
// Rendering
// ============================================================

/// JSON projection of a registered tap.
#[derive(Debug, Serialize)]
struct TapView {
    name: String,
    url: String,
    kind: String,
}

impl From<&Tap> for TapView {
    fn from(t: &Tap) -> Self {
        Self {
            name: t.name.clone(),
            url: t.url.to_string(),
            kind: t.kind.to_string(),
        }
    }
}

/// JSON projection of a status probe result.
#[derive(Debug, Serialize)]
struct StatusView {
    name: String,
    url: String,
    kind: String,
    healthy: bool,
    detail: String,
}

impl StatusView {
    fn new(tap: &Tap, health: &TapHealth) -> Self {
        let (healthy, detail) = match health {
            TapHealth::Healthy { detail } => (true, detail.clone()),
            TapHealth::Unhealthy { reason } => (false, reason.clone()),
        };
        Self {
            name: tap.name.clone(),
            url: tap.url.to_string(),
            kind: tap.kind.to_string(),
            healthy,
            detail,
        }
    }
}

fn print_json(value: &serde_json::Value) -> Result<(), TapCmdError> {
    let s = serde_json::to_string_pretty(value)?;
    println!("{s}");
    Ok(())
}

/// Render a clean fixed-column table of taps to stdout.
fn print_tap_table(taps: &[Tap]) {
    let name_w = taps
        .iter()
        .map(|t| t.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let url_w = taps
        .iter()
        .map(|t| t.url.as_str().len())
        .max()
        .unwrap_or(3)
        .max(3);
    println!("{:<name_w$}  {:<url_w$}  KIND", "NAME", "URL");
    for t in taps {
        println!(
            "{:<name_w$}  {:<url_w$}  {}",
            t.name,
            t.url.as_str(),
            t.kind
        );
    }
}

/// Render a status table to stdout:
/// `<name>  <url>  HEALTHY|UNHEALTHY (<reason>)`.
fn print_status_table(results: &[(Tap, TapHealth)]) {
    let name_w = results
        .iter()
        .map(|(t, _)| t.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let url_w = results
        .iter()
        .map(|(t, _)| t.url.as_str().len())
        .max()
        .unwrap_or(3)
        .max(3);
    for (t, h) in results {
        let status = match h {
            TapHealth::Healthy { detail } => format!("HEALTHY ({detail})"),
            TapHealth::Unhealthy { reason } => format!("UNHEALTHY ({reason})"),
        };
        println!(
            "{:<name_w$}  {:<url_w$}  {}",
            t.name,
            t.url.as_str(),
            status
        );
    }
}

// ============================================================
// Errors
// ============================================================

/// Errors surfaced by the `tap` command. Wraps the lower-level
/// [`barista_tap::TapError`] and adds CLI-only failure modes.
#[derive(Debug, thiserror::Error)]
enum TapCmdError {
    /// A tap-domain error (validation, duplicate, persistence).
    #[error(transparent)]
    Tap(#[from] barista_tap::TapError),

    /// `tap status <name>` named a tap that isn't registered.
    #[error("no tap named '{name}' is registered")]
    NotFound {
        /// The name that was looked up.
        name: String,
    },

    /// Failed to build the async runtime for `status`.
    #[error("could not start probe runtime: {0}")]
    Runtime(String),

    /// JSON serialization failed.
    #[error("serializing output: {0}")]
    Serialize(#[from] serde_json::Error),

    /// A persistence error from `barista-config`.
    #[error(transparent)]
    Persist(#[from] barista_config::TapPersistError),
}
