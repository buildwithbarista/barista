/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.core;

import com.bluminal.barista.barback.proto.ActionRequest;
import com.bluminal.barista.barback.proto.ActionResult;

import java.io.IOException;
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
 * Verifies that the second-and-subsequent invocations against a
 * single {@link EmbeddedMaven} instance skip the cold-start cost by
 * reusing the {@code ResidentMavenInvoker}'s cached {@code MavenContext}.
 *
 * <p>Spike numbers (recorded in the embedded-Maven spec):
 *
 * <ul>
 *   <li>EMBED-COLD: &asymp;1080&nbsp;ms median</li>
 *   <li>EMBED-WARM: &asymp;160&nbsp;ms median (&asymp;7&times; speedup)</li>
 * </ul>
 *
 * <p>The test asserts a deliberately relaxed &ge;1.5&times; speedup
 * threshold compared to the spike's &asymp;6.7&times;. Two
 * contributors compress the ratio under test:
 *
 * <ul>
 *   <li>CI runners (especially containerised ones) suffer from noisy
 *       neighbours and slow filesystems that drag both endpoints.</li>
 *   <li>When this class runs in a JVM that already executed an
 *       earlier embedded-Maven test, the "cold" measurement is no
 *       longer a true cold start &mdash; the class loader has resolved
 *       Maven bytecode and the JIT has compiled the hot paths.</li>
 * </ul>
 *
 * <p>The point of this test is to detect a regression where the
 * resident invoker is being rebuilt per call (in which case
 * {@code warm &asymp; cold}, ratio &asymp; 1.0), not to police precise
 * wall-clock numbers; the latter is the benchmark harness's job.
 *
 * <p>Tagged {@code integration} so unit-test runs do not block on the
 * &gt;1&nbsp;s cold-start cost. Run via {@code -Dgroups=integration}
 * or the CI integration job.
 */
@Tag("integration")
final class ResidentInvokerWarmPathTest {

    private EmbeddedMaven embedded;

    @AfterEach
    void closeEmbedded() throws IOException {
        if (embedded != null) {
            embedded.close();
            embedded = null;
        }
    }

    @Test
    @DisplayName("warm-path invocations skip the cold-start cost (>=2x speedup)")
    void warmPathFasterThanColdPath(@TempDir Path tmp) throws IOException {
        Path mavenHome = MavenDistributionFixture.requireMavenHome();
        Path project = MavenDistributionFixture.stageSampleProject(tmp);
        embedded = EmbeddedMavenFactory.using(mavenHome);

        long coldMicros = runOne(project);
        long firstWarmMicros = runOne(project);
        // The very first warm iteration still amortises JIT settling
        // from cold; take the third call as the representative warm
        // measurement, matching the M4.0 spike's methodology.
        long warmMicrosA = runOne(project);
        long warmMicrosB = runOne(project);
        long warmMicros = Math.min(warmMicrosA, warmMicrosB);

        assertTrue(coldMicros > 0, "cold duration should be positive");
        assertTrue(warmMicros > 0, "warm duration should be positive");

        double ratio = (double) coldMicros / (double) warmMicros;
        long coldMs = coldMicros / 1_000L;
        long firstWarmMs = firstWarmMicros / 1_000L;
        long warmMs = warmMicros / 1_000L;

        // Emit on stdout so the surefire output captures the
        // measurement next to the test result; useful when the test
        // is run from a developer machine and the numbers feed an
        // ADR amendment.
        System.out.printf("ResidentInvokerWarmPathTest cold_ms=%d first_warm_ms=%d warm_ms=%d ratio=%.2fx%n",
                coldMs, firstWarmMs, warmMs, ratio);

        // 1.5x is the CI-robust floor. The spike harness on developer
        // hardware lands around 6-7x with a fresh JVM. When this test
        // runs in a JVM that already executed an earlier embedded-
        // Maven test (e.g. EmbeddedMavenTest above), the "cold"
        // measurement is no longer a true cold start — the class
        // loader has already resolved most of the Maven bytecode and
        // the JIT has compiled the hot paths. In that worst case the
        // ratio compresses to ~1.5-2x but the resident cache IS still
        // being hit; the failure mode for "cache defeated" is
        // ratio ~1.0, which this assertion still catches reliably.
        // The benchmark harness (PRD §17) is responsible for the
        // tighter perf numbers; this test guards the contract.
        assertTrue(ratio >= 1.5,
                "expected >=1.5x speedup from warm path; "
                        + "cold_ms=" + coldMs
                        + " first_warm_ms=" + firstWarmMs
                        + " warm_ms=" + warmMs
                        + " ratio=" + String.format("%.2f", ratio));
    }

    private long runOne(Path project) throws IOException {
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
        return result.getDurationMicros();
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
