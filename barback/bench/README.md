# barback-bench

JMH (Java Microbenchmark Harness) module for the `barback` worker
daemon. The module is kept separate from the daemon's main Maven
module so JMH and its annotation processor stay off the daemon's
runtime classpath.

## Benches

| Class | Mode | What it measures |
|---|---|---|
| `ColdStartBench` | `SingleShotTime`, ms | Wall-clock from a fresh `EmbeddedMavenFactory.discover(Set.of())` through the first `execute(...)` on a 1-module sample project. New factory + new invoker every iteration via `@Setup(Level.Invocation)`. |
| `PluginCacheHitBench` | `AverageTime`, ns | Steady-state `PluginCache.loadOrBuild` hit-path cost across a pre-warmed 5-plugin manifest. |
| `PluginCacheMissBench` | `AverageTime`, µs | Miss-path cost across the same 5-plugin manifest, with the cache override list inflated to force a fresh `URLClassLoader` + Mojo-class linkage on every call. |
| `WorkerPoolBench` | `AverageTime`, µs | `WorkerPool` submit-and-await latency for a 64-task batch, swept over `{VT, TPE}` backends and pinned `workers=8`. The `VT` backend requires JDK 21+; the `TPE` backend runs on any JDK. |

`ColdStartBench` and the `PluginCache*` benches require additional
fixtures:

- **Maven 4 distribution** — `ColdStartBench` resolves a distribution
  via `-Dbarista.maven.home=<path>` (highest precedence), the
  `BARISTA_MAVEN_HOME` environment variable, or a staged
  `barback/spike/m40-t2/apache-maven-4.0.0-rc-3/` left behind by
  `barback/spike/m40-t2/run.sh`. The bench `@Setup` throws with a
  clear remediation message if none of these resolves.
- **Sample project** — `ColdStartBench` copies
  `barback/spike/m40-t2/sample-project/` into a per-trial temp
  directory.
- **Plugin manifest** — `PluginCacheHitBench` and `PluginCacheMissBench`
  read five standard Maven plugin JARs from
  `~/.m2/repository/org/apache/maven/plugins/`. Populate the local
  repository (e.g. run `barback/spike/m40-t1/run.sh` once, or any
  project that exercises `maven-resources-plugin`,
  `maven-compiler-plugin`, `maven-surefire-plugin`, `maven-jar-plugin`,
  `maven-install-plugin`) before invoking the bench.

## Building

The bench harness must compile cleanly under both JDK 17 and JDK 21
(see "JDK matrix" below). The shaded uber-JAR's main class is
`org.openjdk.jmh.Main`.

From the `barback/` directory:

```sh
mvn -f bench/pom.xml package
```

Output: `barback/bench/target/barback-bench.jar`.

## Listing benches

The mechanical proof that all four benches are wired into the harness
is the JMH `-lp` listing on the shaded JAR:

```sh
java -jar barback/bench/target/barback-bench.jar -lp
```

Expected output:

```
com.bluminal.barista.barback.bench.ColdStartBench.coldStart
com.bluminal.barista.barback.bench.PluginCacheHitBench.cacheHit
com.bluminal.barista.barback.bench.PluginCacheMissBench.cacheMiss
com.bluminal.barista.barback.bench.WorkerPoolBench.submitAndAwait
```

## Running

Canonical run command, recording JSON results under a JDK-tagged file
name for the dashboard:

```sh
mvn -f barback/bench/pom.xml package
java -jar barback/bench/target/barback-bench.jar \
    -prof gc \
    -rf json \
    -rff barback/bench/results-jdk21.json
```

Repeat under each JDK you want recorded, swapping the suffix on
`-rff` (e.g. `results-jdk17.json`) so the per-JDK files can be
compared side-by-side by the bench dashboard ingestion. The `-prof
gc` profiler records the allocation cost alongside wall-clock, which
the dashboard surfaces as a secondary signal for any bench that
shows a regression.

Filter to a single bench by regex:

```sh
java -jar barback/bench/target/barback-bench.jar 'PluginCache.*'
```

## JDK matrix

Every recorded number must exist for both JDK 17 and JDK 21 so the
dashboard surfaces fallback-path overhead alongside the virtual-thread
path. The matrix exists because `WorkerPool` runtime-branches on
`Runtime.version().feature() >= 21` &mdash; numbers recorded only
under JDK 21 would hide a regression in the JDK-17 platform-thread
fallback path:

- **CI** — the existing `barback` matrix job
  (`.github/workflows/ci.yml`) already runs
  `mvn -f barback/bench/pom.xml package -DskipTests` in the JDK 17
  and JDK 21 cells. That gates the shaded JAR's *compile* under both
  JDKs on every PR. Benches are not *run* in CI &mdash; the
  measurement cost is too high for per-PR feedback.
- **Real runs** — happen on the operator-provisioned dashboard
  hardware (R-Bench-1 / R-Bench-2 per the cross-cutting workstream
  A.1 T3 plan). The run command above is the canonical invocation
  those runners use; results land in
  `barback/bench/results-jdk{17,21}.json` and the dashboard
  ingestion script consumes them from there.

## Why fixtures are replicated, not imported

The integration-test fixture
(`MavenDistributionFixture` under `barback/src/test/java/`) does the
same job as the `MavenHome` and project-staging helpers in this
module. We replicate the minimum (env-var + system-property lookup
and a directory-shape check) rather than declaring a `test-jar`
dependency on the parent &mdash; that would drag JUnit + AssertJ +
the entire barback test surface onto the shaded JMH uber-JAR,
inflating its size by an order of magnitude and risking version
drift between the fixture and the production code under benchmark.
See `bench/util/MavenHome.java` and `ColdStartBench#stageSampleProject`
for the replicated logic.
