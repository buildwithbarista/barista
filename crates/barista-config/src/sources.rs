//! Layered configuration loader machinery.
//!
//! The entry point is [`load_effective_config`]. It walks the six
//! layers documented at the crate root, recording each layer's
//! contribution in a [`LoadAudit`] alongside the resolved
//! [`Config`].

use std::path::{Path, PathBuf};

use crate::barista_toml::{BaristaTomlExtensions, ProjectConfigFile};
use crate::schema::*;
use crate::settings_xml::{SettingsError, parse_settings_xml};

/// Environment-variable getter signature used by [`LoaderInputs`].
/// Returning `Option<String>` rather than `Result` keeps the "var
/// unset" case cheap and reserves errors for the value-parsing
/// step.
pub type EnvGetter<'a> = dyn Fn(&str) -> Option<String> + 'a;

// ============================================================
// Loader inputs
// ============================================================

/// Inputs the loader needs from the caller. All fields are
/// optional; sensible defaults read from the real process
/// environment.
#[derive(Default, Clone)]
pub struct LoaderInputs<'a> {
    /// Optional: explicit user config-toml path. If `None`, uses
    /// `<home>/.barista/config.toml` (skipped if missing).
    pub user_config_path: Option<PathBuf>,

    /// Optional: explicit project `barista.toml` path. If `None`,
    /// walks up from `cwd_override` (or the real CWD) looking for
    /// `barista.toml`, stopping at any directory containing `.git`
    /// or at the filesystem root.
    pub project_config_path: Option<PathBuf>,

    /// Optional: explicit `settings.xml` path. If `None`, uses
    /// `<home>/.m2/settings.xml` when present.
    pub settings_xml_path: Option<PathBuf>,

    /// Environment-variable getter. If `None`, reads
    /// [`std::env::var`].
    pub env_get: Option<&'a EnvGetter<'a>>,

    /// HOME-dir override. If `None`, resolved from the `HOME` env
    /// var.
    pub home_override: Option<PathBuf>,

    /// CWD override. If `None`, uses [`std::env::current_dir`].
    pub cwd_override: Option<PathBuf>,

    /// CLI flag overrides — highest-precedence layer.
    pub cli: CliOverrides,
}

impl std::fmt::Debug for LoaderInputs<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoaderInputs")
            .field("user_config_path", &self.user_config_path)
            .field("project_config_path", &self.project_config_path)
            .field("settings_xml_path", &self.settings_xml_path)
            .field("env_get", &self.env_get.map(|_| "<fn>"))
            .field("home_override", &self.home_override)
            .field("cwd_override", &self.cwd_override)
            .field("cli", &self.cli)
            .finish()
    }
}

/// Subset of [`Config`] that a CLI flag can set. Every field is
/// `Option`; `None` means "don't override this".
#[derive(Debug, Default, Clone)]
pub struct CliOverrides {
    pub compat_mode: Option<CompatMode>,
    pub no_daemon: Option<bool>,
    pub log_level: Option<String>,
    pub max_concurrent_connections: Option<u32>,
    pub cache_dir: Option<PathBuf>,
}

// ============================================================
// Audit
// ============================================================

/// Per-layer audit so callers can see which file/env-var set each
/// field. Powers a future `barista config show` command.
#[derive(Debug, Clone)]
pub struct LoadAudit {
    pub layers_applied: Vec<LayerAudit>,
}

#[derive(Debug, Clone)]
pub struct LayerAudit {
    pub layer: LayerSource,
    pub fields_set: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerSource {
    Defaults,
    UserConfig(PathBuf),
    ProjectConfig(PathBuf),
    SettingsXml(PathBuf),
    Environment,
    Cli,
}

// ============================================================
// Errors
// ============================================================

#[derive(Debug, thiserror::Error)]
pub enum LoaderError {
    #[error("config file at {path:?}: {source}")]
    FileRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config file at {path:?}: invalid TOML: {detail}")]
    TomlParse { path: PathBuf, detail: String },
    #[error("environment variable {name:?}: invalid value: {detail}")]
    EnvParse { name: String, detail: String },
    #[error("HOME directory could not be resolved")]
    NoHome,
    #[error("settings.xml at {path:?}: {detail}")]
    SettingsXml { path: PathBuf, detail: String },
}

impl From<SettingsError> for LoaderError {
    fn from(e: SettingsError) -> Self {
        match e {
            SettingsError::Io { path, source } => LoaderError::FileRead { path, source },
            SettingsError::XmlParse { path, detail } => LoaderError::SettingsXml { path, detail },
            other => LoaderError::SettingsXml {
                path: PathBuf::new(),
                detail: other.to_string(),
            },
        }
    }
}

/// Public-facing wrapper around [`parse_settings_xml`]: ingests a
/// `settings.xml` file at `path` and returns its typed
/// [`crate::settings_xml::SettingsXml`] representation. Errors are
/// surfaced as [`LoaderError`] so callers using the loader's error
/// type don't need to handle two error families.
pub fn load_settings_xml(path: &Path) -> Result<crate::settings_xml::SettingsXml, LoaderError> {
    parse_settings_xml(path).map_err(LoaderError::from)
}

// ============================================================
// Entry point
// ============================================================

/// Load the effective config by applying all six layers in order
/// (defaults → user file → project file → settings.xml → env →
/// CLI). Records each layer's contributions in a [`LoadAudit`].
pub fn load_effective_config(inputs: LoaderInputs<'_>) -> Result<(Config, LoadAudit), LoaderError> {
    let home = resolve_home(&inputs)?;
    let mut audit = LoadAudit {
        layers_applied: Vec::new(),
    };

    // 1. Compiled defaults (path placeholders expanded against HOME).
    let mut effective = Config::default();
    expand_default_paths(&mut effective, &home);
    audit.layers_applied.push(LayerAudit {
        layer: LayerSource::Defaults,
        fields_set: vec!["<all>".to_string()],
    });

    // 2. User-level TOML.
    let user_path = inputs
        .user_config_path
        .clone()
        .unwrap_or_else(|| home.join(".barista").join("config.toml"));
    if user_path.exists() {
        let partial = read_partial_toml(&user_path)?;
        let fields = apply_with_home(&partial, &mut effective, &home);
        audit.layers_applied.push(LayerAudit {
            layer: LayerSource::UserConfig(user_path),
            fields_set: fields,
        });
    }

    // 3. Project-level TOML.
    let cwd = match &inputs.cwd_override {
        Some(p) => p.clone(),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let project_path = match inputs.project_config_path.clone() {
        Some(p) => Some(p),
        None => find_project_config(&cwd),
    };
    if let Some(path) = project_path {
        if path.exists() {
            let file = read_project_toml(&path)?;
            let mut fields = apply_with_home(&file.base, &mut effective, &home);
            // Attach project-only extensions (taps, modules,
            // plugins, project metadata) — but only if at least
            // one section is populated, so an extensions-less
            // file leaves `project_extensions` as None.
            if extensions_present(&file.extensions) {
                effective.project_extensions = Some(file.extensions);
                fields.push("project-extensions".into());
            }
            audit.layers_applied.push(LayerAudit {
                layer: LayerSource::ProjectConfig(path),
                fields_set: fields,
            });
        }
    }

    // 4. Settings.xml — parse and apply.
    let settings_path = inputs
        .settings_xml_path
        .clone()
        .unwrap_or_else(|| home.join(".m2").join("settings.xml"));
    if settings_path.exists() {
        let settings = load_settings_xml(&settings_path)?;
        let fields = apply_settings_xml(&settings, &mut effective, &home);
        effective.maven_settings = settings;
        audit.layers_applied.push(LayerAudit {
            layer: LayerSource::SettingsXml(settings_path),
            fields_set: fields,
        });
    }

    // 5. Environment variables.
    let env_fields = apply_env(&inputs, &mut effective, &home)?;
    if !env_fields.is_empty() {
        audit.layers_applied.push(LayerAudit {
            layer: LayerSource::Environment,
            fields_set: env_fields,
        });
    }

    // 6. CLI overrides.
    let cli_fields = apply_cli(&inputs.cli, &mut effective);
    if !cli_fields.is_empty() {
        audit.layers_applied.push(LayerAudit {
            layer: LayerSource::Cli,
            fields_set: cli_fields,
        });
    }

    Ok((effective, audit))
}

// ============================================================
// Helpers
// ============================================================

fn resolve_home(inputs: &LoaderInputs<'_>) -> Result<PathBuf, LoaderError> {
    if let Some(h) = &inputs.home_override {
        return Ok(h.clone());
    }
    let getter: Box<EnvGetter<'_>> = match inputs.env_get {
        Some(f) => Box::new(move |k| f(k)),
        None => Box::new(|k| std::env::var(k).ok()),
    };
    getter("HOME").map(PathBuf::from).ok_or(LoaderError::NoHome)
}

/// Expand a path that may begin with `~` or `~/` against `home`.
/// Other paths are returned verbatim.
pub(crate) fn expand_tilde(p: &Path, home: &Path) -> PathBuf {
    let s = match p.to_str() {
        Some(s) => s,
        None => return p.to_path_buf(),
    };
    if s == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home.join(rest);
    }
    p.to_path_buf()
}

fn expand_default_paths(cfg: &mut Config, home: &Path) {
    cfg.paths.cache_dir = expand_tilde(&cfg.paths.cache_dir, home);
    cfg.paths.user_config_dir = expand_tilde(&cfg.paths.user_config_dir, home);
    cfg.paths.settings_xml = expand_tilde(&cfg.paths.settings_xml, home);
    cfg.paths.m2_repository = expand_tilde(&cfg.paths.m2_repository, home);
    cfg.daemon.socket_dir = expand_tilde(&cfg.daemon.socket_dir, home);
}

/// Apply a [`PartialConfig`] and tilde-expand any paths it sets.
fn apply_with_home(partial: &PartialConfig, target: &mut Config, home: &Path) -> Vec<String> {
    let fields = partial.apply_to(target);
    // Re-expand any path fields the partial may have set; absolute
    // and bare-relative paths are pass-throughs.
    target.paths.cache_dir = expand_tilde(&target.paths.cache_dir, home);
    target.paths.user_config_dir = expand_tilde(&target.paths.user_config_dir, home);
    target.paths.settings_xml = expand_tilde(&target.paths.settings_xml, home);
    target.paths.m2_repository = expand_tilde(&target.paths.m2_repository, home);
    target.daemon.socket_dir = expand_tilde(&target.daemon.socket_dir, home);
    fields
}

/// Apply the parsed Maven `settings.xml` to the running effective
/// [`Config`]. Returns the list of field names this layer mutated,
/// for the [`LoadAudit`].
///
/// Currently this:
///
/// - Maps `<localRepository>` to `paths.m2-repository` (tilde-expanded).
/// - Records `<offline>true</offline>` in the audit (no direct Config
///   field today; the offline flag is consumed via `maven_settings`).
///
/// Servers, mirrors, profiles, proxies, and plugin groups are not
/// merged into [`Config`] directly — they are carried verbatim on
/// `Config::maven_settings` for the resolver and network layer to
/// consume.
fn apply_settings_xml(
    settings: &crate::settings_xml::SettingsXml,
    target: &mut Config,
    home: &Path,
) -> Vec<String> {
    let mut touched = Vec::new();
    if let Some(local_repo) = &settings.local_repository {
        target.paths.m2_repository = expand_tilde(Path::new(local_repo), home);
        touched.push("paths.m2-repository".into());
    }
    if settings.offline {
        touched.push("maven-settings.offline".into());
    }
    if !settings.servers.is_empty() {
        touched.push("maven-settings.servers".into());
    }
    if !settings.mirrors.is_empty() {
        touched.push("maven-settings.mirrors".into());
    }
    if !settings.profiles.is_empty() {
        touched.push("maven-settings.profiles".into());
    }
    if !settings.active_profile_ids.is_empty() {
        touched.push("maven-settings.active-profiles".into());
    }
    if !settings.proxies.is_empty() {
        touched.push("maven-settings.proxies".into());
    }
    if !settings.plugin_groups.is_empty() {
        touched.push("maven-settings.plugin-groups".into());
    }
    touched
}

fn read_project_toml(path: &Path) -> Result<ProjectConfigFile, LoaderError> {
    let raw = std::fs::read_to_string(path).map_err(|e| LoaderError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })?;
    toml::from_str::<ProjectConfigFile>(&raw).map_err(|e| LoaderError::TomlParse {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })
}

/// True iff at least one project-only section is populated. An
/// extensions-less project file should not cause
/// `Config.project_extensions` to flip to `Some(default)`.
fn extensions_present(ext: &BaristaTomlExtensions) -> bool {
    ext.project.is_some()
        || !ext.taps.is_empty()
        || !ext.modules.excluded.is_empty()
        || !ext.modules.overrides.is_empty()
        || !ext.plugins.classloader_cache_overrides.is_empty()
}

fn read_partial_toml(path: &Path) -> Result<PartialConfig, LoaderError> {
    let raw = std::fs::read_to_string(path).map_err(|e| LoaderError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })?;
    toml::from_str::<PartialConfig>(&raw).map_err(|e| LoaderError::TomlParse {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })
}

/// Walk up from `start` looking for `barista.toml`. Stops at any
/// directory containing a `.git` entry (treated as the project
/// boundary) or at the filesystem root.
fn find_project_config(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        let candidate = dir.join("barista.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        // Project-root boundary: stop ascending once we see a .git.
        if dir.join(".git").exists() {
            return None;
        }
        cur = dir.parent();
    }
    None
}

// ---------- Environment-variable layer ----------

/// All recognised `BARISTA_*` environment variables. Listed
/// explicitly (rather than reflected) so the supported surface is
/// audit-able and typo'd variables can be warned about.
const ENV_VARS: &[(&str, EnvKind)] = &[
    (
        "BARISTA_NETWORK__MAX_CONCURRENT_CONNECTIONS",
        EnvKind::NetMaxConn,
    ),
    ("BARISTA_NETWORK__REQUEST_TIMEOUT_SECS", EnvKind::NetTimeout),
    ("BARISTA_NETWORK__HTTP2_ENABLED", EnvKind::NetHttp2),
    ("BARISTA_NETWORK__PROXY", EnvKind::NetProxy),
    ("BARISTA_DAEMON__ENABLED", EnvKind::DaemonEnabled),
    ("BARISTA_DAEMON__IDLE_SHUTDOWN_SECS", EnvKind::DaemonIdle),
    ("BARISTA_DAEMON__MAX_HEAP", EnvKind::DaemonHeap),
    ("BARISTA_DAEMON__SOCKET_DIR", EnvKind::DaemonSocket),
    ("BARISTA_MAVEN__COMPAT_MODE", EnvKind::MavenCompat),
    ("BARISTA_MAVEN__HONOR_MVN_CONFIG", EnvKind::MavenMvnCfg),
    ("BARISTA_MAVEN__HONOR_JVM_CONFIG", EnvKind::MavenJvmCfg),
    ("BARISTA_LOGGING__LEVEL", EnvKind::LogLevel),
    ("BARISTA_LOGGING__MAVEN_SHAPE", EnvKind::LogShape),
    ("BARISTA_TELEMETRY__ENABLED", EnvKind::TelemEnabled),
    ("BARISTA_TELEMETRY__ENDPOINT", EnvKind::TelemEndpoint),
    ("BARISTA_PATHS__CACHE_DIR", EnvKind::PathCache),
    ("BARISTA_PATHS__USER_CONFIG_DIR", EnvKind::PathUserCfg),
    ("BARISTA_PATHS__SETTINGS_XML", EnvKind::PathSettings),
    ("BARISTA_PATHS__M2_REPOSITORY", EnvKind::PathM2Repo),
];

#[derive(Debug, Clone, Copy)]
enum EnvKind {
    NetMaxConn,
    NetTimeout,
    NetHttp2,
    NetProxy,
    DaemonEnabled,
    DaemonIdle,
    DaemonHeap,
    DaemonSocket,
    MavenCompat,
    MavenMvnCfg,
    MavenJvmCfg,
    LogLevel,
    LogShape,
    TelemEnabled,
    TelemEndpoint,
    PathCache,
    PathUserCfg,
    PathSettings,
    PathM2Repo,
}

fn parse_bool(name: &str, raw: &str) -> Result<bool, LoaderError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        other => Err(LoaderError::EnvParse {
            name: name.to_string(),
            detail: format!("expected one of true/false/1/0 (case-insensitive); got {other:?}"),
        }),
    }
}

fn parse_u32(name: &str, raw: &str) -> Result<u32, LoaderError> {
    raw.trim()
        .parse::<u32>()
        .map_err(|e| LoaderError::EnvParse {
            name: name.to_string(),
            detail: format!("expected a non-negative integer: {e}"),
        })
}

fn parse_compat(name: &str, raw: &str) -> Result<CompatMode, LoaderError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "three-nine" | "3.9" | "threenine" => Ok(CompatMode::ThreeNine),
        "four-zero" | "4.0" | "fourzero" => Ok(CompatMode::FourZero),
        "auto" => Ok(CompatMode::Auto),
        other => Err(LoaderError::EnvParse {
            name: name.to_string(),
            detail: format!("unknown compat mode {other:?}"),
        }),
    }
}

fn apply_env(
    inputs: &LoaderInputs<'_>,
    target: &mut Config,
    home: &Path,
) -> Result<Vec<String>, LoaderError> {
    let mut touched = Vec::new();
    let get_owned: Box<EnvGetter<'_>> = match inputs.env_get {
        Some(f) => Box::new(move |k| f(k)),
        None => Box::new(|k| std::env::var(k).ok()),
    };

    for (name, kind) in ENV_VARS {
        let Some(raw) = get_owned(name) else { continue };
        match kind {
            EnvKind::NetMaxConn => {
                target.network.max_concurrent_connections = parse_u32(name, &raw)?;
                touched.push("network.max-concurrent-connections".into());
            }
            EnvKind::NetTimeout => {
                target.network.request_timeout_secs = parse_u32(name, &raw)?;
                touched.push("network.request-timeout-secs".into());
            }
            EnvKind::NetHttp2 => {
                target.network.http2_enabled = parse_bool(name, &raw)?;
                touched.push("network.http2-enabled".into());
            }
            EnvKind::NetProxy => {
                target.network.proxy = Some(raw);
                touched.push("network.proxy".into());
            }
            EnvKind::DaemonEnabled => {
                target.daemon.enabled = parse_bool(name, &raw)?;
                touched.push("daemon.enabled".into());
            }
            EnvKind::DaemonIdle => {
                target.daemon.idle_shutdown_secs = parse_u32(name, &raw)?;
                touched.push("daemon.idle-shutdown-secs".into());
            }
            EnvKind::DaemonHeap => {
                target.daemon.max_heap = Some(raw);
                touched.push("daemon.max-heap".into());
            }
            EnvKind::DaemonSocket => {
                target.daemon.socket_dir = expand_tilde(Path::new(&raw), home);
                touched.push("daemon.socket-dir".into());
            }
            EnvKind::MavenCompat => {
                target.maven.compat_mode = parse_compat(name, &raw)?;
                touched.push("maven.compat-mode".into());
            }
            EnvKind::MavenMvnCfg => {
                target.maven.honor_mvn_config = parse_bool(name, &raw)?;
                touched.push("maven.honor-mvn-config".into());
            }
            EnvKind::MavenJvmCfg => {
                target.maven.honor_jvm_config = parse_bool(name, &raw)?;
                touched.push("maven.honor-jvm-config".into());
            }
            EnvKind::LogLevel => {
                target.logging.level = raw;
                touched.push("logging.level".into());
            }
            EnvKind::LogShape => {
                target.logging.maven_shape = parse_bool(name, &raw)?;
                touched.push("logging.maven-shape".into());
            }
            EnvKind::TelemEnabled => {
                target.telemetry.enabled = parse_bool(name, &raw)?;
                touched.push("telemetry.enabled".into());
            }
            EnvKind::TelemEndpoint => {
                target.telemetry.endpoint = Some(raw);
                touched.push("telemetry.endpoint".into());
            }
            EnvKind::PathCache => {
                target.paths.cache_dir = expand_tilde(Path::new(&raw), home);
                touched.push("paths.cache-dir".into());
            }
            EnvKind::PathUserCfg => {
                target.paths.user_config_dir = expand_tilde(Path::new(&raw), home);
                touched.push("paths.user-config-dir".into());
            }
            EnvKind::PathSettings => {
                target.paths.settings_xml = expand_tilde(Path::new(&raw), home);
                touched.push("paths.settings-xml".into());
            }
            EnvKind::PathM2Repo => {
                target.paths.m2_repository = expand_tilde(Path::new(&raw), home);
                touched.push("paths.m2-repository".into());
            }
        }
    }

    Ok(touched)
}

// ---------- CLI layer ----------

fn apply_cli(cli: &CliOverrides, target: &mut Config) -> Vec<String> {
    let mut touched = Vec::new();
    if let Some(v) = cli.compat_mode {
        target.maven.compat_mode = v;
        touched.push("maven.compat-mode".into());
    }
    if let Some(v) = cli.no_daemon {
        // --no-daemon flag is a tri-state: Some(true) disables.
        target.daemon.enabled = !v;
        touched.push("daemon.enabled".into());
    }
    if let Some(v) = &cli.log_level {
        target.logging.level = v.clone();
        touched.push("logging.level".into());
    }
    if let Some(v) = cli.max_concurrent_connections {
        target.network.max_concurrent_connections = v;
        touched.push("network.max-concurrent-connections".into());
    }
    if let Some(v) = &cli.cache_dir {
        target.paths.cache_dir = v.clone();
        touched.push("paths.cache-dir".into());
    }
    touched
}

// Small convenience: a freshly-defaulted Config with path
// placeholders expanded against a given HOME. Useful in tests
// and (in the future) for `barista config show`.
impl Config {
    #[doc(hidden)]
    pub fn default_with_home(home: &Path) -> Self {
        let mut cfg = Self::default();
        expand_default_paths(&mut cfg, home);
        cfg
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::TempDir;

    /// Build a minimal env-var getter from a [`HashMap`].
    fn env_from(map: &HashMap<String, String>) -> impl Fn(&str) -> Option<String> + '_ {
        move |k| map.get(k).cloned()
    }

    fn empty_env() -> HashMap<String, String> {
        HashMap::new()
    }

    /// A `LoaderInputs` with HOME set to `home`, no real-env reads.
    fn inputs_for<'a>(home: &Path, env: &'a HashMap<String, String>) -> LoaderInputs<'a> {
        // SAFETY of trait objects: env outlives the inputs because
        // the test threads through `env_from` returning an
        // `impl Fn + '_`.
        let g: &'a EnvGetter<'a> = Box::leak(Box::new(env_from(env)));
        LoaderInputs {
            home_override: Some(home.to_path_buf()),
            cwd_override: Some(home.to_path_buf()),
            env_get: Some(g),
            ..Default::default()
        }
    }

    // 1. Defaults round-trip.
    #[test]
    fn test_01_defaults_match_documented() {
        let home = TempDir::new().unwrap();
        let env = empty_env();
        let (cfg, audit) = load_effective_config(inputs_for(home.path(), &env)).unwrap();
        assert_eq!(cfg.network.max_concurrent_connections, 6);
        assert_eq!(cfg.network.request_timeout_secs, 60);
        assert!(cfg.network.http2_enabled);
        assert_eq!(cfg.network.proxy, None);
        assert!(cfg.daemon.enabled);
        assert_eq!(cfg.daemon.idle_shutdown_secs, 600);
        assert_eq!(cfg.maven.compat_mode, CompatMode::Auto);
        assert_eq!(cfg.logging.level, "info");
        assert!(!cfg.logging.maven_shape);
        assert!(!cfg.telemetry.enabled);
        assert!(cfg.compat.excluded_modules.is_empty());
        assert_eq!(cfg.paths.cache_dir, home.path().join(".barista/cache"));
        assert_eq!(cfg.paths.settings_xml, home.path().join(".m2/settings.xml"));
        assert_eq!(audit.layers_applied[0].layer, LayerSource::Defaults);
    }

    // 2. Empty user config file preserves defaults.
    #[test]
    fn test_02_empty_user_config_preserves_defaults() {
        let home = TempDir::new().unwrap();
        let user_cfg = home.path().join(".barista").join("config.toml");
        fs::create_dir_all(user_cfg.parent().unwrap()).unwrap();
        fs::write(&user_cfg, "").unwrap();

        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.user_config_path = Some(user_cfg);
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert_eq!(cfg.network.max_concurrent_connections, 6);
    }

    // 3. User config overrides defaults.
    #[test]
    fn test_03_user_config_overrides_defaults() {
        let home = TempDir::new().unwrap();
        let user_cfg = home.path().join(".barista").join("config.toml");
        fs::create_dir_all(user_cfg.parent().unwrap()).unwrap();
        fs::write(&user_cfg, "[network]\nmax-concurrent-connections = 8\n").unwrap();

        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.user_config_path = Some(user_cfg);
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert_eq!(cfg.network.max_concurrent_connections, 8);
        // Unrelated fields stay at defaults.
        assert_eq!(cfg.network.request_timeout_secs, 60);
    }

    // 4. Project config beats user config.
    #[test]
    fn test_04_project_beats_user() {
        let home = TempDir::new().unwrap();
        let user_cfg = home.path().join(".barista").join("config.toml");
        fs::create_dir_all(user_cfg.parent().unwrap()).unwrap();
        fs::write(&user_cfg, "[network]\nmax-concurrent-connections = 8\n").unwrap();

        let proj_dir = TempDir::new().unwrap();
        let proj_cfg = proj_dir.path().join("barista.toml");
        fs::write(&proj_cfg, "[network]\nmax-concurrent-connections = 10\n").unwrap();

        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.user_config_path = Some(user_cfg);
        inputs.project_config_path = Some(proj_cfg);
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert_eq!(cfg.network.max_concurrent_connections, 10);
    }

    // 5. Env var beats project config.
    #[test]
    fn test_05_env_beats_project() {
        let home = TempDir::new().unwrap();
        let proj_dir = TempDir::new().unwrap();
        let proj_cfg = proj_dir.path().join("barista.toml");
        fs::write(&proj_cfg, "[network]\nmax-concurrent-connections = 10\n").unwrap();

        let mut env = empty_env();
        env.insert(
            "BARISTA_NETWORK__MAX_CONCURRENT_CONNECTIONS".into(),
            "12".into(),
        );
        let mut inputs = inputs_for(home.path(), &env);
        inputs.project_config_path = Some(proj_cfg);
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert_eq!(cfg.network.max_concurrent_connections, 12);
    }

    // 6. CLI override beats env.
    #[test]
    fn test_06_cli_beats_env() {
        let home = TempDir::new().unwrap();
        let mut env = empty_env();
        env.insert(
            "BARISTA_NETWORK__MAX_CONCURRENT_CONNECTIONS".into(),
            "12".into(),
        );
        let mut inputs = inputs_for(home.path(), &env);
        inputs.cli.max_concurrent_connections = Some(16);
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert_eq!(cfg.network.max_concurrent_connections, 16);
    }

    // 7. Walk-up discovery of barista.toml.
    #[test]
    fn test_07_walkup_finds_barista_toml() {
        let proj = TempDir::new().unwrap();
        let nested = proj.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        let toml_path = proj.path().join("barista.toml");
        fs::write(&toml_path, "[network]\nmax-concurrent-connections = 9\n").unwrap();

        let home = TempDir::new().unwrap();
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.cwd_override = Some(nested);
        // Leave project_config_path = None to exercise discovery.
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert_eq!(cfg.network.max_concurrent_connections, 9);
    }

    // 8. Walk-up stops at .git.
    #[test]
    fn test_08_walkup_stops_at_git() {
        // outer/ has a barista.toml; outer/inner/.git marks a
        // project boundary; CWD is outer/inner/sub.
        let outer = TempDir::new().unwrap();
        fs::write(
            outer.path().join("barista.toml"),
            "[network]\nmax-concurrent-connections = 99\n",
        )
        .unwrap();
        let inner = outer.path().join("inner");
        fs::create_dir_all(inner.join(".git")).unwrap();
        let sub = inner.join("sub");
        fs::create_dir_all(&sub).unwrap();

        let home = TempDir::new().unwrap();
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.cwd_override = Some(sub);
        let (cfg, _) = load_effective_config(inputs).unwrap();
        // Outer barista.toml must NOT be picked up.
        assert_eq!(cfg.network.max_concurrent_connections, 6);
    }

    // 9. Bool env var parses.
    #[test]
    fn test_09_bool_env_parses() {
        let home = TempDir::new().unwrap();
        let mut env = empty_env();
        env.insert("BARISTA_DAEMON__ENABLED".into(), "false".into());
        let (cfg, _) = load_effective_config(inputs_for(home.path(), &env)).unwrap();
        assert!(!cfg.daemon.enabled);
    }

    // 10. Invalid bool env var errors.
    #[test]
    fn test_10_invalid_bool_errors() {
        let home = TempDir::new().unwrap();
        let mut env = empty_env();
        env.insert("BARISTA_DAEMON__ENABLED".into(), "yes".into());
        let err = load_effective_config(inputs_for(home.path(), &env)).unwrap_err();
        match err {
            LoaderError::EnvParse { name, .. } => {
                assert_eq!(name, "BARISTA_DAEMON__ENABLED");
            }
            other => panic!("expected EnvParse, got {other:?}"),
        }
    }

    // 11. Invalid TOML → TomlParse.
    #[test]
    fn test_11_invalid_toml_errors() {
        let home = TempDir::new().unwrap();
        let bad = home.path().join("bad.toml");
        fs::write(&bad, "this is = not = toml\n").unwrap();
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.user_config_path = Some(bad.clone());
        let err = load_effective_config(inputs).unwrap_err();
        match err {
            LoaderError::TomlParse { path, .. } => assert_eq!(path, bad),
            other => panic!("expected TomlParse, got {other:?}"),
        }
    }

    // 12. Missing explicit user config → defaults preserved (no error).
    #[test]
    fn test_12_missing_explicit_user_config_no_error() {
        let home = TempDir::new().unwrap();
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.user_config_path = Some(home.path().join("does-not-exist.toml"));
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert_eq!(cfg.network.max_concurrent_connections, 6);
    }

    // 13. Logging level env override.
    #[test]
    fn test_13_logging_level_env() {
        let home = TempDir::new().unwrap();
        let mut env = empty_env();
        env.insert("BARISTA_LOGGING__LEVEL".into(), "debug".into());
        let (cfg, _) = load_effective_config(inputs_for(home.path(), &env)).unwrap();
        assert_eq!(cfg.logging.level, "debug");
    }

    // 14. Path env override.
    #[test]
    fn test_14_cache_dir_env_override() {
        let home = TempDir::new().unwrap();
        let mut env = empty_env();
        env.insert("BARISTA_PATHS__CACHE_DIR".into(), "/custom/path".into());
        let (cfg, _) = load_effective_config(inputs_for(home.path(), &env)).unwrap();
        assert_eq!(cfg.paths.cache_dir, PathBuf::from("/custom/path"));
    }

    // 15. Settings.xml stub doesn't affect Config.
    #[test]
    fn test_15_settings_xml_stub_noop() {
        let home = TempDir::new().unwrap();
        let m2 = home.path().join(".m2");
        fs::create_dir_all(&m2).unwrap();
        fs::write(m2.join("settings.xml"), "<settings/>").unwrap();
        let env = empty_env();
        let (cfg, audit) = load_effective_config(inputs_for(home.path(), &env)).unwrap();
        assert_eq!(cfg, Config::default_with_home(home.path()));
        assert!(
            audit
                .layers_applied
                .iter()
                .any(|l| matches!(&l.layer, LayerSource::SettingsXml(_)))
        );
    }

    // 16. HOME override expands ~ in defaults.
    #[test]
    fn test_16_home_override_expands_tilde() {
        let home = TempDir::new().unwrap();
        let env = empty_env();
        let (cfg, _) = load_effective_config(inputs_for(home.path(), &env)).unwrap();
        assert!(cfg.paths.cache_dir.starts_with(home.path()));
        assert!(cfg.daemon.socket_dir.starts_with(home.path()));
    }

    // 17. CompatMode round-trips through TOML, env, and CLI.
    #[test]
    fn test_17_compat_mode_round_trip() {
        // TOML
        let home = TempDir::new().unwrap();
        let user_cfg = home.path().join("u.toml");
        fs::write(&user_cfg, "[maven]\ncompat-mode = \"three-nine\"\n").unwrap();
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.user_config_path = Some(user_cfg);
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert_eq!(cfg.maven.compat_mode, CompatMode::ThreeNine);

        // Env
        let mut env = empty_env();
        env.insert("BARISTA_MAVEN__COMPAT_MODE".into(), "4.0".into());
        let (cfg, _) = load_effective_config(inputs_for(home.path(), &env)).unwrap();
        assert_eq!(cfg.maven.compat_mode, CompatMode::FourZero);

        // CLI
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.cli.compat_mode = Some(CompatMode::Auto);
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert_eq!(cfg.maven.compat_mode, CompatMode::Auto);
    }

    // 18. Unknown TOML key errors via deny_unknown_fields.
    #[test]
    fn test_18_unknown_key_errors() {
        let home = TempDir::new().unwrap();
        let bad = home.path().join("u.toml");
        fs::write(&bad, "[network]\nbogus = true\n").unwrap();
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.user_config_path = Some(bad);
        let err = load_effective_config(inputs).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bogus"), "expected 'bogus' in error: {msg}");
    }

    // 19. Partial [network] table preserves other fields.
    #[test]
    fn test_19_partial_table_preserves_rest() {
        let home = TempDir::new().unwrap();
        let user_cfg = home.path().join("u.toml");
        fs::write(&user_cfg, "[network]\nmax-concurrent-connections = 4\n").unwrap();
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.user_config_path = Some(user_cfg);
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert_eq!(cfg.network.max_concurrent_connections, 4);
        assert_eq!(cfg.network.request_timeout_secs, 60);
        assert!(cfg.network.http2_enabled);
    }

    // 20. Audit records each layer's contribution.
    #[test]
    fn test_20_audit_records_layers() {
        let home = TempDir::new().unwrap();
        let user_cfg = home.path().join("u.toml");
        fs::write(&user_cfg, "[network]\nmax-concurrent-connections = 7\n").unwrap();

        let proj = TempDir::new().unwrap();
        let proj_cfg = proj.path().join("barista.toml");
        fs::write(&proj_cfg, "[logging]\nlevel = \"warn\"\n").unwrap();

        let mut env = empty_env();
        env.insert("BARISTA_DAEMON__ENABLED".into(), "false".into());

        let mut inputs = inputs_for(home.path(), &env);
        inputs.user_config_path = Some(user_cfg);
        inputs.project_config_path = Some(proj_cfg);
        inputs.cli.log_level = Some("trace".into());
        let (_cfg, audit) = load_effective_config(inputs).unwrap();
        assert_eq!(audit.layers_applied[0].layer, LayerSource::Defaults);
        assert!(matches!(
            audit.layers_applied[1].layer,
            LayerSource::UserConfig(_)
        ));
        assert!(matches!(
            audit.layers_applied[2].layer,
            LayerSource::ProjectConfig(_)
        ));
        // Env and CLI layers also recorded.
        assert!(
            audit
                .layers_applied
                .iter()
                .any(|l| l.layer == LayerSource::Environment)
        );
        assert!(
            audit
                .layers_applied
                .iter()
                .any(|l| l.layer == LayerSource::Cli)
        );
    }

    // 21. Audit reports ProjectConfig(path) correctly.
    #[test]
    fn test_21_audit_project_config_path() {
        let home = TempDir::new().unwrap();
        let proj = TempDir::new().unwrap();
        let proj_cfg = proj.path().join("barista.toml");
        fs::write(&proj_cfg, "[logging]\nlevel = \"warn\"\n").unwrap();
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.project_config_path = Some(proj_cfg.clone());
        let (_cfg, audit) = load_effective_config(inputs).unwrap();
        let found = audit
            .layers_applied
            .iter()
            .any(|l| matches!(&l.layer, LayerSource::ProjectConfig(p) if p == &proj_cfg));
        assert!(found);
    }

    // 22. ~ in a TOML path expands against HOME.
    #[test]
    fn test_22_tilde_in_toml_expands() {
        let home = TempDir::new().unwrap();
        let user_cfg = home.path().join("u.toml");
        fs::write(&user_cfg, "[paths]\ncache-dir = \"~/custom-cache\"\n").unwrap();
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.user_config_path = Some(user_cfg);
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert_eq!(cfg.paths.cache_dir, home.path().join("custom-cache"));
    }

    // 23. CLI override clears an env-set field if Some.
    #[test]
    fn test_23_cli_overrides_env_field() {
        let home = TempDir::new().unwrap();
        let mut env = empty_env();
        env.insert("BARISTA_LOGGING__LEVEL".into(), "warn".into());
        let mut inputs = inputs_for(home.path(), &env);
        inputs.cli.log_level = Some("error".into());
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert_eq!(cfg.logging.level, "error");
    }

    // 24. excluded_modules round-trips.
    #[test]
    fn test_24_excluded_modules_round_trip() {
        let home = TempDir::new().unwrap();
        let user_cfg = home.path().join("u.toml");
        fs::write(
            &user_cfg,
            "[compat]\nexcluded-modules = [\"foo\", \"bar\"]\n",
        )
        .unwrap();
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.user_config_path = Some(user_cfg);
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert_eq!(cfg.compat.excluded_modules, vec!["foo", "bar"]);
    }

    // 25. End-to-end happy path: every layer sets a distinct field.
    #[test]
    fn test_25_end_to_end_all_layers() {
        let home = TempDir::new().unwrap();
        // user: sets network.request-timeout-secs
        let user_cfg = home.path().join(".barista").join("config.toml");
        fs::create_dir_all(user_cfg.parent().unwrap()).unwrap();
        fs::write(&user_cfg, "[network]\nrequest-timeout-secs = 30\n").unwrap();

        // project: sets logging.level
        let proj = TempDir::new().unwrap();
        let proj_cfg = proj.path().join("barista.toml");
        fs::write(&proj_cfg, "[logging]\nlevel = \"warn\"\n").unwrap();

        // env: sets daemon.idle-shutdown-secs
        let mut env = empty_env();
        env.insert("BARISTA_DAEMON__IDLE_SHUTDOWN_SECS".into(), "120".into());

        // cli: sets max-concurrent-connections
        let mut inputs = inputs_for(home.path(), &env);
        inputs.user_config_path = Some(user_cfg);
        inputs.project_config_path = Some(proj_cfg);
        inputs.cli.max_concurrent_connections = Some(20);

        let (cfg, _) = load_effective_config(inputs).unwrap();

        // Default unchanged
        assert!(cfg.network.http2_enabled);
        // From user
        assert_eq!(cfg.network.request_timeout_secs, 30);
        // From project
        assert_eq!(cfg.logging.level, "warn");
        // From env
        assert_eq!(cfg.daemon.idle_shutdown_secs, 120);
        // From CLI
        assert_eq!(cfg.network.max_concurrent_connections, 20);
        // Path defaults expanded against HOME
        assert_eq!(cfg.paths.cache_dir, home.path().join(".barista/cache"));
    }

    // 27. Project file with [[taps]] populates project_extensions on the
    //     effective Config.
    #[test]
    fn test_27_project_extensions_populated_from_taps() {
        let home = TempDir::new().unwrap();
        let proj = TempDir::new().unwrap();
        let proj_cfg = proj.path().join("barista.toml");
        fs::write(
            &proj_cfg,
            r#"
[network]
max-concurrent-connections = 5

[[taps]]
id = "acme"
url = "https://taps.acme.com/b"

[plugins]
classloader-cache-overrides = { "org.example:plugin" = "no-cache" }
"#,
        )
        .unwrap();
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.project_config_path = Some(proj_cfg);
        let (cfg, audit) = load_effective_config(inputs).unwrap();

        // Base layer applied.
        assert_eq!(cfg.network.max_concurrent_connections, 5);
        // Extensions attached.
        let ext = cfg.project_extensions.expect("project extensions");
        assert_eq!(ext.taps.len(), 1);
        assert_eq!(ext.taps[0].id, "acme");
        assert_eq!(ext.plugins.classloader_cache_overrides.len(), 1);
        // Audit notes the extensions were set.
        let proj_audit = audit
            .layers_applied
            .iter()
            .find(|l| matches!(&l.layer, LayerSource::ProjectConfig(_)))
            .unwrap();
        assert!(
            proj_audit
                .fields_set
                .iter()
                .any(|f| f == "project-extensions")
        );
    }

    // 28. A project file with only base fields leaves project_extensions = None.
    #[test]
    fn test_28_project_without_extensions_leaves_none() {
        let home = TempDir::new().unwrap();
        let proj = TempDir::new().unwrap();
        let proj_cfg = proj.path().join("barista.toml");
        fs::write(&proj_cfg, "[network]\nmax-concurrent-connections = 5\n").unwrap();
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.project_config_path = Some(proj_cfg);
        let (cfg, _) = load_effective_config(inputs).unwrap();
        assert!(cfg.project_extensions.is_none());
    }

    // 29. User-level config files cannot accidentally carry
    //     project-only sections — `PartialConfig` denies unknown
    //     fields, so a `[[taps]]` in `~/.barista/config.toml`
    //     would surface as a parse error.
    #[test]
    fn test_29_user_config_rejects_project_only_sections() {
        let home = TempDir::new().unwrap();
        let user_cfg = home.path().join("u.toml");
        fs::write(&user_cfg, "[[taps]]\nid = \"x\"\nurl = \"https://x/y\"\n").unwrap();
        let env = empty_env();
        let mut inputs = inputs_for(home.path(), &env);
        inputs.user_config_path = Some(user_cfg);
        let err = load_effective_config(inputs).unwrap_err();
        assert!(matches!(err, LoaderError::TomlParse { .. }));
    }

    // 30. NoHome error when HOME is unset and no override.
    #[test]
    fn test_26_no_home_errors() {
        let env = empty_env();
        let g: &EnvGetter<'_> = &|_| None;
        let inputs = LoaderInputs {
            env_get: Some(g),
            ..Default::default()
        };
        let _ = env;
        let err = load_effective_config(inputs).unwrap_err();
        assert!(matches!(err, LoaderError::NoHome));
    }
}
