// SPDX-License-Identifier: MIT OR Apache-2.0

//! Skipper — BFS subtree pruning for the dependency walker.
//!
//! Maven's BFS collector becomes "BFS+Skipper" when this module
//! activates. The skipper asks: "before we fetch this coord's POM and
//! walk its transitives, is it possible the resulting subgraph would
//! win against the already-resolved state? If not, skip."
//!
//! The skipper is correctness-safe: if it decides to skip, the
//! resolved graph is identical to a walk without skipping (modulo
//! the audit trail's "also_seen_at" entries). If the skipper is
//! disabled (see [`crate::walker::WalkOptions::enable_skipper`]),
//! the walker still produces the same graph, just slower.
//!
//! # Two pruning modes
//!
//! 1. **Already-resolved-shallower.** If a coord is winning at strictly
//!    shallower depth than the candidate visit AND the new path's
//!    accumulated exclusions are a superset of the winning path's
//!    exclusions, the new visit cannot produce any new edge that
//!    wouldn't already be produced by the winning visit. Skip.
//!
//! 2. **Known-leaf cache** (MRESOLVER-256). If a coord was previously
//!    expanded and found to have zero transitive dependencies, we can
//!    skip re-fetching its POM on subsequent visits — there's nothing
//!    to expand.
//!
//! # Correctness invariant
//!
//! `walk(opts.enable_skipper = true) == walk(opts.enable_skipper = false)`
//! for the `winners` map. The `audit.also_seen_at` lists may differ
//! (the skipper short-circuits before recording some losers), but the
//! winning version per coord is identical. This invariant is exercised
//! by the integration tests in `tests/walker_skipper.rs`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use barista_coords::Coords;
use barista_pom::RawExclusion;

use crate::walker::ResolvedDep;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The skipper's decision for a candidate visit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipDecision {
    /// Fetch the POM and walk transitives.
    Walk,
    /// Skip the subtree.
    Skip { reason: SkipReason },
}

/// Why a particular candidate visit was pruned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The coord is already winning at strictly shallower depth, and
    /// the new path's accumulated exclusions are a superset of the
    /// winning path's exclusions (so we couldn't produce a different
    /// transitive subgraph).
    AlreadyResolvedShallowerWithCompatibleExclusions { winning_depth: u32, new_depth: u32 },
    /// The coord is a known leaf (no transitive deps in its POM).
    /// Cache-hit on a leaf node per MRESOLVER-256.
    KnownLeaf,
}

/// An exclusion pattern set. The walker accumulates these along the
/// path; the skipper compares two sets to decide whether the new path's
/// exclusions are "compatible" (i.e. a superset of the winner's).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExclusionSet {
    items: BTreeSet<ExclusionPattern>,
}

/// A single Maven `<exclusion>` pattern. Either field may be `"*"` to
/// match any group / artifact.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExclusionPattern {
    pub group: String,
    pub artifact: String,
}

impl ExclusionPattern {
    /// Construct a pattern. Empty strings are treated as `"*"` (Maven's
    /// own grammar disallows empty `<groupId>` / `<artifactId>` inside an
    /// `<exclusion>`, but we normalise defensively).
    pub fn new(group: impl Into<String>, artifact: impl Into<String>) -> Self {
        let mut g = group.into();
        let mut a = artifact.into();
        if g.is_empty() {
            g = "*".into();
        }
        if a.is_empty() {
            a = "*".into();
        }
        Self {
            group: g,
            artifact: a,
        }
    }

    /// Does this pattern match the given coordinate?
    pub fn matches(&self, coords: &Coords) -> bool {
        (self.group == "*" || self.group == coords.group)
            && (self.artifact == "*" || self.artifact == coords.artifact)
    }

    /// Does this pattern *subsume* another, i.e. is every coord matched
    /// by `other` also matched by `self`?
    fn subsumes(&self, other: &ExclusionPattern) -> bool {
        (self.group == "*" || self.group == other.group)
            && (self.artifact == "*" || self.artifact == other.artifact)
    }
}

impl ExclusionSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct from a slice of [`RawExclusion`] (the walker's wire
    /// type).
    pub fn from_raw(raw: &[RawExclusion]) -> Self {
        let mut s = Self::new();
        for r in raw {
            s.insert(ExclusionPattern::new(&r.group_id, &r.artifact_id));
        }
        s
    }

    pub fn insert(&mut self, pat: ExclusionPattern) {
        self.items.insert(pat);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Iterate the underlying patterns.
    pub fn iter(&self) -> impl Iterator<Item = &ExclusionPattern> {
        self.items.iter()
    }

    /// Does any pattern in this set match `coords`?
    pub fn matches_any(&self, coords: &Coords) -> bool {
        self.items.iter().any(|p| p.matches(coords))
    }

    /// Is this set a *superset* of `other` (in terms of coords matched)?
    ///
    /// Wildcard semantics: `*:*` in `self` subsumes any pattern in
    /// `other`; `g:*` subsumes `g:a`; etc.
    pub fn is_superset_of(&self, other: &ExclusionSet) -> bool {
        for pat in &other.items {
            if !self.contains_matching(pat) {
                return false;
            }
        }
        true
    }

    fn contains_matching(&self, pat: &ExclusionPattern) -> bool {
        self.items.iter().any(|our| our.subsumes(pat))
    }
}

// ---------------------------------------------------------------------------
// SkipperState
// ---------------------------------------------------------------------------

/// Stats for the audit / benchmark.
#[derive(Debug, Default, Clone)]
pub struct SkipperStats {
    /// Times the skipper short-circuited because the coord was already
    /// winning at shallower depth with compatible exclusions.
    pub skips_already_resolved: u64,
    /// Times the skipper short-circuited because the coord is a known
    /// leaf (MRESOLVER-256).
    pub skips_known_leaf: u64,
    /// Times [`SkipperState::decide`] returned `Walk`.
    pub walks: u64,
}

impl SkipperStats {
    /// Total number of skips across all reasons.
    pub fn total_skips(&self) -> u64 {
        self.skips_already_resolved + self.skips_known_leaf
    }

    /// Total decisions made.
    pub fn total_decisions(&self) -> u64 {
        self.walks + self.total_skips()
    }

    /// Fraction of decisions that were skips, in `[0.0, 1.0]`. Returns
    /// `0.0` when no decisions have been made yet.
    pub fn skip_rate(&self) -> f64 {
        let total = self.total_decisions();
        if total == 0 {
            0.0
        } else {
            self.total_skips() as f64 / total as f64
        }
    }
}

/// The skipper's mutable state. Updated as the BFS walk progresses.
#[derive(Debug, Default, Clone)]
pub struct SkipperState {
    /// For each winning coord, the path's accumulated exclusions at the
    /// time it was emitted. Compared against new candidates' exclusions
    /// to verify the "compatible exclusions" invariant.
    winners_exclusions: HashMap<Coords, ExclusionSet>,
    /// Coords whose POMs were expanded and found to have no transitive
    /// dependencies (leaf cache per MRESOLVER-256).
    known_leaves: HashSet<Coords>,
    /// Telemetry.
    pub stats: SkipperStats,
    /// When `false`, [`decide`](Self::decide) always returns `Walk` and
    /// [`record_visit`](Self::record_visit) is a no-op for the leaf
    /// cache. This lets the walker run in differential-test mode.
    enabled: bool,
}

impl SkipperState {
    /// Create a new skipper in the enabled state.
    pub fn new() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Create a disabled skipper. Always returns `Walk`; never records.
    /// Used by [`crate::walker::WalkOptions::enable_skipper`] = `false`.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// Is the skipper enabled?
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Decide what to do with a candidate visit.
    ///
    /// - `coords`: the candidate's resolution identity.
    /// - `depth`: the candidate's BFS depth (root direct deps are 1).
    /// - `path_exclusions`: exclusions accumulated along the candidate's
    ///   parent path.
    /// - `winners`: the walker's current winners map (HashMap is what the
    ///   walker actually uses internally during BFS).
    pub fn decide(
        &mut self,
        coords: &Coords,
        depth: u32,
        path_exclusions: &ExclusionSet,
        winners: &HashMap<Coords, ResolvedDep>,
    ) -> SkipDecision {
        if !self.enabled {
            self.stats.walks += 1;
            return SkipDecision::Walk;
        }

        // 1. Known-leaf cache fast path. Re-visiting a leaf cannot
        //    produce any new edges.
        if self.known_leaves.contains(coords) {
            self.stats.skips_known_leaf += 1;
            return SkipDecision::Skip {
                reason: SkipReason::KnownLeaf,
            };
        }

        // 2. Already-winning at strictly shallower depth?
        if let Some(winner) = winners.get(coords) {
            if winner.depth < depth {
                // Exclusion compatibility: the new path's accumulated
                // exclusions must be a superset of the winning path's
                // exclusions. Otherwise the alternate path might
                // surface a transitive that the winner's exclusion set
                // would have masked, so we still have to walk it.
                let winner_excl = self
                    .winners_exclusions
                    .get(coords)
                    .cloned()
                    .unwrap_or_default();
                if path_exclusions.is_superset_of(&winner_excl) {
                    self.stats.skips_already_resolved += 1;
                    return SkipDecision::Skip {
                        reason: SkipReason::AlreadyResolvedShallowerWithCompatibleExclusions {
                            winning_depth: winner.depth,
                            new_depth: depth,
                        },
                    };
                }
            }
        }

        self.stats.walks += 1;
        SkipDecision::Walk
    }

    /// Called by the walker when it finishes expanding a coord: records
    /// the path's exclusions (so future visits can compare) and whether
    /// the coord turned out to be a leaf (so future visits can skip
    /// outright).
    pub fn record_visit(&mut self, coords: Coords, path_exclusions: ExclusionSet, was_leaf: bool) {
        if !self.enabled {
            return;
        }
        self.winners_exclusions
            .insert(coords.clone(), path_exclusions);
        if was_leaf {
            self.known_leaves.insert(coords);
        }
    }

    /// Read-only view of the per-coord exclusion sets recorded so far.
    /// Useful for introspection in tests; the BTreeMap re-collection
    /// keeps the view deterministic.
    pub fn winners_exclusions(&self) -> BTreeMap<Coords, ExclusionSet> {
        self.winners_exclusions
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Read-only view of the known-leaf set.
    pub fn known_leaves(&self) -> BTreeSet<Coords> {
        self.known_leaves.iter().cloned().collect()
    }

    /// Consume self and return final stats.
    pub fn into_stats(self) -> SkipperStats {
        self.stats
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walker::Scope;

    fn co(g: &str, a: &str) -> Coords {
        Coords::new(g, a).unwrap()
    }

    fn pat(g: &str, a: &str) -> ExclusionPattern {
        ExclusionPattern::new(g, a)
    }

    fn excl_set(items: &[(&str, &str)]) -> ExclusionSet {
        let mut s = ExclusionSet::new();
        for (g, a) in items {
            s.insert(pat(g, a));
        }
        s
    }

    fn winner(coords: &Coords, depth: u32) -> ResolvedDep {
        ResolvedDep {
            coords: coords.clone(),
            version: "1.0".to_string(),
            scope: Scope::Compile,
            classifier: None,
            type_: "jar".into(),
            optional: false,
            depth,
            winning_path: vec![coords.clone()],
        }
    }

    fn winners_with(entries: &[(Coords, u32)]) -> HashMap<Coords, ResolvedDep> {
        let mut m = HashMap::new();
        for (c, d) in entries {
            m.insert(c.clone(), winner(c, *d));
        }
        m
    }

    // 1. Empty winners → Walk.
    #[test]
    fn decide_empty_winners_walks() {
        let mut sk = SkipperState::new();
        let winners = HashMap::new();
        let d = sk.decide(&co("ex", "A"), 1, &ExclusionSet::new(), &winners);
        assert_eq!(d, SkipDecision::Walk);
        assert_eq!(sk.stats.walks, 1);
    }

    // 2. Already-winning shallower + compatible exclusions → Skip.
    #[test]
    fn decide_already_resolved_shallower_skips() {
        let mut sk = SkipperState::new();
        let c = co("ex", "A");
        let winners = winners_with(&[(c.clone(), 1)]);
        // Winner recorded with empty exclusions; new visit has empty too.
        sk.record_visit(c.clone(), ExclusionSet::new(), false);
        let d = sk.decide(&c, 3, &ExclusionSet::new(), &winners);
        assert!(matches!(
            d,
            SkipDecision::Skip {
                reason: SkipReason::AlreadyResolvedShallowerWithCompatibleExclusions { .. }
            }
        ));
        assert_eq!(sk.stats.skips_already_resolved, 1);
    }

    // 3. Same depth → Walk (skipper only skips strictly deeper).
    #[test]
    fn decide_same_depth_walks() {
        let mut sk = SkipperState::new();
        let c = co("ex", "A");
        let winners = winners_with(&[(c.clone(), 2)]);
        sk.record_visit(c.clone(), ExclusionSet::new(), false);
        let d = sk.decide(&c, 2, &ExclusionSet::new(), &winners);
        assert_eq!(d, SkipDecision::Walk);
    }

    // 4. New path has FEWER exclusions → not a superset → Walk.
    #[test]
    fn decide_fewer_exclusions_walks() {
        let mut sk = SkipperState::new();
        let c = co("ex", "A");
        let winners = winners_with(&[(c.clone(), 1)]);
        let winner_excl = excl_set(&[("org.foo", "bar")]);
        sk.record_visit(c.clone(), winner_excl, false);
        // New path has no exclusions — strictly fewer.
        let d = sk.decide(&c, 3, &ExclusionSet::new(), &winners);
        assert_eq!(d, SkipDecision::Walk);
    }

    // 5. New path has MORE (superset) exclusions → Skip.
    #[test]
    fn decide_more_exclusions_skips() {
        let mut sk = SkipperState::new();
        let c = co("ex", "A");
        let winners = winners_with(&[(c.clone(), 1)]);
        let winner_excl = excl_set(&[("org.foo", "bar")]);
        sk.record_visit(c.clone(), winner_excl, false);
        let new_excl = excl_set(&[("org.foo", "bar"), ("org.baz", "qux")]);
        let d = sk.decide(&c, 3, &new_excl, &winners);
        assert!(matches!(
            d,
            SkipDecision::Skip {
                reason: SkipReason::AlreadyResolvedShallowerWithCompatibleExclusions { .. }
            }
        ));
    }

    // 6. Known-leaf coord → Skip(KnownLeaf), regardless of winners.
    #[test]
    fn decide_known_leaf_skips() {
        let mut sk = SkipperState::new();
        let c = co("ex", "A");
        sk.record_visit(c.clone(), ExclusionSet::new(), true);
        let winners = HashMap::new();
        let d = sk.decide(&c, 5, &ExclusionSet::new(), &winners);
        assert_eq!(
            d,
            SkipDecision::Skip {
                reason: SkipReason::KnownLeaf
            }
        );
        assert_eq!(sk.stats.skips_known_leaf, 1);
    }

    // 7. record_visit(was_leaf=true) → next decide returns KnownLeaf.
    #[test]
    fn record_leaf_then_decide_returns_known_leaf() {
        let mut sk = SkipperState::new();
        let c = co("ex", "leaf");
        sk.record_visit(c.clone(), ExclusionSet::new(), true);
        assert!(sk.known_leaves().contains(&c));
        let d = sk.decide(&c, 1, &ExclusionSet::new(), &HashMap::new());
        assert!(matches!(
            d,
            SkipDecision::Skip {
                reason: SkipReason::KnownLeaf
            }
        ));
    }

    // 8. record_visit(was_leaf=false) → no entry in known_leaves.
    #[test]
    fn record_non_leaf_does_not_cache_as_leaf() {
        let mut sk = SkipperState::new();
        let c = co("ex", "branch");
        sk.record_visit(c.clone(), ExclusionSet::new(), false);
        assert!(!sk.known_leaves().contains(&c));
    }

    // 9. is_superset_of: empty ⊇ empty.
    #[test]
    fn superset_empty_empty() {
        let a = ExclusionSet::new();
        let b = ExclusionSet::new();
        assert!(a.is_superset_of(&b));
    }

    // 10. Empty is NOT superset of non-empty.
    #[test]
    fn superset_empty_not_of_non_empty() {
        let a = ExclusionSet::new();
        let b = excl_set(&[("g", "a")]);
        assert!(!a.is_superset_of(&b));
    }

    // 11. {*:*} ⊇ any pattern.
    #[test]
    fn superset_star_star_dominates() {
        let a = excl_set(&[("*", "*")]);
        let b = excl_set(&[("g", "a"), ("h", "b"), ("*", "c")]);
        assert!(a.is_superset_of(&b));
    }

    // 12. {g:*} ⊇ {g:a}.
    #[test]
    fn superset_group_wildcard_dominates_specific_artifact() {
        let a = excl_set(&[("g", "*")]);
        let b = excl_set(&[("g", "a")]);
        assert!(a.is_superset_of(&b));
    }

    // 13. {g:a} ⊉ {g:*} (specific does not dominate wildcard).
    #[test]
    fn superset_specific_does_not_dominate_wildcard() {
        let a = excl_set(&[("g", "a")]);
        let b = excl_set(&[("g", "*")]);
        assert!(!a.is_superset_of(&b));
    }

    // 14. Stats: walks tick on Walk.
    #[test]
    fn stats_walks_tick() {
        let mut sk = SkipperState::new();
        for i in 0..5 {
            sk.decide(
                &co("ex", &format!("A{i}")),
                1,
                &ExclusionSet::new(),
                &HashMap::new(),
            );
        }
        assert_eq!(sk.stats.walks, 5);
        assert_eq!(sk.stats.total_skips(), 0);
        assert_eq!(sk.stats.total_decisions(), 5);
        assert_eq!(sk.stats.skip_rate(), 0.0);
    }

    // 15. Stats: skip_rate is meaningful.
    #[test]
    fn stats_skip_rate() {
        let mut sk = SkipperState::new();
        let c = co("ex", "A");
        sk.record_visit(c.clone(), ExclusionSet::new(), true);
        // 3 skips, 1 walk.
        for _ in 0..3 {
            sk.decide(&c, 5, &ExclusionSet::new(), &HashMap::new());
        }
        sk.decide(&co("ex", "B"), 1, &ExclusionSet::new(), &HashMap::new());
        assert_eq!(sk.stats.skips_known_leaf, 3);
        assert_eq!(sk.stats.walks, 1);
        assert!((sk.stats.skip_rate() - 0.75).abs() < 1e-9);
    }

    // 16. Disabled skipper: always returns Walk.
    #[test]
    fn disabled_always_walks() {
        let mut sk = SkipperState::disabled();
        let c = co("ex", "A");
        sk.record_visit(c.clone(), ExclusionSet::new(), true);
        // Even though we just recorded a leaf, disabled skipper walks.
        let winners = winners_with(&[(c.clone(), 1)]);
        let d = sk.decide(&c, 5, &ExclusionSet::new(), &winners);
        assert_eq!(d, SkipDecision::Walk);
        assert_eq!(sk.stats.walks, 1);
        assert_eq!(sk.stats.total_skips(), 0);
    }

    // 17. Disabled skipper: record_visit does not populate state.
    #[test]
    fn disabled_record_is_noop() {
        let mut sk = SkipperState::disabled();
        sk.record_visit(co("ex", "A"), ExclusionSet::new(), true);
        assert!(sk.known_leaves().is_empty());
        assert!(sk.winners_exclusions().is_empty());
    }

    // 18. ExclusionPattern::new normalises empty -> "*".
    #[test]
    fn exclusion_pattern_normalises_empty() {
        let p = ExclusionPattern::new("", "");
        assert_eq!(p.group, "*");
        assert_eq!(p.artifact, "*");
    }

    // 19. ExclusionPattern::matches with wildcards.
    #[test]
    fn exclusion_pattern_matches() {
        let p = pat("org.foo", "*");
        assert!(p.matches(&co("org.foo", "anything")));
        assert!(!p.matches(&co("org.bar", "anything")));
        let p2 = pat("*", "*");
        assert!(p2.matches(&co("any", "thing")));
    }

    // 20. ExclusionSet::from_raw round-trips through RawExclusion.
    #[test]
    fn exclusion_set_from_raw() {
        let raw = vec![
            RawExclusion {
                group_id: "org.foo".into(),
                artifact_id: "bar".into(),
            },
            RawExclusion {
                group_id: "*".into(),
                artifact_id: "evil".into(),
            },
        ];
        let s = ExclusionSet::from_raw(&raw);
        assert_eq!(s.len(), 2);
        assert!(s.matches_any(&co("org.foo", "bar")));
        assert!(s.matches_any(&co("anywhere", "evil")));
        assert!(!s.matches_any(&co("anywhere", "innocent")));
    }

    // 21. ExclusionSet::matches_any with empty -> false.
    #[test]
    fn exclusion_set_empty_matches_nothing() {
        let s = ExclusionSet::new();
        assert!(!s.matches_any(&co("g", "a")));
    }

    // 22. is_superset_of with disjoint sets.
    #[test]
    fn superset_disjoint_is_false() {
        let a = excl_set(&[("g1", "a1")]);
        let b = excl_set(&[("g2", "a2")]);
        assert!(!a.is_superset_of(&b));
        assert!(!b.is_superset_of(&a));
    }

    // 23. Multiple coords, mixed decisions.
    #[test]
    fn multiple_coords_mixed_decisions() {
        let mut sk = SkipperState::new();
        let a = co("ex", "A");
        let b = co("ex", "B");
        let c = co("ex", "C");

        // A is a known leaf; B is a non-leaf at depth 1; C is unknown.
        sk.record_visit(a.clone(), ExclusionSet::new(), true);
        sk.record_visit(b.clone(), ExclusionSet::new(), false);
        let winners = winners_with(&[(a.clone(), 1), (b.clone(), 1)]);

        assert!(matches!(
            sk.decide(&a, 3, &ExclusionSet::new(), &winners),
            SkipDecision::Skip {
                reason: SkipReason::KnownLeaf
            }
        ));
        assert!(matches!(
            sk.decide(&b, 3, &ExclusionSet::new(), &winners),
            SkipDecision::Skip {
                reason: SkipReason::AlreadyResolvedShallowerWithCompatibleExclusions { .. }
            }
        ));
        assert_eq!(
            sk.decide(&c, 3, &ExclusionSet::new(), &winners),
            SkipDecision::Walk
        );
    }

    // 24. winning_depth & new_depth carried through SkipReason.
    #[test]
    fn skip_reason_carries_depths() {
        let mut sk = SkipperState::new();
        let c = co("ex", "A");
        let winners = winners_with(&[(c.clone(), 2)]);
        sk.record_visit(c.clone(), ExclusionSet::new(), false);
        let d = sk.decide(&c, 7, &ExclusionSet::new(), &winners);
        match d {
            SkipDecision::Skip {
                reason:
                    SkipReason::AlreadyResolvedShallowerWithCompatibleExclusions {
                        winning_depth,
                        new_depth,
                    },
            } => {
                assert_eq!(winning_depth, 2);
                assert_eq!(new_depth, 7);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // 25. Re-inserting the same pattern is idempotent.
    #[test]
    fn exclusion_set_insert_idempotent() {
        let mut s = ExclusionSet::new();
        s.insert(pat("g", "a"));
        s.insert(pat("g", "a"));
        assert_eq!(s.len(), 1);
    }
}
