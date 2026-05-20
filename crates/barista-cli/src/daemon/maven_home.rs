// SPDX-License-Identifier: MIT OR Apache-2.0

//! Resolve the Maven 4 distribution home the `barback` daemon loads its
//! embedded core from.
//!
//! # Why the launcher resolves this at all
//!
//! `barback`'s `EmbeddedMavenFactory` refuses to start without a Maven 4
//! distribution directory, configured via the `-Dbarista.maven.home=<path>`
//! JVM system property or the `BARISTA_MAVEN_HOME` environment variable.
//! In a dev checkout one is staged by hand; in an **end-user install**
//! (`brew install barista`, a GitHub release tarball, the container image)
//! there is no Maven on the host, so a first-run `barista verify` would
//! spawn barback, barback would throw "no embedded Maven 4 distribution
//! configured", and the user would only see `BAR-DAEMON-SPAWN-TIMEOUT`
//! with no actionable remediation.
//!
//! The release tarballs ship a pinned Maven 4 distribution **bundled**
//! inside the artifact, under `share/barista/maven-4/` (a sibling of the
//! `bin/` directory holding the `barista` executable). When neither the
//! override nor the env var is set, the launcher discovers that bundled
//! distribution from its own executable's location and points barback at
//! it — so a freshly-installed `barista` "just works" with no environment
//! configuration.
//!
//! # Resolution precedence
//!
//! [`resolve_maven_home`] evaluates, in order, and the first hit wins:
//!
//! 1. **`-Dbarista.maven.home=` override** — if the caller has surfaced an
//!    explicit Maven-home override (e.g. a future `--maven-home` flag or a
//!    `BARISTA_MAVEN_HOME_OVERRIDE` escape hatch), it takes precedence over
//!    everything. Modeled as the `override_home` parameter so the source is
//!    explicit and testable.
//! 2. **`BARISTA_MAVEN_HOME` env var** — the existing contract barback reads
//!    directly. If the user (or the dev/test harness) exported it, the
//!    launcher leaves it untouched and reports the env source.
//! 3. **Bundled fallback** — `<install-root>/share/barista/maven-4`, derived
//!    from the running executable's location, used only when the directory
//!    exists and validates as a Maven home (see [`bundled_maven_home`]).
//! 4. **None** — no source resolves. The launcher does NOT inject a
//!    `BARISTA_MAVEN_HOME`; barback surfaces its existing actionable error
//!    ("set -Dbarista.maven.home / export BARISTA_MAVEN_HOME …") rather than
//!    a bare spawn timeout.
//!
//! Auto-download of the distribution (delivery shape "b") is intentionally
//! NOT implemented: bundling is the default. A future config opt-in could
//! add an on-demand download for size-constrained environments, but that is
//! out of scope here.

use std::path::{Path, PathBuf};

/// Environment variable barback reads to locate the Maven 4 distribution.
/// Mirrors `EmbeddedMavenFactory.MAVEN_HOME_ENV` on the Java side.
pub const MAVEN_HOME_ENV: &str = "BARISTA_MAVEN_HOME";

/// Install-root-relative path to the bundled Maven 4 distribution inside a
/// release tarball: `<install-root>/share/barista/maven-4`. The `barista`
/// executable lives at `<install-root>/bin/barista`, so the install root is
/// the executable's grandparent directory.
///
/// The components are spelled out (rather than a single joined literal) so
/// the path is built portably with the platform separator.
pub const BUNDLED_MAVEN_REL: [&str; 3] = ["share", "barista", "maven-4"];

/// Where the resolved Maven home came from. Surfaced in a `tracing` line so
/// the resolution is observable in the field without a debugger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MavenHomeSource {
    /// An explicit override (a `-Dbarista.maven.home=` flag / escape hatch).
    Override,
    /// The inherited `BARISTA_MAVEN_HOME` environment variable.
    Env,
    /// The bundled distribution under `<install-root>/share/barista/maven-4`.
    Bundled,
    /// Nothing resolved; barback will surface its own actionable error.
    None,
}

impl MavenHomeSource {
    /// Stable lowercase label for the `tracing` line / tests.
    pub fn label(self) -> &'static str {
        match self {
            MavenHomeSource::Override => "override",
            MavenHomeSource::Env => "env",
            MavenHomeSource::Bundled => "bundled",
            MavenHomeSource::None => "none",
        }
    }
}

/// The resolved Maven home plus the source that provided it.
///
/// `path` is `None` only when `source == MavenHomeSource::None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMavenHome {
    /// The resolved distribution directory, if any.
    pub path: Option<PathBuf>,
    /// Which precedence tier provided `path`.
    pub source: MavenHomeSource,
}

/// A filesystem probe, injected so [`bundled_maven_home`] is unit-testable
/// against a temp-dir layout without requiring a real install on disk and
/// without the function reaching out to the ambient filesystem directly.
///
/// The two questions the bundled-home validation needs to ask are "is this a
/// directory?" and "is this a regular file?"; the probe answers both.
pub trait FsProbe {
    /// Does `p` exist and is it a directory?
    fn is_dir(&self, p: &Path) -> bool;
    /// Does `p` exist and is it a regular file?
    fn is_file(&self, p: &Path) -> bool;
}

/// Real filesystem probe backed by [`std::path::Path`] queries.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealFs;

impl FsProbe for RealFs {
    fn is_dir(&self, p: &Path) -> bool {
        p.is_dir()
    }
    fn is_file(&self, p: &Path) -> bool {
        p.is_file()
    }
}

/// Derive the bundled Maven home from the running executable's path and
/// validate it looks like a Maven distribution.
///
/// `current_exe` is the path to the running `barista` binary (production
/// callers pass `std::env::current_exe()`; tests pass a synthesized path).
/// In a release install it is `<install-root>/bin/barista`, so the install
/// root is `current_exe.parent().parent()` and the candidate distribution is
/// `<install-root>/share/barista/maven-4`.
///
/// Returns `Some(<candidate>)` only when, per `fs`:
///   * the candidate directory exists, AND
///   * it contains a `bin/mvn` **or** `bin/mvn.cmd` launcher, AND
///   * it contains a `lib/` directory.
///
/// The `lib/` + launcher check guards against pointing barback at a
/// non-Maven directory that merely happens to sit at the expected path; it
/// mirrors barback's own `requireDistribution` check (which validates
/// `lib/` + `boot/`) closely enough to fail fast in the launcher rather than
/// surfacing the failure as an opaque spawn timeout.
///
/// Pure with respect to `fs`: given the same `current_exe` and the same
/// probe answers, it always returns the same result.
pub fn bundled_maven_home(current_exe: &Path, fs: &impl FsProbe) -> Option<PathBuf> {
    // <install-root> = grandparent of the executable
    // (current_exe == <root>/bin/barista).
    let install_root = current_exe.parent()?.parent()?;

    let mut candidate = install_root.to_path_buf();
    for component in BUNDLED_MAVEN_REL {
        candidate.push(component);
    }

    if !fs.is_dir(&candidate) {
        return None;
    }
    // Must contain a Maven launcher under bin/ and a lib/ directory.
    let has_launcher =
        fs.is_file(&candidate.join("bin").join("mvn")) || fs.is_file(&candidate.join("bin").join("mvn.cmd"));
    let has_lib = fs.is_dir(&candidate.join("lib"));
    if has_launcher && has_lib {
        Some(candidate)
    } else {
        None
    }
}

/// Resolve the Maven home for the barback spawn, applying the precedence
/// documented on this module.
///
/// Parameters are injected so the precedence is exercisable without touching
/// process-wide state:
///   * `override_home` — an explicit `-Dbarista.maven.home=`-equivalent
///     override, if the caller surfaced one (highest precedence).
///   * `env_home` — the value of `BARISTA_MAVEN_HOME` as seen by the
///     launcher (`None` when unset/empty).
///   * `current_exe` — the running executable path, for the bundled probe.
///   * `fs` — the filesystem probe.
pub fn resolve_maven_home(
    override_home: Option<PathBuf>,
    env_home: Option<PathBuf>,
    current_exe: Option<&Path>,
    fs: &impl FsProbe,
) -> ResolvedMavenHome {
    if let Some(p) = override_home {
        return ResolvedMavenHome {
            path: Some(p),
            source: MavenHomeSource::Override,
        };
    }
    if let Some(p) = env_home {
        return ResolvedMavenHome {
            path: Some(p),
            source: MavenHomeSource::Env,
        };
    }
    if let Some(exe) = current_exe
        && let Some(bundled) = bundled_maven_home(exe, fs)
    {
        return ResolvedMavenHome {
            path: Some(bundled),
            source: MavenHomeSource::Bundled,
        };
    }
    ResolvedMavenHome {
        path: None,
        source: MavenHomeSource::None,
    }
}

/// Read the launcher's view of the ambient Maven-home sources from the
/// process environment, then resolve via [`resolve_maven_home`].
///
/// This is the production entry point [`super::launcher::spawn_daemon`]
/// calls. It treats:
///   * `BARISTA_MAVEN_HOME_OVERRIDE` as the explicit override (an escape
///     hatch a future `--maven-home` flag would also feed),
///   * `BARISTA_MAVEN_HOME` as the env-var tier,
///   * `std::env::current_exe()` (canonicalized) as the bundled-probe root.
///
/// Empty-string env vars are treated as unset (matching barback's
/// `EmbeddedMavenFactory.resolveMavenHome`, which ignores empty values).
pub fn resolve_maven_home_from_env() -> ResolvedMavenHome {
    let non_empty = |k: &str| -> Option<PathBuf> {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let override_home = non_empty("BARISTA_MAVEN_HOME_OVERRIDE");
    let env_home = non_empty(MAVEN_HOME_ENV);
    // Canonicalize the exe path so `..`/symlink shenanigans in the launch
    // path don't defeat the grandparent derivation. Fall back to the
    // non-canonical path if canonicalization fails (e.g. a deleted exe on
    // Linux); the bundled probe will simply not validate in that case.
    let exe = std::env::current_exe()
        .ok()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p));
    resolve_maven_home(override_home, env_home, exe.as_deref(), &RealFs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A scripted filesystem probe: a set of paths that "are directories"
    /// and a set that "are files". Everything else is absent.
    struct FakeFs {
        dirs: HashSet<PathBuf>,
        files: HashSet<PathBuf>,
    }

    impl FakeFs {
        fn new() -> Self {
            Self {
                dirs: HashSet::new(),
                files: HashSet::new(),
            }
        }
        fn with_dir(mut self, p: impl Into<PathBuf>) -> Self {
            self.dirs.insert(p.into());
            self
        }
        fn with_file(mut self, p: impl Into<PathBuf>) -> Self {
            self.files.insert(p.into());
            self
        }
    }

    impl FsProbe for FakeFs {
        fn is_dir(&self, p: &Path) -> bool {
            self.dirs.contains(p)
        }
        fn is_file(&self, p: &Path) -> bool {
            self.files.contains(p)
        }
    }

    /// Build the `<root>/share/barista/maven-4` path for assertions.
    fn maven_dir(root: &str) -> PathBuf {
        let mut p = PathBuf::from(root);
        for c in BUNDLED_MAVEN_REL {
            p.push(c);
        }
        p
    }

    #[test]
    fn bundled_home_found_for_well_formed_install() {
        // <root>/bin/barista  ->  install root is <root>
        let exe = Path::new("/opt/barista/bin/barista");
        let mvn_home = maven_dir("/opt/barista");
        let fs = FakeFs::new()
            .with_dir(mvn_home.clone())
            .with_file(mvn_home.join("bin").join("mvn"))
            .with_dir(mvn_home.join("lib"));
        assert_eq!(bundled_maven_home(exe, &fs), Some(mvn_home));
    }

    #[test]
    fn bundled_home_accepts_windows_launcher() {
        let exe = Path::new("/opt/barista/bin/barista");
        let mvn_home = maven_dir("/opt/barista");
        // No `bin/mvn`, but `bin/mvn.cmd` present → still a Maven home.
        let fs = FakeFs::new()
            .with_dir(mvn_home.clone())
            .with_file(mvn_home.join("bin").join("mvn.cmd"))
            .with_dir(mvn_home.join("lib"));
        assert_eq!(bundled_maven_home(exe, &fs), Some(mvn_home));
    }

    #[test]
    fn bundled_home_none_when_maven_dir_missing() {
        let exe = Path::new("/opt/barista/bin/barista");
        // The maven-4 dir simply does not exist (an un-bundled / dev build).
        let fs = FakeFs::new();
        assert_eq!(bundled_maven_home(exe, &fs), None);
    }

    #[test]
    fn bundled_home_none_when_launcher_missing() {
        // Directory present and lib/ present, but no bin/mvn → reject; the
        // probe must not accept an arbitrary non-Maven directory.
        let exe = Path::new("/opt/barista/bin/barista");
        let mvn_home = maven_dir("/opt/barista");
        let fs = FakeFs::new()
            .with_dir(mvn_home.clone())
            .with_dir(mvn_home.join("lib"));
        assert_eq!(bundled_maven_home(exe, &fs), None);
    }

    #[test]
    fn bundled_home_none_when_lib_missing() {
        // bin/mvn present but no lib/ → reject.
        let exe = Path::new("/opt/barista/bin/barista");
        let mvn_home = maven_dir("/opt/barista");
        let fs = FakeFs::new()
            .with_dir(mvn_home.clone())
            .with_file(mvn_home.join("bin").join("mvn"));
        assert_eq!(bundled_maven_home(exe, &fs), None);
    }

    #[test]
    fn bundled_home_none_when_exe_too_shallow() {
        // An exe with no grandparent (`/barista`) can't yield an install
        // root; resolve to None rather than panicking.
        let exe = Path::new("/barista");
        let fs = FakeFs::new();
        assert_eq!(bundled_maven_home(exe, &fs), None);
    }

    #[test]
    fn precedence_override_beats_everything() {
        // Even with a valid env value AND a valid bundled layout, the
        // explicit override wins.
        let exe = Path::new("/opt/barista/bin/barista");
        let mvn_home = maven_dir("/opt/barista");
        let fs = FakeFs::new()
            .with_dir(mvn_home.clone())
            .with_file(mvn_home.join("bin").join("mvn"))
            .with_dir(mvn_home.join("lib"));
        let resolved = resolve_maven_home(
            Some(PathBuf::from("/explicit/override")),
            Some(PathBuf::from("/from/env")),
            Some(exe),
            &fs,
        );
        assert_eq!(resolved.source, MavenHomeSource::Override);
        assert_eq!(resolved.path, Some(PathBuf::from("/explicit/override")));
    }

    #[test]
    fn precedence_env_beats_bundled() {
        let exe = Path::new("/opt/barista/bin/barista");
        let mvn_home = maven_dir("/opt/barista");
        let fs = FakeFs::new()
            .with_dir(mvn_home.clone())
            .with_file(mvn_home.join("bin").join("mvn"))
            .with_dir(mvn_home.join("lib"));
        let resolved =
            resolve_maven_home(None, Some(PathBuf::from("/from/env")), Some(exe), &fs);
        assert_eq!(resolved.source, MavenHomeSource::Env);
        assert_eq!(resolved.path, Some(PathBuf::from("/from/env")));
    }

    #[test]
    fn precedence_bundled_when_no_override_or_env() {
        let exe = Path::new("/opt/barista/bin/barista");
        let mvn_home = maven_dir("/opt/barista");
        let fs = FakeFs::new()
            .with_dir(mvn_home.clone())
            .with_file(mvn_home.join("bin").join("mvn"))
            .with_dir(mvn_home.join("lib"));
        let resolved = resolve_maven_home(None, None, Some(exe), &fs);
        assert_eq!(resolved.source, MavenHomeSource::Bundled);
        assert_eq!(resolved.path, Some(mvn_home));
    }

    #[test]
    fn precedence_none_when_nothing_resolves() {
        // No override, no env, and no bundled layout (empty fs) → None.
        let exe = Path::new("/opt/barista/bin/barista");
        let fs = FakeFs::new();
        let resolved = resolve_maven_home(None, None, Some(exe), &fs);
        assert_eq!(resolved.source, MavenHomeSource::None);
        assert_eq!(resolved.path, None);
    }

    #[test]
    fn source_labels_are_stable() {
        assert_eq!(MavenHomeSource::Override.label(), "override");
        assert_eq!(MavenHomeSource::Env.label(), "env");
        assert_eq!(MavenHomeSource::Bundled.label(), "bundled");
        assert_eq!(MavenHomeSource::None.label(), "none");
    }
}
