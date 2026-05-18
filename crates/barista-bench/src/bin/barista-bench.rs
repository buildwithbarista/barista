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
    Baseline, IterationMeasurement, Manifest, ResultsDocument, RunHardware, Summary,
    load_manifest, write_results,
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
    arg_required_else_help = true,
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
        .or_else(|| hostname())
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
    eprintln!("barista-bench: hw      = {}, {} core(s) {}",
        hardware.cpu, hardware.cores_logical, hardware.os);

    let baseline_filter: Option<Vec<&str>> = if args.baselines.is_empty() {
        None
    } else {
        Some(args.baselines.iter().map(String::as_str).collect())
    };

    let mut had_error = false;
    for (manifest_path, manifest) in &manifests {
        if let Some(pat) = &args.filter
            && !manifest.id.to_ascii_lowercase().contains(&pat.to_ascii_lowercase())
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

        let baselines = filter_baselines(&manifest.effective_baselines(), baseline_filter.as_deref());
        if baselines.is_empty() {
            eprintln!(
                "barista-bench: {}: no baselines after filter — skipping",
                manifest.id
            );
            continue;
        }

        let iterations = args.iterations.unwrap_or(manifest.iterations);
        let warmup = args.warmup_iterations.unwrap_or(manifest.warmup_iterations);

        for baseline in &baselines {
            eprintln!(
                "\nbarista-bench: {} / {}  ({} warmup + {} measured) — cwd={}",
                manifest.id,
                baseline.id,
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
            match measure_baseline(&cwd, baseline, warmup, iterations) {
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
        && let Err(e) = write_index(&output_root, &run_id, &timestamp, &git_sha, &hardware, &runner_id)
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
        let read = fs::read_dir(dir).map_err(|e| format!("reading corpus dir {}: {e}", dir.display()))?;
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
            let m = load_manifest(&path)
                .map_err(|e| format!("loading {}: {e}", path.display()))?;
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
) -> Result<Vec<IterationMeasurement>, String> {
    // Warmup runs: discard times, but they DO get the `prepare` step so
    // each iteration starts from a clean tree.
    for _ in 0..warmup {
        if let Some(prepare) = &baseline.prepare {
            run_argv(cwd, prepare, /*measured=*/ false)
                .map_err(|e| format!("warmup prepare failed: {e}"))?;
        }
        let _ = run_argv(cwd, &baseline.command, /*measured=*/ false)?;
    }
    // Measured runs.
    let mut iters = Vec::with_capacity(iterations as usize);
    for i in 0..iterations {
        if let Some(prepare) = &baseline.prepare {
            run_argv(cwd, prepare, /*measured=*/ false)
                .map_err(|e| format!("iteration {i} prepare failed: {e}"))?;
        }
        let start = Instant::now();
        let exit = run_argv(cwd, &baseline.command, /*measured=*/ true)?;
        let wall_ms = start.elapsed().as_millis() as u64;
        iters.push(IterationMeasurement {
            iteration: i,
            wall_ms,
            cpu_user_ms: None,
            cpu_sys_ms: None,
            peak_rss_kb: None,
            network_bytes: None,
            disk_read_bytes: None,
            disk_write_bytes: None,
            exit_code: exit,
        });
    }
    Ok(iters)
}

/// Run an argv-split command in `cwd`. Returns the exit code; an exit
/// other than `0` is an error during warmup (we abort) but allowed
/// during measurement (recorded on the iteration so the dashboard can
/// flag failed runs).
fn run_argv(cwd: &Path, cmdline: &str, measured: bool) -> Result<i32, String> {
    let argv = shell_split(cmdline);
    if argv.is_empty() {
        return Err(format!("empty command: {cmdline:?}"));
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(cwd);
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
    metadata.insert("baseline_display_name".to_string(), baseline.display_name.clone());
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
    let n = iters.len() as f64;
    let walls: Vec<f64> = iters.iter().map(|i| i.wall_ms as f64).collect();
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
        let cpu = read_first_line("/proc/cpuinfo", "model name")
            .unwrap_or_else(|| "unknown".into());
        let cores_logical = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(0);
        let memory_gb = read_first_line("/proc/meminfo", "MemTotal:")
            .and_then(|s| {
                s.split_whitespace()
                    .nth(0)
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
    command_output(&[
        "sysctl",
        "-n",
        key.as_ref().to_str()?,
    ])
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
            network_bytes: None,
            disk_read_bytes: None,
            disk_write_bytes: None,
            exit_code: 0,
        }
    }
}
