//! Build-time identity probe for the `/version` ops endpoint.
//!
//! Emits three `cargo:rustc-env=…` directives so the crate can read
//! them back via `env!()` at compile time:
//!
//! - `ROASTERY_BUILD_GIT_SHA` — short (7+ char) git commit SHA, or the
//!   sentinel string `"unknown"` if the working tree isn't a git
//!   checkout / `git` isn't on `PATH`. The handler treats `"unknown"`
//!   as `null` in the JSON response so clean tarball installs don't
//!   surface a lie.
//! - `ROASTERY_BUILD_DATE` — RFC-3339 UTC timestamp captured at
//!   compile time, or `"unknown"` if `SystemTime::now()` somehow
//!   returns a pre-epoch value.
//! - `ROASTERY_BUILD_RUSTC` — output of `rustc -V`, or `"unknown"` if
//!   the subprocess fails for any reason.
//!
//! **None of these lookups may fail the build.** A failing build
//! script makes `cargo install roastery` impossible on machines that
//! happen not to have `git` on PATH (e.g. container builds from a
//! source tarball). Each probe is wrapped in a graceful fallback that
//! emits `"unknown"` on any error path.
//!
//! We also emit `cargo:rerun-if-changed=build.rs` to suppress the
//! default behaviour of re-running on every source change — the
//! build-time identity only needs to refresh when this file or the
//! current `HEAD` does. `cargo:rerun-if-changed=.git/HEAD` covers the
//! latter without forcing a rebuild when individual `src/` files
//! change.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Re-run when the current commit changes. `.git/HEAD` is the
    // cheapest stable signal across detached-HEAD and branch checkouts.
    // If the file doesn't exist (tarball install), cargo silently
    // ignores the directive — no harm done.
    println!("cargo:rerun-if-changed=.git/HEAD");

    let git_sha = git_short_sha().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=ROASTERY_BUILD_GIT_SHA={git_sha}");

    let build_date = build_date_rfc3339().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=ROASTERY_BUILD_DATE={build_date}");

    let rustc = rustc_version().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=ROASTERY_BUILD_RUSTC={rustc}");
}

/// Run `git rev-parse --short=12 HEAD` and return the trimmed stdout
/// on success, or `None` on any failure (no git, not a repo, …).
///
/// 12 hex chars is long enough to be unambiguous in practice while
/// still fitting on one line of `/version` JSON output. CI overrides
/// via the standard `GITHUB_SHA` env var if present — that way release
/// builds carry the exact commit the workflow ran on even when the
/// build runs out of a shallow clone.
fn git_short_sha() -> Option<String> {
    if let Ok(env_sha) = std::env::var("GITHUB_SHA")
        && !env_sha.is_empty()
    {
        // Trim to the same 12-char width `git rev-parse --short=12`
        // would emit. `chars().take(…)` is byte-safe for hex.
        let short: String = env_sha.chars().take(12).collect();
        return Some(short);
    }

    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Compose an RFC-3339 timestamp from `SystemTime::now()` without
/// pulling in `chrono`/`time` as a build dep.
///
/// The format is `YYYY-MM-DDTHH:MM:SSZ` (UTC, no fractional seconds).
/// Returns `None` only on the impossible-in-practice case that the
/// build clock is before the Unix epoch.
fn build_date_rfc3339() -> Option<String> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format_rfc3339_utc(secs))
}

/// Format `secs_since_epoch` as an RFC-3339 UTC string. Implemented
/// inline to avoid the `chrono`/`time` build-dep tax for a 5-line
/// calendar walk.
///
/// Handles years from 1970 onwards. Algorithm: civil_from_days from
/// Howard Hinnant's date library
/// (<https://howardhinnant.github.io/date_algorithms.html#civil_from_days>),
/// pared down to the unsigned slice we need (post-1970 dates, no era
/// arithmetic required).
fn format_rfc3339_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let secs_of_day = secs % 86_400;
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;

    // Shift so day 0 is 0000-03-01 (Hinnant's "era" trick). For any
    // build clock at-or-after 1970-01-01 the shifted value stays
    // comfortably positive, so we can do the whole walk in `u64`
    // without `as` casts.
    let z: u64 = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Run `rustc -V` and capture its stdout (e.g.
/// `"rustc 1.84.0 (9fc6b4312 2024-12-30)"`). Returns `None` on any
/// failure — the absence of `rustc` on the build machine is in theory
/// impossible during a `cargo build`, but the graceful path costs us
/// nothing and keeps the build infallible.
fn rustc_version() -> Option<String> {
    // `RUSTC` is set by cargo to the absolute path of the compiler
    // that's about to be invoked; using it instead of bare `rustc`
    // means we report the actual rustc compiling this crate even when
    // multiple toolchains are installed.
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let out = Command::new(rustc).arg("-V").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
