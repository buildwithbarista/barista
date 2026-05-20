// SPDX-License-Identifier: MIT OR Apache-2.0

//! Maven coordinate parsing for the upstream-on-miss path.
//!
//! Clients hint the fetcher at the artifact they're trying to land
//! via the `X-Barista-Coords` request header. The header carries a
//! colon-separated Maven coordinate in one of three shapes:
//!
//! ```text
//! g:a:v             // group, artifact, version (default packaging = jar)
//! g:a:t:v           // group, artifact, type, version
//! g:a:t:c:v         // group, artifact, type, classifier, version
//! ```
//!
//! Each component must be non-empty and made of characters in
//! `[A-Za-z0-9._-]`. The same character class Maven itself accepts for
//! group / artifact / version segments — anything outside it is
//! either a typo or an injection attempt against the upstream URL.
//!
//! The parsed [`Coords`] knows how to render itself into a Maven
//! repository layout path via [`Coords::to_maven_path`].

use super::error::UpstreamError;

/// Default packaging type when the coords have no explicit `t`
/// segment. Matches Maven's own default.
const DEFAULT_TYPE: &str = "jar";

/// A parsed Maven artifact coordinate.
///
/// `classifier` is `None` for both the 3-component (`g:a:v`) and
/// 4-component (`g:a:t:v`) shapes; only the 5-component shape carries
/// a classifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coords {
    /// Group id (e.g. `org.slf4j`).
    pub group: String,
    /// Artifact id (e.g. `slf4j-api`).
    pub artifact: String,
    /// Packaging type / file extension (e.g. `jar`, `pom`, `war`).
    pub r#type: String,
    /// Optional classifier (e.g. `sources`, `javadoc`).
    pub classifier: Option<String>,
    /// Version string (e.g. `2.0.13`).
    pub version: String,
}

impl Coords {
    /// Parse `X-Barista-Coords` header content.
    ///
    /// Accepts `g:a:v`, `g:a:t:v`, and `g:a:t:c:v`. Anything else is
    /// rejected as [`UpstreamError::InvalidCoords`]. Whitespace is not
    /// trimmed by this function — callers (the HTTP handler) trim the
    /// raw header value before passing it in.
    pub fn parse(raw: &str) -> Result<Self, UpstreamError> {
        if raw.is_empty() {
            return Err(UpstreamError::InvalidCoords {
                reason: "header was empty".to_string(),
            });
        }
        // Reject any whitespace inside the coords; the Maven coord
        // grammar has no whitespace at all and a value with embedded
        // spaces is almost certainly a copy/paste accident.
        if raw.chars().any(char::is_whitespace) {
            return Err(UpstreamError::InvalidCoords {
                reason: "coords must not contain whitespace".to_string(),
            });
        }

        let parts: Vec<&str> = raw.split(':').collect();
        let (group, artifact, r#type, classifier, version) = match parts.as_slice() {
            [g, a, v] => (
                (*g).to_string(),
                (*a).to_string(),
                DEFAULT_TYPE.to_string(),
                None,
                (*v).to_string(),
            ),
            [g, a, t, v] => (
                (*g).to_string(),
                (*a).to_string(),
                (*t).to_string(),
                None,
                (*v).to_string(),
            ),
            [g, a, t, c, v] => (
                (*g).to_string(),
                (*a).to_string(),
                (*t).to_string(),
                Some((*c).to_string()),
                (*v).to_string(),
            ),
            _ => {
                return Err(UpstreamError::InvalidCoords {
                    reason: format!(
                        "expected 3, 4, or 5 colon-separated components, got {}",
                        parts.len()
                    ),
                });
            }
        };

        validate_segment("group", &group)?;
        validate_segment("artifact", &artifact)?;
        validate_segment("type", &r#type)?;
        if let Some(c) = classifier.as_deref() {
            validate_segment("classifier", c)?;
        }
        validate_segment("version", &version)?;

        Ok(Self {
            group,
            artifact,
            r#type,
            classifier,
            version,
        })
    }

    /// Render the Maven repository layout path for this coordinate.
    ///
    /// Examples:
    ///
    /// - `org.slf4j:slf4j-api:2.0.13`
    ///   → `org/slf4j/slf4j-api/2.0.13/slf4j-api-2.0.13.jar`
    /// - `org.example:foo:jar:sources:1.0`
    ///   → `org/example/foo/1.0/foo-1.0-sources.jar`
    ///
    /// Group ids dot-separated in the input become slash-separated in
    /// the output. The leading `/` is intentionally omitted so callers
    /// can join the path onto a base URL with [`url::Url::join`]
    /// without surprising it into discarding earlier segments.
    pub fn to_maven_path(&self) -> String {
        let group_path = self.group.replace('.', "/");
        let suffix = match &self.classifier {
            Some(c) => format!(
                "{artifact}-{version}-{classifier}.{ext}",
                artifact = self.artifact,
                version = self.version,
                classifier = c,
                ext = self.r#type,
            ),
            None => format!(
                "{artifact}-{version}.{ext}",
                artifact = self.artifact,
                version = self.version,
                ext = self.r#type,
            ),
        };
        format!(
            "{group}/{artifact}/{version}/{suffix}",
            group = group_path,
            artifact = self.artifact,
            version = self.version,
        )
    }
}

/// Validate one coordinate segment: non-empty and matches the
/// `[A-Za-z0-9._-]+` character class.
fn validate_segment(name: &str, value: &str) -> Result<(), UpstreamError> {
    if value.is_empty() {
        return Err(UpstreamError::InvalidCoords {
            reason: format!("{name} segment was empty"),
        });
    }
    for c in value.chars() {
        if !is_allowed_segment_char(c) {
            return Err(UpstreamError::InvalidCoords {
                reason: format!("{name} segment contains illegal character {c:?}"),
            });
        }
    }
    Ok(())
}

/// `[A-Za-z0-9._-]` — the character class Maven itself accepts for
/// group / artifact / version / classifier / type segments.
fn is_allowed_segment_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn parse_three_components_defaults_type_to_jar() {
        let c = Coords::parse("org.slf4j:slf4j-api:2.0.13").unwrap();
        assert_eq!(c.group, "org.slf4j");
        assert_eq!(c.artifact, "slf4j-api");
        assert_eq!(c.r#type, "jar");
        assert!(c.classifier.is_none());
        assert_eq!(c.version, "2.0.13");
    }

    #[test]
    fn parse_four_components_picks_up_type() {
        let c = Coords::parse("org.example:foo:pom:1.0").unwrap();
        assert_eq!(c.group, "org.example");
        assert_eq!(c.artifact, "foo");
        assert_eq!(c.r#type, "pom");
        assert!(c.classifier.is_none());
        assert_eq!(c.version, "1.0");
    }

    #[test]
    fn parse_five_components_picks_up_classifier() {
        let c = Coords::parse("org.example:foo:jar:sources:1.0").unwrap();
        assert_eq!(c.group, "org.example");
        assert_eq!(c.artifact, "foo");
        assert_eq!(c.r#type, "jar");
        assert_eq!(c.classifier.as_deref(), Some("sources"));
        assert_eq!(c.version, "1.0");
    }

    #[test]
    fn parse_rejects_empty_header() {
        let err = Coords::parse("").unwrap_err();
        assert!(matches!(err, UpstreamError::InvalidCoords { .. }));
    }

    #[test]
    fn parse_rejects_too_few_components() {
        let err = Coords::parse("only:two").unwrap_err();
        assert!(matches!(err, UpstreamError::InvalidCoords { .. }));
    }

    #[test]
    fn parse_rejects_too_many_components() {
        let err = Coords::parse("a:b:c:d:e:f").unwrap_err();
        assert!(matches!(err, UpstreamError::InvalidCoords { .. }));
    }

    #[test]
    fn parse_rejects_empty_segment() {
        let err = Coords::parse("org.example::1.0").unwrap_err();
        assert!(matches!(err, UpstreamError::InvalidCoords { .. }));
    }

    #[test]
    fn parse_rejects_whitespace() {
        let err = Coords::parse("org.example:foo bar:1.0").unwrap_err();
        assert!(matches!(err, UpstreamError::InvalidCoords { .. }));
    }

    #[test]
    fn parse_rejects_illegal_character() {
        // `/` would let a malicious caller break out of the Maven path
        // layout when the coords get joined onto the upstream URL.
        let err = Coords::parse("org.example:foo/bar:1.0").unwrap_err();
        assert!(matches!(err, UpstreamError::InvalidCoords { .. }));
        // Same story for `..` would be allowed (`.` is fine in
        // segments) but the actual injection vector is `/` — the
        // segment-by-segment validator does the right thing for
        // both.
        let err = Coords::parse("org.example:foo:1.0;evil").unwrap_err();
        assert!(matches!(err, UpstreamError::InvalidCoords { .. }));
    }

    #[test]
    fn to_maven_path_three_components() {
        let c = Coords::parse("org.slf4j:slf4j-api:2.0.13").unwrap();
        assert_eq!(
            c.to_maven_path(),
            "org/slf4j/slf4j-api/2.0.13/slf4j-api-2.0.13.jar"
        );
    }

    #[test]
    fn to_maven_path_four_components_uses_type_as_extension() {
        let c = Coords::parse("org.example:foo:pom:1.0").unwrap();
        assert_eq!(c.to_maven_path(), "org/example/foo/1.0/foo-1.0.pom");
    }

    #[test]
    fn to_maven_path_five_components_inserts_classifier() {
        let c = Coords::parse("org.example:foo:jar:sources:1.0").unwrap();
        assert_eq!(
            c.to_maven_path(),
            "org/example/foo/1.0/foo-1.0-sources.jar"
        );
    }
}
