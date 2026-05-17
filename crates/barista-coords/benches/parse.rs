// Integration-test / example / benchmark target — workspace security
// lints are allowed here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`)
// is the documented contract for failing a test loudly. This allow block
// keeps the crate root's `#![allow(...)]` from being silently dropped by
// the separate compilation unit each bench file forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Criterion microbenchmarks for Maven coordinate parsing and rendering.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p barista-coords --bench parse
//! ```
//!
//! These are informative numbers used to spot egregious regressions
//! during development. The canonical regression detector lives in the
//! Tier-2 gate.

use std::hint::black_box;
use std::str::FromStr;

use barista_coords::{Coords, GATC, GATCV};
use criterion::{Criterion, criterion_group, criterion_main};

/// Parsing the 3-component form (`g:a:v`) — the common case in a
/// lockfile entry.
fn bench_gatcv_parse_short(c: &mut Criterion) {
    let input = "org.apache.commons:commons-lang3:3.14.0";
    c.bench_function("GATCV parse: 3-component (g:a:v)", |bench| {
        bench.iter(|| {
            let v = GATCV::from_str(black_box(input)).unwrap();
            black_box(v);
        });
    });
}

/// Parsing the 5-component form (`g:a:packaging:classifier:v`) — the
/// classifier-bearing path (e.g. `sources` / `javadoc` jars).
fn bench_gatcv_parse_full(c: &mut Criterion) {
    let input = "com.google.guava:guava:jar:sources:33.0.0-jre";
    c.bench_function("GATCV parse: 5-component (g:a:p:c:v)", |bench| {
        bench.iter(|| {
            let v = GATCV::from_str(black_box(input)).unwrap();
            black_box(v);
        });
    });
}

/// Parsing the minimal 2-component `Coords` form — the resolution
/// identity used as a HashMap key during conflict resolution.
fn bench_coords_parse(c: &mut Criterion) {
    let input = "org.apache.commons:commons-lang3";
    c.bench_function("Coords parse: g:a", |bench| {
        bench.iter(|| {
            let v = Coords::from_str(black_box(input)).unwrap();
            black_box(v);
        });
    });
}

/// Parsing a 4-component `GATC` with explicit packaging + classifier.
fn bench_gatc_parse_with_classifier(c: &mut Criterion) {
    let input = "com.google.guava:guava:jar:sources";
    c.bench_function("GATC parse: 4-component (g:a:p:c)", |bench| {
        bench.iter(|| {
            let v = GATC::from_str(black_box(input)).unwrap();
            black_box(v);
        });
    });
}

/// Display round-trip on a 5-component `GATCV`: parse once, then
/// measure `to_string` cost. Catches regressions in the formatter
/// path (which has packaging/classifier branching).
fn bench_gatcv_display_full(c: &mut Criterion) {
    let v: GATCV = "com.google.guava:guava:jar:sources:33.0.0-jre"
        .parse()
        .unwrap();
    c.bench_function("GATCV display: 5-component", |bench| {
        bench.iter(|| {
            let s = black_box(&v).to_string();
            black_box(s);
        });
    });
}

criterion_group!(
    benches,
    bench_gatcv_parse_short,
    bench_gatcv_parse_full,
    bench_coords_parse,
    bench_gatc_parse_with_classifier,
    bench_gatcv_display_full,
);
criterion_main!(benches);
