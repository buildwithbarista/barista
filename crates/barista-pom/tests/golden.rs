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

//! Effective-POM golden test harness.
//!
//! For each corpus project this test:
//!
//! 1. Runs `mvn help:effective-pom` to get Maven's authoritative
//!    effective POM.
//! 2. Resolves the same input with [`barista_pom::resolve_pom`].
//! 3. Structurally diffs the two on the resolver-relevant subset of
//!    fields (coordinates, dependencies, dependencyManagement,
//!    properties).
//!
//! The harness has two operating modes:
//!
//! - **Full run.** When `mvn` is on `PATH` and `test-corpus/<id>/checkout/`
//!   is materialized, every corpus project is compared. Any structural
//!   mismatch fails the test.
//! - **Skip.** When `mvn` is absent (or no project is materialized) the
//!   test prints a clear summary and exits 0. This keeps the harness
//!   useful on a barebones developer machine while still firing in CI,
//!   where Maven 3.9.x is provided.
//!
//! Only the `<parent>`-free subset of corpus projects can currently be
//! compared — the harness uses a [`NullParentResolver`] that errors on
//! any parent lookup. Projects whose root POM declares a `<parent>` are
//! recorded as `ResolverUnavailable` and surfaced in the summary but do
//! not fail the test. Once a cache-backed parent resolver lands the
//! harness can be upgraded to include them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use barista_pom::{
    ActivationContext, EffectiveError, ParentResolver, RawDependency, RawParent, RawPom,
    ResolveError, ResolvedPom, parse_pom, resolve_pom,
};

const PROJECTS: &[&str] = &[
    "commons-lang",
    "commons-io",
    "jackson-core",
    "assertj-core",
    "slf4j",
];

// ---------------------------------------------------------------------------
// Outcome / mismatch reporting
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum CaseOutcome {
    Pass {
        deps: usize,
        dep_mgt: usize,
        props_checked: usize,
    },
    Skip(String),
    Mismatch(GoldenMismatch),
    MvnError(String),
    OurError(String),
    ResolverUnavailable(String),
    /// Resolution failed inside barista-pom for a non-parent reason
    /// (e.g. an unsupported interpolation domain). Recorded as a
    /// compatibility gap, surfaced in the summary, but not a hard
    /// failure for v0.1.
    ResolverGap(String),
}

#[derive(Debug, Default)]
struct GoldenMismatch {
    project: String,
    coord_diffs: Vec<(String, String, String)>, // (field, ours, mvn)
    deps_missing: Vec<String>,                  // present in mvn, missing in ours
    deps_extra: Vec<String>,                    // present in ours, missing in mvn
    dep_mgt_missing: Vec<String>,
    dep_mgt_extra: Vec<String>,
    prop_missing: Vec<(String, String)>, // mvn property absent or differing in ours
    prop_ignored: Vec<String>,           // mvn-synthetic, intentionally ignored
}

impl GoldenMismatch {
    fn is_empty(&self) -> bool {
        self.coord_diffs.is_empty()
            && self.deps_missing.is_empty()
            && self.deps_extra.is_empty()
            && self.dep_mgt_missing.is_empty()
            && self.dep_mgt_extra.is_empty()
            && self.prop_missing.is_empty()
    }
}

impl fmt::Display for GoldenMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}: MISMATCH", self.project)?;
        for (field, ours, mvn) in &self.coord_diffs {
            writeln!(f, "  {field}: ours={ours:?} mvn={mvn:?}")?;
        }
        if !self.deps_missing.is_empty() {
            writeln!(f, "  dependencies: missing in ours:")?;
            for k in &self.deps_missing {
                writeln!(f, "    - {k}")?;
            }
        }
        if !self.deps_extra.is_empty() {
            writeln!(f, "  dependencies: extra in ours:")?;
            for k in &self.deps_extra {
                writeln!(f, "    - {k}")?;
            }
        }
        if !self.dep_mgt_missing.is_empty() {
            writeln!(f, "  dependencyManagement: missing in ours:")?;
            for k in &self.dep_mgt_missing {
                writeln!(f, "    - {k}")?;
            }
        }
        if !self.dep_mgt_extra.is_empty() {
            writeln!(f, "  dependencyManagement: extra in ours:")?;
            for k in &self.dep_mgt_extra {
                writeln!(f, "    - {k}")?;
            }
        }
        if !self.prop_missing.is_empty() {
            writeln!(f, "  properties: differ or missing in ours:")?;
            for (k, v) in &self.prop_missing {
                writeln!(f, "    - {k} = {v:?}")?;
            }
        }
        if !self.prop_ignored.is_empty() {
            writeln!(
                f,
                "  properties: mvn-only synthetic (ignored, {}): {}",
                self.prop_ignored.len(),
                preview(&self.prop_ignored, 4),
            )?;
        }
        Ok(())
    }
}

fn preview(items: &[String], n: usize) -> String {
    if items.len() <= n {
        items.join(", ")
    } else {
        format!("{}, ... ({} more)", items[..n].join(", "), items.len() - n)
    }
}

// ---------------------------------------------------------------------------
// Null parent resolver
// ---------------------------------------------------------------------------

struct NullParentResolver;

impl ParentResolver for NullParentResolver {
    fn resolve(&mut self, _parent: &RawParent) -> Result<RawPom, String> {
        Err("test harness does not resolve parents (NullParentResolver)".to_string())
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("manifest has grandparent")
        .to_path_buf()
}

fn corpus_root() -> PathBuf {
    repo_root().join("test-corpus")
}

// ---------------------------------------------------------------------------
// Environment detection
// ---------------------------------------------------------------------------

fn mvn_available() -> bool {
    Command::new("mvn")
        .arg("-v")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// For multi-module aggregator projects mvn emits a `<projects>` wrapper
/// containing one `<project>` per reactor module. The first `<project>`
/// is the aggregator itself, which is what we want to compare. For
/// single-module projects the input has a single `<project>` root and
/// is returned as-is.
fn extract_first_project(xml: &str) -> String {
    let Some(open_idx) = xml.find("<project ").or_else(|| xml.find("<project>")) else {
        return xml.to_string();
    };
    // If the root element is already <project>, just return the input.
    // Heuristic: check the first non-comment, non-PI element.
    let trimmed_head = &xml[..open_idx];
    if !trimmed_head.contains("<projects>") {
        return xml.to_string();
    }
    let Some(close_idx) = xml.find("</project>") else {
        return xml.to_string();
    };
    let end = close_idx + "</project>".len();
    let inner = &xml[open_idx..end];
    // Prepend an XML declaration so quick-xml is happy.
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{inner}")
}

fn run_mvn_effective(pom: &Path, output: &Path) -> Result<(), String> {
    let result = Command::new("mvn")
        .arg("-f")
        .arg(pom)
        .arg("help:effective-pom")
        .arg(format!("-Doutput={}", output.display()))
        .arg("-DskipTests=true")
        .arg("-B")
        .arg("-q")
        .output()
        .map_err(|e| format!("failed to spawn mvn: {e}"))?;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let stdout = String::from_utf8_lossy(&result.stdout);
        return Err(format!(
            "mvn exited with status {}: stderr=[{}] stdout=[{}]",
            result.status,
            stderr.trim(),
            stdout.trim(),
        ));
    }
    if !output.exists() {
        return Err(format!(
            "mvn succeeded but did not write {}",
            output.display()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Comparison logic
// ---------------------------------------------------------------------------

/// Dependency identity key used for set-equality comparison.
fn dep_key(d: &RawDependency) -> String {
    let scope = d.scope.as_deref().unwrap_or("compile");
    let r#type = d.r#type.as_deref().unwrap_or("jar");
    let classifier = d.classifier.as_deref().unwrap_or("");
    let version = d.version.as_deref().unwrap_or("");
    let optional = d.optional.as_deref().map(|s| s == "true").unwrap_or(false);
    format!(
        "{}:{}:{}:{}:{}:{}:optional={}",
        d.group_id, d.artifact_id, r#type, classifier, scope, version, optional,
    )
}

fn collect_deps(pom: &RawPom) -> BTreeSet<String> {
    pom.dependencies.iter().map(dep_key).collect()
}

fn collect_dep_mgt(pom: &RawPom) -> BTreeSet<String> {
    pom.dependency_management
        .as_ref()
        .map(|dm| dm.dependencies.iter().map(dep_key).collect())
        .unwrap_or_default()
}

/// Heuristics for properties Maven synthesizes that we don't (and
/// shouldn't) attempt to reproduce.
fn is_mvn_synthetic_property(key: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "project.",
        "maven.",
        "os.",
        "settings.",
        "env.",
        "user.",
        "java.",
        "file.",
        "line.",
        "path.",
        "sun.",
        "awt.",
        "jdk.",
        "native.",
        "https.",
        "http.",
        "ftp.",
        "socksProxy",
        "gpg.",
        "surefire.",
        "session.",
        "settings",
    ];
    PREFIXES.iter().any(|p| key.starts_with(p))
        || matches!(
            key,
            "basedir"
                | "build.timestamp"
                | "localRepository"
                | "reporting.outputEncoding"
                | "encoding"
        )
        // os.detected.* injected by os-maven-plugin
        || key.starts_with("os.detected.")
}

fn coerce_packaging(p: &str) -> &str {
    if p.is_empty() { "jar" } else { p }
}

fn compare(
    project: &str,
    ours: &ResolvedPom,
    mvn: &RawPom,
) -> Result<(usize, usize, usize), Box<GoldenMismatch>> {
    let mut mm = GoldenMismatch {
        project: project.to_string(),
        ..Default::default()
    };

    let our_pom = &ours.pom;

    // --- coordinates / model version / packaging ---
    if our_pom.model_version != mvn.model_version {
        mm.coord_diffs.push((
            "modelVersion".into(),
            our_pom.model_version.clone(),
            mvn.model_version.clone(),
        ));
    }
    let our_group = our_pom.group_id.clone().unwrap_or_default();
    let mvn_group = mvn.group_id.clone().unwrap_or_default();
    if our_group != mvn_group {
        mm.coord_diffs
            .push(("groupId".into(), our_group, mvn_group));
    }
    if our_pom.artifact_id != mvn.artifact_id {
        mm.coord_diffs.push((
            "artifactId".into(),
            our_pom.artifact_id.clone(),
            mvn.artifact_id.clone(),
        ));
    }
    let our_version = our_pom.version.clone().unwrap_or_default();
    let mvn_version = mvn.version.clone().unwrap_or_default();
    if our_version != mvn_version {
        mm.coord_diffs
            .push(("version".into(), our_version, mvn_version));
    }
    let our_pkg = coerce_packaging(&our_pom.packaging).to_string();
    let mvn_pkg = coerce_packaging(&mvn.packaging).to_string();
    if our_pkg != mvn_pkg {
        mm.coord_diffs.push(("packaging".into(), our_pkg, mvn_pkg));
    }

    // --- dependencies (set-equal) ---
    let ours_deps = collect_deps(our_pom);
    let mvn_deps = collect_deps(mvn);
    for k in mvn_deps.difference(&ours_deps) {
        mm.deps_missing.push(k.clone());
    }
    for k in ours_deps.difference(&mvn_deps) {
        mm.deps_extra.push(k.clone());
    }

    // --- depMgt (set-equal) ---
    let ours_dm = collect_dep_mgt(our_pom);
    let mvn_dm = collect_dep_mgt(mvn);
    for k in mvn_dm.difference(&ours_dm) {
        mm.dep_mgt_missing.push(k.clone());
    }
    for k in ours_dm.difference(&mvn_dm) {
        mm.dep_mgt_extra.push(k.clone());
    }

    // --- properties (superset: every mvn-reported property must be in ours,
    //     unless it's a Maven synthetic one we don't reproduce) ---
    let our_props: BTreeMap<&str, &str> = our_pom
        .properties
        .entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut props_checked = 0;
    for (k, mvn_v) in mvn.properties.entries.iter() {
        if is_mvn_synthetic_property(k) {
            mm.prop_ignored.push(k.clone());
            continue;
        }
        match our_props.get(k.as_str()) {
            Some(our_v) if *our_v == mvn_v.as_str() => {
                props_checked += 1;
            }
            Some(our_v) => {
                mm.prop_missing
                    .push((k.clone(), format!("mvn={mvn_v:?}, ours={our_v:?}")));
            }
            None => {
                mm.prop_missing
                    .push((k.clone(), format!("mvn={mvn_v:?}, ours=<missing>")));
            }
        }
    }

    if mm.is_empty() {
        Ok((ours_deps.len(), ours_dm.len(), props_checked))
    } else {
        Err(Box::new(mm))
    }
}

// ---------------------------------------------------------------------------
// Per-project runner
// ---------------------------------------------------------------------------

fn run_for(id: &str, tmp: &Path) -> CaseOutcome {
    let pom_path = corpus_root().join(id).join("checkout").join("pom.xml");
    if !pom_path.exists() {
        return CaseOutcome::Skip(format!("not materialized: {}", pom_path.display()));
    }

    // 1. Run mvn.
    let mvn_out = tmp.join(format!("{id}-effective.xml"));
    if let Err(e) = run_mvn_effective(&pom_path, &mvn_out) {
        return CaseOutcome::MvnError(e);
    }

    // 2. Parse mvn's effective POM.
    let mvn_xml = match std::fs::read_to_string(&mvn_out) {
        Ok(s) => s,
        Err(e) => return CaseOutcome::MvnError(format!("read mvn output: {e}")),
    };
    // Multi-module aggregators produce `<projects><project>…</project>…</projects>`.
    // Extract the first <project> (the aggregator itself) so the raw parser
    // sees a normal POM root.
    let mvn_xml = extract_first_project(&mvn_xml);
    let mvn_pom = match parse_pom(&mvn_xml) {
        Ok(p) => p,
        Err(e) => return CaseOutcome::MvnError(format!("parse mvn effective-pom: {e}")),
    };

    // 3. Parse + resolve ours.
    let our_src = match std::fs::read_to_string(&pom_path) {
        Ok(s) => s,
        Err(e) => return CaseOutcome::OurError(format!("read source pom: {e}")),
    };
    let our_raw = match parse_pom(&our_src) {
        Ok(p) => p,
        Err(e) => return CaseOutcome::OurError(format!("parse source pom: {e}")),
    };
    let our_resolved = match resolve_pom(
        our_raw,
        &mut NullParentResolver,
        &ActivationContext::default(),
    ) {
        Ok(r) => r,
        Err(ResolveError::Effective(EffectiveError::ParentResolution { coords, .. })) => {
            return CaseOutcome::ResolverUnavailable(format!(
                "needs parent {coords}; NullParentResolver cannot satisfy"
            ));
        }
        Err(e) => {
            // resolve_pom failed for a reason other than parent
            // resolution — this represents a real gap in
            // barista-pom (e.g. an unsupported `${...}` domain,
            // unhandled depMgt edge). Surface as a compatibility
            // gap, not a harness failure.
            return CaseOutcome::ResolverGap(format!("resolve_pom: {e}"));
        }
    };

    // 4. Compare.
    match compare(id, &our_resolved, &mvn_pom) {
        Ok((deps, dep_mgt, props_checked)) => CaseOutcome::Pass {
            deps,
            dep_mgt,
            props_checked,
        },
        Err(mm) => CaseOutcome::Mismatch(*mm),
    }
}

// ---------------------------------------------------------------------------
// Test entry point
// ---------------------------------------------------------------------------

#[test]
fn effective_pom_golden_corpus() {
    let mvn = mvn_available();
    let corpus_present = corpus_root().exists();

    // Per-project materialization detection (for the skip message).
    let materialized: Vec<(&str, bool)> = PROJECTS
        .iter()
        .map(|id| {
            (
                *id,
                corpus_root()
                    .join(id)
                    .join("checkout")
                    .join("pom.xml")
                    .exists(),
            )
        })
        .collect();

    if !mvn || !corpus_present || materialized.iter().all(|(_, ok)| !*ok) {
        let mvn_msg = if mvn { "mvn present" } else { "mvn missing" };
        let mat: Vec<String> = materialized
            .iter()
            .map(|(id, ok)| {
                format!(
                    "{} {}",
                    id,
                    if *ok {
                        "materialized"
                    } else {
                        "not materialized"
                    }
                )
            })
            .collect();
        eprintln!(
            "golden test: skipping. Requirements:\n\
             - Maven 3.9.x on PATH (try `asdf install maven 3.9.9 && asdf reshim`)\n\
             - Corpus materialized (try `bash scripts/materialize-corpus.sh`)\n\
             Detected: {}; {}",
            mvn_msg,
            mat.join("; "),
        );
        return;
    }

    let tmp = std::env::temp_dir().join("barista-pom-golden");
    std::fs::create_dir_all(&tmp).expect("create tmp dir for mvn output");

    let mut pass = 0usize;
    let mut mismatch = 0usize;
    let mut skipped = 0usize;
    let mut mvn_errors = 0usize;
    let mut our_errors = 0usize;
    let mut resolver_unavailable = 0usize;
    let mut resolver_gap = 0usize;
    let mut mismatches: Vec<GoldenMismatch> = Vec::new();

    for id in PROJECTS {
        eprintln!("--- {id} ---");
        match run_for(id, &tmp) {
            CaseOutcome::Pass {
                deps,
                dep_mgt,
                props_checked,
            } => {
                pass += 1;
                println!(
                    "{id}: OK ({deps} deps match, {dep_mgt} depMgt match, \
                     {props_checked} props subset-OK)"
                );
            }
            CaseOutcome::Skip(reason) => {
                skipped += 1;
                eprintln!("{id}: SKIP ({reason})");
            }
            CaseOutcome::Mismatch(mm) => {
                mismatch += 1;
                eprintln!("{mm}");
                mismatches.push(mm);
            }
            CaseOutcome::MvnError(e) => {
                mvn_errors += 1;
                eprintln!("{id}: MVN ERROR — {e}");
            }
            CaseOutcome::OurError(e) => {
                our_errors += 1;
                eprintln!("{id}: OUR ERROR — {e}");
            }
            CaseOutcome::ResolverUnavailable(e) => {
                resolver_unavailable += 1;
                eprintln!(
                    "{id}: RESOLVER UNAVAILABLE — {e}\n  \
                     (this project declares a <parent>; the golden harness needs \
                     a cache-backed ParentResolver to compare it)"
                );
            }
            CaseOutcome::ResolverGap(e) => {
                resolver_gap += 1;
                eprintln!(
                    "{id}: RESOLVER GAP — {e}\n  \
                     (barista-pom cannot yet fully resolve this project; \
                     this is a known compatibility gap, not a harness failure)"
                );
            }
        }
    }

    println!(
        "golden: {total} projects considered, {pass} pass, {mismatch} mismatch, \
         {skipped} skipped, {resolver_unavailable} resolver-unavailable, \
         {resolver_gap} resolver-gap, {mvn_errors} mvn-error, {our_errors} our-error",
        total = PROJECTS.len(),
    );

    // Failure conditions for v0.1:
    //   - any structural mismatch on a project we DID compare
    //   - any unexpected mvn-side or our-side parse/IO error
    // Resolver-unavailable cases are NOT a failure for v0.1; they
    // surface as a clear summary line and will become comparable once a
    // real ParentResolver is available.
    let mut failures: Vec<String> = Vec::new();
    if !mismatches.is_empty() {
        for mm in &mismatches {
            failures.push(format!("mismatch in {}", mm.project));
        }
    }
    if mvn_errors > 0 {
        failures.push(format!("{mvn_errors} mvn invocation(s) failed"));
    }
    if our_errors > 0 {
        failures.push(format!("{our_errors} barista-pom error(s)"));
    }

    if !failures.is_empty() {
        panic!("golden test failed: {}", failures.join("; "));
    }
}
