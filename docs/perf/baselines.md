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

## barista-resolver

Walker + skipper microbenches over synthetic graphs. Run with
`cargo bench -p barista-resolver --bench walker` (or `--bench walker
-- --quick` for the values below). The PRD §5 combined-prune-rate
gate is checked via `cargo run --release -p barista-resolver
--example skip_rate_report`.

| Benchmark | Median per-iter |
|---|---|
| Walker diamond, skipper on | 8.22 µs |
| Walker diamond, skipper off | 8.01 µs |
| Walker fan-out 5×10, skipper on | 73.00 µs |
| Walker fan-out 5×10, skipper off | 80.09 µs |
| Walker deep chain (depth 50), skipper on | 324.35 µs |
| Walker full synthetic, skipper on | 118.88 µs |

### Combined prune rate (PRD §5)

The "combined prune rate" is `1 - actual_pom_fetches /
naive_visit_count`, where `naive_visit_count` is the size of the
unfolded BFS tree (the number of candidate-visits a non-pruning walk
would issue). It counts every visit BFS+nearest-wins+skipper
collectively avoid — the user-visible payoff PRD §5 cares about.
The skipper-only rate (`SkipperStats::skip_rate`) is reported
alongside for transparency; it can be lower because nearest-wins
already prunes most candidates before the SkipperSeam is consulted.

| Graph | Naive visits | Fetches (skipper on) | Skipper-only rate | Combined prune rate |
|---|---:|---:|---:|---:|
| diamond | 4 | 3 | 25.0% | 25.0% |
| fan-out 5×10 (shared leaves) | 55 | 15 | 72.7% | **72.7%** |
| deep chain (depth 50) | 50 | 50 | 0.0% | 0.0% |
| full synthetic (6×8×4) | 246 | 18 | 32.6% | 92.7% |

The fan-out 5×10 row is the gating measurement for the PRD §5 ≥60%
target. The synthetic fan-out stands in for "corpus median" until
the resolver corpus grows beyond its five seed fixtures. The deep
chain is a deliberate baseline: a linear graph has no cross-edges
for the skipper to prune.
