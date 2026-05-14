//! `xtask` — workspace task runner.
//!
//! Entry point dispatches to subcommand modules. Today there is one
//! subcommand (`security`); future tasks (e.g., release packaging,
//! benchmark replays, doc generation) get a new module each.
//!
//! Per-binary clippy allows: this is a CLI entry point, so `unwrap` /
//! `expect` / `panic` are the documented contract for "fail fast,
//! print a backtrace, exit non-zero". The workspace-wide lint config
//! treats them as warnings; we silence them at the binary root rather
//! than peppering `#[allow]` over every call site.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use clap::{Parser, Subcommand};

mod security;

#[derive(Parser, Debug)]
#[command(
    name = "xtask",
    about = "Workspace task runner — see subcommands for what's available.",
    version,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the full locally-runnable security suite (clippy, cargo-deny,
    /// cargo-audit, semgrep, gitleaks). Optional tools that aren't
    /// installed are skipped with an install hint unless `--strict` is
    /// set.
    Security(security::Args),
}

fn main() {
    let cli = Cli::parse();
    let exit_code = match cli.command {
        Command::Security(args) => security::run(args),
    };
    std::process::exit(exit_code);
}
