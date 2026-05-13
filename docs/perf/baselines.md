# Performance baselines

Recorded Criterion microbenchmark results for the Barista crates. Each
section reports the latest median per-iter wall time on the developer's
machine. Numbers are informative — the canonical regression detector is
the Tier-2 gate (separate document).

These numbers will go stale as the code changes. Re-run the benches
listed below to refresh them, and update both the table values and the
hardware/date header.

The hardware these numbers were taken on:

- CPU: Apple M4 Max
- RAM: 128 GB
- OS: Darwin 25.2.0 arm64 (macOS)
- Rust: rustc 1.86.0 (05f9846f8 2025-03-31)
- Date: 2026-05-13

## barista-version

Run with `cargo bench -p barista-version --bench compare` to refresh.

| Benchmark | Median per-iter |
|---|---|
| Version cmp: 1.2.3 vs 1.2.4 | 14.02 ns |
| Version cmp: 1.0.0-alpha-1 vs 1.0.0-rc-3 | 64.60 ns |
| Version parse: 1.0.0-rc-3 | 420.42 ns |
| Version cmp: 1 vs 1.0.0 (canonical eq) | 4.55 ns |

## barista-pom

Run with `cargo bench -p barista-pom --bench parse` and `cargo bench -p barista-pom --bench effective` to refresh.

| Benchmark | Median per-iter |
|---|---|
| RawPom parse: sample-pom.xml (small) | 13.65 µs |
| Effective: build_effective on no-parent small POM | 2.05 µs |
| Effective: interpolate_string (~5 placeholders) | 4.11 µs |
