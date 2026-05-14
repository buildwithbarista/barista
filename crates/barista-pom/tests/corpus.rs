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

//! Integration test: parse every pom.xml in the materialized test
//! corpus. Skipped (with a clear `eprintln!`) when the corpus has not
//! been materialized via `scripts/materialize-corpus.sh`.

use std::path::{Path, PathBuf};

use barista_pom::parse_pom;

const PROJECTS: &[&str] = &[
    "commons-lang",
    "commons-io",
    "jackson-core",
    "assertj-core",
    "slf4j",
];

fn corpus_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at crates/barista-pom; the corpus
    // lives at <repo-root>/test-corpus, i.e. two levels up.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("manifest has grandparent")
        .join("test-corpus")
}

#[test]
fn parses_all_corpus_poms() {
    let root = corpus_root();
    if !root.exists() {
        eprintln!(
            "test-corpus/ not found at {} -- skipping; run scripts/materialize-corpus.sh",
            root.display()
        );
        return;
    }

    let mut missing = Vec::new();
    let mut results = Vec::new();
    for id in PROJECTS {
        let pom_path = root.join(id).join("checkout").join("pom.xml");
        if !pom_path.exists() {
            missing.push(*id);
            continue;
        }
        let content = std::fs::read_to_string(&pom_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", pom_path.display()));
        match parse_pom(&content) {
            Ok(pom) => {
                let dep_count = pom.dependencies.len()
                    + pom
                        .dependency_management
                        .as_ref()
                        .map(|dm| dm.dependencies.len())
                        .unwrap_or(0);
                let prop_count = pom.properties.entries.len();
                assert_eq!(
                    pom.model_version, "4.0.0",
                    "{id}: expected modelVersion 4.0.0"
                );
                println!("{id}: OK ({dep_count} deps, {prop_count} props)");
                results.push((id, dep_count, prop_count));
            }
            Err(e) => panic!("{id}: parse failed: {e}"),
        }
    }

    if !missing.is_empty() {
        eprintln!(
            "corpus not fully materialized; missing: {missing:?} -- run scripts/materialize-corpus.sh"
        );
        if results.is_empty() {
            return;
        }
    }

    assert!(!results.is_empty(), "no corpus projects parsed");
}
