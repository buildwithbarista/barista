// SPDX-License-Identifier: MIT OR Apache-2.0

// Integration-test target — workspace security lints relaxed
// (`unwrap`/`expect`/`panic!` are the documented failure contract
// for assertions). Mirror of the allow block at the top of
// `cmd_no_daemon.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! M4.3 T6 — `--ci` end-to-end reproducibility acceptance test.
//!
//! Drives `barista verify --ci --no-daemon` 5 times in independent
//! temp directories against a 1-module Java fixture and SHA-256-diffs
//! every produced `.class` file under `target/classes/` plus the
//! packaged JAR under `target/`. The AC ([T] from M4.3) is
//! byte-identical output across the 5 runs.
//!
//! ## Why `--no-daemon` instead of the daemon path
//!
//! The daemon path (`barista verify --ci`) would also satisfy the AC
//! once the embedded Maven 4 distribution is staged on the test host
//! (the daemon stages it from `$BARISTA_MAVEN_HOME` / per the M4.0
//! spike rationale). On bare CI hosts and the workstation toolchain
//! that test is gated by mvn-availability; the `--no-daemon` path is
//! the unconditional surface that always exercises the determinism
//! seam end-to-end.
//!
//! Inductively, byte-equality on the `--no-daemon` path covers the
//! daemon path too: the daemon's `EmbeddedMaven.buildMavenArgs`
//! (M4.2 T3) translates the same wire-shape `ActionRequest.environment`
//! / `ActionRequest.system_properties` maps into the same Maven CLI
//! flags the forked-mvn path already constructs. Both paths run
//! against the same Maven distribution downstream — if the forked
//! path is deterministic, the daemon path is too. The daemon path
//! gets its own coverage via the unit tests in `cmd::verify::tests`
//! that exercise the request-builder seam.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolve the test's `barista` binary path (cargo sets this for
/// integration tests with `[[bin]]` targets).
fn barista_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_barista"))
}

/// Returns whether the host has a usable `mvn` on `$PATH`. The
/// reproducibility test forks `mvn` via `--no-daemon`, so we skip on
/// hosts that don't have it.
fn host_has_mvn() -> bool {
    which::which("mvn").is_ok()
}

/// Write a 1-module Java project at `dir` configured for
/// reproducible builds: the pom pins the compiler + jar plugins, and
/// the build relies on `project.build.outputTimestamp` (injected by
/// `--ci`) for archive determinism.
///
/// Maven's reproducible-builds plugin chain stamps the same
/// timestamp into JAR `META-INF/MANIFEST.MF` and ZIP entry headers
/// when the property is set. Without it, `maven-archiver` records
/// the wall-clock build time, which differs across runs.
fn write_repro_fixture(dir: &Path) {
    let src = dir.join("src/main/java/example");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        dir.join("pom.xml"),
        // Minimal 1-module Java project. Compiler and jar plugins
        // pinned so the bytecode shape and packaging behavior are
        // host-independent. `outputTimestamp` placeholder is left
        // empty so `--ci`'s `-Dproject.build.outputTimestamp=...`
        // override drives the value at invocation time.
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>example</groupId>
  <artifactId>ci-repro</artifactId>
  <version>0.1.0</version>
  <packaging>jar</packaging>
  <properties>
    <maven.compiler.source>17</maven.compiler.source>
    <maven.compiler.target>17</maven.compiler.target>
    <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>
  </properties>
  <build>
    <plugins>
      <plugin>
        <artifactId>maven-compiler-plugin</artifactId>
        <version>3.13.0</version>
      </plugin>
      <plugin>
        <artifactId>maven-jar-plugin</artifactId>
        <version>3.4.1</version>
      </plugin>
    </plugins>
  </build>
</project>
"#,
    )
    .unwrap();
    fs::write(
        src.join("Hello.java"),
        "package example;\npublic final class Hello { public static String greet() { return \"hi\"; } }\n",
    )
    .unwrap();
    // asdf-style toolchain wrappers refuse to dispatch outside a dir
    // with `.tool-versions`. Pin the same versions the workspace
    // does so the test runs on the canonical CI image.
    fs::write(
        dir.join(".tool-versions"),
        "java temurin-21.0.4+7.0.LTS\nmaven 3.9.9\n",
    )
    .unwrap();
}

/// SHA-256 a file by shelling out to `shasum -a 256` (universal on
/// macOS + Linux runners) or `sha256sum` (Linux). No `sha2` dep
/// pulled into the integration-test target.
fn sha256_file(path: &Path) -> String {
    if let Ok(out) = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout);
        if let Some(hex) = s.split_whitespace().next() {
            return hex.to_string();
        }
    }
    if let Ok(out) = Command::new("sha256sum").arg(path).output()
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout);
        if let Some(hex) = s.split_whitespace().next() {
            return hex.to_string();
        }
    }
    panic!("no shasum/sha256sum found on PATH; can't hash {path:?}");
}

/// Recursively collect every `.class` file under `dir`, relative
/// to `dir`. Sorted for determinism.
fn collect_class_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !dir.exists() {
        return out;
    }
    fn walk(d: &Path, base: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(d).unwrap() {
            let entry = entry.unwrap();
            let p = entry.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("class") {
                out.push(p.strip_prefix(base).unwrap().to_path_buf());
            }
        }
    }
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Run `barista verify --ci --no-daemon` against `project_dir`. The
/// test pins `BARISTA_SOURCE_DATE_EPOCH=1577836800` (2020-01-01) on
/// every run so the epoch is hermetic across worktrees and git
/// states. Epoch zero would be syntactically valid but `maven-jar-
/// plugin`'s validator rejects anything outside
/// `1980-01-02 .. 2099-12-31`, so we use a value inside the range.
fn run_ci_verify(project_dir: &Path) -> std::process::Output {
    Command::new(barista_bin())
        // `--ci` macro expansion adds --frozen --output json --quiet
        // --no-color; we run `package` instead of `verify` because the
        // 1-module fixture carries no test sources. `package` exercises
        // the load-bearing JAR-timestamp determinism path the
        // M4.3 T6 implementation is meant to guarantee.
        .arg("--ci")
        .arg("--no-daemon")
        .arg("--root")
        .arg(project_dir)
        .arg("package")
        // Pin the epoch so the assertion is reproducible across
        // hosts and git states.
        .env("BARISTA_SOURCE_DATE_EPOCH", "1577836800")
        .output()
        .expect("spawn barista")
}

#[test]
fn ci_verify_byte_identical_across_5_consecutive_runs() {
    if !host_has_mvn() {
        eprintln!("skipping: no `mvn` on PATH");
        return;
    }

    const RUNS: usize = 5;
    let mut tempdirs: Vec<tempfile::TempDir> = Vec::with_capacity(RUNS);
    let mut projects: Vec<PathBuf> = Vec::with_capacity(RUNS);

    for _ in 0..RUNS {
        let td = tempfile::tempdir().unwrap();
        let project = td.path().join("project");
        fs::create_dir_all(&project).unwrap();
        write_repro_fixture(&project);
        projects.push(project);
        tempdirs.push(td);
    }

    // Execute each run sequentially. Parallel execution would
    // share `~/.m2/repository` lockfiles and produce false
    // negatives unrelated to the determinism contract under test.
    for (i, project) in projects.iter().enumerate() {
        let out = run_ci_verify(project);
        assert!(
            out.status.success(),
            "run {}: barista --ci --no-daemon package failed: stdout={} stderr={}",
            i,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // Hash every produced .class file across the 5 runs; relative
    // path set is the same on all runs by construction (the fixture
    // is identical) — assert the hashes match.
    let baseline_classes_dir = projects[0].join("target").join("classes");
    let baseline_files = collect_class_files(&baseline_classes_dir);
    assert!(
        !baseline_files.is_empty(),
        "run 0 produced no .class files; verify the fixture compiled successfully",
    );

    for rel in &baseline_files {
        let base_hash = sha256_file(&baseline_classes_dir.join(rel));
        for (i, project) in projects.iter().enumerate().skip(1) {
            let other_hash = sha256_file(&project.join("target").join("classes").join(rel));
            assert_eq!(
                base_hash, other_hash,
                "SHA-256 mismatch on {rel:?} between run 0 and run {i}:\n  \
                 run 0: {base_hash}\n  \
                 run {i}: {other_hash}\n  \
                 --ci should produce byte-identical .class output across runs",
            );
        }
    }

    // Hash the packaged JAR across runs. This is the *load-bearing*
    // assertion for `SOURCE_DATE_EPOCH` / `project.build.outputTimestamp`
    // propagation — the JAR's `META-INF/MANIFEST.MF` and entry headers
    // would otherwise embed wall-clock build timestamps that differ
    // run-to-run.
    let baseline_jar = find_jar(&projects[0].join("target"));
    let base_jar_hash = sha256_file(&baseline_jar);
    for (i, project) in projects.iter().enumerate().skip(1) {
        let other_jar = find_jar(&project.join("target"));
        let other_jar_hash = sha256_file(&other_jar);
        assert_eq!(
            base_jar_hash, other_jar_hash,
            "SHA-256 mismatch on packaged JAR between run 0 ({baseline_jar:?}) \
             and run {i} ({other_jar:?}):\n  \
             run 0: {base_jar_hash}\n  \
             run {i}: {other_jar_hash}\n  \
             --ci should produce byte-identical packaged JARs across runs; \
             this typically means SOURCE_DATE_EPOCH / project.build.outputTimestamp \
             didn't reach maven-archiver",
        );
    }
}

/// Find the single `.jar` in `dir`. Panics on zero / multiple
/// matches so a fixture mistake fails loudly rather than silently
/// hashing the wrong file.
fn find_jar(dir: &Path) -> PathBuf {
    let jars: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jar"))
        .collect();
    match jars.len() {
        0 => panic!("no .jar in {dir:?}; package phase should have produced one"),
        1 => jars.into_iter().next().unwrap(),
        n => panic!("expected exactly one .jar in {dir:?}, found {n}: {jars:?}"),
    }
}
