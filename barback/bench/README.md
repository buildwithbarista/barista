# barback-bench

JMH (Java Microbenchmark Harness) module for the `barback` worker daemon.

This module is kept separate from the daemon's main Maven module so JMH and its
annotation processor stay off the daemon's runtime classpath.

## Running

From the `barback/` directory:

```sh
mvn -f bench/pom.xml package
java -jar bench/target/barback-bench.jar
```

The shaded `barback-bench.jar` is a self-contained uber-JAR whose main class is
`org.openjdk.jmh.Main` — pass any standard JMH CLI options after it (e.g.
`-h` for help, `-l` to list benchmarks, a regex to filter).

## Status

Real benchmarks are added per-feature as the daemon takes shape. The
`PlaceholderBench` class exists only to prove the harness compiles and the JMH
annotation processor is wired up correctly.
