// Clean counterpart: must NOT be flagged by
//   .semgrep/barista-rust.yml :: barista-rust-unchecked-command-new
//
// `Command::new` with a hard-coded literal program name is the
// well-formed shape. The rule explicitly `pattern-not`s this case so
// the round-trip test asserts zero findings here.

use std::process::Command;

pub fn run_mvn() -> std::io::Result<std::process::ExitStatus> {
    Command::new("mvn").arg("--version").status()
}
