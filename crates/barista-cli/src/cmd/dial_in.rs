//! `barista dial-in` — interactive configuration wizard.
//!
//! Walks the user through a small set of onboarding questions and
//! writes the answers to `~/.barista/config.toml` (overridable via
//! `--output`). The file is laid out so that `barista-config`'s
//! layered loader can round-trip it without error: every emitted
//! key maps to a real field on `PartialConfig`, and any informational
//! answers that don't have a home in the schema yet are emitted as
//! TOML comments above the active sections.
//!
//! ## Decoupled prompting
//!
//! The wizard never reads stdin directly. It takes a `&mut dyn
//! Prompter`, so tests can drive the flow programmatically with
//! [`ScriptedPrompter`] and the real CLI uses [`StdinPrompter`].
//!
//! ## Round-trip property
//!
//! The TOML produced by [`dial_in`] is intentionally a strict subset
//! of the schema understood by `barista-config`. Integration tests
//! load every written file back through `load_effective_config` and
//! assert success — that's the [T] linkage for M3.1 Task 5.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::cli::{DialInArgs, GlobalFlags};

// ============================================================
// Defaults
// ============================================================

/// Default Maven Central mirror suggested by the wizard. Stored as
/// a comment in the emitted TOML because mirrors live in
/// `~/.m2/settings.xml`, not in `~/.barista/config.toml`.
pub const DEFAULT_MIRROR_URL: &str = "https://repo.maven.apache.org/maven2";

/// Hard upper bound for the parallel-fetch concurrency answer.
/// Anything above this is clamped down; the resolver and HTTP pool
/// don't get materially faster past this point and very high values
/// can trip TCP/file-descriptor limits.
pub const MAX_CONCURRENCY: u32 = 32;

/// Returns the system's reported parallelism, clamped to
/// `[1, MAX_CONCURRENCY]`. Used as the default answer to the
/// "parallel-fetch concurrency" prompt.
pub fn default_concurrency() -> u32 {
    let n = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4);
    n.clamp(1, MAX_CONCURRENCY)
}

// ============================================================
// Prompter abstraction
// ============================================================

/// Minimal IO surface the wizard uses to gather answers.
///
/// Implementors are free to render the question however they like;
/// the wizard appends ` [default: …]` to the default itself. The
/// returned value is the user's parsed answer (already trimmed for
/// text inputs, already coerced for booleans).
pub trait Prompter {
    /// Ask for a free-form string. If the user submits an empty
    /// line and `default` is `Some`, the default is returned.
    fn ask_text(&mut self, question: &str, default: Option<&str>) -> Result<String, DialInError>;

    /// Ask a yes/no question. Accepts `y`, `yes`, `n`, `no`
    /// (case-insensitive). Empty input falls back to `default`.
    fn ask_bool(&mut self, question: &str, default: bool) -> Result<bool, DialInError>;
}

/// `Prompter` that reads from stdin and writes prompts to stderr.
///
/// stderr (rather than stdout) is used so the wizard remains usable
/// when stdout is piped — e.g. someone running `barista dial-in
/// --output - 2>/dev/null` would still see prompts in a TTY-only
/// world; in practice the CLI doesn't support `-` as a path, but
/// keeping prompts on stderr is the friendlier default.
pub struct StdinPrompter<R: BufRead, W: Write> {
    reader: R,
    writer: W,
}

impl<R: BufRead, W: Write> StdinPrompter<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }
}

impl StdinPrompter<std::io::BufReader<std::io::Stdin>, std::io::Stderr> {
    /// Real-world constructor backed by the process's stdin/stderr.
    pub fn from_process() -> Self {
        Self {
            reader: std::io::BufReader::new(std::io::stdin()),
            writer: std::io::stderr(),
        }
    }
}

impl<R: BufRead, W: Write> Prompter for StdinPrompter<R, W> {
    fn ask_text(&mut self, question: &str, default: Option<&str>) -> Result<String, DialInError> {
        match default {
            Some(d) => write!(self.writer, "{question} [{d}]: ")?,
            None => write!(self.writer, "{question}: ")?,
        }
        self.writer.flush()?;
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            // EOF — treat as "accept default" if any, else empty.
            return Ok(default.unwrap_or("").to_string());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(default.unwrap_or("").to_string());
        }
        Ok(trimmed.to_string())
    }

    fn ask_bool(&mut self, question: &str, default: bool) -> Result<bool, DialInError> {
        let hint = if default { "Y/n" } else { "y/N" };
        write!(self.writer, "{question} [{hint}]: ")?;
        self.writer.flush()?;
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(default);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" => Ok(default),
            "y" | "yes" => Ok(true),
            "n" | "no" => Ok(false),
            other => Err(DialInError::BadAnswer {
                question: question.to_string(),
                value: other.to_string(),
                expected: "y/yes/n/no",
            }),
        }
    }
}

/// `Prompter` backed by a pre-recorded queue of answers. Used by
/// tests and by `--non-interactive` (where every answer is the
/// empty string, which makes every prompt accept its default).
pub struct ScriptedPrompter {
    answers: VecDeque<String>,
}

impl ScriptedPrompter {
    /// Build a scripted prompter from a list of answers, applied in
    /// order. An empty string means "accept the default" for that
    /// prompt.
    pub fn new<I, S>(answers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            answers: answers.into_iter().map(Into::into).collect(),
        }
    }

    /// Build a scripted prompter that answers every question with
    /// the empty string, accepting all defaults. Used by
    /// `--non-interactive`.
    pub fn all_defaults() -> Self {
        Self {
            answers: VecDeque::new(),
        }
    }

    fn next_or_default(&mut self) -> Option<String> {
        self.answers.pop_front()
    }
}

impl Prompter for ScriptedPrompter {
    fn ask_text(&mut self, _question: &str, default: Option<&str>) -> Result<String, DialInError> {
        let answer = self.next_or_default().unwrap_or_default();
        if answer.is_empty() {
            Ok(default.unwrap_or("").to_string())
        } else {
            Ok(answer)
        }
    }

    fn ask_bool(&mut self, question: &str, default: bool) -> Result<bool, DialInError> {
        let answer = self.next_or_default().unwrap_or_default();
        match answer.trim().to_ascii_lowercase().as_str() {
            "" => Ok(default),
            "y" | "yes" | "true" => Ok(true),
            "n" | "no" | "false" => Ok(false),
            other => Err(DialInError::BadAnswer {
                question: question.to_string(),
                value: other.to_string(),
                expected: "y/yes/n/no",
            }),
        }
    }
}

// ============================================================
// Options + report
// ============================================================

/// Caller-supplied options for [`dial_in`]. Mirrors the CLI args
/// but is decoupled from `clap` so library consumers (tests, future
/// non-CLI front-ends) can call the wizard directly.
#[derive(Debug, Clone)]
pub struct DialInOpts {
    /// Where to write the generated config. The caller is responsible
    /// for tilde-expanding; the wizard never touches `~`.
    pub output_path: PathBuf,
    /// If true, overwrite an existing file at `output_path`. If
    /// false, an existing file causes [`DialInError::WouldOverwrite`].
    pub force: bool,
}

/// Captured answers from a single dial-in run, plus the file the
/// wizard wrote to. Returned so callers (and tests) can introspect
/// what was decided without re-parsing the TOML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialInReport {
    pub output_path: PathBuf,
    pub mirror_url: String,
    pub use_roastery: bool,
    pub roastery_url: Option<String>,
    pub concurrency: u32,
    pub strict: bool,
}

// ============================================================
// Errors
// ============================================================

/// Errors surfaced from `barista dial-in`.
#[derive(Debug, thiserror::Error)]
pub enum DialInError {
    #[error("I/O while reading or writing answers: {0}")]
    Io(#[from] std::io::Error),

    #[error("refusing to overwrite existing config at {path:?}; pass --force to clobber it")]
    WouldOverwrite { path: PathBuf },

    #[error(
        "could not resolve HOME (set $HOME or pass --output <PATH> to choose an explicit \
         destination)"
    )]
    NoHome,

    #[error("answer to {question:?} was {value:?}, but expected one of: {expected}")]
    BadAnswer {
        question: String,
        value: String,
        expected: &'static str,
    },

    #[error("answer to {question:?} ({value:?}) is not a valid {kind}")]
    InvalidNumber {
        question: String,
        value: String,
        kind: &'static str,
    },
}

// ============================================================
// Prompt strings
// ============================================================
//
// Held as `const`s so tests can assert on the verbatim text. Any
// reword here is a user-facing change and should be made
// deliberately.

pub const PROMPT_MIRROR_URL: &str = "Default Maven mirror URL";
pub const PROMPT_USE_ROASTERY: &str = "Use a roastery cache for shared artifact storage?";
pub const PROMPT_ROASTERY_URL: &str = "  Roastery URL";
pub const PROMPT_CONCURRENCY: &str = "Parallel-fetch concurrency (1-32)";
pub const PROMPT_STRICT: &str = "Enable strict resolution by default?";

// ============================================================
// Entry points
// ============================================================

/// Run the dial-in wizard end-to-end: ask the questions, then
/// write the TOML to `opts.output_path`. The `prompter` is the
/// I/O surface, so tests can drive a [`ScriptedPrompter`] and the
/// CLI can drive a [`StdinPrompter`].
pub fn dial_in(opts: DialInOpts, prompter: &mut dyn Prompter) -> Result<DialInReport, DialInError> {
    // Refuse-to-overwrite gate runs *before* prompts so the user
    // doesn't answer every question only to find their work
    // thrown away.
    if opts.output_path.exists() && !opts.force {
        return Err(DialInError::WouldOverwrite {
            path: opts.output_path.clone(),
        });
    }

    // 1. Mirror URL.
    let mirror_url = prompter.ask_text(PROMPT_MIRROR_URL, Some(DEFAULT_MIRROR_URL))?;

    // 2. Roastery? — and the conditional follow-up URL.
    let use_roastery = prompter.ask_bool(PROMPT_USE_ROASTERY, false)?;
    let roastery_url = if use_roastery {
        let url = prompter.ask_text(PROMPT_ROASTERY_URL, None)?;
        if url.is_empty() { None } else { Some(url) }
    } else {
        None
    };

    // 3. Parallel-fetch concurrency. Clamp aggressively — both ends.
    let default_conc = default_concurrency();
    let conc_raw = prompter.ask_text(PROMPT_CONCURRENCY, Some(&default_conc.to_string()))?;
    let conc_parsed: u32 = conc_raw.parse().map_err(|_| DialInError::InvalidNumber {
        question: PROMPT_CONCURRENCY.to_string(),
        value: conc_raw.clone(),
        kind: "unsigned integer",
    })?;
    let concurrency = conc_parsed.clamp(1, MAX_CONCURRENCY);

    // 4. Strict mode.
    let strict = prompter.ask_bool(PROMPT_STRICT, false)?;

    // Render and write.
    let toml_text = render_toml(
        &mirror_url,
        use_roastery,
        roastery_url.as_deref(),
        concurrency,
        strict,
    );
    write_atomically(&opts.output_path, &toml_text)?;

    Ok(DialInReport {
        output_path: opts.output_path,
        mirror_url,
        use_roastery,
        roastery_url,
        concurrency,
        strict,
    })
}

/// CLI entry: wires `dial-in` into the dispatcher.
///
/// Returns the process exit code (0 on success, 1 on user-facing
/// error). The wizard always honors `--non-interactive` by feeding
/// every prompt its default answer.
pub fn run(global: &GlobalFlags, args: &DialInArgs) -> i32 {
    match run_inner(global, args) {
        Ok(report) => {
            if !global.quiet {
                eprintln!(
                    "dial-in: wrote {} (concurrency={}, roastery={})",
                    report.output_path.display(),
                    report.concurrency,
                    if report.use_roastery { "yes" } else { "no" },
                );
            }
            0
        }
        Err(e) => {
            eprintln!("error: barista dial-in failed: {e}");
            1
        }
    }
}

fn run_inner(_global: &GlobalFlags, args: &DialInArgs) -> Result<DialInReport, DialInError> {
    let output_path = match &args.output_path {
        Some(p) => p.clone(),
        None => default_output_path()?,
    };
    let opts = DialInOpts {
        output_path,
        force: args.force,
    };

    if args.non_interactive {
        let mut prompter = ScriptedPrompter::all_defaults();
        dial_in(opts, &mut prompter)
    } else {
        let mut prompter = StdinPrompter::from_process();
        dial_in(opts, &mut prompter)
    }
}

/// Resolve `~/.barista/config.toml`, creating the parent directory
/// at write-time rather than here.
fn default_output_path() -> Result<PathBuf, DialInError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(DialInError::NoHome)?;
    Ok(home.join(".barista").join("config.toml"))
}

// ============================================================
// TOML rendering
// ============================================================

/// Render the answers into a TOML document.
///
/// Only fields that map to `PartialConfig` are emitted as real keys;
/// the mirror URL, roastery URL, and strict flag don't have homes
/// in today's schema yet, so they're recorded as guidance comments
/// at the top of the file. This keeps the output round-trip-parsable
/// by `barista-config::load_effective_config` while still preserving
/// the user's answers in a human-readable form.
fn render_toml(
    mirror_url: &str,
    use_roastery: bool,
    roastery_url: Option<&str>,
    concurrency: u32,
    strict: bool,
) -> String {
    let mut out = String::new();
    out.push_str("# barista user config — generated by `barista dial-in`.\n");
    out.push_str("#\n");
    out.push_str("# This is the user-level layer (~/.barista/config.toml). It is merged with\n");
    out.push_str("# project-level barista.toml, ~/.m2/settings.xml, BARISTA_* env vars, and CLI\n");
    out.push_str("# flags by `barista-config`. Anything below can be overridden per-project.\n");
    out.push_str("#\n");
    out.push_str("# Onboarding answers:\n");
    out.push_str(&format!("#   mirror-url     = {mirror_url:?}\n"));
    out.push_str("#   (Mirrors are honored via ~/.m2/settings.xml; see Maven docs for the\n");
    out.push_str("#   <mirrors> block. dial-in records your choice here for reference.)\n");
    out.push_str(&format!("#   use-roastery   = {use_roastery}\n"));
    if let Some(url) = roastery_url {
        out.push_str(&format!("#   roastery-url   = {url:?}\n"));
    }
    out.push_str(&format!("#   strict-default = {strict}\n"));
    out.push('\n');
    out.push_str("[network]\n");
    out.push_str(&format!("max-concurrent-connections = {concurrency}\n"));
    out
}

/// Write `contents` to `path`, creating the parent directory if
/// needed. Uses a temp-file + rename so a crash mid-write cannot
/// leave a half-written config in place.
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ============================================================
// Unit tests (round-trip property lives in tests/cmd_dial_in.rs)
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_concurrency_is_clamped() {
        let c = default_concurrency();
        assert!((1..=MAX_CONCURRENCY).contains(&c), "got {c}");
    }

    #[test]
    fn scripted_prompter_accepts_default_on_empty() {
        let mut p = ScriptedPrompter::new(vec!["".to_string(), "".to_string()]);
        let s = p.ask_text("q", Some("hello")).unwrap();
        assert_eq!(s, "hello");
        let b = p.ask_bool("q", true).unwrap();
        assert!(b);
    }

    #[test]
    fn scripted_prompter_uses_provided_answer() {
        let mut p = ScriptedPrompter::new(vec!["world".to_string()]);
        assert_eq!(p.ask_text("q", Some("hello")).unwrap(), "world");
    }

    #[test]
    fn scripted_prompter_bool_rejects_garbage() {
        let mut p = ScriptedPrompter::new(vec!["maybe".to_string()]);
        let err = p.ask_bool("q", false).unwrap_err();
        assert!(matches!(err, DialInError::BadAnswer { .. }));
    }

    #[test]
    fn render_toml_emits_network_section() {
        let s = render_toml("https://example.com/m2", false, None, 8, false);
        assert!(s.contains("[network]"));
        assert!(s.contains("max-concurrent-connections = 8"));
        assert!(s.contains("mirror-url"));
    }
}
