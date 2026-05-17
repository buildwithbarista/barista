/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.bench;

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;

import com.bluminal.barista.barback.workers.WorkerPool;

import org.openjdk.jmh.annotations.Benchmark;
import org.openjdk.jmh.annotations.BenchmarkMode;
import org.openjdk.jmh.annotations.Fork;
import org.openjdk.jmh.annotations.Level;
import org.openjdk.jmh.annotations.Measurement;
import org.openjdk.jmh.annotations.Mode;
import org.openjdk.jmh.annotations.OutputTimeUnit;
import org.openjdk.jmh.annotations.Param;
import org.openjdk.jmh.annotations.Scope;
import org.openjdk.jmh.annotations.Setup;
import org.openjdk.jmh.annotations.State;
import org.openjdk.jmh.annotations.TearDown;
import org.openjdk.jmh.annotations.Warmup;
import org.openjdk.jmh.infra.Blackhole;

/**
 * JMH bench: {@link WorkerPool} backend comparison &mdash; virtual
 * threads (JDK 21+) vs bounded {@link java.util.concurrent.ThreadPoolExecutor}
 * (JDK 17 fallback) under a fixed batch-of-{@value #BATCH} trivial-task
 * submit-and-await workload.
 *
 * <h2>What this measures</h2>
 *
 * <p>The "trivial task" is a single-field accumulate &mdash; small
 * enough that the bench numbers reflect <em>scheduling overhead</em>,
 * not user-code cost. Every {@code @Benchmark} call submits
 * {@value #BATCH} {@link Callable}s through {@link WorkerPool#submit}
 * and waits for all of them. Per-iteration cost ≈ batch fan-out +
 * thread bring-up (cached) + concurrency-budget contention + result
 * collection.
 *
 * <h2>Backend selection</h2>
 *
 * <p>{@link Backend#VT} drives {@link WorkerPool#newVirtualThreadExecutor()}.
 * {@link Backend#TPE} drives {@link WorkerPool#newPlatformThreadPool(int)}.
 * Both are wrapped via {@link WorkerPool#createWith(ExecutorService, int)}
 * so the concurrency-budget semaphore is honoured on both branches
 * (matching the production code). The runtime-version check in
 * {@link WorkerPool#create(int)} is deliberately bypassed because
 * comparing both backends head-to-head on the same JVM is the whole
 * point of this bench. The VT param is skipped via an exception in
 * {@link #setUp()} on JDK 17 &mdash; JMH will surface the bench as
 * failing for that param on that JVM, which is the correct signal:
 * "this backend does not exist here".
 *
 * <h2>JDK 17 vs JDK 21</h2>
 *
 * <p>This is the bench the JDK matrix calls out by name. On JDK 21, both
 * params run and the dashboard sees the head-to-head ratio. On JDK 17,
 * only TPE runs and the dashboard sees the absolute fallback cost;
 * comparing it against JDK 21's TPE measurement on the same hardware
 * isolates "JDK-version effect on the same backend" from "VT-vs-TPE
 * effect".
 */
@State(Scope.Benchmark)
@BenchmarkMode(Mode.AverageTime)
@OutputTimeUnit(TimeUnit.MICROSECONDS)
@Fork(1)
@Warmup(iterations = 3, time = 1)
@Measurement(iterations = 5, time = 2)
public class WorkerPoolBench {

    /** Number of tasks submitted per {@code @Benchmark} call. */
    public static final int BATCH = 64;

    /**
     * Which backend to drive. {@link Backend#VT} requires JDK 21+
     * &mdash; on older JVMs the {@code @Setup} call throws and JMH
     * surfaces the bench as failing for that parameterisation.
     */
    public enum Backend {
        /** Virtual threads. Requires {@code Runtime.version().feature() >= 21}. */
        VT,
        /** Platform-thread {@link java.util.concurrent.ThreadPoolExecutor}. */
        TPE
    }

    /**
     * Sweep both backends so the dashboard can graph VT vs TPE on the
     * same JVM in one bench run. JMH expands this into two
     * {@code @Benchmark} runs per fork.
     */
    @Param({"VT", "TPE"})
    public Backend backend;

    /**
     * Concurrency budget. Defaults to the daemon's {@code 1C} sizing
     * model at 8 cores &mdash; the typical 2026 developer laptop.
     * Smaller pools amplify the contention-on-the-semaphore cost (a
     * useful sensitivity dimension to record but out of scope for
     * v0.1 dashboard).
     */
    @Param({"8"})
    public int workers;

    private WorkerPool pool;
    private List<Callable<Integer>> tasks;

    @Setup(Level.Trial)
    public void setUp() {
        ExecutorService exec = switch (backend) {
            case VT -> {
                if (Runtime.version().feature() < 21) {
                    throw new IllegalStateException(
                            "WorkerPoolBench[VT] requires JDK 21+; running on JDK "
                                    + Runtime.version().feature()
                                    + ". Re-run with the bench's JDK-21 cell to "
                                    + "exercise the virtual-thread backend.");
                }
                yield WorkerPool.newVirtualThreadExecutor();
            }
            case TPE -> WorkerPool.newPlatformThreadPool(workers);
        };
        this.pool = WorkerPool.createWith(exec, workers);

        // Pre-allocate the task list so submission cost in the timed
        // section doesn't include object construction. The task body
        // is a single integer accumulation — small enough that thread
        // scheduling dominates, large enough to defeat constant
        // folding (the value depends on `i`).
        this.tasks = new ArrayList<>(BATCH);
        for (int i = 0; i < BATCH; i++) {
            final int idx = i;
            tasks.add(() -> idx ^ (idx << 1));
        }
    }

    @TearDown(Level.Trial)
    public void tearDown() {
        pool.close();
    }

    /**
     * Submit a {@value #BATCH}-element batch and await every result.
     * The aggregated XOR is consumed via {@link Blackhole} so JMH
     * cannot dead-code-eliminate the result collection.
     */
    @Benchmark
    public int submitAndAwait(Blackhole bh) throws InterruptedException, ExecutionException {
        List<Future<Integer>> futures = new ArrayList<>(BATCH);
        for (Callable<Integer> t : tasks) {
            futures.add(pool.submit(t));
        }
        int acc = 0;
        for (Future<Integer> f : futures) {
            acc ^= f.get();
        }
        bh.consume(acc);
        return acc;
    }
}
