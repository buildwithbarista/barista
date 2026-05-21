# Walkthrough capture-pass dataset — 2026-05-18

Companion to [`../walkthrough-2026-05-18-postfix/`](../walkthrough-2026-05-18-postfix/).
Same workload (P03 Spring Boot starter-web, warm caches, Apple M4 Max),
same hardware, same iteration count — but emitted from the new
`barista-bench run --capture` mode that routes each barista subprocess
through a per-iteration `mitmdump` reverse-proxy and parses the
resulting HAR to populate `network_calls` + `network_bytes` per the
v1 schema.

## Read this dataset alongside the timing-pass dataset

Wall-clock under `--capture` is mitmproxy-instrumented:

| baseline | timing-pass (postfix) median | capture-pass median | delta |
|---|---:|---:|---:|
| `barista pull --update` | 740 ms | 745 ms | +0.7% |
| `barista compile` (warm daemon) | 224.5 ms | 250 ms | +11% |
| `barista package -DskipTests` | 1044.5 ms | 1123 ms | +7.5% |

The proxy overhead is small in this dataset because most iterations
make **zero requests** (everything's cached locally). On a cold-cache
workload the overhead would be larger.

## The headline finding: warm-cache barista = zero upstream calls

| Step | `barista` network_calls | `barista` network_bytes |
|---|---:|---:|
| `barista pull --update` | **0** | **0 B** |
| `barista compile` (warm daemon) | **0** | **0 B** |
| `barista package -DskipTests` | **0** | **0 B** |

Every byte the build needs comes from the local content-addressed
cache + lockfile. This is the lockfile-fast-path + CAS working as
designed: once a project's deps are cached, subsequent builds are
fully offline.

For `mvn` baselines on the same workload, the harness reports
`network_calls = null` (not zero) — `--capture` doesn't yet wire mvn
through the proxy. The settings.xml `<proxies>` route used by the
existing `scripts/run-baseline-captures.sh` is the path; integrating
that into the bench-harness loop is a filed follow-up.

## What's actually captured

Per `(manifest, baseline)` pair we have:

- `<manifest_id>/<baseline_id>.json` — the v1 results document.
  Same shape as the postfix dataset, but each iteration's
  `network_calls` + `network_bytes` are populated (zero in this
  warm-cache scenario).
- `<manifest_id>/<baseline_id>-capture/iter-N.har` — the raw HAR
  emitted by mitmdump for measured iteration N. **Gitignored**
  because real-workload HARs can be ~80 MB each; for the warm-cache
  workload they are ~4 KB each because no requests are captured.
  Retain locally for forensic analysis; the parsed counts are what
  publish.

## Limitations of this dataset

1. **Warm-cache only.** The interesting comparison ("how many calls
   does barista make to Maven Central vs mvn on a cold project?") is
   a cold-cache benchmark — a separate manifest with a prepare step
   that clears `~/.barista/cache` per iteration. Filed as a
   follow-up; not in this dataset.
2. **mvn / mvnd not captured by the harness.** Those baselines run
   normally (timing-pass numbers populated; network fields left
   `null`). The M B.1 T3 baseline-captures workflow exists for
   per-tool mvn/mvnd captures via settings.xml proxy wiring; that
   path is operator-driven, not harness-driven.
3. **Single workload.** Same P03 caveat as the postfix dataset.

## Reproducing

```bash
# From the repository root:
cd barista
cargo build --release -p barista-cli -p barista-bench
export PATH="$PWD/target/release:$PATH"
export BARISTA_MAVEN_HOME=/tmp/barista-mvn4/apache-maven-4.0.0-rc-3

# Warm caches first (single timing pass also works):
( cd bench/projects/p03/checkout && barista pull )

# Capture pass:
barista-bench run \
  --corpus bench/projects \
  --filter p03- \
  --capture \
  --iterations 5 \
  --warmup-iterations 1 \
  --output bench/runs/<your-id>/
```

Requires `mitmdump` on `$PATH` (Homebrew: `brew install mitmproxy`).
No CA cert install needed because the harness uses mitmproxy's
**reverse-proxy** mode, not the HTTPS-MITM forward-proxy mode — the
barista subprocess talks plain HTTP to `localhost:<port>` and
mitmdump proxies upstream over HTTPS using its built-in trust store.
