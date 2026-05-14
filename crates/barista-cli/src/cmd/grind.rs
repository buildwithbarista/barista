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

use std::path::PathBuf;

use barista_lockfile::{Lockfile, LockfileEntry};

use crate::cli::{GlobalFlags, GrindCommand, TreeArgs, TreeFormat};
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
}

pub mod tree {
    //! `barista grind tree` — render the dependency graph from the
    //! on-disk `barista.lock`.

    use super::*;

    /// Entry point. Returns the process exit code.
    pub fn run(global: &GlobalFlags, args: &TreeArgs) -> i32 {
        match run_inner(global, args) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: barista grind tree failed: {e}");
                1
            }
        }
    }

    fn run_inner(global: &GlobalFlags, args: &TreeArgs) -> Result<(), TreeError> {
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
        let rendered = match args.format {
            TreeFormat::Text => render_text(&lf),
            TreeFormat::Json => render_json(&lf)?,
        };
        print!("{rendered}");
        Ok(())
    }

    // ---- text renderer ------------------------------------------------

    /// Render the lockfile as an indented ASCII tree.
    ///
    /// Each reactor entry is a root. Direct dependencies (entries
    /// with an empty `from_path`) are listed under a synthetic
    /// `(direct dependencies)` heading when no reactor is present, or
    /// under each reactor entry when one or more exist (the lockfile
    /// schema does not currently associate direct deps with a
    /// specific reactor module, so we list them once under the first
    /// reactor entry).
    ///
    /// Transitive entries are placed under their `from_path` parent.
    /// If a parent cannot be located in the lockfile (which can
    /// happen with hand-crafted lockfiles or future schema changes),
    /// the entry is rendered at the top level under an `(orphan)`
    /// heading so it stays visible.
    ///
    /// Output ends in a single trailing newline.
    pub fn render_text(lf: &Lockfile) -> String {
        let mut out = String::new();

        // Index entries by their full path = from_path + [coords].
        // This is what a *child* entry references as its parent path.
        // We use the BFS-flavored key directly: a child whose
        // from_path equals an entry's full path is that entry's
        // dependency.

        // Group children by parent's full-path key. Direct deps key
        // off the empty path.
        let mut children: std::collections::BTreeMap<Vec<String>, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (i, e) in lf.entries.iter().enumerate() {
            children.entry(e.from_path.clone()).or_default().push(i);
        }

        if lf.reactor.is_empty() {
            // No reactor: render direct deps at the top, then
            // recurse on transitives.
            out.push_str("(no reactor)\n");
            render_children(lf, &children, &[], "", &mut out);
        } else {
            for (i, r) in lf.reactor.iter().enumerate() {
                let last = i + 1 == lf.reactor.len();
                let coords_v = format!("{}:{}", r.coords, r.version);
                out.push_str(&coords_v);
                out.push('\n');
                // Attach direct deps (empty from_path) under the
                // first reactor entry only — see doc comment.
                if i == 0 {
                    render_children(lf, &children, &[], "", &mut out);
                }
                if !last {
                    out.push('\n');
                }
            }
        }

        // Surface orphan transitive entries (entries whose from_path
        // does not match any entry's full path) so users notice them.
        let known_paths: std::collections::BTreeSet<Vec<String>> = std::iter::once(Vec::new())
            .chain(lf.entries.iter().map(entry_full_path))
            .collect();
        let mut orphans: Vec<usize> = Vec::new();
        for (i, e) in lf.entries.iter().enumerate() {
            if !e.from_path.is_empty() && !known_paths.contains(&e.from_path) {
                orphans.push(i);
            }
        }
        if !orphans.is_empty() {
            out.push_str("\n(orphan transitives)\n");
            for (k, &idx) in orphans.iter().enumerate() {
                let last = k + 1 == orphans.len();
                let prefix = if last { "└── " } else { "├── " };
                out.push_str(prefix);
                out.push_str(&format_entry_line(&lf.entries[idx]));
                out.push('\n');
            }
        }

        out
    }

    /// The full BFS path of an entry, used as the key its own
    /// children reference in `from_path`.
    fn entry_full_path(e: &LockfileEntry) -> Vec<String> {
        let mut v = e.from_path.clone();
        v.push(e.coords.clone());
        v
    }

    /// One line of the tree: `group:artifact:version  [scope]`.
    fn format_entry_line(e: &LockfileEntry) -> String {
        format!("{}:{}  [{}]", e.coords, e.version, e.scope)
    }

    /// Render the children of a node whose full path is `parent_path`.
    ///
    /// `indent` is the prefix already emitted on previous lines for
    /// parent indentation (e.g. `"│   "`). It is extended per child
    /// with the connector glyphs.
    fn render_children(
        lf: &Lockfile,
        children: &std::collections::BTreeMap<Vec<String>, Vec<usize>>,
        parent_path: &[String],
        indent: &str,
        out: &mut String,
    ) {
        let Some(kids) = children.get(parent_path) else {
            return;
        };
        for (i, &idx) in kids.iter().enumerate() {
            let last = i + 1 == kids.len();
            let connector = if last { "└── " } else { "├── " };
            out.push_str(indent);
            out.push_str(connector);
            out.push_str(&format_entry_line(&lf.entries[idx]));
            out.push('\n');

            let child_indent = if last { "    " } else { "│   " };
            let mut next_indent = String::with_capacity(indent.len() + child_indent.len());
            next_indent.push_str(indent);
            next_indent.push_str(child_indent);

            let mut child_path = parent_path.to_vec();
            child_path.push(lf.entries[idx].coords.clone());
            render_children(lf, children, &child_path, &next_indent, out);
        }
    }

    // ---- json renderer ------------------------------------------------

    /// Render the lockfile as a flat JSON document. The shape is
    /// stable across the v0.1 schema and carries a `schema_version`
    /// so downstream consumers can detect breaking changes.
    pub fn render_json(lf: &Lockfile) -> Result<String, TreeError> {
        let doc = TreeJson::from_lockfile(lf);
        let mut s = serde_json::to_string_pretty(&doc)?;
        s.push('\n');
        Ok(s)
    }

    /// JSON shape emitted by `grind tree --format json`.
    #[derive(serde::Serialize)]
    pub struct TreeJson {
        /// Stable shape version. Bumped on breaking changes.
        pub schema_version: u32,
        /// Reactor modules, in lockfile order.
        pub reactor: Vec<ReactorJson>,
        /// Resolved entries, in lockfile order.
        pub nodes: Vec<TreeNode>,
    }

    /// JSON representation of one reactor module.
    #[derive(serde::Serialize)]
    pub struct ReactorJson {
        pub coords: String,
        pub version: String,
        pub relative_path: String,
    }

    /// JSON representation of one resolved entry.
    #[derive(serde::Serialize)]
    pub struct TreeNode {
        pub coords: String,
        pub version: String,
        pub scope: String,
        pub depth: u32,
        pub from_path: Vec<String>,
    }

    impl TreeJson {
        fn from_lockfile(lf: &Lockfile) -> Self {
            Self {
                schema_version: 1,
                reactor: lf
                    .reactor
                    .iter()
                    .map(|r| ReactorJson {
                        coords: r.coords.clone(),
                        version: r.version.clone(),
                        relative_path: r.relative_path.clone(),
                    })
                    .collect(),
                nodes: lf
                    .entries
                    .iter()
                    .map(|e| TreeNode {
                        coords: e.coords.clone(),
                        version: e.version.clone(),
                        scope: e.scope.clone(),
                        depth: e.depth,
                        from_path: e.from_path.clone(),
                    })
                    .collect(),
            }
        }
    }
}
