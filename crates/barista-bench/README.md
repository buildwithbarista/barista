# barista-bench

Benchmark harness and on-disk schemas for [Barista](https://barista.build).

This crate is the source of truth for two contracts that drive Barista's
performance program:

| Contract              | File              | Format | Purpose                                                                 |
| --------------------- | ----------------- | ------ | ----------------------------------------------------------------------- |
| Benchmark manifest    | `Bench.toml`      | TOML   | Per-project: how to invoke the bench, what to measure, which tier.      |
| Benchmark results     | `results.json`    | JSON   | Per-run: measurements, summary stats, hardware fingerprint.             |

Both contracts have machine-readable JSON-Schema definitions under
[`schema/`](./schema/) (Draft 2020-12), so any consumer — CI gates, the
dashboard backend, third-party reviewers — can validate documents
without instantiating the Rust types.

## The three benchmark tiers

Barista's performance program splits into three tiers with different
audiences, frequencies, and rigor:

- **Tier 1** — sub-second to ~60-second microbenchmarks driven by
  [`criterion`](https://docs.rs/criterion) inside each crate's
  `benches/` directory. Run by developers on every change to a
  performance-sensitive code path.
- **Tier 2** — CI performance gate. Runs on every PR touching the
  resolver, cache, IPC, or `barback`. Self-hosted runner; enforces
  regression thresholds.
- **Tier 3** — public competitive corpus against `mvn`, `mvnd`, and
  cache extensions. Runs on every release tag and nightly. Published
  to `bench.barista.build`.

All three tiers emit the same `results.json` shape; the `hardware_tier`
field on each results document tags which tier produced it.

## `Bench.toml`

A small TOML document checked in next to every benchmark target:

```toml
schema = "barista.bench.manifest/v1"
id = "P02"
display_name = "Spring PetClinic"
category = "corpus"
corpus_id = "spring-petclinic-3.3.0"
command = "barista verify"
metrics = ["wall_ms", "cpu_user_ms", "peak_rss_kb"]
hardware_tier = 3
iterations = 5
warmup_iterations = 1

[allowed_variance]
wall_ms_p95 = 0.10
```

| Field               | Type             | Required | Notes                                                                                     |
| ------------------- | ---------------- | -------- | ----------------------------------------------------------------------------------------- |
| `schema`            | const string     | yes      | Always `"barista.bench.manifest/v1"`.                                                     |
| `id`                | string           | yes      | Stable identifier (e.g. `P02`, `version-compare`).                                        |
| `display_name`      | string           | yes      | Human-readable dashboard label.                                                           |
| `category`          | enum             | yes      | `microbench` or `corpus`.                                                                 |
| `corpus_id`         | string           | no       | Foreign key into a project corpus or fixture directory.                                   |
| `command`           | string           | yes      | Shell command line invoked under measurement.                                             |
| `metrics`           | array of strings | yes      | At least one. Known values: `wall_ms`, `cpu_user_ms`, `cpu_sys_ms`, `peak_rss_kb`, `network_bytes`, `disk_read_bytes`, `disk_write_bytes`. |
| `hardware_tier`     | 1 / 2 / 3        | yes      | Matches the tier this manifest is calibrated for.                                         |
| `iterations`        | int (≥1)         | no       | Default 5. Median + p95 are reported across these.                                        |
| `warmup_iterations` | int (≥0)         | no       | Default 1. Not measured.                                                                  |
| `allowed_variance`  | map<string,f64>  | no       | Per-metric variance budget for the regression gate.                                       |
| `labels`            | map<string,str>  | no       | Free-form labels surfaced on dashboard rows.                                              |

Unknown top-level fields are **rejected** — the contract is closed.

## `results.json`

Emitted by the harness at the end of every run. One file per run.

```json
{
  "schema": "barista.bench.results/v1",
  "manifest_id": "P02",
  "run_id": "2026-05-10T18:30:00Z-abcd1234",
  "timestamp": "2026-05-10T18:30:00Z",
  "git_sha": "abcd1234abcd1234abcd1234abcd1234abcd1234",
  "barista_version": "0.1.0",
  "hardware_tier": 3,
  "runner_id": "R-Bench-3",
  "hardware": { "id": "R-Bench-3", "cpu": "...", "cores_physical": 16, "cores_logical": 32, "memory_gb": 64, "os": "Ubuntu 24.04" },
  "iterations": [
    { "iteration": 0, "wall_ms": 8420, "cpu_user_ms": 31200, "peak_rss_kb": 1269760, "exit_code": 0 }
  ],
  "summary": { "avg_wall_ms": 8453.4, "median_wall_ms": 8463.0, "p95_wall_ms": 8511.0, "stddev_wall_ms": 44.2 },
  "metadata": { "jdk": "21", "baseline": "barista" }
}
```

Top-level keys, the `hardware` block, `iterations[]` entries, and the
`summary` block are all closed (`additionalProperties: false`). Free-form
extension lives in the `metadata` map.

## Library API (v0.1)

The crate exposes a deliberately small surface; higher-level orchestration
(CLI, corpus runner, dashboard uploader) consumes these types from
downstream crates and workflows.

```rust,no_run
use barista_bench::{load_manifest, write_results, ResultsDocument};

let manifest = load_manifest("Bench.toml")?;
// ... run the benchmark, build `results` ...
let results: ResultsDocument = todo!();
write_results("bench-results/results.json", &results)?;
# Ok::<_, barista_bench::Error>(())
```

## Versioning

Both contracts embed a schema discriminator (`barista.bench.manifest/v1`
and `barista.bench.results/v1`). Breaking changes bump the trailing
`vN`; consumers should reject unknown values rather than guess.

## License

Dual-licensed under MIT OR Apache-2.0.
