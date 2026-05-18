# Walkthrough cold-cache dataset — 2026-05-18

First **cold-cache** capture pass against the P03 Spring Boot
starter-web target. Companion to the warm-cache walkthroughs at
[`../walkthrough-2026-05-18-postfix/`](../walkthrough-2026-05-18-postfix/)
(timing-only) and [`../walkthrough-2026-05-18-capture/`](../walkthrough-2026-05-18-capture/)
(warm-cache + capture, network_calls = 0 because everything is local).

This is the first dataset where the "calls to Maven Central"
comparison is a real measurement and not a sanity-check that the
lockfile fast-path stayed local.

## Headline finding

| Tool | Wall time | Upstream calls | Bytes |
|---|---:|---:|---:|
| `barista pull --update` | **17.9 s** | **438** | **21.2 MB** |
| `mvn -B -q dependency:resolve` | **30.1 s** | — (not captured) | — |

barista is **1.68× faster on a cold-cache resolve** of this
~170-transitive-dep Spring Boot project, with a network-call shape
the harness now captures: 438 distinct HTTP requests to Maven
Central, ~21 MB of artifacts.

`mvn` capture isn't yet wired through the bench harness (the env-var
hook used for barista — `BARISTA_TEST_UPSTREAM_URL` — is
barista-specific; mvn needs a settings.xml `<proxies>` wiring).
Filed as a follow-up — once it lands, the "calls" column gets a
direct mvn baseline alongside.

## Configuration

| | |
|---|---|
| Workload | `bench/projects/p03/checkout/` (vendored Spring Boot 3.3.5 starter-web; ~170 transitive deps) |
| Cache state | **Cold per iteration** — `cache_isolation = "per-iteration"` in the manifest |
| Iterations | 1 (Maven Central rate-limits repeated rapid cold-pulls with HTTP 429 — see below) |
| Reference mvn | Apache Maven 3.9.9 (`mvn -B -q dependency:resolve`) |
| Hardware | Apple M4 Max, 16 logical cores, 128 GB RAM, macOS 26.2 |
| JDK | Temurin 21.0.4+7.0.LTS |

## How "cold" is enforced

The new manifest field `cache_isolation = "per-iteration"` tells the
harness to allocate a fresh tempdir per measured iteration and
route both tools at it:

- barista: `BARISTA_PATHS__CACHE_DIR=<tempdir>/barista` (fresh CAS)
  **and** `BARISTA_PATHS__M2_REPOSITORY=<tempdir>/m2` (fresh fallback;
  without this barista hardlinks from the user's `~/.m2` and makes
  zero calls).
- mvn: `MAVEN_OPTS=-Dmaven.repo.local=<tempdir>/m2` (fresh local repo).

All three env vars use absolute paths so the subprocess's CWD
change (into the project checkout) doesn't misroute. Iteration
caches land at
`bench/runs/<run_id>/cold-caches/<manifest>/<baseline>/iter-N/`
and are retained for forensic inspection (gitignored).

## Maven Central rate-limited us

A prior 3-iteration run triggered HTTP 429 responses on iterations
2 and 3 of the barista cold-pull cell:

```
error: barista pull failed: ...transport error... HTTP 429 fetching
       http://localhost:.../spring-boot-starter-parent-3.3.5.pom
```

This is a real-world finding: barista's parallel resolver is fast
enough that three back-to-back cold pulls of a 438-request workload
cross Maven Central's rate-limit threshold. mvn's serial fetch
takes ~30 s per iteration which keeps it under the throttle.

For canonical numbers, the right answer is to either (a) run from
a local mirror to bypass the rate limit, or (b) add a back-off
between cold iterations. This dataset uses single-iteration runs
(N=1, so no median / p95 / stddev) to sidestep the issue. A
follow-up will add iteration-spacing to the harness.

## What the harness change adds

`crates/barista-bench/src/manifest.rs`:

- New `CacheIsolation` enum (`None` | `PerIteration`).
- New `Manifest::cache_isolation` field (defaults to `None` so the
  warm-cache manifests stay unchanged).

`crates/barista-bench/src/bin/barista-bench.rs`:

- `cold_cache_env()` helper that materialises per-iteration
  tempdirs and returns the three env vars.
- `measure_baseline()` + `measure_baseline_with_capture()` now
  accept an `Option<&Path>` cache-root-base argument.
- `summarize()` filters out failed iterations (exit_code ≠ 0) when
  computing medians, so rate-limit failures don't pollute the
  summary stats. Falls back to the full set when literally every
  iteration failed.

## Reproducing

```bash
# From the monorepo root:
cd barista
cargo build --release -p barista-cli -p barista-bench
export PATH="$PWD/target/release:$PATH"

barista-bench run \
  --corpus bench/projects \
  --filter p03-pull-cold \
  --capture \
  --iterations 1 \
  --warmup-iterations 0 \
  --output bench/runs/<your-id>/
```

Requires `mitmdump` on `$PATH` (Homebrew: `brew install mitmproxy`)
and a network connection to Maven Central. Expect ~20 MB of
upstream traffic per cold iteration.
