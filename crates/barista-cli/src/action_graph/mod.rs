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

use crate::cmd::MavenPhase;

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

/// `compile` lifecycle phase prefix. Stops at `compile`.
pub const COMPILE_PHASE_PREFIX: &[&str] = &["process-resources", "compile"];

/// `test` lifecycle phase prefix. Stops at `test`.
pub const TEST_PHASE_PREFIX: &[&str] = &[
    "process-resources",
    "compile",
    "process-test-resources",
    "test-compile",
    "test",
];

/// `package` lifecycle phase prefix. Stops at `package`.
pub const PACKAGE_PHASE_PREFIX: &[&str] = &[
    "process-resources",
    "compile",
    "process-test-resources",
    "test-compile",
    "test",
    "prepare-package",
    "package",
];

/// `install` lifecycle phase prefix. Extends `verify` with `install`.
pub const INSTALL_PHASE_PREFIX: &[&str] = &[
    "process-resources",
    "compile",
    "process-test-resources",
    "test-compile",
    "test",
    "prepare-package",
    "package",
    "integration-test",
    "verify",
    "install",
];

/// `deploy` lifecycle phase prefix. Extends `install` with `deploy`.
pub const DEPLOY_PHASE_PREFIX: &[&str] = &[
    "process-resources",
    "compile",
    "process-test-resources",
    "test-compile",
    "test",
    "prepare-package",
    "package",
    "integration-test",
    "verify",
    "install",
    "deploy",
];

/// `clean` lifecycle: a single action. (Maven's `clean` is a separate
/// lifecycle from `default`; per its own definition it has no prefix.)
pub const CLEAN_PHASE_PREFIX: &[&str] = &["clean"];

/// `site` lifecycle: a single action in v0.1. (Maven's `site` lifecycle
/// has `pre-site`, `site`, `post-site`, `site-deploy`; for v0.1 we
/// dispatch the `site` phase verbatim and let the daemon's embedded
/// Maven core inflate the constituent mojos.)
pub const SITE_PHASE_PREFIX: &[&str] = &["site"];

/// Return the lifecycle phase prefix for the given [`MavenPhase`].
///
/// Each prefix is the ordered list of phase names the daemon must
/// execute to satisfy the requested goal. The list is what `mvn
/// help:describe -Dcmd=<phase>` would produce for the same phase
/// against Maven's default lifecycle binding.
///
/// `install` and `deploy` are non-idempotent (they publish artifacts);
/// the per-phase `retryable` flag on [`PlannedAction`] flips to false
/// for those two terminal steps so the M4.2 T6 auto-respawn driver
/// does not double-publish on a daemon-crash retry.
#[must_use]
pub fn phase_prefix(phase: MavenPhase) -> &'static [&'static str] {
    match phase {
        MavenPhase::Clean => CLEAN_PHASE_PREFIX,
        MavenPhase::Compile => COMPILE_PHASE_PREFIX,
        MavenPhase::Test => TEST_PHASE_PREFIX,
        MavenPhase::Package => PACKAGE_PHASE_PREFIX,
        MavenPhase::Verify => VERIFY_PHASE_PREFIX,
        MavenPhase::Install => INSTALL_PHASE_PREFIX,
        MavenPhase::Deploy => DEPLOY_PHASE_PREFIX,
        MavenPhase::Site => SITE_PHASE_PREFIX,
    }
}

/// Whether a given lifecycle phase is safe to retry after an
/// auto-respawn (M4.2 T6 retry path). `install` and `deploy` mutate
/// remote / shared state (the local `~/.m2/repository`, or a remote
/// Nexus / Artifactory) so retrying them after a partial failure
/// risks double-publishing. Every other phase is idempotent within
/// a single module.
#[must_use]
pub fn phase_is_retryable(phase_name: &str) -> bool {
    !matches!(phase_name, "install" | "deploy")
}

/// Build a lifecycle [`ActionGraph`] for the given phase against a
/// single module. The `include_clean` flag, when true, prepends a
/// `clean` phase action so callers can express `barista verify clean`
/// semantics; the bare `barista clean` command already has `clean` as
/// its prefix, so the flag is a no-op there.
#[must_use]
pub fn lifecycle_graph(
    phase: MavenPhase,
    module_root: PathBuf,
    include_clean: bool,
) -> ActionGraph {
    let prefix = phase_prefix(phase);
    let mut actions = Vec::with_capacity(prefix.len() + 1);
    if include_clean && !prefix.contains(&"clean") {
        actions.push(PlannedAction {
            phase: "clean",
            retryable: true,
        });
    }
    for p in prefix {
        actions.push(PlannedAction {
            phase: p,
            retryable: phase_is_retryable(p),
        });
    }
    let pom_path = module_root.join("pom.xml");
    ActionGraph {
        module_root,
        pom_path,
        actions,
    }
}

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

/// The ordered list of every Maven default-lifecycle phase up
/// through `deploy`. Used by `shot_graph` to materialize the phase
/// prefix for an arbitrary single-phase request — `barista shot
/// package` runs `process-resources … package`, `barista shot test`
/// runs `process-resources … test`, etc.
///
/// `install` and `deploy` are included here for completeness but
/// `shot_graph` flips `retryable=false` on those two phases (they
/// have remote side-effects; the auto-respawn retry is unsafe). The
/// retryability inversion matches the design note in
/// [`PlannedAction::retryable`].
pub const DEFAULT_LIFECYCLE_PHASES: &[&str] = &[
    "validate",
    "initialize",
    "generate-sources",
    "process-sources",
    "generate-resources",
    "process-resources",
    "compile",
    "process-classes",
    "generate-test-sources",
    "process-test-sources",
    "generate-test-resources",
    "process-test-resources",
    "test-compile",
    "process-test-classes",
    "test",
    "prepare-package",
    "package",
    "pre-integration-test",
    "integration-test",
    "post-integration-test",
    "verify",
    "install",
    "deploy",
];

/// Phases with remote side-effects: not retryable in `shot_graph`.
const NON_RETRYABLE_PHASES: &[&str] = &["install", "deploy"];

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

/// Error returned by [`shot_graph`] when the expression doesn't
/// resolve to a known Maven lifecycle phase.
#[derive(Debug, thiserror::Error)]
pub enum ShotGraphError {
    /// `expr` wasn't a known Maven default-lifecycle phase.
    #[error(
        "barista shot: unknown phase `{phase}`. \
         Valid phases: validate, initialize, compile, test, package, verify, install, deploy, …"
    )]
    UnknownPhase {
        /// The phase the user asked for.
        phase: String,
    },
}

/// Build the action graph for `barista shot <phase>` against a
/// single module.
///
/// `expr` is a Maven lifecycle phase name (e.g. `"test"`,
/// `"package"`, `"compile"`). The returned graph contains every
/// default-lifecycle phase **up to and including** `expr`, mirroring
/// Maven's semantics where `mvn test` runs the prefix
/// `process-resources … test`.
///
/// # Retryability
///
/// All phases up through `verify` are marked `retryable = true`
/// (idempotent for auto-respawn). `install` and `deploy` are
/// `retryable = false` because they have remote side-effects: a
/// second dispatch after a crash could double-install or
/// double-publish.
///
/// # v0.1 scope
///
/// Only single-phase expressions. Multi-phase composition (e.g.
/// `barista shot "clean package"`) is a v0.2 follow-up — Maven's own
/// lifecycle composer is non-trivial and out of scope for the warm-
/// path optimisation T3 ships.
pub fn shot_graph(module_root: PathBuf, expr: &str) -> Result<ActionGraph, ShotGraphError> {
    let phase = expr.trim();
    let idx = DEFAULT_LIFECYCLE_PHASES
        .iter()
        .position(|p| *p == phase)
        .ok_or_else(|| ShotGraphError::UnknownPhase {
            phase: phase.to_string(),
        })?;

    let mut actions = Vec::with_capacity(idx + 1);
    for p in &DEFAULT_LIFECYCLE_PHASES[..=idx] {
        actions.push(PlannedAction {
            phase: p,
            retryable: !NON_RETRYABLE_PHASES.contains(p),
        });
    }
    let pom_path = module_root.join("pom.xml");
    Ok(ActionGraph {
        module_root,
        pom_path,
        actions,
    })
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
        // install/deploy non-retryable. The verify graph stops at
        // `verify` so every phase here remains retryable.
        let g = verify_graph(PathBuf::from("/tmp/project"), true);
        for a in &g.actions {
            assert!(a.retryable, "phase {} must be retryable in v0.1", a.phase);
        }
    }

    #[test]
    fn install_graph_marks_install_non_retryable() {
        // M4.3 T2: `install` / `deploy` are non-idempotent terminal
        // steps. The auto-respawn driver consults the per-action
        // retryable flag, so flipping it to false here is what stops
        // the M4.2 T6 retry path from double-publishing.
        let g = lifecycle_graph(MavenPhase::Install, PathBuf::from("/tmp/p"), false);
        let install = g.actions.iter().find(|a| a.phase == "install").unwrap();
        assert!(
            !install.retryable,
            "install must not be retryable — double-publish risk"
        );
        // Every preceding phase remains retryable.
        for a in g.actions.iter().take_while(|a| a.phase != "install") {
            assert!(a.retryable, "{} must be retryable", a.phase);
        }
    }

    #[test]
    fn deploy_graph_marks_install_and_deploy_non_retryable() {
        let g = lifecycle_graph(MavenPhase::Deploy, PathBuf::from("/tmp/p"), false);
        for a in &g.actions {
            let want_retryable = !matches!(a.phase, "install" | "deploy");
            assert_eq!(
                a.retryable, want_retryable,
                "phase {} retryable={}",
                a.phase, want_retryable
            );
        }
    }

    #[test]
    fn clean_graph_is_single_action() {
        let g = lifecycle_graph(MavenPhase::Clean, PathBuf::from("/tmp/p"), false);
        assert_eq!(g.actions.len(), 1);
        assert_eq!(g.actions[0].phase, "clean");
    }

    #[test]
    fn compile_graph_stops_at_compile() {
        let g = lifecycle_graph(MavenPhase::Compile, PathBuf::from("/tmp/p"), false);
        let names: Vec<&str> = g.actions.iter().map(|a| a.phase).collect();
        assert_eq!(names, vec!["process-resources", "compile"]);
    }

    #[test]
    fn package_graph_stops_at_package_no_integration_tests() {
        let g = lifecycle_graph(MavenPhase::Package, PathBuf::from("/tmp/p"), false);
        let names: Vec<&str> = g.actions.iter().map(|a| a.phase).collect();
        assert!(!names.contains(&"integration-test"));
        assert!(!names.contains(&"verify"));
        assert_eq!(names.last(), Some(&"package"));
    }

    #[test]
    fn lifecycle_graph_include_clean_prepends_for_default_lifecycle() {
        let g = lifecycle_graph(MavenPhase::Compile, PathBuf::from("/tmp/p"), true);
        assert_eq!(g.actions.first().map(|a| a.phase), Some("clean"));
    }

    #[test]
    fn lifecycle_graph_clean_phase_no_double_clean_prefix() {
        // `barista clean --clean` (or whatever opt-in path) must not
        // produce two "clean" actions; phase_prefix(Clean) already
        // contains "clean".
        let g = lifecycle_graph(MavenPhase::Clean, PathBuf::from("/tmp/p"), true);
        assert_eq!(g.actions.len(), 1);
        assert_eq!(g.actions[0].phase, "clean");
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

    #[test]
    fn shot_graph_test_phase_runs_lifecycle_prefix_through_test() {
        let g = shot_graph(PathBuf::from("/tmp/p"), "test").unwrap();
        let names: Vec<&str> = g.actions.iter().map(|a| a.phase).collect();
        assert_eq!(*names.last().unwrap(), "test");
        // Earliest phases the test prefix must include.
        assert!(names.contains(&"compile"));
        assert!(names.contains(&"test-compile"));
    }

    #[test]
    fn shot_graph_package_phase_runs_through_package() {
        let g = shot_graph(PathBuf::from("/tmp/p"), "package").unwrap();
        let names: Vec<&str> = g.actions.iter().map(|a| a.phase).collect();
        assert_eq!(*names.last().unwrap(), "package");
        assert!(names.contains(&"test"));
    }

    #[test]
    fn shot_graph_unknown_phase_errors() {
        let err = shot_graph(PathBuf::from("/tmp/p"), "definitely-not-a-phase").unwrap_err();
        assert!(matches!(err, ShotGraphError::UnknownPhase { .. }));
    }

    #[test]
    fn shot_graph_deploy_is_non_retryable() {
        let g = shot_graph(PathBuf::from("/tmp/p"), "deploy").unwrap();
        let deploy = g.actions.iter().find(|a| a.phase == "deploy").unwrap();
        assert!(!deploy.retryable, "deploy must be non-retryable");
        let install = g.actions.iter().find(|a| a.phase == "install").unwrap();
        assert!(!install.retryable, "install must be non-retryable");
        let compile = g.actions.iter().find(|a| a.phase == "compile").unwrap();
        assert!(compile.retryable, "compile must remain retryable");
    }

    #[test]
    fn shot_graph_trims_whitespace() {
        let g = shot_graph(PathBuf::from("/tmp/p"), "  compile  ").unwrap();
        assert_eq!(*g.actions.last().unwrap().phase, *"compile");
    }
}
