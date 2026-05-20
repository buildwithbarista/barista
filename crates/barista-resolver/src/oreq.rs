// SPDX-License-Identifier: MIT OR Apache-2.0

//! O-REQ-01 through O-REQ-05 — request-count optimizations.
//!
//! This module implements the in-session and across-invocation
//! request-count optimizations described in PRD §18.3 (the "request
//! count, E1" optimization catalog). Each optimization is named, has a
//! counter that advances exactly when the corresponding redundant
//! upstream request was avoided, and links to a finding in the
//! catalog at `docs/efficiency/findings/`.
//!
//! # Contract
//!
//! Counters are **advisory** — they are diagnostic signals consumed
//! by tests, the `barista-bench` results document (via the
//! `metadata` map), and operator-facing telemetry. They are
//! **not load-bearing for correctness**: a walk that ignores the
//! counters and routes every fetch to the underlying
//! [`MetadataSource`] produces the same resolved graph. The
//! optimization is purely about avoiding redundant work.
//!
//! # The five optimizations
//!
//! | ID         | PRD §18.3 anchor                                      | Finding         | Hook                                                  |
//! |------------|-------------------------------------------------------|-----------------|-------------------------------------------------------|
//! | O-REQ-01   | session-scoped per-`(repo, GA)` metadata dedup        | EFF-2026-001    | [`OreqSession::lookup_metadata`]                      |
//! | O-REQ-02   | conditional fetches (`If-None-Match`/`If-Modified-Since`) | EFF-2026-004 | [`OreqSession::record_metadata_origin`] (FetchOrigin::Disk -> 304-equivalent saved) |
//! | O-REQ-03   | lockfile-mode skip metadata                           | EFF-2026-005    | [`OreqSession::frozen_pin`]                           |
//! | O-REQ-04   | parent-POM dedup across siblings                      | EFF-2026-006    | [`OreqSession::lookup_parent_pom`]                    |
//! | O-REQ-05   | effective-POM caching (in-session)                    | EFF-2026-007    | [`OreqSession::lookup_effective_pom`]                 |
//!
//! Findings 001 is a seed catalog entry from B.1 T4 covering the
//! O-REQ-01 pattern. Findings 004–007 are filed by this task
//! (B.2 T1) and live in `docs/efficiency/findings/`.
//!
//! # In-session vs. across-invocation
//!
//! O-REQ-01, O-REQ-02 (in its 304-saved form), O-REQ-03, O-REQ-04,
//! and O-REQ-05 all operate on a single CLI invocation — they live
//! in [`OreqSession`], which is created at the start of `walk` and
//! discarded at the end. O-REQ-05's across-invocation form
//! (effective POMs cached on disk) is downstream of this task and
//! lives in `barista-cache`; the in-session counter still fires when
//! two paths in the same walk resolve to the same effective POM, so
//! the test coverage and the counter are meaningful in T1.
//!
//! # Counter exposure
//!
//! [`OreqStats`] is a snapshot of the five counters that callers
//! read at the end of a walk. The bench harness rolls these into
//! `results.json::metadata` under keys `oreq_01_avoided`,
//! `oreq_02_avoided`, ..., `oreq_05_avoided` so the dashboard
//! can chart them.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use barista_coords::Coords;
use barista_pom::{RawPom, ResolvedPom};

use crate::source::{FetchOrigin, GaMetadata};

/// Five `AtomicU64` counters, one per O-REQ-XX optimization.
///
/// All loads use [`Ordering::Relaxed`] — these counters are
/// diagnostic, not synchronizing.
#[derive(Debug, Default)]
pub struct OreqCounters {
    /// O-REQ-01 — `maven-metadata.xml` fetches deduplicated by
    /// in-session cache.
    /// EFF-LINK: docs/efficiency/findings/EFF-2026-001.md
    pub oreq_01_metadata_dedup_avoided: AtomicU64,
    /// O-REQ-02 — `maven-metadata.xml` fetches served from the
    /// local cache via a conditional revalidation (304-equivalent)
    /// rather than a full upstream GET. The signal is "the source
    /// reported [`FetchOrigin::Disk`] or [`FetchOrigin::InMemory`]
    /// on the first in-session lookup", which is the in-session
    /// proxy for "the cache layer's `If-None-Match` / `If-Modified-Since`
    /// roundtrip avoided a body transfer."
    /// EFF-LINK: docs/efficiency/findings/EFF-2026-004.md
    pub oreq_02_conditional_revalidations: AtomicU64,
    /// O-REQ-03 — `maven-metadata.xml` fetches skipped entirely
    /// because the session was configured with frozen-lockfile
    /// pins that already specify a concrete version.
    /// EFF-LINK: docs/efficiency/findings/EFF-2026-005.md
    pub oreq_03_frozen_metadata_skipped: AtomicU64,
    /// O-REQ-04 — POM fetches for a coord+version that was already
    /// fetched (typically a shared parent POM across multiple
    /// children) served from the in-session cache.
    /// EFF-LINK: docs/efficiency/findings/EFF-2026-006.md
    pub oreq_04_parent_pom_dedup_avoided: AtomicU64,
    /// O-REQ-05 — effective-POM resolutions for a coord+version
    /// already resolved in this session served from the in-session
    /// cache instead of re-running the (parent merge + interpolation +
    /// depMgt) pipeline.
    /// EFF-LINK: docs/efficiency/findings/EFF-2026-007.md
    pub oreq_05_effective_pom_dedup_avoided: AtomicU64,
}

impl OreqCounters {
    /// Snapshot all five counters into a plain-data [`OreqStats`].
    pub fn snapshot(&self) -> OreqStats {
        OreqStats {
            oreq_01_avoided: self.oreq_01_metadata_dedup_avoided.load(Ordering::Relaxed),
            oreq_02_avoided: self
                .oreq_02_conditional_revalidations
                .load(Ordering::Relaxed),
            oreq_03_avoided: self.oreq_03_frozen_metadata_skipped.load(Ordering::Relaxed),
            oreq_04_avoided: self
                .oreq_04_parent_pom_dedup_avoided
                .load(Ordering::Relaxed),
            oreq_05_avoided: self
                .oreq_05_effective_pom_dedup_avoided
                .load(Ordering::Relaxed),
        }
    }
}

/// Plain-data snapshot of [`OreqCounters`]. `Copy + Clone` so a
/// caller can hold it after the session goes out of scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OreqStats {
    /// Avoided `maven-metadata.xml` fetches via in-session dedup.
    pub oreq_01_avoided: u64,
    /// `maven-metadata.xml` revalidations served as a 304-equivalent
    /// (cache origin) rather than a full upstream GET.
    pub oreq_02_avoided: u64,
    /// `maven-metadata.xml` fetches skipped due to frozen-lockfile pins.
    pub oreq_03_avoided: u64,
    /// Parent / sibling POM fetches deduplicated in-session.
    pub oreq_04_avoided: u64,
    /// Effective-POM resolutions deduplicated in-session.
    pub oreq_05_avoided: u64,
}

impl OreqStats {
    /// Total number of avoided fetches across all five optimizations.
    /// Convenient for "show me the total redundant work avoided"
    /// reporting; does not weight the optimizations.
    pub fn total_avoided(&self) -> u64 {
        self.oreq_01_avoided
            .saturating_add(self.oreq_02_avoided)
            .saturating_add(self.oreq_03_avoided)
            .saturating_add(self.oreq_04_avoided)
            .saturating_add(self.oreq_05_avoided)
    }

    /// Render the snapshot as the key/value pairs that the
    /// `barista-bench` `results.json::metadata` map expects. The
    /// values are stringified so the on-disk format does not need a
    /// schema bump; consumers parse them back via `str::parse::<u64>`.
    pub fn to_bench_metadata(&self) -> [(String, String); 5] {
        [
            (
                "oreq_01_avoided".to_string(),
                self.oreq_01_avoided.to_string(),
            ),
            (
                "oreq_02_avoided".to_string(),
                self.oreq_02_avoided.to_string(),
            ),
            (
                "oreq_03_avoided".to_string(),
                self.oreq_03_avoided.to_string(),
            ),
            (
                "oreq_04_avoided".to_string(),
                self.oreq_04_avoided.to_string(),
            ),
            (
                "oreq_05_avoided".to_string(),
                self.oreq_05_avoided.to_string(),
            ),
        ]
    }
}

/// Per-session optimization state.
///
/// Created at the start of [`crate::walker::walk`], threaded through
/// the BFS loop, and discarded when the walk finishes. The session
/// owns the per-`(repo, GA)` metadata cache (O-REQ-01), the
/// frozen-lockfile pin map (O-REQ-03), the parent-POM dedup cache
/// (O-REQ-04), and the effective-POM dedup cache (O-REQ-05). The
/// counters are exposed via [`OreqSession::stats`].
///
/// The internal maps are guarded by `Mutex` rather than relying on
/// the walker's single-threaded BFS contract — this future-proofs
/// the session for the eventual parallel resolver work in
/// M2.3, and the lock is uncontended in the single-threaded case so
/// the overhead is negligible. Lock acquisition is short and never
/// holds across `.await` points (callers `lookup_*` -> get the
/// optional hit -> drop the guard before invoking the source).
#[derive(Debug)]
pub struct OreqSession {
    counters: OreqCounters,
    /// O-REQ-01: in-session `maven-metadata.xml` cache keyed by
    /// `(repo, group:artifact)`. The repo is identified by the
    /// caller-supplied string; for v0.1 the only repo is the
    /// configured default upstream, but the API takes a key so a
    /// future multi-repo resolver doesn't have to re-plumb it.
    metadata_cache: Mutex<HashMap<MetadataKey, GaMetadata>>,
    /// O-REQ-03: frozen-lockfile pins. When `Some`, [`is_frozen`]
    /// returns true and metadata-lookups for pinned coords short-circuit.
    frozen_pins: Option<HashMap<Coords, String>>,
    /// O-REQ-04 + O-REQ-05: in-session POM caches. The raw POM
    /// cache (O-REQ-04) catches sibling-modules-share-a-parent-POM,
    /// and the resolved-POM cache (O-REQ-05) catches the case where
    /// two walks of the same `(coords, version)` would otherwise
    /// re-run the (parent + interpolation + depMgt) pipeline.
    raw_pom_cache: Mutex<HashMap<PomKey, RawPom>>,
    resolved_pom_cache: Mutex<HashMap<PomKey, ResolvedPom>>,
}

/// Key for the O-REQ-01 metadata cache. The repo string identifies
/// the upstream (typically the configured default repository URL);
/// the coord identifies the group:artifact. The two together form
/// the dedup key per PRD §18.3 O-REQ-01.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MetadataKey {
    pub repo: String,
    pub coords: Coords,
}

/// Key for the O-REQ-04/05 POM caches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PomKey {
    coords: Coords,
    version: String,
}

impl OreqSession {
    /// Construct a new session with all counters zeroed and no
    /// frozen-lockfile pins.
    pub fn new() -> Self {
        Self {
            counters: OreqCounters::default(),
            metadata_cache: Mutex::new(HashMap::new()),
            frozen_pins: None,
            raw_pom_cache: Mutex::new(HashMap::new()),
            resolved_pom_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Configure frozen-lockfile mode (O-REQ-03). When set, any
    /// metadata lookup for a coord whose pin is present skips the
    /// upstream fetch entirely. The pin's version string is the
    /// authoritative resolved version.
    pub fn with_frozen_pins(mut self, pins: HashMap<Coords, String>) -> Self {
        self.frozen_pins = Some(pins);
        self
    }

    /// `true` iff this session is operating in frozen-lockfile mode.
    pub fn is_frozen(&self) -> bool {
        self.frozen_pins.is_some()
    }

    /// Return the frozen pin for `coords` if the session is frozen
    /// and a pin exists. Does **not** advance any counter — counters
    /// only advance through [`Self::record_frozen_skip`] which the
    /// caller invokes after deciding to take the short-circuit path.
    pub fn frozen_pin(&self, coords: &Coords) -> Option<String> {
        self.frozen_pins
            .as_ref()
            .and_then(|p| p.get(coords).cloned())
    }

    /// O-REQ-01: look up `(repo, coords)` in the in-session metadata
    /// cache. On a hit, increments the O-REQ-01 counter and returns
    /// the cached metadata. On a miss, returns `None` and the caller
    /// must fetch from the underlying source, then deposit the
    /// result via [`Self::deposit_metadata`].
    pub fn lookup_metadata(&self, key: &MetadataKey) -> Option<GaMetadata> {
        let guard = self
            .metadata_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(md) = guard.get(key) {
            self.counters
                .oreq_01_metadata_dedup_avoided
                .fetch_add(1, Ordering::Relaxed);
            Some(md.clone())
        } else {
            None
        }
    }

    /// Deposit metadata into the in-session cache.
    ///
    /// Should be called by the caller after a successful
    /// [`crate::source::MetadataSource::fetch_metadata`] when the
    /// previous [`Self::lookup_metadata`] returned `None`.
    ///
    /// `origin` is consulted for O-REQ-02 accounting: when the
    /// underlying source reported the answer came from a local
    /// cache ([`FetchOrigin::Disk`] / [`FetchOrigin::InMemory`])
    /// rather than a fresh upstream fetch ([`FetchOrigin::Remote`]),
    /// we treat that as a successful conditional revalidation in the
    /// 304-equivalent sense the cache layer implements (see PRD §18.3
    /// O-REQ-02). Fixture origin is ignored — fixtures don't model
    /// upstream behavior.
    pub fn deposit_metadata(&self, key: MetadataKey, md: GaMetadata, origin: FetchOrigin) {
        self.record_metadata_origin(origin);
        let mut guard = self
            .metadata_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        guard.insert(key, md);
    }

    /// O-REQ-02 — record an in-session fetch's origin. Used by
    /// callers that want to bump the conditional-revalidation
    /// counter without going through [`Self::deposit_metadata`]
    /// (e.g. when the lookup returned a hit but the caller still
    /// wants to report that the *first* fetch in the session was
    /// itself a 304-equivalent).
    pub fn record_metadata_origin(&self, origin: FetchOrigin) {
        match origin {
            FetchOrigin::Disk | FetchOrigin::InMemory => {
                self.counters
                    .oreq_02_conditional_revalidations
                    .fetch_add(1, Ordering::Relaxed);
            }
            FetchOrigin::Remote | FetchOrigin::Fixture => {}
        }
    }

    /// O-REQ-03 — record one avoided metadata fetch under
    /// frozen-lockfile mode. Caller invokes this whenever a metadata
    /// lookup would have happened but a frozen pin satisfied the
    /// request directly. See [`Self::frozen_pin`] for the
    /// non-counter lookup that decides whether to take this branch.
    pub fn record_frozen_skip(&self) {
        self.counters
            .oreq_03_frozen_metadata_skipped
            .fetch_add(1, Ordering::Relaxed);
    }

    /// O-REQ-04 — look up a raw POM (typically a parent POM, but
    /// any `(coords, version)` works) in the in-session cache. On a
    /// hit, increments the O-REQ-04 counter and returns the cached
    /// `RawPom`. On a miss, returns `None`.
    pub fn lookup_parent_pom(&self, coords: &Coords, version: &str) -> Option<RawPom> {
        let key = PomKey {
            coords: coords.clone(),
            version: version.to_string(),
        };
        let guard = self.raw_pom_cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(p) = guard.get(&key) {
            self.counters
                .oreq_04_parent_pom_dedup_avoided
                .fetch_add(1, Ordering::Relaxed);
            Some(p.clone())
        } else {
            None
        }
    }

    /// Deposit a raw POM into the in-session cache. Idempotent —
    /// subsequent puts for the same `(coords, version)` overwrite
    /// the previous entry (POMs at the same coord+version are
    /// content-equivalent by Maven's immutability rules; collisions
    /// in practice indicate a corpus bug, not data loss).
    pub fn deposit_parent_pom(&self, coords: &Coords, version: &str, pom: RawPom) {
        let key = PomKey {
            coords: coords.clone(),
            version: version.to_string(),
        };
        let mut guard = self.raw_pom_cache.lock().unwrap_or_else(|p| p.into_inner());
        guard.insert(key, pom);
    }

    /// O-REQ-05 — look up an effective POM in the in-session
    /// cache. On a hit, increments the O-REQ-05 counter and returns
    /// the cached [`ResolvedPom`]. On a miss, returns `None` and
    /// the caller must run the resolve pipeline then
    /// [`Self::deposit_effective_pom`] the result.
    pub fn lookup_effective_pom(&self, coords: &Coords, version: &str) -> Option<ResolvedPom> {
        let key = PomKey {
            coords: coords.clone(),
            version: version.to_string(),
        };
        let guard = self
            .resolved_pom_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(r) = guard.get(&key) {
            self.counters
                .oreq_05_effective_pom_dedup_avoided
                .fetch_add(1, Ordering::Relaxed);
            Some(r.clone())
        } else {
            None
        }
    }

    /// Deposit an effective POM into the in-session cache. See
    /// [`Self::deposit_parent_pom`] re: overwrite semantics.
    pub fn deposit_effective_pom(&self, coords: &Coords, version: &str, resolved: ResolvedPom) {
        let key = PomKey {
            coords: coords.clone(),
            version: version.to_string(),
        };
        let mut guard = self
            .resolved_pom_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        guard.insert(key, resolved);
    }

    /// Snapshot the current counter values.
    pub fn stats(&self) -> OreqStats {
        self.counters.snapshot()
    }

    /// Borrow the underlying counters for atomic-level reads in
    /// tests that want to observe individual increments without
    /// snapshotting all five.
    pub fn counters(&self) -> &OreqCounters {
        &self.counters
    }
}

impl Default for OreqSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use barista_pom::{EffectivePom, Properties, RawPom, ResolvedPom};

    fn co(g: &str, a: &str) -> Coords {
        Coords::new(g, a).expect("valid coords")
    }

    fn meta(g: &str, a: &str, versions: &[&str]) -> GaMetadata {
        GaMetadata {
            coords: co(g, a),
            versions: versions.iter().map(|s| (*s).to_string()).collect(),
            latest_snapshot_timestamp: None,
            last_updated: None,
        }
    }

    fn raw_pom(g: &str, a: &str, v: &str) -> RawPom {
        RawPom {
            model_version: "4.0.0".into(),
            group_id: Some(g.into()),
            artifact_id: a.into(),
            version: Some(v.into()),
            packaging: "jar".into(),
            properties: Properties::default(),
            ..RawPom::default()
        }
    }

    fn resolved_pom(g: &str, a: &str, v: &str) -> ResolvedPom {
        let pom = raw_pom(g, a, v);
        ResolvedPom {
            effective: EffectivePom {
                pom: pom.clone(),
                interpolations: Vec::new(),
                parent_chain: Vec::new(),
            },
            pom,
            active_profile_ids: Vec::new(),
            imported_boms: Vec::new(),
        }
    }

    // ---- O-REQ-01 --------------------------------------------------------

    #[test]
    fn oreq_01_metadata_dedup_fires_on_repeat_lookup() {
        let s = OreqSession::new();
        let key = MetadataKey {
            repo: "central".into(),
            coords: co("org.example", "lib"),
        };
        // First lookup: miss.
        assert!(s.lookup_metadata(&key).is_none());
        // Deposit + look up again: hit.
        s.deposit_metadata(
            key.clone(),
            meta("org.example", "lib", &["1.0", "2.0"]),
            FetchOrigin::Remote,
        );
        assert_eq!(s.stats().oreq_01_avoided, 0);
        let hit = s.lookup_metadata(&key).expect("cached");
        assert_eq!(hit.versions, vec!["1.0", "2.0"]);
        assert_eq!(s.stats().oreq_01_avoided, 1);
        // A second hit increments again.
        let _ = s.lookup_metadata(&key);
        assert_eq!(s.stats().oreq_01_avoided, 2);
    }

    #[test]
    fn oreq_01_does_not_fire_across_distinct_repos() {
        let s = OreqSession::new();
        let key_central = MetadataKey {
            repo: "central".into(),
            coords: co("org.example", "lib"),
        };
        let key_nexus = MetadataKey {
            repo: "nexus".into(),
            coords: co("org.example", "lib"),
        };
        s.deposit_metadata(
            key_central.clone(),
            meta("org.example", "lib", &["1.0"]),
            FetchOrigin::Remote,
        );
        assert!(s.lookup_metadata(&key_nexus).is_none());
        assert_eq!(s.stats().oreq_01_avoided, 0);
    }

    // ---- O-REQ-02 --------------------------------------------------------

    #[test]
    fn oreq_02_counter_advances_on_disk_origin() {
        let s = OreqSession::new();
        let key = MetadataKey {
            repo: "central".into(),
            coords: co("g", "a"),
        };
        s.deposit_metadata(key, meta("g", "a", &["1.0"]), FetchOrigin::Disk);
        assert_eq!(s.stats().oreq_02_avoided, 1);
    }

    #[test]
    fn oreq_02_counter_does_not_advance_on_remote_origin() {
        let s = OreqSession::new();
        let key = MetadataKey {
            repo: "central".into(),
            coords: co("g", "a"),
        };
        s.deposit_metadata(key, meta("g", "a", &["1.0"]), FetchOrigin::Remote);
        assert_eq!(s.stats().oreq_02_avoided, 0);
    }

    // ---- O-REQ-03 --------------------------------------------------------

    #[test]
    fn oreq_03_frozen_pin_short_circuits_metadata_lookup() {
        let mut pins = HashMap::new();
        pins.insert(co("org.foo", "bar"), "1.2.3".to_string());
        let s = OreqSession::new().with_frozen_pins(pins);
        assert!(s.is_frozen());
        // The caller asks for the pin first; if present, skips metadata.
        let pin = s.frozen_pin(&co("org.foo", "bar"));
        assert_eq!(pin.as_deref(), Some("1.2.3"));
        // Counter does NOT advance on lookup — only on explicit
        // record. This keeps the lookup pure and lets the caller
        // decide whether the short-circuit actually fired.
        assert_eq!(s.stats().oreq_03_avoided, 0);
        s.record_frozen_skip();
        assert_eq!(s.stats().oreq_03_avoided, 1);
    }

    #[test]
    fn oreq_03_no_pin_for_unknown_coords() {
        let mut pins = HashMap::new();
        pins.insert(co("org.foo", "bar"), "1.0".to_string());
        let s = OreqSession::new().with_frozen_pins(pins);
        assert!(s.frozen_pin(&co("org.other", "lib")).is_none());
    }

    // ---- O-REQ-04 --------------------------------------------------------

    #[test]
    fn oreq_04_parent_pom_dedup_fires_on_repeat() {
        let s = OreqSession::new();
        let coords = co("org.springframework.boot", "spring-boot-dependencies");
        assert!(s.lookup_parent_pom(&coords, "3.2.0").is_none());
        s.deposit_parent_pom(
            &coords,
            "3.2.0",
            raw_pom(
                "org.springframework.boot",
                "spring-boot-dependencies",
                "3.2.0",
            ),
        );
        // Two sibling starter POMs walk the parent chain → second
        // lookup hits the cache.
        let hit = s.lookup_parent_pom(&coords, "3.2.0").expect("cached");
        assert_eq!(hit.artifact_id, "spring-boot-dependencies");
        assert_eq!(s.stats().oreq_04_avoided, 1);
        let _ = s.lookup_parent_pom(&coords, "3.2.0");
        assert_eq!(s.stats().oreq_04_avoided, 2);
    }

    #[test]
    fn oreq_04_distinguishes_versions() {
        let s = OreqSession::new();
        let coords = co("g", "parent");
        s.deposit_parent_pom(&coords, "1.0", raw_pom("g", "parent", "1.0"));
        assert!(s.lookup_parent_pom(&coords, "2.0").is_none());
        assert_eq!(s.stats().oreq_04_avoided, 0);
    }

    // ---- O-REQ-05 --------------------------------------------------------

    #[test]
    fn oreq_05_effective_pom_dedup_fires_on_repeat() {
        let s = OreqSession::new();
        let coords = co("g", "a");
        assert!(s.lookup_effective_pom(&coords, "1.0").is_none());
        s.deposit_effective_pom(&coords, "1.0", resolved_pom("g", "a", "1.0"));
        assert!(s.lookup_effective_pom(&coords, "1.0").is_some());
        assert_eq!(s.stats().oreq_05_avoided, 1);
    }

    // ---- Snapshot / metadata export -------------------------------------

    #[test]
    fn snapshot_collects_all_counters() {
        let s = OreqSession::new();
        // Drive each counter independently.
        s.deposit_metadata(
            MetadataKey {
                repo: "r".into(),
                coords: co("g", "a"),
            },
            meta("g", "a", &["1.0"]),
            FetchOrigin::Disk, // bumps O-REQ-02
        );
        let _ = s.lookup_metadata(&MetadataKey {
            repo: "r".into(),
            coords: co("g", "a"),
        }); // bumps O-REQ-01
        s.record_frozen_skip(); // bumps O-REQ-03
        s.deposit_parent_pom(&co("g", "parent"), "1.0", raw_pom("g", "parent", "1.0"));
        let _ = s.lookup_parent_pom(&co("g", "parent"), "1.0"); // O-REQ-04
        s.deposit_effective_pom(&co("g", "eff"), "1.0", resolved_pom("g", "eff", "1.0"));
        let _ = s.lookup_effective_pom(&co("g", "eff"), "1.0"); // O-REQ-05

        let stats = s.stats();
        assert_eq!(stats.oreq_01_avoided, 1);
        assert_eq!(stats.oreq_02_avoided, 1);
        assert_eq!(stats.oreq_03_avoided, 1);
        assert_eq!(stats.oreq_04_avoided, 1);
        assert_eq!(stats.oreq_05_avoided, 1);
        assert_eq!(stats.total_avoided(), 5);
    }

    #[test]
    fn bench_metadata_export_has_stable_keys() {
        let stats = OreqStats {
            oreq_01_avoided: 3,
            oreq_02_avoided: 0,
            oreq_03_avoided: 12,
            oreq_04_avoided: 1,
            oreq_05_avoided: 2,
        };
        let kv = stats.to_bench_metadata();
        let keys: Vec<&str> = kv.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "oreq_01_avoided",
                "oreq_02_avoided",
                "oreq_03_avoided",
                "oreq_04_avoided",
                "oreq_05_avoided",
            ]
        );
        let values: Vec<&str> = kv.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(values, vec!["3", "0", "12", "1", "2"]);
    }

    #[test]
    fn fixture_origin_does_not_bump_oreq_02() {
        let s = OreqSession::new();
        s.deposit_metadata(
            MetadataKey {
                repo: "r".into(),
                coords: co("g", "a"),
            },
            meta("g", "a", &["1.0"]),
            FetchOrigin::Fixture,
        );
        assert_eq!(s.stats().oreq_02_avoided, 0);
    }

    #[test]
    fn default_session_has_no_pins_and_zero_stats() {
        let s = OreqSession::default();
        assert!(!s.is_frozen());
        assert_eq!(s.stats(), OreqStats::default());
    }
}
