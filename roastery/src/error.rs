//! Typed errors for the roastery server.
//!
//! Subsequent milestones will extend this enum with variants for
//! storage I/O (T2), protocol parsing (T3/T4), and upstream-fetch
//! failures (T6). The scaffold ships only the variants the bootstrap
//! path can hit today.

use std::io;
use std::net::SocketAddr;
use std::path::Path;

use thiserror::Error;

/// All fatal errors the roastery server can surface from `run`.
#[derive(Debug, Error)]
pub enum RoasteryError {
    /// Server configuration is invalid (bad address, unreadable
    /// storage directory, malformed upstream URL, …).
    #[error("invalid server configuration: {0}")]
    Config(String),

    /// Could not bind the TCP listener to the configured address.
    #[error("failed to bind listener on {addr}: {source}")]
    Bind {
        /// The address the server attempted to bind.
        addr: SocketAddr,
        /// The underlying OS error from `tokio::net::TcpListener`.
        #[source]
        source: io::Error,
    },

    /// Generic I/O failure (storage-dir creation, listener accept
    /// loop, signal-handler installation).
    #[error("I/O error: {source}")]
    Io {
        #[from]
        source: io::Error,
    },
}

impl RoasteryError {
    /// Construct a [`RoasteryError::Config`] complaining about a
    /// path-shaped value. Used by `ServerConfig` validation.
    pub(crate) fn config_path(reason: &str, path: &Path) -> Self {
        RoasteryError::Config(format!("{reason}: {}", path.display()))
    }
}

/// Crate-wide `Result` alias.
pub type Result<T> = std::result::Result<T, RoasteryError>;
