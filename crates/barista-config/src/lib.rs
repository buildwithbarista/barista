// Workspace security lints (clippy::unwrap_used, clippy::expect_used,
// clippy::panic, clippy::as_conversions) are warned on workspace-wide via
// the root `Cargo.toml`. Pre-existing parser internals in this crate are
// allowed here while the codebase incrementally ratchets them down.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Layered configuration loader for Barista.
//!
//! Resolves the effective [`Config`] from six sources in increasing
//! precedence order:
//!
//! 1. Compiled defaults.
//! 2. `~/.barista/config.toml` — user-level.
//! 3. `./barista.toml` — project-level (walks up from CWD,
//!    stopping at a directory containing `.git` or at the
//!    filesystem root).
//! 4. `~/.m2/settings.xml` — Maven settings (servers, mirrors,
//!    profiles, proxies, plugin groups). Parsed and exposed via
//!    `Config::maven_settings` for downstream consumers.
//! 5. `BARISTA_*` environment variables (double-underscore as
//!    nested-field separator, ALL CAPS).
//! 6. CLI flag overrides (highest precedence).
//!
//! Each layer is loaded into a partial form (every field
//! `Option`-typed) and merged on top of the running effective
//! config; later layers win on conflict.
//!
//! ## Path expansion
//!
//! Paths read from TOML or environment variables may begin with
//! `~/` or `~`; these are expanded against the resolved HOME
//! directory (or the test [`LoaderInputs::home_override`]).
//! Absolute and relative paths are kept verbatim.

pub mod barista_toml;
pub mod dot_mvn;
pub mod schema;
pub mod settings_xml;
pub mod sources;

pub use barista_toml::{
    BaristaTomlExtensions, ClassloaderCachePolicy, ModuleOverride, ModulesConfig, PluginsConfig,
    ProjectConfigFile, ProjectMetadata, TapDecl,
};
pub use dot_mvn::{
    DotMvnConfig, DotMvnError, ExtensionSurvey, load_dot_mvn, survey_extensions,
    warn_extensions_unsupported,
};
pub use schema::*;
pub use settings_xml::{
    Activation, Mirror, Proxy, Repository, RepositoryPolicy, Server, SettingsError, SettingsXml,
    XmlProfile, decrypt_password, parse_settings_xml,
};
pub use sources::{
    CliOverrides, LayerAudit, LayerSource, LoadAudit, LoaderError, LoaderInputs,
    load_effective_config, load_settings_xml,
};
