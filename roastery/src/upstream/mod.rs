// SPDX-License-Identifier: MIT OR Apache-2.0

//! Upstream-on-miss fetch path.
//!
//! When a `GET /v1/cas/sha256/{digest}` lands on a digest the local
//! CAS doesn't have, the handler consults this module: if the request
//! carries an `X-Barista-Coords` hint header and the operator has
//! opted into upstream fetches, the [`UpstreamFetcher`] attempts to
//! fetch the artifact from one of the configured Maven repositories.
//!
//! ## Contract
//!
//! - **Trigger**: only `GET`. `HEAD` and `PUT` never invoke the
//!   fetcher.
//! - **Hint**: the client sends `X-Barista-Coords: g:a[:t[:c]]:v`.
//!   Without it the handler returns 404 even if upstreams are
//!   configured — the digest alone isn't enough to know where to
//!   look.
//! - **Verification**: the upstream's bytes are streamed through
//!   [`crate::storage::Cas::put`], which hashes-and-verifies in
//!   flight. A repository that serves the wrong bytes for the
//!   requested digest is logged + the next repository is tried.
//! - **Side effect**: a successful fetch persists the blob into the
//!   local CAS. The handler then re-issues the local `stat`+`get`
//!   path to stream the response, so concurrent requests for the
//!   same digest deduplicate via the local store.
//!
//! See [`UpstreamFetcher::try_fetch`] for the per-attempt algorithm
//! and the metric labels recorded.

pub mod coords;
pub mod error;
pub mod fetch;

pub use coords::Coords;
pub use error::UpstreamError;
pub use fetch::UpstreamFetcher;
