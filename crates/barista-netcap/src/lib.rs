// Tests legitimately use `expect`/`unwrap`/`panic!` to keep failure
// messages compact; the workspace lint policy elevates these to warn,
// which `-D warnings` turns into a hard error. Scope the exemption to
// test code so production code in `src/` still has to justify each
// panic-path with a `#[allow(...)]` adjacent to a SAFETY-style comment.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! Network-traffic capture harness for Barista's resource-efficiency
//! program.
//!
//! `barista-netcap` drives [mitmproxy] as a child process, decrypts the
//! HTTP/HTTPS traffic a JVM build tool (`mvn`, `mvnd`, `barista`) emits
//! during a benchmark run, and writes the result to a `.har`
//! ([HTTP Archive][har-spec]) file that the companion analysis crate
//! `barista-netanalyze` consumes.
//!
//! This crate does **not** reimplement mitmproxy. It drives the real
//! mitmproxy binary because (a) mitmproxy is the canonical mature
//! implementation of its niche and (b) re-implementing a TLS-decrypting
//! proxy is well outside the scope of the resource-efficiency program.
//! The trade-off is that real captures require `mitmdump` to be on
//! `$PATH`; the crate degrades gracefully when it isn't — see
//! [`CaSetup`] for the "what's missing and how do I fix it" reporter.
//!
//! ## Three-line usage
//!
//! ```no_run
//! # async fn demo() -> Result<(), barista_netcap::NetcapError> {
//! use barista_netcap::{CaptureConfig, CaptureSession};
//!
//! let cfg = CaptureConfig::for_har("/tmp/session.har");
//! let session = CaptureSession::start(cfg).await?;
//! // ... run the build tool against `127.0.0.1:{session.listen_port()}` ...
//! let summary = session.stop().await?;
//! println!("captured {} requests", summary.har.entry_count);
//! # Ok(()) }
//! ```
//!
//! ## CA setup
//!
//! Before HTTPS traffic can be decrypted, mitmproxy's CA must be trusted
//! by the JDK running the build. [`CaSetup::ensure_installed`] reports
//! what state the host is in and prints the exact commands the user
//! should run; it deliberately does **not** modify the truststore.
//!
//! [mitmproxy]: https://mitmproxy.org
//! [har-spec]: http://www.softwareishard.com/blog/har-12-spec/

pub mod ca;
pub mod error;
pub mod har;
pub mod session;

pub use ca::{CaCertificate, CaSetup, CaStatus};
pub use error::NetcapError;
pub use har::{HarSummary, validate as validate_har};
pub use session::{CaptureConfig, CaptureSession, CaptureSummary};
