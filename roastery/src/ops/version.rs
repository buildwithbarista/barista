// SPDX-License-Identifier: MIT OR Apache-2.0

//! `GET /version` — build identity.
//!
//! Returns a small JSON document carrying the crate version (from
//! `CARGO_PKG_VERSION`, set by cargo on every build) plus the
//! best-effort identity emitted by `build.rs`: short git SHA, build
//! date (RFC-3339 UTC), and `rustc -V` output.
//!
//! The three build-script fields all fall back to the literal string
//! `"unknown"` when the lookup fails (clean tarball install with no
//! git, missing `rustc`, …). To keep the JSON contract truthful, the
//! handler maps `"unknown"` to `null` so consumers can distinguish
//! "we don't know" from "we do know and the value is the string
//! `unknown`."
//!
//! ## Shape
//!
//! ```json
//! {
//!   "name": "roastery",
//!   "version": "0.1.0-alpha.0",
//!   "git_sha": "abc123def456",
//!   "build_date": "2026-05-19T12:34:56Z",
//!   "rustc": "rustc 1.84.0 (9fc6b4312 2024-12-30)"
//! }
//! ```
//!
//! Any of `git_sha`, `build_date`, `rustc` may be `null`.

use axum::Json;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Crate version, sourced from `Cargo.toml`'s `version` field at
/// compile time. Always non-null.
const NAME: &str = "roastery";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build-script-emitted identity. Each may be the sentinel string
/// `"unknown"`; [`normalise`] maps that to JSON `null`.
const BUILD_GIT_SHA: &str = env!("ROASTERY_BUILD_GIT_SHA");
const BUILD_DATE: &str = env!("ROASTERY_BUILD_DATE");
const BUILD_RUSTC: &str = env!("ROASTERY_BUILD_RUSTC");

/// Sentinel for "we couldn't determine this at build time." See
/// `build.rs` for the failure modes that emit it.
const UNKNOWN: &str = "unknown";

/// JSON body returned by [`version_handler`].
///
/// Fields are `Option<&'static str>` so the `unknown` sentinel can be
/// serialised as JSON `null` rather than the literal string
/// `"unknown"`. The `serde` default skips nothing — we want the keys
/// present-but-null in the response so clients can write
/// `body["git_sha"]` without checking for key existence.
#[derive(Debug, Serialize)]
pub struct VersionBody {
    /// Crate name. Always `"roastery"`.
    pub name: &'static str,
    /// Crate version from `Cargo.toml` (e.g. `"0.1.0-alpha.0"`).
    pub version: &'static str,
    /// Short git SHA of the source tree, or `null` if not known.
    pub git_sha: Option<&'static str>,
    /// RFC-3339 UTC timestamp captured at compile time, or `null`.
    pub build_date: Option<&'static str>,
    /// `rustc -V` output, or `null` if it couldn't run.
    pub rustc: Option<&'static str>,
}

/// Map the build-time sentinel `"unknown"` to `None`; everything else
/// passes through as `Some(s)`.
fn normalise(s: &'static str) -> Option<&'static str> {
    if s == UNKNOWN { None } else { Some(s) }
}

/// `GET /version` — emit the build-identity JSON body.
pub async fn version_handler() -> Response {
    Json(VersionBody {
        name: NAME,
        version: VERSION,
        git_sha: normalise(BUILD_GIT_SHA),
        build_date: normalise(BUILD_DATE),
        rustc: normalise(BUILD_RUSTC),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn normalise_maps_unknown_to_none() {
        assert_eq!(normalise("unknown"), None);
        assert_eq!(
            normalise("rustc 1.84.0 (abc 2024-01-01)"),
            Some("rustc 1.84.0 (abc 2024-01-01)")
        );
        // Empty string is *not* the sentinel — we'd rather surface
        // "the build script ran and produced an empty value, please
        // investigate" than silently null it out.
        assert_eq!(normalise(""), Some(""));
    }

    #[test]
    fn name_is_crate_name() {
        // Cheap sanity check that we're surfacing the intended crate
        // name — the const-emptiness check clippy would flag is
        // redundant (these are all `&'static str` literals built at
        // compile time via `env!` or string-literal assignment, so
        // their non-emptiness is enforced by `build.rs` + cargo).
        assert_eq!(NAME, "roastery");
    }
}
