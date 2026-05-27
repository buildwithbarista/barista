# Barista Benchmark Methodology — v0.1

This document defines the reference hardware and measurement methodology for Barista's performance benchmarks, ensuring reproducibility and comparability over time.

## Benchmark Tiers

Barista uses a three-tier approach to performance validation:

### Tier 1: Developer-Loop Microbenchmarks

Sub-second to ~60s `criterion`-based benchmarks run locally on every change to performance-sensitive code. These measure internal hot paths: version comparison, POM parsing, content-addressed store hash verification, resolver inner loops, and inter-process communication framing.

- **Scope:** Barista-internal only; not competitive.
- **Location:** `crates/<crate>/benches/`
- **Hardware:** Developer's machine (no fixed requirements).

### Tier 2: CI/CD Performance Gate

Triggered on pull requests that touch `barista-resolver`, `barista-cache`, `barista-ipc`, or `barback/`. A curated subset of ~5 projects from the Tier-3 corpus runs within a <10 min CI budget. This enforces *relative* regression thresholds and blocks merge on excessive drift.

- **Scope:** Relative regression gates; tolerates any contributor hardware.
- **Implementation:** `.github/workflows/perf-gate.yml`
- **Escape hatch:** `docs/perf/accepted-regressions.md` for documented, approved regressions.

### Tier 3: Public Competitive Benchmarks

Task-oriented corpus (`bench/projects/`) measured against Maven 3.9.x, Maven 4.0.x, and Maven Daemon 2.x across JDK 17 and JDK 21. Runs on every release tag plus nightly on fixed reference hardware, published to the public dashboard.

- **Scope:** Public-facing, reproducible proof of performance.
- **Frequency:** Every release tag + nightly.
- **Publication:** Public benchmark dashboard.

## Reference Hardware — Tier 3

All Tier-3 results are measured on one of three reference platforms:

| Platform | Hardware | OS | Role |
|----------|----------|----|----|
| **R-Bench-1 (primary, x86)** | AMD Ryzen 9 7950X (16c/32t), 64 GB DDR5-6000, 2 TB NVMe SSD | Ubuntu 24.04 LTS | Headline results; self-hosted dedicated runner |
| **R-Bench-2 (Apple silicon)** | Mac mini M2 Pro, 32 GB RAM, 1 TB SSD | macOS 14 | Self-hosted dedicated runner |
| **R-Bench-3 (cloud, AWS)** | `m7i.4xlarge` (16 vCPU, 64 GB RAM, gp3 EBS @ 16k IOPS) | Ubuntu 24.04 LTS | Reviewer-reproducible reference; `us-east-1` |

Each self-hosted runner (R-Bench-1 and R-Bench-2) is isolated on a dedicated network segment with no other workloads and redundant connectivity. R-Bench-3 uses standard AWS on-demand instances, spun up per release tag and nightly run, then terminated.

### The Cloud Reference: Reproducibility & Integrity

R-Bench-3 serves as the integrity check on self-hosted runners. Because it uses commodity cloud hardware with no co-tenant workload in the relevant CPU and memory budget, any reviewer can rent the same instance type, run the benchmark from this repository, and directly verify the published numbers.

Per release tag, R-Bench-3 results are required to fall within a documented variance band of R-Bench-1:

- **Warm metrics (primed cache):** ±15%
- **Cold metrics (cleared cache):** ±25%

(These bands account for cloud-vs-bare-metal CPU and SSD latency differences.)

Drift outside these bounds flags a potentially compromised self-hosted runner and triggers investigation.

## Result Authenticity & Storage

Each runner signs its result bundle with a per-runner GPG key. Public keys are stored in `bench/runner-keys/` and private keys are securely escrowed off-runner, so result authenticity survives hardware changes.

Result bundles are published immediately to object storage (Cloudflare R2), ensuring the historical dataset is independent of any single runner's uptime.

The result-bundle schema is defined in `crates/barista-bench/schema/results.schema.json` and is versioned.

## Measurement Methodology

Each benchmark dimension:

1. Runs a warmup sequence (iterations discarded).
2. Runs measured iterations.
3. Reports **median** and **p95** latency.

Cold-path measurements clear caches before the run; warm measurements reuse a primed cache state.

**Tier 2** enforces relative thresholds per dimension (defined in `.github/workflows/perf-gate.yml`).

**Tier 3** publishes the full cross-tool comparison (Barista vs Maven variants).

## Status & Roadmap — v0.1

In v0.1, R-Bench-1 and R-Bench-2 are operator-hosted by deliberate, transparent choice. This allows the performance program to ship at the intended rigor before adoption signals justify dedicated or sponsored infrastructure. Migration to dedicated or vendor-sponsored hosting is a tracked future item.

The public benchmark dashboard surfaces a prominent "last updated" timestamp per data series and is transparent about which runners currently have fresh results.
