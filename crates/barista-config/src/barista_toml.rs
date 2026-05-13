//! `barista.toml` — project-local configuration schema.
//!
//! This module extends the base [`PartialConfig`](crate::schema::PartialConfig)
//! (defined in `schema.rs`) with project-level concerns that don't
//! apply to user-level config: tap declarations, per-module
//! overrides, plugin classloader-cache overrides, and optional
//! project metadata.
//!
//! `barista.toml` is loaded by the layered loader (`sources.rs`) as
//! the third layer (project beats user; env + CLI beat both). This
//! module defines the project-only schema and the combined
//! [`ProjectConfigFile`] type the loader deserializes into.
//!
//! ## Layout of a `barista.toml`
//!
//! A `barista.toml` file is the flat union of the base config
//! sections (`[network]`, `[daemon]`, …) and the project-only
//! sections (`[project]`, `[[taps]]`, `[modules]`, `[plugins]`).
//! `#[serde(flatten)]` is used in [`ProjectConfigFile`] so both
//! groups can co-exist at the top level.
//!
//! ```toml
//! [project]
//! name = "my-app"
//!
//! [network]
//! max-concurrent-connections = 12
//!
//! [[taps]]
//! id = "acme"
//! url = "https://taps.acme.com/barback-pool"
//!
//! [modules]
//! excluded = ["legacy-thing"]
//!
//! [modules.overrides."foo-module"]
//! network = { max-concurrent-connections = 4 }
//!
//! [plugins]
//! classloader-cache-overrides = { "org.apache.maven.plugins:maven-clean-plugin" = "no-cache" }
//! ```
//!
//! Per-module override APPLICATION (selecting which module is
//! currently being built and projecting its overrides onto the
//! effective config) is out of scope for this module; it lands with
//! the resolver / CLI integration in a later milestone. This module
//! only defines the schema and surfaces it on
//! [`Config::project_extensions`](crate::schema::Config::project_extensions).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::schema::{
    PartialConfig, PartialDaemonConfig, PartialLoggingConfig, PartialMavenConfig,
    PartialNetworkConfig,
};

// ============================================================
// Project-only schema
// ============================================================

/// Project-local extensions to [`Config`](crate::schema::Config).
///
/// Loaded from `barista.toml`, attached to the effective config as
/// [`Config::project_extensions`](crate::schema::Config::project_extensions).
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct BaristaTomlExtensions {
    /// Optional `[project]` metadata, overriding values normally
    /// derived from `pom.xml`.
    #[serde(default)]
    pub project: Option<ProjectMetadata>,

    /// `[[taps]]` — remote `barback` worker pools the project can
    /// offload to.
    #[serde(default)]
    pub taps: Vec<TapDecl>,

    /// `[modules]` — module exclusions and per-module overrides.
    #[serde(default)]
    pub modules: ModulesConfig,

    /// `[plugins]` — plugin classloader-cache policy overrides.
    #[serde(default)]
    pub plugins: PluginsConfig,
}

/// Optional `[project]` block. Each field, if present, overrides
/// the corresponding value derived from `pom.xml`.
///
/// These overrides exist for monorepo or multi-language situations
/// where the source-of-truth for the coordinate differs from what a
/// generated POM happens to say.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProjectMetadata {
    pub name: Option<String>,
    pub group_id: Option<String>,
    pub artifact_id: Option<String>,
    pub version: Option<String>,
}

/// A single `[[taps]]` entry.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TapDecl {
    /// Stable identifier for this tap. Used in logs and CLI flags.
    pub id: String,

    /// HTTPS URL of the tap's barback pool entry point.
    pub url: String,

    /// Optional bearer-token credential. May be a literal token or
    /// an `${env.VAR}` reference; expansion is the tap client's
    /// responsibility.
    pub auth: Option<String>,

    /// Optional load-balancing weight. The tap client treats `None`
    /// as 1.
    pub weight: Option<u32>,

    /// Whether this tap is eligible to be picked. Defaults to true
    /// when absent.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// `[modules]` — module exclusions and per-module config overrides.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ModulesConfig {
    /// Module artifactIds to skip in default builds. Merged into
    /// the same effective set as `compat.excluded-modules` by
    /// downstream code; kept here as a convenience for projects
    /// that prefer the shorter spelling.
    #[serde(default)]
    pub excluded: Vec<String>,

    /// Per-module overrides. Each key is a module's `artifactId`;
    /// the value is a subset of [`PartialConfig`] applied when that
    /// module is the build target.
    #[serde(default)]
    pub overrides: BTreeMap<String, ModuleOverride>,
}

/// Per-module subset of [`PartialConfig`]. Same field names, same
/// deserialization semantics.
///
/// Application — i.e. projecting these onto the effective config
/// when Barista knows which module is currently being built — is
/// downstream of this module and is not implemented here.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ModuleOverride {
    pub network: Option<PartialNetworkConfig>,
    pub daemon: Option<PartialDaemonConfig>,
    pub maven: Option<PartialMavenConfig>,
    pub logging: Option<PartialLoggingConfig>,
}

/// `[plugins]` block — plugin classloader-cache policy overrides.
///
/// Barista caches Maven-plugin classloaders by default. A handful
/// of plugins misbehave with the cache; this map lets a project
/// opt specific coordinates out.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PluginsConfig {
    /// Plugin coordinate (`groupId:artifactId`) → policy.
    #[serde(default)]
    pub classloader_cache_overrides: BTreeMap<String, ClassloaderCachePolicy>,
}

/// Classloader-cache policy for an individual plugin coordinate.
///
/// The default for plugins not listed in
/// [`PluginsConfig::classloader_cache_overrides`] is
/// [`ClassloaderCachePolicy::Cache`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClassloaderCachePolicy {
    /// Reuse a cached classloader across invocations of this
    /// plugin.
    Cache,
    /// Always create a fresh classloader for this plugin.
    NoCache,
}

// ============================================================
// Combined deserialization target for `barista.toml`
// ============================================================

/// Wire format of a project-level `barista.toml`. Combines the
/// base [`PartialConfig`] (network / daemon / paths / maven /
/// logging / telemetry / compat) and the project-only
/// [`BaristaTomlExtensions`] (project / taps / modules / plugins)
/// at the same top-level scope via `#[serde(flatten)]`.
///
/// This is what `sources.rs` parses the project file into.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProjectConfigFile {
    #[serde(flatten)]
    pub base: PartialConfig,

    #[serde(flatten)]
    pub extensions: BaristaTomlExtensions,
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    // 1. Empty `BaristaTomlExtensions::default()` round-trips
    //    through TOML — an empty serialized form deserializes back
    //    to the default.
    #[test]
    fn test_01_default_extensions_round_trip() {
        let default = BaristaTomlExtensions::default();
        let serialized = toml::to_string(&default).unwrap();
        let parsed: BaristaTomlExtensions = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, default);
        // And the obvious empty input parses too.
        let empty: BaristaTomlExtensions = toml::from_str("").unwrap();
        assert_eq!(empty, BaristaTomlExtensions::default());
    }

    // 2. `[project]` metadata block parses.
    #[test]
    fn test_02_project_metadata_parses() {
        let src = r#"
[project]
name = "my-app"
group-id = "com.acme"
artifact-id = "my-app"
version = "1.2.3"
"#;
        let ext: BaristaTomlExtensions = toml::from_str(src).unwrap();
        let p = ext.project.expect("project block");
        assert_eq!(p.name.as_deref(), Some("my-app"));
        assert_eq!(p.group_id.as_deref(), Some("com.acme"));
        assert_eq!(p.artifact_id.as_deref(), Some("my-app"));
        assert_eq!(p.version.as_deref(), Some("1.2.3"));
    }

    // 3. `[[taps]]` array — 1 tap then 3 taps.
    #[test]
    fn test_03_taps_array_parses() {
        let one = r#"
[[taps]]
id = "acme"
url = "https://taps.acme.com/barback-pool"
"#;
        let ext: BaristaTomlExtensions = toml::from_str(one).unwrap();
        assert_eq!(ext.taps.len(), 1);
        assert_eq!(ext.taps[0].id, "acme");
        assert_eq!(ext.taps[0].url, "https://taps.acme.com/barback-pool");

        let three = r#"
[[taps]]
id = "a"
url = "https://a.example/b"

[[taps]]
id = "b"
url = "https://b.example/b"
weight = 2

[[taps]]
id = "c"
url = "https://c.example/b"
enabled = false
"#;
        let ext: BaristaTomlExtensions = toml::from_str(three).unwrap();
        assert_eq!(ext.taps.len(), 3);
        assert_eq!(ext.taps[1].weight, Some(2));
        assert!(!ext.taps[2].enabled);
    }

    // 4. Tap with `auth = "${env.TOKEN}"` parses literally.
    #[test]
    fn test_04_tap_auth_env_reference() {
        let src = r#"
[[taps]]
id = "private"
url = "https://taps.priv/b"
auth = "${env.TOKEN}"
"#;
        let ext: BaristaTomlExtensions = toml::from_str(src).unwrap();
        assert_eq!(ext.taps[0].auth.as_deref(), Some("${env.TOKEN}"));
    }

    // 5. Tap `enabled` defaults to true when absent.
    #[test]
    fn test_05_tap_enabled_defaults_true() {
        let src = r#"
[[taps]]
id = "x"
url = "https://x/y"
"#;
        let ext: BaristaTomlExtensions = toml::from_str(src).unwrap();
        assert!(ext.taps[0].enabled);
        assert!(ext.taps[0].weight.is_none());
        assert!(ext.taps[0].auth.is_none());
    }

    // 6. `[modules]` with `excluded` list parses.
    #[test]
    fn test_06_modules_excluded_parses() {
        let src = r#"
[modules]
excluded = ["legacy-thing", "experimental-module"]
"#;
        let ext: BaristaTomlExtensions = toml::from_str(src).unwrap();
        assert_eq!(
            ext.modules.excluded,
            vec![
                "legacy-thing".to_string(),
                "experimental-module".to_string()
            ]
        );
        assert!(ext.modules.overrides.is_empty());
    }

    // 7. `[modules.overrides."foo-bar"]` per-module override parses.
    #[test]
    fn test_07_module_override_parses() {
        let src = r#"
[modules.overrides."foo-bar"]
[modules.overrides."foo-bar".network]
max-concurrent-connections = 4
"#;
        let ext: BaristaTomlExtensions = toml::from_str(src).unwrap();
        let foo = ext
            .modules
            .overrides
            .get("foo-bar")
            .expect("foo-bar override");
        let net = foo.network.as_ref().expect("network override");
        assert_eq!(net.max_concurrent_connections, Some(4));
    }

    // 8. Per-module `network.max-concurrent-connections = 12` parses
    //    via the inline-table style as well.
    #[test]
    fn test_08_module_override_inline_network() {
        let src = r#"
[modules.overrides."svc"]
network = { max-concurrent-connections = 12, http2-enabled = false }
maven = { compat-mode = "three-nine" }
"#;
        let ext: BaristaTomlExtensions = toml::from_str(src).unwrap();
        let svc = ext.modules.overrides.get("svc").unwrap();
        let net = svc.network.as_ref().unwrap();
        assert_eq!(net.max_concurrent_connections, Some(12));
        assert_eq!(net.http2_enabled, Some(false));
        let mv = svc.maven.as_ref().unwrap();
        assert_eq!(mv.compat_mode, Some(crate::schema::CompatMode::ThreeNine));
    }

    // 9. `[plugins.classloader-cache-overrides]` map parses.
    #[test]
    fn test_09_plugins_classloader_cache_overrides() {
        let src = r#"
[plugins]
classloader-cache-overrides = { "org.apache.maven.plugins:maven-clean-plugin" = "no-cache", "com.example:tidy-plugin" = "cache" }
"#;
        let ext: BaristaTomlExtensions = toml::from_str(src).unwrap();
        let map = &ext.plugins.classloader_cache_overrides;
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("org.apache.maven.plugins:maven-clean-plugin"),
            Some(&ClassloaderCachePolicy::NoCache)
        );
        assert_eq!(
            map.get("com.example:tidy-plugin"),
            Some(&ClassloaderCachePolicy::Cache)
        );
    }

    // 10. `ClassloaderCachePolicy` round-trips through TOML
    //     (kebab-case for `no-cache`).
    #[test]
    fn test_10_policy_round_trip() {
        #[derive(Deserialize, Serialize, PartialEq, Debug)]
        struct Wrap {
            policy: ClassloaderCachePolicy,
        }
        let nc = Wrap {
            policy: ClassloaderCachePolicy::NoCache,
        };
        let s = toml::to_string(&nc).unwrap();
        assert!(s.contains("no-cache"));
        let back: Wrap = toml::from_str(&s).unwrap();
        assert_eq!(back, nc);

        let c = Wrap {
            policy: ClassloaderCachePolicy::Cache,
        };
        let s = toml::to_string(&c).unwrap();
        let back: Wrap = toml::from_str(&s).unwrap();
        assert_eq!(back, c);
    }

    // 11. `deny_unknown_fields` rejects the typo
    //     `classloader-cach-overrides` (missing 'e') with a
    //     useful message.
    #[test]
    fn test_11_unknown_field_errors_on_plugins_typo() {
        let src = r#"
[plugins]
classloader-cach-overrides = { "x:y" = "cache" }
"#;
        let err = toml::from_str::<BaristaTomlExtensions>(src).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("classloader-cach-overrides") || msg.contains("unknown"),
            "expected unknown-field error, got: {msg}"
        );
    }

    // 12. End-to-end: a `ProjectConfigFile` with both `[network]`
    //     (base) and `[[taps]]` (extension) at the top level.
    #[test]
    fn test_12_combined_file_flatten() {
        let src = r#"
[network]
max-concurrent-connections = 7

[[taps]]
id = "acme"
url = "https://taps.acme.com/b"
"#;
        let file: ProjectConfigFile = toml::from_str(src).unwrap();
        let net = file.base.network.expect("network");
        assert_eq!(net.max_concurrent_connections, Some(7));
        assert_eq!(file.extensions.taps.len(), 1);
        assert_eq!(file.extensions.taps[0].id, "acme");
    }

    // 13. `ProjectConfigFile` denies unknown top-level fields too.
    #[test]
    fn test_13_combined_file_denies_unknown_top_level() {
        let src = r#"
[bogus-section]
x = 1
"#;
        let err = toml::from_str::<ProjectConfigFile>(src).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bogus-section") || msg.contains("unknown"),
            "expected unknown-field error, got: {msg}"
        );
    }

    // 14. Empty `ProjectConfigFile` parses to all-defaults.
    #[test]
    fn test_14_empty_combined_file() {
        let file: ProjectConfigFile = toml::from_str("").unwrap();
        assert!(file.base.network.is_none());
        assert!(file.extensions.project.is_none());
        assert!(file.extensions.taps.is_empty());
        assert!(file.extensions.modules.excluded.is_empty());
        assert!(file.extensions.modules.overrides.is_empty());
        assert!(
            file.extensions
                .plugins
                .classloader_cache_overrides
                .is_empty()
        );
    }

    // 15. `[project]` block partially populated — only `name`.
    #[test]
    fn test_15_project_partial() {
        let src = r#"
[project]
name = "just-name"
"#;
        let ext: BaristaTomlExtensions = toml::from_str(src).unwrap();
        let p = ext.project.unwrap();
        assert_eq!(p.name.as_deref(), Some("just-name"));
        assert!(p.group_id.is_none());
        assert!(p.artifact_id.is_none());
        assert!(p.version.is_none());
    }

    // 16. Unknown field in a `[[taps]]` entry errors.
    #[test]
    fn test_16_tap_unknown_field_errors() {
        let src = r#"
[[taps]]
id = "x"
url = "https://x/y"
priority = 5
"#;
        let err = toml::from_str::<BaristaTomlExtensions>(src).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("priority") || msg.contains("unknown"),
            "expected unknown-field error, got: {msg}"
        );
    }

    // 17. Unknown policy variant errors.
    #[test]
    fn test_17_unknown_policy_variant_errors() {
        let src = r#"
[plugins]
classloader-cache-overrides = { "x:y" = "sometimes" }
"#;
        let err = toml::from_str::<BaristaTomlExtensions>(src).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("sometimes") || msg.to_lowercase().contains("variant"),
            "expected variant error, got: {msg}"
        );
    }
}
