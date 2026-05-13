//! Criterion microbenchmarks for raw `pom.xml` parsing.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p barista-pom --bench parse
//! ```
//!
//! These are informative numbers used to spot egregious regressions
//! during development. The canonical regression detector lives in the
//! Tier-2 gate.

use std::hint::black_box;

use barista_pom::parse_pom;
use criterion::{Criterion, criterion_group, criterion_main};

/// A representative POM with a parent block, properties, ~7 deps,
/// a build section, and a profile. Embedded so the bench is
/// self-contained and doesn't require the test corpus.
const SAMPLE_POM: &str = include_str!("fixtures/sample-pom.xml");

/// End-to-end parse cost of a small, realistic POM.
fn bench_parse_small(c: &mut Criterion) {
    c.bench_function("RawPom parse: sample-pom.xml (small)", |bench| {
        bench.iter(|| {
            let pom = parse_pom(black_box(SAMPLE_POM)).unwrap();
            black_box(pom);
        });
    });
}

criterion_group!(benches, bench_parse_small);
criterion_main!(benches);
