// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.lifecycle;

import java.time.Clock;
import java.time.Duration;
import java.time.Instant;
import java.util.Objects;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;
import java.util.logging.Level;
import java.util.logging.Logger;

/**
 * Sliding-window idle-shutdown timer for the {@code barback} daemon.
 *
 * <p>Each {@link #recordActivity()} call resets a single-shot countdown
 * to the configured {@code idleTimeout}. When the countdown elapses
 * without further activity, the supplied {@code onIdleExpired} callback
 * fires exactly once on the timer's internal scheduler thread. Realises
 * the {@code barback.idle_shutdown_seconds} configuration knob from
 * §11.2.3 of the daemon spec ("after {@code idle_shutdown_seconds} of
 * no requests, the daemon self-terminates").
 *
 * <h2>Threading model</h2>
 *
 * <p>The timer owns a private single-threaded
 * {@link ScheduledExecutorService} whose worker is a <em>daemon</em>
 * thread named {@code barback-idle-timer}. The daemon-thread choice is
 * deliberate: the production accept loop in
 * {@code Server#runAcceptLoop} is the only non-daemon thread that
 * keeps the JVM alive, so the timer must never extend the process
 * lifetime past listener shutdown — if the listener exits first
 * (because of a {@code Shutdown} envelope or a {@code SIGTERM}), the
 * idle-timer thread is reaped automatically.
 *
 * <p>{@link #recordActivity()} is safe to call from any thread,
 * including from inside a per-connection worker. It does a
 * compare-and-swap on a single {@link AtomicReference} holding the
 * currently-scheduled {@link ScheduledFuture}; the old future is
 * cancelled and a fresh one scheduled. Lost cancels are harmless —
 * each scheduled task re-validates against the last-activity instant
 * before firing, so a stale wake-up after a recent
 * {@code recordActivity} simply observes that the deadline has moved
 * and re-schedules itself.
 *
 * <h2>Clock injection</h2>
 *
 * <p>The constructor takes a {@link Clock} so tests can drive the
 * timer with a manually advanced clock without spending real wall time.
 * Production callers pass {@link Clock#systemUTC()}; the timer never
 * relies on the clock's time zone (only on {@link Clock#instant()}
 * deltas).
 *
 * <h2>One-shot guarantee</h2>
 *
 * <p>The {@code onIdleExpired} callback fires at most once per
 * {@link IdleTimer} instance. Once the callback has been invoked the
 * timer transitions to a terminal state and ignores subsequent
 * {@link #recordActivity()} calls — by then the {@code Server}
 * shutdown is in progress and any racing dispatch is being closed
 * out. This avoids the awkward case where a worker thread blocks
 * inside the dispatch path, calls {@code recordActivity} mid-tear-down,
 * and accidentally re-arms the idle timer against a partially-closed
 * server.
 */
public final class IdleTimer implements AutoCloseable {

    private static final Logger LOG = Logger.getLogger(IdleTimer.class.getName());

    /**
     * Resolution at which a fired scheduled task re-checks whether the
     * idle window has actually elapsed. Activity recorded after the
     * task was scheduled but before it ran can move the deadline
     * forward; this slack covers the cancel-race between
     * {@link #recordActivity()} and the scheduler firing the prior
     * task. Five milliseconds is well under the {@code +5s} drain
     * budget in the milestone-level idle-shutdown acceptance criterion.
     */
    private static final Duration RECHECK_SLACK = Duration.ofMillis(5);

    private final Duration idleTimeout;
    private final Runnable onIdleExpired;
    private final Clock clock;
    private final ScheduledExecutorService scheduler;

    /** Most recent activity timestamp. Read by the scheduled re-check. */
    private final AtomicReference<Instant> lastActivity = new AtomicReference<>();

    /** Currently-scheduled re-check, swapped on every recordActivity. */
    private final AtomicReference<ScheduledFuture<?>> pending = new AtomicReference<>();

    /** Set once the timer is started. */
    private final AtomicBoolean started = new AtomicBoolean(false);

    /** Set once the timer is stopped (idempotent shutdown). */
    private final AtomicBoolean stopped = new AtomicBoolean(false);

    /** Latches the moment {@code onIdleExpired} has fired. */
    private final AtomicBoolean fired = new AtomicBoolean(false);

    /**
     * Build a new idle-shutdown timer.
     *
     * @param idleTimeout how long without activity before
     *     {@code onIdleExpired} fires. Must be strictly positive; zero
     *     is rejected because a zero-second window would fire before
     *     {@link #start()} returned.
     * @param onIdleExpired the callback to invoke when the window
     *     elapses. Runs on the timer's internal daemon thread; should
     *     return promptly (the {@code Server::shutdown} surface this
     *     wires up to is non-blocking).
     * @param clock the clock the timer reads. Production callers pass
     *     {@link Clock#systemUTC()}; tests inject a mutable clock.
     */
    public IdleTimer(Duration idleTimeout, Runnable onIdleExpired, Clock clock) {
        this.idleTimeout = Objects.requireNonNull(idleTimeout, "idleTimeout");
        if (idleTimeout.isZero() || idleTimeout.isNegative()) {
            throw new IllegalArgumentException(
                    "idleTimeout must be > 0; got " + idleTimeout);
        }
        this.onIdleExpired = Objects.requireNonNull(onIdleExpired, "onIdleExpired");
        this.clock = Objects.requireNonNull(clock, "clock");
        this.scheduler = Executors.newSingleThreadScheduledExecutor(daemonThreadFactory());
    }

    /**
     * Start the timer. The first idle window begins immediately and
     * the configured callback fires {@code idleTimeout} from now if
     * {@link #recordActivity()} is never called. Calling {@code start}
     * twice on the same instance throws {@link IllegalStateException}
     * so a missed lifecycle wire is caught early.
     */
    public void start() {
        if (!started.compareAndSet(false, true)) {
            throw new IllegalStateException("IdleTimer.start() already called");
        }
        if (stopped.get()) {
            throw new IllegalStateException(
                    "IdleTimer.start() called after stop(); construct a new instance");
        }
        // Treat startup as an initial activity beat so the first
        // window is exactly idleTimeout, not 0.
        lastActivity.set(clock.instant());
        scheduleNext(idleTimeout);
        LOG.log(Level.FINE,
                () -> "IdleTimer started (idleTimeout=" + idleTimeout + ")");
    }

    /**
     * Record that an action just made it onto a worker. Resets the
     * idle countdown. Safe to call from any thread, including before
     * {@link #start()} (the recorded timestamp is picked up on the
     * first scheduled re-check) and after the callback has fired
     * (no-op).
     *
     * <p>Callers should invoke this once per inbound action dispatch,
     * before the action begins execution. The exact placement is the
     * caller's choice; the recommended seam is at the top of the
     * dispatch switch in {@code Server} so a freshly-arrived
     * {@code Ping}/{@code Action}/{@code Shutdown} envelope resets the
     * window even on idle-but-talkative clients.
     */
    public void recordActivity() {
        if (fired.get() || stopped.get()) {
            return;
        }
        lastActivity.set(clock.instant());
        if (started.get()) {
            // Re-schedule from scratch so the next firing is exactly
            // idleTimeout from this instant. Lost cancels are
            // harmless: the re-check inside fireOrReschedule re-reads
            // lastActivity and re-arms.
            scheduleNext(idleTimeout);
        }
    }

    /**
     * Stop the timer without firing the callback. Idempotent. Used by
     * the {@code Server} teardown path so an in-progress shutdown
     * cannot race a scheduled idle wake-up.
     */
    public void stop() {
        if (!stopped.compareAndSet(false, true)) {
            return;
        }
        ScheduledFuture<?> f = pending.getAndSet(null);
        if (f != null) {
            f.cancel(false);
        }
        scheduler.shutdownNow();
        LOG.log(Level.FINE, "IdleTimer stopped");
    }

    /**
     * Equivalent to {@link #stop()}. Lets {@code IdleTimer} participate
     * in try-with-resources or {@code AutoCloseable} chains alongside
     * {@code Server}.
     */
    @Override
    public void close() {
        stop();
    }

    /**
     * Whether the {@code onIdleExpired} callback has fired. Visible
     * for tests and the {@code Server} teardown path; production
     * callers should not branch on this.
     */
    public boolean hasFired() {
        return fired.get();
    }

    // ----------------------------------------------------------------
    // Implementation details below this line
    // ----------------------------------------------------------------

    private void scheduleNext(Duration delay) {
        long delayNanos = Math.max(0L, delay.toNanos());
        ScheduledFuture<?> next;
        try {
            next = scheduler.schedule(
                    this::fireOrReschedule, delayNanos, TimeUnit.NANOSECONDS);
        } catch (java.util.concurrent.RejectedExecutionException e) {
            // Scheduler is shutting down — happens during a stop()
            // racing a recordActivity() from a worker thread. Safe to
            // drop; the stop() path has already disabled further work.
            LOG.log(Level.FINE,
                    "IdleTimer scheduler refused next slot during shutdown", e);
            return;
        }
        ScheduledFuture<?> prior = pending.getAndSet(next);
        if (prior != null) {
            prior.cancel(false);
        }
    }

    private void fireOrReschedule() {
        if (stopped.get() || fired.get()) {
            return;
        }
        Instant last = lastActivity.get();
        if (last == null) {
            // Defensive: scheduleNext was called before start() set the
            // initial activity timestamp. Re-arm for one full window.
            scheduleNext(idleTimeout);
            return;
        }
        Duration elapsed = Duration.between(last, clock.instant());
        Duration remaining = idleTimeout.minus(elapsed);
        // Treat anything <= RECHECK_SLACK as "elapsed" so we don't
        // bounce on sub-millisecond drift between Clock.instant()
        // and the scheduler's monotonic tick.
        if (remaining.compareTo(RECHECK_SLACK) <= 0) {
            if (fired.compareAndSet(false, true)) {
                LOG.log(Level.INFO,
                        () -> "idle window of " + idleTimeout
                                + " elapsed; firing shutdown callback");
                try {
                    onIdleExpired.run();
                } catch (RuntimeException e) {
                    // The callback misbehaving must not blow up the
                    // timer thread silently — log loud + leave the
                    // fired flag set so we never double-fire.
                    LOG.log(Level.SEVERE,
                            "IdleTimer onIdleExpired callback threw", e);
                }
            }
            return;
        }
        // Activity arrived after we scheduled this task. Re-arm for
        // the remaining slice instead of a full window so the
        // sliding-window semantics hold even under bursty contention.
        scheduleNext(remaining);
    }

    private static ThreadFactory daemonThreadFactory() {
        return r -> {
            Thread t = new Thread(r, "barback-idle-timer");
            t.setDaemon(true);
            return t;
        };
    }
}
