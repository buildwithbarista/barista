// Integration-test / example / benchmark target — workspace security
// lints are allowed here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`)
// is the documented contract for failing a test loudly. This allow block
// keeps the crate root's `#![allow(...)]` from being silently dropped by
// the separate compilation unit each test file forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Snapshot tests for the effective Barista config.
//!
//! Each scenario constructs a [`LoaderInputs`] for a specific
//! layered situation, runs the loader, and snapshots the rendered
//! output. A future regression in layer precedence, env-var parsing,
//! or settings.xml integration surfaces as a clear visual diff
//! during PR review — complementary to the targeted assertions in
//! the unit tests inside `sources.rs`.
//!
//! Determinism guards:
//!
//! - Field names are emitted in kebab-case and sorted lexicographically.
//! - HOME / project / settings-xml paths are pinned to stable
//!   `<HOME>`, `<PROJ>`, `<SETTINGS>` placeholders so snapshots
//!   reproduce across runs and machines.
//! - No timestamps, hostnames, or process-state-dependent values
//!   appear in the rendered output.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use barista_config::sources::EnvGetter;
use barista_config::{
    CliOverrides, CompatMode, Config, LayerSource, LoadAudit, LoaderInputs, load_effective_config,
};
use tempfile::TempDir;

// =====================================================================
// Rendering
// =====================================================================

/// Render the effective `Config` + `LoadAudit` to a stable,
/// human-readable string. Field names are kebab-case and sorted
/// alphabetically; lists are emitted as bracketed comma-separated
/// values, with empty lists rendered as `[]`.
fn render_effective(config: &Config, audit: &LoadAudit) -> String {
    let mut lines: Vec<(String, String)> = Vec::new();

    // ---- paths ----
    lines.push((
        "paths.cache-dir".into(),
        format_path(&config.paths.cache_dir),
    ));
    lines.push((
        "paths.user-config-dir".into(),
        format_path(&config.paths.user_config_dir),
    ));
    lines.push((
        "paths.settings-xml".into(),
        format_path(&config.paths.settings_xml),
    ));
    lines.push((
        "paths.m2-repository".into(),
        format_path(&config.paths.m2_repository),
    ));

    // ---- network ----
    lines.push((
        "network.max-concurrent-connections".into(),
        config.network.max_concurrent_connections.to_string(),
    ));
    lines.push((
        "network.request-timeout-secs".into(),
        config.network.request_timeout_secs.to_string(),
    ));
    lines.push((
        "network.http2-enabled".into(),
        config.network.http2_enabled.to_string(),
    ));
    lines.push((
        "network.proxy".into(),
        format_opt_string(config.network.proxy.as_deref()),
    ));

    // ---- daemon ----
    lines.push(("daemon.enabled".into(), config.daemon.enabled.to_string()));
    lines.push((
        "daemon.idle-shutdown-secs".into(),
        config.daemon.idle_shutdown_secs.to_string(),
    ));
    lines.push((
        "daemon.max-heap".into(),
        format_opt_string(config.daemon.max_heap.as_deref()),
    ));
    lines.push((
        "daemon.socket-dir".into(),
        format_path(&config.daemon.socket_dir),
    ));

    // ---- maven ----
    lines.push((
        "maven.compat-mode".into(),
        format_compat(config.maven.compat_mode),
    ));
    lines.push((
        "maven.honor-mvn-config".into(),
        config.maven.honor_mvn_config.to_string(),
    ));
    lines.push((
        "maven.honor-jvm-config".into(),
        config.maven.honor_jvm_config.to_string(),
    ));

    // ---- logging ----
    lines.push((
        "logging.level".into(),
        format!("\"{}\"", config.logging.level),
    ));
    lines.push((
        "logging.maven-shape".into(),
        config.logging.maven_shape.to_string(),
    ));

    // ---- telemetry ----
    lines.push((
        "telemetry.enabled".into(),
        config.telemetry.enabled.to_string(),
    ));
    lines.push((
        "telemetry.endpoint".into(),
        format_opt_string(config.telemetry.endpoint.as_deref()),
    ));
    lines.push((
        "telemetry.client-id".into(),
        format_opt_string(config.telemetry.client_id.as_deref()),
    ));
    lines.push((
        "telemetry.transport-enabled".into(),
        config.telemetry.transport_enabled.to_string(),
    ));

    // ---- compat ----
    lines.push((
        "compat.excluded-modules".into(),
        format_str_list(&config.compat.excluded_modules),
    ));

    // ---- maven-settings (summary) ----
    let ms = &config.maven_settings;
    lines.push((
        "maven-settings.local-repository".into(),
        format_opt_string(ms.local_repository.as_deref()),
    ));
    lines.push(("maven-settings.offline".into(), ms.offline.to_string()));
    lines.push((
        "maven-settings.interactive-mode".into(),
        ms.interactive_mode.to_string(),
    ));
    lines.push((
        "maven-settings.servers".into(),
        format_str_list(&ms.servers.iter().map(|s| s.id.clone()).collect::<Vec<_>>()),
    ));
    lines.push((
        "maven-settings.mirrors".into(),
        format_str_list(&ms.mirrors.iter().map(|m| m.id.clone()).collect::<Vec<_>>()),
    ));
    lines.push((
        "maven-settings.profiles".into(),
        format_str_list(&ms.profiles.iter().map(|p| p.id.clone()).collect::<Vec<_>>()),
    ));
    lines.push((
        "maven-settings.active-profiles".into(),
        format_str_list(&ms.active_profile_ids),
    ));
    lines.push((
        "maven-settings.proxies".into(),
        format_str_list(&ms.proxies.iter().map(|p| p.id.clone()).collect::<Vec<_>>()),
    ));
    lines.push((
        "maven-settings.plugin-groups".into(),
        format_str_list(&ms.plugin_groups),
    ));

    // ---- project-extensions ----
    if let Some(ext) = &config.project_extensions {
        lines.push((
            "project-extensions.project".into(),
            match &ext.project {
                None => "<unset>".into(),
                Some(p) => format!(
                    "name={}, group-id={}, artifact-id={}, version={}",
                    format_opt_string(p.name.as_deref()),
                    format_opt_string(p.group_id.as_deref()),
                    format_opt_string(p.artifact_id.as_deref()),
                    format_opt_string(p.version.as_deref()),
                ),
            },
        ));
        lines.push((
            "project-extensions.taps".into(),
            format_str_list(&ext.taps.iter().map(|t| t.name.clone()).collect::<Vec<_>>()),
        ));
        lines.push((
            "project-extensions.modules.excluded".into(),
            format_str_list(&ext.modules.excluded),
        ));
        lines.push((
            "project-extensions.modules.overrides".into(),
            format_str_list(&ext.modules.overrides.keys().cloned().collect::<Vec<_>>()),
        ));
        lines.push((
            "project-extensions.plugins.classloader-cache-overrides".into(),
            format_str_list(
                &ext.plugins
                    .classloader_cache_overrides
                    .iter()
                    .map(|(k, v)| format!("{k}={v:?}"))
                    .collect::<Vec<_>>(),
            ),
        ));
    } else {
        lines.push(("project-extensions".into(), "<unset>".into()));
    }

    lines.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::new();
    out.push_str("=== effective config ===\n");
    let width = lines.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, v) in &lines {
        out.push_str(&format!("{k:<width$} = {v}\n"));
    }

    out.push_str("\n=== audit ===\n");
    for layer in &audit.layers_applied {
        let (name, path) = match &layer.layer {
            LayerSource::Defaults => ("defaults".to_string(), String::new()),
            LayerSource::UserConfig(p) => ("user-config".to_string(), format_path(p)),
            LayerSource::ProjectConfig(p) => ("project-config".to_string(), format_path(p)),
            LayerSource::SettingsXml(p) => ("settings-xml".to_string(), format_path(p)),
            LayerSource::Environment => ("environment".to_string(), String::new()),
            LayerSource::Cli => ("cli".to_string(), String::new()),
        };
        let mut fields = layer.fields_set.clone();
        fields.sort();
        out.push_str(&format!(
            "{name:<16} <- {path:<40} [{fields}]\n",
            name = name,
            path = path,
            fields = fields.join(", "),
        ));
    }

    out
}

fn format_path(p: &Path) -> String {
    format!("\"{}\"", p.display())
}

fn format_opt_string(s: Option<&str>) -> String {
    match s {
        None => "<unset>".into(),
        Some(v) => format!("\"{v}\""),
    }
}

fn format_compat(m: CompatMode) -> String {
    match m {
        CompatMode::ThreeNine => "three-nine".into(),
        CompatMode::FourZero => "four-zero".into(),
        CompatMode::Auto => "auto".into(),
    }
}

fn format_str_list(items: &[String]) -> String {
    if items.is_empty() {
        "[]".into()
    } else {
        let mut sorted = items.to_vec();
        sorted.sort();
        format!(
            "[{}]",
            sorted
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Replace tempdir-generated paths with stable placeholders so
/// snapshots are byte-identical across runs.
fn redact_paths(s: &str, replacements: &[(&str, &str)]) -> String {
    let mut out = s.to_string();
    for (from, to) in replacements {
        out = out.replace(from, to);
    }
    out
}

// =====================================================================
// Test harness helpers
// =====================================================================

type BoxedEnvGetter = Box<dyn Fn(&str) -> Option<String>>;

fn env_from(map: HashMap<String, String>) -> BoxedEnvGetter {
    Box::new(move |k: &str| map.get(k).cloned())
}

/// Bundle the per-scenario sandbox dirs + replacement table. Holds
/// `TempDir` guards so the dirs live for the duration of the test.
struct Sandbox {
    home: TempDir,
    proj: TempDir,
    replacements: Vec<(String, String)>,
}

impl Sandbox {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let proj = TempDir::new().unwrap();
        let replacements = vec![
            (home.path().display().to_string(), "<HOME>".to_string()),
            (proj.path().display().to_string(), "<PROJ>".to_string()),
        ];
        Sandbox {
            home,
            proj,
            replacements,
        }
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn proj(&self) -> &Path {
        self.proj.path()
    }

    fn redact(&self, s: &str) -> String {
        let pairs: Vec<(&str, &str)> = self
            .replacements
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        redact_paths(s, &pairs)
    }
}

/// Build a `LoaderInputs` with HOME pinned to the sandbox, CWD set to
/// the sandbox's home (so project-config discovery doesn't escape
/// into the real filesystem), an env getter from `env_map`, and a
/// `cli` value.
///
/// The returned `Box<EnvGetter>` must outlive the inputs; this
/// helper threads it through `Box::leak`, which is a test-only
/// shortcut.
fn make_inputs(
    home: &Path,
    cwd: &Path,
    env_map: HashMap<String, String>,
    cli: CliOverrides,
) -> LoaderInputs<'static> {
    let getter = Box::leak(Box::new(env_from(env_map))) as &dyn Fn(&str) -> Option<String>;
    // Cast through the public type alias.
    let getter: &'static EnvGetter<'static> = getter;
    LoaderInputs {
        home_override: Some(home.to_path_buf()),
        cwd_override: Some(cwd.to_path_buf()),
        env_get: Some(getter),
        cli,
        ..Default::default()
    }
}

// =====================================================================
// Scenarios
// =====================================================================

#[test]
fn defaults_only() {
    let sb = Sandbox::new();
    let inputs = make_inputs(
        sb.home(),
        sb.home(),
        HashMap::new(),
        CliOverrides::default(),
    );
    let (cfg, audit) = load_effective_config(inputs).unwrap();
    let rendered = render_effective(&cfg, &audit);
    insta::assert_snapshot!(sb.redact(&rendered));
}

#[test]
fn user_only() {
    let sb = Sandbox::new();
    let user_cfg = sb.home().join(".barista").join("config.toml");
    fs::create_dir_all(user_cfg.parent().unwrap()).unwrap();
    fs::write(&user_cfg, "[network]\nmax-concurrent-connections = 8\n").unwrap();

    let mut inputs = make_inputs(
        sb.home(),
        sb.home(),
        HashMap::new(),
        CliOverrides::default(),
    );
    inputs.user_config_path = Some(user_cfg);
    let (cfg, audit) = load_effective_config(inputs).unwrap();
    let rendered = render_effective(&cfg, &audit);
    insta::assert_snapshot!(sb.redact(&rendered));
}

#[test]
fn project_only() {
    let sb = Sandbox::new();
    let proj_cfg = sb.proj().join("barista.toml");
    fs::write(&proj_cfg, "[logging]\nlevel = \"debug\"\n").unwrap();

    let mut inputs = make_inputs(
        sb.home(),
        sb.home(),
        HashMap::new(),
        CliOverrides::default(),
    );
    inputs.project_config_path = Some(proj_cfg);
    let (cfg, audit) = load_effective_config(inputs).unwrap();
    let rendered = render_effective(&cfg, &audit);
    insta::assert_snapshot!(sb.redact(&rendered));
}

#[test]
fn project_overrides_user() {
    let sb = Sandbox::new();
    let user_cfg = sb.home().join(".barista").join("config.toml");
    fs::create_dir_all(user_cfg.parent().unwrap()).unwrap();
    fs::write(&user_cfg, "[network]\nmax-concurrent-connections = 8\n").unwrap();
    let proj_cfg = sb.proj().join("barista.toml");
    fs::write(&proj_cfg, "[network]\nmax-concurrent-connections = 10\n").unwrap();

    let mut inputs = make_inputs(
        sb.home(),
        sb.home(),
        HashMap::new(),
        CliOverrides::default(),
    );
    inputs.user_config_path = Some(user_cfg);
    inputs.project_config_path = Some(proj_cfg);
    let (cfg, audit) = load_effective_config(inputs).unwrap();
    let rendered = render_effective(&cfg, &audit);
    insta::assert_snapshot!(sb.redact(&rendered));
}

#[test]
fn env_overrides_project() {
    let sb = Sandbox::new();
    let user_cfg = sb.home().join(".barista").join("config.toml");
    fs::create_dir_all(user_cfg.parent().unwrap()).unwrap();
    fs::write(&user_cfg, "[network]\nmax-concurrent-connections = 8\n").unwrap();
    let proj_cfg = sb.proj().join("barista.toml");
    fs::write(&proj_cfg, "[network]\nmax-concurrent-connections = 10\n").unwrap();

    let mut env = HashMap::new();
    env.insert(
        "BARISTA_NETWORK__MAX_CONCURRENT_CONNECTIONS".into(),
        "12".into(),
    );

    let mut inputs = make_inputs(sb.home(), sb.home(), env, CliOverrides::default());
    inputs.user_config_path = Some(user_cfg);
    inputs.project_config_path = Some(proj_cfg);
    let (cfg, audit) = load_effective_config(inputs).unwrap();
    let rendered = render_effective(&cfg, &audit);
    insta::assert_snapshot!(sb.redact(&rendered));
}

#[test]
fn cli_overrides_env() {
    let sb = Sandbox::new();
    let user_cfg = sb.home().join(".barista").join("config.toml");
    fs::create_dir_all(user_cfg.parent().unwrap()).unwrap();
    fs::write(&user_cfg, "[network]\nmax-concurrent-connections = 8\n").unwrap();
    let proj_cfg = sb.proj().join("barista.toml");
    fs::write(&proj_cfg, "[network]\nmax-concurrent-connections = 10\n").unwrap();

    let mut env = HashMap::new();
    env.insert(
        "BARISTA_NETWORK__MAX_CONCURRENT_CONNECTIONS".into(),
        "12".into(),
    );
    let cli = CliOverrides {
        max_concurrent_connections: Some(16),
        ..Default::default()
    };

    let mut inputs = make_inputs(sb.home(), sb.home(), env, cli);
    inputs.user_config_path = Some(user_cfg);
    inputs.project_config_path = Some(proj_cfg);
    let (cfg, audit) = load_effective_config(inputs).unwrap();
    let rendered = render_effective(&cfg, &audit);
    insta::assert_snapshot!(sb.redact(&rendered));
}

#[test]
fn settings_xml_local_repository() {
    let sb = Sandbox::new();
    let m2 = sb.home().join(".m2");
    fs::create_dir_all(&m2).unwrap();
    let settings_path = m2.join("settings.xml");
    fs::write(
        &settings_path,
        r#"<settings><localRepository>/srv/m2</localRepository></settings>"#,
    )
    .unwrap();

    let inputs = make_inputs(
        sb.home(),
        sb.home(),
        HashMap::new(),
        CliOverrides::default(),
    );
    let (cfg, audit) = load_effective_config(inputs).unwrap();
    let rendered = render_effective(&cfg, &audit);
    insta::assert_snapshot!(sb.redact(&rendered));
}

#[test]
fn settings_xml_full() {
    let sb = Sandbox::new();
    let m2 = sb.home().join(".m2");
    fs::create_dir_all(&m2).unwrap();
    let settings_path = m2.join("settings.xml");
    fs::write(
        &settings_path,
        r#"<settings>
  <servers>
    <server>
      <id>central-auth</id>
      <username>alice</username>
      <password>s3cret</password>
    </server>
    <server>
      <id>internal</id>
      <username>bob</username>
      <password>hunter2</password>
    </server>
  </servers>
  <mirrors>
    <mirror>
      <id>company-mirror</id>
      <url>https://mirror.example.com/maven2</url>
      <mirrorOf>*</mirrorOf>
    </mirror>
  </mirrors>
  <profiles>
    <profile>
      <id>release</id>
      <properties>
        <build.flavor>release</build.flavor>
      </properties>
    </profile>
  </profiles>
  <activeProfiles>
    <activeProfile>release</activeProfile>
  </activeProfiles>
</settings>
"#,
    )
    .unwrap();

    let inputs = make_inputs(
        sb.home(),
        sb.home(),
        HashMap::new(),
        CliOverrides::default(),
    );
    let (cfg, audit) = load_effective_config(inputs).unwrap();
    let rendered = render_effective(&cfg, &audit);
    insta::assert_snapshot!(sb.redact(&rendered));
}

#[test]
fn barista_toml_with_taps_and_modules() {
    let sb = Sandbox::new();
    let proj_cfg = sb.proj().join("barista.toml");
    fs::write(
        &proj_cfg,
        r#"
[[taps]]
name = "acme"
url = "https://roastery.acme.com"

[[taps]]
name = "internal"
url = "https://roastery.internal"
kind = "worker"

[modules]
excluded = ["legacy-util", "deprecated-mod"]

[plugins]
classloader-cache-overrides = { "org.example:plugin" = "no-cache", "org.example:other" = "cache" }
"#,
    )
    .unwrap();

    let mut inputs = make_inputs(
        sb.home(),
        sb.home(),
        HashMap::new(),
        CliOverrides::default(),
    );
    inputs.project_config_path = Some(proj_cfg);
    let (cfg, audit) = load_effective_config(inputs).unwrap();
    let rendered = render_effective(&cfg, &audit);
    insta::assert_snapshot!(sb.redact(&rendered));
}

#[test]
fn all_six_layers() {
    let sb = Sandbox::new();

    // Layer 2 — user
    let user_cfg = sb.home().join(".barista").join("config.toml");
    fs::create_dir_all(user_cfg.parent().unwrap()).unwrap();
    fs::write(
        &user_cfg,
        "[network]\nrequest-timeout-secs = 30\n[telemetry]\nenabled = true\nclient-id = \"abc-123\"\n",
    )
    .unwrap();

    // Layer 3 — project
    let proj_cfg = sb.proj().join("barista.toml");
    fs::write(
        &proj_cfg,
        "[logging]\nlevel = \"warn\"\n[compat]\nexcluded-modules = [\"big-mod\"]\n",
    )
    .unwrap();

    // Layer 4 — settings.xml
    let m2 = sb.home().join(".m2");
    fs::create_dir_all(&m2).unwrap();
    let settings_path = m2.join("settings.xml");
    fs::write(
        &settings_path,
        r#"<settings><localRepository>/srv/m2</localRepository></settings>"#,
    )
    .unwrap();

    // Layer 5 — env
    let mut env = HashMap::new();
    env.insert("BARISTA_DAEMON__IDLE_SHUTDOWN_SECS".into(), "120".into());
    env.insert("BARISTA_MAVEN__COMPAT_MODE".into(), "three-nine".into());

    // Layer 6 — CLI
    let cli = CliOverrides {
        max_concurrent_connections: Some(20),
        log_level: Some("trace".into()),
        ..Default::default()
    };

    let mut inputs = make_inputs(sb.home(), sb.home(), env, cli);
    inputs.user_config_path = Some(user_cfg);
    inputs.project_config_path = Some(proj_cfg);
    let (cfg, audit) = load_effective_config(inputs).unwrap();
    let rendered = render_effective(&cfg, &audit);
    insta::assert_snapshot!(sb.redact(&rendered));
}

#[test]
fn walk_up_finds_project_toml() {
    let sb = Sandbox::new();
    let proj_cfg = sb.proj().join("barista.toml");
    fs::write(&proj_cfg, "[network]\nmax-concurrent-connections = 9\n").unwrap();
    let nested = sb.proj().join("a/b/c");
    fs::create_dir_all(&nested).unwrap();

    // CWD is nested; loader walks up to find barista.toml at proj root.
    let inputs = make_inputs(sb.home(), &nested, HashMap::new(), CliOverrides::default());
    let (cfg, audit) = load_effective_config(inputs).unwrap();
    let rendered = render_effective(&cfg, &audit);
    insta::assert_snapshot!(sb.redact(&rendered));
}

#[test]
fn compat_mode_via_env() {
    let sb = Sandbox::new();
    let mut env = HashMap::new();
    env.insert("BARISTA_MAVEN__COMPAT_MODE".into(), "4.0".into());
    env.insert("BARISTA_LOGGING__MAVEN_SHAPE".into(), "true".into());

    let inputs = make_inputs(sb.home(), sb.home(), env, CliOverrides::default());
    let (cfg, audit) = load_effective_config(inputs).unwrap();
    let rendered = render_effective(&cfg, &audit);
    insta::assert_snapshot!(sb.redact(&rendered));
}

#[test]
fn cli_cache_dir_and_no_daemon() {
    let sb = Sandbox::new();
    let cli = CliOverrides {
        cache_dir: Some(PathBuf::from("/var/cache/barista")),
        no_daemon: Some(true),
        compat_mode: Some(CompatMode::FourZero),
        ..Default::default()
    };
    let inputs = make_inputs(sb.home(), sb.home(), HashMap::new(), cli);
    let (cfg, audit) = load_effective_config(inputs).unwrap();
    let rendered = render_effective(&cfg, &audit);
    insta::assert_snapshot!(sb.redact(&rendered));
}
