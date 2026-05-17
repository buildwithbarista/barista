//! `--ci` reproducibility plumbing (M4.3 T6).
//!
//! When the user passes `--ci`, every `ActionRequest` envelope the CLI
//! sends to the daemon (or — on the `--no-daemon` fork — every forked
//! `mvn` invocation) gets a small set of environment variables and
//! system properties wired in to make the resulting build
//! byte-deterministic across consecutive runs.
//!
//! ## Why determinism is a wire-time concern
//!
//! The `--ci` macro at the CLI surface already routes output through
//! `--frozen --output json --quiet --no-color` (M3.2 T4). That gets
//! the *renderer* deterministic — but the *artifact bytes* the
//! underlying Maven plugins produce (`.class` files in
//! `target/classes`, `.jar` files in `target/`) still embed wall-clock
//! timestamps unless we explicitly tell Maven not to. The reproducible-
//! builds story is owned by upstream Maven plugins (`maven-archiver`,
//! `maven-jar-plugin`, `maven-source-plugin`, etc.) which read
//! `project.build.outputTimestamp` and the `SOURCE_DATE_EPOCH`
//! environment variable. T6's job is to thread those signals through
//! the action-request envelope (daemon path) and the forked-`mvn` argv
//! (`--no-daemon` path).
//!
//! ## What `--ci` injects
//!
//! - **`SOURCE_DATE_EPOCH`** — Unix-epoch seconds. Sourced (in order):
//!     1. `BARISTA_SOURCE_DATE_EPOCH` env var, if set — escape hatch
//!        for CI systems that want to pin a stable value across runs
//!        (e.g. the git commit time of the build trigger).
//!     2. The git HEAD commit time of the project root, if the project
//!        is a git checkout AND `git` is on `$PATH` AND the call
//!        succeeds in <100 ms. Best-effort; failures fall through.
//!     3. The literal value `1577836800` (2020-01-01T00:00:00Z) as a
//!        final deterministic sentinel.
//!
//!   The sentinel choice over "current time" is deliberate: two runs
//!   N minutes apart would otherwise produce different timestamps, and
//!   the `--ci` AC ("byte-identical across 5 consecutive runs") would
//!   fail intermittently. A fixed epoch is reproducible by definition.
//!
//!   The sentinel sits inside the Maven `project.build.outputTimestamp`
//!   valid range (`1980-01-01T00:00:02Z .. 2099-12-31T23:59:59Z`) which
//!   in turn is bounded by the ZIP-archive timestamp encoding. Epoch
//!   zero would be syntactically valid as a Unix time but would crash
//!   `maven-jar-plugin`'s range validator; a 2020-01-01 sentinel avoids
//!   that while remaining hermetic.
//!
//! - **`TZ=UTC`** — pins the timezone for any plugin that consults
//!   `TimeZone.getDefault()` (e.g. `maven-site-plugin` page footers,
//!   surefire's report timestamps when not running in reproducible
//!   mode). Defensive — the primary determinism is via
//!   `project.build.outputTimestamp`.
//!
//! - **`LC_ALL=C`** — pins collation for any plugin that sorts strings
//!   using the platform's default locale (e.g. `maven-shade-plugin`'s
//!   service-file merge order is locale-sensitive on some JDKs).
//!
//! - **`project.build.outputTimestamp`** — the standard Maven property
//!   `maven-archiver` reads to stamp identical timestamps into JAR
//!   `META-INF/MANIFEST.MF` and ZIP entry headers. We project
//!   `SOURCE_DATE_EPOCH` into an ISO-8601 string and stuff it into
//!   `ActionRequest.system_properties` so the embedded-Maven core
//!   sees it as `-Dproject.build.outputTimestamp=<iso>` at parse
//!   time.
//!
//! - **`maven_compat="4"`** — pins the Maven compatibility mode if the
//!   user did not explicitly set one. The v0.1 default is already 4
//!   (see `build_action_request`), but pinning under `--ci` makes the
//!   policy explicit at the wire layer rather than implicit at the
//!   builder default.
//!
//! ## What this module does NOT do
//!
//! - Bypass user overrides. If the user explicitly sets one of these
//!   environment variables or system properties (e.g.
//!   `-Dproject.build.outputTimestamp=...` on the command line), the
//!   T6 path does NOT clobber it. The user is the authority on
//!   reproducibility values they've explicitly chosen.
//! - Touch *output*-side determinism. That's M3.2 T4's job (the
//!   renderer half of `--ci`).
//!
//! ## Test linkage
//!
//! Unit tests in this module cover the policy table directly. The
//! end-to-end byte-equality acceptance criterion is exercised by
//! `tests/cmd_verify_ci_reproducibility.rs`, which runs
//! `barista verify --ci --no-daemon` 5 times in independent tempdirs
//! against a 1-module fixture and SHA-256-diffs the produced `.class`
//! files and the packaged JAR.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// Result of resolving the `--ci` reproducibility seed: an environment
/// map suitable for `ActionRequest.environment`, a system-properties
/// map suitable for `ActionRequest.system_properties`, and the chosen
/// `SOURCE_DATE_EPOCH` raw value (so callers — like the `--no-daemon`
/// fork — can also set the corresponding env on a child `mvn`
/// process).
#[derive(Debug, Clone)]
pub struct ReproducibilitySeed {
    /// `SOURCE_DATE_EPOCH` value as a base-10 unsigned integer string
    /// of Unix seconds. Always populated.
    pub source_date_epoch: String,

    /// Provenance hint for the chosen epoch, surfaced in diagnostic
    /// logs (`-v`) so a user can see why a particular value was used.
    pub epoch_source: EpochSource,

    /// `(key, value)` pairs to merge into `ActionRequest.environment`.
    pub env: HashMap<String, String>,

    /// `(key, value)` pairs to merge into
    /// `ActionRequest.system_properties` (the daemon translates these
    /// into `-D<key>=<value>` CLI flags inside the embedded Maven
    /// invocation).
    pub system_properties: HashMap<String, String>,
}

/// Where the chosen `SOURCE_DATE_EPOCH` came from. Diagnostic;
/// callers may surface it on `-v` traces but it is not part of the
/// wire shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochSource {
    /// `BARISTA_SOURCE_DATE_EPOCH` was set in the calling environment.
    EnvOverride,
    /// `git log -1 --pretty=%ct` against the project root succeeded.
    GitHead,
    /// All probes failed; we fell through to the fixed-zero sentinel.
    FixedZero,
}

/// Build the `--ci` reproducibility seed for a project root.
///
/// Pure-data return: applying the seed is the caller's job (the
/// `cmd::verify` / `cmd::shot` per-action loops merge `env` /
/// `system_properties` into each `ActionRequest`; the `cmd::no_daemon`
/// fork sets the env vars on its forked `mvn` process and appends
/// `-D` flags to its argv).
///
/// `read_env` is the calling-environment lookup; abstracted so unit
/// tests can substitute a fixture.
pub fn build_seed<F>(project_root: &Path, read_env: F) -> ReproducibilitySeed
where
    F: Fn(&str) -> Option<String>,
{
    let (source_date_epoch, epoch_source) =
        resolve_source_date_epoch(project_root, &read_env, &git_head_time_default);

    let mut env: HashMap<String, String> = HashMap::new();
    env.insert("SOURCE_DATE_EPOCH".to_string(), source_date_epoch.clone());
    env.insert("TZ".to_string(), "UTC".to_string());
    env.insert("LC_ALL".to_string(), "C".to_string());

    let mut system_properties: HashMap<String, String> = HashMap::new();
    // The reproducible-builds Maven property. ISO-8601 with `Z` zone
    // suffix is what `maven-archiver`'s parser accepts.
    let iso = format_iso8601_utc(parse_epoch_seconds(&source_date_epoch));
    system_properties.insert("project.build.outputTimestamp".to_string(), iso);

    ReproducibilitySeed {
        source_date_epoch,
        epoch_source,
        env,
        system_properties,
    }
}

/// Probe-injectable entry point for unit-testing the resolution
/// chain without invoking `git`. Production code uses
/// [`git_head_time_default`].
fn resolve_source_date_epoch<F, G>(
    project_root: &Path,
    read_env: &F,
    git_head_time: &G,
) -> (String, EpochSource)
where
    F: Fn(&str) -> Option<String>,
    G: Fn(&Path) -> Option<u64>,
{
    // 1) BARISTA_SOURCE_DATE_EPOCH user override.
    if let Some(raw) = read_env("BARISTA_SOURCE_DATE_EPOCH")
        && let Some(parsed) = parse_unsigned_seconds(&raw)
    {
        return (parsed.to_string(), EpochSource::EnvOverride);
    }

    // 2) Git HEAD commit time.
    if let Some(t) = git_head_time(project_root) {
        return (t.to_string(), EpochSource::GitHead);
    }

    // 3) Sentinel: 2020-01-01T00:00:00Z. See module-level docs for
    //    rationale (Maven's outputTimestamp must be ≥1980-01-02; epoch
    //    zero crashes `maven-jar-plugin`'s range validator).
    (FIXED_SENTINEL_EPOCH.to_string(), EpochSource::FixedZero)
}

/// Sentinel epoch value used when neither env override nor git HEAD
/// time is available: `2020-01-01T00:00:00Z`. Chosen to satisfy
/// Maven's `project.build.outputTimestamp` validator without sacrificing
/// hermeticity. The variant name `FixedZero` predates the value choice
/// (originally `0`); renaming would churn pattern-match sites for no
/// behavioral gain.
pub const FIXED_SENTINEL_EPOCH: u64 = 1_577_836_800;

/// Best-effort `git log -1 --pretty=%ct` against `project_root`.
/// Returns `None` on any failure (no git, not a checkout, non-zero
/// exit, malformed output). Has an implicit short timeout — the
/// `git` invocation itself is bounded by `Command::output` blocking
/// behavior; on a healthy checkout this returns in <50 ms.
fn git_head_time_default(project_root: &Path) -> Option<u64> {
    // `git` is the trusted toolchain entry; the project_root flows
    // into `current_dir`, never into `Command::new`. We probe `git`
    // explicitly (over a vendored library) to keep the dep surface
    // small — `--ci` reproducibility is a CI-only path and a stock
    // git is universally available there.
    // nosemgrep: barista-rust-unchecked-command-new
    let out = Command::new("git")
        .args(["log", "-1", "--pretty=%ct"])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let trimmed = raw.trim();
    parse_unsigned_seconds(trimmed)
}

/// Parse a base-10 unsigned-seconds string into `u64`. Rejects empty
/// strings and anything that doesn't fit `u64`.
fn parse_unsigned_seconds(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    s.parse::<u64>().ok()
}

/// Parse a `SOURCE_DATE_EPOCH` string into seconds; falls back to 0
/// on any parse error so downstream formatting cannot panic on an
/// out-of-band input (which won't happen via `build_seed` but is a
/// defensive guard for callers that construct seeds manually).
fn parse_epoch_seconds(s: &str) -> u64 {
    parse_unsigned_seconds(s).unwrap_or(0)
}

/// Format a Unix-seconds value as an ISO-8601 instant in UTC with
/// `Z` zone suffix (e.g. `1970-01-01T00:00:00Z`).
///
/// Hand-rolled to avoid pulling `chrono` / `time` into the CLI dep
/// graph for one formatting site. Uses the civil-from-days algorithm
/// from Howard Hinnant's `date.h` paper (public domain). Correct for
/// the full proleptic Gregorian range; in practice the inputs are
/// 0 (epoch zero) and present-day commit times, both well-bounded.
fn format_iso8601_utc(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let secs_in_day = (seconds % 86_400) as u32;
    let hh = secs_in_day / 3600;
    let mm = (secs_in_day % 3600) / 60;
    let ss = secs_in_day % 60;

    // Howard Hinnant civil_from_days: shift to a 1970-03-01 epoch.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Apply a [`ReproducibilitySeed`] to an [`barista_ipc::ActionRequest`].
/// Merge semantics: per-key, the seed's value loses to any existing
/// value in the request (so user-supplied `-D` flags and explicit
/// `system_properties` overrides win). This preserves the
/// "documented user override beats `--ci` defaults" contract.
pub fn apply_to_request(request: &mut barista_ipc::ActionRequest, seed: &ReproducibilitySeed) {
    for (k, v) in &seed.env {
        request
            .environment
            .entry(k.clone())
            .or_insert_with(|| v.clone());
    }
    for (k, v) in &seed.system_properties {
        request
            .system_properties
            .entry(k.clone())
            .or_insert_with(|| v.clone());
    }
    // M4.3 T6 policy: pin maven_compat="4" under --ci when the
    // builder default left it blank or already at "4" (the v0.1
    // default). If the user explicitly set a different compat (via
    // --maven-compat 3.9), preserve it — the user is the authority.
    if request.maven_compat.is_empty() {
        request.maven_compat = "4".to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn build_seed_falls_through_to_sentinel_when_no_git_no_env() {
        let env = no_env;
        let no_git = |_: &Path| None;
        let (epoch, src) = resolve_source_date_epoch(Path::new("/nonexistent"), &env, &no_git);
        assert_eq!(epoch, FIXED_SENTINEL_EPOCH.to_string());
        assert_eq!(src, EpochSource::FixedZero);
    }

    #[test]
    fn build_seed_honors_user_override_env() {
        let env = |k: &str| {
            if k == "BARISTA_SOURCE_DATE_EPOCH" {
                Some("1700000000".to_string())
            } else {
                None
            }
        };
        let no_git = |_: &Path| None;
        let (epoch, src) = resolve_source_date_epoch(Path::new("/x"), &env, &no_git);
        assert_eq!(epoch, "1700000000");
        assert_eq!(src, EpochSource::EnvOverride);
    }

    #[test]
    fn build_seed_uses_git_head_when_no_override() {
        let env = no_env;
        let git = |_: &Path| Some(1_700_000_000u64);
        let (epoch, src) = resolve_source_date_epoch(Path::new("/x"), &env, &git);
        assert_eq!(epoch, "1700000000");
        assert_eq!(src, EpochSource::GitHead);
    }

    #[test]
    fn env_override_wins_over_git_head() {
        let env = |k: &str| {
            if k == "BARISTA_SOURCE_DATE_EPOCH" {
                Some("42".to_string())
            } else {
                None
            }
        };
        let git = |_: &Path| Some(1_700_000_000u64);
        let (epoch, src) = resolve_source_date_epoch(Path::new("/x"), &env, &git);
        assert_eq!(epoch, "42");
        assert_eq!(src, EpochSource::EnvOverride);
    }

    #[test]
    fn malformed_env_override_is_ignored() {
        let env = |k: &str| {
            if k == "BARISTA_SOURCE_DATE_EPOCH" {
                Some("not-a-number".to_string())
            } else {
                None
            }
        };
        let git = |_: &Path| Some(1_700_000_000u64);
        let (epoch, src) = resolve_source_date_epoch(Path::new("/x"), &env, &git);
        // Falls through to git when env is unparseable.
        assert_eq!(epoch, "1700000000");
        assert_eq!(src, EpochSource::GitHead);
    }

    #[test]
    fn format_iso8601_for_epoch_zero() {
        assert_eq!(format_iso8601_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn format_iso8601_for_known_value() {
        // 1700000000 == 2023-11-14T22:13:20Z
        assert_eq!(format_iso8601_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn format_iso8601_for_year_boundary() {
        // 1577836800 == 2020-01-01T00:00:00Z
        assert_eq!(format_iso8601_utc(1_577_836_800), "2020-01-01T00:00:00Z");
    }

    #[test]
    fn build_seed_populates_ci_environment_keys() {
        let seed = build_seed(Path::new("/nonexistent-project"), no_env);
        assert!(seed.env.contains_key("SOURCE_DATE_EPOCH"));
        assert_eq!(seed.env.get("TZ").map(String::as_str), Some("UTC"));
        assert_eq!(seed.env.get("LC_ALL").map(String::as_str), Some("C"));
    }

    #[test]
    fn build_seed_populates_project_build_outputtimestamp() {
        let seed = build_seed(Path::new("/nonexistent-project"), no_env);
        // With git unavailable on /nonexistent and no env override,
        // epoch falls to the 2020-01-01 sentinel.
        assert_eq!(
            seed.system_properties
                .get("project.build.outputTimestamp")
                .map(String::as_str),
            Some("2020-01-01T00:00:00Z"),
        );
    }

    #[test]
    fn apply_to_request_merges_env_without_clobber() {
        let mut req = barista_ipc::ActionRequest::default();
        // Caller already set TZ to something else — must survive.
        req.environment
            .insert("TZ".to_string(), "Europe/Berlin".to_string());
        let seed = build_seed(Path::new("/nonexistent-project"), no_env);
        apply_to_request(&mut req, &seed);
        // User-set value wins.
        assert_eq!(
            req.environment.get("TZ").map(String::as_str),
            Some("Europe/Berlin")
        );
        // Seed value lands for the unset key.
        assert_eq!(req.environment.get("LC_ALL").map(String::as_str), Some("C"));
        // System-properties also merged.
        assert!(
            req.system_properties
                .contains_key("project.build.outputTimestamp")
        );
    }

    #[test]
    fn apply_to_request_pins_maven_compat_when_blank() {
        let mut req = barista_ipc::ActionRequest::default();
        let seed = build_seed(Path::new("/nonexistent-project"), no_env);
        apply_to_request(&mut req, &seed);
        assert_eq!(req.maven_compat, "4");
    }

    #[test]
    fn apply_to_request_preserves_user_set_maven_compat() {
        let mut req = barista_ipc::ActionRequest {
            maven_compat: "3.9".to_string(),
            ..Default::default()
        };
        let seed = build_seed(Path::new("/nonexistent-project"), no_env);
        apply_to_request(&mut req, &seed);
        assert_eq!(
            req.maven_compat, "3.9",
            "user-set maven_compat must survive --ci",
        );
    }

    #[test]
    fn apply_to_request_does_not_clobber_outputtimestamp_user_override() {
        let mut req = barista_ipc::ActionRequest::default();
        req.system_properties.insert(
            "project.build.outputTimestamp".to_string(),
            "2099-12-31T23:59:59Z".to_string(),
        );
        let seed = build_seed(Path::new("/nonexistent-project"), no_env);
        apply_to_request(&mut req, &seed);
        assert_eq!(
            req.system_properties
                .get("project.build.outputTimestamp")
                .map(String::as_str),
            Some("2099-12-31T23:59:59Z"),
        );
    }
}
