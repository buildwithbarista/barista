// Integration-test / example / benchmark target — workspace security
// lints are allowed here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`)
// is the documented contract for failing a test loudly. This allow block
// keeps the crate root's `#![allow(...)]` from being silently dropped by
// the separate compilation unit each test file forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Report combined prune rates (nearest-wins + skipper) on a
//! corpus of synthetic graphs and assert the milestone-level 60%
//! target from PRD §5 on the fan-out 5x10 graph.
//!
//! Run with:
//!
//! ```text
//! cargo run --release -p barista-resolver --example skip_rate_report
//! ```
//!
//! Exits `0` when the combined-prune-rate threshold is met on the
//! gating graph, `1` otherwise.
//!
//! # Methodology
//!
//! The PRD calls for "BFS+Skipper pruning ≥ 60% of nodes on the
//! corpus median". Two metrics are useful:
//!
//! 1. **Skipper-only rate**:
//!    `SkipperStats.total_skips / SkipperStats.total_decisions`.
//!    May be low because the walker's nearest-wins check already
//!    prunes most candidates before the SkipperSeam is consulted
//!    (the seam currently sits AFTER nearest-wins in `walker.rs`).
//!
//! 2. **Combined prune rate**:
//!    `1 - actual_pom_fetches / naive_visit_count`. Counts every
//!    candidate-visit the walker's combined BFS+nearest-wins+
//!    skipper machinery avoids — which is the user-visible payoff
//!    PRD §5 cares about.
//!
//! The gate uses (2). (1) is reported alongside for transparency.

#[path = "../benches/common/graphs.rs"]
mod graphs;

use std::process::ExitCode;

use barista_resolver::walker::{WalkOptions, walk};

use crate::graphs::{Graph, as_resolved, deep_chain, diamond, fan_out_shared, full_synthetic};

/// One row of the report.
struct Row {
    name: &'static str,
    naive_visits: u64,
    pom_fetches_skipper_on: u64,
    pom_fetches_skipper_off: u64,
    skipper_only_rate: f64,
}

impl Row {
    /// `1 - actual_with_skipper / naive_visit_count`.
    fn combined_prune_rate(&self) -> f64 {
        if self.naive_visits == 0 {
            return 0.0;
        }
        1.0 - (self.pom_fetches_skipper_on as f64 / self.naive_visits as f64)
    }
}

fn run_graph(g: Graph) -> Row {
    let resolved = as_resolved(g.root.clone());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("tokio runtime");

    // Pass 1: skipper enabled.
    g.source.reset_counters();
    let opts_on = WalkOptions {
        enable_skipper: true,
        ..WalkOptions::default()
    };
    let graph_on = rt
        .block_on(walk(&resolved, &g.source, &opts_on))
        .expect("walk ok");
    let fetches_on = g.source.pom_fetches();
    let skipper_rate = graph_on.skipper_stats.skip_rate();

    // Pass 2: skipper disabled (nearest-wins still applies).
    g.source.reset_counters();
    let opts_off = WalkOptions {
        enable_skipper: false,
        ..WalkOptions::default()
    };
    let _ = rt
        .block_on(walk(&resolved, &g.source, &opts_off))
        .expect("walk ok");
    let fetches_off = g.source.pom_fetches();

    Row {
        name: g.name,
        naive_visits: g.naive_visits,
        pom_fetches_skipper_on: fetches_on,
        pom_fetches_skipper_off: fetches_off,
        skipper_only_rate: skipper_rate,
    }
}

fn main() -> ExitCode {
    let rows = [
        run_graph(diamond()),
        run_graph(fan_out_shared(5, 10)),
        run_graph(deep_chain(50)),
        run_graph(full_synthetic()),
    ];

    println!("Resolver prune-rate report");
    println!("==========================");
    println!();
    println!(
        "{:<18} | {:>14} | {:>14} | {:>14} | {:>14} | {:>14}",
        "graph", "naive visits", "fetches (on)", "fetches (off)", "skipper rate", "combined rate",
    );
    println!("{}", "-".repeat(110));
    for r in &rows {
        println!(
            "{:<18} | {:>14} | {:>14} | {:>14} | {:>13.1}% | {:>13.1}%",
            r.name,
            r.naive_visits,
            r.pom_fetches_skipper_on,
            r.pom_fetches_skipper_off,
            r.skipper_only_rate * 100.0,
            r.combined_prune_rate() * 100.0,
        );
    }
    println!();

    // The gating row: fan-out 5x10 is the standard target.
    let gate = rows
        .iter()
        .find(|r| r.name == "fan_out_shared")
        .expect("fan_out_shared row present");

    const TARGET: f64 = 0.60;
    let rate = gate.combined_prune_rate();
    if rate >= TARGET {
        println!(
            "PASS: combined prune rate on `{}` = {:.1}% (>= {:.0}%)",
            gate.name,
            rate * 100.0,
            TARGET * 100.0,
        );
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "FAIL: combined prune rate on `{}` = {:.1}% (< {:.0}%)",
            gate.name,
            rate * 100.0,
            TARGET * 100.0,
        );
        ExitCode::FAILURE
    }
}
