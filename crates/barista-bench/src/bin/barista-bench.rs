// SPDX-License-Identifier: MIT OR Apache-2.0

//! `barista-bench` — the benchmark-runner CLI.
//!
//! Reads one or more `Bench.toml` manifests, runs the cross-tool
//! baselines they declare with warmup + measured iterations, captures
//! per-iteration wall-clock time, and emits one `results.json`
//! document per `(manifest, baseline)` pair against the v1 schema.
//!
//! The shape mirrors what the Tier-2 regression gate
//! (`.github/workflows/perf-gate.yml`) and the Tier-3 dashboard ingest
//! pipeline (`bench.barista.build`) expect — the placeholder
//! `barista-bench run --corpus tier-2 --baselines barista --iterations
//! 5 --output .perf-gate/current/` invocation documented in that
//! workflow becomes executable when this binary lands.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use barista_bench::{
    Baseline, IterationMeasurement, Manifest, ResultsDocument, RunHardware, Summary, load_manifest,
    write_results,
};
use clap::{Parser, Subcommand};

// ---------------------------------------------------------------------------
// CLI surface
// ---------------------------------------------------------------------------

/// Run Barista benchmark manifests with cross-tool baselines and emit
/// `results.json` per the `barista.bench.results/v1` schema.
#[derive(Debug, Parser)]
#[command(
    name = "barista-bench",
    version,
    about = "Run Barista benchmark manifests and emit results.json.",
    propagate_version = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: BenchCommand,
}

#[derive(Debug, Subcommand)]
enum BenchCommand {
    /// Run one manifest (`--manifest`) or every manifest in a corpus
    /// directory (`--corpus`).
    Run(RunArgs),
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// Path to a single `Bench.toml` manifest.
    #[arg(long, value_name = "PATH", conflicts_with = "corpus")]
    manifest: Option<PathBuf>,

    /// Corpus directory containing one subdirectory per benchmark
    /// target. Each subdirectory must contain a `Bench.toml`.
    #[arg(long, value_name = "DIR", conflicts_with = "manifest")]
    corpus: Option<PathBuf>,

    /// Comma-separated list of baseline IDs to include (e.g.
    /// `barista,mvn`). Default: run every baseline declared by the
    /// manifest.
    #[arg(long, value_name = "IDS", value_delimiter = ',')]
    baselines: Vec<String>,

    /// Glob-ish filter on manifest IDs (substring match). When set,
    /// only manifests whose `id` contains the filter are run. Useful
    /// with `--corpus`.
    #[arg(long, value_name = "PATTERN")]
    filter: Option<String>,

    /// Override the manifest's `iterations` value (measured runs).
    #[arg(long, value_name = "N")]
    iterations: Option<u32>,

    /// Override the manifest's `warmup_iterations` value.
    #[arg(long, value_name = "N")]
    warmup_iterations: Option<u32>,

    /// Override the manifest's `iteration_spacing_seconds`. Sleeps
    /// between successive iterations (warmup AND measured); never
    /// before the first or after the last. Set higher than the
    /// manifest's default when an upstream rate-limit is observed;
    /// set to 0 to disable spacing entirely.
    #[arg(long, value_name = "SECONDS")]
    iteration_spacing_seconds: Option<u32>,

    /// Output directory. Each `(manifest, baseline)` pair writes to
    /// `<output>/<manifest_id>/<baseline_id>.json`. Default:
    /// `bench-runs/<run_id>/`.
    #[arg(long, value_name = "DIR")]
    output: Option<PathBuf>,

    /// Identifier of the runner producing these results, echoed into
    /// `results.json::runner_id`. Default: hostname when available,
    /// else `local-dev`.
    #[arg(long, value_name = "ID")]
    runner_id: Option<String>,

    /// Tag this run with a `barista_version` string. Default: the
    /// `BARISTA_VERSION` environment variable when set, else the
    /// version of *this* binary (which is built from the same
    /// workspace as `barista`).
    #[arg(long, value_name = "SEMVER")]
    barista_version: Option<String>,

    /// Print the resolved plan and exit without running any
    /// subprocesses. Useful for dry-running CI wiring changes.
    #[arg(long)]
    dry_run: bool,

    /// Capture pass: route each barista subprocess through a
    /// per-iteration `mitmdump` reverse-proxy session, parse the
    /// resulting HAR, and write per-iteration `network_calls` +
    /// `network_bytes` into each `results.json`. Wall-clock under
    /// `--capture` is mitmproxy-instrumented and is NOT comparable
    /// to a `--capture`-free timing pass — emit both passes
    /// separately if you want both numbers.
    ///
    /// mvn / mvnd baselines are not captured by this flag at v0.1
    /// (their proxy wiring lives in
    /// `scripts/run-baseline-captures.sh`); barista baselines are
    /// detected by argv[0] = `"barista"` and captured. Other
    /// baselines run normally and leave `network_*` as `None`.
    #[cfg(feature = "capture")]
    #[arg(long)]
    capture: bool,

    /// Upstream URL the capture-mode reverse proxy forwards barista
    /// requests to. Defaults to Maven Central; override for an
    /// alternate mirror.
    #[cfg(feature = "capture")]
    #[arg(
        long,
        value_name = "URL",
        default_value = "https://repo.maven.apache.org/maven2"
    )]
    capture_upstream: String,
}

fn main() {
    let cli = Cli::parse();
    let exit = match cli.command {
        BenchCommand::Run(args) => run(args),
    };
    std::process::exit(exit);
}

// ---------------------------------------------------------------------------
// `run` subcommand
// ---------------------------------------------------------------------------

fn run(args: RunArgs) -> i32 {
    let manifests = match collect_manifests(&args) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("barista-bench: {e}");
            return 2;
        }
    };
    if manifests.is_empty() {
        eprintln!("barista-bench: no manifests matched the selection");
        return 2;
    }

    let git_sha = git_head_sha().unwrap_or_else(|| "0".repeat(40));
    let timestamp = rfc3339_now();
    let run_id = format!("{}-{}", timestamp, &git_sha[..8]);
    let output_root = args
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from("bench-runs").join(&run_id));
    let hardware = detect_hardware();
    let runner_id = args
        .runner_id
        .clone()
        .or_else(hostname)
        .unwrap_or_else(|| "local-dev".to_string());
    let barista_version = args
        .barista_version
        .clone()
        .or_else(|| std::env::var("BARISTA_VERSION").ok())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    eprintln!("barista-bench: run_id = {run_id}");
    eprintln!("barista-bench: output  = {}", output_root.display());
    eprintln!("barista-bench: runner  = {runner_id}");
    eprintln!("barista-bench: git     = {git_sha}");
    eprintln!(
        "barista-bench: hw      = {}, {} core(s) {}",
        hardware.cpu, hardware.cores_logical, hardware.os
    );

    let baseline_filter: Option<Vec<&str>> = if args.baselines.is_empty() {
        None
    } else {
        Some(args.baselines.iter().map(String::as_str).collect())
    };

    let mut had_error = false;
    for (manifest_path, manifest) in &manifests {
        if let Some(pat) = &args.filter
            && !manifest
                .id
                .to_ascii_lowercase()
                .contains(&pat.to_ascii_lowercase())
        {
            continue;
        }
        let work_dir = manifest_path.parent().unwrap_or_else(|| Path::new("."));
        let checkout_dir = work_dir.join("checkout");
        // If the manifest sits next to a `checkout/`, run baselines
        // inside it; otherwise run them in the manifest's directory.
        let cwd = if checkout_dir.is_dir() {
            checkout_dir
        } else {
            work_dir.to_path_buf()
        };

        let baselines =
            filter_baselines(&manifest.effective_baselines(), baseline_filter.as_deref());
        if baselines.is_empty() {
            eprintln!(
                "barista-bench: {}: no baselines after filter — skipping",
                manifest.id
            );
            continue;
        }

        let iterations = args.iterations.unwrap_or(manifest.iterations);
        let warmup = args.warmup_iterations.unwrap_or(manifest.warmup_iterations);
        let spacing = std::time::Duration::from_secs(
            args.iteration_spacing_seconds
                .unwrap_or(manifest.iteration_spacing_seconds) as u64,
        );

        for baseline in &baselines {
            #[cfg(feature = "capture")]
            let mode_tag = if args.capture { " [capture]" } else { "" };
            #[cfg(not(feature = "capture"))]
            let mode_tag = "";
            eprintln!(
                "\nbarista-bench: {} / {}{}  ({} warmup + {} measured) — cwd={}",
                manifest.id,
                baseline.id,
                mode_tag,
                warmup,
                iterations,
                cwd.display()
            );
            if args.dry_run {
                eprintln!("  command : {}", baseline.command);
                if let Some(p) = &baseline.prepare {
                    eprintln!("  prepare : {p}");
                }
                continue;
            }
            // Per-iteration cold-cache root, materialized only when
            // the manifest opts in. Each iteration gets its own
            // subdirectory under this root so a forensic look at
            // "what files did iter 3 download" stays straightforward.
            let cold_cache_root: Option<PathBuf> = match manifest.cache_isolation {
                barista_bench::CacheIsolation::PerIteration => Some(
                    output_root
                        .join("cold-caches")
                        .join(&manifest.id)
                        .join(&baseline.id),
                ),
                barista_bench::CacheIsolation::None => None,
            };

            #[cfg(feature = "capture")]
            let measurement = if args.capture {
                let har_dir = output_root
                    .join(&manifest.id)
                    .join(format!("{}-capture", baseline.id));
                capture::measure_baseline_with_capture(
                    &cwd,
                    baseline,
                    warmup,
                    iterations,
                    spacing,
                    &args.capture_upstream,
                    &har_dir,
                    cold_cache_root.as_deref(),
                )
            } else {
                measure_baseline(
                    &cwd,
                    baseline,
                    warmup,
                    iterations,
                    spacing,
                    cold_cache_root.as_deref(),
                )
            };
            #[cfg(not(feature = "capture"))]
            let measurement = measure_baseline(
                &cwd,
                baseline,
                warmup,
                iterations,
                spacing,
                cold_cache_root.as_deref(),
            );

            match measurement {
                Ok(iters) => {
                    let doc = build_results_doc(
                        manifest,
                        baseline,
                        iters,
                        &run_id,
                        &timestamp,
                        &git_sha,
                        &barista_version,
                        &runner_id,
                        &hardware,
                    );
                    let out_path = output_root
                        .join(&manifest.id)
                        .join(format!("{}.json", baseline.id));
                    if let Some(parent) = out_path.parent() {
                        if let Err(e) = fs::create_dir_all(parent) {
                            eprintln!("  ✗ create_dir_all {}: {e}", parent.display());
                            had_error = true;
                            continue;
                        }
                    }
                    if let Err(e) = write_results(&out_path, &doc) {
                        eprintln!("  ✗ write_results: {e}");
                        had_error = true;
                        continue;
                    }
                    eprintln!(
                        "  ✓ median {:>7.1} ms   p95 {:>7.1} ms   stddev {:>6.1} ms   → {}",
                        doc.summary.median_wall_ms,
                        doc.summary.p95_wall_ms,
                        doc.summary.stddev_wall_ms,
                        out_path.display()
                    );
                }
                Err(e) => {
                    eprintln!("  ✗ {e}");
                    had_error = true;
                }
            }
        }
    }

    // Index of results for the dashboard ingest pipeline. Cheap; always
    // emitted so `bench.barista.build` can be pointed at a local
    // run-directory in dev.
    if !args.dry_run
        && let Err(e) = write_index(
            &output_root,
            &run_id,
            &timestamp,
            &git_sha,
            &hardware,
            &runner_id,
        )
    {
        eprintln!("barista-bench: warning: could not write index.json: {e}");
    }

    if had_error { 1 } else { 0 }
}

// ---------------------------------------------------------------------------
// Manifest collection
// ---------------------------------------------------------------------------

fn collect_manifests(args: &RunArgs) -> Result<Vec<(PathBuf, Manifest)>, String> {
    if let Some(path) = &args.manifest {
        let m = load_manifest(path).map_err(|e| format!("loading {}: {e}", path.display()))?;
        return Ok(vec![(path.clone(), m)]);
    }
    if let Some(dir) = &args.corpus {
        let mut entries: Vec<PathBuf> = Vec::new();
        let read =
            fs::read_dir(dir).map_err(|e| format!("reading corpus dir {}: {e}", dir.display()))?;
        for child in read.flatten() {
            let p = child.path();
            if !p.is_dir() {
                continue;
            }
            let manifest_path = p.join("Bench.toml");
            if manifest_path.is_file() {
                entries.push(manifest_path);
            }
        }
        entries.sort();
        let mut out = Vec::with_capacity(entries.len());
        for path in entries {
            let m = load_manifest(&path).map_err(|e| format!("loading {}: {e}", path.display()))?;
            out.push((path, m));
        }
        return Ok(out);
    }
    Err("must pass either --manifest <PATH> or --corpus <DIR>".to_string())
}

fn filter_baselines(all: &[Baseline], filter: Option<&[&str]>) -> Vec<Baseline> {
    match filter {
        None => all.to_vec(),
        Some(want) => all
            .iter()
            .filter(|b| want.contains(&b.id.as_str()))
            .cloned()
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

fn measure_baseline(
    cwd: &Path,
    baseline: &Baseline,
    warmup: u32,
    iterations: u32,
    spacing: std::time::Duration,
    cache_root_base: Option<&Path>,
) -> Result<Vec<IterationMeasurement>, String> {
    // Warmup runs: discard times, but they DO get the `prepare` step so
    // each iteration starts from a clean tree.
    for w in 0..warmup {
        if w > 0 {
            sleep_with_notice(spacing, "warmup", w);
        }
        let env = cold_cache_env(cache_root_base, "warmup", w)?;
        if let Some(prepare) = &baseline.prepare {
            run_argv_with_env(cwd, prepare, /*measured=*/ false, &env)
                .map_err(|e| format!("warmup prepare failed: {e}"))?;
        }
        let _ = run_argv_with_env(cwd, &baseline.command, /*measured=*/ false, &env)?;
    }
    // Measured runs.
    let mut iters = Vec::with_capacity(iterations as usize);
    for i in 0..iterations {
        // Space between iterations (NOT before the first; NOT after
        // the last). When warmup_iterations > 0, also space between
        // the final warmup and the first measured iteration — Maven
        // Central's rate-limit window doesn't distinguish them.
        if i > 0 || warmup > 0 {
            sleep_with_notice(spacing, "iter", i);
        }
        let env = cold_cache_env(cache_root_base, "iter", i)?;
        if let Some(prepare) = &baseline.prepare {
            run_argv_with_env(cwd, prepare, /*measured=*/ false, &env)
                .map_err(|e| format!("iteration {i} prepare failed: {e}"))?;
        }
        let start = Instant::now();
        let exit = run_argv_with_env(cwd, &baseline.command, /*measured=*/ true, &env)?;
        let wall_ms = start.elapsed().as_millis() as u64;
        iters.push(IterationMeasurement {
            iteration: i,
            wall_ms,
            cpu_user_ms: None,
            cpu_sys_ms: None,
            peak_rss_kb: None,
            network_calls: None,
            network_bytes: None,
            disk_read_bytes: None,
            disk_write_bytes: None,
            exit_code: exit,
        });
    }
    Ok(iters)
}

/// Sleep `dur` between iterations, with a stderr notice so the
/// operator knows the bench isn't hung. No-op for zero-second
/// durations.
fn sleep_with_notice(dur: std::time::Duration, phase: &str, idx: u32) {
    if dur.is_zero() {
        return;
    }
    eprintln!(
        "  ⏱  sleeping {}s before {phase} {idx} (rate-limit-aware spacing)",
        dur.as_secs()
    );
    std::thread::sleep(dur);
}

/// Compute the env-var pairs that route the subprocess at iteration
/// `idx` of phase `phase` (`"warmup"` or `"iter"`) to an isolated
/// cache root under `cache_root_base`. Returns an empty vec when the
/// manifest didn't opt into cache isolation. The function creates
/// the iteration's tempdir (plus `barista/` + `m2/` subdirectories)
/// before returning so the subprocess can write into them.
///
/// All paths are absolutised before they're handed to the subprocess
/// because `cmd.current_dir(...)` makes the subprocess's CWD different
/// from the parent's CWD — a relative `BARISTA_PATHS__CACHE_DIR` would
/// resolve against the subprocess's CWD (typically a checkout) and
/// silently miss the intended tempdir.
///
/// Three env vars are set:
///
/// - `BARISTA_PATHS__CACHE_DIR` — barista's CAS/index/lock root.
/// - `BARISTA_PATHS__M2_REPOSITORY` — barista's fallback for
///   already-fetched artifacts. Without overriding this, barista
///   hardlinks straight out of the user's `~/.m2/repository` and
///   makes ZERO network calls even with an empty CAS, which makes
///   "cold-cache" measurements meaningless.
/// - `MAVEN_OPTS` (with `-Dmaven.repo.local=...`) — mvn/mvnd's local
///   repository, so each iteration is genuinely cold against
///   `mvn dependency:resolve` too.
fn cold_cache_env(
    cache_root_base: Option<&Path>,
    phase: &str,
    idx: u32,
) -> Result<Vec<(&'static str, String)>, String> {
    let Some(base) = cache_root_base else {
        return Ok(Vec::new());
    };
    let iter_dir = base.join(format!("{phase}-{idx}"));
    let barista_cache = iter_dir.join("barista");
    let m2 = iter_dir.join("m2");
    fs::create_dir_all(&barista_cache).map_err(|e| {
        format!(
            "creating cold-cache barista root {}: {e}",
            barista_cache.display()
        )
    })?;
    fs::create_dir_all(&m2)
        .map_err(|e| format!("creating cold-cache m2 root {}: {e}", m2.display()))?;
    // Resolve to absolute paths so a subprocess CWD change doesn't
    // misroute. `canonicalize` follows symlinks, which is what we
    // want — the iteration dirs are real paths after the
    // `create_dir_all` above.
    let barista_cache_abs = fs::canonicalize(&barista_cache).map_err(|e| {
        format!(
            "canonicalize {} (barista cache root): {e}",
            barista_cache.display()
        )
    })?;
    let m2_abs = fs::canonicalize(&m2)
        .map_err(|e| format!("canonicalize {} (m2 root): {e}", m2.display()))?;
    Ok(vec![
        (
            "BARISTA_PATHS__CACHE_DIR",
            barista_cache_abs.display().to_string(),
        ),
        ("BARISTA_PATHS__M2_REPOSITORY", m2_abs.display().to_string()),
        (
            "MAVEN_OPTS",
            format!("-Dmaven.repo.local={}", m2_abs.display()),
        ),
    ])
}

/// Run an argv-split command in `cwd`, optionally with extra env
/// vars layered onto the subprocess. Returns the exit code; an exit
/// other than `0` is an error during warmup (we abort) but allowed
/// during measurement (recorded on the iteration so the dashboard can
/// flag failed runs).
fn run_argv_with_env(
    cwd: &Path,
    cmdline: &str,
    measured: bool,
    env: &[(&str, String)],
) -> Result<i32, String> {
    let argv = shell_split(cmdline);
    if argv.is_empty() {
        return Err(format!("empty command: {cmdline:?}"));
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    // Stream stdout/stderr to /dev/null during measurement so terminal
    // I/O doesn't dominate the timing on a small workload.
    // `BARISTA_BENCH_PASSTHROUGH=1` flips both streams to inherit for
    // debugging a failing baseline; the wall-clock measurement is
    // then noise-polluted (terminal I/O) but the failure cause is
    // visible.
    let passthrough = std::env::var("BARISTA_BENCH_PASSTHROUGH").is_ok();
    if passthrough {
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());
    } else {
        cmd.stdout(Stdio::null());
        // Keep stderr open during measurement so a failure surfaces
        // its diagnostic. The overhead of writing a few hundred bytes
        // of error text is negligible against second-scale timings
        // and is invaluable when a baseline fails: pre-fix the
        // harness would surface only `non-zero exit (2) from ...`
        // with the actual cause silenced.
        cmd.stderr(Stdio::inherit());
    }
    let status = cmd.status().map_err(|e| {
        format!(
            "failed to spawn `{}` (cwd={}): {e}",
            argv.join(" "),
            cwd.display()
        )
    })?;
    let code = status.code().unwrap_or(-1);
    if !measured && code != 0 {
        return Err(format!(
            "non-zero exit ({}) from `{}` (cwd={})",
            code,
            argv.join(" "),
            cwd.display()
        ));
    }
    Ok(code)
}

/// Whitespace-aware argv split honoring double-quoted segments. This
/// is intentionally a tenth as featureful as a real shell — Bench.toml
/// commands are author-controlled and should keep clear of `$VAR`,
/// `&&`, `|`, etc. (use multiple `prepare` strings instead).
fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
            }
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------------------
// Results assembly + summary stats
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_results_doc(
    manifest: &Manifest,
    baseline: &Baseline,
    iterations: Vec<IterationMeasurement>,
    run_id: &str,
    timestamp: &str,
    git_sha: &str,
    barista_version: &str,
    runner_id: &str,
    hardware: &RunHardware,
) -> ResultsDocument {
    let summary = summarize(&iterations);
    let mut metadata = BTreeMap::new();
    metadata.insert(
        "baseline_display_name".to_string(),
        baseline.display_name.clone(),
    );
    if let Some(corpus_id) = &manifest.corpus_id {
        metadata.insert("corpus_id".to_string(), corpus_id.clone());
    }
    ResultsDocument {
        schema: barista_bench::RESULTS_SCHEMA.to_string(),
        manifest_id: manifest.id.clone(),
        baseline_id: Some(baseline.id.clone()),
        resolved_command: Some(baseline.command.clone()),
        run_id: run_id.to_string(),
        timestamp: timestamp.to_string(),
        git_sha: git_sha.to_string(),
        barista_version: barista_version.to_string(),
        hardware_tier: manifest.hardware_tier,
        runner_id: runner_id.to_string(),
        hardware: hardware.clone(),
        iterations,
        summary,
        metadata,
    }
}

fn summarize(iters: &[IterationMeasurement]) -> Summary {
    debug_assert!(!iters.is_empty(), "summarize requires ≥1 iteration");
    // Filter to successful iterations only — failed iterations (exit
    // code ≠ 0; common when Maven Central rate-limits a rapid
    // cold-pull sequence with HTTP 429) measure 'how fast the tool
    // surfaces the failure', not the workload. If literally every
    // iteration failed, fall back to the full set so the summary
    // isn't a degenerate zero — the dashboard can still see the
    // non-zero exit codes per iteration and flag the run.
    let successful: Vec<&IterationMeasurement> =
        iters.iter().filter(|i| i.exit_code == 0).collect();
    let source: Vec<&IterationMeasurement> = if successful.is_empty() {
        iters.iter().collect()
    } else {
        successful
    };
    let n = source.len() as f64;
    let walls: Vec<f64> = source.iter().map(|i| i.wall_ms as f64).collect();
    let avg = walls.iter().sum::<f64>() / n;
    let mut sorted = walls.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = if iters.len() % 2 == 1 {
        sorted[iters.len() / 2]
    } else {
        (sorted[iters.len() / 2 - 1] + sorted[iters.len() / 2]) / 2.0
    };
    // Nearest-rank p95.
    let p95_index = ((iters.len() as f64) * 0.95).ceil() as usize - 1;
    let p95 = sorted[p95_index.min(sorted.len() - 1)];
    // Sample stddev (n-1 denominator when n>1; 0 when n=1).
    let stddev = if iters.len() < 2 {
        0.0
    } else {
        let var = walls.iter().map(|x| (x - avg).powi(2)).sum::<f64>() / ((n) - 1.0);
        var.sqrt()
    };
    Summary {
        avg_wall_ms: avg,
        median_wall_ms: median,
        p95_wall_ms: p95,
        stddev_wall_ms: stddev,
    }
}

// ---------------------------------------------------------------------------
// Run-time metadata: git SHA, RFC 3339 timestamp, hardware fingerprint
// ---------------------------------------------------------------------------

fn git_head_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(sha)
    } else {
        None
    }
}

/// Best-effort RFC 3339 in UTC. Avoids pulling in `chrono` — we just
/// need YYYY-MM-DDTHH:MM:SSZ.
fn rfc3339_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Convert epoch seconds to UTC components. Naive (no leap seconds,
    // ignores TAI vs UTC drift) but sufficient for run_id stability.
    let (year, month, day, hour, min, sec) = epoch_to_utc(now);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Convert epoch seconds to (Y, M, D, h, m, s) in UTC. Algorithm
/// derived from Howard Hinnant's `days_from_civil` / `civil_from_days`
/// proof — exact for the proleptic Gregorian calendar.
fn epoch_to_utc(epoch_secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let z = (epoch_secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y } as u32;
    let secs_in_day = epoch_secs % 86_400;
    let h = (secs_in_day / 3_600) as u32;
    let min = ((secs_in_day % 3_600) / 60) as u32;
    let s = (secs_in_day % 60) as u32;
    (y, m, d, h, min, s)
}

fn hostname() -> Option<String> {
    let out = Command::new("hostname").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

fn detect_hardware() -> RunHardware {
    // Per-OS best-effort detection. Falls through to opaque defaults
    // rather than failing the run — the dashboard can flag undetected
    // hardware as `unknown` rows.
    let os = detect_os();
    if cfg!(target_os = "macos") {
        let cpu = sysctl_string("machdep.cpu.brand_string").unwrap_or_else(|| "unknown".into());
        let cores_logical = sysctl_u32("hw.logicalcpu").unwrap_or(0);
        let cores_physical = sysctl_u32("hw.physicalcpu").unwrap_or(cores_logical);
        let mem_bytes = sysctl_u64("hw.memsize").unwrap_or(0);
        let memory_gb = (mem_bytes / 1024 / 1024 / 1024) as u32;
        return RunHardware {
            id: hostname().unwrap_or_else(|| "local-dev".into()),
            cpu,
            cores_physical,
            cores_logical,
            memory_gb,
            os,
        };
    }
    if cfg!(target_os = "linux") {
        let cpu =
            read_first_line("/proc/cpuinfo", "model name").unwrap_or_else(|| "unknown".into());
        let cores_logical = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(0);
        let memory_gb = read_first_line("/proc/meminfo", "MemTotal:")
            .and_then(|s| {
                s.split_whitespace()
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .map(|kb| (kb / 1024 / 1024) as u32)
            .unwrap_or(0);
        return RunHardware {
            id: hostname().unwrap_or_else(|| "local-dev".into()),
            cpu,
            cores_physical: cores_logical,
            cores_logical,
            memory_gb,
            os,
        };
    }
    RunHardware {
        id: hostname().unwrap_or_else(|| "local-dev".into()),
        cpu: "unknown".into(),
        cores_physical: 0,
        cores_logical: std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(0),
        memory_gb: 0,
        os,
    }
}

fn detect_os() -> String {
    if cfg!(target_os = "macos") {
        // Best-effort: ask sw_vers.
        if let Some(name) = command_output(&["sw_vers", "-productName"])
            && let Some(version) = command_output(&["sw_vers", "-productVersion"])
        {
            return format!("{name} {version}");
        }
        return "macOS".into();
    }
    if cfg!(target_os = "linux") {
        if let Some(line) = read_first_line("/etc/os-release", "PRETTY_NAME=") {
            return line.trim_matches('"').into();
        }
        return "Linux".into();
    }
    std::env::consts::OS.into()
}

fn sysctl_string<S: AsRef<OsStr>>(key: S) -> Option<String> {
    command_output(&["sysctl", "-n", key.as_ref().to_str()?])
}

fn sysctl_u32<S: AsRef<OsStr>>(key: S) -> Option<u32> {
    sysctl_string(key).and_then(|s| s.parse::<u32>().ok())
}

fn sysctl_u64<S: AsRef<OsStr>>(key: S) -> Option<u64> {
    sysctl_string(key).and_then(|s| s.parse::<u64>().ok())
}

fn command_output(argv: &[&str]) -> Option<String> {
    let out = Command::new(argv[0])
        .args(&argv[1..])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

/// Read the value associated with `prefix` from the named file.
/// For `/proc/cpuinfo` / `/etc/os-release` shapes where lines look
/// like `key : value` or `KEY=value`.
fn read_first_line(path: &str, prefix: &str) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(
                rest.trim_start_matches(|c: char| c == ':' || c == '=' || c.is_whitespace())
                    .to_string(),
            );
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Index file — small companion document the dashboard can poll for a
// list of runs without scanning the whole bucket.
// ---------------------------------------------------------------------------

fn write_index(
    output_root: &Path,
    run_id: &str,
    timestamp: &str,
    git_sha: &str,
    hardware: &RunHardware,
    runner_id: &str,
) -> Result<(), String> {
    fs::create_dir_all(output_root).map_err(|e| e.to_string())?;
    // Enumerate every results.json under output_root so the dashboard
    // can render the run without re-walking the bucket. Keys are
    // relative-to-index paths; tighter than absolute paths and works
    // regardless of where the directory ends up uploaded.
    let mut results = Vec::new();
    walk_results(output_root, output_root, &mut results)
        .map_err(|e| format!("scanning results: {e}"))?;
    results.sort();
    let index = serde_json::json!({
        "schema": "barista.bench.index/v1",
        "run_id": run_id,
        "timestamp": timestamp,
        "git_sha": git_sha,
        "runner_id": runner_id,
        "hardware": hardware,
        "results_root": ".",
        "results": results,
        "produced_by": "barista-bench"
    });
    let path = output_root.join("index.json");
    let mut text = serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?;
    text.push('\n');
    fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

fn walk_results(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_results(root, &path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("json")
            && path.file_name().and_then(|s| s.to_str()) != Some("index.json")
        {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests — focus on argv splitting + summary stats. Subprocess + git +
// hardware detection paths are covered by integration runs.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_split_plain_words() {
        let v = shell_split("barista pull --update");
        assert_eq!(v, vec!["barista", "pull", "--update"]);
    }

    #[test]
    fn shell_split_quoted_argument() {
        let v = shell_split("mvn -Dmessage=\"hello world\" verify");
        assert_eq!(v, vec!["mvn", "-Dmessage=hello world", "verify"]);
    }

    #[test]
    fn shell_split_empty_string_yields_empty_vec() {
        let v = shell_split("   ");
        assert!(v.is_empty());
    }

    #[test]
    fn summarize_single_iter_has_zero_stddev() {
        let iters = vec![iter(0, 100)];
        let s = summarize(&iters);
        assert!((s.avg_wall_ms - 100.0).abs() < 1e-9);
        assert!((s.median_wall_ms - 100.0).abs() < 1e-9);
        assert!((s.p95_wall_ms - 100.0).abs() < 1e-9);
        assert!(s.stddev_wall_ms.abs() < 1e-9);
    }

    #[test]
    fn summarize_odd_count_median_is_middle_element() {
        let iters = vec![iter(0, 30), iter(1, 10), iter(2, 20)];
        let s = summarize(&iters);
        assert!((s.median_wall_ms - 20.0).abs() < 1e-9);
        assert!((s.avg_wall_ms - 20.0).abs() < 1e-9);
    }

    #[test]
    fn summarize_even_count_median_is_avg_of_two_middle() {
        let iters = vec![iter(0, 10), iter(1, 20), iter(2, 30), iter(3, 40)];
        let s = summarize(&iters);
        // sorted: 10, 20, 30, 40 -> median = (20+30)/2 = 25.
        assert!((s.median_wall_ms - 25.0).abs() < 1e-9);
    }

    #[test]
    fn summarize_p95_uses_nearest_rank() {
        // 20 iterations of 1..=20. p95 should be the 19th element
        // (nearest-rank with n=20, p=0.95 → ceil(20*0.95)=19 → idx 18).
        let iters: Vec<_> = (1..=20).map(|i| iter(i - 1, i as u64)).collect();
        let s = summarize(&iters);
        assert!((s.p95_wall_ms - 19.0).abs() < 1e-9, "got {}", s.p95_wall_ms);
    }

    #[test]
    fn epoch_to_utc_known_dates() {
        // 1970-01-01T00:00:00Z
        assert_eq!(epoch_to_utc(0), (1970, 1, 1, 0, 0, 0));
        // 2024-01-01T00:00:00Z = 1_704_067_200
        assert_eq!(epoch_to_utc(1_704_067_200), (2024, 1, 1, 0, 0, 0));
        // 2024-02-29T12:34:56Z (leap year) = 1_709_210_096
        assert_eq!(epoch_to_utc(1_709_210_096), (2024, 2, 29, 12, 34, 56));
    }

    fn iter(idx: u32, wall_ms: u64) -> IterationMeasurement {
        IterationMeasurement {
            iteration: idx,
            wall_ms,
            cpu_user_ms: None,
            cpu_sys_ms: None,
            peak_rss_kb: None,
            network_calls: None,
            network_bytes: None,
            disk_read_bytes: None,
            disk_write_bytes: None,
            exit_code: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// `--capture` mode: drive a per-iteration mitmproxy reverse-proxy
// session, parse the captured HAR via barista-netanalyze, and emit
// `network_calls` + `network_bytes` alongside `wall_ms`.
//
// Gated by the `capture` cargo feature (on by default). The feature
// pulls in `tokio` + `barista-netcap` + `barista-netanalyze`; hosts
// without mitmproxy installed can build with `--no-default-features`
// to get a timing-only harness with no async runtime.
// ---------------------------------------------------------------------------

#[cfg(feature = "capture")]
mod capture {
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use barista_bench::IterationMeasurement;
    use barista_netanalyze::har::parse_har_bytes;
    use barista_netcap::{CaptureConfig, CaptureSession};

    use super::{Baseline, shell_split};

    /// Time mitmdump takes to bind its listen socket after spawn.
    /// mitmproxy doesn't emit a "ready" signal we can wait on (per the
    /// netcap docs), so we poll for a TCP listener with a tight budget.
    const READY_POLL_TIMEOUT: Duration = Duration::from_secs(5);
    const READY_POLL_INTERVAL: Duration = Duration::from_millis(50);

    /// How the harness can route a given baseline through mitmproxy
    /// for HAR capture.
    ///
    /// `Barista` uses **reverse-proxy** mode: mitmdump listens on
    /// plain HTTP at `localhost:PORT` and forwards every request to
    /// the configured upstream over HTTPS. barista is told about the
    /// proxy via `BARISTA_TEST_UPSTREAM_URL`, which rebases every
    /// fetch URL onto `http://localhost:PORT/<path>`. No CA cert
    /// install needed because the subprocess only talks plain HTTP
    /// to localhost.
    ///
    /// `Maven` uses **forward-proxy** mode: mitmdump listens on
    /// `localhost:PORT` and intercepts HTTPS traffic via TLS-MITM.
    /// Maven Resolver (Aether) deliberately **ignores** the JVM
    /// `-Dhttps.proxyHost` system property — the only reliable way
    /// to route mvn through a proxy is a `<proxies>` block in a
    /// `settings.xml` consumed via `--settings <path>`. The harness
    /// synthesises that file per iteration. mitmproxy's CA must be
    /// in the JDK truststore for the TLS-MITM to succeed; that's a
    /// one-time operator step documented in
    /// `crates/barista-netcap/README.md`.
    ///
    /// `None` means the baseline can't be captured by this harness
    /// (e.g. `barista --no-daemon ...`, which forks an upstream mvn
    /// that bypasses the env hook AND doesn't have a settings.xml
    /// surface the harness can inject into). These baselines fall
    /// through to the timing-only path with `network_*` left as
    /// `None` so the dashboard renders `—`.
    enum BaselineCapture {
        Barista,
        Maven,
        None,
    }

    fn baseline_capture_kind(baseline: &Baseline) -> BaselineCapture {
        let argv = shell_split(&baseline.command);
        match argv.first().map(String::as_str) {
            Some("barista") if !argv.iter().any(|a| a == "--no-daemon") => BaselineCapture::Barista,
            Some("mvn") | Some("mvnd") => BaselineCapture::Maven,
            _ => BaselineCapture::None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn measure_baseline_with_capture(
        cwd: &Path,
        baseline: &Baseline,
        warmup: u32,
        iterations: u32,
        spacing: std::time::Duration,
        upstream_url: &str,
        har_dir: &Path,
        cache_root_base: Option<&Path>,
    ) -> Result<Vec<IterationMeasurement>, String> {
        let kind = baseline_capture_kind(baseline);
        if matches!(kind, BaselineCapture::None) {
            eprintln!(
                "  ⓘ {} is not capturable by this harness; running timing-only (network_* will be None).",
                baseline.id
            );
            return super::measure_baseline(
                cwd,
                baseline,
                warmup,
                iterations,
                spacing,
                cache_root_base,
            );
        }

        std::fs::create_dir_all(har_dir)
            .map_err(|e| format!("could not create HAR output dir {}: {e}", har_dir.display()))?;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime build: {e}"))?;

        // Warmup iterations: same shape as production timing path, but
        // each subprocess goes through a capture session so the warmup
        // JIT is identical to the measured runs. HAR is discarded.
        for w in 0..warmup {
            if w > 0 {
                super::sleep_with_notice(spacing, "warmup", w);
            }
            let env = super::cold_cache_env(cache_root_base, "warmup", w)?;
            if let Some(prepare) = &baseline.prepare {
                super::run_argv_with_env(cwd, prepare, /*measured=*/ false, &env)
                    .map_err(|e| format!("warmup {w} prepare failed: {e}"))?;
            }
            let _ = rt
                .block_on(run_one_capture_iteration(
                    cwd,
                    baseline,
                    &kind,
                    upstream_url,
                    &har_dir.join(format!("warmup-{w}.har")),
                    &env,
                    har_dir,
                    "warmup",
                    w,
                ))
                .map_err(|e| format!("warmup {w} capture iteration failed: {e}"))?;
        }

        // Measured iterations: same flow, HAR retained, request counts
        // populated.
        let mut iters = Vec::with_capacity(iterations as usize);
        for i in 0..iterations {
            // Space between iterations (NOT before the first; NOT
            // after the last). Also space between the final warmup
            // and the first measured iteration when both exist —
            // upstream rate-limit windows don't distinguish warmup
            // from measured.
            if i > 0 || warmup > 0 {
                super::sleep_with_notice(spacing, "iter", i);
            }
            let env = super::cold_cache_env(cache_root_base, "iter", i)?;
            if let Some(prepare) = &baseline.prepare {
                super::run_argv_with_env(cwd, prepare, /*measured=*/ false, &env)
                    .map_err(|e| format!("iter {i} prepare failed: {e}"))?;
            }
            let har_path = har_dir.join(format!("iter-{i}.har"));
            let outcome = rt
                .block_on(run_one_capture_iteration(
                    cwd,
                    baseline,
                    &kind,
                    upstream_url,
                    &har_path,
                    &env,
                    har_dir,
                    "iter",
                    i,
                ))
                .map_err(|e| format!("iter {i} capture failed: {e}"))?;
            let (calls, bytes) = parse_har_counts(&har_path).unwrap_or((None, None));
            iters.push(IterationMeasurement {
                iteration: i,
                wall_ms: outcome.wall_ms,
                cpu_user_ms: None,
                cpu_sys_ms: None,
                peak_rss_kb: None,
                network_calls: calls,
                network_bytes: bytes,
                disk_read_bytes: None,
                disk_write_bytes: None,
                exit_code: outcome.exit_code,
            });
        }
        Ok(iters)
    }

    struct IterationOutcome {
        wall_ms: u64,
        exit_code: i32,
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_one_capture_iteration(
        cwd: &Path,
        baseline: &Baseline,
        kind: &BaselineCapture,
        upstream_url: &str,
        har_path: &Path,
        extra_env: &[(&str, String)],
        side_artifacts_dir: &Path,
        phase: &str,
        idx: u32,
    ) -> Result<IterationOutcome, String> {
        // Build the mitmproxy config + the argv/env shape for the
        // subprocess based on which tool the baseline drives.
        // - Barista: reverse-proxy mode + `BARISTA_TEST_UPSTREAM_URL`.
        // - Maven:   forward-proxy mode (HTTPS-MITM via JDK truststore)
        //            + `--settings <generated-xml>` injected into argv.
        let (cfg, argv, extra_env_for_mvn) = match kind {
            BaselineCapture::Barista => {
                // mitmproxy's `reverse:` mode wants `scheme://host[:port]`
                // only — no path. Split the upstream URL so mitmdump
                // gets the host part and barista gets the path part.
                let (mitm_target, _) = split_upstream(upstream_url)?;
                let mut cfg = CaptureConfig::for_har(har_path.to_path_buf());
                cfg.extra_args = vec![
                    "--mode".to_string(),
                    format!("reverse:{mitm_target}"),
                    "--ssl-insecure".to_string(),
                ];
                (
                    cfg,
                    shell_split(&baseline.command),
                    Vec::<(&'static str, String)>::new(),
                )
            }
            BaselineCapture::Maven => {
                // Forward-proxy mode is mitmdump's default — no
                // `--mode` argument. mitmdump listens on PORT, TLS-MITMs
                // outbound HTTPS using its CA (operator-installed into
                // the JDK truststore once; see netcap README).
                let cfg = CaptureConfig::for_har(har_path.to_path_buf());
                // Maven Resolver IGNORES `-Dhttps.proxyHost` — the only
                // reliable wiring is a settings.xml `<proxies>` block
                // consumed via `--settings <path>`. We synthesise a
                // one-shot file per iteration and inject the flag.
                let argv = shell_split(&baseline.command);
                if argv.is_empty() {
                    return Err(format!("empty command: {:?}", baseline.command));
                }
                // mitmdump's port isn't known until after spawn, so the
                // settings.xml is written inside the spawn ceremony
                // below (see `mvn_settings_xml`). We pass a Vec::new()
                // placeholder here and the actual --settings flag is
                // pushed onto argv after the proxy spawns.
                (cfg, argv, Vec::<(&'static str, String)>::new())
            }
            BaselineCapture::None => unreachable!("filtered in caller"),
        };

        let session = CaptureSession::start(cfg)
            .await
            .map_err(|e| format!("CaptureSession::start: {e}"))?;
        let listen_port = session.listen_port();
        wait_for_listener(listen_port).await?;

        // Per-tool finalization that needs the proxy's bound port.
        let (argv, kind_env): (Vec<String>, Vec<(&'static str, String)>) = match kind {
            BaselineCapture::Barista => {
                let (_, upstream_path) = split_upstream(upstream_url)?;
                let proxy_url = format!("http://127.0.0.1:{listen_port}{upstream_path}");
                (argv, vec![("BARISTA_TEST_UPSTREAM_URL", proxy_url)])
            }
            BaselineCapture::Maven => {
                let settings_xml = mvn_settings_xml(side_artifacts_dir, phase, idx, listen_port)?;
                // Inject `--settings <path>` right after argv[0] so it
                // applies before any other flag mvn parses.
                let mut new_argv = Vec::with_capacity(argv.len() + 2);
                new_argv.push(argv[0].clone());
                new_argv.push("--settings".to_string());
                new_argv.push(settings_xml.display().to_string());
                new_argv.extend(argv.into_iter().skip(1));
                (new_argv, Vec::new())
            }
            BaselineCapture::None => unreachable!(),
        };
        let _ = extra_env_for_mvn; // reserved for future per-tool env

        // Subprocess setup. Wall_ms measures just the subprocess — not
        // the proxy spawn/teardown overhead.
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.current_dir(cwd);
        let passthrough = std::env::var("BARISTA_BENCH_PASSTHROUGH").is_ok();
        if passthrough {
            cmd.stdout(Stdio::inherit());
            cmd.stderr(Stdio::inherit());
        } else {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::inherit());
        }
        for (k, v) in &kind_env {
            cmd.env(k, v);
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }

        let start = Instant::now();
        let status = cmd
            .status()
            .map_err(|e| format!("failed to spawn `{}`: {e}", argv.join(" ")))?;
        let wall_ms = start.elapsed().as_millis() as u64;
        let exit_code = status.code().unwrap_or(-1);

        // Tear down the proxy. mitmdump flushes HAR on signal; the
        // post-stop file is what we parse.
        let _ = session
            .stop()
            .await
            .map_err(|e| format!("CaptureSession::stop: {e}"))?;

        Ok(IterationOutcome { wall_ms, exit_code })
    }

    /// Write a one-shot `settings.xml` for the given iteration that
    /// routes every Maven Resolver request through the localhost
    /// mitmproxy on `port`. Returns the absolute path so the caller
    /// can pass it via `--settings <path>` to mvn.
    fn mvn_settings_xml(
        side_artifacts_dir: &Path,
        phase: &str,
        idx: u32,
        port: u16,
    ) -> Result<std::path::PathBuf, String> {
        let filename = format!("{phase}-{idx}.settings.xml");
        let path = side_artifacts_dir.join(filename);
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<settings xmlns="http://maven.apache.org/SETTINGS/1.0.0">
  <proxies>
    <proxy>
      <id>barista-bench-https</id>
      <active>true</active>
      <protocol>https</protocol>
      <host>127.0.0.1</host>
      <port>{port}</port>
    </proxy>
    <proxy>
      <id>barista-bench-http</id>
      <active>true</active>
      <protocol>http</protocol>
      <host>127.0.0.1</host>
      <port>{port}</port>
    </proxy>
  </proxies>
</settings>
"#,
        );
        std::fs::write(&path, body).map_err(|e| {
            format!(
                "writing per-iteration settings.xml at {}: {e}",
                path.display()
            )
        })?;
        // Absolute path so mvn doesn't resolve it against its CWD.
        std::fs::canonicalize(&path)
            .map_err(|e| format!("canonicalize settings.xml at {}: {e}", path.display()))
    }

    async fn wait_for_listener(port: u16) -> Result<(), String> {
        let deadline = Instant::now() + READY_POLL_TIMEOUT;
        loop {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "mitmdump did not begin listening on 127.0.0.1:{port} within {READY_POLL_TIMEOUT:?}"
                ));
            }
            tokio::time::sleep(READY_POLL_INTERVAL).await;
        }
    }

    /// Split an upstream URL like `https://repo.maven.apache.org/maven2`
    /// into `("https://repo.maven.apache.org", "/maven2")`. The first
    /// half is what mitmdump's `--mode reverse:` accepts; the second
    /// half is the path prefix we hand back to barista via
    /// `BARISTA_TEST_UPSTREAM_URL`. Path-less inputs return an empty
    /// second element.
    fn split_upstream(url: &str) -> Result<(String, String), String> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| format!("upstream URL missing scheme: {url:?}"))?;
        match rest.find('/') {
            Some(slash) => {
                let host = &rest[..slash];
                let path = &rest[slash..];
                Ok((format!("{scheme}://{host}"), path.to_string()))
            }
            None => Ok((format!("{scheme}://{rest}"), String::new())),
        }
    }

    /// Parse the HAR at `path` and return `(network_calls, network_bytes)`.
    /// Returns `(None, None)` if the file doesn't exist or doesn't
    /// parse; the harness reports the iteration as zero-counts rather
    /// than failing the whole baseline.
    fn parse_har_counts(path: &Path) -> Option<(Option<u64>, Option<u64>)> {
        let bytes = std::fs::read(path).ok()?;
        let har = parse_har_bytes(&bytes).ok()?;
        let entries = &har.log.entries;
        let calls = entries.len() as u64;
        let total_bytes: i64 = entries
            .iter()
            .map(|e| {
                // HAR content.size can be -1 ("unknown"); clamp to 0.
                e.response.content.size.max(0)
            })
            .sum();
        Some((Some(calls), Some(total_bytes.max(0) as u64)))
    }

    #[cfg(test)]
    mod split_upstream_tests {
        use super::split_upstream;

        #[test]
        fn splits_off_maven_path() {
            let (host, path) = split_upstream("https://repo.maven.apache.org/maven2").unwrap();
            assert_eq!(host, "https://repo.maven.apache.org");
            assert_eq!(path, "/maven2");
        }

        #[test]
        fn host_only_returns_empty_path() {
            let (host, path) = split_upstream("https://repo.maven.apache.org").unwrap();
            assert_eq!(host, "https://repo.maven.apache.org");
            assert_eq!(path, "");
        }

        #[test]
        fn rejects_missing_scheme() {
            assert!(split_upstream("repo.maven.apache.org/maven2").is_err());
        }
    }
}
