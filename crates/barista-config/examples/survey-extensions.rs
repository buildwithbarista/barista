//! Regenerate `docs/compat/dot-mvn-extensions-survey.md` from the
//! materialized test corpus.
//!
//! Run from the repo root:
//!
//! ```text
//! bash scripts/materialize-corpus.sh
//! cargo run -p barista-config --example survey-extensions --release \
//!     > docs/compat/dot-mvn-extensions-survey.md
//! ```
//!
//! The example resolves corpus paths relative to the repo root,
//! which is the parent of `CARGO_MANIFEST_DIR/../..` (the
//! `barista-config` crate sits at `crates/barista-config/`).

use std::path::PathBuf;

use barista_config::survey_extensions;
use barista_test_fixtures::load_corpus_index;

fn main() {
    // crates/barista-config/Cargo.toml -> repo root is two levels up.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("manifest dir has two parents")
        .to_path_buf();

    let entries = load_corpus_index();
    let corpus_paths: Vec<(String, PathBuf)> = entries
        .iter()
        .map(|e| (e.id.clone(), repo_root.join(&e.relative_path)))
        .collect();

    let survey = survey_extensions(&corpus_paths);

    let today = current_date_yyyy_mm_dd();

    let mut out = String::new();
    out.push_str("# `.mvn/extensions.xml` corpus-impact survey\n\n");
    out.push_str(
        "Barista does not yet apply Maven build extensions \
         (`.mvn/extensions.xml`). Full extensions support is out of \
         scope for v0.1 — but visibility into how common extensions \
         are in real-world Maven projects keeps the v0.2 scoping \
         decision data-driven.\n\n",
    );
    out.push_str("This document is the running survey, regenerated as the test corpus grows.\n\n");

    out.push_str("## Method\n\n");
    out.push_str("1. Materialize the corpus: `bash scripts/materialize-corpus.sh`.\n");
    out.push_str(
        "2. For each project, check for `.mvn/extensions.xml`. If \
         present, parse the extensions list and tally per-extension \
         counts (keyed by `groupId:artifactId`).\n",
    );
    out.push_str(
        "3. Regenerate this document via \
         `cargo run -p barista-config --example survey-extensions \
         --release > docs/compat/dot-mvn-extensions-survey.md`.\n\n",
    );

    out.push_str("## Current findings (regenerated ");
    out.push_str(&today);
    out.push_str(")\n\n");
    out.push_str(&format!(
        "- **Total projects surveyed:** {}\n",
        survey.total_projects
    ));
    out.push_str(&format!(
        "- **Projects using `.mvn/extensions.xml`:** {} / {}\n",
        survey.projects_with_extensions.len(),
        survey.total_projects,
    ));

    if survey.projects_with_extensions.is_empty() {
        out.push_str("- **Extensions seen:** none.\n\n");
    } else {
        out.push_str("- **Projects with extensions:** ");
        out.push_str(&survey.projects_with_extensions.join(", "));
        out.push_str(".\n");
        out.push_str("- **Extensions seen:**\n\n");
        out.push_str("| Extension | Projects using it |\n");
        out.push_str("|---|---|\n");
        for (ext, count) in &survey.extension_counts {
            out.push_str(&format!("| `{ext}` | {count} |\n"));
        }
        out.push('\n');
    }

    out.push_str("## Interpretation\n\n");
    if survey.projects_with_extensions.is_empty() {
        out.push_str(
            "None of the currently materialized corpus projects ship a \
             `.mvn/extensions.xml`. The corpus is small (5 projects, \
             growing toward ~100) and skews toward Apache Commons / \
             FasterXML / SLF4J / AssertJ — libraries that prefer to \
             pin tooling in `pom.xml` rather than via build \
             extensions. As the corpus grows to include projects with \
             richer build environments (Spring, Quarkus, Hibernate, \
             gRPC-Java, large internal-style monorepos), this \
             baseline number will shift; the survey will surface that \
             change.\n\n",
        );
        out.push_str(
            "For v0.2 scoping: extensions are not blocking *this* \
             corpus, but two extension families warrant pre-emptive \
             planning because they appear at the moment a project \
             does adopt extensions:\n\n",
        );
        out.push_str(
            "- **`os-maven-plugin`** (`kr.motd.maven:os-maven-plugin`) \
             — sets `os.detected.*` properties used by protobuf and \
             other native-bridge plugins. Without it, dependent \
             plugins fail at execution time.\n",
        );
        out.push_str(
            "- **`maven-build-cache-extension`** \
             (`com.gradle:maven-build-cache-extension`, formerly \
             `org.apache.maven.extensions:maven-build-cache-extension`) \
             — overlaps with Barista's content-addressed cache and \
             may conflict if both are active.\n\n",
        );
    } else {
        out.push_str(
            "At least one corpus project ships a `.mvn/extensions.xml`. \
             Each entry in the table above is a candidate for v0.2 \
             extension support; entries with higher project counts or \
             that affect effective-POM output (and therefore the \
             resolver) take priority.\n\n",
        );
    }

    out.push_str("## Open questions\n\n");
    out.push_str(
        "- Should Barista support the `maven-build-cache-extension` \
         natively (its goals overlap with Barista's content-addressed \
         cache)?\n",
    );
    out.push_str(
        "- Which extensions change effective-POM output? Those are \
         the ones that affect the resolver and need top priority.\n",
    );
    out.push_str(
        "- For extensions that *don't* affect resolution (e.g. \
         reporting-only), is warn-and-skip an acceptable long-term \
         policy?\n",
    );

    print!("{out}");
}

/// Return today's date as `YYYY-MM-DD` using the system clock.
///
/// The example deliberately avoids pulling in `chrono`/`time` for a
/// one-line need.
fn current_date_yyyy_mm_dd() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's algorithm: days-since-epoch -> (year, month, day).
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i32 + (era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
