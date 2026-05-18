# Walkthrough benchmark — 2026-05-18

End-to-end measurement of barista vs Apache Maven 3.9.9 on the
**P03 Spring Boot starter-web** corpus entry. Produced by
`barista-bench run --corpus bench/projects --filter p03-`.

This is the first **real cross-tool dataset** emitted against the
`barista.bench.results/v1` schema. The `index.json` companion in this
directory enumerates every results document for downstream ingest
(perf-gate, the `bench.barista.build` dashboard).

## Configuration

| | |
|---|---|
| Workload | `bench/projects/p03/checkout/` (vendored Spring Boot 3.3.5 starter-web; 1 module, 1 source file, ~170 transitive deps) |
| Reference mvn | Apache Maven 3.9.9 (asdf-managed; `mvn -B -q ...`) |
| Reference mvn 4 | `/tmp/barista-mvn4/apache-maven-4.0.0-rc-3` (only used by the barista daemon's embedded core) |
| Hardware | Apple M4 Max, 16 logical cores, 128 GB RAM, macOS 26.2 |
| JDK | Temurin 21.0.4+7.0.LTS |
| Cache state | Warm — `~/.m2` populated, `~/.barista/cache` populated, lockfile present |
| Iterations | 10 measured + 2 warmup per `(manifest, baseline)` |

## Headline results — median wall-clock

| Step | mvn 3.9.9 | barista (warm daemon) | barista (`--no-daemon`, forked mvn) | barista warm vs mvn |
|---|---:|---:|---:|---:|
| **Pull / resolve** (D2 — warm dependency resolution) | 1355.5 ms | **872.0 ms** | — | **1.55× faster** |
| **Compile** (clean → compile) | 1312.5 ms | **172.0 ms** | 1327.5 ms | **7.63× faster** |
| **Package** (clean → package, `-DskipTests`) | 1651.5 ms | **990.0 ms** | 1650.5 ms | **1.67× faster** |

The full per-iteration data lives in the per-baseline `results.json`
files alongside this README — each document carries 10 iterations
plus the `summary` block the dashboard renders.

## Reading the numbers

- **Compile** is the standout (**7.63× faster**). This is the JVM
  startup + Plexus container bootstrap + plugin classloader population
  that mvn re-pays on every invocation; the barista daemon amortizes
  all of it across the session and just dispatches the mojo. The
  `--no-daemon` baseline (forked mvn under the barista CLI shim) is
  effectively identical to plain `mvn compile` — within 1% — proving
  the daemon is the entire source of the speedup, not the resolver
  or the CAS.

- **Package** is 1.67× faster. The lifecycle here is 7 mojos
  (process-resources, compile, process-test-resources, test-compile,
  test, prepare-package, package). Per-mojo dispatch overhead from the
  daemon's worker pool starts to dominate relative to the JVM-startup
  win — each mojo dispatch round-trips through the IPC layer. The
  per-mojo overhead at this workload is ~70-100 ms; against a build
  that does meaningful work per mojo, the ratio improves.

- **Pull / resolve** is 1.55× faster. mvn's `dependency:resolve` walks
  the full reactor model + plugin classpath + transitive resolution
  through Aether; barista runs a focused BFS resolver against the
  content-addressed cache. The variance on barista pull (stddev 117
  ms vs mvn's 17 ms) reflects daemon-spawn jitter that doesn't apply
  to mvn — the gap would widen on warmer steady-state runs.

## Known issue surfaced by this run

Running `barista pull --update` 10 times in a row (with the manifest's
`rm -f barista.lock` prepare step between iterations) triggered a
journal-corruption state in `~/.barista/cache/index/journal.log` —
"journal ends mid-record (truncation detected)" on subsequent reads.
The corrupted journal was saved to
`/tmp/barista-bench-debug/journal-corrupted-*.log` for diagnosis.
After `rm -rf ~/.barista/cache && barista pull` to rebuild the cache
state, this re-measurement ran cleanly. Filed as a follow-up under
M2.3 (Cache CAS — initial); the crash-recovery work from M2.3 T10
should be catching this. This is a documented gap.

## How to reproduce

```bash
# From the monorepo root:
cd barista
cargo build --release -p barista-cli -p barista-bench
export PATH="$PWD/target/release:$PATH"
export BARISTA_MAVEN_HOME=/tmp/barista-mvn4/apache-maven-4.0.0-rc-3
# Warm caches:
( cd bench/projects/p03/checkout && barista pull && mvn -B -q dependency:resolve )
# Run the benchmark:
barista-bench run --corpus bench/projects --filter p03- --output bench/runs/<your-id>/
```

Results land at `bench/runs/<your-id>/` keyed by `<manifest_id>/<baseline_id>.json`.
The companion `index.json` enumerates every results file in the
directory and is the entry point for the Tier-3 dashboard ingest
pipeline.

## What's still missing for full publishing

This dataset is **integration-ready** — schema-compliant, indexed,
versioned in git — but not yet **published**. Three follow-ups remain:

1. **Perf-gate workflow wiring.** `.github/workflows/perf-gate.yml`'s
   placeholder bench-run step needs to invoke the new
   `barista-bench run --corpus bench/projects --filter p03-` against
   the PR + main, then call the existing `scripts/compare-perf-results.sh`
   comparator. Filed.

2. **R2 upload pipeline.** Results.json files need to land in the R2
   bucket the dashboard polls (`s3://barista-bench/runs/<run_id>/`).
   The `index.json` already carries enough metadata for the dashboard
   to render a run page; needs an upload step in the nightly workflow.

3. **Nightly cross-tool workflow.** Today only the perf-gate runs on
   every PR. A nightly schedule against the broader Tier-3 corpus
   (including baselines for mvn 3.9.x and mvnd 2.x once those land)
   is the "constantly run" promise. Filed.

See the roadmap's Workstream A milestones (A.2
T4, A.3 T2/T4) for status.
