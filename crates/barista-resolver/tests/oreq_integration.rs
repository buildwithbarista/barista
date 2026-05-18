// Integration-test target — workspace security lints relaxed here so
// failing assertions panic loudly (the documented contract for tests).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Integration tests for the O-REQ-01..05 optimizations (PRD §18.3).
//!
//! Each test constructs a fixture topology that *would* trigger the
//! corresponding redundant fetch in the un-optimized resolver, drives
//! the walker with an [`OreqSession`] attached, and asserts the
//! relevant counter advanced by the expected amount. The
//! `all_five_counters_advance_on_diamond_with_shared_parent` test at
//! the bottom is the milestone-level integration check — it exercises
//! all five optimizations in a single walk.
//!
//! The optimizations under test (mapping to PRD §18.3):
//!
//! * O-REQ-01 — `maven-metadata.xml` dedup per (repo, GA)
//! * O-REQ-02 — `Disk` / `InMemory` origin counted as a 304-equivalent
//! * O-REQ-03 — frozen-lockfile pins short-circuit metadata fetches
//! * O-REQ-04 — sibling-shared parent / transitive POMs deduped
//! * O-REQ-05 — effective POMs deduped (same coord+version, two paths)

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use barista_coords::Coords;
use barista_pom::{EffectivePom, Properties, RawDependency, RawPom, ResolvedPom, raw::RawParent};
use barista_resolver::oreq::OreqSession;
use barista_resolver::source::{
    FetchOrigin, GaMetadata, MetadataError, MetadataSource, ResolveKey,
};
use barista_resolver::walker::{WalkOptions, walk};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn co(g: &str, a: &str) -> Coords {
    Coords::new(g, a).expect("valid coords")
}

fn dep(g: &str, a: &str, v: &str) -> RawDependency {
    RawDependency {
        group_id: g.into(),
        artifact_id: a.into(),
        version: Some(v.into()),
        ..RawDependency::default()
    }
}

fn pom(g: &str, a: &str, v: &str, deps: Vec<RawDependency>) -> RawPom {
    RawPom {
        model_version: "4.0.0".into(),
        group_id: Some(g.into()),
        artifact_id: a.into(),
        version: Some(v.into()),
        packaging: "jar".into(),
        dependencies: deps,
        properties: Properties::default(),
        ..RawPom::default()
    }
}

fn resolved(p: RawPom) -> ResolvedPom {
    ResolvedPom {
        effective: EffectivePom {
            pom: p.clone(),
            interpolations: Vec::new(),
            parent_chain: Vec::new(),
        },
        pom: p,
        active_profile_ids: Vec::new(),
        imported_boms: Vec::new(),
    }
}

/// A [`MetadataSource`] that records how many times each
/// `fetch_pom` / `fetch_metadata` was called. The counters are
/// shared across clones so the test can read them after the walk
/// finishes.
#[derive(Debug, Clone)]
struct CountingSource {
    poms: HashMap<(Coords, String), RawPom>,
    metadata: HashMap<Coords, Vec<String>>,
    pom_calls: Arc<AtomicU64>,
    metadata_calls: Arc<AtomicU64>,
    /// When set, `fetch_metadata` reports this origin so the
    /// O-REQ-02 counter can be exercised independently of the
    /// resolver's actual cache layer.
    metadata_origin: FetchOrigin,
}

impl CountingSource {
    fn new() -> Self {
        Self {
            poms: HashMap::new(),
            metadata: HashMap::new(),
            pom_calls: Arc::new(AtomicU64::new(0)),
            metadata_calls: Arc::new(AtomicU64::new(0)),
            metadata_origin: FetchOrigin::Remote,
        }
    }

    fn with_metadata_origin(mut self, origin: FetchOrigin) -> Self {
        self.metadata_origin = origin;
        self
    }

    fn add_pom(&mut self, coords: Coords, version: impl Into<String>, p: RawPom) {
        let v = version.into();
        self.metadata
            .entry(coords.clone())
            .or_default()
            .push(v.clone());
        self.poms.insert((coords, v), p);
    }

    fn pom_call_count(&self) -> u64 {
        self.pom_calls.load(Ordering::Relaxed)
    }

    fn metadata_call_count(&self) -> u64 {
        self.metadata_calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl MetadataSource for CountingSource {
    async fn fetch_pom(
        &self,
        coords: &ResolveKey,
        version: &str,
    ) -> Result<(RawPom, FetchOrigin), MetadataError> {
        self.pom_calls.fetch_add(1, Ordering::Relaxed);
        match self.poms.get(&(coords.clone(), version.to_string())) {
            Some(p) => Ok((p.clone(), FetchOrigin::Fixture)),
            None => Err(MetadataError::NotFound {
                coords: coords.to_string(),
                version: version.to_string(),
            }),
        }
    }

    async fn fetch_metadata(
        &self,
        coords: &ResolveKey,
    ) -> Result<(GaMetadata, FetchOrigin), MetadataError> {
        self.metadata_calls.fetch_add(1, Ordering::Relaxed);
        match self.metadata.get(coords) {
            Some(v) => Ok((
                GaMetadata {
                    coords: coords.clone(),
                    versions: v.clone(),
                    latest_snapshot_timestamp: None,
                    last_updated: None,
                },
                self.metadata_origin,
            )),
            None => Err(MetadataError::MetadataNotFound {
                coords: coords.to_string(),
            }),
        }
    }
}

/// Build a [`WalkOptions`] with the supplied [`OreqSession`] attached.
fn opts_with_session(session: Arc<OreqSession>) -> WalkOptions {
    WalkOptions {
        oreq: Some(session),
        ..WalkOptions::default()
    }
}

// ---------------------------------------------------------------------------
// O-REQ-01 — in-session maven-metadata.xml dedup
// EFF-LINK: docs/efficiency/findings/EFF-2026-001.md
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oreq_01_metadata_dedup_via_walker() {
    // Two direct deps declared with `RELEASE` against the same GA →
    // each path hits fetch_metadata. Without dedup, that's 2 metadata
    // fetches; with dedup, it's 1, and O-REQ-01 fires once.
    let mut src = CountingSource::new();
    src.add_pom(co("ex", "lib"), "1.0", pom("ex", "lib", "1.0", vec![]));
    src.add_pom(co("ex", "lib"), "2.0", pom("ex", "lib", "2.0", vec![]));
    src.add_pom(
        co("ex", "wrapper"),
        "1.0",
        pom(
            "ex",
            "wrapper",
            "1.0",
            vec![{
                let mut d = dep("ex", "lib", "RELEASE");
                d.version = Some("RELEASE".into());
                d
            }],
        ),
    );

    let mut root_dep = dep("ex", "lib", "RELEASE");
    root_dep.version = Some("RELEASE".into());
    let root = pom(
        "ex",
        "root",
        "1.0",
        vec![root_dep, dep("ex", "wrapper", "1.0")],
    );

    let session = Arc::new(OreqSession::new());
    // Disable the skipper so the second `ex:lib` visit actually
    // reaches the metadata-resolution site. With the skipper enabled
    // the second visit is short-circuited via nearest-wins before
    // `resolve_spec` runs, which is correct but masks the O-REQ-01
    // signal in this fixture.
    let opts = WalkOptions {
        oreq: Some(Arc::clone(&session)),
        enable_skipper: false,
        ..WalkOptions::default()
    };
    let g = walk(&resolved(root), &src, &opts).await.expect("walk ok");

    // Single underlying fetch_metadata call for ex:lib despite two
    // RELEASE resolutions that both need it.
    assert_eq!(src.metadata_call_count(), 1);
    assert_eq!(g.oreq_stats.oreq_01_avoided, 1);
}

// ---------------------------------------------------------------------------
// O-REQ-02 — conditional-revalidation (304-equivalent) counter
// EFF-LINK: docs/efficiency/findings/EFF-2026-004.md
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oreq_02_conditional_revalidation_counted_when_source_serves_from_cache() {
    // Source reports Disk origin on metadata fetches → O-REQ-02 fires
    // once per first-fetch (and not on subsequent in-session dedup
    // hits, which are O-REQ-01 territory).
    let mut src = CountingSource::new().with_metadata_origin(FetchOrigin::Disk);
    src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));
    src.add_pom(co("ex", "B"), "1.0", pom("ex", "B", "1.0", vec![]));

    let mut da = dep("ex", "A", "LATEST");
    da.version = Some("LATEST".into());
    let mut db = dep("ex", "B", "LATEST");
    db.version = Some("LATEST".into());
    let root = pom("ex", "root", "1.0", vec![da, db]);

    let session = Arc::new(OreqSession::new());
    let opts = opts_with_session(Arc::clone(&session));
    let g = walk(&resolved(root), &src, &opts).await.expect("walk ok");

    // Two distinct GAs → two distinct first-fetches → O-REQ-02 = 2.
    assert_eq!(g.oreq_stats.oreq_02_avoided, 2);
    // No dedup (different GAs).
    assert_eq!(g.oreq_stats.oreq_01_avoided, 0);
}

#[tokio::test]
async fn oreq_02_does_not_fire_when_source_reports_remote_origin() {
    let mut src = CountingSource::new(); // default Remote
    src.add_pom(co("ex", "A"), "1.0", pom("ex", "A", "1.0", vec![]));
    let mut d = dep("ex", "A", "LATEST");
    d.version = Some("LATEST".into());
    let root = pom("ex", "root", "1.0", vec![d]);

    let session = Arc::new(OreqSession::new());
    let opts = opts_with_session(Arc::clone(&session));
    let g = walk(&resolved(root), &src, &opts).await.expect("walk ok");
    assert_eq!(g.oreq_stats.oreq_02_avoided, 0);
}

// ---------------------------------------------------------------------------
// O-REQ-03 — frozen-lockfile mode skips metadata fetches
// EFF-LINK: docs/efficiency/findings/EFF-2026-005.md
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oreq_03_frozen_pin_skips_metadata_for_latest() {
    let mut src = CountingSource::new();
    src.add_pom(co("ex", "A"), "1.5", pom("ex", "A", "1.5", vec![]));
    // Note: we DO add versions to the metadata table so that without
    // the frozen pin, `LATEST` would fetch them. The assertion below
    // proves the pin short-circuited the fetch.
    let mut d = dep("ex", "A", "LATEST");
    d.version = Some("LATEST".into());
    let root = pom("ex", "root", "1.0", vec![d]);

    let mut pins = HashMap::new();
    pins.insert(co("ex", "A"), "1.5".to_string());
    let session = Arc::new(OreqSession::new().with_frozen_pins(pins));
    let opts = opts_with_session(Arc::clone(&session));
    let g = walk(&resolved(root), &src, &opts).await.expect("walk ok");

    assert_eq!(g.oreq_stats.oreq_03_avoided, 1);
    // The frozen path short-circuited so fetch_metadata was NOT called.
    assert_eq!(src.metadata_call_count(), 0);
    // And the picked version is the pin.
    assert_eq!(
        g.winners.get(&co("ex", "A")).unwrap().version,
        "1.5".to_string()
    );
}

#[tokio::test]
async fn oreq_03_frozen_pin_short_circuits_soft_version() {
    // Soft-version path also consults frozen pins so a lockfile-pinned
    // resolve never even reads the inline `<version>` text.
    let mut src = CountingSource::new();
    src.add_pom(co("ex", "A"), "2.0", pom("ex", "A", "2.0", vec![]));
    let root = pom("ex", "root", "1.0", vec![dep("ex", "A", "1.0")]);
    let mut pins = HashMap::new();
    pins.insert(co("ex", "A"), "2.0".to_string());
    let session = Arc::new(OreqSession::new().with_frozen_pins(pins));
    let opts = opts_with_session(Arc::clone(&session));
    let g = walk(&resolved(root), &src, &opts).await.expect("walk ok");
    assert_eq!(g.oreq_stats.oreq_03_avoided, 1);
    assert_eq!(g.winners.get(&co("ex", "A")).unwrap().version, "2.0");
}

// ---------------------------------------------------------------------------
// O-REQ-04 — parent / sibling POM dedup (cross-walk)
// EFF-LINK: docs/efficiency/findings/EFF-2026-006.md
//
// Note on test surface: in the current single-pass BFS walker
// (M2.1 T2), the `fetch_pom`-dedup site only fires when the same
// (coord, version) is fetched across two `walk` calls that share a
// session — within a single walk, nearest-wins terminates duplicate
// (coord, version) visits *before* they reach `fetch_pom`. The
// dedup primitive is correct in both directions; the in-walk fire
// surface activates once T7 / M2.3 wires the parent-POM resolver
// through the same session-aware path. The cross-walk test below is
// the addressable T1 surface.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oreq_04_parent_pom_deduped_across_walks_sharing_session() {
    // Reactor-style scenario: two top-level modules each pull the
    // same parent POM (`ex:platform-bom:1.0`) as a dependency. Two
    // sequential `walk` calls share one `OreqSession` (representing
    // the single CLI invocation). Second walk's fetch_pom hits the
    // session cache.
    let mut src = CountingSource::new();
    src.add_pom(
        co("ex", "platform-bom"),
        "1.0",
        pom("ex", "platform-bom", "1.0", vec![]),
    );
    src.add_pom(
        co("ex", "starter-web"),
        "1.0",
        pom(
            "ex",
            "starter-web",
            "1.0",
            vec![dep("ex", "platform-bom", "1.0")],
        ),
    );
    src.add_pom(
        co("ex", "starter-jdbc"),
        "1.0",
        pom(
            "ex",
            "starter-jdbc",
            "1.0",
            vec![dep("ex", "platform-bom", "1.0")],
        ),
    );

    let session = Arc::new(OreqSession::new());
    // Walk 1: module-A depends on starter-web (which pulls platform-bom).
    let root_a = pom(
        "ex",
        "module-a",
        "1.0",
        vec![dep("ex", "starter-web", "1.0")],
    );
    let opts = opts_with_session(Arc::clone(&session));
    let _g1 = walk(&resolved(root_a), &src, &opts)
        .await
        .expect("walk-a ok");
    // After walk-1: counter is 0 (no dedup yet — each coord was new).
    assert_eq!(session.stats().oreq_04_avoided, 0);
    let pom_calls_after_walk1 = src.pom_call_count();
    assert!(
        pom_calls_after_walk1 >= 2,
        "walk-1 should fetch at least starter-web + platform-bom"
    );

    // Walk 2: module-B depends on starter-jdbc (which also pulls platform-bom).
    let root_b = pom(
        "ex",
        "module-b",
        "1.0",
        vec![dep("ex", "starter-jdbc", "1.0")],
    );
    let g2 = walk(&resolved(root_b), &src, &opts)
        .await
        .expect("walk-b ok");
    // platform-bom is served from session cache → O-REQ-04 fires.
    assert!(
        g2.oreq_stats.oreq_04_avoided >= 1,
        "O-REQ-04 must fire on cross-walk shared POM: {:?}",
        g2.oreq_stats
    );
    // And the underlying source's fetch_pom was not called for
    // platform-bom in walk-2 (the second walk only fetched
    // starter-jdbc fresh).
    let pom_calls_walk2 = src.pom_call_count() - pom_calls_after_walk1;
    assert!(
        pom_calls_walk2 < 2,
        "walk-2 should reuse platform-bom from session, fetched {pom_calls_walk2}"
    );
}

// ---------------------------------------------------------------------------
// O-REQ-05 — effective-POM dedup (cross-walk)
// EFF-LINK: docs/efficiency/findings/EFF-2026-007.md
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oreq_05_effective_pom_deduped_across_walks_sharing_session() {
    // Identical setup to O-REQ-04 cross-walk test. The effective-POM
    // cache fires on the second walk's transitive resolution.
    let mut src = CountingSource::new();
    src.add_pom(
        co("ex", "shared-lib"),
        "1.0",
        pom("ex", "shared-lib", "1.0", vec![]),
    );
    src.add_pom(
        co("ex", "module-a"),
        "1.0",
        pom(
            "ex",
            "module-a",
            "1.0",
            vec![dep("ex", "shared-lib", "1.0")],
        ),
    );
    src.add_pom(
        co("ex", "module-b"),
        "1.0",
        pom(
            "ex",
            "module-b",
            "1.0",
            vec![dep("ex", "shared-lib", "1.0")],
        ),
    );
    let session = Arc::new(OreqSession::new());
    let opts = opts_with_session(Arc::clone(&session));
    let _ = walk(
        &resolved(pom(
            "ex",
            "root-1",
            "1.0",
            vec![dep("ex", "module-a", "1.0")],
        )),
        &src,
        &opts,
    )
    .await
    .expect("walk-1 ok");
    assert_eq!(session.stats().oreq_05_avoided, 0);
    let g2 = walk(
        &resolved(pom(
            "ex",
            "root-2",
            "1.0",
            vec![dep("ex", "module-b", "1.0")],
        )),
        &src,
        &opts,
    )
    .await
    .expect("walk-2 ok");
    assert!(
        g2.oreq_stats.oreq_05_avoided >= 1,
        "O-REQ-05 must fire across walks: {:?}",
        g2.oreq_stats
    );
}

// ---------------------------------------------------------------------------
// Milestone-level: all five counters advance on a single walk.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_five_counters_advance_across_two_walks_sharing_session() {
    // Two sequential `walk` calls share one session. Each walk
    // resolves a small module; the union of the two walks exercises
    // every optimization:
    //
    //   * O-REQ-01 — both walks contain `RELEASE` deps on `ex:lib`,
    //     so the second walk's metadata-fetch hits the session cache.
    //   * O-REQ-02 — `Disk` metadata origin reports cache-hits as
    //     conditional-revalidations (304-equivalent).
    //   * O-REQ-03 — `ex:pinned` is frozen-pinned in the session, so
    //     its `LATEST` declaration short-circuits.
    //   * O-REQ-04 — both modules share `ex:shared`. Walk-1 fetches
    //     it; walk-2 reads from the session cache.
    //   * O-REQ-05 — same as O-REQ-04 but at the effective-POM layer.
    let mut src = CountingSource::new().with_metadata_origin(FetchOrigin::Disk);
    src.add_pom(co("ex", "lib"), "1.0", pom("ex", "lib", "1.0", vec![]));
    src.add_pom(co("ex", "lib"), "2.0", pom("ex", "lib", "2.0", vec![]));
    src.add_pom(
        co("ex", "pinned"),
        "9.9",
        pom("ex", "pinned", "9.9", vec![]),
    );
    src.add_pom(
        co("ex", "shared"),
        "1.0",
        pom("ex", "shared", "1.0", vec![]),
    );
    src.add_pom(
        co("ex", "module-a"),
        "1.0",
        pom("ex", "module-a", "1.0", vec![dep("ex", "shared", "1.0")]),
    );
    src.add_pom(
        co("ex", "module-b"),
        "1.0",
        pom("ex", "module-b", "1.0", vec![dep("ex", "shared", "1.0")]),
    );

    let mut pins = HashMap::new();
    pins.insert(co("ex", "pinned"), "9.9".to_string());
    let session = Arc::new(OreqSession::new().with_frozen_pins(pins));
    let opts = WalkOptions {
        oreq: Some(Arc::clone(&session)),
        enable_skipper: false,
        ..WalkOptions::default()
    };

    // Walk 1 — uses ex:lib via RELEASE (warms metadata cache),
    // ex:pinned via LATEST (frozen short-circuit), and ex:module-a
    // (pulls ex:shared).
    let mut lib_rel = dep("ex", "lib", "RELEASE");
    lib_rel.version = Some("RELEASE".into());
    let mut pinned_latest = dep("ex", "pinned", "LATEST");
    pinned_latest.version = Some("LATEST".into());
    let root_1 = pom(
        "ex",
        "root-1",
        "1.0",
        vec![lib_rel, pinned_latest, dep("ex", "module-a", "1.0")],
    );
    let _g1 = walk(&resolved(root_1), &src, &opts)
        .await
        .expect("walk-1 ok");

    // After walk-1 we expect O-REQ-02 (Disk metadata) and O-REQ-03
    // (one frozen-skip) to have fired, but not the dedup-shaped ones.
    assert!(session.stats().oreq_02_avoided >= 1, "walk-1 O-REQ-02");
    assert_eq!(session.stats().oreq_03_avoided, 1, "walk-1 O-REQ-03");

    // Walk 2 — references ex:lib again via RELEASE (now session-hit),
    // and ex:module-b (pulls ex:shared, also session-hit).
    let mut lib_rel_2 = dep("ex", "lib", "RELEASE");
    lib_rel_2.version = Some("RELEASE".into());
    let root_2 = pom(
        "ex",
        "root-2",
        "1.0",
        vec![lib_rel_2, dep("ex", "module-b", "1.0")],
    );
    let g2 = walk(&resolved(root_2), &src, &opts)
        .await
        .expect("walk-2 ok");

    let stats = session.stats();
    assert!(stats.oreq_01_avoided >= 1, "O-REQ-01 must fire: {stats:?}");
    assert!(stats.oreq_02_avoided >= 1, "O-REQ-02 must fire: {stats:?}");
    assert_eq!(
        stats.oreq_03_avoided, 1,
        "O-REQ-03 must fire once: {stats:?}"
    );
    assert!(stats.oreq_04_avoided >= 1, "O-REQ-04 must fire: {stats:?}");
    assert!(stats.oreq_05_avoided >= 1, "O-REQ-05 must fire: {stats:?}");
    assert!(stats.total_avoided() >= 5, "total avoided: {stats:?}");

    // The graph snapshot exposes the session's running totals at end
    // of walk-2.
    assert_eq!(g2.oreq_stats, stats);
}

// ---------------------------------------------------------------------------
// Negative control: with no session attached, walks are byte-identical
// to pre-B.2-T1 behaviour (no counters, no extra calls).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_session_means_zero_counters_and_no_dedup_path() {
    let mut src = CountingSource::new();
    src.add_pom(
        co("ex", "A"),
        "1.0",
        pom("ex", "A", "1.0", vec![dep("ex", "C", "1.0")]),
    );
    src.add_pom(
        co("ex", "B"),
        "1.0",
        pom("ex", "B", "1.0", vec![dep("ex", "C", "1.0")]),
    );
    src.add_pom(co("ex", "C"), "1.0", pom("ex", "C", "1.0", vec![]));
    let root = pom(
        "ex",
        "root",
        "1.0",
        vec![dep("ex", "A", "1.0"), dep("ex", "B", "1.0")],
    );
    let opts = WalkOptions {
        oreq: None,
        enable_skipper: false,
        ..WalkOptions::default()
    };
    let g = walk(&resolved(root), &src, &opts).await.expect("walk ok");
    let s = g.oreq_stats;
    assert_eq!(s.oreq_01_avoided, 0);
    assert_eq!(s.oreq_02_avoided, 0);
    assert_eq!(s.oreq_03_avoided, 0);
    assert_eq!(s.oreq_04_avoided, 0);
    assert_eq!(s.oreq_05_avoided, 0);
}

// ---------------------------------------------------------------------------
// Suppress unused-import warning: `RawParent` reserved for future
// parent-resolver-backed O-REQ-04 tests once T7 wires the parent path.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _reserved_parent_helper() -> Option<RawParent> {
    None
}
