// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.workers;

import java.util.Objects;
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.Semaphore;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import java.util.logging.Level;
import java.util.logging.Logger;

/**
 * Worker pool for the {@code barback} daemon. Wraps an
 * {@link ExecutorService} that dispatches mojo invocations on a fixed
 * concurrency budget &mdash; this is the JVM-side realisation of the
 * {@code barback.workers} configuration knob described in the daemon
 * spec ("number of workers; default {@code 1C} = one per core").
 *
 * <h2>JDK-branch threading model</h2>
 *
 * <p>The pool picks one of two backing executors at construction time
 * based on {@link Runtime#version()}:
 *
 * <ul>
 *   <li><strong>Java 21+:</strong> {@link Executors#newVirtualThreadPerTaskExecutor()}.
 *       Virtual threads have no native stack, so the executor itself
 *       is effectively unbounded. The {@code workers} value is enforced
 *       as a <em>concurrency budget</em> via an internal {@link Semaphore}
 *       so the daemon never has more than {@code workers} actions
 *       running at the same instant. This matches the daemon spec's
 *       "no per-thread native stack &hellip; the worker pool can be sized
 *       large (hundreds) with minimal cost" note while still respecting
 *       the configured budget.</li>
 *   <li><strong>Java 17 fallback:</strong> a bounded
 *       {@link ThreadPoolExecutor} with exactly {@code workers}
 *       platform threads, a bounded {@link LinkedBlockingQueue} of
 *       capacity {@code workers * 4}, and
 *       {@link ThreadPoolExecutor.CallerRunsPolicy}. The bounded queue
 *       makes back-pressure visible &mdash; an unbounded queue would
 *       silently absorb overload as memory growth and tail latency
 *       &mdash; and the caller-runs policy converts overflow into
 *       synchronous execution on the submitter's thread rather than
 *       dropping work. Platform threads consume ~512 KiB native stack
 *       each, so callers on Java 17 are encouraged to size the pool
 *       close to core count (the {@code 1C} default).</li>
 * </ul>
 *
 * <p>The branch is selected exactly once, at construction, by
 * {@code Runtime.version().feature() >= 21}. This mirrors the
 * {@code ThreadFactoryProvider.forCurrentRuntime()} sketch in the
 * daemon spec (§11.2.2) and the runtime branch documented in the
 * "JDK support policy" section: compile target stays {@code --release 17}
 * and Java 21+ features are reached via the JDK-version-agnostic
 * {@link ExecutorService} contract returned by
 * {@code newVirtualThreadPerTaskExecutor}.
 *
 * <h2>Test-injection seam</h2>
 *
 * <p>{@link #createWith(ExecutorService, int)} bypasses the runtime
 * branch and wraps a caller-supplied executor. This is how the test
 * suite exercises <em>both</em> branches under a single active JVM:
 * the virtual-thread path is reachable on JDK 21+ via {@link #create(int)},
 * and the {@code ThreadPoolExecutor} fallback is reachable on any JDK
 * via {@link #createWith(ExecutorService, int) createWith(makeFallback(workers), workers)}.
 * The CI matrix exercises the runtime-branch selection itself by
 * running the full test suite under both JDK 17 and JDK 21 cells per
 * the project's CI policy &mdash; the JDK-17 cell drives the
 * {@code ThreadPoolExecutor} branch through {@link #create(int)} for
 * real.
 *
 * <h2>Lifecycle</h2>
 *
 * <p>{@link WorkerPool} implements {@link AutoCloseable}. {@link #close()}
 * calls {@link ExecutorService#shutdown()} on the backing executor and
 * waits up to {@link #SHUTDOWN_GRACE} for in-flight work to drain
 * before forcing {@link ExecutorService#shutdownNow()}. Tasks queued
 * after {@link #close()} are rejected with
 * {@link RejectedExecutionException}, which the daemon's accept loop
 * already handles by closing the offending connection (see
 * {@code Server#runAcceptLoop}).
 *
 * <h2>JMH compatibility</h2>
 *
 * <p>The constructors and factory methods are public and unbound to
 * any singleton so JMH benches in {@code barback/bench/} can construct
 * pools directly and measure virtual-vs-platform thread overhead under
 * controlled load. No hidden global state.
 */
public final class WorkerPool implements AutoCloseable {

    private static final Logger LOG = Logger.getLogger(WorkerPool.class.getName());

    /**
     * How long {@link #close()} waits for the backing executor to drain
     * before forcing {@link ExecutorService#shutdownNow()}. Aligned
     * with the existing accept-loop drain in {@code Server}.
     */
    static final long SHUTDOWN_GRACE_SECONDS = 5L;
    static final java.time.Duration SHUTDOWN_GRACE =
            java.time.Duration.ofSeconds(SHUTDOWN_GRACE_SECONDS);

    /**
     * Queue-capacity multiplier for the Java-17 bounded queue. A
     * runqueue of {@code workers * 4} gives a 4&times; burst headroom
     * before the {@link ThreadPoolExecutor.CallerRunsPolicy} kicks in
     * and makes overload visible to the submitter. Adjust only with
     * benchmark evidence.
     */
    static final int QUEUE_CAPACITY_FACTOR = 4;

    private final ExecutorService executor;
    private final Semaphore concurrencyBudget;
    private final int workers;
    private final Backend backend;

    /**
     * Identifies which executor implementation a {@link WorkerPool}
     * is wrapping. Surfaced by {@link #backend()} for diagnostics and
     * by the test suite to assert the correct branch was taken.
     */
    public enum Backend {
        /**
         * JDK 21+ virtual threads. The backing executor is
         * {@link Executors#newVirtualThreadPerTaskExecutor()}; the
         * configured {@code workers} value is enforced as a
         * concurrency budget via a {@link Semaphore}.
         */
        VIRTUAL_THREADS,
        /**
         * JDK 17 fallback. The backing executor is a bounded
         * {@link ThreadPoolExecutor} sized to {@code workers} platform
         * threads.
         */
        PLATFORM_THREAD_POOL,
        /**
         * The caller supplied the executor via
         * {@link #createWith(ExecutorService, int)}. The pool does not
         * know which threading model is in use; the concurrency
         * budget is still enforced via the {@link Semaphore}.
         */
        INJECTED
    }

    private WorkerPool(ExecutorService executor, int workers, Backend backend) {
        this.executor = Objects.requireNonNull(executor, "executor");
        if (workers <= 0) {
            throw new IllegalArgumentException(
                    "workers must be >= 1; got " + workers);
        }
        this.workers = workers;
        this.backend = Objects.requireNonNull(backend, "backend");
        // Fair semaphore: tasks are admitted in submission order. This
        // matters when the configured budget is small and submissions
        // burst — without fairness, a single hot caller could starve
        // siblings.
        this.concurrencyBudget = new Semaphore(workers, true);
    }

    /**
     * Build a worker pool sized to {@code workers} concurrent actions,
     * picking the backing executor based on the active JVM:
     * virtual threads on Java 21+, platform-thread {@link ThreadPoolExecutor}
     * on Java 17.
     *
     * @param workers concurrency budget. Must be &ge; 1. Values are
     *     not capped by this class; callers are expected to resolve
     *     {@code default_workers} expressions like {@code "1C"} to a
     *     concrete integer before calling.
     * @return a ready-to-use pool. Close with {@link #close()} or
     *     try-with-resources.
     */
    public static WorkerPool create(int workers) {
        if (workers <= 0) {
            throw new IllegalArgumentException(
                    "workers must be >= 1; got " + workers);
        }
        int feature = Runtime.version().feature();
        if (feature >= 21) {
            ExecutorService vt = newVirtualThreadExecutor();
            LOG.log(Level.FINE,
                    () -> "WorkerPool created on JDK " + feature
                            + " using virtual threads (budget=" + workers + ")");
            return new WorkerPool(vt, workers, Backend.VIRTUAL_THREADS);
        }
        ExecutorService tpe = newPlatformThreadPool(workers);
        LOG.log(Level.FINE,
                () -> "WorkerPool created on JDK " + feature
                        + " using bounded ThreadPoolExecutor (workers=" + workers + ")");
        return new WorkerPool(tpe, workers, Backend.PLATFORM_THREAD_POOL);
    }

    /**
     * Build a worker pool around a caller-supplied executor. The
     * runtime branch is bypassed; the pool wraps {@code executor}
     * exactly as given and still enforces the configured concurrency
     * budget via its internal {@link Semaphore}.
     *
     * <p>Primary use case: drive the JDK-17 {@link ThreadPoolExecutor}
     * branch from a JDK-21 unit test (and vice versa) without
     * rebooting the JVM. JMH benches also use this entry point to
     * compare backends head-to-head under controlled load.
     *
     * @param executor backing executor. Lifecycle is owned by the
     *     returned {@link WorkerPool}; {@link #close()} will call
     *     {@link ExecutorService#shutdown()} on it.
     * @param workers concurrency budget. Must be &ge; 1.
     */
    public static WorkerPool createWith(ExecutorService executor, int workers) {
        return new WorkerPool(executor, workers, Backend.INJECTED);
    }

    /**
     * Construct a fresh JDK 21+ {@link Executors#newVirtualThreadPerTaskExecutor()}.
     * Public so JMH benches and tests can wire a known backend through
     * {@link #createWith(ExecutorService, int)} without invoking
     * {@link #create(int)}.
     *
     * @throws UnsupportedOperationException if invoked on a JVM that
     *     does not have {@code newVirtualThreadPerTaskExecutor} (i.e.
     *     pre-Java 21). The {@code --release 17} compile target
     *     forces us to look the method up reflectively; callers on
     *     JDK 17 should use {@link #newPlatformThreadPool(int)}.
     */
    public static ExecutorService newVirtualThreadExecutor() {
        try {
            return (ExecutorService) Executors.class
                    .getMethod("newVirtualThreadPerTaskExecutor")
                    .invoke(null);
        } catch (NoSuchMethodException e) {
            throw new UnsupportedOperationException(
                    "newVirtualThreadPerTaskExecutor is not available on this JVM ("
                            + "feature=" + Runtime.version().feature()
                            + "); use newPlatformThreadPool(int) on JDK 17",
                    e);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(
                    "failed to construct virtual-thread executor reflectively", e);
        }
    }

    /**
     * Construct a fresh JDK 17 fallback executor: a bounded
     * {@link ThreadPoolExecutor} sized to {@code workers} platform
     * threads, a {@link LinkedBlockingQueue} of capacity
     * {@code workers * QUEUE_CAPACITY_FACTOR}, and
     * {@link ThreadPoolExecutor.CallerRunsPolicy} for graceful
     * degradation under overload.
     *
     * <p>Public so JMH benches and tests can wire this backend through
     * {@link #createWith(ExecutorService, int)} on any JVM.
     */
    public static ExecutorService newPlatformThreadPool(int workers) {
        if (workers <= 0) {
            throw new IllegalArgumentException(
                    "workers must be >= 1; got " + workers);
        }
        ThreadFactory factory = platformWorkerThreadFactory();
        ThreadPoolExecutor pool = new ThreadPoolExecutor(
                workers,
                workers,
                0L, TimeUnit.MILLISECONDS,
                new LinkedBlockingQueue<>(Math.max(workers * QUEUE_CAPACITY_FACTOR, 1)),
                factory,
                new ThreadPoolExecutor.CallerRunsPolicy());
        // We size core == max so threads stay warm; this matches the
        // daemon's "1C" sizing model where the pool is sized to physical
        // resources rather than churned per-task.
        pool.allowCoreThreadTimeOut(false);
        return pool;
    }

    private static ThreadFactory platformWorkerThreadFactory() {
        return new ThreadFactory() {
            private final AtomicLong seq = new AtomicLong(0);

            @Override
            public Thread newThread(Runnable r) {
                Thread t = new Thread(r, "barback-worker-" + seq.incrementAndGet());
                // Worker threads must NOT be daemon: an action that
                // forks a long-running mojo (e.g. an integration-test
                // server) needs the JVM to stay alive until the action
                // returns. The daemon-level shutdown path drains the
                // pool explicitly via close().
                t.setDaemon(false);
                return t;
            }
        };
    }

    /**
     * Submit a task and return a {@link Future} for its result. The
     * configured concurrency budget is enforced via an internal
     * {@link Semaphore}: at most {@link #workers()} tasks run
     * simultaneously, regardless of how cheap the backing executor's
     * threads are.
     *
     * <p>If the pool has been {@linkplain #close() closed}, the call
     * raises {@link RejectedExecutionException} and does not consume
     * the budget.
     */
    public <T> Future<T> submit(Callable<T> task) {
        Objects.requireNonNull(task, "task");
        try {
            return executor.submit(wrap(task));
        } catch (RejectedExecutionException e) {
            // The wrap() inner Callable acquires the permit inside
            // call(), so a rejection at submit() time means the budget
            // was never touched. Re-throw unchanged.
            throw e;
        }
    }

    /**
     * Submit a {@link Runnable} task. The configured concurrency
     * budget is enforced.
     */
    public Future<?> submit(Runnable task) {
        Objects.requireNonNull(task, "task");
        return executor.submit(wrap(task));
    }

    /**
     * Fire-and-forget execution. The configured concurrency budget is
     * enforced.
     */
    public void execute(Runnable task) {
        Objects.requireNonNull(task, "task");
        executor.execute(wrap(task));
    }

    /**
     * Wrap a {@link Callable} so it acquires the concurrency permit
     * before running and releases it on completion. Acquisition is
     * uninterruptible: a task that has already been submitted to the
     * backing executor must not silently disappear because the calling
     * code received an unrelated interrupt.
     */
    private <T> Callable<T> wrap(Callable<T> task) {
        return () -> {
            concurrencyBudget.acquireUninterruptibly();
            try {
                return task.call();
            } finally {
                concurrencyBudget.release();
            }
        };
    }

    private Runnable wrap(Runnable task) {
        return () -> {
            concurrencyBudget.acquireUninterruptibly();
            try {
                task.run();
            } finally {
                concurrencyBudget.release();
            }
        };
    }

    /** Configured concurrency budget (number of simultaneous actions). */
    public int workers() {
        return workers;
    }

    /** Which executor backend this pool is using. */
    public Backend backend() {
        return backend;
    }

    /**
     * Expose the underlying executor for code that genuinely needs an
     * {@link ExecutorService} reference (e.g. the existing
     * {@code Server} accept-loop drain path). Callers should prefer
     * {@link #submit(Callable)} / {@link #execute(Runnable)} so the
     * concurrency budget is honoured.
     */
    public ExecutorService executor() {
        return executor;
    }

    /**
     * Initiate an orderly shutdown of the backing executor, wait up to
     * {@link #SHUTDOWN_GRACE} for in-flight work to drain, and force
     * {@link ExecutorService#shutdownNow()} if any tasks remain.
     */
    @Override
    public void close() {
        executor.shutdown();
        try {
            if (!executor.awaitTermination(SHUTDOWN_GRACE_SECONDS, TimeUnit.SECONDS)) {
                LOG.log(Level.WARNING,
                        () -> "WorkerPool did not drain within " + SHUTDOWN_GRACE_SECONDS
                                + "s; forcing shutdownNow()");
                executor.shutdownNow();
            }
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            executor.shutdownNow();
        }
    }
}
