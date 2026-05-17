//! Maven lifecycle action graph for `barista verify`.
//!
//! The action graph is the ordered list of mojo invocations the
//! daemon executes to fulfill a lifecycle goal. For the `verify`
//! goal in a single-module Maven project, the graph is the
//! lifecycle phase prefix:
//!
//! ```text
//!   process-resources
//!   compile
//!   process-test-resources
//!   test-compile
//!   test
//!   prepare-package
//!   package
//!   integration-test
//!   verify
//! ```
//!
//! Each entry binds to one or more mojos at execution time. Resolving
//! the *actual* mojos for a phase against the effective POM (the
//! plugins declared in `<build>`, plus the default lifecycle bindings
//! Maven ships with) is the daemon's job: the CLI side hands the
//! daemon `(phase, project_root, pom_path, effective_pom_blob)` and
//! the daemon's embedded Maven core inflates that into the concrete
//! mojo set.
//!
//! # v0.1 single-module scope
//!
//! The action graph here covers only single-module projects: one
//! invocation list, one project root. Reactor topo-sort + per-level
//! parallelism (multi-module projects) lands in M4.3 Task 4 —
//! `action_graph` grows a `Reactor` type that wraps a `Vec<Module>`,
//! each module carrying its own phase list. Today's `ActionGraph` is
//! the single-module case the reactor wrapper would emit one of.
//!
//! # Wire shape
//!
//! Each entry becomes one `ActionRequest` on the wire. The
//! `mojo_coords` field is the lifecycle phase *name* in v0.1 — the
//! daemon resolves the phase to its constituent mojos via the
//! embedded Maven core. v0.2+ may pre-resolve mojos on the CLI side
//! (matching `mvn`'s `-X` diagnostic output) so the daemon's
//! work-per-action is uniform; that's a representation change
//! invisible to the user, and the v0.1 phase-name shape stays
//! forward-compatible with it.

use std::path::{Path, PathBuf};

use barista_ipc::ActionRequest;

/// The `verify` lifecycle phase prefix per Maven's default
/// lifecycle (see `mvn help:describe -Dcmd=verify` for the
/// canonical list).
pub const VERIFY_PHASE_PREFIX: &[&str] = &[
    "process-resources",
    "compile",
    "process-test-resources",
    "test-compile",
    "test",
    "prepare-package",
    "package",
    "integration-test",
    "verify",
];

/// An ordered list of mojo invocations for one module.
#[derive(Debug, Clone)]
pub struct ActionGraph {
    /// Module the actions target (single-module case: the project
    /// root). Stored as an absolute path; reactor topo-sort in T4
    /// uses this to group invocations by module.
    pub module_root: PathBuf,
    /// Module's `pom.xml`. Daemon needs this for the effective-POM
    /// reconstruction.
    pub pom_path: PathBuf,
    /// Ordered actions. Index N completes before action N+1 starts
    /// (sequential dispatch — per-level parallelism lands in T4).
    pub actions: Vec<PlannedAction>,
}

/// One planned action in the graph. Carries the lifecycle phase the
/// daemon should execute against the module's effective POM.
#[derive(Debug, Clone)]
pub struct PlannedAction {
    /// Lifecycle phase name, e.g. `"compile"`. The daemon's embedded
    /// Maven core resolves this to its constituent mojos.
    pub phase: &'static str,
    /// Whether the daemon should treat this action as idempotent for
    /// auto-respawn purposes (M4.2 T6 / M4.3 T1). All lifecycle
    /// phases up through `verify` are idempotent; `install`/`deploy`
    /// (handled in M4.3 Task 2) flip this to `false` so the
    /// respawn-and-retry path is skipped.
    pub retryable: bool,
}

/// Build the action graph for `barista verify` against a single
/// module. The `clean` prefix is included only when `include_clean`
/// is true; the user opts in via positional args (M4.3 Task 2 wires
/// `barista verify clean` to set this to `true`).
///
/// `module_root` is the directory containing the module's `pom.xml`.
/// In the single-module case it equals the project root; in the
/// reactor case (T4) it varies per module.
#[must_use]
pub fn verify_graph(module_root: PathBuf, include_clean: bool) -> ActionGraph {
    let mut actions = Vec::with_capacity(VERIFY_PHASE_PREFIX.len() + 1);
    if include_clean {
        actions.push(PlannedAction {
            phase: "clean",
            retryable: true,
        });
    }
    for phase in VERIFY_PHASE_PREFIX {
        actions.push(PlannedAction {
            phase,
            retryable: true,
        });
    }
    let pom_path = module_root.join("pom.xml");
    ActionGraph {
        module_root,
        pom_path,
        actions,
    }
}

/// Build the `ActionRequest` envelope for one [`PlannedAction`] in an
/// [`ActionGraph`].
///
/// The fields populated here are the v0.1 minimum the daemon needs to
/// look up the module's effective POM, resolve the phase to its
/// constituent mojos, and execute them. Fields that aren't yet wired
/// (`effective_pom_blob`, `classpath`, `plugin_classpath`,
/// `credentials`) are left empty/default; the daemon's `BAR-DAEMON-
/// NOT-YET-IMPLEMENTED` stub path (M4.2 T2 placeholder) reads
/// `mojo_coords` + `project_root` + `pom_path` and returns its
/// response based on those three.
///
/// The wire-level fields the daemon's M4.2 T3 embedded-Maven core
/// consumes (`effective_pom_blob`, classpath, etc.) become non-empty
/// in M4.3 Task 2 when the action graph grows the M1.2 effective-POM
/// blob + M2.x classpath wiring. The v0.1 happy path documented in
/// T1's acceptance criteria covers the surface area below.
pub fn build_action_request(
    graph: &ActionGraph,
    action: &PlannedAction,
    project_root: &Path,
) -> ActionRequest {
    // The mux layer's `submit_action` overwrites `action_id` with a
    // freshly minted UUIDv7, so the empty default below is fine.
    //
    // Maven compat: v0.1 defaults to Maven 4. The daemon's
    // `EmbeddedMaven` is built against rc-3 per ADR-008.
    //
    // Stream IDs are conventional `1`/`2` (stdout/stderr) — the mux
    // layer disambiguates by `action_id`, so only the within-action
    // uniqueness matters.
    ActionRequest {
        action_id: String::new(),
        mojo_coords: action.phase.to_string(),
        project_root: project_root.display().to_string(),
        pom_path: graph.pom_path.display().to_string(),
        working_directory: graph.module_root.display().to_string(),
        maven_compat: "4".to_string(),
        stdout_stream_id: 1,
        stderr_stream_id: 2,
        ..ActionRequest::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_graph_has_full_lifecycle_prefix() {
        let g = verify_graph(PathBuf::from("/tmp/project"), false);
        let names: Vec<&str> = g.actions.iter().map(|a| a.phase).collect();
        assert_eq!(names, VERIFY_PHASE_PREFIX);
    }

    #[test]
    fn verify_graph_with_clean_prepends_clean() {
        let g = verify_graph(PathBuf::from("/tmp/project"), true);
        assert_eq!(g.actions[0].phase, "clean");
        assert_eq!(g.actions[1].phase, "process-resources");
        assert_eq!(g.actions.len(), 1 + VERIFY_PHASE_PREFIX.len());
    }

    #[test]
    fn verify_graph_phases_are_all_retryable_in_v01() {
        // M4.3 T1 only covers idempotent lifecycle phases; T2 makes
        // install/deploy non-retryable. Pinning the v0.1 invariant
        // here surfaces a delta when T2 lands.
        let g = verify_graph(PathBuf::from("/tmp/project"), true);
        for a in &g.actions {
            assert!(a.retryable, "phase {} must be retryable in v0.1", a.phase);
        }
    }

    #[test]
    fn build_action_request_populates_minimum_fields() {
        let g = verify_graph(PathBuf::from("/tmp/project"), false);
        let action = &g.actions[1]; // "compile"
        let req = build_action_request(&g, action, Path::new("/tmp/project"));
        assert_eq!(req.mojo_coords, "compile");
        assert_eq!(req.project_root, "/tmp/project");
        assert_eq!(req.pom_path, "/tmp/project/pom.xml");
        assert_eq!(req.working_directory, "/tmp/project");
        assert_eq!(req.maven_compat, "4");
        assert_ne!(req.stdout_stream_id, req.stderr_stream_id);
    }

    #[test]
    fn pom_path_is_module_root_join_pom_xml() {
        let g = verify_graph(PathBuf::from("/projects/foo"), false);
        assert_eq!(g.pom_path, PathBuf::from("/projects/foo/pom.xml"));
    }
}
