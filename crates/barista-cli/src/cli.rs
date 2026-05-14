//! Barista CLI surface.
//!
//! Defines every command, subcommand, global flag, and per-command flag
//! in one place using `clap` derive macros. The mapping to product
//! requirements is intentionally direct:
//!
//! - The "signature verbs" — `pull`, `grind`, `pour`, `dial-in`, `shot`,
//!   `wrapper` — are Barista-native; they're the value-add surface.
//! - The "Maven-vocabulary" commands — `clean`, `compile`, `test`,
//!   `package`, `verify`, `install`, `deploy`, `site` — make `barista`
//!   a drop-in for `mvn`. They route through the warm-JVM daemon in a
//!   future milestone; for now they return a structured "not yet
//!   executable" stub.
//!
//! Subcommand implementations live (or will live) in sibling modules
//! and plug into the router via [`dispatch`].

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// `barista` — a fast, fully Maven-compatible JVM build tool.
///
/// Drop-in for `mvn`: same lifecycle phases, same `pom.xml`, same
/// `settings.xml`, same plugins — but with a parallel resolver, a
/// content-addressed cache, and a warm-JVM daemon.
#[derive(Debug, Parser)]
#[command(
    name = "barista",
    version,
    about = "Fast, fully Maven-compatible JVM build tool.",
    long_about = None,
    propagate_version = true,
    disable_help_subcommand = true,
    arg_required_else_help = true,
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalFlags,

    #[command(subcommand)]
    pub command: Command,
}

/// Global flags accepted on every subcommand.
///
/// These mirror PRD §9. A few notes on semantics:
///
/// - `--ci` is a *macro*: it expands (in the dispatcher / config
///   resolver) to `--frozen --output json --quiet`. `--frozen` is a
///   lockfile flag and lands as a global in a later milestone.
/// - `--root` and `-f`/`--file` are alternate spellings of the same
///   idea; resolving precedence happens in T7 (project root
///   resolution).
/// - `--strict` is duplicated on per-command arg structs (e.g.
///   `PullArgs::strict`) where a per-command override is useful;
///   resolution merges the two.
#[derive(Debug, clap::Args)]
pub struct GlobalFlags {
    /// Output format. `human` (default), `json`, or `ndjson`.
    #[arg(
        long,
        value_enum,
        default_value_t = OutputFormat::Human,
        global = true,
        value_name = "FORMAT",
    )]
    pub output: OutputFormat,

    /// CI shortcut: equivalent to `--frozen --output json --quiet`.
    #[arg(long, global = true)]
    pub ci: bool,

    /// Suppress non-essential output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Increase verbosity (stackable: `-v`, `-vv`, `-vvv`).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Project root override; auto-detected from CWD if absent.
    #[arg(long, global = true, value_name = "PATH")]
    pub root: Option<PathBuf>,

    /// Pom file override (alternate spelling of `--root <pom-dir>`).
    #[arg(short = 'f', long, global = true, value_name = "POM")]
    pub file: Option<PathBuf>,

    /// Strict resolution (PubGrub): error on unresolvable conflicts
    /// instead of nearest-wins.
    #[arg(long, global = true)]
    pub strict: bool,

    /// Force one-shot execution; bypass the barback daemon.
    #[arg(long, global = true)]
    pub no_daemon: bool,

    /// Maven compatibility mode.
    #[arg(long, value_enum, global = true, value_name = "MODE")]
    pub maven_compat: Option<MavenCompatFlag>,

    /// Override the project `barista.toml` path.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Disable ANSI colors (tty detection is on by default).
    #[arg(long, global = true)]
    pub no_color: bool,
}

/// Renderer selection for command output.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable text, ANSI-decorated on a tty.
    Human,
    /// A single JSON document on stdout.
    Json,
    /// Newline-delimited JSON; streams events as they happen.
    Ndjson,
}

/// Maven compatibility level.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum MavenCompatFlag {
    /// Auto-detect from the project (default).
    #[value(name = "auto")]
    Auto,
    /// Maven 3.9-compatible behavior.
    #[value(name = "3.9")]
    ThreeNine,
    /// Maven 4.0-compatible behavior.
    #[value(name = "4.0")]
    FourZero,
}

/// Top-level subcommand.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Resolve dependencies and write `barista.lock`.
    Pull(PullArgs),

    /// Inspect the resolved dependency graph.
    Grind {
        #[command(subcommand)]
        subcommand: GrindCommand,
    },

    /// Materialize resolved artifacts into a target directory.
    Pour(PourArgs),

    /// Interactive configuration wizard.
    #[command(name = "dial-in")]
    DialIn(DialInArgs),

    /// Run a one-off command without warming the daemon.
    Shot(ShotArgs),

    /// Generate `baristaw` wrapper scripts in the project root.
    // Disable the auto-propagated `--version` flag so `wrapper`
    // can use its own `--version` arg to pin the bundled Barista
    // version. Users who want the binary version can still call
    // `barista --version` at the top level.
    #[command(disable_version_flag = true)]
    Wrapper(WrapperArgs),

    // -- Maven-vocabulary commands ------------------------------
    // Each routes to barback in Phase 4. Phase 3 returns a
    // structured "not yet executable" error from
    // `cmd::maven_vocab` (Task 6).
    /// `mvn clean` drop-in.
    Clean(MavenVocabArgs),
    /// `mvn compile` drop-in.
    Compile(MavenVocabArgs),
    /// `mvn test` drop-in.
    Test(MavenVocabArgs),
    /// `mvn package` drop-in.
    Package(MavenVocabArgs),
    /// `mvn verify` drop-in.
    Verify(MavenVocabArgs),
    /// `mvn install` drop-in.
    Install(MavenVocabArgs),
    /// `mvn deploy` drop-in.
    Deploy(MavenVocabArgs),
    /// `mvn site` drop-in.
    Site(MavenVocabArgs),
}

/// Arguments for `barista pull`.
#[derive(Debug, clap::Args)]
pub struct PullArgs {
    /// Re-resolve from scratch; ignore the on-disk lockfile.
    #[arg(long)]
    pub update: bool,

    /// Limit resolution to the given Maven scope.
    #[arg(long, value_enum, default_value_t = ScopeArg::Compile, value_name = "SCOPE")]
    pub scope: ScopeArg,

    /// Skip artifact downloads (write the lockfile only).
    #[arg(long)]
    pub no_fetch: bool,

    /// Print the resolution rationale to stderr.
    #[arg(long)]
    pub explain: bool,
}

/// Maven dependency scope, as accepted by `--scope`.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum ScopeArg {
    /// Required for compilation; transitive.
    Compile,
    /// Required at runtime; not at compile time.
    Runtime,
    /// Required for tests only.
    Test,
    /// Provided by the runtime (e.g. servlet API).
    Provided,
    /// System-scoped (rarely used).
    System,
}

/// Subcommands of `barista grind`.
#[derive(Debug, Subcommand)]
pub enum GrindCommand {
    /// Render the resolved dependency tree.
    Tree(TreeArgs),
    /// Compare two lockfiles.
    Diff(DiffArgs),
    /// Query the graph for security / version concerns.
    Audit(AuditArgs),
    /// Explain why a coord is in the graph.
    Why(WhyArgs),
}

/// Arguments for `barista grind tree`.
#[derive(Debug, clap::Args)]
pub struct TreeArgs {
    /// Output format. `text` (default) or `json`.
    #[arg(long, value_enum, default_value_t = TreeFormat::Text, value_name = "FORMAT")]
    pub format: TreeFormat,
}

/// Renderer selection for `grind tree`.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum TreeFormat {
    /// Indented text tree, human-readable.
    Text,
    /// Structured JSON (machine-readable).
    Json,
}

/// Arguments for `barista grind diff`.
#[derive(Debug, clap::Args)]
pub struct DiffArgs {
    /// Base git ref to diff against (default: `HEAD`).
    #[arg(default_value = "HEAD", value_name = "BASE")]
    pub base_ref: String,
}

/// Arguments for `barista grind audit` (stub for v0.1).
#[derive(Debug, clap::Args)]
pub struct AuditArgs {}

/// Arguments for `barista grind why`.
#[derive(Debug, clap::Args)]
pub struct WhyArgs {
    /// Coordinate to explain: `group:artifact` or
    /// `group:artifact:version`.
    #[arg(value_name = "COORD")]
    pub coords: String,
}

/// Arguments for `barista pour`.
#[derive(Debug, clap::Args)]
pub struct PourArgs {
    /// Target directory for materialization
    /// (default: `~/.m2/repository`).
    #[arg(long, value_name = "DIR")]
    pub target: Option<PathBuf>,
}

/// Arguments for `barista dial-in`.
#[derive(Debug, clap::Args)]
pub struct DialInArgs {
    /// Run non-interactively (read answers from stdin or use
    /// defaults).
    #[arg(long)]
    pub non_interactive: bool,
}

/// Arguments for `barista shot`.
#[derive(Debug, clap::Args)]
pub struct ShotArgs {
    /// The command + args to run; forwarded to barback in
    /// shot mode.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS"
    )]
    pub args: Vec<String>,
}

/// Arguments for `barista wrapper`.
#[derive(Debug, clap::Args)]
pub struct WrapperArgs {
    /// Barista version to pin in the wrapper. Defaults to the
    /// current binary's version.
    #[arg(long)]
    pub version: Option<String>,
}

/// Pass-through arguments for the Maven-vocabulary commands.
///
/// Anything after the phase name is forwarded verbatim to the
/// Maven lifecycle (or, in Phase 4, to the barback daemon).
#[derive(Debug, clap::Args)]
pub struct MavenVocabArgs {
    /// Pass-through args/flags forwarded to the lifecycle phase.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS"
    )]
    pub args: Vec<String>,
}

/// Top-level CLI dispatch.
///
/// Each subcommand has a stub implementation that returns exit
/// code `2` (a not-yet-implemented sentinel). M3.1 T2–T6 + T8
/// replace the stubs with real impls in subsequent batches.
pub fn dispatch(cli: Cli) -> i32 {
    // The `--ci` shortcut expands to `--frozen --output json
    // --quiet` once `--frozen` lands. We still bind `cli.global`
    // here so that the (future) per-command dispatchers can read
    // it; the field is intentionally unused for now.
    let _global = &cli.global;

    match cli.command {
        Command::Pull(_) => stub("pull"),
        Command::Grind { subcommand } => match subcommand {
            GrindCommand::Tree(_) => stub("grind tree"),
            GrindCommand::Diff(_) => stub("grind diff"),
            GrindCommand::Audit(_) => stub("grind audit"),
            GrindCommand::Why(_) => stub("grind why"),
        },
        Command::Pour(_) => stub("pour"),
        Command::DialIn(_) => stub("dial-in"),
        Command::Shot(_) => stub("shot"),
        Command::Wrapper(_) => stub("wrapper"),
        Command::Clean(_) => stub_maven("clean"),
        Command::Compile(_) => stub_maven("compile"),
        Command::Test(_) => stub_maven("test"),
        Command::Package(_) => stub_maven("package"),
        Command::Verify(_) => stub_maven("verify"),
        Command::Install(_) => stub_maven("install"),
        Command::Deploy(_) => stub_maven("deploy"),
        Command::Site(_) => stub_maven("site"),
    }
}

fn stub(cmd: &str) -> i32 {
    eprintln!(
        "barista: `{cmd}` not yet implemented in this build. \
         The full implementation lands in a subsequent milestone."
    );
    2
}

fn stub_maven(phase: &str) -> i32 {
    eprintln!(
        "barista: maven-vocabulary command `{phase}` is not yet \
         executable — daemon support lands in a later phase. \
         For now, run `mvn {phase}` to execute the phase."
    );
    2
}
