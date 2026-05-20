// SPDX-License-Identifier: MIT OR Apache-2.0

//! Maven version requirement parsing.
//!
//! A version *requirement* is what a `<dependency><version>` declaration
//! produces. It is either:
//!
//! - A **soft hint** — a literal version string like `"1.2.3"`. The
//!   resolver treats this as a preference; conflicts resolve via
//!   nearest-wins.
//! - A **hard range** — bracketed syntax that the resolver MUST
//!   satisfy. Examples: `[1.0]`, `[1.0,)`, `(,1.0]`, `[1.0,2.0)`,
//!   `[1.0,2.0),[3.0,)` (union of intervals).
//! - A **meta-version** — `LATEST` or `RELEASE`. Requires consulting
//!   `maven-metadata.xml` (via [`crate::MetadataSource::fetch_metadata`])
//!   to resolve to a concrete version.
//!
//! # Semantics
//!
//! For a [`VersionSpec::Hard`] requirement, the resolver MUST pick a
//! version that lies in (at least) one of the listed intervals; if no
//! such version is published, resolution fails. For a
//! [`VersionSpec::Soft`] requirement the resolver treats the version
//! as a hint — conflicting soft requirements from different graph
//! paths are reconciled by the nearest-wins rule and the resulting
//! version need not equal the declared one.
//!
//! `LATEST` and `RELEASE` are Maven meta-versions that were
//! deprecated in Maven 4. Barista accepts them for compatibility but
//! emits a [`SpecWarning`] so users can replace them with concrete
//! versions in their lockfile.

use barista_version::Version;
use std::fmt;

/// A parsed `<version>` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionSpec {
    /// Plain version string, used as a soft preference.
    Soft(String),
    /// One or more half-open intervals (union semantics).
    Hard(Vec<Interval>),
    /// `LATEST` — must be resolved against metadata.
    Latest,
    /// `RELEASE` — must be resolved against metadata.
    Release,
}

/// A single interval inside a hard range. Either bound may be
/// [`Bound::Unbounded`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interval {
    pub lower: Bound<Version>,
    pub upper: Bound<Version>,
}

/// Interval endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bound<T> {
    /// No bound on this side of the interval.
    Unbounded,
    /// Inclusive lower / upper.
    Included(T),
    /// Exclusive lower / upper.
    Excluded(T),
}

/// Errors produced by [`VersionSpec::parse`].
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("empty version requirement")]
    Empty,
    #[error("unmatched bracket in requirement: {input:?}")]
    UnmatchedBracket { input: String },
    #[error("expected ',' separator inside interval: {input:?}")]
    MissingComma { input: String },
    #[error("invalid bound in requirement {input:?}: {detail}")]
    InvalidBound { input: String, detail: String },
}

impl VersionSpec {
    /// Parse a `<version>` element's text into a [`VersionSpec`].
    pub fn parse(raw: &str) -> Result<Self, ParseError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ParseError::Empty);
        }
        if trimmed.eq_ignore_ascii_case("LATEST") {
            return Ok(VersionSpec::Latest);
        }
        if trimmed.eq_ignore_ascii_case("RELEASE") {
            return Ok(VersionSpec::Release);
        }
        if trimmed.starts_with('[') || trimmed.starts_with('(') {
            return parse_hard(trimmed).map(VersionSpec::Hard);
        }
        Ok(VersionSpec::Soft(trimmed.to_owned()))
    }

    /// Test whether a concrete [`Version`] satisfies this spec.
    ///
    /// - [`VersionSpec::Soft`] always returns `true` — soft is a
    ///   preference, not a constraint.
    /// - [`VersionSpec::Hard`] returns `true` iff the version lies in
    ///   at least one interval.
    /// - [`VersionSpec::Latest`] and [`VersionSpec::Release`] return
    ///   `true` unconditionally; the resolver must resolve them
    ///   against `maven-metadata.xml` before constraint checking.
    pub fn satisfies(&self, v: &Version) -> bool {
        match self {
            VersionSpec::Soft(_) | VersionSpec::Latest | VersionSpec::Release => true,
            VersionSpec::Hard(intervals) => intervals.iter().any(|i| i.contains(v)),
        }
    }

    /// Render back to the canonical string form. Round-trip stable
    /// for parsed inputs (modulo whitespace and case for meta-versions).
    pub fn to_string_canonical(&self) -> String {
        match self {
            VersionSpec::Soft(s) => s.clone(),
            VersionSpec::Latest => "LATEST".to_owned(),
            VersionSpec::Release => "RELEASE".to_owned(),
            VersionSpec::Hard(intervals) => intervals
                .iter()
                .map(interval_to_string)
                .collect::<Vec<_>>()
                .join(","),
        }
    }
}

impl Interval {
    /// Test whether a concrete version is inside this interval.
    pub fn contains(&self, v: &Version) -> bool {
        let lower_ok = match &self.lower {
            Bound::Unbounded => true,
            Bound::Included(b) => v >= b,
            Bound::Excluded(b) => v > b,
        };
        let upper_ok = match &self.upper {
            Bound::Unbounded => true,
            Bound::Included(b) => v <= b,
            Bound::Excluded(b) => v < b,
        };
        lower_ok && upper_ok
    }
}

fn interval_to_string(i: &Interval) -> String {
    // Special case for exact-match `[X]` — both bounds inclusive and
    // equal. The canonical form mirrors Maven's display.
    if let (Bound::Included(lo), Bound::Included(hi)) = (&i.lower, &i.upper) {
        if lo == hi {
            return format!("[{lo}]");
        }
    }
    let open = match &i.lower {
        Bound::Included(_) => '[',
        Bound::Excluded(_) | Bound::Unbounded => '(',
    };
    let close = match &i.upper {
        Bound::Included(_) => ']',
        Bound::Excluded(_) | Bound::Unbounded => ')',
    };
    let lo = match &i.lower {
        Bound::Unbounded => String::new(),
        Bound::Included(v) | Bound::Excluded(v) => v.to_string(),
    };
    let hi = match &i.upper {
        Bound::Unbounded => String::new(),
        Bound::Included(v) | Bound::Excluded(v) => v.to_string(),
    };
    format!("{open}{lo},{hi}{close}")
}

/// Parse a hard range, possibly a comma-joined union of intervals.
fn parse_hard(s: &str) -> Result<Vec<Interval>, ParseError> {
    let mut intervals = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace and comma separators between intervals.
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let open = bytes[i];
        if open != b'[' && open != b'(' {
            return Err(ParseError::InvalidBound {
                input: s.to_owned(),
                detail: format!("expected '[' or '(' at position {i}"),
            });
        }
        // Find the matching close bracket.
        let mut j = i + 1;
        while j < bytes.len() && bytes[j] != b']' && bytes[j] != b')' {
            j += 1;
        }
        if j >= bytes.len() {
            return Err(ParseError::UnmatchedBracket {
                input: s.to_owned(),
            });
        }
        let close = bytes[j];
        let inner = &s[i + 1..j];
        intervals.push(parse_interval(s, inner, open, close)?);
        i = j + 1;
    }
    if intervals.is_empty() {
        return Err(ParseError::InvalidBound {
            input: s.to_owned(),
            detail: "no intervals found".to_owned(),
        });
    }
    Ok(intervals)
}

fn parse_interval(full: &str, inner: &str, open: u8, close: u8) -> Result<Interval, ParseError> {
    let inner = inner.trim();
    // Exact-match form: `[X]` (no comma).
    if !inner.contains(',') {
        if open != b'[' || close != b']' {
            return Err(ParseError::MissingComma {
                input: full.to_owned(),
            });
        }
        if inner.is_empty() {
            return Err(ParseError::InvalidBound {
                input: full.to_owned(),
                detail: "empty exact-match interval".to_owned(),
            });
        }
        let v = parse_version_token(full, inner)?;
        return Ok(Interval {
            lower: Bound::Included(v.clone()),
            upper: Bound::Included(v),
        });
    }
    let mut parts = inner.splitn(2, ',');
    let lo_str = parts.next().unwrap_or("").trim();
    let hi_str = parts.next().unwrap_or("").trim();
    let lower = match (lo_str, open) {
        ("", _) => Bound::Unbounded,
        (s, b'[') => Bound::Included(parse_version_token(full, s)?),
        (s, b'(') => Bound::Excluded(parse_version_token(full, s)?),
        _ => unreachable!("open bracket validated as '[' or '('"),
    };
    let upper = match (hi_str, close) {
        ("", _) => Bound::Unbounded,
        (s, b']') => Bound::Included(parse_version_token(full, s)?),
        (s, b')') => Bound::Excluded(parse_version_token(full, s)?),
        _ => unreachable!("close bracket validated as ']' or ')'"),
    };
    Ok(Interval { lower, upper })
}

fn parse_version_token(full: &str, tok: &str) -> Result<Version, ParseError> {
    Version::parse_strict(tok).map_err(|e| ParseError::InvalidBound {
        input: full.to_owned(),
        detail: format!("invalid version {tok:?}: {e}"),
    })
}

impl fmt::Display for VersionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_canonical())
    }
}

/// Diagnostic emitted when a project uses a meta-version or when a
/// soft requirement is implicitly narrowed during resolution.
///
/// Maven 4 deprecated `LATEST` and `RELEASE`; Barista accepts them
/// for compatibility but surfaces this warning so users can replace
/// them with concrete versions in their lockfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecWarning {
    LatestUsed {
        coords: String,
        resolved_to: String,
    },
    ReleaseUsed {
        coords: String,
        resolved_to: String,
    },
    SoftRangeNarrowing {
        coords: String,
        declared: String,
        resolved_to: String,
    },
}

impl fmt::Display for SpecWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecWarning::LatestUsed {
                coords,
                resolved_to,
            } => write!(
                f,
                "{coords}: LATEST is deprecated; resolved to {resolved_to}. \
                 Pin a concrete version in your POM or lockfile."
            ),
            SpecWarning::ReleaseUsed {
                coords,
                resolved_to,
            } => write!(
                f,
                "{coords}: RELEASE is deprecated; resolved to {resolved_to}. \
                 Pin a concrete version in your POM or lockfile."
            ),
            SpecWarning::SoftRangeNarrowing {
                coords,
                declared,
                resolved_to,
            } => write!(
                f,
                "{coords}: soft requirement {declared} resolved to {resolved_to} \
                 due to a conflicting requirement on another graph path."
            ),
        }
    }
}

impl std::error::Error for SpecWarning {}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s)
    }

    #[test]
    fn parses_soft() {
        assert_eq!(
            VersionSpec::parse("1.2.3").unwrap(),
            VersionSpec::Soft("1.2.3".to_owned())
        );
    }

    #[test]
    fn parses_latest_case_insensitive() {
        assert_eq!(VersionSpec::parse("LATEST").unwrap(), VersionSpec::Latest);
        assert_eq!(VersionSpec::parse("latest").unwrap(), VersionSpec::Latest);
        assert_eq!(VersionSpec::parse("Latest").unwrap(), VersionSpec::Latest);
    }

    #[test]
    fn parses_release_case_insensitive() {
        assert_eq!(VersionSpec::parse("RELEASE").unwrap(), VersionSpec::Release);
        assert_eq!(VersionSpec::parse("release").unwrap(), VersionSpec::Release);
    }

    #[test]
    fn parses_exact_match() {
        let spec = VersionSpec::parse("[1.0]").unwrap();
        assert_eq!(
            spec,
            VersionSpec::Hard(vec![Interval {
                lower: Bound::Included(v("1.0")),
                upper: Bound::Included(v("1.0")),
            }])
        );
    }

    #[test]
    fn parses_lower_bound_inclusive_unbounded_upper() {
        let spec = VersionSpec::parse("[1.0,)").unwrap();
        assert_eq!(
            spec,
            VersionSpec::Hard(vec![Interval {
                lower: Bound::Included(v("1.0")),
                upper: Bound::Unbounded,
            }])
        );
    }

    #[test]
    fn parses_unbounded_lower_inclusive_upper() {
        let spec = VersionSpec::parse("(,1.0]").unwrap();
        assert_eq!(
            spec,
            VersionSpec::Hard(vec![Interval {
                lower: Bound::Unbounded,
                upper: Bound::Included(v("1.0")),
            }])
        );
    }

    #[test]
    fn parses_inclusive_lower_exclusive_upper() {
        let spec = VersionSpec::parse("[1.0,2.0)").unwrap();
        assert_eq!(
            spec,
            VersionSpec::Hard(vec![Interval {
                lower: Bound::Included(v("1.0")),
                upper: Bound::Excluded(v("2.0")),
            }])
        );
    }

    #[test]
    fn parses_exclusive_both() {
        let spec = VersionSpec::parse("(1.0,2.0)").unwrap();
        assert_eq!(
            spec,
            VersionSpec::Hard(vec![Interval {
                lower: Bound::Excluded(v("1.0")),
                upper: Bound::Excluded(v("2.0")),
            }])
        );
    }

    #[test]
    fn parses_union_two_intervals() {
        let spec = VersionSpec::parse("[1.0,2.0),[3.0,)").unwrap();
        assert_eq!(
            spec,
            VersionSpec::Hard(vec![
                Interval {
                    lower: Bound::Included(v("1.0")),
                    upper: Bound::Excluded(v("2.0")),
                },
                Interval {
                    lower: Bound::Included(v("3.0")),
                    upper: Bound::Unbounded,
                },
            ])
        );
    }

    #[test]
    fn empty_is_error() {
        assert_eq!(VersionSpec::parse(""), Err(ParseError::Empty));
        assert_eq!(VersionSpec::parse("   "), Err(ParseError::Empty));
    }

    #[test]
    fn unmatched_bracket_is_error() {
        match VersionSpec::parse("[1.0") {
            Err(ParseError::UnmatchedBracket { .. }) => {}
            other => panic!("expected UnmatchedBracket, got {other:?}"),
        }
    }

    #[test]
    fn missing_comma_is_error() {
        // `[1.0 2.0]` — no comma; treated as an exact-match attempt
        // whose token isn't a valid version. The Version parser is
        // permissive so we instead enforce that ranges (parenthesized
        // forms) and multi-token forms require a comma.
        match VersionSpec::parse("(1.0 2.0)") {
            Err(ParseError::MissingComma { .. }) => {}
            other => panic!("expected MissingComma, got {other:?}"),
        }
    }

    #[test]
    fn soft_satisfies_any_version() {
        let spec = VersionSpec::parse("1.2.3").unwrap();
        assert!(spec.satisfies(&v("1.0")));
        assert!(spec.satisfies(&v("99.0")));
    }

    #[test]
    fn meta_versions_satisfy_any_version() {
        assert!(VersionSpec::Latest.satisfies(&v("1.0")));
        assert!(VersionSpec::Release.satisfies(&v("9.9.9")));
    }

    #[test]
    fn half_open_range_satisfies() {
        let spec = VersionSpec::parse("[1.0,2.0)").unwrap();
        assert!(spec.satisfies(&v("1.5")));
        assert!(spec.satisfies(&v("1.0")));
        assert!(!spec.satisfies(&v("2.0")));
        assert!(!spec.satisfies(&v("0.9")));
    }

    #[test]
    fn union_range_satisfies() {
        let spec = VersionSpec::parse("[1.0,2.0),[3.0,)").unwrap();
        assert!(spec.satisfies(&v("1.5")));
        assert!(!spec.satisfies(&v("2.5")));
        assert!(spec.satisfies(&v("3.0")));
        assert!(spec.satisfies(&v("10.0")));
    }

    #[test]
    fn round_trip_half_open() {
        let raw = "[1.0,2.0)";
        let parsed = VersionSpec::parse(raw).unwrap();
        let rendered = parsed.to_string();
        let reparsed = VersionSpec::parse(&rendered).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn round_trip_meta_versions() {
        for raw in ["LATEST", "RELEASE"] {
            let parsed = VersionSpec::parse(raw).unwrap();
            assert_eq!(parsed.to_string(), raw);
            assert_eq!(VersionSpec::parse(&parsed.to_string()).unwrap(), parsed);
        }
    }

    #[test]
    fn round_trip_soft_qualifier() {
        let raw = "1.2.3-rc-1";
        let parsed = VersionSpec::parse(raw).unwrap();
        assert_eq!(parsed.to_string(), raw);
        assert_eq!(VersionSpec::parse(&parsed.to_string()).unwrap(), parsed);
    }

    #[test]
    fn round_trip_union() {
        let raw = "[1.0,2.0),[3.0,)";
        let parsed = VersionSpec::parse(raw).unwrap();
        let reparsed = VersionSpec::parse(&parsed.to_string()).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn exact_match_rejects_other_version() {
        let spec = VersionSpec::parse("[1.0]").unwrap();
        assert!(spec.satisfies(&v("1.0")));
        assert!(!spec.satisfies(&v("1.0.1")));
        assert!(!spec.satisfies(&v("0.9")));
    }

    #[test]
    fn whitespace_tolerance() {
        let spec = VersionSpec::parse(" [ 1.0 , 2.0 ) ").unwrap();
        assert_eq!(
            spec,
            VersionSpec::Hard(vec![Interval {
                lower: Bound::Included(v("1.0")),
                upper: Bound::Excluded(v("2.0")),
            }])
        );
    }

    #[test]
    fn unbounded_lower_inclusive_upper_semantics() {
        let spec = VersionSpec::parse("(,1.0]").unwrap();
        assert!(spec.satisfies(&v("1.0")));
        assert!(spec.satisfies(&v("0.5")));
        assert!(!spec.satisfies(&v("1.1")));
    }

    #[test]
    fn exclusive_lower_inclusive_upper() {
        let spec = VersionSpec::parse("(1.0,2.0]").unwrap();
        assert_eq!(
            spec,
            VersionSpec::Hard(vec![Interval {
                lower: Bound::Excluded(v("1.0")),
                upper: Bound::Included(v("2.0")),
            }])
        );
        assert!(!spec.satisfies(&v("1.0")));
        assert!(spec.satisfies(&v("1.5")));
        assert!(spec.satisfies(&v("2.0")));
    }

    #[test]
    fn warning_display_includes_coords_and_version() {
        let w = SpecWarning::LatestUsed {
            coords: "com.example:foo".to_owned(),
            resolved_to: "1.4.2".to_owned(),
        };
        let rendered = w.to_string();
        assert!(rendered.contains("com.example:foo"), "got: {rendered}");
        assert!(rendered.contains("1.4.2"), "got: {rendered}");
        assert!(rendered.contains("LATEST"), "got: {rendered}");

        let w = SpecWarning::ReleaseUsed {
            coords: "g:a".to_owned(),
            resolved_to: "9.9".to_owned(),
        };
        assert!(w.to_string().contains("RELEASE"));

        let w = SpecWarning::SoftRangeNarrowing {
            coords: "g:a".to_owned(),
            declared: "1.0".to_owned(),
            resolved_to: "1.4".to_owned(),
        };
        let s = w.to_string();
        assert!(s.contains("1.0") && s.contains("1.4"));
    }

    #[test]
    fn exact_match_canonical_form_is_bracketed_single() {
        let spec = VersionSpec::parse("[1.0]").unwrap();
        assert_eq!(spec.to_string(), "[1.0]");
    }

    #[test]
    fn unbounded_upper_renders_without_version() {
        let spec = VersionSpec::parse("[1.0,)").unwrap();
        assert_eq!(spec.to_string(), "[1.0,)");
    }

    #[test]
    fn unbounded_lower_renders_without_version() {
        let spec = VersionSpec::parse("(,2.0]").unwrap();
        assert_eq!(spec.to_string(), "(,2.0]");
    }
}
