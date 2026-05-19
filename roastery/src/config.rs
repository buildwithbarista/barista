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
//! | Variable                       | Default            | Notes                                                |
//! |--------------------------------|--------------------|------------------------------------------------------|
//! | `ROASTERY_BIND`                | `127.0.0.1:7878`   | `host:port` for the TCP listener.                    |
//! | `ROASTERY_STORAGE_DIR`         | `./.roastery-data` | Filesystem CAS root; created if missing.             |
//! | `ROASTERY_STORAGE_BACKEND`     | `fs`               | `fs` (default), `s3`, or `gcs`.                      |
//! | `ROASTERY_STORAGE_BUCKET`      | unset              | Required for `s3` / `gcs` backends.                  |
//! | `ROASTERY_STORAGE_REGION`      | unset              | Required for `s3`.                                   |
//! | `ROASTERY_STORAGE_PROJECT`     | unset              | Required for `gcs`.                                  |
//! | `ROASTERY_TLS_CERT`            | unset              | PEM server cert chain (server-side TLS).             |
//! | `ROASTERY_TLS_KEY`             | unset              | PEM private key. Required with `ROASTERY_TLS_CERT`.  |
//! | `ROASTERY_BEARER_TOKENS_FILE`  | unset              | Path to a `<label>:<secret>` tokens file.            |
//! | `ROASTERY_MTLS_CA_CERT`        | unset              | PEM CA bundle the client cert must chain to.         |
//! | `ROASTERY_UPSTREAM`            | unset              | Reserved for upstream-on-miss.                       |
//!
//! ## Fail-closed default
//!
//! A roastery bound to a non-loopback address (`bind` other than
//! `127.0.0.1` / `::1` / `localhost`) with **neither** bearer **nor**
//! mTLS configured refuses to start — see
//! [`ServerConfig::validate`]. Loopback binds without auth are
//! explicitly allowed so the `cargo run -p roastery` developer
//! workflow stays one-command. Production deployments must configure
//! at least one of [`AuthConfig::bearer`] / [`AuthConfig::mtls`].
//!
//! ## mTLS prerequisite
//!
//! `ROASTERY_MTLS_CA_CERT` requires `ROASTERY_TLS_CERT` +
//! `ROASTERY_TLS_KEY` to also be set: there is no way to validate a
//! client certificate without first terminating a TLS handshake.
//! The validator surfaces this as `RoasteryError::Config` at startup.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use url::Url;

use crate::error::{Result, RoasteryError};

/// Default bind address when `ROASTERY_BIND` is unset.
pub const DEFAULT_BIND: &str = "127.0.0.1:7878";

/// Default storage directory when `ROASTERY_STORAGE_DIR` is unset.
pub const DEFAULT_STORAGE_DIR: &str = "./.roastery-data";

/// Which content-addressed storage backend the server is configured
/// against. The filesystem variant carries its own root so the
/// backend can be initialised from a single `StorageBackend` value
/// without re-reading `ServerConfig::storage_dir`.
///
/// Stub variants `S3` and `Gcs` parse cleanly today (so config files
/// remain forward-compatible) but their [`crate::storage::Cas`]
/// methods return [`crate::error::StorageError::NotImplemented`]
/// until v0.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageBackend {
    /// Filesystem CAS rooted at the given path. Default.
    Filesystem(PathBuf),
    /// Stub Amazon S3 backend.
    S3 {
        /// Bucket name.
        bucket: String,
        /// AWS region (e.g. `us-east-1`).
        region: String,
    },
    /// Stub Google Cloud Storage backend.
    Gcs {
        /// Bucket name.
        bucket: String,
        /// GCP project ID.
        project: String,
    },
}

/// Server-side TLS configuration.
///
/// When `Some` on [`ServerConfig::tls`] the listener terminates TLS
/// using `rustls`. Required when [`AuthConfig::mtls`] is configured:
/// client-cert verification can only happen during a TLS handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsConfig {
    /// PEM-encoded server certificate chain.
    pub cert_path: PathBuf,
    /// PEM-encoded private key matching `cert_path`.
    pub key_path: PathBuf,
}

/// Bearer-token auth configuration.
///
/// Operator publishes a tokens file at `tokens_file`; the server
/// loads + hashes it once at startup. See [`crate::auth::bearer`]
/// for the file format and the `[T]` proof that plaintext never
/// survives the loader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BearerAuthConfig {
    /// Path to the tokens file.
    pub tokens_file: PathBuf,
}

/// mTLS auth configuration.
///
/// Operator publishes a CA bundle (one or more PEM-encoded
/// certificates concatenated). Every client that connects MUST
/// present a certificate chained to one of those trust anchors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtlsAuthConfig {
    /// Path to the PEM CA bundle.
    pub ca_cert: PathBuf,
}

/// Top-level authentication configuration.
///
/// Either, both, or neither of `bearer` / `mtls` may be set. When
/// both are set, **either** mechanism suffices on a per-request
/// basis (see [`crate::auth::layer`] for the decision order).
///
/// An `AuthConfig` with both fields `None` is the "no auth
/// configured" state — only valid on a loopback bind, otherwise
/// `ServerConfig::validate` rejects it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthConfig {
    /// Bearer-token configuration. `None` to disable.
    pub bearer: Option<BearerAuthConfig>,
    /// mTLS configuration. `None` to disable.
    pub mtls: Option<MtlsAuthConfig>,
}

impl AuthConfig {
    /// True iff at least one auth mechanism is configured.
    pub fn any_configured(&self) -> bool {
        self.bearer.is_some() || self.mtls.is_some()
    }
}

/// Server configuration.
///
/// Construct with [`ServerConfig::from_env`] in production, or with
/// [`ServerConfig::with_bind`] when wiring an integration test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Socket address the listener binds to.
    pub bind: SocketAddr,
    /// On-disk root for cached artifacts.
    ///
    /// Kept as a dedicated field (in addition to the richer
    /// [`StorageBackend::Filesystem`] variant carried by `storage`)
    /// so callers that always want a local working directory — log
    /// files, scratch space, future SQLite indexes — have a stable
    /// path regardless of which CAS backend is configured.
    pub storage_dir: PathBuf,
    /// Which CAS backend the server uses. Defaults to
    /// [`StorageBackend::Filesystem`] anchored at `storage_dir`.
    pub storage: StorageBackend,
    /// Server-side TLS material. The listener terminates TLS with
    /// `rustls` when this is `Some`.
    pub tls: Option<TlsConfig>,
    /// Authentication configuration — bearer + mTLS. Either, both, or
    /// neither may be set; non-loopback binds require at least one
    /// (enforced by [`ServerConfig::validate`]).
    pub auth: AuthConfig,
    /// Upstream registry to consult on cache miss. Wired by a
    /// subsequent task.
    pub upstream: Option<Url>,
}

impl ServerConfig {
    /// Build a configuration with all fields at their defaults except
    /// `bind`, which is overridden to `addr`. Intended for tests that
    /// want to bind to an ephemeral port (`127.0.0.1:0`).
    pub fn with_bind(addr: SocketAddr) -> Self {
        let storage_dir = PathBuf::from(DEFAULT_STORAGE_DIR);
        Self {
            bind: addr,
            storage: StorageBackend::Filesystem(storage_dir.clone()),
            storage_dir,
            tls: None,
            auth: AuthConfig::default(),
            upstream: None,
        }
    }

    /// Load configuration from process environment variables, falling
    /// back to documented defaults.
    pub fn from_env() -> Result<Self> {
        let bind = parse_bind(env::var_os("ROASTERY_BIND"))?;
        let storage_dir = parse_storage_dir(env::var_os("ROASTERY_STORAGE_DIR"))?;
        let storage = parse_storage_backend(
            env::var_os("ROASTERY_STORAGE_BACKEND"),
            env::var_os("ROASTERY_STORAGE_BUCKET"),
            env::var_os("ROASTERY_STORAGE_REGION"),
            env::var_os("ROASTERY_STORAGE_PROJECT"),
            &storage_dir,
        )?;
        let tls = parse_tls(
            env::var_os("ROASTERY_TLS_CERT"),
            env::var_os("ROASTERY_TLS_KEY"),
        )?;
        let auth = parse_auth(
            env::var_os("ROASTERY_BEARER_TOKENS_FILE"),
            env::var_os("ROASTERY_MTLS_CA_CERT"),
        );
        let upstream = parse_upstream(env::var_os("ROASTERY_UPSTREAM"))?;

        Ok(Self {
            bind,
            storage_dir,
            storage,
            tls,
            auth,
            upstream,
        })
    }

    /// Ensure preconditions for a clean startup hold:
    ///
    /// - `storage_dir` is creatable.
    /// - Any configured TLS / bearer-tokens / mTLS-CA files exist.
    /// - mTLS is not configured without server-side TLS (the client
    ///   cert can only be inspected during a TLS handshake).
    /// - A non-loopback bind has at least one auth mechanism
    ///   configured. Loopback binds (`127.0.0.1`, `::1`, `localhost`)
    ///   without auth are explicitly allowed for the dev loop.
    ///
    /// Called from `server::run` on startup so a misconfigured
    /// server fails fast with a typed error rather than booting and
    /// then accepting unauthenticated traffic.
    pub fn validate(&self) -> Result<()> {
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

        if let Some(b) = &self.auth.bearer {
            if !b.tokens_file.exists() {
                return Err(RoasteryError::config_path(
                    "bearer tokens file does not exist",
                    &b.tokens_file,
                ));
            }
        }

        if let Some(m) = &self.auth.mtls {
            if !m.ca_cert.exists() {
                return Err(RoasteryError::config_path(
                    "mTLS CA cert file does not exist",
                    &m.ca_cert,
                ));
            }
            // mTLS requires server-side TLS. The TLS layer is the
            // only place where a client cert can be inspected; with
            // no TLS termination here, there's no handshake to
            // collect a cert from.
            if self.tls.is_none() {
                return Err(RoasteryError::Config(
                    "mTLS is configured (ROASTERY_MTLS_CA_CERT) but server-side TLS \
                     (ROASTERY_TLS_CERT + ROASTERY_TLS_KEY) is not — mTLS cannot work \
                     without a TLS handshake"
                        .to_string(),
                ));
            }
        }

        // Fail-closed default: a non-loopback bind with no auth
        // configured refuses to start.  Error code BAR-AUTH-005.
        if !is_loopback(&self.bind) && !self.auth.any_configured() {
            return Err(RoasteryError::Config(format!(
                "BAR-AUTH-005: non-loopback bind {} requires auth configuration \
                 (set ROASTERY_BEARER_TOKENS_FILE and/or ROASTERY_MTLS_CA_CERT)",
                self.bind
            )));
        }

        Ok(())
    }
}

/// Is `addr` a loopback address?
///
/// We treat both IPv4 `127.0.0.0/8` and IPv6 `::1` as loopback (the
/// standard libstd notion). Hostnames don't reach here — the env
/// var loader parses the bind into a `SocketAddr` before
/// `validate` ever sees it; an operator who writes `localhost:7878`
/// in `ROASTERY_BIND` has it resolved by the surrounding shell or
/// the `SocketAddr::from_str` parser to one of the loopback IPs.
fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
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

fn parse_storage_backend(
    backend: Option<OsString>,
    bucket: Option<OsString>,
    region: Option<OsString>,
    project: Option<OsString>,
    storage_dir: &Path,
) -> Result<StorageBackend> {
    let kind = backend
        .as_ref()
        .and_then(|v| v.to_str())
        .unwrap_or("fs")
        .to_ascii_lowercase();

    match kind.as_str() {
        "" | "fs" | "filesystem" => Ok(StorageBackend::Filesystem(storage_dir.to_path_buf())),
        "s3" => {
            let bucket = bucket
                .as_ref()
                .and_then(|v| v.to_str())
                .ok_or_else(|| {
                    RoasteryError::Config(
                        "ROASTERY_STORAGE_BACKEND=s3 requires ROASTERY_STORAGE_BUCKET"
                            .to_string(),
                    )
                })?
                .to_string();
            let region = region
                .as_ref()
                .and_then(|v| v.to_str())
                .ok_or_else(|| {
                    RoasteryError::Config(
                        "ROASTERY_STORAGE_BACKEND=s3 requires ROASTERY_STORAGE_REGION"
                            .to_string(),
                    )
                })?
                .to_string();
            Ok(StorageBackend::S3 { bucket, region })
        }
        "gcs" => {
            let bucket = bucket
                .as_ref()
                .and_then(|v| v.to_str())
                .ok_or_else(|| {
                    RoasteryError::Config(
                        "ROASTERY_STORAGE_BACKEND=gcs requires ROASTERY_STORAGE_BUCKET"
                            .to_string(),
                    )
                })?
                .to_string();
            let project = project
                .as_ref()
                .and_then(|v| v.to_str())
                .ok_or_else(|| {
                    RoasteryError::Config(
                        "ROASTERY_STORAGE_BACKEND=gcs requires ROASTERY_STORAGE_PROJECT"
                            .to_string(),
                    )
                })?
                .to_string();
            Ok(StorageBackend::Gcs { bucket, project })
        }
        other => Err(RoasteryError::Config(format!(
            "unknown ROASTERY_STORAGE_BACKEND {other:?} (expected fs, s3, or gcs)"
        ))),
    }
}

/// Parse the bearer + mTLS env vars into an [`AuthConfig`].
///
/// Each value is the path the operator supplied; existence is
/// checked in [`ServerConfig::validate`] so a missing-file error is
/// presented uniformly with the other startup-time checks.
fn parse_auth(bearer_tokens: Option<OsString>, mtls_ca: Option<OsString>) -> AuthConfig {
    let bearer = bearer_tokens.map(|p| BearerAuthConfig {
        tokens_file: PathBuf::from(p),
    });
    let mtls = mtls_ca.map(|p| MtlsAuthConfig {
        ca_cert: PathBuf::from(p),
    });
    AuthConfig { bearer, mtls }
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
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn with_bind_overrides_only_bind() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let cfg = ServerConfig::with_bind(addr);
        assert_eq!(cfg.bind, addr);
        assert_eq!(cfg.storage_dir, PathBuf::from(DEFAULT_STORAGE_DIR));
        assert_eq!(
            cfg.storage,
            StorageBackend::Filesystem(PathBuf::from(DEFAULT_STORAGE_DIR))
        );
        assert!(cfg.tls.is_none());
        assert!(cfg.upstream.is_none());
        assert!(!cfg.auth.any_configured());
    }

    #[test]
    fn parse_storage_backend_defaults_to_filesystem() {
        let dir = PathBuf::from("/var/lib/roastery");
        let backend = parse_storage_backend(None, None, None, None, &dir).unwrap();
        assert_eq!(backend, StorageBackend::Filesystem(dir));
    }

    #[test]
    fn parse_storage_backend_selects_s3_when_requested() {
        let dir = PathBuf::from("/tmp/unused");
        let backend = parse_storage_backend(
            Some("s3".into()),
            Some("artifacts".into()),
            Some("us-west-2".into()),
            None,
            &dir,
        )
        .unwrap();
        assert_eq!(
            backend,
            StorageBackend::S3 {
                bucket: "artifacts".to_string(),
                region: "us-west-2".to_string(),
            }
        );
    }

    #[test]
    fn parse_storage_backend_s3_requires_bucket_and_region() {
        let dir = PathBuf::from("/tmp/unused");
        let err = parse_storage_backend(Some("s3".into()), None, None, None, &dir).unwrap_err();
        assert!(matches!(err, RoasteryError::Config(_)));
        let err = parse_storage_backend(
            Some("s3".into()),
            Some("b".into()),
            None,
            None,
            &dir,
        )
        .unwrap_err();
        assert!(matches!(err, RoasteryError::Config(_)));
    }

    #[test]
    fn parse_storage_backend_selects_gcs_when_requested() {
        let dir = PathBuf::from("/tmp/unused");
        let backend = parse_storage_backend(
            Some("gcs".into()),
            Some("artifacts".into()),
            None,
            Some("barista-build".into()),
            &dir,
        )
        .unwrap();
        assert_eq!(
            backend,
            StorageBackend::Gcs {
                bucket: "artifacts".to_string(),
                project: "barista-build".to_string(),
            }
        );
    }

    #[test]
    fn parse_storage_backend_rejects_unknown_kind() {
        let dir = PathBuf::from("/tmp/unused");
        let err = parse_storage_backend(Some("azure".into()), None, None, None, &dir)
            .unwrap_err();
        assert!(matches!(err, RoasteryError::Config(_)));
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
            storage: StorageBackend::Filesystem(tmp.clone()),
            storage_dir: tmp.clone(),
            tls: None,
            auth: AuthConfig::default(),
            upstream: None,
        };
        cfg.validate().unwrap();
        assert!(tmp.exists());

        // Best-effort cleanup; ignore failures.
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_auth_constructs_optional_substructs() {
        let auth = parse_auth(None, None);
        assert!(!auth.any_configured());

        let auth = parse_auth(Some("/etc/roastery/tokens".into()), None);
        assert!(auth.bearer.is_some());
        assert!(auth.mtls.is_none());

        let auth = parse_auth(None, Some("/etc/roastery/ca.pem".into()));
        assert!(auth.bearer.is_none());
        assert!(auth.mtls.is_some());

        let auth = parse_auth(
            Some("/etc/roastery/tokens".into()),
            Some("/etc/roastery/ca.pem".into()),
        );
        assert!(auth.any_configured());
        assert_eq!(
            auth.bearer.as_ref().unwrap().tokens_file,
            PathBuf::from("/etc/roastery/tokens")
        );
        assert_eq!(
            auth.mtls.as_ref().unwrap().ca_cert,
            PathBuf::from("/etc/roastery/ca.pem")
        );
    }

    fn fresh_storage_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "roastery-validate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    /// `[T]` linkage: `server_refuses_to_start_with_non_loopback_bind_and_no_auth`.
    #[test]
    fn validate_rejects_non_loopback_bind_without_auth() {
        let storage = fresh_storage_dir();
        let cfg = ServerConfig {
            bind: "0.0.0.0:8443".parse().unwrap(),
            storage: StorageBackend::Filesystem(storage.clone()),
            storage_dir: storage.clone(),
            tls: None,
            auth: AuthConfig::default(),
            upstream: None,
        };
        let err = cfg.validate().unwrap_err();
        let RoasteryError::Config(msg) = &err else {
            panic!("expected Config error, got {err:?}");
        };
        assert!(
            msg.contains("BAR-AUTH-005"),
            "expected BAR-AUTH-005 in error message, got: {msg}"
        );
        let _ = fs::remove_dir_all(&storage);
    }

    #[test]
    fn validate_allows_loopback_bind_without_auth() {
        let storage = fresh_storage_dir();
        let cfg = ServerConfig {
            bind: "127.0.0.1:8443".parse().unwrap(),
            storage: StorageBackend::Filesystem(storage.clone()),
            storage_dir: storage.clone(),
            tls: None,
            auth: AuthConfig::default(),
            upstream: None,
        };
        cfg.validate().unwrap();
        let _ = fs::remove_dir_all(&storage);
    }

    #[test]
    fn validate_allows_non_loopback_bind_with_bearer_auth() {
        let storage = fresh_storage_dir();
        let mut tokens = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write;
        writeln!(tokens, "ci:secret").unwrap();
        let cfg = ServerConfig {
            bind: "0.0.0.0:8443".parse().unwrap(),
            storage: StorageBackend::Filesystem(storage.clone()),
            storage_dir: storage.clone(),
            tls: None,
            auth: AuthConfig {
                bearer: Some(BearerAuthConfig {
                    tokens_file: tokens.path().to_path_buf(),
                }),
                mtls: None,
            },
            upstream: None,
        };
        cfg.validate().unwrap();
        let _ = fs::remove_dir_all(&storage);
    }

    #[test]
    fn validate_rejects_mtls_without_server_tls() {
        let storage = fresh_storage_dir();
        let ca = tempfile::NamedTempFile::new().unwrap();
        let cfg = ServerConfig {
            bind: "127.0.0.1:8443".parse().unwrap(),
            storage: StorageBackend::Filesystem(storage.clone()),
            storage_dir: storage.clone(),
            tls: None,
            auth: AuthConfig {
                bearer: None,
                mtls: Some(MtlsAuthConfig {
                    ca_cert: ca.path().to_path_buf(),
                }),
            },
            upstream: None,
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, RoasteryError::Config(_)));
        let _ = fs::remove_dir_all(&storage);
    }
}
