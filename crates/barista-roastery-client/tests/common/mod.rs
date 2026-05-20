// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared test scaffolding for the roastery-client integration
//! suite.
//!
//! The harness mirrors the in-process server pattern from the
//! roastery crate's own tests: spin a real `roastery::run`-style
//! server on an ephemeral port, return a fixture with the bind
//! address, and tear it down on Drop. The client under test then
//! points at the fixture's URL.

// Each integration-test file compiles `common` as part of its own
// crate and uses only the subset of fixtures it needs (e.g.
// `roastery_speedup.rs` uses only `spawn_plain_server`). Suppress the
// resulting dead-code warnings for the unused fixtures rather than
// duplicating the harness per consumer.
#![allow(dead_code)]

pub mod certs;
pub mod harness;
