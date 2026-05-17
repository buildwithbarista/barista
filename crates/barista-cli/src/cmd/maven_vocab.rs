//! Maven-vocabulary command routing.
//!
//! `barista clean | compile | test | package | verify | install |
//! deploy | site` are drop-in synonyms for the equivalent `mvn`
//! lifecycle phases. This milestone of the project doesn't yet
//! execute the lifecycle — that lands when the barback daemon
//! comes online in a subsequent milestone. Until then, these
//! commands surface a structured "not yet executable" error that
//! names the requested phase, shows the pass-through args, and
//! points the user at the `mvn` fallback.
//!
//! The error is intentionally chatty rather than terse: this is
//! the surface a user hits first if they install barista before
//! the daemon path ships, and the message has to leave no doubt
//! about what works today and what doesn't.

use crate::cli::{GlobalFlags, MavenVocabArgs};
use crate::project::{ResolveInputs, resolve_project_root};

/// Known Maven lifecycle phases that barista exposes as
/// top-level subcommands.
///
/// The set is closed — every variant has a 1:1 mapping onto the
/// corresponding `mvn <phase>` invocation. Unknown phases stay
/// reachable via the `mvn` fallback explicitly named in the error
/// message; barista doesn't try to grow this list past Maven's
/// built-in lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MavenPhase {
    /// `mvn clean` — delete the `target/` directory.
    Clean,
    /// `mvn compile` — compile main sources.
    Compile,
    /// `mvn test` — run unit tests.
    Test,
    /// `mvn package` — assemble the project artifact.
    Package,
    /// `mvn verify` — run integration tests + verification.
    Verify,
    /// `mvn install` — install the artifact into `~/.m2/repository`.
    Install,
    /// `mvn deploy` — publish the artifact to a remote repository.
    Deploy,
    /// `mvn site` — generate the project site.
    Site,
}

impl MavenPhase {
    /// The phase name as written on the `mvn` command line.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Compile => "compile",
            Self::Test => "test",
            Self::Package => "package",
            Self::Verify => "verify",
            Self::Install => "install",
            Self::Deploy => "deploy",
            Self::Site => "site",
        }
    }

    /// Parse a phase name. Recognises every variant emitted by
    /// [`MavenPhase::as_str`]. Used by `barista shot <expr>` (M4.3 T3)
    /// to route an arbitrary phase expression through the same
    /// lifecycle-dispatch machinery the named subcommands use.
    pub fn from_phase_name(s: &str) -> Option<Self> {
        match s {
            "clean" => Some(Self::Clean),
            "compile" => Some(Self::Compile),
            "test" => Some(Self::Test),
            "package" => Some(Self::Package),
            "verify" => Some(Self::Verify),
            "install" => Some(Self::Install),
            "deploy" => Some(Self::Deploy),
            "site" => Some(Self::Site),
            _ => None,
        }
    }
}

/// Dispatch entry-point for every Maven-vocabulary command.
///
/// Until the barback daemon lands, every phase short-circuits
/// here, prints a structured error, and returns exit code `2`.
/// The error captures three facts:
///
/// 1. **Where** — the resolved project root (or a hint that no
///    `pom.xml` was found from this CWD).
/// 2. **What** — the phase the user asked for and the args they
///    passed along with it.
/// 3. **How to recover** — the `mvn <phase>` fallback plus two
///    barista-native inspection commands that *do* work today.
///
/// The output goes to stderr so a user who pipes barista into
/// another process still sees the explanation.
pub fn run(global: &GlobalFlags, phase: MavenPhase, args: &MavenVocabArgs) -> i32 {
    // R2 mitigation: `--no-daemon` short-circuits to a forked
    // upstream `mvn` invocation. Always honoured first regardless of
    // which path the daemon-backed branch ultimately picks.
    if global.no_daemon {
        return crate::cmd::no_daemon::dispatch(global, phase, args);
    }
    // M4.3 T2: all eight Maven-vocab commands route through the
    // shared lifecycle dispatcher. The dispatcher's
    // {@code dispatch_lifecycle} entry point builds the phase prefix
    // (clean | compile | test | package | verify | install | deploy
    // | site), discovers / spawns the daemon, and submits each action
    // through the M4.2 T6 auto-respawn driver. Windows (no production
    // daemon yet) falls back to the historical "not yet executable"
    // stub below.
    #[cfg(unix)]
    {
        crate::cmd::verify::run_phase(global, phase, args)
    }
    #[cfg(not(unix))]
    {
        eprint!("{}", render(global, phase, args));
        2
    }
}

/// Render the structured error message as a single string.
///
/// Split out from [`run`] so tests can assert on the rendered
/// output (and so insta snapshots don't need to capture stderr
/// from a spawned subprocess).
pub fn render(global: &GlobalFlags, phase: MavenPhase, args: &MavenVocabArgs) -> String {
    let phase_name = phase.as_str();

    // Light project-setup: confirm we have a project root so the
    // error message can name it. If the user's CWD isn't a
    // project, surface that distinctly — it's a *different*
    // failure mode than "lifecycle not wired yet" and we don't
    // want them conflated.
    let root = match resolve_project_root(ResolveInputs {
        root: global.root.clone(),
        file: global.file.clone(),
        ..Default::default()
    }) {
        Ok(r) => Some(r.root.display().to_string()),
        Err(_) => None,
    };

    let project_line = match &root {
        Some(r) => format!("project: {r}\n"),
        None => "project: (no pom.xml found; see --help)\n".to_string(),
    };

    let args_suffix = if args.args.is_empty() {
        String::new()
    } else {
        format!(" {}", args.args.join(" "))
    };

    format!(
        "barista: `{phase_name}` is not yet executable.\n\
         \n\
         {project_line}\
         phase:   {phase_name}\n\
         args:    {args_line}\n\
         \n\
         The Maven-vocabulary lifecycle phases (clean / compile / test /\n\
         package / verify / install / deploy / site) are drop-in synonyms\n\
         for `mvn <phase>`. Execution wires through the barback daemon,\n\
         which lands in a subsequent milestone.\n\
         \n\
         For now you can:\n  \
           - Run `mvn {phase_name}{args_suffix}` in this project. Output\n    \
             will be identical until barista's barback path differs.\n  \
           - Run `barista pull --no-fetch` to inspect what barista would\n    \
             resolve before lifecycle execution.\n  \
           - Run `barista grind tree` to view the resolved dependency\n    \
             graph as recorded in `barista.lock`.\n",
        args_line = args_summary(&args.args),
    )
}

/// Format the user's pass-through args for the `args:` line of
/// the error. Empty list becomes `(none)`; otherwise each arg is
/// backtick-quoted to make whitespace and stray flags visible.
fn args_summary(args: &[String]) -> String {
    if args.is_empty() {
        "(none)".to_string()
    } else {
        args.iter()
            .map(|a| format!("`{a}`"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_as_str_round_trips() {
        // Belt-and-suspenders: makes sure no two variants
        // collide on the same wire string and that the set
        // exactly matches what the CLI parser exposes.
        let all = [
            (MavenPhase::Clean, "clean"),
            (MavenPhase::Compile, "compile"),
            (MavenPhase::Test, "test"),
            (MavenPhase::Package, "package"),
            (MavenPhase::Verify, "verify"),
            (MavenPhase::Install, "install"),
            (MavenPhase::Deploy, "deploy"),
            (MavenPhase::Site, "site"),
        ];
        for (variant, s) in all {
            assert_eq!(variant.as_str(), s);
        }
    }

    #[test]
    fn args_summary_empty_is_none_marker() {
        assert_eq!(args_summary(&[]), "(none)");
    }

    #[test]
    fn args_summary_backticks_each_arg() {
        let v = vec!["-DskipTests".to_string(), "-Dx=1".to_string()];
        assert_eq!(args_summary(&v), "`-DskipTests` `-Dx=1`");
    }
}
