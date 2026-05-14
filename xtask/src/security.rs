//! `cargo xtask security` — orchestrates the full locally-runnable
//! security suite for the Barista workspace.
//!
//! # Layers
//!
//! The suite is intentionally redundant with what CI runs: a contributor
//! who runs `cargo xtask security` before pushing should see the same
//! diagnostics CI would surface (modulo the heavy SBOM / Java-side
//! checks that don't make sense on a developer laptop).
//!
//! | Layer    | Command                                                       | Required |
//! |----------|---------------------------------------------------------------|----------|
//! | clippy   | `cargo clippy --workspace --all-targets -- -D warnings`       | yes      |
//! | deny     | `cargo deny check`                                            | optional |
//! | audit    | `cargo audit`                                                 | optional |
//! | semgrep  | `semgrep --config .semgrep/ --error`                          | optional |
//! | gitleaks | `gitleaks detect --no-git --redact`                           | optional |
//!
//! `--strict` promotes a missing optional tool to a failure (intended for
//! CI where every tool should be present and a missing one is a config
//! bug, not a developer-laptop reality).
//!
//! # Exit code policy
//!
//! - `0` if every required check passed *and* every available optional
//!   check passed.
//! - `1` if any required check failed *or* any available optional check
//!   failed *or* (under `--strict`) any optional check was skipped for
//!   being uninstalled.
//!
//! A missing optional tool in non-strict mode prints a `note:` line to
//! stderr and does not affect the exit code.

use std::process::{Command, ExitStatus};

use clap::Args as ClapArgs;

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// CLI flags for `cargo xtask security`.
#[derive(ClapArgs, Debug, Clone)]
pub struct SecurityArgs {
    /// Promote missing optional tools to failures.
    ///
    /// In `--strict` mode, every check in the suite must run (i.e. every
    /// tool must be installed); skipping for "not installed" becomes a
    /// failure. CI invokes the suite with `--strict` so the CI image's
    /// scanner inventory is verified by-build, not by-trust.
    #[arg(long)]
    pub strict: bool,

    /// Run only the named check. Repeatable. If omitted, every check
    /// runs in the canonical order.
    ///
    /// Valid names: `clippy`, `deny`, `audit`, `semgrep`, `gitleaks`.
    #[arg(long = "check", value_name = "NAME")]
    pub check: Vec<String>,
}

// Re-export under the name `Args` so `main.rs`'s `Command::Security(security::Args)`
// reads cleanly even though clap requires the type to be named
// `*Args` (or `*Opts`, etc.) by convention.
pub type Args = SecurityArgs;

/// Entry point invoked by `main.rs`. Returns a process exit code.
pub fn run(args: Args) -> i32 {
    let env = RealEnv;
    run_with_env(&args, &env)
}

// ---------------------------------------------------------------------------
// Check registry
// ---------------------------------------------------------------------------

/// A single check in the suite. `Required` checks fail the run if they
/// fail or if their tool is missing (no graceful skip). `Optional`
/// checks fail the run if they fail when present, but skip with a hint
/// when their tool is missing (unless `--strict` is set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Requirement {
    Required,
    Optional,
}

/// Static description of a check. The actual command-line invocation
/// is built lazily in [`Check::command`] so we don't pay for argument
/// allocation on checks we end up skipping.
struct Check {
    /// Stable lower-kebab name used by `--check NAME`.
    name: &'static str,
    /// Human-readable label shown in the summary line.
    label: &'static str,
    /// Executable to invoke. For `cargo` subcommands this is `"cargo"`
    /// and the subcommand goes in `args`.
    program: &'static str,
    /// Argument list passed to `program`.
    args: &'static [&'static str],
    /// Required-vs-optional policy.
    requirement: Requirement,
    /// Shell-flavored install hint shown when an optional tool is
    /// missing. Empty for `Required` checks (they don't get a graceful
    /// skip).
    install_hint: &'static str,
}

/// The canonical check list, in the order they run.
///
/// Order is deliberate, not alphabetical:
///   1. clippy first — fastest, catches the most common contributor
///      misses, and forces a workspace build so subsequent checks
///      run against fresh artifacts.
///   2. deny + audit — supply-chain checks; cheap, source-only.
///   3. semgrep — SAST; slower than the cargo-side checks.
///   4. gitleaks — secret scan over the working tree; runs last so
///      a leaked secret surfaces clearly at the end of the output.
const CHECKS: &[Check] = &[
    Check {
        name: "clippy",
        label: "cargo clippy",
        program: "cargo",
        args: &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"],
        requirement: Requirement::Required,
        install_hint: "",
    },
    Check {
        name: "deny",
        label: "cargo deny check",
        program: "cargo-deny",
        args: &["deny", "check"],
        requirement: Requirement::Optional,
        install_hint: "cargo install cargo-deny --locked",
    },
    Check {
        name: "audit",
        label: "cargo audit",
        program: "cargo-audit",
        args: &["audit"],
        requirement: Requirement::Optional,
        install_hint: "cargo install cargo-audit --locked",
    },
    Check {
        // The unsafe-line ratchet introduced by M C.3 Task 2. Compares
        // the current `unsafe`-construct count per crate against the
        // baseline checked in at `docs/ci/geiger-baseline.json`. Fails
        // when a crate grows past its allowance — the workspace-wide
        // policy is "no new unsafe outside the cache CAS layer," with
        // `barista-cache` and `barista-pom` carrying the only sanctioned
        // baselines today. Treated as Required because the script ships
        // in the repo (no external install) and the policy is the same
        // gate CI enforces in `sast.yml`'s `unsafe-ratchet` job.
        name: "unsafe",
        label: "unsafe-line ratchet",
        program: "python3",
        args: &["scripts/count-unsafe.py", "--check"],
        requirement: Requirement::Required,
        install_hint: "",
    },
    Check {
        name: "semgrep",
        label: "semgrep (.semgrep/)",
        program: "semgrep",
        // `--error`  exit non-zero on a finding (default is to print
        //            and return 0).
        // `--quiet`  suppress the progress banner so clean runs are
        //            silent.
        //
        // We scan with only the project-local `.semgrep/` rule pack
        // here (not the registry `r/rust` pack); the heavier registry
        // packs run in CI's `sast.yml`. The SAST round-trip job in
        // that workflow validates that the custom rules still fire
        // on `tests/fixtures/sast/`; we mirror the workspace-wide
        // invocation here so a contributor sees the same findings CI
        // would. Honors `.semgrepignore` for any deliberate exclusions.
        // `--exclude` skips the SAST fixture directory. The fixtures
        //   under `tests/fixtures/sast/` are deliberate violations
        //   that the custom rules in `.semgrep/` are designed to fire
        //   on — they're the inputs to the `semgrep-fixture-round-trip`
        //   job in CI (which targets that directory explicitly to
        //   prove the rules still catch them). Excluding them here
        //   keeps `cargo xtask security` green on a clean tree while
        //   the round-trip job continues to validate the rule pack.
        args: &[
            "--config",
            ".semgrep/",
            "--error",
            "--quiet",
            "--exclude",
            "tests/fixtures/sast/",
        ],
        requirement: Requirement::Optional,
        install_hint: "brew install semgrep   # or: pipx install semgrep",
    },
    Check {
        name: "gitleaks",
        label: "gitleaks detect",
        program: "gitleaks",
        // `--no-git`  scan the working tree (not history) so the xtask
        //             matches what the pre-commit hook covers on a
        //             developer's machine.
        // `--redact`  keep any accidental hit out of the terminal
        //             scrollback.
        //
        // Honors `.gitleaks.toml` (config-level allowlists) and
        // `.gitleaksignore` (per-finding fingerprint waivers). A
        // contributor who adds an intentional test fixture is
        // expected to follow the `.gitleaksignore` workflow described
        // in CONTRIBUTING.md so the suite stays green.
        args: &["detect", "--no-git", "--redact"],
        requirement: Requirement::Optional,
        install_hint: "brew install gitleaks  # or download from gitleaks releases",
    },
];

impl Check {
    /// Construct a runnable `std::process::Command` for this check.
    ///
    /// `cargo-deny` and `cargo-audit` ship as separate binaries on PATH;
    /// they're also invokable as `cargo deny` / `cargo audit`. We
    /// invoke the binaries directly so the "tool present?" probe and
    /// the actual invocation use the same name.
    fn command(&self) -> Command {
        let mut cmd = Command::new(self.program);
        cmd.args(self.args);
        cmd
    }
}

// ---------------------------------------------------------------------------
// Outcome reporting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Tool ran and exited 0.
    Passed,
    /// Tool ran and exited non-zero.
    Failed,
    /// Tool not installed (only valid for `Optional`, or `Required`
    /// when the orchestrator decides to report it rather than panic).
    SkippedMissing,
    /// User asked for a specific subset that excluded this check.
    SkippedFilter,
}

impl Outcome {
    fn glyph(self) -> &'static str {
        match self {
            Outcome::Passed => "PASS",
            Outcome::Failed => "FAIL",
            Outcome::SkippedMissing => "SKIP (not installed)",
            Outcome::SkippedFilter => "SKIP (--check filter)",
        }
    }
}

// ---------------------------------------------------------------------------
// Orchestration — testable variant taking an Env trait
// ---------------------------------------------------------------------------

/// Indirection layer for filesystem PATH lookups and process spawning.
///
/// In production [`RealEnv`] uses `which::which` + `std::process::Command`.
/// Tests use [`FakeEnv`] to assert the skip-gracefully behavior without
/// actually installing or uninstalling tools on the host.
trait Env {
    fn tool_present(&self, program: &str) -> bool;
    fn spawn(&self, command: Command) -> std::io::Result<ExitStatus>;
}

struct RealEnv;

impl Env for RealEnv {
    fn tool_present(&self, program: &str) -> bool {
        which::which(program).is_ok()
    }

    fn spawn(&self, mut command: Command) -> std::io::Result<ExitStatus> {
        command.status()
    }
}

fn run_with_env<E: Env>(args: &Args, env: &E) -> i32 {
    let filter: Option<Vec<&str>> = if args.check.is_empty() {
        None
    } else {
        Some(args.check.iter().map(String::as_str).collect())
    };

    // Validate filter names up-front — typos in `--check` would
    // otherwise silently run zero checks and exit 0.
    if let Some(ref names) = filter {
        let valid: Vec<&str> = CHECKS.iter().map(|c| c.name).collect();
        let unknown: Vec<&str> = names
            .iter()
            .copied()
            .filter(|n| !valid.contains(n))
            .collect();
        if !unknown.is_empty() {
            eprintln!(
                "xtask security: unknown --check name(s): {}\n  valid names: {}",
                unknown.join(", "),
                valid.join(", ")
            );
            return 2;
        }
    }

    let mut results: Vec<(&Check, Outcome)> = Vec::with_capacity(CHECKS.len());

    for check in CHECKS {
        // Per-check filter handling.
        if let Some(ref names) = filter {
            if !names.contains(&check.name) {
                results.push((check, Outcome::SkippedFilter));
                continue;
            }
        }

        // Tool-presence probe.
        if !env.tool_present(check.program) {
            match check.requirement {
                Requirement::Required => {
                    eprintln!(
                        "xtask security: required tool `{}` not found on PATH",
                        check.program
                    );
                    results.push((check, Outcome::Failed));
                    continue;
                }
                Requirement::Optional => {
                    if args.strict {
                        eprintln!(
                            "xtask security: --strict: optional tool `{}` not installed (hint: {})",
                            check.program, check.install_hint
                        );
                        results.push((check, Outcome::Failed));
                    } else {
                        eprintln!(
                            "note: {} not installed; install via `{}` for full coverage",
                            check.program, check.install_hint
                        );
                        results.push((check, Outcome::SkippedMissing));
                    }
                    continue;
                }
            }
        }

        // Tool is present — actually run it.
        eprintln!("\n==> {} ({})", check.label, render_command(check));
        let status = env.spawn(check.command());
        let outcome = match status {
            Ok(s) if s.success() => Outcome::Passed,
            Ok(_) => Outcome::Failed,
            Err(e) => {
                eprintln!("xtask security: failed to spawn `{}`: {}", check.program, e);
                Outcome::Failed
            }
        };
        results.push((check, outcome));
    }

    print_summary(&results);

    if results
        .iter()
        .any(|(_, o)| matches!(o, Outcome::Failed))
    {
        1
    } else {
        0
    }
}

fn render_command(check: &Check) -> String {
    let parts: Vec<&str> = std::iter::once(check.program).chain(check.args.iter().copied()).collect();
    parts.join(" ")
}

fn print_summary(results: &[(&Check, Outcome)]) {
    eprintln!("\n=== security suite summary ===");
    for (check, outcome) in results {
        eprintln!("  {:24}  {}", check.label, outcome.glyph());
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;

    /// Test double for [`Env`]. Records which tools the test claims are
    /// present and what exit status each spawned program should return.
    struct FakeEnv {
        installed: Vec<&'static str>,
        spawn_results: RefCell<std::collections::HashMap<&'static str, i32>>,
    }

    impl FakeEnv {
        fn new(installed: &[&'static str]) -> Self {
            Self {
                installed: installed.to_vec(),
                spawn_results: RefCell::new(std::collections::HashMap::new()),
            }
        }

        fn with_exit_code(self, program: &'static str, code: i32) -> Self {
            self.spawn_results.borrow_mut().insert(program, code);
            self
        }
    }

    impl Env for FakeEnv {
        fn tool_present(&self, program: &str) -> bool {
            self.installed.contains(&program)
        }

        fn spawn(&self, command: Command) -> std::io::Result<ExitStatus> {
            // `Command` doesn't expose the program name on stable Rust
            // through a public accessor on all platforms, but
            // `get_program` is stable since 1.57 and returns an OsStr.
            let program = command
                .get_program()
                .to_str()
                .expect("test programs are always UTF-8")
                .to_owned();
            let code = self
                .spawn_results
                .borrow()
                .get(program.as_str())
                .copied()
                .unwrap_or(0);
            Ok(ExitStatus::from_raw(code << 8))
        }
    }

    fn args_default() -> Args {
        Args {
            strict: false,
            check: Vec::new(),
        }
    }

    /// Clean tree, every optional tool installed, every check passes → exit 0.
    #[test]
    fn all_checks_pass_returns_zero() {
        let env = FakeEnv::new(&["cargo", "cargo-deny", "cargo-audit", "python3", "semgrep", "gitleaks"]);
        assert_eq!(run_with_env(&args_default(), &env), 0);
    }

    /// Required check fails → exit 1 even though everything else is fine.
    #[test]
    fn failing_required_check_returns_one() {
        let env = FakeEnv::new(&["cargo", "cargo-deny", "cargo-audit", "python3", "semgrep", "gitleaks"])
            .with_exit_code("cargo", 1);
        assert_eq!(run_with_env(&args_default(), &env), 1);
    }

    /// Failing optional check (tool present, exits non-zero) → exit 1.
    #[test]
    fn failing_optional_check_returns_one() {
        let env = FakeEnv::new(&["cargo", "cargo-deny", "cargo-audit", "python3", "semgrep", "gitleaks"])
            .with_exit_code("semgrep", 1);
        assert_eq!(run_with_env(&args_default(), &env), 1);
    }

    /// Optional tool missing in non-strict mode → skipped, exit 0.
    /// This is the formal `[T]` for the skip-gracefully contract.
    #[test]
    fn missing_optional_tool_skips_gracefully() {
        // Only the required tools (`cargo` for clippy + `python3` for
        // the unsafe-line ratchet) are installed. Optional tools
        // (cargo-deny, cargo-audit, semgrep, gitleaks) are absent and
        // must be skipped, not failed.
        let env = FakeEnv::new(&["cargo", "python3"]);
        let code = run_with_env(&args_default(), &env);
        assert_eq!(
            code, 0,
            "non-strict run must exit 0 when only required tools are present"
        );
    }

    /// Optional tool missing in `--strict` mode → treated as failure.
    #[test]
    fn missing_optional_tool_strict_returns_one() {
        let env = FakeEnv::new(&["cargo", "python3"]);
        let args = Args {
            strict: true,
            check: Vec::new(),
        };
        assert_eq!(run_with_env(&args, &env), 1);
    }

    /// Missing required tool → always fails (no graceful skip even
    /// without `--strict`). Cargo is the only required tool today;
    /// "required tool missing" should never happen in practice (the
    /// xtask is invoked via `cargo xtask`!), but the policy is
    /// codified anyway.
    #[test]
    fn missing_required_tool_returns_one() {
        let env = FakeEnv::new(&["cargo-deny", "cargo-audit", "semgrep", "gitleaks"]);
        assert_eq!(run_with_env(&args_default(), &env), 1);
    }

    /// `--check clippy` only runs clippy; other checks are filter-skipped.
    #[test]
    fn check_filter_runs_only_named_check() {
        let env = FakeEnv::new(&["cargo", "cargo-deny", "cargo-audit", "python3", "semgrep", "gitleaks"])
            // Force every other check to exit non-zero — they should never run.
            .with_exit_code("cargo-deny", 1)
            .with_exit_code("cargo-audit", 1)
            .with_exit_code("semgrep", 1)
            .with_exit_code("gitleaks", 1);
        let args = Args {
            strict: false,
            check: vec!["clippy".to_owned()],
        };
        assert_eq!(
            run_with_env(&args, &env),
            0,
            "only clippy should run; other failing checks must be filter-skipped"
        );
    }

    /// Unknown check name is caught up-front (exit 2, distinct from a
    /// failing check) rather than silently producing a no-op success.
    #[test]
    fn unknown_check_filter_returns_two() {
        let env = FakeEnv::new(&["cargo"]);
        let args = Args {
            strict: false,
            check: vec!["definitely-not-a-check".to_owned()],
        };
        assert_eq!(run_with_env(&args, &env), 2);
    }
}

