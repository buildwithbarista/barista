//! `barista wrapper` — generate `baristaw` launcher scripts.
//!
//! Analogous to Maven's `mvnw` and Gradle's `gradlew`. The command
//! drops three files into the project root:
//!
//! ```text
//! <project>/
//! ├── baristaw                # POSIX shell launcher (mode 0755)
//! ├── baristaw.cmd            # Windows batch launcher
//! └── .barista/
//!     └── wrapper.properties  # pinned version + download template
//! ```
//!
//! The two scripts are byte-for-byte the same on every project; the
//! per-project state lives in `.barista/wrapper.properties` so the
//! scripts can be regenerated without losing the pinned version.
//!
//! On first invocation each launcher reads `wrapper.properties`,
//! downloads the matching `barista` release into
//! `~/.barista/wrapper/<version>/`, verifies its checksum (if one
//! was recorded), and execs it. Subsequent invocations skip straight
//! to the exec.
//!
//! ## What this module does *not* do
//!
//! - It never reaches the network. The generator only writes local
//!   files; the download happens later, inside the script the user
//!   eventually runs.
//! - It does not validate that the chosen `--version` actually exists
//!   upstream. The user is free to pin a version that hasn't been
//!   published yet (useful when bootstrapping a new release).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::cli::{GlobalFlags, WrapperArgs};

/// The script the launcher reads on every invocation. Mirrored
/// into the project's `.barista/` directory.
const WRAPPER_PROPERTIES_FILE: &str = "wrapper.properties";

/// Per-project state directory the wrapper writes into.
const WRAPPER_STATE_DIR: &str = ".barista";

/// Embedded launcher templates. Each is the full content of the
/// generated file — no placeholder substitution happens at generate
/// time. The scripts themselves substitute `{version}` and `{target}`
/// into the download URL at *run* time.
const TEMPLATE_BARISTAW_SH: &str = include_str!("wrapper/templates/baristaw.sh");
const TEMPLATE_BARISTAW_CMD: &str = include_str!("wrapper/templates/baristaw.cmd");

/// Default download-URL template baked into the wrapper.
///
/// `{version}` and `{target}` are substituted by the launcher script
/// at run time, so the literal placeholders are preserved here. The
/// org name is a flag default — not hard-coded anywhere else.
pub const DEFAULT_DISTRIBUTION_URL: &str = "https://github.com/buildwithbarista/barista/releases/\
     download/v{version}/barista-{target}.tar.gz";

/// Returns the Barista version this binary was built from. Used as
/// the default value for `barista wrapper --version`.
pub fn current_barista_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `barista wrapper` dispatch entry point.
///
/// Returns the process exit code:
///
/// - `0` on success.
/// - `1` on a user-facing error (missing project dir, refusing to
///   overwrite without `--force`, I/O failure).
pub fn run(global: &GlobalFlags, args: &WrapperArgs) -> i32 {
    match run_inner(global, args) {
        Ok(report) => {
            if !global.quiet {
                eprintln!("{report}");
            }
            0
        }
        Err(e) => {
            eprintln!("error: barista wrapper failed: {e}");
            1
        }
    }
}

fn run_inner(global: &GlobalFlags, args: &WrapperArgs) -> Result<String, WrapperError> {
    let target_dir = resolve_target_dir(global)?;
    let plan = GeneratePlan {
        target_dir,
        version: args
            .version
            .clone()
            .unwrap_or_else(|| current_barista_version().to_string()),
        distribution_url: args
            .distribution_url
            .clone()
            .unwrap_or_else(|| DEFAULT_DISTRIBUTION_URL.to_string()),
        checksum_sha256: args.checksum.clone(),
        force: args.force,
    };
    let outcome = generate(&plan)?;
    Ok(format!(
        "baristaw: wrote {} files under {} (version {})",
        outcome.written.len(),
        plan.target_dir.display(),
        plan.version,
    ))
}

/// Resolve the directory the wrapper will be written into.
///
/// The wrapper is intentionally project-agnostic — it doesn't need
/// a `pom.xml` to anchor on, because the whole point of running it
/// is to bootstrap a project that might not have one yet. So we use
/// the explicit `--root` flag if provided, else the CWD.
fn resolve_target_dir(global: &GlobalFlags) -> Result<PathBuf, WrapperError> {
    if let Some(root) = &global.root {
        if !root.is_dir() {
            return Err(WrapperError::TargetNotDirectory { path: root.clone() });
        }
        return Ok(root.clone());
    }
    let cwd = std::env::current_dir().map_err(|source| WrapperError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    Ok(cwd)
}

/// Pure description of *what* to generate, separated from CLI
/// argument parsing so tests can drive it directly.
#[derive(Debug, Clone)]
pub struct GeneratePlan {
    /// Directory the launcher pair is written into.
    pub target_dir: PathBuf,
    /// Pinned version recorded in `wrapper.properties`.
    pub version: String,
    /// URL template recorded in `wrapper.properties`.
    pub distribution_url: String,
    /// Optional SHA-256 of the release archive.
    pub checksum_sha256: Option<String>,
    /// If true, overwrite an existing wrapper without complaint.
    pub force: bool,
}

/// Result of a successful [`generate`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerateOutcome {
    /// Absolute paths of every file the call wrote (or rewrote).
    pub written: Vec<PathBuf>,
}

/// Write the wrapper file tree for [`GeneratePlan`].
///
/// On success returns the list of paths written, in the order they
/// were emitted. The order is stable across calls and useful for
/// snapshot tests.
pub fn generate(plan: &GeneratePlan) -> Result<GenerateOutcome, WrapperError> {
    let sh_path = plan.target_dir.join("baristaw");
    let cmd_path = plan.target_dir.join("baristaw.cmd");
    let state_dir = plan.target_dir.join(WRAPPER_STATE_DIR);
    let props_path = state_dir.join(WRAPPER_PROPERTIES_FILE);

    // Refuse to clobber unless --force; tested above the I/O layer
    // so the precondition is the same on every platform.
    if !plan.force {
        for p in [&sh_path, &cmd_path, &props_path] {
            if p.exists() {
                return Err(WrapperError::AlreadyExists { path: p.clone() });
            }
        }
    }

    fs::create_dir_all(&state_dir).map_err(|source| WrapperError::Io {
        path: state_dir.clone(),
        source,
    })?;

    // The two launchers are platform-neutral text. Mode bits are
    // set explicitly on Unix so a freshly-generated `baristaw` is
    // executable right out of the box.
    write_atomic(&sh_path, TEMPLATE_BARISTAW_SH.as_bytes())?;
    set_executable(&sh_path)?;
    write_atomic(&cmd_path, TEMPLATE_BARISTAW_CMD.as_bytes())?;

    let props_text = render_properties(plan);
    write_atomic(&props_path, props_text.as_bytes())?;

    Ok(GenerateOutcome {
        written: vec![sh_path, cmd_path, props_path],
    })
}

/// Render the contents of `wrapper.properties`.
///
/// Exposed for tests + the `insta` snapshot. The result is a small
/// TOML document, parseable by the `toml` crate.
pub fn render_properties(plan: &GeneratePlan) -> String {
    let mut out = String::new();
    out.push_str("# baristaw wrapper configuration.\n");
    out.push_str("# Edit `version` to change the pinned barista release.\n");
    out.push_str("# {version} and {target} in `distribution_url` are\n");
    out.push_str("# substituted by the launcher script at run time.\n");
    out.push('\n');
    out.push_str(&format!("version = \"{}\"\n", plan.version));
    out.push_str(&format!(
        "distribution_url = \"{}\"\n",
        plan.distribution_url
    ));
    match &plan.checksum_sha256 {
        Some(c) => out.push_str(&format!("checksum_sha256 = \"{c}\"\n")),
        None => out.push_str("checksum_sha256 = \"\"\n"),
    }
    out
}

/// Atomic write: stage as a tempfile in the same directory, then
/// rename into place. Avoids leaving a half-written launcher behind
/// if the process is interrupted.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), WrapperError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| WrapperError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, bytes).map_err(|source| WrapperError::Io {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, path).map_err(|source| WrapperError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Mark `path` executable on Unix (mode `0o755`). No-op on Windows,
/// where executability is decided by extension.
#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), WrapperError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(0o755);
    fs::set_permissions(path, perms).map_err(|source| WrapperError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), WrapperError> {
    Ok(())
}

/// Errors surfaced from `barista wrapper`.
#[derive(Debug, thiserror::Error)]
pub enum WrapperError {
    /// `--root` was passed but is not a directory.
    #[error("`--root {path:?}` is not a directory")]
    TargetNotDirectory {
        /// The path the user passed.
        path: PathBuf,
    },

    /// One of the wrapper files already exists and `--force` was not
    /// passed. Names the offending path so the user can either rm it
    /// or re-run with `--force`.
    #[error(
        "{path:?} already exists. Re-run with `--force` to overwrite, \
         or delete the file first."
    )]
    AlreadyExists {
        /// The path that would have been overwritten.
        path: PathBuf,
    },

    /// Generic I/O error at a known path.
    #[error("I/O at {path:?}: {source}")]
    Io {
        /// The path the I/O operation was targeting.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_properties_round_trips_through_toml() {
        let plan = GeneratePlan {
            target_dir: PathBuf::from("/tmp/example"),
            version: "0.1.0-alpha.0".to_string(),
            distribution_url: DEFAULT_DISTRIBUTION_URL.to_string(),
            checksum_sha256: Some("abc123".to_string()),
            force: false,
        };
        let text = render_properties(&plan);
        let parsed: toml::Value = toml::from_str(&text).expect("valid TOML");
        assert_eq!(parsed["version"].as_str(), Some("0.1.0-alpha.0"));
        assert_eq!(
            parsed["distribution_url"].as_str(),
            Some(DEFAULT_DISTRIBUTION_URL)
        );
        assert_eq!(parsed["checksum_sha256"].as_str(), Some("abc123"));
    }

    #[test]
    fn render_properties_emits_empty_checksum_when_absent() {
        let plan = GeneratePlan {
            target_dir: PathBuf::from("/tmp/example"),
            version: "0.2.0".to_string(),
            distribution_url: DEFAULT_DISTRIBUTION_URL.to_string(),
            checksum_sha256: None,
            force: false,
        };
        let text = render_properties(&plan);
        assert!(text.contains("checksum_sha256 = \"\""));
    }

    #[test]
    fn current_version_matches_cargo_pkg_version() {
        assert_eq!(current_barista_version(), env!("CARGO_PKG_VERSION"));
    }
}
