// SPDX-License-Identifier: MIT OR Apache-2.0

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
//! 6. Hardlink the fetched artifacts into `~/.m2/repository` so any
//!    embedded Maven core (the barback daemon's classloader chain)
//!    can find them on the conventional Maven layout.
//! 7. Write the resulting `barista.lock` atomically.
//!
//! # `--no-fetch`
//!
//! `barista pull --no-fetch` short-circuits the full pipeline: it
//! parses the project root + an existing `barista.lock` (if any)
//! and reports the entry count. Useful as a "does this project even
//! parse?" smoke test or as a CI gate ahead of the full fetch.
//!
//! # `--frozen`
//!
//! When the global `--frozen` flag is set (directly or via the
//! `--ci` macro), the full-fetch path computes the project signature
//! and compares it against the on-disk lockfile's
//! `meta.project_signature`. A mismatch produces a
//! [`PullError::FrozenSignatureMismatch`] without writing anything.
//! A match short-circuits the resolve+fetch pass: the on-disk
//! lockfile is treated as authoritative.
//!
//! # `--update`
//!
//! `barista pull --update` ignores the on-disk lockfile and always
//! re-resolves + re-fetches + rewrites the lockfile. Idempotent on
//! a clean tree: re-running on the same source produces the same
//! lockfile bytes (modulo the `meta.generated_at` timestamp).
//!
//! # Output
//!
//! The hand-rolled `eprintln!("pull: …")` path of M3.1 has been
//! replaced with the structured-output pipeline in [`crate::output`].
//! [`run`] builds a [`PullReport`] and dispatches it through a
//! renderer chosen by `--output`. Human format prints the same
//! `pull: <summary>` line to stderr; JSON / NDJSON emit a structured
//! document on stdout.

use std::path::{Path, PathBuf};

use barista_cache::cas::{Cas, ContentHash};
use barista_cache::checksum::{self, Verification};
use barista_cache::fetch::{ConditionalHeaders, FetchConfig, FetchError, FetchOutcome, Fetcher};
use barista_cache::index::{Index, IndexEntry, IndexKey, Origin};
use barista_cache::source::CacheSource;
use barista_config::{Config, LoadAudit, LoaderError, LoaderInputs, load_effective_config};
use barista_coords::Coords;
use barista_lockfile::signature::{ReactorModule, compute_signature};
use barista_lockfile::{Lockfile, LockfileEntry, LockfileError};
use barista_pom::effective::ParentResolver;
use barista_pom::profile::{ActivationContext, ResolveError as PomResolveError, resolve_pom};
use barista_pom::raw::{ParseError as PomParseError, RawParent, RawPom, parse_pom};
use barista_resolver::source::MetadataSource;
use barista_resolver::walker::{ResolvedDep, Scope as WalkScope, WalkOptions, walk};

use crate::cli::{GlobalFlags, PullArgs, ScopeArg};
use crate::output::make_runtime_renderer;
use crate::output::progress::{ProgressSink, make_runtime_progress_sink};
use crate::output::report::{LockfileStatus, PullReport};
use crate::project::{ResolveError, ResolveInputs, resolve_project_root};

/// Run `barista pull`.
///
/// Returns the process exit code:
///
/// - `0` on success.
/// - `1` on a recoverable, user-facing error (bad project root,
///   missing pom.xml, unparseable lockfile, fetch failure, etc.).
/// - `2` reserved for `--frozen` signature mismatch — the on-disk
///   lockfile is stale relative to the source tree.
pub fn run(global: &GlobalFlags, args: &PullArgs) -> i32 {
    let mut renderer = make_runtime_renderer(global);
    // Per-format progress sink. NDJSON streams real events; JSON and
    // Human currently get the NullSink / HumanSink no-op (see
    // `crate::output::progress` for the rationale).
    let mut sink = make_runtime_progress_sink(global);
    let exit = match run_inner(global, args, sink.as_mut()) {
        Ok(report) => {
            // `--quiet` suppresses the human-readable summary only:
            // JSON / NDJSON consumers (and the `--ci` shortcut) need
            // the structured document regardless. The renderer's per-
            // format impl knows whether to short-circuit; pull just
            // always asks it to render and the human renderer is the
            // one that respects `--quiet`.
            let should_render = match global.output {
                crate::cli::OutputFormat::Human => !global.quiet,
                crate::cli::OutputFormat::Json | crate::cli::OutputFormat::Ndjson => true,
            };
            if should_render {
                if let Err(e) = renderer.render_pull(&report) {
                    eprintln!("error: rendering pull report failed: {e}");
                    return 1;
                }
            }
            0
        }
        Err(PullError::FrozenSignatureMismatch { on_disk, computed }) => {
            let err = PullError::FrozenSignatureMismatch { on_disk, computed };
            if matches!(global.output, crate::cli::OutputFormat::Human) {
                eprintln!("error: barista pull --frozen: {err}");
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
///
/// `sink` receives streaming progress events. Pass [`NullSink`] when
/// you don't care; the production [`run`] entry point picks the
/// right sink for the active `--output` format.
pub fn run_inner(
    global: &GlobalFlags,
    args: &PullArgs,
    sink: &mut dyn ProgressSink,
) -> Result<PullReport, PullError> {
    sink.started("pull");

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
    let (config, _audit): (Config, LoadAudit) = load_effective_config(LoaderInputs {
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

    // Now that we have a project coordinate, surface it on the stream.
    let project_coord = pom_coords_for_summary(&raw_pom);
    sink.resolving(Some(&project_coord), None);

    // -- 4. The two paths --------------------------------------------------
    if args.no_fetch {
        let report = run_no_fetch(&root.root, &raw_pom, global, args, sink)?;
        sink.completed("pull");
        return Ok(report);
    }

    let report = run_full_fetch(&root.root, raw_pom, &config, global, args, sink)?;
    sink.completed("pull");
    Ok(report)
}

/// Full-fetch path: resolve, fetch, materialize ~/.m2, write lockfile.
///
/// The pipeline:
///
///   1. Build the cache stack: [`Cas`] + [`Index`] +
///      [`Fetcher`] + [`CacheSource`] rooted at `config.paths.cache_dir`.
///   2. Run [`resolve_pom`] against a [`CacheSourceParentResolver`]
///      so a `<parent>` in the root POM is fetched from upstream.
///   3. Compute the project signature over the resolved effective
///      POM. With `--frozen`, compare against any on-disk lockfile
///      and short-circuit on match (or error on mismatch). Without
///      `--frozen` + on a match, the on-disk lockfile is
///      authoritative — return its entry count without re-walking.
///   4. Walk the dependency graph (single-module v0.1 scope).
///   5. For each unique `(coords, version, classifier, type_)`,
///      fetch the JAR via the cache infrastructure: hit-or-fetch in
///      the [`Cas`], verify sidecar checksums, record the
///      [`IndexEntry`].
///   6. Hardlink each CAS blob into `~/.m2/repository` (the daemon
///      pour step expects them there).
///   7. Construct a [`Lockfile`] and write it atomically to
///      `<project_root>/barista.lock`.
fn run_full_fetch(
    project_root: &Path,
    raw_pom: RawPom,
    config: &Config,
    global: &GlobalFlags,
    args: &PullArgs,
    sink: &mut dyn ProgressSink,
) -> Result<PullReport, PullError> {
    // -- 1. Open the cache stack -----------------------------------------
    let cache_root = &config.paths.cache_dir;
    std::fs::create_dir_all(cache_root).map_err(|source| PullError::Io {
        path: cache_root.clone(),
        source,
    })?;
    let cas = Cas::open(cache_root).map_err(|e| PullError::CacheOpen {
        path: cache_root.clone(),
        detail: e.to_string(),
    })?;
    // Use the recovery-aware opener (M2.3 T10): a torn journal tail
    // (e.g. an earlier process killed mid-append, or — empirically —
    // a long sequence of back-to-back `pull --update` invocations
    // that ended up leaving the journal in a partial state) is
    // repaired by truncating to the last known-good record. The
    // strict `Index::open` would have failed exit 1 here with
    // `journal at ... ends mid-record (truncation detected)`. The
    // truncation event is surfaced to the user via stderr so they
    // know the cache self-healed.
    let (index, open_report) =
        Index::open_with_recovery(cache_root).map_err(|e| PullError::IndexOpen {
            path: cache_root.clone(),
            detail: e.to_string(),
        })?;
    if open_report.journal_truncated {
        eprintln!(
            "barista: warning: cache index journal at {} had a torn tail; \
             truncated at byte offset {} and continuing.",
            cache_root.join("index").join("journal.log").display(),
            open_report.journal_truncated_at.unwrap_or(0),
        );
    }
    // The `BARISTA_TEST_UPSTREAM_URL` env var lets integration
    // tests (cmd_pull_full_fetch.rs) point the fetcher at a
    // wiremock server. Production resolves to Maven Central.
    // `http2_enabled = false` when the override is set so the
    // fetcher can talk to an HTTP/1.1-only mock server.
    let (default_upstream, http2_enabled) = match std::env::var("BARISTA_TEST_UPSTREAM_URL") {
        Ok(url) if !url.is_empty() => (url, false),
        _ => (
            "https://repo.maven.apache.org/maven2".to_string(),
            config.network.http2_enabled,
        ),
    };
    let fetch_config = FetchConfig {
        max_concurrent_connections: config.network.max_concurrent_connections.max(1),
        request_timeout: std::time::Duration::from_secs(
            config.network.request_timeout_secs.max(1) as u64
        ),
        http2_enabled,
        user_agent: concat!("barista/", env!("CARGO_PKG_VERSION")).to_string(),
        default_upstream,
    };
    let fetcher = Fetcher::new(fetch_config.clone()).map_err(|e| PullError::Fetcher {
        detail: e.to_string(),
    })?;
    let cache_source = CacheSource::new(
        cas.clone(),
        index.clone(),
        fetcher.clone(),
        cache_root.clone(),
        config.maven.snapshot_update_policy,
        config.maven.release_update_policy,
    );

    // The walker (and resolve_pom for parent chains) is async-first;
    // build a current-thread runtime that's shared across the
    // parent-resolver adapter and the walker.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| PullError::Runtime {
            detail: format!("tokio current-thread runtime build: {e}"),
        })?;

    // -- 2. Effective POM (parent resolution) ----------------------------
    let mut parent_resolver = CacheSourceParentResolver::new(&runtime, &cache_source);
    let resolved_root = resolve_pom(
        raw_pom.clone(),
        &mut parent_resolver,
        &ActivationContext::default(),
    )?;

    // -- 3. Project signature + --frozen ---------------------------------
    let signature = compute_signature(&[ReactorModule {
        group_id: resolved_root
            .pom
            .group_id
            .clone()
            .or_else(|| {
                resolved_root
                    .pom
                    .parent
                    .as_ref()
                    .map(|p| p.group_id.clone())
            })
            .unwrap_or_default(),
        artifact_id: resolved_root.pom.artifact_id.clone(),
        pom: resolved_root.pom.clone(),
    }])
    .map_err(|e| PullError::Signature {
        detail: e.to_string(),
    })?;
    let lock_path = project_root.join("barista.lock");
    let on_disk: Option<Lockfile> = if lock_path.exists() {
        Some(Lockfile::read(&lock_path)?)
    } else {
        None
    };

    // `--frozen` is authoritative: signature mismatch → hard error.
    // `--update` ignores any on-disk lockfile entirely.
    // Default: signature match → the on-disk lockfile is authoritative
    // (no resolve + no fetch); mismatch → re-resolve and overwrite.
    if global.frozen
        && let Some(lf) = &on_disk
        && lf.meta.project_signature != signature
    {
        return Err(PullError::FrozenSignatureMismatch {
            on_disk: lf.meta.project_signature.clone(),
            computed: signature,
        });
    }
    let lockfile_was_authoritative = !args.update
        && on_disk
            .as_ref()
            .is_some_and(|lf| lf.meta.project_signature == signature);
    if lockfile_was_authoritative {
        // Authoritative: the lockfile already pins this exact source
        // tree. Don't re-walk or re-fetch — just emit a report that
        // matches what the existing lockfile says.
        // SAFETY: matched in lockfile_was_authoritative ⇒ on_disk is Some.
        let lf = on_disk.expect("on_disk is Some when lockfile_was_authoritative");
        for entry in &lf.entries {
            sink.cached(&entry.coords);
        }
        return Ok(PullReport {
            project_root: project_root.to_path_buf(),
            lockfile_status: LockfileStatus::Unchanged,
            entries: lf.entries.len(),
            fetched: 0,
            project_signature: Some(short_sig(&signature)),
            coords: Some(pom_coords_for_summary(&raw_pom)),
            no_fetch: false,
            strict: global.strict,
        });
    }

    // -- 4. Walk the dependency graph ------------------------------------
    let walk_opts = WalkOptions {
        include_scopes: scope_filter(args.scope),
        ..WalkOptions::default()
    };
    let graph = runtime
        .block_on(walk(&resolved_root, &cache_source, &walk_opts))
        .map_err(|e| PullError::Resolve {
            detail: e.to_string(),
        })?;

    // -- 5. Fetch every JAR ----------------------------------------------
    let mut entries: Vec<LockfileEntry> = Vec::with_capacity(graph.resolved.len());
    let mut fetched_count: u64 = 0;
    for dep in &graph.resolved {
        let coord_display = format!(
            "{}:{}:{}",
            dep.coords.group, dep.coords.artifact, dep.version
        );
        sink.fetching(&coord_display, None);
        let outcome = runtime
            .block_on(fetch_artifact_to_cache(
                &cas,
                &index,
                &fetcher,
                &dep.coords,
                &dep.version,
                &dep.type_,
                dep.classifier.as_deref(),
            ))
            .map_err(|e| PullError::FetchArtifact {
                coord: coord_display.clone(),
                detail: e.to_string(),
            })?;
        if matches!(outcome.source, FetchSource::Remote) {
            fetched_count += 1;
            sink.fetched(&coord_display);
        } else {
            sink.cached(&coord_display);
        }
        entries.push(build_lockfile_entry(dep, &outcome));
    }

    // -- 6. Hardlink every fetched artifact into ~/.m2 -------------------
    //
    // The daemon's pour step (pre-verify) expects artifacts at the
    // conventional Maven layout under ~/.m2/repository so the
    // embedded Maven core can resolve them. We mirror them here so
    // `barista pull && barista verify` works without an intermediate
    // `barista pour`.
    let m2_root = &config.paths.m2_repository;
    for (dep, entry) in graph.resolved.iter().zip(entries.iter()) {
        let hash = ContentHash::from_hex(&entry.sha256).map_err(|e| PullError::CasMaterialize {
            coord: format_coord(entry),
            detail: e.to_string(),
        })?;
        barista_cache::m2::materialize(
            &cas,
            &hash,
            m2_root,
            &dep.coords,
            &dep.version,
            dep.classifier.as_deref(),
            &dep.type_,
        )
        .map_err(|e| PullError::CasMaterialize {
            coord: format_coord(entry),
            detail: e.to_string(),
        })?;
    }

    // -- 7. Write barista.lock -------------------------------------------
    sink.writing_lockfile();
    let mut lockfile = Lockfile::new(signature.clone(), settings_fingerprint(config));
    lockfile.entries = entries;
    lockfile.write(&lock_path)?;

    let status = match on_disk.as_ref() {
        // The on-disk and the freshly-computed lockfiles differ at
        // most in `meta.generated_at` (a wall-clock timestamp) when
        // resolution itself converged on the same answer. We compare
        // the durable subset (entries + reactor + settings_snapshot +
        // project_signature + settings_fingerprint) — everything but
        // the timestamp — so a no-op re-run reports `Unchanged`.
        Some(prev) if lockfiles_equal_ignoring_timestamp(prev, &lockfile) => {
            LockfileStatus::Unchanged
        }
        Some(_) | None => LockfileStatus::Written,
    };

    Ok(PullReport {
        project_root: project_root.to_path_buf(),
        lockfile_status: status,
        entries: lockfile.entries.len(),
        fetched: fetched_count.try_into().unwrap_or(usize::MAX),
        project_signature: Some(short_sig(&signature)),
        coords: Some(pom_coords_for_summary(&raw_pom)),
        no_fetch: false,
        strict: global.strict,
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
    sink: &mut dyn ProgressSink,
) -> Result<PullReport, PullError> {
    let coords = pom_coords_for_summary(raw_pom);
    let lock_path = project_root.join("barista.lock");
    let (status, entries, signature) = if lock_path.exists() {
        let lf = Lockfile::read(&lock_path)?;
        // Per-coord streaming. The schema requires `coord` on
        // `cached`; we pass it borrowed from the entry so the loop
        // doesn't allocate. We flush every 64 events to keep the
        // pipe moving on long streams without paying a syscall per
        // coord. (At 500 entries that's ~8 flushes, not 500.)
        for (i, entry) in lf.entries.iter().enumerate() {
            sink.cached(&entry.coords);
            if (i + 1) % 64 == 0 {
                sink.flush();
            }
        }
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

    #[error("opening cache CAS at {path:?}: {detail}")]
    CacheOpen { path: PathBuf, detail: String },

    #[error("opening cache index at {path:?}: {detail}")]
    IndexOpen { path: PathBuf, detail: String },

    #[error("building HTTP fetcher: {detail}")]
    Fetcher { detail: String },

    #[error("async runtime: {detail}")]
    Runtime { detail: String },

    #[error("computing project signature: {detail}")]
    Signature { detail: String },

    #[error("resolving dependency graph: {detail}")]
    Resolve { detail: String },

    #[error("fetching artifact {coord}: {detail}")]
    FetchArtifact { coord: String, detail: String },

    #[error("materializing {coord} into ~/.m2: {detail}")]
    CasMaterialize { coord: String, detail: String },

    #[error(
        "lockfile is stale (signature mismatch): on-disk {on_disk}, computed {computed}. \
         Re-run without `--frozen` (or with `--update`) to refresh the lockfile."
    )]
    FrozenSignatureMismatch { on_disk: String, computed: String },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Translate the CLI's `ScopeArg` into the resolver's
/// [`WalkOptions::include_scopes`] set. An empty set means "all
/// scopes except `Import`" — which is the resolver's default. We
/// build a single-element set per-scope so the resolver knows
/// exactly which scope to keep in the resolved graph.
fn scope_filter(scope: ScopeArg) -> std::collections::BTreeSet<WalkScope> {
    let mut s = std::collections::BTreeSet::new();
    let single = match scope {
        ScopeArg::Compile => WalkScope::Compile,
        ScopeArg::Runtime => WalkScope::Runtime,
        ScopeArg::Test => WalkScope::Test,
        ScopeArg::Provided => WalkScope::Provided,
        ScopeArg::System => WalkScope::System,
    };
    s.insert(single);
    s
}

/// Compose a `group:artifact:version[:classifier]` form for logs.
fn format_coord(e: &LockfileEntry) -> String {
    match e.classifier.as_deref() {
        Some(c) => format!("{}:{}:{}:{}", e.coords, e.version, c, e.type_),
        None => format!("{}:{}", e.coords, e.version),
    }
}

/// Map a [`WalkScope`] back to its lockfile string form.
fn scope_to_str(s: WalkScope) -> &'static str {
    match s {
        WalkScope::Compile => "compile",
        WalkScope::Provided => "provided",
        WalkScope::Runtime => "runtime",
        WalkScope::Test => "test",
        WalkScope::System => "system",
        WalkScope::Import => "import",
    }
}

/// Source from which an artifact's bytes were satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchSource {
    /// Already present in the CAS — no network I/O.
    Cache,
    /// Fetched from the upstream repository this run.
    Remote,
}

/// Outcome of fetching a single artifact: the bytes are in CAS;
/// these are the pieces the lockfile needs to record.
struct FetchedArtifact {
    sha256: ContentHash,
    sha1: Option<String>,
    size_bytes: u64,
    source_url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    source: FetchSource,
}

/// Fetch (or look up) one artifact's bytes by `(coords, version,
/// type, classifier)` and ensure they land in the CAS + index.
///
/// On a cache hit, no network I/O is performed and the
/// pre-existing [`IndexEntry`]'s metadata is returned. On a miss,
/// fetches from the configured upstream, verifies sidecar
/// checksums via [`checksum::verify`], puts the bytes into the CAS,
/// and records a new index entry.
///
/// This is a CLI-local re-implementation of the `CacheSource`'s
/// private `fetch_and_cache` path, exposed because the cache crate
/// only publishes POM/metadata fetches on its public surface. The
/// guarantee — once this returns Ok, the CAS contains the artifact
/// addressed by the returned SHA-256 — is identical to what
/// `CacheSource::fetch_pom` provides for POMs.
async fn fetch_artifact_to_cache(
    cas: &Cas,
    index: &Index,
    fetcher: &Fetcher,
    coords: &Coords,
    version: &str,
    type_: &str,
    classifier: Option<&str>,
) -> Result<FetchedArtifact, ArtifactFetchError> {
    let key = IndexKey::new(coords.clone(), version, type_, classifier.map(String::from));
    if let Some(entry) = index.get(&key)
        && cas.contains(&entry.hash)
    {
        return Ok(FetchedArtifact {
            sha256: entry.hash,
            sha1: entry.sha1_hex,
            size_bytes: entry.size_bytes,
            source_url: entry.origin.repository_url,
            etag: entry.origin.etag,
            last_modified: entry.origin.last_modified,
            source: FetchSource::Cache,
        });
    }

    let url = fetcher.url_for_artifact(
        None,
        &coords.group,
        &coords.artifact,
        version,
        classifier,
        type_,
    );
    let sha256_url = fetcher.url_for_sidecar(&url, "sha256");
    let sha1_url = fetcher.url_for_sidecar(&url, "sha1");

    let empty = ConditionalHeaders::default();
    let (artifact, sha256_sidecar, sha1_sidecar) = tokio::join!(
        fetcher.fetch(&url, &empty),
        fetcher.fetch(&sha256_url, &empty),
        fetcher.fetch(&sha1_url, &empty),
    );
    let artifact = artifact.map_err(ArtifactFetchError::Fetch)?;
    let (bytes, etag, last_modified) = match artifact {
        FetchOutcome::Fresh {
            bytes,
            etag,
            last_modified,
            ..
        } => (bytes, etag, last_modified),
        FetchOutcome::NotModified => {
            // We never sent conditional headers; treat as a transport
            // anomaly.
            return Err(ArtifactFetchError::UnexpectedNotModified { url: url.clone() });
        }
    };
    let sha256_text = match sha256_sidecar {
        Ok(FetchOutcome::Fresh { bytes, .. }) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        _ => None,
    };
    let sha1_text = match sha1_sidecar {
        Ok(FetchOutcome::Fresh { bytes, .. }) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        _ => None,
    };

    let verification = checksum::verify(&bytes, sha256_text.as_deref(), sha1_text.as_deref())
        .map_err(|e| ArtifactFetchError::Checksum {
            url: url.clone(),
            detail: e.to_string(),
        })?;

    let (hash, _path) = cas.put(&bytes).map_err(|e| ArtifactFetchError::Cas {
        detail: e.to_string(),
    })?;

    let sha1_hex = match &verification {
        Verification::Sha1Verified { hex } => Some(hex.clone()),
        _ => None,
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = IndexEntry {
        hash,
        size_bytes: bytes.len() as u64,
        sha1_hex: sha1_hex.clone(),
        origin: Origin {
            repository_url: url.clone(),
            etag: etag.clone(),
            last_modified: last_modified.clone(),
            upstream_last_updated: None,
            tier: Default::default(),
        },
        atime_unix: now,
        created_unix: now,
    };
    index
        .put(key, entry)
        .map_err(|e| ArtifactFetchError::Index {
            detail: e.to_string(),
        })?;

    Ok(FetchedArtifact {
        sha256: hash,
        sha1: sha1_hex,
        size_bytes: bytes.len() as u64,
        source_url: url,
        etag,
        last_modified,
        source: FetchSource::Remote,
    })
}

#[derive(Debug, thiserror::Error)]
enum ArtifactFetchError {
    #[error("upstream HTTP: {0}")]
    Fetch(#[source] FetchError),
    #[error("upstream returned 304 Not Modified without conditional headers: {url}")]
    UnexpectedNotModified { url: String },
    #[error("checksum verification failed at {url}: {detail}")]
    Checksum { url: String, detail: String },
    #[error("cas put: {detail}")]
    Cas { detail: String },
    #[error("index put: {detail}")]
    Index { detail: String },
}

/// Build the per-artifact lockfile entry from a resolver output +
/// fetcher outcome.
fn build_lockfile_entry(dep: &ResolvedDep, fetched: &FetchedArtifact) -> LockfileEntry {
    LockfileEntry {
        coords: format!("{}:{}", dep.coords.group, dep.coords.artifact),
        version: dep.version.clone(),
        scope: scope_to_str(dep.scope).to_string(),
        optional: dep.optional,
        sha256: fetched.sha256.to_hex(),
        sha1: fetched.sha1.clone(),
        size_bytes: fetched.size_bytes,
        source_url: fetched.source_url.clone(),
        etag: fetched.etag.clone(),
        last_modified: fetched.last_modified.clone(),
        classifier: dep.classifier.clone(),
        type_: dep.type_.clone(),
        from_path: dep
            .winning_path
            .iter()
            .map(|c| format!("{}:{}", c.group, c.artifact))
            .collect(),
        depth: dep.depth,
        snapshot_resolution: None,
        exclusions: Vec::new(),
    }
}

/// Compute the `settings_fingerprint` for the lockfile meta block.
/// v0.1 keeps this minimal: a SHA-256 of the effective Maven
/// settings.xml repositories + mirrors, in declaration order. When
/// no settings are configured this collapses to the SHA-256 of the
/// empty byte string — stable across runs.
fn settings_fingerprint(_config: &Config) -> String {
    use sha2::{Digest, Sha256};
    let hasher = Sha256::new();
    // v0.1: nothing in settings.xml actually influences resolution
    // outcomes (we always go to Maven Central). When mirror /
    // repository plumbing lands, this becomes a hash over the
    // declared mirrors + repos in canonical order.
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// True iff two lockfiles agree on every field except the wall-
/// clock timestamp in `meta.generated_at`. Used to decide between
/// `LockfileStatus::Unchanged` and `LockfileStatus::Written` on a
/// rewrite.
fn lockfiles_equal_ignoring_timestamp(a: &Lockfile, b: &Lockfile) -> bool {
    if a.meta.schema_version != b.meta.schema_version
        || a.meta.project_signature != b.meta.project_signature
        || a.meta.settings_fingerprint != b.meta.settings_fingerprint
    {
        return false;
    }
    if a.reactor != b.reactor
        || a.entries != b.entries
        || a.settings_snapshot != b.settings_snapshot
    {
        return false;
    }
    true
}

/// [`ParentResolver`] backed by a [`CacheSource`].
///
/// When `resolve_pom` walks a `<parent>` chain it does so
/// synchronously through the [`ParentResolver`] trait; our cache
/// layer is async-first. This adapter bridges the gap by
/// `block_on`-ing each parent fetch through a tokio current-thread
/// runtime handle the caller owns. The runtime is **not** entered
/// recursively — `resolve_pom` is called from outside any tokio
/// task, so `block_on` here is the entry point.
struct CacheSourceParentResolver<'a> {
    runtime: &'a tokio::runtime::Runtime,
    source: &'a CacheSource,
}

impl<'a> CacheSourceParentResolver<'a> {
    fn new(runtime: &'a tokio::runtime::Runtime, source: &'a CacheSource) -> Self {
        Self { runtime, source }
    }
}

impl ParentResolver for CacheSourceParentResolver<'_> {
    fn resolve(&mut self, parent: &RawParent) -> Result<RawPom, String> {
        let coords = Coords::new(&parent.group_id, &parent.artifact_id)
            .map_err(|e| format!("invalid parent coords: {e}"))?;
        let version = parent.version.clone();
        let fut = self.source.fetch_pom(&coords, &version);
        match self.runtime.block_on(fut) {
            Ok((pom, _origin)) => Ok(pom),
            Err(e) => Err(format!(
                "failed to fetch parent {}:{}:{}: {}",
                parent.group_id, parent.artifact_id, parent.version, e
            )),
        }
    }
}
