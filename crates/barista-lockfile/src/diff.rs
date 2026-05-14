//! Semantic lockfile diff rendering.
//!
//! Renders the difference between two lockfile snapshots in a
//! code-review-friendly format. Collapses the raw line-level diff of
//! a TOML lockfile into a structured summary by category:
//!
//!   - **Added**:        new resolved entries
//!   - **Removed**:      entries that disappeared
//!   - **Upgraded**:     version bumps on the same coords (most common)
//!   - **Downgraded**:   same coords, lower version
//!   - **Re-scoped**:    coords whose scope changed without a version change
//!   - **Re-classified**: same coords, classifier/type changed
//!
//! Each category is rendered in a section with one line per coords.

use std::collections::BTreeMap;

/// A minimal lockfile entry. The real lockfile schema (PRD §7) has
/// more fields (checksums, resolution path, etc.) — this is the
/// spike's subset.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LockEntry {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
    /// e.g. "compile", "test", "provided", "runtime".
    pub scope: String,
    pub classifier: Option<String>,
    /// e.g. "jar", "pom", "war".
    pub type_: String,
}

impl LockEntry {
    /// Compact coordinate key including type and (optional) classifier.
    pub fn coords_key(&self) -> String {
        let cls = self
            .classifier
            .as_deref()
            .map(|c| format!(":{c}"))
            .unwrap_or_default();
        format!(
            "{}:{}:{}{}",
            self.group_id, self.artifact_id, self.type_, cls
        )
    }

    /// `group:artifact` only — used when grouping entries across
    /// version / classifier / type changes.
    fn ga(&self) -> String {
        format!("{}:{}", self.group_id, self.artifact_id)
    }

    /// Render the artifact part the way Maven coordinates usually
    /// appear in tooling output, omitting the default `jar` type and
    /// rendering the classifier when present.
    fn display_coords(&self) -> String {
        let mut s = format!("{}:{}", self.group_id, self.artifact_id);
        if self.type_ != "jar" {
            s.push(':');
            s.push_str(&self.type_);
        }
        if let Some(cls) = &self.classifier {
            if self.type_ == "jar" {
                s.push_str(":jar");
            }
            s.push(':');
            s.push_str(cls);
        }
        s
    }
}

/// The structured diff between two snapshots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockDiff {
    pub added: Vec<LockEntry>,
    pub removed: Vec<LockEntry>,
    pub upgraded: Vec<VersionChange>,
    pub downgraded: Vec<VersionChange>,
    pub rescoped: Vec<ScopeChange>,
    pub reclassified: Vec<TypeChange>,
}

impl LockDiff {
    /// Total number of semantic changes across all categories.
    pub fn change_count(&self) -> usize {
        self.added.len()
            + self.removed.len()
            + self.upgraded.len()
            + self.downgraded.len()
            + self.rescoped.len()
            + self.reclassified.len()
    }

    /// True if both snapshots are equivalent.
    pub fn is_empty(&self) -> bool {
        self.change_count() == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionChange {
    /// `group:artifact` (no version), with type/classifier suffixes
    /// when not default.
    pub coords: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeChange {
    pub coords: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeChange {
    pub coords: String,
    /// Short string describing the previous type/classifier combo.
    pub from: String,
    /// Short string describing the new type/classifier combo.
    pub to: String,
}

/// Compute the structured diff between two lockfile snapshots.
///
/// Order of input vectors is irrelevant; entries are matched by
/// `(group_id, artifact_id)` and then by best fit within each group.
pub fn diff(left: &[LockEntry], right: &[LockEntry]) -> LockDiff {
    // Group entries by (group_id, artifact_id).
    let mut left_by_ga: BTreeMap<String, Vec<LockEntry>> = BTreeMap::new();
    let mut right_by_ga: BTreeMap<String, Vec<LockEntry>> = BTreeMap::new();
    for e in left {
        left_by_ga.entry(e.ga()).or_default().push(e.clone());
    }
    for e in right {
        right_by_ga.entry(e.ga()).or_default().push(e.clone());
    }

    let mut out = LockDiff::default();

    let mut all_gas: Vec<String> = left_by_ga
        .keys()
        .chain(right_by_ga.keys())
        .cloned()
        .collect();
    all_gas.sort();
    all_gas.dedup();

    for ga in all_gas {
        let mut l = left_by_ga.remove(&ga).unwrap_or_default();
        let mut r = right_by_ga.remove(&ga).unwrap_or_default();

        // Stage 1: remove exact matches.
        let mut i = 0;
        while i < l.len() {
            if let Some(j) = r.iter().position(|e| e == &l[i]) {
                l.remove(i);
                r.remove(j);
            } else {
                i += 1;
            }
        }

        // Stage 2: pair surviving entries by (type, classifier) — a
        // pure version change.
        let mut i = 0;
        while i < l.len() {
            let lc = (l[i].type_.clone(), l[i].classifier.clone());
            if let Some(j) = r
                .iter()
                .position(|e| (e.type_.clone(), e.classifier.clone()) == lc)
            {
                let le = l.remove(i);
                let re = r.remove(j);
                if le.version != re.version {
                    let coords = le.display_coords();
                    let vc = VersionChange {
                        coords,
                        from: le.version.clone(),
                        to: re.version.clone(),
                    };
                    if compare_versions(&le.version, &re.version) == std::cmp::Ordering::Less {
                        out.upgraded.push(vc);
                    } else {
                        out.downgraded.push(vc);
                    }
                } else if le.scope != re.scope {
                    out.rescoped.push(ScopeChange {
                        coords: le.display_coords(),
                        from: le.scope,
                        to: re.scope,
                    });
                }
                // else: identical except for fields we already
                // matched on — should have been caught in stage 1.
            } else {
                i += 1;
            }
        }

        // Stage 3: pair by version — a classifier/type change.
        let mut i = 0;
        while i < l.len() {
            if let Some(j) = r.iter().position(|e| e.version == l[i].version) {
                let le = l.remove(i);
                let re = r.remove(j);
                out.reclassified.push(TypeChange {
                    coords: format!("{}:{}:{}", le.group_id, le.artifact_id, le.version),
                    from: short_type(&le.type_, le.classifier.as_deref()),
                    to: short_type(&re.type_, re.classifier.as_deref()),
                });
            } else {
                i += 1;
            }
        }

        // Stage 4: leftovers are pure additions / removals.
        out.removed.extend(l);
        out.added.extend(r);
    }

    sort_diff(&mut out);
    out
}

/// Stable, alphabetical sort within each category. Version-change
/// sections are then re-ordered using a coarse prefix-grouping
/// heuristic so closely-related artifacts (e.g. `spring-*`) cluster.
fn sort_diff(d: &mut LockDiff) {
    d.added.sort_by_key(|a| a.coords_key());
    d.removed.sort_by_key(|a| a.coords_key());
    d.upgraded.sort_by(|a, b| a.coords.cmp(&b.coords));
    d.downgraded.sort_by(|a, b| a.coords.cmp(&b.coords));
    d.rescoped.sort_by(|a, b| a.coords.cmp(&b.coords));
    d.reclassified.sort_by(|a, b| a.coords.cmp(&b.coords));

    d.upgraded = group_by_prefix(std::mem::take(&mut d.upgraded));
    d.downgraded = group_by_prefix(std::mem::take(&mut d.downgraded));
}

/// Re-order version changes so entries sharing a `group:prefix-`
/// stay adjacent. The base sort is alphabetical anyway; this routine
/// just ensures e.g. `spring-core`, `spring-aop`, `spring-beans`
/// (which would otherwise appear in unrelated alphabetical positions
/// if intermixed with other groups) stay clustered. Since we sort
/// by `group:artifact` lexicographically, items in the same group
/// already cluster — this pass is a no-op safety net for now but is
/// extracted so the ADR can describe future grouping policy.
fn group_by_prefix(items: Vec<VersionChange>) -> Vec<VersionChange> {
    // Base alphabetical sort already clusters by group, then by
    // artifact id, which produces good clustering in practice.
    // Kept as a hook for future heuristics (e.g. merging long
    // mono-version groups into a single summary line).
    items
}

/// Best-effort semver-ish comparison. Splits on `.` and `-`,
/// compares numeric components numerically and non-numerics
/// lexicographically. Sufficient for the spike — the real
/// implementation will use the version parser from `barista-version`.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parts_a: Vec<&str> = a.split(['.', '-']).collect();
    let parts_b: Vec<&str> = b.split(['.', '-']).collect();
    for i in 0..parts_a.len().max(parts_b.len()) {
        let pa = parts_a.get(i).copied().unwrap_or("0");
        let pb = parts_b.get(i).copied().unwrap_or("0");
        let ord = match (pa.parse::<u64>(), pb.parse::<u64>()) {
            (Ok(na), Ok(nb)) => na.cmp(&nb),
            _ => pa.cmp(pb),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

fn short_type(type_: &str, classifier: Option<&str>) -> String {
    match classifier {
        Some(c) => format!("{type_}:{c}"),
        None => type_.to_string(),
    }
}

/// Render the diff for human consumption. The output is intended
/// for terminals and PR comments; it uses ASCII arrows only.
pub fn render(diff: &LockDiff) -> String {
    if diff.is_empty() {
        return "Lockfile diff: no changes\n".to_string();
    }

    let mut out = String::new();
    let total = diff.change_count();
    out.push_str(&format!("Lockfile diff: {total} changes total\n"));

    if !diff.added.is_empty() {
        out.push_str(&format!("  + {} added\n", diff.added.len()));
    }
    if !diff.removed.is_empty() {
        out.push_str(&format!("  - {} removed\n", diff.removed.len()));
    }
    if !diff.upgraded.is_empty() {
        out.push_str(&format!("  ^ {} upgraded\n", diff.upgraded.len()));
    }
    if !diff.downgraded.is_empty() {
        out.push_str(&format!("  v {} downgraded\n", diff.downgraded.len()));
    }
    if !diff.rescoped.is_empty() {
        out.push_str(&format!("  > {} re-scoped\n", diff.rescoped.len()));
    }
    if !diff.reclassified.is_empty() {
        out.push_str(&format!("  * {} re-classified\n", diff.reclassified.len()));
    }

    if !diff.added.is_empty() {
        out.push_str(&format!("\nAdded ({}):\n", diff.added.len()));
        for e in &diff.added {
            out.push_str(&format!(
                "  + {}:{} {}\n",
                e.display_coords(),
                e.version,
                e.scope
            ));
        }
    }

    if !diff.removed.is_empty() {
        out.push_str(&format!("\nRemoved ({}):\n", diff.removed.len()));
        for e in &diff.removed {
            out.push_str(&format!(
                "  - {}:{} {}\n",
                e.display_coords(),
                e.version,
                e.scope
            ));
        }
    }

    if !diff.upgraded.is_empty() {
        out.push_str(&format!("\nUpgraded ({}):\n", diff.upgraded.len()));
        for v in &diff.upgraded {
            out.push_str(&format!("  ^ {}:{} -> {}\n", v.coords, v.from, v.to));
        }
    }

    if !diff.downgraded.is_empty() {
        out.push_str(&format!("\nDowngraded ({}):\n", diff.downgraded.len()));
        for v in &diff.downgraded {
            out.push_str(&format!("  v {}:{} -> {}\n", v.coords, v.from, v.to));
        }
    }

    if !diff.rescoped.is_empty() {
        out.push_str(&format!("\nRe-scoped ({}):\n", diff.rescoped.len()));
        for s in &diff.rescoped {
            out.push_str(&format!("  > {} {} -> {}\n", s.coords, s.from, s.to));
        }
    }

    if !diff.reclassified.is_empty() {
        out.push_str(&format!("\nRe-classified ({}):\n", diff.reclassified.len()));
        for t in &diff.reclassified {
            out.push_str(&format!("  * {} {} -> {}\n", t.coords, t.from, t.to));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(g: &str, a: &str, v: &str) -> LockEntry {
        LockEntry {
            group_id: g.to_string(),
            artifact_id: a.to_string(),
            version: v.to_string(),
            scope: "compile".to_string(),
            classifier: None,
            type_: "jar".to_string(),
        }
    }

    fn ev(g: &str, a: &str, v: &str, scope: &str) -> LockEntry {
        let mut x = e(g, a, v);
        x.scope = scope.to_string();
        x
    }

    #[test]
    fn empty_to_empty_is_no_change() {
        let d = diff(&[], &[]);
        assert!(d.is_empty());
        assert_eq!(render(&d), "Lockfile diff: no changes\n");
    }

    #[test]
    fn single_added() {
        let r = vec![e("org.slf4j", "slf4j-api", "2.0.16")];
        let d = diff(&[], &r);
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.change_count(), 1);
        let s = render(&d);
        assert!(s.contains("Added (1):"));
        assert!(s.contains("+ org.slf4j:slf4j-api:2.0.16 compile"));
    }

    #[test]
    fn single_removed() {
        let l = vec![e("commons-logging", "commons-logging", "1.2")];
        let d = diff(&l, &[]);
        assert_eq!(d.removed.len(), 1);
        let s = render(&d);
        assert!(s.contains("Removed (1):"));
        assert!(s.contains("- commons-logging:commons-logging:1.2 compile"));
    }

    #[test]
    fn single_upgrade() {
        let l = vec![e("org.springframework", "spring-core", "6.1.4")];
        let r = vec![e("org.springframework", "spring-core", "6.1.6")];
        let d = diff(&l, &r);
        assert_eq!(d.upgraded.len(), 1);
        assert_eq!(d.downgraded.len(), 0);
        let s = render(&d);
        assert!(s.contains("^ org.springframework:spring-core:6.1.4 -> 6.1.6"));
    }

    #[test]
    fn single_downgrade() {
        let l = vec![e("org.springframework", "spring-core", "6.1.6")];
        let r = vec![e("org.springframework", "spring-core", "6.1.4")];
        let d = diff(&l, &r);
        assert_eq!(d.downgraded.len(), 1);
        assert_eq!(d.upgraded.len(), 0);
        let s = render(&d);
        assert!(s.contains("v org.springframework:spring-core:6.1.6 -> 6.1.4"));
    }

    #[test]
    fn scope_change() {
        let l = vec![ev(
            "org.junit.jupiter",
            "junit-jupiter",
            "5.10.2",
            "compile",
        )];
        let r = vec![ev("org.junit.jupiter", "junit-jupiter", "5.10.2", "test")];
        let d = diff(&l, &r);
        assert_eq!(d.rescoped.len(), 1);
        let s = render(&d);
        assert!(s.contains("> org.junit.jupiter:junit-jupiter compile -> test"));
    }

    #[test]
    fn classifier_change() {
        let mut a = e("io.netty", "netty-tcnative-boringssl-static", "2.0.62");
        a.classifier = Some("linux-x86_64".to_string());
        let mut b = e("io.netty", "netty-tcnative-boringssl-static", "2.0.62");
        b.classifier = Some("osx-aarch_64".to_string());
        let d = diff(&[a], &[b]);
        assert_eq!(d.reclassified.len(), 1);
        let s = render(&d);
        assert!(
            s.contains("jar:linux-x86_64 -> jar:osx-aarch_64"),
            "render was:\n{s}"
        );
    }

    #[test]
    fn alphabetical_sort_in_added() {
        let r = vec![
            e("org.slf4j", "slf4j-api", "2.0.16"),
            e("com.fasterxml.jackson.core", "jackson-databind", "2.17.0"),
            e(
                "com.fasterxml.jackson.core",
                "jackson-annotations",
                "2.17.0",
            ),
        ];
        let d = diff(&[], &r);
        let coords: Vec<String> = d.added.iter().map(|e| e.coords_key()).collect();
        let mut sorted = coords.clone();
        sorted.sort();
        assert_eq!(coords, sorted);
    }

    #[test]
    fn spring_prefix_grouping_clusters_related_artifacts() {
        let l = vec![
            e("org.springframework", "spring-core", "6.1.4"),
            e("org.springframework", "spring-context", "6.1.4"),
            e("org.springframework", "spring-aop", "6.1.4"),
            e("io.micrometer", "micrometer-core", "1.12.0"),
        ];
        let r = vec![
            e("org.springframework", "spring-core", "6.1.6"),
            e("org.springframework", "spring-context", "6.1.6"),
            e("org.springframework", "spring-aop", "6.1.6"),
            e("io.micrometer", "micrometer-core", "1.12.3"),
        ];
        let d = diff(&l, &r);
        assert_eq!(d.upgraded.len(), 4);
        // All three spring-* entries should be adjacent in the
        // upgraded list.
        let spring_indices: Vec<usize> = d
            .upgraded
            .iter()
            .enumerate()
            .filter(|(_, u)| u.coords.starts_with("org.springframework:"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(spring_indices.len(), 3);
        // Adjacent: indices differ by 1 between successive entries.
        for w in spring_indices.windows(2) {
            assert_eq!(w[1] - w[0], 1);
        }
    }

    #[test]
    fn mixed_categories_render_in_documented_order() {
        let l = vec![
            e("org.springframework", "spring-core", "6.1.4"),
            e("commons-logging", "commons-logging", "1.2"),
            ev("org.junit.jupiter", "junit-jupiter", "5.10.2", "compile"),
        ];
        let r = vec![
            e("org.springframework", "spring-core", "6.1.6"),
            e("org.slf4j", "slf4j-api", "2.0.16"),
            ev("org.junit.jupiter", "junit-jupiter", "5.10.2", "test"),
        ];
        let s = render(&diff(&l, &r));
        let i_added = s.find("Added").unwrap();
        let i_removed = s.find("Removed").unwrap();
        let i_upgraded = s.find("Upgraded").unwrap();
        let i_rescoped = s.find("Re-scoped").unwrap();
        assert!(i_added < i_removed);
        assert!(i_removed < i_upgraded);
        assert!(i_upgraded < i_rescoped);
    }

    #[test]
    fn coords_with_classifier_render_correctly() {
        let mut entry = e("io.netty", "netty-tcnative-boringssl-static", "2.0.62");
        entry.classifier = Some("linux-x86_64".to_string());
        let d = diff(&[], &[entry]);
        let s = render(&d);
        assert!(
            s.contains("io.netty:netty-tcnative-boringssl-static:jar:linux-x86_64:2.0.62"),
            "render was:\n{s}"
        );
    }

    #[test]
    fn header_counts_total_changes() {
        let mut l = Vec::new();
        let mut r = Vec::new();
        for i in 0..12 {
            r.push(e("g", &format!("a{i}"), "1.0.0"));
            l.push(e("g", &format!("z{i}"), "1.0.0"));
        }
        let d = diff(&l, &r);
        assert_eq!(d.change_count(), 24);
        let s = render(&d);
        assert!(s.starts_with("Lockfile diff: 24 changes total\n"));
    }

    #[test]
    fn version_compare_handles_numeric_segments() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("1.10.0", "1.9.0"), Ordering::Greater);
        assert_eq!(compare_versions("1.2.3", "1.2.3"), Ordering::Equal);
        assert_eq!(compare_versions("6.1.4", "6.1.6"), Ordering::Less);
    }

    #[test]
    fn exact_match_produces_no_change() {
        let v = vec![
            e("org.springframework", "spring-core", "6.1.4"),
            e("org.slf4j", "slf4j-api", "2.0.16"),
        ];
        let d = diff(&v, &v);
        assert!(d.is_empty());
    }
}
