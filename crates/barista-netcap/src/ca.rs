//! CA-certificate setup helpers for the mitmproxy-based capture harness.
//!
//! ## What this module does
//!
//! `barista-netcap` decrypts the HTTPS traffic between a JVM build tool
//! (`mvn`, `mvnd`, `barista`) and upstream Maven repositories by routing
//! every request through a local mitmproxy instance. mitmproxy presents
//! the JVM with a TLS certificate signed by mitmproxy's own root CA, and
//! for the JVM to accept that certificate the CA's public cert
//! (`mitmproxy-ca-cert.pem`) has to be installed into the JDK truststore.
//!
//! ## What this module does NOT do
//!
//! It does **not** auto-import the CA into the truststore. On macOS that
//! import targets a path inside `$JAVA_HOME` and typically requires
//! `sudo`; on Linux the right path depends on the distro and the JDK
//! vendor; on Windows it's a `keytool.exe` invocation against a
//! Windows-specific `cacerts` location. Doing this silently from a Rust
//! library is a footgun (the change persists beyond the test session and
//! is invisible at the next `JAVA_HOME` rotation), and the security
//! posture for adding a new root of trust deserves an explicit consent
//! step. So instead we return a [`CaStatus`] that *describes* what's
//! missing and *suggests* the exact commands the user should run.
//!
//! ## How callers use it
//!
//! ```no_run
//! use barista_netcap::ca::CaSetup;
//!
//! match CaSetup::ensure_installed() {
//!     Ok(status) => println!("{status:#?}"),
//!     Err(e) => eprintln!("CA check failed: {e}"),
//! }
//! ```
//!
//! The `Ok` arm is the common path even on hosts where the cert isn't
//! installed yet — that situation is communicated via [`CaStatus`], not
//! via [`Err`].

use std::env;
use std::path::{Path, PathBuf};

use crate::error::NetcapError;

/// The default install location for mitmproxy's CA bundle on every
/// platform mitmproxy supports — `~/.mitmproxy/`. mitmproxy writes this
/// directory the first time it's run; both `mitmdump` and `mitmproxy`
/// share it.
const MITMPROXY_DIR: &str = ".mitmproxy";

/// The PEM-encoded CA certificate file inside [`MITMPROXY_DIR`].
const CA_CERT_PEM: &str = "mitmproxy-ca-cert.pem";

/// Static metadata about the on-disk mitmproxy CA bundle.
///
/// Construct via [`CaSetup::ensure_installed`], not directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaCertificate {
    /// Absolute path to `mitmproxy-ca-cert.pem`.
    pub path: PathBuf,
}

/// Result of checking whether the host is ready to trust mitmproxy-signed
/// TLS certificates. **None of these variants are errors** — they're the
/// three legitimate states the host can be in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaStatus {
    /// `mitmproxy-ca-cert.pem` exists at the expected location. The
    /// caller still needs to (a) verify the cert is in the JDK
    /// truststore and (b) trust that the user has done so — there is
    /// no portable way to query the truststore for our specific alias
    /// without invoking `keytool` and parsing its output, which is more
    /// fragile than asking the user to confirm.
    Installed {
        /// On-disk location of the CA PEM.
        cert: CaCertificate,
        /// Shell commands the user is expected to have run (or should
        /// run) to import the CA into the active JDK truststore. We
        /// emit these so a "what next?" log line can render them.
        suggested_truststore_commands: Vec<String>,
    },
    /// mitmproxy is installed (its binary is on `$PATH`) but the CA
    /// bundle hasn't been generated yet — the user has never run
    /// mitmproxy. Suggested fix: run `mitmdump --help` once to trigger
    /// CA generation.
    NotInstalled {
        /// The path we looked at.
        expected_path: PathBuf,
        /// Commands the user should run to (a) generate the CA and (b)
        /// import it into the active JDK truststore.
        suggested_commands: Vec<String>,
    },
    /// mitmproxy itself isn't installed. Without it there's no CA to
    /// import; the user must install mitmproxy first.
    MissingMitmproxy {
        /// Commands the user should run to install mitmproxy.
        install_hints: Vec<String>,
    },
}

/// Entry point for CA-status inspection.
#[derive(Debug, Default, Clone, Copy)]
pub struct CaSetup;

impl CaSetup {
    /// Reports the current state of the mitmproxy CA on this host.
    ///
    /// **Deterministic on every host:** this function only reads
    /// environment variables and the filesystem; it never spawns
    /// processes and never mutates state. It is safe to call from any
    /// context, including the workspace's test harness on a host that
    /// has never seen mitmproxy.
    ///
    /// The function returns `Err` only when the OS-level environment is
    /// pathological — specifically, when `$HOME` is unset on Unix. Every
    /// other "thing is missing" condition surfaces as a [`CaStatus`]
    /// variant.
    pub fn ensure_installed() -> Result<CaStatus, NetcapError> {
        Self::ensure_installed_with_home(home_dir()?.as_path())
    }

    /// Test seam — same logic as [`Self::ensure_installed`] but takes the
    /// home directory as an argument so the integration tests can sandbox
    /// `$HOME` to a `tempfile::TempDir` without setting global env vars
    /// (which would race other tests in the same process).
    pub fn ensure_installed_with_home(home: &Path) -> Result<CaStatus, NetcapError> {
        let cert_path = home.join(MITMPROXY_DIR).join(CA_CERT_PEM);
        let mitmproxy_present = locate_mitmdump().is_some();

        if !mitmproxy_present {
            return Ok(CaStatus::MissingMitmproxy {
                install_hints: install_hints(),
            });
        }

        if !cert_path.exists() {
            return Ok(CaStatus::NotInstalled {
                expected_path: cert_path.clone(),
                suggested_commands: bootstrap_commands(&cert_path),
            });
        }

        Ok(CaStatus::Installed {
            cert: CaCertificate {
                path: cert_path.clone(),
            },
            suggested_truststore_commands: truststore_import_commands(&cert_path),
        })
    }
}

/// Locate `mitmdump` on `$PATH`. We prefer `mitmdump` over `mitmproxy`
/// because the capture-session driver uses the headless variant; if both
/// are present they share the same CA so the choice is moot for this
/// check.
pub(crate) fn locate_mitmdump() -> Option<PathBuf> {
    which::which("mitmdump").ok()
}

fn home_dir() -> Result<PathBuf, NetcapError> {
    // Prefer `HOME` on Unix; fall back to `USERPROFILE` on Windows. This
    // matches the resolution logic in `barista-config::sources` and avoids
    // pulling in a `dirs`-family crate just for one read.
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home));
    }
    if let Some(profile) = env::var_os("USERPROFILE") {
        return Ok(PathBuf::from(profile));
    }
    Err(NetcapError::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "neither $HOME nor %USERPROFILE% is set; cannot locate mitmproxy CA",
    )))
}

fn install_hints() -> Vec<String> {
    vec![
        "brew install mitmproxy   # macOS / Linuxbrew".to_string(),
        "pipx install mitmproxy   # cross-platform via Python".to_string(),
        "see https://mitmproxy.org/#install for other options".to_string(),
    ]
}

fn bootstrap_commands(cert_path: &Path) -> Vec<String> {
    let mut cmds = vec![
        // Running `mitmdump` for a moment generates the CA on first use.
        "mitmdump --listen-port 0 &  # generates the CA bundle".to_string(),
        "sleep 1 && kill %1          # stop the bootstrap proxy".to_string(),
    ];
    cmds.extend(truststore_import_commands(cert_path));
    cmds
}

fn truststore_import_commands(cert_path: &Path) -> Vec<String> {
    let cert_display = cert_path.display();
    vec![
        // Add to the *active* JDK's truststore. The user is expected to
        // know which JDK their build tool resolves to; we don't try to
        // guess. `barista-netbarista` validation should warn if the alias
        // is missing for the active `$JAVA_HOME`.
        format!(
            "sudo keytool -importcert -trustcacerts \\\n  \
             -keystore \"$JAVA_HOME/lib/security/cacerts\" \\\n  \
             -storepass changeit \\\n  \
             -alias mitmproxy-barista \\\n  \
             -file {cert_display}"
        ),
        format!(
            "# Verify:\nkeytool -list -keystore \"$JAVA_HOME/lib/security/cacerts\" \\\n  \
             -storepass changeit -alias mitmproxy-barista"
        ),
    ]
}
