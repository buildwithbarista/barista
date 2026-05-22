// SPDX-License-Identifier: MIT OR Apache-2.0

//! Build-time identity probe for the `/version` ops endpoint.
//!
//! Emits three `cargo:rustc-env=…` directives so the crate can read
//! them back via `env!()` at compile time:
//!
//! - `ROASTERY_BUILD_GIT_SHA` — short (7+ char) git commit SHA, or the
//!   sentinel string `"unknown"` if the working tree isn't a git
//!   checkout / `git` isn't on `PATH`. The handler treats `"unknown"`
//!   as `null` in the JSON response so clean tarball installs don't
//!   surface a lie.
//! - `ROASTERY_BUILD_DATE` — RFC-3339 UTC timestamp. Honors
//!   `SOURCE_DATE_EPOCH` (the cross-ecosystem reproducible-builds
//!   convention, <https://reproducible-builds.org/docs/source-date-epoch/>)
//!   when that variable is set to a Unix-seconds value, so a release
//!   pipeline that pins the timestamp to the tagged commit's date gets
//!   a byte-identical binary across independent builders. Falls back to
//!   `SystemTime::now()` for ordinary local/dev builds, or `"unknown"`
//!   if the clock somehow returns a pre-epoch value.
//! - `ROASTERY_BUILD_RUSTC` — output of `rustc -V`, or `"unknown"` if
//!   the subprocess fails for any reason.
//!
//! **None of these lookups may fail the build.** A failing build
//! script makes `cargo install roastery` impossible on machines that
//! happen not to have `git` on PATH (e.g. container builds from a
//! source tarball). Each probe is wrapped in a graceful fallback that
//! emits `"unknown"` on any error path.
//!
//! We also emit `cargo:rerun-if-changed=build.rs` to suppress the
//! default behaviour of re-running on every source change — the
//! build-time identity only needs to refresh when this file or the
//! current `HEAD` does. `cargo:rerun-if-changed=.git/HEAD` covers the
//! latter without forcing a rebuild when individual `src/` files
//! change.
//!
//! # Bazel REAPI gRPC code generation
//!
//! The second job this script performs is compiling the vendored
//! Protocol Buffer schemas under `proto/` into Rust gRPC bindings via
//! [`tonic_prost_build`]. The output lands in `$OUT_DIR` and is pulled
//! into the crate by `tonic::include_proto!` from `src/proto/reapi.rs`.
//!
//! - **No system `protoc` required.** We point `protoc` at the binary
//!   shipped by the `protoc-bin-vendored` build-dependency (and add its
//!   bundled `google/protobuf/*` well-known-type include directory), so
//!   a clean `cargo build` works on CI and contributor machines with no
//!   protobuf toolchain installed.
//! - **Pinned schemas are the source of truth.** The exact upstream
//!   commits are recorded in `proto/REVISIONS.txt`; bumping a pin is a
//!   re-fetch + rebuild, never a hand-edit of generated code.
//! - **Build vs runtime split (tonic 0.14).** We generate the prost
//!   message types + the tonic service traits for both server and
//!   client (the integration tests drive a generated client), but only
//!   the CAS + Capabilities services are *implemented* by the server.

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---- Bazel REAPI gRPC codegen ---------------------------------------
    // Run this first so a codegen failure surfaces before the (always
    // infallible) build-identity probes below. Returning the error from
    // `main` makes cargo print it and fail the build with a non-zero
    // exit — no `panic!` needed (the workspace lint policy forbids
    // panics in non-test code, build scripts included).
    generate_reapi_bindings()?;

    println!("cargo:rerun-if-changed=build.rs");
    // Re-run when the current commit changes. `.git/HEAD` is the
    // cheapest stable signal across detached-HEAD and branch checkouts.
    // If the file doesn't exist (tarball install), cargo silently
    // ignores the directive — no harm done.
    println!("cargo:rerun-if-changed=.git/HEAD");
    // A reproducible release pins the embedded build date via
    // `SOURCE_DATE_EPOCH`; re-run when it changes so a flip between a
    // pinned release build and an ordinary `now()` dev build is
    // observed by cargo rather than served from a stale rustc-env cache.
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let git_sha = git_short_sha().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=ROASTERY_BUILD_GIT_SHA={git_sha}");

    let build_date = build_date_rfc3339().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=ROASTERY_BUILD_DATE={build_date}");

    let rustc = rustc_version().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=ROASTERY_BUILD_RUSTC={rustc}");

    Ok(())
}

/// Run `git rev-parse --short=12 HEAD` and return the trimmed stdout
/// on success, or `None` on any failure (no git, not a repo, …).
///
/// 12 hex chars is long enough to be unambiguous in practice while
/// still fitting on one line of `/version` JSON output. CI overrides
/// via the standard `GITHUB_SHA` env var if present — that way release
/// builds carry the exact commit the workflow ran on even when the
/// build runs out of a shallow clone.
fn git_short_sha() -> Option<String> {
    if let Ok(env_sha) = std::env::var("GITHUB_SHA")
        && !env_sha.is_empty()
    {
        // Trim to the same 12-char width `git rev-parse --short=12`
        // would emit. `chars().take(…)` is byte-safe for hex.
        let short: String = env_sha.chars().take(12).collect();
        return Some(short);
    }

    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Compose an RFC-3339 timestamp without pulling in `chrono`/`time`
/// as a build dep.
///
/// Source of the seconds value, in priority order:
///   1. `SOURCE_DATE_EPOCH` (Unix seconds) when set to a parseable
///      non-negative integer — the reproducible-builds convention. A
///      release pipeline sets this to the tagged commit's author date
///      so two independent builders embed an identical timestamp.
///   2. `SystemTime::now()` otherwise (ordinary local/dev builds).
///
/// The format is `YYYY-MM-DDTHH:MM:SSZ` (UTC, no fractional seconds).
/// Returns `None` only on the impossible-in-practice case that the
/// build clock is before the Unix epoch (the `SOURCE_DATE_EPOCH` path
/// cannot hit that — a negative / unparseable value falls through to
/// the clock).
fn build_date_rfc3339() -> Option<String> {
    if let Some(secs) = source_date_epoch() {
        return Some(format_rfc3339_utc(secs));
    }
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format_rfc3339_utc(secs))
}

/// Parse `SOURCE_DATE_EPOCH` as Unix seconds. Returns `None` when the
/// variable is unset, empty, or not a non-negative integer (in which
/// case the caller falls back to the wall clock). Per the spec the
/// value is a count of seconds since the Unix epoch.
fn source_date_epoch() -> Option<u64> {
    let raw = std::env::var("SOURCE_DATE_EPOCH").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

/// Format `secs_since_epoch` as an RFC-3339 UTC string. Implemented
/// inline to avoid the `chrono`/`time` build-dep tax for a 5-line
/// calendar walk.
///
/// Handles years from 1970 onwards. Algorithm: civil_from_days from
/// Howard Hinnant's date library
/// (<https://howardhinnant.github.io/date_algorithms.html#civil_from_days>),
/// pared down to the unsigned slice we need (post-1970 dates, no era
/// arithmetic required).
fn format_rfc3339_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let secs_of_day = secs % 86_400;
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;

    // Shift so day 0 is 0000-03-01 (Hinnant's "era" trick). For any
    // build clock at-or-after 1970-01-01 the shifted value stays
    // comfortably positive, so we can do the whole walk in `u64`
    // without `as` casts.
    let z: u64 = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Run `rustc -V` and capture its stdout (e.g.
/// `"rustc 1.84.0 (9fc6b4312 2024-12-30)"`). Returns `None` on any
/// failure — the absence of `rustc` on the build machine is in theory
/// impossible during a `cargo build`, but the graceful path costs us
/// nothing and keeps the build infallible.
fn rustc_version() -> Option<String> {
    // `RUSTC` is set by cargo to the absolute path of the compiler
    // that's about to be invoked; using it instead of bare `rustc`
    // means we report the actual rustc compiling this crate even when
    // multiple toolchains are installed.
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let out = Command::new(rustc).arg("-V").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Compile the vendored REAPI + googleapis `.proto` files into Rust
/// gRPC bindings under `$OUT_DIR`.
///
/// The include root is `proto/` (so the upstream `import` paths like
/// `build/bazel/semver/semver.proto` and `google/rpc/status.proto`
/// resolve against the vendored tree). The `google/protobuf/*`
/// well-known types are *not* vendored — they resolve from the
/// `protoc` install's bundled include directory, which we add
/// explicitly from `protoc-bin-vendored`.
///
/// We point `protoc` at the vendored binary so no system protobuf
/// toolchain is needed. If a contributor has set `PROTOC` themselves
/// we leave their choice alone (the `protoc-bin-vendored` lookup only
/// runs when `PROTOC` is unset).
fn generate_reapi_bindings() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env_var("CARGO_MANIFEST_DIR")?);
    let proto_root = manifest_dir.join("proto");

    // The single REAPI file (defines every service + message) plus the
    // ByteStream service used for large blobs. Their transitive imports
    // are resolved from `proto_root` and the WKT include dir.
    let reapi_proto = proto_root.join("build/bazel/remote/execution/v2/remote_execution.proto");
    let bytestream_proto = proto_root.join("google/bytestream/bytestream.proto");

    // Re-run codegen whenever a vendored proto, this build script, or
    // the protoc override changes. We list the two entry-point protos
    // plus the proto root so a bumped dependency proto is observed.
    println!("cargo:rerun-if-changed={}", reapi_proto.display());
    println!("cargo:rerun-if-changed={}", bytestream_proto.display());
    println!("cargo:rerun-if-changed={}", proto_root.display());
    println!("cargo:rerun-if-env-changed=PROTOC");

    // Vendored protoc: only set PROTOC if the contributor hasn't already
    // pointed it somewhere. `protoc-bin-vendored` ships both the binary
    // and the bundled well-known-type include path.
    if std::env::var_os("PROTOC").is_none() {
        let protoc = protoc_bin_vendored::protoc_bin_path()?;
        // SAFETY: build scripts run single-threaded, before any other
        // crate code; setting `PROTOC` here only influences the
        // `tonic-prost-build`/`prost-build` protoc invocation that
        // follows on this same thread. There is no concurrent reader of
        // the environment to race with. The crate's workspace
        // `unsafe_code` lint warns on `unsafe`; this one block is the
        // documented exception (Rust 2024 made `set_var` unsafe).
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("PROTOC", protoc);
        }
    }
    let wkt_include = protoc_bin_vendored::include_path()?;

    tonic_prost_build::configure()
        // Generate both server (we implement CAS + Capabilities) and
        // client (the integration tests drive a generated client) sides.
        .build_server(true)
        .build_client(true)
        // Emit a single `reapi.rs` that declares the full nested
        // package-module tree (`build::bazel::…`, `google::rpc`, …).
        // The generated code uses relative `super::` paths to reach
        // cross-package types (e.g. a CAS response referencing
        // `google.rpc.Status`), so the modules MUST share the real
        // package hierarchy as ancestors. `include_file` lays that out
        // for us; `src/proto/reapi.rs` `include!`s it at one anchor
        // point and the `super::` chains resolve. (Multiple
        // `include_proto!` calls at flat module names break those
        // chains — hence the single-file approach.)
        .include_file("reapi_generated.rs")
        // Generated code lives in OUT_DIR; it is `include!`d, never
        // checked in, so it is exempt from the workspace clippy gate.
        .compile_protos(&[reapi_proto, bytestream_proto], &[proto_root, wkt_include])?;

    Ok(())
}

/// Read a required environment variable, mapping the absent case to a
/// boxed error so the caller can `?` it. Used for the cargo-provided
/// `CARGO_MANIFEST_DIR`, which is always present during a build.
fn env_var(key: &str) -> Result<String, Box<dyn std::error::Error>> {
    std::env::var(key).map_err(|e| format!("env var {key}: {e}").into())
}
