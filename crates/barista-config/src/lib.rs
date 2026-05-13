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
//!    profiles). Currently a stub; a real parser lands in a
//!    subsequent task.
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

pub mod schema;
pub mod sources;

pub use schema::*;
pub use sources::{
    CliOverrides, LayerAudit, LayerSource, LoadAudit, LoaderError, LoaderInputs, Mirror, Server,
    SettingsXml, XmlProfile, load_effective_config, load_settings_xml,
};
