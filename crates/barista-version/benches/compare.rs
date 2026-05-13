//! Criterion microbenchmarks for `Version` parsing and comparison.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p barista-version --bench compare
//! ```
//!
//! These are informative numbers used to spot egregious regressions
//! during development. The canonical regression detector lives in the
//! Tier-2 gate.

use std::hint::black_box;

use barista_version::Version;
use criterion::{criterion_group, criterion_main, Criterion};

/// Comparing two short release versions — the common case.
fn bench_simple_compare(c: &mut Criterion) {
    let a: Version = "1.2.3".parse().unwrap();
    let b: Version = "1.2.4".parse().unwrap();
    c.bench_function("Version cmp: 1.2.3 vs 1.2.4", |bench| {
        bench.iter(|| black_box(&a).cmp(black_box(&b)));
    });
}

/// Comparing two versions with qualifiers — exercises the
/// qualifier-ordering path (`alpha` < `rc`, etc.).
fn bench_qualifier_compare(c: &mut Criterion) {
    let a: Version = "1.0.0-alpha-1".parse().unwrap();
    let b: Version = "1.0.0-rc-3".parse().unwrap();
    c.bench_function("Version cmp: 1.0.0-alpha-1 vs 1.0.0-rc-3", |bench| {
        bench.iter(|| black_box(&a).cmp(black_box(&b)));
    });
}

/// Just the `FromStr` cost of parsing a typical version string.
/// Useful baseline for separating parse-vs-compare.
fn bench_parse_only(c: &mut Criterion) {
    c.bench_function("Version parse: 1.0.0-rc-3", |bench| {
        bench.iter(|| {
            let v: Version = black_box("1.0.0-rc-3").parse().unwrap();
            black_box(v);
        });
    });
}

/// Comparing two versions that are canonically equal but textually
/// different — exercises the trailing-zero normalization path.
fn bench_canonical_eq(c: &mut Criterion) {
    let a: Version = "1".parse().unwrap();
    let b: Version = "1.0.0".parse().unwrap();
    c.bench_function("Version cmp: 1 vs 1.0.0 (canonical eq)", |bench| {
        bench.iter(|| black_box(&a).cmp(black_box(&b)));
    });
}

criterion_group!(
    benches,
    bench_simple_compare,
    bench_qualifier_compare,
    bench_parse_only,
    bench_canonical_eq
);
criterion_main!(benches);
