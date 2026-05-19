//! The `roastery` binary entry point.
//!
//! Keep this file slim: parse config, init tracing, hand off to
//! [`roastery::run`]. All testable logic lives in the library.

// CLI entrypoints document their panic contract: bubble errors up,
// then `eprintln!` + `exit(1)`. The `expect_used` allow is for the
// last-resort runtime construction error path below.
#![allow(clippy::expect_used)]

use std::process::ExitCode;

use roastery::{ServerConfig, init_tracing, run};

fn main() -> ExitCode {
    // Minimal flag handling so container smoke tests can probe the
    // binary without standing up the network stack. Distroless images
    // ship no shell, so `docker run <image> --version` (the canonical
    // image-boot probe) routes here directly. Any non-flag argv
    // falls through to the server startup path below.
    //
    // This is deliberately not a `clap`-driven CLI: roastery is
    // env-var-configured (see `ServerConfig::from_env`), so the only
    // legitimate argv inputs are the two diagnostic flags below.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("roastery {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!(
                    "roastery {} — remote artifact cache server\n\
                     \n\
                     Usage: roastery [--version] [--help]\n\
                     \n\
                     Configuration is read from environment variables.\n\
                     See https://github.com/buildwithbarista/barista for docs.",
                    env!("CARGO_PKG_VERSION")
                );
                return ExitCode::SUCCESS;
            }
            _ => {
                eprintln!("roastery: unknown argument: {arg}");
                eprintln!("roastery: configuration is read from environment variables; see --help");
                return ExitCode::from(2);
            }
        }
    }

    init_tracing();

    let config = match ServerConfig::from_env() {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("roastery: {err}");
            return ExitCode::from(2);
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    match runtime.block_on(run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("roastery: {err}");
            ExitCode::FAILURE
        }
    }
}
