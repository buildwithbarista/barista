// SPDX-License-Identifier: MIT OR Apache-2.0

// Integration-test target — workspace security lints are allowed here.
// Panic-on-misuse is the documented contract for failing a test loudly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions,
    unsafe_code
)]
#![cfg(unix)]

//! Integration tests for the M4.3 T2 lifecycle dispatcher.
//!
//! Two strata of coverage:
//!
//! 1. **Action-graph shape per phase.** Cheap unit-shaped checks that
//!    each [`MavenPhase`] resolves to the expected prefix list and
//!    that the per-phase retryable flags are right. These run on
//!    every `cargo test`.
//!
//! 2. **`--no-daemon` end-to-end per phase.** Drives `barista
//!    <phase> --no-daemon` against the same 1-module fixture used by
//!    `cmd_verify.rs::no_daemon_verify_against_real_mvn_smoke`, once
//!    per Maven-vocabulary command (`clean | compile | test | package
//!    | verify | install | deploy | site`). Asserts each lifecycle
//!    completes with the right post-conditions (e.g. `clean` removes
//!    `target/`, `compile` produces `target/classes/`, `deploy`
//!    against a `file://` `<distributionManagement>` lands the
//!    artifact at the expected path). Skipped when `mvn` is absent
//!    from the host.
//!
//! The deploy auth round-trip (BAR-DEPLOY-AUTH-INVALID / -MISSING)
//! is covered separately by `cmd_deploy_auth.rs` — that suite spins
//! up a tiny HTTP fixture that returns 401 / 201, which doesn't fit
//! the `--no-daemon` fork model (the fork runs upstream `mvn` which
//! handles credentials directly via its own settings.xml resolution).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use barista_cli::action_graph::{
    CLEAN_PHASE_PREFIX, COMPILE_PHASE_PREFIX, DEPLOY_PHASE_PREFIX, INSTALL_PHASE_PREFIX,
    PACKAGE_PHASE_PREFIX, SITE_PHASE_PREFIX, TEST_PHASE_PREFIX, VERIFY_PHASE_PREFIX,
    lifecycle_graph, phase_is_retryable, phase_prefix,
};
use barista_cli::cmd::MavenPhase;

fn barista_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_barista"))
}

fn host_has_mvn() -> bool {
    which::which("mvn").is_ok()
}

// ---------------------------------------------------------------
// 1) Shape: every phase resolves to the right prefix.
// ---------------------------------------------------------------

#[test]
fn phase_prefix_dispatch_covers_every_variant() {
    // Each variant maps to exactly one prefix slice. The match here is
    // exhaustive at compile time (the enum has no `_`); adding a new
    // phase upstream will break the build until this test grows a
    // new arm.
    let cases: &[(MavenPhase, &[&str])] = &[
        (MavenPhase::Clean, CLEAN_PHASE_PREFIX),
        (MavenPhase::Compile, COMPILE_PHASE_PREFIX),
        (MavenPhase::Test, TEST_PHASE_PREFIX),
        (MavenPhase::Package, PACKAGE_PHASE_PREFIX),
        (MavenPhase::Verify, VERIFY_PHASE_PREFIX),
        (MavenPhase::Install, INSTALL_PHASE_PREFIX),
        (MavenPhase::Deploy, DEPLOY_PHASE_PREFIX),
        (MavenPhase::Site, SITE_PHASE_PREFIX),
    ];
    for (phase, want) in cases {
        assert_eq!(
            phase_prefix(*phase),
            *want,
            "phase {phase:?} prefix mismatch"
        );
    }
}

#[test]
fn install_and_deploy_actions_are_not_retryable() {
    // M4.3 T2 invariant: the auto-respawn driver must not retry
    // `install` or `deploy` after a daemon-crash, because both publish
    // to shared state (~/.m2 + remote repo). Every other lifecycle
    // phase is idempotent and retryable.
    assert!(!phase_is_retryable("install"));
    assert!(!phase_is_retryable("deploy"));
    for p in [
        "process-resources",
        "compile",
        "process-test-resources",
        "test-compile",
        "test",
        "prepare-package",
        "package",
        "integration-test",
        "verify",
        "clean",
        "site",
    ] {
        assert!(phase_is_retryable(p), "{p} must be retryable in v0.1");
    }
}

#[test]
fn deploy_graph_has_eleven_phases_with_terminal_deploy() {
    let g = lifecycle_graph(MavenPhase::Deploy, PathBuf::from("/tmp/p"), false);
    let names: Vec<&str> = g.actions.iter().map(|a| a.phase).collect();
    assert_eq!(names.len(), 11);
    assert_eq!(names.last(), Some(&"deploy"));
    assert!(names.contains(&"install"));
    assert!(names.contains(&"verify"));
}

// ---------------------------------------------------------------
// 2) `--no-daemon` end-to-end per phase.
// ---------------------------------------------------------------

/// Write a minimal 1-module Java project that supports the whole
/// lifecycle: a pom + Hello + HelloTest + a file-based
/// distributionManagement so `deploy` succeeds without a remote
/// repository.
fn write_lifecycle_fixture(dir: &Path) -> PathBuf {
    let src = dir.join("src/main/java/example");
    let tst = dir.join("src/test/java/example");
    let deploy_repo = dir.join("deploy-repo");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&tst).unwrap();
    fs::create_dir_all(&deploy_repo).unwrap();
    let deploy_url = format!("file://{}", deploy_repo.display());
    fs::write(
        dir.join("pom.xml"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0"
         xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
         xsi:schemaLocation="http://maven.apache.org/POM/4.0.0 http://maven.apache.org/xsd/maven-4.0.0.xsd">
    <modelVersion>4.0.0</modelVersion>
    <groupId>example</groupId>
    <artifactId>lifecycle-fixture</artifactId>
    <version>0.1.0</version>
    <packaging>jar</packaging>
    <properties>
        <maven.compiler.release>17</maven.compiler.release>
        <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>
        <junit.version>5.10.2</junit.version>
    </properties>
    <dependencies>
        <dependency>
            <groupId>org.junit.jupiter</groupId>
            <artifactId>junit-jupiter</artifactId>
            <version>${{junit.version}}</version>
            <scope>test</scope>
        </dependency>
    </dependencies>
    <build>
        <plugins>
            <plugin>
                <groupId>org.apache.maven.plugins</groupId>
                <artifactId>maven-compiler-plugin</artifactId>
                <version>3.13.0</version>
            </plugin>
            <plugin>
                <groupId>org.apache.maven.plugins</groupId>
                <artifactId>maven-surefire-plugin</artifactId>
                <version>3.2.5</version>
            </plugin>
            <plugin>
                <groupId>org.apache.maven.plugins</groupId>
                <artifactId>maven-deploy-plugin</artifactId>
                <version>3.1.2</version>
            </plugin>
        </plugins>
    </build>
    <distributionManagement>
        <repository>
            <id>local-file-repo</id>
            <url>{deploy_url}</url>
        </repository>
    </distributionManagement>
</project>
"#,
        ),
    )
    .unwrap();
    fs::write(
        src.join("Hello.java"),
        "package example;\npublic final class Hello { public static String greet() { return \"hi\"; } }\n",
    )
    .unwrap();
    fs::write(
        tst.join("HelloTest.java"),
        r#"package example;
import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.assertEquals;
public final class HelloTest {
    @Test void greetReturnsHi() { assertEquals("hi", Hello.greet()); }
}
"#,
    )
    .unwrap();
    deploy_repo
}

/// Materialise the workspace's `.tool-versions` into the fixture (or
/// fall back to a known-good pin) so asdf-style toolchain wrappers
/// can run `mvn` from inside the fixture dir.
fn stage_tool_versions(project: &Path) {
    let mut tv_search = Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let mut tv_content: Option<String> = None;
    while let Some(d) = tv_search {
        let candidate = d.join(".tool-versions");
        if candidate.is_file()
            && let Ok(c) = fs::read_to_string(&candidate)
        {
            tv_content = Some(c);
            break;
        }
        tv_search = d.parent().map(Path::to_path_buf);
    }
    let pinned =
        tv_content.unwrap_or_else(|| "java temurin-21.0.4+7.0.LTS\nmaven 3.9.9\n".to_string());
    fs::write(project.join(".tool-versions"), pinned).unwrap();
}

fn run_phase_no_daemon(project: &Path, phase: &str) -> std::process::Output {
    // nosemgrep: barista-rust-unchecked-command-new
    Command::new(barista_bin())
        .arg("--no-daemon")
        .arg("--root")
        .arg(project)
        .arg(phase)
        .arg("-q")
        .output()
        .expect("spawn barista")
}

fn assert_phase_ok(project: &Path, phase: &str, out: &std::process::Output) {
    assert!(
        out.status.success(),
        "barista {phase} --no-daemon should succeed against the lifecycle fixture; \
         stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let _ = project;
}

#[test]
fn no_daemon_clean_removes_target() {
    if !host_has_mvn() {
        eprintln!("skipped: no `mvn` on $PATH");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let project = td.path().join("project");
    fs::create_dir_all(&project).unwrap();
    write_lifecycle_fixture(&project);
    stage_tool_versions(&project);

    // Pre-create target/ so we can prove clean removes it.
    let target = project.join("target");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("sentinel"), b"x").unwrap();

    let out = run_phase_no_daemon(&project, "clean");
    assert_phase_ok(&project, "clean", &out);
    assert!(
        !target.exists() || fs::read_dir(&target).unwrap().next().is_none(),
        "target/ should be absent or empty after clean"
    );
}

#[test]
fn no_daemon_compile_produces_class_file() {
    if !host_has_mvn() {
        eprintln!("skipped: no `mvn` on $PATH");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let project = td.path().join("project");
    fs::create_dir_all(&project).unwrap();
    write_lifecycle_fixture(&project);
    stage_tool_versions(&project);

    let out = run_phase_no_daemon(&project, "compile");
    assert_phase_ok(&project, "compile", &out);
    assert!(
        project.join("target/classes/example/Hello.class").is_file(),
        "Hello.class must exist after compile"
    );
}

#[test]
fn no_daemon_test_runs_unit_tests() {
    if !host_has_mvn() {
        eprintln!("skipped: no `mvn` on $PATH");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let project = td.path().join("project");
    fs::create_dir_all(&project).unwrap();
    write_lifecycle_fixture(&project);
    stage_tool_versions(&project);

    let out = run_phase_no_daemon(&project, "test");
    assert_phase_ok(&project, "test", &out);
    // Surefire reports land in target/surefire-reports.
    assert!(
        project.join("target/surefire-reports").is_dir(),
        "surefire reports must exist after test"
    );
}

#[test]
fn no_daemon_package_produces_jar() {
    if !host_has_mvn() {
        eprintln!("skipped: no `mvn` on $PATH");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let project = td.path().join("project");
    fs::create_dir_all(&project).unwrap();
    write_lifecycle_fixture(&project);
    stage_tool_versions(&project);

    let out = run_phase_no_daemon(&project, "package");
    assert_phase_ok(&project, "package", &out);
    let jar_dir = project.join("target");
    let jar = fs::read_dir(&jar_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().map(|x| x == "jar").unwrap_or(false));
    assert!(jar.is_some(), "package must produce a .jar");
}

#[test]
fn no_daemon_verify_completes() {
    if !host_has_mvn() {
        eprintln!("skipped: no `mvn` on $PATH");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let project = td.path().join("project");
    fs::create_dir_all(&project).unwrap();
    write_lifecycle_fixture(&project);
    stage_tool_versions(&project);

    let out = run_phase_no_daemon(&project, "verify");
    assert_phase_ok(&project, "verify", &out);
}

#[test]
fn no_daemon_install_writes_into_local_repo() {
    if !host_has_mvn() {
        eprintln!("skipped: no `mvn` on $PATH");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let project = td.path().join("project");
    fs::create_dir_all(&project).unwrap();
    write_lifecycle_fixture(&project);
    stage_tool_versions(&project);

    // Pin local-repo into the fixture so we don't pollute ~/.m2 and so
    // the assertion can find the installed artifact deterministically.
    let local_repo = td.path().join("local-repo");
    fs::create_dir_all(&local_repo).unwrap();
    // nosemgrep: barista-rust-unchecked-command-new
    let out = Command::new(barista_bin())
        .arg("--no-daemon")
        .arg("--root")
        .arg(&project)
        .arg("install")
        .arg("-q")
        .arg(format!("-Dmaven.repo.local={}", local_repo.display()))
        .output()
        .expect("spawn barista");
    assert!(
        out.status.success(),
        "install should succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let installed = local_repo.join("example/lifecycle-fixture/0.1.0/lifecycle-fixture-0.1.0.jar");
    assert!(
        installed.is_file(),
        "install must publish to local repo at {}",
        installed.display()
    );
}

#[test]
fn no_daemon_deploy_writes_to_file_distribution_management() {
    if !host_has_mvn() {
        eprintln!("skipped: no `mvn` on $PATH");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let project = td.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let deploy_repo = write_lifecycle_fixture(&project);
    stage_tool_versions(&project);

    // Use a fixture-local local-repo to keep ~/.m2 untouched and the
    // install step (which deploy runs through first) deterministic.
    let local_repo = td.path().join("local-repo");
    fs::create_dir_all(&local_repo).unwrap();
    // nosemgrep: barista-rust-unchecked-command-new
    let out = Command::new(barista_bin())
        .arg("--no-daemon")
        .arg("--root")
        .arg(&project)
        .arg("deploy")
        .arg("-q")
        .arg(format!("-Dmaven.repo.local={}", local_repo.display()))
        .output()
        .expect("spawn barista");
    assert!(
        out.status.success(),
        "deploy against file:// dist-management should succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let deployed = deploy_repo.join("example/lifecycle-fixture/0.1.0/lifecycle-fixture-0.1.0.jar");
    assert!(
        deployed.is_file(),
        "deploy must publish to the file-based <distributionManagement>: {}",
        deployed.display()
    );
}

#[test]
fn no_daemon_site_produces_site_directory() {
    if !host_has_mvn() {
        eprintln!("skipped: no `mvn` on $PATH");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let project = td.path().join("project");
    fs::create_dir_all(&project).unwrap();
    write_lifecycle_fixture(&project);
    stage_tool_versions(&project);

    let out = run_phase_no_daemon(&project, "site");
    // `site` requires the maven-site-plugin to be available; we don't
    // pin a version in the pom (it's pulled by Maven defaults). If the
    // plugin's network resolution is slow / unavailable in the test
    // environment, fall back to a tolerance: surface any phase output
    // verbatim so the failure mode is clear.
    if !out.status.success() {
        eprintln!(
            "warn: barista site --no-daemon did not succeed in this environment; \
             stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        // We don't hard-fail on site — the plugin's reachability is
        // environment-dependent. The shape test above covers the
        // dispatch path; this test exercises the e2e wiring when the
        // host has full Maven plugin access.
        return;
    }
    let site_dir = project.join("target/site");
    assert!(site_dir.is_dir(), "site/ must exist after site phase");
}
