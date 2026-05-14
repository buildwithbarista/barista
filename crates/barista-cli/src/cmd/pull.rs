//! `barista pull` — resolve dependencies, fetch artifacts, write
//! `barista.lock`.
//!
//! This is Barista's flagship value-add verb. The end-to-end shape:
//!
//! 1. Resolve the project root (CWD walk-up, `--root`, or `-f`).
//! 2. Load the layered effective config (defaults → user → project →
//!    `settings.xml` → env → CLI).
//! 3. Read and parse the root `pom.xml`; produce an effective POM
//!    with parent merge, interpolation, BOM imports, profile
//!    activation, and `<dependencyManagement>` applied.
//! 4. Resolve the dependency graph against a [`MetadataSource`].
//! 5. Fetch each resolved artifact into the content-addressed
//!    cache; record sha256 / sha1 / size / source URL.
//! 6. Write the resulting `barista.lock` atomically.
//!
//! **v0.1 scope.** Only the `--no-fetch` path is fully wired
//! end-to-end. The full-fetch path requires a configured cache
//! root + a reachable upstream (Maven Central or a configured
//! mirror); it returns a structured [`PullError::NotYetImplemented`]
//! error pointing the caller at `--no-fetch`. Wiring the real fetch
//! path is a v0.2 effort tracked alongside the M3.x cache-pipeline
//! tasks.
//!
//! With `--no-fetch`, `barista pull` exercises the
//! project-root / config / POM-parse pipeline and validates an
//! existing `barista.lock` if one is present — enough to be useful
//! as a "does this project even parse?" smoke test.
//!
//! # Output
//!
//! The hand-rolled `eprintln!("pull: …")` path of M3.1 has been
//! replaced with the structured-output pipeline in [`crate::output`].
//! [`run`] builds a [`PullReport`] and dispatches it through a
//! renderer chosen by `--output`. Human format prints the same
//! `pull: <summary>` line to stderr; JSON / NDJSON emit a structured
//! document on stdout.

use barista_config::{Config, LoadAudit, LoaderError, LoaderInputs, load_effective_config};
use barista_lockfile::{Lockfile, LockfileError};
use barista_pom::effective::ParentResolver;
use barista_pom::profile::{ActivationContext, ResolveError as PomResolveError, resolve_pom};
use barista_pom::raw::{ParseError as PomParseError, RawParent, RawPom, parse_pom};

use crate::cli::{GlobalFlags, PullArgs};
use crate::output::report::{LockfileStatus, PullReport};
use crate::output::make_runtime_renderer;
use crate::project::{ResolveError, ResolveInputs, resolve_project_root};

/// Run `barista pull`.
///
/// Returns the process exit code:
///
/// - `0` on success.
/// - `1` on a recoverable, user-facing error (bad project root,
///   missing pom.xml, unparseable lockfile, etc.).
/// - `2` reserved for "not yet implemented" — the full-fetch path
///   when called without `--no-fetch`.
pub fn run(global: &GlobalFlags, args: &PullArgs) -> i32 {
    let mut renderer = make_runtime_renderer(global);
    let exit = match run_inner(global, args) {
        Ok(report) => {
            // Render the report only when not in `--quiet`. JSON/NDJSON
            // consumers typically want the document even under
            // `--quiet`, but mirroring the pre-renderer behaviour for
            // M3.2 T1 keeps the snapshot suite stable; T3 revisits.
            if !global.quiet {
                if let Err(e) = renderer.render_pull(&report) {
                    eprintln!("error: rendering pull report failed: {e}");
                    return 1;
                }
            }
            0
        }
        Err(PullError::NotYetImplemented { detail }) => {
            // Keep the pre-existing direct-stderr message for the
            // stub path so M3.x tooling that greps for it doesn't
            // change. Renderer is still consulted for json/ndjson.
            let err = PullError::NotYetImplemented { detail };
            if matches!(global.output, crate::cli::OutputFormat::Human) {
                eprintln!(
                    "barista: pull (full-fetch path) is not yet wired in this build: {err}"
                );
            } else if let Err(re) = renderer.render_error(&err) {
                eprintln!("error: rendering error report failed: {re}");
            }
            2
        }
        Err(e) => {
            if matches!(global.output, crate::cli::OutputFormat::Human) {
                eprintln!("error: barista pull failed: {e}");
            } else if let Err(re) = renderer.render_error(&e) {
                eprintln!("error: rendering error report failed: {re}");
            }
            1
        }
    };
    if let Err(e) = renderer.finish() {
        eprintln!("error: flushing output failed: {e}");
        return 1;
    }
    exit
}

/// Library-friendly entry point used by [`run`] and integration
/// tests. Drives the full pipeline and returns a structured report
/// on success.
pub fn run_inner(global: &GlobalFlags, args: &PullArgs) -> Result<PullReport, PullError> {
    // -- 1. Project root ---------------------------------------------------
    let root = resolve_project_root(ResolveInputs {
        root: global.root.clone(),
        file: global.file.clone(),
        ..Default::default()
    })?;

    // -- 2. Effective config ----------------------------------------------
    //
    // The project config is conventionally at `<root>/barista.toml`. The
    // loader treats missing files as "no contribution," so handing it a
    // path that doesn't exist is fine.
    let (_config, _audit): (Config, LoadAudit) = load_effective_config(LoaderInputs {
        project_config_path: Some(root.root.join("barista.toml")),
        cwd_override: Some(root.root.clone()),
        ..Default::default()
    })?;

    // -- 3. Parse the root POM --------------------------------------------
    //
    // The raw parse is cheap and never needs the network; we run it
    // on every path so an unparseable pom.xml is surfaced before we
    // branch on `--no-fetch`.
    let pom_text = std::fs::read_to_string(&root.pom).map_err(|source| PullError::Io {
        path: root.pom.clone(),
        source,
    })?;
    let raw_pom: RawPom = parse_pom(&pom_text)?;

    // -- 4. The two paths --------------------------------------------------
    if args.no_fetch {
        return run_no_fetch(&root.root, &raw_pom, global, args);
    }

    // Full-fetch path: run the effective-POM pipeline. POMs with a
    // `<parent>` need a real network-backed resolver; v0.1 falls
    // through to `NotYetImplemented` after this stage anyway, so we
    // use a [`NullParentResolver`] that produces a clean error if
    // the POM has a parent.
    let mut null_parent = NullParentResolver;
    let _resolved = resolve_pom(raw_pom, &mut null_parent, &ActivationContext::default())?;

    // -- 5. Full-fetch path (v0.2) ----------------------------------------
    //
    // The wiring sketch lives in the module docstring. Concretely, it
    // would:
    //
    //   - Build a [`barista_cache::CacheSource`] rooted at the
    //     resolved cache dir from the loaded config.
    //   - Choose [`barista_resolver::walker::walk`] (default) or
    //     [`barista_resolver::resolve_strict`] (`--strict`).
    //   - Filter by `args.scope`.
    //   - Compute the project signature via
    //     [`barista_lockfile::compute_signature`].
    //   - Map walker output to [`barista_lockfile::LockfileEntry`]
    //     entries with sha256/sha1/size/source_url from the cache
    //     index.
    //   - Write the lockfile via [`Lockfile::write`].
    //
    // For v0.1 we bail out cleanly so users can still exercise the
    // `--no-fetch` validation path.
    let _ = args.update;
    let _ = global.strict;
    Err(PullError::NotYetImplemented {
        detail: "the network fetch path needs a configured cache root and a reachable upstream \
                 (Maven Central or a mirror). Use `--no-fetch` to validate the project + \
                 existing lockfile, or wait for the M3.x cache wiring to land."
            .to_string(),
    })
}

/// Implementation of the `--no-fetch` branch.
///
/// Reads the existing `barista.lock` (if any) and reports the entry
/// count. With no lockfile on disk, reports that none was found.
/// Either outcome is success — `--no-fetch` is a non-mutating
/// validation pass.
fn run_no_fetch(
    project_root: &std::path::Path,
    raw_pom: &RawPom,
    global: &GlobalFlags,
    args: &PullArgs,
) -> Result<PullReport, PullError> {
    let coords = pom_coords_for_summary(raw_pom);
    let lock_path = project_root.join("barista.lock");
    let (status, entries, signature) = if lock_path.exists() {
        let lf = Lockfile::read(&lock_path)?;
        let sig = short_sig(&lf.meta.project_signature);
        (LockfileStatus::Unchanged, lf.entries.len(), Some(sig))
    } else {
        (LockfileStatus::Absent, 0, None)
    };

    Ok(PullReport {
        project_root: project_root.to_path_buf(),
        lockfile_status: status,
        entries,
        fetched: 0,
        project_signature: signature,
        coords: Some(coords),
        no_fetch: args.no_fetch,
        strict: global.strict,
    })
}

/// Format the project coordinates from a raw POM, falling back to
/// the artifact-id alone when the parent supplies group/version.
fn pom_coords_for_summary(pom: &RawPom) -> String {
    let g = pom
        .group_id
        .clone()
        .or_else(|| pom.parent.as_ref().map(|p| p.group_id.clone()))
        .unwrap_or_else(|| "<no-group>".to_string());
    let v = pom
        .version
        .clone()
        .or_else(|| pom.parent.as_ref().map(|p| p.version.clone()))
        .unwrap_or_else(|| "<no-version>".to_string());
    format!("{g}:{}:{v}", pom.artifact_id)
}

/// Truncate a hex signature for human-readable summaries.
fn short_sig(sig: &str) -> String {
    if sig.len() > 12 {
        format!("{}…", &sig[..12])
    } else {
        sig.to_string()
    }
}

/// Errors surfaced from `barista pull`.
#[derive(Debug, thiserror::Error)]
pub enum PullError {
    #[error("project setup: {0}")]
    Project(#[from] ResolveError),

    #[error("config load: {0}")]
    Config(#[from] LoaderError),

    #[error("I/O at {path:?}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("pom parse: {0}")]
    PomParse(#[from] PomParseError),

    #[error("pom resolve: {0}")]
    PomResolve(#[from] PomResolveError),

    #[error("lockfile: {0}")]
    Lockfile(#[from] LockfileError),

    #[error("not yet implemented: {detail}")]
    NotYetImplemented { detail: String },
}

/// [`ParentResolver`] that always refuses to resolve a parent.
///
/// Used by `--no-fetch` (and by the v0.2 stub path) where we don't
/// have a network-backed [`barista_cache::CacheSource`] to walk a
/// real parent chain. POMs with a `<parent>` declaration produce a
/// clean [`EffectiveError::ParentResolution`] error pointing the
/// user at the configured-cache requirement.
struct NullParentResolver;

impl ParentResolver for NullParentResolver {
    fn resolve(&mut self, parent: &RawParent) -> Result<RawPom, String> {
        // The trait wants `Result<RawPom, String>`; `resolve_pom`
        // wraps the string into [`EffectiveError::ParentResolution`].
        // We surface a hint pointing the caller at `--no-fetch`.
        Err(format!(
            "parent {}:{}:{} cannot be resolved in v0.1 (no configured cache); \
             try `--no-fetch` to validate without resolving the parent chain, \
             or wait for the M3.x cache-pipeline tasks to wire CacheSource",
            parent.group_id, parent.artifact_id, parent.version
        ))
    }
}
