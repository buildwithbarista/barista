//! Capture-session driver.
//!
//! Wraps `mitmdump` (or any user-supplied substitute via
//! [`CaptureConfig::program`]) as a Tokio child process and exposes a
//! `start` / `stop` lifecycle that's safe across panics: the child is
//! tagged `kill_on_drop(true)` so if the caller is unwound mid-capture,
//! the mitmdump process is reaped instead of orphaned.
//!
//! ## Why a child process, not an embedded proxy
//!
//! mitmproxy is the canonical mature implementation of "MITM TLS proxy
//! that emits HAR output." Re-implementing it in Rust to avoid the
//! subprocess would be a 12-month research project; PRD §18.8 explicitly
//! names mitmproxy as the transport, so we drive the real thing. The
//! cost is: callers must have mitmproxy on `$PATH` to run an *actual*
//! capture. The [`CaptureConfig::program`] override exists so the unit
//! tests can drive the lifecycle with a portable stub (`/bin/sh`-based
//! sleep loop) and not require mitmproxy to be installed in CI.
//!
//! ## Lifecycle
//!
//! 1. `CaptureSession::start(config)` — allocates a free TCP port (or
//!    honours the caller's pin), spawns the proxy, and returns the
//!    handle.
//! 2. Build-tool traffic is routed through `127.0.0.1:<listen_port>`
//!    (caller wires that up via `https.proxyHost`/`https.proxyPort` on
//!    the JVM, or `HTTPS_PROXY=` in env).
//! 3. `CaptureSession::stop(self)` — sends SIGTERM, waits up to
//!    `config.timeout`, validates the emitted HAR, and returns a
//!    [`CaptureSummary`].

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::time;

use crate::ca::locate_mitmdump;
use crate::error::NetcapError;
use crate::har::{self, HarSummary};

/// How long to wait for the proxy subprocess to flush its HAR and exit
/// after we send the stop signal, if the caller does not override it.
const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Parameters for a single capture session.
///
/// Construct with [`CaptureConfig::for_har`], then tweak fields as
/// needed.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Absolute path the proxy should write its HAR to. The parent
    /// directory is created if it doesn't already exist.
    pub output_har_path: PathBuf,

    /// TCP port the proxy should listen on. `None` requests a free
    /// ephemeral port (the typical choice — pinning a port is only
    /// useful when the build tool's proxy config is set in advance).
    pub listen_port: Option<u16>,

    /// Upstream proxy to chain through (corporate-network case).
    /// Forwarded to mitmdump as `--mode upstream:<value>`.
    pub upstream_proxy: Option<String>,

    /// Maximum time `stop()` will wait for the subprocess to terminate
    /// after the stop signal is sent.
    pub timeout: Duration,

    /// Executable to spawn. Defaults to `mitmdump` resolved via `$PATH`
    /// inside [`CaptureSession::start`]; tests override this with a
    /// stub. Programs supplied here are used **verbatim** — no
    /// `mitmdump`-specific flag inference.
    pub program: Option<PathBuf>,

    /// Extra command-line arguments to append after the mitmdump-flag
    /// list. Tests use this to drive their stub; production callers
    /// rarely need it but it's a useful escape hatch for, e.g.,
    /// `--anticache` or `--no-http2`.
    pub extra_args: Vec<String>,
}

impl CaptureConfig {
    /// Smallest-useful constructor: write HAR to `path`, otherwise take
    /// every default.
    pub fn for_har(path: impl Into<PathBuf>) -> Self {
        Self {
            output_har_path: path.into(),
            listen_port: None,
            upstream_proxy: None,
            timeout: DEFAULT_STOP_TIMEOUT,
            program: None,
            extra_args: Vec::new(),
        }
    }
}

/// Live capture-session handle. Drop-safe: dropping without calling
/// [`Self::stop`] will SIGKILL the child via `kill_on_drop`.
#[derive(Debug)]
pub struct CaptureSession {
    /// The actual port the proxy is listening on (resolved at start
    /// time even when the caller asked for an ephemeral port).
    listen_port: u16,
    /// Where the HAR will land.
    output_har_path: PathBuf,
    /// How long stop() will wait.
    stop_timeout: Duration,
    /// Tokio child handle. `Option<>` so [`Self::stop`] can take it.
    child: Option<Child>,
}

impl CaptureSession {
    /// Spawn the proxy according to `config`. Returns once the
    /// subprocess has been spawned (mitmproxy itself does not emit a
    /// "ready" signal we can wait on; callers that need to confirm the
    /// listen socket is up should poll with a short sleep — that's a
    /// concern for `barista-bench`, not this crate).
    pub async fn start(config: CaptureConfig) -> Result<Self, NetcapError> {
        // Materialise the output directory before spawning so mitmdump
        // doesn't fail mid-startup with a "no such directory" error.
        if let Some(parent) = config.output_har_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let listen_port = match config.listen_port {
            Some(p) => p,
            None => allocate_free_port()?,
        };

        let program = resolve_program(config.program.as_deref())?;

        let mut cmd = Command::new(&program);
        cmd.kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if is_mitmdump(&program) {
            // mitmdump-specific flag set. Kept inside the
            // `is_mitmdump` branch so the test-stub path can spawn an
            // arbitrary executable (e.g. `/bin/sh -c '...'`) without
            // these flags polluting its argv.
            cmd.arg("--listen-port")
                .arg(listen_port.to_string())
                .arg("--set")
                .arg(format!("hardump={}", config.output_har_path.display()))
                // mitmproxy is the MITM here *by design*: the captured
                // traffic IS the test subject. We accept upstream TLS
                // certs as-is and rely on the operator having vetted
                // the network.
                .arg("--ssl-insecure")
                // Silence the interactive "I'm about to MITM your
                // TLS!" banner; CI captures are non-interactive.
                .arg("--set")
                .arg("termlog_verbosity=warn");

            if let Some(upstream) = &config.upstream_proxy {
                cmd.arg("--mode").arg(format!("upstream:{upstream}"));
            }
        }

        for arg in &config.extra_args {
            cmd.arg(arg);
        }

        let child = cmd.spawn().map_err(|source| NetcapError::SpawnFailed {
            program: program.display().to_string(),
            source,
        })?;

        Ok(Self {
            listen_port,
            output_har_path: config.output_har_path,
            stop_timeout: config.timeout,
            child: Some(child),
        })
    }

    /// Actual listen port (resolved if the caller asked for ephemeral).
    pub fn listen_port(&self) -> u16 {
        self.listen_port
    }

    /// Path the HAR will be written to.
    pub fn output_har_path(&self) -> &PathBuf {
        &self.output_har_path
    }

    /// Send the stop signal, wait for the subprocess to flush, validate
    /// the HAR, and return a [`CaptureSummary`].
    ///
    /// On Unix this sends SIGTERM via `Child::kill`, which Tokio
    /// implements as `SIGKILL` on Unix. **mitmproxy 12.2.3 does NOT
    /// flush its HAR add-on on SIGKILL** (verified empirically: the
    /// `--set hardump=...` output file is absent after a SIGKILL
    /// shutdown, present after a SIGTERM shutdown). An earlier
    /// generation of the netcap crate assumed the add-on registered
    /// an `atexit`-style hook that survived SIGKILL; that assumption
    /// no longer holds. We send SIGTERM via the system `kill(1)`
    /// binary instead — keeps the crate free of a `nix` / `libc`
    /// dependency — and fall back to `tokio::process::Child::kill`
    /// (SIGKILL) on stop_timeout so we never leak a stuck mitmdump.
    pub async fn stop(mut self) -> Result<CaptureSummary, NetcapError> {
        // `take()` so a panic after this point doesn't double-kill via
        // the Drop guard.
        #[allow(clippy::expect_used)]
        let mut child = self
            .child
            .take()
            .expect("CaptureSession::stop called twice — would have moved by ownership");

        // Send SIGTERM via the system `kill` binary so mitmproxy's
        // HAR add-on gets a chance to flush. If the child has no PID
        // (already exited), there's nothing to signal — `wait()`
        // below will surface its status.
        if let Some(pid) = child.id() {
            let _ = std::process::Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }

        let status = match time::timeout(self.stop_timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(io)) => return Err(NetcapError::Io(io)),
            Err(_) => {
                // SIGTERM didn't take within the budget. Escalate to
                // SIGKILL via `tokio::process::Child::kill`; the
                // resulting HAR may be missing/truncated but we
                // refuse to block forever.
                let _ = child.kill().await;
                return Err(NetcapError::StopTimeout);
            }
        };

        let summary = har::validate(&self.output_har_path)?;

        Ok(CaptureSummary {
            har: summary,
            listen_port: self.listen_port,
            exit_status: render_status(&status),
        })
    }
}

/// Final report from a completed capture session.
#[derive(Debug, Clone)]
pub struct CaptureSummary {
    /// Outcome of [`crate::har::validate`] on the emitted HAR.
    pub har: HarSummary,
    /// Port the proxy was bound to.
    pub listen_port: u16,
    /// Human-rendered child exit status, for logging.
    pub exit_status: String,
}

/// Bind ephemeral port 0 on `127.0.0.1`, read back the kernel-assigned
/// port, and release the socket. There is an inherent TOCTOU window
/// between releasing the socket and mitmdump claiming the same port —
/// but on a single-tenant capture host the window is unobservable in
/// practice, and the alternative (pinning a hard-coded port) is much
/// worse for concurrent runs.
fn allocate_free_port() -> Result<u16, NetcapError> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .map_err(|source| NetcapError::PortAllocation { source })?;
    let port = listener
        .local_addr()
        .map_err(|source| NetcapError::PortAllocation { source })?
        .port();
    drop(listener);
    Ok(port)
}

fn resolve_program(explicit: Option<&std::path::Path>) -> Result<PathBuf, NetcapError> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    locate_mitmdump().ok_or(NetcapError::MitmproxyMissing)
}

/// True if the resolved program looks like a real mitmdump invocation
/// (i.e. the filename — minus `.exe` — is `mitmdump`). The test stub
/// path is deliberately *not* matched here so the mitmdump flag set is
/// elided when running against `/bin/sh`.
fn is_mitmdump(program: &std::path::Path) -> bool {
    program
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("mitmdump"))
        .unwrap_or(false)
}

fn render_status(status: &std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        format!("exit code: {code}")
    } else {
        "signalled".to_string()
    }
}
