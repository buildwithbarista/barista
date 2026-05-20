// SPDX-License-Identifier: MIT OR Apache-2.0

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

//! Tier-1 microbenchmarks for the resolver.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p barista-resolver --bench walker
//! ```
//!
//! Three benchmark families:
//!
//! - `walker: <graph>, skipper on` — `walk()` end-to-end with the
//!   BFS+Skipper pruning enabled. Measures absolute resolution wall
//!   time on a hand-built synthetic graph.
//! - `walker: <graph>, skipper off` — same graph with
//!   [`WalkOptions::enable_skipper`] = `false`. The wall-time delta
//!   versus "skipper on" is the skipper's payoff above what
//!   nearest-wins already achieves.
//! - `walker: full synthetic` — a larger two-level fan-out graph
//!   (~250 unfolded visits, ~18 unique coords) stresses BFS frontier
//!   management and skipper pruning at scale.
//!
//! These are informative numbers used to spot egregious regressions
//! during development. Reproducible numbers (and the 60% combined
//! prune-rate gate from PRD §5) live in
//! `examples/skip_rate_report.rs`.

#[path = "common/graphs.rs"]
mod graphs;

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use tokio::runtime::Builder;

use barista_resolver::walker::{WalkOptions, walk};

use crate::graphs::{Graph, as_resolved, deep_chain, diamond, fan_out_shared, full_synthetic};

/// Drive a single `walk()` from a pre-built `Graph`, with the
/// requested skipper setting. The graph is rebuilt at setup time
/// (outside Criterion's timing loop); only the walk itself is
/// measured.
fn bench_one(c: &mut Criterion, label: &str, build: fn() -> Graph, enable_skipper: bool) {
    let rt = Builder::new_current_thread()
        .build()
        .expect("tokio current-thread runtime");
    let g = build();
    let resolved = as_resolved(g.root.clone());
    let opts = WalkOptions {
        enable_skipper,
        ..WalkOptions::default()
    };

    c.bench_function(label, |b| {
        b.iter(|| {
            // Reset fetch counters each iteration so they don't grow
            // unboundedly across the Criterion warm-up + measurement
            // passes. The counter is incidental to the timed work.
            g.source.reset_counters();
            let result = rt.block_on(walk(
                black_box(&resolved),
                black_box(&g.source),
                black_box(&opts),
            ));
            black_box(result.expect("walk ok"));
        });
    });
}

fn bench_walker_diamond_on(c: &mut Criterion) {
    bench_one(c, "walker: diamond, skipper on", diamond, true);
}

fn bench_walker_diamond_off(c: &mut Criterion) {
    bench_one(c, "walker: diamond, skipper off", diamond, false);
}

fn bench_walker_fan_out_on(c: &mut Criterion) {
    bench_one(
        c,
        "walker: fan-out 5x10, skipper on",
        || fan_out_shared(5, 10),
        true,
    );
}

fn bench_walker_fan_out_off(c: &mut Criterion) {
    bench_one(
        c,
        "walker: fan-out 5x10, skipper off",
        || fan_out_shared(5, 10),
        false,
    );
}

fn bench_walker_deep_chain(c: &mut Criterion) {
    bench_one(
        c,
        "walker: deep chain (depth 50), skipper on",
        || deep_chain(50),
        true,
    );
}

fn bench_walker_full_synthetic(c: &mut Criterion) {
    bench_one(
        c,
        "walker: full synthetic, skipper on",
        full_synthetic,
        true,
    );
}

criterion_group!(
    benches,
    bench_walker_diamond_on,
    bench_walker_diamond_off,
    bench_walker_fan_out_on,
    bench_walker_fan_out_off,
    bench_walker_deep_chain,
    bench_walker_full_synthetic,
);
criterion_main!(benches);
