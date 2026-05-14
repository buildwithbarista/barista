//! Integration tests for `barista pour`.
//!
//! These drive the CLI library's `dispatch` entry point so we
//! exercise the same path the binary does — argv parse, dispatch,
//! exit code — while using a `run_inner` library handle for richer
//! assertions where they're useful.
//!
//! Each test builds a hermetic fixture in a `tempdir`: a minimal
//! `pom.xml`, a project-level `barista.toml` pointing `paths.cache-dir`
//! at a per-test CAS root, an in-place CAS seeded with the bytes that
//! the lockfile entries pin, and a `barista.lock` whose `sha256` /
//! `size_bytes` fields are derived from the seeded bytes (not
//! hand-rolled, so the assertions stay honest if the schema shifts).
//!
//! Test mapping to `[T]` acceptance criteria
//! ----------------------------------------
//!
//! | criterion                                                | test fn                                                       |
//! |----------------------------------------------------------|---------------------------------------------------------------|
//! | happy path: locked + cached artifacts → materialized     | [`happy_path_materializes_into_target`]                       |
//! | `--target` override is honored                           | [`target_flag_overrides_default_m2`]                          |
//! | `--scope compile` filters out `test`-only entries        | [`scope_compile_filters_out_test_only_entries`]              |
//! | `--dry-run` materializes nothing                         | [`dry_run_materializes_nothing`]                              |
//! | locked coord missing from CAS → [`PourError::NotInCache`]| [`missing_from_cache_returns_not_in_cache`]                   |
//! | missing lockfile → friendly structured error             | [`missing_lockfile_returns_structured_error`]                 |
//! | exit codes follow convention (0 / 1 / 2)                 | [`exit_codes_follow_convention_*`]                            |
//! | human-readable summary snapshot                          | [`pour_summary_snapshot`]                                     |

use std::fs;
use std::path::{Path, PathBuf};

use barista_cache::cas::Cas;
use barista_cli::cli::{Cli, GlobalFlags, OutputFormat, PourArgs, ScopeArg, dispatch};
use barista_cli::cmd::pour::{PourError, run_inner};
use barista_lockfile::{Lockfile, LockfileEntry};
use clap::Parser;
use tempfile::TempDir;

// ---- fixture helpers ---------------------------------------------------

const MINIMAL_POM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
  <packaging>jar</packaging>
</project>
"#;

/// One artifact in a fixture: identifying coords + the bytes that
/// land in the CAS.
#[derive(Clone)]
struct ArtifactFixture {
    coords: &'static str, // "g:a"
    version: &'static str,
    scope: &'static str,
    bytes: &'static [u8],
    classifier: Option<&'static str>,
    type_: &'static str,
}

impl ArtifactFixture {
    fn jar(coords: &'static str, version: &'static str, bytes: &'static [u8]) -> Self {
        Self {
            coords,
            version,
            scope: "compile",
            bytes,
            classifier: None,
            type_: "jar",
        }
    }

    fn with_scope(mut self, scope: &'static str) -> Self {
        self.scope = scope;
        self
    }

    /// Hash these bytes using a throwaway CAS so we don't pull in a
    /// direct `sha2` dependency just for tests.
    fn sha256_hex(&self) -> String {
        let td = tempfile::tempdir().expect("hash-oracle tempdir");
        let cas = Cas::open(td.path()).expect("hash-oracle cas");
        let (hash, _) = cas.put(self.bytes).expect("hash-oracle put");
        hash.to_hex()
    }

    fn into_lockfile_entry(self) -> LockfileEntry {
        LockfileEntry {
            coords: self.coords.to_string(),
            version: self.version.to_string(),
            scope: self.scope.to_string(),
            optional: false,
            sha256: self.sha256_hex(),
            sha1: None,
            size_bytes: self.bytes.len() as u64,
            source_url: format!(
                "https://repo.maven.apache.org/maven2/{}/{}-{}.jar",
                self.coords.replace([':', '.'], "/"),
                self.coords.split(':').next_back().unwrap_or(""),
                self.version,
            ),
            etag: None,
            last_modified: None,
            classifier: self.classifier.map(str::to_string),
            type_: self.type_.to_string(),
            from_path: Vec::new(),
            depth: 0,
            snapshot_resolution: None,
            exclusions: Vec::new(),
        }
    }
}

/// A fully seeded fixture: a project root with `pom.xml`,
/// `barista.toml` pinning a per-test CAS root, a populated CAS, and
/// a `barista.lock` consistent with the seeded bytes.
struct Fixture {
    _tempdir: TempDir,
    project_root: PathBuf,
    #[allow(dead_code)] // retained for diagnostic inspection across future tests
    cache_root: PathBuf,
}

impl Fixture {
    fn build(artifacts: &[ArtifactFixture]) -> Self {
        Self::build_inner(
            artifacts, /* skip_cache_seed: */ false, /* skip_lock: */ false,
        )
    }

    fn build_without_seeding(artifacts: &[ArtifactFixture]) -> Self {
        Self::build_inner(
            artifacts, /* skip_cache_seed: */ true, /* skip_lock: */ false,
        )
    }

    fn build_without_lockfile() -> Self {
        Self::build_inner(
            &[],
            /* skip_cache_seed: */ true,
            /* skip_lock: */ true,
        )
    }

    fn build_inner(artifacts: &[ArtifactFixture], skip_cache_seed: bool, skip_lock: bool) -> Self {
        let td = tempfile::tempdir().expect("tempdir");
        let project_root = td.path().join("proj");
        let cache_root = td.path().join("cache");
        fs::create_dir_all(&project_root).unwrap();
        fs::create_dir_all(&cache_root).unwrap();

        // Project-level config: pin cache-dir to our per-test root so
        // we never touch the developer's real `~/.barista/cache`.
        let barista_toml = format!("[paths]\ncache-dir = {:?}\n", cache_root.to_string_lossy());
        fs::write(project_root.join("barista.toml"), barista_toml).unwrap();
        fs::write(project_root.join("pom.xml"), MINIMAL_POM).unwrap();

        if !skip_cache_seed {
            let cas = Cas::open(&cache_root).expect("open cas");
            for a in artifacts {
                cas.put(a.bytes).expect("seed cas");
            }
        }

        if !skip_lock {
            let mut lf = Lockfile::new("a".repeat(64), "b".repeat(64));
            for a in artifacts {
                lf.entries.push(a.clone().into_lockfile_entry());
            }
            lf.write(&project_root.join("barista.lock")).unwrap();
        }

        Self {
            _tempdir: td,
            project_root,
            cache_root,
        }
    }

    fn root_str(&self) -> &str {
        self.project_root.to_str().unwrap()
    }
}

/// Build a default `GlobalFlags` pointing at the fixture project
/// root. Used by `run_inner` callers.
fn flags_for(fx: &Fixture) -> GlobalFlags {
    GlobalFlags {
        output: OutputFormat::Human,
        ci: false,
        quiet: true,
        verbose: 0,
        root: Some(fx.project_root.clone()),
        file: None,
        strict: false,
        frozen: false,
        no_daemon: false,
        maven_compat: None,
        config: None,
        no_color: false,
    }
}

fn default_pour_args(target: PathBuf) -> PourArgs {
    PourArgs {
        target: Some(target),
        scope: ScopeArg::Compile,
        dry_run: false,
    }
}

fn run_dispatch(argv: &[&str]) -> i32 {
    let cli = Cli::try_parse_from(argv).expect("parse argv");
    dispatch(cli)
}

// =======================================================================
// Happy path: locked + cached artifacts are materialized
// =======================================================================

#[test]
fn happy_path_materializes_into_target() {
    let fx = Fixture::build(&[
        ArtifactFixture::jar("org.example:lib-a", "1.0.0", b"lib-a-bytes"),
        ArtifactFixture::jar("org.example:lib-b", "2.5.1", b"lib-b-bytes"),
    ]);
    let target_td = tempfile::tempdir().unwrap();

    let args = default_pour_args(target_td.path().to_path_buf());
    let report = run_inner(&flags_for(&fx), &args).expect("pour ok");

    assert!(!report.dry_run);
    assert_eq!(report.considered, 2);
    assert_eq!(report.planned, 2);
    assert_eq!(report.materialized, 2);
    assert_eq!(&report.scope, "compile");

    // Files exist at the conventional Maven paths.
    let p_a = target_td
        .path()
        .join("org/example/lib-a/1.0.0/lib-a-1.0.0.jar");
    let p_b = target_td
        .path()
        .join("org/example/lib-b/2.5.1/lib-b-2.5.1.jar");
    assert!(p_a.is_file(), "missing {p_a:?}");
    assert!(p_b.is_file(), "missing {p_b:?}");
    assert_eq!(fs::read(&p_a).unwrap(), b"lib-a-bytes");
    assert_eq!(fs::read(&p_b).unwrap(), b"lib-b-bytes");
}

// =======================================================================
// --target override is honored
// =======================================================================

#[test]
fn target_flag_overrides_default_m2() {
    let fx = Fixture::build(&[ArtifactFixture::jar(
        "com.acme:widget",
        "0.1.0",
        b"widget-bytes",
    )]);
    let target_td = tempfile::tempdir().unwrap();

    let code = run_dispatch(&[
        "barista",
        "--quiet",
        "--root",
        fx.root_str(),
        "pour",
        "--target",
        target_td.path().to_str().unwrap(),
    ]);
    assert_eq!(code, 0);

    let dest = target_td
        .path()
        .join("com/acme/widget/0.1.0/widget-0.1.0.jar");
    assert!(dest.is_file(), "expected {dest:?} to exist");
    assert_eq!(fs::read(&dest).unwrap(), b"widget-bytes");
}

// =======================================================================
// --scope compile filters out test-only deps
// =======================================================================

#[test]
fn scope_compile_filters_out_test_only_entries() {
    let fx = Fixture::build(&[
        ArtifactFixture::jar("g:compile-dep", "1.0.0", b"compile-bytes"),
        ArtifactFixture::jar("g:test-dep", "1.0.0", b"test-bytes").with_scope("test"),
    ]);
    let target_td = tempfile::tempdir().unwrap();

    let report = run_inner(
        &flags_for(&fx),
        &default_pour_args(target_td.path().to_path_buf()),
    )
    .expect("pour ok");

    assert_eq!(report.considered, 2);
    assert_eq!(report.planned, 1, "only the compile dep should be selected");
    assert_eq!(report.materialized, 1);

    assert!(
        target_td
            .path()
            .join("g/compile-dep/1.0.0/compile-dep-1.0.0.jar")
            .is_file()
    );
    assert!(
        !target_td
            .path()
            .join("g/test-dep/1.0.0/test-dep-1.0.0.jar")
            .exists(),
        "test-scoped artifact must not be materialized under --scope compile"
    );
}

#[test]
fn scope_test_includes_only_test_scope() {
    let fx = Fixture::build(&[
        ArtifactFixture::jar("g:compile-dep", "1.0.0", b"compile-bytes"),
        ArtifactFixture::jar("g:test-dep", "1.0.0", b"test-bytes").with_scope("test"),
    ]);
    let target_td = tempfile::tempdir().unwrap();
    let args = PourArgs {
        target: Some(target_td.path().to_path_buf()),
        scope: ScopeArg::Test,
        dry_run: false,
    };

    let report = run_inner(&flags_for(&fx), &args).expect("pour ok");
    assert_eq!(report.planned, 1, "only the test dep should be selected");
    assert!(
        target_td
            .path()
            .join("g/test-dep/1.0.0/test-dep-1.0.0.jar")
            .is_file()
    );
}

// =======================================================================
// --dry-run materializes nothing
// =======================================================================

#[test]
fn dry_run_materializes_nothing() {
    let fx = Fixture::build(&[ArtifactFixture::jar("g:a", "1.0", b"dry-bytes")]);
    let target_td = tempfile::tempdir().unwrap();

    let args = PourArgs {
        target: Some(target_td.path().to_path_buf()),
        scope: ScopeArg::Compile,
        dry_run: true,
    };

    let report = run_inner(&flags_for(&fx), &args).expect("pour ok");
    assert!(report.dry_run);
    assert_eq!(report.planned, 1);
    assert_eq!(report.materialized, 0);

    let would_be = target_td.path().join("g/a/1.0/a-1.0.jar");
    assert_eq!(report.planned_paths, vec![would_be.clone()]);
    assert!(
        !would_be.exists(),
        "--dry-run must not write the artifact: found {would_be:?}"
    );

    // And nothing else under the target dir either.
    let mut found = Vec::new();
    walk(target_td.path(), &mut found);
    assert!(
        found.iter().all(|p| !p.is_file()),
        "dry-run materialized files: {found:?}"
    );
}

// =======================================================================
// Cache miss → NotInCache
// =======================================================================

#[test]
fn missing_from_cache_returns_not_in_cache() {
    // Build a fixture where the lockfile pins two artifacts but the
    // CAS has neither.
    let fx = Fixture::build_without_seeding(&[
        ArtifactFixture::jar("g:missing-a", "1.0", b"never-seeded-a"),
        ArtifactFixture::jar("g:missing-b", "2.0", b"never-seeded-b"),
    ]);
    let target_td = tempfile::tempdir().unwrap();

    let err = run_inner(
        &flags_for(&fx),
        &default_pour_args(target_td.path().to_path_buf()),
    )
    .expect_err("must error on cache miss");

    match err {
        PourError::NotInCache { coords } => {
            assert_eq!(coords.len(), 2);
            assert!(coords.iter().any(|c| c.starts_with("g:missing-a:")));
            assert!(coords.iter().any(|c| c.starts_with("g:missing-b:")));
        }
        other => panic!("expected NotInCache, got {other:?}"),
    }

    // And no partial materialization happened.
    assert!(
        !target_td.path().join("g").exists(),
        "no files should be written when the CAS is empty"
    );
}

// =======================================================================
// Missing lockfile is a structured error
// =======================================================================

#[test]
fn missing_lockfile_returns_structured_error() {
    let fx = Fixture::build_without_lockfile();
    let target_td = tempfile::tempdir().unwrap();
    let err = run_inner(
        &flags_for(&fx),
        &default_pour_args(target_td.path().to_path_buf()),
    )
    .expect_err("must error on missing lockfile");

    match err {
        PourError::NoLockfile { expected_at, hint } => {
            assert_eq!(expected_at, fx.project_root.join("barista.lock"));
            assert!(
                hint.contains("barista pull"),
                "hint should point at `barista pull`: {hint}"
            );
        }
        other => panic!("expected NoLockfile, got {other:?}"),
    }
}

// =======================================================================
// Exit codes
// =======================================================================

#[test]
fn exit_codes_follow_convention_ok() {
    let fx = Fixture::build(&[ArtifactFixture::jar("g:ok", "1.0", b"ok-bytes")]);
    let target_td = tempfile::tempdir().unwrap();
    let code = run_dispatch(&[
        "barista",
        "--quiet",
        "--root",
        fx.root_str(),
        "pour",
        "--target",
        target_td.path().to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "happy path must exit 0");
}

#[test]
fn exit_codes_follow_convention_no_lockfile() {
    let fx = Fixture::build_without_lockfile();
    let target_td = tempfile::tempdir().unwrap();
    let code = run_dispatch(&[
        "barista",
        "--quiet",
        "--root",
        fx.root_str(),
        "pour",
        "--target",
        target_td.path().to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "missing-lockfile is a precondition: exit 2");
}

#[test]
fn exit_codes_follow_convention_cache_miss() {
    let fx = Fixture::build_without_seeding(&[ArtifactFixture::jar(
        "g:missing",
        "1.0",
        b"never-seeded",
    )]);
    let target_td = tempfile::tempdir().unwrap();
    let code = run_dispatch(&[
        "barista",
        "--quiet",
        "--root",
        fx.root_str(),
        "pour",
        "--target",
        target_td.path().to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "cache-miss is a precondition: exit 2");
}

#[test]
fn exit_codes_follow_convention_bad_root() {
    let bogus = tempfile::tempdir().unwrap();
    let does_not_exist = bogus.path().join("nope");
    let code = run_dispatch(&[
        "barista",
        "--quiet",
        "--root",
        does_not_exist.to_str().unwrap(),
        "pour",
    ]);
    assert_eq!(
        code, 2,
        "bad project root is a project-setup precondition: exit 2"
    );
}

// =======================================================================
// Idempotency
// =======================================================================

#[test]
fn pour_is_idempotent() {
    let fx = Fixture::build(&[ArtifactFixture::jar("g:repeat", "1.0", b"repeat-bytes")]);
    let target_td = tempfile::tempdir().unwrap();
    let args = default_pour_args(target_td.path().to_path_buf());

    let r1 = run_inner(&flags_for(&fx), &args).expect("first pour");
    let r2 = run_inner(&flags_for(&fx), &args).expect("second pour");
    assert_eq!(r1.materialized, 1);
    assert_eq!(r2.materialized, 1);

    let dest = target_td.path().join("g/repeat/1.0/repeat-1.0.jar");
    assert!(dest.is_file());
}

// =======================================================================
// CLI flag parsing — ensure new flags accept argv
// =======================================================================

#[test]
fn pour_accepts_dry_run_flag_via_argv() {
    let fx = Fixture::build(&[ArtifactFixture::jar(
        "g:flagcheck",
        "1.0",
        b"flagcheck-bytes",
    )]);
    let target_td = tempfile::tempdir().unwrap();
    let code = run_dispatch(&[
        "barista",
        "--quiet",
        "--root",
        fx.root_str(),
        "pour",
        "--target",
        target_td.path().to_str().unwrap(),
        "--dry-run",
    ]);
    assert_eq!(code, 0);
    // Dry-run wrote nothing.
    assert!(
        !target_td
            .path()
            .join("g/flagcheck/1.0/flagcheck-1.0.jar")
            .exists(),
        "--dry-run must not write artifacts"
    );
}

#[test]
fn pour_accepts_scope_flag_via_argv() {
    let fx = Fixture::build(&[
        ArtifactFixture::jar("g:c", "1.0", b"compile-only"),
        ArtifactFixture::jar("g:t", "1.0", b"test-only").with_scope("test"),
    ]);
    let target_td = tempfile::tempdir().unwrap();
    let code = run_dispatch(&[
        "barista",
        "--quiet",
        "--root",
        fx.root_str(),
        "pour",
        "--target",
        target_td.path().to_str().unwrap(),
        "--scope",
        "test",
    ]);
    assert_eq!(code, 0);
    assert!(target_td.path().join("g/t/1.0/t-1.0.jar").is_file());
    assert!(!target_td.path().join("g/c/1.0/c-1.0.jar").exists());
}

// =======================================================================
// Human-readable summary snapshot
// =======================================================================

#[test]
fn pour_summary_snapshot() {
    let fx = Fixture::build(&[
        ArtifactFixture::jar("g:a", "1.0", b"snap-a"),
        ArtifactFixture::jar("g:b", "2.0", b"snap-b"),
        ArtifactFixture::jar("g:t", "1.0", b"snap-t").with_scope("test"),
    ]);
    let target_td = tempfile::tempdir().unwrap();

    let report = run_inner(
        &flags_for(&fx),
        &default_pour_args(target_td.path().to_path_buf()),
    )
    .expect("pour ok");

    // Stable rendering: replace the volatile target path with a
    // placeholder before snapshotting.
    let summary = report
        .summary()
        .replace(target_td.path().to_str().unwrap(), "<TARGET>");
    insta::assert_snapshot!("pour_summary_compile_two_of_three", summary);

    // Dry-run snapshot too.
    let dry_args = PourArgs {
        target: Some(target_td.path().to_path_buf()),
        scope: ScopeArg::Compile,
        dry_run: true,
    };
    let dry_report = run_inner(&flags_for(&fx), &dry_args).expect("dry-run ok");
    let dry_summary = dry_report
        .summary()
        .replace(target_td.path().to_str().unwrap(), "<TARGET>");
    insta::assert_snapshot!("pour_summary_compile_dry_run", dry_summary);
}

// =======================================================================
// Helpers
// =======================================================================

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
}
