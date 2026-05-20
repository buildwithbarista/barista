// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tap registration, inspection, and health probing.
//!
//! A **tap** is a named, registered remote endpoint — either a
//! [`roastery`](TapKind::Roastery) shared-cache server or a
//! (placeholder) [`worker`](TapKind::Worker). This crate owns the
//! tap *domain*: the validated [`Tap`] value, the in-memory
//! [`TapRegistry`] that enforces name uniqueness, and the async
//! [`probe`] that liveness-checks a tap over HTTP.
//!
//! ## Scope
//!
//! v0.1 ships **registration and inspection only**. A tap is recorded
//! (name, URL, kind), listed, removed, and health-probed. **Routing
//! build actions to a tap is explicitly out of scope** for v0.1 — a
//! registered tap is a named, inspectable endpoint and nothing more.
//!
//! ## Layering
//!
//! - This crate owns the domain types, the registry, and the health
//!   logic. It is transport-aware (it uses `reqwest` for the probe)
//!   but persistence-agnostic.
//! - `barista-config` owns the on-disk `[[taps]]` serde shape
//!   ([`barista_config::TapDecl`]). The [`From`]/[`TryFrom`] bridge
//!   in this crate converts between the two.
//! - The CLI wires the registry to the config file: load → mutate →
//!   write back.
//!
//! The dependency edge is one-way: `barista-tap` depends on
//! `barista-config`, never the reverse, and neither depends on the
//! CLI.

use std::time::Duration;

use serde::Serialize;
use url::Url;

/// Default per-probe timeout. A dead endpoint resolves to
/// [`TapHealth::Unhealthy`] within this bound rather than hanging.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The character class a tap name must match: ASCII alphanumerics
/// plus `.`, `_`, and `-`. Documented here so the CLI help text and
/// the validator never drift.
pub const NAME_CHARS_DESC: &str = "letters, digits, '.', '_', and '-'";

// ============================================================
// Domain types
// ============================================================

/// What kind of endpoint a tap points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TapKind {
    /// A roastery shared-cache server (the common case). Probed via
    /// its unauthenticated `/healthz` liveness endpoint.
    #[default]
    Roastery,
    /// A remote worker endpoint. Placeholder in v0.1 — registered
    /// and liveness-probed (plain HTTP `HEAD`), but never routed to.
    Worker,
}

impl TapKind {
    /// The lowercase wire/CLI spelling (`"roastery"` / `"worker"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Roastery => "roastery",
            Self::Worker => "worker",
        }
    }
}

impl std::fmt::Display for TapKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TapKind {
    type Err = TapError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "roastery" => Ok(Self::Roastery),
            "worker" => Ok(Self::Worker),
            other => Err(TapError::InvalidKind {
                value: other.to_string(),
            }),
        }
    }
}

/// A validated tap: a unique name, an absolute `http`/`https` URL,
/// and a [`TapKind`].
///
/// Construct via [`Tap::new`], which performs all validation, so an
/// existing `Tap` is always well-formed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tap {
    /// Unique, human-readable name. Matches [`NAME_CHARS_DESC`].
    pub name: String,
    /// Absolute `http`/`https` endpoint URL.
    pub url: Url,
    /// Endpoint kind.
    pub kind: TapKind,
}

impl Tap {
    /// Validate and build a [`Tap`].
    ///
    /// - `name` must be non-empty and match `[A-Za-z0-9._-]+`.
    /// - `url` must parse and be an absolute `http`/`https` URL.
    ///
    /// # Errors
    ///
    /// Returns [`TapError::InvalidName`] or [`TapError::InvalidUrl`]
    /// on a malformed input. Uniqueness is *not* checked here — that
    /// is the [`TapRegistry`]'s job.
    pub fn new(
        name: impl Into<String>,
        url: impl AsRef<str>,
        kind: TapKind,
    ) -> Result<Self, TapError> {
        let name = name.into();
        validate_name(&name)?;
        let url = parse_endpoint_url(url.as_ref())?;
        Ok(Self { name, url, kind })
    }
}

/// Validate a tap name: non-empty and `[A-Za-z0-9._-]+`.
fn validate_name(name: &str) -> Result<(), TapError> {
    if name.is_empty() {
        return Err(TapError::InvalidName {
            name: name.to_string(),
            reason: "name must not be empty".to_string(),
        });
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(TapError::InvalidName {
            name: name.to_string(),
            reason: format!("name may only contain {NAME_CHARS_DESC}"),
        });
    }
    Ok(())
}

/// Parse + validate a tap endpoint URL: must be absolute `http`/`https`.
fn parse_endpoint_url(raw: &str) -> Result<Url, TapError> {
    let url = Url::parse(raw).map_err(|e| TapError::InvalidUrl {
        url: raw.to_string(),
        reason: e.to_string(),
    })?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(TapError::InvalidUrl {
                url: raw.to_string(),
                reason: format!("scheme must be http or https, got {other:?}"),
            });
        }
    }
    if !url.has_host() {
        return Err(TapError::InvalidUrl {
            url: raw.to_string(),
            reason: "URL must have a host".to_string(),
        });
    }
    Ok(url)
}

// ============================================================
// Registry
// ============================================================

/// An in-memory set of registered taps with unique names.
///
/// The registry is the unit of mutation the CLI loads from
/// `barista.toml`, mutates ([`add`](TapRegistry::add) /
/// [`remove`](TapRegistry::remove)), and writes back. Insertion order
/// is preserved so a `tap list` reflects the order taps were added.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TapRegistry {
    taps: Vec<Tap>,
}

impl TapRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a registry from an iterator of taps, rejecting the first
    /// duplicate name encountered.
    ///
    /// # Errors
    ///
    /// [`TapError::DuplicateName`] if two taps share a name.
    pub fn from_taps(taps: impl IntoIterator<Item = Tap>) -> Result<Self, TapError> {
        let mut reg = Self::new();
        for tap in taps {
            reg.add(tap)?;
        }
        Ok(reg)
    }

    /// Register a tap.
    ///
    /// # Errors
    ///
    /// [`TapError::DuplicateName`] if a tap with the same name is
    /// already registered.
    pub fn add(&mut self, tap: Tap) -> Result<(), TapError> {
        if self.get(&tap.name).is_some() {
            return Err(TapError::DuplicateName { name: tap.name });
        }
        self.taps.push(tap);
        Ok(())
    }

    /// Remove the tap named `name`.
    ///
    /// Idempotent: returns `Ok(true)` if a tap was removed, `Ok(false)`
    /// if no tap by that name existed. Removing an absent tap is a
    /// clean no-op, never an error.
    pub fn remove(&mut self, name: &str) -> Result<bool, TapError> {
        let before = self.taps.len();
        self.taps.retain(|t| t.name != name);
        Ok(self.taps.len() != before)
    }

    /// Look up a tap by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Tap> {
        self.taps.iter().find(|t| t.name == name)
    }

    /// All registered taps, in insertion order.
    #[must_use]
    pub fn list(&self) -> &[Tap] {
        &self.taps
    }

    /// True if no taps are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.taps.is_empty()
    }

    /// Number of registered taps.
    #[must_use]
    pub fn len(&self) -> usize {
        self.taps.len()
    }
}

// ============================================================
// Persistence bridge (barista-config <-> barista-tap)
// ============================================================

impl From<TapKind> for barista_config::TapKindDecl {
    fn from(k: TapKind) -> Self {
        match k {
            TapKind::Roastery => Self::Roastery,
            TapKind::Worker => Self::Worker,
        }
    }
}

impl From<barista_config::TapKindDecl> for TapKind {
    fn from(k: barista_config::TapKindDecl) -> Self {
        match k {
            barista_config::TapKindDecl::Roastery => Self::Roastery,
            barista_config::TapKindDecl::Worker => Self::Worker,
        }
    }
}

impl From<&Tap> for barista_config::TapDecl {
    fn from(t: &Tap) -> Self {
        Self {
            name: t.name.clone(),
            url: t.url.to_string(),
            kind: t.kind.into(),
        }
    }
}

impl TryFrom<barista_config::TapDecl> for Tap {
    type Error = TapError;

    /// Re-validate a persisted [`TapDecl`] back into a domain [`Tap`].
    ///
    /// Validation runs on load too, so a hand-edited `barista.toml`
    /// with a malformed name or URL surfaces a clear error rather
    /// than a silently-broken tap.
    fn try_from(d: barista_config::TapDecl) -> Result<Self, Self::Error> {
        Tap::new(d.name, &d.url, d.kind.into())
    }
}

impl TapRegistry {
    /// Build a registry from persisted [`TapDecl`]s, re-validating
    /// each and rejecting duplicate names.
    ///
    /// # Errors
    ///
    /// Surfaces the first validation or duplicate-name error.
    pub fn from_decls(
        decls: impl IntoIterator<Item = barista_config::TapDecl>,
    ) -> Result<Self, TapError> {
        let mut reg = Self::new();
        for d in decls {
            reg.add(Tap::try_from(d)?)?;
        }
        Ok(reg)
    }

    /// Project the registry back to the persisted [`TapDecl`] shape,
    /// in insertion order, ready to hand to
    /// [`barista_config::save_taps`].
    #[must_use]
    pub fn to_decls(&self) -> Vec<barista_config::TapDecl> {
        self.taps.iter().map(barista_config::TapDecl::from).collect()
    }
}

// ============================================================
// Health probe
// ============================================================

/// The outcome of a [`probe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum TapHealth {
    /// The endpoint answered the liveness check.
    Healthy {
        /// A short human-readable detail (e.g. `HTTP 200`).
        detail: String,
    },
    /// The endpoint did not answer, or answered with an error.
    Unhealthy {
        /// Why the probe judged the tap unhealthy.
        reason: String,
    },
}

impl TapHealth {
    /// True for [`TapHealth::Healthy`].
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy { .. })
    }
}

/// Liveness-probe a single tap over HTTP, bounded by `timeout`.
///
/// Transport, by kind:
///
/// - [`TapKind::Roastery`]: `GET <url>/healthz`. The roastery's
///   unauthenticated SRE liveness endpoint returns `200 ok`. A `2xx`
///   is healthy; any other status (or a transport error) is
///   unhealthy.
/// - [`TapKind::Worker`]: a plain `HEAD <url>` liveness check. Any
///   response (even a `4xx`/`5xx`) proves the host is reachable and
///   is treated as healthy; only a transport-level failure (DNS,
///   connect, TLS, timeout) is unhealthy. This keeps the placeholder
///   worker probe a pure reachability signal — the worker protocol
///   is out of scope for v0.1.
///
/// The probe never panics and never hangs past `timeout`: a dead
/// endpoint resolves to [`TapHealth::Unhealthy`] within the bound.
pub async fn probe(tap: &Tap, timeout: Duration) -> TapHealth {
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            return TapHealth::Unhealthy {
                reason: format!("could not build HTTP client: {e}"),
            };
        }
    };

    match tap.kind {
        TapKind::Roastery => probe_roastery(&client, &tap.url).await,
        TapKind::Worker => probe_worker(&client, &tap.url).await,
    }
}

/// `GET <base>/healthz` — a `2xx` is healthy.
async fn probe_roastery(client: &reqwest::Client, base: &Url) -> TapHealth {
    // Join `/healthz` onto the base, preserving any path prefix the
    // operator configured. `Url::join` against a relative segment
    // replaces the final path component, so normalise to a trailing
    // slash first to treat the base as a directory.
    let health_url = match join_health_path(base) {
        Ok(u) => u,
        Err(reason) => return TapHealth::Unhealthy { reason },
    };

    tracing::debug!(url = %health_url, "probing roastery tap");
    match client.get(health_url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                TapHealth::Healthy {
                    detail: format!("HTTP {}", status.as_u16()),
                }
            } else {
                TapHealth::Unhealthy {
                    reason: format!("health endpoint returned HTTP {}", status.as_u16()),
                }
            }
        }
        Err(e) => TapHealth::Unhealthy {
            reason: classify_reqwest_error(&e),
        },
    }
}

/// `HEAD <url>` — any HTTP response means reachable (healthy); only a
/// transport error is unhealthy.
async fn probe_worker(client: &reqwest::Client, url: &Url) -> TapHealth {
    tracing::debug!(url = %url.as_str(), "probing worker tap");
    match client.head(url.clone()).send().await {
        Ok(resp) => TapHealth::Healthy {
            detail: format!("reachable (HTTP {})", resp.status().as_u16()),
        },
        Err(e) => TapHealth::Unhealthy {
            reason: classify_reqwest_error(&e),
        },
    }
}

/// Join `/healthz` onto a base URL, treating the base as a directory.
fn join_health_path(base: &Url) -> Result<Url, String> {
    let mut dir = base.clone();
    // Ensure a trailing slash so `join("healthz")` appends rather
    // than replacing the last path segment.
    if !dir.path().ends_with('/') {
        let with_slash = format!("{}/", dir.path());
        dir.set_path(&with_slash);
    }
    dir.join("healthz")
        .map_err(|e| format!("could not derive health URL from {base}: {e}"))
}

/// Turn a `reqwest::Error` into a short, user-facing reason string.
fn classify_reqwest_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        "connection refused / host unreachable".to_string()
    } else {
        // Strip the (verbose) URL prefix reqwest sometimes attaches;
        // the caller already knows the URL.
        format!("request failed: {e}")
    }
}

// ============================================================
// Errors
// ============================================================

/// Errors raised by tap registration, validation, and bridging.
///
/// Probe outcomes are *not* errors — a dead tap is a normal
/// [`TapHealth::Unhealthy`] result, not a `TapError`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TapError {
    /// A tap with this name is already registered.
    #[error("a tap named {name:?} is already registered")]
    DuplicateName {
        /// The conflicting name.
        name: String,
    },

    /// No tap with this name is registered.
    #[error("no tap named {name:?} is registered")]
    NotFound {
        /// The name that was looked up.
        name: String,
    },

    /// The tap name failed validation.
    #[error("invalid tap name {name:?}: {reason}")]
    InvalidName {
        /// The offending name.
        name: String,
        /// Why it was rejected.
        reason: String,
    },

    /// The tap URL failed validation.
    #[error("invalid tap url {url:?}: {reason}")]
    InvalidUrl {
        /// The offending URL.
        url: String,
        /// Why it was rejected.
        reason: String,
    },

    /// An unrecognised tap kind string (not `roastery` / `worker`).
    #[error("invalid tap kind {value:?}: expected \"roastery\" or \"worker\"")]
    InvalidKind {
        /// The offending kind string.
        value: String,
    },

    /// An I/O error while loading or persisting taps. Wraps the
    /// persistence error from `barista-config` as a string so this
    /// crate's error type stays `PartialEq` / `Eq` for tests.
    #[error("tap persistence: {0}")]
    Io(String),
}

impl From<barista_config::TapPersistError> for TapError {
    fn from(e: barista_config::TapPersistError) -> Self {
        Self::Io(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::str::FromStr;

    fn roastery(name: &str, url: &str) -> Tap {
        Tap::new(name, url, TapKind::Roastery).unwrap()
    }

    // ---------- validation ----------

    #[test]
    fn rejects_empty_name() {
        let err = Tap::new("", "https://r.example", TapKind::Roastery).unwrap_err();
        assert!(matches!(err, TapError::InvalidName { .. }));
    }

    #[test]
    fn rejects_name_with_illegal_chars() {
        for bad in ["has space", "slash/y", "ampers&nd", "uni\u{00e9}code"] {
            let err = Tap::new(bad, "https://r.example", TapKind::Roastery).unwrap_err();
            assert!(matches!(err, TapError::InvalidName { .. }), "name {bad:?}");
        }
    }

    #[test]
    fn accepts_valid_names() {
        for ok in ["acme", "ACME", "a.b_c-1", "0", "roastery.east-1"] {
            assert!(Tap::new(ok, "https://r.example", TapKind::Roastery).is_ok());
        }
    }

    #[test]
    fn rejects_relative_and_non_http_urls() {
        for bad in ["not a url", "ftp://r.example", "/relative/path", "file:///etc"] {
            let err = Tap::new("ok", bad, TapKind::Roastery).unwrap_err();
            assert!(matches!(err, TapError::InvalidUrl { .. }), "url {bad:?}");
        }
    }

    #[test]
    fn accepts_http_and_https() {
        assert!(Tap::new("a", "http://r.example:8080", TapKind::Worker).is_ok());
        assert!(Tap::new("b", "https://r.example/prefix", TapKind::Roastery).is_ok());
    }

    #[test]
    fn kind_round_trips_through_str() {
        assert_eq!(TapKind::from_str("roastery").unwrap(), TapKind::Roastery);
        assert_eq!(TapKind::from_str("worker").unwrap(), TapKind::Worker);
        assert!(matches!(
            TapKind::from_str("nope"),
            Err(TapError::InvalidKind { .. })
        ));
        assert_eq!(TapKind::Roastery.as_str(), "roastery");
    }

    // ---------- registry ----------

    #[test]
    fn add_rejects_duplicate_names() {
        let mut reg = TapRegistry::new();
        reg.add(roastery("a", "https://a.example")).unwrap();
        let err = reg.add(roastery("a", "https://b.example")).unwrap_err();
        assert_eq!(
            err,
            TapError::DuplicateName {
                name: "a".to_string()
            }
        );
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn remove_is_idempotent() {
        let mut reg = TapRegistry::new();
        reg.add(roastery("a", "https://a.example")).unwrap();
        // First remove: present.
        assert!(reg.remove("a").unwrap());
        // Second remove: absent, but a clean `false`, not an error.
        assert!(!reg.remove("a").unwrap());
        // Removing a never-registered tap is also a clean no-op.
        assert!(!reg.remove("never").unwrap());
        assert!(reg.is_empty());
    }

    #[test]
    fn list_preserves_insertion_order() {
        let mut reg = TapRegistry::new();
        reg.add(roastery("first", "https://1.example")).unwrap();
        reg.add(roastery("second", "https://2.example")).unwrap();
        reg.add(roastery("third", "https://3.example")).unwrap();
        let names: Vec<_> = reg.list().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["first", "second", "third"]);
    }

    #[test]
    fn get_finds_by_name() {
        let mut reg = TapRegistry::new();
        reg.add(roastery("a", "https://a.example")).unwrap();
        assert!(reg.get("a").is_some());
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn from_taps_rejects_duplicates() {
        let err = TapRegistry::from_taps([
            roastery("dup", "https://1.example"),
            roastery("dup", "https://2.example"),
        ])
        .unwrap_err();
        assert!(matches!(err, TapError::DuplicateName { .. }));
    }

    // ---------- bridge ----------

    #[test]
    fn decl_round_trip_preserves_fields() {
        let mut reg = TapRegistry::new();
        reg.add(roastery("a", "https://a.example/")).unwrap();
        reg.add(Tap::new("w", "http://w.example:9000", TapKind::Worker).unwrap())
            .unwrap();

        let decls = reg.to_decls();
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].name, "a");
        assert_eq!(decls[1].kind, barista_config::TapKindDecl::Worker);

        let back = TapRegistry::from_decls(decls).unwrap();
        assert_eq!(back, reg);
    }

    #[test]
    fn from_decls_revalidates() {
        // A hand-edited config with a bad URL surfaces on load.
        let bad = barista_config::TapDecl {
            name: "x".to_string(),
            url: "not-a-url".to_string(),
            kind: barista_config::TapKindDecl::Roastery,
        };
        let err = TapRegistry::from_decls([bad]).unwrap_err();
        assert!(matches!(err, TapError::InvalidUrl { .. }));
    }

    // ---------- health-url derivation ----------

    #[test]
    fn join_health_path_appends_to_bare_host() {
        let base = Url::parse("https://r.example").unwrap();
        assert_eq!(
            join_health_path(&base).unwrap().as_str(),
            "https://r.example/healthz"
        );
    }

    #[test]
    fn join_health_path_preserves_prefix() {
        let base = Url::parse("https://r.example/cache").unwrap();
        assert_eq!(
            join_health_path(&base).unwrap().as_str(),
            "https://r.example/cache/healthz"
        );
    }
}
