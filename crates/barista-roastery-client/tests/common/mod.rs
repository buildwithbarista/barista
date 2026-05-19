//! Shared test scaffolding for the roastery-client integration
//! suite.
//!
//! The harness mirrors the in-process server pattern from the
//! roastery crate's own tests: spin a real `roastery::run`-style
//! server on an ephemeral port, return a fixture with the bind
//! address, and tear it down on Drop. The client under test then
//! points at the fixture's URL.

pub mod certs;
pub mod harness;
