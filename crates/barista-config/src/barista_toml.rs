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
//! name = "acme"
//! url = "https://roastery.acme.com"
//! kind = "roastery"
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

/// A single `[[taps]]` entry — the persisted on-disk shape of a
/// registered tap.
///
/// A tap is a named, registered remote endpoint: either a
/// [`roastery`](TapKindDecl::Roastery) shared-cache server or a
/// (placeholder) [`worker`](TapKindDecl::Worker). v0.1 ships
/// **registration and inspection only** — recording the endpoint and
/// health-probing it. Routing build actions to a tap is out of scope
/// for v0.1.
///
/// This is the persistence shape; the domain types (validation,
/// registry operations, health probing) live in the `barista-tap`
/// crate, which bridges to and from this struct.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TapDecl {
    /// Unique, human-readable name for this tap. Used as the lookup
    /// key for `tap remove` / `tap status <name>`.
    pub name: String,

    /// Absolute `http`/`https` URL of the tap endpoint. For a
    /// roastery tap, the base URL of the cache server; for a worker
    /// tap, the worker's entry point.
    pub url: String,

    /// What kind of endpoint this tap points at. Defaults to
    /// [`roastery`](TapKindDecl::Roastery) when omitted.
    #[serde(default)]
    pub kind: TapKindDecl,
}

/// On-disk spelling of a tap's kind.
///
/// Mirrors `barista_tap::TapKind`; kept here so `barista-config`
/// owns the serde shape without depending on `barista-tap`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TapKindDecl {
    /// A roastery shared-cache server (the common case). Probed via
    /// its unauthenticated `/healthz` endpoint.
    #[default]
    Roastery,
    /// A remote worker endpoint. Placeholder in v0.1 — registered
    /// and liveness-probed, but never routed to.
    Worker,
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
// `[[taps]]` persistence
// ============================================================

/// Errors raised while loading or persisting the `[[taps]]` section
/// of a `barista.toml`.
#[derive(Debug, thiserror::Error)]
pub enum TapPersistError {
    /// Reading the existing `barista.toml` failed.
    #[error("reading {path}: {source}")]
    Read {
        /// The file we tried to read.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Writing the new `barista.toml` failed.
    #[error("writing {path}: {source}")]
    Write {
        /// The file we tried to write.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The existing file is not valid TOML.
    #[error("parsing {path}: {detail}")]
    Parse {
        /// The file we tried to parse.
        path: std::path::PathBuf,
        /// A human-readable parse-error description.
        detail: String,
    },
    /// Re-serializing the mutated document failed.
    #[error("serializing taps: {0}")]
    Serialize(String),
}

/// Read just the `[[taps]]` array from a `barista.toml`.
///
/// A missing file (or a file with no `[[taps]]` section) yields an
/// empty `Vec` — the caller treats "no taps registered" and "no
/// config file yet" identically, which keeps `tap list` and
/// `tap remove` clean no-ops on a fresh project.
///
/// Returns a [`TapPersistError`] only when the file exists but is
/// unreadable or is not valid TOML / not a valid `[[taps]]` shape.
pub fn load_taps(path: &std::path::Path) -> Result<Vec<TapDecl>, TapPersistError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(TapPersistError::Read {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };
    let file: ProjectConfigFile =
        toml::from_str(&raw).map_err(|e| TapPersistError::Parse {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;
    Ok(file.extensions.taps)
}

/// Persist `taps` into the `[[taps]]` section of `barista.toml`,
/// preserving every other section of the file.
///
/// The write is atomic: the new document is written to a sibling
/// temp file and then renamed over the target, so a crash mid-write
/// can never leave a half-written `barista.toml` behind.
///
/// # Reformatting caveat
///
/// To preserve sibling sections without depending on a
/// format-preserving editor, this parses the existing file into a
/// generic [`toml::Table`], replaces only the `taps` key, and
/// reserializes. The *values* of other sections are preserved
/// exactly, but the document is re-emitted by the TOML serializer,
/// so comments and the original key ordering/whitespace are not
/// preserved. This is an accepted v0.1 trade-off; the data
/// round-trips losslessly even though the byte-for-byte formatting
/// may change.
pub fn save_taps(path: &std::path::Path, taps: &[TapDecl]) -> Result<(), TapPersistError> {
    // Start from the existing document (as a generic table) so we
    // keep every other section. A missing file starts from an empty
    // table.
    let mut doc: toml::Table = match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e| TapPersistError::Parse {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => {
            return Err(TapPersistError::Read {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };

    if taps.is_empty() {
        // Drop the section entirely when empty so an emptied registry
        // round-trips back to a tap-less file (and `load_taps`
        // returns an empty Vec rather than an empty array literal).
        doc.remove("taps");
    } else {
        // Serialize the typed taps through serde so the kebab-case /
        // enum spellings match the deserialization shape, then splice
        // the resulting array of tables into the document.
        let value = toml::Value::try_from(taps)
            .map_err(|e| TapPersistError::Serialize(e.to_string()))?;
        doc.insert("taps".to_string(), value);
    }

    let rendered =
        toml::to_string_pretty(&doc).map_err(|e| TapPersistError::Serialize(e.to_string()))?;

    write_atomically(path, &rendered).map_err(|source| TapPersistError::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// Write `contents` to `path` atomically (temp-file + rename),
/// creating the parent directory if needed.
fn write_atomically(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension("toml.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
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
name = "acme"
url = "https://roastery.acme.com"
"#;
        let ext: BaristaTomlExtensions = toml::from_str(one).unwrap();
        assert_eq!(ext.taps.len(), 1);
        assert_eq!(ext.taps[0].name, "acme");
        assert_eq!(ext.taps[0].url, "https://roastery.acme.com");
        // Kind defaults to roastery when omitted.
        assert_eq!(ext.taps[0].kind, TapKindDecl::Roastery);

        let three = r#"
[[taps]]
name = "a"
url = "https://a.example"

[[taps]]
name = "b"
url = "https://b.example"
kind = "roastery"

[[taps]]
name = "c"
url = "https://c.example"
kind = "worker"
"#;
        let ext: BaristaTomlExtensions = toml::from_str(three).unwrap();
        assert_eq!(ext.taps.len(), 3);
        assert_eq!(ext.taps[1].kind, TapKindDecl::Roastery);
        assert_eq!(ext.taps[2].kind, TapKindDecl::Worker);
    }

    // 4. A `kind = "worker"` tap parses to the worker variant.
    #[test]
    fn test_04_tap_kind_worker() {
        let src = r#"
[[taps]]
name = "builder"
url = "https://worker.priv"
kind = "worker"
"#;
        let ext: BaristaTomlExtensions = toml::from_str(src).unwrap();
        assert_eq!(ext.taps[0].kind, TapKindDecl::Worker);
    }

    // 5. Tap `kind` defaults to roastery when absent.
    #[test]
    fn test_05_tap_kind_defaults_roastery() {
        let src = r#"
[[taps]]
name = "x"
url = "https://x.example"
"#;
        let ext: BaristaTomlExtensions = toml::from_str(src).unwrap();
        assert_eq!(ext.taps[0].kind, TapKindDecl::Roastery);
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
name = "acme"
url = "https://roastery.acme.com"
"#;
        let file: ProjectConfigFile = toml::from_str(src).unwrap();
        let net = file.base.network.expect("network");
        assert_eq!(net.max_concurrent_connections, Some(7));
        assert_eq!(file.extensions.taps.len(), 1);
        assert_eq!(file.extensions.taps[0].name, "acme");
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
name = "x"
url = "https://x.example"
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

    // ---------- [[taps]] persistence ----------

    use tempfile::TempDir;

    fn tap(name: &str, url: &str, kind: TapKindDecl) -> TapDecl {
        TapDecl {
            name: name.to_string(),
            url: url.to_string(),
            kind,
        }
    }

    // 18. Loading taps from a non-existent file yields an empty Vec
    //     (a fresh project has no `barista.toml` yet).
    #[test]
    fn test_18_load_taps_missing_file_is_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("barista.toml");
        let taps = load_taps(&path).unwrap();
        assert!(taps.is_empty());
    }

    // 19. save -> load round-trips the taps losslessly, including the
    //     kind, and persists across a fresh load ("restart").
    #[test]
    fn test_19_save_then_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("barista.toml");
        let taps = vec![
            tap("acme", "https://roastery.acme.com", TapKindDecl::Roastery),
            tap("builder", "https://worker.acme.com", TapKindDecl::Worker),
        ];
        save_taps(&path, &taps).unwrap();

        // Fresh load — simulates a separate process / restart.
        let loaded = load_taps(&path).unwrap();
        assert_eq!(loaded, taps);
        // And the full effective parse sees them too.
        let ext: BaristaTomlExtensions =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(ext.taps.len(), 2);
        assert_eq!(ext.taps[0].name, "acme");
        assert_eq!(ext.taps[1].kind, TapKindDecl::Worker);
    }

    // 20. Saving over a file with other sections preserves those
    //     sections' values (network, project) — only `[[taps]]` is
    //     touched.
    #[test]
    fn test_20_save_preserves_other_sections() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("barista.toml");
        std::fs::write(
            &path,
            r#"
[project]
name = "my-app"

[network]
max-concurrent-connections = 11
"#,
        )
        .unwrap();

        save_taps(&path, &[tap("r", "https://r.example", TapKindDecl::Roastery)]).unwrap();

        // Re-parse the whole file: network + project survived, taps added.
        let raw = std::fs::read_to_string(&path).unwrap();
        let file: ProjectConfigFile = toml::from_str(&raw).unwrap();
        assert_eq!(
            file.base.network.unwrap().max_concurrent_connections,
            Some(11)
        );
        assert_eq!(
            file.extensions.project.unwrap().name.as_deref(),
            Some("my-app")
        );
        assert_eq!(file.extensions.taps.len(), 1);
        assert_eq!(file.extensions.taps[0].name, "r");
    }

    // 21. Saving an empty slice drops the `[[taps]]` section entirely
    //     so an emptied registry round-trips back to a tap-less file.
    #[test]
    fn test_21_save_empty_removes_section() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("barista.toml");
        save_taps(&path, &[tap("r", "https://r.example", TapKindDecl::Roastery)]).unwrap();
        assert_eq!(load_taps(&path).unwrap().len(), 1);

        save_taps(&path, &[]).unwrap();
        assert!(load_taps(&path).unwrap().is_empty());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("[[taps]]"), "taps section should be gone:\n{raw}");
    }

    // 22. Backward-compat: an existing config with no `[[taps]]`
    //     section still parses cleanly and yields no taps.
    #[test]
    fn test_22_backward_compat_no_taps_section() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("barista.toml");
        std::fs::write(&path, "[network]\nmax-concurrent-connections = 3\n").unwrap();
        let taps = load_taps(&path).unwrap();
        assert!(taps.is_empty());
    }
}
