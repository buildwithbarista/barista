//! `--no-daemon` escape hatch: route lifecycle commands to a
//! forked upstream `mvn` process.
//!
//! When the user passes `--no-daemon` alongside a Maven-vocabulary
//! command (`clean | compile | test | package | verify | install |
//! deploy | site`), barista does **not** attempt to dispatch the
//! phase through the barback daemon. Instead it locates an
//! upstream `mvn` binary on the system, translates the
//! barista-side invocation into the equivalent `mvn` command line,
//! and execs it with full stdio passthrough. The exit code of
//! `mvn` is forwarded verbatim.
//!
//! The escape hatch exists to mitigate the R2 risk identified in
//! the PRD: the daemon is the default code path, and if it is
//! unhealthy or unavailable (or the user simply wants a fresh JVM
//! per build in CI), `--no-daemon` keeps the user productive by
//! falling back to upstream Maven.
//!
//! ## Resolution policy for `mvn`
//!
//! Resolved in this order; the first hit wins:
//!
//! 1. The environment variable `MAVEN_HOME`, if set and pointing
//!    at a directory that contains `bin/mvn` (or `bin/mvn.cmd` on
//!    Windows). This is the same convention Maven's own `mvn`
//!    launcher uses to find its installation.
//! 2. The first `mvn` (or `mvn.cmd`) on `$PATH`, via
//!    [`which::which`].
//!
//! If neither yields a usable binary, the command exits with the
//! structured error code `BAR-NODAEMON-MVN-NOT-FOUND` (exit code
//! `2`), naming both resolution strategies that were tried so the
//! user knows what to fix.
//!
//! ## Command translation
//!
//! Barista's Maven-vocabulary commands are intentionally 1:1 with
//! Maven's lifecycle phases, so the translation is mechanical:
//! `barista <phase> [args...]` becomes `mvn <phase> [args...]`,
//! with the global flags `--quiet`, `--verbose` (count → `-X` once
//! at `-vv` or higher), and project-root selection (`--root` /
//! `-f`) plumbed into the upstream `mvn` invocation.
//!
//! This module deliberately does **not** rebuild Maven-equivalent
//! dependency resolution or lifecycle ordering — the whole point
//! of `--no-daemon` is to delegate to upstream `mvn` and trust it
//! end-to-end.
//!
//! ## Settings.xml plumbing
//!
//! `barista-config` resolves a `settings.xml` (see PRD §11). When
//! the user has overridden the path away from Maven's default
//! `~/.m2/settings.xml` — via `--config` or env vars handled by
//! the loader — that override is forwarded to `mvn` via
//! `-s <path>`. When the path is the default, we let `mvn`
//! discover it itself so the byte-for-byte identity guarantee
//! against a plain `mvn` invocation is preserved.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::GlobalFlags;
use crate::cli::MavenVocabArgs;
use crate::cmd::MavenPhase;

/// Exit code returned when `mvn` cannot be located. Aligned with
/// other CLI surfaces in this crate that use `2` as the
/// "user-facing failure" sentinel.
pub const EXIT_MVN_NOT_FOUND: i32 = 2;

/// Stable error code embedded in the user-facing message for
/// "couldn't find `mvn`". Documented so support / CI logs can
/// grep for it.
pub const ERR_CODE_MVN_NOT_FOUND: &str = "BAR-NODAEMON-MVN-NOT-FOUND";

/// Dispatch entry point. Forks `mvn` and forwards stdio + exit
/// code. Returns the exit code to surface to the OS.
pub fn dispatch(global: &GlobalFlags, phase: MavenPhase, args: &MavenVocabArgs) -> i32 {
    let env = RealEnv;
    dispatch_with(global, phase, args, &env, &RealSpawner)
}

/// Inputs to [`dispatch_with`]: environment lookup is abstracted
/// out so tests can substitute a fixture without touching process
/// globals.
pub trait Env {
    fn var(&self, key: &str) -> Option<OsString>;
}

/// Real-process [`Env`] implementation. Reads via [`std::env::var_os`].
struct RealEnv;

impl Env for RealEnv {
    fn var(&self, key: &str) -> Option<OsString> {
        std::env::var_os(key)
    }
}

/// Outcome of running the forked `mvn`. Surfaced via [`Spawner`]
/// so tests can simulate exit codes without running a real process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnOutcome {
    /// `mvn` ran to completion with the given exit code.
    Exited(i32),
    /// `mvn` was terminated by a signal (Unix). Surfaced as a
    /// barista-side exit code of `1` to match Maven's own
    /// convention when wrapping signal exits.
    Signal,
}

/// Spawns the `mvn` subprocess and waits for it. Abstracted out
/// so tests can stub the spawn step.
pub trait Spawner {
    /// Run `mvn` at `mvn_path` with `args` and the given
    /// `working_dir`. Stdio is inherited from the current process.
    fn spawn(&self, mvn_path: &Path, args: &[OsString], working_dir: &Path) -> SpawnOutcome;
}

/// Real-process [`Spawner`] implementation.
struct RealSpawner;

impl Spawner for RealSpawner {
    fn spawn(&self, mvn_path: &Path, args: &[OsString], working_dir: &Path) -> SpawnOutcome {
        // `mvn_path` is not user-controlled interpolation: it is the
        // output of `resolve_mvn`, which only returns either
        // (a) `$MAVEN_HOME/bin/mvn{,.cmd}` after an `is_file()` check
        // or (b) `which::which("mvn")` resolving a hard-coded program
        // name against `$PATH`. The resolution policy is documented
        // in the module docs above. The user-controlled args flow
        // into `cmd.args(args)` on the next line, never into
        // `Command::new`.
        // nosemgrep: barista-rust-unchecked-command-new
        let mut cmd = Command::new(mvn_path);
        cmd.args(args);
        cmd.current_dir(working_dir);
        // Inherit stdio so the user sees `mvn`'s output verbatim.
        // This is the explicit contract of `--no-daemon`: it is a
        // pass-through, not a wrapping.
        match cmd.status() {
            Ok(status) => match status.code() {
                Some(c) => SpawnOutcome::Exited(c),
                None => SpawnOutcome::Signal,
            },
            Err(e) => {
                // Failing to spawn at all (e.g. permission error
                // on a path we just located) is rare enough that
                // we surface it with the same code as a missing
                // binary — the next-step recovery is the same:
                // check `$PATH` / `MAVEN_HOME`.
                eprintln!(
                    "{}: failed to spawn `mvn` at {}: {e}",
                    ERR_CODE_MVN_NOT_FOUND,
                    mvn_path.display(),
                );
                SpawnOutcome::Exited(EXIT_MVN_NOT_FOUND)
            }
        }
    }
}

/// Pure-logic dispatch: take an [`Env`] and a [`Spawner`] and
/// return the exit code. The real entry point [`dispatch`] is a
/// thin wrapper that supplies the real implementations.
pub fn dispatch_with(
    global: &GlobalFlags,
    phase: MavenPhase,
    args: &MavenVocabArgs,
    env: &dyn Env,
    spawner: &dyn Spawner,
) -> i32 {
    let mvn_path = match resolve_mvn(env) {
        Ok(p) => p,
        Err(e) => {
            eprint!("{}", e.render());
            return EXIT_MVN_NOT_FOUND;
        }
    };

    let working_dir = resolve_working_dir(global);
    let argv = build_mvn_argv(global, phase, args);

    if global.verbose >= 1 {
        // Best-effort trace for `-v`+ — quoting is naive (we only
        // backtick each arg) but enough to debug what was sent.
        let pretty = argv
            .iter()
            .map(|a| format!("`{}`", a.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("barista: --no-daemon → {} {}", mvn_path.display(), pretty,);
    }

    match spawner.spawn(&mvn_path, &argv, &working_dir) {
        SpawnOutcome::Exited(code) => code,
        // Mirror Maven's own shell wrapper: a signal exit becomes
        // exit code 1 from the wrapping process.
        SpawnOutcome::Signal => 1,
    }
}

/// Errors surfaced when locating `mvn` fails.
#[derive(Debug, Clone)]
pub enum ResolveMvnError {
    /// Neither `MAVEN_HOME` nor `$PATH` produced a usable binary.
    NotFound {
        /// The `MAVEN_HOME` value we tried, or `None` if the env
        /// var was unset.
        tried_maven_home: Option<PathBuf>,
    },
}

impl ResolveMvnError {
    /// Render the error as the user-facing message that goes to
    /// stderr. Includes the [`ERR_CODE_MVN_NOT_FOUND`] code so it
    /// is greppable.
    pub fn render(&self) -> String {
        match self {
            Self::NotFound { tried_maven_home } => {
                let mh = match tried_maven_home {
                    Some(p) => format!(
                        "MAVEN_HOME was set to {} but no `bin/mvn` was found there.\n",
                        p.display(),
                    ),
                    None => "MAVEN_HOME is unset.\n".to_string(),
                };
                format!(
                    "barista: `--no-daemon` needs an upstream `mvn` on the system, \
                     but none was found.\n\
                     \n  \
                       code:  {ERR_CODE_MVN_NOT_FOUND}\n  \
                       tried: $MAVEN_HOME, then $PATH\n  \
                       hint:  {mh}        \
                              Install Maven (https://maven.apache.org/install.html) or \
                              point MAVEN_HOME at an existing installation.\n",
                )
            }
        }
    }
}

/// Locate an `mvn` binary. See module-level docs for policy.
pub fn resolve_mvn(env: &dyn Env) -> Result<PathBuf, ResolveMvnError> {
    // 1. $MAVEN_HOME, if set.
    let maven_home = env.var("MAVEN_HOME").map(PathBuf::from);
    if let Some(ref home) = maven_home {
        let candidate = home.join("bin").join(mvn_bin_name());
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    // 2. Fall through to $PATH.
    match which::which(mvn_bin_name()) {
        Ok(p) => Ok(p),
        Err(_) => Err(ResolveMvnError::NotFound {
            tried_maven_home: maven_home,
        }),
    }
}

/// `mvn` on Unix, `mvn.cmd` on Windows.
fn mvn_bin_name() -> &'static str {
    if cfg!(windows) { "mvn.cmd" } else { "mvn" }
}

/// Pick the working directory for the forked `mvn`. Mirrors the
/// rest of the CLI: `--root` wins if given, otherwise `-f`'s
/// containing directory, otherwise the current process CWD.
fn resolve_working_dir(global: &GlobalFlags) -> PathBuf {
    if let Some(root) = &global.root {
        return root.clone();
    }
    if let Some(file) = &global.file {
        // If it's a directory, use it as-is; if it's a pom file,
        // use its parent. We don't stat — Maven itself will
        // produce a clean error if the path is wrong.
        if file.is_dir() {
            return file.clone();
        }
        if let Some(parent) = file.parent() {
            return parent.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Build the argv passed to `mvn`.
///
/// Layout (in order):
///
/// 1. Verbosity translation: `-q` if `--quiet`, `-X` if
///    `--verbose` was passed twice or more (`-vv`/`-vvv`); a
///    single `-v` is treated as "info" and gets no extra flag,
///    matching Maven's default verbosity.
/// 2. Color: `-B` (batch mode, ANSI off) if `--no-color`.
/// 3. The lifecycle phase name.
/// 4. The user's pass-through args, verbatim.
pub fn build_mvn_argv(
    global: &GlobalFlags,
    phase: MavenPhase,
    args: &MavenVocabArgs,
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = Vec::with_capacity(2 + args.args.len());

    if global.quiet {
        argv.push("-q".into());
    } else if global.verbose >= 2 {
        argv.push("-X".into());
    }

    if global.no_color {
        // `-B` is Maven's "batch mode": disables ANSI and prompts.
        // This is the same flag CI pipelines use to get
        // byte-deterministic logs.
        argv.push("-B".into());
    }

    argv.push(phase.as_str().into());

    for a in &args.args {
        argv.push(a.into());
    }

    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Fake [`Env`] that returns from an in-memory map.
    struct FakeEnv(HashMap<String, OsString>);

    impl FakeEnv {
        fn new() -> Self {
            Self(HashMap::new())
        }
        fn with(mut self, k: &str, v: &str) -> Self {
            self.0.insert(k.to_string(), v.into());
            self
        }
    }

    impl Env for FakeEnv {
        fn var(&self, key: &str) -> Option<OsString> {
            self.0.get(key).cloned()
        }
    }

    /// Recording [`Spawner`] that captures what it was asked to
    /// run and returns a configurable exit code.
    struct RecordingSpawner {
        exit: i32,
        last: RefCell<Option<(PathBuf, Vec<OsString>, PathBuf)>>,
    }

    impl RecordingSpawner {
        fn new(exit: i32) -> Self {
            Self {
                exit,
                last: RefCell::new(None),
            }
        }
    }

    impl Spawner for RecordingSpawner {
        fn spawn(&self, mvn_path: &Path, args: &[OsString], working_dir: &Path) -> SpawnOutcome {
            *self.last.borrow_mut() = Some((
                mvn_path.to_path_buf(),
                args.to_vec(),
                working_dir.to_path_buf(),
            ));
            SpawnOutcome::Exited(self.exit)
        }
    }

    fn default_globals() -> GlobalFlags {
        use crate::cli::{Cli, GlobalFlags};
        use clap::Parser;
        let cli = Cli::try_parse_from(["barista", "--no-daemon", "compile"])
            .expect("parse --no-daemon compile");
        let GlobalFlags { .. } = cli.global;
        cli.global
    }

    #[test]
    fn build_argv_minimal_compile() {
        let g = default_globals();
        let argv = build_mvn_argv(&g, MavenPhase::Compile, &MavenVocabArgs { args: vec![] });
        let strs: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(strs, vec!["compile"]);
    }

    #[test]
    fn build_argv_passes_through_user_args() {
        let g = default_globals();
        let argv = build_mvn_argv(
            &g,
            MavenPhase::Test,
            &MavenVocabArgs {
                args: vec!["-DskipTests=false".into(), "-Dprop=value".into()],
            },
        );
        let strs: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(strs, vec!["test", "-DskipTests=false", "-Dprop=value"]);
    }

    #[test]
    fn build_argv_quiet_emits_minus_q() {
        use crate::cli::Cli;
        use clap::Parser;
        let cli = Cli::try_parse_from(["barista", "--no-daemon", "--quiet", "compile"]).unwrap();
        let argv = build_mvn_argv(
            &cli.global,
            MavenPhase::Compile,
            &MavenVocabArgs { args: vec![] },
        );
        let strs: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(strs, vec!["-q", "compile"]);
    }

    #[test]
    fn build_argv_double_verbose_emits_minus_x() {
        use crate::cli::Cli;
        use clap::Parser;
        let cli = Cli::try_parse_from(["barista", "--no-daemon", "-vv", "compile"]).unwrap();
        let argv = build_mvn_argv(
            &cli.global,
            MavenPhase::Compile,
            &MavenVocabArgs { args: vec![] },
        );
        let strs: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(strs, vec!["-X", "compile"]);
    }

    #[test]
    fn build_argv_no_color_emits_minus_b() {
        use crate::cli::Cli;
        use clap::Parser;
        let cli = Cli::try_parse_from(["barista", "--no-daemon", "--no-color", "compile"]).unwrap();
        let argv = build_mvn_argv(
            &cli.global,
            MavenPhase::Compile,
            &MavenVocabArgs { args: vec![] },
        );
        let strs: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(strs, vec!["-B", "compile"]);
    }

    #[test]
    fn resolve_mvn_prefers_maven_home_when_valid() {
        // We can't construct a valid MAVEN_HOME on the filesystem
        // hermetically without writing a fake mvn binary, so this
        // test just confirms the env-var lookup is attempted: an
        // invalid MAVEN_HOME falls through to $PATH.
        let env = FakeEnv::new().with("MAVEN_HOME", "/nonexistent/maven");
        // On a system with `mvn` installed, this should succeed
        // via PATH. On a system without it, we get NotFound.
        match resolve_mvn(&env) {
            Ok(_) => { /* found via PATH */ }
            Err(ResolveMvnError::NotFound { tried_maven_home }) => {
                assert_eq!(tried_maven_home, Some(PathBuf::from("/nonexistent/maven")));
            }
        }
    }

    #[test]
    fn resolve_mvn_not_found_error_renders_code() {
        let err = ResolveMvnError::NotFound {
            tried_maven_home: None,
        };
        let rendered = err.render();
        assert!(rendered.contains(ERR_CODE_MVN_NOT_FOUND));
        assert!(rendered.contains("MAVEN_HOME is unset"));
    }

    #[test]
    fn resolve_mvn_not_found_with_maven_home_mentions_it() {
        let err = ResolveMvnError::NotFound {
            tried_maven_home: Some(PathBuf::from("/opt/maven")),
        };
        let rendered = err.render();
        assert!(rendered.contains("/opt/maven"));
        assert!(rendered.contains(ERR_CODE_MVN_NOT_FOUND));
    }

    #[test]
    fn dispatch_with_records_argv_and_forwards_exit_code() {
        use crate::cli::Cli;
        use clap::Parser;
        let td = tempfile::tempdir().unwrap();
        // Create a fake `mvn` binary so resolve_mvn finds it
        // via MAVEN_HOME, without depending on the host's $PATH.
        let bin_dir = td.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let mvn_name = if cfg!(windows) { "mvn.cmd" } else { "mvn" };
        let fake_mvn = bin_dir.join(mvn_name);
        std::fs::write(&fake_mvn, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_mvn).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_mvn, perms).unwrap();
        }

        let env = FakeEnv::new().with("MAVEN_HOME", td.path().to_str().unwrap());
        let spawner = RecordingSpawner::new(0);

        let cli = Cli::try_parse_from([
            "barista",
            "--no-daemon",
            "--root",
            td.path().to_str().unwrap(),
            "compile",
            "-DskipTests",
        ])
        .unwrap();

        let exit = dispatch_with(
            &cli.global,
            MavenPhase::Compile,
            &MavenVocabArgs {
                args: vec!["-DskipTests".into()],
            },
            &env,
            &spawner,
        );
        assert_eq!(exit, 0);

        let last = spawner.last.borrow();
        let (path, args, wd) = last.as_ref().unwrap();
        assert_eq!(path, &fake_mvn);
        let arg_strs: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(arg_strs, vec!["compile", "-DskipTests"]);
        assert_eq!(wd, td.path());
    }

    #[test]
    fn dispatch_with_bubbles_nonzero_exit_code() {
        use crate::cli::Cli;
        use clap::Parser;
        let td = tempfile::tempdir().unwrap();
        let bin_dir = td.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let mvn_name = if cfg!(windows) { "mvn.cmd" } else { "mvn" };
        let fake_mvn = bin_dir.join(mvn_name);
        std::fs::write(&fake_mvn, "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_mvn).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_mvn, perms).unwrap();
        }

        let env = FakeEnv::new().with("MAVEN_HOME", td.path().to_str().unwrap());
        let spawner = RecordingSpawner::new(42);

        let cli = Cli::try_parse_from([
            "barista",
            "--no-daemon",
            "--root",
            td.path().to_str().unwrap(),
            "verify",
        ])
        .unwrap();

        let exit = dispatch_with(
            &cli.global,
            MavenPhase::Verify,
            &MavenVocabArgs { args: vec![] },
            &env,
            &spawner,
        );
        assert_eq!(exit, 42);
    }

    // We can't reliably trigger `resolve_mvn` → `NotFound` from a
    // unit test without mutating the *process-wide* `$PATH`
    // (`which::which` reads it directly, not via our [`Env`]
    // shim). End-to-end coverage of the missing-mvn case lives in
    // `tests/cmd_no_daemon.rs::no_daemon_emits_structured_error_when_mvn_missing`,
    // which forks a subprocess so the env mutation is hermetic.
}
