// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.integration;

import com.bluminal.barista.barback.Server;
import com.bluminal.barista.barback.workers.WorkerPool;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.DisabledOnOs;
import org.junit.jupiter.api.condition.OS;
import org.junit.jupiter.api.io.TempDir;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.TimeUnit;

import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Integration test for the M4.2 T5 idle-shutdown acceptance criterion:
 * <em>"Idle shutdown fires within {@code idle_shutdown_seconds + 5}"</em>.
 *
 * <p>Mechanical proof: a {@link Server} is started with
 * {@code idleShutdownSeconds = 2} and no client ever connects. The
 * test asserts {@link Server#awaitShutdown()} returns within 7 seconds
 * (2 s window + 5 s drain budget) and that the socket inode has been
 * removed by the teardown path.
 *
 * <p>The test runs in-process &mdash; not as a forked JVM &mdash;
 * because the idle-timer&nbsp;&rArr;&nbsp;{@code Server#shutdown}
 * &rArr; accept-loop teardown chain is the same regardless of whether
 * the JVM is the test harness or the production {@code main}. The
 * production {@code main} adds a {@code System.exit} via natural JVM
 * shutdown (the only non-daemon thread is the accept loop, which
 * terminates after the worker pool drains), so the in-process check
 * is the load-bearing one for the wire contract.
 */
class IdleShutdownIT {

    private static final long IDLE_SECONDS = 2L;
    /**
     * Slack budget per the milestone-level acceptance criterion:
     * "{@code idle_shutdown_seconds + 5}". Two seconds idle + five
     * seconds for the accept loop to unblock, drain the (empty)
     * worker pool, and count down the termination latch.
     */
    private static final long DRAIN_BUDGET_SECONDS = 5L;
    private static final long DEADLINE_SECONDS = IDLE_SECONDS + DRAIN_BUDGET_SECONDS;

    @TempDir
    Path tempDir;

    private Server server;

    @AfterEach
    void tearDown() throws Exception {
        if (server != null) {
            server.close();
            server = null;
        }
    }

    @Test
    @DisplayName("idle daemon exits within idleShutdownSeconds + 5s with no client activity")
    @DisabledOnOs(OS.WINDOWS)
    void idleDaemon_exitsWithinDeadline() throws Exception {
        WorkerPool pool = WorkerPool.createWith(
                WorkerPool.newPlatformThreadPool(1), 1);
        Path sock = tempDir.resolve("idle.sock");
        Server.SocketConfig config = new Server.SocketConfig(
                sock, /* workers */ 1, /* idleShutdownSeconds */ (int) IDLE_SECONDS);

        long startNanos = System.nanoTime();
        server = Server.startWith(config, pool);
        // Sanity: the listener is up.
        assertTrue(Files.exists(sock),
                "socket inode must exist immediately after Server.startWith returns");

        // The contract under test. The accept loop must terminate
        // within DEADLINE_SECONDS without any client ever connecting.
        boolean terminatedInTime = awaitShutdown(server, DEADLINE_SECONDS);
        long elapsedMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - startNanos);

        assertTrue(terminatedInTime,
                "daemon must self-terminate within "
                        + DEADLINE_SECONDS + "s of being started with "
                        + "idleShutdownSeconds=" + IDLE_SECONDS
                        + "; actually waited " + elapsedMs + "ms");
        // Lower bound: must NOT have fired before the idle window
        // elapsed. A 250ms slack accounts for clock-resolution races
        // around the scheduler wake-up.
        assertTrue(elapsedMs >= TimeUnit.SECONDS.toMillis(IDLE_SECONDS) - 250,
                "daemon must not self-terminate before idle window elapses; "
                        + "fired after only " + elapsedMs + "ms");
        // Socket inode must have been removed by the teardown.
        assertFalse(Files.exists(sock),
                "socket inode must be removed by the shutdown path");
    }

    /**
     * Block until the server's accept loop has terminated, with a
     * wall-clock deadline. Returns {@code true} on a clean shutdown
     * within the deadline; {@code false} on a deadline miss. We poll
     * on {@code socketPath} existence (which the shutdown path
     * removes) as a side-channel signal, because {@code awaitShutdown}
     * itself does not support a timeout — but we ALSO need to call
     * the real awaitShutdown so the test does not race the teardown.
     */
    private static boolean awaitShutdown(Server s, long seconds)
            throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(seconds);
        Thread waiter = new Thread(() -> {
            try {
                s.awaitShutdown();
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
        }, "idle-shutdown-it-waiter");
        waiter.setDaemon(true);
        waiter.start();
        long remaining = deadline - System.nanoTime();
        if (remaining <= 0) {
            return false;
        }
        waiter.join(TimeUnit.NANOSECONDS.toMillis(remaining));
        return !waiter.isAlive();
    }
}
