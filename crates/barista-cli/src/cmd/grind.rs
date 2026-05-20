// SPDX-License-Identifier: MIT OR Apache-2.0

//! `barista grind` subcommands.
//!
//! For v0.1, only `grind tree` has a real implementation. `diff`,
//! `audit`, and `why` print a "not yet implemented" message and exit
//! with code `2` (the "stub" sentinel used elsewhere in the CLI).
//!
//! `grind tree` renders the dependency graph from the on-disk
//! `barista.lock`. The lockfile is the canonical artifact for the
//! resolved graph; reading it is cheap, offline, and does not require
//! the resolver / fetcher wiring (which lands in a later milestone).
//! If no lockfile is present, we surface a friendly error pointing
//! the user at `barista pull --no-fetch`.
//!
//! # Output
//!
//! The two flag-driven shapes — `--format text` and `--format json` —
//! still exist (M3.1 contract), but the bytes they emit are produced
//! by the M3.2 T1 renderer pipeline. The legacy [`tree::render_text`]
//! / [`tree::render_json`] helpers are preserved as thin wrappers over
//! the shared renderer so existing snapshot tests don't have to
//! retarget. The global `--output {human,json,ndjson}` flag, when
//! set, takes precedence over `--format`: it routes the same
//! [`GrindTreeReport`] through the chosen renderer.

use std::path::PathBuf;

use barista_lockfile::Lockfile;

use crate::cli::{GlobalFlags, GrindCommand, OutputFormat, TreeArgs, TreeFormat};
use crate::output::{Renderer, make_runtime_renderer};
use crate::project::{ResolveError, ResolveInputs, resolve_project_root};

/// Dispatch a `barista grind <sub>` invocation. Returns the process
/// exit code.
pub fn run(global: &GlobalFlags, cmd: &GrindCommand) -> i32 {
    match cmd {
        GrindCommand::Tree(args) => tree::run(global, args),
        GrindCommand::Diff(_) => stub("diff"),
        GrindCommand::Audit(_) => stub("audit"),
        GrindCommand::Why(_) => stub("why"),
    }
}

fn stub(sub: &str) -> i32 {
    eprintln!(
        "barista: `grind {sub}` not yet implemented in this build. \
         The full implementation lands in a subsequent milestone."
    );
    2
}

/// Errors raised by `grind tree`.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    /// Could not resolve the project root.
    #[error("project setup: {0}")]
    Project(#[from] ResolveError),

    /// The project has no `barista.lock` yet. The user needs to run
    /// `barista pull --no-fetch` (or wait for the resolver wiring
    /// that lets `grind tree` re-resolve on demand).
    #[error("no barista.lock at {expected_at:?}\n  hint: {hint}")]
    NoLockfile {
        /// Where we looked.
        expected_at: PathBuf,
        /// What to do next.
        hint: String,
    },

    /// Failed to read or parse the lockfile.
    #[error("lockfile: {0}")]
    Lockfile(#[from] barista_lockfile::LockfileError),

    /// JSON serialization failed.
    #[error("serialization: {0}")]
    Serialize(#[from] serde_json::Error),

    /// Failed to render the report through the chosen output format.
    #[error("output: {0}")]
    Render(#[from] crate::output::RenderError),
}

pub mod tree {
    //! `barista grind tree` — render the dependency graph from the
    //! on-disk `barista.lock`.

    use super::*;
    use crate::output::human::{render_tree_text, report_from_lockfile};
    use crate::output::report::GrindTreeReport;

    /// Entry point. Returns the process exit code.
    pub fn run(global: &GlobalFlags, args: &TreeArgs) -> i32 {
        let mut renderer = make_runtime_renderer(global);
        let exit = match run_inner(global, args, &mut *renderer) {
            Ok(()) => 0,
            Err(e) => {
                if matches!(global.output, OutputFormat::Human) {
                    eprintln!("error: barista grind tree failed: {e}");
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

    fn run_inner(
        global: &GlobalFlags,
        args: &TreeArgs,
        renderer: &mut dyn Renderer,
    ) -> Result<(), TreeError> {
        let root = resolve_project_root(ResolveInputs {
            root: global.root.clone(),
            file: global.file.clone(),
            ..Default::default()
        })?;
        let lock_path = root.root.join("barista.lock");
        if !lock_path.exists() {
            return Err(TreeError::NoLockfile {
                expected_at: lock_path,
                hint: "run `barista pull --no-fetch` to create one, or wait for the full \
                       M3.x wiring that resolves the tree from the resolver directly."
                    .to_string(),
            });
        }
        let lf = Lockfile::read(&lock_path)?;
        let report = report_from_lockfile(&lf);

        // Compatibility: for Human output, honour the per-command
        // `--format json` (M3.1) by routing through the JSON renderer
        // even when the global default is Human.
        match (global.output, args.format) {
            (OutputFormat::Human, TreeFormat::Json) => {
                // Build an ad-hoc JSON renderer over stdout — pretty,
                // matching M3.1 byte-for-byte. Done inline rather
                // than swapping the caller's renderer so the global
                // `--output` path stays predictable.
                let mut json = crate::output::JsonRenderer::new(
                    Box::new(std::io::stdout()),
                    /* pretty: */ true,
                );
                json.render_grind_tree(&report)?;
            }
            _ => {
                renderer.render_grind_tree(&report)?;
            }
        }
        Ok(())
    }

    /// Render the lockfile as an indented ASCII tree.
    ///
    /// Preserved for callers that want the rendering without going
    /// through a [`Renderer`]. Identical to the byte output the
    /// pre-renderer implementation produced.
    pub fn render_text(lf: &Lockfile) -> String {
        render_tree_text(&report_from_lockfile(lf))
    }

    /// Render the lockfile as a pretty-printed JSON document.
    ///
    /// Preserves the M3.1 `grind tree --format json` shape
    /// (`schema_version`, `reactor[]`, `nodes[]`) — *without* the
    /// `"command"` discriminator that the new global `--output json`
    /// renderer prepends. This stays the per-command machine-readable
    /// view, separate from the global structured-output pipeline.
    /// Ends in a trailing newline.
    pub fn render_json(lf: &Lockfile) -> Result<String, TreeError> {
        let r = report_from_lockfile(lf);
        let legacy = LegacyTreeJson {
            schema_version: r.schema_version,
            reactor: r.reactor.iter().map(LegacyReactor::from).collect(),
            nodes: r.nodes.iter().map(LegacyNode::from).collect(),
        };
        let mut s = serde_json::to_string_pretty(&legacy)?;
        s.push('\n');
        Ok(s)
    }

    /// Legacy JSON shape for `grind tree --format json`.
    ///
    /// Distinct from the renderer's [`GrindTreeReport`] for two
    /// reasons: this one has no `"command"` discriminator (that's a
    /// global-renderer property), and it uses snake-case keys
    /// (`from_path`, `relative_path`, `schema_version`) because
    /// `--format json` predates the global structured-output
    /// pipeline and is locked to its existing shape.
    #[derive(serde::Serialize)]
    struct LegacyTreeJson {
        schema_version: u32,
        reactor: Vec<LegacyReactor>,
        nodes: Vec<LegacyNode>,
    }

    #[derive(serde::Serialize)]
    struct LegacyReactor {
        coords: String,
        version: String,
        relative_path: String,
    }

    #[derive(serde::Serialize)]
    struct LegacyNode {
        coords: String,
        version: String,
        scope: String,
        depth: u32,
        from_path: Vec<String>,
    }

    impl From<&crate::output::report::ReactorModule> for LegacyReactor {
        fn from(r: &crate::output::report::ReactorModule) -> Self {
            Self {
                coords: r.coords.clone(),
                version: r.version.clone(),
                relative_path: r.relative_path.clone(),
            }
        }
    }

    impl From<&crate::output::report::TreeNode> for LegacyNode {
        fn from(n: &crate::output::report::TreeNode) -> Self {
            Self {
                coords: n.coords.clone(),
                version: n.version.clone(),
                scope: n.scope.clone(),
                depth: n.depth,
                from_path: n.from_path.clone(),
            }
        }
    }

    /// Build the structured [`GrindTreeReport`] from a lockfile.
    /// Exposed so callers writing their own renderers don't have to
    /// reach into [`crate::output::human`].
    pub fn build_report(lf: &Lockfile) -> GrindTreeReport {
        report_from_lockfile(lf)
    }
}
