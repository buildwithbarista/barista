// SPDX-License-Identifier: MIT OR Apache-2.0

//! Public error type for `barista-netcap`.
//!
//! The crate has a single error enum so callers (`barista-cli`, the
//! analysis pipeline, integration tests) can pattern-match against
//! specific failure modes — most importantly, the
//! [`NetcapError::MitmproxyMissing`] variant, which is *not* a "bug" but a
//! routine outcome on hosts that haven't installed mitmproxy. Treating it
//! as a typed variant rather than a string lets the CLI emit a clean
//! "please install mitmproxy" message instead of a stack-traceish dump.

use std::io;
use std::path::PathBuf;

/// Failure modes for capture-session operations and CA helpers.
#[derive(Debug, thiserror::Error)]
pub enum NetcapError {
    /// `mitmdump` / `mitmproxy` was not found on `$PATH`. The CLI should
    /// surface this with an install hint; tests should `assume!()`-skip.
    #[error(
        "mitmproxy is not installed (looked for `mitmdump` on PATH); \
         install via `brew install mitmproxy` or `pipx install mitmproxy`"
    )]
    MitmproxyMissing,

    /// The mitmproxy CA certificate was not present at the expected path.
    /// Carries the path we looked at so the caller can render a precise
    /// diagnostic. The status reporter in `ca.rs` prefers this variant over
    /// silently returning "not installed" so a misconfigured `$HOME`
    /// (e.g. inside a container) is distinguishable from a fresh install.
    #[error("mitmproxy CA certificate not found at {path}")]
    CaCertMissing {
        /// The PEM path we expected to find.
        path: PathBuf,
    },

    /// The capture subprocess failed to spawn. Wraps the underlying
    /// [`io::Error`] so callers can drill into permissions / ENOENT cases.
    #[error("failed to spawn capture subprocess `{program}`: {source}")]
    SpawnFailed {
        /// The program name we tried to spawn (typically `mitmdump`).
        program: String,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// The capture subprocess exited before the caller invoked
    /// [`CaptureSession::stop`]. Carries the OS-reported exit status (or
    /// "signalled" when the child was killed by a signal on Unix).
    ///
    /// [`CaptureSession::stop`]: crate::CaptureSession::stop
    #[error("capture subprocess exited prematurely: {status}")]
    SubprocessExited {
        /// Human-rendered status (`exit code: N` or `signalled`).
        status: String,
    },

    /// `CaptureSession::stop` waited longer than [`CaptureConfig::timeout`]
    /// for the subprocess to terminate after sending the stop signal. This
    /// usually means the process was wedged; the session reports it but the
    /// child is force-killed via `kill_on_drop`.
    ///
    /// [`CaptureConfig::timeout`]: crate::CaptureConfig::timeout
    #[error("timed out waiting for capture subprocess to terminate")]
    StopTimeout,

    /// The HAR file produced by the capture was missing, empty, or did not
    /// parse as JSON with a top-level `log` object. The accompanying
    /// message describes which check failed.
    #[error("captured HAR file at {path} failed validation: {reason}")]
    HarInvalid {
        /// The HAR file we inspected.
        path: PathBuf,
        /// Human-readable explanation of which validation step failed.
        reason: String,
    },

    /// Could not bind a free TCP port for the proxy to listen on. Almost
    /// always means the host is out of ephemeral ports.
    #[error("failed to allocate a free TCP port for the proxy listener: {source}")]
    PortAllocation {
        /// The underlying I/O error from the bind attempt.
        #[source]
        source: io::Error,
    },

    /// Catch-all for I/O failures during session bookkeeping (creating the
    /// output directory, reading the HAR, etc.) — kept distinct from
    /// [`Self::SpawnFailed`] so the caller can tell process from
    /// filesystem errors at a glance.
    #[error("netcap I/O error: {0}")]
    Io(#[from] io::Error),
}
