//! Strict-mode derivation formatter.
//!
//! Turns a [`StrictDerivation`] (built from PubGrub's conflict
//! report) into a human-readable "why no solution exists" message.
//! The shape mirrors `mvn dependency:tree` output so users already
//! comfortable with Maven diagnostics can read it without ramping
//! up.
//!
//! Output is plain text — no ANSI colors. Color belongs in the CLI
//! terminal handler, not here.
//!
//! Example output:
//!
//! ```text
//! error: cannot resolve dependencies — conflicting requirements on `org.example:lib`.
//!
//!   conflicting requirements on `org.example:lib`:
//!     org.example:a:1.0.0   requires  org.example:lib  [1.0]   (available: 1.0, 2.0)
//!     org.example:b:1.0.0   requires  org.example:lib  [2.0]   (available: 1.0, 2.0)
//!
//!   no version of `org.example:lib` satisfies both `[1.0]` and `[2.0]`.
//!
//! hints:
//!   • Override a transitive's pin by adding your project's own
//!     <dependencyManagement> entry for the conflicting coord.
//!   • Or run without --strict to fall back to nearest-wins
//!     resolution (the default; loses the conflict signal).
//! ```

use std::collections::BTreeMap;
use std::fmt;

use crate::strict::{DepEdge, StrictDerivation};

/// Render a [`StrictDerivation`] as a multi-line error message.
///
/// The string is `\n`-terminated for each line and includes a
/// trailing "hints:" section. Callers can print it as-is.
pub fn format_derivation(d: &StrictDerivation) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "error: cannot resolve dependencies — {}.\n\n",
        d.root_cause
    ));

    if d.contributing_edges.is_empty() {
        out.push_str("  (no contributing edges were recorded by the solver.)\n\n");
    } else {
        // Group edges by `to_coords` so multi-requirement conflicts
        // on one coord render together. Stable ordering via BTreeMap.
        let groups = group_by_target(&d.contributing_edges);

        for (target, edges) in &groups {
            out.push_str(&format!("  conflicting requirements on `{target}`:\n"));
            for edge in edges {
                let from = render_from(edge);
                let range_maven = pubgrub_range_to_maven(&edge.required_range);
                let available = if edge.available_versions.is_empty() {
                    String::new()
                } else {
                    format!("   (available: {})", edge.available_versions.join(", "))
                };
                out.push_str(&format!(
                    "    {from}   requires  {target}  {range_maven}{available}\n",
                ));
            }
            out.push('\n');
        }

        // For each group with ≥2 edges, emit a one-line "no version
        // satisfies all of these ranges" summary.
        for (target, edges) in &groups {
            if edges.len() >= 2 {
                let ranges: Vec<String> = edges
                    .iter()
                    .map(|e| format!("`{}`", pubgrub_range_to_maven(&e.required_range)))
                    .collect();
                out.push_str(&format!(
                    "  no version of `{target}` satisfies {}.\n\n",
                    join_with_and(&ranges),
                ));
            }
        }
    }

    out.push_str("hints:\n");
    out.push_str("  • Override a transitive's pin by adding your project's own\n");
    out.push_str("    <dependencyManagement> entry for the conflicting coord.\n");
    out.push_str("  • Or run without --strict to fall back to nearest-wins\n");
    out.push_str("    resolution (the default; loses the conflict signal).\n");

    out
}

/// Group edges by their `to_coords` (printed as `group:artifact`).
fn group_by_target(edges: &[DepEdge]) -> Vec<(String, Vec<&DepEdge>)> {
    let mut map: BTreeMap<String, Vec<&DepEdge>> = BTreeMap::new();
    for e in edges {
        let key = format!("{}:{}", e.to_coords.group, e.to_coords.artifact);
        map.entry(key).or_default().push(e);
    }
    map.into_iter().collect()
}

/// Render the "from" side of a dep edge.
///
/// The synthetic root edge — where `from_coords.group == "<root>"`
/// — is rendered as the literal phrase `root project`.
fn render_from(edge: &DepEdge) -> String {
    if edge.from_coords.group == "<root>" {
        "root project".to_string()
    } else {
        format!(
            "{}:{}:{}",
            edge.from_coords.group, edge.from_coords.artifact, edge.from_version
        )
    }
}

/// Translate PubGrub's range `Display` into Maven range syntax.
///
/// PubGrub emits things like `>=1.0, <2.0` or `1.0`; Maven uses
/// `[1.0,2.0)` or `[1.0]`. Best-effort translation; falls back to
/// passthrough when the input doesn't match a known shape (e.g.
/// the input is already Maven syntax, or it's `*` / `∅`).
pub fn pubgrub_range_to_maven(pg: &str) -> String {
    let pg = pg.trim();

    if pg.is_empty() {
        return String::new();
    }

    // Already Maven syntax — pass through.
    if pg.starts_with('[') || pg.starts_with('(') {
        return pg.to_string();
    }

    // Single bare version: "1.0" → "[1.0]".
    if !pg.contains(',') && !pg.contains('=') && !pg.contains('<') && !pg.contains('>') {
        return format!("[{pg}]");
    }

    // Compound: ">=1.0, <2.0" → "[1.0,2.0)".
    if let Some((lo, hi)) = pg.split_once(',') {
        let lo = lo.trim();
        let hi = hi.trim();
        let (lo_kind, lo_v) = parse_bound(lo);
        let (hi_kind, hi_v) = parse_bound(hi);
        if let (Some(lv), Some(hv)) = (lo_v, hi_v) {
            return format!(
                "{}{lv},{hv}{}",
                bound_bracket(lo_kind, true),
                bound_bracket(hi_kind, false),
            );
        }
    }

    // Open-ended: ">=1.0" → "[1.0,)" etc. Order matters: check
    // two-char prefixes before one-char prefixes.
    if let Some(rest) = pg.strip_prefix(">=") {
        return format!("[{},)", rest.trim());
    }
    if let Some(rest) = pg.strip_prefix("<=") {
        return format!("(,{}]", rest.trim());
    }
    if let Some(rest) = pg.strip_prefix('>') {
        return format!("({},)", rest.trim());
    }
    if let Some(rest) = pg.strip_prefix('<') {
        return format!("(,{})", rest.trim());
    }

    // Unknown shape — pass through unchanged.
    pg.to_string()
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum BoundKind {
    Inclusive,
    Exclusive,
}

fn parse_bound(s: &str) -> (BoundKind, Option<&str>) {
    if let Some(v) = s.strip_prefix(">=") {
        (BoundKind::Inclusive, Some(v.trim()))
    } else if let Some(v) = s.strip_prefix("<=") {
        (BoundKind::Inclusive, Some(v.trim()))
    } else if let Some(v) = s.strip_prefix('>') {
        (BoundKind::Exclusive, Some(v.trim()))
    } else if let Some(v) = s.strip_prefix('<') {
        (BoundKind::Exclusive, Some(v.trim()))
    } else {
        (BoundKind::Inclusive, None)
    }
}

fn bound_bracket(b: BoundKind, lower: bool) -> char {
    match (b, lower) {
        (BoundKind::Inclusive, true) => '[',
        (BoundKind::Inclusive, false) => ']',
        (BoundKind::Exclusive, true) => '(',
        (BoundKind::Exclusive, false) => ')',
    }
}

/// Join a slice of strings with English conjunctions.
///
/// - `[]` → `""`
/// - `["a"]` → `"a"`
/// - `["a", "b"]` → `"a" and "b"` (no Oxford comma for a pair)
/// - `["a", "b", "c"]` → `"a, b, and c"` (Oxford comma for ≥3)
fn join_with_and(items: &[String]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        2 => format!("{} and {}", items[0], items[1]),
        _ => {
            let head = &items[..items.len() - 1];
            let tail = &items[items.len() - 1];
            format!("{}, and {}", head.join(", "), tail)
        }
    }
}

impl fmt::Display for StrictDerivation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_derivation(self))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use barista_coords::Coords;

    fn co(group: &str, artifact: &str) -> Coords {
        Coords::new(group, artifact).expect("valid coords")
    }

    fn root_co() -> Coords {
        // The synthetic root uses literal `<root>` per T1's
        // contract. Coords::new accepts arbitrary non-empty strings.
        Coords::new("<root>", "<root>").expect("valid coords")
    }

    fn edge(
        from: Coords,
        from_v: &str,
        to: Coords,
        range: &str,
        available: &[&str],
    ) -> DepEdge {
        DepEdge {
            from_coords: from,
            from_version: from_v.to_string(),
            to_coords: to,
            required_range: range.to_string(),
            available_versions: available.iter().map(|s| s.to_string()).collect(),
        }
    }

    // 1. Empty derivation → sensible "no detail" message.
    #[test]
    fn empty_derivation_produces_no_detail_message() {
        let d = StrictDerivation {
            root_cause: "no candidates found".to_string(),
            contributing_edges: vec![],
        };
        let s = format_derivation(&d);
        assert!(s.contains("error: cannot resolve dependencies"));
        assert!(s.contains("no candidates found"));
        assert!(s.contains("no contributing edges were recorded"));
        assert!(s.contains("hints:"));
    }

    // 2. Single-edge conflict renders from/to/range/available.
    #[test]
    fn single_edge_renders_all_fields() {
        let d = StrictDerivation {
            root_cause: "conflict on org.example:lib".to_string(),
            contributing_edges: vec![edge(
                co("org.example", "a"),
                "1.0.0",
                co("org.example", "lib"),
                "1.0",
                &["1.0", "2.0"],
            )],
        };
        let s = format_derivation(&d);
        assert!(s.contains("org.example:a:1.0.0"));
        assert!(s.contains("org.example:lib"));
        assert!(s.contains("[1.0]"));
        assert!(s.contains("(available: 1.0, 2.0)"));
    }

    // 3. Multi-edge conflict on the same target groups together.
    #[test]
    fn multi_edge_same_target_groups_together() {
        let d = StrictDerivation {
            root_cause: "conflict on org.example:lib".to_string(),
            contributing_edges: vec![
                edge(
                    co("org.example", "a"),
                    "1.0.0",
                    co("org.example", "lib"),
                    "1.0",
                    &["1.0", "2.0"],
                ),
                edge(
                    co("org.example", "b"),
                    "1.0.0",
                    co("org.example", "lib"),
                    "2.0",
                    &["1.0", "2.0"],
                ),
            ],
        };
        let s = format_derivation(&d);
        // Both edges appear under a single "conflicting requirements
        // on `org.example:lib`" header.
        let header_count = s.matches("conflicting requirements on `org.example:lib`").count();
        assert_eq!(header_count, 1);
        assert!(s.contains("org.example:a:1.0.0"));
        assert!(s.contains("org.example:b:1.0.0"));
        // And the "no version satisfies" summary appears for that target.
        assert!(s.contains("no version of `org.example:lib` satisfies"));
    }

    // 4. Multi-edge conflict on TWO different targets shows two groups.
    #[test]
    fn conflicts_on_two_different_targets_render_two_groups() {
        let d = StrictDerivation {
            root_cause: "multiple conflicts".to_string(),
            contributing_edges: vec![
                edge(
                    co("org.example", "a"),
                    "1.0.0",
                    co("org.example", "libx"),
                    "1.0",
                    &["1.0"],
                ),
                edge(
                    co("org.example", "b"),
                    "1.0.0",
                    co("org.example", "libx"),
                    "2.0",
                    &["1.0"],
                ),
                edge(
                    co("org.example", "c"),
                    "1.0.0",
                    co("org.example", "liby"),
                    "1.0",
                    &["1.0"],
                ),
                edge(
                    co("org.example", "d"),
                    "1.0.0",
                    co("org.example", "liby"),
                    "2.0",
                    &["1.0"],
                ),
            ],
        };
        let s = format_derivation(&d);
        assert!(s.contains("conflicting requirements on `org.example:libx`"));
        assert!(s.contains("conflicting requirements on `org.example:liby`"));
        assert!(s.contains("no version of `org.example:libx` satisfies"));
        assert!(s.contains("no version of `org.example:liby` satisfies"));
    }

    // 5. Root edges render as "root project".
    #[test]
    fn root_edge_renders_as_root_project() {
        let d = StrictDerivation {
            root_cause: "root needs lib".to_string(),
            contributing_edges: vec![edge(
                root_co(),
                "0.0.0",
                co("org.example", "lib"),
                "1.0",
                &["1.0"],
            )],
        };
        let s = format_derivation(&d);
        assert!(s.contains("root project"));
        assert!(!s.contains("<root>:<root>:0.0.0"));
    }

    // 6. Available versions list rendered (comma-separated).
    #[test]
    fn available_versions_rendered_comma_separated() {
        let d = StrictDerivation {
            root_cause: "x".to_string(),
            contributing_edges: vec![edge(
                co("org.example", "a"),
                "1.0",
                co("org.example", "lib"),
                "1.0",
                &["1.0", "1.5", "2.0", "2.5"],
            )],
        };
        let s = format_derivation(&d);
        assert!(s.contains("(available: 1.0, 1.5, 2.0, 2.5)"));
    }

    // 6b. No available versions: the "(available: ...)" segment is omitted.
    #[test]
    fn empty_available_versions_omits_segment() {
        let d = StrictDerivation {
            root_cause: "x".to_string(),
            contributing_edges: vec![edge(
                co("org.example", "a"),
                "1.0",
                co("org.example", "lib"),
                "1.0",
                &[],
            )],
        };
        let s = format_derivation(&d);
        assert!(!s.contains("available:"));
    }

    // 7. pubgrub_range_to_maven("1.0") → "[1.0]".
    #[test]
    fn maven_range_single_version() {
        assert_eq!(pubgrub_range_to_maven("1.0"), "[1.0]");
        assert_eq!(pubgrub_range_to_maven("2.1.3"), "[2.1.3]");
    }

    // 8. ">=1.0, <2.0" → "[1.0,2.0)".
    #[test]
    fn maven_range_inclusive_exclusive() {
        assert_eq!(pubgrub_range_to_maven(">=1.0, <2.0"), "[1.0,2.0)");
    }

    // 9. ">1.0, <=2.0" → "(1.0,2.0]".
    #[test]
    fn maven_range_exclusive_inclusive() {
        assert_eq!(pubgrub_range_to_maven(">1.0, <=2.0"), "(1.0,2.0]");
    }

    // 10. ">=1.0" → "[1.0,)".
    #[test]
    fn maven_range_open_upper() {
        assert_eq!(pubgrub_range_to_maven(">=1.0"), "[1.0,)");
        assert_eq!(pubgrub_range_to_maven(">1.0"), "(1.0,)");
        assert_eq!(pubgrub_range_to_maven("<=2.0"), "(,2.0]");
        assert_eq!(pubgrub_range_to_maven("<2.0"), "(,2.0)");
    }

    // 11. "(,1.0]" (already Maven) passes through.
    #[test]
    fn maven_range_passthrough_when_already_maven() {
        assert_eq!(pubgrub_range_to_maven("(,1.0]"), "(,1.0]");
        assert_eq!(pubgrub_range_to_maven("[1.0,2.0)"), "[1.0,2.0)");
        assert_eq!(pubgrub_range_to_maven("[1.0]"), "[1.0]");
    }

    // 12. join_with_and handles 0/1/2/3 items correctly.
    #[test]
    fn join_with_and_handles_all_arities() {
        let zero: Vec<String> = vec![];
        assert_eq!(join_with_and(&zero), "");
        assert_eq!(join_with_and(&["a".into()]), "a");
        assert_eq!(join_with_and(&["a".into(), "b".into()]), "a and b");
        assert_eq!(
            join_with_and(&["a".into(), "b".into(), "c".into()]),
            "a, b, and c"
        );
        assert_eq!(
            join_with_and(&["a".into(), "b".into(), "c".into(), "d".into()]),
            "a, b, c, and d"
        );
    }

    // 13. Hints section always appended.
    #[test]
    fn hints_section_always_present() {
        let empty = StrictDerivation {
            root_cause: "x".to_string(),
            contributing_edges: vec![],
        };
        assert!(format_derivation(&empty).contains("hints:"));

        let with_edge = StrictDerivation {
            root_cause: "x".to_string(),
            contributing_edges: vec![edge(
                co("g", "a"),
                "1",
                co("g", "b"),
                "1",
                &["1"],
            )],
        };
        let s = format_derivation(&with_edge);
        assert!(s.contains("hints:"));
        assert!(s.contains("<dependencyManagement>"));
        assert!(s.contains("--strict"));
    }

    // 14. Display impl matches format_derivation.
    #[test]
    fn display_impl_matches_format_derivation() {
        let d = StrictDerivation {
            root_cause: "conflict on org.example:lib".to_string(),
            contributing_edges: vec![edge(
                co("org.example", "a"),
                "1.0.0",
                co("org.example", "lib"),
                "1.0",
                &["1.0", "2.0"],
            )],
        };
        assert_eq!(format!("{d}"), format_derivation(&d));
    }

    // Bonus: single edge on a target doesn't emit the "no version
    // satisfies" summary (that summary only fires for ≥2 edges).
    #[test]
    fn single_edge_skips_no_version_summary() {
        let d = StrictDerivation {
            root_cause: "x".to_string(),
            contributing_edges: vec![edge(
                co("g", "a"),
                "1",
                co("g", "b"),
                "1",
                &["1"],
            )],
        };
        let s = format_derivation(&d);
        assert!(!s.contains("no version of"));
    }

    // Bonus: the full sample output shape is stable. Useful as a
    // visual snapshot during human review (T4 will replace this
    // with the conflict-fixture suite).
    #[test]
    fn sample_output_is_stable() {
        let d = StrictDerivation {
            root_cause: "conflicting requirements on `org.example:lib`".to_string(),
            contributing_edges: vec![
                edge(
                    co("org.example", "a"),
                    "1.0.0",
                    co("org.example", "lib"),
                    "1.0",
                    &["1.0", "2.0"],
                ),
                edge(
                    co("org.example", "b"),
                    "1.0.0",
                    co("org.example", "lib"),
                    "2.0",
                    &["1.0", "2.0"],
                ),
            ],
        };
        let s = format_derivation(&d);
        // Lightweight structural assertions; exact whitespace is
        // tested implicitly through the other tests.
        assert!(s.starts_with("error: cannot resolve dependencies"));
        assert!(s.contains("conflicting requirements on `org.example:lib`:"));
        assert!(s.contains("[1.0]"));
        assert!(s.contains("[2.0]"));
        assert!(s.contains("no version of `org.example:lib` satisfies `[1.0]` and `[2.0]`."));
        assert!(s.trim_end().ends_with("loses the conflict signal)."));
    }
}
