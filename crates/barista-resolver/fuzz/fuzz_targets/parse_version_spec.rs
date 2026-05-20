// SPDX-License-Identifier: MIT OR Apache-2.0

#![no_main]
//! Fuzz target: `VersionSpec::parse` must never panic on arbitrary
//! byte input. This is the strongest invariant we can assert without
//! a ground-truth oracle.

use barista_resolver::version_spec::VersionSpec;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = VersionSpec::parse(s);
    }
});
