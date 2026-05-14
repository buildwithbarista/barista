// Synthetic violation: tripped by
//   .semgrep/barista-rust.yml :: barista-rust-unchecked-command-new
//
// This fixture is NOT compiled. It lives outside any crate's source
// tree (`crates/<name>/src/**`) so cargo never sees it; the `.rs`
// extension is required for Semgrep to identify the language.
//
// The pattern under test: `Command::new` invoked with an interpolated
// user-controlled string. The intended outcome is for Semgrep to emit
// exactly one finding.

use std::process::Command;

pub fn run_user_program(user_input: &str) -> std::io::Result<std::process::ExitStatus> {
    // This is the violation. The program-name argument is a
    // user-controlled string with no allowlist check.
    Command::new(user_input).status()
}
