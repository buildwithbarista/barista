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
