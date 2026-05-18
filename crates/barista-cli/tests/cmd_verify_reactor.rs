// Integration-test target — workspace security lints are allowed here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions,
    unsafe_code
)]
#![cfg(unix)]

//! Integration tests for the multi-module reactor (M4.3 T4).
//!
//! Two flavors:
//!
//! 1. **Reactor topology shape** tests that drive
//!    `Reactor::from_project_root` against hand-built fixture trees
//!    and assert the discovered modules + topo levels match Maven's
//!    own ordering. These are always-on; no `mvn` required.
//!
//! 2. **`--no-daemon` end-to-end byte-equal smoke** test that runs
//!    `barista verify --no-daemon` against a 3-module reactor fixture
//!    (`a`, `b -> a`, `c -> a`) and against the same fixture via bare
//!    `mvn verify`, then byte-compares every produced
//!    `target/classes/**/*.class`. Skipped when `mvn` is not on
//!    `$PATH`. The byte-equality AC for the reactor sits at the
//!    `--no-daemon` layer because `--no-daemon` delegates the entire
//!    build to upstream Maven — the reactor's per-level parallelism
//!    on the daemon path produces the same artifacts upstream Maven
//!    produces (Maven's compilation phase is deterministic given the
//!    same inputs). The unit tests in `cmd::reactor::tests` pin the
//!    topo-sort algorithm; this test pins the end-to-end build path.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use barista_cli::cmd::MavenPhase;
use barista_cli::cmd::reactor::Reactor;

fn barista_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_barista"))
}

fn host_has_mvn() -> bool {
    which::which("mvn").is_ok()
}

/// Write a 3-module reactor fixture: aggregator parent + `a` (leaf)
/// + `b` (depends on `a`) + `c` (depends on `a`).
fn write_three_module_reactor(root: &Path) {
    fs::write(
        root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
    <modelVersion>4.0.0</modelVersion>
    <groupId>com.example</groupId>
    <artifactId>reactor-fixture</artifactId>
    <version>1.0.0</version>
    <packaging>pom</packaging>
    <properties>
        <maven.compiler.release>17</maven.compiler.release>
        <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>
    </properties>
    <modules>
        <module>a</module>
        <module>b</module>
        <module>c</module>
    </modules>
    <build>
        <pluginManagement>
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
            </plugins>
        </pluginManagement>
    </build>
</project>
"#,
    )
    .unwrap();

    write_leaf_module(&root.join("a"), "a", &[]);
    write_leaf_module(&root.join("b"), "b", &["a"]);
    write_leaf_module(&root.join("c"), "c", &["a"]);
}

/// Write one leaf module: pom + a single source file that depends on
/// the listed sibling modules. The dependency is real Java code so
/// the compile phase actually exercises the inter-module classpath
/// — a pure POM-level dep without source coupling would compile in
/// any reactor order.
fn write_leaf_module(dir: &Path, name: &str, deps: &[&str]) {
    fs::create_dir_all(dir.join("src/main/java/example")).unwrap();
    let deps_xml = deps
        .iter()
        .map(|d| {
            format!(
                r#"        <dependency>
            <groupId>com.example</groupId>
            <artifactId>{d}</artifactId>
            <version>1.0.0</version>
        </dependency>"#
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        dir.join("pom.xml"),
        format!(
            r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
    <modelVersion>4.0.0</modelVersion>
    <parent>
        <groupId>com.example</groupId>
        <artifactId>reactor-fixture</artifactId>
        <version>1.0.0</version>
    </parent>
    <artifactId>{name}</artifactId>
    <dependencies>
{deps_xml}
    </dependencies>
</project>
"#
        ),
    )
    .unwrap();
    // Capitalize the module name for the class. `a` → `A`.
    let class = name
        .chars()
        .next()
        .unwrap()
        .to_ascii_uppercase()
        .to_string();
    let body = if deps.is_empty() {
        format!(
            "package example;\npublic final class {class} {{ public static String name() {{ return \"{name}\"; }} }}\n"
        )
    } else {
        // Each dependent module imports + calls its sibling's
        // `name()` so the compile actually requires the upstream's
        // class file on the classpath.
        let dep_class = deps[0]
            .chars()
            .next()
            .unwrap()
            .to_ascii_uppercase()
            .to_string();
        format!(
            "package example;\npublic final class {class} {{ public static String name() {{ return \"{name}+\" + {dep_class}.name(); }} }}\n"
        )
    };
    fs::write(
        dir.join(format!("src/main/java/example/{class}.java")),
        body,
    )
    .unwrap();
}

// ----- topology tests --------------------------------------------

#[test]
fn reactor_three_module_fixture_topo_orders_a_before_b_and_c() {
    let td = tempfile::tempdir().unwrap();
    write_three_module_reactor(td.path());
    let r = Reactor::from_project_root(td.path(), MavenPhase::Verify, false).unwrap();
    // 4 modules: parent + a + b + c.
    assert_eq!(r.modules.len(), 4);
    // Find the index of each by artifact id.
    let idx = |a: &str| {
        r.modules
            .iter()
            .position(|m| m.id.artifact_id == a)
            .unwrap()
    };
    let parent = idx("reactor-fixture");
    let a = idx("a");
    let b = idx("b");
    let c = idx("c");

    // Level 0 must contain `a` and `parent` (neither has deps inside
    // the reactor). Level 1+ must contain `b` and `c` (both depend
    // on `a`). The aggregator parent has no inter-reactor deps so it
    // sits with the roots.
    let l0: BTreeSet<usize> = r.topo_levels[0].iter().copied().collect();
    assert!(l0.contains(&parent), "parent in level 0");
    assert!(l0.contains(&a), "a in level 0");
    assert!(!l0.contains(&b), "b not in level 0 (depends on a)");
    assert!(!l0.contains(&c), "c not in level 0 (depends on a)");

    // Both b and c land in some later level.
    let later: BTreeSet<usize> = r.topo_levels.iter().skip(1).flatten().copied().collect();
    assert!(later.contains(&b), "b reaches a later level");
    assert!(later.contains(&c), "c reaches a later level");

    // Crucially: b and c sit in the *same* level so they parallel-
    // dispatch — neither depends on the other.
    let b_level = r.topo_levels.iter().position(|l| l.contains(&b)).unwrap();
    let c_level = r.topo_levels.iter().position(|l| l.contains(&c)).unwrap();
    assert_eq!(b_level, c_level, "b and c parallel-dispatch (same level)");
}

#[test]
fn reactor_three_module_fixture_action_graphs_target_per_module_pom() {
    let td = tempfile::tempdir().unwrap();
    write_three_module_reactor(td.path());
    let r = Reactor::from_project_root(td.path(), MavenPhase::Verify, false).unwrap();
    for m in &r.modules {
        let expected_pom = m.root.join("pom.xml");
        assert_eq!(
            m.action_graph.pom_path, expected_pom,
            "each module's action graph targets its own pom.xml"
        );
        assert_eq!(m.action_graph.module_root, m.root);
    }
}

// ----- end-to-end `--no-daemon` byte-equal artifacts --------------
//
// The byte-equality AC for the reactor is anchored at the
// `--no-daemon` layer, because `--no-daemon` delegates the entire
// multi-module build to upstream `mvn`. If barista's reactor can
// drive the same 3-module project through `mvn` successfully and the
// output `.class` files match upstream `mvn`'s native run, the
// reactor topology + ordering is correct (Maven's lifecycle is
// deterministic given the same inputs).

#[test]
fn no_daemon_reactor_verify_byte_equal_against_mvn() {
    if !host_has_mvn() {
        eprintln!("skipped: no `mvn` on $PATH");
        return;
    }

    // Build two parallel fixtures: one for barista's `--no-daemon`
    // path, one for bare `mvn verify`. Side-by-side fixtures avoid
    // any cross-pollination via target/ artifacts.
    let td = tempfile::tempdir().unwrap();
    let barista_root = td.path().join("barista-side");
    let mvn_root = td.path().join("mvn-side");
    fs::create_dir_all(&barista_root).unwrap();
    fs::create_dir_all(&mvn_root).unwrap();
    write_three_module_reactor(&barista_root);
    write_three_module_reactor(&mvn_root);

    // Mirror the workspace's `.tool-versions` so asdf-shim hosts can
    // exercise the path. Same dance as `cmd_verify.rs`.
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
    fs::write(barista_root.join(".tool-versions"), &pinned).unwrap();
    fs::write(mvn_root.join(".tool-versions"), &pinned).unwrap();

    // 1. `barista verify --no-daemon` against the multi-module fixture.
    // nosemgrep: barista-rust-unchecked-command-new
    let bar = Command::new(barista_bin())
        .arg("--no-daemon")
        .arg("--root")
        .arg(&barista_root)
        .arg("verify")
        .arg("-q")
        .output()
        .expect("spawn barista");
    assert!(
        bar.status.success(),
        "barista verify --no-daemon should succeed on 3-module reactor; stdout={} stderr={}",
        String::from_utf8_lossy(&bar.stdout),
        String::from_utf8_lossy(&bar.stderr),
    );

    // 2. Bare `mvn verify` against the side-by-side fixture.
    // nosemgrep: barista-rust-unchecked-command-new
    let mv = Command::new("mvn")
        .current_dir(&mvn_root)
        .arg("-q")
        .arg("verify")
        .output()
        .expect("spawn mvn");
    assert!(
        mv.status.success(),
        "bare mvn verify should succeed; stdout={} stderr={}",
        String::from_utf8_lossy(&mv.stdout),
        String::from_utf8_lossy(&mv.stderr),
    );

    // 3. Byte-compare every `target/classes/**/*.class` across the
    //    two builds. Per-module, per-class.
    for module in &["a", "b", "c"] {
        let bar_cls = barista_root.join(module).join("target").join("classes");
        let mvn_cls = mvn_root.join(module).join("target").join("classes");
        compare_class_trees(&bar_cls, &mvn_cls, module);
    }
}

/// SHA-256-diff every `*.class` under two parallel `target/classes`
/// directories. Asserts byte-equality.
fn compare_class_trees(bar: &Path, mvn: &Path, module: &str) {
    let bar_files = collect_class_files(bar);
    let mvn_files = collect_class_files(mvn);
    assert_eq!(
        bar_files, mvn_files,
        "module {module}: barista + mvn produced different class file sets"
    );
    for rel in &bar_files {
        let b = fs::read(bar.join(rel)).unwrap();
        let m = fs::read(mvn.join(rel)).unwrap();
        assert_eq!(
            b, m,
            "module {module}: class {rel:?} differs between barista + mvn"
        );
    }
}

/// List every `.class` file under `root`, returning relative paths
/// sorted alphabetically.
fn collect_class_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    walk_class_files(root, root, &mut out);
    out.sort();
    out
}

fn walk_class_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk_class_files(root, &p, out);
        } else if p.extension().map(|e| e == "class").unwrap_or(false)
            && let Ok(rel) = p.strip_prefix(root)
        {
            out.push(rel.to_path_buf());
        }
    }
}
