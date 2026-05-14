// Integration-test / example / benchmark target — workspace security
// lints are allowed here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`)
// is the documented contract for failing a test loudly. This allow block
// keeps the crate root's `#![allow(...)]` from being silently dropped by
// the separate compilation unit each test file forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Q9 spike: run BFS+Skipper against shapes derived from real corpus
//! projects.
//!
//! For each of the 5 currently-configured corpus projects, this
//! example encodes a small graph derived from `mvn dependency:tree
//! -Dverbose` output and runs the BFS+Skipper algorithm to see
//! whether it produces the same resolved version per coordinate.
//!
//! The graphs are *hand-encoded* from real `mvn dependency:tree`
//! output captured against the materialized corpus checkouts. They
//! cover the project's declared deps and one or two levels of
//! transitives — enough to exercise the algorithm on real-shaped
//! input, not enough to constitute a full resolver port.
//!
//! Run: cargo run --example spike-corpus
//!
//! Research scratch code; the production resolver lives in
//! `src/lib.rs` and lands in a subsequent milestone. The algorithm
//! reproduced here is the same one demonstrated in `spike-bfs.rs`,
//! intentionally copied rather than extracted into a shared module so
//! that the two spikes can be deleted independently.

use std::collections::{HashMap, HashSet, VecDeque};

// --- Data model ---------------------------------------------------------

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Coords {
    group: String,
    artifact: String,
}

impl Coords {
    fn new(group: &str, artifact: &str) -> Self {
        Self {
            group: group.into(),
            artifact: artifact.into(),
        }
    }
}

impl std::fmt::Display for Coords {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.group, self.artifact)
    }
}

#[derive(Clone, Debug)]
struct Dep {
    coords: Coords,
    version: String,
}

impl Dep {
    fn new(group: &str, artifact: &str, version: &str) -> Self {
        Self {
            coords: Coords::new(group, artifact),
            version: version.into(),
        }
    }
}

#[derive(Clone, Debug)]
struct PomEntry {
    deps: Vec<Dep>,
}

type Repo = HashMap<(Coords, String), PomEntry>;

fn pom(deps: Vec<Dep>) -> PomEntry {
    PomEntry { deps }
}

// --- BFS+Skipper resolver (mirror of spike-bfs.rs) ----------------------

fn resolve(root: Dep, repo: &Repo) -> HashMap<Coords, String> {
    let mut winners: HashMap<Coords, (String, usize)> = HashMap::new();
    let mut queue: VecDeque<(Dep, usize)> = VecDeque::new();

    winners.insert(root.coords.clone(), (root.version.clone(), 0));
    queue.push_back((root, 0));

    while let Some((dep, depth)) = queue.pop_front() {
        if let Some((won_ver, won_depth)) = winners.get(&dep.coords) {
            if *won_depth < depth || (*won_depth == depth && won_ver != &dep.version) {
                continue;
            }
        }

        let key = (dep.coords.clone(), dep.version.clone());
        let Some(entry) = repo.get(&key) else {
            continue;
        };

        let child_depth = depth + 1;
        for child in &entry.deps {
            match winners.get(&child.coords) {
                Some((_, won_depth)) if *won_depth <= child_depth => {
                    // Already a winner at shallower-or-equal depth.
                }
                _ => {
                    winners.insert(child.coords.clone(), (child.version.clone(), child_depth));
                    queue.push_back((child.clone(), child_depth));
                }
            }
        }
    }

    winners.into_iter().map(|(c, (v, _))| (c, v)).collect()
}

// --- Test driver --------------------------------------------------------

/// One expected `coord -> version` mapping that mvn produces.
type Expected = HashMap<Coords, String>;

fn coord(g: &str, a: &str) -> Coords {
    Coords::new(g, a)
}

fn run_corpus_case(name: &str, root: Dep, repo: Repo, expected: Expected) -> (usize, usize) {
    println!("=== {name} ===");
    println!(
        "  graph: {} pom entries; root = {}:{}",
        repo.len(),
        root.coords,
        root.version
    );

    let resolved = resolve(root, &repo);

    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut checked: HashSet<Coords> = HashSet::new();

    // Compare each expected coord to the resolved version.
    let mut expected_keys: Vec<_> = expected.keys().collect();
    expected_keys.sort_by(|a, b| a.group.cmp(&b.group).then(a.artifact.cmp(&b.artifact)));

    for c in expected_keys {
        let exp_v = &expected[c];
        checked.insert(c.clone());
        match resolved.get(c) {
            Some(got) if got == exp_v => {
                pass += 1;
            }
            Some(got) => {
                fail += 1;
                println!("  MISMATCH {c}: expected {exp_v}, got {got}");
            }
            None => {
                fail += 1;
                println!("  MISSING  {c}: expected {exp_v}, resolver did not include it");
            }
        }
    }

    // Surface unexpected extras (artifacts the resolver pulled in that
    // mvn's tree did not list at all). This catches over-inclusion.
    for (c, v) in &resolved {
        if !checked.contains(c) {
            // It's normal for the root itself to be in `resolved` but
            // not in `expected`; allow that silently.
            println!("  EXTRA    {c}:{v} (not in expected mvn output)");
        }
    }

    if fail == 0 {
        println!("  PASS ({pass}/{} checks)", pass + fail);
    } else {
        println!(
            "  FAIL ({pass} pass / {fail} fail of {} checks)",
            pass + fail
        );
    }
    (pass, fail)
}

// --- Fixture: commons-lang 3.14.0 --------------------------------------
//
// Source: `mvn -B dependency:tree -Dverbose` against
// test-corpus/commons-lang/checkout (rel/commons-lang-3.14.0).
//
// Direct deps include JUnit Jupiter 5.10.0, junit-pioneer 1.9.1,
// hamcrest, easymock, commons-text, JMH 1.37, jsr305. No conflicts;
// mvn's tree shows only "omitted for duplicate" notes (same-version
// dedup), never "omitted for conflict". This is a clean baseline.

fn commons_lang_graph() -> (Dep, Repo, Expected) {
    let root = Dep::new("org.apache.commons", "commons-lang3", "3.14.0");
    let mut r: Repo = HashMap::new();

    r.insert(
        (
            coord("org.apache.commons", "commons-lang3"),
            "3.14.0".into(),
        ),
        pom(vec![
            Dep::new("org.junit.jupiter", "junit-jupiter", "5.10.0"),
            Dep::new("org.junit-pioneer", "junit-pioneer", "1.9.1"),
            Dep::new("org.hamcrest", "hamcrest", "2.2"),
            Dep::new("org.easymock", "easymock", "5.2.0"),
            Dep::new("org.apache.commons", "commons-text", "1.11.0"),
            Dep::new("org.openjdk.jmh", "jmh-core", "1.37"),
            Dep::new("org.openjdk.jmh", "jmh-generator-annprocess", "1.37"),
            Dep::new("com.google.code.findbugs", "jsr305", "3.0.2"),
        ]),
    );
    r.insert(
        (coord("org.junit.jupiter", "junit-jupiter"), "5.10.0".into()),
        pom(vec![
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.0"),
            Dep::new("org.junit.jupiter", "junit-jupiter-params", "5.10.0"),
            Dep::new("org.junit.jupiter", "junit-jupiter-engine", "5.10.0"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.jupiter", "junit-jupiter-api"),
            "5.10.0".into(),
        ),
        pom(vec![
            Dep::new("org.opentest4j", "opentest4j", "1.3.0"),
            Dep::new("org.junit.platform", "junit-platform-commons", "1.10.0"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.jupiter", "junit-jupiter-params"),
            "5.10.0".into(),
        ),
        pom(vec![
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.0"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.jupiter", "junit-jupiter-engine"),
            "5.10.0".into(),
        ),
        pom(vec![
            Dep::new("org.junit.platform", "junit-platform-engine", "1.10.0"),
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.0"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.platform", "junit-platform-engine"),
            "1.10.0".into(),
        ),
        pom(vec![
            Dep::new("org.opentest4j", "opentest4j", "1.3.0"),
            Dep::new("org.junit.platform", "junit-platform-commons", "1.10.0"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.platform", "junit-platform-commons"),
            "1.10.0".into(),
        ),
        pom(vec![Dep::new(
            "org.apiguardian",
            "apiguardian-api",
            "1.1.2",
        )]),
    );
    r.insert(
        (coord("org.junit-pioneer", "junit-pioneer"), "1.9.1".into()),
        pom(vec![
            // pioneer originally pulls 5.9.0; depMgt in commons-lang
            // raises to 5.10.0. We pre-bake the managed version here
            // since the spike doesn't model depMgt.
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.0"),
            Dep::new("org.junit.jupiter", "junit-jupiter-params", "5.10.0"),
            Dep::new("org.junit.platform", "junit-platform-launcher", "1.10.0"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.platform", "junit-platform-launcher"),
            "1.10.0".into(),
        ),
        pom(vec![
            Dep::new("org.junit.platform", "junit-platform-engine", "1.10.0"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (coord("org.hamcrest", "hamcrest"), "2.2".into()),
        pom(vec![]),
    );
    r.insert(
        (coord("org.easymock", "easymock"), "5.2.0".into()),
        pom(vec![Dep::new("org.objenesis", "objenesis", "3.3")]),
    );
    r.insert(
        (coord("org.objenesis", "objenesis"), "3.3".into()),
        pom(vec![]),
    );
    r.insert(
        (coord("org.apache.commons", "commons-text"), "1.11.0".into()),
        pom(vec![Dep::new(
            "org.apache.commons",
            "commons-lang3",
            "3.13.0",
        )]),
    );
    r.insert(
        (coord("org.openjdk.jmh", "jmh-core"), "1.37".into()),
        pom(vec![
            Dep::new("net.sf.jopt-simple", "jopt-simple", "5.0.4"),
            Dep::new("org.apache.commons", "commons-math3", "3.6.1"),
        ]),
    );
    r.insert(
        (
            coord("org.openjdk.jmh", "jmh-generator-annprocess"),
            "1.37".into(),
        ),
        pom(vec![Dep::new("org.openjdk.jmh", "jmh-core", "1.37")]),
    );
    r.insert(
        (coord("net.sf.jopt-simple", "jopt-simple"), "5.0.4".into()),
        pom(vec![]),
    );
    r.insert(
        (coord("org.apache.commons", "commons-math3"), "3.6.1".into()),
        pom(vec![]),
    );
    r.insert(
        (coord("org.opentest4j", "opentest4j"), "1.3.0".into()),
        pom(vec![]),
    );
    r.insert(
        (coord("org.apiguardian", "apiguardian-api"), "1.1.2".into()),
        pom(vec![]),
    );
    r.insert(
        (coord("com.google.code.findbugs", "jsr305"), "3.0.2".into()),
        pom(vec![]),
    );
    r.insert(
        (
            coord("org.apache.commons", "commons-lang3"),
            "3.13.0".into(),
        ),
        pom(vec![]),
    );

    let expected: Expected = [
        ("org.junit.jupiter:junit-jupiter", "5.10.0"),
        ("org.junit.jupiter:junit-jupiter-api", "5.10.0"),
        ("org.junit.jupiter:junit-jupiter-params", "5.10.0"),
        ("org.junit.jupiter:junit-jupiter-engine", "5.10.0"),
        ("org.junit.platform:junit-platform-engine", "1.10.0"),
        ("org.junit.platform:junit-platform-commons", "1.10.0"),
        ("org.junit.platform:junit-platform-launcher", "1.10.0"),
        ("org.junit-pioneer:junit-pioneer", "1.9.1"),
        ("org.hamcrest:hamcrest", "2.2"),
        ("org.easymock:easymock", "5.2.0"),
        ("org.objenesis:objenesis", "3.3"),
        ("org.apache.commons:commons-text", "1.11.0"),
        ("org.openjdk.jmh:jmh-core", "1.37"),
        ("org.openjdk.jmh:jmh-generator-annprocess", "1.37"),
        ("net.sf.jopt-simple:jopt-simple", "5.0.4"),
        ("org.apache.commons:commons-math3", "3.6.1"),
        ("org.opentest4j:opentest4j", "1.3.0"),
        ("org.apiguardian:apiguardian-api", "1.1.2"),
        ("com.google.code.findbugs:jsr305", "3.0.2"),
    ]
    .into_iter()
    .map(|(k, v)| {
        let (g, a) = k.split_once(':').unwrap();
        (coord(g, a), v.into())
    })
    .collect();

    (root, r, expected)
}

// --- Fixture: commons-io 2.16.1 ----------------------------------------
//
// Source: `mvn -B dependency:tree -Dverbose` against
// test-corpus/commons-io/checkout.
//
// Interesting: byte-buddy 1.14.13 is declared directly (depth 1) while
// mockito-core 4.11.0 pulls byte-buddy 1.12.19 transitively (depth 3).
// mvn marks "omitted for conflict with 1.14.13" — nearest-wins. This
// is the lone real conflict in the 5-project corpus.

fn commons_io_graph() -> (Dep, Repo, Expected) {
    let root = Dep::new("commons-io", "commons-io", "2.16.1");
    let mut r: Repo = HashMap::new();

    r.insert(
        (coord("commons-io", "commons-io"), "2.16.1".into()),
        pom(vec![
            Dep::new("org.junit.jupiter", "junit-jupiter", "5.10.2"),
            Dep::new("org.junit-pioneer", "junit-pioneer", "1.9.1"),
            Dep::new("net.bytebuddy", "byte-buddy", "1.14.13"),
            Dep::new("net.bytebuddy", "byte-buddy-agent", "1.14.13"),
            Dep::new("org.mockito", "mockito-inline", "4.11.0"),
            Dep::new("com.google.jimfs", "jimfs", "1.3.0"),
            Dep::new("org.apache.commons", "commons-lang3", "3.14.0"),
            Dep::new("commons-codec", "commons-codec", "1.16.1"),
            Dep::new("org.openjdk.jmh", "jmh-core", "1.37"),
            Dep::new("org.openjdk.jmh", "jmh-generator-annprocess", "1.37"),
        ]),
    );
    r.insert(
        (coord("org.junit.jupiter", "junit-jupiter"), "5.10.2".into()),
        pom(vec![
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.2"),
            Dep::new("org.junit.jupiter", "junit-jupiter-params", "5.10.2"),
            Dep::new("org.junit.jupiter", "junit-jupiter-engine", "5.10.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.jupiter", "junit-jupiter-api"),
            "5.10.2".into(),
        ),
        pom(vec![
            Dep::new("org.opentest4j", "opentest4j", "1.3.0"),
            Dep::new("org.junit.platform", "junit-platform-commons", "1.10.2"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.jupiter", "junit-jupiter-params"),
            "5.10.2".into(),
        ),
        pom(vec![
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.2"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.jupiter", "junit-jupiter-engine"),
            "5.10.2".into(),
        ),
        pom(vec![
            Dep::new("org.junit.platform", "junit-platform-engine", "1.10.2"),
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.2"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.platform", "junit-platform-commons"),
            "1.10.2".into(),
        ),
        pom(vec![Dep::new(
            "org.apiguardian",
            "apiguardian-api",
            "1.1.2",
        )]),
    );
    r.insert(
        (
            coord("org.junit.platform", "junit-platform-engine"),
            "1.10.2".into(),
        ),
        pom(vec![
            Dep::new("org.opentest4j", "opentest4j", "1.3.0"),
            Dep::new("org.junit.platform", "junit-platform-commons", "1.10.2"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (coord("org.junit-pioneer", "junit-pioneer"), "1.9.1".into()),
        pom(vec![
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.2"),
            Dep::new("org.junit.jupiter", "junit-jupiter-params", "5.10.2"),
            Dep::new("org.junit.platform", "junit-platform-launcher", "1.10.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.platform", "junit-platform-launcher"),
            "1.10.2".into(),
        ),
        pom(vec![
            Dep::new("org.junit.platform", "junit-platform-engine", "1.10.2"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (coord("net.bytebuddy", "byte-buddy"), "1.14.13".into()),
        pom(vec![]),
    );
    r.insert(
        (coord("net.bytebuddy", "byte-buddy-agent"), "1.14.13".into()),
        pom(vec![]),
    );
    r.insert(
        (coord("org.mockito", "mockito-inline"), "4.11.0".into()),
        pom(vec![Dep::new("org.mockito", "mockito-core", "4.11.0")]),
    );
    r.insert(
        (coord("org.mockito", "mockito-core"), "4.11.0".into()),
        pom(vec![
            // mockito-core 4.11.0 declares byte-buddy 1.12.19, but
            // commons-io's direct dep at 1.14.13 wins (nearest-wins).
            Dep::new("net.bytebuddy", "byte-buddy", "1.12.19"),
            Dep::new("net.bytebuddy", "byte-buddy-agent", "1.12.19"),
            Dep::new("org.objenesis", "objenesis", "3.3"),
        ]),
    );
    r.insert(
        (coord("com.google.jimfs", "jimfs"), "1.3.0".into()),
        pom(vec![Dep::new("com.google.guava", "guava", "32.1.1-jre")]),
    );
    r.insert(
        (coord("com.google.guava", "guava"), "32.1.1-jre".into()),
        pom(vec![
            Dep::new("com.google.guava", "failureaccess", "1.0.1"),
            Dep::new(
                "com.google.guava",
                "listenablefuture",
                "9999.0-empty-to-avoid-conflict-with-guava",
            ),
            Dep::new("com.google.code.findbugs", "jsr305", "3.0.2"),
            Dep::new("org.checkerframework", "checker-qual", "3.33.0"),
            Dep::new("com.google.errorprone", "error_prone_annotations", "2.18.0"),
            Dep::new("com.google.j2objc", "j2objc-annotations", "2.8"),
        ]),
    );
    r.insert(
        (coord("org.openjdk.jmh", "jmh-core"), "1.37".into()),
        pom(vec![
            Dep::new("net.sf.jopt-simple", "jopt-simple", "5.0.4"),
            Dep::new("org.apache.commons", "commons-math3", "3.6.1"),
        ]),
    );
    r.insert(
        (
            coord("org.openjdk.jmh", "jmh-generator-annprocess"),
            "1.37".into(),
        ),
        pom(vec![Dep::new("org.openjdk.jmh", "jmh-core", "1.37")]),
    );

    // Leaves.
    for c in [
        ("org.apache.commons", "commons-lang3", "3.14.0"),
        ("commons-codec", "commons-codec", "1.16.1"),
        ("org.objenesis", "objenesis", "3.3"),
        ("com.google.guava", "failureaccess", "1.0.1"),
        (
            "com.google.guava",
            "listenablefuture",
            "9999.0-empty-to-avoid-conflict-with-guava",
        ),
        ("com.google.code.findbugs", "jsr305", "3.0.2"),
        ("org.checkerframework", "checker-qual", "3.33.0"),
        ("com.google.errorprone", "error_prone_annotations", "2.18.0"),
        ("com.google.j2objc", "j2objc-annotations", "2.8"),
        ("net.sf.jopt-simple", "jopt-simple", "5.0.4"),
        ("org.apache.commons", "commons-math3", "3.6.1"),
        ("org.opentest4j", "opentest4j", "1.3.0"),
        ("org.apiguardian", "apiguardian-api", "1.1.2"),
        ("net.bytebuddy", "byte-buddy", "1.12.19"),
        ("net.bytebuddy", "byte-buddy-agent", "1.12.19"),
    ] {
        r.insert((coord(c.0, c.1), c.2.into()), pom(vec![]));
    }

    let expected: Expected = [
        ("net.bytebuddy:byte-buddy", "1.14.13"),
        ("net.bytebuddy:byte-buddy-agent", "1.14.13"),
        ("org.mockito:mockito-inline", "4.11.0"),
        ("org.mockito:mockito-core", "4.11.0"),
        ("org.objenesis:objenesis", "3.3"),
        ("com.google.jimfs:jimfs", "1.3.0"),
        ("com.google.guava:guava", "32.1.1-jre"),
        ("org.junit.jupiter:junit-jupiter", "5.10.2"),
        ("org.junit.jupiter:junit-jupiter-api", "5.10.2"),
        ("org.junit.jupiter:junit-jupiter-params", "5.10.2"),
        ("org.junit.jupiter:junit-jupiter-engine", "5.10.2"),
        ("org.junit.platform:junit-platform-commons", "1.10.2"),
        ("org.junit.platform:junit-platform-engine", "1.10.2"),
        ("org.junit.platform:junit-platform-launcher", "1.10.2"),
        ("org.opentest4j:opentest4j", "1.3.0"),
        ("org.apiguardian:apiguardian-api", "1.1.2"),
        ("org.apache.commons:commons-lang3", "3.14.0"),
        ("commons-codec:commons-codec", "1.16.1"),
        ("org.openjdk.jmh:jmh-core", "1.37"),
        ("org.openjdk.jmh:jmh-generator-annprocess", "1.37"),
        ("net.sf.jopt-simple:jopt-simple", "5.0.4"),
        ("org.apache.commons:commons-math3", "3.6.1"),
        ("org.junit-pioneer:junit-pioneer", "1.9.1"),
        ("com.google.guava:failureaccess", "1.0.1"),
        (
            "com.google.guava:listenablefuture",
            "9999.0-empty-to-avoid-conflict-with-guava",
        ),
        ("com.google.code.findbugs:jsr305", "3.0.2"),
        ("org.checkerframework:checker-qual", "3.33.0"),
        ("com.google.errorprone:error_prone_annotations", "2.18.0"),
        ("com.google.j2objc:j2objc-annotations", "2.8"),
    ]
    .into_iter()
    .map(|(k, v)| {
        let (g, a) = k.split_once(':').unwrap();
        (coord(g, a), v.into())
    })
    .collect();

    (root, r, expected)
}

// --- Fixture: jackson-core 2.18.0 --------------------------------------
//
// Source: `mvn -B dependency:tree -Dverbose` against
// test-corpus/jackson-core/checkout.
//
// jackson-core is single-module. Interesting: it declares
// `junit-jupiter-api` BOTH directly (scope=test) AND transitively
// via `junit-jupiter`. mvn shows the direct one as resolved with
// a "scope not updated to test" note. Versions are aligned, so the
// spike's coord-only model gets the right answer.

fn jackson_core_graph() -> (Dep, Repo, Expected) {
    let root = Dep::new("com.fasterxml.jackson.core", "jackson-core", "2.18.0");
    let mut r: Repo = HashMap::new();

    r.insert(
        (
            coord("com.fasterxml.jackson.core", "jackson-core"),
            "2.18.0".into(),
        ),
        pom(vec![
            Dep::new("ch.randelshofer", "fastdoubleparser", "1.0.1"),
            Dep::new("org.junit.jupiter", "junit-jupiter", "5.10.2"),
            // Direct + at same coord as a transitive — the scope-narrowing
            // case. Same version (5.10.2), so no version conflict to model.
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.2"),
            Dep::new("org.assertj", "assertj-core", "3.24.2"),
        ]),
    );
    r.insert(
        (coord("ch.randelshofer", "fastdoubleparser"), "1.0.1".into()),
        pom(vec![]),
    );
    r.insert(
        (coord("org.junit.jupiter", "junit-jupiter"), "5.10.2".into()),
        pom(vec![
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.2"),
            Dep::new("org.junit.jupiter", "junit-jupiter-params", "5.10.2"),
            Dep::new("org.junit.jupiter", "junit-jupiter-engine", "5.10.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.jupiter", "junit-jupiter-api"),
            "5.10.2".into(),
        ),
        pom(vec![
            Dep::new("org.opentest4j", "opentest4j", "1.3.0"),
            Dep::new("org.junit.platform", "junit-platform-commons", "1.10.2"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.jupiter", "junit-jupiter-params"),
            "5.10.2".into(),
        ),
        pom(vec![
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.2"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.jupiter", "junit-jupiter-engine"),
            "5.10.2".into(),
        ),
        pom(vec![
            Dep::new("org.junit.platform", "junit-platform-engine", "1.10.2"),
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.2"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.platform", "junit-platform-commons"),
            "1.10.2".into(),
        ),
        pom(vec![Dep::new(
            "org.apiguardian",
            "apiguardian-api",
            "1.1.2",
        )]),
    );
    r.insert(
        (
            coord("org.junit.platform", "junit-platform-engine"),
            "1.10.2".into(),
        ),
        pom(vec![
            Dep::new("org.opentest4j", "opentest4j", "1.3.0"),
            Dep::new("org.junit.platform", "junit-platform-commons", "1.10.2"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (coord("org.assertj", "assertj-core"), "3.24.2".into()),
        pom(vec![Dep::new("net.bytebuddy", "byte-buddy", "1.12.21")]),
    );

    for c in [
        ("org.opentest4j", "opentest4j", "1.3.0"),
        ("org.apiguardian", "apiguardian-api", "1.1.2"),
        ("net.bytebuddy", "byte-buddy", "1.12.21"),
    ] {
        r.insert((coord(c.0, c.1), c.2.into()), pom(vec![]));
    }

    let expected: Expected = [
        ("ch.randelshofer:fastdoubleparser", "1.0.1"),
        ("org.junit.jupiter:junit-jupiter", "5.10.2"),
        ("org.junit.jupiter:junit-jupiter-api", "5.10.2"),
        ("org.junit.jupiter:junit-jupiter-params", "5.10.2"),
        ("org.junit.jupiter:junit-jupiter-engine", "5.10.2"),
        ("org.junit.platform:junit-platform-commons", "1.10.2"),
        ("org.junit.platform:junit-platform-engine", "1.10.2"),
        ("org.opentest4j:opentest4j", "1.3.0"),
        ("org.apiguardian:apiguardian-api", "1.1.2"),
        ("org.assertj:assertj-core", "3.24.2"),
        ("net.bytebuddy:byte-buddy", "1.12.21"),
    ]
    .into_iter()
    .map(|(k, v)| {
        let (g, a) = k.split_once(':').unwrap();
        (coord(g, a), v.into())
    })
    .collect();

    (root, r, expected)
}

// --- Fixture: assertj-core (assertj-performance-tests submodule) ------
//
// Source: `mvn -B dependency:tree -Dverbose` against the assertj-core
// reactor, picking the assertj-performance-tests submodule from the
// reactor (small, leaf-style module — representative of the project's
// non-Core modules). Single module's deps modeled here.

fn assertj_perf_graph() -> (Dep, Repo, Expected) {
    let root = Dep::new("org.assertj", "assertj-performance-tests", "3.26.3");
    let mut r: Repo = HashMap::new();

    r.insert(
        (
            coord("org.assertj", "assertj-performance-tests"),
            "3.26.3".into(),
        ),
        pom(vec![
            Dep::new("org.assertj", "assertj-core", "3.26.3"),
            Dep::new("org.junit.jupiter", "junit-jupiter", "5.10.3"),
        ]),
    );
    r.insert(
        (coord("org.assertj", "assertj-core"), "3.26.3".into()),
        pom(vec![Dep::new("net.bytebuddy", "byte-buddy", "1.14.18")]),
    );
    r.insert(
        (coord("org.junit.jupiter", "junit-jupiter"), "5.10.3".into()),
        pom(vec![
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.3"),
            Dep::new("org.junit.jupiter", "junit-jupiter-params", "5.10.3"),
            Dep::new("org.junit.jupiter", "junit-jupiter-engine", "5.10.3"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.jupiter", "junit-jupiter-api"),
            "5.10.3".into(),
        ),
        pom(vec![
            Dep::new("org.opentest4j", "opentest4j", "1.3.0"),
            Dep::new("org.junit.platform", "junit-platform-commons", "1.10.3"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.jupiter", "junit-jupiter-params"),
            "5.10.3".into(),
        ),
        pom(vec![
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.3"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.jupiter", "junit-jupiter-engine"),
            "5.10.3".into(),
        ),
        pom(vec![
            Dep::new("org.junit.platform", "junit-platform-engine", "1.10.3"),
            Dep::new("org.junit.jupiter", "junit-jupiter-api", "5.10.3"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );
    r.insert(
        (
            coord("org.junit.platform", "junit-platform-commons"),
            "1.10.3".into(),
        ),
        pom(vec![Dep::new(
            "org.apiguardian",
            "apiguardian-api",
            "1.1.2",
        )]),
    );
    r.insert(
        (
            coord("org.junit.platform", "junit-platform-engine"),
            "1.10.3".into(),
        ),
        pom(vec![
            Dep::new("org.opentest4j", "opentest4j", "1.3.0"),
            Dep::new("org.junit.platform", "junit-platform-commons", "1.10.3"),
            Dep::new("org.apiguardian", "apiguardian-api", "1.1.2"),
        ]),
    );

    for c in [
        ("net.bytebuddy", "byte-buddy", "1.14.18"),
        ("org.opentest4j", "opentest4j", "1.3.0"),
        ("org.apiguardian", "apiguardian-api", "1.1.2"),
    ] {
        r.insert((coord(c.0, c.1), c.2.into()), pom(vec![]));
    }

    let expected: Expected = [
        ("org.assertj:assertj-core", "3.26.3"),
        ("net.bytebuddy:byte-buddy", "1.14.18"),
        ("org.junit.jupiter:junit-jupiter", "5.10.3"),
        ("org.junit.jupiter:junit-jupiter-api", "5.10.3"),
        ("org.junit.jupiter:junit-jupiter-params", "5.10.3"),
        ("org.junit.jupiter:junit-jupiter-engine", "5.10.3"),
        ("org.junit.platform:junit-platform-commons", "1.10.3"),
        ("org.junit.platform:junit-platform-engine", "1.10.3"),
        ("org.opentest4j:opentest4j", "1.3.0"),
        ("org.apiguardian:apiguardian-api", "1.1.2"),
    ]
    .into_iter()
    .map(|(k, v)| {
        let (g, a) = k.split_once(':').unwrap();
        (coord(g, a), v.into())
    })
    .collect();

    (root, r, expected)
}

// --- Fixture: slf4j 2.0.16 (integration submodule) ---------------------
//
// Source: `mvn -B dependency:tree -Dverbose` against the slf4j reactor,
// picking the `integration` submodule — small, exercises a chain
// (felix.main -> felix.framework). Mostly trivial.

fn slf4j_integration_graph() -> (Dep, Repo, Expected) {
    let root = Dep::new("org.slf4j", "integration", "2.0.16");
    let mut r: Repo = HashMap::new();

    r.insert(
        (coord("org.slf4j", "integration"), "2.0.16".into()),
        pom(vec![
            Dep::new("org.slf4j", "slf4j-api", "2.0.16"),
            Dep::new("org.apache.felix", "org.apache.felix.main", "5.6.1"),
            Dep::new("junit", "junit", "4.10"),
        ]),
    );
    r.insert(
        (coord("org.slf4j", "slf4j-api"), "2.0.16".into()),
        pom(vec![]),
    );
    r.insert(
        (
            coord("org.apache.felix", "org.apache.felix.main"),
            "5.6.1".into(),
        ),
        pom(vec![Dep::new(
            "org.apache.felix",
            "org.apache.felix.framework",
            "5.6.1",
        )]),
    );
    r.insert(
        (
            coord("org.apache.felix", "org.apache.felix.framework"),
            "5.6.1".into(),
        ),
        pom(vec![]),
    );
    r.insert(
        (coord("junit", "junit"), "4.10".into()),
        pom(vec![Dep::new("org.hamcrest", "hamcrest-core", "1.1")]),
    );
    r.insert(
        (coord("org.hamcrest", "hamcrest-core"), "1.1".into()),
        pom(vec![]),
    );

    let expected: Expected = [
        ("org.slf4j:slf4j-api", "2.0.16"),
        ("org.apache.felix:org.apache.felix.main", "5.6.1"),
        ("org.apache.felix:org.apache.felix.framework", "5.6.1"),
        ("junit:junit", "4.10"),
        ("org.hamcrest:hamcrest-core", "1.1"),
    ]
    .into_iter()
    .map(|(k, v)| {
        let (g, a) = k.split_once(':').unwrap();
        (coord(g, a), v.into())
    })
    .collect();

    (root, r, expected)
}

// --- Main ---------------------------------------------------------------

fn main() {
    let mut total_pass = 0usize;
    let mut total_fail = 0usize;

    for (name, (root, repo, expected)) in [
        ("commons-lang 3.14.0", commons_lang_graph()),
        (
            "commons-io 2.16.1 (byte-buddy conflict)",
            commons_io_graph(),
        ),
        ("jackson-core 2.18.0", jackson_core_graph()),
        ("assertj-performance-tests 3.26.3", assertj_perf_graph()),
        ("slf4j integration 2.0.16", slf4j_integration_graph()),
    ] {
        let (p, f) = run_corpus_case(name, root, repo, expected);
        total_pass += p;
        total_fail += f;
        println!();
    }

    println!("=== TOTAL ===");
    println!(
        "  {total_pass} pass / {total_fail} fail of {} checks",
        total_pass + total_fail
    );
    if total_fail == 0 {
        println!("  CORPUS-WIDE PASS");
    } else {
        println!("  CORPUS-WIDE FAIL");
        std::process::exit(1);
    }
}
