# Walkthrough cold-cache dataset — 2026-05-18

Cold-cache capture pass against the P03 Spring Boot starter-web
target, with both barista and mvn captured. The "calls to Maven
Central" comparison is the deterministic headline metric — wall
times vary meaningfully with upstream response time but the
call-count + byte-count shape doesn't.

## Headline finding

| Tool | Wall time | Upstream calls | Bytes |
|---|---:|---:|---:|
| `barista pull --update` | **21.2 s** | **438** | **21.16 MB** |
| `mvn -B -q dependency:resolve` | **28.7 s** | **514** | **39.73 MB** |

**barista fetches less from Maven Central:**

| | barista vs mvn |
|---|---:|
| Upstream calls | **14.8% fewer** (438 vs 514) |
| Bytes downloaded | **46.7% less** (21.16 vs 39.73 MB) |
| Wall time (this run) | **1.35× faster** (21.2 s vs 28.7 s) |

The 76-call gap is mvn's plugin-descriptor fetches (Maven Resolver
hydrates the plugin classpath POM-tree even for a deps-only
command). barista's pull-only resolver skips this because no
plugin execution is involved. The 18.6 MB byte-gap is dominated by
plugin jars + transitive plugin POMs.

## Why N=1 (and not the multi-iteration data the spacing was built for)

The harness now supports `iteration_spacing_seconds` on the
manifest — set to 60s for this manifest — which sleeps between
iterations so a sequence of cold pulls doesn't trip Maven Central's
HTTP 429 rate-limiter. The spacing works as designed (no 429s in
the multi-iteration test run).

BUT: the multi-iteration run surfaced a separate bug: `iter 0`
correctly went through the mitmproxy (438 calls captured), `iter 1`
and `iter 2` made **0 calls** despite each iteration getting a
fresh `BARISTA_PATHS__CACHE_DIR` tempdir. Examining the iter-1
tempdir: 146 objects materialized (20 MB), byte-identical to the
host `~/.barista/cache` contents but with different inodes —
meaning barista's resolver READ from the host cache despite the
env override and COPIED the artifacts into the new tempdir.

The env override appears to be **honored for writes** (iter 0's
cache gets populated correctly) but **partially fall-through-on
reads** (iter 1+ find data at some path the override doesn't
control). Manual reproduction with the same env vars from a fresh
shell DOES route through mitmproxy correctly — so the leak is
specific to the bench-harness invocation pattern. Filed as a
follow-up task; while it's open, cold-cache manifests use
`iterations = 1` so the data we publish is honest.

## Configuration

| | |
|---|---|
| Workload | `bench/projects/p03/checkout/` (vendored Spring Boot 3.3.5 starter-web; ~170 transitive deps) |
| Cache state | **Cold per iteration** — `cache_isolation = "per-iteration"` |
| Iterations | 1 (single clean measurement; multi-iter blocked on the cache-leak bug) |
| Iteration spacing | 60 s declared, inactive at N=1 (skip-before-first / skip-after-last) |
| Hardware | Apple M4 Max, 16 logical cores, 128 GB RAM, macOS 26.2 |
| JDK | Temurin 21.0.4+7.0.LTS |

## How "cold" is enforced

The harness allocates a fresh tempdir per measured iteration and
routes both tools at it:

- **barista**: `BARISTA_PATHS__CACHE_DIR=<tempdir>/barista` (fresh
  CAS) **and** `BARISTA_PATHS__M2_REPOSITORY=<tempdir>/m2` (fresh
  fallback). The `BARISTA_TEST_UPSTREAM_URL` env var routes the
  resolver at mitmdump's **reverse-proxy** on `localhost:PORT`
  (plain HTTP; no CA install needed).
- **mvn**: a one-shot `settings.xml` is written per iteration with
  a `<proxies>` block pointing at `localhost:PORT`, and the mvn
  command line gets `--settings <path>` injected. mvn Resolver
  deliberately **ignores** `-Dhttps.proxyHost` (M B.1 T3 finding),
  so the settings.xml route is the only reliable mvn-side proxy
  wiring. mitmdump runs in **forward-proxy** mode here, doing
  TLS-MITM via the CA already in the JDK truststore.

All env vars and the settings.xml use absolute paths so the
subprocess CWD change doesn't misroute.

## Wall-clock varies with Maven Central's response time

Multiple cold runs against Maven Central from the same machine
within an hour produced wall times spanning ~17-30 seconds for
barista and ~26-30 seconds for mvn. The **call-count + byte-count
comparison is robust** across these runs — those numbers are
deterministic given a fixed workload + dependency graph. Wall
times are reported but should be read as "one run on this
network" rather than "stable measurement of the tool".

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
  --output bench/runs/<your-id>/
```

Requires `mitmdump` on `$PATH` (Homebrew: `brew install
mitmproxy`) and mitmproxy's CA cert in the JDK truststore for the
mvn forward-proxy capture (one-time `keytool -importcert` step
documented in `crates/barista-netcap/README.md`). Network
connection to Maven Central. Expect ~60 MB of upstream traffic per
iteration (20 MB barista + 40 MB mvn).
