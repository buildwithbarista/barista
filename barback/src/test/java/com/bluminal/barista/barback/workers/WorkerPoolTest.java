// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.workers;

import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledForJreRange;
import org.junit.jupiter.api.condition.JRE;

import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Future;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicInteger;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Unit tests for {@link WorkerPool}.
 *
 * <h2>Both branches under a single JVM</h2>
 *
 * <p>The {@code [T]} acceptance criterion for milestone 4.2 Task 2
 * requires both the JDK 21+ virtual-thread path and the JDK 17
 * platform-thread {@link ThreadPoolExecutor} path to be exercised. We
 * achieve that under any active JVM via two complementary mechanisms:
 *
 * <ul>
 *   <li>The {@link WorkerPool#create(int)} factory picks a backend
 *       based on {@link Runtime#version()}. The CI matrix (see
 *       {@code .github/workflows/ci.yml} job {@code barback}) runs the
 *       full test suite under JDK 17 and JDK 21 cells &mdash; that's
 *       what makes "identical outputs on both branches" mechanically
 *       true on real JDKs.</li>
 *   <li>The {@link WorkerPool#createWith(ExecutorService, int)}
 *       injection seam lets <em>this</em> JVM drive either backend
 *       directly. We use it to validate the platform-thread fallback
 *       even on the developer's JDK 21 box (and vice versa on a
 *       JDK 17 box).</li>
 * </ul>
 *
 * <p>{@link #create_factoryPicksRuntimeAppropriateBackend()} verifies
 * the runtime-branch selection on whatever JVM happens to be running
 * the tests; the CI matrix supplies the JDK-17 coverage.
 */
class WorkerPoolTest {

    private static final long ASSERT_TIMEOUT_SECONDS = 10L;

    // ----------------------------------------------------------------
    // Runtime branch selection
    // ----------------------------------------------------------------

    @Test
    @DisplayName("create() picks VIRTUAL_THREADS on JDK 21+")
    @EnabledForJreRange(min = JRE.JAVA_21)
    void create_picksVirtualThreadsOnJdk21Plus() throws Exception {
        try (WorkerPool pool = WorkerPool.create(4)) {
            assertEquals(WorkerPool.Backend.VIRTUAL_THREADS, pool.backend(),
                    "JDK >= 21 must select the virtual-thread backend");
            // Spot-check the executor really is the virtual-thread one
            // by submitting a task that names its own thread.
            Future<String> name = pool.submit(() -> Thread.currentThread().toString());
            String desc = name.get(ASSERT_TIMEOUT_SECONDS, TimeUnit.SECONDS);
            assertTrue(desc.contains("Virtual"),
                    "virtual-thread executor must produce virtual threads; got " + desc);
        }
    }

    @Test
    @DisplayName("create() picks PLATFORM_THREAD_POOL on JDK 17")
    @EnabledForJreRange(max = JRE.JAVA_20)
    void create_picksPlatformThreadPoolOnJdk17() throws Exception {
        try (WorkerPool pool = WorkerPool.create(2)) {
            assertEquals(WorkerPool.Backend.PLATFORM_THREAD_POOL, pool.backend(),
                    "JDK < 21 must select the bounded ThreadPoolExecutor backend");
            assertTrue(pool.executor() instanceof ThreadPoolExecutor,
                    "fallback backend must be a ThreadPoolExecutor");
            ThreadPoolExecutor tpe = (ThreadPoolExecutor) pool.executor();
            assertEquals(2, tpe.getCorePoolSize());
            assertEquals(2, tpe.getMaximumPoolSize());
        }
    }

    @Test
    @DisplayName("create() picks a runtime-appropriate backend")
    void create_factoryPicksRuntimeAppropriateBackend() {
        try (WorkerPool pool = WorkerPool.create(1)) {
            int feature = Runtime.version().feature();
            WorkerPool.Backend expected = feature >= 21
                    ? WorkerPool.Backend.VIRTUAL_THREADS
                    : WorkerPool.Backend.PLATFORM_THREAD_POOL;
            assertEquals(expected, pool.backend(),
                    "runtime branch selection must match Runtime.version().feature() >= 21");
        }
    }

    // ----------------------------------------------------------------
    // Virtual-thread executor (always reachable on JDK 21+)
    // ----------------------------------------------------------------

    @Test
    @DisplayName("virtual-thread executor: N submitted tasks all complete and run on virtual threads")
    @EnabledForJreRange(min = JRE.JAVA_21)
    void virtualThreadExecutor_submitN_allCompleteOnVirtualThreads() throws Exception {
        int n = 32;
        ExecutorService vt = WorkerPool.newVirtualThreadExecutor();
        try (WorkerPool pool = WorkerPool.createWith(vt, n)) {
            Set<String> threadDescriptors = runAndCollectThreadDescriptors(pool, n);

            // Every task must have observed itself running on a virtual thread.
            for (String d : threadDescriptors) {
                assertTrue(d.contains("Virtual"),
                        "every task must run on a virtual thread; got " + d);
            }
            // 32 tasks on virtual threads should occupy 32 distinct
            // worker threads (one per task) since virtual threads are
            // never reused inside `newVirtualThreadPerTaskExecutor`.
            assertEquals(n, threadDescriptors.size(),
                    "newVirtualThreadPerTaskExecutor allocates one virtual thread per task");
        }
    }

    // ----------------------------------------------------------------
    // ThreadPoolExecutor fallback (reachable on any JDK via injection)
    // ----------------------------------------------------------------

    @Test
    @DisplayName("ThreadPoolExecutor fallback: N submitted tasks all complete; pool sized to workers")
    void threadPoolFallback_submitN_allCompleteAndPoolSizeMatchesWorkers() throws Exception {
        int workers = 4;
        int tasks = 16;
        ExecutorService tpe = WorkerPool.newPlatformThreadPool(workers);
        try (WorkerPool pool = WorkerPool.createWith(tpe, workers)) {
            assertEquals(WorkerPool.Backend.INJECTED, pool.backend(),
                    "createWith reports the INJECTED backend");
            assertTrue(pool.executor() instanceof ThreadPoolExecutor);
            ThreadPoolExecutor backing = (ThreadPoolExecutor) pool.executor();
            assertEquals(workers, backing.getMaximumPoolSize(),
                    "ThreadPoolExecutor max-pool size must equal workers");
            assertEquals(workers, backing.getCorePoolSize(),
                    "ThreadPoolExecutor core-pool size must equal workers");

            Set<String> threadDescriptors = runAndCollectThreadDescriptors(pool, tasks);

            // Every task must have run on a platform worker thread.
            for (String d : threadDescriptors) {
                assertTrue(d.contains("barback-worker-"),
                        "every task must run on a named platform worker; got " + d);
                assertFalse(d.contains("Virtual"),
                        "fallback path must not produce virtual threads; got " + d);
            }
            // 16 tasks across 4 workers re-use threads; distinct count
            // is bounded above by the pool size.
            assertTrue(threadDescriptors.size() <= workers,
                    "platform thread pool must reuse threads; saw " + threadDescriptors.size()
                            + " distinct, expected <= " + workers);
        }
    }

    // ----------------------------------------------------------------
    // Identical-output property: both branches return the same answer
    // for the same task batch. This is the AC's "identical outputs on
    // both branches" assertion in unit-test form.
    // ----------------------------------------------------------------

    @Test
    @DisplayName("Both backends produce byte-identical outputs for the same task batch")
    @EnabledForJreRange(min = JRE.JAVA_21)
    void bothBackends_identicalOutputs() throws Exception {
        List<Integer> inputs = new ArrayList<>();
        for (int i = 0; i < 64; i++) {
            inputs.add(i);
        }

        List<Integer> virtualOutputs = runBatchOrdered(
                WorkerPool.createWith(WorkerPool.newVirtualThreadExecutor(), 8), inputs);
        List<Integer> platformOutputs = runBatchOrdered(
                WorkerPool.createWith(WorkerPool.newPlatformThreadPool(8), 8), inputs);

        assertEquals(virtualOutputs, platformOutputs,
                "both backends must compute identical outputs for the same input batch");
    }

    // ----------------------------------------------------------------
    // Concurrency-budget enforcement
    // ----------------------------------------------------------------

    @Test
    @DisplayName("Concurrency budget caps simultaneous tasks at the configured workers value")
    void concurrencyBudget_neverExceedsWorkers() throws Exception {
        int workers = 3;
        int tasks = 30;
        // Use the virtual-thread executor if available (so the
        // executor itself is effectively unbounded and only the
        // semaphore enforces the cap). On JDK 17, fall back to a
        // pool generously larger than `workers` so the underlying
        // pool can't accidentally be the cap.
        ExecutorService backing = Runtime.version().feature() >= 21
                ? WorkerPool.newVirtualThreadExecutor()
                : WorkerPool.newPlatformThreadPool(tasks);
        try (WorkerPool pool = WorkerPool.createWith(backing, workers)) {
            AtomicInteger inFlight = new AtomicInteger();
            AtomicInteger peak = new AtomicInteger();
            CountDownLatch done = new CountDownLatch(tasks);

            for (int i = 0; i < tasks; i++) {
                pool.submit(() -> {
                    int current = inFlight.incrementAndGet();
                    peak.accumulateAndGet(current, Math::max);
                    try {
                        // Hold the slot long enough for siblings to
                        // try and grab their own permit, so a faulty
                        // budget would show.
                        Thread.sleep(50);
                    } catch (InterruptedException e) {
                        Thread.currentThread().interrupt();
                    } finally {
                        inFlight.decrementAndGet();
                        done.countDown();
                    }
                    return null;
                });
            }

            assertTrue(done.await(ASSERT_TIMEOUT_SECONDS, TimeUnit.SECONDS),
                    "all tasks must complete within " + ASSERT_TIMEOUT_SECONDS + "s");
            assertTrue(peak.get() <= workers,
                    "concurrency budget exceeded: peak=" + peak.get()
                            + ", workers=" + workers);
        }
    }

    // ----------------------------------------------------------------
    // Lifecycle: close() drains in-flight work
    // ----------------------------------------------------------------

    @Test
    @DisplayName("close() drains in-flight tasks before returning")
    void close_drainsInFlightTasks() throws Exception {
        AtomicInteger completed = new AtomicInteger();
        WorkerPool pool = WorkerPool.create(2);
        try {
            for (int i = 0; i < 4; i++) {
                pool.submit(() -> {
                    Thread.sleep(50);
                    completed.incrementAndGet();
                    return null;
                });
            }
        } finally {
            pool.close();
        }
        assertEquals(4, completed.get(),
                "close() must wait for submitted tasks to complete");
    }

    @Test
    @DisplayName("close() rejects subsequent submissions")
    void close_rejectsSubsequentSubmissions() throws InterruptedException {
        WorkerPool pool = WorkerPool.create(1);
        pool.close();
        // close() calls shutdown() then awaitTermination(), so the backing
        // executor MUST be in the terminated (or at minimum shutdown) state
        // before close() returns. Asserting these post-conditions before the
        // submit() call makes the rejection deterministic: an ExecutorService
        // that isShutdown() will always reject new submissions, eliminating
        // the race between the shutdown() call and the reject-check that caused
        // sporadic test passes without the expected exception on loaded runners.
        assertTrue(pool.executor().isShutdown(),
                "executor must be shutdown after close()");
        assertTrue(pool.executor().isTerminated()
                        || pool.executor().awaitTermination(
                                WorkerPool.SHUTDOWN_GRACE_SECONDS, TimeUnit.SECONDS),
                "executor must have terminated within the shutdown grace period");
        assertThrows(java.util.concurrent.RejectedExecutionException.class,
                () -> pool.submit(() -> "after-close"));
    }

    // ----------------------------------------------------------------
    // Argument validation
    // ----------------------------------------------------------------

    @Test
    @DisplayName("create(0) and create(negative) are rejected")
    void create_rejectsNonPositiveWorkers() {
        assertThrows(IllegalArgumentException.class, () -> WorkerPool.create(0));
        assertThrows(IllegalArgumentException.class, () -> WorkerPool.create(-1));
    }

    @Test
    @DisplayName("newPlatformThreadPool(0) and newPlatformThreadPool(negative) are rejected")
    void newPlatformThreadPool_rejectsNonPositiveWorkers() {
        assertThrows(IllegalArgumentException.class,
                () -> WorkerPool.newPlatformThreadPool(0));
        assertThrows(IllegalArgumentException.class,
                () -> WorkerPool.newPlatformThreadPool(-1));
    }

    // ----------------------------------------------------------------
    // Test helpers
    // ----------------------------------------------------------------

    /**
     * Submit {@code n} tasks that each record their own thread
     * descriptor. Returns the set of distinct descriptors observed.
     */
    private static Set<String> runAndCollectThreadDescriptors(WorkerPool pool, int n)
            throws InterruptedException, ExecutionException, TimeoutException {
        List<Future<String>> futures = new ArrayList<>(n);
        for (int i = 0; i < n; i++) {
            futures.add(pool.submit(() -> {
                // Small sleep makes the platform-thread pool actually
                // distribute work across multiple workers rather than
                // letting a single hot thread chew through everything.
                Thread.sleep(2);
                return Thread.currentThread().toString();
            }));
        }
        Set<String> descriptors = new HashSet<>();
        for (Future<String> f : futures) {
            descriptors.add(f.get(ASSERT_TIMEOUT_SECONDS, TimeUnit.SECONDS));
        }
        return descriptors;
    }

    /**
     * Run a batch of inputs through {@code pool}'s
     * {@link WorkerPool#submit(java.util.concurrent.Callable)} method
     * and return the outputs in submission order. The computation is
     * deterministic ({@code i &rarr; i * i + 1}) so different backends
     * must produce identical outputs.
     */
    private static List<Integer> runBatchOrdered(WorkerPool pool, List<Integer> inputs)
            throws InterruptedException, ExecutionException, TimeoutException {
        try (pool) {
            List<Future<Integer>> futures = new ArrayList<>(inputs.size());
            for (Integer i : inputs) {
                futures.add(pool.submit(() -> i * i + 1));
            }
            List<Integer> outputs = new ArrayList<>(inputs.size());
            for (Future<Integer> f : futures) {
                outputs.add(f.get(ASSERT_TIMEOUT_SECONDS, TimeUnit.SECONDS));
            }
            return outputs;
        }
    }
}
