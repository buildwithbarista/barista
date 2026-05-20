// SPDX-License-Identifier: MIT OR Apache-2.0

// CLI entry points wrap library calls in `?` propagation. Surface
// any unwrap/expect/panic here as a documented panic-path so the
// workspace lint policy stays meaningful.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `barista-netanalyze` CLI.
//!
//! Thin wrapper around the library: load a `.har`, run the default
//! analyzer registry, write per-finding markdown files to a
//! configurable output directory.
//!
//! ```text
//! barista-netanalyze --input session.har --output-dir auto-generated/
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use barista_netanalyze::{analyze, load_har, write_findings};

/// Run the analysis pipeline over a captured `.har` and write
/// per-finding markdown files to disk.
#[derive(Debug, Parser)]
#[command(name = "barista-netanalyze", version, about, long_about = None)]
struct Args {
    /// Path to the input `.har` file (produced by `barista-netcap`).
    #[arg(short, long)]
    input: PathBuf,

    /// Directory where per-finding `.md` files are written. Created
    /// if it doesn't exist. Defaults to `./auto-generated/`.
    #[arg(short = 'o', long = "output-dir", default_value = "./auto-generated")]
    output_dir: PathBuf,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(written) => {
            println!("wrote {written} finding markdown file(s)");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("barista-netanalyze: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<usize, barista_netanalyze::AnalyzeError> {
    let har = load_har(&args.input)?;
    let findings = analyze(&har);
    let written = write_findings(&findings, &args.output_dir)?;
    Ok(written.len())
}
