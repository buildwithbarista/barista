# Walkthrough cold-cache dataset — 2026-05-18

Cold-cache capture pass against the P03 Spring Boot starter-web
target. Companion to the warm-cache walkthroughs at
[`../walkthrough-2026-05-18-postfix/`](../walkthrough-2026-05-18-postfix/)
(timing-only) and [`../walkthrough-2026-05-18-capture/`](../walkthrough-2026-05-18-capture/)
(warm-cache + capture, network_calls = 0 because everything is local).

This dataset is the answer to "how many calls to Maven Central
does each tool make?" — the deterministic part of cold-cache
benchmarks, captured for both barista and mvn.

## Headline finding

| Tool | Wall time | Upstream calls | Bytes |
|---|---:|---:|---:|
| `barista pull --update` | **26.4 s** | **438** | **21.16 MB** |
| `mvn -B -q dependency:resolve` | **27.1 s** | **514** | **39.73 MB** |

**barista fetches less from Maven Central.** Same workload, same
~170-dep dependency closure, but barista emits 76 fewer HTTP
requests and downloads 18.6 MB less than mvn:

| | barista vs mvn |
|---|---:|
| Upstream calls | **14.8% fewer** |
| Bytes downloaded | **46.7% less** |

The wall-clock numbers are within ±3% of each other on this
particular run; both tools spent most of their time waiting on
Maven Central. **Wall-clock varies meaningfully with Maven
Central's response time across runs** (a separate run earlier in
the session saw barista at 17.9 s vs mvn at 30.1 s, a 1.68× delta)
— only the call-count + byte-count comparison is robust across
network conditions on the same workload.

## Where the 76-call gap comes from

mvn's resolver fetches more because it pulls in:

- **Maven plugin descriptors** (`maven-metadata.xml`, plugin `.pom`
  files for every default-lifecycle plugin even when those plugins
  aren't executed by `dependency:resolve`).
- **Plugin transitive dependencies** (the resolver hydrates each
  plugin's classpath POM-tree even for a deps-only command).
- **Extra metadata refresh checks** Maven Resolver issues to honor
  its `<updatePolicy>` settings.

barista's resolver does dependency-graph traversal only and skips
the plugin-descriptor fetches because no plugin execution is
involved in `barista pull`. The 76-request difference is the
plugin-tooling tax mvn pays on every cold resolve. The
46.7%-fewer-bytes finding is dominated by mvn fetching plugin
artifacts (jars + transitive jars), which are typically larger
than the pure dependency-resolution artifacts (POMs + the actual
declared jars).

## Configuration

| | |
|---|---|
| Workload | `bench/projects/p03/checkout/` (vendored Spring Boot 3.3.5 starter-web; ~170 transitive deps) |
| Cache state | **Cold per iteration** — `cache_isolation = "per-iteration"` in the manifest |
| Iterations | 1 (Maven Central rate-limits repeated rapid cold-pulls with HTTP 429) |
| Hardware | Apple M4 Max, 16 logical cores, 128 GB RAM, macOS 26.2 |
| JDK | Temurin 21.0.4+7.0.LTS |

## How "cold" is enforced

The manifest field `cache_isolation = "per-iteration"` tells the
harness to allocate a fresh tempdir per measured iteration and
route both tools at it:

- **barista**: `BARISTA_PATHS__CACHE_DIR=<tempdir>/barista` (fresh
  CAS) **and** `BARISTA_PATHS__M2_REPOSITORY=<tempdir>/m2` (fresh
  fallback; without this barista hardlinks from the user's `~/.m2`
  and makes zero calls). Plus
  `BARISTA_TEST_UPSTREAM_URL=http://localhost:PORT/maven2` to route
  the resolver at mitmdump's reverse-proxy.
- **mvn**: a one-shot `settings.xml` is written per iteration with
  a `<proxies>` block pointing at `localhost:PORT`, and the mvn
  command line gets `--settings <path>` injected. mvn Resolver
  deliberately **ignores** `-Dhttps.proxyHost` (architectural
  finding from M B.1 T3), so the settings.xml `<proxies>` route is
  the only reliable mvn-side proxy wiring.

All three env vars and the settings.xml path use absolute paths
so the subprocess's CWD change (into the project checkout)
doesn't misroute.

mitmproxy runs in **reverse-proxy mode** for barista (plain HTTP
to localhost; no CA cert install) and **forward-proxy mode** for
mvn (HTTPS-MITM via the mitmproxy CA already in the JDK
truststore, installed in the M B.1 T3 capture-setup). The harness
picks the right mode per baseline via `baseline_capture_kind()`.

## Maven Central rate-limit caveat

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
and mitmproxy's CA cert in the JDK truststore for the mvn forward-proxy
capture (one-time `keytool -importcert` step documented in
`crates/barista-netcap/README.md`). Network connection to Maven
Central. Expect ~60 MB of upstream traffic per iteration (20 MB
barista + 40 MB mvn).
