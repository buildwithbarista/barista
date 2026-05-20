// SPDX-License-Identifier: MIT OR Apache-2.0

// Workspace security lints (clippy::unwrap_used, clippy::expect_used,
// clippy::panic, clippy::as_conversions) are warned on workspace-wide via
// the root `Cargo.toml`. The coords crate's existing test-helper panics
// and `as` conversions are allowed here pending an incremental ratchet.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Maven artifact coordinates.
//!
//! This crate models the three Maven identity tuples that appear throughout
//! a dependency-resolution and caching pipeline:
//!
//! - [`Coords`] — the **resolution identity** `(group, artifact)`. Two
//!   artifacts that share a `Coords` are "the same dependency" for the
//!   purposes of conflict resolution: a resolver picks exactly one version
//!   per `Coords`.
//! - [`GATC`] — the **artifact-file identity**
//!   `(group, artifact, packaging, classifier)`. A single `Coords` may map
//!   to many `GATC`s (e.g. the main `jar`, the `sources` jar, the `javadoc`
//!   jar, a `tests` classifier). Caches key on `GATC` plus version to find
//!   the right file on disk.
//! - [`GATCV`] — the **fully-qualified coordinate**
//!   `(group, artifact, packaging, classifier, version)`. This is the form
//!   that appears in lockfile entries and in `mvn dependency:tree` output.
//!
//! All three types parse from and display as Maven's canonical
//! colon-separated string syntax (`group:artifact[:packaging[:classifier]]:version`
//! for `GATCV`). They implement `serde::Serialize` / `Deserialize` using the
//! string form, so a TOML or JSON document can list a dependency as a bare
//! string rather than an inline table.
//!
//! Reference: <https://maven.apache.org/pom.html#Maven_Coordinates>.
//!
//! # Examples
//!
//! ```
//! use barista_coords::GATCV;
//! use std::str::FromStr;
//!
//! let gatcv = GATCV::from_str("org.apache.commons:commons-lang3:3.14.0").unwrap();
//! assert_eq!(gatcv.gatc.packaging, "jar");
//! assert_eq!(gatcv.gatc.classifier, None);
//! assert_eq!(gatcv.version, "3.14.0");
//!
//! // Two different GATCVs that share resolution identity:
//! let main = GATCV::from_str("com.google.guava:guava:33.0.0-jre").unwrap();
//! let sources =
//!     GATCV::from_str("com.google.guava:guava:jar:sources:33.0.0-jre").unwrap();
//! assert_eq!(main.coords(), sources.coords());
//! ```

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Default Maven packaging when none is specified.
pub const DEFAULT_PACKAGING: &str = "jar";

/// Errors returned when parsing a coordinate string.
#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum ParseError {
    /// Input was empty.
    #[error("coordinate string is empty")]
    Empty,
    /// Fewer colon-separated components than the minimum for this type.
    #[error("expected at least 2 components, got {0}")]
    TooFewComponents(usize),
    /// More colon-separated components than the maximum (5) allowed.
    #[error("expected at most 5 components, got {0}")]
    TooManyComponents(usize),
    /// A required component was the empty string (e.g. `"group::version"`).
    #[error("component {field:?} is empty")]
    EmptyComponent {
        /// Which named component was empty.
        field: &'static str,
    },
}

// ---------------------------------------------------------------------------
// Coords
// ---------------------------------------------------------------------------

/// The minimal Maven identity: `(group, artifact)`.
///
/// This is the **resolution identity**. A dependency resolver picks one
/// version per `Coords`; conflicts between `1.0` and `2.0` of the same
/// `Coords` are resolved by the conflict-resolution algorithm.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Coords {
    /// Maven `groupId` (e.g. `org.apache.commons`).
    pub group: String,
    /// Maven `artifactId` (e.g. `commons-lang3`).
    pub artifact: String,
}

impl Coords {
    /// Construct from owned strings, validating non-emptiness.
    pub fn new(group: impl Into<String>, artifact: impl Into<String>) -> Result<Self, ParseError> {
        let group = group.into();
        let artifact = artifact.into();
        if group.is_empty() {
            return Err(ParseError::EmptyComponent { field: "group" });
        }
        if artifact.is_empty() {
            return Err(ParseError::EmptyComponent { field: "artifact" });
        }
        Ok(Self { group, artifact })
    }
}

impl FromStr for Coords {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ParseError::Empty);
        }
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() < 2 {
            return Err(ParseError::TooFewComponents(parts.len()));
        }
        if parts.len() > 2 {
            return Err(ParseError::TooManyComponents(parts.len()));
        }
        Coords::new(parts[0], parts[1])
    }
}

impl fmt::Display for Coords {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.group, self.artifact)
    }
}

// ---------------------------------------------------------------------------
// GATC
// ---------------------------------------------------------------------------

/// Extended Maven identity: `(group, artifact, packaging, classifier)`.
///
/// `packaging` defaults to `"jar"`. `classifier` is `None` for the default
/// (unclassified) artifact and `Some("sources")` / `Some("javadoc")` /
/// `Some("tests")` / etc. for the well-known auxiliary artifacts.
///
/// `Display` uses the terse Maven form — the form `mvn dependency:tree`
/// prints — when both `packaging` is the default and `classifier` is
/// absent: e.g. `"g:a"`. As soon as either is non-default, the explicit
/// form is emitted: `"g:a:packaging"` or `"g:a:packaging:classifier"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GATC {
    /// Maven `groupId`.
    pub group: String,
    /// Maven `artifactId`.
    pub artifact: String,
    /// Packaging (`"jar"`, `"pom"`, `"war"`, …). Defaults to `"jar"`.
    pub packaging: String,
    /// Optional classifier (`Some("sources")`, …). `None` for the default
    /// (main) artifact.
    pub classifier: Option<String>,
}

impl GATC {
    /// The resolution identity for this artifact-file identity.
    pub fn coords(&self) -> Coords {
        Coords {
            group: self.group.clone(),
            artifact: self.artifact.clone(),
        }
    }

    /// Whether this `GATC` uses the canonical default `(jar, no classifier)`.
    pub fn is_default(&self) -> bool {
        self.packaging == DEFAULT_PACKAGING && self.classifier.is_none()
    }
}

impl FromStr for GATC {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ParseError::Empty);
        }
        let parts: Vec<&str> = s.split(':').collect();
        match parts.len() {
            0..=1 => Err(ParseError::TooFewComponents(parts.len())),
            2 => {
                check_nonempty(parts[0], "group")?;
                check_nonempty(parts[1], "artifact")?;
                Ok(GATC {
                    group: parts[0].to_owned(),
                    artifact: parts[1].to_owned(),
                    packaging: DEFAULT_PACKAGING.to_owned(),
                    classifier: None,
                })
            }
            3 => {
                check_nonempty(parts[0], "group")?;
                check_nonempty(parts[1], "artifact")?;
                check_nonempty(parts[2], "packaging")?;
                Ok(GATC {
                    group: parts[0].to_owned(),
                    artifact: parts[1].to_owned(),
                    packaging: parts[2].to_owned(),
                    classifier: None,
                })
            }
            4 => {
                check_nonempty(parts[0], "group")?;
                check_nonempty(parts[1], "artifact")?;
                // 4-component GATC is `g:a:packaging:classifier`. Maven does
                // not permit an empty packaging slot with a classifier
                // present (`g:a::classifier`); a classifier requires an
                // explicit packaging.
                check_nonempty(parts[2], "packaging")?;
                check_nonempty(parts[3], "classifier")?;
                Ok(GATC {
                    group: parts[0].to_owned(),
                    artifact: parts[1].to_owned(),
                    packaging: parts[2].to_owned(),
                    classifier: Some(parts[3].to_owned()),
                })
            }
            n => Err(ParseError::TooManyComponents(n)),
        }
    }
}

impl fmt::Display for GATC {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.classifier, self.packaging.as_str()) {
            (None, p) if p == DEFAULT_PACKAGING => write!(f, "{}:{}", self.group, self.artifact),
            (None, p) => write!(f, "{}:{}:{}", self.group, self.artifact, p),
            (Some(c), p) => write!(f, "{}:{}:{}:{}", self.group, self.artifact, p, c),
        }
    }
}

// ---------------------------------------------------------------------------
// GATCV
// ---------------------------------------------------------------------------

/// Fully-qualified Maven coordinate: a [`GATC`] plus a version.
///
/// This is the form that appears in lockfile entries and in
/// `mvn dependency:tree` output. Parsing accepts 3, 4, or 5 components:
///
/// | Form                | Components                       |
/// |---------------------|----------------------------------|
/// | `g:a:v`             | group:artifact:version           |
/// | `g:a:packaging:v`   | group:artifact:packaging:version |
/// | `g:a:packaging:c:v` | full 5-tuple                     |
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GATCV {
    /// The artifact-file identity.
    pub gatc: GATC,
    /// Version string (Maven version semantics are out of scope here).
    pub version: String,
}

impl GATCV {
    /// The resolution identity for this fully-qualified coordinate.
    pub fn coords(&self) -> Coords {
        self.gatc.coords()
    }
}

impl FromStr for GATCV {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ParseError::Empty);
        }
        let parts: Vec<&str> = s.split(':').collect();
        match parts.len() {
            0..=2 => Err(ParseError::TooFewComponents(parts.len())),
            3 => {
                check_nonempty(parts[0], "group")?;
                check_nonempty(parts[1], "artifact")?;
                check_nonempty(parts[2], "version")?;
                Ok(GATCV {
                    gatc: GATC {
                        group: parts[0].to_owned(),
                        artifact: parts[1].to_owned(),
                        packaging: DEFAULT_PACKAGING.to_owned(),
                        classifier: None,
                    },
                    version: parts[2].to_owned(),
                })
            }
            4 => {
                check_nonempty(parts[0], "group")?;
                check_nonempty(parts[1], "artifact")?;
                check_nonempty(parts[2], "packaging")?;
                check_nonempty(parts[3], "version")?;
                Ok(GATCV {
                    gatc: GATC {
                        group: parts[0].to_owned(),
                        artifact: parts[1].to_owned(),
                        packaging: parts[2].to_owned(),
                        classifier: None,
                    },
                    version: parts[3].to_owned(),
                })
            }
            5 => {
                check_nonempty(parts[0], "group")?;
                check_nonempty(parts[1], "artifact")?;
                // 5-component form is `g:a:packaging:classifier:v`. Maven
                // does not permit `g:a::classifier:v` — the classifier
                // form requires an explicit packaging slot.
                check_nonempty(parts[2], "packaging")?;
                check_nonempty(parts[3], "classifier")?;
                check_nonempty(parts[4], "version")?;
                Ok(GATCV {
                    gatc: GATC {
                        group: parts[0].to_owned(),
                        artifact: parts[1].to_owned(),
                        packaging: parts[2].to_owned(),
                        classifier: Some(parts[3].to_owned()),
                    },
                    version: parts[4].to_owned(),
                })
            }
            n => Err(ParseError::TooManyComponents(n)),
        }
    }
}

impl fmt::Display for GATCV {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.gatc.classifier, self.gatc.packaging.as_str()) {
            (None, p) if p == DEFAULT_PACKAGING => write!(
                f,
                "{}:{}:{}",
                self.gatc.group, self.gatc.artifact, self.version
            ),
            (None, p) => write!(
                f,
                "{}:{}:{}:{}",
                self.gatc.group, self.gatc.artifact, p, self.version
            ),
            (Some(c), p) => write!(
                f,
                "{}:{}:{}:{}:{}",
                self.gatc.group, self.gatc.artifact, p, c, self.version
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn check_nonempty(s: &str, field: &'static str) -> Result<(), ParseError> {
    if s.is_empty() {
        Err(ParseError::EmptyComponent { field })
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// serde — serialize as the canonical string form.
// ---------------------------------------------------------------------------

macro_rules! impl_serde_via_str {
    ($t:ty) => {
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
                ser.collect_str(self)
            }
        }
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
                let s = String::deserialize(de)?;
                <$t>::from_str(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}

impl_serde_via_str!(Coords);
impl_serde_via_str!(GATC);
impl_serde_via_str!(GATCV);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ----- Coords -----

    #[test]
    fn coords_parse_basic() {
        let c: Coords = "org.example:lib".parse().unwrap();
        assert_eq!(c.group, "org.example");
        assert_eq!(c.artifact, "lib");
    }

    #[test]
    fn coords_display_roundtrip() {
        let s = "org.apache.commons:commons-lang3";
        let c: Coords = s.parse().unwrap();
        assert_eq!(c.to_string(), s);
        assert_eq!(c.to_string().parse::<Coords>().unwrap(), c);
    }

    #[test]
    fn coords_rejects_empty() {
        assert_eq!("".parse::<Coords>(), Err(ParseError::Empty));
    }

    #[test]
    fn coords_rejects_single_component() {
        assert_eq!(
            "lonely".parse::<Coords>(),
            Err(ParseError::TooFewComponents(1))
        );
    }

    #[test]
    fn coords_rejects_extra_components() {
        assert_eq!(
            "g:a:v".parse::<Coords>(),
            Err(ParseError::TooManyComponents(3))
        );
    }

    #[test]
    fn coords_rejects_empty_components() {
        assert_eq!(
            ":artifact".parse::<Coords>(),
            Err(ParseError::EmptyComponent { field: "group" })
        );
        assert_eq!(
            "group:".parse::<Coords>(),
            Err(ParseError::EmptyComponent { field: "artifact" })
        );
    }

    // ----- GATC -----

    #[test]
    fn gatc_two_components_defaults() {
        let g: GATC = "g:a".parse().unwrap();
        assert_eq!(g.packaging, "jar");
        assert_eq!(g.classifier, None);
        assert!(g.is_default());
    }

    #[test]
    fn gatc_three_components_packaging() {
        let g: GATC = "g:a:pom".parse().unwrap();
        assert_eq!(g.packaging, "pom");
        assert_eq!(g.classifier, None);
    }

    #[test]
    fn gatc_four_components_classifier() {
        let g: GATC = "g:a:jar:sources".parse().unwrap();
        assert_eq!(g.packaging, "jar");
        assert_eq!(g.classifier.as_deref(), Some("sources"));
    }

    #[test]
    fn gatc_display_terse_for_default() {
        let g: GATC = "g:a".parse().unwrap();
        assert_eq!(g.to_string(), "g:a");
    }

    #[test]
    fn gatc_display_explicit_packaging() {
        let g: GATC = "g:a:pom".parse().unwrap();
        assert_eq!(g.to_string(), "g:a:pom");
    }

    #[test]
    fn gatc_display_with_classifier() {
        let g: GATC = "g:a:jar:sources".parse().unwrap();
        assert_eq!(g.to_string(), "g:a:jar:sources");
    }

    #[test]
    fn gatc_rejects_classifier_without_packaging() {
        // `g:a::sources` parses as 4 components with an empty packaging slot;
        // Maven does not permit classifier without explicit packaging.
        assert_eq!(
            "g:a::sources".parse::<GATC>(),
            Err(ParseError::EmptyComponent { field: "packaging" })
        );
    }

    // ----- GATCV -----

    #[test]
    fn gatcv_three_component_form() {
        let v: GATCV = "g:a:1.0".parse().unwrap();
        assert_eq!(v.gatc.packaging, "jar");
        assert_eq!(v.gatc.classifier, None);
        assert_eq!(v.version, "1.0");
    }

    #[test]
    fn gatcv_four_component_form() {
        let v: GATCV = "g:a:war:1.0".parse().unwrap();
        assert_eq!(v.gatc.packaging, "war");
        assert_eq!(v.gatc.classifier, None);
        assert_eq!(v.version, "1.0");
    }

    #[test]
    fn gatcv_five_component_form() {
        let v: GATCV = "g:a:jar:sources:1.0".parse().unwrap();
        assert_eq!(v.gatc.packaging, "jar");
        assert_eq!(v.gatc.classifier.as_deref(), Some("sources"));
        assert_eq!(v.version, "1.0");
    }

    #[test]
    fn gatcv_display_roundtrip_all_forms() {
        for s in [
            "g:a:1.0",
            "g:a:pom:1.0",
            "g:a:jar:sources:1.0",
            "org.apache.commons:commons-lang3:3.14.0",
            "com.google.guava:guava:jar:javadoc:33.0.0-jre",
        ] {
            let v: GATCV = s.parse().unwrap();
            assert_eq!(v.to_string(), s, "round-trip failed for {s}");
            assert_eq!(v.to_string().parse::<GATCV>().unwrap(), v);
        }
    }

    #[test]
    fn gatcv_rejects_too_few_components() {
        assert_eq!("g:a".parse::<GATCV>(), Err(ParseError::TooFewComponents(2)));
    }

    #[test]
    fn gatcv_rejects_too_many_components() {
        assert_eq!(
            "g:a:p:c:v:extra".parse::<GATCV>(),
            Err(ParseError::TooManyComponents(6))
        );
    }

    #[test]
    fn gatcv_rejects_classifier_without_packaging() {
        // `g:a::sources:1.0` — 5 components with empty packaging slot.
        // Maven does not permit this form; classifier requires explicit packaging.
        assert_eq!(
            "g:a::sources:1.0".parse::<GATCV>(),
            Err(ParseError::EmptyComponent { field: "packaging" })
        );
    }

    #[test]
    fn gatcv_rejects_empty_version() {
        assert_eq!(
            "g:a:".parse::<GATCV>(),
            Err(ParseError::EmptyComponent { field: "version" })
        );
    }

    #[test]
    fn gatcv_rejects_empty_string() {
        assert_eq!("".parse::<GATCV>(), Err(ParseError::Empty));
    }

    // ----- Identity -----

    #[test]
    fn coords_equal_across_versions_and_classifiers() {
        let a: GATCV = "g:a:1.0".parse().unwrap();
        let b: GATCV = "g:a:jar:sources:2.0".parse().unwrap();
        assert_eq!(a.coords(), b.coords());
        assert_eq!(a.coords().to_string(), "g:a");
    }

    // ----- serde -----

    #[test]
    fn gatcv_serde_json_roundtrip() {
        let v: GATCV = "com.google.guava:guava:jar:sources:33.0.0-jre"
            .parse()
            .unwrap();
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "\"com.google.guava:guava:jar:sources:33.0.0-jre\"");
        let v2: GATCV = serde_json::from_str(&json).unwrap();
        assert_eq!(v, v2);
    }

    #[test]
    fn coords_serde_json_roundtrip() {
        let c: Coords = "org.example:lib".parse().unwrap();
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "\"org.example:lib\"");
        let c2: Coords = serde_json::from_str(&json).unwrap();
        assert_eq!(c, c2);
    }

    #[test]
    fn gatc_serde_json_roundtrip() {
        let g: GATC = "g:a:jar:tests".parse().unwrap();
        let json = serde_json::to_string(&g).unwrap();
        assert_eq!(json, "\"g:a:jar:tests\"");
        let g2: GATC = serde_json::from_str(&json).unwrap();
        assert_eq!(g, g2);
    }

    #[test]
    fn deserialize_surfaces_parse_error() {
        let err = serde_json::from_str::<GATCV>("\"not-enough\"").unwrap_err();
        assert!(err.to_string().contains("at least"));
    }

    // ----- Constructor / accessor -----

    #[test]
    fn coords_new_rejects_empty() {
        assert_eq!(
            Coords::new("", "a"),
            Err(ParseError::EmptyComponent { field: "group" })
        );
        assert_eq!(
            Coords::new("g", ""),
            Err(ParseError::EmptyComponent { field: "artifact" })
        );
    }
}
