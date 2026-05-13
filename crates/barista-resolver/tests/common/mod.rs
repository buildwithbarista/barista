//! Shared test helpers for resolver integration tests.
//!
//! Integration test files (`tests/*.rs`) each compile to their own
//! binary, so common code goes here in `tests/common/` and is included
//! via `mod common;` from each test file.

pub mod fixture_source;
