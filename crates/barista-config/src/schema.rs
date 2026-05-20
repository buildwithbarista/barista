// SPDX-License-Identifier: MIT OR Apache-2.0

//! Effective and partial configuration schemas.
//!
//! [`Config`] is the fully-merged result handed to callers. The
//! `Partial*` mirror types are what TOML files deserialize into;
//! every field is wrapped in `Option` so an absent key in a config
//! file means "inherit from the prior layer", not "reset to
//! default".

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------- Effective config ----------

/// Effective Barista configuration (fully merged across all
/// layers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    pub paths: PathsConfig,
    pub network: NetworkConfig,
    pub daemon: DaemonConfig,
    pub maven: MavenConfig,
    pub logging: LoggingConfig,
    pub telemetry: TelemetryConfig,
    pub compat: CompatConfig,
    /// Local artifact cache configuration, including any optional
    /// roastery (remote shared cache) the cache layer should consult
    /// before falling back to upstream Maven repositories. Defaults
    /// to "no roastery configured" — degrades silently to upstream
    /// fetches on every miss.
    #[serde(default)]
    pub cache: CacheConfig,
    /// Project-local extensions (taps, per-module overrides,
    /// plugin classloader-cache policy, project metadata) parsed
    /// out of `barista.toml`. `None` when no project file was
    /// found or the file omitted all extension sections.
    ///
    /// User-level config (`~/.barista/config.toml`) cannot
    /// populate this field — these settings only make sense
    /// project-local.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_extensions: Option<crate::barista_toml::BaristaTomlExtensions>,

    /// Parsed Maven `settings.xml` content (servers, mirrors,
    /// profiles, proxies, plugin groups). Populated by the
    /// settings.xml layer of the loader and consumed by downstream
    /// components (resolver, network layer). Not part of Barista's
    /// own TOML schema.
    #[serde(skip)]
    pub maven_settings: crate::settings_xml::SettingsXml,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PathsConfig {
    /// Local cache root. Default: `~/.barista/cache`.
    pub cache_dir: PathBuf,
    /// User-level config dir. Default: `~/.barista`.
    pub user_config_dir: PathBuf,
    /// Path to `~/.m2/settings.xml`.
    pub settings_xml: PathBuf,
    /// Path to `~/.m2/repository`.
    pub m2_repository: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct NetworkConfig {
    /// Peak concurrent HTTP connections. Default: 6.
    pub max_concurrent_connections: u32,
    /// Per-request timeout (seconds). Default: 60.
    pub request_timeout_secs: u32,
    /// Whether to use HTTP/2 connection pooling. Default: true.
    pub http2_enabled: bool,
    /// Proxy URL. None means "no proxy".
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DaemonConfig {
    /// Whether to use the daemon. Default: true.
    pub enabled: bool,
    /// Idle shutdown after this many seconds. Default: 600.
    pub idle_shutdown_secs: u32,
    /// Max heap size (e.g. "2g"). None means JVM default.
    pub max_heap: Option<String>,
    /// Path to socket dir. Default: `~/.barista/run`.
    pub socket_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MavenConfig {
    /// Compat mode: 3.9 / 4.0 / auto. Default: auto.
    pub compat_mode: CompatMode,
    /// If true, honor `.mvn/maven.config` properties. Default: true.
    pub honor_mvn_config: bool,
    /// If true, honor `.mvn/jvm.config` JVM args. Default: true.
    pub honor_jvm_config: bool,
    /// Update policy for SNAPSHOT artifacts and repository metadata.
    /// Default: [`UpdatePolicy::Daily`] (matches Maven's default).
    #[serde(default = "MavenConfig::default_snapshot_update_policy")]
    pub snapshot_update_policy: UpdatePolicy,
    /// Update policy for non-SNAPSHOT (release) artifacts. Default:
    /// [`UpdatePolicy::Never`] — releases are immutable, so the cache
    /// is authoritative once populated.
    #[serde(default = "MavenConfig::default_release_update_policy")]
    pub release_update_policy: UpdatePolicy,
}

impl MavenConfig {
    fn default_snapshot_update_policy() -> UpdatePolicy {
        UpdatePolicy::Daily
    }
    fn default_release_update_policy() -> UpdatePolicy {
        UpdatePolicy::Never
    }
}

/// Maven update policy, parsed from `<updatePolicy>` in `settings.xml`
/// or from a CLI flag.
///
/// Per Maven semantics:
///
/// * `always`         — re-fetch metadata on every build.
/// * `daily`          — re-fetch if the local cached copy is more
///   than 24 hours old. This is Maven's default.
/// * `interval:N`     — re-fetch if the local cached copy is more
///   than `N` minutes old.
/// * `never`          — never re-fetch unless the build was invoked
///   with `--update` (Maven's `-U`).
///
/// Lives in `barista-config` rather than `barista-resolver` to keep
/// the dependency edge one-way: the resolver depends on config, not
/// the other way around.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdatePolicy {
    /// Re-fetch on every build.
    Always,
    /// Re-fetch if the local cached copy is more than 24 hours old.
    Daily,
    /// Re-fetch if the local cached copy is more than `minutes`
    /// minutes old.
    Interval { minutes: u32 },
    /// Never re-fetch unless `--update` is passed on the CLI.
    Never,
}

impl Default for UpdatePolicy {
    fn default() -> Self {
        Self::Daily
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompatMode {
    ThreeNine,
    FourZero,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LoggingConfig {
    /// Default verbosity. Default: "info".
    pub level: String,
    /// If true, emit Maven-shaped logs alongside structured logs.
    pub maven_shape: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct TelemetryConfig {
    /// Telemetry opt-in. Default: `false`. The CLI never flips
    /// this implicitly; it must be set by the user (config file or
    /// the `BARISTA_TELEMETRY__ENABLED=1` env override).
    pub enabled: bool,
    /// Endpoint URL the transport posts to. `None` means "no
    /// transport configured" — even when `enabled = true` the
    /// no-op `NullSink` is used. The actual HTTP transport lands
    /// in a later milestone.
    pub endpoint: Option<String>,
    /// Stable opaque per-install identifier. When `None` (the
    /// default), no per-install ID is attached to outgoing events.
    /// Operators who want to correlate events across runs can pin
    /// a value via `~/.barista/config.toml`.
    pub client_id: Option<String>,
    /// Master switch for the HTTP transport. Default: `false`.
    ///
    /// This is the **third independent guard** on outbound network
    /// traffic — `enabled`, `endpoint.is_some()`, and
    /// `transport-enabled` must **all** be true before any HTTP
    /// request leaves the process. It exists so the privacy
    /// posture (what we send, where, when) can be reviewed and
    /// signed off before the transport is allowed to fire, even
    /// for users who have set `enabled = true` and configured an
    /// endpoint. Flip this on once the privacy doc lands and the
    /// "v0.2" go-live is approved.
    pub transport_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct CompatConfig {
    /// Modules excluded from default builds.
    pub excluded_modules: Vec<String>,
}

/// Local-cache configuration.
///
/// Currently carries only the optional [`RoasteryConfig`] section
/// pointing at a shared remote cache. When `roastery` is `None`
/// (the default), the cache layer behaves exactly as before: on a
/// local miss, fetch directly from the configured upstream Maven
/// repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct CacheConfig {
    /// Optional remote roastery the cache layer consults before
    /// falling back to upstream. See [`RoasteryConfig`] for the wire
    /// shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roastery: Option<RoasteryConfig>,
}

/// Configuration for a single remote roastery cache server.
///
/// Resolved-form representation: bearer tokens have already been
/// pulled out of their environment variable so the cache layer can
/// hand the secret straight to the roastery client without re-doing
/// the env-var dance. mTLS material is still represented as a
/// filesystem path because the rustls-side loader reads the files
/// directly.
///
/// # TOML wire shape
///
/// The on-disk form lives under `[cache.roastery]` in
/// `~/.barista/config.toml` or `./barista.toml`:
///
/// ```toml
/// [cache.roastery]
/// url = "https://roastery.example.com:8443"
/// # Bearer token — read from the named env var at config-load
/// # time. Never inline the token in TOML per the secrets policy.
/// auth-token-env = "ROASTERY_TOKEN"
/// # Alternatively, mTLS:
/// # mtls-client-cert = "/etc/barista/client.pem"
/// # mtls-client-key  = "/etc/barista/client.key"
/// # Optional custom CA bundle; omit for the system trust store.
/// tls-ca = "/etc/barista/ca.pem"
/// # If true, T3's push-after-build path will upload locally-built
/// # artifacts to the roastery. T2 defines the field; T3 wires the
/// # use site.
/// push = false
/// timeout-secs = 30
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RoasteryConfig {
    /// Base URL the client points at, e.g.
    /// `https://roastery.example.com:8443`. Required.
    pub url: String,
    /// How the client authenticates against the roastery.
    pub auth: RoasteryAuth,
    /// How the client validates the roastery's TLS certificate.
    pub tls: RoasteryTls,
    /// Per-request timeout in seconds. Default: 30.
    #[serde(default = "RoasteryConfig::default_timeout_secs")]
    pub timeout_secs: u32,
    /// Whether the cache layer should push locally-built artifacts
    /// up to the roastery after a successful build.
    ///
    /// T2 defines this field with a `false` default but does NOT
    /// consume it — the upload path lands in T3. Recorded here so
    /// the configuration shape is stable across the two milestones
    /// and operators can opt in once T3 ships.
    #[serde(default)]
    pub push: bool,
}

impl RoasteryConfig {
    fn default_timeout_secs() -> u32 {
        30
    }
}

/// How the roastery client authenticates.
///
/// `Anonymous` is the right pick when the roastery is unauthenticated
/// (typical for local development); `Bearer` carries a token resolved
/// from an environment variable (the variable name is captured in
/// the partial-config form and the actual token is materialised
/// during `apply_to`); `Mtls` carries paths to PEM-encoded client
/// material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RoasteryAuth {
    /// Send no credentials.
    Anonymous,
    /// Send `Authorization: Bearer <token>` on every protected
    /// request. The token lives in this struct in plaintext at
    /// runtime — it was sourced from an environment variable named
    /// in the TOML's `auth-token-env` field (so the secret never
    /// touches disk via the config file).
    Bearer {
        /// The resolved bearer token. Never logged.
        token: String,
    },
    /// Mutual TLS — present the supplied client certificate and key
    /// during the TLS handshake.
    Mtls {
        /// Path to a PEM-encoded client certificate (leaf first;
        /// intermediates, if any, follow).
        client_cert_pem_path: PathBuf,
        /// Path to the PEM-encoded private key matching the leaf
        /// certificate.
        client_key_pem_path: PathBuf,
    },
}

impl Default for RoasteryAuth {
    fn default() -> Self {
        Self::Anonymous
    }
}

/// How the roastery client validates the server's TLS certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RoasteryTls {
    /// Use the operating-system trust store.
    SystemRoots,
    /// Verify against a caller-supplied CA bundle.
    CustomCa {
        /// Path to a PEM file containing the trusted CA
        /// certificate(s).
        ca_cert_pem_path: PathBuf,
    },
    /// No TLS — plain HTTP. Refused at client-construction time
    /// against an `https://` base URL.
    PlainHttp,
}

impl Default for RoasteryTls {
    fn default() -> Self {
        Self::SystemRoots
    }
}

// ---------- Defaults ----------
//
// Path defaults are stored with a literal leading `~`; the loader
// expands them against the resolved HOME directory after defaults
// are constructed. This keeps `Config::default()` independent of
// the process environment and trivially `const`-shaped for tests.

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            cache_dir: PathBuf::from("~/.barista/cache"),
            user_config_dir: PathBuf::from("~/.barista"),
            settings_xml: PathBuf::from("~/.m2/settings.xml"),
            m2_repository: PathBuf::from("~/.m2/repository"),
        }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            max_concurrent_connections: 6,
            request_timeout_secs: 60,
            http2_enabled: true,
            proxy: None,
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_shutdown_secs: 600,
            max_heap: None,
            socket_dir: PathBuf::from("~/.barista/run"),
        }
    }
}

impl Default for MavenConfig {
    fn default() -> Self {
        Self {
            compat_mode: CompatMode::Auto,
            honor_mvn_config: true,
            honor_jvm_config: true,
            snapshot_update_policy: UpdatePolicy::Daily,
            release_update_policy: UpdatePolicy::Never,
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            maven_shape: false,
        }
    }
}

// ---------- Partial layer types ----------

/// Partial config — what a TOML file deserializes into. Every
/// field is `Option`; absent fields inherit from prior layers.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PartialConfig {
    pub paths: Option<PartialPathsConfig>,
    pub network: Option<PartialNetworkConfig>,
    pub daemon: Option<PartialDaemonConfig>,
    pub maven: Option<PartialMavenConfig>,
    pub logging: Option<PartialLoggingConfig>,
    pub telemetry: Option<PartialTelemetryConfig>,
    pub compat: Option<PartialCompatConfig>,
    pub cache: Option<PartialCacheConfig>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PartialPathsConfig {
    pub cache_dir: Option<PathBuf>,
    pub user_config_dir: Option<PathBuf>,
    pub settings_xml: Option<PathBuf>,
    pub m2_repository: Option<PathBuf>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PartialNetworkConfig {
    pub max_concurrent_connections: Option<u32>,
    pub request_timeout_secs: Option<u32>,
    pub http2_enabled: Option<bool>,
    pub proxy: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PartialDaemonConfig {
    pub enabled: Option<bool>,
    pub idle_shutdown_secs: Option<u32>,
    pub max_heap: Option<String>,
    pub socket_dir: Option<PathBuf>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PartialMavenConfig {
    pub compat_mode: Option<CompatMode>,
    pub honor_mvn_config: Option<bool>,
    pub honor_jvm_config: Option<bool>,
    pub snapshot_update_policy: Option<UpdatePolicy>,
    pub release_update_policy: Option<UpdatePolicy>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PartialLoggingConfig {
    pub level: Option<String>,
    pub maven_shape: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PartialTelemetryConfig {
    pub enabled: Option<bool>,
    pub endpoint: Option<String>,
    pub client_id: Option<String>,
    pub transport_enabled: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PartialCompatConfig {
    pub excluded_modules: Option<Vec<String>>,
}

/// Partial form of [`CacheConfig`] (TOML-facing).
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PartialCacheConfig {
    pub roastery: Option<PartialRoasteryConfig>,
}

/// Partial form of [`RoasteryConfig`] (TOML-facing).
///
/// The bearer-token-bearing form is the *name of an environment
/// variable*, not the token itself, per the secrets policy: an
/// operator-friendly TOML file should never need to contain a live
/// credential. The variable name flows through `apply_to`, which
/// resolves it via [`PartialRoasteryConfig::resolve_auth`] using a
/// caller-supplied env lookup so unit tests can avoid touching the
/// process environment.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PartialRoasteryConfig {
    /// Base URL of the roastery, e.g.
    /// `https://roastery.example.com:8443`.
    pub url: Option<String>,
    /// Name of the environment variable carrying a bearer token.
    /// When present, the auth mechanism resolves to
    /// [`RoasteryAuth::Bearer`]; the literal token is sourced from
    /// the process environment at config-load time.
    pub auth_token_env: Option<String>,
    /// Path to a PEM-encoded mTLS client certificate. When set
    /// alongside `mtls_client_key`, the auth mechanism resolves to
    /// [`RoasteryAuth::Mtls`].
    pub mtls_client_cert: Option<PathBuf>,
    /// Path to a PEM-encoded mTLS client private key.
    pub mtls_client_key: Option<PathBuf>,
    /// Path to a PEM-encoded CA bundle the client should trust for
    /// server-cert verification. When absent, the OS trust store is
    /// used. Setting `tls_ca = ""` is treated as absent.
    pub tls_ca: Option<PathBuf>,
    /// Force plain HTTP — only legal against an `http://` base URL.
    /// Mostly useful for development; production deployments should
    /// leave this `false` and rely on TLS.
    #[serde(default)]
    pub plain_http: Option<bool>,
    /// Per-request timeout in seconds.
    pub timeout_secs: Option<u32>,
    /// Whether the cache layer should push locally-built artifacts
    /// up to the roastery after a successful build. T3 will wire
    /// the use site; T2 keeps the field plumbed so configs stay
    /// stable.
    pub push: Option<bool>,
}

/// Error returned when a [`PartialRoasteryConfig`] section is
/// internally inconsistent — e.g. neither a bearer-token env var
/// nor a complete mTLS pair is supplied.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RoasteryConfigError {
    /// `mtls-client-cert` and `mtls-client-key` must be set
    /// together — supplying one without the other is rejected.
    #[error(
        "[cache.roastery]: mtls-client-cert and mtls-client-key must be set together \
         (got cert={cert_set}, key={key_set})"
    )]
    MtlsPairIncomplete { cert_set: bool, key_set: bool },
    /// `auth-token-env` and `mtls-client-cert`/`mtls-client-key`
    /// are mutually exclusive — a single roastery can authenticate
    /// in only one mode at a time.
    #[error(
        "[cache.roastery]: auth-token-env and mtls-client-* are mutually exclusive — pick one"
    )]
    AmbiguousAuth,
    /// `auth-token-env` named an environment variable that wasn't
    /// set (or was empty).
    #[error("[cache.roastery]: env var {name:?} is not set or is empty")]
    MissingTokenEnv { name: String },
    /// `url` was missing.
    #[error("[cache.roastery]: url is required")]
    MissingUrl,
}

impl PartialRoasteryConfig {
    /// Resolve the partial section into a full [`RoasteryConfig`].
    ///
    /// `env_lookup` is invoked exactly when `auth_token_env` is set;
    /// it returns the resolved token string (or `None` if the
    /// variable is unset). Tests inject a closure that consults a
    /// `HashMap` so they don't touch the real process environment.
    pub fn resolve(
        &self,
        env_lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<RoasteryConfig, RoasteryConfigError> {
        let url = self.url.clone().ok_or(RoasteryConfigError::MissingUrl)?;

        let has_token = self.auth_token_env.is_some();
        let cert_set = self.mtls_client_cert.is_some();
        let key_set = self.mtls_client_key.is_some();
        if has_token && (cert_set || key_set) {
            return Err(RoasteryConfigError::AmbiguousAuth);
        }
        if cert_set != key_set {
            return Err(RoasteryConfigError::MtlsPairIncomplete { cert_set, key_set });
        }

        let auth = if has_token {
            let name = self.auth_token_env.clone().unwrap_or_default();
            let token = env_lookup(&name)
                .filter(|s| !s.is_empty())
                .ok_or(RoasteryConfigError::MissingTokenEnv { name })?;
            RoasteryAuth::Bearer { token }
        } else if cert_set && key_set {
            RoasteryAuth::Mtls {
                client_cert_pem_path: self.mtls_client_cert.clone().unwrap_or_default(),
                client_key_pem_path: self.mtls_client_key.clone().unwrap_or_default(),
            }
        } else {
            RoasteryAuth::Anonymous
        };

        let tls = match self.tls_ca.as_ref() {
            Some(p) if !p.as_os_str().is_empty() => RoasteryTls::CustomCa {
                ca_cert_pem_path: p.clone(),
            },
            _ if self.plain_http.unwrap_or(false) => RoasteryTls::PlainHttp,
            _ => RoasteryTls::SystemRoots,
        };

        Ok(RoasteryConfig {
            url,
            auth,
            tls,
            timeout_secs: self.timeout_secs.unwrap_or(30),
            push: self.push.unwrap_or(false),
        })
    }
}

impl PartialConfig {
    /// Apply this partial onto an effective [`Config`], returning
    /// the dotted field paths that were set (used by
    /// [`LoadAudit`](crate::sources::LoadAudit)).
    pub fn apply_to(&self, target: &mut Config) -> Vec<String> {
        let mut touched = Vec::new();
        if let Some(p) = &self.paths {
            if let Some(v) = &p.cache_dir {
                target.paths.cache_dir = v.clone();
                touched.push("paths.cache-dir".into());
            }
            if let Some(v) = &p.user_config_dir {
                target.paths.user_config_dir = v.clone();
                touched.push("paths.user-config-dir".into());
            }
            if let Some(v) = &p.settings_xml {
                target.paths.settings_xml = v.clone();
                touched.push("paths.settings-xml".into());
            }
            if let Some(v) = &p.m2_repository {
                target.paths.m2_repository = v.clone();
                touched.push("paths.m2-repository".into());
            }
        }
        if let Some(n) = &self.network {
            if let Some(v) = n.max_concurrent_connections {
                target.network.max_concurrent_connections = v;
                touched.push("network.max-concurrent-connections".into());
            }
            if let Some(v) = n.request_timeout_secs {
                target.network.request_timeout_secs = v;
                touched.push("network.request-timeout-secs".into());
            }
            if let Some(v) = n.http2_enabled {
                target.network.http2_enabled = v;
                touched.push("network.http2-enabled".into());
            }
            if let Some(v) = &n.proxy {
                target.network.proxy = Some(v.clone());
                touched.push("network.proxy".into());
            }
        }
        if let Some(d) = &self.daemon {
            if let Some(v) = d.enabled {
                target.daemon.enabled = v;
                touched.push("daemon.enabled".into());
            }
            if let Some(v) = d.idle_shutdown_secs {
                target.daemon.idle_shutdown_secs = v;
                touched.push("daemon.idle-shutdown-secs".into());
            }
            if let Some(v) = &d.max_heap {
                target.daemon.max_heap = Some(v.clone());
                touched.push("daemon.max-heap".into());
            }
            if let Some(v) = &d.socket_dir {
                target.daemon.socket_dir = v.clone();
                touched.push("daemon.socket-dir".into());
            }
        }
        if let Some(m) = &self.maven {
            if let Some(v) = m.compat_mode {
                target.maven.compat_mode = v;
                touched.push("maven.compat-mode".into());
            }
            if let Some(v) = m.honor_mvn_config {
                target.maven.honor_mvn_config = v;
                touched.push("maven.honor-mvn-config".into());
            }
            if let Some(v) = m.honor_jvm_config {
                target.maven.honor_jvm_config = v;
                touched.push("maven.honor-jvm-config".into());
            }
            if let Some(v) = m.snapshot_update_policy {
                target.maven.snapshot_update_policy = v;
                touched.push("maven.snapshot-update-policy".into());
            }
            if let Some(v) = m.release_update_policy {
                target.maven.release_update_policy = v;
                touched.push("maven.release-update-policy".into());
            }
        }
        if let Some(l) = &self.logging {
            if let Some(v) = &l.level {
                target.logging.level = v.clone();
                touched.push("logging.level".into());
            }
            if let Some(v) = l.maven_shape {
                target.logging.maven_shape = v;
                touched.push("logging.maven-shape".into());
            }
        }
        if let Some(t) = &self.telemetry {
            if let Some(v) = t.enabled {
                target.telemetry.enabled = v;
                touched.push("telemetry.enabled".into());
            }
            if let Some(v) = &t.endpoint {
                target.telemetry.endpoint = Some(v.clone());
                touched.push("telemetry.endpoint".into());
            }
            if let Some(v) = &t.client_id {
                target.telemetry.client_id = Some(v.clone());
                touched.push("telemetry.client-id".into());
            }
            if let Some(v) = t.transport_enabled {
                target.telemetry.transport_enabled = v;
                touched.push("telemetry.transport-enabled".into());
            }
        }
        if let Some(c) = &self.compat {
            if let Some(v) = &c.excluded_modules {
                target.compat.excluded_modules = v.clone();
                touched.push("compat.excluded-modules".into());
            }
        }
        if let Some(c) = &self.cache {
            if let Some(r) = &c.roastery {
                // Resolve the section against the live process
                // environment. Failures are logged at debug level
                // and silently dropped so a misconfigured roastery
                // section can't take a build offline — the loader's
                // explicit `validate_cache` entry point gives
                // callers the strict path when they want it.
                match r.resolve(|name| std::env::var(name).ok()) {
                    Ok(resolved) => {
                        target.cache.roastery = Some(resolved);
                        touched.push("cache.roastery".into());
                    }
                    Err(e) => {
                        tracing::debug!(
                            error = %e,
                            "[cache.roastery] section ignored due to resolution error"
                        );
                    }
                }
            }
        }
        touched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maven_config_default_update_policies() {
        let m = MavenConfig::default();
        assert_eq!(m.snapshot_update_policy, UpdatePolicy::Daily);
        assert_eq!(m.release_update_policy, UpdatePolicy::Never);
    }

    #[test]
    fn update_policy_toml_roundtrip_simple() {
        let toml_src = r#"
[maven]
compat-mode = "auto"
honor-mvn-config = true
honor-jvm-config = true
snapshot-update-policy = "always"
release-update-policy = "never"
"#;
        let p: PartialConfig = toml::from_str(toml_src).expect("parse");
        let m = p.maven.clone().expect("maven section");
        assert_eq!(m.snapshot_update_policy, Some(UpdatePolicy::Always));
        assert_eq!(m.release_update_policy, Some(UpdatePolicy::Never));

        let mut cfg = Config::default();
        let touched = p.apply_to(&mut cfg);
        assert!(touched.iter().any(|t| t == "maven.snapshot-update-policy"));
        assert!(touched.iter().any(|t| t == "maven.release-update-policy"));
        assert_eq!(cfg.maven.snapshot_update_policy, UpdatePolicy::Always);
        assert_eq!(cfg.maven.release_update_policy, UpdatePolicy::Never);
    }

    #[test]
    fn update_policy_toml_roundtrip_interval() {
        // The Interval variant is externally-tagged: its TOML
        // representation is `[interval] minutes = 30` inline as a
        // table. Test the round-trip via toml.
        let original = UpdatePolicy::Interval { minutes: 30 };
        let s = toml::to_string(&original).expect("ser");
        let back: UpdatePolicy = toml::from_str(&s).expect("de");
        assert_eq!(back, original);
    }

    #[test]
    fn update_policy_default_is_daily() {
        assert_eq!(UpdatePolicy::default(), UpdatePolicy::Daily);
    }

    // ---------- [T] roastery / cache config ----------

    /// [T] `config_parses_roastery_section_with_bearer_token_env`
    #[test]
    fn config_parses_roastery_section_with_bearer_token_env() {
        let toml_src = r#"
[cache.roastery]
url = "https://roastery.example.com:8443"
auth-token-env = "ROASTERY_TOKEN"
timeout-secs = 15
push = true
"#;
        let p: PartialConfig = toml::from_str(toml_src).expect("parse");
        let cache = p.cache.expect("cache section");
        let r = cache.roastery.expect("roastery section");
        assert_eq!(r.url.as_deref(), Some("https://roastery.example.com:8443"));
        assert_eq!(r.auth_token_env.as_deref(), Some("ROASTERY_TOKEN"));
        assert_eq!(r.timeout_secs, Some(15));
        assert_eq!(r.push, Some(true));

        // Resolve using an injected env (no process env touched).
        let resolved = r
            .resolve(|name| {
                if name == "ROASTERY_TOKEN" {
                    Some("hunter2".to_string())
                } else {
                    None
                }
            })
            .expect("resolve");
        assert_eq!(resolved.url, "https://roastery.example.com:8443");
        match &resolved.auth {
            RoasteryAuth::Bearer { token } => assert_eq!(token, "hunter2"),
            other => panic!("expected Bearer, got {other:?}"),
        }
        assert!(matches!(resolved.tls, RoasteryTls::SystemRoots));
        assert_eq!(resolved.timeout_secs, 15);
        assert!(resolved.push);
    }

    /// [T] `config_parses_roastery_section_with_mtls`
    #[test]
    fn config_parses_roastery_section_with_mtls() {
        let toml_src = r#"
[cache.roastery]
url = "https://roastery.example.com:8443"
mtls-client-cert = "/etc/barista/client.pem"
mtls-client-key = "/etc/barista/client.key"
tls-ca = "/etc/barista/ca.pem"
"#;
        let p: PartialConfig = toml::from_str(toml_src).expect("parse");
        let r = p.cache.unwrap().roastery.unwrap();
        let resolved = r.resolve(|_| None).expect("resolve");
        match &resolved.auth {
            RoasteryAuth::Mtls {
                client_cert_pem_path,
                client_key_pem_path,
            } => {
                assert_eq!(
                    client_cert_pem_path,
                    &PathBuf::from("/etc/barista/client.pem")
                );
                assert_eq!(
                    client_key_pem_path,
                    &PathBuf::from("/etc/barista/client.key")
                );
            }
            other => panic!("expected Mtls, got {other:?}"),
        }
        match &resolved.tls {
            RoasteryTls::CustomCa { ca_cert_pem_path } => {
                assert_eq!(ca_cert_pem_path, &PathBuf::from("/etc/barista/ca.pem"));
            }
            other => panic!("expected CustomCa, got {other:?}"),
        }
        // Default timeout + push when omitted.
        assert_eq!(resolved.timeout_secs, 30);
        assert!(!resolved.push);
    }

    /// [T] `config_without_roastery_section_yields_none`
    #[test]
    fn config_without_roastery_section_yields_none() {
        // An otherwise-valid config without [cache.roastery] must
        // still parse and the resolved cache.roastery must be None.
        let toml_src = r#"
[maven]
compat-mode = "auto"
honor-mvn-config = true
honor-jvm-config = true
"#;
        let p: PartialConfig = toml::from_str(toml_src).expect("parse");
        assert!(p.cache.is_none());
        let mut cfg = Config::default();
        let _ = p.apply_to(&mut cfg);
        assert!(cfg.cache.roastery.is_none());
    }

    #[test]
    fn roastery_resolve_rejects_ambiguous_auth() {
        let p = PartialRoasteryConfig {
            url: Some("http://r".into()),
            auth_token_env: Some("X".into()),
            mtls_client_cert: Some("/c".into()),
            mtls_client_key: Some("/k".into()),
            ..Default::default()
        };
        let err = p
            .resolve(|_| Some("t".to_string()))
            .expect_err("should reject");
        assert_eq!(err, RoasteryConfigError::AmbiguousAuth);
    }

    #[test]
    fn roastery_resolve_rejects_partial_mtls_pair() {
        let p = PartialRoasteryConfig {
            url: Some("http://r".into()),
            mtls_client_cert: Some("/c".into()),
            ..Default::default()
        };
        let err = p.resolve(|_| None).expect_err("should reject");
        assert!(matches!(err, RoasteryConfigError::MtlsPairIncomplete { .. }));
    }

    #[test]
    fn roastery_resolve_rejects_missing_token_env() {
        let p = PartialRoasteryConfig {
            url: Some("http://r".into()),
            auth_token_env: Some("DEFINITELY_NOT_SET_QQ".into()),
            ..Default::default()
        };
        let err = p.resolve(|_| None).expect_err("should reject");
        assert!(matches!(err, RoasteryConfigError::MissingTokenEnv { .. }));
    }

    #[test]
    fn roastery_resolve_plain_http_only_when_explicit_and_no_ca() {
        let p = PartialRoasteryConfig {
            url: Some("http://r".into()),
            plain_http: Some(true),
            ..Default::default()
        };
        let resolved = p.resolve(|_| None).expect("ok");
        assert!(matches!(resolved.tls, RoasteryTls::PlainHttp));
    }

    #[test]
    fn roastery_resolve_anonymous_by_default() {
        let p = PartialRoasteryConfig {
            url: Some("http://r".into()),
            ..Default::default()
        };
        let resolved = p.resolve(|_| None).expect("ok");
        assert!(matches!(resolved.auth, RoasteryAuth::Anonymous));
    }

    #[test]
    fn apply_to_wires_resolved_roastery_into_effective_config() {
        // `apply_to` resolves the section against the *live* process
        // environment, so to keep this test hermetic (and to avoid the
        // `unsafe` env-mutation that the workspace `unsafe_code` lint
        // forbids) we use an Anonymous-auth section, which needs no
        // env lookup at all. The token-from-env resolution path is
        // covered hermetically by
        // `config_parses_roastery_section_with_bearer_token_env`,
        // which drives `resolve` with an injected closure.
        let p = PartialConfig {
            cache: Some(PartialCacheConfig {
                roastery: Some(PartialRoasteryConfig {
                    url: Some("http://r".into()),
                    plain_http: Some(true),
                    timeout_secs: Some(7),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        };
        let mut cfg = Config::default();
        let touched = p.apply_to(&mut cfg);

        assert!(touched.iter().any(|t| t == "cache.roastery"));
        let r = cfg.cache.roastery.expect("roastery resolved");
        assert_eq!(r.url, "http://r");
        assert!(matches!(r.auth, RoasteryAuth::Anonymous));
        assert!(matches!(r.tls, RoasteryTls::PlainHttp));
        assert_eq!(r.timeout_secs, 7);
    }
}
