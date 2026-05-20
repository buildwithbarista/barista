// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Integration tests for [`barista_netcap::CaSetup`].
//!
//! These tests must run cleanly on a host *without* mitmproxy installed,
//! because that's the default state in CI. The contract under test is
//! "ensure_installed returns a sensible [`CaStatus`] instead of panicking
//! or erroring," so we exercise the function with controlled
//! `$HOME`-stand-ins and assert the variant matches what's on disk.

use std::fs;

use barista_netcap::ca::{CaSetup, CaStatus};

/// Confidence check: calling against the real environment never panics
/// and produces some `Ok(_)` value. Whether that value is
/// `MissingMitmproxy`, `NotInstalled`, or `Installed` depends on the
/// CI host — all three are valid.
#[test]
fn ensure_installed_does_not_panic_on_host() {
    let _ = CaSetup::ensure_installed().expect("ensure_installed should not Err on a normal host");
}

#[test]
fn reports_not_installed_when_home_has_no_mitmproxy_dir() {
    // We can't observe the result reliably without controlling
    // mitmdump-on-PATH (the host might have it installed). The check
    // below tolerates either:
    //   - MissingMitmproxy (host has no mitmdump on PATH), OR
    //   - NotInstalled     (host has mitmdump but tempdir HOME has no
    //                       ~/.mitmproxy/mitmproxy-ca-cert.pem)
    let home = tempfile::tempdir().expect("tempdir");
    let status =
        CaSetup::ensure_installed_with_home(home.path()).expect("ensure_installed_with_home");

    match status {
        CaStatus::Installed { .. } => panic!(
            "tempdir HOME should not contain ~/.mitmproxy/mitmproxy-ca-cert.pem; \
             test fixture is wrong"
        ),
        CaStatus::NotInstalled {
            expected_path,
            suggested_commands,
        } => {
            assert!(expected_path.starts_with(home.path()));
            assert!(
                !suggested_commands.is_empty(),
                "NotInstalled should carry actionable hints"
            );
        }
        CaStatus::MissingMitmproxy { install_hints } => {
            assert!(
                !install_hints.is_empty(),
                "MissingMitmproxy should carry install hints"
            );
        }
    }
}

#[test]
fn reports_installed_when_pem_present_and_mitmdump_on_path() {
    // We can only assert the Installed branch when mitmdump is on PATH
    // — otherwise the function correctly short-circuits to
    // MissingMitmproxy regardless of the PEM. Skip the assertion arm
    // when mitmproxy isn't installed so the test stays green on
    // mitmproxy-less CI.
    let home = tempfile::tempdir().expect("tempdir");
    let mitm_dir = home.path().join(".mitmproxy");
    fs::create_dir_all(&mitm_dir).expect("mkdir");
    let cert = mitm_dir.join("mitmproxy-ca-cert.pem");
    fs::write(
        &cert,
        b"-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----\n",
    )
    .expect("write cert");

    let status = CaSetup::ensure_installed_with_home(home.path()).expect("ensure_installed");

    match status {
        CaStatus::Installed {
            cert: c,
            suggested_truststore_commands,
        } => {
            assert_eq!(c.path, cert);
            assert!(
                suggested_truststore_commands
                    .iter()
                    .any(|s| s.contains("keytool")),
                "expected a keytool command in the suggested list, got {suggested_truststore_commands:?}"
            );
        }
        CaStatus::NotInstalled { .. } => panic!("PEM was written; should not be NotInstalled"),
        CaStatus::MissingMitmproxy { .. } => {
            // Host has no mitmdump — accept and move on.
        }
    }
}
