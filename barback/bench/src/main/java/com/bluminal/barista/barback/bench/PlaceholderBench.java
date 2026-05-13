package com.bluminal.barista.barback.bench;

import java.util.concurrent.TimeUnit;
import org.openjdk.jmh.annotations.Benchmark;
import org.openjdk.jmh.annotations.BenchmarkMode;
import org.openjdk.jmh.annotations.Mode;
import org.openjdk.jmh.annotations.OutputTimeUnit;
import org.openjdk.jmh.annotations.Scope;
import org.openjdk.jmh.annotations.State;

/**
 * Placeholder benchmark proving the JMH harness compiles end-to-end.
 *
 * <p>Real benchmarks land per-feature as the daemon is implemented in a
 * subsequent release.
 */
@State(Scope.Benchmark)
@BenchmarkMode(Mode.AverageTime)
@OutputTimeUnit(TimeUnit.NANOSECONDS)
public class PlaceholderBench {

    @Benchmark
    public int identity() {
        return 42;
    }
}
