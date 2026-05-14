//! Project-root resolution.
//!
//! Resolves the directory containing the top-level `pom.xml` from
//! a combination of:
//!
//! 1. An explicit `--root <dir>` flag.
//! 2. An explicit `-f` / `--file <pom>` flag (file or directory).
//! 3. A walk-up from the current working directory, bounded by
//!    `.git` so we never escape the enclosing project.
//! 4. A sticky fallback at `~/.barista/run/last-project`, useful
//!    for short-lived shell snippets that `cd` around without
//!    re-establishing the project context.
//!
//! Every Phase 3 subcommand that needs a project consumes this
//! module. Subcommands that intentionally have no project (e.g.
//! `dial-in`, `wrapper`) skip it.
//!
//! The resolver is pure: every filesystem-touching path is
//! exposed via [`ResolveInputs`] so tests can drive the entire
//! decision matrix without touching the user's real `$HOME` or
//! CWD.

use std::path::{Path, PathBuf};

/// The result of a successful project-root resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRoot {
    /// Directory containing `pom.xml`.
    pub root: PathBuf,
    /// Path to the project's `pom.xml`.
    pub pom: PathBuf,
    /// How the root was discovered.
    pub source: RootSource,
}

/// The mechanism that produced a [`ProjectRoot`].
///
/// Mostly informational — useful for `-v` reporting and for tests
/// that want to assert which branch fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSource {
    /// User passed `--root <dir>`.
    ExplicitRoot,
    /// User passed `-f <file>` or `--file <file>`.
    ExplicitFile,
    /// Walked up from CWD until `pom.xml` was found.
    WalkUp,
    /// Loaded from `~/.barista/run/last-project`.
    Sticky,
}

/// Errors returned by [`resolve_project_root`].
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// No `pom.xml` found on any of the configured strategies.
    #[error("no pom.xml found in {dir:?} (walked up from CWD; no fallback available)")]
    NoPomFound {
        /// The CWD the search started from.
        dir: PathBuf,
    },
    /// `--root` was passed but does not name a directory.
    #[error("`--root {root:?}` is not a directory")]
    RootNotDirectory {
        /// The path the user provided.
        root: PathBuf,
    },
    /// `--root` points at a directory with no `pom.xml`.
    #[error("`--root {root:?}` does not contain a pom.xml")]
    RootMissingPom {
        /// The path the user provided.
        root: PathBuf,
    },
    /// `-f` / `--file` was passed but the path does not exist.
    #[error("`-f {file:?}` does not exist")]
    FileMissing {
        /// The path the user provided.
        file: PathBuf,
    },
    /// Generic I/O error reading something on the filesystem.
    #[error("I/O error at {path:?}: {source}")]
    Io {
        /// The path the I/O error happened on.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// `$HOME` could not be resolved and no override was passed.
    #[error("HOME directory could not be resolved")]
    NoHome,
}

/// Closure type for the environment-variable getter on
/// [`ResolveInputs`]. Boxed-up here so clippy doesn't flag the
/// struct field as a complex type.
pub type EnvGetter<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Inputs to [`resolve_project_root`].
///
/// Every fact the resolver needs is on this struct so tests can
/// substitute fixtures without process-global state.
#[derive(Default)]
pub struct ResolveInputs<'a> {
    /// `--root <dir>` override.
    pub root: Option<PathBuf>,
    /// `-f` / `--file <pom>` override.
    pub file: Option<PathBuf>,
    /// CWD; defaults to `std::env::current_dir()`.
    pub cwd_override: Option<PathBuf>,
    /// HOME; defaults to `$HOME`.
    pub home_override: Option<PathBuf>,
    /// Skip the sticky-fallback step.
    pub skip_sticky: bool,
    /// Env-var getter (currently unused; reserved for future
    /// `BARISTA_*` overrides — keeps the API stable across the
    /// rest of M3.1).
    pub env_get: Option<EnvGetter<'a>>,
}

impl<'a> std::fmt::Debug for ResolveInputs<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolveInputs")
            .field("root", &self.root)
            .field("file", &self.file)
            .field("cwd_override", &self.cwd_override)
            .field("home_override", &self.home_override)
            .field("skip_sticky", &self.skip_sticky)
            .field("env_get", &self.env_get.map(|_| "<fn>"))
            .finish()
    }
}

/// Resolve a project root from the given inputs.
///
/// The resolution order is fixed:
///
/// 1. `--root` — wins outright; missing `pom.xml` is an error.
/// 2. `--file` — wins next; a directory is treated as a root, a
///    file as the pom path itself.
/// 3. Walk-up from CWD, bounded by `.git`.
/// 4. Sticky fallback at `~/.barista/run/last-project`.
/// 5. Hard error.
pub fn resolve_project_root(inputs: ResolveInputs<'_>) -> Result<ProjectRoot, ResolveError> {
    // 1. --root wins if present.
    if let Some(root) = inputs.root.clone() {
        let root = absolutize(&root, &inputs)?;
        if !root.is_dir() {
            return Err(ResolveError::RootNotDirectory { root });
        }
        let pom = root.join("pom.xml");
        if !pom.is_file() {
            return Err(ResolveError::RootMissingPom { root });
        }
        return Ok(ProjectRoot {
            root,
            pom,
            source: RootSource::ExplicitRoot,
        });
    }

    // 2. -f / --file wins next.
    if let Some(file) = inputs.file.clone() {
        let abs = absolutize(&file, &inputs)?;
        if !abs.exists() {
            return Err(ResolveError::FileMissing { file: abs });
        }
        let (root, pom) = if abs.is_dir() {
            let pom = abs.join("pom.xml");
            if !pom.is_file() {
                return Err(ResolveError::RootMissingPom { root: abs.clone() });
            }
            (abs, pom)
        } else {
            let root = abs
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/"));
            (root, abs)
        };
        return Ok(ProjectRoot {
            root,
            pom,
            source: RootSource::ExplicitFile,
        });
    }

    // 3. Walk up from CWD, stopping at `.git`.
    let cwd = current_dir(&inputs)?;
    let mut cursor = cwd.clone();
    loop {
        let pom = cursor.join("pom.xml");
        if pom.is_file() {
            return Ok(ProjectRoot {
                root: cursor.clone(),
                pom,
                source: RootSource::WalkUp,
            });
        }
        // Stop at the .git boundary; we don't escape a project
        // even if a stray pom.xml sits one directory above.
        if cursor.join(".git").exists() {
            break;
        }
        if !cursor.pop() {
            break;
        }
    }

    // 4. Sticky fallback.
    if !inputs.skip_sticky {
        if let Some(sticky) = read_sticky(&inputs)? {
            let pom = sticky.join("pom.xml");
            if pom.is_file() {
                return Ok(ProjectRoot {
                    root: sticky,
                    pom,
                    source: RootSource::Sticky,
                });
            }
        }
    }

    Err(ResolveError::NoPomFound { dir: cwd })
}

/// Write `root` to `~/.barista/run/last-project`.
///
/// Called after a successful command so the next invocation can
/// recover the project root without a fresh walk-up — useful for
/// shell snippets that `cd` into a subdir and run a one-off.
///
/// Idempotent; creates the parent directories if missing.
pub fn record_sticky(root: &Path, home: Option<&Path>) -> Result<(), ResolveError> {
    let home = resolve_home(home)?;
    let run_dir = home.join(".barista").join("run");
    std::fs::create_dir_all(&run_dir).map_err(|e| ResolveError::Io {
        path: run_dir.clone(),
        source: e,
    })?;
    let path = run_dir.join("last-project");
    std::fs::write(&path, root.to_string_lossy().as_bytes())
        .map_err(|e| ResolveError::Io { path, source: e })?;
    Ok(())
}

fn read_sticky(inputs: &ResolveInputs<'_>) -> Result<Option<PathBuf>, ResolveError> {
    let home = resolve_home(inputs.home_override.as_deref())?;
    let path = home.join(".barista").join("run").join("last-project");
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let p = PathBuf::from(s.trim());
            if p.is_dir() { Ok(Some(p)) } else { Ok(None) }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ResolveError::Io { path, source: e }),
    }
}

fn resolve_home(over: Option<&Path>) -> Result<PathBuf, ResolveError> {
    if let Some(h) = over {
        return Ok(h.to_path_buf());
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(ResolveError::NoHome)
}

fn current_dir(inputs: &ResolveInputs<'_>) -> Result<PathBuf, ResolveError> {
    if let Some(c) = inputs.cwd_override.clone() {
        return Ok(c);
    }
    std::env::current_dir().map_err(|e| ResolveError::Io {
        path: PathBuf::new(),
        source: e,
    })
}

fn absolutize(p: &Path, inputs: &ResolveInputs<'_>) -> Result<PathBuf, ResolveError> {
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    let cwd = current_dir(inputs)?;
    Ok(cwd.join(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Create an empty `pom.xml` at `dir`.
    fn touch_pom(dir: &Path) {
        fs::write(dir.join("pom.xml"), b"<project/>").unwrap();
    }

    /// Create an empty `.git` directory marker at `dir`.
    fn touch_git(dir: &Path) {
        fs::create_dir_all(dir.join(".git")).unwrap();
    }

    // ----- --root branch -------------------------------------------------

    #[test]
    fn root_with_pom_is_explicit_root() {
        let td = tempdir().unwrap();
        touch_pom(td.path());
        let out = resolve_project_root(ResolveInputs {
            root: Some(td.path().to_path_buf()),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(out.root, td.path());
        assert_eq!(out.pom, td.path().join("pom.xml"));
        assert_eq!(out.source, RootSource::ExplicitRoot);
    }

    #[test]
    fn root_without_pom_errors() {
        let td = tempdir().unwrap();
        let err = resolve_project_root(ResolveInputs {
            root: Some(td.path().to_path_buf()),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, ResolveError::RootMissingPom { .. }));
    }

    #[test]
    fn root_not_a_directory_errors() {
        let td = tempdir().unwrap();
        let f = td.path().join("not-a-dir");
        fs::write(&f, b"hi").unwrap();
        let err = resolve_project_root(ResolveInputs {
            root: Some(f),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, ResolveError::RootNotDirectory { .. }));
    }

    // ----- -f / --file branch -------------------------------------------

    #[test]
    fn file_pointing_at_pom_xml_is_explicit_file() {
        let td = tempdir().unwrap();
        touch_pom(td.path());
        let pom = td.path().join("pom.xml");
        let out = resolve_project_root(ResolveInputs {
            file: Some(pom.clone()),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(out.pom, pom);
        assert_eq!(out.root, td.path());
        assert_eq!(out.source, RootSource::ExplicitFile);
    }

    #[test]
    fn file_pointing_at_dir_with_pom_is_explicit_file() {
        let td = tempdir().unwrap();
        touch_pom(td.path());
        let out = resolve_project_root(ResolveInputs {
            file: Some(td.path().to_path_buf()),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(out.root, td.path());
        assert_eq!(out.source, RootSource::ExplicitFile);
    }

    #[test]
    fn file_missing_errors() {
        let td = tempdir().unwrap();
        let missing = td.path().join("nope.xml");
        let err = resolve_project_root(ResolveInputs {
            file: Some(missing),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, ResolveError::FileMissing { .. }));
    }

    #[test]
    fn file_pointing_at_dir_without_pom_errors() {
        let td = tempdir().unwrap();
        let err = resolve_project_root(ResolveInputs {
            file: Some(td.path().to_path_buf()),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, ResolveError::RootMissingPom { .. }));
    }

    // ----- walk-up branch -----------------------------------------------

    #[test]
    fn walk_up_from_nested_cwd_finds_pom() {
        let td = tempdir().unwrap();
        let root = td.path();
        touch_pom(root);
        touch_git(root);
        let nested = root.join("a").join("b").join("c");
        fs::create_dir_all(&nested).unwrap();
        let out = resolve_project_root(ResolveInputs {
            cwd_override: Some(nested),
            home_override: Some(td.path().join("no-home")),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(out.root, root);
        assert_eq!(out.source, RootSource::WalkUp);
    }

    #[test]
    fn walk_up_stops_at_git_boundary() {
        // Layout:
        //   outer/pom.xml          <- should NOT be found
        //   outer/inner/.git/
        //   outer/inner/sub/       <- CWD; no pom.xml anywhere below outer
        let td = tempdir().unwrap();
        let outer = td.path();
        touch_pom(outer);
        let inner = outer.join("inner");
        fs::create_dir_all(&inner).unwrap();
        touch_git(&inner);
        let sub = inner.join("sub");
        fs::create_dir_all(&sub).unwrap();

        let err = resolve_project_root(ResolveInputs {
            cwd_override: Some(sub),
            home_override: Some(td.path().join("no-home")),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap_err();
        // Should fail rather than find the outer pom.xml.
        assert!(matches!(err, ResolveError::NoPomFound { .. }));
    }

    #[test]
    fn walk_up_no_pom_errors() {
        let td = tempdir().unwrap();
        let nested = td.path().join("x").join("y");
        fs::create_dir_all(&nested).unwrap();
        // Plant a .git in td so we don't escape the tempdir.
        touch_git(td.path());
        let err = resolve_project_root(ResolveInputs {
            cwd_override: Some(nested),
            home_override: Some(td.path().join("no-home")),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, ResolveError::NoPomFound { .. }));
    }

    // ----- sticky fallback ----------------------------------------------

    #[test]
    fn sticky_fallback_returns_recorded_root() {
        let td = tempdir().unwrap();
        let proj = td.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        touch_pom(&proj);

        let home = td.path().join("home");
        fs::create_dir_all(home.join(".barista").join("run")).unwrap();
        record_sticky(&proj, Some(&home)).unwrap();

        // CWD: somewhere with no pom and a .git so we stop the
        // walk-up quickly.
        let cwd = td.path().join("cwd");
        fs::create_dir_all(&cwd).unwrap();
        touch_git(&cwd);

        let out = resolve_project_root(ResolveInputs {
            cwd_override: Some(cwd),
            home_override: Some(home),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(out.root, proj);
        assert_eq!(out.source, RootSource::Sticky);
    }

    #[test]
    fn sticky_skipped_when_skip_sticky_set() {
        let td = tempdir().unwrap();
        let proj = td.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        touch_pom(&proj);

        let home = td.path().join("home");
        record_sticky(&proj, Some(&home)).unwrap();

        let cwd = td.path().join("cwd");
        fs::create_dir_all(&cwd).unwrap();
        touch_git(&cwd);

        let err = resolve_project_root(ResolveInputs {
            cwd_override: Some(cwd),
            home_override: Some(home),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, ResolveError::NoPomFound { .. }));
    }

    #[test]
    fn sticky_pointing_at_deleted_dir_falls_through() {
        let td = tempdir().unwrap();
        let proj = td.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        let home = td.path().join("home");
        record_sticky(&proj, Some(&home)).unwrap();
        // Now nuke the project dir.
        fs::remove_dir_all(&proj).unwrap();

        let cwd = td.path().join("cwd");
        fs::create_dir_all(&cwd).unwrap();
        touch_git(&cwd);

        let err = resolve_project_root(ResolveInputs {
            cwd_override: Some(cwd),
            home_override: Some(home),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, ResolveError::NoPomFound { .. }));
    }

    // ----- precedence ---------------------------------------------------

    #[test]
    fn root_wins_over_file() {
        let td = tempdir().unwrap();
        let root = td.path().join("r");
        fs::create_dir_all(&root).unwrap();
        touch_pom(&root);
        let other = td.path().join("o");
        fs::create_dir_all(&other).unwrap();
        touch_pom(&other);

        let out = resolve_project_root(ResolveInputs {
            root: Some(root.clone()),
            file: Some(other.join("pom.xml")),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(out.root, root);
        assert_eq!(out.source, RootSource::ExplicitRoot);
    }

    #[test]
    fn root_wins_over_walk_up_and_sticky() {
        let td = tempdir().unwrap();
        // walk-up source
        let walk_root = td.path().join("walk");
        fs::create_dir_all(&walk_root).unwrap();
        touch_pom(&walk_root);
        touch_git(&walk_root);
        // sticky source
        let sticky_root = td.path().join("sticky");
        fs::create_dir_all(&sticky_root).unwrap();
        touch_pom(&sticky_root);
        // explicit root
        let explicit = td.path().join("explicit");
        fs::create_dir_all(&explicit).unwrap();
        touch_pom(&explicit);

        let home = td.path().join("home");
        record_sticky(&sticky_root, Some(&home)).unwrap();

        let out = resolve_project_root(ResolveInputs {
            root: Some(explicit.clone()),
            cwd_override: Some(walk_root),
            home_override: Some(home),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(out.root, explicit);
        assert_eq!(out.source, RootSource::ExplicitRoot);
    }

    #[test]
    fn relative_root_resolves_against_cwd() {
        let td = tempdir().unwrap();
        let proj = td.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        touch_pom(&proj);

        let out = resolve_project_root(ResolveInputs {
            root: Some(PathBuf::from("proj")),
            cwd_override: Some(td.path().to_path_buf()),
            home_override: Some(td.path().join("no-home")),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(out.root, td.path().join("proj"));
    }

    // ----- record_sticky -------------------------------------------------

    #[test]
    fn record_sticky_creates_parent_dirs() {
        let td = tempdir().unwrap();
        let home = td.path().join("fresh-home");
        let proj = td.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        record_sticky(&proj, Some(&home)).unwrap();
        let recorded =
            fs::read_to_string(home.join(".barista").join("run").join("last-project")).unwrap();
        assert_eq!(recorded.trim(), proj.to_string_lossy());
    }

    #[test]
    fn record_sticky_overwrites_idempotently() {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        let a = td.path().join("a");
        let b = td.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        record_sticky(&a, Some(&home)).unwrap();
        record_sticky(&b, Some(&home)).unwrap();
        let recorded =
            fs::read_to_string(home.join(".barista").join("run").join("last-project")).unwrap();
        assert_eq!(recorded.trim(), b.to_string_lossy());
    }

    // ----- walk-up edge case: pom alongside .git -------------------------

    #[test]
    fn walk_up_finds_pom_when_git_lives_in_same_dir() {
        // The .git boundary check must run AFTER the pom check,
        // otherwise a project rooted at a git repo would be
        // unfindable.
        let td = tempdir().unwrap();
        let root = td.path();
        touch_pom(root);
        touch_git(root);

        let out = resolve_project_root(ResolveInputs {
            cwd_override: Some(root.to_path_buf()),
            home_override: Some(td.path().join("no-home")),
            skip_sticky: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(out.root, root);
        assert_eq!(out.source, RootSource::WalkUp);
    }
}
