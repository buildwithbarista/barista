//! Server configuration for the roastery binary.
//!
//! The scaffold ships a small set of fields covering the bootstrap
//! path — bind address, storage directory, and two placeholder
//! structs for TLS and upstream-on-miss that later milestones will
//! populate. Values are loaded from environment variables with
//! documented defaults so an operator can `cargo run -p roastery`
//! and get a working scaffold with zero configuration.
//!
//! Environment variables:
//!
//! | Variable                 | Default               | Notes                                |
//! |--------------------------|-----------------------|--------------------------------------|
//! | `ROASTERY_BIND`          | `127.0.0.1:7878`      | `host:port` for the TCP listener.    |
//! | `ROASTERY_STORAGE_DIR`   | `./.roastery-data`    | Created if missing (used by T2).     |
//! | `ROASTERY_TLS_CERT`      | unset                 | Reserved for T5; presence triggers   |
//! | `ROASTERY_TLS_KEY`       | unset                 | TLS once both files are present.     |
//! | `ROASTERY_UPSTREAM`      | unset                 | Reserved for T6 upstream-on-miss.    |

use std::env;
use std::ffi::OsString;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use url::Url;

use crate::error::{Result, RoasteryError};

/// Default bind address when `ROASTERY_BIND` is unset.
pub const DEFAULT_BIND: &str = "127.0.0.1:7878";

/// Default storage directory when `ROASTERY_STORAGE_DIR` is unset.
pub const DEFAULT_STORAGE_DIR: &str = "./.roastery-data";

/// Placeholder TLS configuration.
///
/// The scaffold validates that the two paths exist when present but
/// does not load them; the actual TLS terminator lands in M5.1 T5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsConfig {
    /// PEM-encoded server certificate chain.
    pub cert_path: PathBuf,
    /// PEM-encoded private key matching `cert_path`.
    pub key_path: PathBuf,
}

/// Server configuration.
///
/// Construct with [`ServerConfig::from_env`] in production, or with
/// [`ServerConfig::with_bind`] when wiring an integration test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Socket address the listener binds to.
    pub bind: SocketAddr,
    /// On-disk root for cached artifacts. Used by M5.1 T2.
    pub storage_dir: PathBuf,
    /// TLS material. M5.1 T5 will switch the connection builder to
    /// `rustls` when this is `Some`.
    pub tls: Option<TlsConfig>,
    /// Upstream registry to consult on cache miss. Wired by M5.1 T6.
    pub upstream: Option<Url>,
}

impl ServerConfig {
    /// Build a configuration with all fields at their defaults except
    /// `bind`, which is overridden to `addr`. Intended for tests that
    /// want to bind to an ephemeral port (`127.0.0.1:0`).
    pub fn with_bind(addr: SocketAddr) -> Self {
        Self {
            bind: addr,
            storage_dir: PathBuf::from(DEFAULT_STORAGE_DIR),
            tls: None,
            upstream: None,
        }
    }

    /// Load configuration from process environment variables, falling
    /// back to documented defaults.
    pub fn from_env() -> Result<Self> {
        let bind = parse_bind(env::var_os("ROASTERY_BIND"))?;
        let storage_dir = parse_storage_dir(env::var_os("ROASTERY_STORAGE_DIR"))?;
        let tls = parse_tls(
            env::var_os("ROASTERY_TLS_CERT"),
            env::var_os("ROASTERY_TLS_KEY"),
        )?;
        let upstream = parse_upstream(env::var_os("ROASTERY_UPSTREAM"))?;

        Ok(Self {
            bind,
            storage_dir,
            tls,
            upstream,
        })
    }

    /// Ensure on-disk preconditions hold: `storage_dir` is creatable,
    /// any configured TLS files exist. Called from `server::run` on
    /// startup.
    pub(crate) fn validate(&self) -> Result<()> {
        // Create the storage dir if missing; surface a Config error
        // (not raw I/O) for any failure so the caller can present a
        // friendlier message.
        fs::create_dir_all(&self.storage_dir).map_err(|e| {
            RoasteryError::Config(format!(
                "cannot create storage dir {}: {e}",
                self.storage_dir.display()
            ))
        })?;

        if let Some(tls) = &self.tls {
            if !tls.cert_path.exists() {
                return Err(RoasteryError::config_path(
                    "TLS cert file does not exist",
                    &tls.cert_path,
                ));
            }
            if !tls.key_path.exists() {
                return Err(RoasteryError::config_path(
                    "TLS key file does not exist",
                    &tls.key_path,
                ));
            }
        }

        Ok(())
    }
}

fn parse_bind(raw: Option<OsString>) -> Result<SocketAddr> {
    let s = raw
        .as_ref()
        .and_then(|v| v.to_str())
        .unwrap_or(DEFAULT_BIND);
    SocketAddr::from_str(s)
        .map_err(|e| RoasteryError::Config(format!("invalid ROASTERY_BIND {s:?}: {e}")))
}

fn parse_storage_dir(raw: Option<OsString>) -> Result<PathBuf> {
    Ok(raw
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STORAGE_DIR)))
}

fn parse_tls(cert: Option<OsString>, key: Option<OsString>) -> Result<Option<TlsConfig>> {
    match (cert, key) {
        (None, None) => Ok(None),
        (Some(c), Some(k)) => Ok(Some(TlsConfig {
            cert_path: PathBuf::from(c),
            key_path: PathBuf::from(k),
        })),
        (Some(_), None) | (None, Some(_)) => Err(RoasteryError::Config(
            "ROASTERY_TLS_CERT and ROASTERY_TLS_KEY must be set together".to_string(),
        )),
    }
}

fn parse_upstream(raw: Option<OsString>) -> Result<Option<Url>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let s = raw
        .to_str()
        .ok_or_else(|| RoasteryError::Config("ROASTERY_UPSTREAM is not valid UTF-8".to_string()))?;
    Url::parse(s)
        .map(Some)
        .map_err(|e| RoasteryError::Config(format!("invalid ROASTERY_UPSTREAM {s:?}: {e}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn with_bind_overrides_only_bind() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let cfg = ServerConfig::with_bind(addr);
        assert_eq!(cfg.bind, addr);
        assert_eq!(cfg.storage_dir, PathBuf::from(DEFAULT_STORAGE_DIR));
        assert!(cfg.tls.is_none());
        assert!(cfg.upstream.is_none());
    }

    #[test]
    fn parse_bind_defaults_when_unset() {
        let addr = parse_bind(None).unwrap();
        assert_eq!(addr.to_string(), DEFAULT_BIND);
    }

    #[test]
    fn parse_bind_rejects_garbage() {
        let err = parse_bind(Some(OsString::from("not-a-socket-addr"))).unwrap_err();
        assert!(matches!(err, RoasteryError::Config(_)));
    }

    #[test]
    fn parse_tls_requires_both_or_neither() {
        assert!(parse_tls(None, None).unwrap().is_none());
        assert!(parse_tls(Some("c".into()), None).is_err());
        assert!(parse_tls(None, Some("k".into())).is_err());
        let tls = parse_tls(Some("c.pem".into()), Some("k.pem".into()))
            .unwrap()
            .unwrap();
        assert_eq!(tls.cert_path, PathBuf::from("c.pem"));
        assert_eq!(tls.key_path, PathBuf::from("k.pem"));
    }

    #[test]
    fn parse_upstream_accepts_url() {
        let u = parse_upstream(Some("https://repo1.maven.org/maven2/".into()))
            .unwrap()
            .unwrap();
        assert_eq!(u.scheme(), "https");
    }

    #[test]
    fn parse_upstream_rejects_garbage() {
        let err = parse_upstream(Some("not a url".into())).unwrap_err();
        assert!(matches!(err, RoasteryError::Config(_)));
    }

    #[test]
    fn validate_creates_missing_storage_dir() {
        let tmp = std::env::temp_dir().join(format!(
            "roastery-test-{}-{}",
            std::process::id(),
            // Pseudo-random suffix so parallel tests don't collide.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        assert!(!tmp.exists());

        let cfg = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            storage_dir: tmp.clone(),
            tls: None,
            upstream: None,
        };
        cfg.validate().unwrap();
        assert!(tmp.exists());

        // Best-effort cleanup; ignore failures.
        let _ = fs::remove_dir_all(&tmp);
    }
}
