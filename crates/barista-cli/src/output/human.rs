//! Human-readable renderer.
//!
//! Designed for a developer at a tty. Two writers:
//!
//! - `out` — primary stream (stdout in production). Receives the
//!   `grind tree` text rendering.
//! - `err` — secondary stream (stderr in production). Receives the
//!   informational summary lines for `pull` / `pour` and any error
//!   surfaces. Mirrors what the pre-renderer command runners did with
//!   `eprintln!`.
//!
//! `ansi` is the colour gate. The current set of human surfaces is
//! plain text — no colours need styling at v0.1 — so the flag is
//! threaded through and stored but is otherwise inert. Adding ANSI
//! decoration later is a localized change to this file.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use barista_lockfile::LockfileEntry;

use super::report::{GrindTreeReport, PourReport, PullReport, TreeNode, VerifyReport};
use super::{RenderResult, Renderer};

/// Renderer for `OutputFormat::Human`.
pub struct HumanRenderer {
    out: Box<dyn Write + Send>,
    err: Box<dyn Write + Send>,
    #[allow(dead_code)] // wired for ANSI styling once human surfaces grow colours
    ansi: bool,
}

impl HumanRenderer {
    /// Construct a renderer over the given streams. The `ansi` flag
    /// is stored for future ANSI-decoration of human output; the
    /// renderer is plain-text at v0.1 so it has no immediate effect.
    pub fn new(out: Box<dyn Write + Send>, err: Box<dyn Write + Send>, ansi: bool) -> Self {
        Self { out, err, ansi }
    }

    /// Whether ANSI styling is enabled. Used by callers and tests
    /// that want to confirm tty-detection wiring.
    pub fn ansi(&self) -> bool {
        self.ansi
    }
}

impl Renderer for HumanRenderer {
    fn render_pull(&mut self, report: &PullReport) -> RenderResult<()> {
        // Mirrors the pre-renderer behaviour: `pull: <summary>` to
        // stderr. The caller has already filtered on `--quiet`.
        writeln!(self.err, "pull: {}", report.summary())?;
        Ok(())
    }

    fn render_grind_tree(&mut self, report: &GrindTreeReport) -> RenderResult<()> {
        let text = render_tree_text(report);
        write!(self.out, "{text}")?;
        Ok(())
    }

    fn render_pour(&mut self, report: &PourReport) -> RenderResult<()> {
        // Pre-renderer: `pour: <summary>` to stderr (gated on quiet
        // by the caller).
        writeln!(self.err, "pour: {}", report.summary())?;
        Ok(())
    }

    fn render_verify(&mut self, report: &VerifyReport) -> RenderResult<()> {
        // Mirror the `pull` / `pour` shape: `<phase>: <summary>` to
        // stderr. The caller has already filtered on `--quiet`.
        writeln!(self.err, "{}: {}", report.phase, report.summary())?;
        // For non-trivial action graphs, surface a per-invocation
        // summary so the developer can see which mojo took how long.
        // Failed invocations include their `failure_message` for a
        // one-glance diagnosis.
        for inv in &report.invocations {
            if inv.exit_code != 0 {
                writeln!(
                    self.err,
                    "  ✗ {phase} :: {mojo} (exit={code}) — {msg}",
                    phase = inv.phase,
                    mojo = inv.mojo,
                    code = inv.exit_code,
                    msg = if inv.failure_message.is_empty() {
                        inv.status.as_str()
                    } else {
                        inv.failure_message.as_str()
                    },
                )?;
            }
        }
        Ok(())
    }

    fn render_error(&mut self, err: &(dyn std::error::Error + 'static)) -> RenderResult<()> {
        // Match the existing `error: barista <cmd> failed: {e}`
        // shape used by `cmd::pull::run` / `cmd::pour::run`. The
        // command's own error type carries the verb in its message,
        // so we don't have to thread it through here.
        writeln!(self.err, "error: {err}")?;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> RenderResult<()> {
        self.out.flush()?;
        self.err.flush()?;
        Ok(())
    }
}

/// Render a [`GrindTreeReport`] as an indented ASCII tree.
///
/// Preserves the exact output the pre-renderer
/// `barista_cli::cmd::grind::tree::render_text` produced for a
/// [`barista_lockfile::Lockfile`]:
///
/// - Reactor entries are roots.
/// - Direct dependencies (entries with empty `from_path`) are listed
///   under the first reactor entry, or under a synthetic
///   `(no reactor)` heading if there are no reactor entries.
/// - Transitives are placed under their `from_path` parent.
/// - Entries whose `from_path` does not match any known entry land
///   under an `(orphan transitives)` heading.
///
/// Output ends in a single trailing newline.
pub fn render_tree_text(report: &GrindTreeReport) -> String {
    let mut out = String::new();

    // Group children by their parent's "full path" — empty for
    // direct deps, otherwise the parent's `from_path + [coords]`.
    let mut children: BTreeMap<Vec<String>, Vec<usize>> = BTreeMap::new();
    for (i, e) in report.nodes.iter().enumerate() {
        children.entry(e.from_path.clone()).or_default().push(i);
    }

    if report.reactor.is_empty() {
        out.push_str("(no reactor)\n");
        render_children_nodes(&report.nodes, &children, &[], "", &mut out);
    } else {
        for (i, r) in report.reactor.iter().enumerate() {
            let last = i + 1 == report.reactor.len();
            out.push_str(&format!("{}:{}\n", r.coords, r.version));
            if i == 0 {
                render_children_nodes(&report.nodes, &children, &[], "", &mut out);
            }
            if !last {
                out.push('\n');
            }
        }
    }

    // Surface orphan transitive entries (entries whose from_path
    // does not match any entry's full path) so users notice them.
    let known_paths: BTreeSet<Vec<String>> = std::iter::once(Vec::new())
        .chain(report.nodes.iter().map(node_full_path))
        .collect();
    let mut orphans: Vec<usize> = Vec::new();
    for (i, e) in report.nodes.iter().enumerate() {
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
            out.push_str(&format_node_line(&report.nodes[idx]));
            out.push('\n');
        }
    }

    out
}

fn node_full_path(e: &TreeNode) -> Vec<String> {
    let mut v = e.from_path.clone();
    v.push(e.coords.clone());
    v
}

fn format_node_line(e: &TreeNode) -> String {
    format!("{}:{}  [{}]", e.coords, e.version, e.scope)
}

fn render_children_nodes(
    nodes: &[TreeNode],
    children: &BTreeMap<Vec<String>, Vec<usize>>,
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
        out.push_str(&format_node_line(&nodes[idx]));
        out.push('\n');

        let child_indent = if last { "    " } else { "│   " };
        let mut next_indent = String::with_capacity(indent.len() + child_indent.len());
        next_indent.push_str(indent);
        next_indent.push_str(child_indent);

        let mut child_path = parent_path.to_vec();
        child_path.push(nodes[idx].coords.clone());
        render_children_nodes(nodes, children, &child_path, &next_indent, out);
    }
}

/// Build a [`GrindTreeReport`] from a [`barista_lockfile::Lockfile`].
///
/// Lives here (alongside the text renderer) rather than in
/// `output::report` because it depends on the lockfile crate; the
/// shape definitions in `report` stay free of that dependency.
pub fn report_from_lockfile(lf: &barista_lockfile::Lockfile) -> GrindTreeReport {
    GrindTreeReport {
        schema_version: 1,
        reactor: lf
            .reactor
            .iter()
            .map(|r| super::report::ReactorModule {
                coords: r.coords.clone(),
                version: r.version.clone(),
                relative_path: r.relative_path.clone(),
            })
            .collect(),
        nodes: lf.entries.iter().map(node_from_entry).collect(),
    }
}

fn node_from_entry(e: &LockfileEntry) -> TreeNode {
    TreeNode {
        coords: e.coords.clone(),
        version: e.version.clone(),
        scope: e.scope.clone(),
        depth: e.depth,
        from_path: e.from_path.clone(),
    }
}
