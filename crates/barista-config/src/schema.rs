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
    /// Telemetry opt-in. Default: false.
    pub enabled: bool,
    /// Endpoint URL. None means no-op transport.
    pub endpoint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct CompatConfig {
    /// Modules excluded from default builds.
    pub excluded_modules: Vec<String>,
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
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PartialCompatConfig {
    pub excluded_modules: Option<Vec<String>>,
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
        }
        if let Some(c) = &self.compat {
            if let Some(v) = &c.excluded_modules {
                target.compat.excluded_modules = v.clone();
                touched.push("compat.excluded-modules".into());
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
}
