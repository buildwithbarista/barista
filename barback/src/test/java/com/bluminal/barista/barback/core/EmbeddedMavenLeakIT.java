/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.core;

import com.bluminal.barista.barback.proto.ActionRequest;
import com.bluminal.barista.barback.proto.ActionResult;

import java.io.IOException;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryMXBean;
import java.lang.management.MemoryUsage;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Comparator;
import java.util.UUID;
import java.util.stream.Stream;

import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Tag;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Long-running leak-burn-down test: drives 100 sequential actions
 * through a single {@link EmbeddedMaven} instance and asserts the
 * old-gen heap drift stays within the M4.2 acceptance band of
 * &plusmn;10&nbsp;MiB.
 *
 * <p>This covers the M4.2 acceptance criterion verbatim: "daemon
 * survives 100 sequential actions without leak (JVM heap stable to
 * &plusmn;10 MB)". The envelope is honored not by zero per-action
 * growth (Maven&nbsp;4.0.0-rc-3 leaks &asymp;0.57&nbsp;MiB per call
 * out of {@code ResidentMavenInvoker}'s session cache &mdash; see the
 * "Periodic invoker eviction" javadoc on {@link EmbeddedMaven}) but
 * by the eviction policy in {@link EmbeddedMaven}: every
 * {@link EmbeddedMaven#MAX_ACTIONS_PER_INVOKER} calls the daemon
 * closes the held {@code ResidentMavenInvoker} and rebuilds it, which
 * releases the accumulated {@code MavenContext} cache for the next
 * major GC to reclaim. The peak inside one cycle stays well under
 * 10&nbsp;MiB; the trough between cycles is &asymp; baseline. Over
 * 100 actions the drift between the post-warmup baseline and the
 * final sample falls inside the &plusmn;10&nbsp;MiB band.
 *
 * <p><b>"No leak per invoker" vs "no growth across invocations."</b>
 * This test guards the latter at the daemon level. A true per-invoker
 * leak (e.g. the resident cache growing without bound between
 * eviction-boundary calls) would still fail the assertion because
 * the cycle peak would exceed 10&nbsp;MiB above baseline. Conversely,
 * the eviction policy specifically <em>does not</em> claim that an
 * individual {@code ResidentMavenInvoker} has zero growth &mdash; it
 * claims that bounding the invoker's lifetime caps the visible
 * growth at the daemon level. If a future upstream Maven release
 * fixes the session-cache shape, the policy can be removed; this
 * test should still pass against that fixed version with
 * {@code MAX_ACTIONS_PER_INVOKER} raised to {@code Integer.MAX_VALUE}
 * or the policy deleted outright.
 *
 * <p>The full criterion is shared with the worker-pool plumbing
 * (M4.2 T2) and the failure-model wiring (M4.2 T6), both of which
 * add their own action loops on top of this one.
 *
 * <p>Tagged {@code integration} so unit-test runs do not block on the
 * &asymp;30&nbsp;s cost of 100 compiles. Run via {@code -Dgroups=integration}
 * or the CI integration job.
 */
@Tag("integration")
final class EmbeddedMavenLeakIT {

    /** Number of measured actions to drive through the embedded core. */
    private static final int LOOP_COUNT = 100;

    /**
     * Acceptance-criterion heap-drift envelope: &plusmn;10&nbsp;MiB
     * across 100 sequential actions. Expressed in bytes so the
     * comparison stays integer-clean.
     *
     * <p>The envelope is honored by {@link EmbeddedMaven}'s periodic
     * eviction policy: every
     * {@link EmbeddedMaven#MAX_ACTIONS_PER_INVOKER} actions the held
     * {@code ResidentMavenInvoker} is closed and rebuilt, capping the
     * upstream rc-3 session-cache growth (&asymp;0.57&nbsp;MiB / action)
     * inside a bounded window. With {@code N = 15} the worst-case
     * peak above the eviction-trough baseline is &asymp;14 &times;
     * 0.57 = 8.0&nbsp;MiB &mdash; comfortably inside the 10&nbsp;MiB
     * band even with the heap-sampling jitter that
     * {@link #sampleHeap(MemoryMXBean)} encounters.
     */
    private static final long HEAP_DRIFT_CEILING_BYTES = 10L * 1024L * 1024L;

    /**
     * Warm-up iteration count before measuring the steady-state band.
     * The first compile pays the cold-start cost (&asymp;1&nbsp;s), the
     * next dozen settle JIT tiered compilation, plugin classloader
     * caching, and the resident invoker's cached {@code MavenContext}
     * fields. After that the heap profile should be flat.
     */
    private static final int WARMUP_ITERS = 20;

    private EmbeddedMaven embedded;

    @AfterEach
    void closeEmbedded() throws IOException {
        if (embedded != null) {
            embedded.close();
            embedded = null;
        }
    }

    @Test
    @DisplayName("100 sequential actions leave heap stable within 10 MiB")
    void hundredActionsHeapStable(@TempDir Path tmp) throws IOException {
        Path mavenHome = MavenDistributionFixture.requireMavenHome();
        Path project = MavenDistributionFixture.stageSampleProject(tmp);
        embedded = EmbeddedMavenFactory.using(mavenHome);

        MemoryMXBean memory = ManagementFactory.getMemoryMXBean();

        // Warm-up: pay the cold-start cost and let the resident
        // invoker's cache + JIT settle before sampling the baseline.
        for (int i = 0; i < WARMUP_ITERS; i++) {
            executeOnce(project);
        }
        long baselineHeapBytes = sampleHeap(memory);

        long maxHeapBytes = baselineHeapBytes;
        long minHeapBytes = baselineHeapBytes;
        for (int i = 0; i < LOOP_COUNT; i++) {
            executeOnce(project);
            // Sample every 10 iterations to keep the test's overhead
            // negligible vs. the embedded-Maven cost.
            if (i % 10 == 9) {
                long current = sampleHeap(memory);
                maxHeapBytes = Math.max(maxHeapBytes, current);
                minHeapBytes = Math.min(minHeapBytes, current);
            }
        }
        long finalHeapBytes = sampleHeap(memory);
        maxHeapBytes = Math.max(maxHeapBytes, finalHeapBytes);
        minHeapBytes = Math.min(minHeapBytes, finalHeapBytes);

        long driftBytes = finalHeapBytes - baselineHeapBytes;
        long bandBytes = maxHeapBytes - minHeapBytes;
        long peakAboveBaseline = maxHeapBytes - baselineHeapBytes;
        long troughBelowBaseline = baselineHeapBytes - minHeapBytes;

        // Total invocations = warm-up + measured loop (no off-by-one).
        assertEquals(WARMUP_ITERS + LOOP_COUNT, embedded.invocationCount(),
                "invocation counter should equal warmup+loop");

        System.out.printf(
                "EmbeddedMavenLeakIT baseline_mib=%d final_mib=%d max_mib=%d min_mib=%d "
                        + "band_mib=%d drift_mib=%d peak_above_mib=%d trough_below_mib=%d "
                        + "rebuilds=%d ceiling_mib=%d%n",
                bytesToMib(baselineHeapBytes), bytesToMib(finalHeapBytes),
                bytesToMib(maxHeapBytes), bytesToMib(minHeapBytes),
                bytesToMib(bandBytes), bytesToMib(driftBytes),
                bytesToMib(peakAboveBaseline), bytesToMib(troughBelowBaseline),
                embedded.invokerRebuildCount(),
                bytesToMib(HEAP_DRIFT_CEILING_BYTES));

        // The acceptance criterion is "JVM heap stable to ±10 MiB".
        // Honor the literal envelope: no sample inside the measured
        // loop may be more than 10 MiB above the post-warmup baseline,
        // and no sample may be more than 10 MiB below it. Both bounds
        // are necessary — a leak shows up as peak-above; a runaway
        // free (e.g. accidentally tearing down the resident cache
        // every call) shows up as trough-below.
        assertTrue(peakAboveBaseline <= HEAP_DRIFT_CEILING_BYTES,
                "peak old-gen usage exceeded baseline + "
                        + bytesToMib(HEAP_DRIFT_CEILING_BYTES) + " MiB envelope: "
                        + "baseline_mib=" + bytesToMib(baselineHeapBytes)
                        + " max_mib=" + bytesToMib(maxHeapBytes)
                        + " peak_above_mib=" + bytesToMib(peakAboveBaseline)
                        + " rebuilds=" + embedded.invokerRebuildCount()
                        + " — the resident invoker eviction policy may be "
                        + "mis-tuned (raise MAX_ACTIONS_PER_INVOKER cadence) "
                        + "or upstream growth rate has increased; check the "
                        + "embedded.maven.version pin in barback/pom.xml.");
        assertTrue(troughBelowBaseline <= HEAP_DRIFT_CEILING_BYTES,
                "minimum old-gen usage fell more than "
                        + bytesToMib(HEAP_DRIFT_CEILING_BYTES) + " MiB below baseline: "
                        + "baseline_mib=" + bytesToMib(baselineHeapBytes)
                        + " min_mib=" + bytesToMib(minHeapBytes)
                        + " trough_below_mib=" + bytesToMib(troughBelowBaseline));
        assertTrue(Math.abs(driftBytes) <= HEAP_DRIFT_CEILING_BYTES,
                "final-minus-baseline heap drift exceeded ±"
                        + bytesToMib(HEAP_DRIFT_CEILING_BYTES) + " MiB: "
                        + "baseline_mib=" + bytesToMib(baselineHeapBytes)
                        + " final_mib=" + bytesToMib(finalHeapBytes)
                        + " drift_mib=" + bytesToMib(driftBytes));
    }

    private void executeOnce(Path project) throws IOException {
        cleanTarget(project);
        ActionRequest request = ActionRequest.newBuilder()
                .setActionId(UUID.randomUUID().toString())
                .setMojoCoords("compile")
                .setPomPath(project.resolve("pom.xml").toString())
                .setProjectRoot(project.toString())
                .setWorkingDirectory(project.toString())
                .setQuiet(true)
                .build();
        ActionResult result = embedded.execute(request);
        assertEquals(ActionResult.Status.SUCCESS, result.getStatus(),
                "compile should succeed; failure=" + result.getFailureMessage());
    }

    /**
     * Force several full GC passes before reading heap usage, then
     * read the long-lived (old-generation) tenured pool rather than
     * the total heap. {@link System#gc()} is a hint rather than a
     * guarantee, and on G1/ZGC a single hint commonly skips
     * collecting young-gen survivor regions whose contents are
     * already garbage. The "GC, sleep, GC, sleep" double-pump pattern
     * here is what the JDK ecosystem's leak tests use to coax the
     * runtime into reporting steady-state retained memory.
     *
     * <p>We additionally fall back to the aggregate heap reading if
     * no old-gen pool is reported (some pools have non-standard
     * names across collectors); the aggregate is noisier but never
     * unavailable.
     */
    private static long sampleHeap(MemoryMXBean memory) {
        for (int i = 0; i < 3; i++) {
            System.gc();
            try {
                Thread.sleep(100L);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                break;
            }
        }
        // Prefer the old-gen pool. G1 and ZGC name theirs distinctively
        // ("G1 Old Gen" / "ZHeap" / "Tenured Gen"); we look for any
        // pool with "Old" or "Tenured" or that ends in "Heap".
        long oldGen = -1L;
        for (java.lang.management.MemoryPoolMXBean pool
                : java.lang.management.ManagementFactory.getMemoryPoolMXBeans()) {
            if (pool.getType() != java.lang.management.MemoryType.HEAP) {
                continue;
            }
            String name = pool.getName();
            if (name.contains("Old") || name.contains("Tenured") || name.equals("ZHeap")) {
                MemoryUsage usage = pool.getUsage();
                if (usage != null) {
                    oldGen = Math.max(oldGen, usage.getUsed());
                }
            }
        }
        if (oldGen >= 0) {
            return oldGen;
        }
        return memory.getHeapMemoryUsage().getUsed();
    }

    private static long bytesToMib(long bytes) {
        return bytes / (1024L * 1024L);
    }

    private static void cleanTarget(Path project) throws IOException {
        Path target = project.resolve("target");
        if (!Files.exists(target)) {
            return;
        }
        try (Stream<Path> walk = Files.walk(target)) {
            walk.sorted(Comparator.reverseOrder()).forEach(p -> {
                try {
                    Files.deleteIfExists(p);
                } catch (IOException ignored) {
                    // Best-effort; the @TempDir guarantees final cleanup.
                }
            });
        }
    }
}
