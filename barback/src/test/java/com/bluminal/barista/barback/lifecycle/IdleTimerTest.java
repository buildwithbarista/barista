// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.lifecycle;

import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Test;

import java.time.Clock;
import java.time.Duration;
import java.time.Instant;
import java.time.ZoneId;
import java.time.ZoneOffset;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.CyclicBarrier;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Unit tests for {@link IdleTimer}. Three concerns are pinned here:
 *
 * <ol>
 *   <li>The callback fires when the idle window elapses without
 *       activity (the load-bearing T5 contract).</li>
 *   <li>{@link IdleTimer#recordActivity()} resets the deadline so a
 *       talkative client never trips the idle path.</li>
 *   <li>Thread-safety: concurrent {@code recordActivity} from many
 *       threads does not corrupt the timer or double-fire the
 *       callback.</li>
 * </ol>
 *
 * <p>The tests use a {@link MutableClock} so the in-process scheduler
 * sees moves of the wall clock without spending real time on a
 * {@code Thread.sleep}; the integration tests under
 * {@code com.bluminal.barista.barback.integration} cover the
 * end-to-end timing path under real wall-clock.
 */
class IdleTimerTest {

    @Test
    @DisplayName("constructor rejects zero or negative idleTimeout")
    void constructor_rejectsNonPositiveTimeout() {
        assertThrows(IllegalArgumentException.class, () -> new IdleTimer(
                Duration.ZERO, () -> {}, Clock.systemUTC()));
        assertThrows(IllegalArgumentException.class, () -> new IdleTimer(
                Duration.ofSeconds(-1), () -> {}, Clock.systemUTC()));
    }

    @Test
    @DisplayName("callback fires once after the idle window elapses")
    void idleWindow_firesCallbackOnce() throws Exception {
        MutableClock clock = new MutableClock(Instant.parse("2026-01-01T00:00:00Z"));
        AtomicInteger fired = new AtomicInteger(0);
        CountDownLatch firedOnce = new CountDownLatch(1);
        try (IdleTimer timer = new IdleTimer(
                Duration.ofMillis(50),
                () -> {
                    fired.incrementAndGet();
                    firedOnce.countDown();
                },
                clock)) {
            timer.start();
            // Advance the mutable clock past the window. The
            // timer's scheduler will still wake up on the real
            // monotonic interval (~50ms) — what matters is that when
            // it does, fireOrReschedule reads the mutable clock and
            // sees the deadline has passed.
            clock.advance(Duration.ofMillis(200));
            assertTrue(firedOnce.await(2, TimeUnit.SECONDS),
                    "callback must fire once the idle window elapses");
            // Give any racing scheduled re-check a moment to run so a
            // double-fire bug would show up here.
            Thread.sleep(100);
            assertEquals(1, fired.get(),
                    "callback must be invoked at most once per IdleTimer instance");
            assertTrue(timer.hasFired(), "hasFired() must reflect the post-fire state");
        }
    }

    @Test
    @DisplayName("recordActivity resets the idle window")
    void recordActivity_resetsWindow() throws Exception {
        MutableClock clock = new MutableClock(Instant.parse("2026-01-01T00:00:00Z"));
        AtomicInteger fired = new AtomicInteger(0);
        try (IdleTimer timer = new IdleTimer(
                Duration.ofMillis(100),
                fired::incrementAndGet,
                clock)) {
            timer.start();
            // Tick forward in 30ms increments with a recordActivity
            // between each. The cumulative time well exceeds the
            // 100ms window but no single quiet stretch is long
            // enough — the callback must NOT fire.
            for (int i = 0; i < 10; i++) {
                clock.advance(Duration.ofMillis(30));
                Thread.sleep(15);
                timer.recordActivity();
            }
            assertEquals(0, fired.get(),
                    "recordActivity calls inside the idle window must reset it; "
                            + "callback should not have fired");
            // Now go quiet — the next scheduled wake-up should fire
            // the callback.
            clock.advance(Duration.ofMillis(200));
            // Wait for the scheduler to observe the move.
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2);
            while (fired.get() == 0 && System.nanoTime() < deadline) {
                Thread.sleep(20);
            }
            assertEquals(1, fired.get(),
                    "callback must fire once activity stops and the clock advances "
                            + "past the idle window");
        }
    }

    @Test
    @DisplayName("stop prevents the callback from firing")
    void stop_preventsCallback() throws Exception {
        MutableClock clock = new MutableClock(Instant.parse("2026-01-01T00:00:00Z"));
        AtomicInteger fired = new AtomicInteger(0);
        IdleTimer timer = new IdleTimer(
                Duration.ofMillis(50),
                fired::incrementAndGet,
                clock);
        timer.start();
        timer.stop();
        clock.advance(Duration.ofMillis(500));
        Thread.sleep(150);
        assertEquals(0, fired.get(),
                "callback must not fire after stop()");
        // recordActivity after stop must be a safe no-op.
        timer.recordActivity();
        assertFalse(timer.hasFired());
        // stop() must be idempotent.
        timer.stop();
    }

    @Test
    @DisplayName("recordActivity is safe under concurrent contention")
    void recordActivity_threadSafe() throws Exception {
        MutableClock clock = new MutableClock(Instant.parse("2026-01-01T00:00:00Z"));
        AtomicInteger fired = new AtomicInteger(0);
        try (IdleTimer timer = new IdleTimer(
                Duration.ofMillis(75),
                fired::incrementAndGet,
                clock)) {
            timer.start();
            int threads = 32;
            int callsPerThread = 250;
            CyclicBarrier gate = new CyclicBarrier(threads);
            Thread[] workers = new Thread[threads];
            for (int i = 0; i < threads; i++) {
                workers[i] = new Thread(() -> {
                    try {
                        gate.await();
                        for (int j = 0; j < callsPerThread; j++) {
                            // Move the wall clock just under the
                            // idle window between calls. The
                            // sliding-window invariant must hold:
                            // even with 32 threads hammering
                            // recordActivity, no firing.
                            timer.recordActivity();
                        }
                    } catch (Exception e) {
                        throw new RuntimeException(e);
                    }
                }, "idletest-" + i);
            }
            for (Thread t : workers) t.start();
            for (Thread t : workers) t.join();
            // No advance: the clock hasn't moved past the window.
            // Give the scheduler a moment to settle.
            Thread.sleep(120);
            // After the burst the clock is still inside the window,
            // so the callback must not have fired.
            assertEquals(0, fired.get(),
                    "callback must not fire under concurrent recordActivity "
                            + "while the clock is still inside the idle window");
            // Now advance past the window and confirm the timer
            // recovers correctly from the burst.
            clock.advance(Duration.ofMillis(300));
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2);
            while (fired.get() == 0 && System.nanoTime() < deadline) {
                Thread.sleep(20);
            }
            assertEquals(1, fired.get(),
                    "callback must fire exactly once after the burst settles "
                            + "and the clock crosses the idle deadline");
        }
    }

    @Test
    @DisplayName("recordActivity before start records the timestamp safely")
    void recordActivity_beforeStart_isSafe() {
        MutableClock clock = new MutableClock(Instant.parse("2026-01-01T00:00:00Z"));
        AtomicInteger fired = new AtomicInteger(0);
        try (IdleTimer timer = new IdleTimer(
                Duration.ofSeconds(60),
                fired::incrementAndGet,
                clock)) {
            // recordActivity before start() must not throw and must
            // not fire the callback.
            timer.recordActivity();
            timer.recordActivity();
            assertEquals(0, fired.get());
            assertFalse(timer.hasFired());
        }
    }

    @Test
    @DisplayName("start twice on the same instance throws IllegalStateException")
    void start_calledTwice_throws() {
        try (IdleTimer timer = new IdleTimer(
                Duration.ofSeconds(60), () -> {}, Clock.systemUTC())) {
            timer.start();
            assertThrows(IllegalStateException.class, timer::start);
        }
    }

    @Test
    @DisplayName("start after stop throws IllegalStateException")
    void start_afterStop_throws() {
        IdleTimer timer = new IdleTimer(
                Duration.ofSeconds(60), () -> {}, Clock.systemUTC());
        timer.stop();
        assertThrows(IllegalStateException.class, timer::start);
    }

    @Test
    @DisplayName("misbehaving callback does not unblock a second firing")
    void misbehavingCallback_doesNotDoubleFire() throws Exception {
        MutableClock clock = new MutableClock(Instant.parse("2026-01-01T00:00:00Z"));
        AtomicInteger entries = new AtomicInteger(0);
        try (IdleTimer timer = new IdleTimer(
                Duration.ofMillis(50),
                () -> {
                    entries.incrementAndGet();
                    throw new RuntimeException("intentional test failure");
                },
                clock)) {
            timer.start();
            clock.advance(Duration.ofMillis(300));
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2);
            while (entries.get() == 0 && System.nanoTime() < deadline) {
                Thread.sleep(20);
            }
            assertEquals(1, entries.get(),
                    "callback must be entered exactly once even when it throws");
            Thread.sleep(150);
            assertEquals(1, entries.get(),
                    "thrown-from callback must not trigger a re-fire");
        }
    }

    // ----------------------------------------------------------------
    // MutableClock — a minimal in-process clock for deterministic
    // idle-window tests. Volatile field; readers see the latest
    // advance() value without locking.
    // ----------------------------------------------------------------

    private static final class MutableClock extends Clock {
        private volatile Instant now;

        MutableClock(Instant start) {
            this.now = start;
        }

        void advance(Duration by) {
            this.now = this.now.plus(by);
        }

        @Override
        public ZoneId getZone() {
            return ZoneOffset.UTC;
        }

        @Override
        public Clock withZone(ZoneId zone) {
            return this;
        }

        @Override
        public Instant instant() {
            return now;
        }
    }
}
